//! Requests between a directory and a cluster of different organizations
//! (spec/19-tenancy.md D63): signed peer assertions and pinned
//! certificates. The protocol is `glidex-trust`'s (D84); this signs with
//! the keys this host keeps, a directory's or the cluster's.

use super::{ConnectorError, ConnectorStore};
use serde_json::Value;

pub use glidex_trust::peer::{
    ASSERTION_SECS, CLIENT_ASSERTION_TYPE, PeerError, Replays, SCHEME, TYP, fingerprint_of,
    from_headers, post, post_unsigned, verify, verify_client_assertion,
};

/// `owner`'s active key (a directory's, or `issuer::CLUSTER_OWNER`'s) as
/// a signer for `glidex-trust`.
pub struct OwnerKey<'a> {
    pub keys: &'a ConnectorStore,
    pub owner: &'a str,
}

impl glidex_trust::Signer for OwnerKey<'_> {
    fn sign_jwt(&self, typ: &str, claims: &Value) -> Result<String, String> {
        self.keys
            .sign_jwt(self.owner, typ, claims)
            .map_err(|e| e.to_string())
    }
}

/// The cluster key, as a signer.
pub fn cluster_key(keys: &ConnectorStore) -> OwnerKey<'_> {
    OwnerKey {
        keys,
        owner: super::issuer::CLUSTER_OWNER,
    }
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
        &glidex_trust::peer::assertion_claims(iss, aud, purpose, now),
    )
}

/// A trusted cluster's client assertion for directory token endpoint
/// `aud` (`private_key_jwt`, RFC 7523, D69): `iss` and `sub` its client
/// id, signed with the cluster key.
pub fn client_assertion(
    keys: &ConnectorStore,
    client_id: &str,
    aud: &str,
    now: u64,
) -> Result<String, ConnectorError> {
    keys.sign_jwt(
        super::issuer::CLUSTER_OWNER,
        "JWT",
        &glidex_trust::peer::client_assertion_claims(client_id, aud, now),
    )
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
