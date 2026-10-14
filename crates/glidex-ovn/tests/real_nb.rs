//! Against a real northbound database (spec/clustering.md §15). Ignored: it
//! needs `ovn-nbctl` and a database, e.g. a throwaway OVN on this host:
//!
//! ```text
//! GLIDEX_TEST_NB=unix:/var/run/ovn/ovnnb_db.sock cargo test -p glidex-ovn -- --ignored
//! ```
//!
//! It makes the objects of an isolated and a NAT network, a VM port and the
//! edge, checks `sync` is a no-op the second time, and deletes everything.

use glidex_ovn::*;
use glidex_ovs::exec::{Cmd, Exec, Output, SystemExec};
use glidex_ovs::OvsError;
use std::path::Path;

/// The system, with every command printed (run with --nocapture to see them).
struct Traced(SystemExec);

impl Exec for Traced {
    fn run(&self, cmd: &Cmd) -> Result<Output, OvsError> {
        let out = self.0.run(cmd);
        if std::env::var("GLIDEX_TEST_TRACE").is_ok() {
            eprintln!("$ {} -> {:?}", cmd.display(), out.as_ref().map(|o| o.status));
        }
        out
    }
    fn read_file(&self, path: &Path) -> Result<String, OvsError> {
        self.0.read_file(path)
    }
    fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), OvsError> {
        self.0.write_file(path, data)
    }
    fn remove_file(&self, path: &Path) -> Result<(), OvsError> {
        self.0.remove_file(path)
    }
    fn create_dir(&self, path: &Path, mode: u32) -> Result<(), OvsError> {
        self.0.create_dir(path, mode)
    }
    fn exists(&self, path: &Path) -> bool {
        self.0.exists(path)
    }
}

fn nbctl(db: &str, args: &[&str]) -> String {
    let out = std::process::Command::new("ovn-nbctl").arg(format!("--db={db}")).args(args).output().expect("ovn-nbctl");
    assert!(out.status.success(), "ovn-nbctl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn count(db: &str, table: &str, cond: &str) -> usize {
    nbctl(db, &["--bare", "--columns=_uuid", "find", table, cond]).lines().filter(|l| !l.trim().is_empty()).count()
}

#[test]
#[ignore = "needs ovn-nbctl and a throwaway northbound database (GLIDEX_TEST_NB)"]
fn sync_makes_the_objects_northd_accepts_them_and_cleans_up() {
    let Ok(db) = std::env::var("GLIDEX_TEST_NB") else { panic!("set GLIDEX_TEST_NB") };
    let exec = Traced(SystemExec::new());
    let nb = Nb::new(&exec, NbConn { db: vec![db.clone()], key: "".into(), cert: "".into(), ca: "".into(), daemon: None });
    let ext = ExternalSpec { physnet: "uplink".into(), external_ip: "192.0.2.64".parse().unwrap(), external_prefix: 24, gateway: "192.0.2.1".parse().unwrap(), gateway_nodes: vec!["chassis-b".into()], snat_ct_zone: Some(60001) };
    let net = |name: &str, kind: NetKind, cidr: Option<&str>, router: Option<&str>| NetworkSpec { name: name.into(), kind, cidr: cidr.map(|c| c.parse().unwrap()), dns: vec!["192.0.2.53".parse().unwrap()], mtu: 1442, router: router.map(String::from) };
    let desired = Desired {
        edge: Some(EdgeSpec { physnet: "uplink".into(), external_ip: "192.0.2.50".parse().unwrap(), external_prefix: 24, gateway: "192.0.2.1".parse().unwrap(), gateway_nodes: vec!["chassis-a".into(), "chassis-b".into()], snat_ct_zone: Some(60000) }),
        routers: vec![RouterSpec { name: "r1".into(), external: Some(ext) }, RouterSpec { name: "r2".into(), external: None }],
        networks: vec![
            net("gxt-iso", NetKind::Isolated, Some("10.89.240.0/24"), None),
            net("gxt-nat", NetKind::Nat, Some("10.89.241.0/24"), None),
            net("gxt-app", NetKind::Nat, Some("10.89.242.0/24"), Some("r1")),
            net("gxt-db", NetKind::Nat, Some("10.89.243.0/24"), Some("r1")),
            net("gxt-lan", NetKind::Provider { physnet: "lan".into(), vlan: Some(30) }, None, None),
        ],
        ports: vec![
            PortSpec { network: "gxt-iso".into(), lport: "gxt-0".into(), mac: "52:54:00:aa:bb:01".into(), ip: Some("10.89.240.2".parse().unwrap()), vm_id: "vm-a".into(), nic: 0, chassis: Some("chassis-a".into()) },
            PortSpec { network: "gxt-app".into(), lport: "gxt-1".into(), mac: "52:54:00:aa:bb:02".into(), ip: Some("10.89.242.2".parse().unwrap()), vm_id: "vm-b".into(), nic: 0, chassis: Some("chassis-b".into()) },
            PortSpec { network: "gxt-lan".into(), lport: "gxt-2".into(), mac: "52:54:00:aa:bb:03".into(), ip: None, vm_id: "vm-c".into(), nic: 0, chassis: Some("chassis-a".into()) },
        ],
        node_addresses: vec!["192.0.2.11".parse().unwrap(), "192.0.2.12".parse().unwrap()],
        nat_supernet: Some("10.89.0.0/16".parse().unwrap()),
    };
    let first = sync(&nb, &desired).unwrap_or_else(|e| panic!("first sync: {e:?}"));
    assert!(!first.changed.is_empty());
    let second = sync(&nb, &desired).unwrap_or_else(|e| panic!("second sync: {e:?}"));
    assert!(second.changed.is_empty(), "a second sync changed {:?}", second.changed);

    // What OVN now holds.
    assert_eq!(count(&db, "DHCP_Options", "external_ids:glidex-owner=glidex"), 4, "one DHCP row per routed or isolated network, none twice");
    let opts = nbctl(&db, &["--bare", "--columns=options", "find", "DHCP_Options", "external_ids:glidex-network=gxt-nat"]);
    assert!(opts.contains("router=10.89.241.1") && opts.contains("mtu=1442") && opts.contains("dns_server"), "{opts}");
    let iso = nbctl(&db, &["--bare", "--columns=options", "find", "DHCP_Options", "external_ids:glidex-network=gxt-iso"]);
    assert!(!iso.contains("router="), "an isolated network has no gateway: {iso}");
    assert!(!nbctl(&db, &["--bare", "--columns=dhcpv4_options", "list", "Logical_Switch_Port", "gxt-0"]).trim().is_empty(), "the VM port gets DHCP");
    let ps = nbctl(&db, &["--bare", "--columns=port_security", "list", "Logical_Switch_Port", "gxt-1"]);
    assert!(ps.contains("52:54:00:aa:bb:02 10.89.242.2"), "port security pins the address: {ps}");
    assert!(nbctl(&db, &["get", "Logical_Switch_Port", "gxt-1", "options:requested-chassis"]).contains("chassis-b"));
    let lan = nbctl(&db, &["show", "gx-gxt-lan"]);
    assert!(lan.contains("localnet"), "{lan}");
    assert!(nbctl(&db, &["--bare", "--columns=tag_request", "list", "Logical_Switch_Port", "gx-gxt-lan-ln"]).contains("30") || nbctl(&db, &["--bare", "--columns=tag", "list", "Logical_Switch_Port", "gx-gxt-lan-ln"]).contains("30"));
    let policies = nbctl(&db, &["lr-policy-list", "gxr-r1"]);
    assert!(policies.contains("$gx_nat_supernet && ip4.dst != $gx_r_r1_nets") && policies.contains("$gx_nodes"), "{policies}");
    assert!(nbctl(&db, &["lr-nat-list", "gxr-r1"]).contains("192.0.2.64"));
    assert!(nbctl(&db, &["lr-nat-list", "gx-edge"]).contains("10.89.241.0/24"));
    assert!(!nbctl(&db, &["lr-nat-list", "gx-edge"]).contains("10.89.242.0/24"), "a VPC network leaves through its own router");
    assert!(nbctl(&db, &["get", "Logical_Router", "gxr-r1", "options:snat-ct-zone"]).contains("60001"));
    assert!(nbctl(&db, &["get", "Logical_Router", "gx-edge", "options:snat-ct-zone"]).contains("60000"));
    let members = nbctl(&db, &["ha-chassis-group-list"]);
    assert!(members.contains("gx-edge") && members.contains("gxr-r1") && members.contains("chassis-a"), "{members}");
    assert_eq!(nbctl(&db, &["--bare", "--columns=addresses", "find", "Address_Set", "name=gx_r_r1_nets"]).split_whitespace().count(), 2);
    assert_eq!(count(&db, "Logical_Router", "name=gxr-r2"), 1, "a router without a gateway still exists");

    // northd compiles all of it into logical flows without complaint.
    if let Ok(log) = std::env::var("GLIDEX_TEST_NORTHD_LOG") {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let bad: Vec<&str> = text.lines().filter(|l| l.contains("|ERR|") || l.contains("|WARN|") && (l.contains("rror") || l.contains("parse"))).collect();
        assert!(bad.is_empty(), "northd: {bad:#?}");
    }
    if let Ok(sb) = std::env::var("GLIDEX_TEST_SB") {
        let out = std::process::Command::new("ovn-sbctl").arg(format!("--db={sb}")).args(["lflow-list", "gxr-r1"]).output().unwrap();
        let flows = String::from_utf8_lossy(&out.stdout);
        assert!(flows.contains("ip4.dst == $gx_nat_supernet"), "the isolation policy is a logical flow");
    }

    // A router that leaves the plan goes, with its group; then everything goes.
    let mut less = desired.clone();
    less.routers.retain(|r| r.name != "r1");
    less.networks.retain(|n| n.router.is_none());
    less.ports.retain(|p| p.network != "gxt-app");
    sync(&nb, &less).unwrap_or_else(|e| panic!("removing a router: {e:?}"));
    assert!(!nbctl(&db, &["ha-chassis-group-list"]).contains("gxr-r1"));
    sync(&nb, &Desired::default()).unwrap_or_else(|e| panic!("cleanup: {e:?}"));
    for (t, c) in [("Logical_Switch", "external_ids:glidex-owner=glidex"), ("Logical_Router", "external_ids:glidex-owner=glidex"), ("DHCP_Options", "external_ids:glidex-owner=glidex"), ("HA_Chassis_Group", "external_ids:glidex-owner=glidex")] {
        assert_eq!(count(&db, t, c), 0, "{t} left behind");
    }
}
