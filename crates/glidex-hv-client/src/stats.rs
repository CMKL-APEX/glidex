//! Per-disk block counters (spec/metering.md §5.2): Cloud Hypervisor's
//! `GET /vm.counters` and QEMU's `query-blockstats`. Both are cumulative
//! from the instance's launch.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One guest disk's counters. `index` is its position in the launched
/// disk list (`[root, data disks…, seed]`): CH names it `_disk<i>`, glidex
/// gives QEMU `id=vd<i>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockStats {
    pub index: usize,
    pub read_ops: u64,
    pub write_ops: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    /// Cumulative time to complete requests, ns. QEMU only: CH v53
    /// reports a lifetime running mean that can't be metered (D17).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_time_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_time_ns: Option<u64>,
}

/// CH `vm.counters`: `{"_disk0": {...}, "net0": {...}}`. The
/// `*_latency_*` fields are ignored (D17), so their `u64::MAX`
/// "no data" sentinel never reaches a meter.
pub fn parse_ch_counters(v: &Value) -> Vec<BlockStats> {
    let mut out: Vec<BlockStats> = v
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(dev, c)| {
            let index = dev.strip_prefix("_disk")?.parse().ok()?;
            let n = |k: &str| c.get(k).and_then(Value::as_u64);
            Some(BlockStats {
                index,
                read_ops: n("read_ops")?,
                write_ops: n("write_ops")?,
                read_bytes: n("read_bytes")?,
                write_bytes: n("write_bytes")?,
                read_time_ns: None,
                write_time_ns: None,
            })
        })
        .collect();
    out.sort_by_key(|b| b.index);
    out
}

/// QMP `query-blockstats`: guest disks are
/// `/machine/peripheral/vd<i>/virtio-backend`; firmware `pflash` and
/// anything else is skipped.
pub fn parse_qmp_blockstats(v: &Value) -> Vec<BlockStats> {
    let mut out: Vec<BlockStats> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let qdev = e.get("qdev")?.as_str()?;
            let index = qdev.strip_prefix("/machine/peripheral/vd")?.split('/').next()?.parse().ok()?;
            let st = e.get("stats")?;
            let n = |k: &str| st.get(k).and_then(Value::as_u64);
            Some(BlockStats {
                index,
                read_ops: n("rd_operations")?,
                write_ops: n("wr_operations")?,
                read_bytes: n("rd_bytes")?,
                write_bytes: n("wr_bytes")?,
                read_time_ns: n("rd_total_time_ns"),
                write_time_ns: n("wr_total_time_ns"),
            })
        })
        .collect();
    out.sort_by_key(|b| b.index);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ch_v53_counters_recorded_live() {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/ch_v53_vm_counters.json")).unwrap();
        let s = parse_ch_counters(&v);
        assert_eq!(s.iter().map(|b| b.index).collect::<Vec<_>>(), vec![0, 1], "net0 is not a disk");
        assert_eq!((s[0].read_ops, s[0].write_ops), (12826, 11432));
        assert_eq!((s[0].read_bytes, s[0].write_bytes), (464455168, 1087837696));
        assert!(s.iter().all(|b| b.read_time_ns.is_none() && b.write_time_ns.is_none()));
        // The second disk had no writes: its latency sentinels are ignored.
        assert_eq!(s[1].write_ops, 0);
    }

    #[test]
    fn qemu_blockstats_recorded_live() {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/qemu_10.2_query_blockstats.json")).unwrap();
        let s = parse_qmp_blockstats(&v);
        assert_eq!(s.iter().map(|b| b.index).collect::<Vec<_>>(), vec![0, 1], "pflash is skipped");
        assert!(s[0].read_ops > 0 && s[0].read_time_ns.unwrap() > 0);
        assert!(s.iter().all(|b| b.write_time_ns.is_some()));
    }

    #[test]
    fn sorted_and_tolerant() {
        let v = serde_json::json!({
            "_disk10": {"read_ops": 1, "write_ops": 2, "read_bytes": 3, "write_bytes": 4},
            "_disk2": {"read_ops": 5, "write_ops": 6, "read_bytes": 7, "write_bytes": 8},
            "_disk3": {"read_ops": 1},
        });
        assert_eq!(parse_ch_counters(&v).iter().map(|b| b.index).collect::<Vec<_>>(), vec![2, 10]);
        assert!(parse_qmp_blockstats(&serde_json::json!({"x": 1})).is_empty());
    }
}
