//! REST twins of `glidex.tenancy.v1` (spec/tenancy.md §14): the proto
//! messages as JSON, path parameters merged into the request, and the same
//! `service::tenancy` functions as the RPCs.

use super::{ApiErr, Caller};
use crate::service::tenancy as svc;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use glidex_api::proto::glidex::tenancy::v1 as pb;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Page {
    #[serde(default)]
    page_size: Option<u32>,
    #[serde(default)]
    page_token: String,
}

pub async fn list(
    c: Caller,
    Query(q): Query<Page>,
) -> Result<Json<pb::ListTenantsResponse>, ApiErr> {
    let req = pb::ListTenantsRequest {
        page_size: q.page_size,
        page_token: q.page_token,
        ..Default::default()
    };
    Ok(Json(svc::list_tenants(&c, req)?))
}

pub async fn get(c: Caller, Path(id): Path<String>) -> Result<Json<pb::GetTenantResponse>, ApiErr> {
    Ok(Json(svc::get_tenant(
        &c,
        pb::GetTenantRequest {
            id,
            ..Default::default()
        },
    )?))
}

pub async fn create(
    c: Caller,
    Json(req): Json<pb::CreateTenantRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    Ok((StatusCode::CREATED, Json(svc::create_tenant(&c, req)?)))
}

pub async fn update(
    c: Caller,
    Path(id): Path<String>,
    Json(mut req): Json<pb::UpdateTenantRequest>,
) -> Result<Json<pb::UpdateTenantResponse>, ApiErr> {
    req.id = id;
    Ok(Json(svc::update_tenant(&c, req)?))
}

pub async fn suspend(
    c: Caller,
    Path(id): Path<String>,
) -> Result<Json<pb::SuspendTenantResponse>, ApiErr> {
    Ok(Json(svc::suspend_tenant(
        &c,
        pb::SuspendTenantRequest {
            id,
            ..Default::default()
        },
    )?))
}

pub async fn resume(
    c: Caller,
    Path(id): Path<String>,
) -> Result<Json<pb::ResumeTenantResponse>, ApiErr> {
    Ok(Json(svc::resume_tenant(
        &c,
        pb::ResumeTenantRequest {
            id,
            ..Default::default()
        },
    )?))
}

pub async fn delete(c: Caller, Path(id): Path<String>) -> Result<StatusCode, ApiErr> {
    svc::delete_tenant(
        &c,
        pb::DeleteTenantRequest {
            id,
            ..Default::default()
        },
    )?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_members(
    c: Caller,
    Path(id): Path<String>,
) -> Result<Json<pb::ListMembersResponse>, ApiErr> {
    Ok(Json(svc::list_members(
        &c,
        pb::ListMembersRequest {
            id,
            ..Default::default()
        },
    )?))
}

pub async fn add_member(
    c: Caller,
    Path(id): Path<String>,
    Json(mut req): Json<pb::AddMemberRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    req.id = id;
    Ok((StatusCode::CREATED, Json(svc::add_member(&c, req)?)))
}

pub async fn remove_member(
    c: Caller,
    Path((id, principal)): Path<(String, String)>,
) -> Result<StatusCode, ApiErr> {
    svc::remove_member(
        &c,
        pb::RemoveMemberRequest {
            id,
            principal,
            ..Default::default()
        },
    )?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_bindings(
    c: Caller,
    Path(id): Path<String>,
) -> Result<Json<pb::ListBindingsResponse>, ApiErr> {
    Ok(Json(svc::list_bindings(
        &c,
        pb::ListBindingsRequest {
            id,
            ..Default::default()
        },
    )?))
}

pub async fn add_binding(
    c: Caller,
    Path(id): Path<String>,
    Json(mut req): Json<pb::AddBindingRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    req.id = id;
    Ok((StatusCode::CREATED, Json(svc::add_binding(&c, req)?)))
}

pub async fn remove_binding(
    c: Caller,
    Path((id, link)): Path<(String, String)>,
) -> Result<StatusCode, ApiErr> {
    svc::remove_binding(
        &c,
        pb::RemoveBindingRequest {
            id,
            link,
            ..Default::default()
        },
    )?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_admissions(c: Caller) -> Result<Json<pb::ListAdmissionsResponse>, ApiErr> {
    Ok(Json(svc::list_admissions(
        &c,
        pb::ListAdmissionsRequest::default(),
    )?))
}

pub async fn set_admission(
    c: Caller,
    Path(tenant): Path<String>,
    Json(mut req): Json<pb::SetAdmissionRequest>,
) -> Result<Json<pb::SetAdmissionResponse>, ApiErr> {
    req.tenant = tenant;
    Ok(Json(svc::set_admission(&c, req)?))
}

pub async fn remove_admission(c: Caller, Path(tenant): Path<String>) -> Result<StatusCode, ApiErr> {
    svc::remove_admission(
        &c,
        pb::RemoveAdmissionRequest {
            tenant,
            ..Default::default()
        },
    )?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_device_pools(
    c: Caller,
    Path(id): Path<String>,
) -> Result<Json<pb::ListDevicePoolsResponse>, ApiErr> {
    let req = pb::ListDevicePoolsRequest {
        id,
        ..Default::default()
    };
    Ok(Json(svc::list_device_pools(&c, req).await?))
}

pub async fn set_device_pools(
    c: Caller,
    Path(id): Path<String>,
    Json(mut req): Json<pb::SetDevicePoolsRequest>,
) -> Result<Json<pb::SetDevicePoolsResponse>, ApiErr> {
    req.id = id;
    Ok(Json(svc::set_device_pools(&c, req).await?))
}
