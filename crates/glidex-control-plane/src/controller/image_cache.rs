//! Per-node image caches (spec/clustering.md §9.2): the catalog record of an
//! image is cluster-wide, its files are a copy on each node that needs one.
//! `image_caches/<image>/<node>` says where a copy is.
//!
//! A node gets its copy from a node that has one, over the node API, and
//! checks it against the digest in the record before using it. (The plan
//! has each node download from the source; copying from a holder keeps one
//! pull per image and the same check.)

use crate::images::{Image, ImageKind, ImageStatus};
use crate::state::{VmManager, VmManagerError};
use crate::store::{Origin, TableId};
use redb::ReadableTable;
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
    /// Every node's copy of `image`: `(node, row)`.
    pub fn image_caches(&self, image: &str) -> Vec<(String, CacheRow)> {
        let db = self.store.database();
        let Ok(txn) = db.begin_read() else { return Vec::new() };
        let Ok(t) = txn.open_table(TableId::ImageCaches.definition()) else { return Vec::new() };
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

    fn put_cache_row(&self, image: &str, row: Option<CacheRow>) -> Result<(), VmManagerError> {
        let key = cache_key(image, &self.local_node_id());
        let db = self.store.database();
        db.write(Origin::Controller, |tx| -> Result<(), VmManagerError> {
            let mut t = tx.open_table(TableId::ImageCaches.definition()).map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
            match &row {
                Some(r) => {
                    t.insert(key.as_str(), serde_json::to_vec(r).map_err(|e| VmManagerError::PersistenceError(e.to_string()))?.as_slice()).map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
                }
                None => {
                    t.remove(key.as_str()).map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
                }
            }
            Ok(())
        })
    }

    /// Whether anything on this node needs `img`'s file.
    async fn needs_image_here(&self, img: &Image) -> bool {
        let me = self.local_node_id();
        let disks = self.images.list_disks().into_iter().any(|d| d.node.as_deref().is_none_or(|n| n == me) && matches!(&d.origin, crate::images::DiskOrigin::Image { image_id, .. } if *image_id == img.id));
        if disks {
            return true;
        }
        self.vms.read().await.values().any(|v| self.is_local(v) && v.config().firmware_image.as_deref() == Some(img.id.as_str()))
    }

    fn image_files(&self, img: &Image) -> Vec<(&'static str, std::path::PathBuf)> {
        match img.kind {
            ImageKind::Disk => vec![("main", self.images.image_path(&img.id))],
            ImageKind::Firmware => vec![("main", self.images.firmware_path(&img.id)), ("vars", self.images.firmware_vars_template(&img.id))],
        }
    }

    /// The node-API address of a node (other than this one) with a ready copy.
    fn holder_of(&self, img: &Image) -> Option<String> {
        let me = self.local_node_id();
        self.image_caches(&img.id)
            .into_iter()
            .filter(|(n, r)| *n != me && r.phase == CachePhase::Ready)
            .find_map(|(n, _)| self.nodes().get(&n).ok().flatten().and_then(|n| n.status.advertise).map(|a| a.to_string()))
    }

    /// Make sure this node has `img`'s file if it needs one, and say so.
    /// Used by every node for a Ready image; the node that downloaded it
    /// records its own copy.
    pub(crate) async fn reconcile_image_cache(&self, img: &Image) -> Next {
        let me = self.local_node_id();
        let have = self.image_files(img).iter().filter(|(p, _)| *p == "main").all(|(_, f)| f.exists());
        let row = self.image_caches(&img.id).into_iter().find(|(n, _)| *n == me).map(|(_, r)| r);
        if have {
            if row.as_ref().is_none_or(|r| r.phase != CachePhase::Ready) {
                let bytes = std::fs::metadata(self.images.image_file(img)).map(|m| m.len()).unwrap_or(0);
                self.put_cache_row(&img.id, Some(CacheRow { phase: CachePhase::Ready, bytes, verified_at: crate::tenancy::now(), error: String::new() }))?;
            }
            return Ok(None);
        }
        if !matches!(img.status, ImageStatus::Ready) || !self.needs_image_here(img).await {
            return Ok(None);
        }
        let Some(holder) = self.holder_of(img) else {
            // Nobody has it yet (the downloader is still writing its row).
            return Ok(Some(Duration::from_secs(5)));
        };
        let Some(cluster) = self.cluster() else { return Ok(None) };
        self.put_cache_row(&img.id, Some(CacheRow { phase: CachePhase::Downloading, bytes: 0, verified_at: 0, error: String::new() }))?;
        let mut result: Result<u64, String> = Ok(0);
        for (part, dest) in self.image_files(img) {
            let tmp = dest.with_extension("cache.part");
            let path = format!("/cluster/v1/images/{}/file?part={part}", img.id);
            match cluster.client.get_to_file(&holder, &path, &tmp).await {
                Ok(404) if part == "vars" => continue,
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
            if part == "main" {
                let (t, want) = (tmp.clone(), img.sha256.clone());
                let digest = tokio::task::spawn_blocking(move || crate::images::download::sha256_path(&t)).await.ok().and_then(|r| r.ok());
                if !want.is_empty() && digest.as_deref() != Some(want.as_str()) {
                    let _ = std::fs::remove_file(&tmp);
                    result = Err(format!("the copy from {holder} does not match the image's digest"));
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
                self.put_cache_row(&img.id, Some(CacheRow { phase: CachePhase::Ready, bytes, verified_at: crate::tenancy::now(), error: String::new() }))?;
                // Disks and VMs waiting for the file.
                for d in self.images.list_disks() {
                    if matches!(&d.origin, crate::images::DiskOrigin::Image { image_id, .. } if *image_id == img.id) {
                        self.queue.add(crate::controller::queue::Key::Disk(d.id.clone()));
                    }
                }
                Ok(None)
            }
            Err(e) => {
                tracing::warn!(image = %img.name, "fetching the image: {}", e);
                self.put_cache_row(&img.id, Some(CacheRow { phase: CachePhase::Failed, bytes: 0, verified_at: 0, error: e }))?;
                Ok(Some(Duration::from_secs(30)))
            }
        }
    }

    /// A deleted image: this node removes its copy and its row.
    pub(crate) fn drop_image_cache(&self, img: &Image) -> Result<(), VmManagerError> {
        for (_, f) in self.image_files(img) {
            let _ = std::fs::remove_file(&f);
        }
        let _ = std::fs::remove_file(self.images.part_path(&img.id));
        self.put_cache_row(&img.id, None)
    }
}
