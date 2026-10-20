//! The token exchange (spec/tenancy.md §9, D48–D50): a workload's JWT, from
//! an issuer one of a tenant's federations trusts, for a short-lived token
//! acting as the service account its first matching rule names. Every
//! refusal is the same `invalid_grant`; why goes to the audit log.

use crate::auth::AuthService;
use crate::auth::store::TokenKind;
use crate::directory::federation::{FederatedToken, Federation, FederationError, MAX_TTL_SECS};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Instant;

pub const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
pub const JWT_TYPE: &str = "urn:ietf:params:oauth:token-type:jwt";
pub const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
/// Exchanges a minute per federation (§9).
const PER_MINUTE: f64 = 30.0;
const LEEWAY_SECS: u64 = 60;
/// Claims kept on the token for audit, besides `sub`.
const NOTED: [&str; 5] = [
    "repository",
    "ref",
    "environment",
    "project_path",
    "workflow",
];

/// What the client sent.
#[derive(Debug, Default, Clone)]
pub struct Request {
    pub grant_type: String,
    pub subject_token: String,
    pub subject_token_type: String,
    /// `<tenant slug>/<federation id>`.
    pub federation: String,
}

/// An answer: RFC 6749 §5.1 or §5.2.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Granted {
        token: String,
        expires_in: u64,
    },
    /// `invalid_request`: the request itself is malformed.
    Malformed(String),
    /// `invalid_grant`, for every other refusal; the detail is for audit.
    Refused(String),
    TooMany,
}

impl Answer {
    pub fn status(&self) -> u16 {
        match self {
            Answer::Granted { .. } => 200,
            Answer::TooMany => 429,
            _ => 400,
        }
    }

    pub fn body(&self) -> Value {
        match self {
            Answer::Granted { token, expires_in } => json!({
                "access_token": token,
                "issued_token_type": ACCESS_TOKEN_TYPE,
                "token_type": "Bearer",
                "expires_in": expires_in,
            }),
            Answer::Malformed(m) => json!({"error": "invalid_request", "error_description": m}),
            // The same body whatever failed (D50).
            Answer::Refused(_) => {
                json!({"error": "invalid_grant", "error_description": "the token can't be exchanged"})
            }
            Answer::TooMany => {
                json!({"error": "slow_down", "error_description": "too many exchanges for this federation"})
            }
        }
    }
}

fn buckets() -> &'static Mutex<HashMap<String, (Instant, f64)>> {
    static B: std::sync::OnceLock<Mutex<HashMap<String, (Instant, f64)>>> =
        std::sync::OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}

fn take(key: &str) -> bool {
    let mut b = buckets().lock().unwrap();
    let now = Instant::now();
    let (at, level) = b.entry(key.to_string()).or_insert((now, PER_MINUTE));
    *level = (*level + now.duration_since(*at).as_secs_f64() * PER_MINUTE / 60.0).min(PER_MINUTE);
    *at = now;
    if *level >= 1.0 {
        *level -= 1.0;
        true
    } else {
        false
    }
}

fn hex_sha256(s: &str) -> String {
    use sha2::Digest;
    crate::cluster::pki::hex(&sha2::Sha256::digest(s.as_bytes()))
}

/// The verified claims of `jwt` for federation `f` (D49).
async fn verify(auth: &AuthService, f: &Federation, jwt: &str) -> Result<Value, String> {
    let header = jsonwebtoken::decode_header(jwt).map_err(|_| "not a JWT".to_string())?;
    if !matches!(header.alg, Algorithm::RS256 | Algorithm::ES256) {
        return Err(format!("alg {:?} isn't RS256 or ES256", header.alg));
    }
    let keys = auth
        .issuers
        .keys(&f.issuer, header.kid.as_deref())
        .await
        .map_err(|e| format!("the issuer's keys: {e}"))?;
    let jwk = match &header.kid {
        Some(kid) => keys.jwks.find(kid).cloned(),
        None if keys.jwks.keys.len() == 1 => keys.jwks.keys.first().cloned(),
        None => None,
    }
    .ok_or("signed by a key the issuer doesn't list")?;
    let key =
        DecodingKey::from_jwk(&jwk).map_err(|_| "the issuer's key can't be used".to_string())?;
    let mut v = Validation::new(header.alg);
    v.leeway = LEEWAY_SECS;
    v.set_issuer(&[f.issuer.as_str()]);
    v.set_audience(&[f.audience.as_str()]);
    v.set_required_spec_claims(&["exp", "iat", "iss", "aud"]);
    v.validate_nbf = true;
    let data =
        jsonwebtoken::decode::<Value>(jwt, &key, &v).map_err(|e| format!("{:?}", e.kind()))?;
    let c = data.claims;
    let (iat, exp) = (
        c.get("iat").and_then(Value::as_u64).unwrap_or(0),
        c.get("exp").and_then(Value::as_u64).unwrap_or(0),
    );
    if exp.saturating_sub(iat) > MAX_TTL_SECS as u64 + LEEWAY_SECS {
        return Err("the token is valid for more than an hour".into());
    }
    if iat > crate::tenancy::now() + LEEWAY_SECS {
        return Err("the token is from the future".into());
    }
    Ok(c)
}

/// Exchange `req`; every outcome for a named federation is audited.
pub async fn exchange(app: &crate::api::App, req: &Request, request_id: &str) -> Answer {
    let auth: &AuthService = &app.auth;
    if req.grant_type != GRANT_TYPE {
        return Answer::Malformed(format!("grant_type is {GRANT_TYPE}"));
    }
    if req.subject_token_type != JWT_TYPE {
        return Answer::Malformed(format!("subject_token_type is {JWT_TYPE}"));
    }
    if req.subject_token.is_empty() || req.subject_token.len() > 16 * 1024 {
        return Answer::Malformed("subject_token is a JWT".into());
    }
    let Some((slug, id)) = req.federation.split_once('/') else {
        return Answer::Malformed("federation is <tenant>/<federation id>".into());
    };
    if !take(&req.federation) {
        return Answer::TooMany;
    }
    let who = format!("system:federation:{}", req.federation);
    let refuse = |why: String, details: Value| {
        auth.audit_system(
            &who,
            request_id,
            "exchangeToken",
            None,
            "denied",
            Some("invalid_grant"),
            json!({"reason": why, "claims": details}),
        );
        Answer::Refused(why)
    };
    let tenant = match auth.tenants.resolve(slug) {
        Ok(t) => t,
        Err(_) => return refuse("no such tenant".into(), Value::Null),
    };
    let f = match auth.federations.get(&tenant.id, id) {
        Ok(Some(f)) if f.enabled => f,
        Ok(Some(_)) => return refuse("the federation is disabled".into(), Value::Null),
        _ => return refuse("no such federation".into(), Value::Null),
    };
    let claims = match verify(auth, &f, &req.subject_token).await {
        Ok(c) => c,
        Err(e) => return refuse(e, Value::Null),
    };
    let noted: BTreeMap<String, String> = std::iter::once("sub")
        .chain(NOTED)
        .filter_map(|k| Some((k.to_string(), claims.get(k)?.as_str()?.to_string())))
        .collect();
    let Some(rule) = f.rules.iter().find(|r| r.matches(&claims)) else {
        return refuse("no rule matches".into(), json!(noted));
    };
    // The service account is still the tenant's and live.
    let sa = match auth.store.token_by_id(&rule.service_account) {
        Ok(Some((_, t))) => t,
        _ => return refuse("the rule's service account is gone".into(), json!(noted)),
    };
    let now = crate::tenancy::now();
    let TokenKind::ServiceAccount { project } = &sa.kind else {
        return refuse("the rule names no service account".into(), json!(noted));
    };
    let owner = app
        .manager
        .projects()
        .get(project)
        .ok()
        .flatten()
        .map(|p| p.tenant_id);
    let ours = owner.as_deref() == Some(tenant.id.as_str());
    if !ours || sa.expires_at <= now {
        return refuse(
            "the rule's service account isn't the tenant's, or expired".into(),
            json!(noted),
        );
    }
    let exp = claims.get("exp").and_then(Value::as_u64).unwrap_or(now);
    let ttl = (rule.ttl_secs() as u64)
        .min(sa.expires_at - now)
        .min(MAX_TTL_SECS as u64);
    let replay_key = match claims.get("jti").and_then(Value::as_str) {
        Some(j) if !j.is_empty() => format!("jti:{j}"),
        _ => format!("sha256:{}", hex_sha256(&req.subject_token)),
    };
    let secret = format!(
        "{}{}",
        crate::auth::TOKEN_PREFIX,
        crate::auth::random_token(32)
    );
    let rec = FederatedToken {
        id: format!("fed-{}", uuid::Uuid::new_v4().simple()),
        federation: format!("{}/{}", tenant.id, f.id),
        rule: rule.fingerprint(),
        service_account: sa.id.clone(),
        subject: noted.get("sub").cloned().unwrap_or_default(),
        claims: noted.clone(),
        replay_key,
        subject_expires_at: exp,
        created_at: now,
        expires_at: now + ttl,
    };
    match auth.federated_tokens.mint(&hex_sha256(&secret), &rec) {
        Ok(()) => {}
        Err(FederationError::Conflict(_)) => {
            return refuse("the token was exchanged already".into(), json!(noted));
        }
        Err(e) => {
            tracing::error!("minting a federated token: {e}");
            return refuse("storage error".into(), json!(noted));
        }
    }
    auth.audit_system(
        &who,
        request_id,
        "exchangeToken",
        Some(&format!("token:{}", sa.id)),
        "ok",
        None,
        json!({"federated": rec.id, "claims": noted, "ttl_secs": ttl}),
    );
    Answer::Granted {
        token: secret,
        expires_in: ttl,
    }
}
