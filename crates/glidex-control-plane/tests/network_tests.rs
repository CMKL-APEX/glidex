//! Control-plane networking against an in-process glidex-netd (real Unix
//! socket, recording executor underneath): no root, no OVS.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use glidex_control_plane::api::create_router;
use glidex_control_plane::network::Netd;
use glidex_control_plane::state::VmManager;
use glidex_netd::server::{self, Access, Config, Netd as NetdServer};
use glidex_netd::supervisor::FakeSupervisor;
use glidex_ovs::{Output, RecordingExec};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const OWNED_BRIDGE: &str = r#"{"data":[["gxbr-lab","system",["map",[["glidex-owner","glidex"],["glidex-role","bridge"]]],["set",[]]]],"headings":["name","datapath_type","external_ids","ports"]}"#;
const OWNED_IF: &str = r#"{"data":[["gx00000000-0",["map",[["glidex-owner","glidex"],["glidex-role","vm"]]],""]],"headings":["name","external_ids","type"]}"#;

struct Harness {
    app: Router,
    manager: std::sync::Arc<VmManager>,
    exec: Arc<RecordingExec>,
    _dir: TempDir,
}

/// The VM in a `{vm}` answer (`POST /vms`, writes), in the proto3 JSON
/// shape (spec/grpc.md §9.1).
fn vm_of(v: &Value) -> &Value {
    assert!(v["vm"].is_object(), "not a VM answer: {v}");
    &v["vm"]
}

/// The id of the VM in a `{vm}` answer.
fn vm_id(v: &Value) -> String {
    vm_of(v)["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no VM id: {v}"))
        .to_string()
}

/// A proto VM's `state`, short and lower-case (`VM_STATE_FAILED` is
/// `failed`).
fn state(vm: &Value) -> String {
    let s = vm["state"]
        .as_str()
        .unwrap_or_else(|| panic!("no state: {vm}"));
    s.strip_prefix("VM_STATE_")
        .unwrap_or_else(|| panic!("not a VM state: {s}"))
        .to_ascii_lowercase()
}

fn fake_exec(ovs_running: bool) -> Arc<RecordingExec> {
    let exec = Arc::new(RecordingExec::new());
    if ovs_running {
        exec.on(
            "ovs-vsctl --version",
            Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"),
        );
    } else {
        exec.on("ovs-vsctl --version", Output::failed(127, "not found"));
    }
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

/// Control plane + fake netd listening in a temp run dir.
fn harness(ovs_running: bool) -> Harness {
    let dir = TempDir::new().unwrap();
    let run_dir = dir.path().join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let exec = fake_exec(ovs_running);
    let my_group = nix::unistd::Group::from_gid(nix::unistd::getgid())
        .unwrap()
        .unwrap()
        .name;
    let netd = Arc::new(
        NetdServer::new(
            exec.clone(),
            Arc::new(FakeSupervisor::default()),
            // netd's policy for the test user's own group (the socket is
            // bound to it below), so the result doesn't depend on whether
            // the user running the tests is in glidex or glidex-admin.
            Config {
                run_dir: run_dir.clone(),
                state_path: dir.path().join("netd.db"),
                group: my_group.clone(),
                admin_group: "no-such-group-glidex-test".into(),
                policy: glidex_netd::auth::default_policy(&my_group, "no-such-group-glidex-test"),
                ..Config::default()
            },
        )
        .unwrap(),
    );
    let gid = Some(nix::unistd::getgid().as_raw());
    let full = server::bind(&run_dir.join("netd.sock"), 0o660, gid).unwrap();
    let status = server::bind(&run_dir.join("netd-ro.sock"), 0o666, None).unwrap();
    let n2 = netd.clone();
    std::thread::spawn(move || server::serve(n2, status, Access::Status, None));
    std::thread::spawn(move || server::serve(netd, full, Access::Full, gid));

    let manager =
        VmManager::with_db_path_and_netd(dir.path().join("cp.db"), Netd::new(&run_dir)).unwrap();
    Harness {
        app: create_router(manager.clone()),
        manager,
        exec,
        _dir: dir,
    }
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn vm(name: &str, networks: Value) -> Value {
    json!({
        "name": name, "vcpu_count": 1, "mem_size_mib": 512,
        "hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR",
        "firmware_path": "/nonexistent/CLOUDHV.fd",
        "rootfs_path": "/nonexistent/disk.raw",
        "networks": networks
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn nat_network_crud() {
    let h = harness(true);
    let (status, body) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let net = &body["network"];
    let id = net["id"].as_str().unwrap().to_string();
    // D24: the bridge is named after the id, as names repeat across tenants.
    let bridge = format!("gxbr-{}", &id[..8]);
    assert_eq!(net["bridge"], bridge.as_str(), "{body}");
    assert_eq!(net["port_type"], "PORT_TYPE_TAP");
    assert_eq!(net["mode"], "NETWORK_MODE_NAT");
    let calls = h.exec.calls();
    assert!(
        calls
            .iter()
            .any(|c| *c == format!("ip addr replace 10.88.0.1/24 dev {bridge}")),
        "{calls:#?}"
    );

    let (status, list) = request(&h.app, "GET", "/networks", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["networks"].as_array().unwrap().len(), 1);
    // By id or by name.
    let (status, by_id) = request(&h.app, "GET", &format!("/networks/{id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{by_id}");
    assert_eq!(by_id["network"]["name"], "lab");

    let (status, _) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = request(&h.app, "DELETE", "/networks/lab", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&h.app, "GET", "/networks/lab", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn vms_reference_networks() {
    let h = harness(true);
    request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;

    let (status, body) = request(
        &h.app,
        "POST",
        "/vms",
        Some(vm("v1", json!([{"network": "lab"}]))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let mac = vm_of(&body)["nics"][0]["mac"].as_str().unwrap().to_string();
    assert!(
        mac.starts_with("02:"),
        "stable MAC assigned at create: {mac}"
    );
    let id = vm_id(&body);

    let (status, body) = request(
        &h.app,
        "POST",
        "/vms",
        Some(vm("v2", json!([{"network": "nope"}]))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let mut qemu = vm("v3", json!([{"network": "lab"}]));
    qemu["hypervisor"] = json!("HYPERVISOR_QEMU");
    qemu["kernel_image_path"] = json!("/k");
    qemu.as_object_mut().unwrap().remove("firmware_path");
    let (status, body) = request(&h.app, "POST", "/vms", Some(qemu)).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "QEMU VMs take networks too: {body}"
    );
    assert!(
        vm_of(&body)["nics"][0]["mac"]
            .as_str()
            .unwrap()
            .starts_with("02:"),
        "{body}"
    );
    let qemu_id = vm_id(&body);

    let (status, body) = request(
        &h.app,
        "POST",
        "/vms",
        Some(vm(
            "v4",
            json!([{"network": "lab", "mac": "01:00:5e:00:00:01"}]),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "multicast MAC: {body}");

    let (status, body) = request(&h.app, "DELETE", "/networks/lab", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["message"].as_str().unwrap().contains("v1"));

    for vm_id in [&id, &qemu_id] {
        let (status, _) = request(&h.app, "DELETE", &format!("/vms/{vm_id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let (status, _) = request(&h.app, "DELETE", "/networks/lab", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_start_detaches_ports() {
    failed_start_detaches_ports_for("cloudhypervisor", "cloud-hypervisor").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn qemu_failed_start_detaches_ports() {
    failed_start_detaches_ports_for("qemu", "qemu-system-x86_64").await;
}

/// A hypervisor's short name (`qemu`, `cloudhypervisor`) as its proto name.
fn hypervisor_pb(h: &str) -> String {
    match h {
        "cloudhypervisor" => "HYPERVISOR_CLOUD_HYPERVISOR".into(),
        h => format!("HYPERVISOR_{}", h.to_ascii_uppercase()),
    }
}

async fn failed_start_detaches_ports_for(hypervisor: &str, binary: &str) {
    if std::process::Command::new(binary)
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: {binary} not installed");
        return;
    }
    let h = harness(true);
    h.manager.initialize().await.unwrap();
    h.manager.start_controllers();
    request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    let mut spec = vm("boom", json!([{"network": "lab"}]));
    spec["hypervisor"] = json!(hypervisor_pb(hypervisor));
    let (_, body) = request(&h.app, "POST", "/vms", Some(spec)).await;
    let id = vm_id(&body);
    let port = glidex_ovs::names::port_name(&id, 0).unwrap();

    // The firmware path doesn't exist, so the launch fails after the port
    // was attached (spec/reconciliation.md §9.1 step 5).
    let (status, body) = request(&h.app, "POST", &format!("/vms/{id}/start?wait=60"), None).await;
    assert!(
        status.is_server_error() || status.is_client_error(),
        "{status} {body}"
    );
    assert_eq!(state(&body["details"]["vm"]), "failed", "{body}");
    let calls = h.exec.calls();
    let attached = calls
        .iter()
        .position(|c| c.starts_with(&format!("ip tuntap add dev {port}")))
        .expect("attached");

    // Ports belong to the VM while it should run (D16); stopping it
    // releases them.
    let (status, body) = request(&h.app, "POST", &format!("/vms/{id}/stop?wait=60"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // `created` when the launch failed before anything ran (QEMU refuses
    // the missing firmware while building its command line).
    assert!(
        matches!(state(vm_of(&body)).as_str(), "stopped" | "created"),
        "{body}"
    );
    let calls = h.exec.calls();
    let detached = calls
        .iter()
        .rposition(|c| c == &format!("ovs-vsctl --if-exists del-port {port}"))
        .expect("detached");
    assert!(detached > attached);
    assert!(vm_of(&body)["nics"][0]["port"].is_null(), "{body}");
    request(&h.app, "DELETE", &format!("/vms/{id}?wait=30"), None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ovs_status_and_bridges() {
    let h = harness(true);
    let (status, body) = request(&h.app, "GET", "/ovs/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["netd"]["available"], true);
    assert_eq!(body["netd"]["access"], "NETD_ACCESS_FULL");
    assert_eq!(body["host"]["ovs_version"], "3.7.1");

    let (status, body) = request(
        &h.app,
        "POST",
        "/ovs/bridges",
        Some(json!({"name": "gxbr-lab", "datapath": "DATAPATH_SYSTEM"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["bridge"]["spec"]["datapath"], "DATAPATH_SYSTEM",
        "{body}"
    );
    let (status, body) = request(&h.app, "GET", "/ovs/bridges", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["bridges"][0]["spec"]["name"], "gxbr-lab");
    // The same over RPC (glidex.host.v1).
    let (status, body) = request(
        &h.app,
        "POST",
        "/glidex.host.v1.HostNetworkService/ListBridges",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bridges"][0]["spec"]["name"], "gxbr-lab");
    let (status, body) = request(
        &h.app,
        "POST",
        "/glidex.host.v1.HostNetworkService/GetOvsStatus",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["host"]["ovs_version"], "3.7.1", "{body}");
    let (status, body) = request(
        &h.app,
        "POST",
        "/glidex.host.v1.PciService/ListPciDevices",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = request(&h.app, "GET", "/ovs/bridges/gxbr-lab/uplinks", None).await;
    assert_eq!((status, &body), (StatusCode::OK, &json!({})), "no uplinks");

    let (status, body) = request(
        &h.app,
        "POST",
        "/ovs/bridges",
        Some(json!({"name": "Bad!", "datapath": "DATAPATH_SYSTEM"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_network");
}

/// spec/reconciliation.md §10.3: the network controller checks netd's
/// records and reports the network's phase.
#[tokio::test(flavor = "multi_thread")]
async fn network_controller_reports_the_phase() {
    let h = harness(true);
    h.manager.start_controllers();
    let (status, _) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let mut net = Value::Null;
    for _ in 0..50 {
        let (_, n) = request(&h.app, "GET", "/networks/lab", None).await;
        if n["network"]["conditions"]
            .as_array()
            .is_some_and(|c| !c.is_empty())
        {
            net = n["network"].clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // The fake OVS reports the bridge with no ports and no running
    // dnsmasq: the controller says what is missing instead of guessing.
    assert!(
        matches!(
            net["phase"].as_str(),
            Some("NETWORK_PHASE_READY" | "NETWORK_PHASE_DEGRADED")
        ),
        "{net}"
    );
    let ready = &net["conditions"][0];
    assert_eq!(ready["kind"], "Ready", "{net}");
    if net["phase"] == "NETWORK_PHASE_DEGRADED" {
        assert!(!ready["message"].as_str().unwrap().is_empty(), "{net}");
        let (status, ev) = request(&h.app, "GET", "/networks/lab/events", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(ev["events"][0]["reason"], "Degraded", "{ev}");
    }
    h.manager.stop_controllers().await;
}

/// An isolated network's bridge is marked isolated in netd, which fences
/// it off from the host (spec/security.md §8.4); bridges recorded before
/// the flag existed are marked by the network controller.
#[tokio::test(flavor = "multi_thread")]
async fn isolated_networks_are_fenced_by_netd() {
    use glidex_netd::proto::{BridgeRecord, Op};
    use glidex_ovs::bridge::{BridgeSpec, Datapath};
    let h = harness(true);
    let netd = Netd::new(h._dir.path().join("run"));
    let isolated = |name: &str| {
        netd.call::<Vec<BridgeRecord>>(Op::ListBridges)
            .unwrap()
            .into_iter()
            .find(|b| b.spec.name == name)
            .map(|b| b.spec.isolated)
    };

    let (status, body) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "iso", "mode": "NETWORK_MODE_ISOLATED", "bridge": "gxbr-iso"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let iso = body["network"]["id"].as_str().unwrap().to_string();
    let (status, body) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(isolated("gxbr-iso"), Some(true));
    assert_eq!(
        isolated("gxbr-lab"),
        Some(false),
        "NAT bridges are fenced by their NAT rules"
    );

    // A record from before the flag: the controller marks it.
    let old = BridgeSpec {
        name: "gxbr-iso".into(),
        datapath: Datapath::System,
        mtu: None,
        adopt: false,
        isolated: false,
    };
    netd.call::<BridgeRecord>(Op::EnsureBridge(old)).unwrap();
    assert_eq!(isolated("gxbr-iso"), Some(false));
    h.manager.reconcile_network(&iso).await.unwrap();
    assert_eq!(isolated("gxbr-iso"), Some(true));
    let (_, ev) = request(&h.app, "GET", "/networks/iso/events", None).await;
    assert_eq!(ev["events"][0]["reason"], "BridgeUpdated", "{ev}");
    // Then it is left alone.
    h.manager.reconcile_network(&iso).await.unwrap();
    let (_, ev2) = request(&h.app, "GET", "/networks/iso/events", None).await;
    assert_eq!(
        ev2["events"].as_array().unwrap().len(),
        ev["events"].as_array().unwrap().len(),
        "{ev2}"
    );
}

/// netd lost its records (state database wiped): the NAT is created again
/// while no VM uses the network, but only reported once one does, since a
/// new subnet would renumber its VMs.
#[tokio::test(flavor = "multi_thread")]
async fn lost_nat_is_recreated_only_without_vms() {
    use glidex_netd::proto::{NatInfo, Op};
    let h = harness(true);
    let netd = Netd::new(h._dir.path().join("run"));
    let nat_exists = || {
        netd.call::<Vec<NatInfo>>(Op::ListNat)
            .unwrap()
            .iter()
            .any(|n| n.state.bridge == "gxbr-lab")
    };
    let lose_nat = || {
        netd.call::<Value>(Op::DeleteNat {
            bridge: "gxbr-lab".into(),
        })
        .unwrap();
    };

    let (_, body) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    let lab = body["network"]["id"].as_str().unwrap().to_string();
    assert!(nat_exists());

    lose_nat();
    assert!(!nat_exists());
    h.manager.reconcile_network(&lab).await.unwrap();
    assert!(nat_exists(), "no VM uses it: the NAT is created again");
    let (_, ev) = request(&h.app, "GET", "/networks/lab/events", None).await;
    assert_eq!(ev["events"][0]["reason"], "NatRestored", "{ev}");

    let (status, body) = request(
        &h.app,
        "POST",
        "/vms",
        Some(vm("v1", json!([{"network": "lab"}]))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    lose_nat();
    h.manager.reconcile_network(&lab).await.unwrap();
    assert!(
        !nat_exists(),
        "a VM uses it: not renumbered behind its back"
    );
    let (_, net) = request(&h.app, "GET", "/networks/lab", None).await;
    let net = &net["network"];
    assert_eq!(net["phase"], "NETWORK_PHASE_DEGRADED", "{net}");
    assert_eq!(
        net["conditions"][0]["message"], "netd has no NAT for it",
        "{net}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn netd_errors_map_to_rest() {
    // OVS not running: creating a network is unsupported on this host.
    let h = harness(false);
    let (status, body) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "unsupported_on_host");
    assert!(body["details"]["missing"].as_array().is_some());

    // No netd at all.
    let dir = TempDir::new().unwrap();
    let manager =
        VmManager::with_db_path_and_netd(dir.path().join("cp.db"), Netd::new(dir.path())).unwrap();
    let app = create_router(manager);
    let (status, body) = request(
        &app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["error"], "netd_unavailable");
    let (status, body) = request(&app, "GET", "/ovs/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["netd"]["available"], false);
    // VMs without networks still work.
    let mut v = vm("plain", json!([]));
    v.as_object_mut().unwrap().remove("networks");
    let (status, _) = request(&app, "POST", "/vms", Some(v)).await;
    assert_eq!(status, StatusCode::CREATED);
}

/// Project networks, quotas and two-sided sharing (spec/security.md §6.2).
#[tokio::test(flavor = "multi_thread")]
async fn project_networks_and_sharing() {
    let h = harness(true);
    let pa = h
        .manager
        .projects()
        .create("pa", String::new(), None)
        .unwrap()
        .id;
    let pb = h
        .manager
        .projects()
        .create("pb", String::new(), None)
        .unwrap()
        .id;

    let (status, net) = request(
        &h.app,
        "POST",
        &format!("/projects/{pa}/networks"),
        Some(json!({"name": "pnet", "mode": "NETWORK_MODE_NAT"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{net}");
    let net = &net["network"];
    let pnet = net["id"].as_str().unwrap().to_string();
    assert_eq!(net["project"], pa);
    assert!(net["bridge"].as_str().unwrap().starts_with("gxp-"), "{net}");
    // Project networks are NAT or isolated, never bridged, and pick their own bridge.
    let (status, _) = request(
        &h.app,
        "POST",
        &format!("/projects/{pa}/networks"),
        Some(json!({"name": "x", "mode": "NETWORK_MODE_BRIDGED"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = request(
        &h.app,
        "POST",
        &format!("/projects/{pa}/networks"),
        Some(json!({"name": "x", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-mine"})),
    )
    .await;
    // Not a field of a project network's create (the proto shape refuses it).
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let pc = h
        .manager
        .projects()
        .create("pc", String::new(), None)
        .unwrap()
        .id;
    let (status, iso) = request(
        &h.app,
        "POST",
        &format!("/projects/{pc}/networks"),
        Some(json!({"name": "iso", "mode": "NETWORK_MODE_ISOLATED"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{iso}");
    let iso = &iso["network"];
    assert_eq!(
        (
            iso["mode"].as_str(),
            iso["port_type"].as_str(),
            iso["project"].as_str()
        ),
        (
            Some("NETWORK_MODE_ISOLATED"),
            Some("PORT_TYPE_TAP"),
            Some(pc.as_str())
        )
    );
    // vhost-user is allowed too, but only where the host has DPDK (this one hasn't).
    let (status, body) = request(
        &h.app,
        "POST",
        &format!("/projects/{pc}/networks"),
        Some(json!({"name": "iso-dpdk", "mode": "NETWORK_MODE_ISOLATED", "port_type": "PORT_TYPE_VHOST_USER"})),
    )
    .await;
    assert_eq!(
        (status, body["error"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("unsupported_on_host")
        ),
        "{body}"
    );
    // An isolated network has no subnet to choose.
    let (status, _) = request(
        &h.app,
        "POST",
        &format!("/projects/{pc}/networks"),
        Some(json!({"name": "iso2", "mode": "NETWORK_MODE_ISOLATED", "subnet": "10.99.0.0/24"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Quota: 2 project networks by default.
    let req = |n: &str| serde_json::from_value(json!({"name": n, "mode": "nat"})).unwrap();
    h.manager
        .create_project_network(
            &pa,
            req("pnet2"),
            glidex_control_plane::tenancy::QuotaMode::Enforce,
        )
        .await
        .unwrap();
    let over = h
        .manager
        .create_project_network(
            &pa,
            req("pnet3"),
            glidex_control_plane::tenancy::QuotaMode::Enforce,
        )
        .await;
    assert!(
        matches!(
            over,
            Err(glidex_control_plane::state::VmManagerError::QuotaExceeded(
                _
            ))
        ),
        "{over:?}"
    );

    // Not usable from another project until shared and accepted.
    let mut spec = vm("b1", json!([{"network": "pnet"}]));
    spec["project"] = json!(pb);
    let (status, body) = request(&h.app, "POST", "/vms", Some(spec.clone())).await;
    assert!(status.is_client_error(), "{status} {body}");
    // Project networks are never granted, only shared.
    let (status, _) = request(
        &h.app,
        "PUT",
        "/networks/pnet/grants",
        Some(json!({"grants": [pb]})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = request(
        &h.app,
        "POST",
        "/networks/pnet/shares",
        Some(json!({"project": pb})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // Offers to unknown projects look the same and record nothing.
    let (status, _) = request(
        &h.app,
        "POST",
        "/networks/pnet/shares",
        Some(json!({"project": "no-such-project"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, shares) = request(
        &h.app,
        "GET",
        &format!("/projects/{pb}/network-shares"),
        None,
    )
    .await;
    let expires = shares["shares"][0]["expires_at"].clone();
    assert!(expires.is_string(), "{shares}");
    assert_eq!(
        shares,
        json!({"shares": [{"network": pnet, "network_name": "pnet", "owner_project": pa, "status": "SHARE_STATUS_OFFERED", "expires_at": expires}]})
    );
    // An offer alone grants nothing.
    let (status, _) = request(&h.app, "POST", "/vms", Some(spec.clone())).await;
    assert!(status.is_client_error());

    let (status, body) = request(
        &h.app,
        "POST",
        &format!("/projects/{pb}/network-shares/pnet/accept"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["network"]["shares"], json!([pb]));
    let (status, body) = request(&h.app, "POST", "/vms", Some(spec)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = vm_id(&body);

    // Ending the share is refused while the target's VMs use it.
    let (status, _) = request(
        &h.app,
        "DELETE",
        &format!("/networks/pnet/shares/{pb}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = request(
        &h.app,
        "DELETE",
        &format!("/projects/{pb}/network-shares/pnet"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Deleting the network is refused too.
    let (status, _) = request(&h.app, "DELETE", "/networks/pnet", None).await;
    assert_eq!(status, StatusCode::CONFLICT);

    request(&h.app, "DELETE", &format!("/vms/{id}"), None).await;
    let (status, _) = request(
        &h.app,
        "DELETE",
        &format!("/projects/{pb}/network-shares/pnet"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, shares) = request(
        &h.app,
        "GET",
        &format!("/projects/{pb}/network-shares"),
        None,
    )
    .await;
    assert_eq!(shares, json!({}));
    // A project that still owns networks can't be deleted.
    let (status, _) = request(&h.app, "DELETE", &format!("/projects/{pa}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

/// Deleting a network is a request the network controller finishes
/// (spec/reconciliation.md §6.3, §10.3): with netd away it waits, says
/// why, and keeps the network from new VMs; once netd is back, netd's NAT
/// and bridge go, then the record.
#[tokio::test(flavor = "multi_thread")]
async fn network_deletion_waits_for_netd() {
    let Harness {
        app,
        manager,
        exec,
        _dir: dir,
    } = harness(true);
    let (status, body) = request(
        &app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    drop(app);
    drop(manager);

    // The same database, with netd unreachable.
    let away = dir.path().join("away");
    std::fs::create_dir_all(&away).unwrap();
    let manager =
        VmManager::with_db_path_and_netd(dir.path().join("cp.db"), Netd::new(&away)).unwrap();
    let app = create_router(manager.clone());
    let (status, net) = request(&app, "DELETE", "/networks/lab", None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{net}");
    let net = &net["network"];
    assert!(net["deletion_requested_at"].is_string(), "{net}");
    assert_eq!(net["phase"], "NETWORK_PHASE_NETD_UNAVAILABLE", "{net}");
    assert_eq!(net["conditions"][0]["reason"], "NetdUnavailable", "{net}");
    // No new VM may use it meanwhile.
    let (status, body) = request(
        &app,
        "POST",
        "/vms",
        Some(vm("late", json!([{"network": "lab"}]))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"].as_str().unwrap().contains("being deleted"),
        "{body}"
    );
    drop(app);
    drop(manager);

    // netd is back: the controller finishes the deletion.
    let manager = VmManager::with_db_path_and_netd(
        dir.path().join("cp.db"),
        Netd::new(dir.path().join("run")),
    )
    .unwrap();
    let app = create_router(manager.clone());
    manager.start_controllers();
    let mut gone = false;
    for _ in 0..100 {
        let (status, _) = request(&app, "GET", "/networks/lab", None).await;
        if status == StatusCode::NOT_FOUND {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(gone, "the network controller deleted it");
    let calls = exec.calls();
    assert!(
        calls
            .iter()
            .any(|c| c.starts_with("ip addr del") || c.contains("gxbr-lab")),
        "{calls:#?}"
    );
    manager.stop_controllers().await;
}

/// Host networks granted to a whole tenant (spec/tenancy.md D25), and
/// project networks shared only within their tenant.
#[tokio::test(flavor = "multi_thread")]
async fn host_networks_are_granted_to_tenants_and_shares_stay_in_one() {
    use glidex_control_plane::tenants::DEFAULT_TENANT;
    let h = harness(true);
    let (status, t) = request(&h.app, "POST", "/tenants", Some(json!({"slug": "acme"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{t}");
    let acme = t["tenant"]["id"].as_str().unwrap().to_string();
    let (status, a) = request(&h.app, "PUT", "/admissions/acme", Some(json!({}))).await;
    assert!(status.is_success(), "{a}");
    let projects = h.manager.projects();
    let pa = projects
        .create_in(&acme, "pa", String::new(), None)
        .unwrap()
        .id;
    let pa2 = projects
        .create_in(&acme, "pa2", String::new(), None)
        .unwrap()
        .id;
    let pd = projects.create("pd", String::new(), None).unwrap().id;
    let in_project = |name: &str, project: &str, net: &str| {
        let mut spec = vm(name, json!([{"network": net}]));
        spec["project"] = json!(project);
        spec
    };

    // Granted to acme, by slug: its projects may attach, others may not.
    let (status, net) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "tenants": ["acme"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{net}");
    assert_eq!(net["network"]["tenant_grants"], json!([acme]));
    assert!(
        net["network"]["grants"].is_null(),
        "no default project grant: {net}"
    );
    let (status, body) = request(&h.app, "POST", "/vms", Some(in_project("a1", &pa, "lab"))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = request(&h.app, "POST", "/vms", Some(in_project("d1", &pd, "lab"))).await;
    assert!(status.is_client_error(), "{status} {body}");
    // Cedar (base.network-grant) refuses before the manager's own check.
    assert!(
        body["error"] == "forbidden" || body["message"].as_str().unwrap().contains("not available"),
        "{body}"
    );

    // Regranted to the default tenant instead.
    let (status, net) = request(
        &h.app,
        "PUT",
        "/networks/lab/grants",
        Some(json!({"tenants": [DEFAULT_TENANT]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{net}");
    assert_eq!(net["network"]["tenant_grants"], json!([DEFAULT_TENANT]));
    let (status, body) = request(&h.app, "POST", "/vms", Some(in_project("d2", &pd, "lab"))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _) = request(&h.app, "POST", "/vms", Some(in_project("a2", &pa, "lab"))).await;
    assert!(status.is_client_error());
    let (status, _) = request(
        &h.app,
        "PUT",
        "/networks/lab/grants",
        Some(json!({"tenants": ["no-such-tenant"]})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A project network of acme is offered to another acme project, but an
    // offer to the default tenant's project records nothing, with the
    // same answer.
    let (status, body) = request(
        &h.app,
        "POST",
        &format!("/projects/{pa}/networks"),
        Some(json!({"name": "apnet", "mode": "NETWORK_MODE_ISOLATED"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    for target in [&pd, &pa2] {
        let (status, why) = request(
            &h.app,
            "POST",
            "/networks/apnet/shares",
            Some(json!({"project": target})),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{why}");
    }
    let (_, shares) = request(
        &h.app,
        "GET",
        &format!("/projects/{pd}/network-shares"),
        None,
    )
    .await;
    assert_eq!(shares, json!({}), "nothing across tenants");
    let (_, shares) = request(
        &h.app,
        "GET",
        &format!("/projects/{pa2}/network-shares"),
        None,
    )
    .await;
    assert_eq!(
        shares["shares"][0]["status"], "SHARE_STATUS_OFFERED",
        "{shares}"
    );
}

/// D24 (spec/tenancy.md): networks are keyed by id; a project network's
/// name is unique in its tenant, a host network's on the cluster. A name
/// several networks have is ambiguous where both are in view; a VM's NIC
/// names its network by id, with the name beside it; the RPC transport
/// takes the same keys.
#[tokio::test(flavor = "multi_thread")]
async fn network_names_are_per_tenant_and_ids_are_the_keys() {
    let h = harness(true);
    let (status, t) = request(&h.app, "POST", "/tenants", Some(json!({"slug": "acme"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{t}");
    let (status, a) = request(&h.app, "PUT", "/admissions/acme", Some(json!({}))).await;
    assert!(status.is_success(), "{a}");
    let acme = t["tenant"]["id"].as_str().unwrap().to_string();
    let projects = h.manager.projects();
    let pa = projects
        .create_in(&acme, "pa", String::new(), None)
        .unwrap()
        .id;
    let pa2 = projects
        .create_in(&acme, "pa2", String::new(), None)
        .unwrap()
        .id;
    let pd = projects.create("pd", String::new(), None).unwrap().id;
    let create = |project: &str| {
        let uri = format!("/projects/{project}/networks");
        let app = h.app.clone();
        async move {
            request(
                &app,
                "POST",
                &uri,
                Some(json!({"name": "web", "mode": "NETWORK_MODE_ISOLATED"})),
            )
            .await
        }
    };

    // Two tenants may each have `web`; one tenant may not have two.
    let (status, a_web) = create(&pa).await;
    assert_eq!(status, StatusCode::CREATED, "{a_web}");
    let (status, d_web) = create(&pd).await;
    assert_eq!(status, StatusCode::CREATED, "{d_web}");
    let (status, body) = create(&pa2).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let a_id = a_web["network"]["id"].as_str().unwrap().to_string();
    let d_id = d_web["network"]["id"].as_str().unwrap().to_string();
    assert_ne!(a_id, d_id);
    assert_ne!(a_web["network"]["bridge"], d_web["network"]["bridge"]);
    // A host network can't take a name a project network has.
    let (status, body) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "web", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Seeing both (the test caller sees every project): the name is
    // ambiguous, the ids are not.
    let (status, body) = request(&h.app, "GET", "/networks/web", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "ambiguous", "{body}");
    let (status, body) = request(&h.app, "GET", &format!("/networks/{a_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["network"]["project"], pa.as_str());

    // A VM's network name resolves in its project's tenant, and its NIC
    // carries the id and the name.
    let mut spec = vm("v1", json!([{"network": "web"}]));
    spec["project"] = json!(pd);
    let (status, body) = request(&h.app, "POST", "/vms", Some(spec)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let nic = &vm_of(&body)["nics"][0];
    assert_eq!(
        (nic["network"].as_str(), nic["network_name"].as_str()),
        (Some(d_id.as_str()), Some("web")),
        "{body}"
    );
    let vm = vm_id(&body);

    // The same over RPC (Connect, JSON).
    let (status, body) = request(
        &h.app,
        "POST",
        "/glidex.network.v1.NetworkService/GetNetwork",
        Some(json!({"id": d_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["network"]["name"], "web");
    assert_eq!(body["network"]["mode"], "NETWORK_MODE_ISOLATED");
    let (status, body) = request(
        &h.app,
        "POST",
        "/glidex.network.v1.NetworkService/GetNetwork",
        Some(json!({"id": "web"})),
    )
    .await;
    // `ambiguous` (the ErrorInfo reason) is a failed precondition in RPCs.
    assert_eq!(body["code"], "failed_precondition", "{status} {body}");
    let (status, body) = request(
        &h.app,
        "POST",
        "/glidex.network.v1.NetworkService/ListNetworks",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["networks"].as_array().unwrap().len(), 2, "{body}");
    let (status, body) = request(
        &h.app,
        "POST",
        "/glidex.network.v1.RouterService/ListRouters",
        Some(json!({"project": pa})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Deleting by id leaves the other tenant's `web` alone.
    let (status, body) = request(&h.app, "DELETE", &format!("/vms/{vm}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = request(&h.app, "DELETE", &format!("/networks/{d_id}"), None).await;
    assert!(status.is_success(), "{status} {body}");
    let (status, body) = request(&h.app, "GET", "/networks/web", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["network"]["id"], a_id.as_str());
}

/// The next message of a Connect server stream (`application/connect+json`);
/// `None` at its end.
async fn next_frame(body: &mut Body, buf: &mut Vec<u8>) -> Option<Value> {
    loop {
        if buf.len() >= 5 {
            let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
            if buf.len() >= 5 + len {
                let flags = buf[0];
                let msg: Value = serde_json::from_slice(&buf[5..5 + len]).unwrap();
                buf.drain(..5 + len);
                return (flags & 0x02 == 0).then_some(msg);
            }
        }
        let frame = tokio::time::timeout(std::time::Duration::from_secs(15), body.frame())
            .await
            .expect("a frame within 15 s")?
            .unwrap();
        if let Ok(data) = frame.into_data() {
            buf.extend_from_slice(&data);
        }
    }
}

/// `glidex.watch.v1.WatchService.WatchEvents` (spec/grpc.md G4): an
/// object's events as a stream, the ones it has, `synced`, then new ones
/// as they happen.
#[tokio::test(flavor = "multi_thread")]
async fn a_networks_events_stream_as_they_happen() {
    use glidex_netd::proto::Op;
    let h = harness(true);
    let (_, body) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    let lab = body["network"]["id"].as_str().unwrap().to_string();
    let mut req = vec![0u8];
    let msg = json!({"kind": "WATCH_KIND_NETWORK", "id": lab}).to_string();
    req.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    req.extend_from_slice(msg.as_bytes());
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/glidex.watch.v1.WatchService/WatchEvents")
                .header("content-type", "application/connect+json")
                .body(Body::from(req))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    let mut buf = Vec::new();
    loop {
        let e = next_frame(&mut body, &mut buf).await.expect("synced");
        if e["type"] == "WATCH_EVENT_TYPE_SYNCED" {
            break;
        }
        assert_eq!(e["type"], "WATCH_EVENT_TYPE_ADDED", "{e}");
    }
    // netd lost the NAT; the controller makes it again and says so.
    let netd = Netd::new(h._dir.path().join("run"));
    netd.call::<Value>(Op::DeleteNat {
        bridge: "gxbr-lab".into(),
    })
    .unwrap();
    h.manager.reconcile_network(&lab).await.unwrap();
    let e = next_frame(&mut body, &mut buf).await.expect("an event");
    assert_eq!(e["type"], "WATCH_EVENT_TYPE_ADDED", "{e}");
    assert_eq!(e["event"]["reason"], "NatRestored", "{e}");

    // A kind without events, or an object the caller can't see, is refused.
    let mut req = vec![0u8];
    let msg = json!({"kind": "WATCH_KIND_NODE", "id": "local"}).to_string();
    req.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    req.extend_from_slice(msg.as_bytes());
    let resp = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/glidex.watch.v1.WatchService/WatchEvents")
                .header("content-type", "application/connect+json")
                .body(Body::from(req))
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = resp.into_body();
    let mut buf = Vec::new();
    assert!(
        next_frame(&mut body, &mut buf).await.is_none(),
        "refused: the stream ends at once with its error"
    );
}

/// spec/workload-identity.md §15: a node NAT network's guests may be given
/// this host's API; netd opens its ports for that bridge only, and closes
/// them again. An isolated network's can't.
#[tokio::test(flavor = "multi_thread")]
async fn a_nat_network_may_open_the_hosts_api() {
    let h = harness(true);
    let (status, net) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "lab", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-lab"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{net}");
    let id = net["network"]["id"].as_str().unwrap().to_string();
    assert!(
        net["network"].get("host_api").is_none(),
        "closed by default"
    );
    let last_nft = || {
        h.exec
            .calls()
            .into_iter()
            .rfind(|c| c.starts_with("nft -f -") && c.contains("table inet glidex {"))
            .unwrap_or_default()
    };
    let open = "iifname \"gxbr-lab\" tcp dport { 8841 } accept";
    assert!(!last_nft().contains(open));

    let (status, net) = request(
        &h.app,
        "PUT",
        "/networks/lab/host_api",
        Some(json!({"host_api": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{net}");
    assert_eq!(net["network"]["host_api"], true, "{net}");
    h.manager.reconcile_network(&id).await.unwrap();
    assert!(last_nft().contains(open), "{}", last_nft());
    let (_, ev) = request(&h.app, "GET", "/networks/lab/events", None).await;
    assert!(ev.to_string().contains("HostApi"), "{ev}");

    let (status, _) = request(
        &h.app,
        "PUT",
        "/networks/lab/host_api",
        Some(json!({"host_api": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    h.manager.reconcile_network(&id).await.unwrap();
    assert!(!last_nft().contains(open), "{}", last_nft());

    let (status, _) = request(
        &h.app,
        "POST",
        "/networks",
        Some(json!({"name": "iso", "mode": "NETWORK_MODE_ISOLATED", "bridge": "gxbr-iso"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = request(
        &h.app,
        "PUT",
        "/networks/iso/host_api",
        Some(json!({"host_api": true})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
