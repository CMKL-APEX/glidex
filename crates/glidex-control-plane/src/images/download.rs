//! Image download pipeline (spec §5): stream to `<id>.qcow2.part` while
//! hashing, verify the digest and the image header, convert raw to qcow2,
//! then make the file read-only and rename it into place.

use super::catalog::{self, Arch, ChecksumSource, HashAlgo, VersionSource};
use super::qemu_img::{self, DiskFormat};
use super::{now, validate_name, Image, ImageError, ImageManager, ImageSource, ImageStatus, PullImageRequest};
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
        Self { sha256: Sha256::new(), sha512: (expected == Some(HashAlgo::Sha512)).then(Sha512::new) }
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

fn sha256_file(path: &Path) -> Result<String, ImageError> {
    let mut h = Hashers::new(None);
    hash_file(path, &mut h).map_err(|e| ImageError::Io(format!("{}: {}", path.display(), e)))?;
    Ok(h.finish(None).0)
}

impl ImageManager {
    /// Start a download (spec §5). Returns the record and whether it is
    /// new; an in-flight download of the same catalog key is returned as is.
    pub async fn pull_image(self: &Arc<Self>, req: PullImageRequest) -> Result<(Image, bool), ImageError> {
        let arch = Arch::host().ok_or_else(|| ImageError::InvalidImage("unsupported host architecture".into()))?;
        let source = match (&req.catalog, &req.url) {
            (Some(key), None) => {
                let entry = catalog::find(key)
                    .ok_or_else(|| ImageError::InvalidImage(format!("unknown catalog image: {}", key)))?;
                let img = entry.for_arch(arch).ok_or_else(|| {
                    ImageError::InvalidImage(format!("{} has no image for {:?}", key, arch))
                })?;
                if let Some(existing) = self.images.read().unwrap().values().find(|i| {
                    i.catalog_key() == Some(key.as_str())
                        && i.deletion_requested_at.is_none()
                        && matches!(i.status, ImageStatus::Downloading { .. } | ImageStatus::Verifying)
                }) {
                    return Ok((existing.clone(), false));
                }
                ImageSource::Catalog { key: key.clone(), url: img.url.to_string(), version: String::new() }
            }
            (None, Some(url)) => {
                let parsed = reqwest::Url::parse(url).map_err(|e| ImageError::InvalidImage(format!("bad URL: {}", e)))?;
                let (u, allow) = (parsed.clone(), self.settings.allow_private_urls);
                tokio::task::spawn_blocking(move || check_url(&u, allow))
                    .await
                    .map_err(|e| ImageError::Io(e.to_string()))?
                    .map_err(ImageError::InvalidImage)?;
                let sha = match &req.sha256 {
                    Some(s) => {
                        let s = s.trim().to_ascii_lowercase();
                        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
                            return Err(ImageError::InvalidImage("sha256 must be 64 hex characters".into()));
                        }
                        Some(s)
                    }
                    None => None,
                };
                ImageSource::Url { url: parsed.to_string(), expected_sha256: sha }
            }
            _ => return Err(ImageError::InvalidImage("give exactly one of catalog or url".into())),
        };

        let name = match &req.name {
            Some(n) => n.clone(),
            None => match &source {
                ImageSource::Catalog { key, .. } => key.clone(),
                ImageSource::Url { url, .. } => {
                    let file = catalog::url_file_name(url);
                    let stem = file.split('.').next().unwrap_or(file);
                    if stem.is_empty() { "image".to_string() } else { stem.to_string() }
                }
            },
        };
        validate_name("image", &name)?;
        if let Some(other) = self.images.read().unwrap().values().find(|i| i.name == name) {
            return Err(ImageError::AlreadyExists(if other.deletion_requested_at.is_some() {
                format!("an image named {} is being deleted; try again in a moment", name)
            } else {
                format!("an image named {} already exists (pass a different name, or delete it first)", name)
            }));
        }

        let img = Image {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            download: super::DownloadMeta {
                expected: match &source {
                    ImageSource::Url { expected_sha256: Some(s), .. } => Some((HashAlgo::Sha256, s.clone())),
                    _ => None,
                },
                url: match &source {
                    ImageSource::Url { url, .. } => url.clone(),
                    ImageSource::Catalog { .. } => String::new(),
                },
                ..Default::default()
            },
            source,
            status: ImageStatus::Downloading { received_bytes: 0, total_bytes: None },
            format: DiskFormat::Qcow2,
            virtual_size_bytes: 0,
            file_size_bytes: 0,
            sha256: String::new(),
            arch,
            created_at: now(),
            retry_seq: 0,
            applied_retry_seq: 0,
            deletion_requested_at: None,
        };
        self.insert_image(&img)?;
        tracing::info!(image = %img.name, source = ?img.source, "image download queued");
        self.spawn_download(img.id.clone());
        Ok((img, true))
    }

    /// Whether a download task runs for image `id`.
    pub fn download_running(&self, id: &str) -> bool {
        self.tasks.lock().unwrap().contains_key(id)
    }

    pub(crate) fn spawn_download(self: &Arc<Self>, id: String) {
        let mgr = self.clone();
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            let result = mgr.run_download(&task_id).await;
            mgr.tasks.lock().unwrap().remove(&task_id);
            if let Err(e) = result {
                tracing::warn!(image = %task_id, "image download failed: {}", e);
                let _ = std::fs::remove_file(mgr.part_path(&task_id));
                if let Ok(mut img) = mgr.get_image(&task_id) {
                    img.status = ImageStatus::Failed { reason: e.to_string() };
                    let _ = mgr.put_image(&img);
                }
            }
        });
        self.tasks.lock().unwrap().insert(id, handle.abort_handle());
    }

    async fn run_download(self: &Arc<Self>, id: &str) -> Result<(), ImageError> {
        let _permit = self.downloads.clone().acquire_owned().await.map_err(|e| ImageError::Io(e.to_string()))?;
        let mut img = self.get_image(id)?;

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
            match self.fetch(&mut img, &part, &mut hashers, &mut received).await {
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
        if let (Some((_, want)), Some(got)) = (&img.download.expected, &expected_digest) {
            if want != got {
                return Err(ImageError::InvalidImage(format!("checksum mismatch (expected {}, got {})", want, got)));
            }
        }

        let mgr = self.clone();
        let id_owned = id.to_string();
        let max = self.settings.max_image_size;
        let (info, sha256) = tokio::task::spawn_blocking(move || mgr.verify_and_install(&id_owned, sha256, max))
            .await
            .map_err(|e| ImageError::Io(e.to_string()))??;

        let mut img = self.get_image(id)?;
        img.virtual_size_bytes = info.virtual_size;
        img.file_size_bytes = info.actual_size;
        img.sha256 = sha256;
        img.format = DiskFormat::Qcow2;
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
        let validator = img.download.etag.clone().or_else(|| img.download.last_modified.clone());
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
            416 if resuming => return Err(ImageError::Download("server refused to resume (416)".into())),
            code if (500..600).contains(&code) => return Err(ImageError::Download(format!("HTTP {}", status))),
            _ => return Err(ImageError::InvalidImage(format!("HTTP {} from {}", status, img.download.url))),
        };
        let header = |name: reqwest::header::HeaderName| {
            resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
        };
        if restart {
            // Fresh body: forget anything already written.
            *received = 0;
            *hashers = Hashers::new(img.download.expected.as_ref().map(|(a, _)| *a));
            img.download.etag = header(reqwest::header::ETAG).filter(|e| !e.starts_with("W/"));
            img.download.last_modified = header(reqwest::header::LAST_MODIFIED);
            if let ImageSource::Catalog { version, .. } = &mut img.source {
                if version.is_empty() {
                    if let Some(lm) = &img.download.last_modified {
                        *version = lm.clone();
                    }
                }
            }
        }
        let total = resp.content_length().map(|n| n + *received);
        if let Some(t) = total {
            if t > self.settings.max_image_size.saturating_mul(2) {
                return Err(ImageError::InvalidImage(format!("download is {} bytes, over GLIDEX_MAX_IMAGE_SIZE", t)));
            }
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(!restart)
            .truncate(restart)
            .mode(0o600)
            .open(part)
            .map_err(|e| ImageError::Io(format!("{}: {}", part.display(), e)))?;
        img.status = ImageStatus::Downloading { received_bytes: *received, total_bytes: total };
        self.put_image(img)?;

        let mut last_persist = Instant::now();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(dl)?;
            file.write_all(&chunk).map_err(|e| ImageError::Io(format!("{}: {}", part.display(), e)))?;
            hashers.update(&chunk);
            *received += chunk.len() as u64;
            if *received > self.settings.max_image_size.saturating_mul(2) {
                return Err(ImageError::InvalidImage("download exceeds GLIDEX_MAX_IMAGE_SIZE".into()));
            }
            img.status = ImageStatus::Downloading { received_bytes: *received, total_bytes: total };
            if last_persist.elapsed() >= PERSIST_EVERY {
                file.flush().ok();
                self.put_image(img)?;
                last_persist = Instant::now();
            } else {
                self.cache_image(img);
            }
        }
        if let Some(t) = total {
            if *received < t {
                return Err(ImageError::Download(format!("connection closed after {} of {} bytes", received, t)));
            }
        }
        file.sync_all().map_err(|e| ImageError::Io(format!("{}: {}", part.display(), e)))?;
        Ok(())
    }

    /// Fill in `download.url`, the expected digest and the version for a
    /// catalog image.
    async fn resolve_catalog(&self, img: &mut Image) -> Result<(), ImageError> {
        let ImageSource::Catalog { key, .. } = &img.source else {
            return Ok(());
        };
        let entry = catalog::find(key).ok_or_else(|| ImageError::InvalidImage(format!("unknown catalog image: {}", key)))?;
        let arch_img = entry
            .for_arch(img.arch)
            .ok_or_else(|| ImageError::InvalidImage(format!("{} has no image for {:?}", key, img.arch)))?;
        let get_text = |url: &'static str| async move {
            let resp = self.http.get(url).send().await.map_err(|e| ImageError::Download(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(ImageError::Download(format!("HTTP {} from {}", resp.status(), url)));
            }
            resp.text().await.map_err(|e| ImageError::Download(e.to_string()))
        };

        let (url, digest, version) = match arch_img.checksum {
            ChecksumSource::File { url: sums_url, algo } => {
                let sums = catalog::parse_checksums(&get_text(sums_url).await?);
                let file = catalog::url_file_name(arch_img.url);
                let digest = sums
                    .iter()
                    .find(|(n, _)| n == file)
                    .map(|(_, d)| d.clone())
                    .ok_or_else(|| ImageError::Download(format!("{} is not listed in {}", file, sums_url)))?;
                let version = match arch_img.version {
                    VersionSource::UbuntuBuildInfo(u) => {
                        get_text(u).await.ok().and_then(|t| catalog::parse_build_info(&t)).unwrap_or_default()
                    }
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
                let (url, sha, name) = catalog::parse_fedora_releases(&json, img.arch)
                    .ok_or_else(|| ImageError::Download("no Fedora Cloud Base image in releases.json".into()))?;
                (url, (HashAlgo::Sha256, sha), name)
            }
        };
        if !url.starts_with("https://") {
            return Err(ImageError::Download(format!("catalog resolved to a non-https URL: {}", url)));
        }
        img.download.url = url.clone();
        img.download.expected = Some(digest);
        if let ImageSource::Catalog { url: u, version: v, .. } = &mut img.source {
            *u = url;
            *v = version;
        }
        Ok(())
    }

    /// Steps 4–6 of spec §5, blocking. Returns the final `qemu-img info`
    /// and the sha256 of the installed file.
    fn verify_and_install(&self, id: &str, streamed_sha256: String, max_size: u64) -> Result<(qemu_img::ImgInfo, String), ImageError> {
        let part = self.part_path(id);
        let info = qemu_img::info(&part, None)?;
        if info.backing_filename.is_some() {
            return Err(ImageError::InvalidImage("image has a backing file; refusing a downloaded image that points at host paths".into()));
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
                let converted = self.settings.image_dir.join(format!("{}.qcow2.conv.part", id));
                let _ = std::fs::remove_file(&converted);
                qemu_img::convert(&part, DiskFormat::Raw, &converted, DiskFormat::Qcow2)?;
                std::fs::rename(&converted, &part).map_err(|e| ImageError::Io(e.to_string()))?;
                sha256_file(&part)?
            }
            other => return Err(ImageError::InvalidImage(format!("unsupported image format {} (want qcow2 or raw)", other))),
        };
        let f = std::fs::File::open(&part).map_err(|e| ImageError::Io(e.to_string()))?;
        f.sync_all().map_err(|e| ImageError::Io(e.to_string()))?;
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o444)).map_err(|e| ImageError::Io(e.to_string()))?;
        let dest = self.image_path(id);
        std::fs::rename(&part, &dest).map_err(|e| ImageError::Io(e.to_string()))?;
        let info = qemu_img::info(&dest, Some(DiskFormat::Qcow2))?;
        Ok((info, sha256))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.1", "169.254.169.254", "100.64.0.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1", "0.0.0.0"] {
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
        assert_eq!(s256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(s512.unwrap().starts_with("ddaf35a193617aba"));
    }
}
