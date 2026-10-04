//! Built-in catalog of UEFI firmware (spec/images.md §4.1).
//!
//! Firmware is pinned, unlike cloud images: one tested build is what a
//! guest should boot, so each downloadable entry carries its digest
//! (bumped together with the installer's `EDK2_FIRMWARE_VERSION`). OVMF for
//! QEMU comes from the host's `ovmf` / `edk2-ovmf` package instead, which
//! the package manager verified: pulling it copies the code image and its
//! variable-store template into the image directory.

use super::catalog::Arch;
use crate::hypervisor::HypervisorType;
use std::path::PathBuf;

/// Release of https://github.com/cloud-hypervisor/edk2/releases the
/// `cloudhv-edk2` entry pins (same as `glidex-install`).
pub const EDK2_FIRMWARE_VERSION: &str = "ch-811ce5ea35";

/// Largest firmware file accepted (OVMF 4M builds are 4 MiB).
pub const MAX_FIRMWARE_SIZE: u64 = 64 << 20;

#[derive(Debug, Clone, Copy)]
pub enum FirmwareSource {
    /// A pinned download: `(arch, url, sha256)` per architecture.
    Download { version: &'static str, files: &'static [(Arch, &'static str, &'static str)] },
    /// The host's firmware package: the first existing code image.
    Host { package: &'static str, candidates: &'static [&'static str] },
}

#[derive(Debug, Clone, Copy)]
pub struct FirmwareEntry {
    pub key: &'static str,
    pub name: &'static str,
    pub hypervisor: HypervisorType,
    pub source: FirmwareSource,
}

pub const FIRMWARE_CATALOG: &[FirmwareEntry] = &[
    FirmwareEntry {
        key: "cloudhv-edk2",
        name: "Cloud-Hypervisor EDK2 UEFI",
        hypervisor: HypervisorType::CloudHypervisor,
        source: FirmwareSource::Download {
            version: EDK2_FIRMWARE_VERSION,
            files: &[
                (
                    Arch::X86_64,
                    "https://github.com/cloud-hypervisor/edk2/releases/download/ch-811ce5ea35/CLOUDHV.fd",
                    "db5c16e374efab916910a87e0d800fd94b4a55c32bc87e01481a300e5196136b",
                ),
                (
                    Arch::Aarch64,
                    "https://github.com/cloud-hypervisor/edk2/releases/download/ch-811ce5ea35/CLOUDHV_EFI.fd",
                    "43570f9d7f8f8b87e0218956daa8f652260273d811ff791727215569f5812220",
                ),
            ],
        },
    },
    FirmwareEntry {
        key: "ovmf",
        name: "OVMF UEFI (host package)",
        hypervisor: HypervisorType::Qemu,
        source: FirmwareSource::Host { package: "ovmf / edk2-ovmf", candidates: crate::hypervisor::qemu::OVMF_CODE_CANDIDATES },
    },
];

pub fn find(key: &str) -> Option<&'static FirmwareEntry> {
    FIRMWARE_CATALOG.iter().find(|e| e.key == key)
}

impl FirmwareEntry {
    /// `(url, sha256)` of the pinned download for `arch`.
    pub fn download_for(&self, arch: Arch) -> Option<(&'static str, &'static str)> {
        match self.source {
            FirmwareSource::Download { files, .. } => files.iter().find(|f| f.0 == arch).map(|f| (f.1, f.2)),
            FirmwareSource::Host { .. } => None,
        }
    }

    /// The host file a `Host` entry imports, if installed.
    pub fn host_file(&self) -> Option<PathBuf> {
        match self.source {
            FirmwareSource::Host { candidates, .. } => candidates.iter().map(PathBuf::from).find(|p| p.is_file()),
            FirmwareSource::Download { .. } => None,
        }
    }

    /// Whether this entry can be pulled on a host of `arch`.
    pub fn supports(&self, arch: Arch) -> bool {
        match self.source {
            FirmwareSource::Download { .. } => self.download_for(arch).is_some(),
            // QEMU is driven as qemu-system-x86_64 (hypervisor/qemu.rs).
            FirmwareSource::Host { .. } => arch == Arch::X86_64,
        }
    }

    pub fn version(&self) -> &'static str {
        match self.source {
            FirmwareSource::Download { version, .. } => version,
            FirmwareSource::Host { .. } => "",
        }
    }
}

/// The file `glidex-install` leaves in `~/.glidex` for `cloudhv-edk2`: a
/// pull copies it instead of downloading when its digest matches the pin.
pub fn installer_copy(arch: Arch) -> Option<PathBuf> {
    let name = match arch {
        Arch::X86_64 => "CLOUDHV.fd",
        Arch::Aarch64 => "CLOUDHV_EFI.fd",
    };
    dirs::home_dir().map(|h| h.join(".glidex").join(name)).filter(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_pinned_https_or_host() {
        let mut keys: Vec<_> = FIRMWARE_CATALOG.iter().map(|e| e.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), FIRMWARE_CATALOG.len(), "duplicate firmware keys");
        for e in FIRMWARE_CATALOG {
            if let FirmwareSource::Download { files, .. } = e.source {
                for (_, url, sha) in files {
                    assert!(url.starts_with("https://"), "{}", e.key);
                    assert!(url.contains(e.version()), "{} URL is not the pinned version", e.key);
                    assert!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()), "{}", e.key);
                }
            }
        }
        assert!(find("cloudhv-edk2").unwrap().download_for(Arch::X86_64).is_some());
        assert!(!find("ovmf").unwrap().supports(Arch::Aarch64));
    }
}
