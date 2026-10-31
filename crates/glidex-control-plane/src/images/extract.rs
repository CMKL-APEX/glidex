//! Kernel images extracted from disk images (spec/23-kernel-boot.md §10),
//! the image manager's half: the extractor appliance, `kernel_from` pulls,
//! and turning the extractor's output into an installed kernel image. The
//! run itself is the VM controller's (`controller/extract.rs`): it needs a
//! VM instance, which only the runner can start.

use super::catalog::Arch;
use super::download::{InstalledKernel, sha256_hex, sha256_path};
use super::extractor::{self, ExtractorPin};
use super::{
    DiskFormat, Image, ImageError, ImageKind, ImageManager, ImageSource, ImageStatus, now,
    validate_name,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The extractor appliance's cache, in the image directory: one
/// subdirectory per pinned build, `kernel` and `initrd`.
pub const EXTRACTOR_DIR: &str = ".extractor";

/// A pinned package's size limit.
const MAX_DEB: usize = 256 << 20;

fn io(p: &Path) -> impl Fn(std::io::Error) -> ImageError + '_ {
    move |e| ImageError::Io(format!("{}: {}", p.display(), e))
}

impl ImageManager {
    /// Why kernels of `arch` can't be extracted here, if they can't: off
    /// on this host, no extractor pinned, or another architecture (the
    /// extractor runs under KVM, so only on a host of the image's).
    pub fn extraction_refused(&self, arch: Arch) -> Option<String> {
        if !self.settings.kernel_extraction {
            return Some(
                "kernel extraction is off on this host (GLIDEX_KERNEL_EXTRACTION=off)".into(),
            );
        }
        if extractor::pin_for(arch).is_none() {
            return Some(format!("no kernel extractor for {} yet", arch.as_str()));
        }
        match Arch::host() {
            Some(host) if host == arch => None,
            host => Some(format!(
                "{} kernels are extracted on a {} host; this one is {}",
                arch.as_str(),
                arch.as_str(),
                host.map(Arch::as_str).unwrap_or(std::env::consts::ARCH)
            )),
        }
    }

    /// The extractor's output disk for kernel image `id` (§10.2).
    pub fn extract_out_path(&self, id: &str) -> PathBuf {
        crate::paths::file_in(
            &self.settings.image_dir,
            &format!(".{}.extract.raw", super::safe_stem(id)),
        )
    }

    /// The extractor appliance for `arch`: `(kernel, initramfs)`, built
    /// from its pinned packages the first time and kept (§10.1).
    pub async fn ensure_extractor(
        self: &Arc<Self>,
        arch: Arch,
    ) -> Result<(PathBuf, PathBuf), ImageError> {
        if !self.settings.kernel_extraction {
            return Err(ImageError::InvalidImage(
                "kernel extraction is off on this host (GLIDEX_KERNEL_EXTRACTION=off)".into(),
            ));
        }
        let pin = extractor::pin_for(arch).ok_or_else(|| {
            ImageError::InvalidImage(format!("no kernel extractor for {} yet", arch.as_str()))
        })?;
        let _once = self.extractor_lock.lock().await;
        let dir = self
            .settings
            .image_dir
            .join(EXTRACTOR_DIR)
            .join(pin.version);
        let (kernel, initrd) = (dir.join("kernel"), dir.join("initrd"));
        let k = kernel.clone();
        let have = tokio::task::spawn_blocking(move || sha256_path(&k).ok())
            .await
            .ok()
            .flatten();
        if have.as_deref() == Some(pin.kernel.sha256) && initrd.is_file() {
            return Ok((kernel, initrd));
        }
        tracing::info!(version = pin.version, "downloading the kernel extractor");
        let kernel_deb = self.fetch_pinned(pin.kernel_deb).await?;
        let busybox_deb = self.fetch_pinned(pin.busybox_deb).await?;
        let (k, i) = (kernel.clone(), initrd.clone());
        tokio::task::spawn_blocking(move || {
            write_appliance(pin, &kernel_deb, &busybox_deb, &k, &i)
        })
        .await
        .map_err(|e| ImageError::Io(e.to_string()))??;
        Ok((kernel, initrd))
    }

    /// A pinned file, in memory, checked against its digest.
    async fn fetch_pinned(&self, (url, sha256): (&str, &str)) -> Result<Vec<u8>, ImageError> {
        let dl = |e: reqwest::Error| ImageError::Download(format!("{url}: {e}"));
        let resp = self.http.get(url).send().await.map_err(dl)?;
        if !resp.status().is_success() {
            return Err(ImageError::Download(format!(
                "HTTP {} from {url}",
                resp.status()
            )));
        }
        let mut data = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            data.extend_from_slice(&chunk.map_err(dl)?);
            if data.len() > MAX_DEB {
                return Err(ImageError::InvalidImage(format!("{url} is too large")));
            }
        }
        let got = sha256_hex(&data);
        if got != sha256 {
            return Err(ImageError::InvalidImage(format!(
                "{url}: checksum mismatch (expected {sha256}, got {got})"
            )));
        }
        Ok(data)
    }

    /// `POST /images {"kernel_from": …}`: a kernel image to extract from
    /// disk image `key` (§4.2). Returns it, and whether it is new: one
    /// already extracting from the same image is returned as is.
    pub(crate) fn pull_kernel_from(
        &self,
        key: &str,
        name: Option<String>,
        nodes: Vec<String>,
    ) -> Result<(Image, bool), ImageError> {
        let src = self.get_image(key)?;
        if src.kind != ImageKind::Disk {
            return Err(ImageError::InvalidImage(format!(
                "kernel_from needs a disk image; {} is not one",
                src.name
            )));
        }
        if src.deletion_requested_at.is_some()
            || matches!(
                src.status,
                ImageStatus::Failed { .. } | ImageStatus::Missing
            )
        {
            return Err(ImageError::NotReady(format!(
                "image {} is not usable ({:?})",
                src.name, src.status
            )));
        }
        if let Some(existing) = glidex_raftstore::wait::read(&self.images)
            .unwrap()
            .values()
            .find(|i| {
                i.for_image.as_deref() == Some(src.id.as_str())
                    && i.status == ImageStatus::Extracting
                    && i.deletion_requested_at.is_none()
            })
        {
            return Ok((existing.clone(), false));
        }
        let name = name.unwrap_or_else(|| {
            let mut n = format!("{}-kernel", src.name);
            n.truncate(64);
            n
        });
        validate_name("image", &name)?;
        if glidex_raftstore::wait::read(&self.images)
            .unwrap()
            .values()
            .any(|i| i.name == name)
        {
            return Err(ImageError::AlreadyExists(format!(
                "an image named {name} already exists (pass a different name, or delete it first)"
            )));
        }
        let mut img = extraction_record(name, src.arch, nodes);
        img.source = ImageSource::Extracted {
            image_id: src.id.clone(),
            disk_id: None,
            image_sha256: String::new(),
            path: String::new(),
            verified: false,
        };
        img.for_image = Some(src.id.clone());
        self.insert_image(&img)?;
        tracing::info!(image = %img.name, from = %src.name, "kernel extraction queued");
        Ok((img, true))
    }

    /// A kernel image to extract from VM `vm`'s root disk `disk`, owned by
    /// the VM (§10.5, K12), named after it; on `node` when not here (the
    /// disk's node, §10.2). The caller checks that it can be extracted.
    pub(crate) fn new_disk_kernel(
        &self,
        vm: &str,
        vm_name: &str,
        disk: &super::Disk,
        arch: Arch,
        node: Option<&str>,
    ) -> Result<Image, ImageError> {
        let stem: String = vm_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let mut img = extraction_record(String::new(), arch, Vec::new());
        img.name = format!("{}-kernel-{}", stem.trim_start_matches('.'), &img.id[..8]);
        if img.name.len() > 64 {
            img.name = format!("kernel-{}", img.id);
        }
        validate_name("image", &img.name)?;
        img.source = ImageSource::Extracted {
            image_id: String::new(),
            disk_id: Some(disk.id.clone()),
            image_sha256: String::new(),
            path: String::new(),
            verified: false,
        };
        img.owner = Some(vm.to_string());
        img.extract_node = node.map(str::to_string);
        self.insert_image(&img)?;
        tracing::info!(image = %img.name, disk = %disk.name, "kernel extraction from a VM's disk queued");
        Ok(img)
    }

    /// Drop kernel image `id`, record and files, as never made (a duplicate,
    /// or a failed extraction nobody asked for).
    pub(crate) fn discard_kernel(&self, id: &str) -> Result<(), ImageError> {
        if let Ok(img) = self.get_image(id) {
            self.remove_image_record(id)?;
            for (_, p) in self.image_files(&img) {
                let _ = std::fs::remove_file(p);
            }
        }
        for p in [
            self.part_path(id),
            self.initrd_part_path(id),
            self.extract_out_path(id),
        ] {
            let _ = std::fs::remove_file(p);
        }
        Ok(())
    }

    /// A ready kernel image with the same kernel and initramfs as `img`
    /// (K12: identical files are stored once).
    pub(crate) fn same_kernel(&self, img: &Image) -> Option<Image> {
        self.images
            .read()
            .unwrap()
            .values()
            .find(|i| {
                i.id != img.id
                    && i.is_kernel()
                    && i.usable()
                    && i.arch == img.arch
                    && i.sha256 == img.sha256
                    && i.initrd.as_ref().map(|f| &f.sha256)
                        == img.initrd.as_ref().map(|f| &f.sha256)
            })
            .cloned()
    }

    /// After the extractor ran (§10.3, §10.4): read its output disk `out`,
    /// choose the kernel, and install it and its initramfs as kernel image
    /// `id`. Returns the image with the files, release, command line and
    /// source filled in, made from `origin`. Blocking.
    pub(crate) fn finish_extraction(
        &self,
        id: &str,
        origin: &ExtractOrigin,
        out: &Path,
    ) -> Result<Image, ImageError> {
        let f = std::fs::File::open(out).map_err(io(out))?;
        let entries = extractor::parse_stream(
            BufReader::new(f),
            self.settings.max_kernel_size,
            self.settings.max_initrd_size,
        )?;
        let mut file = std::fs::File::open(out).map_err(io(out))?;
        let choice = extractor::choose(&entries, origin.arch, |e| {
            Ok(String::from_utf8_lossy(&extractor::read_entry(&mut file, e)?).into_owned())
        })?;
        let kernel_sha = copy_range(
            &mut file,
            choice.kernel.offset,
            choice.kernel.size,
            &self.part_path(id),
        )?;
        let initrd_sha = copy_range(
            &mut file,
            choice.initrd.offset,
            choice.initrd.size,
            &self.initrd_part_path(id),
        )?;
        let installed: InstalledKernel = self.install_kernel(
            id,
            origin.arch,
            kernel_sha,
            Some(super::FileMeta {
                sha256: initrd_sha,
                size_bytes: choice.initrd.size,
            }),
        )?;
        if let Some(r) = &installed.release
            && *r != choice.version
        {
            return Err(ImageError::InvalidImage(format!(
                "{} says it is release {r}",
                choice.kernel.label()
            )));
        }
        let mut img = self.get_image(id)?;
        let from_header = installed.release.is_some();
        installed.apply(&mut img);
        // An arm64 `Image` names no release: the file's name does (§10.4).
        if !from_header {
            img.kernel_release = Some(choice.version.clone());
        }
        img.cmdline = Some(choice.cmdline);
        img.source = ImageSource::Extracted {
            image_id: origin.image_id.clone(),
            disk_id: origin.disk_id.clone(),
            image_sha256: origin.sha256.clone(),
            path: choice.kernel.label(),
            verified: origin.verified,
        };
        Ok(img)
    }
}

/// A kernel extracted on a node other than the leader (§10.2): the
/// fields [`ImageManager::finish_extraction`] sets, which the node reports
/// in its cache row and the leader copies onto the image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedKernel {
    pub sha256: String,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel_release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initrd: Option<super::FileMeta>,
    pub cmdline: String,
    /// Where in the disk image it was (`vda13:/vmlinuz-…`).
    pub path: String,
    pub image_sha256: String,
    pub verified: bool,
}

impl ExtractedKernel {
    /// From the image [`ImageManager::finish_extraction`] made.
    pub fn of(img: &Image) -> Option<Self> {
        let ImageSource::Extracted {
            path,
            image_sha256,
            verified,
            ..
        } = &img.source
        else {
            return None;
        };
        Some(ExtractedKernel {
            sha256: img.sha256.clone(),
            size_bytes: img.virtual_size_bytes,
            kernel_release: img.kernel_release.clone(),
            initrd: img.initrd.clone(),
            cmdline: img.cmdline.clone().unwrap_or_default(),
            path: path.clone(),
            image_sha256: image_sha256.clone(),
            verified: *verified,
        })
    }

    /// Onto kernel image `img`, which then is the extracted kernel.
    pub fn apply(&self, img: &mut Image) {
        img.virtual_size_bytes = self.size_bytes;
        img.file_size_bytes = self.size_bytes + self.initrd.as_ref().map_or(0, |i| i.size_bytes);
        img.sha256 = self.sha256.clone();
        img.format = super::DiskFormat::Raw;
        img.kernel_release = self.kernel_release.clone();
        img.initrd = self.initrd.clone();
        img.cmdline = Some(self.cmdline.clone());
        if let ImageSource::Extracted {
            path,
            image_sha256,
            verified,
            ..
        } = &mut img.source
        {
            *path = self.path.clone();
            *image_sha256 = self.image_sha256.clone();
            *verified = self.verified;
        }
    }
}

/// What an extraction reads (§10.2): a disk image, or a VM's disk.
#[derive(Debug, Clone)]
pub struct ExtractOrigin {
    /// The file the extractor gets read-only, and its format.
    pub path: PathBuf,
    pub format: DiskFormat,
    /// A linked disk glidex made: its qcow2 backing file is the image's.
    pub backing_files: bool,
    pub arch: Arch,
    /// The disk image, or empty for a VM's disk (`disk_id`).
    pub image_id: String,
    pub disk_id: Option<String>,
    /// The image's digest (empty for a VM's disk, which the guest writes).
    pub sha256: String,
    /// As trusted as the image; a VM's disk never is.
    pub verified: bool,
}

/// A new kernel image record waiting for its extraction.
fn extraction_record(name: String, arch: Arch, nodes: Vec<String>) -> Image {
    Image {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        kind: ImageKind::Kernel,
        hypervisor: None,
        source: ImageSource::Extracted {
            image_id: String::new(),
            disk_id: None,
            image_sha256: String::new(),
            path: String::new(),
            verified: false,
        },
        status: ImageStatus::Extracting,
        format: DiskFormat::Raw,
        virtual_size_bytes: 0,
        file_size_bytes: 0,
        sha256: String::new(),
        arch,
        created_at: now(),
        download: Default::default(),
        retry_seq: 0,
        applied_retry_seq: 0,
        deletion_requested_at: None,
        pinned: nodes,
        release: Vec::new(),
        keep_server_copy: true,
        initrd: None,
        cmdline: None,
        kernel_release: None,
        for_image: None,
        owner: None,
        extract_node: None,
    }
}

/// Copy `size` bytes at `offset` of `from` into a new `0600` file `to`;
/// returns their sha256.
fn copy_range(
    from: &mut std::fs::File,
    offset: u64,
    size: u64,
    to: &Path,
) -> Result<String, ImageError> {
    use sha2::{Digest, Sha256};
    from.seek(SeekFrom::Start(offset)).map_err(io(to))?;
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(to)
        .map_err(io(to))?;
    let mut h = Sha256::new();
    let mut left = size;
    let mut buf = vec![0u8; 1 << 20];
    while left > 0 {
        let n = (left as usize).min(buf.len());
        from.read_exact(&mut buf[..n]).map_err(io(to))?;
        h.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(io(to))?;
        left -= n as u64;
    }
    out.sync_all().map_err(io(to))?;
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Assemble the appliance and write it into its cache directory: each
/// file through a temporary, then renamed, `0444`.
fn write_appliance(
    pin: &ExtractorPin,
    kernel_deb: &[u8],
    busybox_deb: &[u8],
    kernel: &Path,
    initrd: &Path,
) -> Result<(), ImageError> {
    let (k, i) = extractor::assemble(pin, kernel_deb, busybox_deb)?;
    let dir = kernel.parent().unwrap_or(Path::new("."));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(io(dir))?;
    // The initramfs first: a kernel with the pinned digest means both are in place.
    for (path, data) in [(initrd, i), (kernel, k)] {
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &data).map_err(io(&tmp))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444)).map_err(io(&tmp))?;
        std::fs::rename(&tmp, path).map_err(io(path))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_range_copies_and_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("s");
        std::fs::write(&src, b"0123456789").unwrap();
        let mut f = std::fs::File::open(&src).unwrap();
        let to = dir.path().join("t");
        let sha = copy_range(&mut f, 3, 4, &to).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"3456");
        assert_eq!(sha, sha256_hex(b"3456"));
        assert_eq!(
            std::fs::metadata(&to).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            copy_range(&mut f, 8, 4, &dir.path().join("u")).is_err(),
            "past the end"
        );
    }
}
