//! glidex-netd tests without root or OVS: handlers against a recording
//! executor, and the real socket layer in a temp directory.

use glidex_netd::auth::{Peer, Policy};
use glidex_netd::client::{Client, ClientError};
use glidex_netd::proto::*;
use glidex_netd::server::{self, Access, Config, Netd};
use glidex_netd::supervisor::FakeSupervisor;
use glidex_ovs::bridge::{BridgeSpec, Datapath};
use glidex_ovs::host::HostCapabilities;
use glidex_ovs::nat::NatSpec;
use glidex_ovs::names;
use glidex_ovs::vm_port::{VmPortKind, VmPortSpec};
use glidex_ovs::{Output, RecordingExec};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

const VM: &str = "1a2b3c4d-5e6f-4a1b-9c2d-0123456789ab";
const OWNED_BRIDGE: &str = r#"{"data":[["gxbr-nat","system",["map",[["glidex-owner","glidex"],["glidex-role","bridge"]]],["set",[]]]],"headings":["name","datapath_type","external_ids","ports"]}"#;
const OWNED_IF: &str = r#"{"data":[["gx1a2b3c4d-0",["map",[["glidex-owner","glidex"],["glidex-role","vm"]]],""]],"headings":["name","external_ids","type"]}"#;

fn exec() -> Arc<RecordingExec> {
    let exec = Arc::new(RecordingExec::new());
    exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
    exec.on(
        "ovs-vsctl --format=json --columns=iface_types",
        Output::ok(r#"{"data":[[["set",["internal","system","tap"]],["set",["system"]],false]],"headings":["iface_types","datapath_types","dpdk_initialized"]}"#),
    );
    exec.on("ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge", Output::ok(OWNED_BRIDGE));
    exec.on("ovs-vsctl --format=json --columns=name,external_ids,type find Interface", Output::ok(OWNED_IF));
    exec.on("ip -4 -j route show", Output::ok(r#"[{"dst":"default","dev":"eth0"}]"#));
    exec.file("/proc/sys/net/ipv4/ip_forward", "1\n");
    exec
}

fn netd(exec: Arc<RecordingExec>, dir: &TempDir) -> (Netd, Arc<FakeSupervisor>) {
    netd_with_policy(exec, dir, test_policy(&["*"]))
}

fn netd_with_policy(exec: Arc<RecordingExec>, dir: &TempDir, policy: Policy) -> (Netd, Arc<FakeSupervisor>) {
    let sup = Arc::new(FakeSupervisor::default());
    let config = Config {
        run_dir: dir.path().join("run"),
        state_path: dir.path().join("netd.db"),
        policy,
        ..Config::default()
    };
    (Netd::new(exec, sup.clone(), config).unwrap(), sup)
}

/// A policy granting `patterns` to the test user's primary group (the
/// socket tests connect as that user, not as a `glidex` member).
fn test_policy(patterns: &[&str]) -> Policy {
    let group = nix::unistd::Group::from_gid(nix::unistd::getgid()).unwrap().unwrap().name;
    [(group, patterns.iter().map(|p| p.to_string()).collect())].into_iter().collect()
}

fn peer() -> Peer {
    Peer { uid: 1000, gid: 1000, pid: 1 }
}

fn port_spec() -> VmPortSpec {
    VmPortSpec {
        bridge: "gxbr-nat".into(),
        vm_id: VM.into(),
        nic_index: 0,
        kind: VmPortKind::Tap,
        mac: names::mac_address(VM, 0).unwrap(),
        vlan: None,
        mtu: None,
        queue_pairs: 1,
    }
}

fn setup_nat(netd: &Netd) {
    netd.handle(
        Op::EnsureBridge(BridgeSpec { name: "gxbr-nat".into(), datapath: Datapath::System, mtu: None, adopt: false }),
        &peer(),
    )
    .unwrap();
    netd.handle(Op::EnsureNat(NatSpec { bridge: "gxbr-nat".into(), subnet: None, dns: true }), &peer())
        .unwrap();
}

#[test]
fn nat_network_lifecycle() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    let (netd, sup) = netd(exec.clone(), &dir);
    setup_nat(&netd);

    let calls = exec.calls();
    assert!(calls.contains(&"ip addr replace 10.88.0.1/24 dev gxbr-nat".to_string()), "{calls:#?}");
    assert!(calls.iter().any(|c| c.starts_with("nft -f - <<< table inet glidex\ndelete table inet glidex") && c.contains("masquerade")));
    assert!(sup.running("gxbr-nat"));

    let nats: Vec<NatInfo> = serde_json::from_value(netd.handle(Op::ListNat, &peer()).unwrap()).unwrap();
    assert_eq!(nats.len(), 1);
    assert_eq!(nats[0].state.subnet.to_string(), "10.88.0.0/24");
    assert!(nats[0].dnsmasq_running);
}

#[test]
fn nat_passes_dockers_forward_drop() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    exec.on("iptables -w -S DOCKER-USER", Output::ok("-N DOCKER-USER\n"));
    exec.on("iptables -w -C DOCKER-USER -j GLIDEX-FORWARD", Output::failed(1, "Bad rule"));
    let (netd, _sup) = netd(exec.clone(), &dir);
    setup_nat(&netd);
    let calls = exec.calls();
    for c in [
        "iptables -w -A GLIDEX-FORWARD -i gxbr-nat -s 10.88.0.0/24 -j ACCEPT",
        "iptables -w -I DOCKER-USER 1 -j GLIDEX-FORWARD",
    ] {
        assert!(calls.iter().any(|x| x == c), "missing {c}: {calls:#?}");
    }

    // Deleting the last NAT network takes glidex's chain out again.
    netd.handle(Op::DeleteNat { bridge: "gxbr-nat".into() }, &peer()).unwrap();
    let calls = exec.calls();
    for c in ["iptables -w -D DOCKER-USER -j GLIDEX-FORWARD", "iptables -w -X GLIDEX-FORWARD"] {
        assert!(calls.iter().any(|x| x == c), "missing {c}: {calls:#?}");
    }
}

use glidex_netd::supervisor::Supervisor;

#[test]
fn attach_reserves_address_and_detach_keeps_it() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    let (netd, sup) = netd(exec.clone(), &dir);
    setup_nat(&netd);

    let res: AttachResult = serde_json::from_value(netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap()).unwrap();
    assert_eq!(res.port, "gx1a2b3c4d-0");
    assert_eq!(res.ipv4.unwrap().to_string(), "10.88.0.2");
    assert!(exec.calls().iter().any(|c| c == "ip tuntap add dev gx1a2b3c4d-0 mode tap user 1000"), "tap owned by the peer uid");
    let hosts = exec.writes().into_iter().filter(|(p, _)| p.ends_with("gxbr-nat.hosts")).last().unwrap();
    assert_eq!(String::from_utf8(hosts.1).unwrap(), format!("{},10.88.0.2\n", port_spec().mac));
    assert!(sup.events.lock().unwrap().contains(&"reload gxbr-nat".to_string()));

    // Detach (VM stop) keeps the reservation; the next start gets the same IP.
    netd.handle(Op::DetachVmPort { vm_id: VM.into(), nic_index: 0 }, &peer()).unwrap();
    let ports: Vec<VmPortRecord> = serde_json::from_value(netd.handle(Op::ListVmPorts, &peer()).unwrap()).unwrap();
    assert!(ports.is_empty());
    let again: AttachResult = serde_json::from_value(netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap()).unwrap();
    assert_eq!(again.ipv4, res.ipv4);

    // Release (VM delete) detaches and frees the address.
    netd.handle(Op::ReleaseVm { vm_id: VM.into() }, &peer()).unwrap();
    let nats: Vec<NatInfo> = serde_json::from_value(netd.handle(Op::ListNat, &peer()).unwrap()).unwrap();
    assert!(nats[0].state.reservations.is_empty());
}

/// A deleted VM's dnsmasq lease outlives its reservation; the next VM must
/// not be reserved that address (dnsmasq wouldn't hand it out). Deleting
/// the network drops the lease file.
#[test]
fn reservations_skip_live_leases_and_delete_drops_them() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    let (netd, _) = netd(exec.clone(), &dir);
    setup_nat(&netd);
    exec.file("/var/lib/glidex/dnsmasq/gxbr-nat.leases", "4000000000 02:89:53:f9:ac:2d 10.88.0.2 * ff:b5\n");

    let res: AttachResult = serde_json::from_value(netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap()).unwrap();
    assert_eq!(res.ipv4.unwrap().to_string(), "10.88.0.3");

    netd.handle(Op::ReleaseVm { vm_id: VM.into() }, &peer()).unwrap();
    netd.handle(Op::DeleteNat { bridge: "gxbr-nat".into() }, &peer()).unwrap();
    assert!(exec.calls().contains(&"rm /var/lib/glidex/dnsmasq/gxbr-nat.leases".to_string()), "{:#?}", exec.calls());
}

#[test]
fn deletes_are_refused_while_in_use() {
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec(), &dir);
    setup_nat(&netd);
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();

    let err = netd.handle(Op::DeleteNat { bridge: "gxbr-nat".into() }, &peer()).unwrap_err();
    assert_eq!(err.code(), "conflict");
    let err = netd.handle(Op::DeleteBridge { name: "gxbr-nat".into() }, &peer()).unwrap_err();
    assert_eq!(err.code(), "conflict");

    netd.handle(Op::DetachVmPort { vm_id: VM.into(), nic_index: 0 }, &peer()).unwrap();
    netd.handle(Op::DeleteNat { bridge: "gxbr-nat".into() }, &peer()).unwrap();
    netd.handle(Op::DeleteBridge { name: "gxbr-nat".into() }, &peer()).unwrap();
}

fn other_peer() -> Peer {
    Peer { uid: 2000, gid: 2000, pid: 2 }
}

fn root_peer() -> Peer {
    Peer { uid: 0, gid: 0, pid: 3 }
}

fn port_count(netd: &Netd) -> usize {
    serde_json::from_value::<Vec<VmPortRecord>>(netd.handle(Op::ListVmPorts, &peer()).unwrap()).unwrap().len()
}

#[test]
fn only_the_owner_detaches_and_releases() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    let (netd, _) = netd(exec.clone(), &dir);
    setup_nat(&netd);
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();
    exec.clear_calls();

    let err = netd.handle(Op::DetachVmPort { vm_id: VM.into(), nic_index: 0 }, &other_peer()).unwrap_err();
    assert_eq!(err.code(), "not_owned");
    assert!(err.to_string().contains("uid 1000"), "{err}");
    let err = netd.handle(Op::ReleaseVm { vm_id: VM.into() }, &other_peer()).unwrap_err();
    assert_eq!(err.code(), "not_owned");
    assert!(!exec.calls().iter().any(|c| c.contains("del-port")), "nothing detached: {:#?}", exec.calls());
    assert_eq!(port_count(&netd), 1);
    let nats: Vec<NatInfo> = serde_json::from_value(netd.handle(Op::ListNat, &peer()).unwrap()).unwrap();
    assert_eq!(nats[0].state.reservations.len(), 1, "reservation kept");

    // The owner may.
    netd.handle(Op::DetachVmPort { vm_id: VM.into(), nic_index: 0 }, &peer()).unwrap();
    assert_eq!(port_count(&netd), 0);
    // Without a record there is no owner to check (detach is idempotent).
    netd.handle(Op::DetachVmPort { vm_id: VM.into(), nic_index: 0 }, &other_peer()).unwrap();
}

#[test]
fn root_may_detach_and_release_any_port() {
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec(), &dir);
    setup_nat(&netd);
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();
    netd.handle(Op::DetachVmPort { vm_id: VM.into(), nic_index: 0 }, &root_peer()).unwrap();
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();
    netd.handle(Op::ReleaseVm { vm_id: VM.into() }, &root_peer()).unwrap();
    assert_eq!(port_count(&netd), 0);
}

#[test]
fn release_is_all_or_nothing() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    let (netd, _) = netd(exec.clone(), &dir);
    setup_nat(&netd);
    // nic 0 attached by the peer, nic 1 by someone else.
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();
    let nic1 = VmPortSpec { nic_index: 1, mac: names::mac_address(VM, 1).unwrap(), ..port_spec() };
    netd.handle(Op::AttachVmPort(nic1), &other_peer()).unwrap();
    exec.clear_calls();

    let err = netd.handle(Op::ReleaseVm { vm_id: VM.into() }, &peer()).unwrap_err();
    assert_eq!(err.code(), "not_owned");
    assert!(!exec.calls().iter().any(|c| c.contains("del-port")), "no partial release: {:#?}", exec.calls());
    assert_eq!(port_count(&netd), 2);
}

#[test]
fn attach_requires_a_glidex_bridge() {
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec(), &dir);
    let err = netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap_err();
    assert_eq!(err.code(), "not_found");
}

#[test]
fn sync_detaches_stopped_vms_and_reports_orphans() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    let (netd, _) = netd(exec.clone(), &dir);
    setup_nat(&netd);
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();
    exec.on(
        "ovs-vsctl --format=json --columns=name,external_ids list Interface",
        Output::ok(r#"{"data":[["gxdeadbeef-0",["map",[["glidex-owner","glidex"],["glidex-role","vm"],["glidex-vm-id","deadbeef-0000"],["glidex-nic","0"]]]]],"headings":["name","external_ids"]}"#),
    );

    let report: ReconcileReport = serde_json::from_value(netd.handle(Op::SyncVms { running: vec![] }, &peer()).unwrap()).unwrap();
    assert_eq!(report.detached, vec!["gx1a2b3c4d-0"]);
    assert_eq!(report.orphans, vec!["gxdeadbeef-0"], "untracked ports are reported, not deleted");
    assert!(!exec.calls().iter().any(|c| c.contains("del-port gxdeadbeef-0")));
}

#[test]
fn state_survives_restart_and_is_reconciled() {
    let dir = TempDir::new().unwrap();
    {
        let (netd, _) = netd(exec(), &dir);
        setup_nat(&netd);
    }
    let exec = exec();
    let (netd, sup) = netd(exec.clone(), &dir);
    let report = netd.reconcile_startup();
    assert!(report.errors.is_empty(), "{report:?}");
    assert!(report.repaired.contains(&"nat gxbr-nat".to_string()));
    assert!(exec.calls().contains(&"ip addr replace 10.88.0.1/24 dev gxbr-nat".to_string()));
    assert!(sup.running("gxbr-nat"));
}

#[test]
fn probe_reports_host_status() {
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec(), &dir);
    let caps: HostCapabilities = serde_json::from_value(netd.handle(Op::Probe, &peer()).unwrap()).unwrap();
    assert!(caps.ovs_installed && caps.ovs_running);
}

// ---- socket layer ----------------------------------------------------------

fn primary_gid() -> u32 {
    nix::unistd::getgid().as_raw()
}

fn start(dir: &TempDir, access: Access, group_gid: Option<u32>) -> std::path::PathBuf {
    start_with_policy(dir, access, group_gid, test_policy(&["*"]))
}

fn start_with_policy(dir: &TempDir, access: Access, group_gid: Option<u32>, policy: Policy) -> std::path::PathBuf {
    let (netd, _) = netd_with_policy(exec(), dir, policy);
    let path = dir.path().join(format!("{:?}.sock", access));
    let listener = server::bind(&path, if access == Access::Status { 0o666 } else { 0o660 }, group_gid).unwrap();
    let netd = Arc::new(netd);
    std::thread::spawn(move || server::serve(netd, listener, access, group_gid));
    path
}

#[test]
fn full_socket_serves_group_members() {
    let dir = TempDir::new().unwrap();
    let path = start(&dir, Access::Full, Some(primary_gid()));
    let mut client = Client::connect(&path).unwrap();
    assert_eq!(client.hello.protocol, PROTOCOL_VERSION);
    let bridges: Vec<BridgeRecord> = client.call(Op::ListBridges, Duration::from_secs(5)).unwrap();
    assert!(bridges.is_empty());
    let err = client
        .call_value(Op::DeleteNat { bridge: "Bad Name!".into() }, Duration::from_secs(5))
        .map(|_| ());
    // Unknown NAT is a no-op; the connection keeps working after errors too.
    assert!(err.is_ok());

    let mode = std::fs::metadata(&path).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777).unwrap();
    assert_eq!(mode, 0o660);
}

#[test]
fn full_socket_rejects_peers_outside_the_group() {
    if nix::unistd::getuid().is_root() {
        return; // root is always authorized
    }
    let dir = TempDir::new().unwrap();
    // gid 0 ("root" group): an ordinary user isn't a member.
    let path = start(&dir, Access::Full, Some(0));
    match Client::connect(&path) {
        Err(ClientError::Unavailable(_)) => {}
        Ok(_) => panic!("non-member was served"),
        Err(e) => panic!("unexpected error: {e}"),
    }
}

#[test]
fn status_socket_serves_only_status() {
    let dir = TempDir::new().unwrap();
    let path = start(&dir, Access::Status, None);
    let mut client = Client::connect(&path).unwrap();
    let caps: HostCapabilities = client.call(Op::Probe, Duration::from_secs(5)).unwrap();
    assert!(caps.ovs_installed);
    for op in [Op::ListBridges, Op::ListVmPorts, Op::SyncVms { running: vec![] }] {
        match client.call_value(op, Duration::from_secs(5)) {
            Err(ClientError::Remote(body)) => assert_eq!(body.code, "permission_denied"),
            other => panic!("expected permission_denied, got {other:?}"),
        }
    }
}

#[test]
fn hello_must_come_first_and_garbage_is_rejected() {
    use std::io::{BufRead, BufReader, Write};
    let dir = TempDir::new().unwrap();
    let path = start(&dir, Access::Full, Some(primary_gid()));
    let mut stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();

    stream.write_all(b"{\"id\":1,\"op\":\"list_bridges\"}\n").unwrap();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("first request must be hello"), "{line}");

    line.clear();
    stream.write_all(b"not json\n").unwrap();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("protocol_error"), "{line}");

    line.clear();
    stream.write_all(b"{\"id\":2,\"op\":\"exec\",\"args\":{\"cmd\":\"sh\"}}\n").unwrap();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("protocol_error"), "unknown ops don't parse: {line}");
}

fn expect_denied(client: &mut Client, op: Op) {
    let name = op.name();
    match client.call_value(op, Duration::from_secs(5)) {
        Err(ClientError::Remote(body)) => assert_eq!(body.code, "permission_denied", "{name}: {body:?}"),
        other => panic!("{name}: expected permission_denied, got {other:?}"),
    }
}

#[test]
fn policy_limits_ops_per_group() {
    if nix::unistd::getuid().is_root() {
        return; // root is exempt from the policy
    }
    let dir = TempDir::new().unwrap();
    let path = start_with_policy(&dir, Access::Full, Some(primary_gid()), test_policy(&["list_*", "delete_nat"]));
    let mut client = Client::connect(&path).expect("hello is always allowed");
    let _: Vec<BridgeRecord> = client.call(Op::ListBridges, Duration::from_secs(5)).unwrap();
    let _: Vec<NatInfo> = client.call(Op::ListNat, Duration::from_secs(5)).unwrap();
    client.call_value(Op::DeleteNat { bridge: "gxbr-x".into() }, Duration::from_secs(5)).unwrap();
    let _: HostCapabilities = client.call(Op::Probe, Duration::from_secs(5)).unwrap();
    expect_denied(&mut client, Op::DeleteBridge { name: "gxbr-x".into() });
    expect_denied(&mut client, Op::SyncVms { running: vec![] });
    expect_denied(&mut client, Op::ReleaseVm { vm_id: VM.into() });
    // The connection keeps working after a denial.
    let _: Vec<BridgeRecord> = client.call(Op::ListBridges, Duration::from_secs(5)).unwrap();
}

#[test]
fn policy_without_the_peers_groups_denies_everything_but_hello_and_probe() {
    if nix::unistd::getuid().is_root() {
        return;
    }
    let dir = TempDir::new().unwrap();
    // The default policy names glidex and glidex-admin; give the test a
    // policy whose only group surely isn't the user's.
    let policy: Policy = [("no-such-group-glidex-test".to_string(), vec!["*".to_string()])].into_iter().collect();
    let path = start_with_policy(&dir, Access::Full, Some(primary_gid()), policy);
    let mut client = Client::connect(&path).unwrap();
    let _: HostCapabilities = client.call(Op::Probe, Duration::from_secs(5)).unwrap();
    expect_denied(&mut client, Op::ListBridges);
    expect_denied(&mut client, Op::DetachVmPort { vm_id: VM.into(), nic_index: 0 });
}

#[test]
fn admin_socket_serves_admin_group_members_under_the_policy() {
    let dir = TempDir::new().unwrap();
    let path = start_with_policy(&dir, Access::Admin, Some(primary_gid()), test_policy(&["list_*"]));
    let mut client = Client::connect(&path).unwrap();
    let _: Vec<BridgeRecord> = client.call(Op::ListBridges, Duration::from_secs(5)).unwrap();
    if !nix::unistd::getuid().is_root() {
        expect_denied(&mut client, Op::SyncVms { running: vec![] });
    }
    let mode = std::fs::metadata(&path).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777).unwrap();
    assert_eq!(mode, 0o660);
    assert_eq!(Access::Admin.socket_name(), "netd-admin.sock");
}

#[test]
fn admin_socket_rejects_peers_outside_the_admin_group() {
    if nix::unistd::getuid().is_root() {
        return;
    }
    let dir = TempDir::new().unwrap();
    let path = start(&dir, Access::Admin, Some(0));
    assert!(matches!(Client::connect(&path), Err(ClientError::Unavailable(_))));
    // No admin group on the host: nobody but root.
    let dir = TempDir::new().unwrap();
    let path = start(&dir, Access::Admin, None);
    assert!(matches!(Client::connect(&path), Err(ClientError::Unavailable(_))));
}

#[test]
fn on_behalf_of_is_accepted_and_optional() {
    use std::io::{BufRead, BufReader, Write};
    let dir = TempDir::new().unwrap();
    let path = start(&dir, Access::Full, Some(primary_gid()));
    let mut client = Client::connect(&path).unwrap();
    let obo = OnBehalfOf { user: "alice".into(), project: Some("default".into()), request_id: Some("req-1".into()) };
    let _: ReconcileReport = client.call_as(Op::SyncVms { running: vec![] }, Some(&obo), Duration::from_secs(5)).unwrap();
    let _: Vec<BridgeRecord> = client.call_as(Op::ListBridges, None, Duration::from_secs(5)).unwrap();

    // A raw client that has never heard of on_behalf_of, and one that sends
    // only the user.
    let mut stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    for req in [
        r#"{"id":1,"op":"hello","args":{"protocol":1}}"#,
        r#"{"id":2,"op":"sync_vms","args":{"running":[]}}"#,
        r#"{"id":3,"op":"sync_vms","args":{"running":[]},"on_behalf_of":{"user":"bob"}}"#,
    ] {
        line.clear();
        stream.write_all(format!("{req}\n").as_bytes()).unwrap();
        reader.read_line(&mut line).unwrap();
        let resp: Response = serde_json::from_str(&line).unwrap();
        assert!(resp.error.is_none(), "{req}: {line}");
    }
}

// ---- uplinks (M6) ----------------------------------------------------------

use glidex_ovs::uplink::{UplinkKind, UplinkSpec};

const UP_BRIDGE: &str = r#"{"data":[["gxbr-up","system",["map",[["glidex-owner","glidex"],["glidex-role","bridge"]]],["set",[]]]],"headings":["name","datapath_type","external_ids","ports"]}"#;

fn uplink_exec(nic_addr: bool, gateway_ok: bool) -> Arc<RecordingExec> {
    let exec = exec();
    exec.on("ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge name=gxbr-up", Output::ok(UP_BRIDGE));
    exec.on("ovs-vsctl --format=json --columns=name,external_ids find Interface name=gxup0", Output::ok(r#"{"data":[["gxup0",["map",[["glidex-owner","glidex"],["glidex-role","uplink"]]]]],"headings":["name","external_ids"]}"#));
    exec.on("ovs-vsctl --format=json --columns=name,error,link_state find Interface", Output::ok(r#"{"data":[["gxup0",["set",[]],"up"]],"headings":["name","error","link_state"]}"#));
    exec.file("/sys/class/net/gxup0", "");
    exec.file("/sys/class/net/gxup0/address", "52:54:00:12:34:56\n");
    let addr = if nic_addr { r#"{"family":"inet","local":"192.0.2.10","prefixlen":24,"scope":"global"}"# } else { "" };
    exec.on("ip -j addr show dev gxup0", Output::ok(format!(r#"[{{"addr_info":[{addr}]}}]"#)));
    exec.on("ip -4 -j route show dev gxup0", Output::ok(r#"[{"dst":"default","gateway":"192.0.2.1","protocol":"static"}]"#));
    let state = if gateway_ok { r#"["REACHABLE"],"lladdr":"aa:bb:cc:dd:ee:ff""# } else { r#"["FAILED"]"# };
    exec.on("ip -j neigh show 192.0.2.1 dev gxbr-up", Output::ok(format!(r#"[{{"dst":"192.0.2.1","state":{state}}}]"#)));
    exec
}

fn up_netd(exec: Arc<RecordingExec>, dir: &TempDir, commit_window: Duration) -> Netd {
    let config = Config {
        run_dir: dir.path().join("run"),
        state_path: dir.path().join("netd.db"),
        commit_window,
        gateway_check: Duration::from_millis(300),
        ..Config::default()
    };
    let netd = Netd::new(exec, Arc::new(FakeSupervisor::default()), config).unwrap();
    netd.handle(Op::EnsureBridge(BridgeSpec { name: "gxbr-up".into(), datapath: Datapath::System, mtu: None, adopt: false }), &peer()).unwrap();
    netd
}

fn kernel_uplink(migrate: bool) -> UplinkSpec {
    UplinkSpec { name: "gxup0".into(), bridge: "gxbr-up".into(), kind: UplinkKind::Kernel { ifname: "gxup0".into() }, migrate_ip: migrate }
}

fn ensure(netd: &Netd, spec: UplinkSpec, confirm: bool) -> Result<UplinkResult, glidex_ovs::OvsError> {
    netd.handle(Op::EnsureUplink(EnsureUplinkArgs { spec, confirm }), &peer())
        .map(|v| serde_json::from_value(v).unwrap())
}

#[test]
fn free_nic_becomes_active_uplink() {
    let dir = TempDir::new().unwrap();
    let exec = uplink_exec(false, true);
    let netd = up_netd(exec.clone(), &dir, Duration::from_secs(60));
    let res = ensure(&netd, kernel_uplink(false), false).unwrap();
    assert_eq!(res.phase, UplinkPhase::Active);
    assert!(exec.calls().iter().any(|c| c.starts_with("ovs-vsctl --may-exist add-port gxbr-up gxup0")));
    // Idempotent.
    assert_eq!(ensure(&netd, kernel_uplink(false), false).unwrap().phase, UplinkPhase::Active);
    // A bridge with uplinks can't be deleted.
    assert_eq!(netd.handle(Op::DeleteBridge { name: "gxbr-up".into() }, &peer()).unwrap_err().code(), "conflict");
}

#[test]
fn in_use_nic_needs_migrate_and_confirm() {
    let dir = TempDir::new().unwrap();
    let exec = uplink_exec(true, true);
    let netd = up_netd(exec.clone(), &dir, Duration::from_secs(60));
    for (migrate, confirm) in [(false, false), (true, false), (false, true)] {
        let err = ensure(&netd, kernel_uplink(migrate), confirm).unwrap_err();
        assert_eq!(err.code(), "host_interface_in_use", "migrate={migrate} confirm={confirm}");
    }
    assert!(!exec.calls().iter().any(|c| c.contains("add-port gxbr-up gxup0")), "nothing changed");
}

#[test]
fn migration_is_pending_until_committed_with_the_token() {
    let dir = TempDir::new().unwrap();
    let exec = uplink_exec(true, true);
    let netd = up_netd(exec.clone(), &dir, Duration::from_secs(60));
    let res = ensure(&netd, kernel_uplink(true), true).unwrap();
    assert_eq!(res.phase, UplinkPhase::PendingCommit);
    let token = res.record.pending.as_ref().unwrap().token.clone();
    assert_eq!(token.len(), 32);
    assert!(exec.calls().contains(&"ip addr replace 192.0.2.10/24 dev gxbr-up".to_string()));

    let err = netd.handle(Op::CommitUplink { bridge: "gxbr-up".into(), name: "gxup0".into(), token: "wrong".into() }, &peer()).unwrap_err();
    assert_eq!(err.code(), "invalid_argument");
    let ok: UplinkResult = serde_json::from_value(
        netd.handle(Op::CommitUplink { bridge: "gxbr-up".into(), name: "gxup0".into(), token }, &peer()).unwrap(),
    )
    .unwrap();
    assert_eq!(ok.phase, UplinkPhase::Active);
    assert!(netd.expire_pending().is_empty(), "committed uplinks never expire");

    // Deleting gives the NIC its address back.
    exec.clear_calls();
    netd.handle(Op::DeleteUplink { bridge: "gxbr-up".into(), name: "gxup0".into() }, &peer()).unwrap();
    let calls = exec.calls();
    assert!(calls.contains(&"ovs-vsctl --if-exists del-port gxup0".to_string()));
    assert!(calls.contains(&"ip addr replace 192.0.2.10/24 dev gxup0".to_string()), "{calls:#?}");
}

#[test]
fn uncommitted_migration_rolls_back_at_the_deadline() {
    let dir = TempDir::new().unwrap();
    let exec = uplink_exec(true, true);
    let netd = up_netd(exec.clone(), &dir, Duration::from_secs(0));
    ensure(&netd, kernel_uplink(true), true).unwrap();
    exec.clear_calls();
    assert_eq!(netd.expire_pending(), vec!["gxup0"]);
    let calls = exec.calls();
    assert!(calls.contains(&"ovs-vsctl --if-exists del-port gxup0".to_string()));
    assert!(calls.contains(&"ip addr replace 192.0.2.10/24 dev gxup0".to_string()));
    let uplinks: Vec<UplinkResult> = serde_json::from_value(netd.handle(Op::ListUplinks, &peer()).unwrap()).unwrap();
    assert!(uplinks.is_empty());
}

#[test]
fn unreachable_gateway_rolls_back_immediately() {
    let dir = TempDir::new().unwrap();
    let exec = uplink_exec(true, false);
    let netd = up_netd(exec.clone(), &dir, Duration::from_secs(60));
    let err = ensure(&netd, kernel_uplink(true), true).unwrap_err();
    assert_eq!(err.code(), "migration_rolled_back");
    let calls = exec.calls();
    assert!(calls.contains(&"ip addr replace 192.0.2.10/24 dev gxup0".to_string()), "address restored");
    assert!(calls.contains(&"ovs-vsctl --if-exists del-port gxup0".to_string()));
}

#[test]
fn uplinks_are_refused_on_nat_bridges() {
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec(), &dir);
    setup_nat(&netd);
    let spec = UplinkSpec { name: "gxup0".into(), bridge: "gxbr-nat".into(), kind: UplinkKind::Kernel { ifname: "gxup0".into() }, migrate_ip: false };
    assert_eq!(ensure(&netd, spec, false).unwrap_err().code(), "conflict");
}

#[test]
fn dpdk_uplink_binds_and_restores_the_driver() {
    let dir = TempDir::new().unwrap();
    let exec = uplink_exec(false, true);
    exec.set("ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge name=gxbr-up",
        Output::ok(UP_BRIDGE.replace("\"system\"", "\"netdev\"")));
    exec.set("ovs-vsctl --format=json --columns=iface_types",
        Output::ok(r#"{"data":[[["set",["dpdk","dpdkvhostuserclient","tap"]],["set",["netdev","system"]],true]],"headings":["iface_types","datapath_types","dpdk_initialized"]}"#));
    exec.on("ovs-vsctl --format=json --columns=name,external_ids find Interface name=up0", Output::ok(r#"{"data":[["up0",["map",[["glidex-owner","glidex"]]]]],"headings":["name","external_ids"]}"#));
    exec.file("/sys/kernel/iommu_groups/0", "");
    exec.file("/sys/bus/pci/devices/0000:41:00.0", "");
    exec.file("/sys/bus/pci/devices/0000:41:00.0/uevent", "DRIVER=ixgbe\n");
    let sup = Arc::new(FakeSupervisor::default());
    let netd = Netd::new(exec.clone(), sup, Config { run_dir: dir.path().join("run"), state_path: dir.path().join("netd.db"), ..Config::default() }).unwrap();
    netd.handle(Op::EnsureBridge(BridgeSpec { name: "gxbr-up".into(), datapath: Datapath::Netdev, mtu: None, adopt: false }), &peer()).unwrap();

    let spec = UplinkSpec { name: "up0".into(), bridge: "gxbr-up".into(), kind: UplinkKind::Dpdk { pci: "0000:41:00.0".into(), n_rxq: 2 }, migrate_ip: false };
    let res = ensure(&netd, spec, false).unwrap();
    assert_eq!(res.record.orig_driver.as_deref(), Some("ixgbe"));
    assert!(exec.writes().iter().any(|(p, d)| p.ends_with("driver_override") && d == b"vfio-pci"));
    netd.handle(Op::DeleteUplink { bridge: "gxbr-up".into(), name: "up0".into() }, &peer()).unwrap();
    assert!(exec.writes().iter().any(|(p, d)| p.ends_with("driver_override") && d == b"\n"), "driver restored");
}

#[test]
fn committed_migrations_are_reapplied_at_start() {
    let dir = TempDir::new().unwrap();
    {
        let netd = up_netd(uplink_exec(true, true), &dir, Duration::from_secs(60));
        let res = ensure(&netd, kernel_uplink(true), true).unwrap();
        let token = res.record.pending.unwrap().token;
        netd.handle(Op::CommitUplink { bridge: "gxbr-up".into(), name: "gxup0".into(), token }, &peer()).unwrap();
    }
    // After a reboot the host puts the address back on the NIC; netd moves it again.
    let exec = uplink_exec(true, true);
    let netd = Netd::new(exec.clone(), Arc::new(FakeSupervisor::default()), Config {
        run_dir: dir.path().join("run"),
        state_path: dir.path().join("netd.db"),
        ..Config::default()
    })
    .unwrap();
    let report = netd.reconcile_startup();
    assert!(report.errors.is_empty(), "{report:?}");
    let calls = exec.calls();
    assert!(calls.iter().any(|c| c.starts_with("ovs-vsctl --may-exist add-port gxbr-up gxup0")));
    assert!(calls.contains(&"ip addr replace 192.0.2.10/24 dev gxbr-up".to_string()));
}

#[test]
fn uncommitted_migrations_are_rolled_back_at_start() {
    let dir = TempDir::new().unwrap();
    {
        let netd = up_netd(uplink_exec(true, true), &dir, Duration::from_secs(600));
        ensure(&netd, kernel_uplink(true), true).unwrap();
    }
    let exec = uplink_exec(true, true);
    let netd = Netd::new(exec.clone(), Arc::new(FakeSupervisor::default()), Config {
        run_dir: dir.path().join("run"),
        state_path: dir.path().join("netd.db"),
        ..Config::default()
    })
    .unwrap();
    let report = netd.reconcile_startup();
    assert!(report.repaired.iter().any(|r| r.contains("rolled back uncommitted uplink gxup0")), "{report:?}");
    assert!(exec.calls().contains(&"ip addr replace 192.0.2.10/24 dev gxup0".to_string()));
}
