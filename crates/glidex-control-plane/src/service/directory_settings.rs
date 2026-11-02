//! A directory's settings (spec/19-tenancy.md §11.1, D78, D81):
//! `glidex.directory.v1.DirectorySettingsService`. Always the caller's own
//! directory (D58).

use super::{Call, ServiceError, ServiceResult};
use crate::api::auth_error;
use crate::authz::EntitySet;
use crate::service::tenancy::tenant_error;
use crate::tenants::{DirectorySettings, SelfService, TenantLabel};
use axum::http::StatusCode;
use glidex_api::proto::glidex::directory::v1 as pb;

fn settings_pb(s: &DirectorySettings) -> pb::DirectorySettings {
    let ss = &s.self_service;
    pb::DirectorySettings {
        self_service: pb::SelfServiceSettings {
            enabled: ss.enabled,
            creator_teams: ss.creator_teams.clone(),
            max_per_user: Some(ss.max_per_user),
            max_total: ss.max_total,
            approval_required: ss.approval_required,
            slug_prefix: ss.slug_prefix.clone(),
            admins_may_delete: Some(ss.admins_may_delete),
            ..Default::default()
        }
        .into(),
        tenant_label: pb::TenantLabel {
            singular: s.tenant_label.singular.clone(),
            plural: s.tenant_label.plural.clone(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

/// The settings `p` asks for, its teams resolved to ids of the caller's
/// directory; absent parts are the defaults.
fn settings_from(call: &Call, p: &pb::DirectorySettings) -> ServiceResult<DirectorySettings> {
    let mut s = DirectorySettings::default();
    if let Some(ss) = p.self_service.as_option() {
        let dflt = SelfService::default();
        let mut teams = Vec::new();
        for key in &ss.creator_teams {
            let store = &call.auth().store;
            let team = match store.team(key).map_err(|e| auth_error(e.into()))? {
                Some(t) => Some(t),
                None => store.team_by_name(key).map_err(|e| auth_error(e.into()))?,
            }
            .filter(|t| t.directory_id == call.p.directory_id())
            .ok_or_else(|| {
                ServiceError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid",
                    format!("no team {key} in this directory"),
                )
            })?;
            if !teams.contains(&team.id) {
                teams.push(team.id);
            }
        }
        s.self_service = SelfService {
            enabled: ss.enabled,
            creator_teams: teams,
            max_per_user: ss.max_per_user.unwrap_or(dflt.max_per_user),
            max_total: ss.max_total,
            approval_required: ss.approval_required,
            slug_prefix: ss.slug_prefix.clone(),
            admins_may_delete: ss.admins_may_delete.unwrap_or(dflt.admins_may_delete),
        };
    }
    if let Some(l) = p.tenant_label.as_option() {
        let dflt = TenantLabel::default();
        let or = |v: &str, d: String| if v.is_empty() { d } else { v.to_string() };
        s.tenant_label = TenantLabel {
            singular: or(&l.singular, dflt.singular),
            plural: or(&l.plural, dflt.plural),
        };
    }
    Ok(s)
}

fn current(call: &Call) -> ServiceResult<DirectorySettings> {
    Ok(call
        .auth()
        .tenants
        .get_directory(call.p.directory_id())
        .map_err(tenant_error)?
        .map(|d| d.settings)
        .unwrap_or_default())
}

/// Whether `p` may create a tenant of their own now, and how many more
/// (D78), for `WhoAmI`.
pub fn self_service_of(
    app: &crate::api::App,
    p: &crate::auth::Principal,
) -> glidex_api::proto::glidex::system::v1::SelfService {
    use glidex_api::proto::glidex::system::v1::SelfService as Pb;
    let Some(user) = p.user.as_ref().filter(|_| !p.token_narrowed) else {
        return Pb::default();
    };
    let dir = p.directory_id();
    let ss = app
        .auth
        .tenants
        .get_directory(dir)
        .ok()
        .flatten()
        .map(|d| d.settings.self_service)
        .unwrap_or_default();
    let permitted = ss.enabled
        && app
            .auth
            .authorize(
                p,
                "createOwnTenant",
                crate::authz::Ent::Directory(dir.into()),
                EntitySet::new(),
                &[],
            )
            .allowed;
    if !permitted {
        return Pb::default();
    }
    let (mine, all) = app
        .auth
        .tenants
        .self_service_counts(dir, &user.id)
        .unwrap_or((u32::MAX, u32::MAX));
    let remaining = ss
        .max_per_user
        .saturating_sub(mine)
        .min(ss.max_total.map_or(u32::MAX, |m| m.saturating_sub(all)));
    Pb {
        allowed: remaining > 0,
        remaining: Some(remaining),
        approval_required: ss.approval_required,
        slug_prefix: ss.slug_prefix,
        ..Default::default()
    }
}

/// What the caller's directory calls a tenant (D81), for `WhoAmI`.
pub fn tenant_label(app: &crate::api::App, directory: &str) -> TenantLabel {
    app.auth
        .tenants
        .get_directory(directory)
        .ok()
        .flatten()
        .map(|d| d.settings.tenant_label)
        .unwrap_or_default()
}

pub fn get_directory_settings(
    call: &Call,
    _req: pb::GetDirectorySettingsRequest,
) -> ServiceResult<pb::GetDirectorySettingsResponse> {
    call.require(call.directory_ent(), EntitySet::new())?;
    Ok(pb::GetDirectorySettingsResponse {
        settings: settings_pb(&current(call)?).into(),
        ..Default::default()
    })
}

pub fn update_directory_settings(
    call: &Call,
    req: pb::UpdateDirectorySettingsRequest,
) -> ServiceResult<pb::UpdateDirectorySettingsResponse> {
    call.require(call.directory_ent(), EntitySet::new())?;
    let p = req.settings.as_option().ok_or_else(|| {
        ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "settings is required")
    })?;
    let s = settings_from(call, p)?;
    call.set_target(format!("directory:{}", call.p.directory_id()));
    call.detail("settings", serde_json::json!(s));
    let d = call
        .auth()
        .tenants
        .set_settings(call.p.directory_id(), s)
        .map_err(tenant_error)?;
    Ok(pb::UpdateDirectorySettingsResponse {
        settings: settings_pb(&d.settings).into(),
        ..Default::default()
    })
}
