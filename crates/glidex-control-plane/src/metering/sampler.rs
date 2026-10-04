//! One sampling round over VMs and disks (spec/metering.md §5.1, §5.3,
//! §5.6, §6.1): snapshots in, ledger observations out.

use super::ledger::{Flag, MeteringError, Origin, Round, Subject, SubjectKind};
use super::sources::Host;
use crate::images::{Disk, DiskPhase};
use crate::models::{Runner, Vm, VmPhase};
use std::collections::BTreeSet;

/// Meters a VM accrues while it has an instance (§2).
const VM_GAUGES: [&str; 5] = ["vm.running", "vm.paused", "cpu.alloc", "mem.alloc", "mem.used"];

const MIB: u64 = 1 << 20;

pub fn vm_subject(vm: &Vm) -> Subject {
    Subject::new(SubjectKind::Vm, vm.id.clone(), vm.name.clone(), Some(vm.project.clone()))
}

fn disk_subject(d: &Disk) -> Subject {
    Subject::new(SubjectKind::Disk, d.id.clone(), d.name.clone(), (!d.project.is_empty()).then(|| d.project.clone()))
}

/// The phases in which an instance exists and is metered.
fn live(phase: VmPhase) -> bool {
    matches!(phase, VmPhase::Starting | VmPhase::Running | VmPhase::Paused | VmPhase::Stopping)
}

/// Meter every VM: usage from the instance's cgroup (or `/proc`), and
/// allocation from the sizing it was launched with (D11).
pub fn sample_vms(round: &mut Round, vms: &[Vm], host: &Host, now: u64) -> Result<(), MeteringError> {
    for vm in vms {
        sample_vm(round, vm, host, now)?;
    }
    Ok(())
}

fn sample_vm(round: &mut Round, vm: &Vm, host: &Host, now: u64) -> Result<(), MeteringError> {
    let s = vm_subject(vm);
    let st = &vm.status;
    let since = st.phase_since.map(|t| t * 1000).unwrap_or(0);
    let Some(inst) = st.instance.as_ref().filter(|_| live(st.phase)) else {
        // No instance: end the gauges when it really ended.
        let end = st.last_exit.as_ref().map(|e| e.at * 1000).unwrap_or(since.max(1)).min(now);
        for m in VM_GAUGES {
            round.gauge_end_at(&s, m, end)?;
        }
        return Ok(());
    };
    let run = inst.instance_id.as_str();
    let launched = inst.launched_at * 1000;
    // The exit time of the run before this one, if it is the one recorded.
    let prev_end = st.last_exit.as_ref().map(|e| e.at * 1000);
    let c = vm.config();
    let running = st.phase != VmPhase::Paused;
    let levels = [
        ("vm.running", running as u64),
        ("vm.paused", !running as u64),
        ("cpu.alloc", if running { c.vcpu_count as u64 } else { 0 }),
        ("mem.alloc", c.mem_size_mib as u64),
    ];
    for (m, level) in levels {
        round.gauge_run(&s, m, run, launched, level, since, now, prev_end)?;
    }

    let usage = match &inst.runner {
        Runner::Systemd { unit } => inst
            .shim_pid
            .and_then(|pid| host.cgroup_of(pid))
            // The shim's cgroup must be this VM's unit (pid reuse check).
            .filter(|cg| cg.file_name().is_some_and(|n| n.to_string_lossy() == *unit))
            .and_then(|cg| host.cgroup_usage(&cg)),
        Runner::Detached => inst.hypervisor_pid.and_then(|pid| host.proc_usage(pid, inst.hypervisor_starttime)),
    };
    let Some(u) = usage else { return Ok(()) };
    // One run of the cgroup (or process) = one unit invocation of this
    // instance; the shim's start time tells two invocations apart.
    let reset_key = match &inst.runner {
        Runner::Systemd { .. } => format!("{run}/{}", inst.shim_starttime.unwrap_or(0)),
        Runner::Detached => format!("{run}/{}", inst.hypervisor_starttime.unwrap_or(0)),
    };
    round.counter(&s, "cpu.used", &reset_key, u.cpu_usec, now, Origin::ZeroAt(launched))?;
    // Hugepage-backed guest RAM is not in memory.current (§5.1).
    let used_mib = u.memory_bytes / MIB + if c.hugepages { c.mem_size_mib as u64 } else { 0 };
    round.gauge_run(&s, "mem.used", run, launched, used_mib, now, now, prev_end)?;
    round.max(&s, "mem.peak", used_mib, now);
    if u.from_proc {
        round.flag(&s, now, Flag::SourceProc);
    }
    Ok(())
}

/// Meter provisioned disk size (`disk.alloc`, §5.3): every disk whose
/// storage is reserved, attached or not, running or not.
pub fn sample_disks(round: &mut Round, disks: &[Disk], now: u64) -> Result<(), MeteringError> {
    for d in disks {
        let s = disk_subject(d);
        let charged = matches!(d.phase, DiskPhase::Ready | DiskPhase::Resizing | DiskPhase::Missing) && d.size_bytes > 0;
        if charged {
            let mib = d.size_bytes.div_ceil(MIB);
            // First sight accrues from creation (or metering's start).
            round.gauge_run(&s, "disk.alloc", "disk", d.created_at * 1000, mib, 0, now, None)?;
        } else {
            round.gauge_end_at(&s, "disk.alloc", now)?;
        }
    }
    Ok(())
}

/// Forget the cursors of subjects that no longer exist. Nothing is
/// charged for the time since their last reading: when a record went
/// away is unknown, and at most one interval is lost (§6.4).
pub fn forget_gone(round: &mut Round, with_cursors: &BTreeSet<String>, live: &BTreeSet<String>) {
    for key in with_cursors.difference(live) {
        round.forget_subject(key);
    }
}

#[cfg(test)]
mod tests {
    use super::super::ledger::{combine, Ledger, LedgerSettings, HOUR_MS};
    use super::*;
    use redb::Database;
    use std::path::Path;
    use std::sync::Arc;

    const T0: u64 = 1_791_000_000 / 3600 * HOUR_MS;

    fn vm(phase: &str, instance: serde_json::Value, extra: serde_json::Value) -> Vm {
        let mut status = serde_json::json!({ "phase": phase, "phase_since": T0 / 1000, "instance": instance });
        if let (Some(st), Some(extra)) = (status.as_object_mut(), extra.as_object()) {
            st.extend(extra.clone());
        }
        Vm {
            id: "vm-1".into(),
            name: "web-1".into(),
            project: "p1".into(),
            created_at: 0,
            generation: 1,
            resource_version: 1,
            deletion_requested_at: None,
            finalizers: vec![],
            spec: serde_json::from_value(serde_json::json!({
                "config": { "vcpu_count": 2, "mem_size_mib": 1024, "rootfs_path": "/r", "kernel_args": "" }
            }))
            .unwrap(),
            status: serde_json::from_value(status).unwrap(),
        }
    }

    fn instance(id: &str, launched_s: u64) -> serde_json::Value {
        serde_json::json!({
            "instance_id": id, "runner": { "kind": "systemd", "unit": "glidex-vm@vm-1.service" },
            "boot_id": "b", "launched_generation": 1, "shim_pid": 42, "shim_starttime": 7, "launched_at": launched_s,
        })
    }

    /// A ledger that started at time 0 (so launches are billed in
    /// full), and an empty fake host.
    fn setup() -> (tempfile::TempDir, Ledger, Host) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Database::create(dir.path().join("t.db")).unwrap());
        let l = Ledger::new(db, LedgerSettings::from_secs(30, 120)).unwrap();
        l.set_started_at_for_test(0);
        let host = Host { proc_root: dir.path().join("proc"), cgroup_root: dir.path().join("cg") };
        (dir, l, host)
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn cgroup(host: &Host, cpu_usec: u64, mem_bytes: u64) {
        write(&host.proc_root.join("42/cgroup"), "0::/glidex.slice/glidex-vms.slice/glidex-vm@vm-1.service\n");
        let cg = host.cgroup_root.join("glidex.slice/glidex-vms.slice/glidex-vm@vm-1.service");
        write(&cg.join("cpu.stat"), &format!("usage_usec {cpu_usec}\n"));
        write(&cg.join("memory.current"), &format!("{mem_bytes}\n"));
    }

    fn totals(l: &Ledger) -> std::collections::BTreeMap<String, u64> {
        combine(&l.scan(0, u64::MAX / 10, None).unwrap())
    }

    #[test]
    fn running_vm_cpu_memory_and_allocation() {
        let (_dir, l, host) = setup();
        let launched = T0 / 1000;
        let v = vm("running", instance("i1", launched), serde_json::json!({}));
        cgroup(&host, 1_000_000, 512 * MIB);
        let mut r = l.begin_round(T0 + 30_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &host, T0 + 30_000).unwrap();
        l.commit(r).unwrap();
        cgroup(&host, 4_000_000, 768 * MIB);
        let mut r = l.begin_round(T0 + 60_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &host, T0 + 60_000).unwrap();
        l.commit(r).unwrap();
        let t = totals(&l);
        assert_eq!(t["cpu.used"], 4_000_000, "counted from launch");
        assert_eq!(t["cpu.alloc"], 2 * 60, "2 vCPU × 60 s since launch");
        assert_eq!(t["mem.alloc"], 1024 * 60);
        assert_eq!(t["vm.running"], 60);
        assert_eq!(t["mem.used"], 512 * 30, "512 MiB held from the first reading");
        assert_eq!(t["mem.peak"], 768);
    }

    #[test]
    fn pause_stops_cpu_alloc_but_not_memory() {
        let (_dir, l, host) = setup();
        cgroup(&host, 0, 0);
        let launched = T0 / 1000;
        let mut v = vm("running", instance("i1", launched), serde_json::json!({}));
        let mut r = l.begin_round(T0 + 30_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &host, T0 + 30_000).unwrap();
        // Paused at +40 s, seen at +60 s.
        v.status.phase = VmPhase::Paused;
        v.status.phase_since = Some(launched + 40);
        sample_vms(&mut r, std::slice::from_ref(&v), &host, T0 + 60_000).unwrap();
        l.commit(r).unwrap();
        let t = totals(&l);
        assert_eq!(t["cpu.alloc"], 2 * 40);
        assert_eq!(t["mem.alloc"], 1024 * 60);
        assert_eq!((t["vm.running"], t["vm.paused"]), (40, 20));
    }

    #[test]
    fn exit_ends_gauges_at_the_exit_time() {
        let (_dir, l, host) = setup();
        cgroup(&host, 0, 0);
        let launched = T0 / 1000;
        let mut v = vm("running", instance("i1", launched), serde_json::json!({}));
        let mut r = l.begin_round(T0 + 30_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &host, T0 + 30_000).unwrap();
        // Exited at +45 s, seen at +90 s.
        v.status.phase = VmPhase::Stopped;
        v.status.instance = None;
        v.status.last_exit = serde_json::from_value(serde_json::json!({ "at": launched + 45, "instance_id": "i1", "cause": "clean_exit" })).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &host, T0 + 90_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &host, T0 + 120_000).unwrap();
        l.commit(r).unwrap();
        assert_eq!(totals(&l)["cpu.alloc"], 2 * 45);
    }

    #[test]
    fn foreign_cgroup_is_not_read() {
        let (_dir, l, host) = setup();
        // pid 42 now belongs to something else.
        write(&host.proc_root.join("42/cgroup"), "0::/user.slice/session-1.scope\n");
        let v = vm("running", instance("i1", T0 / 1000), serde_json::json!({}));
        let mut r = l.begin_round(T0 + 30_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &host, T0 + 30_000).unwrap();
        l.commit(r).unwrap();
        assert!(!totals(&l).contains_key("cpu.used"));
    }

    #[test]
    fn disks_charge_provisioned_size_while_reserved() {
        let (_dir, l, _) = setup();
        let mut d: Disk = serde_json::from_value(serde_json::json!({
            "id": "d1", "name": "data", "project": "p1", "format": "qcow2", "size_bytes": 10u64 << 30,
            "origin": { "kind": "blank" }, "created_at": T0 / 1000, "phase": "ready",
        }))
        .unwrap();
        let mut r = l.begin_round(T0 + 30_000).unwrap();
        sample_disks(&mut r, std::slice::from_ref(&d), T0 + 30_000).unwrap();
        sample_disks(&mut r, std::slice::from_ref(&d), T0 + 60_000).unwrap();
        d.phase = DiskPhase::Failed;
        sample_disks(&mut r, std::slice::from_ref(&d), T0 + 90_000).unwrap();
        l.commit(r).unwrap();
        assert_eq!(totals(&l)["disk.alloc"], 10 * 1024 * 90);
        // Gone: its cursors are forgotten.
        let with = l.cursor_subjects(&[SubjectKind::Disk, SubjectKind::Vm]).unwrap();
        let mut r = l.begin_round(T0 + 120_000).unwrap();
        forget_gone(&mut r, &with, &BTreeSet::new());
        l.commit(r).unwrap();
        assert!(l.cursor_subjects(&[SubjectKind::Disk]).unwrap().is_empty());
    }
}
