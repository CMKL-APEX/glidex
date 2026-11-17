//! The DPU backend in netd (spec/31-dpu.md D3): the §6 table against an
//! in-process agent (the real agent code on a recording executor, fed the
//! test DPU's outputs), stickiness, the host's VF steps and the
//! management link.

use glidex_dpu_agent::{Agent, AgentApi, AgentError, AgentOp};
use glidex_netd::auth::Peer;
use glidex_netd::dpu::{self, DpuBackend, LinkState};
use glidex_netd::proto::*;
use glidex_netd::server::{Config, Netd};
use glidex_netd::supervisor::FakeSupervisor;
use glidex_ovs::vm_port::{PortBackend, VmPortBinding, VmPortKind, VmPortSpec};
use glidex_ovs::{Output, RecordingExec, names};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

const VM: &str = "1a2b3c4d-5e6f-4a1b-9c2d-0123456789ab";
const PF: &str = "/sys/bus/pci/devices/0000:61:00.1";

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../glidex-dpu/tests/fixtures/one-vf/dpu-{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// A tagged VM port on the DPU, as `ovs-vsctl find` prints it.
fn tagged(name: &str, vm: &str, nic: u8) -> String {
    format!(
        r#"["{name}",["map",[["glidex-nic","{nic}"],["glidex-owner","glidex"],["glidex-role","vm"],["glidex-vm-id","{vm}"]]]]"#
    )
}

/// The test DPU, prepared, with VF 0 there (fixture `one-vf`) and VM's
/// NIC 0 on its `br-int`.
fn dpu_exec() -> Arc<glidex_dpu::RecordingExec> {
    dpu_exec_with(&[tagged("en3f0pf0sf3000", VM, 0)])
}

/// The same with `ports` as the tagged ports on the DPU.
fn dpu_exec_with(ports: &[String]) -> Arc<glidex_dpu::RecordingExec> {
    use glidex_dpu::Output;
    let exec = Arc::new(glidex_dpu::RecordingExec::new());
    for (name, cmd) in glidex_dpu::probe::probe_cmds(glidex_dpu::probe::DEFAULT_MST_DEVICE) {
        exec.on(&cmd.display(), Output::ok(fixture(name)));
    }
    exec.file(
        "/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages",
        "1024\n",
    );
    exec.file(
        "/sys/kernel/mm/hugepages/hugepages-2048kB/free_hugepages",
        "954\n",
    );
    exec.on("devlink dev show", Output::ok("pci/0000:03:00.0\n"));
    exec.on("devlink dev eswitch show", Output::ok(fixture("eswitch")));
    exec.on(
        "systemctl is-active virtio-net-controller",
        Output::ok("active\n"),
    );
    exec.on(
        "systemctl is-enabled virtio-net-controller",
        Output::ok("enabled\n"),
    );
    exec.on(
        "ovs-vsctl --format=json --columns=name,external_ids find Interface",
        Output::ok(format!(
            r#"{{"data":[{}],"headings":["name","external_ids"]}}"#,
            ports.join(",")
        )),
    );
    exec
}

struct Down;

impl AgentApi for Down {
    fn call(&self, _: AgentOp) -> Result<serde_json::Value, AgentError> {
        Err(AgentError::Unavailable("connection refused".into()))
    }
}

/// The host: br-int for host ports, the virtio-net PF on `pci-pf-stub`
/// with `total` VFs whose drivers are already `vfio-pci`.
fn host_exec(total: u16) -> Arc<RecordingExec> {
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
        "ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge name=br-int",
        Output::ok(r#"{"data":[["br-int","system",["map",[["glidex-owner","glidex"]]],["set",[]]]],"headings":["name","datapath_type","external_ids","ports"]}"#),
    );
    exec.on(
        "ovs-vsctl --format=json --columns=name,external_ids,type find Interface",
        Output::ok(r#"{"data":[],"headings":["name","external_ids","type"]}"#),
    );
    exec.file(&format!("{PF}/sriov_totalvfs"), &format!("{total}\n"));
    exec.file(&format!("{PF}/sriov_numvfs"), "0\n");
    exec.file(&format!("{PF}/sriov_drivers_autoprobe"), "1\n");
    exec.link(&format!("{PF}/driver"), "pci-pf-stub");
    for n in 0..total {
        let vf = format!("0000:61:02.{}", 3 + n);
        exec.link(&format!("{PF}/virtfn{n}"), &vf);
        exec.link(&format!("/sys/bus/pci/devices/{vf}/driver"), "vfio-pci");
        exec.link(
            &format!("/sys/bus/pci/devices/{vf}/iommu_group"),
            &(30 + n).to_string(),
        );
    }
    exec.file("/dev/vfio/30", "");
    exec
}

fn dpu_config() -> dpu::DpuConfig {
    dpu::parse(
        br#"{"dpus":[{"id":"dpu0","agent":"tcp://192.0.2.2:7443",
        "link":{"iface":"tmfifo_net0","address":"192.0.2.1/30"},
        "tls":{"ca":"/nonexistent/ca.pem","cert":"/nonexistent/c.pem","key":"/nonexistent/c.key"},
        "host_pf":"0000:61:00.1","port_timeout_secs":1}]}"#,
    )
    .unwrap()
}

fn netd(host: Arc<RecordingExec>, agent: Arc<dyn AgentApi>, dir: &TempDir) -> Netd {
    let config = Config {
        run_dir: dir.path().join("run"),
        state_path: dir.path().join("netd.db"),
        ..Config::default()
    };
    Netd::new(host, Arc::new(FakeSupervisor::default()), config)
        .unwrap()
        .with_dpu(Arc::new(DpuBackend::new(dpu_config(), agent)))
}

fn healthy() -> Arc<dyn AgentApi> {
    Arc::new(Agent::new(dpu_exec()))
}

fn spec(nic: u8, backend: PortBackend) -> VmPortSpec {
    VmPortSpec {
        bridge: "br-int".into(),
        vm_id: VM.into(),
        nic_index: nic,
        kind: VmPortKind::Tap,
        mac: names::mac_address(VM, nic).unwrap(),
        vlan: None,
        mtu: Some(1442),
        queue_pairs: 1,
        ovn_lport: Some(format!("gx-1a2b3c4d-{nic}")),
        port_security: true,
        allowed_addresses: vec![],
        backend,
    }
}

fn root() -> Peer {
    Peer {
        uid: 0,
        gid: 0,
        pid: 1,
    }
}

fn attach(netd: &Netd, s: VmPortSpec) -> Result<AttachResult, String> {
    netd.handle(Op::AttachVmPort(s), &root())
        .map(|v| serde_json::from_value(v).unwrap())
        .map_err(|e| e.code().to_string())
}

#[test]
fn host_is_host_whatever_the_dpu() {
    let dir = TempDir::new().unwrap();
    let netd = netd(host_exec(2), Arc::new(Down), &dir);
    let r = attach(&netd, spec(0, PortBackend::Host)).unwrap();
    assert!(matches!(r.binding, VmPortBinding::Tap { .. }));
}

#[test]
fn dpu_on_a_healthy_dpu_gets_a_vfio_vf() {
    let dir = TempDir::new().unwrap();
    let host = host_exec(2);
    let netd = netd(host.clone(), healthy(), &dir);
    let r = attach(&netd, spec(0, PortBackend::Dpu)).unwrap();
    assert_eq!(
        r.binding,
        VmPortBinding::Vfio {
            bdf: "0000:61:02.3".into()
        }
    );
    assert_eq!(r.backend, glidex_dpu::Backend::Dpu);
    assert_eq!(r.port, "en3f0pf0sf3000");
    // §7.1 step 3: no autoprobe, then every VF at once.
    let writes: Vec<(PathBuf, String)> = host
        .writes()
        .into_iter()
        .map(|(p, d)| (p, String::from_utf8(d).unwrap()))
        .collect();
    let at = |f: &str| writes.iter().position(|(p, _)| p.ends_with(f));
    assert_eq!(writes[at("sriov_drivers_autoprobe").unwrap()].1, "0");
    assert_eq!(writes[at("sriov_numvfs").unwrap()].1, "2");
    assert!(at("sriov_drivers_autoprobe") < at("sriov_numvfs"));
    // Step 7: the VF's IOMMU group is the VMs' group's, as for a
    // passed-through device (spec/22-host-topology.md §5.3).
    let rule = &writes[at("90-glidex-vfio-30.rules").unwrap()].1;
    assert!(
        rule.contains("KERNEL==\"30\"") && rule.contains("MODE=\"0660\""),
        "{rule}"
    );
    assert!(
        host.calls()
            .iter()
            .any(|c| c.starts_with("chgrp ") && c.contains("/dev/vfio/30; chmod 660")),
        "{:?}",
        host.calls()
    );
    let records: Vec<VmPortRecord> =
        serde_json::from_value(netd.handle(Op::ListVmPorts, &root()).unwrap()).unwrap();
    assert_eq!(records[0].dpu_vf, Some(glidex_dpu::VfId { pf: 0, vf: 0 }));

    // Detach: off vfio-pci, off the DPU, record and VF gone.
    netd.handle(
        Op::DetachVmPort {
            vm_id: VM.into(),
            nic_index: 0,
        },
        &root(),
    )
    .unwrap();
    let w = host.writes();
    assert!(
        w.iter()
            .any(|(p, d)| p.ends_with("0000:61:02.3/driver_override") && d == b"\n")
    );
    assert!(
        w.iter()
            .any(|(p, _)| p.ends_with("0000:61:02.3/driver/unbind"))
    );
    assert!(
        host.calls()
            .contains(&"rm /etc/udev/rules.d/90-glidex-vfio-30.rules".to_string())
    );
    let records: Vec<VmPortRecord> =
        serde_json::from_value(netd.handle(Op::ListVmPorts, &root()).unwrap()).unwrap();
    assert!(records.is_empty());
}

#[test]
fn dpu_refuses_when_the_dpu_is_down_or_full_and_auto_falls_back() {
    let dir = TempDir::new().unwrap();
    let netd_down = netd(host_exec(2), Arc::new(Down), &dir);
    assert_eq!(
        attach(&netd_down, spec(0, PortBackend::Dpu)).unwrap_err(),
        "dpu_unavailable"
    );
    let r = attach(&netd_down, spec(0, PortBackend::Auto)).unwrap();
    assert!(matches!(r.binding, VmPortBinding::Tap { .. }));
    assert_eq!(r.backend, glidex_dpu::Backend::Host);
    drop(netd_down);

    // One VF, held: `dpu` is full, `auto` uses the host.
    let dir = TempDir::new().unwrap();
    let netd = netd(host_exec(1), healthy(), &dir);
    assert_eq!(
        attach(&netd, spec(0, PortBackend::Auto)).unwrap().backend,
        glidex_dpu::Backend::Dpu
    );
    assert_eq!(
        attach(&netd, spec(1, PortBackend::Dpu)).unwrap_err(),
        "dpu_capacity"
    );
    assert_eq!(
        attach(&netd, spec(1, PortBackend::Auto)).unwrap().backend,
        glidex_dpu::Backend::Host
    );
}

#[test]
fn a_dpu_port_must_be_on_a_cluster_network() {
    let dir = TempDir::new().unwrap();
    let netd = netd(host_exec(2), healthy(), &dir);
    let s = VmPortSpec {
        bridge: "gxbr-nat".into(),
        ovn_lport: None,
        ..spec(0, PortBackend::Dpu)
    };
    assert_eq!(attach(&netd, s).unwrap_err(), "invalid_argument");
}

#[test]
fn the_recorded_backend_wins_on_reattach() {
    let dir = TempDir::new().unwrap();
    {
        // The DPU is down at the first attach: the host.
        let netd = netd(host_exec(2), Arc::new(Down), &dir);
        attach(&netd, spec(0, PortBackend::Auto)).unwrap();
    }
    // netd restarts, the DPU is back: the port stays on the host.
    let netd2 = netd(host_exec(2), healthy(), &dir);
    let r = attach(&netd2, spec(0, PortBackend::Auto)).unwrap();
    assert_eq!(r.backend, glidex_dpu::Backend::Host);
    // And a DPU port stays a DPU port, with its VF.
    let r = attach(&netd2, spec(1, PortBackend::Auto)).unwrap();
    assert_eq!(r.backend, glidex_dpu::Backend::Dpu);
    let again = attach(&netd2, spec(1, PortBackend::Auto)).unwrap();
    assert_eq!(again.binding, r.binding);
}

#[test]
fn the_link_address_is_applied_kept_and_reapplied() {
    let dir = TempDir::new().unwrap();
    let host = host_exec(2);
    let netd = netd(host.clone(), healthy(), &dir);
    let show = "ip -o addr show dev tmfifo_net0";
    let with = "5: tmfifo_net0    inet 192.0.2.1/30 scope global tmfifo_net0\\       valid_lft forever preferred_lft forever\n";
    // No interface yet (rshim down), then there with its address, then
    // recreated by an rshim restart without it.
    host.on(
        show,
        Output::failed(1, "Device \"tmfifo_net0\" does not exist."),
    );
    host.on(show, Output::ok(with));
    host.on(
        show,
        Output::ok("5: tmfifo_net0    inet6 fe80::1/64 scope link\n"),
    );
    assert_eq!(netd.ensure_dpu_link().unwrap().unwrap(), LinkState::Absent);
    assert_eq!(netd.ensure_dpu_link().unwrap().unwrap(), LinkState::Present);
    let adds = |h: &RecordingExec| {
        h.calls()
            .iter()
            .filter(|c| c.starts_with("ip addr add"))
            .count()
    };
    assert_eq!(adds(&host), 0);
    assert_eq!(netd.ensure_dpu_link().unwrap().unwrap(), LinkState::Applied);
    assert!(
        host.calls()
            .contains(&"ip addr add 192.0.2.1/30 dev tmfifo_net0".to_string())
    );

    // The probe says when the address is missing.
    let p: dpu::DpuProbe =
        serde_json::from_value(netd.handle(Op::DpuProbe { id: None }, &root()).unwrap()).unwrap();
    assert!(p.reachable);
    assert_eq!(p.missing, ["link_address"]);
    assert_eq!((p.vfs_total, p.vfs_free), (2, 2));
    host.set(show, Output::ok(with));
    let p: dpu::DpuProbe =
        serde_json::from_value(netd.handle(Op::DpuProbe { id: None }, &root()).unwrap()).unwrap();
    assert!(p.missing.is_empty(), "{:?}", p.missing);
}

#[test]
fn an_unreachable_agent_is_reported_by_the_probe() {
    let dir = TempDir::new().unwrap();
    let netd = netd(host_exec(2), Arc::new(Down), &dir);
    let p: dpu::DpuProbe =
        serde_json::from_value(netd.handle(Op::DpuProbe { id: None }, &root()).unwrap()).unwrap();
    assert!(!p.reachable);
    assert!(p.missing.contains(&"agent".to_string()));
    assert_eq!(
        netd.handle(
            Op::DpuProbe {
                id: Some("dpu9".into())
            },
            &root()
        )
        .unwrap_err()
        .code(),
        "not_found"
    );
}

#[test]
fn ports_on_the_dpu_are_listed_with_whether_netd_has_them() {
    let dir = TempDir::new().unwrap();
    let netd = netd(host_exec(2), healthy(), &dir);
    let ports: Vec<DpuPortInfo> =
        serde_json::from_value(netd.handle(Op::DpuListPorts, &root()).unwrap()).unwrap();
    assert_eq!(ports.len(), 1);
    assert!(!ports[0].recorded);
    attach(&netd, spec(0, PortBackend::Dpu)).unwrap();
    let ports: Vec<DpuPortInfo> =
        serde_json::from_value(netd.handle(Op::DpuListPorts, &root()).unwrap()).unwrap();
    assert!(ports[0].recorded);
}

#[test]
fn an_invalid_dpu_json_stops_netd() {
    let dir = TempDir::new().unwrap();
    let netd_json = dir.path().join("netd.json");
    std::fs::write(
        &netd_json,
        format!(
            r#"{{"run_dir":"{}","state_path":"{}"}}"#,
            dir.path().join("run").display(),
            dir.path().join("netd.db").display()
        ),
    )
    .unwrap();
    let dpu_json = dir.path().join("dpu.json");
    std::fs::write(&dpu_json, "{\"dpus\": [").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_glidex-netd"))
        .arg("--config")
        .arg(&netd_json)
        .arg("--dpu-config")
        .arg(&dpu_json)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("dpu.json"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn start_restores_missing_dpu_ports_and_reports_untracked_ones() {
    let dir = TempDir::new().unwrap();
    {
        let netd = netd(host_exec(2), healthy(), &dir);
        attach(&netd, spec(0, PortBackend::Dpu)).unwrap();
    }
    // The DPU was reset: NIC 0's representor is gone, and a port of a
    // VM netd doesn't know is there.
    const OTHER: &str = "9a2b3c4d-5e6f-4a1b-9c2d-0123456789ab";
    let dpu = dpu_exec_with(&[tagged("en3f0pf0sf3001", OTHER, 0)]);
    let netd2 = netd(host_exec(2), Arc::new(Agent::new(dpu.clone())), &dir);
    let report = netd2.reconcile_startup();
    assert!(
        report
            .repaired
            .contains(&"PortRestored en3f0pf0sf3000".to_string()),
        "{report:?}"
    );
    assert_eq!(report.orphans, ["DPU port en3f0pf0sf3001"]);
    let calls = dpu.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("virtnet modify -p 0 -v 0")),
        "{calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| c.contains("add-port br-int en3f0pf0sf3000"))
    );
    assert!(
        !calls.iter().any(|c| c.contains("del-port")),
        "an untracked port is never deleted"
    );
    drop(netd2);

    // Nothing missing: nothing done.
    let dpu = dpu_exec();
    let netd3 = netd(host_exec(2), Arc::new(Agent::new(dpu.clone())), &dir);
    let report = netd3.reconcile_startup();
    assert!(
        !report
            .repaired
            .iter()
            .any(|r| r.starts_with("PortRestored"))
    );
    assert!(report.orphans.is_empty(), "{report:?}");
    assert!(!dpu.calls().iter().any(|c| c.starts_with("virtnet modify")));
    drop(netd3);

    // The agent down: an error, no change.
    let netd4 = netd(host_exec(2), Arc::new(Down), &dir);
    let report = netd4.reconcile_startup();
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.starts_with("DPU ports not reconciled")),
        "{report:?}"
    );
}

#[test]
fn the_packaged_example_dpu_json_parses() {
    let text = std::fs::read(format!(
        "{}/../../packaging/dpu.json.example",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let d = dpu::parse(&text).unwrap();
    assert_eq!(d.id, "dpu0");
    assert_eq!(d.link.unwrap().iface, "tmfifo_net0");
}

/// The chassis ops through netd to the real agent code (spec/31-dpu.md
/// §8.3): netd relays a CSR out and a signed certificate in, and no
/// private key crosses either way.
#[test]
fn the_dpu_chassis_is_enrolled_through_the_agent_without_its_key() {
    use glidex_netd::dpu_chassis::DpuChassisCerts;
    use rcgen::{
        BasicConstraints, CertificateParams, CertificateSigningRequestParams, DnType, IsCa, Issuer,
        KeyPair,
    };

    let dir = TempDir::new().unwrap();
    let exec = dpu_exec();
    exec.on(
        "ovs-vsctl --format=json --columns=external_ids list Open_vSwitch",
        glidex_dpu::Output::ok(r#"{"data":[[["map",[]]]],"headings":["external_ids"]}"#),
    );
    exec.on(
        "ovs-vsctl --format=json --columns=name,datapath_type find Bridge",
        glidex_dpu::Output::ok(
            r#"{"data":[["br-int","doca"]],"headings":["name","datapath_type"]}"#,
        ),
    );
    let mut agent = Agent::new(exec.clone());
    agent.chassis.dir = dir.path().join("dpu-ovn");
    let backend = Arc::new(DpuBackend::new(
        dpu::parse(
            br#"{"dpus":[{"id":"dpu0","agent":"tcp://192.0.2.2:7443",
            "tls":{"ca":"/nonexistent/ca.pem","cert":"/nonexistent/c.pem","key":"/nonexistent/c.key"},
            "ovn":{"encap_ip":"192.0.2.31"}}]}"#,
        )
        .unwrap(),
        Arc::new(agent),
    ));
    let config = Config {
        run_dir: dir.path().join("run"),
        state_path: dir.path().join("netd.db"),
        ..Config::default()
    };
    let netd = Netd::new(host_exec(1), Arc::new(FakeSupervisor::default()), config)
        .unwrap()
        .with_dpu(backend.clone())
        .with_dpu_chassis(backend);

    let wrong = netd
        .handle(
            Op::DpuChassisCsr {
                id: "dpu9".into(),
                chassis: "node:node-a-dpu".into(),
            },
            &root(),
        )
        .unwrap_err();
    assert_eq!(wrong.code(), "not_found");

    let answer = netd
        .handle(
            Op::DpuChassisCsr {
                id: "dpu0".into(),
                chassis: "node:node-a-dpu".into(),
            },
            &root(),
        )
        .unwrap();
    let text = answer.to_string();
    assert!(!text.contains("PRIVATE KEY"), "{text}");
    let csr_pem = answer["csr"].as_str().unwrap();

    // The leader's part: sign the request's public key with the cluster CA.
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name.push(DnType::CommonName, "cluster CA");
    let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
    let mut req = CertificateSigningRequestParams::from_pem(csr_pem).unwrap();
    req.params.distinguished_name = rcgen::DistinguishedName::new();
    req.params
        .distinguished_name
        .push(DnType::CommonName, "node:node-a-dpu");
    let cert_pem = req
        .params
        .signed_by(&req.public_key, &Issuer::from_params(&ca, &ca_key))
        .unwrap()
        .pem();

    let op = Op::EnsureDpuChassis(EnsureDpuChassisArgs {
        id: "dpu0".into(),
        chassis: "node:node-a-dpu".into(),
        sb_remotes: vec!["ssl:192.0.2.11:6642".into()],
        certs: DpuChassisCerts { cert_pem, ca_pem },
    });
    assert!(!serde_json::to_string(&op).unwrap().contains("PRIVATE KEY"));
    netd.handle(op, &root()).unwrap();
    let calls = exec.calls();
    assert!(
        calls.iter().any(|c| c.starts_with("ovs-vsctl set-ssl")),
        "{calls:?}"
    );
    let set = calls
        .iter()
        .find(|c| c.contains("external_ids:system-id=node:node-a-dpu"))
        .unwrap();
    assert!(
        set.contains("external_ids:ovn-encap-ip=192.0.2.31"),
        "{set}"
    );
    assert!(set.contains("external_ids:ovn-remote=ssl:192.0.2.11:6642"));
    // The key never left the DPU's directory.
    assert!(dir.path().join("dpu-ovn/chassis.key").exists());

    netd.handle(
        Op::LeaveDpuOvn {
            id: "dpu0".into(),
            confirm: true,
        },
        &root(),
    )
    .unwrap();
    assert!(exec.calls().contains(&"ovs-vsctl del-ssl".to_string()));
    assert!(!dir.path().join("dpu-ovn/chassis.key").exists());
}
