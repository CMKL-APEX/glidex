//! The glidex agent on a BlueField DPU (spec/31-dpu.md §10.2) and netd's
//! client for it. The agent runs `glidex-dpu` on the DPU for the one netd
//! whose certificate it accepts; it has no OVN central program and no
//! state of its own.

pub mod chassis;
pub mod client;
pub mod proto;
pub mod server;
pub mod tls;

pub use client::AgentClient;
pub use proto::{AgentApi, AgentError, AgentOp};
pub use server::Agent;
