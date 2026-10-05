//! Network meters from bridge-port counters (spec/metering.md §5.4):
//! per VM NIC (`net.*`) and per network (`bridge.*`).

use super::ledger::{Delta, MeteringError, Origin, Round, Subject, SubjectKind};
use crate::models::Vm;
use crate::network::Network;
use glidex_ovs::nat_meter::{Direction, NatCounter};
use glidex_ovs::stats::{BridgeStats, PortRole, PortStats};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A network's bridge ports, each with where its counters start.
type Ingress<'a> = (Subject, Vec<(&'a PortStats, Origin)>);

/// The VM NIC subject: `<vm id>.<nic>`.
pub fn nic_subject(vm: &Vm, nic: u8, network: Option<&str>) -> Subject {
    let mut s = Subject::new(SubjectKind::Nic, format!("{}.{}", vm.id, nic), format!("{}/nic{}", vm.name, nic), Some(vm.project.clone()));
    s.vm_id = Some(vm.id.clone());
    s.nic = Some(nic as u32);
    s.network = network.map(str::to_string);
    s
}

fn network_subject(n: &Network) -> Subject {
    let mut s = Subject::new(SubjectKind::Network, n.name.clone(), n.name.clone(), n.project.clone());
    s.network = Some(n.name.clone());
    s
}

/// Traffic on a bridge that no single network owns (a bridge shared by
/// several VLAN networks: its uplink; or a bridge with no network).
fn bridge_subject(bridge: &str) -> Subject {
    Subject::new(SubjectKind::Network, format!("bridge:{bridge}"), bridge.to_string(), None)
}

/// `(rate per second × factor)` of a sum of deltas over one interval,
/// or `None` when they don't share it or it spans a gap (§8.5).
fn summed_rate(deltas: &[Delta], factor: u64) -> Option<u64> {
    let first = deltas.first()?;
    if deltas.iter().any(|d| d.from != first.from || d.to != first.to) {
        return None;
    }
    let sum = Delta { amount: deltas.iter().map(|d| d.amount).sum(), ..*first };
    sum.rate(factor)
}

pub fn sample_ports(round: &mut Round, bridges: &[BridgeStats], vms: &[Vm], networks: &[Network], now: u64) -> Result<(), MeteringError> {
    let vms: HashMap<&str, &Vm> = vms.iter().map(|v| (v.id.as_str(), v)).collect();
    for b in bridges {
        let nets: Vec<&Network> = networks.iter().filter(|n| n.bridge == b.bridge).collect();
        // Per-network ingress: subject → [(port, origin)].
        let mut ingress: BTreeMap<String, Ingress> = BTreeMap::new();
        for p in &b.ports {
            let vm = match (p.role, p.vm_id.as_deref()) {
                (PortRole::Vm, Some(vm_id)) => vms.get(vm_id).copied(),
                _ => None,
            };
            // A VM port counts from its VM's launch; any other port first
            // seen takes a baseline.
            let origin = vm.and_then(|v| v.status.instance.as_ref()).map_or(Origin::Unknown, |i| Origin::ZeroAt(i.launched_at * 1000));
            let nic_net = match (vm, p.nic) {
                (Some(vm), Some(nic)) => {
                    let net = vm.status.nics.iter().find(|n| n.nic_index == nic).map(|n| n.network.as_str());
                    sample_nic(round, vm, nic, net, p, origin, now)?;
                    net
                }
                _ => None,
            };
            // Whose bridge total this port's ingress belongs to.
            let owner = match nets.as_slice() {
                [only] => network_subject(only),
                _ => match nic_net.and_then(|name| nets.iter().find(|n| n.name == name)) {
                    Some(n) => network_subject(n),
                    None => bridge_subject(&b.bridge),
                },
            };
            ingress.entry(owner.id.clone()).or_insert_with(|| (owner, Vec::new())).1.push((p, origin));
        }
        for (_, (subject, ports)) in ingress {
            sample_bridge(round, &subject, &ports, now)?;
        }
    }
    Ok(())
}

/// External traffic of NAT networks (§5.5): per VM NIC by reserved MAC,
/// and per network. `boot_id` makes counter handles unique across host
/// reboots (a recreated table can reuse handle numbers).
pub fn sample_nat(round: &mut Round, counters: &[NatCounter], vms: &[Vm], networks: &[Network], boot_id: &str, now: u64) -> Result<(), MeteringError> {
    // MAC → (VM, NIC index, network).
    let mut by_mac: HashMap<String, (&Vm, u8, &str)> = HashMap::new();
    for vm in vms {
        for n in &vm.status.nics {
            by_mac.insert(n.mac.to_ascii_lowercase(), (vm, n.nic_index, n.network.as_str()));
        }
    }
    for c in counters {
        let reset = format!("{boot_id}/{}", c.handle);
        let dir = match c.dir {
            Direction::In => "rx",
            Direction::Out => "tx",
        };
        match &c.mac {
            Some(mac) => {
                // A reservation whose VM isn't known (deleted): nobody to bill.
                let Some((vm, nic, net)) = by_mac.get(&mac.to_ascii_lowercase()) else { continue };
                let s = nic_subject(vm, *nic, Some(net));
                let origin = vm.status.instance.as_ref().map_or(Origin::Unknown, |i| Origin::ZeroAt(i.launched_at * 1000));
                let d = round.counter(&s, &format!("net.ext_{dir}_bytes"), &reset, c.bytes, now, origin)?;
                round.counter(&s, &format!("net.ext_{dir}_packets"), &reset, c.packets, now, origin)?;
                if let Some(kbps) = d.and_then(|d| d.rate(8)) {
                    round.max(&s, &format!("net.ext_{dir}_kbps_peak"), kbps / 1000, now);
                }
            }
            None => {
                let s = match networks.iter().find(|n| n.bridge == c.bridge) {
                    Some(n) => network_subject(n),
                    None => bridge_subject(&c.bridge),
                };
                let d = round.counter(&s, &format!("bridge.ext_{dir}_bytes"), &reset, c.bytes, now, Origin::Unknown)?;
                round.counter(&s, &format!("bridge.ext_{dir}_packets"), &reset, c.packets, now, Origin::Unknown)?;
                if let Some(kbps) = d.and_then(|d| d.rate(8)) {
                    round.max(&s, &format!("bridge.ext_{dir}_kbps_peak"), kbps / 1000, now);
                }
            }
        }
    }
    Ok(())
}

fn sample_nic(round: &mut Round, vm: &Vm, nic: u8, network: Option<&str>, p: &PortStats, origin: Origin, now: u64) -> Result<(), MeteringError> {
    let s = nic_subject(vm, nic, network);
    // The switch's view, swapped to the guest's (§2).
    let rx = round.counter(&s, "net.rx_bytes", &p.uuid, p.tx_bytes, now, origin)?;
    let tx = round.counter(&s, "net.tx_bytes", &p.uuid, p.rx_bytes, now, origin)?;
    let rxp = round.counter(&s, "net.rx_packets", &p.uuid, p.tx_packets, now, origin)?;
    let txp = round.counter(&s, "net.tx_packets", &p.uuid, p.rx_packets, now, origin)?;
    if let Some(kbps) = rx.and_then(|d| d.rate(8)) {
        round.max(&s, "net.rx_kbps_peak", kbps / 1000, now);
    }
    if let Some(kbps) = tx.and_then(|d| d.rate(8)) {
        round.max(&s, "net.tx_kbps_peak", kbps / 1000, now);
    }
    if let Some(pps) = summed_rate(&[rxp, txp].into_iter().flatten().collect::<Vec<_>>(), 1000) {
        round.max(&s, "net.pps_peak", pps, now);
    }
    Ok(())
}

fn sample_bridge(round: &mut Round, s: &Subject, ports: &[(&PortStats, Origin)], now: u64) -> Result<(), MeteringError> {
    let mut bytes = Vec::new();
    for (p, origin) in ports {
        let (b, pk) = p.ingress();
        if let Some(d) = round.counter_part(s, "bridge.bytes", &p.uuid, &p.uuid, b, now, *origin)? {
            bytes.push(d);
        }
        round.counter_part(s, "bridge.packets", &p.uuid, &p.uuid, pk, now, *origin)?;
    }
    let keep: BTreeSet<String> = ports.iter().map(|(p, _)| p.uuid.clone()).collect();
    round.prune_parts(s, "bridge.bytes", &keep)?;
    round.prune_parts(s, "bridge.packets", &keep)?;
    if let Some(kbps) = summed_rate(&bytes, 8) {
        round.max(s, "bridge.kbps_peak", kbps / 1000, now);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::ledger::{combine, Ledger, LedgerSettings, HOUR_MS};
    use super::*;
    use redb::Database;
    use std::sync::Arc;

    const T0: u64 = 1_791_000_000 / 3600 * HOUR_MS;

    fn vm(id: &str, nets: &[&str]) -> Vm {
        let nics: Vec<_> = nets
            .iter()
            .enumerate()
            .map(|(i, n)| serde_json::json!({ "network": n, "nic_index": i, "mac": "02:00:00:00:00:01" }))
            .collect();
        Vm {
            id: id.into(),
            name: format!("{id}-name"),
            project: "p1".into(),
            created_at: 0,
            generation: 1,
            resource_version: 1,
            deletion_requested_at: None,
            finalizers: vec![],
            spec: serde_json::from_value(serde_json::json!({
                "config": { "vcpu_count": 1, "mem_size_mib": 512, "rootfs_path": "/r", "kernel_args": "" }
            }))
            .unwrap(),
            status: serde_json::from_value(serde_json::json!({
                "phase": "running",
                "nics": nics,
                "instance": { "instance_id": "i1", "runner": { "kind": "detached" }, "boot_id": "b",
                              "launched_generation": 1, "launched_at": T0 / 1000 },
            }))
            .unwrap(),
        }
    }

    fn net(name: &str, bridge: &str, vlan: Option<u16>) -> Network {
        serde_json::from_value(serde_json::json!({
            "name": name, "bridge": bridge, "mode": "nat", "port_type": "tap", "vlan": vlan,
            "created_at": 0, "project": "p1",
        }))
        .unwrap()
    }

    fn port(uuid: &str, role: PortRole, vm: Option<(&str, u8)>, rx: u64, tx: u64) -> PortStats {
        PortStats {
            uuid: uuid.into(),
            name: uuid.into(),
            role,
            vm_id: vm.map(|v| v.0.to_string()),
            nic: vm.map(|v| v.1),
            tag: None,
            rx_bytes: rx,
            tx_bytes: tx,
            rx_packets: rx / 100,
            tx_packets: tx / 100,
            rx_dropped: 0,
            tx_dropped: 0,
        }
    }

    fn setup() -> (tempfile::TempDir, Ledger) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Database::create(dir.path().join("t.db")).unwrap());
        let l = Ledger::new(db, LedgerSettings::from_secs(30, 120)).unwrap();
        l.set_started_at_for_test(0);
        (dir, l)
    }

    fn by_subject(l: &Ledger) -> BTreeMap<String, BTreeMap<String, u64>> {
        let rows = l.scan(0, u64::MAX / 10, None).unwrap();
        let mut out: BTreeMap<String, Vec<_>> = BTreeMap::new();
        for r in rows {
            out.entry(r.subject.id.clone()).or_default().push(r);
        }
        out.into_iter().map(|(k, v)| (k, combine(&v))).collect()
    }

    /// VM a sends 1 MB to VM b on the same NAT bridge; b downloads 3 MB
    /// from outside through the gateway (§2: the two views).
    #[test]
    fn nic_and_bridge_views() {
        let (_d, l) = setup();
        let vms = [vm("a", &["nat"]), vm("b", &["nat"])];
        let nets = [net("nat", "gxbr-nat", None)];
        let snap = |a_rx: u64, b_rx: u64, b_tx: u64, gw_tx: u64| {
            vec![BridgeStats {
                bridge: "gxbr-nat".into(),
                ports: vec![
                    // OVS view: a's rx = what a sent; b's tx = what b got.
                    port("pa", PortRole::Vm, Some(("a", 0)), a_rx, 0),
                    port("pb", PortRole::Vm, Some(("b", 0)), b_rx, b_tx),
                    // Gateway reports the host's view: tx = into the bridge.
                    port("gw", PortRole::Gateway, None, 0, gw_tx),
                ],
            }]
        };
        let mut r = l.begin_round(T0 + 30_000).unwrap();
        sample_ports(&mut r, &snap(0, 0, 0, 0), &vms, &nets, T0 + 30_000).unwrap();
        sample_ports(&mut r, &snap(1_000_000, 0, 4_000_000, 3_000_000), &vms, &nets, T0 + 60_000).unwrap();
        l.commit(r).unwrap();
        let t = by_subject(&l);
        assert_eq!(t["a.0"]["net.tx_bytes"], 1_000_000);
        assert_eq!(t["b.0"]["net.rx_bytes"], 4_000_000, "1 MB from a + 3 MB from outside");
        // Each frame once: a's 1 MB entering at pa, 3 MB entering at the gateway.
        assert_eq!(t["nat"]["bridge.bytes"], 4_000_000);
        // 4 MB in 30 s = 1066 kbps (integer).
        assert_eq!(t["nat"]["bridge.kbps_peak"], 1066);
        assert_eq!(t["b.0"]["net.rx_kbps_peak"], 1066);
    }

    #[test]
    fn shared_bridge_splits_vm_ports_by_network() {
        let (_d, l) = setup();
        let vms = [vm("a", &["red"]), vm("b", &["blue"])];
        let nets = [net("red", "gxbr-up", Some(10)), net("blue", "gxbr-up", Some(20))];
        let snap = |v: u64| {
            vec![BridgeStats {
                bridge: "gxbr-up".into(),
                ports: vec![
                    port("pa", PortRole::Vm, Some(("a", 0)), v, 0),
                    port("pb", PortRole::Vm, Some(("b", 0)), 2 * v, 0),
                    port("up", PortRole::Uplink, None, 5 * v, 0),
                ],
            }]
        };
        let mut r = l.begin_round(T0 + 60_000).unwrap();
        sample_ports(&mut r, &snap(0), &vms, &nets, T0 + 30_000).unwrap();
        sample_ports(&mut r, &snap(100), &vms, &nets, T0 + 60_000).unwrap();
        l.commit(r).unwrap();
        let t = by_subject(&l);
        assert_eq!(t["red"]["bridge.bytes"], 100);
        assert_eq!(t["blue"]["bridge.bytes"], 200);
        assert_eq!(t["bridge:gxbr-up"]["bridge.bytes"], 500, "the shared uplink is host-level");
        assert_eq!(by_subject(&l)["bridge:gxbr-up"].len(), 3, "bytes, packets, peak");
    }

    #[test]
    fn nat_external_counters_per_nic_and_network() {
        let (_d, l) = setup();
        let mut a = vm("a", &["nat"]);
        a.status.nics[0].mac = "02:FF:EF:A5:E8:72".into();
        let vms = [a];
        let nets = [net("nat", "gxbr-nat", None)];
        let snap = |vm_in: u64, br_in: u64, handle: u64| {
            vec![
                NatCounter { bridge: "gxbr-nat".into(), mac: Some("02:ff:ef:a5:e8:72".into()), dir: Direction::In, bytes: vm_in, packets: vm_in / 1000, handle },
                NatCounter { bridge: "gxbr-nat".into(), mac: None, dir: Direction::In, bytes: br_in, packets: br_in / 1000, handle: handle + 10 },
                // A reservation of a deleted VM: skipped.
                NatCounter { bridge: "gxbr-nat".into(), mac: Some("02:00:00:00:00:99".into()), dir: Direction::Out, bytes: 5, packets: 1, handle: 7 },
            ]
        };
        let mut r = l.begin_round(T0 + 90_000).unwrap();
        sample_nat(&mut r, &snap(0, 0, 4), &vms, &nets, "boot1", T0 + 30_000).unwrap();
        sample_nat(&mut r, &snap(3_000_000, 3_500_000, 4), &vms, &nets, "boot1", T0 + 60_000).unwrap();
        // Host rebooted: same handle numbers, new boot id → counts from 0.
        sample_nat(&mut r, &snap(1_000, 1_000, 4), &vms, &nets, "boot2", T0 + 90_000).unwrap();
        l.commit(r).unwrap();
        let t = by_subject(&l);
        assert_eq!(t["a.0"]["net.ext_rx_bytes"], 3_001_000);
        assert_eq!(t["a.0"]["net.ext_rx_kbps_peak"], 800);
        assert_eq!(t["nat"]["bridge.ext_rx_bytes"], 3_501_000);
        assert!(!t["a.0"].contains_key("net.ext_tx_bytes"));
    }

    #[test]
    fn a_recreated_port_counts_from_zero() {
        let (_d, l) = setup();
        let vms = [vm("a", &["nat"])];
        let nets = [net("nat", "gxbr-nat", None)];
        let one = |uuid: &str, tx: u64| vec![BridgeStats { bridge: "gxbr-nat".into(), ports: vec![port(uuid, PortRole::Vm, Some(("a", 0)), 0, tx)] }];
        let mut r = l.begin_round(T0 + 90_000).unwrap();
        sample_ports(&mut r, &one("p1", 0), &vms, &nets, T0 + 30_000).unwrap();
        sample_ports(&mut r, &one("p1", 500), &vms, &nets, T0 + 60_000).unwrap();
        // Detached and re-attached: new uuid, counters restart.
        sample_ports(&mut r, &one("p2", 70), &vms, &nets, T0 + 90_000).unwrap();
        l.commit(r).unwrap();
        assert_eq!(by_subject(&l)["a.0"]["net.rx_bytes"], 570);
    }
}
