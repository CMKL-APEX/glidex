//! A node's running cluster membership: its TLS identity, its Raft member
//! (servers), the :8842 node API, and the read barrier (§6.4).

use super::config::ClusterConfig;
use super::identity::{Files, Identity};
use super::net::{ClusterTls, PeerCert, PeerClient, TlsMaterial};
use super::pki::{Ca, PkiError};
use super::raft::{ClusterNode, RaftId, Rpc, Transport, TransportError};
use crate::node::NodeRole;
use crate::store::Db;
use bytes::Bytes;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("{0}")]
    Pki(#[from] PkiError),
    #[error("{0}")]
    Net(#[from] super::net::NetError),
    #[error("{0}")]
    Store(#[from] crate::store::StoreError),
    #[error("{0}")]
    Other(String),
}

/// What a node's own manager answers when a server relays a request to it.
pub trait NodeHandlers: Send + Sync {
    fn vm_stats(&self, vm_id: &str) -> Option<serde_json::Value>;
    /// A file of image `id` this node holds (`main`, or a firmware `vars`
    /// template).
    fn image_file(&self, id: &str, part: &str) -> Option<std::path::PathBuf>;
}

pub struct Cluster {
    pub(crate) handlers: OnceLock<Arc<dyn NodeHandlers>>,
    pub identity: Identity,
    pub files: Files,
    pub db: Arc<Db>,
    pub node: Option<Arc<ClusterNode>>,
    pub tls: Arc<ClusterTls>,
    pub client: Arc<PeerClient>,
    pub config: ClusterConfig,
    pub(crate) ca: Mutex<Option<Ca>>,
    /// The API router, served to forwarded requests; set once the app exists.
    pub(crate) api: OnceLock<axum::Router>,
    stop: tokio::sync::watch::Sender<bool>,
    barrier: Arc<Barrier>,
    /// Servers only: recent applied write sets for watches.
    pub(crate) log: Option<Arc<super::sync::WriteLog>>,
    pub(crate) liveness: super::sync::Liveness,
}

/// Raft messages over mTLS HTTP/2.
pub struct HttpTransport {
    pub client: Arc<PeerClient>,
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn call(&self, _target: RaftId, addr: &str, rpc: Rpc, body: Vec<u8>) -> Result<Vec<u8>, TransportError> {
        let r = self.client.request(addr, hyper::Method::POST, rpc.path(), &[("content-type", "application/json".into())], Bytes::from(body)).await.map_err(|e| TransportError(e.to_string()))?;
        if !r.status.is_success() {
            return Err(TransportError(format!("{addr}{}: {}", rpc.path(), r.status)));
        }
        Ok(r.body.to_vec())
    }

    async fn snapshot(&self, _target: RaftId, addr: &str, header: Vec<u8>, data: PathBuf) -> Result<Vec<u8>, TransportError> {
        use base64::Engine;
        let h = base64::engine::general_purpose::STANDARD.encode(header);
        let r = self.client.post_file(addr, "/raft/snapshot", &[("x-glidex-snapshot", h)], &data).await.map_err(|e| TransportError(e.to_string()))?;
        if !r.status.is_success() {
            return Err(TransportError(format!("{addr}/raft/snapshot: {}", r.status)));
        }
        Ok(r.body.to_vec())
    }
}

/// Coalesces concurrent read-index requests: a caller that arrives while a
/// round is in flight waits for the next one, so what it reads is never older
/// than its request (§6.4).
#[derive(Default)]
struct Barrier {
    state: Mutex<BarrierState>,
}

#[derive(Default)]
struct BarrierState {
    running: bool,
    waiters: Vec<tokio::sync::oneshot::Sender<Result<u64, String>>>,
}

impl Barrier {
    async fn index<F, Fut>(self: &Arc<Self>, round: F) -> Result<u64, String>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<u64, String>> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let start = {
            let mut s = self.state.lock().unwrap();
            s.waiters.push(tx);
            !std::mem::replace(&mut s.running, true)
        };
        if start {
            let me = self.clone();
            tokio::spawn(async move {
                loop {
                    let batch = {
                        let mut s = me.state.lock().unwrap();
                        if s.waiters.is_empty() {
                            s.running = false;
                            return;
                        }
                        std::mem::take(&mut s.waiters)
                    };
                    let r = round().await;
                    for w in batch {
                        let _ = w.send(r.clone());
                    }
                }
            });
        }
        rx.await.map_err(|_| "read barrier dropped".to_string())?
    }
}

impl Cluster {
    /// Start this node's membership: TLS, Raft (servers) and the :8842 API.
    /// `listen` is where the node API binds.
    pub async fn start(db: Arc<Db>, files: Files, identity: Identity, config: ClusterConfig, listen: std::net::SocketAddr) -> Result<Arc<Cluster>, ClusterError> {
        let material = TlsMaterial::from_pem(&files.read(files.cert())?, &files.read(files.key())?, &files.read(files.trust())?)?;
        let tls = ClusterTls::new(material)?;
        let client = PeerClient::new(tls.clone());
        let node = match identity.role {
            NodeRole::Server => {
                let transport: Arc<dyn Transport> = Arc::new(HttpTransport { client: client.clone() });
                Some(ClusterNode::start(db.clone(), &files.raft(), identity.raft_id, &config.raft_settings(), transport).await?)
            }
            NodeRole::Agent => None,
        };
        let ca = files.load_ca()?;
        let log = node.is_some().then(|| {
            let l = super::sync::WriteLog::new(20_000);
            l.follow(&db);
            l
        });
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let cluster = Arc::new(Cluster { identity, files, db, node, tls: tls.clone(), client, config, ca: Mutex::new(ca), api: OnceLock::new(), stop, barrier: Arc::new(Barrier::default()), log, liveness: Default::default(), handlers: OnceLock::new() });
        // A runtime that was just stopped may still hold the port for a moment.
        let mut bound = tokio::net::TcpListener::bind(listen).await;
        for _ in 0..50 {
            if bound.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            bound = tokio::net::TcpListener::bind(listen).await;
        }
        let listener = bound.map_err(|e| ClusterError::Other(format!("cannot listen on {listen}: {e}")))?;
        let router = super::server::router(cluster.clone());
        tokio::spawn(super::net::serve(listener, tls, router, stop_rx));
        if let Some(n) = &cluster.node {
            n.replicate();
            cluster.db.set_forwarder(Arc::new(LeaderForwarder { cluster: Arc::downgrade(&cluster), runtime: tokio::runtime::Handle::current() }));
        }
        cluster.start_lifecycle();
        Ok(cluster)
    }

    pub fn role(&self) -> NodeRole {
        self.identity.role
    }

    pub fn is_leader(&self) -> bool {
        self.node.as_ref().is_some_and(|n| n.is_leader())
    }

    pub fn leader_addr(&self) -> Option<String> {
        self.node.as_ref().and_then(|n| n.leader_addr())
    }

    /// The server to send writes and reads to: this one when it leads, else
    /// the leader. Agents ask any server they know of (§8).
    pub fn set_api(&self, router: axum::Router) {
        let _ = self.api.set(router);
    }

    pub(crate) fn stop_rx(&self) -> tokio::sync::watch::Receiver<bool> {
        self.stop.subscribe()
    }

    pub fn shutdown(&self) {
        let _ = self.stop.send(true);
    }

    pub async fn stop(&self) {
        self.shutdown();
        if let Some(n) = &self.node {
            n.shutdown().await;
        }
    }

    /// POST to the leader as a server (following one "not the leader").
    pub async fn post_leader(&self, path: &str, body: Bytes) -> Result<super::net::Reply, String> {
        let mut addr = self.leader_addr().ok_or("no leader")?;
        for _ in 0..2 {
            let r = self.client.request_timeout(&addr, hyper::Method::POST, path, &[("content-type", "application/json".into())], body.clone(), Duration::from_secs(30)).await.map_err(|e| e.to_string())?;
            if r.status == hyper::StatusCode::MISDIRECTED_REQUEST {
                if let Some(l) = serde_json::from_slice::<serde_json::Value>(&r.body).ok().and_then(|v| v["leader"].as_str().map(String::from)) {
                    addr = l;
                    continue;
                }
            }
            return Ok(r);
        }
        Err("no leader".into())
    }

    pub fn signing_ca(&self) -> Option<Ca> {
        self.ca.lock().unwrap().clone()
    }

    /// Wait until this node knows who leads, for a bounded time.
    pub async fn wait_for_leader(&self, timeout: Duration) -> Option<String> {
        let node = self.node.as_ref()?;
        node.wait_for_leader(timeout).await.ok()?;
        node.leader_addr()
    }

    /// Linearizable reads (§6.4): after this returns, the local replica has
    /// applied everything committed before the call. On the leader a heartbeat
    /// round to a quorum; on a follower, the leader's read index and a wait.
    pub async fn read_barrier(self: &Arc<Self>) -> Result<(), String> {
        let Some(node) = self.node.clone() else { return Err("this node is not a server".into()) };
        // Right after an election or a restart there is briefly no leader.
        if node.leader().is_none() {
            let _ = node.wait_for_leader(Duration::from_secs(3)).await;
        }
        let me = self.clone();
        let idx = self.barrier
            .clone()
            .index(move || {
                let (me, node) = (me.clone(), node.clone());
                async move {
                    if node.is_leader() {
                        let (read, _) = node.raft.get_read_log_id().await.map_err(|e| e.to_string())?;
                        return Ok(read.map(|l| l.index).unwrap_or(0));
                    }
                    let mut addr = node.leader_addr();
                    for _ in 0..30 {
                        if addr.is_some() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        addr = node.leader_addr();
                    }
                    let Some(addr) = addr else { return Err("no leader".into()) };
                    let r = me.client.request(&addr, hyper::Method::POST, "/cluster/v1/read-index", &[], Bytes::new()).await.map_err(|e| e.to_string())?;
                    if !r.status.is_success() {
                        return Err(format!("leader answered {}", r.status));
                    }
                    let v: serde_json::Value = serde_json::from_slice(&r.body).map_err(|e| e.to_string())?;
                    v["index"].as_u64().ok_or_else(|| "bad read index".to_string())
                }
            })
            .await?;
        let node = self.node.as_ref().unwrap();
        node.raft.wait(Some(Duration::from_secs(10))).applied_index_at_least(Some(idx), "read barrier").await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Verify a peer's certificate against what the leader issued (D20) and
    /// the deny list: a certificate the CA signed but the cluster never
    /// issued is refused.
    pub fn check_peer(&self, peer: &PeerCert) -> Result<(), &'static str> {
        use redb::{ReadableTable, ReadableTableMetadata};
        let Ok(txn) = self.db.begin_read() else { return Err("store unavailable") };
        let denied = txn.open_table(crate::store::TableId::NodeDenylist.definition()).ok().is_some_and(|t| t.get(peer.serial.as_str()).ok().flatten().is_some());
        if denied {
            return Err("certificate revoked");
        }
        // A node that has not received its first snapshot has an empty
        // registry and holds nothing to protect; it must still be able to
        // take the leader's first messages. From then on the registry decides.
        if let Ok(t) = txn.open_table(crate::store::TableId::IssuedCerts.definition()) {
            let empty = t.is_empty().unwrap_or(true);
            if !empty && t.get(peer.serial.as_str()).ok().flatten().is_none() {
                return Err("certificate was not issued by this cluster");
            }
        }
        Ok(())
    }
}

/// Sends a follower's simple writes to the leader (see [`crate::store::Forwarder`]).
struct LeaderForwarder {
    cluster: std::sync::Weak<Cluster>,
    runtime: tokio::runtime::Handle,
}

impl LeaderForwarder {
    fn post(&self, path: &str, body: Vec<u8>) -> Result<Bytes, crate::store::StoreError> {
        use crate::store::StoreError;
        let c = self.cluster.upgrade().ok_or(StoreError::NotLeader { leader: None })?;
        let path = path.to_string();
        tokio::task::block_in_place(|| {
            self.runtime.block_on(async move {
                let Some(addr) = c.leader_addr() else { return Err(StoreError::NotLeader { leader: None }) };
                let r = c
                    .client
                    .request(&addr, hyper::Method::POST, &path, &[("content-type", "application/json".into())], Bytes::from(body))
                    .await
                    .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
                if r.status.is_success() {
                    Ok(r.body)
                } else {
                    Err(StoreError::Io(std::io::Error::other(format!("the leader answered {}: {}", r.status, String::from_utf8_lossy(&r.body)))))
                }
            })
        })
    }
}

impl crate::store::Forwarder for LeaderForwarder {
    fn write_raw(&self, ops: Vec<crate::store::Op>) -> Result<(), crate::store::StoreError> {
        let body = serde_json::to_vec(&ops).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?;
        self.post("/cluster/v1/raw", body).map(|_| ())
    }

    fn call(&self, op: &str, args: serde_json::Value) -> Result<serde_json::Value, crate::store::StoreError> {
        let body = serde_json::to_vec(&serde_json::json!({ "op": op, "args": args })).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?;
        let out = self.post("/cluster/v1/auth", body)?;
        serde_json::from_slice(&out).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))
    }
}
