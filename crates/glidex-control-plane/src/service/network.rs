//! The network family (spec/grpc.md §9, G3): networks, their grants and
//! shares, and VPC routers. Networks are keyed by id; a name resolves
//! among the networks the caller can see (spec/tenancy.md D24).

use super::{Call, ServiceError, ServiceResult};
use crate::api::add_network;
use crate::authz::EntitySet;
use crate::network::{NetError, Network};
use crate::state::VmManagerError;
use axum::http::StatusCode;

/// Network `key` (an id, or a name among host networks and the project
/// networks of the projects the caller sees) if the caller may read it,
/// else not found; a name several have is `409 ambiguous`. The operation's
/// own action is the caller's to check (it may need more context).
pub fn visible_network(call: &Call, key: &str) -> ServiceResult<Network> {
    let not_found = || ServiceError::new(StatusCode::NOT_FOUND, "not_found", "network not found");
    let visible = call.visible_projects()?;
    let n = call
        .manager()
        .find_network_in(key, |p| visible.contains(p))
        .map_err(|e| match e {
            VmManagerError::Network(NetError::Ambiguous(m)) => {
                ServiceError::new(StatusCode::CONFLICT, "ambiguous", m)
            }
            _ => not_found(),
        })?;
    if let Some(p) = &n.project {
        call.set_project(p);
    }
    call.set_target(format!("network:{}", n.id));
    let mut es = EntitySet::new();
    let e = add_network(&mut es, &n);
    if !call.allowed("readNetwork", e, es) {
        return Err(not_found());
    }
    Ok(n)
}

use crate::api::project_entities;
use crate::authz::Ent;
use crate::network::{NetworkMode, NetworkPhase, NetworkScope};
use crate::service::common::{condition_pb, event_pb, ts, ts_opt};
use glidex_api::proto::glidex::network::v1 as pb;
use glidex_ovs::vm_port::VmPortKind;

fn manager_err(e: VmManagerError) -> ServiceError {
    crate::api::error_to_response(e).into()
}

fn invalid(m: impl Into<String>) -> ServiceError {
    ServiceError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", m)
}

fn not_found(what: &str, key: &str) -> ServiceError {
    ServiceError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("{what} not found: {key}"),
    )
}

// ---- conversions ----------------------------------------------------------

fn mode_pb(m: NetworkMode) -> pb::NetworkMode {
    match m {
        NetworkMode::Nat => pb::NetworkMode::NETWORK_MODE_NAT,
        NetworkMode::Bridged => pb::NetworkMode::NETWORK_MODE_BRIDGED,
        NetworkMode::Isolated => pb::NetworkMode::NETWORK_MODE_ISOLATED,
    }
}

fn mode_of(m: pb::NetworkMode) -> ServiceResult<NetworkMode> {
    match m {
        pb::NetworkMode::NETWORK_MODE_NAT => Ok(NetworkMode::Nat),
        pb::NetworkMode::NETWORK_MODE_BRIDGED => Ok(NetworkMode::Bridged),
        pb::NetworkMode::NETWORK_MODE_ISOLATED => Ok(NetworkMode::Isolated),
        pb::NetworkMode::NETWORK_MODE_UNSPECIFIED => Err(invalid("mode is required")),
    }
}

fn scope_pb(s: NetworkScope) -> pb::NetworkScope {
    match s {
        NetworkScope::Node => pb::NetworkScope::NETWORK_SCOPE_NODE,
        NetworkScope::Cluster => pb::NetworkScope::NETWORK_SCOPE_CLUSTER,
    }
}

fn scope_of(s: pb::NetworkScope) -> Option<NetworkScope> {
    match s {
        pb::NetworkScope::NETWORK_SCOPE_NODE => Some(NetworkScope::Node),
        pb::NetworkScope::NETWORK_SCOPE_CLUSTER => Some(NetworkScope::Cluster),
        pb::NetworkScope::NETWORK_SCOPE_UNSPECIFIED => None,
    }
}

fn port_pb(k: VmPortKind) -> pb::PortType {
    match k {
        VmPortKind::Tap => pb::PortType::PORT_TYPE_TAP,
        VmPortKind::VhostUser => pb::PortType::PORT_TYPE_VHOST_USER,
    }
}

fn port_of(k: pb::PortType) -> VmPortKind {
    match k {
        pb::PortType::PORT_TYPE_VHOST_USER => VmPortKind::VhostUser,
        pb::PortType::PORT_TYPE_TAP | pb::PortType::PORT_TYPE_UNSPECIFIED => VmPortKind::Tap,
    }
}

fn phase_pb(p: NetworkPhase) -> pb::NetworkPhase {
    match p {
        NetworkPhase::Pending => pb::NetworkPhase::NETWORK_PHASE_PENDING,
        NetworkPhase::Ready => pb::NetworkPhase::NETWORK_PHASE_READY,
        NetworkPhase::Degraded => pb::NetworkPhase::NETWORK_PHASE_DEGRADED,
        NetworkPhase::NetdUnavailable => pb::NetworkPhase::NETWORK_PHASE_NETD_UNAVAILABLE,
    }
}

pub fn network_pb(n: &Network) -> pb::Network {
    pb::Network {
        id: n.id.clone(),
        name: n.name.clone(),
        scope: scope_pb(n.scope).into(),
        node: n.node.clone(),
        physnet: n.physnet.clone(),
        router: n.router.clone(),
        bridge: n.bridge.clone(),
        mode: mode_pb(n.mode).into(),
        port_type: port_pb(n.port_type).into(),
        vlan: n.vlan.map(u32::from),
        mtu: n.mtu.map(u32::from),
        dns: Some(n.dns),
        owns_bridge: Some(n.owns_bridge),
        created_at: ts(n.created_at).into(),
        project: n.project.clone(),
        all_projects: Some(n.all_projects),
        grants: n.grants.clone(),
        tenant_grants: n.tenant_grants.clone(),
        shares: n.shares.clone(),
        share_offers: n
            .share_offers
            .iter()
            .map(|o| pb::ShareOffer {
                project: o.project.clone(),
                offered_by: o.offered_by.clone(),
                offered_at: ts(o.offered_at).into(),
                expires_at: ts(o.expires_at).into(),
                ..Default::default()
            })
            .collect(),
        phase: phase_pb(n.phase).into(),
        conditions: n.conditions.iter().map(condition_pb).collect(),
        deletion_requested_at: n.deletion_requested_at.and_then(ts_opt).into(),
        ..Default::default()
    }
}

pub fn router_pb(r: &crate::router::Router) -> pb::Router {
    pb::Router {
        id: r.id.clone(),
        project: r.project.clone(),
        spec: pb::RouterSpec {
            name: r.spec.name.clone(),
            external: Some(r.spec.external),
            external_ip: r.spec.external_ip.map(|ip| ip.to_string()),
            gateway_nodes: r.spec.gateway_nodes.clone().unwrap_or_default(),
            ..Default::default()
        }
        .into(),
        status: pb::RouterStatus {
            external_ip: r.status.external_ip.map(|ip| ip.to_string()),
            snat_ct_zone: r.status.snat_ct_zone.map(u32::from),
            ..Default::default()
        }
        .into(),
        created_at: ts(r.created_at).into(),
        ..Default::default()
    }
}

fn u16_of(v: Option<u32>, what: &str) -> ServiceResult<Option<u16>> {
    v.map(|x| u16::try_from(x).map_err(|_| invalid(format!("{what} is out of range"))))
        .transpose()
}

/// A create request as the network code takes it.
#[allow(clippy::too_many_arguments)]
fn spec_of(
    name: String,
    mode: pb::NetworkMode,
    port_type: pb::PortType,
    bridge: Option<String>,
    subnet: Option<String>,
    vlan: Option<u32>,
    mtu: Option<u32>,
    dns: Option<bool>,
    scope: pb::NetworkScope,
    physnet: Option<String>,
    router: Option<String>,
) -> ServiceResult<crate::network::CreateNetworkRequest> {
    Ok(crate::network::CreateNetworkRequest {
        name,
        mode: mode_of(mode)?,
        port_type: port_of(port_type),
        bridge,
        subnet: subnet
            .map(|s| {
                s.parse()
                    .map_err(|e: glidex_ovs::OvsError| invalid(format!("subnet: {e}")))
            })
            .transpose()?,
        vlan: u16_of(vlan, "vlan")?,
        mtu: u16_of(mtu, "mtu")?,
        dns: dns.unwrap_or(true),
        scope: scope_of(scope),
        physnet,
        router,
    })
}

/// Host networks, and project networks of (or shared with) projects the
/// caller can see. Share offers are shown only to those who may manage
/// the network's shares.
pub fn network_view(
    call: &Call,
    visible: &crate::auth::LinkedProjects,
    mut n: Network,
) -> Option<Network> {
    if let Some(p) = &n.project {
        if !visible.contains(p) && !n.shares.iter().any(|s| visible.contains(s)) {
            return None;
        }
        let mut es = EntitySet::new();
        let e = add_network(&mut es, &n);
        es.project(p);
        let manage = call
            .auth()
            .authorize(
                &call.p,
                "unshareNetwork",
                e,
                es,
                &[("project", Ent::Project(p.clone()))],
            )
            .allowed;
        if !manage {
            n.share_offers.clear();
        }
    }
    Some(n)
}

/// `network_view` of a network the caller reached (and may read), or not
/// found.
fn viewed(call: &Call, n: Network) -> ServiceResult<Network> {
    let mut es = EntitySet::new();
    let e = add_network(&mut es, &n);
    call.require(e, es)?;
    let visible = call.visible_projects()?;
    let key = n.id.clone();
    network_view(call, &visible, n).ok_or_else(|| not_found("network", &key))
}

fn resolve_tenants(call: &Call, keys: &[String]) -> ServiceResult<Vec<String>> {
    keys.iter()
        .map(|k| {
            call.auth()
                .tenants
                .resolve(k)
                .map(|t| t.id)
                .map_err(|_| not_found("tenant", k))
        })
        .collect()
}

fn resolve_projects(call: &Call, keys: &[String]) -> ServiceResult<Vec<String>> {
    keys.iter()
        .map(|k| {
            call.manager()
                .projects()
                .resolve(k)
                .map(|p| p.id)
                .map_err(|_| not_found("project", k))
        })
        .collect()
}

// ---- networks -------------------------------------------------------------

pub fn list_networks(
    call: &Call,
    _req: pb::ListNetworksRequest,
) -> ServiceResult<pb::ListNetworksResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let visible = call.visible_projects()?;
    let networks = call
        .manager()
        .list_networks()
        .map_err(manager_err)?
        .into_iter()
        .filter_map(|n| network_view(call, &visible, n))
        .map(|n| network_pb(&n))
        .collect();
    Ok(pb::ListNetworksResponse {
        networks,
        ..Default::default()
    })
}

pub fn get_network(
    call: &Call,
    req: pb::GetNetworkRequest,
) -> ServiceResult<pb::GetNetworkResponse> {
    let n = viewed(call, visible_network(call, &req.id)?)?;
    Ok(pb::GetNetworkResponse {
        network: network_pb(&n).into(),
        ..Default::default()
    })
}

pub async fn create_network(
    call: &Call,
    req: pb::CreateNetworkRequest,
) -> ServiceResult<pb::CreateNetworkResponse> {
    call.set_target(format!("network:{}", req.name));
    call.require(Ent::Host, EntitySet::new())?;
    let grants = if !req.grants.is_empty() {
        resolve_projects(call, &req.grants)?
    } else if req.all_projects || !req.tenants.is_empty() {
        Vec::new()
    } else {
        call.target_project(None).ok().into_iter().collect()
    };
    let tenants = resolve_tenants(call, &req.tenants)?;
    let spec = spec_of(
        req.name,
        req.mode.as_known().unwrap_or_default(),
        req.port_type.as_known().unwrap_or_default(),
        req.bridge,
        req.subnet,
        req.vlan,
        req.mtu,
        req.dns,
        req.scope.as_known().unwrap_or_default(),
        req.physnet,
        req.router,
    )?;
    let n = call
        .manager()
        .create_host_network(spec, grants, tenants, req.all_projects)
        .await
        .map_err(manager_err)?;
    call.set_target(format!("network:{}", n.id));
    Ok(pb::CreateNetworkResponse {
        network: network_pb(&n).into(),
        ..Default::default()
    })
}

/// The network while the controller finishes it; `None` once gone (at
/// once, or within `wait_secs`).
pub async fn delete_network(
    call: &Call,
    req: pb::DeleteNetworkRequest,
) -> ServiceResult<Option<pb::Network>> {
    let n = visible_network(call, &req.id)?;
    let mut es = EntitySet::new();
    let e = add_network(&mut es, &n);
    match &n.project {
        // A project's owner deletes its own networks (spec/security.md §6.2).
        Some(_) => call.require_action("deleteProjectNetwork", e, es, &[])?,
        None => call.require(e, es)?,
    }
    let m = call.manager();
    let Some(still) = m.delete_network(&n.id).await.map_err(manager_err)? else {
        return Ok(None);
    };
    let visible = call.visible_projects()?;
    let Some(still) = network_view(call, &visible, still) else {
        return Ok(None);
    };
    let Some(secs) = req.wait_secs else {
        return Ok(Some(network_pb(&still)));
    };
    let id = n.id.clone();
    Ok(m.wait_gone(
        std::time::Duration::from_secs(u64::from(secs.min(300))),
        || {
            m.networks
                .get(&id)
                .ok()
                .flatten()
                .and_then(|n| network_view(call, &visible, n))
        },
    )
    .await
    .map(|n| network_pb(&n)))
}

pub fn network_events(
    call: &Call,
    req: pb::ListNetworkEventsRequest,
) -> ServiceResult<pb::ListNetworkEventsResponse> {
    let n = viewed(call, visible_network(call, &req.id)?)?;
    let events = call
        .manager()
        .object_events("network", &n.id)
        .map_err(manager_err)?;
    Ok(pb::ListNetworkEventsResponse {
        events: events.iter().map(event_pb).collect(),
        ..Default::default()
    })
}

fn chassis_pb(v: &serde_json::Value) -> Option<pb::Chassis> {
    Some(pb::Chassis {
        chassis: v.get("chassis")?.as_str()?.to_string(),
        node: v["node"].as_str().map(String::from),
        node_name: v["node_name"].as_str().map(String::from),
        priority: v["priority"].as_i64(),
        ..Default::default()
    })
}

pub async fn network_ovn(
    call: &Call,
    req: pb::GetNetworkOvnRequest,
) -> ServiceResult<pb::GetNetworkOvnResponse> {
    let n = viewed(call, visible_network(call, &req.id)?)?;
    let v = call.manager().network_ovn(&n).await.map_err(manager_err)?;
    let strs = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let ports = v["ports"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| pb::OvnPort {
                    name: p["name"].as_str().unwrap_or_default().to_string(),
                    kind: p["kind"].as_str().unwrap_or_default().to_string(),
                    addresses: strs(&p["addresses"]),
                    up: p["up"].as_bool(),
                    bound: chassis_pb(&p["bound"]).into(),
                    vm: p["vm"]["id"]
                        .as_str()
                        .map(|id| pb::VmRef {
                            id: id.to_string(),
                            name: p["vm"]["name"].as_str().unwrap_or_default().to_string(),
                            ..Default::default()
                        })
                        .into(),
                    ..Default::default()
                })
                .collect()
        })
        .unwrap_or_default();
    let r = &v["router"];
    let router = r["name"].as_str().map(|name| pb::OvnRouter {
        name: name.to_string(),
        nat: strs(&r["nat"]),
        active_gateway: chassis_pb(&r["active_gateway"]).into(),
        gateway_candidates: r["gateway_candidates"]
            .as_array()
            .map(|a| a.iter().filter_map(chassis_pb).collect())
            .unwrap_or_default(),
        ..Default::default()
    });
    Ok(pb::GetNetworkOvnResponse {
        switch: v["switch"].as_str().unwrap_or_default().to_string(),
        ports,
        router: router.into(),
        ..Default::default()
    })
}

pub fn grant_network(
    call: &Call,
    req: pb::GrantNetworkRequest,
) -> ServiceResult<pb::GrantNetworkResponse> {
    let n = visible_network(call, &req.id)?;
    let mut es = EntitySet::new();
    let e = add_network(&mut es, &n);
    call.require(e, es)?;
    let grants = resolve_projects(call, &req.grants)?;
    let tenants = resolve_tenants(call, &req.tenants)?;
    call.detail("tenants", serde_json::json!(tenants));
    let n = call
        .manager()
        .grant_network(&n.id, grants, tenants, req.all_projects)
        .map_err(manager_err)?;
    Ok(pb::GrantNetworkResponse {
        network: network_pb(&n).into(),
        ..Default::default()
    })
}

pub async fn create_project_network(
    call: &Call,
    req: pb::CreateProjectNetworkRequest,
) -> ServiceResult<pb::CreateProjectNetworkResponse> {
    let project = call.target_project(Some(&req.project))?;
    call.set_project(&project);
    call.set_target(format!("network:{}", req.name));
    call.require(Ent::Project(project.clone()), project_entities(&project))?;
    call.require_admitted(&project)?;
    let spec = spec_of(
        req.name,
        req.mode.as_known().unwrap_or_default(),
        req.port_type.as_known().unwrap_or_default(),
        None,
        req.subnet,
        req.vlan,
        req.mtu,
        req.dns,
        req.scope.as_known().unwrap_or_default(),
        None,
        req.router,
    )?;
    let quota = call.quota_mode(&project);
    let (n, over) = call
        .manager()
        .create_project_network(&project, spec, quota)
        .await
        .map_err(manager_err)?;
    call.note_overruns(&over);
    call.set_target(format!("network:{}", n.id));
    Ok(pb::CreateProjectNetworkResponse {
        network: network_pb(&n).into(),
        ..Default::default()
    })
}

pub async fn set_network_router(
    call: &Call,
    req: pb::SetNetworkRouterRequest,
) -> ServiceResult<pb::SetNetworkRouterResponse> {
    let project = call.target_project(Some(&req.project))?;
    call.set_project(&project);
    call.require(Ent::Project(project.clone()), project_entities(&project))?;
    let m = call.manager();
    let n = m
        .find_network(&project, &req.network)
        .ok()
        .filter(|n| n.project.as_deref() == Some(project.as_str()))
        .ok_or_else(|| not_found("network", &req.network))?;
    call.set_target(format!("network:{}", n.id));
    let n = m
        .set_network_router(&project, &n.id, req.router.as_deref())
        .await
        .map_err(manager_err)?;
    Ok(pb::SetNetworkRouterResponse {
        network: network_pb(&n).into(),
        ..Default::default()
    })
}

// ---- sharing project networks (spec/security.md §6.2.1) -------------------

pub fn offer_share(
    call: &Call,
    req: pb::OfferNetworkShareRequest,
) -> ServiceResult<pb::OfferNetworkShareResponse> {
    let n = visible_network(call, &req.id)?;
    let mut es = EntitySet::new();
    let e = add_network(&mut es, &n);
    es.project(&req.project);
    // D25: only within the network's tenant. A target elsewhere gets the
    // answer a missing one does, so offers can't probe other tenants.
    let tenant_of = |p: &str| {
        call.manager()
            .projects()
            .get(p)
            .ok()
            .flatten()
            .map(|p| p.tenant_id)
    };
    let same_tenant = match (
        n.project.as_deref().and_then(tenant_of),
        tenant_of(&req.project),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    call.detail("to_project", serde_json::json!(req.project));
    if !same_tenant {
        call.detail("refused", serde_json::json!("another tenant"));
        return Ok(Default::default());
    }
    call.require_action(
        "offerNetworkShare",
        e,
        es,
        &[("project", Ent::Project(req.project.clone()))],
    )?;
    let by = call.p.user_id().unwrap_or("-").to_string();
    call.manager()
        .offer_network_share(&n.id, &req.project, &by)
        .map_err(manager_err)?;
    // The same answer whether or not the target exists.
    Ok(Default::default())
}

pub async fn unshare(
    call: &Call,
    req: pb::UnshareNetworkRequest,
) -> ServiceResult<pb::UnshareNetworkResponse> {
    let n = visible_network(call, &req.id)?;
    let mut es = EntitySet::new();
    let e = add_network(&mut es, &n);
    es.project(&req.project);
    call.require_action(
        "unshareNetwork",
        e,
        es,
        &[("project", Ent::Project(req.project.clone()))],
    )?;
    call.manager()
        .end_network_share(&n.id, &req.project)
        .await
        .map_err(manager_err)?;
    Ok(Default::default())
}

pub fn list_shares(
    call: &Call,
    req: pb::ListNetworkSharesRequest,
) -> ServiceResult<pb::ListNetworkSharesResponse> {
    let project = call.target_project(Some(&req.project))?;
    call.require(Ent::Project(project.clone()), project_entities(&project))?;
    let shares = call
        .manager()
        .network_shares_for(&project)
        .map_err(manager_err)?
        .into_iter()
        .map(|n| {
            let accepted = n.shares.contains(&project);
            pb::NetworkShare {
                expires_at: (!accepted)
                    .then(|| {
                        n.share_offers
                            .iter()
                            .find(|o| o.project == project)
                            .map(|o| ts(o.expires_at))
                    })
                    .flatten()
                    .into(),
                status: if accepted {
                    pb::ShareStatus::SHARE_STATUS_ACCEPTED
                } else {
                    pb::ShareStatus::SHARE_STATUS_OFFERED
                }
                .into(),
                owner_project: n.project.clone(),
                network: n.id,
                network_name: n.name,
                ..Default::default()
            }
        })
        .collect();
    Ok(pb::ListNetworkSharesResponse {
        shares,
        ..Default::default()
    })
}

/// The network a share of `project` is about: by id, or by a name among
/// the networks offered to or shared with it.
fn share_target(
    call: &Call,
    project: &str,
    network: &str,
    action: &str,
) -> ServiceResult<(String, Network)> {
    let project = call.target_project(Some(project))?;
    call.set_project(&project);
    let m = call.manager();
    let n = match m.get_network(network) {
        Ok(n) => n,
        Err(_) => {
            let mut named = m
                .network_shares_for(&project)
                .map_err(manager_err)?
                .into_iter()
                .filter(|n| n.name == network);
            match (named.next(), named.next()) {
                (Some(n), None) => n,
                (Some(_), Some(_)) => {
                    return Err(ServiceError::new(
                        StatusCode::CONFLICT,
                        "ambiguous",
                        format!("more than one network is named {network}: give its id"),
                    ));
                }
                (None, _) => return Err(not_found("network", network)),
            }
        }
    };
    call.set_target(format!("network:{}", n.id));
    let mut es = project_entities(&project);
    let ne = add_network(&mut es, &n);
    call.require_action(
        action,
        Ent::Project(project.clone()),
        es,
        &[("network", ne)],
    )?;
    Ok((project, n))
}

pub fn accept_share(
    call: &Call,
    req: pb::AcceptNetworkShareRequest,
) -> ServiceResult<pb::AcceptNetworkShareResponse> {
    let (project, n) = share_target(call, &req.project, &req.network, "acceptNetworkShare")?;
    let n = call
        .manager()
        .accept_network_share(&project, &n.id)
        .map_err(manager_err)?;
    Ok(pb::AcceptNetworkShareResponse {
        network: network_pb(&n).into(),
        ..Default::default()
    })
}

pub async fn leave_share(
    call: &Call,
    req: pb::LeaveNetworkShareRequest,
) -> ServiceResult<pb::LeaveNetworkShareResponse> {
    let (project, n) = share_target(call, &req.project, &req.network, "leaveNetworkShare")?;
    call.manager()
        .end_network_share(&n.id, &project)
        .await
        .map_err(manager_err)?;
    Ok(Default::default())
}

// ---- VPC routers (spec/clustering.md §11.2a) ------------------------------

fn router_project(call: &Call, project: &str) -> ServiceResult<String> {
    let project = call.target_project(Some(project))?;
    call.set_project(&project);
    call.require(Ent::Project(project.clone()), project_entities(&project))?;
    Ok(project)
}

pub fn list_routers(
    call: &Call,
    req: pb::ListRoutersRequest,
) -> ServiceResult<pb::ListRoutersResponse> {
    let project = router_project(call, &req.project)?;
    let routers = call
        .manager()
        .list_routers(&project)
        .map_err(manager_err)?
        .iter()
        .map(router_pb)
        .collect();
    Ok(pb::ListRoutersResponse {
        routers,
        ..Default::default()
    })
}

pub fn get_router(call: &Call, req: pb::GetRouterRequest) -> ServiceResult<pb::GetRouterResponse> {
    let project = router_project(call, &req.project)?;
    let r = call
        .manager()
        .get_router(&project, &req.router)
        .map_err(manager_err)?;
    Ok(pb::GetRouterResponse {
        router: router_pb(&r).into(),
        ..Default::default()
    })
}

pub async fn create_router(
    call: &Call,
    req: pb::CreateRouterRequest,
) -> ServiceResult<pb::CreateRouterResponse> {
    call.set_target(format!("router:{}", req.name));
    let project = router_project(call, &req.project)?;
    call.require_admitted(&project)?;
    let spec = crate::router::RouterSpec {
        name: req.name,
        external: req.external.unwrap_or(true),
        external_ip: req
            .external_ip
            .map(|s| {
                s.parse()
                    .map_err(|_| invalid(format!("external_ip: {s} is not an IPv4 address")))
            })
            .transpose()?,
        gateway_nodes: (!req.gateway_nodes.is_empty()).then_some(req.gateway_nodes),
    };
    // The external pool and the gateway nodes are site resources.
    if spec.external_ip.is_some() || spec.gateway_nodes.is_some() {
        call.require_action("createNetwork", Ent::Host, EntitySet::new(), &[])?;
    }
    // Its own gateway nodes are nodes in the cluster, able to carry it.
    if let Some(gws) = spec.gateway_nodes.as_ref() {
        let nodes = call.manager().nodes().list().unwrap_or_default();
        for g in gws {
            crate::api::nodes::live_gateway_node(&nodes, g)
                .map_err(|m| ServiceError::new(StatusCode::BAD_REQUEST, "invalid", m))?;
        }
        if let (true, Some(edge)) = (spec.external, call.manager().ovn.load().edge.clone()) {
            crate::api::nodes::check_gateway_nodes(&nodes, gws, &edge.physnet)
                .map_err(|m| ServiceError::new(StatusCode::CONFLICT, "gateway_not_ready", m))?;
        }
    }
    let quota = call.quota_mode(&project);
    let (r, over) = call
        .manager()
        .create_router(&project, spec, quota)
        .await
        .map_err(manager_err)?;
    call.note_overruns(&over);
    Ok(pb::CreateRouterResponse {
        router: router_pb(&r).into(),
        ..Default::default()
    })
}

pub async fn delete_router(
    call: &Call,
    req: pb::DeleteRouterRequest,
) -> ServiceResult<pb::DeleteRouterResponse> {
    call.set_target(format!("router:{}", req.router));
    let project = router_project(call, &req.project)?;
    let r = call
        .manager()
        .get_router(&project, &req.router)
        .map_err(manager_err)?;
    call.manager()
        .delete_router(&project, &r.spec.name)
        .await
        .map_err(manager_err)?;
    Ok(Default::default())
}
