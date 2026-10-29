//! Disk operations (spec §6): create, grow, shrink, extend root, delete.
//! The volume itself is made and changed by the disk's datastore driver
//! (spec/24-storage.md §7); partitions are edited here, on what it hands back.
//!
//! Everything here blocks (it shells out to qemu-img & co.) and assumes
//! the caller (`VmManager`) has already checked that no running or paused
//! VM has the disk open and holds the disk's `BusyGuard`.

use super::partition::{self, MIB, TableKind, round_up};
use super::qemu_img::{self, DiskFormat, GrowOutcome};
use super::{
    CloneMode, CreateDiskRequest, Disk, DiskCreateSpec, DiskOrigin, DiskPhase, ExtendMode,
    ExtendOutcome, ImageError, ImageHold, ImageManager, ImageStatus, validate_name,
};
use crate::datastore::VolumeSource;
use std::path::Path;
use std::sync::Arc;

/// `size_gib` / `size_bytes` → bytes rounded up to a whole MiB.
pub fn requested_size(
    size_gib: Option<u64>,
    size_bytes: Option<u64>,
) -> Result<Option<u64>, ImageError> {
    let bytes = match (size_gib, size_bytes) {
        (Some(_), Some(_)) => {
            return Err(ImageError::invalid_disk(
                "give size_gib or size_bytes, not both",
            ));
        }
        (Some(g), None) => Some(
            g.checked_mul(super::GIB)
                .ok_or_else(|| ImageError::invalid_disk("size too large"))?,
        ),
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

impl ImageManager {
    /// Validate a create request (spec §6.1) and return the record, in
    /// phase `Pending` and not yet persisted: the disk controller makes its
    /// file once the source image is ready (spec/12-reconciliation.md §10.1).
    /// `in_tenant` accepts the projects of the new disk's tenant, among
    /// which its name must be unique (spec/19-tenancy.md D24).
    pub fn new_disk_record(
        &self,
        req: &CreateDiskRequest,
        in_tenant: impl Fn(&str) -> bool,
    ) -> Result<Disk, ImageError> {
        validate_name("disk", &req.name)?;
        if self.disk_name_taken_in(&req.name, in_tenant) {
            return Err(ImageError::AlreadyExists(format!(
                "a disk named {} already exists",
                req.name
            )));
        }
        // A block datastore's volumes are raw (spec/24-storage.md S19).
        let block = self.datastore_is_block(req.datastore.as_deref());
        let format = match (req.format, block) {
            (Some(DiskFormat::Qcow2), true) => {
                return Err(ImageError::invalid_disk(
                    "disks on an lvm datastore are raw volumes; leave format out",
                ));
            }
            (_, true) => DiskFormat::Raw,
            (f, false) => f.unwrap_or_default(),
        };
        let size = requested_size(req.size_gib, req.size_bytes)?;
        let origin = match &req.image {
            None => {
                if req.clone.is_some() {
                    return Err(ImageError::invalid_disk("clone needs an image"));
                }
                if size.is_none() {
                    return Err(ImageError::invalid_disk(
                        "a blank disk needs size_gib or size_bytes",
                    ));
                }
                DiskOrigin::Blank
            }
            Some(key) => {
                let img = self.get_image(key)?;
                match img.kind {
                    super::ImageKind::Disk => {}
                    super::ImageKind::Firmware => {
                        return Err(ImageError::InvalidImage(format!(
                            "image {} is UEFI firmware, not a disk image",
                            img.name
                        )));
                    }
                    super::ImageKind::Kernel => {
                        return Err(ImageError::InvalidImage(format!(
                            "image {} is a kernel, not a disk image",
                            img.name
                        )));
                    }
                }
                if img.deletion_requested_at.is_some() {
                    return Err(ImageError::NotReady(format!(
                        "image {} is being deleted",
                        img.name
                    )));
                }
                match &img.status {
                    ImageStatus::Missing => {
                        return Err(ImageError::NotReady(format!(
                            "image {} is missing its file",
                            img.name
                        )));
                    }
                    // A disk would wait for it forever: retry it first.
                    ImageStatus::Failed { reason } => {
                        return Err(ImageError::NotReady(format!(
                            "image {} failed ({}); retry it first",
                            img.name, reason
                        )));
                    }
                    _ => {}
                }
                let mode = req.clone.unwrap_or_default();
                // On LVM a linked disk is a thin snapshot, raw as well.
                if (mode, format) == (CloneMode::Linked, DiskFormat::Raw) && !block {
                    return Err(ImageError::invalid_disk(
                        "a raw disk cannot be linked; use clone: full",
                    ));
                }
                if let (Some(s), ImageStatus::Ready) = (size, &img.status)
                    && s < img.virtual_size_bytes
                {
                    return Err(too_small(s, &img.name, img.virtual_size_bytes));
                }
                DiskOrigin::Image {
                    image_id: img.id.clone(),
                    mode,
                }
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
        // Bound now when asked, else to the first VM's node (§9.2).
        disk.node = req.node.clone();
        disk.create = Some(DiskCreateSpec {
            size_bytes: size,
            extend_root: req.extend_root,
        });
        self.choose_datastore(req.datastore.as_deref(), &mut disk)?;
        Ok(disk)
    }

    /// Whether the datastore `key` names (else the default for disks) keeps
    /// block volumes. One that doesn't exist is not: `choose_datastore`
    /// says so.
    fn datastore_is_block(&self, key: Option<&str>) -> bool {
        use crate::datastore::record;
        match key {
            Some(k) => record::find(&self.db, k).ok().flatten(),
            None => record::default_for_disks(&self.db).ok(),
        }
        .is_some_and(|d| d.driver.is_block())
    }

    /// Put a new disk on the datastore asked for, else the default for disks
    /// (spec/24-storage.md S13), checking that it can go there: the node it is
    /// bound to is one of the datastore's, and a datastore that doesn't
    /// overcommit has room on some node it can go to (§5.2).
    fn choose_datastore(&self, key: Option<&str>, disk: &mut Disk) -> Result<(), ImageError> {
        use crate::datastore::record;
        let io = |e: record::DatastoreError| ImageError::Io(e.to_string());
        let ds = match key {
            Some(k) => record::find(&self.db, k)
                .map_err(io)?
                .ok_or_else(|| ImageError::NotFound(format!("no datastore {k}")))?,
            None => record::default_for_disks(&self.db).map_err(io)?,
        };
        if let Some(n) = &disk.node
            && !ds.on_node(n)
        {
            return Err(ImageError::invalid_disk(format!(
                "datastore {} is not on node {n}",
                ds.name
            )));
        }
        disk.datastore = ds.id.clone();
        if ds.is_shared() {
            // Bound to no node: its VM may run on any node that reaches it.
            if disk.node.is_some() {
                return Err(ImageError::invalid_disk(format!(
                    "datastore {} is shared: a disk on it is bound to no node",
                    ds.name
                )));
            }
            // Made by this node if it reaches the datastore, else by one that
            // does; the leader moves it later as needed (S6).
            let rows = record::rows(&self.db, &ds.id).map_err(io)?;
            let me = self.node_id.load();
            let mut ok: Vec<&String> = rows
                .iter()
                .filter(|(n, r)| r.reachable && ds.on_node(n))
                .map(|(n, _)| n)
                .collect();
            ok.sort();
            disk.worker = if ok.iter().any(|n| **n == **me) {
                Some(me.to_string())
            } else {
                ok.first().map(|n| n.to_string())
            };
        }
        if ds.overcommit {
            return Ok(());
        }
        let need = self.estimated_size(disk);
        let pending = |node: &str| -> u64 {
            self.list_disks()
                .iter()
                .filter(|d| {
                    d.datastore == ds.id
                        && matches!(d.phase, DiskPhase::Pending | DiskPhase::Creating)
                        && d.node.as_deref().is_none_or(|n| n == node)
                })
                .map(|d| self.estimated_size(d))
                .sum()
        };
        let best = record::rows(&self.db, &ds.id)
            .map_err(io)?
            .into_iter()
            .filter(|(n, r)| {
                r.reachable && ds.on_node(n) && disk.node.as_deref().is_none_or(|b| b == n)
            })
            .map(|(n, r)| r.free_bytes.saturating_sub(pending(&n)))
            .max()
            .unwrap_or(0);
        if need > best {
            return Err(ImageError::InsufficientStorage {
                message: format!(
                    "datastore {} has {} bytes free where this disk could go, and the disk needs {}",
                    ds.name, best, need
                ),
                details: serde_json::json!({ "free_bytes": best, "needed_bytes": need }),
            });
        }
        Ok(())
    }

    /// What a disk will take: its size once made, else what it asked for,
    /// else its image's size or the default root size.
    pub fn estimated_size(&self, d: &Disk) -> u64 {
        let requested = d.create.as_ref().and_then(|c| c.size_bytes);
        match &d.origin {
            DiskOrigin::Blank => requested.unwrap_or(d.size_bytes),
            DiskOrigin::Image { image_id, .. } => {
                let image_size = self
                    .get_image(image_id)
                    .map(|i| i.virtual_size_bytes)
                    .unwrap_or(0);
                self.root_size(requested, image_size)
            }
        }
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
        let spec = disk.create.clone().unwrap_or(DiskCreateSpec {
            size_bytes: None,
            extend_root: None,
        });
        let mut disk = disk.clone();
        let driver = self.driver(&disk)?;
        // Held while it is made: no other node touches it (S16).
        let _lock = driver.lock(&disk)?;
        if driver.exists(&disk) {
            let made = driver.binding(&disk);
            disk.size_bytes = match driver.volume_size(&disk) {
                Some(s) => s,
                None => qemu_img::info(&made.path(), Some(disk.format))?.virtual_size,
            };
            disk.phase = DiskPhase::Ready;
            disk.create = None;
            return Ok(MaterializeOutcome {
                disk,
                warnings: vec!["the file was already made".into()],
                _hold: None,
            });
        }
        let mut hold = None;
        let image_path;
        let (size, image_size, source) = match disk.origin.clone() {
            DiskOrigin::Blank => {
                let size = spec
                    .size_bytes
                    .ok_or_else(|| ImageError::invalid_disk("a blank disk needs a size"))?;
                (size, None, VolumeSource::Blank)
            }
            DiskOrigin::Image { image_id, mode } => {
                let img = self.get_image(&image_id)?;
                hold = Some(self.hold_image(&img.id));
                // Re-read under the hold: a delete that won the race has
                // removed the record by now.
                let img = self.get_image(&img.id)?;
                if img.status != ImageStatus::Ready {
                    return Err(ImageError::NotReady(format!(
                        "image {} is not ready ({:?})",
                        img.name, img.status
                    )));
                }
                // A linked disk on a shared datastore overlays the
                // datastore's own copy, which every node there sees
                // (spec/24-storage.md S8); anything else this node's.
                let store = (mode == CloneMode::Linked)
                    .then(|| crate::datastore::record::find(&self.db, &disk.datastore))
                    .and_then(|r| r.ok().flatten())
                    .filter(|ds| ds.holds_image_files());
                image_path = match &store {
                    Some(ds) => ds.image_copy_path(&self.node_id.load(), &img.id),
                    None => self.image_path(&img.id),
                };
                // On a node of a cluster the catalog says Ready before this
                // node's copy has arrived (spec/14-clustering.md §9.2).
                if !image_path.exists() {
                    return Err(ImageError::NotReady(match &store {
                        Some(ds) => format!(
                            "image {} has not been copied into datastore {} yet",
                            img.name, ds.name
                        ),
                        None => format!("image {} has not been copied to this node yet", img.name),
                    }));
                }
                let image_size = img.virtual_size_bytes;
                if let Some(s) = spec.size_bytes.filter(|s| *s < image_size) {
                    return Err(too_small(s, &img.name, image_size));
                }
                let size = self.root_size(spec.size_bytes, image_size);
                let source = VolumeSource::Image {
                    path: &image_path,
                    size_bytes: image_size,
                    linked: mode == CloneMode::Linked,
                };
                (size, Some(image_size), source)
            }
        };
        disk.size_bytes = size;
        let extend = image_size.is_some_and(|s| size > s) && spec.extend_root != Some(false);
        let mut warnings = Vec::new();
        driver.create_volume(&disk.clone(), size, source, &mut |made| {
            if extend {
                match self.extend_root_at(&made.path(), &mut disk, ExtendMode::Offline) {
                    Ok((_, mut w)) => warnings.append(&mut w),
                    Err(ImageError::InvalidDisk { message, .. }) => {
                        warnings.push(format!("root partition not extended: {}", message));
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })?;
        disk.phase = DiskPhase::Ready;
        disk.create = None;
        tracing::info!(disk = %disk.name, size = disk.size_bytes, origin = ?disk.origin, "disk created");
        Ok(MaterializeOutcome {
            disk,
            warnings,
            _hold: hold,
        })
    }

    /// Remove a disk's file after its record is gone. Failures are logged:
    /// the file stays as an orphan (spec §6.5).
    pub fn remove_disk_file(&self, disk: &Disk) {
        let gone = self.driver(disk).and_then(|d| {
            let _lock = d.lock(disk)?;
            d.delete_volume(disk)
        });
        if let Err(e) = gone {
            tracing::warn!(disk = %disk.name, "could not remove disk file: {}", e);
        }
    }

    /// Minimum size a disk can be shrunk to.
    pub fn shrink_minimum(&self, disk: &Disk) -> Result<(u64, TableKind), ImageError> {
        let path = self.driver(disk)?.tool_binding(disk)?.path().to_path_buf();
        let table = partition::read_table(&path, disk.format, disk.size_bytes)?.ok_or_else(|| {
            ImageError::invalid_disk("disk has no partition table, so where its data ends is unknown; it cannot be shrunk")
        })?;
        let mut min = table.min_disk_size();
        if let DiskOrigin::Image {
            image_id,
            mode: CloneMode::Linked,
        } = &disk.origin
        {
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
        let driver = self.driver(&disk)?;
        let _lock = driver.lock(&disk)?;
        let path = driver.tool_binding(&disk)?.path().to_path_buf();
        let new_size = round_up(new_size, MIB);
        if new_size == disk.size_bytes {
            return Ok((disk, None, vec!["size unchanged".into()]));
        }
        if new_size > disk.size_bytes {
            self.grow(&mut disk, &path, new_size, extend_root)
        } else {
            self.shrink(&mut disk, new_size)
                .map(|d| (d, None, Vec::new()))
        }
    }

    fn grow(
        &self,
        disk: &mut Disk,
        path: &Path,
        new_size: u64,
        extend_root: Option<bool>,
    ) -> Result<(Disk, Option<ExtendOutcome>, Vec<String>), ImageError> {
        self.driver(disk)?.expand_volume(disk, new_size)?;
        let old = disk.size_bytes;
        disk.size_bytes = new_size;
        self.put_disk(disk)?;
        tracing::info!(disk = %disk.name, old, new = new_size, "disk grown");

        let partial = |e: ImageError| {
            e.context(&format!(
                "disk grown to {} bytes, but the partition table was not updated",
                new_size
            ))
        };
        let table = partition::read_table(path, disk.format, new_size).map_err(partial)?;
        if let Some(t) = &table
            && t.kind == TableKind::Gpt
        {
            match partition::edit(
                path,
                disk.format,
                new_size,
                qemu_img::sgdisk_relocate_backup,
            ) {
                Ok(()) => {}
                Err(e @ ImageError::ToolMissing { .. }) => {
                    if extend_root.unwrap_or(disk.is_from_image()) {
                        // On-boot growpart fixes the backup header too.
                        disk.pending_growpart = true;
                        self.put_disk(disk)?;
                        return Ok((
                            disk.clone(),
                            Some(ExtendOutcome::OnBoot),
                            vec![format!(
                                "{}; the guest will extend the root partition on next boot",
                                e
                            )],
                        ));
                    }
                    return Err(partial(e));
                }
                Err(e) => return Err(partial(e)),
            }
        }
        if !extend_root.unwrap_or(disk.is_from_image()) {
            return Ok((disk.clone(), None, Vec::new()));
        }
        if table.is_none() {
            return Ok((
                disk.clone(),
                Some(ExtendOutcome::Skipped),
                vec!["no partition table; nothing to extend".into()],
            ));
        }
        let (outcome, warnings) = self
            .extend_root_at(path, disk, ExtendMode::Offline)
            .map_err(|e| {
                e.context(&format!(
                    "disk grown to {} bytes, but the root partition was not extended",
                    new_size
                ))
            })?;
        self.put_disk(disk)?;
        Ok((disk.clone(), Some(outcome), warnings))
    }

    fn shrink(&self, disk: &mut Disk, new_size: u64) -> Result<Disk, ImageError> {
        let (min, kind) = self.shrink_minimum(disk)?;
        if new_size < min {
            return Err(ImageError::InvalidDisk {
                message: format!(
                    "{} bytes would cut into a partition{}; the minimum is {} bytes. glidex does not shrink filesystems: shrink the filesystem and partition inside the guest first",
                    new_size,
                    if disk.is_linked() {
                        " or the backing image"
                    } else {
                        ""
                    },
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
        self.driver(disk)?
            .shrink_volume(disk, new_size, &mut |copy| {
                if kind == TableKind::Gpt {
                    partition::edit(
                        &copy.path(),
                        disk.format,
                        new_size,
                        qemu_img::sgdisk_relocate_backup,
                    )?;
                }
                Ok(())
            })?;
        let old = disk.size_bytes;
        disk.size_bytes = new_size;
        self.put_disk(disk)?;
        tracing::info!(disk = %disk.name, old, new = new_size, "disk shrunk");
        Ok(disk.clone())
    }

    /// `POST /disks/{id}/extend-root` (spec §6.4). Persists the disk.
    pub fn extend_root(
        &self,
        disk_id: &str,
        mode: ExtendMode,
    ) -> Result<(Disk, ExtendOutcome, Vec<String>), ImageError> {
        let mut disk = self.get_disk(disk_id)?;
        let driver = self.driver(&disk)?;
        let _lock = driver.lock(&disk)?;
        let path = driver.tool_binding(&disk)?.path().to_path_buf();
        let (outcome, warnings) = self.extend_root_at(&path, &mut disk, mode)?;
        self.put_disk(&disk)?;
        Ok((disk, outcome, warnings))
    }

    /// Extend the root partition of the disk file at `path` (which may be
    /// a temporary file during create). Updates `disk.pending_growpart`
    /// but does not persist.
    fn extend_root_at(
        &self,
        path: &Path,
        disk: &mut Disk,
        mode: ExtendMode,
    ) -> Result<(ExtendOutcome, Vec<String>), ImageError> {
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
                Ok((
                    ExtendOutcome::OnBoot,
                    vec![format!(
                        "{}; the guest will extend the root partition on next boot",
                        e
                    )],
                ))
            }
            Err(e) => Err(e),
        }
    }
}

impl ImageManager {
    /// The record a disk has once moved as `spec` says (spec/24-storage.md
    /// §8.3): on its new datastore, bound or worked by the mover, standalone
    /// when flattened.
    pub fn moved_record(&self, disk: &Disk, spec: &super::MoveSpec) -> Disk {
        let mut d = disk.clone();
        d.datastore = spec.datastore.clone();
        d.node = spec.node.clone();
        d.worker = spec.node.is_none().then(|| spec.mover.clone());
        d.moving = None;
        if spec.flatten
            && let DiskOrigin::Image { mode, .. } = &mut d.origin
        {
            *mode = CloneMode::Full;
        }
        if spec.raw {
            d.format = DiskFormat::Raw;
        }
        d
    }

    /// Copy `src`'s volume to where `dst` (its moved record) says, holding
    /// both volumes' locks: a linked disk rebased onto the destination's
    /// copy of its image, or flattened. A copy already in place (a crash
    /// after the rename) is taken as made. The old volume stays: the caller
    /// removes it once the move is recorded.
    pub fn move_volume(&self, src: &Disk, dst: &Disk, flatten: bool) -> Result<(), ImageError> {
        let (from, to) = (self.driver(src)?, self.driver(dst)?);
        let _src_lock = from.lock(src)?;
        let _dst_lock = to.lock(dst)?;
        let source = from.tool_binding(src)?;
        if matches!(source, crate::models::VolumeBinding::VhostUserBlk { .. }) {
            return Err(ImageError::invalid_disk(format!(
                "disk {} can't be moved off a hyperstor datastore yet",
                src.name
            )));
        }
        let path = source.path().to_path_buf();
        if to.exists(dst) {
            return Ok(());
        }
        let backing = match &src.origin {
            DiskOrigin::Image {
                image_id,
                mode: CloneMode::Linked,
            } if !flatten => Some(self.image_for_move(dst, image_id)?),
            _ => None,
        };
        to.copy_in(dst, &path, src.format, flatten, &mut |made| {
            if let Some(b) = &backing {
                let made_path = made.path();
                let dir = made_path.parent().unwrap_or(Path::new("."));
                let rel = crate::paths::relative_to(b, dir).unwrap_or_else(|| b.clone());
                qemu_img::rebase_unsafe(&made.path(), &rel)?;
            }
            Ok(())
        })?;
        tracing::info!(disk = %src.name, from = %src.datastore, to = %dst.datastore, "disk copied to its new datastore");
        Ok(())
    }

    /// The image file a linked disk moved to `dst`'s datastore overlays:
    /// a shared datastore's own copy, else this node's (S8).
    fn image_for_move(&self, dst: &Disk, image_id: &str) -> Result<std::path::PathBuf, ImageError> {
        let img = self.get_image(image_id)?;
        let ds = crate::datastore::record::find(&self.db, &dst.datastore)
            .map_err(|e| ImageError::Io(e.to_string()))?
            .filter(|ds| ds.holds_image_files());
        let path = match &ds {
            Some(ds) => ds.image_copy_path(&self.node_id.load(), image_id),
            None => self.image_path(image_id),
        };
        if !path.exists() {
            return Err(ImageError::NotReady(match ds {
                Some(ds) => format!(
                    "image {} has not been copied into datastore {} yet",
                    img.name, ds.name
                ),
                None => format!("image {} has not been copied to this node yet", img.name),
            }));
        }
        Ok(path)
    }
}
