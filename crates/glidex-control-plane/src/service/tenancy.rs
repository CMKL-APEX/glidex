//! `glidex.tenancy.v1`: tenants, their members and bindings, and this
//! cluster's admissions (spec/tenancy.md §14, T0). REST (`api/tenants.rs`)
//! and RPC (`api/rpc/tenancy.rs`) both call these functions and nothing
//! else.

use super::{Call, ServiceError, ServiceResult};
use crate::api::auth_error;
use crate::authz::{Ent, EntitySet, TENANT_ROLES};
use crate::tenants::{self, AdmissionStatus, TenantError};
use axum::http::StatusCode;
use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::api::v1::EntityRef;
use glidex_api::proto::glidex::directory::v1 as pb_dir;
use glidex_api::proto::glidex::tenancy::v1 as pb;

const MAX_PAGE: usize = 500;
const DEFAULT_PAGE: usize = 100;

pub fn tenant_error(e: TenantError) -> ServiceError {
    match e {
        TenantError::NotFound(m) => ServiceError::new(StatusCode::NOT_FOUND, "not_found", m),
        TenantError::AlreadyExists(m) => ServiceError::new(
            StatusCode::CONFLICT,
            "conflict",
            format!("already exists: {m}"),
        ),
        TenantError::Invalid(m) => ServiceError::new(StatusCode::BAD_REQUEST, "invalid", m),
        TenantError::InUse(m) => ServiceError::new(StatusCode::CONFLICT, "tenant_in_use", m),
        TenantError::Storage(m) => {
            ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "persistence_error", m)
        }
    }
}

fn ts(secs: u64) -> Timestamp {
    Timestamp::from_unix_secs(secs as i64)
}

pub fn quotas_to_pb(q: &crate::tenancy::Quotas) -> pb::Quotas {
    let c = |v: Option<u64>| v.map(|v| v.min(u32::MAX as u64) as u32);
    pb::Quotas {
        vms: c(q.vms),
        vcpus: c(q.vcpus),
        memory_mib: c(q.memory_mib),
        disk_gib: c(q.disk_gib),
        running_vms: c(q.running_vms),
        networks: c(q.networks),
        routers: c(q.routers),
        external_ips: c(q.external_ips),
        devices: q
            .devices
            .iter()
            .map(|(k, &v)| (k.clone(), v.min(u32::MAX as u64) as u32))
            .collect(),
        ..Default::default()
    }
}

pub fn quotas_from_pb(q: &pb::Quotas) -> crate::tenancy::Quotas {
    let c = |v: Option<u32>| v.map(u64::from);
    crate::tenancy::Quotas {
        vms: c(q.vms),
        vcpus: c(q.vcpus),
        memory_mib: c(q.memory_mib),
        disk_gib: c(q.disk_gib),
        running_vms: c(q.running_vms),
        networks: c(q.networks),
        routers: c(q.routers),
        external_ips: c(q.external_ips),
        devices: q
            .devices
            .iter()
            .map(|(k, &v)| (k.clone(), v.into()))
            .collect(),
    }
}

fn entity_ref(e: &Ent) -> EntityRef {
    EntityRef {
        r#type: e.type_name().into(),
        id: Some(e.id().into_owned()),
        ..Default::default()
    }
}

fn entity_from(r: &EntityRef) -> ServiceResult<Ent> {
    let ent =
        r.id.as_deref()
            .and_then(|id| Ent::from_parts(&r.r#type, id));
    match ent {
        Some(e @ (Ent::User(_) | Ent::Team(_) | Ent::Token(_))) => Ok(e),
        _ => Err(ServiceError::new(
            StatusCode::BAD_REQUEST,
            "invalid",
            "principal must be a User, Team or Token with an id",
        )),
    }
}

/// `user:<id>` or `team:<id>`, as REST paths name a member.
pub fn parse_principal(key: &str) -> ServiceResult<EntityRef> {
    let (ty, id) = key.split_once(':').ok_or_else(|| {
        ServiceError::new(
            StatusCode::BAD_REQUEST,
            "invalid",
            "a principal is user:<id> or team:<id>",
        )
    })?;
    let ty = match ty {
        "user" | "User" => "User",
        "team" | "Team" => "Team",
        _ => {
            return Err(ServiceError::new(
                StatusCode::BAD_REQUEST,
                "invalid",
                "a principal is user:<id> or team:<id>",
            ));
        }
    };
    Ok(EntityRef {
        r#type: ty.into(),
        id: Some(id.into()),
        ..Default::default()
    })
}

fn admission_pb(call: &Call, a: &tenants::Admission) -> pb::Admission {
    // A trusted directory's tenant is named as the handshake brought it.
    let (slug, name) = match call.auth().tenants.get(&a.tenant_id).ok().flatten() {
        Some(t) => (t.slug, t.display_name),
        None => call
            .auth()
            .trust
            .directories()
            .unwrap_or_default()
            .into_iter()
            .flat_map(|d| d.tenants)
            .find(|t| t.id == a.tenant_id)
            .map(|t| (t.slug, t.display_name))
            .unwrap_or_default(),
    };
    pb::Admission {
        tenant_slug: slug,
        tenant_display_name: name,
        tenant_id: a.tenant_id.clone(),
        directory_id: a.directory_id.clone(),
        cap: quotas_to_pb(&a.cap).into(),
        status: match a.status {
            AdmissionStatus::Admitted => pb::AdmissionStatus::ADMISSION_STATUS_ADMITTED,
            AdmissionStatus::Suspended => pb::AdmissionStatus::ADMISSION_STATUS_SUSPENDED,
        }
        .into(),
        by: a.by.clone(),
        at: ts(a.at).into(),
        ..Default::default()
    }
}

fn tenant_pb(call: &Call, t: &tenants::Tenant) -> ServiceResult<pb::Tenant> {
    let admission = call.auth().tenants.admission(&t.id).map_err(tenant_error)?;
    Ok(pb::Tenant {
        id: t.id.clone(),
        directory_id: t.directory_id.clone(),
        slug: t.slug.clone(),
        display_name: t.display_name.clone(),
        owns_directory: Some(t.owns_directory),
        operator: Some(t.operator),
        suspended: Some(t.suspended),
        created_at: ts(t.created_at).into(),
        admission: admission.as_ref().map(|a| admission_pb(call, a)).into(),
        operates_here: Some(call.auth().tenants.operator().map_err(tenant_error)? == t.id),
        ..Default::default()
    })
}

/// `e` exists in `directory`: nobody is a member or holds a role in
/// another directory's tenant (D3, D58); another directory's principals
/// answer as missing.
fn principal_exists(call: &Call, e: &Ent, directory: &str) -> ServiceResult<()> {
    let ok = call
        .auth()
        .principal_directory(e)
        .map_err(auth_error)?
        .is_some_and(|d| d == directory);
    if ok {
        Ok(())
    } else {
        Err(ServiceError::new(
            StatusCode::BAD_REQUEST,
            "invalid",
            format!("{e} doesn't exist"),
        ))
    }
}

/// A tenant the caller may read, by id or slug; `404` otherwise (as for
/// projects, security.md §7.4).
pub(crate) fn visible(call: &Call, key: &str) -> ServiceResult<tenants::Tenant> {
    let not_found = || ServiceError::new(StatusCode::NOT_FOUND, "not_found", "tenant not found");
    let t = call
        .auth()
        .tenants
        .resolve_in(call.p.directory_id(), key)
        .map_err(|_| not_found())?;
    call.set_target(format!("tenant:{}", t.id));
    call.require_visible(
        "readTenant",
        Ent::Tenant(t.id.clone()),
        EntitySet::new(),
        "tenant",
    )?;
    Ok(t)
}

// ---- tenants -------------------------------------------------------------

pub fn list_tenants(
    call: &Call,
    req: pb::ListTenantsRequest,
) -> ServiceResult<pb::ListTenantsResponse> {
    let all = call.auth().tenants.list().map_err(tenant_error)?;
    let everyone = call.allowed("listTenants", call.directory_ent(), EntitySet::new());
    let visible: Vec<_> = all
        .into_iter()
        .filter(|t| t.directory_id == call.p.directory_id())
        .filter(|t| {
            everyone || call.allowed("readTenant", Ent::Tenant(t.id.clone()), EntitySet::new())
        })
        .collect();
    let size = req
        .page_size
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_PAGE)
        .min(MAX_PAGE);
    let start: usize = if req.page_token.is_empty() {
        0
    } else {
        req.page_token
            .parse()
            .map_err(|_| ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "bad page_token"))?
    };
    let page: Vec<_> = visible.iter().skip(start).take(size).collect();
    let next = if start + page.len() < visible.len() {
        (start + page.len()).to_string()
    } else {
        String::new()
    };
    Ok(pb::ListTenantsResponse {
        tenants: page
            .into_iter()
            .map(|t| tenant_pb(call, t))
            .collect::<ServiceResult<_>>()?,
        next_page_token: next,
        ..Default::default()
    })
}

pub fn get_tenant(call: &Call, req: pb::GetTenantRequest) -> ServiceResult<pb::GetTenantResponse> {
    let t = visible(call, &req.id)?;
    Ok(pb::GetTenantResponse {
        tenant: tenant_pb(call, &t)?.into(),
        ..Default::default()
    })
}

pub fn create_tenant(
    call: &Call,
    req: pb::CreateTenantRequest,
) -> ServiceResult<pb::CreateTenantResponse> {
    call.require(call.directory_ent(), EntitySet::new())?;
    let t = call
        .auth()
        .tenants
        .create_in(
            call.p.directory_id(),
            &req.slug,
            req.display_name,
            req.operator.unwrap_or(false),
        )
        .map_err(tenant_error)?;
    call.set_target(format!("tenant:{}", t.id));
    call.detail("slug", serde_json::json!(t.slug));
    // The first administrator's invitation, made as any invitation is (its
    // rules, D22 for an operator tenant); if it can't be, no tenant either.
    let link = match req.first_admin.as_option() {
        Some(f) => {
            let inv = pb_dir::CreateInvitationRequest {
                tenant: t.id.clone(),
                email: f.email.clone(),
                role: "role.tenant-admin".into(),
                connector: f.connector.clone(),
                days: f.days,
                ..Default::default()
            };
            let as_invite = call.with_action("manageTenantInvitations");
            match super::invitations::create_invitation(&as_invite, inv) {
                Ok(r) => Some(r.link),
                Err(e) => {
                    let _ = call.auth().tenants.delete(&t.id);
                    return Err(e);
                }
            }
        }
        None => None,
    };
    Ok(pb::CreateTenantResponse {
        tenant: tenant_pb(call, &t)?.into(),
        invitation_link: link,
        ..Default::default()
    })
}

pub fn update_tenant(
    call: &Call,
    req: pb::UpdateTenantRequest,
) -> ServiceResult<pb::UpdateTenantResponse> {
    let mut t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    if let Some(name) = req.display_name {
        t.display_name = name;
    }
    if let Some(op) = req.operator {
        if !op && t.id == call.auth().tenants.operator().map_err(tenant_error)? {
            return Err(ServiceError::new(
                StatusCode::CONFLICT,
                "conflict",
                "this tenant operates this cluster",
            ));
        }
        t.operator = op;
    }
    call.auth().tenants.update(&t).map_err(tenant_error)?;
    Ok(pb::UpdateTenantResponse {
        tenant: tenant_pb(call, &t)?.into(),
        ..Default::default()
    })
}

fn set_suspended(call: &Call, key: &str, suspended: bool) -> ServiceResult<pb::Tenant> {
    let mut t = visible(call, key)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    if t.owns_directory || t.id == call.auth().tenants.operator().map_err(tenant_error)? {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "conflict",
            "the directory's owner and the cluster's operator can't be suspended",
        ));
    }
    t.suspended = suspended;
    call.auth().tenants.update(&t).map_err(tenant_error)?;
    tenant_pb(call, &t)
}

pub fn suspend_tenant(
    call: &Call,
    req: pb::SuspendTenantRequest,
) -> ServiceResult<pb::SuspendTenantResponse> {
    Ok(pb::SuspendTenantResponse {
        tenant: set_suspended(call, &req.id, true)?.into(),
        ..Default::default()
    })
}

pub fn resume_tenant(
    call: &Call,
    req: pb::ResumeTenantRequest,
) -> ServiceResult<pb::ResumeTenantResponse> {
    Ok(pb::ResumeTenantResponse {
        tenant: set_suspended(call, &req.id, false)?.into(),
        ..Default::default()
    })
}

pub fn delete_tenant(
    call: &Call,
    req: pb::DeleteTenantRequest,
) -> ServiceResult<pb::DeleteTenantResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let projects = call.manager().projects().list().map_err(|e| {
        ServiceError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "persistence_error",
            e.to_string(),
        )
    })?;
    if let Some(p) = projects.iter().find(|p| p.tenant_id == t.id) {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "tenant_in_use",
            format!("tenant {} still has project {}", t.slug, p.name),
        ));
    }
    call.auth().tenants.delete(&t.id).map_err(tenant_error)?;
    call.auth()
        .forget_entity(&Ent::Tenant(t.id.clone()))
        .map_err(auth_error)?;
    Ok(pb::DeleteTenantResponse::default())
}

// ---- members --------------------------------------------------------------

fn member_pb(call: &Call, m: &tenants::Member) -> ServiceResult<pb::Member> {
    Ok(pb::Member {
        principal_name: principal_name(call, &m.principal)?.unwrap_or_default(),
        tenant_id: m.tenant_id.clone(),
        principal: entity_ref(&m.principal).into(),
        by: m.by.clone(),
        at: ts(m.at).into(),
        ..Default::default()
    })
}

pub fn list_members(
    call: &Call,
    req: pb::ListMembersRequest,
) -> ServiceResult<pb::ListMembersResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let members = call.auth().tenants.members(&t.id).map_err(tenant_error)?;
    Ok(pb::ListMembersResponse {
        members: members
            .iter()
            .map(|m| member_pb(call, m))
            .collect::<ServiceResult<_>>()?,
        ..Default::default()
    })
}

pub fn add_member(call: &Call, req: pb::AddMemberRequest) -> ServiceResult<pb::AddMemberResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let principal = entity_from(req.principal.as_option().ok_or_else(|| {
        ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "principal is required")
    })?)?;
    principal_exists(call, &principal, &t.directory_id)?;
    call.detail("principal", serde_json::json!(principal));
    let by = call.p.user_id().unwrap_or("-").to_string();
    let m = call
        .auth()
        .tenants
        .add_member(&t.id, principal, &by)
        .map_err(tenant_error)?;
    Ok(pb::AddMemberResponse {
        member: member_pb(call, &m)?.into(),
        ..Default::default()
    })
}

pub fn remove_member(
    call: &Call,
    req: pb::RemoveMemberRequest,
) -> ServiceResult<pb::RemoveMemberResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let principal = entity_from(&parse_principal(&req.principal)?)?;
    call.detail("principal", serde_json::json!(principal));
    call.auth()
        .tenants
        .remove_member(&t.id, &principal)
        .map_err(tenant_error)?;
    Ok(pb::RemoveMemberResponse::default())
}

// ---- bindings (tenant roles) ---------------------------------------------

/// A user's display name, a team's or a token's name.
fn principal_name(call: &Call, e: &Ent) -> ServiceResult<Option<String>> {
    let s = &call.auth().store;
    Ok(match e {
        Ent::User(id) => s
            .user(id)
            .map_err(|e| auth_error(e.into()))?
            .map(|u| u.display_name),
        Ent::Team(id) => s
            .team(id)
            .map_err(|e| auth_error(e.into()))?
            .map(|t| t.name),
        Ent::Token(id) => s
            .token_by_id(id)
            .map_err(|e| auth_error(e.into()))?
            .map(|t| t.1.name),
        _ => None,
    })
}

fn binding_pb(call: &Call, l: &crate::auth::store::LinkRecord) -> ServiceResult<pb::Binding> {
    let name = principal_name(call, &l.link.principal)?;
    Ok(pb::Binding {
        id: l.link.id.clone(),
        role: l.link.template.clone(),
        principal: entity_ref(&l.link.principal).into(),
        principal_name: name.unwrap_or_default(),
        created_by: l.created_by.clone(),
        created_at: ts(l.created_at).into(),
        ..Default::default()
    })
}

pub fn list_bindings(
    call: &Call,
    req: pb::ListBindingsRequest,
) -> ServiceResult<pb::ListBindingsResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let mine = Ent::Tenant(t.id.clone());
    let links = call
        .auth()
        .store
        .links()
        .map_err(|e| auth_error(e.into()))?;
    Ok(pb::ListBindingsResponse {
        bindings: links
            .iter()
            .filter(|l| l.link.resource == mine)
            .map(|l| binding_pb(call, l))
            .collect::<ServiceResult<_>>()?,
        ..Default::default()
    })
}

pub fn add_binding(
    call: &Call,
    req: pb::AddBindingRequest,
) -> ServiceResult<pb::AddBindingResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    if !TENANT_ROLES.contains(&req.role.as_str()) {
        return Err(ServiceError::new(
            StatusCode::BAD_REQUEST,
            "invalid",
            format!("tenant roles are {}", TENANT_ROLES.join(", ")),
        ));
    }
    let principal = entity_from(req.principal.as_option().ok_or_else(|| {
        ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "principal is required")
    })?)?;
    principal_exists(call, &principal, &t.directory_id)?;
    call.detail("role", serde_json::json!(req.role));
    call.detail("principal", serde_json::json!(principal));
    let by = call.p.user_id().unwrap_or("-").to_string();
    let l = call
        .auth()
        .add_link(&req.role, principal, Ent::Tenant(t.id.clone()), &by)
        .map_err(auth_error)?;
    Ok(pb::AddBindingResponse {
        binding: binding_pb(call, &l)?.into(),
        ..Default::default()
    })
}

pub fn remove_binding(
    call: &Call,
    req: pb::RemoveBindingRequest,
) -> ServiceResult<pb::RemoveBindingResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let links = call
        .auth()
        .store
        .links()
        .map_err(|e| auth_error(e.into()))?;
    match links.into_iter().find(|l| l.link.id == req.link) {
        Some(l) if l.link.resource == Ent::Tenant(t.id.clone()) => {
            call.detail("link", serde_json::json!(l.link));
            call.auth().remove_link(&req.link).map_err(auth_error)?;
            Ok(pb::RemoveBindingResponse::default())
        }
        _ => Err(ServiceError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such binding on this tenant",
        )),
    }
}

// ---- admissions (the cluster's operator) ----------------------------------

pub fn list_admissions(
    call: &Call,
    _req: pb::ListAdmissionsRequest,
) -> ServiceResult<pb::ListAdmissionsResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let all = call.auth().tenants.admissions().map_err(tenant_error)?;
    Ok(pb::ListAdmissionsResponse {
        admissions: all.iter().map(|a| admission_pb(call, a)).collect(),
        ..Default::default()
    })
}

pub fn set_admission(
    call: &Call,
    req: pb::SetAdmissionRequest,
) -> ServiceResult<pb::SetAdmissionResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let store = &call.auth().tenants;
    let t = store.resolve(&req.tenant).map_err(tenant_error)?;
    call.set_target(format!("tenant:{}", t.id));
    let current = store.admission(&t.id).map_err(tenant_error)?;
    let cap = match req.cap.as_option() {
        Some(q) => quotas_from_pb(q),
        None => current
            .as_ref()
            .map(|a| a.cap.clone())
            .unwrap_or_else(tenants::unlimited),
    };
    let status = match (req.suspended, &current) {
        (Some(true), _) => AdmissionStatus::Suspended,
        (Some(false), _) | (None, None) => AdmissionStatus::Admitted,
        (None, Some(a)) => a.status,
    };
    call.detail("cap", serde_json::json!(cap));
    let by = call.p.user_id().unwrap_or("-").to_string();
    let a = store.admit(&t.id, cap, status, &by).map_err(tenant_error)?;
    Ok(pb::SetAdmissionResponse {
        admission: admission_pb(call, &a).into(),
        ..Default::default()
    })
}

pub fn remove_admission(
    call: &Call,
    req: pb::RemoveAdmissionRequest,
) -> ServiceResult<pb::RemoveAdmissionResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let store = &call.auth().tenants;
    // A tenant of a trusted directory (D66): this cluster stops admitting
    // it, and tells its directory.
    let remote = call
        .auth()
        .trust
        .directories()
        .unwrap_or_default()
        .into_iter()
        .find(|d| {
            d.tenants
                .iter()
                .any(|t| t.id == req.tenant || t.slug == req.tenant)
        });
    if let Some(mut d) = remote {
        let tenant = d
            .tenants
            .iter()
            .find(|t| t.id == req.tenant || t.slug == req.tenant)
            .map(|t| t.id.clone())
            .unwrap_or_default();
        call.set_target(format!("tenant:{tenant}"));
        crate::api::trust::forget_tenant_here(&call.app, &mut d, &tenant);
        let app = call.app.clone();
        tokio::spawn(
            async move { crate::api::trust::tell_directory_ended(&app, &d, &tenant).await },
        );
        return Ok(pb::RemoveAdmissionResponse::default());
    }
    let t = store.resolve(&req.tenant).map_err(tenant_error)?;
    call.set_target(format!("tenant:{}", t.id));
    if t.id == store.operator().map_err(tenant_error)? {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "conflict",
            "the cluster's operator stays admitted",
        ));
    }
    store.remove_admission(&t.id).map_err(tenant_error)?;
    Ok(pb::RemoveAdmissionResponse::default())
}

// ---- tenant pools (spec/host-topology.md §9.1a) ----------------------------

/// The tenant's pools as answered: with what its projects ask of each,
/// the cluster pool's free count, and whether the tenant is still granted
/// it. `cluster` is [`crate::service::node::cluster_pools`].
pub(crate) async fn tenant_pools_pb(
    call: &Call,
    m: &crate::state::VmManager,
    tenant: &str,
    cluster: &[glidex_api::proto::glidex::cluster::v1::PciPool],
) -> ServiceResult<Vec<pb::TenantDevicePool>> {
    let pools = call
        .auth()
        .tenants
        .device_pools(tenant)
        .map_err(tenant_error)?;
    let used = m
        .tenant_pools_used(tenant)
        .await
        .map_err(crate::service::host::manager_err)?;
    Ok(pools
        .into_iter()
        .map(|p| pb::TenantDevicePool {
            used: used.get(&p.name).copied().unwrap_or(0).min(u32::MAX as u64) as u32,
            free: cluster
                .iter()
                .find(|c| c.name == p.from)
                .map_or(0, |c| c.free),
            ungranted: !crate::api::vms::pool_tenant_grants(call, &p.from).contains(&p.tenant_id),
            name: p.name,
            from: p.from,
            max: p.max,
            projects: p.projects,
            by: p.by,
            at: ts(p.at).into(),
            ..Default::default()
        })
        .collect())
}

pub async fn list_device_pools(
    call: &Call,
    req: pb::ListDevicePoolsRequest,
) -> ServiceResult<pb::ListDevicePoolsResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let m = call.manager();
    let cluster = crate::service::node::cluster_pools(m).await;
    Ok(pb::ListDevicePoolsResponse {
        pools: tenant_pools_pb(call, m, &t.id, &cluster).await?,
        ..Default::default()
    })
}

/// Replace the tenant's pools: each names a cluster pool the tenant is
/// granted, a `max` of at least 1, and projects of the tenant. VMs that
/// hold devices of a pool left out keep them.
pub async fn set_device_pools(
    call: &Call,
    req: pb::SetDevicePoolsRequest,
) -> ServiceResult<pb::SetDevicePoolsResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let bad = |m: String| ServiceError::new(StatusCode::BAD_REQUEST, "invalid", m);
    let m = call.manager();
    let cluster = m.pci_pools();
    let by = call.p.user_id().unwrap_or("-").to_string();
    let at = crate::auth::store::now();
    let mut out: Vec<tenants::TenantPool> = Vec::new();
    for p in &req.pools {
        let name = p.name.trim();
        if name.is_empty()
            || name.len() > 63
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(bad(format!(
                "tenant pool name '{name}': 1-63 lowercase letters, digits and dashes"
            )));
        }
        if out.iter().any(|o| o.name == name) {
            return Err(bad(format!("tenant pool {name} is named twice")));
        }
        if cluster.iter().any(|c| c.name == name) {
            return Err(bad(format!("{name} is a cluster pool's name")));
        }
        if !cluster.iter().any(|c| c.name == p.from) {
            return Err(bad(format!(
                "tenant pool {name}: no cluster pool {}",
                p.from
            )));
        }
        if !crate::api::vms::pool_tenant_grants(call, &p.from).contains(&t.id) {
            return Err(ServiceError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                format!(
                    "tenant pool {name}: the tenant isn't granted pool {}",
                    p.from
                ),
            ));
        }
        if p.max == 0 {
            return Err(bad(format!("tenant pool {name}: max must be at least 1")));
        }
        let mut projects = Vec::new();
        for key in &p.projects {
            // By id, or by name (unique in the tenant).
            let projects_store = m.projects();
            let pr = projects_store
                .get(key)
                .ok()
                .flatten()
                .or_else(|| {
                    projects_store
                        .list()
                        .ok()?
                        .into_iter()
                        .find(|pr| pr.name == *key && pr.tenant_id == t.id)
                })
                .filter(|pr| pr.tenant_id == t.id)
                .ok_or_else(|| {
                    bad(format!(
                        "tenant pool {name}: no project {key} in this tenant"
                    ))
                })?;
            if !projects.contains(&pr.id) {
                projects.push(pr.id);
            }
        }
        // Unchanged pools keep who set them and when.
        let old = call
            .auth()
            .tenants
            .device_pool(&t.id, name)
            .map_err(tenant_error)?;
        let (by, at) = match old {
            Some(o) if o.from == p.from && o.max == p.max && o.projects == projects => (o.by, o.at),
            _ => (by.clone(), at),
        };
        out.push(tenants::TenantPool {
            tenant_id: t.id.clone(),
            name: name.to_string(),
            from: p.from.clone(),
            max: p.max,
            projects,
            by,
            at,
        });
    }
    call.detail(
        "pools",
        serde_json::json!(out.iter().map(|p| &p.name).collect::<Vec<_>>()),
    );
    call.auth()
        .tenants
        .set_device_pools(&t.id, out)
        .map_err(tenant_error)?;
    let all = crate::service::node::cluster_pools(m).await;
    Ok(pb::SetDevicePoolsResponse {
        pools: tenant_pools_pb(call, m, &t.id, &all).await?,
        ..Default::default()
    })
}

// ---- offers to other directories' tenants (spec/tenancy.md D64) -------------

fn offer_pb(o: &tenants::Offer) -> pb::Offer {
    pb::Offer {
        id: o.id.clone(),
        tenant: o.tenant.clone(),
        directory: o.directory.clone(),
        cap: quotas_to_pb(&o.cap).into(),
        by: o.by.clone(),
        created_at: ts(o.created_at).into(),
        expires_at: ts(o.expires_at).into(),
        redeemed_at: o.redeemed_at.map(ts).into(),
        ..Default::default()
    }
}

/// Where directories reach this cluster's API (D64): `cluster.api_url`,
/// else the web UI's published host on the API's port.
pub fn offer_api(call: &Call) -> Option<String> {
    api_endpoint(&call.auth().config, call.manager())
}

/// This host's public API address: `cluster.api_url`, else the web UI's
/// host on the API's port.
pub fn api_endpoint(
    cfg: &crate::config::Config,
    manager: &crate::state::VmManager,
) -> Option<String> {
    if let Some(u) = cfg.cluster.ui_origin_of(cfg.cluster.api_url.as_deref()) {
        return Some(u);
    }
    let ui = manager.ui_url()?;
    let host = ui.parse::<axum::http::Uri>().ok()?.host()?.to_string();
    let port = cfg.listen.first().map_or(8841, |a| a.port());
    Some(format!("https://{host}:{port}"))
}

pub fn offer_admission(
    call: &Call,
    req: pb::OfferAdmissionRequest,
) -> ServiceResult<pb::OfferAdmissionResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let bad = |m: &str| ServiceError::new(StatusCode::BAD_REQUEST, "invalid", m);
    let tenant = req.tenant.trim().to_ascii_lowercase();
    tenants::validate_slug(&tenant).map_err(tenant_error)?;
    let directory = crate::trust::normalize_issuer(&req.directory)
        .ok_or_else(|| bad("directory is its issuer URL, https://…/d/<id>"))?;
    let days = req.days.map(u64::from).unwrap_or(crate::trust::OFFER_DAYS);
    if !(1..=crate::trust::OFFER_MAX_DAYS).contains(&days) {
        return Err(bad("an offer lasts 1 to 30 days"));
    }
    let api = offer_api(call)
        .ok_or_else(|| bad("this cluster has no public API address: set cluster.api_url"))?;
    let fp = crate::service::system::api_fingerprint(&call.app)
        .and_then(|f| glidex_tls::Trust::normalize_pin(&f))
        .ok_or_else(|| bad("this cluster's API has no TLS certificate to pin"))?;
    let now = crate::tenancy::now();
    let secret = crate::auth::random_token(32);
    let o = tenants::Offer {
        id: uuid::Uuid::new_v4().to_string(),
        tenant: tenant.clone(),
        directory: directory.clone(),
        cap: req
            .cap
            .as_option()
            .map(quotas_from_pb)
            .unwrap_or_else(tenants::unlimited),
        secret_hash: crate::auth::sha256_hex(secret.as_bytes()),
        by: call.p.user_id().unwrap_or("-").to_string(),
        created_at: now,
        expires_at: now + days * 86_400,
        redeemed_at: None,
    };
    call.auth().tenants.put_offer(&o).map_err(tenant_error)?;
    call.set_target(format!("offer:{}", o.id));
    call.detail("tenant", serde_json::json!(tenant));
    call.detail("directory", serde_json::json!(directory));
    let cluster_name = call
        .auth()
        .config
        .server_name
        .clone()
        .or_else(glidex_tls::hostname)
        .unwrap_or_else(|| "glidex".into());
    let token = crate::trust::OfferToken {
        cluster_id: crate::api::op::cluster_id(&call.app),
        cluster_name,
        api,
        tls_sha256: fp,
        tenant,
        directory,
        offer: o.id.clone(),
        secret,
        expires_at: o.expires_at,
    }
    .encode();
    Ok(pb::OfferAdmissionResponse {
        offer: offer_pb(&o).into(),
        token,
        ..Default::default()
    })
}

pub fn list_offers(
    call: &Call,
    _req: pb::ListOffersRequest,
) -> ServiceResult<pb::ListOffersResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let mut all = call.auth().tenants.offers().map_err(tenant_error)?;
    all.sort_by_key(|o| o.created_at);
    Ok(pb::ListOffersResponse {
        offers: all.iter().map(offer_pb).collect(),
        ..Default::default()
    })
}

pub fn revoke_offer(
    call: &Call,
    req: pb::RevokeOfferRequest,
) -> ServiceResult<pb::RevokeOfferResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    call.set_target(format!("offer:{}", req.id));
    if !call
        .auth()
        .tenants
        .remove_offer(&req.id)
        .map_err(tenant_error)?
    {
        return Err(ServiceError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such offer",
        ));
    }
    Ok(pb::RevokeOfferResponse::default())
}

// ---- trust with other organizations (spec/tenancy.md D65, D66) --------------

fn trust_err(e: crate::trust::StoreError) -> ServiceError {
    ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

/// Trusted cluster `c` as tenant `tenant` sees it, with what the cluster
/// last reported of it (D70).
fn tenant_cluster_pb(c: &crate::trust::TrustedCluster, tenant: &str) -> pb::TenantCluster {
    let report = c.report.as_ref().and_then(|r| {
        let t = r.tenants.iter().find(|t| t.tenant == tenant)?;
        Some(pb::TenantClusterReport {
            reported_at: ts(r.at).into(),
            status: if t.suspended {
                pb::AdmissionStatus::ADMISSION_STATUS_SUSPENDED
            } else {
                pb::AdmissionStatus::ADMISSION_STATUS_ADMITTED
            }
            .into(),
            cap: quotas_to_pb(&t.cap).into(),
            projects: t.projects,
            vms: t.vms,
            running_vms: t.running_vms,
            disks: t.disks,
            disk_gib: t.disk_gib,
            networks: t.networks,
            ..Default::default()
        })
    });
    pb::TenantCluster {
        cluster_id: c.cluster_id.clone(),
        name: c.name.clone(),
        api: c.api.clone(),
        created_at: ts(c.created_at).into(),
        report: report.into(),
        ..Default::default()
    }
}

pub fn list_tenant_clusters(
    call: &Call,
    req: pb::ListTenantClustersRequest,
) -> ServiceResult<pb::ListTenantClustersResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let all = call
        .auth()
        .trust
        .clusters(&t.directory_id)
        .map_err(trust_err)?;
    Ok(pb::ListTenantClustersResponse {
        clusters: all
            .iter()
            .filter(|c| c.tenants.contains(&t.id))
            .map(|c| tenant_cluster_pb(c, &t.id))
            .collect(),
        ..Default::default()
    })
}

/// Redeem a cluster's offer for tenant `id` (D65): this directory signs a
/// redeem to the cluster, pinning its certificate, and records the
/// cluster it answers for.
pub async fn opt_in_cluster(
    call: &Call,
    req: pb::OptInClusterRequest,
) -> ServiceResult<pb::OptInClusterResponse> {
    use crate::trust::{OfferToken, REDEEM, TrustedCluster, normalize_issuer};
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let bad = |m: String| ServiceError::new(StatusCode::BAD_REQUEST, "offer_refused", m);
    let token =
        OfferToken::decode(&req.offer).ok_or_else(|| bad("not an offer token (gxo_…)".into()))?;
    let now = crate::tenancy::now();
    if now >= token.expires_at {
        return Err(bad(
            "the offer expired: ask the cluster's operator for a new one".into(),
        ));
    }
    let dir = t.directory_id.clone();
    let d = call
        .auth()
        .tenants
        .get_directory(&dir)
        .map_err(tenant_error)?
        .ok_or_else(|| bad("no such directory".into()))?;
    let iss = crate::api::issuer::issuer_url(call.auth(), call.manager(), &d)
        .and_then(|i| normalize_issuer(&i))
        .ok_or_else(|| bad("this directory has no issuer URL yet: set auth.issuer_base".into()))?;
    if normalize_issuer(&token.directory).as_deref() != Some(iss.as_str()) {
        return Err(bad(format!(
            "the offer is for directory {}, not this one ({iss})",
            token.directory
        )));
    }
    if token.tenant != t.slug {
        return Err(bad(format!(
            "the offer is for tenant {}, not {}",
            token.tenant, t.slug
        )));
    }
    // The issuer is fixed from now: the cluster pins it.
    call.auth()
        .tenants
        .set_issuer(&dir, &iss, false)
        .map_err(tenant_error)?;
    let keys = &call.auth().connectors;
    keys.maintain_signing_keys(&dir, now).map_err(|e| {
        ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
    })?;
    let jwks = keys.jwks(&dir, now).map_err(|e| {
        ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
    })?;
    let api = offer_api(call)
        .ok_or_else(|| bad("this host has no public API address: set cluster.api_url".into()))?;
    let fp = crate::service::system::api_fingerprint(&call.app)
        .and_then(|f| glidex_tls::Trust::normalize_pin(&f))
        .ok_or_else(|| bad("this host's API has no TLS certificate to pin".into()))?;
    let assertion =
        crate::directory::peer::assertion(keys, &dir, &iss, &token.cluster_id, REDEEM, now)
            .map_err(|e| {
                ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
            })?;
    let body = serde_json::json!({
        "offer": token.offer, "secret": token.secret, "issuer": iss, "directory": dir,
        "tenant": { "id": t.id, "slug": t.slug, "display_name": t.display_name },
        "jwks": jwks, "api": api, "tls_sha256": fp,
    });
    let (status, answer) = crate::directory::peer::post(
        &token.api,
        &token.tls_sha256,
        "/trust/v1/redeem",
        &assertion,
        &body,
    )
    .await
    .map_err(|e| ServiceError::new(StatusCode::BAD_GATEWAY, "cluster_unreachable", e))?;
    if status != 200 || answer["cluster_id"].as_str() != Some(token.cluster_id.as_str()) {
        return Err(bad(
            "the cluster refused the offer (used, revoked, expired, or for someone else)".into(),
        ));
    }
    let mut c = call
        .auth()
        .trust
        .cluster(&dir, &token.cluster_id)
        .map_err(trust_err)?
        .unwrap_or_else(|| TrustedCluster {
            directory_id: dir.clone(),
            cluster_id: token.cluster_id.clone(),
            name: token.cluster_name.clone(),
            api: token.api.clone(),
            tls_sha256: token.tls_sha256.clone(),
            jwks: serde_json::json!({ "keys": [] }),
            tenants: Vec::new(),
            created_at: now,
            ui_callback: None,
            report: None,
        });
    c.api = token.api.clone();
    c.ui_callback = answer["ui_callback"]
        .as_str()
        .filter(|u| url::Url::parse(u).is_ok_and(|u| matches!(u.scheme(), "https" | "http")))
        .map(String::from);
    c.tls_sha256 = token.tls_sha256.clone();
    c.jwks = answer["cluster_key"].clone();
    if !c.tenants.contains(&t.id) {
        c.tenants.push(t.id.clone());
    }
    call.auth().trust.put_cluster(&c).map_err(trust_err)?;
    call.set_target(format!("tenant:{}", t.id));
    call.detail("cluster", serde_json::json!(c.cluster_id));
    Ok(pb::OptInClusterResponse {
        cluster: tenant_cluster_pb(&c, &t.id).into(),
        ..Default::default()
    })
}

/// Tenant `id` stops using a cluster (D66): forgotten here, and the
/// cluster told (best effort).
pub async fn opt_out_cluster(
    call: &Call,
    req: pb::OptOutClusterRequest,
) -> ServiceResult<pb::OptOutClusterResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let dir = t.directory_id.clone();
    let mut c = call
        .auth()
        .trust
        .cluster(&dir, &req.cluster)
        .map_err(trust_err)?
        .filter(|c| c.tenants.contains(&t.id))
        .ok_or_else(|| {
            ServiceError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "the tenant doesn't use that cluster",
            )
        })?;
    c.tenants.retain(|x| *x != t.id);
    if c.tenants.is_empty() {
        call.auth()
            .trust
            .remove_cluster(&dir, &c.cluster_id)
            .map_err(trust_err)?;
    } else {
        call.auth().trust.put_cluster(&c).map_err(trust_err)?;
    }
    call.set_target(format!("tenant:{}", t.id));
    call.detail("cluster", serde_json::json!(c.cluster_id));
    let iss = call
        .auth()
        .tenants
        .get_directory(&dir)
        .ok()
        .flatten()
        .and_then(|d| d.issuer);
    if let Some(iss) = iss
        && let Ok(a) = crate::directory::peer::assertion(
            &call.auth().connectors,
            &dir,
            &iss,
            &c.cluster_id,
            crate::trust::END,
            crate::tenancy::now(),
        )
        && let Err(e) = crate::directory::peer::post(
            &c.api,
            &c.tls_sha256,
            "/trust/v1/end",
            &a,
            &serde_json::json!({ "issuer": iss, "tenant": t.id }),
        )
        .await
    {
        tracing::warn!(cluster = %c.cluster_id, "telling the cluster trust ended: {}", e);
    }
    Ok(pb::OptOutClusterResponse::default())
}

pub fn list_trusted_directories(
    call: &Call,
    _req: pb::ListTrustedDirectoriesRequest,
) -> ServiceResult<pb::ListTrustedDirectoriesResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let all = call.auth().trust.directories().map_err(trust_err)?;
    Ok(pb::ListTrustedDirectoriesResponse {
        directories: all.iter().map(|d| trusted_directory_pb(call, d)).collect(),
        ..Default::default()
    })
}

/// A trusted directory with how fresh its replica is here (D13).
fn trusted_directory_pb(call: &Call, d: &crate::trust::TrustedDirectory) -> pb::TrustedDirectory {
    let now = crate::auth::store::now();
    let st = call.auth().trust.feed_state(&d.id).unwrap_or_default();
    let last = st.checked_at.max(st.applied_at);
    let lag = call.auth().trust.lag(d, now).unwrap_or(u64::MAX);
    let max = d
        .max_staleness_minutes
        .unwrap_or(call.auth().config.trust.default_max_staleness_minutes);
    pb::TrustedDirectory {
        id: d.id.clone(),
        issuer: d.issuer.clone(),
        tenants: d
            .tenants
            .iter()
            .map(|t| pb::RemoteTenant {
                id: t.id.clone(),
                slug: t.slug.clone(),
                display_name: t.display_name.clone(),
                ..Default::default()
            })
            .collect(),
        created_at: ts(d.created_at).into(),
        max_staleness_minutes: d.max_staleness_minutes,
        effective_max_staleness_minutes: max,
        last_sync: if last > 0 {
            ts(last).into()
        } else {
            Default::default()
        },
        lag_seconds: lag,
        stale: lag > max * 60,
        ..Default::default()
    }
}

pub fn set_trusted_directory(
    call: &Call,
    req: pb::SetTrustedDirectoryRequest,
) -> ServiceResult<pb::SetTrustedDirectoryResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    if req.max_staleness_minutes == Some(0) {
        return Err(ServiceError::new(
            StatusCode::BAD_REQUEST,
            "invalid",
            "max_staleness_minutes is at least 1",
        ));
    }
    let mut d = call
        .auth()
        .trust
        .directories()
        .map_err(trust_err)?
        .into_iter()
        .find(|d| d.id == req.id)
        .ok_or_else(|| {
            ServiceError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "no such trusted directory",
            )
        })?;
    d.max_staleness_minutes = req.max_staleness_minutes;
    call.auth().trust.put_directory(&d).map_err(trust_err)?;
    call.set_target(format!("directory:{}", d.issuer));
    call.detail(
        "max_staleness_minutes",
        serde_json::json!(d.max_staleness_minutes),
    );
    Ok(pb::SetTrustedDirectoryResponse {
        directory: trusted_directory_pb(call, &d).into(),
        ..Default::default()
    })
}
