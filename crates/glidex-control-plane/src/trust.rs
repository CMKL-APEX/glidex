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
pub const FEED: &str = "feed";

const FEED_STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("feed_state");

/// The tables a feed carries rows of (D67), by name.
pub const FEED_TABLES: [&str; 6] = [
    "users",
    "teams",
    "tenants",
    "tenant_members",
    "projects",
    "policy_links",
];

fn table(name: &str) -> Option<TableDefinition<'static, &'static str, &'static [u8]>> {
    FEED_TABLES
        .iter()
        .find(|t| **t == name)
        .map(|t| TableDefinition::new(t))
}

/// One record of a feed: the table, its key there, its value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub table: String,
    pub key: String,
    pub value: Value,
}

/// A directory's filtered snapshot for one cluster (D67).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: String,
    pub jwks: Value,
    pub rows: Vec<Row>,
    /// The chains of this cluster's browser logins ended in the last
    /// 12 h: the cluster ends the sessions it made from them (D68, D69).
    #[serde(default)]
    pub ended_sids: Vec<String>,
}

/// What the cluster last applied from a trusted directory (D68).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedState {
    pub revision: String,
    /// `(table, key)` of every row it wrote.
    pub rows: Vec<(String, String)>,
    pub applied_at: u64,
}

/// A browser login at a trusted directory under way (D69): what its
/// callback checks.
#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub trusted: String,
    pub nonce: String,
    pub verifier: String,
    pub return_to: String,
    /// The secret of the browser's cookie that started it.
    pub browser: String,
    pub created: u64,
}

/// How long a browser login may take.
pub const LOGIN_SECS: u64 = 600;

/// Logins under way, by `state`.
#[derive(Default)]
pub struct PendingLogins(std::sync::Mutex<std::collections::HashMap<String, PendingLogin>>);

impl PendingLogins {
    pub fn put(&self, state: String, p: PendingLogin) {
        let mut m = self.0.lock().unwrap();
        m.retain(|_, v| v.created + LOGIN_SECS > p.created);
        m.insert(state, p);
    }

    /// The login `state` names, once, while it is good.
    pub fn take(&self, state: &str, now: u64) -> Option<PendingLogin> {
        self.0
            .lock()
            .unwrap()
            .remove(state)
            .filter(|p| p.created + LOGIN_SECS > now)
    }
}

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
    /// Its web UI's callback for logins at this directory (D69): the only
    /// `redirect_uri` its client `cluster:<id>` may use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_callback: Option<String>,
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

/// A snapshot's revision: the SHA-256 of what it holds.
pub fn revision_of(jwks: &Value, rows: &[Row], ended_sids: &[String]) -> String {
    crate::auth::sha256_hex(
        serde_json::to_string(&(jwks, rows, ended_sids))
            .unwrap_or_default()
            .as_bytes(),
    )
}

impl TrustStore {
    /// Every row of feed table `name`, as `(key, value)`.
    pub fn raw_rows(&self, name: &str) -> Result<Vec<(String, Value)>, StoreError> {
        let def = table(name).ok_or_else(|| StoreError(format!("not a feed table: {name}")))?;
        let txn = self.db.begin_read()?;
        let t = match txn.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for e in t.iter()? {
            let (k, v) = e?;
            out.push((k.value().to_string(), serde_json::from_slice(v.value())?));
        }
        Ok(out)
    }

    pub fn feed_state(&self, trusted: &str) -> Result<FeedState, StoreError> {
        let txn = self.db.begin_read()?;
        let t = match txn.open_table(FEED_STATE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(FeedState::default()),
            Err(e) => return Err(e.into()),
        };
        Ok(match t.get(trusted)? {
            Some(v) => serde_json::from_slice(v.value())?,
            None => FeedState::default(),
        })
    }

    /// Replace trusted directory `d`'s replica with `rows` in one write
    /// (D68): the rows the last feed wrote go, these come. A row that would
    /// overwrite a record that isn't this directory's replica is refused,
    /// and so is the whole snapshot.
    pub fn apply_feed(
        &self,
        d: &TrustedDirectory,
        snap: &Snapshot,
        now: u64,
    ) -> Result<usize, StoreError> {
        let old = self.feed_state(&d.id)?;
        let mine: std::collections::HashSet<(String, String)> = old.rows.iter().cloned().collect();
        let txn = self.db.begin(crate::store::Origin::Api)?;
        let mut written = Vec::new();
        {
            for (tname, key) in &old.rows {
                if let Some(def) = table(tname) {
                    txn.open_table(def)?.remove(key.as_str())?;
                }
            }
            for r in &snap.rows {
                let def = table(&r.table)
                    .ok_or_else(|| StoreError(format!("a feed row of table {}", r.table)))?;
                let mut t = txn.open_table(def)?;
                let ours = mine.contains(&(r.table.clone(), r.key.clone()));
                if !ours && t.get(r.key.as_str())?.is_some() {
                    return Err(StoreError(format!(
                        "the feed would overwrite {} {} of this host",
                        r.table, r.key
                    )));
                }
                t.insert(r.key.as_str(), serde_json::to_vec(&r.value)?.as_slice())?;
                written.push((r.table.clone(), r.key.clone()));
            }
            let st = FeedState {
                revision: snap.revision.clone(),
                rows: written.clone(),
                applied_at: now,
            };
            txn.open_table(FEED_STATE)?
                .insert(d.id.as_str(), serde_json::to_vec(&st)?.as_slice())?;
        }
        txn.commit()?;
        Ok(written.len())
    }

    /// Forget trusted directory `id`'s replica (trust ended, D66).
    pub fn drop_replica(&self, id: &str) -> Result<(), StoreError> {
        let old = self.feed_state(id)?;
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            for (tname, key) in &old.rows {
                if let Some(def) = table(tname) {
                    txn.open_table(def)?.remove(key.as_str())?;
                }
            }
            txn.open_table(FEED_STATE)?.remove(id)?;
        }
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_feed_never_overwrites_this_hosts_records() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::store::Db::create(dir.path().join("t.db")).unwrap());
        let s = TrustStore::new(db.clone());
        let auth = crate::auth::store::IdentityStore::new(db).unwrap();
        let local = auth
            .user_for_identity("pam", "root", "root", None, true)
            .unwrap()
            .unwrap();
        let d = TrustedDirectory {
            id: "trusted-x".into(),
            issuer: "https://acme.example/d/local".into(),
            directory: "local".into(),
            api: String::new(),
            tls_sha256: String::new(),
            jwks: Value::Null,
            tenants: vec![],
            created_at: 0,
        };
        let row = |key: &str| Row {
            table: "users".into(),
            key: key.into(),
            value: serde_json::json!({ "id": key, "directory_id": "trusted-x", "display_name": "x", "created_at": 0 }),
        };
        let snap = |rows: Vec<Row>| Snapshot {
            revision: "r".into(),
            jwks: Value::Null,
            rows,
            ended_sids: Vec::new(),
        };
        // The host's own user can't be replaced by a directory's row.
        assert!(s.apply_feed(&d, &snap(vec![row(&local.id)]), 1).is_err());
        assert_eq!(auth.user(&local.id).unwrap().unwrap().directory_id, "local");
        // Its own rows are replaced, and dropped with the replica.
        assert_eq!(s.apply_feed(&d, &snap(vec![row("u1")]), 1).unwrap(), 1);
        assert_eq!(
            s.apply_feed(&d, &snap(vec![row("u1"), row("u2")]), 2)
                .unwrap(),
            2
        );
        assert!(auth.user("u2").unwrap().is_some());
        s.drop_replica("trusted-x").unwrap();
        assert!(auth.user("u1").unwrap().is_none() && auth.user("u2").unwrap().is_none());
    }

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
