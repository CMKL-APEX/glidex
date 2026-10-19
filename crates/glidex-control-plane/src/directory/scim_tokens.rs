//! SCIM bearer tokens (spec/tenancy.md §8.1, D21): `gxs_<base62>`, minted
//! for one connector and shown once. Only the token's SHA-256 is stored,
//! as the row's key, so a request is checked with one lookup. A token
//! authorizes that connector's SCIM endpoints and nothing else: it is not
//! a Cedar principal.

use crate::store::Db;
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const SCIM_TOKENS: TableDefinition<&str, &[u8]> = TableDefinition::new("scim_tokens");

pub const PREFIX: &str = "gxs_";
/// Default and longest lifetime.
pub const DEFAULT_DAYS: u64 = 180;
pub const MAX_DAYS: u64 = 365;
/// Tokens per connector, enough to rotate.
const MAX_PER_CONNECTOR: usize = 5;
/// How long an expired token stays listed.
const KEEP_SECS: u64 = 30 * 86_400;

#[derive(Debug, thiserror::Error)]
pub enum ScimTokenError {
    #[error("no such SCIM token")]
    NotFound,
    #[error("{0}")]
    Invalid(String),
    #[error("SCIM token storage error: {0}")]
    Storage(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for ScimTokenError {
            fn from(e: $t) -> Self {
                ScimTokenError::Storage(e.to_string())
            }
        }
    )*};
}
storage_from!(
    crate::store::StoreError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    serde_json::Error
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimToken {
    /// Names it in lists and to revoke it; not secret.
    pub id: String,
    pub connector_id: String,
    pub name: String,
    pub created_by: String,
    pub created_at: u64,
    pub expires_at: u64,
}

impl ScimToken {
    pub fn is_live(&self, now: u64) -> bool {
        now < self.expires_at
    }
}

fn hash(token: &str) -> String {
    crate::cluster::pki::hex(&Sha256::digest(token.as_bytes()))
}

fn now() -> u64 {
    crate::tenancy::now()
}

/// The directory's SCIM tokens.
pub struct ScimTokenStore {
    db: Arc<Db>,
}

impl ScimTokenStore {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    fn all(&self) -> Result<Vec<(String, ScimToken)>, ScimTokenError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(SCIM_TOKENS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            out.push((k.value().to_string(), serde_json::from_slice(v.value())?));
        }
        Ok(out)
    }

    /// The connector's tokens still listed, newest first.
    pub fn list(&self, connector: &str) -> Result<Vec<ScimToken>, ScimTokenError> {
        let t = now();
        let mut out: Vec<ScimToken> = self
            .all()?
            .into_iter()
            .map(|(_, x)| x)
            .filter(|x| x.connector_id == connector && x.expires_at + KEEP_SECS > t)
            .collect();
        out.sort_by_key(|x| std::cmp::Reverse(x.created_at));
        Ok(out)
    }

    /// A new token for `connector` and its secret (shown once). Prunes the
    /// connector's long-expired ones.
    pub fn create(
        &self,
        connector: &str,
        name: &str,
        by: &str,
        days: Option<u64>,
    ) -> Result<(ScimToken, String), ScimTokenError> {
        let name = name.trim();
        if name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err(ScimTokenError::Invalid(
                "a SCIM token's name is up to 64 printable characters".into(),
            ));
        }
        let days = days.unwrap_or(DEFAULT_DAYS);
        if !(1..=MAX_DAYS).contains(&days) {
            return Err(ScimTokenError::Invalid(format!(
                "a SCIM token lasts 1-{MAX_DAYS} days"
            )));
        }
        let at = now();
        let secret = format!("{PREFIX}{}", crate::auth::random_token(32));
        let tok = ScimToken {
            id: uuid::Uuid::new_v4().simple().to_string(),
            connector_id: connector.into(),
            name: if name.is_empty() {
                "scim".into()
            } else {
                name.into()
            },
            created_by: by.into(),
            created_at: at,
            expires_at: at + days * 86_400,
        };
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut table = txn.open_table(SCIM_TOKENS)?;
            let rows: Vec<(String, ScimToken)> = table
                .iter()?
                .filter_map(|e| e.ok())
                .filter_map(|(k, v)| {
                    Some((
                        k.value().to_string(),
                        serde_json::from_slice(v.value()).ok()?,
                    ))
                })
                .filter(|(_, x): &(String, ScimToken)| x.connector_id == connector)
                .collect();
            for (k, x) in &rows {
                if x.expires_at + KEEP_SECS <= at {
                    table.remove(k.as_str())?;
                }
            }
            if rows.iter().filter(|(_, x)| x.is_live(at)).count() >= MAX_PER_CONNECTOR {
                return Err(ScimTokenError::Invalid(format!(
                    "a connector has at most {MAX_PER_CONNECTOR} live SCIM tokens: revoke one first"
                )));
            }
            table.insert(hash(&secret).as_str(), serde_json::to_vec(&tok)?.as_slice())?;
        }
        txn.commit()?;
        Ok((tok, secret))
    }

    /// Revoking deletes it.
    pub fn revoke(&self, connector: &str, id: &str) -> Result<ScimToken, ScimTokenError> {
        let found = self
            .all()?
            .into_iter()
            .find(|(_, x)| x.id == id && x.connector_id == connector)
            .ok_or(ScimTokenError::NotFound)?;
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut table = txn.open_table(SCIM_TOKENS)?;
            table.remove(found.0.as_str())?;
        }
        txn.commit()?;
        Ok(found.1)
    }

    /// Every token of `connector` goes (the connector is deleted).
    pub fn remove_for(&self, connector: &str) -> Result<usize, ScimTokenError> {
        let gone: Vec<String> = self
            .all()?
            .into_iter()
            .filter(|(_, x)| x.connector_id == connector)
            .map(|(k, _)| k)
            .collect();
        if gone.is_empty() {
            return Ok(0);
        }
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut table = txn.open_table(SCIM_TOKENS)?;
            for k in &gone {
                table.remove(k.as_str())?;
            }
        }
        txn.commit()?;
        Ok(gone.len())
    }

    /// The live token `presented` is, if it is `connector`'s. Anything
    /// else, another connector's token included, is `None`.
    pub fn verify(
        &self,
        connector: &str,
        presented: &str,
    ) -> Result<Option<ScimToken>, ScimTokenError> {
        if !presented.starts_with(PREFIX) || presented.len() > 128 {
            return Ok(None);
        }
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(SCIM_TOKENS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let Some(v) = table.get(hash(presented).as_str())? else {
            return Ok(None);
        };
        let tok: ScimToken = serde_json::from_slice(v.value())?;
        Ok((tok.connector_id == connector && tok.is_live(now())).then_some(tok))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (ScimTokenStore, tempfile::TempDir) {
        let d = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Db::create(d.path().join("t.db")).unwrap());
        (ScimTokenStore::new(db), d)
    }

    #[test]
    fn a_token_is_its_connectors_only_until_revoked() {
        let (s, _d) = store();
        let (t, secret) = s.create("jc", "jumpcloud", "ada", None).unwrap();
        assert!(secret.starts_with(PREFIX));
        assert_eq!(t.expires_at - t.created_at, DEFAULT_DAYS * 86_400);
        assert_eq!(s.verify("jc", &secret).unwrap().unwrap().id, t.id);
        assert!(s.verify("other", &secret).unwrap().is_none());
        assert!(s.verify("jc", "gxs_wrong").unwrap().is_none());
        assert!(s.verify("jc", &secret[PREFIX.len()..]).unwrap().is_none());
        // Never stored as given.
        let raw = s.all().unwrap();
        assert!(
            raw.iter()
                .all(|(k, _)| k != &secret && !k.contains(&secret))
        );
        assert_eq!(s.list("jc").unwrap().len(), 1);
        assert!(matches!(
            s.revoke("other", &t.id),
            Err(ScimTokenError::NotFound)
        ));
        s.revoke("jc", &t.id).unwrap();
        assert!(s.verify("jc", &secret).unwrap().is_none());
    }

    #[test]
    fn lifetimes_and_counts_are_bounded() {
        let (s, _d) = store();
        assert!(s.create("jc", "", "ada", Some(0)).is_err());
        assert!(s.create("jc", "", "ada", Some(MAX_DAYS + 1)).is_err());
        for _ in 0..MAX_PER_CONNECTOR {
            s.create("jc", "", "ada", Some(1)).unwrap();
        }
        assert!(s.create("jc", "", "ada", None).is_err());
        s.create("other", "", "ada", None).unwrap();
        assert_eq!(s.remove_for("jc").unwrap(), MAX_PER_CONNECTOR);
        assert!(s.list("jc").unwrap().is_empty());
    }
}
