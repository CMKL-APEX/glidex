//! Networks (host and project networks, grants, shares) and host
//! networking through glidex-netd (spec/networking.md §11,
//! spec/security.md §6.2, §8).

use super::{ApiErr, Caller, Proto, error_to_response, proto_body, proto_no_content};
use crate::authz::{Ent, EntitySet};
use crate::models::ApiError;
use crate::network::{NetError, Netd, NetdAccess};
use crate::service::network as svc;
use crate::state::VmManagerError;
use axum::{Json, extract::Path, http::StatusCode, response::IntoResponse};
use glidex_api::proto::glidex::network::v1 as pb;
use glidex_netd::proto::{
    BridgeRecord, EnsureUplinkArgs, OnBehalfOf, Op, UplinkPhase, UplinkResult,
};
use glidex_ovs::bridge::BridgeSpec;
use glidex_ovs::install::{InstallReport, InstallRequest};
use glidex_ovs::uplink::{UplinkKind, UplinkSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn manager_err(e: VmManagerError) -> ApiErr {
    error_to_response(e)
}

fn on_behalf_of(c: &Caller) -> OnBehalfOf {
    OnBehalfOf {
        user: c
            .p
            .user_id()
            .map(String::from)
            .unwrap_or_else(|| c.p.display()),
        project: None,
        request_id: Some(c.request_id.clone()),
    }
}

/// Run a blocking netd call off the async runtime, on behalf of `c`.
async fn netd_blocking<T, F>(c: &Caller, f: F) -> Result<T, ApiErr>
where
    T: Send + 'static,
    F: FnOnce(&Netd, &OnBehalfOf) -> Result<T, NetError> + Send + 'static,
{
    let netd = c.manager().netd().clone();
    let who = on_behalf_of(c);
    tokio::task::spawn_blocking(move || f(&netd, &who))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new("internal", e.to_string())),
            )
        })?
        .map_err(|e| error_to_response(VmManagerError::Network(e)))
}

// ---- networks, shares, routers: REST twins of glidex.network.v1 ----------

/// The old shape's enum values (spec/grpc.md §9.1 step 3).
const ENUMS: &[(&str, &str)] = &[
    ("mode", "NETWORK_MODE"),
    ("port_type", "PORT_TYPE"),
    ("scope", "NETWORK_SCOPE"),
];

fn body<T: serde::de::DeserializeOwned + buffa::MessageName>(v: Value) -> Result<T, ApiErr> {
    proto_body(v, ENUMS, &[])
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

/// `GET /cluster/ovn/tunnels` (spec/clustering-ui.md §8b): each node's tunnels.
pub async fn cluster_ovn_tunnels(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(crate::authz::Ent::Cluster, crate::authz::EntitySet::new())?;
    Ok(Json(c.manager().ovn_tunnels().await))
}

/// `GET /cluster/ovn` (spec/clustering-ui.md §8): the OVN databases, chassis and gateways.
pub async fn cluster_ovn(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(crate::authz::Ent::Cluster, crate::authz::EntitySet::new())?;
    Ok(Json(c.manager().ovn_overview().await.map_err(manager_err)?))
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

// ---- host networking -----------------------------------------------------

#[derive(Serialize)]
struct OvsStatus {
    netd: NetdStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct NetdStatus {
    available: bool,
    access: NetdAccess,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub async fn ovs_status(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    let netd = c.manager().netd().clone();
    let status = tokio::task::spawn_blocking(move || {
        let (access, result) = netd.probe();
        match result {
            Ok(host) => OvsStatus {
                netd: NetdStatus {
                    available: true,
                    access,
                    error: None,
                },
                host: Some(host),
            },
            Err(e) => OvsStatus {
                netd: NetdStatus {
                    available: false,
                    access,
                    error: Some(e.to_string()),
                },
                host: None,
            },
        }
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new("internal", e.to_string())),
        )
    })?;
    Ok(Json(status))
}

pub async fn ovs_install(
    c: Caller,
    Json(req): Json<InstallRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    let report: InstallReport =
        netd_blocking(&c, move |n, who| n.call_as(Op::InstallOvs(req), who)).await?;
    Ok(Json(report))
}

pub async fn ovs_dpdk_init(
    c: Caller,
    Json(settings): Json<glidex_ovs::install::DpdkSettings>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    netd_blocking(&c, move |n, who| {
        n.call_as::<serde_json::Value>(Op::InitDpdk(settings), who)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_bridges(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    let bridges: Vec<BridgeRecord> = netd_blocking(&c, |n, _| n.call(Op::ListBridges)).await?;
    Ok(Json(bridges))
}

pub async fn create_bridge(
    c: Caller,
    Json(spec): Json<BridgeSpec>,
) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("bridge:{}", spec.name));
    c.require(Ent::Host, EntitySet::new())?;
    let bridge: BridgeRecord =
        netd_blocking(&c, move |n, who| n.call_as(Op::EnsureBridge(spec), who)).await?;
    Ok((StatusCode::CREATED, Json(bridge)))
}

pub async fn delete_bridge(
    c: Caller,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("bridge:{}", name));
    c.require(Ent::Host, EntitySet::new())?;
    if let Some(net) = c
        .manager()
        .list_networks()
        .map_err(manager_err)?
        .into_iter()
        .find(|n| n.bridge == name)
    {
        return Err(manager_err(VmManagerError::Network(NetError::Conflict(
            format!("bridge '{}' is used by network '{}'", name, net.name),
        ))));
    }
    c.manager().meter_final_sample().await;
    netd_blocking(&c, move |n, who| {
        n.call_as::<serde_json::Value>(Op::DeleteBridge { name }, who)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_uplinks(
    c: Caller,
    Path(bridge): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    let all: Vec<UplinkResult> = netd_blocking(&c, |n, _| n.call(Op::ListUplinks)).await?;
    Ok(Json(
        all.into_iter()
            .filter(|u| u.record.spec.bridge == bridge)
            .collect::<Vec<_>>(),
    ))
}

#[derive(Deserialize)]
pub struct CreateUplinkRequest {
    name: String,
    #[serde(flatten)]
    kind: UplinkKind,
    #[serde(default)]
    migrate_ip: bool,
    #[serde(default)]
    confirm: bool,
    /// Migrate a DHCP-managed address anyway (it may only last until the lease ends).
    #[serde(default)]
    accept_dhcp: bool,
}

pub async fn create_uplink(
    c: Caller,
    Path(bridge): Path<String>,
    Json(req): Json<CreateUplinkRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("uplink:{}/{}", bridge, req.name));
    // A confirmed uplink may move the host's IP: step-up (spec §7.1).
    let action = if req.confirm {
        "confirmUplink"
    } else {
        "ensureUplink"
    };
    c.require_action(action, Ent::Host, EntitySet::new(), &[])?;
    let args = EnsureUplinkArgs {
        spec: UplinkSpec {
            name: req.name,
            bridge,
            kind: req.kind,
            migrate_ip: req.migrate_ip,
        },
        confirm: req.confirm,
        accept_dhcp: req.accept_dhcp,
    };
    let res: UplinkResult =
        netd_blocking(&c, move |n, who| n.call_as(Op::EnsureUplink(args), who)).await?;
    report_soon(&c);
    // 202: the IP migration must still be committed within the window.
    let status = match res.phase {
        UplinkPhase::PendingCommit => StatusCode::ACCEPTED,
        UplinkPhase::Active => StatusCode::CREATED,
    };
    Ok((status, Json(res)))
}

#[derive(Deserialize)]
pub struct CommitRequest {
    token: String,
}

pub async fn commit_uplink(
    c: Caller,
    Path((bridge, name)): Path<(String, String)>,
    Json(req): Json<CommitRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("uplink:{}/{}", bridge, name));
    c.require(Ent::Host, EntitySet::new())?;
    c.manager().meter_final_sample().await;
    let res: UplinkResult = netd_blocking(&c, move |n, who| {
        n.call_as(
            Op::CommitUplink {
                bridge,
                name,
                token: req.token,
            },
            who,
        )
    })
    .await?;
    report_soon(&c);
    Ok(Json(res))
}

/// `POST /ovs/bridges/{b}/uplinks/{u}/reapply`: set up again an uplink netd
/// rolled back at start (the gateway check failed there): a fresh snapshot,
/// the gateway check and a commit window, as when it was first added.
pub async fn reapply_uplink(
    c: Caller,
    Path((bridge, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("uplink:{}/{}", bridge, name));
    // It moves the host's IP again: as a confirmed uplink, step-up.
    c.require_action("confirmUplink", Ent::Host, EntitySet::new(), &[])?;
    let res: UplinkResult = netd_blocking(&c, move |n, who| {
        n.call_as(Op::ReapplyUplink { bridge, name }, who)
    })
    .await?;
    report_soon(&c);
    let status = match res.phase {
        UplinkPhase::PendingCommit => StatusCode::ACCEPTED,
        UplinkPhase::Active => StatusCode::OK,
    };
    Ok((status, Json(res)))
}

/// This node's uplinks changed: report what it has now (the shared edge's
/// defaults read it), without waiting for the next periodic report.
fn report_soon(c: &Caller) {
    let m = c.app.manager.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = m.report_self() {
            tracing::warn!("reporting this node after an uplink change: {}", e);
        }
    });
}

pub async fn delete_uplink(
    c: Caller,
    Path((bridge, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("uplink:{}/{}", bridge, name));
    c.require(Ent::Host, EntitySet::new())?;
    c.manager().meter_final_sample().await;
    netd_blocking(&c, move |n, who| {
        n.call_as::<serde_json::Value>(Op::DeleteUplink { bridge, name }, who)
    })
    .await?;
    report_soon(&c);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_pci_devices(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    Ok(Json(crate::pci::scan_pci_devices()))
}
