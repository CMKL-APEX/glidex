//! Logging in and out, and the REST twins of `glidex.access.v1`
//! (spec/security.md §5, §6, §7, §10, §13): projects, role links, users,
//! teams, tokens, site policies, audit and capability checks. Logins set
//! cookies and stay HTTP-only; the rest is in `service::access`.

use super::{
    ApiErr, AppState, Caller, ClientAddr, Listener, Proto, RequestId, auth_error, err, proto_body,
};
use crate::auth::{self, AuthError, Method, Principal};
use crate::directory::invitations::InviteToken;
use crate::directory::oidc::After;
use crate::service::access as acc;
use crate::service::login::{self as svc, audit_login, oidc_client, oidc_error, oidc_user_invited};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use glidex_api::proto::glidex::access::v1 as pb;
use glidex_api::proto::glidex::auth::v1 as login_pb;
use serde::Deserialize;
use zeroize::Zeroizing;

// ---- login -------------------------------------------------------------

pub async fn methods(State(app): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "pam": app.auth.pam_connector(None).is_ok(),
        "oidc": app.auth.oidc_enabled(),
        "disabled": app.auth.is_disabled(),
        "connectors": svc::discover(&app, Default::default()).connectors,
    }))
}

/// `POST /auth/discover` (spec/tenancy.md §6.1), the REST twin of
/// `LoginService.Discover`.
pub async fn discover(
    State(app): State<AppState>,
    Json(req): Json<login_pb::DiscoverRequest>,
) -> impl IntoResponse {
    Json(svc::discover(&app, req))
}

#[derive(Deserialize)]
pub struct LoginBody {
    #[serde(default = "default_method")]
    method: String,
    username: String,
    password: Zeroizing<String>,
    /// A PAM connector; the first one when absent.
    #[serde(default)]
    connector: Option<String>,
}

fn default_method() -> String {
    "pam".into()
}

/// Whether cookies should be `Secure`: the browser reached us over TLS.
fn secure_transport(app: &AppState, headers: &HeaderMap, listener: Listener) -> bool {
    match listener {
        Listener::Ui | Listener::Relay => {
            headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                == Some("https")
        }
        Listener::Tcp => app.auth.config.tls.enabled(),
        Listener::Api => false,
        // A forwarded login: the first server saw how the browser connected.
        Listener::Cluster => {
            headers.get("x-glidex-secure").and_then(|v| v.to_str().ok()) == Some("true")
        }
    }
}

fn session_cookie(value: &str, secure: bool, max_age: Option<u64>) -> HeaderValue {
    let mut c = format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/",
        auth::SESSION_COOKIE,
        value
    );
    if let Some(a) = max_age {
        c.push_str(&format!("; Max-Age={}", a));
    }
    if secure {
        c.push_str("; Secure");
    }
    HeaderValue::from_str(&c).expect("cookie is ASCII")
}

/// Who the caller is, for `/auth/whoami` and login responses: the
/// `SystemService.WhoAmI` message (spec/grpc.md §9.1).
fn whoami_json(app: &AppState, p: &Principal) -> Result<serde_json::Value, ApiErr> {
    let w = crate::service::system::whoami_of(app, p)?;
    Ok(serde_json::to_value(w).unwrap_or_default())
}

pub async fn login(
    State(app): State<AppState>,
    Extension(rid): Extension<RequestId>,
    listener: Option<Extension<Listener>>,
    addr: Option<Extension<ClientAddr>>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Result<Response, ApiErr> {
    let listener = listener.map(|l| l.0).unwrap_or(Listener::Tcp);
    if body.method != "pam" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "only \"pam\" logins use this endpoint; OIDC starts at /auth/oidc/start",
        ));
    }
    // Passwords only over TLS or from this host (spec §5.3).
    let local = match listener {
        Listener::Ui | Listener::Api | Listener::Cluster | Listener::Relay => true,
        Listener::Tcp => {
            app.auth.config.tls.enabled() || addr.is_none_or(|a| a.0.0.ip().is_loopback())
        }
    };
    if !local {
        return Err(err(
            StatusCode::FORBIDDEN,
            "tls_required",
            "password login needs TLS",
        ));
    }
    let a = app.clone();
    let username = body.username.clone();
    let password = body.password;
    let connector = body.connector.clone();
    let result = tokio::task::spawn_blocking(move || {
        a.auth
            .login_pam_with(connector.as_deref(), &username, &password)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let user = match result {
        Ok((u, _)) => u,
        Err(e) => {
            audit_login(&app, None, &rid.0, "pam", "denied", &body.username);
            return Err(auth_error(e));
        }
    };
    let (cookie, _) = app
        .auth
        .create_session(&user, Method::Pam)
        .map_err(auth_error)?;
    let p = app
        .auth
        .session_principal(&cookie, auth::Transport::Tcp, None)
        .map_err(auth_error)?
        .ok_or_else(|| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "new session missing",
            )
        })?;
    audit_login(&app, Some(&p), &rid.0, "pam", "ok", &body.username);
    let mut resp = Json(whoami_json(&app, &p)?).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&cookie, secure_transport(&app, &headers, listener), None),
    );
    Ok(resp)
}

/// A browser session for a local user identified on api.sock (gxctl
/// `ui`). The session carries the user's stored teams only: break-glass
/// (from Unix groups on the socket) doesn't carry over to a browser.
pub async fn peer_session(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    if c.p.method != Method::Peer {
        return Err(err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only local users on the glidex socket can open a session this way",
        ));
    }
    let user =
        c.p.user
            .clone()
            .ok_or_else(|| err(StatusCode::FORBIDDEN, "forbidden", "no user"))?;
    let (cookie, csrf) = c
        .auth()
        .create_session(&user, Method::Pam)
        .map_err(auth_error)?;
    c.set_target(format!("user:{}", user.id));
    Ok(Json(
        serde_json::json!({ "cookie_name": auth::SESSION_COOKIE, "cookie": cookie, "csrf": csrf }),
    ))
}

/// The host's identity for client profiles (spec/gxctl-auth.md §7.1):
/// static after start. The fingerprint is public information — the server
/// logs it at every start (security.md §5.1.1) — and lets gxctl tell a
/// genuine untrusted self-signed certificate from a proxy that terminated
/// TLS in front of it.
///
/// REST twin of `SystemService.GetServerInfo`: the same message, as proto
/// JSON (an absent cluster or fingerprint is left out rather than `null`).
pub async fn server_info(State(app): State<AppState>) -> impl IntoResponse {
    Json(crate::service::system::server_info(&app))
}

/// `POST /auth/token`, the REST twin of `LoginService.PasswordToken`.
pub async fn pam_token(
    State(app): State<AppState>,
    Extension(rid): Extension<RequestId>,
    listener: Option<Extension<Listener>>,
    addr: Option<Extension<ClientAddr>>,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let origin = svc::Origin {
        request_id: &rid.0,
        listener: listener.map(|l| l.0).unwrap_or(Listener::Tcp),
        client_ip: addr.map(|a| a.0.0.ip()),
    };
    let req = proto_body(v)?;
    let m = svc::password_token(&app, &origin, req).await?;
    Ok(Json(login_pb::PasswordTokenResponse {
        issued: m.to_pb().into(),
        ..Default::default()
    }))
}

pub async fn logout(
    c: Caller,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
) -> Result<Response, ApiErr> {
    c.auth().end_session(&c.p).map_err(auth_error)?;
    let secure = secure_transport(
        &c.app,
        &headers,
        listener.map(|l| l.0).unwrap_or(Listener::Tcp),
    );
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, session_cookie("", secure, Some(0)));
    Ok(resp)
}

pub async fn whoami(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(StatusCode::OK, crate::service::system::whoami(&c)?))
}

pub async fn update_me(
    c: Caller,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::UpdateMeRequest = proto_body(v)?;
    Ok(Proto(StatusCode::OK, acc::update_me(&c, req)?))
}

/// The caller's own SSH public keys from their home directory, read by
/// glidex-authd: `{available, reason?, username?, keys}`.
pub async fn my_ssh_keys(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        acc::my_ssh_keys(&c, Default::default()).await?,
    ))
}

// ---- OIDC ----------------------------------------------------------------

const OIDC_COOKIE: &str = "gx_oidc";

#[derive(Deserialize, Default)]
pub struct OidcStartQuery {
    #[serde(default)]
    return_to: Option<String>,
    #[serde(default)]
    reauth: bool,
}

fn first_origin(app: &AppState) -> Option<&str> {
    app.auth
        .config
        .auth
        .allowed_origins
        .first()
        .map(String::as_str)
}

/// Start a login through connector `id`: where to send the browser, and
/// the cookie that binds the login to it.
async fn begin_oidc(
    app: &AppState,
    id: Option<&str>,
    listener: Option<Extension<Listener>>,
    headers: &HeaderMap,
    return_to: Option<&str>,
    reauth: bool,
    after: After,
) -> Result<(String, HeaderValue), ApiErr> {
    let (_, oidc) = oidc_client(app, id)?;
    let start = oidc
        .start(
            return_to,
            &oidc.redirect_uri(first_origin(app)),
            reauth,
            after,
        )
        .await
        .map_err(oidc_error)?;
    let secure = secure_transport(app, headers, listener.map(|l| l.0).unwrap_or(Listener::Tcp));
    // Lax: it must come back on the IdP's top-level redirect.
    let mut c = format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=600",
        OIDC_COOKIE, start.browser
    );
    if secure {
        c.push_str("; Secure");
    }
    Ok((start.redirect, HeaderValue::from_str(&c).expect("ascii")))
}

async fn start_oidc(
    app: AppState,
    id: Option<&str>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    q: OidcStartQuery,
) -> Result<Response, ApiErr> {
    let (redirect, cookie) = begin_oidc(
        &app,
        id,
        listener,
        &headers,
        q.return_to.as_deref(),
        q.reauth,
        After::Login,
    )
    .await?;
    let mut resp = Redirect::to(&redirect).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(resp)
}

#[derive(Deserialize, Default)]
pub struct StartBody {
    #[serde(default)]
    return_to: Option<String>,
    /// `<id>.<secret>` from an invitation link (spec/tenancy.md §6.1).
    #[serde(default)]
    invitation: Option<String>,
}

/// `POST /auth/c/{id}/start`: `{redirect}` for the page to follow, so an
/// invitation's secret goes in a body, never in a URL. `404` for an
/// invitation that isn't open or isn't for this connector.
pub async fn connector_start_post(
    State(app): State<AppState>,
    Path(id): Path<String>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Json(b): Json<StartBody>,
) -> Result<Response, ApiErr> {
    let invite = match b.invitation.as_deref() {
        Some(s) => {
            let open = InviteToken::parse(s)
                .and_then(|t| app.auth.invitations.open(&t).ok().flatten())
                .filter(|i| i.connector_id == id);
            if open.is_none() {
                return Err(err(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "no such invitation",
                ));
            }
            After::Invite(s.trim().to_string())
        }
        None => After::Login,
    };
    let (redirect, cookie) = begin_oidc(
        &app,
        Some(&id),
        listener,
        &headers,
        b.return_to.as_deref(),
        false,
        invite,
    )
    .await?;
    let mut resp = Json(serde_json::json!({ "redirect": redirect })).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(resp)
}

/// `POST /auth/link/c/{id}/start` (spec/tenancy.md §6.2): add a sign-in
/// through connector `id` to the caller, who is signed in, beside the ones
/// it has. `manageOwnSignIns` (a recent login). Answers `{redirect}`; the
/// callback goes back to `return_to` with `?linked=<id>` or
/// `?link_error=<code>`.
pub async fn link_start(
    c: Caller,
    Path(id): Path<String>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Json(b): Json<StartBody>,
) -> Result<Response, ApiErr> {
    let Some(user) = c.p.user_id().map(String::from) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "only a user signs in: a service account has no sign-in methods",
        ));
    };
    c.require_action(
        "manageOwnSignIns",
        crate::authz::Ent::User(user.clone()),
        crate::authz::EntitySet::new(),
        &[],
    )?;
    c.set_target(format!("user:{user}"));
    c.detail("connector", serde_json::json!(id));
    let (redirect, cookie) = begin_oidc(
        &c.app,
        Some(&id),
        listener,
        &headers,
        b.return_to.as_deref(),
        true,
        After::Link {
            user,
            session: c.p.session_hash.clone(),
        },
    )
    .await?;
    let mut resp = Json(serde_json::json!({ "redirect": redirect })).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(resp)
}

/// `return_to` with `key=value` added.
fn with_query(path: &str, key: &str, value: &str) -> String {
    let sep = if path.contains('?') { '&' } else { '?' };
    let value: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    format!("{path}{sep}{key}={value}")
}

/// `GET /auth/oidc/start`: the first OIDC connector (from before connectors).
pub async fn oidc_start(
    State(app): State<AppState>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Query(q): Query<OidcStartQuery>,
) -> Result<Response, ApiErr> {
    start_oidc(app, None, listener, headers, q).await
}

/// `GET /auth/c/{id}/start` (spec/tenancy.md §6).
pub async fn connector_start(
    State(app): State<AppState>,
    Path(id): Path<String>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Query(q): Query<OidcStartQuery>,
) -> Result<Response, ApiErr> {
    start_oidc(app, Some(&id), listener, headers, q).await
}

#[derive(Deserialize, Default)]
pub struct OidcCallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

async fn finish_oidc(
    app: AppState,
    id: Option<&str>,
    rid: RequestId,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    q: OidcCallbackQuery,
) -> Result<Response, ApiErr> {
    if let Some(e) = q.error {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "login_failed",
            format!("identity provider: {}", e),
        ));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "code and state are required",
        ));
    };
    let (c, oidc) = oidc_client(&app, id)?;
    let browser = super::cookie(&headers, OIDC_COOKIE);
    let (identity, return_to, after) = match oidc
        .callback(
            &code,
            &state,
            browser.as_deref(),
            &oidc.redirect_uri(first_origin(&app)),
        )
        .await
    {
        Ok(v) => v,
        // A sign-in being added goes back to its page with why it wasn't.
        Err(crate::directory::oidc::CallbackError {
            started: Some((return_to, After::Link { user: uid, .. })),
            ..
        }) => {
            audit_login(&app, None, &rid.0, "oidc-link", "denied", &uid);
            return Ok(
                Redirect::to(&with_query(&return_to, "link_error", "not_allowed")).into_response(),
            );
        }
        Err(e) => {
            audit_login(&app, None, &rid.0, "oidc", "denied", "-");
            return Err(oidc_error(e.error).into());
        }
    };
    let clear_oidc = || {
        HeaderValue::from_str(&format!(
            "{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
            OIDC_COOKIE
        ))
        .expect("ascii")
    };
    let invite = match after {
        After::Link { user: uid, session } => {
            // The session that started it must still be live: the login
            // state is this browser's (its Lax cookie), and the session
            // cookie (Strict) doesn't come back on the IdP's redirect.
            let current = session
                .as_deref()
                .filter(|h| app.auth.session_is_live(h, &uid))
                .and_then(|_| app.auth.store.user(&uid).ok().flatten());
            let ext = svc::external_of(&c, &identity);
            let outcome = match &current {
                None => Err("not_signed_in"),
                Some(_) => match app.auth.link_external(&c, &ext, &uid) {
                    Ok(_) => Ok(()),
                    Err(AuthError::NotAllowed(m)) if m.contains("another user") => Err("taken"),
                    Err(_) => Err("not_allowed"),
                },
            };
            audit_login(
                &app,
                None,
                &rid.0,
                "oidc-link",
                if outcome.is_ok() { "ok" } else { "denied" },
                &uid,
            );
            let to = match outcome {
                Ok(()) => with_query(&return_to, "linked", &c.id),
                Err(code) => with_query(&return_to, "link_error", code),
            };
            let mut resp = Redirect::to(&to).into_response();
            resp.headers_mut().append(header::SET_COOKIE, clear_oidc());
            return Ok(resp);
        }
        After::Invite(s) => InviteToken::parse(&s),
        After::Login => None,
    };
    let user = match oidc_user_invited(&app, &c, &identity, invite.as_ref()) {
        Ok(u) => u,
        Err(e) => {
            audit_login(&app, None, &rid.0, "oidc", "denied", &identity.subject);
            return Err(e.into());
        }
    };
    let (cookie, _) = app
        .auth
        .create_session_with(&user, Method::Oidc, identity.mfa)
        .map_err(auth_error)?;
    let p = app
        .auth
        .session_principal(&cookie, auth::Transport::Tcp, None)
        .map_err(auth_error)?;
    audit_login(&app, p.as_ref(), &rid.0, "oidc", "ok", &user.id);
    let secure = secure_transport(
        &app,
        &headers,
        listener.map(|l| l.0).unwrap_or(Listener::Tcp),
    );
    let mut resp = Redirect::to(&return_to).into_response();
    resp.headers_mut()
        .append(header::SET_COOKIE, session_cookie(&cookie, secure, None));
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0",
            OIDC_COOKIE
        ))
        .expect("ascii"),
    );
    Ok(resp)
}

pub async fn oidc_callback(
    State(app): State<AppState>,
    Extension(rid): Extension<RequestId>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Query(q): Query<OidcCallbackQuery>,
) -> Result<Response, ApiErr> {
    finish_oidc(app, None, rid, listener, headers, q).await
}

pub async fn connector_callback(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Extension(rid): Extension<RequestId>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Query(q): Query<OidcCallbackQuery>,
) -> Result<Response, ApiErr> {
    finish_oidc(app, Some(&id), rid, listener, headers, q).await
}

pub async fn oidc_device_start(State(app): State<AppState>) -> Result<impl IntoResponse, ApiErr> {
    Ok(Json(svc::start_device(&app, Default::default()).await?))
}

pub async fn connector_device_start(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = login_pb::StartDeviceRequest {
        connector: Some(id),
        ..Default::default()
    };
    Ok(Json(svc::start_device(&app, req).await?))
}

/// `POST …/device/poll`, the REST twin of `LoginService.PollDevice`:
/// `202` while the user hasn't finished.
async fn poll_device(
    app: AppState,
    id: Option<String>,
    rid: RequestId,
    mut v: serde_json::Value,
) -> Result<Response, ApiErr> {
    let origin = svc::Origin {
        request_id: &rid.0,
        listener: Listener::Tcp,
        client_ip: None,
    };
    if let (Some(id), Some(o)) = (id, v.as_object_mut()) {
        o.insert("connector".into(), id.into());
    }
    let req = proto_body(v)?;
    Ok(match svc::poll_device(&app, &origin, req).await? {
        None => (
            StatusCode::ACCEPTED,
            Json(login_pb::PollDeviceResponse {
                pending: Some(true),
                ..Default::default()
            }),
        )
            .into_response(),
        Some(m) => Json(login_pb::PollDeviceResponse {
            issued: m.to_pb().into(),
            ..Default::default()
        })
        .into_response(),
    })
}

pub async fn oidc_device_poll(
    State(app): State<AppState>,
    Extension(rid): Extension<RequestId>,
    Json(v): Json<serde_json::Value>,
) -> Result<Response, ApiErr> {
    poll_device(app, None, rid, v).await
}

pub async fn connector_device_poll(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Extension(rid): Extension<RequestId>,
    Json(v): Json<serde_json::Value>,
) -> Result<Response, ApiErr> {
    poll_device(app, Some(id), rid, v).await
}

// ---- REST twins of glidex.access.v1 (spec/grpc.md §9.1) -----------------
//
// The proto messages as JSON, path and query parameters merged in, on the
// same `service::access` functions as the RPCs. Status codes are as before
// (`201` for a create, `204` for a delete).

/// Without an `update_mask`, the fields present (of `fields`) are the
/// ones changed.
fn masked<T: serde::de::DeserializeOwned + buffa::MessageName>(
    v: serde_json::Value,
    fields: &[&str],
) -> Result<(T, Vec<String>), ApiErr> {
    let explicit: Option<Vec<String>> = v.get("update_mask").and_then(|m| m.as_str()).map(|s| {
        s.split(',')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    });
    let present: Vec<String> = v
        .as_object()
        .map(|o| {
            o.keys()
                .filter(|k| fields.contains(&k.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let req = proto_body(v)?;
    Ok((req, explicit.unwrap_or(present)))
}

fn field_mask(paths: Vec<String>) -> buffa_types::google::protobuf::FieldMask {
    buffa_types::google::protobuf::FieldMask {
        paths,
        ..Default::default()
    }
}

// ---- capability checks (spec §7.7) ---------------------------------------

pub async fn authz_check(
    c: Caller,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::CheckRequest = proto_body(v)?;
    Ok(Proto(StatusCode::OK, acc::check(&c, req).await?))
}

/// `POST /authz/allowed` (spec/clustering-ui.md §3.5): `{allowed:
/// [{actions}], reauth: [{actions}]}`, one entry per resource.
pub async fn authz_allowed(
    c: Caller,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::AllowedRequest = proto_body(v)?;
    Ok(Proto(StatusCode::OK, acc::allowed(&c, req).await?))
}

// ---- tokens --------------------------------------------------------------

pub async fn list_tokens(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        acc::list_tokens(&c, Default::default())?,
    ))
}

/// `{token, record}`: the secret is shown once.
pub async fn create_token(
    c: Caller,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::CreateTokenRequest = proto_body(v)?;
    Ok(Proto(StatusCode::CREATED, acc::create_token(&c, req)?))
}

pub async fn revoke_token(c: Caller, Path(id): Path<String>) -> Result<Response, ApiErr> {
    let req = pb::RevokeTokenRequest {
        id,
        ..Default::default()
    };
    acc::revoke_token(&c, req)?;
    Ok(super::proto_no_content())
}

// ---- users and teams -----------------------------------------------------

pub async fn list_users(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        acc::list_users(&c, Default::default())?,
    ))
}

pub async fn create_user(
    c: Caller,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::CreateUserRequest = proto_body(v)?;
    Ok(Proto(StatusCode::CREATED, acc::create_user(&c, req)?))
}

pub async fn update_user(
    c: Caller,
    Path(id): Path<String>,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let (mut req, mask): (pb::UpdateUserRequest, _) =
        masked(v, &["display_name", "disabled", "default_project"])?;
    req.id = id;
    req.update_mask = field_mask(mask).into();
    Ok(Proto(StatusCode::OK, acc::update_user(&c, req)?))
}

/// `DELETE /users/{id}`: what went with the user.
pub async fn delete_user(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::DeleteUserRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, acc::delete_user(&c, req)?))
}

pub async fn link_identity(
    c: Caller,
    Path(id): Path<String>,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let mut req: pb::LinkIdentityRequest = proto_body(v)?;
    req.id = id;
    Ok(Proto(StatusCode::CREATED, acc::link_identity(&c, req)?))
}

#[derive(Deserialize, Default)]
pub struct UnlinkQuery {
    #[serde(default)]
    end_sessions: bool,
}

/// `DELETE /users/{id}/identities/{key}[?end_sessions=true]`.
pub async fn unlink_identity(
    c: Caller,
    Path((id, key)): Path<(String, String)>,
    Query(q): Query<UnlinkQuery>,
) -> Result<Response, ApiErr> {
    let req = pb::UnlinkIdentityRequest {
        id,
        key,
        end_sessions: q.end_sessions.then_some(true),
        ..Default::default()
    };
    acc::unlink_identity(&c, req)?;
    Ok(super::proto_no_content())
}

pub async fn list_teams(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        acc::list_teams(&c, Default::default())?,
    ))
}

pub async fn create_team(
    c: Caller,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::CreateTeamRequest = proto_body(v)?;
    Ok(Proto(StatusCode::CREATED, acc::create_team(&c, req)?))
}

pub async fn update_team(
    c: Caller,
    Path(id): Path<String>,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let mut req: pb::UpdateTeamRequest = proto_body(v)?;
    req.id = id;
    Ok(Proto(StatusCode::OK, acc::update_team(&c, req)?))
}

pub async fn delete_team(c: Caller, Path(id): Path<String>) -> Result<Response, ApiErr> {
    let req = pb::DeleteTeamRequest {
        id,
        ..Default::default()
    };
    acc::delete_team(&c, req)?;
    Ok(super::proto_no_content())
}

pub async fn add_member(
    c: Caller,
    Path((id, user)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::AddTeamMemberRequest {
        id,
        user,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, acc::add_team_member(&c, req)?))
}

pub async fn remove_member(
    c: Caller,
    Path((id, user)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::RemoveTeamMemberRequest {
        id,
        user,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, acc::remove_team_member(&c, req)?))
}

// ---- projects and role links ---------------------------------------------

pub async fn list_projects(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        acc::list_projects(&c, Default::default()).await?,
    ))
}

pub async fn create_project(
    c: Caller,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::CreateProjectRequest = proto_body(v)?;
    Ok(Proto(StatusCode::CREATED, acc::create_project(&c, req)?))
}

pub async fn get_project(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetProjectRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, acc::get_project(&c, req).await?))
}

pub async fn update_project(
    c: Caller,
    Path(id): Path<String>,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let (mut req, mask): (pb::UpdateProjectRequest, _) = masked(v, &["description", "quotas"])?;
    req.id = id;
    req.update_mask = field_mask(mask).into();
    Ok(Proto(StatusCode::OK, acc::update_project(&c, req)?))
}

pub async fn delete_project(c: Caller, Path(id): Path<String>) -> Result<Response, ApiErr> {
    let req = pb::DeleteProjectRequest {
        id,
        ..Default::default()
    };
    acc::delete_project(&c, req).await?;
    Ok(super::proto_no_content())
}

pub async fn list_bindings(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListProjectBindingsRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, acc::list_project_bindings(&c, req)?))
}

pub async fn add_binding(
    c: Caller,
    Path(id): Path<String>,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let mut req: pb::AddProjectBindingRequest = proto_body(v)?;
    req.id = id;
    Ok(Proto(
        StatusCode::CREATED,
        acc::add_project_binding(&c, req)?,
    ))
}

pub async fn remove_binding(
    c: Caller,
    Path((id, link)): Path<(String, String)>,
) -> Result<Response, ApiErr> {
    let req = pb::RemoveProjectBindingRequest {
        id,
        link,
        ..Default::default()
    };
    acc::remove_project_binding(&c, req)?;
    Ok(super::proto_no_content())
}

#[derive(serde::Deserialize)]
pub struct SystemBindingsQuery {
    node: Option<String>,
}

/// `GET /system/bindings[?node=<id or name>]`.
pub async fn list_system_bindings(
    c: Caller,
    Query(q): Query<SystemBindingsQuery>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListSystemBindingsRequest {
        node: q.node,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, acc::list_system_bindings(&c, req)?))
}

pub async fn add_system_binding(
    c: Caller,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::AddSystemBindingRequest = proto_body(v)?;
    Ok(Proto(
        StatusCode::CREATED,
        acc::add_system_binding(&c, req)?,
    ))
}

pub async fn remove_system_binding(
    c: Caller,
    Path(link): Path<String>,
) -> Result<Response, ApiErr> {
    let req = pb::RemoveSystemBindingRequest {
        link,
        ..Default::default()
    };
    acc::remove_system_binding(&c, req)?;
    Ok(super::proto_no_content())
}

// ---- site policies (spec §7.6) -------------------------------------------

pub async fn list_policies(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        acc::list_policies(&c, Default::default())?,
    ))
}

pub async fn get_policy(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetPolicyRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, acc::get_policy(&c, req)?))
}

pub async fn policy_versions(
    c: Caller,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListPolicyVersionsRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, acc::policy_versions(&c, req)?))
}

pub async fn put_policy(
    c: Caller,
    Path(id): Path<String>,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let mut req: pb::PutPolicyRequest = proto_body(v)?;
    req.id = id;
    Ok(Proto(StatusCode::OK, acc::put_policy(&c, req)?))
}

#[derive(Deserialize)]
pub struct VersionQuery {
    version: u64,
}

pub async fn delete_policy(
    c: Caller,
    Path(id): Path<String>,
    Query(q): Query<VersionQuery>,
) -> Result<Response, ApiErr> {
    let req = pb::DeletePolicyRequest {
        id,
        version: q.version,
        ..Default::default()
    };
    acc::delete_policy(&c, req)?;
    Ok(super::proto_no_content())
}

pub async fn validate_policy(
    c: Caller,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::ValidatePolicyRequest = proto_body(v)?;
    Ok(Proto(StatusCode::OK, acc::validate_policy(&c, req)?))
}

pub async fn simulate_policy(
    c: Caller,
    Json(v): Json<serde_json::Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::SimulatePolicyRequest = proto_body(v)?;
    Ok(Proto(StatusCode::OK, acc::simulate_policy(&c, req).await?))
}

pub async fn reload_policies(c: Caller) -> Result<Response, ApiErr> {
    acc::reload_policies(&c, Default::default())?;
    Ok(super::proto_no_content())
}

// ---- audit (spec §10) ----------------------------------------------------

#[derive(Deserialize, Default)]
pub struct AuditQuery {
    /// Unix milliseconds.
    #[serde(default)]
    since: Option<u64>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// `GET /audit` → `{entries}`.
pub async fn read_audit(
    c: Caller,
    Query(q): Query<AuditQuery>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ReadAuditRequest {
        since: q
            .since
            .map(|ms| buffa_types::google::protobuf::Timestamp {
                seconds: (ms / 1000) as i64,
                nanos: ((ms % 1000) * 1_000_000) as i32,
                ..Default::default()
            })
            .into(),
        project: q.project,
        user: q.user,
        limit: q.limit,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, acc::read_audit(&c, req)?))
}
