//! `/cluster/*`: REST twins of `glidex.cluster.v1.ClusterService` (the logic
//! is in `service::cluster`), and the snapshot download, which stays REST.

use super::{ApiErr, Caller, Proto, err, proto_body};
use crate::authz::{Ent, EntitySet};
use crate::cluster::manage;
use crate::service::cluster as svc;
use axum::{Json, http::StatusCode, response::IntoResponse};
use glidex_api::proto::glidex::cluster::v1 as pb;
use serde_json::Value;

fn cluster_err(e: crate::cluster::ClusterError) -> ApiErr {
    err(StatusCode::CONFLICT, "cluster_error", e.to_string())
}

/// The old shape's enum values of the cluster family.
pub(crate) const ENUMS: &[(&str, &str)] = &[
    ("role", "NODE_ROLE"),
    ("br_int_datapath", "BR_INT_DATAPATH"),
];

pub(crate) fn body<T: serde::de::DeserializeOwned + buffa::MessageName>(
    v: Option<Json<Value>>,
    path: &[(&str, &str)],
) -> Result<T, ApiErr> {
    let mut v = v
        .map(|v| v.0)
        .unwrap_or_else(|| Value::Object(Default::default()));
    if let Some(o) = v.as_object_mut() {
        for (k, val) in path {
            o.insert((*k).into(), Value::String((*val).into()));
        }
    }
    proto_body(v, ENUMS, &[])
}

pub async fn init(c: Caller, b: Option<Json<Value>>) -> Result<impl IntoResponse, ApiErr> {
    let req = body(b, &[])?;
    Ok(Proto(StatusCode::CREATED, svc::init(&c, req).await?))
}

pub async fn join(c: Caller, b: Option<Json<Value>>) -> Result<impl IntoResponse, ApiErr> {
    let req = body(b, &[])?;
    Ok(Proto(StatusCode::CREATED, svc::join(&c, req).await?))
}

pub async fn import_join(c: Caller, b: Option<Json<Value>>) -> Result<impl IntoResponse, ApiErr> {
    let req = body(b, &[])?;
    Ok(Proto(
        StatusCode::ACCEPTED,
        svc::import_join(&c, req).await?,
    ))
}

pub async fn rejoin(c: Caller, b: Option<Json<Value>>) -> Result<impl IntoResponse, ApiErr> {
    let req = body(b, &[])?;
    Ok(Proto(StatusCode::OK, svc::rejoin(&c, req).await?))
}

pub async fn dissolve(c: Caller, b: Option<Json<Value>>) -> Result<impl IntoResponse, ApiErr> {
    let req = body(b, &[])?;
    Ok(Proto(StatusCode::OK, svc::dissolve(&c, req).await?))
}

pub async fn leave(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::leave(&c, Default::default()).await?,
    ))
}

#[derive(serde::Deserialize, Default)]
pub struct StatusQuery {
    #[serde(default)]
    ports: bool,
}

pub async fn status(
    c: Caller,
    via: Option<axum::Extension<super::RelayedBy>>,
    axum::extract::Query(q): axum::extract::Query<StatusQuery>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetClusterStatusRequest {
        ports: q.ports,
        ..Default::default()
    };
    let via = via.as_ref().map(|v| v.0.0.as_str());
    Ok(Proto(StatusCode::OK, svc::status(&c, req, via).await?))
}

pub async fn create_token(c: Caller, b: Option<Json<Value>>) -> Result<impl IntoResponse, ApiErr> {
    let req = body(b, &[])?;
    Ok(Proto(
        StatusCode::CREATED,
        svc::create_token(&c, req).await?,
    ))
}

pub async fn promote(c: Caller, b: Option<Json<Value>>) -> Result<impl IntoResponse, ApiErr> {
    let req = body(b, &[])?;
    Ok(Proto(StatusCode::OK, svc::promote(&c, req).await?))
}

/// `gxctl cluster rotate-ca [--grace <secs>]` (§12.5).
pub async fn rotate_ca(c: Caller, b: Option<Json<Value>>) -> Result<impl IntoResponse, ApiErr> {
    let req = body(b, &[])?;
    Ok(Proto(StatusCode::OK, svc::rotate_ca(&c, req).await?))
}

pub async fn rotate_secrets_key(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::rotate_secrets_key(&c, Default::default()).await?,
    ))
}

/// A consistent snapshot of the replicated tables, streamed to the caller.
/// It holds credential and token hashes: store it like the database itself.
pub async fn snapshot(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.manager().cluster().ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "not_clustered",
            "this host is not part of a cluster",
        )
    })?;
    c.set_target("cluster".to_string());
    // Audited as it leaves: the file holds every secret hash.
    c.audit_always();
    let db = c.manager().database();
    let dir = tempfile::tempdir()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let path = dir.path().join("snapshot.gxsnap");
    let p2 = path.clone();
    tokio::task::spawn_blocking(move || manage::export_snapshot(&db, &p2))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?
        .map_err(cluster_err)?;
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let stream = tokio_util_stream(file, dir);
    Ok((
        StatusCode::OK,
        [
            ("content-type", "application/octet-stream"),
            (
                "content-disposition",
                "attachment; filename=\"glidex-cluster.gxsnap\"",
            ),
        ],
        axum::body::Body::from_stream(stream),
    ))
}

/// Stream a file, deleting its directory when the stream ends.
fn tokio_util_stream(
    f: tokio::fs::File,
    dir: tempfile::TempDir,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
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

// ---- OVN as a cluster setting (spec/clustering-ui.md §8a) -----------------

pub async fn get_ovn_settings(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::get_ovn_settings(&c, Default::default())?,
    ))
}

pub async fn put_ovn_settings(
    c: Caller,
    b: Option<Json<Value>>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = body(b, &[])?;
    Ok(Proto(StatusCode::OK, svc::put_ovn_settings(&c, req).await?))
}

pub async fn edge_defaults(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::edge_defaults(&c, Default::default())?,
    ))
}

pub async fn cluster_ovn(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::ovn_overview(&c, Default::default()).await?,
    ))
}

pub async fn cluster_ovn_tunnels(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::ovn_tunnels(&c, Default::default()).await?,
    ))
}
