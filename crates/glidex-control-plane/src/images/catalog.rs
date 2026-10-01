//! Built-in catalog of distro cloud images.
//!
//! Entries point at each vendor's "current/latest" build and the checksum
//! file published next to it, not at pinned digests: cloud images are
//! rebuilt weekly with security fixes, so a pinned build goes stale. The
//! checksum is fetched over HTTPS from the same vendor host and the image
//! is verified against it (spec §4).

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn host() -> Option<Arch> {
        match std::env::consts::ARCH {
            "x86_64" => Some(Arch::X86_64),
            "aarch64" => Some(Arch::Aarch64),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HashAlgo {
    Sha256,
    Sha512,
}

/// Where an entry's expected digest comes from.
#[derive(Debug, Clone, Copy)]
pub enum ChecksumSource {
    /// A `SHA256SUMS`-style file (GNU or BSD lines) listing the image's
    /// file name.
    File { url: &'static str, algo: HashAlgo },
    /// Fedora's `releases.json`, which carries both URL and sha256.
    FedoraReleases,
}

/// How the build behind "current/latest" is identified.
#[derive(Debug, Clone, Copy)]
pub enum VersionSource {
    /// Ubuntu's `unpacked/build-info.txt`, `serial=` line.
    UbuntuBuildInfo(&'static str),
    /// Another file name in the checksum file with the same digest (e.g.
    /// AlmaLinux's `…-latest…` is a copy of `…-9.6-20250522…`).
    ChecksumAlias,
    /// The image response's `Last-Modified` date.
    LastModified,
    /// The file name in `releases.json`.
    FedoraFileName,
}

#[derive(Debug, Clone, Copy)]
pub struct ArchImage {
    pub arch: Arch,
    pub url: &'static str,
    pub checksum: ChecksumSource,
    pub version: VersionSource,
}

#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    pub key: &'static str,
    pub distro: &'static str,
    pub release: &'static str,
    pub images: &'static [ArchImage],
}

impl CatalogEntry {
    pub fn for_arch(&self, arch: Arch) -> Option<&ArchImage> {
        self.images.iter().find(|i| i.arch == arch)
    }
}

macro_rules! ubuntu {
    ($key:expr, $release:expr, $suite:literal) => {
        CatalogEntry {
            key: $key,
            distro: "Ubuntu",
            release: $release,
            images: &[
                ArchImage {
                    arch: Arch::X86_64,
                    url: concat!("https://cloud-images.ubuntu.com/", $suite, "/current/", $suite, "-server-cloudimg-amd64.img"),
                    checksum: ChecksumSource::File {
                        url: concat!("https://cloud-images.ubuntu.com/", $suite, "/current/SHA256SUMS"),
                        algo: HashAlgo::Sha256,
                    },
                    version: VersionSource::UbuntuBuildInfo(concat!(
                        "https://cloud-images.ubuntu.com/", $suite, "/current/unpacked/build-info.txt"
                    )),
                },
                ArchImage {
                    arch: Arch::Aarch64,
                    url: concat!("https://cloud-images.ubuntu.com/", $suite, "/current/", $suite, "-server-cloudimg-arm64.img"),
                    checksum: ChecksumSource::File {
                        url: concat!("https://cloud-images.ubuntu.com/", $suite, "/current/SHA256SUMS"),
                        algo: HashAlgo::Sha256,
                    },
                    version: VersionSource::UbuntuBuildInfo(concat!(
                        "https://cloud-images.ubuntu.com/", $suite, "/current/unpacked/build-info.txt"
                    )),
                },
            ],
        }
    };
}

macro_rules! debian {
    ($key:expr, $release:expr, $suite:literal, $n:literal) => {
        CatalogEntry {
            key: $key,
            distro: "Debian",
            release: $release,
            images: &[
                ArchImage {
                    arch: Arch::X86_64,
                    url: concat!("https://cloud.debian.org/images/cloud/", $suite, "/latest/debian-", $n, "-generic-amd64.qcow2"),
                    checksum: ChecksumSource::File {
                        url: concat!("https://cloud.debian.org/images/cloud/", $suite, "/latest/SHA512SUMS"),
                        algo: HashAlgo::Sha512,
                    },
                    version: VersionSource::LastModified,
                },
                ArchImage {
                    arch: Arch::Aarch64,
                    url: concat!("https://cloud.debian.org/images/cloud/", $suite, "/latest/debian-", $n, "-generic-arm64.qcow2"),
                    checksum: ChecksumSource::File {
                        url: concat!("https://cloud.debian.org/images/cloud/", $suite, "/latest/SHA512SUMS"),
                        algo: HashAlgo::Sha512,
                    },
                    version: VersionSource::LastModified,
                },
            ],
        }
    };
}

pub const CATALOG: &[CatalogEntry] = &[
    ubuntu!("ubuntu-26.04", "26.04 LTS (Resolute Raccoon)", "resolute"),
    ubuntu!("ubuntu-24.04", "24.04 LTS (Noble Numbat)", "noble"),
    ubuntu!("ubuntu-22.04", "22.04 LTS (Jammy Jellyfish)", "jammy"),
    debian!("debian-13", "13 (Trixie)", "trixie", "13"),
    debian!("debian-12", "12 (Bookworm)", "bookworm", "12"),
    CatalogEntry {
        key: "fedora-cloud",
        distro: "Fedora",
        release: "Cloud Base (latest release)",
        images: &[
            ArchImage {
                arch: Arch::X86_64,
                url: "https://fedoraproject.org/releases.json",
                checksum: ChecksumSource::FedoraReleases,
                version: VersionSource::FedoraFileName,
            },
            ArchImage {
                arch: Arch::Aarch64,
                url: "https://fedoraproject.org/releases.json",
                checksum: ChecksumSource::FedoraReleases,
                version: VersionSource::FedoraFileName,
            },
        ],
    },
    CatalogEntry {
        key: "alma-9",
        distro: "AlmaLinux",
        release: "9 (GenericCloud)",
        images: &[
            ArchImage {
                arch: Arch::X86_64,
                url: "https://repo.almalinux.org/almalinux/9/cloud/x86_64/images/AlmaLinux-9-GenericCloud-latest.x86_64.qcow2",
                checksum: ChecksumSource::File {
                    url: "https://repo.almalinux.org/almalinux/9/cloud/x86_64/images/CHECKSUM",
                    algo: HashAlgo::Sha256,
                },
                version: VersionSource::ChecksumAlias,
            },
            ArchImage {
                arch: Arch::Aarch64,
                url: "https://repo.almalinux.org/almalinux/9/cloud/aarch64/images/AlmaLinux-9-GenericCloud-latest.aarch64.qcow2",
                checksum: ChecksumSource::File {
                    url: "https://repo.almalinux.org/almalinux/9/cloud/aarch64/images/CHECKSUM",
                    algo: HashAlgo::Sha256,
                },
                version: VersionSource::ChecksumAlias,
            },
        ],
    },
];

pub fn find(key: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.key == key)
}

/// Entries that have an image for `arch`.
pub fn for_arch(arch: Arch) -> impl Iterator<Item = (&'static CatalogEntry, &'static ArchImage)> {
    CATALOG.iter().filter_map(move |e| e.for_arch(arch).map(|i| (e, i)))
}

/// Parse a checksum file into `(file name, lower-case hex digest)` pairs.
/// Accepts GNU `sha256sum` lines (`<hex>  <name>` / `<hex> *<name>`) and
/// BSD lines (`SHA256 (<name>) = <hex>`); comments and anything else are
/// ignored.
pub fn parse_checksums(text: &str) -> Vec<(String, String)> {
    let is_hex = |s: &str| s.len() >= 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // BSD: ALGO (name) = hex
        if let (Some(open), Some(close)) = (line.find(" ("), line.rfind(") = ")) {
            if open < close {
                let name = &line[open + 2..close];
                let hex = line[close + 4..].trim();
                if is_hex(hex) {
                    out.push((name.to_string(), hex.to_ascii_lowercase()));
                }
                continue;
            }
        }
        // GNU: hex, whitespace, optional '*' (binary mode), name
        if let Some((hex, rest)) = line.split_once(char::is_whitespace) {
            let name = rest.trim_start().trim_start_matches('*');
            if is_hex(hex) && !name.is_empty() {
                out.push((name.to_string(), hex.to_ascii_lowercase()));
            }
        }
    }
    out
}

/// File name at the end of a URL path.
pub fn url_file_name(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path)
}

/// Pick the newest Fedora Cloud Base qcow2 for `arch` from `releases.json`:
/// returns `(url, sha256, file name)`. Pre-releases ("45 Beta") are skipped.
pub fn parse_fedora_releases(json: &serde_json::Value, arch: Arch) -> Option<(String, String, String)> {
    let arch = match arch {
        Arch::X86_64 => "x86_64",
        Arch::Aarch64 => "aarch64",
    };
    json.as_array()?
        .iter()
        .filter(|e| {
            e["variant"] == "Cloud"
                && e["subvariant"] == "Cloud_Base"
                && e["arch"] == arch
                && e["link"].as_str().is_some_and(|l| l.ends_with(".qcow2"))
        })
        .filter_map(|e| {
            let version: u32 = e["version"].as_str()?.parse().ok()?;
            let link = e["link"].as_str()?.to_string();
            let sha = e["sha256"].as_str()?.to_ascii_lowercase();
            Some((version, link, sha))
        })
        .max_by_key(|(v, _, _)| *v)
        .map(|(_, link, sha)| {
            let name = url_file_name(&link).to_string();
            (link, sha, name)
        })
}

/// Ubuntu `build-info.txt` → serial.
pub fn parse_build_info(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("serial="))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_is_https_with_checksum_for_both_arches() {
        for e in CATALOG {
            for arch in [Arch::X86_64, Arch::Aarch64] {
                let img = e.for_arch(arch).unwrap_or_else(|| panic!("{} has no {:?} image", e.key, arch));
                assert!(img.url.starts_with("https://"), "{}", e.key);
                if let ChecksumSource::File { url, .. } = img.checksum {
                    assert!(url.starts_with("https://"), "{}", e.key);
                }
            }
        }
        let mut keys: Vec<_> = CATALOG.iter().map(|e| e.key).collect();
        keys.dedup();
        assert_eq!(keys.len(), CATALOG.len(), "duplicate catalog keys");
        assert!(find("ubuntu-26.04").is_some());
    }

    #[test]
    fn gnu_and_bsd_checksum_lines() {
        let sums = "\
# comment
059a3c42617171be59960d0305d91cf984483e605f031432e9c003ca03521361 *resolute-server-cloudimg-amd64.img
a733e7d49442a03e70d03e4eb5aaf3967f3efc69ef70952f9bb10fc1ee2c4876eb95956b5ad2d31350e5fada768feb651352535fb8cd1233f61998a5a7d2e93c  debian-13-generic-amd64.qcow2
SHA256 (AlmaLinux-9-GenericCloud-latest.x86_64.qcow2) = 6BDAB6376D46D42E4203ACE3733EFAFC7C5D37C7CB443A6CC74750097002D74B
not a checksum line
";
        let parsed = parse_checksums(sums);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].0, "resolute-server-cloudimg-amd64.img");
        assert_eq!(parsed[1].0, "debian-13-generic-amd64.qcow2");
        assert_eq!(parsed[1].1.len(), 128);
        assert_eq!(parsed[2].0, "AlmaLinux-9-GenericCloud-latest.x86_64.qcow2");
        assert_eq!(&parsed[2].1[..8], "6bdab637");
    }

    #[test]
    fn fedora_picks_newest_final_release() {
        let json = serde_json::json!([
            {"version": "45 Beta", "arch": "x86_64", "variant": "Cloud", "subvariant": "Cloud_Base",
             "link": "https://x/45_Beta/Fedora-Cloud-Base-Generic-45_Beta-1.3.x86_64.qcow2", "sha256": "AA"},
            {"version": "44", "arch": "x86_64", "variant": "Cloud", "subvariant": "Cloud_Base",
             "link": "https://x/44/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2", "sha256": "BB"},
            {"version": "43", "arch": "x86_64", "variant": "Cloud", "subvariant": "Cloud_Base",
             "link": "https://x/43/Fedora-Cloud-Base-Generic-43-1.6.x86_64.qcow2", "sha256": "CC"},
            {"version": "44", "arch": "x86_64", "variant": "Cloud", "subvariant": "Cloud_Base_UKI",
             "link": "https://x/44/Fedora-Cloud-Base-UEFI-UKI-44-1.7.x86_64.qcow2", "sha256": "DD"},
            {"version": "44", "arch": "aarch64", "variant": "Cloud", "subvariant": "Cloud_Base",
             "link": "https://x/44/Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2", "sha256": "EE"}
        ]);
        let (url, sha, name) = parse_fedora_releases(&json, Arch::X86_64).unwrap();
        assert_eq!(url, "https://x/44/Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2");
        assert_eq!(sha, "bb");
        assert_eq!(name, "Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2");
        assert_eq!(parse_fedora_releases(&json, Arch::Aarch64).unwrap().1, "ee");
    }

    #[test]
    fn ubuntu_build_info_serial() {
        assert_eq!(
            parse_build_info("serial=20260927\norig_prefix=resolute-server-cloudimg\n").as_deref(),
            Some("20260927")
        );
        assert_eq!(parse_build_info("suite=noble\n"), None);
    }

    #[test]
    fn file_name_from_url() {
        assert_eq!(url_file_name("https://h/a/b/c.qcow2?x=1"), "c.qcow2");
        assert_eq!(url_file_name("https://h/c.img"), "c.img");
    }
}
