//! The agent's chassis ops (spec/31-dpu.md §8.3): the key stays on the
//! DPU, the certificate must be for it, and the exact commands.

use glidex_dpu::{Output, RecordingExec};
use glidex_dpu_agent::chassis::{ChassisArgs, ChassisStatus};
use glidex_dpu_agent::{Agent, AgentApi, AgentError, AgentOp};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tempfile::TempDir;

fn agent(dir: &TempDir) -> (Agent, Arc<RecordingExec>) {
    let exec = Arc::new(RecordingExec::new());
    let mut a = Agent::new(exec.clone());
    a.chassis.dir = dir.path().join("ovn");
    (a, exec)
}

fn csr(a: &Agent) -> String {
    let v = a
        .call(AgentOp::Csr {
            chassis: "node:node-a-dpu".into(),
        })
        .unwrap();
    v["csr"].as_str().unwrap().to_string()
}

fn mode(p: &std::path::Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// The cluster CA and a certificate it signs for `key`'s public half.
fn signed_for(key: &KeyPair) -> (String, String) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name.push(DnType::CommonName, "cluster CA");
    let ca_pem = ca.self_signed(&ca_key).unwrap().pem();
    let mut leaf = CertificateParams::new(Vec::<String>::new()).unwrap();
    leaf.distinguished_name
        .push(DnType::CommonName, "node:node-a-dpu");
    let cert = leaf
        .signed_by(key, &Issuer::from_params(&ca, &ca_key))
        .unwrap()
        .pem();
    (cert, ca_pem)
}

fn code(e: AgentError) -> String {
    match e {
        AgentError::Remote(b) => b.code,
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_csr_comes_from_a_key_that_stays_on_the_dpu() {
    let dir = TempDir::new().unwrap();
    let (a, _) = agent(&dir);
    let first = csr(&a);
    assert!(first.contains("BEGIN CERTIFICATE REQUEST"), "{first}");
    assert!(!first.contains("PRIVATE KEY"));
    let key = a.chassis.key_path();
    assert_eq!(mode(&key), 0o600);
    assert_eq!(mode(&a.chassis.dir), 0o700);
    // The same key next time: a retried enrolment doesn't orphan a cert.
    let pem = std::fs::read_to_string(&key).unwrap();
    csr(&a);
    assert_eq!(std::fs::read_to_string(&key).unwrap(), pem);

    for bad in ["node:node-a", "node:x;rm-dpu", ""] {
        let e = a
            .call(AgentOp::Csr {
                chassis: bad.into(),
            })
            .unwrap_err();
        assert_eq!(code(e), "invalid_argument", "{bad}");
    }
}

#[test]
fn only_a_certificate_for_the_dpus_key_is_installed() {
    let dir = TempDir::new().unwrap();
    let (a, exec) = agent(&dir);
    let install = |cert: &str, ca: &str| {
        a.call(AgentOp::InstallCert {
            cert_pem: cert.into(),
            ca_pem: ca.into(),
        })
    };

    // Before any CSR there is no key to match.
    let other = KeyPair::generate().unwrap();
    let (cert, ca) = signed_for(&other);
    assert_eq!(code(install(&cert, &ca).unwrap_err()), "invalid_argument");

    csr(&a);
    // A certificate for another key.
    assert_eq!(code(install(&cert, &ca).unwrap_err()), "invalid_argument");
    // Anything carrying a private key.
    let with_key = format!("{cert}{}", other.serialize_pem());
    assert_eq!(
        code(install(&with_key, &ca).unwrap_err()),
        "invalid_argument"
    );
    assert!(!a.chassis.cert_path().exists());
    assert!(!exec.calls().iter().any(|c| c.contains("set-ssl")));

    let key = KeyPair::from_pem(&std::fs::read_to_string(a.chassis.key_path()).unwrap()).unwrap();
    let (cert, ca) = signed_for(&key);
    assert_eq!(install(&cert, &ca).unwrap()["changed"], true);
    assert_eq!(
        std::fs::read_to_string(a.chassis.cert_path()).unwrap(),
        cert
    );
    let d = a.chassis.dir.display();
    assert!(
        exec.calls().contains(&format!(
            "ovs-vsctl set-ssl {d}/chassis.key {d}/chassis.crt {d}/ca.crt"
        )),
        "{:?}",
        exec.calls()
    );
    // Not running: nothing to restart. Again: nothing changed.
    assert!(!exec.calls().iter().any(|c| c.contains("restart")));
    exec.on("systemctl is-active ovn-controller", Output::ok("active\n"));
    assert_eq!(install(&cert, &ca).unwrap()["changed"], false);
    assert!(!exec.calls().iter().any(|c| c.contains("restart")));
}

fn args(encap: Option<&str>) -> ChassisArgs {
    ChassisArgs {
        system_id: "node:node-a-dpu".into(),
        sb_remotes: vec!["ssl:192.0.2.11:6642".into(), "ssl:192.0.2.12:6642".into()],
        encap_ip: encap.map(|e| e.parse().unwrap()),
        br_int: "br-int".into(),
    }
}

#[test]
fn chassis_sets_the_dpu_up_and_leave_undoes_it() {
    let dir = TempDir::new().unwrap();
    let (mut a, exec) = agent(&dir);
    assert_eq!(
        code(a.call(AgentOp::Chassis(args(None))).unwrap_err()),
        "unsupported"
    );
    exec.clear_calls();

    exec.on(
        "ovs-vsctl --format=json --columns=external_ids list Open_vSwitch",
        Output::ok(r#"{"data":[[["map",[["glidex-chassis","1"],["ovn-remote","ssl:192.0.2.11:6642,ssl:192.0.2.12:6642"],["system-id","node:node-a-dpu"]]]]],"headings":["external_ids"]}"#),
    );
    exec.on(
        "ovs-vsctl --format=json --columns=name,datapath_type find Bridge name=br-int",
        Output::ok(r#"{"data":[["br-int","doca"]],"headings":["name","datapath_type"]}"#),
    );
    exec.on("systemctl is-active ovn-controller", Output::ok("active\n"));
    let st: ChassisStatus =
        serde_json::from_value(a.call(AgentOp::Chassis(args(Some("192.0.2.31")))).unwrap())
            .unwrap();
    assert_eq!(
        st,
        ChassisStatus {
            chassis: Some("node:node-a-dpu".into()),
            controller_running: true,
            sb_remotes: vec!["ssl:192.0.2.11:6642".into(), "ssl:192.0.2.12:6642".into()],
            br_int: true,
            datapath: Some("doca".into()),
        }
    );
    let calls: Vec<String> = exec
        .calls()
        .into_iter()
        .filter(|c| !c.contains("--format=json") && !c.contains("is-active"))
        .collect();
    assert_eq!(
        calls,
        [
            "ovs-vsctl --may-exist add-br br-int -- set Bridge br-int datapath_type=doca \
             external_ids:glidex-owner=glidex external_ids:glidex-role=ovn-int fail_mode=secure \
             other-config:disable-in-band=true",
            "ovs-vsctl set Open_vSwitch . external_ids:system-id=node:node-a-dpu \
             external_ids:ovn-remote=ssl:192.0.2.11:6642,ssl:192.0.2.12:6642 \
             external_ids:ovn-encap-type=geneve external_ids:ovn-encap-ip=192.0.2.31 \
             external_ids:ovn-bridge=br-int external_ids:glidex-chassis=1",
            "ovs-vsctl --if-exists remove Open_vSwitch . external_ids ovn-bridge-mappings",
            "systemctl is-enabled ovn-host",
            "systemctl enable ovn-host",
            "systemctl start ovn-host",
            "systemctl start ovn-controller",
        ]
    );

    // The agent's own address when netd's dpu.json has none.
    a.chassis.encap_ip = Some("192.0.2.32".parse().unwrap());
    exec.clear_calls();
    a.call(AgentOp::Chassis(args(None))).unwrap();
    assert!(
        exec.calls()
            .iter()
            .any(|c| c.contains("ovn-encap-ip=192.0.2.32"))
    );

    csr(&a);
    exec.clear_calls();
    a.call(AgentOp::LeaveOvn).unwrap();
    assert_eq!(
        exec.calls(),
        [
            "systemctl disable --now ovn-host",
            "systemctl stop ovn-controller",
            "ovs-vsctl --if-exists remove Open_vSwitch . external_ids system-id ovn-remote \
             ovn-encap-type ovn-encap-ip ovn-bridge glidex-chassis",
            "ovs-vsctl del-ssl",
        ]
    );
    assert!(!a.chassis.key_path().exists());
}
