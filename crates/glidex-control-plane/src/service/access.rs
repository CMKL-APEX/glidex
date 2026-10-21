//! `glidex.access.v1` (spec/security.md §6, §7, §10, §13): projects and
//! their role bindings, users, teams, API tokens, host role bindings, site
//! policies, the audit log and capability checks. The REST twins in
//! `api/access.rs` and the RPC adapter in `api/rpc/access.rs` call these.

use super::common::{entity_of, entity_ref, principal_of, ts, ts_opt};
use super::{Call, ServiceError, ServiceResult};
use crate::api::{
    add_network, auth_error, credential_entities, directory_ent, disk_entities, project_entities,
    vm_entities,
};
use crate::auth::store::{
    DisabledReason, Identity, MemberSource, StoreError, Team, TeamMember, TokenKind, User,
};
use crate::auth::{self, AuthError, Method};
use crate::authz::{self, Ent, EntitySet, SiteSource};
use crate::service::tenancy::{quotas_from_pb as quotas_of, quotas_to_pb};
use axum::http::StatusCode;
use glidex_api::proto::glidex::access::v1 as pb;

fn store_err(e: StoreError) -> ServiceError {
    auth_error(AuthError::Store(e)).into()
}

/// §8.3: a field its connector owns.
fn managed_conflict(what: &str, connector: &str) -> ServiceError {
    ServiceError::new(
        StatusCode::CONFLICT,
        "managed_by_connector",
        format!("{what} is provisioned by connector {connector}: change it at the IdP"),
    )
}

fn node_err(e: crate::node::NodeError) -> ServiceError {
    ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
}

fn manager_err(e: crate::state::VmManagerError) -> ServiceError {
    crate::api::error_to_response(e).into()
}

fn bad(m: impl Into<String>) -> ServiceError {
    ServiceError::new(StatusCode::BAD_REQUEST, "invalid", m)
}

fn not_found(what: &str) -> ServiceError {
    ServiceError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("{what} not found"),
    )
}

fn mask_of(m: Option<&buffa_types::google::protobuf::FieldMask>) -> Vec<String> {
    m.map(|m| m.paths.clone()).unwrap_or_default()
}

/// A JSON value as a `google.protobuf.Struct`: objects only (an empty one
/// for anything else).
fn struct_of(v: &serde_json::Value) -> Option<buffa_types::google::protobuf::Struct> {
    match v {
        serde_json::Value::Object(_) => serde_json::from_value(v.clone()).ok(),
        _ => None,
    }
}

// ---- conversions ----------------------------------------------------------

pub fn user_pb(u: &User, identities: &[Identity]) -> pb::User {
    pb::User {
        id: u.id.clone(),
        display_name: u.display_name.clone(),
        disabled: Some(u.disabled),
        default_project: u.default_project.clone(),
        created_at: ts(u.created_at).into(),
        managed_by: u.managed.as_ref().map(|m| m.connector.clone()),
        external_id: u.managed.as_ref().and_then(|m| m.external_id.clone()),
        email: u.email.clone(),
        disabled_reason: u.disabled_reason.map(|r| r.as_str().into()),
        identities: identities.iter().map(identity_pb).collect(),
        ..Default::default()
    }
}

fn identity_pb(i: &Identity) -> pb::Identity {
    pb::Identity {
        provider: i.provider.clone(),
        subject: i.subject.clone(),
        user_id: i.user_id.clone(),
        email: i.email.clone(),
        created_at: ts(i.created_at).into(),
        ..Default::default()
    }
}

fn team_pb(t: &Team) -> pb::Team {
    pb::Team {
        id: t.id.clone(),
        name: t.name.clone(),
        members: t
            .members
            .iter()
            .map(|m| pb::TeamMember {
                user_id: m.user_id.clone(),
                source: match &m.source {
                    MemberSource::Manual => "manual".into(),
                    MemberSource::Pam => "pam".into(),
                    MemberSource::Oidc => "oidc".into(),
                    MemberSource::Connector(id) => format!("connector:{id}"),
                },
                ..Default::default()
            })
            .collect(),
        created_at: ts(t.created_at).into(),
        managed_by: t.managed.as_ref().map(|m| m.connector.clone()),
        external_id: t.managed.as_ref().and_then(|m| m.external_id.clone()),
        ..Default::default()
    }
}

/// A project's quotas from the request. As before the port, a project
/// network, router or external address limit left out gets the default
/// (2, 1, 1), not unlimited; the others left out are unlimited.
fn project_quotas(q: &glidex_api::proto::glidex::tenancy::v1::Quotas) -> crate::tenancy::Quotas {
    let defaults = crate::tenancy::Quotas::new_project();
    let mut out = quotas_of(q);
    out.networks = out.networks.or(defaults.networks);
    out.routers = out.routers.or(defaults.routers);
    out.external_ips = out.external_ips.or(defaults.external_ips);
    out
}

fn usage_pb(u: &crate::tenancy::Usage) -> pb::ProjectUsage {
    let c = |v: u64| v.min(u64::from(u32::MAX)) as u32;
    pb::ProjectUsage {
        vms: c(u.vms),
        vcpus: c(u.vcpus),
        memory_mib: c(u.memory_mib),
        disk_gib: c(u.disk_gib),
        running_vms: c(u.running_vms),
        networks: c(u.networks),
        routers: c(u.routers),
        external_ips: c(u.external_ips),
        devices: u.devices.iter().map(|(k, &v)| (k.clone(), c(v))).collect(),
        ..Default::default()
    }
}

fn project_pb(p: &crate::tenancy::Project, usage: Option<&crate::tenancy::Usage>) -> pb::Project {
    pb::Project {
        id: p.id.clone(),
        name: p.name.clone(),
        tenant_id: p.tenant_id.clone(),
        description: p.description.clone(),
        quotas: quotas_to_pb(&p.quotas).into(),
        created_at: ts(p.created_at).into(),
        usage: usage.map(usage_pb).into(),
        ..Default::default()
    }
}

/// Role links with their principals' names, so whoever may read the links
/// sees who they are without listing users or teams (host rights).
fn bindings_pb(
    call: &Call,
    links: Vec<auth::store::LinkRecord>,
) -> ServiceResult<Vec<pb::Binding>> {
    let s = &call.auth().store;
    let mut tokens: Option<Vec<(String, auth::store::Token)>> = None;
    links
        .into_iter()
        .map(|record| {
            let name = match &record.link.principal {
                Ent::User(id) => s.user(id).map_err(store_err)?.map(|u| u.display_name),
                Ent::Team(id) if !id.starts_with("unix:") => {
                    s.team(id).map_err(store_err)?.map(|t| t.name)
                }
                Ent::Token(id) => {
                    if tokens.is_none() {
                        tokens = Some(s.tokens().map_err(store_err)?);
                    }
                    tokens
                        .as_ref()
                        .and_then(|ts| ts.iter().find(|(_, t)| &t.id == id))
                        .map(|(_, t)| t.name.clone())
                }
                _ => None,
            };
            Ok(binding_pb(&record, name))
        })
        .collect()
}

fn binding_pb(l: &auth::store::LinkRecord, principal_name: Option<String>) -> pb::Binding {
    pb::Binding {
        id: l.link.id.clone(),
        role: l.link.template.clone(),
        principal: entity_ref(&l.link.principal).into(),
        principal_name,
        resource: entity_ref(&l.link.resource).into(),
        created_by: l.created_by.clone(),
        created_at: ts(l.created_at).into(),
        ..Default::default()
    }
}

fn token_pb(t: &auth::store::Token, roles: &[auth::store::LinkRecord]) -> pb::Token {
    let (kind, owner, project) = match &t.kind {
        TokenKind::Personal { owner } => (
            pb::TokenKind::TOKEN_KIND_PERSONAL,
            Some(owner.clone()),
            None,
        ),
        TokenKind::ServiceAccount { project } => (
            pb::TokenKind::TOKEN_KIND_SERVICE_ACCOUNT,
            None,
            Some(project.clone()),
        ),
    };
    pb::Token {
        id: t.id.clone(),
        name: t.name.clone(),
        kind: kind.into(),
        owner,
        project,
        created_by: t.created_by.clone(),
        created_at: ts(t.created_at).into(),
        expires_at: ts_opt(t.expires_at).into(),
        last_used_at: t.last_used_at.and_then(ts_opt).into(),
        last_used_from: t.last_used_from.clone(),
        device: t.device.clone(),
        client: t.client.clone(),
        roles: roles.iter().map(|l| binding_pb(l, None)).collect(),
        ..Default::default()
    }
}

fn source_pb(s: authz::PolicySource) -> pb::PolicySource {
    use authz::PolicySource as S;
    match s {
        S::Base => pb::PolicySource::POLICY_SOURCE_BASE,
        S::Role => pb::PolicySource::POLICY_SOURCE_ROLE,
        S::Link => pb::PolicySource::POLICY_SOURCE_LINK,
        S::Site => pb::PolicySource::POLICY_SOURCE_SITE,
        S::File => pb::PolicySource::POLICY_SOURCE_FILE,
    }
}

fn loaded_pb(p: &authz::PolicyInfo) -> pb::LoadedPolicy {
    pb::LoadedPolicy {
        id: p.id.clone(),
        source: source_pb(p.source).into(),
        template: Some(p.template),
        text: p.text.clone(),
        ..Default::default()
    }
}

fn site_pb(p: &auth::store::SitePolicy) -> pb::SitePolicy {
    pb::SitePolicy {
        id: p.id.clone(),
        text: p.text.clone(),
        description: p.description.clone(),
        enabled: Some(p.enabled),
        version: p.version,
        updated_by: p.updated_by.clone(),
        updated_at: ts(p.updated_at).into(),
        ..Default::default()
    }
}

fn decision_pb(d: &authz::Decision) -> pb::Decision {
    pb::Decision {
        allowed: Some(d.allowed),
        policies: d.policies.clone(),
        errors: d.errors.clone(),
        ..Default::default()
    }
}

fn audit_pb(e: &auth::store::AuditEntry) -> pb::AuditEntry {
    let ms = e.time;
    pb::AuditEntry {
        time: buffa_types::google::protobuf::Timestamp {
            seconds: (ms / 1000) as i64,
            nanos: ((ms % 1000) * 1_000_000) as i32,
            ..Default::default()
        }
        .into(),
        request_id: e.request_id.clone(),
        principal: struct_of(&e.principal).into(),
        source: e.source.clone(),
        action: e.action.clone(),
        project: e.project.clone(),
        target: e.target.clone(),
        result: e.result.clone(),
        error_code: e.error_code.clone(),
        policies: e.policies.clone(),
        details: struct_of(&e.details).into(),
        ..Default::default()
    }
}

// ---- projects -------------------------------------------------------------

pub async fn list_projects(
    call: &Call,
    _req: pb::ListProjectsRequest,
) -> ServiceResult<pb::ListProjectsResponse> {
    let visible = call.visible_projects()?;
    let mut projects = Vec::new();
    for p in call
        .manager()
        .projects()
        .list()
        .map_err(|e| manager_err(e.into()))?
    {
        if visible.contains(&p.id)
            && call.allowed(
                "readProject",
                Ent::Project(p.id.clone()),
                project_entities(&p.id),
            )
        {
            let usage = call.manager().usage(&p.id).await.map_err(manager_err)?;
            projects.push(project_pb(&p, Some(&usage)));
        }
    }
    Ok(pb::ListProjectsResponse {
        projects,
        ..Default::default()
    })
}

pub fn create_project(
    call: &Call,
    req: pb::CreateProjectRequest,
) -> ServiceResult<pb::CreateProjectResponse> {
    let key = req
        .tenant
        .as_deref()
        .unwrap_or(crate::tenants::DEFAULT_TENANT);
    let tenant = call
        .auth()
        .tenants
        .resolve(key)
        .map_err(|_| not_found("tenant"))?;
    call.require(Ent::Tenant(tenant.id.clone()), EntitySet::new())?;
    // Without explicit quotas a project gets the site default (§6.3).
    let quotas = match req.quotas.as_option() {
        Some(q) => project_quotas(q),
        None => call.auth().config.quotas.default.clone(),
    };
    let p = call
        .manager()
        .projects()
        .create_in(&tenant.id, &req.name, req.description, Some(quotas))
        .map_err(|e| manager_err(e.into()))?;
    call.set_project(&p.id);
    call.set_target(format!("project:{}", p.id));
    let by = call.p.user_id().unwrap_or("-").to_string();
    for o in req.owners {
        call.auth()
            .add_link("role.owner", Ent::User(o), Ent::Project(p.id.clone()), &by)
            .map_err(|e| ServiceError::from(auth_error(e)))?;
    }
    Ok(pb::CreateProjectResponse {
        project: project_pb(&p, None).into(),
        ..Default::default()
    })
}

fn visible_project(call: &Call, key: &str) -> ServiceResult<crate::tenancy::Project> {
    let p = call
        .manager()
        .projects()
        .resolve(key)
        .map_err(|_| not_found("project"))?;
    call.set_project(&p.id);
    call.set_target(format!("project:{}", p.id));
    call.require_visible(
        "readProject",
        Ent::Project(p.id.clone()),
        project_entities(&p.id),
        "project",
    )?;
    Ok(p)
}

pub async fn get_project(
    call: &Call,
    req: pb::GetProjectRequest,
) -> ServiceResult<pb::GetProjectResponse> {
    let p = visible_project(call, &req.id)?;
    let usage = call.manager().usage(&p.id).await.map_err(manager_err)?;
    Ok(pb::GetProjectResponse {
        project: project_pb(&p, Some(&usage)).into(),
        ..Default::default()
    })
}

pub fn update_project(
    call: &Call,
    req: pb::UpdateProjectRequest,
) -> ServiceResult<pb::UpdateProjectResponse> {
    let mut p = visible_project(call, &req.id)?;
    for path in mask_of(req.update_mask.as_option()) {
        match path.as_str() {
            "description" => p.description = req.description.clone(),
            "quotas" => {
                let q = req
                    .quotas
                    .as_option()
                    .map(project_quotas)
                    .unwrap_or_default();
                call.detail("quotas", serde_json::json!(q));
                p.quotas = q;
            }
            other => return Err(bad(format!("can't change {other}: description or quotas"))),
        }
    }
    call.manager()
        .projects()
        .put(&p)
        .map_err(|e| manager_err(e.into()))?;
    Ok(pb::UpdateProjectResponse {
        project: project_pb(&p, None).into(),
        ..Default::default()
    })
}

pub async fn delete_project(
    call: &Call,
    req: pb::DeleteProjectRequest,
) -> ServiceResult<pb::DeleteProjectResponse> {
    let p = visible_project(call, &req.id)?;
    if let Some(what) = call
        .manager()
        .project_in_use(&p.id)
        .await
        .map_err(manager_err)?
    {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "conflict",
            format!("project still owns {}", what),
        ));
    }
    call.manager()
        .projects()
        .delete(&p.id)
        .map_err(|e| manager_err(e.into()))?;
    call.manager().forget_project(&p.id).map_err(manager_err)?;
    call.auth()
        .forget_entity(&Ent::Project(p.id.clone()))
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    // Service accounts of the project go with it.
    for (_, t) in call.auth().store.tokens().map_err(store_err)? {
        if matches!(&t.kind, TokenKind::ServiceAccount { project } if *project == p.id) {
            call.auth()
                .revoke_token(&t.id)
                .map_err(|e| ServiceError::from(auth_error(e)))?;
        }
    }
    Ok(pb::DeleteProjectResponse::default())
}

fn check_principal_exists(call: &Call, e: &Ent) -> ServiceResult<()> {
    let s = &call.auth().store;
    let ok = match e {
        Ent::User(id) => s.user(id).map_err(store_err)?.is_some(),
        Ent::Team(id) => id.starts_with("unix:") || s.team(id).map_err(store_err)?.is_some(),
        Ent::Token(id) => s.token_by_id(id).map_err(store_err)?.is_some(),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(bad(format!("{} doesn't exist", e)))
    }
}

pub fn list_project_bindings(
    call: &Call,
    req: pb::ListProjectBindingsRequest,
) -> ServiceResult<pb::ListProjectBindingsResponse> {
    let p = visible_project(call, &req.id)?;
    let links: Vec<_> = call
        .auth()
        .store
        .links()
        .map_err(store_err)?
        .into_iter()
        .filter(|l| l.link.resource == Ent::Project(p.id.clone()))
        .collect();
    Ok(pb::ListProjectBindingsResponse {
        bindings: bindings_pb(call, links)?,
        ..Default::default()
    })
}

pub fn add_project_binding(
    call: &Call,
    req: pb::AddProjectBindingRequest,
) -> ServiceResult<pb::AddProjectBindingResponse> {
    let p = visible_project(call, &req.id)?;
    if !authz::PROJECT_ROLES.contains(&req.role.as_str()) {
        return Err(bad("only project roles can be given in a project"));
    }
    let principal = principal_of(
        req.principal
            .as_option()
            .ok_or_else(|| bad("principal is required"))?,
    )?;
    check_principal_exists(call, &principal)?;
    call.detail("role", serde_json::json!(req.role));
    call.detail("principal", serde_json::json!(principal));
    let l = call
        .auth()
        .add_link(
            &req.role,
            principal,
            Ent::Project(p.id),
            call.p.user_id().unwrap_or("-"),
        )
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    Ok(pb::AddProjectBindingResponse {
        binding: bindings_pb(call, vec![l])?.pop().into(),
        ..Default::default()
    })
}

pub fn remove_project_binding(
    call: &Call,
    req: pb::RemoveProjectBindingRequest,
) -> ServiceResult<pb::RemoveProjectBindingResponse> {
    let p = visible_project(call, &req.id)?;
    let found = call
        .auth()
        .store
        .links()
        .map_err(store_err)?
        .into_iter()
        .find(|l| l.link.id == req.link);
    match found {
        Some(l) if l.link.resource == Ent::Project(p.id.clone()) => {
            call.detail("link", serde_json::json!(l.link));
            call.auth()
                .remove_link(&req.link)
                .map_err(|e| ServiceError::from(auth_error(e)))?;
            Ok(pb::RemoveProjectBindingResponse::default())
        }
        _ => Err(not_found("binding")),
    }
}

// ---- host role bindings ---------------------------------------------------

pub fn list_system_bindings(
    call: &Call,
    req: pb::ListSystemBindingsRequest,
) -> ServiceResult<pb::ListSystemBindingsResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let only = match req.node {
        Some(key) => Some(Ent::Node(binding_node(call, &key)?)),
        None => None,
    };
    let links: Vec<_> = call
        .auth()
        .store
        .links()
        .map_err(store_err)?
        .into_iter()
        .filter(|l| match &only {
            Some(host) => &l.link.resource == host,
            None => is_system_resource(&l.link.resource),
        })
        .collect();
    Ok(pb::ListSystemBindingsResponse {
        bindings: bindings_pb(call, links)?,
        ..Default::default()
    })
}

/// Where system bindings live: the cluster, or one node's host.
fn is_system_resource(e: &Ent) -> bool {
    matches!(e, Ent::Cluster | Ent::Node(_))
}

/// The id of the node `key` names (id or name) for a host binding. A node
/// that has left can still be named, to list or clear what it had.
fn binding_node(call: &Call, key: &str) -> ServiceResult<String> {
    if key == authz::node_id() {
        return Ok(key.to_string());
    }
    let nodes = call.manager().nodes().list().map_err(node_err)?;
    crate::cluster::membership::resolve(&nodes, key)
        .map(|n| n.meta.id)
        .map_err(|_| not_found("node"))
}

pub fn add_system_binding(
    call: &Call,
    req: pb::AddSystemBindingRequest,
) -> ServiceResult<pb::AddSystemBindingResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    if !authz::HOST_ROLES.contains(&req.role.as_str()) {
        return Err(bad("only host roles and grants are given on the host"));
    }
    let resource = match &req.node {
        None => Ent::Cluster,
        Some(key) => {
            if !authz::PER_HOST_ROLES.contains(&req.role.as_str()) {
                return Err(bad(format!(
                    "{} is cluster-wide: only {} can be given on one host",
                    req.role,
                    authz::PER_HOST_ROLES.join(", ")
                )));
            }
            let id = binding_node(call, key)?;
            let live = call
                .manager()
                .nodes()
                .get(&id)
                .map_err(node_err)?
                .is_none_or(|n| !n.status.phase.is_tombstone());
            if !live {
                return Err(ServiceError::new(
                    StatusCode::CONFLICT,
                    "node_departed",
                    format!("node {key} has left the cluster"),
                ));
            }
            Ent::Node(id)
        }
    };
    let principal = principal_of(
        req.principal
            .as_option()
            .ok_or_else(|| bad("principal is required"))?,
    )?;
    check_principal_exists(call, &principal)?;
    // Host roles go to the operator's people only (spec/tenancy.md D4).
    let operator = call
        .auth()
        .tenants
        .operator()
        .map_err(|e| ServiceError::from(auth_error(crate::auth::tenant_err(e))))?;
    if !call
        .auth()
        .is_member(&principal, &operator)
        .map_err(|e| ServiceError::from(auth_error(e)))?
    {
        return Err(ServiceError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_operator_member",
            format!(
                "{} isn't a member of tenant {operator}, which operates this cluster: add them there first",
                principal
            ),
        ));
    }
    call.detail("role", serde_json::json!(req.role));
    call.detail("principal", serde_json::json!(principal));
    call.detail("resource", serde_json::json!(resource));
    let l = call
        .auth()
        .add_link(
            &req.role,
            principal,
            resource,
            call.p.user_id().unwrap_or("-"),
        )
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    Ok(pb::AddSystemBindingResponse {
        binding: bindings_pb(call, vec![l])?.pop().into(),
        ..Default::default()
    })
}

pub fn remove_system_binding(
    call: &Call,
    req: pb::RemoveSystemBindingRequest,
) -> ServiceResult<pb::RemoveSystemBindingResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let found = call
        .auth()
        .store
        .links()
        .map_err(store_err)?
        .into_iter()
        .find(|l| l.link.id == req.link);
    match found {
        Some(l) if is_system_resource(&l.link.resource) => {
            call.detail("link", serde_json::json!(l.link));
            call.auth()
                .remove_link(&req.link)
                .map_err(|e| ServiceError::from(auth_error(e)))?;
            Ok(pb::RemoveSystemBindingResponse::default())
        }
        _ => Err(not_found("binding")),
    }
}

// ---- users ----------------------------------------------------------------

pub fn list_users(call: &Call, _req: pb::ListUsersRequest) -> ServiceResult<pb::ListUsersResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let s = &call.auth().store;
    let users = s
        .users()
        .map_err(store_err)?
        .iter()
        .map(|u| user_pb(u, &s.identities_of(&u.id).unwrap_or_default()))
        .collect();
    Ok(pb::ListUsersResponse {
        users,
        ..Default::default()
    })
}

fn valid_provider(p: &str) -> bool {
    p == "pam" || p == "unix" || p.starts_with("oidc:")
}

pub fn create_user(
    call: &Call,
    req: pb::CreateUserRequest,
) -> ServiceResult<pb::CreateUserResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let s = &call.auth().store;
    for i in &req.identities {
        if !valid_provider(&i.provider) || i.subject.is_empty() {
            return Err(bad(
                "identities are pam:, unix: or oidc:<issuer> with a subject",
            ));
        }
        if s.identity(&i.provider, &i.subject)
            .map_err(store_err)?
            .is_some()
        {
            return Err(ServiceError::new(
                StatusCode::CONFLICT,
                "conflict",
                format!("identity {}:{} is already linked", i.provider, i.subject),
            ));
        }
    }
    let u = User {
        id: uuid::Uuid::new_v4().to_string(),
        display_name: req.display_name,
        disabled: false,
        default_project: None,
        created_at: crate::auth::store::now(),
        managed: None,
        email: None,
        disabled_reason: None,
    };
    s.put_user(&u).map_err(store_err)?;
    for i in req.identities {
        s.put_identity(&Identity {
            provider: i.provider,
            subject: i.subject,
            user_id: u.id.clone(),
            email: None,
            created_at: u.created_at,
        })
        .map_err(store_err)?;
    }
    call.set_target(format!("user:{}", u.id));
    Ok(pb::CreateUserResponse {
        user: user_pb(&u, &[]).into(),
        ..Default::default()
    })
}

pub fn update_user(
    call: &Call,
    req: pb::UpdateUserRequest,
) -> ServiceResult<pb::UpdateUserResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    call.set_target(format!("user:{}", req.id));
    let s = &call.auth().store;
    let mut u = s
        .user(&req.id)
        .map_err(store_err)?
        .ok_or_else(|| not_found("user"))?;
    let managed = u.managed.as_ref().map(|m| m.connector.clone());
    for path in mask_of(req.update_mask.as_option()) {
        match path.as_str() {
            "display_name" => {
                if let Some(c) = &managed {
                    return Err(managed_conflict("its display name", c));
                }
                u.display_name = req.display_name.clone()
            }
            "default_project" => {
                u.default_project = if req.default_project.is_empty() {
                    None
                } else {
                    Some(call.target_project(Some(&req.default_project))?)
                }
            }
            "disabled" => {
                // An administrator may stop a managed user at once, and
                // only undo its own stop (§8.3, D42).
                if !req.disabled
                    && u.disabled
                    && let Some(c) = &managed
                    && u.disabled_reason != Some(DisabledReason::Admin)
                {
                    return Err(managed_conflict("whether it is enabled", c));
                }
                if req.disabled && !u.disabled {
                    u.disabled_reason = Some(DisabledReason::Admin);
                } else if !req.disabled {
                    u.disabled_reason = None;
                }
                u.disabled = req.disabled;
                if req.disabled {
                    s.remove_sessions_of(&u.id).map_err(store_err)?;
                    // And its directory refresh tokens (D55).
                    let _ = call.auth().refresh_tokens.end_user(&u.id);
                }
            }
            other => return Err(bad(format!("can't change {other}"))),
        }
    }
    s.put_user(&u).map_err(store_err)?;
    Ok(pb::UpdateUserResponse {
        user: user_pb(&u, &[]).into(),
        ..Default::default()
    })
}

pub fn link_identity(
    call: &Call,
    req: pb::LinkIdentityRequest,
) -> ServiceResult<pb::LinkIdentityResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    call.set_target(format!("user:{}", req.id));
    let s = &call.auth().store;
    s.user(&req.id)
        .map_err(store_err)?
        .ok_or_else(|| not_found("user"))?;
    if !valid_provider(&req.provider) || req.subject.is_empty() {
        return Err(bad(
            "identities are pam:, unix: or oidc:<issuer> with a subject",
        ));
    }
    if s.identity(&req.provider, &req.subject)
        .map_err(store_err)?
        .is_some()
    {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "conflict",
            "identity is already linked",
        ));
    }
    let i = Identity {
        provider: req.provider,
        subject: req.subject,
        user_id: req.id,
        email: None,
        created_at: crate::auth::store::now(),
    };
    s.put_identity(&i).map_err(store_err)?;
    Ok(pb::LinkIdentityResponse {
        identity: identity_pb(&i).into(),
        ..Default::default()
    })
}

/// Roles whose last holder can't be deleted: they administer everything.
const ADMIN_ROLES: [&str; 2] = ["role.system-admin", "role.directory-admin"];

pub fn delete_user(
    call: &Call,
    req: pb::DeleteUserRequest,
) -> ServiceResult<pb::DeleteUserResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    call.set_target(format!("user:{}", req.id));
    if call.p.user_id() == Some(req.id.as_str()) {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "cannot_delete_self",
            "you can't delete yourself: another administrator can",
        ));
    }
    let s = &call.auth().store;
    let user = s
        .user(&req.id)
        .map_err(store_err)?
        .ok_or_else(|| not_found("user"))?;
    // The IdP still has it: it would only provision it again (§8.3).
    if let Some(m) = &user.managed
        && !user.disabled
    {
        return Err(managed_conflict("an enabled user", &m.connector));
    }
    // Nobody is left unable to administer the cluster or the directory.
    let links = s.links().map_err(store_err)?;
    let me = Ent::User(req.id.clone());
    for l in links
        .iter()
        .filter(|l| l.link.principal == me && ADMIN_ROLES.contains(&l.link.template.as_str()))
    {
        let others = links.iter().any(|o| {
            o.link.id != l.link.id
                && o.link.template == l.link.template
                && o.link.resource == l.link.resource
        });
        if !others {
            return Err(ServiceError::new(
                StatusCode::CONFLICT,
                "last_admin",
                format!(
                    "{} is the last {} of {}: give the role to someone else first",
                    user.display_name, l.link.template, l.link.resource
                ),
            ));
        }
    }
    call.detail("display_name", serde_json::json!(user.display_name));
    let d = s
        .delete_user(&req.id)
        .map_err(store_err)?
        .ok_or_else(|| not_found("user"))?;
    call.auth()
        .passkeys
        .forget_user(&req.id)
        .map_err(passkey_err)?;
    let n = |x: usize| x.min(u32::MAX as usize) as u32;
    Ok(pb::DeleteUserResponse {
        identities: n(d.identities),
        sessions: n(d.sessions),
        tokens: n(d.tokens),
        links: n(d.links),
        teams: n(d.teams),
        tenants: n(d.tenants),
        ..Default::default()
    })
}

pub fn unlink_identity(
    call: &Call,
    req: pb::UnlinkIdentityRequest,
) -> ServiceResult<pb::UnlinkIdentityResponse> {
    // One's own sign-in methods with a recent login (spec/tenancy.md §6.2),
    // anyone's as a directory administrator.
    let own = call.p.user_id() == Some(req.id.as_str());
    if own {
        let (e, es) = self_entities(call);
        call.require_action("manageOwnSignIns", e, es, &[])?;
    } else {
        call.require(directory_ent(), EntitySet::new())?;
    }
    call.set_target(format!("user:{}", req.id));
    let s = &call.auth().store;
    let all = s.identities_of(&req.id).map_err(store_err)?;
    let found = all
        .iter()
        .find(|i| Identity::key(&i.provider, &i.subject) == req.key);
    let Some(i) = found else {
        return Err(not_found("identity"));
    };
    let disabled = s
        .user(&req.id)
        .map_err(store_err)?
        .is_some_and(|u| u.disabled);
    keeps_a_way_in(&all, i, disabled)?;
    call.detail("identity", serde_json::json!(req.key));
    s.remove_identity(&i.provider, &i.subject)
        .map_err(store_err)?;
    if i.provider == crate::auth::passkeys::PROVIDER {
        call.auth()
            .passkeys
            .delete(&i.subject)
            .map_err(passkey_err)?;
    }
    if req.end_sessions == Some(true) && !own {
        let n = s.remove_sessions_of(&req.id).map_err(store_err)?;
        let _ = call.auth().refresh_tokens.end_user(&req.id);
        call.detail("sessions_ended", serde_json::json!(n));
    }
    Ok(pb::UnlinkIdentityResponse::default())
}

/// A user may have many sign-in methods, but not none, unless it is
/// disabled (it can't sign in anyway): then its last one can go, to be
/// added to the user its person really is. Passkeys alone are enough only
/// when there are two (spec/passkeys.md P4): one lost device isn't a
/// lock-out.
fn keeps_a_way_in(all: &[Identity], removing: &Identity, disabled: bool) -> ServiceResult<()> {
    if disabled {
        return Ok(());
    }
    let left: Vec<&Identity> = all
        .iter()
        .filter(|i| !(i.provider == removing.provider && i.subject == removing.subject))
        .collect();
    let only_one_passkey = left.len() == 1 && left[0].provider == crate::auth::passkeys::PROVIDER;
    if left.is_empty() || only_one_passkey {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "last_identity",
            if left.is_empty() {
                "this is the user's only way to sign in: add another first, or disable the user to take it away"
            } else {
                "that would leave one passkey as the only way to sign in: add another sign-in method or a second passkey first"
            },
        ));
    }
    Ok(())
}

fn passkey_err(e: crate::auth::passkeys::PasskeyError) -> ServiceError {
    use crate::auth::passkeys::PasskeyError as E;
    match e {
        E::NotFound => not_found("passkey"),
        E::Invalid(m) => ServiceError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", m),
        E::Refused(m) => ServiceError::new(StatusCode::UNAUTHORIZED, "passkey_refused", m),
        E::Storage(m) => ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", m),
    }
}

fn passkey_pb(p: &crate::auth::passkeys::Passkey) -> pb::Passkey {
    pb::Passkey {
        id: p.id.clone(),
        name: p.name.clone(),
        algorithm: match p.alg {
            crate::auth::webauthn::ES256 => "ES256",
            crate::auth::webauthn::EDDSA => "EdDSA",
            crate::auth::webauthn::RS256 => "RS256",
            _ => "?",
        }
        .into(),
        backed_up: Some(p.backed_up),
        created_at: ts(p.created_at).into(),
        last_used_at: p.last_used_at.map(ts).into(),
        ..Default::default()
    }
}

/// One's own (`manageOwnSignIns`), or anyone's with `admin_action`.
fn own_or(call: &Call, user: &str, admin_action: &'static str) -> ServiceResult<bool> {
    let own = call.p.user_id() == Some(user);
    if own {
        let (e, es) = self_entities(call);
        call.require_action("manageOwnSignIns", e, es, &[])?;
    } else {
        call.with_action(admin_action)
            .require(directory_ent(), EntitySet::new())?;
    }
    call.set_target(format!("user:{user}"));
    Ok(own)
}

pub fn list_passkeys(
    call: &Call,
    req: pb::ListPasskeysRequest,
) -> ServiceResult<pb::ListPasskeysResponse> {
    let own = call.p.user_id() == Some(req.id.as_str());
    if !own {
        call.require(directory_ent(), EntitySet::new())?;
    }
    let list = call.auth().passkeys.list(&req.id).map_err(passkey_err)?;
    Ok(pb::ListPasskeysResponse {
        passkeys: list.iter().map(passkey_pb).collect(),
        rp_id: call.auth().config.auth.passkeys.rp_id.clone(),
        ..Default::default()
    })
}

pub fn rename_passkey(
    call: &Call,
    req: pb::RenamePasskeyRequest,
) -> ServiceResult<pb::RenamePasskeyResponse> {
    if call.p.user_id() != Some(req.id.as_str()) {
        return Err(ServiceError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "only its user renames a passkey",
        ));
    }
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(bad("a passkey's name is 1-64 printable characters"));
    }
    let mut p = call
        .auth()
        .passkeys
        .get(&req.passkey)
        .map_err(passkey_err)?
        .filter(|p| p.user_id == req.id)
        .ok_or_else(|| not_found("passkey"))?;
    p.name = name.to_string();
    call.auth().passkeys.put(&p).map_err(passkey_err)?;
    Ok(pb::RenamePasskeyResponse {
        passkey: passkey_pb(&p).into(),
        ..Default::default()
    })
}

pub fn delete_passkey(
    call: &Call,
    req: pb::DeletePasskeyRequest,
) -> ServiceResult<pb::DeletePasskeyResponse> {
    own_or(call, &req.id, "manageUsers")?;
    let p = call
        .auth()
        .passkeys
        .get(&req.passkey)
        .map_err(passkey_err)?
        .filter(|p| p.user_id == req.id)
        .ok_or_else(|| not_found("passkey"))?;
    let s = &call.auth().store;
    let all = s.identities_of(&req.id).map_err(store_err)?;
    let disabled = s
        .user(&req.id)
        .map_err(store_err)?
        .is_some_and(|u| u.disabled);
    let me = Identity {
        provider: crate::auth::passkeys::PROVIDER.into(),
        subject: p.id.clone(),
        user_id: req.id.clone(),
        email: None,
        created_at: 0,
    };
    keeps_a_way_in(&all, &me, disabled)?;
    call.detail("passkey", serde_json::json!({ "id": p.id, "name": p.name }));
    s.remove_identity(crate::auth::passkeys::PROVIDER, &p.id)
        .map_err(store_err)?;
    call.auth().passkeys.delete(&p.id).map_err(passkey_err)?;
    Ok(pb::DeletePasskeyResponse::default())
}

pub fn update_me(call: &Call, req: pb::UpdateMeRequest) -> ServiceResult<pb::UpdateMeResponse> {
    let Some(mut user) = call.p.user.clone() else {
        return Err(bad("service accounts have no profile"));
    };
    if call.p.method == Method::Disabled {
        return Err(bad("authentication is disabled"));
    }
    user.default_project = if req.default_project.is_empty() {
        None
    } else {
        let id = call.target_project(Some(&req.default_project))?;
        call.require_action(
            "readProject",
            Ent::Project(id.clone()),
            project_entities(&id),
            &[],
        )?;
        Some(id)
    };
    call.auth().store.put_user(&user).map_err(store_err)?;
    Ok(pb::UpdateMeResponse {
        user: user_pb(&user, &[]).into(),
        ..Default::default()
    })
}

/// The caller's own SSH public keys from their home directory, read by
/// glidex-authd (the control plane can't read home directories). Only for
/// users with a local (PAM or Unix) account; for prefilling guest login
/// credentials.
pub async fn my_ssh_keys(
    call: &Call,
    _req: pb::GetMySshKeysRequest,
) -> ServiceResult<pb::GetMySshKeysResponse> {
    let unavailable = |reason: &str| pb::GetMySshKeysResponse {
        available: Some(false),
        reason: Some(reason.into()),
        ..Default::default()
    };
    let Some(user) = call.p.user.clone() else {
        return Ok(unavailable("service accounts have no home directory"));
    };
    let identities = call
        .auth()
        .store
        .identities_of(&user.id)
        .map_err(store_err)?;
    let Some(login) = ["pam", "unix"].iter().find_map(|p| {
        identities
            .iter()
            .find(|i| i.provider == *p)
            .map(|i| i.subject.clone())
    }) else {
        // Keys its directory holds (a pull's SSH key field, spec/tenancy.md §8.2).
        let stored = call.auth().store.user(&user.id).map_err(store_err)?;
        if let Some(m) = stored
            .and_then(|u| u.managed)
            .filter(|m| !m.ssh_keys.is_empty())
        {
            return Ok(pb::GetMySshKeysResponse {
                available: Some(true),
                keys: m.ssh_keys,
                source: Some(format!("connector:{}", m.connector)),
                ..Default::default()
            });
        }
        return Ok(unavailable(
            "your account has no local login on this host; paste your public key",
        ));
    };
    let client = glidex_authd::client::AuthdClient::new(&call.auth().config.auth.pam.authd_socket);
    let who = login.clone();
    let result = tokio::task::spawn_blocking(move || client.public_keys(&who))
        .await
        .map_err(|e| {
            ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
        })?;
    Ok(match result {
        Ok(keys) => pb::GetMySshKeysResponse {
            available: Some(true),
            username: Some(login),
            keys,
            source: Some("local".into()),
            ..Default::default()
        },
        Err(glidex_authd::client::AuthdError::Denied) => {
            unavailable("your local account can't log in to glidex")
        }
        Err(e) => {
            tracing::warn!("reading public keys for {}: {}", login, e);
            unavailable("your public keys can't be read right now (glidex-authd)")
        }
    })
}

// ---- teams ----------------------------------------------------------------

pub fn list_teams(call: &Call, _req: pb::ListTeamsRequest) -> ServiceResult<pb::ListTeamsResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    Ok(pb::ListTeamsResponse {
        teams: call
            .auth()
            .store
            .teams()
            .map_err(store_err)?
            .iter()
            .map(team_pb)
            .collect(),
        ..Default::default()
    })
}

fn valid_team_name(name: &str) -> ServiceResult<()> {
    // unix:* teams come only from peer groups (spec §7.3).
    if name.is_empty() || name.len() > 64 || name.starts_with("unix:") {
        return Err(bad(
            "team names are 1-64 characters and can't start with \"unix:\"",
        ));
    }
    Ok(())
}

pub fn create_team(
    call: &Call,
    req: pb::CreateTeamRequest,
) -> ServiceResult<pb::CreateTeamResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    valid_team_name(&req.name)?;
    let s = &call.auth().store;
    if s.team_by_name(&req.name).map_err(store_err)?.is_some() {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "conflict",
            "team exists",
        ));
    }
    let t = Team {
        id: uuid::Uuid::new_v4().to_string(),
        name: req.name,
        members: vec![],
        created_at: crate::auth::store::now(),
        managed: None,
    };
    s.put_team(&t).map_err(store_err)?;
    call.set_target(format!("team:{}", t.id));
    Ok(pb::CreateTeamResponse {
        team: team_pb(&t).into(),
        ..Default::default()
    })
}

fn team(call: &Call, id: &str) -> ServiceResult<Team> {
    call.set_target(format!("team:{}", id));
    call.auth()
        .store
        .team(id)
        .map_err(store_err)?
        .ok_or_else(|| not_found("team"))
}

pub fn update_team(
    call: &Call,
    req: pb::UpdateTeamRequest,
) -> ServiceResult<pb::UpdateTeamResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    valid_team_name(&req.name)?;
    let mut t = team(call, &req.id)?;
    if let Some(m) = &t.managed {
        return Err(managed_conflict("its name", &m.connector));
    }
    t.name = req.name;
    call.auth().store.put_team(&t).map_err(store_err)?;
    Ok(pb::UpdateTeamResponse {
        team: team_pb(&t).into(),
        ..Default::default()
    })
}

pub fn delete_team(
    call: &Call,
    req: pb::DeleteTeamRequest,
) -> ServiceResult<pb::DeleteTeamResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    if let Some(m) = team(call, &req.id)?.managed {
        return Err(managed_conflict("the team", &m.connector));
    }
    call.auth().store.remove_team(&req.id).map_err(store_err)?;
    call.auth()
        .forget_entity(&Ent::Team(req.id))
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    Ok(pb::DeleteTeamResponse::default())
}

pub fn add_team_member(
    call: &Call,
    req: pb::AddTeamMemberRequest,
) -> ServiceResult<pb::AddTeamMemberResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let mut t = team(call, &req.id)?;
    call.auth()
        .store
        .user(&req.user)
        .map_err(store_err)?
        .ok_or_else(|| not_found("user"))?;
    if !t
        .members
        .iter()
        .any(|m| m.user_id == req.user && m.source == MemberSource::Manual)
    {
        t.members.push(TeamMember {
            user_id: req.user,
            source: MemberSource::Manual,
        });
        call.auth().store.put_team(&t).map_err(store_err)?;
    }
    Ok(pb::AddTeamMemberResponse {
        team: team_pb(&t).into(),
        ..Default::default()
    })
}

pub fn remove_team_member(
    call: &Call,
    req: pb::RemoveTeamMemberRequest,
) -> ServiceResult<pb::RemoveTeamMemberResponse> {
    call.require(directory_ent(), EntitySet::new())?;
    let mut t = team(call, &req.id)?;
    // Memberships a connector gives are its own (§8.3); manual ones stay
    // the directory's.
    let mine = |m: &TeamMember| match &m.source {
        MemberSource::Connector(c) => Some(c.clone()),
        _ => None,
    };
    let synced: Vec<String> = t
        .members
        .iter()
        .filter(|m| m.user_id == req.user)
        .filter_map(mine)
        .filter(|c| {
            call.auth()
                .connectors
                .get(c)
                .ok()
                .flatten()
                .is_some_and(|c| !c.provisioning.is_none())
        })
        .collect();
    let before = t.members.len();
    t.members
        .retain(|m| m.user_id != req.user || mine(m).is_some_and(|c| synced.contains(&c)));
    if t.members.len() == before
        && let Some(c) = synced.first()
    {
        return Err(managed_conflict("this membership", c));
    }
    call.auth().store.put_team(&t).map_err(store_err)?;
    Ok(pb::RemoveTeamMemberResponse {
        team: team_pb(&t).into(),
        ..Default::default()
    })
}

// ---- tokens ---------------------------------------------------------------

fn self_entities(call: &Call) -> (Ent, EntitySet) {
    let id = call.p.user_id().unwrap_or_default().to_string();
    (Ent::User(id), EntitySet::new())
}

pub fn list_tokens(
    call: &Call,
    _req: pb::ListTokensRequest,
) -> ServiceResult<pb::ListTokensResponse> {
    let all = call.allowed("manageAnyTokens", directory_ent(), EntitySet::new());
    let links = call.auth().store.links().map_err(store_err)?;
    let mut tokens = Vec::new();
    for (_, t) in call.auth().store.tokens().map_err(store_err)? {
        let visible = all
            || match &t.kind {
                TokenKind::Personal { owner } => call.p.user_id() == Some(owner.as_str()),
                TokenKind::ServiceAccount { project } => call.allowed(
                    "manageServiceTokens",
                    Ent::Project(project.clone()),
                    project_entities(project),
                ),
            };
        if visible {
            let roles: Vec<_> = links
                .iter()
                .filter(|l| l.link.principal == Ent::Token(t.id.clone()))
                .cloned()
                .collect();
            tokens.push(token_pb(&t, &roles));
        }
    }
    Ok(pb::ListTokensResponse {
        tokens,
        ..Default::default()
    })
}

pub fn create_token(
    call: &Call,
    req: pb::CreateTokenRequest,
) -> ServiceResult<pb::CreateTokenResponse> {
    if call.p.token.is_some() {
        return Err(ServiceError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "tokens can't create tokens",
        ));
    }
    let by = call.p.user_id().unwrap_or("-").to_string();
    let service = req.kind.as_known() == Some(pb::TokenKind::TOKEN_KIND_SERVICE_ACCOUNT);
    let kind = if service {
        let project = call.target_project(req.project.as_deref())?;
        call.set_project(&project);
        call.require_action(
            "manageServiceTokens",
            Ent::Project(project.clone()),
            project_entities(&project),
            &[],
        )?;
        for r in &req.roles {
            if !authz::PROJECT_ROLES.contains(&r.role.as_str())
                || r.project.as_deref().is_some_and(|p| {
                    call.target_project(Some(p)).ok().as_deref() != Some(project.as_str())
                })
            {
                return Err(bad(
                    "a service account only gets project roles in its own project",
                ));
            }
        }
        TokenKind::ServiceAccount { project }
    } else {
        let (e, es) = self_entities(call);
        call.require_action("manageOwnTokens", e, es, &[])?;
        TokenKind::Personal { owner: by.clone() }
    };
    let (secret, token) = call
        .auth()
        .create_token(
            &req.name,
            kind.clone(),
            &by,
            req.expires_in_days.map(u64::from),
            req.device.as_deref(),
            req.client.as_deref(),
        )
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    call.set_target(format!("token:{}", token.id));
    let mut roles = Vec::new();
    for r in &req.roles {
        let resource = match &r.project {
            Some(p) => Ent::Project(call.target_project(Some(p))?),
            None => match &kind {
                TokenKind::ServiceAccount { project } => Ent::Project(project.clone()),
                TokenKind::Personal { .. } => Ent::Cluster,
            },
        };
        match call
            .auth()
            .add_link(&r.role, Ent::Token(token.id.clone()), resource, &by)
        {
            Ok(l) => roles.push(l),
            Err(e) => {
                let _ = call.auth().revoke_token(&token.id);
                return Err(auth_error(e).into());
            }
        }
    }
    Ok(pb::CreateTokenResponse {
        token: secret,
        record: token_pb(&token, &roles).into(),
        ..Default::default()
    })
}

pub fn revoke_token(
    call: &Call,
    req: pb::RevokeTokenRequest,
) -> ServiceResult<pb::RevokeTokenResponse> {
    let (_, t) = call
        .auth()
        .store
        .token_by_id(&req.id)
        .map_err(store_err)?
        .ok_or_else(|| not_found("token"))?;
    call.set_target(format!("token:{}", t.id));
    let mine = matches!(&t.kind, TokenKind::Personal { owner } if call.p.user_id() == Some(owner.as_str()));
    let ok =
        mine || match &t.kind {
            TokenKind::ServiceAccount { project } => call.allowed(
                "manageServiceTokens",
                Ent::Project(project.clone()),
                project_entities(project),
            ),
            _ => false,
        } || call.allowed("manageAnyTokens", directory_ent(), EntitySet::new());
    if !ok {
        return Err(not_found("token"));
    }
    call.auth()
        .revoke_token(&req.id)
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    Ok(pb::RevokeTokenResponse::default())
}

// ---- site policies (spec §7.6) ---------------------------------------------

/// Limits on site policies.
const MAX_POLICY_BYTES: usize = 64 * 1024;
const MAX_SITE_POLICIES: usize = 200;

pub fn list_policies(
    call: &Call,
    _req: pb::ListPoliciesRequest,
) -> ServiceResult<pb::ListPoliciesResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    Ok(pb::ListPoliciesResponse {
        policies: call.auth().engine.listing().iter().map(loaded_pb).collect(),
        site: call
            .auth()
            .store
            .site_policies()
            .map_err(store_err)?
            .iter()
            .map(site_pb)
            .collect(),
        ..Default::default()
    })
}

pub fn get_policy(call: &Call, req: pb::GetPolicyRequest) -> ServiceResult<pb::GetPolicyResponse> {
    use pb::__buffa::oneof::get_policy_response::Policy;
    call.require(Ent::Cluster, EntitySet::new())?;
    if let Some(p) = call.auth().store.site_policy(&req.id).map_err(store_err)? {
        return Ok(pb::GetPolicyResponse {
            source: pb::PolicySource::POLICY_SOURCE_SITE.into(),
            policy: Some(Policy::Site(Box::new(site_pb(&p)))),
            ..Default::default()
        });
    }
    match call
        .auth()
        .engine
        .listing()
        .into_iter()
        .find(|p| p.id == req.id)
    {
        Some(p) => Ok(pb::GetPolicyResponse {
            source: source_pb(p.source).into(),
            policy: Some(Policy::Loaded(Box::new(loaded_pb(&p)))),
            ..Default::default()
        }),
        None => Err(not_found("policy")),
    }
}

pub fn policy_versions(
    call: &Call,
    req: pb::ListPolicyVersionsRequest,
) -> ServiceResult<pb::ListPolicyVersionsResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let versions = call
        .auth()
        .store
        .site_policy_versions(&req.id)
        .map_err(store_err)?
        .iter()
        .map(|v| pb::SitePolicyVersion {
            id: v.id.clone(),
            version: v.version,
            text: v.text.clone(),
            enabled: Some(v.enabled),
            author: v.author.clone(),
            time: ts(v.time).into(),
            deleted: v.deleted.then_some(true),
            ..Default::default()
        })
        .collect();
    Ok(pb::ListPolicyVersionsResponse {
        versions,
        ..Default::default()
    })
}

/// The stored site policies with `changes` applied.
fn candidate_inputs(
    call: &Call,
    changes: &[pb::PolicyChange],
) -> ServiceResult<(Vec<authz::Link>, Vec<SiteSource>)> {
    let (links, mut site) = call
        .auth()
        .policy_inputs()
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    let stored = call.auth().store.site_policies().map_err(store_err)?;
    for ch in changes {
        if site
            .iter()
            .any(|s| s.id == ch.id && s.source == authz::PolicySource::File)
        {
            return Err(ServiceError::new(
                StatusCode::CONFLICT,
                "conflict",
                format!(
                    "{} comes from a policy file and can't be changed here",
                    ch.id
                ),
            ));
        }
        site.retain(|s| s.id != ch.id);
        if ch.delete == Some(true) {
            continue;
        }
        let current = stored.iter().find(|p| p.id == ch.id);
        let text = ch
            .text
            .clone()
            .or(current.map(|p| p.text.clone()))
            .unwrap_or_default();
        let enabled = ch.enabled.or(current.map(|p| p.enabled)).unwrap_or(true);
        if enabled {
            site.push(SiteSource {
                id: ch.id.clone(),
                text,
                source: authz::PolicySource::Site,
            });
        }
    }
    Ok((links, site))
}

/// Refuse a change that would stop the caller from writing policies
/// (§7.6). Break-glass principals can always recover.
fn lock_out_check(call: &Call, set: &cedar_policy::PolicySet) -> ServiceResult<()> {
    if call.p.is_break_glass() {
        return Ok(());
    }
    let d = call.auth().authorize_in(
        Some(set),
        &call.p,
        "writePolicy",
        Ent::Cluster,
        EntitySet::new(),
        &[],
    );
    if d.allowed {
        Ok(())
    } else {
        Err(ServiceError::new(
            StatusCode::CONFLICT,
            "would_lock_out",
            "after this change you could no longer manage policies",
        ))
    }
}

fn build(
    call: &Call,
    links: &[authz::Link],
    site: &[SiteSource],
) -> ServiceResult<cedar_policy::PolicySet> {
    call.auth()
        .engine
        .build(links, site)
        .map_err(|e| ServiceError::from(auth_error(e.into())))
}

pub fn put_policy(call: &Call, req: pb::PutPolicyRequest) -> ServiceResult<pb::PutPolicyResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    call.set_target(format!("policy:{}", req.id));
    if req.text.len() > MAX_POLICY_BYTES {
        return Err(ServiceError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_policy",
            "a policy is at most 64 KiB",
        ));
    }
    let stored = call.auth().store.site_policies().map_err(store_err)?;
    if req.version == 0 && stored.len() >= MAX_SITE_POLICIES {
        return Err(ServiceError::new(
            StatusCode::CONFLICT,
            "conflict",
            "at most 200 site policies",
        ));
    }
    call.auth()
        .engine
        .validate_site_policy(&req.id, &req.text)
        .map_err(|e| ServiceError::from(auth_error(e.into())))?;
    let enabled = req.enabled.unwrap_or(true);
    let change = pb::PolicyChange {
        id: req.id.clone(),
        text: Some(req.text.clone()),
        enabled: Some(enabled),
        ..Default::default()
    };
    let (links, site) = candidate_inputs(call, &[change])?;
    let set = build(call, &links, &site)?;
    lock_out_check(call, &set)?;
    call.detail(
        "sha256",
        serde_json::json!(auth::sha256_hex(req.text.as_bytes())),
    );
    let p = call
        .auth()
        .store
        .write_site_policy(
            &req.id,
            req.version,
            Some((&req.text, &req.description, enabled)),
            call.p.user_id().unwrap_or("-"),
        )
        .map_err(store_err)?;
    call.auth()
        .reload_policies()
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    call.detail("version", serde_json::json!(p.as_ref().map(|p| p.version)));
    Ok(pb::PutPolicyResponse {
        policy: p.as_ref().map(site_pb).into(),
        ..Default::default()
    })
}

pub fn delete_policy(
    call: &Call,
    req: pb::DeletePolicyRequest,
) -> ServiceResult<pb::DeletePolicyResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    call.set_target(format!("policy:{}", req.id));
    let change = pb::PolicyChange {
        id: req.id.clone(),
        delete: Some(true),
        ..Default::default()
    };
    let (links, site) = candidate_inputs(call, &[change])?;
    let set = build(call, &links, &site)?;
    lock_out_check(call, &set)?;
    call.auth()
        .store
        .write_site_policy(&req.id, req.version, None, call.p.user_id().unwrap_or("-"))
        .map_err(store_err)?;
    call.auth()
        .reload_policies()
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    Ok(pb::DeletePolicyResponse::default())
}

pub fn validate_policy(
    call: &Call,
    req: pb::ValidatePolicyRequest,
) -> ServiceResult<pb::ValidatePolicyResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    let errors = match call.auth().engine.validate_site_policy(&req.id, &req.text) {
        Ok(()) => Vec::new(),
        Err(authz::AuthzError::Validation(errors)) => errors,
        Err(e) => vec![e.to_string()],
    };
    Ok(pb::ValidatePolicyResponse {
        valid: Some(errors.is_empty()),
        errors,
        ..Default::default()
    })
}

pub async fn simulate_policy(
    call: &Call,
    req: pb::SimulatePolicyRequest,
) -> ServiceResult<pb::SimulatePolicyResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    if req.requests.len() > 100 {
        return Err(bad("at most 100 requests"));
    }
    for ch in &req.changes {
        if let Some(t) = &ch.text {
            call.auth()
                .engine
                .validate_site_policy(&ch.id, t)
                .map_err(|e| ServiceError::from(auth_error(e.into())))?;
        }
    }
    let (links, site) = candidate_inputs(call, &req.changes)?;
    let candidate = build(call, &links, &site)?;
    let (links, site) = call
        .auth()
        .policy_inputs()
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    let current = build(call, &links, &site)?;
    let mut results = Vec::new();
    for r in &req.requests {
        let failed = |m: String| pb::SimulatedResult {
            error: Some(m),
            ..Default::default()
        };
        let principal = principal_of(
            r.principal
                .as_option()
                .ok_or_else(|| bad("principal is required"))?,
        )?;
        let resource = match r.resource.as_option() {
            Some(e) => entity_of(e)?,
            None => Ent::Cluster,
        };
        let Some(p) = call
            .auth()
            .principal_for(&principal)
            .map_err(|e| ServiceError::from(auth_error(e)))?
        else {
            results.push(failed(format!("{} not found", principal)));
            continue;
        };
        let Some(es) = resource_entities(call, &resource).await else {
            results.push(failed(format!("{} not found", resource)));
            continue;
        };
        let before = call.auth().authorize_in(
            Some(&current),
            &p,
            &r.action,
            resource.clone(),
            es.clone(),
            &[],
        );
        let after = call
            .auth()
            .authorize_in(Some(&candidate), &p, &r.action, resource, es, &[]);
        results.push(pb::SimulatedResult {
            current: decision_pb(&before).into(),
            candidate: decision_pb(&after).into(),
            ..Default::default()
        });
    }
    Ok(pb::SimulatePolicyResponse {
        results,
        ..Default::default()
    })
}

pub fn reload_policies(
    call: &Call,
    _req: pb::ReloadPoliciesRequest,
) -> ServiceResult<pb::ReloadPoliciesResponse> {
    call.require(Ent::Cluster, EntitySet::new())?;
    call.auth()
        .reload_policies()
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    Ok(pb::ReloadPoliciesResponse::default())
}

// ---- audit (spec §10) -------------------------------------------------------

pub fn read_audit(call: &Call, req: pb::ReadAuditRequest) -> ServiceResult<pb::ReadAuditResponse> {
    let all = call.allowed("readAudit", Ent::Cluster, EntitySet::new());
    let project = match &req.project {
        Some(p) => Some(call.target_project(Some(p))?),
        None => None,
    };
    if !all {
        // Project owners read their project's entries.
        match &project {
            Some(p) => call.require_action(
                "readProjectAudit",
                Ent::Project(p.clone()),
                project_entities(p),
                &[],
            )?,
            None => call.require_action("readAudit", Ent::Cluster, EntitySet::new(), &[])?,
        }
    }
    let since_ms = req
        .since
        .as_option()
        .map(|t| (t.seconds.max(0) as u64) * 1000 + (t.nanos.max(0) as u64) / 1_000_000)
        .unwrap_or(0);
    let limit = req.limit.map(|l| l as usize).unwrap_or(500).min(5000);
    let entries = call
        .auth()
        .store
        .audit(since_ms, limit, |e| {
            project
                .as_ref()
                .is_none_or(|p| e.project.as_deref() == Some(p.as_str()))
                && req.user.as_ref().is_none_or(|u| {
                    e.principal.get("user").and_then(|v| v.as_str()) == Some(u.as_str())
                })
        })
        .map_err(store_err)?;
    Ok(pb::ReadAuditResponse {
        entries: entries.iter().map(audit_pb).collect(),
        ..Default::default()
    })
}

// ---- capability checks (spec §7.7) ----------------------------------------

/// Entities for any resource, loaded from the managers; `None` when it
/// doesn't exist.
pub async fn resource_entities(call: &Call, e: &Ent) -> Option<EntitySet> {
    let m = call.manager();
    Some(match e {
        Ent::Cluster => EntitySet::new(),
        // A node's host sits under the cluster: links on the cluster cover it.
        Ent::Host | Ent::Node(_) => {
            let mut es = EntitySet::new();
            es.host(&e.id());
            es
        }
        Ent::Project(id) => {
            m.projects().get(id).ok().flatten()?;
            project_entities(id)
        }
        Ent::Vm(id) => vm_entities(&m.get_vm(id).await.ok()?).1,
        Ent::Disk(id) => {
            let d = m.get_disk(id).await.ok()?;
            disk_entities(&d.id, &d.project).1
        }
        Ent::Credential(key) => {
            let (project, username) = key.split_once('/')?;
            credential_entities(project, username).1
        }
        Ent::Network(name) => {
            let n = m.get_network(name).ok()?;
            let mut es = EntitySet::new();
            add_network(&mut es, &n);
            es
        }
        Ent::Image(id) => {
            let mut es = EntitySet::new();
            es.image(id);
            es
        }
        Ent::Datastore(id) => {
            let mut es = EntitySet::new();
            es.datastore(id);
            es
        }
        Ent::PciPool(name) => crate::api::vms::pci_pool_entities(call, name).1,
        Ent::PciDevice(id) => {
            let local = call.manager().local_node_id();
            let (node, bdf) = id.split_once('/').unwrap_or((local.as_str(), id.as_str()));
            crate::api::vms::pci_entities(call, node, bdf).1
        }
        // Tenants and the directory are filled in by AuthService.
        Ent::Tenant(id) => {
            call.auth().tenants.get(id).ok().flatten()?;
            EntitySet::new()
        }
        Ent::Directory(id) if id == crate::tenants::LOCAL_DIRECTORY => EntitySet::new(),
        Ent::Directory(_) => return None,
        Ent::User(id) => {
            let mut es = EntitySet::new();
            if call.p.user_id() != Some(id) {
                es.user(id, false, &[]);
            }
            es
        }
        Ent::Team(_) | Ent::Token(_) => return None,
    })
}

fn schema_has(call: &Call, action: &str) -> bool {
    call.auth()
        .engine
        .schema()
        .actions()
        .any(|a| a.id().unescaped() == action)
}

pub async fn check(call: &Call, req: pb::CheckRequest) -> ServiceResult<pb::CheckResponse> {
    if req.checks.len() > 200 {
        return Err(bad("at most 200 checks"));
    }
    let mut results = Vec::new();
    for item in &req.checks {
        if !schema_has(call, &item.action) {
            results.push(pb::CheckResult {
                allowed: Some(false),
                error: Some("unknown action".into()),
                ..Default::default()
            });
            continue;
        }
        let resource = match item.resource.as_option() {
            Some(r) => entity_of(r)?,
            None => Ent::Cluster,
        };
        let Some(es) = resource_entities(call, &resource).await else {
            results.push(pb::CheckResult {
                allowed: Some(false),
                ..Default::default()
            });
            continue;
        };
        if call.allowed(&item.action, resource.clone(), es.clone()) {
            results.push(pb::CheckResult {
                allowed: Some(true),
                ..Default::default()
            });
        } else {
            // Step-up refuses it only until the user logs in again: say so,
            // so a client offers the action and its re-login.
            let reauth = call.allowed_after_reauth(&item.action, resource, es);
            results.push(pb::CheckResult {
                allowed: Some(false),
                reauth: Some(reauth),
                ..Default::default()
            });
        }
    }
    Ok(pb::CheckResponse {
        results,
        ..Default::default()
    })
}

const MAX_ALLOWED_ACTIONS: usize = 50;
const MAX_ALLOWED_RESOURCES: usize = 500;

/// For each resource, the actions the caller may take on it. An action
/// that doesn't apply to the resource's type (the schema's `appliesTo`) is
/// left out; an unknown action is an error, so a typo in a client fails
/// loudly.
pub async fn allowed(call: &Call, req: pb::AllowedRequest) -> ServiceResult<pb::AllowedResponse> {
    if req.actions.len() > MAX_ALLOWED_ACTIONS || req.resources.len() > MAX_ALLOWED_RESOURCES {
        return Err(bad(format!(
            "at most {MAX_ALLOWED_ACTIONS} actions and {MAX_ALLOWED_RESOURCES} resources"
        )));
    }
    let schema = call.auth().engine.schema();
    // action → the resource types it applies to.
    let mut applies: Vec<(&str, Vec<String>)> = Vec::new();
    for a in &req.actions {
        let uid: cedar_policy::EntityUid = format!("Glidex::Action::{:?}", a)
            .parse()
            .map_err(|_| bad(format!("bad action name {a}")))?;
        let Some(types) = schema.resources_for_action(&uid) else {
            return Err(bad(format!("unknown action {a}")));
        };
        applies.push((
            a.as_str(),
            types.map(|t| t.basename().to_string()).collect(),
        ));
    }
    let list = |actions: Vec<String>| pb::ActionList {
        actions,
        ..Default::default()
    };
    let mut allowed = Vec::new();
    let mut reauth = Vec::new();
    for r in &req.resources {
        let r = entity_of(r)?;
        let ty = r.type_name();
        let candidates: Vec<&str> = applies
            .iter()
            .filter(|(_, ts)| ts.iter().any(|t| t == ty))
            .map(|(a, _)| *a)
            .collect();
        let Some(es) = (if candidates.is_empty() {
            None
        } else {
            resource_entities(call, &r).await
        }) else {
            allowed.push(list(Vec::new()));
            reauth.push(list(Vec::new()));
            continue;
        };
        let (ok, later): (Vec<&str>, Vec<&str>) = candidates
            .into_iter()
            .partition(|a| call.allowed(a, r.clone(), es.clone()));
        reauth.push(list(
            later
                .into_iter()
                .filter(|a| call.allowed_after_reauth(a, r.clone(), es.clone()))
                .map(String::from)
                .collect(),
        ));
        allowed.push(list(ok.into_iter().map(String::from).collect()));
    }
    Ok(pb::AllowedResponse {
        allowed,
        reauth,
        ..Default::default()
    })
}
