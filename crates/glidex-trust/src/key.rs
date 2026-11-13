//! Signing what a cluster says to its trusted directories (D63): the
//! cluster key, an ES256 (P-256) key whose public half each directory
//! pins at the handshake.

use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use base64::Engine as _;
use serde_json::{Value, json};

/// Signs a JWT (ES256) with the key the other side pinned. The glidex
/// control plane signs with its stored, sealed key; another cluster can
/// use a [`ClusterKey`].
pub trait Signer: Send + Sync {
    /// `claims` as a compact JWS of type `typ`.
    fn sign_jwt(&self, typ: &str, claims: &Value) -> Result<String, String>;
}

fn b64u(v: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v)
}

/// A P-256 key in memory with its `kid`. Its holder keeps it sealed at
/// rest ([`ClusterKey::to_pkcs8`]) and rotates it by publishing a new
/// key beside the old one ([`jwks`]) until the old one's assertions are
/// past (60 s).
pub struct ClusterKey {
    kid: String,
    pair: EcdsaKeyPair,
}

impl std::fmt::Debug for ClusterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterKey")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl ClusterKey {
    /// A new key named `kid`.
    pub fn generate(kid: &str) -> Result<ClusterKey, String> {
        let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING)
            .map_err(|_| "a new P-256 key: not made".to_string())?;
        Ok(ClusterKey {
            kid: kid.into(),
            pair,
        })
    }

    /// A key from its PKCS#8 (v1) DER.
    pub fn from_pkcs8(kid: &str, der: &[u8]) -> Result<ClusterKey, String> {
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der)
            .map_err(|_| "not a P-256 PKCS#8 key".to_string())?;
        Ok(ClusterKey {
            kid: kid.into(),
            pair,
        })
    }

    /// Its private key as PKCS#8 (v1) DER: secret; seal it before storing.
    pub fn to_pkcs8(&self) -> Result<Vec<u8>, String> {
        self.pair
            .to_pkcs8v1()
            .map(|d| d.as_ref().to_vec())
            .map_err(|_| "the key doesn't serialize".to_string())
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// Its public JWK.
    pub fn jwk(&self) -> Value {
        // Uncompressed SEC1: 0x04, then x and y.
        let point = self.pair.public_key().as_ref();
        json!({
            "kty": "EC", "crv": "P-256", "x": b64u(&point[1..33]), "y": b64u(&point[33..65]),
            "kid": self.kid, "use": "sig", "alg": "ES256",
        })
    }
}

/// A JWKS of `keys`' public halves: what a cluster sends at the
/// handshake.
pub fn jwks(keys: &[&ClusterKey]) -> Value {
    json!({ "keys": keys.iter().map(|k| k.jwk()).collect::<Vec<_>>() })
}

impl Signer for ClusterKey {
    fn sign_jwt(&self, typ: &str, claims: &Value) -> Result<String, String> {
        let header = json!({ "alg": "ES256", "typ": typ, "kid": self.kid });
        let input = format!(
            "{}.{}",
            b64u(header.to_string().as_bytes()),
            b64u(claims.to_string().as_bytes())
        );
        // FIXED_SIGNING: r ‖ s, as JWS's ES256 wants (RFC 7518 §3.4).
        let sig = self
            .pair
            .sign(&aws_lc_rs::rand::SystemRandom::new(), input.as_bytes())
            .map_err(|_| "signing failed".to_string())?;
        Ok(format!("{input}.{}", b64u(sig.as_ref())))
    }
}

impl<F: Fn(&str, &Value) -> Result<String, String> + Send + Sync> Signer for F {
    fn sign_jwt(&self, typ: &str, claims: &Value) -> Result<String, String> {
        self(typ, claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_survives_pkcs8_and_its_jwts_verify() {
        let k = ClusterKey::generate("c1:1").unwrap();
        let back = ClusterKey::from_pkcs8("c1:1", &k.to_pkcs8().unwrap()).unwrap();
        assert_eq!(k.jwk(), back.jwk());
        let jwt = back.sign_jwt("JWT", &json!({ "a": 1 })).unwrap();
        let set: jsonwebtoken::jwk::JwkSet = serde_json::from_value(jwks(&[&k])).unwrap();
        let key = jsonwebtoken::DecodingKey::from_jwk(set.find("c1:1").unwrap()).unwrap();
        let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
        v.required_spec_claims.clear();
        v.validate_exp = false;
        let c = jsonwebtoken::decode::<Value>(&jwt, &key, &v)
            .unwrap()
            .claims;
        assert_eq!(c["a"], 1);
        assert!(ClusterKey::from_pkcs8("x", b"junk").is_err());
    }
}
