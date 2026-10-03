//! glidex-authd: the root helper that verifies local users' passwords with
//! PAM for the control plane (spec/security.md §5.3, decision 6).
//!
//! The library holds the wire protocol, a blocking client for the control
//! plane ([`client::AuthdClient`]) and the server internals (used by the
//! binary and the tests).

pub mod activation;
pub mod authenticator;
pub mod client;
pub mod config;
pub mod keys;
pub mod limiter;
pub mod pam;
pub mod proto;
pub mod server;
