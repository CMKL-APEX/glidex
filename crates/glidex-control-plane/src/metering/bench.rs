//! Load targets (spec/metering.md §15.6): a sampling round at 200 VMs
//! (2 NICs, 2 disks each) within `sample_secs / 2`, and a one-project
//! month query under 200 ms. Run with
//! `cargo test --release --lib metering::bench -- --ignored --nocapture`.

use super::ledger::{Ledger, LedgerSettings, Origin, Subject, SubjectKind, HOUR_MS};
use super::query::{self, GroupKey, Granularity};
use super::rates::{bandwidth_p95, compute_p95, group_slots};
use redb::Database;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

const VMS: usize = 200;
/// 2026-10-01T00:00Z: the fleet launches and metering starts.
const T0: u64 = 1_790_812_800_000;
const PROJECTS: usize = 10;

struct Fleet {
    vms: Vec<Subject>,
    nics: Vec<Subject>,
    disks: Vec<Subject>,
    nets: Vec<Subject>,
}

fn fleet() -> Fleet {
    let p = |i: usize| Some(format!("p{}", i % PROJECTS));
    let mut f = Fleet { vms: vec![], nics: vec![], disks: vec![], nets: vec![] };
    for i in 0..VMS {
        f.vms.push(Subject::new(SubjectKind::Vm, format!("vm{i}"), format!("vm{i}"), p(i)));
        for n in 0..2 {
            let mut s = Subject::new(SubjectKind::Nic, format!("vm{i}.{n}"), format!("vm{i}/nic{n}"), p(i));
            s.vm_id = Some(format!("vm{i}"));
            s.network = Some(format!("net{}", i % PROJECTS));
            f.nics.push(s);
            let mut d = Subject::new(SubjectKind::Disk, format!("d{i}.{n}"), format!("d{i}.{n}"), p(i));
            d.vm_id = Some(format!("vm{i}"));
            f.disks.push(d);
        }
    }
    for k in 0..PROJECTS {
        f.nets.push(Subject::new(SubjectKind::Network, format!("net{k}"), format!("net{k}"), p(k)));
    }
    f
}

/// One round's worth of observations for the whole fleet at `t`.
fn observe(r: &mut super::Round, f: &Fleet, t: u64, step: u64) {
    for (i, s) in f.vms.iter().enumerate() {
        r.counter(s, "cpu.used", "i", step * 1_000_000 * (i as u64 % 4 + 1), t, Origin::ZeroAt(T0)).unwrap();
        for m in ["vm.running", "cpu.alloc", "mem.alloc", "mem.used"] {
            r.gauge_run(s, m, "i", T0, 512, 0, t, None).unwrap();
        }
        r.max(s, "mem.peak", 400, t);
    }
    for s in &f.nics {
        for m in ["net.rx_bytes", "net.tx_bytes", "net.ext_rx_bytes", "net.ext_tx_bytes", "net.rx_packets", "net.tx_packets"] {
            r.counter(s, m, "port", step * 3_750_000, t, Origin::ZeroAt(T0)).unwrap();
        }
        r.max(s, "net.rx_kbps_peak", 1000, t);
    }
    for s in &f.disks {
        for m in ["disk.read_ops", "disk.write_ops", "disk.read_bytes", "disk.write_bytes", "disk.read_time_ns", "disk.write_time_ns"] {
            r.counter(s, m, "i", step * 15_000, t, Origin::ZeroAt(T0)).unwrap();
        }
        r.gauge_run(s, "disk.alloc", "disk", T0, 10240, 0, t, None).unwrap();
        r.max(s, "disk.iops_peak", 500_000, t);
    }
    for (k, s) in f.nets.iter().enumerate() {
        r.counter_part(s, "bridge.bytes", &format!("port{k}"), "u", step * 7_500_000, t, Origin::ZeroAt(T0)).unwrap();
    }
}

#[test]
#[ignore = "benchmark: run in release with --ignored --nocapture"]
fn load_targets() {
    // On disk, not /tmp (often a RAM-backed tmpfs).
    let dir = tempfile::TempDir::new_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
    let path = dir.path().join("bench.db");
    let l = Ledger::new(Arc::new(Database::create(&path).unwrap()), LedgerSettings::from_secs(30, 120)).unwrap();
    l.set_started_at_for_test(T0);
    let f = fleet();
    let t0 = T0;

    // A month of history: one sample every 5 minutes (fills every slot).
    let gen = Instant::now();
    // GLIDEX_BENCH_DAYS shortens the history (default: a whole month).
    let days: usize = std::env::var("GLIDEX_BENCH_DAYS").ok().and_then(|d| d.parse().ok()).unwrap_or(31);
    let slots_per_month = days * 24 * 12;
    let mut slowest_round = std::time::Duration::ZERO;
    for step in 0..=slots_per_month as u64 {
        let t = t0 + step * 300_000;
        let start = Instant::now();
        let mut r = l.begin_round(t).unwrap();
        observe(&mut r, &f, t, step);
        l.commit(r).unwrap();
        slowest_round = slowest_round.max(start.elapsed());
        if step % 1000 == 0 {
            let mib = std::fs::metadata(&path).map(|m| m.len() >> 20).unwrap_or(0);
            println!("  round {step}: {:?} so far, last round {:?}, {mib} MiB", gen.elapsed(), start.elapsed());
        }
    }
    let db_mib = std::fs::metadata(&path).map(|m| m.len() >> 20).unwrap_or(0);
    println!("history: {} rounds in {:?}; slowest round {:?}; database {} MiB", slots_per_month + 1, gen.elapsed(), slowest_round, db_mib);
    for (name, rows, bytes) in l.table_sizes() {
        println!("  {name:<14} {rows:>8} rows {:>7} MiB  {:>5} B/row", bytes >> 20, bytes.checked_div(rows).unwrap_or(0));
    }

    // A sampling round at full size, after a month of history.
    let t = t0 + (slots_per_month as u64 + 1) * 300_000;
    let start = Instant::now();
    let mut r = l.begin_round(t).unwrap();
    observe(&mut r, &f, t, slots_per_month as u64 + 1);
    l.commit(r).unwrap();
    let round = start.elapsed();
    println!("one round (200 VMs, 400 NICs, 400 disks, 10 networks): {round:?}");

    // One project's month: usage by VM, and bandwidth p95 by VM.
    let (from, to) = (t0 / 1000, t0 / 1000 + days as u64 * 86400);
    let only: BTreeSet<String> = ["p3".to_string()].into();
    let mut times = vec![];
    for _ in 0..5 {
        let start = Instant::now();
        let rows = l.scan(from, to, Some(&only)).unwrap();
        let q = query::Query {
            from,
            to,
            granularity: Granularity::Month,
            tz: query::Tz::utc(),
            group_by: vec![GroupKey::Project, GroupKey::Vm],
            meters: None,
            now: to,
        };
        let out = query::aggregate(&rows, &q);
        assert_eq!(out.len(), VMS / PROJECTS + 1, "20 VMs + the network");
        times.push(start.elapsed());
    }
    times.sort();
    println!("usage query, one project, one month: median {:?}, max {:?}", times[2], times[4]);

    let start = Instant::now();
    let slots = l.scan_slots(from, to, Some(&only), |s| s.kind == SubjectKind::Nic).unwrap();
    let groups = group_slots(&slots, from, to, &[GroupKey::Vm]);
    let p95: Vec<_> = groups.values().map(|g| bandwidth_p95(g, SubjectKind::Nic)).collect();
    let bw = start.elapsed();
    assert_eq!(p95.len(), VMS / PROJECTS);
    println!("bandwidth p95, one project, one month by VM: {bw:?} ({} slots each)", p95[0].slots.counted);

    let start = Instant::now();
    let slots = l.scan_slots(from, to, Some(&only), |s| s.kind == SubjectKind::Vm).unwrap();
    let groups = group_slots(&slots, from, to, &[GroupKey::Vm]);
    let cm: Vec<_> = groups.values().map(compute_p95).collect();
    println!("CPU and memory p95, one project, one month by VM: {:?} ({} slots each)", start.elapsed(), cm[0].slots.counted);
    assert_eq!(cm.len(), VMS / PROJECTS);

    assert!(round.as_millis() < 15_000, "round within sample_secs / 2");
    assert!(times[2].as_millis() < 200, "usage query under 200 ms (median {:?})", times[2]);
    let _ = HOUR_MS;
}
