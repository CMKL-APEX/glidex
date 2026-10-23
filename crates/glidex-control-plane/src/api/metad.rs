//! The control plane's side of the metadata endpoint
//! (spec/workload-identity.md W11, §7): `metad.sock`, which only
//! glidex-metad's Unix user may use. metad says which peer address a guest
//! request came from (`glidex_ovs::metadata::peer_address` of a VM port,
//! W3); this finds the running VM on this node behind it and answers for
//! it. metad holds no key and can't name a VM of its own choosing.

use super::issuer::{self, ResolvedVm};
use super::{App, AppState, PeerUid, err};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Mutex;

/// What metad asks: the peer address a guest's request came from, and for
/// an identity token its audience and lifetime.
#[derive(Debug, Deserialize)]
pub struct Ask {
    pub peer: String,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub ttl: Option<u64>,
}

pub fn router(app: AppState) -> Router {
    Router::new()
        .route("/v1/instance", post(instance))
        .route("/v1/identity", post(identity))
        .route("/v1/glidex/token", post(glidex_token))
        .layer(middleware::from_fn_with_state(app.clone(), only_metad))
        .with_state(app)
}

/// Only glidex-metad's user (`metad_user`), by its peer uid.
async fn only_metad(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let ok = req
        .extensions()
        .get::<PeerUid>()
        .is_some_and(|PeerUid(uid)| {
            nix::unistd::User::from_name(&app.auth.config.metad_user)
                .ok()
                .flatten()
                .is_some_and(|u| u.uid.as_raw() == *uid)
        });
    if !ok {
        return err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only glidex-metad may use this socket",
        )
        .into_response();
    }
    next.run(req).await
}

/// The VM on this node whose NIC has the port behind `peer`: its id. Who
/// it is (project, tenant) is resolved where the answer is made, since an
/// agent's cache holds only its own VMs.
async fn vm_id_of(app: &App, peer: &str) -> Result<String, super::ApiErr> {
    let unknown = || {
        err(
            StatusCode::NOT_FOUND,
            "unknown_port",
            "no running VM on this node has that port",
        )
    };
    let ip: Ipv4Addr = peer.parse().map_err(|_| {
        err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "peer is an IPv4 address",
        )
    })?;
    let index = glidex_ovs::metadata::index_of(ip).ok_or_else(unknown)?;
    let vm = app
        .manager
        .list_vms()
        .await
        .into_iter()
        .find(|v| {
            app.manager.is_local(v)
                && v.status
                    .nics
                    .iter()
                    .any(|n| n.metadata_index == Some(index))
        })
        .ok_or_else(unknown)?;
    Ok(vm.id)
}

/// Answers already given: the same one until 80% of its lifetime (W8,
/// §4.4), so a guest asking often signs and writes nothing. Keyed by the
/// VM and its service account, never by the peer address: a port's index
/// passes to another VM once that port is gone.
struct Given {
    answer: Value,
    issued_at: u64,
    lifetime: u64,
}

fn given() -> &'static Mutex<HashMap<String, Given>> {
    static G: std::sync::OnceLock<Mutex<HashMap<String, Given>>> = std::sync::OnceLock::new();
    G.get_or_init(Default::default)
}

/// `key`'s answer while fresh, with `expires_in` what remains of it.
fn reuse(key: &str, now: u64) -> Option<Value> {
    let mut g = given().lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|_, v| now < v.issued_at + v.lifetime * 4 / 5);
    let v = g.get(key)?;
    let mut a = v.answer.clone();
    a["expires_in"] = (v.issued_at + v.lifetime - now).into();
    Some(a)
}

fn remember(key: String, answer: &Value, lifetime: u64, now: u64) {
    given().lock().unwrap_or_else(|e| e.into_inner()).insert(
        key,
        Given {
            answer: answer.clone(),
            issued_at: now,
            lifetime,
        },
    );
}

/// What a guest asks, about a VM resolved on its node: answered there when
/// writes go there, else by the leader (W2, §15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Instance,
    Identity,
    GlidexToken,
}

/// A question the leader answers for a node (`/cluster/v1/metadata`).
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct Question {
    pub vm_id: String,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<u64>,
}

/// The answer about `vm`, signed and recorded here (writes go here).
fn answer_here(app: &App, vm: &ResolvedVm, q: &Question) -> Result<Value, super::ApiErr> {
    let id = &vm.identity;
    if q.kind == Kind::Instance {
        return Ok(json!({
            "vm_id": id.vm_id, "name": id.vm_name, "project": id.project,
            "tenant": id.tenant, "cluster": id.cluster,
        }));
    }
    let now = crate::auth::store::now();
    let audience = q.audience.as_deref().unwrap_or("");
    let key = match q.kind {
        Kind::Identity => format!(
            "identity|{}|{:?}|{audience}|{:?}",
            id.vm_id, id.service_account, q.ttl
        ),
        _ => format!("glidex|{}|{:?}", id.vm_id, id.service_account),
    };
    if let Some(a) = reuse(&key, now) {
        return Ok(a);
    }
    let (answer, lifetime) = match q.kind {
        Kind::Identity => {
            let token = issuer::sign_identity(app, &vm.directory, id, audience, q.ttl)?;
            let ttl = crate::directory::workload::ttl(q.ttl)
                .unwrap_or(crate::directory::workload::DEFAULT_TTL_SECS);
            (json!({ "token": token, "expires_in": ttl }), ttl)
        }
        _ => {
            let (token, ttl) = issuer::vm_glidex_token(app, vm, None)?;
            (
                json!({
                    "access_token": token,
                    "token_type": "Bearer",
                    "expires_in": ttl,
                    "endpoint": crate::service::tenancy::api_endpoint(&app.auth.config, &app.manager),
                }),
                ttl,
            )
        }
    };
    remember(key, &answer, lifetime, now);
    Ok(answer)
}

/// Ask the leader, as this node (its certificate on :8842).
async fn ask_leader(app: &App, q: &Question) -> Result<Value, super::ApiErr> {
    let unavailable = |m: String| err(StatusCode::SERVICE_UNAVAILABLE, "leader_unavailable", m);
    let body = bytes::Bytes::from(serde_json::to_vec(q).map_err(|e| unavailable(e.to_string()))?);
    let reply = match (app.manager.node_link(), app.manager.cluster()) {
        (Some(link), _) => link.post_leader(LEADER_PATH, body).await,
        (None, Some(c)) => c.post_leader(LEADER_PATH, body).await,
        (None, None) => return Err(unavailable("not in a cluster".into())),
    }
    .map_err(unavailable)?;
    let v: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
    if reply.status.is_success() {
        return Ok(v);
    }
    Err(err(
        reply.status,
        v["error"].as_str().unwrap_or("leader_refused"),
        v["message"]
            .as_str()
            .unwrap_or("the leader refused")
            .to_string(),
    ))
}

/// The leader's path for metadata questions from nodes.
pub const LEADER_PATH: &str = "/cluster/v1/metadata";

/// The leader's answer for node `node` (from its certificate): only about a
/// running VM placed on that node (spec/clustering.md §12.3).
pub async fn leader_answer(app: &App, node: &str, q: &Question) -> Result<Value, super::ApiErr> {
    let refused = || {
        err(
            StatusCode::FORBIDDEN,
            "not_placed_here",
            "that VM isn't placed on the asking node",
        )
    };
    let vm = app.manager.get_vm(&q.vm_id).await.map_err(|_| refused())?;
    if vm.status.placement.as_ref().map(|p| p.node.as_str()) != Some(node) {
        return Err(refused());
    }
    let resolved = issuer::vm_identity(app, &q.vm_id).await?;
    answer_here(app, &resolved, q)
}

/// Answer `kind` for the guest behind `peer`.
async fn answer(app: &App, ask: &Ask, kind: Kind) -> Result<Value, super::ApiErr> {
    let q = Question {
        vm_id: vm_id_of(app, &ask.peer).await?,
        kind,
        audience: ask.audience.clone(),
        ttl: ask.ttl,
    };
    // Tokens are signed and recorded where writes go (tenancy.md D53); a
    // node that can't write asks the leader, as itself.
    if app.auth.tenants.can_write() {
        let vm = issuer::vm_identity(app, &q.vm_id).await?;
        answer_here(app, &vm, &q)
    } else {
        ask_leader(app, &q).await
    }
}

async fn reply(app: AppState, ask: Ask, kind: Kind) -> Response {
    match answer(&app, &ask, kind).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}

/// `/v1/instance`: what the VM is.
async fn instance(State(app): State<AppState>, Json(ask): Json<Ask>) -> Response {
    reply(app, ask, Kind::Instance).await
}

/// `/v1/identity`: an identity token for the audience asked (§4.2).
async fn identity(State(app): State<AppState>, Json(ask): Json<Ask>) -> Response {
    reply(app, ask, Kind::Identity).await
}

/// `/v1/glidex/token`: a glidex token for the VM's service account (W6).
async fn glidex_token(State(app): State<AppState>, Json(ask): Json<Ask>) -> Response {
    reply(app, ask, Kind::GlidexToken).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_reused_until_80_percent_of_their_life() {
        let a = json!({ "token": "t1", "expires_in": 900 });
        remember("test|vm-a".into(), &a, 900, 1_000);
        assert_eq!(reuse("test|vm-a", 1_100).unwrap()["expires_in"], 800);
        assert_eq!(reuse("test|vm-a", 1_719).unwrap()["token"], "t1");
        assert!(reuse("test|vm-a", 1_720).is_none(), "past 80%");
        assert!(reuse("test|vm-b", 1_100).is_none(), "another VM's own");
    }
}
