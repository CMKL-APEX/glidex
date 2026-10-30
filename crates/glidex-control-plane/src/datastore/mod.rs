//! Datastores (spec/24-storage.md): where disks live, behind a driver.
//!
//! The built-in `local` datastore is each node's disk directory, through
//! [`file::FileDriver`]; disk records that name no datastore are on it. It
//! is never stored ([`record`]). Registered `dir` datastores (S2) are a
//! directory on each node, through the same driver.
//!
//! Like the disk operations that call it, everything here blocks.

pub mod file;
pub mod hyperstor;
pub mod lvm;
pub mod manage;
pub mod probe;
pub mod record;

use crate::images::qemu_img::DiskFormat;
use crate::images::{Disk, ImageError};
use std::path::Path;

/// The built-in datastore: each node's disk directory (spec/24-storage.md S3).
pub const LOCAL: &str = "local";

/// What a hypervisor, or a disk tool, needs to open a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeBinding {
    /// A file in a file datastore.
    File {
        path: String,
        format: DiskFormat,
        /// qcow2 overlay on an image: the backend must allow backing
        /// files. Only ever set for disks glidex created.
        backing_files: bool,
        /// On a shared datastore: the file whose write lock says who has the
        /// volume open (spec/24-storage.md S16).
        lock: Option<String>,
    },
    /// A raw block device, `/dev/<vg>/<lv>` (spec/24-storage.md §11.1).
    Block { path: String },
    /// A hyperstor volume (spec/24-storage.md §11.2): served to the VM over
    /// vhost-user-blk at `socket` once staged; to the partition code it is
    /// the path `hyperstor://<map>/<volume>`, read and written through
    /// `hyperstorctl` (`socket` is empty there).
    VhostUserBlk {
        socket: String,
        volume: String,
        map: String,
    },
}

impl VolumeBinding {
    /// Where a disk tool opens it: the file or device, or a hyperstor
    /// volume's `hyperstor://` path.
    pub fn path(&self) -> std::borrow::Cow<'_, Path> {
        match self {
            VolumeBinding::File { path, .. } | VolumeBinding::Block { path } => {
                std::borrow::Cow::Borrowed(Path::new(path))
            }
            VolumeBinding::VhostUserBlk { volume, map, .. } => {
                std::borrow::Cow::Owned(std::path::PathBuf::from(hyperstor::tool_path(map, volume)))
            }
        }
    }

    /// A vhost-user-blk socket the hypervisor connects to, once staged.
    pub fn vhost_socket(&self) -> Option<&str> {
        match self {
            VolumeBinding::VhostUserBlk { socket, .. } if !socket.is_empty() => Some(socket),
            _ => None,
        }
    }

    pub fn format(&self) -> DiskFormat {
        match self {
            VolumeBinding::File { format, .. } => *format,
            VolumeBinding::Block { .. } | VolumeBinding::VhostUserBlk { .. } => DiskFormat::Raw,
        }
    }

    pub fn lock(&self) -> Option<&str> {
        match self {
            VolumeBinding::File { lock, .. } => lock.as_deref(),
            // On one node the hypervisors' own locks suffice (S19); a
            // hyperstor volume's lease keeps every other node out (S23).
            VolumeBinding::Block { .. } | VolumeBinding::VhostUserBlk { .. } => None,
        }
    }

    pub fn is_block(&self) -> bool {
        matches!(self, VolumeBinding::Block { .. })
    }
}

/// What a driver can do (spec/24-storage.md §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// Disks can be qcow2 overlays on an image in the same datastore.
    pub linked_clones: bool,
    /// Volumes can be made smaller.
    pub shrink: bool,
}

/// What a new volume starts from.
#[derive(Debug, Clone, Copy)]
pub enum VolumeSource<'a> {
    Blank,
    /// An image file of `size_bytes` (virtual), copied, or linked when
    /// `linked`.
    Image {
        path: &'a Path,
        size_bytes: u64,
        linked: bool,
    },
}

/// Called on a volume before it becomes the disk's: a failure leaves the
/// disk as it was.
pub type Prepare<'a> = &'a mut dyn FnMut(&VolumeBinding) -> Result<(), ImageError>;

/// A datastore's driver. Its methods follow CSI's (spec/24-storage.md §7, §12)
/// so that a CSI adapter can be one more driver.
pub trait StorageDriver: Send + Sync + std::fmt::Debug {
    fn capabilities(&self) -> Caps;

    /// How `disk`'s volume is opened, whether or not it exists yet.
    fn binding(&self, disk: &Disk) -> VolumeBinding;

    fn exists(&self, disk: &Disk) -> bool;

    /// Make `disk`'s volume of `size` bytes from `source`, run `prepare`
    /// on it, and only then make it the disk's. Nothing is left behind if
    /// either fails.
    fn create_volume(
        &self,
        disk: &Disk,
        size: u64,
        source: VolumeSource<'_>,
        prepare: Prepare<'_>,
    ) -> Result<(), ImageError>;

    /// Make `disk`'s volume a copy of `from` (another datastore's volume, of
    /// `from_format`), standalone with `flatten`; `prepare` runs on the copy, then
    /// it becomes the disk's (spec/24-storage.md §8.3). Nothing is left
    /// behind if either fails.
    fn copy_in(
        &self,
        disk: &Disk,
        from: &Path,
        from_format: DiskFormat,
        flatten: bool,
        prepare: Prepare<'_>,
    ) -> Result<(), ImageError>;

    /// Remove `disk`'s volume; one already gone is not an error.
    fn delete_volume(&self, disk: &Disk) -> Result<(), ImageError>;

    /// Grow `disk`'s volume in place to `size` bytes.
    fn expand_volume(&self, disk: &Disk, size: u64) -> Result<(), ImageError>;

    /// Shrink `disk`'s volume to `size` bytes: on a copy, given to
    /// `prepare` and checked, that replaces the volume only if both pass.
    fn shrink_volume(&self, disk: &Disk, size: u64, prepare: Prepare<'_>)
    -> Result<(), ImageError>;

    /// Hold `disk`'s volume against every other node while the returned
    /// guard lives (S16): `Busy` when someone holds it. `None` where no
    /// lock is needed (a node-local volume only its node opens).
    fn lock(&self, disk: &Disk) -> Result<Option<std::fs::File>, ImageError> {
        let Some(path) = self.binding(disk).lock().map(String::from) else {
            return Ok(None);
        };
        match glidex_vm_shim::util::try_lock_file(Path::new(&path)) {
            Ok(Some(f)) => Ok(Some(f)),
            Ok(None) => Err(ImageError::Busy(format!(
                "disk {} is in use elsewhere ({path} is locked)",
                disk.name
            ))),
            Err(e) => Err(ImageError::Io(format!("{path}: {e}"))),
        }
    }

    /// Bytes `disk`'s volume takes where it lives (metering's
    /// `disk.stored`); `None` when it can't be told.
    fn stored_bytes(&self, disk: &Disk) -> Option<u64> {
        crate::images::qemu_img::info(&self.binding(disk).path(), Some(disk.format))
            .ok()
            .map(|i| i.actual_size)
    }

    /// `disk`'s volume's size in bytes, when the driver can tell it without
    /// a disk tool; `None`: ask `qemu-img info`.
    fn volume_size(&self, disk: &Disk) -> Option<u64> {
        let _ = disk;
        None
    }

    /// `disk`'s volume as the disk tools (partition reads and edits) open
    /// it on this node; the staged binding unless the driver says
    /// otherwise.
    fn tool_binding(&self, disk: &Disk) -> Result<VolumeBinding, ImageError> {
        self.stage(disk)
    }

    /// `disk`'s volume, staged for a VM about to start here; `preempt`
    /// when the VM was re-placed from a fenced node (S23), which only a
    /// driver with leases uses.
    fn attach_for_vm(&self, disk: &Disk, preempt: bool) -> Result<VolumeBinding, ImageError> {
        let _ = preempt;
        self.stage(disk)
    }

    /// The VM using `disk` here has stopped: let go of what staging holds
    /// (a hyperstor volume's lease). Nothing for most drivers.
    fn unstage(&self, disk: &Disk) -> Result<(), ImageError> {
        let _ = disk;
        Ok(())
    }

    /// `disk`'s volume, ready to be opened on this node.
    fn stage(&self, disk: &Disk) -> Result<VolumeBinding, ImageError> {
        let b = self.binding(disk);
        if !self.exists(disk) {
            return Err(ImageError::Io(format!(
                "disk {} file is missing: {}",
                disk.name,
                b.path().display()
            )));
        }
        Ok(b)
    }
}
