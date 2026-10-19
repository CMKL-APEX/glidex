//! The cluster family's nodes, imports and departures (spec/clustering.md
//! §5, §7). A standalone host lists one node, `local`.

use super::{Call, ServiceError, ServiceResult};
use crate::authz::{Ent, EntitySet};
use crate::service::cluster::{Rule, node_rules, proto_of, rules};
use axum::http::StatusCode;
use glidex_api::proto::glidex::cluster::v1 as pb;
use serde_json::{Value, json};

type Caller = Call;
type ApiErr = ServiceError;

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> ServiceError {
    ServiceError::new(status, code, message)
}

/// A node record as the message.
pub(crate) fn node_pb(n: &crate::node::Node) -> ServiceResult<pb::Node> {
    let v = serde_json::to_value(n).map_err(|e| {
        ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
    })?;
    proto_of(v, &rules(&node_rules("")))
}

const PHASE: &[(&str, Rule)] = &[("phase", Rule::Enum("NODE_PHASE"))];

fn store_err(e: crate::node::NodeError) -> ApiErr {
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

/// The id of the node `key` names (an id or a name, as membership
/// operations take it); `key` itself when there is none, so the caller
/// is authorized before learning that.
fn node_key(call: &Call, key: String) -> ServiceResult<String> {
    let nodes = call.manager().nodes().list().map_err(store_err)?;
    Ok(crate::cluster::membership::resolve(&nodes, &key)
        .map(|n| n.meta.id)
        .unwrap_or(key))
}

pub fn list(call: &Call, _req: pb::ListNodesRequest) -> ServiceResult<pb::ListNodesResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let nodes = call.manager().nodes().list().map_err(store_err)?;
    Ok(pb::ListNodesResponse {
        nodes: nodes.iter().map(node_pb).collect::<ServiceResult<_>>()?,
        ..Default::default()
    })
}

/// `GET /nodes/{id}/topology`: node `id`'s NUMA and PCIe topology, read
/// on that node (spec/host-topology.md §3.2).
pub async fn get_node_topology(
    call: &Call,
    req: pb::GetNodeTopologyRequest,
) -> ServiceResult<pb::GetNodeTopologyResponse> {
    let id = node_key(call, req.id)?;
    let mut es = EntitySet::new();
    es.host(&id);
    call.require(Ent::host_of(&id), es)?;
    let t = call
        .manager()
        .node_topology(&id)
        .await
        .map_err(crate::service::host::manager_err)?;
    Ok(pb::GetNodeTopologyResponse {
        topology: crate::service::host::topology_pb(&t).into(),
        ..Default::default()
    })
}

pub fn get_node(call: &Call, req: pb::GetNodeRequest) -> ServiceResult<pb::GetNodeResponse> {
    let id = node_key(call, req.id)?;
    let mut es = EntitySet::new();
    es.host(&id);
    call.require(Ent::host_of(&id), es)?;
    match call.manager().nodes().get(&id).map_err(store_err)? {
        Some(n) => Ok(pb::GetNodeResponse {
            node: node_pb(&n)?.into(),
            ..Default::default()
        }),
        None => Err(err(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("node {} not found", id),
        )),
    }
}

/// What is still on a node: the things that keep it from being removed (§5.5).
fn contents(c: &Caller, id: &str) -> Value {
    let m = c.manager();
    let vms: Vec<String> = crate::cluster::sync::node_vms(&m.database(), id)
        .into_iter()
        .map(|(v, _)| v)
        .collect();
    let disks: Vec<String> = m
        .images()
        .list_disks()
        .into_iter()
        .filter(|d| d.node.as_deref() == Some(id))
        .map(|d| d.id)
        .collect();
    let networks: Vec<String> = m
        .networks_list()
        .into_iter()
        .filter(|n| n.node.as_deref() == Some(id))
        .map(|n| n.name)
        .collect();
    serde_json::json!({ "vms": vms, "disks": disks, "networks": networks })
}

async fn set_drain(c: &Caller, key: String, drain: bool) -> Result<Value, ApiErr> {
    let id = &node_key(c, key)?;
    let mut es = EntitySet::new();
    es.host(id);
    c.require(Ent::host_of(id), es)?;
    let store = c.manager().nodes();
    let mut n = store.get(id).map_err(store_err)?.ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("node {id} not found"),
        )
    })?;
    use crate::node::NodePhase;
    match (drain, n.status.phase) {
        (true, NodePhase::Active) => n.status.phase = NodePhase::Draining,
        (false, NodePhase::Draining) => n.status.phase = NodePhase::Active,
        (true, NodePhase::Draining) | (false, NodePhase::Active) => {}
        (_, p) => {
            return Err(err(
                StatusCode::CONFLICT,
                "conflict",
                format!("the node is {p:?}"),
            ));
        }
    }
    n.spec.unschedulable = drain;
    n.meta.generation += 1;
    n.meta.resource_version += 1;
    c.set_target(format!("node:{id}"));
    store.put(&n).map_err(store_err)?;
    Ok(
        serde_json::json!({ "node": n.spec.name, "phase": n.status.phase, "remaining": contents(c, id) }),
    )
}

pub async fn drain(call: &Call, req: pb::DrainNodeRequest) -> ServiceResult<pb::DrainNodeResponse> {
    proto_of(set_drain(call, req.id, true).await?, PHASE)
}

pub async fn undrain(
    call: &Call,
    req: pb::UndrainNodeRequest,
) -> ServiceResult<pb::UndrainNodeResponse> {
    proto_of(set_drain(call, req.id, false).await?, PHASE)
}

fn member_err(e: crate::cluster::membership::MemberError) -> ApiErr {
    use crate::cluster::membership::MemberError as M;
    match e {
        M::NotFound(m) => err(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("node {m} not found"),
        ),
        M::Conflict(m) => err(StatusCode::CONFLICT, "conflict", m),
        M::Invalid(m) => err(StatusCode::BAD_REQUEST, "invalid", m),
        M::NotLeader => err(
            StatusCode::MISDIRECTED_REQUEST,
            "not_leader",
            "this is not the leader; the request was not forwarded",
        ),
        M::Failed(m) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal", m),
    }
}

async fn member_op<F, Fut>(c: &Caller, id: &str, f: F) -> Result<Value, ApiErr>
where
    F: FnOnce(std::sync::Arc<crate::state::VmManager>) -> Fut,
    Fut: std::future::Future<Output = Result<Value, crate::cluster::membership::MemberError>>,
{
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("node:{id}"));
    // Membership operations are always audited (§12.4).
    c.audit_always();
    f(c.app.manager.clone()).await.map_err(member_err)
}

pub async fn remove(
    call: &Call,
    req: pb::RemoveNodeRequest,
) -> ServiceResult<pb::RemoveNodeResponse> {
    let (id, force) = (req.id, req.force);
    let v = member_op(call, &id.clone(), |m| async move {
        m.remove_node(&id, force).await
    })
    .await?;
    proto_of(v, PHASE)
}

pub async fn forget(
    call: &Call,
    req: pb::ForgetNodeRequest,
) -> ServiceResult<pb::ForgetNodeResponse> {
    let id = req.id;
    let v = member_op(call, &id.clone(), |m| async move {
        m.forget_node(&id, req.fenced, req.force).await
    })
    .await?;
    proto_of(v, PHASE)
}

pub async fn purge(call: &Call, req: pb::PurgeNodeRequest) -> ServiceResult<pb::PurgeNodeResponse> {
    let id = req.id;
    let v = member_op(
        call,
        &id.clone(),
        |m| async move { m.purge_node(&id).await },
    )
    .await?;
    proto_of(v, PHASE)
}

pub async fn rejoin_token(
    call: &Call,
    req: pb::CreateRejoinTokenRequest,
) -> ServiceResult<pb::CreateRejoinTokenResponse> {
    let by = call.actor();
    let ttl = req.ttl_secs.unwrap_or(3600).clamp(60, 7 * 86400);
    let raft_intact = req.raft_intact.unwrap_or(true);
    let id = req.id;
    let v = member_op(call, &id.clone(), |m| async move {
        m.rejoin_token(&id, raft_intact, ttl, &by).await
    })
    .await?;
    proto_of(v, &[])
}

fn departure_err(e: crate::cluster::departure::DepartureError) -> ApiErr {
    use crate::cluster::departure::DepartureError as D;
    match e {
        D::NotFound(m) => err(StatusCode::NOT_FOUND, "not_found", format!("{m} not found")),
        D::Conflict(m) => err(StatusCode::CONFLICT, "conflict", m),
        D::Invalid(m) => err(StatusCode::BAD_REQUEST, "invalid", m),
        D::Failed(m) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal", m),
    }
}

fn detach_start(c: &Caller, id: &str) -> Result<std::sync::Arc<crate::state::VmManager>, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("node:{id}"));
    c.audit_always();
    Ok(c.app.manager.clone())
}

/// `forget --departed`: record that a node left by itself (§5.8.3).
pub async fn forget_departed(
    call: &Call,
    req: pb::ForgetDepartedRequest,
) -> ServiceResult<pb::ForgetDepartedResponse> {
    let m = detach_start(call, &req.id)?;
    let v = m.forget_departed(&req.id).await.map_err(departure_err)?;
    proto_of(v, PHASE)
}

/// Return a frozen node to `Active`.
pub async fn abort_detach(
    call: &Call,
    req: pb::AbortDetachRequest,
) -> ServiceResult<pb::AbortDetachResponse> {
    let id = req.id;
    let m = detach_start(call, &id)?;
    {
        let node = m
            .nodes()
            .list()
            .map_err(store_err)?
            .into_iter()
            .find(|n| n.meta.id == id || (n.spec.name == id && !n.status.phase.is_tombstone()))
            .ok_or_else(|| {
                err(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    format!("node {id} not found"),
                )
            })?;
        let plan = crate::cluster::departure::plans(&m.database())
            .into_iter()
            .find(|p| p.node == node.meta.id && p.state == "frozen")
            .ok_or_else(|| {
                err(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "no detach is under way for that node",
                )
            })?;
        crate::cluster::departure::abort_plan(&m.database(), &plan.plan).map_err(departure_err)?;
        Ok(pb::AbortDetachResponse {
            node: node.spec.name,
            aborted: plan.plan,
            ..Default::default()
        })
    }
}

/// `POST /nodes/{id}/detach`: freeze the node and start the handover (§5.8.2).
pub async fn detach(
    call: &Call,
    req: pb::DetachNodeRequest,
) -> ServiceResult<pb::DetachNodeResponse> {
    let m = detach_start(call, &req.id)?;
    let options = crate::cluster::departure::DetachOptions {
        map_networks: req.map_networks.into_iter().collect(),
        with_access: req.with_access,
    };
    let v = m
        .start_detach(&req.id, options, req.timeout_secs)
        .await
        .map_err(departure_err)?;
    proto_of(v, &[("deadline", Rule::Secs)])
}

fn import_err(e: crate::cluster::import::ImportError) -> ApiErr {
    use crate::cluster::import::ImportError as I;
    match e {
        I::NotFound(m) => err(StatusCode::NOT_FOUND, "not_found", format!("{m} not found")),
        I::Conflict(m) => err(StatusCode::CONFLICT, "conflict", m),
        I::Invalid(m) => err(StatusCode::BAD_REQUEST, "invalid", m),
        I::Failed(m) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal", m),
    }
}

fn import_cluster(c: &Caller) -> Result<std::sync::Arc<crate::cluster::Cluster>, ApiErr> {
    c.manager().cluster().ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "not_clustered",
            "this host is not part of a cluster",
        )
    })
}

const PLAN: &[(&str, Rule)] = &[
    ("state", Rule::Enum("IMPORT_STATE")),
    ("created_at", Rule::Secs),
    ("expires_at", Rule::Secs),
];

/// A plan as the message.
pub(crate) fn plan_pb(v: Value) -> ServiceResult<pb::ImportPlan> {
    proto_of(v, PLAN)
}

/// What an administrator sees of a plan: no CSR, no poll hash.
pub(crate) fn plan_view(p: &crate::cluster::import::ImportPlan) -> Value {
    json!({ "plan": p.plan, "state": p.state, "node": p.name, "node_id": p.node_id, "advertise": p.advertise.to_string(), "summary": p.summary, "problems": p.problems, "mappings": p.mappings, "created_by": p.created_by, "created_at": p.created_at, "expires_at": p.expires_at })
}

pub fn list_imports(
    call: &Call,
    _req: pb::ListImportsRequest,
) -> ServiceResult<pb::ListImportsResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let cl = import_cluster(call)?;
    Ok(pb::ListImportsResponse {
        plans: crate::cluster::import::plans(&cl.db)
            .iter()
            .map(|p| plan_pb(plan_view(p)))
            .collect::<ServiceResult<_>>()?,
        ..Default::default()
    })
}

pub fn get_import(call: &Call, req: pb::GetImportRequest) -> ServiceResult<pb::GetImportResponse> {
    let c = call;
    let plan = req.plan;
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = import_cluster(c)?;
    let p = crate::cluster::import::read_plan(&cl.db, &plan).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("plan {plan} not found"),
        )
    })?;
    let mut v = plan_view(&p);
    // One plan's view also says what it brings, for the mapping editor.
    v["sources"] = crate::cluster::import::sources(&cl.db, &p);
    Ok(pb::GetImportResponse {
        plan: plan_pb(v)?.into(),
        ..Default::default()
    })
}

/// `GET /departures` (spec/clustering-ui.md §6): detaches, in progress or
/// done, with their step.
pub fn list_departures(
    call: &Call,
    _req: pb::ListDeparturesRequest,
) -> ServiceResult<pb::ListDeparturesResponse> {
    let c = call;
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = import_cluster(c)?;
    let names: std::collections::BTreeMap<String, String> = c
        .manager()
        .nodes()
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|n| (n.meta.id, n.spec.name))
        .collect();
    let out: Vec<Value> = crate::cluster::departure::plans(&cl.db)
        .into_iter()
        .map(|p| {
            json!({
                "plan": p.plan, "node": p.node, "node_name": names.get(&p.node), "state": p.state,
                "started_at": p.started_at, "deadline": p.deadline,
                "map_networks": p.options.map_networks, "with_access": p.options.with_access,
                "acknowledged": p.receipt.is_some(), "network_names": p.network_names,
            })
        })
        .collect();
    proto_of(
        json!({ "departures": out }),
        &[
            ("departures.state", Rule::Enum("DEPARTURE_STATE")),
            ("departures.started_at", Rule::Secs),
            ("departures.deadline", Rule::Secs),
        ],
    )
}

struct ApproveBody {
    mappings: crate::cluster::import::ImportMappings,
    dry_run: bool,
}

pub async fn approve_import(
    call: &Call,
    req: pb::ApproveImportRequest,
) -> ServiceResult<pb::ApproveImportResponse> {
    let c = call;
    let plan = req.plan;
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("import:{plan}"));
    c.audit_always();
    let cl = import_cluster(c)?;
    let b = ApproveBody {
        mappings: crate::cluster::import::ImportMappings {
            projects: req.projects.into_iter().collect(),
            networks: req.networks.into_iter().collect(),
            credentials: req.credentials.into_iter().collect(),
            over_quota: req.over_quota,
        },
        dry_run: req.dry_run,
    };
    // The approver must be able to add these resources by hand (§12.2): write
    // rights on every project it merges into, project creation for new ones,
    // and exceeding quotas when asked to.
    if !b.dry_run {
        let (merged, creates) = cl
            .plan_destinations(&plan, &b.mappings)
            .map_err(import_err)?;
        for p in &merged {
            for action in ["createVm", "createDisk", "createCredential"] {
                c.require_action(
                    action,
                    Ent::Project(p.clone()),
                    crate::api::project_entities(p),
                    &[],
                )?;
            }
            if b.mappings.over_quota {
                c.require_action(
                    "exceedQuota",
                    Ent::Project(p.clone()),
                    crate::api::project_entities(p),
                    &[],
                )?;
            }
        }
        if creates {
            // Imported projects that are new land in the default tenant.
            c.require_action(
                "createProject",
                Ent::Tenant(crate::tenants::DEFAULT_TENANT.into()),
                EntitySet::new(),
                &[],
            )?;
        }
    }
    let r = tokio::task::spawn_blocking(move || {
        if b.dry_run {
            cl.check_import(&plan, b.mappings)
        } else {
            cl.approve_import(&plan, b.mappings)
        }
    })
    .await;
    let p = match r {
        Ok(r) => r.map_err(import_err)?,
        Err(e) => {
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                e.to_string(),
            ));
        }
    };
    Ok(pb::ApproveImportResponse {
        plan: plan_pb(plan_view(&p))?.into(),
        ..Default::default()
    })
}

pub async fn reject_import(
    call: &Call,
    req: pb::RejectImportRequest,
) -> ServiceResult<pb::RejectImportResponse> {
    let c = call;
    let plan = req.plan;
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("import:{plan}"));
    c.audit_always();
    let cl = import_cluster(c)?;
    tokio::task::spawn_blocking(move || cl.reject_import(&plan))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?
        .map_err(import_err)?;
    Ok(Default::default())
}

/// Every node (id or name) that carries an OVN gateway: the shared edge's
/// and each VPC router's (or the edge's, for a router without its own).
pub(crate) fn gateway_node_keys(c: &Caller) -> Vec<(String, String)> {
    let m = c.manager();
    let edge: Vec<String> = m
        .ovn
        .load()
        .edge
        .as_ref()
        .map(|e| e.gateway_nodes.clone())
        .unwrap_or_default();
    let mut out: Vec<(String, String)> = edge
        .iter()
        .map(|g| (g.clone(), "the shared edge (gx-edge)".to_string()))
        .collect();
    for r in crate::router::list(&m.database()).unwrap_or_default() {
        if !r.spec.external {
            continue;
        }
        for g in r.spec.gateway_nodes.clone().unwrap_or_else(|| edge.clone()) {
            out.push((g, format!("router {}", r.spec.name)));
        }
    }
    out
}

struct NodeOvnBody {
    br_int_datapath: Option<crate::node::BrIntDatapath>,
    /// physnet → bridge: replaces the node's own mappings (the file's stay).
    bridge_mappings: Option<std::collections::BTreeMap<String, String>>,
}

/// A physical network or bridge name: what OVS's `ovn-bridge-mappings` and a
/// Linux interface name accept.
fn valid_mapping(physnet: &str, bridge: &str) -> Result<(), String> {
    let ok = |s: &str, max: usize| {
        !s.is_empty()
            && s.len() <= max
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    if !ok(physnet, 64) {
        return Err(format!(
            "physical network '{physnet}': letters, digits, - _ . (at most 64)"
        ));
    }
    if !ok(bridge, 15) {
        return Err(format!(
            "bridge '{bridge}': letters, digits, - _ . (at most 15, an interface name)"
        ));
    }
    Ok(())
}

/// `PUT /nodes/{id}/ovn` (`configureNode` on the node's host): the node's
/// `br-int` datapath (spec/clustering.md §11.6). The node applies it within
/// its next OVN round; netd refuses to flip a `br-int` with VM ports on it.
pub async fn put_node_ovn(
    call: &Call,
    req: pb::ConfigureNodeOvnRequest,
) -> ServiceResult<pb::ConfigureNodeOvnResponse> {
    use crate::node::BrIntDatapath as Dp;
    let c = call;
    let id = node_key(c, req.id)?;
    let b = NodeOvnBody {
        br_int_datapath: match req.br_int_datapath.as_known().unwrap_or_default() {
            pb::BrIntDatapath::BR_INT_DATAPATH_AUTO => Some(Dp::Auto),
            pb::BrIntDatapath::BR_INT_DATAPATH_SYSTEM => Some(Dp::System),
            pb::BrIntDatapath::BR_INT_DATAPATH_NETDEV => Some(Dp::Netdev),
            pb::BrIntDatapath::BR_INT_DATAPATH_UNSPECIFIED => None,
        },
        bridge_mappings: req
            .set_bridge_mappings
            .then(|| req.bridge_mappings.into_iter().collect()),
    };
    let mut es = EntitySet::new();
    es.host(&id);
    c.require(Ent::host_of(&id), es)?;
    c.set_target(format!("node:{id}"));
    c.audit_always();
    let m = c.manager();
    let store = m.nodes();
    let mut n = store.get(&id).map_err(store_err)?.ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("node {id} not found"),
        )
    })?;
    use crate::node::BrIntDatapath as D;
    if let Some(maps) = &b.bridge_mappings {
        for (p, br) in maps {
            valid_mapping(p, br).map_err(|m| err(StatusCode::BAD_REQUEST, "invalid", m))?;
        }
        // A gateway still needs the edge's (or its router's) physical network.
        if let Some(edge) = m.ovn.load().edge.clone() {
            let still_mapped = maps.contains_key(&edge.physnet)
                || (n.status.features.physnets.contains_key(&edge.physnet)
                    && !n.spec.bridge_mappings.contains_key(&edge.physnet));
            if !still_mapped
                && let Some((_, what)) = gateway_node_keys(c)
                    .into_iter()
                    .find(|(g, _)| *g == n.meta.id || *g == n.spec.name)
            {
                return Err(err(
                    StatusCode::CONFLICT,
                    "gateway_node",
                    format!(
                        "{} is a gateway node of {what}, which needs physical network {}",
                        n.spec.name, edge.physnet
                    ),
                ));
            }
        }
    }
    let datapath = b.br_int_datapath.unwrap_or(n.spec.br_int_datapath);
    let mut want = n.clone();
    want.spec.br_int_datapath = datapath;
    let wanted = want.wanted_br_int();
    if b.br_int_datapath == Some(D::Netdev) && !n.status.features.dpdk {
        return Err(err(
            StatusCode::CONFLICT,
            "dpdk_not_running",
            format!(
                "{}: OVS doesn't run DPDK there; the userspace datapath needs it (install with --ovs-profile dpdk)",
                n.spec.name
            ),
        ));
    }
    if wanted == "netdev" {
        // A gateway's SNAT is metered from kernel conntrack (§13.2).
        if let Some((_, what)) = gateway_node_keys(c)
            .into_iter()
            .find(|(g, _)| *g == n.meta.id || *g == n.spec.name)
        {
            return Err(err(
                StatusCode::CONFLICT,
                "gateway_node",
                format!(
                    "{} is a gateway node of {what}; gateways need br-int on the kernel (system) datapath",
                    n.spec.name
                ),
            ));
        }
    }
    if wanted == "system" {
        // vhost-user NICs on cluster networks need the userspace datapath.
        let nets = m.networks_list();
        let blocking: Vec<String> = m
            .list_vms()
            .await
            .into_iter()
            .filter(|v| {
                v.status
                    .placement
                    .as_ref()
                    .is_some_and(|p| p.node == n.meta.id)
            })
            .filter(|v| {
                v.config().networks.iter().any(|a| {
                    nets.iter().any(|x| {
                        x.id == a.network
                            && x.scope == crate::network::NetworkScope::Cluster
                            && x.port_type == glidex_ovs::vm_port::VmPortKind::VhostUser
                    })
                })
            })
            .map(|v| v.name.clone())
            .collect();
        if !blocking.is_empty() {
            return Err(err(
                StatusCode::CONFLICT,
                "vhost_user_vms",
                format!(
                    "{} runs VMs with vhost-user NICs on cluster networks, which need the userspace datapath: {}",
                    n.spec.name,
                    blocking.join(", ")
                ),
            ));
        }
    }
    let before = n.clone();
    n.spec.br_int_datapath = datapath;
    if let Some(maps) = b.bridge_mappings {
        // What the node reaches: its file's mappings (what it reported
        // beyond its old setting) and the new setting over them, at once,
        // so gateway checks see it before the node reports again.
        let old = std::mem::replace(&mut n.spec.bridge_mappings, maps);
        let mut physnets: std::collections::BTreeMap<String, String> = n
            .status
            .features
            .physnets
            .clone()
            .into_iter()
            .filter(|(p, br)| old.get(p) != Some(br))
            .collect();
        physnets.extend(n.spec.bridge_mappings.clone());
        n.status.features.physnets = physnets;
    }
    if n != before {
        n.meta.generation += 1;
        n.meta.resource_version += 1;
        store.put(&n).map_err(store_err)?;
    }
    proto_of(
        json!({
            "node": n.spec.name, "br_int_datapath": n.spec.br_int_datapath, "wanted": n.wanted_br_int(), "current": n.status.features.br_int_datapath,
            "bridge_mappings": n.spec.bridge_mappings, "physnets": n.status.features.physnets,
        }),
        &[("br_int_datapath", Rule::Enum("BR_INT_DATAPATH"))],
    )
}

/// The live node a gateway key (id or name) means. A key that names only a
/// node that left (a tombstone keeps its name) is refused, saying so.
pub(crate) fn live_gateway_node<'a>(
    nodes: &'a [crate::node::Node],
    k: &str,
) -> Result<&'a crate::node::Node, String> {
    let matching = || nodes.iter().filter(|n| n.meta.id == k || n.spec.name == k);
    if let Some(n) = matching().find(|n| !n.status.phase.is_tombstone()) {
        return Ok(n);
    }
    match matching().next() {
        Some(n) => Err(format!(
            "{} ({}) is {:?}: a gateway node must be a node in the cluster",
            n.spec.name,
            &n.meta.id[..n.meta.id.len().min(8)],
            n.status.phase
        )),
        None => Err(format!("no node {k}")),
    }
}

/// Whether `keys` (node ids or names) can carry an OVN gateway on `physnet`:
/// each maps that physical network and wants `br-int` on the kernel datapath
/// (spec/clustering.md §11.2, §13.2). The reason when not.
pub(crate) fn check_gateway_nodes(
    nodes: &[crate::node::Node],
    keys: &[String],
    physnet: &str,
) -> Result<(), String> {
    for k in keys {
        let n = live_gateway_node(nodes, k)?;
        if !n.status.features.physnets.contains_key(physnet) {
            return Err(format!(
                "{} doesn't map physical network {physnet}: add it to cluster.ovn.bridge_mappings in that node's control-plane.json (a bridge with the uplink) and restart its control plane",
                n.spec.name
            ));
        }
        if n.wanted_br_int() != "system" {
            return Err(format!(
                "{} runs br-int on the userspace datapath; a gateway needs the kernel (system) datapath, which its external traffic is metered from: set its br-int datapath to system first",
                n.spec.name
            ));
        }
    }
    Ok(())
}

/// `POST /nodes/{id}/spare-image-files/delete` (`pullImage` on the
/// cluster): the node deletes the unrecorded image files it has reported
/// (spec/clustering.md §9.2.3), at its next report.
pub fn delete_spare_image_files(
    call: &Call,
    req: pb::DeleteSpareImageFilesRequest,
) -> ServiceResult<pb::DeleteSpareImageFilesResponse> {
    let c = call;
    let id = node_key(c, req.id)?;
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("node:{id}"));
    c.audit_always();
    let store = c.manager().nodes();
    let mut n = store.get(&id).map_err(store_err)?.ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("node {id} not found"),
        )
    })?;
    let files = n.status.features.spare_image_files.clone();
    n.spec.drop_spare_files_before = Some(crate::tenancy::now() + 1);
    n.meta.generation += 1;
    n.meta.resource_version += 1;
    store.put(&n).map_err(store_err)?;
    proto_of(
        json!({ "node": n.spec.name, "files": files.len(), "bytes": files.iter().map(|f| f.bytes).sum::<u64>() }),
        &[],
    )
}
