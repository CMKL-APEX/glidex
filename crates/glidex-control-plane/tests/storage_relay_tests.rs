//! The storage relay (spec/19-tenancy.md D86): a tenant's member reads a
//! trusted hyperstor cluster's storage through this server, which calls
//! the cluster with an access token it mints and never hands out. The
//! cluster is a stand-in over pinned HTTPS.

mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use glidex_control_plane::api::{self, App, AppState};
use glidex_control_plane::auth::{self, AuthService};
use glidex_control_plane::authz::Ent;
use glidex_control_plane::config::Config;
use glidex_control_plane::tenants::DEFAULT_TENANT;
use glidex_control_plane::trust::TrustedCluster;
use glidex_trust::fake::{Reply, Request as Seen, serve_https};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tower::ServiceExt;

const ORIGIN: &str = "https://localhost:5173";
const BASE: &str = "https://glidex.example:8841";
const CLUSTER: &str = "c-store";

struct Cp {
    router: Router,
    app: AppState,
    _dir: TempDir,
}

fn cp() -> Cp {
    let dir = TempDir::new().unwrap();
    let manager = common::with_db_path(dir.path().join("t.db")).unwrap();
    let mut cfg = Config::default();
    cfg.authz.policy_files_dir = dir.path().join("policies");
    cfg.auth.allowed_origins = vec![ORIGIN.into()];
    cfg.auth.secrets_key = Some(dir.path().join("secrets.kek"));
    cfg.auth.issuer_base = Some(BASE.into());
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    let app = Arc::new(App { manager, auth });
    Cp {
        router: api::router(app.clone()),
        app,
        _dir: dir,
    }
}

impl Cp {
    /// A browser session of a new user `name`, a member of the default
    /// tenant when `member`.
    fn session(&self, name: &str, member: bool) -> (String, String) {
        let id = self
            .app
            .auth
            .store
            .user_for_identity("pam", name, name, None, true)
            .unwrap()
            .unwrap()
            .id;
        if member {
            self.app
                .auth
                .tenants
                .add_member(DEFAULT_TENANT, Ent::User(id.clone()), "test")
                .unwrap();
        }
        let u = self.app.auth.store.user(&id).unwrap().unwrap();
        let (cookie, csrf) = self.app.auth.create_session(&u, auth::Method::Pam).unwrap();
        (id, format!("{cookie}|{csrf}"))
    }

    async fn relay(
        &self,
        session: &str,
        cluster: &str,
        call: &str,
        body: Value,
    ) -> (StatusCode, Value, String) {
        let (cookie, csrf) = session.split_once('|').unwrap();
        let req = Request::builder()
            .method("POST")
            .uri(format!(
                "/tenants/{DEFAULT_TENANT}/clusters/{cluster}/storage/{call}"
            ))
            .header(
                header::COOKIE,
                format!("{}={}", auth::SESSION_COOKIE, cookie),
            )
            .header(auth::CSRF_HEADER, csrf)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let s = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes).to_string();
        (
            s,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            text,
        )
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_relay_reads_a_trusted_clusters_storage_as_the_person() {
    // The cluster's API: what it was asked, and its answers.
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    let mgmt = serve_https(Arc::new(move |r: Seen| {
        let path = r.path.clone();
        let method = r.method.clone();
        log.lock().unwrap().push(r);
        Box::pin(async move {
            match (method.as_str(), path.as_str()) {
                ("POST", "/api/hyperstor.v1.ClusterService/ListVolumes") => Reply::json(
                    200,
                    json!({ "volumes": [{ "id": "a1", "name": "web-root", "pool": 3, "size": "1024", "tenant": 7, "project": 71 }] }),
                ),
                ("POST", "/api/hyperstor.v1.S3Service/ListS3Keys") => Reply::json(
                    403,
                    json!({ "code": "permission_denied", "message": "not yours", "details": [{ "type": "google.rpc.ErrorInfo", "debug": { "reason": "outside_realm" } }] }),
                ),
                ("GET", "/auth/whoami") => Reply::json(401, json!({ "error": "unauthenticated", "message": "directory_stale" })),
                _ => Reply::json(404, json!({ "error": "not_found" })),
            }
        })
    }))
    .await
    .unwrap();

    let c = cp();
    let now = glidex_trust::now();
    c.app
        .auth
        .tenants
        .set_issuer("local", &format!("{BASE}/d/local"), false)
        .unwrap();
    c.app
        .auth
        .connectors
        .maintain_signing_keys("local", now)
        .unwrap();
    for (id, api) in [
        (CLUSTER, mgmt.base.clone()),
        ("c-elsewhere", mgmt.base.clone()),
    ] {
        c.app
            .auth
            .trust
            .put_cluster(&TrustedCluster {
                directory_id: "local".into(),
                cluster_id: id.into(),
                name: "store-north".into(),
                api,
                tls_sha256: mgmt.pin.clone(),
                jwks: json!({ "keys": [] }),
                // The second is trusted for another tenant only.
                tenants: vec![if id == CLUSTER {
                    DEFAULT_TENANT.into()
                } else {
                    "acme".into()
                }],
                created_at: now,
                ui_callback: None,
                report: None,
            })
            .unwrap();
    }
    let (alice, s) = c.session("alice", true);

    // An allowed call: forwarded with a bearer token for alice, aud the
    // cluster's id; the answer comes back, without the token.
    let (st, v, text) = c
        .relay(
            &s,
            CLUSTER,
            "hyperstor.v1.ClusterService/ListVolumes",
            json!({ "pool": 3 }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["volumes"][0]["name"], "web-root");
    let token = {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&seen[0].body).unwrap(),
            json!({ "pool": 3 })
        );
        assert_eq!(seen[0].headers["connect-protocol-version"], "1");
        seen[0].headers[header::AUTHORIZATION]
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap()
            .to_string()
    };
    assert!(!text.contains(&token), "the browser never sees the token");
    let h = jsonwebtoken::decode_header(&token).unwrap();
    assert_eq!(h.typ.as_deref(), Some(glidex_trust::tokens::ACCESS_TYP));
    let claims: Value = {
        use base64::Engine as _;
        let p = token.split('.').nth(1).unwrap();
        serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(p)
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(claims["aud"], CLUSTER);
    assert_eq!(claims["sub"], alice);
    assert_eq!(claims["iss"], format!("{BASE}/d/local"));
    assert!(claims["exp"].as_u64().unwrap() <= claims["iat"].as_u64().unwrap() + 15 * 60);

    // The same session's next call reuses the token.
    let (st, _, _) = c
        .relay(
            &s,
            CLUSTER,
            "hyperstor.v1.ClusterService/ListVolumes",
            json!({ "pool": 3 }),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[1].headers[header::AUTHORIZATION],
            format!("Bearer {token}")
        );
    }

    // The cluster's refusal comes back as it is; its 401 as 403, since a
    // 401 here would end the browser's session.
    let (st, v, _) = c
        .relay(&s, CLUSTER, "hyperstor.v1.S3Service/ListS3Keys", json!({}))
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(v["details"][0]["debug"]["reason"], "outside_realm");
    let (st, v, _) = c.relay(&s, CLUSTER, "auth/whoami", json!({})).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(v["message"], "directory_stale");
    assert_eq!(seen.lock().unwrap().last().unwrap().method, "GET");

    // Nothing else is forwarded.
    let before = seen.lock().unwrap().len();
    for call in [
        "hyperstor.v1.ClusterService/DeleteVolume",
        "hyperstor.v1.ClusterService/GetStatus",
        "auth/logout",
    ] {
        let (st, v, _) = c.relay(&s, CLUSTER, call, json!({})).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{call}");
        assert_eq!(v["error"], "not_relayed", "{call}");
    }
    assert_eq!(seen.lock().unwrap().len(), before);

    // A cluster the tenant doesn't use, an unknown one, someone not in the
    // tenant: refused before the cluster is called.
    for cluster in ["c-elsewhere", "c-unknown"] {
        let (st, v, _) = c.relay(&s, cluster, "auth/whoami", json!({})).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{cluster}: {v}");
    }
    let (_, bob) = c.session("bob", false);
    let (st, v, _) = c.relay(&bob, CLUSTER, "auth/whoami", json!({})).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(v["error"], "not_member");
    assert_eq!(seen.lock().unwrap().len(), before);

    // A cluster that doesn't answer: a stable error of this server's.
    drop(mgmt);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (st, v, _) = c.relay(&s, CLUSTER, "auth/whoami", json!({})).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY, "{v}");
    assert_eq!(v["error"], "cluster_unreachable");
}
