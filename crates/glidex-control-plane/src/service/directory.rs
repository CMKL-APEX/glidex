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
        Kind::Google => {
            let s = c.google().map_err(err_of)?;
            Settings::Google(Box::new(pb::GoogleSettings {
                accounts: match s.accounts {
                    dir::presets::google::Accounts::Workspace => {
                        pb::GoogleAccounts::GOOGLE_ACCOUNTS_WORKSPACE
                    }
                    dir::presets::google::Accounts::Any => pb::GoogleAccounts::GOOGLE_ACCOUNTS_ANY,
                }
                .into(),
                client_id: s.client_id,
                domains: s.domains,
                allowed_emails: s.allowed_emails,
                device_client_id: s.device_client_id,
                trust_2sv: Some(s.trust_2sv),
                redirect_uri: s.redirect_uri,
                issuer: s.issuer,
                ..Default::default()
            }))
        }
        Kind::Jumpcloud => {
            let s = c.jumpcloud().map_err(err_of)?;
            Settings::Jumpcloud(Box::new(pb::JumpcloudSettings {
                client_id: s.client_id,
                groups_claim: s.groups_claim,
                allowed_emails: s.allowed_emails,
                redirect_uri: s.redirect_uri,
                issuer: s.issuer,
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
        provisioning: Some(c.provisioning.as_str().into()),
        group_mode: Some(c.group_mode.as_str().into()),
        sync: c.sync.as_ref().map(sync_settings_pb).into(),
        sync_status: (!c.provisioning.is_none() || c.sync.is_some())
            .then(|| call.auth().sync_status.get(&c.id).ok().flatten())
            .flatten()
            .map(|s| sync_status_pb(&s))
            .into(),
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
        Settings::Google(g) => {
            let accounts = match g.accounts.as_known().unwrap_or_default() {
                pb::GoogleAccounts::GOOGLE_ACCOUNTS_ANY => "any",
                _ => "workspace",
            };
            let mut v = serde_json::json!({
                "accounts": accounts,
                "client_id": g.client_id,
                "domains": g.domains,
                "allowed_emails": g.allowed_emails,
                "trust_2sv": g.trust_2sv.unwrap_or(false),
            });
            for (k, x) in [
                ("device_client_id", &g.device_client_id),
                ("redirect_uri", &g.redirect_uri),
                ("issuer", &g.issuer),
            ] {
                if let Some(x) = x {
                    v[k] = serde_json::json!(x);
                }
            }
            (Kind::Google, v)
        }
        Settings::Jumpcloud(j) => {
            let mut v = serde_json::json!({
                "client_id": j.client_id,
                "allowed_emails": j.allowed_emails,
            });
            for (k, x) in [
                ("groups_claim", &j.groups_claim),
                ("redirect_uri", &j.redirect_uri),
                ("issuer", &j.issuer),
            ] {
                if let Some(x) = x {
                    v[k] = serde_json::json!(x);
                }
            }
            (Kind::Jumpcloud, v)
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

fn sync_settings_pb(s: &dir::SyncSettings) -> pb::SyncSettings {
    pb::SyncSettings {
        interval_minutes: s.interval_minutes,
        max_disable: s.max_disable,
        admin_email: s.admin_email.clone(),
        groups: s.groups.clone(),
        ssh_keys_field: s.ssh_keys_field.clone(),
        ..Default::default()
    }
}

fn sync_settings_of(p: &pb::SyncSettings) -> dir::SyncSettings {
    dir::SyncSettings {
        interval_minutes: p.interval_minutes,
        max_disable: p.max_disable,
        admin_email: p.admin_email.trim().to_ascii_lowercase(),
        groups: p
            .groups
            .iter()
            .map(|g| g.trim().to_ascii_lowercase())
            .filter(|g| !g.is_empty())
            .collect(),
        ssh_keys_field: p.ssh_keys_field.clone().filter(|f| !f.trim().is_empty()),
    }
}

fn sync_status_pb(s: &dir::sync::SyncStatus) -> pb::SyncStatus {
    use dir::sync::SyncState as S;
    pb::SyncStatus {
        state: match s.state {
            S::Ok => pb::SyncState::SYNC_STATE_OK,
            S::Failed => pb::SyncState::SYNC_STATE_FAILED,
            S::Blocked => pb::SyncState::SYNC_STATE_BLOCKED,
        }
        .into(),
        started_at: ts(s.started_at).into(),
        finished_at: ts(s.finished_at).into(),
        changes: pb::SyncChanges {
            created: s.changes.created as u32,
            adopted: s.changes.adopted as u32,
            updated: s.changes.updated as u32,
            disabled: s.changes.disabled as u32,
            enabled: s.changes.enabled as u32,
            ..Default::default()
        }
        .into(),
        error: s.error.clone(),
        would_disable: s.would_disable.clone(),
        last_applied_at: s.last_applied_at.map(ts).into(),
        ..Default::default()
    }
}

/// A pull now (§8.2); `confirm` applies one the safety stop blocked (D46),
/// which takes `confirmConnectorSync` as well.
pub async fn sync_connector(
    call: &Call,
    req: pb::SyncConnectorRequest,
) -> ServiceResult<pb::SyncConnectorResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let c = get_visible(call, &req.id)?;
    let confirm = req.confirm == Some(true);
    if confirm {
        call.with_action("confirmConnectorSync")
            .require(directory_ent(), EntitySet::new())?;
    }
    if c.provisioning != dir::Provisioning::Pull {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "not_pulled",
            format!("connector {} isn't provisioned by a pull", c.id),
        ));
    }
    let source = dir::sync::source_for(call.auth(), &c).ok_or_else(|| {
        ServiceError::new(
            StatusCode::CONFLICT,
            "no_source",
            format!("connector {} has nothing to pull from yet", c.id),
        )
    })?;
    call.set_target(format!("connector:{}", c.id));
    let status = dir::sync::run(call.auth(), &c, source.as_ref(), confirm, &call.request_id)
        .await
        .ok_or_else(|| {
            ServiceError::new(
                StatusCode::CONFLICT,
                "sync_running",
                "a run of this connector is going already",
            )
        })?;
    call.detail("state", serde_json::json!(status.state));
    Ok(pb::SyncConnectorResponse {
        status: sync_status_pb(&status).into(),
        ..Default::default()
    })
}

fn scim_token_err(e: dir::scim_tokens::ScimTokenError) -> ServiceError {
    use dir::scim_tokens::ScimTokenError as E;
    match e {
        E::NotFound => ServiceError::new(StatusCode::NOT_FOUND, "not_found", e.to_string()),
        E::Invalid(m) => ServiceError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", m),
        E::Storage(m) => ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", m),
    }
}

fn scim_token_pb(t: &dir::scim_tokens::ScimToken) -> pb::ScimToken {
    pb::ScimToken {
        id: t.id.clone(),
        name: t.name.clone(),
        created_by: t.created_by.clone(),
        created_at: ts(t.created_at).into(),
        expires_at: ts(t.expires_at).into(),
        ..Default::default()
    }
}

/// Where a connector's SCIM endpoints are on this server (§8.1).
pub fn scim_path(connector: &str) -> String {
    format!("/scim/v2/c/{connector}")
}

pub fn list_scim_tokens(
    call: &Call,
    req: pb::ListScimTokensRequest,
) -> ServiceResult<pb::ListScimTokensResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let c = get_visible(call, &req.id)?;
    let tokens = call
        .auth()
        .scim_tokens
        .list(&c.id)
        .map_err(scim_token_err)?;
    Ok(pb::ListScimTokensResponse {
        tokens: tokens.iter().map(scim_token_pb).collect(),
        path: scim_path(&c.id),
        ..Default::default()
    })
}

pub fn create_scim_token(
    call: &Call,
    req: pb::CreateScimTokenRequest,
) -> ServiceResult<pb::CreateScimTokenResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let c = get_visible(call, &req.id)?;
    if c.provisioning != dir::Provisioning::Scim {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "not_provisioned",
            format!(
                "connector {} isn't provisioned by SCIM: set provisioning to scim first",
                c.id
            ),
        ));
    }
    call.set_target(format!("connector:{}", c.id));
    let (t, secret) = call
        .auth()
        .scim_tokens
        .create(
            &c.id,
            &req.name,
            call.p.user_id().unwrap_or("-"),
            req.days.map(u64::from),
        )
        .map_err(scim_token_err)?;
    // Its id and lifetime are audited, never the token.
    call.detail(
        "scim_token",
        serde_json::json!({ "id": t.id, "name": t.name, "expires_at": t.expires_at }),
    );
    Ok(pb::CreateScimTokenResponse {
        token: scim_token_pb(&t).into(),
        secret,
        path: scim_path(&c.id),
        ..Default::default()
    })
}

pub fn revoke_scim_token(
    call: &Call,
    req: pb::RevokeScimTokenRequest,
) -> ServiceResult<pb::RevokeScimTokenResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let c = get_visible(call, &req.id)?;
    call.set_target(format!("connector:{}", c.id));
    let t = call
        .auth()
        .scim_tokens
        .revoke(&c.id, &req.token)
        .map_err(scim_token_err)?;
    call.detail(
        "scim_token",
        serde_json::json!({ "id": t.id, "name": t.name }),
    );
    Ok(pb::RevokeScimTokenResponse::default())
}

fn provisioning_of(s: Option<&str>) -> ServiceResult<crate::directory::Provisioning> {
    crate::directory::Provisioning::parse(s.unwrap_or("")).ok_or_else(|| {
        ServiceError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_connector",
            "provisioning is none or scim",
        )
    })
}

fn group_mode_of(s: Option<&str>) -> ServiceResult<crate::directory::GroupMode> {
    crate::directory::GroupMode::parse(s.unwrap_or("")).ok_or_else(|| {
        ServiceError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_connector",
            "group_mode is mapped or auto",
        )
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
        provisioning: provisioning_of(p.provisioning.as_deref())?,
        group_mode: group_mode_of(p.group_mode.as_deref())?,
        sync: p.sync.as_option().map(sync_settings_of),
        settings,
        source: Source::Api,
        created_at: now,
        updated_at: now,
    };
    let g = guarded(call, &mut c)?;
    // D22: MFA on by default where new users join the owner or an operator.
    if g && p.require_mfa.is_none() && kind.is_oidc() {
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
            "provisioning" => c.provisioning = provisioning_of(p.provisioning.as_deref())?,
            "group_mode" => c.group_mode = group_mode_of(p.group_mode.as_deref())?,
            "sync" => c.sync = p.sync.as_option().map(sync_settings_of),
            "pam" | "oidc" | "google" | "jumpcloud" => {
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
    let disable = match req.users.as_deref() {
        None | Some("disable") => true,
        Some("detach") => false,
        Some(_) => {
            return Err(ServiceError::new(
                StatusCode::BAD_REQUEST,
                "invalid",
                "users is disable or detach",
            ));
        }
    };
    call.auth().connectors.delete(&c.id).map_err(err_of)?;
    call.auth()
        .scim_tokens
        .remove_for(&c.id)
        .map_err(scim_token_err)?;
    let _ = call.auth().sync_status.remove(&c.id);
    let (users, teams) = call
        .auth()
        .store
        .release_connector(&c.id, disable)
        .map_err(|e| ServiceError::from(auth_error(crate::auth::AuthError::Store(e))))?;
    call.detail(
        "users",
        serde_json::json!({ "count": users, "disabled": disable }),
    );
    Ok(pb::DeleteConnectorResponse {
        users: users as u32,
        teams: teams as u32,
        ..Default::default()
    })
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

async fn pull_steps(call: &Call, c: &Connector, steps: &mut Vec<pb::TestStep>) {
    use dir::google_directory::GoogleDirectory;
    let g = GoogleDirectory::default();
    match g.token(call.auth(), c).await {
        Ok(token) => {
            steps.push(step(
                "token",
                true,
                "the service account acts as its administrator",
            ));
            let base = match c.google().ok().and_then(|s| s.issuer) {
                Some(mock) => format!("{mock}/admin/directory/v1"),
                None => "https://admin.googleapis.com/admin/directory/v1".into(),
            };
            for email in c
                .sync
                .as_ref()
                .map(|s| s.groups.clone())
                .unwrap_or_default()
            {
                match g.group(&token, &base, &email).await {
                    Ok((_, name)) => steps.push(step("group", true, format!("{email} ({name})"))),
                    Err(e) => steps.push(step("group", false, format!("{email}: {e}"))),
                }
            }
        }
        Err(e) => steps.push(step("token", false, e.to_string())),
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
        Kind::Oidc | Kind::Google | Kind::Jumpcloud => {
            if c.source == Source::Api {
                // The device client's secret only where there is one.
                for field in &c.secrets_in_use() {
                    let set = call
                        .auth()
                        .connectors
                        .secret(&c, field)
                        .map_err(err_of)?
                        .is_some();
                    steps.push(step(
                        field,
                        set,
                        if set {
                            "set".to_string()
                        } else {
                            format!("not set: PUT /connectors/{{id}}/secrets/{field}")
                        },
                    ));
                }
            }
            match call.app.auth.oidc_for(Some(&c.id)) {
                Ok((_, oidc)) => match oidc.discovery().await {
                    Ok(d) => {
                        steps.push(step("discovery", true, format!("issuer {}", d.issuer)));
                        let offered = d.device_authorization_endpoint.is_some();
                        let (ok, what) = match (offered, c.has_device_client()) {
                            (true, true) => (true, "offered (gxctl logins)"),
                            (true, false) => (
                                false,
                                "no device client: set device_client_id (a TVs and Limited Input client) for gxctl logins",
                            ),
                            (false, _) => (false, "not offered: gxctl needs a token or PAM"),
                        };
                        steps.push(step("device grant", ok, what));
                    }
                    Err(e) => steps.push(step("discovery", false, e.to_string())),
                },
                Err(e) => steps.push(step("connector", false, e.to_string())),
            }
            // A pull's token, and each group it reads (§8.2).
            if c.provisioning == dir::Provisioning::Pull {
                pull_steps(call, &c, &mut steps).await;
            }
        }
    }
    Ok(pb::TestConnectorResponse {
        steps,
        ..Default::default()
    })
}
