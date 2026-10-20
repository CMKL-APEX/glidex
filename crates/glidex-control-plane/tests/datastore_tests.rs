//! Datastores (spec/storage.md S2): the built-in `local`, registered `dir`
//! datastores under the datastore root, what each node finds at them, and
//! disks put on them.

mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use glidex_control_plane::api::{self, App, AppState};
use glidex_control_plane::auth::{AuthService, store::TokenKind};
use glidex_control_plane::authz::Ent;
use glidex_control_plane::config::Config;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

/// The datastore root of this test process: set once, before anything reads
/// it, and shared by the tests here (each uses its own directory in it).
fn root() -> &'static Path {
    static ROOT: OnceLock<TempDir> = OnceLock::new();
    ROOT.get_or_init(|| {
        let d = TempDir::new().unwrap();
        // SAFETY: test-only; every test sets it the same way before reading it.
        unsafe { std::env::set_var("GLIDEX_DATASTORE_ROOT", d.path()) };
        d
    })
    .path()
}

struct H {
    router: Router,
    app: AppState,
    _dir: TempDir,
}

fn harness() -> H {
    root();
    let dir = TempDir::new().unwrap();
    // SAFETY: test-only; every test sets it the same way before reading it.
    unsafe { std::env::set_var("GLIDEX_RUN_DIR", dir.path().join("run")) };
    let manager = common::with_db_path(dir.path().join("t.db")).unwrap();
    manager.start_controllers();
    let mut cfg = Config::default();
    cfg.authz.policy_files_dir = dir.path().join("policies");
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    let app = Arc::new(App { manager, auth });
    H {
        router: api::router(app.clone()),
        app,
        _dir: dir,
    }
}

impl H {
    fn user(&self, name: &str) -> String {
        self.app
            .auth
            .store
            .user_for_identity("pam", name, name, None, true)
            .unwrap()
            .unwrap()
            .id
    }

    fn token(&self, user: &str) -> String {
        self.app
            .auth
            .create_token(
                "t",
                TokenKind::Personal { owner: user.into() },
                user,
                None,
                None,
                None,
            )
            .unwrap()
            .0
    }

    /// The project disks go to here.
    fn project(&self) -> String {
        self.app.manager.default_project_id()
    }

    fn admin(&self, name: &str) -> String {
        let id = self.user(name);
        self.app
            .auth
            .add_link(
                "role.system-admin",
                Ent::User(id.clone()),
                Ent::Cluster,
                "test",
            )
            .unwrap();
        self.app
            .auth
            .make_administrator(&Ent::User(id.clone()), "test")
            .unwrap();
        self.token(&id)
    }

    async fn rest(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: &str,
    ) -> (StatusCode, Value) {
        let b = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn datastore(&self, key: &str, t: &str) -> Value {
        let (s, v) = self
            .rest("GET", &format!("/datastores/{key}"), None, t)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["datastore"].clone()
    }

    /// `key` once this node has looked at it.
    async fn probed(&self, key: &str, t: &str) -> Value {
        for _ in 0..200 {
            let d = self.datastore(key, t).await;
            if d["node_status"].as_array().is_some_and(|a| !a.is_empty()) {
                return d;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("datastore {key} was never probed");
    }

    /// The disk `name` once its controller has settled it.
    async fn ready_disk(&self, name: &str, t: &str) -> Value {
        for _ in 0..400 {
            let (s, v) = self.rest("GET", &format!("/disks/{name}"), None, t).await;
            assert_eq!(s, StatusCode::OK, "{v}");
            match v["disk"]["status"].as_str() {
                Some("ready") => return v["disk"].clone(),
                Some("failed") => panic!("disk {name} failed: {v}"),
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        panic!("disk {name} never got ready");
    }
}

/// A fresh directory under the datastore root.
fn place(name: &str) -> PathBuf {
    let p = root().join(format!("{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&p).unwrap();
    p
}

/// A proto3 JSON bool: left out when false.
fn flag(v: &Value) -> bool {
    v.as_bool().unwrap_or(false)
}

fn num(v: &Value) -> u64 {
    match v {
        Value::Null => 0,
        Value::String(s) => s.parse().unwrap(),
        _ => v.as_u64().unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn local_is_built_in_and_disks_go_to_the_default() {
    let h = harness();
    let t = h.admin("root");

    // `local` is listed, is the default, and is checked like any other.
    let (s, v) = h.rest("GET", "/datastores", None, &t).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let all = v["datastores"].as_array().unwrap();
    assert_eq!(all.len(), 1, "{v}");
    assert_eq!(all[0]["id"], "local");
    assert!(flag(&all[0]["builtin"]), "{v}");
    assert!(flag(&all[0]["default_for_disks"]), "{v}");
    let local = h.probed("local", &t).await;
    let row = &local["node_status"][0];
    assert_eq!(row["reachable"], true, "{local}");
    assert!(num(&row["total_bytes"]) > 0, "{local}");
    for (m, body) in [
        ("PATCH", Some(json!({"overcommit": false}))),
        ("DELETE", None),
    ] {
        let (s, v) = h.rest(m, "/datastores/local", body, &t).await;
        assert_eq!(s, StatusCode::CONFLICT, "{m}: {v}");
    }

    // A registered one: outside the root is refused, inside it is checked.
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(json!({"name": "fast", "path": "/var/tmp/fast"})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let dir = place("fast");
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(json!({"name": "fast", "path": dir})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = v["datastore"]["id"].as_str().unwrap().to_string();
    assert!(!flag(&v["datastore"]["default_for_disks"]), "{v}");
    assert!(flag(&v["datastore"]["overcommit"]), "{v}");
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(json!({"name": "fast", "path": place("again")})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "a second fast: {v}");
    let fast = h.probed("fast", &t).await;
    assert_eq!(fast["node_status"][0]["reachable"], true, "{fast}");
    assert!(dir.join("disks").is_dir());

    // A disk goes to `local` unless it says otherwise.
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "here", "size_gib": 1})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(h.ready_disk("here", &t).await["datastore"], "local");
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "there", "size_gib": 1, "datastore": "fast"})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let there = h.ready_disk("there", &t).await;
    assert_eq!(there["datastore"], id.as_str());
    let file = PathBuf::from(there["path"].as_str().unwrap());
    assert_eq!(file.parent().unwrap(), dir.join("disks"), "{there}");
    assert!(file.exists());

    // Made the default, new disks go there; `local` is no longer it.
    let (s, v) = h
        .rest(
            "PATCH",
            "/datastores/fast",
            Some(json!({"default_for_disks": true})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(!flag(&h.datastore("local", &t).await["default_for_disks"]));
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "dflt", "size_gib": 1})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(h.ready_disk("dflt", &t).await["datastore"], id.as_str());
    assert_eq!(num(&h.datastore("fast", &t).await["disks"]), 2);

    // Not deleted while disks are on it.
    let (s, v) = h.rest("DELETE", "/datastores/fast", None, &t).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["message"].as_str().unwrap().contains("dflt"), "{v}");
    for d in ["there", "dflt"] {
        let (s, v) = h.rest("DELETE", &format!("/disks/{d}"), None, &t).await;
        assert!(s.is_success(), "{d}: {v}");
    }
    for _ in 0..200 {
        if !file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!file.exists(), "the disk's file goes with it");
    let (s, v) = h.rest("DELETE", "/datastores/fast", None, &t).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    assert!(flag(&h.datastore("local", &t).await["default_for_disks"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_datastore_that_doesnt_overcommit_refuses_what_doesnt_fit() {
    let h = harness();
    let t = h.admin("root");
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(json!({"name": "tight", "path": place("tight"), "overcommit": false})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let free = num(&h.probed("tight", &t).await["node_status"][0]["free_bytes"]);
    assert!(free > 0);
    let too_big = free / (1 << 30) + 1024;
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "huge", "size_gib": too_big, "datastore": "tight"})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::INSUFFICIENT_STORAGE, "{v}");
    assert_eq!(v["error"], "insufficient_storage", "{v}");
    // The same on `local`, which overcommits: accepted (thin files).
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "huge", "size_gib": too_big})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_administrators_manage_datastores() {
    let h = harness();
    let admin = h.admin("root");
    let user = h.user("alice");
    let t = h.token(&user);
    let (s, v) = h.rest("GET", "/datastores", None, &t).await;
    assert_eq!(s, StatusCode::OK, "anyone sees them: {v}");
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(json!({"name": "mine", "path": place("mine")})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(json!({"name": "ops", "path": place("ops")})),
            &admin,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = h
        .rest(
            "PATCH",
            "/datastores/ops",
            Some(json!({"overcommit": false})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
}

/// A shared datastore (spec/storage.md S3): claimed with its marker, disks
/// on it bound to no node and locked while they are worked on.
#[tokio::test(flavor = "multi_thread")]
async fn a_shared_datastore_is_claimed_and_its_disks_are_bound_to_no_node() {
    let h = harness();
    let t = h.admin("root");
    let dir = place("nfs");
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(json!({"name": "nfs", "path": dir, "driver": "DATASTORE_DRIVER_SHARED_DIR"})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(
        v["datastore"]["topology"], "DATASTORE_TOPOLOGY_SHARED",
        "{v}"
    );
    let id = v["datastore"]["id"].as_str().unwrap().to_string();
    let nfs = h.probed("nfs", &t).await;
    assert!(flag(&nfs["node_status"][0]["reachable"]), "{nfs}");
    // This host claimed it.
    let marker: Value =
        serde_json::from_slice(&std::fs::read(dir.join(".glidex-datastore")).unwrap()).unwrap();
    assert_eq!(marker["datastore"], id.as_str());

    // Bound to no node, so a node can't be named.
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "pinned", "size_gib": 1, "datastore": "nfs", "node": "local"})),
            &t,
        )
        .await;
    assert!(s.is_client_error(), "{s} {v}");
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "sd", "size_gib": 1, "datastore": "nfs"})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let sd = h.ready_disk("sd", &t).await;
    assert!(sd["node"].is_null(), "{sd}");
    let file = PathBuf::from(sd["path"].as_str().unwrap());
    assert_eq!(file.parent().unwrap(), dir.join("disks"), "{sd}");
    // Its lock file was made (and held) while the disk was made (S16).
    let lock = dir
        .join(".locks")
        .join(format!("{}.lock", sd["id"].as_str().unwrap()));
    assert!(lock.exists(), "{}", lock.display());

    // A directory with files and no marker is never claimed.
    let busy = place("busy");
    std::fs::write(busy.join("someone-elses"), b"x").unwrap();
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(json!({"name": "busy", "path": busy, "driver": "DATASTORE_DRIVER_SHARED_DIR"})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let b = h.probed("busy", &t).await;
    assert_eq!(b["node_status"][0]["reason"], "NotEmpty", "{b}");
    assert!(!busy.join(".glidex-datastore").exists());
}

/// A linked disk on a shared datastore overlays the datastore's own copy of
/// its image, made when the disk needs it and gone with the image
/// (spec/storage.md S8, S9).
#[tokio::test(flavor = "multi_thread")]
async fn a_linked_disk_on_a_shared_datastore_overlays_the_datastores_copy() {
    use glidex_control_plane::store::{Origin, TableId};
    let h = harness();
    let t = h.admin("root");
    let dir = place("nfs-img");
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(json!({"name": "nfs-img", "path": dir, "driver": "DATASTORE_DRIVER_SHARED_DIR"})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    h.probed("nfs-img", &t).await;

    // A ready qcow2 image on this host.
    let id = "img-shared";
    let images = h.app.manager.images();
    let base = images.image_path(id);
    std::fs::create_dir_all(base.parent().unwrap()).unwrap();
    let out = std::process::Command::new("qemu-img")
        .args(["create", "-q", "-f", "qcow2"])
        .arg(&base)
        .arg("64M")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let sha = glidex_control_plane::images::download::sha256_path(&base).unwrap();
    let size = std::fs::metadata(&base).unwrap().len();
    let img = json!({
        "id": id, "name": "base-shared", "kind": "disk", "source": { "kind": "url", "url": "http://example.invalid/x", "expected_sha256": sha },
        "status": { "state": "ready" }, "format": "qcow2", "virtual_size_bytes": 64 << 20, "file_size_bytes": size, "sha256": sha,
        "arch": "x86_64", "created_at": 1,
    });
    h.app
        .manager
        .database()
        .write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Images.definition())?
                    .insert(id, img.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();
    images.sync_keys(TableId::Images, &[id.to_string()]);

    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(
                json!({"project": h.project(), "name": "lnk", "image": id, "datastore": "nfs-img"}),
            ),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let d = h.ready_disk("lnk", &t).await;
    assert_eq!(d["origin"]["image"]["mode"], "CLONE_MODE_LINKED", "{d}");
    // Its overlay names the datastore's copy, relative to itself.
    let copy = dir.join("images").join(format!("{id}.qcow2"));
    assert!(copy.exists());
    let (s, detail) = h.rest("GET", "/disks/lnk", None, &t).await;
    assert_eq!(s, StatusCode::OK, "{detail}");
    assert_eq!(
        detail["disk"]["info"]["backing_filename"],
        format!("../images/{id}.qcow2"),
        "{detail}"
    );
    let (s, i) = h.rest("GET", &format!("/images/{id}"), None, &t).await;
    assert_eq!(s, StatusCode::OK, "{i}");
    let copies = i["image"]["copies"].as_array().cloned().unwrap_or_default();
    assert!(
        copies
            .iter()
            .any(|c| c["datastore_name"] == "nfs-img" && c["state"] == "ready"),
        "{i}"
    );

    // The image can't go while the disk overlays it; then its copy goes too.
    let (s, v) = h.rest("DELETE", &format!("/images/{id}"), None, &t).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, v) = h.rest("DELETE", "/disks/lnk", None, &t).await;
    assert!(s.is_success(), "{v}");
    for _ in 0..200 {
        let (s, _) = h.rest("DELETE", &format!("/images/{id}"), None, &t).await;
        if s.is_success() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for _ in 0..200 {
        if !copy.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!copy.exists(), "the datastore's copy goes with the image");
}

/// Moving disks between datastores (spec/storage.md §8.3): a blank disk to
/// a shared datastore and back; a linked one as an overlay on the shared
/// datastore's copy of its image, then flattened.
#[tokio::test(flavor = "multi_thread")]
async fn disks_move_between_datastores() {
    use glidex_control_plane::store::{Origin, TableId};
    let h = harness();
    let t = h.admin("root");
    let nfs = place("nfs-mv");
    let ssd = place("ssd-mv");
    for (name, dir, driver) in [
        ("nfs-mv", &nfs, "DATASTORE_DRIVER_SHARED_DIR"),
        ("ssd-mv", &ssd, "DATASTORE_DRIVER_DIR"),
    ] {
        let (s, v) = h
            .rest(
                "POST",
                "/datastores",
                Some(json!({"name": name, "path": dir, "driver": driver})),
                &t,
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        h.probed(name, &t).await;
    }
    let do_move = |name: String, to: &'static str, flatten: bool| {
        let h = &h;
        let t = t.clone();
        async move {
            h.rest(
                "POST",
                &format!("/disks/{name}/move"),
                Some(json!({ "datastore": to, "flatten": flatten, "wait_secs": 60 })),
                &t,
            )
            .await
        }
    };

    // A blank disk: local → nfs-mv → local.
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "mb", "size_gib": 1})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let before = h.ready_disk("mb", &t).await;
    let old = PathBuf::from(before["path"].as_str().unwrap());
    let (s, v) = do_move("mb".into(), "nfs-mv", false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let d = &v["disk"];
    assert!(d["moving_to"].is_null(), "settled: {v}");
    let now = PathBuf::from(d["path"].as_str().unwrap());
    assert_eq!(now.parent().unwrap(), nfs.join("disks"), "{v}");
    assert!(
        now.exists() && !old.exists(),
        "copied, then the old file removed"
    );
    let (s, v) = do_move("mb".into(), "local", false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["disk"]["datastore"], "local", "{v}");
    assert!(!now.exists());
    // Refused: where it is already, a datastore that doesn't exist.
    let (s, v) = do_move("mb".into(), "local", false).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = do_move("mb".into(), "nowhere", false).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");

    // A linked disk: an overlay on nfs-mv's copy of its image, by a relative path.
    let id = "img-mv";
    let images = h.app.manager.images();
    let base = images.image_path(id);
    std::fs::create_dir_all(base.parent().unwrap()).unwrap();
    assert!(
        std::process::Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2"])
            .arg(&base)
            .arg("64M")
            .status()
            .unwrap()
            .success()
    );
    let sha = glidex_control_plane::images::download::sha256_path(&base).unwrap();
    let size = std::fs::metadata(&base).unwrap().len();
    let img = json!({
        "id": id, "name": "base-mv", "kind": "disk", "source": { "kind": "url", "url": "http://example.invalid/x", "expected_sha256": sha },
        "status": { "state": "ready" }, "format": "qcow2", "virtual_size_bytes": 64 << 20, "file_size_bytes": size, "sha256": sha,
        "arch": "x86_64", "created_at": 1,
    });
    h.app
        .manager
        .database()
        .write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Images.definition())?
                    .insert(id, img.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();
    images.sync_keys(TableId::Images, &[id.to_string()]);
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "ml", "image": id})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    h.ready_disk("ml", &t).await;
    let (s, v) = do_move("ml".into(), "nfs-mv", false).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, detail) = h.rest("GET", "/disks/ml", None, &t).await;
    assert_eq!(detail["disk"]["datastore"], v["disk"]["datastore"]);
    assert_eq!(
        detail["disk"]["info"]["backing_filename"],
        format!("../images/{id}.qcow2"),
        "{detail}"
    );
    assert!(nfs.join("images").join(format!("{id}.qcow2")).exists());
    // Flattened onto ssd-mv: standalone, full.
    let (s, v) = do_move("ml".into(), "ssd-mv", true).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["disk"]["origin"]["image"]["mode"], "CLONE_MODE_FULL",
        "{v}"
    );
    let (_, detail) = h.rest("GET", "/disks/ml", None, &t).await;
    assert!(
        detail["disk"]["info"]["backing_filename"].is_null(),
        "{detail}"
    );
    let path = PathBuf::from(detail["disk"]["path"].as_str().unwrap());
    assert_eq!(path.parent().unwrap(), ssd.join("disks"));
}

/// An `lvm` datastore (spec/storage.md §11.1): a volume group and thin
/// pool instead of a path, and its disks are raw. Without glidex-stord (or
/// with the group not allowed) no node reaches it.
#[tokio::test(flavor = "multi_thread")]
async fn lvm_datastores_name_a_volume_group_and_keep_raw_disks() {
    let h = harness();
    let t = h.admin("root");
    for bad in [
        json!({"name": "thin", "driver": "DATASTORE_DRIVER_LVM", "volume_group": "vg0"}),
        json!({"name": "thin", "driver": "DATASTORE_DRIVER_LVM", "volume_group": "vg0", "thin_pool": "pool", "path": "/srv/glidex/x"}),
        json!({"name": "thin", "driver": "DATASTORE_DRIVER_LVM", "volume_group": "vg/0", "thin_pool": "pool"}),
    ] {
        let (s, v) = h.rest("POST", "/datastores", Some(bad), &t).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    }
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(json!({"name": "thin", "driver": "DATASTORE_DRIVER_LVM",
                        "volume_group": "gx-no-such-vg", "thin_pool": "pool"})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let ds = &v["datastore"];
    assert_eq!(ds["volume_group"], "gx-no-such-vg");
    assert_eq!(ds["thin_pool"], "pool");
    assert_eq!(ds["topology"], "DATASTORE_TOPOLOGY_NODE_LOCAL");
    assert!(ds["path"].as_str().unwrap_or("").is_empty(), "{v}");
    let d = h.probed("thin", &t).await;
    let row = &d["node_status"][0];
    assert!(!flag(&row["reachable"]), "{d}");
    assert!(
        ["StordUnavailable", "NotAllowed", "NoVolumeGroup"]
            .contains(&row["reason"].as_str().unwrap_or("")),
        "{d}"
    );
    assert_eq!(row["path"], "gx-no-such-vg/pool");

    // Raw, whatever the request leaves out; qcow2 refused.
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(
                json!({"project": h.project(), "name": "lv1", "size_gib": 1, "datastore": "thin",
                        "format": "DISK_FORMAT_QCOW2"}),
            ),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(
                json!({"project": h.project(), "name": "lv1", "size_gib": 1, "datastore": "thin"}),
            ),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["disk"]["format"], "DISK_FORMAT_RAW", "{v}");
}

/// The `lvm` driver against a real glidex-stord and volume group (spec/
/// storage.md §11.1, §19 H4–H7). Ignored by default; run with a stord whose
/// stord.json allows the group and hands devices to this test's user:
///
/// ```text
/// GLIDEX_STORD_SOCKET=/run/gx-s6-test/stord/stord.sock GLIDEX_STORD_VG=gx-test \
///   GLIDEX_STORD_POOL=pool cargo test -p glidex-control-plane --test datastore_tests \
///   lvm_disks_on_a_real_pool -- --ignored
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lvm_disks_on_a_real_pool() {
    use glidex_control_plane::store::{Origin, TableId};
    let vg = std::env::var("GLIDEX_STORD_VG").expect("GLIDEX_STORD_VG");
    let pool = std::env::var("GLIDEX_STORD_POOL").expect("GLIDEX_STORD_POOL");
    let h = harness();
    let t = h.admin("root");
    let (s, v) = h
        .rest(
            "POST",
            "/datastores",
            Some(
                json!({"name": "real-thin", "driver": "DATASTORE_DRIVER_LVM",
                        "volume_group": vg, "thin_pool": pool}),
            ),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let d = h.probed("real-thin", &t).await;
    assert!(flag(&d["node_status"][0]["reachable"]), "{d}");

    // A blank disk: a volume, grown, never shrunk, removed.
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "lvb", "size_gib": 1, "datastore": "real-thin"})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let id = v["disk"]["id"].as_str().unwrap().to_string();
    let disk = h.ready_disk("lvb", &t).await;
    assert_eq!(disk["path"], format!("/dev/{vg}/gx-d-{id}"), "{disk}");
    assert_eq!(num(&disk["size_bytes"]), 1 << 30);
    let (s, v) = h
        .rest(
            "POST",
            "/disks/lvb/resize?wait=60",
            Some(json!({"size_gib": 2})),
            &t,
        )
        .await;
    assert!(s.is_success(), "{v}");
    let (_, v) = h.rest("GET", "/disks/lvb", None, &t).await;
    assert_eq!(num(&v["disk"]["size_bytes"]), 2 << 30, "{v}");
    let (s, v) = h
        .rest(
            "POST",
            "/disks/lvb/resize",
            Some(json!({"size_gib": 1})),
            &t,
        )
        .await;
    assert!(s.is_client_error(), "a volume can't shrink: {v}");

    // A linked disk from an image with a partition: a thin snapshot of the
    // node's image volume, its root partition extended into the new space.
    let img_id = format!("img-lvm-{}", uuid::Uuid::new_v4().simple());
    let images = h.app.manager.images();
    let base = images.image_path(&img_id);
    std::fs::create_dir_all(base.parent().unwrap()).unwrap();
    let raw = base.with_extension("raw");
    let sh = |cmd: &mut std::process::Command| assert!(cmd.status().unwrap().success(), "{cmd:?}");
    sh(std::process::Command::new("truncate")
        .args(["-s", "64M"])
        .arg(&raw));
    sh(std::process::Command::new("sgdisk")
        .args(["-o", "-n", "1:2048:+32M", "-t", "1:8300"])
        .stdout(std::process::Stdio::null())
        .arg(&raw));
    sh(std::process::Command::new("qemu-img")
        .args(["convert", "-q", "-f", "raw", "-O", "qcow2"])
        .arg(&raw)
        .arg(&base));
    let sha = glidex_control_plane::images::download::sha256_path(&base).unwrap();
    let img = json!({
        "id": img_id, "name": img_id, "kind": "disk",
        "source": { "kind": "url", "url": "http://example.invalid/x", "expected_sha256": sha },
        "status": { "state": "ready" }, "format": "qcow2", "virtual_size_bytes": 64 << 20,
        "file_size_bytes": std::fs::metadata(&base).unwrap().len(), "sha256": sha,
        "arch": "x86_64", "created_at": 1,
    });
    h.app
        .manager
        .database()
        .write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Images.definition())?
                    .insert(img_id.as_str(), img.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();
    images.sync_keys(TableId::Images, std::slice::from_ref(&img_id));
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(
                json!({"project": h.project(), "name": "lvl", "image": img_id,
                        "size_bytes": 128 << 20, "datastore": "real-thin"}),
            ),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    h.ready_disk("lvl", &t).await;
    let (_, v) = h.rest("GET", "/disks/lvl", None, &t).await;
    let root = &v["disk"]["partition_table"]["partitions"][0];
    // Grown from 32 MiB to the end, less the GPT backup.
    assert!(num(&root["size_bytes"]) > 120 << 20, "{v}");
    let mut c = glidex_stord::client::Client::connect(std::path::Path::new(
        &std::env::var("GLIDEX_STORD_SOCKET").unwrap(),
    ))
    .unwrap();
    let vols: Vec<glidex_stord::proto::Volume> = c
        .call(glidex_stord::proto::Op::ListVolumes { vg: vg.clone() })
        .unwrap();
    let image_vol = format!("gx-i-{img_id}");
    let lvl = vols
        .iter()
        .find(|v| v.origin.as_deref() == Some(image_vol.as_str()))
        .unwrap_or_else(|| panic!("a snapshot of {image_vol}: {vols:?}"));
    assert!(lvl.name.starts_with("gx-d-"));

    // Moves (§8.3): a qcow2 disk onto the volume group becomes a raw
    // volume, and back a raw file; a linked one is flattened.
    let mv = |name: &'static str, to: &'static str| {
        let h = &h;
        let t = t.clone();
        async move {
            let (s, v) = h
                .rest(
                    "POST",
                    &format!("/disks/{name}/move"),
                    Some(json!({"datastore": to, "wait_secs": 120})),
                    &t,
                )
                .await;
            assert_eq!(s, StatusCode::OK, "{v}");
            assert!(v["disk"]["moving_to"].is_null(), "settled: {v}");
            v["disk"].clone()
        }
    };
    let (s, v) = h
        .rest(
            "POST",
            "/disks",
            Some(json!({"project": h.project(), "name": "lvq", "image": img_id, "size_bytes": 96 << 20})),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let local = h.ready_disk("lvq", &t).await;
    assert_eq!(local["format"], "DISK_FORMAT_QCOW2", "{local}");
    let old = PathBuf::from(local["path"].as_str().unwrap());
    let on_lvm = mv("lvq", "real-thin").await;
    assert_eq!(on_lvm["format"], "DISK_FORMAT_RAW", "{on_lvm}");
    assert_eq!(
        on_lvm["origin"]["image"]["mode"], "CLONE_MODE_FULL",
        "{on_lvm}"
    );
    assert!(
        on_lvm["path"]
            .as_str()
            .unwrap()
            .starts_with(&format!("/dev/{vg}/")),
        "{on_lvm}"
    );
    assert!(!old.exists(), "the qcow2 file went");
    let (_, v) = h.rest("GET", "/disks/lvq", None, &t).await;
    assert!(
        num(&v["disk"]["partition_table"]["partitions"][0]["size_bytes"]) > 0,
        "the data came along: {v}"
    );
    let back = mv("lvq", "local").await;
    assert_eq!(back["format"], "DISK_FORMAT_RAW", "{back}");
    assert!(
        PathBuf::from(back["path"].as_str().unwrap()).exists(),
        "{back}"
    );
    let (_, v) = h.rest("GET", "/disks/lvq", None, &t).await;
    assert!(
        num(&v["disk"]["partition_table"]["partitions"][0]["size_bytes"]) > 0,
        "and back: {v}"
    );

    // Gone with the disks; the image volume with the image.
    for name in ["lvb", "lvl", "lvq"] {
        let (s, v) = h.rest("DELETE", &format!("/disks/{name}"), None, &t).await;
        assert!(s.is_success(), "{v}");
    }
    for _ in 0..200 {
        let (s, _) = h.rest("GET", "/disks/lvl", None, &t).await;
        if s == StatusCode::NOT_FOUND {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (s, v) = h
        .rest("DELETE", &format!("/images/{img_id}"), None, &t)
        .await;
    assert!(s.is_success(), "{v}");
    for _ in 0..200 {
        let vols: Vec<glidex_stord::proto::Volume> = c
            .call(glidex_stord::proto::Op::ListVolumes { vg: vg.clone() })
            .unwrap();
        if !vols
            .iter()
            .any(|v| v.name == image_vol || v.name == format!("gx-d-{id}"))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("volumes left in {vg}");
}
