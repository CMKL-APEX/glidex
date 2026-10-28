//! Tenants, operators and admission on one host (spec/tenancy.md T0,
//! §17): the tenant API on REST, the Connect protocol and gRPC; the
//! tenant, operator and directory boundaries; admission and caps; the
//! host-link boundary; and the migration of a pre-tenancy database.

mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use buffa::Message;
use glidex_api::proto::glidex::tenancy::v1 as pb;
use glidex_control_plane::api::{self, App, AppState};
use glidex_control_plane::auth::{AuthService, store::TokenKind};
use glidex_control_plane::authz::Ent;
use glidex_control_plane::config::Config;
use glidex_control_plane::tenants::{ADMIN_TENANT, DEFAULT_TENANT};
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
    harness_with(|_| {})
}

fn harness_with(f: impl FnOnce(&mut Config)) -> H {
    let dir = TempDir::new().unwrap();
    // SAFETY: test-only; every test sets it the same way before reading it.
    unsafe { std::env::set_var("GLIDEX_RUN_DIR", dir.path().join("run")) };
    let manager = common::with_db_path(dir.path().join("t.db")).unwrap();
    let mut cfg = Config::default();
    cfg.authz.policy_files_dir = dir.path().join("policies");
    cfg.auth.allowed_origins = vec![ORIGIN.into()];
    f(&mut cfg);
    // The manager has the pools from the same file (main.rs configures it).
    manager.configure(&cfg);
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
        let req = Request::post(format!("/glidex.tenancy.v1.TenantService/{method}"))
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

/// The VM in a `{vm}` answer (`POST /vms`), in the proto3 JSON shape
/// (spec/grpc.md §9.1).
fn vm_of(v: &Value) -> &Value {
    assert!(v["vm"].is_object(), "not a VM answer: {v}");
    &v["vm"]
}

fn vm_body(name: &str, project: &str) -> Value {
    json!({
        "name": name, "project": project, "vcpu_count": 1, "mem_size_mib": 128,
        "kernel_image_path": "/tmp/vmlinux", "rootfs_path": "/tmp/rootfs.ext4",
    })
}

#[tokio::test]
async fn upgraded_host_has_its_directory_two_tenants_and_admissions() {
    let h = harness();
    let admin = h.admin("ada");
    let t = h.token(&admin);
    let (s, v) = h.rest("GET", "/tenants", None, &t).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let slugs: Vec<&str> = v["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, ["admin", "default"]);
    let admin_t = &v["tenants"][0];
    assert_eq!(admin_t["owns_directory"], true);
    assert_eq!(admin_t["operator"], true);
    assert_eq!(admin_t["admission"]["status"], "ADMISSION_STATUS_ADMITTED");
    // The default project is in the default tenant.
    let p = h.app.manager.projects().resolve("default").unwrap();
    assert_eq!(p.tenant_id, DEFAULT_TENANT);
}

#[tokio::test]
async fn the_tenant_api_is_the_same_on_rest_connect_and_grpc() {
    let h = harness();
    let t = h.token(&h.admin("ada"));

    let (s, v) = h
        .rest(
            "POST",
            "/tenants",
            Some(json!({ "slug": "acme", "display_name": "Acme" })),
            &t,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let acme = v["tenant"]["id"].as_str().unwrap().to_string();

    let (s, v) = h
        .connect("CreateTenant", json!({ "slug": "lab" }), &t)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["tenant"]["slug"], "lab");

    let req = pb::CreateTenantRequest {
        slug: "uni".into(),
        operator: Some(true),
        ..Default::default()
    };
    let (status, msg) = h
        .grpc("/glidex.tenancy.v1.TenantService/CreateTenant", &req, &t)
        .await;
    assert_eq!(status, "0");
    let created = pb::CreateTenantResponse::decode_from_slice(&msg).unwrap();
    assert_eq!(created.tenant.as_option().unwrap().operator, Some(true));

    // One answer whichever way it is asked.
    let (_, rest) = h.rest("GET", &format!("/tenants/{acme}"), None, &t).await;
    let (_, conn) = h.connect("GetTenant", json!({ "id": "acme" }), &t).await;
    assert_eq!(rest, conn);

    // The same error code on each: a duplicate slug is a conflict.
    let (s, v) = h
        .rest("POST", "/tenants", Some(json!({ "slug": "acme" })), &t)
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("conflict"))
    );
    let (s, v) = h
        .connect("CreateTenant", json!({ "slug": "acme" }), &t)
        .await;
    assert_eq!(
        (s, v["code"].as_str()),
        (StatusCode::CONFLICT, Some("aborted"))
    );
    let (status, _) = h
        .grpc(
            "/glidex.tenancy.v1.TenantService/CreateTenant",
            &pb::CreateTenantRequest {
                slug: "acme".into(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(status, "10", "ABORTED");
}

#[tokio::test]
async fn tenants_are_closed_to_each_other_and_to_non_owners() {
    let h = harness();
    let at = h.session(&h.admin("ada"));
    let (_, v) = h
        .rest("POST", "/tenants", Some(json!({ "slug": "acme" })), &at)
        .await;
    let acme = v["tenant"]["id"].as_str().unwrap().to_string();
    let (_, v) = h
        .rest("POST", "/tenants", Some(json!({ "slug": "lab" })), &at)
        .await;
    let lab = v["tenant"]["id"].as_str().unwrap().to_string();

    // acme's admin: projects in acme, nothing of lab, no tenants of its own making.
    let alice = h.user("alice");
    let (s, _) = h.rest("POST", &format!("/tenants/{acme}/bindings"), Some(json!({ "role": "role.tenant-admin", "principal": { "type": "User", "id": alice } })), &at).await;
    assert_eq!(s, StatusCode::CREATED);
    let a = h.token(&alice);
    let (s, v) = h
        .rest(
            "POST",
            "/projects",
            Some(json!({ "name": "web", "tenant": "acme" })),
            &a,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["project"]["tenant_id"], acme.as_str());
    let (s, _) = h
        .rest(
            "POST",
            "/projects",
            Some(json!({ "name": "web", "tenant": "lab" })),
            &a,
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "base.tenant-boundary");
    let (s, _) = h.rest("GET", &format!("/tenants/{lab}"), None, &a).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "another tenant is invisible");
    let (s, _) = h
        .rest("POST", "/tenants", Some(json!({ "slug": "mine" })), &a)
        .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "only the directory's owner creates tenants"
    );
    // Her list shows acme only.
    let (_, v) = h.rest("GET", "/tenants", None, &a).await;
    assert_eq!(v["tenants"].as_array().unwrap().len(), 1, "{v}");
    // Project names are per tenant (D24). The directory's admin works in a
    // tenant through a delegated binding (D15), visible to that tenant.
    let ada = h
        .app
        .auth
        .store
        .user_for_identity("pam", "ada", "ada", None, false)
        .unwrap()
        .unwrap()
        .id;
    let (s, _) = h
        .rest(
            "POST",
            &format!("/tenants/{lab}/bindings"),
            Some(
                json!({ "role": "role.tenant-admin", "principal": { "type": "User", "id": ada } }),
            ),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = h
        .rest(
            "POST",
            "/projects",
            Some(json!({ "name": "web", "tenant": "lab" })),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    // Deleting is a step-up action: an old login (a personal token) can't.
    let tok = h.token(&ada);
    let (s, v) = h
        .rest("DELETE", &format!("/tenants/{acme}"), None, &tok)
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("reauth_required"))
    );
    // A tenant with projects can't be deleted.
    let (s, v) = h
        .rest("DELETE", &format!("/tenants/{acme}"), None, &at)
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("tenant_in_use"))
    );
}

#[tokio::test]
async fn host_roles_need_the_operator_and_reach_only_its_cluster() {
    let h = harness();
    let at = h.session(&h.admin("ada"));
    // A host role written straight into the store, for someone who isn't
    // a member of the operator tenant: it hands out nothing.
    let mallory = h.user("mallory");
    h.app
        .auth
        .add_link(
            "role.auditor",
            Ent::User(mallory.clone()),
            Ent::Cluster,
            "test",
        )
        .unwrap();
    let m = h.token(&mallory);
    let (s, _) = h.rest("GET", "/admissions", None, &m).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "base.host-operator");
    // The API refuses to give a host role to a non-member.
    let bob = h.user("bob");
    let (s, v) = h
        .rest(
            "POST",
            "/system/bindings",
            Some(json!({ "role": "role.auditor", "principal": { "type": "User", "id": bob } })),
            &at,
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("not_operator_member")
        ),
        "{v}"
    );
    let (s, _) = h
        .rest(
            "POST",
            &format!("/tenants/{ADMIN_TENANT}/members"),
            Some(json!({ "principal": { "type": "User", "id": bob } })),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = h
        .rest(
            "POST",
            "/system/bindings",
            Some(json!({ "role": "role.auditor", "principal": { "type": "User", "id": bob } })),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = h.rest("GET", "/admissions", None, &h.token(&bob)).await;
    assert_eq!(s, StatusCode::OK);
}

/// spec/clustering.md §12.2: a host role can be given on one node's host.
#[tokio::test]
async fn a_host_role_on_one_node_covers_that_node_only() {
    use glidex_control_plane::node::{Node, NodeStore};
    let h = harness();
    let at = h.session(&h.admin("ada"));
    let nodes = NodeStore::new(h.app.manager.database());
    let spec =
        |name: &str| serde_json::from_value(json!({ "name": name, "role": "agent" })).unwrap();
    nodes.put(&Node::new("n2-id", spec("n2"))).unwrap();
    let mut gone = Node::new("n3-id", spec("n3"));
    gone.status.phase = glidex_control_plane::node::NodePhase::Removed;
    nodes.put(&gone).unwrap();
    let bob = h.user("bob");
    let (s, _) = h
        .rest(
            "POST",
            &format!("/tenants/{ADMIN_TENANT}/members"),
            Some(json!({ "principal": { "type": "User", "id": bob } })),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let grant = |role: &str, node: &str| json!({ "role": role, "principal": { "type": "User", "id": bob }, "node": node });
    // Cluster-wide roles aren't given on a host.
    for role in ["role.system-admin", "role.image-admin"] {
        let (s, v) = h
            .rest("POST", "/system/bindings", Some(grant(role, "n2")), &at)
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{role}: {v}");
    }
    // A node is named by id or name; it has to be there.
    let (s, v) = h
        .rest(
            "POST",
            "/system/bindings",
            Some(grant("role.auditor", "nope")),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    let (s, v) = h
        .rest(
            "POST",
            "/system/bindings",
            Some(grant("role.auditor", "n3")),
            &at,
        )
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("node_departed")),
        "{v}"
    );
    let (s, v) = h
        .rest(
            "POST",
            "/system/bindings",
            Some(grant("role.auditor", "n2")),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(
        v["binding"]["resource"],
        json!({ "type": "Host", "id": "n2-id" })
    );
    let link = v["binding"]["id"].as_str().unwrap().to_string();

    // Bob reads n2, not the cluster's node list nor this host.
    let bt = h.token(&bob);
    let (s, v) = h.rest("GET", "/nodes/n2", None, &bt).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = h.rest("GET", "/nodes", None, &bt).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let local = glidex_control_plane::authz::node_id();
    let (s, _) = h.rest("GET", &format!("/nodes/{local}"), None, &bt).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Listed with every system binding, and by node.
    let (_, v) = h.rest("GET", "/system/bindings", None, &at).await;
    assert!(
        v["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["id"] == link),
        "{v}"
    );
    let (_, v) = h.rest("GET", "/system/bindings?node=n2", None, &at).await;
    let ids: Vec<&str> = v["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [link.as_str()]);
    let (_, v) = h
        .rest("GET", &format!("/system/bindings?node={local}"), None, &at)
        .await;
    assert!(v.get("bindings").is_none(), "{v}");

    let (s, _) = h
        .rest("DELETE", &format!("/system/bindings/{link}"), None, &at)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = h.rest("GET", "/nodes/n2", None, &bt).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn allocation_needs_an_admission_and_the_tenant_cap_binds() {
    let h = harness();
    let at = h.session(&h.admin("ada"));
    let (_, v) = h
        .rest("POST", "/tenants", Some(json!({ "slug": "acme" })), &at)
        .await;
    let acme = v["tenant"]["id"].as_str().unwrap().to_string();
    let owner = h.user("olivia");
    h.app
        .auth
        .add_link(
            "grant.host-paths",
            Ent::User(owner.clone()),
            Ent::Cluster,
            "test",
        )
        .unwrap();
    let o = h.token(&owner);
    let ada = h
        .app
        .auth
        .store
        .user_for_identity("pam", "ada", "ada", None, false)
        .unwrap()
        .unwrap()
        .id;
    let (s, _) = h
        .rest(
            "POST",
            &format!("/tenants/{acme}/bindings"),
            Some(
                json!({ "role": "role.tenant-admin", "principal": { "type": "User", "id": ada } }),
            ),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    for name in ["p1", "p2"] {
        let (s, v) = h
            .rest(
                "POST",
                "/projects",
                Some(json!({ "name": name, "tenant": acme, "owners": [owner] })),
                &at,
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
    let p1 = h
        .app
        .manager
        .projects()
        .list()
        .unwrap()
        .into_iter()
        .find(|p| p.name == "p1")
        .unwrap()
        .id;
    let p2 = h
        .app
        .manager
        .projects()
        .list()
        .unwrap()
        .into_iter()
        .find(|p| p.name == "p2")
        .unwrap()
        .id;

    // Not admitted yet.
    let (s, v) = h.rest("POST", "/vms", Some(vm_body("v1", &p1)), &o).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("not_admitted")),
        "{v}"
    );

    // Admitted with a cap of one VM for the whole tenant.
    let (s, v) = h
        .rest(
            "PUT",
            "/admissions/acme",
            Some(json!({ "cap": { "vms": 1 } })),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = h.rest("POST", "/vms", Some(vm_body("v1", &p1)), &o).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let vm = vm_of(&v)["id"].as_str().unwrap().to_string();
    let (s, v) = h.rest("POST", "/vms", Some(vm_body("v2", &p2)), &o).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("tenant_quota_exceeded")),
        "the cap counts p1 too: {v}"
    );

    // Suspended admission: no starts.
    let (s, _) = h
        .rest(
            "PUT",
            "/admissions/acme",
            Some(json!({ "suspended": true })),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = h.rest("POST", &format!("/vms/{vm}/start"), None, &o).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("not_admitted")),
        "{v}"
    );

    // A suspended tenant's members can't act at all.
    let (s, _) = h
        .rest(
            "PUT",
            "/admissions/acme",
            Some(json!({ "suspended": false })),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = h
        .rest("POST", &format!("/tenants/{acme}/suspend"), None, &at)
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = h.rest("GET", &format!("/vms/{vm}"), None, &o).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "base.tenant-suspended");
}

#[tokio::test]
async fn tenant_pools_split_a_granted_cluster_pool_among_projects() {
    use glidex_control_plane::config::{PciGrant, PciPool, PoolMatch};
    use glidex_control_plane::topology::{SummaryDevice, TopologySummary};
    let h = harness_with(|c| {
        c.pci.pools = vec![PciPool {
            name: "a100".into(),
            class: Some("gpu".into()),
            matches: PoolMatch {
                vendor_id: Some("0x10de".into()),
                ..Default::default()
            },
            nodes: vec![],
            prepare: false,
            per_device: None,
        }];
        c.pci.allow = vec![PciGrant {
            bdf: None,
            pool: Some("a100".into()),
            projects: vec![],
            tenants: vec!["acme".into()],
            node: None,
        }];
    });
    h.app.manager.report_self().unwrap();
    let nodes = h.app.manager.nodes();
    let mut n = nodes.get("local").unwrap().unwrap();
    let gpu = |a: &str| SummaryDevice {
        address: a.into(),
        vendor_id: "0x10de".into(),
        device_id: "0x20b0".into(),
        class_id: "0x030200".into(),
        ready: true,
        ..Default::default()
    };
    n.status.topology = Some(TopologySummary {
        devices: vec![gpu("0000:87:00.0"), gpu("0000:90:00.0")],
        ..n.status.topology.clone().unwrap_or_default()
    });
    nodes.put(&n).unwrap();

    let at = h.session(&h.admin("ada"));
    let (_, v) = h
        .rest("POST", "/tenants", Some(json!({ "slug": "acme" })), &at)
        .await;
    let acme = v["tenant"]["id"].as_str().unwrap().to_string();
    let (s, _) = h
        .rest("PUT", "/admissions/acme", Some(json!({})), &at)
        .await;
    assert_eq!(s, StatusCode::OK);
    let (owner, tom) = (h.user("olivia"), h.user("tom"));
    h.app
        .auth
        .add_link(
            "grant.host-paths",
            Ent::User(owner.clone()),
            Ent::Cluster,
            "test",
        )
        .unwrap();
    let (s, _) = h
        .rest(
            "POST",
            &format!("/tenants/{acme}/bindings"),
            Some(
                json!({ "role": "role.tenant-admin", "principal": { "type": "User", "id": tom } }),
            ),
            &at,
        )
        .await;
    assert_eq!(s, StatusCode::CREATED);
    let (o, t) = (h.token(&owner), h.token(&tom));
    let mut ids = std::collections::HashMap::new();
    for name in ["p1", "p2", "p3"] {
        let (s, v) = h
            .rest(
                "POST",
                "/projects",
                Some(json!({ "name": name, "tenant": acme, "owners": [owner] })),
                &t,
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        ids.insert(name, v["project"]["id"].as_str().unwrap().to_string());
    }
    let path = format!("/tenants/{acme}/device-pools");

    // The tenant admin splits its share; bad pools are refused.
    let pool = |name: &str, from: &str| json!({ "pools": [{ "name": name, "from": from, "max": 1, "projects": ["p1", "p2"] }] });
    for (body, why) in [
        (pool("a100", "a100"), "a cluster pool's name"),
        (pool("team-gpus", "nope"), "no such cluster pool"),
        (pool("Team", "a100"), "the name's form"),
    ] {
        let (s, v) = h.rest("PUT", &path, Some(body), &t).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{why}: {v}");
    }
    let (s, _) = h
        .rest("PUT", &path, Some(pool("team-gpus", "a100")), &o)
        .await;
    assert!(
        s == StatusCode::FORBIDDEN || s == StatusCode::NOT_FOUND,
        "{s}"
    );
    let (s, v) = h
        .rest("PUT", &path, Some(pool("team-gpus", "a100")), &t)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let tp = &v["pools"][0];
    assert_eq!(
        (tp["max"].as_u64(), tp["free"].as_u64()),
        (Some(1), Some(2)),
        "{v}"
    );
    assert_eq!(tp["projects"], json!([ids["p1"], ids["p2"]]), "{v}");

    // p1's members see it, with what they may still take.
    let (s, v) = h
        .rest(
            "GET",
            &format!("/pci-pools?project={}", ids["p1"]),
            None,
            &o,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mine = v["pools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "team-gpus")
        .unwrap_or_else(|| panic!("{v}"));
    assert_eq!(
        (mine["from"].as_str(), mine["free"].as_u64()),
        (Some("a100"), Some(1))
    );

    let with = |name: &str, project: &str| {
        let mut b = vm_body(name, project);
        b["devices"] = json!([{ "pool": "team-gpus" }]);
        b
    };
    let (s, v) = h
        .rest("POST", "/vms", Some(with("v1", &ids["p1"])), &o)
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let d = &vm_of(&v)["devices"][0];
    assert_eq!(
        (d["pool"].as_str(), d["tenant_pool"].as_str()),
        (Some("a100"), Some("team-gpus")),
        "{v}"
    );
    assert!(vm_of(&v)["pool_devices"][0]["address"].is_string(), "{v}");
    // The tenant pool's max is shared by its projects.
    let (s, v) = h
        .rest("POST", "/vms", Some(with("v2", &ids["p2"])), &o)
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "tenant_quota_exceeded", "{v}");
    assert_eq!(
        v["details"]["quota"][0]["resource"], "tenant_pool.team-gpus",
        "{v}"
    );
    // p3 isn't one of its projects.
    let (s, v) = h
        .rest("POST", "/vms", Some(with("v3", &ids["p3"])), &o)
        .await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("forbidden")),
        "{v}"
    );

    let (s, v) = h.rest("GET", &path, None, &t).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["pools"][0]["used"], 1, "{v}");
}

#[tokio::test]
async fn migration_keeps_an_existing_system_admin_whole() {
    let dir = TempDir::new().unwrap();
    // SAFETY: test-only; every test sets it the same way before reading it.
    unsafe { std::env::set_var("GLIDEX_RUN_DIR", dir.path().join("run")) };
    let manager = common::with_db_path(dir.path().join("t.db")).unwrap();
    // A host from before tenants: a user with system-admin on the cluster.
    let uid = {
        let store =
            glidex_control_plane::auth::store::IdentityStore::new(manager.database()).unwrap();
        let u = store
            .user_for_identity("pam", "old", "old", None, true)
            .unwrap()
            .unwrap();
        store
            .put_link(&glidex_control_plane::auth::store::LinkRecord {
                link: glidex_control_plane::authz::Link {
                    id: "link.old".into(),
                    template: "role.system-admin".into(),
                    principal: Ent::User(u.id.clone()),
                    resource: Ent::Cluster,
                },
                created_by: "before".into(),
                created_at: 0,
            })
            .unwrap();
        u.id
    };
    let mut cfg = Config::default();
    cfg.authz.policy_files_dir = dir.path().join("policies");
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    assert!(
        auth.tenants
            .members(ADMIN_TENANT)
            .unwrap()
            .iter()
            .any(|m| m.principal == Ent::User(uid.clone()))
    );
    let links = auth.store.links().unwrap();
    for (role, res) in [
        ("role.directory-admin", Ent::Directory("local".into())),
        ("role.tenant-admin", Ent::Tenant(ADMIN_TENANT.into())),
        ("role.tenant-admin", Ent::Tenant(DEFAULT_TENANT.into())),
    ] {
        assert!(
            links.iter().any(|l| l.link.template == role
                && l.link.resource == res
                && l.link.principal == Ent::User(uid.clone())),
            "{role}"
        );
    }
    // And what they did before still works: users, projects, host reads.
    let app = Arc::new(App { manager, auth });
    let h = H {
        router: api::router(app.clone()),
        app,
        _dir: dir,
    };
    let t = h.token(&uid);
    for (m, p, b) in [
        ("GET", "/users", None),
        ("POST", "/projects", Some(json!({ "name": "p9" }))),
        ("GET", "/nodes", None),
    ] {
        let (s, v) = h.rest(m, p, b, &t).await;
        assert!(s.is_success(), "{m} {p}: {s} {v}");
    }
}

/// A browser's cookies, sent as a browser would: `gx_accounts` only to
/// `/auth/accounts…` (its `Path`), the session's CSRF value on writes.
#[derive(Default)]
struct Browser {
    session: Option<String>,
    accounts: Option<String>,
    csrf: Option<String>,
}

impl Browser {
    async fn call(
        &mut self,
        h: &H,
        method: &str,
        path: &str,
        body: Option<Value>,
        origin: bool,
    ) -> (StatusCode, Value) {
        let mut cookies = Vec::new();
        if let Some(s) = &self.session {
            cookies.push(format!("gx_session={s}"));
        }
        if path.starts_with("/auth/accounts")
            && let Some(a) = &self.accounts
        {
            cookies.push(format!("gx_accounts={a}"));
        }
        let mut b = Request::builder().method(method).uri(path);
        if !cookies.is_empty() {
            b = b.header(header::COOKIE, cookies.join("; "));
        }
        if origin {
            b = b.header(header::ORIGIN, ORIGIN);
        }
        if let Some(c) = &self.csrf {
            b = b.header("x-glidex-csrf", c);
        }
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = h.router.clone().oneshot(req).await.unwrap();
        for c in resp.headers().get_all(header::SET_COOKIE) {
            let c = c.to_str().unwrap();
            let (kv, attrs) = c.split_once(';').unwrap_or((c, ""));
            let (k, v) = kv.split_once('=').unwrap();
            let v = (!attrs.contains("Max-Age=0") && !v.is_empty()).then(|| v.to_string());
            match k {
                "gx_session" => self.session = v,
                "gx_accounts" => {
                    assert!(attrs.contains("Path=/api/auth/accounts"), "{c}");
                    assert!(attrs.contains("HttpOnly"), "{c}");
                    self.accounts = v;
                }
                _ => {}
            }
        }
        let s = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if let Some(c) = v["csrf"].as_str() {
            self.csrf = Some(c.into());
        }
        (s, v)
    }

    /// A login at the user's directory, as any sign-in method ends.
    fn login(&mut self, h: &H, user: &str) {
        let u = h.app.auth.store.user(user).unwrap().unwrap();
        let (cookie, csrf) = h
            .app
            .auth
            .create_session(&u, glidex_control_plane::auth::Method::Pam)
            .unwrap();
        self.session = Some(cookie);
        self.csrf = Some(csrf);
    }

    async fn accounts(&mut self, h: &H) -> Vec<(String, bool, bool)> {
        let (s, v) = self.call(h, "GET", "/auth/accounts", None, false).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["directory"].as_str().unwrap().to_string(),
                    a["signed_in"].as_bool().unwrap(),
                    a["active"].as_bool().unwrap(),
                )
            })
            .collect()
    }
}

/// D71: one browser, accounts in two directories of one host: adding,
/// switching both ways, an ended session staying listed to sign in again,
/// removing, signing out of all; writes need the page's `Origin` and,
/// with an active session, its CSRF value.
#[tokio::test]
async fn a_browser_holds_accounts_in_several_directories() {
    let h = harness();
    h.app
        .auth
        .tenants
        .create_directory("beta", "Beta", "beta-admin")
        .unwrap();
    let alice = h.user("alice");
    let bob = h.user("bob");
    let mut u = h.app.auth.store.user(&bob).unwrap().unwrap();
    u.directory_id = "beta".into();
    h.app.auth.store.put_user(&u).unwrap();

    let mut br = Browser::default();
    br.login(&h, &alice);
    assert_eq!(br.accounts(&h).await, vec![("local".into(), true, true)]);

    // Refused: no Origin; a wrong CSRF value while signed in.
    let (s, _) = br.call(&h, "POST", "/auth/accounts/add", None, false).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let good = br.csrf.replace("nope".into());
    let (s, v) = br.call(&h, "POST", "/auth/accounts/add", None, true).await;
    assert_eq!(
        (s, v["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("csrf_failed"))
    );
    br.csrf = good;

    // Add: alice moves into the list; bob signs in at beta.
    let (s, v) = br.call(&h, "POST", "/auth/accounts/add", None, true).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(br.session.is_none(), "the active session is parked");
    br.login(&h, &bob);
    assert_eq!(
        br.accounts(&h).await,
        vec![("beta".into(), true, true), ("local".into(), true, false)]
    );

    // Switch to alice and back.
    let whoami = |br: &mut Browser| {
        let s = br.session.clone();
        let h = &h;
        async move {
            let mut b = Request::get("/auth/whoami");
            if let Some(s) = s {
                b = b.header(header::COOKIE, format!("gx_session={s}"));
            }
            let resp = h
                .router
                .clone()
                .oneshot(b.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let st = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            (st, v["user"]["id"].as_str().map(String::from))
        }
    };
    let (s, v) = br
        .call(
            &h,
            "POST",
            "/auth/accounts/switch",
            Some(json!({"directory": "local"})),
            true,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(whoami(&mut br).await, (StatusCode::OK, Some(alice.clone())));
    let (s, _) = br
        .call(
            &h,
            "POST",
            "/auth/accounts/switch",
            Some(json!({"directory": "beta"})),
            true,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(whoami(&mut br).await, (StatusCode::OK, Some(bob.clone())));
    let (s, _) = br
        .call(
            &h,
            "POST",
            "/auth/accounts/switch",
            Some(json!({"directory": "gamma"})),
            true,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "not held");

    // Alice's session ends (here, by an administrator ending it): her
    // directory stays listed, signed out, and can't be switched to.
    h.app.auth.store.remove_sessions_of(&alice).unwrap();
    assert_eq!(
        br.accounts(&h).await,
        vec![("beta".into(), true, true), ("local".into(), false, false)]
    );
    let (s, _) = br
        .call(
            &h,
            "POST",
            "/auth/accounts/switch",
            Some(json!({"directory": "local"})),
            true,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Removed: forgotten.
    let (s, _) = br
        .call(
            &h,
            "POST",
            "/auth/accounts/remove",
            Some(json!({"directory": "local"})),
            true,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(br.accounts(&h).await, vec![("beta".into(), true, true)]);

    // Signed out of all: bob's session ends too, and the list is gone.
    br.call(&h, "POST", "/auth/accounts/add", None, true).await;
    br.login(&h, &alice);
    let bob_session = br.accounts.clone().unwrap();
    assert!(
        bob_session.contains("s:"),
        "the accounts cookie names a session"
    );
    let (s, _) = br
        .call(&h, "POST", "/auth/accounts/logout-all", None, true)
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(br.session.is_none() && br.accounts.is_none());
    assert_eq!(
        h.app.auth.store.remove_sessions_of(&bob).unwrap(),
        0,
        "ended"
    );
}

/// D57: a standalone host's server-info names the `aud` its access tokens
/// carry (its own `standalone-…` id), which gxctl's directory login asks
/// tokens for; it reports no cluster id.
#[tokio::test]
async fn server_info_names_a_standalone_hosts_token_audience() {
    let h = harness();
    let resp = h
        .router
        .clone()
        .oneshot(
            Request::get("/auth/server-info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let aud = v["token_audience"].as_str().unwrap_or_default();
    assert!(aud.starts_with("standalone-"), "{v}");
    assert_eq!(aud, glidex_control_plane::api::op::cluster_id(&h.app));
    assert!(v["cluster_id"].is_null(), "{v}");
}

/// A tenant admin with no role on the tenant's projects themselves still
/// lists them (tenant-admin includes owner on its projects); another
/// tenant's project stays out of the list.
#[tokio::test]
async fn a_tenant_admin_lists_the_tenants_projects() {
    let h = harness();
    let at = h.session(&h.admin("root"));
    let (_, v) = h
        .rest("POST", "/tenants", Some(json!({ "slug": "acme" })), &at)
        .await;
    let acme = v["tenant"]["id"].as_str().unwrap().to_string();
    for (name, tenant) in [("in-acme", acme.as_str()), ("elsewhere", DEFAULT_TENANT)] {
        h.app
            .manager
            .projects()
            .create_in(tenant, name, String::new(), None)
            .unwrap();
    }
    let ada = h.user("ada");
    h.app
        .auth
        .add_link(
            "role.tenant-admin",
            Ent::User(ada.clone()),
            Ent::Tenant(acme),
            "test",
        )
        .unwrap();
    let (s, v) = h.rest("GET", "/projects", None, &h.token(&ada)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let names: Vec<&str> = v["projects"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert_eq!(names, vec!["in-acme"], "{v}");
}
