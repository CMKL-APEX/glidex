//! REST twins of `glidex.directory.v1.FederationService` (spec/19-tenancy.md
//! §9): the proto messages as JSON, on `service::federations`.

use super::{ApiErr, Caller, proto_body};
use crate::service::federations as svc;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use glidex_api::proto::glidex::directory::v1 as pb;
use serde_json::Value;

/// `GET /tenants/{tenant}/federations`.
pub async fn list(
    c: Caller,
    Path(tenant): Path<String>,
) -> Result<Json<pb::ListFederationsResponse>, ApiErr> {
    let req = pb::ListFederationsRequest {
        tenant,
        ..Default::default()
    };
    Ok(Json(svc::list_federations(&c, req)?))
}

/// `GET /tenants/{tenant}/federations/{federation}`.
pub async fn get(
    c: Caller,
    Path((tenant, federation)): Path<(String, String)>,
) -> Result<Json<pb::GetFederationResponse>, ApiErr> {
    let req = pb::GetFederationRequest {
        tenant,
        federation,
        ..Default::default()
    };
    Ok(Json(svc::get_federation(&c, req)?))
}

/// `POST /tenants/{tenant}/federations {federation}`.
pub async fn create(
    c: Caller,
    Path(tenant): Path<String>,
    Json(mut v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    if let Some(o) = v.as_object_mut() {
        o.insert("tenant".into(), tenant.into());
    }
    let req: pb::CreateFederationRequest = proto_body(v)?;
    Ok((
        StatusCode::CREATED,
        Json(svc::create_federation(&c, req).await?),
    ))
}

/// `PATCH /tenants/{tenant}/federations/{federation} {changes,
/// update_mask?}`: without a mask, the fields present are the ones changed.
pub async fn update(
    c: Caller,
    Path((tenant, federation)): Path<(String, String)>,
    Json(mut v): Json<Value>,
) -> Result<Json<pb::UpdateFederationResponse>, ApiErr> {
    let present: Vec<String> = v
        .get("changes")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("tenant".into(), tenant.into());
        o.insert("federation".into(), federation.into());
    }
    let mut req: pb::UpdateFederationRequest = proto_body(v)?;
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
    Ok(Json(svc::update_federation(&c, req).await?))
}

/// `DELETE /tenants/{tenant}/federations/{federation}`.
pub async fn delete(
    c: Caller,
    Path((tenant, federation)): Path<(String, String)>,
) -> Result<StatusCode, ApiErr> {
    svc::delete_federation(
        &c,
        pb::DeleteFederationRequest {
            tenant,
            federation,
            ..Default::default()
        },
    )?;
    Ok(StatusCode::NO_CONTENT)
}
