//! Identity, session, token, policy and audit records (spec/11-security.md
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
const SCIM_GROUPS: TableDefinition<&str, &[u8]> = TableDefinition::new("scim_groups");
/// SHA-256 (hex) of the session id → session.
const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
/// SHA-256 (hex) of the token → token.
const TOKENS: TableDefinition<&str, &[u8]> = TableDefinition::new("api_tokens");
const SITE_POLICIES: TableDefinition<&str, &[u8]> = TableDefinition::new("site_policies");
/// `<id>@<version, zero-padded>` → version.
const SITE_POLICY_VERSIONS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("site_policy_versions");
/// `<unix millis, zero-padded>-<seq>` → audit entry.
const AUDIT: TableDefinition<&str, &[u8]> = TableDefinition::new("audit");
/// `<directory>/<unix millis, zero-padded>-<seq>` → audit entry of a
/// hosted directory (spec/19-tenancy.md D74).
const DIRECTORY_AUDIT: TableDefinition<&str, &[u8]> = TableDefinition::new("directory_audit");

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

/// The directory of a record from before T6 (spec/19-tenancy.md D58).
pub fn local_directory() -> String {
    crate::tenants::LOCAL_DIRECTORY.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    /// Its directory (spec/19-tenancy.md D58); `local` from before T6.
    #[serde(default = "local_directory")]
    pub directory_id: String,
    pub display_name: String,
    #[serde(default)]
    pub disabled: bool,
    /// Project used when a request doesn't name one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project: Option<String>,
    pub created_at: u64,
    /// The connector that provisions it (spec/19-tenancy.md §8.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<Managed>,
    /// Its provisioned email (a managed user's; logins keep theirs on the identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Why it is disabled, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<DisabledReason>,
    /// Pre-added (spec/19-tenancy.md D77): the first login through this
    /// connector with this verified address takes the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_login: Option<PendingLogin>,
}

/// A pre-added user's way in (spec/19-tenancy.md §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLogin {
    pub connector: String,
    /// Lower case.
    pub email: String,
}

impl PendingLogin {
    pub fn is(&self, connector: &str, email: &str) -> bool {
        self.connector == connector && self.email.eq_ignore_ascii_case(email)
    }
}

/// A user or team a connector provisions (spec/19-tenancy.md §8): its
/// fields are the connector's (§8.3). Its SCIM `id` is its glidex id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Managed {
    pub connector: String,
    /// The IdP's own id (SCIM `externalId`), when it sends one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// SCIM `userName` (users only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    /// SCIM `name.givenName` and `name.familyName`, kept so the IdP reads
    /// back what it wrote (users only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
    /// SSH public keys its IdP holds (a pull's `sync.ssh_keys_field`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_keys: Vec<String>,
}

/// A group a connector's IdP pushed over SCIM (spec/19-tenancy.md D43). Its
/// members are user ids; with `group_mode: auto` it is also the team of
/// the same id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimGroup {
    pub id: String,
    pub connector: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(default)]
    pub members: Vec<String>,
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

/// Why a user is disabled (D20, D42, §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisabledReason {
    /// An administrator: only an administrator enables it again.
    Admin,
    /// Its IdP, over SCIM (`active: false` or `DELETE`).
    Scim,
    /// Its connector was deleted.
    ConnectorDeleted,
    /// A pull found it gone, out of the groups, suspended or archived (D45).
    Sync,
}

impl DisabledReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            DisabledReason::Admin => "admin",
            DisabledReason::Scim => "scim",
            DisabledReason::ConnectorDeleted => "connector_deleted",
            DisabledReason::Sync => "sync",
        }
    }
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
    /// (spec/14-clustering.md D13).
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberSource {
    Manual,
    /// The file-defined connectors keep their names from before connectors.
    Pam,
    Oidc,
    /// Synced by this connector (spec/19-tenancy.md D18).
    Connector(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    pub user_id: String,
    pub source: MemberSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    /// Its directory (D58); `local` from before T6.
    #[serde(default = "local_directory")]
    pub directory_id: String,
    pub name: String,
    #[serde(default)]
    pub members: Vec<TeamMember>,
    pub created_at: u64,
    /// The connector whose group it is (`group_mode: auto`, D43).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<Managed>,
}

/// A stored role link (an [`authz::Link`](crate::authz::Link) plus who made it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkRecord {
    #[serde(flatten)]
    pub link: Link,
    pub created_by: String,
    pub created_at: u64,
}

/// What deprovisioning a user took away (D20, counts).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Deprovisioned {
    pub sessions: usize,
    pub tokens: usize,
    pub teams: usize,
}

/// What deleting a user took with it (counts).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeletedUser {
    pub identities: usize,
    pub sessions: usize,
    pub tokens: usize,
    pub links: usize,
    pub teams: usize,
    pub tenants: usize,
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
    /// The login was multi-factor (`context.auth.mfa`, spec/19-tenancy.md §6).
    #[serde(default)]
    pub mfa: bool,
    /// A trusted directory's login (D69): its chain, `sid`; the session
    /// ends when the directory's feed says that chain ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
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
    /// audit only, never checked (spec/17-gxctl-auth.md §7.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Client build that minted the token (`gxctl/<version>`); display and
    /// audit only, never checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// The clusters it is good on (spec/19-tenancy.md §7.4): cluster ids, or
    /// `all`; empty: only where it was made. Fed to the others.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clusters: Vec<String>,
}

/// A token's `clusters` naming every cluster its tenant uses.
pub const ALL_CLUSTERS: &str = "all";

impl Token {
    /// Whether it is good on cluster `id`, `here` being where it lives:
    /// listed there, or `all`; with no list, only where it was made.
    pub fn good_on(&self, id: &str, here_made: bool) -> bool {
        if self.clusters.is_empty() {
            return here_made;
        }
        self.clusters.iter().any(|c| c == id || c == ALL_CLUSTERS)
    }
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

fn put<T: Serialize>(
    db: &Db,
    table: crate::store::Def,
    key: &str,
    value: &T,
) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(value)?;
    let txn = match db.begin(crate::store::Origin::Auth) {
        Ok(t) => t,
        // A follower authenticating someone (a session, a user): the leader writes.
        Err(crate::store::StoreError::NotLeader { .. }) => {
            return Ok(db.forward_raw(vec![crate::store::Op::Put {
                table: table_id(table),
                key: key.as_bytes().to_vec(),
                value: bytes,
            }])?);
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
            db.forward_raw(vec![crate::store::Op::Delete {
                table: table_id(table),
                key: key.as_bytes().to_vec(),
            }])?;
            return Ok(true);
        }
        Err(e) => return Err(e.into()),
    };
    let existed = {
        let mut t = txn.open_table(table)?;

        t.remove(key)?.is_some()
    };
    txn.commit()?;
    Ok(existed)
}

fn list<T: DeserializeOwned>(
    db: &Db,
    table: crate::store::Def,
) -> Result<Vec<(String, T)>, StoreError> {
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
        Ok(Self {
            db,
            audit_seq: Default::default(),
            policy_history: POLICY_HISTORY.into(),
        })
    }

    pub fn set_policy_history(&self, n: usize) {
        self.policy_history
            .store(n.max(1), std::sync::atomic::Ordering::Relaxed);
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

    /// Every identity of `provider` (a connector's logins).
    pub fn identities_of_provider(&self, provider: &str) -> Result<Vec<Identity>, StoreError> {
        Ok(list::<Identity>(&self.db, IDENTITIES)?
            .into_iter()
            .map(|(_, i)| i)
            .filter(|i| i.provider == provider)
            .collect())
    }

    pub fn put_identity(&self, i: &Identity) -> Result<(), StoreError> {
        put(
            &self.db,
            IDENTITIES,
            &Identity::key(&i.provider, &i.subject),
            i,
        )
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
                let Ok(i) = serde_json::from_slice::<Identity>(v.value()) else {
                    continue;
                };
                if !i.is_host_local() {
                    continue;
                }
                match f(&i) {
                    Rekey::Keep => {}
                    Rekey::Drop => moves.push((k.value().to_string(), None)),
                    Rekey::To(subject) => {
                        moves.push((k.value().to_string(), Some(Identity { subject, ..i })))
                    }
                }
            }
            let n = moves.len();
            for (old, new) in moves {
                t.remove(&old)?;
                if let Some(i) = new {
                    t.insert(
                        &Identity::key(&i.provider, &i.subject),
                        serde_json::to_vec(&i)?.as_slice(),
                    )?;
                }
            }
            Ok(n)
        })
    }
    /// §8.3: connector `c` goes; its users and teams stop being managed,
    /// in one write. With `disable`, its users are disabled as
    /// [`deprovision`](Self::deprovision) does and its memberships go;
    /// otherwise they stay and its memberships become manual. Returns how
    /// many users and teams it managed.
    pub fn release_connector(&self, c: &str, disable: bool) -> Result<(usize, usize), StoreError> {
        let source = MemberSource::Connector(c.to_string());
        self.db.write(crate::store::Origin::Auth, |tx| {
            let mut ids = Vec::new();
            {
                let mut t = tx.open_table(USERS)?;
                let mut changed = Vec::new();
                for r in t.iter()? {
                    let (k, v) = r?;
                    if let Ok(mut u) = serde_json::from_slice::<User>(v.value())
                        && u.managed.as_ref().is_some_and(|m| m.connector == c)
                    {
                        u.managed = None;
                        if disable {
                            if !(u.disabled && u.disabled_reason == Some(DisabledReason::Admin)) {
                                u.disabled_reason = Some(DisabledReason::ConnectorDeleted);
                            }
                            u.disabled = true;
                        }
                        ids.push(u.id.clone());
                        changed.push((k.value().to_string(), u));
                    }
                }
                for (k, u) in changed {
                    t.insert(k.as_str(), serde_json::to_vec(&u)?.as_slice())?;
                }
            }
            if disable && !ids.is_empty() {
                for (table, is_mine) in [
                    (
                        SESSIONS,
                        &(|v: &[u8]| {
                            serde_json::from_slice::<Session>(v)
                                .is_ok_and(|s| ids.contains(&s.user_id))
                        }) as &dyn Fn(&[u8]) -> bool,
                    ),
                    (TOKENS, &|v: &[u8]| {
                        serde_json::from_slice::<Token>(v).is_ok_and(|k| {
                            matches!(&k.kind, TokenKind::Personal { owner } if ids.contains(owner))
                        })
                    }),
                ] {
                    let mut t = tx.open_table(table)?;
                    let gone: Vec<String> = t
                        .iter()?
                        .filter_map(|r| r.ok())
                        .filter(|(_, v)| is_mine(v.value()))
                        .map(|(k, _)| k.value().to_string())
                        .collect();
                    for k in gone {
                        t.remove(k.as_str())?;
                    }
                }
            }
            let mut teams = 0;
            {
                let mut t = tx.open_table(TEAMS)?;
                let mut changed = Vec::new();
                for r in t.iter()? {
                    let (k, v) = r?;
                    let Ok(mut team) = serde_json::from_slice::<Team>(v.value()) else {
                        continue;
                    };
                    let mut dirty = false;
                    if team.managed.as_ref().is_some_and(|m| m.connector == c) {
                        team.managed = None;
                        teams += 1;
                        dirty = true;
                    }
                    if disable {
                        let before = team.members.len();
                        team.members.retain(|m| m.source != source);
                        dirty |= team.members.len() != before;
                    } else {
                        for m in team.members.iter_mut().filter(|m| m.source == source) {
                            m.source = MemberSource::Manual;
                            dirty = true;
                        }
                        // A manual and a converted membership are one.
                        let mut seen = std::collections::HashSet::new();
                        team.members.retain(|m| {
                            seen.insert(m.user_id.clone()) || m.source != MemberSource::Manual
                        });
                    }
                    if dirty {
                        changed.push((k.value().to_string(), team));
                    }
                }
                for (k, team) in changed {
                    t.insert(k.as_str(), serde_json::to_vec(&team)?.as_slice())?;
                }
            }
            Ok((ids.len(), teams))
        })
    }

    /// D20: disable user `id` for `reason` in one write: its sessions end,
    /// its personal tokens are revoked, and the memberships its connector
    /// gave it (`connector:<id>`) go. An administrator's disabling stays
    /// the reason when there is one. `None` when there is no such user.
    pub fn deprovision(
        &self,
        id: &str,
        reason: DisabledReason,
    ) -> Result<Option<(User, Deprovisioned)>, StoreError> {
        self.db.write(crate::store::Origin::Auth, |tx| {
            deprovision_in(tx, id, reason)
        })
    }

    /// Run a SCIM request's writes (spec/19-tenancy.md §8.1) for `connector`
    /// in one write.
    pub fn scim<R, E: From<StoreError> + From<crate::store::StoreError>>(
        &self,
        directory: &str,
        connector: &str,
        f: impl FnOnce(&ScimTx<'_>) -> Result<R, E>,
    ) -> Result<R, E> {
        self.db.write(crate::store::Origin::Auth, |tx| {
            f(&ScimTx {
                tx,
                connector: connector.to_string(),
                directory: directory.to_string(),
            })
        })
    }

    /// D41: the managed user of `connector` a login is, linking its
    /// identity the first time. An identity already linked counts only if
    /// its user is managed by `connector`; otherwise the one enabled
    /// managed user with no identity of `provider` yet whose `external_id`
    /// is `subject`, else whose `userName` or email is `verified_email`.
    pub fn scim_login(
        &self,
        connector: &str,
        provider: &str,
        subject: &str,
        verified_email: Option<&str>,
    ) -> Result<Option<User>, StoreError> {
        self.managed_login(connector, provider, subject, verified_email, true)
    }

    /// D44: as [`scim_login`](Self::scim_login), by the subject only (a
    /// pull keys its users by the IdP's id, which logins carry).
    pub fn pull_login(
        &self,
        connector: &str,
        provider: &str,
        subject: &str,
    ) -> Result<Option<User>, StoreError> {
        self.managed_login(connector, provider, subject, None, false)
    }

    fn managed_login(
        &self,
        connector: &str,
        provider: &str,
        subject: &str,
        verified_email: Option<&str>,
        by_email: bool,
    ) -> Result<Option<User>, StoreError> {
        // It only links an existing user; it makes none.
        self.scim(&local_directory(), connector, |tx| {
            let ours = |u: &User| u.managed.as_ref().is_some_and(|m| m.connector == connector);
            let key = Identity::key(provider, subject);
            let known: Option<Identity> = {
                let t = tx.tx.open_table(IDENTITIES)?;
                t.get(key.as_str())?
                    .map(|v| serde_json::from_slice(v.value()))
                    .transpose()?
            };
            if let Some(i) = known {
                return Ok(tx.user(&i.user_id)?.filter(ours));
            }
            let linked: Vec<String> = tx
                .rows::<Identity>(IDENTITIES)?
                .into_iter()
                .filter(|i| i.provider == provider)
                .map(|i| i.user_id)
                .collect();
            let free: Vec<User> = tx
                .users()?
                .into_iter()
                .filter(|u| !u.disabled && !linked.contains(&u.id))
                .collect();
            let one = |v: Vec<&User>| (v.len() == 1).then(|| v[0].clone());
            let by_id = one(free
                .iter()
                .filter(|u| {
                    u.managed.as_ref().and_then(|m| m.external_id.as_deref()) == Some(subject)
                })
                .collect());
            let by_email = || {
                if !by_email {
                    return None;
                }
                let e = verified_email?;
                one(free
                    .iter()
                    .filter(|u| {
                        u.email
                            .as_deref()
                            .is_some_and(|x| x.eq_ignore_ascii_case(e))
                            || u.managed
                                .as_ref()
                                .and_then(|m| m.user_name.as_deref())
                                .is_some_and(|x| x.eq_ignore_ascii_case(e))
                    })
                    .collect())
            };
            let Some(u) = by_id.or_else(by_email) else {
                return Ok(None);
            };
            tx.put(
                IDENTITIES,
                &key,
                &Identity {
                    provider: provider.to_string(),
                    subject: subject.to_string(),
                    user_id: u.id.clone(),
                    email: verified_email.map(String::from),
                    created_at: now(),
                },
            )?;
            Ok::<_, StoreError>(Some(u))
        })
    }

    /// D77: the user pre-added in `directory` for `connector` and
    /// `verified_email`, its identity linked and its pending login
    /// cleared in one write. `None` when the identity is already known
    /// or no user waits for it.
    pub fn take_pending_login(
        &self,
        directory: &str,
        connector: &str,
        provider: &str,
        subject: &str,
        verified_email: &str,
    ) -> Result<Option<User>, StoreError> {
        self.scim(directory, connector, |tx| {
            let key = Identity::key(provider, subject);
            let known = {
                let t = tx.tx.open_table(IDENTITIES)?;
                t.get(key.as_str())?.is_some()
            };
            if known {
                return Ok(None);
            }
            let Some(mut u) = tx.rows::<User>(USERS)?.into_iter().find(|u| {
                u.directory_id == directory
                    && u.pending_login
                        .as_ref()
                        .is_some_and(|p| p.is(connector, verified_email))
            }) else {
                return Ok(None);
            };
            u.pending_login = None;
            tx.put_user(&u)?;
            tx.put(
                IDENTITIES,
                &key,
                &Identity {
                    provider: provider.to_string(),
                    subject: subject.to_string(),
                    user_id: u.id.clone(),
                    email: Some(verified_email.to_string()),
                    created_at: now(),
                },
            )?;
            Ok::<_, StoreError>(Some(u))
        })
    }

    /// The managed users and groups of `connector`, read outside a write.
    pub fn scim_read(&self, connector: &str) -> Result<(Vec<User>, Vec<ScimGroup>), StoreError> {
        let users = self
            .users()?
            .into_iter()
            .filter(|u| u.managed.as_ref().is_some_and(|m| m.connector == connector))
            .collect();
        let groups = list::<ScimGroup>(&self.db, SCIM_GROUPS)?
            .into_iter()
            .map(|(_, g)| g)
            .filter(|g| g.connector == connector)
            .collect();
        Ok((users, groups))
    }

    /// Delete user `id` and what is its alone, in one write (spec/19-tenancy.md
    /// §6.3): its identities, sessions, personal tokens, team memberships,
    /// role links and tenant memberships. Service-account tokens it made
    /// stay (they are their project's); audit entries and taken invitations
    /// stay as history. `None` when there is no such user.
    pub fn delete_user(&self, id: &str) -> Result<Option<DeletedUser>, StoreError> {
        const TENANT_MEMBERS: TableDefinition<&str, &[u8]> = TableDefinition::new("tenant_members");
        let me = crate::authz::Ent::User(id.to_string());
        self.db.write(crate::store::Origin::Auth, |tx| {
            let mut out = DeletedUser::default();
            {
                let mut users = tx.open_table(USERS)?;
                if users.get(id)?.is_none() {
                    return Ok(None);
                }
                users.remove(id)?;
            }
            // Every row of `table` whose record `drop` says to drop.
            fn sweep<T: DeserializeOwned>(
                t: &mut crate::store::RecTable<'_>,
                drop: impl Fn(&str, &T) -> bool,
            ) -> Result<Vec<(String, T)>, StoreError> {
                let mut gone = Vec::new();
                for r in t.iter()? {
                    let (k, v) = r?;
                    if let Ok(x) = serde_json::from_slice::<T>(v.value())
                        && drop(k.value(), &x)
                    {
                        gone.push((k.value().to_string(), x));
                    }
                }
                for (k, _) in &gone {
                    t.remove(k.as_str())?;
                }
                Ok(gone)
            }
            {
                let mut t = tx.open_table(IDENTITIES)?;
                out.identities = sweep::<Identity>(&mut t, |_, i| i.user_id == id)?.len();
            }
            {
                let mut t = tx.open_table(SESSIONS)?;
                out.sessions = sweep::<Session>(&mut t, |_, s| s.user_id == id)?.len();
            }
            {
                let mut t = tx.open_table(TOKENS)?;
                out.tokens = sweep::<Token>(
                    &mut t,
                    |_, k| matches!(&k.kind, TokenKind::Personal { owner } if owner == id),
                )?
                .len();
            }
            {
                let mut t = tx.open_table(LINKS)?;
                out.links = sweep::<LinkRecord>(&mut t, |_, l| l.link.principal == me)?.len();
            }
            {
                let mut t = tx.open_table(TEAMS)?;
                let mut changed = Vec::new();
                for r in t.iter()? {
                    let (k, v) = r?;
                    if let Ok(mut team) = serde_json::from_slice::<Team>(v.value()) {
                        let before = team.members.len();
                        team.members.retain(|m| m.user_id != id);
                        if team.members.len() != before {
                            changed.push((k.value().to_string(), team));
                        }
                    }
                }
                out.teams = changed.len();
                for (k, team) in changed {
                    t.insert(k.as_str(), serde_json::to_vec(&team)?.as_slice())?;
                }
            }
            {
                let mut t = tx.open_table(TENANT_MEMBERS)?;
                let suffix = format!("/User:{id}");
                let gone: Vec<String> = t
                    .iter()?
                    .filter_map(|r| r.ok())
                    .map(|(k, _)| k.value().to_string())
                    .filter(|k| k.ends_with(&suffix))
                    .collect();
                out.tenants = gone.len();
                for k in gone {
                    t.remove(k.as_str())?;
                }
            }
            Ok(Some(out))
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
        self.user_for_identity_in(
            &local_directory(),
            provider,
            subject,
            display_name,
            email,
            create,
        )
    }

    /// As `user_for_identity`, a new user being made in `directory` (D58).
    pub fn user_for_identity_in(
        &self,
        directory: &str,
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
                    directory_id: directory.to_string(),
                    display_name: display_name.to_string(),
                    disabled: false,
                    default_project: None,
                    created_at: now(),
                    managed: None,
                    email: None,
                    disabled_reason: None,
                    pending_login: None,
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
    pub fn sync_memberships(
        &self,
        user_id: &str,
        source: MemberSource,
        wanted: &[String],
    ) -> Result<(), StoreError> {
        for mut t in self.teams()? {
            let want = wanted.contains(&t.name);
            let has = t
                .members
                .iter()
                .any(|m| m.user_id == user_id && m.source == source);
            let manual = t
                .members
                .iter()
                .any(|m| m.user_id == user_id && m.source == MemberSource::Manual);
            if want && !has && !manual {
                t.members.push(TeamMember {
                    user_id: user_id.to_string(),
                    source: source.clone(),
                });
                self.put_team(&t)?;
            } else if !want && has {
                t.members
                    .retain(|m| !(m.user_id == user_id && m.source == source));
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
        Ok(self
            .links()?
            .into_iter()
            .filter(|l| l.link.principal == *e || l.link.resource == *e)
            .collect())
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
        let mut v: Vec<SitePolicy> = list(&self.db, SITE_POLICIES)?
            .into_iter()
            .map(|(_, p)| p)
            .collect();
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
                None => (
                    current.as_ref().map(|p| p.text.clone()).unwrap_or_default(),
                    false,
                    true,
                ),
            };
            let entry = SitePolicyVersion {
                id: id.to_string(),
                version,
                text,
                enabled,
                author: author.to_string(),
                time: ts,
                deleted,
            };
            h.insert(
                format!("{}@{:012}", id, version).as_str(),
                serde_json::to_vec(&entry)?.as_slice(),
            )?;
            // Keep the newest `policy_history` versions.
            let keep = self
                .policy_history
                .load(std::sync::atomic::Ordering::Relaxed);
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

    fn audit_key(&self, e: &AuditEntry) -> String {
        let seq = self
            .audit_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{:016}-{:08}", e.time, seq % 100_000_000)
    }

    pub fn append_audit(&self, e: &AuditEntry) -> Result<(), StoreError> {
        put(&self.db, AUDIT, &self.audit_key(e), e)
    }

    /// An entry of hosted directory `dir`'s own audit (D74).
    pub fn append_directory_audit(&self, dir: &str, e: &AuditEntry) -> Result<(), StoreError> {
        put(
            &self.db,
            DIRECTORY_AUDIT,
            &format!("{dir}/{}", self.audit_key(e)),
            e,
        )
    }

    /// Entries at or after `since` (unix millis), oldest first, at most `limit`.
    pub fn audit(
        &self,
        since: u64,
        limit: usize,
        filter: impl Fn(&AuditEntry) -> bool,
    ) -> Result<Vec<AuditEntry>, StoreError> {
        self.audit_in(AUDIT, "", since, limit, filter)
    }

    /// The same, of hosted directory `dir`'s own audit (D74).
    pub fn directory_audit(
        &self,
        dir: &str,
        since: u64,
        limit: usize,
        filter: impl Fn(&AuditEntry) -> bool,
    ) -> Result<Vec<AuditEntry>, StoreError> {
        self.audit_in(DIRECTORY_AUDIT, &format!("{dir}/"), since, limit, filter)
    }

    fn audit_in(
        &self,
        table: crate::store::Def,
        prefix: &str,
        since: u64,
        limit: usize,
        filter: impl Fn(&AuditEntry) -> bool,
    ) -> Result<Vec<AuditEntry>, StoreError> {
        let txn = self.db.begin_read()?;
        let t = match txn.open_table(table) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let start = format!("{prefix}{:016}", since);
        let mut out = Vec::new();
        for e in t.range(start.as_str()..)? {
            let (k, v) = e?;
            if !k.value().starts_with(prefix) {
                break;
            }
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

    /// Remove hosted directory `dir`'s audit (the directory is deleted).
    pub fn remove_directory_audit(&self, dir: &str) -> Result<usize, StoreError> {
        let prefix = format!("{dir}/");
        self.remove_audit_where(DIRECTORY_AUDIT, |k| k.starts_with(&prefix))
    }

    /// Remove the entries of `table` whose keys `pick` chooses.
    fn remove_audit_where(
        &self,
        table: crate::store::Def,
        pick: impl Fn(&str) -> bool,
    ) -> Result<usize, StoreError> {
        let txn = self.db.begin(crate::store::Origin::Auth)?;
        let n = {
            let mut t = txn.open_table(table)?;
            let keys: Vec<String> = t
                .iter()?
                .filter_map(|e| e.ok())
                .map(|(k, _)| k.value().to_string())
                .filter(|k| pick(k))
                .collect();
            for k in &keys {
                t.remove(k.as_str())?;
            }
            keys.len()
        };
        txn.commit()?;
        Ok(n)
    }

    /// Delete entries older than `before` (unix millis), the hosted
    /// directories' too.
    pub fn prune_audit(&self, before: u64) -> Result<usize, StoreError> {
        let end = format!("{:016}", before);
        let dirs = self.remove_audit_where(DIRECTORY_AUDIT, |k| {
            k.split_once('/').is_some_and(|(_, t)| t < end.as_str())
        })?;
        Ok(dirs + self.prune_cluster_audit(before)?)
    }

    fn prune_cluster_audit(&self, before: u64) -> Result<usize, StoreError> {
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

/// [`IdentityStore::deprovision`] inside transaction `tx`.
fn deprovision_in(
    tx: &crate::store::Tx<'_>,
    id: &str,
    reason: DisabledReason,
) -> Result<Option<(User, Deprovisioned)>, StoreError> {
    let mut out = Deprovisioned::default();
    let user = {
        let mut users = tx.open_table(USERS)?;
        let Some(mut u): Option<User> = users
            .get(id)?
            .map(|v| serde_json::from_slice(v.value()))
            .transpose()?
        else {
            return Ok(None);
        };
        if !(u.disabled && u.disabled_reason == Some(DisabledReason::Admin)) {
            u.disabled_reason = Some(reason);
        }
        u.disabled = true;
        users.insert(id, serde_json::to_vec(&u)?.as_slice())?;
        u
    };
    let drop_rows = |t: &mut crate::store::RecTable<'_>,
                     drop: &dyn Fn(&[u8]) -> bool|
     -> Result<usize, StoreError> {
        let gone: Vec<String> = t
            .iter()?
            .filter_map(|r| r.ok())
            .filter(|(_, v)| drop(v.value()))
            .map(|(k, _)| k.value().to_string())
            .collect();
        for k in &gone {
            t.remove(k.as_str())?;
        }
        Ok(gone.len())
    };
    {
        let mut t = tx.open_table(SESSIONS)?;
        out.sessions = drop_rows(&mut t, &|v| {
            serde_json::from_slice::<Session>(v).is_ok_and(|s| s.user_id == id)
        })?;
    }
    {
        let mut t = tx.open_table(TOKENS)?;
        out.tokens = drop_rows(&mut t, &|v| {
            serde_json::from_slice::<Token>(v)
                .is_ok_and(|k| matches!(&k.kind, TokenKind::Personal { owner } if owner == id))
        })?;
    }
    if let Some(m) = &user.managed {
        let source = MemberSource::Connector(m.connector.clone());
        let mut t = tx.open_table(TEAMS)?;
        let mut changed = Vec::new();
        for r in t.iter()? {
            let (k, v) = r?;
            if let Ok(mut team) = serde_json::from_slice::<Team>(v.value()) {
                let before = team.members.len();
                team.members
                    .retain(|x| !(x.user_id == id && x.source == source));
                if team.members.len() != before {
                    changed.push((k.value().to_string(), team));
                }
            }
        }
        out.teams = changed.len();
        for (k, team) in changed {
            t.insert(k.as_str(), serde_json::to_vec(&team)?.as_slice())?;
        }
    }
    Ok(Some((user, out)))
}

/// A SCIM request's view of the store, inside its one write
/// ([`IdentityStore::scim`]).
pub struct ScimTx<'a> {
    tx: &'a crate::store::Tx<'a>,
    pub connector: String,
    /// The connector's directory: where the users and teams it makes go.
    pub directory: String,
}

impl ScimTx<'_> {
    fn rows<T: DeserializeOwned>(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
    ) -> Result<Vec<T>, StoreError> {
        let t = self.tx.open_table(def)?;
        let mut out = Vec::new();
        for r in t.iter()? {
            let (_, v) = r?;
            if let Ok(x) = serde_json::from_slice(v.value()) {
                out.push(x);
            }
        }
        Ok(out)
    }

    fn put<T: Serialize>(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
        key: &str,
        v: &T,
    ) -> Result<(), StoreError> {
        let mut t = self.tx.open_table(def)?;
        t.insert(key, serde_json::to_vec(v)?.as_slice())?;
        Ok(())
    }

    fn mine(&self, m: &Option<Managed>) -> bool {
        m.as_ref().is_some_and(|m| m.connector == self.connector)
    }

    pub fn user(&self, id: &str) -> Result<Option<User>, StoreError> {
        let t = self.tx.open_table(USERS)?;
        Ok(match t.get(id)? {
            Some(v) => Some(serde_json::from_slice(v.value())?),
            None => None,
        })
    }

    /// The users this connector manages, oldest first.
    pub fn users(&self) -> Result<Vec<User>, StoreError> {
        let mut v: Vec<User> = self
            .rows::<User>(USERS)?
            .into_iter()
            .filter(|u| self.mine(&u.managed))
            .collect();
        v.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(v)
    }

    pub fn put_user(&self, u: &User) -> Result<(), StoreError> {
        self.put(USERS, &u.id, u)
    }

    /// Users that aren't managed but log in through this connector with
    /// `email`: a connector that had JIT before SCIM (adopted at `POST`).
    pub fn unmanaged_with_email(
        &self,
        provider: &str,
        email: &str,
    ) -> Result<Vec<User>, StoreError> {
        let ids: Vec<String> = self
            .rows::<Identity>(IDENTITIES)?
            .into_iter()
            .filter(|i| {
                i.provider == provider
                    && i.email
                        .as_deref()
                        .is_some_and(|e| e.eq_ignore_ascii_case(email))
            })
            .map(|i| i.user_id)
            .collect();
        let mut out = Vec::new();
        for id in ids {
            if let Some(u) = self.user(&id)?
                && u.managed.is_none()
                && !out.iter().any(|x: &User| x.id == u.id)
            {
                out.push(u);
            }
        }
        Ok(out)
    }

    /// D20, in this write.
    pub fn deprovision(
        &self,
        id: &str,
        reason: DisabledReason,
    ) -> Result<Option<(User, Deprovisioned)>, StoreError> {
        deprovision_in(self.tx, id, reason)
    }

    pub fn groups(&self) -> Result<Vec<ScimGroup>, StoreError> {
        let mut v: Vec<ScimGroup> = self
            .rows::<ScimGroup>(SCIM_GROUPS)?
            .into_iter()
            .filter(|g| g.connector == self.connector)
            .collect();
        v.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(v)
    }

    pub fn put_group(&self, g: &ScimGroup) -> Result<(), StoreError> {
        self.put(SCIM_GROUPS, &g.id, g)
    }

    pub fn remove_group(&self, id: &str) -> Result<(), StoreError> {
        let mut t = self.tx.open_table(SCIM_GROUPS)?;
        t.remove(id)?;
        Ok(())
    }

    /// A user leaves every group of this connector (it is removed).
    pub fn leave_groups(&self, user: &str) -> Result<(), StoreError> {
        for mut g in self.groups()? {
            let n = g.members.len();
            g.members.retain(|m| m != user);
            if g.members.len() != n {
                self.put_group(&g)?;
            }
        }
        Ok(())
    }

    /// Make the teams match the groups (D43): with `auto`, each group is
    /// the managed team of the same id (made, renamed, or detached when the
    /// group goes); with `mapped`, `group_teams` names the teams. Members
    /// of a group who are enabled and managed by this connector are the
    /// team's `connector:<id>` members, unless they are manual members
    /// already; nobody else is.
    pub fn sync_teams(
        &self,
        auto: bool,
        group_teams: &std::collections::BTreeMap<String, String>,
    ) -> Result<(), StoreError> {
        use std::collections::{BTreeMap, BTreeSet};
        let source = MemberSource::Connector(self.connector.clone());
        let enabled: BTreeSet<String> = self
            .users()?
            .into_iter()
            .filter(|u| !u.disabled)
            .map(|u| u.id)
            .collect();
        let groups = self.groups()?;
        let mut teams: Vec<Team> = self.rows::<Team>(TEAMS)?;
        // Team id → who the groups put in it.
        let mut want: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let members = |g: &ScimGroup| -> BTreeSet<String> {
            g.members
                .iter()
                .filter(|m| enabled.contains(*m))
                .cloned()
                .collect()
        };
        if auto {
            for g in &groups {
                let taken = |name: &str, teams: &[Team]| {
                    teams.iter().any(|t| t.id != g.id && t.name == name)
                };
                let mut name: String = g.display_name.chars().take(64).collect();
                if taken(&name, &teams) {
                    let suffix = format!(" ({})", self.connector);
                    let keep = 64usize.saturating_sub(suffix.chars().count());
                    name = g.display_name.chars().take(keep).collect::<String>() + &suffix;
                }
                match teams.iter_mut().find(|t| t.id == g.id) {
                    Some(t) => {
                        if self.mine(&t.managed) && t.name != name {
                            t.name = name;
                        }
                        if let Some(m) =
                            t.managed.as_mut().filter(|m| m.connector == self.connector)
                        {
                            m.external_id = g.external_id.clone();
                        }
                    }
                    None => teams.push(Team {
                        id: g.id.clone(),
                        directory_id: self.directory.clone(),
                        name,
                        members: vec![],
                        created_at: now(),
                        managed: Some(Managed {
                            connector: self.connector.clone(),
                            external_id: g.external_id.clone(),
                            user_name: None,
                            given_name: None,
                            family_name: None,
                            ssh_keys: vec![],
                        }),
                    }),
                }
                want.entry(g.id.clone()).or_default().extend(members(g));
            }
            // A group that went leaves its team behind, unmanaged and empty
            // of what the IdP gave: its role links give nobody anything.
            for t in teams.iter_mut() {
                if self.mine(&t.managed) && !groups.iter().any(|g| g.id == t.id) {
                    t.managed = None;
                }
            }
        } else {
            for g in &groups {
                let named = [Some(&g.display_name), Some(&g.id), g.external_id.as_ref()]
                    .into_iter()
                    .flatten()
                    .find_map(|k| group_teams.get(k));
                if let Some(name) = named
                    && let Some(t) = teams.iter().find(|t| &t.name == name)
                {
                    want.entry(t.id.clone()).or_default().extend(members(g));
                }
            }
        }
        let before: Vec<Team> = self.rows::<Team>(TEAMS)?;
        for t in teams.iter_mut() {
            let wanted = want.remove(&t.id).unwrap_or_default();
            let manual: BTreeSet<String> = t
                .members
                .iter()
                .filter(|m| m.source != source)
                .map(|m| m.user_id.clone())
                .collect();
            t.members
                .retain(|m| m.source != source || wanted.contains(&m.user_id));
            for u in wanted {
                let has = t
                    .members
                    .iter()
                    .any(|m| m.user_id == u && m.source == source);
                if !has && !manual.contains(&u) {
                    t.members.push(TeamMember {
                        user_id: u,
                        source: source.clone(),
                    });
                }
            }
            if before.iter().find(|b| b.id == t.id) != Some(t) {
                self.put(TEAMS, &t.id.clone(), t)?;
            }
        }
        Ok(())
    }

    /// `user` becomes a member of `tenant` (its connector's
    /// `default_tenant`), in this write.
    pub fn join_tenant(&self, tenant: &str, user: &str) -> Result<(), StoreError> {
        crate::tenants::add_member_in(
            self.tx,
            tenant,
            Ent::User(user.to_string()),
            &format!("connector:{}", self.connector),
        )
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
        let alice = s
            .user_for_identity("unix", "alice", "alice", None, true)
            .unwrap()
            .unwrap();
        let bob = s
            .user_for_identity("pam", "bob", "bob", None, true)
            .unwrap()
            .unwrap();
        let sso = s
            .user_for_identity("oidc:https://idp", "carol", "carol", None, true)
            .unwrap()
            .unwrap();
        assert_eq!(s.scope_local_identities("n1").unwrap(), 2);
        assert!(s.identity("unix", "alice").unwrap().is_none());
        assert_eq!(
            s.identity("unix", "alice@n1").unwrap().unwrap().user_id,
            alice.id
        );
        assert_eq!(
            s.identity("pam", "bob@n1").unwrap().unwrap().user_id,
            bob.id
        );
        // OIDC identities are global and untouched.
        assert_eq!(
            s.identity("oidc:https://idp", "carol")
                .unwrap()
                .unwrap()
                .user_id,
            sso.id
        );
        // Idempotent.
        assert_eq!(s.scope_local_identities("n1").unwrap(), 0);
        // Another node's identity appears (a second host's alice).
        let other = Identity {
            provider: "unix".into(),
            subject: "dave@n2".into(),
            user_id: alice.id.clone(),
            email: None,
            created_at: 1,
        };
        s.put_identity(&other).unwrap();
        // Leaving with n1's resources: n1's names are plain again, n2's are dropped.
        assert_eq!(s.unscope_local_identities("n1").unwrap(), 3);
        assert_eq!(
            s.identity("unix", "alice").unwrap().unwrap().user_id,
            alice.id
        );
        assert_eq!(s.identity("pam", "bob").unwrap().unwrap().user_id, bob.id);
        assert!(s.identity("unix", "dave@n2").unwrap().is_none());
    }

    #[test]
    fn pam_and_unix_identities_share_a_user() {
        let (s, _d) = store();
        let a = s
            .user_for_identity("unix", "alice", "alice", None, true)
            .unwrap()
            .unwrap();
        let b = s
            .user_for_identity("pam", "alice", "alice", None, true)
            .unwrap()
            .unwrap();
        assert_eq!(a.id, b.id);
        let o = s
            .user_for_identity("oidc:https://idp", "alice", "Alice", None, true)
            .unwrap()
            .unwrap();
        assert_ne!(o.id, a.id, "never linked by name or email across providers");
        assert!(
            s.user_for_identity("pam", "bob", "bob", None, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn membership_sync_keeps_manual() {
        let (s, _d) = store();
        for name in ["a", "b", "c"] {
            s.put_team(&Team {
                id: name.into(),
                directory_id: local_directory(),
                name: name.into(),
                members: vec![],
                created_at: 0,
                managed: None,
            })
            .unwrap();
        }
        let mut c = s.team("c").unwrap().unwrap();
        c.members.push(TeamMember {
            user_id: "u".into(),
            source: MemberSource::Manual,
        });
        s.put_team(&c).unwrap();
        s.sync_memberships("u", MemberSource::Oidc, &["a".into(), "b".into()])
            .unwrap();
        assert_eq!(s.teams_of("u").unwrap(), vec!["a", "b", "c"]);
        s.sync_memberships("u", MemberSource::Oidc, &["b".into()])
            .unwrap();
        assert_eq!(s.teams_of("u").unwrap(), vec!["b", "c"]);
        s.sync_memberships("u", MemberSource::Oidc, &[]).unwrap();
        assert_eq!(s.teams_of("u").unwrap(), vec!["c"]);
    }

    #[test]
    fn site_policy_versions_and_conflicts() {
        let (s, _d) = store();
        assert!(matches!(
            s.write_site_policy("site.a", 1, Some(("x", "", true)), "u"),
            Err(StoreError::Conflict(_))
        ));
        let p = s
            .write_site_policy("site.a", 0, Some(("x", "d", true)), "u")
            .unwrap()
            .unwrap();
        assert_eq!(p.version, 1);
        assert!(matches!(
            s.write_site_policy("site.a", 0, Some(("y", "", true)), "u"),
            Err(StoreError::Conflict(_))
        ));
        s.write_site_policy("site.a", 1, Some(("y", "", false)), "u")
            .unwrap();
        s.write_site_policy("site.a", 2, None, "u").unwrap();
        assert!(s.site_policy("site.a").unwrap().is_none());
        let v = s.site_policy_versions("site.a").unwrap();
        assert_eq!(
            v.iter().map(|v| v.version).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(v[2].deleted);
        for i in 0..60 {
            s.write_site_policy("site.b", i, Some(("t", "", true)), "u")
                .unwrap();
        }
        assert_eq!(
            s.site_policy_versions("site.b").unwrap().len(),
            POLICY_HISTORY
        );
        s.set_policy_history(5);
        s.write_site_policy("site.b", 60, Some(("t", "", true)), "u")
            .unwrap();
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

    #[test]
    fn a_scim_first_login_finds_only_its_connectors_users() {
        let user = |id: &str, connector: Option<&str>, ext: &str| User {
            id: id.into(),
            directory_id: local_directory(),
            display_name: id.into(),
            disabled: false,
            default_project: None,
            created_at: 1,
            managed: connector.map(|c| Managed {
                connector: c.into(),
                external_id: Some(ext.into()),
                user_name: Some("alice@example.org".into()),
                given_name: None,
                family_name: None,
                ssh_keys: vec![],
            }),
            email: Some("alice@example.org".into()),
            disabled_reason: None,
            pending_login: None,
        };
        let login = |s: &IdentityStore, c: &str| {
            s.scim_login(c, "oidc:c", "sub-1", Some("alice@example.org"))
        };
        // A local account, then another connector's managed user, with the
        // same email (and subject): each is the only candidate, and neither
        // is c1's, so a login through c1 finds no one and links nothing.
        for foreign in [user("local", None, ""), user("other", Some("c2"), "sub-1")] {
            let (s, _d) = store();
            s.put_user(&foreign).unwrap();
            assert_eq!(login(&s, "c1").unwrap(), None, "{}", foreign.id);
            assert!(s.identity("oidc:c", "sub-1").unwrap().is_none());
        }

        // c1's own user is found among them, and linked.
        let (s, _d) = store();
        for u in [
            user("local", None, ""),
            user("other", Some("c2"), "sub-1"),
            user("mine", Some("c1"), "sub-1"),
        ] {
            s.put_user(&u).unwrap();
        }
        assert_eq!(login(&s, "c1").unwrap().unwrap().id, "mine");
        assert_eq!(
            s.identity("oidc:c", "sub-1").unwrap().unwrap().user_id,
            "mine"
        );
    }
}
