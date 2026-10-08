//! The VM controller (spec/reconciliation.md §9.1).
//!
//! Each round observes the VM's instance (liveness §8.7, `instance.json`,
//! the hypervisor), records what it saw, and takes the next step toward
//! the spec: finish an exit, finish a deletion, stop, launch, or correct
//! drift. Every step is idempotent and safe to repeat after a crash.

use crate::cloud_init;
use crate::controller::queue::{backoff, Key};
use crate::hypervisor::{driver, vfio_device_id, HypervisorError, HypervisorType};
use crate::images::ImageError;
use crate::instance::{liveness, Liveness, Seen};
use crate::models::{
    Condition, ExitCause, ExitRecord, InstanceRef, NicBinding, NicStatus, PowerState, RestartPolicy, Tristate, Vm,
    VmPhase, VmSpec, VmStatus,
};
use crate::network::NetError;
use crate::state::{power_name, VmManager, VmManagerError};
use crate::store::{event_key, Commit, Event, EventKind};
use glidex_hv_client::GuestState;
use glidex_netd::proto::{AttachResult, Op};
use glidex_ovs::vm_port::VmPortSpec;
use glidex_vm_shim::client::ShimClient;
use glidex_vm_shim::state::{InstanceFile, Phase};
use glidex_vm_shim::LaunchFile;
use std::time::Duration;

/// Finalizers of a VM (§6.3), in the order they are worked off.
pub const FINALIZER_INSTANCE: &str = "vm.instance";
pub const FINALIZER_PORTS: &str = "vm.ports";
pub const FINALIZER_DISKS: &str = "vm.disks";
pub const FINALIZER_OWNED_DISK: &str = "vm.owned-disk";
pub const FINALIZER_RUNTIME: &str = "vm.runtime";
pub const FINALIZERS: &[&str] = &[FINALIZER_INSTANCE, FINALIZER_PORTS, FINALIZER_DISKS, FINALIZER_OWNED_DISK, FINALIZER_RUNTIME];

/// After this long running, a crash restart counts as the first again.
const STABLE_SECS: u64 = 600;
/// Crash restarts: 10 s × 2ⁿ, capped at 300 s (§7.5).
const CRASH_BACKOFF_BASE: u64 = 10;
const CRASH_BACKOFF_MAX: u64 = 300;
/// Slack after a stop's grace before the controller escalates (§9.1).
const STOP_SLACK_SECS: u64 = 15;

/// Set (or update) a condition; `last_transition_at` moves only when its
/// status changes.
pub(crate) fn set_cond(conds: &mut Vec<Condition>, kind: &str, status: Tristate, reason: &str, message: impl Into<String>) {
    let message = message.into();
    let now = crate::tenancy::now();
    match conds.iter_mut().find(|c| c.kind == kind) {
        Some(c) => {
            if c.status != status {
                c.last_transition_at = now;
            }
            c.status = status;
            c.reason = reason.to_string();
            c.message = message;
        }
        None => conds.push(Condition { kind: kind.to_string(), status, reason: reason.to_string(), message, last_transition_at: now }),
    }
}

fn clear_cond(conds: &mut Vec<Condition>, kind: &str) {
    conds.retain(|c| c.kind != kind);
}

fn crash_delay(restarts: u32) -> u64 {
    let n = restarts.saturating_sub(1).min(16);
    (CRASH_BACKOFF_BASE << n).min(CRASH_BACKOFF_MAX)
}

/// The spec fields that need a new launch to take effect (§7.3).
fn launch_relevant(spec: &VmSpec) -> serde_json::Value {
    let c = &spec.config;
    serde_json::json!({
        "vcpu_count": c.vcpu_count, "mem_size_mib": c.mem_size_mib, "kernel_args": c.kernel_args,
        "credential": c.credential, "hugepages": c.hugepages, "data_disks": c.data_disks, "networks": c.networks,
    })
}

/// `launch.json`'s `spec`: the generation and spec it was built from.
fn launched_spec(paths: &crate::paths::VmPaths) -> Option<VmSpec> {
    let f = LaunchFile::read(&paths.launch).ok()?;
    serde_json::from_value(f.spec.get("spec")?.clone()).ok()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, VmManagerError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| VmManagerError::PersistenceError(format!("task failed: {}", e)))
}

/// One call on the VM's `shim.sock`.
async fn shim_call<T: Send + 'static>(
    sock: String,
    f: impl FnOnce(&mut ShimClient) -> Result<T, glidex_vm_shim::client::ShimError> + Send + 'static,
) -> Result<T, String> {
    blocking(move || {
        let mut c = ShimClient::connect(std::path::Path::new(&sock)).map_err(|e| e.to_string())?;
        f(&mut c).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Queue pairs for a NIC: the user's choice, else one per vCPU (up to 4)
/// for vhost-user, where a single queue pins the NIC to one OVS PMD, else 1.
fn nic_queue_pairs(requested: Option<u8>, kind: glidex_ovs::vm_port::VmPortKind, vcpus: u8) -> u8 {
    requested.unwrap_or(match kind {
        glidex_ovs::vm_port::VmPortKind::VhostUser => glidex_ovs::tuning::default_vhost_queue_pairs(vcpus as u32),
        glidex_ovs::vm_port::VmPortKind::Tap => 1,
    })
}

/// How stale `status.last_reconciled_at` may get before an otherwise
/// unchanged status is written to refresh it.
const RECONCILED_STAMP_SECS: u64 = 600;

/// Outcome of a round: requeue after this long, or wait for an event.
type Next = Result<Option<Duration>, VmManagerError>;

/// A failed launch, and whether the intent record was already written
/// (after it, the next round's observation handles the failure).
struct LaunchFailure {
    error: Box<VmManagerError>,
    intent: bool,
}

fn before_intent(e: impl Into<VmManagerError>) -> LaunchFailure {
    LaunchFailure { error: Box::new(e.into()), intent: false }
}

impl VmManager {
    /// Replace `status` (never `spec`) of VM `id`, as observed under
    /// `read_gen`. Returns the stored VM, `None` if it is gone.
    pub(crate) async fn write_status(&self, id: &str, read_gen: u64, mut status: VmStatus, events: Vec<Event>) -> Result<Option<Vm>, VmManagerError> {
        let mut vms = self.vms.write().await;
        let Some(cur) = vms.get(id).cloned() else { return Ok(None) };
        // spec/clustering.md §4.1: a round that observes nothing new writes
        // nothing, so `last_reconciled_at` is left out of the comparison and
        // stamped only with another change, or at most every 10 minutes.
        let now = crate::tenancy::now();
        status.observed_generation = read_gen;
        status.last_reconciled_at = cur.status.last_reconciled_at;
        if status.phase != cur.status.phase {
            status.phase_since = Some(now);
        }
        if cur.status == status && events.is_empty() && now.saturating_sub(cur.status.last_reconciled_at) < RECONCILED_STAMP_SECS {
            return Ok(Some(cur));
        }
        status.last_reconciled_at = now;
        let mut vm = cur;
        vm.status = status;
        vm.resource_version += 1;
        self.put_locked(&mut vms, vm, events).map(Some)
    }

    /// D12: `spec.power = Stopped` after an exit nobody asked for, unless
    /// the user changed the spec since it was observed.
    async fn stop_spec_after_exit(&self, id: &str, observed_gen: u64, actor: &str, reason: &str, message: String) -> Result<(), VmManagerError> {
        let mut vms = self.vms.write().await;
        let Some(cur) = vms.get(id).cloned() else { return Ok(()) };
        if cur.generation != observed_gen || cur.spec.power == PowerState::Stopped {
            return Ok(());
        }
        let mut vm = cur;
        vm.spec.power = PowerState::Stopped;
        vm.generation += 1;
        vm.resource_version += 1;
        let ev = Event::new(actor, EventKind::Normal, reason, message.clone());
        let (project, target) = (vm.project.clone(), format!("vm:{}", vm.id));
        self.put_locked(&mut vms, vm, vec![ev])?;
        crate::store::audit_system(
            &self.store.database(),
            "vm-controller",
            "stopVm",
            &project,
            &target,
            serde_json::json!({ "cause": actor, "reason": reason, "message": message }),
        );
        Ok(())
    }

    /// This VM's private copy of the UEFI variable store (QEMU firmware
    /// boot), so boot entries the guest writes survive a restart.
    pub(crate) fn firmware_vars_path(&self, vm_id: &str) -> String {
        self.data_dir.join("firmware-vars").join(format!("{}.fd", vm_id)).to_string_lossy().into_owned()
    }

    async fn observe(&self, vm: &Vm) -> Result<Seen, VmManagerError> {
        let unit = self.runner().unit_active(&vm.id).await;
        let v = vm.clone();
        blocking(move || liveness(&v, unit)).await
    }

    /// One round for VM `id` (§9.1).
    pub async fn reconcile_vm(&self, id: &str) -> Next {
        let Some(vm) = self.vm(id).await else { return Ok(None) };
        let gen = vm.generation;
        let mut st = vm.status.clone();
        let mut events: Vec<Event> = Vec::new();
        let seen = self.observe(&vm).await?;
        let paths = vm.paths();

        // Fill in what the shim reported about our instance.
        if let (Some(inst), Some(f)) = (st.instance.as_mut(), seen.file.as_ref()) {
            if f.instance_id == inst.instance_id {
                inst.shim_pid = Some(f.shim_pid);
                inst.shim_starttime = Some(f.shim_starttime);
                inst.hypervisor_pid = f.hypervisor_pid.or(inst.hypervisor_pid);
                inst.hypervisor_starttime = f.hypervisor_starttime.or(inst.hypervisor_starttime);
            }
        }

        match &seen.liveness {
            Liveness::Unknown(why) => {
                st.phase = VmPhase::Unknown;
                set_cond(&mut st.conditions, "HypervisorReachable", Tristate::Unknown, "CannotVerify", why.clone());
                set_cond(&mut st.conditions, "Ready", Tristate::Unknown, "CannotVerify", "the instance's state cannot be established");
                self.write_status(id, gen, st, events).await?;
                return Ok(Some(Duration::from_secs(10)));
            }
            Liveness::Live => {
                let recorded = st.instance.as_ref().map(|i| i.instance_id.clone());
                match &seen.file {
                    Some(f) if recorded.as_deref() != Some(f.instance_id.as_str()) => {
                        // A live instance we have no (matching) record of: by
                        // D8 it is the only one this VM can have; adopt it.
                        st.instance = Some(self.adopted_ref(&vm, f));
                        events.push(Event::new("controller", EventKind::Warning, "AdoptedUnrecorded", format!("adopted instance {}", f.instance_id)));
                    }
                    None if recorded.is_none() => {
                        st.phase = VmPhase::Unknown;
                        set_cond(&mut st.conditions, "HypervisorReachable", Tristate::Unknown, "LegacyOrphan",
                            "a hypervisor answers on this VM's socket but no shim reports it; stop that process");
                        set_cond(&mut st.conditions, "Ready", Tristate::Unknown, "LegacyOrphan", "an unmanaged hypervisor holds this VM");
                        self.write_status(id, gen, st, events).await?;
                        return Ok(Some(Duration::from_secs(30)));
                    }
                    _ => {}
                }
            }
            Liveness::Dead => {}
        }
        clear_cond(&mut st.conditions, "HypervisorReachable");
        let live = seen.liveness == Liveness::Live;
        let file = seen.file.clone().filter(|f| st.instance.as_ref().is_some_and(|i| i.instance_id == f.instance_id));

        // ---- 3. exit ------------------------------------------------------
        if let Some(inst) = st.instance.clone() {
            let exited = file.as_ref().is_some_and(|f| f.phase == Phase::Exited);
            if live && exited {
                // The shim keeps the console until released.
                let _ = shim_call(paths.shim_socket.clone(), |c| c.release()).await;
                st.phase = VmPhase::Stopping;
                self.write_status(id, gen, st, events).await?;
                return Ok(Some(Duration::from_millis(300)));
            }
            if !live {
                return self.finish_exit(&vm, st, inst, file, &seen.boot_id, events).await;
            }
        }

        // ---- 2. deletion -----------------------------------------------------
        let deleting = vm.deletion_requested_at.is_some();
        if deleting && !live {
            self.finalize_deletion(&vm).await?;
            return Ok(None);
        }

        let desired = if deleting { PowerState::Stopped } else { vm.spec.power };
        if desired == PowerState::Stopped {
            if live {
                return self.stop_round(&vm, st, events).await;
            }
            // ---- 4. stopped: release the ports ------------------------------
            if !st.nics.is_empty() {
                self.detach_ports(&vm.id, &mut st, None).await?;
            }
            st.phase = VmPhase::Stopped;
            st.stop_deadline = None;
            st.next_restart_at = None;
            clear_cond(&mut st.conditions, "CrashLoopBackOff");
            clear_cond(&mut st.conditions, "RestartRequired");
            clear_cond(&mut st.conditions, "DevicesPending");
            set_cond(&mut st.conditions, "ConsoleReady", Tristate::False, "NotRunning", "");
            set_cond(&mut st.conditions, "Ready", Tristate::True, "Converged", "");
            self.write_status(id, gen, st, events).await?;
            return Ok(None);
        }

        st.stop_deadline = None;
        if !live {
            return self.launch_round(&vm, st, events).await;
        }
        self.running_round(&vm, st, file, events).await
    }

    /// An `InstanceRef` for a live instance found without a record.
    pub(crate) fn adopted_ref(&self, vm: &Vm, f: &InstanceFile) -> InstanceRef {
        let paths = vm.paths();
        let launched = LaunchFile::read(&paths.launch).ok();
        let launched_generation = launched
            .as_ref()
            .and_then(|l| l.spec.get("generation").and_then(|g| g.as_u64()))
            .unwrap_or(vm.generation);
        let spec = launched_spec(&paths);
        let c = spec.as_ref().map(|s| &s.config).unwrap_or(vm.config());
        InstanceRef {
            instance_id: f.instance_id.clone(),
            runner: self.runner().record(&vm.id),
            boot_id: f.boot_id.clone(),
            launched_generation,
            disks: c.root_disk.iter().chain(c.data_disks.iter()).cloned().collect(),
            vfio_devices: c.vfio_devices.clone(),
            shim_pid: Some(f.shim_pid),
            shim_starttime: Some(f.shim_starttime),
            hypervisor_pid: f.hypervisor_pid,
            hypervisor_starttime: f.hypervisor_starttime,
            launched_at: f.launched_at,
            growpart_disk: None,
            growpart_seq: None,
        }
    }

    /// §7.5: the instance is gone; record why and act on it.
    async fn finish_exit(&self, vm: &Vm, mut st: VmStatus, inst: InstanceRef, file: Option<InstanceFile>, boot: &str, mut events: Vec<Event>) -> Next {
        let now = crate::tenancy::now();
        // When it really exited (the shim's record), not when we noticed:
        // metering ends allocation there (spec/metering.md §6.4).
        let mut exited_at = now;
        let mut usage = None;
        let (cause, code, signal, message) = if inst.boot_id != boot {
            (ExitCause::HostReboot, None, None, None)
        } else if let Some(e) = file.as_ref().and_then(|f| f.exit.clone()) {
            exited_at = e.at.min(now);
            usage = e.usage;
            (e.cause, e.code, e.signal, e.message)
        } else if file.is_none() {
            // An intent whose launch never happened.
            (ExitCause::Lost, None, None, None)
        } else {
            // The shim died without recording an exit.
            (ExitCause::Lost, None, None, Some("the shim died without recording how the hypervisor ended".into()))
        };
        let shim_died = cause == ExitCause::Lost && file.is_some();
        let disk_ids = match &usage {
            Some(u) if !u.disks.is_empty() => glidex_vm_shim::LaunchFile::read(&crate::paths::vm_paths(&vm.id).launch)
                .ok()
                .filter(|l| l.instance_id == inst.instance_id)
                .map(|l| crate::metering::sources::launched_disk_ids(&l.spec))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        st.last_exit = Some(ExitRecord {
            at: exited_at,
            instance_id: inst.instance_id.clone(),
            cause,
            code,
            signal,
            message: message.clone(),
            usage,
            disk_ids,
            launched_at: inst.launched_at,
        });
        st.instance = None;
        st.phase = VmPhase::Stopped;
        st.stop_deadline = None;
        set_cond(&mut st.conditions, "ConsoleReady", Tristate::False, "NotRunning", "");
        clear_cond(&mut st.conditions, "DevicesPending");
        let ran_for = now.saturating_sub(inst.launched_at);
        let mut stop: Option<(&str, &str, String)> = None;
        let kind = match cause {
            ExitCause::Requested | ExitCause::HostReboot => EventKind::Normal,
            _ => EventKind::Warning,
        };
        events.push(Event::new("controller", kind, "InstanceExited", format!("instance {} ended: {}", inst.instance_id, cause.as_str())));
        match cause {
            ExitCause::Requested => {}
            ExitCause::Lost if !shim_died => {}
            ExitCause::Terminated => stop = Some(("systemd", "Terminated", "the VM's unit was stopped outside glidex".into())),
            ExitCause::CleanExit => stop = Some(("guest", "CleanExit", "guest powered off; desired state set to stopped".into())),
            ExitCause::HostReboot => {
                if vm.spec.on_host_boot == crate::models::HostBootPolicy::Stop {
                    stop = Some(("host", "HostReboot", "host rebooted; on_host_boot is stop".into()));
                }
            }
            ExitCause::LaunchFailed => {
                st.launch_failures += 1;
                st.next_restart_at = Some(now + backoff(st.launch_failures).as_secs());
                st.phase = VmPhase::Failed;
                set_cond(&mut st.conditions, "Ready", Tristate::False, "LaunchFailed", message.clone().unwrap_or_default());
            }
            ExitCause::Crashed | ExitCause::Lost => {
                if ran_for >= STABLE_SECS {
                    st.restart_count = 0;
                }
                match vm.spec.restart_policy {
                    RestartPolicy::OnFailure => {
                        st.restart_count += 1;
                        let delay = crash_delay(st.restart_count);
                        st.next_restart_at = Some(now + delay);
                        set_cond(&mut st.conditions, "CrashLoopBackOff", Tristate::True, "BackingOff", format!("restarting in {} s", delay));
                        set_cond(&mut st.conditions, "Ready", Tristate::False, "Crashed", format!("the hypervisor crashed; restart {} in {} s", st.restart_count, delay));
                    }
                    RestartPolicy::Never => {
                        set_cond(&mut st.conditions, "Ready", Tristate::False, "Crashed", "the hypervisor crashed; restart_policy is never");
                        stop = Some(("controller", "Crashed", "the hypervisor crashed; desired state set to stopped".into()));
                    }
                }
            }
        }

        self.write_status(&vm.id, vm.generation, st, events).await?;
        // Disks the spec no longer references lose their claim with the
        // instance (D11): the disk controller releases them.
        for d in &inst.disks {
            self.queue.add(crate::controller::queue::Key::Disk(d.clone()));
        }
        if let Some((actor, reason, msg)) = stop {
            self.stop_spec_after_exit(&vm.id, vm.generation, actor, reason, msg).await?;
        }
        Ok(Some(Duration::ZERO))
    }

    /// Desired stopped with a live instance (§9.1 step 4).
    async fn stop_round(&self, vm: &Vm, mut st: VmStatus, events: Vec<Event>) -> Next {
        let now = crate::tenancy::now();
        let grace = vm.spec.stop_grace_secs as u64;
        let deadline = *st.stop_deadline.get_or_insert(now + grace + STOP_SLACK_SECS);
        let paths = vm.paths();
        let inst = st.instance.clone();
        let sent = shim_call(paths.shim_socket.clone(), move |c| c.stop(grace)).await;
        if sent.is_err() || now >= deadline {
            // Escalate (§8.8): the unit (or the verified shim), then SIGKILL.
            let runner = self.runner();
            let shim = inst.as_ref().and_then(|i| i.shim_pid.zip(i.shim_starttime));
            let pids: Vec<(u32, u64)> = inst
                .iter()
                .flat_map(|i| [i.shim_pid.zip(i.shim_starttime), i.hypervisor_pid.zip(i.hypervisor_starttime)])
                .flatten()
                .collect();
            if now >= deadline + STOP_SLACK_SECS {
                let _ = runner.kill(&vm.id, &pids).await;
            } else if now >= deadline || sent.is_err() {
                if let Err(e) = runner.stop(&vm.id, shim).await {
                    tracing::warn!(vm_id = %vm.id, "stopping the instance: {}", e);
                }
            }
        }
        st.phase = VmPhase::Stopping;
        set_cond(&mut st.conditions, "Ready", Tristate::False, "Progressing", "stopping");
        self.write_status(&vm.id, vm.generation, st, events).await?;
        Ok(Some(Duration::from_secs(deadline.saturating_sub(now).clamp(1, 5))))
    }

    /// Detach this VM's ports: all of them, or those `keep` rejects.
    pub(crate) async fn detach_ports(&self, vm_id: &str, st: &mut VmStatus, keep: Option<&(dyn Fn(&NicStatus) -> bool + Sync)>) -> Result<(), VmManagerError> {
        // The ports' counters go with them: meter them first (metering §5.4).
        if st.nics.iter().any(|n| !keep.is_some_and(|k| k(n))) {
            self.meter_final_sample().await;
        }
        let _ports = self.ports_lock.lock().await;
        let mut remaining = Vec::new();
        for nic in std::mem::take(&mut st.nics) {
            if keep.is_some_and(|k| k(&nic)) {
                remaining.push(nic);
                continue;
            }
            let netd = self.netd.clone();
            let (id, idx) = (vm_id.to_string(), nic.nic_index);
            match blocking(move || netd.call::<serde_json::Value>(Op::DetachVmPort { vm_id: id, nic_index: idx })).await? {
                Ok(_) => {}
                Err(NetError::Unavailable(e)) => {
                    // netd's own sync removes it once it is back.
                    tracing::warn!(vm_id, nic = idx, "glidex-netd unavailable; port left for its next sync: {}", e);
                    remaining.push(nic);
                }
                Err(e) => {
                    tracing::warn!(vm_id, nic = idx, "glidex-netd detach failed: {}", e);
                    remaining.push(nic);
                }
            }
        }
        st.nics = remaining;
        Ok(())
    }

    /// Desired running or paused, no live instance (§9.1 step 5).
    async fn launch_round(&self, vm: &Vm, mut st: VmStatus, events: Vec<Event>) -> Next {
        let now = crate::tenancy::now();
        if let Some(at) = st.next_restart_at.filter(|at| *at > now) {
            if st.phase != VmPhase::Failed {
                st.phase = VmPhase::Stopped;
            }
            self.write_status(&vm.id, vm.generation, st, events).await?;
            return Ok(Some(Duration::from_secs(at - now)));
        }
        clear_cond(&mut st.conditions, "CrashLoopBackOff");

        // Dependencies.
        let bindings = match self.disk_bindings(vm) {
            Ok(b) => {
                set_cond(&mut st.conditions, "DisksReady", Tristate::True, "Ready", "");
                b
            }
            Err(e) => {
                let reason = match &e {
                    VmManagerError::Image(ImageError::Busy(_)) => "DiskBusy",
                    VmManagerError::Image(ImageError::NotReady(_)) => "DiskNotReady",
                    VmManagerError::Image(ImageError::NotFound(_)) | VmManagerError::Image(ImageError::Io(_)) => "DiskMissing",
                    _ => "DiskNotReady",
                };
                set_cond(&mut st.conditions, "DisksReady", Tristate::False, reason, e.to_string());
                set_cond(&mut st.conditions, "Ready", Tristate::False, reason, e.to_string());
                if !matches!(st.phase, VmPhase::Failed) {
                    st.phase = VmPhase::Stopped;
                }
                self.write_status(&vm.id, vm.generation, st, events).await?;
                return Ok(Some(Duration::from_secs(5)));
            }
        };
        for att in &vm.config().networks {
            match self.networks.get(&att.network) {
                Ok(Some(_)) => {}
                _ => {
                    let msg = format!("network {} does not exist", att.network);
                    set_cond(&mut st.conditions, "NetworkReady", Tristate::False, "NetworkNotReady", msg.clone());
                    set_cond(&mut st.conditions, "Ready", Tristate::False, "NetworkNotReady", msg);
                    self.write_status(&vm.id, vm.generation, st, events).await?;
                    return Ok(Some(Duration::from_secs(5)));
                }
            }
        }

        st.phase = VmPhase::Provisioning;
        set_cond(&mut st.conditions, "Ready", Tristate::False, "Progressing", "provisioning");
        // A new instance takes the spec as it is now, with new ports.
        clear_cond(&mut st.conditions, "RestartRequired");
        st.conditions.retain(|c| !(c.kind == "NetworkReady" && c.reason == "PortLost"));
        let Some(stored) = self.write_status(&vm.id, vm.generation, st.clone(), events).await? else { return Ok(None) };
        st = stored.status;
        // Ports kept from an earlier instance stay ours whatever happens
        // this round (D16); only ports this round adds are undone.
        let kept: Vec<u8> = st.nics.iter().map(|n| n.nic_index).collect();

        match self.provision_and_start(vm, &mut st, bindings).await {
            Ok(()) => {
                st.phase = VmPhase::Starting;
                set_cond(&mut st.conditions, "Ready", Tristate::False, "Progressing", "starting");
                self.write_status(&vm.id, vm.generation, st, vec![]).await?;
                self.watch_instance(&vm.id).await;
                Ok(Some(Duration::from_millis(500)))
            }
            Err(LaunchFailure { error, intent }) => {
                let e = *error;
                if !intent {
                    // Nothing launched: undo this round's ports.
                    let keep = |n: &NicStatus| kept.contains(&n.nic_index);
                    self.detach_ports(&vm.id, &mut st, Some(&keep)).await?;
                }
                st.launch_failures += 1;
                st.next_restart_at = Some(crate::tenancy::now() + backoff(st.launch_failures).as_secs());
                st.phase = VmPhase::Failed;
                let reason = match &e {
                    VmManagerError::Network(NetError::Unavailable(_)) => "NetdUnavailable",
                    VmManagerError::Credential(_) => "CredentialError",
                    VmManagerError::Image(ImageError::ToolMissing { .. }) => "ToolUnavailable",
                    VmManagerError::HypervisorError(HypervisorError::CloudInit(m)) if m.contains("not installed") || m.contains("not found") => "ToolUnavailable",
                    _ => "ProvisioningFailed",
                };
                set_cond(&mut st.conditions, "Ready", Tristate::False, reason, e.to_string());
                let ev = Event::new("controller", EventKind::Warning, reason, e.to_string());
                self.write_status(&vm.id, vm.generation, st.clone(), vec![ev]).await?;
                Ok(Some(backoff(st.launch_failures)))
            }
        }
    }

    /// Seed, firmware vars, ports, `launch.json`, intent, runner (§9.1
    /// step 5.1-5.5). The error says whether the intent was written.
    async fn provision_and_start(
        &self,
        vm: &Vm,
        st: &mut VmStatus,
        (root, data): (Option<(crate::models::DiskBinding, crate::images::Disk)>, Vec<crate::models::DiskBinding>),
    ) -> Result<(), LaunchFailure> {
        let paths = vm.paths();
        crate::paths::ensure_vm_dir(&vm.id).map_err(|e| before_intent(HypervisorError::ProcessStart(e)))?;
        let mut config = vm.config().clone();
        config.root_disk_binding = root.as_ref().map(|(b, _)| b.clone());
        config.data_disk_bindings = data;
        if config.firmware_path.is_some() && vm.hypervisor() == HypervisorType::Qemu {
            config.firmware_vars_path = Some(self.firmware_vars_path(&vm.id));
            if let Some(id) = &config.firmware_image {
                let template = self.images.firmware_vars_template(id);
                config.firmware_vars_template = template.exists().then(|| template.to_string_lossy().into_owned());
            }
        }

        // 5.1 the cloud-init seed (regenerated on every launch).
        let mut growpart_disk = None;
        let mut growpart_seq = None;
        if config.firmware_path.is_some() && config.cloud_init_path.is_none() {
            let seed_path = paths.cloud_init.clone();
            let mut seed = match &config.credential {
                Some(username) => {
                    let cred = self.credentials.get(&vm.project, username).map_err(before_intent)?;
                    cloud_init::SeedConfig::for_credential(&vm.id, &vm.name, &cred)
                }
                None => cloud_init::SeedConfig::for_vm(&vm.id, &vm.name),
            };
            seed.nic_macs = config.networks.iter().enumerate().map(|(i, a)| {
                a.mac.clone().unwrap_or_else(|| glidex_ovs::names::mac_address(&vm.id, i as u8).unwrap_or_default())
            }).collect();
            seed.growpart = root.as_ref().is_some_and(|(_, d)| d.pending_growpart);
            if seed.growpart {
                growpart_disk = root.as_ref().map(|(_, d)| d.id.clone());
                growpart_seq = root.as_ref().map(|(_, d)| d.applied_extend_root_seq);
            }
            if seed.ssh_authorized_keys.is_empty() && seed.passwd_hash.is_none() {
                tracing::info!(vm_id = %vm.id, "VM has no login credential; the guest has no way to log in");
            }
            let s = seed.clone();
            blocking(move || cloud_init::write_seed_image(&seed_path, &s))
                .await
                .map_err(before_intent)?
                .map_err(before_intent)?;
            config.cloud_init_path = Some(paths.cloud_init.clone());
        }

        // 5.2 the ports: record each in status before attaching it (D16).
        config.nic_bindings = self.attach_ports(vm, st).await.map_err(before_intent)?;
        // vhost-user over 4 KiB guest pages makes OVS's PMDs take TLB
        // misses on every packet; back the guest with hugepages when the
        // host has them to spare.
        if !config.hugepages && crate::hypervisor::needs_shared_memory(&config) {
            config.hugepages = crate::hypervisor::hugepages_available_for(config.mem_size_mib);
        }

        // 5.3 launch.json
        let instance_id = uuid::Uuid::new_v4().to_string();
        let d = driver(vm.hypervisor());
        let args = {
            let c = config.clone();
            let api = paths.api_socket.clone();
            blocking(move || d.launch_args(&c, &api)).await.map_err(before_intent)?.map_err(before_intent)?
        };
        let settings = self.settings();
        let launch = LaunchFile {
            version: glidex_vm_shim::launch::LAUNCH_VERSION,
            vm_id: vm.id.clone(),
            instance_id: instance_id.clone(),
            hypervisor: vm.hypervisor().launch_kind(),
            argv: args.argv,
            fallback: args.fallback,
            api_socket: paths.api_socket.clone(),
            console_socket: paths.console_socket.clone(),
            shim_socket: paths.shim_socket.clone(),
            log_path: paths.log.clone(),
            log_max_bytes: settings.log_max_bytes,
            ready_timeout_secs: settings.ready_timeout_secs,
            host_shutdown_grace_secs: settings.host_shutdown_grace_secs,
            spec: serde_json::json!({ "generation": vm.generation, "spec": vm.spec }),
            meter_poll_secs: settings.meter_poll_secs,
        };
        launch.write(&paths.launch).map_err(|e| before_intent(HypervisorError::ProcessStart(e)))?;

        // 5.4 the intent record, before anything runs.
        let runner = self.runner();
        let boot = glidex_vm_shim::util::boot_id().map_err(|e| before_intent(HypervisorError::ProcessStart(e)))?;
        let c = vm.config();
        st.instance = Some(InstanceRef {
            instance_id: instance_id.clone(),
            runner: runner.record(&vm.id),
            boot_id: boot,
            launched_generation: vm.generation,
            disks: c.root_disk.iter().chain(c.data_disks.iter()).cloned().collect(),
            vfio_devices: c.vfio_devices.clone(),
            shim_pid: None,
            shim_starttime: None,
            hypervisor_pid: None,
            hypervisor_starttime: None,
            launched_at: crate::tenancy::now(),
            growpart_disk,
            growpart_seq,
        });
        st.never_started = false;
        let ev = Event::new("controller", EventKind::Normal, "Launching", format!("launching instance {}", instance_id));
        if let Some(stored) = self.write_status(&vm.id, vm.generation, st.clone(), vec![ev]).await.map_err(before_intent)? {
            *st = stored.status;
        }

        // 5.5 start the shim, and wait for it to leave `launching`.
        runner.start(&vm.id, &paths).await.map_err(|e| LaunchFailure {
            error: Box::new(HypervisorError::ProcessStart(std::io::Error::other(e.to_string())).into()),
            intent: true,
        })?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(settings.ready_timeout_secs + 5);
        loop {
            match InstanceFile::read(&paths.instance) {
                Ok(Some(f)) if f.instance_id == instance_id && f.phase != Phase::Launching => {
                    if let Some(inst) = st.instance.as_mut() {
                        inst.shim_pid = Some(f.shim_pid);
                        inst.shim_starttime = Some(f.shim_starttime);
                        inst.hypervisor_pid = f.hypervisor_pid;
                        inst.hypervisor_starttime = f.hypervisor_starttime;
                    }
                    return Ok(());
                }
                _ if tokio::time::Instant::now() >= deadline => return Ok(()),
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    /// Attach (or re-attach: netd is idempotent) every NIC of the spec,
    /// first detaching ports the spec no longer has. Each port is written
    /// to `status.nics` before netd attaches it.
    async fn attach_ports(&self, vm: &Vm, st: &mut VmStatus) -> Result<Vec<NicBinding>, VmManagerError> {
        let networks = vm.config().networks.clone();
        let stale = |n: &NicStatus| networks.get(n.nic_index as usize).is_some_and(|a| a.network == n.network);
        if st.nics.iter().any(|n| !stale(n)) {
            self.detach_ports(&vm.id, st, Some(&stale)).await?;
        }
        if networks.is_empty() {
            return Ok(Vec::new());
        }
        let _ports = self.ports_lock.lock().await;
        let mut out = Vec::new();
        for (i, att) in networks.iter().enumerate() {
            let net = self.networks.get(&att.network)?.ok_or_else(|| NetError::NotFound(att.network.clone()))?;
            let mac = match &att.mac {
                Some(m) => m.clone(),
                None => glidex_ovs::names::mac_address(&vm.id, i as u8).map_err(|e| NetError::Invalid(e.to_string()))?,
            };
            if !st.nics.iter().any(|n| n.nic_index as usize == i) {
                st.nics.push(NicStatus { network: net.name.clone(), nic_index: i as u8, mac: mac.clone(), port: None, ipv4: None, port_ok: false });
                if let Some(stored) = self.write_status(&vm.id, vm.generation, st.clone(), vec![]).await? {
                    *st = stored.status;
                }
            }
            let queue_pairs = nic_queue_pairs(att.queue_pairs, net.port_type, vm.config().vcpu_count);
            let spec = VmPortSpec {
                bridge: net.bridge.clone(),
                vm_id: vm.id.clone(),
                nic_index: i as u8,
                kind: net.port_type,
                mac: mac.clone(),
                vlan: net.vlan,
                mtu: net.mtu,
                queue_pairs,
            };
            let netd = self.netd.clone();
            let res: AttachResult = blocking(move || netd.call(Op::AttachVmPort(spec))).await??;
            if let Some(n) = st.nics.iter_mut().find(|n| n.nic_index as usize == i) {
                n.port = Some(res.port.clone());
                n.ipv4 = res.ipv4;
                n.port_ok = true;
            }
            out.push(NicBinding { id: format!("net{}", i), mac, binding: res.binding, queue_pairs, mtu: net.mtu });
        }
        Ok(out)
    }

    /// A live instance whose guest should be running or paused (§9.1
    /// steps 6-8).
    async fn running_round(&self, vm: &Vm, mut st: VmStatus, file: Option<InstanceFile>, events: Vec<Event>) -> Next {
        if file.as_ref().is_none_or(|f| f.phase == Phase::Launching) {
            st.phase = VmPhase::Starting;
            self.write_status(&vm.id, vm.generation, st, events).await?;
            return Ok(Some(Duration::from_secs(1)));
        }
        let paths = vm.paths();
        let d = driver(vm.hypervisor());
        let api = paths.api_socket.clone();
        let observed = match blocking(move || d.observe(&api)).await? {
            Ok(o) => o,
            Err(e) => {
                set_cond(&mut st.conditions, "HypervisorReachable", Tristate::False, "Unreachable", e.to_string());
                self.write_status(&vm.id, vm.generation, st, events).await?;
                return Ok(Some(Duration::from_secs(5)));
            }
        };
        set_cond(&mut st.conditions, "HypervisorReachable", Tristate::True, "Reachable", "");
        set_cond(&mut st.conditions, "ConsoleReady", Tristate::True, "Ready", "");
        let mut events = events;
        match observed.guest {
            GuestState::NotCreated | GuestState::Created => {
                // Cannot happen with D9; never patched up through the API.
                let _ = shim_call(paths.shim_socket.clone(), |c| c.kill()).await;
                set_cond(&mut st.conditions, "Ready", Tristate::False, "HypervisorError", "the hypervisor runs no booted VM; killed");
                events.push(Event::new("controller", EventKind::Warning, "HypervisorError", "the hypervisor runs no booted VM; killed it"));
                self.write_status(&vm.id, vm.generation, st, events).await?;
                return Ok(Some(Duration::from_secs(1)));
            }
            GuestState::Shutdown => {
                self.write_status(&vm.id, vm.generation, st, events).await?;
                return Ok(Some(Duration::from_secs(1)));
            }
            GuestState::Running | GuestState::Paused => {}
        }

        // ---- 7. drift ----------------------------------------------------------
        if !st.nics.is_empty() {
            self.port_drift(vm, &mut st, &mut events).await?;
        }
        let mut guest = observed.guest;
        let d = driver(vm.hypervisor());
        if vm.spec.power == PowerState::Paused && guest == GuestState::Running {
            let api = paths.api_socket.clone();
            match blocking(move || d.pause(&api)).await? {
                Ok(()) => guest = GuestState::Paused,
                Err(e) => events.push(Event::new("controller", EventKind::Warning, "PauseFailed", e.to_string())),
            }
        } else if vm.spec.power == PowerState::Running && guest == GuestState::Paused {
            let api = paths.api_socket.clone();
            match blocking(move || d.resume(&api)).await? {
                Ok(()) => guest = GuestState::Running,
                Err(e) => events.push(Event::new("controller", EventKind::Warning, "ResumeFailed", e.to_string())),
            }
        }

        // VFIO: hot-plug what the spec has and the guest lacks, and back.
        let wanted: Vec<String> = vm.config().vfio_devices.clone();
        let have: Vec<String> = observed.device_ids.iter().filter(|i| i.starts_with("_vfio_")).cloned().collect();
        let mut held: Vec<String> = st.instance.as_ref().map(|i| i.vfio_devices.clone()).unwrap_or_default();
        let to_add: Vec<String> = wanted.iter().filter(|p| !have.contains(&vfio_device_id(p))).cloned().collect();
        let to_remove: Vec<String> = held.iter().filter(|p| !wanted.contains(p) && have.contains(&vfio_device_id(p))).cloned().collect();
        if !to_add.is_empty() || !to_remove.is_empty() {
            if guest != GuestState::Running {
                set_cond(&mut st.conditions, "DevicesPending", Tristate::True, "GuestPaused", "devices change once the guest runs");
            } else {
                let mut failed = Vec::new();
                for p in to_add {
                    let (api, path) = (paths.api_socket.clone(), p.clone());
                    match blocking(move || d.add_device(&api, &path)).await? {
                        Ok(()) => {
                            if !held.contains(&p) {
                                held.push(p);
                            }
                        }
                        Err(e) => failed.push(format!("{}: {}", p, e)),
                    }
                }
                for p in to_remove {
                    let (api, path) = (paths.api_socket.clone(), p.clone());
                    match blocking(move || d.remove_device(&api, &path)).await? {
                        Ok(()) => held.retain(|h| *h != p),
                        Err(e) => failed.push(format!("{}: {}", p, e)),
                    }
                }
                if failed.is_empty() {
                    clear_cond(&mut st.conditions, "DevicesPending");
                } else {
                    set_cond(&mut st.conditions, "DevicesPending", Tristate::True, "HotplugFailed", failed.join("; "));
                }
            }
        } else {
            clear_cond(&mut st.conditions, "DevicesPending");
        }
        held.retain(|p| wanted.contains(p) || have.contains(&vfio_device_id(p)));
        if let Some(inst) = st.instance.as_mut() {
            inst.vfio_devices = held;
        }

        st.phase = if guest == GuestState::Paused { VmPhase::Paused } else { VmPhase::Running };
        let now = crate::tenancy::now();
        if let Some(inst) = st.instance.as_mut() {
            if now.saturating_sub(inst.launched_at) >= STABLE_SECS {
                st.restart_count = 0;
            }
            // The seed carried a root-partition grow and the guest booted
            // it: say so; the disk controller clears its flag (each
            // controller writes only its own object, §6.2).
            if guest == GuestState::Running {
                if let Some(disk) = inst.growpart_disk.take() {
                    st.seed_growpart_seq = inst.growpart_seq.take();
                    self.queue.add(crate::controller::queue::Key::Disk(disk));
                }
            }
        }
        st.launch_failures = 0;
        st.next_restart_at = None;

        // ---- 8. RestartRequired, Ready ------------------------------------------
        let launched = blocking(move || launched_spec(&paths)).await?;
        let restart_required = launched.as_ref().is_some_and(|l| launch_relevant(l) != launch_relevant(&vm.spec));
        if restart_required {
            set_cond(&mut st.conditions, "RestartRequired", Tristate::True, "ConfigChanged", "changes take effect at the next start");
        } else if st.conditions.iter().any(|c| c.kind == "RestartRequired" && c.reason == "ConfigChanged") {
            clear_cond(&mut st.conditions, "RestartRequired");
        }
        let converged = matches!((vm.spec.power, st.phase), (PowerState::Running, VmPhase::Running) | (PowerState::Paused, VmPhase::Paused))
            && !st.conditions.iter().any(|c| c.kind == "DevicesPending" && c.status == Tristate::True);
        if converged {
            set_cond(&mut st.conditions, "Ready", Tristate::True, "Converged", "");
        } else {
            set_cond(&mut st.conditions, "Ready", Tristate::False, "Progressing", format!("guest is {:?}, desired {}", guest, power_name(vm.spec.power)));
        }
        clear_cond(&mut st.conditions, "CrashLoopBackOff");
        self.write_status(&vm.id, vm.generation, st, events).await?;
        Ok(if converged { None } else { Some(Duration::from_secs(2)) })
    }

    /// §10.3: a live instance's ports, against what OVS has. An OVS port
    /// gone while its tap remains, or a vhost-user port gone, is re-added
    /// (`attach_vm_port` is idempotent, F5). A tap that is gone is not
    /// re-created: the hypervisor holds an fd to the old device, so a new
    /// one would be connected to nothing (`PortLost`, restart needed).
    async fn port_drift(&self, vm: &Vm, st: &mut VmStatus, events: &mut Vec<Event>) -> Result<(), VmManagerError> {
        let netd = self.netd.clone();
        let bridges = match blocking(move || netd.call::<Vec<glidex_netd::proto::BridgeRecord>>(Op::ListBridges)).await? {
            Ok(b) => b,
            Err(_) => return Ok(()), // netd away: nothing to compare with
        };
        let mut lost = Vec::new();
        for i in 0..st.nics.len() {
            let nic = st.nics[i].clone();
            let Some(port) = nic.port.clone() else { continue };
            let Ok(Some(net)) = self.networks.get(&nic.network) else { continue };
            let Some(live) = bridges.iter().find(|b| b.spec.name == net.bridge).and_then(|b| b.live.as_ref()) else { continue };
            if live.ports.contains(&port) {
                st.nics[i].port_ok = true;
                continue;
            }
            let tap = net.port_type == glidex_ovs::vm_port::VmPortKind::Tap;
            if tap && !std::path::Path::new("/sys/class/net").join(&port).exists() {
                st.nics[i].port_ok = false;
                lost.push(port);
                continue;
            }
            let Some(att) = vm.config().networks.get(nic.nic_index as usize) else { continue };
            let spec = VmPortSpec {
                bridge: net.bridge.clone(),
                vm_id: vm.id.clone(),
                nic_index: nic.nic_index,
                kind: net.port_type,
                mac: nic.mac.clone(),
                vlan: net.vlan,
                mtu: net.mtu,
                queue_pairs: nic_queue_pairs(att.queue_pairs, net.port_type, vm.config().vcpu_count),
            };
            let _ports = self.ports_lock.lock().await;
            let netd = self.netd.clone();
            match blocking(move || netd.call::<AttachResult>(Op::AttachVmPort(spec))).await? {
                Ok(_) => {
                    st.nics[i].port_ok = true;
                    events.push(Event::new("controller", EventKind::Warning, "PortRestored", format!("re-added {} to {}", port, net.bridge)));
                }
                Err(e) => {
                    st.nics[i].port_ok = false;
                    events.push(Event::new("controller", EventKind::Warning, "PortRestoreFailed", format!("{}: {}", port, e)));
                }
            }
        }
        if lost.is_empty() {
            if st.conditions.iter().any(|c| c.kind == "NetworkReady" && c.reason == "PortLost") {
                set_cond(&mut st.conditions, "NetworkReady", Tristate::True, "Ready", "");
            }
        } else {
            let msg = format!("tap {} is gone; restart the VM to get a new one", lost.join(", "));
            if !st.conditions.iter().any(|c| c.kind == "NetworkReady" && c.reason == "PortLost") {
                events.push(Event::new("controller", EventKind::Warning, "PortLost", msg.clone()));
            }
            set_cond(&mut st.conditions, "NetworkReady", Tristate::False, "PortLost", msg.clone());
            set_cond(&mut st.conditions, "RestartRequired", Tristate::True, "PortLost", msg);
        }
        Ok(())
    }

    /// Work off a deleted VM's finalizers and drop the record (§6.3). The
    /// instance is gone.
    pub(crate) async fn finalize_deletion(&self, vm: &Vm) -> Result<(), VmManagerError> {
        let has = |f: &str| vm.finalizers.iter().any(|x| x == f);
        // Ports and the VM's NAT reservations.
        if has(FINALIZER_PORTS) && (!vm.config().networks.is_empty() || !vm.status.nics.is_empty()) {
            self.meter_final_sample().await;
            let _ports = self.ports_lock.lock().await;
            let netd = self.netd.clone();
            let id = vm.id.clone();
            match blocking(move || netd.call::<serde_json::Value>(Op::ReleaseVm { vm_id: id })).await? {
                Ok(_) => {}
                // No ports to remove: netd being away only leaks a NAT
                // reservation, as before.
                Err(e) if vm.status.nics.is_empty() => tracing::warn!(vm_id = %vm.id, "glidex-netd release failed: {}", e),
                Err(e) => return Err(e.into()),
            }
        }
        // Disks: claims cleared, the owned root disk deleted (unless kept).
        let mut detach = Vec::new();
        let mut owned = None;
        if has(FINALIZER_DISKS) {
            let c = vm.config();
            let mut ids: Vec<&String> = c.root_disk.iter().chain(c.data_disks.iter()).collect();
            if let Some(i) = &vm.status.instance {
                ids.extend(i.disks.iter());
            }
            ids.sort();
            ids.dedup();
            for id in ids {
                let Ok(mut d) = self.images.get_disk(id) else { continue };
                if d.attached_to.as_deref().is_some_and(|a| a != vm.id) {
                    continue;
                }
                d.attached_to = None;
                if c.owns_root_disk && has(FINALIZER_OWNED_DISK) && Some(id) == c.root_disk.as_ref() {
                    owned = Some(d);
                } else {
                    detach.push(d);
                }
            }
        }
        let ev_key = event_key("vm", &vm.id);
        {
            let mut vms = self.vms.write().await;
            self.store.commit(Commit {
                delete_vm: Some(&vm.id),
                put_disks: detach.iter().collect(),
                delete_disks: owned.iter().map(|d| d.id.as_str()).collect(),
                ..Default::default()
            })?;
            vms.remove(&vm.id);
        }
        let _ = self.store.delete_events(&ev_key);
        for d in &detach {
            self.images.cache_disk(d);
        }
        if let Some(d) = &owned {
            self.images.uncache_disk(&d.id);
            self.images.remove_disk_file(d);
        }
        if has(FINALIZER_RUNTIME) {
            let _ = std::fs::remove_file(self.firmware_vars_path(&vm.id));
            crate::paths::remove_vm_dir(&vm.id);
        }
        self.notify_changed();
        tracing::info!(vm_id = %vm.id, name = %vm.name, "VM deleted");
        Ok(())
    }

    /// Queue `id` for its controller (also used by the API after writes).
    pub fn enqueue_vm(&self, id: &str) {
        self.queue.add(Key::Vm(id.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_track_transitions() {
        let mut c = Vec::new();
        set_cond(&mut c, "Ready", Tristate::False, "Progressing", "a");
        let t0 = c[0].last_transition_at;
        set_cond(&mut c, "Ready", Tristate::False, "Progressing", "b");
        assert_eq!((c.len(), c[0].message.as_str(), c[0].last_transition_at), (1, "b", t0));
        set_cond(&mut c, "Ready", Tristate::True, "Converged", "");
        assert_eq!(c[0].reason, "Converged");
        clear_cond(&mut c, "Ready");
        assert!(c.is_empty());
    }

    #[test]
    fn crash_backoff_is_10s_doubling_to_5min() {
        assert_eq!([1, 2, 3, 5, 6, 30].map(crash_delay), [10, 20, 40, 160, 300, 300]);
    }

    /// spec/clustering.md §4.1 / C0: an idle VM causes no store write across
    /// ten resync rounds.
    #[tokio::test]
    async fn an_unchanged_status_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let manager = VmManager::with_db_path(dir.path().join("glidex.db")).unwrap();
        let config: crate::models::VmConfig = serde_json::from_value(serde_json::json!({
            "vcpu_count": 1, "mem_size_mib": 512, "rootfs_path": "/r", "kernel_args": "", "firmware_path": "/f.fd"
        }))
        .unwrap();
        let vm = manager.create_vm("idle".into(), config).await.unwrap();
        let cur = manager.get_vm(&vm.id).await.unwrap();
        // The first write stamps `last_reconciled_at`.
        manager.write_status(&vm.id, cur.generation, cur.status.clone(), vec![]).await.unwrap();
        let db = manager.database();
        let before = db.revision();
        for _ in 0..10 {
            let cur = manager.get_vm(&vm.id).await.unwrap();
            manager.write_status(&vm.id, cur.generation, cur.status.clone(), vec![]).await.unwrap();
        }
        assert_eq!(db.revision(), before, "ten idle rounds wrote to the store");
        // A real change is written.
        let mut changed = manager.get_vm(&vm.id).await.unwrap().status;
        changed.never_started = !changed.never_started;
        manager.write_status(&vm.id, vm.generation, changed, vec![]).await.unwrap();
        assert_eq!(db.revision(), before + 1);
    }
}
