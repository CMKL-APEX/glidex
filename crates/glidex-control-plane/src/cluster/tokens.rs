//! Join and rejoin tokens (§5.2, §5.7): `<token id>.<secret>.<CA hash>`, shown
//! once. Only `SHA-256(secret)` is stored. Single use, with an expiry.

use crate::node::NodeRole;
use crate::store::{Db, Origin, StoreError, TableId};
use serde::{Deserialize, Serialize};
use redb::ReadableTable;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TokenKind {
    Join { role: NodeRole, allow_import: bool },
    Rejoin { node: String, raft_intact: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinToken {
    pub sha256: String,
    #[serde(flatten)]
    pub kind: TokenKind,
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_at: Option<u64>,
    pub created_by: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("the join token is not valid")]
    Invalid,
    #[error("the join token has expired")]
    Expired,
    #[error("the join token was already used")]
    Used,
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(impl From<$t> for TokenError { fn from(e: $t) -> Self { TokenError::Storage(e.to_string()) } })*};
}
storage_from!(redb::TableError, redb::StorageError, serde_json::Error);

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system randomness");
    super::pki::hex(&b)
}

fn digest(secret: &str) -> String {
    super::pki::hex(&Sha256::digest(secret.as_bytes()))
}

fn now() -> u64 {
    crate::tenancy::now()
}

/// Make a token. Returns the string to show once.
pub fn create(db: &Db, kind: TokenKind, ttl_secs: u64, ca_fingerprint: &str, created_by: &str) -> Result<String, TokenError> {
    let (id, secret) = (random_hex(8), random_hex(32));
    let rec = JoinToken { sha256: digest(&secret), kind, expires_at: now() + ttl_secs, used_at: None, created_by: created_by.to_string() };
    db.write(Origin::Api, |tx| -> Result<(), TokenError> {
        tx.open_table(TableId::JoinTokens.definition())?.insert(&id, serde_json::to_vec(&rec)?.as_slice())?;
        Ok(())
    })?;
    Ok(format!("{id}.{secret}.{ca_fingerprint}"))
}

/// Split a token into its id, secret and the CA hash it pins.
pub fn parse(token: &str) -> Result<(&str, &str, &str), TokenError> {
    let mut p = token.trim().splitn(3, '.');
    match (p.next(), p.next(), p.next()) {
        (Some(i), Some(s), Some(f)) if !i.is_empty() && !s.is_empty() && f.len() == 64 => Ok((i, s, f)),
        _ => Err(TokenError::Invalid),
    }
}

/// Check a token and mark it used, in one write. `used_at` makes it single
/// use even if two joins race (writes are serialized).
pub fn consume(db: &Db, token: &str) -> Result<JoinToken, TokenError> {
    let (id, secret, _) = parse(token)?;
    let want = digest(secret);
    db.write(Origin::Api, |tx| -> Result<JoinToken, TokenError> {
        let mut t = tx.open_table(TableId::JoinTokens.definition())?;
        let rec: JoinToken = match t.get(id)? {
            Some(v) => serde_json::from_slice(v.value())?,
            None => return Err(TokenError::Invalid),
        };
        if !crate::auth::constant_eq(&rec.sha256, &want) {
            return Err(TokenError::Invalid);
        }
        if rec.used_at.is_some() {
            return Err(TokenError::Used);
        }
        if now() > rec.expires_at {
            return Err(TokenError::Expired);
        }
        let mut used = rec.clone();
        used.used_at = Some(now());
        t.insert(id, serde_json::to_vec(&used)?.as_slice())?;
        Ok(rec)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Db) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::create(d.path().join("t.db")).unwrap();
        (d, db)
    }

    #[test]
    fn a_token_works_once() {
        let (_d, db) = db();
        let fp = "a".repeat(64);
        let t = create(&db, TokenKind::Join { role: NodeRole::Agent, allow_import: false }, 3600, &fp, "u").unwrap();
        assert_eq!(parse(&t).unwrap().2, fp);
        let rec = consume(&db, &t).unwrap();
        assert_eq!(rec.kind, TokenKind::Join { role: NodeRole::Agent, allow_import: false });
        assert!(matches!(consume(&db, &t), Err(TokenError::Used)));
    }

    #[test]
    fn wrong_secrets_and_expired_tokens_are_refused() {
        let (_d, db) = db();
        let fp = "b".repeat(64);
        let t = create(&db, TokenKind::Join { role: NodeRole::Server, allow_import: false }, 3600, &fp, "u").unwrap();
        let (id, _, _) = parse(&t).unwrap();
        assert!(matches!(consume(&db, &format!("{id}.{}.{fp}", "0".repeat(64))), Err(TokenError::Invalid)));
        assert!(matches!(consume(&db, "garbage"), Err(TokenError::Invalid)));
        // A token the failed attempts did not use still works.
        consume(&db, &t).unwrap();
        let old = create(&db, TokenKind::Rejoin { node: "n".into(), raft_intact: true }, 0, &fp, "u").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(matches!(consume(&db, &old), Err(TokenError::Expired)));
    }

    #[test]
    fn only_the_hash_of_the_secret_is_stored() {
        let (_d, db) = db();
        let fp = "c".repeat(64);
        let t = create(&db, TokenKind::Join { role: NodeRole::Agent, allow_import: false }, 60, &fp, "u").unwrap();
        let (_, secret, _) = parse(&t).unwrap();
        let dump = serde_json::to_string(&db.dump().unwrap().iter().map(|(_, r)| r.iter().map(|(_, v)| String::from_utf8_lossy(v).into_owned()).collect::<Vec<_>>()).collect::<Vec<_>>()).unwrap();
        assert!(!dump.contains(secret));
    }
}
