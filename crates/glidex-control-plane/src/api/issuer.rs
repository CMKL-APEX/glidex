//! A directory's discovery document and JWKS (spec/19-tenancy.md D52, D53),
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
    app: &super::App,
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

/// `dir`'s trust domain (29-workload-identity.md W13): the one it has, else
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

/// A running VM as its tokens see it (29-workload-identity.md §4.2).
#[derive(Debug, Clone)]
pub struct ResolvedVm {
    /// Its tenant's directory, which signs its tokens.
    pub directory: String,
    pub tenant_id: String,
    pub identity: crate::directory::workload::VmIdentity,
}

/// Who running VM `vm_id` is, for its tokens. `409 not_running` unless it
/// runs (W4).
pub async fn vm_identity(app: &super::App, vm_id: &str) -> Result<ResolvedVm, super::ApiErr> {
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
    let service_account = vm.config().service_account.clone();
    Ok(ResolvedVm {
        directory: tenant.directory_id,
        tenant_id: tenant.id,
        identity: crate::directory::workload::VmIdentity {
            tenant: tenant.slug,
            project: project.name,
            vm_id: vm.id,
            vm_name: vm.name,
            cluster: super::op::cluster_id(app),
            service_account,
        },
    })
}

/// A glidex token for `vm`'s service account (29-workload-identity.md W6): a
/// federated token (19-tenancy.md D48) from origin `vm:<tenant>`, for 15 min,
/// never past the service account's expiry. The secret and its lifetime.
/// `404 no_service_account` when the VM has none, or it is gone, expired,
/// or not its tenant's.
pub fn vm_glidex_token(
    app: &super::App,
    vm: &ResolvedVm,
    replay: Option<String>,
) -> Result<(String, u64), super::ApiErr> {
    use crate::auth::store::TokenKind;
    use crate::directory::workload;
    let internal = |m: String| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", m);
    let none = |m: &str| err(StatusCode::NOT_FOUND, "no_service_account", m);
    let id = &vm.identity;
    let sa_id = id
        .service_account
        .as_deref()
        .ok_or_else(|| none("this VM has no service account"))?;
    let now = crate::auth::store::now();
    let sa = app
        .auth
        .store
        .token_by_id(sa_id)
        .map_err(|e| internal(e.to_string()))?
        .map(|(_, t)| t)
        .filter(|t| t.expires_at > now)
        .ok_or_else(|| none("the VM's service account is gone or expired"))?;
    let TokenKind::ServiceAccount { project } = &sa.kind else {
        return Err(none("the VM's service account isn't a service account"));
    };
    let ours = app
        .manager
        .projects
        .get(project)
        .map_err(|e| internal(e.to_string()))?
        .is_some_and(|p| p.tenant_id == vm.tenant_id);
    if !ours {
        return Err(none("the VM's service account isn't its tenant's"));
    }
    let ttl = workload::GLIDEX_TOKEN_TTL_SECS.min(sa.expires_at - now);
    let td = app
        .auth
        .tenants
        .get_directory(&vm.directory)
        .ok()
        .flatten()
        .and_then(|d| trust_domain(&app.auth, &app.manager, &d))
        .unwrap_or_default();
    let secret = format!(
        "{}{}",
        crate::auth::TOKEN_PREFIX,
        crate::auth::random_token(32)
    );
    let token_id = format!("vmt-{}", uuid::Uuid::new_v4().simple());
    let rec = crate::directory::federation::FederatedToken {
        id: token_id.clone(),
        federation: format!("{}{}", workload::VM_ORIGIN, vm.tenant_id),
        rule: format!("vm:{}", id.vm_id),
        service_account: sa.id.clone(),
        subject: workload::spiffe_id(&td, id),
        claims: [
            ("vm".to_string(), id.vm_id.clone()),
            ("cluster".to_string(), id.cluster.clone()),
        ]
        .into(),
        // From the metadata endpoint each is new; an exchanged identity
        // token only once (§5).
        replay_key: replay.unwrap_or_else(|| format!("vm:{token_id}")),
        subject_expires_at: now + ttl,
        created_at: now,
        expires_at: now + ttl,
    };
    app.auth
        .federated_tokens
        .mint(&glidex_auth::token::token_hash(&secret), &rec)
        .map_err(|e| match e {
            crate::directory::federation::FederationError::Conflict(m) => {
                err(StatusCode::CONFLICT, "replayed", m)
            }
            e => internal(e.to_string()),
        })?;
    app.auth.audit_system(
        &format!("system:vm:{}", id.vm_id),
        &token_id,
        "issueVmToken",
        Some(&format!("token:{}", sa.id)),
        "ok",
        None,
        serde_json::json!({ "vm": id.vm_id, "cluster": id.cluster, "service_account": sa.id, "token": token_id }),
    );
    Ok((secret, ttl))
}

/// An identity token for `id` addressed to `audience`, signed by `dir`
/// (29-workload-identity.md §4.2): `ttl` seconds (§4.4: 10 min to 1 h, default
/// 15 min). Its first one fixes the directory's trust domain (W13).
pub fn sign_identity(
    app: &super::App,
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

/// The claims of `jwt` if it is an identity token of directory `dir` for
/// this cluster (29-workload-identity.md §5): typed `JWT`, signed by one of the
/// directory's published keys, its issuer, `aud` this cluster, unexpired.
pub fn verify_identity(app: &super::App, dir: &str, jwt: &str) -> Result<Value, String> {
    use crate::directory::workload;
    let header = jsonwebtoken::decode_header(jwt).map_err(|e| format!("not a JWT: {e}"))?;
    if header.typ.as_deref() != Some(workload::TYP) {
        return Err("not an identity token".into());
    }
    let d = app
        .auth
        .tenants
        .get_directory(dir)
        .map_err(|e| e.to_string())?
        .ok_or("no such directory")?;
    let iss = d.issuer.clone().ok_or("the directory has issued nothing")?;
    let now = crate::auth::store::now();
    let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(
        app.auth
            .connectors
            .jwks(dir, now)
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let jwk = header
        .kid
        .as_deref()
        .and_then(|k| jwks.find(k))
        .ok_or("signed by no key of the directory")?;
    let key = jsonwebtoken::DecodingKey::from_jwk(jwk).map_err(|e| e.to_string())?;
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
    v.set_issuer(&[iss.as_str()]);
    v.set_audience(&[super::op::cluster_id(app)]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    v.leeway = 60;
    jsonwebtoken::decode::<Value>(jwt, &key, &v)
        .map(|t| t.claims)
        .map_err(|e| format!("not valid here: {e}"))
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
