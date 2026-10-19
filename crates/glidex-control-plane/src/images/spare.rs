//! Spare files (spec/clustering.md §9.2.3): files in a node's image
//! directory that no image record names. A host that joined a cluster, or
//! a node that held an image the cluster later gains, has them. They are
//! never deleted on their own (images.md §2); a node adopts one that
//! matches a ready image (size and SHA-256), and reports the rest.

use super::{ImageKind, ImageManager};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The index of hashed spare files, beside them.
const INDEX: &str = "spare-index.json";

/// One spare file, as a node reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpareFile {
    pub name: String,
    pub bytes: u64,
    /// Seconds since the epoch.
    pub mtime: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    /// File name → (size, mtime, SHA-256): a hash stays valid while the
    /// size and mtime do.
    #[serde(default)]
    hashes: BTreeMap<String, (u64, u64, String)>,
    /// The last `drop_spare_files_before` this node applied.
    #[serde(default)]
    dropped_before: u64,
}

fn mtime_of(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl ImageManager {
    fn index_path(&self) -> PathBuf {
        crate::paths::file_in(&self.settings.image_dir, INDEX)
    }

    fn load_index(&self) -> Index {
        std::fs::read(self.index_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save_index(&self, idx: &Index) {
        if let Ok(b) = serde_json::to_vec(idx) {
            let tmp = crate::paths::file_in(&self.settings.image_dir, &format!("{INDEX}.tmp"));
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, self.index_path());
            }
        }
    }

    /// A file in the image directory by name (one `read_dir` gave).
    fn spare_path(&self, name: &str) -> PathBuf {
        crate::paths::file_in(&self.settings.image_dir, name)
    }

    /// The files of every image record, which are not spare.
    fn recorded_files(&self) -> BTreeSet<PathBuf> {
        let mut known = BTreeSet::new();
        for img in self.images.read().unwrap().values() {
            known.insert(self.image_path(&img.id));
            known.insert(self.part_path(&img.id));
            known.insert(self.initrd_part_path(&img.id));
            known.extend(self.image_files(img).into_iter().map(|(_, p)| p));
        }
        known
    }

    /// Spare files: regular files in the image directory that no record
    /// names, other than in-flight copies and the index itself.
    pub fn spare_files(&self) -> Vec<SpareFile> {
        let known = self.recorded_files();
        let Ok(rd) = std::fs::read_dir(&self.settings.image_dir) else {
            return Vec::new();
        };
        let mut out: Vec<SpareFile> = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                let m = e.metadata().ok().filter(|m| m.is_file())?;
                let transient =
                    name.ends_with(".part") || name == INDEX || name.ends_with(".json.tmp");
                (!transient && !known.contains(&p)).then(|| SpareFile {
                    name,
                    bytes: m.len(),
                    mtime: mtime_of(&m),
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// A spare file with this size and SHA-256, hashing only spare files of
    /// that size (once each: the index keeps the hash while the file is
    /// unchanged). Blocking: hashing reads the whole file.
    pub fn find_spare(&self, size: u64, sha256: &str) -> Option<PathBuf> {
        if sha256.is_empty() {
            return None;
        }
        let mut idx = self.load_index();
        let mut dirty = false;
        let mut found = None;
        for f in self.spare_files().into_iter().filter(|f| {
            f.bytes == size && !f.name.ends_with(".vars.fd") && !f.name.ends_with(".initrd")
        }) {
            let digest = match idx.hashes.get(&f.name) {
                Some((s, m, h)) if *s == f.bytes && *m == f.mtime => h.clone(),
                _ => {
                    let Ok(h) = super::download::sha256_path(&self.spare_path(&f.name)) else {
                        continue;
                    };
                    idx.hashes
                        .insert(f.name.clone(), (f.bytes, f.mtime, h.clone()));
                    dirty = true;
                    h
                }
            };
            if digest == sha256 {
                found = Some(self.spare_path(&f.name));
                break;
            }
        }
        if dirty {
            self.save_index(&idx);
        }
        found
    }

    /// Make the spare file `spare` the file of image `id` (of `kind`): a
    /// firmware's variable-store template (`<stem>.vars.fd`), or a kernel's
    /// initramfs (`<stem>.initrd`), goes with it.
    pub fn adopt_spare(&self, spare: &Path, id: &str, kind: ImageKind) -> std::io::Result<()> {
        let dest = match kind {
            ImageKind::Disk => self.image_path(id),
            ImageKind::Firmware => self.firmware_path(id),
            ImageKind::Kernel => self.kernel_path(id),
        };
        let companion = match kind {
            ImageKind::Disk => None,
            ImageKind::Firmware => Some((".fd", ".vars.fd", self.firmware_vars_template(id))),
            ImageKind::Kernel => Some((".kernel", ".initrd", self.kernel_initrd_path(id))),
        };
        if let Some((suffix, other, to)) = companion {
            let stem = spare
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(suffix))
                .unwrap_or_default();
            let from = self.spare_path(&format!("{stem}{other}"));
            if !stem.is_empty() && from.is_file() {
                std::fs::rename(&from, to)?;
            }
        }
        std::fs::rename(spare, dest)?;
        let mut idx = self.load_index();
        if let Some(n) = spare.file_name().and_then(|n| n.to_str()) {
            idx.hashes.remove(n);
        }
        self.save_index(&idx);
        Ok(())
    }

    /// Whether a delete request `before` hasn't been applied yet.
    pub fn spare_drop_pending(&self, before: u64) -> bool {
        before > self.load_index().dropped_before
    }

    /// Delete the spare files last modified before `before` (seconds), once
    /// per request: `before` is applied only if newer than the last one.
    /// Returns how many went.
    pub fn drop_spare_files(&self, before: u64) -> usize {
        let mut idx = self.load_index();
        if before <= idx.dropped_before {
            return 0;
        }
        let mut n = 0;
        for f in self.spare_files().into_iter().filter(|f| f.mtime < before) {
            if std::fs::remove_file(self.spare_path(&f.name)).is_ok() {
                idx.hashes.remove(&f.name);
                n += 1;
            }
        }
        idx.dropped_before = before;
        self.save_index(&idx);
        if n > 0 {
            tracing::warn!(files = n, "deleted unrecorded image files, as asked");
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::Image;

    fn manager(dir: &Path) -> std::sync::Arc<ImageManager> {
        let db = std::sync::Arc::new(crate::store::Db::create(dir.join("db")).unwrap());
        let settings = crate::images::ImageSettings {
            image_dir: dir.join("images"),
            disk_dir: dir.join("disks"),
            max_image_size: 1 << 30,
            default_root_bytes: 1 << 30,
            max_downloads: 1,
            allow_private_urls: false,
            max_kernel_size: super::super::kernel::MAX_KERNEL_SIZE,
            max_initrd_size: super::super::kernel::MAX_INITRD_SIZE,
        };
        ImageManager::new(db, settings).unwrap()
    }

    #[test]
    fn a_spare_file_matching_an_image_is_adopted_and_the_rest_reported_then_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path());
        let images = dir.path().join("images");
        std::fs::create_dir_all(&images).unwrap();
        // A firmware a host had before it joined, with its vars, and a stray file.
        std::fs::write(images.join("old-id.fd"), b"CODE").unwrap();
        std::fs::write(images.join("old-id.vars.fd"), b"VARS").unwrap();
        std::fs::write(images.join("stray.qcow2"), b"something else").unwrap();
        std::fs::write(images.join("x.qcow2.part"), b"in flight").unwrap();
        let names = |m: &ImageManager| {
            m.spare_files()
                .into_iter()
                .map(|f| f.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&m), ["old-id.fd", "old-id.vars.fd", "stray.qcow2"]);

        let sha = super::super::download::sha256_path(&images.join("old-id.fd")).unwrap();
        assert!(
            m.find_spare(4, "0".repeat(64).as_str()).is_none(),
            "another digest"
        );
        let spare = m.find_spare(4, &sha).expect("matches by size and digest");
        assert!(
            m.load_index().hashes.contains_key("old-id.fd"),
            "hashed once, kept"
        );
        m.adopt_spare(&spare, "cluster-id", ImageKind::Firmware)
            .unwrap();
        assert_eq!(
            std::fs::read(m.firmware_path("cluster-id")).unwrap(),
            b"CODE"
        );
        assert_eq!(
            std::fs::read(m.firmware_vars_template("cluster-id")).unwrap(),
            b"VARS",
            "its vars go with it"
        );

        // The adopted files belong to a record once it is there.
        let mut img: Image = serde_json::from_value(serde_json::json!({
            "id": "cluster-id", "name": "fw", "kind": "firmware", "hypervisor": "cloudhypervisor",
            "source": { "kind": "firmware", "key": "cloudhv-edk2", "url": "u", "version": "v" },
            "status": { "state": "ready" }, "format": "raw", "virtual_size_bytes": 4, "file_size_bytes": 4,
            "sha256": sha, "arch": "x86_64", "created_at": 1,
        }))
        .unwrap();
        img.pinned.clear();
        m.insert_image(&img).unwrap();
        assert_eq!(names(&m), ["stray.qcow2"]);

        // A delete request applies once, to files older than it.
        assert_eq!(m.drop_spare_files(u64::MAX - 1), 1);
        assert!(!images.join("stray.qcow2").exists());
        std::fs::write(images.join("later.qcow2"), b"new").unwrap();
        assert_eq!(
            m.drop_spare_files(u64::MAX - 1),
            0,
            "the same request again does nothing"
        );
        assert!(images.join("later.qcow2").exists());
    }
}
