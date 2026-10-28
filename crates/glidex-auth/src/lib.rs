//! Core authentication, shared by glidex's control plane and hyperstor's
//! management service (hyperstor decisions.md D96).
//!
//! - [`secrets`]: random values, base62, SHA-256 and constant-time
//!   comparison, the building blocks of every secret below.
//! - [`session`]: browser sessions (random ids of which only the SHA-256
//!   is stored, CSRF values, idle and absolute timeouts) and the age a
//!   step-up check sees (glidex spec/security.md §5.5).
//! - [`token`]: access tokens: minting, their lifetime rules and names
//!   (§5.5).
//! - [`cedar`]: a Cedar policy engine: a schema and shipped policies,
//!   role links and site policies, validated strictly and swapped
//!   atomically; a decision is a deny when any policy errors (§7).
//! - [`oidc`]: OIDC login as a confidential client: the Authorization
//!   Code flow with PKCE and a nonce, the Device Authorization Grant, and
//!   ID token verification (§5.4).
//!
//! Each application keeps its own store, schema, entities and token
//! prefix: the crate holds the rules, never the records.

pub mod cedar;
pub mod oidc;
pub mod secrets;
pub mod session;
pub mod token;

/// Unix seconds now; every timestamp the crate compares is one.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
