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
    let mut s = Subject::new(SubjectKind::Disk, d.id.clone(), d.name.clone(), (!d.project.is_empty()).then(|| d.project.clone()));
    // The VM it is attached to, so VM totals can include its disks (§8.6).
    s.vm_id = d.attached_to.clone();
    s
}

/// The phases in which an instance exists and is metered.
fn live(phase: VmPhase) -> bool {
    matches!(phase, VmPhase::Starting | VmPhase::Running | VmPhase::Paused | VmPhase::Stopping)
}

/// Meter every VM: usage from the instance's cgroup (or `/proc`), and
/// allocation from the sizing it was launched with (D11).
pub fn sample_vms(round: &mut Round, vms: &[Vm], disks: &[Disk], host: &Host, now: u64) -> Result<(), MeteringError> {
    for vm in vms {
        sample_vm(round, vm, disks, host, now)?;
    }
    Ok(())
}

fn sample_vm(round: &mut Round, vm: &Vm, disks: &[Disk], host: &Host, now: u64) -> Result<(), MeteringError> {
    let s = vm_subject(vm);
    let st = &vm.status;
    let since = st.phase_since.map(|t| t * 1000).unwrap_or(0);
    let Some(inst) = st.instance.as_ref().filter(|_| live(st.phase)) else {
        // No instance: end the gauges when it really ended.
        let end = st.last_exit.as_ref().map(|e| e.at * 1000).unwrap_or(since.max(1)).min(now);
        for m in VM_GAUGES {
            round.gauge_end_at(&s, m, end)?;
        }
        if let Some(e) = &st.last_exit {
            consume_exit(round, vm, e, disks)?;
        }
        return Ok(());
    };
    let run = inst.instance_id.as_str();
    let launched = inst.launched_at * 1000;
    round.mark_run(&s, run);
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
    if let Some((ids, stats)) = host.block_stats(&vm.id, run) {
        record_disk_io(round, vm, &ids, &stats, disks, launched, run, now)?;
    }
    let Some(u) = usage else { return Ok(()) };
    // One run of the cgroup (or process) = one unit invocation of this
    // instance; the shim's start time tells two invocations apart.
    let reset_key = match &inst.runner {
        Runner::Systemd { .. } => format!("{run}/{}", inst.shim_starttime.unwrap_or(0)),
        Runner::Detached => format!("{run}/{}", inst.hypervisor_starttime.unwrap_or(0)),
    };
    if let Some(d) = round.counter(&s, "cpu.used", &reset_key, u.cpu_usec, now, Origin::ZeroAt(launched))? {
        // µs of CPU per second of wall time = % of one core × 10⁴; per vCPU.
        if let Some(usec_per_s) = d.rate(1) {
            round.live(&s, "cpu_percent", usec_per_s as f64 / 1e4 / (c.vcpu_count.max(1) as f64));
            round.live(&s, "cpu_cores", usec_per_s as f64 / 1e6);
            // Cores × 1000 over one sample interval (§2, §8.7).
            round.max(&s, "cpu.cores_peak", usec_per_s / 1000, now);
        }
    }
    // Hugepage-backed guest RAM is not in memory.current (§5.1).
    let used_mib = u.memory_bytes / MIB + if c.hugepages { c.mem_size_mib as u64 } else { 0 };
    round.gauge_run(&s, "mem.used", run, launched, used_mib, now, now, prev_end)?;
    round.max(&s, "mem.peak", used_mib, now);
    // Host-side I/O (D4): for capacity, next to the guest-level disk.*.
    if let Some(io) = u.io {
        for (m, v) in [
            ("vmio.read_bytes", io.read_bytes),
            ("vmio.write_bytes", io.write_bytes),
            ("vmio.read_ops", io.read_ops),
            ("vmio.write_ops", io.write_ops),
        ] {
            round.counter(&s, m, &reset_key, v, now, Origin::ZeroAt(launched))?;
        }
    }
    round.live(&s, "mem_used_mib", used_mib as f64);
    if c.mem_size_mib > 0 {
        round.live(&s, "mem_percent", used_mib as f64 * 100.0 / c.mem_size_mib as f64);
    }
    if u.from_proc {
        round.flag(&s, now, Flag::SourceProc);
    }
    Ok(())
}

/// The tail since the last sample, from the shim's exit snapshot (D12,
/// §6.4): CPU, the memory peak and the disks' counters, each consumed
/// once (`counter_final` adds nothing the second time).
pub fn consume_exit(round: &mut Round, vm: &Vm, e: &crate::models::ExitRecord, disks: &[Disk]) -> Result<(), MeteringError> {
    let Some(u) = &e.usage else { return Ok(()) };
    let s = vm_subject(vm);
    let at = e.at * 1000;
    let launched = e.launched_at * 1000;
    let seen = round.marked_run(&s)?.as_deref() == Some(e.instance_id.as_str());
    if let Some(cpu) = u.cpu_usage_usec {
        round.counter_exit(&s, "cpu.used", &e.instance_id, cpu, at, launched)?;
    }
    if !seen && launched > 0 {
        // Never sampled live: charge its allocation from launch to exit,
        // with the current sizing (pauses are unknown), once.
        let c = vm.config();
        for (m, level) in [("vm.running", 1), ("cpu.alloc", c.vcpu_count as u64), ("mem.alloc", c.mem_size_mib as u64)] {
            round.gauge_run(&s, m, &e.instance_id, launched, level, launched, at, None)?;
            round.gauge_end_at(&s, m, at)?;
        }
        round.flag(&s, at, Flag::Interpolated);
        round.mark_run(&s, &e.instance_id);
    }
    // `memory.peak` includes page cache, so it is not used for
    // `mem.peak` (working set, §5.1); the samples are.
    let disks_at = u.disks_at.unwrap_or(at);
    for b in &u.disks {
        let Some(Some(id)) = e.disk_ids.get(b.index) else { continue };
        let mut ds = match disks.iter().find(|d| &d.id == id) {
            Some(d) => disk_subject(d),
            None => Subject::new(SubjectKind::Disk, id.clone(), id.clone(), Some(vm.project.clone())),
        };
        ds.vm_id = Some(vm.id.clone());
        let mut counters = vec![
            ("disk.read_ops", b.read_ops),
            ("disk.write_ops", b.write_ops),
            ("disk.read_bytes", b.read_bytes),
            ("disk.write_bytes", b.write_bytes),
        ];
        counters.extend(b.read_time_ns.map(|t| ("disk.read_time_ns", t)));
        counters.extend(b.write_time_ns.map(|t| ("disk.write_time_ns", t)));
        for (m, v) in counters {
            round.counter_exit(&ds, m, &e.instance_id, v, disks_at, launched)?;
        }
    }
    Ok(())
}

/// Disk I/O of one instance (§5.2): each launched managed disk's
/// hypervisor counters, cumulative from launch (reset key: the instance),
/// with 30-second peaks per direction and for the total.
#[allow(clippy::too_many_arguments)]
pub fn record_disk_io(
    round: &mut Round,
    vm: &Vm,
    ids: &[Option<String>],
    stats: &[glidex_hv_client::stats::BlockStats],
    disks: &[Disk],
    launched: u64,
    run: &str,
    now: u64,
) -> Result<(), MeteringError> {
    for b in stats {
        let Some(Some(id)) = ids.get(b.index) else { continue }; // unmanaged root, seed
        let mut s = match disks.iter().find(|d| &d.id == id) {
            Some(d) => disk_subject(d),
            None => Subject::new(SubjectKind::Disk, id.clone(), id.clone(), Some(vm.project.clone())),
        };
        s.vm_id = Some(vm.id.clone());
        let origin = Origin::ZeroAt(launched);
        let rops = round.counter(&s, "disk.read_ops", run, b.read_ops, now, origin)?;
        let wops = round.counter(&s, "disk.write_ops", run, b.write_ops, now, origin)?;
        let rby = round.counter(&s, "disk.read_bytes", run, b.read_bytes, now, origin)?;
        let wby = round.counter(&s, "disk.write_bytes", run, b.write_bytes, now, origin)?;
        if let Some(t) = b.read_time_ns {
            round.counter(&s, "disk.read_time_ns", run, t, now, origin)?;
        }
        if let Some(t) = b.write_time_ns {
            round.counter(&s, "disk.write_time_ns", run, t, now, origin)?;
        }
        let rate = |d: Option<crate::metering::Delta>, f: u64| d.and_then(|d| d.rate(f));
        for (name, v) in [
            ("read_iops", rate(rops, 1000).map(|r| r as f64 / 1000.0)),
            ("write_iops", rate(wops, 1000).map(|r| r as f64 / 1000.0)),
            ("read_mbps", rate(rby, 1).map(|r| r as f64 / 1e6)),
            ("write_mbps", rate(wby, 1).map(|r| r as f64 / 1e6)),
        ] {
            if let Some(v) = v {
                round.live(&s, name, v);
            }
        }
        // ops/s × 1000 and kB/s (§2), per direction and summed.
        for (meter, deltas, factor, div) in [
            ("disk.read_iops_peak", vec![rops], 1000, 1),
            ("disk.write_iops_peak", vec![wops], 1000, 1),
            ("disk.iops_peak", vec![rops, wops], 1000, 1),
            ("disk.read_kBps_peak", vec![rby], 1, 1000),
            ("disk.write_kBps_peak", vec![wby], 1, 1000),
            ("disk.kBps_peak", vec![rby, wby], 1, 1000),
        ] {
            let ds: Vec<_> = deltas.into_iter().flatten().collect();
            if let Some(r) = super::net::summed_rate(&ds, factor) {
                round.max(&s, meter, r / div, now);
            }
        }
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
        let host = Host { proc_root: dir.path().join("proc"), cgroup_root: dir.path().join("cg"), sys_root: dir.path().join("sys") };
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
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 30_000).unwrap();
        l.commit(r).unwrap();
        cgroup(&host, 4_000_000, 768 * MIB);
        let mut r = l.begin_round(T0 + 60_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 60_000).unwrap();
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
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 30_000).unwrap();
        // Paused at +40 s, seen at +60 s.
        v.status.phase = VmPhase::Paused;
        v.status.phase_since = Some(launched + 40);
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 60_000).unwrap();
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
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 30_000).unwrap();
        // Exited at +45 s, seen at +90 s.
        v.status.phase = VmPhase::Stopped;
        v.status.instance = None;
        v.status.last_exit = serde_json::from_value(serde_json::json!({ "at": launched + 45, "instance_id": "i1", "cause": "clean_exit" })).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 90_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 120_000).unwrap();
        l.commit(r).unwrap();
        assert_eq!(totals(&l)["cpu.alloc"], 2 * 45);
    }

    /// D12: the shim's snapshot covers the tail between the last sample
    /// and the exit, once.
    #[test]
    fn exit_snapshot_covers_the_tail_once() {
        let (_dir, l, host) = setup();
        let launched = T0 / 1000;
        let mut v = vm("running", instance("i1", launched), serde_json::json!({}));
        cgroup(&host, 1_000_000, 512 * MIB);
        let mut r = l.begin_round(T0 + 30_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 30_000).unwrap();
        // Exits at +40 s having used 1.5 s of CPU in all; the cgroup is gone.
        v.status.phase = VmPhase::Stopped;
        v.status.instance = None;
        v.status.last_exit = serde_json::from_value(serde_json::json!({
            "at": launched + 40, "instance_id": "i1", "cause": "clean_exit",
            "usage": { "cpu_usage_usec": 1_500_000, "memory_peak_bytes": 900u64 << 20 },
        }))
        .unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 60_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 90_000).unwrap();
        l.commit(r).unwrap();
        let t = totals(&l);
        assert_eq!(t["cpu.used"], 1_500_000, "1.0 s sampled + 0.5 s tail, once");
        assert_eq!(t["cpu.alloc"], 2 * 40, "seen live: allocation is not charged again at exit");
        assert_eq!(t["mem.peak"], 512, "from the working-set samples, not the cgroup's cache-inclusive peak");
    }

    /// An instance that started and exited while the meter was down is
    /// charged from its snapshot: CPU in full, allocation launch → exit.
    #[test]
    fn unseen_instance_is_charged_from_its_snapshot() {
        let (_dir, l, host) = setup();
        let launched = T0 / 1000;
        let mut v = vm("stopped", serde_json::Value::Null, serde_json::json!({}));
        v.status.last_exit = serde_json::from_value(serde_json::json!({
            "at": launched + 100, "instance_id": "i9", "cause": "clean_exit", "launched_at": launched,
            "usage": { "cpu_usage_usec": 20_000_000 },
        }))
        .unwrap();
        let mut r = l.begin_round(T0 + 200_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 200_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 230_000).unwrap();
        l.commit(r).unwrap();
        let t = totals(&l);
        assert_eq!(t["cpu.used"], 20_000_000);
        assert_eq!((t["cpu.alloc"], t["mem.alloc"], t["vm.running"]), (2 * 100, 1024 * 100, 100));
    }

    #[test]
    fn foreign_cgroup_is_not_read() {
        let (_dir, l, host) = setup();
        // pid 42 now belongs to something else.
        write(&host.proc_root.join("42/cgroup"), "0::/user.slice/session-1.scope\n");
        let v = vm("running", instance("i1", T0 / 1000), serde_json::json!({}));
        let mut r = l.begin_round(T0 + 30_000).unwrap();
        sample_vms(&mut r, std::slice::from_ref(&v), &[], &host, T0 + 30_000).unwrap();
        l.commit(r).unwrap();
        assert!(!totals(&l).contains_key("cpu.used"));
    }

    #[test]
    fn disk_io_per_launched_disk_with_peaks() {
        use glidex_hv_client::stats::BlockStats;
        let (_dir, l, _) = setup();
        let v = vm("running", instance("i1", T0 / 1000), serde_json::json!({}));
        // Root managed (d-root), one data disk, then the seed.
        let ids = vec![Some("d-root".to_string()), Some("d-data".to_string()), None];
        let b = |i: usize, r: u64, w: u64, t: Option<u64>| BlockStats {
            index: i, read_ops: r, write_ops: w, read_bytes: r * 4096, write_bytes: w * 4096, read_time_ns: t, write_time_ns: t,
        };
        let mut r = l.begin_round(T0 + 60_000).unwrap();
        record_disk_io(&mut r, &v, &ids, &[b(0, 0, 0, Some(0)), b(1, 0, 0, None), b(2, 9, 9, None)], &[], T0, "i1", T0 + 30_000).unwrap();
        record_disk_io(&mut r, &v, &ids, &[b(0, 15_000, 3_000, Some(9_000_000)), b(1, 300, 0, None), b(2, 99, 99, None)], &[], T0, "i1", T0 + 60_000).unwrap();
        l.commit(r).unwrap();
        let rows = l.scan(0, u64::MAX / 10, None).unwrap();
        let get = |id: &str| combine(rows.iter().filter(|r| r.subject.id == id));
        let root = get("d-root");
        assert_eq!((root["disk.read_ops"], root["disk.write_ops"]), (15_000, 3_000));
        assert_eq!(root["disk.read_iops_peak"], 500_000, "15000 ops / 30 s = 500 IOPS (×1000)");
        assert_eq!(root["disk.iops_peak"], 600_000);
        assert_eq!(root["disk.kBps_peak"], 18_000 * 4096 / 30 / 1000);
        assert_eq!(root["disk.read_time_ns"], 9_000_000);
        assert!(!get("d-data").contains_key("disk.read_time_ns"), "no latency without a counter (CH, D17)");
        assert!(rows.iter().all(|r| r.subject.id != "2"), "the seed is not metered");
        assert!(rows.iter().filter(|r| r.subject.kind == SubjectKind::Disk).all(|r| r.subject.vm_id.as_deref() == Some("vm-1")));
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
