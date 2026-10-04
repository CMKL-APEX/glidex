//! `glidex-vm-shim`: the per-VM supervisor (spec/reconciliation.md §8).
//!
//! The shim is the hypervisor's parent and owns its console (PTY master,
//! console socket and log), so a VM keeps running while the control plane
//! is down. It knows one VM and nothing else: it never writes the database
//! and never talks to netd or systemd.
//!
//! The library half is shared with the control plane: the file formats it
//! writes (`launch.json`) and reads back (`instance.json`), the `shim.sock`
//! protocol and its client, and a few host helpers (process start times,
//! boot id). The supervisor itself is [`supervisor::run`].

pub mod client;
pub mod launch;
pub mod proto;
pub mod proxy;
pub mod sd;
pub mod state;
pub mod supervisor;
pub mod util;

pub use launch::{Fallback, HypervisorKind, LaunchFile};
pub use state::{ExitCause, ExitInfo, InstanceFile, Phase, StopInfo};

/// File names inside a VM's runtime directory (§5.1).
pub const LAUNCH_FILE: &str = "launch.json";
pub const INSTANCE_FILE: &str = "instance.json";
pub const SHIM_SOCKET: &str = "shim.sock";
