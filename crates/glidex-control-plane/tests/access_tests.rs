//! The access family on its RPCs and REST twins (spec/18-grpc.md §9.1, G2):
//! projects and bindings, users, teams, tokens, host bindings, policies,
//! audit and capability checks, in the proto shape on both transports.

mod common;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use buffa::Message;
use glidex_api::proto::glidex::access::v1 as pb;
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

    /// A fresh browser session (`session:cookie|csrf`), for step-up writes.
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

#[tokio::test(flavor = "multi_thread")]
async fn projects_users_teams_and_tokens_over_rest() {
    let h = harness();
    let admin = h.admin("ada");
    let t = h.token(&admin);

    // whoami is the WhoAmI message.
    let (st, v) = h.rest("GET", "/auth/whoami", None, &t).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["user"]["display_name"], "ada");
    assert_eq!(v["method"], "token");
    assert_eq!(v["user_id"], json!(admin));

    // A project: create, read with usage, update by the fields present.
    let (st, v) = h
        .rest(
            "POST",
            "/projects",
            Some(json!({"name": "web", "quotas": {"vms": 3}})),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let p = v["project"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["project"]["quotas"]["vms"], 3);
    let (_, v) = h.rest("GET", "/projects/web", None, &t).await;
    assert!(v["project"]["usage"].is_object(), "{v}");
    let (st, v) = h
        .rest(
            "PATCH",
            &format!("/projects/{p}"),
            Some(json!({"description": "front end"})),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["project"]["description"], "front end");
    assert_eq!(v["project"]["quotas"]["vms"], 3, "quotas untouched: {v}");
    let (_, v) = h.rest("GET", "/projects", None, &t).await;
    assert!(
        v["projects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["id"] == json!(p)),
        "{v}"
    );

    // Users and teams.
    let (st, v) = h
        .rest("POST", "/users", Some(json!({"display_name": "Bob", "identities": [{"provider": "pam", "subject": "bob"}]})), &t)
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let bob = v["user"]["id"].as_str().unwrap().to_string();
    let (st, v) = h
        .rest(
            "PATCH",
            &format!("/users/{bob}"),
            Some(json!({"display_name": "Robert"})),
            &t,
        )
        .await;
    assert_eq!(
        (st, v["user"]["display_name"].as_str()),
        (StatusCode::OK, Some("Robert")),
        "{v}"
    );
    let (_, v) = h.rest("GET", "/users", None, &t).await;
    let listed = v["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == json!(bob))
        .cloned()
        .unwrap();
    assert_eq!(listed["identities"][0]["provider"], "pam");
    let (st, v) = h
        .rest("POST", "/teams", Some(json!({"name": "ops"})), &t)
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let team = v["team"]["id"].as_str().unwrap().to_string();
    let (_, v) = h
        .rest("PUT", &format!("/teams/{team}/members/{bob}"), None, &t)
        .await;
    assert_eq!(
        v["team"]["members"][0],
        json!({"user_id": bob, "source": "manual"})
    );

    // A binding with the principal's name.
    let (st, v) = h
        .rest(
            "POST",
            &format!("/projects/{p}/bindings"),
            Some(json!({"role": "role.viewer", "principal": {"type": "User", "id": bob}})),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["binding"]["principal_name"], "Robert");
    let link = v["binding"]["id"].as_str().unwrap().to_string();
    let (_, v) = h
        .rest("GET", &format!("/projects/{p}/bindings"), None, &t)
        .await;
    assert!(
        v["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["id"] == json!(link)),
        "{v}"
    );
    let (st, _) = h
        .rest(
            "DELETE",
            &format!("/projects/{p}/bindings/{link}"),
            None,
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // Tokens: the secret once, then only the record. Tokens can't make tokens.
    let s = h.session(&admin);
    let (st, v) = h
        .rest(
            "POST",
            "/tokens",
            Some(json!({"name": "ci", "expires_in_days": 2})),
            &s,
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let secret = v["token"].as_str().unwrap().to_string();
    assert!(secret.starts_with("gxt_"));
    assert_eq!(v["record"]["kind"], "TOKEN_KIND_PERSONAL");
    let id = v["record"]["id"].as_str().unwrap().to_string();
    let (_, v) = h.rest("GET", "/tokens", None, &s).await;
    assert!(!v.to_string().contains(&secret), "never listed");
    let (st, _) = h
        .rest("POST", "/tokens", Some(json!({"name": "x"})), &secret)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = h.rest("DELETE", &format!("/tokens/{id}"), None, &s).await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // The audit log, as entries with RFC 3339 times.
    let (st, v) = h.rest("GET", "/audit?limit=5", None, &t).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(
        v["entries"][0]["time"].as_str().unwrap().ends_with('Z'),
        "{v}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn access_services_over_connect_and_grpc() {
    let h = harness();
    let admin = h.admin("ada");
    let t = h.token(&admin);

    let req = pb::CreateProjectRequest {
        name: "rpc".into(),
        ..Default::default()
    };
    let (st, bytes) = h
        .grpc("/glidex.access.v1.ProjectService/CreateProject", &req, &t)
        .await;
    assert_eq!(st, "0");
    let p = pb::CreateProjectResponse::decode_from_slice(&bytes)
        .unwrap()
        .project
        .as_option()
        .unwrap()
        .id
        .clone();
    let (st, v) = h
        .connect(
            "glidex.access.v1.ProjectService/GetProject",
            json!({"id": "rpc"}),
            &t,
        )
        .await;
    assert_eq!(
        (st, v["project"]["id"].as_str()),
        (StatusCode::OK, Some(p.as_str())),
        "{v}"
    );

    // UpdateProject changes only what the mask names.
    let (st, v) = h
        .connect(
            "glidex.access.v1.ProjectService/UpdateProject",
            json!({"id": p, "description": "d", "quotas": {"vms": 1}, "update_mask": "description"}),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["project"]["description"], "d");
    assert_ne!(v["project"]["quotas"]["vms"], 1, "{v}");

    // Teams and the authz checks.
    let (st, v) = h
        .connect(
            "glidex.access.v1.TeamService/CreateTeam",
            json!({"name": "rpc-team"}),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = h
        .connect(
            "glidex.access.v1.AuthzService/Check",
            json!({"checks": [{"action": "readProject", "resource": {"type": "Project", "id": p}}, {"action": "nope"}]}),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["results"][0]["allowed"], true);
    assert_eq!(v["results"][1]["error"], "unknown action");
    let req = pb::AllowedRequest {
        actions: vec!["readProject".into(), "deleteProject".into()],
        resources: vec![glidex_api::proto::glidex::api::v1::EntityRef {
            r#type: "Project".into(),
            id: Some(p.clone()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let (st, bytes) = h
        .grpc("/glidex.access.v1.AuthzService/Allowed", &req, &t)
        .await;
    assert_eq!(st, "0");
    let a = pb::AllowedResponse::decode_from_slice(&bytes).unwrap();
    assert!(a.allowed[0].actions.contains(&"readProject".to_string()));

    // Policies: list, validate, simulate.
    let (st, v) = h
        .connect("glidex.access.v1.PolicyService/ListPolicies", json!({}), &t)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(
        v["policies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["source"] == "POLICY_SOURCE_BASE"),
        "{v}"
    );
    let (_, v) = h
        .connect(
            "glidex.access.v1.PolicyService/ValidatePolicy",
            json!({"id": "site.x", "text": "not cedar"}),
            &t,
        )
        .await;
    assert_eq!(v["valid"], false, "{v}");
    let bob = h.user("bob");
    let (st, v) = h
        .connect(
            "glidex.access.v1.PolicyService/SimulatePolicy",
            json!({"requests": [{"principal": {"type": "User", "id": bob}, "action": "readProject", "resource": {"type": "Project", "id": p}}]}),
            &t,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        v["results"][0]["current"]["allowed"], false,
        "bob has no role: {v}"
    );

    // Someone without directory rights can't list users.
    let (st, _) = h
        .connect(
            "glidex.access.v1.UserService/ListUsers",
            json!({}),
            &h.token(&bob),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, bytes) = h
        .grpc(
            "/glidex.access.v1.UserService/ListUsers",
            &pb::ListUsersRequest::default(),
            &t,
        )
        .await;
    assert_eq!(st, "0");
    assert!(
        pb::ListUsersResponse::decode_from_slice(&bytes)
            .unwrap()
            .users
            .iter()
            .any(|u| u.id == bob)
    );
    let _ = (
        Ent::Cluster,
        TokenKind::Personal {
            owner: String::new(),
        },
    );
}
