//! Requests between a directory and a cluster of different organizations
//! (spec/tenancy.md D63). No CA is shared, so:
//!
//! - each request carries a **signed assertion** (`Authorization:
//!   GlidexPeer <JWT>`): ES256, at most 60 s, single use by `jti`, signed
//!   by a directory's signing keys or by the cluster's key, and checked
//!   against the key set the other side pinned;
//! - each connection **pins** the server's certificate by its SHA-256.

use super::{ConnectorError, ConnectorStore};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// The `Authorization` scheme of a peer request.
pub const SCHEME: &str = "GlidexPeer";
/// The `typ` of a peer assertion.
pub const TYP: &str = "glidex-peer+jwt";
/// How long an assertion is good for.
pub const ASSERTION_SECS: u64 = 60;
/// A peer request's limit.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Why a peer request was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PeerError {
    #[error("no peer assertion")]
    Missing,
    #[error("the peer assertion doesn't verify: {0}")]
    Invalid(String),
    #[error("the peer assertion was used already")]
    Replayed,
}

/// `owner`'s assertion that `iss` asks `aud` for `purpose`: signed with
/// its active key (a directory's, or `issuer::CLUSTER_OWNER`'s).
pub fn assertion(
    keys: &ConnectorStore,
    owner: &str,
    iss: &str,
    aud: &str,
    purpose: &str,
    now: u64,
) -> Result<String, ConnectorError> {
    keys.sign_jwt(
        owner,
        TYP,
        &json!({
            "iss": iss, "aud": aud, "iat": now, "nbf": now, "exp": now + ASSERTION_SECS,
            "jti": crate::auth::random_token(16), "purpose": purpose,
        }),
    )
}

/// The assertion a request carries.
pub fn from_headers(h: &axum::http::HeaderMap) -> Option<String> {
    h.get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix(SCHEME)?
        .trim()
        .to_string()
        .into()
}

/// The `jti`s seen while they could still be presented.
#[derive(Default)]
pub struct Replays(Mutex<HashMap<String, u64>>);

impl Replays {
    /// `true` the first time `jti` is seen before `exp`.
    pub fn first(&self, jti: &str, exp: u64, now: u64) -> bool {
        let mut seen = self.0.lock().unwrap();
        seen.retain(|_, e| *e > now);
        if seen.contains_key(jti) {
            return false;
        }
        seen.insert(jti.to_string(), exp);
        true
    }
}

/// `jwt`'s claims, if it verifies against `jwks` (a JWKS: the keys pinned
/// for the peer) as `iss` asking `aud` for `purpose`, once.
pub fn verify(
    jwt: &str,
    jwks: &Value,
    iss: &str,
    aud: &str,
    purpose: &str,
    replays: &Replays,
    now: u64,
) -> Result<Value, PeerError> {
    let bad = |m: &str| PeerError::Invalid(m.to_string());
    let header = jsonwebtoken::decode_header(jwt).map_err(|e| bad(&e.to_string()))?;
    if header.typ.as_deref() != Some(TYP) || header.alg != jsonwebtoken::Algorithm::ES256 {
        return Err(bad("not a peer assertion"));
    }
    let set: jsonwebtoken::jwk::JwkSet =
        serde_json::from_value(jwks.clone()).map_err(|e| bad(&e.to_string()))?;
    let jwk = set
        .find(header.kid.as_deref().ok_or_else(|| bad("no kid"))?)
        .ok_or_else(|| bad("signed with a key the peer doesn't have"))?;
    let key = jsonwebtoken::DecodingKey::from_jwk(jwk).map_err(|e| bad(&e.to_string()))?;
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
    v.set_audience(&[aud]);
    v.set_issuer(&[iss]);
    v.set_required_spec_claims(&["exp", "iss", "aud"]);
    // Time is `now`'s, checked below.
    v.validate_exp = false;
    v.validate_nbf = false;
    let c = jsonwebtoken::decode::<Value>(jwt, &key, &v)
        .map_err(|e| bad(&e.to_string()))?
        .claims;
    let (exp, nbf) = (
        c["exp"].as_u64().unwrap_or(0),
        c["nbf"].as_u64().unwrap_or(0),
    );
    if now >= exp + 30 || nbf > now + 30 {
        return Err(bad("expired, or not yet good"));
    }
    if c["purpose"].as_str() != Some(purpose) {
        return Err(bad("an assertion for something else"));
    }
    if exp > now + ASSERTION_SECS + 30 {
        return Err(bad("an assertion good for too long"));
    }
    let jti = c["jti"].as_str().ok_or_else(|| bad("no jti"))?;
    if !replays.first(jti, exp + 30, now) {
        return Err(PeerError::Replayed);
    }
    Ok(c)
}

/// The SHA-256 of a certificate's DER, as the offer and pins write it.
pub fn fingerprint_of(der: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `POST <base><path>` with `body`, the server's certificate pinned to
/// `pin` (64 hex digits, its leaf's SHA-256), carrying `assertion`. The
/// status and JSON answer.
pub async fn post(
    base: &str,
    pin: &str,
    path: &str,
    assertion: &str,
    body: &Value,
) -> Result<(u16, Value), String> {
    let uri: axum::http::Uri = base.parse().map_err(|e| format!("{base}: {e}"))?;
    if uri.scheme_str() != Some("https") {
        return Err(format!("{base}: a peer is reached over https"));
    }
    let host = uri
        .host()
        .ok_or("no host")?
        .trim_matches(['[', ']'])
        .to_string();
    let port = uri.port_u16().unwrap_or(443);
    let pin = glidex_tls::Trust::normalize_pin(pin).ok_or("not a SHA-256 fingerprint")?;
    let tls = glidex_tls::ClientTls::with_trust(
        &glidex_tls::Trust {
            pins: vec![pin],
            // The pin is the server's identity; its name may be an address
            // its certificate doesn't list.
            verify_host: false,
            ..Default::default()
        },
        &[b"http/1.1"],
    )?;
    let exchange = async {
        let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| format!("{host}:{port}: {e}"))?;
        let name =
            rustls_pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
        let s = tokio_rustls::TlsConnector::from(tls.config.clone())
            .connect(name, tcp)
            .await
            .map_err(|e| format!("{host}:{port}: TLS: {e}"))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(s))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = hyper::Request::post(format!("{}{path}", base.trim_end_matches('/')))
            .header(
                hyper::header::HOST,
                uri.authority().map(|a| a.as_str()).unwrap_or(""),
            )
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .header(
                hyper::header::AUTHORIZATION,
                format!("{SCHEME} {assertion}"),
            )
            .body(Full::new(Bytes::from(body.to_string())))
            .map_err(|e| e.to_string())?;
        let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok::<_, String>((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    };
    tokio::time::timeout(TIMEOUT, exchange)
        .await
        .map_err(|_| format!("{base}: no answer"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (ConnectorStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(crate::store::Db::create(dir.path().join("t.db")).unwrap());
        (
            ConnectorStore::new(db, &crate::config::Config::default()).unwrap(),
            dir,
        )
    }

    #[test]
    fn assertions_verify_once_for_their_purpose_and_audience() {
        let (s, _d) = store();
        let t = crate::auth::store::now();
        s.maintain_signing_keys("local", t).unwrap();
        let jwks = s.jwks("local", t).unwrap();
        let iss = "https://g.example/d/local";
        let r = Replays::default();
        let a = assertion(&s, "local", iss, "c1", "redeem", t).unwrap();
        assert!(verify(&a, &jwks, iss, "c1", "redeem", &r, t).is_ok());
        assert_eq!(
            verify(&a, &jwks, iss, "c1", "redeem", &r, t + 1),
            Err(PeerError::Replayed)
        );
        let a = assertion(&s, "local", iss, "c1", "redeem", t).unwrap();
        for (iss2, aud, purpose) in [
            ("https://other/d/x", "c1", "redeem"),
            (iss, "c2", "redeem"),
            (iss, "c1", "end"),
        ] {
            assert!(
                matches!(
                    verify(&a, &jwks, iss2, aud, purpose, &r, t),
                    Err(PeerError::Invalid(_))
                ),
                "{iss2} {aud} {purpose}"
            );
        }
        // Expired, or signed by a key the peer doesn't hold.
        let a = assertion(&s, "local", iss, "c1", "redeem", t).unwrap();
        assert!(verify(&a, &jwks, iss, "c1", "redeem", &r, t + ASSERTION_SECS + 31).is_err());
        s.maintain_signing_keys(crate::directory::issuer::CLUSTER_OWNER, t)
            .unwrap();
        let a = assertion(
            &s,
            crate::directory::issuer::CLUSTER_OWNER,
            iss,
            "c1",
            "redeem",
            t,
        )
        .unwrap();
        assert!(verify(&a, &jwks, iss, "c1", "redeem", &r, t).is_err());
    }
}
