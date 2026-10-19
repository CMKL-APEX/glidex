//! REST twins of `glidex.directory.v1.ConnectorService` (spec/tenancy.md
//! §14): the proto messages as JSON, path parameters merged in, and the
//! same `service::directory` functions as the RPCs.

use super::{ApiErr, Caller};
use crate::service::directory as svc;
use axum::Json;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use glidex_api::proto::glidex::directory::v1 as pb;

pub async fn list(c: Caller) -> Result<Json<pb::ListConnectorsResponse>, ApiErr> {
    Ok(Json(svc::list_connectors(
        &c,
        pb::ListConnectorsRequest::default(),
    )?))
}

pub async fn get(
    c: Caller,
    Path(id): Path<String>,
) -> Result<Json<pb::GetConnectorResponse>, ApiErr> {
    Ok(Json(svc::get_connector(
        &c,
        pb::GetConnectorRequest {
            id,
            ..Default::default()
        },
    )?))
}

pub async fn create(
    c: Caller,
    Json(req): Json<pb::CreateConnectorRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    Ok((StatusCode::CREATED, Json(svc::create_connector(&c, req)?)))
}

/// `PATCH /connectors/{id}` with `{"connector": {…}}`: without an
/// `update_mask`, the fields present in `connector` are the ones changed.
pub async fn update(
    c: Caller,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<pb::UpdateConnectorResponse>, ApiErr> {
    let bad = |m: String| super::err(StatusCode::BAD_REQUEST, "invalid", m);
    let mut req: pb::UpdateConnectorRequest =
        serde_json::from_value(body.clone()).map_err(|e| bad(e.to_string()))?;
    req.id = id;
    if req
        .update_mask
        .as_option()
        .is_none_or(|m| m.paths.is_empty())
    {
        let paths: Vec<String> = body["connector"]
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        req.update_mask = buffa_types::google::protobuf::FieldMask {
            paths,
            ..Default::default()
        }
        .into();
    }
    Ok(Json(svc::update_connector(&c, req)?))
}

#[derive(serde::Deserialize)]
pub struct DeleteQuery {
    users: Option<String>,
}

/// `DELETE /connectors/{id}[?users=disable|detach]`.
pub async fn delete(
    c: Caller,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<DeleteQuery>,
) -> Result<Json<pb::DeleteConnectorResponse>, ApiErr> {
    Ok(Json(svc::delete_connector(
        &c,
        pb::DeleteConnectorRequest {
            id,
            users: q.users,
            ..Default::default()
        },
    )?))
}

pub async fn set_secret(
    c: Caller,
    Path((id, field)): Path<(String, String)>,
    Json(mut v): Json<serde_json::Value>,
) -> Result<Json<pb::SetSecretResponse>, ApiErr> {
    if let Some(o) = v.as_object_mut() {
        o.insert("id".into(), id.into());
        o.insert("field".into(), field.into());
    }
    let req: pb::SetSecretRequest = super::proto_body(v)?;
    Ok(Json(svc::set_secret(&c, req)?))
}

pub async fn test(
    c: Caller,
    Path(id): Path<String>,
) -> Result<Json<pb::TestConnectorResponse>, ApiErr> {
    Ok(Json(
        svc::test_connector(
            &c,
            pb::TestConnectorRequest {
                id,
                ..Default::default()
            },
        )
        .await?,
    ))
}
