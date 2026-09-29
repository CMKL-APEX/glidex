//! glidex-netd: the privileged glidex network helper (spec §7).
//!
//! The library exposes the wire protocol and a blocking client for the
//! control plane, plus the server internals (used by the binary and tests).

pub mod auth;
pub mod client;
pub mod proto;
pub mod server;
pub mod store;
pub mod supervisor;
