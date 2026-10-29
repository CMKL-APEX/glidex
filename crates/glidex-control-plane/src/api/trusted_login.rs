//! Logging in to this cluster's web UI at a trusted directory
//! (spec/19-tenancy.md D69): the cluster is that directory's OIDC client
//! `cluster:<id>`.
//!
//! - `GET /auth/t/{trusted}/start`: to the directory's `authorize` with
//!   PKCE, the cluster's callback (registered at the handshake) and a
//!   cookie binding the login to this browser;
//! - `GET /auth/t/{trusted}/callback`: the code exchanged at the
//!   directory's token endpoint over its pinned certificate, signed with
//!   the cluster key (`private_key_jwt`); the ID token checked against the
//!   directory's keys of its last feed; a **cluster session** for its user
//!   in the replica, bound to the ID token's `sid` (it ends when the feed
//!   says that chain ended, D68).

use super::access::{first_origin, secure_transport, session_cookie};
use super::{ApiErr, AppState, Listener, RequestId, err};
use crate::auth::Method;
use crate::directory::op::CLUSTER_CLIENT;
use crate::directory::peer;
use crate::service::login::audit_login;
use crate::trust::{PendingLogin, TrustedDirectory};
use axum::{
    Extension,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

const COOKIE: &str = "gx_tlogin";

fn now() -> u64 {
    crate::auth::store::now()
}

/// This cluster's callback for logins at trusted directory `trusted`: on
/// its published web UI address, else its first allowed origin.
pub fn callback_url(app: &AppState, trusted: &str) -> Option<String> {
    let base = app
        .manager
        .ui_url()
        .or_else(|| first_origin(app).map(String::from))?;
    Some(format!(
        "{}/api/auth/t/{trusted}/callback",
        base.trim_end_matches('/')
    ))
}

/// This cluster as the directories' client.
fn client_id(app: &AppState) -> String {
    format!("{CLUSTER_CLIENT}{}", super::op::cluster_id(app))
}

fn trusted(app: &AppState, id: &str) -> Result<TrustedDirectory, ApiErr> {
    app.auth
        .trust
        .directories()
        .ok()
        .and_then(|ds| ds.into_iter().find(|d| d.id == id))
        .ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                "not_found",
                "no such trusted directory",
            )
        })
}

#[derive(Deserialize, Default)]
pub struct StartQuery {
    #[serde(default)]
    return_to: Option<String>,
}

/// `GET /auth/t/{trusted}/start`.
pub async fn start(
    State(app): State<AppState>,
    Path(id): Path<String>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Query(q): Query<StartQuery>,
) -> Result<Response, ApiErr> {
    use sha2::Digest as _;
    let d = trusted(&app, &id)?;
    let callback = callback_url(&app, &d.id).ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "no_ui_address",
            "this cluster has no web UI address for the directory to send you back to",
        )
    })?;
    let state = crate::auth::random_token(24);
    let nonce = crate::auth::random_token(24);
    let verifier = crate::auth::random_token(48);
    let browser = crate::auth::random_token(24);
    let challenge = {
        use base64::Engine as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()))
    };
    app.auth.trusted_logins.put(
        state.clone(),
        PendingLogin {
            trusted: d.id.clone(),
            nonce: nonce.clone(),
            verifier,
            return_to: crate::directory::oidc::safe_return_to(q.return_to.as_deref()),
            browser: browser.clone(),
            created: now(),
        },
    );
    let mut url = url::Url::parse(&format!("{}/authorize", d.issuer))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id(&app))
        .append_pair("redirect_uri", &callback)
        .append_pair("scope", "openid")
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    let secure = secure_transport(
        &app,
        &headers,
        listener.map(|l| l.0).unwrap_or(Listener::Tcp),
    );
    // Lax: it must come back on the directory's top-level redirect.
    let mut c = format!(
        "{COOKIE}={browser}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}",
        crate::trust::LOGIN_SECS
    );
    if secure {
        c.push_str("; Secure");
    }
    let mut resp = Redirect::to(url.as_str()).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&c).expect("ascii"),
    );
    Ok(resp)
}

#[derive(Deserialize, Default)]
pub struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// The ID token's claims, if `jwt` is `d`'s for this cluster with `nonce`.
fn id_claims(app: &AppState, d: &TrustedDirectory, jwt: &str, nonce: &str) -> Option<Value> {
    let header = jsonwebtoken::decode_header(jwt).ok()?;
    let set: jsonwebtoken::jwk::JwkSet = serde_json::from_value(d.jwks.clone()).ok()?;
    let key = jsonwebtoken::DecodingKey::from_jwk(set.find(header.kid.as_deref()?)?).ok()?;
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
    v.set_audience(&[client_id(app)]);
    v.set_issuer(&[d.issuer.as_str()]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    v.leeway = 30;
    let c = jsonwebtoken::decode::<Value>(jwt, &key, &v).ok()?.claims;
    (c["nonce"].as_str() == Some(nonce) && c["sid"].is_string()).then_some(c)
}

/// `GET /auth/t/{trusted}/callback`.
pub async fn callback(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Extension(rid): Extension<RequestId>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Result<Response, ApiErr> {
    if let Some(e) = q.error {
        return Err(err(
            StatusCode::UNAUTHORIZED,
            "login_failed",
            format!(
                "the directory: {e}{}",
                q.error_description
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            ),
        ));
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "code and state are required",
        ));
    };
    let expired = || {
        err(
            StatusCode::BAD_REQUEST,
            "login_expired",
            "this login expired or began in another browser; start again",
        )
    };
    let p = app
        .auth
        .trusted_logins
        .take(&state, now())
        .ok_or_else(expired)?;
    let browser = super::cookie(&headers, COOKIE).unwrap_or_default();
    if p.trusted != id || !crate::auth::constant_eq(&browser, &p.browser) {
        return Err(expired());
    }
    let d = trusted(&app, &id)?;
    let callback = callback_url(&app, &d.id).ok_or_else(expired)?;
    let client = client_id(&app);
    let assertion = peer::client_assertion(&app.auth.connectors, &client, &d.issuer, now())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let (status, answer) = peer::post_unsigned(
        &d.api,
        &d.tls_sha256,
        &format!("/d/{}/token", d.directory),
        &json!({
            "grant_type": "authorization_code", "code": code, "redirect_uri": callback,
            "client_id": client, "code_verifier": p.verifier,
            "client_assertion_type": peer::CLIENT_ASSERTION_TYPE, "client_assertion": assertion,
        }),
    )
    .await
    .map_err(|e| err(StatusCode::BAD_GATEWAY, "directory_unreachable", e))?;
    let denied = |why: &str, who: &str| {
        audit_login(&app, None, &rid.0, "directory", "denied", who);
        err(StatusCode::UNAUTHORIZED, "login_failed", why.to_string())
    };
    if status != 200 {
        let e = answer["error_description"].as_str().unwrap_or("refused");
        return Err(denied(&format!("the directory: {e}"), &d.id));
    }
    let Some(c) = answer["id_token"]
        .as_str()
        .and_then(|j| id_claims(&app, &d, j, &p.nonce))
    else {
        return Err(denied("the directory's ID token doesn't verify", &d.id));
    };
    let sub = c["sub"].as_str().unwrap_or_default();
    // Its user as the last feed brought them.
    let user = match app.auth.store.user(sub).ok().flatten() {
        Some(u) if !u.disabled && u.directory_id == d.id => u,
        _ => {
            return Err(denied(
                "you aren't on this cluster (yet: it learns its trusted directories' members within a minute)",
                sub,
            ));
        }
    };
    let mfa = c["amr"]
        .as_array()
        .is_some_and(|a| a.iter().any(|m| m == "mfa" || m == "hwk"));
    let sid = c["sid"].as_str().map(String::from);
    let (cookie, _) = app
        .auth
        .create_session_from(&user, Method::Directory, mfa, sid)
        .map_err(super::auth_error)?;
    let principal = app
        .auth
        .session_principal(&cookie, crate::auth::Transport::Tcp, None)
        .map_err(super::auth_error)?;
    audit_login(
        &app,
        principal.as_ref(),
        &rid.0,
        "directory",
        "ok",
        &user.id,
    );
    let secure = secure_transport(
        &app,
        &headers,
        listener.map(|l| l.0).unwrap_or(Listener::Tcp),
    );
    let mut resp = Redirect::to(&p.return_to).into_response();
    resp.headers_mut()
        .append(header::SET_COOKIE, session_cookie(&cookie, secure, None));
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"
        ))
        .expect("ascii"),
    );
    Ok(resp)
}
