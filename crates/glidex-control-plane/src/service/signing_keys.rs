//! A directory's signing keys (spec/tenancy.md D52, D53):
//! `glidex.directory.v1.SigningKeyService`. The keys themselves are
//! `directory::issuer`; only their public facts come out here.

use super::{Call, ServiceError, ServiceResult};
use crate::authz::EntitySet;
use crate::directory::issuer::{RETAIN_SECS, SigningKey};
use axum::http::StatusCode;
use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::directory::v1 as pb;

fn ts(secs: u64) -> Timestamp {
    Timestamp {
        seconds: secs as i64,
        ..Default::default()
    }
}

fn internal(m: impl ToString) -> ServiceError {
    ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", m.to_string())
}

fn key_pb(k: &SigningKey) -> pb::SigningKey {
    pb::SigningKey {
        kid: k.kid.clone(),
        alg: k.alg.clone(),
        created_at: ts(k.created_at).into(),
        retired_at: k.retired_at.map(ts).into(),
        published_until: k.retired_at.map(|r| ts(r + RETAIN_SECS)).into(),
        active: k.retired_at.is_none(),
        ..Default::default()
    }
}

pub fn list_signing_keys(
    call: &Call,
    _req: pb::ListSigningKeysRequest,
) -> ServiceResult<pb::ListSigningKeysResponse> {
    call.require(call.directory_ent(), EntitySet::new())?;
    let now = crate::auth::store::now();
    let keys = call
        .auth()
        .connectors
        .signing_keys(call.p.directory_id())
        .map_err(internal)?;
    let d = call
        .auth()
        .tenants
        .get_directory(call.p.directory_id())
        .map_err(internal)?
        .ok_or_else(|| {
            ServiceError::new(StatusCode::NOT_FOUND, "not_found", "no such directory")
        })?;
    Ok(pb::ListSigningKeysResponse {
        keys: keys
            .iter()
            .filter(|k| k.published(now))
            .map(key_pb)
            .collect(),
        issuer: crate::api::issuer::issuer_url(call.auth(), call.manager(), &d).unwrap_or_default(),
        issuer_fixed: d.issuer.is_some(),
        ..Default::default()
    })
}

pub fn rotate_signing_key(
    call: &Call,
    _req: pb::RotateSigningKeyRequest,
) -> ServiceResult<pb::RotateSigningKeyResponse> {
    call.require(call.directory_ent(), EntitySet::new())?;
    let k = call
        .auth()
        .connectors
        .rotate_signing_key(call.p.directory_id(), crate::auth::store::now())
        .map_err(internal)?;
    Ok(pb::RotateSigningKeyResponse {
        key: key_pb(&k).into(),
        ..Default::default()
    })
}
