//! Authentication, identity, projects, role links, tokens, site policies
//! and audit (spec/security.md §5, §6, §7, §10, §13).

use super::{
    add_network, auth_error, credential_entities, disk_entities, err, error_to_response, vm_entities, ApiErr, Caller,
    ClientAddr, Listener, RequestId,
};
use crate::auth::store::{Identity, MemberSource, StoreError, Team, TeamMember, TokenKind, User};
use crate::auth::{self, AuthError, Method, Principal};
use crate::authz::{self, Ent, EntitySet, SiteSource};
use crate::tenancy::Quotas;
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::AppState;

fn store_err(e: StoreError) -> ApiErr {
    auth_error(AuthError::Store(e))
}

fn manager_err(e: crate::state::VmManagerError) -> ApiErr {
    error_to_response(e)
}

// ---- login -------------------------------------------------------------

pub async fn methods(State(app): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "pam": app.auth.config.auth.pam.enabled,
        "oidc": app.auth.oidc.enabled(),
        "disabled": app.auth.is_disabled(),
    }))
}

#[derive(Deserialize)]
pub struct LoginBody {
    #[serde(default = "default_method")]
    method: String,
    username: String,
    password: Zeroizing<String>,
}

fn default_method() -> String {
    "pam".into()
}

/// Whether cookies should be `Secure`: the browser reached us over TLS.
fn secure_transport(app: &AppState, headers: &HeaderMap, listener: Listener) -> bool {
    match listener {
        Listener::Ui => headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https"),
        Listener::Tcp => app.auth.config.tls.enabled(),
        Listener::Api => false,
        // A forwarded login: the first server saw how the browser connected.
        Listener::Cluster => headers.get("x-glidex-secure").and_then(|v| v.to_str().ok()) == Some("true"),
    }
}

fn session_cookie(value: &str, secure: bool, max_age: Option<u64>) -> HeaderValue {
    let mut c = format!("{}={}; HttpOnly; SameSite=Strict; Path=/", auth::SESSION_COOKIE, value);
    if let Some(a) = max_age {
        c.push_str(&format!("; Max-Age={}", a));
    }
    if secure {
        c.push_str("; Secure");
    }
    HeaderValue::from_str(&c).expect("cookie is ASCII")
}

/// Who the caller is, for `/auth/whoami` and login responses.
fn whoami_json(app: &AppState, p: &Principal) -> Result<serde_json::Value, ApiErr> {
    let links = app.auth.links_of(p).map_err(auth_error)?;
    let projects = app.manager.projects();
    let mut project_roles: Vec<serde_json::Value> = Vec::new();
    let mut host_roles: Vec<String> = Vec::new();
    for l in &links {
        match &l.link.resource {
            Ent::Cluster => host_roles.push(l.link.template.clone()),
            Ent::Project(id) => project_roles.push(serde_json::json!({
                "project": id,
                "project_name": projects.get(id).ok().flatten().map(|p| p.name),
                "role": l.link.template,
                "via": l.link.principal,
            })),
            _ => {}
        }
    }
    host_roles.sort();
    host_roles.dedup();
    Ok(serde_json::json!({
        "user": p.user,
        "token": p.token.as_ref().map(|t| serde_json::json!({ "id": t.id, "name": t.name, "device": t.device, "client": t.client })),
        "method": p.method,
        "teams": p.teams,
        "break_glass": p.is_break_glass(),
        "csrf": p.csrf,
        "host_roles": host_roles,
        "project_roles": project_roles,
        "default_project": p.default_project(),
    }))
}

/// Audit a login outcome (spec §10).
fn audit_login(app: &AppState, p: Option<&Principal>, rid: &str, method: &str, result: &str, who: &str) {
    app.auth.audit(p, rid, "login", None, Some(&format!("user:{}", who)), result, None, &[], serde_json::json!({ "method": method }));
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
        return Err(err(StatusCode::BAD_REQUEST, "invalid", "only \"pam\" logins use this endpoint; OIDC starts at /auth/oidc/start"));
    }
    // Passwords only over TLS or from this host (spec §5.3).
    let local = match listener {
        Listener::Ui | Listener::Api | Listener::Cluster => true,
        Listener::Tcp => app.auth.config.tls.enabled() || addr.is_none_or(|a| a.0 .0.ip().is_loopback()),
    };
    if !local {
        return Err(err(StatusCode::FORBIDDEN, "tls_required", "password login needs TLS"));
    }
    let a = app.clone();
    let username = body.username.clone();
    let password = body.password;
    let result = tokio::task::spawn_blocking(move || a.auth.login_pam(&username, &password))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let user = match result {
        Ok(u) => u,
        Err(e) => {
            audit_login(&app, None, &rid.0, "pam", "denied", &body.username);
            return Err(auth_error(e));
        }
    };
    let (cookie, _) = app.auth.create_session(&user, Method::Pam).map_err(auth_error)?;
    let p = app
        .auth
        .session_principal(&cookie, auth::Transport::Tcp, None)
        .map_err(auth_error)?
        .ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "new session missing"))?;
    audit_login(&app, Some(&p), &rid.0, "pam", "ok", &body.username);
    let mut resp = Json(whoami_json(&app, &p)?).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, session_cookie(&cookie, secure_transport(&app, &headers, listener), None));
    Ok(resp)
}

/// A browser session for a local user identified on api.sock (gxctl
/// `ui`). The session carries the user's stored teams only: break-glass
/// (from Unix groups on the socket) doesn't carry over to a browser.
pub async fn peer_session(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    if c.p.method != Method::Peer {
        return Err(err(StatusCode::FORBIDDEN, "forbidden", "only local users on the glidex socket can open a session this way"));
    }
    let user = c.p.user.clone().ok_or_else(|| err(StatusCode::FORBIDDEN, "forbidden", "no user"))?;
    let (cookie, csrf) = c.auth().create_session(&user, Method::Pam).map_err(auth_error)?;
    c.set_target(format!("user:{}", user.id));
    Ok(Json(serde_json::json!({ "cookie_name": auth::SESSION_COOKIE, "cookie": cookie, "csrf": csrf })))
}

/// The host's identity for client profiles (spec/gxctl-auth.md §7.1):
/// static after start. The fingerprint is public information — the server
/// logs it at every start (security.md §5.1.1) — and lets gxctl tell a
/// genuine untrusted self-signed certificate from a proxy that terminated
/// TLS in front of it.
pub async fn server_info(State(app): State<AppState>) -> impl IntoResponse {
    let fingerprint = match &app.auth.config.tls {
        crate::config::TlsSetting::Mode(crate::config::TlsMode::Off) => None,
        crate::config::TlsSetting::Mode(crate::config::TlsMode::Auto) => {
            glidex_tls::fingerprint(&crate::serve::self_signed_dir().join("cp.crt")).ok()
        }
        crate::config::TlsSetting::Files(f) => glidex_tls::fingerprint(&f.cert).ok(),
    };
    let identity = crate::cluster::identity::Files::beside(app.manager.db_path()).load_identity().ok().flatten();
    let name = app.auth.config.server_name.clone().or_else(glidex_tls::hostname).unwrap_or_else(|| "glidex".into());
    Json(serde_json::json!({
        "cluster_id": identity.as_ref().map(|i| i.cluster_id.clone()),
        "node_id": identity.as_ref().map(|i| i.node_id.clone()),
        "cluster_name": name,
        "version": env!("CARGO_PKG_VERSION"),
        "fingerprint": fingerprint,
        "methods": {
            "pam": app.auth.config.auth.pam.enabled,
            "oidc": app.auth.oidc.enabled(),
            "disabled": app.auth.is_disabled(),
        },
    }))
}

#[derive(Deserialize)]
pub struct PamTokenBody {
    username: String,
    password: Zeroizing<String>,
    #[serde(default)]
    token_name: Option<String>,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    client: Option<String>,
    #[serde(default)]
    days: Option<u64>,
}

/// CLI/CI token login (spec/gxctl-auth.md §7.3): PAM credentials in, a
/// personal bearer token out. Same authd path, rate limits, fixed delay
/// and JIT provisioning as `POST /auth/login`; the answer is a token
/// because the CLI and CI carry tokens while sessions stay the browser's
/// half of §5.5.
pub async fn pam_token(
    State(app): State<AppState>,
    Extension(rid): Extension<RequestId>,
    listener: Option<Extension<Listener>>,
    addr: Option<Extension<ClientAddr>>,
    Json(body): Json<PamTokenBody>,
) -> Result<impl IntoResponse, ApiErr> {
    let listener = listener.map(|l| l.0).unwrap_or(Listener::Tcp);
    if !app.auth.config.auth.pam.enabled {
        return Err(err(StatusCode::UNAUTHORIZED, "unauthenticated", "PAM logins are off: set auth.pam.enabled in control-plane.json"));
    }
    // Passwords only over TLS or from this host (spec §5.3), the same rule as `login`.
    let local = match listener {
        Listener::Ui | Listener::Api | Listener::Cluster => true,
        Listener::Tcp => app.auth.config.tls.enabled() || addr.is_none_or(|a| a.0 .0.ip().is_loopback()),
    };
    if !local {
        return Err(err(StatusCode::FORBIDDEN, "tls_required", "password token logins need TLS"));
    }
    let a = app.clone();
    let username = body.username.clone();
    let password = body.password;
    let result = tokio::task::spawn_blocking(move || a.auth.login_pam(&username, &password))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let user = match result {
        Ok(u) => u,
        Err(e) => {
            audit_login(&app, None, &rid.0, "pam-token", "denied", &body.username);
            return Err(auth_error(e));
        }
    };
    let name = body.token_name.unwrap_or_else(|| "gxctl".into());
    let (secret, token) = app
        .auth
        .create_token(&name, TokenKind::Personal { owner: user.id.clone() }, &user.id, body.days, body.device.as_deref(), body.client.as_deref())
        .map_err(auth_error)?;
    audit_login(&app, None, &rid.0, "pam-token", "ok", &user.id);
    Ok(Json(serde_json::json!({ "token": secret, "token_id": token.id, "expires_at": token.expires_at, "user": user })))
}

pub async fn logout(c: Caller, listener: Option<Extension<Listener>>, headers: HeaderMap) -> Result<Response, ApiErr> {
    c.auth().end_session(&c.p).map_err(auth_error)?;
    let secure = secure_transport(&c.app, &headers, listener.map(|l| l.0).unwrap_or(Listener::Tcp));
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().insert(header::SET_COOKIE, session_cookie("", secure, Some(0)));
    Ok(resp)
}

pub async fn whoami(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Json(whoami_json(&c.app, &c.p)?))
}

#[derive(Deserialize)]
pub struct UpdateMe {
    #[serde(default)]
    default_project: Option<String>,
}

pub async fn update_me(c: Caller, Json(body): Json<UpdateMe>) -> Result<impl IntoResponse, ApiErr> {
    let Some(mut user) = c.p.user.clone() else {
        return Err(err(StatusCode::BAD_REQUEST, "invalid", "service accounts have no profile"));
    };
    if c.p.method == Method::Disabled {
        return Err(err(StatusCode::BAD_REQUEST, "invalid", "authentication is disabled"));
    }
    user.default_project = match body.default_project.as_deref() {
        None | Some("") => None,
        Some(k) => {
            let id = c.target_project(Some(k))?;
            c.require_action("readProject", Ent::Project(id.clone()), super::project_entities(&id), &[])?;
            Some(id)
        }
    };
    c.auth().store.put_user(&user).map_err(store_err)?;
    Ok(Json(user))
}

/// The caller's own SSH public keys from their home directory, read by
/// glidex-authd (the control plane can't read home directories). Only
/// for users with a local (PAM or Unix) account; for prefilling guest
/// login credentials.
pub async fn my_ssh_keys(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    let unavailable = |reason: &str| Json(serde_json::json!({ "available": false, "reason": reason, "keys": [] }));
    let Some(user) = c.p.user.clone() else {
        return Ok(unavailable("service accounts have no home directory"));
    };
    let identities = c.auth().store.identities_of(&user.id).map_err(store_err)?;
    let Some(login) = ["pam", "unix"]
        .iter()
        .find_map(|p| identities.iter().find(|i| i.provider == *p).map(|i| i.subject.clone()))
    else {
        return Ok(unavailable("your account has no local login on this host; paste your public key"));
    };
    let client = glidex_authd::client::AuthdClient::new(&c.auth().config.auth.pam.authd_socket);
    let who = login.clone();
    let result = tokio::task::spawn_blocking(move || client.public_keys(&who))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    Ok(match result {
        Ok(keys) => Json(serde_json::json!({ "available": true, "username": login, "keys": keys })),
        Err(glidex_authd::client::AuthdError::Denied) => unavailable("your local account can't log in to glidex"),
        Err(e) => {
            tracing::warn!("reading public keys for {}: {}", login, e);
            unavailable("your public keys can't be read right now (glidex-authd)")
        }
    })
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

fn oidc_redirect_uri(app: &AppState) -> String {
    app.auth.oidc.redirect_uri(app.auth.config.auth.allowed_origins.first().map(String::as_str))
}

pub async fn oidc_start(
    State(app): State<AppState>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Query(q): Query<OidcStartQuery>,
) -> Result<Response, ApiErr> {
    let start = app
        .auth
        .oidc
        .start(q.return_to.as_deref(), &oidc_redirect_uri(&app), q.reauth)
        .await
        .map_err(oidc_error)?;
    let secure = secure_transport(&app, &headers, listener.map(|l| l.0).unwrap_or(Listener::Tcp));
    let mut resp = Redirect::to(&start.redirect).into_response();
    // Lax: it must come back on the IdP's top-level redirect.
    let mut c = format!("{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=600", OIDC_COOKIE, start.browser);
    if secure {
        c.push_str("; Secure");
    }
    resp.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&c).expect("ascii"));
    Ok(resp)
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

fn oidc_error(e: auth::oidc::OidcError) -> ApiErr {
    use auth::oidc::OidcError as E;
    match e {
        E::Disabled => err(StatusCode::NOT_FOUND, "oidc_disabled", e.to_string()),
        E::NotAllowed(_) => err(StatusCode::FORBIDDEN, "login_not_allowed", e.to_string()),
        E::Pending => err(StatusCode::ACCEPTED, "authorization_pending", e.to_string()),
        E::SlowDown => err(StatusCode::TOO_MANY_REQUESTS, "slow_down", e.to_string()),
        E::Provider(_) => err(StatusCode::BAD_GATEWAY, "idp_error", e.to_string()),
        E::Invalid(_) => err(StatusCode::UNAUTHORIZED, "login_failed", e.to_string()),
    }
}

/// The user for a verified OIDC identity (spec §5.4).
fn oidc_user(app: &AppState, id: &auth::oidc::OidcIdentity) -> Result<User, ApiErr> {
    let cfg = app.auth.oidc.config();
    let provider = format!("oidc:{}", id.issuer);
    let user = app
        .auth
        .store
        .user_for_identity(&provider, &id.subject, &id.display_name, id.email.clone(), cfg.jit)
        .map_err(store_err)?
        .filter(|u| !u.disabled)
        .ok_or_else(|| err(StatusCode::FORBIDDEN, "login_not_allowed", "no glidex account for this identity"))?;
    let teams: Vec<String> = id.groups.iter().filter_map(|g| cfg.group_teams.get(g).cloned()).collect();
    app.auth.store.sync_memberships(&user.id, MemberSource::Oidc, &teams).map_err(store_err)?;
    Ok(user)
}

pub async fn oidc_callback(
    State(app): State<AppState>,
    Extension(rid): Extension<RequestId>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Query(q): Query<OidcCallbackQuery>,
) -> Result<Response, ApiErr> {
    if let Some(e) = q.error {
        return Err(err(StatusCode::UNAUTHORIZED, "login_failed", format!("identity provider: {}", e)));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(err(StatusCode::BAD_REQUEST, "invalid", "code and state are required"));
    };
    let browser = super::cookie(&headers, OIDC_COOKIE);
    let (identity, return_to) = match app.auth.oidc.callback(&code, &state, browser.as_deref(), &oidc_redirect_uri(&app)).await {
        Ok(v) => v,
        Err(e) => {
            audit_login(&app, None, &rid.0, "oidc", "denied", "-");
            return Err(oidc_error(e));
        }
    };
    let user = match oidc_user(&app, &identity) {
        Ok(u) => u,
        Err(e) => {
            audit_login(&app, None, &rid.0, "oidc", "denied", &identity.subject);
            return Err(e);
        }
    };
    let (cookie, _) = app.auth.create_session(&user, Method::Oidc).map_err(auth_error)?;
    let p = app.auth.session_principal(&cookie, auth::Transport::Tcp, None).map_err(auth_error)?;
    audit_login(&app, p.as_ref(), &rid.0, "oidc", "ok", &user.id);
    let secure = secure_transport(&app, &headers, listener.map(|l| l.0).unwrap_or(Listener::Tcp));
    let mut resp = Redirect::to(&return_to).into_response();
    resp.headers_mut().append(header::SET_COOKIE, session_cookie(&cookie, secure, None));
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!("{}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0", OIDC_COOKIE)).expect("ascii"),
    );
    Ok(resp)
}

pub async fn oidc_device_start(State(app): State<AppState>) -> Result<impl IntoResponse, ApiErr> {
    Ok(Json(app.auth.oidc.device_start().await.map_err(oidc_error)?))
}

#[derive(Deserialize)]
pub struct DevicePoll {
    device_code: String,
    #[serde(default)]
    token_name: Option<String>,
    /// What the minting gxctl profile says about its device and build;
    /// stored redacted-for-display, never checked (spec/gxctl-auth.md §7.2).
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    client: Option<String>,
}

/// Lifetime of the token gxctl gets from a device login.
const DEVICE_TOKEN_DAYS: u64 = 1;

pub async fn oidc_device_poll(State(app): State<AppState>, Extension(rid): Extension<RequestId>, Json(body): Json<DevicePoll>) -> Result<Response, ApiErr> {
    let identity = match app.auth.oidc.device_poll(&body.device_code).await {
        Ok(i) => i,
        Err(auth::oidc::OidcError::Pending) => {
            return Ok((StatusCode::ACCEPTED, Json(serde_json::json!({ "status": "pending" }))).into_response())
        }
        Err(e) => return Err(oidc_error(e)),
    };
    let user = oidc_user(&app, &identity)?;
    let name = body.token_name.unwrap_or_else(|| "gxctl".into());
    let (secret, token) = app
        .auth
        .create_token(&name, TokenKind::Personal { owner: user.id.clone() }, &user.id, Some(DEVICE_TOKEN_DAYS), body.device.as_deref(), body.client.as_deref())
        .map_err(auth_error)?;
    audit_login(&app, None, &rid.0, "oidc-device", "ok", &user.id);
    Ok(Json(serde_json::json!({ "status": "ok", "token": secret, "expires_at": token.expires_at, "user": user })).into_response())
}

// ---- capability checks (spec §7.7) ---------------------------------------

#[derive(Deserialize)]
pub struct CheckItem {
    action: String,
    #[serde(default = "host_ent")]
    resource: Ent,
}

fn host_ent() -> Ent {
    Ent::Cluster
}

#[derive(Deserialize)]
pub struct CheckBody {
    checks: Vec<CheckItem>,
}

/// Entities for any resource, loaded from the managers.
async fn resource_entities(c: &Caller, e: &Ent) -> Option<EntitySet> {
    let m = c.manager();
    Some(match e {
        Ent::Cluster => EntitySet::new(),
        // A node's host sits under the cluster: links on the cluster cover it.
        Ent::Host | Ent::Node(_) => {
            let mut es = EntitySet::new();
            es.host(&e.id());
            es
        }
        Ent::Project(id) => {
            m.projects().get(id).ok().flatten()?;
            super::project_entities(id)
        }
        Ent::Vm(id) => vm_entities(&m.get_vm(id).await.ok()?).1,
        Ent::Disk(id) => {
            let d = m.get_disk(id).await.ok()?;
            disk_entities(&d.id, &d.project).1
        }
        Ent::Credential(key) => {
            let (project, username) = key.split_once('/')?;
            credential_entities(project, username).1
        }
        Ent::Network(name) => {
            let n = m.get_network(name).ok()?;
            let mut es = EntitySet::new();
            add_network(&mut es, &n);
            es
        }
        Ent::Image(id) => {
            let mut es = EntitySet::new();
            es.image(id);
            es
        }
        Ent::PciDevice(bdf) => super::vms::pci_entities(c, bdf).1,
        Ent::User(id) => {
            let mut es = EntitySet::new();
            if c.p.user_id() != Some(id) {
                es.user(id, false, &[]);
            }
            es
        }
        Ent::Team(_) | Ent::Token(_) => return None,
    })
}

pub async fn authz_check(c: Caller, Json(body): Json<CheckBody>) -> Result<impl IntoResponse, ApiErr> {
    if body.checks.len() > 200 {
        return Err(err(StatusCode::BAD_REQUEST, "invalid", "at most 200 checks"));
    }
    let schema_actions: Vec<String> = c.auth().engine.schema().actions().map(|a| a.id().unescaped().to_string()).collect();
    let mut out = Vec::with_capacity(body.checks.len());
    for item in &body.checks {
        if !schema_actions.contains(&item.action) {
            out.push(serde_json::json!({ "allowed": false, "error": "unknown action" }));
            continue;
        }
        let allowed = match resource_entities(&c, &item.resource).await {
            Some(es) => c.allowed(&item.action, item.resource.clone(), es),
            None => false,
        };
        out.push(serde_json::json!({ "allowed": allowed }));
    }
    Ok(Json(serde_json::json!({ "results": out })))
}

#[derive(Deserialize)]
pub struct AllowedBody {
    actions: Vec<String>,
    resources: Vec<Ent>,
}

const MAX_ALLOWED_ACTIONS: usize = 50;
const MAX_ALLOWED_RESOURCES: usize = 500;

/// `POST /authz/allowed` (spec/clustering-ui.md §3.5): for each resource,
/// the actions the caller may take on it. An action that doesn't apply to
/// the resource's type (the schema's `appliesTo`) is left out; an unknown
/// action is an error, so a typo in a client fails loudly.
pub async fn authz_allowed(c: Caller, Json(body): Json<AllowedBody>) -> Result<impl IntoResponse, ApiErr> {
    if body.actions.len() > MAX_ALLOWED_ACTIONS || body.resources.len() > MAX_ALLOWED_RESOURCES {
        return Err(err(StatusCode::BAD_REQUEST, "invalid", format!("at most {MAX_ALLOWED_ACTIONS} actions and {MAX_ALLOWED_RESOURCES} resources")));
    }
    let schema = c.auth().engine.schema();
    // action → the resource types it applies to.
    let mut applies: Vec<(&str, Vec<String>)> = Vec::with_capacity(body.actions.len());
    for a in &body.actions {
        let uid: cedar_policy::EntityUid = format!("Glidex::Action::{:?}", a).parse().map_err(|_| err(StatusCode::BAD_REQUEST, "invalid", format!("bad action name {a}")))?;
        let Some(types) = schema.resources_for_action(&uid) else {
            return Err(err(StatusCode::BAD_REQUEST, "invalid", format!("unknown action {a}")));
        };
        applies.push((a.as_str(), types.map(|t| t.basename().to_string()).collect()));
    }
    let mut out = Vec::with_capacity(body.resources.len());
    for r in &body.resources {
        let ty = r.type_name();
        let candidates: Vec<&str> = applies.iter().filter(|(_, ts)| ts.iter().any(|t| t == ty)).map(|(a, _)| *a).collect();
        if candidates.is_empty() {
            out.push(Vec::new());
            continue;
        }
        let ok = match resource_entities(&c, r).await {
            Some(es) => candidates.into_iter().filter(|a| c.allowed(a, r.clone(), es.clone())).map(String::from).collect(),
            None => Vec::new(),
        };
        out.push(ok);
    }
    Ok(Json(serde_json::json!({ "allowed": out })))
}

// ---- tokens --------------------------------------------------------------

#[derive(Deserialize)]
pub struct RoleRef {
    role: String,
    /// Project id or name; absent for host roles.
    #[serde(default)]
    project: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateToken {
    name: String,
    #[serde(default)]
    expires_in_days: Option<u64>,
    /// `personal` (default) or `service_account`.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    project: Option<String>,
    /// Links for the token itself (narrowing a personal token).
    #[serde(default)]
    roles: Vec<RoleRef>,
    /// What the creating client says it is; display only, never checked
    /// (spec/gxctl-auth.md §7.2).
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    client: Option<String>,
}

#[derive(Serialize)]
struct TokenView {
    #[serde(flatten)]
    token: crate::auth::store::Token,
    roles: Vec<crate::auth::store::LinkRecord>,
}

fn self_entities(c: &Caller) -> (Ent, EntitySet) {
    let id = c.p.user_id().unwrap_or_default().to_string();
    (Ent::User(id), EntitySet::new())
}

pub async fn list_tokens(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    let all = c.allowed("manageAnyTokens", Ent::Cluster, EntitySet::new());
    let links = c.auth().store.links().map_err(store_err)?;
    let mut out = Vec::new();
    for (_, t) in c.auth().store.tokens().map_err(store_err)? {
        let visible = all
            || match &t.kind {
                TokenKind::Personal { owner } => c.p.user_id() == Some(owner.as_str()),
                TokenKind::ServiceAccount { project } => {
                    c.allowed("manageServiceTokens", Ent::Project(project.clone()), super::project_entities(project))
                }
            };
        if visible {
            let roles = links.iter().filter(|l| l.link.principal == Ent::Token(t.id.clone())).cloned().collect();
            out.push(TokenView { token: t, roles });
        }
    }
    Ok(Json(out))
}

pub async fn create_token(c: Caller, Json(body): Json<CreateToken>) -> Result<impl IntoResponse, ApiErr> {
    if c.p.token.is_some() {
        return Err(err(StatusCode::FORBIDDEN, "forbidden", "tokens can't create tokens"));
    }
    let by = c.p.user_id().unwrap_or("-").to_string();
    let service = body.kind.as_deref() == Some("service_account");
    let kind = if service {
        let project = c.target_project(body.project.as_deref())?;
        c.set_project(&project);
        c.require_action("manageServiceTokens", Ent::Project(project.clone()), super::project_entities(&project), &[])?;
        for r in &body.roles {
            if !authz::PROJECT_ROLES.contains(&r.role.as_str()) || r.project.as_deref().is_some_and(|p| c.target_project(Some(p)).ok().as_deref() != Some(project.as_str())) {
                return Err(err(StatusCode::BAD_REQUEST, "invalid", "a service account only gets project roles in its own project"));
            }
        }
        TokenKind::ServiceAccount { project }
    } else {
        let (e, es) = self_entities(&c);
        c.require_action("manageOwnTokens", e, es, &[])?;
        TokenKind::Personal { owner: by.clone() }
    };
    let (secret, token) = c.auth().create_token(&body.name, kind.clone(), &by, body.expires_in_days, body.device.as_deref(), body.client.as_deref()).map_err(auth_error)?;
    c.set_target(format!("token:{}", token.id));
    let mut roles = Vec::new();
    for r in &body.roles {
        let resource = match &r.project {
            Some(p) => Ent::Project(c.target_project(Some(p))?),
            None => match &kind {
                TokenKind::ServiceAccount { project } => Ent::Project(project.clone()),
                TokenKind::Personal { .. } => Ent::Cluster,
            },
        };
        match c.auth().add_link(&r.role, Ent::Token(token.id.clone()), resource, &by) {
            Ok(l) => roles.push(l),
            Err(e) => {
                let _ = c.auth().revoke_token(&token.id);
                return Err(auth_error(e));
            }
        }
    }
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "token": secret, "record": TokenView { token, roles } }))))
}

pub async fn revoke_token(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let (_, t) = c
        .auth()
        .store
        .token_by_id(&id)
        .map_err(store_err)?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", "token not found"))?;
    c.set_target(format!("token:{}", t.id));
    let mine = matches!(&t.kind, TokenKind::Personal { owner } if c.p.user_id() == Some(owner.as_str()));
    let ok = mine
        || match &t.kind {
            TokenKind::ServiceAccount { project } => {
                c.allowed("manageServiceTokens", Ent::Project(project.clone()), super::project_entities(project))
            }
            _ => false,
        }
        || c.allowed("manageAnyTokens", Ent::Cluster, EntitySet::new());
    if !ok {
        return Err(err(StatusCode::NOT_FOUND, "not_found", "token not found"));
    }
    c.auth().revoke_token(&id).map_err(auth_error)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- users and teams -----------------------------------------------------

#[derive(Serialize)]
struct UserView {
    #[serde(flatten)]
    user: User,
    identities: Vec<Identity>,
}

pub async fn list_users(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let s = &c.auth().store;
    let out: Vec<UserView> = s
        .users()
        .map_err(store_err)?
        .into_iter()
        .map(|u| UserView { identities: s.identities_of(&u.id).unwrap_or_default(), user: u })
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct IdentityRef {
    provider: String,
    subject: String,
}

#[derive(Deserialize)]
pub struct CreateUser {
    display_name: String,
    #[serde(default)]
    identities: Vec<IdentityRef>,
}

fn valid_provider(p: &str) -> bool {
    p == "pam" || p == "unix" || p.starts_with("oidc:")
}

pub async fn create_user(c: Caller, Json(body): Json<CreateUser>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let s = &c.auth().store;
    for i in &body.identities {
        if !valid_provider(&i.provider) || i.subject.is_empty() {
            return Err(err(StatusCode::BAD_REQUEST, "invalid", "identities are pam:, unix: or oidc:<issuer> with a subject"));
        }
        if s.identity(&i.provider, &i.subject).map_err(store_err)?.is_some() {
            return Err(err(StatusCode::CONFLICT, "conflict", format!("identity {}:{} is already linked", i.provider, i.subject)));
        }
    }
    let u = User {
        id: uuid::Uuid::new_v4().to_string(),
        display_name: body.display_name,
        disabled: false,
        default_project: None,
        created_at: crate::auth::store::now(),
    };
    s.put_user(&u).map_err(store_err)?;
    for i in body.identities {
        s.put_identity(&Identity { provider: i.provider, subject: i.subject, user_id: u.id.clone(), email: None, created_at: u.created_at })
            .map_err(store_err)?;
    }
    c.set_target(format!("user:{}", u.id));
    Ok((StatusCode::CREATED, Json(u)))
}

#[derive(Deserialize)]
pub struct UpdateUser {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    default_project: Option<String>,
}

pub async fn update_user(c: Caller, Path(id): Path<String>, Json(body): Json<UpdateUser>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("user:{}", id));
    let s = &c.auth().store;
    let mut u = s.user(&id).map_err(store_err)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", "user not found"))?;
    if let Some(n) = body.display_name {
        u.display_name = n;
    }
    if let Some(d) = body.default_project {
        u.default_project = if d.is_empty() { None } else { Some(c.target_project(Some(&d))?) };
    }
    if let Some(d) = body.disabled {
        u.disabled = d;
        if d {
            s.remove_sessions_of(&u.id).map_err(store_err)?;
        }
    }
    s.put_user(&u).map_err(store_err)?;
    Ok(Json(u))
}

pub async fn link_identity(c: Caller, Path(id): Path<String>, Json(body): Json<IdentityRef>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("user:{}", id));
    let s = &c.auth().store;
    s.user(&id).map_err(store_err)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", "user not found"))?;
    if !valid_provider(&body.provider) || body.subject.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "invalid", "identities are pam:, unix: or oidc:<issuer> with a subject"));
    }
    if s.identity(&body.provider, &body.subject).map_err(store_err)?.is_some() {
        return Err(err(StatusCode::CONFLICT, "conflict", "identity is already linked"));
    }
    let i = Identity { provider: body.provider, subject: body.subject, user_id: id, email: None, created_at: crate::auth::store::now() };
    s.put_identity(&i).map_err(store_err)?;
    Ok((StatusCode::CREATED, Json(i)))
}

pub async fn unlink_identity(c: Caller, Path((id, key)): Path<(String, String)>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("user:{}", id));
    let s = &c.auth().store;
    let found = s.identities_of(&id).map_err(store_err)?.into_iter().find(|i| Identity::key(&i.provider, &i.subject) == key);
    let Some(i) = found else { return Err(err(StatusCode::NOT_FOUND, "not_found", "identity not found")) };
    s.remove_identity(&i.provider, &i.subject).map_err(store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_teams(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    Ok(Json(c.auth().store.teams().map_err(store_err)?))
}

#[derive(Deserialize)]
pub struct TeamBody {
    name: String,
}

fn valid_team_name(name: &str) -> Result<(), ApiErr> {
    // unix:* teams come only from peer groups (spec §7.3).
    if name.is_empty() || name.len() > 64 || name.starts_with("unix:") {
        return Err(err(StatusCode::BAD_REQUEST, "invalid", "team names are 1-64 characters and can't start with \"unix:\""));
    }
    Ok(())
}

pub async fn create_team(c: Caller, Json(body): Json<TeamBody>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    valid_team_name(&body.name)?;
    let s = &c.auth().store;
    if s.team_by_name(&body.name).map_err(store_err)?.is_some() {
        return Err(err(StatusCode::CONFLICT, "conflict", "team exists"));
    }
    let t = Team { id: uuid::Uuid::new_v4().to_string(), name: body.name, members: vec![], created_at: crate::auth::store::now() };
    s.put_team(&t).map_err(store_err)?;
    c.set_target(format!("team:{}", t.id));
    Ok((StatusCode::CREATED, Json(t)))
}

fn team(c: &Caller, id: &str) -> Result<Team, ApiErr> {
    c.set_target(format!("team:{}", id));
    c.auth().store.team(id).map_err(store_err)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", "team not found"))
}

pub async fn update_team(c: Caller, Path(id): Path<String>, Json(body): Json<TeamBody>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    valid_team_name(&body.name)?;
    let mut t = team(&c, &id)?;
    t.name = body.name;
    c.auth().store.put_team(&t).map_err(store_err)?;
    Ok(Json(t))
}

pub async fn delete_team(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    team(&c, &id)?;
    c.auth().store.remove_team(&id).map_err(store_err)?;
    c.auth().forget_entity(&Ent::Team(id)).map_err(auth_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn add_member(c: Caller, Path((id, user)): Path<(String, String)>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let mut t = team(&c, &id)?;
    c.auth().store.user(&user).map_err(store_err)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", "user not found"))?;
    if !t.members.iter().any(|m| m.user_id == user && m.source == MemberSource::Manual) {
        t.members.push(TeamMember { user_id: user, source: MemberSource::Manual });
        c.auth().store.put_team(&t).map_err(store_err)?;
    }
    Ok(Json(t))
}

pub async fn remove_member(c: Caller, Path((id, user)): Path<(String, String)>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let mut t = team(&c, &id)?;
    t.members.retain(|m| m.user_id != user);
    c.auth().store.put_team(&t).map_err(store_err)?;
    Ok(Json(t))
}

// ---- projects and role links ---------------------------------------------

#[derive(Serialize)]
struct ProjectView {
    #[serde(flatten)]
    project: crate::tenancy::Project,
    usage: crate::tenancy::Usage,
}

pub async fn list_projects(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    let visible = c.visible_projects()?;
    let mut out = Vec::new();
    for p in c.manager().projects().list().map_err(|e| manager_err(e.into()))? {
        if visible.contains(&p.id) && c.allowed("readProject", Ent::Project(p.id.clone()), super::project_entities(&p.id)) {
            let usage = c.manager().usage(&p.id).await.map_err(manager_err)?;
            out.push(ProjectView { project: p, usage });
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct CreateProject {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    quotas: Option<Quotas>,
    /// Users (ids) to make owners.
    #[serde(default)]
    owners: Vec<String>,
}

pub async fn create_project(c: Caller, Json(body): Json<CreateProject>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    // Without explicit quotas a project gets the site default (spec §6.3).
    let quotas = body.quotas.unwrap_or_else(|| c.auth().config.quotas.default.clone());
    let p = c
        .manager()
        .projects()
        .create(&body.name, body.description, Some(quotas))
        .map_err(|e| manager_err(e.into()))?;
    c.set_project(&p.id);
    c.set_target(format!("project:{}", p.id));
    let by = c.p.user_id().unwrap_or("-").to_string();
    for o in body.owners {
        c.auth()
            .add_link("role.owner", Ent::User(o), Ent::Project(p.id.clone()), &by)
            .map_err(auth_error)?;
    }
    Ok((StatusCode::CREATED, Json(p)))
}

fn visible_project(c: &Caller, key: &str) -> Result<crate::tenancy::Project, ApiErr> {
    let p = c
        .manager()
        .projects()
        .resolve(key)
        .map_err(|_| err(StatusCode::NOT_FOUND, "not_found", "project not found"))?;
    c.set_project(&p.id);
    c.set_target(format!("project:{}", p.id));
    c.require_visible("readProject", Ent::Project(p.id.clone()), super::project_entities(&p.id), "project")?;
    Ok(p)
}

pub async fn get_project(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let p = visible_project(&c, &id)?;
    let usage = c.manager().usage(&p.id).await.map_err(manager_err)?;
    Ok(Json(ProjectView { project: p, usage }))
}

#[derive(Deserialize)]
pub struct UpdateProject {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    quotas: Option<Quotas>,
}

pub async fn update_project(c: Caller, Path(id): Path<String>, Json(body): Json<UpdateProject>) -> Result<impl IntoResponse, ApiErr> {
    let mut p = visible_project(&c, &id)?;
    if let Some(d) = body.description {
        p.description = d;
    }
    if let Some(q) = body.quotas {
        c.detail("quotas", serde_json::json!(q));
        p.quotas = q;
    }
    c.manager().projects().put(&p).map_err(|e| manager_err(e.into()))?;
    Ok(Json(p))
}

pub async fn delete_project(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let p = visible_project(&c, &id)?;
    if let Some(what) = c.manager().project_in_use(&p.id).await.map_err(manager_err)? {
        return Err(err(StatusCode::CONFLICT, "conflict", format!("project still owns {}", what)));
    }
    c.manager().projects().delete(&p.id).map_err(|e| manager_err(e.into()))?;
    c.manager().forget_project(&p.id).map_err(manager_err)?;
    c.auth().forget_entity(&Ent::Project(p.id.clone())).map_err(auth_error)?;
    // Service accounts of the project go with it.
    for (_, t) in c.auth().store.tokens().map_err(store_err)? {
        if matches!(&t.kind, TokenKind::ServiceAccount { project } if *project == p.id) {
            c.auth().revoke_token(&t.id).map_err(auth_error)?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct BindingBody {
    role: String,
    principal: Ent,
}

fn check_principal_exists(c: &Caller, e: &Ent) -> Result<(), ApiErr> {
    let s = &c.auth().store;
    let ok = match e {
        Ent::User(id) => s.user(id).map_err(store_err)?.is_some(),
        Ent::Team(id) => id.starts_with("unix:") || s.team(id).map_err(store_err)?.is_some(),
        Ent::Token(id) => s.token_by_id(id).map_err(store_err)?.is_some(),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(err(StatusCode::BAD_REQUEST, "invalid", format!("{} doesn't exist", e)))
    }
}

/// A role link as listed: with its principal's name, so that whoever may
/// read the links sees who they are without listing users or teams
/// (host rights). Only the display name is added.
#[derive(Serialize)]
pub struct BindingView {
    #[serde(flatten)]
    record: auth::store::LinkRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    principal_name: Option<String>,
}

fn with_names(c: &Caller, links: Vec<auth::store::LinkRecord>) -> Result<Vec<BindingView>, ApiErr> {
    let s = &c.auth().store;
    let mut tokens: Option<Vec<(String, auth::store::Token)>> = None;
    links
        .into_iter()
        .map(|record| {
            let principal_name = match &record.link.principal {
                Ent::User(id) => s.user(id).map_err(store_err)?.map(|u| u.display_name),
                Ent::Team(id) if !id.starts_with("unix:") => s.team(id).map_err(store_err)?.map(|t| t.name),
                Ent::Token(id) => {
                    if tokens.is_none() {
                        tokens = Some(s.tokens().map_err(store_err)?);
                    }
                    tokens.as_ref().and_then(|ts| ts.iter().find(|(_, t)| &t.id == id)).map(|(_, t)| t.name.clone())
                }
                _ => None,
            };
            Ok(BindingView { record, principal_name })
        })
        .collect()
}

pub async fn list_bindings(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let p = visible_project(&c, &id)?;
    let links: Vec<_> = c
        .auth()
        .store
        .links()
        .map_err(store_err)?
        .into_iter()
        .filter(|l| l.link.resource == Ent::Project(p.id.clone()))
        .collect();
    Ok(Json(with_names(&c, links)?))
}

pub async fn add_binding(c: Caller, Path(id): Path<String>, Json(body): Json<BindingBody>) -> Result<impl IntoResponse, ApiErr> {
    let p = visible_project(&c, &id)?;
    if !authz::PROJECT_ROLES.contains(&body.role.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid", "only project roles can be given in a project"));
    }
    check_principal_exists(&c, &body.principal)?;
    c.detail("role", serde_json::json!(body.role));
    c.detail("principal", serde_json::json!(body.principal));
    let l = c
        .auth()
        .add_link(&body.role, body.principal, Ent::Project(p.id), c.p.user_id().unwrap_or("-"))
        .map_err(auth_error)?;
    Ok((StatusCode::CREATED, Json(l)))
}

pub async fn remove_binding(c: Caller, Path((id, link)): Path<(String, String)>) -> Result<impl IntoResponse, ApiErr> {
    let p = visible_project(&c, &id)?;
    let found = c.auth().store.links().map_err(store_err)?.into_iter().find(|l| l.link.id == link);
    match found {
        Some(l) if l.link.resource == Ent::Project(p.id.clone()) => {
            c.detail("link", serde_json::json!(l.link));
            c.auth().remove_link(&link).map_err(auth_error)?;
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(err(StatusCode::NOT_FOUND, "not_found", "binding not found")),
    }
}

pub async fn list_system_bindings(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let links: Vec<_> = c.auth().store.links().map_err(store_err)?.into_iter().filter(|l| l.link.resource == Ent::Cluster).collect();
    Ok(Json(with_names(&c, links)?))
}

pub async fn add_system_binding(c: Caller, Json(body): Json<BindingBody>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    if !authz::HOST_ROLES.contains(&body.role.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid", "only host roles and grants are given on the host"));
    }
    check_principal_exists(&c, &body.principal)?;
    c.detail("role", serde_json::json!(body.role));
    c.detail("principal", serde_json::json!(body.principal));
    let l = c.auth().add_link(&body.role, body.principal, Ent::Cluster, c.p.user_id().unwrap_or("-")).map_err(auth_error)?;
    Ok((StatusCode::CREATED, Json(l)))
}

pub async fn remove_system_binding(c: Caller, Path(link): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let found = c.auth().store.links().map_err(store_err)?.into_iter().find(|l| l.link.id == link);
    match found {
        Some(l) if l.link.resource == Ent::Cluster => {
            c.detail("link", serde_json::json!(l.link));
            c.auth().remove_link(&link).map_err(auth_error)?;
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(err(StatusCode::NOT_FOUND, "not_found", "binding not found")),
    }
}

// ---- site policies (spec §7.6) -------------------------------------------

/// Limits on site policies.
const MAX_POLICY_BYTES: usize = 64 * 1024;
const MAX_SITE_POLICIES: usize = 200;

#[derive(Serialize)]
struct PolicyListing {
    policies: Vec<authz::PolicyInfo>,
    site: Vec<crate::auth::store::SitePolicy>,
}

pub async fn list_policies(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    Ok(Json(PolicyListing {
        policies: c.auth().engine.listing(),
        site: c.auth().store.site_policies().map_err(store_err)?,
    }))
}

pub async fn get_policy(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    if let Some(p) = c.auth().store.site_policy(&id).map_err(store_err)? {
        return Ok(Json(serde_json::json!({ "source": "site", "policy": p })));
    }
    match c.auth().engine.listing().into_iter().find(|p| p.id == id) {
        Some(p) => Ok(Json(serde_json::json!({ "source": p.source, "policy": p }))),
        None => Err(err(StatusCode::NOT_FOUND, "not_found", "policy not found")),
    }
}

pub async fn policy_versions(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    Ok(Json(c.auth().store.site_policy_versions(&id).map_err(store_err)?))
}

#[derive(Deserialize, Clone)]
pub struct PolicyChange {
    id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    delete: bool,
}

/// Inputs with `changes` applied to the stored site policies.
fn candidate_inputs(c: &Caller, changes: &[PolicyChange]) -> Result<(Vec<authz::Link>, Vec<SiteSource>), ApiErr> {
    let (links, mut site) = c.auth().policy_inputs().map_err(auth_error)?;
    let stored = c.auth().store.site_policies().map_err(store_err)?;
    for ch in changes {
        if site.iter().any(|s| s.id == ch.id && s.source == authz::PolicySource::File) {
            return Err(err(StatusCode::CONFLICT, "conflict", format!("{} comes from a policy file and can't be changed here", ch.id)));
        }
        site.retain(|s| s.id != ch.id);
        if ch.delete {
            continue;
        }
        let current = stored.iter().find(|p| p.id == ch.id);
        let text = ch.text.clone().or(current.map(|p| p.text.clone())).unwrap_or_default();
        let enabled = ch.enabled.or(current.map(|p| p.enabled)).unwrap_or(true);
        if enabled {
            site.push(SiteSource { id: ch.id.clone(), text, source: authz::PolicySource::Site });
        }
    }
    Ok((links, site))
}

/// Refuse a change that would stop the caller from writing policies
/// (spec §7.6). Break-glass principals can always recover.
fn lock_out_check(c: &Caller, set: &cedar_policy::PolicySet) -> Result<(), ApiErr> {
    if c.p.is_break_glass() {
        return Ok(());
    }
    let d = c.auth().authorize_in(Some(set), &c.p, "writePolicy", Ent::Cluster, EntitySet::new(), &[]);
    if d.allowed {
        Ok(())
    } else {
        Err(err(StatusCode::CONFLICT, "would_lock_out", "after this change you could no longer manage policies"))
    }
}

#[derive(Deserialize)]
pub struct PutPolicy {
    text: String,
    #[serde(default)]
    description: String,
    #[serde(default = "yes")]
    enabled: bool,
    /// The version being replaced; 0 to create.
    version: u64,
}

fn yes() -> bool {
    true
}

pub async fn put_policy(c: Caller, Path(id): Path<String>, Json(body): Json<PutPolicy>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("policy:{}", id));
    if body.text.len() > MAX_POLICY_BYTES {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, "invalid_policy", "a policy is at most 64 KiB"));
    }
    let stored = c.auth().store.site_policies().map_err(store_err)?;
    if body.version == 0 && stored.len() >= MAX_SITE_POLICIES {
        return Err(err(StatusCode::CONFLICT, "conflict", "at most 200 site policies"));
    }
    c.auth().engine.validate_site_policy(&id, &body.text).map_err(|e| auth_error(e.into()))?;
    let change = PolicyChange { id: id.clone(), text: Some(body.text.clone()), description: None, enabled: Some(body.enabled), delete: false };
    let (links, site) = candidate_inputs(&c, &[change])?;
    let set = c.auth().engine.build(&links, &site).map_err(|e| auth_error(e.into()))?;
    lock_out_check(&c, &set)?;
    c.detail("sha256", serde_json::json!(auth::sha256_hex(body.text.as_bytes())));
    let p = c
        .auth()
        .store
        .write_site_policy(&id, body.version, Some((&body.text, &body.description, body.enabled)), c.p.user_id().unwrap_or("-"))
        .map_err(store_err)?;
    c.auth().reload_policies().map_err(auth_error)?;
    c.detail("version", serde_json::json!(p.as_ref().map(|p| p.version)));
    Ok(Json(p))
}

#[derive(Deserialize)]
pub struct VersionQuery {
    version: u64,
}

pub async fn delete_policy(c: Caller, Path(id): Path<String>, Query(q): Query<VersionQuery>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("policy:{}", id));
    let change = PolicyChange { id: id.clone(), text: None, description: None, enabled: None, delete: true };
    let (links, site) = candidate_inputs(&c, &[change])?;
    let set = c.auth().engine.build(&links, &site).map_err(|e| auth_error(e.into()))?;
    lock_out_check(&c, &set)?;
    c.auth().store.write_site_policy(&id, q.version, None, c.p.user_id().unwrap_or("-")).map_err(store_err)?;
    c.auth().reload_policies().map_err(auth_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct ValidateBody {
    id: String,
    text: String,
}

pub async fn validate_policy(c: Caller, Json(body): Json<ValidateBody>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    match c.auth().engine.validate_site_policy(&body.id, &body.text) {
        Ok(()) => Ok(Json(serde_json::json!({ "valid": true }))),
        Err(authz::AuthzError::Validation(errors)) => Ok(Json(serde_json::json!({ "valid": false, "errors": errors }))),
        Err(e) => Ok(Json(serde_json::json!({ "valid": false, "errors": [e.to_string()] }))),
    }
}

#[derive(Deserialize)]
pub struct SimRequest {
    principal: Ent,
    action: String,
    #[serde(default = "host_ent")]
    resource: Ent,
}

#[derive(Deserialize)]
pub struct SimulateBody {
    #[serde(default)]
    changes: Vec<PolicyChange>,
    requests: Vec<SimRequest>,
}

pub async fn simulate_policy(c: Caller, Json(body): Json<SimulateBody>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    if body.requests.len() > 100 {
        return Err(err(StatusCode::BAD_REQUEST, "invalid", "at most 100 requests"));
    }
    for ch in &body.changes {
        if let Some(t) = &ch.text {
            c.auth().engine.validate_site_policy(&ch.id, t).map_err(|e| auth_error(e.into()))?;
        }
        let _ = &ch.description;
    }
    let (links, site) = candidate_inputs(&c, &body.changes)?;
    let candidate = c.auth().engine.build(&links, &site).map_err(|e| auth_error(e.into()))?;
    let (links, site) = c.auth().policy_inputs().map_err(auth_error)?;
    let current = c.auth().engine.build(&links, &site).map_err(|e| auth_error(e.into()))?;
    let mut out = Vec::new();
    for r in &body.requests {
        let Some(p) = c.auth().principal_for(&r.principal).map_err(auth_error)? else {
            out.push(serde_json::json!({ "error": format!("{} not found", r.principal) }));
            continue;
        };
        let Some(es) = resource_entities(&c, &r.resource).await else {
            out.push(serde_json::json!({ "error": format!("{} not found", r.resource) }));
            continue;
        };
        let before = c.auth().authorize_in(Some(&current), &p, &r.action, r.resource.clone(), es.clone(), &[]);
        let after = c.auth().authorize_in(Some(&candidate), &p, &r.action, r.resource.clone(), es, &[]);
        out.push(serde_json::json!({ "current": before, "candidate": after }));
    }
    Ok(Json(serde_json::json!({ "results": out })))
}

pub async fn reload_policies(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.auth().reload_policies().map_err(auth_error)?;
    Ok(StatusCode::NO_CONTENT)
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
    limit: Option<usize>,
}

pub async fn read_audit(c: Caller, Query(q): Query<AuditQuery>) -> Result<impl IntoResponse, ApiErr> {
    let all = c.allowed("readAudit", Ent::Cluster, EntitySet::new());
    let project = match &q.project {
        Some(p) => Some(c.target_project(Some(p))?),
        None => None,
    };
    if !all {
        // Project owners read their project's entries.
        match &project {
            Some(p) => c.require_action("readProjectAudit", Ent::Project(p.clone()), super::project_entities(p), &[])?,
            None => c.require_action("readAudit", Ent::Cluster, EntitySet::new(), &[])?,
        }
    }
    let limit = q.limit.unwrap_or(500).min(5000);
    let entries = c
        .auth()
        .store
        .audit(q.since.unwrap_or(0), limit, |e| {
            project.as_ref().is_none_or(|p| e.project.as_deref() == Some(p.as_str()))
                && q.user.as_ref().is_none_or(|u| e.principal.get("user").and_then(|v| v.as_str()) == Some(u.as_str()))
        })
        .map_err(store_err)?;
    Ok(Json(entries))
}
