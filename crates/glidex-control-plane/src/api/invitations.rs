//! REST twins of `glidex.directory.v1.InvitationService` and
//! `LoginService.InspectInvitation` (spec/tenancy.md §6.1, §14): the proto
//! messages as JSON, on the same `service::invitations` functions.

use super::{ApiErr, AppState, Caller, proto_body};
use crate::service::invitations as svc;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use glidex_api::proto::glidex::auth::v1 as login_pb;
use glidex_api::proto::glidex::directory::v1 as pb;
use serde_json::Value;

/// `GET /tenants/{tenant}/invitations`.
pub async fn list(
    c: Caller,
    Path(tenant): Path<String>,
) -> Result<Json<pb::ListInvitationsResponse>, ApiErr> {
    let req = pb::ListInvitationsRequest {
        tenant,
        ..Default::default()
    };
    Ok(Json(svc::list_invitations(&c, req)?))
}

/// `POST /tenants/{tenant}/invitations`: `201` with the link, shown once.
pub async fn create(
    c: Caller,
    Path(tenant): Path<String>,
    Json(mut v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    if let Some(o) = v.as_object_mut() {
        o.insert("tenant".into(), tenant.into());
    }
    let req: pb::CreateInvitationRequest = proto_body(v)?;
    Ok((StatusCode::CREATED, Json(svc::create_invitation(&c, req)?)))
}

/// `DELETE /invitations/{id}`.
pub async fn revoke(c: Caller, Path(id): Path<String>) -> Result<StatusCode, ApiErr> {
    svc::revoke_invitation(
        &c,
        pb::RevokeInvitationRequest {
            id,
            ..Default::default()
        },
    )?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/invitation`: what a link is for, before its login.
pub async fn inspect(
    State(app): State<AppState>,
    Json(v): Json<Value>,
) -> Result<Json<login_pb::InspectInvitationResponse>, ApiErr> {
    let req = proto_body(v)?;
    Ok(Json(svc::inspect_invitation(&app, req)?))
}
