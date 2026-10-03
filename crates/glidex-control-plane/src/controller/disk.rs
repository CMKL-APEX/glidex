//! The disk controller (spec/reconciliation.md §10.1): makes pending
//! disks once their image is ready, applies resizes and extend-root
//! requests while no instance has the disk open, notices missing files,
//! clears an on-boot root grow once the guest ran it, and finishes
//! deletions.

use crate::controller::queue::Key;
use crate::controller::vm::set_cond;
use crate::images::{Disk, DiskOrigin, DiskPhase, ImageError, ImageStatus};
use crate::models::Tristate;
use crate::state::{VmManager, VmManagerError};
use crate::store::{event_key, Commit, Event, EventKind};
use std::time::Duration;

type Next = Result<Option<Duration>, VmManagerError>;

impl VmManager {
    fn put_disk_with_event(&self, d: &crate::images::Disk, event: Option<Event>) -> Result<(), VmManagerError> {
        self.store.commit(Commit {
            put_disks: vec![d],
            events: event.into_iter().map(|e| (event_key("disk", &d.id), e)).collect(),
            ..Default::default()
        })?;
        self.images.cache_disk(d);
        Ok(())
    }

    /// One round for disk `id`.
    pub async fn reconcile_disk(&self, id: &str) -> Next {
        let Ok(disk) = self.images.get_disk(id) else { return Ok(None) };

        // Deletion: record first, then the file (images.md §6.5).
        if disk.deletion_requested_at.is_some() {
            if let Some(vm) = Self::disk_claimed_by(&*self.vms.read().await, &disk.id, None) {
                let mut d = disk;
                set_cond(&mut d.conditions, "Ready", Tristate::False, "Deleting", format!("waiting for VM {} to let go of it", vm));
                self.images.put_disk(&d)?;
                return Ok(Some(Duration::from_secs(5)));
            }
            if self.images.busy_op(&disk.id).is_some() {
                return Ok(Some(Duration::from_secs(1)));
            }
            self.store.commit(Commit { delete_disks: vec![disk.id.as_str()], ..Default::default() })?;
            let _ = self.store.delete_events(&event_key("disk", &disk.id));
            self.images.uncache_disk(&disk.id);
            self.images.remove_disk_file(&disk);
            tracing::info!(disk = %disk.name, "disk deleted");
            return Ok(None);
        }

        match disk.phase {
            DiskPhase::Pending | DiskPhase::Creating => self.create_round(disk).await,
            DiskPhase::Failed => Ok(None),
            DiskPhase::Ready | DiskPhase::Resizing | DiskPhase::Missing => self.ready_round(disk).await,
        }
    }

    /// Pending → Creating → Ready, once the source image is ready.
    async fn create_round(&self, mut disk: Disk) -> Next {
        if let DiskOrigin::Image { image_id, .. } = &disk.origin {
            let waiting = match self.images.get_image(image_id) {
                Ok(img) => match img.status {
                    ImageStatus::Ready => None,
                    ImageStatus::Downloading { .. } | ImageStatus::Verifying => Some(("ImageNotReady", format!("waiting for image {} to download", img.name), false)),
                    ImageStatus::Failed { reason } => Some(("ImageNotReady", format!("image {} failed: {}", img.name, reason), true)),
                    ImageStatus::Missing => Some(("ImageNotReady", format!("image {} is missing its file", img.name), true)),
                },
                Err(_) => Some(("ImageNotReady", format!("image {} is gone", image_id), true)),
            };
            if let Some((reason, message, stuck)) = waiting {
                set_cond(&mut disk.conditions, "Ready", Tristate::False, reason, message);
                disk.phase = DiskPhase::Pending;
                self.images.put_disk(&disk)?;
                return Ok(Some(Duration::from_secs(if stuck { 30 } else { 3 })));
            }
        }
        let guard = match self.images.begin(&disk.id, "create") {
            Ok(g) => g,
            Err(_) => return Ok(Some(Duration::from_secs(1))),
        };
        if disk.phase != DiskPhase::Creating {
            disk.phase = DiskPhase::Creating;
            set_cond(&mut disk.conditions, "Ready", Tristate::False, "Progressing", "creating");
            self.images.put_disk(&disk)?;
        }
        let (mgr, d) = (self.images.clone(), disk.clone());
        let result = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            mgr.materialize(&d)
        })
        .await
        .map_err(|e| ImageError::Io(e.to_string()))?;
        // Keep any change made meanwhile (a claim by a VM, a deletion).
        let latest = self.images.get_disk(&disk.id).unwrap_or(disk.clone());
        match result {
            Ok(out) => {
                let mut d = out.disk;
                d.attached_to = latest.attached_to.clone();
                d.deletion_requested_at = latest.deletion_requested_at;
                set_cond(&mut d.conditions, "Ready", Tristate::True, "Converged", "");
                let msg = if out.warnings.is_empty() { format!("{} bytes", d.size_bytes) } else { out.warnings.join("; ") };
                self.put_disk_with_event(&d, Some(Event::new("controller", EventKind::Normal, "Created", msg)))?;
                drop(out._hold);
                // VMs waiting for it.
                if let Some(vm) = &d.attached_to {
                    self.queue.add(Key::Vm(vm.clone()));
                }
                Ok(if d.deletion_requested_at.is_some() { Some(Duration::ZERO) } else { None })
            }
            Err(ImageError::NotReady(m)) => {
                let mut d = latest;
                d.phase = DiskPhase::Pending;
                set_cond(&mut d.conditions, "Ready", Tristate::False, "ImageNotReady", m);
                self.images.put_disk(&d)?;
                Ok(Some(Duration::from_secs(3)))
            }
            Err(e @ (ImageError::InvalidDisk { .. } | ImageError::InvalidImage(_))) => {
                let mut d = latest;
                d.phase = DiskPhase::Failed;
                set_cond(&mut d.conditions, "Ready", Tristate::False, "InvalidDisk", e.to_string());
                self.put_disk_with_event(&d, Some(Event::new("controller", EventKind::Warning, "CreateFailed", e.to_string())))?;
                Ok(None)
            }
            Err(e) => {
                let mut d = latest;
                let reason = if matches!(e, ImageError::ToolMissing { .. }) { "ToolUnavailable" } else { "IoError" };
                set_cond(&mut d.conditions, "Ready", Tristate::False, reason, e.to_string());
                self.put_disk_with_event(&d, Some(Event::new("controller", EventKind::Warning, reason, e.to_string())))?;
                Err(e.into())
            }
        }
    }

    /// A made disk: missing file, resize, extend-root, on-boot grow done.
    async fn ready_round(&self, mut disk: Disk) -> Next {
        let path = self.images.disk_path(&disk);
        if !path.exists() {
            if disk.phase != DiskPhase::Missing {
                disk.phase = DiskPhase::Missing;
                set_cond(&mut disk.conditions, "Ready", Tristate::False, "FileMissing", format!("{} is gone", path.display()));
                let ev = Event::new("controller", EventKind::Warning, "FileMissing", "the disk's file is gone");
                self.put_disk_with_event(&disk, Some(ev))?;
            }
            return Ok(None);
        }
        if disk.phase == DiskPhase::Missing {
            disk.phase = DiskPhase::Ready;
        }

        let (in_use, claimed) = {
            let vms = self.vms.read().await;
            (Self::disk_in_use(&vms, &disk.id), Self::disk_claimed_by(&vms, &disk.id, None))
        };
        // A claim outlives its VM's spec only as long as an instance has
        // the disk open (D11).
        if disk.attached_to.is_some() && claimed.is_none() {
            let vm = disk.attached_to.take();
            self.put_disk_with_event(&disk, Some(Event::new("controller", EventKind::Normal, "Released", format!("no longer claimed by VM {}", vm.unwrap_or_default()))))?;
        }
        let mut pending = Vec::new();

        // Resize (§10.1).
        if let Some(r) = disk.resize {
            if r.size_bytes == disk.size_bytes {
                disk.resize = None;
            } else if let Some(vm) = &in_use {
                pending.push(("ResizePending", format!("applied once VM {} stops", vm)));
            } else {
                let (d, guard) = match self.begin_disk_op(&disk.id, "resize").await {
                    Ok(x) => x,
                    Err(_) => return Ok(Some(Duration::from_secs(2))),
                };
                let mgr = self.images.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let _guard = guard;
                    mgr.resize_disk(&d.id, r.size_bytes, r.extend_root)
                })
                .await
                .map_err(|e| ImageError::Io(e.to_string()))?;
                disk = self.images.get_disk(&disk.id)?;
                match result {
                    Ok((_, outcome, warnings)) => {
                        disk.resize = None;
                        let msg = format!("now {} bytes{}{}", disk.size_bytes, outcome.map(|o| format!(", root {:?}", o)).unwrap_or_default(), if warnings.is_empty() { String::new() } else { format!("; {}", warnings.join("; ")) });
                        self.put_disk_with_event(&disk, Some(Event::new("controller", EventKind::Normal, "Resized", msg)))?;
                    }
                    Err(e @ ImageError::InvalidDisk { .. }) => {
                        // No longer valid (the guest grew a partition since):
                        // the spec stays until the user changes it.
                        pending.push(("ResizeInvalid", e.to_string()));
                    }
                    Err(e) => {
                        set_cond(&mut disk.conditions, "Ready", Tristate::False, "IoError", e.to_string());
                        self.images.put_disk(&disk)?;
                        return Err(e.into());
                    }
                }
            }
        }

        // Extend-root (D19).
        if let Some(e) = disk.extend_root.filter(|e| e.seq > disk.applied_extend_root_seq) {
            if let Some(vm) = &in_use {
                pending.push(("ExtendRootPending", format!("applied once VM {} stops", vm)));
            } else {
                let (d, guard) = match self.begin_disk_op(&disk.id, "extend-root").await {
                    Ok(x) => x,
                    Err(_) => return Ok(Some(Duration::from_secs(2))),
                };
                let mgr = self.images.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let _guard = guard;
                    mgr.extend_root(&d.id, e.mode)
                })
                .await
                .map_err(|e| ImageError::Io(e.to_string()))?;
                disk = self.images.get_disk(&disk.id)?;
                match result {
                    Ok((_, outcome, warnings)) => {
                        disk.applied_extend_root_seq = e.seq;
                        let msg = format!("{:?}{}", outcome, if warnings.is_empty() { String::new() } else { format!("; {}", warnings.join("; ")) });
                        self.put_disk_with_event(&disk, Some(Event::new("controller", EventKind::Normal, "RootExtended", msg)))?;
                    }
                    Err(err @ ImageError::InvalidDisk { .. }) => {
                        disk.applied_extend_root_seq = e.seq;
                        self.put_disk_with_event(&disk, Some(Event::new("controller", EventKind::Warning, "ExtendRootFailed", err.to_string())))?;
                    }
                    Err(err) => return Err(err.into()),
                }
            }
        }

        // The on-boot grow ran: the claiming VM booted a seed carrying it.
        if disk.pending_growpart {
            if let Some(vm_id) = &disk.attached_to {
                if let Some(vm) = self.vm(vm_id).await {
                    if vm.status.seed_growpart_seq.is_some_and(|s| s >= disk.applied_extend_root_seq) {
                        disk.pending_growpart = false;
                        self.put_disk_with_event(&disk, Some(Event::new("controller", EventKind::Normal, "RootGrownOnBoot", format!("by VM {}", vm.name))))?;
                    }
                }
            }
        }

        for kind in ["ResizePending", "ResizeInvalid", "ExtendRootPending"] {
            disk.conditions.retain(|c| c.reason != kind);
        }
        match pending.first() {
            Some((reason, msg)) => set_cond(&mut disk.conditions, "Ready", Tristate::False, reason, msg.clone()),
            None => set_cond(&mut disk.conditions, "Ready", Tristate::True, "Converged", ""),
        }
        disk.phase = DiskPhase::Ready;
        let stored = self.images.get_disk(&disk.id)?;
        if stored.conditions != disk.conditions || stored.phase != disk.phase || stored.resize != disk.resize {
            // Keep a claim or deletion written meanwhile.
            disk.attached_to = stored.attached_to;
            disk.deletion_requested_at = stored.deletion_requested_at;
            self.images.put_disk(&disk)?;
        }
        Ok(if pending.is_empty() { None } else { Some(Duration::from_secs(10)) })
    }
}
