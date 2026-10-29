//! Raft integration (spec/14-clustering.md §6.2): the log, the state machine and
//! the transport, from `glidex-raftstore`, for glidex's [`Glidex`] schema.

use crate::store::{Db, Glidex, StoreError};
use std::path::Path;
use std::sync::Arc;

pub use glidex_raftstore::raft::{
    LogId, RaftId, RaftSettings, Rpc, SnapshotHeader, SnapshotMeta, StorageError, Transport,
    TransportError, Vote, io_err,
};

pub type TypeConfig = glidex_raftstore::raft::TypeConfig<Glidex>;
pub type GxRaft = glidex_raftstore::raft::Raft<Glidex>;
pub type ClusterNode = glidex_raftstore::raft::ClusterNode<Glidex>;
pub type RaftService = glidex_raftstore::raft::RaftService<Glidex>;
pub type SnapshotHandle = glidex_raftstore::raft::SnapshotHandle<Glidex>;
pub type LogStore = glidex_raftstore::raft::LogStore<Glidex>;
pub type StateMachine = glidex_raftstore::raft::StateMachine<Glidex>;
pub type NetworkFactory = glidex_raftstore::raft::NetworkFactory<Glidex>;

/// Start this server's Raft member on `db` ([`ClusterNode::start`]). Where
/// the leader's address isn't in the applied membership yet, it is looked up
/// in the replicated `nodes` table.
pub async fn start_node(
    db: Arc<Db>,
    dir: &Path,
    id: RaftId,
    settings: &RaftSettings,
    transport: Arc<dyn Transport>,
) -> Result<Arc<ClusterNode>, StoreError> {
    let node = ClusterNode::start(db.clone(), dir, id, settings, transport).await?;
    node.set_leader_lookup(Box::new(move |leader| {
        crate::node::NodeStore::new(db.clone())
            .list()
            .ok()?
            .into_iter()
            .find(|n| n.status.raft_id == Some(leader))
            .and_then(|n| n.status.advertise)
            .map(|a| a.to_string())
    }));
    Ok(node)
}

/// The Raft id of a node UUID: its first eight bytes (never zero), recorded
/// in the node's status and checked for collisions when the node joins.
pub fn raft_id_of(node_id: &str) -> RaftId {
    let bytes: Vec<u8> = node_id
        .bytes()
        .filter(|b| b.is_ascii_hexdigit())
        .take(16)
        .collect();
    let hex = std::str::from_utf8(&bytes).unwrap_or("");
    let id = u64::from_str_radix(hex, 16).unwrap_or_else(|_| {
        // Not a UUID (the implicit node `local`): hash it.
        use sha2::{Digest, Sha256};
        u64::from_be_bytes(Sha256::digest(node_id.as_bytes())[..8].try_into().unwrap())
    });
    id.max(1)
}
