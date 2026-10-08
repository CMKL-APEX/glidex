//! The node API on :8842 (spec/clustering.md §4): Raft, joins, read index and
//! forwarded requests. Every route but the CA download and the join request
//! needs a certificate the cluster issued; Raft, forwarding and the CA key
//! need a *server's* (§12.3).

use super::net::PeerCert;
use super::raft::{Rpc, SnapshotHandle};
use super::runtime::Cluster;
use crate::node::{Node, NodeRole, NodeSpec, NodeStatus};
use crate::store::{Origin, TableId};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tower::ServiceExt;

type Ctx = State<Arc<Cluster>>;

pub fn router(c: Arc<Cluster>) -> Router {
    Router::new()
        .route("/cluster/v1/ca", get(get_ca).put(put_ca))
        .route("/cluster/v1/join", post(join))
        .route("/cluster/v1/join/ready", post(join_ready))
        .route("/cluster/v1/read-index", post(read_index))
        .route("/cluster/v1/status", get(status))
        .route("/cluster/v1/raw", post(raw_write))
        .route("/cluster/v1/auth", post(auth_call))
        .route("/raft/append", post(raft_append))
        .route("/raft/vote", post(raft_vote))
        .route("/raft/snapshot", post(raft_snapshot))
        .fallback(forwarded)
        .with_state(c)
}

fn fail(status: StatusCode, code: &str, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": code, "message": msg.into() }))).into_response()
}

/// The caller's certificate, issued by this cluster; `server` insists on a
/// server's.
fn peer(c: &Cluster, p: Option<Extension<PeerCert>>, server: bool) -> Result<PeerCert, Response> {
    let Some(Extension(p)) = p else { return Err(fail(StatusCode::UNAUTHORIZED, "unauthenticated", "a node certificate is required")) };
    if let Err(why) = c.check_peer(&p) {
        return Err(fail(StatusCode::UNAUTHORIZED, "unauthenticated", why));
    }
    if server && !p.server {
        return Err(fail(StatusCode::FORBIDDEN, "forbidden", "only a server may do this"));
    }
    Ok(p)
}

/// Answer for a request that needs the leader and arrived elsewhere.
fn not_leader(c: &Cluster) -> Response {
    let leader = c.leader_addr();
    (StatusCode::MISDIRECTED_REQUEST, Json(json!({ "error": "not_leader", "message": "this server is not the leader", "leader": leader }))).into_response()
}

async fn get_ca(State(c): Ctx) -> Response {
    match c.files.read(c.files.trust()) {
        Ok(t) => (StatusCode::OK, [("content-type", "application/x-pem-file")], t).into_response(),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Deserialize)]
struct CaKey {
    key_pem: String,
}

/// A new server receives the CA key from the leader (D20). It never goes
/// through the Raft log.
async fn put_ca(State(c): Ctx, p: Option<Extension<PeerCert>>, Json(b): Json<CaKey>) -> Response {
    if let Err(r) = peer(&c, p, true) {
        return r;
    }
    if c.role() != NodeRole::Server {
        return fail(StatusCode::CONFLICT, "not_a_server", "agents never hold the CA key");
    }
    let trust = match c.files.read(c.files.trust()) {
        Ok(t) => t,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    };
    // Only a key that matches the cluster's CA is accepted.
    let mut found = None;
    for pem in trust.split_inclusive("-----END CERTIFICATE-----") {
        if let Ok(ca) = super::pki::Ca::from_pem(pem.trim_start(), &b.key_pem) {
            if ca.public_key_fingerprint().ok().as_deref() == Some(c.identity.ca_fingerprint.as_str()) {
                found = Some(ca);
            }
        }
    }
    let Some(ca) = found else { return fail(StatusCode::BAD_REQUEST, "invalid", "that key does not match the cluster CA") };
    if let Err(e) = c.files.save_ca_key(ca.key_pem()) {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string());
    }
    *c.ca.lock().unwrap() = Some(ca);
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Serialize, Deserialize)]
pub struct JoinRequest {
    pub token: String,
    pub csr: String,
    pub name: String,
    pub role: NodeRole,
    pub node_id: String,
    pub raft_id: u64,
    pub advertise: SocketAddr,
    #[serde(default)]
    pub tunnel_ip: Option<IpAddr>,
}

#[derive(Serialize, Deserialize)]
pub struct JoinResponse {
    pub cluster_id: String,
    pub node_id: String,
    pub cert_pem: String,
    pub trust_pem: String,
}

/// §5.2 step 3: check the token, sign the certificate, record the node.
async fn join(State(c): Ctx, Json(req): Json<JoinRequest>) -> Response {
    let Some(node) = &c.node else { return fail(StatusCode::CONFLICT, "not_a_server", "this node does not take joins") };
    if !node.is_leader() {
        return not_leader(&c);
    }
    let Some(ca) = c.signing_ca() else { return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", "this server holds no CA key") };
    let db = c.db.clone();
    let (token, name, role) = (req.token.clone(), req.name.clone(), req.role);
    let result = tokio::task::spawn_blocking(move || -> Result<(), Response> {
        let rec = super::tokens::consume(&db, &token).map_err(|e| fail(StatusCode::FORBIDDEN, "invalid_token", e.to_string()))?;
        match rec.kind {
            super::tokens::TokenKind::Join { role: allowed, .. } if allowed == role => Ok(()),
            super::tokens::TokenKind::Join { role: allowed, .. } => Err(fail(StatusCode::FORBIDDEN, "invalid_token", format!("this token is for {allowed:?} nodes, not {role:?}"))),
            super::tokens::TokenKind::Rejoin { .. } => Err(fail(StatusCode::FORBIDDEN, "invalid_token", "a rejoin token can't be used to join")),
        }?;
        let _ = name;
        Ok(())
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(r)) => return r,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
    let server = req.role == NodeRole::Server;
    let (cert_pem, info) = match ca.sign_node(&req.csr, &req.node_id, server, super::pki::NODE_VALIDITY_DAYS) {
        Ok(v) => v,
        Err(e) => return fail(StatusCode::BAD_REQUEST, "invalid_csr", e.to_string()),
    };
    let db = c.db.clone();
    let cluster_id = c.identity.cluster_id.clone();
    let rec = req;
    let trust_pem = c.files.read(c.files.trust()).unwrap_or_default();
    let out = tokio::task::spawn_blocking(move || {
        let nodes = crate::node::NodeStore::new(db.clone());
        let existing = nodes.list().map_err(|e| e.to_string())?;
        if existing.iter().any(|n| n.meta.id == rec.node_id || (!n.status.phase.is_tombstone() && n.spec.name == rec.name)) {
            return Err(format!("a node named {} or with that id already exists", rec.name));
        }
        if existing.iter().any(|n| n.status.raft_id == Some(rec.raft_id)) {
            return Err("that Raft id is taken".to_string());
        }
        let mut n = Node::new(&rec.node_id, NodeSpec { name: rec.name.clone(), role: rec.role, unschedulable: false, labels: Default::default() });
        n.status.advertise = Some(rec.advertise);
        n.status.tunnel_ip = rec.tunnel_ip.or(Some(rec.advertise.ip()));
        n.status.raft_id = (rec.role == NodeRole::Server).then_some(rec.raft_id);
        db.write(Origin::Api, |tx| -> Result<(), JoinWriteError> {
            tx.open_table(TableId::Nodes.definition())?.insert(&n.meta.id, serde_json::to_vec(&n)?.as_slice())?;
            tx.open_table(TableId::IssuedCerts.definition())?
                .insert(&info.serial, serde_json::to_vec(&json!({ "node": info.node_id, "kind": "node", "issuer": info.issuer_fingerprint, "not_after": info.not_after }))?.as_slice())?;
            Ok(())
        })
        .map_err(|e| e.0)?;
        Ok(rec.node_id)
    })
    .await;
    match out {
        Ok(Ok(node_id)) => Json(JoinResponse { cluster_id, node_id, cert_pem, trust_pem }).into_response(),
        Ok(Err(e)) => fail(StatusCode::CONFLICT, "conflict", e),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

/// §5.2 step 4: the new server's Raft is up; add it as a learner, promote it
/// when that leaves an odd number of voters, and hand it the CA key (D20).
async fn join_ready(State(c): Ctx, p: Option<Extension<PeerCert>>) -> Response {
    let peer = match peer(&c, p, true) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(node) = &c.node else { return fail(StatusCode::CONFLICT, "not_a_server", "this node does not take joins") };
    if !node.is_leader() {
        return not_leader(&c);
    }
    let rec = match crate::node::NodeStore::new(c.db.clone()).get(&peer.node_id) {
        Ok(Some(n)) => n,
        _ => return fail(StatusCode::NOT_FOUND, "not_found", "no such node"),
    };
    let (Some(raft_id), Some(addr)) = (rec.status.raft_id, rec.status.advertise) else { return fail(StatusCode::CONFLICT, "conflict", "the node has no Raft id or address") };
    if let Err(e) = node.add_learner(raft_id, &addr.to_string()).await {
        return fail(StatusCode::SERVICE_UNAVAILABLE, "join_failed", e.to_string());
    }
    // Voters are 1, 3 or 5 (§5.11): a pair of learners is promoted together
    // when that makes an odd number, never one by one.
    let voters = node.voters();
    let learners: Vec<_> = node.learners().into_iter().collect();
    let promoted = if voters.len() % 2 == 1 && learners.len() >= 2 && voters.len() + 2 <= 5 {
        let mut next = voters.clone();
        next.extend(learners.iter().take(2));
        node.set_voters(next).await.is_ok()
    } else {
        false
    };
    if let Some(ca) = c.signing_ca() {
        let r = c.client.request(&addr.to_string(), hyper::Method::PUT, "/cluster/v1/ca", &[("content-type", "application/json".into())], json!({ "key_pem": ca.key_pem() }).to_string().into()).await;
        if let Err(e) = r {
            tracing::warn!(node = %peer.node_id, "could not send the CA key: {}", e);
        }
    }
    Json(json!({ "promoted": promoted, "voters": node.voters().len(), "learners": node.learners().len() })).into_response()
}

async fn read_index(State(c): Ctx, p: Option<Extension<PeerCert>>) -> Response {
    if let Err(r) = peer(&c, p, true) {
        return r;
    }
    let Some(node) = &c.node else { return fail(StatusCode::CONFLICT, "not_a_server", "not a server") };
    if !node.is_leader() {
        return not_leader(&c);
    }
    match node.raft.get_read_log_id().await {
        Ok((read, _)) => Json(json!({ "index": read.map(|l| l.index).unwrap_or(0) })).into_response(),
        Err(_) => not_leader(&c),
    }
}

async fn status(State(c): Ctx, p: Option<Extension<PeerCert>>) -> Response {
    if let Err(r) = peer(&c, p, false) {
        return r;
    }
    Json(super::manage::status_of(&c)).into_response()
}

async fn raft_append(State(c): Ctx, p: Option<Extension<PeerCert>>, body: bytes::Bytes) -> Response {
    raft_rpc(c, p, Rpc::Append, body).await
}

async fn raft_vote(State(c): Ctx, p: Option<Extension<PeerCert>>, body: bytes::Bytes) -> Response {
    raft_rpc(c, p, Rpc::Vote, body).await
}

async fn raft_rpc(c: Arc<Cluster>, p: Option<Extension<PeerCert>>, rpc: Rpc, body: bytes::Bytes) -> Response {
    if let Err(r) = peer(&c, p, true) {
        return r;
    }
    let Some(node) = &c.node else { return fail(StatusCode::CONFLICT, "not_a_server", "not a server") };
    match node.service.handle(rpc, &body).await {
        Ok(out) => (StatusCode::OK, [("content-type", "application/json")], out).into_response(),
        Err(e) => fail(StatusCode::BAD_REQUEST, "bad_request", e.to_string()),
    }
}

/// The snapshot arrives as a stream: it is written to a file, never held in
/// memory (§6.2).
async fn raft_snapshot(State(c): Ctx, p: Option<Extension<PeerCert>>, headers: HeaderMap, req: Request) -> Response {
    use base64::Engine;
    use http_body_util::BodyExt;
    use tokio::io::AsyncWriteExt;
    if let Err(r) = peer(&c, p, true) {
        return r;
    }
    let Some(node) = &c.node else { return fail(StatusCode::CONFLICT, "not_a_server", "not a server") };
    let Some(h) = headers.get("x-glidex-snapshot").and_then(|v| v.to_str().ok()).and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok()) else {
        return fail(StatusCode::BAD_REQUEST, "bad_request", "missing snapshot header");
    };
    let dir = c.files.raft().join("snapshots");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string());
    }
    let path = dir.join(format!("recv-{}.snap", super::pki::hex(&rand8())));
    let write = async {
        let mut f = tokio::fs::File::create(&path).await?;
        let mut body = req.into_body();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(std::io::Error::other)?;
            if let Some(d) = frame.data_ref() {
                f.write_all(d).await?;
            }
        }
        f.sync_all().await
    };
    if let Err(e) = write.await {
        let _ = tokio::fs::remove_file(&path).await;
        return fail(StatusCode::BAD_REQUEST, "bad_request", e.to_string());
    }
    let _ = SnapshotHandle::default();
    match node.service.handle_snapshot(&h, path.clone()).await {
        Ok(out) => (StatusCode::OK, [("content-type", "application/json")], out).into_response(),
        Err(e) => {
            let _ = tokio::fs::remove_file(&path).await;
            fail(StatusCode::BAD_REQUEST, "bad_request", e.to_string())
        }
    }
}

fn rand8() -> [u8; 8] {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    b
}

/// `/fwd/<path>`: an API request a follower authenticated and forwarded
/// (§6.4). Only a server may do it; the original principal travels in
/// `X-Glidex-Principal` and is trusted on that basis alone.
async fn forwarded(State(c): Ctx, p: Option<Extension<PeerCert>>, mut req: Request) -> Response {
    let peer = match peer(&c, p, true) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(path) = req.uri().path().strip_prefix("/fwd") else { return fail(StatusCode::NOT_FOUND, "not_found", "no such route") };
    if !c.node.as_ref().is_some_and(|n| n.is_leader()) {
        return not_leader(&c);
    }
    let Some(api) = c.api.get() else { return fail(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "the API is not up yet") };
    let pq = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };
    *req.uri_mut() = match pq.parse() {
        Ok(u) => u,
        Err(_) => return fail(StatusCode::BAD_REQUEST, "bad_request", "bad path"),
    };
    req.extensions_mut().insert(crate::api::Listener::Cluster);
    req.extensions_mut().insert(crate::api::ForwardedBy(peer.node_id));
    match api.clone().oneshot(req).await {
        Ok(r) => r.map(Body::new),
        Err(never) => match never {},
    }
}

/// Any error of recording a joining node, as text.
struct JoinWriteError(String);

macro_rules! join_err {
    ($($t:ty),*) => {$(impl From<$t> for JoinWriteError { fn from(e: $t) -> Self { JoinWriteError(e.to_string()) } })*};
}
join_err!(crate::store::StoreError, redb::TableError, redb::StorageError, serde_json::Error);

/// Tables a follower may ask the leader to write to directly: what
/// authentication touches (sessions, tokens, users, identities, teams) and
/// the audit log.
const RAW_TABLES: [TableId; 6] = [TableId::Sessions, TableId::ApiTokens, TableId::Audit, TableId::Users, TableId::Identities, TableId::Teams];

async fn raw_write(State(c): Ctx, p: Option<Extension<PeerCert>>, Json(ops): Json<Vec<crate::store::Op>>) -> Response {
    if let Err(r) = peer(&c, p, true) {
        return r;
    }
    if !c.node.as_ref().is_some_and(|n| n.is_leader()) {
        return not_leader(&c);
    }
    if let Some(op) = ops.iter().find(|o| !RAW_TABLES.contains(&o.table())) {
        return fail(StatusCode::FORBIDDEN, "forbidden", format!("a server may not write {} this way", op.table().name()));
    }
    let db = c.db.clone();
    let r = tokio::task::spawn_blocking(move || {
        db.write(Origin::Auth, |tx| -> Result<(), JoinWriteError> {
            for op in &ops {
                let key = std::str::from_utf8(op.key()).map_err(|e| JoinWriteError(e.to_string()))?;
                let mut t = tx.open_table(op.table().definition())?;
                match op {
                    crate::store::Op::Put { value, .. } => t.insert(key, value)?,
                    crate::store::Op::Delete { .. } => {
                        t.remove(key)?;
                    }
                }
            }
            Ok(())
        })
    })
    .await;
    match r {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.0),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}

#[derive(Deserialize)]
struct AuthCall {
    op: String,
    args: serde_json::Value,
}

/// Named identity operations a follower can't do itself.
async fn auth_call(State(c): Ctx, p: Option<Extension<PeerCert>>, Json(call): Json<AuthCall>) -> Response {
    if let Err(r) = peer(&c, p, true) {
        return r;
    }
    if !c.node.as_ref().is_some_and(|n| n.is_leader()) {
        return not_leader(&c);
    }
    let db = c.db.clone();
    let r = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        match call.op.as_str() {
            "identity" => {
                let a = &call.args;
                let s = |k: &str| a[k].as_str().unwrap_or("").to_string();
                let store = crate::auth::store::IdentityStore::new(db).map_err(|e| e.to_string())?;
                let u = store.user_for_identity(&s("provider"), &s("subject"), &s("display_name"), a["email"].as_str().map(String::from), true).map_err(|e| e.to_string())?;
                serde_json::to_value(u).map_err(|e| e.to_string())
            }
            other => Err(format!("unknown operation {other}")),
        }
    })
    .await;
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => fail(StatusCode::BAD_REQUEST, "bad_request", e),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}
