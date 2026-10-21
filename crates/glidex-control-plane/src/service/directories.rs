//! The directories a host serves (spec/tenancy.md D27, D59):
//! `glidex.tenancy.v1.DirectoryService`, for the host's operator. It sees
//! that a directory exists, never what is inside it.

use super::{Call, ServiceError, ServiceResult};
use crate::authz::{Ent, EntitySet};
use crate::service::tenancy::tenant_error;
use crate::tenants::{Directory, LOCAL_DIRECTORY};
use axum::http::StatusCode;
use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::tenancy::v1 as pb;

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> ServiceError {
    ServiceError::new(status, code, message)
}

fn directory_pb(call: &Call, d: &Directory) -> pb::HostedDirectory {
    pb::HostedDirectory {
        id: d.id.clone(),
        name: d.name.clone(),
        issuer: crate::api::issuer::issuer_url(call.auth(), call.manager(), d).unwrap_or_default(),
        created_at: Timestamp {
            seconds: d.created_at as i64,
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

pub fn list_directories(
    call: &Call,
    _req: pb::ListDirectoriesRequest,
) -> ServiceResult<pb::ListDirectoriesResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let all = call.auth().tenants.directories().map_err(tenant_error)?;
    Ok(pb::ListDirectoriesResponse {
        directories: all.iter().map(|d| directory_pb(call, d)).collect(),
        ..Default::default()
    })
}

pub fn create_directory(
    call: &Call,
    req: pb::CreateDirectoryRequest,
) -> ServiceResult<pb::CreateDirectoryResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    if req.admin_email.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "admin_email is required",
        ));
    }
    let name = if req.name.trim().is_empty() {
        req.id.clone()
    } else {
        req.name.clone()
    };
    let (d, owner) = call
        .auth()
        .tenants
        .create_directory(&req.id, &name, &req.owner)
        .map_err(tenant_error)?;
    call.set_target(format!("directory:{}", d.id));
    // Its keys now, so its discovery document is complete from the start.
    let now = crate::auth::store::now();
    let made = call
        .auth()
        .connectors
        .maintain_signing_keys(&d.id, now)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()));
    let by = call.p.user_id().unwrap_or("-").to_string();
    // D59, D60: the first administrator comes in with a passkey.
    let inv = made.and_then(|_| {
        call.auth()
            .invitations
            .create(
                &owner.id,
                &req.admin_email,
                "role.tenant-admin",
                crate::auth::passkeys::PROVIDER,
                &by,
                req.days.map(u64::from),
            )
            .map_err(|e| err(StatusCode::BAD_REQUEST, "invalid", e.to_string()))
    });
    let (inv, token) = match inv {
        Ok(v) => v,
        Err(e) => {
            let _ = call.auth().tenants.delete_directory(&d.id);
            let _ = call.auth().connectors.forget_directory_keys(&d.id);
            return Err(e);
        }
    };
    call.detail("owner", serde_json::json!(owner.slug));
    call.detail("invitation", serde_json::json!(inv.id));
    let origin = call
        .manager()
        .ui_url()
        .or_else(|| call.auth().config.auth.allowed_origins.first().cloned())
        .unwrap_or_else(|| "https://localhost:5173".into());
    let (id, secret) = token.split_once('.').expect("id.secret");
    Ok(pb::CreateDirectoryResponse {
        directory: directory_pb(call, &d).into(),
        invitation_link: format!("{}/invite/{id}#{secret}", origin.trim_end_matches('/')),
        ..Default::default()
    })
}

pub fn delete_directory(
    call: &Call,
    req: pb::DeleteDirectoryRequest,
) -> ServiceResult<pb::DeleteDirectoryResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    call.set_target(format!("directory:{}", req.id));
    if req.id == LOCAL_DIRECTORY {
        return Err(err(
            StatusCode::CONFLICT,
            "conflict",
            "the host's own directory stays",
        ));
    }
    let a = call.auth();
    if a.tenants
        .get_directory(&req.id)
        .map_err(tenant_error)?
        .is_none()
    {
        return Err(err(StatusCode::NOT_FOUND, "not_found", "no such directory"));
    }
    let internal = |e: String| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e);
    let users = a.store.users().map_err(|e| internal(e.to_string()))?;
    let connectors = a.connectors.list().map_err(|e| internal(e.to_string()))?;
    let tenants: Vec<_> = a
        .tenants
        .list()
        .map_err(tenant_error)?
        .into_iter()
        .filter(|t| t.directory_id == req.id)
        .collect();
    let mut projects = false;
    for t in &tenants {
        projects |= a.tenants.has_projects(&t.id).map_err(tenant_error)?;
    }
    if users.iter().any(|u| u.directory_id == req.id)
        || connectors.iter().any(|c| c.directory_id == req.id)
        || projects
    {
        return Err(err(
            StatusCode::CONFLICT,
            "directory_not_empty",
            "the directory has users, connectors or projects; its administrators remove them first",
        ));
    }
    for t in &tenants {
        for i in a.invitations.list(Some(&t.id)).unwrap_or_default() {
            let _ = a.invitations.revoke(&i.id);
        }
    }
    a.tenants.delete_directory(&req.id).map_err(tenant_error)?;
    a.connectors
        .forget_directory_keys(&req.id)
        .map_err(|e| internal(e.to_string()))?;
    Ok(pb::DeleteDirectoryResponse::default())
}
