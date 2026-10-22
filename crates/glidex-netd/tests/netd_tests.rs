//! glidex-netd tests without root or OVS: handlers against a recording
//! executor, and the real socket layer in a temp directory.

use glidex_netd::auth::{Peer, Policy};
use glidex_netd::client::{Client, ClientError};
use glidex_netd::proto::*;
use glidex_netd::server::{self, Access, Config, Netd};
use glidex_netd::supervisor::FakeSupervisor;
use glidex_ovs::bridge::{BridgeSpec, Datapath};
use glidex_ovs::host::HostCapabilities;
use glidex_ovs::names;
use glidex_ovs::nat::NatSpec;
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
    exec.on(
        "ovs-vsctl --version",
        Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"),
    );
    exec.on(
        "ovs-vsctl --format=json --columns=iface_types",
        Output::ok(r#"{"data":[[["set",["internal","system","tap"]],["set",["system"]],false]],"headings":["iface_types","datapath_types","dpdk_initialized"]}"#),
    );
    exec.on(
        "ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge",
        Output::ok(OWNED_BRIDGE),
    );
    exec.on(
        "ovs-vsctl --format=json --columns=name,external_ids,type find Interface",
        Output::ok(OWNED_IF),
    );
    exec.on(
        "ip -4 -j route show",
        Output::ok(r#"[{"dst":"default","dev":"eth0"}]"#),
    );
    exec.file("/proc/sys/net/ipv4/ip_forward", "1\n");
    exec
}

fn netd(exec: Arc<RecordingExec>, dir: &TempDir) -> (Netd, Arc<FakeSupervisor>) {
    netd_with_policy(exec, dir, test_policy(&["*"]))
}

fn netd_with_policy(
    exec: Arc<RecordingExec>,
    dir: &TempDir,
    policy: Policy,
) -> (Netd, Arc<FakeSupervisor>) {
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
    let group = nix::unistd::Group::from_gid(nix::unistd::getgid())
        .unwrap()
        .unwrap()
        .name;
    [(group, patterns.iter().map(|p| p.to_string()).collect())]
        .into_iter()
        .collect()
}

fn peer() -> Peer {
    Peer {
        uid: 1000,
        gid: 1000,
        pid: 1,
    }
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
        ovn_lport: None,
    }
}

fn setup_nat(netd: &Netd) {
    netd.handle(
        Op::EnsureBridge(BridgeSpec {
            name: "gxbr-nat".into(),
            datapath: Datapath::System,
            mtu: None,
            adopt: false,
            isolated: false,
        }),
        &peer(),
    )
    .unwrap();
    netd.handle(
        Op::EnsureNat(NatSpec {
            bridge: "gxbr-nat".into(),
            subnet: None,
            dns: true,
        }),
        &peer(),
    )
    .unwrap();
}

#[test]
fn nat_network_lifecycle() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    let (netd, sup) = netd(exec.clone(), &dir);
    setup_nat(&netd);

    let calls = exec.calls();
    assert!(
        calls.contains(&"ip addr replace 10.88.0.1/24 dev gxbr-nat".to_string()),
        "{calls:#?}"
    );
    assert!(calls.iter().any(|c| {
        c.starts_with("nft -f - <<< table inet glidex\ndelete table inet glidex")
            && c.contains("masquerade")
    }));
    assert!(sup.running("gxbr-nat"));

    let nats: Vec<NatInfo> =
        serde_json::from_value(netd.handle(Op::ListNat, &peer()).unwrap()).unwrap();
    assert_eq!(nats.len(), 1);
    assert_eq!(nats[0].state.subnet.to_string(), "10.88.0.0/24");
    assert!(nats[0].dnsmasq_running);
}

#[test]
fn nat_passes_dockers_forward_drop() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    exec.on("iptables -w -S DOCKER-USER", Output::ok("-N DOCKER-USER\n"));
    exec.on(
        "iptables -w -C DOCKER-USER -j GLIDEX-FORWARD",
        Output::failed(1, "Bad rule"),
    );
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
    netd.handle(
        Op::DeleteNat {
            bridge: "gxbr-nat".into(),
        },
        &peer(),
    )
    .unwrap();
    let calls = exec.calls();
    for c in [
        "iptables -w -D DOCKER-USER -j GLIDEX-FORWARD",
        "iptables -w -X GLIDEX-FORWARD",
    ] {
        assert!(calls.iter().any(|x| x == c), "missing {c}: {calls:#?}");
    }
}

#[test]
fn nat_passes_ufws_input_drop() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    exec.on("iptables -w -S INPUT", Output::ok("-P INPUT DROP\n"));
    exec.on(
        "iptables -w -C INPUT -j GLIDEX-INPUT",
        Output::failed(1, "Bad rule"),
    );
    let (netd, _sup) = netd(exec.clone(), &dir);
    setup_nat(&netd);
    let calls = exec.calls();
    for c in [
        "iptables -w -A GLIDEX-INPUT -i gxbr-nat -p udp --dport 67 -j ACCEPT",
        "iptables -w -I INPUT 1 -j GLIDEX-INPUT",
    ] {
        assert!(calls.iter().any(|x| x == c), "missing {c}: {calls:#?}");
    }

    // Deleting the last NAT network takes the chain and its jump out again.
    netd.handle(
        Op::DeleteNat {
            bridge: "gxbr-nat".into(),
        },
        &peer(),
    )
    .unwrap();
    let calls = exec.calls();
    for c in [
        "iptables -w -D INPUT -j GLIDEX-INPUT",
        "iptables -w -X GLIDEX-INPUT",
    ] {
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

    let res: AttachResult =
        serde_json::from_value(netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap())
            .unwrap();
    assert_eq!(res.port, "gx1a2b3c4d-0");
    assert_eq!(res.ipv4.unwrap().to_string(), "10.88.0.2");
    assert!(
        exec.calls()
            .iter()
            .any(|c| c == "ip tuntap add dev gx1a2b3c4d-0 mode tap user 1000"),
        "tap owned by the peer uid"
    );
    let hosts = exec
        .writes()
        .into_iter()
        .rfind(|(p, _)| p.ends_with("gxbr-nat.hosts"))
        .unwrap();
    assert_eq!(
        String::from_utf8(hosts.1).unwrap(),
        format!("{},10.88.0.2\n", port_spec().mac)
    );
    assert!(
        sup.events
            .lock()
            .unwrap()
            .contains(&"reload gxbr-nat".to_string())
    );

    // Detach (VM stop) keeps the reservation; the next start gets the same IP.
    netd.handle(
        Op::DetachVmPort {
            vm_id: VM.into(),
            nic_index: 0,
        },
        &peer(),
    )
    .unwrap();
    let ports: Vec<VmPortRecord> =
        serde_json::from_value(netd.handle(Op::ListVmPorts, &peer()).unwrap()).unwrap();
    assert!(ports.is_empty());
    let again: AttachResult =
        serde_json::from_value(netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap())
            .unwrap();
    assert_eq!(again.ipv4, res.ipv4);

    // Release (VM delete) detaches and frees the address.
    netd.handle(Op::ReleaseVm { vm_id: VM.into() }, &peer())
        .unwrap();
    let nats: Vec<NatInfo> =
        serde_json::from_value(netd.handle(Op::ListNat, &peer()).unwrap()).unwrap();
    assert!(nats[0].state.reservations.is_empty());
}

/// Metering counters (spec/metering.md D14): attaching adds the VM's
/// counters; releasing deletes only that VM's, never the table, and the
/// NAT table rebuilds never touch `inet glidex_meter`.
#[test]
fn nat_meter_counters_follow_reservations_incrementally() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    let (netd, _sup) = netd(exec.clone(), &dir);
    setup_nat(&netd);
    let mac = port_spec().mac.replace(':', "");
    let meter_calls = |exec: &RecordingExec| -> Vec<String> {
        exec.calls()
            .into_iter()
            .filter(|c| c.starts_with("nft -f - <<< add table inet glidex_meter"))
            .collect()
    };
    assert_eq!(
        meter_calls(&exec).len(),
        1,
        "ensure_nat adds the network's counters"
    );
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();
    let after_attach = meter_calls(&exec).pop().unwrap();
    assert!(
        after_attach.contains(&format!("add counter inet glidex_meter m_{mac}_out")),
        "{after_attach}"
    );
    assert!(!after_attach.contains("delete"), "{after_attach}");

    // The counters now exist; releasing the VM deletes only its own.
    let listed = format!(
        r#"{{"nftables":[{{"counter":{{"family":"inet","name":"br_gxbr_nat_in","table":"glidex_meter","handle":2,"packets":0,"bytes":0}}}},
            {{"counter":{{"family":"inet","name":"br_gxbr_nat_out","table":"glidex_meter","handle":3,"packets":0,"bytes":0}}}},
            {{"counter":{{"family":"inet","name":"m_{mac}_in","table":"glidex_meter","handle":4,"packets":0,"bytes":0}}}},
            {{"counter":{{"family":"inet","name":"m_{mac}_out","table":"glidex_meter","handle":5,"packets":0,"bytes":0}}}}]}}"#
    );
    exec.on(
        "nft -j list counters table inet glidex_meter",
        Output::ok(listed),
    );
    netd.handle(Op::ReleaseVm { vm_id: VM.into() }, &peer())
        .unwrap();
    let after_release = meter_calls(&exec).pop().unwrap();
    let deletes: Vec<&str> = after_release
        .lines()
        .filter(|l| l.starts_with("delete"))
        .collect();
    assert_eq!(
        deletes,
        vec![
            format!("delete counter inet glidex_meter m_{mac}_in"),
            format!("delete counter inet glidex_meter m_{mac}_out")
        ]
    );
    assert!(
        exec.calls()
            .iter()
            .all(|c| !c.contains("delete table inet glidex_meter"))
    );
    // nat_counters reports the network's counters, normalized.
    let counters: Vec<glidex_ovs::nat_meter::NatCounter> =
        serde_json::from_value(netd.handle(Op::NatCounters, &peer()).unwrap()).unwrap();
    assert_eq!(counters.len(), 2, "{counters:?}");
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
    exec.file(
        "/var/lib/glidex/dnsmasq/gxbr-nat.leases",
        "4000000000 02:89:53:f9:ac:2d 10.88.0.2 * ff:b5\n",
    );

    let res: AttachResult =
        serde_json::from_value(netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap())
            .unwrap();
    assert_eq!(res.ipv4.unwrap().to_string(), "10.88.0.3");

    netd.handle(Op::ReleaseVm { vm_id: VM.into() }, &peer())
        .unwrap();
    netd.handle(
        Op::DeleteNat {
            bridge: "gxbr-nat".into(),
        },
        &peer(),
    )
    .unwrap();
    assert!(
        exec.calls()
            .contains(&"rm /var/lib/glidex/dnsmasq/gxbr-nat.leases".to_string()),
        "{:#?}",
        exec.calls()
    );
}

#[test]
fn deletes_are_refused_while_in_use() {
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec(), &dir);
    setup_nat(&netd);
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();

    let err = netd
        .handle(
            Op::DeleteNat {
                bridge: "gxbr-nat".into(),
            },
            &peer(),
        )
        .unwrap_err();
    assert_eq!(err.code(), "conflict");
    let err = netd
        .handle(
            Op::DeleteBridge {
                name: "gxbr-nat".into(),
            },
            &peer(),
        )
        .unwrap_err();
    assert_eq!(err.code(), "conflict");

    netd.handle(
        Op::DetachVmPort {
            vm_id: VM.into(),
            nic_index: 0,
        },
        &peer(),
    )
    .unwrap();
    netd.handle(
        Op::DeleteNat {
            bridge: "gxbr-nat".into(),
        },
        &peer(),
    )
    .unwrap();
    netd.handle(
        Op::DeleteBridge {
            name: "gxbr-nat".into(),
        },
        &peer(),
    )
    .unwrap();
}

fn other_peer() -> Peer {
    Peer {
        uid: 2000,
        gid: 2000,
        pid: 2,
    }
}

fn root_peer() -> Peer {
    Peer {
        uid: 0,
        gid: 0,
        pid: 3,
    }
}

fn port_count(netd: &Netd) -> usize {
    serde_json::from_value::<Vec<VmPortRecord>>(netd.handle(Op::ListVmPorts, &peer()).unwrap())
        .unwrap()
        .len()
}

#[test]
fn only_the_owner_detaches_and_releases() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    let (netd, _) = netd(exec.clone(), &dir);
    setup_nat(&netd);
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();
    exec.clear_calls();

    let err = netd
        .handle(
            Op::DetachVmPort {
                vm_id: VM.into(),
                nic_index: 0,
            },
            &other_peer(),
        )
        .unwrap_err();
    assert_eq!(err.code(), "not_owned");
    assert!(err.to_string().contains("uid 1000"), "{err}");
    let err = netd
        .handle(Op::ReleaseVm { vm_id: VM.into() }, &other_peer())
        .unwrap_err();
    assert_eq!(err.code(), "not_owned");
    assert!(
        !exec.calls().iter().any(|c| c.contains("del-port")),
        "nothing detached: {:#?}",
        exec.calls()
    );
    assert_eq!(port_count(&netd), 1);
    let nats: Vec<NatInfo> =
        serde_json::from_value(netd.handle(Op::ListNat, &peer()).unwrap()).unwrap();
    assert_eq!(nats[0].state.reservations.len(), 1, "reservation kept");

    // The owner may.
    netd.handle(
        Op::DetachVmPort {
            vm_id: VM.into(),
            nic_index: 0,
        },
        &peer(),
    )
    .unwrap();
    assert_eq!(port_count(&netd), 0);
    // Without a record there is no owner to check (detach is idempotent).
    netd.handle(
        Op::DetachVmPort {
            vm_id: VM.into(),
            nic_index: 0,
        },
        &other_peer(),
    )
    .unwrap();
}

#[test]
fn root_may_detach_and_release_any_port() {
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec(), &dir);
    setup_nat(&netd);
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();
    netd.handle(
        Op::DetachVmPort {
            vm_id: VM.into(),
            nic_index: 0,
        },
        &root_peer(),
    )
    .unwrap();
    netd.handle(Op::AttachVmPort(port_spec()), &peer()).unwrap();
    netd.handle(Op::ReleaseVm { vm_id: VM.into() }, &root_peer())
        .unwrap();
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
    let nic1 = VmPortSpec {
        nic_index: 1,
        mac: names::mac_address(VM, 1).unwrap(),
        ..port_spec()
    };
    netd.handle(Op::AttachVmPort(nic1), &other_peer()).unwrap();
    exec.clear_calls();

    let err = netd
        .handle(Op::ReleaseVm { vm_id: VM.into() }, &peer())
        .unwrap_err();
    assert_eq!(err.code(), "not_owned");
    assert!(
        !exec.calls().iter().any(|c| c.contains("del-port")),
        "no partial release: {:#?}",
        exec.calls()
    );
    assert_eq!(port_count(&netd), 2);
}

#[test]
fn attach_requires_a_glidex_bridge() {
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec(), &dir);
    let err = netd
        .handle(Op::AttachVmPort(port_spec()), &peer())
        .unwrap_err();
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

    let report: ReconcileReport = serde_json::from_value(
        netd.handle(Op::SyncVms { running: vec![] }, &peer())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(report.detached, vec!["gx1a2b3c4d-0"]);
    assert_eq!(
        report.orphans,
        vec!["gxdeadbeef-0"],
        "untracked ports are reported, not deleted"
    );
    assert!(
        !exec
            .calls()
            .iter()
            .any(|c| c.contains("del-port gxdeadbeef-0"))
    );
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
    assert!(
        exec.calls()
            .contains(&"ip addr replace 10.88.0.1/24 dev gxbr-nat".to_string())
    );
    assert!(sup.running("gxbr-nat"));
}

#[test]
fn probe_reports_host_status() {
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec(), &dir);
    let caps: HostCapabilities =
        serde_json::from_value(netd.handle(Op::Probe, &peer()).unwrap()).unwrap();
    assert!(caps.ovs_installed && caps.ovs_running);
}

// ---- socket layer ----------------------------------------------------------

fn primary_gid() -> u32 {
    nix::unistd::getgid().as_raw()
}

fn start(dir: &TempDir, access: Access, group_gid: Option<u32>) -> std::path::PathBuf {
    start_with_policy(dir, access, group_gid, test_policy(&["*"]))
}

fn start_with_policy(
    dir: &TempDir,
    access: Access,
    group_gid: Option<u32>,
    policy: Policy,
) -> std::path::PathBuf {
    let (netd, _) = netd_with_policy(exec(), dir, policy);
    let path = dir.path().join(format!("{:?}.sock", access));
    let listener = server::bind(
        &path,
        if access == Access::Status {
            0o666
        } else {
            0o660
        },
        group_gid,
    )
    .unwrap();
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
    let bridges: Vec<BridgeRecord> = client
        .call(Op::ListBridges, Duration::from_secs(5))
        .unwrap();
    assert!(bridges.is_empty());
    let err = client
        .call_value(
            Op::DeleteNat {
                bridge: "Bad Name!".into(),
            },
            Duration::from_secs(5),
        )
        .map(|_| ());
    // Unknown NAT is a no-op; the connection keeps working after errors too.
    assert!(err.is_ok());

    let mode = std::fs::metadata(&path)
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777)
        .unwrap();
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
    // Traffic counters are tenant data (spec/metering.md §5.4).
    for op in [
        Op::ListBridges,
        Op::ListVmPorts,
        Op::PortStats,
        Op::NatCounters,
        Op::SyncVms { running: vec![] },
    ] {
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

    stream
        .write_all(b"{\"id\":1,\"op\":\"list_bridges\"}\n")
        .unwrap();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("first request must be hello"), "{line}");

    line.clear();
    stream.write_all(b"not json\n").unwrap();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("protocol_error"), "{line}");

    line.clear();
    stream
        .write_all(b"{\"id\":2,\"op\":\"exec\",\"args\":{\"cmd\":\"sh\"}}\n")
        .unwrap();
    reader.read_line(&mut line).unwrap();
    assert!(
        line.contains("protocol_error"),
        "unknown ops don't parse: {line}"
    );
}

fn expect_denied(client: &mut Client, op: Op) {
    let name = op.name();
    match client.call_value(op, Duration::from_secs(5)) {
        Err(ClientError::Remote(body)) => {
            assert_eq!(body.code, "permission_denied", "{name}: {body:?}")
        }
        other => panic!("{name}: expected permission_denied, got {other:?}"),
    }
}

#[test]
fn policy_limits_ops_per_group() {
    if nix::unistd::getuid().is_root() {
        return; // root is exempt from the policy
    }
    let dir = TempDir::new().unwrap();
    let path = start_with_policy(
        &dir,
        Access::Full,
        Some(primary_gid()),
        test_policy(&["list_*", "delete_nat"]),
    );
    let mut client = Client::connect(&path).expect("hello is always allowed");
    let _: Vec<BridgeRecord> = client
        .call(Op::ListBridges, Duration::from_secs(5))
        .unwrap();
    let _: Vec<NatInfo> = client.call(Op::ListNat, Duration::from_secs(5)).unwrap();
    client
        .call_value(
            Op::DeleteNat {
                bridge: "gxbr-x".into(),
            },
            Duration::from_secs(5),
        )
        .unwrap();
    let _: HostCapabilities = client.call(Op::Probe, Duration::from_secs(5)).unwrap();
    expect_denied(
        &mut client,
        Op::DeleteBridge {
            name: "gxbr-x".into(),
        },
    );
    expect_denied(&mut client, Op::SyncVms { running: vec![] });
    expect_denied(&mut client, Op::ReleaseVm { vm_id: VM.into() });
    // The connection keeps working after a denial.
    let _: Vec<BridgeRecord> = client
        .call(Op::ListBridges, Duration::from_secs(5))
        .unwrap();
}

#[test]
fn policy_without_the_peers_groups_denies_everything_but_hello_and_probe() {
    if nix::unistd::getuid().is_root() {
        return;
    }
    let dir = TempDir::new().unwrap();
    // The default policy names glidex and glidex-admin; give the test a
    // policy whose only group surely isn't the user's.
    let policy: Policy = [(
        "no-such-group-glidex-test".to_string(),
        vec!["*".to_string()],
    )]
    .into_iter()
    .collect();
    let path = start_with_policy(&dir, Access::Full, Some(primary_gid()), policy);
    let mut client = Client::connect(&path).unwrap();
    let _: HostCapabilities = client.call(Op::Probe, Duration::from_secs(5)).unwrap();
    expect_denied(&mut client, Op::ListBridges);
    expect_denied(
        &mut client,
        Op::DetachVmPort {
            vm_id: VM.into(),
            nic_index: 0,
        },
    );
}

#[test]
fn admin_socket_serves_admin_group_members_under_the_policy() {
    let dir = TempDir::new().unwrap();
    let path = start_with_policy(
        &dir,
        Access::Admin,
        Some(primary_gid()),
        test_policy(&["list_*"]),
    );
    let mut client = Client::connect(&path).unwrap();
    let _: Vec<BridgeRecord> = client
        .call(Op::ListBridges, Duration::from_secs(5))
        .unwrap();
    if !nix::unistd::getuid().is_root() {
        expect_denied(&mut client, Op::SyncVms { running: vec![] });
    }
    let mode = std::fs::metadata(&path)
        .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777)
        .unwrap();
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
    assert!(matches!(
        Client::connect(&path),
        Err(ClientError::Unavailable(_))
    ));
    // No admin group on the host: nobody but root.
    let dir = TempDir::new().unwrap();
    let path = start(&dir, Access::Admin, None);
    assert!(matches!(
        Client::connect(&path),
        Err(ClientError::Unavailable(_))
    ));
}

#[test]
fn on_behalf_of_is_accepted_and_optional() {
    use std::io::{BufRead, BufReader, Write};
    let dir = TempDir::new().unwrap();
    let path = start(&dir, Access::Full, Some(primary_gid()));
    let mut client = Client::connect(&path).unwrap();
    let obo = OnBehalfOf {
        user: "alice".into(),
        project: Some("default".into()),
        request_id: Some("req-1".into()),
    };
    let _: ReconcileReport = client
        .call_as(
            Op::SyncVms { running: vec![] },
            Some(&obo),
            Duration::from_secs(5),
        )
        .unwrap();
    let _: Vec<BridgeRecord> = client
        .call_as(Op::ListBridges, None, Duration::from_secs(5))
        .unwrap();

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
    exec.on(
        "ovs-vsctl --format=json --columns=name,error,link_state find Interface",
        Output::ok(
            r#"{"data":[["gxup0",["set",[]],"up"]],"headings":["name","error","link_state"]}"#,
        ),
    );
    exec.file("/sys/class/net/gxup0", "");
    exec.file("/sys/class/net/gxup0/address", "52:54:00:12:34:56\n");
    let addr = if nic_addr {
        r#"{"family":"inet","local":"192.0.2.10","prefixlen":24,"scope":"global"}"#
    } else {
        ""
    };
    exec.on(
        "ip -j addr show dev gxup0",
        Output::ok(format!(r#"[{{"addr_info":[{addr}]}}]"#)),
    );
    exec.on(
        "ip -4 -j route show dev gxup0",
        Output::ok(r#"[{"dst":"default","gateway":"192.0.2.1","protocol":"static"}]"#),
    );
    let state = if gateway_ok {
        r#"["REACHABLE"],"lladdr":"aa:bb:cc:dd:ee:ff""#
    } else {
        r#"["FAILED"]"#
    };
    exec.on(
        "ip -j neigh show 192.0.2.1 dev gxbr-up",
        Output::ok(format!(r#"[{{"dst":"192.0.2.1","state":{state}}}]"#)),
    );
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
    netd.handle(
        Op::EnsureBridge(BridgeSpec {
            name: "gxbr-up".into(),
            datapath: Datapath::System,
            mtu: None,
            adopt: false,
            isolated: false,
        }),
        &peer(),
    )
    .unwrap();
    netd
}

fn kernel_uplink(migrate: bool) -> UplinkSpec {
    UplinkSpec {
        name: "gxup0".into(),
        bridge: "gxbr-up".into(),
        kind: UplinkKind::Kernel {
            ifname: "gxup0".into(),
        },
        migrate_ip: migrate,
    }
}

fn ensure(
    netd: &Netd,
    spec: UplinkSpec,
    confirm: bool,
) -> Result<UplinkResult, glidex_ovs::OvsError> {
    netd.handle(
        Op::EnsureUplink(EnsureUplinkArgs {
            spec,
            confirm,
            accept_dhcp: false,
        }),
        &peer(),
    )
    .map(|v| serde_json::from_value(v).unwrap())
}

#[test]
fn free_nic_becomes_active_uplink() {
    let dir = TempDir::new().unwrap();
    let exec = uplink_exec(false, true);
    let netd = up_netd(exec.clone(), &dir, Duration::from_secs(60));
    let res = ensure(&netd, kernel_uplink(false), false).unwrap();
    assert_eq!(res.phase, UplinkPhase::Active);
    assert!(
        exec.calls()
            .iter()
            .any(|c| c.starts_with("ovs-vsctl --may-exist add-port gxbr-up gxup0"))
    );
    // Idempotent.
    assert_eq!(
        ensure(&netd, kernel_uplink(false), false).unwrap().phase,
        UplinkPhase::Active
    );
    // A bridge with uplinks can't be deleted.
    assert_eq!(
        netd.handle(
            Op::DeleteBridge {
                name: "gxbr-up".into()
            },
            &peer()
        )
        .unwrap_err()
        .code(),
        "conflict"
    );
}

#[test]
fn in_use_nic_needs_migrate_and_confirm() {
    let dir = TempDir::new().unwrap();
    let exec = uplink_exec(true, true);
    let netd = up_netd(exec.clone(), &dir, Duration::from_secs(60));
    for (migrate, confirm) in [(false, false), (true, false), (false, true)] {
        let err = ensure(&netd, kernel_uplink(migrate), confirm).unwrap_err();
        assert_eq!(
            err.code(),
            "host_interface_in_use",
            "migrate={migrate} confirm={confirm}"
        );
    }
    assert!(
        !exec
            .calls()
            .iter()
            .any(|c| c.contains("add-port gxbr-up gxup0")),
        "nothing changed"
    );
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
    assert!(
        exec.calls()
            .contains(&"ip addr replace 192.0.2.10/24 dev gxbr-up".to_string())
    );

    let err = netd
        .handle(
            Op::CommitUplink {
                bridge: "gxbr-up".into(),
                name: "gxup0".into(),
                token: "wrong".into(),
            },
            &peer(),
        )
        .unwrap_err();
    assert_eq!(err.code(), "invalid_argument");
    let ok: UplinkResult = serde_json::from_value(
        netd.handle(
            Op::CommitUplink {
                bridge: "gxbr-up".into(),
                name: "gxup0".into(),
                token,
            },
            &peer(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(ok.phase, UplinkPhase::Active);
    assert!(
        netd.expire_pending().is_empty(),
        "committed uplinks never expire"
    );

    // Deleting gives the NIC its address back.
    exec.clear_calls();
    netd.handle(
        Op::DeleteUplink {
            bridge: "gxbr-up".into(),
            name: "gxup0".into(),
        },
        &peer(),
    )
    .unwrap();
    let calls = exec.calls();
    assert!(calls.contains(&"ovs-vsctl --if-exists del-port gxup0".to_string()));
    assert!(
        calls.contains(&"ip addr replace 192.0.2.10/24 dev gxup0".to_string()),
        "{calls:#?}"
    );
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
    let uplinks: Vec<UplinkResult> =
        serde_json::from_value(netd.handle(Op::ListUplinks, &peer()).unwrap()).unwrap();
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
    assert!(
        calls.contains(&"ip addr replace 192.0.2.10/24 dev gxup0".to_string()),
        "address restored"
    );
    assert!(calls.contains(&"ovs-vsctl --if-exists del-port gxup0".to_string()));
}

#[test]
fn uplinks_are_refused_on_nat_bridges() {
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec(), &dir);
    setup_nat(&netd);
    let spec = UplinkSpec {
        name: "gxup0".into(),
        bridge: "gxbr-nat".into(),
        kind: UplinkKind::Kernel {
            ifname: "gxup0".into(),
        },
        migrate_ip: false,
    };
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
    let netd = Netd::new(
        exec.clone(),
        sup,
        Config {
            run_dir: dir.path().join("run"),
            state_path: dir.path().join("netd.db"),
            ..Config::default()
        },
    )
    .unwrap();
    netd.handle(
        Op::EnsureBridge(BridgeSpec {
            name: "gxbr-up".into(),
            datapath: Datapath::Netdev,
            mtu: None,
            adopt: false,
            isolated: false,
        }),
        &peer(),
    )
    .unwrap();

    let spec = UplinkSpec {
        name: "up0".into(),
        bridge: "gxbr-up".into(),
        kind: UplinkKind::Dpdk {
            pci: "0000:41:00.0".into(),
            n_rxq: 2,
        },
        migrate_ip: false,
    };
    let res = ensure(&netd, spec, false).unwrap();
    assert_eq!(res.record.orig_driver.as_deref(), Some("ixgbe"));
    assert!(
        exec.writes()
            .iter()
            .any(|(p, d)| p.ends_with("driver_override") && d == b"vfio-pci")
    );
    netd.handle(
        Op::DeleteUplink {
            bridge: "gxbr-up".into(),
            name: "up0".into(),
        },
        &peer(),
    )
    .unwrap();
    assert!(
        exec.writes()
            .iter()
            .any(|(p, d)| p.ends_with("driver_override") && d == b"\n"),
        "driver restored"
    );
}

#[test]
fn committed_migrations_are_reapplied_at_start() {
    let dir = TempDir::new().unwrap();
    {
        let netd = up_netd(uplink_exec(true, true), &dir, Duration::from_secs(60));
        let res = ensure(&netd, kernel_uplink(true), true).unwrap();
        let token = res.record.pending.unwrap().token;
        netd.handle(
            Op::CommitUplink {
                bridge: "gxbr-up".into(),
                name: "gxup0".into(),
                token,
            },
            &peer(),
        )
        .unwrap();
    }
    // After a reboot the host puts the address back on the NIC; netd moves it again.
    let exec = uplink_exec(true, true);
    let netd = Netd::new(
        exec.clone(),
        Arc::new(FakeSupervisor::default()),
        Config {
            run_dir: dir.path().join("run"),
            state_path: dir.path().join("netd.db"),
            ..Config::default()
        },
    )
    .unwrap();
    let report = netd.reconcile_startup();
    assert!(report.errors.is_empty(), "{report:?}");
    let calls = exec.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("ovs-vsctl --may-exist add-port gxbr-up gxup0"))
    );
    assert!(calls.contains(&"ip addr replace 192.0.2.10/24 dev gxbr-up".to_string()));
}

fn restarted(exec: Arc<RecordingExec>, dir: &TempDir) -> Netd {
    Netd::new(
        exec,
        Arc::new(FakeSupervisor::default()),
        Config {
            run_dir: dir.path().join("run"),
            state_path: dir.path().join("netd.db"),
            gateway_check: Duration::from_millis(300),
            ..Config::default()
        },
    )
    .unwrap()
}

fn uplinks(netd: &Netd) -> Vec<UplinkResult> {
    serde_json::from_value(netd.handle(Op::ListUplinks, &peer()).unwrap()).unwrap()
}

/// A migration that fails when re-applied at start is rolled back so the host
/// keeps its address, held until re-applied, and re-applied with the first
/// setup's gateway check and commit window.
#[test]
fn a_migration_that_fails_at_start_is_rolled_back_held_and_reapplied() {
    let dir = TempDir::new().unwrap();
    {
        let netd = up_netd(uplink_exec(true, true), &dir, Duration::from_secs(60));
        let token = ensure(&netd, kernel_uplink(true), true)
            .unwrap()
            .record
            .pending
            .unwrap()
            .token;
        netd.handle(
            Op::CommitUplink {
                bridge: "gxbr-up".into(),
                name: "gxup0".into(),
                token,
            },
            &peer(),
        )
        .unwrap();
    }
    // The next boot: the gateway doesn't answer through the bridge.
    let exec = uplink_exec(true, false);
    let netd = restarted(exec.clone(), &dir);
    let report = netd.reconcile_startup();
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("gxup0") && e.contains("did not answer")),
        "{report:?}"
    );
    let calls = exec.calls();
    assert!(
        calls.contains(&"ip addr replace 192.0.2.10/24 dev gxup0".to_string()),
        "the NIC has its address back: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("del-port") && c.contains("gxup0")),
        "off the bridge: {calls:?}"
    );
    let held = uplinks(&netd)
        .into_iter()
        .find(|u| u.record.spec.name == "gxup0")
        .expect("the record stays");
    assert!(
        held.record
            .boot_rollback
            .as_ref()
            .is_some_and(|b| b.reason.contains("did not answer")),
        "{:?}",
        held.record
    );
    drop(netd);

    // Another boot leaves it alone: held until someone re-applies it.
    let exec = uplink_exec(true, true);
    let netd = restarted(exec.clone(), &dir);
    let report = netd.reconcile_startup();
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("held off the bridge")),
        "{report:?}"
    );
    assert!(
        !exec
            .calls()
            .iter()
            .any(|c| c.starts_with("ovs-vsctl --may-exist add-port gxbr-up gxup0")),
        "not re-added"
    );

    // Re-applied: a fresh migration, pending a commit like the first time.
    let r: UplinkResult = serde_json::from_value(
        netd.handle(
            Op::ReapplyUplink {
                bridge: "gxbr-up".into(),
                name: "gxup0".into(),
            },
            &peer(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(r.phase, UplinkPhase::PendingCommit);
    assert!(r.record.boot_rollback.is_none());
    let err = netd
        .handle(
            Op::ReapplyUplink {
                bridge: "gxbr-up".into(),
                name: "gxup0".into(),
            },
            &peer(),
        )
        .unwrap_err();
    assert_eq!(err.code(), "conflict", "only a held uplink is re-applied");
}

/// A DHCP-managed address is moved only when that is accepted explicitly.
#[test]
fn a_dhcp_managed_address_needs_an_explicit_yes() {
    let dir = TempDir::new().unwrap();
    let exec = uplink_exec(true, true);
    exec.file("/sys/class/net/gxup0/ifindex", "3\n");
    exec.file("/run/systemd/netif/leases/3", "ADDRESS=192.0.2.10\n");
    let netd = up_netd(exec, &dir, Duration::from_secs(60));
    let err = ensure(&netd, kernel_uplink(true), true).unwrap_err();
    assert!(err.to_string().contains("DHCP-managed"), "{err}");
    let r: UplinkResult = serde_json::from_value(
        netd.handle(
            Op::EnsureUplink(EnsureUplinkArgs {
                spec: kernel_uplink(true),
                confirm: true,
                accept_dhcp: true,
            }),
            &peer(),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(r.record.accept_dhcp);
    assert!(
        r.live.warnings.iter().any(|w| w.contains("DHCP")),
        "{:?}",
        r.live.warnings
    );
}

#[test]
fn uncommitted_migrations_are_rolled_back_at_start() {
    let dir = TempDir::new().unwrap();
    {
        let netd = up_netd(uplink_exec(true, true), &dir, Duration::from_secs(600));
        ensure(&netd, kernel_uplink(true), true).unwrap();
    }
    let exec = uplink_exec(true, true);
    let netd = Netd::new(
        exec.clone(),
        Arc::new(FakeSupervisor::default()),
        Config {
            run_dir: dir.path().join("run"),
            state_path: dir.path().join("netd.db"),
            ..Config::default()
        },
    )
    .unwrap();
    let report = netd.reconcile_startup();
    assert!(
        report
            .repaired
            .iter()
            .any(|r| r.contains("rolled back uncommitted uplink gxup0")),
        "{report:?}"
    );
    assert!(
        exec.calls()
            .contains(&"ip addr replace 192.0.2.10/24 dev gxup0".to_string())
    );
}

/// Isolated networks' bridges are fenced off from the host and from
/// forwarding (security spec §8.4): defense in depth, should their host
/// interface come up with an IPv6 link-local address.
#[test]
fn isolated_bridges_are_fenced_off_from_the_host() {
    let dir = TempDir::new().unwrap();
    let exec = exec();
    let (netd, _sup) = netd(exec.clone(), &dir);
    let bridge = |isolated: bool| {
        Op::EnsureBridge(BridgeSpec {
            name: "gxbr-nat".into(),
            datapath: Datapath::System,
            mtu: None,
            adopt: false,
            isolated,
        })
    };
    let last_nft = |exec: &RecordingExec| {
        exec.calls()
            .into_iter()
            .rfind(|c| c.starts_with("nft -f - <<< table inet glidex\n"))
    };
    const DELETE_ONLY: &str = "nft -f - <<< table inet glidex\ndelete table inet glidex\n";

    netd.handle(bridge(true), &peer()).unwrap();
    let script = last_nft(&exec).expect("the fence is applied");
    assert!(
        script.contains("    iifname { \"gxbr-nat\" } drop\n  }\n}\n"),
        "{script}"
    );
    assert!(
        script.contains("oifname { \"gxbr-nat\" } drop"),
        "nothing is forwarded to it: {script}"
    );
    assert!(
        !script.contains("masquerade") && !script.contains("dport 67"),
        "{script}"
    );

    // An isolated bridge takes neither NAT nor an uplink.
    let nat = netd.handle(
        Op::EnsureNat(NatSpec {
            bridge: "gxbr-nat".into(),
            subnet: None,
            dns: true,
        }),
        &peer(),
    );
    assert_eq!(nat.unwrap_err().code(), "conflict");
    let up = UplinkSpec {
        name: "gxup0".into(),
        bridge: "gxbr-nat".into(),
        kind: UplinkKind::Kernel {
            ifname: "gxup0".into(),
        },
        migrate_ip: false,
    };
    assert_eq!(ensure(&netd, up, false).unwrap_err().code(), "conflict");

    // A restarted netd fences it again, with no NAT network to rebuild the table.
    let calls_before = exec.calls().len();
    drop(netd);
    let (netd, _sup) = self::netd(exec.clone(), &dir);
    let report = netd.reconcile_startup();
    assert!(
        report
            .repaired
            .iter()
            .any(|r| r == "isolated bridges fenced"),
        "{report:?}"
    );
    assert!(
        exec.calls()[calls_before..]
            .iter()
            .any(|c| c.contains("iifname { \"gxbr-nat\" } drop"))
    );

    // No longer isolated: the fence goes.
    netd.handle(bridge(false), &peer()).unwrap();
    assert_eq!(last_nft(&exec).as_deref(), Some(DELETE_ONLY));
    // Deleting an isolated bridge removes its fence too.
    netd.handle(bridge(true), &peer()).unwrap();
    netd.handle(
        Op::DeleteBridge {
            name: "gxbr-nat".into(),
        },
        &peer(),
    )
    .unwrap();
    assert_eq!(last_nft(&exec).as_deref(), Some(DELETE_ONLY));
}

#[test]
fn ct_external_counters_turn_accounting_on_account_and_persist() {
    let dir = TempDir::new().unwrap();
    let ex = exec();
    let (netd, _) = netd(ex.clone(), &dir);
    ex.on(
        "conntrack -L -f ipv4 -o extended,id",
        Output::ok("ipv4 2 tcp 6 431999 ESTABLISHED src=10.89.0.5 dst=203.0.113.9 sport=1 dport=80 packets=2 bytes=200 src=203.0.113.9 dst=192.0.2.3 sport=80 dport=1 packets=2 bytes=400 [ASSURED] mark=0 zone=60001 use=1 id=5\nipv4 2 tcp 6 10 ESTABLISHED src=10.0.0.1 dst=10.0.0.2 sport=1 dport=2 packets=1 bytes=60 src=10.0.0.2 dst=10.0.0.1 sport=2 dport=1 packets=1 bytes=60 mark=0 zone=7 use=1 id=6"),
    );
    let c: glidex_ovs::ct_meter::CtCounters =
        serde_json::from_value(netd.handle(Op::CtExternalCounters, &peer()).unwrap()).unwrap();
    assert_eq!(c.counters.len(), 1, "only router zones are metered");
    assert_eq!(
        (c.counters[0].tx_bytes, c.counters[0].rx_bytes),
        (200 + 2 * 14, 400 + 2 * 14)
    );
    assert!(
        ex.calls()
            .iter()
            .any(|c| c == "sysctl -w net.netfilter.nf_conntrack_acct=1"),
        "{:?}",
        ex.calls()
    );
    assert!(!Op::CtExternalCounters.is_status() && !Op::CtExternalCounters.is_mutating());
}

#[test]
fn a_port_moves_between_bridges_without_recreating_its_tap() {
    let dir = TempDir::new().unwrap();
    let ex = exec();
    let (netd, _) = netd(ex.clone(), &dir);
    setup_nat(&netd);
    let mut spec = port_spec();
    netd.handle(Op::AttachVmPort(spec.clone()), &peer())
        .unwrap();
    let tap_adds = |ex: &RecordingExec| {
        ex.calls()
            .iter()
            .filter(|c| c.starts_with("ip tuntap add"))
            .count()
    };
    assert_eq!(tap_adds(&ex), 1);
    // The tap exists on the host now.
    ex.file("/sys/class/net/gx1a2b3c4d-0", "");
    // To br-int as an OVN port: the old OVS port goes, the new one comes, the tap stays.
    spec.bridge = "br-int".into();
    spec.ovn_lport = Some("gx1a2b3c4d-0".into());
    ex.on(
        "ovs-vsctl --format=json --columns=name,external_ids,type find Interface",
        Output::ok(OWNED_IF),
    );
    ex.on("ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge", Output::ok(r#"{"data":[["br-int","system",["map",[["glidex-owner","glidex"],["glidex-role","bridge"]]],["set",[]]]],"headings":["name","datapath_type","external_ids","ports"]}"#));
    netd.handle(Op::MoveVmPort(spec), &peer()).unwrap();
    let calls = ex.calls();
    assert!(
        calls.iter().any(|c| c.contains("--if-exists del-port")),
        "{calls:?}"
    );
    assert_eq!(tap_adds(&ex), 1, "the tap was not recreated: {calls:?}");
    assert!(
        !calls.iter().any(|c| c.starts_with("ip link del")),
        "{calls:?}"
    );
}

/// spec/host-topology.md §5.3: a device's IOMMU group is bound to
/// vfio-pci, recorded, listed and put back; a device netd didn't prepare
/// can't be released.
#[test]
fn devices_are_prepared_for_passthrough_and_released() {
    let exec = exec();
    for (bdf, class, driver) in [
        ("0000:07:00.0", "0x030200", "nvidia"),
        ("0000:07:00.1", "0x040300", "snd_hda_intel"),
    ] {
        exec.file(&format!("/sys/bus/pci/devices/{bdf}"), "");
        exec.file(&format!("/sys/bus/pci/devices/{bdf}/class"), class);
        exec.file(
            &format!("/sys/bus/pci/devices/{bdf}/uevent"),
            &format!("DRIVER={driver}\n"),
        );
        exec.link(&format!("/sys/bus/pci/devices/{bdf}/iommu_group"), "30");
        exec.link(&format!("/sys/kernel/iommu_groups/30/devices/{bdf}"), bdf);
    }
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec.clone(), &dir);
    let p = netd
        .handle(
            Op::PrepareDevice {
                address: "0000:07:00.0".into(),
            },
            &peer(),
        )
        .unwrap();
    assert_eq!(p["iommu_group"], "30", "{p}");
    assert_eq!(p["members"].as_array().unwrap().len(), 2, "{p}");
    let listed = netd.handle(Op::ListPreparedDevices, &peer()).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    // Either function of the group releases it.
    netd.handle(
        Op::ReleaseDevice {
            address: "0000:07:00.1".into(),
        },
        &peer(),
    )
    .unwrap();
    let listed = netd.handle(Op::ListPreparedDevices, &peer()).unwrap();
    assert!(listed.as_array().unwrap().is_empty(), "{listed}");
    let e = netd
        .handle(
            Op::ReleaseDevice {
                address: "0000:07:00.0".into(),
            },
            &peer(),
        )
        .unwrap_err();
    assert!(e.to_string().contains("was not prepared"), "{e}");
}

/// spec/host-topology.md §10.1: a PF's VFs are set through `sriov_numvfs`,
/// and a prepared VF is forgotten once the change removes it.
#[test]
fn sriov_vfs_are_set_and_removed_vfs_forgotten() {
    let exec = exec();
    let pf = "/sys/bus/pci/devices/0000:61:00.0";
    exec.file(pf, "");
    exec.file(&format!("{pf}/sriov_totalvfs"), "8\n");
    exec.file(&format!("{pf}/sriov_numvfs"), "1\n");
    exec.link(&format!("{pf}/virtfn0"), "0000:61:00.2");
    let vf = "0000:61:00.2";
    exec.file(&format!("/sys/bus/pci/devices/{vf}"), "");
    exec.file(&format!("/sys/bus/pci/devices/{vf}/class"), "0x020000");
    exec.file(
        &format!("/sys/bus/pci/devices/{vf}/uevent"),
        "DRIVER=mlx5_core\n",
    );
    exec.link(&format!("/sys/bus/pci/devices/{vf}/iommu_group"), "40");
    exec.link(&format!("/sys/kernel/iommu_groups/40/devices/{vf}"), vf);
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec.clone(), &dir);
    netd.handle(Op::PrepareDevice { address: vf.into() }, &peer())
        .unwrap();
    let s = netd
        .handle(
            Op::SetSriovVfs {
                address: "0000:61:00.0".into(),
                vfs: 4,
            },
            &peer(),
        )
        .unwrap();
    assert_eq!(
        (s["total"].as_u64(), s["enabled"].as_u64()),
        (Some(8), Some(4)),
        "{s}"
    );
    let listed = netd.handle(Op::ListPreparedDevices, &peer()).unwrap();
    assert!(
        listed.as_array().unwrap().is_empty(),
        "the old VF went: {listed}"
    );
    assert!(
        exec.calls()
            .contains(&"rm /etc/udev/rules.d/90-glidex-vfio-40.rules".to_string())
    );
    let e = netd
        .handle(
            Op::SetSriovVfs {
                address: "0000:61:00.0".into(),
                vfs: 9,
            },
            &peer(),
        )
        .unwrap_err();
    assert!(e.to_string().contains("at most 8"), "{e}");
}

/// spec/host-topology.md §5.1: a device whose IOMMU group a process has
/// open (a VM, glidex's or libvirt's) isn't released, and its PF's VF count
/// isn't changed; device_holders says who holds it.
#[test]
fn held_devices_are_not_released_and_their_vf_counts_stay() {
    let exec = exec();
    let gpu = "0000:87:00.0";
    exec.file(&format!("/sys/bus/pci/devices/{gpu}"), "");
    exec.file(&format!("/sys/bus/pci/devices/{gpu}/class"), "0x030200\n");
    exec.file(
        &format!("/sys/bus/pci/devices/{gpu}/uevent"),
        "DRIVER=nvidia\n",
    );
    exec.link(&format!("/sys/bus/pci/devices/{gpu}/iommu_group"), "226");
    exec.link(&format!("/sys/kernel/iommu_groups/226/devices/{gpu}"), gpu);
    // A PF with one VF, in group 41.
    let pf = "/sys/bus/pci/devices/0000:61:00.0";
    exec.file(pf, "");
    exec.file(&format!("{pf}/sriov_totalvfs"), "8\n");
    exec.file(&format!("{pf}/sriov_numvfs"), "1\n");
    exec.link(&format!("{pf}/iommu_group"), "40");
    exec.link(&format!("{pf}/virtfn0"), "0000:61:00.2");
    exec.link("/sys/bus/pci/devices/0000:61:00.2/iommu_group", "41");
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec.clone(), &dir);
    netd.handle(
        Op::PrepareDevice {
            address: gpu.into(),
        },
        &peer(),
    )
    .unwrap();
    // A VM opens both groups.
    exec.file("/proc/37226/status", "Uid:\t9869\t9869\t9869\t9869\n");
    exec.file("/proc/37226/comm", "qemu-kvm-one\n");
    exec.link("/proc/37226/fd/30", "/dev/vfio/226");
    exec.link("/proc/37226/fd/31", "/dev/vfio/41");
    let e = netd
        .handle(
            Op::ReleaseDevice {
                address: gpu.into(),
            },
            &peer(),
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("in use by qemu-kvm-one (pid 37226)"),
        "{e}"
    );
    let e = netd
        .handle(
            Op::SetSriovVfs {
                address: "0000:61:00.0".into(),
                vfs: 4,
            },
            &peer(),
        )
        .unwrap_err();
    assert!(e.to_string().contains("IOMMU group 41"), "{e}");
    // Asking for the count it has changes nothing, so it's allowed.
    netd.handle(
        Op::SetSriovVfs {
            address: "0000:61:00.0".into(),
            vfs: 1,
        },
        &peer(),
    )
    .unwrap();
    let h = netd
        .handle(
            Op::DeviceHolders {
                groups: vec!["226".into(), "236".into()],
            },
            &peer(),
        )
        .unwrap();
    assert_eq!(h["226"][0]["comm"], "qemu-kvm-one", "{h}");
    assert_eq!(h["226"][0]["uid"], 9869, "{h}");
    assert!(h.get("236").is_none(), "{h}");
}

/// spec/gpu-vms.md §10: boot settings bind the selected devices that aren't
/// on vfio-pci, never touch one that is, and (outside boot) leave a VF
/// count alone while one of its VFs is on vfio-pci.
#[test]
fn boot_devices_bind_what_is_free_and_leave_what_a_vm_may_hold() {
    use glidex_ovs::boot_devices::{BootDevices, Selector, SriovSetting};
    let exec = exec();
    for (bdf, group, driver) in [
        ("0000:87:00.0", "226", "nvidia"),
        ("0000:90:00.0", "236", "vfio-pci"),
    ] {
        let d = format!("/sys/bus/pci/devices/{bdf}");
        exec.file(&d, "");
        exec.file(&format!("{d}/vendor"), "0x10de\n");
        exec.file(&format!("{d}/device"), "0x20b0\n");
        exec.file(&format!("{d}/class"), "0x030200\n");
        exec.file(&format!("{d}/uevent"), &format!("DRIVER={driver}\n"));
        exec.link(&format!("{d}/driver"), driver);
        exec.link(&format!("{d}/iommu_group"), group);
        exec.link(
            &format!("/sys/kernel/iommu_groups/{group}/devices/{bdf}"),
            bdf,
        );
    }
    // A PF with 2 VFs, one of them on vfio-pci.
    let pf = "/sys/bus/pci/devices/0000:61:00.0";
    exec.file(pf, "");
    exec.file(&format!("{pf}/sriov_totalvfs"), "16\n");
    exec.file(&format!("{pf}/sriov_numvfs"), "2\n");
    exec.link(&format!("{pf}/virtfn0"), "0000:61:00.2");
    exec.link(&format!("{pf}/virtfn1"), "0000:61:00.3");
    exec.file("/sys/bus/pci/devices/0000:61:00.3", "");
    exec.link("/sys/bus/pci/devices/0000:61:00.3/driver", "vfio-pci");
    let dir = TempDir::new().unwrap();
    let (netd, _) = netd(exec.clone(), &dir);
    let cfg = BootDevices {
        sriov: vec![SriovSetting {
            pf: "0000:61:00.0".into(),
            vfs: 8,
        }],
        select: vec![Selector {
            vendor_id: Some("0x10de".into()),
            ..Default::default()
        }],
    };
    let r = netd.apply_boot_devices(&cfg, false);
    assert_eq!(r.bound, ["0000:87:00.0"], "{r:?}");
    assert!(r.failed.is_empty(), "{r:?}");
    assert!(
        r.skipped
            .iter()
            .any(|(a, why)| a == "0000:90:00.0" && why.contains("already on vfio-pci")),
        "{r:?}"
    );
    assert!(
        r.skipped
            .iter()
            .any(|(a, why)| a == "0000:61:00.0" && why.contains("0000:61:00.3")),
        "outside boot the VF count waits: {r:?}"
    );
    assert!(r.sriov.is_empty(), "{r:?}");
    let writes: Vec<String> = exec
        .writes()
        .into_iter()
        .map(|(p, _)| p.display().to_string())
        .collect();
    assert!(
        !writes.iter().any(|w| w.contains("0000:90:00.0")),
        "{writes:?}"
    );
    assert!(
        !writes.iter().any(|w| w.ends_with("sriov_numvfs")),
        "{writes:?}"
    );
    // Recorded like prepare_device, so release works.
    let listed = netd.handle(Op::ListPreparedDevices, &peer()).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    // At boot the VF count is set.
    let r = netd.apply_boot_devices(&cfg, true);
    assert_eq!(r.sriov, [("0000:61:00.0".to_string(), 8)], "{r:?}");
}
