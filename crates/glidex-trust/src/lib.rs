//! The cluster side of trust between a cluster and the directory of
//! another organization (spec/19-tenancy.md §12, D63–D73, D84).
//!
//! A cluster's operator gives a directory an **offer** ([`OfferToken`]);
//! the directory redeems it at the cluster, which then trusts that
//! directory's keys for the tenant it opted in. From then on the cluster
//! pulls the directory's **feed** (its tenants' users, teams, projects,
//! members, role links and API tokens), **reports** back what it holds of
//! them, and accepts the directory's **tokens** for its people. No CA is
//! shared: every request carries a signed **peer assertion** and pins the
//! other side's certificate ([`peer`]).
//!
//! This crate holds the protocol and the checks, not the records: the
//! glidex control plane keeps them in its store, hyperstor's management
//! service in its map service. The `fake` module (feature `fake`) is a
//! directory in-process, for the tests of a cluster that trusts one.

pub mod client;
pub mod cluster;
#[cfg(feature = "fake")]
pub mod fake;
pub mod feed;
pub mod key;
pub mod peer;
pub mod tokens;
pub mod wire;

pub use key::{ClusterKey, Signer};
pub use peer::{PeerError, Replays};
pub use wire::*;

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `n` random bytes, base64url: a `jti`, a `state`, a PKCE verifier.
pub fn random_token(n: usize) -> String {
    use base64::Engine as _;
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("the system's random source");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// SHA-256 of `b`, lower-case hex.
pub fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// Compare two strings in time independent of where they differ.
pub fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
