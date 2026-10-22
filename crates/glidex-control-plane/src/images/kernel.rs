//! Kernel images (spec/kernel-boot.md §4): what a kernel file must look
//! like (K8) and the size limits of a kernel and its initramfs.
//!
//! Only the header is read. A kernel is loaded into guest memory by the
//! hypervisor; a hostile one harms only its own guest, so the check is
//! there to refuse files that aren't kernels before a VM uses them, not to
//! vouch for them.

use super::ImageError;
use super::catalog::Arch;

/// A kernel file's size limit (`GLIDEX_MAX_KERNEL_SIZE`).
pub const MAX_KERNEL_SIZE: u64 = 128 << 20;
/// An initramfs's size limit (`GLIDEX_MAX_INITRD_SIZE`).
pub const MAX_INITRD_SIZE: u64 = 1 << 30;
/// How much of a kernel file [`inspect`] needs: a bzImage's version string
/// is in its setup sectors.
pub const HEADER_LEN: usize = 64 << 10;

/// What a kernel's header says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelHeader {
    pub arch: Arch,
    /// The release (`6.8.0-147-generic`), when the format carries one: an
    /// x86 bzImage does; ELF and arm64 `Image` don't.
    pub release: Option<String>,
}

/// Check that `head` (the first [`HEADER_LEN`] bytes, or the whole file
/// if shorter) is a kernel glidex can boot (K8): an x86_64 bzImage with a
/// 64-bit entry point, an ELF `vmlinux` for x86_64 or aarch64, or an arm64
/// `Image` (EFI stub or not).
pub fn inspect(head: &[u8]) -> Result<KernelHeader, ImageError> {
    let u16_at = |off: usize| {
        head.get(off..off + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    if head.get(0x202..0x206) == Some(b"HdrS") {
        // Boot protocol 2.12+ has `xloadflags`; bit 0 is a 64-bit entry.
        let protocol = u16_at(0x206).unwrap_or(0);
        let xloadflags = u16_at(0x236).unwrap_or(0);
        if protocol < 0x20c || xloadflags & 1 == 0 {
            return Err(ImageError::InvalidImage(
                "bzImage without a 64-bit entry point (boot protocol 2.12 or later)".into(),
            ));
        }
        let release = u16_at(0x20e)
            .filter(|&p| p != 0)
            .and_then(|p| head.get(0x200 + p as usize..))
            .and_then(|s| s.split(|&b| b == 0 || b == b' ').next())
            .and_then(|s| std::str::from_utf8(s).ok())
            .filter(|s| is_release(s))
            .map(str::to_string);
        return Ok(KernelHeader {
            arch: Arch::X86_64,
            release,
        });
    }
    if head.get(..4) == Some(b"\x7fELF") {
        // ELF64, little-endian, then e_machine.
        if head.get(4) != Some(&2) || head.get(5) != Some(&1) {
            return Err(ImageError::InvalidImage(
                "ELF kernel that isn't 64-bit little-endian".into(),
            ));
        }
        let arch = match u16_at(0x12) {
            Some(0x3e) => Arch::X86_64,
            Some(0xb7) => Arch::Aarch64,
            m => {
                return Err(ImageError::InvalidImage(format!(
                    "ELF kernel for machine {:#x}, not x86_64 or aarch64",
                    m.unwrap_or(0)
                )));
            }
        };
        return Ok(KernelHeader {
            arch,
            release: None,
        });
    }
    if head.get(0x38..0x3c) == Some(b"ARM\x64") {
        return Ok(KernelHeader {
            arch: Arch::Aarch64,
            release: None,
        });
    }
    Err(ImageError::InvalidImage(
        "not a kernel: want an x86_64 bzImage, an ELF vmlinux or an arm64 Image".into(),
    ))
}

/// The plain arm64 `Image` inside `data`, when it is wrapped (K8): a stub
/// PE whose `.linux` section holds the kernel (Ubuntu's), the kernel's own
/// EFI zboot image (`MZ`, `zimg`) with a gzip or zstd payload, or a gzip
/// stream. Cloud Hypervisor boots only the plain `Image`, so that is what
/// gets installed. `None` when `data` is one already, or none of these
/// ([`inspect`] then decides). Nothing decompresses past `limit` bytes.
pub fn unwrap_arm64(data: &[u8], limit: u64) -> Result<Option<Vec<u8>>, ImageError> {
    let mut cur: Option<Vec<u8>> = None;
    // A stub around a zboot around a compressed Image, at most.
    for _ in 0..4 {
        let d = cur.as_deref().unwrap_or(data);
        if d.get(0x38..0x3c) == Some(b"ARM\x64") {
            return Ok(cur);
        }
        let next = if d.starts_with(&[0x1f, 0x8b]) {
            decompress("gzip", d, limit)?
        } else if d.starts_with(b"MZ") && d.get(4..8) == Some(b"zimg") {
            zboot_payload(d, limit)?
        } else if let Some(linux) = pe_section(d, b".linux") {
            linux.to_vec()
        } else {
            return Ok(cur);
        };
        cur = Some(next);
    }
    Err(ImageError::InvalidImage(
        "arm64 kernel wrapped too many times".into(),
    ))
}

/// An EFI zboot image's payload, decompressed: its offset and size at 8
/// and 12, the compression's name at 24 (linux's zboot-header.S).
fn zboot_payload(d: &[u8], limit: u64) -> Result<Vec<u8>, ImageError> {
    let u32_at = |off: usize| {
        d.get(off..off + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let (Some(off), Some(size)) = (u32_at(8), u32_at(12)) else {
        return Err(ImageError::InvalidImage("truncated zboot header".into()));
    };
    let kind = d
        .get(24..32)
        .map(|b| {
            String::from_utf8_lossy(b)
                .trim_end_matches('\0')
                .to_string()
        })
        .unwrap_or_default();
    let payload = off
        .checked_add(size)
        .and_then(|end| d.get(off..end))
        .ok_or_else(|| ImageError::InvalidImage("zboot payload outside the file".into()))?;
    decompress(&kind, payload, limit)
}

fn decompress(kind: &str, data: &[u8], limit: u64) -> Result<Vec<u8>, ImageError> {
    use std::io::Read;
    let reader: Box<dyn Read + '_> = match kind {
        "gzip" => Box::new(flate2::read::GzDecoder::new(data)),
        "zstd" => Box::new(
            zstd::stream::read::Decoder::new(data)
                .map_err(|e| ImageError::InvalidImage(format!("zstd kernel: {e}")))?,
        ),
        other => {
            return Err(ImageError::InvalidImage(format!(
                "arm64 kernel compressed with {other:?}; glidex reads gzip and zstd"
            )));
        }
    };
    let mut out = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut out)
        .map_err(|e| ImageError::InvalidImage(format!("{kind} kernel: {e}")))?;
    if out.len() as u64 > limit {
        return Err(ImageError::InvalidImage(format!(
            "the kernel decompresses to more than {limit} bytes"
        )));
    }
    Ok(out)
}

/// The raw data of section `name` of an arm64 PE file.
fn pe_section<'a>(d: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let u16_at = |off: usize| {
        d.get(off..off + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
    };
    let u32_at = |off: usize| {
        d.get(off..off + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    if !d.starts_with(b"MZ") {
        return None;
    }
    let pe = u32_at(0x3c)?;
    if d.get(pe..pe + 4)? != b"PE\0\0" || u16_at(pe + 4)? != 0xaa64 {
        return None;
    }
    let table = pe + 24 + u16_at(pe + 20)?;
    (0..u16_at(pe + 6)?).find_map(|i| {
        let s = table + i * 40;
        let n = d.get(s..s + 8)?;
        if n.split(|&b| b == 0).next() != Some(name) {
            return None;
        }
        let (size, at) = (u32_at(s + 16)?, u32_at(s + 20)?);
        d.get(at..at.checked_add(size)?)
    })
}

/// A kernel release as `uname -r` prints it: printable, no spaces.
fn is_release(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_digit())
        && s.bytes().all(|b| b.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bzImage header: boot protocol 2.15, 64-bit, with `version` at the
    /// `kernel_version` offset.
    pub fn bzimage(version: &str) -> Vec<u8> {
        let mut b = vec![0u8; 0x1000];
        b[0x202..0x206].copy_from_slice(b"HdrS");
        b[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
        b[0x236..0x238].copy_from_slice(&0x7fu16.to_le_bytes());
        let off = 0x400u16;
        b[0x20e..0x210].copy_from_slice(&(off - 0x200).to_le_bytes());
        let s = format!("{version} (buildd@host) #1 SMP");
        b[off as usize..off as usize + s.len()].copy_from_slice(s.as_bytes());
        b
    }

    #[test]
    fn bzimage_with_release() {
        let h = inspect(&bzimage("6.8.0-147-generic")).unwrap();
        assert_eq!(h.arch, Arch::X86_64);
        assert_eq!(h.release.as_deref(), Some("6.8.0-147-generic"));
    }

    #[test]
    fn bzimage_needs_a_64_bit_entry() {
        let mut b = bzimage("6.8.0");
        b[0x236] = 0;
        assert!(inspect(&b).is_err());
        let mut b = bzimage("6.8.0");
        b[0x206..0x208].copy_from_slice(&0x020bu16.to_le_bytes());
        assert!(inspect(&b).is_err());
    }

    #[test]
    fn bzimage_with_a_bad_version_string_has_no_release() {
        let mut b = bzimage("6.8.0");
        b[0x20e..0x210].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(inspect(&b).unwrap().release, None);
        let mut b = bzimage("6.8.0");
        b[0x400] = 0x01;
        assert_eq!(inspect(&b).unwrap().release, None);
        // A pointer past the header: no release, no panic.
        let mut b = bzimage("6.8.0");
        b[0x20e..0x210].copy_from_slice(&0xfff0u16.to_le_bytes());
        assert_eq!(inspect(&b).unwrap().release, None);
    }

    #[test]
    fn elf_and_arm64_image() {
        let mut elf = vec![0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[0x12..0x14].copy_from_slice(&0x3eu16.to_le_bytes());
        assert_eq!(inspect(&elf).unwrap().arch, Arch::X86_64);
        elf[0x12..0x14].copy_from_slice(&0xb7u16.to_le_bytes());
        assert_eq!(inspect(&elf).unwrap().arch, Arch::Aarch64);
        elf[0x12..0x14].copy_from_slice(&0x28u16.to_le_bytes()); // 32-bit ARM
        assert!(inspect(&elf).is_err());
        elf[4] = 1;
        assert!(inspect(&elf).is_err());

        let mut image = vec![0u8; 64];
        image[..2].copy_from_slice(b"MZ");
        image[0x38..0x3c].copy_from_slice(b"ARM\x64");
        assert_eq!(
            inspect(&image).unwrap(),
            KernelHeader {
                arch: Arch::Aarch64,
                release: None
            }
        );
    }

    #[test]
    fn junk_is_not_a_kernel() {
        assert!(inspect(&[]).is_err());
        assert!(inspect(&[0u8; 4096]).is_err());
        assert!(inspect(b"QFI\xfb\x00\x00\x00\x03").is_err());
    }

    /// A plain arm64 `Image`: the EFI stub's `MZ` and the magic at 0x38.
    fn arm64_image() -> Vec<u8> {
        let mut image = vec![0u8; 8192];
        image[..2].copy_from_slice(b"MZ");
        image[0x38..0x3c].copy_from_slice(b"ARM\x64");
        image[100..108].copy_from_slice(b"payload!");
        image
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// An EFI zboot image around `payload` compressed as `kind`.
    fn zboot(kind: &str, payload: &[u8]) -> Vec<u8> {
        let mut z = vec![0u8; 64];
        z[..2].copy_from_slice(b"MZ");
        z[4..8].copy_from_slice(b"zimg");
        z[8..12].copy_from_slice(&64u32.to_le_bytes());
        z[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        z[24..24 + kind.len()].copy_from_slice(kind.as_bytes());
        z.extend_from_slice(payload);
        z
    }

    /// A stub PE (as Ubuntu's arm64 vmlinuz) with sections `.text` and
    /// `.linux`, the latter holding `linux`.
    fn stub(linux: &[u8]) -> Vec<u8> {
        let mut d = vec![0u8; 0x200];
        d[..2].copy_from_slice(b"MZ");
        d[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        d[0x80..0x84].copy_from_slice(b"PE\0\0");
        d[0x84..0x86].copy_from_slice(&0xaa64u16.to_le_bytes());
        d[0x86..0x88].copy_from_slice(&2u16.to_le_bytes());
        // No optional header: the section table follows at 0x80 + 24.
        let table = 0x80 + 24;
        let at = d.len() as u32;
        for (i, (name, size, ptr)) in [
            (&b".text"[..], 16u32, 0x100u32),
            (b".linux", linux.len() as u32, at),
        ]
        .into_iter()
        .enumerate()
        {
            let s = table + i * 40;
            d[s..s + name.len()].copy_from_slice(name);
            d[s + 16..s + 20].copy_from_slice(&size.to_le_bytes());
            d[s + 20..s + 24].copy_from_slice(&ptr.to_le_bytes());
        }
        d.extend_from_slice(linux);
        d
    }

    #[test]
    fn arm64_kernels_are_unwrapped_to_the_image() {
        let image = arm64_image();
        let limit = 1 << 20;
        assert_eq!(unwrap_arm64(&image, limit).unwrap(), None, "already plain");
        assert_eq!(
            unwrap_arm64(&gzip(&image), limit).unwrap(),
            Some(image.clone())
        );
        let zstd = zstd::encode_all(&image[..], 3).unwrap();
        assert_eq!(
            unwrap_arm64(&zboot("zstd", &zstd), limit).unwrap(),
            Some(image.clone())
        );
        // Ubuntu's: a stub whose .linux is a zboot.
        let ubuntu = stub(&zboot("gzip", &gzip(&image)));
        let plain = unwrap_arm64(&ubuntu, limit).unwrap().unwrap();
        assert_eq!(plain, image);
        assert_eq!(inspect(&plain).unwrap().arch, Arch::Aarch64);
        // Not a kernel at all: left for inspect to refuse.
        assert_eq!(unwrap_arm64(b"hello", limit).unwrap(), None);
    }

    #[test]
    fn arm64_unwrapping_refuses_what_it_cant_read() {
        let image = arm64_image();
        let err = unwrap_arm64(&zboot("lz4", &image), 1 << 20).unwrap_err();
        assert!(err.to_string().contains("lz4"), "{err}");
        // A decompression bomb stops at the limit.
        let err = unwrap_arm64(&gzip(&vec![0u8; 1 << 20]), 4096).unwrap_err();
        assert!(err.to_string().contains("more than 4096"), "{err}");
        // A payload outside the file.
        let mut z = zboot("gzip", &gzip(&image));
        z[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(unwrap_arm64(&z, 1 << 20).is_err());
    }
}
