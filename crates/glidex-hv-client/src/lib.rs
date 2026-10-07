//! Blocking clients for the hypervisors' own control sockets: Cloud
//! Hypervisor's HTTP API and QEMU's QMP. Shared by `glidex-vm-shim` (power
//! button, kill, launch health check) and the control plane (observe,
//! pause/resume, hot-plug); see spec/reconciliation.md §5.2, §16.
//!
//! Every call opens its own connection, so the shim and the control plane
//! never hold one open for long, and a restarted control plane reconnects
//! by simply calling again.

pub mod ch;
pub mod qmp;
pub mod stats;

use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum HvError {
    /// The socket is missing, refused the connection, or dropped it.
    #[error("cannot reach the hypervisor: {0}")]
    Connect(String),
    /// The hypervisor answered with an error.
    #[error("hypervisor API request failed: {0}")]
    Api(String),
    #[error("hypervisor I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// What the guest is doing, as the hypervisor reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestState {
    /// The VMM runs but has no VM (CH without `vm.create`).
    NotCreated,
    /// Configured but not booted (CH `Created`, QEMU `prelaunch`).
    Created,
    Running,
    Paused,
    /// The guest shut down; the VMM is about to exit.
    Shutdown,
}

/// The guest's state and the ids of the devices it has (VFIO ids are
/// `_vfio_…`, spec hypervisors.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub guest: GuestState,
    pub device_ids: Vec<String>,
}

/// How long a single request may take.
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Connect to a Unix socket, mapping every failure to `Connect`.
pub(crate) fn connect(path: &str) -> Result<std::os::unix::net::UnixStream, HvError> {
    let stream = std::os::unix::net::UnixStream::connect(path).map_err(|e| HvError::Connect(format!("{}: {}", path, e)))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}
