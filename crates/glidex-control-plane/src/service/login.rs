//! `glidex.auth.v1.LoginService` (spec/tenancy.md §6, §14): discovery,
//! the device grant and password tokens. The REST twins in `api/access.rs`
//! call these too; browser logins (redirects) stay there and share
//! `oidc_client` and `oidc_user`.

use super::{ServiceError, ServiceResult};
use crate::api::{AppState, Listener, auth_error};
use crate::auth::store::{StoreError, TokenKind, User};
use crate::auth::{self, AuthError, Principal};
use crate::directory::Connector;
use axum::http::StatusCode;
use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::auth::v1 as pb;
use std::net::IpAddr;
use std::sync::Arc;

/// Lifetime of the token gxctl gets from a device login.
pub const DEVICE_TOKEN_DAYS: u64 = 1;

/// Where a login request came from: these operations have no caller.
pub struct Origin<'a> {
    pub request_id: &'a str,
    pub listener: Listener,
    pub client_ip: Option<IpAddr>,
}

/// A new personal token, its secret shown once.
pub struct Minted {
    pub secret: String,
    pub token: auth::store::Token,
    pub user: User,
}

impl Minted {
    pub fn to_pb(&self) -> pb::IssuedToken {
        pb::IssuedToken {
            token: self.secret.clone(),
            token_id: self.token.id.clone(),
            expires_at: Timestamp {
                seconds: self.token.expires_at as i64,
                ..Default::default()
            }
            .into(),
            user_id: self.user.id.clone(),
            display_name: self.user.display_name.clone(),
            ..Default::default()
        }
    }
}

/// Audit a login outcome (spec/security.md §10).
pub fn audit_login(
    app: &AppState,
    p: Option<&Principal>,
    rid: &str,
    method: &str,
    result: &str,
    who: &str,
) {
    app.auth.audit(
        p,
        rid,
        "login",
        None,
        Some(&format!("user:{}", who)),
        result,
        None,
        &[],
        serde_json::json!({ "method": method }),
    );
}

/// The connectors a login page offers.
fn login_connectors(app: &AppState) -> Vec<pb::LoginConnector> {
    app.auth
        .connectors
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.enabled)
        .map(|c| pb::LoginConnector {
            id: c.id,
            kind: c.kind.as_str().into(),
            display_name: c.display_name,
            ..Default::default()
        })
        .collect()
}

/// What to offer for a tenant slug or an email address. This host hosts
/// one directory, so every login connector is offered; a hint naming a
/// tenant is echoed.
pub fn discover(app: &AppState, req: pb::DiscoverRequest) -> pb::DiscoverResponse {
    let slug = req.hint.trim().to_ascii_lowercase();
    let tenant = app
        .auth
        .tenants
        .list()
        .unwrap_or_default()
        .into_iter()
        .find(|t| t.slug == slug);
    pb::DiscoverResponse {
        tenant: tenant
            .map(|t| pb::DiscoveredTenant {
                slug: t.slug,
                display_name: t.display_name,
                ..Default::default()
            })
            .into(),
        connectors: login_connectors(app),
        ..Default::default()
    }
}

pub fn oidc_error(e: crate::directory::oidc::OidcError) -> ServiceError {
    use crate::directory::oidc::OidcError as E;
    let (status, code) = match e {
        E::Disabled => (StatusCode::NOT_FOUND, "oidc_disabled"),
        E::NotAllowed(_) => (StatusCode::FORBIDDEN, "login_not_allowed"),
        E::Pending => (StatusCode::ACCEPTED, "authorization_pending"),
        E::SlowDown => (StatusCode::TOO_MANY_REQUESTS, "slow_down"),
        E::Provider(_) => (StatusCode::BAD_GATEWAY, "idp_error"),
        E::Invalid(_) => (StatusCode::UNAUTHORIZED, "login_failed"),
    };
    ServiceError::new(status, code, e.to_string())
}

/// Connector `id` (or the first OIDC one) and its client.
pub fn oidc_client(
    app: &AppState,
    id: Option<&str>,
) -> ServiceResult<(Connector, Arc<crate::directory::oidc::OidcState>)> {
    app.auth.oidc_for(id).map_err(|e| match e {
        AuthError::Store(StoreError::NotFound(_)) => ServiceError::new(
            StatusCode::NOT_FOUND,
            "oidc_disabled",
            "no such OIDC connector, or OIDC logins are off",
        ),
        other => auth_error(other).into(),
    })
}

/// The user for a verified OIDC identity, through the common path.
pub fn oidc_user(
    app: &AppState,
    c: &Connector,
    id: &crate::directory::oidc::OidcIdentity,
) -> ServiceResult<User> {
    let ext = crate::directory::ExternalIdentity {
        connector_id: c.id.clone(),
        subject: id.subject.clone(),
        display_name: id.display_name.clone(),
        email: id.email.clone(),
        groups: id.groups.clone(),
        mfa: id.mfa,
        email_verified: id.email_verified,
    };
    app.auth
        .external_login(c, &ext)
        .map_err(|e| auth_error(e).into())
}

/// Connector `id`'s device-grant client (gxctl); `400
/// device_flow_unsupported` for a `google` connector without one (§5.1).
fn device_client(
    app: &AppState,
    id: Option<&str>,
) -> ServiceResult<(Connector, Arc<crate::directory::oidc::OidcState>)> {
    let (c, _) = oidc_client(app, id)?;
    if !c.has_device_client() {
        return Err(ServiceError::new(
            StatusCode::BAD_REQUEST,
            "device_flow_unsupported",
            "this connector has no device client: log in with a token, or ask for a device client",
        ));
    }
    app.auth
        .oidc_device_for(Some(&c.id))
        .map_err(|e| auth_error(e).into())
}

pub async fn start_device(
    app: &AppState,
    req: pb::StartDeviceRequest,
) -> ServiceResult<pb::StartDeviceResponse> {
    let (_, oidc) = device_client(app, req.connector.as_deref())?;
    let d = oidc.device_start().await.map_err(oidc_error)?;
    Ok(pb::StartDeviceResponse {
        device_code: d.device_code,
        user_code: d.user_code,
        verification_uri: d.verification_uri,
        verification_uri_complete: d.verification_uri_complete,
        interval: d.interval,
        expires_in: d.expires_in,
        ..Default::default()
    })
}

/// `None` while the user hasn't finished.
pub async fn poll_device(
    app: &AppState,
    origin: &Origin<'_>,
    req: pb::PollDeviceRequest,
) -> ServiceResult<Option<Minted>> {
    let (c, oidc) = device_client(app, req.connector.as_deref())?;
    let identity = match oidc.device_poll(&req.device_code).await {
        Ok(i) => i,
        Err(crate::directory::oidc::OidcError::Pending) => return Ok(None),
        Err(e) => return Err(oidc_error(e)),
    };
    let user = oidc_user(app, &c, &identity)?;
    let name = req.token_name.unwrap_or_else(|| "gxctl".into());
    let days = DEVICE_TOKEN_DAYS.min(c.token_max_days());
    let (secret, token) = app
        .auth
        .create_token(
            &name,
            TokenKind::Personal {
                owner: user.id.clone(),
            },
            &user.id,
            Some(days),
            req.device.as_deref(),
            req.client.as_deref(),
        )
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    audit_login(app, None, origin.request_id, "oidc-device", "ok", &user.id);
    Ok(Some(Minted {
        secret,
        token,
        user,
    }))
}

/// CLI/CI token login (spec/gxctl-auth.md §7.3): PAM credentials in, a
/// personal bearer token out. Same authd path, rate limits, fixed delay
/// and JIT provisioning as `POST /auth/login`; the answer is a token
/// because the CLI and CI carry tokens while sessions stay the browser's
/// half of security.md §5.5.
pub async fn password_token(
    app: &AppState,
    origin: &Origin<'_>,
    req: pb::PasswordTokenRequest,
) -> ServiceResult<Minted> {
    if app.auth.pam_connector(req.connector.as_deref()).is_err() {
        return Err(ServiceError::unauthenticated(
            "PAM logins are off: set auth.pam.enabled in control-plane.json, or name a PAM connector",
        ));
    }
    // Passwords only over TLS or from this host (security.md §5.3), the
    // same rule as `login`.
    let local = match origin.listener {
        Listener::Ui | Listener::Api | Listener::Cluster | Listener::Relay => true,
        Listener::Tcp => {
            app.auth.config.tls.enabled() || origin.client_ip.is_none_or(|ip| ip.is_loopback())
        }
    };
    if !local {
        return Err(ServiceError::new(
            StatusCode::FORBIDDEN,
            "tls_required",
            "password token logins need TLS",
        ));
    }
    let a = app.clone();
    let username = req.username.clone();
    let password = zeroize::Zeroizing::new(req.password);
    let connector = req.connector.clone();
    let result = tokio::task::spawn_blocking(move || {
        a.auth
            .login_pam_with(connector.as_deref(), &username, &password)
    })
    .await
    .map_err(|e| ServiceError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let (user, c) = match result {
        Ok(v) => v,
        Err(e) => {
            audit_login(
                app,
                None,
                origin.request_id,
                "pam-token",
                "denied",
                &req.username,
            );
            return Err(auth_error(e).into());
        }
    };
    // D20: a connector without provisioning can't vouch for a long token.
    let days = req
        .days
        .map(u64::from)
        .unwrap_or(app.auth.config.auth.tokens.default_days)
        .min(c.token_max_days());
    let name = req.token_name.unwrap_or_else(|| "gxctl".into());
    let (secret, token) = app
        .auth
        .create_token(
            &name,
            TokenKind::Personal {
                owner: user.id.clone(),
            },
            &user.id,
            Some(days),
            req.device.as_deref(),
            req.client.as_deref(),
        )
        .map_err(|e| ServiceError::from(auth_error(e)))?;
    audit_login(app, None, origin.request_id, "pam-token", "ok", &user.id);
    Ok(Minted {
        secret,
        token,
        user,
    })
}
