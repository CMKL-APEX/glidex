mod common;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use glidex_control_plane::api::create_router;

/// Helper to create a test app instance with a temporary database
fn create_test_app() -> (axum::Router, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let vm_manager = common::with_db_path(db_path).unwrap();
    (create_router(vm_manager), temp_dir)
}

/// Helper to get the db path from a temp dir
fn db_path_from_temp_dir(temp_dir: &TempDir) -> std::path::PathBuf {
    temp_dir.path().join("test.db")
}

/// Helper to extract JSON body from response
async fn body_to_json(body: Body) -> Value {
    let bytes = body.collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// The VM in a `{vm}` answer (`POST /vms`, `GET /vms/{id}`, writes), in the
/// proto3 JSON shape (spec/18-grpc.md §9.1).
fn vm_of(v: &Value) -> &Value {
    assert!(v["vm"].is_object(), "not a VM answer: {v}");
    &v["vm"]
}

/// The VMs of a `GET /vms` answer (`{vms}`; proto3 JSON leaves an empty
/// list out).
fn vms_of(v: &Value) -> Vec<Value> {
    assert!(v.is_object(), "not a VM list: {v}");
    v["vms"].as_array().cloned().unwrap_or_default()
}

/// A proto3 JSON 64-bit integer: a string (`"3"`) or a number; left out
/// (null) at its default, 0.
fn num(v: &Value) -> u64 {
    match v {
        Value::Null => 0,
        Value::String(s) => s.parse().unwrap_or_else(|_| panic!("not a number: {v}")),
        _ => v.as_u64().unwrap_or_else(|| panic!("not a number: {v}")),
    }
}

/// The short lower-case name of a proto enum value: `VM_STATE_RUNNING` with
/// prefix `VM_STATE_` is `running`.
fn short(v: &Value, prefix: &str) -> String {
    let s = v.as_str().unwrap_or_else(|| panic!("not an enum: {v}"));
    s.strip_prefix(prefix)
        .unwrap_or_else(|| panic!("{s} is not a {prefix}* value"))
        .to_ascii_lowercase()
}

/// A VM's `state` (`created`, `running`, ...).
fn state(vm: &Value) -> String {
    short(&vm["state"], "VM_STATE_")
}

// ============================================================================
// Health Check Tests
// ============================================================================

#[tokio::test]
async fn test_health_check() {
    let (app, _temp_dir) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = body_to_json(response.into_body()).await;
    assert_eq!(body["status"], "ok");
}

// ============================================================================
// VM List Tests
// ============================================================================

#[tokio::test]
async fn test_list_vms_empty() {
    let (app, _temp_dir) = create_test_app();

    let response = app
        .oneshot(Request::builder().uri("/vms").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = body_to_json(response.into_body()).await;
    assert_eq!(vms_of(&body).len(), 0, "{body}");
}

// ============================================================================
// VM Create Tests
// ============================================================================

#[tokio::test]
async fn test_create_vm_success() {
    let (app, _temp_dir) = create_test_app();

    let create_request = json!({
        "name": "test-vm",
        "vcpu_count": 2,
        "mem_size_mib": 512,
        "kernel_image_path": "/path/to/kernel",
        "rootfs_path": "/path/to/rootfs.ext4"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from(create_request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    let body = body_to_json(response.into_body()).await;
    let vm = vm_of(&body);
    assert_eq!(vm["name"], "test-vm");
    assert_eq!(vm["vcpu_count"], 2);
    assert_eq!(vm["mem_size_mib"], 512);
    assert_eq!(state(vm), "created");
    assert!(vm["id"].is_string());
}

/// A kernel VM that names no arguments gets the default for the
/// architecture it runs on: its pinned node's, else this host's
/// (spec/23-kernel-boot.md K5). aarch64's serial port is the PL011,
/// `ttyAMA0`.
#[tokio::test]
async fn kernel_args_default_to_the_nodes_serial_console() {
    let temp_dir = TempDir::new().unwrap();
    let manager = common::with_db_path(temp_dir.path().join("test.db")).unwrap();
    manager.report_self().unwrap();
    let app = create_router(manager.clone());
    let create = |name: &str, node: Option<&str>| {
        let mut body = json!({
            "name": name, "vcpu_count": 1, "mem_size_mib": 256,
            "kernel_image_path": "/path/to/kernel", "rootfs_path": "/path/to/rootfs.ext4",
        });
        if let Some(n) = node {
            body["node"] = json!(n);
        }
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
    };
    let host = match std::env::consts::ARCH {
        "aarch64" => "ttyAMA0",
        _ => "ttyS0",
    };
    let r = create("on-the-host", None).await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let body = body_to_json(r.into_body()).await;
    let args = |body: &Value| {
        let id = vm_of(body)["id"].as_str().unwrap().to_string();
        let m = manager.clone();
        async move { m.get_vm(&id).await.unwrap().config().kernel_args.clone() }
    };
    assert_eq!(
        args(&body).await,
        format!("console={host} root=/dev/vda reboot=k panic=1")
    );

    // Pinned to a node that reports aarch64.
    let id = manager.local_node_id();
    let mut node = manager.nodes().get(&id).unwrap().expect("this host's node");
    node.status.features.arch = "aarch64".into();
    manager.nodes().put(&node).unwrap();
    let r = create("on-an-arm-node", Some(&id)).await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let body = body_to_json(r.into_body()).await;
    assert_eq!(
        args(&body).await,
        "console=ttyAMA0 root=/dev/vda reboot=k panic=1"
    );
}

#[tokio::test]
async fn test_create_vm_with_optional_fields() {
    let (app, _temp_dir) = create_test_app();

    let create_request = json!({
        "name": "test-vm-full",
        "vcpu_count": 4,
        "mem_size_mib": 1024,
        "kernel_image_path": "/path/to/kernel",
        "rootfs_path": "/path/to/rootfs.ext4",
        "kernel_args": "console=ttyS0 reboot=k panic=1"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from(create_request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    let body = body_to_json(response.into_body()).await;
    let vm = vm_of(&body);
    assert_eq!(vm["name"], "test-vm-full");
    assert_eq!(vm["vcpu_count"], 4);
    assert_eq!(vm["mem_size_mib"], 1024);
}

#[tokio::test]
async fn test_create_vm_duplicate_name() {
    let (app, _temp_dir) = create_test_app();

    let create_request = json!({
        "name": "duplicate-vm",
        "vcpu_count": 1,
        "mem_size_mib": 256,
        "kernel_image_path": "/path/to/kernel",
        "rootfs_path": "/path/to/rootfs.ext4"
    });

    // Create first VM
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from(create_request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);

    // Try to create VM with same name
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from(create_request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);

    let body = body_to_json(response.into_body()).await;
    assert_eq!(body["error"], "conflict");
}

#[tokio::test]
async fn test_create_vm_invalid_json() {
    let (app, _temp_dir) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from("invalid json"))
                .unwrap(),
        )
        .await
        .unwrap();

    // Axum returns 400 Bad Request for malformed JSON
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_vm_missing_required_fields() {
    let (app, _temp_dir) = create_test_app();

    let create_request = json!({
        "name": "test-vm"
        // missing vcpu_count, mem_size_mib, kernel_image_path
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from(create_request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = body_to_json(response.into_body()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

// ============================================================================
// VM Get Tests
// ============================================================================

#[tokio::test]
async fn test_get_vm_success() {
    let (app, _temp_dir) = create_test_app();

    // Create a VM first
    let create_request = json!({
        "name": "get-test-vm",
        "vcpu_count": 2,
        "mem_size_mib": 512,
        "kernel_image_path": "/path/to/kernel",
        "rootfs_path": "/path/to/rootfs.ext4"
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from(create_request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let created_vm = body_to_json(response.into_body()).await;
    let vm_id = vm_of(&created_vm)["id"].as_str().unwrap();

    // Get the VM
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/vms/{}", vm_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = body_to_json(response.into_body()).await;
    assert_eq!(vm_of(&body)["id"], vm_id);
    assert_eq!(vm_of(&body)["name"], "get-test-vm");
}

#[tokio::test]
async fn test_get_vm_not_found() {
    let (app, _temp_dir) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/vms/nonexistent-id")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let body = body_to_json(response.into_body()).await;
    assert_eq!(body["error"], "not_found");
}

// ============================================================================
// VM Delete Tests
// ============================================================================

#[tokio::test]
async fn test_delete_vm_success() {
    let (app, _temp_dir) = create_test_app();

    // Create a VM first
    let create_request = json!({
        "name": "delete-test-vm",
        "vcpu_count": 1,
        "mem_size_mib": 256,
        "kernel_image_path": "/path/to/kernel",
        "rootfs_path": "/path/to/rootfs.ext4"
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from(create_request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let created_vm = body_to_json(response.into_body()).await;
    let vm_id = vm_of(&created_vm)["id"].as_str().unwrap();

    // Delete the VM
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/vms/{}", vm_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // Verify VM is gone
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/vms/{}", vm_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_vm_not_found() {
    let (app, _temp_dir) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/vms/nonexistent-id")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ============================================================================
// VM List After Operations Tests
// ============================================================================

#[tokio::test]
async fn test_list_vms_after_create() {
    let (app, _temp_dir) = create_test_app();

    // Create two VMs
    for name in ["vm-1", "vm-2"] {
        let create_request = json!({
            "name": name,
            "vcpu_count": 1,
            "mem_size_mib": 256,
            "kernel_image_path": "/path/to/kernel",
            "rootfs_path": "/path/to/rootfs.ext4"
        });

        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vms")
                    .header("content-type", "application/json")
                    .body(Body::from(create_request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
    }

    // List VMs
    let response = app
        .oneshot(Request::builder().uri("/vms").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = body_to_json(response.into_body()).await;
    assert_eq!(vms_of(&body).len(), 2, "{body}");
}

// ============================================================================
// VM Lifecycle Tests (without an actual hypervisor)
// ============================================================================

#[tokio::test]
async fn test_start_vm_not_found() {
    let (app, _temp_dir) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms/nonexistent-id/start")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_stop_vm_not_found() {
    let (app, _temp_dir) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms/nonexistent-id/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_pause_vm_not_found() {
    let (app, _temp_dir) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms/nonexistent-id/pause")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Create a kernel-boot VM through the API; returns its id.
async fn create_simple_vm(app: &axum::Router, name: &str) -> String {
    let create_request = json!({
        "name": name,
        "vcpu_count": 1,
        "mem_size_mib": 256,
        "kernel_image_path": "/path/to/kernel",
        "rootfs_path": "/path/to/rootfs.ext4"
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from(create_request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    vm_of(&body_to_json(response.into_body()).await)["id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn send(
    app: &axum::Router,
    method: &str,
    uri: String,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Stopping a stopped VM writes nothing: the spec already says stopped
/// (spec/12-reconciliation.md §9.2).
#[tokio::test]
async fn test_stop_of_a_stopped_vm_is_a_no_op() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "stop-test-vm").await;
    let (status, body) = send(&app, "POST", format!("/vms/{}/stop", vm_id), None, &[]).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let vm = vm_of(&body);
    assert_eq!(short(&vm["desired_state"], "POWER_STATE_"), "stopped");
    assert_eq!(state(vm), "created");
    assert_eq!(num(&vm["generation"]), 1, "nothing changed");
}

#[tokio::test]
async fn test_graceful_stop_records_the_grace() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "graceful-vm").await;

    let (status, body) = send(
        &app,
        "POST",
        format!("/vms/{}/stop?graceful_timeout_secs=5", vm_id),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        (
            vm_of(&body)["stop_grace_secs"].as_u64(),
            num(&vm_of(&body)["generation"])
        ),
        (Some(5), 2)
    );

    let (status, _) = send(
        &app,
        "POST",
        "/vms/nonexistent-id/stop?graceful_timeout_secs=5".into(),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &app,
        "POST",
        format!("/vms/{}/stop?graceful_timeout_secs=soon", vm_id),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Pause on a stopped VM means "launch it, then pause" (§7.1); the
/// controller does that, the API only records it.
#[tokio::test]
async fn test_pause_records_the_desired_state() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "pause-test-vm").await;
    let (status, body) = send(&app, "POST", format!("/vms/{}/pause", vm_id), None, &[]).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let vm = vm_of(&body);
    assert_eq!(short(&vm["desired_state"], "POWER_STATE_"), "paused");
    assert_eq!(num(&vm["generation"]), 2);
    assert_eq!(num(&vm["observed_generation"]), 0, "no controller ran");
}

#[tokio::test]
async fn test_patch_edits_spec_and_refuses_immutable_fields() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "patch-vm").await;
    let (status, body) = send(
        &app,
        "PATCH",
        format!("/vms/{}", vm_id),
        Some(json!({"change": {"vcpu_count": 4, "restart_policy": "RESTART_POLICY_NEVER"}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        (
            vm_of(&body)["vcpu_count"].as_u64(),
            short(&vm_of(&body)["restart_policy"], "RESTART_POLICY_")
        ),
        (Some(4), "never".to_string())
    );

    let (status, body) = send(
        &app,
        "PATCH",
        format!("/vms/{}", vm_id),
        Some(json!({"change": {}, "update_mask": "hypervisor"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_config");
    let (status, _) = send(
        &app,
        "PATCH",
        format!("/vms/{}", vm_id),
        Some(json!({"bogus": 1})),
        &[],
    )
    .await;
    assert!(status.is_client_error());
    let (status, body) = send(
        &app,
        "PATCH",
        format!("/vms/{}", vm_id),
        Some(json!({"change": {"stop_grace_secs": 301}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn test_if_match_guards_writes() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "match-vm").await;
    let (status, body) = send(
        &app,
        "POST",
        format!("/vms/{}/pause", vm_id),
        None,
        &[("if-match", "999")],
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");
    assert_eq!(body["error"], "precondition_failed");
    let (status, _) = send(
        &app,
        "POST",
        format!("/vms/{}/pause", vm_id),
        None,
        &[("if-match", "1")],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_events_record_spec_writes() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "events-vm").await;
    send(&app, "POST", format!("/vms/{}/start", vm_id), None, &[]).await;
    let (status, body) = send(&app, "GET", format!("/vms/{}/events", vm_id), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let reasons: Vec<&str> = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, ["Created", "PowerChanged"]);
}

#[tokio::test]
async fn test_never_started_vm_is_deleted_at_once() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "gone-vm").await;
    let (status, _) = send(&app, "DELETE", format!("/vms/{}", vm_id), None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(&app, "GET", format!("/vms/{}", vm_id), None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ============================================================================
// Console Endpoint Tests
// ============================================================================

#[tokio::test]
async fn test_get_console_info_not_found() {
    let (app, _temp_dir) = create_test_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/vms/nonexistent-id/console")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_console_info_vm_not_running() {
    let (app, _temp_dir) = create_test_app();

    // Create a VM
    let create_request = json!({
        "name": "console-test-vm",
        "vcpu_count": 1,
        "mem_size_mib": 256,
        "kernel_image_path": "/path/to/kernel",
        "rootfs_path": "/path/to/rootfs.ext4"
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/vms")
                .header("content-type", "application/json")
                .body(Body::from(create_request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let created_vm = body_to_json(response.into_body()).await;
    let vm_id = vm_of(&created_vm)["id"].as_str().unwrap();

    // Get console info (VM not running)
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/vms/{}/console", vm_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = body_to_json(response.into_body()).await;
    assert_eq!(body["vm_id"], vm_id);
    assert_eq!(body["available"], false);
    // The console is reached through the API; the private socket and log
    // paths are not exposed (spec/11-security.md §9).
    assert_eq!(body["websocket"], format!("/vms/{}/console/ws", vm_id));
    assert!(body.get("console_socket_path").is_none());
}

// ============================================================================
// Persistence Tests
// ============================================================================

#[tokio::test]
async fn test_vm_persists_across_restart() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = db_path_from_temp_dir(&temp_dir);

    let vm_id: String;

    // Phase 1: Create a VM with the first manager instance
    {
        let vm_manager = common::with_db_path(db_path.clone()).unwrap();
        vm_manager.initialize().await.unwrap();
        let app = create_router(vm_manager);

        let create_request = json!({
            "name": "persistent-vm",
            "vcpu_count": 2,
            "mem_size_mib": 512,
            "kernel_image_path": "/path/to/kernel",
            "rootfs_path": "/path/to/rootfs.ext4"
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vms")
                    .header("content-type", "application/json")
                    .body(Body::from(create_request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        let body = body_to_json(response.into_body()).await;
        vm_id = vm_of(&body)["id"].as_str().unwrap().to_string();
    }
    // First manager is dropped here (simulates restart)

    // Phase 2: Create a new manager and verify VM is recovered
    {
        let vm_manager = common::with_db_path(db_path).unwrap();
        vm_manager.initialize().await.unwrap();
        let app = create_router(vm_manager);

        // List VMs and verify our VM is there
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/vms").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_to_json(response.into_body()).await;
        let vms = vms_of(&body);
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0]["id"], vm_id);
        assert_eq!(vms[0]["name"], "persistent-vm");

        // Get the specific VM
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/vms/{}", vm_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_to_json(response.into_body()).await;
        let vm = vm_of(&body);
        assert_eq!(vm["id"], vm_id);
        assert_eq!(vm["name"], "persistent-vm");
        assert_eq!(vm["vcpu_count"], 2);
        assert_eq!(vm["mem_size_mib"], 512);
    }
}

#[tokio::test]
async fn test_vm_delete_persists() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = db_path_from_temp_dir(&temp_dir);

    let vm_id: String;

    // Phase 1: Create and then delete a VM
    {
        let vm_manager = common::with_db_path(db_path.clone()).unwrap();
        vm_manager.initialize().await.unwrap();
        let app = create_router(vm_manager);

        // Create VM
        let create_request = json!({
            "name": "delete-persist-vm",
            "vcpu_count": 1,
            "mem_size_mib": 256,
            "kernel_image_path": "/path/to/kernel",
            "rootfs_path": "/path/to/rootfs.ext4"
        });

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/vms")
                    .header("content-type", "application/json")
                    .body(Body::from(create_request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = body_to_json(response.into_body()).await;
        vm_id = vm_of(&body)["id"].as_str().unwrap().to_string();

        // Delete VM
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/vms/{}", vm_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    // Phase 2: Verify VM is still deleted after restart
    {
        let vm_manager = common::with_db_path(db_path).unwrap();
        vm_manager.initialize().await.unwrap();
        let app = create_router(vm_manager);

        // List should be empty
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/vms").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let body = body_to_json(response.into_body()).await;
        assert!(vms_of(&body).is_empty(), "{body}");

        // Get should return not found
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/vms/{}", vm_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn test_multiple_vms_persist() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = db_path_from_temp_dir(&temp_dir);

    // Phase 1: Create multiple VMs
    {
        let vm_manager = common::with_db_path(db_path.clone()).unwrap();
        vm_manager.initialize().await.unwrap();
        let app = create_router(vm_manager);

        for i in 1..=3 {
            let create_request = json!({
                "name": format!("multi-vm-{}", i),
                "vcpu_count": i,
                "mem_size_mib": 256 * i,
                "kernel_image_path": "/path/to/kernel",
                "rootfs_path": "/path/to/rootfs.ext4"
            });

            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/vms")
                        .header("content-type", "application/json")
                        .body(Body::from(create_request.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::CREATED);
        }
    }

    // Phase 2: Verify all VMs are recovered
    {
        let vm_manager = common::with_db_path(db_path).unwrap();
        vm_manager.initialize().await.unwrap();
        let app = create_router(vm_manager);

        let response = app
            .oneshot(Request::builder().uri("/vms").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_to_json(response.into_body()).await;
        let vms = vms_of(&body);
        assert_eq!(vms.len(), 3);

        // Verify names are present (order may vary)
        let names: Vec<&str> = vms.iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"multi-vm-1"));
        assert!(names.contains(&"multi-vm-2"));
        assert!(names.contains(&"multi-vm-3"));
    }
}

/// One server-sent event: `(event, data)`.
async fn next_sse(body: &mut Body, buf: &mut String) -> (String, Value) {
    loop {
        if let Some(end) = buf.find("\n\n") {
            let block: String = buf.drain(..end + 2).collect();
            let mut event = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push_str(v.trim_start());
                }
            }
            if event.is_empty() {
                continue; // keep-alive comment
            }
            return (event, serde_json::from_str(&data).unwrap_or(Value::Null));
        }
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), body.frame())
            .await
            .expect("an event within 10 s")
            .expect("stream open")
            .unwrap();
        if let Ok(bytes) = frame.into_data() {
            buf.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
}

/// `GET /watch` (spec/12-reconciliation.md §12.6): a snapshot, `synced`,
/// then changes as they happen.
#[tokio::test]
async fn test_watch_streams_changes() {
    let (app, _temp_dir) = create_test_app();
    let before = create_simple_vm(&app, "watch-before").await;

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/watch?kinds=vms")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let mut body = resp.into_body();
    let mut buf = String::new();

    let (ev, data) = next_sse(&mut body, &mut buf).await;
    assert_eq!(
        (ev.as_str(), data["kind"].as_str(), data["id"].as_str()),
        ("added", Some("vm"), Some(before.as_str())),
        "{data}"
    );
    assert_eq!(data["object"]["name"], "watch-before");
    assert_eq!(next_sse(&mut body, &mut buf).await.0, "synced");

    let id = create_simple_vm(&app, "watch-new").await;
    let (ev, data) = next_sse(&mut body, &mut buf).await;
    assert_eq!(
        (ev.as_str(), data["id"].as_str()),
        ("added", Some(id.as_str())),
        "{data}"
    );

    let (status, _) = send(
        &app,
        "POST",
        format!("/vms/{}/stop?graceful_timeout_secs=7", id),
        None,
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (ev, data) = next_sse(&mut body, &mut buf).await;
    assert_eq!(
        (ev.as_str(), data["id"].as_str()),
        ("modified", Some(id.as_str())),
        "{data}"
    );
    assert_eq!(data["object"]["stop_grace_secs"], 7);

    let (status, _) = send(&app, "DELETE", format!("/vms/{}", id), None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (ev, data) = next_sse(&mut body, &mut buf).await;
    assert_eq!(
        (ev.as_str(), data["id"].as_str()),
        ("deleted", Some(id.as_str())),
        "{data}"
    );
    assert!(data.get("object").is_none());

    // Unknown kinds are refused up front.
    let (status, _) = send(&app, "GET", "/watch?kinds=pods".into(), None, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The cluster kinds of `GET /watch` (spec/16-clustering-ui.md §3.7): nodes and
/// the policy marker come on request; a standalone host has no cluster
/// summary or import plans to send.
#[tokio::test]
async fn test_watch_streams_nodes_and_the_policy_marker() {
    let dir = TempDir::new().unwrap();
    let manager = common::with_db_path(dir.path().join("glidex.db")).unwrap();
    manager.initialize().await.unwrap();
    let app = create_router(manager);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/watch?kinds=nodes,imports,cluster,policy")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    let mut buf = String::new();
    let mut node = None;
    let mut policy = false;
    loop {
        let (ev, data) = next_sse(&mut body, &mut buf).await;
        if ev == "synced" {
            break;
        }
        assert_eq!(ev, "added", "{data}");
        match data["kind"].as_str() {
            Some("node") => node = data["id"].as_str().map(String::from),
            Some("policy") => policy = data["object"]["generation"].is_u64(),
            other => panic!("unexpected kind {other:?} on a standalone host: {data}"),
        }
    }
    let node = node.expect("this host's node");
    assert!(policy, "the policy marker");

    let (status, v) = send(&app, "POST", format!("/nodes/{node}/drain"), None, &[]).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let (ev, data) = next_sse(&mut body, &mut buf).await;
    assert_eq!(
        (ev.as_str(), data["kind"].as_str(), data["id"].as_str()),
        ("modified", Some("node"), Some(node.as_str())),
        "{data}"
    );
    assert_eq!(
        data["object"]["status"]["phase"], "NODE_PHASE_DRAINING",
        "{data}"
    );
    assert_eq!(data["object"]["meta"]["id"], node.as_str(), "{data}");
}

/// The next message of a Connect server stream (`application/connect+json`):
/// a flag byte, a 4-byte length, then JSON. `None` at the end-of-stream frame.
async fn next_frame(body: &mut Body, buf: &mut Vec<u8>) -> Option<Value> {
    use http_body_util::BodyExt;
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
        let frame = tokio::time::timeout(std::time::Duration::from_secs(10), body.frame())
            .await
            .expect("a frame within 10 s")
            .expect("the stream is open")
            .unwrap();
        if let Ok(data) = frame.into_data() {
            buf.extend_from_slice(&data);
        }
    }
}

/// `glidex.watch.v1.WatchService.Watch` (spec/18-grpc.md G4): the same events
/// as `GET /watch`, as a server stream of `WatchResponse` messages.
#[tokio::test]
async fn test_watch_over_rpc_streams_the_same_events() {
    let (app, _temp_dir) = create_test_app();
    let before = create_simple_vm(&app, "rpc-watch-before").await;
    let mut req = vec![0u8];
    let msg = br#"{"kinds":["WATCH_KIND_VM"]}"#;
    req.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    req.extend_from_slice(msg);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/glidex.watch.v1.WatchService/Watch")
                .header("content-type", "application/connect+json")
                .body(Body::from(req))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    let mut buf = Vec::new();
    let e = next_frame(&mut body, &mut buf).await.expect("an event");
    assert_eq!(
        (e["type"].as_str(), e["kind"].as_str(), e["id"].as_str()),
        (
            Some("WATCH_EVENT_TYPE_ADDED"),
            Some("WATCH_KIND_VM"),
            Some(before.as_str())
        ),
        "{e}"
    );
    assert_eq!(e["vm"]["name"], "rpc-watch-before", "the Vm message: {e}");
    let e = next_frame(&mut body, &mut buf).await.expect("synced");
    assert_eq!(e["type"], "WATCH_EVENT_TYPE_SYNCED", "{e}");

    let id = create_simple_vm(&app, "rpc-watch-new").await;
    let e = next_frame(&mut body, &mut buf).await.expect("an event");
    assert_eq!(
        (e["type"].as_str(), e["id"].as_str()),
        (Some("WATCH_EVENT_TYPE_ADDED"), Some(id.as_str())),
        "{e}"
    );
    let (status, _) = send(&app, "DELETE", format!("/vms/{}", id), None, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let e = next_frame(&mut body, &mut buf).await.expect("an event");
    assert_eq!(
        (e["type"].as_str(), e["id"].as_str()),
        (Some("WATCH_EVENT_TYPE_DELETED"), Some(id.as_str())),
        "{e}"
    );
    assert!(e.get("vm").is_none(), "no object on delete: {e}");
}

// ---- nodes (spec/14-clustering.md C1) ------------------------------------------

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_standalone_host_is_one_node_and_places_vms_on_it() {
    let dir = TempDir::new().unwrap();
    let manager = common::with_db_path(dir.path().join("glidex.db")).unwrap();
    manager.initialize().await.unwrap();
    let app = create_router(manager);

    let (status, nodes) = call(&app, "GET", "/nodes", None).await;
    assert_eq!(status, StatusCode::OK);
    let nodes = nodes["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1, "{nodes:?}");
    assert_eq!(nodes[0]["meta"]["id"], "local");
    assert_eq!(nodes[0]["status"]["phase"], "NODE_PHASE_ACTIVE");
    assert!(nodes[0]["status"]["capacity"]["cpus"].as_u64().unwrap() >= 1);
    // A uint64 is a JSON string (proto3 JSON).
    assert!(
        nodes[0]["status"]["capacity"]["memory_mib"].is_string(),
        "{nodes:?}"
    );
    assert!(
        nodes[0]["meta"]["resource_version"].is_string(),
        "{nodes:?}"
    );

    let (status, one) = call(&app, "GET", "/nodes/local", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["node"]["spec"]["role"], "NODE_ROLE_SERVER", "{one}");
    let (status, _) = call(&app, "GET", "/nodes/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, vm) = call(
        &app,
        "POST",
        "/vms",
        Some(json!({ "name": "placed", "vcpu_count": 1, "mem_size_mib": 256, "hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR",
                     "firmware_path": "/f.fd", "rootfs_path": "/r.raw" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    assert_eq!(vm_of(&vm)["node"], "local");
}

/// Kernel boot admission (spec/23-kernel-boot.md K0): one boot mode, an
/// initrd only with a kernel, `kernel_args` limits at create and PATCH.
#[tokio::test]
async fn test_kernel_boot_admission() {
    let (app, _temp_dir) = create_test_app();
    let create = |extra: Value| {
        let mut body = json!({
            "name": "kb", "vcpu_count": 1, "mem_size_mib": 256,
            "rootfs_path": "/path/to/rootfs.ext4"
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        send(&app, "POST", "/vms".into(), Some(body), &[])
    };
    for (extra, message) in [
        (
            json!({"kernel_image_path": "/k", "firmware_path": "/fw.fd"}),
            "not both",
        ),
        (
            json!({"initrd_path": "/i", "firmware_path": "/fw.fd"}),
            "initrd_path",
        ),
        (
            json!({"kernel_image_path": "/k", "kernel_args": "a".repeat(2048)}),
            "2047",
        ),
        (
            json!({"kernel_image_path": "/k", "kernel_args": "root=/dev/vda\ninit=/bin/sh"}),
            "control",
        ),
        (
            json!({"kernel_image_path": "/k", "initrd_path": "/i\"x"}),
            "initrd_path",
        ),
    ] {
        let (status, body) = create(extra.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{extra}: {body}");
        assert_eq!(body["error"], "invalid_config", "{extra}: {body}");
        assert!(
            body["message"].as_str().unwrap_or("").contains(message),
            "{extra}: {body}"
        );
    }

    let (status, body) = create(json!({
        "kernel_image_path": "/k", "initrd_path": "/i",
        "kernel_args": "a".repeat(2047)
    }))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = vm_of(&body)["id"].as_str().unwrap().to_string();

    let (status, body) = send(
        &app,
        "PATCH",
        format!("/vms/{}", id),
        Some(json!({"change": {"kernel_args": "a".repeat(2048)}})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_config");
    let (status, body) = send(
        &app,
        "PATCH",
        format!("/vms/{}", id),
        Some(json!({"change": {}, "update_mask": "initrd_path"})),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_config");
}
