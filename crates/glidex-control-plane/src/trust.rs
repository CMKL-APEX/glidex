//! Trust between a cluster and the directories of other organizations
//! (spec/19-tenancy.md §12.1, D63–D66): the records both sides keep. What
//! crosses between them, and the cluster's checks, are `glidex-trust`'s
//! (D84), re-exported here.

use redb::{ReadableTable as _, TableDefinition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub use glidex_trust::{
    END, EndedChains, FEED, FEED_TABLES, FeedState, LOGIN_SECS, LOGOUT, OFFER_DAYS, OFFER_MAX_DAYS,
    OFFER_PREFIX, OfferToken, PendingLogin, PendingLogins, REDEEM, REPORT, RemoteTenant, Row,
    Snapshot, TrustedDirectory, normalize_issuer, revision_of, trusted_id,
};

const TRUSTED_DIRECTORIES: TableDefinition<&str, &[u8]> =
    TableDefinition::new("trusted_directories");
const TRUSTED_CLUSTERS: TableDefinition<&str, &[u8]> = TableDefinition::new("trusted_clusters");

const FEED_STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("feed_state");

fn table(name: &str) -> Option<TableDefinition<'static, &'static str, &'static [u8]>> {
    FEED_TABLES
        .iter()
        .find(|t| **t == name)
        .map(|t| TableDefinition::new(t))
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
    /// What it last reported of the opted-in tenants (D70).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<ClusterReport>,
}

/// A cluster's report to a trusted directory (D70): per tenant, its
/// admission and what it holds there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterReport {
    pub at: u64,
    pub tenants: Vec<TenantReport>,
}

/// One tenant's entry in a report (D70). The VM counts default to zero, so
/// a storage cluster sends only `storage` (D85).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantReport {
    /// The tenant's id (the same at both).
    pub tenant: String,
    /// Admitted, or suspended (D9).
    #[serde(default)]
    pub suspended: bool,
    #[serde(default)]
    pub cap: crate::tenancy::Quotas,
    #[serde(default)]
    pub projects: u64,
    #[serde(default)]
    pub vms: u64,
    #[serde(default)]
    pub running_vms: u64,
    #[serde(default)]
    pub disks: u64,
    #[serde(default)]
    pub disk_gib: u64,
    #[serde(default)]
    pub networks: u64,
    /// What a storage cluster holds of the tenant, as totals (D85).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<glidex_trust::StorageReport>,
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
                checked_at: now,
            };
            txn.open_table(FEED_STATE)?
                .insert(d.id.as_str(), serde_json::to_vec(&st)?.as_slice())?;
        }
        txn.commit()?;
        Ok(written.len())
    }

    /// The directory answered a feed request at `now` with nothing new;
    /// written at most once a minute.
    pub fn mark_checked(&self, id: &str, now: u64) -> Result<(), StoreError> {
        let mut st = self.feed_state(id)?;
        if now < st.checked_at + 60 {
            return Ok(());
        }
        st.checked_at = now;
        let txn = self.db.begin(crate::store::Origin::Api)?;
        txn.open_table(FEED_STATE)?
            .insert(id, serde_json::to_vec(&st)?.as_slice())?;
        txn.commit()?;
        Ok(())
    }

    /// Move `id`'s feed times `secs` into the past, as if the directory
    /// had been unreachable that long (tests and drills).
    #[doc(hidden)]
    pub fn backdate_feed(&self, id: &str, secs: u64) -> Result<(), StoreError> {
        let mut st = self.feed_state(id)?;
        st.applied_at = st.applied_at.saturating_sub(secs);
        st.checked_at = st.checked_at.saturating_sub(secs);
        let txn = self.db.begin(crate::store::Origin::Api)?;
        txn.open_table(FEED_STATE)?
            .insert(id, serde_json::to_vec(&st)?.as_slice())?;
        txn.commit()?;
        if let Some(mut d) = self.directories()?.into_iter().find(|d| d.id == id) {
            d.created_at = d.created_at.saturating_sub(secs);
            self.put_directory(&d)?;
        }
        Ok(())
    }

    /// How far behind `d`'s replica is at `now`, in seconds: since the
    /// directory last answered a feed (since trust began, before any).
    pub fn lag(&self, d: &TrustedDirectory, now: u64) -> Result<u64, StoreError> {
        Ok(glidex_trust::cluster::lag(d, &self.feed_state(&d.id)?, now))
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
    fn reports_read_with_or_without_storage() {
        // A storage cluster's: only storage (D85).
        let r: ClusterReport = serde_json::from_value(serde_json::json!({
            "at": 5,
            "tenants": [{ "tenant": "t1", "storage": {
                "volumes": 2, "volume_gib": 100, "volume_used_bytes": 7, "buckets": 1,
                "bucket_bytes": 9, "s3_keys": 3,
            } }],
        }))
        .unwrap();
        let t = &r.tenants[0];
        assert_eq!((t.vms, t.disks, t.suspended), (0, 0, false));
        assert_eq!(t.cap, crate::tenancy::Quotas::default());
        let s = t.storage.as_ref().unwrap();
        assert_eq!((s.volume_gib, s.volume_used_bytes, s.s3_keys), (100, 7, 3));
        // A glidex cluster's, as clusters sent them before storage.
        let old: ClusterReport = serde_json::from_value(serde_json::json!({
            "at": 5,
            "tenants": [{ "tenant": "t1", "suspended": true, "cap": {}, "projects": 1, "vms": 2,
                          "running_vms": 1, "disks": 3, "disk_gib": 40, "networks": 1 }],
        }))
        .unwrap();
        assert_eq!(old.tenants[0].vms, 2);
        assert!(old.tenants[0].storage.is_none());
        // And it is written as before: no `storage` key.
        let v = serde_json::to_value(&old).unwrap();
        assert!(v["tenants"][0].get("storage").is_none());
    }

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
            max_staleness_minutes: None,
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
}
