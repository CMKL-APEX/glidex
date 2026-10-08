//! `/cluster/*`: forming, joining and inspecting the cluster (spec/clustering.md
//! §5). `init`, `join` and `snapshot` act on the host that receives them.

use super::{err, ApiErr, Caller};
use crate::authz::{Ent, EntitySet};
use crate::cluster::manage;
use crate::node::NodeRole;
use axum::{http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::json;

fn cluster_err(e: crate::cluster::ClusterError) -> ApiErr {
    err(StatusCode::CONFLICT, "cluster_error", e.to_string())
}

fn parse_addr(what: &str, v: &Option<String>) -> Result<Option<std::net::SocketAddr>, ApiErr> {
    match v {
        None => Ok(None),
        Some(s) => s.parse().map(Some).map_err(|_| err(StatusCode::BAD_REQUEST, "invalid", format!("{what} must be <ip>:<port>"))),
    }
}

fn parse_ip(v: &Option<String>) -> Result<Option<std::net::IpAddr>, ApiErr> {
    match v {
        None => Ok(None),
        Some(s) => s.parse().map(Some).map_err(|_| err(StatusCode::BAD_REQUEST, "invalid", "tunnel_ip must be an IP address")),
    }
}

#[derive(Deserialize)]
pub struct InitBody {
    #[serde(default)]
    advertise: Option<String>,
    #[serde(default)]
    tunnel_ip: Option<String>,
    /// Init changes how every record is stored and can't be undone but by
    /// restoring the backup it makes (§5.1).
    #[serde(default)]
    force: bool,
}

pub async fn init(c: Caller, Json(b): Json<InitBody>) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.host(&crate::authz::node_id());
    c.require(Ent::Host, es)?;
    if !b.force {
        return Err(err(
            StatusCode::CONFLICT,
            "confirmation_required",
            "this turns the host into a cluster of one and re-keys every record; it keeps a backup but can't be undone: confirm with --force",
        ));
    }
    let opts = manage::InitOptions { advertise: parse_addr("advertise", &b.advertise)?, tunnel_ip: parse_ip(&b.tunnel_ip)?, listen: None };
    let cfg = c.app.auth.config.clone();
    let out = manage::init(&c.app.manager.clone(), &cfg, opts).await.map_err(cluster_err)?;
    // Role links now name the new cluster.
    c.auth().reload_policies().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    c.set_target("cluster".to_string());
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Deserialize)]
pub struct JoinBody {
    server: String,
    token: String,
    #[serde(default)]
    role: Option<NodeRole>,
    #[serde(default)]
    advertise: Option<String>,
    #[serde(default)]
    tunnel_ip: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

pub async fn join(c: Caller, Json(b): Json<JoinBody>) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.host(&crate::authz::node_id());
    c.require(Ent::Host, es)?;
    let server = b.server.parse().map_err(|_| err(StatusCode::BAD_REQUEST, "invalid", "server must be <ip>:<port> of a cluster node (port 8842)"))?;
    let opts = manage::JoinOptions {
        server,
        token: b.token,
        role: b.role.unwrap_or(NodeRole::Server),
        advertise: parse_addr("advertise", &b.advertise)?,
        tunnel_ip: parse_ip(&b.tunnel_ip)?,
        name: b.name,
        listen: None,
    };
    let cfg = c.app.auth.config.clone();
    let out = manage::join(&c.app.manager.clone(), &cfg, opts).await.map_err(cluster_err)?;
    c.auth().reload_policies().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    c.set_target("cluster".to_string());
    Ok((StatusCode::CREATED, Json(out)))
}

fn require_cluster(c: &Caller) -> Result<std::sync::Arc<crate::cluster::Cluster>, ApiErr> {
    c.manager().cluster().ok_or_else(|| err(StatusCode::CONFLICT, "not_clustered", "this host is not part of a cluster: run `gxctl cluster init` or join one"))
}

pub async fn status(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    match c.manager().cluster() {
        Some(cl) => Ok(Json(manage::status_of(&cl))),
        None => Ok(Json(json!({ "clustered": false, "nodes": c.manager().nodes().list().unwrap_or_default().iter().map(|n| json!({ "id": n.meta.id, "name": n.spec.name, "role": n.spec.role, "phase": n.status.phase })).collect::<Vec<_>>() }))),
    }
}

#[derive(Deserialize)]
pub struct TokenBody {
    #[serde(default)]
    role: Option<NodeRole>,
    #[serde(default)]
    ttl_secs: Option<u64>,
    #[serde(default)]
    allow_import: bool,
}

pub async fn create_token(c: Caller, Json(b): Json<TokenBody>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = require_cluster(&c)?;
    let ttl = b.ttl_secs.unwrap_or(3600).clamp(60, 7 * 86400);
    let role = b.role.unwrap_or(NodeRole::Agent);
    let by = c.actor();
    let token = manage::join_token(&cl, role, ttl, b.allow_import, &by).map_err(cluster_err)?;
    c.set_target("join-token".to_string());
    let servers: Vec<String> = crate::node::NodeStore::new(cl.db.clone())
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.spec.role == NodeRole::Server && !n.status.phase.is_tombstone())
        .filter_map(|n| n.status.advertise.map(|a| a.to_string()))
        .collect();
    Ok((StatusCode::CREATED, Json(json!({ "token": token, "role": role, "ttl_secs": ttl, "servers": servers, "ca_fingerprint": cl.signing_fp() }))))
}

#[derive(Deserialize)]
pub struct PromoteBody {
    nodes: Vec<String>,
    #[serde(default)]
    force: bool,
}

pub async fn promote(c: Caller, Json(b): Json<PromoteBody>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = require_cluster(&c)?;
    let out = manage::promote(&cl, &b.nodes, b.force).await.map_err(cluster_err)?;
    c.set_target("cluster".to_string());
    Ok(Json(out))
}

#[derive(Deserialize, Default)]
pub struct RotateBody {
    #[serde(default)]
    grace_secs: Option<u64>,
}

/// `gxctl cluster rotate-ca [--grace <secs>]` (§12.5).
pub async fn rotate_ca(c: Caller, b: Option<Json<RotateBody>>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = require_cluster(&c)?;
    c.set_target("cluster".to_string());
    c.audit_always();
    let out = cl.rotate_ca(b.and_then(|b| b.0.grace_secs)).await.map_err(cluster_err)?;
    Ok(Json(out))
}

/// A consistent snapshot of the replicated tables, streamed to the caller.
/// It holds credential and token hashes: store it like the database itself.
pub async fn snapshot(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.manager().cluster().ok_or_else(|| err(StatusCode::CONFLICT, "not_clustered", "this host is not part of a cluster"))?;
    c.set_target("cluster".to_string());
    // Audited as it leaves: the file holds every secret hash.
    c.audit_always();
    let db = c.manager().database();
    let dir = tempfile::tempdir().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let path = dir.path().join("snapshot.gxsnap");
    let p2 = path.clone();
    tokio::task::spawn_blocking(move || manage::export_snapshot(&db, &p2))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?
        .map_err(cluster_err)?;
    let file = tokio::fs::File::open(&path).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let stream = tokio_util_stream(file, dir);
    Ok((StatusCode::OK, [("content-type", "application/octet-stream"), ("content-disposition", "attachment; filename=\"glidex-cluster.gxsnap\"")], axum::body::Body::from_stream(stream)))
}

/// Stream a file, deleting its directory when the stream ends.
fn tokio_util_stream(f: tokio::fs::File, dir: tempfile::TempDir) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use tokio::io::AsyncReadExt;
    futures_util::stream::unfold((f, Some(dir)), |(mut f, dir)| async move {
        let mut buf = vec![0u8; 256 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => {
                drop(dir);
                None
            }
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(bytes::Bytes::from(buf)), (f, dir)))
            }
            Err(e) => Some((Err(e), (f, dir))),
        }
    })
}
