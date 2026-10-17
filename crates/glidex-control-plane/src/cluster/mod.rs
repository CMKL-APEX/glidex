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

/// Run `f` on `rt` and wait for it from synchronous code (a store write
/// that must reach the leader or Raft). `None` when the runtime shut down
/// first: it drops its tasks, `f` with them.
///
/// Not `block_in_place(|| rt.block_on(f))`: a `block_in_place` that starts
/// once the runtime is shutting down can't hand its worker's core on, so
/// the I/O and time drivers are never driven or shut down again, `f` never
/// wakes, and the shutdown waits on this thread for ever.
pub(crate) fn wait_on<F>(rt: &tokio::runtime::Handle, f: F) -> Option<F::Output>
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
