//! `glidex.tenancy.v1`: tenants, their members and bindings, and this
//! cluster's admissions (spec/tenancy.md §14, T0). REST (`api/tenants.rs`)
//! and RPC (`api/rpc/tenancy.rs`) both call these functions and nothing
//! else.

use super::{Call, ServiceError, ServiceResult};
use crate::api::{auth_error, directory_ent};
use crate::authz::{Ent, EntitySet, TENANT_ROLES};
use crate::tenants::{self, AdmissionStatus, TenantError};
use axum::http::StatusCode;
use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::api::v1::EntityRef;
use glidex_api::proto::glidex::tenancy::v1 as pb;

const MAX_PAGE: usize = 500;
const DEFAULT_PAGE: usize = 100;

pub fn tenant_error(e: TenantError) -> ServiceError {
    match e {
        TenantError::NotFound(m) => ServiceError::new(StatusCode::NOT_FOUND, "not_found", m),
        TenantError::AlreadyExists(m) => ServiceError::new(StatusCode::CONFLICT, "conflict", format!("already exists: {m}")),
        TenantError::Invalid(m) => ServiceError::new(StatusCode::BAD_REQUEST, "invalid", m),
        TenantError::InUse(m) => ServiceError::new(StatusCode::CONFLICT, "tenant_in_use", m),
        TenantError::Storage(m) => ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "persistence_error", m),
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
    }
}

fn entity_ref(e: &Ent) -> EntityRef {
    EntityRef { r#type: e.type_name().into(), id: Some(e.id().into_owned()), ..Default::default() }
}

fn entity_from(r: &EntityRef) -> ServiceResult<Ent> {
    let ent = r.id.as_deref().and_then(|id| Ent::from_parts(&r.r#type, id));
    match ent {
        Some(e @ (Ent::User(_) | Ent::Team(_) | Ent::Token(_))) => Ok(e),
        _ => Err(ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "principal must be a User, Team or Token with an id")),
    }
}

/// `user:<id>` or `team:<id>`, as REST paths name a member.
pub fn parse_principal(key: &str) -> ServiceResult<EntityRef> {
    let (ty, id) = key.split_once(':').ok_or_else(|| ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "a principal is user:<id> or team:<id>"))?;
    let ty = match ty {
        "user" | "User" => "User",
        "team" | "Team" => "Team",
        _ => return Err(ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "a principal is user:<id> or team:<id>")),
    };
    Ok(EntityRef { r#type: ty.into(), id: Some(id.into()), ..Default::default() })
}

fn admission_pb(a: &tenants::Admission) -> pb::Admission {
    pb::Admission {
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
        admission: admission.as_ref().map(admission_pb).into(),
        ..Default::default()
    })
}

fn principal_exists(call: &Call, e: &Ent) -> ServiceResult<()> {
    let s = &call.auth().store;
    let ok = match e {
        Ent::User(id) => s.user(id).map_err(|e| auth_error(e.into()))?.is_some(),
        Ent::Team(id) => id.starts_with("unix:") || s.team(id).map_err(|e| auth_error(e.into()))?.is_some(),
        Ent::Token(id) => s.token_by_id(id).map_err(|e| auth_error(e.into()))?.is_some(),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(ServiceError::new(StatusCode::BAD_REQUEST, "invalid", format!("{e} doesn't exist")))
    }
}

/// A tenant the caller may read, by id or slug; `404` otherwise (as for
/// projects, security.md §7.4).
fn visible(call: &Call, key: &str) -> ServiceResult<tenants::Tenant> {
    let not_found = || ServiceError::new(StatusCode::NOT_FOUND, "not_found", "tenant not found");
    let t = call.auth().tenants.resolve(key).map_err(|_| not_found())?;
    call.set_target(format!("tenant:{}", t.id));
    call.require_visible("readTenant", Ent::Tenant(t.id.clone()), EntitySet::new(), "tenant")?;
    Ok(t)
}

// ---- tenants -------------------------------------------------------------

pub fn list_tenants(call: &Call, req: pb::ListTenantsRequest) -> ServiceResult<pb::ListTenantsResponse> {
    let all = call.auth().tenants.list().map_err(tenant_error)?;
    let everyone = call.allowed("listTenants", directory_ent(), EntitySet::new());
    let visible: Vec<_> = all.into_iter().filter(|t| everyone || call.allowed("readTenant", Ent::Tenant(t.id.clone()), EntitySet::new())).collect();
    let size = req.page_size.map(|n| n as usize).filter(|n| *n > 0).unwrap_or(DEFAULT_PAGE).min(MAX_PAGE);
    let start: usize = if req.page_token.is_empty() {
        0
    } else {
        req.page_token.parse().map_err(|_| ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "bad page_token"))?
    };
    let page: Vec<_> = visible.iter().skip(start).take(size).collect();
    let next = if start + page.len() < visible.len() { (start + page.len()).to_string() } else { String::new() };
    Ok(pb::ListTenantsResponse {
        tenants: page.into_iter().map(|t| tenant_pb(call, t)).collect::<ServiceResult<_>>()?,
        next_page_token: next,
        ..Default::default()
    })
}

pub fn get_tenant(call: &Call, req: pb::GetTenantRequest) -> ServiceResult<pb::GetTenantResponse> {
    let t = visible(call, &req.id)?;
    Ok(pb::GetTenantResponse { tenant: tenant_pb(call, &t)?.into(), ..Default::default() })
}

pub fn create_tenant(call: &Call, req: pb::CreateTenantRequest) -> ServiceResult<pb::CreateTenantResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let t = call.auth().tenants.create(&req.slug, req.display_name, req.operator.unwrap_or(false)).map_err(tenant_error)?;
    call.set_target(format!("tenant:{}", t.id));
    call.detail("slug", serde_json::json!(t.slug));
    Ok(pb::CreateTenantResponse { tenant: tenant_pb(call, &t)?.into(), ..Default::default() })
}

pub fn update_tenant(call: &Call, req: pb::UpdateTenantRequest) -> ServiceResult<pb::UpdateTenantResponse> {
    let mut t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    if let Some(name) = req.display_name {
        t.display_name = name;
    }
    if let Some(op) = req.operator {
        if !op && t.id == call.auth().tenants.operator().map_err(tenant_error)? {
            return Err(ServiceError::new(StatusCode::CONFLICT, "conflict", "this tenant operates this cluster"));
        }
        t.operator = op;
    }
    call.auth().tenants.update(&t).map_err(tenant_error)?;
    Ok(pb::UpdateTenantResponse { tenant: tenant_pb(call, &t)?.into(), ..Default::default() })
}

fn set_suspended(call: &Call, key: &str, suspended: bool) -> ServiceResult<pb::Tenant> {
    let mut t = visible(call, key)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    if t.owns_directory || t.id == call.auth().tenants.operator().map_err(tenant_error)? {
        return Err(ServiceError::new(StatusCode::CONFLICT, "conflict", "the directory's owner and the cluster's operator can't be suspended"));
    }
    t.suspended = suspended;
    call.auth().tenants.update(&t).map_err(tenant_error)?;
    tenant_pb(call, &t)
}

pub fn suspend_tenant(call: &Call, req: pb::SuspendTenantRequest) -> ServiceResult<pb::SuspendTenantResponse> {
    Ok(pb::SuspendTenantResponse { tenant: set_suspended(call, &req.id, true)?.into(), ..Default::default() })
}

pub fn resume_tenant(call: &Call, req: pb::ResumeTenantRequest) -> ServiceResult<pb::ResumeTenantResponse> {
    Ok(pb::ResumeTenantResponse { tenant: set_suspended(call, &req.id, false)?.into(), ..Default::default() })
}

pub fn delete_tenant(call: &Call, req: pb::DeleteTenantRequest) -> ServiceResult<pb::DeleteTenantResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let projects = call
        .manager()
        .projects()
        .list()
        .map_err(|e| ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "persistence_error", e.to_string()))?;
    if let Some(p) = projects.iter().find(|p| p.tenant_id == t.id) {
        return Err(ServiceError::new(StatusCode::CONFLICT, "tenant_in_use", format!("tenant {} still has project {}", t.slug, p.name)));
    }
    call.auth().tenants.delete(&t.id).map_err(tenant_error)?;
    call.auth().forget_entity(&Ent::Tenant(t.id.clone())).map_err(auth_error)?;
    Ok(pb::DeleteTenantResponse::default())
}

// ---- members --------------------------------------------------------------

fn member_pb(m: &tenants::Member) -> pb::Member {
    pb::Member { tenant_id: m.tenant_id.clone(), principal: entity_ref(&m.principal).into(), by: m.by.clone(), at: ts(m.at).into(), ..Default::default() }
}

pub fn list_members(call: &Call, req: pb::ListMembersRequest) -> ServiceResult<pb::ListMembersResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let members = call.auth().tenants.members(&t.id).map_err(tenant_error)?;
    Ok(pb::ListMembersResponse { members: members.iter().map(member_pb).collect(), ..Default::default() })
}

pub fn add_member(call: &Call, req: pb::AddMemberRequest) -> ServiceResult<pb::AddMemberResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let principal = entity_from(req.principal.as_option().ok_or_else(|| ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "principal is required"))?)?;
    principal_exists(call, &principal)?;
    call.detail("principal", serde_json::json!(principal));
    let by = call.p.user_id().unwrap_or("-").to_string();
    let m = call.auth().tenants.add_member(&t.id, principal, &by).map_err(tenant_error)?;
    Ok(pb::AddMemberResponse { member: member_pb(&m).into(), ..Default::default() })
}

pub fn remove_member(call: &Call, req: pb::RemoveMemberRequest) -> ServiceResult<pb::RemoveMemberResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let principal = entity_from(&parse_principal(&req.principal)?)?;
    call.detail("principal", serde_json::json!(principal));
    call.auth().tenants.remove_member(&t.id, &principal).map_err(tenant_error)?;
    Ok(pb::RemoveMemberResponse::default())
}

// ---- bindings (tenant roles) ---------------------------------------------

fn binding_pb(call: &Call, l: &crate::auth::store::LinkRecord) -> ServiceResult<pb::Binding> {
    let s = &call.auth().store;
    let name = match &l.link.principal {
        Ent::User(id) => s.user(id).map_err(|e| auth_error(e.into()))?.map(|u| u.display_name),
        Ent::Team(id) => s.team(id).map_err(|e| auth_error(e.into()))?.map(|t| t.name),
        Ent::Token(id) => s.token_by_id(id).map_err(|e| auth_error(e.into()))?.map(|t| t.1.name),
        _ => None,
    };
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

pub fn list_bindings(call: &Call, req: pb::ListBindingsRequest) -> ServiceResult<pb::ListBindingsResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let mine = Ent::Tenant(t.id.clone());
    let links = call.auth().store.links().map_err(|e| auth_error(e.into()))?;
    Ok(pb::ListBindingsResponse {
        bindings: links.iter().filter(|l| l.link.resource == mine).map(|l| binding_pb(call, l)).collect::<ServiceResult<_>>()?,
        ..Default::default()
    })
}

pub fn add_binding(call: &Call, req: pb::AddBindingRequest) -> ServiceResult<pb::AddBindingResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    if !TENANT_ROLES.contains(&req.role.as_str()) {
        return Err(ServiceError::new(StatusCode::BAD_REQUEST, "invalid", format!("tenant roles are {}", TENANT_ROLES.join(", "))));
    }
    let principal = entity_from(req.principal.as_option().ok_or_else(|| ServiceError::new(StatusCode::BAD_REQUEST, "invalid", "principal is required"))?)?;
    principal_exists(call, &principal)?;
    call.detail("role", serde_json::json!(req.role));
    call.detail("principal", serde_json::json!(principal));
    let by = call.p.user_id().unwrap_or("-").to_string();
    let l = call.auth().add_link(&req.role, principal, Ent::Tenant(t.id.clone()), &by).map_err(auth_error)?;
    Ok(pb::AddBindingResponse { binding: binding_pb(call, &l)?.into(), ..Default::default() })
}

pub fn remove_binding(call: &Call, req: pb::RemoveBindingRequest) -> ServiceResult<pb::RemoveBindingResponse> {
    let t = visible(call, &req.id)?;
    call.require(Ent::Tenant(t.id.clone()), EntitySet::new())?;
    let links = call.auth().store.links().map_err(|e| auth_error(e.into()))?;
    match links.into_iter().find(|l| l.link.id == req.link) {
        Some(l) if l.link.resource == Ent::Tenant(t.id.clone()) => {
            call.detail("link", serde_json::json!(l.link));
            call.auth().remove_link(&req.link).map_err(auth_error)?;
            Ok(pb::RemoveBindingResponse::default())
        }
        _ => Err(ServiceError::new(StatusCode::NOT_FOUND, "not_found", "no such binding on this tenant")),
    }
}

// ---- admissions (the cluster's operator) ----------------------------------

pub fn list_admissions(call: &Call, _req: pb::ListAdmissionsRequest) -> ServiceResult<pb::ListAdmissionsResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let all = call.auth().tenants.admissions().map_err(tenant_error)?;
    Ok(pb::ListAdmissionsResponse { admissions: all.iter().map(admission_pb).collect(), ..Default::default() })
}

pub fn set_admission(call: &Call, req: pb::SetAdmissionRequest) -> ServiceResult<pb::SetAdmissionResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let store = &call.auth().tenants;
    let t = store.resolve(&req.tenant).map_err(tenant_error)?;
    call.set_target(format!("tenant:{}", t.id));
    let current = store.admission(&t.id).map_err(tenant_error)?;
    let cap = match req.cap.as_option() {
        Some(q) => quotas_from_pb(q),
        None => current.as_ref().map(|a| a.cap.clone()).unwrap_or_else(tenants::unlimited),
    };
    let status = match (req.suspended, &current) {
        (Some(true), _) => AdmissionStatus::Suspended,
        (Some(false), _) | (None, None) => AdmissionStatus::Admitted,
        (None, Some(a)) => a.status,
    };
    call.detail("cap", serde_json::json!(cap));
    let by = call.p.user_id().unwrap_or("-").to_string();
    let a = store.admit(&t.id, cap, status, &by).map_err(tenant_error)?;
    Ok(pb::SetAdmissionResponse { admission: admission_pb(&a).into(), ..Default::default() })
}

pub fn remove_admission(call: &Call, req: pb::RemoveAdmissionRequest) -> ServiceResult<pb::RemoveAdmissionResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let store = &call.auth().tenants;
    let t = store.resolve(&req.tenant).map_err(tenant_error)?;
    call.set_target(format!("tenant:{}", t.id));
    if t.id == store.operator().map_err(tenant_error)? {
        return Err(ServiceError::new(StatusCode::CONFLICT, "conflict", "the cluster's operator stays admitted"));
    }
    store.remove_admission(&t.id).map_err(tenant_error)?;
    Ok(pb::RemoveAdmissionResponse::default())
}
