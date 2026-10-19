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
