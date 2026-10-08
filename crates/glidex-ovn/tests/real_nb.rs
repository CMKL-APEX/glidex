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
use glidex_ovs::exec::SystemExec;

#[test]
#[ignore = "needs ovn-nbctl and a throwaway northbound database (GLIDEX_TEST_NB)"]
fn sync_makes_the_objects_and_is_idempotent() {
    let Ok(db) = std::env::var("GLIDEX_TEST_NB") else { panic!("set GLIDEX_TEST_NB to the NB database, e.g. unix:/var/run/ovn/ovnnb_db.sock") };
    let exec = SystemExec::new();
    let conn = NbConn { db: vec![db], key: "".into(), cert: "".into(), ca: "".into(), daemon: None };
    let nb = Nb::new(&exec, conn);
    let desired = Desired {
        edge: Some(EdgeSpec { physnet: "uplink".into(), external_ip: "192.0.2.50".parse().unwrap(), external_prefix: 24, gateway: "192.0.2.1".parse().unwrap(), gateway_nodes: vec!["chassis-a".into(), "chassis-b".into()], snat_ct_zone: None }),
        networks: vec![
            NetworkSpec { name: "gxtest-iso".into(), kind: NetKind::Isolated, cidr: Some("10.89.250.0/24".parse().unwrap()), dns: vec![], mtu: 1442, router: None },
            NetworkSpec { name: "gxtest-nat".into(), kind: NetKind::Nat, cidr: Some("10.89.251.0/24".parse().unwrap()), dns: vec!["192.0.2.53".parse().unwrap()], mtu: 1442, router: None },
        ],
        ports: vec![PortSpec { network: "gxtest-iso".into(), lport: "gxtest-0".into(), mac: "52:54:00:aa:bb:cc".into(), ip: Some("10.89.250.2".parse().unwrap()), vm_id: "vm-test".into(), nic: 0, chassis: Some("chassis-a".into()) }],
        node_addresses: vec!["192.0.2.11".parse().unwrap()],
        nat_supernet: Some("10.89.0.0/16".parse().unwrap()),
        routers: vec![],
    };
    let first = sync(&nb, &desired).unwrap();
    assert!(!first.changed.is_empty());
    let second = sync(&nb, &desired).unwrap();
    assert!(second.changed.is_empty(), "a second sync changed {:?}", second.changed);
    // Clean up: the plan with nothing in it removes what glidex made.
    sync(&nb, &Desired::default()).unwrap();
}
