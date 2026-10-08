//! openraft's type configuration (spec/clustering.md D1, §6.2).

use crate::store::{SnapshotView, WriteSet};
use std::path::PathBuf;

/// A Raft member's id. A node's glidex id is a UUID; this is its first eight
/// bytes (never zero), recorded in the node's status and checked for
/// collisions when the node joins.
pub type RaftId = u64;

openraft::declare_raft_types!(
    pub TypeConfig:
        D = WriteSet,
        R = (),
        NodeId = RaftId,
        Node = openraft::BasicNode,
        Entry = openraft::Entry<TypeConfig>,
        SnapshotData = SnapshotHandle,
        AsyncRuntime = openraft::TokioRuntime,
);

pub type GxRaft = openraft::Raft<TypeConfig>;
pub type Vote = openraft::Vote<RaftId>;
pub type LogId = openraft::LogId<RaftId>;
pub type SnapshotMeta = openraft::SnapshotMeta<RaftId, openraft::BasicNode>;
pub type StorageError = openraft::StorageError<RaftId>;

/// Snapshot data (§6.2): never a copy of the database held in memory.
///
/// A snapshot a node is *sending* is a read transaction over the replicated
/// tables, held open from the moment it was asked for, so its contents match
/// its `last_log_id`; it is streamed to a file only when a follower needs it.
/// A snapshot being *received* is a file.
#[derive(Default)]
pub struct SnapshotHandle {
    pub view: Option<SnapshotView>,
    pub file: Option<PathBuf>,
}

impl std::fmt::Debug for SnapshotHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SnapshotHandle {{ view: {}, file: {:?} }}", self.view.is_some(), self.file)
    }
}

/// The Raft id of a node UUID.
pub fn raft_id_of(node_id: &str) -> RaftId {
    let bytes: Vec<u8> = node_id.bytes().filter(|b| b.is_ascii_hexdigit()).take(16).collect();
    let hex = std::str::from_utf8(&bytes).unwrap_or("");
    let id = u64::from_str_radix(hex, 16).unwrap_or_else(|_| {
        // Not a UUID (the implicit node `local`): hash it.
        use sha2::{Digest, Sha256};
        u64::from_be_bytes(Sha256::digest(node_id.as_bytes())[..8].try_into().unwrap())
    });
    id.max(1)
}

pub fn io_err(e: impl std::fmt::Display) -> StorageError {
    StorageError::IO { source: openraft::StorageIOError::write(&std::io::Error::other(e.to_string())) }
}
