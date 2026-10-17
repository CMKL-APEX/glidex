//! `glidex.directory.v1.ConnectorService`: the directory's connectors
//! (spec/tenancy.md §5, T1). REST (`api/connectors.rs`) and RPC
//! (`api/rpc/directory.rs`) both call these functions and nothing else.

use super::{Call, ServiceError, ServiceResult};
use crate::api::{auth_error, directory_ent};
use crate::auth::connector_err;
use crate::authz::EntitySet;
use crate::directory::{self as dir, Connector, Kind, Source};
use axum::http::StatusCode;
use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::directory::v1 as pb;
use pb::__buffa::oneof::connector::Settings;

fn ts(secs: u64) -> Timestamp {
    Timestamp::from_unix_secs(secs as i64)
}

fn err_of(e: dir::ConnectorError) -> ServiceError {
    match e {
        dir::ConnectorError::NotFound(m) => ServiceError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("connector not found: {m}"),
        ),
        dir::ConnectorError::AlreadyExists(m) => ServiceError::new(
            StatusCode::CONFLICT,
            "conflict",
            format!("connector already exists: {m}"),
        ),
        dir::ConnectorError::Invalid(m) => {
            ServiceError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_connector", m)
        }
        dir::ConnectorError::FileDefined(_) => {
            ServiceError::new(StatusCode::CONFLICT, "file_defined", e.to_string())
        }
        other => auth_error(connector_err(other)).into(),
    }
}

pub fn to_pb(call: &Call, c: &Connector) -> ServiceResult<pb::Connector> {
    let secrets = call.auth().connectors.secret_status(c).map_err(err_of)?;
    let settings = match c.kind {
        Kind::Pam => {
            let s = c.pam().map_err(err_of)?;
            Settings::Pam(Box::new(pb::PamSettings {
                allowed_groups: s.allowed_groups,
                ..Default::default()
            }))
        }
        Kind::Oidc => {
            let s = c.oidc().map_err(err_of)?;
            Settings::Oidc(Box::new(pb::OidcSettings {
                issuer: s.issuer,
                client_id: s.client_id,
                scopes: s.scopes,
                groups_claim: s.groups_claim,
                allowed_domains: s.allowed_domains,
                redirect_uri: s.redirect_uri,
                mfa_acr_values: s.mfa_acr_values,
                ..Default::default()
            }))
        }
    };
    Ok(pb::Connector {
        id: c.id.clone(),
        directory_id: c.directory_id.clone(),
        kind: c.kind.as_str().into(),
        display_name: c.display_name.clone(),
        enabled: Some(c.enabled),
        jit: Some(c.jit),
        required_groups: c.required_groups.clone(),
        require_mfa: Some(c.require_mfa),
        group_teams: c
            .group_teams
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        default_tenant: c.default_tenant.clone(),
        token_max_days: c.token_max_days.map(|d| d as u32),
        settings: Some(settings),
        secrets: secrets
            .into_iter()
            .map(|(field, at)| pb::SecretStatus {
                field,
                set: Some(at.is_some()),
                updated_at: at.map(ts).into(),
                ..Default::default()
            })
            .collect(),
        source: match c.source {
            Source::Api => "api".into(),
            Source::File => "file".into(),
        },
        created_at: ts(c.created_at).into(),
        updated_at: ts(c.updated_at).into(),
        ..Default::default()
    })
}

fn settings_json(s: &Settings) -> (Kind, serde_json::Value) {
    match s {
        Settings::Pam(p) => (
            Kind::Pam,
            serde_json::json!({ "allowed_groups": p.allowed_groups }),
        ),
        Settings::Oidc(o) => {
            let d = |v: &Vec<String>, def: &[&str]| {
                if v.is_empty() {
                    def.iter().map(|s| s.to_string()).collect()
                } else {
                    v.clone()
                }
            };
            let mut v = serde_json::json!({
                "issuer": o.issuer,
                "client_id": o.client_id,
                "scopes": d(&o.scopes, &["openid", "profile", "email", "groups"]),
                "groups_claim": if o.groups_claim.is_empty() { "groups".to_string() } else { o.groups_claim.clone() },
                "allowed_domains": o.allowed_domains,
                "mfa_acr_values": o.mfa_acr_values,
            });
            if let Some(r) = &o.redirect_uri {
                v["redirect_uri"] = serde_json::json!(r);
            }
            (Kind::Oidc, v)
        }
    }
}

/// Whether new users of `c` join the directory's owner or an operator
/// tenant (spec/tenancy.md D22). The default tenant may be named by id or
/// slug; it is stored by id.
fn guarded(call: &Call, c: &mut Connector) -> ServiceResult<bool> {
    let Some(t) = &c.default_tenant else {
        return Ok(false);
    };
    let tenant = call.auth().tenants.resolve(t).map_err(|_| {
        ServiceError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_connector",
            format!("no tenant {t}"),
        )
    })?;
    c.default_tenant = Some(tenant.id.clone());
    Ok(tenant.owns_directory || tenant.operator)
}

fn get_visible(call: &Call, id: &str) -> ServiceResult<Connector> {
    call.set_target(format!("connector:{id}"));
    call.auth()
        .connectors
        .get(id)
        .map_err(err_of)?
        .ok_or_else(|| {
            ServiceError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("connector not found: {id}"),
            )
        })
}

pub fn list_connectors(
    call: &Call,
    _req: pb::ListConnectorsRequest,
) -> ServiceResult<pb::ListConnectorsResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let all = call.auth().connectors.list().map_err(err_of)?;
    Ok(pb::ListConnectorsResponse {
        connectors: all
            .iter()
            .map(|c| to_pb(call, c))
            .collect::<ServiceResult<_>>()?,
        ..Default::default()
    })
}

pub fn get_connector(
    call: &Call,
    req: pb::GetConnectorRequest,
) -> ServiceResult<pb::GetConnectorResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let c = get_visible(call, &req.id)?;
    Ok(pb::GetConnectorResponse {
        connector: to_pb(call, &c)?.into(),
        ..Default::default()
    })
}

pub fn create_connector(
    call: &Call,
    req: pb::CreateConnectorRequest,
) -> ServiceResult<pb::CreateConnectorResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let p = req.connector.as_option().ok_or_else(|| {
        ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "connector is required")
    })?;
    let settings = p.settings.as_ref().ok_or_else(|| {
        ServiceError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_connector",
            "set pam or oidc settings",
        )
    })?;
    let (kind, settings) = settings_json(settings);
    let now = crate::tenancy::now();
    let mut c = Connector {
        id: p.id.clone(),
        directory_id: crate::tenants::LOCAL_DIRECTORY.into(),
        kind,
        display_name: p.display_name.clone(),
        enabled: p.enabled.unwrap_or(true),
        jit: p.jit.unwrap_or(false),
        required_groups: p.required_groups.clone(),
        require_mfa: p.require_mfa.unwrap_or(false),
        group_teams: p
            .group_teams
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        default_tenant: p.default_tenant.clone(),
        token_max_days: p.token_max_days.map(u64::from),
        settings,
        source: Source::Api,
        created_at: now,
        updated_at: now,
    };
    let g = guarded(call, &mut c)?;
    // D22: MFA on by default where new users join the owner or an operator.
    if g && p.require_mfa.is_none() && kind == Kind::Oidc {
        c.require_mfa = true;
    }
    c.validate(g).map_err(err_of)?;
    call.set_target(format!("connector:{}", c.id));
    call.detail("kind", serde_json::json!(kind.as_str()));
    call.auth().connectors.create(&c).map_err(err_of)?;
    Ok(pb::CreateConnectorResponse {
        connector: to_pb(call, &c)?.into(),
        ..Default::default()
    })
}

pub fn update_connector(
    call: &Call,
    req: pb::UpdateConnectorRequest,
) -> ServiceResult<pb::UpdateConnectorResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let mut c = get_visible(call, &req.id)?;
    let p = req.connector.as_option().cloned().unwrap_or_default();
    let paths: Vec<String> = req
        .update_mask
        .as_option()
        .map(|m| m.paths.clone())
        .unwrap_or_default();
    if paths.is_empty() {
        return Err(ServiceError::new(
            StatusCode::BAD_REQUEST,
            "invalid",
            "update_mask names the fields to change",
        ));
    }
    for path in &paths {
        match path.as_str() {
            "display_name" => c.display_name = p.display_name.clone(),
            "enabled" => c.enabled = p.enabled.unwrap_or(true),
            "jit" => c.jit = p.jit.unwrap_or(false),
            "required_groups" => c.required_groups = p.required_groups.clone(),
            "require_mfa" => c.require_mfa = p.require_mfa.unwrap_or(false),
            "group_teams" => {
                c.group_teams = p
                    .group_teams
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            }
            "default_tenant" => c.default_tenant = p.default_tenant.clone(),
            "token_max_days" => c.token_max_days = p.token_max_days.map(u64::from),
            "pam" | "oidc" => {
                let s = p.settings.as_ref().ok_or_else(|| {
                    ServiceError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "invalid_connector",
                        format!("{path} settings are missing"),
                    )
                })?;
                let (kind, settings) = settings_json(s);
                if kind != c.kind {
                    return Err(ServiceError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "invalid_connector",
                        "a connector's kind can't change",
                    ));
                }
                c.settings = settings;
            }
            other => {
                return Err(ServiceError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid",
                    format!("{other} can't be updated"),
                ));
            }
        }
    }
    c.updated_at = crate::tenancy::now();
    let g = guarded(call, &mut c)?;
    c.validate(g).map_err(err_of)?;
    call.detail("fields", serde_json::json!(paths));
    call.auth().connectors.update(&c).map_err(err_of)?;
    Ok(pb::UpdateConnectorResponse {
        connector: to_pb(call, &c)?.into(),
        ..Default::default()
    })
}

pub fn delete_connector(
    call: &Call,
    req: pb::DeleteConnectorRequest,
) -> ServiceResult<pb::DeleteConnectorResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let c = get_visible(call, &req.id)?;
    call.auth().connectors.delete(&c.id).map_err(err_of)?;
    Ok(pb::DeleteConnectorResponse::default())
}

pub fn set_secret(call: &Call, req: pb::SetSecretRequest) -> ServiceResult<pb::SetSecretResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let c = get_visible(call, &req.id)?;
    // The field name is audited, never the value (spec/tenancy.md §13).
    call.detail("field", serde_json::json!(req.field));
    call.auth()
        .connectors
        .set_secret(&c, &req.field, &req.value)
        .map_err(err_of)?;
    Ok(pb::SetSecretResponse {
        connector: to_pb(call, &c)?.into(),
        ..Default::default()
    })
}

fn step(name: &str, ok: bool, message: impl Into<String>) -> pb::TestStep {
    pb::TestStep {
        name: name.into(),
        ok: Some(ok),
        message: message.into(),
        ..Default::default()
    }
}

pub async fn test_connector(
    call: &Call,
    req: pb::TestConnectorRequest,
) -> ServiceResult<pb::TestConnectorResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let c = get_visible(call, &req.id)?;
    let mut steps = Vec::new();
    match c.kind {
        Kind::Pam => {
            let sock = &call.auth().config.auth.pam.authd_socket;
            let there = std::fs::metadata(sock).is_ok();
            steps.push(step(
                "glidex-authd",
                there,
                if there {
                    format!("{} is there", sock.display())
                } else {
                    format!("{} is missing: is glidex-authd installed?", sock.display())
                },
            ));
        }
        Kind::Oidc => {
            if c.source == Source::Api {
                let set = call
                    .auth()
                    .connectors
                    .secret(&c, "client_secret")
                    .map_err(err_of)?
                    .is_some();
                steps.push(step(
                    "client_secret",
                    set,
                    if set {
                        "set"
                    } else {
                        "not set: PUT /connectors/{id}/secrets/client_secret"
                    },
                ));
            }
            match call.app.auth.oidc_for(Some(&c.id)) {
                Ok((_, oidc)) => match oidc.discovery().await {
                    Ok(d) => {
                        steps.push(step("discovery", true, format!("issuer {}", d.issuer)));
                        steps.push(step(
                            "device grant",
                            d.device_authorization_endpoint.is_some(),
                            if d.device_authorization_endpoint.is_some() {
                                "offered (gxctl logins)"
                            } else {
                                "not offered: gxctl needs a token or PAM"
                            },
                        ));
                    }
                    Err(e) => steps.push(step("discovery", false, e.to_string())),
                },
                Err(e) => steps.push(step("connector", false, e.to_string())),
            }
        }
    }
    Ok(pb::TestConnectorResponse {
        steps,
        ..Default::default()
    })
}
