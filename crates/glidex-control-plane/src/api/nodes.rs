//! `/nodes`, `/imports` and `/departures`: REST twins of
//! `glidex.cluster.v1.NodeService` and `ImportService` (the logic is in
//! `service::node`).

use super::{ApiErr, Caller, Proto, proto_no_content};
use crate::api::cluster::body;
use crate::service::node as svc;
use axum::{
    Json,
    extract::Path,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use glidex_api::proto::glidex::cluster::v1 as pb;
use serde_json::Value;

pub(crate) use crate::service::node::{check_gateway_nodes, live_gateway_node};

pub async fn list(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(StatusCode::OK, svc::list(&c, Default::default())?))
}

pub async fn get_node(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetNodeRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::get_node(&c, req)?))
}

pub async fn topology(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetNodeTopologyRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(
        StatusCode::OK,
        svc::get_node_topology(&c, req).await?,
    ))
}

pub async fn pci_pools(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::list_pci_pools(&c, Default::default()).await?,
    ))
}

pub async fn prepare_device(
    c: Caller,
    Path((id, address)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::PrepareDeviceRequest {
        id,
        address,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::prepare_device(&c, req).await?))
}

pub async fn release_device(
    c: Caller,
    Path((id, address)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ReleaseDeviceRequest {
        id,
        address,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::release_device(&c, req).await?))
}

pub async fn cordon_device(
    c: Caller,
    Path((id, address)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::CordonDeviceRequest {
        id,
        address,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::cordon_device(&c, req)?))
}

pub async fn uncordon_device(
    c: Caller,
    Path((id, address)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::UncordonDeviceRequest {
        id,
        address,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::uncordon_device(&c, req)?))
}

pub async fn drain(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::DrainNodeRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::drain(&c, req).await?))
}

pub async fn undrain(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::UndrainNodeRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::undrain(&c, req).await?))
}

pub async fn remove(
    c: Caller,
    Path(id): Path<String>,
    b: Option<Json<Value>>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::RemoveNodeRequest = member_body(b, &id)?;
    Ok(Proto(StatusCode::OK, svc::remove(&c, req).await?))
}

pub async fn forget(
    c: Caller,
    Path(id): Path<String>,
    b: Option<Json<Value>>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::ForgetNodeRequest = member_body(b, &id)?;
    Ok(Proto(StatusCode::OK, svc::forget(&c, req).await?))
}

pub async fn purge(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::PurgeNodeRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::purge(&c, req).await?))
}

pub async fn rejoin_token(
    c: Caller,
    Path(id): Path<String>,
    b: Option<Json<Value>>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::CreateRejoinTokenRequest = member_body(b, &id)?;
    Ok(Proto(StatusCode::OK, svc::rejoin_token(&c, req).await?))
}

/// The membership routes share one body (gxctl sends every flag): each
/// takes the fields its message has.
fn member_body<T: serde::de::DeserializeOwned + buffa::MessageName>(
    b: Option<Json<Value>>,
    id: &str,
) -> Result<T, ApiErr> {
    let mut v = b
        .map(|b| b.0)
        .unwrap_or_else(|| Value::Object(Default::default()));
    if let Some(o) = v.as_object_mut() {
        o.retain(|k, _| glidex_api::has_field(T::FULL_NAME, k));
    }
    body(Some(Json(v)), &[("id", id)])
}

/// `POST /nodes/{id}/detach`: freeze the node and start the handover; with
/// `abort`, return it to `Active`; with `departed`, record that it left by
/// itself.
pub async fn detach(
    c: Caller,
    Path(id): Path<String>,
    b: Option<Json<Value>>,
) -> Result<impl IntoResponse, ApiErr> {
    let mut v = b
        .map(|b| b.0)
        .unwrap_or_else(|| Value::Object(Default::default()));
    let flag = |v: &mut Value, k: &str| {
        v.as_object_mut()
            .and_then(|o| o.remove(k))
            .and_then(|x| x.as_bool())
            .unwrap_or(false)
    };
    let (abort, departed) = (flag(&mut v, "abort"), flag(&mut v, "departed"));
    if departed {
        let req = pb::ForgetDepartedRequest {
            id,
            ..Default::default()
        };
        return Ok(Proto(StatusCode::OK, svc::forget_departed(&c, req).await?).into_response());
    }
    if abort {
        let req = pb::AbortDetachRequest {
            id,
            ..Default::default()
        };
        return Ok(Proto(StatusCode::OK, svc::abort_detach(&c, req).await?).into_response());
    }
    let req: pb::DetachNodeRequest = member_body(Some(Json(v)), &id)?;
    Ok(Proto(StatusCode::OK, svc::detach(&c, req).await?).into_response())
}

pub async fn list_imports(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::list_imports(&c, Default::default())?,
    ))
}

pub async fn get_import(c: Caller, Path(plan): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetImportRequest {
        plan,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::get_import(&c, req)?))
}

pub async fn list_departures(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::list_departures(&c, Default::default())?,
    ))
}

pub async fn approve_import(
    c: Caller,
    Path(plan): Path<String>,
    b: Option<Json<Value>>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::ApproveImportRequest = body(b, &[("plan", &plan)])?;
    Ok(Proto(StatusCode::OK, svc::approve_import(&c, req).await?))
}

pub async fn reject_import(c: Caller, Path(plan): Path<String>) -> Result<Response, ApiErr> {
    let req = pb::RejectImportRequest {
        plan,
        ..Default::default()
    };
    svc::reject_import(&c, req).await?;
    Ok(proto_no_content())
}

/// `PUT /nodes/{id}/ovn` `{br_int_datapath?, bridge_mappings?}`.
pub async fn put_node_ovn(
    c: Caller,
    Path(id): Path<String>,
    b: Option<Json<Value>>,
) -> Result<impl IntoResponse, ApiErr> {
    let mut v = b
        .map(|b| b.0)
        .unwrap_or_else(|| Value::Object(Default::default()));
    if let Some(o) = v.as_object_mut()
        && o.contains_key("bridge_mappings")
    {
        o.insert("set_bridge_mappings".into(), Value::Bool(true));
    }
    let req: pb::ConfigureNodeOvnRequest = body(Some(Json(v)), &[("id", &id)])?;
    Ok(Proto(StatusCode::OK, svc::put_node_ovn(&c, req).await?))
}

pub async fn delete_spare_image_files(
    c: Caller,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::DeleteSpareImageFilesRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(
        StatusCode::ACCEPTED,
        svc::delete_spare_image_files(&c, req)?,
    ))
}
