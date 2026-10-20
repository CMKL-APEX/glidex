//! Kernel extraction (spec/kernel-boot.md §10): the extractor appliance,
//! the stream it writes, and the choice of kernel and command line.
//!
//! A disk image's filesystems are only ever read inside a VM (K9): a
//! pinned Debian kernel with an initramfs glidex writes itself (busybox,
//! the filesystem modules and `extractor-init.sh`) boots with the image
//! attached read-only and copies its boot files to a raw disk. The host
//! parses that disk as a plain stream, with every length checked.

use super::ImageError;
use super::catalog::Arch;
use super::firmware::{self, DebFile};
use std::cmp::Ordering;
use std::io::{BufRead, Read, Seek, SeekFrom};

fn invalid(m: impl Into<String>) -> ImageError {
    ImageError::InvalidImage(m.into())
}

/// One pinned extractor build for an architecture (K13): a Debian cloud
/// kernel and `busybox-static`, from snapshot.debian.org (which keeps a
/// file at its first-seen URL forever), each file checked by digest.
#[derive(Debug)]
pub struct ExtractorPin {
    pub arch: Arch,
    /// The kernel's release; also the cache directory's name.
    pub version: &'static str,
    pub kernel_deb: (&'static str, &'static str),
    pub busybox_deb: (&'static str, &'static str),
    pub kernel: DebFile,
    pub busybox: DebFile,
    /// Modules in load order: every module after those it depends on
    /// (checked when the appliance is built).
    pub modules: &'static [(&'static str, DebFile)],
}

#[cfg(test)]
const KMOD: &str = "usr/lib/modules/6.12.107+deb13-cloud-amd64/kernel";

macro_rules! module {
    ($name:literal, $path:literal, $sha:literal) => {
        (
            $name,
            DebFile {
                path: concat!("usr/lib/modules/6.12.107+deb13-cloud-amd64/kernel/", $path),
                sha256: $sha,
            },
        )
    };
}

pub const EXTRACTORS: &[ExtractorPin] = &[ExtractorPin {
    arch: Arch::X86_64,
    version: "6.12.107+deb13-cloud-amd64",
    kernel_deb: (
        "https://snapshot.debian.org/archive/debian/20260907T023910Z/pool/main/l/linux-signed-amd64/linux-image-6.12.107%2Bdeb13-cloud-amd64_6.12.107-1_amd64.deb",
        "04434ff520f860423eafe4203d986a35682ce73c2f71baa39e988ca0da93ac91",
    ),
    busybox_deb: (
        "https://snapshot.debian.org/archive/debian/20260712T202631Z/pool/main/b/busybox/busybox-static_1.37.0-6%2Bb9_amd64.deb",
        "598a3fd92bdafc34cd81196b2952ad36e910e47a4ecf162f8ce5e8e262598e53",
    ),
    kernel: DebFile {
        path: "boot/vmlinuz-6.12.107+deb13-cloud-amd64",
        sha256: "d1bc2c60a78ae6979736d081bc3c90e45ace6baf414f767dff588e319d3492bd",
    },
    busybox: DebFile {
        path: "usr/bin/busybox",
        sha256: "7ae95c260622412ceba6eb9f45d68f616cdc091a1d71d6c0a08b5875bb2c31dd",
    },
    modules: &[
        module!(
            "virtio_blk",
            "drivers/block/virtio_blk.ko.xz",
            "d84a7dcde87c778579075d924acc5b2b4c58fc49d35a0ef5bea3b59189a8862a"
        ),
        module!(
            "libcrc32c",
            "lib/libcrc32c.ko.xz",
            "b37c48ce6bd849097609b07d160ec7782d146dfb7d7c9b5b400090c5b9018cdf"
        ),
        module!(
            "xfs",
            "fs/xfs/xfs.ko.xz",
            "50b939397be0e6813b1b09bf8332b8149dbd9a01d60a270fc740cf46e5f77569"
        ),
        module!(
            "xor",
            "crypto/xor.ko.xz",
            "cfcada8fc4507909cc37ff7b108413a95d474f93bde40d1b223ef370663d5fe4"
        ),
        module!(
            "raid6_pq",
            "lib/raid6/raid6_pq.ko.xz",
            "e65dfa3b06d5c6dea36a9f68341b6548d9f8c660518f20f8ca233827d53b2ec5"
        ),
        module!(
            "btrfs",
            "fs/btrfs/btrfs.ko.xz",
            "006522d240a7f8c6c9dae18a64dfc1594ae2bb410d297a092dc8a7ceeb44df62"
        ),
        module!(
            "fat",
            "fs/fat/fat.ko.xz",
            "8b23296829aad47a64db2cd58ebc4df0bbbf652a89d7ba48a49d49de1b4b2433"
        ),
        module!(
            "vfat",
            "fs/fat/vfat.ko.xz",
            "76a13dd499fd3b96f5e37dd8a1507693ebbb17feb116ea651c3a88bd9d01b70d"
        ),
        module!(
            "nls_cp437",
            "fs/nls/nls_cp437.ko.xz",
            "680185868bfa5c96cd5ae13240c571a90c556e87edb946ff57b61f188f796a48"
        ),
        module!(
            "nls_iso8859_1",
            "fs/nls/nls_iso8859-1.ko.xz",
            "78c3b2ee8af2a73688e756004959c285d67e3a094a29c56d00c45849e34f7f5d"
        ),
        module!(
            "nls_utf8",
            "fs/nls/nls_utf8.ko.xz",
            "4083c1496be5eb3bd34b44c32a3eeba8cf19d8b372b0c920c3b8dba274b55a56"
        ),
        // FAT's default iocharset (`ascii`) is a module in this build.
        module!(
            "nls_ascii",
            "fs/nls/nls_ascii.ko.xz",
            "969737a5c36c0a2fcdfeccf68e827036ea40016979dbee1043287b05aa3af692"
        ),
    ],
}];

/// The extractor for `arch`, when one is pinned.
pub fn pin_for(arch: Arch) -> Option<&'static ExtractorPin> {
    EXTRACTORS.iter().find(|p| p.arch == arch)
}

/// The appliance's `/init`.
pub const INIT: &str = include_str!("extractor-init.sh");

/// The appliance's kernel command line.
pub const CMDLINE: &str = "console=ttyS0 panic=-1 quiet";

/// Build the appliance from the two packages: `(kernel, initramfs)`.
/// Every file is checked against its pinned digest, the modules are
/// decompressed (the initramfs loads them with busybox `insmod`), and
/// each module's own `depends=` must come earlier in the load order.
pub fn assemble(
    pin: &ExtractorPin,
    kernel_deb: &[u8],
    busybox_deb: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), ImageError> {
    let mut wanted = vec![pin.kernel];
    wanted.extend(pin.modules.iter().map(|(_, f)| *f));
    let mut files = firmware::extract_deb(kernel_deb, &wanted)?.into_iter();
    let kernel = files.next().unwrap_or_default();
    let busybox = firmware::extract_deb(busybox_deb, &[pin.busybox])?
        .pop()
        .unwrap_or_default();

    let mut entries: Vec<(String, u32, Vec<u8>)> = [
        "bin",
        "dev",
        "proc",
        "sys",
        "mnt",
        "tmp",
        "lib",
        "lib/modules",
    ]
    .iter()
    .map(|d| (d.to_string(), 0o040755, Vec::new()))
    .collect();
    entries.push(("bin/busybox".into(), 0o100755, busybox));
    entries.push(("init".into(), 0o100755, INIT.as_bytes().to_vec()));
    let order: Vec<&str> = pin.modules.iter().map(|(n, _)| *n).collect();
    entries.push((
        "lib/modules/order".into(),
        0o100644,
        format!("{}\n", order.join("\n")).into_bytes(),
    ));
    for (i, ((name, f), xz)) in pin.modules.iter().zip(files).enumerate() {
        let ko = xz_decompress(&xz).map_err(|e| invalid(format!("{}: {e}", f.path)))?;
        for dep in module_depends(&ko) {
            if !order[..i].contains(&dep.as_str()) {
                return Err(invalid(format!(
                    "module {name} needs {dep}, which isn't loaded before it"
                )));
            }
        }
        entries.push((format!("lib/modules/{name}.ko"), 0o100644, ko));
    }
    Ok((kernel, newc(&entries)))
}

fn xz_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    lzma_rs::xz_decompress(&mut std::io::BufReader::new(data), &mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

/// A module's `depends=` (from its `.modinfo` strings), with `-` read as
/// `_` as the kernel does.
pub fn module_depends(ko: &[u8]) -> Vec<String> {
    let key = b"\0depends=";
    let Some(start) = ko.windows(key.len()).position(|w| w == key) else {
        return Vec::new();
    };
    let rest = &ko[start + key.len()..];
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..end])
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.replace('-', "_"))
        .collect()
}

/// A `newc` cpio archive of `(path, mode, data)`, as the kernel unpacks
/// an initramfs.
pub fn newc(entries: &[(String, u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let pad = |out: &mut Vec<u8>| out.resize(out.len().next_multiple_of(4), 0);
    let trailer = ("TRAILER!!!".to_string(), 0, Vec::new());
    for (ino, (name, mode, data)) in entries.iter().chain([&trailer]).enumerate() {
        let fields = [
            ino as u32 + 1,
            *mode,
            0,
            0,
            1,
            0,
            data.len() as u32,
            0,
            0,
            0,
            0,
            name.len() as u32 + 1,
            0,
        ];
        out.extend_from_slice(b"070701");
        for f in fields {
            out.extend_from_slice(format!("{f:08X}").as_bytes());
        }
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        pad(&mut out);
        out.extend_from_slice(data);
        pad(&mut out);
    }
    out
}

// ---- the stream ----------------------------------------------------------

/// The size of the extractor's output disk, and so of everything it writes.
pub const OUTPUT_SIZE: u64 = 2 << 30;
/// At most this many files in one stream.
pub const MAX_FILES: usize = 64;
/// A boot configuration file's limit.
pub const MAX_CONFIG_SIZE: u64 = 1 << 20;

/// One file of the stream: where its bytes are in the output disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEntry {
    /// The partition it came from (`vda13`).
    pub partition: String,
    /// Its path inside that partition (`/vmlinuz-6.8.0-147-generic`).
    pub path: String,
    pub offset: u64,
    pub size: u64,
}

impl StreamEntry {
    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or_default()
    }

    /// `vda13:/vmlinuz-6.8.0-147-generic`.
    pub fn label(&self) -> String {
        format!("{}:{}", self.partition, self.path)
    }
}

/// Read the stream's index (§10.3) from the output disk, seeking past each
/// file. Refused, as a whole: a header that isn't `GXFILE <name> <size>`,
/// a name with `..`, a NUL or a control character, more than [`MAX_FILES`]
/// files, a kernel or initramfs over `limits`, any other file over
/// [`MAX_CONFIG_SIZE`], a total over [`OUTPUT_SIZE`], or no `GXEND`.
pub fn parse_stream<R: BufRead + Seek>(
    mut r: R,
    max_kernel: u64,
    max_initrd: u64,
) -> Result<Vec<StreamEntry>, ImageError> {
    let bad = |m: String| invalid(format!("extractor output: {m}"));
    let mut entries = Vec::new();
    let mut pos = 0u64;
    loop {
        let mut line = Vec::new();
        r.by_ref()
            .take(4096)
            .read_until(b'\n', &mut line)
            .map_err(|e| ImageError::Io(e.to_string()))?;
        if line.last() != Some(&b'\n') {
            return Err(bad(
                "a header without an end (the extractor did not finish)".into(),
            ));
        }
        pos += line.len() as u64;
        let line = std::str::from_utf8(&line[..line.len() - 1])
            .map_err(|_| bad("a header that isn't UTF-8".into()))?;
        if line == "GXEND" {
            return Ok(entries);
        }
        let mut f = line.split(' ');
        let (Some("GXFILE"), Some(name), Some(size), None) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            return Err(bad(format!("unexpected header {line:?}")));
        };
        let size: u64 = size
            .parse()
            .map_err(|_| bad(format!("bad size in {line:?}")))?;
        let (partition, path) = name
            .split_once(':')
            .filter(|(p, path)| !p.is_empty() && path.starts_with('/'))
            .ok_or_else(|| bad(format!("bad name {name:?}")))?;
        if name.contains("..") || name.chars().any(|c| c.is_control()) {
            return Err(bad(format!("refused name {name:?}")));
        }
        if entries.len() == MAX_FILES {
            return Err(bad(format!("more than {MAX_FILES} files")));
        }
        let file = path.rsplit('/').next().unwrap_or_default();
        let limit = if file.starts_with("vmlinuz-") {
            max_kernel
        } else if file.starts_with("initrd.img-") || file.starts_with("initramfs-") {
            max_initrd
        } else {
            MAX_CONFIG_SIZE
        };
        if size > limit {
            return Err(bad(format!("{name} is {size} bytes (limit {limit})")));
        }
        if pos + size > OUTPUT_SIZE {
            return Err(bad("more than the output disk holds".into()));
        }
        entries.push(StreamEntry {
            partition: partition.to_string(),
            path: path.to_string(),
            offset: pos,
            size,
        });
        pos += size;
        r.seek(SeekFrom::Start(pos))
            .map_err(|e| ImageError::Io(e.to_string()))?;
    }
}

// ---- the choice ----------------------------------------------------------

/// The kernel extraction picked (§10.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub version: String,
    pub kernel: StreamEntry,
    pub initrd: StreamEntry,
    pub cmdline: String,
}

/// Compare kernel versions segment by segment: runs of digits as numbers,
/// anything else as text (`6.12.113+deb13` > `6.12.94+deb13`).
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    fn segments(s: &str) -> Vec<(bool, &str)> {
        let mut out = Vec::new();
        let mut start = 0;
        let bytes = s.as_bytes();
        for i in 1..=bytes.len() {
            if i == bytes.len() || bytes[i].is_ascii_digit() != bytes[start].is_ascii_digit() {
                out.push((bytes[start].is_ascii_digit(), &s[start..i]));
                start = i;
            }
        }
        out
    }
    let (sa, sb) = (segments(a), segments(b));
    for (x, y) in sa.iter().zip(&sb) {
        let o = match (x, y) {
            ((true, x), (true, y)) => {
                let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
                x.len().cmp(&y.len()).then_with(|| x.cmp(y))
            }
            ((_, x), (_, y)) => x.cmp(y),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    sa.len().cmp(&sb.len())
}

/// Pick the kernel: the highest version among `vmlinuz-<v>` that has an
/// initramfs for the same `<v>` (`initrd.img-<v>` or `initramfs-<v>.img`,
/// on the same partition first); then its command line from the boot
/// configs, which `text` reads (BLS entry, else the first `grub.cfg`
/// `linux` line for it). `BOOT_IMAGE=`, `console=`, `earlyprintk=` and
/// GRUB variables are dropped, `console=ttyS0` appended; without a
/// `root=` the extraction fails.
pub fn choose(
    entries: &[StreamEntry],
    mut text: impl FnMut(&StreamEntry) -> Result<String, ImageError>,
) -> Result<Choice, ImageError> {
    let mut candidates: Vec<(String, &StreamEntry, &StreamEntry)> = entries
        .iter()
        .filter_map(|k| {
            let v = k.file_name().strip_prefix("vmlinuz-")?;
            let names = [format!("initrd.img-{v}"), format!("initramfs-{v}.img")];
            let initrd = entries
                .iter()
                .filter(|e| names.iter().any(|n| e.file_name() == n))
                .min_by_key(|e| e.partition != k.partition)?;
            Some((v.to_string(), k, initrd))
        })
        .collect();
    candidates.sort_by(|a, b| compare_versions(&a.0, &b.0));
    let (version, kernel, initrd) = candidates
        .pop()
        .ok_or_else(|| invalid("no kernel with an initramfs in the image's /boot"))?;
    let kernel_file = format!("vmlinuz-{version}");
    let names_kernel = |path: &str| path.rsplit('/').next() == Some(kernel_file.as_str());

    let mut args = None;
    for e in entries
        .iter()
        .filter(|e| e.path.contains("/loader/entries/"))
    {
        let conf = text(e)?;
        let field = |key: &str| {
            conf.lines().find_map(|l| {
                l.trim()
                    .strip_prefix(key)?
                    .strip_prefix([' ', '\t'])
                    .map(str::trim)
            })
        };
        if field("linux").is_some_and(names_kernel)
            && let Some(options) = field("options")
        {
            args = Some(options.to_string());
            break;
        }
    }
    if args.is_none() {
        'cfg: for e in entries.iter().filter(|e| e.file_name() == "grub.cfg") {
            for l in text(e)?.lines() {
                let mut t = l.split_whitespace();
                if matches!(t.next(), Some("linux" | "linuxefi"))
                    && t.next().is_some_and(names_kernel)
                {
                    args = Some(t.collect::<Vec<_>>().join(" "));
                    break 'cfg;
                }
            }
        }
    }
    let args = args.ok_or_else(|| {
        invalid(format!(
            "no boot configuration names {kernel_file} (no BLS entry or grub.cfg linux line)"
        ))
    })?;
    let mut kept: Vec<&str> = args
        .split_whitespace()
        .filter(|t| {
            !t.starts_with('$')
                && !t.starts_with("BOOT_IMAGE=")
                && !t.starts_with("console=")
                && !t.starts_with("earlyprintk=")
        })
        .collect();
    if !kept.iter().any(|t| t.starts_with("root=")) {
        return Err(invalid(format!(
            "the boot configuration for {kernel_file} names no root="
        )));
    }
    kept.push("console=ttyS0");
    Ok(Choice {
        version,
        kernel: kernel.clone(),
        initrd: initrd.clone(),
        cmdline: kept.join(" "),
    })
}

/// Read one stream entry's bytes (a boot config) from the output disk.
pub fn read_entry<R: Read + Seek>(r: &mut R, e: &StreamEntry) -> Result<Vec<u8>, ImageError> {
    let io = |e: std::io::Error| ImageError::Io(e.to_string());
    r.seek(SeekFrom::Start(e.offset)).map_err(io)?;
    let mut buf = vec![0; e.size as usize];
    r.read_exact(&mut buf).map_err(io)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn stream(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut s = Vec::new();
        for (name, data) in files {
            s.extend_from_slice(format!("GXFILE {name} {}\n", data.len()).as_bytes());
            s.extend_from_slice(data);
        }
        s.extend_from_slice(b"GXEND\n");
        s
    }

    fn parse(s: &[u8]) -> Result<Vec<StreamEntry>, ImageError> {
        parse_stream(Cursor::new(s), 1 << 20, 1 << 20)
    }

    #[test]
    fn stream_round_trip() {
        let s = stream(&[
            ("vda1:/boot/vmlinuz-6.1", b"KERNEL"),
            ("vda1:/boot/grub/grub.cfg", b""),
        ]);
        let e = parse(&s).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].label(), "vda1:/boot/vmlinuz-6.1");
        assert_eq!(read_entry(&mut Cursor::new(&s), &e[0]).unwrap(), b"KERNEL");
        assert_eq!((e[1].file_name(), e[1].size), ("grub.cfg", 0));
        // Zeros after GXEND (the rest of the disk) are never read.
        let mut padded = s.clone();
        padded.resize(s.len() + 4096, 0);
        assert_eq!(parse(&padded).unwrap(), e);
    }

    #[test]
    fn bad_streams_are_refused() {
        let ok = |n: &str| stream(&[(n, b"x")]);
        for (s, why) in [
            (b"".to_vec(), "empty: the extractor didn't run"),
            (vec![0u8; 512], "zeros: no header"),
            (stream(&[]).split_last().unwrap().1.to_vec(), "no end"),
            (b"GXFILE vda1:/a 99\nxx".to_vec(), "truncated, no GXEND"),
            (ok("vda1:/../etc/shadow"), ".."),
            (ok("vda1:relative"), "relative path"),
            (ok(":/x"), "no partition"),
            (ok("vda1:/a\x1bb"), "control character"),
            (b"GXFILE vda1:/a b 1\nxGXEND\n".to_vec(), "space in name"),
            (b"GXFILE vda1:/a -1\nGXEND\n".to_vec(), "negative size"),
            (b"HELLO\nGXEND\n".to_vec(), "unknown header"),
        ] {
            assert!(parse(&s).is_err(), "{why}");
        }
        let many: Vec<(String, &[u8])> = (0..=MAX_FILES)
            .map(|i| (format!("vda1:/f{i}"), &b""[..]))
            .collect();
        let refs: Vec<(&str, &[u8])> = many.iter().map(|(n, d)| (n.as_str(), *d)).collect();
        assert!(parse(&stream(&refs)).is_err(), "too many files");
        // Per-kind limits: a config over 1 MiB, a kernel over its limit.
        let big = vec![0u8; (MAX_CONFIG_SIZE + 1) as usize];
        assert!(parse(&stream(&[("vda1:/grub.cfg", &big)])).is_err());
        assert!(
            parse_stream(
                Cursor::new(stream(&[("vda1:/vmlinuz-1", b"12345")])),
                4,
                100
            )
            .is_err()
        );
        assert!(
            parse_stream(
                Cursor::new(stream(&[("vda1:/initrd.img-1", b"12345")])),
                100,
                4
            )
            .is_err()
        );
    }

    #[test]
    fn versions() {
        use Ordering::*;
        assert_eq!(
            compare_versions("6.12.113+deb13-amd64", "6.12.94+deb13-amd64"),
            Greater
        );
        assert_eq!(
            compare_versions("6.8.0-147-generic", "6.8.0-60-generic"),
            Greater
        );
        assert_eq!(
            compare_versions("5.14.0-687.53.1.el9_8", "5.14.0-687.53.1.el9_8"),
            Equal
        );
        assert_eq!(compare_versions("6.1", "6.1.1"), Less);
        assert_eq!(compare_versions("6.010", "6.9"), Greater);
    }

    /// Stream entries for `files`, and a reader of their text.
    fn layout(files: &[(&str, &str)]) -> (Vec<StreamEntry>, Vec<u8>) {
        let s = stream(
            &files
                .iter()
                .map(|(n, d)| (*n, d.as_bytes()))
                .collect::<Vec<_>>(),
        );
        (parse(&s).unwrap(), s)
    }

    fn pick(files: &[(&str, &str)]) -> Result<Choice, ImageError> {
        let (e, s) = layout(files);
        let mut c = Cursor::new(s);
        choose(&e, |x| {
            Ok(String::from_utf8_lossy(&read_entry(&mut c, x)?).into_owned())
        })
    }

    const FIXTURES: &str = "tests/fixtures/boot";

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/{FIXTURES}/{name}", env!("CARGO_MANIFEST_DIR")))
            .unwrap()
    }

    /// The four layouts of F-K6, with the boot configs the images ship.
    #[test]
    fn choice_on_the_catalog_layouts() {
        let ubuntu = fixture("ubuntu-26.04-grub.cfg");
        let c = pick(&[
            ("vda1:/usr/lib/needrestart/vmlinuz-get-version", "#!/bin/sh"),
            ("vda13:/grub/grub.cfg", &ubuntu),
            ("vda13:/vmlinuz-7.0.0-34-generic", "K"),
            ("vda13:/initrd.img-7.0.0-34-generic", "I"),
            (
                "vda15:/EFI/ubuntu/grub.cfg",
                "search.fs_uuid x root\nconfigfile $prefix/grub.cfg\n",
            ),
        ])
        .unwrap();
        assert_eq!(c.version, "7.0.0-34-generic");
        assert_eq!(c.kernel.label(), "vda13:/vmlinuz-7.0.0-34-generic");
        assert_eq!(c.initrd.label(), "vda13:/initrd.img-7.0.0-34-generic");
        assert_eq!(c.cmdline, "root=LABEL=cloudimg-rootfs ro console=ttyS0");

        let debian = fixture("debian-13-grub.cfg");
        let c = pick(&[
            ("vda1:/boot/initrd.img-6.12.113+deb13-amd64", "I"),
            ("vda1:/boot/grub/grub.cfg", &debian),
            ("vda1:/boot/vmlinuz-6.12.113+deb13-amd64", "K"),
        ])
        .unwrap();
        assert_eq!(
            c.cmdline,
            "root=PARTUUID=1683acde-93a9-4042-8028-4fff322b1a13 ro consoleblank=0 console=ttyS0"
        );

        let alma = fixture("alma-9-loader-entry.conf");
        let c = pick(&[
            ("vda3:/grub2/grub.cfg", &fixture("alma-9-grub.cfg")),
            (
                "vda3:/loader/entries/5cff-5.14.0-687.53.1.el9_8.x86_64.conf",
                &alma,
            ),
            ("vda3:/vmlinuz-5.14.0-687.53.1.el9_8.x86_64", "K"),
            ("vda3:/initramfs-5.14.0-687.53.1.el9_8.x86_64.img", "I"),
        ])
        .unwrap();
        assert_eq!(
            c.cmdline,
            "root=UUID=9f6cf8a5-e21d-43f1-b56f-9d7bbe3545e7 ro no_timer_check biosdevname=0 net.ifnames=0 console=ttyS0"
        );

        let fedora = fixture("fedora-44-loader-entry.conf");
        let c = pick(&[
            ("vda3:/boot/grub2/grub.cfg", &fixture("fedora-44-grub.cfg")),
            (
                "vda3:/boot/loader/entries/b208-6.19.10-300.fc44.x86_64.conf",
                &fedora,
            ),
            ("vda3:/boot/vmlinuz-6.19.10-300.fc44.x86_64", "K"),
            ("vda3:/boot/initramfs-6.19.10-300.fc44.x86_64.img", "I"),
        ])
        .unwrap();
        assert_eq!(
            c.cmdline,
            "no_timer_check systemd.firstboot=off root=UUID=15c26993-ac30-424a-9c4b-faec4434d234 rootflags=subvol=root console=ttyS0"
        );
    }

    #[test]
    fn newest_kernel_with_an_initramfs() {
        let cfg = "menuentry a {\n linux /boot/vmlinuz-6.1.10 root=/dev/vda1 ro\n}\nmenuentry b {\n linux /boot/vmlinuz-6.1.9 root=/dev/vda1 ro quiet\n}\n";
        let c = pick(&[
            ("vda1:/boot/grub/grub.cfg", cfg),
            ("vda1:/boot/vmlinuz-6.1.9", "K"),
            ("vda1:/boot/initrd.img-6.1.9", "I"),
            ("vda1:/boot/vmlinuz-6.1.10", "K"),
            ("vda1:/boot/initrd.img-6.1.10", "I"),
            // Newer, but without an initramfs: skipped.
            ("vda1:/boot/vmlinuz-6.1.11", "K"),
        ])
        .unwrap();
        assert_eq!(c.version, "6.1.10");
        assert_eq!(c.cmdline, "root=/dev/vda1 ro console=ttyS0");
    }

    #[test]
    fn choice_fails_cleanly() {
        let k = ("vda1:/boot/vmlinuz-6.1", "K");
        let i = ("vda1:/boot/initrd.img-6.1", "I");
        assert!(pick(&[k]).is_err(), "no initramfs");
        assert!(pick(&[k, i]).is_err(), "no boot config");
        let no_root = "linux /boot/vmlinuz-6.1 ro quiet\n";
        assert!(
            pick(&[k, i, ("vda1:/boot/grub/grub.cfg", no_root)]).is_err(),
            "no root="
        );
        let other = "linux /boot/vmlinuz-5.0 root=/dev/vda1\n";
        assert!(
            pick(&[k, i, ("vda1:/boot/grub/grub.cfg", other)]).is_err(),
            "names another kernel"
        );
    }

    #[test]
    fn newc_archives() {
        let a = newc(&[("init".into(), 0o100755, b"#!/bin/sh\n".to_vec())]);
        assert!(a.starts_with(b"070701"));
        assert_eq!(a.len() % 4, 0);
        let t = a.windows(10).position(|w| w == b"TRAILER!!!").unwrap();
        assert!(t > 110);
        // The name, then the data, each 4-byte aligned after the header.
        assert_eq!(&a[110..115], b"init\0");
        assert_eq!(&a[116..126], b"#!/bin/sh\n");
    }

    #[test]
    fn module_dependencies() {
        let ko = b"\x7fELF....\0license=GPL\0depends=libcrc32c,raid6-pq,xor\0intree=Y\0";
        assert_eq!(module_depends(ko), ["libcrc32c", "raid6_pq", "xor"]);
        assert!(module_depends(b"\0depends=\0").is_empty());
        assert!(module_depends(b"no modinfo").is_empty());
    }

    #[test]
    fn pins_are_https_and_complete() {
        for p in EXTRACTORS {
            for (url, sha) in [p.kernel_deb, p.busybox_deb] {
                assert!(url.starts_with("https://snapshot.debian.org/"), "{url}");
                assert_eq!(sha.len(), 64);
            }
            assert!(p.kernel.path.ends_with(p.version));
            for (name, f) in p.modules {
                assert!(f.path.starts_with(KMOD), "{name}");
                assert_eq!(f.sha256.len(), 64, "{name}");
            }
        }
        assert!(pin_for(Arch::X86_64).is_some());
    }
}
