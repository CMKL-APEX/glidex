//! Functional tests for firmware (UEFI) boot and cloud-init seed generation.
//!
//! Two tiers:
//!
//! - Default tests exercise the API validation, persistence and the seed
//!   image builder. They need `mkdosfs`/`mcopy`/`mdir` (dosfstools + mtools)
//!   but no hypervisor.
//!
//! - `#[ignore]`d tests boot a real guest with cloud-hypervisor and need
//!   `/dev/kvm`, `cloud-hypervisor`, `openssl` and a UEFI-bootable disk
//!   image. The firmware defaults to `~/.glidex/CLOUDHV.fd` as downloaded
//!   by `glidex-install` (override with `GLIDEX_TEST_FIRMWARE`):
//!
//!   ```sh
//!   GLIDEX_TEST_IMAGE=~/ch/resolute-server-cloudimg-amd64.raw \
//!     cargo test -p glidex-control-plane --test functional_tests -- --ignored --nocapture
//!   ```
//!
//!   The image is copied into a temp dir first, so the original is never
//!   modified.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tower::ServiceExt;

use glidex_control_plane::api::create_router;
use glidex_control_plane::cloud_init::{self, SeedConfig};
use glidex_control_plane::hypervisor::cloud_hypervisor::default_firmware_path;
use glidex_control_plane::state::VmManager;

// ============================================================================
// Helpers
// ============================================================================

fn create_test_app() -> (Router, Arc<VmManager>, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let manager = VmManager::with_db_path(temp_dir.path().join("test.db")).unwrap();
    (create_router(manager.clone()), manager, temp_dir)
}

async fn request(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
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
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

fn firmware_vm(name: &str) -> Value {
    json!({
        "name": name,
        "vcpu_count": 1,
        "mem_size_mib": 512,
        "hypervisor": "cloudhypervisor",
        "firmware_path": "/path/to/CLOUDHV.fd",
        "rootfs_path": "/path/to/disk.raw"
    })
}

fn run_ok(cmd: &mut Command) -> String {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to run {:?}: {}", cmd.get_program(), e));
    assert!(
        out.status.success(),
        "{:?} failed: {}",
        cmd.get_program(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn mtype(image: &Path, file: &str) -> String {
    run_ok(Command::new("mtype").arg("-i").arg(image).arg(format!("::{}", file)))
}

// ============================================================================
// API validation and persistence for firmware boot
// ============================================================================

#[tokio::test]
async fn firmware_vm_can_be_created_without_kernel() {
    let (app, manager, _tmp) = create_test_app();

    let (status, body) = request(&app, "POST", "/vms", Some(firmware_vm("fw-vm"))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["hypervisor"], "cloudhypervisor");
    assert_eq!(body["state"], "created");

    let vm = manager.get_vm(body["id"].as_str().unwrap()).await.unwrap();
    assert_eq!(vm.config.firmware_path.as_deref(), Some("/path/to/CLOUDHV.fd"));
    assert_eq!(vm.config.kernel_image_path, "");
    assert_eq!(vm.config.cloud_init_path, None);
}

#[tokio::test]
async fn firmware_path_rejected_for_other_hypervisors() {
    let (app, _manager, _tmp) = create_test_app();

    let mut req = firmware_vm("fw-qemu");
    req["hypervisor"] = json!("qemu");
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_config");
    assert!(
        body["message"].as_str().unwrap_or_default().contains("firmware_path"),
        "{body}"
    );

    let (_, list) = request(&app, "GET", "/vms", None).await;
    assert_eq!(list.as_array().unwrap().len(), 0, "rejected VMs must not persist");
}

#[tokio::test]
async fn kernel_or_firmware_is_required() {
    let (app, _manager, _tmp) = create_test_app();

    let mut req = firmware_vm("no-boot");
    req.as_object_mut().unwrap().remove("firmware_path");
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap_or_default().contains("kernel_image_path"));
}

#[tokio::test]
async fn cloud_init_path_rejected_for_other_hypervisors() {
    let (app, _manager, _tmp) = create_test_app();

    let req = json!({
        "name": "qemu-ci",
        "vcpu_count": 1,
        "mem_size_mib": 512,
        "hypervisor": "qemu",
        "kernel_image_path": "/path/to/vmlinux",
        "rootfs_path": "/path/to/rootfs",
        "cloud_init_path": "/path/to/seed.img"
    });
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["message"].as_str().unwrap_or_default().contains("cloud_init_path"));
}

#[tokio::test]
async fn zero_vcpus_is_a_bad_request() {
    let (app, _manager, _tmp) = create_test_app();

    let mut req = firmware_vm("zero-cpu");
    req["vcpu_count"] = json!(0);
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "invalid_config");
}

#[tokio::test]
async fn firecracker_is_no_longer_a_hypervisor() {
    let (app, _manager, _tmp) = create_test_app();

    let req = json!({
        "name": "fc",
        "vcpu_count": 1,
        "mem_size_mib": 512,
        "hypervisor": "firecracker",
        "kernel_image_path": "/path/to/vmlinux",
        "rootfs_path": "/path/to/rootfs"
    });
    let (status, _) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// A database written by an older build may hold Firecracker VMs. They must
/// not stop the control plane from starting or hide the other VMs.
#[tokio::test]
async fn legacy_firecracker_records_are_skipped_on_load() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("test.db");

    let (keep_id, legacy_id) = {
        let manager = VmManager::with_db_path(db.clone()).unwrap();
        manager.initialize().await.unwrap();
        let app = create_router(manager);
        let (_, keep) = request(&app, "POST", "/vms", Some(firmware_vm("keep"))).await;
        let (_, legacy) = request(&app, "POST", "/vms", Some(firmware_vm("legacy"))).await;
        (
            keep["id"].as_str().unwrap().to_string(),
            legacy["id"].as_str().unwrap().to_string(),
        )
    };

    // Rewrite one record the way an older build would have stored it.
    {
        use redb::{Database, ReadableDatabase, TableDefinition};
        const VMS: TableDefinition<&str, &[u8]> = TableDefinition::new("vms");
        let database = Database::create(&db).unwrap();
        let mut record: Value = {
            let txn = database.begin_read().unwrap();
            let table = txn.open_table(VMS).unwrap();
            let bytes = table.get(legacy_id.as_str()).unwrap().unwrap();
            serde_json::from_slice(bytes.value()).unwrap()
        };
        record["hypervisor"] = json!("firecracker");
        record["config"]["hypervisor"] = json!("firecracker");
        let txn = database.begin_write().unwrap();
        {
            let mut table = txn.open_table(VMS).unwrap();
            let bytes = serde_json::to_vec(&record).unwrap();
            table.insert(legacy_id.as_str(), bytes.as_slice()).unwrap();
        }
        txn.commit().unwrap();
    }

    let manager = VmManager::with_db_path(db).unwrap();
    manager.initialize().await.unwrap();
    assert!(manager.get_vm(&keep_id).await.is_ok());
    assert!(manager.get_vm(&legacy_id).await.is_err());
    assert_eq!(manager.list_vms().await.len(), 1);
}

#[tokio::test]
async fn hypervisor_defaults_to_cloud_hypervisor() {
    let (app, _manager, _tmp) = create_test_app();

    let mut req = firmware_vm("default-hv");
    req.as_object_mut().unwrap().remove("hypervisor");
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["hypervisor"], "cloudhypervisor");
}

#[tokio::test]
async fn firmware_and_cloud_init_paths_are_tilde_expanded() {
    let (app, manager, _tmp) = create_test_app();
    let home = dirs::home_dir().unwrap();

    let mut req = firmware_vm("tilde");
    req["firmware_path"] = json!("~/fw/CLOUDHV.fd");
    req["cloud_init_path"] = json!("~/seed.img");
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let vm = manager.get_vm(body["id"].as_str().unwrap()).await.unwrap();
    assert_eq!(
        vm.config.firmware_path.map(PathBuf::from),
        Some(home.join("fw/CLOUDHV.fd"))
    );
    assert_eq!(
        vm.config.cloud_init_path.map(PathBuf::from),
        Some(home.join("seed.img"))
    );
}

#[tokio::test]
async fn firmware_config_survives_restart() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("test.db");

    let id = {
        let manager = VmManager::with_db_path(db.clone()).unwrap();
        manager.initialize().await.unwrap();
        let app = create_router(manager);
        let (status, body) = request(&app, "POST", "/vms", Some(firmware_vm("persist"))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body["id"].as_str().unwrap().to_string()
    };

    let manager = VmManager::with_db_path(db).unwrap();
    manager.initialize().await.unwrap();
    let vm = manager.get_vm(&id).await.unwrap();
    assert_eq!(vm.config.firmware_path.as_deref(), Some("/path/to/CLOUDHV.fd"));
    assert_eq!(vm.default_cloud_init_path(), format!("/tmp/cloud-hypervisor-{id}.cloudinit.img"));
}

// ============================================================================
// cloud-init seed image
// ============================================================================

#[test]
fn seed_image_is_a_cidata_volume_with_nocloud_files() {
    let tmp = TempDir::new().unwrap();
    let image = tmp.path().join("seed.img");
    let seed = SeedConfig {
        instance_id: "vm-1234".into(),
        hostname: "func-test".into(),
        ssh_authorized_keys: vec!["ssh-ed25519 AAAAC3Nza test@host".into()],
        passwd_hash: Some("$6$salt$hash".into()),
    };

    cloud_init::write_seed_image(image.to_str().unwrap(), &seed).unwrap();

    let listing = run_ok(Command::new("mdir").arg("-i").arg(&image).arg("::"));
    assert!(listing.contains("CIDATA"), "volume label missing:\n{listing}");

    assert_eq!(mtype(&image, "meta-data"), seed.meta_data());
    assert_eq!(mtype(&image, "user-data"), seed.user_data());
    assert_eq!(mtype(&image, "network-config"), seed.network_config());

    let user_data = mtype(&image, "user-data");
    assert!(user_data.starts_with("#cloud-config\n"));
    assert!(user_data.contains("- 'ssh-ed25519 AAAAC3Nza test@host'"));
    assert!(user_data.contains("type: hash"));

    // Staging files are cleaned up; only the image remains.
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, vec![std::ffi::OsString::from("seed.img")]);
}

#[test]
fn seed_image_is_replaced_on_regeneration() {
    let tmp = TempDir::new().unwrap();
    let image = tmp.path().join("seed.img");
    let path = image.to_str().unwrap();

    let mut seed = SeedConfig {
        instance_id: "first".into(),
        hostname: "first".into(),
        ..Default::default()
    };
    cloud_init::write_seed_image(path, &seed).unwrap();
    seed.instance_id = "second".into();
    cloud_init::write_seed_image(path, &seed).unwrap();

    assert!(mtype(&image, "meta-data").contains("instance-id: 'second'"));
}

#[test]
fn seed_image_error_leaves_no_partial_files() {
    // Parent "directory" is a regular file, so staging cannot be created.
    let tmp = TempDir::new().unwrap();
    let not_a_dir = tmp.path().join("file");
    std::fs::write(&not_a_dir, b"").unwrap();
    let image = not_a_dir.join("seed.img");

    let err = cloud_init::write_seed_image(image.to_str().unwrap(), &SeedConfig::default())
        .unwrap_err();
    assert!(err.to_string().contains("cloud-init seed"), "{err}");
    let entries: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
    assert_eq!(entries.len(), 1, "only the pre-existing file should remain");
}

// ============================================================================
// End-to-end boot (ignored by default; needs KVM + images)
// ============================================================================

/// Stops every VM the manager knows about when dropped, so a failing test
/// never leaves a cloud-hypervisor process behind.
struct ShutdownGuard(Arc<VmManager>);

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        let manager = self.0.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(manager.shutdown())
        });
    }
}

/// Minimal expect-style driver for the VM console socket.
struct Console {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl Console {
    fn connect(path: &str) -> Self {
        let stream = UnixStream::connect(path)
            .unwrap_or_else(|e| panic!("connect console {path}: {e}"));
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        Self { stream, buf: Vec::new() }
    }

    /// Wait until `needle` appears in output received since the last match,
    /// then consume everything up to and including it.
    fn expect(&mut self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(pos) = find(&self.buf, needle.as_bytes()) {
                self.buf.drain(..pos + needle.len());
                return;
            }
            if Instant::now() > deadline {
                let tail = String::from_utf8_lossy(&self.buf);
                let tail: String = tail.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect();
                panic!("timed out waiting for {needle:?}; last output:\n{tail}");
            }
            match self.stream.read(&mut chunk) {
                Ok(0) => std::thread::sleep(Duration::from_millis(100)),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("console read failed: {e}"),
            }
        }
    }

    /// Type slowly: the emulated 16550 UART has a small receive FIFO.
    fn send(&mut self, text: &str) {
        for b in text.bytes() {
            self.stream.write_all(&[b]).unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn env_path(name: &str, default: Option<String>) -> PathBuf {
    let value = std::env::var(name).ok().or(default).unwrap_or_else(|| {
        panic!("{name} must be set to run the end-to-end boot tests (see module docs)")
    });
    let path = match value.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap().join(rest),
        None => PathBuf::from(value),
    };
    assert!(
        path.exists(),
        "{name}={} does not exist (glidex-install downloads the firmware)",
        path.display()
    );
    path.canonicalize().unwrap()
}

/// Throwaway random password and its SHA-512 crypt hash.
fn throwaway_credentials() -> (String, String) {
    let password = uuid::Uuid::new_v4().simple().to_string();
    let mut child = Command::new("openssl")
        .args(["passwd", "-6", "-stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("openssl is required for the boot test");
    child.stdin.take().unwrap().write_all(password.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "openssl passwd failed");
    (password, String::from_utf8(out.stdout).unwrap().trim().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, cloud-hypervisor and GLIDEX_TEST_IMAGE"]
async fn firmware_boot_with_generated_cloud_init() {
    let firmware = env_path(
        "GLIDEX_TEST_FIRMWARE",
        default_firmware_path().map(|p| p.to_string_lossy().into_owned()),
    );
    let source_image = env_path("GLIDEX_TEST_IMAGE", None);
    assert!(Path::new("/dev/kvm").exists(), "/dev/kvm is required");

    let (app, manager, tmp) = create_test_app();
    let _guard = ShutdownGuard(manager.clone());

    // Work on a sparse copy so the source image is never modified.
    let rootfs = tmp.path().join("rootfs.raw");
    run_ok(Command::new("cp").arg("--sparse=always").arg(&source_image).arg(&rootfs));

    let (password, hash) = throwaway_credentials();
    std::env::set_var(cloud_init::PASSWD_HASH_ENV, &hash);

    let hostname = "gx-functest";
    let (status, vm) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({
            "name": hostname,
            "vcpu_count": 2,
            "mem_size_mib": 2048,
            "hypervisor": "cloudhypervisor",
            "firmware_path": firmware,
            "rootfs_path": rootfs,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    let id = vm["id"].as_str().unwrap().to_string();
    let seed_path = manager.get_vm(&id).await.unwrap().default_cloud_init_path();

    // Start: seed is generated and the VM reaches Running.
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/start"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "running");
    assert!(Path::new(&seed_path).exists(), "seed image not generated");
    let persisted = manager.get_vm(&id).await.unwrap();
    assert_eq!(persisted.config.cloud_init_path, None, "generated seed must not be persisted");

    let (status, info) = request(&app, "GET", &format!("/vms/{id}/console"), None).await;
    assert_eq!(status, StatusCode::OK, "{info}");
    let console_path = info["console_socket_path"].as_str().unwrap().to_string();

    // Boot via UEFI firmware on the serial console, provisioned by cloud-init.
    let mut console = tokio::task::spawn_blocking(move || {
        let mut console = Console::connect(&console_path);
        console.expect("Cloud-init v.", Duration::from_secs(300));
        console.expect("finished", Duration::from_secs(60));
        std::thread::sleep(Duration::from_secs(2));
        console.send("\r");
        console.expect(&format!("{hostname} login: "), Duration::from_secs(60));
        console.send("cloud\r");
        console.expect("Password: ", Duration::from_secs(30));
        console.send(&format!("{password}\r"));
        console.expect(&format!("cloud@{hostname}:~$ "), Duration::from_secs(60));
        console.send("echo GX_$((6*7))_$(id -un)_$(sudo -n true && echo root)\r");
        console.expect("GX_42_cloud_root", Duration::from_secs(30));
        console
    })
    .await
    .unwrap();

    // Pause / resume keeps the guest alive.
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/pause"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "paused");
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/start"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "running");
    console = tokio::task::spawn_blocking(move || {
        console.send("echo GX_$((6*8))_resumed\r");
        console.expect("GX_48_resumed", Duration::from_secs(30));
        console
    })
    .await
    .unwrap();
    drop(console);

    // Stop, then delete cleans up the generated seed.
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/stop"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "stopped");

    let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!Path::new(&seed_path).exists(), "seed image not removed on delete");

    std::env::remove_var(cloud_init::PASSWD_HASH_ENV);
}
