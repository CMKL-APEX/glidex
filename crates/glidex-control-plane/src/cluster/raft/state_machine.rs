//! The state machine is the existing ReDB database (D1): applying an entry
//! is a pure byte operation, done in the same transaction that advances
//! `last_applied` (§6.2). A crash therefore never applies an entry twice or
//! skips one.

use super::types::*;
use crate::store::{Db, StoreError, TableId, WriteSet, LAST_APPLIED, LAST_MEMBERSHIP};
use openraft::storage::{RaftSnapshotBuilder, RaftStateMachine, Snapshot};
use openraft::{EntryPayload, StoredMembership};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct StateMachine {
    db: Arc<Db>,
    /// Where received snapshots land before they are installed.
    dir: PathBuf,
}

fn s<T, E: std::fmt::Display>(r: Result<T, E>) -> Result<T, StorageError> {
    r.map_err(io_err)
}

fn put_meta(txn: &redb::WriteTransaction, key: &str, value: &[u8]) -> Result<(), StoreError> {
    txn.open_table(TableId::RaftMeta.definition())?.insert(key, value)?;
    Ok(())
}

impl StateMachine {
    pub fn new(db: Arc<Db>, dir: PathBuf) -> StateMachine {
        StateMachine { db, dir }
    }

    fn position(&self) -> Result<(Option<LogId>, StoredMembership<RaftId, openraft::BasicNode>), StorageError> {
        let view = s(self.db.snapshot_view())?;
        let applied = match &view.last_applied {
            Some(b) => Some(s(serde_json::from_slice::<LogId>(b))?),
            None => None,
        };
        let membership = match &view.last_membership {
            Some(b) => s(serde_json::from_slice(b))?,
            None => StoredMembership::default(),
        };
        Ok((applied, membership))
    }

    fn snapshot_now(&self) -> Result<Option<Snapshot<TypeConfig>>, StorageError> {
        let view = s(self.db.snapshot_view())?;
        let Some(applied) = &view.last_applied else { return Ok(None) };
        let last_log_id: LogId = s(serde_json::from_slice(applied))?;
        let last_membership = match &view.last_membership {
            Some(b) => s(serde_json::from_slice(b))?,
            None => StoredMembership::default(),
        };
        let meta = SnapshotMeta { snapshot_id: format!("{}-{}", last_log_id.leader_id, last_log_id.index), last_log_id: Some(last_log_id), last_membership };
        Ok(Some(Snapshot { meta, snapshot: Box::new(SnapshotHandle { view: Some(view), file: None }) }))
    }
}

pub struct Builder(StateMachine);

impl RaftSnapshotBuilder<TypeConfig> for Builder {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError> {
        // A snapshot is a view of the live database (§6.2): there is nothing
        // to build ahead of time, so this only records a position.
        match self.0.snapshot_now()? {
            Some(sn) => Ok(sn),
            None => Ok(Snapshot { meta: SnapshotMeta::default(), snapshot: Box::new(SnapshotHandle::default()) }),
        }
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotBuilder = Builder;

    async fn applied_state(&mut self) -> Result<(Option<LogId>, StoredMembership<RaftId, openraft::BasicNode>), StorageError> {
        self.position()
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<()>, StorageError>
    where
        I: IntoIterator<Item = openraft::Entry<TypeConfig>> + openraft::OptionalSend,
        I::IntoIter: openraft::OptionalSend,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::with_capacity(entries.len());
            for e in entries {
                let log_id_json = s(serde_json::to_vec(&e.log_id))?;
                let (ws, membership) = match e.payload {
                    EntryPayload::Blank => (WriteSet { format: crate::store::WRITE_SET_FORMAT, origin: crate::store::Origin::System, ops: Vec::new() }, None),
                    EntryPayload::Normal(ws) => (ws, None),
                    EntryPayload::Membership(m) => {
                        let stored = StoredMembership::new(Some(e.log_id), m);
                        (WriteSet { format: crate::store::WRITE_SET_FORMAT, origin: crate::store::Origin::System, ops: Vec::new() }, Some(s(serde_json::to_vec(&stored))?))
                    }
                };
                s(db.apply(&ws, e.log_id.index, |txn| {
                    put_meta(txn, LAST_APPLIED, &log_id_json)?;
                    if let Some(m) = &membership {
                        put_meta(txn, LAST_MEMBERSHIP, m)?;
                    }
                    Ok(())
                }))?;
                out.push(());
            }
            Ok(out)
        })
        .await
        .map_err(io_err)?
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        Builder(self.clone())
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<SnapshotHandle>, StorageError> {
        Ok(Box::new(SnapshotHandle::default()))
    }

    async fn install_snapshot(&mut self, meta: &SnapshotMeta, snapshot: Box<SnapshotHandle>) -> Result<(), StorageError> {
        let path = snapshot.file.clone().ok_or_else(|| io_err("snapshot without data"))?;
        let db = self.db.clone();
        let meta = meta.clone();
        let _ = &self.dir;
        tokio::task::spawn_blocking(move || {
            let mut f = std::io::BufReader::new(s(std::fs::File::open(&path))?);
            let applied = s(serde_json::to_vec(&meta.last_log_id))?;
            let membership = s(serde_json::to_vec(&meta.last_membership))?;
            let index = meta.last_log_id.map(|l| l.index).unwrap_or(0);
            s(db.install_dump(&mut f, index, |txn| {
                // `last_applied` of a snapshot with no log id is "none".
                match &meta.last_log_id {
                    Some(l) => put_meta(txn, LAST_APPLIED, &s2(serde_json::to_vec(l))?)?,
                    None => {
                        let _ = &applied;
                        txn.open_table(TableId::RaftMeta.definition())?.remove(LAST_APPLIED)?;
                    }
                }
                put_meta(txn, LAST_MEMBERSHIP, &membership)
            }))?;
            let _ = std::fs::remove_file(&path);
            Ok(())
        })
        .await
        .map_err(io_err)?
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<TypeConfig>>, StorageError> {
        self.snapshot_now()
    }
}

fn s2<T>(r: Result<T, serde_json::Error>) -> Result<T, StoreError> {
    r.map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))
}
