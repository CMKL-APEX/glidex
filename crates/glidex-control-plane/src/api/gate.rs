//! Reads and writes in a cluster (spec/clustering.md §6.4).
//!
//! A server that takes a request has already authenticated it. A write on a
//! follower is forwarded, whole, to the leader over :8842 with the
//! authenticated principal in `X-Glidex-Principal`; the leader trusts that
//! header only on a connection from a server certificate. A read first waits
//! until this server has applied everything committed before it arrived (a
//! ReadIndex round trip, batched), unless the caller asks for a local, possibly
//! stale, read with `X-Glidex-Consistency: local`.

use super::{err, is_safe, AppState, Listener};
use crate::auth::{AuthError, Principal};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use bytes::Bytes;
use http_body_util::StreamBody;
use hyper::body::Frame;

pub const PRINCIPAL_HEADER: &str = "x-glidex-principal";
pub const CONSISTENCY_HEADER: &str = "x-glidex-consistency";
pub const REVISION_HEADER: &str = "x-glidex-revision";
const MAX_FORWARDED_BODY: usize = 32 << 20;

/// What an RPC needs to arrive at the leader as it was sent: the
/// protocol's own headers (deadlines, compression, gRPC-Web marker).
const RPC_REQUEST_HEADERS: &[&str] = &[
    "te",
    "grpc-timeout",
    "grpc-encoding",
    "grpc-accept-encoding",
    "connect-protocol-version",
    "connect-timeout-ms",
    "connect-content-encoding",
    "connect-accept-encoding",
    "content-encoding",
    "accept-encoding",
    "x-grpc-web",
];

/// RPC response headers to pass back: a trailers-only gRPC reply carries
/// its status in headers.
const RPC_RESPONSE_HEADERS: &[&str] = &["grpc-status", "grpc-message", "grpc-status-details-bin", "grpc-encoding", "content-encoding", "connect-content-encoding"];

/// Routes that need no cluster at all.
fn needs_no_barrier(path: &str) -> bool {
    path == "/health" || path == "/auth/methods" || path == "/auth/server-info"
}

/// Routes about *this host*: they act on its netd, PCI bus or controllers
/// and are never forwarded.
fn is_host_local(path: &str) -> bool {
    path.starts_with("/ovs") || path.starts_with("/pci-devices") || path.starts_with("/system/") || matches!(path, "/cluster/init" | "/cluster/join" | "/cluster/snapshot" | "/cluster/leave")
}

pub fn principal_header(p: &Principal) -> Option<String> {
    let mut p = p.clone();
    // The CSRF value is checked where the browser spoke to us.
    p.csrf = None;
    serde_json::to_vec(&p).ok().map(|j| base64::engine::general_purpose::STANDARD.encode(j))
}

/// The principal a forwarding server vouches for; `None` for a request that
/// carries none (a login).
pub fn forwarded_principal(headers: &HeaderMap) -> Result<Option<Principal>, AuthError> {
    let Some(v) = headers.get(PRINCIPAL_HEADER) else { return Ok(None) };
    let bytes = base64::engine::general_purpose::STANDARD.decode(v.as_bytes()).map_err(|_| AuthError::Invalid("bad forwarded principal".into()))?;
    serde_json::from_slice(&bytes).map(Some).map_err(|_| AuthError::Invalid("bad forwarded principal".into()))
}

fn unavailable(code: &str, msg: &str) -> Response {
    let mut r = err(StatusCode::SERVICE_UNAVAILABLE, code, msg).into_response();
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    r
}

pub async fn cluster_gate(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let Some(cluster) = app.manager.cluster() else { return next.run(req).await };
    let listener = req.extensions().get::<Listener>().copied().unwrap_or(Listener::Tcp);
    // The leader executing a request a server forwarded, or a node's own host.
    if listener == Listener::Cluster || cluster.node.is_none() {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    if is_host_local(&path) || super::rpc::is_local(&path) || req.headers().get(header::UPGRADE).is_some() {
        return next.run(req).await;
    }
    // An RPC is always a POST: its operation says whether it reads or
    // writes (spec/grpc.md G-D6). Streams are reads served here.
    let read = match glidex_api::operation(&path) {
        Some(op) => op.kind == glidex_api::Kind::Read || op.streaming,
        None => is_safe(req.method()),
    };
    if read {
        if needs_no_barrier(&path) {
            return next.run(req).await;
        }
        let local = req.headers().get(CONSISTENCY_HEADER).and_then(|v| v.to_str().ok()) == Some("local");
        if !local {
            if let Err(e) = cluster.read_barrier().await {
                return unavailable("cluster_unavailable", &format!("no quorum to read through ({e}); retry, or ask for a stale read with X-Glidex-Consistency: local"));
            }
        }
        let mut resp = next.run(req).await;
        if let Ok(v) = HeaderValue::from_str(&app.manager.database().revision().to_string()) {
            resp.headers_mut().insert(REVISION_HEADER, v);
        }
        return resp;
    }
    if cluster.is_leader() {
        return next.run(req).await;
    }
    forward(&app, cluster, req).await
}

async fn forward(_app: &AppState, cluster: std::sync::Arc<crate::cluster::Cluster>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body: Bytes = match axum::body::to_bytes(Body::new(body), MAX_FORWARDED_BODY).await {
        Ok(b) => b,
        Err(_) => return err(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "request body too large").into_response(),
    };
    let pq = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| parts.uri.path().to_string());
    let path = format!("/fwd{pq}");
    let mut headers: Vec<(&str, String)> = Vec::new();
    let rpc = glidex_api::operation(parts.uri.path()).is_some();
    let rpc_headers: &[&str] = if rpc { RPC_REQUEST_HEADERS } else { &[] };
    for name in ["content-type", "accept", "if-match", "x-request-id", "user-agent"].iter().chain(rpc_headers) {
        if let Some(v) = parts.headers.get(*name).and_then(|v| v.to_str().ok()) {
            // HTTP/2 allows no other `te` (RFC 9113 §8.2.2).
            if *name == "te" && v != "trailers" {
                continue;
            }
            headers.push((name, v.to_string()));
        }
    }
    if let Some(p) = parts.extensions.get::<Principal>().and_then(principal_header) {
        headers.push((PRINCIPAL_HEADER, p));
    }
    let listener = parts.extensions.get::<Listener>().copied().unwrap_or(Listener::Tcp);
    let secure = match listener {
        Listener::Tcp => true,
        Listener::Ui | Listener::Relay => parts.headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https"),
        _ => false,
    };
    headers.push(("x-glidex-secure", secure.to_string()));
    if let Some(ip) = parts.extensions.get::<super::ClientAddr>() {
        headers.push(("x-forwarded-for", ip.0.ip().to_string()));
    }

    let mut leader = match cluster.leader_addr() {
        Some(l) => Some(l),
        None => cluster.wait_for_leader(std::time::Duration::from_secs(3)).await,
    };
    for attempt in 0..2 {
        let Some(addr) = leader.clone() else {
            return unavailable("cluster_unavailable", "the cluster has no leader right now");
        };
        match cluster.client.request(&addr, parts.method.clone(), &path, &headers, body.clone()).await {
            Ok(r) if r.status == StatusCode::MISDIRECTED_REQUEST && attempt == 0 => {
                leader = serde_json::from_slice::<serde_json::Value>(&r.body).ok().and_then(|v| v["leader"].as_str().map(String::from));
                continue;
            }
            Ok(r) => {
                let body = match r.trailers {
                    // Native gRPC: the status is in the trailers.
                    Some(t) => Body::new(StreamBody::new(futures_util::stream::iter([
                        Ok::<_, std::convert::Infallible>(Frame::data(r.body)),
                        Ok(Frame::trailers(t)),
                    ]))),
                    None => Body::from(r.body),
                };
                let mut resp = Response::new(body);
                *resp.status_mut() = r.status;
                let names = [header::CONTENT_TYPE, header::SET_COOKIE, header::LOCATION, header::ETAG, header::CACHE_CONTROL, header::HeaderName::from_static("x-request-id")];
                let rpc_names: &[&'static str] = if rpc { RPC_RESPONSE_HEADERS } else { &[] };
                for name in names.into_iter().chain(rpc_names.iter().map(|n| header::HeaderName::from_static(n))) {
                    for v in r.headers.get_all(&name) {
                        resp.headers_mut().append(name.clone(), v.clone());
                    }
                }
                return resp;
            }
            // The write may or may not have happened: say so, don't retry blindly.
            Err(e) => {
                tracing::warn!("forwarding to the leader failed: {}", e);
                let mut r = err(StatusCode::SERVICE_UNAVAILABLE, "leader_changed", "the leader could not be reached; the request may not have been applied: check, then retry").into_response();
                r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
                return r;
            }
        }
    }
    unavailable("cluster_unavailable", "the cluster has no leader right now")
}
