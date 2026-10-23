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

/// The running VM on this node whose NIC has the port behind `peer`.
async fn vm_of(app: &App, peer: &str) -> Result<ResolvedVm, super::ApiErr> {
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
    issuer::vm_identity(app, &vm.id).await
}

/// Issuing writes (the first token fixes the directory's issuer; a glidex
/// token is a record): only where writes go.
fn writable(app: &App) -> Result<(), super::ApiErr> {
    if app.auth.tenants.can_write() {
        Ok(())
    } else {
        Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_leader",
            "this node can't issue tokens yet (spec/workload-identity.md §15)",
        ))
    }
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

/// `/v1/instance`: what the VM is.
async fn instance(State(app): State<AppState>, Json(ask): Json<Ask>) -> Response {
    match vm_of(&app, &ask.peer).await {
        Ok(vm) => Json(json!({
            "vm_id": vm.identity.vm_id,
            "name": vm.identity.vm_name,
            "project": vm.identity.project,
            "tenant": vm.identity.tenant,
            "cluster": vm.identity.cluster,
        }))
        .into_response(),
        Err(e) => e.into_response(),
    }
}

/// `/v1/identity`: an identity token for the audience asked (§4.2).
async fn identity(State(app): State<AppState>, Json(ask): Json<Ask>) -> Response {
    let run = async {
        let vm = vm_of(&app, &ask.peer).await?;
        let audience = ask.audience.as_deref().unwrap_or("");
        let key = format!(
            "identity|{}|{:?}|{audience}|{:?}",
            vm.identity.vm_id, vm.identity.service_account, ask.ttl
        );
        let now = crate::auth::store::now();
        if let Some(a) = reuse(&key, now) {
            return Ok(Json(a));
        }
        writable(&app)?;
        let token = issuer::sign_identity(&app, &vm.directory, &vm.identity, audience, ask.ttl)?;
        let expires_in = crate::directory::workload::ttl(ask.ttl)
            .unwrap_or(crate::directory::workload::DEFAULT_TTL_SECS);
        let answer = json!({ "token": token, "expires_in": expires_in });
        remember(key, &answer, expires_in, now);
        Ok::<_, super::ApiErr>(Json(answer))
    };
    match run.await {
        Ok(r) => r.into_response(),
        Err(e) => e.into_response(),
    }
}

/// `/v1/glidex/token`: a glidex token for the VM's service account (W6).
async fn glidex_token(State(app): State<AppState>, Json(ask): Json<Ask>) -> Response {
    let run = async {
        let vm = vm_of(&app, &ask.peer).await?;
        let key = format!(
            "glidex|{}|{:?}",
            vm.identity.vm_id, vm.identity.service_account
        );
        let now = crate::auth::store::now();
        if let Some(a) = reuse(&key, now) {
            return Ok(Json(a));
        }
        writable(&app)?;
        let (token, expires_in) = issuer::vm_glidex_token(&app, &vm, None)?;
        let answer = json!({
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": expires_in,
            "endpoint": crate::service::tenancy::api_endpoint(&app.auth.config, &app.manager),
        });
        remember(key, &answer, expires_in, now);
        Ok::<_, super::ApiErr>(Json(answer))
    };
    match run.await {
        Ok(r) => r.into_response(),
        Err(e) => e.into_response(),
    }
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
