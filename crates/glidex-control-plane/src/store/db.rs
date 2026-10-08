//! The write path of the control-plane database (spec/clustering.md §6.1).
//!
//! Every change to `glidex.db` goes through [`Db::write`]. The closure
//! works on a [`Tx`], which looks like a ReDB write transaction but records
//! every `insert` and `remove` as an [`Op`]. The recorded [`WriteSet`] is
//! what a replicated store proposes to Raft (D2); applying a write set is a
//! pure byte operation ([`Db::apply`]) that needs no glidex logic, so it is
//! identical on every replica and every version.
//!
//! In `Local` mode (a standalone host, D5) the transaction simply commits.
//! `Replicated` mode is added with the Raft integration (milestone C2) behind
//! the same API.

#[cfg(test)]
use redb::ReadableTable as _;
use redb::{ReadTransaction, ReadableDatabase, TableDefinition, TableHandle};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::cell::RefCell;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Rung after every write to the database: `?wait` and `GET /watch`
/// (spec/reconciliation.md §12.6) look again when it changes. Carries no
/// data; listeners re-read.
pub type Bell = Arc<tokio::sync::watch::Sender<u64>>;

pub fn new_bell() -> Bell {
    Arc::new(tokio::sync::watch::channel(0u64).0)
}

pub fn ring(bell: &Bell) {
    bell.send_modify(|v| *v = v.wrapping_add(1));
}

/// Format of [`WriteSet`] this build produces and understands.
pub const WRITE_SET_FORMAT: u16 = 1;

macro_rules! tables {
    ($($variant:ident = $id:literal => $name:literal),+ $(,)?) => {
        /// A table definition as the store uses them.
pub type Def = TableDefinition<'static, &'static str, &'static [u8]>;

/// Every table of `glidex.db`. **Append-only** (§6.6): a table can be
        /// retired, but its id is never reused.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[repr(u16)]
        pub enum TableId { $($variant = $id),+ }

        impl TableId {
            pub const ALL: &'static [TableId] = &[$(TableId::$variant),+];
            pub fn name(self) -> &'static str { match self { $(TableId::$variant => $name),+ } }
            pub fn from_name(name: &str) -> Option<TableId> { match name { $($name => Some(TableId::$variant),)+ _ => None } }
            pub fn from_id(id: u16) -> Option<TableId> { match id { $($id => Some(TableId::$variant),)+ _ => None } }
            pub fn definition(self) -> TableDefinition<'static, &'static str, &'static [u8]> { TableDefinition::new(self.name()) }
        }
    };
}

tables! {
    Vms = 1 => "vms",
    Events = 2 => "events",
    Meta = 3 => "meta",
    Audit = 4 => "audit",
    Projects = 5 => "projects",
    Networks = 6 => "networks",
    Credentials = 7 => "credentials",
    Images = 8 => "images",
    Disks = 9 => "disks",
    ImageMeta = 10 => "image_meta",
    Users = 11 => "users",
    Identities = 12 => "identities",
    Teams = 13 => "teams",
    PolicyLinks = 14 => "policy_links",
    Sessions = 15 => "sessions",
    ApiTokens = 16 => "api_tokens",
    SitePolicies = 17 => "site_policies",
    SitePolicyVersions = 18 => "site_policy_versions",
    MeterCursors = 19 => "meter_cursors",
    MeterOpen = 20 => "meter_open",
    UsageHourly = 21 => "usage_hourly",
    MeterMeta = 22 => "meter_meta",
    Rate5m = 23 => "rate_5m",
    UsageDaily = 24 => "usage_daily",
    UsageMonthlyRates = 25 => "usage_monthly_rates",
}

impl Serialize for TableId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u16(*self as u16)
    }
}

impl<'de> Deserialize<'de> for TableId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let id = u16::deserialize(d)?;
        TableId::from_id(id).ok_or_else(|| serde::de::Error::custom(format!("unknown table id {id}")))
    }
}

/// One change to one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Put { table: TableId, key: Vec<u8>, value: Vec<u8> },
    Delete { table: TableId, key: Vec<u8> },
}

impl Op {
    pub fn table(&self) -> TableId {
        match self {
            Op::Put { table, .. } | Op::Delete { table, .. } => *table,
        }
    }
    pub fn key(&self) -> &[u8] {
        match self {
            Op::Put { key, .. } | Op::Delete { key, .. } => key,
        }
    }
}

/// Who asked for a write; kept in the log for audit and debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    Api,
    Controller,
    Auth,
    Metering,
    Images,
    Network,
    Migration,
    System,
}

/// The ordered `put`/`delete` operations of one write: the Raft log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteSet {
    pub format: u16,
    pub origin: Origin,
    pub ops: Vec<Op>,
}

/// What a store subscriber is told after each applied write set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub revision: u64,
    pub tables: SmallVec<[TableId; 4]>,
    pub keys: Vec<(TableId, Vec<u8>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consistency {
    Linearizable,
    Local,
}

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("Database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("Transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("Table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("Storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("Commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not the leader (leader: {leader:?})")]
    NotLeader { leader: Option<String> },
    #[error("write set of {0} bytes exceeds the 4 MiB entry limit")]
    TooLarge(usize),
    #[error("unknown table {0:?} (not in TableId)")]
    UnknownTable(String),
    #[error("unsupported write-set format {0}")]
    Format(u16),
}

/// Largest write set a replicated store accepts (§6.2).
pub const MAX_WRITE_SET_BYTES: usize = 4 * 1024 * 1024;

/// A write transaction that records what it writes. Dropping it without
/// [`Tx::commit`] aborts it. It holds the write lane (D3) until then.
pub struct Tx<'db> {
    db: &'db Db,
    origin: Origin,
    txn: redb::WriteTransaction,
    ops: RefCell<Vec<Op>>,
    _lane: std::sync::MutexGuard<'db, ()>,
}

impl Tx<'_> {
    pub fn open_table(&self, def: TableDefinition<'static, &'static str, &'static [u8]>) -> Result<RecTable<'_>, redb::TableError> {
        let id = TableId::from_name(def.name()).ok_or_else(|| redb::TableError::TableDoesNotExist(def.name().to_string()))?;
        Ok(RecTable { table: self.txn.open_table(def)?, id, ops: &self.ops })
    }

    /// Persist what was written: commit locally, or propose the write set
    /// and wait until it is applied (§6.1). An empty write set commits
    /// nothing.
    pub fn commit(self) -> Result<(), StoreError> {
        let Tx { db, origin, txn, ops, _lane } = self;
        let ops = ops.into_inner();
        let replicator = match &*db.mode.lock().unwrap() {
            Mode::Local => None,
            Mode::Replicated(r) => Some(r.clone()),
        };
        if ops.is_empty() {
            let _ = txn.abort();
            return Ok(());
        }
        let ws = WriteSet { format: WRITE_SET_FORMAT, origin, ops };
        match replicator {
            None => {
                txn.commit()?;
                let rev = db.revision.fetch_add(1, Ordering::AcqRel) + 1;
                db.published(rev, ws);
            }
            Some(r) => {
                let _ = txn.abort();
                let size = serde_json::to_vec(&ws).map(|v| v.len()).unwrap_or(0);
                if size > MAX_WRITE_SET_BYTES {
                    return Err(StoreError::TooLarge(size));
                }
                r.propose(db, ws)?;
            }
        }
        Ok(())
    }
}

/// A table inside a [`Tx`]. Reads (`get`, `iter`, `range`, …) see the
/// transaction's own writes; `insert` and `remove` are recorded.
pub struct RecTable<'tx> {
    table: redb::Table<'tx, &'static str, &'static [u8]>,
    id: TableId,
    ops: &'tx RefCell<Vec<Op>>,
}

impl<'tx> Deref for RecTable<'tx> {
    type Target = redb::Table<'tx, &'static str, &'static [u8]>;
    fn deref(&self) -> &Self::Target {
        &self.table
    }
}

impl RecTable<'_> {
    pub fn insert(&mut self, key: &str, value: &[u8]) -> Result<(), redb::StorageError> {
        self.table.insert(key, value)?;
        self.ops.borrow_mut().push(Op::Put { table: self.id, key: key.as_bytes().to_vec(), value: value.to_vec() });
        Ok(())
    }

    /// Remove `key`; `Some` if it existed. Deleting an absent key
    /// records nothing.
    pub fn remove(&mut self, key: &str) -> Result<Option<()>, redb::StorageError> {
        let existed = self.table.remove(key)?.is_some();
        if !existed {
            return Ok(None);
        }
        self.ops.borrow_mut().push(Op::Delete { table: self.id, key: key.as_bytes().to_vec() });
        Ok(Some(()))
    }
}

/// How writes reach durable storage.
pub trait Replicator: Send + Sync {
    /// Propose `ws`; return once it is committed **and applied locally**
    /// (the replicator calls [`Db::apply`] itself).
    fn propose(&self, db: &Db, ws: WriteSet) -> Result<u64, StoreError>;
}

enum Mode {
    Local,
    Replicated(Arc<dyn Replicator>),
}

/// The control-plane database.
pub struct Db {
    inner: redb::Database,
    bell: Bell,
    revision: AtomicU64,
    applied: tokio::sync::broadcast::Sender<Applied>,
    mode: Mutex<Mode>,
    /// The write lane (D3): one write at a time.
    lane: Mutex<()>,
    /// Every committed write set, in order, when journaling is on (tests).
    journal: Mutex<Option<Vec<WriteSet>>>,
}

impl Db {
    pub fn create(path: impl AsRef<Path>) -> Result<Db, StoreError> {
        let inner = redb::Database::create(path.as_ref())?;
        // Reads open tables that nothing may have written yet.
        let txn = inner.begin_write()?;
        for &id in TableId::ALL {
            txn.open_table(id.definition())?;
        }
        txn.commit()?;
        let db = Db {
            inner,
            bell: new_bell(),
            revision: AtomicU64::new(0),
            applied: tokio::sync::broadcast::channel(1024).0,
            mode: Mutex::new(Mode::Local),
            lane: Mutex::new(()),
            journal: Mutex::new(None),
        };
        // The replay check (§15): journal every write of this database.
        // A database that already holds data can't be rebuilt from its
        // journal alone, so only fresh ones are checked.
        if std::env::var_os("GLIDEX_CHECK_WRITE_SETS").is_some() && db.dump()?.is_empty() {
            db.journal_writes();
        }
        Ok(db)
    }

    /// The change bell, rung after every write.
    pub fn bell(&self) -> Bell {
        self.bell.clone()
    }

    pub fn begin_read(&self) -> Result<ReadTransaction, redb::TransactionError> {
        self.inner.begin_read()
    }

    /// A read transaction. `Local` reads the local replica as it is.
    pub fn read(&self, _c: Consistency) -> Result<ReadTransaction, StoreError> {
        Ok(self.inner.begin_read()?)
    }

    /// The last applied revision (§6.4). In `Local` mode a counter of
    /// non-empty writes since start.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Applied> {
        self.applied.subscribe()
    }

    /// Switch to replicated writes (C2).
    pub fn set_replicator(&self, r: Arc<dyn Replicator>) {
        *self.mode.lock().unwrap() = Mode::Replicated(r);
    }

    /// Start a write: the transaction records its write set, and holds the
    /// write lane until committed or dropped.
    pub fn begin(&self, origin: Origin) -> Result<Tx<'_>, StoreError> {
        let lane = self.lane.lock().unwrap_or_else(|p| p.into_inner());
        let txn = self.inner.begin_write()?;
        Ok(Tx { db: self, origin, txn, ops: RefCell::new(Vec::new()), _lane: lane })
    }

    /// Run `f` against the current state and persist exactly the writes it
    /// made (§6.1).
    pub fn write<R, E>(&self, origin: Origin, f: impl FnOnce(&Tx<'_>) -> Result<R, E>) -> Result<R, E>
    where
        E: From<StoreError>,
    {
        let tx = self.begin(origin)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Apply a write set as raw bytes, in one transaction (§6.2: "applying
    /// an entry is a pure byte operation"). `also` runs inside the same
    /// transaction (a replicated store advances `last_applied` there).
    pub fn apply(&self, ws: &WriteSet, revision: u64, also: impl FnOnce(&redb::WriteTransaction) -> Result<(), StoreError>) -> Result<(), StoreError> {
        if ws.format > WRITE_SET_FORMAT {
            return Err(StoreError::Format(ws.format));
        }
        let txn = self.inner.begin_write()?;
        for op in &ws.ops {
            let key = std::str::from_utf8(op.key()).map_err(|_| StoreError::UnknownTable("non-utf8 key".into()))?;
            let mut t = txn.open_table(op.table().definition())?;
            match op {
                Op::Put { value, .. } => {
                    t.insert(key, value.as_slice())?;
                }
                Op::Delete { .. } => {
                    t.remove(key)?;
                }
            }
        }
        also(&txn)?;
        txn.commit()?;
        self.revision.store(revision, Ordering::Release);
        self.published(revision, ws.clone());
        Ok(())
    }

    fn published(&self, revision: u64, ws: WriteSet) {
        if let Some(j) = self.journal.lock().unwrap().as_mut() {
            j.push(ws.clone());
        }
        let mut tables: SmallVec<[TableId; 4]> = SmallVec::new();
        let mut keys = Vec::with_capacity(ws.ops.len());
        for op in &ws.ops {
            if !tables.contains(&op.table()) {
                tables.push(op.table());
            }
            keys.push((op.table(), op.key().to_vec()));
        }
        ring(&self.bell);
        let _ = self.applied.send(Applied { revision, tables, keys });
    }

    /// Start recording every committed write set (the replay test, §15).
    pub fn journal_writes(&self) {
        *self.journal.lock().unwrap() = Some(Vec::new());
    }

    pub fn journal(&self) -> Vec<WriteSet> {
        self.journal.lock().unwrap().clone().unwrap_or_default()
    }

    /// Every table's contents, in key order, for comparing databases.
    pub fn dump(&self) -> Result<Vec<(TableId, Vec<(String, Vec<u8>)>)>, StoreError> {
        use redb::ReadableTable;
        let txn = self.inner.begin_read()?;
        let mut out = Vec::new();
        for &id in TableId::ALL {
            let rows = match txn.open_table(id.definition()) {
                Ok(t) => {
                    let mut rows = Vec::new();
                    for r in t.iter()? {
                        let (k, v) = r?;
                        rows.push((k.value().to_string(), v.value().to_vec()));
                    }
                    rows
                }
                Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
                Err(e) => return Err(e.into()),
            };
            if !rows.is_empty() {
                out.push((id, rows));
            }
        }
        Ok(out)
    }

    /// Replay `journal` into an empty database at `path`.
    pub fn replay(path: impl AsRef<Path>, journal: &[WriteSet]) -> Result<Db, StoreError> {
        let db = Db::create(path)?;
        for (i, ws) in journal.iter().enumerate() {
            db.apply(ws, i as u64 + 1, |_| Ok(()))?;
        }
        Ok(db)
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        // With GLIDEX_CHECK_WRITE_SETS set, every database the test suite
        // opens is replayed into a fresh one and compared (§6.1 invariant).
        if std::env::var_os("GLIDEX_CHECK_WRITE_SETS").is_none() || std::thread::panicking() {
            return;
        }
        if self.journal.lock().unwrap().is_none() {
            return;
        }
        let journal = self.journal();
        let Ok(dir) = tempfile::tempdir() else { return };
        // Databases that predate journaling (opened with existing data) can't
        // be checked from their journal alone.
        if self.revision() != journal.len() as u64 {
            return;
        }
        let replayed = Db::replay(dir.path().join("replay.db"), &journal).expect("replay of the write sets failed");
        replayed.journal.lock().unwrap().take();
        let (a, b) = (self.dump().expect("dump"), replayed.dump().expect("dump"));
        assert!(a == b, "replaying the write sets did not reproduce the database (spec/clustering.md §6.1 invariant)");
        replayed.journal.lock().unwrap().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(tx: &Tx<'_>, t: TableId, k: &str, v: &[u8]) {
        tx.open_table(t.definition()).unwrap().insert(k, v).unwrap();
    }

    #[test]
    fn write_sets_replay_to_the_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("a.db")).unwrap();
        db.journal_writes();
        db.write(Origin::Api, |tx| -> Result<(), StoreError> {
            put(tx, TableId::Vms, "a", b"1");
            put(tx, TableId::Vms, "b", b"2");
            put(tx, TableId::Users, "u", b"3");
            Ok(())
        })
        .unwrap();
        db.write(Origin::Api, |tx| -> Result<(), StoreError> {
            let mut t = tx.open_table(TableId::Vms.definition())?;
            assert!(t.remove("a")?.is_some());
            assert!(t.remove("nope")?.is_none());
            // Reads see the transaction's own writes.
            t.insert("c", b"4")?;
            assert!(t.get("c")?.is_some());
            Ok(())
        })
        .unwrap();
        // An empty write set commits nothing.
        let rev = db.revision();
        db.write(Origin::Api, |_| -> Result<(), StoreError> { Ok(()) }).unwrap();
        assert_eq!(db.revision(), rev);
        // A failed closure leaves nothing behind, in the database or the journal.
        let failed: Result<(), StoreError> = db.write(Origin::Api, |tx| {
            put(tx, TableId::Vms, "x", b"9");
            Err(StoreError::NotLeader { leader: None })
        });
        assert!(failed.is_err());
        assert_eq!(db.journal().len(), 2);

        let replayed = Db::replay(dir.path().join("b.db"), &db.journal()).unwrap();
        assert_eq!(db.dump().unwrap(), replayed.dump().unwrap());
        assert_eq!(replayed.revision(), 2);
    }

    #[test]
    fn write_sets_survive_serialization() {
        let ws = WriteSet {
            format: WRITE_SET_FORMAT,
            origin: Origin::Controller,
            ops: vec![
                Op::Put { table: TableId::Events, key: b"vm/1".to_vec(), value: vec![0, 255, 7] },
                Op::Delete { table: TableId::Audit, key: b"k".to_vec() },
            ],
        };
        let back: WriteSet = serde_json::from_slice(&serde_json::to_vec(&ws).unwrap()).unwrap();
        assert_eq!(ws, back);
    }

    #[test]
    fn subscribers_hear_about_each_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("a.db")).unwrap();
        let (mut rx, bell) = (db.subscribe(), db.bell().subscribe());
        db.write(Origin::Api, |tx| -> Result<(), StoreError> {
            put(tx, TableId::Vms, "a", b"1");
            put(tx, TableId::Disks, "d", b"1");
            Ok(())
        })
        .unwrap();
        let a = rx.try_recv().unwrap();
        assert_eq!((a.revision, a.tables.as_slice()), (1, [TableId::Vms, TableId::Disks].as_slice()));
        assert_eq!(a.keys.len(), 2);
        assert!(bell.has_changed().unwrap());
    }

    #[test]
    fn table_ids_are_stable() {
        // Append-only (§6.6): these numbers are on disk in Raft logs.
        assert_eq!((TableId::Vms as u16, TableId::Audit as u16, TableId::UsageMonthlyRates as u16), (1, 4, 25));
        for &t in TableId::ALL {
            assert_eq!(TableId::from_name(t.name()), Some(t));
            assert_eq!(TableId::from_id(t as u16), Some(t));
        }
    }
}
