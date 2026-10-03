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

fn manager_err(e: crate::state::VmManagerError) -> ApiErr {
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
    c.require(Ent::Host, EntitySet::new())?;
    Ok(Json(c.manager().image_catalog()))
}

pub async fn list_images(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    Ok(Json(c.manager().list_images()))
}

pub async fn get_image(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.image(&id);
    c.require(Ent::Image(id.clone()), es)?;
    Ok(Json(c.manager().get_image(&id).await.map_err(manager_err)?))
}

/// `202` for a new download, `200` for one already in flight.
pub async fn pull_image(c: Caller, Json(req): Json<PullImageRequest>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    let (img, created) = c.manager().pull_image(req).await.map_err(manager_err)?;
    c.set_target(format!("image:{}", img.id));
    let status = if created { StatusCode::ACCEPTED } else { StatusCode::OK };
    Ok((status, Json(img)))
}

pub async fn delete_image(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.image(&id);
    c.set_target(format!("image:{}", id));
    c.require(Ent::Image(id.clone()), es)?;
    c.manager().delete_image(&id).map_err(manager_err)?;
    Ok(StatusCode::NO_CONTENT)
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

pub async fn create_disk(c: Caller, Json(req): Json<CreateDiskRequest>) -> Result<impl IntoResponse, ApiErr> {
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
    Ok((StatusCode::CREATED, Json(disk)))
}

pub async fn resize_disk(c: Caller, Path(id): Path<String>, Json(req): Json<ResizeDiskRequest>) -> Result<impl IntoResponse, ApiErr> {
    let d = visible_disk(&c, &id).await?;
    let quota = c.quota_mode(&d.project);
    let (disk, over) = c.manager().resize_disk_with(&d.id, req, quota).await.map_err(manager_err)?;
    c.note_overruns(&over);
    Ok(Json(disk))
}

pub async fn extend_root(c: Caller, Path(id): Path<String>, body: Option<Json<ExtendRootRequest>>) -> Result<impl IntoResponse, ApiErr> {
    let d = visible_disk(&c, &id).await?;
    let mode = body.map(|Json(b)| b.mode).unwrap_or_default();
    Ok(Json(c.manager().extend_root(&d.id, mode).await.map_err(manager_err)?))
}

pub async fn delete_disk(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let d = visible_disk(&c, &id).await?;
    c.manager().delete_disk(&d.id).await.map_err(manager_err)?;
    Ok(StatusCode::NO_CONTENT)
}
