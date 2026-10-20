//! Workload federations (spec/tenancy.md §9, D48–D51):
//! `glidex.directory.v1.FederationService`. The exchange that uses them is
//! `service::exchange`.

use super::{Call, ServiceError, ServiceResult};
use crate::auth::store::TokenKind;
use crate::authz::{Ent, EntitySet};
use crate::directory::federation::{Federation, FederationError, FederationKind, Rule};
use axum::http::StatusCode;
use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::directory::v1 as pb;

fn ts(secs: u64) -> Timestamp {
    Timestamp {
        seconds: secs as i64,
        ..Default::default()
    }
}

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> ServiceError {
    ServiceError::new(status, code, message)
}

fn store_err(e: FederationError) -> ServiceError {
    match e {
        FederationError::NotFound => err(StatusCode::NOT_FOUND, "not_found", "no such federation"),
        FederationError::Invalid(m) => {
            err(StatusCode::UNPROCESSABLE_ENTITY, "invalid_federation", m)
        }
        FederationError::Conflict(m) => err(StatusCode::CONFLICT, "conflict", m),
        FederationError::Storage(m) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal", m),
    }
}

pub fn federation_pb(f: &Federation) -> pb::Federation {
    pb::Federation {
        id: f.id.clone(),
        tenant_id: f.tenant_id.clone(),
        kind: "jwt".into(),
        display_name: f.display_name.clone(),
        issuer: f.issuer.clone(),
        audience: f.audience.clone(),
        enabled: Some(f.enabled),
        rules: f
            .rules
            .iter()
            .map(|r| pb::FederationRule {
                r#match: r
                    .claims
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                service_account: r.service_account.clone(),
                ttl_secs: r.ttl_secs,
                ..Default::default()
            })
            .collect(),
        created_by: f.created_by.clone(),
        created_at: ts(f.created_at).into(),
        updated_at: ts(f.updated_at).into(),
        ..Default::default()
    }
}

fn rules_of(p: &pb::Federation) -> Vec<Rule> {
    p.rules
        .iter()
        .map(|r| Rule {
            claims: r
                .r#match
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            service_account: r.service_account.clone(),
            ttl_secs: r.ttl_secs,
        })
        .collect()
}

/// D50: each rule's service account is one of the tenant's.
fn check_service_accounts(call: &Call, tenant: &str, f: &Federation) -> ServiceResult<()> {
    for (i, r) in f.rules.iter().enumerate() {
        let n = i + 1;
        let refused = || {
            err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_federation",
                format!(
                    "rule {n}: {} isn't a service account of this tenant",
                    r.service_account
                ),
            )
        };
        let Some((_, t)) = call
            .auth()
            .store
            .token_by_id(&r.service_account)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?
        else {
            return Err(refused());
        };
        let TokenKind::ServiceAccount { project } = &t.kind else {
            return Err(refused());
        };
        let owner = call
            .manager()
            .projects()
            .get(project)
            .ok()
            .flatten()
            .map(|p| p.tenant_id);
        if owner.as_deref() != Some(tenant) {
            return Err(refused());
        }
    }
    Ok(())
}

/// The issuer answers now (D49): its discovery and its keys.
async fn check_issuer(call: &Call, issuer: &str) -> ServiceResult<()> {
    call.auth()
        .issuers
        .fetch(issuer)
        .await
        .map(|_| ())
        .map_err(|e| {
            err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "issuer_unreachable",
                format!("the issuer can't be read: {e}"),
            )
        })
}

pub fn list_federations(
    call: &Call,
    req: pb::ListFederationsRequest,
) -> ServiceResult<pb::ListFederationsResponse> {
    let t = super::tenancy::visible(call, &req.tenant)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let list = call.auth().federations.list(&t.id).map_err(store_err)?;
    Ok(pb::ListFederationsResponse {
        federations: list.iter().map(federation_pb).collect(),
        ..Default::default()
    })
}

pub fn get_federation(
    call: &Call,
    req: pb::GetFederationRequest,
) -> ServiceResult<pb::GetFederationResponse> {
    let t = super::tenancy::visible(call, &req.tenant)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let f = call
        .auth()
        .federations
        .get(&t.id, &req.federation)
        .map_err(store_err)?
        .ok_or_else(|| store_err(FederationError::NotFound))?;
    Ok(pb::GetFederationResponse {
        federation: federation_pb(&f).into(),
        ..Default::default()
    })
}

pub async fn create_federation(
    call: &Call,
    req: pb::CreateFederationRequest,
) -> ServiceResult<pb::CreateFederationResponse> {
    let t = super::tenancy::visible(call, &req.tenant)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let p = req.federation.as_option().cloned().unwrap_or_default();
    if !p.kind.is_empty() && p.kind != "jwt" {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_federation",
            "kind is jwt",
        ));
    }
    let now = crate::tenancy::now();
    let f = Federation {
        id: p.id.trim().to_string(),
        tenant_id: t.id.clone(),
        kind: FederationKind::Jwt,
        display_name: p.display_name.trim().to_string(),
        issuer: p.issuer.trim().trim_end_matches('/').to_string(),
        audience: p.audience.trim().to_string(),
        enabled: p.enabled.unwrap_or(true),
        rules: rules_of(&p),
        created_by: call.p.user_id().unwrap_or("-").to_string(),
        created_at: now,
        updated_at: now,
    };
    f.validate().map_err(store_err)?;
    check_service_accounts(call, &t.id, &f)?;
    check_issuer(call, &f.issuer).await?;
    call.set_target(format!("federation:{}/{}", t.slug, f.id));
    call.detail("issuer", serde_json::json!(f.issuer));
    call.detail("rules", serde_json::json!(f.rules));
    call.auth().federations.create(&f).map_err(store_err)?;
    Ok(pb::CreateFederationResponse {
        federation: federation_pb(&f).into(),
        ..Default::default()
    })
}

pub async fn update_federation(
    call: &Call,
    req: pb::UpdateFederationRequest,
) -> ServiceResult<pb::UpdateFederationResponse> {
    let t = super::tenancy::visible(call, &req.tenant)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let mut f = call
        .auth()
        .federations
        .get(&t.id, &req.federation)
        .map_err(store_err)?
        .ok_or_else(|| store_err(FederationError::NotFound))?;
    let p = req.changes.as_option().cloned().unwrap_or_default();
    let paths: Vec<String> = req
        .update_mask
        .as_option()
        .map(|m| m.paths.clone())
        .unwrap_or_default();
    if paths.is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "update_mask names the fields to change",
        ));
    }
    let issuer_was = f.issuer.clone();
    for path in &paths {
        match path.as_str() {
            "display_name" => f.display_name = p.display_name.trim().to_string(),
            "issuer" => f.issuer = p.issuer.trim().trim_end_matches('/').to_string(),
            "audience" => f.audience = p.audience.trim().to_string(),
            "enabled" => f.enabled = p.enabled.unwrap_or(true),
            "rules" => f.rules = rules_of(&p),
            other => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "invalid",
                    format!("{other} can't be changed"),
                ));
            }
        }
    }
    f.updated_at = crate::tenancy::now();
    f.validate().map_err(store_err)?;
    check_service_accounts(call, &t.id, &f)?;
    if f.issuer != issuer_was {
        check_issuer(call, &f.issuer).await?;
    }
    call.set_target(format!("federation:{}/{}", t.slug, f.id));
    call.detail("fields", serde_json::json!(paths));
    call.auth().federations.update(&f).map_err(store_err)?;
    Ok(pb::UpdateFederationResponse {
        federation: federation_pb(&f).into(),
        ..Default::default()
    })
}

pub fn delete_federation(
    call: &Call,
    req: pb::DeleteFederationRequest,
) -> ServiceResult<pb::DeleteFederationResponse> {
    let t = super::tenancy::visible(call, &req.tenant)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    call.set_target(format!("federation:{}/{}", t.slug, req.federation));
    call.auth()
        .federations
        .delete(&t.id, &req.federation)
        .map_err(store_err)?;
    Ok(pb::DeleteFederationResponse::default())
}
