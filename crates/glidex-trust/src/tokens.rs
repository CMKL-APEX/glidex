//! Checking a trusted directory's tokens at a cluster (D69, §7.4): its
//! access tokens addressed to the cluster, the ID tokens of browser
//! logins, and API tokens (`gxt_…`) its feed lists by hash.
//!
//! These check the signature and the claims against the keys of the
//! directory's last feed; the caller still checks that the user (`sub`)
//! is in its replica, enabled, and that the replica is fresh
//! ([`crate::cluster::freshness`]).

use crate::wire::TrustedDirectory;
use serde_json::Value;

/// An access token's `typ` (RFC 9068).
pub const ACCESS_TYP: &str = "at+jwt";
/// An API token's prefix.
pub const API_TOKEN_PREFIX: &str = "gxt_";
/// The clock skew allowed on `exp` and `nbf`.
pub const LEEWAY_SECS: u64 = 30;

/// An API token's key in the feed's `api_tokens` table: the SHA-256
/// (hex) of its secret.
pub fn api_token_hash(secret: &str) -> String {
    crate::sha256_hex(secret.as_bytes())
}

/// A JWT's `iss`, unverified: only to pick the keys to verify it with.
pub fn unverified_issuer(jwt: &str) -> Option<String> {
    use base64::Engine as _;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v["iss"].as_str().map(String::from)
}

fn verified(jwt: &str, d: &TrustedDirectory, aud: &str) -> Option<Value> {
    let header = jsonwebtoken::decode_header(jwt).ok()?;
    let set: jsonwebtoken::jwk::JwkSet = serde_json::from_value(d.jwks.clone()).ok()?;
    let jwk = set.find(header.kid.as_deref()?)?;
    let dk = jsonwebtoken::DecodingKey::from_jwk(jwk).ok()?;
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
    v.set_audience(&[aud]);
    v.set_issuer(&[d.issuer.as_str()]);
    v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    v.leeway = LEEWAY_SECS;
    jsonwebtoken::decode::<Value>(jwt, &dk, &v)
        .ok()
        .map(|t| t.claims)
}

/// The claims of `jwt`, if it is an access token of trusted directory `d`
/// addressed to cluster `cluster_id`, signed with a key of its last feed.
/// `sub` is the user; `sid` the login's chain; `auth_time`;
/// `glidex.mfa`.
pub fn verify_access_token(jwt: &str, d: &TrustedDirectory, cluster_id: &str) -> Option<Value> {
    let header = jsonwebtoken::decode_header(jwt).ok()?;
    if header.typ.as_deref() != Some(ACCESS_TYP) {
        return None;
    }
    verified(jwt, d, cluster_id)
}

/// The claims of `jwt`, if it is trusted directory `d`'s ID token for
/// client `client_id` (`cluster:<id>`) with `nonce` and a `sid`.
pub fn verify_id_token(
    jwt: &str,
    d: &TrustedDirectory,
    client_id: &str,
    nonce: &str,
) -> Option<Value> {
    let c = verified(jwt, d, client_id)?;
    (c["nonce"].as_str() == Some(nonce) && c["sid"].is_string()).then_some(c)
}

/// Whether an ID token's `amr` says the login was multi-factor.
pub fn mfa_of(id_claims: &Value) -> bool {
    id_claims["amr"]
        .as_array()
        .is_some_and(|a| a.iter().any(|m| m == "mfa" || m == "hwk"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::{ClusterKey, Signer, jwks};
    use serde_json::json;

    fn directory(k: &ClusterKey) -> TrustedDirectory {
        TrustedDirectory {
            id: "trusted-x".into(),
            issuer: "https://x.example/d/x".into(),
            directory: "x".into(),
            api: String::new(),
            tls_sha256: String::new(),
            jwks: jwks(&[k]),
            tenants: vec![],
            created_at: 0,
            max_staleness_minutes: None,
        }
    }

    #[test]
    fn access_tokens_verify_for_their_cluster_only() {
        let k = ClusterKey::generate("x:1").unwrap();
        let d = directory(&k);
        let t = crate::now();
        let claims = json!({
            "iss": d.issuer, "sub": "u1", "aud": "c1", "iat": t, "exp": t + 600, "sid": "s1",
        });
        let at = k.sign_jwt(ACCESS_TYP, &claims).unwrap();
        assert_eq!(unverified_issuer(&at).as_deref(), Some(d.issuer.as_str()));
        assert_eq!(verify_access_token(&at, &d, "c1").unwrap()["sub"], "u1");
        assert!(verify_access_token(&at, &d, "c2").is_none());
        // An ID token isn't an access token.
        let id = k.sign_jwt("JWT", &claims).unwrap();
        assert!(verify_access_token(&id, &d, "c1").is_none());
        // Nor is one signed with a key the feed didn't bring.
        let other = ClusterKey::generate("x:1").unwrap();
        assert!(
            verify_access_token(&other.sign_jwt(ACCESS_TYP, &claims).unwrap(), &d, "c1").is_none()
        );
        // Expired.
        let old = json!({ "iss": d.issuer, "sub": "u1", "aud": "c1", "exp": t - 100 });
        assert!(verify_access_token(&k.sign_jwt(ACCESS_TYP, &old).unwrap(), &d, "c1").is_none());
    }

    #[test]
    fn id_tokens_need_their_nonce_and_a_sid() {
        let k = ClusterKey::generate("x:1").unwrap();
        let d = directory(&k);
        let t = crate::now();
        let c = |sid: bool| {
            let mut c = json!({
                "iss": d.issuer, "sub": "u1", "aud": "cluster:c1", "exp": t + 60,
                "nonce": "n1", "amr": ["pwd", "mfa"],
            });
            if sid {
                c["sid"] = "s1".into();
            }
            k.sign_jwt("JWT", &c).unwrap()
        };
        let ok = verify_id_token(&c(true), &d, "cluster:c1", "n1").unwrap();
        assert!(mfa_of(&ok));
        assert!(verify_id_token(&c(true), &d, "cluster:c1", "n2").is_none());
        assert!(verify_id_token(&c(false), &d, "cluster:c1", "n1").is_none());
        assert_eq!(api_token_hash("gxt_a"), crate::sha256_hex(b"gxt_a"));
    }
}
