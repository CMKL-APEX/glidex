//! The handshake's HTTP side (spec/tenancy.md D63–D66), public and
//! authenticated by peer assertions instead of a user:
//!
//! - at a cluster: `POST /trust/v1/redeem` (a directory redeems an offer)
//!   and `POST /trust/v1/end` (a trusted directory ends a tenant's trust);
//! - at a directory: `POST /d/{dir}/trust/v1/end` (a trusted cluster ends
//!   it).
//!
//! Every refusal answers the same way; the reason goes to the audit log.

use super::{AppState, err};
use crate::directory::issuer::CLUSTER_OWNER;
use crate::directory::peer;
use crate::trust::{self, RemoteTenant, TrustedDirectory};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

fn now() -> u64 {
    crate::auth::store::now()
}

/// The one answer to a refused peer request; `why` goes to the audit log.
fn refused(app: &AppState, action: &str, why: &str) -> Response {
    app.auth.audit_system(
        "peer",
        "",
        action,
        None,
        "denied",
        Some("trust_refused"),
        json!({ "why": why }),
    );
    err(
        StatusCode::FORBIDDEN,
        "trust_refused",
        "the request was refused",
    )
    .into_response()
}

#[derive(Deserialize)]
pub struct Redeem {
    offer: String,
    secret: String,
    issuer: String,
    directory: String,
    tenant: RemoteTenant,
    jwks: Value,
    api: String,
    tls_sha256: String,
}

/// `POST /trust/v1/redeem`, at the cluster (D65).
pub async fn redeem(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<Redeem>,
) -> Response {
    const ACTION: &str = "redeemOffer";
    let t = now();
    let Some(assertion) = peer::from_headers(&headers) else {
        return refused(&app, ACTION, "no assertion");
    };
    let offers = app.auth.tenants.offers().unwrap_or_default();
    let Some(mut offer) = offers.into_iter().find(|o| o.id == b.offer) else {
        return refused(&app, ACTION, "no such offer");
    };
    if !offer.is_open(t) {
        return refused(&app, ACTION, "the offer was redeemed, or expired");
    }
    if !crate::auth::constant_eq(
        &crate::auth::sha256_hex(b.secret.as_bytes()),
        &offer.secret_hash,
    ) {
        return refused(&app, ACTION, "the wrong secret");
    }
    let Some(issuer) = trust::normalize_issuer(&b.issuer) else {
        return refused(&app, ACTION, "not an issuer URL");
    };
    if issuer != offer.directory {
        return refused(&app, ACTION, "another directory than the offer's");
    }
    if b.tenant.slug != offer.tenant || b.tenant.id.is_empty() {
        return refused(&app, ACTION, "another tenant than the offer's");
    }
    let Some(pin) = glidex_tls::Trust::normalize_pin(&b.tls_sha256) else {
        return refused(&app, ACTION, "no certificate fingerprint");
    };
    let cluster = super::op::cluster_id(&app);
    if let Err(e) = peer::verify(
        &assertion,
        &b.jwks,
        &issuer,
        &cluster,
        trust::REDEEM,
        &app.auth.peer_replays,
        t,
    ) {
        return refused(&app, ACTION, &e.to_string());
    }
    // Once: the offer is spent before anything else is written.
    offer.redeemed_at = Some(t);
    if let Err(e) = app.auth.tenants.put_offer(&offer) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()).into_response();
    }
    let mut d = app
        .auth
        .trust
        .directory_by_issuer(&issuer)
        .ok()
        .flatten()
        .unwrap_or_else(|| TrustedDirectory {
            id: trust::trusted_id(&issuer),
            issuer: issuer.clone(),
            directory: b.directory.clone(),
            api: String::new(),
            tls_sha256: String::new(),
            jwks: json!({ "keys": [] }),
            tenants: Vec::new(),
            created_at: t,
        });
    d.directory = b.directory.clone();
    d.api = b.api.clone();
    d.tls_sha256 = pin;
    d.jwks = b.jwks.clone();
    d.tenants.retain(|x| x.id != b.tenant.id);
    d.tenants.push(b.tenant.clone());
    let by = format!("offer:{}", offer.id);
    let done = app
        .auth
        .trust
        .put_directory(&d)
        .map_err(|e| e.to_string())
        .and_then(|_| {
            app.auth
                .tenants
                .admit_remote(&b.tenant.id, &d.id, offer.cap.clone(), &by)
                .map_err(|e| e.to_string())
        })
        .and_then(|_| {
            app.auth
                .connectors
                .maintain_signing_keys(CLUSTER_OWNER, t)
                .map_err(|e| e.to_string())
        })
        .and_then(|_| {
            app.auth
                .connectors
                .jwks(CLUSTER_OWNER, t)
                .map_err(|e| e.to_string())
        });
    let cluster_key = match done {
        Ok(k) => k,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e).into_response(),
    };
    app.auth.audit_system(
        &format!("directory:{issuer}"),
        "",
        ACTION,
        Some(&format!("tenant:{}", b.tenant.id)),
        "allowed",
        None,
        json!({ "offer": offer.id, "tenant": b.tenant.slug }),
    );
    Json(json!({ "cluster_id": cluster, "cluster_key": cluster_key })).into_response()
}

#[derive(Deserialize)]
pub struct EndAtCluster {
    issuer: String,
    tenant: String,
}

/// `POST /trust/v1/end`, at the cluster: a trusted directory ends a
/// tenant's trust (D66).
pub async fn end_at_cluster(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(b): Json<EndAtCluster>,
) -> Response {
    const ACTION: &str = "endTrust";
    let Some(assertion) = peer::from_headers(&headers) else {
        return refused(&app, ACTION, "no assertion");
    };
    let Some(issuer) = trust::normalize_issuer(&b.issuer) else {
        return refused(&app, ACTION, "not an issuer URL");
    };
    let Ok(Some(mut d)) = app.auth.trust.directory_by_issuer(&issuer) else {
        return refused(&app, ACTION, "not a trusted directory");
    };
    let cluster = super::op::cluster_id(&app);
    if let Err(e) = peer::verify(
        &assertion,
        &d.jwks,
        &issuer,
        &cluster,
        trust::END,
        &app.auth.peer_replays,
        now(),
    ) {
        return refused(&app, ACTION, &e.to_string());
    }
    forget_tenant_here(&app, &mut d, &b.tenant);
    StatusCode::OK.into_response()
}

/// This cluster stops admitting `tenant` of trusted directory `d`; a
/// directory left with none is forgotten (D66).
pub fn forget_tenant_here(app: &AppState, d: &mut TrustedDirectory, tenant: &str) {
    let _ = app.auth.tenants.remove_admission(tenant);
    d.tenants.retain(|x| x.id != tenant);
    let _ = if d.tenants.is_empty() {
        app.auth.trust.remove_directory(&d.id)
    } else {
        app.auth.trust.put_directory(d)
    };
}

#[derive(Deserialize)]
pub struct EndAtDirectory {
    cluster_id: String,
    tenant: String,
}

/// `POST /d/{dir}/trust/v1/end`, at the directory: a trusted cluster
/// stops admitting one of its tenants (D66).
pub async fn end_at_directory(
    State(app): State<AppState>,
    Path(dir): Path<String>,
    headers: HeaderMap,
    Json(b): Json<EndAtDirectory>,
) -> Response {
    const ACTION: &str = "endTrust";
    let Some(assertion) = peer::from_headers(&headers) else {
        return refused(&app, ACTION, "no assertion");
    };
    let Ok(Some(mut c)) = app.auth.trust.cluster(&dir, &b.cluster_id) else {
        return refused(&app, ACTION, "not a trusted cluster");
    };
    let Some(iss) = app
        .auth
        .tenants
        .get_directory(&dir)
        .ok()
        .flatten()
        .and_then(|d| d.issuer)
    else {
        return refused(&app, ACTION, "no issuer");
    };
    if let Err(e) = peer::verify(
        &assertion,
        &c.jwks,
        &c.cluster_id,
        &iss,
        trust::END,
        &app.auth.peer_replays,
        now(),
    ) {
        return refused(&app, ACTION, &e.to_string());
    }
    c.tenants.retain(|t| *t != b.tenant);
    let _ = if c.tenants.is_empty() {
        app.auth.trust.remove_cluster(&dir, &c.cluster_id)
    } else {
        app.auth.trust.put_cluster(&c)
    };
    StatusCode::OK.into_response()
}

/// Tell trusted directory `d` that this cluster no longer admits `tenant`
/// (D66); best effort: it also finds out when it is next refused.
pub async fn tell_directory_ended(app: &AppState, d: &TrustedDirectory, tenant: &str) {
    let cluster = super::op::cluster_id(app);
    let Ok(a) = peer::assertion(
        &app.auth.connectors,
        CLUSTER_OWNER,
        &cluster,
        &d.issuer,
        trust::END,
        now(),
    ) else {
        return;
    };
    let path = format!("/d/{}/trust/v1/end", d.directory);
    if let Err(e) = peer::post(
        &d.api,
        &d.tls_sha256,
        &path,
        &a,
        &json!({ "cluster_id": cluster, "tenant": tenant }),
    )
    .await
    {
        tracing::warn!(directory = %d.issuer, "telling the directory trust ended: {}", e);
    }
}
