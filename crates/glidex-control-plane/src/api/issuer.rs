//! A directory's discovery document and JWKS (spec/tenancy.md D52, D53),
//! public: `/d/{dir}/.well-known/openid-configuration` and
//! `/d/{dir}/jwks.json`. glidex-ui serves `/d/…` from here.

use super::{AppState, err};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

/// Where `dir`'s issuer is (D52): the URL it was fixed at, else
/// `auth.issuer_base` or the web UI's published address, plus `/d/<dir>`.
/// `None` when neither is known.
pub fn issuer_url(
    auth: &crate::auth::AuthService,
    manager: &crate::state::VmManager,
    dir: &crate::tenants::Directory,
) -> Option<String> {
    if let Some(i) = &dir.issuer {
        return Some(i.clone());
    }
    let base = auth
        .config
        .auth
        .issuer_base
        .as_deref()
        .and_then(crate::config::issuer_base)
        .or_else(|| manager.ui_url())?;
    Some(format!("{}/d/{}", base.trim_end_matches('/'), dir.id))
}

/// `claims` signed by `dir` with `iss` set; the first token a directory
/// issues fixes its issuer URL (D52).
pub fn sign(
    app: &AppState,
    dir: &str,
    typ: &str,
    mut claims: Value,
) -> Result<String, super::ApiErr> {
    let internal = |m: String| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", m);
    let d = app
        .auth
        .tenants
        .get_directory(dir)
        .map_err(|e| internal(e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", "no such directory"))?;
    let iss = issuer_url(&app.auth, &app.manager, &d).ok_or_else(no_issuer)?;
    if d.issuer.is_none() {
        app.auth
            .tenants
            .set_issuer(dir, &iss, false)
            .map_err(|e| internal(e.to_string()))?;
    }
    claims["iss"] = iss.into();
    app.auth
        .connectors
        .sign_jwt(dir, typ, &claims)
        .map_err(|e| internal(e.to_string()))
}

/// `dir`'s trust domain (workload-identity.md W13): the one it has, else
/// `<dir>.<issuer host>`. `None` while it has no issuer URL.
pub fn trust_domain(
    auth: &crate::auth::AuthService,
    manager: &crate::state::VmManager,
    dir: &crate::tenants::Directory,
) -> Option<String> {
    if let Some(t) = &dir.trust_domain {
        return Some(t.clone());
    }
    let iss = issuer_url(auth, manager, dir)?;
    crate::directory::workload::default_trust_domain(&iss, &dir.id)
}

/// Who running VM `vm_id` is, and its directory, for its identity token
/// (workload-identity.md §4.2). `409 not_running` unless it runs (W4).
pub async fn vm_identity(
    app: &AppState,
    vm_id: &str,
) -> Result<(String, crate::directory::workload::VmIdentity), super::ApiErr> {
    let gone = || err(StatusCode::NOT_FOUND, "not_found", "no such VM");
    let internal = |m: String| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", m);
    let vm = app.manager.get_vm(vm_id).await.map_err(|_| gone())?;
    if vm.state() != crate::models::VmState::Running {
        return Err(err(
            StatusCode::CONFLICT,
            "not_running",
            "only a running VM has an identity",
        ));
    }
    let project = app
        .manager
        .projects
        .get(&vm.project)
        .map_err(|e| internal(e.to_string()))?
        .ok_or_else(gone)?;
    let tenant = app
        .auth
        .tenants
        .get(&project.tenant_id)
        .map_err(|e| internal(e.to_string()))?
        .ok_or_else(gone)?;
    let id = crate::directory::workload::VmIdentity {
        tenant: tenant.slug,
        project: project.name,
        vm_id: vm.id,
        vm_name: vm.name,
        cluster: super::op::cluster_id(app),
        service_account: None,
    };
    Ok((tenant.directory_id, id))
}

/// An identity token for `id` addressed to `audience`, signed by `dir`
/// (workload-identity.md §4.2): `ttl` seconds (§4.4: 10 min to 1 h, default
/// 15 min). Its first one fixes the directory's trust domain (W13).
pub fn sign_identity(
    app: &AppState,
    dir: &str,
    id: &crate::directory::workload::VmIdentity,
    audience: &str,
    ttl: Option<u64>,
) -> Result<String, super::ApiErr> {
    use crate::directory::workload;
    let invalid = |m: String| err(StatusCode::BAD_REQUEST, "invalid", m);
    let internal = |m: String| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", m);
    if !workload::valid_audience(audience) {
        return Err(invalid("audience is required".into()));
    }
    let ttl = workload::ttl(ttl).map_err(invalid)?;
    let d = app
        .auth
        .tenants
        .get_directory(dir)
        .map_err(|e| internal(e.to_string()))?
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "not_found", "no such directory"))?;
    let td = trust_domain(&app.auth, &app.manager, &d).ok_or_else(no_issuer)?;
    if d.trust_domain.is_none() {
        app.auth
            .tenants
            .set_trust_domain(dir, &td)
            .map_err(|e| internal(e.to_string()))?;
    }
    let now = crate::auth::store::now();
    let jti = uuid::Uuid::new_v4().to_string();
    let claims = workload::identity_claims(&td, id, audience, now, ttl, &jti);
    sign(app, dir, workload::TYP, claims)
}

fn no_issuer() -> super::ApiErr {
    err(
        StatusCode::SERVICE_UNAVAILABLE,
        "no_issuer",
        "this host has no public address for its directories yet: set auth.issuer_base",
    )
}

fn directory(app: &AppState, id: &str) -> Result<crate::tenants::Directory, Response> {
    match app.auth.tenants.get_directory(id) {
        Ok(Some(d)) => Ok(d),
        Ok(None) => {
            Err(err(StatusCode::NOT_FOUND, "not_found", "no such directory").into_response())
        }
        Err(e) => {
            Err(err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()).into_response())
        }
    }
}

/// Relying parties may keep these a few minutes; a rotation publishes the
/// new key before it signs anything that matters to them.
fn cached(v: Value) -> Response {
    let mut r = Json(v).into_response();
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    r
}

/// `GET /d/{dir}/.well-known/openid-configuration`.
pub async fn discovery(State(app): State<AppState>, Path(dir): Path<String>) -> Response {
    let d = match directory(&app, &dir) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let Some(iss) = issuer_url(&app.auth, &app.manager, &d) else {
        return no_issuer().into_response();
    };
    cached(json!({
        "issuer": iss,
        "jwks_uri": format!("{iss}/jwks.json"),
        "authorization_endpoint": format!("{iss}/authorize"),
        "token_endpoint": format!("{iss}/token"),
        "device_authorization_endpoint": format!("{iss}/device_authorization"),
        "revocation_endpoint": format!("{iss}/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": [
            "authorization_code", "refresh_token", "urn:ietf:params:oauth:grant-type:device_code"
        ],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": [crate::directory::issuer::ALG],
        "scopes_supported": ["openid", "offline_access"],
    }))
}

/// `GET /d/{dir}/jwks.json`.
pub async fn jwks(State(app): State<AppState>, Path(dir): Path<String>) -> Response {
    if let Err(r) = directory(&app, &dir) {
        return r;
    }
    match app.auth.connectors.jwks(&dir, crate::auth::store::now()) {
        Ok(v) => cached(v),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()).into_response(),
    }
}

// ---- REST twins of glidex.directory.v1.SigningKeyService -----------------

/// `GET /directory/keys`.
pub async fn list_keys(
    c: super::Caller,
) -> Result<Json<glidex_api::proto::glidex::directory::v1::ListSigningKeysResponse>, super::ApiErr>
{
    Ok(Json(crate::service::signing_keys::list_signing_keys(
        &c,
        Default::default(),
    )?))
}

/// `POST /directory/keys/rotate`.
pub async fn rotate_key(
    c: super::Caller,
) -> Result<Json<glidex_api::proto::glidex::directory::v1::RotateSigningKeyResponse>, super::ApiErr>
{
    Ok(Json(crate::service::signing_keys::rotate_signing_key(
        &c,
        Default::default(),
    )?))
}
