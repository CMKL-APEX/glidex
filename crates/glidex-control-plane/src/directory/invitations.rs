//! Invitations (spec/tenancy.md D40, §6.1): an email address invited to a
//! tenant, with a tenant role, through one connector. The invitation is a
//! link with a secret; only the secret's SHA-256 is stored. The first
//! login through that connector that brings the secret, with a verified
//! email equal to the invited one, takes it, once, before it expires.
//! That works with `jit: false`: an invitation is how a gated connector
//! (D39) lets a new person in.

use crate::store::Db;
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const INVITATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("invitations");

/// Default and longest lifetime.
pub const DEFAULT_DAYS: u64 = 7;
pub const MAX_DAYS: u64 = 30;
/// How long taken and expired invitations stay listed.
const KEEP_SECS: u64 = 30 * 86_400;

#[derive(Debug, thiserror::Error)]
pub enum InvitationError {
    #[error("no such invitation")]
    NotFound,
    #[error("{0}")]
    Invalid(String),
    #[error("invitation storage error: {0}")]
    Storage(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for InvitationError {
            fn from(e: $t) -> Self {
                InvitationError::Storage(e.to_string())
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
pub struct Invitation {
    pub id: String,
    pub tenant_id: String,
    /// Lower case.
    pub email: String,
    /// A tenant role (`role.tenant-admin`, …).
    pub role: String,
    pub connector_id: String,
    pub invited_by: String,
    pub created_at: u64,
    pub expires_at: u64,
    /// SHA-256 of the secret, hex.
    pub secret_hash: String,
    #[serde(default)]
    pub taken_by: Option<String>,
    #[serde(default)]
    pub taken_at: Option<u64>,
}

impl Invitation {
    pub fn is_open(&self, now: u64) -> bool {
        self.taken_at.is_none() && now < self.expires_at
    }
}

/// What a login brings: `<id>.<secret>`, as the invitation's link carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteToken {
    pub id: String,
    secret: String,
}

impl InviteToken {
    pub fn parse(s: &str) -> Option<InviteToken> {
        let (id, secret) = s.trim().split_once('.')?;
        let ok = |p: &str| {
            !p.is_empty()
                && p.len() <= 128
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        };
        (ok(id) && ok(secret)).then(|| InviteToken {
            id: id.into(),
            secret: secret.into(),
        })
    }

    fn matches(&self, inv: &Invitation) -> bool {
        inv.id == self.id && crate::auth::constant_eq(&hash(&self.secret), &inv.secret_hash)
    }
}

fn hash(secret: &str) -> String {
    crate::cluster::pki::hex(&Sha256::digest(secret.as_bytes()))
}

fn now() -> u64 {
    crate::tenancy::now()
}

/// The directory's invitations.
pub struct InvitationStore {
    db: Arc<Db>,
}

impl InvitationStore {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    fn all(&self) -> Result<Vec<Invitation>, InvitationError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(INVITATIONS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v) = entry?;
            out.push(serde_json::from_slice(v.value())?);
        }
        Ok(out)
    }

    /// The tenant's (or every) invitation still listed, newest first.
    pub fn list(&self, tenant: Option<&str>) -> Result<Vec<Invitation>, InvitationError> {
        let t = now();
        let mut out: Vec<Invitation> = self
            .all()?
            .into_iter()
            .filter(|i| tenant.is_none_or(|x| i.tenant_id == x))
            .filter(|i| listed(i, t))
            .collect();
        out.sort_by_key(|i| std::cmp::Reverse(i.created_at));
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<Option<Invitation>, InvitationError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(INVITATIONS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(match table.get(id)? {
            Some(v) => Some(serde_json::from_slice(v.value())?),
            None => None,
        })
    }

    /// The open invitation `token` names, if its secret matches: what the
    /// invitation page shows before the login.
    pub fn open(&self, token: &InviteToken) -> Result<Option<Invitation>, InvitationError> {
        Ok(self
            .get(&token.id)?
            .filter(|i| token.matches(i) && i.is_open(now())))
    }

    /// A new invitation and its secret (shown once). Prunes ones listed
    /// long enough.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        tenant_id: &str,
        email: &str,
        role: &str,
        connector_id: &str,
        invited_by: &str,
        days: Option<u64>,
    ) -> Result<(Invitation, String), InvitationError> {
        let email = email.trim().to_ascii_lowercase();
        let ok = email
            .split_once('@')
            .is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.'))
            && !email.contains(char::is_whitespace);
        if !ok {
            return Err(InvitationError::Invalid(format!(
                "{email:?} is not an email address"
            )));
        }
        let days = days.unwrap_or(DEFAULT_DAYS);
        if !(1..=MAX_DAYS).contains(&days) {
            return Err(InvitationError::Invalid(format!(
                "an invitation lasts 1-{MAX_DAYS} days"
            )));
        }
        let secret = crate::auth::random_token(32);
        let at = now();
        let inv = Invitation {
            id: uuid::Uuid::new_v4().simple().to_string(),
            tenant_id: tenant_id.into(),
            email,
            role: role.into(),
            connector_id: connector_id.into(),
            invited_by: invited_by.into(),
            created_at: at,
            expires_at: at + days * 86_400,
            secret_hash: hash(&secret),
            taken_by: None,
            taken_at: None,
        };
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut table = txn.open_table(INVITATIONS)?;
            let stale: Vec<String> = table
                .iter()?
                .filter_map(|e| e.ok())
                .filter_map(|(_, v)| serde_json::from_slice::<Invitation>(v.value()).ok())
                .filter(|i| !listed(i, at))
                .map(|i| i.id)
                .collect();
            for id in stale {
                table.remove(id.as_str())?;
            }
            table.insert(inv.id.as_str(), serde_json::to_vec(&inv)?.as_slice())?;
        }
        txn.commit()?;
        let token = format!("{}.{}", inv.id, secret);
        Ok((inv, token))
    }

    /// Revoking deletes it.
    pub fn revoke(&self, id: &str) -> Result<Invitation, InvitationError> {
        let txn = self.db.begin(crate::store::Origin::Api)?;
        let inv = {
            let mut table = txn.open_table(INVITATIONS)?;
            let inv: Invitation = match table.get(id)? {
                Some(v) => serde_json::from_slice(v.value())?,
                None => return Err(InvitationError::NotFound),
            };
            table.remove(id)?;
            inv
        };
        txn.commit()?;
        Ok(inv)
    }

    /// Take the invitation `token` names for a login through `connector`
    /// with `email` (verified), as user `user`: once, while it's open.
    /// Every refusal is the same error, so a login reveals nothing about
    /// other people's invitations.
    pub fn take(
        &self,
        token: &InviteToken,
        connector: &str,
        email: Option<&str>,
        email_verified: bool,
        user: &str,
    ) -> Result<Invitation, InvitationError> {
        let refused = || InvitationError::Invalid("the invitation can't be taken".into());
        let t = now();
        let txn = self.db.begin(crate::store::Origin::Api)?;
        let inv = {
            let mut table = txn.open_table(INVITATIONS)?;
            let mut inv: Invitation = match table.get(token.id.as_str())? {
                Some(v) => serde_json::from_slice(v.value())?,
                None => return Err(refused()),
            };
            let same_email = email.is_some_and(|e| e.trim().eq_ignore_ascii_case(&inv.email));
            if !token.matches(&inv)
                || !inv.is_open(t)
                || inv.connector_id != connector
                || !email_verified
                || !same_email
            {
                return Err(refused());
            }
            inv.taken_by = Some(user.into());
            inv.taken_at = Some(t);
            table.insert(inv.id.as_str(), serde_json::to_vec(&inv)?.as_slice())?;
            inv
        };
        txn.commit()?;
        Ok(inv)
    }

    /// Take a passkey invitation (D60): the link's secret is all the proof
    /// there is, since a new passkey vouches for no email.
    pub fn take_by_link(
        &self,
        token: &InviteToken,
        user: &str,
    ) -> Result<Invitation, InvitationError> {
        let refused = || InvitationError::Invalid("the invitation can't be taken".into());
        let t = now();
        let txn = self.db.begin(crate::store::Origin::Api)?;
        let inv = {
            let mut table = txn.open_table(INVITATIONS)?;
            let mut inv: Invitation = match table.get(token.id.as_str())? {
                Some(v) => serde_json::from_slice(v.value())?,
                None => return Err(refused()),
            };
            if !token.matches(&inv)
                || !inv.is_open(t)
                || inv.connector_id != crate::auth::passkeys::PROVIDER
            {
                return Err(refused());
            }
            inv.taken_by = Some(user.into());
            inv.taken_at = Some(t);
            table.insert(inv.id.as_str(), serde_json::to_vec(&inv)?.as_slice())?;
            inv
        };
        txn.commit()?;
        Ok(inv)
    }

    /// Whether `token` could be taken by a login through `connector` with
    /// `email`: checked before a user is created for it.
    pub fn would_take(
        &self,
        token: &InviteToken,
        connector: &str,
        email: Option<&str>,
        email_verified: bool,
    ) -> Result<Option<Invitation>, InvitationError> {
        Ok(self.open(token)?.filter(|i| {
            i.connector_id == connector
                && email_verified
                && email.is_some_and(|e| e.trim().eq_ignore_ascii_case(&i.email))
        }))
    }
}

/// Open ones, and taken or expired ones for `KEEP_SECS`.
fn listed(i: &Invitation, now: u64) -> bool {
    let ended = i.taken_at.unwrap_or(i.expires_at);
    now < ended + KEEP_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (InvitationStore, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Db::create(dir.path().join("t.db")).unwrap());
        (InvitationStore::new(db), dir)
    }

    #[test]
    fn an_invitation_is_taken_once_by_its_email_with_its_secret() {
        let (s, _d) = store();
        let (inv, token) = s
            .create(
                "t1",
                "Bob@Example.org",
                "role.tenant-member",
                "gc",
                "ada",
                None,
            )
            .unwrap();
        assert_eq!(inv.email, "bob@example.org");
        assert_ne!(inv.secret_hash, token, "only the hash is stored");
        assert!(
            !serde_json::to_string(&s.get(&inv.id).unwrap())
                .unwrap()
                .contains(token.split_once('.').unwrap().1)
        );
        let tok = InviteToken::parse(&token).unwrap();
        let wrong = InviteToken::parse(&format!("{}.nope", inv.id)).unwrap();
        // Not without the secret, not by another email, connector or an
        // unverified address.
        assert!(
            s.take(&wrong, "gc", Some("bob@example.org"), true, "u1")
                .is_err()
        );
        assert!(
            s.take(&tok, "gc", Some("eve@example.org"), true, "u1")
                .is_err()
        );
        assert!(
            s.take(&tok, "other", Some("bob@example.org"), true, "u1")
                .is_err()
        );
        assert!(
            s.take(&tok, "gc", Some("bob@example.org"), false, "u1")
                .is_err()
        );
        assert!(
            s.would_take(&tok, "gc", Some("BOB@example.org"), true)
                .unwrap()
                .is_some()
        );
        let taken = s
            .take(&tok, "gc", Some("BOB@example.org"), true, "u1")
            .unwrap();
        assert_eq!(taken.taken_by.as_deref(), Some("u1"));
        // Once.
        assert!(
            s.take(&tok, "gc", Some("bob@example.org"), true, "u2")
                .is_err()
        );
        assert!(s.open(&tok).unwrap().is_none());
        assert_eq!(
            s.list(Some("t1")).unwrap().len(),
            1,
            "taken ones stay listed"
        );
        assert!(s.list(Some("t2")).unwrap().is_empty());
        // Revoking deletes.
        let (inv2, _) = s
            .create("t1", "c@example.org", "r", "gc", "ada", Some(1))
            .unwrap();
        s.revoke(&inv2.id).unwrap();
        assert!(s.get(&inv2.id).unwrap().is_none());
        assert!(matches!(s.revoke(&inv2.id), Err(InvitationError::NotFound)));
    }

    #[test]
    fn bad_input_is_refused() {
        let (s, _d) = store();
        assert!(
            s.create("t1", "not-an-email", "r", "gc", "ada", None)
                .is_err()
        );
        assert!(
            s.create("t1", "a@b.org", "r", "gc", "ada", Some(0))
                .is_err()
        );
        assert!(
            s.create("t1", "a@b.org", "r", "gc", "ada", Some(31))
                .is_err()
        );
        assert!(InviteToken::parse("no-dot").is_none());
        assert!(InviteToken::parse("a.b/c").is_none());
    }

    #[test]
    fn expired_ones_are_listed_for_a_while_then_go() {
        let i = |taken: Option<u64>, expires: u64| Invitation {
            id: "x".into(),
            tenant_id: "t".into(),
            email: "a@b.org".into(),
            role: "r".into(),
            connector_id: "c".into(),
            invited_by: "ada".into(),
            created_at: 0,
            expires_at: expires,
            secret_hash: String::new(),
            taken_by: None,
            taken_at: taken,
        };
        assert!(listed(&i(None, 100), 100 + KEEP_SECS - 1));
        assert!(!listed(&i(None, 100), 100 + KEEP_SECS));
        assert!(listed(&i(Some(50), 100), 50 + KEEP_SECS - 1));
        assert!(!i(None, 100).is_open(100));
    }
}
