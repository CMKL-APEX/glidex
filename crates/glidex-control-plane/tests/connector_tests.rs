//! Directory connectors (spec/tenancy.md T1, §13–§14): the connector API
//! on REST, the Connect protocol and gRPC; who may manage them; secrets
//! that never come back out; the D22 rules; and logins through an API
//! connector (OIDC code and device flows against a mock IdP, PAM tokens).

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use buffa::Message;
use glidex_api::proto::glidex::directory::v1 as pb;
use glidex_control_plane::api::{self, App, AppState};
use glidex_control_plane::auth::{self, AuthService, store::TokenKind};
use glidex_control_plane::authz::Ent;
use glidex_control_plane::config::Config;
use glidex_control_plane::state::VmManager;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const ORIGIN: &str = "https://localhost:5173";
const SECRET: &str = "corp-client-secret-0001";

struct H {
    router: Router,
    app: AppState,
    dir: TempDir,
}

fn harness_with(f: impl FnOnce(&mut Config)) -> H {
    let dir = TempDir::new().unwrap();
    // SAFETY: test-only; every test sets it the same way before reading it.
    unsafe { std::env::set_var("GLIDEX_RUN_DIR", dir.path().join("run")) };
    let manager = VmManager::with_db_path(dir.path().join("t.db")).unwrap();
    let mut cfg = Config::default();
    cfg.authz.policy_files_dir = dir.path().join("policies");
    cfg.auth.allowed_origins = vec![ORIGIN.into()];
    cfg.auth.secrets_key = Some(dir.path().join("secrets.kek"));
    f(&mut cfg);
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    let app = Arc::new(App { manager, auth });
    H {
        router: api::router(app.clone()),
        app,
        dir,
    }
}

fn harness() -> H {
    harness_with(|_| {})
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

    /// A fresh browser session (`session:cookie|csrf`), for step-up actions.
    fn session(&self, user: &str) -> String {
        let u = self.app.auth.store.user(user).unwrap().unwrap();
        let (cookie, csrf) = self.app.auth.create_session(&u, auth::Method::Pam).unwrap();
        format!("session:{cookie}|{csrf}")
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
        id
    }

    fn with_auth(
        &self,
        b: axum::http::request::Builder,
        who: &str,
    ) -> axum::http::request::Builder {
        if who.is_empty() {
            return b;
        }
        match who.strip_prefix("session:").and_then(|s| s.split_once('|')) {
            Some((cookie, csrf)) => b
                .header(
                    header::COOKIE,
                    format!("{}={}", auth::SESSION_COOKIE, cookie),
                )
                .header(auth::CSRF_HEADER, csrf)
                .header(header::ORIGIN, ORIGIN),
            None => b.header(header::AUTHORIZATION, format!("Bearer {who}")),
        }
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, Value, axum::http::HeaderMap) {
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            headers,
        )
    }

    async fn rest(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        who: &str,
    ) -> (StatusCode, Value) {
        let b = self.with_auth(Request::builder().method(method).uri(path), who);
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let (s, v, _) = self.send(req).await;
        (s, v)
    }

    /// The Connect protocol with JSON.
    async fn connect(&self, method: &str, body: Value, who: &str) -> (StatusCode, Value) {
        let b = self.with_auth(
            Request::post(format!("/glidex.directory.v1.ConnectorService/{method}")),
            who,
        );
        let (s, v, _) = self
            .send(
                b.header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await;
        (s, v)
    }

    /// gRPC with a binary message; returns grpc-status and the reply.
    async fn grpc<M: Message>(&self, method: &str, msg: &M, who: &str) -> (String, Vec<u8>) {
        let bytes = msg.encode_to_vec();
        let mut body = vec![0u8];
        body.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        body.extend_from_slice(&bytes);
        let b = self.with_auth(
            Request::post(format!("/glidex.directory.v1.ConnectorService/{method}")),
            who,
        );
        let resp = self
            .router
            .clone()
            .oneshot(
                b.header(header::CONTENT_TYPE, "application/grpc")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
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

    /// An OIDC connector `id` on `mock`, with its client secret set.
    async fn oidc_connector(&self, mock: &idp::Mock, id: &str, extra: Value) -> Value {
        let admin = self.admin(&format!("admin-{id}"));
        let s = self.session(&admin);
        let mut c = json!({
            "id": id, "display_name": "Corp SSO", "jit": true,
            "group_teams": {"lab-staff": "lab"},
            "oidc": {"issuer": mock.base, "client_id": "glidex"},
        });
        for (k, v) in extra.as_object().unwrap() {
            c[k] = v.clone();
        }
        let (st, v) = self
            .rest("POST", "/connectors", Some(json!({ "connector": c })), &s)
            .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        let (st, v) = self
            .rest(
                "PUT",
                &format!("/connectors/{id}/secrets/client_secret"),
                Some(json!({ "value": SECRET })),
                &s,
            )
            .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        v
    }

    /// Start a browser login through connector `id`; returns (state, nonce,
    /// redirect_uri, browser cookie).
    async fn begin(&self, id: &str) -> (String, String, String, String) {
        let (s, _, headers) = self
            .send(
                Request::get(format!("/auth/c/{id}/start?return_to=/vms"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        let loc =
            reqwest::Url::parse(headers.get(header::LOCATION).unwrap().to_str().unwrap()).unwrap();
        let q: std::collections::HashMap<_, _> = loc.query_pairs().into_owned().collect();
        let browser = headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        (
            q["state"].clone(),
            q["nonce"].clone(),
            q["redirect_uri"].clone(),
            browser,
        )
    }

    async fn finish(
        &self,
        id: &str,
        state: &str,
        browser: &str,
    ) -> (StatusCode, axum::http::HeaderMap) {
        let req = Request::get(format!("/auth/c/{id}/callback?code=c1&state={state}"))
            .header(header::COOKIE, browser)
            .body(Body::empty())
            .unwrap();
        let (s, _, h) = self.send(req).await;
        (s, h)
    }
}

fn session_of(headers: &axum::http::HeaderMap) -> String {
    let c = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|c| c.starts_with("gx_session="))
        .unwrap();
    c.split(';')
        .next()
        .unwrap()
        .split_once('=')
        .unwrap()
        .1
        .to_string()
}

mod pam {
    use glidex_authd::authenticator::{Account, Accounts, AuthFailure, Authenticator};
    use std::collections::HashMap;

    pub struct FakePam;
    impl Authenticator for FakePam {
        fn authenticate(
            &self,
            _service: &str,
            user: &str,
            password: &str,
        ) -> Result<(), AuthFailure> {
            if password == format!("{}-password", user) {
                Ok(())
            } else {
                Err(AuthFailure::Denied("bad".into()))
            }
        }
    }

    pub struct FakeAccounts(pub HashMap<String, Account>);
    impl Accounts for FakeAccounts {
        fn lookup(&self, user: &str) -> Option<Account> {
            self.0.get(user).cloned()
        }
    }
}

fn fake_authd(dir: &TempDir) -> std::path::PathBuf {
    use glidex_authd::authenticator::Account;
    let sock = dir.path().join("auth.sock");
    let accounts = [(
        "bob".to_string(),
        Account {
            uid: 1,
            groups: vec!["glidex-users".into()],
        },
    )]
    .into_iter()
    .collect();
    let cfg = glidex_authd::config::Config {
        failure_delay: std::time::Duration::from_millis(1),
        ..Default::default()
    };
    let authd = Arc::new(glidex_authd::server::Authd::new(
        cfg,
        Some(nix::unistd::getuid().as_raw()),
        Arc::new(pam::FakePam),
        Arc::new(pam::FakeAccounts(accounts)),
    ));
    let l = glidex_authd::server::bind(&sock, 0o600, None).unwrap();
    std::thread::spawn(move || authd.serve(l));
    sock
}

mod idp {
    use axum::{Json, Router, extract::State, routing::get, routing::post};
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
                pem: k
                    .to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)
                    .unwrap()
                    .to_string(),
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
        jsonwebtoken::encode(
            &h,
            &claims,
            &EncodingKey::from_rsa_pem(key().pem.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    pub async fn start() -> Arc<Mock> {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let mock = Arc::new(Mock {
            base: base.clone(),
            ..Default::default()
        });
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn claims(mock: &idp::Mock, nonce: &str) -> Value {
    json!({
        "iss": mock.base, "aud": "glidex", "sub": "user-123", "exp": now() + 300, "iat": now(),
        "nonce": nonce, "email": "alice@example.org", "name": "Alice", "groups": ["lab-staff"],
    })
}

#[tokio::test]
async fn connectors_are_managed_over_rest_connect_and_grpc() {
    let mock = idp::start().await;
    let h = harness();
    let admin = h.admin("ada");
    let s = h.session(&admin);
    let t = h.token(&admin);

    // A personal token is an old login: writes are step-up actions.
    let body = json!({"connector": {"id": "corp", "display_name": "Corp", "oidc": {"issuer": mock.base, "client_id": "glidex"}}});
    let (st, v) = h.rest("POST", "/connectors", Some(body.clone()), &t).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("reauth_required")),
        "{v}"
    );
    let (st, v) = h.rest("POST", "/connectors", Some(body.clone()), &s).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let c = &v["connector"];
    assert_eq!(
        (
            c["kind"].as_str(),
            c["source"].as_str(),
            c["directory_id"].as_str()
        ),
        (Some("oidc"), Some("api"), Some("local"))
    );
    assert_eq!(c["secrets"][0]["field"], "client_secret");
    assert_ne!(c["secrets"][0]["set"], json!(true));
    let (st, _) = h.rest("POST", "/connectors", Some(body), &s).await;
    assert_eq!(st, StatusCode::CONFLICT, "ids are unique");

    // Setting the secret answers with its status, never its value.
    let (st, v) = h
        .rest(
            "PUT",
            "/connectors/corp/secrets/client_secret",
            Some(json!({"value": SECRET})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!v.to_string().contains(SECRET));
    let (st, _) = h
        .rest(
            "PUT",
            "/connectors/corp/secrets/bogus",
            Some(json!({"value": "x"})),
            &s,
        )
        .await;
    assert!(st.is_client_error(), "only the kind's secret fields");

    // Connect: list and read.
    let (st, v) = h.connect("ListConnectors", json!({}), &t).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let ids: Vec<_> = v["connectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&"corp".to_string()), "{ids:?}");
    let (st, v) = h.connect("GetConnector", json!({"id": "corp"}), &t).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["connector"]["secrets"][0]["set"], true);
    assert!(!v.to_string().contains(SECRET));

    // gRPC: read, and a write refused for want of step-up.
    let (st, bytes) = h
        .grpc(
            "GetConnector",
            &pb::GetConnectorRequest {
                id: "corp".into(),
                ..Default::default()
            },
            &t,
        )
        .await;
    assert_eq!(st, "0");
    let got = pb::GetConnectorResponse::decode_from_slice(&bytes).unwrap();
    assert_eq!(got.connector.as_option().unwrap().display_name, "Corp");
    let set = pb::SetSecretRequest {
        id: "corp".into(),
        field: "client_secret".into(),
        value: "other".into(),
        ..Default::default()
    };
    let (st, _) = h.grpc("SetSecret", &set, &t).await;
    assert_eq!(st, "16", "unauthenticated: it needs a recent login");

    // PATCH changes only what it names.
    let (st, v) = h
        .rest(
            "PATCH",
            "/connectors/corp",
            Some(json!({"connector": {"display_name": "Corp SSO"}})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["connector"]["display_name"], "Corp SSO");
    assert_eq!(v["connector"]["oidc"]["client_id"], "glidex");

    // The test endpoint walks the connector against its IdP.
    let (st, v) = h.rest("POST", "/connectors/corp/test", None, &t).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    for step in v["steps"].as_array().unwrap() {
        assert_eq!(step["ok"], true, "{step}");
    }

    // The secret is stored sealed: neither the database nor the audit log holds it.
    let entries = h.app.auth.store.audit(0, 1000, |_| true).unwrap();
    assert!(
        entries.iter().any(|e| e.action == "setConnectorSecret"),
        "audited"
    );
    assert!(!serde_json::to_string(&entries).unwrap().contains(SECRET));
    let db = std::fs::read(h.dir.path().join("t.db")).unwrap();
    assert!(!db.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()));

    // The file connectors are read-only here.
    let (st, v) = h.rest("DELETE", "/connectors/file-pam", None, &s).await;
    assert!(
        st == StatusCode::CONFLICT
            || st == StatusCode::NOT_FOUND
            || st == StatusCode::UNPROCESSABLE_ENTITY,
        "{st} {v}"
    );

    let (st, _) = h.rest("DELETE", "/connectors/corp", None, &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = h.rest("GET", "/connectors/corp", None, &t).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn only_directory_admins_manage_connectors() {
    let h = harness();
    let body = json!({"connector": {"id": "lab", "pam": {"allowed_groups": ["lab"]}}});
    // A user, and a tenant admin, may not.
    let bob = h.user("bob");
    let (st, _) = h.rest("GET", "/connectors", None, &h.token(&bob)).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    h.app
        .auth
        .add_link(
            "role.tenant-admin",
            Ent::User(bob.clone()),
            Ent::Tenant("default".into()),
            "test",
        )
        .unwrap();
    let (st, _) = h
        .rest("POST", "/connectors", Some(body.clone()), &h.session(&bob))
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Being in the owner tenant isn't enough either (D7 needs both)…
    h.app
        .auth
        .tenants
        .add_member("admin", Ent::User(bob.clone()), "test")
        .unwrap();
    let (st, _) = h
        .rest("POST", "/connectors", Some(body.clone()), &h.session(&bob))
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // …nor the role outside it.
    let dora = h.user("dora");
    h.app
        .auth
        .add_link(
            "role.directory-admin",
            Ent::User(dora.clone()),
            Ent::Directory("local".into()),
            "test",
        )
        .unwrap();
    let (st, _) = h
        .rest("POST", "/connectors", Some(body.clone()), &h.session(&dora))
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // A directory admin in the owner tenant may.
    h.app
        .auth
        .tenants
        .add_member("admin", Ent::User(dora.clone()), "test")
        .unwrap();
    let (st, v) = h
        .rest("POST", "/connectors", Some(body), &h.session(&dora))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let (st, v) = h.rest("GET", "/connectors", None, &h.token(&dora)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // Unauthenticated: nothing.
    let (st, _) = h.rest("GET", "/connectors", None, "").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn guarded_connectors_need_groups_and_default_to_mfa() {
    // D22: a connector whose new users join the owner tenant (or an
    // operator) can't let anyone in, and asks for MFA unless told not to.
    let h = harness();
    let s = h.session(&h.admin("ada"));
    let oidc = json!({"issuer": "https://idp.example", "client_id": "glidex"});
    let (st, v) = h.rest("POST", "/connectors", Some(json!({"connector": {"id": "ops", "jit": true, "default_tenant": "admin", "oidc": oidc}})), &s).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let c = json!({"connector": {"id": "ops", "jit": true, "default_tenant": "admin", "required_groups": ["ops"], "oidc": oidc}});
    let (st, v) = h.rest("POST", "/connectors", Some(c), &s).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["connector"]["require_mfa"], true);
    // A user tenant has no such rule.
    let (st, v) = h.rest("POST", "/connectors", Some(json!({"connector": {"id": "users", "jit": true, "default_tenant": "default", "oidc": oidc}})), &s).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_ne!(v["connector"]["require_mfa"], true);
    // Nor may a PATCH remove the groups from a guarded one.
    let (st, v) = h
        .rest(
            "PATCH",
            "/connectors/ops",
            Some(json!({"connector": {"required_groups": []}})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    // Names: `file-` is reserved, and an unknown tenant is refused.
    let (st, _) = h
        .rest(
            "POST",
            "/connectors",
            Some(json!({"connector": {"id": "file-x", "oidc": oidc}})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    let (st, _) = h
        .rest(
            "POST",
            "/connectors",
            Some(json!({"connector": {"id": "x", "default_tenant": "nope", "oidc": oidc}})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn oidc_login_through_an_api_connector() {
    let mock = idp::start().await;
    let h = harness();
    h.app
        .auth
        .store
        .put_team(&auth::store::Team {
            id: "team-lab".into(),
            name: "lab".into(),
            members: vec![],
            created_at: 0,
        })
        .unwrap();
    h.oidc_connector(&mock, "corp", json!({"default_tenant": "default"}))
        .await;

    let (_, v, _) = h
        .send(Request::get("/auth/methods").body(Body::empty()).unwrap())
        .await;
    assert_eq!(v["oidc"], true);
    assert!(
        v["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "corp" && c["kind"] == "oidc"),
        "{v}"
    );
    let (_, v, _) = h
        .send(
            Request::post("/auth/discover")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"hint":"default"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(v["tenant"]["slug"], "default", "{v}");

    let (state, nonce, redirect, browser) = h.begin("corp").await;
    assert!(
        redirect.ends_with("/api/auth/c/corp/callback"),
        "{redirect}"
    );
    *mock.id_token.lock().unwrap() = Some(idp::sign(claims(&mock, &nonce), "k1"));
    let (st, headers) = h.finish("corp", &state, &browser).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert_eq!(headers.get(header::LOCATION).unwrap(), "/vms");

    // The token request carried the stored client secret.
    use base64::Engine as _;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("glidex:{SECRET}"));
    let req = mock.token_requests.lock().unwrap().last().cloned().unwrap();
    assert!(req.starts_with(&format!("Basic {basic} ")), "{req}");

    // A new user, in the connector's team and default tenant.
    let session = session_of(&headers);
    let (_, who, _) = h
        .send(
            Request::get("/auth/whoami")
                .header(
                    header::COOKIE,
                    format!("{}={}", auth::SESSION_COOKIE, session),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(who["user"]["display_name"], "Alice");
    assert_eq!(who["teams"], json!(["team-lab"]));
    let user = who["user"]["id"].as_str().unwrap().to_string();
    assert!(
        h.app
            .auth
            .is_member(&Ent::User(user.clone()), "default")
            .unwrap()
    );
    assert!(!h.app.auth.is_member(&Ent::User(user), "admin").unwrap());
    assert!(
        h.app
            .auth
            .store
            .identity("conn:corp", "user-123")
            .unwrap()
            .is_some(),
        "keyed by connector"
    );

    // A state from one connector isn't good on another.
    h.oidc_connector(&mock, "other", json!({})).await;
    let (state, nonce, _, browser) = h.begin("corp").await;
    *mock.id_token.lock().unwrap() = Some(idp::sign(claims(&mock, &nonce), "k1"));
    let (st, _) = h.finish("other", &state, &browser).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // A disabled connector is gone from the login page and its routes.
    let s = h.session(&h.admin("root"));
    let (st, _) = h
        .rest(
            "PATCH",
            "/connectors/corp",
            Some(json!({"connector": {"enabled": false}})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let (_, v, _) = h
        .send(Request::get("/auth/methods").body(Body::empty()).unwrap())
        .await;
    assert!(
        !v["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "corp"),
        "{v}"
    );
    let (st, _, _) = h
        .send(
            Request::get("/auth/c/corp/start")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn require_mfa_checks_amr_and_acr() {
    let mock = idp::start().await;
    let h = harness();
    h.oidc_connector(&mock, "corp", json!({"require_mfa": true, "oidc": {"issuer": mock.base, "client_id": "glidex", "mfa_acr_values": ["urn:corp:mfa"]}})).await;
    let mut cases = vec![(claims(&mock, ""), StatusCode::FORBIDDEN)];
    let mut c = claims(&mock, "");
    c["amr"] = json!(["pwd", "mfa"]);
    cases.push((c, StatusCode::SEE_OTHER));
    let mut c = claims(&mock, "");
    c["acr"] = json!("urn:corp:mfa");
    cases.push((c, StatusCode::SEE_OTHER));
    let mut c = claims(&mock, "");
    c["acr"] = json!("urn:corp:password");
    cases.push((c, StatusCode::FORBIDDEN));
    for (mut c, want) in cases {
        let (state, nonce, _, browser) = h.begin("corp").await;
        c["nonce"] = json!(nonce);
        *mock.id_token.lock().unwrap() = Some(idp::sign(c.clone(), "k1"));
        let (st, _) = h.finish("corp", &state, &browser).await;
        assert_eq!(st, want, "{c}");
    }
}

#[tokio::test]
async fn device_login_through_an_api_connector() {
    let mock = idp::start().await;
    let h = harness();
    h.oidc_connector(&mock, "corp", json!({})).await;
    let (st, v) = h.rest("POST", "/auth/c/corp/device", None, "").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["user_code"], "ABCD-EFGH");
    let mut c = claims(&mock, "");
    c.as_object_mut().unwrap().remove("nonce");
    *mock.id_token.lock().unwrap() = Some(idp::sign(c, "k1"));
    let poll = json!({"device_code": "dev-1"});
    let (st, v) = h
        .rest("POST", "/auth/c/corp/device/poll", Some(poll.clone()), "")
        .await;
    assert_eq!(
        (st, v["status"].as_str()),
        (StatusCode::ACCEPTED, Some("pending"))
    );
    let (st, v) = h
        .rest("POST", "/auth/c/corp/device/poll", Some(poll), "")
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, who) = h
        .rest("GET", "/auth/whoami", None, v["token"].as_str().unwrap())
        .await;
    assert_eq!(who["user"]["display_name"], "Alice");
    assert!(
        mock.token_requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.contains(&base64_basic())),
        "every token request used the stored secret"
    );
}

fn base64_basic() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(format!("glidex:{SECRET}"))
}

#[tokio::test]
async fn pam_connector_tokens_are_clamped_to_its_maximum() {
    let dir = TempDir::new().unwrap();
    let sock = fake_authd(&dir);
    let h = harness_with(|c| c.auth.pam.authd_socket = sock.clone());
    let s = h.session(&h.admin("ada"));
    let c = json!({"connector": {"id": "lab", "jit": true, "token_max_days": 1, "pam": {"allowed_groups": ["glidex-users"]}}});
    let (st, v) = h.rest("POST", "/connectors", Some(c), &s).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let body =
        json!({"username": "bob", "password": "bob-password", "days": 30, "connector": "lab"});
    let (st, v) = h.rest("POST", "/auth/token", Some(body), "").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let expires = v["expires_at"].as_u64().unwrap();
    assert!(
        expires <= now() + 86_400 + 60,
        "clamped to a day: {expires}"
    );
    // The test endpoint reaches authd.
    let (st, v) = h.rest("POST", "/connectors/lab/test", None, &s).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(
        v["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["ok"] == true),
        "{v}"
    );
    // A group the account isn't in keeps it out.
    let (st, _) = h
        .rest(
            "PATCH",
            "/connectors/lab",
            Some(json!({"connector": {"pam": {"allowed_groups": ["wheel"]}}})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let body = json!({"username": "bob", "password": "bob-password", "connector": "lab"});
    let (st, _) = h.rest("POST", "/auth/token", Some(body), "").await;
    assert!(
        st == StatusCode::UNAUTHORIZED || st == StatusCode::FORBIDDEN,
        "{st}"
    );
}
