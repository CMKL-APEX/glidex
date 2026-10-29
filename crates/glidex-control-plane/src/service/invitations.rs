//! Invitations to tenants (spec/19-tenancy.md D40, §6.1): `glidex.directory.v1.
//! InvitationService`, and `LoginService.InspectInvitation` for the
//! invitation page before its login.

use super::{Call, ServiceError, ServiceResult};
use crate::api::AppState;
use crate::authz::{Ent, EntitySet};
use crate::directory::Connector;
use crate::directory::invitations::{Invitation, InvitationError, InviteToken};
use axum::http::StatusCode;
use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::auth::v1 as login_pb;
use glidex_api::proto::glidex::directory::v1 as pb;

/// Membership only, without a tenant role.
pub const MEMBER: &str = "member";

fn ts(secs: u64) -> Timestamp {
    Timestamp {
        seconds: secs as i64,
        ..Default::default()
    }
}

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> ServiceError {
    ServiceError::new(status, code, message)
}

fn store_err(e: InvitationError) -> ServiceError {
    match e {
        InvitationError::NotFound => err(StatusCode::NOT_FOUND, "not_found", "no such invitation"),
        InvitationError::Invalid(m) => err(StatusCode::BAD_REQUEST, "invalid", m),
        InvitationError::Storage(m) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal", m),
    }
}

fn invitation_pb(i: &Invitation) -> pb::Invitation {
    let now = crate::tenancy::now();
    pb::Invitation {
        id: i.id.clone(),
        tenant_id: i.tenant_id.clone(),
        email: i.email.clone(),
        role: i.role.clone(),
        connector_id: i.connector_id.clone(),
        invited_by: i.invited_by.clone(),
        created_at: ts(i.created_at).into(),
        expires_at: ts(i.expires_at).into(),
        state: if i.taken_at.is_some() {
            pb::InvitationState::INVITATION_STATE_TAKEN
        } else if i.is_open(now) {
            pb::InvitationState::INVITATION_STATE_OPEN
        } else {
            pb::InvitationState::INVITATION_STATE_EXPIRED
        }
        .into(),
        taken_by: i.taken_by.clone(),
        taken_at: i.taken_at.map(ts).into(),
        ..Default::default()
    }
}

pub fn list_invitations(
    call: &Call,
    req: pb::ListInvitationsRequest,
) -> ServiceResult<pb::ListInvitationsResponse> {
    let t = super::tenancy::visible(call, &req.tenant)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let list = call
        .auth()
        .invitations
        .list(Some(&t.id))
        .map_err(store_err)?;
    Ok(pb::ListInvitationsResponse {
        invitations: list.iter().map(invitation_pb).collect(),
        ..Default::default()
    })
}

/// The connector an invitation to `tenant` may go through: an enabled one
/// that logs in by email; for the owner's or an operator's tenant, one
/// D22 allows there, requiring MFA.
fn usable_connector(call: &Call, id: &str, guarded: bool) -> ServiceResult<Connector> {
    let c = call
        .auth()
        .connectors
        .get(id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?
        .ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "invalid",
                format!("no connector {id}"),
            )
        })?;
    if !c.enabled || !c.kind.is_oidc() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "invitations go through an enabled OIDC or google connector: its logins carry a verified email",
        ));
    }
    if guarded {
        // Whether D22 would take it there, whoever else it lets in.
        let mut as_guarded = c.clone();
        as_guarded.jit = false;
        as_guarded.validate(true).map_err(|e| {
            err(
                StatusCode::BAD_REQUEST,
                "invalid",
                format!("the tenant is the directory owner's or an operator's: {e}"),
            )
        })?;
        if !c.require_mfa {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "invalid",
                "the tenant is the directory owner's or an operator's: invite through a connector that requires MFA",
            ));
        }
    }
    Ok(c)
}

pub fn create_invitation(
    call: &Call,
    req: pb::CreateInvitationRequest,
) -> ServiceResult<pb::CreateInvitationResponse> {
    let t = super::tenancy::visible(call, &req.tenant)?;
    let tenant = Ent::Tenant(t.id.clone());
    call.require(tenant.clone(), EntitySet::new())?;
    if req.role != MEMBER && !crate::authz::TENANT_ROLES.contains(&req.role.as_str()) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            format!(
                "role is {MEMBER} (membership only) or one of {}",
                crate::authz::TENANT_ROLES.join(", ")
            ),
        ));
    }
    // Taking it adds a member and links the role: what the inviter may do.
    call.require_action("manageTenantMembers", tenant.clone(), EntitySet::new(), &[])?;
    if req.role != MEMBER {
        call.require_action("manageTenantBindings", tenant, EntitySet::new(), &[])?;
    }
    let c = usable_connector(call, &req.connector, t.owns_directory || t.operator)?;
    let by = call.p.user_id().unwrap_or("-").to_string();
    let (inv, token) = call
        .auth()
        .invitations
        .create(
            &t.id,
            &req.email,
            &req.role,
            &c.id,
            &by,
            req.days.map(u64::from),
        )
        .map_err(store_err)?;
    call.set_target(format!("invitation:{}", inv.id));
    call.detail("email", serde_json::json!(inv.email));
    call.detail("role", serde_json::json!(inv.role));
    call.detail("connector", serde_json::json!(inv.connector_id));
    let origin = call
        .auth()
        .config
        .auth
        .allowed_origins
        .first()
        .map(String::as_str)
        .unwrap_or("http://localhost:5173")
        .trim_end_matches('/');
    // The secret is the fragment: browsers don't send it, so no log has it.
    let (id, secret) = token.split_once('.').expect("id.secret");
    Ok(pb::CreateInvitationResponse {
        invitation: invitation_pb(&inv).into(),
        link: format!("{origin}/invite/{id}#{secret}"),
        ..Default::default()
    })
}

pub fn revoke_invitation(
    call: &Call,
    req: pb::RevokeInvitationRequest,
) -> ServiceResult<pb::RevokeInvitationResponse> {
    let not_found = || err(StatusCode::NOT_FOUND, "not_found", "no such invitation");
    let inv = call
        .auth()
        .invitations
        .get(&req.id)
        .map_err(store_err)?
        .ok_or_else(not_found)?;
    // Seen only by those who may see the tenant's invitations.
    call.require_visible(
        "readTenantInvitations",
        Ent::Tenant(inv.tenant_id.clone()),
        EntitySet::new(),
        "invitation",
    )?;
    call.require(Ent::Tenant(inv.tenant_id.clone()), EntitySet::new())?;
    call.set_target(format!("invitation:{}", inv.id));
    call.auth().invitations.revoke(&inv.id).map_err(store_err)?;
    Ok(pb::RevokeInvitationResponse::default())
}

/// The invitation a link names, for its page: `404` unless the secret
/// matches an open one.
pub fn inspect_invitation(
    app: &AppState,
    req: login_pb::InspectInvitationRequest,
) -> ServiceResult<login_pb::InspectInvitationResponse> {
    let not_found = || err(StatusCode::NOT_FOUND, "not_found", "no such invitation");
    let token = InviteToken::parse(&req.invitation).ok_or_else(not_found)?;
    let inv = app
        .auth
        .invitations
        .open(&token)
        .map_err(store_err)?
        .ok_or_else(not_found)?;
    let tenant = app
        .auth
        .tenants
        .get(&inv.tenant_id)
        .ok()
        .flatten()
        .ok_or_else(not_found)?;
    // A passkey invitation (D60): its page registers one.
    if inv.connector_id == crate::auth::passkeys::PROVIDER {
        return Ok(login_pb::InspectInvitationResponse {
            tenant_slug: tenant.slug,
            tenant_display_name: tenant.display_name,
            connector_id: inv.connector_id.clone(),
            connector_display_name: "Passkey".into(),
            connector_kind: "passkey".into(),
            email: inv.email,
            role: inv.role,
            expires_at: ts(inv.expires_at).into(),
            ..Default::default()
        });
    }
    let c = app
        .auth
        .connectors
        .get(&inv.connector_id)
        .ok()
        .flatten()
        .filter(|c| c.enabled)
        .ok_or_else(not_found)?;
    Ok(login_pb::InspectInvitationResponse {
        tenant_slug: tenant.slug,
        tenant_display_name: tenant.display_name,
        connector_id: c.id.clone(),
        connector_display_name: c.display_name.clone(),
        connector_kind: c.kind.as_str().into(),
        email: inv.email,
        role: inv.role,
        expires_at: ts(inv.expires_at).into(),
        ..Default::default()
    })
}
