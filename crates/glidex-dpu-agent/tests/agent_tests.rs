//! The agent's ops on a `RecordingExec`, and its mutual TLS.

use glidex_dpu::{DpuPortSpec, Output, RecordingExec, VfId};
use glidex_dpu_agent::proto::{AttachRepResult, HelloResult};
use glidex_dpu_agent::{Agent, AgentApi, AgentClient, AgentError, AgentOp, server, tls};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use std::sync::Arc;
use std::time::Duration;

const VM: &str = "1a2b3c4d-5e6f-4a1b-9c2d-0123456789ab";

fn fixture(set: &str, name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/../glidex-dpu/tests/fixtures/{set}/dpu-{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn spec() -> DpuPortSpec {
    DpuPortSpec {
        vm_id: VM.into(),
        nic: 0,
        lsp: "gx-1a2b3c4d-0".into(),
        mac: "02:11:22:33:44:55".into(),
        mtu: 1442,
        queue_pairs: 4,
    }
}

fn attach_op(timeout_secs: u64) -> AgentOp {
    AgentOp::AttachRep {
        vf: VfId { pf: 0, vf: 0 },
        spec: spec(),
        bridge: "br-int".into(),
        timeout_secs,
    }
}

#[test]
fn attach_configures_the_vf_then_puts_its_representor_on_br_int() {
    let exec = Arc::new(RecordingExec::new());
    exec.on(
        "virtnet list",
        Output::ok(fixture("one-vf", "virtnet_list")),
    );
    let agent = Agent::new(exec.clone());
    let r: AttachRepResult = serde_json::from_value(agent.call(attach_op(5)).unwrap()).unwrap();
    assert_eq!(r.representor, "en3f0pf0sf3000");
    assert_eq!(r.bdf, "61:02.3");
    let calls = exec.calls();
    let modify = calls
        .iter()
        .position(|c| c.starts_with("virtnet modify"))
        .unwrap();
    let add = calls
        .iter()
        .position(|c| c.contains("add-port br-int en3f0pf0sf3000"))
        .unwrap();
    assert!(modify < add, "{calls:?}");
    assert_eq!(
        calls[modify],
        "virtnet modify -p 0 -v 0 device -m 02:11:22:33:44:55 -t 1442 -qp 4"
    );
    assert!(calls.contains(&"ip link set en3f0pf0sf3000 up".to_string()));
}

#[test]
fn attach_refuses_more_queue_pairs_than_the_vf_has_and_times_out_without_a_representor() {
    let exec = Arc::new(RecordingExec::new());
    exec.on(
        "virtnet list",
        Output::ok(fixture("one-vf", "virtnet_list")),
    );
    let agent = Agent::new(exec.clone());
    // VF 0 allows 5 queue pairs (fixture).
    let op = AgentOp::AttachRep {
        vf: VfId { pf: 0, vf: 0 },
        spec: DpuPortSpec {
            queue_pairs: 6,
            ..spec()
        },
        bridge: "br-int".into(),
        timeout_secs: 0,
    };
    match agent.call(op) {
        Err(AgentError::Remote(e)) => assert_eq!(e.code, "invalid_argument"),
        other => panic!("{other:?}"),
    }

    let exec = Arc::new(RecordingExec::new());
    exec.on(
        "virtnet list",
        Output::ok(fixture("pf-only", "virtnet_list")),
    );
    let mut agent = Agent::new(exec.clone());
    agent.poll = Duration::from_millis(1);
    match agent.call(attach_op(0)) {
        Err(AgentError::Remote(e)) => assert_eq!(e.code, "dpu_port_timeout"),
        other => panic!("{other:?}"),
    }
    assert!(!exec.calls().iter().any(|c| c.contains("add-port")));
}

#[test]
fn probe_answers_from_the_dpu() {
    let exec = Arc::new(RecordingExec::new());
    exec.on(
        "virtnet list",
        Output::ok(fixture("pf-only", "virtnet_list")),
    );
    let agent = Agent::new(exec);
    let caps: glidex_dpu::DpuCapabilities =
        serde_json::from_value(agent.call(AgentOp::Probe).unwrap()).unwrap();
    assert_eq!(caps.virtio_pfs, ["61:00.1"]);
}

// ---- mutual TLS ----------------------------------------------------------

struct Ca {
    pem: String,
    params: CertificateParams,
    key: KeyPair,
}

fn ca(name: &str) -> Ca {
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    let pem = params.self_signed(&key).unwrap().pem();
    Ca { pem, params, key }
}

fn leaf(ca: &Ca, cn: &str, sans: &[&str]) -> (String, String) {
    let key = KeyPair::generate().unwrap();
    let mut params =
        CertificateParams::new(sans.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
    params.distinguished_name.push(DnType::CommonName, cn);
    let issuer = Issuer::from_params(&ca.params, &ca.key);
    (
        params.signed_by(&key, &issuer).unwrap().pem(),
        key.serialize_pem(),
    )
}

/// An agent on 127.0.0.1 that accepts only `glidex-netd`.
fn start_agent(ca: &Ca) -> String {
    let (cert, key) = leaf(ca, "dpu0-agent", &["127.0.0.1"]);
    let config = tls::server_config(&tls::TlsPem {
        ca: ca.pem.clone(),
        cert,
        key,
    })
    .unwrap();
    let exec = Arc::new(RecordingExec::new());
    let agent = Arc::new(Agent::new(exec));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || server::serve(agent, listener, config, "glidex-netd".into()));
    format!("tcp://{addr}")
}

fn client(addr: &str, ca: &Ca, cert_ca: &Ca, cn: &str) -> AgentClient {
    let (cert, key) = leaf(cert_ca, cn, &[]);
    let config = tls::client_config(&tls::TlsPem {
        ca: ca.pem.clone(),
        cert,
        key,
    })
    .unwrap();
    AgentClient::new(addr, config)
        .unwrap()
        .timeout(Duration::from_secs(5))
}

#[test]
fn only_netds_certificate_from_the_channel_ca_gets_in() {
    let ca1 = ca("dpu channel");
    let addr = start_agent(&ca1);

    let ok = client(&addr, &ca1, &ca1, "glidex-netd");
    let hello: HelloResult =
        serde_json::from_value(ok.call(AgentOp::Hello { protocol: 1 }).unwrap()).unwrap();
    assert_eq!(hello.protocol, 1);
    assert_eq!(
        ok.call(AgentOp::ListPorts).unwrap_err(),
        AgentError::Remote(glidex_dpu_agent::proto::ErrorBody {
            code: "internal".into(),
            message: "could not parse ovs-vsctl json: EOF while parsing a value at line 1 column 0"
                .into()
        })
    );

    // A certificate from another CA: the handshake fails.
    let other = ca("someone else");
    let bad = client(&addr, &ca1, &other, "glidex-netd");
    assert!(matches!(
        bad.call(AgentOp::Probe),
        Err(AgentError::Unavailable(_))
    ));

    // The channel's CA, but not netd: the agent closes the connection.
    let wrong = client(&addr, &ca1, &ca1, "someone");
    assert!(matches!(
        wrong.call(AgentOp::Probe),
        Err(AgentError::Unavailable(_))
    ));

    // No certificate at all.
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates({
        let mut r = rustls::RootCertStore::empty();
        use rustls::pki_types::pem::PemObject;
        for c in rustls::pki_types::CertificateDer::pem_slice_iter(ca1.pem.as_bytes()) {
            r.add(c.unwrap()).unwrap();
        }
        r
    })
    .with_no_client_auth();
    let anon = AgentClient::new(&addr, Arc::new(config))
        .unwrap()
        .timeout(Duration::from_secs(5));
    assert!(matches!(
        anon.call(AgentOp::Probe),
        Err(AgentError::Unavailable(_))
    ));
}
