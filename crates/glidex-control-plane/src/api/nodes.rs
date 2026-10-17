//! `GET /nodes` and `GET /nodes/{id}` (spec/clustering.md §7). A standalone
//! host lists one node, `local`.

use super::{err, ApiErr, Caller};
use crate::authz::{Ent, EntitySet};
use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};

fn store_err(e: crate::node::NodeError) -> ApiErr {
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

pub async fn list(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let nodes = c.manager().nodes().list().map_err(store_err)?;
    Ok(Json(nodes))
}

pub async fn get_node(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.host(&id);
    c.require(Ent::host_of(&id), es)?;
    match c.manager().nodes().get(&id).map_err(store_err)? {
        Some(n) => Ok(Json(n)),
        None => Err(err(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("node {} not found", id),
        )),
    }
}

/// What is still on a node: the things that keep it from being removed (§5.5).
fn contents(c: &Caller, id: &str) -> serde_json::Value {
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

async fn set_drain(c: &Caller, id: &str, drain: bool) -> Result<serde_json::Value, ApiErr> {
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
            ))
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

pub async fn drain(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    Ok(Json(set_drain(&c, &id, true).await?))
}

pub async fn undrain(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    Ok(Json(set_drain(&c, &id, false).await?))
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

#[derive(serde::Deserialize, Default)]
pub struct MemberBody {
    #[serde(default)]
    force: bool,
    #[serde(default)]
    fenced: bool,
    #[serde(default = "yes")]
    raft_intact: bool,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

fn yes() -> bool {
    true
}

fn body(b: Option<Json<MemberBody>>) -> MemberBody {
    b.map(|b| b.0).unwrap_or_default()
}

async fn member_op<F, Fut>(
    c: &Caller,
    id: &str,
    action: &'static str,
    f: F,
) -> Result<Json<serde_json::Value>, ApiErr>
where
    F: FnOnce(std::sync::Arc<crate::state::VmManager>) -> Fut,
    Fut: std::future::Future<
        Output = Result<serde_json::Value, crate::cluster::membership::MemberError>,
    >,
{
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("node:{id}"));
    // Membership operations are always audited (§12.4).
    c.audit_always();
    let _ = action;
    f(c.app.manager.clone()).await.map(Json).map_err(member_err)
}

pub async fn remove(
    c: Caller,
    Path(id): Path<String>,
    b: Option<Json<MemberBody>>,
) -> Result<impl IntoResponse, ApiErr> {
    let b = body(b);
    member_op(&c, &id.clone(), "removeNode", |m| async move {
        m.remove_node(&id, b.force).await
    })
    .await
}

pub async fn forget(
    c: Caller,
    Path(id): Path<String>,
    b: Option<Json<MemberBody>>,
) -> Result<impl IntoResponse, ApiErr> {
    let b = body(b);
    member_op(&c, &id.clone(), "forgetNode", |m| async move {
        m.forget_node(&id, b.fenced, b.force).await
    })
    .await
}

pub async fn purge(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    member_op(&c, &id.clone(), "purgeNode", |m| async move {
        m.purge_node(&id).await
    })
    .await
}

pub async fn rejoin_token(
    c: Caller,
    Path(id): Path<String>,
    b: Option<Json<MemberBody>>,
) -> Result<impl IntoResponse, ApiErr> {
    let b = body(b);
    let by = c.actor();
    let ttl = b.ttl_secs.unwrap_or(3600).clamp(60, 7 * 86400);
    member_op(&c, &id.clone(), "createRejoinToken", |m| async move {
        m.rejoin_token(&id, b.raft_intact, ttl, &by).await
    })
    .await
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

#[derive(serde::Deserialize, Default)]
pub struct DetachBody {
    #[serde(flatten)]
    options: crate::cluster::departure::DetachOptions,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    abort: bool,
    /// `forget --departed`: the node left on its own.
    #[serde(default)]
    departed: bool,
}

/// `POST /nodes/{id}/detach`: freeze the node and start the handover (§5.8.2);
/// with `abort`, return it to `Active`; with `departed`, record that it left
/// by itself (§5.8.3).
pub async fn detach(
    c: Caller,
    Path(id): Path<String>,
    b: Option<Json<DetachBody>>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("node:{id}"));
    c.audit_always();
    let b = b.map(|b| b.0).unwrap_or_default();
    let m = c.app.manager.clone();
    if b.departed {
        return m
            .forget_departed(&id)
            .await
            .map(Json)
            .map_err(departure_err);
    }
    if b.abort {
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
        return Ok(Json(
            serde_json::json!({ "node": node.spec.name, "aborted": plan.plan }),
        ));
    }
    m.start_detach(&id, b.options, b.timeout_secs)
        .await
        .map(Json)
        .map_err(departure_err)
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

/// What an administrator sees of a plan: no CSR, no poll hash.
pub(crate) fn plan_view(p: &crate::cluster::import::ImportPlan) -> serde_json::Value {
    serde_json::json!({ "plan": p.plan, "state": p.state, "node": p.name, "node_id": p.node_id, "advertise": p.advertise.to_string(), "summary": p.summary, "problems": p.problems, "mappings": p.mappings, "created_by": p.created_by, "created_at": p.created_at, "expires_at": p.expires_at })
}

pub async fn list_imports(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = import_cluster(&c)?;
    Ok(Json(
        crate::cluster::import::plans(&cl.db)
            .iter()
            .map(plan_view)
            .collect::<Vec<_>>(),
    ))
}

pub async fn get_import(c: Caller, Path(plan): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = import_cluster(&c)?;
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
    Ok(Json(v))
}

/// `GET /departures` (spec/clustering-ui.md §6): detaches, in progress or
/// done, with their step.
pub async fn list_departures(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = import_cluster(&c)?;
    let names: std::collections::BTreeMap<String, String> = c
        .manager()
        .nodes()
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|n| (n.meta.id, n.spec.name))
        .collect();
    let out: Vec<serde_json::Value> = crate::cluster::departure::plans(&cl.db)
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "plan": p.plan, "node": p.node, "node_name": names.get(&p.node), "state": p.state,
                "started_at": p.started_at, "deadline": p.deadline,
                "map_networks": p.options.map_networks, "with_access": p.options.with_access,
                "acknowledged": p.receipt.is_some(),
            })
        })
        .collect();
    Ok(Json(out))
}

#[derive(serde::Deserialize, Default)]
pub struct ApproveBody {
    #[serde(flatten)]
    mappings: crate::cluster::import::ImportMappings,
    /// Check the mappings without committing.
    #[serde(default)]
    dry_run: bool,
}

pub async fn approve_import(
    c: Caller,
    Path(plan): Path<String>,
    b: Option<Json<ApproveBody>>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("import:{plan}"));
    c.audit_always();
    let cl = import_cluster(&c)?;
    let b = b.map(|b| b.0).unwrap_or_default();
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
                    super::project_entities(p),
                    &[],
                )?;
            }
            if b.mappings.over_quota {
                c.require_action(
                    "exceedQuota",
                    Ent::Project(p.clone()),
                    super::project_entities(p),
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
    match r {
        Ok(r) => r.map(|p| Json(plan_view(&p))).map_err(import_err),
        Err(e) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            e.to_string(),
        )),
    }
}

pub async fn reject_import(
    c: Caller,
    Path(plan): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("import:{plan}"));
    c.audit_always();
    let cl = import_cluster(&c)?;
    tokio::task::spawn_blocking(move || cl.reject_import(&plan))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?
        .map_err(import_err)?;
    Ok(StatusCode::NO_CONTENT)
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

#[derive(serde::Deserialize)]
pub struct NodeOvnBody {
    #[serde(default)]
    br_int_datapath: Option<crate::node::BrIntDatapath>,
    /// physnet → bridge: replaces the node's own mappings (the file's stay).
    #[serde(default)]
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
    c: Caller,
    Path(id): Path<String>,
    Json(b): Json<NodeOvnBody>,
) -> Result<impl IntoResponse, ApiErr> {
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
            if !still_mapped {
                if let Some((_, what)) = gateway_node_keys(&c)
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
    }
    let datapath = b.br_int_datapath.unwrap_or(n.spec.br_int_datapath);
    let mut want = n.clone();
    want.spec.br_int_datapath = datapath;
    let wanted = want.wanted_br_int();
    if b.br_int_datapath == Some(D::Netdev) && !n.status.features.dpdk {
        return Err(err(StatusCode::CONFLICT, "dpdk_not_running", format!("{}: OVS doesn't run DPDK there; the userspace datapath needs it (install with --ovs-profile dpdk)", n.spec.name)));
    }
    if wanted == "netdev" {
        // A gateway's SNAT is metered from kernel conntrack (§13.2).
        if let Some((_, what)) = gateway_node_keys(&c)
            .into_iter()
            .find(|(g, _)| *g == n.meta.id || *g == n.spec.name)
        {
            return Err(err(StatusCode::CONFLICT, "gateway_node", format!("{} is a gateway node of {what}; gateways need br-int on the kernel (system) datapath", n.spec.name)));
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
                        x.name == a.network
                            && x.scope == crate::network::NetworkScope::Cluster
                            && x.port_type == glidex_ovs::vm_port::VmPortKind::VhostUser
                    })
                })
            })
            .map(|v| v.name.clone())
            .collect();
        if !blocking.is_empty() {
            return Err(err(StatusCode::CONFLICT, "vhost_user_vms", format!("{} runs VMs with vhost-user NICs on cluster networks, which need the userspace datapath: {}", n.spec.name, blocking.join(", "))));
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
    Ok(Json(serde_json::json!({
        "node": n.spec.name, "br_int_datapath": n.spec.br_int_datapath, "wanted": n.wanted_br_int(), "current": n.status.features.br_int_datapath,
        "bridge_mappings": n.spec.bridge_mappings, "physnets": n.status.features.physnets,
    })))
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
pub async fn delete_spare_image_files(
    c: Caller,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
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
    Ok((
        StatusCode::ACCEPTED,
        Json(
            serde_json::json!({ "node": n.spec.name, "files": files.len(), "bytes": files.iter().map(|f| f.bytes).sum::<u64>() }),
        ),
    ))
}
