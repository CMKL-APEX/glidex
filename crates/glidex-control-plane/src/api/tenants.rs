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

// ---- DirectoryService (spec/tenancy.md D59) --------------------------------

pub async fn list_directories(c: Caller) -> Result<Json<pb::ListDirectoriesResponse>, ApiErr> {
    Ok(Json(crate::service::directories::list_directories(
        &c,
        pb::ListDirectoriesRequest::default(),
    )?))
}

pub async fn create_directory(
    c: Caller,
    Json(req): Json<pb::CreateDirectoryRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    Ok((
        StatusCode::CREATED,
        Json(crate::service::directories::create_directory(&c, req)?),
    ))
}

pub async fn delete_directory(c: Caller, Path(id): Path<String>) -> Result<StatusCode, ApiErr> {
    crate::service::directories::delete_directory(
        &c,
        pb::DeleteDirectoryRequest {
            id,
            ..Default::default()
        },
    )?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- offers (spec/tenancy.md D64) ------------------------------------------

pub async fn offer_admission(
    c: Caller,
    Json(req): Json<pb::OfferAdmissionRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    Ok((StatusCode::CREATED, Json(svc::offer_admission(&c, req)?)))
}

pub async fn list_offers(c: Caller) -> Result<Json<pb::ListOffersResponse>, ApiErr> {
    Ok(Json(svc::list_offers(
        &c,
        pb::ListOffersRequest::default(),
    )?))
}

pub async fn revoke_offer(c: Caller, Path(id): Path<String>) -> Result<StatusCode, ApiErr> {
    svc::revoke_offer(
        &c,
        pb::RevokeOfferRequest {
            id,
            ..Default::default()
        },
    )?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- trust (spec/tenancy.md D65) ---------------------------------------------

pub async fn list_tenant_clusters(
    c: Caller,
    Path(id): Path<String>,
) -> Result<Json<pb::ListTenantClustersResponse>, ApiErr> {
    Ok(Json(svc::list_tenant_clusters(
        &c,
        pb::ListTenantClustersRequest {
            id,
            ..Default::default()
        },
    )?))
}

#[derive(Deserialize)]
pub struct OptIn {
    offer: String,
}

pub async fn opt_in_cluster(
    c: Caller,
    Path(id): Path<String>,
    Json(b): Json<OptIn>,
) -> Result<impl IntoResponse, ApiErr> {
    let r = svc::opt_in_cluster(
        &c,
        pb::OptInClusterRequest {
            id,
            offer: b.offer,
            ..Default::default()
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(r)))
}

pub async fn opt_out_cluster(
    c: Caller,
    Path((id, cluster)): Path<(String, String)>,
) -> Result<StatusCode, ApiErr> {
    svc::opt_out_cluster(
        &c,
        pb::OptOutClusterRequest {
            id,
            cluster,
            ..Default::default()
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_trusted_directories(
    c: Caller,
) -> Result<Json<pb::ListTrustedDirectoriesResponse>, ApiErr> {
    Ok(Json(svc::list_trusted_directories(&c, Default::default())?))
}

pub async fn set_trusted_directory(
    c: Caller,
    Path(id): Path<String>,
    Json(mut req): Json<pb::SetTrustedDirectoryRequest>,
) -> Result<Json<pb::SetTrustedDirectoryResponse>, ApiErr> {
    req.id = id;
    Ok(Json(svc::set_trusted_directory(&c, req)?))
}
