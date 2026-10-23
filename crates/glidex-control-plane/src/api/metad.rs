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
use serde_json::json;
use std::net::Ipv4Addr;

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
        writable(&app)?;
        let token = issuer::sign_identity(
            &app,
            &vm.directory,
            &vm.identity,
            ask.audience.as_deref().unwrap_or(""),
            ask.ttl,
        )?;
        Ok::<_, super::ApiErr>(Json(json!({ "token": token })))
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
        writable(&app)?;
        let (token, expires_in) = issuer::vm_glidex_token(&app, &vm, None)?;
        Ok::<_, super::ApiErr>(Json(json!({
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": expires_in,
            "endpoint": crate::service::tenancy::api_endpoint(&app.auth.config, &app.manager),
        })))
    };
    match run.await {
        Ok(r) => r.into_response(),
        Err(e) => e.into_response(),
    }
}
