//! The storage family on its RPCs and REST twins (spec/grpc.md §9.1,
//! G2): proto-shaped answers, the old request shape still read, and disk
//! names unique per tenant (spec/tenancy.md D24).

mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use buffa::Message;
use glidex_api::proto::glidex::storage::v1 as pb;
use glidex_control_plane::api::{self, App, AppState};
use glidex_control_plane::auth::{AuthService, store::TokenKind};
use glidex_control_plane::authz::Ent;
use glidex_control_plane::config::Config;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

struct H {
    router: Router,
    app: AppState,
    _dir: TempDir,
}

fn harness() -> H {
    let dir = TempDir::new().unwrap();
    // SAFETY: test-only; every test sets it the same way before reading it.
    unsafe { std::env::set_var("GLIDEX_RUN_DIR", dir.path().join("run")) };
    let manager = common::with_db_path(dir.path().join("t.db")).unwrap();
    let mut cfg = Config::default();
    cfg.authz.policy_files_dir = dir.path().join("policies");
    cfg.auth.allowed_origins = vec![ORIGIN.into()];
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    let app = Arc::new(App { manager, auth });
    H {
        router: api::router(app.clone()),
        app,
        _dir: dir,
    }
}

const ORIGIN: &str = "https://localhost:5173";

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

    /// A fresh browser session (`cookie|csrf`): a recent login, for the
    /// step-up actions (deleting tenants, admissions).
    fn session(&self, user: &str) -> String {
        let u = self.app.auth.store.user(user).unwrap().unwrap();
        let (cookie, csrf) = self
            .app
            .auth
            .create_session(&u, glidex_control_plane::auth::Method::Pam)
            .unwrap();
        format!("session:{cookie}|{csrf}")
    }

    /// A system administrator as bootstrap or migration makes one.
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
        id
    }

    async fn rest(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: &str,
    ) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path);
        let b = match token
            .strip_prefix("session:")
            .and_then(|s| s.split_once('|'))
        {
            Some((cookie, csrf)) => b
                .header(
                    header::COOKIE,
                    format!("{}={}", glidex_control_plane::auth::SESSION_COOKIE, cookie),
                )
                .header(glidex_control_plane::auth::CSRF_HEADER, csrf)
                .header(header::ORIGIN, ORIGIN),
            None => b.header(header::AUTHORIZATION, format!("Bearer {token}")),
        };
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

    /// The Connect protocol with JSON: what curl or a script sends.
    async fn connect(&self, method: &str, body: Value, token: &str) -> (StatusCode, Value) {
        let req = Request::post(format!("/{method}"))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// gRPC with a binary message; returns grpc-status and the reply.
    async fn grpc<M: Message>(&self, path: &str, msg: &M, token: &str) -> (String, Vec<u8>) {
        let bytes = msg.encode_to_vec();
        let mut body = vec![0u8];
        body.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        body.extend_from_slice(&bytes);
        let req = Request::post(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/grpc")
            .body(Body::from(body))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let headers = resp.headers().clone();
        let collected = resp.into_body().collect().await.unwrap();
        let status = collected
            .trailers()
            .and_then(|t| t.get("grpc-status"))
            .or_else(|| headers.get("grpc-status"))
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let b = collected.to_bytes();
        let msg = if b.len() >= 5 {
            b[5..5 + u32::from_be_bytes([b[1], b[2], b[3], b[4]]) as usize].to_vec()
        } else {
            Vec::new()
        };
        (status, msg)
    }
}

/// A tenant `slug`, admitted here, with one project; returns the
/// project's id.
async fn tenant_with_project(h: &H, s: &str, slug: &str, project: &str) -> String {
    let (st, v) = h
        .rest("POST", "/tenants", Some(json!({"slug": slug})), s)
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let (st, v) = h
        .rest("PUT", &format!("/admissions/{slug}"), Some(json!({})), s)
        .await;
    assert!(st.is_success(), "{v}");
    let tenant = h.app.auth.tenants.resolve(slug).unwrap().id;
    h.app
        .manager
        .projects()
        .create_in(&tenant, project, String::new(), None)
        .unwrap()
        .id
}

fn blank(name: &str, project: &str) -> Value {
    json!({"name": name, "project": project, "size_bytes": "1048576"})
}

#[tokio::test(flavor = "multi_thread")]
async fn disk_names_are_unique_per_tenant() {
    let h = harness();
    let admin = h.admin("ada");
    let s = h.session(&admin);
    let pa = tenant_with_project(&h, &s, "acme", "pa").await;
    let pa2 = h
        .app
        .manager
        .projects()
        .create_in(
            &h.app.auth.tenants.resolve("acme").unwrap().id,
            "pa2",
            String::new(),
            None,
        )
        .unwrap()
        .id;
    let pd = h
        .app
        .manager
        .projects()
        .create("pd", String::new(), None)
        .unwrap()
        .id;

    // The same name in two tenants; not twice in one.
    let (st, v) = h.rest("POST", "/disks", Some(blank("data", &pd)), &s).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let in_default = v["disk"]["id"].as_str().unwrap().to_string();
    let (st, v) = h.rest("POST", "/disks", Some(blank("data", &pa)), &s).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let in_acme = v["disk"]["id"].as_str().unwrap().to_string();
    let (st, v) = h
        .rest("POST", "/disks", Some(blank("data", &pa2)), &s)
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("conflict")),
        "{v}"
    );

    // Someone who sees both tenants names the disk by id.
    let t = h.token(&admin);
    let (st, v) = h.rest("GET", "/disks/data", None, &t).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("ambiguous")),
        "{v}"
    );
    let (st, v) = h.rest("GET", &format!("/disks/{in_acme}"), None, &t).await;
    assert_eq!(
        (st, v["disk"]["project"].as_str()),
        (StatusCode::OK, Some(pa.as_str())),
        "{v}"
    );

    // A member of acme only gets acme's.
    let bob = h.user("bob");
    h.app
        .auth
        .add_link(
            "role.editor",
            Ent::User(bob.clone()),
            Ent::Project(pa.clone()),
            "test",
        )
        .unwrap();
    let (st, v) = h.rest("GET", "/disks/data", None, &h.token(&bob)).await;
    assert_eq!(
        (st, v["disk"]["id"].as_str()),
        (StatusCode::OK, Some(in_acme.as_str())),
        "{v}"
    );
    let (st, _) = h
        .rest("GET", &format!("/disks/{in_default}"), None, &h.token(&bob))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // And the manager resolves a VM's disk names in the VM's tenant.
    let d = h.app.manager.get_disk_for(&pa2, "data").await.unwrap();
    assert_eq!(d.id, in_acme);
    let d = h.app.manager.get_disk_for(&pd, "data").await.unwrap();
    assert_eq!(d.id, in_default);
}

#[tokio::test(flavor = "multi_thread")]
async fn rest_answers_in_the_proto_shape_and_reads_the_old_one() {
    let h = harness();
    let admin = h.admin("ada");
    let s = h.session(&admin);
    let p = h
        .app
        .manager
        .projects()
        .create("p", String::new(), None)
        .unwrap()
        .id;

    // The old enum values and a numeric size still work.
    let body = json!({"name": "old", "project": p, "size_bytes": 2097152, "format": "raw"});
    let req = Request::post("/disks")
        .header(header::AUTHORIZATION, format!("Bearer {}", h.token(&admin)))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = h.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(resp.headers().get("glidex-api-shape").unwrap(), "proto");
    let v: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let d = &v["disk"];
    assert_eq!(d["format"], "DISK_FORMAT_RAW", "{v}");
    let sized = pb::Disk {
        size_bytes: 2_097_152,
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_value(&sized).unwrap()["size_bytes"],
        "2097152",
        "64-bit numbers are strings"
    );
    assert_eq!(d["origin"], json!({"blank": {}}));
    assert_eq!(d["phase"], "DISK_PHASE_PENDING");
    assert!(d["created_at"].as_str().unwrap().ends_with('Z'), "RFC 3339");

    // Lists are wrapped.
    let (st, v) = h
        .rest("GET", &format!("/disks?project={p}"), None, &s)
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["disks"].as_array().unwrap().len(), 1, "{v}");

    // Credentials: the update changes what it names.
    let c = json!({"project": p, "username": "dev", "password": "pw-1-long-enough", "ssh_authorized_keys": ["ssh-ed25519 AAAA k"]});
    let (st, v) = h.rest("POST", "/credentials", Some(c), &s).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["credential"]["has_password"], true);
    assert!(!v.to_string().contains("pw-1-long-enough"));
    let (st, v) = h
        .rest(
            "PUT",
            &format!("/credentials/dev?project={p}"),
            Some(json!({"ssh_authorized_keys": []})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["credential"]["has_password"], true, "the password stays");
    assert!(
        v["credential"].get("ssh_authorized_keys").is_none(),
        "keys cleared: {v}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_services_over_connect_and_grpc() {
    let h = harness();
    let admin = h.admin("ada");
    let t = h.token(&admin);
    let p = h
        .app
        .manager
        .projects()
        .create("p", String::new(), None)
        .unwrap()
        .id;

    // gRPC: create a disk, then read it.
    let req = pb::CreateDiskRequest {
        name: "g".into(),
        project: Some(p.clone()),
        size_bytes: Some(1 << 20),
        ..Default::default()
    };
    let (st, bytes) = h
        .grpc("/glidex.storage.v1.DiskService/CreateDisk", &req, &t)
        .await;
    assert_eq!(st, "0");
    let made = pb::CreateDiskResponse::decode_from_slice(&bytes).unwrap();
    let id = made.disk.as_option().unwrap().id.clone();
    assert_eq!(made.settled, None, "not waited for");
    let (st, bytes) = h
        .grpc(
            "/glidex.storage.v1.DiskService/GetDisk",
            &pb::GetDiskRequest {
                id: "g".into(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(st, "0");
    assert_eq!(
        pb::GetDiskResponse::decode_from_slice(&bytes)
            .unwrap()
            .disk
            .as_option()
            .unwrap()
            .id,
        id
    );
    let (st, _) = h
        .grpc(
            "/glidex.storage.v1.DiskService/GetDisk",
            &pb::GetDiskRequest {
                id: "nope".into(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(st, "5", "not found");

    // Connect JSON: credentials and images.
    let (st, v) = h
        .connect(
            "glidex.storage.v1.CredentialService/CreateCredential",
            json!({"project": p, "username": "ops", "password": "pw-2-long-enough"}),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!v.to_string().contains("pw-2-long-enough"));
    let (st, v) = h
        .connect(
            "glidex.storage.v1.CredentialService/ListCredentials",
            json!({"project": p}),
            &t,
        )
        .await;
    assert_eq!(
        (st, v["credentials"][0]["username"].as_str()),
        (StatusCode::OK, Some("ops")),
        "{v}"
    );
    let (st, v) = h
        .connect("glidex.storage.v1.ImageService/ListImages", json!({}), &t)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = h
        .connect(
            "glidex.storage.v1.ImageService/GetImageCatalog",
            json!({"arch": "ARCH_X86_64"}),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["items"].as_array().is_some_and(|i| !i.is_empty()), "{v}");

    // Deleting answers without the disk once it is gone.
    let (st, bytes) = h
        .grpc(
            "/glidex.storage.v1.DiskService/DeleteDisk",
            &pb::DeleteDiskRequest {
                id: id.clone(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(st, "0");
    assert!(
        pb::DeleteDiskResponse::decode_from_slice(&bytes)
            .unwrap()
            .disk
            .as_option()
            .is_none()
    );
}
