//! Built-in catalog of UEFI firmware (spec/images.md §4.1).
//!
//! Firmware is pinned, unlike cloud images: one tested build is what a
//! guest should boot, so each downloadable entry carries its digest
//! (bumped together with the installer's `EDK2_FIRMWARE_VERSION`). OVMF for
//! QEMU comes from the host's `ovmf` / `edk2-ovmf` package, which the
//! package manager verified (pulling it copies the code image and its
//! variable-store template into the image directory), or from a pinned
//! build of Debian's `ovmf` package, unpacked here (`extract_deb`), on any
//! host.

use super::catalog::Arch;
use super::ImageError;
use crate::hypervisor::HypervisorType;
use std::io::Read;
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
    /// A pinned Debian package (x86_64 firmware, `Architecture: all`):
    /// the `.deb` is checked against `sha256`, then the code image and its
    /// variable-store template are unpacked and checked against theirs.
    Deb { version: &'static str, url: &'static str, sha256: &'static str, code: DebFile, vars: DebFile },
}

/// A file inside a `.deb`'s data archive, with its pinned digest.
#[derive(Debug, Clone, Copy)]
pub struct DebFile {
    pub path: &'static str,
    pub sha256: &'static str,
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
    // snapshot.debian.org keeps every file at its first-seen URL forever,
    // unlike the archive's pool. The digests are Debian's (Packages index)
    // and of the unpacked files.
    FirmwareEntry {
        key: "ovmf-debian",
        name: "OVMF UEFI (Debian package)",
        hypervisor: HypervisorType::Qemu,
        source: FirmwareSource::Deb {
            version: "2025.02-8+deb13u1",
            url: "https://snapshot.debian.org/archive/debian/20260104T203950Z/pool/main/e/edk2/ovmf_2025.02-8%2Bdeb13u1_all.deb",
            sha256: "78e0d54df11fc77406cb7a0bc9a39e5bca6d1cbe06556b91d9a73491c52decdf",
            code: DebFile {
                path: "usr/share/OVMF/OVMF_CODE_4M.fd",
                sha256: "624e06de18b4fa535e90db7160d00d3d07d206422b89999bf1e27d920264e4e0",
            },
            vars: DebFile {
                path: "usr/share/OVMF/OVMF_VARS_4M.fd",
                sha256: "5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e",
            },
        },
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
            FirmwareSource::Deb { url, sha256, .. } => (arch == Arch::X86_64).then_some((url, sha256)),
            FirmwareSource::Host { .. } => None,
        }
    }

    /// The host file a `Host` entry imports, if installed.
    pub fn host_file(&self) -> Option<PathBuf> {
        match self.source {
            FirmwareSource::Host { candidates, .. } => candidates.iter().map(PathBuf::from).find(|p| p.is_file()),
            FirmwareSource::Download { .. } | FirmwareSource::Deb { .. } => None,
        }
    }

    /// Whether this entry can be pulled on a host of `arch`.
    pub fn supports(&self, arch: Arch) -> bool {
        match self.source {
            FirmwareSource::Download { .. } | FirmwareSource::Deb { .. } => self.download_for(arch).is_some(),
            // QEMU is driven as qemu-system-x86_64 (hypervisor/qemu.rs).
            FirmwareSource::Host { .. } => arch == Arch::X86_64,
        }
    }

    pub fn version(&self) -> &'static str {
        match self.source {
            FirmwareSource::Download { version, .. } | FirmwareSource::Deb { version, .. } => version,
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

/// Largest unpacked data archive `extract_deb` accepts (Debian's `ovmf`
/// is 16 MiB).
const MAX_DEB_DATA: u64 = 256 << 20;

fn invalid(msg: impl Into<String>) -> ImageError {
    ImageError::InvalidImage(msg.into())
}

/// The members of an `ar` archive (a `.deb`): `(name, contents)`.
fn ar_members(data: &[u8]) -> Result<Vec<(String, &[u8])>, ImageError> {
    let mut rest = data.strip_prefix(b"!<arch>\n").ok_or_else(|| invalid("not a .deb (no ar header)"))?;
    let mut out = Vec::new();
    while !rest.is_empty() {
        if rest.len() < 60 || &rest[58..60] != b"`\n" {
            return Err(invalid("corrupt .deb member header"));
        }
        let name = String::from_utf8_lossy(&rest[..16]).trim_end().trim_end_matches('/').to_string();
        let size: usize = std::str::from_utf8(&rest[48..58])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .ok_or_else(|| invalid("corrupt .deb member size"))?;
        let body = rest.get(60..60 + size).ok_or_else(|| invalid("truncated .deb"))?;
        out.push((name, body));
        // Members are 2-byte aligned.
        rest = rest.get(60 + size + size % 2..).unwrap_or_default();
    }
    Ok(out)
}

/// Unpack `wanted` (paths without a leading `./`) from a `.deb`'s data
/// archive (`data.tar.xz` or `data.tar`), each checked against its pinned
/// digest. Only regular files count.
pub fn extract_deb(deb: &[u8], wanted: &[DebFile]) -> Result<Vec<Vec<u8>>, ImageError> {
    let members = ar_members(deb)?;
    let (name, body) = members
        .iter()
        .find(|(n, _)| n.starts_with("data.tar"))
        .ok_or_else(|| invalid("the .deb has no data archive"))?;
    let tar_bytes = match name.as_str() {
        "data.tar" => body.to_vec(),
        "data.tar.xz" => {
            let mut out = Vec::new();
            let mut reader = std::io::BufReader::new(*body);
            lzma_rs::xz_decompress(&mut reader, &mut out).map_err(|e| invalid(format!("data.tar.xz: {}", e)))?;
            if out.len() as u64 > MAX_DEB_DATA {
                return Err(invalid("the .deb's data archive is too large"));
            }
            out
        }
        other => return Err(invalid(format!("unsupported .deb data archive {}", other))),
    };
    let mut found: Vec<Option<Vec<u8>>> = vec![None; wanted.len()];
    let mut archive = tar::Archive::new(tar_bytes.as_slice());
    for entry in archive.entries().map_err(|e| invalid(format!("data archive: {}", e)))? {
        let mut entry = entry.map_err(|e| invalid(format!("data archive: {}", e)))?;
        if entry.header().entry_type() != tar::EntryType::Regular {
            continue;
        }
        let path = entry.path().map_err(|e| invalid(e.to_string()))?.to_string_lossy().trim_start_matches("./").to_string();
        if let Some(i) = wanted.iter().position(|w| w.path == path) {
            if entry.size() > super::firmware::MAX_FIRMWARE_SIZE {
                return Err(invalid(format!("{} is too large", path)));
            }
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| invalid(e.to_string()))?;
            found[i] = Some(buf);
        }
    }
    found
        .into_iter()
        .zip(wanted)
        .map(|(f, w)| {
            let data = f.ok_or_else(|| invalid(format!("{} is not in the .deb", w.path)))?;
            let digest = super::download::sha256_hex(&data);
            if digest != w.sha256 {
                return Err(invalid(format!("{}: checksum mismatch (expected {}, got {})", w.path, w.sha256, digest)));
            }
            Ok(data)
        })
        .collect()
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
        let deb = find("ovmf-debian").unwrap();
        assert!(deb.supports(Arch::X86_64) && !deb.supports(Arch::Aarch64));
        assert!(deb.download_for(Arch::X86_64).unwrap().0.starts_with("https://snapshot.debian.org/"));
    }

    /// A `.deb` built here: `ar` with an odd-sized member (padding) and an
    /// uncompressed then an xz data archive.
    #[test]
    fn unpacks_and_checks_deb_files() {
        let mut tar_bytes = Vec::new();
        {
            let mut b = tar::Builder::new(&mut tar_bytes);
            for (path, data) in [("./usr/share/OVMF/CODE.fd", &b"code!"[..]), ("./usr/share/OVMF/VARS.fd", &b"vars"[..])] {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                b.append_data(&mut h, path, data).unwrap();
            }
            b.finish().unwrap();
        }
        let ar = |name: &str, data: &[u8]| {
            let mut out = b"!<arch>\n".to_vec();
            for (n, d) in [("debian-binary", &b"2.0\n\n"[..]), (name, data)] {
                out.extend(format!("{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n", format!("{}/", n), 0, 0, 0, 100644, d.len()).as_bytes());
                out.extend_from_slice(d);
                if d.len() % 2 == 1 {
                    out.push(b'\n');
                }
            }
            out
        };
        let code = DebFile { path: "usr/share/OVMF/CODE.fd", sha256: "a" };
        let fixed = |f: DebFile, data: &[u8]| DebFile { sha256: Box::leak(super::super::download::sha256_hex(data).into_boxed_str()), ..f };
        let code = fixed(code, b"code!");
        let vars = fixed(DebFile { path: "usr/share/OVMF/VARS.fd", sha256: "" }, b"vars");

        let plain = ar("data.tar", &tar_bytes);
        assert_eq!(extract_deb(&plain, &[code, vars]).unwrap(), vec![b"code!".to_vec(), b"vars".to_vec()]);

        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut tar_bytes.as_slice(), &mut xz).unwrap();
        let packed = ar("data.tar.xz", &xz);
        assert_eq!(extract_deb(&packed, &[vars]).unwrap(), vec![b"vars".to_vec()]);

        // A wrong digest, a missing file, and not a .deb at all.
        let bad = DebFile { sha256: "00", ..code };
        assert!(matches!(extract_deb(&packed, &[bad]), Err(ImageError::InvalidImage(m)) if m.contains("checksum mismatch")));
        let missing = DebFile { path: "usr/share/OVMF/NONE.fd", ..code };
        assert!(extract_deb(&packed, &[missing]).is_err());
        assert!(extract_deb(b"PK\x03\x04", &[code]).is_err());
    }
}
