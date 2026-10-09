//! Startup (spec/reconciliation.md §9.4): before any controller acts,
//! observe every VM and adopt the instances that are still running, list
//! orphans, and only then tell netd which VMs own ports.

use crate::controller::vm::set_cond;
use crate::instance::Liveness;
use crate::models::{Tristate, VmPhase};
use crate::state::{Orphan, VmManager};
use crate::store::{Event, EventKind};
use glidex_vm_shim::state::Phase;

impl VmManager {
    /// §9.4 steps 2-4. No VM is launched, stopped or changed on the host
    /// here; only what is observed is recorded.
    pub(crate) async fn adopt_instances(&self) {
        let ids: Vec<String> = self.vms.read().await.values().filter(|v| self.is_local(v)).map(|v| v.id.clone()).collect();
        let mut adopted = 0;
        for id in &ids {
            let Some(vm) = self.vm(id).await else { continue };
            let unit = self.runner().unit_active(id).await;
            let v = vm.clone();
            let Ok(seen) = tokio::task::spawn_blocking(move || crate::instance::liveness(&v, unit)).await else { continue };
            if seen.liveness != Liveness::Live {
                continue;
            }
            let mut st = vm.status.clone();
            let mut events = Vec::new();
            match &seen.file {
                Some(f) => {
                    let matches = st.instance.as_ref().is_some_and(|i| i.instance_id == f.instance_id);
                    if !matches {
                        let inst = self.adopted_ref(&vm, f);
                        events.push(Event::new("controller", EventKind::Warning, "AdoptedUnrecorded", format!("adopted instance {}", f.instance_id)));
                        st.instance = Some(inst);
                    } else if let Some(inst) = st.instance.as_mut() {
                        inst.shim_pid = Some(f.shim_pid);
                        inst.shim_starttime = Some(f.shim_starttime);
                        inst.hypervisor_pid = f.hypervisor_pid.or(inst.hypervisor_pid);
                        inst.hypervisor_starttime = f.hypervisor_starttime.or(inst.hypervisor_starttime);
                    }
                    if f.phase == Phase::Running && !matches!(st.phase, VmPhase::Running | VmPhase::Paused) {
                        st.phase = VmPhase::Running;
                    }
                    events.push(Event::new("controller", EventKind::Normal, "Adopted", format!("instance {} is still running", f.instance_id)));
                    adopted += 1;
                }
                None if st.instance.is_none() => {
                    st.phase = VmPhase::Unknown;
                    set_cond(&mut st.conditions, "HypervisorReachable", Tristate::Unknown, "LegacyOrphan",
                        "a hypervisor answers on this VM's socket but no shim reports it; stop that process");
                }
                None => {}
            }
            if let Err(e) = self.write_status(id, vm.status.observed_generation, st, events).await {
                tracing::warn!(vm_id = %id, "recording the adopted instance: {}", e);
            }
        }
        if adopted > 0 {
            tracing::info!(count = adopted, "adopted running VM instances");
        }

        // Orphans (D17): reported, never touched.
        let mut orphans = Vec::new();
        if let Ok(dir) = std::fs::read_dir(crate::paths::run_dir().join("vms")) {
            for e in dir.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if !ids.contains(&name) {
                    orphans.push(Orphan { kind: "runtime_dir", id: name });
                }
            }
        }
        if let Ok(units) = self.runner().list_units().await {
            for u in units {
                if !ids.contains(&u) {
                    orphans.push(Orphan { kind: "unit", id: crate::instance::runner::unit_name(&u) });
                }
            }
        }
        for o in &orphans {
            tracing::warn!(kind = o.kind, id = %o.id, "orphan with no VM record; leaving it alone");
        }
        *self.orphans.lock().unwrap() = orphans;

        // Only now: netd may detach ports of VMs that own none (D16).
        self.sync_netd_ports().await;
    }

}
