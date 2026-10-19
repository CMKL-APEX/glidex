//! Networks (host and project networks, grants, shares) and host
//! networking through glidex-netd (spec/networking.md §11,
//! spec/security.md §6.2, §8).

use super::{ApiErr, Caller, Proto, proto_body, proto_no_content};
use crate::service::host;
use crate::service::network as svc;
use axum::{Json, extract::Path, http::StatusCode, response::IntoResponse};
use glidex_api::proto::glidex::host::v1 as hpb;
use glidex_api::proto::glidex::network::v1 as pb;
use serde_json::Value;

// ---- networks, shares, routers: REST twins of glidex.network.v1 ----------

fn body<T: serde::de::DeserializeOwned + buffa::MessageName>(v: Value) -> Result<T, ApiErr> {
    proto_body(v)
}

/// `body` with path fields set over what the body says.
fn body_with<T: serde::de::DeserializeOwned + buffa::MessageName>(
    mut v: Value,
    path: &[(&str, &str)],
) -> Result<T, ApiErr> {
    if let Some(o) = v.as_object_mut() {
        for (k, val) in path {
            o.insert((*k).into(), Value::String((*val).into()));
        }
    }
    body(v)
}

pub async fn list_networks(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        svc::list_networks(&c, Default::default())?,
    ))
}

pub async fn get_network(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetNetworkRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::get_network(&c, req)?))
}

/// `GET /networks/{id}/ovn` (spec/clustering-ui.md §8): a cluster network
/// as OVN has it. Whoever may see the network may see this.
pub async fn network_ovn(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetNetworkOvnRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::network_ovn(&c, req).await?))
}

pub async fn network_events(
    c: Caller,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListNetworkEventsRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::network_events(&c, req)?))
}

pub async fn create_network(c: Caller, Json(v): Json<Value>) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::CreateNetworkRequest = body(v)?;
    Ok(Proto(
        StatusCode::CREATED,
        svc::create_network(&c, req).await?,
    ))
}

/// A deletion request the network controller finishes (spec/
/// reconciliation.md §6.3): `204` once the network is gone (at once, or
/// within `?wait`), else `202` with it (netd unreachable, say).
pub async fn delete_network(
    c: Caller,
    Path(id): Path<String>,
    axum::extract::Query(w): axum::extract::Query<super::storage::WaitQuery>,
) -> Result<axum::response::Response, ApiErr> {
    let req = pb::DeleteNetworkRequest {
        id,
        wait_secs: w.wait.map(|s| s.min(300) as u32),
        ..Default::default()
    };
    Ok(match svc::delete_network(&c, req).await? {
        None => proto_no_content(),
        Some(n) => Proto(
            StatusCode::ACCEPTED,
            pb::DeleteNetworkResponse {
                network: n.into(),
                ..Default::default()
            },
        )
        .into_response(),
    })
}

pub async fn grant_network(
    c: Caller,
    Path(id): Path<String>,
    Json(v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::GrantNetworkRequest = body_with(v, &[("id", &id)])?;
    Ok(Proto(StatusCode::OK, svc::grant_network(&c, req)?))
}

pub async fn create_project_network(
    c: Caller,
    Path(project): Path<String>,
    Json(v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::CreateProjectNetworkRequest = body_with(v, &[("project", &project)])?;
    Ok(Proto(
        StatusCode::CREATED,
        svc::create_project_network(&c, req).await?,
    ))
}

pub async fn list_routers(
    c: Caller,
    Path(project): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListRoutersRequest {
        project,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::list_routers(&c, req)?))
}

pub async fn get_router(
    c: Caller,
    Path((project, router)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetRouterRequest {
        project,
        router,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::get_router(&c, req)?))
}

pub async fn create_router(
    c: Caller,
    Path(project): Path<String>,
    Json(v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::CreateRouterRequest = body_with(v, &[("project", &project)])?;
    Ok(Proto(
        StatusCode::CREATED,
        svc::create_router(&c, req).await?,
    ))
}

/// `PUT /projects/{id}/networks/{network}/router` `{router}`: put a NAT or
/// isolated network on one of the project's routers, or (`null`) back on
/// the shared edge.
pub async fn set_network_router(
    c: Caller,
    Path((project, network)): Path<(String, String)>,
    Json(v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::SetNetworkRouterRequest =
        body_with(v, &[("project", &project), ("network", &network)])?;
    Ok(Proto(
        StatusCode::OK,
        svc::set_network_router(&c, req).await?,
    ))
}

pub async fn delete_router(
    c: Caller,
    Path((project, router)): Path<(String, String)>,
) -> Result<axum::response::Response, ApiErr> {
    let req = pb::DeleteRouterRequest {
        project,
        router,
        ..Default::default()
    };
    svc::delete_router(&c, req).await?;
    Ok(proto_no_content())
}

/// `202` whether or not the target exists (spec/security.md §6.2.1).
pub async fn offer_share(
    c: Caller,
    Path(id): Path<String>,
    Json(v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: pb::OfferNetworkShareRequest = body_with(v, &[("id", &id)])?;
    Ok(Proto(StatusCode::ACCEPTED, svc::offer_share(&c, req)?))
}

pub async fn unshare(
    c: Caller,
    Path((id, project)): Path<(String, String)>,
) -> Result<axum::response::Response, ApiErr> {
    let req = pb::UnshareNetworkRequest {
        id,
        project,
        ..Default::default()
    };
    svc::unshare(&c, req).await?;
    Ok(proto_no_content())
}

pub async fn list_shares(
    c: Caller,
    Path(project): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListNetworkSharesRequest {
        project,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::list_shares(&c, req)?))
}

pub async fn accept_share(
    c: Caller,
    Path((project, network)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::AcceptNetworkShareRequest {
        project,
        network,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::accept_share(&c, req)?))
}

pub async fn leave_share(
    c: Caller,
    Path((project, network)): Path<(String, String)>,
) -> Result<axum::response::Response, ApiErr> {
    let req = pb::LeaveNetworkShareRequest {
        project,
        network,
        ..Default::default()
    };
    svc::leave_share(&c, req).await?;
    Ok(proto_no_content())
}

// ---- host networking: REST twins of glidex.host.v1 -------------------------

fn host_body<T: serde::de::DeserializeOwned + buffa::MessageName>(
    mut v: Value,
    path: &[(&str, &str)],
) -> Result<T, ApiErr> {
    if let Some(o) = v.as_object_mut() {
        for (k, val) in path {
            o.insert((*k).into(), Value::String((*val).into()));
        }
    }
    proto_body(v)
}

pub async fn ovs_status(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        host::ovs_status(&c, Default::default()).await?,
    ))
}

pub async fn ovs_install(c: Caller, Json(v): Json<Value>) -> Result<impl IntoResponse, ApiErr> {
    let req: hpb::InstallOvsRequest = host_body(v, &[])?;
    Ok(Proto(StatusCode::OK, host::install_ovs(&c, req).await?))
}

pub async fn ovs_dpdk_init(
    c: Caller,
    Json(v): Json<Value>,
) -> Result<axum::response::Response, ApiErr> {
    let req: hpb::InitDpdkRequest = host_body(v, &[])?;
    host::init_dpdk(&c, req).await?;
    Ok(proto_no_content())
}

pub async fn list_bridges(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        host::list_bridges(&c, Default::default()).await?,
    ))
}

pub async fn create_bridge(c: Caller, Json(v): Json<Value>) -> Result<impl IntoResponse, ApiErr> {
    let req: hpb::CreateBridgeRequest = host_body(v, &[])?;
    Ok(Proto(
        StatusCode::CREATED,
        host::create_bridge(&c, req).await?,
    ))
}

pub async fn delete_bridge(
    c: Caller,
    Path(name): Path<String>,
) -> Result<axum::response::Response, ApiErr> {
    let req = hpb::DeleteBridgeRequest {
        name,
        ..Default::default()
    };
    host::delete_bridge(&c, req).await?;
    Ok(proto_no_content())
}

pub async fn list_uplinks(
    c: Caller,
    Path(bridge): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = hpb::ListUplinksRequest {
        bridge,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, host::list_uplinks(&c, req).await?))
}

/// `201`, or `202` while the migrated address waits for its commit.
pub async fn create_uplink(
    c: Caller,
    Path(bridge): Path<String>,
    Json(v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: hpb::EnsureUplinkRequest = host_body(v, &[("bridge", &bridge)])?;
    let r = host::ensure_uplink(&c, req).await?;
    let status = match r.uplink.as_option().is_some_and(host::pending) {
        true => StatusCode::ACCEPTED,
        false => StatusCode::CREATED,
    };
    Ok(Proto(status, r))
}

pub async fn commit_uplink(
    c: Caller,
    Path((bridge, name)): Path<(String, String)>,
    Json(v): Json<Value>,
) -> Result<impl IntoResponse, ApiErr> {
    let req: hpb::CommitUplinkRequest = host_body(v, &[("bridge", &bridge), ("name", &name)])?;
    Ok(Proto(StatusCode::OK, host::commit_uplink(&c, req).await?))
}

/// `POST /ovs/bridges/{b}/uplinks/{u}/reapply`: `200`, or `202` while the
/// address waits for its commit.
pub async fn reapply_uplink(
    c: Caller,
    Path((bridge, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = hpb::ReapplyUplinkRequest {
        bridge,
        name,
        ..Default::default()
    };
    let r = host::reapply_uplink(&c, req).await?;
    let status = match r.uplink.as_option().is_some_and(host::pending) {
        true => StatusCode::ACCEPTED,
        false => StatusCode::OK,
    };
    Ok(Proto(status, r))
}

pub async fn delete_uplink(
    c: Caller,
    Path((bridge, name)): Path<(String, String)>,
) -> Result<axum::response::Response, ApiErr> {
    let req = hpb::DeleteUplinkRequest {
        bridge,
        name,
        ..Default::default()
    };
    host::delete_uplink(&c, req).await?;
    Ok(proto_no_content())
}

pub async fn host_topology(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        host::get_host_topology(&c, Default::default())?,
    ))
}

pub async fn list_pci_devices(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    Ok(Proto(
        StatusCode::OK,
        host::list_pci_devices(&c, Default::default())?,
    ))
}
