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

use crate::config::OidcConfig;
use base64::Engine as _;
use jsonwebtoken::{jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a login `state` stays valid.
const STATE_SECS: u64 = 600;
/// Allowed clock skew for `iat`/`exp`.
const SKEW_SECS: u64 = 60;
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

struct Pending {
    nonce: String,
    verifier: String,
    return_to: String,
    browser: String,
    created: u64,
}

struct Jwks {
    set: JwkSet,
    fetched: Instant,
}

/// Claims glidex reads from an ID token.
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

/// A verified login: what [`AuthService`](super::AuthService) needs.
#[derive(Debug, Clone)]
pub struct OidcIdentity {
    pub issuer: String,
    pub subject: String,
    pub display_name: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
}

/// What `start` returns for the browser.
pub struct Start {
    pub redirect: String,
    /// Value for the `gx_oidc` browser-binding cookie.
    pub browser: String,
}

/// Device grant started for gxctl.
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

pub struct OidcState {
    cfg: OidcConfig,
    http: reqwest::Client,
    discovery: tokio::sync::Mutex<Option<Discovery>>,
    jwks: tokio::sync::Mutex<Option<Jwks>>,
    pending: Mutex<HashMap<String, Pending>>,
    /// Device grants: device_code → nonce-free marker of when it started.
    devices: Mutex<HashMap<String, u64>>,
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn now() -> u64 {
    crate::tenancy::now()
}

/// A same-site path to return to after login; anything else becomes `/`.
pub fn safe_return_to(r: Option<&str>) -> String {
    match r {
        Some(p) if p.starts_with('/') && !p.starts_with("//") && !p.contains('\\') && !p.contains("://") => p.to_string(),
        _ => "/".to_string(),
    }
}

impl OidcState {
    pub fn new(cfg: &OidcConfig) -> Self {
        Self {
            cfg: cfg.clone(),
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

    pub fn enabled(&self) -> bool {
        self.cfg.enabled && !self.cfg.issuer.is_empty() && !self.cfg.client_id.is_empty()
    }

    pub fn config(&self) -> &OidcConfig {
        &self.cfg
    }

    fn client_secret(&self) -> Result<String, OidcError> {
        let path = crate::config::credential("oidc-client-secret")
            .or_else(|| std::env::var_os("GLIDEX_OIDC_CLIENT_SECRET_FILE").map(Into::into))
            .ok_or_else(|| OidcError::Provider("no oidc-client-secret credential".into()))?;
        std::fs::read_to_string(&path)
            .map(|s| s.trim().to_string())
            .map_err(|e| OidcError::Provider(format!("reading the client secret: {}", e)))
    }

    pub async fn discovery(&self) -> Result<Discovery, OidcError> {
        if !self.enabled() {
            return Err(OidcError::Disabled);
        }
        let mut cached = self.discovery.lock().await;
        if let Some(d) = cached.as_ref() {
            return Ok(d.clone());
        }
        let url = format!("{}/.well-known/openid-configuration", self.cfg.issuer.trim_end_matches('/'));
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
            return Err(OidcError::Invalid(format!("discovery issuer {} != configured {}", d.issuer, self.cfg.issuer)));
        }
        *cached = Some(d.clone());
        Ok(d)
    }

    async fn jwks(&self, d: &Discovery, refresh: bool) -> Result<JwkSet, OidcError> {
        let mut cached = self.jwks.lock().await;
        if let Some(j) = cached.as_ref() {
            if !refresh || j.fetched.elapsed() < JWKS_REFRESH_MIN {
                return Ok(j.set.clone());
            }
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
        *cached = Some(Jwks { set: set.clone(), fetched: Instant::now() });
        Ok(set)
    }

    pub fn redirect_uri(&self, default_origin: Option<&str>) -> String {
        self.cfg.redirect_uri.clone().unwrap_or_else(|| {
            format!("{}/api/auth/oidc/callback", default_origin.unwrap_or("http://localhost:5173").trim_end_matches('/'))
        })
    }

    /// Begin a browser login. `reauth` forces a fresh login (step-up).
    pub async fn start(&self, return_to: Option<&str>, redirect_uri: &str, reauth: bool) -> Result<Start, OidcError> {
        let d = self.discovery().await?;
        let state = super::random_token(24);
        let nonce = super::random_token(24);
        let verifier = super::random_token(48);
        let browser = super::random_token(24);
        let challenge = b64url(&sha2::Sha256::digest_bytes(verifier.as_bytes()));
        {
            let mut p = self.pending.lock().unwrap();
            let t = now();
            p.retain(|_, v| v.created + STATE_SECS > t);
            p.insert(
                state.clone(),
                Pending { nonce: nonce.clone(), verifier, return_to: safe_return_to(return_to), browser: browser.clone(), created: t },
            );
        }
        let mut url = reqwest::Url::parse(&d.authorization_endpoint).map_err(|e| OidcError::Invalid(e.to_string()))?;
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
        }
        Ok(Start { redirect: url.to_string(), browser })
    }

    /// Finish a browser login: check `state` against the browser cookie,
    /// redeem the code, verify the ID token. Returns the identity and the
    /// path to return to.
    pub async fn callback(
        &self,
        code: &str,
        state: &str,
        browser: Option<&str>,
        redirect_uri: &str,
    ) -> Result<(OidcIdentity, String), OidcError> {
        let pending = self
            .pending
            .lock()
            .unwrap()
            .remove(state)
            .ok_or_else(|| OidcError::Invalid("unknown or expired login state".into()))?;
        if pending.created + STATE_SECS <= now() {
            return Err(OidcError::Invalid("login state expired".into()));
        }
        if !browser.is_some_and(|b| super::constant_eq(b, &pending.browser)) {
            return Err(OidcError::Invalid("login state belongs to another browser".into()));
        }
        let d = self.discovery().await?;
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
            .await?;
        let id_token = resp
            .get("id_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| OidcError::Invalid("token response has no id_token".into()))?;
        let identity = self.verify(&d, id_token, Some(&pending.nonce)).await?;
        Ok((identity, pending.return_to))
    }

    async fn token_request(&self, d: &Discovery, form: &[(&str, &str)]) -> Result<serde_json::Value, OidcError> {
        let secret = self.client_secret()?;
        let resp = self
            .http
            .post(&d.token_endpoint)
            .basic_auth(&self.cfg.client_id, Some(secret))
            .form(form)
            .send()
            .await
            .map_err(|e| OidcError::Provider(e.to_string()))?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.map_err(|e| OidcError::Invalid(format!("token response: {}", e)))?;
        if !status.is_success() {
            let err = body.get("error").and_then(|v| v.as_str()).unwrap_or("error");
            return Err(match err {
                "authorization_pending" => OidcError::Pending,
                "slow_down" => OidcError::SlowDown,
                _ => OidcError::Provider(format!("token endpoint: {}", err)),
            });
        }
        Ok(body)
    }

    /// Verify an ID token and apply the login rules (domains, groups).
    pub async fn verify(&self, d: &Discovery, id_token: &str, nonce: Option<&str>) -> Result<OidcIdentity, OidcError> {
        let header = jsonwebtoken::decode_header(id_token).map_err(|e| OidcError::Invalid(format!("id_token header: {}", e)))?;
        if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
            return Err(OidcError::Invalid(format!("id_token algorithm {:?} is not allowed", header.alg)));
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
            None => find(&self.jwks(d, true).await?).ok_or_else(|| OidcError::Invalid("id_token key not in JWKS".into()))?,
        };
        let key = DecodingKey::from_jwk(&jwk).map_err(|e| OidcError::Invalid(format!("jwk: {}", e)))?;
        let mut v = Validation::new(header.alg);
        v.set_issuer(&[d.issuer.as_str()]);
        v.set_audience(&[self.cfg.client_id.as_str()]);
        v.leeway = SKEW_SECS;
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let data = jsonwebtoken::decode::<IdClaims>(id_token, &key, &v).map_err(|e| OidcError::Invalid(format!("id_token: {}", e)))?;
        let c = data.claims;
        if let Some(iat) = c.iat {
            if iat > now() + SKEW_SECS {
                return Err(OidcError::Invalid("id_token issued in the future".into()));
            }
        }
        if let Some(expected) = nonce {
            if !c.nonce.as_deref().is_some_and(|n| super::constant_eq(n, expected)) {
                return Err(OidcError::Invalid("id_token nonce mismatch".into()));
            }
        }
        // With several audiences the token must be issued to us (azp).
        if let Some(aud) = c.rest.get("aud").and_then(|a| a.as_array()) {
            if aud.len() > 1 && c.rest.get("azp").and_then(|a| a.as_str()) != Some(self.cfg.client_id.as_str()) {
                return Err(OidcError::Invalid("id_token azp is not this client".into()));
            }
        }
        let groups: Vec<String> = match c.rest.get(&self.cfg.groups_claim) {
            Some(serde_json::Value::Array(a)) => a.iter().filter_map(|g| g.as_str().map(String::from)).collect(),
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            _ => Vec::new(),
        };
        if !self.cfg.allowed_domains.is_empty() {
            let domain = c.email.as_deref().and_then(|e| e.rsplit_once('@')).map(|(_, d)| d.to_ascii_lowercase());
            if !domain.is_some_and(|d| self.cfg.allowed_domains.iter().any(|a| a.eq_ignore_ascii_case(&d))) {
                return Err(OidcError::NotAllowed("email domain is not allowed".into()));
            }
        }
        if !self.cfg.required_groups.is_empty() && !groups.iter().any(|g| self.cfg.required_groups.contains(g)) {
            return Err(OidcError::NotAllowed("not in a required group".into()));
        }
        let display_name = c
            .name
            .clone()
            .or(c.preferred_username.clone())
            .or(c.email.clone())
            .unwrap_or_else(|| c.sub.clone());
        Ok(OidcIdentity { issuer: d.issuer.clone(), subject: c.sub, display_name, email: c.email, groups })
    }

    /// Start a device grant (gxctl `login --oidc`).
    pub async fn device_start(&self) -> Result<DeviceStart, OidcError> {
        let d = self.discovery().await?;
        let endpoint = d
            .device_authorization_endpoint
            .clone()
            .ok_or_else(|| OidcError::Provider("the IdP has no device authorization endpoint".into()))?;
        let secret = self.client_secret()?;
        let resp = self
            .http
            .post(&endpoint)
            .basic_auth(&self.cfg.client_id, Some(secret))
            .form(&[("client_id", self.cfg.client_id.as_str()), ("scope", &self.cfg.scopes.join(" "))])
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| OidcError::Provider(e.to_string()))?;
        let start: DeviceStart = resp.json().await.map_err(|e| OidcError::Invalid(format!("device response: {}", e)))?;
        self.devices.lock().unwrap().insert(start.device_code.clone(), now() + start.expires_in);
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
                &[("grant_type", "urn:ietf:params:oauth:grant-type:device_code"), ("device_code", device_code)],
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
