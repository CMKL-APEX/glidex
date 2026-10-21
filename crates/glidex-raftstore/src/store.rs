//! The write path of a replicated ReDB database (glidex spec/clustering.md
//! §6.1).
//!
//! Every change goes through [`Db::write`]. The closure works on a [`Tx`],
//! which looks like a ReDB write transaction but records every `insert` and
//! `remove` as an [`Op`]. The recorded [`WriteSet`] is what a replicated
//! store proposes to Raft; applying a write set is a pure byte operation
//! ([`Db::apply`]) that needs no application logic, so it is identical on
//! every replica and every version.
//!
//! In `Local` mode (a standalone host) the transaction simply commits;
//! `Replicated` mode ([`crate::raft`]) proposes it behind the same API.
//!
//! What the store holds is the application's [`Schema`]: its tables, and
//! who may write.

#[cfg(test)]
use redb::ReadableTable as _;
use redb::{ReadTransaction, ReadableDatabase, TableDefinition, TableHandle};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::cell::RefCell;
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// What a store holds: its tables and who writes. Implemented by a unit
/// struct for each application (glidex's control plane, hyperstor's map
/// service).
pub trait Schema:
    Debug + Clone + Copy + Default + PartialEq + Eq + PartialOrd + Ord + Send + Sync + 'static
{
    /// The tables. **Append-only**: a table's id is on disk in Raft logs
    /// and snapshots, so a table can be retired but its id is never
    /// reused. Serializes as its id.
    type Table: Debug
        + Clone
        + Copy
        + PartialEq
        + Eq
        + Hash
        + PartialOrd
        + Ord
        + Serialize
        + DeserializeOwned
        + Send
        + Sync
        + 'static;

    /// Who asked for a write; kept in the log for audit and debugging.
    type Origin: Debug
        + Clone
        + Copy
        + PartialEq
        + Eq
        + Serialize
        + DeserializeOwned
        + Send
        + Sync
        + 'static;

    /// Every table, [`Self::LOCAL`] included.
    const TABLES: &'static [Self::Table];

    /// Each member's own bookkeeping (`last_applied`, `last_membership`):
    /// not replicated, and left out of snapshots and comparisons.
    const LOCAL: Self::Table;

    /// The origin of the entries Raft itself writes (blank and membership).
    const SYSTEM: Self::Origin;

    /// The first eight bytes of a snapshot stream.
    const SNAPSHOT_MAGIC: &'static [u8; 8];

    /// When this environment variable is set, every fresh database
    /// journals its write sets and, when dropped, is replayed from them
    /// into a new one and compared (the write-set invariant).
    const CHECK_ENV: &'static str;

    /// The largest write set the store accepts as one Raft entry, in
    /// bytes of its JSON encoding.
    const MAX_WRITE_SET_BYTES: usize = MAX_WRITE_SET_BYTES;

    fn table_name(t: Self::Table) -> &'static str;
    fn table_id(t: Self::Table) -> u16;
    fn table_from_id(id: u16) -> Option<Self::Table>;
    fn table_from_name(name: &str) -> Option<Self::Table>;

    /// Writes that a member which isn't the leader sends to the leader
    /// through its [`Forwarder`] instead of refusing (glidex: a node's
    /// status writes, spec/clustering.md §8.3).
    fn forwarded(_origin: Self::Origin) -> bool {
        false
    }
}

/// A table definition as the store uses them.
pub type Def = TableDefinition<'static, &'static str, &'static [u8]>;

/// The definition of one of the schema's tables.
pub fn definition<S: Schema>(t: S::Table) -> Def {
    TableDefinition::new(S::table_name(t))
}

fn is_local<S: Schema>(t: S::Table) -> bool {
    t == S::LOCAL
}

fn replicated<S: Schema>() -> impl Iterator<Item = S::Table> {
    S::TABLES.iter().copied().filter(|t| !is_local::<S>(*t))
}

/// Rung after every write to the database: listeners look again when it
/// changes. Carries no data; listeners re-read.
pub type Bell = Arc<tokio::sync::watch::Sender<u64>>;

pub fn new_bell() -> Bell {
    Arc::new(tokio::sync::watch::channel(0u64).0)
}

pub fn ring(bell: &Bell) {
    bell.send_modify(|v| *v = v.wrapping_add(1));
}

/// Format of [`WriteSet`] this build produces and understands.
pub const WRITE_SET_FORMAT: u16 = 1;

/// Bytes as base64 text: write sets travel as JSON (log, transport), where
/// a `Vec<u8>` would cost four bytes per byte.
mod b64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD_NO_PAD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let t = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(t)
            .map_err(serde::de::Error::custom)
    }
}

/// One change to one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub enum Op<S: Schema> {
    Put {
        table: S::Table,
        #[serde(with = "b64")]
        key: Vec<u8>,
        #[serde(with = "b64")]
        value: Vec<u8>,
    },
    Delete {
        table: S::Table,
        #[serde(with = "b64")]
        key: Vec<u8>,
    },
}

impl<S: Schema> Op<S> {
    pub fn table(&self) -> S::Table {
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

/// The ordered `put`/`delete` operations of one write: the Raft log entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(bound = "")]
pub struct WriteSet<S: Schema> {
    pub format: u16,
    pub origin: S::Origin,
    pub ops: Vec<Op<S>>,
}

/// What a store subscriber is told after each applied write set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied<S: Schema> {
    pub revision: u64,
    /// What was applied.
    pub ws: Arc<WriteSet<S>>,
    pub tables: SmallVec<[S::Table; 4]>,
    pub keys: Vec<(S::Table, Vec<u8>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consistency {
    Linearizable,
    Local,
}

#[derive(Error, Debug)]
pub enum StoreError {
    /// A member's write was refused by the leader (the object changed, or
    /// isn't the member's to write).
    #[error("refused by the cluster: {0}")]
    Refused(String),
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
    #[error("write set of {0} bytes exceeds the store's entry limit")]
    TooLarge(usize),
    #[error("unknown table {0:?} (not in the schema)")]
    UnknownTable(String),
    #[error("unsupported write-set format {0}")]
    Format(u16),
}

const DUMP_END: u16 = 0xFFFF;
/// Keys of the local table ([`Schema::LOCAL`]).
pub const LAST_APPLIED: &str = "last_applied";
pub const LAST_MEMBERSHIP: &str = "last_membership";
pub const MIRROR_REVISION: &str = "mirror_revision";

fn read_local<S: Schema>(txn: &ReadTransaction, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
    Ok(txn
        .open_table(definition::<S>(S::LOCAL))?
        .get(key)?
        .map(|v| v.value().to_vec()))
}

/// A read transaction over the replicated tables with the Raft position it
/// corresponds to.
pub struct SnapshotView<S: Schema> {
    txn: ReadTransaction,
    pub last_applied: Option<Vec<u8>>,
    pub last_membership: Option<Vec<u8>>,
    schema: PhantomData<S>,
}

impl<S: Schema> SnapshotView<S> {
    /// Stream every replicated table (key order, table order) to `w`. The
    /// size is bounded by one read transaction, not by memory.
    pub fn write_to(&self, w: &mut dyn std::io::Write) -> Result<u64, StoreError> {
        use redb::ReadableTable;
        let mut n = 0u64;
        w.write_all(S::SNAPSHOT_MAGIC)?;
        for id in replicated::<S>() {
            let t = self.txn.open_table(definition::<S>(id))?;
            for r in t.iter()? {
                let (k, v) = r?;
                let (k, v) = (k.value().as_bytes(), v.value());
                w.write_all(&S::table_id(id).to_le_bytes())?;
                w.write_all(&(k.len() as u32).to_le_bytes())?;
                w.write_all(&(v.len() as u32).to_le_bytes())?;
                w.write_all(k)?;
                w.write_all(v)?;
                n += 1;
            }
        }
        w.write_all(&DUMP_END.to_le_bytes())?;
        w.flush()?;
        Ok(n)
    }
}

/// Largest write set a replicated store accepts, unless its schema says
/// otherwise ([`Schema::MAX_WRITE_SET_BYTES`]).
pub const MAX_WRITE_SET_BYTES: usize = 4 * 1024 * 1024;

/// A write transaction that records what it writes. Dropping it without
/// [`Tx::commit`] aborts it. It holds the write lane until then: one write
/// at a time, so a write reads exactly the state its write set applies to.
pub struct Tx<'db, S: Schema> {
    db: &'db Db<S>,
    origin: S::Origin,
    txn: redb::WriteTransaction,
    ops: RefCell<Vec<Op<S>>>,
    /// A forwarded write ([`Schema::forwarded`]) on a member that isn't the
    /// leader: sent to the leader at commit.
    forward: bool,
    _lane: std::sync::MutexGuard<'db, ()>,
}

impl<S: Schema> Tx<'_, S> {
    pub fn open_table(&self, def: Def) -> Result<RecTable<'_, S>, redb::TableError> {
        let id = S::table_from_name(def.name())
            .ok_or_else(|| redb::TableError::TableDoesNotExist(def.name().to_string()))?;
        Ok(RecTable {
            table: self.txn.open_table(def)?,
            id,
            ops: &self.ops,
        })
    }

    /// Persist what was written: commit locally, or propose the write set
    /// and wait until it is applied. An empty write set commits nothing.
    pub fn commit(self) -> Result<(), StoreError> {
        let Tx {
            db,
            origin,
            txn,
            ops,
            forward,
            _lane,
        } = self;
        let ops = ops.into_inner();
        let (replicator, mirror) = match &*db.mode.lock().unwrap() {
            Mode::Local | Mode::Stopped => (None, None),
            Mode::Replicated(r) => (Some(r.clone()), None),
            Mode::Mirror(m) => (None, Some(m.clone())),
        };
        if ops.is_empty() {
            let _ = txn.abort();
            return Ok(());
        }
        let ws = WriteSet {
            format: WRITE_SET_FORMAT,
            origin,
            ops,
        };
        if forward {
            let _ = txn.abort();
            let f = db.forwarder.lock().unwrap().clone();
            let f = f.ok_or(StoreError::NotLeader { leader: None })?;
            let index = f.status_write(&ws)?;
            // Read your writes: the caller's next step reads this store.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while db.revision() < index && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            return Ok(());
        }
        if let Some(m) = mirror {
            let _ = txn.abort();
            return match m.submit(&ws)? {
                Submitted::Acked(_) => Ok(()),
                // The leader is out of reach: carry on from the cache.
                Submitted::Queued => db.apply(&ws, db.revision(), |_| Ok(())),
            };
        }
        match replicator {
            None => {
                txn.commit()?;
                let rev = db.revision.fetch_add(1, Ordering::AcqRel) + 1;
                db.published(rev, ws);
            }
            Some(r) => {
                let _ = txn.abort();
                let size = serde_json::to_vec(&ws).map(|v| v.len()).unwrap_or(0);
                if size > S::MAX_WRITE_SET_BYTES {
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
pub struct RecTable<'tx, S: Schema> {
    table: redb::Table<'tx, &'static str, &'static [u8]>,
    id: S::Table,
    ops: &'tx RefCell<Vec<Op<S>>>,
}

impl<'tx, S: Schema> Deref for RecTable<'tx, S> {
    type Target = redb::Table<'tx, &'static str, &'static [u8]>;
    fn deref(&self) -> &Self::Target {
        &self.table
    }
}

impl<S: Schema> RecTable<'_, S> {
    pub fn insert(&mut self, key: &str, value: &[u8]) -> Result<(), redb::StorageError> {
        self.table.insert(key, value)?;
        self.ops.borrow_mut().push(Op::Put {
            table: self.id,
            key: key.as_bytes().to_vec(),
            value: value.to_vec(),
        });
        Ok(())
    }

    /// Remove `key`; `Some` if it existed. Deleting an absent key
    /// records nothing.
    pub fn remove(&mut self, key: &str) -> Result<Option<()>, redb::StorageError> {
        let existed = self.table.remove(key)?.is_some();
        if !existed {
            return Ok(None);
        }
        self.ops.borrow_mut().push(Op::Delete {
            table: self.id,
            key: key.as_bytes().to_vec(),
        });
        Ok(Some(()))
    }
}

/// How writes reach durable storage.
pub trait Replicator<S: Schema>: Send + Sync {
    /// Propose `ws`; return once it is committed **and applied locally**
    /// (the replicator calls [`Db::apply`] itself).
    fn propose(&self, db: &Db<S>, ws: WriteSet<S>) -> Result<u64, StoreError>;

    /// Called with the write lane held, before a write starts: refuse on a
    /// follower, and on a new leader wait until it has applied everything
    /// committed before it (a write must read exactly the state its write
    /// set will apply to).
    fn before_write(&self) -> Result<(), StoreError> {
        Ok(())
    }

    /// Whether a write would be accepted right now (this member leads).
    fn can_write(&self) -> bool {
        true
    }
}

/// How a follower gets a *simple* write done by the leader (glidex
/// spec/clustering.md §6.4): it can't be a closure (the leader must run it
/// against its state), so it is either raw puts and deletes, a forwarded
/// write set ([`Schema::forwarded`]), or a named operation.
pub trait Forwarder<S: Schema>: Send + Sync {
    fn write_raw(&self, ops: Vec<Op<S>>) -> Result<(), StoreError>;
    /// Have the leader check and apply a forwarded write set; returns the
    /// revision it was applied at.
    fn status_write(&self, ws: &WriteSet<S>) -> Result<u64, StoreError>;
    fn call(&self, op: &str, args: serde_json::Value) -> Result<serde_json::Value, StoreError>;
}

/// A cache of the replicated store on a member that isn't part of the Raft
/// group (glidex §8.2): writes go to the leader, and come back through a
/// watch.
pub trait Mirror<S: Schema>: Send + Sync {
    /// Send `ws` to the leader. `Acked(index)`: it was applied there as entry
    /// `index` and the cache has caught up with it. `Queued`: the leader can't
    /// be reached; the write set is kept to send later and the caller applies
    /// it to the cache now.
    fn submit(&self, ws: &WriteSet<S>) -> Result<Submitted, StoreError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    Acked(u64),
    Queued,
}

enum Mode<S: Schema> {
    Local,
    /// Its Raft member has stopped: writes are refused.
    Stopped,
    Replicated(Arc<dyn Replicator<S>>),
    Mirror(Arc<dyn Mirror<S>>),
}

/// One table's contents, as [`Db::dump`] gives them: keys and values in key order.
pub type TableDump<S> = (<S as Schema>::Table, Vec<(String, Vec<u8>)>);

/// The database.
pub struct Db<S: Schema> {
    inner: redb::Database,
    bell: Bell,
    revision: AtomicU64,
    applied: tokio::sync::broadcast::Sender<Applied<S>>,
    mode: Mutex<Mode<S>>,
    /// The write lane: one write at a time.
    lane: Mutex<()>,
    /// Every committed write set, in order, when journaling is on (tests).
    journal: Mutex<Option<Vec<WriteSet<S>>>>,
    forwarder: Mutex<Option<Arc<dyn Forwarder<S>>>>,
}

impl<S: Schema> Db<S> {
    pub fn create(path: impl AsRef<Path>) -> Result<Db<S>, StoreError> {
        let inner = redb::Database::create(path.as_ref())?;
        // Reads open tables that nothing may have written yet.
        let txn = inner.begin_write()?;
        for &id in S::TABLES {
            txn.open_table(definition::<S>(id))?;
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
            forwarder: Mutex::new(None),
        };
        // The replay check: journal every write of this database. A
        // database that already holds data can't be rebuilt from its
        // journal alone, so only fresh ones are checked.
        if std::env::var_os(S::CHECK_ENV).is_some() && db.dump()?.is_empty() {
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

    /// The last applied revision. In `Local` mode a counter of non-empty
    /// writes since start.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Applied<S>> {
        self.applied.subscribe()
    }

    /// Whether a write would be accepted now: always when local, on the
    /// leader when replicated. Startup work that writes (default records,
    /// migrations) is skipped, not failed, when it can't.
    pub fn can_write(&self) -> bool {
        match &*self.mode.lock().unwrap() {
            Mode::Local => true,
            Mode::Replicated(r) => r.can_write(),
            // A mirror's writes are status for the leader to accept;
            // setup is the leader's.
            Mode::Mirror(_) | Mode::Stopped => false,
        }
    }

    /// Make this database a cache of the replicated store.
    pub fn set_mirror(&self, m: Arc<dyn Mirror<S>>) {
        *self.mode.lock().unwrap() = Mode::Mirror(m);
    }

    pub fn is_mirror(&self) -> bool {
        matches!(&*self.mode.lock().unwrap(), Mode::Mirror(_))
    }

    pub fn set_forwarder(&self, f: Arc<dyn Forwarder<S>>) {
        *self.forwarder.lock().unwrap() = Some(f);
    }

    /// Have the leader apply `ops`, when this member can't write itself.
    pub fn forward_raw(&self, ops: Vec<Op<S>>) -> Result<(), StoreError> {
        let f = self.forwarder.lock().unwrap().clone();
        match f {
            Some(f) => f.write_raw(ops),
            None => Err(StoreError::NotLeader { leader: None }),
        }
    }

    pub fn forward_call(
        &self,
        op: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let f = self.forwarder.lock().unwrap().clone();
        match f {
            Some(f) => f.call(op, args),
            None => Err(StoreError::NotLeader { leader: None }),
        }
    }

    pub fn is_replicated(&self) -> bool {
        matches!(&*self.mode.lock().unwrap(), Mode::Replicated(_))
    }

    /// Run `f` while no write is in progress or can start: for copying the
    /// database file.
    pub fn quiesce<R>(&self, f: impl FnOnce() -> R) -> R {
        let _lane = self.lane.lock().unwrap_or_else(|p| p.into_inner());
        f()
    }

    /// Switch to replicated writes.
    pub fn set_replicator(&self, r: Arc<dyn Replicator<S>>) {
        *self.mode.lock().unwrap() = Mode::Replicated(r);
    }

    /// Refuse every write from now on: the store's Raft member has stopped
    /// ([`crate::raft::ClusterNode::shutdown`]). Also lets go of the
    /// replicator, which holds the member, which holds this database.
    pub fn set_stopped(&self) {
        *self.mode.lock().unwrap() = Mode::Stopped;
    }

    /// Start a write: the transaction records its write set, and holds the
    /// write lane until committed or dropped.
    pub fn begin(&self, origin: S::Origin) -> Result<Tx<'_, S>, StoreError> {
        let lane = self.lane.lock().unwrap_or_else(|p| p.into_inner());
        let replicator = match &*self.mode.lock().unwrap() {
            Mode::Replicated(r) => Some(r.clone()),
            Mode::Stopped => return Err(StoreError::NotLeader { leader: None }),
            _ => None,
        };
        // On a member that isn't the leader, a forwarded write goes to the
        // leader to check; anything else is refused here.
        let mut forward = false;
        if let Some(r) = replicator {
            match r.before_write() {
                Ok(()) => {}
                Err(StoreError::NotLeader { .. })
                    if S::forwarded(origin) && self.forwarder.lock().unwrap().is_some() =>
                {
                    forward = true;
                }
                Err(e) => return Err(e),
            }
        }
        let txn = self.inner.begin_write()?;
        Ok(Tx {
            db: self,
            origin,
            txn,
            ops: RefCell::new(Vec::new()),
            forward,
            _lane: lane,
        })
    }

    /// Run `f` against the current state and persist exactly the writes it
    /// made.
    pub fn write<R, E>(
        &self,
        origin: S::Origin,
        f: impl FnOnce(&Tx<'_, S>) -> Result<R, E>,
    ) -> Result<R, E>
    where
        E: From<StoreError>,
    {
        let tx = self.begin(origin)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Apply a write set as raw bytes, in one transaction ("applying an
    /// entry is a pure byte operation"). `also` runs inside the same
    /// transaction (a replicated store advances `last_applied` there).
    pub fn apply(
        &self,
        ws: &WriteSet<S>,
        revision: u64,
        also: impl FnOnce(&redb::WriteTransaction) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        if ws.format > WRITE_SET_FORMAT {
            return Err(StoreError::Format(ws.format));
        }
        let txn = self.inner.begin_write()?;
        for op in &ws.ops {
            let key = std::str::from_utf8(op.key())
                .map_err(|_| StoreError::UnknownTable("non-utf8 key".into()))?;
            let mut t = txn.open_table(definition::<S>(op.table()))?;
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
        if !ws.ops.is_empty() {
            self.published(revision, ws.clone());
        }
        Ok(())
    }

    fn published(&self, revision: u64, ws: WriteSet<S>) {
        if let Some(j) = self.journal.lock().unwrap().as_mut() {
            j.push(ws.clone());
        }
        let mut tables: SmallVec<[S::Table; 4]> = SmallVec::new();
        let mut keys = Vec::with_capacity(ws.ops.len());
        for op in &ws.ops {
            if !tables.contains(&op.table()) {
                tables.push(op.table());
            }
            keys.push((op.table(), op.key().to_vec()));
        }
        ring(&self.bell);
        let _ = self.applied.send(Applied {
            revision,
            ws: Arc::new(ws),
            tables,
            keys,
        });
    }

    /// Start recording every committed write set (the replay check).
    pub fn journal_writes(&self) {
        *self.journal.lock().unwrap() = Some(Vec::new());
    }

    pub fn journal(&self) -> Vec<WriteSet<S>> {
        self.journal.lock().unwrap().clone().unwrap_or_default()
    }

    /// Every replicated table's contents, in key order, for comparing
    /// databases.
    pub fn dump(&self) -> Result<Vec<TableDump<S>>, StoreError> {
        use redb::ReadableTable;
        let txn = self.inner.begin_read()?;
        let mut out = Vec::new();
        for id in replicated::<S>() {
            let rows = match txn.open_table(definition::<S>(id)) {
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

    /// A consistent view of the replicated tables, for a snapshot:
    /// `last_applied` is read in the same transaction as the data it
    /// describes.
    pub fn snapshot_view(&self) -> Result<SnapshotView<S>, StoreError> {
        let txn = self.inner.begin_read()?;
        let last_applied = read_local::<S>(&txn, LAST_APPLIED)?;
        let last_membership = read_local::<S>(&txn, LAST_MEMBERSHIP)?;
        Ok(SnapshotView {
            txn,
            last_applied,
            last_membership,
            schema: PhantomData,
        })
    }

    /// Replace every replicated table with the contents of a snapshot
    /// stream, in one transaction; `also` runs inside it (the state machine
    /// records `last_applied` there).
    pub fn install_dump(
        &self,
        r: &mut dyn std::io::Read,
        revision: u64,
        also: impl FnOnce(&redb::WriteTransaction) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != S::SNAPSHOT_MAGIC {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "not a snapshot of this store",
            )));
        }
        // Its contents no longer follow from its own write sets: the replay
        // check has nothing to compare it with.
        self.journal.lock().unwrap().take();
        let txn = self.inner.begin_write()?;
        for id in replicated::<S>() {
            txn.delete_table(definition::<S>(id))?;
            txn.open_table(definition::<S>(id))?;
        }
        loop {
            let mut head = [0u8; 2];
            r.read_exact(&mut head)?;
            let table = u16::from_le_bytes(head);
            if table == DUMP_END {
                break;
            }
            let id = S::table_from_id(table)
                .ok_or_else(|| StoreError::UnknownTable(format!("id {table}")))?;
            let mut lens = [0u8; 8];
            r.read_exact(&mut lens)?;
            let (kl, vl) = (
                u32::from_le_bytes(lens[..4].try_into().unwrap()) as usize,
                u32::from_le_bytes(lens[4..].try_into().unwrap()) as usize,
            );
            let mut key = vec![0u8; kl];
            let mut value = vec![0u8; vl];
            r.read_exact(&mut key)?;
            r.read_exact(&mut value)?;
            let key = String::from_utf8(key)
                .map_err(|_| StoreError::UnknownTable("non-utf8 key".into()))?;
            txn.open_table(definition::<S>(id))?
                .insert(key.as_str(), value.as_slice())?;
        }
        also(&txn)?;
        txn.commit()?;
        self.revision.store(revision, Ordering::Release);
        ring(&self.bell);
        Ok(())
    }

    /// Write a row of the local table (not replicated).
    pub fn put_local(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        let txn = self.inner.begin_write()?;
        txn.open_table(definition::<S>(S::LOCAL))?
            .insert(key, value)?;
        txn.commit()?;
        Ok(())
    }

    pub fn delete_local(&self, key: &str) -> Result<(), StoreError> {
        let txn = self.inner.begin_write()?;
        txn.open_table(definition::<S>(S::LOCAL))?.remove(key)?;
        txn.commit()?;
        Ok(())
    }

    /// Remember how far a mirror has caught up (set from the local table
    /// at start).
    pub fn set_revision(&self, rev: u64) {
        self.revision.store(rev, Ordering::Release);
    }

    /// A mirror: replace every replicated table with `ws`'s puts, in one
    /// transaction, at `revision`.
    pub fn install_ops(&self, ws: &WriteSet<S>, revision: u64) -> Result<(), StoreError> {
        let txn = self.inner.begin_write()?;
        for id in replicated::<S>() {
            txn.delete_table(definition::<S>(id))?;
            txn.open_table(definition::<S>(id))?;
        }
        for op in &ws.ops {
            if let Op::Put { table, key, value } = op {
                let key = std::str::from_utf8(key)
                    .map_err(|_| StoreError::UnknownTable("non-utf8 key".into()))?;
                txn.open_table(definition::<S>(*table))?
                    .insert(key, value.as_slice())?;
            }
        }
        txn.open_table(definition::<S>(S::LOCAL))?
            .insert(MIRROR_REVISION, revision.to_string().as_bytes())?;
        txn.commit()?;
        self.revision.store(revision, Ordering::Release);
        ring(&self.bell);
        let _ = self.applied.send(Applied {
            revision,
            ws: Arc::new(ws.clone()),
            tables: replicated::<S>().collect(),
            keys: Vec::new(),
        });
        Ok(())
    }

    /// The revision a mirror had reached when it last stopped.
    pub fn stored_mirror_revision(&self) -> u64 {
        let Ok(txn) = self.inner.begin_read() else {
            return 0;
        };
        let Ok(t) = txn.open_table(definition::<S>(S::LOCAL)) else {
            return 0;
        };
        t.get(MIRROR_REVISION)
            .ok()
            .flatten()
            .and_then(|v| String::from_utf8(v.value().to_vec()).ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// Replay `journal` into an empty database at `path`.
    pub fn replay(path: impl AsRef<Path>, journal: &[WriteSet<S>]) -> Result<Db<S>, StoreError> {
        let db = Db::create(path)?;
        for (i, ws) in journal.iter().enumerate() {
            db.apply(ws, i as u64 + 1, |_| Ok(()))?;
        }
        Ok(db)
    }
}

impl<S: Schema> Drop for Db<S> {
    fn drop(&mut self) {
        // With the schema's check variable set, every database a test suite
        // opens is replayed into a fresh one and compared.
        if std::env::var_os(S::CHECK_ENV).is_none() || std::thread::panicking() {
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
        let replayed = Db::<S>::replay(dir.path().join("replay.db"), &journal)
            .expect("replay of the write sets failed");
        replayed.journal.lock().unwrap().take();
        let (a, b) = (self.dump().expect("dump"), replayed.dump().expect("dump"));
        assert!(
            a == b,
            "replaying the write sets did not reproduce the database (the write-set invariant)"
        );
        replayed.journal.lock().unwrap().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Test, TestOrigin, TestTable};

    fn put(tx: &Tx<'_, Test>, t: TestTable, k: &str, v: &[u8]) {
        tx.open_table(definition::<Test>(t))
            .unwrap()
            .insert(k, v)
            .unwrap();
    }

    #[test]
    fn write_sets_replay_to_the_same_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::<Test>::create(dir.path().join("a.db")).unwrap();
        db.journal_writes();
        db.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
            put(tx, TestTable::Data, "a", b"1");
            put(tx, TestTable::Data, "b", b"2");
            put(tx, TestTable::Other, "u", b"3");
            Ok(())
        })
        .unwrap();
        db.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
            let mut t = tx.open_table(definition::<Test>(TestTable::Data))?;
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
        db.write(TestOrigin::Api, |_| -> Result<(), StoreError> { Ok(()) })
            .unwrap();
        assert_eq!(db.revision(), rev);
        // A failed closure leaves nothing behind, in the database or the journal.
        let failed: Result<(), StoreError> = db.write(TestOrigin::Api, |tx| {
            put(tx, TestTable::Data, "x", b"9");
            Err(StoreError::NotLeader { leader: None })
        });
        assert!(failed.is_err());
        assert_eq!(db.journal().len(), 2);

        let replayed = Db::<Test>::replay(dir.path().join("b.db"), &db.journal()).unwrap();
        assert_eq!(db.dump().unwrap(), replayed.dump().unwrap());
        assert_eq!(replayed.revision(), 2);
    }

    #[test]
    fn write_sets_survive_serialization() {
        let ws = WriteSet::<Test> {
            format: WRITE_SET_FORMAT,
            origin: TestOrigin::Api,
            ops: vec![
                Op::Put {
                    table: TestTable::Data,
                    key: b"vm/1".to_vec(),
                    value: vec![0, 255, 7],
                },
                Op::Delete {
                    table: TestTable::Other,
                    key: b"k".to_vec(),
                },
            ],
        };
        let json = serde_json::to_vec(&ws).unwrap();
        let back: WriteSet<Test> = serde_json::from_slice(&json).unwrap();
        assert_eq!(ws, back);
    }

    #[test]
    fn subscribers_hear_about_each_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::<Test>::create(dir.path().join("a.db")).unwrap();
        let (mut rx, bell) = (db.subscribe(), db.bell().subscribe());
        db.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
            put(tx, TestTable::Data, "a", b"1");
            put(tx, TestTable::Other, "d", b"1");
            Ok(())
        })
        .unwrap();
        let a = rx.try_recv().unwrap();
        assert_eq!(
            (a.revision, a.tables.as_slice()),
            (1, [TestTable::Data, TestTable::Other].as_slice())
        );
        assert_eq!(a.keys.len(), 2);
        assert!(bell.has_changed().unwrap());
    }

    #[test]
    fn snapshots_carry_the_replicated_tables_only() {
        let dir = tempfile::tempdir().unwrap();
        let a = Db::<Test>::create(dir.path().join("a.db")).unwrap();
        a.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
            put(tx, TestTable::Data, "k", b"v");
            Ok(())
        })
        .unwrap();
        a.put_local(LAST_APPLIED, b"local").unwrap();
        let mut buf = Vec::new();
        assert_eq!(a.snapshot_view().unwrap().write_to(&mut buf).unwrap(), 1);
        let b = Db::<Test>::create(dir.path().join("b.db")).unwrap();
        b.install_dump(&mut buf.as_slice(), 7, |_| Ok(())).unwrap();
        assert_eq!(a.dump().unwrap(), b.dump().unwrap());
        assert_eq!(b.revision(), 7);
        assert_eq!(b.snapshot_view().unwrap().last_applied, None);
        // Another store's snapshot is refused.
        let mut wrong = buf.clone();
        wrong[0] ^= 1;
        assert!(
            b.install_dump(&mut wrong.as_slice(), 8, |_| Ok(()))
                .is_err()
        );
    }
}
