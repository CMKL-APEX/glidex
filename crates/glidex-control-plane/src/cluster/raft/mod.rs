//! Raft integration (spec/clustering.md §6.2): the log, the state machine and
//! the transport.

// openraft's traits take its StorageError and RPCError, which are large.
#![allow(clippy::result_large_err)]

pub mod log_store;
pub mod state_machine;
pub mod types;

pub use log_store::LogStore;
pub use state_machine::StateMachine;
pub use types::*;
pub mod network;
pub use network::{NetworkFactory, RaftService, Rpc, Transport, TransportError};
pub mod node;
pub use node::{ClusterNode, RaftSettings};
pub mod testing;
