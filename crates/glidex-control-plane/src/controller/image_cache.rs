//! Per-node image caches (spec/clustering.md §9.2): the catalog record of an
//! image is cluster-wide, its files are a copy on each node that needs one.
//! `image_caches/<image>/<node>` says where a copy is.
//!
//! A node gets its copy from a node that has one, over the node API, and
//! checks it against the digest in the record before using it. (The plan
//! has each node download from the source; copying from a holder keeps one
//! pull per image and the same check.)

use crate::images::{Image, ImageStatus};
use crate::state::{VmManager, VmManagerError};
use crate::store::{Origin, TableId};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CachePhase {
    Downloading,
    Ready,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheRow {
    pub phase: CachePhase,
    pub bytes: u64,
    pub verified_at: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

fn cache_key(image: &str, node: &str) -> String {
    format!("{image}/{node}")
}

type Next = Result<Option<Duration>, VmManagerError>;

impl VmManager {
    /// The copies of `image` on nodes of the cluster: `(node, row)`. A node
    /// that left (a tombstone) took its files with it, so its row, until the
    /// leader prunes it, counts for nothing.
    pub fn image_caches(&self, image: &str) -> Vec<(String, CacheRow)> {
        let gone: std::collections::HashSet<String> = self
            .nodes()
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| n.status.phase.is_tombstone())
            .map(|n| n.meta.id)
            .collect();
        self.image_cache_rows(image)
            .into_iter()
            .filter(|(n, _)| !gone.contains(n))
            .collect()
    }

    /// Every row of `image`'s copies, of nodes in the cluster or not.
    fn image_cache_rows(&self, image: &str) -> Vec<(String, CacheRow)> {
        let db = self.store.database();
        let Ok(txn) = db.begin_read() else {
            return Vec::new();
        };
        let Ok(t) = txn.open_table(TableId::ImageCaches.definition()) else {
            return Vec::new();
        };
        let prefix = format!("{image}/");
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

    /// The leader forgets the copies of nodes that left the cluster (or
    /// whose records are gone): their rows, and their place in `pinned` and
    /// `release`. Without this, deleting an image whose copy was on such a
    /// node would wait for that node forever. Returns whether `img` changed.
    pub(crate) fn prune_departed_copies(&self, img: &mut Image) -> Result<bool, VmManagerError> {
        let nodes = self.nodes().list().unwrap_or_default();
        if nodes.is_empty() {
            return Ok(false);
        }
        let in_cluster = |id: &str| {
            nodes
                .iter()
                .any(|n| n.meta.id == id && !n.status.phase.is_tombstone())
        };
        let stale: Vec<String> = self
            .image_cache_rows(&img.id)
            .into_iter()
            .map(|(n, _)| n)
            .filter(|n| !in_cluster(n))
            .collect();
        if !stale.is_empty() {
            let db = self.store.database();
            db.write(Origin::Controller, |tx| -> Result<(), VmManagerError> {
                let mut t = tx
                    .open_table(TableId::ImageCaches.definition())
                    .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
                for n in &stale {
                    t.remove(cache_key(&img.id, n).as_str())
                        .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
                }
                Ok(())
            })?;
            for n in &stale {
                let name = nodes
                    .iter()
                    .find(|x| x.meta.id == *n)
                    .map(|x| x.spec.name.clone())
                    .unwrap_or_else(|| n.clone());
                tracing::info!(image = %img.name, node = %name, "forgot the copy of a node that left the cluster");
            }
        }
        let (pinned, release) = (img.pinned.len(), img.release.len());
        img.pinned.retain(|n| in_cluster(n));
        img.release.retain(|n| in_cluster(n));
        let changed = img.pinned.len() != pinned || img.release.len() != release;
        if changed {
            self.images.put_image(img)?;
        }
        Ok(!stale.is_empty() || changed)
    }

    fn put_cache_row(&self, image: &str, row: Option<CacheRow>) -> Result<(), VmManagerError> {
        let key = cache_key(image, &self.local_node_id());
        let db = self.store.database();
        db.write(Origin::Controller, |tx| -> Result<(), VmManagerError> {
            let mut t = tx
                .open_table(TableId::ImageCaches.definition())
                .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
            match &row {
                Some(r) => {
                    t.insert(
                        key.as_str(),
                        serde_json::to_vec(r)
                            .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?
                            .as_slice(),
                    )
                    .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
                }
                None => {
                    t.remove(key.as_str())
                        .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
                }
            }
            Ok(())
        })
    }

    /// A node a copy may go to or leave (§9.2.2): known and not a tombstone.
    pub(crate) fn copy_node(
        &self,
        key: &str,
    ) -> Result<crate::node::Node, crate::images::ImageError> {
        // A name may also be a tombstone's (a host that left and joined
        // again): the live node is the one meant.
        let matching: Vec<crate::node::Node> = self
            .nodes()
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| n.meta.id == key || n.spec.name == key)
            .collect();
        if let Some(n) = matching.iter().find(|n| !n.status.phase.is_tombstone()) {
            return Ok(n.clone());
        }
        match matching.first() {
            Some(n) => Err(crate::images::ImageError::InUse(format!(
                "node {} is {:?}",
                n.spec.name, n.status.phase
            ))),
            None => Err(crate::images::ImageError::NotFound(format!("node {key}"))),
        }
    }

    /// What on `node` needs `img` (a disk from it, a VM booting through it).
    async fn users_on(&self, img: &Image, node: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .images
            .list_disks()
            .into_iter()
            .filter(|d| d.node.as_deref() == Some(node) && matches!(&d.origin, crate::images::DiskOrigin::Image { image_id, .. } if *image_id == img.id))
            .map(|d| format!("disk {}", d.name))
            .collect();
        out.extend(
            self.vms
                .read()
                .await
                .values()
                .filter(|v| {
                    v.status.placement.as_ref().map(|p| p.node.as_str()) == Some(node)
                        && boots(v, &img.id)
                })
                .map(|v| format!("VM {}", v.name)),
        );
        out
    }

    /// `POST /images/{id}/copies`: keep a copy on `node` ahead of need.
    pub async fn add_image_copy(
        &self,
        key: &str,
        node: &str,
    ) -> Result<crate::images::ImageResponse, VmManagerError> {
        use crate::images::ImageError;
        if self.cluster().is_none() {
            return Err(
                ImageError::InvalidImage("copies on nodes are for a cluster".into()).into(),
            );
        }
        let mut img = self.images.get_image(key)?;
        if img.deletion_requested_at.is_some() {
            return Err(ImageError::InUse(format!("image {} is being deleted", img.name)).into());
        }
        let n = self.copy_node(node)?;
        if !n.status.features.runs_arch(img.arch.as_str()) {
            return Err(ImageError::InUse(format!(
                "{} is {}, image {} is {}",
                n.spec.name,
                n.status.features.arch,
                img.name,
                img.arch.as_str()
            ))
            .into());
        }
        img.release.retain(|r| *r != n.meta.id);
        if !img.pinned.contains(&n.meta.id) {
            img.pinned.push(n.meta.id.clone());
        }
        self.images.put_image(&img)?;
        self.queue
            .add(crate::controller::queue::Key::Image(img.id.clone()));
        Ok(self.images.image_response(&img, false))
    }

    /// `DELETE /images/{id}/copies/{node}`: let `node`'s copy go. Refused
    /// while the node needs it, or when it is the last ready copy.
    pub async fn remove_image_copy(
        &self,
        key: &str,
        node: &str,
    ) -> Result<crate::images::ImageResponse, VmManagerError> {
        use crate::images::ImageError;
        let mut img = self.images.get_image(key)?;
        let nodes = self.nodes().list().unwrap_or_default();
        let id = nodes
            .iter()
            .find(|n| n.meta.id == node)
            .or_else(|| {
                nodes
                    .iter()
                    .find(|n| n.spec.name == node && !n.status.phase.is_tombstone())
            })
            .map(|n| n.meta.id.clone())
            .unwrap_or_else(|| node.to_string());
        let users = self.users_on(&img, &id).await;
        if !users.is_empty() {
            return Err(
                ImageError::InUse(format!("that node needs it: {}", users.join(", "))).into(),
            );
        }
        let caches = self.image_caches(&img.id);
        if !caches.iter().any(|(n, _)| *n == id) && !img.pinned.contains(&id) {
            return Err(ImageError::NotFound(format!("no copy of {} on {node}", img.name)).into());
        }
        if !caches
            .iter()
            .any(|(n, r)| *n != id && r.phase == CachePhase::Ready)
        {
            return Err(ImageError::InUse(format!(
                "it is the last ready copy of {}: delete the image instead",
                img.name
            ))
            .into());
        }
        img.pinned.retain(|p| *p != id);
        if !img.release.contains(&id) {
            img.release.push(id);
        }
        self.images.put_image(&img)?;
        self.queue
            .add(crate::controller::queue::Key::Image(img.id.clone()));
        Ok(self.images.image_response(&img, false))
    }

    /// Whether anything on this node needs `img`'s file.
    async fn needs_image_here(&self, img: &Image) -> bool {
        let disks = self.images.list_disks().into_iter().any(|d| self.is_local_disk(&d) && matches!(&d.origin, crate::images::DiskOrigin::Image { image_id, .. } if *image_id == img.id));
        if disks {
            return true;
        }
        self.vms
            .read()
            .await
            .values()
            .any(|v| self.is_local(v) && boots(v, &img.id))
    }

    fn image_files(&self, img: &Image) -> Vec<(&'static str, std::path::PathBuf)> {
        self.images.image_files(img)
    }

    /// The node-API address of a node (other than this one) with a ready copy.
    fn holder_of(&self, img: &Image) -> Option<String> {
        let me = self.local_node_id();
        self.image_caches(&img.id)
            .into_iter()
            .filter(|(n, r)| *n != me && r.phase == CachePhase::Ready)
            .find_map(|(n, _)| {
                self.nodes()
                    .get(&n)
                    .ok()
                    .flatten()
                    .and_then(|n| n.status.advertise)
                    .map(|a| a.to_string())
            })
    }

    /// Make sure this node has `img`'s file if it needs one, and say so.
    /// Used by every node for a Ready image; the node that downloaded it
    /// records its own copy.
    pub(crate) async fn reconcile_image_cache(&self, img: &Image) -> Next {
        let me = self.local_node_id();
        // Asked to let this copy go (§9.2.2), and nothing here needs it.
        if img.release.contains(&me) && !self.needs_image_here(img).await {
            if self.image_caches(&img.id).iter().any(|(n, _)| *n == me)
                || self.images.image_file(img).exists()
            {
                tracing::info!(image = %img.name, "dropping this node's copy, as asked");
                self.drop_image_cache(img)?;
            }
            return Ok(None);
        }
        let mut have = self
            .image_files(img)
            .iter()
            .filter(|(p, _)| *p == "main")
            .all(|(_, f)| f.exists());
        // A file this node already has for it, unrecorded (a host that
        // joined, §9.2.3): adopted, whether or not it is needed here.
        if !have && matches!(img.status, ImageStatus::Ready) {
            let (m, size, sha) = (self.images.clone(), img.file_size_bytes, img.sha256.clone());
            if let Ok(Some(spare)) =
                tokio::task::spawn_blocking(move || m.find_spare(size, &sha)).await
            {
                match self.images.adopt_spare(&spare, &img.id, img.kind) {
                    Ok(()) => {
                        tracing::info!(image = %img.name, file = %spare.display(), "adopted a file this node already had");
                        have = true;
                    }
                    Err(e) => {
                        tracing::warn!(image = %img.name, "adopting {}: {}", spare.display(), e)
                    }
                }
            }
        }
        let row = self
            .image_caches(&img.id)
            .into_iter()
            .find(|(n, _)| *n == me)
            .map(|(_, r)| r);
        if have {
            if row.as_ref().is_none_or(|r| r.phase != CachePhase::Ready) {
                let bytes = std::fs::metadata(self.images.image_file(img))
                    .map(|m| m.len())
                    .unwrap_or(0);
                self.put_cache_row(
                    &img.id,
                    Some(CacheRow {
                        phase: CachePhase::Ready,
                        bytes,
                        verified_at: crate::tenancy::now(),
                        error: String::new(),
                    }),
                )?;
            }
            return Ok(None);
        }
        if !matches!(img.status, ImageStatus::Ready)
            || !(img.pinned.contains(&me) || self.needs_image_here(img).await)
        {
            return Ok(None);
        }
        let Some(holder) = self.holder_of(img) else {
            // Nobody has it yet (the downloader is still writing its row).
            return Ok(Some(Duration::from_secs(5)));
        };
        let Some(cluster) = self.cluster() else {
            return Ok(None);
        };
        self.put_cache_row(
            &img.id,
            Some(CacheRow {
                phase: CachePhase::Downloading,
                bytes: 0,
                verified_at: 0,
                error: String::new(),
            }),
        )?;
        let mut result: Result<u64, String> = Ok(0);
        for (part, dest) in self.image_files(img) {
            let tmp = dest.with_extension("cache.part");
            let path = format!("/cluster/v1/images/{}/file?part={part}", img.id);
            match cluster.client.get_to_file(&holder, &path, &tmp).await {
                // Optional parts: a firmware without a template, a kernel
                // without an initramfs.
                Ok(404) if part == "vars" || (part == "initrd" && img.initrd.is_none()) => {
                    continue;
                }
                Ok(200) => {}
                Ok(code) => {
                    let _ = std::fs::remove_file(&tmp);
                    result = Err(format!("{holder} answered {code}"));
                    break;
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    result = Err(e.to_string());
                    break;
                }
            }
            // The parts with a recorded digest: the main file, a kernel's
            // initramfs.
            let want = match part {
                "main" => Some(img.sha256.clone()),
                "initrd" => img.initrd.as_ref().map(|i| i.sha256.clone()),
                _ => None,
            };
            if let Some(want) = want {
                let t = tmp.clone();
                let digest =
                    tokio::task::spawn_blocking(move || crate::images::download::sha256_path(&t))
                        .await
                        .ok()
                        .and_then(|r| r.ok());
                if !want.is_empty() && digest.as_deref() != Some(want.as_str()) {
                    let _ = std::fs::remove_file(&tmp);
                    result = Err(format!(
                        "the copy from {holder} does not match the image's digest"
                    ));
                    break;
                }
            }
            if let Err(e) = std::fs::rename(&tmp, &dest) {
                result = Err(e.to_string());
                break;
            }
            if part == "main" {
                result = Ok(std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0));
            }
        }
        match result {
            Ok(bytes) => {
                self.put_cache_row(
                    &img.id,
                    Some(CacheRow {
                        phase: CachePhase::Ready,
                        bytes,
                        verified_at: crate::tenancy::now(),
                        error: String::new(),
                    }),
                )?;
                // Disks and VMs waiting for the file.
                for d in self.images.list_disks() {
                    if matches!(&d.origin, crate::images::DiskOrigin::Image { image_id, .. } if *image_id == img.id)
                    {
                        self.queue
                            .add(crate::controller::queue::Key::Disk(d.id.clone()));
                    }
                }
                Ok(None)
            }
            Err(e) => {
                tracing::warn!(image = %img.name, "fetching the image: {}", e);
                self.put_cache_row(
                    &img.id,
                    Some(CacheRow {
                        phase: CachePhase::Failed,
                        bytes: 0,
                        verified_at: 0,
                        error: e,
                    }),
                )?;
                Ok(Some(Duration::from_secs(30)))
            }
        }
    }

    /// The leader's part of §9.2.2: a released node whose copy is gone
    /// leaves `release`; with `keep_server_copy: false`, once a pinned node
    /// has a ready copy, copies that nothing needs nor pins are released.
    /// Returns whether to look again soon.
    pub(crate) async fn tend_image_copies(&self, img: &mut Image) -> Result<bool, VmManagerError> {
        let caches = self.image_caches(&img.id);
        let mut changed = false;
        let before = img.release.len();
        img.release.retain(|n| caches.iter().any(|(c, _)| c == n));
        changed |= img.release.len() != before;
        let pinned_ready = img.pinned.iter().any(|p| {
            caches
                .iter()
                .any(|(n, r)| n == p && r.phase == CachePhase::Ready)
        });
        if !img.keep_server_copy && pinned_ready {
            for (n, r) in &caches {
                if r.phase == CachePhase::Ready
                    && !img.pinned.contains(n)
                    && !img.release.contains(n)
                    && self.users_on(img, n).await.is_empty()
                {
                    img.release.push(n.clone());
                    changed = true;
                }
            }
        }
        if changed {
            self.images.put_image(img)?;
        }
        let pending = !img.release.is_empty()
            || img.pinned.iter().any(|p| {
                !caches
                    .iter()
                    .any(|(n, r)| n == p && r.phase == CachePhase::Ready)
            });
        Ok(pending)
    }

    /// A deleted image: this node removes its copy and its row.
    pub(crate) fn drop_image_cache(&self, img: &Image) -> Result<(), VmManagerError> {
        for (_, f) in self.image_files(img) {
            let _ = std::fs::remove_file(&f);
        }
        let _ = std::fs::remove_file(self.images.part_path(&img.id));
        let _ = std::fs::remove_file(self.images.initrd_part_path(&img.id));
        self.put_cache_row(&img.id, None)
    }
}

/// Whether `vm` boots through firmware or kernel image `id`.
fn boots(vm: &crate::models::Vm, id: &str) -> bool {
    let c = vm.config();
    c.firmware_image.as_deref() == Some(id) || c.kernel_image.as_deref() == Some(id)
}
