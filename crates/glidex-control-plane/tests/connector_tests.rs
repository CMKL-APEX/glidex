//! Directory connectors (spec/tenancy.md T1, §13–§14): the connector API
//! on REST, the Connect protocol and gRPC; who may manage them; secrets
//! that never come back out; the D22 rules; and logins through an API
//! connector (OIDC code and device flows against a mock IdP, PAM tokens).

mod common;

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
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const ORIGIN: &str = "https://localhost:5173";
const SECRET: &str = "corp-client-secret-0001";
const DEVICE_SECRET: &str = "corp-device-secret-0002";

struct H {
    router: Router,
    app: AppState,
    dir: TempDir,
}

fn harness_with(f: impl FnOnce(&mut Config)) -> H {
    let dir = TempDir::new().unwrap();
    // SAFETY: test-only; every test sets it the same way before reading it.
    unsafe { std::env::set_var("GLIDEX_RUN_DIR", dir.path().join("run")) };
    let manager = common::with_db_path(dir.path().join("t.db")).unwrap();
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
        let b = self.with_auth(Request::post(procedure(method)), who);
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
        let b = self.with_auth(Request::post(procedure(method)), who);
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

    /// A `google` connector (spec/tenancy.md §5.1) against the mock, with
    /// its client secret set; `google` adds to its settings, `extra` to the
    /// connector.
    async fn google_connector(
        &self,
        mock: &idp::Mock,
        id: &str,
        google: Value,
        extra: Value,
    ) -> (StatusCode, Value) {
        let s = self.session(&self.admin(&format!("admin-{id}")));
        let mut g = json!({"client_id": "glidex", "issuer": mock.base});
        for (k, v) in google.as_object().unwrap() {
            g[k] = v.clone();
        }
        let mut c = json!({"id": id, "display_name": "Google", "jit": true, "google": g});
        for (k, v) in extra.as_object().unwrap() {
            c[k] = v.clone();
        }
        let (st, v) = self
            .rest("POST", "/connectors", Some(json!({ "connector": c })), &s)
            .await;
        if st != StatusCode::CREATED {
            return (st, v);
        }
        let mut fields = vec!["client_secret"];
        if g.get("device_client_id").is_some() {
            fields.push("device_client_secret");
        }
        for f in fields {
            let value = if f == "client_secret" {
                SECRET
            } else {
                DEVICE_SECRET
            };
            let (st, r) = self
                .rest(
                    "PUT",
                    &format!("/connectors/{id}/secrets/{f}"),
                    Some(json!({ "value": value })),
                    &s,
                )
                .await;
            assert_eq!(st, StatusCode::OK, "{r}");
        }
        (st, v)
    }

    /// A browser login through `id` with `claims` (its nonce filled in).
    async fn login_with(&self, mock: &idp::Mock, id: &str, mut claims: Value) -> StatusCode {
        let (state, nonce, _, browser) = self.begin(id).await;
        claims["nonce"] = json!(nonce);
        *mock.id_token.lock().unwrap() = Some(idp::sign(claims, "k1"));
        self.finish(id, &state, &browser).await.0
    }

    /// Start a login the way an invitation page does (`POST
    /// /auth/c/{id}/start` with the invitation in the body); returns the
    /// status, and (state, nonce, browser cookie) when it started.
    async fn begin_invited(
        &self,
        id: &str,
        invitation: &str,
    ) -> (StatusCode, Option<(String, String, String)>) {
        let req = Request::post(format!("/auth/c/{id}/start"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"invitation": invitation, "return_to": "/"}).to_string(),
            ))
            .unwrap();
        let (s, v, headers) = self.send(req).await;
        if s != StatusCode::OK {
            return (s, None);
        }
        let loc = reqwest::Url::parse(v["redirect"].as_str().unwrap()).unwrap();
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
        (s, Some((q["state"].clone(), q["nonce"].clone(), browser)))
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

/// `Method` of ConnectorService, or a full `package.Service/Method`.
fn procedure(method: &str) -> String {
    if method.contains('/') {
        format!("/{method}")
    } else {
        format!("/glidex.directory.v1.ConnectorService/{method}")
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
    // `glidex.auth.v1.PollDeviceResponse`.
    assert_eq!(
        (st, v["pending"].as_bool()),
        (StatusCode::ACCEPTED, Some(true))
    );
    let (st, v) = h
        .rest("POST", "/auth/c/corp/device/poll", Some(poll), "")
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, who) = h
        .rest(
            "GET",
            "/auth/whoami",
            None,
            v["issued"]["token"].as_str().unwrap(),
        )
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
    // `glidex.auth.v1.PasswordTokenResponse`: an RFC 3339 expiry.
    let expires: buffa_types::google::protobuf::Timestamp =
        serde_json::from_value(v["issued"]["expires_at"].clone()).unwrap();
    let expires = expires.seconds as u64;
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

// ---- the close-out of T0/T1 -----------------------------------------------

use glidex_api::proto::glidex::auth::v1 as login;

#[tokio::test]
async fn login_service_over_connect_and_grpc() {
    let mock = idp::start().await;
    let dir = TempDir::new().unwrap();
    let sock = fake_authd(&dir);
    let h = harness_with(|c| c.auth.pam.authd_socket = sock.clone());
    h.oidc_connector(&mock, "corp", json!({})).await;

    // Discover: public, on both transports.
    let (st, v) = h
        .connect(
            "glidex.auth.v1.LoginService/Discover",
            json!({"hint": "default"}),
            "",
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["tenant"]["slug"], "default");
    assert!(
        v["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "corp"),
        "{v}"
    );
    let req = login::DiscoverRequest {
        hint: "nobody@nowhere".into(),
        ..Default::default()
    };
    let (st, bytes) = h
        .grpc("glidex.auth.v1.LoginService/Discover", &req, "")
        .await;
    assert_eq!(st, "0");
    let got = login::DiscoverResponse::decode_from_slice(&bytes).unwrap();
    assert!(
        got.tenant.as_option().is_none(),
        "an unknown hint names no tenant"
    );

    // The device grant through connector corp, over Connect.
    let start = json!({"connector": "corp"});
    let (st, v) = h
        .connect("glidex.auth.v1.LoginService/StartDevice", start, "")
        .await;
    assert_eq!(
        (st, v["user_code"].as_str()),
        (StatusCode::OK, Some("ABCD-EFGH")),
        "{v}"
    );
    let mut c = claims(&mock, "");
    c.as_object_mut().unwrap().remove("nonce");
    *mock.id_token.lock().unwrap() = Some(idp::sign(c, "k1"));
    let poll = json!({"connector": "corp", "device_code": "dev-1", "token_name": "rpc"});
    let (st, v) = h
        .connect("glidex.auth.v1.LoginService/PollDevice", poll.clone(), "")
        .await;
    assert_eq!(
        (st, v["pending"].as_bool()),
        (StatusCode::OK, Some(true)),
        "{v}"
    );
    let (st, v) = h
        .connect("glidex.auth.v1.LoginService/PollDevice", poll, "")
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let token = v["issued"]["token"].as_str().unwrap().to_string();
    assert_eq!(v["issued"]["display_name"], "Alice");
    let (_, who) = h.rest("GET", "/auth/whoami", None, &token).await;
    assert_eq!(who["token"]["name"], "rpc");

    // A password token over gRPC: the request's password is a secret
    // field, the answer carries the token once.
    let req = login::PasswordTokenRequest {
        username: "bob".into(),
        password: "bob-password".into(),
        days: Some(2),
        ..Default::default()
    };
    let (st, bytes) = h
        .grpc("glidex.auth.v1.LoginService/PasswordToken", &req, "")
        .await;
    assert_eq!(st, "0");
    let got = login::PasswordTokenResponse::decode_from_slice(&bytes).unwrap();
    let issued = got.issued.as_option().unwrap();
    assert!(issued.token.starts_with("gxt_"));
    let days =
        (issued.expires_at.as_option().unwrap().seconds as u64).saturating_sub(now()) / 86_400;
    assert!(days <= 2, "{days}");
    let bad = login::PasswordTokenRequest {
        username: "bob".into(),
        password: "wrong".into(),
        ..Default::default()
    };
    let (st, _) = h
        .grpc("glidex.auth.v1.LoginService/PasswordToken", &bad, "")
        .await;
    assert_eq!(st, "16", "unauthenticated");
    // The password never reaches the audit log.
    let entries = h.app.auth.store.audit(0, 1000, |_| true).unwrap();
    assert!(
        !serde_json::to_string(&entries)
            .unwrap()
            .contains("bob-password")
    );
}

#[tokio::test]
async fn mfa_logins_are_marked_and_policies_can_ask() {
    let mock = idp::start().await;
    let h = harness();
    h.oidc_connector(&mock, "corp", json!({})).await;
    for (amr, mfa) in [(json!(["pwd"]), false), (json!(["pwd", "mfa"]), true)] {
        let (state, nonce, _, browser) = h.begin("corp").await;
        let mut c = claims(&mock, &nonce);
        c["amr"] = amr;
        *mock.id_token.lock().unwrap() = Some(idp::sign(c, "k1"));
        let (st, headers) = h.finish("corp", &state, &browser).await;
        assert_eq!(st, StatusCode::SEE_OTHER);
        let cookie = format!("{}={}", auth::SESSION_COOKIE, session_of(&headers));
        let (_, who, _) = h
            .send(
                Request::get("/auth/whoami")
                    .header(header::COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(who["mfa"], mfa, "{who}");
        assert_eq!(who["directory_id"], "local");
    }
    // Site policies can require it (context.auth.mfa is in the schema).
    let admin = h.admin("ada");
    let text = "@id(\"site.mfa-for-host\")\nforbid (principal, action in Glidex::Action::\"host.cluster\", resource)\nunless { context.auth.mfa };";
    let (st, v) = h
        .rest(
            "POST",
            "/authz/validate",
            Some(json!({"id": "site.mfa-for-host", "text": text})),
            &h.token(&admin),
        )
        .await;
    assert_eq!(
        (st, v["valid"].as_bool()),
        (StatusCode::OK, Some(true)),
        "{v}"
    );
}

#[tokio::test]
async fn checks_say_when_a_fresh_login_would_allow() {
    let h = harness();
    let admin = h.admin("ada");
    let s = h.session(&admin);
    let check = json!({"checks": [{"action": "createConnector", "resource": {"type": "Directory", "id": "local"}}]});
    let (_, v) = h
        .rest("POST", "/authz/check", Some(check.clone()), &s)
        .await;
    assert_eq!(v["results"][0]["allowed"], true, "{v}");
    // Age the session past the step-up window.
    let cookie = s
        .strip_prefix("session:")
        .unwrap()
        .split_once('|')
        .unwrap()
        .0;
    let hash = auth::sha256_hex(cookie.as_bytes());
    let mut rec = h.app.auth.store.session(&hash).unwrap().unwrap();
    rec.authenticated_at -= 3600;
    h.app.auth.store.put_session(&hash, &rec).unwrap();
    let (_, v) = h
        .rest("POST", "/authz/check", Some(check.clone()), &s)
        .await;
    assert_eq!(
        (
            v["results"][0]["allowed"].as_bool(),
            v["results"][0]["reauth"].as_bool()
        ),
        (Some(false), Some(true)),
        "{v}"
    );
    let allowed = json!({"actions": ["createConnector", "listConnectors"], "resources": [{"type": "Directory", "id": "local"}]});
    let (_, v) = h
        .rest("POST", "/authz/allowed", Some(allowed.clone()), &s)
        .await;
    assert_eq!(v["allowed"][0]["actions"], json!(["listConnectors"]), "{v}");
    assert_eq!(v["reauth"][0]["actions"], json!(["createConnector"]), "{v}");
    // A token can't log in again: nothing to offer.
    let (_, v) = h
        .rest("POST", "/authz/check", Some(check), &h.token(&admin))
        .await;
    assert_eq!(v["results"][0]["reauth"], false, "{v}");
    // Nor for someone a fresh login wouldn't help.
    let bob = h.user("bob");
    let (_, v) = h
        .rest("POST", "/authz/allowed", Some(allowed), &h.session(&bob))
        .await;
    assert!(v["reauth"][0].get("actions").is_none(), "nothing: {v}");
}

#[tokio::test]
async fn a_peer_seen_first_joins_the_pam_connectors_tenant() {
    let me = nix::unistd::getuid().as_raw();
    let group = nix::unistd::Group::from_gid(nix::unistd::getgid())
        .unwrap()
        .unwrap()
        .name;
    let h = harness_with(|c| {
        c.auth.pam.enabled = false;
        c.users_group = group.clone();
        c.admin_group = "no-such-group-abc".into();
    });
    let s = h.session(&h.admin("ada"));
    let (st, t) = h
        .rest("POST", "/tenants", Some(json!({"slug": "lab"})), &s)
        .await;
    assert_eq!(st, StatusCode::CREATED, "{t}");
    let lab = t["tenant"]["id"].as_str().unwrap().to_string();
    let c = json!({"connector": {"id": "host", "jit": true, "default_tenant": "lab", "pam": {"allowed_groups": [group]}}});
    let (st, v) = h.rest("POST", "/connectors", Some(c), &s).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let req = Request::get("/auth/whoami")
        .extension(api::Listener::Api)
        .extension(api::PeerUid(me))
        .body(Body::empty())
        .unwrap();
    let (st, who, _) = h.send(req).await;
    assert_eq!(st, StatusCode::OK, "{who}");
    let user = who["user"]["id"].as_str().unwrap().to_string();
    assert!(h.app.auth.is_member(&Ent::User(user), &lab).unwrap());
}

#[tokio::test]
async fn a_standalone_host_rotates_its_secrets_key() {
    let mock = idp::start().await;
    let h = harness();
    h.oidc_connector(&mock, "corp", json!({})).await;
    let before = h.app.auth.connectors.kek_id();
    let s = h.session(&h.admin("root"));
    let (st, v) = h
        .rest(
            "POST",
            "/cluster/rotate-secrets-key",
            Some(json!({})),
            &h.token(&h.admin("tok")),
        )
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "a step-up action: {v}");
    let (st, v) = h
        .rest("POST", "/cluster/rotate-secrets-key", Some(json!({})), &s)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["rewrapped"], 1, "{v}");
    let after = h.app.auth.connectors.kek_id();
    assert_ne!(before, after);
    assert_eq!(v["kek_id"], after);
    assert!(h.dir.path().join("secrets.kek.prev").exists());
    // The stored secret still opens, and a login still sends it.
    let c = h.app.auth.connectors.get("corp").unwrap().unwrap();
    assert_eq!(
        h.app
            .auth
            .connectors
            .secret(&c, "client_secret")
            .unwrap()
            .unwrap()
            .as_str(),
        SECRET
    );
    let (state, nonce, _, browser) = h.begin("corp").await;
    *mock.id_token.lock().unwrap() = Some(idp::sign(claims(&mock, &nonce), "k1"));
    let (st, _) = h.finish("corp", &state, &browser).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert!(
        mock.token_requests
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains(&base64_basic())
    );
    // Twice in a row: the first rotation's key becomes the previous one.
    let (st, v) = h
        .rest("POST", "/cluster/rotate-secrets-key", Some(json!({})), &s)
        .await;
    assert_eq!(
        (st, v["rewrapped"].as_u64()),
        (StatusCode::OK, Some(1)),
        "{v}"
    );
    assert_eq!(
        h.app
            .auth
            .connectors
            .secret(&c, "client_secret")
            .unwrap()
            .unwrap()
            .as_str(),
        SECRET
    );
}

/// A Google ID token for `email` (verified), with `hd` when given.
fn google_claims(mock: &idp::Mock, sub: &str, email: &str, hd: Option<&str>) -> Value {
    let mut c = json!({
        "iss": mock.base, "aud": "glidex", "sub": sub, "exp": now() + 300, "iat": now(),
        "email": email, "email_verified": true, "name": sub,
    });
    if let Some(hd) = hd {
        c["hd"] = json!(hd);
    }
    c
}

#[tokio::test]
async fn google_workspace_logins_are_allowed_by_hd_not_by_the_email() {
    // spec/tenancy.md D39: anyone can open a consumer Google account under
    // a company's address; only `hd` says it is the company's Workspace.
    let mock = idp::start().await;
    let h = harness();
    let (st, v) = h
        .google_connector(&mock, "gw", json!({"domains": ["example.org"]}), json!({}))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["connector"]["kind"], "google");
    assert_eq!(
        v["connector"]["google"]["accounts"],
        "GOOGLE_ACCOUNTS_WORKSPACE"
    );
    // The login page offers it: SSO is on with a preset alone.
    let (_, m, _) = h
        .send(Request::get("/auth/methods").body(Body::empty()).unwrap())
        .await;
    assert_eq!(m["oidc"], true, "{m}");
    let ok = google_claims(&mock, "g-1", "alice@example.org", Some("example.org"));
    assert_eq!(
        h.login_with(&mock, "gw", ok.clone()).await,
        StatusCode::SEE_OTHER
    );
    let forged = google_claims(&mock, "g-2", "mallory@example.org", None);
    assert_eq!(
        h.login_with(&mock, "gw", forged).await,
        StatusCode::FORBIDDEN
    );
    let other = google_claims(&mock, "g-3", "eve@other.org", Some("other.org"));
    assert_eq!(
        h.login_with(&mock, "gw", other).await,
        StatusCode::FORBIDDEN
    );
    let mut unverified = ok.clone();
    unverified["email_verified"] = json!(false);
    assert_eq!(
        h.login_with(&mock, "gw", unverified).await,
        StatusCode::FORBIDDEN
    );

    // Google sends no amr: MFA is the operator's word on 2-step
    // verification, and amr in a token counts for nothing.
    let (st, v) = h
        .google_connector(
            &mock,
            "gw-mfa",
            json!({"domains": ["example.org"]}),
            json!({"require_mfa": true}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let mut with_amr = ok.clone();
    with_amr["amr"] = json!(["mfa"]);
    assert_eq!(
        h.login_with(&mock, "gw-mfa", with_amr).await,
        StatusCode::FORBIDDEN
    );
    let (st, v) = h
        .google_connector(
            &mock,
            "gw-2sv",
            json!({"domains": ["example.org"], "trust_2sv": true}),
            json!({"require_mfa": true}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(
        h.login_with(&mock, "gw-2sv", ok).await,
        StatusCode::SEE_OTHER
    );

    // No Workspace is a public mail domain.
    let (st, v) = h
        .google_connector(
            &mock,
            "gw-bad",
            json!({"domains": ["gmail.com"]}),
            json!({}),
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
}

#[tokio::test]
async fn google_consumer_accounts_are_for_people_the_directory_knows() {
    let mock = idp::start().await;
    let h = harness();
    // Anyone with a Google account would get in: refused.
    let any = json!({"accounts": "GOOGLE_ACCOUNTS_ANY"});
    let (st, v) = h
        .google_connector(&mock, "gc", any.clone(), json!({}))
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::UNPROCESSABLE_ENTITY, Some("invalid_connector")),
        "{v}"
    );
    let listed = json!({"accounts": "GOOGLE_ACCOUNTS_ANY", "allowed_emails": ["Bob@gmail.com"]});
    let (st, v) = h
        .google_connector(&mock, "gc", listed, json!({"default_tenant": "default"}))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let bob = google_claims(&mock, "c-bob", "bob@gmail.com", None);
    assert_eq!(
        h.login_with(&mock, "gc", bob.clone()).await,
        StatusCode::SEE_OTHER
    );
    let carol = google_claims(&mock, "c-carol", "carol@gmail.com", None);
    assert_eq!(
        h.login_with(&mock, "gc", carol).await,
        StatusCode::FORBIDDEN
    );
    let mut unverified = google_claims(&mock, "c-bob2", "bob@gmail.com", None);
    unverified["email_verified"] = json!(false);
    assert_eq!(
        h.login_with(&mock, "gc", unverified).await,
        StatusCode::FORBIDDEN
    );
    // Known already: bob logs in again.
    assert_eq!(h.login_with(&mock, "gc", bob).await, StatusCode::SEE_OTHER);
    // Without jit, nobody new gets an account (invitations will).
    let (st, v) = h
        .google_connector(&mock, "gc-closed", any, json!({"jit": false}))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let dave = google_claims(&mock, "c-dave", "dave@gmail.com", None);
    assert_eq!(
        h.login_with(&mock, "gc-closed", dave).await,
        StatusCode::FORBIDDEN
    );

    // D22: consumer accounts never join the owner or an operator, and a
    // Workspace there needs the operator's word on 2-step verification.
    let guarded = json!({"default_tenant": "admin", "jit": false});
    let (st, v) = h
        .google_connector(
            &mock,
            "gc-ops",
            json!({"accounts": "GOOGLE_ACCOUNTS_ANY"}),
            guarded.clone(),
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (st, v) = h
        .google_connector(
            &mock,
            "gw-ops",
            json!({"domains": ["example.org"]}),
            guarded.clone(),
        )
        .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    let (st, v) = h
        .google_connector(
            &mock,
            "gw-ops",
            json!({"domains": ["example.org"], "trust_2sv": true}),
            guarded,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["connector"]["require_mfa"], true, "{v}");
}

#[tokio::test]
async fn google_device_logins_use_the_device_client() {
    // Google takes the device grant only from a TVs and Limited Input
    // client, a client of its own.
    let mock = idp::start().await;
    let h = harness();
    let (st, v) = h
        .google_connector(&mock, "gw", json!({"domains": ["example.org"]}), json!({}))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let (st, v) = h.rest("POST", "/auth/c/gw/device", None, "").await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::BAD_REQUEST, Some("device_flow_unsupported")),
        "{v}"
    );
    let (st, v) = h
        .google_connector(
            &mock,
            "gw-tv",
            json!({"domains": ["example.org"], "device_client_id": "glidex-tv"}),
            json!({}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let (st, v) = h.rest("POST", "/auth/c/gw-tv/device", None, "").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let mut c = google_claims(&mock, "g-tv", "alice@example.org", Some("example.org"));
    c["aud"] = json!("glidex-tv");
    *mock.id_token.lock().unwrap() = Some(idp::sign(c, "k1"));
    let poll = json!({"device_code": "dev-1"});
    let (st, _) = h
        .rest("POST", "/auth/c/gw-tv/device/poll", Some(poll.clone()), "")
        .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    let (st, v) = h
        .rest("POST", "/auth/c/gw-tv/device/poll", Some(poll), "")
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    use base64::Engine as _;
    let tv = base64::engine::general_purpose::STANDARD.encode(format!("glidex-tv:{DEVICE_SECRET}"));
    assert!(
        mock.token_requests
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.starts_with(&format!("Basic {tv} "))),
        "every device token request used the device client"
    );
    // The connector test says gxctl logins work.
    let s = h.session(&h.admin("admin-gw-tv"));
    let (st, v) = h.rest("POST", "/connectors/gw-tv/test", None, &s).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let steps = v["steps"].as_array().unwrap();
    for name in [
        "client_secret",
        "device_client_secret",
        "discovery",
        "device grant",
    ] {
        assert!(
            steps.iter().any(|x| x["name"] == name && x["ok"] == true),
            "{name}: {v}"
        );
    }
}

/// `<id>.<secret>` from an invitation link (`…/invite/<id>#<secret>`).
fn invite_token(link: &str) -> String {
    let (path, secret) = link.split_once('#').unwrap();
    format!("{}.{}", path.rsplit('/').next().unwrap(), secret)
}

#[tokio::test]
async fn invitations_let_one_known_person_in_once() {
    // spec/tenancy.md D40, §6.1: a gated connector (consumer Google, jit
    // off) lets in whoever takes an invitation for their verified address.
    let mock = idp::start().await;
    let h = harness();
    let (st, v) = h
        .google_connector(
            &mock,
            "gc",
            json!({"accounts": "GOOGLE_ACCOUNTS_ANY"}),
            json!({"jit": false}),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let s = h.session(&h.admin("ada"));
    let invite = |email: &str, role: &str| json!({"email": email, "role": role, "connector": "gc"});
    let (st, v) = h
        .rest(
            "POST",
            "/tenants/default/invitations",
            Some(invite("Dave@gmail.com", "role.tenant-admin")),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let link = v["link"].as_str().unwrap().to_string();
    assert!(link.contains("/invite/") && link.contains('#'), "{link}");
    let token = invite_token(&link);
    let secret = token.split_once('.').unwrap().1.to_string();
    assert_eq!(v["invitation"]["email"], "dave@gmail.com");
    assert_eq!(v["invitation"]["state"], "INVITATION_STATE_OPEN");
    // Listed, never with its secret.
    let (st, v) = h
        .rest("GET", "/tenants/default/invitations", None, &s)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["invitations"].as_array().unwrap().len(), 1, "{v}");
    assert!(!v.to_string().contains(&secret), "{v}");

    // The page learns what it's for only with the secret.
    let (st, v) = h
        .rest(
            "POST",
            "/auth/invitation",
            Some(json!({"invitation": token})),
            "",
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        (v["tenant_slug"].as_str(), v["connector_id"].as_str()),
        (Some("default"), Some("gc"))
    );
    let id = token.split_once('.').unwrap().0;
    let (st, _) = h
        .rest(
            "POST",
            "/auth/invitation",
            Some(json!({"invitation": format!("{id}.wrong")})),
            "",
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // Not through another connector.
    h.oidc_connector(&mock, "corp", json!({})).await;
    assert_eq!(
        h.begin_invited("corp", &token).await.0,
        StatusCode::NOT_FOUND
    );

    // Without the invitation, a gated connector lets nobody new in.
    let dave = google_claims(&mock, "c-dave", "dave@gmail.com", None);
    assert_eq!(
        h.login_with(&mock, "gc", dave.clone()).await,
        StatusCode::FORBIDDEN
    );
    // Another address can't take it.
    let (st, started) = h.begin_invited("gc", &token).await;
    assert_eq!(st, StatusCode::OK);
    let (state, nonce, browser) = started.unwrap();
    let mut eve = google_claims(&mock, "c-eve", "eve@gmail.com", None);
    eve["nonce"] = json!(nonce);
    *mock.id_token.lock().unwrap() = Some(idp::sign(eve, "k1"));
    assert_eq!(
        h.finish("gc", &state, &browser).await.0,
        StatusCode::FORBIDDEN
    );
    // Nor an unverified one.
    let (_, started) = h.begin_invited("gc", &token).await;
    let (state, nonce, browser) = started.unwrap();
    let mut unverified = dave.clone();
    unverified["nonce"] = json!(nonce);
    unverified["email_verified"] = json!(false);
    *mock.id_token.lock().unwrap() = Some(idp::sign(unverified, "k1"));
    assert_eq!(
        h.finish("gc", &state, &browser).await.0,
        StatusCode::FORBIDDEN
    );

    // Dave takes it: a user, a member of the tenant with its role.
    let (_, started) = h.begin_invited("gc", &token).await;
    let (state, nonce, browser) = started.unwrap();
    let mut c = dave.clone();
    c["nonce"] = json!(nonce);
    *mock.id_token.lock().unwrap() = Some(idp::sign(c, "k1"));
    let (st, headers) = h.finish("gc", &state, &browser).await;
    assert_eq!(st, StatusCode::SEE_OTHER);
    assert!(
        headers
            .get_all(header::SET_COOKIE)
            .iter()
            .any(|c| c.to_str().unwrap().starts_with("gx_session="))
    );
    let (_, b) = h.rest("GET", "/tenants/default/bindings", None, &s).await;
    assert!(
        b["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["role"] == "role.tenant-admin" && x["principal_name"] == "c-dave"),
        "{b}"
    );
    let (_, m) = h.rest("GET", "/tenants/default/members", None, &s).await;
    assert!(m.to_string().contains("c-dave"), "{m}");
    // Once: the invitation is taken, and Dave logs in on his own now.
    assert_eq!(h.begin_invited("gc", &token).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.login_with(&mock, "gc", dave).await, StatusCode::SEE_OTHER);
    let (_, v) = h
        .rest("GET", "/tenants/default/invitations", None, &s)
        .await;
    assert_eq!(
        v["invitations"][0]["state"], "INVITATION_STATE_TAKEN",
        "{v}"
    );

    // Revoked ones are gone.
    let (_, v) = h
        .rest(
            "POST",
            "/tenants/default/invitations",
            Some(invite("erin@gmail.com", "member")),
            &s,
        )
        .await;
    let erin = invite_token(v["link"].as_str().unwrap());
    let erin_id = v["invitation"]["id"].as_str().unwrap().to_string();
    let (st, _) = h
        .rest("DELETE", &format!("/invitations/{erin_id}"), None, &s)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(h.begin_invited("gc", &erin).await.0, StatusCode::NOT_FOUND);

    // Bad requests.
    for (body, why) in [
        (
            invite("erin@gmail.com", "role.system-admin"),
            "not a tenant role",
        ),
        (invite("not-an-email", "member"), "an email address"),
        (
            json!({"email": "x@gmail.com", "role": "member", "connector": "nope"}),
            "a connector",
        ),
        (
            json!({"email": "x@gmail.com", "role": "member", "connector": "gc", "days": 31}),
            "at most 30 days",
        ),
    ] {
        let (st, v) = h
            .rest("POST", "/tenants/default/invitations", Some(body), &s)
            .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{why}: {v}");
    }
    // D22: into the owner's tenant only through a connector it allows
    // there, requiring MFA.
    let (st, v) = h
        .rest(
            "POST",
            "/tenants/admin/invitations",
            Some(invite("x@gmail.com", "member")),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    // Who may invite: not a member without tenant rights.
    let nobody = h.session(&h.user("nobody"));
    let (st, _) = h
        .rest(
            "POST",
            "/tenants/default/invitations",
            Some(invite("y@gmail.com", "member")),
            &nobody,
        )
        .await;
    assert!(
        st == StatusCode::FORBIDDEN || st == StatusCode::NOT_FOUND,
        "{st}"
    );
}

#[tokio::test]
async fn jumpcloud_groups_come_in_its_claim_and_its_emails_are_the_organizations() {
    // spec/tenancy.md §5.1: JumpCloud's issuer and scopes are fixed; groups
    // come in the claim its "Include group attribute" names (memberOf);
    // an email the token says nothing about counts as verified, one it
    // says is unverified doesn't.
    let mock = idp::start().await;
    let h = harness();
    let s = h.session(&h.admin("admin-jc"));
    let c = json!({"connector": {"id": "jc", "display_name": "JumpCloud", "jit": true,
        "required_groups": ["lab-staff"],
        "jumpcloud": {"client_id": "glidex", "issuer": mock.base, "allowed_emails": ["alice@corp.example"]}}});
    let (st, v) = h.rest("POST", "/connectors", Some(c), &s).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["connector"]["kind"], "jumpcloud");
    let (st, r) = h
        .rest(
            "PUT",
            "/connectors/jc/secrets/client_secret",
            Some(json!({"value": SECRET})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let base = |sub: &str, email: &str| {
        json!({"iss": mock.base, "aud": "glidex", "sub": sub, "exp": now() + 300, "iat": now(),
               "email": email, "name": sub, "memberOf": ["lab-staff"]})
    };
    assert_eq!(
        h.login_with(&mock, "jc", base("j-1", "alice@corp.example"))
            .await,
        StatusCode::SEE_OTHER
    );
    let mut no_group = base("j-2", "alice@corp.example");
    no_group["memberOf"] = json!(["other"]);
    assert_eq!(
        h.login_with(&mock, "jc", no_group).await,
        StatusCode::FORBIDDEN
    );
    // Not on the allowlist.
    assert_eq!(
        h.login_with(&mock, "jc", base("j-3", "bob@corp.example"))
            .await,
        StatusCode::FORBIDDEN
    );
    let mut unverified = base("j-4", "alice@corp.example");
    unverified["email_verified"] = json!(false);
    assert_eq!(
        h.login_with(&mock, "jc", unverified).await,
        StatusCode::FORBIDDEN
    );
    // The issuer is JumpCloud's, or a local mock.
    let bad = json!({"connector": {"id": "jc2", "jumpcloud": {"client_id": "glidex", "issuer": "https://evil.example"}}});
    let (st, v) = h.rest("POST", "/connectors", Some(bad), &s).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
}
