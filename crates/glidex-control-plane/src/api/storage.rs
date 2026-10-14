//! Guest credentials, images and disks (spec/images.md §8,
//! spec/credentials.md), scoped to projects.

use super::{credential_entities, disk_entities, err, ApiErr, Caller};
use crate::authz::{Ent, EntitySet};
use crate::credentials::{CreateCredentialRequest, CredentialInfo, UpdateCredentialRequest};
use crate::images::{CreateDiskRequest, ExtendRootRequest, PullImageRequest, ResizeDiskRequest};
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

pub(crate) fn manager_err(e: crate::state::VmManagerError) -> ApiErr {
    super::error_to_response(e)
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ProjectQuery {
    #[serde(default)]
    project: Option<String>,
}

// ---- guest credentials ---------------------------------------------------

#[derive(serde::Deserialize)]
pub struct CreateCredentialBody {
    #[serde(default)]
    project: Option<String>,
    #[serde(flatten)]
    req: CreateCredentialRequest,
}

pub async fn list_credentials(c: Caller, Query(q): Query<ProjectQuery>) -> Result<impl IntoResponse, ApiErr> {
    let only = match &q.project {
        Some(p) => Some(c.target_project(Some(p))?),
        None => None,
    };
    let visible = c.visible_projects()?;
    let creds = c.manager().list_credentials_in(only.as_deref()).map_err(manager_err)?;
    let out: Vec<CredentialInfo> = creds
        .iter()
        .filter(|cr| visible.contains(&cr.project))
        .filter(|cr| {
            let (e, es) = credential_entities(&cr.project, &cr.username);
            c.allowed("readCredential", e, es)
        })
        .map(CredentialInfo::from)
        .collect();
    Ok(Json(out))
}

pub async fn create_credential(c: Caller, Json(body): Json<CreateCredentialBody>) -> Result<impl IntoResponse, ApiErr> {
    let project = c.target_project(body.project.as_deref())?;
    c.set_project(&project);
    c.set_target(format!("credential:{}", body.req.username));
    c.require(Ent::Project(project.clone()), super::project_entities(&project))?;
    let cred = c.manager().create_credential_in(&project, body.req).map_err(manager_err)?;
    Ok((StatusCode::CREATED, Json(CredentialInfo::from(&cred))))
}

/// A credential in the requested (or default) project the caller may see.
fn visible_credential(c: &Caller, project: Option<&str>, username: &str) -> Result<String, ApiErr> {
    let project = c.target_project(project)?;
    c.set_project(&project);
    c.set_target(format!("credential:{}", username));
    let (e, es) = credential_entities(&project, username);
    c.require_visible("readCredential", e, es, "credential")?;
    Ok(project)
}

pub async fn get_credential(c: Caller, Path(username): Path<String>, Query(q): Query<ProjectQuery>) -> Result<impl IntoResponse, ApiErr> {
    let project = visible_credential(&c, q.project.as_deref(), &username)?;
    let cred = c.manager().get_credential_in(&project, &username).map_err(manager_err)?;
    Ok(Json(CredentialInfo::from(&cred)))
}

pub async fn update_credential(
    c: Caller,
    Path(username): Path<String>,
    Query(q): Query<ProjectQuery>,
    Json(req): Json<UpdateCredentialRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    let project = visible_credential(&c, q.project.as_deref(), &username)?;
    let cred = c.manager().update_credential_in(&project, &username, req).map_err(manager_err)?;
    Ok(Json(CredentialInfo::from(&cred)))
}

pub async fn delete_credential(c: Caller, Path(username): Path<String>, Query(q): Query<ProjectQuery>) -> Result<impl IntoResponse, ApiErr> {
    let project = visible_credential(&c, q.project.as_deref(), &username)?;
    c.manager().delete_credential_in(&project, &username).await.map_err(manager_err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- images (a shared, read-only library) --------------------------------

pub async fn image_catalog(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    Ok(Json(c.manager().image_catalog()))
}

pub async fn list_images(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    Ok(Json(c.manager().list_images().await))
}

pub async fn firmware_catalog(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    Ok(Json(c.manager().firmware_catalog()))
}

pub async fn get_image(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.image(&id);
    c.require(Ent::Image(id.clone()), es)?;
    Ok(Json(c.manager().get_image(&id).await.map_err(manager_err)?))
}

/// `202` for a new download, `200` for one already in flight.
pub async fn pull_image(c: Caller, Json(req): Json<PullImageRequest>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let (img, created) = c.manager().pull_image(req).await.map_err(manager_err)?;
    c.set_target(format!("image:{}", img.id));
    let status = if created { StatusCode::ACCEPTED } else { StatusCode::OK };
    Ok((status, Json(img)))
}

/// A deletion request the image controller finishes (spec/reconciliation.md
/// §6.3): `204` when the image is gone at once (normally), else `202` with
/// it; with `?wait`, `200` once it is gone.
pub async fn delete_image(c: Caller, Path(id): Path<String>, Query(w): Query<WaitQuery>) -> Result<axum::response::Response, ApiErr> {
    let mut es = EntitySet::new();
    es.image(&id);
    c.set_target(format!("image:{}", id));
    c.require(Ent::Image(id.clone()), es)?;
    let m = c.manager();
    let Some(still) = m.delete_image(&id).await.map_err(manager_err)? else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let id = still.id.clone();
    deleted_reply(m, still, w.wait, || m.images.get_image(&id).ok().map(|i| m.images.image_response(&i, false))).await
}

/// The answer to a deletion still under way: `202` with the object, or
/// with `?wait`, `200` once `look` finds it gone (`202` on timeout).
pub(crate) async fn deleted_reply<T: serde::Serialize>(
    m: &crate::state::VmManager,
    still: T,
    wait: Option<u64>,
    look: impl FnMut() -> Option<T>,
) -> Result<axum::response::Response, ApiErr> {
    let Some(secs) = wait else { return Ok((StatusCode::ACCEPTED, Json(still)).into_response()) };
    Ok(match m.wait_gone(std::time::Duration::from_secs(secs.min(300)), look).await {
        None => StatusCode::OK.into_response(),
        Some(t) => (StatusCode::ACCEPTED, Json(t)).into_response(),
    })
}

// ---- disks -------------------------------------------------------------

pub async fn list_disks(c: Caller, Query(q): Query<ProjectQuery>) -> Result<impl IntoResponse, ApiErr> {
    let only = match &q.project {
        Some(p) => Some(c.target_project(Some(p))?),
        None => None,
    };
    let visible = c.visible_projects()?;
    let out: Vec<_> = c
        .manager()
        .list_disks()
        .into_iter()
        .filter(|d| only.as_ref().is_none_or(|p| *p == d.project) && visible.contains(&d.project))
        .filter(|d| {
            let (e, es) = disk_entities(&d.id, &d.project);
            c.allowed("readDisk", e, es)
        })
        .collect();
    Ok(Json(out))
}

/// The disk `key` if the caller may see it (else 404).
async fn visible_disk(c: &Caller, key: &str) -> Result<crate::images::DiskResponse, ApiErr> {
    let d = c
        .manager()
        .get_disk(key)
        .await
        .map_err(|_| err(StatusCode::NOT_FOUND, "not_found", "disk not found"))?;
    c.set_project(&d.project);
    c.set_target(format!("disk:{}", d.id));
    let (e, es) = disk_entities(&d.id, &d.project);
    c.require_visible("readDisk", e, es, "disk")?;
    Ok(d)
}

pub async fn get_disk(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    Ok(Json(visible_disk(&c, &id).await?))
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct WaitQuery {
    #[serde(default)]
    pub(crate) wait: Option<u64>,
}

/// The answer to a disk write (spec/reconciliation.md §12.3): at once with
/// `immediate` (`201` for a create, else `202`); with `?wait`, once the
/// disk settled (`ok`), failed (the error the call would have returned
/// synchronously), or the time ran out (`202`).
async fn disk_reply(c: &Caller, id: &str, wait: Option<u64>, ok: StatusCode, immediate: StatusCode) -> axum::response::Response {
    let Some(secs) = wait else {
        return match c.manager().get_disk(id).await {
            Ok(d) => (immediate, Json(d)).into_response(),
            Err(_) => StatusCode::NO_CONTENT.into_response(),
        };
    };
    match c.manager().wait_disk(id, std::time::Duration::from_secs(secs.min(300))).await {
        (_, None) => StatusCode::NO_CONTENT.into_response(),
        (false, Some(d)) => (StatusCode::ACCEPTED, Json(d)).into_response(),
        (true, Some(d)) => {
            let ready = d.conditions.iter().find(|c| c.kind == "Ready").cloned();
            match ready.as_ref().map(|c| c.reason.as_str()) {
                Some("InvalidDisk") | Some("ResizeInvalid") => {
                    let msg = ready.map(|c| c.message).unwrap_or_default();
                    err(StatusCode::BAD_REQUEST, "invalid_disk", msg).into_response()
                }
                _ if d.phase == crate::images::DiskPhase::Failed || d.phase == crate::images::DiskPhase::Missing => {
                    let msg = ready.map(|c| c.message).unwrap_or_default();
                    err(StatusCode::INTERNAL_SERVER_ERROR, "image_error", msg).into_response()
                }
                _ => (ok, Json(d)).into_response(),
            }
        }
    }
}

pub async fn create_disk(c: Caller, Query(w): Query<WaitQuery>, Json(req): Json<CreateDiskRequest>) -> Result<axum::response::Response, ApiErr> {
    let project = c.target_project(req.project.as_deref())?;
    c.set_project(&project);
    c.require(Ent::Project(project.clone()), super::project_entities(&project))?;
    if let Some(image) = &req.image {
        let mut es = EntitySet::new();
        es.image(image);
        c.require_action("readImage", Ent::Image(image.clone()), es, &[])?;
    }
    let quota = c.quota_mode(&project);
    let (disk, over) = c.manager().create_disk_in(&project, req, quota).await.map_err(manager_err)?;
    c.note_overruns(&over);
    c.set_target(format!("disk:{}", disk.id));
    Ok(disk_reply(&c, &disk.id, w.wait, StatusCode::CREATED, StatusCode::CREATED).await)
}

pub async fn resize_disk(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    Json(req): Json<ResizeDiskRequest>,
) -> Result<axum::response::Response, ApiErr> {
    let d = visible_disk(&c, &id).await?;
    let quota = c.quota_mode(&d.project);
    let (disk, over) = c.manager().resize_disk_with(&d.id, req, quota).await.map_err(manager_err)?;
    c.note_overruns(&over);
    Ok(disk_reply(&c, &disk.id, w.wait, StatusCode::OK, StatusCode::ACCEPTED).await)
}

pub async fn extend_root(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    body: Option<Json<ExtendRootRequest>>,
) -> Result<axum::response::Response, ApiErr> {
    let d = visible_disk(&c, &id).await?;
    let mode = body.map(|Json(b)| b.mode).unwrap_or_default();
    let disk = c.manager().extend_root(&d.id, mode).await.map_err(manager_err)?;
    Ok(disk_reply(&c, &disk.id, w.wait, StatusCode::OK, StatusCode::ACCEPTED).await)
}

/// `204` once the disk is gone (normally at once), `202` while an
/// operation on it finishes first.
pub async fn delete_disk(c: Caller, Path(id): Path<String>) -> Result<axum::response::Response, ApiErr> {
    let d = visible_disk(&c, &id).await?;
    c.manager().delete_disk(&d.id).await.map_err(manager_err)?;
    Ok(match c.manager().get_disk(&d.id).await {
        Ok(still) => (StatusCode::ACCEPTED, Json(still)).into_response(),
        Err(_) => StatusCode::NO_CONTENT.into_response(),
    })
}

pub async fn disk_events(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let d = visible_disk(&c, &id).await?;
    Ok(Json(serde_json::json!({ "events": c.manager().object_events("disk", &d.id).map_err(manager_err)? })))
}

pub async fn image_events(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.image(&id);
    c.require(Ent::Image(id.clone()), es)?;
    let img = c.manager().get_image(&id).await.map_err(manager_err)?;
    Ok(Json(serde_json::json!({ "events": c.manager().object_events("image", &img.id).map_err(manager_err)? })))
}

/// `POST /images/{id}/retry` (`pullImage`): download a failed image again.
pub async fn retry_image(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("image:{}", id));
    Ok((StatusCode::ACCEPTED, Json(c.manager().retry_image(&id).map_err(manager_err)?)))
}
