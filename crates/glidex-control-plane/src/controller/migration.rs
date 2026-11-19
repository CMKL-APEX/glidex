//! Live migration (spec/32-live-migration.md): what may move where (§5).

use std::collections::BTreeMap;

use crate::datastore::record::NodeRow;
use crate::hypervisor::HypervisorType;
use crate::models::{PowerState, VmPhase};
use crate::state::{VmManager, VmManagerError};

/// Why a VM can't migrate, and each target's answer (§5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationCheck {
    /// Reasons that hold whatever the target: the VM itself can't move.
    pub vm: Vec<String>,
    pub targets: Vec<TargetCheck>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetCheck {
    pub node: String,
    pub name: String,
    /// Empty: the VM can go there, if `MigrationCheck::vm` is empty too.
    pub reasons: Vec<String>,
}

impl MigrationCheck {
    /// The targets the VM can migrate to now.
    pub fn fitting(&self) -> impl Iterator<Item = &TargetCheck> {
        self.targets
            .iter()
            .filter(|t| self.vm.is_empty() && t.reasons.is_empty())
    }
}

impl VmManager {
    /// `POST /vms/{id}/migrate {check: true}` (§5, §10): the checks,
    /// against `node` or every other node, without moving anything.
    pub async fn migration_check(
        &self,
        vm_id: &str,
        node: Option<&str>,
    ) -> Result<MigrationCheck, VmManagerError> {
        if self.cluster().is_none() {
            return Err(crate::state::invalid(
                "a standalone host has no other node to migrate a VM to",
            ));
        }
        let want = node.map(|n| self.resolve_node(n)).transpose()?;
        let vms = self.vms.read().await;
        let vm = vms
            .get(vm_id)
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;
        let c = vm.config();
        let source = vm.status.placement.as_ref().map(|p| p.node.clone());
        let mut out = MigrationCheck::default();
        let mut no = |r: String| out.vm.push(r);

        if c.hypervisor != HypervisorType::CloudHypervisor {
            no(format!(
                "it runs on {}; only Cloud Hypervisor VMs migrate",
                c.hypervisor
            ));
        }
        if vm.deletion_requested_at.is_some() {
            no("it is being deleted".into());
        } else if vm.spec.power != PowerState::Running || vm.status.phase != VmPhase::Running {
            no(format!("it is {}, not running", vm.state()));
        }
        if vm.status.boot_cpuid.is_none() {
            no("its node recorded no CPUID when it booted; restart it once".into());
        }
        if c.root_disk.is_none() && !c.rootfs_path.is_empty() {
            no(format!(
                "its root is a host path, {}, on its node",
                c.rootfs_path
            ));
        }
        if !c.vfio_devices.is_empty() || !c.devices.is_empty() || c.gpus.is_some() {
            no("it passes through PCI devices".into());
        }
        let placement = vm.status.placement.as_ref();
        if placement.is_some_and(|p| !p.dpu_nics.is_empty()) {
            no("its NICs are on a DPU".into());
        }
        for a in &c.networks {
            if let Ok(Some(n)) = self.networks.get(&a.network)
                && n.scope == crate::network::NetworkScope::Node
            {
                no(format!("network {} exists on its node only", n.name));
            }
        }
        let db = self.store.database();
        let store_name = |id: &str| {
            crate::datastore::record::find(&db, id)
                .ok()
                .flatten()
                .map(|r| r.name)
                .unwrap_or_else(|| id.to_string())
        };
        // §7: each end's hyperstor-vhost must offer the dirty log, the
        // drain and the delegate, as its node last reported.
        let mut hyperstor: Vec<(String, BTreeMap<String, NodeRow>)> = Vec::new();
        let mut disks = Vec::new();
        for id in c.root_disk.iter().chain(c.data_disks.iter()) {
            let d = self.images.get_disk(id)?;
            if !self.images.is_hyperstor(&d) {
                no(format!(
                    "disk {} is on datastore {}; only hyperstor disks migrate",
                    d.name,
                    store_name(&d.datastore)
                ));
            } else if !hyperstor.iter().any(|(ds, _)| *ds == d.datastore) {
                let rows = crate::datastore::record::rows(&db, &d.datastore).unwrap_or_default();
                hyperstor.push((d.datastore.clone(), rows));
            }
            disks.push(d);
        }
        let migrates = |rows: &BTreeMap<String, NodeRow>, node: &str| {
            rows.get(node)
                .is_some_and(|r| r.reachable && r.live_migration)
        };
        for (ds, rows) in &hyperstor {
            if source.as_deref().is_some_and(|n| !migrates(rows, n)) {
                no(format!(
                    "hyperstor-vhost on its node can't hand datastore {}'s disks over; upgrade hyperstor there",
                    store_name(ds)
                ));
            }
        }
        let datapath = |id: &str| {
            self.nodes()
                .get(id)
                .ok()
                .flatten()
                .and_then(|n| n.status.features.br_int_datapath)
        };
        let source_datapath = source.as_deref().and_then(datapath);
        if source_datapath.as_deref() == Some("netdev") && !c.networks.is_empty() {
            no("its NICs are vhost-user (its node's br-int runs DPDK)".into());
        }

        let req = self.sched_request_pub(vm, &disks);
        for fit in self.evaluate_nodes(&vms, vm_id, &req) {
            if want.as_ref().is_some_and(|w| *w != fit.id) {
                continue;
            }
            let mut reasons = Vec::new();
            if source.as_deref() == Some(fit.id.as_str()) {
                reasons.push("the VM runs there".into());
            } else {
                if let Some(r) = fit.refused {
                    reasons.push(r);
                }
                for (ds, rows) in &hyperstor {
                    if !migrates(rows, &fit.id) {
                        reasons.push(format!(
                            "hyperstor-vhost there can't take datastore {}'s disks over",
                            store_name(ds)
                        ));
                    }
                }
                match self.nodes().get(&fit.id).ok().flatten() {
                    Some(n) => {
                        if let Err(r) = crate::node::cpu_fits(vm.status.boot_cpuid.as_deref(), &n) {
                            reasons.push(r);
                        }
                        if !c.networks.is_empty()
                            && n.status.features.br_int_datapath.as_deref() == Some("netdev")
                        {
                            reasons.push("its br-int runs DPDK: NICs there are vhost-user".into());
                        }
                    }
                    None => reasons.push("no such node".into()),
                }
            }
            out.targets.push(TargetCheck {
                node: fit.id,
                name: fit.name,
                reasons,
            });
        }
        out.targets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}
