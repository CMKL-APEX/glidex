//! The node role over the network (spec/clustering.md §8.2): a cache of the
//! cluster store fed by a list and a watch, status writes sent to the leader,
//! heartbeats, and a queue of writes made while the leader is out of reach.

use super::net::PeerClient;
use super::runtime::Cluster;
use crate::store::{Db, Mirror, StoreError, Submitted, TableId, WriteSet};
use bytes::Bytes;
use redb::ReadableTable;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Status writes that could not reach the leader, in order, kept on disk in
/// the local `raft_meta` table so a restart doesn't lose them.
const OUTBOX_PREFIX: &str = "outbox/";

pub struct NodeLink {
    cluster: Arc<Cluster>,
    db: Arc<Db>,
    /// Server addresses to try, the last good one first.
    servers: Mutex<Vec<String>>,
    runtime: tokio::runtime::Handle,
    /// Set when the cache must be listed again.
    relist: AtomicBool,
    seq: std::sync::atomic::AtomicU64,
    /// Keys with a queued write: the stream must not overwrite them.
    pending: Mutex<HashSet<(TableId, Vec<u8>)>>,
    connected: AtomicBool,
    /// Test and chaos knob: behave as if no server could be reached.
    partitioned: AtomicBool,
    /// The cache holds a usable copy (listed once, or kept from before).
    ready: tokio::sync::watch::Sender<bool>,
    stop: tokio::sync::watch::Sender<bool>,
}

impl NodeLink {
    /// Start the link of `cluster`'s node: this makes `db` a cache of the
    /// cluster store and runs the list/watch, heartbeat and outbox loops.
    pub fn start(cluster: Arc<Cluster>, db: Arc<Db>, seeds: Vec<String>) -> Arc<NodeLink> {
        let (stop, _) = tokio::sync::watch::channel(false);
        let link = Arc::new(NodeLink {
            cluster,
            db: db.clone(),
            servers: Mutex::new(seeds),
            runtime: tokio::runtime::Handle::current(),
            relist: AtomicBool::new(false),
            seq: std::sync::atomic::AtomicU64::new(0),
            pending: Mutex::new(HashSet::new()),
            connected: AtomicBool::new(false),
            partitioned: AtomicBool::new(false),
            ready: tokio::sync::watch::channel(false).0,
            stop,
        });
        db.set_revision(db.stored_mirror_revision());
        if db.revision() > 0 {
            link.ready.send_replace(true);
        }
        link.load_outbox();
        db.set_mirror(link.clone());
        let l = link.clone();
        tokio::spawn(async move { l.sync_loop().await });
        let l = link.clone();
        tokio::spawn(async move { l.heartbeat_loop().await });
        let l = link.clone();
        tokio::spawn(async move { l.outbox_loop().await });
        link
    }

    /// Wait until the cache can be trusted to show this node's VMs, so that
    /// startup never mistakes "not listed yet" for "no VMs" (networking.md §7.7).
    pub async fn wait_ready(&self, timeout: Duration) -> bool {
        let mut rx = self.ready.subscribe();
        let ok = tokio::time::timeout(timeout, rx.wait_for(|r| *r)).await.is_ok();
        ok
    }

    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    pub fn set_partitioned(&self, on: bool) {
        self.partitioned.store(on, Ordering::Release);
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Record whether a server answers; a change is logged, with why.
    fn set_connected(&self, up: bool, why: &str) {
        if self.connected.swap(up, Ordering::AcqRel) != up {
            if up {
                tracing::info!(server = why, "reached the cluster servers");
            } else {
                tracing::warn!("lost the cluster servers: {}", why);
            }
        }
    }

    /// The servers to try, the one that answered last first.
    pub fn servers(&self) -> Vec<String> {
        self.candidates()
    }

    /// The servers this node knows: the ones it was given and the ones in its cache.
    fn candidates(&self) -> Vec<String> {
        let mut v = self.servers.lock().unwrap().clone();
        if let Ok(nodes) = crate::node::NodeStore::new(self.db.clone()).list() {
            for n in nodes {
                if n.spec.role == crate::node::NodeRole::Server && !n.status.phase.is_tombstone() {
                    if let Some(a) = n.status.advertise {
                        let a = a.to_string();
                        if !v.contains(&a) {
                            v.push(a);
                        }
                    }
                }
            }
        }
        v
    }

    fn prefer(&self, addr: &str) {
        let mut s = self.servers.lock().unwrap();
        s.retain(|a| a != addr);
        s.insert(0, addr.to_string());
    }

    /// One request to the leader: tries each known server, following
    /// "not the leader" answers. `Err(None)`: nobody could be reached.
    async fn to_leader(&self, method: hyper::Method, path: &str, body: Bytes, timeout: Duration) -> Result<super::net::Reply, Option<super::net::Reply>> {
        if self.partitioned.load(Ordering::Acquire) {
            self.set_connected(false, "partitioned");
            return Err(None);
        }
        let mut last = String::from("no server known");
        let mut tried = HashSet::new();
        let mut queue: Vec<String> = self.candidates();
        while let Some(addr) = queue.first().cloned() {
            queue.remove(0);
            if !tried.insert(addr.clone()) {
                continue;
            }
            match self.cluster.client.request_timeout(&addr, method.clone(), path, &[("content-type", "application/json".into())], body.clone(), timeout).await {
                Ok(r) if r.status == hyper::StatusCode::MISDIRECTED_REQUEST => {
                    if let Some(l) = serde_json::from_slice::<Value>(&r.body).ok().and_then(|v| v["leader"].as_str().map(String::from)) {
                        queue.insert(0, l);
                    }
                }
                Ok(r) => {
                    self.prefer(&addr);
                    self.set_connected(true, &addr);
                    return Ok(r);
                }
                Err(e) => last = format!("{path}: {e}"),
            }
        }
        self.set_connected(false, &last);
        Err(None)
    }

    // ---- cache ------------------------------------------------------------

    async fn list(&self) -> Result<(), String> {
        let r = self.to_leader(hyper::Method::GET, "/cluster/v1/list", Bytes::new(), Duration::from_secs(60)).await.map_err(|_| "no server could be reached".to_string())?;
        if !r.status.is_success() {
            return Err(format!("list: {}", r.status));
        }
        let v: Value = serde_json::from_slice(&r.body).map_err(|e| e.to_string())?;
        let revision = v["revision"].as_u64().ok_or("no revision")?;
        let ws: WriteSet = serde_json::from_value(v["ws"].clone()).map_err(|e| e.to_string())?;
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || db.install_ops(&ws, revision)).await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        // Writes still queued are applied again over the fresh copy.
        self.reapply_outbox();
        Ok(())
    }

    async fn sync_loop(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        let mut listed = false;
        loop {
            if *stop.borrow() {
                return;
            }
            if !listed || self.relist.swap(false, Ordering::AcqRel) {
                match self.list().await {
                    Ok(()) => {
                        listed = true;
                        self.ready.send_replace(true);
                    }
                    Err(e) => {
                        tracing::warn!("cannot list the cluster store ({}); running from the cache", e);
                        tokio::select! { _ = tokio::time::sleep(Duration::from_secs(2)) => {}, _ = stop.changed() => return }
                        continue;
                    }
                }
            }
            let from = self.db.revision();
            let path = format!("/cluster/v1/watch?from={from}&wait_secs=15");
            match self.to_leader_any(&path).await {
                Ok(r) if r.status == hyper::StatusCode::GONE => {
                    listed = false;
                }
                Ok(r) if r.status.is_success() => {
                    if let Err(e) = self.apply_entries(&r.body).await {
                        tracing::warn!("applying the watch: {}", e);
                        listed = false;
                    }
                }
                Ok(r) => {
                    tracing::debug!("watch: {}", r.status);
                    tokio::select! { _ = tokio::time::sleep(Duration::from_secs(1)) => {}, _ = stop.changed() => return }
                }
                Err(()) => {
                    tokio::select! { _ = tokio::time::sleep(Duration::from_secs(2)) => {}, _ = stop.changed() => return }
                }
            }
        }
    }

    /// A watch can be served by any server (they all apply the same log).
    async fn to_leader_any(&self, path: &str) -> Result<super::net::Reply, ()> {
        if self.partitioned.load(Ordering::Acquire) {
            return Err(());
        }
        let mut last = String::from("no server known");
        for addr in self.candidates() {
            match self.cluster.client.request_timeout(&addr, hyper::Method::GET, path, &[], Bytes::new(), Duration::from_secs(40)).await {
                Ok(r) => {
                    self.prefer(&addr);
                    self.set_connected(true, &addr);
                    return Ok(r);
                }
                Err(e) => last = format!("{path}: {e}"),
            }
        }
        self.set_connected(false, &last);
        Err(())
    }

    async fn apply_entries(&self, body: &[u8]) -> Result<(), String> {
        let v: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let upto = v["revision"].as_u64().ok_or("no revision")?;
        let entries = v["entries"].as_array().cloned().unwrap_or_default();
        let pending = self.pending.lock().unwrap().clone();
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            for e in entries {
                let index = e["index"].as_u64().ok_or("no index")?;
                let mut ws: WriteSet = serde_json::from_value(e["ws"].clone()).map_err(|e| e.to_string())?;
                ws.ops.retain(|o| !pending.contains(&(o.table(), o.key().to_vec())));
                db.apply(&ws, index, |txn| {
                    txn.open_table(TableId::RaftMeta.definition())?.insert(crate::store::MIRROR_REVISION, index.to_string().as_bytes())?;
                    Ok(())
                })
                .map_err(|e| e.to_string())?;
            }
            if upto > db.revision() {
                db.apply(&WriteSet { format: crate::store::WRITE_SET_FORMAT, origin: crate::store::Origin::System, ops: Vec::new() }, upto, |txn| {
                    txn.open_table(TableId::RaftMeta.definition())?.insert(crate::store::MIRROR_REVISION, upto.to_string().as_bytes())?;
                    Ok(())
                })
                .map_err(|e| e.to_string())?;
            }
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    // ---- heartbeats ---------------------------------------------------------

    async fn heartbeat_loop(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        loop {
            let _ = self.to_leader(hyper::Method::POST, "/cluster/v1/heartbeat", Bytes::from_static(b"{}"), Duration::from_secs(4)).await;
            tokio::select! { _ = tokio::time::sleep(Duration::from_secs(5)) => {}, _ = stop.changed() => return }
        }
    }

    // ---- outbox -------------------------------------------------------------

    fn load_outbox(&self) {
        let Ok(txn) = self.db.begin_read() else { return };
        let Ok(t) = txn.open_table(TableId::RaftMeta.definition()) else { return };
        let mut pending = self.pending.lock().unwrap();
        let mut max = 0;
        for r in t.range(OUTBOX_PREFIX..).into_iter().flatten().flatten() {
            let k = r.0.value();
            if !k.starts_with(OUTBOX_PREFIX) {
                break;
            }
            max = max.max(k[OUTBOX_PREFIX.len()..].parse::<u64>().unwrap_or(0));
            if let Ok(ws) = serde_json::from_slice::<WriteSet>(r.1.value()) {
                for o in &ws.ops {
                    pending.insert((o.table(), o.key().to_vec()));
                }
            }
        }
        self.seq.store(max, Ordering::Release);
    }

    fn outbox(&self) -> Vec<(String, WriteSet)> {
        let Ok(txn) = self.db.begin_read() else { return Vec::new() };
        let Ok(t) = txn.open_table(TableId::RaftMeta.definition()) else { return Vec::new() };
        let mut out = Vec::new();
        for r in t.range(OUTBOX_PREFIX..).into_iter().flatten().flatten() {
            let k = r.0.value().to_string();
            if !k.starts_with(OUTBOX_PREFIX) {
                break;
            }
            if let Ok(ws) = serde_json::from_slice::<WriteSet>(r.1.value()) {
                out.push((k, ws));
            }
        }
        out
    }

    fn reapply_outbox(&self) {
        for (_, ws) in self.outbox() {
            let _ = self.db.apply(&ws, self.db.revision(), |_| Ok(()));
        }
    }

    fn enqueue(&self, ws: &WriteSet) -> Result<(), StoreError> {
        let n = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
        let key = format!("{OUTBOX_PREFIX}{n:016}");
        let bytes = serde_json::to_vec(ws).map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
        self.db.put_local(&key, &bytes)?;
        let mut p = self.pending.lock().unwrap();
        for o in &ws.ops {
            p.insert((o.table(), o.key().to_vec()));
        }
        Ok(())
    }

    fn pop_outbox(&self, key: &str) {
        let _ = self.db.delete_local(key);
        let rest = self.outbox();
        let mut p = self.pending.lock().unwrap();
        p.clear();
        for (_, ws) in rest {
            for o in &ws.ops {
                p.insert((o.table(), o.key().to_vec()));
            }
        }
    }

    /// Send queued writes in order once the leader answers. A write the
    /// leader refuses is dropped, and the cache is listed again (§8.2: on any
    /// conflict drop that entry and re-list).
    async fn outbox_loop(self: Arc<Self>) {
        let mut stop = self.stop.subscribe();
        loop {
            tokio::select! { _ = tokio::time::sleep(Duration::from_secs(2)) => {}, _ = stop.changed() => return }
            for (key, ws) in self.outbox() {
                let body = Bytes::from(serde_json::to_vec(&ws).unwrap_or_default());
                match self.to_leader(hyper::Method::POST, "/cluster/v1/status-write", body, Duration::from_secs(10)).await {
                    Ok(r) if r.status.is_success() => self.pop_outbox(&key),
                    Ok(r) => {
                        tracing::warn!("the leader refused a queued status write: {}", String::from_utf8_lossy(&r.body));
                        self.pop_outbox(&key);
                        self.relist.store(true, Ordering::Release);
                    }
                    Err(_) => break,
                }
            }
        }
    }

    /// POST to the leader as this node.
    pub async fn post_leader(&self, path: &str, body: Bytes) -> Result<super::net::Reply, String> {
        self.to_leader(hyper::Method::POST, path, body, Duration::from_secs(30)).await.map_err(|_| "no server could be reached".to_string())
    }

    fn block<F: std::future::Future>(&self, f: F) -> F::Output {
        tokio::task::block_in_place(|| self.runtime.block_on(f))
    }
}

impl Mirror for NodeLink {
    fn submit(&self, ws: &WriteSet) -> Result<Submitted, StoreError> {
        // Writes already waiting go first: order matters.
        if !self.pending.lock().unwrap().is_empty() {
            self.enqueue(ws)?;
            return Ok(Submitted::Queued);
        }
        let body = Bytes::from(serde_json::to_vec(ws).map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?);
        match self.block(self.to_leader(hyper::Method::POST, "/cluster/v1/status-write", body, Duration::from_secs(10))) {
            Ok(r) if r.status.is_success() => {
                let index = serde_json::from_slice::<Value>(&r.body).ok().and_then(|v| v["index"].as_u64()).unwrap_or(0);
                // Read your writes: the controller's next step reads the cache.
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while self.db.revision() < index && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(Submitted::Acked(index))
            }
            Ok(r) => {
                self.relist.store(true, Ordering::Release);
                Err(StoreError::Refused(serde_json::from_slice::<Value>(&r.body).ok().and_then(|v| v["message"].as_str().map(String::from)).unwrap_or_else(|| r.status.to_string())))
            }
            Err(_) => {
                self.enqueue(ws)?;
                Ok(Submitted::Queued)
            }
        }
    }
}

/// The client other code uses to reach servers as this node.
pub fn client_of(c: &Cluster) -> Arc<PeerClient> {
    c.client.clone()
}
