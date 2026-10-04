//! Images and disks through the REST API (spec/images.md §12).
//!
//! Images are served by a local HTTP server (ETag + Range support, plus a
//! path that drops the connection part way), so these tests need no
//! network. They need `qemu-img`, `qemu-io`, `sgdisk` and `growpart`, and
//! are skipped (with a note) where those are missing.

use axum::{
    body::{Body, Bytes},
    extract::{Path as AxPath, State},
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tower::ServiceExt;

use glidex_control_plane::api::create_router;
use glidex_control_plane::images::qemu_img;
use glidex_control_plane::state::VmManager;

const MIB: u64 = 1024 * 1024;

fn tools_present() -> bool {
    let missing: Vec<_> = qemu_img::ALL_TOOLS.iter().filter(|t| !t.available()).map(|t| t.name).collect();
    if !missing.is_empty() {
        eprintln!("skipping: missing {}", missing.join(", "));
    }
    missing.is_empty()
}

fn run_ok(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{:?}: {}", cmd, String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{:02x}", b)).collect()
}

/// A 32 MiB cloud-image-like disk: GPT, 8 MiB ESP, 16 MiB root (x86-64
/// root type) with a marker inside, free space after.
fn fixture_raw(dir: &Path) -> PathBuf {
    let raw = dir.join("fixture.raw");
    std::fs::File::create(&raw).unwrap().set_len(32 * MIB).unwrap();
    run_ok(Command::new(qemu_img::SGDISK.find().unwrap()).args([
        "-n", "1:2048:+8M", "-t", "1:ef00",
        "-n", "2:0:+16M", "-t", "2:8304",
    ]).arg(&raw));
    use std::os::unix::fs::FileExt;
    std::fs::OpenOptions::new().write(true).open(&raw).unwrap().write_all_at(b"GLIDEX-ROOT-MARKER", 12 * MIB).unwrap();
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
    run_ok(Command::new("qemu-img").args(["convert", "-q", "-f", "raw", "-O", "qcow2"]).arg(&raw).arg(&q));
    let b = dir.join("backing.qcow2");
    run_ok(Command::new("qemu-img").args(["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b"]).arg(&q).arg(&b));
    Fixtures { qcow2: std::fs::read(&q).unwrap(), raw: std::fs::read(&raw).unwrap(), with_backing: std::fs::read(&b).unwrap() }
}

#[derive(Clone)]
struct Served {
    files: Arc<HashMap<String, Vec<u8>>>,
    hits: Arc<AtomicUsize>,
}

/// `GET /<name>` with a strong ETag and `Range: bytes=N-` + `If-Range`.
/// `flaky-*`: the first response stops after half the body.
async fn serve(State(st): State<Served>, AxPath(name): AxPath<String>, headers: HeaderMap) -> Response {
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
        h.insert("content-range", format!("bytes {}-{}/{}", s, data.len() - 1, data.len()).parse().unwrap());
    }
    resp
}

async fn start_server(fx: &Fixtures) -> (String, Arc<AtomicUsize>) {
    let mut files = HashMap::new();
    files.insert("good.qcow2".to_string(), fx.qcow2.clone());
    files.insert("disk.raw".to_string(), fx.raw.clone());
    files.insert("backing.qcow2".to_string(), fx.with_backing.clone());
    let hits = Arc::new(AtomicUsize::new(0));
    let st = Served { files: Arc::new(files), hits: hits.clone() };
    let app = Router::new().route("/{name}", get(serve)).with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{}", addr), hits)
}

fn create_app(dir: &Path) -> (Router, Arc<VmManager>) {
    // Local plain-http test server: private addresses must be allowed.
    // Every test sets the same value, so parallel tests don't race on it.
    std::env::set_var("GLIDEX_ALLOW_PRIVATE_IMAGE_URLS", "1");
    let manager = VmManager::with_db_path(dir.join("test.db")).unwrap();
    // Disks are made, resized and extended by the disk controller
    // (spec/reconciliation.md §10.1); the tests wait for it with ?wait.
    manager.start_controllers();
    (create_router(manager.clone()), manager)
}

/// The disk once the disk controller has made it.
async fn ready_disk(app: &Router, key: &str) -> Value {
    for _ in 0..150 {
        let (_, d) = request(app, "GET", &format!("/disks/{key}"), None).await;
        if d["status"] == "ready" {
            return d;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("disk {key} never became ready");
}

/// Reasons and messages of a disk's events, newest last.
async fn disk_events(app: &Router, key: &str) -> Vec<(String, String)> {
    let (_, d) = request(app, "GET", &format!("/disks/{key}"), None).await;
    let (_, ev) = request(app, "GET", &format!("/disks/{}/events", d["id"].as_str().unwrap()), None).await;
    ev["events"].as_array().unwrap().iter().map(|e| (e["reason"].as_str().unwrap().to_string(), e["message"].as_str().unwrap().to_string())).collect()
}

async fn request(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(b) => {
            builder = builder.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Poll an image until it leaves downloading/verifying.
async fn wait_image(app: &Router, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, img) = request(app, "GET", &format!("/images/{id}"), None).await;
        assert_eq!(status, StatusCode::OK, "{img}");
        let state = img["status"]["state"].as_str().unwrap().to_string();
        if state != "downloading" && state != "verifying" {
            return img;
        }
        assert!(Instant::now() < deadline, "image {id} stuck: {img}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn pull_ready(app: &Router, base: &str, file: &str, name: &str, sha: Option<&str>) -> Value {
    let mut body = json!({"url": format!("{base}/{file}"), "name": name});
    if let Some(s) = sha {
        body["sha256"] = json!(s);
    }
    let (status, img) = request(app, "POST", "/images", Some(body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let img = wait_image(app, img["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "ready", "{img}");
    img
}

fn sgdisk_verify(disk: &Path, format: &str, dir: &Path) -> String {
    let raw = dir.join(format!("verify-{}.raw", uuid::Uuid::new_v4()));
    run_ok(Command::new("qemu-img").args(["convert", "-q", "-f", format, "-O", "raw"]).arg(disk).arg(&raw));
    let out = run_ok(Command::new(qemu_img::SGDISK.find().unwrap()).arg("-v").arg(&raw));
    let _ = std::fs::remove_file(&raw);
    out
}

fn marker_intact(disk: &Path, format: &str, dir: &Path) -> bool {
    let out = dir.join(format!("marker-{}.bin", uuid::Uuid::new_v4()));
    // qemu-img dd's count includes the skipped blocks.
    run_ok(Command::new("qemu-img").args(["dd", "-f", format, "-O", "raw", "bs=512"])
        .arg(format!("skip={}", 12 * MIB / 512)).arg(format!("count={}", 12 * MIB / 512 + 1))
        .arg(format!("if={}", disk.display())).arg(format!("of={}", out.display())));
    let data = std::fs::read(&out).unwrap();
    data.starts_with(b"GLIDEX-ROOT-MARKER")
}

#[tokio::test]
async fn catalog_lists_host_images() {
    let tmp = TempDir::new().unwrap();
    let (app, _m) = create_app(tmp.path());
    let (status, cat) = request(&app, "GET", "/images/catalog", None).await;
    assert_eq!(status, StatusCode::OK);
    let keys: Vec<_> = cat.as_array().unwrap().iter().map(|e| e["key"].as_str().unwrap().to_string()).collect();
    assert!(keys.contains(&"ubuntu-26.04".to_string()), "{keys:?}");
    assert!(cat[0]["downloaded_image_id"].is_null());

    let (status, err) = request(&app, "POST", "/images", Some(json!({"catalog": "nope"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"], "invalid_image");
    let (status, _) = request(&app, "POST", "/images", Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, err) = request(&app, "POST", "/images", Some(json!({"url": "file:///etc/passwd"}))).await;
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
    assert_eq!(img["virtual_size_bytes"], 32 * MIB);
    let img_path = PathBuf::from(img["path"].as_str().unwrap());
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&img_path).unwrap().permissions().mode() & 0o777, 0o444);
    assert!(!img_path.with_extension("qcow2.part").exists());

    // Same name again: conflict.
    let (status, _) = request(&app, "POST", "/images", Some(json!({"url": format!("{base}/good.qcow2"), "name": "base"}))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Smaller than the image: refused with the minimum.
    let (status, err) = request(&app, "POST", "/disks?wait=30", Some(json!({"name": "tiny", "image": "base", "size_bytes": 16 * MIB}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert_eq!(err["error"], "invalid_disk");
    assert_eq!(err["details"]["min_size_bytes"], 32 * MIB);

    // Linked, larger: root partition extended offline.
    let (status, d) = request(&app, "POST", "/disks?wait=30", Some(json!({"name": "web-root", "image": "base", "size_bytes": 64 * MIB}))).await;
    assert_eq!(status, StatusCode::CREATED, "{d}");
    assert_eq!(d["origin"]["mode"], "linked");
    let (_, detail) = request(&app, "GET", "/disks/web-root", None).await;
    assert_eq!(detail["info"]["backing-filename"], img_path.to_str().unwrap());
    let parts = detail["partition_table"]["partitions"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[1]["is_root"], true);
    assert_eq!(parts[1]["start_bytes"].as_u64().unwrap() + parts[1]["size_bytes"].as_u64().unwrap(), 64 * MIB - 33 * 512);
    assert_eq!(detail["partition_table"]["free_tail_bytes"], 0);
    let linked_path = PathBuf::from(d["path"].as_str().unwrap());
    assert!(sgdisk_verify(&linked_path, "qcow2", tmp.path()).contains("No problems found"));
    assert!(marker_intact(&linked_path, "qcow2", tmp.path()));

    // The image cannot go while the linked disk exists.
    let (status, err) = request(&app, "DELETE", "/images/base", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{err}");
    assert!(err["message"].as_str().unwrap().contains("web-root"));

    // Full clone, default size (GLIDEX_DEFAULT_ROOT_GIB is 10 GiB).
    let (status, full) = request(&app, "POST", "/disks?wait=30", Some(json!({"name": "copy", "image": "base", "clone": "full", "format": "raw", "extend_root": false}))).await;
    assert_eq!(status, StatusCode::CREATED, "{full}");
    assert_eq!(full["size_bytes"], 10 * 1024 * MIB);
    assert_eq!(full["format"], "raw");

    // Raw + linked is impossible.
    let (status, _) = request(&app, "POST", "/disks?wait=30", Some(json!({"name": "x", "image": "base", "format": "raw"}))).await;
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
    assert_eq!(disks.as_array().unwrap().len(), 1);
    assert_eq!(disks[0]["name"], "copy");
    assert_eq!(disks[0]["status"], "ready");
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

    let (status, img) = request(&app, "POST", "/images", Some(json!({"url": format!("{base}/good.qcow2"), "name": "bad", "sha256": "0".repeat(64)}))).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let img = wait_image(&app, img["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "failed");
    assert!(img["status"]["reason"].as_str().unwrap().contains("checksum mismatch"), "{img}");
    let images_dir = tmp.path().join("images");
    assert_eq!(std::fs::read_dir(&images_dir).unwrap().count(), 0, "partial file left behind");

    let (_, img) = request(&app, "POST", "/images", Some(json!({"url": format!("{base}/backing.qcow2"), "name": "evil"}))).await;
    let img = wait_image(&app, img["id"].as_str().unwrap()).await;
    assert_eq!(img["status"]["state"], "failed");
    assert!(img["status"]["reason"].as_str().unwrap().contains("backing file"), "{img}");
    assert_eq!(img["verified"], false);

    // A failed image can be deleted.
    let (status, _) = request(&app, "DELETE", "/images/evil", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "POST", "/disks?wait=30", Some(json!({"name": "d", "image": "bad"}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "a failed image is not ready");

    // Retry (D19): downloaded again; the digest still doesn't match.
    let (_, before) = request(&app, "GET", "/images/bad", None).await;
    let (status, r) = request(&app, "POST", "/images/bad/retry", None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{r}");
    // The image controller acts on the retry a moment later; until then the
    // image still reads "failed" from before.
    let id = before["id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (_, ev) = request(&app, "GET", &format!("/images/{id}/events"), None).await;
        let reasons: Vec<String> = ev["events"].as_array().unwrap().iter().map(|e| e["reason"].as_str().unwrap().to_string()).collect();
        if reasons.iter().any(|r| r == "Retrying") {
            break;
        }
        assert!(Instant::now() < deadline, "the controller never retried: {reasons:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let img = wait_image(&app, id).await;
    assert_eq!(img["status"]["state"], "failed");
    assert!(img["status"]["reason"].as_str().unwrap().contains("checksum mismatch"), "{img}");
    // Only a failed image can be retried.
    let (status, _) = request(&app, "POST", "/images/nonexistent/retry", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// spec/reconciliation.md §7.6, §10.1: a disk may be created from an image
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
    let (status, img) = request(&app, "POST", "/images", Some(json!({"url": format!("{base}/flaky-good.qcow2"), "name": "slow"}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let (status, d) = request(&app, "POST", "/disks", Some(json!({"name": "early", "image": "slow", "size_bytes": 64 * MIB}))).await;
    assert_eq!(status, StatusCode::CREATED, "{d}");
    assert_eq!(d["status"], "pending", "{d}");
    // The image can't be deleted from under a disk waiting for it.
    let (status, e) = request(&app, "DELETE", "/images/slow", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{e}");
    assert!(e["message"].as_str().unwrap().contains("early"), "{e}");
    let d = ready_disk(&app, "early").await;
    assert_eq!(d["size_bytes"], 64 * MIB);
    let reasons: Vec<String> = disk_events(&app, "early").await.into_iter().map(|e| e.0).collect();
    assert_eq!(reasons, ["Created", "Created"], "recorded by the API, made by the controller");

    // The file goes away behind glidex's back.
    std::fs::remove_file(d["path"].as_str().unwrap()).unwrap();
    let mut missing = false;
    for _ in 0..200 {
        // Resync is every 30 s; a resize request reconciles it at once.
        let _ = request(&app, "POST", "/disks/early/resize", Some(json!({"size_bytes": 80 * MIB}))).await;
        let (_, d) = request(&app, "GET", "/disks/early", None).await;
        if d["status"] == "missing" {
            missing = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(missing, "the lost file is reported");
    assert!(!tmp.path().join("disks").read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains(d["id"].as_str().unwrap())));
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
    assert_eq!(hits.load(Ordering::SeqCst), 2, "expected one dropped attempt and one ranged resume");

    // A raw download is stored as qcow2; the stored digest is the qcow2's.
    let raw_sha = sha256_hex(&fx.raw);
    let img = pull_ready(&app, &base, "disk.raw", "fromraw", Some(&raw_sha)).await;
    assert_eq!(img["format"], "qcow2");
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
    let (status, blank) = request(&app, "POST", "/disks?wait=30", Some(json!({"name": "blank", "size_gib": 1}))).await;
    assert_eq!(status, StatusCode::CREATED, "{blank}");
    let (status, d) = request(&app, "POST", "/disks/blank/resize?wait=30", Some(json!({"size_gib": 2}))).await;
    assert_eq!(status, StatusCode::OK, "{d}");
    assert_eq!(d["size_bytes"], 2 * 1024 * MIB);
    let (status, err) = request(&app, "POST", "/disks/blank/resize?wait=30", Some(json!({"size_gib": 1}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert!(err["message"].as_str().unwrap().contains("no partition table"));

    for format in ["qcow2", "raw"] {
        let name = format!("img-{format}");
        let (status, d) = request(&app, "POST", "/disks?wait=30", Some(json!({
            "name": name, "image": "base", "clone": "full", "format": format,
            "size_bytes": 48 * MIB, "extend_root": false
        }))).await;
        assert_eq!(status, StatusCode::CREATED, "{d}");
        let path = PathBuf::from(d["path"].as_str().unwrap());

        // Root partition ends at 25 MiB (+ GPT backup) → minimum 26 MiB.
        let (status, err) = request(&app, "POST", &format!("/disks/{name}/resize?wait=30"), Some(json!({"size_bytes": 16 * MIB}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
        assert_eq!(err["details"]["min_size_bytes"], 26 * MIB);
        let (status, d) = request(&app, "POST", &format!("/disks/{name}/resize?wait=30"), Some(json!({"size_bytes": 26 * MIB}))).await;
        assert_eq!(status, StatusCode::OK, "{d}");
        assert_eq!(d["size_bytes"], 26 * MIB);
        assert_eq!(qemu_img::info(&path, None).unwrap().virtual_size, 26 * MIB);
        assert!(sgdisk_verify(&path, format, tmp.path()).contains("No problems found"));
        assert!(marker_intact(&path, format, tmp.path()));

        // Grow: from an image, extend_root defaults to true.
        let (status, d) = request(&app, "POST", &format!("/disks/{name}/resize?wait=30"), Some(json!({"size_bytes": 80 * MIB}))).await;
        assert_eq!(status, StatusCode::OK, "{d}");
        // What the controller did is in the disk's events.
        assert!(disk_events(&app, &name).await.last().unwrap().1.contains("Grown"), "{:?}", disk_events(&app, &name).await);
        let (_, detail) = request(&app, "GET", &format!("/disks/{name}"), None).await;
        assert_eq!(detail["partition_table"]["free_tail_bytes"], 0, "{detail}");
        assert!(sgdisk_verify(&path, format, tmp.path()).contains("No problems found"));
        assert!(marker_intact(&path, format, tmp.path()));

        // Nothing left to grow; on-boot just sets the flag.
        let (status, d) = request(&app, "POST", &format!("/disks/{name}/extend-root?wait=30"), Some(json!({}))).await;
        assert_eq!(status, StatusCode::OK, "{d}");
        assert_eq!(disk_events(&app, &name).await.last().unwrap(), &("RootExtended".to_string(), "AlreadyFull".to_string()));
        let (_, d) = request(&app, "POST", &format!("/disks/{name}/extend-root?wait=30"), Some(json!({"mode": "on-boot"}))).await;
        assert_eq!(disk_events(&app, &name).await.last().unwrap(), &("RootExtended".to_string(), "OnBoot".to_string()));
        assert_eq!(d["pending_growpart"], true);
    }

    // Linked disks cannot shrink below their image.
    let (_, _) = request(&app, "POST", "/disks?wait=30", Some(json!({"name": "lnk", "image": "base", "size_bytes": 40 * MIB, "extend_root": false}))).await;
    let (status, err) = request(&app, "POST", "/disks/lnk/resize?wait=30", Some(json!({"size_bytes": 26 * MIB}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    assert_eq!(err["details"]["min_size_bytes"], 32 * MIB);
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

    // VM from an image: owned, linked root disk named after the VM.
    let (status, vm) = request(&app, "POST", "/vms", Some(json!({
        "name": "web 1", "vcpu_count": 1, "mem_size_mib": 512, "image": "base", "root_disk_size_gib": 1
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    let vm_id = vm["id"].as_str().unwrap().to_string();
    let root_id = vm["root_disk"].as_str().unwrap().to_string();
    // Recorded with the VM, made by the disk controller.
    let root = ready_disk(&app, &root_id).await;
    assert_eq!(root["name"], "web-1-root");
    assert_eq!(root["attached_to"], vm_id.as_str());
    assert_eq!(root["size_bytes"], 1024 * MIB);
    let internal = manager.get_vm(&vm_id).await.unwrap();
    assert_eq!(internal.spec.config.rootfs_path, root["path"].as_str().unwrap());
    assert!(internal.spec.config.owns_root_disk);
    assert!(internal.spec.config.firmware_path.is_some(), "image implies firmware boot");

    // Attached: no delete; a Created VM's disk can still be resized.
    let (status, _) = request(&app, "DELETE", &format!("/disks/{root_id}"), None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, d) = request(&app, "POST", &format!("/disks/{root_id}/resize?wait=30"), Some(json!({"size_gib": 2}))).await;
    assert_eq!(status, StatusCode::OK, "{d}");

    // Exactly one root source.
    let (status, _) = request(&app, "POST", "/vms", Some(json!({
        "name": "both", "vcpu_count": 1, "mem_size_mib": 512, "image": "base", "rootfs_path": "/x"
    }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Data disks: attach/detach are spec writes (202); one VM per disk.
    let (_, _) = request(&app, "POST", "/disks?wait=30", Some(json!({"name": "data1", "size_gib": 1}))).await;
    let (status, v) = request(&app, "POST", &format!("/vms/{vm_id}/disks"), Some(json!({"disk": "data1"}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{v}");
    assert_eq!(v["data_disks"].as_array().unwrap().len(), 1);
    let (status, _) = request(&app, "POST", "/vms", Some(json!({
        "name": "other", "vcpu_count": 1, "mem_size_mib": 512, "kernel_image_path": "/k", "rootfs_path": "/x", "data_disks": ["data1"]
    }))).await;
    assert_eq!(status, StatusCode::CONFLICT, "disk already attached");
    let (status, _) = request(&app, "DELETE", &format!("/vms/{vm_id}/disks/{root_id}"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "root disk is not a data disk");
    let (status, v) = request(&app, "DELETE", &format!("/vms/{vm_id}/disks/data1"), None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{v}");
    assert!(v.get("data_disks").is_none());

    // VM on an existing root disk plus a data disk: deleting the VM keeps both.
    let (_, _) = request(&app, "POST", "/disks?wait=30", Some(json!({"name": "mine", "image": "base"}))).await;
    let (status, vm2) = request(&app, "POST", "/vms", Some(json!({
        "name": "vm2", "vcpu_count": 1, "mem_size_mib": 512, "root_disk": "mine", "data_disks": ["data1"]
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{vm2}");
    let vm2_id = vm2["id"].as_str().unwrap();
    let (status, _) = request(&app, "DELETE", &format!("/vms/{vm2_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for name in ["mine", "data1"] {
        let (_, d) = request(&app, "GET", &format!("/disks/{name}"), None).await;
        assert_eq!(d["status"], "ready", "{d}");
        assert!(d["attached_to"].is_null(), "{d}");
    }

    // keep_disk=true keeps an owned root disk, detached.
    let (_, vm3) = request(&app, "POST", "/vms", Some(json!({"name": "vm3", "vcpu_count": 1, "mem_size_mib": 512, "image": "base"}))).await;
    let vm3_root = vm3["root_disk"].as_str().unwrap().to_string();
    let (status, _) = request(&app, "DELETE", &format!("/vms/{}?keep_disk=true", vm3["id"].as_str().unwrap()), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, d) = request(&app, "GET", &format!("/disks/{vm3_root}"), None).await;
    assert!(d["attached_to"].is_null(), "{d}");

    // Deleting the image-built VM deletes its owned root disk.
    let root_path = PathBuf::from(root["path"].as_str().unwrap());
    let (status, _) = request(&app, "DELETE", &format!("/vms/{vm_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = request(&app, "GET", &format!("/disks/{root_id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(!root_path.exists());
}

/// Deleting an image is a request the image controller finishes
/// (spec/reconciliation.md §6.3): a download in flight is cancelled and
/// its files removed; the name is free again.
#[tokio::test]
async fn deleting_an_image_cancels_its_download() {
    if !tools_present() {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let fx = fixtures(tmp.path());
    let (base, _) = start_server(&fx).await;
    let (app, _m) = create_app(tmp.path());

    let (status, img) = request(&app, "POST", "/images", Some(json!({"url": format!("{base}/flaky-good.qcow2"), "name": "doomed"}))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{img}");
    let id = img["id"].as_str().unwrap().to_string();
    let (status, body) = request(&app, "DELETE", "/images/doomed?wait=10", None).await;
    assert!(matches!(status, StatusCode::NO_CONTENT | StatusCode::OK), "{status} {body}");
    let (status, _) = request(&app, "GET", &format!("/images/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Give an aborted download a moment to have written, had it survived.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (status, _) = request(&app, "GET", &format!("/images/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "not brought back by the download task");
    let left: Vec<String> = std::fs::read_dir(tmp.path().join("images"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(&id))
        .collect();
    assert!(left.is_empty(), "files left: {left:?}");
    let (status, events) = request(&app, "GET", "/images", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(events.as_array().unwrap().iter().all(|i| i["name"] != "doomed"));

    let img = pull_ready(&app, &base, "good.qcow2", "doomed", None).await;
    assert_eq!(img["name"], "doomed", "the name is free again");
}
