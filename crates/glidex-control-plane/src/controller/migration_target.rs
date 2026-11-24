//! A live migration's target (spec/32-live-migration.md §4): the node a
//! record names prepares the guest's disks, taps and seed, runs a shim in
//! receive mode, reports the guest received, and lets go of all of it
//! after a cancel. The VM isn't this node's until the commit, so this runs
//! beside its VM controller, from the record (decision 8).

use std::sync::Arc;
use std::time::Duration;

use glidex_vm_shim::launch::LaunchFile;
use glidex_vm_shim::state::{InstanceFile, Phase};

use crate::controller::vm::{blocking, nic_queue_pairs, port_spec, shim_call};
use crate::datastore::hyperstor::HyperstorDriver;
use crate::hypervisor::cloud_hypervisor::CloudHypervisorDriver;
use crate::images::Disk;
use crate::models::{InstanceRef, Migration, MigrationPhase, Vm};
use crate::state::VmManager;

/// How long the receiving shim may take to answer, once started.
const RECEIVE_READY_SECS: u64 = 30;

/// What the target finds of its receiving instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Receiver {
    /// None started for this migration, or one from before the host's
    /// last boot.
    None,
    /// Waiting for the guest.
    Receiving(InstanceFile),
    /// The guest runs here.
    Running(InstanceFile),
    /// The receive failed, or the shim is gone; why.
    Ended(String),
}

/// What the target does next for record `m`, given its receiver (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetStep {
    /// Nothing yet: the source's delegates, the guest, or the commit.
    Wait,
    /// Attach, plug, seed and start the receiving shim.
    Prepare,
    /// The shim receives: write `Copying`.
    Ready,
    /// The guest runs here: promote the disks and write `received`.
    Received,
    /// Preparing or receiving failed before the guest moved.
    Cancel(String),
    /// Stop the shim, detach the disks and taps, write `target_released`.
    Release,
}

/// The target's step (§4, decisions 14 and 15). `delegated`: the source
/// has named a delegate for every hyperstor disk.
pub fn target_step(m: &Migration, receiver: &Receiver, delegated: bool) -> TargetStep {
    use MigrationPhase::*;
    if m.received.is_some() {
        return TargetStep::Wait;
    }
    // A guest running here wins over any cancel (decision 14).
    if let Receiver::Running(_) = receiver
        && m.phase != Failed
    {
        return TargetStep::Received;
    }
    match m.phase {
        Preparing => match receiver {
            Receiver::Receiving(_) => TargetStep::Ready,
            Receiver::Ended(why) => TargetStep::Cancel(why.clone()),
            _ if !delegated => TargetStep::Wait,
            _ => TargetStep::Prepare,
        },
        Copying | Switching => match receiver {
            Receiver::Ended(why) if !m.moved() => TargetStep::Cancel(why.clone()),
            Receiver::None if !m.moved() => {
                TargetStep::Cancel("the receiving instance is gone".into())
            }
            _ => TargetStep::Wait,
        },
        Cancelling | Failed if !m.target_released => TargetStep::Release,
        _ => TargetStep::Wait,
    }
}

impl VmManager {
    /// One pass over the migrations naming this node as their target, from
    /// each node's loop.
    pub async fn migration_target_round(&self) {
        if self.cluster().is_none() {
            return;
        }
        let me = self.local_node_id();
        let vms: Vec<Vm> = self
            .vms
            .read()
            .await
            .values()
            .filter(|v| !self.is_local(v))
            .filter(|v| v.status.migration.as_ref().is_some_and(|m| m.target == me))
            .cloned()
            .collect();
        for vm in vms {
            if let Err(e) = self.migration_target_step(&vm).await {
                tracing::warn!(vm = %vm.name, "a migration's target: {}", e);
            }
        }
    }

    async fn migration_target_step(&self, vm: &Vm) -> Result<(), String> {
        let Some(m) = vm.status.migration.clone() else {
            return Ok(());
        };
        let disks = self.hyperstor_disks(vm)?;
        let delegated = disks.iter().all(|(d, _)| m.delegates.contains_key(&d.id));
        let receiver = self.receiver(vm).await;
        match target_step(&m, &receiver, delegated) {
            TargetStep::Wait => Ok(()),
            TargetStep::Prepare => match self.prepare_target(vm, &m, &disks).await {
                Ok(()) => {
                    self.target_writes(vm, |m| m.phase = MigrationPhase::Copying)
                        .await
                }
                Err(why) => self.target_cancels(vm, why).await,
            },
            TargetStep::Ready => {
                self.target_writes(vm, |m| m.phase = MigrationPhase::Copying)
                    .await
            }
            TargetStep::Cancel(why) => self.target_cancels(vm, why).await,
            TargetStep::Received => {
                let Receiver::Running(f) = receiver else {
                    return Ok(());
                };
                // The lease passes to this node before the source detaches
                // (decision 15).
                for (d, driver) in &disks {
                    let name = d.name.clone();
                    let (d, driver) = (d.clone(), driver.clone());
                    blocking(move || driver.promote(&d))
                        .await
                        .map_err(|e| e.to_string())?
                        .map_err(|e| format!("promoting disk {name}: {e}"))?;
                }
                let inst = self.received_instance(vm, &f).await?;
                self.target_writes(vm, |m| m.received = Some(inst)).await
            }
            TargetStep::Release => {
                self.release_target(vm, &m, &disks).await;
                self.target_writes(vm, |m| m.target_released = true).await
            }
        }
    }

    async fn target_writes(&self, vm: &Vm, f: impl FnOnce(&mut Migration)) -> Result<(), String> {
        self.write_migration(&vm.id, f)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn target_cancels(&self, vm: &Vm, why: String) -> Result<(), String> {
        tracing::warn!(vm = %vm.name, "cancelling a migration to this node: {}", why);
        self.target_writes(vm, |m| {
            m.phase = MigrationPhase::Cancelling;
            m.reason = format!("on the target: {why}");
        })
        .await
    }

    /// The VM's hyperstor disks, each with this node's driver for it. A
    /// disk on another datastore is refused: the check lets only hyperstor
    /// disks migrate (§5).
    pub(crate) fn hyperstor_disks(
        &self,
        vm: &Vm,
    ) -> Result<Vec<(Disk, Arc<HyperstorDriver>)>, String> {
        let me = self.local_node_id();
        let db = self.store.database();
        let c = vm.config();
        let mut out = Vec::new();
        for id in c.root_disk.iter().chain(c.data_disks.iter()) {
            let d = self.images.get_disk(id).map_err(|e| e.to_string())?;
            let ds = crate::datastore::record::find(&db, &d.datastore)
                .ok()
                .flatten()
                .ok_or_else(|| format!("disk {}'s datastore is unknown here", d.name))?;
            let driver = HyperstorDriver::of(&ds, self.images.hyperstor_identity(&ds))
                .ok_or_else(|| format!("disk {} isn't on hyperstor", d.name))?
                .on_node(&me);
            out.push((d, Arc::new(driver)));
        }
        Ok(out)
    }

    /// This node's receiving instance for VM `vm`'s migration: the shim
    /// started from a receiving `launch.json`, in this boot, since the
    /// migration started. One left from an earlier migration to this node
    /// (the VM moved here, then away again) isn't this one's.
    async fn receiver(&self, vm: &Vm) -> Receiver {
        let paths = vm.paths();
        let vm_id = vm.id.clone();
        let started = vm.status.migration.as_ref().map_or(0, |m| m.started_at);
        blocking(move || {
            let Ok(launch) = LaunchFile::read(&paths.launch) else {
                return Receiver::None;
            };
            if launch.vm_id != vm_id || launch.receive.is_none() {
                return Receiver::None;
            }
            let Ok(Some(f)) = InstanceFile::read(&paths.instance) else {
                return Receiver::None;
            };
            let boot = glidex_vm_shim::util::boot_id().unwrap_or_default();
            if f.instance_id != launch.instance_id
                || f.boot_id != boot
                || !of_this_migration(f.launched_at, started)
            {
                return Receiver::None;
            }
            let alive =
                glidex_vm_shim::util::same_process(f.shim_pid, f.shim_starttime).unwrap_or(false);
            match f.phase {
                _ if !alive => Receiver::Ended("its shim is gone".into()),
                Phase::Launching | Phase::Receiving => Receiver::Receiving(f),
                Phase::Running => Receiver::Running(f),
                Phase::Exited => Receiver::Ended(
                    f.exit
                        .as_ref()
                        .and_then(|e| e.message.clone())
                        .unwrap_or_else(|| "the receive failed".into()),
                ),
            }
        })
        .await
        .unwrap_or(Receiver::None)
    }

    /// §4 step 2 on the target: each disk standing by as the source's
    /// delegate, the taps, the seed, then the shim receiving. Each part is
    /// safe to repeat after a restart.
    async fn prepare_target(
        &self,
        vm: &Vm,
        m: &Migration,
        disks: &[(Disk, Arc<HyperstorDriver>)],
    ) -> Result<(), String> {
        let paths = vm.paths();
        crate::paths::ensure_vm_dir(&vm.id).map_err(|e| format!("its directory: {e}"))?;
        for (d, driver) in disks {
            let delegate = m.delegates.get(&d.id).cloned().unwrap_or_default();
            let name = d.name.clone();
            let (d, driver) = (d.clone(), driver.clone());
            blocking(move || driver.attach_delegate(&d, &delegate).map(|_| ()))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("attaching disk {name} as a delegate: {e}"))?;
        }
        self.attach_target_ports(vm).await?;
        let config = vm.config();
        if config.generates_seed() {
            let root = config
                .root_disk
                .as_ref()
                .and_then(|id| self.images.get_disk(id).ok());
            self.write_seed(vm, config, root.as_ref())
                .await
                .map_err(|e| format!("its cloud-init seed: {e}"))?;
        }

        let (args, url) =
            CloudHypervisorDriver::receive_args(&paths.api_socket, &paths.migration_socket)
                .map_err(|e| e.to_string())?;
        let settings = self.settings();
        let instance_id = uuid::Uuid::new_v4().to_string();
        let launch = LaunchFile {
            volume_locks: Vec::new(),
            version: glidex_vm_shim::launch::LAUNCH_VERSION,
            vm_id: vm.id.clone(),
            instance_id: instance_id.clone(),
            hypervisor: vm.hypervisor().launch_kind(),
            argv: args.argv,
            fallback: None,
            api_socket: paths.api_socket.clone(),
            console_socket: paths.console_socket.clone(),
            shim_socket: paths.shim_socket.clone(),
            log_path: paths.log.clone(),
            log_max_bytes: settings.log_max_bytes,
            ready_timeout_secs: RECEIVE_READY_SECS,
            host_shutdown_grace_secs: settings.host_shutdown_grace_secs,
            spec: serde_json::json!({ "generation": vm.generation, "spec": vm.spec }),
            meter_poll_secs: settings.meter_poll_secs,
            // The guest's threads start here on this node's shared CPUs
            // (decision 17).
            cpu_affinity: self.local_shared_pool().await,
            vcpu_pins: Vec::new(),
            cpu_weight: config.effective_cpu_weight(),
            job: false,
            receive: Some(url),
        };
        launch
            .write(&paths.launch)
            .map_err(|e| format!("its launch file: {e}"))?;
        self.runner()
            .start(&vm.id, &paths, RECEIVE_READY_SECS)
            .await
            .map_err(|e| format!("starting the receiving shim: {e}"))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(RECEIVE_READY_SECS + 5);
        loop {
            match self.receiver(vm).await {
                Receiver::Receiving(f) if f.phase == Phase::Receiving => return Ok(()),
                Receiver::Running(_) => return Ok(()),
                Receiver::Ended(why) => return Err(format!("the receiving shim: {why}")),
                _ if tokio::time::Instant::now() >= deadline => {
                    return Err("the receiving shim didn't answer in time".into());
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    /// The VM's taps on this node, with the same names and MACs as on the
    /// source, without recording them: the VM's `status.nics` is the
    /// source's until the commit, and the taps' names are the same.
    async fn attach_target_ports(&self, vm: &Vm) -> Result<(), String> {
        let networks = vm.config().networks.clone();
        if networks.is_empty() {
            return Ok(());
        }
        let _ports = self.ports_lock.lock().await;
        for (i, att) in networks.iter().enumerate() {
            let net = self
                .networks
                .get(&att.network)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("network {} is gone", att.network))?;
            let mac = match &att.mac {
                Some(m) => m.clone(),
                None => {
                    glidex_ovs::names::mac_address(&vm.id, i as u8).map_err(|e| e.to_string())?
                }
            };
            let qp = nic_queue_pairs(att.queue_pairs, net.port_type, vm.config().vcpu_count);
            let spec = port_spec(&vm.id, &net, i as u8, mac, qp, att, false);
            let netd = self.netd.clone();
            blocking(move || {
                netd.call::<glidex_netd::proto::AttachResult>(glidex_netd::proto::Op::AttachVmPort(
                    spec,
                ))
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("attaching NIC {i}: {e}"))?;
        }
        Ok(())
    }

    /// The instance the commit gives the VM: the source's record of the
    /// guest (its disks and CPU policy), with this node's shim.
    async fn received_instance(&self, vm: &Vm, f: &InstanceFile) -> Result<InstanceRef, String> {
        let launch = LaunchFile::read(&vm.paths().launch).map_err(|e| e.to_string())?;
        let c = vm.config();
        let mut inst = vm.status.instance.clone().unwrap_or_else(|| InstanceRef {
            instance_id: String::new(),
            runner: self.runner().record(&vm.id),
            boot_id: String::new(),
            launched_generation: vm.generation,
            disks: c
                .root_disk
                .iter()
                .chain(c.data_disks.iter())
                .cloned()
                .collect(),
            vfio_devices: Vec::new(),
            shim_pid: None,
            shim_starttime: None,
            hypervisor_pid: None,
            hypervisor_starttime: None,
            launched_at: 0,
            growpart_disk: None,
            growpart_seq: None,
            cpu_affinity: Vec::new(),
            launched_max_vcpus: None,
            applied_cpu_policy: None,
            device_errors: Default::default(),
            received_usage: None,
        });
        inst.instance_id = f.instance_id.clone();
        inst.runner = self.runner().record(&vm.id);
        inst.boot_id = f.boot_id.clone();
        inst.shim_pid = Some(f.shim_pid);
        inst.shim_starttime = Some(f.shim_starttime);
        inst.hypervisor_pid = f.hypervisor_pid;
        inst.hypervisor_starttime = f.hypervisor_starttime;
        inst.launched_at = crate::tenancy::now();
        inst.growpart_disk = None;
        inst.growpart_seq = None;
        inst.cpu_affinity = launch.cpu_affinity;
        inst.vfio_devices.clear();
        inst.device_errors.clear();
        // What the receive used, metered as the migration's (decision 19).
        inst.received_usage = self.meter().and_then(|m| {
            let u = m.instance_usage(&inst)?;
            Some(crate::models::ReceivedUsage {
                since: f.launched_at * 1000,
                at: crate::metering::now_ms(),
                cpu_usec: u.cpu_usec,
                io: u.io.map(|io| crate::models::ReceivedIo {
                    read_bytes: io.read_bytes,
                    write_bytes: io.write_bytes,
                    read_ops: io.read_ops,
                    write_ops: io.write_ops,
                }),
            })
        });
        Ok(inst)
    }

    /// A cancelled or failed migration's target lets go (§4, decision 15):
    /// the receiving shim stopped, the standby disks detached before the
    /// source drops its delegates, the taps and the launch files removed.
    /// Each part is best-effort and safe to repeat; what fails is logged.
    async fn release_target(&self, vm: &Vm, m: &Migration, disks: &[(Disk, Arc<HyperstorDriver>)]) {
        let paths = vm.paths();
        if let Receiver::Receiving(f) = self.receiver(vm).await {
            // A shim receiving kills its VMM at once (12-reconciliation.md §8.1).
            let sock = paths.shim_socket.clone();
            if shim_call(sock, |c| c.kill()).await.is_err() {
                let _ = self
                    .runner()
                    .kill(&vm.id, &[(f.shim_pid, f.shim_starttime)])
                    .await;
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while matches!(self.receiver(vm).await, Receiver::Receiving(_))
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let _ = shim_call(paths.shim_socket.clone(), |c| c.release()).await;
        }
        for (d, driver) in disks
            .iter()
            .filter(|(d, _)| m.delegates.contains_key(&d.id))
        {
            let (d, driver) = (d.clone(), driver.clone());
            let name = d.name.clone();
            match blocking(move || driver.detach_forced(&d)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(vm = %vm.name, disk = %name, "detaching a standby disk: {}", e)
                }
                Err(e) => {
                    tracing::warn!(vm = %vm.name, disk = %name, "detaching a standby disk: {}", e)
                }
            }
        }
        self.remove_migration_taps(vm).await;
        for p in [
            paths.launch.clone(),
            paths.instance.clone(),
            std::path::PathBuf::from(&paths.cloud_init),
        ] {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl VmManager {
    /// VM `vm`'s taps on this node, without touching its record: a
    /// migration's end that no longer has the guest (§4). Best-effort.
    pub(crate) async fn remove_migration_taps(&self, vm: &Vm) {
        let _ports = self.ports_lock.lock().await;
        for i in 0..vm.config().networks.len() {
            let netd = self.netd.clone();
            let vm_id = vm.id.clone();
            let r = blocking(move || {
                netd.call::<serde_json::Value>(glidex_netd::proto::Op::DetachVmPort {
                    vm_id,
                    nic_index: i as u8,
                })
            })
            .await;
            if let Ok(Err(e)) = r {
                tracing::warn!(vm = %vm.name, nic = i, "removing a migration's tap: {}", e);
            }
        }
    }
}

/// Whether a receiving instance launched at `launched_at` (unix s) can be
/// that of a migration started at `started_at`: not if it is older.
pub fn of_this_migration(launched_at: u64, started_at: u64) -> bool {
    launched_at >= started_at
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A receiver from an earlier migration to this node, which the VM has
    /// left since, isn't the current migration's: found on the test
    /// cluster, where a VM moved back onto a node it had been received on
    /// was cancelled with "its shim is gone" (LM6 M2).
    #[test]
    fn an_older_receiver_is_not_this_migrations() {
        assert!(of_this_migration(1_000, 1_000));
        assert!(of_this_migration(1_005, 1_000));
        assert!(!of_this_migration(990, 1_000));
    }

    fn file(phase: Phase) -> InstanceFile {
        serde_json::from_value(serde_json::json!({
            "version": 1, "vm_id": "vm1", "instance_id": "i", "boot_id": "b",
            "shim_pid": 1, "shim_starttime": 1, "phase": phase, "launched_at": 1,
        }))
        .unwrap()
    }

    fn record(phase: MigrationPhase) -> Migration {
        Migration {
            target: "node-b".into(),
            phase,
            ..Default::default()
        }
    }

    /// §4 on the target: prepare once the delegates are named, report ready
    /// while receiving, report the guest when it runs (even mid-cancel),
    /// cancel a receive that ended before the guest moved, and let go once
    /// after a cancel.
    #[test]
    fn the_target_follows_the_record_and_its_receiver() {
        use MigrationPhase::*;
        let none = Receiver::None;
        let receiving = Receiver::Receiving(file(Phase::Receiving));
        let running = Receiver::Running(file(Phase::Running));
        let ended = Receiver::Ended("refused".into());

        assert_eq!(
            target_step(&record(Preparing), &none, false),
            TargetStep::Wait
        );
        assert_eq!(
            target_step(&record(Preparing), &none, true),
            TargetStep::Prepare
        );
        assert_eq!(
            target_step(&record(Preparing), &receiving, true),
            TargetStep::Ready
        );
        assert_eq!(
            target_step(&record(Preparing), &ended, true),
            TargetStep::Cancel("refused".into())
        );

        assert_eq!(
            target_step(&record(Copying), &receiving, true),
            TargetStep::Wait
        );
        assert_eq!(
            target_step(&record(Copying), &running, true),
            TargetStep::Received
        );
        assert_eq!(
            target_step(&record(Copying), &ended, true),
            TargetStep::Cancel("refused".into())
        );
        assert!(matches!(
            target_step(&record(Copying), &none, true),
            TargetStep::Cancel(_)
        ));
        // The source sent it: no cancel from here.
        let mut sent = record(Switching);
        sent.sent = true;
        assert_eq!(target_step(&sent, &ended, true), TargetStep::Wait);
        assert_eq!(target_step(&sent, &running, true), TargetStep::Received);

        // Decision 14: the guest here finishes forwards.
        assert_eq!(
            target_step(&record(Cancelling), &running, true),
            TargetStep::Received
        );
        assert_eq!(
            target_step(&record(Cancelling), &receiving, true),
            TargetStep::Release
        );
        assert_eq!(
            target_step(&record(Failed), &none, true),
            TargetStep::Release
        );
        let mut released = record(Failed);
        released.target_released = true;
        assert_eq!(target_step(&released, &none, true), TargetStep::Wait);

        // Once received, the leader commits.
        let mut got = record(Copying);
        got.received = Some(
            serde_json::from_value(serde_json::json!({
                "instance_id": "i", "runner": { "kind": "detached" }, "boot_id": "b",
                "launched_generation": 1, "launched_at": 5,
            }))
            .unwrap(),
        );
        assert_eq!(target_step(&got, &running, true), TargetStep::Wait);
    }
}
