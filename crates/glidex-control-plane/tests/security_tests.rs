//! End-to-end security tests against the authenticated router
//! (spec/security.md §14): authentication, project isolation, quotas,
//! step-up, CSRF/Origin, tokens, site policies, break-glass, audit, PAM
//! login through an in-process glidex-authd, and OIDC against a mock IdP.

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use glidex_control_plane::api::{self, App, AppState, Listener, PeerUid};
use glidex_control_plane::auth::{self, store::TokenKind, AuthService, Method};
use glidex_control_plane::authz::Ent;
use glidex_control_plane::config::Config;
use glidex_control_plane::state::VmManager;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const ORIGIN: &str = "http://localhost:5173";

struct H {
    router: Router,
    app: AppState,
    _dir: TempDir,
}

fn harness_with(f: impl FnOnce(&mut Config)) -> H {
    let dir = TempDir::new().unwrap();
    std::env::set_var("GLIDEX_RUN_DIR", dir.path().join("run"));
    let manager = VmManager::with_db_path(dir.path().join("t.db")).unwrap();
    let mut cfg = Config::default();
    cfg.authz.policy_files_dir = dir.path().join("policies");
    f(&mut cfg);
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    let app = Arc::new(App { manager, auth });
    H { router: api::router(app.clone()), app, _dir: dir }
}

fn harness() -> H {
    harness_with(|_| {})
}

#[derive(Clone)]
enum As {
    Nobody,
    Bearer(String),
    /// Session cookie, CSRF value, Origin.
    Cookie(String, Option<String>, Option<&'static str>),
    Peer(u32),
}

impl H {
    fn user(&self, name: &str) -> String {
        self.app.auth.store.user_for_identity("pam", name, name, None, true).unwrap().unwrap().id
    }

    fn token(&self, user: &str) -> String {
        self.app.auth.create_token("t", TokenKind::Personal { owner: user.into() }, user, None).unwrap().0
    }

    fn session(&self, user: &str) -> As {
        let u = self.app.auth.store.user(user).unwrap().unwrap();
        let (cookie, csrf) = self.app.auth.create_session(&u, Method::Pam).unwrap();
        As::Cookie(cookie, Some(csrf), Some(ORIGIN))
    }

    fn link(&self, role: &str, principal: Ent, resource: Ent) {
        self.app.auth.add_link(role, principal, resource, "test").unwrap();
    }

    fn project(&self, name: &str) -> String {
        self.app.manager.projects().create(name, String::new(), None).unwrap().id
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>, who: &As) -> (StatusCode, Value, axum::http::HeaderMap) {
        let mut b = Request::builder().method(method).uri(path);
        match who {
            As::Nobody => {}
            As::Bearer(t) => b = b.header(header::AUTHORIZATION, format!("Bearer {}", t)),
            As::Cookie(c, csrf, origin) => {
                b = b.header(header::COOKIE, format!("{}={}", auth::SESSION_COOKIE, c));
                if let Some(x) = csrf {
                    b = b.header(auth::CSRF_HEADER, x);
                }
                if let Some(o) = origin {
                    b = b.header(header::ORIGIN, *o);
                }
            }
            As::Peer(uid) => b = b.extension(Listener::Api).extension(PeerUid(*uid)),
        }
        let req = match body {
            Some(v) => b.header(header::CONTENT_TYPE, "application/json").body(Body::from(v.to_string())).unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v, headers)
    }
}

fn vm_body(name: &str, project: &str) -> Value {
    json!({
        "name": name, "project": project, "vcpu_count": 1, "mem_size_mib": 128,
        "kernel_image_path": "/tmp/vmlinux", "rootfs_path": "/tmp/rootfs.ext4",
    })
}

#[tokio::test]
async fn unauthenticated_requests_get_401_and_health_is_public() {
    let h = harness();
    let (s, _, headers) = h.call("GET", "/health", None, &As::Nobody).await;
    assert_eq!(s, StatusCode::OK);
    assert!(headers.contains_key("x-request-id"));
    for path in ["/vms", "/disks", "/credentials", "/projects", "/auth/whoami", "/audit"] {
        let (s, v, _) = h.call("GET", path, None, &As::Nobody).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{path}: {v}");
    }
    let (s, _, _) = h.call("GET", "/vms", None, &As::Bearer("gxt_nope".into())).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn project_isolation_and_not_found() {
    let h = harness();
    let (pa, pb) = (h.project("pa"), h.project("pb"));
    let (alice, bob, carol) = (h.user("alice"), h.user("bob"), h.user("carol"));
    h.link("role.editor", Ent::User(alice.clone()), Ent::Project(pa.clone()));
    h.link("grant.host-paths", Ent::User(alice.clone()), Ent::Host);
    h.link("role.editor", Ent::User(bob.clone()), Ent::Project(pb.clone()));
    h.link("role.viewer", Ent::User(carol.clone()), Ent::Project(pa.clone()));
    let (a, b, c) = (As::Bearer(h.token(&alice)), As::Bearer(h.token(&bob)), As::Bearer(h.token(&carol)));

    let (s, vm, _) = h.call("POST", "/vms", Some(vm_body("v1", "pa")), &a).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    assert_eq!(vm["project"], pa);
    let id = vm["id"].as_str().unwrap();

    // The stored record (host paths, PIDs) is for host readers only.
    let (s, _, _) = h.call("GET", &format!("/vms/{id}?view=full"), None, &c).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let aud = h.user("aud");
    h.link("role.auditor", Ent::User(aud.clone()), Ent::Host);
    let (s, full, _) = h.call("GET", &format!("/vms/{id}?view=full"), None, &As::Bearer(h.token(&aud))).await;
    assert_eq!(s, StatusCode::OK, "{full}");
    assert_eq!(full["spec"]["power"], "stopped", "{full}");

    // Another project's editor can't see it at all.
    let (s, _, _) = h.call("GET", &format!("/vms/{id}"), None, &b).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = h.call("POST", &format!("/vms/{id}/start"), None, &b).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, list, _) = h.call("GET", "/vms", None, &b).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
    // Nor create in a project it has no role in.
    let (s, _, _) = h.call("POST", "/vms", Some(vm_body("v2", "pa")), &b).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // A viewer sees it but can't operate it.
    let (s, _, _) = h.call("GET", &format!("/vms/{id}"), None, &c).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v, _) = h.call("POST", &format!("/vms/{id}/start"), None, &c).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, _, _) = h.call("POST", &format!("/vms/{id}/console/ticket"), None, &c).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Same VM name is fine in another project.
    h.link("grant.host-paths", Ent::User(bob.clone()), Ent::Host);
    let (s, v, _) = h.call("POST", "/vms", Some(vm_body("v1", "pb")), &b).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    // Credentials are per project too.
    let cred = json!({"username": "ops", "project": "pa", "ssh_authorized_keys": ["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl ops@host"]});
    let (s, v, _) = h.call("POST", "/credentials", Some(cred), &a).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, _, _) = h.call("GET", "/credentials/ops?project=pa", None, &b).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, list, _) = h.call("GET", "/credentials", None, &b).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
    // A VM can't use another project's credential.
    let mut body = vm_body("v3", "pb");
    body["credential"] = json!("ops");
    let (s, _, _) = h.call("POST", "/vms", Some(body), &b).await;
    assert!(s.is_client_error());
}

#[tokio::test]
async fn host_paths_need_a_grant() {
    let h = harness();
    let pa = h.project("pa");
    let alice = h.user("alice");
    h.link("role.owner", Ent::User(alice.clone()), Ent::Project(pa.clone()));
    let a = As::Bearer(h.token(&alice));
    let (s, v, _) = h.call("POST", "/vms", Some(vm_body("v", "pa")), &a).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert!(v["message"].as_str().unwrap().contains("useHostPath"), "{v}");
    // VFIO paths must be PCI devices, and need a grant.
    let mut body = vm_body("v", "pa");
    body["vfio_devices"] = json!(["/dev/sda"]);
    h.link("grant.host-paths", Ent::User(alice.clone()), Ent::Host);
    let (s, _, _) = h.call("POST", "/vms", Some(body.clone()), &a).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    body["vfio_devices"] = json!(["/sys/bus/pci/devices/0000:41:00.0"]);
    let (s, v, _) = h.call("POST", "/vms", Some(body), &a).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
}

#[tokio::test]
async fn quotas_are_enforced_and_system_admin_may_exceed() {
    let h = harness();
    let pa = h.project("pa");
    let (owner, admin) = (h.user("owner"), h.user("admin"));
    h.link("role.owner", Ent::User(owner.clone()), Ent::Project(pa.clone()));
    h.link("grant.host-paths", Ent::User(owner.clone()), Ent::Host);
    h.link("role.system-admin", Ent::User(admin.clone()), Ent::Host);
    let (o, a) = (As::Bearer(h.token(&owner)), As::Bearer(h.token(&admin)));

    let (s, _, _) = h.call("PATCH", &format!("/projects/{pa}"), Some(json!({"quotas": {"vms": 1}})), &o).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "owners don't set quotas");
    let (s, v, _) = h.call("PATCH", &format!("/projects/{pa}"), Some(json!({"quotas": {"vms": 1}})), &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    let (s, _, _) = h.call("POST", "/vms", Some(vm_body("v1", "pa")), &o).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v, _) = h.call("POST", "/vms", Some(vm_body("v2", "pa")), &o).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "quota_exceeded");
    assert_eq!(v["details"]["quota"][0]["resource"], "vms");

    let (s, v, _) = h.call("POST", "/vms", Some(vm_body("v3", "pa")), &a).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (_, project, _) = h.call("GET", &format!("/projects/{pa}"), None, &a).await;
    assert_eq!(project["usage"]["vms"], 2);
    // The overrun is audited.
    let entries = h.app.auth.store.audit(0, 100, |e| e.action == "createVm").unwrap();
    assert!(entries.iter().any(|e| e.details.get("quota_exceeded").is_some()), "{entries:#?}");
}

#[tokio::test]
async fn host_network_changes_need_a_recent_login() {
    let h = harness();
    let n = h.user("net");
    h.link("role.net-admin", Ent::User(n.clone()), Ent::Host);
    let fresh = h.session(&n);
    let As::Cookie(cookie, csrf, origin) = fresh.clone() else { unreachable!() };
    let hash = auth::sha256_hex(cookie.as_bytes());
    let mut s = h.app.auth.store.session(&hash).unwrap().unwrap();
    s.authenticated_at -= 700;
    h.app.auth.store.put_session(&hash, &s).unwrap();
    let stale = As::Cookie(cookie, csrf, origin);
    let body = json!({"profile": "kernel"});
    let (st, v, _) = h.call("POST", "/ovs/install", Some(body.clone()), &stale).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["error"], "reauth_required");
    // Non-critical host-network reads don't need it.
    let (st, _, _) = h.call("GET", "/ovs/status", None, &stale).await;
    assert_eq!(st, StatusCode::OK);
    // A fresh login passes authorization (netd itself isn't running here).
    let fresh = h.session(&n);
    let (st, v, _) = h.call("POST", "/ovs/install", Some(body), &fresh).await;
    assert!(st != StatusCode::UNAUTHORIZED && st != StatusCode::FORBIDDEN, "{st} {v}");
}

#[tokio::test]
async fn cookie_writes_need_csrf_and_origin() {
    let h = harness();
    let pa = h.project("pa");
    let u = h.user("u");
    h.link("role.editor", Ent::User(u.clone()), Ent::Project(pa.clone()));
    let As::Cookie(cookie, csrf, _) = h.session(&u) else { unreachable!() };
    let cred = |n: &str| json!({"username": n, "project": "pa", "password": "long-enough-password"});

    let (s, v, _) = h.call("POST", "/credentials", Some(cred("a")), &As::Cookie(cookie.clone(), None, Some(ORIGIN))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("csrf_failed")));
    let (s, _, _) = h.call("POST", "/credentials", Some(cred("a")), &As::Cookie(cookie.clone(), csrf.clone(), None)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "a cookie write needs an Origin");
    let (s, v, _) = h.call("POST", "/credentials", Some(cred("a")), &As::Cookie(cookie.clone(), Some("wrong".into()), Some(ORIGIN))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("csrf_failed")));
    let (s, v, _) = h.call("POST", "/credentials", Some(cred("a")), &As::Cookie(cookie.clone(), csrf.clone(), Some("https://evil.example"))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("origin_not_allowed")));
    let (s, v, _) = h.call("POST", "/credentials", Some(cred("a")), &As::Cookie(cookie.clone(), csrf.clone(), Some(ORIGIN))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    // Reads need neither.
    let (s, _, _) = h.call("GET", "/credentials", None, &As::Cookie(cookie, None, None)).await;
    assert_eq!(s, StatusCode::OK);
    // A foreign Origin is refused for bearer writes too; no Origin is fine.
    let t = h.token(&u);
    let (s, _, _) = h.call("POST", "/credentials", Some(cred("b")), &As::Bearer(t.clone())).await;
    assert_eq!(s, StatusCode::CREATED);
}

#[tokio::test]
async fn tokens_are_capped_by_their_links_and_owner() {
    let h = harness();
    let pa = h.project("pa");
    let u = h.user("u");
    h.link("role.editor", Ent::User(u.clone()), Ent::Project(pa.clone()));
    let session = h.session(&u);
    let (s, v, _) = h
        .call("POST", "/tokens", Some(json!({"name": "ro", "roles": [{"role": "role.viewer", "project": "pa"}]})), &session)
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let secret = v["token"].as_str().unwrap().to_string();
    assert!(secret.starts_with("gxt_"));
    let ro = As::Bearer(secret.clone());
    let (s, _, _) = h.call("GET", "/credentials?project=pa", None, &ro).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _, _) = h.call("POST", "/credentials", Some(json!({"username": "x", "project": "pa", "password": "long-enough-password"})), &ro).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Tokens don't mint tokens.
    let (s, _, _) = h.call("POST", "/tokens", Some(json!({"name": "x"})), &ro).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Listed (without the secret), then revoked.
    let (_, list, _) = h.call("GET", "/tokens", None, &session).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(!list.to_string().contains(&secret));
    let id = list[0]["id"].as_str().unwrap();
    let (s, _, _) = h.call("DELETE", &format!("/tokens/{id}"), None, &session).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = h.call("GET", "/credentials?project=pa", None, &ro).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Service accounts need project.members.
    let (s, _, _) = h.call("POST", "/tokens", Some(json!({"name": "ci", "kind": "service_account", "project": "pa"})), &session).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    h.link("role.owner", Ent::User(u.clone()), Ent::Project(pa.clone()));
    let (s, v, _) = h
        .call("POST", "/tokens", Some(json!({"name": "ci", "kind": "service_account", "project": "pa", "roles": [{"role": "role.operator"}]})), &session)
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let sa = As::Bearer(v["token"].as_str().unwrap().into());
    let (s, _, _) = h.call("GET", "/vms", None, &sa).await;
    assert_eq!(s, StatusCode::OK);
}

fn my_group() -> String {
    nix::unistd::Group::from_gid(nix::unistd::getgid()).unwrap().unwrap().name
}

#[tokio::test]
async fn site_policies_api_and_break_glass() {
    let g = my_group();
    let h = harness_with(|c| {
        c.admin_group = g.clone();
        c.users_group = g.clone();
    });
    let pa = h.project("pa");
    let (admin, alice) = (h.user("admin"), h.user("alice"));
    h.link("role.system-admin", Ent::User(admin.clone()), Ent::Host);
    h.link("role.viewer", Ent::User(alice.clone()), Ent::Project(pa.clone()));
    let a = h.session(&admin);
    let al = As::Bearer(h.token(&alice));

    let forbid_read = "@id(\"site.no-read\")\nforbid (principal, action == Glidex::Action::\"readProject\", resource);";
    let (s, v, _) = h.call("PUT", "/authz/policies/site.no-read", Some(json!({"text": forbid_read, "version": 1})), &a).await;
    assert_eq!(s, StatusCode::CONFLICT, "creating needs version 0: {v}");
    let (s, v, _) = h.call("PUT", "/authz/policies/site.no-read", Some(json!({"text": "@id(\"site.no-read\") permit(principal, action == Glidex::Action::\"nope\", resource);", "version": 0})), &a).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_policy")), "{v}");
    let (s, _, _) = h.call("PUT", "/authz/policies/base.mine", Some(json!({"text": "@id(\"base.mine\") permit(principal, action, resource);", "version": 0})), &a).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // Simulate before saving: the candidate denies, the current set allows.
    let sim = json!({
        "changes": [{"id": "site.no-read", "text": forbid_read}],
        "requests": [{"principal": {"type": "User", "id": alice}, "action": "readProject", "resource": {"type": "Project", "id": pa}}],
    });
    let (s, v, _) = h.call("POST", "/authz/simulate", Some(sim), &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["results"][0]["current"]["allowed"], true);
    assert_eq!(v["results"][0]["candidate"]["allowed"], false);
    assert!(v["results"][0]["candidate"]["policies"].to_string().contains("site.no-read"));

    let (s, v, _) = h.call("PUT", "/authz/policies/site.no-read", Some(json!({"text": forbid_read, "version": 0})), &a).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["version"], 1);
    let (s, _, _) = h.call("GET", &format!("/projects/{pa}"), None, &al).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the site forbid applies");
    let (s, _, _) = h.call("PUT", "/authz/policies/site.no-read", Some(json!({"text": forbid_read, "version": 0})), &a).await;
    assert_eq!(s, StatusCode::CONFLICT, "stale version");

    // A policy that would stop the admin from managing policies is refused.
    let lockout = "@id(\"site.lockout\")\nforbid (principal, action == Glidex::Action::\"writePolicy\", resource);";
    let (s, v, _) = h.call("PUT", "/authz/policies/site.lockout", Some(json!({"text": lockout, "version": 0})), &a).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::CONFLICT, Some("would_lock_out")), "{v}");

    // Break-glass on api.sock ignores site policies, so it can recover.
    let me = nix::unistd::getuid().as_raw();
    let (s, v, _) = h.call("GET", "/auth/whoami", None, &As::Peer(me)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["break_glass"], true);
    assert_eq!(v["method"], "peer");
    let (s, _, _) = h.call("GET", &format!("/projects/{pa}"), None, &As::Peer(me)).await;
    assert_eq!(s, StatusCode::OK);
    let (_, hist, _) = h.call("GET", "/authz/policies/site.no-read/versions", None, &As::Peer(me)).await;
    assert_eq!(hist.as_array().unwrap().len(), 1);
    let (s, _, _) = h.call("DELETE", "/authz/policies/site.no-read?version=1", None, &As::Peer(me)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = h.call("GET", &format!("/projects/{pa}"), None, &al).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn peer_identity_requires_the_glidex_groups() {
    let h = harness_with(|c| {
        c.users_group = "no-such-group-xyz".into();
        c.admin_group = "no-such-group-abc".into();
    });
    let me = nix::unistd::getuid().as_raw();
    let (s, v, _) = h.call("GET", "/auth/whoami", None, &As::Peer(me)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let g = my_group();
    // Independent of whether this host's user is in glidex-admin.
    let h = harness_with(|c| {
        c.users_group = g.clone();
        c.admin_group = "no-such-group-abc".into();
    });
    let (s, v, _) = h.call("GET", "/auth/whoami", None, &As::Peer(me)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["break_glass"], false);
    assert!(v["teams"].to_string().contains(&format!("unix:{}", g)));
    // A local user can open a browser session for themselves.
    let (s, v, _) = h.call("POST", "/auth/session", None, &As::Peer(me)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let cookie = As::Cookie(v["cookie"].as_str().unwrap().into(), None, None);
    let (s, v, _) = h.call("GET", "/auth/whoami", None, &cookie).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["method"], "pam");
}

#[tokio::test]
async fn audit_records_decisions_without_secrets() {
    let h = harness();
    let pa = h.project("pa");
    let (u, aud) = (h.user("u"), h.user("aud"));
    h.link("role.editor", Ent::User(u.clone()), Ent::Project(pa.clone()));
    h.link("role.auditor", Ent::User(aud.clone()), Ent::Host);
    let t = h.token(&u);
    let ut = As::Bearer(t.clone());
    let pw = "super-secret-password-123";
    h.call("POST", "/credentials", Some(json!({"username": "x", "project": "pa", "password": pw})), &ut).await;
    h.call("DELETE", &format!("/projects/{pa}"), None, &ut).await;
    let (s, v, _) = h.call("GET", "/audit", None, &As::Bearer(h.token(&aud))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let text = v.to_string();
    assert!(!text.contains(pw) && !text.contains(&t), "no secrets in the audit log");
    let created = v.as_array().unwrap().iter().find(|e| e["action"] == "createCredential").unwrap();
    assert_eq!(created["result"], "ok");
    assert_eq!(created["project"], pa);
    assert!(created["policies"].as_array().unwrap().iter().any(|p| p.as_str().unwrap().starts_with("link.")));
    let denied = v.as_array().unwrap().iter().find(|e| e["action"] == "deleteProject").unwrap();
    assert_eq!(denied["result"], "denied");
    // An editor can't read the audit log.
    let (s, _, _) = h.call("GET", "/audit", None, &ut).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

// ---- PAM through an in-process glidex-authd -------------------------------

mod pam {
    use glidex_authd::authenticator::{Account, Accounts, AuthFailure, Authenticator};
    use std::collections::HashMap;

    pub struct FakePam;
    impl Authenticator for FakePam {
        fn authenticate(&self, _service: &str, user: &str, password: &str) -> Result<(), AuthFailure> {
            if password == format!("{}-password", user) {
                Ok(())
            } else {
                Err(AuthFailure::Denied("bad".into()))
            }
        }
    }

    pub struct FakeKeys;
    impl glidex_authd::keys::KeyReader for FakeKeys {
        fn public_keys(&self, user: &str) -> Result<Vec<String>, String> {
            Ok(vec![format!("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoq {}@laptop", user)])
        }
    }

    pub struct FakeAccounts(pub HashMap<String, Account>);
    impl Accounts for FakeAccounts {
        fn lookup(&self, user: &str) -> Option<Account> {
            self.0.get(user).cloned()
        }
    }
}

#[tokio::test]
async fn pam_login_provisions_on_first_login() {
    use glidex_authd::authenticator::Account;
    let dir = TempDir::new().unwrap();
    let sock = dir.path().join("auth.sock");
    let accounts = [
        ("alice", vec!["alice", "glidex-users", "lab-unix"]),
        ("mallory", vec!["mallory", "staff"]),
    ]
    .into_iter()
    .map(|(n, g)| (n.to_string(), Account { uid: 1000, groups: g.into_iter().map(String::from).collect() }))
    .collect();
    let cfg = glidex_authd::config::Config { failure_delay: std::time::Duration::from_millis(1), ..Default::default() };
    let authd = Arc::new(
        glidex_authd::server::Authd::new(
            cfg,
            Some(nix::unistd::getuid().as_raw()),
            Arc::new(pam::FakePam),
            Arc::new(pam::FakeAccounts(accounts)),
        )
        .with_key_reader(Arc::new(pam::FakeKeys)),
    );
    let l = glidex_authd::server::bind(&sock, 0o600, None).unwrap();
    std::thread::spawn(move || authd.serve(l));

    let h = harness_with(|c| {
        c.auth.pam.authd_socket = sock.clone();
        c.auth.pam.group_teams.insert("lab-unix".into(), "lab".into());
    });
    h.app.auth.store.put_team(&auth::store::Team { id: "team-lab".into(), name: "lab".into(), members: vec![], created_at: 0 }).unwrap();
    let login = |u: &str, p: &str| json!({"method": "pam", "username": u, "password": p});
    let (s, v, _) = h.call("POST", "/auth/login", Some(login("alice", "wrong")), &As::Nobody).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("login_failed")));
    let (s, _, _) = h.call("POST", "/auth/login", Some(login("mallory", "mallory-password")), &As::Nobody).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "not in glidex-users");
    let (s, v, headers) = h.call("POST", "/auth/login", Some(login("alice", "alice-password")), &As::Nobody).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let set_cookie = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"), "{set_cookie}");
    let cookie = set_cookie.split(';').next().unwrap().split_once('=').unwrap().1.to_string();
    let csrf = v["csrf"].as_str().unwrap().to_string();
    let me = As::Cookie(cookie.clone(), Some(csrf.clone()), Some(ORIGIN));
    let (_, who, _) = h.call("GET", "/auth/whoami", None, &me).await;
    assert_eq!(who["user"]["display_name"], "alice");
    assert_eq!(who["teams"], json!(["team-lab"]), "group_teams synced");
    // Their own public keys, for prefilling a guest credential.
    let (s, keys, _) = h.call("GET", "/users/me/ssh-keys", None, &me).await;
    assert_eq!(s, StatusCode::OK, "{keys}");
    assert_eq!(keys["available"], true);
    assert_eq!(keys["username"], "alice");
    assert_eq!(keys["keys"], json!(["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoq alice@laptop"]));
    // A user without a local account (e.g. SSO only) gets a reason instead.
    let sso = h.app.auth.store.user_for_identity("oidc:https://idp", "s1", "Sam", None, true).unwrap().unwrap();
    let (_, keys, _) = h.call("GET", "/users/me/ssh-keys", None, &As::Bearer(h.token(&sso.id))).await;
    assert_eq!(keys["available"], false);
    assert!(keys["reason"].as_str().unwrap().contains("paste"));
    // New users have no rights yet.
    let (_, projects, _) = h.call("GET", "/projects", None, &me).await;
    assert_eq!(projects.as_array().unwrap().len(), 0);
    // Logout ends the session.
    let (s, _, _) = h.call("POST", "/auth/logout", None, &me).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = h.call("GET", "/auth/whoami", None, &me).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Login events are audited, never with the password.
    let entries = h.app.auth.store.audit(0, 100, |e| e.action == "login").unwrap();
    assert!(entries.iter().any(|e| e.result == "ok") && entries.iter().any(|e| e.result == "denied"));
    assert!(!serde_json::to_string(&entries).unwrap().contains("alice-password"));
}

#[tokio::test]
async fn pam_login_without_jit_needs_a_provisioned_user() {
    use glidex_authd::authenticator::Account;
    let dir = TempDir::new().unwrap();
    let sock = dir.path().join("auth.sock");
    let accounts = [("bob".to_string(), Account { uid: 1, groups: vec!["glidex-users".into()] })].into_iter().collect();
    let cfg = glidex_authd::config::Config { failure_delay: std::time::Duration::from_millis(1), ..Default::default() };
    let authd = Arc::new(glidex_authd::server::Authd::new(cfg, Some(nix::unistd::getuid().as_raw()), Arc::new(pam::FakePam), Arc::new(pam::FakeAccounts(accounts))));
    let l = glidex_authd::server::bind(&sock, 0o600, None).unwrap();
    std::thread::spawn(move || authd.serve(l));
    let h = harness_with(|c| {
        c.auth.pam.authd_socket = sock.clone();
        c.auth.pam.jit = false;
    });
    let body = json!({"username": "bob", "password": "bob-password"});
    let (s, _, _) = h.call("POST", "/auth/login", Some(body.clone()), &As::Nobody).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    h.user("bob");
    let (s, _, _) = h.call("POST", "/auth/login", Some(body), &As::Nobody).await;
    assert_eq!(s, StatusCode::OK);
}

// ---- OIDC against a mock IdP ---------------------------------------------

mod idp {
    use axum::{extract::State, routing::get, routing::post, Json, Router};
    use base64::Engine as _;
    use jsonwebtoken::{EncodingKey, Header};
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::traits::PublicKeyParts;
    use std::sync::{Arc, Mutex, OnceLock};

    pub struct Key {
        pub pem: String,
        pub n: String,
        pub e: String,
    }

    pub fn key() -> &'static Key {
        static K: OnceLock<Key> = OnceLock::new();
        K.get_or_init(|| {
            let k = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
            let b = |v: Vec<u8>| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
            Key {
                pem: k.to_pkcs1_pem(rsa::pkcs8::LineEnding::LF).unwrap().to_string(),
                n: b(k.n().to_bytes_be()),
                e: b(k.e().to_bytes_be()),
            }
        })
    }

    #[derive(Default)]
    pub struct Mock {
        pub base: String,
        pub id_token: Mutex<Option<String>>,
        pub device_polls: Mutex<u32>,
        pub token_requests: Mutex<Vec<String>>,
    }

    pub fn sign(claims: serde_json::Value, kid: &str) -> String {
        let mut h = Header::new(jsonwebtoken::Algorithm::RS256);
        h.kid = Some(kid.into());
        jsonwebtoken::encode(&h, &claims, &EncodingKey::from_rsa_pem(key().pem.as_bytes()).unwrap()).unwrap()
    }

    pub async fn start() -> Arc<Mock> {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let mock = Arc::new(Mock { base: base.clone(), ..Default::default() });
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(|State(m): State<Arc<Mock>>| async move {
                    Json(serde_json::json!({
                        "issuer": m.base, "authorization_endpoint": format!("{}/auth", m.base),
                        "token_endpoint": format!("{}/token", m.base), "jwks_uri": format!("{}/jwks", m.base),
                        "device_authorization_endpoint": format!("{}/device", m.base),
                    }))
                }),
            )
            .route(
                "/jwks",
                get(|| async {
                    let k = key();
                    Json(serde_json::json!({"keys": [{"kty": "RSA", "kid": "k1", "alg": "RS256", "use": "sig", "n": k.n, "e": k.e}]}))
                }),
            )
            .route(
                "/token",
                post(|State(m): State<Arc<Mock>>, headers: axum::http::HeaderMap, body: String| async move {
                    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                    m.token_requests.lock().unwrap().push(format!("{} {}", auth, body));
                    if body.contains("device_code") {
                        let mut n = m.device_polls.lock().unwrap();
                        *n += 1;
                        if *n == 1 {
                            return (axum::http::StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "authorization_pending"})));
                        }
                    }
                    let t = m.id_token.lock().unwrap().clone().unwrap_or_default();
                    (axum::http::StatusCode::OK, Json(serde_json::json!({"id_token": t, "access_token": "x", "token_type": "Bearer"})))
                }),
            )
            .route(
                "/device",
                post(|State(m): State<Arc<Mock>>| async move {
                    Json(serde_json::json!({"device_code": "dev-1", "user_code": "ABCD-EFGH", "verification_uri": format!("{}/activate", m.base), "interval": 1, "expires_in": 600}))
                }),
            )
            .with_state(mock.clone());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        mock
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

async fn oidc_harness(mock: &idp::Mock, f: impl FnOnce(&mut Config)) -> (H, TempDir) {
    let secret_dir = TempDir::new().unwrap();
    let secret = secret_dir.path().join("secret");
    std::fs::write(&secret, "test-client-secret\n").unwrap();
    std::env::set_var("GLIDEX_OIDC_CLIENT_SECRET_FILE", &secret);
    let base = mock.base.clone();
    let h = harness_with(move |c| {
        c.auth.oidc.enabled = true;
        c.auth.oidc.issuer = base;
        c.auth.oidc.client_id = "glidex".into();
        c.auth.oidc.group_teams.insert("lab-staff".into(), "lab".into());
        f(c);
    });
    (h, secret_dir)
}

/// Start a browser login; returns (state, nonce, code_challenge, browser cookie).
async fn oidc_begin(h: &H) -> (String, String, String, String) {
    let (s, _, headers) = h.call("GET", "/auth/oidc/start?return_to=/vms", None, &As::Nobody).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    let loc = reqwest::Url::parse(headers.get(header::LOCATION).unwrap().to_str().unwrap()).unwrap();
    let q: std::collections::HashMap<_, _> = loc.query_pairs().into_owned().collect();
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["response_type"], "code");
    let browser = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
    (q["state"].clone(), q["nonce"].clone(), q["code_challenge"].clone(), browser)
}

async fn oidc_finish(h: &H, state: &str, browser: Option<&str>) -> (StatusCode, axum::http::HeaderMap) {
    let mut b = Request::builder().uri(format!("/auth/oidc/callback?code=c1&state={state}"));
    if let Some(c) = browser {
        b = b.header(header::COOKIE, c);
    }
    let resp = h.router.clone().oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    (resp.status(), resp.headers().clone())
}

fn claims(mock: &idp::Mock, nonce: &str) -> Value {
    json!({
        "iss": mock.base, "aud": "glidex", "sub": "user-123", "exp": now() + 300, "iat": now(),
        "nonce": nonce, "email": "alice@example.org", "name": "Alice", "groups": ["lab-staff"],
    })
}

#[tokio::test]
async fn oidc_code_flow_with_pkce() {
    let mock = idp::start().await;
    let (h, _s) = oidc_harness(&mock, |_| {}).await;
    h.app.auth.store.put_team(&auth::store::Team { id: "team-lab".into(), name: "lab".into(), members: vec![], created_at: 0 }).unwrap();
    let (s, v, _) = h.call("GET", "/auth/methods", None, &As::Nobody).await;
    assert_eq!((s, v["oidc"].as_bool()), (StatusCode::OK, Some(true)));

    let (state, nonce, challenge, browser) = oidc_begin(&h).await;
    *mock.id_token.lock().unwrap() = Some(idp::sign(claims(&mock, &nonce), "k1"));
    let (s, headers) = oidc_finish(&h, &state, Some(&browser)).await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/vms");
    // The token request carried the client secret and a verifier for the challenge.
    let req = mock.token_requests.lock().unwrap().last().cloned().unwrap();
    assert!(req.starts_with("Basic "), "{req}");
    let verifier = req.split("code_verifier=").nth(1).unwrap().split('&').next().unwrap();
    use sha2::Digest;
    use base64::Engine as _;
    let expect = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    assert_eq!(expect, challenge);
    let session = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|c| c.starts_with("gx_session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_string();
    let (_, who, _) = h.call("GET", "/auth/whoami", None, &As::Cookie(session, None, None)).await;
    assert_eq!(who["user"]["display_name"], "Alice");
    assert_eq!(who["method"], "oidc");
    assert_eq!(who["teams"], json!(["team-lab"]));

    // The state is single-use.
    let (s, _) = oidc_finish(&h, &state, Some(&browser)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn oidc_rejects_bad_tokens() {
    let mock = idp::start().await;
    let (h, _s) = oidc_harness(&mock, |_| {}).await;
    type MakeToken<'a> = Box<dyn Fn(&str) -> String + 'a>;
    let cases: Vec<(&str, MakeToken)> = vec![
        ("wrong nonce", Box::new(|_n| idp::sign(claims(&mock, "other"), "k1"))),
        ("wrong audience", Box::new(|n| { let mut c = claims(&mock, n); c["aud"] = json!("someone-else"); idp::sign(c, "k1") })),
        ("wrong issuer", Box::new(|n| { let mut c = claims(&mock, n); c["iss"] = json!("https://evil.example"); idp::sign(c, "k1") })),
        ("expired", Box::new(|n| { let mut c = claims(&mock, n); c["exp"] = json!(now() - 600); idp::sign(c, "k1") })),
        ("issued in the future", Box::new(|n| { let mut c = claims(&mock, n); c["iat"] = json!(now() + 3600); idp::sign(c, "k1") })),
        ("unknown key", Box::new(|n| idp::sign(claims(&mock, n), "k2"))),
        ("HS256", Box::new(|n| jsonwebtoken::encode(&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256), &claims(&mock, n), &jsonwebtoken::EncodingKey::from_secret(b"test-client-secret")).unwrap())),
        ("alg none", Box::new(|n| {
            use base64::Engine as _;
            let b = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
            format!("{}.{}.", b(&json!({"alg": "none", "typ": "JWT"})), b(&claims(&mock, n)))
        })),
    ];
    for (name, make) in cases {
        let (state, nonce, _, browser) = oidc_begin(&h).await;
        *mock.id_token.lock().unwrap() = Some(make(&nonce));
        let (s, _) = oidc_finish(&h, &state, Some(&browser)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{name}");
    }
    // The state belongs to the browser that started the login.
    let (state, nonce, _, _) = oidc_begin(&h).await;
    *mock.id_token.lock().unwrap() = Some(idp::sign(claims(&mock, &nonce), "k1"));
    let (s, _) = oidc_finish(&h, &state, Some("gx_oidc=someone-else")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn oidc_login_rules_and_device_grant() {
    let mock = idp::start().await;
    let (h, _s) = oidc_harness(&mock, |c| {
        c.auth.oidc.required_groups = vec!["glidex".into()];
    })
    .await;
    let (state, nonce, _, browser) = oidc_begin(&h).await;
    *mock.id_token.lock().unwrap() = Some(idp::sign(claims(&mock, &nonce), "k1"));
    let (s, _) = oidc_finish(&h, &state, Some(&browser)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "not in a required group");

    let (h, _s) = oidc_harness(&mock, |_| {}).await;
    let (s, v, _) = h.call("POST", "/auth/oidc/device", None, &As::Nobody).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["user_code"], "ABCD-EFGH");
    let mut c = claims(&mock, "");
    c.as_object_mut().unwrap().remove("nonce");
    *mock.id_token.lock().unwrap() = Some(idp::sign(c, "k1"));
    let poll = json!({"device_code": "dev-1"});
    let (s, v, _) = h.call("POST", "/auth/oidc/device/poll", Some(poll.clone()), &As::Nobody).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::ACCEPTED, Some("pending")));
    let (s, v, _) = h.call("POST", "/auth/oidc/device/poll", Some(poll), &As::Nobody).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let token = v["token"].as_str().unwrap().to_string();
    let (_, who, _) = h.call("GET", "/auth/whoami", None, &As::Bearer(token)).await;
    assert_eq!(who["user"]["display_name"], "Alice");
}

#[tokio::test]
async fn console_tickets_need_vm_console() {
    let h = harness();
    let pa = h.project("pa");
    let (op, viewer) = (h.user("op"), h.user("viewer"));
    h.link("role.operator", Ent::User(op.clone()), Ent::Project(pa.clone()));
    h.link("role.viewer", Ent::User(viewer.clone()), Ent::Project(pa.clone()));
    let vm = h
        .app
        .manager
        .create_vm_in(
            &pa,
            "v".into(),
            serde_json::from_value(json!({"vcpu_count": 1, "mem_size_mib": 64, "kernel_image_path": "/tmp/k", "rootfs_path": "/tmp/r", "kernel_args": ""})).unwrap(),
            Default::default(),
            glidex_control_plane::tenancy::QuotaMode::MayExceed,
        )
        .await
        .unwrap()
        .0;
    let (s, v, _) = h.call("POST", &format!("/vms/{}/console/ticket", vm.id), None, &h.session(&op)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["ticket"].as_str().unwrap().len() > 20);
    let (s, _, _) = h.call("POST", &format!("/vms/{}/console/ticket", vm.id), None, &h.session(&viewer)).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _, _) = h.call("GET", &format!("/vms/{}/console/log", vm.id), None, &As::Bearer(h.token(&viewer))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "the console log is console output");
    let (s, _, _) = h.call("GET", &format!("/vms/{}/console/log", vm.id), None, &As::Bearer(h.token(&op))).await;
    assert_eq!(s, StatusCode::OK);
}

// ---- control-plane.json: site quota defaults (spec §6.3, §13) ------------

#[tokio::test]
async fn site_default_quotas_apply_to_new_projects() {
    use glidex_control_plane::tenancy::Quotas;
    let h = harness_with(|c| c.quotas.default = Quotas { vms: Some(1), networks: Some(2), ..Default::default() });
    let admin = h.user("admin");
    h.link("role.system-admin", Ent::User(admin.clone()), Ent::Host);
    let a = As::Bearer(h.token(&admin));

    // No quotas in the request: the site default.
    let (s, p, _) = h.call("POST", "/projects", Some(json!({"name": "lab"})), &a).await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert_eq!(p["quotas"], json!({"vms": 1, "vcpus": null, "memory_mib": null, "disk_gib": null, "running_vms": null, "networks": 2}));
    // Explicit quotas win.
    let (s, q, _) = h.call("POST", "/projects", Some(json!({"name": "big", "quotas": {"vms": 10}})), &a).await;
    assert_eq!(s, StatusCode::CREATED, "{q}");
    assert_eq!((q["quotas"]["vms"].as_u64(), q["quotas"]["networks"].as_u64()), (Some(10), Some(2)));
    // A typo in a quota name is refused, not ignored.
    let (s, _, _) = h.call("POST", "/projects", Some(json!({"name": "typo", "quotas": {"vm": 10}})), &a).await;
    assert!(s.is_client_error());

    // The default is enforced for the project's members.
    let lab = p["id"].as_str().unwrap().to_string();
    let owner = h.user("owner");
    h.link("role.owner", Ent::User(owner.clone()), Ent::Project(lab.clone()));
    h.link("grant.host-paths", Ent::User(owner.clone()), Ent::Host);
    let o = As::Bearer(h.token(&owner));
    let (s, v, _) = h.call("POST", "/vms", Some(vm_body("v1", "lab")), &o).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v, _) = h.call("POST", "/vms", Some(vm_body("v2", "lab")), &o).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("quota_exceeded")), "{v}");
}

#[tokio::test]
async fn control_plane_json_drives_the_service() {
    let dir = TempDir::new().unwrap();
    let cfg_path = dir.path().join("control-plane.json");
    std::fs::write(
        &cfg_path,
        json!({
            "listen": ["127.0.0.1:0"],
            "authz": { "policy_files_dir": dir.path().join("policies"), "policy_history": 3 },
            "quotas": { "default": { "vms": 4, "networks": null } },
            "auth": { "session": { "idle_minutes": 5, "absolute_hours": 1 } },
        })
        .to_string(),
    )
    .unwrap();
    std::env::set_var("GLIDEX_CONFIG", &cfg_path);
    let cfg = Config::load().unwrap();
    std::env::remove_var("GLIDEX_CONFIG");
    assert_eq!(cfg.quotas.default.vms, Some(4));
    assert_eq!(cfg.quotas.default.networks, None, "null means unlimited");

    let manager = VmManager::with_db_path(dir.path().join("t.db")).unwrap();
    // The default project takes the site default once (main.rs does this
    // at startup).
    assert!(manager.projects().adopt_default_quotas(&cfg.quotas.default).unwrap());
    assert_eq!(manager.projects().resolve("default").unwrap().quotas, cfg.quotas.default);
    let auth = AuthService::new(manager.database(), cfg.clone()).unwrap();
    // policy_history reaches the store.
    for v in 0..5 {
        auth.store.write_site_policy("site.x", v, Some(("t", "", true)), "u").unwrap();
    }
    assert_eq!(auth.store.site_policy_versions("site.x").unwrap().len(), 3);

    // A config that parses but is out of range is refused at startup.
    let mut bad = cfg;
    bad.authz.policy_history = 0;
    assert!(AuthService::new(manager.database(), bad).is_err());
    std::fs::write(&cfg_path, r#"{"quotas": {"default": {"network": 4}}}"#).unwrap();
    assert!(Config::load_from(&cfg_path).is_err(), "typos stop the control plane");
}
