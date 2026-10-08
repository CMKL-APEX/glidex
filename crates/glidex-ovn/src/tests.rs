use super::*;
use glidex_ovs::exec::{Output, RecordingExec};

fn conn() -> NbConn {
    NbConn { db: vec!["ssl:192.0.2.11:6641".into(), "ssl:192.0.2.12:6641".into()], key: "/k".into(), cert: "/c".into(), ca: "/ca".into(), daemon: None }
}

fn net(name: &str, kind: NetKind, cidr: &str) -> NetworkSpec {
    NetworkSpec { name: name.into(), kind, cidr: Some(cidr.parse().unwrap()), dns: vec!["192.0.2.53".parse().unwrap()], mtu: 1442, router: None }
}

fn empty(cols: &str) -> Output {
    Output::ok(format!(r#"{{"data":[],"headings":{cols}}}"#))
}

#[test]
fn commands_carry_the_connection_or_the_daemon() {
    let exec = RecordingExec::new();
    Nb::new(&exec, conn()).txn(vec![vec!["ls-add".into(), "x".into()], vec!["ls-add".into(), "y".into()]]).unwrap();
    assert_eq!(exec.calls(), vec!["ovn-nbctl --db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641 -p /k -c /c -C /ca ls-add x -- ls-add y"]);
    let exec = RecordingExec::new();
    let mut c = conn();
    c.daemon = Some("/run/ovn/nbctl.ctl".into());
    Nb::new(&exec, c).txn(vec![vec!["ls-add".into(), "x".into()]]).unwrap();
    assert_eq!(exec.calls(), vec!["ovn-nbctl ls-add x"], "the daemon already holds the connection");
}

#[test]
fn an_isolated_network_is_a_switch_with_dhcp_and_no_router() {
    let exec = RecordingExec::new();
    exec.on("ovn-nbctl --db=", empty(r#"["name"]"#));
    let d = Desired { networks: vec![net("lab", NetKind::Isolated, "10.89.3.0/24")], ..Default::default() };
    let rep = sync(&Nb::new(&exec, conn()), &d).unwrap();
    assert!(rep.changed.contains(&"network lab".to_string()), "{rep:?}");
    let calls = exec.calls().join("\n");
    assert!(calls.contains("--may-exist ls-add gx-lab"), "{calls}");
    assert!(calls.contains("set Logical_Switch gx-lab external_ids:glidex-owner=glidex external_ids:glidex-network=lab"), "{calls}");
    assert!(calls.contains("dhcp-options-create 10.89.3.0/24 external_ids:glidex-owner=glidex external_ids:glidex-network=lab"), "{calls}");
    assert!(!calls.contains("lrp-add") && !calls.contains("lr-nat-add"), "an isolated network has no router: {calls}");
}

#[test]
fn a_nat_network_is_routed_through_the_edge_with_snat_and_the_isolation_policies() {
    let exec = RecordingExec::new();
    exec.on("ovn-nbctl --db=", empty(r#"["name"]"#));
    // The group doesn't exist yet; after it is made, it has an id.
    let find_group = "ovn-nbctl --db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641 -p /k -c /c -C /ca --format=json --columns=_uuid,name,external_ids,ha_chassis find HA_Chassis_Group";
    exec.on(find_group, empty(r#"["_uuid","name","external_ids","ha_chassis"]"#));
    exec.on(
        "ovn-nbctl --db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641 -p /k -c /c -C /ca --format=json --columns=_uuid find HA_Chassis_Group",
        Output::ok(r#"{"data":[[["uuid","g1"]]],"headings":["_uuid"]}"#),
    );
    let d = Desired {
        edge: Some(EdgeSpec { physnet: "uplink".into(), external_ip: "192.0.2.50".parse().unwrap(), external_prefix: 24, gateway: "192.0.2.1".parse().unwrap(), gateway_nodes: vec!["n1".into(), "n2".into()] }),
        networks: vec![net("web", NetKind::Nat, "10.89.1.0/24")],
        node_addresses: vec!["192.0.2.11".parse().unwrap(), "192.0.2.12".parse().unwrap()],
        nat_supernet: Some("10.89.0.0/16".parse().unwrap()),
        ..Default::default()
    };
    sync(&Nb::new(&exec, conn()), &d).unwrap();
    let calls = exec.calls().join("\n");
    for want in [
        "create Address_Set name=gx_nodes addresses=\"192.0.2.11\",\"192.0.2.12\" external_ids:glidex-owner=glidex",
        "create Address_Set name=gx_nat_supernet addresses=\"10.89.0.0/16\"",
        "--may-exist ls-add gx-ext-uplink",
        "lsp-set-options gx-ext-uplink-ln network_name=uplink",
        "--may-exist lr-add gx-edge",
        "--may-exist lrp-add gx-edge gx-edge-ext",
        "192.0.2.50/24",
        "--may-exist lr-route-add gx-edge 0.0.0.0/0 192.0.2.1",
        "ha-chassis-group-add gx-edge",
        "ha-chassis-group-add-chassis gx-edge n1 100",
        "ha-chassis-group-add-chassis gx-edge n2 99",
        "set Logical_Router_Port gx-edge-ext ha_chassis_group=g1",
        "--may-exist lr-policy-add gx-edge 1100 ip4.dst == $gx_nodes drop",
        "--may-exist lr-policy-add gx-edge 1000 ip4.dst == $gx_nat_supernet drop",
        "--may-exist lrp-add gx-edge gx-lrp-web",
        "10.89.1.1/24",
        "lsp-set-options gx-web-rt router-port=gx-lrp-web",
        "--may-exist lr-nat-add gx-edge snat 192.0.2.50 10.89.1.0/24",
    ] {
        assert!(calls.contains(want), "missing {want:?} in:\n{calls}");
    }
    // The node policy outranks the supernet policy.
    assert!(calls.find("1100").unwrap() < calls.find("1000 ip4.dst").unwrap());
}

#[test]
fn a_vm_port_has_port_security_and_is_bound_to_its_node() {
    let exec = RecordingExec::new();
    exec.on("ovn-nbctl --db=", empty(r#"["name"]"#));
    exec.on(
        "ovn-nbctl --db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641 -p /k -c /c -C /ca --format=json --columns=_uuid find DHCP_Options",
        Output::ok(r#"{"data":[[["uuid","aaaa"]]],"headings":["_uuid"]}"#),
    );
    let d = Desired {
        networks: vec![net("lab", NetKind::Isolated, "10.89.3.0/24")],
        ports: vec![PortSpec { network: "lab".into(), lport: "gx1a2b3c4d-0".into(), mac: "52:54:00:aa:bb:cc".into(), ip: Some("10.89.3.7".parse().unwrap()), vm_id: "vm1".into(), nic: 0, chassis: Some("n1".into()) }],
        ..Default::default()
    };
    sync(&Nb::new(&exec, conn()), &d).unwrap();
    let calls = exec.calls().join("\n");
    for want in [
        "--may-exist lsp-add gx-lab gx1a2b3c4d-0",
        "lsp-set-addresses gx1a2b3c4d-0 52:54:00:aa:bb:cc 10.89.3.7",
        "lsp-set-port-security gx1a2b3c4d-0 52:54:00:aa:bb:cc 10.89.3.7",
        "lsp-set-options gx1a2b3c4d-0 requested-chassis=n1",
        "lsp-set-dhcpv4-options gx1a2b3c4d-0 aaaa",
        "external_ids:glidex-vm-id=vm1",
    ] {
        assert!(calls.contains(want), "missing {want:?} in:\n{calls}");
    }
}

#[test]
fn a_port_that_is_already_right_costs_nothing() {
    let exec = RecordingExec::new();
    exec.on("ovn-nbctl --db=", empty(r#"["name"]"#));
    let ports = r#"{"data":[["gx1a2b3c4d-0",["map",[["glidex-owner","glidex"],["glidex-vm-id","vm1"],["glidex-nic","0"]]],"52:54:00:aa:bb:cc 10.89.3.7","52:54:00:aa:bb:cc 10.89.3.7",["map",[["requested-chassis","n1"]]]]],"headings":["name","external_ids","addresses","port_security","options"]}"#;
    exec.on("ovn-nbctl --db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641 -p /k -c /c -C /ca --format=json --columns=name,external_ids,addresses,port_security,options find Logical_Switch_Port", Output::ok(ports));
    let sw = r#"{"data":[["gx-lab",["map",[["glidex-owner","glidex"],["glidex-network","lab"]]],["map",[]]]],"headings":["name","external_ids","other_config"]}"#;
    exec.on("ovn-nbctl --db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641 -p /k -c /c -C /ca --format=json --columns=name,external_ids,other_config find Logical_Switch", Output::ok(sw));
    exec.on(
        "ovn-nbctl --db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641 -p /k -c /c -C /ca --format=json --columns=name,addresses,external_ids find Address_Set",
        Output::ok(r#"{"data":[["gx_nodes",["set",[]],["map",[["glidex-owner","glidex"]]]]],"headings":["name","addresses","external_ids"]}"#),
    );
    let dhcp = format!(r#"{{"data":[[["uuid","aaaa"],["map",[["lease_time","3600"],["server_id","10.89.3.1"],["server_mac","{}"],["mtu","1442"],["dns_server","{{192.0.2.53}}"]]]]],"headings":["_uuid","options"]}}"#, net("lab", NetKind::Isolated, "10.89.3.0/24").router_mac());
    exec.on("ovn-nbctl --db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641 -p /k -c /c -C /ca --format=json --columns=_uuid,options find DHCP_Options", Output::ok(dhcp));
    let d = Desired {
        networks: vec![net("lab", NetKind::Isolated, "10.89.3.0/24")],
        ports: vec![PortSpec { network: "lab".into(), lport: "gx1a2b3c4d-0".into(), mac: "52:54:00:aa:bb:cc".into(), ip: Some("10.89.3.7".parse().unwrap()), vm_id: "vm1".into(), nic: 0, chassis: Some("n1".into()) }],
        ..Default::default()
    };
    let rep = sync(&Nb::new(&exec, conn()), &d).unwrap();
    assert!(rep.changed.is_empty(), "{rep:?}");
    assert!(exec.calls().iter().all(|c| !c.contains("lsp-add") && !c.contains("dhcp-options-create")), "{:?}", exec.calls());
}

#[test]
fn ports_and_networks_gone_from_the_plan_are_removed_and_foreign_rows_are_left() {
    let exec = RecordingExec::new();
    exec.on("ovn-nbctl --db=", empty(r#"["name"]"#));
    let ports = r#"{"data":[["gxdead-0",["map",[["glidex-owner","glidex"],["glidex-vm-id","gone"],["glidex-nic","0"],["glidex-network","old"]]],"","",["map",[]]]],"headings":["name","external_ids","addresses","port_security","options"]}"#;
    exec.on("ovn-nbctl --db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641 -p /k -c /c -C /ca --format=json --columns=name,external_ids,addresses,port_security,options find Logical_Switch_Port", Output::ok(ports));
    let sw = r#"{"data":[["gx-old",["map",[["glidex-owner","glidex"],["glidex-network","old"]]],["map",[]]]],"headings":["name","external_ids","other_config"]}"#;
    exec.on("ovn-nbctl --db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641 -p /k -c /c -C /ca --format=json --columns=name,external_ids,other_config find Logical_Switch", Output::ok(sw));
    let rep = sync(&Nb::new(&exec, conn()), &Desired::default()).unwrap();
    let calls = exec.calls().join("\n");
    assert!(calls.contains("--if-exists lsp-del gxdead-0"), "{calls}");
    assert!(calls.contains("--if-exists ls-del gx-old"), "{calls}");
    assert!(calls.contains("--if-exists lrp-del gx-lrp-old"), "{calls}");
    assert!(rep.changed.iter().any(|c| c.contains("removed network old")));
    // Only owned rows are ever asked for.
    assert!(calls.contains("find Logical_Switch external_ids:glidex-owner=glidex"), "{calls}");
}

#[test]
fn macs_are_stable_and_locally_administered() {
    let a = derived_mac("gxr:web");
    assert_eq!(a, derived_mac("gxr:web"));
    assert_ne!(a, derived_mac("gxr:db"));
    assert!(a.starts_with("02:"));
    glidex_ovs::names::validate_mac(&a).unwrap();
}
