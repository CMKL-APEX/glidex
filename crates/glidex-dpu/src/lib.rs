//! BlueField DPU management for glidex (spec/31-dpu.md).
//!
//! The DPU side of a DPU-backed VM NIC: the command builders for
//! `ovs-vsctl` on the DPU's OVS-DOCA and for `virtnet` (the virtio-net
//! controller), parsing of what the DPU reports into [`DpuCapabilities`],
//! and the allocator of the virtio-net PF's VFs. Like `glidex-ovs`, it
//! depends on nothing else in glidex; the DPU agent runs it on the DPU and
//! netd wraps it on the host.
//!
//! **Ownership invariant:** it only modifies or deletes OVSDB rows it has
//! tagged (`external_ids:glidex-owner=glidex`), as `glidex-ovs` does.

pub mod alloc;
pub mod exec;
pub mod ovs;
pub mod ovsdb;
pub mod probe;
pub mod types;
pub mod virtnet;

use serde::{Deserialize, Serialize};

pub use alloc::VfAllocator;
pub use exec::{Cmd, Exec, Output, Program, RecordingExec, SystemExec};
pub use probe::DpuCapabilities;
pub use types::{Backend, DatapathType, DpuPortSpec, VfId};

/// Errors, each with a stable protocol code (`DpuError::code`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum DpuError {
    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },

    #[error("not found: {message}")]
    NotFound { message: String },

    #[error("not owned by glidex: {message}")]
    NotOwned { message: String },

    #[error("unsupported on this DPU; missing: {}", missing.join(", "))]
    Unsupported { missing: Vec<String> },

    #[error("the DPU is unavailable: {reason}")]
    DpuUnavailable { reason: String },

    #[error("no free VF on the DPU ({total} in use)")]
    DpuCapacity { total: u16 },

    #[error("the VF's representor did not appear: {message}")]
    DpuPortTimeout { message: String },

    #[error("{program} exited with {status}: {stderr_tail}")]
    CommandFailed {
        program: String,
        status: i32,
        stderr_tail: String,
    },

    #[error("could not parse {what}: {message}")]
    Parse { what: String, message: String },

    #[error("I/O error: {0}")]
    #[serde(rename = "internal")]
    Io(String),
}

impl DpuError {
    /// The protocol error code (spec/31-dpu.md §13).
    pub fn code(&self) -> &'static str {
        match self {
            DpuError::InvalidArgument { .. } => "invalid_argument",
            DpuError::NotFound { .. } => "not_found",
            DpuError::NotOwned { .. } => "not_owned",
            DpuError::Unsupported { .. } => "unsupported",
            DpuError::DpuUnavailable { .. } => "dpu_unavailable",
            DpuError::DpuCapacity { .. } => "dpu_capacity",
            DpuError::DpuPortTimeout { .. } => "dpu_port_timeout",
            DpuError::CommandFailed { .. } => "command_failed",
            DpuError::Parse { .. } => "internal",
            DpuError::Io(_) => "internal",
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        DpuError::InvalidArgument {
            message: message.into(),
        }
    }

    pub fn parse(what: impl Into<String>, message: impl Into<String>) -> Self {
        DpuError::Parse {
            what: what.into(),
            message: message.into(),
        }
    }

    pub fn command_failed(cmd: &Cmd, out: &Output) -> Self {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .lines()
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        DpuError::CommandFailed {
            program: cmd.program.name().to_string(),
            status: out.status,
            stderr_tail: tail,
        }
    }
}
