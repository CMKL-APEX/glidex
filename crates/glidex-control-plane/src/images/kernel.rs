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
}
