//! glidex-stord: the privileged glidex storage helper (spec/24-storage.md
//! §11.1, S17). It makes, activates and removes glidex's logical volumes
//! in the volume groups the operator allows, hands their device nodes to
//! the `glidex` user, and lets each VM's unit open exactly its own.
//!
//! The library exposes the wire protocol and a blocking client for the
//! control plane, plus the server internals (used by the binary and tests).

pub mod auth;
pub mod client;
pub mod devices;
pub mod lvm;
pub mod names;
pub mod proto;
pub mod sd;
pub mod server;
