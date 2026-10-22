//! OIDC login (spec/security.md §5.4).
//!
//! Authorization Code flow with PKCE and a nonce; the control plane is a
//! confidential client (secret from the systemd credential
//! `oidc-client-secret`). ID tokens are checked for signature (JWKS,
//! cached, refreshed once on an unknown `kid`), `iss`, `aud`, `exp`, `iat`
//! skew and `nonce`, with algorithms limited to RS256/ES256. The identity
//! key is `(issuer, sub)`; `email`/`name` are display only.
//!
//! gxctl uses the Device Authorization Grant when the IdP supports it; the
//! control plane validates the resulting ID token the same way and hands
//! gxctl a short-lived personal access token.
//!
//! The flow is [`glidex_auth::oidc`]'s; this module gives it glidex's
//! configuration, client secrets, presets and [`After`].

use crate::config::OidcConfig;
pub use glidex_auth::oidc::{
    DeviceStart, Discovery, IdClaims, OidcError, OidcIdentity, Start, safe_return_to,
};

/// A failed callback, and what its login was for once the state was this
/// browser's (`(return_to, after)`).
pub type CallbackError = glidex_auth::oidc::CallbackError<After>;

/// What a browser login is for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum After {
    /// Signing in.
    #[default]
    Login,
    /// Signing in and taking an invitation (`<id>.<secret>`, spec/tenancy.md §6.1).
    Invite(String),
    /// Adding this sign-in to `user`, who started it signed in through
    /// `session` (its hash; §6.2). The session cookie is `SameSite=Strict`
    /// and doesn't come back on the IdP's redirect, so the callback checks
    /// that session is still live instead.
    Link {
        user: String,
        session: Option<String>,
    },
}

/// Where a connector's client secret comes from.
pub enum ClientSecret {
    /// The systemd credential `oidc-client-secret` (the file-defined
    /// connector, as before connectors).
    Credential,
    /// Decrypted from the connector store on each use (spec/tenancy.md §13).
    Stored(Box<dyn Fn() -> Result<zeroize::Zeroizing<String>, String> + Send + Sync>),
}

impl From<ClientSecret> for glidex_auth::oidc::ClientSecret {
    fn from(s: ClientSecret) -> Self {
        match s {
            ClientSecret::Stored(f) => glidex_auth::oidc::ClientSecret::new(f),
            ClientSecret::Credential => glidex_auth::oidc::ClientSecret::new(|| {
                let path = crate::config::credential("oidc-client-secret")
                    .or_else(|| std::env::var_os("GLIDEX_OIDC_CLIENT_SECRET_FILE").map(Into::into))
                    .ok_or_else(|| "no oidc-client-secret credential".to_string())?;
                std::fs::read_to_string(&path)
                    .map(|s| zeroize::Zeroizing::new(s.trim().to_string()))
                    .map_err(|e| format!("reading the client secret: {}", e))
            }),
        }
    }
}

/// One connector's OIDC client.
pub struct OidcState {
    cfg: OidcConfig,
    inner: glidex_auth::oidc::OidcState<After>,
}

impl OidcState {
    pub fn new(cfg: &OidcConfig) -> Self {
        Self::with(
            cfg,
            ClientSecret::Credential,
            Vec::new(),
            "/api/auth/oidc/callback",
        )
    }

    pub fn with(
        cfg: &OidcConfig,
        secret: ClientSecret,
        mfa_acr: Vec<String>,
        callback_path: &str,
    ) -> Self {
        let client = glidex_auth::oidc::ClientConfig {
            enabled: cfg.enabled,
            issuer: cfg.issuer.clone(),
            client_id: cfg.client_id.clone(),
            scopes: cfg.scopes.clone(),
            groups_claim: cfg.groups_claim.clone(),
            allowed_domains: cfg.allowed_domains.clone(),
            required_groups: cfg.required_groups.clone(),
            redirect_uri: cfg.redirect_uri.clone(),
            callback_path: callback_path.to_string(),
            mfa_acr,
        };
        Self {
            cfg: cfg.clone(),
            inner: glidex_auth::oidc::OidcState::new(client, secret.into()),
        }
    }

    /// With a preset's issuer spellings, request parameters and checks.
    pub fn with_preset(mut self, preset: super::presets::Preset) -> Self {
        self.inner = self.inner.with_preset(preset.into());
        self
    }

    pub fn enabled(&self) -> bool {
        self.inner.enabled()
    }

    pub fn config(&self) -> &OidcConfig {
        &self.cfg
    }

    pub async fn discovery(&self) -> Result<Discovery, OidcError> {
        self.inner.discovery().await
    }

    pub fn redirect_uri(&self, default_origin: Option<&str>) -> String {
        self.inner.redirect_uri(default_origin)
    }

    /// Begin a browser login. `reauth` forces a fresh login (step-up);
    /// `after` comes back from [`callback`](Self::callback).
    pub async fn start(
        &self,
        return_to: Option<&str>,
        redirect_uri: &str,
        reauth: bool,
        after: After,
    ) -> Result<Start, OidcError> {
        self.inner
            .start(return_to, redirect_uri, reauth, after)
            .await
    }

    /// Finish a browser login: check `state` against the browser cookie,
    /// redeem the code, verify the ID token. Returns the identity, the
    /// path to return to and what the login was for.
    pub async fn callback(
        &self,
        code: &str,
        state: &str,
        browser: Option<&str>,
        redirect_uri: &str,
    ) -> Result<(OidcIdentity, String, After), CallbackError> {
        self.inner
            .callback(code, state, browser, redirect_uri)
            .await
    }

    /// Verify an ID token and apply the login rules (domains, groups).
    pub async fn verify(
        &self,
        d: &Discovery,
        id_token: &str,
        nonce: Option<&str>,
    ) -> Result<OidcIdentity, OidcError> {
        self.inner.verify(d, id_token, nonce).await
    }

    /// Start a device grant (gxctl `login --oidc`).
    pub async fn device_start(&self) -> Result<DeviceStart, OidcError> {
        self.inner.device_start().await
    }

    /// Poll a device grant started here.
    pub async fn device_poll(&self, device_code: &str) -> Result<OidcIdentity, OidcError> {
        self.inner.device_poll(device_code).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_to_is_same_site_only() {
        assert_eq!(safe_return_to(Some("/vms/1")), "/vms/1");
        assert_eq!(safe_return_to(Some("//evil.example")), "/");
        assert_eq!(safe_return_to(Some("https://evil.example")), "/");
        assert_eq!(safe_return_to(Some("/\\evil")), "/");
        assert_eq!(safe_return_to(None), "/");
    }
}
