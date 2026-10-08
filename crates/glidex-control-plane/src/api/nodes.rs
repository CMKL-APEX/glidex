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
