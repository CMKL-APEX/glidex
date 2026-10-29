//! `glidex.system.v1.SystemService`: what a client asks before and right
//! after logging in (spec/18-grpc.md G0).

use super::{Call, ServiceResult};
use crate::api::{App, auth_error};
use crate::auth::Principal;
use crate::authz::Ent;
use glidex_api::proto::glidex::system::v1::{
    EntityRef, GetServerInfoResponse, LoginMethods, ProjectRole, WhoAmIResponse,
};

/// The SHA-256 of the API's certificate, `AB:CD:…` (`None` without TLS).
pub fn api_fingerprint(app: &App) -> Option<String> {
    match &app.auth.config.tls {
        crate::config::TlsSetting::Mode(crate::config::TlsMode::Off) => None,
        crate::config::TlsSetting::Mode(crate::config::TlsMode::Auto) => {
            glidex_tls::fingerprint(&crate::serve::self_signed_dir().join("cp.crt")).ok()
        }
        crate::config::TlsSetting::Files(f) => glidex_tls::fingerprint(&f.cert).ok(),
    }
}

/// `GetServerInfo` / `GET /auth/server-info` (spec/17-gxctl-auth.md §7.1):
/// what client login profiles bind to. Public.
pub fn server_info(app: &App) -> GetServerInfoResponse {
    let fingerprint = api_fingerprint(app);
    let identity = crate::cluster::identity::Files::beside(app.manager.db_path())
        .load_identity()
        .ok()
        .flatten();
    let name = app
        .auth
        .config
        .server_name
        .clone()
        .or_else(glidex_tls::hostname)
        .unwrap_or_else(|| "glidex".into());
    GetServerInfoResponse {
        cluster_id: identity.as_ref().map(|i| i.cluster_id.clone()),
        node_id: identity.as_ref().map(|i| i.node_id.clone()),
        cluster_name: name,
        version: env!("CARGO_PKG_VERSION").into(),
        fingerprint,
        token_audience: crate::api::op::cluster_id(app),
        methods: LoginMethods {
            pam: Some(app.auth.pam_connector(None).is_ok()),
            oidc: Some(app.auth.oidc_enabled()),
            disabled: Some(app.auth.is_disabled()),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

/// One project role of the caller.
pub(crate) struct RoleLink {
    pub project: String,
    pub project_name: Option<String>,
    pub role: String,
    /// The linked principal the role comes through.
    pub via: Ent,
}

/// The caller's role links: host roles (deduplicated) and project roles.
pub(crate) fn roles_of(app: &App, p: &Principal) -> ServiceResult<(Vec<String>, Vec<RoleLink>)> {
    let links = app.auth.links_of(p).map_err(auth_error)?;
    let projects = app.manager.projects();
    let mut host_roles = Vec::new();
    let mut project_roles = Vec::new();
    for l in &links {
        match &l.link.resource {
            Ent::Cluster => host_roles.push(l.link.template.clone()),
            Ent::Project(id) => project_roles.push(RoleLink {
                project: id.clone(),
                project_name: projects.get(id).ok().flatten().map(|p| p.name),
                role: l.link.template.clone(),
                via: l.link.principal.clone(),
            }),
            _ => {}
        }
    }
    host_roles.sort();
    host_roles.dedup();
    Ok((host_roles, project_roles))
}

fn entity_ref(e: &Ent) -> EntityRef {
    // Ent serializes as {"type", "id"}: the REST shape this mirrors.
    let v = serde_json::to_value(e).unwrap_or_default();
    EntityRef {
        r#type: v["type"].as_str().unwrap_or_default().into(),
        id: v["id"].as_str().map(String::from),
        ..Default::default()
    }
}

/// `WhoAmI`: who the caller is and which roles they hold; also the
/// answer to a browser login (`api/access.rs`).
pub fn whoami(call: &Call) -> ServiceResult<WhoAmIResponse> {
    whoami_of(&call.app, &call.p)
}

pub fn whoami_of(app: &App, p: &crate::auth::Principal) -> ServiceResult<WhoAmIResponse> {
    let (host_roles, project_roles) = roles_of(app, p)?;
    Ok(WhoAmIResponse {
        user_id: p.user.as_ref().map(|u| u.id.clone()),
        display_name: p
            .user
            .as_ref()
            .map(|u| u.display_name.clone())
            .or_else(|| p.token.as_ref().map(|t| t.name.clone()))
            .unwrap_or_default(),
        method: p.method.as_str().into(),
        token_id: p.token.as_ref().map(|t| t.id.clone()),
        teams: p.teams.clone(),
        break_glass: Some(p.is_break_glass()),
        host_roles,
        project_roles: project_roles
            .into_iter()
            .map(|r| ProjectRole {
                project: r.project,
                project_name: r.project_name,
                role: r.role,
                via: entity_ref(&r.via).into(),
                ..Default::default()
            })
            .collect(),
        default_project: p.default_project().map(String::from),
        mfa: Some(p.mfa),
        directory_id: p.directory_id().into(),
        user: p
            .user
            .as_ref()
            // Its own sign-in methods (the account page lists them).
            .map(|u| {
                let ids = app.auth.store.identities_of(&u.id).unwrap_or_default();
                crate::service::access::user_pb(u, &ids)
            })
            .into(),
        token: p
            .token
            .as_ref()
            .map(|t| glidex_api::proto::glidex::system::v1::TokenSummary {
                id: t.id.clone(),
                name: t.name.clone(),
                device: t.device.clone(),
                client: t.client.clone(),
                ..Default::default()
            })
            .into(),
        csrf: p.csrf.clone(),
        ..Default::default()
    })
}
