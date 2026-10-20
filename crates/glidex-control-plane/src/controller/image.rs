//! The image controller (spec/reconciliation.md §10.2): supervises
//! downloads (resuming one whose task is gone), downloads a failed image
//! again once asked to (`retry_seq`), and notices a missing file. A ready
//! image whose file disappears is never downloaded again: catalog URLs
//! point at "current/latest", and a different file would sit under the
//! linked disks written against the old one.

use crate::controller::queue::Key;
use crate::images::{DiskPhase, ImageStatus};
use crate::state::{VmManager, VmManagerError};
use crate::store::{Event, EventKind, event_key};
use std::time::Duration;

type Next = Result<Option<Duration>, VmManagerError>;

impl VmManager {
    fn image_event(&self, id: &str, kind: EventKind, reason: &str, message: impl Into<String>) {
        let _ = self.store.push_event(
            &event_key("image", id),
            Event::new("controller", kind, reason, message),
        );
    }

    /// One round for image `id`.
    pub async fn reconcile_image(&self, id: &str) -> Next {
        let Ok(mut img) = self.images.get_image(id) else {
            return Ok(None);
        };
        let leads = self.store.database().can_write();
        if leads && self.cluster().is_some() {
            self.prune_departed_copies(&mut img)?;
        }
        if img.deletion_requested_at.is_some() {
            // Every node's copy goes first (§9.2); the record is the leader's to remove.
            if self.cluster().is_some() {
                self.drop_image_cache(&img)?;
                if !leads {
                    return Ok(None);
                }
                if self
                    .image_caches(&img.id)
                    .iter()
                    .any(|(n, _)| *n != self.local_node_id())
                {
                    return Ok(Some(Duration::from_secs(5)));
                }
            }
            // The `image.download` and `image.file` finalizers (§6.3).
            if self.images.finish_image_delete(&img.id)? {
                self.image_event(&img.id, EventKind::Normal, "Deleted", "");
            }
            return Ok(None);
        }
        // A node that doesn't write the catalog keeps only its copy.
        if !leads {
            return self.reconcile_image_cache(&img).await;
        }
        let extracted = matches!(img.source, crate::images::ImageSource::Extracted { .. });
        match img.status.clone() {
            // A kernel extraction (spec/kernel-boot.md §10), resumed if its
            // task is gone (a restart).
            ImageStatus::Extracting => {
                if !self.images.download_running(&img.id) {
                    self.spawn_extraction(&img.id);
                }
                Ok(Some(Duration::from_secs(10)))
            }
            ImageStatus::Downloading { .. } | ImageStatus::Verifying => {
                if !self.images.download_running(&img.id) {
                    tracing::info!(image = %img.name, "resuming image download");
                    self.images.spawn_download(img.id.clone());
                }
                Ok(Some(Duration::from_secs(10)))
            }
            ImageStatus::Failed { .. } if img.retry_seq > img.applied_retry_seq => {
                img.applied_retry_seq = img.retry_seq;
                img.status = if extracted {
                    ImageStatus::Extracting
                } else {
                    ImageStatus::Downloading {
                        received_bytes: 0,
                        total_bytes: None,
                    }
                };
                let _ = std::fs::remove_file(self.images.part_path(&img.id));
                let _ = std::fs::remove_file(self.images.initrd_part_path(&img.id));
                self.images.put_image(&img)?;
                self.image_event(
                    &img.id,
                    EventKind::Normal,
                    "Retrying",
                    format!(
                        "{} {}",
                        if extracted { "extraction" } else { "download" },
                        img.retry_seq
                    ),
                );
                if extracted {
                    self.spawn_extraction(&img.id);
                } else {
                    self.images.spawn_download(img.id.clone());
                }
                Ok(Some(Duration::from_secs(10)))
            }
            ImageStatus::Failed { .. } => Ok(None),
            ImageStatus::Ready => {
                // Copies on chosen nodes, copies let go (§9.2.2).
                let tending = if self.cluster().is_some() {
                    self.tend_image_copies(&mut img).await?
                } else {
                    false
                };
                let again =
                    |n: Next| -> Next { n.map(|d| d.or(tending.then(|| Duration::from_secs(10)))) };
                if self.cluster().is_some()
                    && !self.images.image_file(&img).exists()
                    && self.image_caches(&img.id).iter().any(|(n, r)| {
                        *n != self.local_node_id()
                            && r.phase == crate::controller::image_cache::CachePhase::Ready
                    })
                {
                    // Another node has the file: this leader fetches it only if it needs it.
                    return again(self.reconcile_image_cache(&img).await);
                }
                if !self.images.image_file(&img).exists() {
                    img.status = ImageStatus::Missing;
                    self.images.put_image(&img)?;
                    self.image_event(
                        &img.id,
                        EventKind::Warning,
                        "FileMissing",
                        "the image's file is gone; it is not downloaded again",
                    );
                    return Ok(None);
                }
                if self.cluster().is_some() {
                    self.reconcile_image_cache(&img).await?;
                }
                // Disks waiting for it.
                for d in self.images.list_disks() {
                    if d.phase == DiskPhase::Pending
                        && matches!(&d.origin, crate::images::DiskOrigin::Image { image_id, .. } if *image_id == img.id)
                    {
                        self.queue.add(Key::Disk(d.id.clone()));
                    }
                }
                again(Ok(None))
            }
            ImageStatus::Missing => {
                if self.images.image_file(&img).exists() {
                    img.status = ImageStatus::Ready;
                    self.images.put_image(&img)?;
                    self.image_event(
                        &img.id,
                        EventKind::Normal,
                        "FileFound",
                        "the image's file is back",
                    );
                } else if self.cluster().is_some()
                    && self.image_caches(&img.id).iter().any(|(n, r)| {
                        *n != self.local_node_id()
                            && r.phase == crate::controller::image_cache::CachePhase::Ready
                    })
                {
                    // Another node has it (§9.2.2).
                    img.status = ImageStatus::Ready;
                    self.images.put_image(&img)?;
                    self.image_event(
                        &img.id,
                        EventKind::Normal,
                        "FileFound",
                        "another node has a copy",
                    );
                    return Ok(Some(Duration::from_secs(1)));
                }
                Ok(None)
            }
        }
    }

    /// `POST /images/{id}/retry`: download a failed image again (D19).
    pub fn retry_image(&self, key: &str) -> Result<crate::images::ImageResponse, VmManagerError> {
        let mut img = self.images.get_image(key)?;
        if img.deletion_requested_at.is_some() {
            return Err(crate::images::ImageError::InvalidImage(format!(
                "image {} is being deleted",
                img.name
            ))
            .into());
        }
        if matches!(img.status, ImageStatus::Failed { .. }) {
            img.retry_seq = img.retry_seq.max(img.applied_retry_seq) + 1;
            self.images.put_image(&img)?;
            let _ = self.store.push_event(
                &event_key("image", &img.id),
                Event::new("api", EventKind::Normal, "RetryRequested", ""),
            );
            self.queue.add(Key::Image(img.id.clone()));
        } else if !matches!(
            img.status,
            ImageStatus::Downloading { .. } | ImageStatus::Verifying
        ) {
            return Err(crate::images::ImageError::InvalidImage(format!(
                "image {} has not failed ({:?})",
                img.name, img.status
            ))
            .into());
        }
        Ok(self.images.image_response(&img, false))
    }
}
