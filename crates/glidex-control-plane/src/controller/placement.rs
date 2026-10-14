//! Placing VMs that have no node yet (spec/clustering.md §9.1): on the leader,
//! for every VM waiting for one, and again whenever nodes or capacity change.
//! A VM is placed once; after that it stays (D6).

use crate::controller::queue::Key;
use crate::models::Tristate;
use crate::state::VmManager;
use crate::store::{event_key, Commit, Event, EventKind};

impl VmManager {
    /// One pass over the VMs without a placement.
    pub async fn place_pending(&self) {
        if self.cluster().is_none() || !self.store.database().can_write() {
            return;
        }
        let mut vms = self.vms.write().await;
        let waiting: Vec<String> = vms.values().filter(|v| v.status.placement.is_none() && v.deletion_requested_at.is_none()).map(|v| v.id.clone()).collect();
        for id in waiting {
            let Some(mut vm) = vms.get(&id).cloned() else { continue };
            let mut disks: Vec<crate::images::Disk> = Vec::new();
            for d in vm.config().root_disk.iter().chain(vm.config().data_disks.iter()) {
                if let Ok(d) = self.images.get_disk(d) {
                    disks.push(d);
                }
            }
            let req = self.sched_request_pub(&vm, &disks);
            match self.pick_node(&vms, &req) {
                Ok(node) => {
                    vm.status.placement = Some(crate::models::Placement { node: node.clone(), at: crate::tenancy::now() });
                    crate::controller::vm::set_cond(&mut vm.status.conditions, "Scheduled", Tristate::True, "Scheduled", "");
                    vm.resource_version += 1;
                    for d in &mut disks {
                        d.node.get_or_insert_with(|| node.clone());
                    }
                    let ev = Event::new("scheduler", EventKind::Normal, "Scheduled", format!("placed on node {node}"));
                    let commit = Commit { put_vm: Some(&vm), put_disks: disks.iter().collect(), events: vec![(event_key("vm", &vm.id), ev)], reserve_ips: self.cluster_nics(&vm), ..Default::default() };
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
                    let same = vm.status.conditions.iter().any(|c| c.kind == "Scheduled" && c.status == Tristate::False && c.message == msg);
                    if same {
                        continue;
                    }
                    crate::controller::vm::set_cond(&mut vm.status.conditions, "Scheduled", Tristate::False, "Unschedulable", msg);
                    vm.resource_version += 1;
                    if self.store.commit(Commit { put_vm: Some(&vm), ..Default::default() }).is_ok() {
                        vms.insert(vm.id.clone(), vm);
                        self.notify_changed();
                    }
                }
            }
        }
    }
}
