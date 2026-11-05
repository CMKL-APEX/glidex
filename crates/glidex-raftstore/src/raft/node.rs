//! A running Raft member: the log, the state machine, the transport, and the
//! [`Replicator`] that makes [`Db::write`] a Raft proposal (glidex
//! spec/14-clustering.md §6.1).

use super::*;
use crate::store::{Db, Replicator, Schema, StoreError, WriteSet};
use openraft::error::{ClientWriteError, RaftError};
use openraft::{BasicNode, Config, SnapshotPolicy};
use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

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
            log_keep_entries: 70_000,
        }
    }
}

/// Purge the log to `keep` entries behind the snapshot. openraft purges
/// after it builds a snapshot, which `LogsSinceLast` asks for only once that
/// many entries were applied since the last one; a restart makes the live
/// database the snapshot and starts that count again. A member restarted
/// more often than that never purged: the prism servers kept every entry,
/// 27,000 in three days (2026-11-03). Run once the member has loaded its
/// state: its metrics then show the snapshot.
async fn trim_log<S: Schema>(raft: Raft<S>, keep: u64) {
    let Ok(m) = raft
        .wait(Some(Duration::from_secs(60)))
        .metrics(|m| m.snapshot.is_some(), "a snapshot to trim behind")
        .await
    else {
        return;
    };
    let Some(snapshot) = m.snapshot else { return };
    let upto = snapshot.index.saturating_sub(keep);
    if upto > m.purged.map(|l| l.index).unwrap_or(0) {
        let _ = raft.trigger().purge_log(upto).await;
    }
}

/// How often a stall is reported again while it lasts (D30).
const STALL_REPEAT: Duration = Duration::from_secs(300);

/// One look at a member's Raft metrics, as the stall watch needs it.
#[derive(Debug, Clone, Copy)]
struct RaftView {
    applied: Option<u64>,
    last_log: Option<u64>,
    leader: Option<RaftId>,
    term: u64,
}

#[derive(Debug, PartialEq)]
enum StallEvent {
    Began,
    Still,
    Ended(Duration),
}

/// Whether a member's log stopped (spec/14-clustering.md §6.2a, D30):
/// nothing applied for `limit` while entries waited, no leader was known or
/// the term moved. Prism's servers held elections for two hours on
/// 2026-11-03 with nothing applied, and nobody knew until the UI went down.
#[derive(Debug, Default)]
struct StallWatch {
    last: Option<RaftView>,
    /// When the trouble began, unless an entry was applied since.
    trouble: Option<(Instant, SystemTime)>,
    /// When the current stall was last reported.
    reported: Option<Instant>,
}

impl StallWatch {
    fn check(
        &mut self,
        now: Instant,
        wall: SystemTime,
        v: RaftView,
        limit: Duration,
    ) -> Option<StallEvent> {
        let last = self.last.replace(v);
        let mut ended = None;
        if last.is_some_and(|l| v.applied > l.applied) {
            if self.reported.take().is_some() {
                ended = self.trouble.map(|(t, _)| StallEvent::Ended(now - t));
            }
            self.trouble = None;
        }
        let trouble =
            v.last_log > v.applied || v.leader.is_none() || last.is_some_and(|l| l.term != v.term);
        if trouble && self.trouble.is_none() {
            self.trouble = Some((now, wall));
        }
        if ended.is_some() {
            return ended;
        }
        let (began, _) = self.trouble?;
        if now - began < limit {
            return None;
        }
        match self.reported {
            None => {
                self.reported = Some(now);
                Some(StallEvent::Began)
            }
            Some(r) if now - r >= STALL_REPEAT => {
                self.reported = Some(now);
                Some(StallEvent::Still)
            }
            Some(_) => None,
        }
    }

    fn stalled_since(&self) -> Option<SystemTime> {
        self.reported.and(self.trouble.map(|(_, w)| w))
    }
}

/// Run the stall watch each second until Raft stops; `stalled` holds
/// when the current stall began.
async fn watch_stalls<S: Schema>(
    raft: Raft<S>,
    stalled: Arc<Mutex<Option<SystemTime>>>,
    limit: Duration,
) {
    let rx = raft.metrics();
    drop(raft);
    let mut w = StallWatch::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        if rx.has_changed().is_err() {
            return;
        }
        let v = {
            let m = rx.borrow();
            RaftView {
                applied: m.last_applied.map(|l| l.index),
                last_log: m.last_log_index,
                leader: m.current_leader,
                term: m.current_term,
            }
        };
        let event = w.check(Instant::now(), SystemTime::now(), v, limit);
        *stalled.lock().unwrap() = w.stalled_since();
        let waiting = v
            .last_log
            .unwrap_or(0)
            .saturating_sub(v.applied.unwrap_or(0));
        let leader = v.leader.map_or("none".to_string(), |l| l.to_string());
        let lasted = |since: Option<SystemTime>| {
            since
                .and_then(|t| t.elapsed().ok())
                .map_or(0, |d| d.as_secs())
        };
        match event {
            Some(StallEvent::Began | StallEvent::Still) => tracing::warn!(
                "the Raft log has applied nothing for {}s: term {}, leader {leader}, {waiting} entries wait",
                lasted(w.stalled_since()),
                v.term
            ),
            Some(StallEvent::Ended(d)) => tracing::info!(
                "the Raft log applies again, after a stall of {}s",
                d.as_secs()
            ),
            None => {}
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
    /// When the current stall began (D30).
    stalled: Arc<Mutex<Option<SystemTime>>>,
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
            // Recording a position costs nothing: often, so the log stays
            // near `log_keep_entries` rather than twice that.
            snapshot_policy: SnapshotPolicy::LogsSinceLast(
                (settings.log_keep_entries / 10).max(100),
            ),
            max_in_snapshot_log_to_keep: settings.log_keep_entries,
            purge_batch_size: (settings.log_keep_entries / 10).max(1),
            // So that an append stays within what members take.
            max_payload_entries: crate::store::MAX_APPEND_ENTRIES,
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
        tokio::spawn(trim_log(raft.clone(), settings.log_keep_entries));
        let stalled = Arc::new(Mutex::new(None));
        tokio::spawn(watch_stalls(
            raft.clone(),
            stalled.clone(),
            Duration::from_millis(settings.election_ms.1) * COMMIT_ELECTIONS,
        ));
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
            stalled,
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

    /// When this member's log stopped applying, while it stays stopped
    /// (spec/14-clustering.md §6.2a).
    pub fn stalled_since(&self) -> Option<SystemTime> {
        *self.stalled.lock().unwrap()
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

/// How long a write waits for its leader's read barrier, and for its entry
/// to be committed and applied, in election timeouts. openraft answers a
/// proposal only once its entry is applied or truncated, and its barrier
/// waits for the apply without a limit: a member that lost its leadership
/// while no other leader commits waited for ever, holding the write lane,
/// and every other write of that member queued behind it (prism1 and
/// prism2's control planes never finished starting on 2026-11-03).
const BARRIER_ELECTIONS: u32 = 4;
const COMMIT_ELECTIONS: u32 = 10;

impl<S: Schema> RaftReplicator<S> {
    /// `None` when the control plane is shutting down; `Some(None)` when
    /// `f` took longer than `limit`.
    fn block<F>(&self, f: F, limit: Duration) -> Option<Option<F::Output>>
    where
        F: std::future::Future + Send + 'static,
        F::Output: Send + 'static,
    {
        wait_on(&self.node.runtime, async move {
            tokio::time::timeout(limit, f).await.ok()
        })
    }

    fn elections(&self, n: u32) -> Duration {
        Duration::from_millis(self.node.raft.config().election_timeout_max) * n
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
        // Not confirmed in time: as good as not the leader; the caller retries.
        let raft = n.raft.clone();
        self.block(
            async move { raft.ensure_linearizable().await },
            self.elections(BARRIER_ELECTIONS),
        )
        .flatten()
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
        let limit = self.elections(COMMIT_ELECTIONS);
        let written = match self.block(async move { raft.client_write(ws).await }, limit) {
            Some(Some(w)) => w,
            Some(None) => {
                return Err(StoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "the write was not committed within {}s (no leader commits); it may still be applied later",
                        limit.as_secs()
                    ),
                )));
            }
            None => {
                return Err(StoreError::Io(std::io::Error::other(
                    "the control plane is shutting down",
                )));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: Duration = Duration::from_secs(60);

    fn view(applied: u64, last_log: u64, leader: Option<RaftId>, term: u64) -> RaftView {
        RaftView {
            applied: Some(applied),
            last_log: Some(last_log),
            leader,
            term,
        }
    }

    /// Feed one view a second for `secs` seconds from `start`; the events seen.
    fn run(
        w: &mut StallWatch,
        t0: Instant,
        start: u64,
        secs: u64,
        f: impl Fn(u64) -> RaftView,
    ) -> Vec<(u64, StallEvent)> {
        (start..start + secs)
            .filter_map(|s| {
                let at = t0 + Duration::from_secs(s);
                w.check(
                    at,
                    SystemTime::UNIX_EPOCH + Duration::from_secs(s),
                    f(s),
                    LIMIT,
                )
                .map(|e| (s, e))
            })
            .collect()
    }

    #[test]
    fn an_idle_member_with_a_leader_never_stalls() {
        let mut w = StallWatch::default();
        let t0 = Instant::now();
        assert!(run(&mut w, t0, 0, 3600, |_| view(10, 10, Some(1), 3)).is_empty());
        assert_eq!(w.stalled_since(), None);
    }

    #[test]
    fn entries_that_wait_are_a_stall_after_the_limit_until_one_is_applied() {
        let mut w = StallWatch::default();
        let t0 = Instant::now();
        assert!(run(&mut w, t0, 0, 5, |_| view(10, 10, Some(1), 3)).is_empty());
        // From second 5 an entry waits and nothing is applied.
        let events = run(&mut w, t0, 5, 400, |_| view(10, 11, Some(1), 3));
        assert_eq!(
            events,
            vec![(65, StallEvent::Began), (365, StallEvent::Still)]
        );
        assert_eq!(
            w.stalled_since(),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(5))
        );
        let events = run(&mut w, t0, 405, 1, |_| view(11, 11, Some(1), 3));
        assert_eq!(
            events,
            vec![(405, StallEvent::Ended(Duration::from_secs(400)))]
        );
        assert_eq!(w.stalled_since(), None);
    }

    #[test]
    fn elections_without_end_are_a_stall_though_nothing_waits() {
        // A follower that the leaders never reach with their entries: it
        // sees a new term, and often a leader, but applies nothing.
        let mut w = StallWatch::default();
        let t0 = Instant::now();
        let events = run(&mut w, t0, 0, 120, |s| {
            view(10, 10, (s % 30 > 5).then_some(2), 3 + s / 30)
        });
        assert_eq!(events, vec![(60, StallEvent::Began)]);
    }

    #[test]
    fn no_leader_is_a_stall() {
        let mut w = StallWatch::default();
        let t0 = Instant::now();
        let events = run(&mut w, t0, 0, 61, |_| view(10, 10, None, 3));
        assert_eq!(events, vec![(60, StallEvent::Began)]);
    }

    #[test]
    fn a_member_that_catches_up_or_recovers_in_time_is_not_stalled() {
        let mut w = StallWatch::default();
        let t0 = Instant::now();
        // Behind by 100 entries, applying one a second.
        assert!(run(&mut w, t0, 0, 100, |s| view(s, 100, Some(1), 3)).is_empty());
        // An election, and the new leader's first entry applied 50 s later.
        assert!(run(&mut w, t0, 100, 50, |_| view(100, 100, None, 4)).is_empty());
        assert!(run(&mut w, t0, 150, 100, |_| view(101, 101, Some(2), 4)).is_empty());
        assert_eq!(w.stalled_since(), None);
    }
}
