//! openraft's type configuration (glidex spec/14-clustering.md D1, §6.2), for
//! a store of schema `S`.

use crate::store::{Schema, SnapshotView, WriteSet};
use std::marker::PhantomData;
use std::path::PathBuf;

/// A Raft member's id (never zero).
pub type RaftId = u64;

/// openraft's type configuration for a store of schema `S`: entries are
/// [`WriteSet`]s, snapshots are views of the database. Written out rather
/// than with `declare_raft_types!`, which can't take a type parameter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct TypeConfig<S: Schema>(PhantomData<S>);

impl<S: Schema> openraft::RaftTypeConfig for TypeConfig<S> {
    type D = WriteSet<S>;
    type R = ();
    type NodeId = RaftId;
    type Node = openraft::BasicNode;
    type Entry = openraft::Entry<TypeConfig<S>>;
    type SnapshotData = SnapshotHandle<S>;
    type AsyncRuntime = openraft::TokioRuntime;
    type Responder = openraft::impls::OneshotResponder<TypeConfig<S>>;
}

pub type Raft<S> = openraft::Raft<TypeConfig<S>>;
pub type Vote = openraft::Vote<RaftId>;
pub type LogId = openraft::LogId<RaftId>;
pub type SnapshotMeta = openraft::SnapshotMeta<RaftId, openraft::BasicNode>;
pub type StorageError = openraft::StorageError<RaftId>;

/// Snapshot data (§6.2): never a copy of the database held in memory.
///
/// A snapshot a member is *sending* is a read transaction over the
/// replicated tables, held open from the moment it was asked for, so its
/// contents match its `last_log_id`; it is streamed to a file only when a
/// follower needs it. A snapshot being *received* is a file.
pub struct SnapshotHandle<S: Schema> {
    pub view: Option<SnapshotView<S>>,
    pub file: Option<PathBuf>,
}

impl<S: Schema> Default for SnapshotHandle<S> {
    fn default() -> Self {
        SnapshotHandle {
            view: None,
            file: None,
        }
    }
}

impl<S: Schema> std::fmt::Debug for SnapshotHandle<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SnapshotHandle {{ view: {}, file: {:?} }}",
            self.view.is_some(),
            self.file
        )
    }
}

pub fn io_err(e: impl std::fmt::Display) -> StorageError {
    StorageError::IO {
        source: openraft::StorageIOError::write(&std::io::Error::other(e.to_string())),
    }
}
