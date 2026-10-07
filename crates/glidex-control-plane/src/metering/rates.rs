//! 95th percentiles over 5-minute slots (spec/metering.md §8.5.1, §8.6,
//! D15, D16): series per group, summed slot by slot, nearest rank.

use super::ledger::{SlotRow, SubjectKind, SLOTS_PER_HOUR, SLOT_MS};
use super::query::{key_of, names_itself, GroupKey, Named};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

const SLOT_SECS: u64 = SLOT_MS / 1000;

/// Nearest-rank 95th percentile: the value at 1-based rank ⌈0.95 N⌉ of
/// the sorted values.
pub fn p95(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let rank = (values.len() * 95).div_ceil(100);
    Some(v[rank.max(1) - 1])
}

/// One group's slots: slot start (unix s) → series → summed delta.
#[derive(Debug, Default)]
pub struct GroupSlots {
    pub keys: BTreeMap<GroupKey, Named>,
    pub slots: BTreeMap<u64, BTreeMap<String, u64>>,
    pub interpolated: BTreeSet<u64>,
}

impl GroupSlots {
    /// One series per slot; slots the group was present in but the series
    /// has no delta count as 0.
    fn series(&self, f: impl Fn(&BTreeMap<String, u64>) -> u64) -> Vec<u64> {
        self.slots.values().map(f).collect()
    }
}

/// Group slot rows over slot starts in `[from, to)` (unix s). A slot is
/// counted for a group if any member was present in it (§8.5.1).
pub fn group_slots(rows: &[(u64, SlotRow)], from: u64, to: u64, group_by: &[GroupKey]) -> BTreeMap<Vec<Option<String>>, GroupSlots> {
    let mut out: BTreeMap<Vec<Option<String>>, GroupSlots> = BTreeMap::new();
    for (hour, row) in rows {
        let Some(s) = &row.subject else { continue };
        let keys: Vec<Option<Named>> = group_by.iter().map(|k| key_of(s, *k)).collect();
        let ids = keys.iter().map(|k| k.as_ref().map(|n| n.id.clone())).collect();
        let g = out.entry(ids).or_default();
        for (k, v) in group_by.iter().zip(keys) {
            if let Some(v) = v {
                if names_itself(*k, s.kind) || !g.keys.contains_key(k) {
                    g.keys.insert(*k, v);
                }
            }
        }
        for i in 0..SLOTS_PER_HOUR {
            let start = hour + i as u64 * SLOT_SECS;
            if row.present & (1 << i) == 0 || start < from || start >= to {
                continue;
            }
            let slot = g.slots.entry(start).or_default();
            for (m, v) in &row.series {
                *slot.entry(m.clone()).or_insert(0) += v[i];
            }
            if row.interpolated & (1 << i) != 0 {
                g.interpolated.insert(start);
            }
        }
    }
    out
}

/// Bytes in one slot → Mbps.
fn mbps(bytes: u64) -> f64 {
    round3(bytes as f64 * 8.0 / SLOT_SECS as f64 / 1e6)
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SlotCounts {
    pub counted: usize,
    pub interpolated: usize,
}

/// 95th percentiles of network traffic for one group (§8.5.1). Billable
/// is the larger direction. For networks (`bridge.*`) the total has one
/// series, bridge ingress.
#[derive(Debug, Serialize, PartialEq)]
pub struct BandwidthP95 {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_mbps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_mbps: Option<f64>,
    pub billable_mbps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext_rx_mbps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext_tx_mbps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext_billable_mbps: Option<f64>,
    pub slots: SlotCounts,
}

pub fn bandwidth_p95(g: &GroupSlots, kind: SubjectKind) -> BandwidthP95 {
    let has = |m: &str| g.slots.values().any(|s| s.contains_key(m));
    let p = |m: &str| has(m).then(|| p95(&g.series(|s| s.get(m).copied().unwrap_or(0))).map(mbps)).flatten();
    let max = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, y) => x.or(y),
    };
    let slots = SlotCounts { counted: g.slots.len(), interpolated: g.interpolated.len() };
    if kind == SubjectKind::Network {
        let (er, et) = (p("bridge.ext_rx_bytes"), p("bridge.ext_tx_bytes"));
        return BandwidthP95 {
            rx_mbps: None,
            tx_mbps: None,
            billable_mbps: p("bridge.bytes"),
            ext_rx_mbps: er,
            ext_tx_mbps: et,
            ext_billable_mbps: max(er, et),
            slots,
        };
    }
    let (rx, tx, er, et) = (p("net.rx_bytes"), p("net.tx_bytes"), p("net.ext_rx_bytes"), p("net.ext_tx_bytes"));
    BandwidthP95 { rx_mbps: rx, tx_mbps: tx, billable_mbps: max(rx, tx), ext_rx_mbps: er, ext_tx_mbps: et, ext_billable_mbps: max(er, et), slots }
}

/// 95th percentiles of disk I/O for one group (§8.6, D16): billable is
/// the p95 of read + write per slot, not the sum of their p95s.
#[derive(Debug, Serialize, PartialEq)]
pub struct DiskIoP95 {
    pub read_iops: Option<f64>,
    pub write_iops: Option<f64>,
    pub billable_iops: Option<f64>,
    pub read_mbps: Option<f64>,
    pub write_mbps: Option<f64>,
    pub billable_mbps: Option<f64>,
    pub slots: SlotCounts,
}

pub fn disk_io_p95(g: &GroupSlots) -> DiskIoP95 {
    let get = |s: &BTreeMap<String, u64>, m: &str| s.get(m).copied().unwrap_or(0);
    let iops = |v: Option<u64>| v.map(|o| round3(o as f64 / SLOT_SECS as f64));
    // Disk throughput in MB/s (decimal, §8.3).
    let mbs = |v: Option<u64>| v.map(|b| round3(b as f64 / SLOT_SECS as f64 / 1e6));
    DiskIoP95 {
        read_iops: iops(p95(&g.series(|s| get(s, "disk.read_ops")))),
        write_iops: iops(p95(&g.series(|s| get(s, "disk.write_ops")))),
        billable_iops: iops(p95(&g.series(|s| get(s, "disk.read_ops") + get(s, "disk.write_ops")))),
        read_mbps: mbs(p95(&g.series(|s| get(s, "disk.read_bytes")))),
        write_mbps: mbs(p95(&g.series(|s| get(s, "disk.write_bytes")))),
        billable_mbps: mbs(p95(&g.series(|s| get(s, "disk.read_bytes") + get(s, "disk.write_bytes")))),
        slots: SlotCounts { counted: g.slots.len(), interpolated: g.interpolated.len() },
    }
}

/// 95th percentiles of CPU and memory for a group of VMs (§8.7, D18):
/// cores and MiB used, and utilization (used ÷ allocated) per slot, of
/// the group's sums slot by slot.
#[derive(Debug, Serialize, PartialEq)]
pub struct ComputeP95 {
    pub cpu_cores: Option<f64>,
    pub cpu_percent: Option<f64>,
    pub mem_mib: Option<f64>,
    pub mem_percent: Option<f64>,
    pub slots: SlotCounts,
}

/// Utilization of one slot × 1000 (so 37.5 % is 37 500): `used` per
/// `alloc`, with `scale` turning the units into a percentage. `None`
/// when nothing was allocated (a paused VM has no `cpu.alloc`).
fn percent_milli(used: u64, alloc: u64, scale: u128) -> Option<u64> {
    (alloc > 0).then(|| (used as u128 * 100_000 / (alloc as u128 * scale)) as u64)
}

/// CPU % of one slot: µs of CPU over vCPU·s × 10⁶.
fn cpu_percent_milli(s: &BTreeMap<String, u64>) -> Option<u64> {
    percent_milli(s.get("cpu.used").copied().unwrap_or(0), s.get("cpu.alloc").copied().unwrap_or(0), 1_000_000)
}

/// Memory % of one slot: MiB·s used over MiB·s allocated.
fn mem_percent_milli(s: &BTreeMap<String, u64>) -> Option<u64> {
    percent_milli(s.get("mem.used").copied().unwrap_or(0), s.get("mem.alloc").copied().unwrap_or(0), 1)
}

pub fn compute_p95(g: &GroupSlots) -> ComputeP95 {
    let has = |m: &str| g.slots.values().any(|s| s.contains_key(m));
    let get = |s: &BTreeMap<String, u64>, m: &str| s.get(m).copied().unwrap_or(0);
    let pct = |f: fn(&BTreeMap<String, u64>) -> Option<u64>| {
        let v: Vec<u64> = g.slots.values().filter_map(f).collect();
        p95(&v).map(|x| round3(x as f64 / 1000.0))
    };
    ComputeP95 {
        cpu_cores: has("cpu.used").then(|| p95(&g.series(|s| get(s, "cpu.used"))).map(|u| round3(u as f64 / SLOT_SECS as f64 / 1e6))).flatten(),
        cpu_percent: pct(cpu_percent_milli),
        mem_mib: has("mem.used").then(|| p95(&g.series(|s| get(s, "mem.used"))).map(|m| round3(m as f64 / SLOT_SECS as f64))).flatten(),
        mem_percent: pct(mem_percent_milli),
        slots: SlotCounts { counted: g.slots.len(), interpolated: g.interpolated.len() },
    }
}

/// Which rates a series shows (§9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesKind {
    Network,
    Disk,
    Compute,
}

/// The 5-minute series of a group, for graphs: slot start → rates.
pub fn series_points(g: &GroupSlots, kind: SeriesKind) -> Vec<serde_json::Value> {
    g.slots
        .iter()
        .map(|(start, s)| {
            let get = |m: &str| s.get(m).copied();
            let mut o = serde_json::Map::new();
            o.insert("slot".into(), super::query::rfc3339(*start).into());
            if kind == SeriesKind::Compute {
                if let Some(u) = get("cpu.used") {
                    o.insert("cpu_cores".into(), round3(u as f64 / SLOT_SECS as f64 / 1e6).into());
                }
                if let Some(m) = get("mem.used") {
                    o.insert("mem_mib".into(), round3(m as f64 / SLOT_SECS as f64).into());
                }
                for (k, v) in [("cpu_percent", cpu_percent_milli(s)), ("mem_percent", mem_percent_milli(s))] {
                    if let Some(v) = v {
                        o.insert(k.into(), round3(v as f64 / 1000.0).into());
                    }
                }
            } else if kind == SeriesKind::Disk {
                for (m, k) in [("disk.read_ops", "read_iops"), ("disk.write_ops", "write_iops")] {
                    o.insert(k.into(), round3(get(m).unwrap_or(0) as f64 / SLOT_SECS as f64).into());
                }
                for (m, k) in [("disk.read_bytes", "read_mbps"), ("disk.write_bytes", "write_mbps")] {
                    o.insert(k.into(), round3(get(m).unwrap_or(0) as f64 / SLOT_SECS as f64 / 1e6).into());
                }
                for (t, ops, k) in [("disk.read_time_ns", "disk.read_ops", "read_latency_ms"), ("disk.write_time_ns", "disk.write_ops", "write_latency_ms")] {
                    if let (Some(t), Some(o_)) = (get(t), get(ops)) {
                        if o_ > 0 {
                            o.insert(k.into(), round3(t as f64 / o_ as f64 / 1e6).into());
                        }
                    }
                }
            } else {
                for (m, k) in [
                    ("net.rx_bytes", "rx_mbps"),
                    ("net.tx_bytes", "tx_mbps"),
                    ("net.ext_rx_bytes", "ext_rx_mbps"),
                    ("net.ext_tx_bytes", "ext_tx_mbps"),
                    ("bridge.bytes", "mbps"),
                    ("bridge.ext_rx_bytes", "ext_rx_mbps"),
                    ("bridge.ext_tx_bytes", "ext_tx_mbps"),
                ] {
                    if let Some(b) = get(m) {
                        o.insert(k.into(), mbps(b).into());
                    }
                }
            }
            o.into()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::ledger::Subject;
    use super::*;

    fn row(kind: SubjectKind, id: &str, vm: Option<&str>, present: u16, series: &[(&str, [u64; 12])]) -> SlotRow {
        let mut s = Subject::new(kind, id, id, Some("p1".into()));
        s.vm_id = vm.map(String::from);
        SlotRow { subject: Some(s), present, interpolated: 0, series: series.iter().map(|(m, v)| (m.to_string(), *v)).collect() }
    }

    #[test]
    fn nearest_rank() {
        assert_eq!(p95(&[]), None);
        assert_eq!(p95(&[7]), Some(7));
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(p95(&v), Some(95));
        // 8640 slots (30 days) with the top 5% (432) at 1000: dropped.
        let mut v = vec![10u64; 8640 - 432];
        v.extend(vec![1000u64; 432]);
        assert_eq!(p95(&v), Some(10));
        v.push(1000); // one more burst slot tips it over.
        assert_eq!(p95(&v), Some(1000));
        assert_eq!(p95(&[0, 0, 0]), Some(0));
    }

    #[test]
    fn group_p95_is_of_the_summed_series() {
        // Two NICs of one VM peak in different slots: the group's p95 is
        // below the sum of their p95s (D15).
        let mut a = [0u64; 12];
        let mut b = [0u64; 12];
        a[0] = 37_500_000_000; // 1000 Mbps for one slot
        b[6] = 37_500_000_000;
        let rows = vec![
            (0, row(SubjectKind::Nic, "vm.0", Some("vm"), 0xfff, &[("net.rx_bytes", a)])),
            (0, row(SubjectKind::Nic, "vm.1", Some("vm"), 0xfff, &[("net.rx_bytes", b)])),
        ];
        let g = group_slots(&rows, 0, 3600, &[GroupKey::Vm]);
        assert_eq!(g.len(), 1);
        let bw = bandwidth_p95(g.values().next().unwrap(), SubjectKind::Nic);
        // 12 slots: rank ⌈11.4⌉ = 12 → the max of the summed series, 1000.
        assert_eq!(bw.rx_mbps, Some(1000.0));
        assert_eq!(bw.slots, SlotCounts { counted: 12, interpolated: 0 });
        // Over 24 slots (two hours, the second idle), rank 23 is 1000;
        // per NIC each p95 would be 0 → sum 0. The group sees the bursts.
        let mut rows2 = rows.clone();
        rows2.push((3600, row(SubjectKind::Nic, "vm.0", Some("vm"), 0xfff, &[("net.rx_bytes", [0; 12])])));
        let g = group_slots(&rows2, 0, 7200, &[GroupKey::Vm]);
        assert_eq!(bandwidth_p95(g.values().next().unwrap(), SubjectKind::Nic).rx_mbps, Some(1000.0));
    }

    #[test]
    fn billable_network_is_the_larger_direction_and_disk_the_total() {
        let mut rx = [3_750_000_000u64; 12]; // 100 Mbps
        rx[0] = 0;
        let tx = [7_500_000_000u64; 12]; // 200 Mbps
        let g = group_slots(&[(0, row(SubjectKind::Nic, "n", None, 0xfff, &[("net.rx_bytes", rx), ("net.tx_bytes", tx)]))], 0, 3600, &[GroupKey::Nic]);
        let bw = bandwidth_p95(g.values().next().unwrap(), SubjectKind::Nic);
        assert_eq!((bw.rx_mbps, bw.tx_mbps, bw.billable_mbps), (Some(100.0), Some(200.0), Some(200.0)));
        assert_eq!(bw.ext_rx_mbps, None, "not measured is absent, not zero");
        // Reads in slots 0-5, writes in 6-11: the total never exceeds 700.
        let mut r = [0u64; 12];
        let mut w = [0u64; 12];
        for i in 0..12 {
            r[i] = if i < 6 { 210_000 } else { 0 }; // 700 IOPS
            w[i] = if i < 6 { 0 } else { 90_000 }; // 300 IOPS
        }
        let g = group_slots(&[(0, row(SubjectKind::Disk, "d", None, 0xfff, &[("disk.read_ops", r), ("disk.write_ops", w)]))], 0, 3600, &[GroupKey::Disk]);
        let io = disk_io_p95(g.values().next().unwrap());
        assert_eq!((io.read_iops, io.write_iops, io.billable_iops), (Some(700.0), Some(300.0), Some(700.0)));
        assert_ne!(io.billable_iops.unwrap(), io.read_iops.unwrap() + io.write_iops.unwrap(), "not the sum of p95s");
    }

    #[test]
    fn compute_p95_sums_vms_and_skips_unallocated_slots() {
        // Two 2-vCPU VMs: one busy (1.5 cores) in slots 0-5, the other in
        // 6-11. Each slot: 1.5 cores of 4 allocated = 37.5 %.
        let busy = 450_000_000u64; // 1.5 cores × 300 s, in µs
        let alloc = [600u64; 12]; // 2 vCPU × 300 s
        let mut a = [0u64; 12];
        let mut b = [0u64; 12];
        for i in 0..12 {
            if i < 6 { a[i] = busy } else { b[i] = busy }
        }
        let mem = [512 * 300u64; 12]; // 512 MiB held
        let mem_alloc = [1024 * 300u64; 12];
        let rows = vec![
            (0, row(SubjectKind::Vm, "a", Some("a"), 0xfff, &[("cpu.used", a), ("cpu.alloc", alloc), ("mem.used", mem), ("mem.alloc", mem_alloc)])),
            (0, row(SubjectKind::Vm, "b", Some("b"), 0xfff, &[("cpu.used", b), ("cpu.alloc", alloc), ("mem.used", mem), ("mem.alloc", mem_alloc)])),
        ];
        let g = group_slots(&rows, 0, 3600, &[GroupKey::Project]);
        let c = compute_p95(g.values().next().unwrap());
        // The project never uses more than 1.5 cores at once (not 3, D15).
        assert_eq!((c.cpu_cores, c.cpu_percent), (Some(1.5), Some(37.5)));
        assert_eq!((c.mem_mib, c.mem_percent), (Some(1024.0), Some(50.0)));
        // Paused (no cpu.alloc) slots have no CPU utilization, not 0 %.
        let mut paused = alloc;
        paused[..11].fill(0);
        let rows = vec![(0, row(SubjectKind::Vm, "a", Some("a"), 0xfff, &[("cpu.used", [300_000; 12]), ("cpu.alloc", paused)]))];
        let g = group_slots(&rows, 0, 3600, &[GroupKey::Vm]);
        let c = compute_p95(g.values().next().unwrap());
        assert_eq!(c.cpu_percent, Some(0.05), "only the allocated slot: 1 ms/s of 2 vCPUs");
        assert_eq!(c.mem_percent, None, "no memory meters");
        let p = series_points(g.values().next().unwrap(), SeriesKind::Compute);
        assert_eq!(p[0].get("cpu_percent"), None);
        assert_eq!(p[11]["cpu_cores"], 0.001);
    }

    #[test]
    fn only_present_slots_count() {
        // A NIC attached for 3 of 12 slots: 3 counted, not diluted by 9 zeros.
        let g = group_slots(&[(0, row(SubjectKind::Nic, "n", None, 0b111, &[("net.rx_bytes", [3_750_000_000; 12])]))], 0, 3600, &[GroupKey::Nic]);
        let bw = bandwidth_p95(g.values().next().unwrap(), SubjectKind::Nic);
        assert_eq!((bw.slots.counted, bw.rx_mbps), (3, Some(100.0)));
    }
}
