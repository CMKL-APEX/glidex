//! The Raft log in its own ReDB file, `raft/log.redb` (§6.2), so purging the
//! log never touches the state machine. Entries are fsynced before they are
//! acknowledged: a ReDB commit is durable.

use super::types::*;
use openraft::storage::{LogFlushed, LogState, RaftLogReader, RaftLogStorage};
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::fmt::Debug;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Arc;

const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("log");
const KV: TableDefinition<&str, &[u8]> = TableDefinition::new("kv");
const VOTE: &str = "vote";
const COMMITTED: &str = "committed";
const PURGED: &str = "last_purged";

#[derive(Clone)]
pub struct LogStore {
    db: Arc<redb::Database>,
}

fn s<T>(r: Result<T, impl std::fmt::Display>) -> Result<T, StorageError> {
    r.map_err(io_err)
}

impl LogStore {
    pub fn open(path: &Path) -> Result<LogStore, StorageError> {
        if let Some(dir) = path.parent() {
            s(std::fs::create_dir_all(dir))?;
        }
        let db = s(redb::Database::create(path))?;
        let t = s(db.begin_write())?;
        s(t.open_table(LOG))?;
        s(t.open_table(KV))?;
        s(t.commit())?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(LogStore { db: Arc::new(db) })
    }

    fn kv<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StorageError> {
        let txn = s(self.db.begin_read())?;
        let t = s(txn.open_table(KV))?;
        match s(t.get(key))? {
            Some(v) => Ok(Some(s(serde_json::from_slice(v.value()))?)),
            None => Ok(None),
        }
    }

    fn set_kv<T: serde::Serialize>(&self, key: &str, v: Option<&T>) -> Result<(), StorageError> {
        let txn = s(self.db.begin_write())?;
        {
            let mut t = s(txn.open_table(KV))?;
            match v {
                Some(v) => {
                    s(t.insert(key, s(serde_json::to_vec(v))?.as_slice()))?;
                }
                None => {
                    s(t.remove(key))?;
                }
            }
        }
        s(txn.commit())
    }

    /// Whether this store has ever held a vote or an entry: a server whose
    /// log is gone must not rejoin as the same voter (D18).
    pub fn is_fresh(&self) -> Result<bool, StorageError> {
        let txn = s(self.db.begin_read())?;
        let log = s(txn.open_table(LOG))?;
        Ok(s(log.is_empty())? && self.kv::<Vote>(VOTE)?.is_none() && self.kv::<LogId>(PURGED)?.is_none())
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + openraft::OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<openraft::Entry<TypeConfig>>, StorageError> {
        let db = self.db.clone();
        let bounds = (range.start_bound().cloned(), range.end_bound().cloned());
        tokio::task::spawn_blocking(move || {
            let txn = s(db.begin_read())?;
            let t = s(txn.open_table(LOG))?;
            let mut out = Vec::new();
            for r in s(t.range(bounds))? {
                let (_, v) = s(r)?;
                out.push(s(serde_json::from_slice(v.value()))?);
            }
            Ok(out)
        })
        .await
        .map_err(io_err)?
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = LogStore;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError> {
        let last_purged_log_id: Option<LogId> = self.kv(PURGED)?;
        let txn = s(self.db.begin_read())?;
        let t = s(txn.open_table(LOG))?;
        let last = match s(t.last())? {
            Some((_, v)) => {
                let e: openraft::Entry<TypeConfig> = s(serde_json::from_slice(v.value()))?;
                Some(e.log_id)
            }
            None => last_purged_log_id,
        };
        Ok(LogState { last_purged_log_id, last_log_id: last })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote) -> Result<(), StorageError> {
        let (this, vote) = (self.clone(), vote.clone());
        tokio::task::spawn_blocking(move || this.set_kv(VOTE, Some(&vote))).await.map_err(io_err)?
    }

    async fn read_vote(&mut self) -> Result<Option<Vote>, StorageError> {
        self.kv(VOTE)
    }

    async fn save_committed(&mut self, committed: Option<LogId>) -> Result<(), StorageError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.set_kv(COMMITTED, committed.as_ref())).await.map_err(io_err)?
    }

    async fn read_committed(&mut self) -> Result<Option<LogId>, StorageError> {
        self.kv(COMMITTED)
    }

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>) -> Result<(), StorageError>
    where
        I: IntoIterator<Item = openraft::Entry<TypeConfig>> + openraft::OptionalSend,
        I::IntoIter: openraft::OptionalSend,
    {
        let rows: Vec<(u64, Vec<u8>)> = entries.into_iter().map(|e| Ok((e.log_id.index, serde_json::to_vec(&e)?))).collect::<Result<_, serde_json::Error>>().map_err(io_err)?;
        let db = self.db.clone();
        let res = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let txn = db.begin_write().map_err(|e| e.to_string())?;
            {
                let mut t = txn.open_table(LOG).map_err(|e| e.to_string())?;
                for (i, v) in &rows {
                    t.insert(*i, v.as_slice()).map_err(|e| e.to_string())?;
                }
            }
            txn.commit().map_err(|e| e.to_string())
        })
        .await
        .map_err(io_err)?;
        match res {
            Ok(()) => {
                callback.log_io_completed(Ok(()));
                Ok(())
            }
            Err(e) => {
                callback.log_io_completed(Err(std::io::Error::other(e.clone())));
                Err(io_err(e))
            }
        }
    }

    async fn truncate(&mut self, log_id: LogId) -> Result<(), StorageError> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let txn = s(db.begin_write())?;
            {
                let mut t = s(txn.open_table(LOG))?;
                let keys: Vec<u64> = s(t.range(log_id.index..))?.filter_map(|r| r.ok().map(|(k, _)| k.value())).collect();
                for k in keys {
                    s(t.remove(k))?;
                }
            }
            s(txn.commit())
        })
        .await
        .map_err(io_err)?
    }

    async fn purge(&mut self, log_id: LogId) -> Result<(), StorageError> {
        let db = self.db.clone();
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let txn = s(db.begin_write())?;
            {
                let mut t = s(txn.open_table(LOG))?;
                let keys: Vec<u64> = s(t.range(..=log_id.index))?.filter_map(|r| r.ok().map(|(k, _)| k.value())).collect();
                for k in keys {
                    s(t.remove(k))?;
                }
                let mut kv = s(txn.open_table(KV))?;
                s(kv.insert(PURGED, s(serde_json::to_vec(&log_id))?.as_slice()))?;
            }
            drop(this);
            s(txn.commit())
        })
        .await
        .map_err(io_err)?
    }
}
