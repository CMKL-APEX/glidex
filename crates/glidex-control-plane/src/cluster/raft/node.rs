//! A running Raft member: the log, the state machine, the transport, and the
//! [`Replicator`] that makes `Db::write` a Raft proposal (§6.1).

use super::*;
use crate::store::{Db, Replicator, StoreError, WriteSet};
use openraft::error::{ClientWriteError, RaftError};
use openraft::{BasicNode, Config, SnapshotPolicy};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// `cluster.raft.*` (§10.2).
#[derive(Debug, Clone)]
pub struct RaftSettings {
    pub heartbeat_ms: u64,
    pub election_ms: (u64, u64),
    pub log_keep_entries: u64,
}

impl Default for RaftSettings {
    fn default() -> Self {
        RaftSettings { heartbeat_ms: 250, election_ms: (1000, 2000), log_keep_entries: 10_000 }
    }
}

pub struct ClusterNode {
    pub id: RaftId,
    pub db: Arc<Db>,
    pub raft: GxRaft,
    pub service: Arc<RaftService>,
    pub log: LogStore,
    pub dir: PathBuf,
    runtime: tokio::runtime::Handle,
    /// The term in which this node, as leader, has applied everything that
    /// was committed before it.
    ready_term: AtomicU64,
}

impl ClusterNode {
    /// Open the log in `dir/log.redb` and start Raft on `db`. A fresh node
    /// has no membership: it waits to be added (`add_learner`), or
    /// [`bootstrap`](Self::bootstrap)ped as a cluster of one.
    pub async fn start(db: Arc<Db>, dir: &Path, id: RaftId, settings: &RaftSettings, transport: Arc<dyn Transport>) -> Result<Arc<ClusterNode>, StoreError> {
        std::fs::create_dir_all(dir)?;
        let log = LogStore::open(&dir.join("log.redb")).map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
        let sm = StateMachine::new(db.clone(), dir.join("snapshots"));
        let cfg = Config {
            heartbeat_interval: settings.heartbeat_ms,
            election_timeout_min: settings.election_ms.0,
            election_timeout_max: settings.election_ms.1,
            // Snapshots are views of the live database, built when a follower
            // needs one (§6.2); the policy only tells Raft when it may purge.
            snapshot_policy: SnapshotPolicy::LogsSinceLast(settings.log_keep_entries.max(100)),
            max_in_snapshot_log_to_keep: settings.log_keep_entries,
            purge_batch_size: (settings.log_keep_entries / 10).max(1),
            ..Default::default()
        };
        let cfg = Arc::new(cfg.validate().map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?);
        let net = NetworkFactory { transport, dir: dir.join("snapshots") };
        let raft = GxRaft::new(id, cfg, net, log.clone(), sm).await.map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
        let service = Arc::new(RaftService { raft: raft.clone(), dir: dir.join("snapshots") });
        Ok(Arc::new(ClusterNode { id, db, raft, service, log, dir: dir.to_path_buf(), runtime: tokio::runtime::Handle::current(), ready_term: AtomicU64::new(0) }))
    }

    /// Make this node the only voter of a new cluster.
    pub async fn bootstrap(&self, addr: &str) -> Result<(), StoreError> {
        let mut m = BTreeMap::new();
        m.insert(self.id, BasicNode::new(addr));
        self.raft.initialize(m).await.map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))
    }

    /// Start the log at a snapshot boundary: purge everything up to what is
    /// applied, so that a member that joins later is always brought up from a
    /// snapshot of the database. The database holds state (a standalone
    /// host's records, a recovered backup) that was never in the log; without
    /// this a new member would get only the log's entries and miss it
    /// (§5.1 step 3: "create the initial snapshot at index 0").
    pub async fn seal_baseline(&self) -> Result<(), StoreError> {
        let io = |e: &dyn std::fmt::Display| StoreError::Io(std::io::Error::other(e.to_string()));
        // The bootstrap entry must be applied first.
        let m = self.raft.wait(Some(Duration::from_secs(30))).metrics(|m| m.last_applied.is_some(), "the first entry applied").await.map_err(|e| io(&e))?;
        let applied = m.last_applied.expect("waited for it");
        self.raft.trigger().snapshot().await.map_err(|e| io(&e))?;
        self.raft.wait(Some(Duration::from_secs(30))).metrics(|m| m.snapshot.is_some_and(|s| s.index >= applied.index), "baseline snapshot").await.map_err(|e| io(&e))?;
        self.raft.trigger().purge_log(applied.index).await.map_err(|e| io(&e))?;
        self.raft.wait(Some(Duration::from_secs(30))).metrics(|m| m.purged.is_some_and(|p| p.index >= applied.index), "baseline purge").await.map_err(|e| io(&e))?;
        Ok(())
    }

    /// Route `Db::write` through this node (§6.1).
    pub fn replicate(self: &Arc<Self>) {
        self.db.set_replicator(Arc::new(RaftReplicator { node: self.clone() }));
    }

    pub fn is_leader(&self) -> bool {
        self.raft.metrics().borrow().current_leader == Some(self.id)
    }

    pub fn leader(&self) -> Option<RaftId> {
        self.raft.metrics().borrow().current_leader
    }

    /// The leader's advertise address, if known.
    pub fn leader_addr(&self) -> Option<String> {
        let m = self.raft.metrics().borrow().clone();
        let l = m.current_leader?;
        let addr = m.membership_config.nodes().find(|(id, _)| **id == l).map(|(_, n)| n.addr.clone());
        if addr.is_some() {
            return addr;
        }
        // A member that has not applied the membership yet knows who leads
        // but not where: the replicated `nodes` table may already say.
        crate::node::NodeStore::new(self.db.clone()).list().ok()?.into_iter().find(|n| n.status.raft_id == Some(l)).and_then(|n| n.status.advertise).map(|a| a.to_string())
    }

    /// Wait for a leader to be elected, as seen by this node.
    pub async fn wait_for_leader(&self, timeout: Duration) -> Result<RaftId, StoreError> {
        let m = self
            .raft
            .wait(Some(timeout))
            .metrics(|m| m.current_leader.is_some(), "a leader")
            .await
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
        Ok(m.current_leader.unwrap())
    }

    pub async fn add_learner(&self, id: RaftId, addr: &str) -> Result<(), StoreError> {
        self.raft.add_learner(id, BasicNode::new(addr), true).await.map(|_| ()).map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))
    }

    /// Replace the voter set (joint consensus, one change at a time, §5.11).
    pub async fn set_voters(&self, voters: BTreeSet<RaftId>) -> Result<(), StoreError> {
        self.raft
            .change_membership(openraft::ChangeMembers::ReplaceAllVoters(voters), true)
            .await
            .map(|_| ())
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))
    }

    pub fn voters(&self) -> BTreeSet<RaftId> {
        self.raft.metrics().borrow().membership_config.membership().voter_ids().collect()
    }

    pub fn learners(&self) -> BTreeSet<RaftId> {
        self.raft.metrics().borrow().membership_config.membership().learner_ids().collect()
    }

    pub async fn shutdown(&self) {
        let _ = self.raft.shutdown().await;
    }

    fn not_leader(&self) -> StoreError {
        StoreError::NotLeader { leader: self.leader_addr() }
    }
}

struct RaftReplicator {
    node: Arc<ClusterNode>,
}

impl RaftReplicator {
    fn block<F: std::future::Future>(&self, f: F) -> F::Output {
        tokio::task::block_in_place(|| self.node.runtime.block_on(f))
    }
}

impl Replicator for RaftReplicator {
    fn before_write(&self) -> Result<(), StoreError> {
        let n = &self.node;
        let m = n.raft.metrics().borrow().clone();
        if m.current_leader != Some(n.id) {
            return Err(n.not_leader());
        }
        if n.ready_term.load(Ordering::Acquire) == m.current_term {
            return Ok(());
        }
        // A new leader first applies everything committed before it, so the
        // closure of the first write reads the state its write set lands on.
        self.block(n.raft.ensure_linearizable()).map_err(|_| n.not_leader())?;
        n.ready_term.store(m.current_term, Ordering::Release);
        Ok(())
    }

    fn can_write(&self) -> bool {
        let n = &self.node;
        let m = n.raft.metrics().borrow().clone();
        m.current_leader == Some(n.id)
    }

    fn propose(&self, _db: &Db, ws: WriteSet) -> Result<u64, StoreError> {
        let n = &self.node;
        match self.block(n.raft.client_write(ws)) {
            Ok(r) => Ok(r.log_id.index),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(f))) => Err(StoreError::NotLeader { leader: f.leader_node.map(|n| n.addr) }),
            Err(e) => Err(StoreError::Io(std::io::Error::other(e.to_string()))),
        }
    }
}
