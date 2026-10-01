//! Partition tables of managed disks: read-only parsing (GPT and MBR), the
//! choice of root partition, and edits through a sparse "shadow" file.
//!
//! Partition tools (`sgdisk`, `growpart`) need a raw block view of the
//! disk. A raw disk is edited in place. For a qcow2 disk the only regions
//! those tools touch are the first and last few sectors (protective MBR,
//! primary GPT, backup GPT), so `edit` copies just those windows into a
//! sparse raw file the size of the disk, runs the tool on it, checks the
//! tool wrote nothing outside the windows, and writes changed windows back
//! with `qemu-io`. No FUSE or NBD export, so no root and no `/dev/fuse`.

use super::qemu_img::{self, DiskFormat};
use super::ImageError;
use serde::Serialize;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

pub const SECTOR: u64 = 512;
/// Size of the head and tail windows copied for an edit.
const WINDOW: u64 = 1024 * 1024;
/// GPT backup: 32 sectors of entries + 1 header sector.
pub const GPT_BACKUP_SECTORS: u64 = 33;
pub const MIB: u64 = 1024 * 1024;

// Discoverable Partitions Specification root types.
const GUID_ROOT_X86_64: &str = "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709";
const GUID_ROOT_AARCH64: &str = "B921B045-1DF0-41C3-AF44-4C6F280D3FAE";
const GUID_LINUX_FS: &str = "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TableKind {
    Gpt,
    Mbr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Partition {
    pub number: u32,
    pub start_bytes: u64,
    pub size_bytes: u64,
    /// GPT type GUID (upper case) or MBR type byte as `0x83`.
    #[serde(rename = "type")]
    pub type_id: String,
    pub is_root: bool,
}

impl Partition {
    pub fn end_bytes(&self) -> u64 {
        self.start_bytes + self.size_bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PartitionTable {
    pub kind: TableKind,
    pub partitions: Vec<Partition>,
    /// Unpartitioned space after the last partition (and, for GPT, before
    /// the backup header at the end of the disk).
    pub free_tail_bytes: u64,
}

impl PartitionTable {
    pub fn root(&self) -> Option<&Partition> {
        self.partitions.iter().find(|p| p.is_root)
    }

    /// Smallest disk size that keeps every partition (and a GPT backup)
    /// intact, rounded up to a whole MiB.
    pub fn min_disk_size(&self) -> u64 {
        let end = self.partitions.iter().map(Partition::end_bytes).max().unwrap_or(0);
        let tail = match self.kind {
            TableKind::Gpt => GPT_BACKUP_SECTORS * SECTOR,
            TableKind::Mbr => 0,
        };
        round_up(end + tail, MIB)
    }
}

pub fn round_up(v: u64, to: u64) -> u64 {
    v.div_ceil(to) * to
}

/// Window size for a disk: 1 MiB, or less for tiny disks so the head and
/// tail windows never overlap.
fn window_for(disk_size: u64) -> u64 {
    (disk_size / 2 / SECTOR * SECTOR).min(WINDOW)
}

/// Read the partition table of a disk. `Ok(None)`: no partition table.
pub fn read_table(path: &Path, format: DiskFormat, disk_size: u64) -> Result<Option<PartitionTable>, ImageError> {
    let head = read_head(path, format, disk_size)?;
    Ok(parse_table(&head, disk_size, std::env::consts::ARCH))
}

fn read_head(path: &Path, format: DiskFormat, disk_size: u64) -> Result<Vec<u8>, ImageError> {
    let len = window_for(disk_size);
    let mut buf = vec![0u8; len as usize];
    match format {
        DiskFormat::Raw => {
            let f = File::open(path).map_err(|e| io(path, e))?;
            let n = f.read_at(&mut buf, 0).map_err(|e| io(path, e))?;
            buf.truncate(n);
        }
        DiskFormat::Qcow2 => {
            let tmp = tempfile::Builder::new()
                .prefix(".gx-head")
                .tempfile_in(path.parent().unwrap_or(Path::new(".")))
                .map_err(|e| io(path, e))?;
            qemu_img::read_region(path, format, 0, len, tmp.path())?;
            buf.clear();
            File::open(tmp.path())
                .and_then(|mut f| f.read_to_end(&mut buf))
                .map_err(|e| io(path, e))?;
        }
    }
    Ok(buf)
}

fn io(path: &Path, e: std::io::Error) -> ImageError {
    ImageError::Io(format!("{}: {}", path.display(), e))
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// Mixed-endian GPT GUID → canonical upper-case string.
fn guid(b: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{}",
        le32(b, 0),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        b[8],
        b[9],
        b[10..16].iter().map(|x| format!("{:02X}", x)).collect::<String>()
    )
}

/// Parse a table from the first bytes of a disk (at least through the GPT
/// entries). `arch` picks which GPT root type GUID counts as root.
pub fn parse_table(head: &[u8], disk_size: u64, arch: &str) -> Option<PartitionTable> {
    if head.len() < 2 * SECTOR as usize || head[510] != 0x55 || head[511] != 0xAA {
        return None;
    }
    let mbr_types: Vec<u8> = (0..4).map(|i| head[446 + i * 16 + 4]).collect();
    if mbr_types.contains(&0xEE) {
        return parse_gpt(head, disk_size, arch);
    }
    parse_mbr(head, disk_size)
}

fn parse_gpt(head: &[u8], disk_size: u64, arch: &str) -> Option<PartitionTable> {
    let hdr = &head[SECTOR as usize..2 * SECTOR as usize];
    if &hdr[..8] != b"EFI PART" {
        return None;
    }
    let entries_lba = le64(hdr, 72);
    let count = le32(hdr, 80) as u64;
    let entry_size = le32(hdr, 84) as u64;
    if entry_size < 128 || count > 1024 {
        return None;
    }
    let start = (entries_lba * SECTOR) as usize;
    let end = start + (count * entry_size) as usize;
    if end > head.len() {
        return None;
    }
    let mut partitions = Vec::new();
    for i in 0..count as usize {
        let e = &head[start + i * entry_size as usize..start + (i + 1) * entry_size as usize];
        if e[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let (first, last) = (le64(e, 32), le64(e, 40));
        if last < first {
            continue;
        }
        partitions.push(Partition {
            number: i as u32 + 1,
            start_bytes: first * SECTOR,
            size_bytes: (last - first + 1) * SECTOR,
            type_id: guid(&e[..16]),
            is_root: false,
        });
    }
    let mut table = PartitionTable { kind: TableKind::Gpt, partitions, free_tail_bytes: 0 };
    let usable_end = disk_size.saturating_sub(GPT_BACKUP_SECTORS * SECTOR);
    finish(&mut table, usable_end, arch);
    Some(table)
}

fn parse_mbr(head: &[u8], disk_size: u64) -> Option<PartitionTable> {
    let mut partitions = Vec::new();
    for i in 0..4 {
        let e = &head[446 + i * 16..446 + (i + 1) * 16];
        let ty = e[4];
        let (first, sectors) = (le32(e, 8) as u64, le32(e, 12) as u64);
        if ty == 0 || sectors == 0 {
            continue;
        }
        partitions.push(Partition {
            number: i as u32 + 1,
            start_bytes: first * SECTOR,
            size_bytes: sectors * SECTOR,
            type_id: format!("0x{:02x}", ty),
            is_root: false,
        });
    }
    if partitions.is_empty() {
        return None;
    }
    let mut table = PartitionTable { kind: TableKind::Mbr, partitions, free_tail_bytes: 0 };
    finish(&mut table, disk_size, "");
    Some(table)
}

fn finish(table: &mut PartitionTable, usable_end: u64, arch: &str) {
    table.partitions.sort_by_key(|p| p.start_bytes);
    let last_end = table.partitions.iter().map(Partition::end_bytes).max().unwrap_or(0);
    table.free_tail_bytes = usable_end.saturating_sub(last_end);
    if let Some(n) = select_root(table, arch) {
        for p in &mut table.partitions {
            p.is_root = p.number == n;
        }
    }
}

/// The root partition: the one with the arch's GPT root type, else the
/// last partition if it is a Linux filesystem. MBR: the last partition if
/// it is type 0x83.
fn select_root(table: &PartitionTable, arch: &str) -> Option<u32> {
    let root_guid = match arch {
        "aarch64" => GUID_ROOT_AARCH64,
        _ => GUID_ROOT_X86_64,
    };
    if let Some(p) = table.partitions.iter().find(|p| p.type_id == root_guid) {
        return Some(p.number);
    }
    let last = table.partitions.iter().max_by_key(|p| p.end_bytes())?;
    match table.kind {
        TableKind::Gpt if last.type_id == GUID_LINUX_FS => Some(last.number),
        TableKind::Mbr if last.type_id == "0x83" => Some(last.number),
        _ => None,
    }
}

/// The partition `extend-root` would grow: the root, which must also be
/// the last partition on the disk.
pub fn growable_root(table: &PartitionTable) -> Result<&Partition, ImageError> {
    let root = table.root().ok_or_else(|| {
        ImageError::invalid_disk("no root partition found (no Linux root or filesystem partition at the end of the disk)")
    })?;
    let last = table.partitions.iter().max_by_key(|p| p.end_bytes()).unwrap();
    if last.number != root.number {
        return Err(ImageError::invalid_disk(format!(
            "root partition {} is not the last partition; partition {} is in the way",
            root.number, last.number
        )));
    }
    Ok(root)
}

/// Run `f` on a raw view of the disk. Raw disks are passed through as is;
/// qcow2 disks go through a shadow file (see module docs).
pub fn edit<T>(
    path: &Path,
    format: DiskFormat,
    disk_size: u64,
    f: impl FnOnce(&Path) -> Result<T, ImageError>,
) -> Result<T, ImageError> {
    if format == DiskFormat::Raw {
        return f(path);
    }
    let w = window_for(disk_size);
    if w < 64 * SECTOR {
        return Err(ImageError::invalid_disk("disk is too small to hold a partition table"));
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = tempfile::Builder::new()
        .prefix(".gx-edit")
        .tempdir_in(dir)
        .map_err(|e| io(dir, e))?;
    let windows = [(0u64, w), (disk_size - w, w)];

    // Shadow: sparse, disk-sized, with the original head and tail copied in.
    let shadow_path = tmp.path().join("shadow");
    let mut originals = Vec::new();
    {
        let shadow = File::create(&shadow_path).map_err(|e| io(&shadow_path, e))?;
        shadow.set_len(disk_size).map_err(|e| io(&shadow_path, e))?;
        for (i, (off, len)) in windows.iter().enumerate() {
            let chunk = tmp.path().join(format!("orig{}", i));
            qemu_img::read_region(path, format, *off, *len, &chunk)?;
            let data = std::fs::read(&chunk).map_err(|e| io(&chunk, e))?;
            shadow.write_all_at(&data, *off).map_err(|e| io(&shadow_path, e))?;
            originals.push(data);
        }
        shadow.sync_all().map_err(|e| io(&shadow_path, e))?;
    }

    let result = f(&shadow_path)?;

    // The tool may only have written inside the windows: anything else
    // would be lost, so refuse to write back at all.
    for (start, end) in data_extents(&shadow_path)? {
        let inside = windows.iter().any(|(off, len)| start >= *off && end <= off + len);
        if !inside {
            return Err(ImageError::Io(format!(
                "partition tool wrote outside the partition-table regions ({}..{}); disk left unchanged",
                start, end
            )));
        }
    }
    let mut shadow = File::open(&shadow_path).map_err(|e| io(&shadow_path, e))?;
    for (i, (off, len)) in windows.iter().enumerate() {
        let mut data = vec![0u8; *len as usize];
        shadow.seek(SeekFrom::Start(*off)).map_err(|e| io(&shadow_path, e))?;
        shadow.read_exact(&mut data).map_err(|e| io(&shadow_path, e))?;
        if data == originals[i] {
            continue;
        }
        let name = format!("new{}", i);
        File::create(tmp.path().join(&name))
            .and_then(|mut f| f.write_all(&data))
            .map_err(|e| io(tmp.path(), e))?;
        qemu_img::write_region(path, format, tmp.path(), &name, *off, *len)?;
    }
    Ok(result)
}

/// Allocated extents of a sparse file, as `[start, end)` byte ranges.
fn data_extents(path: &Path) -> Result<Vec<(u64, u64)>, ImageError> {
    use std::os::fd::AsRawFd;
    let f = File::open(path).map_err(|e| io(path, e))?;
    let size = f.metadata().map_err(|e| io(path, e))?.len() as i64;
    let fd = f.as_raw_fd();
    let mut out = Vec::new();
    let mut off: i64 = 0;
    while off < size {
        let start = unsafe { libc::lseek(fd, off, libc::SEEK_DATA) };
        if start < 0 {
            break; // ENXIO: no more data
        }
        let end = unsafe { libc::lseek(fd, start, libc::SEEK_HOLE) };
        if end < 0 {
            return Err(io(path, std::io::Error::last_os_error()));
        }
        out.push((start as u64, end as u64));
        off = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Tests that need host tools are skipped where they are missing.
    fn have(tool: &str) -> bool {
        qemu_img::ALL_TOOLS.iter().find(|t| t.name == tool).is_some_and(|t| t.available())
    }

    /// Build a raw disk with `sgdisk` arguments.
    fn gpt_disk(dir: &Path, size_mib: u64, args: &[&str]) -> std::path::PathBuf {
        let path = dir.join("d.raw");
        File::create(&path).unwrap().set_len(size_mib * MIB).unwrap();
        let st = Command::new(qemu_img::SGDISK.find().unwrap()).args(args).arg(&path).output().unwrap();
        assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stderr));
        path
    }

    fn table(path: &Path, arch: &str) -> Option<PartitionTable> {
        let size = std::fs::metadata(path).unwrap().len();
        let head = std::fs::read(path).unwrap();
        parse_table(&head[..window_for(size) as usize], size, arch)
    }

    #[test]
    fn gpt_root_by_type_last() {
        if !have("sgdisk") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = gpt_disk(dir.path(), 64, &["-n", "1:2048:+8M", "-t", "1:ef00", "-n", "2:0:+20M", "-t", &format!("2:{}", GUID_ROOT_X86_64)]);
        let t = table(&p, "x86_64").unwrap();
        assert_eq!(t.kind, TableKind::Gpt);
        assert_eq!(t.partitions.len(), 2);
        assert_eq!(t.partitions[0].type_id, "C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        let root = growable_root(&t).unwrap();
        assert_eq!(root.number, 2);
        assert_eq!(root.start_bytes, 18432 * SECTOR);
        assert_eq!(root.size_bytes, 20 * MIB);
        assert_eq!(t.free_tail_bytes, 64 * MIB - 33 * SECTOR - root.end_bytes());
        assert_eq!(t.min_disk_size(), 30 * MIB);
    }

    /// Ubuntu cloud images: root is partition 1 but physically last, after
    /// /boot (XBOOTLDR), BIOS boot and the ESP.
    #[test]
    fn ubuntu_layout_root_is_partition_one_at_the_end() {
        if !have("sgdisk") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = gpt_disk(
            dir.path(),
            64,
            &[
                "-n", "13:2048:+16M", "-t", "13:ea00",
                "-n", "14:0:+4M", "-t", "14:ef02",
                "-n", "15:0:+8M", "-t", "15:ef00",
                "-n", "1:0:0", "-t", "1:8304",
            ],
        );
        let t = table(&p, "x86_64").unwrap();
        assert_eq!(t.partitions.iter().find(|p| p.number == 1).unwrap().type_id, GUID_ROOT_X86_64);
        assert_eq!(growable_root(&t).unwrap().number, 1);
    }

    #[test]
    fn gpt_root_not_last_is_refused() {
        if !have("sgdisk") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = gpt_disk(
            dir.path(),
            64,
            &["-n", "1:2048:+20M", "-t", &format!("1:{}", GUID_ROOT_X86_64), "-n", "2:0:+8M", "-t", "2:8200"],
        );
        let t = table(&p, "x86_64").unwrap();
        assert_eq!(t.root().unwrap().number, 1);
        let err = growable_root(&t).unwrap_err().to_string();
        assert!(err.contains("partition 2 is in the way"), "{}", err);
    }

    #[test]
    fn gpt_falls_back_to_last_linux_fs() {
        if !have("sgdisk") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = gpt_disk(dir.path(), 64, &["-n", "1:2048:+8M", "-t", "1:ef00", "-n", "2:0:0", "-t", "2:8300"]);
        let t = table(&p, "x86_64").unwrap();
        assert_eq!(growable_root(&t).unwrap().number, 2);
        // The aarch64 root type is not x86-64's, but the fallback applies.
        assert_eq!(table(&p, "aarch64").unwrap().root().unwrap().number, 2);
        assert_eq!(t.free_tail_bytes, 0);
    }

    #[test]
    fn gpt_without_linux_partition_has_no_root() {
        if !have("sgdisk") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = gpt_disk(dir.path(), 64, &["-n", "1:2048:+8M", "-t", "1:ef00"]);
        assert!(growable_root(&table(&p, "x86_64").unwrap()).is_err());
    }

    #[test]
    fn mbr_table() {
        let mut head = vec![0u8; 4096];
        head[510] = 0x55;
        head[511] = 0xAA;
        let e = &mut head[446..462];
        e[4] = 0x83;
        e[8..12].copy_from_slice(&2048u32.to_le_bytes());
        e[12..16].copy_from_slice(&40960u32.to_le_bytes());
        let t = parse_table(&head, 64 * MIB, "x86_64").unwrap();
        assert_eq!(t.kind, TableKind::Mbr);
        assert_eq!(t.partitions[0].type_id, "0x83");
        assert!(t.partitions[0].is_root);
        assert_eq!(t.min_disk_size(), 21 * MIB);
        assert_eq!(t.free_tail_bytes, 64 * MIB - 21 * MIB);
    }

    #[test]
    fn blank_disk_has_no_table() {
        assert_eq!(parse_table(&vec![0u8; 4096], 64 * MIB, "x86_64"), None);
    }

    #[test]
    fn edit_qcow2_through_shadow_keeps_data() {
        if !["sgdisk", "qemu-img", "qemu-io", "growpart"].iter().all(|t| have(t)) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let raw = gpt_disk(dir.path(), 64, &["-n", "1:2048:+8M", "-n", "2:0:+20M"]);
        // Data inside partition 2, which must survive the edit.
        let pattern: Vec<u8> = (0..MIB).map(|i| (i % 251) as u8).collect();
        File::options().write(true).open(&raw).unwrap().write_all_at(&pattern, 20 * MIB).unwrap();
        let q = dir.path().join("d.qcow2");
        qemu_img::convert(&raw, DiskFormat::Raw, &q, DiskFormat::Qcow2).unwrap();
        qemu_img::resize(&q, DiskFormat::Qcow2, 128 * MIB, false).unwrap();

        let outcome = edit(&q, DiskFormat::Qcow2, 128 * MIB, |shadow| {
            qemu_img::sgdisk_relocate_backup(shadow)?;
            qemu_img::growpart(shadow, 2)
        })
        .unwrap();
        assert_eq!(outcome, qemu_img::GrowOutcome::Changed);

        let t = read_table(&q, DiskFormat::Qcow2, 128 * MIB).unwrap().unwrap();
        assert_eq!(t.partitions[1].end_bytes(), 128 * MIB - 33 * SECTOR);
        assert_eq!(t.free_tail_bytes, 0);
        let out = dir.path().join("out.raw");
        qemu_img::convert(&q, DiskFormat::Qcow2, &out, DiskFormat::Raw).unwrap();
        let mut back = vec![0u8; MIB as usize];
        File::open(&out).unwrap().read_exact_at(&mut back, 20 * MIB).unwrap();
        assert_eq!(back, pattern);
        let verify = Command::new(qemu_img::SGDISK.find().unwrap()).arg("-v").arg(&out).output().unwrap();
        assert!(String::from_utf8_lossy(&verify.stdout).contains("No problems found"));
        qemu_img::check(&q, DiskFormat::Qcow2).unwrap();

        // The tail window is copied too: data there survives an edit.
        let mut f = File::create(dir.path().join("tail.bin")).unwrap();
        f.write_all(&pattern[..4096]).unwrap();
        qemu_img::write_region(&q, DiskFormat::Qcow2, dir.path(), "tail.bin", 128 * MIB - MIB / 2, 4096).unwrap();
        edit(&q, DiskFormat::Qcow2, 128 * MIB, qemu_img::sgdisk_relocate_backup).unwrap();
        let back_tail = dir.path().join("back.bin");
        qemu_img::read_region(&q, DiskFormat::Qcow2, 128 * MIB - MIB / 2, 4096, &back_tail).unwrap();
        assert_eq!(std::fs::read(&back_tail).unwrap(), &pattern[..4096]);

        // Second run: nothing left to grow.
        let again = edit(&q, DiskFormat::Qcow2, 128 * MIB, |s| qemu_img::growpart(s, 2)).unwrap();
        assert_eq!(again, qemu_img::GrowOutcome::NoChange);
    }
}
