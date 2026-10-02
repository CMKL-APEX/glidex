//! REST API (spec/rest-api.md) with authentication and Cedar
//! authorization (spec/security.md §5, §7).
//!
//! Every route is declared once in [`routes`] together with the Cedar
//! action it maps to. A per-route layer hands that action to the handler
//! (through [`Caller`]) and audits the request; a global layer
//! authenticates it. Handlers ask Cedar before touching anything.

#![allow(clippy::result_large_err)] // handlers return (StatusCode, Json<ApiError>), as before

mod access;
mod errors;
mod net;
mod storage;
mod vms;

pub use errors::{error_to_response, ApiErr};

use crate::auth::{self, AuthError, AuthService, Method, Principal, Transport};
use crate::authz::{Decision, Ent, EntitySet};
use crate::models::ApiError;
use crate::state::VmManager;
use crate::tenancy::QuotaMode;
use axum::{
    extract::{FromRequestParts, Request, State},
    http::{header, request::Parts, HeaderMap, HeaderValue, Method as HttpMethod, StatusCode},
    middleware::{from_fn_with_state, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put, MethodRouter},
    Extension, Json, Router,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

pub struct App {
    pub manager: Arc<VmManager>,
    pub auth: Arc<AuthService>,
}

pub type AppState = Arc<App>;

/// Which listener a request came in on; set by the listener (main.rs).
/// Absent means a plain TCP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listener {
    Tcp,
    /// `api.sock`: local users, identified by peer uid.
    Api,
    /// `ui.sock`: glidex-ui proxying browsers.
    Ui,
}

/// Peer uid of a Unix-socket connection.
#[derive(Debug, Clone, Copy)]
pub struct PeerUid(pub u32);

/// Remote address of a TCP connection.
#[derive(Debug, Clone, Copy)]
pub struct ClientAddr(pub SocketAddr);

#[derive(Debug, Clone)]
pub struct RequestId(pub String);

/// The Cedar action a route maps to (set per route).
#[derive(Debug, Clone, Copy)]
pub struct RouteAction(pub &'static str);

/// What a handler learned for the audit entry.
#[derive(Debug, Default)]
struct AuditInfo {
    policies: Vec<String>,
    project: Option<String>,
    target: Option<String>,
    details: serde_json::Map<String, serde_json::Value>,
    denied: bool,
}

#[derive(Debug, Clone, Default)]
struct AuditSlot(Arc<Mutex<AuditInfo>>);

/// Route actions that aren't Cedar actions.
pub const PUBLIC: &str = "public";
/// Any authenticated principal; the handler scopes the result itself.
pub const AUTHENTICATED: &str = "authenticated";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteSpec {
    pub method: &'static str,
    pub path: &'static str,
    pub action: &'static str,
}

/// Router with authentication off: every request is the break-glass
/// system user. For embedding and tests.
pub fn create_router(manager: Arc<VmManager>) -> Router {
    let auth = AuthService::disabled(manager.database()).expect("auth store");
    router(Arc::new(App { manager, auth }))
}

pub fn router(app: AppState) -> Router {
    let (r, _) = routes(&app);
    r.layer(from_fn_with_state(app.clone(), authenticate)).with_state(app)
}

/// Every route with its action, for tests and documentation.
pub fn route_table(app: &AppState) -> Vec<RouteSpec> {
    routes(app).1
}

struct Routes<'a> {
    app: &'a AppState,
    router: Router<AppState>,
    table: Vec<RouteSpec>,
}

impl Routes<'_> {
    fn add(mut self, method: &'static str, path: &'static str, action: &'static str, mr: MethodRouter<AppState>) -> Self {
        self.table.push(RouteSpec { method, path, action });
        let mr: MethodRouter<AppState> = mr
            .layer::<_, std::convert::Infallible>(from_fn_with_state(self.app.clone(), audit_layer))
            .layer::<_, std::convert::Infallible>(Extension(RouteAction(action)));
        self.router = self.router.route(path, mr);
        self
    }
}

fn routes(app: &AppState) -> (Router<AppState>, Vec<RouteSpec>) {
    let r = Routes { app, router: Router::new(), table: Vec::new() }
        // ---- VMs
        .add("GET", "/vms", "listVms", get(vms::list))
        .add("POST", "/vms", "createVm", post(vms::create))
        .add("GET", "/vms/{id}", "readVm", get(vms::get_one))
        .add("DELETE", "/vms/{id}", "deleteVm", delete(vms::delete_one))
        .add("POST", "/vms/{id}/start", "startVm", post(vms::start))
        .add("POST", "/vms/{id}/stop", "stopVm", post(vms::stop))
        .add("POST", "/vms/{id}/pause", "pauseVm", post(vms::pause))
        .add("GET", "/vms/{id}/console", "getConsoleInfo", get(vms::console_info))
        .add("POST", "/vms/{id}/console/ticket", "openConsole", post(vms::console_ticket))
        .add("GET", "/vms/{id}/console/ws", "openConsole", get(vms::console_ws))
        .add("GET", "/vms/{id}/console/log", "openConsole", get(vms::console_log))
        .add("POST", "/vms/{id}/devices", "attachDevice", post(vms::attach_device))
        .add("DELETE", "/vms/{id}/devices", "detachDevice", delete(vms::detach_device))
        .add("POST", "/vms/{id}/disks", "attachDisk", post(vms::attach_disk))
        .add("DELETE", "/vms/{id}/disks/{disk}", "detachDisk", delete(vms::detach_disk))
        // ---- guest credentials
        .add("GET", "/credentials", "listCredentials", get(storage::list_credentials))
        .add("POST", "/credentials", "createCredential", post(storage::create_credential))
        .add("GET", "/credentials/{username}", "readCredential", get(storage::get_credential))
        .add("PUT", "/credentials/{username}", "updateCredential", put(storage::update_credential))
        .add("DELETE", "/credentials/{username}", "deleteCredential", delete(storage::delete_credential))
        // ---- images and disks
        .add("GET", "/images/catalog", "readImage", get(storage::image_catalog))
        .add("GET", "/images", "readImage", get(storage::list_images))
        .add("POST", "/images", "pullImage", post(storage::pull_image))
        .add("GET", "/images/{id}", "readImage", get(storage::get_image))
        .add("DELETE", "/images/{id}", "deleteImage", delete(storage::delete_image))
        .add("GET", "/disks", "listDisks", get(storage::list_disks))
        .add("POST", "/disks", "createDisk", post(storage::create_disk))
        .add("GET", "/disks/{id}", "readDisk", get(storage::get_disk))
        .add("DELETE", "/disks/{id}", "deleteDisk", delete(storage::delete_disk))
        .add("POST", "/disks/{id}/resize", "resizeDisk", post(storage::resize_disk))
        .add("POST", "/disks/{id}/extend-root", "extendRoot", post(storage::extend_root))
        // ---- networks
        .add("GET", "/networks", "readNetwork", get(net::list_networks))
        .add("POST", "/networks", "createNetwork", post(net::create_network))
        .add("GET", "/networks/{name}", "readNetwork", get(net::get_network))
        .add("DELETE", "/networks/{name}", "deleteNetwork", delete(net::delete_network))
        .add("PUT", "/networks/{name}/grants", "grantNetwork", put(net::grant_network))
        .add("POST", "/networks/{name}/shares", "offerNetworkShare", post(net::offer_share))
        .add("DELETE", "/networks/{name}/shares/{project}", "unshareNetwork", delete(net::unshare))
        .add("POST", "/projects/{id}/networks", "createProjectNetwork", post(net::create_project_network))
        .add("GET", "/projects/{id}/network-shares", "listNetworkShares", get(net::list_shares))
        .add("POST", "/projects/{id}/network-shares/{network}/accept", "acceptNetworkShare", post(net::accept_share))
        .add("DELETE", "/projects/{id}/network-shares/{network}", "leaveNetworkShare", delete(net::leave_share))
        // ---- host networking
        .add("GET", "/ovs/status", "readOvsStatus", get(net::ovs_status))
        .add("POST", "/ovs/install", "installOvs", post(net::ovs_install))
        .add("POST", "/ovs/dpdk-init", "initDpdk", post(net::ovs_dpdk_init))
        .add("GET", "/ovs/bridges", "listBridges", get(net::list_bridges))
        .add("POST", "/ovs/bridges", "createBridge", post(net::create_bridge))
        .add("DELETE", "/ovs/bridges/{name}", "deleteBridge", delete(net::delete_bridge))
        .add("GET", "/ovs/bridges/{name}/uplinks", "listUplinks", get(net::list_uplinks))
        .add("POST", "/ovs/bridges/{name}/uplinks", "ensureUplink", post(net::create_uplink))
        .add("DELETE", "/ovs/bridges/{name}/uplinks/{uplink}", "deleteUplink", delete(net::delete_uplink))
        .add("POST", "/ovs/bridges/{name}/uplinks/{uplink}/commit", "commitUplink", post(net::commit_uplink))
        .add("GET", "/pci-devices", "listPciDevices", get(net::list_pci_devices))
        // ---- authentication
        .add("GET", "/health", PUBLIC, get(health_check))
        .add("GET", "/auth/methods", PUBLIC, get(access::methods))
        .add("POST", "/auth/login", PUBLIC, post(access::login))
        .add("GET", "/auth/oidc/start", PUBLIC, get(access::oidc_start))
        .add("GET", "/auth/oidc/callback", PUBLIC, get(access::oidc_callback))
        .add("POST", "/auth/oidc/device", PUBLIC, post(access::oidc_device_start))
        .add("POST", "/auth/oidc/device/poll", PUBLIC, post(access::oidc_device_poll))
        .add("POST", "/auth/logout", AUTHENTICATED, post(access::logout))
        .add("POST", "/auth/session", AUTHENTICATED, post(access::peer_session))
        .add("GET", "/auth/whoami", AUTHENTICATED, get(access::whoami))
        .add("POST", "/authz/check", AUTHENTICATED, post(access::authz_check))
        // ---- tokens
        .add("GET", "/tokens", AUTHENTICATED, get(access::list_tokens))
        .add("POST", "/tokens", AUTHENTICATED, post(access::create_token))
        .add("DELETE", "/tokens/{id}", AUTHENTICATED, delete(access::revoke_token))
        // ---- users and teams
        .add("GET", "/users", "listUsers", get(access::list_users))
        .add("GET", "/users/me", AUTHENTICATED, get(access::whoami))
        .add("PATCH", "/users/me", AUTHENTICATED, axum::routing::patch(access::update_me))
        .add("POST", "/users", "manageUsers", post(access::create_user))
        .add("PATCH", "/users/{id}", "manageUsers", axum::routing::patch(access::update_user))
        .add("POST", "/users/{id}/identities", "manageUsers", post(access::link_identity))
        .add("DELETE", "/users/{id}/identities/{key}", "manageUsers", delete(access::unlink_identity))
        .add("GET", "/teams", "listTeams", get(access::list_teams))
        .add("POST", "/teams", "manageTeams", post(access::create_team))
        .add("PATCH", "/teams/{id}", "manageTeams", axum::routing::patch(access::update_team))
        .add("DELETE", "/teams/{id}", "manageTeams", delete(access::delete_team))
        .add("PUT", "/teams/{id}/members/{user}", "manageTeams", put(access::add_member))
        .add("DELETE", "/teams/{id}/members/{user}", "manageTeams", delete(access::remove_member))
        // ---- projects and role links
        .add("GET", "/projects", AUTHENTICATED, get(access::list_projects))
        .add("POST", "/projects", "createProject", post(access::create_project))
        .add("GET", "/projects/{id}", "readProject", get(access::get_project))
        .add("PATCH", "/projects/{id}", "updateProject", axum::routing::patch(access::update_project))
        .add("DELETE", "/projects/{id}", "deleteProject", delete(access::delete_project))
        .add("GET", "/projects/{id}/bindings", "readBindings", get(access::list_bindings))
        .add("POST", "/projects/{id}/bindings", "manageBindings", post(access::add_binding))
        .add("DELETE", "/projects/{id}/bindings/{link}", "manageBindings", delete(access::remove_binding))
        .add("GET", "/system/bindings", "readSystemBindings", get(access::list_system_bindings))
        .add("POST", "/system/bindings", "manageSystemBindings", post(access::add_system_binding))
        .add("DELETE", "/system/bindings/{link}", "manageSystemBindings", delete(access::remove_system_binding))
        // ---- policies and audit
        .add("GET", "/authz/policies", "readPolicy", get(access::list_policies))
        .add("GET", "/authz/policies/{id}", "readPolicy", get(access::get_policy))
        .add("GET", "/authz/policies/{id}/versions", "readPolicy", get(access::policy_versions))
        .add("PUT", "/authz/policies/{id}", "writePolicy", put(access::put_policy))
        .add("DELETE", "/authz/policies/{id}", "deletePolicy", delete(access::delete_policy))
        .add("POST", "/authz/validate", "validatePolicy", post(access::validate_policy))
        .add("POST", "/authz/simulate", "simulatePolicy", post(access::simulate_policy))
        .add("POST", "/authz/reload", "reloadPolicies", post(access::reload_policies))
        .add("GET", "/audit", AUTHENTICATED, get(access::read_audit));
    (r.router, r.table)
}

async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

pub(crate) fn err(status: StatusCode, code: &str, message: impl Into<String>) -> ApiErr {
    (status, Json(ApiError::new(code, message)))
}

fn plain_error(status: StatusCode, code: &str, message: &str) -> Response {
    err(status, code, message).into_response()
}

fn is_safe(m: &HttpMethod) -> bool {
    matches!(*m, HttpMethod::GET | HttpMethod::HEAD | HttpMethod::OPTIONS)
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim().to_string())
}

/// Client IP: the TCP peer, or (only from glidex-ui) X-Forwarded-For.
fn client_ip(parts_ext: &axum::http::Extensions, headers: &HeaderMap, listener: Listener) -> Option<IpAddr> {
    match listener {
        Listener::Ui => headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse().ok()),
        Listener::Tcp => parts_ext.get::<ClientAddr>().map(|a| a.0.ip()),
        Listener::Api => None,
    }
}

/// Global layer: request id, Origin check, and the principal.
async fn authenticate(State(app): State<AppState>, mut req: Request, next: Next) -> Response {
    let request_id = auth::random_token(12);
    req.extensions_mut().insert(RequestId(request_id.clone()));
    let listener = req.extensions().get::<Listener>().copied().unwrap_or(Listener::Tcp);
    let headers = req.headers().clone();
    let safe = is_safe(req.method());
    let websocket = headers.get(header::UPGRADE).is_some();

    // Origin (spec §5.6): browsers send it on cross-origin requests and
    // on every WebSocket upgrade.
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(String::from);
    if let Some(o) = &origin {
        if (!safe || websocket) && !app.auth.config.auth.allowed_origins.iter().any(|a| a == o) {
            return plain_error(StatusCode::FORBIDDEN, "origin_not_allowed", "this origin may not use the API");
        }
    }

    let principal = if app.auth.is_disabled() {
        Some(Principal::system())
    } else {
        let resolved = match listener {
            Listener::Api => match req.extensions().get::<PeerUid>() {
                Some(PeerUid(uid)) => match app.auth.peer_principal(*uid) {
                    Ok(Some(p)) => Ok(Some(p)),
                    Ok(None) => {
                        return plain_error(StatusCode::FORBIDDEN, "forbidden", "this user may not use the glidex socket");
                    }
                    Err(e) => Err(e),
                },
                None => Ok(None),
            },
            Listener::Ui | Listener::Tcp => {
                if listener == Listener::Ui && !ui_peer_ok(&app, req.extensions().get::<PeerUid>()) {
                    return plain_error(StatusCode::FORBIDDEN, "forbidden", "only glidex-ui may use this socket");
                }
                let ip = client_ip(req.extensions(), &headers, listener);
                if let Some(token) = bearer(&headers) {
                    match app.auth.token_principal(&token, Transport::Tcp, ip) {
                        Ok(Some(p)) => Ok(Some(p)),
                        Ok(None) => return plain_error(StatusCode::UNAUTHORIZED, "unauthenticated", "invalid or expired token"),
                        Err(e) => Err(e),
                    }
                } else if let Some(c) = cookie(&headers, auth::SESSION_COOKIE) {
                    match app.auth.session_principal(&c, Transport::Tcp, ip) {
                        Ok(Some(p)) => {
                            // Cookie-authenticated writes need a same-origin
                            // request carrying the CSRF value.
                            if !safe {
                                let csrf = headers.get(auth::CSRF_HEADER).and_then(|v| v.to_str().ok());
                                let ok = origin.is_some()
                                    && csrf.is_some_and(|c| p.csrf.as_deref().is_some_and(|s| auth::constant_eq(c, s)));
                                if !ok {
                                    return plain_error(StatusCode::FORBIDDEN, "csrf_failed", "missing or wrong CSRF header or Origin");
                                }
                            }
                            Ok(Some(p))
                        }
                        Ok(None) => Ok(None),
                        Err(e) => Err(e),
                    }
                } else {
                    Ok(None)
                }
            }
        };
        match resolved {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("authentication error: {}", e);
                return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "authentication failed");
            }
        }
    };
    if let Some(p) = principal {
        req.extensions_mut().insert(p);
    }
    let mut resp = next.run(req).await;
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        resp.headers_mut().insert("x-request-id", v);
    }
    resp
}

fn ui_peer_ok(app: &App, peer: Option<&PeerUid>) -> bool {
    let Some(PeerUid(uid)) = peer else { return false };
    nix::unistd::User::from_name(&app.auth.config.ui_user)
        .ok()
        .flatten()
        .is_some_and(|u| u.uid.as_raw() == *uid)
}

/// Per-route layer: audit writes, and denials of any request.
async fn audit_layer(State(app): State<AppState>, mut req: Request, next: Next) -> Response {
    let slot = AuditSlot::default();
    req.extensions_mut().insert(slot.clone());
    let action = req.extensions().get::<RouteAction>().map(|a| a.0).unwrap_or("-");
    let principal = req.extensions().get::<Principal>().cloned();
    let request_id = req.extensions().get::<RequestId>().map(|r| r.0.clone()).unwrap_or_default();
    let safe = is_safe(req.method());
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    let resp = next.run(req).await;
    let info = slot.0.lock().unwrap();
    let status = resp.status();
    let audited = !safe || info.denied;
    // Public routes (logins) are audited by their handlers.
    if audited && action != PUBLIC && !app.auth.is_disabled() {
        let result = if info.denied {
            "denied".to_string()
        } else if status.is_success() {
            "ok".to_string()
        } else {
            format!("error:{}", status.as_u16())
        };
        let mut details = info.details.clone();
        details.insert("http".into(), serde_json::json!(format!("{} {}", method, path)));
        app.auth.audit(
            principal.as_ref(),
            &request_id,
            action,
            info.project.as_deref(),
            info.target.as_deref(),
            &result,
            (!status.is_success()).then(|| status.as_str()),
            &info.policies,
            serde_json::Value::Object(details),
        );
    }
    drop(info);
    resp
}

/// The authenticated caller of a route.
pub struct Caller {
    pub p: Principal,
    pub action: &'static str,
    pub app: AppState,
    pub request_id: String,
    audit: AuditSlot,
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Self::Rejection> {
        let p = parts
            .extensions
            .get::<Principal>()
            .cloned()
            .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "unauthenticated", "authentication required"))?;
        Ok(Caller {
            p,
            action: parts.extensions.get::<RouteAction>().map(|a| a.0).unwrap_or(AUTHENTICATED),
            app: app.clone(),
            request_id: parts.extensions.get::<RequestId>().map(|r| r.0.clone()).unwrap_or_default(),
            audit: parts.extensions.get::<AuditSlot>().cloned().unwrap_or_default(),
        })
    }
}

/// `401 reauth_required` for step-up, else `403`.
fn deny_response(action: &str, d: &Decision) -> ApiErr {
    if d.denied_by("base.step-up") {
        return err(
            StatusCode::UNAUTHORIZED,
            "reauth_required",
            "this action needs a recent login; log in again and retry",
        );
    }
    err(StatusCode::FORBIDDEN, "forbidden", format!("not allowed: {}", action))
}

impl Caller {
    pub fn manager(&self) -> &VmManager {
        &self.app.manager
    }

    pub fn auth(&self) -> &AuthService {
        &self.app.auth
    }

    /// Ask Cedar, recording the decision for the audit entry.
    pub fn decide(&self, action: &str, resource: Ent, es: EntitySet, extra: &[(&'static str, Ent)]) -> Decision {
        let d = self.app.auth.authorize(&self.p, action, resource, es, extra);
        let mut a = self.audit.0.lock().unwrap();
        for p in &d.policies {
            if !a.policies.contains(p) {
                a.policies.push(p.clone());
            }
        }
        if !d.allowed {
            a.denied = true;
        }
        d
    }

    pub fn allowed(&self, action: &str, resource: Ent, es: EntitySet) -> bool {
        self.app.auth.authorize(&self.p, action, resource, es, &[]).allowed
    }

    pub fn require_action(&self, action: &str, resource: Ent, es: EntitySet, extra: &[(&'static str, Ent)]) -> Result<(), ApiErr> {
        let d = self.decide(action, resource, es, extra);
        if d.allowed {
            Ok(())
        } else {
            Err(deny_response(action, &d))
        }
    }

    /// The route's own action on `resource`.
    pub fn require(&self, resource: Ent, es: EntitySet) -> Result<(), ApiErr> {
        self.require_action(self.action, resource, es, &[])
    }

    /// Like `require`, but a resource the caller can't even `read` is
    /// reported as not found (spec §7.4).
    pub fn require_visible(&self, read_action: &str, resource: Ent, es: EntitySet, what: &str) -> Result<(), ApiErr> {
        if read_action != self.action && !self.allowed(read_action, resource.clone(), es.clone()) {
            self.audit.0.lock().unwrap().denied = true;
            return Err(err(StatusCode::NOT_FOUND, "not_found", format!("{} not found", what)));
        }
        let d = self.decide(self.action, resource, es, &[]);
        if d.allowed {
            Ok(())
        } else if read_action == self.action {
            Err(err(StatusCode::NOT_FOUND, "not_found", format!("{} not found", what)))
        } else {
            Err(deny_response(self.action, &d))
        }
    }

    pub fn set_project(&self, project: &str) {
        self.audit.0.lock().unwrap().project = Some(project.to_string());
    }

    pub fn set_target(&self, target: impl Into<String>) {
        self.audit.0.lock().unwrap().target = Some(target.into());
    }

    pub fn detail(&self, key: &str, value: serde_json::Value) {
        self.audit.0.lock().unwrap().details.insert(key.into(), value);
    }

    /// The project a new resource goes into: the requested one (id or
    /// name), else the caller's default project, else the only project
    /// the caller has a link in.
    pub fn target_project(&self, requested: Option<&str>) -> Result<String, ApiErr> {
        let projects = self.manager().projects();
        let resolve = |k: &str| {
            projects
                .resolve(k)
                .map(|p| p.id)
                .map_err(|_| err(StatusCode::NOT_FOUND, "not_found", format!("project {} not found", k)))
        };
        if let Some(r) = requested {
            return resolve(r);
        }
        if let Some(d) = self.p.default_project() {
            return resolve(d);
        }
        if self.p.method == Method::Disabled || self.p.is_break_glass() {
            return Ok(self.manager().default_project_id());
        }
        match self.app.auth.linked_projects(&self.p).map_err(auth_error)? {
            auth::LinkedProjects::Some(v) if v.len() == 1 => Ok(v[0].clone()),
            _ => Err(err(
                StatusCode::BAD_REQUEST,
                "project_required",
                "name a project (\"project\" field or ?project=); you have no default project",
            )),
        }
    }

    /// Whether the caller may go over `project`'s quotas.
    pub fn quota_mode(&self, project: &str) -> QuotaMode {
        let mut es = EntitySet::new();
        es.project(project);
        if self.allowed("exceedQuota", Ent::Project(project.into()), es) {
            QuotaMode::MayExceed
        } else {
            QuotaMode::Enforce
        }
    }

    /// Record quota limits a request went over (spec §6.3).
    pub fn note_overruns(&self, over: &[crate::tenancy::QuotaOverrun]) {
        if !over.is_empty() {
            self.detail("quota_exceeded", serde_json::json!(over));
            let d = self.app.auth.authorize(&self.p, "exceedQuota", Ent::Host, EntitySet::new(), &[]);
            let mut a = self.audit.0.lock().unwrap();
            a.policies.extend(d.policies);
        }
    }

    /// Projects whose resources the caller may list.
    pub fn visible_projects(&self) -> Result<auth::LinkedProjects, ApiErr> {
        self.app.auth.linked_projects(&self.p).map_err(auth_error)
    }
}

pub(crate) fn auth_error(e: AuthError) -> ApiErr {
    match e {
        AuthError::Unauthenticated => err(StatusCode::UNAUTHORIZED, "unauthenticated", e.to_string()),
        AuthError::Denied => err(StatusCode::UNAUTHORIZED, "login_failed", e.to_string()),
        AuthError::RateLimited => err(StatusCode::TOO_MANY_REQUESTS, "rate_limited", e.to_string()),
        AuthError::Unavailable(_) => err(StatusCode::SERVICE_UNAVAILABLE, "unavailable", e.to_string()),
        AuthError::Invalid(_) => err(StatusCode::BAD_REQUEST, "invalid", e.to_string()),
        AuthError::Store(crate::auth::store::StoreError::NotFound(_)) => err(StatusCode::NOT_FOUND, "not_found", e.to_string()),
        AuthError::Store(crate::auth::store::StoreError::AlreadyExists(_))
        | AuthError::Store(crate::auth::store::StoreError::Conflict(_)) => err(StatusCode::CONFLICT, "conflict", e.to_string()),
        AuthError::Store(crate::auth::store::StoreError::Invalid(_)) => err(StatusCode::BAD_REQUEST, "invalid", e.to_string()),
        AuthError::Store(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "persistence_error", e.to_string()),
        AuthError::Authz(crate::authz::AuthzError::Validation(v)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError::new("invalid_policy", "policy validation failed").with_details(serde_json::json!({ "errors": v }))),
        ),
        AuthError::Authz(e) => err(StatusCode::UNPROCESSABLE_ENTITY, "invalid_policy", e.to_string()),
    }
}

// ---- entities for Cedar ---------------------------------------------------

pub(crate) fn project_entities(project: &str) -> EntitySet {
    let mut es = EntitySet::new();
    es.project(project);
    es
}

pub(crate) fn vm_entities(vm: &crate::models::Vm) -> (Ent, EntitySet) {
    let mut es = EntitySet::new();
    let e = Ent::Vm(vm.id.clone());
    es.in_project(e.clone(), &vm.project);
    (e, es)
}

pub(crate) fn disk_entities(id: &str, project: &str) -> (Ent, EntitySet) {
    let mut es = EntitySet::new();
    let e = Ent::Disk(id.to_string());
    es.in_project(e.clone(), project);
    (e, es)
}

pub(crate) fn credential_ent(project: &str, username: &str) -> Ent {
    Ent::Credential(format!("{}/{}", project, username))
}

pub(crate) fn credential_entities(project: &str, username: &str) -> (Ent, EntitySet) {
    let mut es = EntitySet::new();
    let e = credential_ent(project, username);
    es.in_project(e.clone(), project);
    (e, es)
}

pub(crate) fn add_network(es: &mut EntitySet, n: &crate::network::Network) -> Ent {
    es.network(&n.name, n.project.as_deref(), n.all_projects, &n.grants, &n.shares);
    Ent::Network(n.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_maps_to_a_schema_action() {
        let dir = tempfile::TempDir::new().unwrap();
        let manager = VmManager::with_db_path(dir.path().join("t.db")).unwrap();
        let auth = AuthService::disabled(manager.database()).unwrap();
        let app = Arc::new(App { manager, auth });
        let schema = app.auth.engine.schema();
        let actions: Vec<String> = schema.actions().map(|a| a.id().unescaped().to_string()).collect();
        let table = route_table(&app);
        assert!(table.len() > 80);
        for r in &table {
            if r.action == PUBLIC || r.action == AUTHENTICATED {
                continue;
            }
            assert!(actions.iter().any(|a| a == r.action), "{} {} → unknown action {}", r.method, r.path, r.action);
            // Groups and roles are never requested directly.
            assert!(!r.action.contains('.') && !r.action.starts_with("role"), "{} {}", r.method, r.path);
        }
        let public: Vec<_> = table.iter().filter(|r| r.action == PUBLIC).map(|r| r.path).collect();
        assert_eq!(
            public,
            ["/health", "/auth/methods", "/auth/login", "/auth/oidc/start", "/auth/oidc/callback", "/auth/oidc/device", "/auth/oidc/device/poll"]
        );
    }

    #[test]
    fn cookie_and_bearer_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; gx_session=abc; b=2".parse().unwrap());
        assert_eq!(cookie(&h, "gx_session").as_deref(), Some("abc"));
        assert_eq!(cookie(&h, "nope"), None);
        h.insert(header::AUTHORIZATION, "Bearer gxt_x".parse().unwrap());
        assert_eq!(bearer(&h).as_deref(), Some("gxt_x"));
    }
}
