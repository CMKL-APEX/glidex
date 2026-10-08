//! Disk operations (spec §6): create, grow, shrink, extend root, delete.
//!
//! Everything here blocks (it shells out to qemu-img & co.) and assumes
//! the caller (`VmManager`) has already checked that no running or paused
//! VM has the disk open and holds the disk's `BusyGuard`.

use super::partition::{self, round_up, TableKind, MIB};
use super::qemu_img::{self, DiskFormat, GrowOutcome};
use super::{
    validate_name, CloneMode, CreateDiskRequest, Disk, DiskCreateSpec, DiskOrigin, DiskPhase, ExtendMode, ExtendOutcome,
    ImageError, ImageHold, ImageManager, ImageStatus,
};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `size_gib` / `size_bytes` → bytes rounded up to a whole MiB.
pub fn requested_size(size_gib: Option<u64>, size_bytes: Option<u64>) -> Result<Option<u64>, ImageError> {
    let bytes = match (size_gib, size_bytes) {
        (Some(_), Some(_)) => return Err(ImageError::invalid_disk("give size_gib or size_bytes, not both")),
        (Some(g), None) => Some(g.checked_mul(super::GIB).ok_or_else(|| ImageError::invalid_disk("size too large"))?),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    match bytes {
        Some(0) => Err(ImageError::invalid_disk("size must be greater than 0")),
        Some(b) => Ok(Some(round_up(b, MIB))),
        None => Ok(None),
    }
}

/// What `materialize` made. Keep `_hold` until the disk record is
/// persisted and cached: it keeps the source image from being deleted.
pub struct MaterializeOutcome {
    pub disk: Disk,
    pub warnings: Vec<String>,
    pub _hold: Option<ImageHold>,
}

/// A root disk's size: the request, else the image's own size rounded up
/// to a MiB, but at least the default root size.
pub fn wanted_root_size(requested: Option<u64>, image_size: u64, default_root: u64) -> u64 {
    requested.unwrap_or_else(|| round_up(image_size, MIB).max(default_root))
}

fn too_small(size: u64, image: &str, image_size: u64) -> ImageError {
    ImageError::InvalidDisk {
        message: format!(
            "size {} is smaller than image {} ({} bytes); that would cut off its partitions",
            size, image, image_size
        ),
        details: serde_json::json!({ "min_size_bytes": round_up(image_size, MIB) }),
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<(), ImageError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| ImageError::Io(format!("{}: {}", path.display(), e)))
}

/// Removes a temporary file unless disarmed.
struct TempFile(Option<PathBuf>);

impl TempFile {
    fn path(&self) -> &Path {
        self.0.as_deref().unwrap()
    }

    fn keep(mut self) -> PathBuf {
        self.0.take().unwrap()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl ImageManager {
    fn temp_path(&self, id: &str, what: &str, format: DiskFormat) -> TempFile {
        let p = self.settings.disk_dir.join(format!(".{}.{}.{}", id, what, format.extension()));
        let _ = std::fs::remove_file(&p);
        TempFile(Some(p))
    }

    /// Validate a create request (spec §6.1) and return the record, in
    /// phase `Pending` and not yet persisted: the disk controller makes its
    /// file once the source image is ready (spec/reconciliation.md §10.1).
    pub fn new_disk_record(&self, req: &CreateDiskRequest) -> Result<Disk, ImageError> {
        validate_name("disk", &req.name)?;
        if self.disk_name_taken(&req.name) {
            return Err(ImageError::AlreadyExists(format!("a disk named {} already exists", req.name)));
        }
        let format = req.format.unwrap_or_default();
        let size = requested_size(req.size_gib, req.size_bytes)?;
        let origin = match &req.image {
            None => {
                if req.clone.is_some() {
                    return Err(ImageError::invalid_disk("clone needs an image"));
                }
                if size.is_none() {
                    return Err(ImageError::invalid_disk("a blank disk needs size_gib or size_bytes"));
                }
                DiskOrigin::Blank
            }
            Some(key) => {
                let img = self.get_image(key)?;
                if img.is_firmware() {
                    return Err(ImageError::InvalidImage(format!("image {} is UEFI firmware, not a disk image", img.name)));
                }
                if img.deletion_requested_at.is_some() {
                    return Err(ImageError::NotReady(format!("image {} is being deleted", img.name)));
                }
                match &img.status {
                    ImageStatus::Missing => return Err(ImageError::NotReady(format!("image {} is missing its file", img.name))),
                    // A disk would wait for it forever: retry it first.
                    ImageStatus::Failed { reason } => {
                        return Err(ImageError::NotReady(format!("image {} failed ({}); retry it first", img.name, reason)))
                    }
                    _ => {}
                }
                let mode = req.clone.unwrap_or_default();
                if (mode, format) == (CloneMode::Linked, DiskFormat::Raw) {
                    return Err(ImageError::invalid_disk("a raw disk cannot be linked; use clone: full"));
                }
                if let (Some(s), ImageStatus::Ready) = (size, &img.status) {
                    if s < img.virtual_size_bytes {
                        return Err(too_small(s, &img.name, img.virtual_size_bytes));
                    }
                }
                DiskOrigin::Image { image_id: img.id.clone(), mode }
            }
        };
        let mut disk = Disk::new(
            uuid::Uuid::new_v4().to_string(),
            req.name.clone(),
            req.project.clone().unwrap_or_default(),
            format,
            0,
            origin,
        );
        disk.phase = DiskPhase::Pending;
        // One node until the scheduler binds disks at first use (C4).
        disk.node = Some(crate::authz::node_id());
        disk.create = Some(DiskCreateSpec { size_bytes: size, extend_root: req.extend_root });
        Ok(disk)
    }

    /// The size a disk cloned from an image of `image_size` bytes gets.
    pub fn root_size(&self, requested: Option<u64>, image_size: u64) -> u64 {
        wanted_root_size(requested, image_size, self.settings.default_root_bytes)
    }

    /// Make the file of a pending disk (spec §6.1): on a temporary file,
    /// renamed into place. Its own leftover temporary from a crash is
    /// replaced; a file already in place (a crash after the rename) is
    /// taken as made. Returns the disk in phase `Ready` (not persisted).
    /// `NotReady` while the source image is not ready yet.
    pub fn materialize(self: &Arc<Self>, disk: &Disk) -> Result<MaterializeOutcome, ImageError> {
        let spec = disk.create.clone().unwrap_or(DiskCreateSpec { size_bytes: None, extend_root: None });
        let mut disk = disk.clone();
        let final_path = self.disk_path(&disk);
        if final_path.exists() {
            let info = qemu_img::info(&final_path, Some(disk.format))?;
            disk.size_bytes = info.virtual_size;
            disk.phase = DiskPhase::Ready;
            disk.create = None;
            return Ok(MaterializeOutcome { disk, warnings: vec!["the file was already made".into()], _hold: None });
        }
        let tmp = self.temp_path(&disk.id, "create", disk.format);
        let mut warnings = Vec::new();
        let mut hold = None;
        let (size, image_size) = match disk.origin.clone() {
            DiskOrigin::Blank => {
                let size = spec.size_bytes.ok_or_else(|| ImageError::invalid_disk("a blank disk needs a size"))?;
                qemu_img::create(tmp.path(), disk.format, size, None)?;
                (size, None)
            }
            DiskOrigin::Image { image_id, mode } => {
                let img = self.get_image(&image_id)?;
                hold = Some(self.hold_image(&img.id));
                // Re-read under the hold: a delete that won the race has
                // removed the record by now.
                let img = self.get_image(&img.id)?;
                if img.status != ImageStatus::Ready {
                    return Err(ImageError::NotReady(format!("image {} is not ready ({:?})", img.name, img.status)));
                }
                let image_path = self.image_path(&img.id);
                // On a node of a cluster the catalog says Ready before this
                // node's copy has arrived (spec/clustering.md §9.2).
                if !image_path.exists() {
                    return Err(ImageError::NotReady(format!("image {} has not been copied to this node yet", img.name)));
                }
                let image_size = img.virtual_size_bytes;
                if let Some(s) = spec.size_bytes.filter(|s| *s < image_size) {
                    return Err(too_small(s, &img.name, image_size));
                }
                let size = self.root_size(spec.size_bytes, image_size);
                match mode {
                    CloneMode::Linked => qemu_img::create(tmp.path(), disk.format, size, Some(&image_path))?,
                    CloneMode::Full => {
                        qemu_img::convert(&image_path, DiskFormat::Qcow2, tmp.path(), disk.format)?;
                        if size > image_size {
                            qemu_img::resize(tmp.path(), disk.format, size, false)?;
                        }
                    }
                }
                (size, Some(image_size))
            }
        };
        set_mode(tmp.path(), 0o600)?;
        disk.size_bytes = size;
        let grew = image_size.is_some_and(|s| size > s);
        if grew && spec.extend_root != Some(false) {
            match self.extend_root_at(tmp.path(), &mut disk, ExtendMode::Offline) {
                Ok((_, mut w)) => warnings.append(&mut w),
                Err(ImageError::InvalidDisk { message, .. }) => {
                    warnings.push(format!("root partition not extended: {}", message));
                }
                Err(e) => return Err(e),
            }
        }
        std::fs::rename(tmp.keep(), &final_path).map_err(|e| ImageError::Io(e.to_string()))?;
        disk.phase = DiskPhase::Ready;
        disk.create = None;
        tracing::info!(disk = %disk.name, size = disk.size_bytes, origin = ?disk.origin, "disk created");
        Ok(MaterializeOutcome { disk, warnings, _hold: hold })
    }

    /// Remove a disk's file after its record is gone. Failures are logged:
    /// the file stays as an orphan (spec §6.5).
    pub fn remove_disk_file(&self, disk: &Disk) {
        let path = self.disk_path(disk);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(path = %path.display(), "could not remove disk file: {}", e),
        }
    }

    /// Minimum size a disk can be shrunk to.
    pub fn shrink_minimum(&self, disk: &Disk) -> Result<(u64, TableKind), ImageError> {
        let path = self.disk_path(disk);
        let table = partition::read_table(&path, disk.format, disk.size_bytes)?.ok_or_else(|| {
            ImageError::invalid_disk("disk has no partition table, so where its data ends is unknown; it cannot be shrunk")
        })?;
        let mut min = table.min_disk_size();
        if let DiskOrigin::Image { image_id, mode: CloneMode::Linked } = &disk.origin {
            // The overlay must cover every sector the backing file supplies.
            if let Ok(img) = self.get_image(image_id) {
                min = min.max(round_up(img.virtual_size_bytes, MIB));
            }
        }
        Ok((min, table.kind))
    }

    /// Grow or shrink (spec §6.2, §6.3). Persists the new size.
    pub fn resize_disk(
        &self,
        disk_id: &str,
        new_size: u64,
        extend_root: Option<bool>,
    ) -> Result<(Disk, Option<ExtendOutcome>, Vec<String>), ImageError> {
        let mut disk = self.get_disk(disk_id)?;
        let path = self.disk_path(&disk);
        let new_size = round_up(new_size, MIB);
        if new_size == disk.size_bytes {
            return Ok((disk, None, vec!["size unchanged".into()]));
        }
        if new_size > disk.size_bytes {
            self.grow(&mut disk, &path, new_size, extend_root)
        } else {
            self.shrink(&mut disk, &path, new_size).map(|d| (d, None, Vec::new()))
        }
    }

    fn grow(
        &self,
        disk: &mut Disk,
        path: &Path,
        new_size: u64,
        extend_root: Option<bool>,
    ) -> Result<(Disk, Option<ExtendOutcome>, Vec<String>), ImageError> {
        // In place: growing only adds space at the end.
        qemu_img::resize(path, disk.format, new_size, false)?;
        let old = disk.size_bytes;
        disk.size_bytes = new_size;
        self.put_disk(disk)?;
        tracing::info!(disk = %disk.name, old, new = new_size, "disk grown");

        let partial = |e: ImageError| {
            e.context(&format!("disk grown to {} bytes, but the partition table was not updated", new_size))
        };
        let table = partition::read_table(path, disk.format, new_size).map_err(partial)?;
        if let Some(t) = &table {
            if t.kind == TableKind::Gpt {
                match partition::edit(path, disk.format, new_size, qemu_img::sgdisk_relocate_backup) {
                    Ok(()) => {}
                    Err(e @ ImageError::ToolMissing { .. }) => {
                        if extend_root.unwrap_or(disk.is_from_image()) {
                            // On-boot growpart fixes the backup header too.
                            disk.pending_growpart = true;
                            self.put_disk(disk)?;
                            return Ok((
                                disk.clone(),
                                Some(ExtendOutcome::OnBoot),
                                vec![format!("{}; the guest will extend the root partition on next boot", e)],
                            ));
                        }
                        return Err(partial(e));
                    }
                    Err(e) => return Err(partial(e)),
                }
            }
        }
        if !extend_root.unwrap_or(disk.is_from_image()) {
            return Ok((disk.clone(), None, Vec::new()));
        }
        if table.is_none() {
            return Ok((disk.clone(), Some(ExtendOutcome::Skipped), vec!["no partition table; nothing to extend".into()]));
        }
        let (outcome, warnings) = self
            .extend_root_at(path, disk, ExtendMode::Offline)
            .map_err(|e| e.context(&format!("disk grown to {} bytes, but the root partition was not extended", new_size)))?;
        self.put_disk(disk)?;
        Ok((disk.clone(), Some(outcome), warnings))
    }

    fn shrink(&self, disk: &mut Disk, path: &Path, new_size: u64) -> Result<Disk, ImageError> {
        let (min, kind) = self.shrink_minimum(disk)?;
        if new_size < min {
            return Err(ImageError::InvalidDisk {
                message: format!(
                    "{} bytes would cut into a partition{}; the minimum is {} bytes. glidex does not shrink filesystems: shrink the filesystem and partition inside the guest first",
                    new_size,
                    if disk.is_linked() { " or the backing image" } else { "" },
                    min
                ),
                details: serde_json::json!({ "min_size_bytes": min }),
            });
        }
        if kind == TableKind::Gpt {
            qemu_img::SGDISK.require()?;
            if disk.format == DiskFormat::Qcow2 {
                qemu_img::QEMU_IO.require()?;
            }
        }
        // Work on a copy: `qemu-img resize --shrink` discards data without
        // asking, and a crash part way must leave the original intact.
        let tmp = self.temp_path(&disk.id, "shrink", disk.format);
        qemu_img::copy_sparse(path, tmp.path())?;
        qemu_img::resize(tmp.path(), disk.format, new_size, true)?;
        if kind == TableKind::Gpt {
            partition::edit(tmp.path(), disk.format, new_size, qemu_img::sgdisk_relocate_backup)?;
        }
        qemu_img::check(tmp.path(), disk.format)?;
        set_mode(tmp.path(), 0o600)?;
        let old = disk.size_bytes;
        std::fs::rename(tmp.keep(), path).map_err(|e| ImageError::Io(e.to_string()))?;
        disk.size_bytes = new_size;
        self.put_disk(disk)?;
        tracing::info!(disk = %disk.name, old, new = new_size, "disk shrunk");
        Ok(disk.clone())
    }

    /// `POST /disks/{id}/extend-root` (spec §6.4). Persists the disk.
    pub fn extend_root(&self, disk_id: &str, mode: ExtendMode) -> Result<(Disk, ExtendOutcome, Vec<String>), ImageError> {
        let mut disk = self.get_disk(disk_id)?;
        let path = self.disk_path(&disk);
        let (outcome, warnings) = self.extend_root_at(&path, &mut disk, mode)?;
        self.put_disk(&disk)?;
        Ok((disk, outcome, warnings))
    }

    /// Extend the root partition of the disk file at `path` (which may be
    /// a temporary file during create). Updates `disk.pending_growpart`
    /// but does not persist.
    fn extend_root_at(&self, path: &Path, disk: &mut Disk, mode: ExtendMode) -> Result<(ExtendOutcome, Vec<String>), ImageError> {
        let table = partition::read_table(path, disk.format, disk.size_bytes)?
            .ok_or_else(|| ImageError::invalid_disk("disk has no partition table"))?;
        let root = partition::growable_root(&table)?.clone();
        if mode == ExtendMode::OnBoot {
            disk.pending_growpart = true;
            return Ok((ExtendOutcome::OnBoot, Vec::new()));
        }
        let result = partition::edit(path, disk.format, disk.size_bytes, |raw| {
            if table.kind == TableKind::Gpt {
                qemu_img::sgdisk_relocate_backup(raw)?;
            }
            qemu_img::growpart(raw, root.number)
        });
        match result {
            Ok(GrowOutcome::Changed) => {
                tracing::info!(disk = %disk.name, partition = root.number, "root partition extended");
                Ok((ExtendOutcome::Grown, Vec::new()))
            }
            Ok(GrowOutcome::NoChange) => Ok((ExtendOutcome::AlreadyFull, Vec::new())),
            // Without the host tools, let the guest do it (spec §9).
            Err(e @ ImageError::ToolMissing { .. }) => {
                disk.pending_growpart = true;
                Ok((ExtendOutcome::OnBoot, vec![format!("{}; the guest will extend the root partition on next boot", e)]))
            }
            Err(e) => Err(e),
        }
    }
}
