//! Images and disks through the REST API (spec/10-images.md §12).
//!
//! Images are served by a local HTTP server (ETag + Range support, plus a
//! path that drops the connection part way), so these tests need no
//! network. They need `qemu-img`, `qemu-io`, `sgdisk` and `growpart`, and
//! are skipped (with a note) where those are missing.

mod common;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path as AxPath, State},
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tower::ServiceExt;

use glidex_control_plane::api::create_router;
use glidex_control_plane::images::qemu_img;
use glidex_control_plane::state::VmManager;

const MIB: u64 = 1024 * 1024;

fn tools_present() -> bool {
    let missing: Vec<_> = qemu_img::ALL_TOOLS
        .iter()
        .filter(|t| !t.available())
        .map(|t| t.name)
        .collect();
    if !missing.is_empty() {
        eprintln!("skipping: missing {}", missing.join(", "));
    }
    missing.is_empty()
}

fn run_ok(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{:?}: {}",
        cmd,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// A 32 MiB cloud-image-like disk: GPT, 8 MiB ESP, 16 MiB root (x86-64
/// root type) with a marker inside, free space after.
fn fixture_raw(dir: &Path) -> PathBuf {
    let raw = dir.join("fixture.raw");
    std::fs::File::create(&raw)
        .unwrap()
        .set_len(32 * MIB)
        .unwrap();
    run_ok(
        Command::new(qemu_img::SGDISK.find().unwrap())
            .args([
                "-n",
                "1:2048:+8M",
                "-t",
                "1:ef00",
                "-n",
                "2:0:+16M",
                "-t",
                "2:8304",
            ])
            .arg(&raw),
    );
    use std::os::unix::fs::FileExt;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&raw)
        .unwrap()
        .write_all_at(b"GLIDEX-ROOT-MARKER", 12 * MIB)
        .unwrap();
    raw
}

struct Fixtures {
    qcow2: Vec<u8>,
    raw: Vec<u8>,
    with_backing: Vec<u8>,
}

fn fixtures(dir: &Path) -> Fixtures {
    let raw = fixture_raw(dir);
    let q = dir.join("fixture.qcow2");
    run_ok(
        Command::new("qemu-img")
            .args(["convert", "-q", "-f", "raw", "-O", "qcow2"])
            .arg(&raw)
            .arg(&q),
    );
    let b = dir.join("backing.qcow2");
    run_ok(
        Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b"])
            .arg(&q)
            .arg(&b),
    );
    Fixtures {
        qcow2: std::fs::read(&q).unwrap(),
        raw: std::fs::read(&raw).unwrap(),
        with_backing: std::fs::read(&b).unwrap(),
    }
}

#[derive(Clone)]
struct Served {
    files: Arc<HashMap<String, Vec<u8>>>,
    hits: Arc<AtomicUsize>,
}

/// `GET /<name>` with a strong ETag and `Range: bytes=N-` + `If-Range`.
/// `flaky-*`: the first response stops after half the body.
async fn serve(
    State(st): State<Served>,
    AxPath(name): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(data) = st.files.get(name.trim_start_matches("flaky-")) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", &sha256_hex(data)[..16]);
    let start = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.trim_end_matches('-').parse::<usize>().ok())
        .filter(|_| headers.get("if-range").and_then(|v| v.to_str().ok()) == Some(etag.as_str()));
    let hit = st.hits.fetch_add(1, Ordering::SeqCst);
    let (status, body) = match start {
        Some(s) => (StatusCode::PARTIAL_CONTENT, data[s..].to_vec()),
        None => (StatusCode::OK, data.clone()),
    };
    let len = body.len();
    let mut resp = if name.starts_with("flaky-") && hit == 0 {
        // Advertise the full length, then fail half way through.
        let half = Bytes::from(body[..len / 2].to_vec());
        let stream = futures_util::stream::iter(vec![
            Ok::<_, std::io::Error>(half),
            Err(std::io::Error::other("dropped")),
        ]);
        Response::new(Body::from_stream(stream))
    } else {
        Response::new(Body::from(body))
    };
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert("etag", etag.parse().unwrap());
    h.insert("content-length", len.to_string().parse().unwrap());
    if let Some(s) = start {
        h.insert(
            "content-range",
            format!("bytes {}-{}/{}", s, data.len() - 1, data.len())
                .parse()
                .unwrap(),
        );
    }
    resp
}

async fn start_server(fx: &Fixtures) -> (String, Arc<AtomicUsize>) {
    let mut files = HashMap::new();
    files.insert("good.qcow2".to_string(), fx.qcow2.clone());
    files.insert("disk.raw".to_string(), fx.raw.clone());
    files.insert("backing.qcow2".to_string(), fx.with_backing.clone());
    files.insert("fw.fd".to_string(), FIRMWARE.to_vec());
    files.insert("vars.fd".to_string(), VARS.to_vec());
    files.insert(
        "vmlinuz-6.8.0-1-generic".to_string(),
        bzimage("6.8.0-1-generic"),
    );
    files.insert(
        "vmlinuz-6.9.0-2-generic".to_string(),
        bzimage("6.9.0-2-generic"),
    );
    files.insert("initrd.img".to_string(), INITRD.to_vec());
    let hits = Arc::new(AtomicUsize::new(0));
    let st = Served {
        files: Arc::new(files),
        hits: hits.clone(),
    };
    let app = Router::new().route("/{name}", get(serve)).with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{}", addr), hits)
}

fn create_app(dir: &Path) -> (Router, Arc<VmManager>) {
    // Local plain-http test server: private addresses must be allowed.
    // Every test sets the same value, so parallel tests don't race on it.
    // SAFETY: test-only; every test sets it the same way before reading it.
    unsafe { std::env::set_var("GLIDEX_ALLOW_PRIVATE_IMAGE_URLS", "1") };
    // Never download the kernel extractor here; extraction runs in the
    // ignored functional tests (spec/23-kernel-boot.md §10).
    // SAFETY: as above.
    unsafe { std::env::set_var("GLIDEX_KERNEL_EXTRACTION", "off") };
    let manager = common::with_db_path(dir.join("test.db")).unwrap();
    // Disks are made, resized and extended by the disk controller
    // (spec/12-reconciliation.md §10.1); the tests wait for it with ?wait.
    manager.start_controllers();
    (create_router(manager.clone()), manager)
}

/// The disk once the disk controller has made it.
async fn ready_disk(app: &Router, key: &str) -> Value {
    for _ in 0..150 {
        let (_, d) = request(app, "GET", &format!("/disks/{key}"), None).await;
        let d = disk_of(&d);
        if d["status"] == "ready" {
            assert_eq!(d["phase"], "DISK_PHASE_READY", "{d}");
            return d;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("disk {key} never became ready");
}

/// Reasons and messages of a disk's events, newest last.
async fn disk_events(app: &Router, key: &str) -> Vec<(String, String)> {
    let (_, d) = request(app, "GET", &format!("/disks/{key}"), None).await;
    let (_, ev) = request(
        app,
        "GET",
        &format!("/disks/{}/events", d["disk"]["id"].as_str().unwrap()),
        None,
    )
    .await;
    ev["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["reason"].as_str().unwrap().to_string(),
                e["message"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, v, _) = request_with_headers(app, method, uri, body).await;
    (status, v)
}

async fn request_with_headers(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, HeaderMap) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(b) => {
            builder = builder.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        headers,
    )
}

/// A proto3 JSON 64-bit integer: a string (`"1073741824"`), or a number
/// (REST error details); left out (null) at its default, 0.
fn num(v: &Value) -> u64 {
    match v {
        Value::Null => 0,
        Value::String(s) => s.parse().unwrap_or_else(|_| panic!("not a number: {v}")),
        _ => v.as_u64().unwrap_or_else(|| panic!("not a number: {v}")),
    }
}

/// The VM in a `{vm}` answer (`POST /vms`, writes), in the proto3 JSON
/// shape (spec/18-grpc.md §9.1).
fn vm_of(v: &Value) -> &Value {
    assert!(v["vm"].is_object(), "not a VM answer: {v}");
    &v["vm"]
}

/// The id of the VM in a `{vm}` answer.
fn id_of(v: &Value) -> String {
    vm_of(v)["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no VM id: {v}"))
        .to_string()
}

/// The disk of a `{disk, settled?}` or `{disk}` answer.
fn disk_of(v: &Value) -> Value {
    v["disk"].clone()
}

/// The image of an `{image, created?}` or `{image}` answer.
fn image_of(v: &Value) -> Value {
    v["image"].clone()
}

/// The `key` list of a wrapped list answer (`{images: [...]}`); proto3 JSON
/// leaves an empty list out.
fn items(v: &Value, key: &str) -> Vec<Value> {
    assert!(v.is_object(), "not a wrapped list: {v}");
    v[key].as_array().cloned().unwrap_or_default()
}

/// Poll an image until it leaves downloading/verifying.
async fn wait_image(app: &Router, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, img) = request(app, "GET", &format!("/images/{id}"), None).await;
        assert_eq!(status, StatusCode::OK, "{img}");
        let img = image_of(&img);
        let state = img["status"]["state"].as_str().unwrap().to_string();
        if state != "IMAGE_STATE_DOWNLOADING" && state != "IMAGE_STATE_VERIFYING" {
            return img;
        }
        assert!(Instant::now() < deadline, "image {id} stuck: {img}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A stand-in firmware file (never booted).
const FIRMWARE: &[u8] = &[0x5a; 64 * 1024];
/// A stand-in UEFI variable-store template.
const VARS: &[u8] = &[0xa5; 16 * 1024];
/// A stand-in initramfs (never booted).
const INITRD: &[u8] = &[0x37; 48 * 1024];

/// A stand-in kernel (never booted): an x86_64 bzImage header, boot
/// protocol 2.15, carrying `release`.
fn bzimage(release: &str) -> Vec<u8> {
    let mut b = vec![0u8; 32 * 1024];
    b[0x202..0x206].copy_from_slice(b"HdrS");
    b[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
    b[0x236..0x238].copy_from_slice(&0x7fu16.to_le_bytes());
    b[0x20e..0x210].copy_from_slice(&0x200u16.to_le_bytes());
    let v = format!("{release} (buildd@test) #1 SMP");
    b[0x400..0x400 + v.len()].copy_from_slice(v.as_bytes());
    b
}

/// Pull a served kernel with `initrd.img`, as a kernel image.
async fn pull_kernel(app: &Router, base: &str, file: &str, name: &str) -> Value {
    let body = json!({
        "url": format!("{base}/{file}"), "kind": "IMAGE_KIND_KERNEL", "name": name,
        "sha256": sha256_hex(&bzimage(file.trim_start_matches("vmlinuz-"))),
        "initrd_url": format!("{base}/initrd.img"), "initrd_sha256": sha256_hex(INITRD),
        "cmdline": "root=LABEL=cloudimg-rootfs ro console=ttyS0",
    });
    let (status, img) = request(app, "POST", "/images", Some(body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let img = wait_image(app, img["image"]["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    img
}

/// Pull `fw.fd` as a firmware image for `hypervisor`.
/// A hypervisor's short name (`qemu`, `cloudhypervisor`) as its proto name.
fn hypervisor_pb(h: &str) -> String {
    match h {
        "cloudhypervisor" => "HYPERVISOR_CLOUD_HYPERVISOR".into(),
        h => format!("HYPERVISOR_{}", h.to_ascii_uppercase()),
    }
}

async fn pull_firmware(app: &Router, base: &str, name: &str, hypervisor: &str) -> Value {
    let body = json!({"url": format!("{base}/fw.fd"), "name": name, "kind": "IMAGE_KIND_FIRMWARE", "hypervisor": hypervisor_pb(hypervisor), "sha256": sha256_hex(FIRMWARE)});
    let (status, img) = request(app, "POST", "/images", Some(body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let img = wait_image(app, img["image"]["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    img
}

async fn pull_ready(app: &Router, base: &str, file: &str, name: &str, sha: Option<&str>) -> Value {
    let mut body = json!({"url": format!("{base}/{file}"), "name": name});
    if let Some(s) = sha {
        body["sha256"] = json!(s);
    }
    let (status, img) = request(app, "POST", "/images", Some(body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let img = wait_image(app, img["image"]["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    img
}

fn sgdisk_verify(disk: &Path, format: &str, dir: &Path) -> String {
    let raw = dir.join(format!("verify-{}.raw", uuid::Uuid::new_v4()));
    run_ok(
        Command::new("qemu-img")
            .args(["convert", "-q", "-f", format, "-O", "raw"])
            .arg(disk)
            .arg(&raw),
    );
    let out = run_ok(
        Command::new(qemu_img::SGDISK.find().unwrap())
            .arg("-v")
            .arg(&raw),
    );
    let _ = std::fs::remove_file(&raw);
    out
}

fn marker_intact(disk: &Path, format: &str, dir: &Path) -> bool {
    let out = dir.join(format!("marker-{}.bin", uuid::Uuid::new_v4()));
    // qemu-img dd's count includes the skipped blocks.
    run_ok(
        Command::new("qemu-img")
            .args(["dd", "-f", format, "-O", "raw", "bs=512"])
            .arg(format!("skip={}", 12 * MIB / 512))
            .arg(format!("count={}", 12 * MIB / 512 + 1))
            .arg(format!("if={}", disk.display()))
            .arg(format!("of={}", out.display())),
    );
    let data = std::fs::read(&out).unwrap();
    data.starts_with(b"GLIDEX-ROOT-MARKER")
}

#[tokio::test]
async fn catalog_lists_host_images() {
    let tmp = TempDir::new().unwrap();
    let (app, _m) = create_app(tmp.path());
    let (status, cat, _) = request_with_headers(&app, "GET", "/images/catalog", None).await;
    assert_eq!(status, StatusCode::OK);
    let cat = items(&cat, "items");
    let keys: Vec<_> = cat
        .iter()
        .map(|e| e["key"].as_str().unwrap().to_string())
        .collect();
    assert!(keys.contains(&"ubuntu-26.04".to_string()), "{keys:?}");
    assert!(cat[0]["downloaded_image_id"].is_null());

    let (status, err) = request(&app, "POST", "/images", Some(json!({"catalog": "nope"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"], "invalid_image");
    let (status, _) = request(&app, "POST", "/images", Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, err) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"url": "file:///etc/passwd"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
}

#[tokio::test]
async fn download_verify_then_linked_and_full_disks() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, manager) = create_app(tmp.path());

    let sha = sha256_hex(&fx.qcow2);
    let img = pull_ready(&app, &base, "good.qcow2", "base", Some(&sha)).await;
    assert_eq!(img["verified"], true);
    assert_eq!(img["sha256"], sha);
    assert_eq!(num(&img["virtual_size_bytes"]), 32 * MIB, "{img}");
    let img_path = PathBuf::from(img["path"].as_str().unwrap());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&img_path).unwrap().permissions().mode() & 0o777,
        0o444
    );
    assert!(!img_path.with_extension("qcow2.part").exists());

    // Same name again: conflict.
    let (status, _) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"url": format!("{base}/good.qcow2"), "name": "base"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Smaller than the image: refused with the minimum.
    let (status, err) = request(
        &app,
        "POST",
        "/disks?wait=30",
        Some(json!({"name": "tiny", "image": "base", "size_bytes": 16 * MIB})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert_eq!(err["error"], "invalid_disk");
    assert_eq!(num(&err["details"]["min_size_bytes"]), 32 * MIB, "{err}");

    // Linked, larger: root partition extended offline.
    let (status, d) = request(
        &app,
        "POST",
        "/disks?wait=30",
        Some(json!({"name": "web-root", "image": "base", "size_bytes": 64 * MIB})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{d}");
    let d = disk_of(&d);
    assert_eq!(d["origin"]["image"]["mode"], "CLONE_MODE_LINKED", "{d}");
    assert_eq!(d["origin"]["image"]["image_id"], img["id"], "{d}");
    let (_, detail) = request(&app, "GET", "/disks/web-root", None).await;
    let detail = disk_of(&detail);
    // The overlay names its image relative to itself (spec/24-storage.md S8).
    let backing = detail["info"]["backing_filename"].as_str().unwrap();
    assert!(backing.starts_with("../"), "{detail}");
    let linked_dir = PathBuf::from(d["path"].as_str().unwrap());
    assert_eq!(
        linked_dir
            .parent()
            .unwrap()
            .join(backing)
            .canonicalize()
            .unwrap(),
        img_path.canonicalize().unwrap(),
        "{detail}"
    );
    let parts = detail["partition_table"]["partitions"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[1]["is_root"], true);
    assert_eq!(
        num(&parts[1]["start_bytes"]) + num(&parts[1]["size_bytes"]),
        64 * MIB - 33 * 512
    );
    assert_eq!(num(&detail["partition_table"]["free_tail_bytes"]), 0);
    let linked_path = PathBuf::from(d["path"].as_str().unwrap());
    assert!(sgdisk_verify(&linked_path, "qcow2", tmp.path()).contains("No problems found"));
    assert!(marker_intact(&linked_path, "qcow2", tmp.path()));

    // The image cannot go while the linked disk exists.
    let (status, err) = request(&app, "DELETE", "/images/base", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");
    assert!(err["message"].as_str().unwrap().contains("web-root"));

    // Full clone, default size (GLIDEX_DEFAULT_ROOT_GIB is 10 GiB).
    let (status, full) = request(&app, "POST", "/disks?wait=30", Some(json!({"name": "copy", "image": "base", "clone": "CLONE_MODE_FULL", "format": "DISK_FORMAT_RAW", "extend_root": false}))).await;
    assert_eq!(status, StatusCode::CREATED, "{full}");
    let full = disk_of(&full);
    assert_eq!(num(&full["size_bytes"]), 10 * 1024 * MIB, "{full}");
    assert_eq!(full["format"], "DISK_FORMAT_RAW");

    // Raw + linked is impossible.
    let (status, _) = request(
        &app,
        "POST",
        "/disks?wait=30",
        Some(json!({"name": "x", "image": "base", "format": "DISK_FORMAT_RAW"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Deleting the linked disk frees the image; the full copy doesn't hold it.
    let (status, _) = request(&app, "DELETE", "/disks/web-root", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!linked_path.exists());
    let (status, _) = request(&app, "DELETE", "/images/base", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!img_path.exists());
    let (status, _) = request(&app, "GET", "/disks/copy", None).await;
    assert_eq!(status, StatusCode::OK);

    // Records survive a control-plane restart.
    manager.stop_controllers().await;
    drop(app);
    drop(manager);
    let (app, _m) = create_app(tmp.path());
    let (_, disks) = request(&app, "GET", "/disks", None).await;
    let disks = items(&disks, "disks");
    assert_eq!(disks.len(), 1);
    assert_eq!(disks[0]["name"], "copy");
    assert_eq!(disks[0]["status"], "ready");
    assert_eq!(disks[0]["phase"], "DISK_PHASE_READY");
}

#[tokio::test]
async fn checksum_mismatch_and_backing_file_fail() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, _m) = create_app(tmp.path());

    let (status, img) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"url": format!("{base}/good.qcow2"), "name": "bad", "sha256": "0".repeat(64)})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let img = wait_image(&app, img["image"]["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "IMAGE_STATE_FAILED");
    assert!(
        img["status"]["reason"]
            .as_str()
            .unwrap()
            .contains("checksum mismatch"),
        "{img}"
    );
    let images_dir = tmp.path().join("images");
    assert_eq!(
        std::fs::read_dir(&images_dir).unwrap().count(),
        0,
        "partial file left behind"
    );

    let (_, img) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"url": format!("{base}/backing.qcow2"), "name": "evil"})),
    )
    .await;
    let img = wait_image(&app, img["image"]["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "IMAGE_STATE_FAILED");
    assert!(
        img["status"]["reason"]
            .as_str()
            .unwrap()
            .contains("backing file"),
        "{img}"
    );
    assert_eq!(img["verified"], false);

    // A failed image can be deleted.
    let (status, _) = request(&app, "DELETE", "/images/evil", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(
        &app,
        "POST",
        "/disks?wait=30",
        Some(json!({"name": "d", "image": "bad"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "a failed image is not ready");

    // Retry (D19): downloaded again; the digest still doesn't match.
    let (_, before) = request(&app, "GET", "/images/bad", None).await;
    let (status, r) = request(&app, "POST", "/images/bad/retry", None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{r}");
    // The image controller acts on the retry a moment later; until then the
    // image still reads "failed" from before.
    let id = before["image"]["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, ev) = request(&app, "GET", &format!("/images/{id}/events"), None).await;
        let reasons: Vec<String> = ev["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["reason"].as_str().unwrap().to_string())
            .collect();
        if reasons.iter().any(|r| r == "Retrying") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the controller never retried: {reasons:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let img = wait_image(&app, id).await;
    assert_eq!(img["status"]["state"], "IMAGE_STATE_FAILED");
    assert!(
        img["status"]["reason"]
            .as_str()
            .unwrap()
            .contains("checksum mismatch"),
        "{img}"
    );
    // Only a failed image can be retried.
    let (status, _) = request(&app, "POST", "/images/nonexistent/retry", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// spec/12-reconciliation.md §7.6, §10.1: a disk may be created from an image
/// still downloading; it waits pending and is made once the image is.
/// A disk whose file disappears is reported missing, never recreated.
#[tokio::test]
async fn disks_wait_for_their_image_and_notice_a_lost_file() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, _m) = create_app(tmp.path());

    // The flaky URL needs a retry (about 2 s), so the image is still
    // downloading when the disk is created.
    let (status, img) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"url": format!("{base}/flaky-good.qcow2"), "name": "slow"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let (status, d) = request(
        &app,
        "POST",
        "/disks",
        Some(json!({"name": "early", "image": "slow", "size_bytes": 64 * MIB})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{d}");
    let d = disk_of(&d);
    assert_eq!(d["status"], "pending", "{d}");
    assert_eq!(d["phase"], "DISK_PHASE_PENDING", "{d}");
    // The image can't be deleted from under a disk waiting for it.
    let (status, e) = request(&app, "DELETE", "/images/slow", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{e}");
    assert!(e["message"].as_str().unwrap().contains("early"), "{e}");
    let d = ready_disk(&app, "early").await;
    assert_eq!(num(&d["size_bytes"]), 64 * MIB);
    let reasons: Vec<String> = disk_events(&app, "early")
        .await
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(
        reasons,
        ["Created", "Created"],
        "recorded by the API, made by the controller"
    );

    // The file goes away behind glidex's back.
    std::fs::remove_file(d["path"].as_str().unwrap()).unwrap();
    let mut missing = false;
    for _ in 0..200 {
        // Resync is every 30 s; a resize request reconciles it at once.
        let _ = request(
            &app,
            "POST",
            "/disks/early/resize",
            Some(json!({"size_bytes": 80 * MIB})),
        )
        .await;
        let (_, d) = request(&app, "GET", "/disks/early", None).await;
        let d = disk_of(&d);
        if d["status"] == "missing" {
            assert_eq!(d["phase"], "DISK_PHASE_MISSING", "{d}");
            missing = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(missing, "the lost file is reported");
    assert!(!tmp.path().join("disks").read_dir().unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .contains(d["id"].as_str().unwrap())
    }));
}

#[tokio::test]
async fn resume_after_dropped_connection_and_raw_conversion() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, hits) = start_server(&fx).await;
    let (app, _m) = create_app(tmp.path());

    let sha = sha256_hex(&fx.qcow2);
    let img = pull_ready(&app, &base, "flaky-good.qcow2", "flaky", Some(&sha)).await;
    assert_eq!(img["sha256"], sha);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "expected one dropped attempt and one ranged resume"
    );

    // A raw download is stored as qcow2; the stored digest is the qcow2's.
    let raw_sha = sha256_hex(&fx.raw);
    let img = pull_ready(&app, &base, "disk.raw", "fromraw", Some(&raw_sha)).await;
    assert_eq!(img["format"], "DISK_FORMAT_QCOW2");
    assert_ne!(img["sha256"], raw_sha);
    let info = qemu_img::info(Path::new(img["path"].as_str().unwrap()), None).unwrap();
    assert_eq!(info.format, "qcow2");
    assert_eq!(info.virtual_size, 32 * MIB);
}

#[tokio::test]
async fn grow_shrink_and_extend_root() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, _m) = create_app(tmp.path());
    pull_ready(&app, &base, "good.qcow2", "base", None).await;

    // Blank disks have no table: grow works, shrink is refused.
    let (status, blank) = request(
        &app,
        "POST",
        "/disks?wait=30",
        Some(json!({"name": "blank", "size_gib": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{blank}");
    let (status, d) = request(
        &app,
        "POST",
        "/disks/blank/resize?wait=30",
        Some(json!({"size_gib": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["settled"], true, "{d}");
    assert_eq!(num(&disk_of(&d)["size_bytes"]), 2 * 1024 * MIB);
    let (status, err) = request(
        &app,
        "POST",
        "/disks/blank/resize?wait=30",
        Some(json!({"size_gib": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("no partition table")
    );

    for format in ["qcow2", "raw"] {
        let name = format!("img-{format}");
        let (status, d) = request(
            &app,
            "POST",
            "/disks?wait=30",
            Some(json!({
                "name": name, "image": "base", "clone": "CLONE_MODE_FULL", "format": format!("DISK_FORMAT_{}", format.to_ascii_uppercase()),
                "size_bytes": 48 * MIB, "extend_root": false
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{d}");
        let path = PathBuf::from(d["disk"]["path"].as_str().unwrap());

        // Root partition ends at 25 MiB (+ GPT backup) → minimum 26 MiB.
        let (status, err) = request(
            &app,
            "POST",
            &format!("/disks/{name}/resize?wait=30"),
            Some(json!({"size_bytes": 16 * MIB})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
        assert_eq!(num(&err["details"]["min_size_bytes"]), 26 * MIB, "{err}");
        let (status, d) = request(
            &app,
            "POST",
            &format!("/disks/{name}/resize?wait=30"),
            Some(json!({"size_bytes": 26 * MIB})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{d}");
        assert_eq!(num(&disk_of(&d)["size_bytes"]), 26 * MIB);
        assert_eq!(qemu_img::info(&path, None).unwrap().virtual_size, 26 * MIB);
        assert!(sgdisk_verify(&path, format, tmp.path()).contains("No problems found"));
        assert!(marker_intact(&path, format, tmp.path()));

        // Grow: from an image, extend_root defaults to true.
        let (status, d) = request(
            &app,
            "POST",
            &format!("/disks/{name}/resize?wait=30"),
            Some(json!({"size_bytes": 80 * MIB})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{d}");
        // What the controller did is in the disk's events.
        assert!(
            disk_events(&app, &name)
                .await
                .last()
                .unwrap()
                .1
                .contains("Grown"),
            "{:?}",
            disk_events(&app, &name).await
        );
        let (_, detail) = request(&app, "GET", &format!("/disks/{name}"), None).await;
        assert_eq!(
            num(&detail["disk"]["partition_table"]["free_tail_bytes"]),
            0,
            "{detail}"
        );
        assert!(sgdisk_verify(&path, format, tmp.path()).contains("No problems found"));
        assert!(marker_intact(&path, format, tmp.path()));

        // Nothing left to grow; on-boot just sets the flag.
        let (status, d) = request(
            &app,
            "POST",
            &format!("/disks/{name}/extend-root?wait=30"),
            Some(json!({})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{d}");
        assert_eq!(
            disk_events(&app, &name).await.last().unwrap(),
            &("RootExtended".to_string(), "AlreadyFull".to_string())
        );
        let (_, d) = request(
            &app,
            "POST",
            &format!("/disks/{name}/extend-root?wait=30"),
            Some(json!({"mode": "EXTEND_MODE_ON_BOOT"})),
        )
        .await;
        assert_eq!(
            disk_events(&app, &name).await.last().unwrap(),
            &("RootExtended".to_string(), "OnBoot".to_string())
        );
        assert_eq!(d["disk"]["pending_growpart"], true, "{d}");
    }

    // Linked disks cannot shrink below their image.
    let (_, _) = request(
        &app,
        "POST",
        "/disks?wait=30",
        Some(json!({"name": "lnk", "image": "base", "size_bytes": 40 * MIB, "extend_root": false})),
    )
    .await;
    let (status, err) = request(
        &app,
        "POST",
        "/disks/lnk/resize?wait=30",
        Some(json!({"size_bytes": 26 * MIB})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert_eq!(num(&err["details"]["min_size_bytes"]), 32 * MIB, "{err}");
}

#[tokio::test]
async fn vms_create_attach_and_own_disks() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, manager) = create_app(tmp.path());
    pull_ready(&app, &base, "good.qcow2", "base", None).await;

    // An image boots through firmware: none pulled yet, so refused.
    let (status, err) = request(&app, "POST", "/vms", Some(json!({
        "name": "web 1", "vcpu_count": 1, "mem_size_mib": 512, "image": "base", "root_disk_size_gib": 1
    }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("no firmware image"),
        "{err}"
    );
    let fw = pull_firmware(&app, &base, "edk2", "cloudhypervisor").await;

    // VM from an image: owned, linked root disk named after the VM.
    let (status, vm) = request(&app, "POST", "/vms", Some(json!({
        "name": "web 1", "vcpu_count": 1, "mem_size_mib": 512, "image": "base", "root_disk_size_gib": 1
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    assert_eq!(
        vm_of(&vm)["firmware"],
        fw["id"],
        "the default firmware image"
    );
    let vm_id = id_of(&vm);
    let root_id = vm_of(&vm)["root_disk"].as_str().unwrap().to_string();
    // Recorded with the VM, made by the disk controller.
    let root = ready_disk(&app, &root_id).await;
    assert_eq!(root["name"], "web-1-root");
    assert_eq!(root["attached_to"], vm_id.as_str());
    assert_eq!(num(&root["size_bytes"]), 1024 * MIB);
    let internal = manager.get_vm(&vm_id).await.unwrap();
    assert_eq!(
        internal.spec.config.rootfs_path,
        root["path"].as_str().unwrap()
    );
    assert!(internal.spec.config.owns_root_disk);
    assert_eq!(
        internal.spec.config.firmware_path.as_deref(),
        fw["path"].as_str(),
        "image implies firmware boot"
    );
    assert_eq!(
        internal.spec.config.firmware_image.as_deref(),
        fw["id"].as_str()
    );
    // In use: the firmware image stays.
    let (status, err) = request(&app, "DELETE", "/images/edk2", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");
    let (_, img) = request(&app, "GET", "/images/edk2", None).await;
    assert_eq!(img["image"]["used_by_vms"], json!(["web 1"]), "{img}");

    // Attached: no delete; a Created VM's disk can still be resized.
    let (status, _) = request(&app, "DELETE", &format!("/disks/{root_id}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, d) = request(
        &app,
        "POST",
        &format!("/disks/{root_id}/resize?wait=30"),
        Some(json!({"size_gib": 2})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{d}");

    // Exactly one root source.
    let (status, _) = request(&app, "POST", "/vms", Some(json!({
        "name": "both", "vcpu_count": 1, "mem_size_mib": 512, "image": "base", "rootfs_path": "/x"
    }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Data disks: attach/detach are spec writes (202); one VM per disk.
    let (_, _) = request(
        &app,
        "POST",
        "/disks?wait=30",
        Some(json!({"name": "data1", "size_gib": 1})),
    )
    .await;
    let (status, v) = request(
        &app,
        "POST",
        &format!("/vms/{vm_id}/disks"),
        Some(json!({"disk": "data1"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{v}");
    assert_eq!(vm_of(&v)["data_disks"].as_array().unwrap().len(), 1, "{v}");
    let (status, _) = request(&app, "POST", "/vms", Some(json!({
        "name": "other", "vcpu_count": 1, "mem_size_mib": 512, "kernel_image_path": "/k", "rootfs_path": "/x", "data_disks": ["data1"]
    }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "disk already attached");
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/vms/{vm_id}/disks/{root_id}"),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "root disk is not a data disk"
    );
    let (status, v) = request(&app, "DELETE", &format!("/vms/{vm_id}/disks/data1"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{v}");
    assert!(vm_of(&v).get("data_disks").is_none(), "{v}");

    // VM on an existing root disk plus a data disk: deleting the VM keeps both.
    let (_, _) = request(
        &app,
        "POST",
        "/disks?wait=30",
        Some(json!({"name": "mine", "image": "base"})),
    )
    .await;
    let (status, vm2) = request(&app, "POST", "/vms", Some(json!({
        "name": "vm2", "vcpu_count": 1, "mem_size_mib": 512, "root_disk": "mine", "data_disks": ["data1"]
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{vm2}");
    let vm2_id = id_of(&vm2);
    let (status, _) = request(&app, "DELETE", &format!("/vms/{vm2_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for name in ["mine", "data1"] {
        let (_, d) = request(&app, "GET", &format!("/disks/{name}"), None).await;
        let d = disk_of(&d);
        assert_eq!(d["status"], "ready", "{d}");
        assert!(d["attached_to"].is_null(), "{d}");
    }

    // keep_disk=true keeps an owned root disk, detached.
    let (_, vm3) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({"name": "vm3", "vcpu_count": 1, "mem_size_mib": 512, "image": "base"})),
    )
    .await;
    let vm3_root = vm_of(&vm3)["root_disk"].as_str().unwrap().to_string();
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/vms/{}?keep_disk=true", id_of(&vm3)),
        None,
    )
    .await;
    // At once, or once a create already running on its disk ends (S29).
    assert!(
        [StatusCode::NO_CONTENT, StatusCode::ACCEPTED].contains(&status),
        "{status}"
    );
    assert!(
        gone(&app, &format!("/vms/{}", id_of(&vm3))).await,
        "vm3 went"
    );
    let (_, d) = request(&app, "GET", &format!("/disks/{vm3_root}"), None).await;
    let d = disk_of(&d);
    assert!(d.is_object(), "{d}");
    assert!(d["attached_to"].is_null(), "{d}");
    assert!(d["owner"].is_null(), "no longer its VM's: {d}");
    // Deleted while it may still be being made: taken either way (S29).
    let (status, err) = request(&app, "DELETE", &format!("/disks/{vm3_root}"), None).await;
    assert!(
        [StatusCode::NO_CONTENT, StatusCode::ACCEPTED].contains(&status),
        "{status} {err}"
    );
    assert!(
        gone(&app, &format!("/disks/{vm3_root}")).await,
        "the kept disk went"
    );

    // Deleting the image-built VM deletes its owned root disk.
    let root_path = PathBuf::from(root["path"].as_str().unwrap());
    let (status, _) = request(&app, "DELETE", &format!("/vms/{vm_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "GET", &format!("/disks/{root_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!root_path.exists());
}

/// Deleting an image is a request the image controller finishes
/// (spec/12-reconciliation.md §6.3): a download in flight is cancelled and
/// its files removed; the name is free again.
/// Whether `path` answers 404 within 30 s.
async fn gone(app: &Router, path: &str) -> bool {
    for _ in 0..150 {
        let (status, _) = request(app, "GET", path, None).await;
        if status == StatusCode::NOT_FOUND {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    false
}

/// A VM, or a disk, whose disk is still being made can be deleted
/// (spec/24-storage.md S29): the request is taken while the create runs,
/// no new attempt starts, and both go once the running one ends. Before,
/// a create that kept failing kept the disk busy, and every delete was
/// refused with "busy (create)".
#[tokio::test]
async fn deleting_while_a_disk_is_being_made() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, manager) = create_app(tmp.path());
    pull_ready(&app, &base, "good.qcow2", "base", None).await;
    pull_firmware(&app, &base, "edk2", "cloudhypervisor").await;
    // Holds `id`'s create mark, as a create that keeps failing does.
    let hold = |id: String| {
        let images = manager.images().clone();
        async move {
            for _ in 0..300 {
                if let Ok(g) = images.begin(&id, "create") {
                    return g;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("disk {id} stayed busy");
        }
    };

    let (status, vm) = request(&app, "POST", "/vms", Some(json!({
        "name": "p1", "vcpu_count": 1, "mem_size_mib": 512, "image": "base", "root_disk_size_gib": 1
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    let vm_id = id_of(&vm);
    let root_id = vm_of(&vm)["root_disk"].as_str().unwrap().to_string();
    let busy = hold(root_id.clone()).await;
    let (status, v) = request(&app, "DELETE", &format!("/vms/{vm_id}"), None).await;
    assert!(
        status.is_success(),
        "taken while the create runs: {status} {v}"
    );
    // Still there while the create runs: the disk would be written back.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let (status, _) = request(&app, "GET", &format!("/disks/{root_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    drop(busy);
    assert!(gone(&app, &format!("/vms/{vm_id}")).await, "the VM went");
    assert!(
        gone(&app, &format!("/disks/{root_id}")).await,
        "its disk went"
    );

    // A disk on its own: the same.
    let (status, d) = request(
        &app,
        "POST",
        "/disks",
        Some(json!({
            "name": "d1", "size_gib": 1
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{d}");
    let disk_id = disk_of(&d)["id"].as_str().unwrap().to_string();
    let busy = hold(disk_id.clone()).await;
    let (status, v) = request(&app, "DELETE", &format!("/disks/{disk_id}"), None).await;
    assert!(
        status.is_success(),
        "taken while the create runs: {status} {v}"
    );
    let (status, _) = request(&app, "GET", &format!("/disks/{disk_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "kept until the create ends");
    drop(busy);
    assert!(
        gone(&app, &format!("/disks/{disk_id}")).await,
        "the disk went"
    );
}

#[tokio::test]
async fn deleting_an_image_cancels_its_download() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, _m) = create_app(tmp.path());

    let (status, img) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"url": format!("{base}/flaky-good.qcow2"), "name": "doomed"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let id = img["image"]["id"].as_str().unwrap().to_string();
    let (status, body) = request(&app, "DELETE", "/images/doomed?wait=10", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "gone in time: {body}");
    let (status, _) = request(&app, "GET", &format!("/images/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Give an aborted download a moment to have written, had it survived.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (status, _) = request(&app, "GET", &format!("/images/{id}"), None).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "not brought back by the download task"
    );
    let left: Vec<String> = std::fs::read_dir(tmp.path().join("images"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(&id))
        .collect();
    assert!(left.is_empty(), "files left: {left:?}");
    let (status, images) = request(&app, "GET", "/images", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        items(&images, "images")
            .iter()
            .all(|i| i["name"] != "doomed")
    );

    let img = pull_ready(&app, &base, "good.qcow2", "doomed", None).await;
    assert_eq!(img["name"], "doomed", "the name is free again");
}

#[tokio::test]
async fn firmware_images() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, _manager) = create_app(tmp.path());

    let (status, cat) = request(&app, "GET", "/images/firmware-catalog", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        items(&cat, "items")
            .iter()
            .any(|e| e["key"] == "cloudhv-edk2" && e["hypervisor"] == "HYPERVISOR_CLOUD_HYPERVISOR"),
        "{cat}"
    );

    // A firmware download names its hypervisor.
    let (status, err) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"url": format!("{base}/fw.fd"), "kind": "IMAGE_KIND_FIRMWARE"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let fw = pull_firmware(&app, &base, "ovmf-test", "qemu").await;
    assert_eq!(
        (fw["kind"].as_str(), fw["hypervisor"].as_str()),
        (Some("IMAGE_KIND_FIRMWARE"), Some("HYPERVISOR_QEMU"))
    );
    assert_eq!(fw["sha256"], sha256_hex(FIRMWARE));
    assert!(fw["path"].as_str().unwrap().ends_with(".fd"));
    let mode = std::fs::metadata(fw["path"].as_str().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o444);

    // Not a disk source, and only for its own hypervisor.
    let (status, err) = request(
        &app,
        "POST",
        "/disks",
        Some(json!({"name": "d", "image": "ovmf-test"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    pull_ready(&app, &base, "good.qcow2", "base", None).await;
    let (status, err) = request(&app, "POST", "/vms", Some(json!({
        "name": "wrong-hv", "vcpu_count": 1, "mem_size_mib": 512, "image": "base", "firmware": "ovmf-test"
    }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["message"].as_str().unwrap().contains("built for qemu"),
        "{err}"
    );
    let (status, err) = request(&app, "POST", "/vms", Some(json!({
        "name": "both", "vcpu_count": 1, "mem_size_mib": 512, "image": "base", "hypervisor": "HYPERVISOR_QEMU",
        "firmware": "ovmf-test", "firmware_path": "/x.fd"
    }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let (status, vm) = request(&app, "POST", "/vms", Some(json!({
        "name": "q", "vcpu_count": 1, "mem_size_mib": 512, "image": "base", "hypervisor": "HYPERVISOR_QEMU", "firmware": "ovmf-test"
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    assert_eq!(vm_of(&vm)["firmware"], fw["id"], "{vm}");

    // Free once the VM is gone; deleting removes the file.
    let (status, _) = request(
        &app,
        "DELETE",
        &format!("/vms/{}?wait=30", id_of(&vm)),
        None,
    )
    .await;
    // Never started: gone at once (204, spec/18-grpc.md §9.1).
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "DELETE", "/images/ovmf-test?wait=30", None).await;
    assert!(status.is_success(), "{status}");
    assert!(!Path::new(fw["path"].as_str().unwrap()).exists());
}

/// `{"firmware": "ovmf"}` copies the host's OVMF code and its variable
/// template into the image directory, ready at once.
#[tokio::test]
async fn ovmf_firmware_is_imported_from_the_host_package() {
    let Some(code) = glidex_control_plane::hypervisor::qemu::OVMF_CODE_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
    else {
        eprintln!("skipping: no OVMF installed");
        return;
    };
    let tmp = TempDir::new().unwrap();
    let (app, _manager) = create_app(tmp.path());
    let (status, img) = request(&app, "POST", "/images", Some(json!({"firmware": "ovmf"}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let img = image_of(&img);
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    assert_eq!(
        (img["name"].as_str(), img["hypervisor"].as_str()),
        (Some("ovmf"), Some("HYPERVISOR_QEMU"))
    );
    assert_eq!(
        img["source"]["firmware"]["url"],
        code.to_str().unwrap(),
        "{img}"
    );
    assert_eq!(
        std::fs::read(img["path"].as_str().unwrap()).unwrap(),
        std::fs::read(code).unwrap()
    );
    let vars = img["path"].as_str().unwrap().replace(".fd", ".vars.fd");
    assert!(
        Path::new(&vars).is_file(),
        "the variable store template comes along"
    );
    let (_, cat) = request(&app, "GET", "/images/firmware-catalog", None).await;
    let entry = items(&cat, "items")
        .iter()
        .find(|e| e["key"] == "ovmf")
        .unwrap()
        .clone();
    assert_eq!(entry["downloaded_image_id"], img["id"]);
}

/// Startup imports local firmware once: not again, and not after the
/// admin deleted it (spec/10-images.md §4.1 "Auto-import").
#[tokio::test]
async fn local_firmware_is_auto_imported_once() {
    use glidex_control_plane::images::AutoImport;
    if !glidex_control_plane::hypervisor::qemu::OVMF_CODE_CANDIDATES
        .iter()
        .any(|p| Path::new(p).is_file())
    {
        eprintln!("skipping: no OVMF installed");
        return;
    }
    let ovmf =
        |r: &[(&str, AutoImport)]| r.iter().find(|(k, _)| *k == "ovmf").map(|(_, a)| a.clone());
    let tmp = TempDir::new().unwrap();
    let (app, manager) = create_app(tmp.path());

    let first = manager.auto_import_firmware().await;
    assert_eq!(
        ovmf(&first),
        Some(AutoImport::Imported("ovmf".into())),
        "{first:?}"
    );
    let (_, img) = request(&app, "GET", "/images/ovmf", None).await;
    let img = image_of(&img);
    assert_eq!(
        (img["kind"].as_str(), img["status"]["state"].as_str()),
        (Some("IMAGE_KIND_FIRMWARE"), Some("IMAGE_STATE_READY")),
        "{img}"
    );
    // Cloud Hypervisor's entry depends on this host's ~/.glidex; it never downloads.
    let (_, all) = request(&app, "GET", "/images", None).await;
    assert!(
        items(&all, "images")
            .iter()
            .all(|i| i["status"]["state"] == "IMAGE_STATE_READY"),
        "{all}"
    );

    assert_eq!(
        ovmf(&manager.auto_import_firmware().await),
        Some(AutoImport::AlreadyDone)
    );
    let (status, _) = request(&app, "DELETE", "/images/ovmf?wait=30", None).await;
    assert!(status.is_success(), "{status}");

    // A restart doesn't bring the deleted image back.
    manager.stop_controllers().await;
    drop(app);
    drop(manager);
    let (app, manager) = create_app(tmp.path());
    assert_eq!(
        ovmf(&manager.auto_import_firmware().await),
        Some(AutoImport::AlreadyDone)
    );
    let (status, _) = request(&app, "GET", "/images/ovmf", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// An entry whose hypervisor already has firmware is marked done without
/// importing anything.
#[tokio::test]
async fn auto_import_leaves_existing_firmware_alone() {
    use glidex_control_plane::images::AutoImport;
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, manager) = create_app(tmp.path());
    pull_firmware(&app, &base, "my-ovmf", "qemu").await;
    let result = manager.auto_import_firmware().await;
    if let Some((_, r)) = result.iter().find(|(k, _)| *k == "ovmf") {
        assert_eq!(*r, AutoImport::HaveOne("my-ovmf".into()));
    }
    let (_, all) = request(&app, "GET", "/images", None).await;
    assert_eq!(
        items(&all, "images")
            .iter()
            .filter(|i| i["hypervisor"] == "HYPERVISOR_QEMU")
            .count(),
        1,
        "{all}"
    );
}

/// A QEMU firmware download can bring its variable-store template.
#[tokio::test]
async fn firmware_download_with_a_vars_template() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, _manager) = create_app(tmp.path());
    let fw = |name: &str, extra: Value| {
        let mut body = json!({"url": format!("{base}/fw.fd"), "sha256": sha256_hex(FIRMWARE), "name": name, "kind": "IMAGE_KIND_FIRMWARE", "hypervisor": "HYPERVISOR_QEMU"});
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };

    // Only for a QEMU firmware URL, and a digest needs the URL.
    let (status, err) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"url": format!("{base}/good.qcow2"), "vars_url": format!("{base}/vars.fd")})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let mut ch = fw("ch", json!({"vars_url": format!("{base}/vars.fd")}));
    ch["hypervisor"] = json!("HYPERVISOR_CLOUD_HYPERVISOR");
    let (status, err) = request(&app, "POST", "/images", Some(ch)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let (status, err) = request(
        &app,
        "POST",
        "/images",
        Some(fw("nourl", json!({"vars_sha256": sha256_hex(VARS)}))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");

    let (status, img) = request(
        &app,
        "POST",
        "/images",
        Some(fw(
            "with-vars",
            json!({"vars_url": format!("{base}/vars.fd"), "vars_sha256": sha256_hex(VARS)}),
        )),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let img = wait_image(&app, img["image"]["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    assert_eq!(
        (img["vars_template"].as_bool(), img["verified"].as_bool()),
        (Some(true), Some(true))
    );
    let vars = img["path"].as_str().unwrap().replace(".fd", ".vars.fd");
    assert_eq!(std::fs::read(&vars).unwrap(), VARS);
    assert_eq!(
        std::fs::metadata(&vars).unwrap().permissions().mode() & 0o777,
        0o444
    );

    // Without a vars digest it works but isn't verified; a wrong one fails.
    let (_, img) = request(
        &app,
        "POST",
        "/images",
        Some(fw(
            "unpinned-vars",
            json!({"vars_url": format!("{base}/vars.fd")}),
        )),
    )
    .await;
    let img = wait_image(&app, img["image"]["id"].as_str().unwrap()).await;
    assert_eq!(
        (img["status"]["state"].as_str(), img["verified"].as_bool()),
        (Some("IMAGE_STATE_READY"), Some(false)),
        "{img}"
    );
    let (_, img) = request(
        &app,
        "POST",
        "/images",
        Some(fw(
            "bad-vars",
            json!({"vars_url": format!("{base}/vars.fd"), "vars_sha256": sha256_hex(b"x")}),
        )),
    )
    .await;
    let img = wait_image(&app, img["image"]["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "IMAGE_STATE_FAILED", "{img}");
    assert!(
        img["status"]["reason"]
            .as_str()
            .unwrap()
            .contains("vars_url: checksum mismatch"),
        "{img}"
    );
    assert!(!Path::new(&img["path"].as_str().unwrap().replace(".fd", ".vars.fd")).exists());

    // Deleting removes the template too.
    let (status, _) = request(&app, "DELETE", "/images/with-vars?wait=30", None).await;
    assert!(status.is_success());
    assert!(!Path::new(&vars).exists());
}

/// The pinned Debian OVMF package: downloaded, checked and unpacked.
#[tokio::test]
#[ignore = "downloads Debian's ovmf package (6 MB) from snapshot.debian.org"]
async fn ovmf_debian_is_downloaded_and_unpacked() {
    let tmp = TempDir::new().unwrap();
    let (app, _manager) = create_app(tmp.path());
    let (status, cat) = request(&app, "GET", "/images/firmware-catalog", None).await;
    assert_eq!(status, StatusCode::OK);
    let entry = items(&cat, "items")
        .iter()
        .find(|e| e["key"] == "ovmf-debian")
        .unwrap()
        .clone();
    assert_eq!(
        (entry["source"].as_str(), entry["available"].as_bool()),
        (Some("debian"), Some(true)),
        "{entry}"
    );

    let (status, img) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"firmware": "ovmf-debian"})),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let mut img = image_of(&img);
    for _ in 0..600 {
        let (_, i) = request(
            &app,
            "GET",
            &format!("/images/{}", img["id"].as_str().unwrap()),
            None,
        )
        .await;
        img = image_of(&i);
        if !matches!(
            img["status"]["state"].as_str(),
            Some("IMAGE_STATE_DOWNLOADING" | "IMAGE_STATE_VERIFYING")
        ) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    assert_eq!(
        (
            img["hypervisor"].as_str(),
            img["vars_template"].as_bool(),
            img["verified"].as_bool()
        ),
        (Some("HYPERVISOR_QEMU"), Some(true), Some(true))
    );
    assert_eq!(
        img["sha256"], "624e06de18b4fa535e90db7160d00d3d07d206422b89999bf1e27d920264e4e0",
        "the unpacked OVMF_CODE_4M.fd"
    );
    assert_eq!(num(&img["file_size_bytes"]), 3653632);
    let path = img["path"].as_str().unwrap();
    assert!(!Path::new(&format!("{}.part", path.trim_end_matches(".fd"))).exists());
    assert_eq!(
        std::fs::metadata(path.replace(".fd", ".vars.fd"))
            .unwrap()
            .len(),
        540672
    );
}

/// Kernel images (spec/23-kernel-boot.md K1): a URL download with its
/// initramfs, checked digests and header, never a disk source.
#[tokio::test]
async fn kernel_images_are_downloaded_and_checked() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, _manager) = create_app(tmp.path());

    let k = pull_kernel(&app, &base, "vmlinuz-6.8.0-1-generic", "k1").await;
    assert_eq!(k["kind"], "IMAGE_KIND_KERNEL", "{k}");
    assert_eq!(k["verified"], true, "{k}");
    assert_eq!(k["kernel"]["release"], "6.8.0-1-generic", "{k}");
    assert_eq!(
        k["kernel"]["cmdline"],
        "root=LABEL=cloudimg-rootfs ro console=ttyS0"
    );
    assert_eq!(k["kernel"]["initrd"]["sha256"], sha256_hex(INITRD), "{k}");
    let path = k["path"].as_str().unwrap();
    assert!(path.ends_with(".kernel"), "{path}");
    let initrd = path.replace(".kernel", ".initrd");
    assert_eq!(std::fs::read(&initrd).unwrap(), INITRD);
    use std::os::unix::fs::PermissionsExt;
    for f in [path, initrd.as_str()] {
        let mode = std::fs::metadata(f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444, "{f}");
    }

    // The default name keeps the release; no initrd is fine.
    let (status, img) = request(
        &app,
        "POST",
        "/images",
        Some(json!({
            "url": format!("{base}/vmlinuz-6.9.0-2-generic"), "kind": "IMAGE_KIND_KERNEL"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let img = wait_image(&app, img["image"]["id"].as_str().unwrap()).await;
    assert_eq!(img["name"], "vmlinuz-6.9.0-2-generic", "{img}");
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    assert!(img["kernel"]["initrd"].is_null(), "{img}");
    assert_eq!(img["verified"], false, "no digest given: {img}");

    // A wrong initramfs digest, and a file that isn't a kernel, fail the image.
    let (_, bad) = request(&app, "POST", "/images", Some(json!({
        "url": format!("{base}/vmlinuz-6.8.0-1-generic"), "kind": "IMAGE_KIND_KERNEL", "name": "bad-initrd",
        "initrd_url": format!("{base}/initrd.img"), "initrd_sha256": sha256_hex(b"other"),
    }))).await;
    let bad = wait_image(&app, bad["image"]["id"].as_str().unwrap()).await;
    assert_eq!(bad["status"]["state"], "IMAGE_STATE_FAILED", "{bad}");
    assert!(
        bad["status"]["reason"]
            .as_str()
            .unwrap()
            .contains("initrd_url: checksum mismatch"),
        "{bad}"
    );
    let (_, junk) = request(
        &app,
        "POST",
        "/images",
        Some(json!({
            "url": format!("{base}/fw.fd"), "kind": "IMAGE_KIND_KERNEL", "name": "junk"
        })),
    )
    .await;
    let junk = wait_image(&app, junk["image"]["id"].as_str().unwrap()).await;
    assert_eq!(junk["status"]["state"], "IMAGE_STATE_FAILED", "{junk}");
    assert!(
        junk["status"]["reason"]
            .as_str()
            .unwrap()
            .contains("not a kernel"),
        "{junk}"
    );
    assert!(
        !tmp.path().join("images").read_dir().unwrap().any(|e| {
            let n = e.unwrap().file_name().to_string_lossy().into_owned();
            n.ends_with(".part")
        }),
        "no partial files left"
    );

    // Kernel-only fields need a kernel; the command line is checked.
    for body in [
        json!({"url": format!("{base}/good.qcow2"), "initrd_url": format!("{base}/initrd.img")}),
        json!({"url": format!("{base}/good.qcow2"), "cmdline": "ro"}),
        json!({"url": format!("{base}/vmlinuz-6.8.0-1-generic"), "kind": "IMAGE_KIND_KERNEL", "cmdline": "a\nb"}),
        json!({"url": format!("{base}/vmlinuz-6.8.0-1-generic"), "kind": "IMAGE_KIND_KERNEL", "initrd_sha256": sha256_hex(INITRD)}),
        json!({"kernel_path": "relative/vmlinuz"}),
        json!({"initrd_path": "/boot/initrd.img"}),
    ] {
        let (status, err) = request(&app, "POST", "/images", Some(body.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {err}");
    }

    // Never the source of a disk.
    let (status, err) = request(
        &app,
        "POST",
        "/disks",
        Some(json!({"name": "d", "image": "k1"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["message"].as_str().unwrap().contains("is a kernel"),
        "{err}"
    );

    // Deleted with both files.
    let (status, _) = request(&app, "DELETE", "/images/k1?wait=30", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!std::path::Path::new(path).exists() && !std::path::Path::new(&initrd).exists());
}

/// A kernel copied from host files (spec/23-kernel-boot.md §4.2): ready at
/// once, recorded as a host source, unverified.
#[tokio::test]
async fn kernel_images_from_host_files() {
    let tmp = TempDir::new().unwrap();
    let (app, _manager) = create_app(tmp.path());
    let kernel = tmp.path().join("vmlinuz-6.8.0-3-generic");
    let initrd = tmp.path().join("initrd.img-6.8.0-3-generic");
    std::fs::write(&kernel, bzimage("6.8.0-3-generic")).unwrap();
    std::fs::write(&initrd, INITRD).unwrap();
    let (status, img) = request(
        &app,
        "POST",
        "/images",
        Some(json!({
            "kernel_path": kernel, "initrd_path": initrd
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let img = image_of(&img);
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    assert_eq!(img["name"], "vmlinuz-6.8.0-3-generic");
    assert_eq!(
        img["source"]["host"]["path"],
        kernel.to_str().unwrap(),
        "{img}"
    );
    assert_eq!(img["verified"], false);
    assert_eq!(img["kernel"]["release"], "6.8.0-3-generic");
    assert_eq!(img["kernel"]["initrd"]["sha256"], sha256_hex(INITRD));

    // Not a kernel: refused before any record exists.
    let (status, err) = request(
        &app,
        "POST",
        "/images",
        Some(json!({
            "kernel_path": initrd, "name": "nope"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let (status, _) = request(&app, "GET", "/images/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// An aarch64 kernel wrapped as Ubuntu ships it (a stub PE whose `.linux`
/// is an EFI zboot of a gzip'd Image) is installed as the plain `Image`
/// Cloud Hypervisor boots, with that file's size and digest; the host's
/// file is left as it is (spec/23-kernel-boot.md K8).
#[tokio::test]
async fn wrapped_arm64_kernels_are_installed_plain() {
    use std::io::Write;
    let mut image = vec![0u8; 16384];
    image[..2].copy_from_slice(b"MZ");
    image[0x38..0x3c].copy_from_slice(b"ARM\x64");
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&image).unwrap();
    let payload = gz.finish().unwrap();
    let mut zboot = vec![0u8; 64];
    zboot[..2].copy_from_slice(b"MZ");
    zboot[4..8].copy_from_slice(b"zimg");
    zboot[8..12].copy_from_slice(&64u32.to_le_bytes());
    zboot[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    zboot[24..28].copy_from_slice(b"gzip");
    zboot.extend_from_slice(&payload);
    // The stub: a PE with one section, `.linux`.
    let mut stub = vec![0u8; 0x200];
    stub[..2].copy_from_slice(b"MZ");
    stub[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    stub[0x80..0x84].copy_from_slice(b"PE\0\0");
    stub[0x84..0x86].copy_from_slice(&0xaa64u16.to_le_bytes());
    stub[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
    let section = 0x80 + 24;
    stub[section..section + 6].copy_from_slice(b".linux");
    stub[section + 16..section + 20].copy_from_slice(&(zboot.len() as u32).to_le_bytes());
    stub[section + 20..section + 24].copy_from_slice(&0x200u32.to_le_bytes());
    stub.extend_from_slice(&zboot);

    let tmp = TempDir::new().unwrap();
    let (app, _manager) = create_app(tmp.path());
    let kernel = tmp.path().join("vmlinuz-7.0.0-38-generic");
    std::fs::write(&kernel, &stub).unwrap();
    let (status, img) = request(
        &app,
        "POST",
        "/images",
        Some(json!({ "kernel_path": kernel, "arch": "ARCH_AARCH64", "name": "arm" })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let img = image_of(&img);
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    assert_eq!(img["arch"], "ARCH_AARCH64", "{img}");
    assert_eq!(
        img["sha256"],
        sha256_hex(&image),
        "the plain Image's digest: {img}"
    );
    assert_eq!(num(&img["virtual_size_bytes"]), image.len() as u64, "{img}");
    assert_eq!(
        std::fs::read(&kernel).unwrap(),
        stub,
        "the host file is unchanged"
    );
}

/// VMs boot kernel images (spec/23-kernel-boot.md K2): no firmware needed for
/// an image VM, the kernel's command line by default, a generated seed
/// and so a credential, in-use protection, and a PATCH to another kernel.
#[tokio::test]
async fn vms_boot_kernel_images() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, manager) = create_app(tmp.path());
    pull_ready(&app, &base, "good.qcow2", "base", None).await;
    let k1 = pull_kernel(&app, &base, "vmlinuz-6.8.0-1-generic", "k1").await;
    let k2 = pull_kernel(&app, &base, "vmlinuz-6.9.0-2-generic", "k2").await;
    let (status, c) = request(
        &app,
        "POST",
        "/credentials",
        Some(json!({
            "username": "kuser", "password": "a-long-enough-password"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{c}");

    // No firmware image exists: kernel boot doesn't need one.
    let (status, vm) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({
            "name": "kb", "vcpu_count": 1, "mem_size_mib": 512, "image": "base",
            "root_disk_size_gib": 1, "kernel": "k1", "credential": "kuser"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    let v = vm_of(&vm);
    assert_eq!(v["kernel"], k1["id"], "{vm}");
    assert_eq!(v["boot"], "BOOT_MODE_KERNEL", "{vm}");
    assert!(v["firmware"].is_null(), "{vm}");
    assert!(
        v["warnings"].is_null(),
        "the kernel's cmdline has a root=: {vm}"
    );
    let id = id_of(&vm);
    let c = manager.get_vm(&id).await.unwrap().spec.config;
    assert_eq!(c.kernel_image.as_deref(), k1["id"].as_str());
    assert_eq!(c.kernel_image_path, k1["path"].as_str().unwrap());
    assert_eq!(
        c.initrd_path.as_deref(),
        Some(
            k1["path"]
                .as_str()
                .unwrap()
                .replace(".kernel", ".initrd")
                .as_str()
        )
    );
    assert_eq!(c.kernel_args, "root=LABEL=cloudimg-rootfs ro console=ttyS0");
    assert_eq!(c.generate_seed, Some(true));
    assert!(c.generates_seed());
    assert!(c.firmware_path.is_none());

    // One boot mode; a kernel image isn't firmware, nor the other way round.
    let fw = pull_firmware(&app, &base, "edk2", "cloudhypervisor").await;
    for (body, what) in [
        (
            json!({"kernel": "k1", "firmware": "edk2"}),
            "kernel and firmware",
        ),
        (
            json!({"kernel": "k1", "kernel_image_path": "/k"}),
            "two kernels",
        ),
        (json!({"kernel": "edk2"}), "firmware as a kernel"),
        (json!({"firmware": "k1"}), "a kernel as firmware"),
        (
            json!({"kernel": "k1", "generate_seed": true, "cloud_init_path": "/seed.img"}),
            "two seeds",
        ),
    ] {
        let mut b = json!({"name": "x", "vcpu_count": 1, "mem_size_mib": 512, "image": "base"});
        b.as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        let (status, err) = request(&app, "POST", "/vms", Some(b)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {err}");
    }

    // Explicit arguments win; without root= on a cloud image, a warning.
    let (status, vm2) = request(
        &app,
        "POST",
        "/vms",
        Some(json!({
            "name": "kb2", "vcpu_count": 1, "mem_size_mib": 512, "image": "base",
            "root_disk_size_gib": 1, "kernel": "k1", "kernel_args": "console=ttyS0"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{vm2}");
    assert!(
        vm_of(&vm2)["warnings"].to_string().contains("no root="),
        "{vm2}"
    );

    // In use: the kernel stays.
    let (status, err) = request(&app, "DELETE", "/images/k1", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");
    let (_, img) = request(&app, "GET", "/images/k1", None).await;
    assert_eq!(img["image"]["used_by_vms"], json!(["kb", "kb2"]), "{img}");

    // Another kernel at the next launch (K6), the same architecture.
    let (status, body) = request(
        &app,
        "PATCH",
        &format!("/vms/{id}"),
        Some(json!({
            "change": {"kernel": "k2"}, "update_mask": "kernel"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(vm_of(&body)["kernel"], k2["id"], "{body}");
    let c = manager.get_vm(&id).await.unwrap().spec.config;
    assert_eq!(c.kernel_image_path, k2["path"].as_str().unwrap());
    // Not to firmware, and not on a VM without a kernel image.
    let (status, err) = request(
        &app,
        "PATCH",
        &format!("/vms/{id}"),
        Some(json!({
            "change": {"kernel": "edk2"}, "update_mask": "kernel"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let (status, fwvm) = request(&app, "POST", "/vms", Some(json!({
        "name": "fwvm", "vcpu_count": 1, "mem_size_mib": 512, "image": "base", "root_disk_size_gib": 1
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{fwvm}");
    assert_eq!(vm_of(&fwvm)["boot"], "BOOT_MODE_FIRMWARE", "{fwvm}");
    assert_eq!(vm_of(&fwvm)["firmware"], fw["id"]);
    let (status, err) = request(
        &app,
        "PATCH",
        &format!("/vms/{}", id_of(&fwvm)),
        Some(json!({
            "change": {"kernel": "k2"}, "update_mask": "kernel"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let (status, err) = request(
        &app,
        "PATCH",
        &format!("/vms/{id}"),
        Some(json!({
            "change": {}, "update_mask": "generate_seed"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");

    // k1 is used by kb2 only now.
    let (_, img) = request(&app, "GET", "/images/k1", None).await;
    assert_eq!(img["image"]["used_by_vms"], json!(["kb2"]), "{img}");
}

/// `kernel_from` and `with_kernel` (spec/23-kernel-boot.md §10, K10), on a
/// host where extraction is off: refused clearly, and a disk pull that
/// asks for its kernel still pulls the disk.
#[tokio::test]
async fn kernel_extraction_requests() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, _manager) = create_app(tmp.path());
    pull_ready(&app, &base, "good.qcow2", "base", None).await;
    pull_firmware(&app, &base, "edk2", "cloudhypervisor").await;

    let (status, err) = request(
        &app,
        "POST",
        "/images",
        Some(json!({"kernel_from": "base"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("can't be extracted"),
        "{err}"
    );
    for body in [
        json!({"kernel_from": "base", "url": format!("{base}/good.qcow2")}),
        json!({"kernel_from": "base", "cmdline": "ro"}),
        json!({"kernel_from": "base", "kind": "IMAGE_KIND_DISK"}),
        json!({"kernel_from": "edk2"}),
        json!({"kernel_from": "nope"}),
        json!({"firmware": "cloudhv-edk2", "with_kernel": true}),
        json!({"url": format!("{base}/vmlinuz-6.8.0-1-generic"), "kind": "IMAGE_KIND_KERNEL", "with_kernel": true}),
    ] {
        let (status, err) = request(&app, "POST", "/images", Some(body.clone())).await;
        assert!(status.is_client_error(), "{body}: {status} {err}");
    }

    // The disk comes anyway; no kernel record without an extractor.
    let (status, img) = request(
        &app,
        "POST",
        "/images",
        Some(json!({
            "url": format!("{base}/good.qcow2"), "name": "with-k", "with_kernel": true
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let img = wait_image(&app, img["image"]["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "IMAGE_STATE_READY", "{img}");
    let (_, all) = request(&app, "GET", "/images", None).await;
    assert!(
        !items(&all, "images")
            .iter()
            .any(|i| i["kind"] == "IMAGE_KIND_KERNEL"),
        "{all}"
    );
}

/// `kernel_from_disk` and `POST /vms/{id}/extract-kernel` (spec/23-kernel-boot.md
/// K11, §10.5) on a host where extraction is off: the admission rules, a
/// manual `kernel` turning following off, and clear refusals.
#[tokio::test]
async fn kernel_from_disk_requests() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, manager) = create_app(tmp.path());
    pull_ready(&app, &base, "good.qcow2", "base", None).await;
    pull_kernel(&app, &base, "vmlinuz-6.8.0-1-generic", "k1").await;
    pull_kernel(&app, &base, "vmlinuz-6.9.0-2-generic", "k2").await;
    pull_firmware(&app, &base, "edk2", "cloudhypervisor").await;

    let vm = |extra: Value| {
        let mut b = json!({"name": "f", "vcpu_count": 1, "mem_size_mib": 512});
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b
    };
    for (body, why) in [
        (
            vm(json!({"image": "base", "kernel_from_disk": true})),
            "no kernel image",
        ),
        (
            vm(json!({"rootfs_path": "/r.img", "kernel": "k1", "kernel_from_disk": true})),
            "no managed disk",
        ),
    ] {
        let (status, err) = request(&app, "POST", "/vms", Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {err}");
        assert!(
            err["message"]
                .as_str()
                .unwrap()
                .contains("kernel_from_disk"),
            "{err}"
        );
    }
    let (status, v) = request(
        &app,
        "POST",
        "/vms",
        Some(vm(json!({
            "image": "base", "root_disk_size_gib": 1, "kernel": "k1", "kernel_from_disk": true
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(vm_of(&v)["kernel_from_disk"], true, "{v}");
    let id = id_of(&v);

    // Choosing a kernel by hand stops following the guest's.
    let (status, body) = request(
        &app,
        "PATCH",
        &format!("/vms/{id}"),
        Some(json!({
            "change": {"kernel": "k2"}, "update_mask": "kernel"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(vm_of(&body)["kernel_from_disk"].is_null(), "{body}");
    // And back on, with both in the mask keeping it.
    let (status, body) = request(&app, "PATCH", &format!("/vms/{id}"), Some(json!({
        "change": {"kernel": "k1", "kernel_from_disk": true}, "update_mask": "kernel,kernel_from_disk"
    }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(vm_of(&body)["kernel_from_disk"], true, "{body}");
    assert!(
        manager
            .get_vm(&id)
            .await
            .unwrap()
            .spec
            .config
            .kernel_from_disk
    );

    // Extracting now: off on this host; a firmware VM can't at all.
    let (status, err) = request(&app, "POST", &format!("/vms/{id}/extract-kernel"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(
        err["message"]
            .as_str()
            .unwrap()
            .contains("can't be extracted"),
        "{err}"
    );
    let (status, fw) = request(
        &app,
        "POST",
        "/vms",
        Some(vm(json!({
            "name": "fw", "image": "base", "root_disk_size_gib": 1
        }))),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{fw}");
    let (status, err) = request(
        &app,
        "POST",
        &format!("/vms/{}/extract-kernel", id_of(&fw)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let (status, err) = request(
        &app,
        "PATCH",
        &format!("/vms/{}", id_of(&fw)),
        Some(json!({
            "change": {"kernel_from_disk": true}, "update_mask": "kernel_from_disk"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    let (status, _) = request(&app, "POST", "/vms/nope/extract-kernel", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
