//! Trust between a cluster and the directories of other organizations
//! (spec/tenancy.md §12.1, D63–D66): the offer token a cluster's operator
//! sends, and (task 5b) redeeming it and ending trust.

use base64::Engine as _;
use redb::{ReadableTable as _, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

const TRUSTED_DIRECTORIES: TableDefinition<&str, &[u8]> =
    TableDefinition::new("trusted_directories");
const TRUSTED_CLUSTERS: TableDefinition<&str, &[u8]> = TableDefinition::new("trusted_clusters");

/// What a peer request asks for (an assertion's `purpose`, D63).
pub const REDEEM: &str = "redeem";
pub const END: &str = "end";

/// An offer token's prefix.
pub const OFFER_PREFIX: &str = "gxo_";
/// An offer's default and longest life, in days.
pub const OFFER_DAYS: u64 = 7;
pub const OFFER_MAX_DAYS: u64 = 30;

/// What an offer token carries (D64): enough for the directory to reach
/// the cluster, pin it, and redeem the offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferToken {
    pub cluster_id: String,
    pub cluster_name: String,
    /// The cluster's API, `https://host[:port]`.
    pub api: String,
    /// Its certificate's SHA-256, 64 hex digits.
    pub tls_sha256: String,
    /// The tenant's slug and its directory's issuer.
    pub tenant: String,
    pub directory: String,
    pub offer: String,
    pub secret: String,
    pub expires_at: u64,
}

impl OfferToken {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("serializable");
        format!(
            "{OFFER_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        )
    }

    pub fn decode(s: &str) -> Option<OfferToken> {
        let b = s.trim().strip_prefix(OFFER_PREFIX)?;
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(b)
            .ok()?;
        serde_json::from_slice(&json).ok()
    }
}

/// The issuer URL as offers and trust records compare it: lower-case
/// scheme and host, no trailing `/`.
pub fn normalize_issuer(s: &str) -> Option<String> {
    let u: axum::http::Uri = s.trim().parse().ok()?;
    let a = u.authority()?;
    (u.scheme_str() == Some("https") && u.query().is_none()).then(|| {
        format!(
            "https://{}{}",
            a.as_str().to_ascii_lowercase(),
            u.path().trim_end_matches('/')
        )
    })
}

/// A tenant of another directory, as a cluster that admits it knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTenant {
    pub id: String,
    pub slug: String,
    #[serde(default)]
    pub display_name: String,
}

/// A directory of another organization this cluster trusts (D65):
/// cluster scope, keyed by its issuer's id (`id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDirectory {
    /// `trusted-<first 16 hex of the issuer's SHA-256>`: the admissions'
    /// `directory_id`, never one of this host's own directory ids.
    pub id: String,
    pub issuer: String,
    /// Its id at its own host.
    pub directory: String,
    /// Its API for this cluster's calls (`/d/<directory>/trust/v1/…`),
    /// and that API's certificate SHA-256.
    pub api: String,
    pub tls_sha256: String,
    /// Its signing keys as the handshake brought them (a JWKS).
    pub jwks: Value,
    pub tenants: Vec<RemoteTenant>,
    pub created_at: u64,
}

/// The id a trusted directory's records take here.
pub fn trusted_id(issuer: &str) -> String {
    let h = crate::auth::sha256_hex(issuer.as_bytes());
    format!("trusted-{}", &h[..16])
}

/// A cluster of another organization a directory's tenants use (D65):
/// directory scope, keyed `<directory>/<cluster_id>`. Also the directory's
/// client registration of the cluster (D54).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedCluster {
    pub directory_id: String,
    pub cluster_id: String,
    pub name: String,
    pub api: String,
    pub tls_sha256: String,
    /// The cluster's key (a JWKS): its assertions and client assertions.
    pub jwks: Value,
    /// The tenants of this directory opted in to it.
    pub tenants: Vec<String>,
    pub created_at: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("trust records: {0}")]
pub struct StoreError(String);

macro_rules! store_err {
    ($($t:ty),*) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self { StoreError(e.to_string()) }
        }
    )*};
}
store_err!(
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    crate::store::StoreError,
    serde_json::Error
);

/// Both sides' trust records.
pub struct TrustStore {
    db: Arc<crate::store::Db>,
}

impl TrustStore {
    pub fn new(db: Arc<crate::store::Db>) -> Self {
        TrustStore { db }
    }

    fn all<T: serde::de::DeserializeOwned>(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
    ) -> Result<Vec<T>, StoreError> {
        let txn = self.db.begin_read()?;
        let t = match txn.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for e in t.iter()? {
            out.push(serde_json::from_slice(e?.1.value())?);
        }
        Ok(out)
    }

    fn put<T: Serialize>(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
        key: &str,
        v: Option<&T>,
    ) -> Result<(), StoreError> {
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut t = txn.open_table(def)?;
            match v {
                Some(v) => {
                    t.insert(key, serde_json::to_vec(v)?.as_slice())?;
                }
                None => {
                    t.remove(key)?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn directories(&self) -> Result<Vec<TrustedDirectory>, StoreError> {
        self.all(TRUSTED_DIRECTORIES)
    }

    pub fn directory_by_issuer(
        &self,
        issuer: &str,
    ) -> Result<Option<TrustedDirectory>, StoreError> {
        Ok(self.directories()?.into_iter().find(|d| d.issuer == issuer))
    }

    pub fn put_directory(&self, d: &TrustedDirectory) -> Result<(), StoreError> {
        self.put(TRUSTED_DIRECTORIES, &d.id, Some(d))
    }

    pub fn remove_directory(&self, id: &str) -> Result<(), StoreError> {
        self.put::<TrustedDirectory>(TRUSTED_DIRECTORIES, id, None)
    }

    pub fn clusters(&self, directory: &str) -> Result<Vec<TrustedCluster>, StoreError> {
        Ok(self
            .all::<TrustedCluster>(TRUSTED_CLUSTERS)?
            .into_iter()
            .filter(|c| c.directory_id == directory)
            .collect())
    }

    pub fn cluster(&self, directory: &str, id: &str) -> Result<Option<TrustedCluster>, StoreError> {
        Ok(self
            .clusters(directory)?
            .into_iter()
            .find(|c| c.cluster_id == id))
    }

    pub fn put_cluster(&self, c: &TrustedCluster) -> Result<(), StoreError> {
        self.put(
            TRUSTED_CLUSTERS,
            &format!("{}/{}", c.directory_id, c.cluster_id),
            Some(c),
        )
    }

    pub fn remove_cluster(&self, directory: &str, id: &str) -> Result<(), StoreError> {
        self.put::<TrustedCluster>(TRUSTED_CLUSTERS, &format!("{directory}/{id}"), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_tokens_round_trip_and_refuse_junk() {
        let t = OfferToken {
            cluster_id: "c1".into(),
            cluster_name: "north".into(),
            api: "https://north.example:8841".into(),
            tls_sha256: "ab".repeat(32),
            tenant: "acme".into(),
            directory: "https://id.acme.example/d/acme".into(),
            offer: "o1".into(),
            secret: "s".into(),
            expires_at: 5,
        };
        let s = t.encode();
        assert!(s.starts_with(OFFER_PREFIX));
        assert_eq!(OfferToken::decode(&s), Some(t));
        assert_eq!(OfferToken::decode("gxo_!!"), None);
        assert_eq!(OfferToken::decode("gxt_abc"), None);
        assert_eq!(
            normalize_issuer("https://ID.Acme.example/d/acme/").as_deref(),
            Some("https://id.acme.example/d/acme")
        );
        assert_eq!(normalize_issuer("http://id.acme.example/d/acme"), None);
    }
}
