//! OIDC login (glidex spec/11-security.md §5.4).
//!
//! Authorization Code flow with PKCE and a nonce; the application is a
//! confidential client (its secret from a [`ClientSecret`] it supplies).
//! ID tokens are checked for signature (JWKS, cached, refreshed once on an
//! unknown `kid`), `iss`, `aud`, `exp`, `iat` skew and `nonce`, with
//! algorithms limited to RS256/ES256. The identity key is
//! `(issuer, sub)`; `email`/`name` are display only.
//!
//! A CLI uses the Device Authorization Grant when the IdP supports it; the
//! ID token it ends with is validated the same way.
//!
//! What a browser login is for (glidex: signing in, taking an invitation,
//! adding a sign-in) is the application's type `A`: kept with the login
//! state, never in a URL, and handed back by [`OidcState::callback`].

use base64::Engine as _;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, jwk::JwkSet};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::now;
use crate::secrets::{constant_eq, random_token};

/// How long a login `state` stays valid.
pub const STATE_SECS: u64 = 600;
/// Allowed clock skew for `iat`/`exp`.
pub const SKEW_SECS: u64 = 60;
/// Don't refetch the JWKS more often than this on unknown `kid`s.
const JWKS_REFRESH_MIN: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("OIDC login is not configured")]
    Disabled,
    #[error("identity provider error: {0}")]
    Provider(String),
    #[error("invalid OIDC response: {0}")]
    Invalid(String),
    #[error("login not allowed: {0}")]
    NotAllowed(String),
    #[error("authorization pending")]
    Pending,
    #[error("slow down")]
    SlowDown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    #[serde(default)]
    pub device_authorization_endpoint: Option<String>,
}

/// One client: what the flow needs from the application's configuration.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub enabled: bool,
    pub issuer: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    /// The ID token claim that lists the user's groups.
    pub groups_claim: String,
    /// Email domains allowed to log in; empty allows any. A login needs
    /// a verified address in one of them.
    pub allowed_domains: Vec<String>,
    /// A token without `email_verified` counts as verified: the IdP
    /// assigns its users' addresses (spec/19-tenancy.md D75).
    pub trust_email: bool,
    /// Groups of which a user must be in one; empty requires none.
    pub required_groups: Vec<String>,
    /// Where the IdP sends the browser back; must be registered with it.
    /// Default: `<origin>` + `callback_path`.
    pub redirect_uri: Option<String>,
    /// Default callback path on the UI origin.
    pub callback_path: String,
    /// `acr` values that count as a multi-factor login.
    pub mfa_acr: Vec<String>,
}

/// What a check of the IdP's own claims says about a login it allows,
/// where it decides instead of the generic claims.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Checked {
    /// Whether the login counts as multi-factor.
    pub mfa: Option<bool>,
    /// Whether the email is the user's.
    pub email_verified: Option<bool>,
}

/// Checks of the claims one IdP adds to the generic ones, after the ID
/// token is verified (glidex's presets, spec/19-tenancy.md §5.1).
pub trait ClaimCheck: Send + Sync {
    /// `Err` with why the login isn't allowed.
    fn check(&self, claims: &HashMap<String, serde_json::Value>) -> Result<Checked, String>;
}

/// What a client takes from an IdP preset.
#[derive(Clone, Default)]
pub struct Preset {
    /// Other spellings of the issuer that the IdP's tokens carry.
    pub issuer_aliases: Vec<String>,
    /// Added to the authorization request.
    pub auth_params: Vec<(String, String)>,
    /// `None`: only the generic OIDC checks.
    pub rule: Option<Arc<dyn ClaimCheck>>,
}

/// Where a client's secret comes from: called on each use, so a rotated
/// secret takes effect without a new client. `Err` says why there is none.
pub struct ClientSecret(Box<dyn Fn() -> Result<zeroize::Zeroizing<String>, String> + Send + Sync>);

impl ClientSecret {
    pub fn new(
        f: impl Fn() -> Result<zeroize::Zeroizing<String>, String> + Send + Sync + 'static,
    ) -> Self {
        ClientSecret(Box::new(f))
    }
}

struct Pending<A> {
    nonce: String,
    verifier: String,
    return_to: String,
    browser: String,
    created: u64,
    /// What the login is for: kept here, never in a URL.
    after: A,
}

/// A failed callback, and what its login was for once the state was this
/// browser's (`(return_to, after)`).
#[derive(Debug)]
pub struct CallbackError<A> {
    pub error: OidcError,
    pub started: Option<(String, A)>,
}

impl<A> From<OidcError> for CallbackError<A> {
    fn from(error: OidcError) -> Self {
        CallbackError {
            error,
            started: None,
        }
    }
}

struct Jwks {
    set: JwkSet,
    fetched: Instant,
}

/// Claims read from an ID token.
#[derive(Debug, Clone, Deserialize)]
pub struct IdClaims {
    pub sub: String,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub iat: Option<u64>,
    #[serde(flatten)]
    pub rest: HashMap<String, serde_json::Value>,
}

/// A verified login.
#[derive(Debug, Clone)]
pub struct OidcIdentity {
    pub issuer: String,
    pub subject: String,
    pub display_name: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
    /// `amr` says `mfa`, or `acr` is one of the client's MFA values;
    /// a preset may decide instead (Google: `trust_2sv`).
    pub mfa: bool,
    /// The token's `email_verified`.
    pub email_verified: bool,
}

/// What `start` returns for the browser.
pub struct Start {
    pub redirect: String,
    /// Value for the browser-binding cookie (glidex: `gx_oidc`).
    pub browser: String,
}

/// Device grant started for a CLI.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    #[serde(default = "default_interval")]
    pub interval: u64,
    pub expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

pub struct OidcState<A> {
    cfg: ClientConfig,
    /// What its preset adds (glidex spec/19-tenancy.md §5.1).
    preset: Preset,
    secret: ClientSecret,
    http: reqwest::Client,
    discovery: tokio::sync::Mutex<Option<Discovery>>,
    jwks: tokio::sync::Mutex<Option<Jwks>>,
    pending: Mutex<HashMap<String, Pending<A>>>,
    /// Device grants: device_code → nonce-free marker of when it started.
    devices: Mutex<HashMap<String, u64>>,
}

/// `application/x-www-form-urlencoded`, as client credentials go in a
/// Basic header (RFC 6749 §2.3.1): an IdP that follows it (Ory Hydra,
/// JumpCloud's) decodes them, so a secret with `+`, `/` or `%` sent as is
/// would not match. Letters, digits and `*-._` stay as they are.
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// A same-site path to return to after login; anything else becomes `/`.
pub fn safe_return_to(r: Option<&str>) -> String {
    match r {
        Some(p)
            if p.starts_with('/')
                && !p.starts_with("//")
                && !p.contains('\\')
                && !p.contains("://") =>
        {
            p.to_string()
        }
        _ => "/".to_string(),
    }
}

impl<A: Clone> OidcState<A> {
    pub fn new(cfg: ClientConfig, secret: ClientSecret) -> Self {
        Self {
            cfg,
            preset: Default::default(),
            secret,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            discovery: tokio::sync::Mutex::new(None),
            jwks: tokio::sync::Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            devices: Mutex::new(HashMap::new()),
        }
    }

    /// With a preset's issuer spellings, request parameters and checks.
    pub fn with_preset(mut self, preset: Preset) -> Self {
        self.preset = preset;
        self
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled && !self.cfg.issuer.is_empty() && !self.cfg.client_id.is_empty()
    }

    pub fn config(&self) -> &ClientConfig {
        &self.cfg
    }

    fn client_secret(&self) -> Result<String, OidcError> {
        (self.secret.0)()
            .map(|s| s.to_string())
            .map_err(OidcError::Provider)
    }

    pub async fn discovery(&self) -> Result<Discovery, OidcError> {
        if !self.enabled() {
            return Err(OidcError::Disabled);
        }
        let mut cached = self.discovery.lock().await;
        if let Some(d) = cached.as_ref() {
            return Ok(d.clone());
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.cfg.issuer.trim_end_matches('/')
        );
        let d: Discovery = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| OidcError::Provider(e.to_string()))?
            .json()
            .await
            .map_err(|e| OidcError::Invalid(format!("discovery: {}", e)))?;
        if d.issuer.trim_end_matches('/') != self.cfg.issuer.trim_end_matches('/') {
            return Err(OidcError::Invalid(format!(
                "discovery issuer {} != configured {}",
                d.issuer, self.cfg.issuer
            )));
        }
        *cached = Some(d.clone());
        Ok(d)
    }

    async fn jwks(&self, d: &Discovery, refresh: bool) -> Result<JwkSet, OidcError> {
        let mut cached = self.jwks.lock().await;
        if let Some(j) = cached.as_ref()
            && (!refresh || j.fetched.elapsed() < JWKS_REFRESH_MIN)
        {
            return Ok(j.set.clone());
        }
        let set: JwkSet = self
            .http
            .get(&d.jwks_uri)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| OidcError::Provider(e.to_string()))?
            .json()
            .await
            .map_err(|e| OidcError::Invalid(format!("jwks: {}", e)))?;
        *cached = Some(Jwks {
            set: set.clone(),
            fetched: Instant::now(),
        });
        Ok(set)
    }

    pub fn redirect_uri(&self, default_origin: Option<&str>) -> String {
        self.cfg.redirect_uri.clone().unwrap_or_else(|| {
            format!(
                "{}{}",
                default_origin
                    .unwrap_or("http://localhost:5173")
                    .trim_end_matches('/'),
                self.cfg.callback_path
            )
        })
    }

    /// Begin a browser login. `reauth` forces a fresh login (step-up);
    /// `after` comes back from [`callback`](Self::callback).
    pub async fn start(
        &self,
        return_to: Option<&str>,
        redirect_uri: &str,
        reauth: bool,
        after: A,
    ) -> Result<Start, OidcError> {
        let d = self.discovery().await?;
        let (state, nonce, challenge, browser) = self.remember(return_to, after);
        let mut url = reqwest::Url::parse(&d.authorization_endpoint)
            .map_err(|e| OidcError::Invalid(e.to_string()))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("response_type", "code")
                .append_pair("client_id", &self.cfg.client_id)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("scope", &self.cfg.scopes.join(" "))
                .append_pair("state", &state)
                .append_pair("nonce", &nonce)
                .append_pair("code_challenge", &challenge)
                .append_pair("code_challenge_method", "S256");
            if reauth {
                q.append_pair("prompt", "login").append_pair("max_age", "0");
            }
            for (k, v) in &self.preset.auth_params {
                q.append_pair(k, v);
            }
        }
        Ok(Start {
            redirect: url.to_string(),
            browser,
        })
    }

    /// Keep a new login's state, forgetting expired ones. Returns its
    /// `state`, `nonce`, PKCE challenge and browser-binding value.
    fn remember(&self, return_to: Option<&str>, after: A) -> (String, String, String, String) {
        let state = random_token(24);
        let nonce = random_token(24);
        let verifier = random_token(48);
        let browser = random_token(24);
        let challenge = b64url(&sha2::Sha256::digest_bytes(verifier.as_bytes()));
        let mut p = self.pending.lock().unwrap();
        let t = now();
        p.retain(|_, v| v.created + STATE_SECS > t);
        p.insert(
            state.clone(),
            Pending {
                nonce: nonce.clone(),
                verifier,
                return_to: safe_return_to(return_to),
                browser: browser.clone(),
                created: t,
                after,
            },
        );
        (state, nonce, challenge, browser)
    }

    /// Take a login's state: once only, while fresh, from its browser.
    fn take(&self, state: &str, browser: Option<&str>) -> Result<Pending<A>, OidcError> {
        let pending = self
            .pending
            .lock()
            .unwrap()
            .remove(state)
            .ok_or_else(|| OidcError::Invalid("unknown or expired login state".into()))?;
        if pending.created + STATE_SECS <= now() {
            return Err(OidcError::Invalid("login state expired".into()));
        }
        if !browser.is_some_and(|b| constant_eq(b, &pending.browser)) {
            return Err(OidcError::Invalid(
                "login state belongs to another browser".into(),
            ));
        }
        Ok(pending)
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
    ) -> Result<(OidcIdentity, String, A), CallbackError<A>> {
        let pending = self.take(state, browser)?;
        // From here the login is this browser's: a failure says what it was for.
        let with = |e: OidcError| CallbackError {
            error: e,
            started: Some((pending.return_to.clone(), pending.after.clone())),
        };
        let d = self.discovery().await.map_err(with)?;
        let resp = self
            .token_request(
                &d,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", code),
                    ("redirect_uri", redirect_uri),
                    ("code_verifier", &pending.verifier),
                ],
            )
            .await
            .map_err(with)?;
        let id_token = resp
            .get("id_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| with(OidcError::Invalid("token response has no id_token".into())))?;
        let identity = self
            .verify(&d, id_token, Some(&pending.nonce))
            .await
            .map_err(with)?;
        Ok((identity, pending.return_to, pending.after))
    }

    async fn token_request(
        &self,
        d: &Discovery,
        form: &[(&str, &str)],
    ) -> Result<serde_json::Value, OidcError> {
        let secret = self.client_secret()?;
        let resp = self
            .http
            .post(&d.token_endpoint)
            .basic_auth(form_encode(&self.cfg.client_id), Some(form_encode(&secret)))
            .form(form)
            .send()
            .await
            .map_err(|e| OidcError::Provider(e.to_string()))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| OidcError::Invalid(format!("token response: {}", e)))?;
        if !status.is_success() {
            let err = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("error");
            return Err(match err {
                "authorization_pending" => OidcError::Pending,
                "slow_down" => OidcError::SlowDown,
                _ => OidcError::Provider(format!("token endpoint: {}", err)),
            });
        }
        Ok(body)
    }

    /// Verify an ID token and apply the login rules (domains, groups).
    pub async fn verify(
        &self,
        d: &Discovery,
        id_token: &str,
        nonce: Option<&str>,
    ) -> Result<OidcIdentity, OidcError> {
        let header = jsonwebtoken::decode_header(id_token)
            .map_err(|e| OidcError::Invalid(format!("id_token header: {}", e)))?;
        if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
            return Err(OidcError::Invalid(format!(
                "id_token algorithm {:?} is not allowed",
                header.alg
            )));
        }
        let kid = header.kid.clone();
        let find = |set: &JwkSet| -> Option<jsonwebtoken::jwk::Jwk> {
            match &kid {
                Some(k) => set.find(k).cloned(),
                None if set.keys.len() == 1 => set.keys.first().cloned(),
                None => None,
            }
        };
        let jwk = match find(&self.jwks(d, false).await?) {
            Some(j) => j,
            None => find(&self.jwks(d, true).await?)
                .ok_or_else(|| OidcError::Invalid("id_token key not in JWKS".into()))?,
        };
        let key =
            DecodingKey::from_jwk(&jwk).map_err(|e| OidcError::Invalid(format!("jwk: {}", e)))?;
        let mut v = Validation::new(header.alg);
        let mut issuers = vec![d.issuer.as_str()];
        issuers.extend(self.preset.issuer_aliases.iter().map(String::as_str));
        v.set_issuer(&issuers);
        v.set_audience(&[self.cfg.client_id.as_str()]);
        v.leeway = SKEW_SECS;
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let data = jsonwebtoken::decode::<IdClaims>(id_token, &key, &v)
            .map_err(|e| OidcError::Invalid(format!("id_token: {}", e)))?;
        self.identity(&d.issuer, data.claims, nonce)
    }

    /// The login rules on a verified token's claims.
    fn identity(
        &self,
        issuer: &str,
        c: IdClaims,
        nonce: Option<&str>,
    ) -> Result<OidcIdentity, OidcError> {
        if let Some(iat) = c.iat
            && iat > now() + SKEW_SECS
        {
            return Err(OidcError::Invalid("id_token issued in the future".into()));
        }
        if let Some(expected) = nonce
            && !c.nonce.as_deref().is_some_and(|n| constant_eq(n, expected))
        {
            return Err(OidcError::Invalid("id_token nonce mismatch".into()));
        }
        // With several audiences the token must be issued to us (azp).
        if let Some(aud) = c.rest.get("aud").and_then(|a| a.as_array())
            && aud.len() > 1
            && c.rest.get("azp").and_then(|a| a.as_str()) != Some(self.cfg.client_id.as_str())
        {
            return Err(OidcError::Invalid("id_token azp is not this client".into()));
        }
        let groups: Vec<String> = match c.rest.get(&self.cfg.groups_claim) {
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|g| g.as_str().map(String::from))
                .collect(),
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        // The preset's checks, and its word on MFA and the address where
        // it has one.
        let preset = match &self.preset.rule {
            Some(rule) => rule.check(&c.rest).map_err(OidcError::NotAllowed)?,
            None => Checked::default(),
        };
        let email_verified =
            preset
                .email_verified
                .unwrap_or_else(|| match c.rest.get("email_verified") {
                    Some(serde_json::Value::Bool(b)) => *b,
                    Some(serde_json::Value::String(s)) => s == "true",
                    None => self.cfg.trust_email,
                    Some(_) => false,
                });
        if !self.cfg.allowed_domains.is_empty() {
            // An address nobody checked proves nothing about its domain.
            let domain = c
                .email
                .as_deref()
                .filter(|_| email_verified)
                .and_then(|e| e.rsplit_once('@'))
                .map(|(_, d)| d.to_ascii_lowercase());
            if !domain.is_some_and(|d| {
                self.cfg
                    .allowed_domains
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&d))
            }) {
                return Err(OidcError::NotAllowed("email domain is not allowed".into()));
            }
        }
        if !self.cfg.required_groups.is_empty()
            && !groups.iter().any(|g| self.cfg.required_groups.contains(g))
        {
            return Err(OidcError::NotAllowed("not in a required group".into()));
        }
        let display_name = c
            .name
            .clone()
            .or(c.preferred_username.clone())
            .or(c.email.clone())
            .unwrap_or_else(|| c.sub.clone());
        let amr_mfa = matches!(c.rest.get("amr"), Some(serde_json::Value::Array(a)) if a.iter().any(|m| m.as_str() == Some("mfa")));
        let acr_mfa = c
            .rest
            .get("acr")
            .and_then(|a| a.as_str())
            .is_some_and(|a| self.cfg.mfa_acr.iter().any(|m| m == a));
        Ok(OidcIdentity {
            issuer: issuer.to_string(),
            subject: c.sub,
            display_name,
            email: c.email,
            groups,
            mfa: preset.mfa.unwrap_or(amr_mfa || acr_mfa),
            email_verified,
        })
    }

    /// Start a device grant (a CLI's login).
    pub async fn device_start(&self) -> Result<DeviceStart, OidcError> {
        let d = self.discovery().await?;
        let endpoint = d.device_authorization_endpoint.clone().ok_or_else(|| {
            OidcError::Provider("the IdP has no device authorization endpoint".into())
        })?;
        let secret = self.client_secret()?;
        let resp = self
            .http
            .post(&endpoint)
            .basic_auth(form_encode(&self.cfg.client_id), Some(form_encode(&secret)))
            .form(&[
                ("client_id", self.cfg.client_id.as_str()),
                ("scope", &self.cfg.scopes.join(" ")),
            ])
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| OidcError::Provider(e.to_string()))?;
        let start: DeviceStart = resp
            .json()
            .await
            .map_err(|e| OidcError::Invalid(format!("device response: {}", e)))?;
        self.devices
            .lock()
            .unwrap()
            .insert(start.device_code.clone(), now() + start.expires_in);
        Ok(start)
    }

    /// Poll a device grant started here.
    pub async fn device_poll(&self, device_code: &str) -> Result<OidcIdentity, OidcError> {
        let expires = *self
            .devices
            .lock()
            .unwrap()
            .get(device_code)
            .ok_or_else(|| OidcError::Invalid("unknown device code".into()))?;
        if expires <= now() {
            self.devices.lock().unwrap().remove(device_code);
            return Err(OidcError::Invalid("device code expired".into()));
        }
        let d = self.discovery().await?;
        let resp = self
            .token_request(
                &d,
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", device_code),
                ],
            )
            .await?;
        self.devices.lock().unwrap().remove(device_code);
        let id_token = resp
            .get("id_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| OidcError::Invalid("token response has no id_token".into()))?;
        self.verify(&d, id_token, None).await
    }
}

/// `Sha256::digest` returning a `Vec`, without pulling the trait in here.
trait DigestBytes {
    fn digest_bytes(data: &[u8]) -> Vec<u8>;
}

impl DigestBytes for sha2::Sha256 {
    fn digest_bytes(data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        sha2::Sha256::digest(data).to_vec()
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn client_credentials_are_form_encoded_for_basic() {
        assert_eq!(super::form_encode("glidex-web_1.0"), "glidex-web_1.0");
        assert_eq!(
            super::form_encode("a+b/c=d%e~f g"),
            "a%2Bb%2Fc%3Dd%25e%7Ef+g"
        );
    }

    use super::*;

    #[test]
    fn return_to_is_same_site_only() {
        assert_eq!(safe_return_to(Some("/vms/1")), "/vms/1");
        assert_eq!(safe_return_to(Some("//evil.example")), "/");
        assert_eq!(safe_return_to(Some("https://evil.example")), "/");
        assert_eq!(safe_return_to(Some("/\\evil")), "/");
        assert_eq!(safe_return_to(None), "/");
    }

    fn client(enabled: bool) -> OidcState<&'static str> {
        OidcState::new(
            ClientConfig {
                enabled,
                issuer: "https://idp.example".into(),
                client_id: "app".into(),
                scopes: vec!["openid".into()],
                groups_claim: "groups".into(),
                allowed_domains: vec!["example.org".into()],
                trust_email: false,
                required_groups: vec!["staff".into()],
                redirect_uri: None,
                callback_path: "/auth/callback".into(),
                mfa_acr: vec!["phr".into()],
            },
            ClientSecret::new(|| Err("no secret".into())),
        )
    }

    #[test]
    fn login_state_is_single_use_and_bound_to_its_browser() {
        let c = client(true);
        let (state, nonce, challenge, browser) = c.remember(Some("//evil"), "login");
        assert!(state.len() >= 30 && nonce.len() >= 30 && browser.len() >= 30);
        // S256 of a 48-byte verifier: 32 bytes, unpadded base64url.
        assert_eq!(challenge.len(), 43);
        assert!(matches!(
            c.take(&state, Some("someone else")),
            Err(OidcError::Invalid(m)) if m.contains("another browser")
        ));
        // The failed attempt used the state up.
        assert!(c.take(&state, Some(&browser)).is_err());
        let (state, _, _, browser) = c.remember(Some("/vms"), "invite");
        let p = c.take(&state, Some(&browser)).unwrap();
        assert_eq!((p.return_to.as_str(), p.after), ("/vms", "invite"));
        assert!(matches!(
            c.take(&state, Some(&browser)),
            Err(OidcError::Invalid(m)) if m.contains("unknown")
        ));
        assert!(c.take("nope", None).is_err());
    }

    #[test]
    fn login_state_expires_after_ten_minutes() {
        let c = client(true);
        let (fresh, _, _, browser) = c.remember(None, "login");
        c.pending.lock().unwrap().get_mut(&fresh).unwrap().created = now() - STATE_SECS;
        assert!(matches!(
            c.take(&fresh, Some(&browser)),
            Err(OidcError::Invalid(m)) if m == "login state expired"
        ));
        // A new login forgets the expired ones.
        let (old, _, _, _) = c.remember(None, "login");
        c.pending.lock().unwrap().get_mut(&old).unwrap().created = now() - STATE_SECS;
        c.remember(None, "login");
        assert!(!c.pending.lock().unwrap().contains_key(&old));
    }

    #[tokio::test]
    async fn disabled_clients_and_unknown_device_codes_need_no_network() {
        assert!(!client(false).enabled());
        assert!(matches!(
            client(false).discovery().await,
            Err(OidcError::Disabled)
        ));
        assert!(matches!(
            client(true).device_poll("x").await,
            Err(OidcError::Invalid(m)) if m == "unknown device code"
        ));
        assert_eq!(
            client(true).redirect_uri(Some("https://ui.example/")),
            "https://ui.example/auth/callback"
        );
    }

    fn claims(v: serde_json::Value) -> IdClaims {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn login_rules_on_verified_claims() {
        let c = client(true);
        let ok = |extra: serde_json::Value| {
            let mut v = serde_json::json!({
                "sub": "s1", "email": "ann@Example.org", "email_verified": true,
                "groups": ["staff"], "nonce": "n",
            });
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            c.identity("https://idp.example", claims(v), Some("n"))
        };
        let id = ok(serde_json::json!({ "acr": "phr" })).unwrap();
        assert_eq!(id.display_name, "ann@Example.org");
        assert!(id.mfa && id.email_verified);
        assert!(matches!(
            ok(serde_json::json!({ "nonce": "other" })),
            Err(OidcError::Invalid(_))
        ));
        assert!(matches!(
            ok(serde_json::json!({ "email": "ann@elsewhere.org" })),
            Err(OidcError::NotAllowed(_))
        ));
        assert!(matches!(
            ok(serde_json::json!({ "groups": "visitors" })),
            Err(OidcError::NotAllowed(_))
        ));
        assert!(matches!(
            ok(serde_json::json!({ "iat": now() + SKEW_SECS + 5 })),
            Err(OidcError::Invalid(_))
        ));
        assert!(matches!(
            ok(serde_json::json!({ "aud": ["app", "other"], "azp": "other" })),
            Err(OidcError::Invalid(_))
        ));
        let v = ok(serde_json::json!({ "amr": ["pwd", "mfa"], "email_verified": "true" })).unwrap();
        assert!(v.mfa && v.email_verified);
    }

    /// D75: a domain rule reads only an address the IdP verified, or one
    /// it says nothing about when the connector trusts its addresses.
    #[test]
    fn allowed_domains_need_a_verified_address() {
        let login = |trust_email: bool, verified: Option<serde_json::Value>| {
            let mut c = client(true);
            c.cfg.trust_email = trust_email;
            let mut v = serde_json::json!({
                "sub": "s1", "email": "ann@example.org", "groups": ["staff"],
            });
            if let Some(x) = verified {
                v["email_verified"] = x;
            }
            c.identity("https://idp.example", claims(v), None)
        };
        let refused = |r: Result<OidcIdentity, OidcError>| matches!(r, Err(OidcError::NotAllowed(m)) if m.contains("domain"));
        assert!(login(false, Some(true.into())).unwrap().email_verified);
        assert!(login(false, Some("true".into())).unwrap().email_verified);
        assert!(refused(login(false, None)));
        assert!(refused(login(false, Some(false.into()))));
        assert!(refused(login(false, Some("yes".into()))));
        // Trusted: a missing claim counts, an explicit `false` doesn't.
        assert!(login(true, None).unwrap().email_verified);
        assert!(refused(login(true, Some(false.into()))));
        assert!(refused(login(true, Some("false".into()))));
        // Without a domain rule, an unverified address still logs in,
        // marked unverified for the rules downstream.
        let mut c = client(true);
        c.cfg.allowed_domains.clear();
        let v = serde_json::json!({ "sub": "s1", "email": "x@any.org", "groups": ["staff"] });
        assert!(!c.identity("i", claims(v), None).unwrap().email_verified);
    }

    #[test]
    fn a_preset_decides_where_it_has_a_word() {
        struct Always;
        impl ClaimCheck for Always {
            fn check(&self, c: &HashMap<String, serde_json::Value>) -> Result<Checked, String> {
                if c.contains_key("blocked") {
                    return Err("blocked".into());
                }
                Ok(Checked {
                    mfa: Some(false),
                    email_verified: Some(true),
                })
            }
        }
        let c = client(true).with_preset(Preset {
            rule: Some(Arc::new(Always)),
            ..Default::default()
        });
        let v = serde_json::json!({
            "sub": "s", "email": "a@example.org", "groups": ["staff"], "amr": ["mfa"],
        });
        let id = c.identity("i", claims(v.clone()), None).unwrap();
        assert!(!id.mfa && id.email_verified);
        let mut b = v;
        b["blocked"] = true.into();
        assert!(matches!(
            c.identity("i", claims(b), None),
            Err(OidcError::NotAllowed(m)) if m == "blocked"
        ));
    }
}
