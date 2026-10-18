//! REST twins of `glidex.storage.v1` (spec/images.md §8,
//! spec/credentials.md): the proto messages as JSON, path and query
//! parameters merged in, and the same `service::storage` functions as the
//! RPCs. Status codes say what the RPCs put in their answers (`202` for a
//! write still under way, `204` once a deletion is done).

use super::{ApiErr, Caller, Proto, proto_body, proto_no_content};
use crate::service::storage::{self as svc, DiskReply};
use axum::{
    Json,
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use glidex_api::proto::glidex::storage::v1 as pb;
use serde_json::Value;

#[derive(Debug, serde::Deserialize, Default)]
pub struct ProjectQuery {
    #[serde(default)]
    project: Option<String>,
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct WaitQuery {
    #[serde(default)]
    pub(crate) wait: Option<u64>,
}

impl WaitQuery {
    fn secs(&self) -> Option<u32> {
        self.wait.map(|w| w.min(u64::from(u32::MAX)) as u32)
    }
}

fn body<T: serde::de::DeserializeOwned + buffa::MessageName>(v: Value) -> Result<T, ApiErr> {
    proto_body(v)
}

// ---- guest credentials ----------------------------------------------------

pub async fn list_credentials(
    c: Caller,
    Query(q): Query<ProjectQuery>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListCredentialsRequest {
        project: q.project,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::list_credentials(&c, req)?))
}

pub async fn create_credential(
    c: Caller,
    Json(v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::CreateCredentialRequest = body(v)?;
    Ok(Proto(StatusCode::CREATED, svc::create_credential(&c, req)?))
}

pub async fn get_credential(
    c: Caller,
    Path(username): Path<String>,
    Query(q): Query<ProjectQuery>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetCredentialRequest {
        username,
        project: q.project,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::get_credential(&c, req)?))
}

/// `PUT /credentials/{username}`: the fields present are the ones changed
/// (an empty `ssh_authorized_keys` clears the keys), unless the body names
/// an `update_mask`.
pub async fn update_credential(
    c: Caller,
    Path(username): Path<String>,
    Query(q): Query<ProjectQuery>,
    Json(v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let present: Vec<String> = v
        .as_object()
        .map(|o| {
            o.keys()
                .filter(|k| *k == "password" || *k == "ssh_authorized_keys")
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut req: pb::UpdateCredentialRequest = body(v)?;
    req.username = username;
    if req.project.is_none() {
        req.project = q.project;
    }
    if req
        .update_mask
        .as_option()
        .is_none_or(|m| m.paths.is_empty())
    {
        req.update_mask = buffa_types::google::protobuf::FieldMask {
            paths: present,
            ..Default::default()
        }
        .into();
    }
    Ok(Proto(StatusCode::OK, svc::update_credential(&c, req)?))
}

pub async fn delete_credential(
    c: Caller,
    Path(username): Path<String>,
    Query(q): Query<ProjectQuery>,
) -> Result<Response, ApiErr> {
    let req = pb::DeleteCredentialRequest {
        username,
        project: q.project,
        ..Default::default()
    };
    svc::delete_credential(&c, req).await?;
    Ok(proto_no_content())
}

// ---- images (a shared, read-only library) --------------------------------

#[derive(serde::Deserialize, Default)]
pub struct ArchQuery {
    /// `x86_64` or `aarch64` (or `ARCH_…`); default: this host's
    /// (spec/clustering.md §9.2.1).
    #[serde(default)]
    arch: Option<String>,
}

fn arch_of(q: &ArchQuery) -> Result<glidex_api::proto::glidex::common::v1::Arch, ApiErr> {
    use glidex_api::proto::glidex::common::v1::Arch;
    match q
        .arch
        .as_deref()
        .map(|a| a.trim_start_matches("ARCH_").to_ascii_lowercase())
    {
        None => Ok(Arch::ARCH_UNSPECIFIED),
        Some(a) if a == "x86_64" => Ok(Arch::ARCH_X86_64),
        Some(a) if a == "aarch64" => Ok(Arch::ARCH_AARCH64),
        Some(a) => Err(super::err(
            StatusCode::BAD_REQUEST,
            "invalid",
            format!("unknown arch {a}: x86_64 or aarch64"),
        )),
    }
}

pub async fn image_catalog(
    c: Caller,
    Query(q): Query<ArchQuery>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetImageCatalogRequest {
        arch: arch_of(&q)?.into(),
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::image_catalog(&c, req)?))
}

pub async fn firmware_catalog(
    c: Caller,
    Query(q): Query<ArchQuery>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetFirmwareCatalogRequest {
        arch: arch_of(&q)?.into(),
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::firmware_catalog(&c, req)?))
}

pub async fn list_images(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::list_images(&c, Default::default()).await?,
    ))
}

pub async fn get_image(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetImageRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::get_image(&c, req).await?))
}

/// `202` for a new download, `200` for one already in flight.
pub async fn pull_image(c: Caller, Json(v): Json<Value>) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::PullImageRequest = body(v)?;
    let r = svc::pull_image(&c, req).await?;
    let status = if r.created == Some(true) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok(Proto(status, r))
}

/// A deletion request the image controller finishes (spec/reconciliation.md
/// §6.3): `204` once the image is gone (normally at once, or with `?wait`
/// when it went in time), else `202` with it.
pub async fn delete_image(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
) -> Result<Response, ApiErr> {
    let req = pb::DeleteImageRequest {
        id,
        wait_secs: w.secs(),
        ..Default::default()
    };
    let r = svc::delete_image(&c, req).await?;
    Ok(match r.image.as_option() {
        None => proto_no_content(),
        Some(_) => Proto(StatusCode::ACCEPTED, r).into_response(),
    })
}

pub async fn image_events(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListImageEventsRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::image_events(&c, req).await?))
}

/// `POST /images/{id}/retry` (`pullImage`): download a failed image again.
pub async fn retry_image(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::RetryImageRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::ACCEPTED, svc::retry_image(&c, req)?))
}

/// `POST /images/{id}/copies` `{node}`: keep a copy on a node ahead of need
/// (spec/clustering.md §9.2.2).
pub async fn add_image_copy(
    c: Caller,
    Path(id): Path<String>,
    Json(v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let mut req: pb::AddImageCopyRequest = body(v)?;
    req.id = id;
    Ok(Proto(
        StatusCode::ACCEPTED,
        svc::add_image_copy(&c, req).await?,
    ))
}

/// `DELETE /images/{id}/copies/{node}`: let a node's copy go (§9.2.2).
pub async fn remove_image_copy(
    c: Caller,
    Path((id, node)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::RemoveImageCopyRequest {
        id,
        node,
        ..Default::default()
    };
    Ok(Proto(
        StatusCode::ACCEPTED,
        svc::remove_image_copy(&c, req).await?,
    ))
}

// ---- disks -------------------------------------------------------------

pub async fn list_disks(
    c: Caller,
    Query(q): Query<ProjectQuery>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListDisksRequest {
        project: q.project,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::list_disks(&c, req)?))
}

pub async fn get_disk(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetDiskRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::get_disk(&c, req).await?))
}

/// The answer to a disk write (spec/reconciliation.md §12.3): at once with
/// `immediate` (`201` for a create, else `202`); with `?wait`, `ok` once
/// the disk settled, `202` when the time ran out; `204` once it's gone.
/// The body is `{disk, settled}` (every write's response has that shape).
fn disk_reply(r: DiskReply, ok: StatusCode, immediate: StatusCode) -> Response {
    let status = match &r {
        DiskReply::Gone => return proto_no_content(),
        DiskReply::Now(_) => immediate,
        DiskReply::Settled(_) => ok,
        DiskReply::Pending(_) => StatusCode::ACCEPTED,
    };
    let (disk, settled) = r.parts();
    let body = pb::CreateDiskResponse {
        disk: disk.into(),
        settled,
        ..Default::default()
    };
    Proto(status, body).into_response()
}

pub async fn create_disk(
    c: Caller,
    Query(w): Query<WaitQuery>,
    Json(v): Json<Value>,
) -> Result<Response, ApiErr> {
    let mut req: pb::CreateDiskRequest = body(v)?;
    req.wait_secs = req.wait_secs.or(w.secs());
    let r = svc::create_disk(&c, req).await?;
    Ok(disk_reply(r, StatusCode::CREATED, StatusCode::CREATED))
}

pub async fn resize_disk(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    Json(v): Json<Value>,
) -> Result<Response, ApiErr> {
    let mut req: pb::ResizeDiskRequest = body(v)?;
    req.id = id;
    req.wait_secs = req.wait_secs.or(w.secs());
    let r = svc::resize_disk(&c, req).await?;
    Ok(disk_reply(r, StatusCode::OK, StatusCode::ACCEPTED))
}

pub async fn extend_root(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    v: Option<Json<Value>>,
) -> Result<Response, ApiErr> {
    let v = v.map(|Json(v)| v).unwrap_or_else(|| serde_json::json!({}));
    let mut req: pb::ExtendRootRequest = body(v)?;
    req.id = id;
    req.wait_secs = req.wait_secs.or(w.secs());
    let r = svc::extend_root(&c, req).await?;
    Ok(disk_reply(r, StatusCode::OK, StatusCode::ACCEPTED))
}

/// `204` once the disk is gone (normally at once), `202` while an
/// operation on it finishes first.
pub async fn delete_disk(c: Caller, Path(id): Path<String>) -> Result<Response, ApiErr> {
    let req = pb::DeleteDiskRequest {
        id,
        ..Default::default()
    };
    Ok(match svc::delete_disk(&c, req).await? {
        None => proto_no_content(),
        Some(d) => Proto(
            StatusCode::ACCEPTED,
            pb::DeleteDiskResponse {
                disk: svc::disk_pb(&d).into(),
                ..Default::default()
            },
        )
        .into_response(),
    })
}

pub async fn disk_events(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListDiskEventsRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::disk_events(&c, req).await?))
}
