//! Networks (host and project networks, grants, shares) and host
//! networking through glidex-netd (spec/networking.md §11,
//! spec/security.md §6.2, §8).

use super::{add_network, err, error_to_response, ApiErr, Caller};
use crate::authz::{Ent, EntitySet};
use crate::models::ApiError;
use crate::network::{CreateNetworkRequest, NetError, Netd, NetdAccess, Network};
use crate::state::VmManagerError;
use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use glidex_netd::proto::{BridgeRecord, EnsureUplinkArgs, OnBehalfOf, Op, UplinkPhase, UplinkResult};
use glidex_ovs::bridge::BridgeSpec;
use glidex_ovs::install::{InstallReport, InstallRequest};
use glidex_ovs::uplink::{UplinkKind, UplinkSpec};
use serde::{Deserialize, Serialize};

fn manager_err(e: VmManagerError) -> ApiErr {
    error_to_response(e)
}

fn on_behalf_of(c: &Caller) -> OnBehalfOf {
    OnBehalfOf {
        user: c.p.user_id().map(String::from).unwrap_or_else(|| c.p.display()),
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError::new("internal", e.to_string()))))?
        .map_err(|e| error_to_response(VmManagerError::Network(e)))
}

fn network_entities(n: &Network) -> (Ent, EntitySet) {
    let mut es = EntitySet::new();
    let e = add_network(&mut es, n);
    (e, es)
}

// ---- networks ------------------------------------------------------------

/// Host networks, and project networks of (or shared with) projects the
/// caller can see. Share offers are shown only to those who may manage
/// the network's shares.
fn network_view(c: &Caller, visible: &crate::auth::LinkedProjects, mut n: Network) -> Option<Network> {
    if let Some(p) = &n.project {
        if !visible.contains(p) && !n.shares.iter().any(|s| visible.contains(s)) {
            return None;
        }
        let (e, mut es) = network_entities(&n);
        es.project(p);
        let manage = c.auth().authorize(&c.p, "unshareNetwork", e, es, &[("project", Ent::Project(p.clone()))]).allowed;
        if !manage {
            n.share_offers.clear();
        }
    }
    Some(n)
}

pub async fn list_networks(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    let visible = c.visible_projects()?;
    let nets: Vec<Network> = c
        .manager()
        .list_networks()
        .map_err(manager_err)?
        .into_iter()
        .filter_map(|n| network_view(&c, &visible, n))
        .collect();
    Ok(Json(nets))
}

pub async fn get_network(c: Caller, Path(name): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let n = c.manager().get_network(&name).map_err(manager_err)?;
    let (e, es) = network_entities(&n);
    c.require(e, es)?;
    let visible = c.visible_projects()?;
    network_view(&c, &visible, n)
        .map(Json)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", format!("network not found: {}", name)))
}

pub async fn network_events(c: Caller, Path(name): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let n = c.manager().get_network(&name).map_err(manager_err)?;
    let (e, es) = network_entities(&n);
    c.require(e, es)?;
    let visible = c.visible_projects()?;
    if network_view(&c, &visible, n.clone()).is_none() {
        return Err(err(StatusCode::NOT_FOUND, "not_found", format!("network not found: {}", name)));
    }
    Ok(Json(serde_json::json!({ "events": c.manager().object_events("network", &n.name).map_err(manager_err)? })))
}

#[derive(Deserialize)]
pub struct CreateHostNetwork {
    #[serde(flatten)]
    req: CreateNetworkRequest,
    /// Projects (ids or names) that may use the network. Default: the
    /// caller's default project.
    #[serde(default)]
    grants: Option<Vec<String>>,
    #[serde(default)]
    all_projects: bool,
}

fn resolve_projects(c: &Caller, keys: &[String]) -> Result<Vec<String>, ApiErr> {
    keys.iter()
        .map(|k| {
            c.manager()
                .projects()
                .resolve(k)
                .map(|p| p.id)
                .map_err(|_| err(StatusCode::NOT_FOUND, "not_found", format!("project {} not found", k)))
        })
        .collect()
}

pub async fn create_network(c: Caller, Json(body): Json<CreateHostNetwork>) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("network:{}", body.req.name));
    c.require(Ent::Host, EntitySet::new())?;
    let grants = match &body.grants {
        Some(g) => resolve_projects(&c, g)?,
        None if body.all_projects => Vec::new(),
        None => c.target_project(None).ok().into_iter().collect(),
    };
    let net = c
        .manager()
        .create_host_network(body.req, grants, body.all_projects)
        .await
        .map_err(manager_err)?;
    Ok((StatusCode::CREATED, Json(net)))
}

pub async fn delete_network(c: Caller, Path(name): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let n = c.manager().get_network(&name).map_err(manager_err)?;
    c.set_target(format!("network:{}", n.name));
    let (e, es) = network_entities(&n);
    match &n.project {
        // A project's owner deletes its own networks (spec §6.2).
        Some(p) => {
            c.set_project(p);
            c.require_action("deleteProjectNetwork", e, es, &[])?
        }
        None => c.require(e, es)?,
    }
    c.manager().delete_network(&name).await.map_err(manager_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct GrantBody {
    #[serde(default)]
    grants: Vec<String>,
    #[serde(default)]
    all_projects: bool,
}

pub async fn grant_network(c: Caller, Path(name): Path<String>, Json(body): Json<GrantBody>) -> Result<impl IntoResponse, ApiErr> {
    let n = c.manager().get_network(&name).map_err(manager_err)?;
    c.set_target(format!("network:{}", n.name));
    let (e, es) = network_entities(&n);
    c.require(e, es)?;
    let grants = resolve_projects(&c, &body.grants)?;
    Ok(Json(c.manager().grant_network(&name, grants, body.all_projects).map_err(manager_err)?))
}

pub async fn create_project_network(
    c: Caller,
    Path(project): Path<String>,
    Json(req): Json<CreateNetworkRequest>,
) -> Result<impl IntoResponse, ApiErr> {
    let project = c.target_project(Some(&project))?;
    c.set_project(&project);
    c.set_target(format!("network:{}", req.name));
    c.require(Ent::Project(project.clone()), super::project_entities(&project))?;
    let quota = c.quota_mode(&project);
    let (net, over) = c.manager().create_project_network(&project, req, quota).await.map_err(manager_err)?;
    c.note_overruns(&over);
    Ok((StatusCode::CREATED, Json(net)))
}

// ---- sharing project networks (spec §6.2.1) -------------------------------

#[derive(Deserialize)]
pub struct OfferBody {
    /// Target project id.
    project: String,
}

pub async fn offer_share(c: Caller, Path(name): Path<String>, Json(body): Json<OfferBody>) -> Result<impl IntoResponse, ApiErr> {
    let n = c.manager().get_network(&name).map_err(manager_err)?;
    c.set_target(format!("network:{}", n.name));
    if let Some(p) = &n.project {
        c.set_project(p);
    }
    let (e, mut es) = network_entities(&n);
    es.project(&body.project);
    c.require_action("offerNetworkShare", e, es, &[("project", Ent::Project(body.project.clone()))])?;
    c.detail("to_project", serde_json::json!(body.project));
    let by = c.p.user_id().unwrap_or("-").to_string();
    c.manager().offer_network_share(&name, &body.project, &by).map_err(manager_err)?;
    // The same answer whether or not the target exists (spec §6.2.1).
    Ok(StatusCode::ACCEPTED)
}

pub async fn unshare(c: Caller, Path((name, project)): Path<(String, String)>) -> Result<impl IntoResponse, ApiErr> {
    let n = c.manager().get_network(&name).map_err(manager_err)?;
    c.set_target(format!("network:{}", n.name));
    let (e, mut es) = network_entities(&n);
    es.project(&project);
    c.require_action("unshareNetwork", e, es, &[("project", Ent::Project(project.clone()))])?;
    c.manager().end_network_share(&name, &project).await.map_err(manager_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct ShareView {
    network: String,
    owner_project: Option<String>,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
}

pub async fn list_shares(c: Caller, Path(project): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let project = c.target_project(Some(&project))?;
    c.require(Ent::Project(project.clone()), super::project_entities(&project))?;
    let out: Vec<ShareView> = c
        .manager()
        .network_shares_for(&project)
        .map_err(manager_err)?
        .into_iter()
        .map(|n| {
            let accepted = n.shares.contains(&project);
            ShareView {
                expires_at: (!accepted).then(|| n.share_offers.iter().find(|o| o.project == project).map(|o| o.expires_at)).flatten(),
                status: if accepted { "accepted" } else { "offered" },
                owner_project: n.project.clone(),
                network: n.name,
            }
        })
        .collect();
    Ok(Json(out))
}

async fn share_target(c: &Caller, project: &str, network: &str, action: &str) -> Result<(String, Network), ApiErr> {
    let project = c.target_project(Some(project))?;
    c.set_project(&project);
    c.set_target(format!("network:{}", network));
    let n = c.manager().get_network(network).map_err(manager_err)?;
    let mut es = super::project_entities(&project);
    let ne = add_network(&mut es, &n);
    c.require_action(action, Ent::Project(project.clone()), es, &[("network", ne)])?;
    Ok((project, n))
}

pub async fn accept_share(c: Caller, Path((project, network)): Path<(String, String)>) -> Result<impl IntoResponse, ApiErr> {
    let (project, _) = share_target(&c, &project, &network, "acceptNetworkShare").await?;
    Ok(Json(c.manager().accept_network_share(&project, &network).map_err(manager_err)?))
}

pub async fn leave_share(c: Caller, Path((project, network)): Path<(String, String)>) -> Result<impl IntoResponse, ApiErr> {
    let (project, _) = share_target(&c, &project, &network, "leaveNetworkShare").await?;
    c.manager().end_network_share(&network, &project).await.map_err(manager_err)?;
    Ok(StatusCode::NO_CONTENT)
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
            Ok(host) => OvsStatus { netd: NetdStatus { available: true, access, error: None }, host: Some(host) },
            Err(e) => OvsStatus { netd: NetdStatus { available: false, access, error: Some(e.to_string()) }, host: None },
        }
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError::new("internal", e.to_string()))))?;
    Ok(Json(status))
}

pub async fn ovs_install(c: Caller, Json(req): Json<InstallRequest>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    let report: InstallReport = netd_blocking(&c, move |n, who| n.call_as(Op::InstallOvs(req), who)).await?;
    Ok(Json(report))
}

pub async fn ovs_dpdk_init(c: Caller, Json(settings): Json<glidex_ovs::install::DpdkSettings>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    netd_blocking(&c, move |n, who| n.call_as::<serde_json::Value>(Op::InitDpdk(settings), who)).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_bridges(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    let bridges: Vec<BridgeRecord> = netd_blocking(&c, |n, _| n.call(Op::ListBridges)).await?;
    Ok(Json(bridges))
}

pub async fn create_bridge(c: Caller, Json(spec): Json<BridgeSpec>) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("bridge:{}", spec.name));
    c.require(Ent::Host, EntitySet::new())?;
    let bridge: BridgeRecord = netd_blocking(&c, move |n, who| n.call_as(Op::EnsureBridge(spec), who)).await?;
    Ok((StatusCode::CREATED, Json(bridge)))
}

pub async fn delete_bridge(c: Caller, Path(name): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("bridge:{}", name));
    c.require(Ent::Host, EntitySet::new())?;
    if let Some(net) = c.manager().list_networks().map_err(manager_err)?.into_iter().find(|n| n.bridge == name) {
        return Err(manager_err(VmManagerError::Network(NetError::Conflict(format!(
            "bridge '{}' is used by network '{}'",
            name, net.name
        )))));
    }
    netd_blocking(&c, move |n, who| n.call_as::<serde_json::Value>(Op::DeleteBridge { name }, who)).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_uplinks(c: Caller, Path(bridge): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    let all: Vec<UplinkResult> = netd_blocking(&c, |n, _| n.call(Op::ListUplinks)).await?;
    Ok(Json(all.into_iter().filter(|u| u.record.spec.bridge == bridge).collect::<Vec<_>>()))
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
}

pub async fn create_uplink(c: Caller, Path(bridge): Path<String>, Json(req): Json<CreateUplinkRequest>) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("uplink:{}/{}", bridge, req.name));
    // A confirmed uplink may move the host's IP: step-up (spec §7.1).
    let action = if req.confirm { "confirmUplink" } else { "ensureUplink" };
    c.require_action(action, Ent::Host, EntitySet::new(), &[])?;
    let args = EnsureUplinkArgs {
        spec: UplinkSpec { name: req.name, bridge, kind: req.kind, migrate_ip: req.migrate_ip },
        confirm: req.confirm,
    };
    let res: UplinkResult = netd_blocking(&c, move |n, who| n.call_as(Op::EnsureUplink(args), who)).await?;
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

pub async fn commit_uplink(c: Caller, Path((bridge, name)): Path<(String, String)>, Json(req): Json<CommitRequest>) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("uplink:{}/{}", bridge, name));
    c.require(Ent::Host, EntitySet::new())?;
    let res: UplinkResult =
        netd_blocking(&c, move |n, who| n.call_as(Op::CommitUplink { bridge, name, token: req.token }, who)).await?;
    Ok(Json(res))
}

pub async fn delete_uplink(c: Caller, Path((bridge, name)): Path<(String, String)>) -> Result<impl IntoResponse, ApiErr> {
    c.set_target(format!("uplink:{}/{}", bridge, name));
    c.require(Ent::Host, EntitySet::new())?;
    netd_blocking(&c, move |n, who| n.call_as::<serde_json::Value>(Op::DeleteUplink { bridge, name }, who)).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_pci_devices(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    Ok(Json(crate::pci::scan_pci_devices()))
}
