//! A running Raft member: the log, the state machine, the transport, and the
//! [`Replicator`] that makes [`Db::write`] a Raft proposal (glidex
//! spec/clustering.md §6.1).

use super::*;
use crate::store::{Db, Replicator, Schema, StoreError, WriteSet};
use openraft::error::{ClientWriteError, RaftError};
use openraft::{BasicNode, Config, SnapshotPolicy};
use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Raft timing and log retention (glidex: `cluster.raft.*`, §10.2).
#[derive(Debug, Clone)]
pub struct RaftSettings {
    pub heartbeat_ms: u64,
    pub election_ms: (u64, u64),
    pub log_keep_entries: u64,
}

impl Default for RaftSettings {
    fn default() -> Self {
        RaftSettings {
            heartbeat_ms: 250,
            election_ms: (1000, 2000),
            log_keep_entries: 10_000,
        }
    }
}

/// Where a member finds the leader's address when the membership it has
/// applied doesn't say yet (glidex: the replicated `nodes` table).
pub type LeaderLookup = Box<dyn Fn(RaftId) -> Option<String> + Send + Sync>;

pub struct ClusterNode<S: Schema> {
    pub id: RaftId,
    pub db: Arc<Db<S>>,
    pub raft: Raft<S>,
    pub service: Arc<RaftService<S>>,
    pub log: LogStore<S>,
    pub dir: PathBuf,
    runtime: tokio::runtime::Handle,
    /// The term in which this node, as leader, has applied everything that
    /// was committed before it.
    ready_term: AtomicU64,
    leader_lookup: OnceLock<LeaderLookup>,
}

impl<S: Schema> ClusterNode<S> {
    /// Open the log in `dir/log.redb` and start Raft on `db`. A fresh node
    /// has no membership: it waits to be added (`add_learner`), or
    /// [`bootstrap`](Self::bootstrap)ped as a cluster of one.
    pub async fn start(
        db: Arc<Db<S>>,
        dir: &Path,
        id: RaftId,
        settings: &RaftSettings,
        transport: Arc<dyn Transport>,
    ) -> Result<Arc<ClusterNode<S>>, StoreError> {
        std::fs::create_dir_all(dir)?;
        let log = LogStore::open(&dir.join("log.redb"))
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
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
        let cfg = Arc::new(
            cfg.validate()
                .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?,
        );
        let net = NetworkFactory {
            transport,
            dir: dir.join("snapshots"),
            schema: PhantomData,
        };
        let raft = Raft::<S>::new(id, cfg, net, log.clone(), sm)
            .await
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
        let service = Arc::new(RaftService {
            raft: raft.clone(),
            dir: dir.join("snapshots"),
        });
        Ok(Arc::new(ClusterNode {
            id,
            db,
            raft,
            service,
            log,
            dir: dir.to_path_buf(),
            runtime: tokio::runtime::Handle::current(),
            ready_term: AtomicU64::new(0),
            leader_lookup: OnceLock::new(),
        }))
    }

    /// Where to look up the leader's address when the applied membership
    /// doesn't have it yet; set once, after [`start`](Self::start).
    pub fn set_leader_lookup(&self, f: LeaderLookup) {
        let _ = self.leader_lookup.set(f);
    }

    /// Make this node the only voter of a new cluster.
    pub async fn bootstrap(&self, addr: &str) -> Result<(), StoreError> {
        let mut m = BTreeMap::new();
        m.insert(self.id, BasicNode::new(addr));
        self.raft
            .initialize(m)
            .await
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))
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
        let m = self
            .raft
            .wait(Some(Duration::from_secs(30)))
            .metrics(|m| m.last_applied.is_some(), "the first entry applied")
            .await
            .map_err(|e| io(&e))?;
        let applied = m.last_applied.expect("waited for it");
        self.raft.trigger().snapshot().await.map_err(|e| io(&e))?;
        self.raft
            .wait(Some(Duration::from_secs(30)))
            .metrics(
                |m| m.snapshot.is_some_and(|s| s.index >= applied.index),
                "baseline snapshot",
            )
            .await
            .map_err(|e| io(&e))?;
        self.raft
            .trigger()
            .purge_log(applied.index)
            .await
            .map_err(|e| io(&e))?;
        self.raft
            .wait(Some(Duration::from_secs(30)))
            .metrics(
                |m| m.purged.is_some_and(|p| p.index >= applied.index),
                "baseline purge",
            )
            .await
            .map_err(|e| io(&e))?;
        Ok(())
    }

    /// Route `Db::write` through this node (§6.1).
    pub fn replicate(self: &Arc<Self>) {
        self.db
            .set_replicator(Arc::new(RaftReplicator { node: self.clone() }));
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
        let addr = m
            .membership_config
            .nodes()
            .find(|(id, _)| **id == l)
            .map(|(_, n)| n.addr.clone());
        if addr.is_some() {
            return addr;
        }
        // A member that has not applied the membership yet knows who leads
        // but not where: the application's records may already say.
        self.leader_lookup.get().and_then(|f| f(l))
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
        self.raft
            .add_learner(id, BasicNode::new(addr), true)
            .await
            .map(|_| ())
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))
    }

    /// Replace the voter set (joint consensus, one change at a time, §5.11).
    pub async fn set_voters(&self, voters: BTreeSet<RaftId>) -> Result<(), StoreError> {
        self.raft
            .change_membership(openraft::ChangeMembers::ReplaceAllVoters(voters), true)
            .await
            .map(|_| ())
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))
    }

    /// Take members out of the group altogether (not left behind as learners):
    /// voters first, by joint consensus, then whatever is still a learner.
    pub async fn remove_members(&self, ids: BTreeSet<RaftId>) -> Result<(), StoreError> {
        let io = |e: String| StoreError::Io(std::io::Error::other(e));
        let voting: BTreeSet<RaftId> = ids.intersection(&self.voters()).copied().collect();
        if !voting.is_empty() {
            self.raft
                .change_membership(openraft::ChangeMembers::RemoveVoters(voting), false)
                .await
                .map_err(|e| io(e.to_string()))?;
        }
        let learning: BTreeSet<RaftId> = ids.intersection(&self.learners()).copied().collect();
        if !learning.is_empty() {
            self.raft
                .change_membership(openraft::ChangeMembers::RemoveNodes(learning), false)
                .await
                .map_err(|e| io(e.to_string()))?;
        }
        Ok(())
    }

    pub fn voters(&self) -> BTreeSet<RaftId> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .voter_ids()
            .collect()
    }

    pub fn learners(&self) -> BTreeSet<RaftId> {
        self.raft
            .metrics()
            .borrow()
            .membership_config
            .membership()
            .learner_ids()
            .collect()
    }

    /// Stops Raft; the store refuses writes from then on, and is closed
    /// once the last handle to it is dropped.
    pub async fn shutdown(&self) {
        let _ = self.raft.shutdown().await;
        self.db.set_stopped();
    }

    fn not_leader(&self) -> StoreError {
        StoreError::NotLeader {
            leader: self.leader_addr(),
        }
    }
}

struct RaftReplicator<S: Schema> {
    node: Arc<ClusterNode<S>>,
}

impl<S: Schema> RaftReplicator<S> {
    /// `None` when the control plane is shutting down.
    fn block<F>(&self, f: F) -> Option<F::Output>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        wait_on(&self.node.runtime, f)
    }
}

impl<S: Schema> Replicator<S> for RaftReplicator<S> {
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
        let raft = n.raft.clone();
        self.block(async move { raft.ensure_linearizable().await })
            .and_then(|r| r.ok())
            .ok_or_else(|| n.not_leader())?;
        n.ready_term.store(m.current_term, Ordering::Release);
        Ok(())
    }

    fn can_write(&self) -> bool {
        let n = &self.node;
        let m = n.raft.metrics().borrow().clone();
        m.current_leader == Some(n.id)
    }

    fn propose(&self, _db: &Db<S>, ws: WriteSet<S>) -> Result<u64, StoreError> {
        let n = &self.node;
        let raft = n.raft.clone();
        let Some(written) = self.block(async move { raft.client_write(ws).await }) else {
            return Err(StoreError::Io(std::io::Error::other(
                "the control plane is shutting down",
            )));
        };
        match written {
            Ok(r) => Ok(r.log_id.index),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(f))) => {
                Err(StoreError::NotLeader {
                    leader: f.leader_node.map(|n| n.addr),
                })
            }
            Err(e) => Err(StoreError::Io(std::io::Error::other(e.to_string()))),
        }
    }
}

/// Run `f` on `rt` and wait for it from synchronous code (a store write
/// that must reach the leader or Raft). `None` when the runtime shut down
/// first: it drops its tasks, `f` with them.
///
/// Not `block_in_place(|| rt.block_on(f))`: a `block_in_place` that starts
/// once the runtime is shutting down can't hand its worker's core on, so
/// the I/O and time drivers are never driven or shut down again, `f` never
/// wakes, and the shutdown waits on this thread for ever.
pub fn wait_on<F>(rt: &tokio::runtime::Handle, f: F) -> Option<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    rt.spawn(async move {
        let _ = tx.send(f.await);
    });
    tokio::task::block_in_place(|| rx.recv().ok())
}
