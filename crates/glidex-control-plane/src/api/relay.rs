//! Host-only requests across nodes, and the web UI on agents
//! (spec/28-host-page.md, spec/16-clustering-ui.md §3.6).
//!
//! A host-only request (`/ovs`, `/pci-devices`, `/system/reconcile`, …)
//! means this host, unless it names another node in `X-Glidex-Target-Node`
//! (W2): a server then authorizes the route's action on that node's `Host`
//! and sends it there (`/cluster/v1/host/…`, a server certificate), where
//! it runs with that one decision (`PreAuthorized`).
//!
//! An agent has no users, sessions or tokens (the auth tables replicate to
//! servers only), so its glidex-ui sends browsers to a server (W5), and its
//! UI socket answers API requests with `421 use_a_server`. Servers still
//! take requests that agents of an older build relay (`/cluster/v1/ui/…`,
//! W6): authenticated as if the browser had come there, the agent never
//! vouching for a principal.

use super::{ApiErr, AppState, Listener, PreAuthorized, RemoteHost, RouteAction, err};
use crate::auth::Principal;
use crate::authz::{Ent, EntitySet};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub const VIA_NODE_HEADER: &str = "x-glidex-via-node";
pub const TARGET_NODE_HEADER: &str = "x-glidex-target-node";
pub const AUTHORIZED_HEADER: &str = "x-glidex-authorized";
/// On relayed answers: the node that ran the request.
pub const SERVED_BY_HEADER: &str = "x-glidex-served-by";
const MAX_RELAYED_BODY: usize = 32 << 20;

/// Response headers that describe the answer (hop-by-hop ones are left out).
const PASS_RESPONSE: [&str; 12] = [
    "content-type",
    "set-cookie",
    "location",
    "etag",
    "cache-control",
    "content-disposition",
    "retry-after",
    "x-request-id",
    "x-glidex-revision",
    "x-glidex-served-by",
    "sec-websocket-accept",
    "sec-websocket-protocol",
];

/// Paths about one host that an agent's UI means for the agent itself.
/// (`/system/bindings` are the cluster's role links, not the host's.)
pub fn is_remote_host_path(path: &str) -> bool {
    // `leave` is this host leaving, not the server it is relayed to.
    path.starts_with("/ovs/")
        // The host family's RPCs (spec/18-grpc.md §8).
        || path.starts_with("/glidex.host.v1.")
        || path == "/pci-devices"
        || path == "/host/topology"
        || path == "/system/reconcile"
        || path == "/cluster/leave"
        || path == "/glidex.cluster.v1.ClusterService/LeaveCluster"
}

fn copy_headers(from: &HeaderMap, names: &[&'static str]) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for &n in names {
        for v in from.get_all(n) {
            if let Ok(s) = v.to_str() {
                out.push((n, s.to_string()));
            }
        }
    }
    out
}

fn respond_headers(resp: &mut Response, from: &HeaderMap) {
    for n in PASS_RESPONSE {
        let name = HeaderName::from_static(n);
        for v in from.get_all(&name) {
            resp.headers_mut().append(name.clone(), v.clone());
        }
    }
}

/// Global layer, outermost. On every host: glidex-ui's own `PUT /ui/self`
/// on the UI socket (§3 of spec/28-host-page.md), and the target-node header
/// of host-only requests (W2). On an agent, the UI socket's API requests
/// get `421 use_a_server`: its glidex-ui sends browsers to a server (W5).
pub async fn ui_relay(State(app): State<AppState>, mut req: Request, next: Next) -> Response {
    let listener = req
        .extensions()
        .get::<Listener>()
        .copied()
        .unwrap_or(Listener::Tcp);
    let own = req.uri().path() == UI_SELF_PATH;
    if listener == Listener::Ui && (own || app.manager.is_agent()) {
        if !super::ui_peer_ok(&app, req.extensions().get::<super::PeerUid>()) {
            return super::plain_error(
                StatusCode::FORBIDDEN,
                "forbidden",
                "only glidex-ui may use this socket",
            );
        }
        if own {
            return ui_self(&app, req).await;
        }
        return use_a_server(&app);
    }
    if matches!(listener, Listener::Ui | Listener::Tcp | Listener::Api)
        && let Err(r) = take_target(&app, &mut req)
    {
        return r;
    }
    next.run(req).await
}

/// glidex-ui's report of itself (spec/28-host-page.md §3). glidex-ui never
/// proxies a browser to it (it answers `/api/ui/…` itself).
pub const UI_SELF_PATH: &str = "/ui/self";

#[derive(serde::Deserialize, Default)]
struct UiSelf {
    #[serde(default)]
    url: Option<String>,
}

async fn ui_self(app: &AppState, req: Request) -> Response {
    if req.method() != axum::http::Method::PUT {
        return super::plain_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "PUT only",
        );
    }
    let body = axum::body::to_bytes(req.into_body(), 64 << 10)
        .await
        .unwrap_or_default();
    let me: UiSelf = serde_json::from_slice(&body).unwrap_or_default();
    let reported = me
        .url
        .as_deref()
        .and_then(crate::cluster::config::ui_origin);
    let cfg = &app.auth.config;
    let custom = cfg.cluster.ui_origin();
    let agent = app.manager.is_agent();
    if !agent {
        // W3: the custom address, else what glidex-ui reports, else the
        // first allowed origin.
        let published = custom
            .clone()
            .or(reported)
            .or_else(|| cfg.auth.allowed_origins.first().cloned());
        if app.manager.set_ui_url(published) {
            let m = app.manager.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = m.report_self() {
                    tracing::warn!("publishing the web UI's address: {}", e);
                }
            });
        }
    }
    let servers = app
        .manager
        .node_link()
        .map(|l| l.servers())
        .unwrap_or_default();
    let mut out = serde_json::json!({ "servers": servers });
    if agent {
        if let Some(url) = server_ui(app) {
            out["redirect"] = url.into();
        }
        out["home"] = format!("/hosts/{}", app.manager.local_node_id()).into();
    } else if let Some(o) = custom {
        out["extra_hosts"] = serde_json::json!([o.trim_start_matches("https://")]);
    }
    axum::Json(out).into_response()
}

/// The web UI an agent sends browsers to: the published address of the
/// server its node link reached last, then of the others (W5).
pub fn server_ui(app: &AppState) -> Option<String> {
    let nodes = app.manager.nodes().list().ok()?;
    let servers: Vec<_> = nodes
        .iter()
        .filter(|n| n.spec.role == crate::node::NodeRole::Server && !n.status.phase.is_tombstone())
        .collect();
    let order = app
        .manager
        .node_link()
        .map(|l| l.servers())
        .unwrap_or_default();
    order
        .iter()
        .filter_map(|a| {
            servers
                .iter()
                .find(|n| n.status.advertise.is_some_and(|x| x.to_string() == *a))
        })
        .chain(servers.iter())
        .find_map(|n| n.status.ui_url.clone())
}

/// An agent's answer to an API request on its UI socket (W5).
fn use_a_server(app: &AppState) -> Response {
    let servers = app
        .manager
        .node_link()
        .map(|l| l.servers())
        .unwrap_or_default();
    let location = server_ui(app);
    let message = match &location {
        Some(u) => format!("this node doesn't serve the web UI; use {u}"),
        None => format!(
            "this node doesn't serve the web UI; use a server: {}",
            servers.join(", ")
        ),
    };
    let body = super::errors::ApiError::new("use_a_server", message)
        .with_details(serde_json::json!({ "location": location, "servers": servers }));
    (StatusCode::MISDIRECTED_REQUEST, axum::Json(body)).into_response()
}

/// W2: `X-Glidex-Target-Node` on a request that came to this host. Absent
/// or naming this host: the request runs here. Naming another node, on a
/// server and a host-only path: authorized on that node's `Host`, then run
/// there (`remote_host`). Anything else is `400 bad_target_node`.
fn take_target(app: &AppState, req: &mut Request) -> Result<(), Response> {
    let Some(v) = req.headers_mut().remove(TARGET_NODE_HEADER) else {
        return Ok(());
    };
    let bad = |m: String| err(StatusCode::BAD_REQUEST, "bad_target_node", m).into_response();
    let t = v
        .to_str()
        .map_err(|_| bad("the target node is not text".into()))?
        .to_string();
    if !is_remote_host_path(req.uri().path()) {
        return Err(bad(format!(
            "{} is not about one host; it takes no target node",
            req.uri().path()
        )));
    }
    if t == app.manager.local_node_id() {
        return Ok(());
    }
    let server = app.manager.cluster().is_some_and(|c| c.node.is_some());
    if !server {
        return Err(bad(format!("this host can't act on node {t}")));
    }
    let known = app
        .manager
        .nodes()
        .get(&t)
        .ok()
        .flatten()
        .is_some_and(|n| !n.status.phase.is_tombstone());
    if !known {
        return Err(bad(format!("no node {t} in this cluster")));
    }
    req.extensions_mut().insert(RemoteHost(t));
    Ok(())
}

/// Per-route layer (inside the audit layer): a relayed host-local request
/// for another node's host is authorized here, on that node's `Host`, and
/// then run there.
pub async fn remote_host(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let Some(RemoteHost(node)) = req.extensions().get::<RemoteHost>().cloned() else {
        return next.run(req).await;
    };
    // A REST route's action, or an RPC's from the operation table.
    let action = req
        .extensions()
        .get::<RouteAction>()
        .map(|a| a.0)
        .or_else(|| glidex_api::operation(req.uri().path()).map(|o| o.action));
    let Some(action) = action else {
        return next.run(req).await;
    };
    match run_on_host(&app, &node, action, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn run_on_host(
    app: &AppState,
    node: &str,
    action: &'static str,
    req: Request,
) -> Result<Response, ApiErr> {
    let p = req
        .extensions()
        .get::<Principal>()
        .cloned()
        .ok_or_else(|| {
            err(
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "authentication required",
            )
        })?;
    let mut es = EntitySet::new();
    es.host(node);
    let d = app
        .auth
        .authorize(&p, action, Ent::host_of(node), es.clone(), &[]);
    if let Some(slot) = req.extensions().get::<super::AuditSlot>() {
        let mut a = slot.0.lock().unwrap();
        a.policies.extend(d.policies.iter().cloned());
        a.target = Some(format!("node:{node}"));
        a.denied = !d.allowed;
    }
    if !d.allowed {
        return Err(super::deny_response(action, &d));
    }
    let cluster = app.manager.cluster().ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "not_clustered",
            "this host is not part of a cluster",
        )
    })?;
    let addr = app
        .manager
        .nodes()
        .get(node)
        .ok()
        .flatten()
        .and_then(|n| n.status.advertise)
        .map(|a| a.to_string())
        .ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("node {node} has no address"),
            )
        })?;
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, MAX_RELAYED_BODY)
        .await
        .map_err(|_| {
            err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                "request body too large",
            )
        })?;
    let pq = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    let mut headers = copy_headers(&parts.headers, &["content-type", "accept", "x-request-id"]);
    if glidex_api::operation(parts.uri.path()).is_some() {
        headers.extend(copy_headers(
            &parts.headers,
            super::gate::RPC_REQUEST_HEADERS,
        ));
    }
    // Some handlers ask for a second action on the host (a confirmed uplink
    // moves its IP: `confirmUplink`); pass those the caller holds as well.
    let mut authorized = vec![action.to_string()];
    for extra in related_actions(action) {
        if app
            .auth
            .authorize(&p, extra, Ent::host_of(node), es.clone(), &[])
            .allowed
        {
            authorized.push(extra.to_string());
        }
    }
    headers.push((AUTHORIZED_HEADER, authorized.join(",")));
    if let Some(h) = super::gate::principal_header(&p) {
        headers.push((super::gate::PRINCIPAL_HEADER, h));
    }
    let r = cluster
        .client
        .request(
            &addr,
            parts.method.clone(),
            &format!("/cluster/v1/host{pq}"),
            &headers,
            body,
        )
        .await
        .map_err(|e| {
            err(
                StatusCode::BAD_GATEWAY,
                "node_unreachable",
                format!("node {node}: {e}"),
            )
        })?;
    let mut resp = Response::new(Body::from(r.body));
    *resp.status_mut() = r.status;
    respond_headers(&mut resp, &r.headers);
    Ok(resp)
}

/// Actions a route's handler may check besides the route's own.
fn related_actions(action: &str) -> &'static [&'static str] {
    match action {
        "ensureUplink" => &["confirmUplink"],
        _ => &[],
    }
}

/// On a server, `/cluster/v1/ui/<path>` from a node: the browser request it
/// relayed, made ready for this API. `None` (with the answer) when refused.
pub fn accept_relayed(req: &mut Request, peer_node: &str, self_node: &str) -> Result<(), Response> {
    // A relay can't vouch for anyone.
    if req.headers().contains_key(super::gate::PRINCIPAL_HEADER) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "a relayed request may not carry a principal",
        )
        .into_response());
    }
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let Some(rest) = pq.strip_prefix("/cluster/v1/ui") else {
        return Err(err(StatusCode::NOT_FOUND, "not_found", "no such route").into_response());
    };
    let rest = if rest.is_empty() {
        "/".to_string()
    } else {
        rest.to_string()
    };
    *req.uri_mut() = rest
        .parse()
        .map_err(|_| err(StatusCode::BAD_REQUEST, "bad_request", "bad path").into_response())?;
    // Which node relayed it is what its certificate says.
    if let Ok(v) = HeaderValue::from_str(peer_node) {
        req.headers_mut().insert(VIA_NODE_HEADER, v);
    }
    let target = req
        .headers()
        .get(TARGET_NODE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    req.headers_mut().remove(TARGET_NODE_HEADER);
    if let Some(t) = target {
        // A node may only ask for its own host.
        if t == peer_node && t != self_node && is_remote_host_path(req.uri().path()) {
            req.extensions_mut().insert(RemoteHost(t));
        }
    }
    req.extensions_mut().insert(Listener::Relay);
    req.extensions_mut()
        .insert(super::RelayedBy(peer_node.to_string()));
    Ok(())
}

/// On an agent, `/cluster/v1/host/<path>` from a server: a host-local
/// request it authorized for this host.
pub fn accept_authorized(req: &mut Request) -> Result<(), Response> {
    let Some(action) = req
        .headers()
        .get(AUTHORIZED_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
    else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "no authorized action",
        )
        .into_response());
    };
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let Some(rest) = pq.strip_prefix("/cluster/v1/host") else {
        return Err(err(StatusCode::NOT_FOUND, "not_found", "no such route").into_response());
    };
    let uri: axum::http::Uri = rest
        .parse()
        .map_err(|_| err(StatusCode::BAD_REQUEST, "bad_request", "bad path").into_response())?;
    if !is_remote_host_path(uri.path()) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only host-local paths may be sent here",
        )
        .into_response());
    }
    *req.uri_mut() = uri;
    req.extensions_mut().insert(Listener::Cluster);
    req.extensions_mut().insert(PreAuthorized(
        action
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(String::from)
            .collect(),
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_paths_are_the_hosts_own_and_role_links_are_the_clusters() {
        for p in [
            "/ovs/status",
            "/ovs/bridges/br0/uplinks",
            "/pci-devices",
            "/system/reconcile",
            "/cluster/leave",
            "/glidex.host.v1.HostNetworkService/ListBridges",
            "/glidex.host.v1.PciService/ListPciDevices",
        ] {
            assert!(is_remote_host_path(p), "{p}");
        }
        for p in [
            "/system/bindings",
            "/vms",
            "/cluster/status",
            "/cluster/dissolve",
            "/ovsx",
            "/glidex.network.v1.NetworkService/ListNetworks",
        ] {
            assert!(!is_remote_host_path(p), "{p}");
        }
    }
}
