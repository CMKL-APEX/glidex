//! Kernel extraction runs (spec/kernel-boot.md §10.2): the extractor
//! appliance boots as a one-shot job in a VM instance, so under the
//! systemd runner it gets the same unit and sandbox as a VM (the control
//! plane's own sandbox has no `/dev/kvm`). The image manager prepares the
//! appliance and reads the result (`images/extract.rs`).

use crate::controller::image_cache::{CachePhase, CacheRow};
use crate::controller::queue::Key;
use crate::hypervisor::{cloud_hypervisor, qemu, resolve_binary};
use crate::images::Image;
use crate::images::catalog::Arch;
use crate::images::extract::{ExtractOrigin, ExtractedKernel};
use crate::images::extractor::OUTPUT_SIZE;
use crate::images::{ImageError, ImageSource, ImageStatus};
use crate::state::VmManager;
use crate::store::{Event, EventKind, event_key};
use glidex_vm_shim::client::ShimClient;
use glidex_vm_shim::launch::{HypervisorKind, LAUNCH_VERSION, LaunchFile};
use glidex_vm_shim::state::{ExitCause, InstanceFile, Phase};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::time::Duration;

type Next = Result<Option<Duration>, crate::state::VmManagerError>;

/// An extraction that takes longer is stopped (it takes seconds, F-K7).
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(120);

/// How a one-shot job ended.
pub(crate) struct JobOutcome {
    /// Exit status 0 on its own.
    pub clean: bool,
    /// Why not, when it wasn't.
    pub reason: String,
    /// The end of its console log.
    pub log_tail: String,
}

/// The VM-instance id of kernel image `id`'s extraction.
pub fn job_id(image_id: &str) -> String {
    format!("extract-{image_id}")
}

/// Whether a runtime directory or unit belongs to an extraction job.
pub fn is_job(instance: &str) -> bool {
    instance.starts_with("extract-")
}

impl VmManager {
    /// Start kernel image `id`'s extraction unless it is running.
    pub(crate) fn spawn_extraction(&self, id: &str) {
        let me = self.arc();
        let task_id = id.to_string();
        self.images.spawn_task(id.to_string(), async move {
            me.run_extraction(&task_id).await.map(|_| ())
        });
    }

    /// Extract kernel image `id` (§10) from what its source names: a disk
    /// image (waited for while it downloads) or a VM's disk.
    async fn run_extraction(self: Arc<Self>, id: &str) -> Result<String, ImageError> {
        let img = self.images.get_image(id)?;
        let ImageSource::Extracted {
            image_id, disk_id, ..
        } = &img.source
        else {
            return Err(ImageError::InvalidImage(format!(
                "{} is not extracted",
                img.name
            )));
        };
        if let Some(disk) = disk_id {
            // Asked for by id (`extract-kernel`): kept even if a twin exists.
            return self.extract_from_disk(id, disk, false).await;
        }
        // The disk image may still be downloading.
        let src = loop {
            let s = self.images.get_image(image_id).map_err(|_| {
                ImageError::NotReady("the disk image it comes from was deleted".into())
            })?;
            if s.deletion_requested_at.is_some() {
                return Err(ImageError::NotReady(format!(
                    "image {} is being deleted",
                    s.name
                )));
            }
            match &s.status {
                ImageStatus::Ready => break s,
                ImageStatus::Downloading { .. } | ImageStatus::Verifying => {
                    tokio::time::sleep(Duration::from_secs(2)).await
                }
                other => {
                    return Err(ImageError::NotReady(format!(
                        "image {} is {other:?}",
                        s.name
                    )));
                }
            }
        };
        // Not deleted from under the run.
        let _hold = self.images.hold_image(&src.id);
        let origin = ExtractOrigin {
            path: self.images.image_path(&src.id),
            format: src.format,
            backing_files: false,
            arch: src.arch,
            image_id: src.id.clone(),
            disk_id: None,
            sha256: src.sha256.clone(),
            verified: src.verified(),
        };
        self.extract_into(id, &origin, &src.name).await?;
        Ok(id.to_string())
    }

    /// Whether node `n` can extract kernels of `arch` for the leader
    /// (§10.2): ready, schedulable, of that architecture, with KVM.
    fn extracts_on(n: &crate::node::Node, arch: Arch) -> bool {
        !n.status.phase.is_tombstone()
            && n.status.phase.schedulable()
            && n.status.ready == crate::models::Tristate::True
            && n.status.features.kvm
            && n.status.features.arch == arch.as_str()
    }

    /// Why a kernel of `arch` can't be extracted, if it can't (§10.2): on
    /// this host, or in a cluster on another node of that architecture.
    pub(crate) fn extraction_unavailable(&self, arch: Arch) -> Option<String> {
        let here = self.images.extraction_refused(arch)?;
        if self.cluster().is_none() || crate::images::extractor::pin_for(arch).is_none() {
            return Some(here);
        }
        let me = self.local_node_id();
        if self
            .nodes()
            .list()
            .unwrap_or_default()
            .iter()
            .any(|n| n.meta.id != me && Self::extracts_on(n, arch))
        {
            return None;
        }
        Some(format!(
            "{here}, and no other ready node is {} with KVM",
            arch.as_str()
        ))
    }

    /// The leader's round for kernel image `img`, extracting (§10.2): run
    /// here when this host can and has the disk image, else on a node of
    /// its architecture with a copy, whose cache row says how it went.
    pub(crate) fn lead_extraction(&self, img: &mut Image) -> Next {
        if let Some(node) = img.extract_node.clone() {
            return self.follow_extraction(img, &node);
        }
        if self.images.download_running(&img.id) {
            return Ok(Some(Duration::from_secs(10)));
        }
        let src = match &img.source {
            ImageSource::Extracted {
                image_id,
                disk_id: None,
                ..
            } if self.cluster().is_some() => self.images.get_image(image_id).ok(),
            _ => None,
        };
        // A VM's disk, a standalone host, or a source gone (the run says so).
        let Some(src) = src else {
            self.spawn_extraction(&img.id);
            return Ok(Some(Duration::from_secs(10)));
        };
        let here = self.images.extraction_refused(src.arch).is_none();
        let downloading = matches!(
            src.status,
            ImageStatus::Downloading { .. } | ImageStatus::Verifying
        );
        if here && (downloading || self.images.image_path(&src.id).exists()) {
            self.spawn_extraction(&img.id);
            return Ok(Some(Duration::from_secs(10)));
        }
        if downloading {
            return Ok(Some(Duration::from_secs(5)));
        }
        if src.status != ImageStatus::Ready || src.deletion_requested_at.is_some() {
            // The run fails, saying why.
            self.spawn_extraction(&img.id);
            return Ok(Some(Duration::from_secs(10)));
        }
        // A node with a copy first; else one that gets a copy.
        let me = self.local_node_id();
        let holders: Vec<String> = self
            .image_caches(&src.id)
            .into_iter()
            .filter(|(_, r)| r.phase == CachePhase::Ready)
            .map(|(n, _)| n)
            .collect();
        let mut nodes: Vec<crate::node::Node> = self
            .nodes()
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| Self::extracts_on(n, src.arch) && (n.meta.id != me || here))
            .collect();
        nodes.sort_by_key(|n| (!holders.contains(&n.meta.id), n.meta.id != me));
        let Some(node) = nodes.first() else {
            let why = self
                .extraction_unavailable(src.arch)
                .unwrap_or_else(|| format!("no node can extract it from {}", src.name));
            img.status = ImageStatus::Failed {
                reason: format!("kernels can't be extracted: {why}"),
            };
            self.images.put_image(img)?;
            return Ok(None);
        };
        if !holders.contains(&node.meta.id) {
            let mut s = src.clone();
            s.release.retain(|r| *r != node.meta.id);
            if !s.pinned.contains(&node.meta.id) {
                s.pinned.push(node.meta.id.clone());
                self.images.put_image(&s)?;
                self.queue.add(Key::Image(s.id.clone()));
            }
        }
        if node.meta.id == me {
            // This leader, once its copy is here.
            return Ok(Some(Duration::from_secs(5)));
        }
        img.extract_node = Some(node.meta.id.clone());
        self.images.put_image(img)?;
        self.image_event(
            &img.id,
            EventKind::Normal,
            "Delegated",
            format!("extracting on {} from {}", node.spec.name, src.name),
        );
        Ok(Some(Duration::from_secs(5)))
    }

    /// Kernel image `img`, extracting on `node` (§10.2): recorded from the
    /// node's cache row once it is done; placed again if the node left.
    fn follow_extraction(&self, img: &mut Image, node: &str) -> Next {
        let name = match self.copy_node(node) {
            Ok(n) => n.spec.name,
            Err(e) => {
                img.extract_node = None;
                self.images.put_image(img)?;
                self.image_event(
                    &img.id,
                    EventKind::Warning,
                    "Delegated",
                    format!("the node extracting it is gone ({e}); choosing another"),
                );
                return Ok(Some(Duration::from_secs(1)));
            }
        };
        let row = self
            .image_caches(&img.id)
            .into_iter()
            .find(|(n, _)| n == node)
            .map(|(_, r)| r);
        match row {
            Some(CacheRow {
                phase: CachePhase::Ready,
                extracted: Some(k),
                ..
            }) => {
                k.apply(img);
                img.status = ImageStatus::Ready;
                self.images.put_image(img)?;
                let from = match &img.source {
                    ImageSource::Extracted { image_id, .. } => self
                        .images
                        .get_image(image_id)
                        .map(|s| s.name)
                        .unwrap_or_else(|_| image_id.clone()),
                    _ => String::new(),
                };
                self.extracted_event(img, &from, Some(&name));
                tracing::info!(image = %img.name, node = %name, release = ?img.kernel_release, "kernel extracted");
                Ok(Some(Duration::from_secs(1)))
            }
            Some(CacheRow {
                phase: CachePhase::Failed,
                error,
                ..
            }) => {
                tracing::warn!(image = %img.name, node = %name, "kernel extraction failed: {error}");
                img.status = ImageStatus::Failed {
                    reason: format!("on {name}: {error}"),
                };
                self.images.put_image(img)?;
                Ok(None)
            }
            _ => Ok(Some(Duration::from_secs(5))),
        }
    }

    /// A node's round for kernel image `img`, extracting: run here when the
    /// leader chose this node (§10.2), once the disk image's copy is here.
    pub(crate) fn extract_for_leader(&self, img: &Image) -> Next {
        if self.images.download_running(&img.id) {
            return Ok(Some(Duration::from_secs(10)));
        }
        let me = self.local_node_id();
        if self
            .image_caches(&img.id)
            .iter()
            .any(|(n, r)| *n == me && matches!(r.phase, CachePhase::Ready | CachePhase::Failed))
        {
            // Done: the leader records it.
            return Ok(None);
        }
        let have = match &img.source {
            ImageSource::Extracted { image_id, .. } => self.images.image_path(image_id).exists(),
            _ => false,
        };
        if have {
            self.spawn_delegated_extraction(&img.id);
            return Ok(Some(Duration::from_secs(10)));
        }
        Ok(Some(Duration::from_secs(5)))
    }

    /// Run kernel image `id`'s extraction for the leader. Its result goes in
    /// this node's cache row, never the catalog, which is the leader's.
    fn spawn_delegated_extraction(&self, id: &str) {
        let me = self.arc();
        let task_id = id.to_string();
        self.images.spawn_task(id.to_string(), async move {
            let result = me.run_delegated_extraction(&task_id).await;
            if me
                .images
                .get_image(&task_id)
                .is_ok_and(|i| i.deletion_requested_at.is_none())
            {
                let row = match result {
                    Ok((kernel, bytes)) => CacheRow {
                        phase: CachePhase::Ready,
                        bytes,
                        verified_at: crate::tenancy::now(),
                        error: String::new(),
                        extracted: Some(kernel),
                    },
                    Err(e) => {
                        tracing::warn!(image = %task_id, "kernel extraction failed: {e}");
                        CacheRow {
                            phase: CachePhase::Failed,
                            bytes: 0,
                            verified_at: crate::tenancy::now(),
                            error: e.to_string(),
                            extracted: None,
                        }
                    }
                };
                if let Err(e) = me.put_cache_row(&task_id, Some(row)) {
                    tracing::warn!(image = %task_id, "recording the extraction: {e}");
                }
            }
            Ok(())
        });
    }

    async fn run_delegated_extraction(
        &self,
        id: &str,
    ) -> Result<(ExtractedKernel, u64), ImageError> {
        let img = self.images.get_image(id)?;
        let ImageSource::Extracted {
            image_id,
            disk_id: None,
            ..
        } = &img.source
        else {
            return Err(ImageError::InvalidImage(format!(
                "{} is not extracted from a disk image",
                img.name
            )));
        };
        let src = self
            .images
            .get_image(image_id)
            .map_err(|_| ImageError::NotReady("the disk image it comes from was deleted".into()))?;
        let _hold = self.images.hold_image(&src.id);
        let origin = ExtractOrigin {
            path: self.images.image_path(&src.id),
            format: src.format,
            backing_files: false,
            arch: src.arch,
            image_id: src.id.clone(),
            disk_id: None,
            sha256: src.sha256.clone(),
            verified: src.verified(),
        };
        let done = self.extract_files(id, &origin).await;
        if done.is_err() {
            for p in [
                self.images.part_path(id),
                self.images.initrd_part_path(id),
                self.images.extract_out_path(id),
            ] {
                let _ = std::fs::remove_file(p);
            }
        }
        let done = done?;
        let kernel = ExtractedKernel::of(&done)
            .ok_or_else(|| ImageError::Io("the extraction recorded no kernel".into()))?;
        Ok((kernel, done.file_size_bytes))
    }

    /// Extract kernel image `id` from VM disk `disk_id` (§10.5): the disk
    /// is busy for the run, so its VM can't start meanwhile. With `dedupe`,
    /// a kernel already stored (same files) is used instead and `id`
    /// dropped (K12). Returns the kernel image's id.
    async fn extract_from_disk(
        &self,
        id: &str,
        disk_id: &str,
        dedupe: bool,
    ) -> Result<String, ImageError> {
        let img = self.images.get_image(id)?;
        let disk = self
            .images
            .get_disk(disk_id)
            .map_err(|_| ImageError::NotReady("the disk it comes from was deleted".into()))?;
        let path = self.images.disk_path(&disk);
        if !path.exists() {
            return Err(ImageError::NotReady(format!(
                "disk {}'s file is not on this server",
                disk.name
            )));
        }
        let _busy = self.images.begin(&disk.id, "extract-kernel")?;
        let origin = ExtractOrigin {
            path,
            format: disk.format,
            backing_files: disk.is_linked(),
            arch: img.arch,
            image_id: String::new(),
            disk_id: Some(disk.id.clone()),
            sha256: String::new(),
            verified: false,
        };
        let done = self.extract_into(id, &origin, &disk.name).await?;
        if let Some(same) = self.images.same_kernel(&done).filter(|_| dedupe) {
            self.images.discard_kernel(id)?;
            tracing::info!(kernel = %same.name, "the extracted kernel is already stored");
            return Ok(same.id);
        }
        Ok(id.to_string())
    }

    /// Run the extractor on `origin` and install the result as kernel
    /// image `id`, `Ready` (§10.2–10.4). `from` names the source in events.
    async fn extract_into(
        &self,
        id: &str,
        origin: &ExtractOrigin,
        from: &str,
    ) -> Result<crate::images::Image, ImageError> {
        let mut img = self.extract_files(id, origin).await?;
        img.status = ImageStatus::Ready;
        self.images.put_image(&img)?;
        self.extracted_event(&img, from, None);
        tracing::info!(image = %img.name, release = ?img.kernel_release, "kernel extracted");
        Ok(img)
    }

    /// The `Extracted` event of kernel image `img`, from `from`, on `node`
    /// when another node extracted it.
    pub(crate) fn extracted_event(
        &self,
        img: &crate::images::Image,
        from: &str,
        node: Option<&str>,
    ) {
        let _ = self.store.push_event(
            &event_key("image", &img.id),
            Event::new(
                "controller",
                EventKind::Normal,
                "Extracted",
                format!(
                    "{} from {} ({}){}",
                    img.kernel_release.as_deref().unwrap_or("kernel"),
                    from,
                    match &img.source {
                        ImageSource::Extracted { path, .. } => path.as_str(),
                        _ => "",
                    },
                    node.map(|n| format!(" on {n}")).unwrap_or_default()
                ),
            ),
        );
    }

    /// Run the extractor on `origin` and install its kernel as kernel image
    /// `id`'s files (§10.2–10.4): the image with what was found, not yet
    /// recorded. Runs here, on the leader or on the node it chose.
    async fn extract_files(
        &self,
        id: &str,
        origin: &ExtractOrigin,
    ) -> Result<crate::images::Image, ImageError> {
        let _permit = self
            .images
            .extractions
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| ImageError::Io(e.to_string()))?;
        let out = self.images.extract_out_path(id);
        let o = out.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&o)?
                .set_len(OUTPUT_SIZE)
        })
        .await
        .map_err(|e| ImageError::Io(e.to_string()))?
        .map_err(|e| ImageError::Io(format!("{}: {e}", out.display())))?;

        let job = self.images.extractor_job.read().unwrap().clone();
        if let Some(job) = job {
            let (disk, o) = (origin.path.clone(), out.clone());
            tokio::task::spawn_blocking(move || job(&disk, &o))
                .await
                .map_err(|e| ImageError::Io(e.to_string()))?
                .map_err(|e| {
                    let _ = std::fs::remove_file(&out);
                    ImageError::InvalidImage(format!("the extractor failed: {e}"))
                })?;
        } else {
            let (kernel, initrd) = self.images.ensure_extractor(origin.arch).await?;
            let s = |p: &std::path::Path| p.to_string_lossy().into_owned();
            let appliance = (s(&kernel), s(&initrd));
            let a = (appliance.0.as_str(), appliance.1.as_str());
            let disk = s(&origin.path);
            let source = (disk.as_str(), origin.format.into(), origin.backing_files);
            let cmdline = crate::images::extractor::cmdline(origin.arch);
            // glidex runs QEMU on x86_64 only.
            let x86 = origin.arch == crate::images::catalog::Arch::X86_64;
            let (kind, argv) = if let Some(ch) = resolve_binary("cloud-hypervisor") {
                let argv = cloud_hypervisor::extractor_args(&s(&ch), a, &cmdline, source, &s(&out));
                (HypervisorKind::CloudHypervisor, argv)
            } else if x86 && let Some(q) = resolve_binary("qemu-system-x86_64") {
                let argv = qemu::extractor_args(&s(&q), a, &cmdline, source, &s(&out));
                (HypervisorKind::Qemu, argv)
            } else if x86 {
                return Err(ImageError::ToolMissing {
                    tool: "cloud-hypervisor or qemu-system-x86_64".into(),
                    package: "cloud-hypervisor or qemu-system-x86".into(),
                });
            } else {
                return Err(ImageError::ToolMissing {
                    tool: "cloud-hypervisor".into(),
                    package: "cloud-hypervisor".into(),
                });
            };
            let outcome = self
                .run_job(&job_id(id), kind, argv, EXTRACT_TIMEOUT)
                .await
                .map_err(|e| ImageError::Io(format!("the extractor did not start: {e}")))?;
            if !outcome.clean {
                let _ = std::fs::remove_file(&out);
                return Err(ImageError::InvalidImage(format!(
                    "the extractor {}; its last output:\n{}",
                    outcome.reason, outcome.log_tail
                )));
            }
        }
        let (mgr, id_owned, origin_c, o) = (
            self.images.clone(),
            id.to_string(),
            origin.clone(),
            out.clone(),
        );
        let result =
            tokio::task::spawn_blocking(move || mgr.finish_extraction(&id_owned, &origin_c, &o))
                .await
                .map_err(|e| ImageError::Io(e.to_string()))?;
        let _ = std::fs::remove_file(&out);
        result
    }

    /// `POST /vms/{id}/extract-kernel` (§10.5): extract the kernel on VM
    /// `vm_id`'s root disk now, as a kernel image the VM owns. The VM must
    /// be stopped, boot a kernel image and have a managed root disk; in a
    /// cluster, the disk's node runs it. Returns the kernel image, still
    /// extracting (`wait` for it with the image API).
    pub async fn extract_vm_kernel(
        &self,
        vm_id: &str,
    ) -> Result<crate::images::Image, crate::state::VmManagerError> {
        let bad = |m: &str| -> crate::state::VmManagerError {
            crate::hypervisor::HypervisorError::InvalidConfig(m.to_string()).into()
        };
        let vm = self.get_vm(vm_id).await?;
        let c = vm.config();
        if c.kernel_image.is_none() {
            return Err(bad("extract-kernel is for a VM that boots a kernel image"));
        }
        let Some(disk_id) = c.root_disk.clone() else {
            return Err(bad("extract-kernel needs a managed root disk"));
        };
        if vm.spec.power != crate::models::PowerState::Stopped || vm.status.instance.is_some() {
            return Err(ImageError::InUse(
                "stop the VM first: its disk is read while nothing writes it".into(),
            )
            .into());
        }
        let disk = self.images.get_disk(&disk_id)?;
        // On a node-local datastore, the disk's node reads it; on a shared
        // one, any node that reaches the datastore (spec/storage.md §5).
        if disk
            .node
            .as_deref()
            .is_some_and(|n| n != self.local_node_id())
            || self.images.driver(&disk).is_err()
        {
            return Err(ImageError::InUse(format!(
                "disk {} is not reachable from this node: ask a server on a node that has it",
                disk.name
            ))
            .into());
        }
        if disk.moving.is_some() {
            return Err(ImageError::Busy(format!(
                "disk {} is being moved to another datastore",
                disk.name
            ))
            .into());
        }
        let arch = self
            .vm_arch(c, std::slice::from_ref(&disk))
            .unwrap_or(crate::images::catalog::Arch::X86_64);
        let img = self.images.new_disk_kernel(&vm.id, &vm.name, &disk, arch)?;
        self.spawn_extraction(&img.id);
        Ok(img)
    }

    /// Before a launch of a `kernel_from_disk` VM (K11, §10.5): extract the
    /// kernel from its root disk when the disk changed since the last time,
    /// and record what it boots in `st.disk_kernel`. A failure keeps the
    /// kernel it has (condition `KernelExtracted=False`); `Err` only when
    /// there is none to boot.
    pub(crate) async fn follow_disk_kernel(
        &self,
        vm: &crate::models::Vm,
        st: &mut crate::models::VmStatus,
    ) -> Result<(), crate::state::VmManagerError> {
        use crate::controller::vm::{clear_cond, set_cond};
        use crate::models::{DiskKernel, Tristate};
        let c = vm.config();
        if !c.kernel_from_disk {
            st.disk_kernel = None;
            return Ok(());
        }
        let Some(disk_id) = c.root_disk.clone() else {
            return Ok(());
        };
        let disk = self.images.get_disk(&disk_id)?;
        // Not made yet, busy, or moving (spec/storage.md §8.3): the disk
        // check that follows holds the launch; extract once it is ready.
        if disk.phase != crate::images::DiskPhase::Ready
            || self.images.busy_op(&disk.id).is_some()
            || disk.moving.is_some()
        {
            return Ok(());
        }
        let stamp = std::fs::metadata(self.images.disk_path(&disk))
            .map(|m| {
                use std::os::unix::fs::MetadataExt;
                (m.len(), m.mtime() as u64)
            })
            .unwrap_or_default();
        if st.disk_kernel.as_ref().is_some_and(|k| {
            k.disk_stamp == stamp && self.images.get_image(&k.image).is_ok_and(|i| i.usable())
        }) {
            return Ok(());
        }
        let current = st
            .disk_kernel
            .as_ref()
            .map(|k| k.image.clone())
            .or_else(|| c.kernel_image.clone());
        let arch = self
            .vm_arch(c, std::slice::from_ref(&disk))
            .unwrap_or(crate::images::catalog::Arch::X86_64);
        let result = async {
            let img = self.images.new_disk_kernel(&vm.id, &vm.name, &disk, arch)?;
            let id = img.id.clone();
            let r = self.extract_from_disk(&id, &disk.id, true).await;
            if r.is_err() {
                let _ = self.images.discard_kernel(&id);
            }
            r
        }
        .await;
        match result {
            Ok(kernel) => {
                clear_cond(&mut st.conditions, "KernelExtracted");
                if current.as_deref() != Some(kernel.as_str()) {
                    let name = |id: &str| {
                        self.images
                            .get_image(id)
                            .ok()
                            .and_then(|i| i.kernel_release.or(Some(i.name)))
                            .unwrap_or_else(|| id.to_string())
                    };
                    let _ = self.store.push_event(
                        &event_key("vm", &vm.id),
                        Event::new(
                            "controller",
                            EventKind::Normal,
                            "KernelUpdated",
                            format!(
                                "{} -> {} (from the root disk)",
                                current.as_deref().map(name).unwrap_or_default(),
                                name(&kernel)
                            ),
                        ),
                    );
                }
                let previous = match &st.disk_kernel {
                    Some(k) if k.image == kernel => k.previous.clone(),
                    _ if current.as_deref() == Some(kernel.as_str()) => None,
                    _ => current,
                };
                st.disk_kernel = Some(DiskKernel {
                    image: kernel,
                    previous,
                    disk_stamp: stamp,
                });
                Ok(())
            }
            Err(e) => {
                set_cond(
                    &mut st.conditions,
                    "KernelExtracted",
                    Tristate::False,
                    "ExtractFailed",
                    e.to_string(),
                );
                let _ = self.store.push_event(
                    &event_key("vm", &vm.id),
                    Event::new(
                        "controller",
                        EventKind::Warning,
                        "KernelExtractFailed",
                        e.to_string(),
                    ),
                );
                if current.is_some() {
                    Ok(())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Run `argv` once in VM instance `id` (a `glidex-vm@` unit under the
    /// systemd runner) and wait for it to end, at most `timeout`; then
    /// release the shim and remove the instance's directory.
    pub(crate) async fn run_job(
        &self,
        id: &str,
        hypervisor: HypervisorKind,
        argv: Vec<String>,
        timeout: Duration,
    ) -> Result<JobOutcome, String> {
        let paths = crate::paths::vm_paths(id);
        let _ = std::fs::remove_dir_all(&paths.dir);
        crate::paths::ensure_vm_dir(id).map_err(|e| e.to_string())?;
        let instance_id = uuid::Uuid::new_v4().to_string();
        LaunchFile {
            version: LAUNCH_VERSION,
            vm_id: id.to_string(),
            instance_id: instance_id.clone(),
            hypervisor,
            argv,
            fallback: None,
            api_socket: paths.api_socket.clone(),
            console_socket: paths.console_socket.clone(),
            shim_socket: paths.shim_socket.clone(),
            log_path: paths.log.clone(),
            log_max_bytes: 1 << 20,
            ready_timeout_secs: 5,
            host_shutdown_grace_secs: 0,
            spec: serde_json::Value::Null,
            meter_poll_secs: 0,
            cpu_affinity: Vec::new(),
            vcpu_pins: Vec::new(),
            job: true,
            volume_locks: Vec::new(),
        }
        .write(&paths.launch)
        .map_err(|e| e.to_string())?;
        let runner = self.runner();
        runner.start(id, &paths).await.map_err(|e| e.to_string())?;

        let deadline = tokio::time::Instant::now() + timeout;
        let shim = std::path::PathBuf::from(&paths.shim_socket);
        let exit = loop {
            match InstanceFile::read(&paths.instance) {
                Ok(Some(f)) if f.instance_id == instance_id && f.phase == Phase::Exited => {
                    break f.exit;
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    let s = shim.clone();
                    let _ =
                        tokio::task::spawn_blocking(move || ShimClient::connect(&s)?.kill()).await;
                    break None;
                }
                _ => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        };
        let log = std::fs::read(&paths.log).unwrap_or_default();
        let log = String::from_utf8_lossy(&log[log.len().saturating_sub(4096)..]).into_owned();
        // The shim keeps the console until released; then the unit ends.
        let s = shim.clone();
        let _ = tokio::task::spawn_blocking(move || ShimClient::connect(&s)?.release()).await;
        let gone = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < gone {
            match runner.unit_active(id).await {
                Some(Ok(true)) => tokio::time::sleep(Duration::from_millis(200)).await,
                _ => break,
            }
        }
        let _ = std::fs::remove_dir_all(&paths.dir);
        Ok(match exit {
            Some(e) if e.cause == ExitCause::CleanExit => JobOutcome {
                clean: true,
                reason: String::new(),
                log_tail: log,
            },
            Some(e) => JobOutcome {
                clean: false,
                reason: format!(
                    "ended {}{}",
                    e.cause.as_str(),
                    e.message.map(|m| format!(": {m}")).unwrap_or_default()
                ),
                log_tail: log,
            },
            None => JobOutcome {
                clean: false,
                reason: format!("did not finish in {} s", timeout.as_secs()),
                log_tail: log,
            },
        })
    }
}
