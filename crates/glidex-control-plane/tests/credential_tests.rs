//! REST API tests for the credential store and its use by VMs.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

use glidex_control_plane::api::create_router;
use glidex_control_plane::cloud_init::SeedConfig;
use glidex_control_plane::state::VmManager;

fn create_test_app() -> (Router, Arc<VmManager>, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let manager = VmManager::with_db_path(temp_dir.path().join("test.db")).unwrap();
    (create_router(manager.clone()), manager, temp_dir)
}

async fn request(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, String) {
    let builder = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn json_of(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

const PASSWORD: &str = "correct-horse-battery";
const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample alice@host";

async fn create_alice(app: &Router) -> Value {
    let (status, body) = request(
        app,
        "POST",
        "/credentials",
        Some(json!({"username": "alice", "password": PASSWORD, "ssh_authorized_keys": [KEY]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    json_of(&body)
}

fn firmware_vm(name: &str, credential: Option<&str>) -> Value {
    let mut vm = json!({
        "name": name,
        "vcpu_count": 1,
        "mem_size_mib": 512,
        "hypervisor": "cloudhypervisor",
        "firmware_path": "/path/to/CLOUDHV.fd",
        "rootfs_path": "/path/to/disk.raw"
    });
    if let Some(c) = credential {
        vm["credential"] = json!(c);
    }
    vm
}

#[tokio::test]
async fn responses_never_contain_password_or_hash() {
    let (app, manager, _tmp) = create_test_app();

    let created = create_alice(&app).await;
    assert_eq!(created["username"], "alice");
    assert_eq!(created["has_password"], true);
    assert_eq!(created["ssh_authorized_keys"], json!([KEY]));

    for (method, uri) in [("GET", "/credentials"), ("GET", "/credentials/alice")] {
        let (status, body) = request(&app, method, uri, None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains(PASSWORD), "{uri} leaked the password");
        assert!(!body.contains("$6$"), "{uri} leaked the hash");
        assert!(!body.contains("password_hash"), "{uri}: {body}");
    }

    // The stored record does hold a SHA-512-crypt hash for cloud-init.
    let stored = manager.get_credential("alice").unwrap();
    assert!(stored.password_hash.unwrap().starts_with("$6$"));
}

#[tokio::test]
async fn credential_crud_status_codes() {
    let (app, _manager, _tmp) = create_test_app();
    create_alice(&app).await;

    let (status, body) = request(
        &app,
        "POST",
        "/credentials",
        Some(json!({"username": "alice", "password": PASSWORD})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    for bad in [
        json!({"username": "root", "password": PASSWORD}),
        json!({"username": "Bad Name", "password": PASSWORD}),
        json!({"username": "bob", "password": "short"}),
        json!({"username": "bob"}),
        json!({"username": "bob", "ssh_authorized_keys": ["not-a-key"]}),
    ] {
        let (status, body) = request(&app, "POST", "/credentials", Some(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert_eq!(json_of(&body)["error"], "invalid_credential");
    }

    let (status, body) = request(
        &app,
        "PUT",
        "/credentials/alice",
        Some(json!({"password": "another-good-password", "ssh_authorized_keys": []})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(json_of(&body)["ssh_authorized_keys"], json!([]));

    let (status, _) = request(&app, "GET", "/credentials/nobody-here", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = request(&app, "PUT", "/credentials/nobody-here", Some(json!({"password": PASSWORD}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = request(&app, "DELETE", "/credentials/alice", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "DELETE", "/credentials/alice", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn vm_credential_must_exist_and_needs_firmware_boot() {
    let (app, _manager, _tmp) = create_test_app();

    let (status, body) = request(&app, "POST", "/vms", Some(firmware_vm("vm1", Some("ghost")))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("credential not found"));

    create_alice(&app).await;

    let kernel_vm = json!({
        "name": "vm2", "vcpu_count": 1, "mem_size_mib": 512,
        "hypervisor": "cloudhypervisor",
        "kernel_image_path": "/path/to/vmlinux", "rootfs_path": "/path/to/rootfs",
        "credential": "alice"
    });
    let (status, body) = request(&app, "POST", "/vms", Some(kernel_vm)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let mut custom_seed = firmware_vm("vm3", Some("alice"));
    custom_seed["cloud_init_path"] = json!("/path/to/seed.img");
    let (status, body) = request(&app, "POST", "/vms", Some(custom_seed)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, body) = request(&app, "POST", "/vms", Some(firmware_vm("vm4", Some("alice")))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(json_of(&body)["credential"], "alice");
}

#[tokio::test]
async fn credential_in_use_cannot_be_deleted() {
    let (app, _manager, _tmp) = create_test_app();
    create_alice(&app).await;

    let (_, vm) = request(&app, "POST", "/vms", Some(firmware_vm("uses-alice", Some("alice")))).await;
    let vm_id = json_of(&vm)["id"].as_str().unwrap().to_string();

    let (status, body) = request(&app, "DELETE", "/credentials/alice", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("uses-alice"), "{body}");

    let (status, _) = request(&app, "DELETE", &format!("/vms/{vm_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "DELETE", "/credentials/alice", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn credentials_persist_across_restart() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("test.db");
    {
        let manager = VmManager::with_db_path(db.clone()).unwrap();
        create_alice(&create_router(manager)).await;
    }
    let manager = VmManager::with_db_path(db.clone()).unwrap();
    let cred = manager.get_credential("alice").unwrap();
    assert_eq!(cred.ssh_authorized_keys, vec![KEY.to_string()]);

    let mode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "database holds hashes and must be owner-only");
}

#[tokio::test]
async fn seed_uses_only_the_credential() {
    let (app, manager, _tmp) = create_test_app();
    create_alice(&app).await;
    let cred = manager.get_credential("alice").unwrap();
    let hash = cred.password_hash.clone().unwrap();

    let seed = SeedConfig::for_credential("vm-id", "My VM", &cred);
    let user_data = seed.user_data();
    assert!(user_data.contains("  - name: alice\n"), "{user_data}");
    assert!(user_data.contains(&format!("passwd: '{hash}'")));
    assert!(user_data.contains("    - name: alice\n      password: "));
    assert!(user_data.contains(&format!("- '{KEY}'")));
    assert!(!user_data.contains("name: cloud"));
    assert_eq!(user_data.matches("ssh-").count(), 1, "host keys must not be added");
    assert!(!format!("{seed:?}").contains("$6$"), "Debug must redact the hash");
}

#[test]
fn seed_image_is_owner_only() {
    let tmp = TempDir::new().unwrap();
    let image = tmp.path().join("seed.img");
    let seed = SeedConfig {
        instance_id: "i".into(),
        hostname: "h".into(),
        passwd_hash: Some("$6$salt$hash".into()),
        ..Default::default()
    };
    glidex_control_plane::cloud_init::write_seed_image(image.to_str().unwrap(), &seed).unwrap();
    let mode = std::fs::metadata(&image).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}
