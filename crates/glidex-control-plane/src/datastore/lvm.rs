//! The `lvm` driver (spec/storage.md §11.1, S19): each disk is a raw
//! LVM-thin volume, `gx-d-<disk-id>`, made, activated and removed by
//! glidex-stord. A linked disk is a thin snapshot of the node's image
//! volume `gx-i-<image-id>`, made once from the node's verified image file.
//!
//! glidex-stord hands each activated device to the glidex user; filling it
//! (`qemu-img convert`) and editing its partitions happen here, as glidex.

use super::{Caps, Prepare, StorageDriver, VolumeBinding, VolumeSource};
use crate::images::qemu_img::{self, DiskFormat};
use crate::images::{Disk, DiskOrigin, ImageError};
use glidex_stord::client::{Client, ClientError};
use glidex_stord::proto::{Activated, Op, Volume};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// glidex-stord's socket, unless `GLIDEX_STORD_SOCKET` says otherwise.
pub fn socket_path() -> PathBuf {
    std::env::var_os("GLIDEX_STORD_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            glidex_stord::client::socket_path(Path::new(glidex_stord::proto::DEFAULT_RUN_DIR))
        })
}

/// Image volumes are made one at a time on a node: two disks from one
/// image must not both start filling it.
static IMAGE_VOLUMES: Mutex<()> = Mutex::new(());

#[derive(Debug)]
pub struct LvmDriver {
    vg: String,
    pool: String,
    socket: PathBuf,
}

/// `gx-d-<disk-id>`.
pub fn volume_name(disk: &Disk) -> String {
    format!(
        "{}{}",
        glidex_stord::names::DISK_PREFIX,
        crate::images::safe_stem(&disk.id)
    )
}

/// `gx-i-<image-id>`.
pub fn image_volume_name(image_id: &str) -> String {
    format!(
        "{}{}",
        glidex_stord::names::IMAGE_PREFIX,
        crate::images::safe_stem(image_id)
    )
}

fn stord_err(e: ClientError) -> ImageError {
    match e {
        ClientError::Remote(b) if b.code == "busy" => ImageError::Busy(b.message),
        ClientError::Remote(b) if b.code == "invalid_request" => {
            ImageError::invalid_disk(b.message)
        }
        ClientError::Remote(b) => ImageError::Io(format!("glidex-stord: {}", b.message)),
        other => ImageError::NotReady(other.to_string()),
    }
}

impl LvmDriver {
    pub fn new(vg: String, pool: String, socket: PathBuf) -> Self {
        Self { vg, pool, socket }
    }

    fn call<T: serde::de::DeserializeOwned>(&self, op: Op) -> Result<T, ImageError> {
        let mut c = Client::connect(&self.socket).map_err(stord_err)?;
        c.call(op).map_err(stord_err)
    }

    fn volumes(&self) -> Result<Vec<Volume>, ImageError> {
        self.call(Op::ListVolumes {
            vg: self.vg.clone(),
        })
    }

    fn device(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/dev/{}/{name}", self.vg))
    }

    fn block(&self, name: &str) -> VolumeBinding {
        VolumeBinding::Block {
            path: self.device(name).to_string_lossy().into_owned(),
        }
    }

    fn delete(&self, name: &str) -> Result<(), ImageError> {
        self.call::<serde_json::Value>(Op::DeleteVolume {
            vg: self.vg.clone(),
            name: name.to_string(),
        })
        .map(|_| ())
    }

    fn create(&self, name: &str, size: u64) -> Result<Activated, ImageError> {
        self.call(Op::CreateVolume {
            vg: self.vg.clone(),
            pool: self.pool.clone(),
            name: name.to_string(),
            size_bytes: size,
        })
    }

    /// The node's sealed image volume for `image_id`, made from `file`
    /// (`size` bytes, virtual) when there is none yet.
    fn image_volume(&self, image_id: &str, file: &Path, size: u64) -> Result<String, ImageError> {
        let _one = IMAGE_VOLUMES.lock().unwrap_or_else(|e| e.into_inner());
        let name = image_volume_name(image_id);
        match self.volumes()?.into_iter().find(|v| v.name == name) {
            Some(v) if v.read_only => return Ok(name),
            // Left half-filled by a crash: start over.
            Some(_) => self.delete(&name)?,
            None => {}
        }
        let made = self.create(&name, size)?;
        let filled =
            qemu_img::convert_to_device(file, Some(DiskFormat::Qcow2), Path::new(&made.path))
                .and_then(|()| {
                    self.call::<serde_json::Value>(Op::SealVolume {
                        vg: self.vg.clone(),
                        name: name.clone(),
                    })
                    .map(|_| ())
                });
        if let Err(e) = filled {
            let _ = self.delete(&name);
            return Err(e);
        }
        Ok(name)
    }

    /// Remove image `id`'s volume, if there is one (a missing one is fine).
    pub fn delete_image_volume(&self, id: &str) -> Result<(), ImageError> {
        self.delete(&image_volume_name(id))
    }

    /// Build `disk`'s volume under a temporary name with `make` (given that
    /// name), run `prepare` on it, then rename it into place: a volume that
    /// exists under the disk's own name is complete, as a file is once
    /// renamed. A failure removes the temporary volume.
    fn build(
        &self,
        disk: &Disk,
        make: impl FnOnce(&str) -> Result<(), ImageError>,
        prepare: Prepare<'_>,
    ) -> Result<(), ImageError> {
        let name = volume_name(disk);
        let tmp = format!("{name}.new");
        // Left by an attempt that didn't finish.
        if self.volumes()?.iter().any(|v| v.name == tmp) {
            self.delete(&tmp)?;
        }
        let r = make(&tmp)
            .and_then(|()| prepare(&self.block(&tmp)))
            .and_then(|()| {
                self.call::<Activated>(Op::RenameVolume {
                    vg: self.vg.clone(),
                    from: tmp.clone(),
                    to: name,
                })
                .map(|_| ())
            });
        if r.is_err() {
            let _ = self.delete(&tmp);
        }
        r
    }
}

impl StorageDriver for LvmDriver {
    fn capabilities(&self) -> Caps {
        Caps {
            linked_clones: true,
            shrink: false,
        }
    }

    fn binding(&self, disk: &Disk) -> VolumeBinding {
        self.block(&volume_name(disk))
    }

    fn exists(&self, disk: &Disk) -> bool {
        // An inactive volume has no device node: ask LVM.
        let name = volume_name(disk);
        self.volumes()
            .map(|v| v.iter().any(|v| v.name == name))
            .unwrap_or(false)
    }

    fn create_volume(
        &self,
        disk: &Disk,
        size: u64,
        source: VolumeSource<'_>,
        prepare: Prepare<'_>,
    ) -> Result<(), ImageError> {
        match source {
            VolumeSource::Blank => self.build(disk, |t| self.create(t, size).map(|_| ()), prepare),
            VolumeSource::Image {
                path,
                size_bytes,
                linked: false,
            } => self.build(
                disk,
                |t| {
                    let a = self.create(t, size.max(size_bytes))?;
                    qemu_img::convert_to_device(path, Some(DiskFormat::Qcow2), Path::new(&a.path))
                },
                prepare,
            ),
            VolumeSource::Image {
                path,
                size_bytes,
                linked: true,
            } => {
                let DiskOrigin::Image { image_id, .. } = &disk.origin else {
                    return Err(ImageError::invalid_disk("a linked disk names its image"));
                };
                let origin = self.image_volume(image_id, path, size_bytes)?;
                self.build(
                    disk,
                    |t| {
                        self.call::<Activated>(Op::SnapshotVolume {
                            vg: self.vg.clone(),
                            origin,
                            name: t.to_string(),
                            size_bytes: size.max(size_bytes),
                        })
                        .map(|_| ())
                    },
                    prepare,
                )
            }
        }
    }

    fn copy_in(
        &self,
        disk: &Disk,
        from: &Path,
        _flatten: bool,
        prepare: Prepare<'_>,
    ) -> Result<(), ImageError> {
        // Always a standalone copy: a raw volume has no backing file. The
        // source is a disk glidex made, so its format may be probed.
        let info = qemu_img::info(from, None)?;
        self.build(
            disk,
            |t| {
                let a = self.create(t, info.virtual_size)?;
                qemu_img::convert_to_device(from, None, Path::new(&a.path))
            },
            prepare,
        )
    }

    fn delete_volume(&self, disk: &Disk) -> Result<(), ImageError> {
        self.delete(&volume_name(disk))
    }

    fn expand_volume(&self, disk: &Disk, size: u64) -> Result<(), ImageError> {
        self.call::<Activated>(Op::ExtendVolume {
            vg: self.vg.clone(),
            name: volume_name(disk),
            size_bytes: size,
        })
        .map(|_| ())
    }

    fn shrink_volume(&self, disk: &Disk, _: u64, _: Prepare<'_>) -> Result<(), ImageError> {
        Err(ImageError::invalid_disk(format!(
            "disk {} is an LVM volume, which can't be made smaller",
            disk.name
        )))
    }

    fn stage(&self, disk: &Disk) -> Result<VolumeBinding, ImageError> {
        let a: Activated = self
            .call(Op::Activate {
                vg: self.vg.clone(),
                name: volume_name(disk),
            })
            .map_err(|e| match e {
                ImageError::Io(m) if m.contains("no volume") => {
                    ImageError::Io(format!("disk {} volume is missing: {m}", disk.name))
                }
                other => other,
            })?;
        Ok(VolumeBinding::Block { path: a.path })
    }
}

/// What `node` finds of an `lvm` datastore (§4.1): its volume group and
/// thin pool, through glidex-stord. Blocks.
pub fn probe(vg: &str, pool: &str, socket: &Path, now: u64) -> super::record::NodeRow {
    let mut row = super::record::NodeRow {
        path: format!("{vg}/{pool}"),
        checked_at: now,
        ..Default::default()
    };
    let st: Result<glidex_stord::proto::PoolStatus, ClientError> = Client::connect(socket)
        .and_then(|mut c| {
            c.call(Op::PoolStatus {
                vg: vg.to_string(),
                pool: pool.to_string(),
            })
        });
    match st {
        Ok(st) if st.usable => {
            row.reachable = true;
            row.total_bytes = st.size_bytes;
            row.free_bytes = st.size_bytes.saturating_sub(st.used_bytes);
            if st.metadata_percent >= 80.0 {
                row.warnings.push(format!(
                    "the thin pool's metadata is {:.0}% full: extend it (lvextend --poolmetadatasize)",
                    st.metadata_percent
                ));
            }
        }
        Ok(st) => {
            row.reason = st.reason;
            row.message = st.message;
        }
        Err(e) => {
            row.reason = "StordUnavailable".into();
            row.message = e.to_string();
        }
    }
    row
}

/// Let VM `vm_id`'s unit open exactly `devices` (spec/storage.md S18).
/// Without block volumes on a host with no glidex-stord there is nothing
/// to undo, so nothing is asked. Blocks.
pub fn allow_devices(vm_id: &str, devices: Vec<String>) -> Result<(), ImageError> {
    let socket = socket_path();
    if devices.is_empty() && !socket.exists() {
        return Ok(());
    }
    let mut c = Client::connect(&socket).map_err(stord_err)?;
    c.call::<serde_json::Value>(Op::AllowDevices {
        vm_id: vm_id.to_string(),
        devices,
    })
    .map(|_| ())
    .map_err(stord_err)
}
