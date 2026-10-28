//! Image download pipeline (spec §5): stream to `<id>.qcow2.part` while
//! hashing, verify the digest and the image header, convert raw to qcow2,
//! then make the file read-only and rename it into place.

use super::catalog::{self, Arch, ChecksumSource, HashAlgo, VersionSource};
use super::firmware::{self, FirmwareSource, MAX_FIRMWARE_SIZE};
use super::qemu_img::{self, DiskFormat};
use super::{FileMeta, kernel};
use super::{
    Image, ImageError, ImageKind, ImageManager, ImageSource, ImageStatus, PullImageRequest, now,
    validate_name,
};
use futures_util::StreamExt;
use sha2::{Digest, Sha256, Sha512};
use std::io::{Read, Write};
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Progress is persisted at most this often (it is cached in memory always).
const PERSIST_EVERY: Duration = Duration::from_secs(5);
/// Attempts per download, resuming with `Range` when the server allows.
const ATTEMPTS: u32 = 4;

/// Loopback, link-local, RFC 1918, CGNAT, ULA and unspecified addresses.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private(IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Whether `host` is, or resolves to, any private address. Unresolvable
/// names count as private (fail closed).
fn host_is_private(host: &str, port: u16) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_private(ip);
    }
    use std::net::ToSocketAddrs;
    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();
            addrs.is_empty() || addrs.iter().any(|a| is_private(a.ip()))
        }
        Err(_) => true,
    }
}

/// Check a URL the control plane is about to fetch for a caller (SSRF):
/// `https://` to a public address. With `allow_private`, private addresses
/// are allowed too, and only those may use plain `http://` (a LAN mirror).
pub fn check_url(url: &reqwest::Url, allow_private: bool) -> Result<(), String> {
    let host = url.host_str().ok_or("URL has no host")?;
    let port = url.port_or_known_default().unwrap_or(443);
    let private = host_is_private(host, port);
    match url.scheme() {
        "https" | "http" => {}
        other => return Err(format!("scheme {} is not allowed; use https://", other)),
    }
    if private && !allow_private {
        return Err(format!(
            "{} is a private, loopback or link-local address (set GLIDEX_ALLOW_PRIVATE_IMAGE_URLS=1 to allow)",
            host
        ));
    }
    if url.scheme() == "http" && !(allow_private && private) {
        return Err("plain http:// is only allowed to private addresses with GLIDEX_ALLOW_PRIVATE_IMAGE_URLS=1; use https://".into());
    }
    Ok(())
}

pub fn http_client(allow_private: bool) -> Result<reqwest::Client, ImageError> {
    // Every redirect hop gets the same scheme / address check as the
    // original URL, so a public URL cannot bounce to an internal one.
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("too many redirects");
        }
        match check_url(attempt.url(), allow_private) {
            Ok(()) => attempt.follow(),
            Err(e) => attempt.error(format!("redirect refused: {}", e)),
        }
    });
    reqwest::Client::builder()
        .redirect(policy)
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .user_agent(concat!("glidex/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| ImageError::Download(e.to_string()))
}

/// sha256 of `data`, lower-case hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// sha256 always (what we store), plus the vendor's algorithm if different.
#[derive(Clone)]
struct Hashers {
    sha256: Sha256,
    sha512: Option<Sha512>,
}

impl Hashers {
    fn new(expected: Option<HashAlgo>) -> Self {
        Self {
            sha256: Sha256::new(),
            sha512: (expected == Some(HashAlgo::Sha512)).then(Sha512::new),
        }
    }

    fn update(&mut self, data: &[u8]) {
        self.sha256.update(data);
        if let Some(h) = &mut self.sha512 {
            h.update(data);
        }
    }

    /// `(sha256, digest in the expected algorithm)`.
    fn finish(self, algo: Option<HashAlgo>) -> (String, Option<String>) {
        let sha256 = hex(&self.sha256.finalize());
        let other = match algo {
            Some(HashAlgo::Sha256) => Some(sha256.clone()),
            Some(HashAlgo::Sha512) => self.sha512.map(|h| hex(&h.finalize())),
            None => None,
        };
        (sha256, other)
    }
}

fn hash_file(path: &Path, hashers: &mut Hashers) -> std::io::Result<u64> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(total);
        }
        hashers.update(&buf[..n]);
        total += n as u64;
    }
}

/// SHA-256 of a file, hex.
pub fn sha256_path(path: &Path) -> Result<String, ImageError> {
    sha256_file(path)
}

fn sha256_file(path: &Path) -> Result<String, ImageError> {
    let mut h = Hashers::new(None);
    hash_file(path, &mut h).map_err(|e| ImageError::Io(format!("{}: {}", path.display(), e)))?;
    Ok(h.finish(None).0)
}

/// A caller's sha256: 64 hex characters, lower-cased.
fn parse_sha256(s: &str) -> Result<String, ImageError> {
    let s = s.trim().to_ascii_lowercase();
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ImageError::InvalidImage(
            "sha256 must be 64 hex characters".into(),
        ));
    }
    Ok(s)
}

impl ImageManager {
    /// Start a download (spec §5). Returns the record and whether it is
    /// new; an in-flight download of the same catalog key is returned as is.
    pub async fn pull_image(
        self: &Arc<Self>,
        req: PullImageRequest,
    ) -> Result<(Image, bool), ImageError> {
        // A kernel extracted from a disk image (spec/kernel-boot.md §10).
        if let Some(from) = &req.kernel_from {
            if req.catalog.is_some()
                || req.url.is_some()
                || req.firmware.is_some()
                || req.kernel_path.is_some()
                || req.initrd_path.is_some()
                || req.initrd_url.is_some()
                || req.cmdline.is_some()
                || req.with_kernel.is_some()
                || req.hypervisor.is_some()
                || req.kind.is_some_and(|k| k != ImageKind::Kernel)
            {
                return Err(ImageError::InvalidImage(
                    "kernel_from takes a disk image and, optionally, name and nodes".into(),
                ));
            }
            return self.pull_kernel_from(from, req.name.clone(), req.nodes.clone());
        }
        if req.with_kernel.is_some()
            && (req.firmware.is_some()
                || req.kernel_path.is_some()
                || req.kind.is_some_and(|k| k != ImageKind::Disk))
        {
            return Err(ImageError::InvalidImage(
                "with_kernel is for a disk image pull (catalog or url)".into(),
            ));
        }
        // In a cluster the leader downloads for any node's architecture (§9.2.1).
        let arch = match req.arch.as_deref() {
            Some(a) => Arch::parse(a).ok_or_else(|| {
                ImageError::InvalidImage(format!("unknown arch {a}: x86_64 or aarch64"))
            })?,
            None => Arch::host()
                .ok_or_else(|| ImageError::InvalidImage("unsupported host architecture".into()))?,
        };
        let firmware_url = req.kind == Some(ImageKind::Firmware);
        if req.hypervisor.is_some() && !firmware_url {
            return Err(ImageError::InvalidImage(
                "hypervisor is only for a firmware download (kind: firmware)".into(),
            ));
        }
        // A kernel image (spec/kernel-boot.md §4.2): a URL download, or a
        // copy of host files.
        let kernel = req.kind == Some(ImageKind::Kernel) || req.kernel_path.is_some();
        if req.kernel_path.is_some() && req.kind.is_some_and(|k| k != ImageKind::Kernel) {
            return Err(ImageError::InvalidImage(
                "kernel_path makes a kernel image (kind: kernel)".into(),
            ));
        }
        if !kernel
            && (req.initrd_url.is_some()
                || req.initrd_sha256.is_some()
                || req.initrd_path.is_some()
                || req.cmdline.is_some())
        {
            return Err(ImageError::InvalidImage(
                "initrd_url, initrd_sha256, initrd_path and cmdline are for a kernel image (kind: kernel)"
                    .into(),
            ));
        }
        if let Some(c) = &req.cmdline {
            crate::hypervisor::check_kernel_args(c)
                .map_err(|e| ImageError::InvalidImage(format!("cmdline: {e}")))?;
        }
        if req.initrd_path.is_some() && req.kernel_path.is_none() {
            return Err(ImageError::InvalidImage(
                "initrd_path goes with kernel_path".into(),
            ));
        }
        let initrd_sha256 = req.initrd_sha256.as_deref().map(parse_sha256).transpose()?;
        let initrd_url = match &req.initrd_url {
            None if initrd_sha256.is_some() => {
                return Err(ImageError::InvalidImage(
                    "initrd_sha256 needs initrd_url".into(),
                ));
            }
            None => None,
            Some(_) if req.url.is_none() => {
                return Err(ImageError::InvalidImage(
                    "initrd_url is for a kernel download (url, kind: kernel)".into(),
                ));
            }
            Some(u) => {
                let parsed = reqwest::Url::parse(u)
                    .map_err(|e| ImageError::InvalidImage(format!("bad initrd_url: {}", e)))?;
                let (p, allow) = (parsed.clone(), self.settings.allow_private_urls);
                tokio::task::spawn_blocking(move || check_url(&p, allow))
                    .await
                    .map_err(|e| ImageError::Io(e.to_string()))?
                    .map_err(|e| ImageError::InvalidImage(format!("initrd_url: {}", e)))?;
                Some(parsed.to_string())
            }
        };
        // Host files a kernel image is copied from: `(kernel, initramfs)`.
        let mut kernel_import: Option<(std::path::PathBuf, Option<std::path::PathBuf>)> = None;
        let vars_sha256 = req.vars_sha256.as_deref().map(parse_sha256).transpose()?;
        let vars_url = match &req.vars_url {
            None if vars_sha256.is_some() => return Err(ImageError::InvalidImage("vars_sha256 needs vars_url".into())),
            None => None,
            Some(_) if !(firmware_url && req.url.is_some() && req.hypervisor == Some(crate::hypervisor::HypervisorType::Qemu)) => {
                return Err(ImageError::InvalidImage(
                    "vars_url is for a QEMU firmware download (url, kind: firmware, hypervisor: qemu)".into(),
                ))
            }
            Some(u) => {
                let parsed = reqwest::Url::parse(u).map_err(|e| ImageError::InvalidImage(format!("bad vars_url: {}", e)))?;
                let (p, allow) = (parsed.clone(), self.settings.allow_private_urls);
                tokio::task::spawn_blocking(move || check_url(&p, allow))
                    .await
                    .map_err(|e| ImageError::Io(e.to_string()))?
                    .map_err(|e| ImageError::InvalidImage(format!("vars_url: {}", e)))?;
                Some(parsed.to_string())
            }
        };
        // What a firmware catalog entry imports from a host file instead of
        // downloading: `(code, vars template)`.
        let mut import: Option<(std::path::PathBuf, Option<std::path::PathBuf>)> = None;
        let mut hypervisor = None;
        let source = match (&req.catalog, &req.url, &req.firmware) {
            (None, None, Some(key)) => {
                let entry = firmware::find(key).ok_or_else(|| {
                    ImageError::InvalidImage(format!("unknown firmware: {}", key))
                })?;
                if !entry.supports(arch) {
                    return Err(ImageError::InvalidImage(format!(
                        "{} has no firmware for {:?}",
                        key, arch
                    )));
                }
                if let Some(existing) = self.images.read().unwrap().values().find(|i| {
                    i.firmware_key() == Some(key.as_str())
                        && i.arch == arch
                        && i.deletion_requested_at.is_none()
                        && matches!(
                            i.status,
                            ImageStatus::Downloading { .. } | ImageStatus::Verifying
                        )
                }) {
                    return Ok((existing.clone(), false));
                }
                hypervisor = Some(entry.hypervisor);
                match entry.source {
                    FirmwareSource::Download { version, .. } => {
                        let (url, sha) = entry.download_for(arch).expect("supported arch");
                        // The installer's copy of the same pinned build.
                        if let Some(local) = firmware::installer_copy(arch) {
                            let l = local.clone();
                            let digest = tokio::task::spawn_blocking(move || sha256_file(&l))
                                .await
                                .map_err(|e| ImageError::Io(e.to_string()))?;
                            if digest.ok().as_deref() == Some(sha) {
                                import = Some((local, None));
                            }
                        }
                        ImageSource::Firmware {
                            key: key.clone(),
                            url: url.to_string(),
                            version: version.to_string(),
                        }
                    }
                    FirmwareSource::Deb { version, url, .. } => ImageSource::Firmware {
                        key: key.clone(),
                        url: url.to_string(),
                        version: version.to_string(),
                    },
                    FirmwareSource::Host { package, .. } => {
                        if Some(arch) != Arch::host() {
                            return Err(ImageError::InvalidImage(format!(
                                "{key} comes from this host's {package} package, which is not for {}: pull it on a server of that architecture",
                                arch.as_str()
                            )));
                        }
                        let code = entry.host_file().ok_or_else(|| ImageError::ToolMissing {
                            tool: "OVMF firmware".into(),
                            package: package.into(),
                        })?;
                        let vars = crate::hypervisor::qemu::ovmf_vars_template(&code);
                        let url = code.to_string_lossy().into_owned();
                        import = Some((code, vars));
                        ImageSource::Firmware {
                            key: key.clone(),
                            url,
                            version: String::new(),
                        }
                    }
                }
            }
            (Some(_), None, None) if firmware_url => {
                return Err(ImageError::InvalidImage(
                    "kind: firmware takes a url or a firmware key, not a catalog key".into(),
                ));
            }
            (None, None, None) if req.kernel_path.is_some() => {
                let path = req.kernel_path.clone().unwrap_or_default();
                if !path.starts_with('/')
                    || req
                        .initrd_path
                        .as_ref()
                        .is_some_and(|p| !p.starts_with('/'))
                {
                    return Err(ImageError::InvalidImage(
                        "kernel_path and initrd_path must be absolute".into(),
                    ));
                }
                kernel_import =
                    Some((path.clone().into(), req.initrd_path.clone().map(Into::into)));
                ImageSource::Host { path }
            }
            (Some(_), None, None) if kernel => {
                return Err(ImageError::InvalidImage(
                    "kind: kernel takes a url or kernel_path, not a catalog key".into(),
                ));
            }
            (_, _, _) if req.kernel_path.is_some() => {
                return Err(ImageError::InvalidImage(
                    "kernel_path copies a host file: no catalog, firmware or url with it".into(),
                ));
            }
            (Some(key), None, None) => {
                let entry = catalog::find(key).ok_or_else(|| {
                    ImageError::InvalidImage(format!("unknown catalog image: {}", key))
                })?;
                let img = entry.for_arch(arch).ok_or_else(|| {
                    ImageError::InvalidImage(format!("{} has no image for {:?}", key, arch))
                })?;
                if let Some(existing) = self.images.read().unwrap().values().find(|i| {
                    i.catalog_key() == Some(key.as_str())
                        && i.arch == arch
                        && i.deletion_requested_at.is_none()
                        && matches!(
                            i.status,
                            ImageStatus::Downloading { .. } | ImageStatus::Verifying
                        )
                }) {
                    return Ok((existing.clone(), false));
                }
                ImageSource::Catalog {
                    key: key.clone(),
                    url: img.url.to_string(),
                    version: String::new(),
                }
            }
            (None, Some(url), None) => {
                if firmware_url {
                    hypervisor = Some(req.hypervisor.ok_or_else(|| {
                        ImageError::InvalidImage(
                            "a firmware download needs hypervisor (cloudhypervisor, qemu or pylon)"
                                .into(),
                        )
                    })?);
                }
                let parsed = reqwest::Url::parse(url)
                    .map_err(|e| ImageError::InvalidImage(format!("bad URL: {}", e)))?;
                let (u, allow) = (parsed.clone(), self.settings.allow_private_urls);
                tokio::task::spawn_blocking(move || check_url(&u, allow))
                    .await
                    .map_err(|e| ImageError::Io(e.to_string()))?
                    .map_err(ImageError::InvalidImage)?;
                let sha = req.sha256.as_deref().map(parse_sha256).transpose()?;
                ImageSource::Url {
                    url: parsed.to_string(),
                    expected_sha256: sha,
                }
            }
            _ => {
                return Err(ImageError::InvalidImage(
                    "give exactly one of catalog, firmware or url".into(),
                ));
            }
        };
        let kind = if kernel {
            ImageKind::Kernel
        } else if hypervisor.is_some() {
            ImageKind::Firmware
        } else {
            ImageKind::Disk
        };

        let name = match &req.name {
            Some(n) => n.clone(),
            None => match &source {
                // Names stay unique: another architecture's copy of the same
                // key keeps the plain name (§9.2.1).
                ImageSource::Catalog { key, .. } | ImageSource::Firmware { key, .. } => {
                    let taken_by_other_arch = self
                        .images
                        .read()
                        .unwrap()
                        .values()
                        .any(|i| i.name == *key && i.arch != arch);
                    if taken_by_other_arch {
                        format!("{key}-{}", arch.as_str())
                    } else {
                        key.clone()
                    }
                }
                // A kernel's file name carries its release
                // (`vmlinuz-6.8.0-147-generic`): keep all of it.
                ImageSource::Url { url, .. } if kernel => kernel_name(catalog::url_file_name(url)),
                ImageSource::Host { path } => {
                    kernel_name(path.rsplit('/').next().unwrap_or_default())
                }
                // `kernel_from` names its own (extract.rs).
                ImageSource::Extracted { .. } => "kernel".to_string(),
                ImageSource::Url { url, .. } => {
                    let file = catalog::url_file_name(url);
                    let stem = file.split('.').next().unwrap_or(file);
                    if stem.is_empty() {
                        "image".to_string()
                    } else {
                        stem.to_string()
                    }
                }
            },
        };
        validate_name("image", &name)?;
        if let Some(other) = self
            .images
            .read()
            .unwrap()
            .values()
            .find(|i| i.name == name)
        {
            return Err(ImageError::AlreadyExists(
                if other.deletion_requested_at.is_some() {
                    format!(
                        "an image named {} is being deleted; try again in a moment",
                        name
                    )
                } else {
                    format!(
                        "an image named {} already exists (pass a different name, or delete it first)",
                        name
                    )
                },
            ));
        }

        let mut img = Image {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            kind,
            hypervisor,
            download: super::DownloadMeta {
                expected: match &source {
                    ImageSource::Url {
                        expected_sha256: Some(s),
                        ..
                    } => Some((HashAlgo::Sha256, s.clone())),
                    ImageSource::Firmware { key, .. } if import.is_none() => firmware::find(key)
                        .and_then(|e| e.download_for(arch))
                        .map(|(_, sha)| (HashAlgo::Sha256, sha.to_string())),
                    _ => None,
                },
                url: match &source {
                    ImageSource::Url { url, .. } => url.clone(),
                    ImageSource::Firmware { url, .. } if import.is_none() => url.clone(),
                    ImageSource::Firmware { .. }
                    | ImageSource::Catalog { .. }
                    | ImageSource::Host { .. }
                    | ImageSource::Extracted { .. } => String::new(),
                },
                vars_url,
                vars_sha256,
                initrd_url,
                initrd_sha256,
                ..Default::default()
            },
            source,
            status: ImageStatus::Downloading {
                received_bytes: 0,
                total_bytes: None,
            },
            format: if kind == ImageKind::Disk {
                DiskFormat::Qcow2
            } else {
                DiskFormat::Raw
            },
            virtual_size_bytes: 0,
            file_size_bytes: 0,
            sha256: String::new(),
            arch,
            created_at: now(),
            retry_seq: 0,
            applied_retry_seq: 0,
            deletion_requested_at: None,
            pinned: req.nodes.clone(),
            release: Vec::new(),
            keep_server_copy: req.keep_server_copy.unwrap_or(true),
            initrd: None,
            cmdline: req.cmdline.clone(),
            kernel_release: None,
            for_image: None,
            owner: None,
            extract_node: None,
        };
        if let Some((kernel_file, initrd_file)) = kernel_import {
            // Copied before the record exists, so a failure leaves nothing.
            let (mgr, id, arch) = (self.clone(), img.id.clone(), img.arch);
            let installed = tokio::task::spawn_blocking(move || {
                mgr.import_kernel(&id, arch, &kernel_file, initrd_file.as_deref())
            })
            .await
            .map_err(|e| ImageError::Io(e.to_string()))??;
            installed.apply(&mut img);
            img.status = ImageStatus::Ready;
            self.insert_image(&img)?;
            tracing::info!(image = %img.name, source = ?img.source, "kernel imported");
            return Ok((img, true));
        }
        if let Some((code, vars)) = import {
            // A local copy: made before the record exists, so a failure
            // leaves nothing behind.
            let (mgr, id) = (self.clone(), img.id.clone());
            let (size, sha256) = tokio::task::spawn_blocking(move || {
                mgr.import_firmware(&id, &code, vars.as_deref())
            })
            .await
            .map_err(|e| ImageError::Io(e.to_string()))??;
            img.virtual_size_bytes = size;
            img.file_size_bytes = size;
            img.sha256 = sha256;
            img.status = ImageStatus::Ready;
            self.insert_image(&img)?;
            tracing::info!(image = %img.name, source = ?img.source, "firmware imported");
            return Ok((img, true));
        }
        self.insert_image(&img)?;
        tracing::info!(image = %img.name, source = ?img.source, "image download queued");
        self.spawn_download(img.id.clone());
        Ok((img, true))
    }

    /// Import each firmware catalog entry that has a local source, once
    /// (spec §4.1 "Auto-import"): the installer's copy of the pinned
    /// `CLOUDHV.fd`, or the host's OVMF. Never downloads. An entry is
    /// marked done once imported, or once a firmware image for its
    /// hypervisor exists, so a deleted image is not brought back.
    pub async fn auto_import_firmware(self: &Arc<Self>) -> Vec<(&'static str, super::AutoImport)> {
        use super::AutoImport;
        let Some(arch) = Arch::host() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in firmware::FIRMWARE_CATALOG
            .iter()
            .filter(|e| e.supports(arch))
        {
            let marker = format!("firmware-auto-import/{}", entry.key);
            let result = match self.meta_get(&marker) {
                Err(e) => AutoImport::Failed(e.to_string()),
                Ok(Some(_)) => AutoImport::AlreadyDone,
                Ok(None) => {
                    let existing = self.list_images().into_iter().find(|i| {
                        i.is_firmware()
                            && i.hypervisor == Some(entry.hypervisor)
                            && i.deletion_requested_at.is_none()
                    });
                    let local = match entry.source {
                        FirmwareSource::Download { .. } => {
                            let want = entry.download_for(arch).map(|(_, sha)| sha.to_string());
                            match firmware::installer_copy(arch) {
                                Some(p) => {
                                    tokio::task::spawn_blocking(move || sha256_file(&p).ok())
                                        .await
                                        .ok()
                                        .flatten()
                                        .is_some_and(|d| Some(d) == want)
                                }
                                None => false,
                            }
                        }
                        FirmwareSource::Host { .. } => entry.host_file().is_some(),
                        // Only ever downloaded: pulled by hand.
                        FirmwareSource::Deb { .. } => false,
                    };
                    match existing {
                        Some(img) => AutoImport::HaveOne(img.name),
                        None if !local => AutoImport::NoLocalSource,
                        None => {
                            let req = PullImageRequest {
                                firmware: Some(entry.key.to_string()),
                                ..Default::default()
                            };
                            match self.pull_image(req).await {
                                Ok((img, _)) => AutoImport::Imported(img.name),
                                Err(e) => AutoImport::Failed(e.to_string()),
                            }
                        }
                    }
                }
            };
            if matches!(result, AutoImport::Imported(_) | AutoImport::HaveOne(_))
                && let Err(e) = self.meta_put(&marker, now().to_string().as_bytes())
            {
                tracing::warn!(
                    firmware = entry.key,
                    "could not record the firmware auto-import: {}",
                    e
                );
            }
            out.push((entry.key, result));
        }
        out
    }

    /// Whether a download task runs for image `id`.
    pub fn download_running(&self, id: &str) -> bool {
        self.tasks.lock().unwrap().contains_key(id)
    }

    /// Start downloading image `id` unless a download for it already runs.
    /// The check and the record share the lock: the API that queues a pull
    /// and the controller that resumes one race for the same image, and two
    /// downloads into one `.part` file fail each other.
    pub(crate) fn spawn_download(self: &Arc<Self>, id: String) {
        let mgr = self.clone();
        let task_id = id.clone();
        self.spawn_task(id, async move { mgr.run_download(&task_id).await });
    }

    /// Run `work` for image `id` unless a task for it already runs (a
    /// download, or an extraction, spec/kernel-boot.md §10). If it fails,
    /// its temporary files go and an image still in progress is `Failed`;
    /// never one that finished.
    pub(crate) fn spawn_task(
        self: &Arc<Self>,
        id: String,
        work: impl std::future::Future<Output = Result<(), ImageError>> + Send + 'static,
    ) {
        let mut tasks = self.tasks.lock().unwrap();
        if tasks.contains_key(&id) {
            return;
        }
        let mgr = self.clone();
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            let result = work.await;
            mgr.tasks.lock().unwrap().remove(&task_id);
            if let Err(e) = result {
                tracing::warn!(image = %task_id, "image download or extraction failed: {}", e);
                for p in [
                    mgr.part_path(&task_id),
                    mgr.initrd_part_path(&task_id),
                    mgr.extract_out_path(&task_id),
                ] {
                    let _ = std::fs::remove_file(p);
                }
                if let Some(mut img) = mgr.get_image(&task_id).ok().filter(|i| {
                    matches!(
                        i.status,
                        ImageStatus::Downloading { .. }
                            | ImageStatus::Verifying
                            | ImageStatus::Extracting
                    )
                }) {
                    img.status = ImageStatus::Failed {
                        reason: e.to_string(),
                    };
                    let _ = mgr.put_image(&img);
                }
            }
        });
        // Still under the lock, so the task's own removal comes after this.
        tasks.insert(id, handle.abort_handle());
    }

    async fn run_download(self: &Arc<Self>, id: &str) -> Result<(), ImageError> {
        let _permit = self
            .downloads
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| ImageError::Io(e.to_string()))?;
        let mut img = self.get_image(id)?;
        if matches!(img.source, ImageSource::Extracted { .. }) {
            return Err(ImageError::InvalidImage(
                "an extracted kernel is not downloaded".into(),
            ));
        }

        if img.download.url.is_empty() {
            self.resolve_catalog(&mut img).await?;
            self.put_image(&img)?;
        }

        let part = self.part_path(id);
        let algo = img.download.expected.as_ref().map(|(a, _)| *a);
        let mut hashers = Hashers::new(algo);
        let mut received = 0u64;

        // Restart: keep the partial file only if the server can be asked
        // for "the rest of this exact file"; rehash what is already there.
        if part.exists() && (img.download.etag.is_some() || img.download.last_modified.is_some()) {
            let (p, mut h) = (part.clone(), hashers.clone());
            let (n, h) = tokio::task::spawn_blocking(move || hash_file(&p, &mut h).map(|n| (n, h)))
                .await
                .map_err(|e| ImageError::Io(e.to_string()))?
                .map_err(|e| ImageError::Io(format!("{}: {}", part.display(), e)))?;
            received = n;
            hashers = h;
        } else {
            let _ = std::fs::remove_file(&part);
        }

        let mut attempt = 0;
        loop {
            attempt += 1;
            match self
                .fetch(&mut img, &part, &mut hashers, &mut received)
                .await
            {
                Ok(()) => break,
                Err(e) if attempt < ATTEMPTS && matches!(e, ImageError::Download(_)) => {
                    tracing::warn!(image = %img.name, attempt, "download interrupted, retrying: {}", e);
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
                Err(e) => return Err(e),
            }
        }

        img.status = ImageStatus::Verifying;
        self.put_image(&img)?;
        let (sha256, expected_digest) = hashers.finish(algo);
        if let (Some((_, want)), Some(got)) = (&img.download.expected, &expected_digest)
            && want != got
        {
            return Err(ImageError::InvalidImage(format!(
                "checksum mismatch (expected {}, got {})",
                want, got
            )));
        }

        if img.is_kernel() {
            let initrd = match img.download.initrd_url.clone() {
                Some(url) => Some(
                    self.fetch_to_file(
                        "initrd_url",
                        &url,
                        img.download.initrd_sha256.as_deref(),
                        &self.initrd_part_path(id),
                        self.settings.max_initrd_size,
                    )
                    .await?,
                ),
                None => None,
            };
            let (mgr, id_owned, arch) = (self.clone(), id.to_string(), img.arch);
            let installed = tokio::task::spawn_blocking(move || {
                mgr.install_kernel(&id_owned, arch, sha256, initrd)
            })
            .await
            .map_err(|e| ImageError::Io(e.to_string()))??;
            let mut img = self.get_image(id)?;
            installed.apply(&mut img);
            img.status = ImageStatus::Ready;
            self.put_image(&img)?;
            tracing::info!(image = %img.name, release = ?img.kernel_release, "kernel image ready");
            return Ok(());
        }

        let mgr = self.clone();
        let id_owned = id.to_string();
        let max = self.settings.max_image_size;
        let firmware = img.is_firmware();
        let vars = match &img.download.vars_url {
            Some(url) => Some(
                self.fetch_vars(url, img.download.vars_sha256.as_deref())
                    .await?,
            ),
            None => None,
        };
        let deb = img
            .firmware_key()
            .and_then(firmware::find)
            .and_then(|e| match e.source {
                FirmwareSource::Deb { code, vars, .. } => Some((code, vars)),
                _ => None,
            });
        let (virtual_size, actual_size, sha256) = tokio::task::spawn_blocking(move || {
            if firmware {
                mgr.install_firmware(&id_owned, sha256, deb, vars)
            } else {
                mgr.verify_and_install(&id_owned, sha256, max)
                    .map(|(info, sha)| (info.virtual_size, info.actual_size, sha))
            }
        })
        .await
        .map_err(|e| ImageError::Io(e.to_string()))??;

        let mut img = self.get_image(id)?;
        img.virtual_size_bytes = virtual_size;
        img.file_size_bytes = actual_size;
        img.sha256 = sha256;
        img.format = if firmware {
            DiskFormat::Raw
        } else {
            DiskFormat::Qcow2
        };
        img.status = ImageStatus::Ready;
        self.put_image(&img)?;
        tracing::info!(image = %img.name, size = img.virtual_size_bytes, "image ready");
        Ok(())
    }

    /// One HTTP attempt, appending to `part`. Errors that a retry may fix
    /// are `ImageError::Download`.
    async fn fetch(
        &self,
        img: &mut Image,
        part: &Path,
        hashers: &mut Hashers,
        received: &mut u64,
    ) -> Result<(), ImageError> {
        let dl = |e: reqwest::Error| ImageError::Download(e.to_string());
        let mut req = self.http.get(&img.download.url);
        let validator = img
            .download
            .etag
            .clone()
            .or_else(|| img.download.last_modified.clone());
        let resuming = *received > 0 && validator.is_some();
        if resuming {
            req = req
                .header(reqwest::header::RANGE, format!("bytes={}-", received))
                .header(reqwest::header::IF_RANGE, validator.unwrap());
        }
        let resp = req.send().await.map_err(dl)?;
        let status = resp.status();
        let restart = match status.as_u16() {
            206 if resuming => false,
            200 => true,
            416 if resuming => {
                return Err(ImageError::Download(
                    "server refused to resume (416)".into(),
                ));
            }
            code if (500..600).contains(&code) => {
                return Err(ImageError::Download(format!("HTTP {}", status)));
            }
            _ => {
                return Err(ImageError::InvalidImage(format!(
                    "HTTP {} from {}",
                    status, img.download.url
                )));
            }
        };
        let header = |name: reqwest::header::HeaderName| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        if restart {
            // Fresh body: forget anything already written.
            *received = 0;
            *hashers = Hashers::new(img.download.expected.as_ref().map(|(a, _)| *a));
            img.download.etag = header(reqwest::header::ETAG).filter(|e| !e.starts_with("W/"));
            img.download.last_modified = header(reqwest::header::LAST_MODIFIED);
            if let ImageSource::Catalog { version, .. } = &mut img.source
                && version.is_empty()
                && let Some(lm) = &img.download.last_modified
            {
                *version = lm.clone();
            }
        }
        let total = resp.content_length().map(|n| n + *received);
        let limit = match img.kind {
            ImageKind::Firmware => MAX_FIRMWARE_SIZE,
            ImageKind::Kernel => self.settings.max_kernel_size,
            ImageKind::Disk => self.settings.max_image_size.saturating_mul(2),
        };
        if let Some(t) = total
            && t > limit
        {
            return Err(ImageError::InvalidImage(format!(
                "download is {} bytes, over the {} byte limit",
                t, limit
            )));
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(!restart)
            .truncate(restart)
            .mode(0o600)
            .open(part)
            .map_err(|e| ImageError::Io(format!("{}: {}", part.display(), e)))?;
        img.status = ImageStatus::Downloading {
            received_bytes: *received,
            total_bytes: total,
        };
        self.put_image(img)?;

        let mut last_persist = Instant::now();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(dl)?;
            file.write_all(&chunk)
                .map_err(|e| ImageError::Io(format!("{}: {}", part.display(), e)))?;
            hashers.update(&chunk);
            *received += chunk.len() as u64;
            if *received > limit {
                return Err(ImageError::InvalidImage(format!(
                    "download exceeds the {} byte limit",
                    limit
                )));
            }
            img.status = ImageStatus::Downloading {
                received_bytes: *received,
                total_bytes: total,
            };
            if last_persist.elapsed() >= PERSIST_EVERY {
                file.flush().ok();
                self.put_image(img)?;
                last_persist = Instant::now();
            } else {
                self.cache_image(img);
            }
        }
        if let Some(t) = total
            && *received < t
        {
            return Err(ImageError::Download(format!(
                "connection closed after {} of {} bytes",
                received, t
            )));
        }
        file.sync_all()
            .map_err(|e| ImageError::Io(format!("{}: {}", part.display(), e)))?;
        Ok(())
    }

    /// Fill in `download.url`, the expected digest and the version for a
    /// catalog image.
    async fn resolve_catalog(&self, img: &mut Image) -> Result<(), ImageError> {
        let ImageSource::Catalog { key, .. } = &img.source else {
            return Ok(());
        };
        let entry = catalog::find(key)
            .ok_or_else(|| ImageError::InvalidImage(format!("unknown catalog image: {}", key)))?;
        let arch_img = entry.for_arch(img.arch).ok_or_else(|| {
            ImageError::InvalidImage(format!("{} has no image for {:?}", key, img.arch))
        })?;
        let get_text = |url: &'static str| async move {
            let resp = self
                .http
                .get(url)
                .send()
                .await
                .map_err(|e| ImageError::Download(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(ImageError::Download(format!(
                    "HTTP {} from {}",
                    resp.status(),
                    url
                )));
            }
            resp.text()
                .await
                .map_err(|e| ImageError::Download(e.to_string()))
        };

        let (url, digest, version) = match arch_img.checksum {
            ChecksumSource::File {
                url: sums_url,
                algo,
            } => {
                let sums = catalog::parse_checksums(&get_text(sums_url).await?);
                let file = catalog::url_file_name(arch_img.url);
                let digest = sums
                    .iter()
                    .find(|(n, _)| n == file)
                    .map(|(_, d)| d.clone())
                    .ok_or_else(|| {
                        ImageError::Download(format!("{} is not listed in {}", file, sums_url))
                    })?;
                let version = match arch_img.version {
                    VersionSource::UbuntuBuildInfo(u) => get_text(u)
                        .await
                        .ok()
                        .and_then(|t| catalog::parse_build_info(&t))
                        .unwrap_or_default(),
                    VersionSource::ChecksumAlias => sums
                        .iter()
                        .find(|(n, d)| *d == digest && n != file && !n.contains("latest"))
                        .map(|(n, _)| n.clone())
                        .unwrap_or_default(),
                    // Filled in from the image response.
                    VersionSource::LastModified | VersionSource::FedoraFileName => String::new(),
                };
                (arch_img.url.to_string(), (algo, digest), version)
            }
            ChecksumSource::FedoraReleases => {
                let json: serde_json::Value = serde_json::from_str(&get_text(arch_img.url).await?)
                    .map_err(|e| ImageError::Download(format!("releases.json: {}", e)))?;
                let (url, sha, name) =
                    catalog::parse_fedora_releases(&json, img.arch).ok_or_else(|| {
                        ImageError::Download("no Fedora Cloud Base image in releases.json".into())
                    })?;
                (url, (HashAlgo::Sha256, sha), name)
            }
        };
        if !url.starts_with("https://") {
            return Err(ImageError::Download(format!(
                "catalog resolved to a non-https URL: {}",
                url
            )));
        }
        img.download.url = url.clone();
        img.download.expected = Some(digest);
        if let ImageSource::Catalog {
            url: u, version: v, ..
        } = &mut img.source
        {
            *u = url;
            *v = version;
        }
        Ok(())
    }

    /// Fetch a firmware download's variable-store template (small; one
    /// attempt, the image can be retried), checked against `sha256`.
    async fn fetch_vars(&self, url: &str, sha256: Option<&str>) -> Result<Vec<u8>, ImageError> {
        let dl = |e: reqwest::Error| ImageError::Download(format!("vars_url: {}", e));
        let resp = self.http.get(url).send().await.map_err(dl)?;
        if !resp.status().is_success() {
            return Err(ImageError::Download(format!(
                "vars_url: HTTP {} from {}",
                resp.status(),
                url
            )));
        }
        let mut data = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            data.extend_from_slice(&chunk.map_err(dl)?);
            if data.len() as u64 > MAX_FIRMWARE_SIZE {
                return Err(ImageError::InvalidImage(
                    "vars_url: over the firmware size limit".into(),
                ));
            }
        }
        if data.is_empty() {
            return Err(ImageError::InvalidImage("vars_url: empty file".into()));
        }
        if let Some(want) = sha256 {
            let got = sha256_hex(&data);
            if got != want {
                return Err(ImageError::InvalidImage(format!(
                    "vars_url: checksum mismatch (expected {}, got {})",
                    want, got
                )));
            }
        }
        Ok(data)
    }

    /// Write `data` as `dest`, read-only, through a temporary file.
    fn write_firmware_file(&self, id: &str, dest: &Path, data: &[u8]) -> Result<(), ImageError> {
        let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or(id);
        let tmp = self.settings.image_dir.join(format!(".{}.install", name));
        let io = |e: std::io::Error| ImageError::Io(format!("{}: {}", tmp.display(), e));
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(io)?;
        f.write_all(data).map_err(io)?;
        f.sync_all().map_err(io)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444)).map_err(io)?;
        std::fs::rename(&tmp, dest).map_err(io)
    }

    /// Install a downloaded firmware file (spec §5, firmware): no image
    /// header to check, only a plausible size; a pinned Debian package is
    /// unpacked into its code image and variable-store template, and a
    /// URL download's template (`vars`) is stored next to it. Returns
    /// `(size, size, sha256)` of the code file.
    fn install_firmware(
        &self,
        id: &str,
        sha256: String,
        deb: Option<(firmware::DebFile, firmware::DebFile)>,
        vars: Option<Vec<u8>>,
    ) -> Result<(u64, u64, String), ImageError> {
        let part = self.part_path(id);
        if let Some((code, vars)) = deb {
            let bytes = std::fs::read(&part)
                .map_err(|e| ImageError::Io(format!("{}: {}", part.display(), e)))?;
            let mut files = firmware::extract_deb(&bytes, &[code, vars])?.into_iter();
            let (code_data, vars_data) = (
                files.next().unwrap_or_default(),
                files.next().unwrap_or_default(),
            );
            self.write_firmware_file(id, &self.firmware_vars_template(id), &vars_data)?;
            self.write_firmware_file(id, &self.firmware_path(id), &code_data)?;
            let _ = std::fs::remove_file(&part);
            let size = code_data.len() as u64;
            return Ok((size, size, sha256_hex(&code_data)));
        }
        let size = std::fs::metadata(&part)
            .map_err(|e| ImageError::Io(e.to_string()))?
            .len();
        if size == 0 || size > MAX_FIRMWARE_SIZE {
            return Err(ImageError::InvalidImage(format!(
                "{} bytes is not a firmware image",
                size
            )));
        }
        if let Some(vars) = vars {
            self.write_firmware_file(id, &self.firmware_vars_template(id), &vars)?;
        }
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o444))
            .map_err(|e| ImageError::Io(e.to_string()))?;
        std::fs::rename(&part, self.firmware_path(id))
            .map_err(|e| ImageError::Io(e.to_string()))?;
        Ok((size, size, sha256))
    }

    /// Copy host firmware (and its variable-store template) into the image
    /// directory, read-only. Returns `(size, sha256)` of the code file.
    fn import_firmware(
        &self,
        id: &str,
        code: &Path,
        vars: Option<&Path>,
    ) -> Result<(u64, String), ImageError> {
        let io = |p: &Path, e: std::io::Error| ImageError::Io(format!("{}: {}", p.display(), e));
        let copy = |from: &Path, to: &Path| -> Result<u64, ImageError> {
            let size = std::fs::metadata(from).map_err(|e| io(from, e))?.len();
            if size == 0 || size > MAX_FIRMWARE_SIZE {
                return Err(ImageError::InvalidImage(format!(
                    "{} ({} bytes) is not a firmware image",
                    from.display(),
                    size
                )));
            }
            let name = to.file_name().and_then(|n| n.to_str()).unwrap_or(id);
            let tmp = self.settings.image_dir.join(format!(".{}.import", name));
            std::fs::copy(from, &tmp).map_err(|e| io(from, e))?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444))
                .map_err(|e| io(&tmp, e))?;
            std::fs::rename(&tmp, to).map_err(|e| io(to, e))?;
            Ok(size)
        };
        let dest = self.firmware_path(id);
        let size = copy(code, &dest)?;
        if let Some(vars) = vars
            && let Err(e) = copy(vars, &self.firmware_vars_template(id))
        {
            let _ = std::fs::remove_file(&dest);
            return Err(e);
        }
        Ok((size, sha256_file(&dest)?))
    }

    /// Fetch a second file of an image (a kernel's initramfs) into `dest`:
    /// one attempt (the image can be retried), at most `limit` bytes,
    /// checked against `sha256` when given.
    async fn fetch_to_file(
        &self,
        label: &str,
        url: &str,
        sha256: Option<&str>,
        dest: &Path,
        limit: u64,
    ) -> Result<FileMeta, ImageError> {
        let dl = |e: reqwest::Error| ImageError::Download(format!("{label}: {e}"));
        let io = |e: std::io::Error| ImageError::Io(format!("{}: {}", dest.display(), e));
        let resp = self.http.get(url).send().await.map_err(dl)?;
        if !resp.status().is_success() {
            return Err(ImageError::Download(format!(
                "{label}: HTTP {} from {url}",
                resp.status()
            )));
        }
        if resp.content_length().is_some_and(|n| n > limit) {
            return Err(ImageError::InvalidImage(format!(
                "{label}: over the {limit} byte limit"
            )));
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(dest)
            .map_err(io)?;
        let mut hashers = Hashers::new(None);
        let mut size = 0u64;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(dl)?;
            size += chunk.len() as u64;
            if size > limit {
                return Err(ImageError::InvalidImage(format!(
                    "{label}: over the {limit} byte limit"
                )));
            }
            file.write_all(&chunk).map_err(io)?;
            hashers.update(&chunk);
        }
        file.sync_all().map_err(io)?;
        if size == 0 {
            return Err(ImageError::InvalidImage(format!("{label}: empty file")));
        }
        let (got, _) = hashers.finish(None);
        if let Some(want) = sha256
            && got != want
        {
            return Err(ImageError::InvalidImage(format!(
                "{label}: checksum mismatch (expected {want}, got {got})"
            )));
        }
        Ok(FileMeta {
            sha256: got,
            size_bytes: size,
        })
    }

    /// Install a downloaded kernel (`<id>.qcow2.part`) and its initramfs
    /// (`<id>.initrd.part`, when there is one): the header must be a
    /// kernel for `arch` (K8). The initramfs is moved first, so a kernel
    /// file in place means a complete image. Blocking.
    pub(super) fn install_kernel(
        &self,
        id: &str,
        arch: Arch,
        sha256: String,
        initrd: Option<FileMeta>,
    ) -> Result<InstalledKernel, ImageError> {
        let part = self.part_path(id);
        let io = |p: &Path, e: std::io::Error| ImageError::Io(format!("{}: {}", p.display(), e));
        // The installed file is the plain arm64 Image, so its digest too.
        let (size, sha256) = match self.unwrap_kernel(&part, arch)? {
            Some(plain) => {
                std::fs::write(&part, &plain).map_err(|e| io(&part, e))?;
                (plain.len() as u64, sha256_hex(&plain))
            }
            None => (
                std::fs::metadata(&part).map_err(|e| io(&part, e))?.len(),
                sha256,
            ),
        };
        let header = check_kernel_file(&part, size, arch, self.settings.max_kernel_size)?;
        if initrd.is_some() {
            let from = self.initrd_part_path(id);
            std::fs::set_permissions(&from, std::fs::Permissions::from_mode(0o444))
                .map_err(|e| io(&from, e))?;
            std::fs::rename(&from, self.kernel_initrd_path(id)).map_err(|e| io(&from, e))?;
        }
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o444))
            .map_err(|e| io(&part, e))?;
        std::fs::rename(&part, self.kernel_path(id)).map_err(|e| io(&part, e))?;
        Ok(InstalledKernel {
            size,
            sha256,
            release: header.release,
            initrd,
        })
    }

    /// The plain `Image` of an aarch64 kernel file that is wrapped (a stub
    /// PE, EFI zboot or gzip; K8), or `None` to install the file as it is.
    /// One over the size limit is left for the size check to refuse.
    fn unwrap_kernel(&self, path: &Path, arch: Arch) -> Result<Option<Vec<u8>>, ImageError> {
        if arch != Arch::Aarch64 {
            return Ok(None);
        }
        let io = |e: std::io::Error| ImageError::Io(format!("{}: {}", path.display(), e));
        let limit = self.settings.max_kernel_size;
        if std::fs::metadata(path).map_err(io)?.len() > limit {
            return Ok(None);
        }
        kernel::unwrap_arm64(&std::fs::read(path).map_err(io)?, limit)
    }

    /// Copy a host kernel (and initramfs) into a kernel image, read-only,
    /// with the same checks as a download. Blocking.
    fn import_kernel(
        &self,
        id: &str,
        arch: Arch,
        kernel_file: &Path,
        initrd_file: Option<&Path>,
    ) -> Result<InstalledKernel, ImageError> {
        let io = |p: &Path, e: std::io::Error| ImageError::Io(format!("{}: {}", p.display(), e));
        let copy = |from: &Path, to: &Path, limit: u64| -> Result<FileMeta, ImageError> {
            let size = std::fs::metadata(from).map_err(|e| io(from, e))?.len();
            if size == 0 || size > limit {
                return Err(ImageError::InvalidImage(format!(
                    "{} is {} bytes (limit {})",
                    from.display(),
                    size,
                    limit
                )));
            }
            let name = to.file_name().and_then(|n| n.to_str()).unwrap_or(id);
            let tmp = self.settings.image_dir.join(format!(".{}.import", name));
            std::fs::copy(from, &tmp).map_err(|e| io(from, e))?;
            let sha256 = sha256_file(&tmp)?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444))
                .map_err(|e| io(&tmp, e))?;
            std::fs::rename(&tmp, to).map_err(|e| io(to, e))?;
            Ok(FileMeta {
                sha256,
                size_bytes: size,
            })
        };
        // A wrapped arm64 kernel is unwrapped into a scratch copy; the
        // host's file is never changed.
        let unwrapped = self
            .settings
            .image_dir
            .join(format!(".{}.unwrapped", super::safe_stem(id)));
        let kernel_file = match self.unwrap_kernel(kernel_file, arch)? {
            Some(plain) => {
                std::fs::write(&unwrapped, &plain).map_err(|e| io(&unwrapped, e))?;
                unwrapped.as_path()
            }
            None => kernel_file,
        };
        let result = self.import_checked_kernel(id, arch, kernel_file, initrd_file, &copy);
        let _ = std::fs::remove_file(&unwrapped);
        result
    }

    /// The rest of [`Self::import_kernel`], from a kernel file that is
    /// what gets installed.
    fn import_checked_kernel(
        &self,
        id: &str,
        arch: Arch,
        kernel_file: &Path,
        initrd_file: Option<&Path>,
        copy: &dyn Fn(&Path, &Path, u64) -> Result<FileMeta, ImageError>,
    ) -> Result<InstalledKernel, ImageError> {
        let io = |p: &Path, e: std::io::Error| ImageError::Io(format!("{}: {}", p.display(), e));
        let size = std::fs::metadata(kernel_file)
            .map_err(|e| io(kernel_file, e))?
            .len();
        let header = check_kernel_file(kernel_file, size, arch, self.settings.max_kernel_size)?;
        let initrd = match initrd_file {
            Some(f) => Some(copy(
                f,
                &self.kernel_initrd_path(id),
                self.settings.max_initrd_size,
            )?),
            None => None,
        };
        let dest = self.kernel_path(id);
        match copy(kernel_file, &dest, self.settings.max_kernel_size) {
            Ok(k) => Ok(InstalledKernel {
                size: k.size_bytes,
                sha256: k.sha256,
                release: header.release,
                initrd,
            }),
            Err(e) => {
                let _ = std::fs::remove_file(self.kernel_initrd_path(id));
                Err(e)
            }
        }
    }

    /// Steps 4–6 of spec §5, blocking. Returns the final `qemu-img info`
    /// and the sha256 of the installed file.
    fn verify_and_install(
        &self,
        id: &str,
        streamed_sha256: String,
        max_size: u64,
    ) -> Result<(qemu_img::ImgInfo, String), ImageError> {
        let part = self.part_path(id);
        let info = qemu_img::info(&part, None)?;
        if info.backing_filename.is_some() {
            return Err(ImageError::InvalidImage(
                "image has a backing file; refusing a downloaded image that points at host paths"
                    .into(),
            ));
        }
        if info.virtual_size > max_size {
            return Err(ImageError::InvalidImage(format!(
                "virtual size {} is over GLIDEX_MAX_IMAGE_SIZE ({})",
                info.virtual_size, max_size
            )));
        }
        let sha256 = match info.format.as_str() {
            "qcow2" => {
                // Re-probe with an explicit format: a qcow2 header that only
                // looks valid would fail here rather than at first boot.
                qemu_img::info(&part, Some(DiskFormat::Qcow2))?;
                streamed_sha256
            }
            "raw" => {
                let converted = crate::paths::file_in(
                    &self.settings.image_dir,
                    &format!("{}.qcow2.conv.part", super::safe_stem(id)),
                );
                let _ = std::fs::remove_file(&converted);
                qemu_img::convert(&part, DiskFormat::Raw, &converted, DiskFormat::Qcow2)?;
                std::fs::rename(&converted, &part).map_err(|e| ImageError::Io(e.to_string()))?;
                sha256_file(&part)?
            }
            other => {
                return Err(ImageError::InvalidImage(format!(
                    "unsupported image format {} (want qcow2 or raw)",
                    other
                )));
            }
        };
        let f = std::fs::File::open(&part).map_err(|e| ImageError::Io(e.to_string()))?;
        f.sync_all().map_err(|e| ImageError::Io(e.to_string()))?;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o444))
            .map_err(|e| ImageError::Io(e.to_string()))?;
        let dest = self.image_path(id);
        std::fs::rename(&part, &dest).map_err(|e| ImageError::Io(e.to_string()))?;
        let info = qemu_img::info(&dest, Some(DiskFormat::Qcow2))?;
        Ok((info, sha256))
    }
}

/// What installing a kernel image found.
pub(super) struct InstalledKernel {
    pub(super) size: u64,
    pub(super) sha256: String,
    pub(super) release: Option<String>,
    pub(super) initrd: Option<FileMeta>,
}

impl InstalledKernel {
    pub(super) fn apply(self, img: &mut Image) {
        img.virtual_size_bytes = self.size;
        img.file_size_bytes = self.size + self.initrd.as_ref().map_or(0, |i| i.size_bytes);
        img.sha256 = self.sha256;
        img.format = DiskFormat::Raw;
        img.kernel_release = self.release;
        img.initrd = self.initrd;
    }
}

/// A kernel file's size and header (K8), for an image of `arch`.
fn check_kernel_file(
    path: &Path,
    size: u64,
    arch: Arch,
    limit: u64,
) -> Result<kernel::KernelHeader, ImageError> {
    if size == 0 || size > limit {
        return Err(ImageError::InvalidImage(format!(
            "kernel is {size} bytes (limit {limit})"
        )));
    }
    let mut head = vec![0u8; kernel::HEADER_LEN.min(size as usize)];
    std::fs::File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
        .map_err(|e| ImageError::Io(format!("{}: {}", path.display(), e)))?;
    let header = kernel::inspect(&head)?;
    if header.arch != arch {
        return Err(ImageError::InvalidImage(format!(
            "the kernel is for {}, the image for {}",
            header.arch.as_str(),
            arch.as_str()
        )));
    }
    Ok(header)
}

/// A kernel image's default name from its file name: all of it (the
/// release is in it), cut to a valid image name.
fn kernel_name(file: &str) -> String {
    let name: String = file
        .trim_start_matches('.')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    if name.is_empty() {
        "kernel".to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_names_keep_the_release() {
        assert_eq!(
            kernel_name("vmlinuz-6.8.0-147-generic"),
            "vmlinuz-6.8.0-147-generic"
        );
        assert_eq!(kernel_name("..vmlinuz+x"), "vmlinuz-x");
        assert_eq!(kernel_name(""), "kernel");
        assert_eq!(kernel_name(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn private_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "0.0.0.0",
        ] {
            assert!(is_private(ip.parse().unwrap()), "{}", ip);
        }
        for ip in ["8.8.8.8", "91.189.91.1", "2606:4700::1111"] {
            assert!(!is_private(ip.parse().unwrap()), "{}", ip);
        }
    }

    #[test]
    fn url_rules() {
        let u = |s: &str| reqwest::Url::parse(s).unwrap();
        assert!(check_url(&u("https://8.8.8.8/x.img"), false).is_ok());
        assert!(check_url(&u("http://8.8.8.8/x.img"), false).is_err());
        assert!(check_url(&u("http://8.8.8.8/x.img"), true).is_err());
        assert!(check_url(&u("https://127.0.0.1/x.img"), false).is_err());
        assert!(check_url(&u("https://[::1]/x.img"), false).is_err());
        assert!(check_url(&u("http://127.0.0.1:8080/x.img"), true).is_ok());
        assert!(check_url(&u("file:///etc/passwd"), true).is_err());
        assert!(check_url(&u("ftp://8.8.8.8/x"), false).is_err());
    }

    #[test]
    fn hashing() {
        let mut h = Hashers::new(Some(HashAlgo::Sha512));
        h.update(b"abc");
        let (s256, s512) = h.finish(Some(HashAlgo::Sha512));
        assert_eq!(
            s256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(s512.unwrap().starts_with("ddaf35a193617aba"));
    }
}
