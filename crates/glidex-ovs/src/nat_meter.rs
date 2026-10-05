//! External-traffic counters for NAT networks (spec/metering.md §5.5):
//! the `inet glidex_meter` table, separate from the NAT table so that
//! rebuilding `inet glidex` never resets them.
//!
//! **Invariant (D14):** this table is only ever changed incrementally:
//! counters are added (`add counter` is a no-op when it exists), the one
//! chain is flushed and refilled, and only counters whose reservation or
//! network is gone are deleted. The table itself is deleted by uninstall
//! alone. Named counters survive `flush chain` (verified, §15.3).

use crate::exec::{Cmd, Exec, Program};
use crate::nat::NatState;
use crate::OvsError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const METER_TABLE: &str = "glidex_meter";
/// Ethernet header bytes, added per packet so nft's L3 byte counts match
/// OVS's L2 frame counts (§5.5).
pub const ETHERNET_HEADER: u64 = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// From outside the network to it (guest rx).
    In,
    /// From the network to outside (guest tx).
    Out,
}

impl Direction {
    fn suffix(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

/// One counter, as reported by `nat_counters`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NatCounter {
    pub bridge: String,
    /// The VM NIC's MAC; `None` for the whole network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    pub dir: Direction,
    /// L2-normalized: nft bytes + 14 × packets.
    pub bytes: u64,
    pub packets: u64,
    /// nft object handle: a re-created counter gets a new one.
    pub handle: u64,
}

/// `br_<bridge>_<dir>`; bridge names are `[a-z0-9-]`, so mapping `-` to
/// `_` can't collide.
fn bridge_counter(bridge: &str, dir: Direction) -> String {
    format!("br_{}_{}", bridge.replace('-', "_"), dir.suffix())
}

/// `m_<mac hex>_<dir>`: reservations are keyed by MAC (the VM id isn't
/// recoverable from it; the control plane maps MAC → NIC).
fn mac_counter(mac: &str, dir: Direction) -> String {
    format!("m_{}_{}", mac.replace(':', "").to_ascii_lowercase(), dir.suffix())
}

/// Every counter the current NAT state needs, by name.
fn desired(nats: &[NatState]) -> BTreeMap<String, (String, Option<String>, Direction)> {
    let mut out = BTreeMap::new();
    for n in nats {
        for dir in [Direction::In, Direction::Out] {
            out.insert(bridge_counter(&n.bridge, dir), (n.bridge.clone(), None, dir));
            for mac in n.reservations.keys() {
                out.insert(mac_counter(mac, dir), (n.bridge.clone(), Some(mac.clone()), dir));
            }
        }
    }
    out
}

/// The incremental update for `nats`, given the counters that exist
/// (D14). `None` when there is nothing to do (no NAT, no table).
pub fn meter_script(nats: &[NatState], existing: &BTreeSet<String>) -> Option<String> {
    let want = desired(nats);
    if want.is_empty() && existing.is_empty() {
        return None;
    }
    let t = format!("inet {METER_TABLE}");
    let mut s = format!("add table {t}\n");
    s.push_str(&format!(
        "add chain {t} forward {{ type filter hook forward priority filter + 10; policy accept; }}\n"
    ));
    for name in want.keys() {
        s.push_str(&format!("add counter {t} {name}\n"));
    }
    s.push_str(&format!("flush chain {t} forward\n"));
    // After the NAT table's forward chain (priority filter) and other
    // filter chains at priority 0: dropped packets are never counted.
    // The rules have no verdict.
    for n in nats {
        s.push_str(&format!(
            "add rule {t} forward iifname \"{br}\" counter name \"{o}\"\nadd rule {t} forward oifname \"{br}\" counter name \"{i}\"\n",
            br = n.bridge,
            o = bridge_counter(&n.bridge, Direction::Out),
            i = bridge_counter(&n.bridge, Direction::In),
        ));
        for (mac, ip) in &n.reservations {
            // Out: keyed on MAC + reserved address (M0: `ether saddr`
            // matches in forward). In: already de-SNATed, from the gateway's
            // MAC, so the address alone.
            s.push_str(&format!(
                "add rule {t} forward iifname \"{br}\" ether saddr {mac} ip saddr {ip} counter name \"{o}\"\nadd rule {t} forward oifname \"{br}\" ip daddr {ip} counter name \"{i}\"\n",
                br = n.bridge,
                o = mac_counter(mac, Direction::Out),
                i = mac_counter(mac, Direction::In),
            ));
        }
    }
    for gone in existing.iter().filter(|n| !want.contains_key(*n)) {
        s.push_str(&format!("delete counter {t} {gone}\n"));
    }
    Some(s)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawCounter {
    bytes: u64,
    packets: u64,
    handle: u64,
}

/// The counters in `inet glidex_meter`; empty when the table doesn't exist.
fn list_counters(exec: &dyn Exec) -> Result<BTreeMap<String, RawCounter>, OvsError> {
    let out = exec.run(&Cmd::new(Program::Nft, ["-j", "list", "counters", "table", "inet", METER_TABLE]))?;
    if out.status != 0 {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("No such file or directory") || err.contains("does not exist") {
            return Ok(BTreeMap::new());
        }
        return Err(OvsError::Io(format!("nft list counters: {}", err.trim())));
    }
    parse_counters(&out.stdout_str())
}

fn parse_counters(json: &str) -> Result<BTreeMap<String, RawCounter>, OvsError> {
    if json.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| OvsError::Io(format!("unexpected nft output: {e}")))?;
    let mut out = BTreeMap::new();
    for item in v.get("nftables").and_then(|n| n.as_array()).into_iter().flatten() {
        let Some(c) = item.get("counter") else { continue };
        if c.get("table").and_then(|t| t.as_str()) != Some(METER_TABLE) {
            continue;
        }
        let num = |k: &str| c.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        if let Some(name) = c.get("name").and_then(|n| n.as_str()) {
            out.insert(name.to_string(), RawCounter { bytes: num("bytes"), packets: num("packets"), handle: num("handle") });
        }
    }
    Ok(out)
}

/// Bring `inet glidex_meter` in line with `nats` (D14).
pub fn apply_meter(exec: &dyn Exec, nats: &[NatState]) -> Result<(), OvsError> {
    let existing: BTreeSet<String> = list_counters(exec)?.into_keys().collect();
    if let Some(script) = meter_script(nats, &existing) {
        exec.check(&Cmd::new(Program::Nft, ["-f", "-"]).stdin(script))?;
    }
    Ok(())
}

/// The counters for the current NAT state, L2-normalized.
pub fn nat_counters(exec: &dyn Exec, nats: &[NatState]) -> Result<Vec<NatCounter>, OvsError> {
    let have = list_counters(exec)?;
    Ok(desired(nats)
        .into_iter()
        .filter_map(|(name, (bridge, mac, dir))| {
            let c = have.get(&name)?;
            Some(NatCounter {
                bridge,
                mac,
                dir,
                bytes: c.bytes + ETHERNET_HEADER * c.packets,
                packets: c.packets,
                handle: c.handle,
            })
        })
        .collect())
}

/// Uninstall: drop the table (the only place that does, D14).
pub fn drop_script() -> String {
    format!("table inet {t}\ndelete table inet {t}\n", t = METER_TABLE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};
    use crate::net::Ipv4Net;

    fn nat(bridge: &str, macs: &[(&str, &str)]) -> NatState {
        let mut n = NatState::new(bridge, "10.88.0.0/24".parse::<Ipv4Net>().unwrap(), true).unwrap();
        for (m, ip) in macs {
            n.reservations.insert(m.to_string(), ip.parse().unwrap());
        }
        n
    }

    /// Print a script for `nft -c -f -` checks on a real host.
    #[test]
    #[ignore = "prints a script for nft -c"]
    fn print_script_for_nft_check() {
        let n = nat("gxbr-nat", &[("02:ff:ef:a5:e8:72", "10.88.0.3"), ("02:a3:98:bb:9d:40", "10.88.0.5")]);
        let existing: BTreeSet<String> = ["m_020000000009_in".to_string()].into();
        print!("{}", meter_script(&[n], &BTreeSet::new()).unwrap());
        // A second, incremental update deleting a stale counter.
        let n2 = nat("gxbr-nat", &[("02:ff:ef:a5:e8:72", "10.88.0.3")]);
        print!("{}", meter_script(&[n2], &existing).unwrap().replace("delete counter inet glidex_meter m_020000000009_in\n", ""));
    }

    #[test]
    fn golden_script() {
        let s = meter_script(&[nat("gxbr-nat", &[("02:ff:ef:a5:e8:72", "10.88.0.3")])], &BTreeSet::new()).unwrap();
        assert_eq!(
            s,
            "add table inet glidex_meter\n\
             add chain inet glidex_meter forward { type filter hook forward priority filter + 10; policy accept; }\n\
             add counter inet glidex_meter br_gxbr_nat_in\n\
             add counter inet glidex_meter br_gxbr_nat_out\n\
             add counter inet glidex_meter m_02ffefa5e872_in\n\
             add counter inet glidex_meter m_02ffefa5e872_out\n\
             flush chain inet glidex_meter forward\n\
             add rule inet glidex_meter forward iifname \"gxbr-nat\" counter name \"br_gxbr_nat_out\"\n\
             add rule inet glidex_meter forward oifname \"gxbr-nat\" counter name \"br_gxbr_nat_in\"\n\
             add rule inet glidex_meter forward iifname \"gxbr-nat\" ether saddr 02:ff:ef:a5:e8:72 ip saddr 10.88.0.3 counter name \"m_02ffefa5e872_out\"\n\
             add rule inet glidex_meter forward oifname \"gxbr-nat\" ip daddr 10.88.0.3 counter name \"m_02ffefa5e872_in\"\n"
        );
    }

    /// D14: changing one network or reservation never deletes, re-creates
    /// or flushes another's counters, and never touches the table itself.
    #[test]
    fn updates_are_incremental() {
        let a = nat("gxbr-a", &[("02:00:00:00:00:01", "10.88.0.2"), ("02:00:00:00:00:02", "10.88.0.3")]);
        let b = nat("gxbr-b", &[("02:00:00:00:00:03", "10.88.1.2")]);
        let existing: BTreeSet<String> = desired(&[a.clone(), b.clone()]).into_keys().collect();
        // Release one reservation on a.
        let mut a2 = a.clone();
        a2.release("02:00:00:00:00:02");
        let s = meter_script(&[a2, b.clone()], &existing).unwrap();
        let deletes: Vec<&str> = s.lines().filter(|l| l.starts_with("delete")).collect();
        assert_eq!(
            deletes,
            vec!["delete counter inet glidex_meter m_020000000002_in", "delete counter inet glidex_meter m_020000000002_out"]
        );
        assert!(!s.contains("delete table") && !s.contains("flush table"));
        // Delete network a entirely: only a's counters go.
        let s = meter_script(&[b], &existing).unwrap();
        let deleted: BTreeSet<&str> = s.lines().filter_map(|l| l.strip_prefix("delete counter inet glidex_meter ")).collect();
        assert_eq!(
            deleted,
            ["br_gxbr_a_in", "br_gxbr_a_out", "m_020000000001_in", "m_020000000001_out", "m_020000000002_in", "m_020000000002_out"]
                .into_iter()
                .collect()
        );
        // Nothing at all: no table is created.
        assert_eq!(meter_script(&[], &BTreeSet::new()), None);
    }

    #[test]
    fn counters_are_normalized_and_mapped() {
        // Shape of `nft -j list counters table inet glidex_meter` (nft 1.1).
        let json = r#"{"nftables": [{"metainfo": {"version": "1.1.3", "json_schema_version": 1}},
            {"counter": {"family": "inet", "name": "br_gxbr_nat_out", "table": "glidex_meter", "handle": 3, "packets": 10, "bytes": 1000}},
            {"counter": {"family": "inet", "name": "m_02ffefa5e872_in", "table": "glidex_meter", "handle": 6, "packets": 2, "bytes": 3000}},
            {"counter": {"family": "inet", "name": "stale_one", "table": "glidex_meter", "handle": 9, "packets": 1, "bytes": 1}}]}"#;
        let ex = RecordingExec::new();
        ex.on("nft -j list counters table inet glidex_meter", Output::ok(json));
        let n = nat("gxbr-nat", &[("02:ff:ef:a5:e8:72", "10.88.0.3")]);
        let got = nat_counters(&ex, &[n]).unwrap();
        assert_eq!(
            got,
            vec![
                NatCounter { bridge: "gxbr-nat".into(), mac: None, dir: Direction::Out, bytes: 1000 + 140, packets: 10, handle: 3 },
                NatCounter {
                    bridge: "gxbr-nat".into(),
                    mac: Some("02:ff:ef:a5:e8:72".into()),
                    dir: Direction::In,
                    bytes: 3000 + 28,
                    packets: 2,
                    handle: 6
                },
            ]
        );
    }

    #[test]
    fn missing_table_is_empty_and_apply_creates_it() {
        let ex = RecordingExec::new();
        ex.on("nft -j list counters", Output::failed(1, "Error: No such file or directory; did you mean table 'glidex' in family inet?"));
        apply_meter(&ex, &[nat("gxbr-nat", &[])]).unwrap();
        let calls = ex.calls();
        assert!(calls[1].starts_with("nft -f - <<< add table inet glidex_meter\n"), "{calls:?}");
    }
}
