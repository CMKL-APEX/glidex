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
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": [crate::directory::issuer::ALG],
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
