//! Identity, session, token, policy and audit records (spec/security.md
//! §5, §6.1, §7.6, §10), in the control-plane database.
//!
//! Every table maps a string key to a JSON value, like the other stores.
//! Secrets are never stored: sessions and tokens are keyed by the SHA-256
//! of their value.

use crate::authz::{Ent, Link};
use crate::store::Db;
use redb::{ReadableTable, TableDefinition};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

const USERS: TableDefinition<&str, &[u8]> = TableDefinition::new("users");
/// `<provider>:<subject>` → user id.
const IDENTITIES: TableDefinition<&str, &[u8]> = TableDefinition::new("identities");
const TEAMS: TableDefinition<&str, &[u8]> = TableDefinition::new("teams");
const LINKS: TableDefinition<&str, &[u8]> = TableDefinition::new("policy_links");
/// SHA-256 (hex) of the session id → session.
const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
/// SHA-256 (hex) of the token → token.
const TOKENS: TableDefinition<&str, &[u8]> = TableDefinition::new("api_tokens");
const SITE_POLICIES: TableDefinition<&str, &[u8]> = TableDefinition::new("site_policies");
/// `<id>@<version, zero-padded>` → version.
const SITE_POLICY_VERSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("site_policy_versions");
/// `<unix millis, zero-padded>-<seq>` → audit entry.
const AUDIT: TableDefinition<&str, &[u8]> = TableDefinition::new("audit");

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error("identity storage error: {0}")]
    Storage(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self {
                StoreError::Storage(e.to_string())
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

pub fn now() -> u64 {
    crate::tenancy::now()
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub disabled: bool,
    /// Project used when a request doesn't name one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project: Option<String>,
    pub created_at: u64,
}

/// A login identity linked to a user (spec §6.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// `unix`, `pam`, or `oidc:<issuer>`.
    pub provider: String,
    pub subject: String,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub created_at: u64,
}

impl Identity {
    pub fn key(provider: &str, subject: &str) -> String {
        format!("{}:{}", provider, subject)
    }

    /// `unix` and `pam` identities are names on one host, not global
    /// (spec/clustering.md D13).
    pub fn is_host_local(&self) -> bool {
        self.provider == "unix" || self.provider == "pam"
    }

    /// The subject of a host-local identity scoped to `node`: `alice@<node>`.
    pub fn scoped_subject(name: &str, node: &str) -> String {
        format!("{}@{}", name, node)
    }

    /// Split `alice@<node>` into the name and the node.
    pub fn split_scoped(subject: &str) -> Option<(&str, &str)> {
        subject.rsplit_once('@')
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberSource {
    Manual,
    Pam,
    Oidc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    pub user_id: String,
    pub source: MemberSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub members: Vec<TeamMember>,
    pub created_at: u64,
}

/// A stored role link (an [`authz::Link`](crate::authz::Link) plus who made it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRecord {
    #[serde(flatten)]
    pub link: Link,
    pub created_by: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub user_id: String,
    pub method: String,
    pub csrf: String,
    pub created_at: u64,
    pub last_seen: u64,
    /// For step-up (`base.step-up`).
    pub authenticated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TokenKind {
    /// Acts as `owner`; narrowed by its own links when it has any.
    Personal { owner: String },
    /// Belongs to a project; only its own links count.
    ServiceAccount { project: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    /// Public id (the Cedar `Token` entity id); not the secret.
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub kind: TokenKind,
    pub created_by: String,
    pub created_at: u64,
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_from: Option<String>,
    /// Device the minting client claimed (`gxctl auth login`); display and
    /// audit only, never checked (spec/gxctl-auth.md §7.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Client build that minted the token (`gxctl/<version>`); display and
    /// audit only, never checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitePolicy {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub description: String,
    pub enabled: bool,
    pub version: u64,
    pub updated_by: String,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitePolicyVersion {
    pub id: String,
    pub version: u64,
    pub text: String,
    pub enabled: bool,
    pub author: String,
    pub time: u64,
    /// The policy was deleted at this version.
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Unix milliseconds.
    pub time: u64,
    pub request_id: String,
    pub principal: serde_json::Value,
    pub source: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

/// Versions of a site policy kept by default (spec §7.6).
pub const POLICY_HISTORY: usize = 50;

enum Rekey {
    Keep,
    Drop,
    To(String),
}

pub struct IdentityStore {
    db: Arc<Db>,
    audit_seq: std::sync::atomic::AtomicU64,
    /// Versions kept per site policy (config `authz.policy_history`).
    policy_history: std::sync::atomic::AtomicUsize,
}

fn get<T: DeserializeOwned>(
    db: &Db,
    table: crate::store::Def,
    key: &str,
) -> Result<Option<T>, StoreError> {
    let txn = db.begin_read()?;
    let t = txn.open_table(table)?;
    Ok(match t.get(key)? {
        Some(v) => Some(serde_json::from_slice(v.value())?),
        None => None,
    })
}

fn table_id(table: crate::store::Def) -> crate::store::TableId {
    use redb::TableHandle;
    crate::store::TableId::from_name(table.name()).expect("a known table")
}

fn put<T: Serialize>(db: &Db, table: crate::store::Def, key: &str, value: &T) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(value)?;
    let txn = match db.begin(crate::store::Origin::Auth) {
        Ok(t) => t,
        // A follower authenticating someone (a session, a user): the leader writes.
        Err(crate::store::StoreError::NotLeader { .. }) => {
            return Ok(db.forward_raw(vec![crate::store::Op::Put { table: table_id(table), key: key.as_bytes().to_vec(), value: bytes }])?)
        }
        Err(e) => return Err(e.into()),
    };
    {
        let mut t = txn.open_table(table)?;
        t.insert(key, bytes.as_slice())?;
    }
    txn.commit()?;
    Ok(())
}

fn remove(db: &Db, table: crate::store::Def, key: &str) -> Result<bool, StoreError> {
    let txn = match db.begin(crate::store::Origin::Auth) {
        Ok(t) => t,
        Err(crate::store::StoreError::NotLeader { .. }) => {
            db.forward_raw(vec![crate::store::Op::Delete { table: table_id(table), key: key.as_bytes().to_vec() }])?;
            return Ok(true);
        }
        Err(e) => return Err(e.into()),
    };
    let existed = {
        let mut t = txn.open_table(table)?;
        let existed = t.remove(key)?.is_some();
        existed
    };
    txn.commit()?;
    Ok(existed)
}

fn list<T: DeserializeOwned>(db: &Db, table: crate::store::Def) -> Result<Vec<(String, T)>, StoreError> {
    let txn = db.begin_read()?;
    let t = txn.open_table(table)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (k, v) = entry?;
        out.push((k.value().to_string(), serde_json::from_slice(v.value())?));
    }
    Ok(out)
}

impl IdentityStore {
    pub fn new(db: Arc<Db>) -> Result<Self, StoreError> {
        Ok(Self { db, audit_seq: Default::default(), policy_history: POLICY_HISTORY.into() })
    }

    pub fn set_policy_history(&self, n: usize) {
        self.policy_history.store(n.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn database(&self) -> Arc<Db> {
        self.db.clone()
    }

    // ---- users and identities ------------------------------------------

    pub fn user(&self, id: &str) -> Result<Option<User>, StoreError> {
        get(&self.db, USERS, id)
    }

    pub fn users(&self) -> Result<Vec<User>, StoreError> {
        let mut v: Vec<User> = list(&self.db, USERS)?.into_iter().map(|(_, u)| u).collect();
        v.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        Ok(v)
    }

    pub fn put_user(&self, u: &User) -> Result<(), StoreError> {
        put(&self.db, USERS, &u.id, u)
    }

    pub fn identity(&self, provider: &str, subject: &str) -> Result<Option<Identity>, StoreError> {
        get(&self.db, IDENTITIES, &Identity::key(provider, subject))
    }

    pub fn identities_of(&self, user_id: &str) -> Result<Vec<Identity>, StoreError> {
        Ok(list::<Identity>(&self.db, IDENTITIES)?
            .into_iter()
            .map(|(_, i)| i)
            .filter(|i| i.user_id == user_id)
            .collect())
    }

    pub fn put_identity(&self, i: &Identity) -> Result<(), StoreError> {
        put(&self.db, IDENTITIES, &Identity::key(&i.provider, &i.subject), i)
    }

    /// D13, at `gxctl cluster init`: re-key every unscoped `unix:` and
    /// `pam:` identity to `<name>@<node>`, in one write. Returns how many
    /// were re-keyed.
    pub fn scope_local_identities(&self, node: &str) -> Result<usize, StoreError> {
        self.rekey_local_identities(|i| match Identity::split_scoped(&i.subject) {
            Some(_) => Rekey::Keep,
            None => Rekey::To(Identity::scoped_subject(&i.subject, node)),
        })
    }

    /// The inverse, when `node` leaves with its resources (§5.8.1): its
    /// identities become plain names again, and those scoped to other nodes
    /// are dropped.
    pub fn unscope_local_identities(&self, node: &str) -> Result<usize, StoreError> {
        self.rekey_local_identities(|i| match Identity::split_scoped(&i.subject) {
            Some((name, n)) if n == node => Rekey::To(name.to_string()),
            Some(_) => Rekey::Drop,
            None => Rekey::Keep,
        })
    }

    fn rekey_local_identities(&self, f: impl Fn(&Identity) -> Rekey) -> Result<usize, StoreError> {
        self.db.write(crate::store::Origin::Auth, |tx| {
            let mut t = tx.open_table(IDENTITIES)?;
            let mut moves = Vec::new();
            for r in t.iter()? {
                let (k, v) = r?;
                let Ok(i) = serde_json::from_slice::<Identity>(v.value()) else { continue };
                if !i.is_host_local() {
                    continue;
                }
                match f(&i) {
                    Rekey::Keep => {}
                    Rekey::Drop => moves.push((k.value().to_string(), None)),
                    Rekey::To(subject) => moves.push((k.value().to_string(), Some(Identity { subject, ..i }))),
                }
            }
            let n = moves.len();
            for (old, new) in moves {
                t.remove(&old)?;
                if let Some(i) = new {
                    t.insert(&Identity::key(&i.provider, &i.subject), serde_json::to_vec(&i)?.as_slice())?;
                }
            }
            Ok(n)
        })
    }

    pub fn remove_identity(&self, provider: &str, subject: &str) -> Result<bool, StoreError> {
        remove(&self.db, IDENTITIES, &Identity::key(provider, subject))
    }

    /// The user behind an identity, creating both when `create` is set.
    /// A new `pam:` or `unix:` identity links to the user of the other
    /// one for the same login name (spec §6.1).
    pub fn user_for_identity(
        &self,
        provider: &str,
        subject: &str,
        display_name: &str,
        email: Option<String>,
        create: bool,
    ) -> Result<Option<User>, StoreError> {
        if let Some(i) = self.identity(provider, subject)? {
            return self.user(&i.user_id);
        }
        if !create {
            return Ok(None);
        }
        // A follower: the leader creates the user and identity, once, however
        // many servers see this person log in at the same moment.
        if !self.db.can_write() && self.db.is_replicated() {
            let v = self.db.forward_call("identity", serde_json::json!({ "provider": provider, "subject": subject, "display_name": display_name, "email": email }))?;
            return Ok(serde_json::from_value(v)?);
        }
        let twin = match provider {
            "pam" => self.identity("unix", subject)?,
            "unix" => self.identity("pam", subject)?,
            _ => None,
        };
        let user = match twin.and_then(|t| self.user(&t.user_id).transpose()) {
            Some(u) => u?,
            None => {
                let u = User {
                    id: uuid::Uuid::new_v4().to_string(),
                    display_name: display_name.to_string(),
                    disabled: false,
                    default_project: None,
                    created_at: now(),
                };
                self.put_user(&u)?;
                u
            }
        };
        self.put_identity(&Identity {
            provider: provider.to_string(),
            subject: subject.to_string(),
            user_id: user.id.clone(),
            email,
            created_at: now(),
        })?;
        Ok(Some(user))
    }

    // ---- teams ----------------------------------------------------------

    pub fn team(&self, id: &str) -> Result<Option<Team>, StoreError> {
        get(&self.db, TEAMS, id)
    }

    pub fn teams(&self) -> Result<Vec<Team>, StoreError> {
        let mut v: Vec<Team> = list(&self.db, TEAMS)?.into_iter().map(|(_, t)| t).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    pub fn team_by_name(&self, name: &str) -> Result<Option<Team>, StoreError> {
        Ok(self.teams()?.into_iter().find(|t| t.name == name))
    }

    pub fn put_team(&self, t: &Team) -> Result<(), StoreError> {
        put(&self.db, TEAMS, &t.id, t)
    }

    pub fn remove_team(&self, id: &str) -> Result<bool, StoreError> {
        remove(&self.db, TEAMS, id)
    }

    /// Ids of the stored teams `user_id` is a member of.
    pub fn teams_of(&self, user_id: &str) -> Result<Vec<String>, StoreError> {
        Ok(self
            .teams()?
            .into_iter()
            .filter(|t| t.members.iter().any(|m| m.user_id == user_id))
            .map(|t| t.id)
            .collect())
    }

    /// Make `user_id`'s `source` memberships exactly the teams named in
    /// `wanted` (spec §5.3, §5.4). Manual memberships are left alone.
    pub fn sync_memberships(&self, user_id: &str, source: MemberSource, wanted: &[String]) -> Result<(), StoreError> {
        for mut t in self.teams()? {
            let want = wanted.contains(&t.name);
            let has = t.members.iter().any(|m| m.user_id == user_id && m.source == source);
            let manual = t.members.iter().any(|m| m.user_id == user_id && m.source == MemberSource::Manual);
            if want && !has && !manual {
                t.members.push(TeamMember { user_id: user_id.to_string(), source });
                self.put_team(&t)?;
            } else if !want && has {
                t.members.retain(|m| !(m.user_id == user_id && m.source == source));
                self.put_team(&t)?;
            }
        }
        Ok(())
    }

    // ---- role links -----------------------------------------------------

    pub fn links(&self) -> Result<Vec<LinkRecord>, StoreError> {
        Ok(list(&self.db, LINKS)?.into_iter().map(|(_, l)| l).collect())
    }

    pub fn put_link(&self, l: &LinkRecord) -> Result<(), StoreError> {
        put(&self.db, LINKS, &l.link.id, l)
    }

    pub fn remove_link(&self, id: &str) -> Result<bool, StoreError> {
        remove(&self.db, LINKS, id)
    }

    /// Links whose principal or resource is `e` (cleanup on delete).
    pub fn links_mentioning(&self, e: &Ent) -> Result<Vec<LinkRecord>, StoreError> {
        Ok(self.links()?.into_iter().filter(|l| l.link.principal == *e || l.link.resource == *e).collect())
    }

    // ---- sessions -------------------------------------------------------

    pub fn session(&self, hash: &str) -> Result<Option<Session>, StoreError> {
        get(&self.db, SESSIONS, hash)
    }

    pub fn put_session(&self, hash: &str, s: &Session) -> Result<(), StoreError> {
        put(&self.db, SESSIONS, hash, s)
    }

    pub fn remove_session(&self, hash: &str) -> Result<bool, StoreError> {
        remove(&self.db, SESSIONS, hash)
    }

    /// Drop every session of a user (disable, logout everywhere).
    pub fn remove_sessions_of(&self, user_id: &str) -> Result<usize, StoreError> {
        let mine: Vec<String> = list::<Session>(&self.db, SESSIONS)?
            .into_iter()
            .filter(|(_, s)| s.user_id == user_id)
            .map(|(k, _)| k)
            .collect();
        for k in &mine {
            remove(&self.db, SESSIONS, k)?;
        }
        Ok(mine.len())
    }

    /// Drop sessions for which `expired` is true.
    pub fn prune_sessions(&self, expired: impl Fn(&Session) -> bool) -> Result<usize, StoreError> {
        let old: Vec<String> = list::<Session>(&self.db, SESSIONS)?
            .into_iter()
            .filter(|(_, s)| expired(s))
            .map(|(k, _)| k)
            .collect();
        for k in &old {
            remove(&self.db, SESSIONS, k)?;
        }
        Ok(old.len())
    }

    // ---- tokens ---------------------------------------------------------

    pub fn token_by_hash(&self, hash: &str) -> Result<Option<Token>, StoreError> {
        get(&self.db, TOKENS, hash)
    }

    pub fn tokens(&self) -> Result<Vec<(String, Token)>, StoreError> {
        list(&self.db, TOKENS)
    }

    pub fn token_by_id(&self, id: &str) -> Result<Option<(String, Token)>, StoreError> {
        Ok(self.tokens()?.into_iter().find(|(_, t)| t.id == id))
    }

    pub fn put_token(&self, hash: &str, t: &Token) -> Result<(), StoreError> {
        put(&self.db, TOKENS, hash, t)
    }

    pub fn remove_token(&self, hash: &str) -> Result<bool, StoreError> {
        remove(&self.db, TOKENS, hash)
    }

    // ---- site policies --------------------------------------------------

    pub fn site_policies(&self) -> Result<Vec<SitePolicy>, StoreError> {
        let mut v: Vec<SitePolicy> = list(&self.db, SITE_POLICIES)?.into_iter().map(|(_, p)| p).collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(v)
    }

    pub fn site_policy(&self, id: &str) -> Result<Option<SitePolicy>, StoreError> {
        get(&self.db, SITE_POLICIES, id)
    }

    /// Write (or, with `None`, delete) a site policy if its stored version
    /// is `expected` (0: must not exist), recording the new version in
    /// the history, in one transaction (spec §7.6).
    pub fn write_site_policy(
        &self,
        id: &str,
        expected: u64,
        new: Option<(&str, &str, bool)>,
        author: &str,
    ) -> Result<Option<SitePolicy>, StoreError> {
        let txn = self.db.begin(crate::store::Origin::Auth)?;
        let result = {
            let mut t = txn.open_table(SITE_POLICIES)?;
            let current: Option<SitePolicy> = match t.get(id)? {
                Some(v) => Some(serde_json::from_slice(v.value())?),
                None => None,
            };
            let current_version = current.as_ref().map(|p| p.version).unwrap_or(0);
            if current_version != expected {
                return Err(StoreError::Conflict(format!(
                    "policy {} is at version {}, not {}",
                    id, current_version, expected
                )));
            }
            if new.is_none() && current.is_none() {
                return Err(StoreError::NotFound(id.to_string()));
            }
            let version = current_version + 1;
            let ts = now();
            let mut h = txn.open_table(SITE_POLICY_VERSIONS)?;
            let (text, enabled, deleted) = match new {
                Some((text, _, enabled)) => (text.to_string(), enabled, false),
                None => (current.as_ref().map(|p| p.text.clone()).unwrap_or_default(), false, true),
            };
            let entry = SitePolicyVersion { id: id.to_string(), version, text, enabled, author: author.to_string(), time: ts, deleted };
            h.insert(format!("{}@{:012}", id, version).as_str(), serde_json::to_vec(&entry)?.as_slice())?;
            // Keep the newest `policy_history` versions.
            let keep = self.policy_history.load(std::sync::atomic::Ordering::Relaxed);
            let prefix = format!("{}@", id);
            let mut keys = Vec::new();
            for e in h.range(prefix.as_str()..)? {
                let (k, _) = e?;
                if !k.value().starts_with(&prefix) {
                    break;
                }
                keys.push(k.value().to_string());
            }
            if keys.len() > keep {
                for k in &keys[..keys.len() - keep] {
                    h.remove(k.as_str())?;
                }
            }
            match new {
                Some((text, description, enabled)) => {
                    let p = SitePolicy {
                        id: id.to_string(),
                        text: text.to_string(),
                        description: description.to_string(),
                        enabled,
                        version,
                        updated_by: author.to_string(),
                        updated_at: ts,
                    };
                    t.insert(id, serde_json::to_vec(&p)?.as_slice())?;
                    Some(p)
                }
                None => {
                    t.remove(id)?;
                    None
                }
            }
        };
        txn.commit()?;
        Ok(result)
    }

    pub fn site_policy_versions(&self, id: &str) -> Result<Vec<SitePolicyVersion>, StoreError> {
        let txn = self.db.begin_read()?;
        let h = txn.open_table(SITE_POLICY_VERSIONS)?;
        let prefix = format!("{}@", id);
        let mut out = Vec::new();
        for e in h.range(prefix.as_str()..)? {
            let (k, v) = e?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            out.push(serde_json::from_slice(v.value())?);
        }
        Ok(out)
    }

    // ---- audit ----------------------------------------------------------

    pub fn append_audit(&self, e: &AuditEntry) -> Result<(), StoreError> {
        let seq = self.audit_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        put(&self.db, AUDIT, &format!("{:016}-{:08}", e.time, seq % 100_000_000), e)
    }

    /// Entries at or after `since` (unix millis), oldest first, at most `limit`.
    pub fn audit(&self, since: u64, limit: usize, filter: impl Fn(&AuditEntry) -> bool) -> Result<Vec<AuditEntry>, StoreError> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(AUDIT)?;
        let start = format!("{:016}", since);
        let mut out = Vec::new();
        for e in t.range(start.as_str()..)? {
            let (_, v) = e?;
            let entry: AuditEntry = serde_json::from_slice(v.value())?;
            if filter(&entry) {
                out.push(entry);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Delete entries older than `before` (unix millis).
    pub fn prune_audit(&self, before: u64) -> Result<usize, StoreError> {
        let txn = self.db.begin(crate::store::Origin::Auth)?;
        let n = {
            let mut t = txn.open_table(AUDIT)?;
            let end = format!("{:016}", before);
            let keys: Vec<String> = t
                .range(..end.as_str())?
                .map(|e| e.map(|(k, _)| k.value().to_string()))
                .collect::<Result<_, _>>()?;
            for k in &keys {
                t.remove(k.as_str())?;
            }
            keys.len()
        };
        txn.commit()?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (IdentityStore, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Db::create(dir.path().join("i.db")).unwrap());
        (IdentityStore::new(db).unwrap(), dir)
    }

    #[test]
    fn local_identities_are_scoped_to_a_node_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::create(dir.path().join("i.db")).unwrap());
        let s = IdentityStore::new(db).unwrap();
        let alice = s.user_for_identity("unix", "alice", "alice", None, true).unwrap().unwrap();
        let bob = s.user_for_identity("pam", "bob", "bob", None, true).unwrap().unwrap();
        let sso = s.user_for_identity("oidc:https://idp", "carol", "carol", None, true).unwrap().unwrap();
        assert_eq!(s.scope_local_identities("n1").unwrap(), 2);
        assert!(s.identity("unix", "alice").unwrap().is_none());
        assert_eq!(s.identity("unix", "alice@n1").unwrap().unwrap().user_id, alice.id);
        assert_eq!(s.identity("pam", "bob@n1").unwrap().unwrap().user_id, bob.id);
        // OIDC identities are global and untouched.
        assert_eq!(s.identity("oidc:https://idp", "carol").unwrap().unwrap().user_id, sso.id);
        // Idempotent.
        assert_eq!(s.scope_local_identities("n1").unwrap(), 0);
        // Another node's identity appears (a second host's alice).
        let other = Identity { provider: "unix".into(), subject: "dave@n2".into(), user_id: alice.id.clone(), email: None, created_at: 1 };
        s.put_identity(&other).unwrap();
        // Leaving with n1's resources: n1's names are plain again, n2's are dropped.
        assert_eq!(s.unscope_local_identities("n1").unwrap(), 3);
        assert_eq!(s.identity("unix", "alice").unwrap().unwrap().user_id, alice.id);
        assert_eq!(s.identity("pam", "bob").unwrap().unwrap().user_id, bob.id);
        assert!(s.identity("unix", "dave@n2").unwrap().is_none());
    }

    #[test]
    fn pam_and_unix_identities_share_a_user() {
        let (s, _d) = store();
        let a = s.user_for_identity("unix", "alice", "alice", None, true).unwrap().unwrap();
        let b = s.user_for_identity("pam", "alice", "alice", None, true).unwrap().unwrap();
        assert_eq!(a.id, b.id);
        let o = s.user_for_identity("oidc:https://idp", "alice", "Alice", None, true).unwrap().unwrap();
        assert_ne!(o.id, a.id, "never linked by name or email across providers");
        assert!(s.user_for_identity("pam", "bob", "bob", None, false).unwrap().is_none());
    }

    #[test]
    fn membership_sync_keeps_manual() {
        let (s, _d) = store();
        for name in ["a", "b", "c"] {
            s.put_team(&Team { id: name.into(), name: name.into(), members: vec![], created_at: 0 }).unwrap();
        }
        let mut c = s.team("c").unwrap().unwrap();
        c.members.push(TeamMember { user_id: "u".into(), source: MemberSource::Manual });
        s.put_team(&c).unwrap();
        s.sync_memberships("u", MemberSource::Oidc, &["a".into(), "b".into()]).unwrap();
        assert_eq!(s.teams_of("u").unwrap(), vec!["a", "b", "c"]);
        s.sync_memberships("u", MemberSource::Oidc, &["b".into()]).unwrap();
        assert_eq!(s.teams_of("u").unwrap(), vec!["b", "c"]);
        s.sync_memberships("u", MemberSource::Oidc, &[]).unwrap();
        assert_eq!(s.teams_of("u").unwrap(), vec!["c"]);
    }

    #[test]
    fn site_policy_versions_and_conflicts() {
        let (s, _d) = store();
        assert!(matches!(s.write_site_policy("site.a", 1, Some(("x", "", true)), "u"), Err(StoreError::Conflict(_))));
        let p = s.write_site_policy("site.a", 0, Some(("x", "d", true)), "u").unwrap().unwrap();
        assert_eq!(p.version, 1);
        assert!(matches!(s.write_site_policy("site.a", 0, Some(("y", "", true)), "u"), Err(StoreError::Conflict(_))));
        s.write_site_policy("site.a", 1, Some(("y", "", false)), "u").unwrap();
        s.write_site_policy("site.a", 2, None, "u").unwrap();
        assert!(s.site_policy("site.a").unwrap().is_none());
        let v = s.site_policy_versions("site.a").unwrap();
        assert_eq!(v.iter().map(|v| v.version).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(v[2].deleted);
        for i in 0..60 {
            s.write_site_policy("site.b", i, Some(("t", "", true)), "u").unwrap();
        }
        assert_eq!(s.site_policy_versions("site.b").unwrap().len(), POLICY_HISTORY);
        s.set_policy_history(5);
        s.write_site_policy("site.b", 60, Some(("t", "", true)), "u").unwrap();
        assert_eq!(s.site_policy_versions("site.b").unwrap().len(), 5);
    }

    #[test]
    fn audit_append_query_prune() {
        let (s, _d) = store();
        for t in [1000, 2000, 3000] {
            s.append_audit(&AuditEntry {
                time: t,
                request_id: "r".into(),
                principal: serde_json::json!({"user": "u"}),
                source: "test".into(),
                action: "readVm".into(),
                project: None,
                target: None,
                result: "allow".into(),
                error_code: None,
                policies: vec![],
                details: serde_json::Value::Null,
            })
            .unwrap();
        }
        assert_eq!(s.audit(1500, 10, |_| true).unwrap().len(), 2);
        assert_eq!(s.prune_audit(2500).unwrap(), 2);
        assert_eq!(s.audit(0, 10, |_| true).unwrap().len(), 1);
    }
}
