use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

use glidex_control_plane::api::create_router;
use glidex_control_plane::state::VmManager;

/// Helper to create a test app instance with a temporary database
fn create_test_app() -> (axum::Router, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.db");
    let vm_manager = VmManager::with_db_path(db_path).unwrap();
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
        .oneshot(
            Request::builder()
                .uri("/vms")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = body_to_json(response.into_body()).await;
    assert!(body.is_array());
    assert_eq!(body.as_array().unwrap().len(), 0);
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
    assert_eq!(body["name"], "test-vm");
    assert_eq!(body["vcpu_count"], 2);
    assert_eq!(body["mem_size_mib"], 512);
    assert_eq!(body["state"], "created");
    assert!(body["id"].is_string());
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
    assert_eq!(body["name"], "test-vm-full");
    assert_eq!(body["vcpu_count"], 4);
    assert_eq!(body["mem_size_mib"], 1024);
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

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
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
    let vm_id = created_vm["id"].as_str().unwrap();

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
    assert_eq!(body["id"], vm_id);
    assert_eq!(body["name"], "get-test-vm");
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
    let vm_id = created_vm["id"].as_str().unwrap();

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
        .oneshot(
            Request::builder()
                .uri("/vms")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = body_to_json(response.into_body()).await;
    assert!(body.is_array());
    assert_eq!(body.as_array().unwrap().len(), 2);
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
    body_to_json(response.into_body()).await["id"].as_str().unwrap().to_string()
}

async fn send(app: &axum::Router, method: &str, uri: String, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = match body {
        Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Stopping a stopped VM writes nothing: the spec already says stopped
/// (spec/reconciliation.md §9.2).
#[tokio::test]
async fn test_stop_of_a_stopped_vm_is_a_no_op() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "stop-test-vm").await;
    let (status, body) = send(&app, "POST", format!("/vms/{}/stop", vm_id), None, &[]).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["desired_state"], "stopped");
    assert_eq!(body["state"], "created");
    assert_eq!(body["generation"], 1, "nothing changed");
}

#[tokio::test]
async fn test_graceful_stop_records_the_grace() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "graceful-vm").await;

    let (status, body) = send(&app, "POST", format!("/vms/{}/stop?graceful_timeout_secs=5", vm_id), None, &[]).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!((body["stop_grace_secs"].as_u64(), body["generation"].as_u64()), (Some(5), Some(2)));

    let (status, _) = send(&app, "POST", "/vms/nonexistent-id/stop?graceful_timeout_secs=5".into(), None, &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&app, "POST", format!("/vms/{}/stop?graceful_timeout_secs=soon", vm_id), None, &[]).await;
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
    assert_eq!(body["desired_state"], "paused");
    assert_eq!(body["generation"], 2);
    assert_eq!(body["observed_generation"], 0, "no controller ran");
}

#[tokio::test]
async fn test_patch_edits_spec_and_refuses_immutable_fields() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "patch-vm").await;
    let (status, body) = send(&app, "PATCH", format!("/vms/{}", vm_id), Some(json!({"config": {"vcpu_count": 4}, "restart_policy": "never"})), &[]).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!((body["vcpu_count"].as_u64(), body["restart_policy"].as_str()), (Some(4), Some("never")));

    let (status, body) = send(&app, "PATCH", format!("/vms/{}", vm_id), Some(json!({"config": {"hypervisor": "qemu"}})), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_config");
    let (status, _) = send(&app, "PATCH", format!("/vms/{}", vm_id), Some(json!({"bogus": 1})), &[]).await;
    assert!(status.is_client_error());
    let (status, body) = send(&app, "PATCH", format!("/vms/{}", vm_id), Some(json!({"stop_grace_secs": 301})), &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn test_if_match_guards_writes() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "match-vm").await;
    let (status, body) = send(&app, "POST", format!("/vms/{}/pause", vm_id), None, &[("if-match", "999")]).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED, "{body}");
    assert_eq!(body["error"], "precondition_failed");
    let (status, _) = send(&app, "POST", format!("/vms/{}/pause", vm_id), None, &[("if-match", "1")]).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_events_record_spec_writes() {
    let (app, _temp_dir) = create_test_app();
    let vm_id = create_simple_vm(&app, "events-vm").await;
    send(&app, "POST", format!("/vms/{}/start", vm_id), None, &[]).await;
    let (status, body) = send(&app, "GET", format!("/vms/{}/events", vm_id), None, &[]).await;
    assert_eq!(status, StatusCode::OK);
    let reasons: Vec<&str> = body["events"].as_array().unwrap().iter().map(|e| e["reason"].as_str().unwrap()).collect();
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
    let vm_id = created_vm["id"].as_str().unwrap();

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
    // paths are not exposed (spec/security.md §9).
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
        let vm_manager = VmManager::with_db_path(db_path.clone()).unwrap();
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
        vm_id = body["id"].as_str().unwrap().to_string();
    }
    // First manager is dropped here (simulates restart)

    // Phase 2: Create a new manager and verify VM is recovered
    {
        let vm_manager = VmManager::with_db_path(db_path).unwrap();
        vm_manager.initialize().await.unwrap();
        let app = create_router(vm_manager);

        // List VMs and verify our VM is there
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/vms")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_to_json(response.into_body()).await;
        let vms = body.as_array().unwrap();
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
        assert_eq!(body["id"], vm_id);
        assert_eq!(body["name"], "persistent-vm");
        assert_eq!(body["vcpu_count"], 2);
        assert_eq!(body["mem_size_mib"], 512);
    }
}

#[tokio::test]
async fn test_vm_delete_persists() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = db_path_from_temp_dir(&temp_dir);

    let vm_id: String;

    // Phase 1: Create and then delete a VM
    {
        let vm_manager = VmManager::with_db_path(db_path.clone()).unwrap();
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
        vm_id = body["id"].as_str().unwrap().to_string();

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
        let vm_manager = VmManager::with_db_path(db_path).unwrap();
        vm_manager.initialize().await.unwrap();
        let app = create_router(vm_manager);

        // List should be empty
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/vms")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = body_to_json(response.into_body()).await;
        assert!(body.as_array().unwrap().is_empty());

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
        let vm_manager = VmManager::with_db_path(db_path.clone()).unwrap();
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
        let vm_manager = VmManager::with_db_path(db_path).unwrap();
        vm_manager.initialize().await.unwrap();
        let app = create_router(vm_manager);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/vms")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_to_json(response.into_body()).await;
        let vms = body.as_array().unwrap();
        assert_eq!(vms.len(), 3);

        // Verify names are present (order may vary)
        let names: Vec<&str> = vms.iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"multi-vm-1"));
        assert!(names.contains(&"multi-vm-2"));
        assert!(names.contains(&"multi-vm-3"));
    }
}
