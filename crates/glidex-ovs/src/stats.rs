//! Per-port traffic counters of glidex bridges, for metering
//! (spec/metering.md §5.4). Read-only: three `ovs-vsctl list` calls.

use crate::exec::Exec;
use crate::vsctl::{NIC_KEY, VM_ID_KEY};
use crate::vsctl::{self, Row};
use crate::OvsError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// What a port is to its bridge. The counter direction depends on it
/// (§5.4): OVS reports `vm` and `uplink` ports from the switch's point of
/// view and the bridge's internal (`gateway`) port from the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortRole {
    Vm,
    Uplink,
    /// The bridge's own internal port (the NAT gateway address).
    Gateway,
    Other,
}

/// One interface's counters, as OVS reports them (not swapped).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortStats {
    /// OVSDB Interface `_uuid`: a re-created port gets a new one, so it
    /// is the counters' reset key.
    pub uuid: String,
    pub name: String,
    pub role: PortRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nic: Option<u8>,
    /// The port's VLAN tag, if any (several networks may share a bridge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<u16>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
}

impl PortStats {
    /// Bytes and packets that entered the bridge through this port.
    pub fn ingress(&self) -> (u64, u64) {
        match self.role {
            PortRole::Gateway => (self.tx_bytes, self.tx_packets),
            _ => (self.rx_bytes, self.rx_packets),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeStats {
    pub bridge: String,
    pub ports: Vec<PortStats>,
}

/// `["uuid", "…"]` → `"…"`.
fn uuid(v: &Value) -> Option<String> {
    let a = v.as_array()?;
    (a.first()?.as_str()? == "uuid").then(|| a.get(1)?.as_str().map(str::to_string))?
}

/// A uuid, or a `["set", [uuid…]]`, as a list.
fn uuids(v: Option<&Value>) -> Vec<String> {
    let Some(v) = v else { return Vec::new() };
    if let Some(u) = uuid(v) {
        return vec![u];
    }
    v.as_array()
        .filter(|a| a.first().and_then(Value::as_str) == Some("set"))
        .and_then(|a| a.get(1)?.as_array())
        .map(|items| items.iter().filter_map(uuid).collect())
        .unwrap_or_default()
}

fn counter(stats: &BTreeMap<String, String>, key: &str) -> u64 {
    stats.get(key).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Counters of every port on every glidex-owned bridge.
pub fn bridge_stats(exec: &dyn Exec) -> Result<Vec<BridgeStats>, OvsError> {
    let bridges = vsctl::list(exec, "Bridge", &["name", "external_ids", "ports"])?;
    let ports = vsctl::list(exec, "Port", &["_uuid", "name", "interfaces", "tag"])?;
    let ifaces = vsctl::list(exec, "Interface", &["_uuid", "name", "type", "external_ids", "statistics"])?;
    Ok(assemble(&bridges, &ports, &ifaces))
}

fn assemble(bridges: &[Row], ports: &[Row], ifaces: &[Row]) -> Vec<BridgeStats> {
    let ports: BTreeMap<String, &Row> = ports.iter().filter_map(|p| Some((uuid(p.0.get("_uuid")?)?, p))).collect();
    let ifaces: BTreeMap<String, &Row> = ifaces.iter().filter_map(|i| Some((uuid(i.0.get("_uuid")?)?, i))).collect();
    let mut out = Vec::new();
    for b in bridges.iter().filter(|b| b.owned_by_glidex()) {
        let Some(name) = b.str("name") else { continue };
        let mut stats = Vec::new();
        for port in uuids(b.0.get("ports")).iter().filter_map(|u| ports.get(u)) {
            let tag = port.int("tag").and_then(|t| u16::try_from(t).ok());
            for (id, iface) in uuids(port.0.get("interfaces")).iter().filter_map(|u| Some((u, *ifaces.get(u)?))) {
                let ids = iface.map("external_ids");
                let iname = iface.str("name").unwrap_or_default();
                let role = match ids.get(vsctl::ROLE_KEY).map(String::as_str) {
                    Some("vm") => PortRole::Vm,
                    Some("uplink") => PortRole::Uplink,
                    _ if iface.str("type").as_deref() == Some("internal") && iname == name => PortRole::Gateway,
                    _ => PortRole::Other,
                };
                let st = iface.map("statistics");
                stats.push(PortStats {
                    uuid: id.clone(),
                    name: iname,
                    role,
                    vm_id: ids.get(VM_ID_KEY).cloned(),
                    nic: ids.get(NIC_KEY).and_then(|n| n.parse().ok()),
                    tag,
                    rx_bytes: counter(&st, "rx_bytes"),
                    tx_bytes: counter(&st, "tx_bytes"),
                    rx_packets: counter(&st, "rx_packets"),
                    tx_packets: counter(&st, "tx_packets"),
                    rx_dropped: counter(&st, "rx_dropped"),
                    tx_dropped: counter(&st, "tx_dropped"),
                });
            }
        }
        stats.sort_by(|a, b| a.name.cmp(&b.name));
        out.push(BridgeStats { bridge: name, ports: stats });
    }
    out.sort_by(|a, b| a.bridge.cmp(&b.bridge));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    // Recorded from a host with one NAT bridge and two VMs (2026-10-04),
    // plus an unowned bridge that must be ignored.
    const BRIDGES: &str = r#"{"data":[["gxbr-nat",["map",[["glidex-owner","glidex"],["glidex-role","bridge"]]],["set",[["uuid","48012139-ded3-410c-884c-a1e14f21dea4"],["uuid","b1f8f5a8-dfa5-4643-854e-14383a6be253"],["uuid","bb3bd583-bfa2-4c0c-806d-67562e6e34a7"]]]],["br-other",["map",[]],["uuid","00000000-0000-0000-0000-0000000000a1"]]],"headings":["name","external_ids","ports"]}"#;
    const PORTS: &str = r#"{"data":[[["uuid","b1f8f5a8-dfa5-4643-854e-14383a6be253"],"gxb7ab781b-0",["uuid","185230a4-b411-445c-92c3-b4e6f693e8c9"],["set",[]]],[["uuid","48012139-ded3-410c-884c-a1e14f21dea4"],"gx93c3868e-0",["uuid","369340db-b50a-41f9-adf2-d7f40917e49e"],12],[["uuid","bb3bd583-bfa2-4c0c-806d-67562e6e34a7"],"gxbr-nat",["uuid","dd348d51-3c03-485e-9d41-2d86fae740db"],["set",[]]],[["uuid","00000000-0000-0000-0000-0000000000a1"],"br-other",["uuid","00000000-0000-0000-0000-0000000000a2"],["set",[]]]],"headings":["_uuid","name","interfaces","tag"]}"#;
    const IFACES: &str = r#"{"data":[[["uuid","185230a4-b411-445c-92c3-b4e6f693e8c9"],"gxb7ab781b-0","",["map",[["glidex-nic","0"],["glidex-owner","glidex"],["glidex-role","vm"],["glidex-vm-id","b7ab781b-b407-4bd6-9c78-55e08f9ef731"]]],["map",[["collisions",0],["rx_bytes",1256080],["rx_dropped",0],["rx_packets",15719],["tx_bytes",43115123],["tx_dropped",0],["tx_packets",20024]]]],[["uuid","dd348d51-3c03-485e-9d41-2d86fae740db"],"gxbr-nat","internal",["map",[]],["map",[["rx_bytes",2420536],["rx_packets",37929],["tx_bytes",135802952],["tx_packets",49389]]]],[["uuid","369340db-b50a-41f9-adf2-d7f40917e49e"],"gx93c3868e-0","",["map",[["glidex-nic","0"],["glidex-owner","glidex"],["glidex-role","vm"],["glidex-vm-id","93c3868e-726e-436b-afcd-95b9c90f6c38"]]],["map",[["rx_bytes",882359],["rx_packets",11359],["tx_bytes",29749502],["tx_packets",13878]]]],[["uuid","00000000-0000-0000-0000-0000000000a2"],"br-other","internal",["map",[]],["map",[["rx_bytes",1]]]]],"headings":["_uuid","name","type","external_ids","statistics"]}"#;

    /// As root on a host with glidex bridges: `--ignored live_bridge_stats`.
    #[test]
    #[ignore = "needs root and a glidex bridge"]
    fn live_bridge_stats() {
        let stats = bridge_stats(&crate::SystemExec::default()).unwrap();
        assert!(!stats.is_empty());
        for b in &stats {
            for p in &b.ports {
                println!("{} {:<14} {:?} ingress={:?}", b.bridge, p.name, p.role, p.ingress());
            }
        }
    }

    #[test]
    fn owned_bridges_with_roles_and_tags() {
        let ex = RecordingExec::new();
        ex.on("ovs-vsctl --format=json --columns=name,external_ids,ports list Bridge", Output::ok(BRIDGES));
        ex.on("ovs-vsctl --format=json --columns=_uuid,name,interfaces,tag list Port", Output::ok(PORTS));
        ex.on("ovs-vsctl --format=json --columns=_uuid,name,type,external_ids,statistics list Interface", Output::ok(IFACES));
        let stats = bridge_stats(&ex).unwrap();
        assert_eq!(stats.len(), 1, "unowned bridges are left out");
        let b = &stats[0];
        assert_eq!(b.bridge, "gxbr-nat");
        let roles: Vec<_> = b.ports.iter().map(|p| (p.name.as_str(), p.role, p.tag, p.nic)).collect();
        assert_eq!(
            roles,
            vec![
                ("gx93c3868e-0", PortRole::Vm, Some(12), Some(0)),
                ("gxb7ab781b-0", PortRole::Vm, None, Some(0)),
                ("gxbr-nat", PortRole::Gateway, None, None),
            ]
        );
        let vm = &b.ports[1];
        assert_eq!(vm.vm_id.as_deref(), Some("b7ab781b-b407-4bd6-9c78-55e08f9ef731"));
        assert_eq!((vm.rx_bytes, vm.tx_bytes, vm.rx_packets, vm.tx_packets), (1256080, 43115123, 15719, 20024));
        // The gateway port reports the host's view: its tx is ingress.
        assert_eq!(b.ports[2].ingress(), (135802952, 49389));
        assert_eq!(vm.ingress(), (1256080, 15719));
    }
}
