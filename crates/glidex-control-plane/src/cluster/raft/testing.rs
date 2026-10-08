//! An in-process Raft cluster with fault injection (spec/clustering.md §15):
//! messages between members are routed through a [`Mesh`] that can drop,
//! delay and reorder them, partition members and take them down.

use super::*;
use crate::store::{Db, Origin, StoreError, TableId};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Faults {
    /// Directed pairs whose messages are dropped.
    blocked: HashSet<(RaftId, RaftId)>,
    down: HashSet<RaftId>,
    /// Extra latency on every message, plus up to `jitter_ms` more at random
    /// (which reorders messages that are in flight together).
    delay_ms: u64,
    jitter_ms: u64,
    /// Percentage of messages dropped.
    loss_pct: u8,
}

#[derive(Default)]
pub struct Mesh {
    services: Mutex<HashMap<RaftId, Arc<RaftService>>>,
    faults: Mutex<Faults>,
}

impl Mesh {
    pub fn new() -> Arc<Mesh> {
        Arc::new(Mesh::default())
    }

    pub fn register(&self, id: RaftId, s: Arc<RaftService>) {
        self.services.lock().unwrap().insert(id, s);
    }

    pub fn isolate(&self, id: RaftId, all: &[RaftId]) {
        let mut f = self.faults.lock().unwrap();
        for &o in all.iter().filter(|o| **o != id) {
            f.blocked.insert((id, o));
            f.blocked.insert((o, id));
        }
    }

    pub fn heal(&self) {
        let mut f = self.faults.lock().unwrap();
        f.blocked.clear();
        f.down.clear();
    }

    pub fn set_down(&self, id: RaftId, down: bool) {
        let mut f = self.faults.lock().unwrap();
        if down {
            f.down.insert(id);
        } else {
            f.down.remove(&id);
        }
    }

    pub fn set_latency(&self, delay_ms: u64, jitter_ms: u64) {
        let mut f = self.faults.lock().unwrap();
        f.delay_ms = delay_ms;
        f.jitter_ms = jitter_ms;
    }

    pub fn set_loss(&self, pct: u8) {
        self.faults.lock().unwrap().loss_pct = pct;
    }

    pub fn transport(self: &Arc<Self>, from: RaftId) -> Arc<dyn Transport> {
        Arc::new(MeshTransport { mesh: self.clone(), from })
    }

    async fn route(&self, from: RaftId, to: RaftId) -> Result<Arc<RaftService>, TransportError> {
        let (delay, drop) = {
            let f = self.faults.lock().unwrap();
            let r = rand_u64();
            let jitter = if f.jitter_ms > 0 { r % (f.jitter_ms + 1) } else { 0 };
            let lost = f.loss_pct > 0 && (r >> 32) % 100 < f.loss_pct as u64;
            let cut = f.blocked.contains(&(from, to)) || f.down.contains(&from) || f.down.contains(&to) || lost;
            (f.delay_ms + jitter, cut)
        };
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        if drop {
            return Err(TransportError(format!("{from} -> {to}: dropped")));
        }
        self.services.lock().unwrap().get(&to).cloned().ok_or_else(|| TransportError(format!("{to} is not running")))
    }
}

fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    u64::from_le_bytes(b)
}

struct MeshTransport {
    mesh: Arc<Mesh>,
    from: RaftId,
}

#[async_trait::async_trait]
impl Transport for MeshTransport {
    async fn call(&self, target: RaftId, _addr: &str, rpc: Rpc, body: Vec<u8>) -> Result<Vec<u8>, TransportError> {
        let svc = self.mesh.route(self.from, target).await?;
        svc.handle(rpc, &body).await
    }

    async fn snapshot(&self, target: RaftId, _addr: &str, header: Vec<u8>, data: PathBuf) -> Result<Vec<u8>, TransportError> {
        let svc = self.mesh.route(self.from, target).await?;
        // The receiver owns (and removes) its copy.
        let copy = data.with_extension("recv");
        std::fs::copy(&data, &copy).map_err(|e| TransportError(e.to_string()))?;
        svc.handle_snapshot(&header, copy).await
    }
}

pub struct TestNode {
    pub id: RaftId,
    pub node: Arc<ClusterNode>,
    pub db: Arc<Db>,
    pub dir: PathBuf,
}

pub struct TestCluster {
    pub mesh: Arc<Mesh>,
    pub nodes: Vec<TestNode>,
    pub root: tempfile::TempDir,
    pub settings: RaftSettings,
}

pub fn fast_settings() -> RaftSettings {
    RaftSettings { heartbeat_ms: 100, election_ms: (800, 1600), log_keep_entries: 100 }
}

impl TestCluster {
    /// `n` members, the first bootstrapped, the rest added and promoted.
    pub async fn new(n: usize) -> TestCluster {
        let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_test_writer().try_init();
        let mut c = TestCluster { mesh: Mesh::new(), nodes: Vec::new(), root: tempfile::tempdir().unwrap(), settings: fast_settings() };
        c.add_started(1).await;
        c.nodes[0].node.bootstrap("n1").await.unwrap();
        c.nodes[0].node.wait_for_leader(Duration::from_secs(10)).await.unwrap();
        c.nodes[0].node.replicate();
        c.nodes[0].node.seal_baseline().await.unwrap();
        for i in 2..=n {
            c.add_started(i as u64).await;
        }
        let ids: BTreeSet<RaftId> = (1..=n as u64).collect();
        for i in 2..=n as u64 {
            c.nodes[0].node.add_learner(i, &format!("n{i}")).await.unwrap();
        }
        if n > 1 {
            c.nodes[0].node.set_voters(ids).await.unwrap();
        }
        for t in &c.nodes {
            t.node.replicate();
        }
        c
    }

    /// Start a member that is not yet part of the cluster.
    pub async fn add_started(&mut self, id: RaftId) -> usize {
        let dir = self.root.path().join(format!("n{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Arc::new(Db::create(dir.join("glidex.db")).unwrap());
        let node = ClusterNode::start(db.clone(), &dir.join("raft"), id, &self.settings, self.mesh.transport(id)).await.unwrap();
        self.mesh.register(id, node.service.clone());
        self.nodes.push(TestNode { id, node, db, dir });
        self.nodes.len() - 1
    }

    pub fn ids(&self) -> Vec<RaftId> {
        self.nodes.iter().map(|n| n.id).collect()
    }

    pub fn by_id(&self, id: RaftId) -> &TestNode {
        self.nodes.iter().find(|n| n.id == id).unwrap()
    }

    /// The member that every running member agrees is the leader.
    pub async fn leader(&self, among: &[RaftId]) -> RaftId {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let views: Vec<Option<RaftId>> = among.iter().map(|i| self.by_id(*i).node.leader()).collect();
            if let Some(Some(l)) = views.first() {
                if views.iter().all(|v| *v == Some(*l)) && among.contains(l) {
                    return *l;
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "no agreed leader among {among:?}: {views:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stop a member for good (its database stays).
    pub async fn kill(&self, id: RaftId) {
        self.mesh.set_down(id, true);
        self.by_id(id).node.shutdown().await;
    }

    /// One write through member `id`'s store: sets `key` in the `meta` table.
    pub fn put(&self, id: RaftId, key: &str, value: &str) -> Result<(), StoreError> {
        let db = self.by_id(id).db.clone();
        let (k, v) = (key.to_string(), value.to_string());
        db.write(Origin::Api, move |tx| -> Result<(), StoreError> {
            tx.open_table(TableId::Meta.definition())?.insert(&k, v.as_bytes())?;
            Ok(())
        })
    }

    /// Wait until every one of `among` has applied at least what `from` has.
    pub async fn converge(&self, among: &[RaftId], from: RaftId) {
        let want = self.by_id(from).db.revision();
        for &i in among {
            let node = &self.by_id(i).node;
            node.raft.wait(Some(Duration::from_secs(45))).applied_index_at_least(Some(want), "converge").await.unwrap_or_else(|e| panic!("{i} did not reach {want}: {e}"));
        }
    }

    /// Whether the replicated tables of these members are identical.
    pub fn same_data(&self, among: &[RaftId]) -> bool {
        let first = self.by_id(among[0]).db.dump().unwrap();
        among.iter().all(|i| self.by_id(*i).db.dump().unwrap() == first)
    }
}
