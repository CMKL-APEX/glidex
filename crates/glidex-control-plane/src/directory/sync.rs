//! Pull sync (spec/19-tenancy.md §8.2, D44–D46): a run reads a connector's
//! users and groups from its IdP's directory (a [`Source`]), plans what
//! changes, stops when it would disable too many, and applies the plan
//! through the same writes SCIM uses ([`ScimTx`]), at most
//! [`CHUNK`] changes per write. The task that runs it on the leader is
//! [`spawn`]; the status of each connector's last run is a
//! [`SyncStatus`] in `connector_sync`.

use crate::auth::AuthService;
use crate::auth::store::{DisabledReason, ScimGroup, ScimTx, User};
use crate::directory::scim::UserAttrs;
use crate::directory::{Connector, GroupMode, Provisioning};
use crate::store::Db;
use redb::TableDefinition;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex};

const CONNECTOR_SYNC: TableDefinition<&str, &[u8]> = TableDefinition::new("connector_sync");
/// Most changes in one write (§8.2).
pub const CHUNK: usize = 500;

/// A user as the IdP's directory has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUser {
    /// The IdP's id: for Google, the ID token's `sub` (D44).
    pub id: String,
    pub email: String,
    pub display_name: Option<String>,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
    /// Not suspended or archived (D45).
    pub active: bool,
    pub ssh_keys: Vec<String>,
}

/// A group the connector names, with its members (source user ids,
/// nested groups' members included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceGroup {
    pub id: String,
    pub email: String,
    pub name: String,
    pub members: Vec<String>,
}

/// Everything one run read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub users: Vec<SourceUser>,
    pub groups: Vec<SourceGroup>,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The IdP said no, or answered what can't be read: nothing changes
    /// (D46). The message has no secrets.
    #[error("{0}")]
    Source(String),
    #[error("sync storage error: {0}")]
    Storage(String),
}

impl From<crate::auth::store::StoreError> for SyncError {
    fn from(e: crate::auth::store::StoreError) -> Self {
        SyncError::Storage(e.to_string())
    }
}

impl From<crate::store::StoreError> for SyncError {
    fn from(e: crate::store::StoreError) -> Self {
        SyncError::Storage(e.to_string())
    }
}

/// Where a pull reads from.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    async fn read(&self, auth: &AuthService, c: &Connector) -> Result<Snapshot, SyncError>;
}

// ---- status -------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncState {
    #[default]
    Ok,
    Failed,
    Blocked,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub created: usize,
    pub adopted: usize,
    pub updated: usize,
    pub disabled: usize,
    pub enabled: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    pub connector: String,
    pub state: SyncState,
    pub started_at: u64,
    pub finished_at: u64,
    #[serde(default)]
    pub changes: Changes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Blocked: the users it would disable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub would_disable: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_applied_at: Option<u64>,
}

/// Each pulled connector's last run.
pub struct SyncStatusStore {
    db: Arc<Db>,
}

impl SyncStatusStore {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    pub fn get(&self, connector: &str) -> Result<Option<SyncStatus>, SyncError> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| SyncError::Storage(e.to_string()))?;
        let table = match txn.open_table(CONNECTOR_SYNC) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(SyncError::Storage(e.to_string())),
        };
        let v = table
            .get(connector)
            .map_err(|e| SyncError::Storage(e.to_string()))?;
        Ok(match v {
            Some(v) => serde_json::from_slice(v.value()).ok(),
            None => None,
        })
    }

    fn put(&self, s: &SyncStatus) -> Result<(), SyncError> {
        self.db
            .write(crate::store::Origin::Api, |tx| {
                let mut t = tx.open_table(CONNECTOR_SYNC)?;
                t.insert(
                    s.connector.as_str(),
                    serde_json::to_vec(s)
                        .map_err(|e| {
                            crate::store::StoreError::Io(std::io::Error::other(e.to_string()))
                        })?
                        .as_slice(),
                )?;
                Ok::<_, crate::store::StoreError>(())
            })
            .map_err(|e| SyncError::Storage(e.to_string()))
    }

    pub fn remove(&self, connector: &str) -> Result<(), SyncError> {
        self.db
            .write(crate::store::Origin::Api, |tx| {
                tx.open_table(CONNECTOR_SYNC)?.remove(connector)?;
                Ok::<_, crate::store::StoreError>(())
            })
            .map_err(|e| SyncError::Storage(e.to_string()))
    }
}

// ---- the plan -------------------------------------------------------------

/// One change to one user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// A new managed user, with the id it gets.
    Create(String, SourceUser),
    /// A user from before the pull with the same subject (D44).
    Adopt(String, SourceUser),
    Update(String, SourceUser),
    /// D45: out of the groups, suspended, archived or gone.
    Disable(String),
    Enable(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub ops: Vec<Op>,
    /// The connector's groups as they will be, members by glidex id.
    pub groups: Vec<ScimGroup>,
}

impl Plan {
    pub fn would_disable(&self) -> Vec<String> {
        self.ops
            .iter()
            .filter_map(|o| match o {
                Op::Disable(id) => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    fn changes(&self) -> Changes {
        let mut c = Changes::default();
        for o in &self.ops {
            match o {
                Op::Create(..) => c.created += 1,
                Op::Adopt(..) => c.adopted += 1,
                Op::Update(..) => c.updated += 1,
                Op::Disable(_) => c.disabled += 1,
                Op::Enable(_) => c.enabled += 1,
            }
        }
        c
    }
}

fn attrs(u: &SourceUser) -> UserAttrs {
    UserAttrs {
        user_name: u.email.clone(),
        external_id: Some(u.id.clone()),
        display_name: u.display_name.clone(),
        given_name: u.given_name.clone(),
        family_name: u.family_name.clone(),
        formatted: None,
        email: Some(u.email.clone()),
        active: u.active,
    }
}

/// What the connector holds now.
pub struct Current {
    /// Its managed users.
    pub managed: Vec<User>,
    /// Unmanaged users with an identity of the connector: subject → user.
    pub logged_in: BTreeMap<String, User>,
    pub groups: Vec<ScimGroup>,
}

/// Plan a run: who is in (members of the snapshot's groups), what changes
/// for each, and the groups as they will be. `new_id` names new users.
pub fn plan(
    connector: &str,
    snap: &Snapshot,
    cur: &Current,
    new_id: &mut dyn FnMut() -> String,
) -> Plan {
    let by_source: BTreeMap<&str, &SourceUser> =
        snap.users.iter().map(|u| (u.id.as_str(), u)).collect();
    let wanted: BTreeSet<&str> = snap
        .groups
        .iter()
        .flat_map(|g| g.members.iter().map(String::as_str))
        .filter(|id| by_source.contains_key(id))
        .collect();
    let mut ops = Vec::new();
    // source id → glidex id
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let managed: BTreeMap<&str, &User> = cur
        .managed
        .iter()
        .filter_map(|u| Some((u.managed.as_ref()?.external_id.as_deref()?, u)))
        .collect();
    for sid in &wanted {
        let su = by_source[sid];
        match managed.get(sid) {
            Some(u) => {
                ids.insert(su.id.clone(), u.id.clone());
                let now = UserAttrs::of(u);
                let next = attrs(su);
                if (
                    now.user_name.as_str(),
                    &now.email,
                    &now.given_name,
                    &now.family_name,
                ) != (
                    next.user_name.as_str(),
                    &next.email,
                    &next.given_name,
                    &next.family_name,
                ) || u.display_name != next.display()
                    || u.managed.as_ref().map(|m| &m.ssh_keys) != Some(&su.ssh_keys)
                {
                    ops.push(Op::Update(u.id.clone(), su.clone()));
                }
                if su.active && u.disabled && u.disabled_reason == Some(DisabledReason::Sync) {
                    ops.push(Op::Enable(u.id.clone()));
                } else if !su.active && !u.disabled {
                    ops.push(Op::Disable(u.id.clone()));
                }
            }
            None => match cur.logged_in.get(*sid) {
                Some(u) => {
                    ids.insert(su.id.clone(), u.id.clone());
                    ops.push(Op::Adopt(u.id.clone(), su.clone()));
                    if !su.active && !u.disabled {
                        ops.push(Op::Disable(u.id.clone()));
                    }
                }
                // Somebody suspended who never had an account doesn't get one.
                None if su.active => {
                    let id = new_id();
                    ids.insert(su.id.clone(), id.clone());
                    ops.push(Op::Create(id, su.clone()));
                }
                None => {}
            },
        }
    }
    for (sid, u) in &managed {
        if !wanted.contains(sid) && !u.disabled {
            ops.push(Op::Disable(u.id.clone()));
        }
    }
    let now = crate::auth::store::now();
    let groups = snap
        .groups
        .iter()
        .map(|g| {
            let before = cur.groups.iter().find(|x| x.id == g.id);
            let members = g
                .members
                .iter()
                .filter_map(|m| ids.get(m).cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            ScimGroup {
                id: g.id.clone(),
                connector: connector.to_string(),
                display_name: g.name.clone(),
                external_id: Some(g.email.clone()),
                members,
                created_at: before.map(|b| b.created_at).unwrap_or(now),
                updated_at: now,
            }
        })
        .collect();
    Plan { ops, groups }
}

/// The SSH keys the directory holds for `su` (T4 task 5).
fn set_keys(u: &mut User, su: &SourceUser) {
    if let Some(m) = u.managed.as_mut() {
        m.ssh_keys = su.ssh_keys.clone();
    }
}

fn apply_op(tx: &ScimTx<'_>, c: &Connector, op: &Op) -> Result<(), SyncError> {
    match op {
        Op::Create(id, su) => {
            let mut u = User {
                id: id.clone(),
                directory_id: c.directory_id.clone(),
                display_name: String::new(),
                disabled: false,
                default_project: None,
                created_at: crate::auth::store::now(),
                managed: None,
                email: None,
                disabled_reason: None,
                pending_login: None,
            };
            attrs(su).apply(&mut u, &c.id);
            set_keys(&mut u, su);
            tx.put_user(&u)?;
            if let Some(t) = &c.default_tenant {
                tx.join_tenant(t, id)?;
            }
        }
        Op::Adopt(id, su) | Op::Update(id, su) => {
            if let Some(mut u) = tx.user(id)? {
                attrs(su).apply(&mut u, &c.id);
                set_keys(&mut u, su);
                tx.put_user(&u)?;
                if matches!(op, Op::Adopt(..))
                    && let Some(t) = &c.default_tenant
                {
                    tx.join_tenant(t, id)?;
                }
            }
        }
        Op::Disable(id) => {
            tx.deprovision(id, DisabledReason::Sync)?;
        }
        Op::Enable(id) => {
            if let Some(mut u) = tx.user(id)?
                && u.disabled
                && u.disabled_reason == Some(DisabledReason::Sync)
            {
                u.disabled = false;
                u.disabled_reason = None;
                tx.put_user(&u)?;
            }
        }
    }
    Ok(())
}

// ---- runs -----------------------------------------------------------------

fn running() -> &'static Mutex<HashSet<String>> {
    static R: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Read what the connector holds now.
fn current(auth: &AuthService, c: &Connector) -> Result<Current, SyncError> {
    let (managed, groups) = auth.store.scim_read(&c.id)?;
    let provider = c.identity_provider();
    let mut logged_in = BTreeMap::new();
    for i in auth.store.identities_of_provider(&provider)? {
        if let Some(u) = auth.store.user(&i.user_id)?
            && u.managed.is_none()
        {
            logged_in.insert(i.subject, u);
        }
    }
    Ok(Current {
        managed,
        logged_in,
        groups,
    })
}

/// One run of connector `c` from `source`. With `confirm`, a run the
/// safety stop would block applies (D46). The status is recorded and
/// returned; another run of the same connector already going is `None`.
pub async fn run(
    auth: &AuthService,
    c: &Connector,
    source: &dyn Source,
    confirm: bool,
    request_id: &str,
) -> Option<SyncStatus> {
    if !running().lock().unwrap().insert(c.id.clone()) {
        return None;
    }
    let status = run_once(auth, c, source, confirm).await;
    running().lock().unwrap().remove(&c.id);
    let prev = auth.sync_status.get(&c.id).ok().flatten();
    let mut status = status;
    if status.state != SyncState::Ok {
        status.last_applied_at = prev.and_then(|p| p.last_applied_at);
    }
    if let Err(e) = auth.sync_status.put(&status) {
        tracing::error!("recording connector {}'s sync: {e}", c.id);
    }
    let who = format!("system:sync:{}", c.id);
    let (result, code) = match status.state {
        SyncState::Ok => ("ok", None),
        SyncState::Failed => ("error", Some("sync_failed")),
        SyncState::Blocked => ("denied", Some("sync_blocked")),
    };
    // A run that changed nothing isn't worth an audit entry.
    let c2 = status.changes;
    if status.state != SyncState::Ok || c2 != Changes::default() {
        auth.audit_system(
            &who,
            request_id,
            "syncConnector",
            Some(&format!("connector:{}", c.id)),
            result,
            code,
            serde_json::json!({
                "changes": c2,
                "confirmed": confirm,
                "would_disable": status.would_disable,
                "error": status.error,
            }),
        );
    }
    Some(status)
}

async fn run_once(
    auth: &AuthService,
    c: &Connector,
    source: &dyn Source,
    confirm: bool,
) -> SyncStatus {
    let started_at = crate::auth::store::now();
    let done = |state, changes, error: Option<String>, would_disable| SyncStatus {
        connector: c.id.clone(),
        state,
        started_at,
        finished_at: crate::auth::store::now(),
        changes,
        error,
        would_disable,
        last_applied_at: None,
    };
    let failed = |e: SyncError| {
        done(
            SyncState::Failed,
            Changes::default(),
            Some(e.to_string()),
            vec![],
        )
    };
    let snap = match source.read(auth, c).await {
        Ok(s) => s,
        Err(e) => return failed(e),
    };
    let cur = match current(auth, c) {
        Ok(x) => x,
        Err(e) => return failed(e),
    };
    let p = plan(&c.id, &snap, &cur, &mut || uuid::Uuid::new_v4().to_string());
    let would = p.would_disable();
    let limit = c
        .sync
        .as_ref()
        .map(|s| s.disable_limit(cur.managed.len()))
        .unwrap_or(crate::directory::SYNC_DISABLE_FLOOR);
    if would.len() > limit && !confirm {
        return done(SyncState::Blocked, Changes::default(), None, would);
    }
    if let Err(e) = apply(auth, c, &p) {
        return failed(e);
    }
    let mut s = done(SyncState::Ok, p.changes(), None, vec![]);
    s.last_applied_at = Some(s.finished_at);
    s
}

/// Apply `p` in writes of at most [`CHUNK`] changes; the groups and teams
/// go last, in their own write.
pub fn apply(auth: &AuthService, c: &Connector, p: &Plan) -> Result<(), SyncError> {
    for chunk in p.ops.chunks(CHUNK) {
        auth.store.scim(&c.directory_id, &c.id, |tx| {
            for op in chunk {
                apply_op(tx, c, op)?;
            }
            Ok::<_, SyncError>(())
        })?;
    }
    auth.store.scim(&c.directory_id, &c.id, |tx| {
        let keep: BTreeSet<&str> = p.groups.iter().map(|g| g.id.as_str()).collect();
        for g in tx.groups()? {
            if !keep.contains(g.id.as_str()) {
                tx.remove_group(&g.id)?;
            }
        }
        for g in &p.groups {
            tx.put_group(g)?;
        }
        tx.sync_teams(c.group_mode == GroupMode::Auto, &c.group_teams)?;
        Ok::<_, SyncError>(())
    })
}

/// The source a connector's kind reads from; `None` before there is one.
pub fn source_for(auth: &AuthService, c: &Connector) -> Option<Arc<dyn Source>> {
    if let Some(s) = auth.sync_source_override() {
        return Some(s);
    }
    static GOOGLE: std::sync::OnceLock<Arc<crate::directory::google_directory::GoogleDirectory>> =
        std::sync::OnceLock::new();
    match c.kind {
        crate::directory::Kind::Google => {
            Some(GOOGLE.get_or_init(|| Arc::new(Default::default())).clone() as Arc<dyn Source>)
        }
        _ => None,
    }
}

/// Whether `c` is due for a run at `now`.
fn due(auth: &AuthService, c: &Connector, now: u64) -> bool {
    let every = c.sync.as_ref().map(|s| s.interval_minutes()).unwrap_or(15) as u64 * 60;
    match auth.sync_status.get(&c.id).ok().flatten() {
        Some(s) => now >= s.finished_at + every,
        None => true,
    }
}

/// The task that runs pulled connectors when they are due: on a standalone
/// host, or the cluster's leader (where writes go).
pub fn spawn(auth: Arc<AuthService>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            if !auth.tenants.can_write() || auth.is_disabled() {
                continue;
            }
            let Ok(connectors) = auth.connectors.list() else {
                continue;
            };
            let now = crate::auth::store::now();
            for c in connectors {
                if c.provisioning != Provisioning::Pull || !c.enabled || !due(&auth, &c, now) {
                    continue;
                }
                let Some(src) = source_for(&auth, &c) else {
                    continue;
                };
                let rid = format!("sync-{}", crate::auth::random_token(8));
                if let Some(s) = run(&auth, &c, src.as_ref(), false, &rid).await
                    && s.state != SyncState::Ok
                {
                    tracing::warn!(connector = %c.id, state = ?s.state, error = ?s.error, "pull sync did not apply");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::Managed;

    fn su(id: &str, active: bool) -> SourceUser {
        SourceUser {
            id: id.into(),
            email: format!("{id}@corp.example"),
            display_name: Some(id.to_uppercase()),
            given_name: None,
            family_name: None,
            active,
            ssh_keys: vec![],
        }
    }

    fn managed(id: &str, ext: &str, disabled: Option<DisabledReason>) -> User {
        User {
            id: id.into(),
            directory_id: crate::auth::store::local_directory(),
            display_name: ext.to_uppercase(),
            disabled: disabled.is_some(),
            default_project: None,
            created_at: 0,
            managed: Some(Managed {
                connector: "g".into(),
                external_id: Some(ext.into()),
                user_name: Some(format!("{ext}@corp.example")),
                given_name: None,
                family_name: None,
                ssh_keys: vec![],
            }),
            email: Some(format!("{ext}@corp.example")),
            disabled_reason: disabled,
            pending_login: None,
        }
    }

    #[test]
    fn a_plan_follows_the_groups_and_states() {
        let snap = Snapshot {
            users: vec![
                su("a", true),
                su("b", false),
                su("c", true),
                su("d", true),
                su("e", false),
                su("f", true),
            ],
            groups: vec![
                SourceGroup {
                    id: "g1".into(),
                    email: "eng@corp.example".into(),
                    name: "eng".into(),
                    members: vec!["a".into(), "b".into(), "c".into()],
                },
                SourceGroup {
                    id: "g2".into(),
                    email: "ops@corp.example".into(),
                    name: "ops".into(),
                    members: vec!["c".into(), "d".into(), "e".into(), "zz".into()],
                },
            ],
        };
        let mut jit = managed("u-d", "d", None);
        jit.managed = None;
        let cur = Current {
            managed: vec![
                managed("u-a", "a", Some(DisabledReason::Sync)),
                managed("u-b", "b", None),
                managed("u-c", "c", Some(DisabledReason::Admin)),
                managed("u-x", "x", None),
            ],
            logged_in: [("d".to_string(), jit)].into(),
            groups: vec![],
        };
        let mut n = 0;
        let p = plan("g", &snap, &cur, &mut || {
            n += 1;
            format!("new-{n}")
        });
        let has = |o: Op| p.ops.contains(&o);
        assert!(has(Op::Enable("u-a".into())), "back and active: {p:?}");
        assert!(has(Op::Disable("u-b".into())), "suspended");
        assert!(
            !p.ops
                .iter()
                .any(|o| matches!(o, Op::Enable(id) if id == "u-c")),
            "an administrator's stop holds"
        );
        assert!(
            has(Op::Adopt("u-d".into(), su("d", true))),
            "logged in before the pull"
        );
        assert!(has(Op::Disable("u-x".into())), "left the groups");
        assert!(
            !p.ops
                .iter()
                .any(|o| matches!(o, Op::Create(_, u) if u.id == "e")),
            "suspended and new: no account"
        );
        assert!(
            !p.ops
                .iter()
                .any(|o| matches!(o, Op::Create(_, u) if u.id == "f")),
            "in no group: no account"
        );
        assert_eq!(p.would_disable(), ["u-b", "u-x"]);
        // Members by glidex id; unknown members dropped.
        assert_eq!(p.groups[0].members, ["u-a", "u-b", "u-c"]);
        assert_eq!(p.groups[1].members, ["u-c", "u-d"]);
        assert_eq!(p.groups[1].external_id.as_deref(), Some("ops@corp.example"));
        // Nothing changes twice.
        let again = Current {
            managed: vec![managed("u-a", "a", None)],
            logged_in: BTreeMap::new(),
            groups: vec![],
        };
        let snap2 = Snapshot {
            users: vec![su("a", true)],
            groups: vec![SourceGroup {
                id: "g".into(),
                email: "x@corp.example".into(),
                name: "x".into(),
                members: vec!["a".into()],
            }],
        };
        let mut never = || unreachable!();
        assert!(plan("g", &snap2, &again, &mut never).ops.is_empty());
    }

    #[test]
    fn the_disable_limit_has_a_floor() {
        let s = crate::directory::SyncSettings::default();
        assert_eq!(s.disable_limit(10), 5);
        assert_eq!(s.disable_limit(1000), 100);
        let s = crate::directory::SyncSettings {
            max_disable: Some(50),
            ..Default::default()
        };
        assert_eq!(s.disable_limit(100), 50);
    }
}
