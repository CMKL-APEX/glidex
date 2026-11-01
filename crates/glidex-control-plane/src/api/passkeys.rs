//! The passkey ceremonies (spec/25-passkeys.md §2, P2–P5): HTTP only, as
//! WebAuthn's own JSON. Registration is a signed-in user's step-up action;
//! sign-in is public, and every refusal is the same `401
//! passkey_refused`, with the reason in the audit log.

use super::{ApiErr, AppState, Caller, ClientAddr, Listener, RequestId, err};
use crate::auth::passkeys::{self as pk, CredentialJson, Purpose};
use crate::auth::{self, Method};
use crate::authz::{Ent, EntitySet};
use crate::service::login::audit_login;
use axum::{
    Extension, Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

const COOKIE: &str = "glidex_webauthn";

fn refused() -> ApiErr {
    err(
        StatusCode::UNAUTHORIZED,
        "passkey_refused",
        "the passkey can't be used to sign in here",
    )
}

/// The ceremony's origin and RP id (P2): the request's own `https`
/// origin, under `auth.passkeys.rp_id` (or its host when that is unset).
fn origin_and_rp(app: &AppState, headers: &HeaderMap) -> Result<(String, String), ApiErr> {
    let cfg = &app.auth.config.auth.passkeys;
    if !cfg.enabled {
        return Err(err(
            StatusCode::NOT_FOUND,
            "passkeys_disabled",
            "passkeys are turned off here",
        ));
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            err(
                StatusCode::BAD_REQUEST,
                "invalid",
                "a passkey ceremony comes from a browser page (Origin)",
            )
        })?;
    // WebAuthn's secure contexts: https pages, and `http://localhost`.
    let local = origin
        .strip_prefix("http://")
        .filter(|h| *h == "localhost" || h.starts_with("localhost:"));
    let host = origin.strip_prefix("https://").or(local).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "passkeys need an https page",
        )
    })?;
    let host = host
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host)
        .to_ascii_lowercase();
    let rp = cfg
        .rp_id
        .clone()
        .map(|r| r.to_ascii_lowercase())
        .unwrap_or_else(|| host.clone());
    if !(host == rp || host.ends_with(&format!(".{rp}"))) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            format!("this page ({host}) isn't under the passkeys' domain {rp}"),
        ));
    }
    Ok((origin.to_string(), rp))
}

fn cookie_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(&format!("{COOKIE}="))
                .map(String::from)
        })
}

fn with_cookie(mut r: Response, value: &str, max_age: u64, secure: bool) -> Response {
    let mut c = format!("{COOKIE}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}");
    if secure {
        c.push_str("; Secure");
    }
    r.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&c).expect("cookie is ASCII"),
    );
    r
}

fn secure(headers: &HeaderMap, listener: Option<Extension<Listener>>, app: &AppState) -> bool {
    match listener.map(|l| l.0).unwrap_or(Listener::Tcp) {
        Listener::Ui | Listener::Relay => {
            headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                == Some("https")
        }
        Listener::Tcp => app.auth.config.tls.enabled(),
        _ => false,
    }
}

#[derive(Deserialize)]
pub struct StartRegistration {
    #[serde(default)]
    name: String,
}

/// `POST /auth/passkeys/register/start {name}`: one's own, step-up (P4).
pub async fn register_start(
    c: Caller,
    headers: HeaderMap,
    listener: Option<Extension<Listener>>,
    Json(body): Json<StartRegistration>,
) -> Result<Response, ApiErr> {
    let user =
        c.p.user
            .clone()
            .ok_or_else(|| err(StatusCode::FORBIDDEN, "forbidden", "a user adds a passkey"))?;
    if c.p.token.is_some() {
        return Err(err(
            StatusCode::FORBIDDEN,
            "forbidden",
            "a passkey is added from a browser session",
        ));
    }
    c.require_action(
        "manageOwnSignIns",
        Ent::User(user.id.clone()),
        EntitySet::new(),
        &[],
    )?;
    let name = body.name.trim();
    let name = if name.is_empty() { "Passkey" } else { name };
    if name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "a passkey's name is up to 64 printable characters",
        ));
    }
    let (origin, rp) = origin_and_rp(&c.app, &headers)?;
    let pks = &c.app.auth.passkeys;
    let mine = pks
        .list(&user.id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    if mine.len() >= pk::MAX_PER_USER {
        return Err(err(
            StatusCode::CONFLICT,
            "too_many_passkeys",
            format!(
                "a user has at most {} passkeys: remove one first",
                pk::MAX_PER_USER
            ),
        ));
    }
    let handle = pks
        .handle(&user.id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let (cookie, cer) = c.app.auth.ceremonies.start(
        Purpose::Register {
            user_id: user.id.clone(),
            name: name.to_string(),
        },
        &rp,
        &origin,
    );
    let login_name = c
        .app
        .auth
        .store
        .identities_of(&user.id)
        .ok()
        .and_then(|ids| ids.into_iter().find_map(|i| i.email))
        .unwrap_or_else(|| user.display_name.clone());
    let opts = pk::creation_options(
        &cer,
        &c.app.auth.config.auth.passkeys.rp_name,
        &handle,
        &login_name,
        &user.display_name,
        &mine,
    );
    Ok(with_cookie(
        Json(opts).into_response(),
        &cookie,
        pk::CEREMONY_SECS,
        secure(&headers, listener, &c.app),
    ))
}

/// `POST /auth/passkeys/register/finish {credential}`.
pub async fn register_finish(
    c: Caller,
    headers: HeaderMap,
    listener: Option<Extension<Listener>>,
    Json(cred): Json<CredentialJson>,
) -> Result<Response, ApiErr> {
    let user =
        c.p.user
            .clone()
            .ok_or_else(|| err(StatusCode::FORBIDDEN, "forbidden", "a user adds a passkey"))?;
    let bad = |m: String| err(StatusCode::BAD_REQUEST, "passkey_not_added", m);
    let cer = cookie_of(&headers)
        .and_then(|k| c.app.auth.ceremonies.take(&k))
        .ok_or_else(|| bad("the passkey prompt expired: start again".into()))?;
    let Purpose::Register { user_id, name } = cer.purpose.clone() else {
        return Err(bad("this isn't a registration".into()));
    };
    if user_id != user.id {
        return Err(bad("the registration was started by another user".into()));
    }
    let p =
        pk::finish_registration(&cer, &cred, &user.id, &name).map_err(|e| bad(e.to_string()))?;
    let store = &c.app.auth.store;
    if c.app.auth.passkeys.get(&p.id).ok().flatten().is_some()
        || store.identity(pk::PROVIDER, &p.id).ok().flatten().is_some()
    {
        return Err(bad("this passkey is registered already".into()));
    }
    store
        .put_identity(&auth::store::Identity {
            provider: pk::PROVIDER.into(),
            subject: p.id.clone(),
            user_id: user.id.clone(),
            email: None,
            created_at: p.created_at,
        })
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    c.app
        .auth
        .passkeys
        .put(&p)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    c.set_target(format!("user:{}", user.id));
    c.detail(
        "passkey",
        serde_json::json!({ "id": p.id, "name": p.name, "alg": p.alg }),
    );
    let body = serde_json::json!({ "passkey": { "id": p.id, "name": p.name } });
    let r = (StatusCode::CREATED, Json(body)).into_response();
    Ok(with_cookie(r, "", 0, secure(&headers, listener, &c.app)))
}

/// `POST /auth/passkeys/login/start`.
pub async fn login_start(
    State(app): State<AppState>,
    headers: HeaderMap,
    listener: Option<Extension<Listener>>,
) -> Result<Response, ApiErr> {
    let (origin, rp) = origin_and_rp(&app, &headers)?;
    let (cookie, cer) = app.auth.ceremonies.start(Purpose::Login, &rp, &origin);
    let r = Json(pk::request_options(&cer)).into_response();
    Ok(with_cookie(
        r,
        &cookie,
        pk::CEREMONY_SECS,
        secure(&headers, listener, &app),
    ))
}

/// `POST /auth/passkeys/login/finish {credential}`: a session (method
/// `passkey`, multi-factor, P3), as at any login.
pub async fn login_finish(
    State(app): State<AppState>,
    Extension(rid): Extension<RequestId>,
    listener: Option<Extension<Listener>>,
    addr: Option<Extension<ClientAddr>>,
    headers: HeaderMap,
    Json(cred): Json<CredentialJson>,
) -> Result<Response, ApiErr> {
    // X-Forwarded-For counts only from glidex-ui (or a relaying server):
    // over TCP the peer is the client, whatever the header says.
    let client = super::client_ip(
        addr.map(|a| a.0.0),
        &headers,
        listener.as_ref().map_or(Listener::Tcp, |l| l.0),
    )
    .map_or_else(|| "-".into(), |ip| ip.to_string());
    if !app.auth.ceremonies.attempt(&client) {
        return Err(err(
            StatusCode::TOO_MANY_REQUESTS,
            "slow_down",
            "too many sign-in attempts: wait a minute",
        ));
    }
    let deny = |why: &str| {
        audit_login(&app, None, &rid.0, "passkey", "denied", why);
        refused()
    };
    let Some(cer) = cookie_of(&headers).and_then(|k| app.auth.ceremonies.take(&k)) else {
        return Err(deny("no ceremony, or it expired"));
    };
    if !matches!(cer.purpose, Purpose::Login) {
        return Err(deny("not a sign-in ceremony"));
    }
    let Ok(Some(stored)) = app.auth.passkeys.get(&cred.id) else {
        return Err(deny("an unknown passkey"));
    };
    let Ok(Some(user)) = app.auth.store.user(&stored.user_id) else {
        return Err(deny("its user is gone"));
    };
    let Ok(handle) = app.auth.passkeys.handle(&user.id) else {
        return Err(deny("no user handle"));
    };
    let counter = match pk::finish_login(&cer, &cred, &stored, &handle) {
        Ok(n) => n,
        Err(e) => return Err(deny(&e.to_string())),
    };
    if user.disabled {
        return Err(deny("the user is disabled"));
    }
    let mut stored = stored;
    stored.counter = counter;
    stored.last_used_at = Some(crate::tenancy::now());
    let _ = app.auth.passkeys.put(&stored);
    let (cookie, _) = app
        .auth
        .create_session_with(&user, Method::Passkey, true)
        .map_err(super::auth_error)?;
    let p = app
        .auth
        .session_principal(&cookie, auth::Transport::Tcp, None)
        .map_err(super::auth_error)?
        .ok_or_else(|| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "new session missing",
            )
        })?;
    audit_login(&app, Some(&p), &rid.0, "passkey", "ok", &stored.name);
    let w = crate::service::system::whoami_of(&app, &p)?;
    let secure = secure(&headers, listener, &app);
    let mut r = Json(serde_json::to_value(w).unwrap_or_default()).into_response();
    let mut sc = format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/",
        auth::SESSION_COOKIE,
        cookie
    );
    if secure {
        sc.push_str("; Secure");
    }
    r.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&sc).expect("cookie is ASCII"),
    );
    Ok(with_cookie(r, "", 0, secure))
}

// ---- a passkey invitation's first passkey (spec/19-tenancy.md D60) ------------

#[derive(Deserialize)]
pub struct InviteStart {
    invitation: String,
}

#[derive(Deserialize)]
pub struct InviteFinish {
    invitation: String,
    credential: CredentialJson,
}

fn invitation_refused() -> ApiErr {
    err(
        StatusCode::NOT_FOUND,
        "not_found",
        "no such invitation, or it was taken or expired",
    )
}

/// The open passkey invitation `s` names.
fn passkey_invitation(
    app: &AppState,
    s: &str,
) -> Result<
    (
        crate::directory::invitations::InviteToken,
        crate::directory::invitations::Invitation,
    ),
    ApiErr,
> {
    let token =
        crate::directory::invitations::InviteToken::parse(s).ok_or_else(invitation_refused)?;
    let inv = app
        .auth
        .invitations
        .open(&token)
        .ok()
        .flatten()
        .filter(|i| i.connector_id == pk::PROVIDER)
        .ok_or_else(invitation_refused)?;
    Ok((token, inv))
}

/// `POST /auth/invitation/passkey/start {invitation}`: the options for a
/// new user's first passkey.
pub async fn invite_start(
    State(app): State<AppState>,
    headers: HeaderMap,
    listener: Option<Extension<Listener>>,
    Json(body): Json<InviteStart>,
) -> Result<Response, ApiErr> {
    let (origin, rp) = origin_and_rp(&app, &headers)?;
    let (_, inv) = passkey_invitation(&app, &body.invitation)?;
    let user_id = uuid::Uuid::new_v4().to_string();
    let handle = app
        .auth
        .passkeys
        .handle(&user_id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let (cookie, cer) = app.auth.ceremonies.start(
        Purpose::Invite {
            invitation: body.invitation.trim().to_string(),
            user_id,
        },
        &rp,
        &origin,
    );
    let opts = pk::creation_options(
        &cer,
        &app.auth.config.auth.passkeys.rp_name,
        &handle,
        &inv.email,
        &inv.email,
        &[],
    );
    Ok(with_cookie(
        Json(opts).into_response(),
        &cookie,
        pk::CEREMONY_SECS,
        secure(&headers, listener, &app),
    ))
}

/// `POST /auth/invitation/passkey/finish {invitation, credential}`: the
/// user made in the invitation's directory with this passkey, the
/// invitation taken, and a session.
pub async fn invite_finish(
    State(app): State<AppState>,
    Extension(rid): Extension<RequestId>,
    listener: Option<Extension<Listener>>,
    headers: HeaderMap,
    Json(body): Json<InviteFinish>,
) -> Result<Response, ApiErr> {
    let bad = |m: String| err(StatusCode::BAD_REQUEST, "passkey_not_added", m);
    let internal = |e: String| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e);
    let cer = cookie_of(&headers)
        .and_then(|k| app.auth.ceremonies.take(&k))
        .ok_or_else(|| bad("the passkey prompt expired: start again".into()))?;
    let Purpose::Invite {
        invitation,
        user_id,
    } = cer.purpose.clone()
    else {
        return Err(bad("this isn't an invitation's passkey".into()));
    };
    if invitation != body.invitation.trim() {
        return Err(bad("the passkey prompt was for another invitation".into()));
    }
    let (token, inv) = passkey_invitation(&app, &invitation)?;
    let p = pk::finish_registration(&cer, &body.credential, &user_id, "Passkey")
        .map_err(|e| bad(e.to_string()))?;
    let a = &app.auth;
    if a.passkeys.get(&p.id).ok().flatten().is_some()
        || a.store
            .identity(pk::PROVIDER, &p.id)
            .ok()
            .flatten()
            .is_some()
    {
        return Err(bad("this passkey is registered already".into()));
    }
    let tenant = a
        .tenants
        .get(&inv.tenant_id)
        .map_err(|e| internal(e.to_string()))?
        .ok_or_else(invitation_refused)?;
    // The invitation first: whoever takes it, once.
    let inv = a
        .invitations
        .take_by_link(&token, &user_id)
        .map_err(|_| invitation_refused())?;
    let now = crate::tenancy::now();
    let local = inv.email.split('@').next().unwrap_or("").to_string();
    let user = auth::store::User {
        id: user_id.clone(),
        directory_id: tenant.directory_id.clone(),
        display_name: if local.is_empty() {
            inv.email.clone()
        } else {
            local
        },
        disabled: false,
        default_project: None,
        created_at: now,
        managed: None,
        email: Some(inv.email.clone()),
        disabled_reason: None,
        pending_login: None,
    };
    a.store
        .put_user(&user)
        .map_err(|e| internal(e.to_string()))?;
    a.store
        .put_identity(&auth::store::Identity {
            provider: pk::PROVIDER.into(),
            subject: p.id.clone(),
            user_id: user_id.clone(),
            email: None,
            created_at: p.created_at,
        })
        .map_err(|e| internal(e.to_string()))?;
    a.passkeys.put(&p).map_err(|e| internal(e.to_string()))?;
    let by = format!("invitation:{}", inv.id);
    a.tenants
        .add_member(&inv.tenant_id, Ent::User(user_id.clone()), &by)
        .map_err(|e| internal(e.to_string()))?;
    if inv.role != crate::service::invitations::MEMBER {
        a.add_link(
            &inv.role,
            Ent::User(user_id.clone()),
            Ent::Tenant(inv.tenant_id.clone()),
            &by,
        )
        .map_err(super::auth_error)?;
    }
    // The owner tenant's administrator administers its directory (D59).
    if tenant.owns_directory && inv.role == "role.tenant-admin" {
        a.add_link(
            "role.directory-admin",
            Ent::User(user_id.clone()),
            Ent::Directory(tenant.directory_id.clone()),
            &by,
        )
        .map_err(super::auth_error)?;
    }
    let (cookie, _) = a
        .create_session_with(&user, Method::Passkey, true)
        .map_err(super::auth_error)?;
    let principal = a
        .session_principal(&cookie, auth::Transport::Tcp, None)
        .map_err(super::auth_error)?
        .ok_or_else(|| internal("new session missing".into()))?;
    audit_login(
        &app,
        Some(&principal),
        &rid.0,
        "passkey",
        "ok",
        "invitation",
    );
    let w = crate::service::system::whoami_of(&app, &principal)?;
    let secure = secure(&headers, listener, &app);
    let mut r = (
        StatusCode::CREATED,
        Json(serde_json::to_value(w).unwrap_or_default()),
    )
        .into_response();
    let mut sc = format!(
        "{}={}; HttpOnly; SameSite=Strict; Path=/",
        auth::SESSION_COOKIE,
        cookie
    );
    if secure {
        sc.push_str("; Secure");
    }
    r.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&sc).expect("cookie is ASCII"),
    );
    Ok(with_cookie(r, "", 0, secure))
}
