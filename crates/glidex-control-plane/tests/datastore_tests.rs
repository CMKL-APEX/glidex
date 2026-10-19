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
