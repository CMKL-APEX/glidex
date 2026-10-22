//! Clustering (spec/clustering.md): a replicated control plane (Raft) and,
//! later, cluster networks (OVN).

pub mod config;
pub mod identity;
pub mod manage;
pub mod net;
pub mod pki;
pub mod raft;
pub mod runtime;
pub mod server;
pub mod tokens;
pub use runtime::{Cluster, ClusterError};
pub mod agent;
pub mod departure;
pub mod import;
pub mod lifecycle;
pub mod membership;
pub mod rotation;
pub mod sync;

pub(crate) use glidex_raftstore::raft::wait_on;
