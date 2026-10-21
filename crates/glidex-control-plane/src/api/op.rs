//! A directory's OpenID provider endpoints (spec/tenancy.md D54–D57),
//! under its issuer, HTTP only:
//!
//! - `GET /d/{dir}/authorize`: the code flow with PKCE (S256) for the
//!   built-in `gxctl` client and a loopback redirect (RFC 8252). The
//!   browser's session at this host approves it; without one, the web UI
//!   logs in first and comes back (`/?next=…`).
//! - `POST /d/{dir}/device_authorization`, and the web UI's
//!   `POST /d/{dir}/device/inspect` and `…/device/answer` (signed in).
//! - `POST /d/{dir}/token`: `authorization_code`, `refresh_token` and
//!   `device_code`; with `resource=urn:glidex:cluster:<id>`, an access
//!   token for that cluster (D57).
//! - `POST /d/{dir}/revoke`: ends a refresh token's chain.
//!
//! And [`access_principal`]: this cluster accepting its own directory's
//! access tokens (D56).

use super::{AppState, Caller, err};
use crate::auth::{Method, Principal, Transport};
use crate::directory::op::{
    self as op, CLUSTER_URN, CodeRequest, DeviceRequest, GXCTL, Grant, Poll, RefreshRefused,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use serde_json::{Value, json};
use std::collections::HashMap;

/// The `typ` of an access token (RFC 9068).
pub const ACCESS_TYP: &str = "at+jwt";
/// How long an ID token is good for.
const ID_TOKEN_SECS: u64 = 3600;

/// The id access tokens for this cluster carry as `aud`: its cluster id,
/// `local` for a standalone host.
pub fn cluster_id(app: &AppState) -> String {
    app.manager
        .cluster()
        .map(|c| c.identity.cluster_id.clone())
        .unwrap_or_else(|| crate::tenants::LOCAL_DIRECTORY.into())
}

/// Access tokens' lifetime (D55: default 1 h, at most 12 h).
pub fn access_ttl(app: &AppState) -> u64 {
    app.auth.config.auth.access_token_ttl_minutes.unwrap_or(60) * 60
}

/// An OAuth error (RFC 6749 §5.2).
fn oauth_err(status: StatusCode, code: &str, description: &str) -> Response {
    let mut r = (
        status,
        Json(json!({ "error": code, "error_description": description })),
    )
        .into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn no_store(v: Value) -> Response {
    let mut r = Json(v).into_response();
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Form or JSON fields.
fn fields(headers: &HeaderMap, body: &Bytes) -> HashMap<String, String> {
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.starts_with("application/json"));
    if json {
        return serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .map(|o| {
                o.into_iter()
                    .filter_map(|(k, v)| Some((k, v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
    }
    url::form_urlencoded::parse(body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn hosted(app: &AppState, dir: &str) -> Option<crate::tenants::Directory> {
    app.auth.tenants.get_directory(dir).ok().flatten()
}

fn now() -> u64 {
    crate::auth::store::now()
}

/// How the browser session logged in, as `amr` (RFC 8176).
fn grant_of(p: &Principal) -> Option<Grant> {
    let user = p.user.as_ref()?;
    let amr = match p.method {
        Method::Passkey => vec!["hwk".to_string(), "user".into(), "mfa".into()],
        Method::Oidc if p.mfa => vec!["fed".to_string(), "mfa".into()],
        Method::Oidc => vec!["fed".to_string()],
        Method::Directory if p.mfa => vec!["mfa".to_string()],
        _ => vec!["pwd".to_string()],
    };
    Some(Grant {
        user_id: user.id.clone(),
        auth_time: p.authenticated_at,
        amr,
        mfa: p.mfa,
    })
}

/// How long a new chain for `user` may last (D55, D20): 90 days when one
/// of the connectors they sign in through provisions them, else their
/// connectors' `token_max_days` (30 days by default).
fn chain_max_secs(app: &AppState, user: &str) -> u64 {
    let connectors = app.auth.connectors.list().unwrap_or_default();
    let idents = app.auth.store.identities_of(user).unwrap_or_default();
    let mine: Vec<_> = connectors
        .iter()
        .filter(|c| idents.iter().any(|i| i.provider == c.id))
        .collect();
    if mine
        .iter()
        .any(|c| c.provisioning != crate::directory::Provisioning::None)
    {
        return op::REFRESH_MAX_SECS;
    }
    let days = mine
        .iter()
        .map(|c| c.token_max_days())
        .min()
        .unwrap_or(crate::directory::DEFAULT_TOKEN_MAX_DAYS);
    (days * 86_400).min(op::REFRESH_MAX_SECS)
}

/// The cluster a `resource` names: only this one until the handshake
/// (D56); `Err` for anything else.
fn audience(app: &AppState, resource: Option<&String>) -> Result<Option<String>, Response> {
    let Some(r) = resource.filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    match r.strip_prefix(CLUSTER_URN) {
        Some(id) if id == cluster_id(app) => Ok(Some(id.to_string())),
        _ => Err(oauth_err(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "resource must be urn:glidex:cluster:<this cluster's id>",
        )),
    }
}

/// The token response for a chain: always a refresh token, an ID token
/// for `openid`, and an access token for `aud` when asked.
fn token_response(
    app: &AppState,
    dir: &str,
    refresh: (String, op::RefreshToken),
    aud: Option<String>,
    nonce: Option<String>,
) -> Response {
    let (secret, r) = refresh;
    let t = now();
    let user = app.auth.store.user(&r.user_id).ok().flatten();
    let mut out = json!({ "token_type": "Bearer", "refresh_token": secret, "scope": r.scope });
    if r.scope.split(' ').any(|s| s == "openid") {
        let mut id = json!({
            "sub": r.user_id, "aud": r.client_id, "iat": t, "exp": t + ID_TOKEN_SECS,
            "auth_time": r.grant_info.auth_time, "amr": r.grant_info.amr, "sid": r.family,
        });
        if let Some(u) = &user {
            id["name"] = u.display_name.clone().into();
        }
        if let Some(n) = nonce {
            id["nonce"] = n.into();
        }
        match super::issuer::sign(app, dir, "JWT", id) {
            Ok(j) => out["id_token"] = j.into(),
            Err(e) => return e.into_response(),
        }
    }
    if let Some(aud) = aud {
        let ttl = access_ttl(app);
        let claims = json!({
            "sub": r.user_id, "aud": aud, "client_id": r.client_id, "iat": t, "nbf": t,
            "exp": t + ttl, "jti": crate::auth::random_token(16), "sid": r.family,
            "auth_time": r.grant_info.auth_time, "amr": r.grant_info.amr,
            "scope": r.scope, "glidex": { "directory": dir, "mfa": r.grant_info.mfa },
        });
        match super::issuer::sign(app, dir, ACCESS_TYP, claims) {
            Ok(j) => {
                out["access_token"] = j.into();
                out["expires_in"] = ttl.into();
            }
            Err(e) => return e.into_response(),
        }
    }
    no_store(out)
}

// ---- authorize -------------------------------------------------------------

/// `GET /d/{dir}/authorize` (RFC 6749 §4.1, RFC 7636, RFC 8252).
pub async fn authorize(
    State(app): State<AppState>,
    Path(dir): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if hosted(&app, &dir).is_none() {
        return err(StatusCode::NOT_FOUND, "not_found", "no such directory").into_response();
    }
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
    // Until the redirect is known good, errors are shown, not redirected.
    if get("client_id") != GXCTL {
        return oauth_err(
            StatusCode::BAD_REQUEST,
            "invalid_client",
            "unknown client_id",
        );
    }
    let redirect_uri = get("redirect_uri");
    if !op::loopback_redirect(redirect_uri) {
        return oauth_err(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri must be http://127.0.0.1:<port>/… or http://[::1]:<port>/…",
        );
    }
    let back = |params: &[(&str, &str)]| {
        let mut u = url::Url::parse(redirect_uri).expect("checked");
        {
            let mut qp = u.query_pairs_mut();
            for (k, v) in params {
                qp.append_pair(k, v);
            }
            if let Some(s) = q.get("state") {
                qp.append_pair("state", s);
            }
        }
        Redirect::to(u.as_str()).into_response()
    };
    if get("response_type") != "code" {
        return back(&[("error", "unsupported_response_type")]);
    }
    if get("code_challenge_method") != "S256" || !(43..=128).contains(&get("code_challenge").len())
    {
        return back(&[
            ("error", "invalid_request"),
            ("error_description", "PKCE with S256 is required"),
        ]);
    }
    let scope = match get("scope") {
        "" => "openid".to_string(),
        s => s.to_string(),
    };
    // The browser's session at this host approves it.
    let session = super::cookie(&headers, crate::auth::SESSION_COOKIE).and_then(|c| {
        app.auth
            .session_principal(&c, Transport::Tcp, None)
            .ok()
            .flatten()
    });
    let Some(grant) = session.as_ref().and_then(grant_of) else {
        let here = format!("/d/{dir}/authorize?{}", encode_query(&q));
        let next: String = url::form_urlencoded::byte_serialize(here.as_bytes()).collect();
        return Redirect::to(&format!("/?next={next}")).into_response();
    };
    let code = app.auth.op_pending.issue_code(
        CodeRequest {
            directory: dir.clone(),
            client_id: GXCTL.into(),
            redirect_uri: redirect_uri.into(),
            code_challenge: get("code_challenge").into(),
            scope,
            nonce: q.get("nonce").cloned(),
        },
        grant,
        now(),
    );
    back(&[("code", &code)])
}

fn encode_query(q: &HashMap<String, String>) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    let mut keys: Vec<_> = q.keys().collect();
    keys.sort();
    for k in keys {
        s.append_pair(k, &q[k]);
    }
    s.finish()
}

// ---- device grant ------------------------------------------------------------

/// `POST /d/{dir}/device_authorization` (RFC 8628 §3.1).
pub async fn device_authorization(
    State(app): State<AppState>,
    Path(dir): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(d) = hosted(&app, &dir) else {
        return err(StatusCode::NOT_FOUND, "not_found", "no such directory").into_response();
    };
    let f = fields(&headers, &body);
    if f.get("client_id").map(String::as_str) != Some(GXCTL) {
        return oauth_err(
            StatusCode::BAD_REQUEST,
            "invalid_client",
            "unknown client_id",
        );
    }
    let Some(iss) = super::issuer::issuer_url(&app.auth, &app.manager, &d) else {
        return oauth_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "no issuer",
        );
    };
    let scope = f
        .get("scope")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "openid".into());
    let (device_code, user_code) = app.auth.op_pending.start_device(
        DeviceRequest {
            directory: dir.clone(),
            client_id: GXCTL.into(),
            scope,
        },
        now(),
    );
    // The web UI's page for it, at the issuer's origin.
    let base = iss.strip_suffix(&format!("/d/{dir}")).unwrap_or(&iss);
    let page = format!("{base}/device");
    no_store(json!({
        "device_code": device_code,
        "user_code": user_code,
        "verification_uri": page,
        "verification_uri_complete": format!("{page}?directory={dir}&code={user_code}"),
        "expires_in": op::DEVICE_SECS,
        "interval": op::DEVICE_INTERVAL,
    }))
}

#[derive(serde::Deserialize)]
pub struct DeviceAnswer {
    user_code: String,
    #[serde(default)]
    approve: bool,
}

/// The device page's caller: a person signed in to this host, never a
/// token (a token approving a login would make itself a refresh token).
fn device_caller(c: &Caller) -> Result<Grant, super::ApiErr> {
    if c.p.token.is_some() || c.p.method == Method::Token {
        return Err(err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "approve a device login from a browser session",
        ));
    }
    grant_of(&c.p).ok_or_else(|| err(StatusCode::FORBIDDEN, "forbidden", "not a user"))
}

/// `POST /d/{dir}/device/inspect {user_code}`: what a code is for.
pub async fn device_inspect(
    c: Caller,
    Path(dir): Path<String>,
    Json(b): Json<DeviceAnswer>,
) -> Result<Json<Value>, super::ApiErr> {
    device_caller(&c)?;
    match c.auth().op_pending.device_request(&b.user_code, now()) {
        Some(r) if r.directory == dir => Ok(Json(json!({
            "client_id": r.client_id, "client_name": "gxctl (command line)", "scope": r.scope,
        }))),
        _ => Err(err(
            StatusCode::NOT_FOUND,
            "not_found",
            "no login waits for this code",
        )),
    }
}

/// `POST /d/{dir}/device/answer {user_code, approve}`.
pub async fn device_answer(
    c: Caller,
    Path(dir): Path<String>,
    Json(b): Json<DeviceAnswer>,
) -> Result<Json<Value>, super::ApiErr> {
    let grant = device_caller(&c)?;
    let pending = &c.auth().op_pending;
    if pending
        .device_request(&b.user_code, now())
        .is_none_or(|r| r.directory != dir)
    {
        return Err(err(
            StatusCode::NOT_FOUND,
            "not_found",
            "no login waits for this code",
        ));
    }
    pending.answer_device(&b.user_code, b.approve.then_some(grant), now());
    Ok(Json(json!({ "approved": b.approve })))
}

// ---- token and revoke --------------------------------------------------------

/// `POST /d/{dir}/token` (RFC 6749 §3.2).
pub async fn token(
    State(app): State<AppState>,
    Path(dir): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if hosted(&app, &dir).is_none() {
        return err(StatusCode::NOT_FOUND, "not_found", "no such directory").into_response();
    }
    let f = fields(&headers, &body);
    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
    if get("client_id") != GXCTL {
        return oauth_err(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "unknown client_id",
        );
    }
    let aud = match audience(&app, f.get("resource")) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let bad_grant = |m: &str| oauth_err(StatusCode::BAD_REQUEST, "invalid_grant", m);
    let t = now();
    let start = |grant: Grant, scope: &str, nonce: Option<String>| {
        if !app
            .auth
            .store
            .user(&grant.user_id)
            .ok()
            .flatten()
            .is_some_and(|u| !u.disabled)
        {
            return bad_grant("the user can't log in");
        }
        let max = chain_max_secs(&app, &grant.user_id);
        match app
            .auth
            .refresh_tokens
            .start(&dir, GXCTL, scope, &grant, max, t)
        {
            Ok(r) => {
                app.auth.audit_system(
                    &format!("directory:{dir}"),
                    "",
                    "directoryLogin",
                    Some(&format!("user:{}", grant.user_id)),
                    "allowed",
                    None,
                    json!({ "client_id": GXCTL, "grant_type": get("grant_type"), "sid": r.1.family }),
                );
                token_response(&app, &dir, r, aud.clone(), nonce)
            }
            Err(e) => {
                err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()).into_response()
            }
        }
    };
    match get("grant_type") {
        "authorization_code" => {
            let Some((req, grant)) = app.auth.op_pending.take_code(get("code"), t) else {
                return bad_grant("unknown, used or expired code");
            };
            if req.directory != dir
                || req.client_id != get("client_id")
                || req.redirect_uri != get("redirect_uri")
                || !op::pkce_ok(&req.code_challenge, get("code_verifier"))
            {
                return bad_grant("the code doesn't match this request");
            }
            start(grant, &req.scope, req.nonce)
        }
        "urn:ietf:params:oauth:grant-type:device_code" => {
            match app
                .auth
                .op_pending
                .poll(get("device_code"), get("client_id"), t)
            {
                Poll::Approved(grant, req) if req.directory == dir => {
                    start(grant, &req.scope, None)
                }
                Poll::Approved(..) | Poll::Expired => oauth_err(
                    StatusCode::BAD_REQUEST,
                    "expired_token",
                    "the device code expired",
                ),
                Poll::Pending => oauth_err(
                    StatusCode::BAD_REQUEST,
                    "authorization_pending",
                    "not answered yet",
                ),
                Poll::SlowDown => oauth_err(StatusCode::BAD_REQUEST, "slow_down", "ask less often"),
                Poll::Denied => oauth_err(
                    StatusCode::BAD_REQUEST,
                    "access_denied",
                    "the login was denied",
                ),
            }
        }
        "refresh_token" => {
            let rotated =
                match app
                    .auth
                    .refresh_tokens
                    .rotate(get("refresh_token"), get("client_id"), t)
                {
                    Ok(r) => r,
                    Err(e) => {
                        return err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string())
                            .into_response();
                    }
                };
            match rotated {
                Ok((secret, r)) if r.directory_id == dir => {
                    if !app
                        .auth
                        .store
                        .user(&r.user_id)
                        .ok()
                        .flatten()
                        .is_some_and(|u| !u.disabled)
                    {
                        let _ = app.auth.refresh_tokens.end_family(&r.family);
                        return bad_grant("the user can't log in");
                    }
                    token_response(&app, &dir, (secret, r), aud, None)
                }
                Ok(_) => bad_grant("not this directory's token"),
                Err(RefreshRefused::Reused) => {
                    tracing::warn!(directory = %dir, "a replaced refresh token came back: its chain was ended");
                    app.auth.audit_system(
                        &format!("directory:{dir}"),
                        "",
                        "directoryRefresh",
                        None,
                        "denied",
                        Some("refresh_reused"),
                        json!({ "client_id": GXCTL }),
                    );
                    bad_grant("the refresh token was already used; its chain is ended")
                }
                Err(_) => bad_grant("unknown or expired refresh token"),
            }
        }
        _ => oauth_err(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "authorization_code, refresh_token or device_code",
        ),
    }
}

/// `POST /d/{dir}/revoke` (RFC 7009): ends the chain of a refresh token;
/// `200` either way.
pub async fn revoke(
    State(app): State<AppState>,
    Path(_dir): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let f = fields(&headers, &body);
    if let Some(t) = f.get("token").filter(|t| t.starts_with(op::REFRESH_PREFIX)) {
        let _ = app.auth.refresh_tokens.revoke(t);
    }
    StatusCode::OK.into_response()
}

// ---- this cluster accepting access tokens (D56) -------------------------------

/// The principal for `jwt`, an access token of a directory hosted here
/// addressed to this cluster; `None` for anything else.
pub fn access_principal(
    app: &AppState,
    jwt: &str,
    transport: Transport,
    ip: Option<std::net::IpAddr>,
) -> Option<Principal> {
    let header = jsonwebtoken::decode_header(jwt).ok()?;
    if header.typ.as_deref() != Some(ACCESS_TYP) {
        return None;
    }
    let kid = header.kid?;
    let (dir, _) = kid.split_once(':')?;
    let d = hosted(app, dir)?;
    let iss = d.issuer?;
    let t = now();
    let key = app
        .auth
        .connectors
        .signing_keys(dir)
        .ok()?
        .into_iter()
        .find(|k| k.kid == kid && k.published(t))?;
    let dk = jsonwebtoken::DecodingKey::from_ec_components(&key.x, &key.y).ok()?;
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
    v.set_audience(&[cluster_id(app)]);
    v.set_issuer(&[iss]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    v.leeway = 30;
    let c = jsonwebtoken::decode::<Value>(jwt, &dk, &v).ok()?.claims;
    // Ended chains end their access tokens here at once.
    let sid = c["sid"].as_str()?;
    if !app.auth.refresh_tokens.family_alive(sid, t).ok()? {
        return None;
    }
    let user = app.auth.store.user(c["sub"].as_str()?).ok()??;
    if user.disabled {
        return None;
    }
    let teams = app.auth.store.teams_of(&user.id).ok()?;
    Some(Principal {
        directory: user.directory_id.clone(),
        user: Some(user),
        token: None,
        token_narrowed: false,
        teams,
        method: Method::Directory,
        transport,
        authenticated_at: c["auth_time"].as_u64().unwrap_or(0),
        source_ip: ip,
        session_hash: None,
        csrf: None,
        peer_uid: None,
        mfa: c["glidex"]["mfa"].as_bool().unwrap_or(false),
        federated: None,
    })
}
