//! Placing VMs that have no node yet (spec/clustering.md §9.1): on the leader,
//! for every VM waiting for one, and again whenever nodes or capacity change.
//! A VM is placed once; after that it stays (D6).

use crate::controller::queue::Key;
use crate::models::Tristate;
use crate::state::VmManager;
use crate::store::{Commit, Event, EventKind, event_key};

impl VmManager {
    /// One pass over the VMs without a placement.
    pub async fn place_pending(&self) {
        if self.cluster().is_none() || !self.store.database().can_write() {
            return;
        }
        let mut vms = self.vms.write().await;
        let waiting: Vec<String> = vms
            .values()
            .filter(|v| v.status.placement.is_none() && v.deletion_requested_at.is_none())
            .map(|v| v.id.clone())
            .collect();
        for id in waiting {
            let Some(mut vm) = vms.get(&id).cloned() else {
                continue;
            };
            let mut disks: Vec<crate::images::Disk> = Vec::new();
            for d in vm
                .config()
                .root_disk
                .iter()
                .chain(vm.config().data_disks.iter())
            {
                if let Ok(d) = self.images.get_disk(d) {
                    disks.push(d);
                }
            }
            let req = self.sched_request_pub(&vm, &disks);
            match self.pick_node(&vms, &req) {
                Ok(choice) => {
                    let node = choice.node;
                    vm.status.placement = Some(crate::models::Placement {
                        node: node.clone(),
                        at: crate::tenancy::now(),
                        page_kib: Some(choice.page_kib),
                        numa_node: choice.numa_node,
                        numa: choice.numa,
                        cpus: choice.cpus,
                        pool_devices: choice.pool_devices,
                    });
                    let policy =
                        !vm.config().numa_policy.is_none() || !vm.config().cpu_policy.is_shared();
                    VmManager::note_numa_alignment(&mut vm, policy, choice.numa_aligned);
                    crate::controller::vm::set_cond(
                        &mut vm.status.conditions,
                        "Scheduled",
                        Tristate::True,
                        "Scheduled",
                        "",
                    );
                    vm.resource_version += 1;
                    for d in &mut disks {
                        let shared = self.images.is_shared(d);
                        d.bind_to(&node, shared);
                    }
                    let ev = Event::new(
                        "scheduler",
                        EventKind::Normal,
                        "Scheduled",
                        format!("placed on node {node}"),
                    );
                    let commit = Commit {
                        put_vm: Some(&vm),
                        put_disks: disks.iter().collect(),
                        events: vec![(event_key("vm", &vm.id), ev)],
                        reserve_ips: self.cluster_nics(&vm),
                        ..Default::default()
                    };
                    if let Err(e) = self.store.commit(commit) {
                        tracing::warn!(vm = %vm.name, "recording a placement: {}", e);
                        continue;
                    }
                    for d in &disks {
                        self.images.cache_disk(d);
                    }
                    vms.insert(vm.id.clone(), vm.clone());
                    self.queue.add(Key::Vm(vm.id.clone()));
                    self.notify_changed();
                    tracing::info!(vm = %vm.name, %node, "placed");
                }
                Err(refused) => {
                    let msg = crate::scheduler::explain(&refused);
                    let same = vm.status.conditions.iter().any(|c| {
                        c.kind == "Scheduled" && c.status == Tristate::False && c.message == msg
                    });
                    if same {
                        continue;
                    }
                    crate::controller::vm::set_cond(
                        &mut vm.status.conditions,
                        "Scheduled",
                        Tristate::False,
                        "Unschedulable",
                        msg,
                    );
                    vm.resource_version += 1;
                    if self
                        .store
                        .commit(Commit {
                            put_vm: Some(&vm),
                            ..Default::default()
                        })
                        .is_ok()
                    {
                        vms.insert(vm.id.clone(), vm);
                        self.notify_changed();
                    }
                }
            }
        }
    }
}

impl VmManager {
    /// `POST /vms/{id}/place` (spec/storage.md S10, §9): place a stopped VM
    /// again, or a lost one (its node forgotten, `--fenced`), on `node` or
    /// wherever the scheduler finds room. Only what can go with it may be
    /// on its old node: its disks on shared datastores (a node-local disk
    /// only if the target is its node), no host-path root, no PCI devices,
    /// no node networks. Nothing moves by itself (D8).
    pub async fn place_vm(
        &self,
        vm_id: &str,
        node: Option<&str>,
        actor: &str,
    ) -> Result<crate::models::Vm, crate::state::VmManagerError> {
        use crate::models::{PowerState, VmPhase};
        use crate::state::VmManagerError;
        use crate::state::invalid;
        if self.cluster().is_none() {
            return Err(invalid(
                "a standalone host has no other node to place a VM on",
            ));
        }
        let target = node.map(|n| self.resolve_node(n)).transpose()?;
        {
            let mut vms = self.vms.write().await;
            let cur = vms
                .get(vm_id)
                .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?
                .clone();
            if cur.deletion_requested_at.is_some() {
                return Err(VmManagerError::Deleting(cur.name.clone()));
            }
            // Lost: its node was forgotten, so it runs nowhere (§5.6).
            let old = cur.status.placement.as_ref().map(|p| p.node.clone());
            let lost = old.as_ref().is_some_and(|n| {
                self.nodes()
                    .get(n)
                    .ok()
                    .flatten()
                    .is_none_or(|n| n.status.phase.is_tombstone())
            });
            let stopped = cur.spec.power == PowerState::Stopped
                && cur.status.instance.is_none()
                && matches!(cur.status.phase, VmPhase::Stopped | VmPhase::Failed);
            if !lost && !stopped {
                // Only a stopped VM, or one whose node was forgotten.
                return Err(VmManagerError::InvalidState {
                    current: cur.state(),
                    operation: "place (stop it first)".into(),
                });
            }
            let c = cur.config();
            if c.root_disk.is_none() && !c.rootfs_path.is_empty() {
                return Err(invalid(format!(
                    "VM {}'s root is a host path, {}, on its node",
                    cur.name, c.rootfs_path
                )));
            }
            if !c.vfio_devices.is_empty() {
                return Err(invalid(format!(
                    "VM {} passes through PCI devices of its node",
                    cur.name
                )));
            }
            for a in &c.networks {
                if let Ok(Some(n)) = self.networks.get(&a.network)
                    && n.scope == crate::network::NetworkScope::Node
                {
                    return Err(invalid(format!(
                        "network {} exists on one node only: attach the VM to a cluster network",
                        n.name
                    )));
                }
            }
            for id in c.root_disk.iter().chain(c.data_disks.iter()) {
                let d = self.images.get_disk(id)?;
                if self.images.is_shared(&d) {
                    continue;
                }
                let here = target.is_some() && d.node == target && !lost;
                if !here {
                    return Err(invalid(format!(
                        "disk {} is on node-local datastore {}{}: it can't go with the VM (put it on a shared datastore)",
                        d.name,
                        d.datastore,
                        d.node
                            .as_deref()
                            .map(|n| format!(" of node {n}"))
                            .unwrap_or_default()
                    )));
                }
            }
            let mut vm = cur;
            vm.status.placement = None;
            if lost {
                // The forgotten node's instance is gone with it.
                vm.status.instance = None;
                vm.status.phase = VmPhase::Stopped;
                // Its node was forgotten as fenced: its disks' leases are
                // taken from it (S23).
                vm.status.takeover_disks = true;
                vm.status.staged_disks.clear();
            }
            vm.status
                .conditions
                .retain(|c| !(c.kind == "Ready" && c.reason == "Lost") && c.kind != "Scheduled");
            // A pin to the old node would keep it there.
            vm.spec.node = target.clone();
            let msg = format!(
                "to be placed {} (was on {})",
                target
                    .as_deref()
                    .map(|t| format!("on node {t}"))
                    .unwrap_or_else(|| "where the scheduler finds room".into()),
                old.as_deref().unwrap_or("no node")
            );
            self.spec_write(&mut vms, vm, actor, "Replacing", msg)?;
        }
        self.place_pending().await;
        self.vms
            .read()
            .await
            .get(vm_id)
            .cloned()
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))
    }
}
