//! Control-plane networking against an in-process glidex-netd (real Unix
//! socket, recording executor underneath): no root, no OVS.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use glidex_control_plane::api::create_router;
use glidex_control_plane::network::Netd;
use glidex_control_plane::state::VmManager;
use glidex_netd::server::{self, Access, Config, Netd as NetdServer};
use glidex_netd::supervisor::FakeSupervisor;
use glidex_ovs::{Output, RecordingExec};
use http_body_util::BodyExt;
use serde_json::{json, Value};
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

fn fake_exec(ovs_running: bool) -> Arc<RecordingExec> {
    let exec = Arc::new(RecordingExec::new());
    if ovs_running {
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
    } else {
        exec.on("ovs-vsctl --version", Output::failed(127, "not found"));
    }
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

/// Control plane + fake netd listening in a temp run dir.
fn harness(ovs_running: bool) -> Harness {
    let dir = TempDir::new().unwrap();
    let run_dir = dir.path().join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    let exec = fake_exec(ovs_running);
    let my_group = nix::unistd::Group::from_gid(nix::unistd::getgid()).unwrap().unwrap().name;
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

    let manager = VmManager::with_db_path_and_netd(dir.path().join("cp.db"), Netd::new(&run_dir)).unwrap();
    Harness {
        app: create_router(manager.clone()),
        manager,
        exec,
        _dir: dir,
    }
}

async fn request(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(b) => builder.header("content-type", "application/json").body(Body::from(b.to_string())).unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn vm(name: &str, networks: Value) -> Value {
    json!({
        "name": name, "vcpu_count": 1, "mem_size_mib": 512,
        "hypervisor": "cloudhypervisor",
        "firmware_path": "/nonexistent/CLOUDHV.fd",
        "rootfs_path": "/nonexistent/disk.raw",
        "networks": networks
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn nat_network_crud() {
    let h = harness(true);
    let (status, body) = request(&h.app, "POST", "/networks", Some(json!({"name": "lab", "mode": "nat"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["bridge"], "gxbr-lab");
    assert_eq!(body["port_type"], "tap");
    let calls = h.exec.calls();
    assert!(calls.iter().any(|c| c == "ip addr replace 10.88.0.1/24 dev gxbr-lab"), "{calls:#?}");

    let (status, list) = request(&h.app, "GET", "/networks", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (status, _) = request(&h.app, "POST", "/networks", Some(json!({"name": "lab", "mode": "nat"}))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, _) = request(&h.app, "DELETE", "/networks/lab", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&h.app, "GET", "/networks/lab", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn vms_reference_networks() {
    let h = harness(true);
    request(&h.app, "POST", "/networks", Some(json!({"name": "lab", "mode": "nat"}))).await;

    let (status, body) = request(&h.app, "POST", "/vms", Some(vm("v1", json!([{"network": "lab"}])))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let mac = body["nics"][0]["mac"].as_str().unwrap().to_string();
    assert!(mac.starts_with("02:"), "stable MAC assigned at create: {mac}");
    let id = body["id"].as_str().unwrap().to_string();

    let (status, body) = request(&h.app, "POST", "/vms", Some(vm("v2", json!([{"network": "nope"}])))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let mut qemu = vm("v3", json!([{"network": "lab"}]));
    qemu["hypervisor"] = json!("qemu");
    qemu["kernel_image_path"] = json!("/k");
    qemu.as_object_mut().unwrap().remove("firmware_path");
    let (status, body) = request(&h.app, "POST", "/vms", Some(qemu)).await;
    assert_eq!(status, StatusCode::CREATED, "QEMU VMs take networks too: {body}");
    assert!(body["nics"][0]["mac"].as_str().unwrap().starts_with("02:"), "{body}");
    let qemu_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = request(&h.app, "POST", "/vms", Some(vm("v4", json!([{"network": "lab", "mac": "01:00:5e:00:00:01"}])))).await;
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

async fn failed_start_detaches_ports_for(hypervisor: &str, binary: &str) {
    if std::process::Command::new(binary).arg("--version").output().is_err() {
        eprintln!("skipping: {binary} not installed");
        return;
    }
    let h = harness(true);
    h.manager.initialize().await.unwrap();
    h.manager.start_controllers();
    request(&h.app, "POST", "/networks", Some(json!({"name": "lab", "mode": "nat"}))).await;
    let mut spec = vm("boom", json!([{"network": "lab"}]));
    spec["hypervisor"] = json!(hypervisor);
    let (_, body) = request(&h.app, "POST", "/vms", Some(spec)).await;
    let id = body["id"].as_str().unwrap().to_string();
    let port = glidex_ovs::names::port_name(&id, 0).unwrap();

    // The firmware path doesn't exist, so the launch fails after the port
    // was attached (spec/reconciliation.md §9.1 step 5).
    let (status, body) = request(&h.app, "POST", &format!("/vms/{id}/start?wait=60"), None).await;
    assert!(status.is_server_error() || status.is_client_error(), "{status} {body}");
    assert_eq!(body["details"]["vm"]["state"], "failed", "{body}");
    let calls = h.exec.calls();
    let attached = calls.iter().position(|c| c.starts_with(&format!("ip tuntap add dev {port}"))).expect("attached");

    // Ports belong to the VM while it should run (D16); stopping it
    // releases them.
    let (status, body) = request(&h.app, "POST", &format!("/vms/{id}/stop?wait=60"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // `created` when the launch failed before anything ran (QEMU refuses
    // the missing firmware while building its command line).
    assert!(matches!(body["state"].as_str(), Some("stopped" | "created")), "{body}");
    let calls = h.exec.calls();
    let detached = calls.iter().rposition(|c| c == &format!("ovs-vsctl --if-exists del-port {port}")).expect("detached");
    assert!(detached > attached);
    assert!(body["nics"][0]["port"].is_null(), "{body}");
    request(&h.app, "DELETE", &format!("/vms/{id}?wait=30"), None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ovs_status_and_bridges() {
    let h = harness(true);
    let (status, body) = request(&h.app, "GET", "/ovs/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["netd"]["available"], true);
    assert_eq!(body["netd"]["access"], "full");
    assert_eq!(body["host"]["ovs_version"], "3.7.1");

    let (status, body) = request(&h.app, "POST", "/ovs/bridges", Some(json!({"name": "gxbr-lab", "datapath": "system"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = request(&h.app, "GET", "/ovs/bridges", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["spec"]["name"], "gxbr-lab");

    let (status, body) = request(&h.app, "POST", "/ovs/bridges", Some(json!({"name": "Bad!", "datapath": "system"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_network");
}

#[tokio::test(flavor = "multi_thread")]
async fn netd_errors_map_to_rest() {
    // OVS not running: creating a network is unsupported on this host.
    let h = harness(false);
    let (status, body) = request(&h.app, "POST", "/networks", Some(json!({"name": "lab", "mode": "nat"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "unsupported_on_host");
    assert!(body["details"]["missing"].as_array().is_some());

    // No netd at all.
    let dir = TempDir::new().unwrap();
    let manager = VmManager::with_db_path_and_netd(dir.path().join("cp.db"), Netd::new(dir.path())).unwrap();
    let app = create_router(manager);
    let (status, body) = request(&app, "POST", "/networks", Some(json!({"name": "lab", "mode": "nat"}))).await;
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
    let pa = h.manager.projects().create("pa", String::new(), None).unwrap().id;
    let pb = h.manager.projects().create("pb", String::new(), None).unwrap().id;

    let (status, net) = request(&h.app, "POST", &format!("/projects/{pa}/networks"), Some(json!({"name": "pnet", "mode": "nat"}))).await;
    assert_eq!(status, StatusCode::CREATED, "{net}");
    assert_eq!(net["project"], pa);
    assert!(net["bridge"].as_str().unwrap().starts_with("gxp-"), "{net}");
    // Project networks are NAT only and pick their own bridge.
    let (status, _) = request(&h.app, "POST", &format!("/projects/{pa}/networks"), Some(json!({"name": "x", "mode": "isolated"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Quota: 2 project networks by default.
    let req = |n: &str| serde_json::from_value(json!({"name": n, "mode": "nat"})).unwrap();
    h.manager.create_project_network(&pa, req("pnet2"), glidex_control_plane::tenancy::QuotaMode::Enforce).await.unwrap();
    let over = h.manager.create_project_network(&pa, req("pnet3"), glidex_control_plane::tenancy::QuotaMode::Enforce).await;
    assert!(matches!(over, Err(glidex_control_plane::state::VmManagerError::QuotaExceeded(_))), "{over:?}");

    // Not usable from another project until shared and accepted.
    let mut spec = vm("b1", json!([{"network": "pnet"}]));
    spec["project"] = json!(pb);
    let (status, body) = request(&h.app, "POST", "/vms", Some(spec.clone())).await;
    assert!(status.is_client_error(), "{status} {body}");
    // Project networks are never granted, only shared.
    let (status, _) = request(&h.app, "PUT", "/networks/pnet/grants", Some(json!({"grants": [pb]}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = request(&h.app, "POST", "/networks/pnet/shares", Some(json!({"project": pb}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    // Offers to unknown projects look the same and record nothing.
    let (status, _) = request(&h.app, "POST", "/networks/pnet/shares", Some(json!({"project": "no-such-project"}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (_, shares) = request(&h.app, "GET", &format!("/projects/{pb}/network-shares"), None).await;
    assert_eq!(shares, json!([{"network": "pnet", "owner_project": pa, "status": "offered", "expires_at": shares[0]["expires_at"]}]));
    // An offer alone grants nothing.
    let (status, _) = request(&h.app, "POST", "/vms", Some(spec.clone())).await;
    assert!(status.is_client_error());

    let (status, body) = request(&h.app, "POST", &format!("/projects/{pb}/network-shares/pnet/accept"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["shares"], json!([pb]));
    let (status, body) = request(&h.app, "POST", "/vms", Some(spec)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();

    // Ending the share is refused while the target's VMs use it.
    let (status, _) = request(&h.app, "DELETE", &format!("/networks/pnet/shares/{pb}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = request(&h.app, "DELETE", &format!("/projects/{pb}/network-shares/pnet"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // Deleting the network is refused too.
    let (status, _) = request(&h.app, "DELETE", "/networks/pnet", None).await;
    assert_eq!(status, StatusCode::CONFLICT);

    request(&h.app, "DELETE", &format!("/vms/{id}"), None).await;
    let (status, _) = request(&h.app, "DELETE", &format!("/projects/{pb}/network-shares/pnet"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, shares) = request(&h.app, "GET", &format!("/projects/{pb}/network-shares"), None).await;
    assert_eq!(shares, json!([]));
    // A project that still owns networks can't be deleted.
    let (status, _) = request(&h.app, "DELETE", &format!("/projects/{pa}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
}
