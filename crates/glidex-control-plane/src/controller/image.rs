//! The image controller (spec/reconciliation.md §10.2): supervises
//! downloads (resuming one whose task is gone), downloads a failed image
//! again once asked to (`retry_seq`), and notices a missing file. A ready
//! image whose file disappears is never downloaded again: catalog URLs
//! point at "current/latest", and a different file would sit under the
//! linked disks written against the old one.

use crate::controller::queue::Key;
use crate::images::{DiskPhase, ImageStatus};
use crate::state::{VmManager, VmManagerError};
use crate::store::{event_key, Event, EventKind};
use std::time::Duration;

type Next = Result<Option<Duration>, VmManagerError>;

impl VmManager {
    fn image_event(&self, id: &str, kind: EventKind, reason: &str, message: impl Into<String>) {
        let _ = self.store.push_event(&event_key("image", id), Event::new("controller", kind, reason, message));
    }

    /// One round for image `id`.
    pub async fn reconcile_image(&self, id: &str) -> Next {
        let Ok(mut img) = self.images.get_image(id) else {
            return Ok(None);
        };
        let leads = self.store.database().can_write();
        if img.deletion_requested_at.is_some() {
            // Every node's copy goes first (§9.2); the record is the leader's to remove.
            if self.cluster().is_some() {
                self.drop_image_cache(&img)?;
                if !leads {
                    return Ok(None);
                }
                if self.image_caches(&img.id).iter().any(|(n, _)| *n != self.local_node_id()) {
                    return Ok(Some(Duration::from_secs(5)));
                }
            }
            // The `image.download` and `image.file` finalizers (§6.3).
            self.images.finish_image_delete(&img.id)?;
            self.image_event(&img.id, EventKind::Normal, "Deleted", "");
            return Ok(None);
        }
        // A node that doesn't write the catalog keeps only its copy.
        if !leads {
            return self.reconcile_image_cache(&img).await;
        }
        match img.status.clone() {
            ImageStatus::Downloading { .. } | ImageStatus::Verifying => {
                if !self.images.download_running(&img.id) {
                    tracing::info!(image = %img.name, "resuming image download");
                    self.images.spawn_download(img.id.clone());
                }
                Ok(Some(Duration::from_secs(10)))
            }
            ImageStatus::Failed { .. } if img.retry_seq > img.applied_retry_seq => {
                img.applied_retry_seq = img.retry_seq;
                img.status = ImageStatus::Downloading { received_bytes: 0, total_bytes: None };
                let _ = std::fs::remove_file(self.images.part_path(&img.id));
                self.images.put_image(&img)?;
                self.image_event(&img.id, EventKind::Normal, "Retrying", format!("download {}", img.retry_seq));
                self.images.spawn_download(img.id.clone());
                Ok(Some(Duration::from_secs(10)))
            }
            ImageStatus::Failed { .. } => Ok(None),
            ImageStatus::Ready => {
                if self.cluster().is_some() && !self.images.image_file(&img).exists() && self.image_caches(&img.id).iter().any(|(n, r)| *n != self.local_node_id() && r.phase == crate::controller::image_cache::CachePhase::Ready) {
                    // Another node has the file: this leader fetches it only if it needs it.
                    return self.reconcile_image_cache(&img).await;
                }
                if !self.images.image_file(&img).exists() {
                    img.status = ImageStatus::Missing;
                    self.images.put_image(&img)?;
                    self.image_event(&img.id, EventKind::Warning, "FileMissing", "the image's file is gone; it is not downloaded again");
                    return Ok(None);
                }
                if self.cluster().is_some() {
                    self.reconcile_image_cache(&img).await?;
                }
                // Disks waiting for it.
                for d in self.images.list_disks() {
                    if d.phase == DiskPhase::Pending && matches!(&d.origin, crate::images::DiskOrigin::Image { image_id, .. } if *image_id == img.id) {
                        self.queue.add(Key::Disk(d.id.clone()));
                    }
                }
                Ok(None)
            }
            ImageStatus::Missing => {
                if self.images.image_file(&img).exists() {
                    img.status = ImageStatus::Ready;
                    self.images.put_image(&img)?;
                    self.image_event(&img.id, EventKind::Normal, "FileFound", "the image's file is back");
                }
                Ok(None)
            }
        }
    }

    /// `POST /images/{id}/retry`: download a failed image again (D19).
    pub fn retry_image(&self, key: &str) -> Result<crate::images::ImageResponse, VmManagerError> {
        let mut img = self.images.get_image(key)?;
        if img.deletion_requested_at.is_some() {
            return Err(crate::images::ImageError::InvalidImage(format!("image {} is being deleted", img.name)).into());
        }
        if matches!(img.status, ImageStatus::Failed { .. }) {
            img.retry_seq = img.retry_seq.max(img.applied_retry_seq) + 1;
            self.images.put_image(&img)?;
            let _ = self.store.push_event(&event_key("image", &img.id), Event::new("api", EventKind::Normal, "RetryRequested", ""));
            self.queue.add(Key::Image(img.id.clone()));
        } else if !matches!(img.status, ImageStatus::Downloading { .. } | ImageStatus::Verifying) {
            return Err(crate::images::ImageError::InvalidImage(format!("image {} has not failed ({:?})", img.name, img.status)).into());
        }
        Ok(self.images.image_response(&img, false))
    }
}
