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
        None => Err(err(StatusCode::NOT_FOUND, "not_found", format!("node {} not found", id))),
    }
}

/// What is still on a node: the things that keep it from being removed (§5.5).
fn contents(c: &Caller, id: &str) -> serde_json::Value {
    let m = c.manager();
    let vms: Vec<String> = crate::cluster::sync::node_vms(&m.database(), id).into_iter().map(|(v, _)| v).collect();
    let disks: Vec<String> = m.images().list_disks().into_iter().filter(|d| d.node.as_deref() == Some(id)).map(|d| d.id).collect();
    let networks: Vec<String> = m.networks_list().into_iter().filter(|n| n.node.as_deref() == Some(id)).map(|n| n.name).collect();
    serde_json::json!({ "vms": vms, "disks": disks, "networks": networks })
}

async fn set_drain(c: &Caller, id: &str, drain: bool) -> Result<serde_json::Value, ApiErr> {
    let mut es = EntitySet::new();
    es.host(id);
    c.require(Ent::host_of(id), es)?;
    let store = c.manager().nodes();
    let mut n = store.get(id).map_err(store_err)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", format!("node {id} not found")))?;
    use crate::node::NodePhase;
    match (drain, n.status.phase) {
        (true, NodePhase::Active) => n.status.phase = NodePhase::Draining,
        (false, NodePhase::Draining) => n.status.phase = NodePhase::Active,
        (true, NodePhase::Draining) | (false, NodePhase::Active) => {}
        (_, p) => return Err(err(StatusCode::CONFLICT, "conflict", format!("the node is {p:?}"))),
    }
    n.spec.unschedulable = drain;
    n.meta.generation += 1;
    n.meta.resource_version += 1;
    c.set_target(format!("node:{id}"));
    store.put(&n).map_err(store_err)?;
    Ok(serde_json::json!({ "node": n.spec.name, "phase": n.status.phase, "remaining": contents(c, id) }))
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
        M::NotFound(m) => err(StatusCode::NOT_FOUND, "not_found", format!("node {m} not found")),
        M::Conflict(m) => err(StatusCode::CONFLICT, "conflict", m),
        M::Invalid(m) => err(StatusCode::BAD_REQUEST, "invalid", m),
        M::NotLeader => err(StatusCode::MISDIRECTED_REQUEST, "not_leader", "this is not the leader; the request was not forwarded"),
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

async fn member_op<F, Fut>(c: &Caller, id: &str, action: &'static str, f: F) -> Result<Json<serde_json::Value>, ApiErr>
where
    F: FnOnce(std::sync::Arc<crate::state::VmManager>) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, crate::cluster::membership::MemberError>>,
{
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("node:{id}"));
    // Membership operations are always audited (§12.4).
    c.audit_always();
    let _ = action;
    f(c.app.manager.clone()).await.map(Json).map_err(member_err)
}

pub async fn remove(c: Caller, Path(id): Path<String>, b: Option<Json<MemberBody>>) -> Result<impl IntoResponse, ApiErr> {
    let b = body(b);
    member_op(&c, &id.clone(), "removeNode", |m| async move { m.remove_node(&id, b.force).await }).await
}

pub async fn forget(c: Caller, Path(id): Path<String>, b: Option<Json<MemberBody>>) -> Result<impl IntoResponse, ApiErr> {
    let b = body(b);
    member_op(&c, &id.clone(), "forgetNode", |m| async move { m.forget_node(&id, b.fenced, b.force).await }).await
}

pub async fn purge(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    member_op(&c, &id.clone(), "purgeNode", |m| async move { m.purge_node(&id).await }).await
}

pub async fn rejoin_token(c: Caller, Path(id): Path<String>, b: Option<Json<MemberBody>>) -> Result<impl IntoResponse, ApiErr> {
    let b = body(b);
    let by = c.actor();
    let ttl = b.ttl_secs.unwrap_or(3600).clamp(60, 7 * 86400);
    member_op(&c, &id.clone(), "createRejoinToken", |m| async move { m.rejoin_token(&id, b.raft_intact, ttl, &by).await }).await
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
pub async fn detach(c: Caller, Path(id): Path<String>, b: Option<Json<DetachBody>>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("node:{id}"));
    c.audit_always();
    let b = b.map(|b| b.0).unwrap_or_default();
    let m = c.app.manager.clone();
    if b.departed {
        return m.forget_departed(&id).await.map(Json).map_err(departure_err);
    }
    if b.abort {
        let node = m.nodes().list().map_err(store_err)?.into_iter().find(|n| n.meta.id == id || n.spec.name == id).ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", format!("node {id} not found")))?;
        let plan = crate::cluster::departure::plans(&m.database()).into_iter().find(|p| p.node == node.meta.id && p.state == "frozen").ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", "no detach is under way for that node"))?;
        crate::cluster::departure::abort_plan(&m.database(), &plan.plan).map_err(departure_err)?;
        return Ok(Json(serde_json::json!({ "node": node.spec.name, "aborted": plan.plan })));
    }
    m.start_detach(&id, b.options, b.timeout_secs).await.map(Json).map_err(departure_err)
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
    c.manager().cluster().ok_or_else(|| err(StatusCode::CONFLICT, "not_clustered", "this host is not part of a cluster"))
}

/// What an administrator sees of a plan: no CSR, no poll hash.
pub(crate) fn plan_view(p: &crate::cluster::import::ImportPlan) -> serde_json::Value {
    serde_json::json!({ "plan": p.plan, "state": p.state, "node": p.name, "node_id": p.node_id, "advertise": p.advertise.to_string(), "summary": p.summary, "problems": p.problems, "mappings": p.mappings, "created_by": p.created_by, "created_at": p.created_at, "expires_at": p.expires_at })
}

pub async fn list_imports(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = import_cluster(&c)?;
    Ok(Json(crate::cluster::import::plans(&cl.db).iter().map(plan_view).collect::<Vec<_>>()))
}

pub async fn get_import(c: Caller, Path(plan): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = import_cluster(&c)?;
    crate::cluster::import::read_plan(&cl.db, &plan).map(|p| Json(plan_view(&p))).ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", format!("plan {plan} not found")))
}

#[derive(serde::Deserialize, Default)]
pub struct ApproveBody {
    #[serde(flatten)]
    mappings: crate::cluster::import::ImportMappings,
    /// Check the mappings without committing.
    #[serde(default)]
    dry_run: bool,
}

pub async fn approve_import(c: Caller, Path(plan): Path<String>, b: Option<Json<ApproveBody>>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("import:{plan}"));
    c.audit_always();
    let cl = import_cluster(&c)?;
    let b = b.map(|b| b.0).unwrap_or_default();
    // The approver must be able to add these resources by hand (§12.2): write
    // rights on every project it merges into, project creation for new ones,
    // and exceeding quotas when asked to.
    if !b.dry_run {
        let (merged, creates) = cl.plan_destinations(&plan, &b.mappings).map_err(import_err)?;
        for p in &merged {
            for action in ["createVm", "createDisk", "createCredential"] {
                c.require_action(action, Ent::Project(p.clone()), super::project_entities(p), &[])?;
            }
            if b.mappings.over_quota {
                c.require_action("exceedQuota", Ent::Project(p.clone()), super::project_entities(p), &[])?;
            }
        }
        if creates {
            c.require_action("createProject", Ent::Cluster, EntitySet::new(), &[])?;
        }
    }
    let r = tokio::task::spawn_blocking(move || if b.dry_run { cl.check_import(&plan, b.mappings) } else { cl.approve_import(&plan, b.mappings) }).await;
    match r {
        Ok(r) => r.map(|p| Json(plan_view(&p))).map_err(import_err),
        Err(e) => Err(err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())),
    }
}

pub async fn reject_import(c: Caller, Path(plan): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target(format!("import:{plan}"));
    c.audit_always();
    let cl = import_cluster(&c)?;
    tokio::task::spawn_blocking(move || cl.reject_import(&plan)).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?.map_err(import_err)?;
    Ok(StatusCode::NO_CONTENT)
}
