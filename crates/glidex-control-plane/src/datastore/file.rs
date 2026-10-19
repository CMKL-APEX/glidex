//! The file driver: volumes are qcow2 or raw files named by disk id in one
//! directory (spec/images.md §2). Every change is made on a dot-file
//! temporary in the same directory and renamed into place, so a crash
//! leaves either the old file or the new one.

use super::{Caps, Prepare, StorageDriver, VolumeBinding, VolumeSource};
use crate::images::qemu_img::{self, DiskFormat};
use crate::images::{Disk, ImageError};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct FileDriver {
    dir: PathBuf,
}

impl FileDriver {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `<dir>/<disk-id>.<qcow2|raw>`.
    pub fn path(&self, disk: &Disk) -> PathBuf {
        crate::paths::file_in(
            &self.dir,
            &format!(
                "{}.{}",
                crate::images::safe_stem(&disk.id),
                disk.format.extension()
            ),
        )
    }

    fn temp(&self, disk: &Disk, what: &str) -> TempFile {
        let p = crate::paths::file_in(
            &self.dir,
            &format!(
                ".{}.{}.{}",
                crate::images::safe_stem(&disk.id),
                what,
                disk.format.extension()
            ),
        );
        let _ = std::fs::remove_file(&p);
        TempFile(Some(p))
    }

    fn file(path: &Path, disk: &Disk) -> VolumeBinding {
        VolumeBinding::File {
            path: path.to_string_lossy().into_owned(),
            format: disk.format,
            backing_files: disk.is_linked(),
        }
    }
}

impl StorageDriver for FileDriver {
    fn capabilities(&self) -> Caps {
        Caps {
            linked_clones: true,
            shrink: true,
        }
    }

    fn binding(&self, disk: &Disk) -> VolumeBinding {
        Self::file(&self.path(disk), disk)
    }

    fn exists(&self, disk: &Disk) -> bool {
        self.path(disk).exists()
    }

    fn create_volume(
        &self,
        disk: &Disk,
        size: u64,
        source: VolumeSource<'_>,
        prepare: Prepare<'_>,
    ) -> Result<(), ImageError> {
        let tmp = self.temp(disk, "create");
        match source {
            VolumeSource::Blank => qemu_img::create(tmp.path(), disk.format, size, None)?,
            VolumeSource::Image {
                path, linked: true, ..
            } => {
                // Named relative to this directory, where the overlay
                // lives, so it never depends on where that is mounted
                // (spec/storage.md S8).
                let backing = crate::paths::relative_to(path, &self.dir)
                    .unwrap_or_else(|| path.to_path_buf());
                qemu_img::create(tmp.path(), disk.format, size, Some(&backing))?
            }
            VolumeSource::Image {
                path, size_bytes, ..
            } => {
                qemu_img::convert(path, DiskFormat::Qcow2, tmp.path(), disk.format)?;
                if size > size_bytes {
                    qemu_img::resize(tmp.path(), disk.format, size, false)?;
                }
            }
        }
        set_mode(tmp.path(), 0o600)?;
        prepare(&Self::file(tmp.path(), disk))?;
        std::fs::rename(tmp.keep(), self.path(disk)).map_err(|e| ImageError::Io(e.to_string()))
    }

    fn delete_volume(&self, disk: &Disk) -> Result<(), ImageError> {
        let path = self.path(disk);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ImageError::Io(format!("{}: {}", path.display(), e))),
        }
    }

    fn expand_volume(&self, disk: &Disk, size: u64) -> Result<(), ImageError> {
        // In place: growing only adds space at the end.
        qemu_img::resize(&self.path(disk), disk.format, size, false)
    }

    fn shrink_volume(
        &self,
        disk: &Disk,
        size: u64,
        prepare: Prepare<'_>,
    ) -> Result<(), ImageError> {
        // Work on a copy: `qemu-img resize --shrink` discards data without
        // asking, and a crash part way must leave the original intact.
        let path = self.path(disk);
        let tmp = self.temp(disk, "shrink");
        qemu_img::copy_sparse(&path, tmp.path())?;
        qemu_img::resize(tmp.path(), disk.format, size, true)?;
        prepare(&Self::file(tmp.path(), disk))?;
        qemu_img::check(tmp.path(), disk.format)?;
        set_mode(tmp.path(), 0o600)?;
        std::fs::rename(tmp.keep(), &path).map_err(|e| ImageError::Io(e.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::{DiskOrigin, DiskPhase};

    fn disk(id: &str, format: DiskFormat) -> Disk {
        let mut d = Disk::new(
            id.into(),
            id.into(),
            String::new(),
            format,
            0,
            DiskOrigin::Blank,
        );
        d.phase = DiskPhase::Pending;
        d
    }

    /// Only the disk's own file, never a temporary, is left in the directory.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn a_failed_prepare_leaves_nothing_behind() {
        if qemu_img::QEMU_IMG.require().is_err() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let drv = FileDriver::new(tmp.path().to_path_buf());
        let d = disk("d1", DiskFormat::Raw);
        let mut seen = None;
        let r = drv.create_volume(&d, 1 << 20, VolumeSource::Blank, &mut |b| {
            seen = Some(b.clone());
            Err(ImageError::invalid_disk("no"))
        });
        assert!(r.is_err());
        // `prepare` saw a temporary, not the disk's own file.
        let seen = seen.unwrap();
        assert_ne!(seen.path(), drv.path(&d));
        assert!(!drv.exists(&d));
        assert!(names(tmp.path()).is_empty());

        drv.create_volume(&d, 2 << 20, VolumeSource::Blank, &mut |_| Ok(()))
            .unwrap();
        assert!(drv.exists(&d));
        let mode = std::fs::metadata(drv.path(&d))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // A shrink whose `prepare` fails keeps the volume as it was.
        let r = drv.shrink_volume(&d, 1 << 20, &mut |_| Err(ImageError::invalid_disk("no")));
        assert!(r.is_err());
        assert_eq!(std::fs::metadata(drv.path(&d)).unwrap().len(), 2 << 20);
        assert_eq!(names(tmp.path()), vec!["d1.raw".to_string()]);

        drv.delete_volume(&d).unwrap();
        drv.delete_volume(&d).unwrap();
        assert!(names(tmp.path()).is_empty());
    }

    #[test]
    fn staging_a_missing_volume_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let drv = FileDriver::new(tmp.path().to_path_buf());
        let d = disk("gone", DiskFormat::Qcow2);
        let e = drv.stage(&d).unwrap_err();
        assert!(e.to_string().contains("file is missing"), "{e}");
        assert_eq!(
            drv.binding(&d),
            VolumeBinding::File {
                path: tmp.path().join("gone.qcow2").to_string_lossy().into_owned(),
                format: DiskFormat::Qcow2,
                backing_files: false,
            }
        );
    }
}
