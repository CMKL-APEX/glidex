//! RPCs next to REST (spec/grpc.md G0): the System service over the
//! Connect protocol, gRPC and gRPC-Web, through the same router, layers
//! and service functions as REST, with the per-listener protocol and
//! credential rules of §6 and the error mapping of §7.

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
};
use base64::Engine;
use buffa::Message;
use glidex_api::proto::glidex::system::v1::{GetServerInfoResponse, WhoAmIResponse};
use glidex_api::proto::google::rpc::ErrorInfo;
use glidex_control_plane::api::{self, App, AppState, Listener, PeerUid};
use glidex_control_plane::auth::{self, AuthService, Method, store::TokenKind};
use glidex_control_plane::config::Config;
use glidex_control_plane::state::VmManager;
use http_body_util::BodyExt;
use serde_json::Value;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const ORIGIN: &str = "https://localhost:5173";
const SERVER_INFO: &str = "/glidex.system.v1.SystemService/GetServerInfo";
const WHOAMI: &str = "/glidex.system.v1.SystemService/WhoAmI";

struct H {
    router: Router,
    app: AppState,
    _dir: TempDir,
}

fn harness() -> H {
    let dir = TempDir::new().unwrap();
    // SAFETY: test-only; every test sets it the same way before reading it.
    unsafe { std::env::set_var("GLIDEX_RUN_DIR", dir.path().join("run")) };
    let manager = VmManager::with_db_path(dir.path().join("t.db")).unwrap();
    let mut cfg = Config::default();
    cfg.authz.policy_files_dir = dir.path().join("policies");
    cfg.auth.allowed_origins = vec![ORIGIN.into()];
    cfg.server_name = Some("lab-cp".into());
    // ui.sock checks its peer is the UI's user: make that us.
    cfg.ui_user = nix::unistd::User::from_uid(nix::unistd::getuid())
        .unwrap()
        .unwrap()
        .name;
    let auth = AuthService::new(manager.database(), cfg).unwrap();
    let app = Arc::new(App { manager, auth });
    H {
        router: api::router(app.clone()),
        app,
        _dir: dir,
    }
}

#[derive(Clone)]
enum Who {
    Nobody,
    Bearer(String),
    /// Session cookie with its CSRF value and the UI's Origin.
    Cookie(String, String),
}

/// What came back: status, headers, body bytes, trailers.
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: bytes::Bytes,
    trailers: Option<HeaderMap>,
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

    fn session(&self, user: &str) -> Who {
        let u = self.app.auth.store.user(user).unwrap().unwrap();
        let (cookie, csrf) = self.app.auth.create_session(&u, Method::Pam).unwrap();
        Who::Cookie(cookie, csrf)
    }

    async fn send(
        &self,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
        who: &Who,
        listener: Option<Listener>,
    ) -> Reply {
        let mut b = Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, content_type);
        match who {
            Who::Nobody => {}
            Who::Bearer(t) => b = b.header(header::AUTHORIZATION, format!("Bearer {t}")),
            Who::Cookie(c, csrf) => {
                b = b
                    .header(header::COOKIE, format!("{}={}", auth::SESSION_COOKIE, c))
                    .header(auth::CSRF_HEADER, csrf.as_str())
                    .header(header::ORIGIN, ORIGIN)
            }
        }
        if let Some(l) = listener {
            b = b.extension(l);
            if l == Listener::Ui {
                b = b.extension(PeerUid(nix::unistd::getuid().as_raw()));
            }
        }
        let resp = self
            .router
            .clone()
            .oneshot(b.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let (parts, body) = resp.into_parts();
        let collected = body.collect().await.unwrap();
        let trailers = collected.trailers().cloned();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: collected.to_bytes(),
            trailers,
        }
    }

    /// Connect protocol, JSON (what curl sends).
    async fn connect(&self, path: &str, who: &Who) -> (StatusCode, Value) {
        let r = self
            .send(path, "application/json", b"{}".to_vec(), who, None)
            .await;
        (
            r.status,
            serde_json::from_slice(&r.body).unwrap_or(Value::Null),
        )
    }

    /// gRPC: an empty request message in one envelope.
    async fn grpc(&self, path: &str, who: &Who, listener: Option<Listener>) -> Reply {
        self.send(path, "application/grpc", envelope(0, &[]), who, listener)
            .await
    }
}

fn envelope(flags: u8, msg: &[u8]) -> Vec<u8> {
    let mut v = vec![flags];
    v.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    v.extend_from_slice(msg);
    v
}

/// The envelopes of a gRPC / gRPC-Web body: (flags, payload).
fn envelopes(mut b: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    while b.len() >= 5 {
        let len = u32::from_be_bytes([b[1], b[2], b[3], b[4]]) as usize;
        out.push((b[0], b[5..5 + len].to_vec()));
        b = &b[5 + len..];
    }
    out
}

/// `grpc-status` from the trailers, or from the headers of a
/// trailers-only reply.
fn grpc_status(r: &Reply) -> Option<String> {
    r.trailers
        .as_ref()
        .and_then(|t| t.get("grpc-status"))
        .or_else(|| r.headers.get("grpc-status"))
        .map(|v| v.to_str().unwrap().to_string())
}

/// `ErrorInfo.reason` from a Connect error body.
fn connect_reason(v: &Value) -> Option<String> {
    let d = v["details"]
        .as_array()?
        .iter()
        .find(|d| d["type"] == "google.rpc.ErrorInfo")?;
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(d["value"].as_str()?.trim_end_matches('='))
        .ok()?;
    let info = ErrorInfo::decode_from_slice(&bytes).ok()?;
    assert_eq!(info.domain, "glidex");
    Some(info.reason)
}

#[tokio::test]
async fn server_info_is_one_answer_on_every_transport() {
    let h = harness();
    // REST twin: the same message as proto JSON.
    let rest = h
        .router
        .clone()
        .oneshot(
            Request::get("/auth/server-info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rest.status(), StatusCode::OK);
    let rest: Value =
        serde_json::from_slice(&rest.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(rest["cluster_name"], "lab-cp");
    assert_eq!(
        rest["methods"]["oidc"], false,
        "set falses are sent: {rest}"
    );
    assert!(rest["cluster_id"].is_null(), "standalone: {rest}");

    let (s, connect) = h.connect(SERVER_INFO, &Who::Nobody).await;
    assert_eq!(s, StatusCode::OK, "{connect}");
    assert_eq!(connect, rest);

    let r = h.grpc(SERVER_INFO, &Who::Nobody, None).await;
    assert_eq!(grpc_status(&r).as_deref(), Some("0"));
    let (flags, msg) = envelopes(&r.body).remove(0);
    assert_eq!(flags, 0);
    let info = GetServerInfoResponse::decode_from_slice(&msg).unwrap();
    assert_eq!(info.cluster_name, "lab-cp");
    assert_eq!(info.version, rest["version"].as_str().unwrap());
}

#[tokio::test]
async fn whoami_needs_a_credential_and_reports_the_caller() {
    let h = harness();
    let (s, v) = h.connect(WHOAMI, &Who::Nobody).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["code"], "unauthenticated");
    assert_eq!(connect_reason(&v).as_deref(), Some("unauthenticated"));

    let alice = h.user("alice");
    let token = Who::Bearer(h.token(&alice));
    let (s, v) = h.connect(WHOAMI, &token).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["user_id"], alice.as_str());
    assert_eq!(v["method"], "token");
    assert_eq!(v["break_glass"], false);

    let r = h.grpc(WHOAMI, &token, None).await;
    assert_eq!(grpc_status(&r).as_deref(), Some("0"));
    let me = WhoAmIResponse::decode_from_slice(&envelopes(&r.body)[0].1).unwrap();
    assert_eq!(me.user_id.as_deref(), Some(alice.as_str()));

    // gRPC errors: status 16 (UNAUTHENTICATED).
    let r = h.grpc(WHOAMI, &Who::Nobody, None).await;
    assert_eq!(grpc_status(&r).as_deref(), Some("16"));
}

#[tokio::test]
async fn cookies_only_for_grpc_web_through_the_ui() {
    let h = harness();
    let alice = h.user("alice");
    let cookie = h.session(&alice);

    // Connect over TCP with a cookie: refused, though CSRF and Origin are right.
    let (s, v) = h.connect(WHOAMI, &cookie).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(connect_reason(&v).as_deref(), Some("cookie_not_accepted"));

    // gRPC-Web from glidex-ui with the cookie: the browser path.
    let r = h
        .send(
            WHOAMI,
            "application/grpc-web+proto",
            envelope(0, &[]),
            &cookie,
            Some(Listener::Ui),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let frames = envelopes(&r.body);
    let me = WhoAmIResponse::decode_from_slice(&frames[0].1).unwrap();
    assert_eq!(me.method, "pam");
    // gRPC-Web carries its trailers as the last envelope (flag 0x80).
    let (flags, trailer) = frames.last().unwrap();
    assert_eq!(*flags & 0x80, 0x80);
    assert!(
        String::from_utf8_lossy(trailer).contains("grpc-status: 0")
            || String::from_utf8_lossy(trailer).contains("grpc-status:0")
    );

    // The same request without CSRF is stopped by the authentication layer.
    let Who::Cookie(c, _) = &cookie else {
        unreachable!()
    };
    let r = h
        .send(
            WHOAMI,
            "application/grpc-web+proto",
            envelope(0, &[]),
            &Who::Cookie(c.clone(), "wrong".into()),
            Some(Listener::Ui),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn each_listener_takes_its_protocols() {
    let h = harness();
    let token = Who::Bearer(h.token(&h.user("alice")));
    // gRPC-Web is the browser's: not on TCP.
    let r = h
        .send(
            WHOAMI,
            "application/grpc-web+proto",
            envelope(0, &[]),
            &token,
            None,
        )
        .await;
    assert_eq!(
        grpc_web_status(&r).as_deref(),
        Some("7"),
        "PERMISSION_DENIED"
    );
    // The UI socket takes nothing but gRPC-Web.
    let r = h.grpc(WHOAMI, &token, Some(Listener::Ui)).await;
    assert_eq!(grpc_status(&r).as_deref(), Some("7"));
}

/// `grpc-status` of a gRPC-Web reply: headers (trailers-only) or the
/// trailer envelope.
fn grpc_web_status(r: &Reply) -> Option<String> {
    if let Some(v) = r.headers.get("grpc-status") {
        return Some(v.to_str().unwrap().to_string());
    }
    let (flags, t) = envelopes(&r.body)
        .into_iter()
        .find(|(f, _)| f & 0x80 != 0)?;
    debug_assert!(flags & 0x80 != 0);
    String::from_utf8_lossy(&t).lines().find_map(|l| {
        l.trim()
            .strip_prefix("grpc-status:")
            .map(|s| s.trim().to_string())
    })
}

#[tokio::test]
async fn health_is_served_without_credentials() {
    let h = harness();
    // grpc.health.v1.HealthCheckRequest{service: ""} is empty on the wire.
    let r = h
        .grpc("/grpc.health.v1.Health/Check", &Who::Nobody, None)
        .await;
    assert_eq!(grpc_status(&r).as_deref(), Some("0"));
    // HealthCheckResponse{status: SERVING = 1}: field 1, varint 1.
    assert_eq!(envelopes(&r.body)[0].1, vec![0x08, 0x01]);
}

#[tokio::test]
async fn unknown_methods_are_unimplemented() {
    let h = harness();
    let r = h
        .grpc("/glidex.system.v1.SystemService/Nope", &Who::Nobody, None)
        .await;
    assert_eq!(grpc_status(&r).as_deref(), Some("12"));
}
