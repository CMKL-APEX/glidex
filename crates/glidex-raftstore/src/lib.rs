//! A replicated ReDB store on openraft, shared by glidex's control plane and
//! hyperstor's map service (hyperstor decisions.md D20).
//!
//! - [`store`]: the database. Every write is a transaction whose `insert`s
//!   and `remove`s are recorded as a [`store::WriteSet`]; applying a write
//!   set is a pure byte operation, the same on every member and version
//!   (glidex spec/14-clustering.md §6.1).
//! - [`raft`]: write sets as Raft log entries, the database as the state
//!   machine, snapshots as views of it, and a running member
//!   ([`raft::ClusterNode`]) that turns [`store::Db::write`] into a
//!   proposal (§6.2).
//! - [`testing`]: an in-process cluster with fault injection.
//!
//! Each application describes its tables and who writes with a
//! [`store::Schema`]; the formats on disk and on the wire (write sets,
//! the Raft log, snapshots) depend only on it.

pub mod raft;
pub mod store;
pub mod testing;
