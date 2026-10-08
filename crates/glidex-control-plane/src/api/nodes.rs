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
