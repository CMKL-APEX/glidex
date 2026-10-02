//! Functional tests for firmware (UEFI) boot and cloud-init seed generation.
//!
//! Two tiers:
//!
//! - Default tests exercise the API validation, persistence and the seed
//!   image builder. They need `mkdosfs`/`mcopy`/`mdir` (dosfstools + mtools)
//!   but no hypervisor.
//!
//! - `#[ignore]`d tests boot a real guest with cloud-hypervisor (or QEMU,
//!   the `qemu_*` variants) and need `/dev/kvm`, the hypervisor and a
//!   UEFI-bootable disk image. The firmware defaults to
//!   `~/.glidex/CLOUDHV.fd` as downloaded by `glidex-install` (override with
//!   `GLIDEX_TEST_FIRMWARE`), or the host's OVMF for QEMU (override with
//!   `GLIDEX_TEST_QEMU_FIRMWARE`):
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
use glidex_control_plane::hypervisor::HypervisorType;
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
async fn qemu_firmware_vm_can_be_created_without_kernel() {
    let (app, manager, _tmp) = create_test_app();

    let mut req = firmware_vm("fw-qemu");
    req["hypervisor"] = json!("qemu");
    req["firmware_path"] = json!("/usr/share/OVMF/OVMF_CODE_4M.fd");
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["hypervisor"], "qemu");

    let vm = manager.get_vm(body["id"].as_str().unwrap()).await.unwrap();
    assert_eq!(vm.config.firmware_path.as_deref(), Some("/usr/share/OVMF/OVMF_CODE_4M.fd"));
    assert_eq!(vm.config.kernel_image_path, "");
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
async fn qemu_accepts_a_custom_cloud_init_path() {
    let (app, manager, _tmp) = create_test_app();

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
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let vm = manager.get_vm(body["id"].as_str().unwrap()).await.unwrap();
    assert_eq!(vm.config.cloud_init_path.as_deref(), Some("/path/to/seed.img"));
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
    // Per-VM private runtime directory (spec/security.md §9).
    assert_eq!(
        vm.default_cloud_init_path(),
        glidex_control_plane::paths::vm_dir(&id).join("cloudinit.img").to_string_lossy()
    );
    assert!(vm.socket_path.ends_with(&format!("/vms/{id}/api.sock")), "{}", vm.socket_path);
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
        username: "func-user".into(),
        ssh_authorized_keys: vec!["ssh-ed25519 AAAAC3Nza test@host".into()],
        passwd_hash: Some("$6$salt$hash".into()),
        nic_macs: Vec::new(),
        growpart: false,
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

/// The UEFI firmware the boot tests use for `hypervisor`.
fn test_firmware(hypervisor: &str) -> PathBuf {
    match hypervisor {
        "qemu" => env_path(
            "GLIDEX_TEST_QEMU_FIRMWARE",
            HypervisorType::Qemu.default_firmware_path().map(|p| p.to_string_lossy().into_owned()),
        ),
        _ => env_path(
            "GLIDEX_TEST_FIRMWARE",
            default_firmware_path().map(|p| p.to_string_lossy().into_owned()),
        ),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, cloud-hypervisor and GLIDEX_TEST_IMAGE"]
async fn firmware_boot_with_generated_cloud_init() {
    firmware_boot_e2e("cloudhypervisor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, qemu-system-x86_64, OVMF and GLIDEX_TEST_IMAGE"]
async fn qemu_firmware_boot_with_generated_cloud_init() {
    firmware_boot_e2e("qemu").await;
}

/// Boot a cloud image through UEFI with a generated seed and a stored
/// credential, log in, pause/resume, then shut the guest down gracefully.
async fn firmware_boot_e2e(hypervisor: &str) {
    let firmware = test_firmware(hypervisor);
    let source_image = env_path("GLIDEX_TEST_IMAGE", None);
    assert!(Path::new("/dev/kvm").exists(), "/dev/kvm is required");

    let (app, manager, tmp) = create_test_app();
    let _guard = ShutdownGuard(manager.clone());

    // Work on a sparse copy so the source image is never modified.
    let rootfs = tmp.path().join("rootfs.raw");
    run_ok(Command::new("cp").arg("--sparse=always").arg(&source_image).arg(&rootfs));

    // Guest login from the credential store, with a throwaway password.
    let username = "gxtester";
    let password = uuid::Uuid::new_v4().simple().to_string();
    let (status, body) = request(
        &app,
        "POST",
        "/credentials",
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let hostname = "gx-functest";
    let (status, vm) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({
            "name": hostname,
            "vcpu_count": 2,
            "mem_size_mib": 2048,
            "hypervisor": hypervisor,
            "firmware_path": firmware,
            "rootfs_path": rootfs,
            "credential": username,
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

    let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;

    // Boot via UEFI firmware on the serial console, provisioned by cloud-init.
    let mut console = tokio::task::spawn_blocking(move || {
        let mut console = Console::connect(&console_path);
        console.expect("Cloud-init v.", Duration::from_secs(300));
        console.expect("finished", Duration::from_secs(60));
        std::thread::sleep(Duration::from_secs(2));
        console.send("\r");
        console.expect(&format!("{hostname} login: "), Duration::from_secs(60));
        console.send(&format!("{username}\r"));
        console.expect("Password: ", Duration::from_secs(30));
        console.send(&format!("{password}\r"));
        console.expect(&format!("{username}@{hostname}:~$ "), Duration::from_secs(60));
        console.send("echo GX_$((6*7))_$(id -un)_$(sudo -n true && echo root)\r");
        console.expect(&format!("GX_42_{username}_root"), Duration::from_secs(30));
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

    // QEMU keeps a private copy of the UEFI variable store.
    let vars = tmp.path().join("firmware-vars").join(format!("{id}.fd"));
    assert_eq!(vars.exists(), hypervisor == "qemu", "{}", vars.display());

    // Graceful stop: the guest shuts down on the power button, well
    // before the deadline that would force it.
    let started = Instant::now();
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/stop?graceful_timeout_secs=120"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "stopped");
    assert!(started.elapsed() < Duration::from_secs(110), "guest ignored the power button");
    // Lossy: the console log is the guest's raw bytes, not always UTF-8.
    let log = String::from_utf8_lossy(&std::fs::read(glidex_control_plane::paths::vm_paths(&id).log).unwrap()).into_owned();
    // The kernel's very last line: nothing the guest printed before the
    // hypervisor exited may be lost (hypervisor/console.rs).
    assert!(log.contains("reboot: Power down"), "guest did not power off cleanly:\n{log}");

    // Delete cleans up the generated seed (and the variable store).
    let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!Path::new(&seed_path).exists(), "seed image not removed on delete");
    assert!(!vars.exists(), "variable store not removed on delete");

    // The credential is no longer referenced and can be removed.
    let (status, _) = request(&app, "DELETE", &format!("/credentials/{username}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// Host's IPv4 default gateway (from `ip -j route`), used as a target that
/// is reachable only through NAT.
fn host_default_gateway() -> Option<String> {
    let out = Command::new("ip").args(["-4", "-j", "route", "show", "default"]).output().ok()?;
    let routes: Value = serde_json::from_slice(&out.stdout).ok()?;
    routes.get(0)?.get("gateway")?.as_str().map(str::to_string)
}

/// M4 acceptance (spec §15): a VM on a NAT network gets a 10.88.x address
/// by DHCP and reaches a host outside the machine through masquerading.
/// Needs a running glidex-netd (`GLIDEX_NETD_RUN_DIR`, default
/// /run/glidex) that this user may use, with Open vSwitch running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, glidex-netd, Open vSwitch and GLIDEX_TEST_IMAGE"]
async fn nat_network_e2e() {
    nat_e2e("cloudhypervisor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, QEMU, OVMF, glidex-netd, Open vSwitch and GLIDEX_TEST_IMAGE"]
async fn qemu_nat_network_e2e() {
    nat_e2e("qemu").await;
}

/// A Cloud-Hypervisor guest and a QEMU guest on one tap network reach
/// each other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots two real VMs; needs KVM, cloud-hypervisor, QEMU, OVMF, glidex-netd, Open vSwitch and GLIDEX_TEST_IMAGE"]
async fn mixed_hypervisors_share_a_network() {
    let (app, manager, tmp) = create_test_app();
    let _guard = ShutdownGuard(manager.clone());
    request(&app, "DELETE", "/networks/mixed", None).await;
    let (st, net) = request(&app, "POST", "/networks", Some(json!({"name": "mixed", "mode": "nat", "bridge": "gxbr-mixed"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{net}");

    let ch = start_and_login(&app, &tmp, "mixed", "gx-mix-ch", json!({"hypervisor": "cloudhypervisor"})).await;
    let qemu = start_and_login(&app, &tmp, "mixed", "gx-mix-qemu", json!({"hypervisor": "qemu"})).await;
    let (ch_ip, qemu_ip) = (ch.ipv4.clone().unwrap(), qemu.ipv4.clone().unwrap());
    let qemu = run_checks(qemu, vec![(format!("ping -c 2 -W 3 {ch_ip} >/dev/null && echo GX_$((60+1))_TO_CH"), "GX_61_TO_CH".into())]).await;
    let ch = run_checks(ch, vec![(format!("ping -c 2 -W 3 {qemu_ip} >/dev/null && echo GX_$((60+2))_TO_QEMU"), "GX_62_TO_QEMU".into())]).await;
    stop_and_delete(&app, qemu).await;
    stop_and_delete(&app, ch).await;
    let (st, body) = request(&app, "DELETE", "/networks/mixed", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
}

/// NAT network acceptance for one hypervisor. Ends with the guest
/// powering itself off, which must leave the VM stopped with its port
/// released.
async fn nat_e2e(hypervisor: &str) {
    let firmware = test_firmware(hypervisor);
    let source_image = env_path("GLIDEX_TEST_IMAGE", None);
    let gateway = host_default_gateway().expect("host has no IPv4 default gateway");

    let (app, manager, tmp) = create_test_app();
    let _guard = ShutdownGuard(manager.clone());

    let (_, status) = request(&app, "GET", "/ovs/status", None).await;
    assert_eq!(status["netd"]["access"], "full", "glidex-netd not usable: {status}");
    assert_eq!(status["host"]["ovs_running"], true, "Open vSwitch not running: {status}");

    // Leftovers from an interrupted run.
    request(&app, "DELETE", "/networks/e2e", None).await;

    let (st, net) = request(
        &app,
        "POST",
        "/networks",
        Some(json!({"name": "e2e", "mode": "nat", "bridge": "gxbr-e2e"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{net}");

    let rootfs = tmp.path().join("rootfs.raw");
    run_ok(Command::new("cp").arg("--sparse=always").arg(&source_image).arg(&rootfs));
    let username = "gxnet";
    let password = uuid::Uuid::new_v4().simple().to_string();
    let (st, body) = request(&app, "POST", "/credentials", Some(json!({"username": username, "password": password}))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");

    let hostname = "gx-nattest";
    let (st, vm) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({
            "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048,
            "hypervisor": hypervisor,
            "firmware_path": firmware, "rootfs_path": rootfs,
            "credential": username,
            "networks": [{"network": "e2e", "queue_pairs": 2}],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{vm}");
    let id = vm["id"].as_str().unwrap().to_string();

    let (st, body) = request(&app, "POST", &format!("/vms/{id}/start"), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let ipv4 = body["nics"][0]["ipv4"].as_str().expect("NAT reservation").to_string();
    assert!(ipv4.starts_with("10.88."), "{ipv4}");

    let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;
    let expected_ip = ipv4.clone();
    tokio::task::spawn_blocking(move || {
        let mut c = Console::connect(&console_path);
        c.expect("Cloud-init v.", Duration::from_secs(300));
        c.expect("finished", Duration::from_secs(120));
        std::thread::sleep(Duration::from_secs(2));
        c.send("\r");
        c.expect(&format!("{hostname} login: "), Duration::from_secs(60));
        c.send(&format!("{username}\r"));
        c.expect("Password: ", Duration::from_secs(30));
        c.send(&format!("{password}\r"));
        c.expect(&format!("{username}@{hostname}:~$ "), Duration::from_secs(60));
        // DHCP from netd's dnsmasq gave the reserved address (whatever
        // the guest named the interface).
        c.send(&format!("ip -4 -o addr | grep -q ' {expected_ip}/24 ' && echo GX_$((40+1))_DHCP_OK\r"));
        c.expect("GX_41_DHCP_OK", Duration::from_secs(30));
        // Masquerade: reach the host's own default gateway.
        c.send(&format!("ping -c 2 -W 3 {gateway} >/dev/null && echo GX_$((40+2))_NAT_OK\r"));
        c.expect("GX_42_NAT_OK", Duration::from_secs(30));
        // queue_pairs: 2 reached the guest as a multiqueue NIC.
        c.send("ls -d /sys/class/net/*/queues/rx-1 >/dev/null && echo GX_$((40+3))_MQ_OK\r");
        c.expect("GX_43_MQ_OK", Duration::from_secs(30));
        c.send("sudo poweroff\r");
    })
    .await
    .unwrap();

    // The guest powered itself off: the VM ends up stopped, port released.
    let deadline = Instant::now() + Duration::from_secs(120);
    while manager.reap_exited_vms().await.is_empty() {
        assert!(Instant::now() < deadline, "VM still running after guest poweroff");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let (_, body) = request(&app, "GET", &format!("/vms/{id}"), None).await;
    assert_eq!(body["state"], "stopped", "{body}");
    assert!(body["nics"][0]["port"].is_null(), "{body}");

    let (st, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, body) = request(&app, "DELETE", "/networks/e2e", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    let (_, bridges) = request(&app, "GET", "/ovs/bridges", None).await;
    assert!(
        !bridges.as_array().unwrap().iter().any(|b| b["spec"]["name"] == "gxbr-e2e"),
        "bridge removed with its network: {bridges}"
    );
    request(&app, "DELETE", &format!("/credentials/{username}"), None).await;
}

// ---- uplink e2e (M6 bridged/kernel, M8 AF_XDP) -----------------------------
//
// Need the fake LAN from `sudo scripts/dev/fake-lan-setup.sh`: veth pairs
// gxup0/gxup1 whose peers live in netns `gxlan` with a gateway and dnsmasq
// (it also sets netd's commit window to 10 s). Never run against real NICs.
// Undo with `sudo scripts/dev/fake-lan-teardown.sh`.

fn host_ip(args: &[&str]) -> String {
    String::from_utf8_lossy(&Command::new("ip").args(args).output().unwrap().stdout).into_owned()
}

/// A logged-in guest started by `start_and_login`.
struct Guest {
    id: String,
    console: Console,
    ipv4: Option<String>,
    username: String,
}

/// Boot a VM with a throwaway credential (named after the host) on
/// `network` and log in on the console. `extra` overrides fields of the
/// create request; `"hypervisor"` also picks the firmware.
async fn start_and_login(app: &Router, tmp: &TempDir, network: &str, hostname: &str, extra: Value) -> Guest {
    let hypervisor = extra["hypervisor"].as_str().unwrap_or("cloudhypervisor").to_string();
    let firmware = test_firmware(&hypervisor);
    let source_image = env_path("GLIDEX_TEST_IMAGE", None);
    let rootfs = tmp.path().join(format!("{hostname}.raw"));
    run_ok(Command::new("cp").arg("--sparse=always").arg(&source_image).arg(&rootfs));
    let username = format!("gx{}", hostname.replace('-', "").chars().rev().take(6).collect::<String>());
    let password = uuid::Uuid::new_v4().simple().to_string();
    request(app, "DELETE", &format!("/credentials/{username}"), None).await;
    let (st, body) = request(app, "POST", "/credentials", Some(json!({"username": username, "password": password}))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let mut spec = json!({
        "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": hypervisor,
        "firmware_path": firmware, "rootfs_path": rootfs, "credential": username,
        "networks": [{"network": network}],
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        spec[k] = v;
    }
    let (st, vm) = request(app, "POST", "/vms", Some(spec)).await;
    assert_eq!(st, StatusCode::CREATED, "{vm}");
    let id = vm["id"].as_str().unwrap().to_string();
    let (st, body) = request(app, "POST", &format!("/vms/{id}/start"), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let ipv4 = body["nics"][0]["ipv4"].as_str().map(str::to_string);
    let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;
    let host = hostname.to_string();
    let user = username.clone();
    let console = tokio::task::spawn_blocking(move || {
        let mut c = Console::connect(&console_path);
        c.expect("Cloud-init v.", Duration::from_secs(300));
        c.expect("finished", Duration::from_secs(180));
        std::thread::sleep(Duration::from_secs(2));
        c.send("\r");
        c.expect(&format!("{host} login: "), Duration::from_secs(60));
        c.send(&format!("{user}\r"));
        c.expect("Password: ", Duration::from_secs(30));
        c.send(&format!("{password}\r"));
        c.expect(&format!("{user}@{host}:~$ "), Duration::from_secs(60));
        c
    })
    .await
    .unwrap();
    Guest { id, console, ipv4, username }
}

/// Run `checks` (command, expected marker) on a guest's console.
async fn run_checks(guest: Guest, checks: Vec<(String, String)>) -> Guest {
    let Guest { id, mut console, ipv4, username } = guest;
    let console = tokio::task::spawn_blocking(move || {
        for (cmd, marker) in checks {
            console.send(&format!("{cmd}\r"));
            console.expect(&marker, Duration::from_secs(40));
        }
        console
    })
    .await
    .unwrap();
    Guest { id, console, ipv4, username }
}

async fn stop_and_delete(app: &Router, guest: Guest) {
    drop(guest.console);
    request(app, "POST", &format!("/vms/{}/stop", guest.id), None).await;
    let (st, _) = request(app, "DELETE", &format!("/vms/{}", guest.id), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    request(app, "DELETE", &format!("/credentials/{}", guest.username), None).await;
}

/// Boot a VM on `network`, log in, run `checks`, then delete the VM.
async fn boot_and_check(app: &Router, tmp: &TempDir, network: &str, hostname: &str, extra: Value, checks: Vec<(String, String)>) {
    let guest = start_and_login(app, tmp, network, hostname, extra).await;
    let guest = run_checks(guest, checks).await;
    stop_and_delete(app, guest).await;
}

async fn require_lan_and_netd(app: &Router) {
    assert!(std::env::var("GLIDEX_TEST_LAN").is_ok(), "set GLIDEX_TEST_LAN=1 after running sudo scripts/dev/fake-lan-setup.sh");
    assert!(host_ip(&["-br", "link", "show", "gxup0"]).contains("gxup0"), "fake LAN missing (gxup0)");
    let (_, status) = request(app, "GET", "/ovs/status", None).await;
    assert_eq!(status["netd"]["access"], "full", "{status}");
    assert_eq!(status["host"]["ovs_running"], true, "{status}");
}

/// M6 acceptance: kernel uplink on an in-use NIC with IP migration —
/// refused, rolled back without commit, committed, VM on the LAN, restored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs root-run glidex-netd, OVS, the fake LAN (GLIDEX_TEST_LAN) and GLIDEX_TEST_IMAGE"]
async fn bridged_uplink_e2e() {
    let (app, manager, tmp) = create_test_app();
    let _guard = ShutdownGuard(manager.clone());
    require_lan_and_netd(&app).await;
    // Leftovers from an interrupted run.
    request(&app, "DELETE", "/networks/lan", None).await;
    request(&app, "DELETE", "/ovs/bridges/gxbr-up/uplinks/gxup0", None).await;
    request(&app, "DELETE", "/ovs/bridges/gxbr-up", None).await;
    assert!(host_ip(&["-4", "-o", "addr", "show", "dev", "gxup0"]).contains("192.0.2.10/24"), "gxup0 should hold 192.0.2.10");

    let (st, body) = request(&app, "POST", "/ovs/bridges", Some(json!({"name": "gxbr-up", "datapath": "system"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let uplink = json!({"name": "gxup0", "kind": "kernel", "ifname": "gxup0"});

    // 1. In use by the host: refused without migrate_ip + confirm.
    let (st, body) = request(&app, "POST", "/ovs/bridges/gxbr-up/uplinks", Some(uplink.clone())).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "host_interface_in_use");
    assert!(body["details"]["reasons"].as_array().unwrap().iter().any(|r| r == "has_addresses"), "{body}");

    // 2. Migrated but never committed: rolled back by netd (10 s window in the dev config).
    let mut migrate = uplink.clone();
    migrate["migrate_ip"] = json!(true);
    migrate["confirm"] = json!(true);
    let (st, body) = request(&app, "POST", "/ovs/bridges/gxbr-up/uplinks", Some(migrate.clone())).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert!(host_ip(&["-4", "-o", "addr", "show", "dev", "gxbr-up"]).contains("192.0.2.10/24"), "address moved to the bridge");
    assert!(Command::new("ping").args(["-c", "1", "-W", "2", "192.0.2.1"]).status().unwrap().success(), "LAN reachable through the bridge");
    // netd deletes the record as the last rollback step, so wait for that.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (_, list) = request(&app, "GET", "/ovs/bridges/gxbr-up/uplinks", None).await;
        if list.as_array().unwrap().is_empty() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "not rolled back: {list}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(host_ip(&["-4", "-o", "addr", "show", "dev", "gxup0"]).contains("192.0.2.10/24"), "address restored");
    assert!(host_ip(&["-4", "route", "show", "198.51.100.0/24"]).contains("dev gxup0"), "route restored");

    // 3. Migrated and committed.
    let (st, body) = request(&app, "POST", "/ovs/bridges/gxbr-up/uplinks", Some(migrate)).await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    let token = body["record"]["pending"]["token"].as_str().unwrap().to_string();
    let (st, body) = request(&app, "POST", "/ovs/bridges/gxbr-up/uplinks/gxup0/commit", Some(json!({"token": token}))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["phase"], "active");
    assert!(host_ip(&["-4", "route", "show", "198.51.100.0/24"]).contains("dev gxbr-up"), "route moved");

    // 4. A VM on the bridged network gets a LAN address and reaches the LAN and the host.
    let (st, body) = request(&app, "POST", "/networks", Some(json!({"name": "lan", "mode": "bridged", "bridge": "gxbr-up"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    boot_and_check(&app, &tmp, "lan", "gx-lantest", json!({}), vec![
        ("ip -4 -o addr | grep -q ' 192.0.2.1[0-9][0-9]/24 ' && echo GX_$((60+1))_LAN_DHCP".into(), "GX_61_LAN_DHCP".into()),
        ("ping -c 2 -W 3 192.0.2.1 >/dev/null && echo GX_$((60+2))_LAN_GW".into(), "GX_62_LAN_GW".into()),
        ("ping -c 2 -W 3 192.0.2.10 >/dev/null && echo GX_$((60+3))_HOST".into(), "GX_63_HOST".into()),
    ]).await;

    // 5. Cleanup restores the NIC.
    let (st, _) = request(&app, "DELETE", "/networks/lan", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, body) = request(&app, "DELETE", "/ovs/bridges/gxbr-up/uplinks/gxup0", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    assert!(host_ip(&["-4", "-o", "addr", "show", "dev", "gxup0"]).contains("192.0.2.10/24"), "address back on gxup0");
    assert!(host_ip(&["-4", "route", "show", "198.51.100.0/24"]).contains("dev gxup0"), "route back on gxup0");
    let (st, _) = request(&app, "DELETE", "/ovs/bridges/gxbr-up", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

/// M8 acceptance (combination D): AF_XDP uplink in generic mode on a
/// userspace-datapath bridge, tap VM port, DHCP from the LAN.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs root-run glidex-netd, OVS with afxdp, the fake LAN (GLIDEX_TEST_LAN) and GLIDEX_TEST_IMAGE"]
async fn afxdp_uplink_e2e() {
    let (app, manager, tmp) = create_test_app();
    let _guard = ShutdownGuard(manager.clone());
    require_lan_and_netd(&app).await;
    request(&app, "DELETE", "/networks/xdp", None).await;
    request(&app, "DELETE", "/ovs/bridges/gxbr-xdp/uplinks/gxup1", None).await;
    request(&app, "DELETE", "/ovs/bridges/gxbr-xdp", None).await;

    let (st, body) = request(&app, "POST", "/ovs/bridges", Some(json!({"name": "gxbr-xdp", "datapath": "netdev"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let (st, body) = request(&app, "POST", "/ovs/bridges/gxbr-xdp/uplinks", Some(json!({
        "name": "gxup1", "kind": "afxdp", "ifname": "gxup1", "xdp_mode": "generic"
    }))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert!(body["live"]["error"].is_null(), "AF_XDP port error: {body}");

    let (st, body) = request(&app, "POST", "/networks", Some(json!({"name": "xdp", "mode": "bridged", "bridge": "gxbr-xdp"}))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    boot_and_check(&app, &tmp, "xdp", "gx-xdptest", json!({}), vec![
        ("ip -4 -o addr | grep -q ' 198.18.0.1[0-9][0-9]/24 ' && echo GX_$((70+1))_XDP_DHCP".into(), "GX_71_XDP_DHCP".into()),
        ("ping -c 2 -W 3 198.18.0.1 >/dev/null && echo GX_$((70+2))_XDP_GW".into(), "GX_72_XDP_GW".into()),
    ]).await;

    request(&app, "DELETE", "/networks/xdp", None).await;
    let (st, _) = request(&app, "DELETE", "/ovs/bridges/gxbr-xdp/uplinks/gxup1", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = request(&app, "DELETE", "/ovs/bridges/gxbr-xdp", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

/// M7 acceptance: vhost-user VM port on a userspace-datapath NAT network
/// with OVS-DPDK (shared guest memory, CH as vhost-user server).
/// Needs `ovs install --profile dpdk` and `ovs dpdk-init` done first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs root-run glidex-netd, OVS-DPDK initialized (GLIDEX_TEST_DPDK) and GLIDEX_TEST_IMAGE"]
async fn vhost_user_e2e() {
    vhost_user_net_e2e("cloudhypervisor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs root-run glidex-netd, OVS-DPDK initialized (GLIDEX_TEST_DPDK), QEMU, OVMF and GLIDEX_TEST_IMAGE"]
async fn qemu_vhost_user_e2e() {
    vhost_user_net_e2e("qemu").await;
}

async fn vhost_user_net_e2e(hypervisor: &str) {
    assert!(std::env::var("GLIDEX_TEST_DPDK").is_ok(), "set GLIDEX_TEST_DPDK=1 once OVS-DPDK is initialized");
    let (app, manager, tmp) = create_test_app();
    let _guard = ShutdownGuard(manager.clone());
    let (_, status) = request(&app, "GET", "/ovs/status", None).await;
    assert_eq!(status["host"]["dpdk_initialized"], true, "{status}");
    assert!(
        status["host"]["iface_types"].as_array().unwrap().iter().any(|t| t == "dpdkvhostuserclient"),
        "{status}"
    );
    request(&app, "DELETE", "/networks/fast", None).await;

    let (st, net) = request(&app, "POST", "/networks", Some(json!({
        "name": "fast", "mode": "nat", "port_type": "vhost_user", "bridge": "gxbr-fast"
    }))).await;
    assert_eq!(st, StatusCode::CREATED, "{net}");
    let (_, bridges) = request(&app, "GET", "/ovs/bridges", None).await;
    let fast = bridges.as_array().unwrap().iter().find(|b| b["spec"]["name"] == "gxbr-fast").unwrap().clone();
    assert_eq!(fast["spec"]["datapath"], "netdev");

    let (_, nats) = request(&app, "GET", "/networks/fast", None).await;
    assert_eq!(nats["port_type"], "vhost_user");
    // OVS-DPDK's vhost-user backend needs hugepage-backed guest memory.
    boot_and_check(&app, &tmp, "fast", "gx-vhosttest", json!({"hugepages": true, "mem_size_mib": 1024, "hypervisor": hypervisor}), vec![
        ("ip -4 -o addr | grep -q ' 10.88.' && echo GX_$((80+1))_VHOST_DHCP".into(), "GX_81_VHOST_DHCP".into()),
        ("ping -c 2 -W 3 $(ip -4 route show default | awk '{print $3}') >/dev/null && echo GX_$((80+2))_VHOST_GW".into(), "GX_82_VHOST_GW".into()),
    ]).await;

    let (st, body) = request(&app, "DELETE", "/networks/fast", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
}

/// spec/images.md §12: pull a catalog image, boot a VM created from it with
/// a 12 GiB root disk, check the guest's `/` filled it; stop, grow to
/// 16 GiB, boot again and check again. Downloads the image (~700 MB) from
/// the vendor; `GLIDEX_TEST_CATALOG` picks the entry (default ubuntu-26.04).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "downloads a cloud image and boots it; needs network, KVM, cloud-hypervisor and qemu-img/qemu-io/sgdisk/growpart"]
async fn catalog_image_boots_and_root_grows() {
    catalog_image_e2e("cloudhypervisor").await;
}

/// The same on QEMU, without a `firmware_path`: an `image` VM must get
/// the host's OVMF by default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "downloads a cloud image and boots it; needs network, KVM, QEMU, OVMF and qemu-img/qemu-io/sgdisk/growpart"]
async fn qemu_catalog_image_boots_and_root_grows() {
    catalog_image_e2e("qemu").await;
}

async fn catalog_image_e2e(hypervisor: &str) {
    // QEMU relies on the server-side default firmware.
    let firmware = (hypervisor != "qemu").then(|| test_firmware(hypervisor));
    let key = std::env::var("GLIDEX_TEST_CATALOG").unwrap_or_else(|_| "ubuntu-26.04".into());
    let (app, manager, _tmp) = create_test_app();
    let _guard = ShutdownGuard(manager.clone());

    let (status, img) = request(&app, "POST", "/images", Some(json!({"catalog": key}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let image_id = img["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(1800);
    let img = loop {
        let (_, img) = request(&app, "GET", &format!("/images/{image_id}"), None).await;
        match img["status"]["state"].as_str().unwrap() {
            "downloading" | "verifying" => {}
            _ => break img,
        }
        assert!(Instant::now() < deadline, "download timed out: {img}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    assert_eq!(img["status"]["state"], "ready", "{img}");
    assert_eq!(img["verified"], true);
    eprintln!("pulled {key}: {}", img["source"]);

    let username = "gxtester";
    let password = uuid::Uuid::new_v4().simple().to_string();
    let (status, body) = request(&app, "POST", "/credentials", Some(json!({"username": username, "password": password}))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let hostname = "gx-imgtest";
    let mut spec = json!({
        "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": hypervisor,
        "image": image_id, "root_disk_size_gib": 12, "credential": username,
    });
    if let Some(firmware) = &firmware {
        spec["firmware_path"] = json!(firmware);
    }
    let (status, vm) = request(&app, "POST", "/vms", Some(spec)).await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    let id = vm["id"].as_str().unwrap().to_string();
    let config = manager.get_vm(&id).await.unwrap().config;
    let expected = HypervisorType::Qemu.default_firmware_path().map(|p| p.to_string_lossy().into_owned());
    if hypervisor == "qemu" {
        assert_eq!(config.firmware_path, expected, "QEMU image VMs default to OVMF");
    }
    let root = vm["root_disk"].as_str().unwrap().to_string();
    // glidex extended the root partition offline, before the first boot.
    assert!(vm.get("warnings").is_none(), "{vm}");
    let (_, disk) = request(&app, "GET", &format!("/disks/{root}"), None).await;
    assert_eq!(disk["partition_table"]["free_tail_bytes"], 0, "{disk}");

    // Boot, log in, and check the root filesystem against `min_gib`.
    let boot_and_check = |min_gib: u32, marker: &'static str| {
        let (app, id) = (app.clone(), id.clone());
        let (username, password) = (username.to_string(), password.clone());
        async move {
            let (status, body) = request(&app, "POST", &format!("/vms/{id}/start"), None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;
            tokio::task::spawn_blocking(move || {
                let mut console = Console::connect(&console_path);
                console.expect(&format!("{hostname} login: "), Duration::from_secs(400));
                console.send(&format!("{username}\r"));
                console.expect("Password: ", Duration::from_secs(30));
                console.send(&format!("{password}\r"));
                console.expect(&format!("{username}@{hostname}:~$ "), Duration::from_secs(60));
                // cloud-init's resizefs runs during boot; wait for it to finish.
                console.send("cloud-init status --wait >/dev/null; df -BG --output=size / | tail -1\r");
                console.send(&format!(
                    "[ $(df -BG --output=size / | tail -1 | tr -dc 0-9) -ge {min_gib} ] && echo {marker}_$((40+2))\r"
                ));
                console.expect(&format!("{marker}_42"), Duration::from_secs(120));
            })
            .await
            .unwrap();
            let (status, body) = request(&app, "POST", &format!("/vms/{id}/stop"), None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    };

    boot_and_check(11, "GX_ROOT12").await;

    let (status, d) = request(&app, "POST", &format!("/disks/{root}/resize"), Some(json!({"size_gib": 16}))).await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["extend_root"], "grown", "{d}");

    boot_and_check(15, "GX_ROOT16").await;

    let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "GET", &format!("/disks/{root}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "owned root disk is deleted with the VM");
    let (status, _) = request(&app, "DELETE", &format!("/images/{image_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}
