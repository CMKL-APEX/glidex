//! A directory as a token issuer (spec/19-tenancy.md D52, D53;
//! 29-workload-identity.md §4.1).
//!
//! Each directory signs with ES256 keys of its own, `kid` =
//! `<directory>:<n>`. The private key is sealed with the directory's data
//! key, like a connector secret (§13); only the public half is ever served.
//! The active key is replaced every [`ROTATE_SECS`] (or on demand); a
//! retired key stays in the JWKS for [`RETAIN_SECS`], longer than any token
//! it signed lives, and is then deleted.

use super::{ConnectorError, ConnectorStore, secrets};
use base64::Engine as _;
use p256::ecdsa::signature::Signer as _;
use p256::elliptic_curve::sec1::ToEncodedPoint as _;
use redb::TableDefinition;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) const SIGNING_KEYS: TableDefinition<&str, &[u8]> = TableDefinition::new("signing_keys");
/// This cluster's own key (D63): it signs what the cluster says to the
/// directories it trusts. Cluster scope: never in a directory's records.
pub(super) const CLUSTER_KEYS: TableDefinition<&str, &[u8]> = TableDefinition::new("cluster_keys");
/// The owner name the cluster key is kept and sealed under.
pub const CLUSTER_OWNER: &str = "~cluster";

type Def = TableDefinition<'static, &'static str, &'static [u8]>;

/// Where an owner's keys live: a directory's, or the cluster's.
fn table_of(owner: &str) -> Def {
    if owner == CLUSTER_OWNER {
        CLUSTER_KEYS
    } else {
        SIGNING_KEYS
    }
}

/// The active key's lifetime.
pub const ROTATE_SECS: u64 = 30 * 86_400;
/// How long a retired key is still published: past the longest token a
/// directory signs (an access token, at most 12 h, D55), with an hour to spare.
pub const RETAIN_SECS: u64 = 13 * 3_600;
/// The only algorithm.
pub const ALG: &str = "ES256";

/// What `SealedKey::private` is sealed as (the connector-secret format,
/// with a field no connector has).
const SEAL_AS: &str = "signing-key";

/// One signing key as stored: the public point in the clear, the private
/// scalar sealed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningKey {
    pub directory_id: String,
    pub n: u32,
    pub kid: String,
    pub alg: String,
    /// The public point, base64url (RFC 7518 §6.2.1).
    pub x: String,
    pub y: String,
    private: secrets::Sealed,
    pub created_at: u64,
    /// When a newer key replaced it; it is then only published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<u64>,
}

impl SigningKey {
    /// Its public JWK.
    pub fn jwk(&self) -> Value {
        json!({
            "kty": "EC", "crv": "P-256", "x": self.x, "y": self.y,
            "kid": self.kid, "use": "sig", "alg": self.alg,
        })
    }

    /// Published at `now`: active, or retired less than `RETAIN_SECS` ago.
    pub fn published(&self, now: u64) -> bool {
        self.retired_at.is_none_or(|r| now < r + RETAIN_SECS)
    }
}

fn b64u(v: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v)
}

fn row(directory: &str, n: u32) -> String {
    format!("{directory}/{n:08}")
}

impl ConnectorStore {
    /// `directory`'s signing keys, oldest first.
    pub fn signing_keys(&self, directory: &str) -> Result<Vec<SigningKey>, ConnectorError> {
        let prefix = format!("{directory}/");
        Ok(self
            .read_all::<SigningKey>(table_of(directory))?
            .into_iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, v)| v)
            .collect())
    }

    /// The key `directory` signs with now, if it has one.
    pub fn active_signing_key(
        &self,
        directory: &str,
    ) -> Result<Option<SigningKey>, ConnectorError> {
        Ok(self
            .signing_keys(directory)?
            .into_iter()
            .rfind(|k| k.retired_at.is_none()))
    }

    /// A new active key for `directory`; the one before it is retired.
    pub fn rotate_signing_key(
        &self,
        directory: &str,
        now: u64,
    ) -> Result<SigningKey, ConnectorError> {
        let keys = self.signing_keys(directory)?;
        let n = keys.last().map_or(1, |k| k.n + 1);
        let secret = p256::SecretKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let point = secret.public_key().to_encoded_point(false);
        let (Some(x), Some(y)) = (point.x(), point.y()) else {
            return Err(ConnectorError::Invalid(
                "a P-256 key without coordinates".into(),
            ));
        };
        let scalar = zeroize::Zeroizing::new(b64u(&secret.to_bytes()));
        let dk = self.data_key(directory)?;
        let key = SigningKey {
            directory_id: directory.into(),
            n,
            kid: format!("{directory}:{n}"),
            alg: ALG.into(),
            x: b64u(x),
            y: b64u(y),
            private: secrets::seal(&dk, directory, SEAL_AS, &n.to_string(), &scalar, now),
            created_at: now,
            retired_at: None,
        };
        for mut k in keys.into_iter().filter(|k| k.retired_at.is_none()) {
            k.retired_at = Some(now);
            self.put_raw(table_of(directory), &row(directory, k.n), &k)?;
        }
        self.put_raw(table_of(directory), &row(directory, n), &key)?;
        Ok(key)
    }

    /// Make `directory` a key when it has none or the active one is due,
    /// and delete retired keys nothing can still verify against. `true`
    /// when anything changed. Writes: on the leader or a standalone host.
    pub fn maintain_signing_keys(&self, directory: &str, now: u64) -> Result<bool, ConnectorError> {
        let mut changed = false;
        for k in self.signing_keys(directory)? {
            if !k.published(now) {
                self.remove_raw(table_of(directory), &row(directory, k.n))?;
                changed = true;
            }
        }
        let due = match self.active_signing_key(directory)? {
            Some(k) => now >= k.created_at + ROTATE_SECS,
            None => true,
        };
        if due {
            self.rotate_signing_key(directory, now)?;
            changed = true;
        }
        Ok(changed)
    }

    /// Forget `directory`'s keys and its data key (it was deleted).
    pub fn forget_directory_keys(&self, directory: &str) -> Result<(), ConnectorError> {
        for k in self.signing_keys(directory)? {
            self.remove_raw(table_of(directory), &row(directory, k.n))?;
        }
        self.remove_raw(super::KEYS, directory)?;
        Ok(())
    }

    /// `directory`'s JWKS: every key published at `now`.
    pub fn jwks(&self, directory: &str, now: u64) -> Result<Value, ConnectorError> {
        let keys: Vec<Value> = self
            .signing_keys(directory)?
            .iter()
            .filter(|k| k.published(now))
            .map(SigningKey::jwk)
            .collect();
        Ok(json!({ "keys": keys }))
    }

    /// `claims` as a compact JWS signed with `directory`'s active key
    /// (`ConnectorError::Invalid` when it has none yet).
    pub fn sign_jwt(
        &self,
        directory: &str,
        typ: &str,
        claims: &Value,
    ) -> Result<String, ConnectorError> {
        let key = self.active_signing_key(directory)?.ok_or_else(|| {
            ConnectorError::Invalid(format!("directory {directory} has no signing key yet"))
        })?;
        let dk = self.data_key(directory)?;
        let scalar = secrets::open(&dk, directory, SEAL_AS, &key.n.to_string(), &key.private)?;
        let bytes = zeroize::Zeroizing::new(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(scalar.as_bytes())
                .map_err(|_| secrets::SecretError::Decrypt)?,
        );
        let signer = p256::ecdsa::SigningKey::from_slice(&bytes)
            .map_err(|_| secrets::SecretError::Decrypt)?;
        let header = json!({ "alg": ALG, "typ": typ, "kid": key.kid });
        let input = format!(
            "{}.{}",
            b64u(header.to_string().as_bytes()),
            b64u(claims.to_string().as_bytes())
        );
        let sig: p256::ecdsa::Signature = signer.sign(input.as_bytes());
        Ok(format!("{input}.{}", b64u(&sig.to_bytes())))
    }
}

/// How often the keys are looked at.
const MAINTAIN_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

/// The task that gives each hosted directory a key and rotates it when due:
/// on a standalone host, or the cluster's leader (where writes go).
pub fn spawn(auth: std::sync::Arc<crate::auth::AuthService>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(MAINTAIN_EVERY);
        loop {
            tick.tick().await;
            if !auth.tenants.can_write() || auth.is_disabled() {
                continue;
            }
            let Ok(dirs) = auth.tenants.directories() else {
                continue;
            };
            let now = crate::auth::store::now();
            // The cluster's own key too (D63).
            if let Err(e) = auth.connectors.maintain_signing_keys(CLUSTER_OWNER, now) {
                tracing::warn!("the cluster key: {}", e);
            }
            for d in dirs {
                match auth.connectors.maintain_signing_keys(&d.id, now) {
                    Ok(true) => tracing::info!(directory = %d.id, "signing keys updated"),
                    Ok(false) => {}
                    Err(e) => tracing::warn!(directory = %d.id, "signing keys: {}", e),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn store() -> (ConnectorStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = std::sync::Arc::new(crate::store::Db::create(dir.path().join("t.db")).unwrap());
        (ConnectorStore::new(db, &Config::default()).unwrap(), dir)
    }

    #[test]
    fn a_token_verifies_against_the_published_jwks_with_another_library() {
        let (s, _d) = store();
        assert!(s.maintain_signing_keys("local", 1_000).unwrap());
        assert!(
            !s.maintain_signing_keys("local", 1_001).unwrap(),
            "nothing due"
        );
        let claims = json!({ "iss": "https://g.example/d/local", "sub": "u1", "aud": "c1", "exp": 4_000_000_000u64 });
        let jwt = s.sign_jwt("local", "JWT", &claims).unwrap();
        let jwks: jsonwebtoken::jwk::JwkSet =
            serde_json::from_value(s.jwks("local", 1_001).unwrap()).unwrap();
        let header = jsonwebtoken::decode_header(&jwt).unwrap();
        assert_eq!(header.kid.as_deref(), Some("local:1"));
        let jwk = jwks.find(header.kid.as_deref().unwrap()).unwrap();
        let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
        v.set_audience(&["c1"]);
        v.set_issuer(&["https://g.example/d/local"]);
        let got = jsonwebtoken::decode::<Value>(
            &jwt,
            &jsonwebtoken::DecodingKey::from_jwk(jwk).unwrap(),
            &v,
        )
        .unwrap();
        assert_eq!(got.claims["sub"], "u1");
        // No private part is ever published.
        let text = s.jwks("local", 1_001).unwrap().to_string();
        assert!(!text.contains("\"d\""), "{text}");
    }

    #[test]
    fn rotation_keeps_the_old_key_published_until_its_tokens_expired() {
        let (s, _d) = store();
        s.maintain_signing_keys("local", 1_000).unwrap();
        let old = s.sign_jwt("local", "JWT", &json!({"sub": "u"})).unwrap();
        // Due after 30 days.
        assert!(
            s.maintain_signing_keys("local", 1_000 + ROTATE_SECS)
                .unwrap()
        );
        let now = 1_000 + ROTATE_SECS;
        let kids: Vec<String> = s
            .signing_keys("local")
            .unwrap()
            .iter()
            .map(|k| k.kid.clone())
            .collect();
        assert_eq!(kids, ["local:1", "local:2"]);
        let jwks = s.jwks("local", now + 60).unwrap().to_string();
        assert!(
            jwks.contains("local:1") && jwks.contains("local:2"),
            "{jwks}"
        );
        assert_eq!(
            jsonwebtoken::decode_header(&old).unwrap().kid.as_deref(),
            Some("local:1")
        );
        let new = s.sign_jwt("local", "JWT", &json!({"sub": "u"})).unwrap();
        assert_eq!(
            jsonwebtoken::decode_header(&new).unwrap().kid.as_deref(),
            Some("local:2")
        );
        // Past the retention, the old key is dropped.
        assert!(s.maintain_signing_keys("local", now + RETAIN_SECS).unwrap());
        let jwks = s.jwks("local", now + RETAIN_SECS).unwrap().to_string();
        assert!(
            !jwks.contains("local:1") && jwks.contains("local:2"),
            "{jwks}"
        );
    }

    #[test]
    fn directories_have_separate_keys() {
        let (s, _d) = store();
        s.maintain_signing_keys("local", 1).unwrap();
        s.maintain_signing_keys("acme", 1).unwrap();
        assert_eq!(s.signing_keys("local").unwrap().len(), 1);
        assert_eq!(s.active_signing_key("acme").unwrap().unwrap().kid, "acme:1");
        assert!(s.sign_jwt("none", "JWT", &json!({})).is_err());
    }
}
