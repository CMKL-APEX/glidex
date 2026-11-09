//! The write path of a replicated ReDB database (glidex spec/14-clustering.md
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
    /// status writes, spec/14-clustering.md §8.3).
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
    #[error("Durability error: {0}")]
    Durability(#[from] redb::SetDurabilityError),
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
/// Small enough that an append of [`MAX_APPEND_ENTRIES`] of them reaches a
/// member within a heartbeat: openraft 0.9 gives each append that long,
/// and one that never fits is retried for ever (spec/14-clustering.md D24).
pub const MAX_WRITE_SET_BYTES: usize = 1024 * 1024;
/// Most entries the leader sends a member in one append.
pub const MAX_APPEND_ENTRIES: u64 = 4;
/// The largest append a member may receive: [`MAX_APPEND_ENTRIES`] write
/// sets at their limit, and room for the entries around them. A smaller
/// limit on the receiving side refuses such an append on every retry, and
/// the log stops there (spec/14-clustering.md §6.2).
pub const MAX_APPEND_BYTES: usize = MAX_WRITE_SET_BYTES * (MAX_APPEND_ENTRIES as usize + 1);

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
            // The revision arrives over the runtime's I/O: wait off the worker.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            crate::wait::blocking(|| {
                while db.revision() < index && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            });
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
    /// Put `value` at `key`. Writing the value the key already holds
    /// records nothing, so a write that changes nothing makes no Raft entry
    /// (glidex spec/14-clustering.md D29).
    pub fn insert(&mut self, key: &str, value: &[u8]) -> Result<(), redb::StorageError> {
        if self
            .table
            .insert(key, value)?
            .is_some_and(|old| old.value() == value)
        {
            return Ok(());
        }
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
/// spec/14-clustering.md §6.4): it can't be a closure (the leader must run it
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
    checkpoints: Mutex<Checkpoints>,
}

/// A checkpoint comes once this many entries were applied without one
/// (spec/14-clustering.md D31)...
pub const CHECKPOINT_ENTRIES: u64 = 100;
/// ...or this long after the last one.
pub const CHECKPOINT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

/// Applies committed without a flush since the last checkpoint (D31).
struct Checkpoints {
    pending: u64,
    since: std::time::Instant,
}

/// Makes sure a redb file exists at `path`, made whole: a new one is built
/// beside it (`<path>.new`) and renamed into place, then the directory is
/// synced. redb writes a new file's header before its magic number, and
/// refuses a file without one ever after ("invalid data"), so a kill while
/// it creates the file in place leaves a store nothing opens. A kill here
/// leaves no file or a complete one; a leftover `.new` is half made and
/// holds nothing, so it is deleted.
pub fn create_whole(path: &Path) -> Result<(), StoreError> {
    let mut name = path.as_os_str().to_owned();
    name.push(".new");
    let new = std::path::PathBuf::from(name);
    match std::fs::remove_file(&new) {
        Ok(()) => tracing::warn!("removed the half-made store file {}", new.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if path.exists() {
        return Ok(());
    }
    drop(redb::Database::create(&new)?);
    std::fs::rename(&new, path)?;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

impl<S: Schema> Db<S> {
    pub fn create(path: impl AsRef<Path>) -> Result<Db<S>, StoreError> {
        create_whole(path.as_ref())?;
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
            checkpoints: Mutex::new(Checkpoints {
                pending: 0,
                since: std::time::Instant::now(),
            }),
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
        let _lane = self.take_lane();
        f()
    }

    /// Wait for the write lane. Its holder may be waiting on the runtime's
    /// I/O (Raft's round trip), so on a runtime worker the wait goes through
    /// `block_in_place`: a worker blocked outright can leave no thread to
    /// drive that I/O, and then neither write ever finishes.
    fn take_lane(&self) -> std::sync::MutexGuard<'_, ()> {
        crate::wait::lock(&self.lane).unwrap_or_else(|p| p.into_inner())
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
        let lane = self.take_lane();
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
        self.apply_batch(vec![(ws.clone(), revision)], also)
    }

    /// Apply write sets in order, all in one transaction (spec
    /// 14-clustering.md D32): one commit for a batch of Raft entries, not
    /// one each. All or none of them land. Once committed, the revision
    /// advances through each entry's in turn, and subscribers hear each
    /// one in order.
    pub fn apply_batch(
        &self,
        batch: Vec<(WriteSet<S>, u64)>,
        also: impl FnOnce(&redb::WriteTransaction) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        self.apply_batch_with(batch, false, also)
    }

    /// [`apply_batch`](Self::apply_batch) without a flush, unless the batch
    /// is due to be a checkpoint: it brings the applies since the last one
    /// to [`CHECKPOINT_ENTRIES`], or comes [`CHECKPOINT_INTERVAL`] after it
    /// (spec/14-clustering.md D31). Only for entries a log keeps: a crash
    /// loses what was applied since the last checkpoint, and the log
    /// applies it again.
    pub fn apply_checkpointed(
        &self,
        batch: Vec<(WriteSet<S>, u64)>,
        also: impl FnOnce(&redb::WriteTransaction) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        self.apply_batch_with(batch, true, also)
    }

    fn apply_batch_with(
        &self,
        batch: Vec<(WriteSet<S>, u64)>,
        checkpointed: bool,
        also: impl FnOnce(&redb::WriteTransaction) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        if let Some((ws, _)) = batch.iter().find(|(ws, _)| ws.format > WRITE_SET_FORMAT) {
            return Err(StoreError::Format(ws.format));
        }
        let mut cp = self.checkpoints.lock().unwrap();
        let n = batch.len() as u64;
        let durable = !checkpointed
            || cp.pending + n >= CHECKPOINT_ENTRIES
            || cp.since.elapsed() >= CHECKPOINT_INTERVAL;
        let mut txn = self.inner.begin_write()?;
        if !durable {
            txn.set_durability(redb::Durability::None)?;
        }
        for (ws, _) in &batch {
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
        }
        also(&txn)?;
        txn.commit()?;
        // A durable commit makes every commit before it durable too.
        if durable {
            *cp = Checkpoints {
                pending: 0,
                since: std::time::Instant::now(),
            };
        } else {
            cp.pending += n;
        }
        drop(cp);
        // Each revision before its announcement: a subscriber that hears
        // revision r finds `revision()` at r or past it.
        for (ws, revision) in batch {
            self.revision.store(revision, Ordering::Release);
            if !ws.ops.is_empty() {
                self.published(revision, ws);
            }
        }
        Ok(())
    }

    /// Make every apply so far durable: an empty durable commit, when
    /// [`apply_checkpointed`](Self::apply_checkpointed) left any unflushed.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        self.checkpoint_when(|_| true)
    }

    /// [`checkpoint`](Self::checkpoint), once [`CHECKPOINT_INTERVAL`] has
    /// passed since the last one: for a timer, when no batch comes to carry
    /// it.
    pub fn checkpoint_if_due(&self) -> Result<(), StoreError> {
        self.checkpoint_when(|cp| cp.since.elapsed() >= CHECKPOINT_INTERVAL)
    }

    fn checkpoint_when(&self, due: impl FnOnce(&Checkpoints) -> bool) -> Result<(), StoreError> {
        let mut cp = self.checkpoints.lock().unwrap();
        if cp.pending == 0 || !due(&cp) {
            return Ok(());
        }
        self.inner.begin_write()?.commit()?;
        *cp = Checkpoints {
            pending: 0,
            since: std::time::Instant::now(),
        };
        Ok(())
    }

    /// Applies committed without a flush since the last checkpoint.
    pub fn unflushed(&self) -> u64 {
        self.checkpoints.lock().unwrap().pending
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

    /// What a kill leaves while redb creates a file (#317): sized, with the
    /// header written but not its magic number. Opening it in place fails.
    fn half_made(path: &Path) {
        let mut bytes = vec![0u8; 1_056_768];
        bytes[8..16].copy_from_slice(&[0, 4, 0, 0, 0, 0x10, 0, 0]);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_half_made_file_is_refused_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        half_made(&path);
        assert!(
            redb::Database::create(&path).is_err(),
            "redb opens it after all"
        );
    }

    #[test]
    fn a_kill_while_creating_leaves_a_store_that_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let mut new = path.as_os_str().to_owned();
        new.push(".new");
        half_made(Path::new(&new));
        let db = Db::<Test>::create(&path).unwrap();
        assert!(!Path::new(&new).exists(), "the half-made file stays");
        drop(db);
        Db::<Test>::create(&path).unwrap();
    }

    /// The child of the test below: creates the store named in the variable.
    #[test]
    fn create_in_a_child() {
        if let Some(path) = std::env::var_os("GLIDEX_RAFTSTORE_CREATE_CHILD") {
            Db::<Test>::create(Path::new(&path)).unwrap();
        }
    }

    #[test]
    fn a_store_killed_at_any_point_of_its_creation_opens() {
        let exe = std::env::current_exe().unwrap();
        for i in 0..40u64 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.db");
            let mut child = std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "store::tests::create_in_a_child",
                    "--test-threads=1",
                ])
                .env("GLIDEX_RAFTSTORE_CREATE_CHILD", &path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            // Kill points spread over the child's start and its creation.
            std::thread::sleep(std::time::Duration::from_micros(i * i * 10));
            let _ = child.kill();
            child.wait().unwrap();
            Db::<Test>::create(&path)
                .unwrap_or_else(|e| panic!("kill after {} us: {e}", i * i * 10));
        }
    }

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
        // So does one that puts only the values already held (D29); a put
        // that changes something is recorded alone.
        db.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
            put(tx, TestTable::Data, "b", b"2");
            put(tx, TestTable::Other, "u", b"3");
            Ok(())
        })
        .unwrap();
        assert_eq!(db.revision(), rev);
        db.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
            put(tx, TestTable::Data, "b", b"2");
            put(tx, TestTable::Data, "c", b"5");
            Ok(())
        })
        .unwrap();
        let last = db.journal().last().unwrap().clone();
        assert_eq!(
            last.ops,
            vec![Op::Put {
                table: TestTable::Data,
                key: b"c".to_vec(),
                value: b"5".to_vec()
            }]
        );
        // A failed closure leaves nothing behind, in the database or the journal.
        let failed: Result<(), StoreError> = db.write(TestOrigin::Api, |tx| {
            put(tx, TestTable::Data, "x", b"9");
            Err(StoreError::NotLeader { leader: None })
        });
        assert!(failed.is_err());
        assert_eq!(db.journal().len(), 3);

        let replayed = Db::<Test>::replay(dir.path().join("b.db"), &db.journal()).unwrap();
        assert_eq!(db.dump().unwrap(), replayed.dump().unwrap());
        assert_eq!(replayed.revision(), 3);
    }

    /// Commits once the runtime's timer fires, as Raft's commit waits on
    /// its network.
    struct OnTimer(tokio::runtime::Handle);

    impl Replicator<Test> for OnTimer {
        fn propose(&self, db: &Db<Test>, ws: WriteSet<Test>) -> Result<u64, StoreError> {
            let wait = tokio::time::sleep(std::time::Duration::from_millis(50));
            crate::raft::wait_on(&self.0, wait);
            let rev = db.revision() + 1;
            db.apply(&ws, rev, |_| Ok(()))?;
            Ok(rev)
        }
    }

    /// A write waiting for the lane mustn't block its worker outright: with
    /// the only worker blocked, nothing drives the timer (in production,
    /// the I/O) that the lane's holder waits on, and neither finishes.
    #[test]
    fn a_write_waiting_for_the_lane_leaves_the_runtime_running() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Db::<Test>::create(dir.path().join("a.db")).unwrap());
        db.set_replicator(Arc::new(OnTimer(rt.handle().clone())));
        let (done, rx) = std::sync::mpsc::channel();
        for i in 0..2 {
            let (db, done) = (db.clone(), done.clone());
            rt.spawn(async move {
                let r = db.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
                    put(tx, TestTable::Data, &format!("k{i}"), b"v");
                    Ok(())
                });
                let _ = done.send(r.is_ok());
            });
        }
        let results: Vec<_> = (0..2)
            .map(|_| rx.recv_timeout(std::time::Duration::from_secs(10)))
            .collect();
        // Dropping a hung runtime would wait on its blocked threads.
        rt.shutdown_background();
        for r in results {
            assert_eq!(r, Ok(true), "a write hung or failed");
        }
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

    fn ws(ops: Vec<Op<Test>>) -> WriteSet<Test> {
        WriteSet {
            format: WRITE_SET_FORMAT,
            origin: TestOrigin::Api,
            ops,
        }
    }

    fn put_op(k: &str, v: &[u8]) -> Op<Test> {
        Op::Put {
            table: TestTable::Data,
            key: k.as_bytes().to_vec(),
            value: v.to_vec(),
        }
    }

    #[test]
    fn a_batch_lands_whole_and_is_announced_entry_by_entry() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::<Test>::create(dir.path().join("a.db")).unwrap();
        let mut rx = db.subscribe();
        let calls = std::cell::Cell::new(0);
        db.apply_batch(
            vec![
                (ws(vec![put_op("a", b"1")]), 5),
                // A blank or membership entry: no rows, no announcement.
                (ws(Vec::new()), 6),
                (
                    ws(vec![
                        put_op("a", b"2"),
                        Op::Delete {
                            table: TestTable::Data,
                            key: b"a".to_vec(),
                        },
                        put_op("b", b"3"),
                    ]),
                    7,
                ),
            ],
            |txn| {
                calls.set(calls.get() + 1);
                txn.open_table(definition::<Test>(Test::LOCAL))?
                    .insert(LAST_APPLIED, b"7".as_slice())?;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(db.revision(), 7);
        assert_eq!(
            db.dump().unwrap(),
            vec![(TestTable::Data, vec![("b".to_string(), b"3".to_vec())])]
        );
        let heard: Vec<u64> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|a| a.revision)
            .collect();
        assert_eq!(heard, vec![5, 7]);
        assert_eq!(
            db.snapshot_view().unwrap().last_applied.as_deref(),
            Some(b"7".as_slice())
        );

        // One entry it can't apply, and none of the batch lands.
        let mut bad = ws(vec![put_op("c", b"4")]);
        bad.format = WRITE_SET_FORMAT + 1;
        let r = db.apply_batch(vec![(ws(vec![put_op("d", b"5")]), 8), (bad, 9)], |_| Ok(()));
        assert!(matches!(r, Err(StoreError::Format(_))), "{r:?}");
        assert_eq!(db.revision(), 7);
        assert!(rx.try_recv().is_err());
        assert_eq!(db.dump().unwrap().len(), 1);
        let r = db.apply_batch(vec![(ws(vec![put_op("d", b"5")]), 8)], |_| {
            Err(StoreError::Io(std::io::Error::other("no")))
        });
        assert!(r.is_err());
        assert_eq!(
            db.dump().unwrap(),
            vec![(TestTable::Data, vec![("b".to_string(), b"3".to_vec())])]
        );
    }

    fn entry(i: u64) -> (WriteSet<Test>, u64) {
        (ws(vec![put_op(&format!("k{i:03}"), b"v")]), i)
    }

    #[test]
    fn checkpointed_applies_flush_every_hundred_entries_or_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::<Test>::create(dir.path().join("a.db")).unwrap();
        for i in 1..=99 {
            db.apply_checkpointed(vec![entry(i)], |_| Ok(())).unwrap();
        }
        assert_eq!(db.unflushed(), 99);
        // Not a second since the last checkpoint: not due.
        db.checkpoint_if_due().unwrap();
        assert_eq!(db.unflushed(), 99);
        // The batch that reaches 100 is itself the checkpoint.
        db.apply_checkpointed(vec![entry(100), entry(101)], |_| Ok(()))
            .unwrap();
        assert_eq!(db.unflushed(), 0);
        db.apply_checkpointed(vec![entry(102)], |_| Ok(())).unwrap();
        assert_eq!(db.unflushed(), 1);
        db.checkpoint().unwrap();
        assert_eq!(db.unflushed(), 0);
        // Durable applies leave nothing to flush.
        db.apply_batch(vec![entry(103)], |_| Ok(())).unwrap();
        assert_eq!(db.unflushed(), 0);
        // A second after the last checkpoint, the timer's is due, and so
        // is the next batch.
        db.apply_checkpointed(vec![entry(104)], |_| Ok(())).unwrap();
        std::thread::sleep(CHECKPOINT_INTERVAL);
        db.checkpoint_if_due().unwrap();
        assert_eq!(db.unflushed(), 0);
        std::thread::sleep(CHECKPOINT_INTERVAL);
        db.apply_checkpointed(vec![entry(105)], |_| Ok(())).unwrap();
        assert_eq!(db.unflushed(), 0);
        assert_eq!(db.revision(), 105);
        assert_eq!(db.dump().unwrap()[0].1.len(), 105);
    }

    /// The child of the test below: applies 10 entries and checkpoints,
    /// applies 5 more, and dies without closing the database.
    #[test]
    fn apply_in_a_child_and_die() {
        let Some(path) = std::env::var_os("GLIDEX_RAFTSTORE_CRASH_CHILD") else {
            return;
        };
        let db = Db::<Test>::create(Path::new(&path)).unwrap();
        for i in 1..=10 {
            db.apply_checkpointed(vec![entry(i)], |_| Ok(())).unwrap();
        }
        db.checkpoint().unwrap();
        for i in 11..=15 {
            db.apply_checkpointed(vec![entry(i)], |_| Ok(())).unwrap();
        }
        assert_eq!(db.unflushed(), 5);
        std::process::abort();
    }

    /// What a crash between checkpoints leaves: the state of the last one.
    #[test]
    fn a_crash_between_checkpoints_leaves_the_last_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::tests::apply_in_a_child_and_die",
                "--test-threads=1",
            ])
            .env("GLIDEX_RAFTSTORE_CRASH_CHILD", &path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "the child didn't die");
        let db = Db::<Test>::create(&path).unwrap();
        let rows = db.dump().unwrap();
        let keys: Vec<_> = rows[0].1.iter().map(|(k, _)| k.as_str()).collect();
        let want: Vec<String> = (1..=10).map(|i| format!("k{i:03}")).collect();
        assert_eq!(keys, want);
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
