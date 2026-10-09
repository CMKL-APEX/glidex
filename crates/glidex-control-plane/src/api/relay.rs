//! The web UI on an agent (spec/clustering-ui.md §3.6).
//!
//! An agent has no users, sessions or tokens (the auth tables replicate to
//! servers only), so it can't authenticate a browser. Its UI socket relays
//! each request whole to a server over :8842 (`/cluster/v1/ui/…`, the
//! agent's certificate), where it is authenticated as if the browser had
//! come there; the agent never vouches for a principal. Responses stream
//! back (event streams), and WebSocket upgrades are spliced through.
//!
//! A host-local request (`/ovs`, `/pci-devices`, `/system/reconcile`) asked
//! from an agent's UI means the agent's host: the server authorizes the
//! route's action on that node's `Host`, then sends it to the agent
//! (`/cluster/v1/host/…`, a server certificate), which runs it with that one
//! decision (`PreAuthorized`).

use super::{err, ApiErr, AppState, Listener, PreAuthorized, RemoteHost, RouteAction};
use crate::auth::Principal;
use crate::authz::{Ent, EntitySet};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use http_body_util::BodyExt;

pub const VIA_NODE_HEADER: &str = "x-glidex-via-node";
pub const TARGET_NODE_HEADER: &str = "x-glidex-target-node";
pub const AUTHORIZED_HEADER: &str = "x-glidex-authorized";
/// On relayed answers: the node that ran the request.
pub const SERVED_BY_HEADER: &str = "x-glidex-served-by";
const MAX_RELAYED_BODY: usize = 32 << 20;

/// Request headers a relay passes on: what authenticates the browser and
/// describes the body. Nothing that could claim an identity for it.
const PASS_REQUEST: [&str; 13] = [
    "content-type",
    "accept",
    "cookie",
    "authorization",
    "origin",
    "x-glidex-csrf",
    "x-forwarded-for",
    "x-forwarded-proto",
    "user-agent",
    "if-match",
    "x-glidex-consistency",
    "x-request-id",
    "last-event-id",
];

/// Extra request headers of a WebSocket upgrade.
const PASS_UPGRADE: [&str; 6] = ["upgrade", "connection", "sec-websocket-key", "sec-websocket-version", "sec-websocket-protocol", "sec-websocket-extensions"];

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
    path.starts_with("/ovs/") || path == "/pci-devices" || path == "/system/reconcile"
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

/// Global layer, outermost: on an agent, the UI socket's requests go to a server.
pub async fn ui_relay(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let listener = req.extensions().get::<Listener>().copied().unwrap_or(Listener::Tcp);
    let Some(cluster) = app.manager.cluster() else { return next.run(req).await };
    // Servers serve their UI themselves.
    if listener != Listener::Ui || cluster.node.is_some() {
        return next.run(req).await;
    }
    if !super::ui_peer_ok(&app, req.extensions().get::<super::PeerUid>()) {
        return super::plain_error(StatusCode::FORBIDDEN, "forbidden", "only glidex-ui may use this socket");
    }
    let servers = app.manager.node_link().map(|l| l.servers()).unwrap_or_default();
    let server_list = || servers.join(", ");
    if !app.auth.config.cluster.ui_relay {
        return err(StatusCode::MISDIRECTED_REQUEST, "use_a_server", format!("this node doesn't serve the UI; use a server: {}", server_list())).into_response();
    }
    // The Origin is checked here, where the browser connected (the server
    // trusts the relay for it, as for a forwarded request).
    let websocket = req.headers().get(header::UPGRADE).is_some();
    if let Some(o) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if (!super::is_safe(req.method()) || websocket) && !app.auth.config.auth.origin_allowed(o) {
            return super::plain_error(StatusCode::FORBIDDEN, "origin_not_allowed", "this origin may not use the API");
        }
    }
    let path = req.uri().path().to_string();
    let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| path.clone());
    let target = format!("/cluster/v1/ui{pq}");
    let me = cluster.identity.node_id.clone();
    let mut headers = copy_headers(req.headers(), &PASS_REQUEST);
    headers.push((VIA_NODE_HEADER, me.clone()));
    if is_remote_host_path(&path) {
        headers.push((TARGET_NODE_HEADER, me.clone()));
    }
    let unreachable = || err(StatusCode::SERVICE_UNAVAILABLE, "cluster_unavailable", format!("this node can't reach a server; log in on a server: {}", server_list())).into_response();

    if websocket {
        headers.extend(copy_headers(req.headers(), &PASS_UPGRADE));
        let (mut parts, _) = req.into_parts();
        let Some(on) = parts.extensions.remove::<hyper::upgrade::OnUpgrade>() else {
            return err(StatusCode::BAD_REQUEST, "bad_request", "upgrade without a connection to upgrade").into_response();
        };
        for addr in &servers {
            let Ok((status, rh, body, up)) = cluster.client.upgrade_with(addr, parts.method.clone(), &target, &headers).await else { continue };
            let mut resp = Response::new(Body::from(body));
            *resp.status_mut() = status;
            respond_headers(&mut resp, &rh);
            if let Some(mut server) = up {
                resp.headers_mut().insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
                if let Some(u) = rh.get(header::UPGRADE) {
                    resp.headers_mut().insert(header::UPGRADE, u.clone());
                }
                tokio::spawn(async move {
                    let Ok(client) = on.await else { return };
                    let mut client = hyper_util::rt::TokioIo::new(client);
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                });
            }
            return resp;
        }
        return unreachable();
    }

    let (parts, body) = req.into_parts();
    let body: Bytes = match axum::body::to_bytes(body, MAX_RELAYED_BODY).await {
        Ok(b) => b,
        Err(_) => return err(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "request body too large").into_response(),
    };
    for addr in &servers {
        let Ok(r) = cluster.client.request_stream(addr, parts.method.clone(), &target, &headers, body.clone()).await else { continue };
        let (rp, incoming) = r.into_parts();
        let mut resp = Response::new(Body::new(incoming.map_err(axum::Error::new)));
        *resp.status_mut() = rp.status;
        respond_headers(&mut resp, &rp.headers);
        return resp;
    }
    unreachable()
}

/// Per-route layer (inside the audit layer): a relayed host-local request
/// for another node's host is authorized here, on that node's `Host`, and
/// then run there.
pub async fn remote_host(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let Some(RemoteHost(node)) = req.extensions().get::<RemoteHost>().cloned() else { return next.run(req).await };
    let Some(action) = req.extensions().get::<RouteAction>().map(|a| a.0) else { return next.run(req).await };
    match run_on_host(&app, &node, action, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn run_on_host(app: &AppState, node: &str, action: &'static str, req: Request) -> Result<Response, ApiErr> {
    let p = req.extensions().get::<Principal>().cloned().ok_or_else(|| err(StatusCode::UNAUTHORIZED, "unauthenticated", "authentication required"))?;
    let mut es = EntitySet::new();
    es.host(node);
    let d = app.auth.authorize(&p, action, Ent::host_of(node), es, &[]);
    if let Some(slot) = req.extensions().get::<super::AuditSlot>() {
        let mut a = slot.0.lock().unwrap();
        a.policies.extend(d.policies.iter().cloned());
        a.target = Some(format!("node:{node}"));
        a.denied = !d.allowed;
    }
    if !d.allowed {
        return Err(super::deny_response(action, &d));
    }
    let cluster = app.manager.cluster().ok_or_else(|| err(StatusCode::CONFLICT, "not_clustered", "this host is not part of a cluster"))?;
    let addr = app
        .manager
        .nodes()
        .get(node)
        .ok()
        .flatten()
        .and_then(|n| n.status.advertise)
        .map(|a| a.to_string())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", format!("node {node} has no address")))?;
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, MAX_RELAYED_BODY).await.map_err(|_| err(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "request body too large"))?;
    let pq = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| parts.uri.path().to_string());
    let mut headers = copy_headers(&parts.headers, &["content-type", "accept", "x-request-id"]);
    headers.push((AUTHORIZED_HEADER, action.to_string()));
    if let Some(h) = super::gate::principal_header(&p) {
        headers.push((super::gate::PRINCIPAL_HEADER, h));
    }
    let r = cluster
        .client
        .request(&addr, parts.method.clone(), &format!("/cluster/v1/host{pq}"), &headers, body)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, "node_unreachable", format!("node {node}: {e}")))?;
    let mut resp = Response::new(Body::from(r.body));
    *resp.status_mut() = r.status;
    respond_headers(&mut resp, &r.headers);
    Ok(resp)
}

/// On a server, `/cluster/v1/ui/<path>` from a node: the browser request it
/// relayed, made ready for this API. `None` (with the answer) when refused.
pub fn accept_relayed(req: &mut Request, peer_node: &str, self_node: &str) -> Result<(), Response> {
    // A relay can't vouch for anyone.
    if req.headers().contains_key(super::gate::PRINCIPAL_HEADER) {
        return Err(err(StatusCode::FORBIDDEN, "forbidden", "a relayed request may not carry a principal").into_response());
    }
    let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
    let Some(rest) = pq.strip_prefix("/cluster/v1/ui") else { return Err(err(StatusCode::NOT_FOUND, "not_found", "no such route").into_response()) };
    let rest = if rest.is_empty() { "/".to_string() } else { rest.to_string() };
    *req.uri_mut() = rest.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad_request", "bad path").into_response())?;
    // Which node relayed it is what its certificate says.
    if let Ok(v) = HeaderValue::from_str(peer_node) {
        req.headers_mut().insert(VIA_NODE_HEADER, v);
    }
    let target = req.headers().get(TARGET_NODE_HEADER).and_then(|v| v.to_str().ok()).map(String::from);
    req.headers_mut().remove(TARGET_NODE_HEADER);
    if let Some(t) = target {
        // A node may only ask for its own host.
        if t == peer_node && t != self_node && is_remote_host_path(req.uri().path()) {
            req.extensions_mut().insert(RemoteHost(t));
        }
    }
    req.extensions_mut().insert(Listener::Relay);
    req.extensions_mut().insert(super::RelayedBy(peer_node.to_string()));
    Ok(())
}

/// On an agent, `/cluster/v1/host/<path>` from a server: a host-local
/// request it authorized for this host.
pub fn accept_authorized(req: &mut Request) -> Result<(), Response> {
    let Some(action) = req.headers().get(AUTHORIZED_HEADER).and_then(|v| v.to_str().ok()).map(String::from) else {
        return Err(err(StatusCode::BAD_REQUEST, "bad_request", "no authorized action").into_response());
    };
    let pq = req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
    let Some(rest) = pq.strip_prefix("/cluster/v1/host") else { return Err(err(StatusCode::NOT_FOUND, "not_found", "no such route").into_response()) };
    let uri: axum::http::Uri = rest.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "bad_request", "bad path").into_response())?;
    if !is_remote_host_path(uri.path()) {
        return Err(err(StatusCode::FORBIDDEN, "forbidden", "only host-local paths may be sent here").into_response());
    }
    *req.uri_mut() = uri;
    req.extensions_mut().insert(Listener::Cluster);
    req.extensions_mut().insert(PreAuthorized(action));
    Ok(())
}
