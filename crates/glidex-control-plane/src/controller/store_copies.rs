//! Copies of images in shared datastores (spec/storage.md S8, S9): a linked
//! disk on a shared datastore overlays the datastore's own copy of its
//! image, `<path>/images/<image-id>.qcow2`, which every node reaching the
//! datastore sees. `image_caches/<image>/ds:<datastore>` says where one is,
//! beside the per-node rows (`image_cache.rs`).
//!
//! The copy is made by a worker that needs it (a node making a linked disk
//! there), under the datastore's lock for that image, from this node's own
//! copy when it has one, else from a node that does; it is checked against
//! the image's digest before it is used.

use super::image_cache::{CachePhase, CacheRow};
use crate::datastore::record::{self, Datastore};
use crate::images::{CloneMode, DiskOrigin, DiskPhase, Image, ImageKind, ImageStatus};
use crate::state::{VmManager, VmManagerError};
use crate::store::{Origin, TableId};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The key part that names a datastore rather than a node.
pub const DS_PREFIX: &str = "ds:";

type Next = Result<Option<Duration>, VmManagerError>;

fn persistence(e: impl std::fmt::Display) -> VmManagerError {
    VmManagerError::PersistenceError(e.to_string())
}

impl VmManager {
    /// The shared datastores holding a copy of `image`: `(datastore, row)`.
    pub fn store_copies(&self, image: &str) -> Vec<(String, CacheRow)> {
        let db = self.store.database();
        let Ok(txn) = db.begin_read() else {
            return Vec::new();
        };
        let Ok(t) = txn.open_table(TableId::ImageCaches.definition()) else {
            return Vec::new();
        };
        let prefix = format!("{image}/{DS_PREFIX}");
        let mut out = Vec::new();
        if let Ok(it) = t.range(prefix.as_str()..format!("{prefix}~").as_str()) {
            for r in it.flatten() {
                if let Ok(row) = serde_json::from_slice::<CacheRow>(r.1.value()) {
                    out.push((r.0.value()[prefix.len()..].to_string(), row));
                }
            }
        }
        out
    }

    /// Write (or, with `None`, remove) the row of `ds`'s copy of `image`.
    /// A node writes it as status, which the leader takes from a node that
    /// reaches `ds` (`check_status_write`).
    fn put_store_row(
        &self,
        image: &str,
        ds: &str,
        row: Option<&CacheRow>,
    ) -> Result<(), VmManagerError> {
        let key = format!("{image}/{DS_PREFIX}{ds}");
        self.store
            .database()
            .write(Origin::Controller, |tx| -> Result<(), VmManagerError> {
                let mut t = tx
                    .open_table(TableId::ImageCaches.definition())
                    .map_err(persistence)?;
                match row {
                    Some(r) => t
                        .insert(
                            key.as_str(),
                            serde_json::to_vec(r).map_err(persistence)?.as_slice(),
                        )
                        .map_err(persistence)?,
                    None => {
                        t.remove(key.as_str()).map_err(persistence)?;
                    }
                }
                Ok(())
            })
    }

    /// This node's path to `ds`'s copy of image `id`.
    pub(crate) fn store_image_path(&self, ds: &Datastore, id: &str) -> PathBuf {
        ds.image_copy_path(&self.local_node_id(), id)
    }

    /// The shared datastores this node must put `img` in now: those where
    /// it makes a linked disk from it (its worker) and no ready copy is.
    fn stores_needing(&self, img: &Image) -> Vec<Datastore> {
        let db = self.store.database();
        let ready: Vec<String> = self
            .store_copies(&img.id)
            .into_iter()
            .filter(|(_, r)| r.phase == CachePhase::Ready)
            .map(|(d, _)| d)
            .collect();
        let mut out: Vec<Datastore> = Vec::new();
        for d in self.images.list_disks() {
            let linked = matches!(&d.origin, DiskOrigin::Image { image_id, mode: CloneMode::Linked } if *image_id == img.id);
            if !linked
                || !matches!(d.phase, DiskPhase::Pending | DiskPhase::Creating)
                || ready.contains(&d.datastore)
                || out.iter().any(|o| o.id == d.datastore)
                || !self.images.is_shared(&d)
                || !self.is_local_disk(&d)
            {
                continue;
            }
            if let Ok(Some(ds)) = record::find(&db, &d.datastore) {
                out.push(ds);
            }
        }
        out
    }

    /// Put `img` in the shared datastores that need it here (S9). Every node
    /// runs this for a ready image.
    pub(crate) async fn reconcile_store_copies(&self, img: &Image) -> Next {
        if img.kind != ImageKind::Disk || !matches!(img.status, ImageStatus::Ready) {
            return Ok(None);
        }
        let mut again = None;
        for ds in self.stores_needing(img) {
            match self.make_store_copy(img, &ds).await {
                Ok(()) => {
                    // The disks waiting for it.
                    for d in self.images.list_disks() {
                        if d.datastore == ds.id
                            && matches!(&d.origin, DiskOrigin::Image { image_id, .. } if *image_id == img.id)
                        {
                            self.queue
                                .add(crate::controller::queue::Key::Disk(d.id.clone()));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(image = %img.name, datastore = %ds.name, "copying the image into the datastore: {}", e);
                    again = Some(Duration::from_secs(10));
                }
            }
        }
        Ok(again)
    }

    /// Copy `img` into shared datastore `ds` and record it, unless another
    /// node is doing so (the lock), or did (the file, checked).
    async fn make_store_copy(&self, img: &Image, ds: &Datastore) -> Result<(), String> {
        let root = PathBuf::from(ds.path_for(&self.local_node_id()));
        let lock = crate::paths::file_in(
            &root.join(".locks"),
            &format!("image-{}.lock", crate::images::safe_stem(&img.id)),
        );
        let _held = match glidex_vm_shim::util::try_lock_file(&lock) {
            Ok(Some(f)) => f,
            Ok(None) => return Err("another node is copying it".into()),
            Err(e) => return Err(format!("{}: {e}", lock.display())),
        };
        let dest = self.store_image_path(ds, &img.id);
        let dir = dest.parent().expect("a file in images/").to_path_buf();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let have = dest.exists() && digest_ok(&dest, &img.sha256).await;
        if !have {
            let tmp = dir.join(format!(".{}.part", crate::images::safe_stem(&img.id)));
            let _ = std::fs::remove_file(&tmp);
            self.put_store_row(
                &img.id,
                &ds.id,
                Some(&CacheRow {
                    phase: CachePhase::Downloading,
                    bytes: 0,
                    verified_at: 0,
                    error: String::new(),
                }),
            )
            .map_err(|e| e.to_string())?;
            let fetched = self.fetch_into(img, &tmp).await;
            let checked = match fetched {
                Ok(()) if digest_ok(&tmp, &img.sha256).await => Ok(()),
                Ok(()) => Err("the copy does not match the image's digest".to_string()),
                Err(e) => Err(e),
            };
            let placed = checked.and_then(|()| {
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444))
                    .and_then(|()| std::fs::rename(&tmp, &dest))
                    .map_err(|e| format!("{}: {e}", dest.display()))
            });
            if let Err(e) = placed {
                let _ = std::fs::remove_file(&tmp);
                let _ = self.put_store_row(
                    &img.id,
                    &ds.id,
                    Some(&CacheRow {
                        phase: CachePhase::Failed,
                        bytes: 0,
                        verified_at: 0,
                        error: e.clone(),
                    }),
                );
                return Err(e);
            }
            tracing::info!(image = %img.name, datastore = %ds.name, "image copied into the datastore");
        }
        let bytes = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        self.put_store_row(
            &img.id,
            &ds.id,
            Some(&CacheRow {
                phase: CachePhase::Ready,
                bytes,
                verified_at: crate::tenancy::now(),
                error: String::new(),
            }),
        )
        .map_err(|e| e.to_string())
    }

    /// `img`'s bytes into `tmp`: from this node's copy, else from a node
    /// with a ready one.
    async fn fetch_into(&self, img: &Image, tmp: &Path) -> Result<(), String> {
        let local = self.images.image_path(&img.id);
        if local.exists() {
            let (from, to) = (local.clone(), tmp.to_path_buf());
            return tokio::task::spawn_blocking(move || std::fs::copy(&from, &to))
                .await
                .map_err(|e| e.to_string())?
                .map(|_| ())
                .map_err(|e| format!("{}: {e}", local.display()));
        }
        let me = self.local_node_id();
        let holder = self
            .image_caches(&img.id)
            .into_iter()
            .filter(|(n, r)| *n != me && r.phase == CachePhase::Ready)
            .find_map(|(n, _)| self.nodes().get(&n).ok().flatten()?.status.advertise)
            .ok_or("no node has a ready copy to copy from")?;
        let cluster = self.cluster().ok_or("no node has a copy")?;
        let path = format!("/cluster/v1/images/{}/file?part=main", img.id);
        match cluster
            .client
            .get_to_file(&holder.to_string(), &path, tmp)
            .await
        {
            Ok(200) => Ok(()),
            Ok(code) => Err(format!("{holder} answered {code}")),
            Err(e) => Err(e.to_string()),
        }
    }

    /// A deleted image: this node removes the copies in shared datastores
    /// it reaches, and their rows. Linked disks on them kept the image from
    /// being deleted until they went (images.md §3).
    pub(crate) fn drop_store_copies(&self, img: &Image) -> Result<(), VmManagerError> {
        let db = self.store.database();
        let me = self.local_node_id();
        for (ds_id, _) in self.store_copies(&img.id) {
            let ds = record::find(&db, &ds_id).ok().flatten();
            let reaches = ds.as_ref().is_some_and(|ds| {
                record::rows(&db, &ds.id)
                    .unwrap_or_default()
                    .get(&me)
                    .is_some_and(|r| r.reachable)
            });
            match ds {
                Some(ds) if reaches => {
                    let _ = std::fs::remove_file(self.store_image_path(&ds, &img.id));
                    self.put_store_row(&img.id, &ds_id, None)?;
                }
                // A datastore that is gone took its files with it; the
                // leader forgets the row.
                None if db.can_write() => self.put_store_row(&img.id, &ds_id, None)?,
                _ => {}
            }
        }
        Ok(())
    }
}

/// Whether the file at `p` has digest `want` (any, when the image has none).
async fn digest_ok(p: &Path, want: &str) -> bool {
    if want.is_empty() {
        return true;
    }
    let p = p.to_path_buf();
    tokio::task::spawn_blocking(move || crate::images::download::sha256_path(&p))
        .await
        .ok()
        .and_then(|r| r.ok())
        .is_some_and(|d| d == want)
}

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
