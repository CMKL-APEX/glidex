//! Host-network management for glidex.
//!
//! This crate owns everything glidex does to the host network: installing
//! Open vSwitch, bridges, VM ports (tap / vhost-user), and NAT networks.
//! It depends on nothing else in glidex; `glidex-netd` wraps it in a root
//! daemon (see `spec/networking.md`).
//!
//! **Ownership invariant:** it only modifies or deletes objects it has
//! tagged (`external_ids:glidex-owner=glidex` in OVSDB, the `inet glidex`
//! nftables table, files under its own directories).

pub mod bridge;
pub mod ct_meter;
pub mod exec;
pub mod host;
pub mod install;
pub mod ipmigrate;
pub mod names;
pub mod nat;
pub mod nat_meter;
pub mod net;
pub mod source_build;
pub mod stats;
pub mod tuning;
pub mod nic;
pub mod ovn;
pub mod uplink;
pub mod vm_port;
pub mod vsctl;

use serde::{Deserialize, Serialize};

pub use exec::{Cmd, Exec, Output, Program, RecordingExec, SystemExec};

/// Errors, each with a stable protocol code (`OvsError::code`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum OvsError {
    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },

    #[error("not found: {message}")]
    NotFound { message: String },

    #[error("not owned by glidex: {message}")]
    NotOwned { message: String },

    #[error("conflict: {message}")]
    Conflict { message: String },

    #[error("confirmation required: {impact}")]
    ConfirmationRequired { impact: String },

    #[error("unsupported on this host; missing: {}", missing.join(", "))]
    Unsupported { missing: Vec<String> },

    #[error("{ifname} is in use by the host: {}", reasons.join(", "))]
    HostInterfaceInUse { ifname: String, reasons: Vec<String> },

    #[error("migration rolled back: {reason}")]
    MigrationRolledBack { reason: String },

    #[error("{program} exited with {status}: {stderr_tail}")]
    CommandFailed {
        program: String,
        status: i32,
        stderr_tail: String,
    },

    #[error("I/O error: {0}")]
    #[serde(rename = "internal")]
    Io(String),
}

impl OvsError {
    pub fn invalid(message: impl Into<String>) -> Self {
        OvsError::InvalidArgument {
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        OvsError::NotFound {
            message: message.into(),
        }
    }

    pub fn not_owned(message: impl Into<String>) -> Self {
        OvsError::NotOwned {
            message: message.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        OvsError::Conflict {
            message: message.into(),
        }
    }

    pub fn command_failed(cmd: &Cmd, out: &Output) -> Self {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: String = stderr
            .trim()
            .chars()
            .rev()
            .take(500)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        OvsError::CommandFailed {
            program: cmd.program.name().to_string(),
            status: out.status,
            stderr_tail: tail,
        }
    }

    /// Protocol / REST error code.
    pub fn code(&self) -> &'static str {
        match self {
            OvsError::InvalidArgument { .. } => "invalid_argument",
            OvsError::NotFound { .. } => "not_found",
            OvsError::NotOwned { .. } => "not_owned",
            OvsError::Conflict { .. } => "conflict",
            OvsError::ConfirmationRequired { .. } => "confirmation_required",
            OvsError::Unsupported { .. } => "unsupported",
            OvsError::HostInterfaceInUse { .. } => "host_interface_in_use",
            OvsError::MigrationRolledBack { .. } => "migration_rolled_back",
            OvsError::CommandFailed { .. } => "command_failed",
            OvsError::Io(_) => "internal",
        }
    }
}
