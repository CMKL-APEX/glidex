//! Thin wrappers around the host disk tools (`qemu-img`, `qemu-io`,
//! `sgdisk`, `growpart`, `cp`), plus image format detection.
//!
//! Every call is a `Command` with an explicit argv, never a shell string.
//! All functions block; callers run them on `spawn_blocking`.

use super::ImageError;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Format of a managed disk or image file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiskFormat {
    #[default]
    Qcow2,
    Raw,
}

impl DiskFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            DiskFormat::Qcow2 => "qcow2",
            DiskFormat::Raw => "raw",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            DiskFormat::Qcow2 => "qcow2",
            DiskFormat::Raw => "raw",
        }
    }
}

/// Disk image formats a hypervisor can open; names match Cloud-Hypervisor's
/// `ImageType` API enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ImageType {
    FixedVhd,
    Qcow2,
    Raw,
    Vhdx,
}

impl ImageType {
    /// QEMU's `-drive format=` name.
    pub fn qemu_format(&self) -> &'static str {
        match self {
            ImageType::FixedVhd => "vpc",
            ImageType::Qcow2 => "qcow2",
            ImageType::Raw => "raw",
            ImageType::Vhdx => "vhdx",
        }
    }
}

impl From<DiskFormat> for ImageType {
    fn from(f: DiskFormat) -> Self {
        match f {
            DiskFormat::Qcow2 => ImageType::Qcow2,
            DiskFormat::Raw => ImageType::Raw,
        }
    }
}

/// Identify a disk image by the same magic bytes CH's own probe used.
/// Anything without a recognised header is a raw image.
///
/// Only for user-supplied paths: managed disks carry their format in the
/// database and are never probed, so a guest that writes a qcow2 header
/// into a raw disk cannot change how it is opened next time.
pub fn detect_image_type(path: &str) -> Option<ImageType> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 8];
    let n = file.read(&mut header).ok()?;
    if n >= 4 && header[..4] == *b"QFI\xfb" {
        return Some(ImageType::Qcow2);
    }
    if n == 8 && header == *b"vhdxfile" {
        return Some(ImageType::Vhdx);
    }
    // VHD keeps a 512-byte footer at the end: "conectix" cookie, then the
    // big-endian disk type at offset 60 (2 = fixed, the only kind CH runs).
    let mut footer = [0u8; 512];
    if file.seek(SeekFrom::End(-512)).is_ok()
        && file.read_exact(&mut footer).is_ok()
        && footer[..8] == *b"conectix"
        && footer[60..64] == 2u32.to_be_bytes()
    {
        return Some(ImageType::FixedVhd);
    }
    Some(ImageType::Raw)
}

/// A host tool and the package that provides it, per package manager.
#[derive(Debug, Clone, Copy)]
pub struct Tool {
    pub name: &'static str,
    /// apt / dnf / pacman package names.
    pub packages: (&'static str, &'static str, &'static str),
}

pub const QEMU_IMG: Tool = Tool { name: "qemu-img", packages: ("qemu-utils", "qemu-img", "qemu-img") };
pub const QEMU_IO: Tool = Tool { name: "qemu-io", packages: ("qemu-utils", "qemu-img", "qemu-img") };
pub const SGDISK: Tool = Tool { name: "sgdisk", packages: ("gdisk", "gdisk", "gptfdisk") };
pub const GROWPART: Tool = Tool {
    name: "growpart",
    packages: ("cloud-guest-utils", "cloud-utils-growpart", "cloud-guest-utils"),
};
pub const CP: Tool = Tool { name: "cp", packages: ("coreutils", "coreutils", "coreutils") };

pub const ALL_TOOLS: &[Tool] = &[QEMU_IMG, QEMU_IO, SGDISK, GROWPART];

impl Tool {
    /// Absolute path of the tool on `PATH` (plus the sbin dirs, where
    /// `sgdisk` lives on some distros but which are not on every user's PATH).
    pub fn find(&self) -> Option<PathBuf> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .chain(["/usr/local/sbin", "/usr/sbin", "/sbin"].map(PathBuf::from))
            .map(|dir| dir.join(self.name))
            .find(|p| is_executable(p))
    }

    pub fn require(&self) -> Result<PathBuf, ImageError> {
        self.find().ok_or_else(|| ImageError::ToolMissing {
            tool: self.name.to_string(),
            package: format!(
                "{} (apt) / {} (dnf) / {} (pacman)",
                self.packages.0, self.packages.1, self.packages.2
            ),
        })
    }

    pub fn available(&self) -> bool {
        self.find().is_some()
    }

    fn command(&self) -> Result<Command, ImageError> {
        let mut cmd = Command::new(self.require()?);
        // Tool output is parsed in places; keep it in English.
        cmd.env("LC_ALL", "C");
        Ok(cmd)
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Run a command and turn a non-zero exit into `ImageError::Tool`.
fn run(tool: &Tool, mut cmd: Command) -> Result<Output, ImageError> {
    let out = cmd
        .output()
        .map_err(|e| ImageError::Io(format!("{}: {}", tool.name, e)))?;
    if !out.status.success() {
        return Err(tool_failure(tool, &out));
    }
    Ok(out)
}

pub fn tool_failure(tool: &Tool, out: &Output) -> ImageError {
    let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if msg.is_empty() {
        msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
    }
    ImageError::Tool { tool: tool.name.to_string(), stderr: msg }
}

/// The parts of `qemu-img info --output=json` glidex uses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImgInfo {
    pub format: String,
    #[serde(rename = "virtual-size")]
    pub virtual_size: u64,
    #[serde(rename = "actual-size", default)]
    pub actual_size: u64,
    #[serde(rename = "backing-filename", default, skip_serializing_if = "Option::is_none")]
    pub backing_filename: Option<String>,
    #[serde(rename = "backing-filename-format", default, skip_serializing_if = "Option::is_none")]
    pub backing_format: Option<String>,
}

/// `qemu-img info`. `format: None` lets qemu-img probe, which is only done
/// for downloads being verified; the backing chain is never opened.
pub fn info(path: &Path, format: Option<DiskFormat>) -> Result<ImgInfo, ImageError> {
    let mut cmd = QEMU_IMG.command()?;
    cmd.arg("info").arg("--output=json").arg("-U");
    if let Some(f) = format {
        cmd.arg("-f").arg(f.as_str());
    }
    cmd.arg(path);
    let out = run(&QEMU_IMG, cmd)?;
    serde_json::from_slice(&out.stdout)
        .map_err(|e| ImageError::Io(format!("qemu-img info output: {}", e)))
}

/// `qemu-img create`, optionally as an overlay on a qcow2 backing file.
pub fn create(
    path: &Path,
    format: DiskFormat,
    size_bytes: u64,
    backing: Option<&Path>,
) -> Result<(), ImageError> {
    let mut cmd = QEMU_IMG.command()?;
    cmd.arg("create").arg("-q").arg("-f").arg(format.as_str());
    if let Some(b) = backing {
        cmd.arg("-b").arg(b).arg("-F").arg("qcow2");
    }
    cmd.arg(path).arg(size_bytes.to_string());
    run(&QEMU_IMG, cmd).map(|_| ())
}

/// `qemu-img convert` (sparse output, no backing file in the result).
pub fn convert(src: &Path, src_format: DiskFormat, dst: &Path, dst_format: DiskFormat) -> Result<(), ImageError> {
    let mut cmd = QEMU_IMG.command()?;
    cmd.arg("convert")
        .arg("-q")
        .arg("-f")
        .arg(src_format.as_str())
        .arg("-O")
        .arg(dst_format.as_str())
        .arg(src)
        .arg(dst);
    run(&QEMU_IMG, cmd).map(|_| ())
}

/// `qemu-img resize`. `shrink` must be set to make the file smaller;
/// qemu-img then discards everything past the new end without asking.
pub fn resize(path: &Path, format: DiskFormat, size_bytes: u64, shrink: bool) -> Result<(), ImageError> {
    let mut cmd = QEMU_IMG.command()?;
    cmd.arg("resize").arg("-q").arg("-f").arg(format.as_str());
    if shrink {
        cmd.arg("--shrink");
    }
    cmd.arg(path).arg(size_bytes.to_string());
    run(&QEMU_IMG, cmd).map(|_| ())
}

/// `qemu-img check` (qcow2 only; raw has no metadata to check).
pub fn check(path: &Path, format: DiskFormat) -> Result<(), ImageError> {
    if format == DiskFormat::Raw {
        return Ok(());
    }
    let mut cmd = QEMU_IMG.command()?;
    cmd.arg("check").arg("-q").arg("-f").arg(format.as_str()).arg(path);
    run(&QEMU_IMG, cmd).map(|_| ())
}

/// Read `len` bytes at `offset` of the guest-visible disk into `out`.
/// Both must be multiples of 512.
pub fn read_region(path: &Path, format: DiskFormat, offset: u64, len: u64, out: &Path) -> Result<(), ImageError> {
    debug_assert!(offset.is_multiple_of(512) && len.is_multiple_of(512));
    let _ = std::fs::remove_file(out);
    let mut cmd = QEMU_IMG.command()?;
    // qemu-img dd counts `count` from the start of the input, skipped
    // blocks included (unlike dd(1)), so it is the end block here.
    cmd.arg("dd")
        .arg("-f")
        .arg(format.as_str())
        .arg("-O")
        .arg("raw")
        .arg("bs=512")
        .arg(format!("skip={}", offset / 512))
        .arg(format!("count={}", (offset + len) / 512))
        .arg(format!("if={}", path.display()))
        .arg(format!("of={}", out.display()));
    let output = run(&QEMU_IMG, cmd)?;
    // It also exits 0 on some errors ("cannot skip to specified offset")
    // with an empty output file, so check what actually arrived.
    let got = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    if got != len {
        return Err(ImageError::Tool {
            tool: "qemu-img dd".into(),
            stderr: format!(
                "read {} of {} bytes at offset {}: {}",
                got,
                len,
                offset,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(())
}

/// Write the contents of `src` (in directory `dir`) at `offset` of the
/// guest-visible disk. `src` is a bare file name: qemu-io splits its `-c`
/// argument on spaces, so the path must not contain any.
pub fn write_region(path: &Path, format: DiskFormat, dir: &Path, src: &str, offset: u64, len: u64) -> Result<(), ImageError> {
    debug_assert!(!src.contains(' ') && !src.contains('/'));
    let mut cmd = QEMU_IO.command()?;
    cmd.current_dir(dir)
        .arg("-f")
        .arg(format.as_str())
        .arg("-c")
        .arg(format!("write -s {} {} {}", src, offset, len))
        .arg(path);
    let out = run(&QEMU_IO, cmd)?;
    // qemu-io reports command errors on stdout with a zero exit status.
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.contains("wrote ") {
        return Err(ImageError::Tool { tool: "qemu-io".into(), stderr: stdout.trim().to_string() });
    }
    Ok(())
}

/// Copy a file, keeping holes and sharing extents where the filesystem can.
pub fn copy_sparse(src: &Path, dst: &Path) -> Result<(), ImageError> {
    let mut cmd = CP.command()?;
    cmd.arg("--reflink=auto").arg("--sparse=always").arg("--").arg(src).arg(dst);
    run(&CP, cmd).map(|_| ())
}

/// `sgdisk -e`: move the GPT backup header to the end of the disk.
pub fn sgdisk_relocate_backup(path: &Path) -> Result<(), ImageError> {
    let mut cmd = SGDISK.command()?;
    cmd.arg("-e").arg(path);
    run(&SGDISK, cmd).map(|_| ())
}

/// Result of `growpart`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrowOutcome {
    Changed,
    /// The partition already fills the space after it.
    NoChange,
}

pub fn growpart(path: &Path, partition: u32) -> Result<GrowOutcome, ImageError> {
    let mut cmd = GROWPART.command()?;
    cmd.arg(path).arg(partition.to_string());
    let out = cmd
        .output()
        .map_err(|e| ImageError::Io(format!("growpart: {}", e)))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    match out.status.code() {
        Some(0) => Ok(GrowOutcome::Changed),
        // Exit 1 + NOCHANGE is growpart's "nothing to do".
        Some(1) if stdout.contains("NOCHANGE") => Ok(GrowOutcome::NoChange),
        _ => Err(tool_failure(&GROWPART, &out)),
    }
}
