//! Functional tests for firmware (UEFI) boot, direct kernel boot
//! (`kernel_boot_with_initrd`: `GLIDEX_TEST_KERNEL`, `GLIDEX_TEST_INITRD`)
//! and cloud-init seed generation.
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
//!   `GLIDEX_TEST_QEMU_FIRMWARE`). The firmware-boot tests boot through a
//!   firmware image pulled from the firmware catalog instead:
//!   `cloudhv-edk2`, and `ovmf` for QEMU (`GLIDEX_TEST_QEMU_FIRMWARE_KEY=
//!   ovmf-debian` downloads Debian's):
//!
//!   ```sh
//!   GLIDEX_TEST_IMAGE=~/ch/resolute-server-cloudimg-amd64.raw \
//!     cargo test -p glidex-control-plane --test functional_tests -- --ignored --nocapture
//!   ```
//!
//!   The image is copied into a temp dir first, so the original is never
//!   modified.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
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
use glidex_control_plane::hypervisor::HypervisorType;
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
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// A hypervisor's short name (`qemu`, `cloudhypervisor`) as its proto name.
fn hypervisor_pb(h: &str) -> String {
    match h {
        "cloudhypervisor" => "HYPERVISOR_CLOUD_HYPERVISOR".into(),
        h => format!("HYPERVISOR_{}", h.to_ascii_uppercase()),
    }
}

/// A proto3 JSON 64-bit integer: a string (`"17179869184"`) or a number;
/// left out (null) at its default, 0.
fn num(v: &Value) -> u64 {
    match v {
        Value::Null => 0,
        Value::String(s) => s.parse().unwrap_or_else(|_| panic!("not a number: {v}")),
        _ => v.as_u64().unwrap_or_else(|| panic!("not a number: {v}")),
    }
}

/// The VM in a `{vm}` answer (`POST /vms`, `GET /vms/{id}`, writes), in the
/// proto3 JSON shape (spec/grpc.md §9.1).
fn vm_of(v: &Value) -> &Value {
    assert!(v["vm"].is_object(), "not a VM answer: {v}");
    &v["vm"]
}

/// The short lower-case name of a proto enum value: `VM_STATE_RUNNING` with
/// prefix `VM_STATE_` is `running`.
fn short(v: &Value, prefix: &str) -> String {
    let s = v.as_str().unwrap_or_else(|| panic!("not an enum: {v}"));
    s.strip_prefix(prefix)
        .unwrap_or_else(|| panic!("{s} is not a {prefix}* value"))
        .to_ascii_lowercase()
}

/// The `state` of the VM in a `{vm}` answer (`created`, `running`, ...).
fn state(v: &Value) -> String {
    short(&vm_of(v)["state"], "VM_STATE_")
}

/// The `hypervisor` of the VM in a `{vm}` answer (`cloud_hypervisor`, `qemu`).
fn hypervisor(v: &Value) -> String {
    short(&vm_of(v)["hypervisor"], "HYPERVISOR_")
}

/// The id of the VM in a `{vm}` answer.
fn vm_id(v: &Value) -> String {
    vm_of(v)["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no VM id: {v}"))
        .to_string()
}

fn firmware_vm(name: &str) -> Value {
    json!({
        "name": name,
        "vcpu_count": 1,
        "mem_size_mib": 512,
        "hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR",
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
    run_ok(
        Command::new("mtype")
            .arg("-i")
            .arg(image)
            .arg(format!("::{}", file)),
    )
}

// ============================================================================
// API validation and persistence for firmware boot
// ============================================================================

#[tokio::test]
async fn firmware_vm_can_be_created_without_kernel() {
    let (app, manager, _tmp) = create_test_app();

    let (status, body) = request(&app, "POST", "/vms", Some(firmware_vm("fw-vm"))).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(hypervisor(&body), "cloud_hypervisor");
    assert_eq!(state(&body), "created");

    let vm = manager.get_vm(&vm_id(&body)).await.unwrap();
    assert_eq!(
        vm.spec.config.firmware_path.as_deref(),
        Some("/path/to/CLOUDHV.fd")
    );
    assert_eq!(vm.spec.config.kernel_image_path, "");
    assert_eq!(vm.spec.config.cloud_init_path, None);
}

#[tokio::test]
async fn qemu_firmware_vm_can_be_created_without_kernel() {
    let (app, manager, _tmp) = create_test_app();

    let mut req = firmware_vm("fw-qemu");
    req["hypervisor"] = json!("HYPERVISOR_QEMU");
    req["firmware_path"] = json!("/usr/share/OVMF/OVMF_CODE_4M.fd");
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(hypervisor(&body), "qemu");

    let vm = manager.get_vm(&vm_id(&body)).await.unwrap();
    assert_eq!(
        vm.spec.config.firmware_path.as_deref(),
        Some("/usr/share/OVMF/OVMF_CODE_4M.fd")
    );
    assert_eq!(vm.spec.config.kernel_image_path, "");
}

#[tokio::test]
async fn kernel_or_firmware_is_required() {
    let (app, _manager, _tmp) = create_test_app();

    let mut req = firmware_vm("no-boot");
    req.as_object_mut().unwrap().remove("firmware_path");
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("a kernel or firmware is required"),
        "{body}"
    );
}

#[tokio::test]
async fn qemu_accepts_a_custom_cloud_init_path() {
    let (app, manager, _tmp) = create_test_app();

    let req = json!({
        "name": "qemu-ci",
        "vcpu_count": 1,
        "mem_size_mib": 512,
        "hypervisor": "HYPERVISOR_QEMU",
        "kernel_image_path": "/path/to/vmlinux",
        "rootfs_path": "/path/to/rootfs",
        "cloud_init_path": "/path/to/seed.img"
    });
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let vm = manager.get_vm(&vm_id(&body)).await.unwrap();
    assert_eq!(
        vm.spec.config.cloud_init_path.as_deref(),
        Some("/path/to/seed.img")
    );
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
    let (status, body) = request(&app, "POST", "/vms", Some(req)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
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
        (vm_id(&keep), vm_id(&legacy))
    };

    // Rewrite one record the way an older build would have stored it.
    {
        use glidex_control_plane::store::{Db, Origin};
        use redb::TableDefinition;
        const VMS: TableDefinition<&str, &[u8]> = TableDefinition::new("vms");
        let database = Db::create(&db).unwrap();
        let mut record: Value = {
            let txn = database.begin_read().unwrap();
            let table = txn.open_table(VMS).unwrap();
            let bytes = table.get(legacy_id.as_str()).unwrap().unwrap();
            serde_json::from_slice(bytes.value()).unwrap()
        };
        record["spec"]["config"]["hypervisor"] = json!("firecracker");
        let txn = database.begin(Origin::System).unwrap();
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
    assert_eq!(hypervisor(&body), "cloud_hypervisor");
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

    let vm = manager.get_vm(&vm_id(&body)).await.unwrap();
    assert_eq!(
        vm.spec.config.firmware_path.map(PathBuf::from),
        Some(home.join("fw/CLOUDHV.fd"))
    );
    assert_eq!(
        vm.spec.config.cloud_init_path.map(PathBuf::from),
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
        vm_id(&body)
    };

    let manager = VmManager::with_db_path(db).unwrap();
    manager.initialize().await.unwrap();
    let vm = manager.get_vm(&id).await.unwrap();
    assert_eq!(
        vm.spec.config.firmware_path.as_deref(),
        Some("/path/to/CLOUDHV.fd")
    );
    // Per-VM private runtime directory (spec/security.md §9).
    assert_eq!(
        vm.default_cloud_init_path(),
        glidex_control_plane::paths::vm_dir(&id)
            .join("cloudinit.img")
            .to_string_lossy()
    );
    assert!(
        vm.paths()
            .api_socket
            .ends_with(&format!("/vms/{id}/api.sock")),
        "{}",
        vm.paths().api_socket
    );
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
    assert!(
        listing.contains("CIDATA"),
        "volume label missing:\n{listing}"
    );

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

    let err =
        cloud_init::write_seed_image(image.to_str().unwrap(), &SeedConfig::default()).unwrap_err();
    assert!(err.to_string().contains("cloud-init seed"), "{err}");
    let entries: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
    assert_eq!(entries.len(), 1, "only the pre-existing file should remain");
}

// ============================================================================
// End-to-end boot (ignored by default; needs KVM + images)
// ============================================================================

/// Stops every VM the manager knows about when dropped, so a failing test
/// never leaves a cloud-hypervisor process behind: VMs outlive the control
/// plane now, so nothing else would (spec/reconciliation.md §19).
struct ShutdownGuard(Option<Arc<VmManager>>);

impl ShutdownGuard {
    /// Let go of the manager without stopping its VMs (a simulated
    /// control-plane restart).
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        let Some(manager) = self.0.take() else { return };
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(manager.stop_all(Duration::from_secs(60)))
        });
    }
}

/// An app whose VM controller runs (detached runner), for tests that boot
/// real guests. The shim binary must be built (`cargo build -p
/// glidex-vm-shim`, or set GLIDEX_VM_SHIM).
async fn create_running_app() -> (Router, Arc<VmManager>, TempDir) {
    let (app, manager, tmp) = create_test_app();
    manager.initialize().await.unwrap();
    manager.start_controllers();
    (app, manager, tmp)
}

/// Minimal expect-style driver for the VM console socket.
struct Console {
    stream: UnixStream,
    buf: Vec<u8>,
}

impl Console {
    fn connect(path: &str) -> Self {
        let stream =
            UnixStream::connect(path).unwrap_or_else(|e| panic!("connect console {path}: {e}"));
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        Self {
            stream,
            buf: Vec::new(),
        }
    }

    /// Drop what the console replays to a new client (the recent history,
    /// sent at once: a previous boot's prompts would match): read until it
    /// has been quiet for half a second.
    fn skip_history(&mut self) {
        let mut chunk = [0u8; 4096];
        let mut quiet_since = Instant::now();
        while quiet_since.elapsed() < Duration::from_millis(500) {
            match self.stream.read(&mut chunk) {
                Ok(n) if n > 0 => quiet_since = Instant::now(),
                _ => {}
            }
        }
        self.buf.clear();
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
                let tail: String = tail
                    .chars()
                    .rev()
                    .take(600)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
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

    /// [`Self::expect`] without the panic: whether `needle` came in time.
    fn try_expect(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(pos) = find(&self.buf, needle.as_bytes()) {
                self.buf.drain(..pos + needle.len());
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            match self.stream.read(&mut chunk) {
                Ok(n) if n > 0 => self.buf.extend_from_slice(&chunk[..n]),
                _ => std::thread::sleep(Duration::from_millis(50)),
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
            HypervisorType::Qemu
                .default_firmware_path()
                .map(|p| p.to_string_lossy().into_owned()),
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

/// Pull the hypervisor's firmware catalog entry as a firmware image: the
/// installer's pinned CLOUDHV.fd or the host's OVMF is copied (else it is
/// downloaded). `GLIDEX_TEST_QEMU_FIRMWARE_KEY` picks another QEMU entry
/// (`ovmf-debian`). Returns its id.
async fn pull_firmware_image(app: &Router, hypervisor: &str) -> String {
    let qemu_key = std::env::var("GLIDEX_TEST_QEMU_FIRMWARE_KEY").unwrap_or_else(|_| "ovmf".into());
    let key = if hypervisor == "qemu" {
        qemu_key.as_str()
    } else {
        "cloudhv-edk2"
    };
    let (status, fw) = request(app, "POST", "/images", Some(json!({"firmware": key}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{fw}");
    let id = fw["image"]["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let (_, fw) = request(app, "GET", &format!("/images/{id}"), None).await;
        let fw = &fw["image"];
        match fw["status"]["state"].as_str() {
            Some("IMAGE_STATE_DOWNLOADING" | "IMAGE_STATE_VERIFYING") => {
                assert!(Instant::now() < deadline, "firmware download timed out")
            }
            _ => {
                assert_eq!(fw["status"]["state"], "IMAGE_STATE_READY", "{fw}");
                return id;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Boot a cloud image through UEFI (a firmware image) with a generated
/// seed and a stored credential, log in, pause/resume, then shut the guest
/// down gracefully.
async fn firmware_boot_e2e(hypervisor: &str) {
    let source_image = env_path("GLIDEX_TEST_IMAGE", None);
    assert!(Path::new("/dev/kvm").exists(), "/dev/kvm is required");

    let (app, manager, tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));
    let firmware = pull_firmware_image(&app, hypervisor).await;

    // Work on a sparse copy so the source image is never modified.
    let rootfs = tmp.path().join("rootfs.raw");
    run_ok(
        Command::new("cp")
            .arg("--sparse=always")
            .arg(&source_image)
            .arg(&rootfs),
    );
    // The copy keeps the source's mode; a managed image is read-only.
    std::fs::set_permissions(&rootfs, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();

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
            "hypervisor": hypervisor_pb(hypervisor),
            "firmware": firmware,
            "rootfs_path": rootfs,
            "credential": username,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    let id = vm_id(&vm);
    let seed_path = manager.get_vm(&id).await.unwrap().default_cloud_init_path();

    // Start: seed is generated and the VM reaches Running.
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/start?wait=120"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state(&body), "running");
    assert!(Path::new(&seed_path).exists(), "seed image not generated");
    let persisted = manager.get_vm(&id).await.unwrap();
    assert_eq!(
        persisted.spec.config.cloud_init_path, None,
        "generated seed must not be persisted"
    );

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
        console.expect(
            &format!("{username}@{hostname}:~$ "),
            Duration::from_secs(60),
        );
        console.send("echo GX_$((6*7))_$(id -un)_$(sudo -n true && echo root)\r");
        console.expect(&format!("GX_42_{username}_root"), Duration::from_secs(30));
        console
    })
    .await
    .unwrap();

    // Pause / resume keeps the guest alive.
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/pause?wait=60"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state(&body), "paused");
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/start?wait=60"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state(&body), "running");
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
    let (status, body) = request(
        &app,
        "POST",
        &format!("/vms/{id}/stop?graceful_timeout_secs=120&wait=200"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state(&body), "stopped");
    assert!(
        started.elapsed() < Duration::from_secs(110),
        "guest ignored the power button"
    );
    // Lossy: the console log is the guest's raw bytes, not always UTF-8.
    let log = String::from_utf8_lossy(
        &std::fs::read(glidex_control_plane::paths::vm_paths(&id).log).unwrap(),
    )
    .into_owned();
    // The kernel's very last line: nothing the guest printed before the
    // hypervisor exited may be lost (glidex-vm-shim's proxy).
    assert!(
        log.contains("reboot: Power down"),
        "guest did not power off cleanly:\n{log}"
    );

    // Delete cleans up the generated seed (and the variable store).
    let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        !Path::new(&seed_path).exists(),
        "seed image not removed on delete"
    );
    assert!(!vars.exists(), "variable store not removed on delete");

    // The credential is no longer referenced and can be removed.
    let (status, _) = request(&app, "DELETE", &format!("/credentials/{username}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "downloads a catalog image and the kernel extractor, boots a real VM; needs KVM and cloud-hypervisor"]
async fn extracted_kernel_boots_its_image() {
    extracted_kernel_e2e("cloudhypervisor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "downloads a catalog image and the kernel extractor, boots a real VM; needs KVM and qemu-system-x86_64"]
async fn qemu_extracted_kernel_boots_its_image() {
    extracted_kernel_e2e("qemu").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "downloads a catalog image and the kernel extractor, boots a real VM twice; needs KVM and cloud-hypervisor"]
async fn guest_kernel_updates_are_followed() {
    assert!(Path::new("/dev/kvm").exists(), "/dev/kvm is required");
    let (app, manager, _tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));
    let (status, img) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"catalog": "ubuntu-26.04"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let image_id = img["image"]["id"].as_str().unwrap().to_string();
    let (_, all) = request(&app, "GET", "/images", None).await;
    let k = all["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["for_image"] == image_id.as_str())
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        settled_image(&app, &image_id, Duration::from_secs(900)).await["status"]["state"],
        "IMAGE_STATE_READY"
    );
    let kimg = settled_image(&app, &k, Duration::from_secs(600)).await;
    assert_eq!(kimg["status"]["state"], "IMAGE_STATE_READY", "{kimg}");

    let username = "gxfollow";
    let password = uuid::Uuid::new_v4().simple().to_string();
    let (status, body) = request(
        &app,
        "POST",
        "/credentials",
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let hostname = "gx-follow";
    let (status, vm) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({
            "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048, "image": image_id,
            "root_disk_size_gib": 12, "kernel": k, "kernel_from_disk": true, "credential": username,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    let id = vm_id(&vm);
    let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;

    // Logs in on a fresh console (skipping the replay) and runs `cmd`,
    // expecting `marker` in its output.
    let session = |cmd: String, marker: String, wait: Duration| {
        let (path, user, pass) = (console_path.clone(), username.to_string(), password.clone());
        tokio::task::spawn_blocking(move || {
            let mut console = Console::connect(&path);
            console.skip_history();
            for attempt in 0..12 {
                std::thread::sleep(Duration::from_secs(if attempt == 0 { 2 } else { 10 }));
                console.send("\r");
                if !console.try_expect("login: ", Duration::from_secs(60)) {
                    continue;
                }
                console.send(&format!("{user}\r"));
                if !console.try_expect("Password: ", Duration::from_secs(20)) {
                    continue;
                }
                console.send(&format!("{pass}\r"));
                std::thread::sleep(Duration::from_secs(3));
                console.send(&format!("{cmd}\r"));
                if console.try_expect(&marker, wait) {
                    return;
                }
            }
            panic!("could not log in and see {marker}");
        })
    };

    // First launch: the disk's kernel is the image's, so `k` itself.
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/start?wait=600"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(vm_of(&body)["booted_kernel"], k.as_str(), "{body}");
    assert!(vm_of(&body)["previous_kernel"].is_null(), "{body}");
    // The guest rebuilds its initramfs, as a kernel update would.
    session(
        "sudo update-initramfs -u -k all >/dev/null 2>&1; echo GX_REBUILT_$?".into(),
        "GX_REBUILT_0".into(),
        Duration::from_secs(300),
    )
    .await
    .unwrap();
    let (status, body) = request(
        &app,
        "POST",
        &format!("/vms/{id}/stop?graceful_timeout_secs=120&wait=200"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Second launch: the disk changed, so its new kernel is extracted.
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/start?wait=600"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let booted = vm_of(&body)["booted_kernel"].as_str().unwrap().to_string();
    assert_ne!(booted, k, "a new kernel image: {body}");
    assert_eq!(vm_of(&body)["previous_kernel"], k.as_str(), "{body}");
    let (_, newk) = request(&app, "GET", &format!("/images/{booted}"), None).await;
    let newk = &newk["image"];
    assert_eq!(newk["owner"], id.as_str(), "{newk}");
    assert_eq!(
        newk["kernel"]["release"], kimg["kernel"]["release"],
        "{newk}"
    );
    assert_ne!(
        newk["kernel"]["initrd"]["sha256"], kimg["kernel"]["initrd"]["sha256"],
        "{newk}"
    );
    let (_, ev) = request(&app, "GET", &format!("/vms/{id}/events"), None).await;
    assert!(ev.to_string().contains("KernelUpdated"), "{ev}");
    session(
        "echo GX_UP_$(uname -r)".into(),
        "GX_UP_".into(),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    let (status, _) = request(
        &app,
        "POST",
        &format!("/vms/{id}/stop?graceful_timeout_secs=120&wait=200"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Back to the image's kernel by hand: following stops.
    let (status, body) = request(
        &app,
        "PATCH",
        &format!("/vms/{id}"),
        Some(json!({
            "change": {"kernel": k}, "update_mask": "kernel"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/start?wait=600"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(vm_of(&body)["kernel_from_disk"].is_null(), "{body}");
    assert!(vm_of(&body)["booted_kernel"].is_null(), "{body}");
    session(
        "echo GX_BACK_ok".into(),
        "GX_BACK_ok".into(),
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    let (status, _) = request(&app, "POST", &format!("/vms/{id}/stop?wait=200"), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// Wait for image `id` to leave downloading, verifying and extracting.
async fn settled_image(app: &Router, id: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let (status, img) = request(app, "GET", &format!("/images/{id}"), None).await;
        assert_eq!(status, StatusCode::OK, "{img}");
        let img = img["image"].clone();
        match img["status"]["state"].as_str() {
            Some(
                "IMAGE_STATE_DOWNLOADING" | "IMAGE_STATE_VERIFYING" | "IMAGE_STATE_EXTRACTING",
            ) => {
                assert!(Instant::now() < deadline, "image {id} stuck: {img}")
            }
            _ => return img,
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// spec/kernel-boot.md K3: pulling `GLIDEX_TEST_CATALOG` (default
/// `ubuntu-26.04`) also extracts its kernel, through the pinned Debian
/// extractor run as a one-shot VM job; a VM boots that kernel image with a
/// generated seed and a credential, and `uname -r` is its release.
async fn extracted_kernel_e2e(hypervisor: &str) {
    assert!(Path::new("/dev/kvm").exists(), "/dev/kvm is required");
    let key = std::env::var("GLIDEX_TEST_CATALOG").unwrap_or_else(|_| "ubuntu-26.04".into());
    let (app, manager, _tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));

    let (status, img) = request(&app, "POST", "/images", Some(json!({"catalog": key}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let image_id = img["image"]["id"].as_str().unwrap().to_string();
    let (_, all) = request(&app, "GET", "/images", None).await;
    let kernel_id = all["images"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["for_image"] == image_id.as_str())
        .unwrap_or_else(|| panic!("no kernel image for {key}: {all}"))["id"]
        .as_str()
        .unwrap()
        .to_string();
    let img = settled_image(&app, &image_id, Duration::from_secs(900)).await;
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    let k = settled_image(&app, &kernel_id, Duration::from_secs(600)).await;
    assert_eq!(k["status"]["state"], "IMAGE_STATE_READY", "{k}");
    assert_eq!(k["kind"], "IMAGE_KIND_KERNEL");
    assert_eq!(k["verified"], true, "a catalog image's kernel: {k}");
    assert_eq!(
        k["source"]["extracted"]["image_sha256"], img["sha256"],
        "{k}"
    );
    let release = k["kernel"]["release"]
        .as_str()
        .expect("a bzImage release")
        .to_string();
    let cmdline = k["kernel"]["cmdline"].as_str().unwrap().to_string();
    assert!(
        cmdline.contains("root=") && cmdline.ends_with("console=ttyS0"),
        "{cmdline}"
    );
    eprintln!(
        "{key}: kernel {release} from {}, cmdline {cmdline}",
        k["source"]["extracted"]["path"]
    );

    let username = "gxextract";
    let password = uuid::Uuid::new_v4().simple().to_string();
    let (status, body) = request(
        &app,
        "POST",
        "/credentials",
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let hostname = "gx-extracted";
    let (status, vm) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({
            "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048,
            "hypervisor": hypervisor_pb(hypervisor), "image": image_id, "root_disk_size_gib": 12,
            "kernel": kernel_id, "credential": username,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    let id = vm_id(&vm);
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/start?wait=600"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;
    let want = format!("GX_{release}_ok");
    tokio::task::spawn_blocking(move || {
        let mut console = Console::connect(&console_path);
        console.expect("Linux version", Duration::from_secs(120));
        console.expect("login: ", Duration::from_secs(400));
        // cloud-init may still be creating the user: try until it works.
        for attempt in 0..12 {
            std::thread::sleep(Duration::from_secs(if attempt == 0 { 2 } else { 10 }));
            console.send("\r");
            if !console.try_expect("login: ", Duration::from_secs(30)) {
                continue;
            }
            console.send(&format!("{username}\r"));
            if !console.try_expect("Password: ", Duration::from_secs(20)) {
                continue;
            }
            console.send(&format!("{password}\r"));
            std::thread::sleep(Duration::from_secs(3));
            console.send("echo GX_$(uname -r)_ok\r");
            if console.try_expect(&want, Duration::from_secs(20)) {
                return;
            }
        }
        panic!("could not log in and see {want}");
    })
    .await
    .unwrap();

    let (status, body) = request(
        &app,
        "POST",
        &format!("/vms/{id}/stop?graceful_timeout_secs=120&wait=200"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for i in [&kernel_id, &image_id] {
        let (status, body) = request(&app, "DELETE", &format!("/images/{i}?wait=60"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, cloud-hypervisor and GLIDEX_TEST_{KERNEL,INITRD,IMAGE}"]
async fn kernel_image_boot_with_generated_cloud_init() {
    kernel_image_e2e("cloudhypervisor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, qemu-system-x86_64 and GLIDEX_TEST_{KERNEL,INITRD,IMAGE}"]
async fn qemu_kernel_image_boot_with_generated_cloud_init() {
    kernel_image_e2e("qemu").await;
}

/// A kernel image (spec/kernel-boot.md K1, K2) copied from the host files
/// `GLIDEX_TEST_KERNEL` and `GLIDEX_TEST_INITRD` boots the cloud image
/// `GLIDEX_TEST_IMAGE` with its own command line, a generated seed and a
/// stored credential: log in, check the running kernel is the image's,
/// shut down on the power button.
async fn kernel_image_e2e(hypervisor: &str) {
    let kernel = env_path("GLIDEX_TEST_KERNEL", None);
    let initrd = env_path("GLIDEX_TEST_INITRD", None);
    let source_image = env_path("GLIDEX_TEST_IMAGE", None);
    assert!(Path::new("/dev/kvm").exists(), "/dev/kvm is required");

    let (app, manager, tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));
    let (status, k) = request(
        &app,
        "POST",
        "/images",
        Some(json!({
            "kernel_path": kernel, "initrd_path": initrd,
            "cmdline": "root=LABEL=cloudimg-rootfs ro console=ttyS0", "name": "test-kernel"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{k}");
    let release = k["image"]["kernel"]["release"]
        .as_str()
        .expect("a bzImage carries its release")
        .to_string();

    let rootfs = tmp.path().join("rootfs.img");
    run_ok(
        Command::new("cp")
            .arg("--sparse=always")
            .arg(&source_image)
            .arg(&rootfs),
    );
    std::fs::set_permissions(&rootfs, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let username = "gxkernel";
    let password = uuid::Uuid::new_v4().simple().to_string();
    let (status, body) = request(
        &app,
        "POST",
        "/credentials",
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let hostname = "gx-kernel";
    let (status, vm) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({
            "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048,
            "hypervisor": hypervisor_pb(hypervisor), "kernel": "test-kernel",
            "rootfs_path": rootfs, "credential": username,
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    assert_eq!(vm_of(&vm)["boot"], "BOOT_MODE_KERNEL", "{vm}");
    let id = vm_id(&vm);
    let seed_path = manager.get_vm(&id).await.unwrap().default_cloud_init_path();
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/start?wait=120"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(Path::new(&seed_path).exists(), "seed image not generated");

    let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;
    let want = format!("GX_{release}_root");
    let console = tokio::task::spawn_blocking(move || {
        let mut console = Console::connect(&console_path);
        console.expect("Linux version", Duration::from_secs(60));
        console.expect("Cloud-init v.", Duration::from_secs(300));
        console.expect("finished", Duration::from_secs(120));
        std::thread::sleep(Duration::from_secs(2));
        console.send("\r");
        console.expect(&format!("{hostname} login: "), Duration::from_secs(60));
        console.send(&format!("{username}\r"));
        console.expect("Password: ", Duration::from_secs(30));
        console.send(&format!("{password}\r"));
        console.expect(
            &format!("{username}@{hostname}:~$ "),
            Duration::from_secs(60),
        );
        console.send("echo GX_$(uname -r)_$(sudo -n true && echo root)\r");
        console.expect(&want, Duration::from_secs(30));
        console
    })
    .await
    .unwrap();
    drop(console);

    let (status, body) = request(
        &app,
        "POST",
        &format!("/vms/{id}/stop?graceful_timeout_secs=120&wait=200"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(state(&body), "stopped");
    let log = String::from_utf8_lossy(
        &std::fs::read(glidex_control_plane::paths::vm_paths(&id).log).unwrap(),
    )
    .into_owned();
    assert!(
        log.contains("reboot: Power down"),
        "guest did not power off cleanly:\n{log}"
    );
    let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "DELETE", "/images/test-kernel?wait=30", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, cloud-hypervisor and GLIDEX_TEST_{KERNEL,INITRD,IMAGE}"]
async fn kernel_boot_with_initrd() {
    kernel_boot_e2e("cloudhypervisor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, qemu-system-x86_64 and GLIDEX_TEST_{KERNEL,INITRD,IMAGE}"]
async fn qemu_kernel_boot_with_initrd() {
    kernel_boot_e2e("qemu").await;
}

/// Direct kernel boot from host paths (spec/kernel-boot.md K0): a distro
/// kernel and its initramfs mount the cloud image's root partition, on the
/// serial port by default and on the virtio console with `console=hvc0`.
/// `GLIDEX_TEST_KERNEL` / `GLIDEX_TEST_INITRD` are the image build's own
/// (Ubuntu publishes them in `unpacked/`); the image must have the
/// `cloudimg-rootfs` label. No seed or NIC: the boot is checked up to
/// systemd, then the VM is stopped.
async fn kernel_boot_e2e(hypervisor: &str) {
    let kernel = env_path("GLIDEX_TEST_KERNEL", None);
    let initrd = env_path("GLIDEX_TEST_INITRD", None);
    let source_image = env_path("GLIDEX_TEST_IMAGE", None);
    assert!(Path::new("/dev/kvm").exists(), "/dev/kvm is required");

    let (app, manager, tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));
    let rootfs = tmp.path().join("rootfs.img");
    run_ok(
        Command::new("cp")
            .arg("--sparse=always")
            .arg(&source_image)
            .arg(&rootfs),
    );
    std::fs::set_permissions(&rootfs, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();

    for console_dev in ["ttyS0", "hvc0"] {
        let name = format!("gx-kernel-{console_dev}");
        let (status, vm) = request(
            &app,
            "POST",
            "/vms",
            Some(json!({
                "name": name,
                "vcpu_count": 2,
                "mem_size_mib": 2048,
                "hypervisor": hypervisor_pb(hypervisor),
                "kernel_image_path": kernel,
                "initrd_path": initrd,
                "kernel_args": format!("console={console_dev} root=LABEL=cloudimg-rootfs ro panic=-1"),
                "rootfs_path": rootfs,
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{vm}");
        let id = vm_id(&vm);
        let (status, body) =
            request(&app, "POST", &format!("/vms/{id}/start?wait=120"), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(state(&body), "running");

        // The kernel's first line on the console (serial: from the very
        // start; virtio: replayed from the console's history), the initrd's
        // root mount, then systemd.
        let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;
        tokio::task::spawn_blocking(move || {
            let mut console = Console::connect(&console_path);
            if console_dev == "ttyS0" {
                console.expect("Linux version", Duration::from_secs(60));
            }
            console.expect("Welcome to ", Duration::from_secs(180));
        })
        .await
        .unwrap();

        let (status, body) = request(
            &app,
            "POST",
            &format!("/vms/{id}/stop?graceful_timeout_secs=5&wait=120"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(state(&body), "stopped");
        let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, cloud-hypervisor, a built glidex-vm-shim and GLIDEX_TEST_IMAGE"]
async fn vm_survives_control_plane_restart() {
    restart_e2e("cloudhypervisor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM; needs KVM, QEMU, OVMF, a built glidex-vm-shim and GLIDEX_TEST_IMAGE"]
async fn qemu_vm_survives_control_plane_restart() {
    restart_e2e("qemu").await;
}

/// Log in on a fresh console connection to instance `instance_id`. The
/// log is appended across instances (spec §8.6), so skip the replay up to
/// this instance's separator line first: earlier boots' prompts are in it.
fn console_login(
    path: &str,
    instance_id: &str,
    hostname: &str,
    username: &str,
    password: &str,
) -> Console {
    let mut console = Console::connect(path);
    console.expect(
        &format!("--- glidex: instance {instance_id} started"),
        Duration::from_secs(30),
    );
    std::thread::sleep(Duration::from_secs(1));
    console.send("\r");
    console.expect(&format!("{hostname} login: "), Duration::from_secs(240));
    console.send(&format!("{username}\r"));
    console.expect("Password: ", Duration::from_secs(30));
    console.send(&format!("{password}\r"));
    console.expect(
        &format!("{username}@{hostname}:~$ "),
        Duration::from_secs(60),
    );
    console
}

/// Wait until `f` holds for the VM.
async fn wait_vm(
    manager: &VmManager,
    id: &str,
    what: &str,
    timeout: Duration,
    f: impl Fn(&glidex_control_plane::models::Vm) -> bool,
) -> glidex_control_plane::models::Vm {
    let deadline = Instant::now() + timeout;
    loop {
        let vm = manager.get_vm(id).await.unwrap();
        if f(&vm) {
            return vm;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            vm.status
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// spec/reconciliation.md §19: survival, adoption, crash restart and a
/// guest power-off.
async fn restart_e2e(hypervisor: &str) {
    use glidex_vm_shim::util::same_process;
    let firmware = test_firmware(hypervisor);
    let source_image = env_path("GLIDEX_TEST_IMAGE", None);
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("test.db");
    // Raw: this test SIGKILLs the hypervisor, which a qcow2 image may not
    // survive (Cloud Hypervisor then refuses to reopen it).
    let rootfs = tmp.path().join("rootfs.raw");
    run_ok(
        Command::new("qemu-img")
            .args(["convert", "-O", "raw"])
            .arg(&source_image)
            .arg(&rootfs),
    );
    let (username, hostname) = ("gxtester", "gx-restart");
    let password = uuid::Uuid::new_v4().simple().to_string();

    // ---- control plane #1: create, start, log in -------------------------
    let manager = VmManager::with_db_path(db.clone()).unwrap();
    manager.initialize().await.unwrap();
    manager.start_controllers();
    let guard = ShutdownGuard(Some(manager.clone()));
    let app = create_router(manager.clone());
    let (status, body) = request(
        &app,
        "POST",
        "/credentials",
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, vm) = request(&app, "POST", "/vms?wait=120", Some(json!({
        "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": hypervisor_pb(hypervisor),
        "firmware_path": firmware, "rootfs_path": rootfs, "credential": username, "power": "POWER_STATE_RUNNING",
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    assert_eq!(state(&vm), "running", "{vm}");
    let id = vm_id(&vm);
    let before = manager.get_vm(&id).await.unwrap().status.instance.unwrap();
    let hv = (
        before.hypervisor_pid.unwrap(),
        before.hypervisor_starttime.unwrap(),
    );
    let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;
    let mut console = {
        let (p, h, u, pw) = (
            console_path.clone(),
            hostname.to_string(),
            username.to_string(),
            password.clone(),
        );
        let inst = before.instance_id.clone();
        tokio::task::spawn_blocking(move || {
            let mut c = console_login(&p, &inst, &h, &u, &pw);
            c.send("echo GX_BEFORE_$((6*7))\r");
            c.expect("GX_BEFORE_42", Duration::from_secs(30));
            c
        })
        .await
        .unwrap()
    };

    // ---- the control plane goes away; the guest does not -----------------
    manager.stop_controllers().await;
    guard.disarm();
    drop(app);
    drop(manager);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        same_process(hv.0, hv.1).unwrap(),
        "the hypervisor died with the control plane"
    );

    // ---- control plane #2 adopts it ---------------------------------------
    let manager = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match VmManager::with_db_path(db.clone()) {
                Ok(m) => break m,
                Err(e) if Instant::now() < deadline => {
                    eprintln!("database still open: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(e) => panic!("{e}"),
            }
        }
    };
    manager.initialize().await.unwrap();
    manager.start_controllers();
    let _guard = ShutdownGuard(Some(manager.clone()));
    let app = create_router(manager.clone());
    let vm = wait_vm(&manager, &id, "adoption", Duration::from_secs(30), |v| {
        v.is_converged()
    })
    .await;
    let after = vm.status.instance.clone().unwrap();
    assert_eq!(
        after.instance_id, before.instance_id,
        "a new instance was launched"
    );
    assert_eq!(after.hypervisor_pid, before.hypervisor_pid);
    let events = manager.vm_events(&id).unwrap();
    assert!(events.iter().any(|e| e.reason == "Adopted"), "{events:?}");
    // The console connection held across the restart still works.
    console = tokio::task::spawn_blocking(move || {
        console.send("echo GX_AFTER_$((6*8))\r");
        console.expect("GX_AFTER_48", Duration::from_secs(30));
        console
    })
    .await
    .unwrap();
    drop(console);

    // ---- a crash is restarted (restart_policy on_failure) -----------------
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(hv.0 as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    let vm = wait_vm(
        &manager,
        &id,
        "crash restart",
        Duration::from_secs(120),
        |v| {
            v.status
                .instance
                .as_ref()
                .is_some_and(|i| i.instance_id != before.instance_id)
                && v.is_converged()
        },
    )
    .await;
    let exit = vm.status.last_exit.clone().unwrap();
    assert_eq!(
        (exit.cause, exit.signal),
        (glidex_control_plane::models::ExitCause::Crashed, Some(9)),
        "{exit:?}"
    );
    assert_eq!(vm.status.restart_count, 1);
    assert_eq!(
        vm.spec.power,
        glidex_control_plane::models::PowerState::Running
    );

    // ---- the guest powers itself off: desired state follows ---------------
    {
        let (p, h, u, pw) = (
            console_path.clone(),
            hostname.to_string(),
            username.to_string(),
            password.clone(),
        );
        let inst = vm.status.instance.as_ref().unwrap().instance_id.clone();
        tokio::task::spawn_blocking(move || {
            let mut c = console_login(&p, &inst, &h, &u, &pw);
            c.send("sudo poweroff\r");
        })
        .await
        .unwrap();
    }
    let vm = wait_vm(
        &manager,
        &id,
        "guest poweroff",
        Duration::from_secs(120),
        |v| v.spec.power == glidex_control_plane::models::PowerState::Stopped && v.is_converged(),
    )
    .await;
    assert_eq!(
        vm.status.last_exit.as_ref().unwrap().cause,
        glidex_control_plane::models::ExitCause::CleanExit
    );
    let log = String::from_utf8_lossy(
        &std::fs::read(glidex_control_plane::paths::vm_paths(&id).log).unwrap(),
    )
    .into_owned();
    // Appended across instances, with separators (§8.6).
    assert!(log.matches("--- glidex: instance").count() >= 4, "{log}");
    assert!(
        log.contains(&format!(
            "--- glidex: instance {} exited: crashed (signal 9) ---",
            before.instance_id
        )),
        "{log}"
    );

    let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    request(&app, "DELETE", &format!("/credentials/{username}"), None).await;
}

/// Host's IPv4 default gateway (from `ip -j route`), used as a target that
/// is reachable only through NAT.
fn host_default_gateway() -> Option<String> {
    let out = Command::new("ip")
        .args(["-4", "-j", "route", "show", "default"])
        .output()
        .ok()?;
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
    let (app, manager, tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));
    request(&app, "DELETE", "/networks/mixed", None).await;
    let (st, net) = request(
        &app,
        "POST",
        "/networks",
        Some(json!({"name": "mixed", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-mixed"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{net}");

    let ch = start_and_login(
        &app,
        &tmp,
        "mixed",
        "gx-mix-ch",
        json!({"hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR"}),
    )
    .await;
    let qemu = start_and_login(
        &app,
        &tmp,
        "mixed",
        "gx-mix-qemu",
        json!({"hypervisor": "HYPERVISOR_QEMU"}),
    )
    .await;
    let (ch_ip, qemu_ip) = (ch.ipv4.clone().unwrap(), qemu.ipv4.clone().unwrap());
    let qemu = run_checks(
        qemu,
        vec![(
            format!("ping -c 2 -W 3 {ch_ip} >/dev/null && echo GX_$((60+1))_TO_CH"),
            "GX_61_TO_CH".into(),
        )],
    )
    .await;
    let ch = run_checks(
        ch,
        vec![(
            format!("ping -c 2 -W 3 {qemu_ip} >/dev/null && echo GX_$((60+2))_TO_QEMU"),
            "GX_62_TO_QEMU".into(),
        )],
    )
    .await;
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

    let (app, manager, tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));

    let (_, status) = request(&app, "GET", "/ovs/status", None).await;
    assert_eq!(
        status["netd"]["access"], "NETD_ACCESS_FULL",
        "glidex-netd not usable: {status}"
    );
    assert_eq!(
        status["host"]["ovs_running"], true,
        "Open vSwitch not running: {status}"
    );

    // Leftovers from an interrupted run.
    request(&app, "DELETE", "/networks/e2e", None).await;

    let (st, net) = request(
        &app,
        "POST",
        "/networks",
        Some(json!({"name": "e2e", "mode": "NETWORK_MODE_NAT", "bridge": "gxbr-e2e"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{net}");

    let rootfs = tmp.path().join("rootfs.raw");
    run_ok(
        Command::new("cp")
            .arg("--sparse=always")
            .arg(&source_image)
            .arg(&rootfs),
    );
    let username = "gxnet";
    let password = uuid::Uuid::new_v4().simple().to_string();
    let (st, body) = request(
        &app,
        "POST",
        "/credentials",
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");

    let hostname = "gx-nattest";
    let (st, vm) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({
            "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048,
            "hypervisor": hypervisor_pb(hypervisor),
            "firmware_path": firmware, "rootfs_path": rootfs,
            "credential": username,
            "networks": [{"network": "e2e", "queue_pairs": 2}],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{vm}");
    let id = vm_id(&vm);

    let (st, body) = request(&app, "POST", &format!("/vms/{id}/start?wait=120"), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let ipv4 = vm_of(&body)["nics"][0]["ipv4"]
        .as_str()
        .expect("NAT reservation")
        .to_string();
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
        c.expect(
            &format!("{username}@{hostname}:~$ "),
            Duration::from_secs(60),
        );
        // DHCP from netd's dnsmasq gave the reserved address (whatever
        // the guest named the interface).
        c.send(&format!(
            "ip -4 -o addr | grep -q ' {expected_ip}/24 ' && echo GX_$((40+1))_DHCP_OK\r"
        ));
        c.expect("GX_41_DHCP_OK", Duration::from_secs(30));
        // Masquerade: reach the host's own default gateway.
        c.send(&format!(
            "ping -c 2 -W 3 {gateway} >/dev/null && echo GX_$((40+2))_NAT_OK\r"
        ));
        c.expect("GX_42_NAT_OK", Duration::from_secs(30));
        // queue_pairs: 2 reached the guest as a multiqueue NIC.
        c.send("ls -d /sys/class/net/*/queues/rx-1 >/dev/null && echo GX_$((40+3))_MQ_OK\r");
        c.expect("GX_43_MQ_OK", Duration::from_secs(30));
        c.send("sudo poweroff\r");
    })
    .await
    .unwrap();

    // The guest powered itself off: the VM ends up stopped (its desired
    // state too: CleanExit, §7.5), port released.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let vm = manager.get_vm(&id).await.unwrap();
        if vm.state() == glidex_control_plane::models::VmState::Stopped && vm.status.nics.is_empty()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "VM still running after guest poweroff"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let (_, body) = request(&app, "GET", &format!("/vms/{id}"), None).await;
    assert_eq!(state(&body), "stopped", "{body}");
    let vm = vm_of(&body);
    assert_eq!(
        short(&vm["desired_state"], "POWER_STATE_"),
        "stopped",
        "{body}"
    );
    assert_eq!(
        short(&vm["last_exit"]["cause"], "EXIT_CAUSE_"),
        "clean_exit",
        "{body}"
    );
    assert!(vm["nics"][0]["port"].is_null(), "{body}");

    let (st, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, body) = request(&app, "DELETE", "/networks/e2e", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    let (_, bridges) = request(&app, "GET", "/ovs/bridges", None).await;
    assert!(
        !bridges["bridges"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|b| b["spec"]["name"] == "gxbr-e2e"),
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
async fn start_and_login(
    app: &Router,
    tmp: &TempDir,
    network: &str,
    hostname: &str,
    extra: Value,
) -> Guest {
    let hypervisor = extra["hypervisor"]
        .as_str()
        .unwrap_or("cloudhypervisor")
        .to_string();
    let firmware = test_firmware(&hypervisor);
    let source_image = env_path("GLIDEX_TEST_IMAGE", None);
    let rootfs = tmp.path().join(format!("{hostname}.raw"));
    run_ok(
        Command::new("cp")
            .arg("--sparse=always")
            .arg(&source_image)
            .arg(&rootfs),
    );
    let username = format!(
        "gx{}",
        hostname
            .replace('-', "")
            .chars()
            .rev()
            .take(6)
            .collect::<String>()
    );
    let password = uuid::Uuid::new_v4().simple().to_string();
    request(app, "DELETE", &format!("/credentials/{username}"), None).await;
    let (st, body) = request(
        app,
        "POST",
        "/credentials",
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let mut spec = json!({
        "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": hypervisor_pb(&hypervisor),
        "firmware_path": firmware, "rootfs_path": rootfs, "credential": username,
        "networks": [{"network": network}],
    });
    for (k, v) in extra.as_object().cloned().unwrap_or_default() {
        spec[k] = v;
    }
    let (st, vm) = request(app, "POST", "/vms", Some(spec)).await;
    assert_eq!(st, StatusCode::CREATED, "{vm}");
    let id = vm_id(&vm);
    let (st, body) = request(app, "POST", &format!("/vms/{id}/start?wait=120"), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let ipv4 = vm_of(&body)["nics"][0]["ipv4"].as_str().map(str::to_string);
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
    Guest {
        id,
        console,
        ipv4,
        username,
    }
}

/// Run `checks` (command, expected marker) on a guest's console.
async fn run_checks(guest: Guest, checks: Vec<(String, String)>) -> Guest {
    let Guest {
        id,
        mut console,
        ipv4,
        username,
    } = guest;
    let console = tokio::task::spawn_blocking(move || {
        for (cmd, marker) in checks {
            console.send(&format!("{cmd}\r"));
            console.expect(&marker, Duration::from_secs(40));
        }
        console
    })
    .await
    .unwrap();
    Guest {
        id,
        console,
        ipv4,
        username,
    }
}

async fn stop_and_delete(app: &Router, guest: Guest) {
    drop(guest.console);
    request(
        app,
        "POST",
        &format!("/vms/{}/stop?wait=60", guest.id),
        None,
    )
    .await;
    let (st, _) = request(app, "DELETE", &format!("/vms/{}", guest.id), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    request(
        app,
        "DELETE",
        &format!("/credentials/{}", guest.username),
        None,
    )
    .await;
}

/// Boot a VM on `network`, log in, run `checks`, then delete the VM.
async fn boot_and_check(
    app: &Router,
    tmp: &TempDir,
    network: &str,
    hostname: &str,
    extra: Value,
    checks: Vec<(String, String)>,
) {
    let guest = start_and_login(app, tmp, network, hostname, extra).await;
    let guest = run_checks(guest, checks).await;
    stop_and_delete(app, guest).await;
}

async fn require_lan_and_netd(app: &Router) {
    assert!(
        std::env::var("GLIDEX_TEST_LAN").is_ok(),
        "set GLIDEX_TEST_LAN=1 after running sudo scripts/dev/fake-lan-setup.sh"
    );
    assert!(
        host_ip(&["-br", "link", "show", "gxup0"]).contains("gxup0"),
        "fake LAN missing (gxup0)"
    );
    let (_, status) = request(app, "GET", "/ovs/status", None).await;
    assert_eq!(status["netd"]["access"], "NETD_ACCESS_FULL", "{status}");
    assert_eq!(status["host"]["ovs_running"], true, "{status}");
}

/// The uplink tests own their bridge. Never touch one that carries anything
/// but the test's own fake-LAN NIC (a host's real uplink, say): its
/// pre-cleanup deletes the bridge.
async fn refuse_foreign_bridge(app: &Router, bridge: &str, own: &str) {
    let (_, list) = request(app, "GET", "/ovs/bridges", None).await;
    let Some(b) = list["bridges"]
        .as_array()
        .and_then(|l| l.iter().find(|b| b["spec"]["name"] == bridge))
    else {
        return;
    };
    let ports: Vec<&str> = b["live"]["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["name"].as_str().or(p.as_str()))
        .filter(|p| *p != bridge && *p != own)
        .collect();
    assert!(
        ports.is_empty(),
        "bridge {bridge} exists with other ports {ports:?}: not the test's, refusing to touch it"
    );
}

/// M6 acceptance: kernel uplink on an in-use NIC with IP migration —
/// refused, rolled back without commit, committed, VM on the LAN, restored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs root-run glidex-netd, OVS, the fake LAN (GLIDEX_TEST_LAN) and GLIDEX_TEST_IMAGE"]
async fn bridged_uplink_e2e() {
    let (app, manager, tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));
    require_lan_and_netd(&app).await;
    refuse_foreign_bridge(&app, "gxbr-e2eup", "gxup0").await;
    // Leftovers from an interrupted run.
    request(&app, "DELETE", "/networks/lan", None).await;
    request(
        &app,
        "DELETE",
        "/ovs/bridges/gxbr-e2eup/uplinks/gxup0",
        None,
    )
    .await;
    request(&app, "DELETE", "/ovs/bridges/gxbr-e2eup", None).await;
    assert!(
        host_ip(&["-4", "-o", "addr", "show", "dev", "gxup0"]).contains("192.0.2.10/24"),
        "gxup0 should hold 192.0.2.10"
    );

    let (st, body) = request(
        &app,
        "POST",
        "/ovs/bridges",
        Some(json!({"name": "gxbr-e2eup", "datapath": "DATAPATH_SYSTEM"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let uplink = json!({"name": "gxup0", "kind": "UPLINK_KIND_KERNEL", "ifname": "gxup0"});

    // 1. In use by the host: refused without migrate_ip + confirm.
    let (st, body) = request(
        &app,
        "POST",
        "/ovs/bridges/gxbr-e2eup/uplinks",
        Some(uplink.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "host_interface_in_use");
    assert!(
        body["details"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r == "has_addresses"),
        "{body}"
    );

    // 2. Migrated but never committed: rolled back by netd (10 s window in the dev config).
    let mut migrate = uplink.clone();
    migrate["migrate_ip"] = json!(true);
    migrate["confirm"] = json!(true);
    let (st, body) = request(
        &app,
        "POST",
        "/ovs/bridges/gxbr-e2eup/uplinks",
        Some(migrate.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    assert!(
        host_ip(&["-4", "-o", "addr", "show", "dev", "gxbr-e2eup"]).contains("192.0.2.10/24"),
        "address moved to the bridge"
    );
    assert!(
        Command::new("ping")
            .args(["-c", "1", "-W", "2", "192.0.2.1"])
            .status()
            .unwrap()
            .success(),
        "LAN reachable through the bridge"
    );
    // netd deletes the record as the last rollback step, so wait for that.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (_, list) = request(&app, "GET", "/ovs/bridges/gxbr-e2eup/uplinks", None).await;
        // `{}` once none are left: an empty list is left out.
        if list["uplinks"].as_array().is_none_or(|l| l.is_empty()) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "not rolled back: {list}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        host_ip(&["-4", "-o", "addr", "show", "dev", "gxup0"]).contains("192.0.2.10/24"),
        "address restored"
    );
    assert!(
        host_ip(&["-4", "route", "show", "198.51.100.0/24"]).contains("dev gxup0"),
        "route restored"
    );

    // 3. Migrated and committed.
    let (st, body) = request(
        &app,
        "POST",
        "/ovs/bridges/gxbr-e2eup/uplinks",
        Some(migrate),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{body}");
    let token = body["uplink"]["record"]["pending"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, body) = request(
        &app,
        "POST",
        "/ovs/bridges/gxbr-e2eup/uplinks/gxup0/commit",
        Some(json!({"token": token})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["uplink"]["phase"], "UPLINK_PHASE_ACTIVE", "{body}");
    assert!(
        host_ip(&["-4", "route", "show", "198.51.100.0/24"]).contains("dev gxbr-e2eup"),
        "route moved"
    );

    // 4. A VM on the bridged network gets a LAN address and reaches the LAN and the host.
    let (st, body) = request(
        &app,
        "POST",
        "/networks",
        Some(json!({"name": "lan", "mode": "NETWORK_MODE_BRIDGED", "bridge": "gxbr-e2eup"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    boot_and_check(
        &app,
        &tmp,
        "lan",
        "gx-lantest",
        json!({}),
        vec![
            (
                "ip -4 -o addr | grep -q ' 192.0.2.1[0-9][0-9]/24 ' && echo GX_$((60+1))_LAN_DHCP"
                    .into(),
                "GX_61_LAN_DHCP".into(),
            ),
            (
                "ping -c 2 -W 3 192.0.2.1 >/dev/null && echo GX_$((60+2))_LAN_GW".into(),
                "GX_62_LAN_GW".into(),
            ),
            (
                "ping -c 2 -W 3 192.0.2.10 >/dev/null && echo GX_$((60+3))_HOST".into(),
                "GX_63_HOST".into(),
            ),
        ],
    )
    .await;

    // 5. Cleanup restores the NIC.
    let (st, _) = request(&app, "DELETE", "/networks/lan", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, body) = request(
        &app,
        "DELETE",
        "/ovs/bridges/gxbr-e2eup/uplinks/gxup0",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    assert!(
        host_ip(&["-4", "-o", "addr", "show", "dev", "gxup0"]).contains("192.0.2.10/24"),
        "address back on gxup0"
    );
    assert!(
        host_ip(&["-4", "route", "show", "198.51.100.0/24"]).contains("dev gxup0"),
        "route back on gxup0"
    );
    let (st, _) = request(&app, "DELETE", "/ovs/bridges/gxbr-e2eup", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

/// M8 acceptance (combination D): AF_XDP uplink in generic mode on a
/// userspace-datapath bridge, tap VM port, DHCP from the LAN.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs root-run glidex-netd, OVS with afxdp, the fake LAN (GLIDEX_TEST_LAN) and GLIDEX_TEST_IMAGE"]
async fn afxdp_uplink_e2e() {
    let (app, manager, tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));
    require_lan_and_netd(&app).await;
    refuse_foreign_bridge(&app, "gxbr-e2exdp", "gxup1").await;
    request(&app, "DELETE", "/networks/xdp", None).await;
    request(
        &app,
        "DELETE",
        "/ovs/bridges/gxbr-e2exdp/uplinks/gxup1",
        None,
    )
    .await;
    request(&app, "DELETE", "/ovs/bridges/gxbr-e2exdp", None).await;

    let (st, body) = request(
        &app,
        "POST",
        "/ovs/bridges",
        Some(json!({"name": "gxbr-e2exdp", "datapath": "DATAPATH_NETDEV"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let (st, body) = request(
        &app,
        "POST",
        "/ovs/bridges/gxbr-e2exdp/uplinks",
        Some(json!({
            "name": "gxup1", "kind": "UPLINK_KIND_AFXDP", "ifname": "gxup1", "xdp_mode": "XDP_MODE_GENERIC"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert!(
        body["uplink"]["live"]["error"].is_null(),
        "AF_XDP port error: {body}"
    );

    let (st, body) = request(
        &app,
        "POST",
        "/networks",
        Some(json!({"name": "xdp", "mode": "NETWORK_MODE_BRIDGED", "bridge": "gxbr-e2exdp"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    boot_and_check(
        &app,
        &tmp,
        "xdp",
        "gx-xdptest",
        json!({}),
        vec![
            (
                "ip -4 -o addr | grep -q ' 198.18.0.1[0-9][0-9]/24 ' && echo GX_$((70+1))_XDP_DHCP"
                    .into(),
                "GX_71_XDP_DHCP".into(),
            ),
            (
                "ping -c 2 -W 3 198.18.0.1 >/dev/null && echo GX_$((70+2))_XDP_GW".into(),
                "GX_72_XDP_GW".into(),
            ),
        ],
    )
    .await;

    request(&app, "DELETE", "/networks/xdp", None).await;
    let (st, _) = request(
        &app,
        "DELETE",
        "/ovs/bridges/gxbr-e2exdp/uplinks/gxup1",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = request(&app, "DELETE", "/ovs/bridges/gxbr-e2exdp", None).await;
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
    assert!(
        std::env::var("GLIDEX_TEST_DPDK").is_ok(),
        "set GLIDEX_TEST_DPDK=1 once OVS-DPDK is initialized"
    );
    let (app, manager, tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));
    let (_, status) = request(&app, "GET", "/ovs/status", None).await;
    assert_eq!(status["host"]["dpdk_initialized"], true, "{status}");
    assert!(
        status["host"]["iface_types"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "dpdkvhostuserclient"),
        "{status}"
    );
    request(&app, "DELETE", "/networks/fast", None).await;

    let (st, net) = request(
        &app,
        "POST",
        "/networks",
        Some(json!({
            "name": "fast", "mode": "NETWORK_MODE_NAT", "port_type": "PORT_TYPE_VHOST_USER", "bridge": "gxbr-fast"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{net}");
    let (_, bridges) = request(&app, "GET", "/ovs/bridges", None).await;
    let fast = bridges["bridges"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|b| b["spec"]["name"] == "gxbr-fast")
        .unwrap()
        .clone();
    assert_eq!(fast["spec"]["datapath"], "DATAPATH_NETDEV", "{fast}");

    let (_, net) = request(&app, "GET", "/networks/fast", None).await;
    assert_eq!(net["network"]["port_type"], "PORT_TYPE_VHOST_USER", "{net}");
    // OVS-DPDK's vhost-user backend needs hugepage-backed guest memory.
    boot_and_check(&app, &tmp, "fast", "gx-vhosttest", json!({"hugepages": true, "mem_size_mib": 1024, "hypervisor": hypervisor_pb(hypervisor)}), vec![
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

/// The same on QEMU: an `image` VM boots through the `ovmf` firmware
/// image (the host's OVMF) by default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "downloads a cloud image and boots it; needs network, KVM, QEMU, OVMF and qemu-img/qemu-io/sgdisk/growpart"]
async fn qemu_catalog_image_boots_and_root_grows() {
    catalog_image_e2e("qemu").await;
}

async fn catalog_image_e2e(hypervisor: &str) {
    let key = std::env::var("GLIDEX_TEST_CATALOG").unwrap_or_else(|_| "ubuntu-26.04".into());
    let (app, manager, _tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));

    // The firmware the VM boots through by default (spec/images.md §7):
    // copied from the installer's pinned file or the host's OVMF, or
    // downloaded.
    let fw_key = if hypervisor == "qemu" {
        "ovmf"
    } else {
        "cloudhv-edk2"
    };
    let (status, fw) = request(&app, "POST", "/images", Some(json!({"firmware": fw_key}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{fw}");
    let fw_id = fw["image"]["id"].as_str().unwrap().to_string();
    let fw = loop {
        let (_, fw) = request(&app, "GET", &format!("/images/{fw_id}"), None).await;
        let fw = fw["image"].clone();
        if !matches!(
            fw["status"]["state"].as_str(),
            Some("IMAGE_STATE_DOWNLOADING" | "IMAGE_STATE_VERIFYING")
        ) {
            break fw;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    assert_eq!(fw["status"]["state"], "IMAGE_STATE_READY", "{fw}");

    let (status, img) = request(&app, "POST", "/images", Some(json!({"catalog": key}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let image_id = img["image"]["id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(1800);
    let img = loop {
        let (_, img) = request(&app, "GET", &format!("/images/{image_id}"), None).await;
        let img = img["image"].clone();
        match img["status"]["state"].as_str().unwrap() {
            "IMAGE_STATE_DOWNLOADING" | "IMAGE_STATE_VERIFYING" => {}
            _ => break img,
        }
        assert!(Instant::now() < deadline, "download timed out: {img}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    assert_eq!(img["verified"], true);
    eprintln!("pulled {key}: {}", img["source"]);

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

    let hostname = "gx-imgtest";
    let spec = json!({
        "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": hypervisor_pb(hypervisor),
        "image": image_id, "root_disk_size_gib": 12, "credential": username,
    });
    let (status, vm) = request(&app, "POST", "/vms", Some(spec)).await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    let id = vm_id(&vm);
    let config = manager.get_vm(&id).await.unwrap().spec.config;
    assert_eq!(
        config.firmware_image.as_deref(),
        Some(fw_id.as_str()),
        "image VMs default to the firmware image"
    );
    let root = vm_of(&vm)["root_disk"].as_str().unwrap().to_string();
    // glidex extended the root partition offline, before the first boot.
    assert!(vm_of(&vm).get("warnings").is_none(), "{vm}");
    // The disk controller makes the disk after the create returns.
    let mut disk = Value::Null;
    for _ in 0..240 {
        disk = request(&app, "GET", &format!("/disks/{root}"), None)
            .await
            .1["disk"]
            .clone();
        if disk["busy_op"].is_null() && disk["phase"] != "DISK_PHASE_PENDING" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert_eq!(
        num(&disk["partition_table"]["free_tail_bytes"]),
        0,
        "{disk}"
    );

    // Boot, log in, and check the root filesystem against `min_gib`.
    let boot_and_check = |min_gib: u32, marker: &'static str| {
        let (app, id) = (app.clone(), id.clone());
        let (username, password) = (username.to_string(), password.clone());
        async move {
            let (status, body) =
                request(&app, "POST", &format!("/vms/{id}/start?wait=120"), None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;
            tokio::task::spawn_blocking(move || {
                let mut console = Console::connect(&console_path);
                // The first boot's prompts are in the history at the second.
                console.skip_history();
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
            let (status, body) =
                request(&app, "POST", &format!("/vms/{id}/stop?wait=60"), None).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
    };

    boot_and_check(11, "GX_ROOT12").await;

    // Applied by the disk controller: `?wait` answers once it has (200).
    let (status, d) = request(
        &app,
        "POST",
        &format!("/disks/{root}/resize?wait=120"),
        Some(json!({"size_gib": 16})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    // Grown, root partition included (offline: nothing left for the boot);
    // the guest's own view of it is checked next.
    assert_eq!(d["settled"], true, "{d}");
    let d = &d["disk"];
    assert_eq!(num(&d["size_bytes"]), 16u64 << 30, "{d}");
    // An optional bool: sent when set, absent otherwise.
    assert_ne!(d["pending_growpart"], true, "{d}");

    boot_and_check(15, "GX_ROOT16").await;

    let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "GET", &format!("/disks/{root}"), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "owned root disk is deleted with the VM"
    );
    let (status, _) = request(&app, "DELETE", &format!("/images/{image_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots two real VMs with GPUs; needs KVM, cloud-hypervisor, GLIDEX_TEST_IMAGE and GLIDEX_TEST_GPUS"]
async fn numa_placement_e2e() {
    numa_e2e("cloudhypervisor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots two real VMs with GPUs; needs KVM, QEMU, OVMF, GLIDEX_TEST_IMAGE and GLIDEX_TEST_GPUS"]
async fn qemu_numa_placement_e2e() {
    numa_e2e("qemu").await;
}

/// The CPUs a thread may run on (`Cpus_allowed_list`).
fn allowed_cpus(pid: u32, tid: &str) -> Vec<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/status")).unwrap();
    let list = status
        .lines()
        .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
        .unwrap()
        .trim()
        .to_string();
    glidex_ovs::tuning::parse_cpu_list(&list)
}

/// A hypervisor's threads: (tid, name).
fn threads(pid: u32) -> Vec<(String, String)> {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .flatten()
        .map(|e| {
            let tid = e.file_name().to_string_lossy().into_owned();
            let comm = std::fs::read_to_string(e.path().join("comm"))
                .unwrap_or_default()
                .trim()
                .to_string();
            (tid, comm)
        })
        .collect()
}

/// Pages of a process per NUMA node, from `/proc/<pid>/numa_maps`.
fn pages_per_node(pid: u32) -> std::collections::BTreeMap<u32, u64> {
    let mut out = std::collections::BTreeMap::new();
    for line in std::fs::read_to_string(format!("/proc/{pid}/numa_maps"))
        .unwrap()
        .lines()
    {
        for f in line.split_whitespace() {
            if let Some((n, pages)) = f.strip_prefix('N').and_then(|r| r.split_once('='))
                && let (Ok(n), Ok(p)) = (n.parse::<u32>(), pages.parse::<u64>())
            {
                *out.entry(n).or_default() += p;
            }
        }
    }
    out
}

/// spec/host-topology.md HT2 acceptance: a `strict`, dedicated VM with a
/// GPU runs on the GPU's NUMA node, each vCPU on its own CPU and every
/// thread on the CPUs it holds, its memory on that node; a shared VM with
/// the second GPU keeps off those CPUs; a control-plane restart changes
/// none of it. `GLIDEX_TEST_GPUS`: two PCI addresses bound to vfio-pci, on
/// one NUMA node (e.g. `0000:87:00.0,0000:90:00.0`).
async fn numa_e2e(hypervisor: &str) {
    let gpus: Vec<String> = std::env::var("GLIDEX_TEST_GPUS")
        .expect("GLIDEX_TEST_GPUS: two vfio-pci GPUs")
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    assert_eq!(gpus.len(), 2, "GLIDEX_TEST_GPUS needs two addresses");
    let numa_of = |a: &str| -> u32 {
        std::fs::read_to_string(format!("/sys/bus/pci/devices/{a}/numa_node"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    let gpu_node = numa_of(&gpus[0]);
    let source_image = env_path("GLIDEX_TEST_IMAGE", None);
    assert!(Path::new("/dev/kvm").exists(), "/dev/kvm is required");

    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("test.db");
    let manager = VmManager::with_db_path(db.clone()).unwrap();
    manager.initialize().await.unwrap();
    manager.start_controllers();
    let guard = ShutdownGuard(Some(manager.clone()));
    let app = create_router(manager.clone());
    let firmware = pull_firmware_image(&app, hypervisor).await;
    let (username, password) = ("gxtester", uuid::Uuid::new_v4().simple().to_string());
    let (status, body) = request(
        &app,
        "POST",
        "/credentials",
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let rootfs = |name: &str| {
        let p = tmp.path().join(format!("{name}.raw"));
        run_ok(
            Command::new("qemu-img")
                .args(["convert", "-O", "raw"])
                .arg(&source_image)
                .arg(&p),
        );
        p
    };

    // ---- a strict, dedicated VM with the first GPU ------------------------
    let hostname = "gx-numa";
    let (status, vm) = request(&app, "POST", "/vms?wait=180", Some(json!({
        "name": hostname, "vcpu_count": 4, "mem_size_mib": 4096, "hypervisor": hypervisor_pb(hypervisor),
        "firmware": firmware, "rootfs_path": rootfs("a"), "credential": username,
        "numa_policy": "NUMA_POLICY_STRICT", "cpu_policy": "CPU_POLICY_DEDICATED",
        "vfio_devices": [format!("/sys/bus/pci/devices/{}", gpus[0])],
        "power": "POWER_STATE_RUNNING",
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    assert_eq!(state(&vm), "running", "{vm}");
    let id = vm_id(&vm);
    let a = manager.get_vm(&id).await.unwrap();
    let placement = a.status.placement.clone().unwrap();
    assert_eq!(placement.numa_node, Some(gpu_node), "{placement:?}");
    let held = placement.cpus.clone();
    assert_eq!(held.len(), 4, "two whole cores: {held:?}");
    let topology = glidex_control_plane::topology::read().unwrap();
    assert!(
        held.iter()
            .all(|c| topology.node_of_cpu(*c) == Some(gpu_node)),
        "{held:?} not all on node {gpu_node}"
    );
    let inst = a.status.instance.clone().unwrap();
    let pid = inst.hypervisor_pid.unwrap();

    // Every thread on the held CPUs; vCPU i alone on held[i]. Cloud
    // Hypervisor names its vCPU threads; QEMU's come from QMP.
    let api = glidex_control_plane::paths::vm_paths(&id).api_socket;
    let vcpu_tids: std::collections::BTreeMap<String, usize> = if hypervisor == "qemu" {
        glidex_hv_client::qmp::QmpClient::new(&api)
            .query_cpus_fast()
            .unwrap()
            .into_iter()
            .map(|(i, tid)| (tid.to_string(), i as usize))
            .collect()
    } else {
        threads(pid)
            .into_iter()
            .filter_map(|(tid, name)| {
                name.strip_prefix("vcpu")
                    .and_then(|i| i.parse().ok())
                    .map(|i| (tid, i))
            })
            .collect()
    };
    let vcpu_index = |tid: &str| vcpu_tids.get(tid).copied();
    let mut vcpus_seen = 0;
    for (tid, name) in threads(pid) {
        let allowed = allowed_cpus(pid, &tid);
        match vcpu_index(&tid) {
            Some(i) => {
                assert_eq!(allowed, vec![held[i]], "vCPU {i} ({name})");
                vcpus_seen += 1;
            }
            None => assert!(
                allowed.iter().all(|c| held.contains(c)),
                "thread {name} on {allowed:?}, outside {held:?}"
            ),
        }
    }
    assert_eq!(vcpus_seen, 4, "vCPU threads of {pid}");

    // The guest sees its GPU and four CPUs.
    let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;
    {
        let (p, inst_id, u, pw) = (
            console_path.clone(),
            inst.instance_id.clone(),
            username.to_string(),
            password.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let mut c = console_login(&p, &inst_id, hostname, &u, &pw);
            c.send("echo GX_NV_$(grep -l 0x10de /sys/bus/pci/devices/*/vendor | wc -l)_CPUS_$(nproc)\r");
            c.expect("GX_NV_1_CPUS_4", Duration::from_secs(30));
            // Touch guest memory so the host allocates it.
            c.send("head -c 1G /dev/zero | tail -c 1 >/dev/null; echo GX_TOUCHED\r");
            c.expect("GX_TOUCHED", Duration::from_secs(120));
        })
        .await
        .unwrap();
    }
    let pages = pages_per_node(pid);
    let total: u64 = pages.values().sum();
    let here = pages.get(&gpu_node).copied().unwrap_or(0);
    assert!(
        here * 10 >= total * 9,
        "only {here} of {total} pages on node {gpu_node}: {pages:?}"
    );

    // ---- a shared, strict VM with the second GPU keeps off those CPUs -----
    let (status, vm) = request(&app, "POST", "/vms?wait=180", Some(json!({
        "name": "gx-numa-shared", "vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": hypervisor_pb(hypervisor),
        "firmware": firmware, "rootfs_path": rootfs("b"), "credential": username,
        "numa_policy": "NUMA_POLICY_STRICT",
        "vfio_devices": [format!("/sys/bus/pci/devices/{}", gpus[1])],
        "power": "POWER_STATE_RUNNING",
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    assert_eq!(state(&vm), "running", "{vm}");
    let b_id = vm_id(&vm);
    let b = manager.get_vm(&b_id).await.unwrap();
    let bp = b.status.placement.clone().unwrap();
    assert_eq!(
        (bp.numa_node, bp.cpus.is_empty()),
        (Some(numa_of(&gpus[1])), true)
    );
    let b_pid = b.status.instance.clone().unwrap().hypervisor_pid.unwrap();
    for (tid, name) in threads(b_pid) {
        let allowed = allowed_cpus(b_pid, &tid);
        assert!(
            allowed.iter().all(|c| !held.contains(c)),
            "shared thread {name} may run on dedicated CPUs: {allowed:?}"
        );
    }

    // ---- the control plane restarts; the binding stays --------------------
    manager.stop_controllers().await;
    guard.disarm();
    drop(app);
    drop(manager);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let manager = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match VmManager::with_db_path(db.clone()) {
                Ok(m) => break m,
                Err(e) if Instant::now() < deadline => {
                    eprintln!("database still open: {e}");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(e) => panic!("{e}"),
            }
        }
    };
    manager.initialize().await.unwrap();
    manager.start_controllers();
    let _guard = ShutdownGuard(Some(manager.clone()));
    let vm = wait_vm(&manager, &id, "adoption", Duration::from_secs(30), |v| {
        v.is_converged()
    })
    .await;
    assert_eq!(vm.status.instance.unwrap().hypervisor_pid, Some(pid));
    for (tid, _) in threads(pid) {
        if let Some(i) = vcpu_index(&tid) {
            assert_eq!(
                allowed_cpus(pid, &tid),
                vec![held[i]],
                "after restart: vCPU {i}"
            );
        }
    }
    let app = create_router(manager.clone());
    for vm in [&b_id, &id] {
        let (status, body) = request(
            &app,
            "POST",
            &format!("/vms/{vm}/stop?graceful_timeout_secs=60&wait=120"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _) = request(&app, "DELETE", &format!("/vms/{vm}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
}

/// A cloud image booted from an LVM-thin volume (spec/storage.md §11.1):
/// a linked disk, a thin snapshot of the node's image volume, its root
/// partition grown, opened by the hypervisor as a raw block device. Needs,
/// besides KVM and a qcow2 cloud image (`GLIDEX_TEST_LVM_IMAGE`), a running
/// glidex-stord that allows the volume group and hands devices to this
/// test's user:
///
/// ```sh
/// GLIDEX_TEST_LVM_IMAGE=~/images/resolute-server-cloudimg-amd64.img \
///   GLIDEX_STORD_SOCKET=/run/gx-s6-test/stord/stord.sock GLIDEX_STORD_VG=gx-test \
///   GLIDEX_STORD_POOL=pool cargo test -p glidex-control-plane --test functional_tests \
///   lvm_root_disk_boots -- --ignored --nocapture
/// ```
async fn lvm_root_disk_e2e(hypervisor: &str) {
    let vg = std::env::var("GLIDEX_STORD_VG").expect("GLIDEX_STORD_VG");
    let pool = std::env::var("GLIDEX_STORD_POOL").expect("GLIDEX_STORD_POOL");
    root_disk_e2e(
        hypervisor,
        "GLIDEX_TEST_LVM_IMAGE",
        json!({"name": "boot-thin", "driver": "DATASTORE_DRIVER_LVM",
                "volume_group": vg, "thin_pool": pool}),
    )
    .await;
}

/// A cloud image booted from a datastore made from `datastore` (named
/// `boot-thin`): a linked 6 GiB disk of the qcow2 image in `image_env`,
/// its root partition grown into the volume.
async fn root_disk_e2e(hypervisor: &str, image_env: &str, datastore: Value) {
    use glidex_control_plane::store::{Origin, TableId};
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let source = env_path(image_env, None);
    let lvm = datastore["driver"] == "DATASTORE_DRIVER_LVM";
    assert!(Path::new("/dev/kvm").exists(), "/dev/kvm is required");
    let (app, manager, tmp) = create_running_app().await;
    let _guard = ShutdownGuard(Some(manager.clone()));
    if !lvm {
        // Its key is sealed with the secrets key, which servers hold.
        let mut cfg = glidex_control_plane::config::Config::default();
        cfg.auth.secrets_key = Some(tmp.path().join("secrets.kek"));
        let auth = glidex_control_plane::auth::AuthService::disabled_with(manager.database(), cfg)
            .unwrap();
        manager.set_secrets_key(auth);
    }
    let firmware = pull_firmware_image(&app, hypervisor).await;

    // The cloud image as a ready image record.
    let images = manager.images();
    let img_id = format!("lvm-boot-{}", uuid::Uuid::new_v4().simple());
    let file = images.image_path(&img_id);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    run_ok(
        Command::new("cp")
            .arg("--sparse=always")
            .arg(&source)
            .arg(&file),
    );
    let info: Value = serde_json::from_str(&run_ok(
        Command::new("qemu-img")
            .args(["info", "--output=json"])
            .arg(&file),
    ))
    .unwrap();
    assert_eq!(info["format"], "qcow2", "{image_env} must be qcow2");
    let size = info["virtual-size"].as_u64().unwrap();
    let sha = glidex_control_plane::images::download::sha256_path(&file).unwrap();
    let rec = json!({
        "id": img_id, "name": img_id, "kind": "disk",
        "source": { "kind": "url", "url": "http://example.invalid/x", "expected_sha256": sha },
        "status": { "state": "ready" }, "format": "qcow2", "virtual_size_bytes": size,
        "file_size_bytes": std::fs::metadata(&file).unwrap().len(), "sha256": sha,
        "arch": "x86_64", "created_at": 1,
    });
    manager
        .database()
        .write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Images.definition())?
                    .insert(img_id.as_str(), rec.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();
    images.sync_keys(TableId::Images, std::slice::from_ref(&img_id));

    let (s, v) = request(&app, "POST", "/datastores", Some(datastore)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let username = "gxtester";
    let password = uuid::Uuid::new_v4().simple().to_string();
    let (s, v) = request(
        &app,
        "POST",
        "/credentials",
        Some(json!({"username": username, "password": password})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = request(
        &app,
        "POST",
        "/disks",
        Some(json!({"name": "lvm-root", "image": img_id, "size_gib": 6, "datastore": "boot-thin"})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let (_, d) = request(&app, "GET", "/disks/lvm-root", None).await;
        match d["disk"]["status"].as_str() {
            Some("ready") => break,
            Some("failed") => panic!("{d}"),
            _ => assert!(Instant::now() < deadline, "disk never ready: {d}"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    let hostname = "gx-lvmtest";
    let (s, vm) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({
            "name": hostname, "vcpu_count": 2, "mem_size_mib": 2048,
            "hypervisor": hypervisor_pb(hypervisor), "firmware": firmware,
            "root_disk": "lvm-root", "credential": username,
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm_id(&vm);
    let (s, body) = request(&app, "POST", &format!("/vms/{id}/start?wait=120"), None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let console_path = glidex_control_plane::paths::vm_paths(&id).console_socket;
    let (_, d) = request(&app, "GET", "/disks/lvm-root", None).await;
    let disk_id = d["disk"]["id"].as_str().unwrap().replace('-', "");
    let serial = disk_id[..20].to_string();
    tokio::task::spawn_blocking(move || {
        let mut console = Console::connect(&console_path);
        console.expect(&format!("{hostname} login: "), Duration::from_secs(300));
        console.send(&format!("{username}\r"));
        console.expect("Password: ", Duration::from_secs(30));
        console.send(&format!("{password}\r"));
        console.expect(
            &format!("{username}@{hostname}:~$ "),
            Duration::from_secs(60),
        );
        // The guest sees the 6 GiB volume, and its root grew into it.
        console.send(
            "echo GX_$(cat /sys/block/vda/size)_$(df --output=size -BG / | tail -1 | tr -d ' G')\r",
        );
        console.expect("GX_12582912_", Duration::from_secs(30));
        if !lvm {
            // A vhost-user-blk disk with the volume's serial.
            console.send("echo GX_$(cat /sys/block/vda/serial)_\r");
            console.expect(&format!("GX_{serial}_"), Duration::from_secs(30));
        }
    })
    .await
    .unwrap();
    assert!(
        !lvm || manager
            .images()
            .stored_bytes(&manager.images().get_disk("lvm-root").unwrap())
            .unwrap()
            > 0,
        "metering sees the volume's blocks"
    );
    let (s, body) = request(&app, "POST", &format!("/vms/{id}/stop?wait=120"), None).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let (s, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, v) = request(&app, "DELETE", "/disks/lvm-root", None).await;
    assert!(s.is_success(), "{v}");
    // The image volume goes with the image.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (s, v) = request(&app, "DELETE", &format!("/images/{img_id}"), None).await;
        if s.is_success() {
            break;
        }
        assert!(Instant::now() < deadline, "{v}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if !lvm {
        // The disk's clone, in hyperstor's trash, keeps the image's volume
        // until it is purged; then the leader's probe removes it (S24).
        if let Ok(tls) = std::env::var("GLIDEX_TEST_HYPERSTOR_ADMIN_TLS") {
            let api = std::env::var("GLIDEX_TEST_HYPERSTOR_API").unwrap();
            let ctl =
                std::env::var("GLIDEX_HYPERSTORCTL").unwrap_or_else(|_| "hyperstorctl".into());
            let hs = |args: &[&str]| {
                run_ok(
                    Command::new(&ctl)
                        .args(["--api", &api, "--tls", &tls])
                        .args(args),
                )
            };
            let image_volume = format!("glidex image {img_id} on");
            assert!(
                hs(&["volumes"]).contains(&image_volume),
                "kept for its clone"
            );
            hs(&["trash", "purge", "--id", &disk_id]);
            let deadline = Instant::now() + Duration::from_secs(150);
            while hs(&["volumes"]).contains(&image_volume) {
                assert!(
                    Instant::now() < deadline,
                    "the image volume was left behind"
                );
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        let (s, v) = request(&app, "DELETE", "/datastores/boot-thin", None).await;
        assert!(s.is_success(), "{v}");
        return;
    }
    let vg = std::env::var("GLIDEX_STORD_VG").unwrap();
    let mut c = glidex_stord::client::Client::connect(Path::new(
        &std::env::var("GLIDEX_STORD_SOCKET").unwrap(),
    ))
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let vols: Vec<glidex_stord::proto::Volume> = c
            .call(glidex_stord::proto::Op::ListVolumes { vg: vg.clone() })
            .unwrap();
        if !vols.iter().any(|v| v.name.contains(&img_id)) {
            break;
        }
        assert!(Instant::now() < deadline, "image volume left: {vols:?}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM from an LVM volume; needs KVM, cloud-hypervisor, a running glidex-stord and a VG"]
async fn lvm_root_disk_boots() {
    lvm_root_disk_e2e("cloudhypervisor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM from an LVM volume; needs KVM, QEMU, a running glidex-stord and a VG"]
async fn qemu_lvm_root_disk_boots() {
    lvm_root_disk_e2e("qemu").await;
}

/// A cloud image booted from a hyperstor volume (spec/storage.md §11.2):
/// the datastore enrolled with a mgmt token, the image written into an
/// image volume and snapshotted, the disk a clone of it grown and its root
/// partition extended through `hyperstorctl`, the VM's disk a
/// vhost-user-blk socket of the node's `hyperstor-vhost`. Needs KVM, a
/// qcow2 cloud image, a hyperstor cluster, `hyperstor-vhost serve` for it
/// (`GLIDEX_HYPERSTOR_VHOST_DIR`, its `--dir`), `hyperstorctl`
/// (`GLIDEX_HYPERSTORCTL`) and a single-use mgmt token in a file:
///
/// ```sh
/// GLIDEX_TEST_HYPERSTOR_IMAGE=~/images/resolute-server-cloudimg-amd64.img \
///   GLIDEX_TEST_HYPERSTOR_API=127.0.0.1:8850 GLIDEX_TEST_HYPERSTOR_POOL=1 \
///   GLIDEX_TEST_HYPERSTOR_TOKEN=/path/to/mgmt.token \
///   GLIDEX_HYPERSTOR_VHOST_DIR=/run/hyperstor/vhost \
///   cargo test -p glidex-control-plane --test functional_tests \
///   hyperstor_root_disk_boots -- --ignored --nocapture
/// ```
async fn hyperstor_root_disk_e2e(hypervisor: &str) {
    let var = |v: &str| std::env::var(v).unwrap_or_else(|_| panic!("{v}"));
    let token = std::fs::read_to_string(var("GLIDEX_TEST_HYPERSTOR_TOKEN")).unwrap();
    root_disk_e2e(
        hypervisor,
        "GLIDEX_TEST_HYPERSTOR_IMAGE",
        json!({"name": "boot-thin", "driver": "DATASTORE_DRIVER_HYPERSTOR",
                "hyperstor_api": [var("GLIDEX_TEST_HYPERSTOR_API")],
                "hyperstor_pool": var("GLIDEX_TEST_HYPERSTOR_POOL").parse::<u32>().unwrap(),
                "hyperstor_token": token.trim()}),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM from a hyperstor volume; needs KVM, cloud-hypervisor, a hyperstor cluster, hyperstor-vhost and a mgmt token"]
async fn hyperstor_root_disk_boots() {
    hyperstor_root_disk_e2e("cloudhypervisor").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM from a hyperstor volume; needs KVM, QEMU, a hyperstor cluster, hyperstor-vhost and a mgmt token"]
async fn qemu_hyperstor_root_disk_boots() {
    hyperstor_root_disk_e2e("qemu").await;
}
