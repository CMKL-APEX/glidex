//! Kernel extraction runs (spec/kernel-boot.md §10.2): the extractor
//! appliance boots as a one-shot job in a VM instance, so under the
//! systemd runner it gets the same unit and sandbox as a VM (the control
//! plane's own sandbox has no `/dev/kvm`). The image manager prepares the
//! appliance and reads the result (`images/extract.rs`).

use crate::hypervisor::{cloud_hypervisor, qemu, resolve_binary};
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
        self.images.spawn_task(
            id.to_string(),
            async move { me.run_extraction(&task_id).await },
        );
    }

    /// Extract kernel image `id` (§10): wait for its disk image, run the
    /// extractor on it, install what it chose.
    async fn run_extraction(self: Arc<Self>, id: &str) -> Result<(), ImageError> {
        let img = self.images.get_image(id)?;
        let ImageSource::Extracted { image_id, .. } = &img.source else {
            return Err(ImageError::InvalidImage(format!(
                "{} is not extracted",
                img.name
            )));
        };
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
        let disk = self.images.image_path(&src.id);
        if !disk.exists() {
            return Err(ImageError::NotReady(format!(
                "image {}'s file is not on this server",
                src.name
            )));
        }
        // Not deleted from under the run.
        let _hold = self.images.hold_image(&src.id);
        let _permit = self
            .images
            .extractions
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| ImageError::Io(e.to_string()))?;
        let (kernel, initrd) = self.images.ensure_extractor(src.arch).await?;
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

        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let appliance = (s(&kernel), s(&initrd));
        let image: (String, crate::images::qemu_img::ImageType) = (s(&disk), src.format.into());
        let (kind, argv) = if let Some(ch) = resolve_binary("cloud-hypervisor") {
            let a = (appliance.0.as_str(), appliance.1.as_str());
            let argv = cloud_hypervisor::extractor_args(&s(&ch), a, (&image.0, image.1), &s(&out));
            (HypervisorKind::CloudHypervisor, argv)
        } else if let Some(q) = resolve_binary("qemu-system-x86_64") {
            let a = (appliance.0.as_str(), appliance.1.as_str());
            let argv = qemu::extractor_args(&s(&q), a, (&image.0, image.1), &s(&out));
            (HypervisorKind::Qemu, argv)
        } else {
            return Err(ImageError::ToolMissing {
                tool: "cloud-hypervisor or qemu-system-x86_64".into(),
                package: "cloud-hypervisor or qemu-system-x86".into(),
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
        let (mgr, id_owned, src_c, o) = (
            self.images.clone(),
            id.to_string(),
            src.clone(),
            out.clone(),
        );
        let result =
            tokio::task::spawn_blocking(move || mgr.finish_extraction(&id_owned, &src_c, &o))
                .await
                .map_err(|e| ImageError::Io(e.to_string()))?;
        let _ = std::fs::remove_file(&out);
        let mut img = result?;
        img.status = ImageStatus::Ready;
        self.images.put_image(&img)?;
        let _ = self.store.push_event(
            &event_key("image", id),
            Event::new(
                "controller",
                EventKind::Normal,
                "Extracted",
                format!(
                    "{} from {} ({})",
                    img.kernel_release.as_deref().unwrap_or("kernel"),
                    src.name,
                    match &img.source {
                        ImageSource::Extracted { path, .. } => path.as_str(),
                        _ => "",
                    }
                ),
            ),
        );
        tracing::info!(image = %img.name, release = ?img.kernel_release, "kernel extracted");
        Ok(())
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
