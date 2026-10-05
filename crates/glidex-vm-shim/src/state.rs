//! `instance.json`: what the shim reports about its instance (spec §8.4),
//! written atomically on every phase change.

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const INSTANCE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The hypervisor is being started; its socket has not answered yet.
    Launching,
    Running,
    /// The hypervisor is gone; the shim keeps the console until `release`.
    Exited,
}

/// How an instance ended (spec §7.5). `HostReboot` and `Lost` are only
/// ever recorded by the control plane: no shim is left to write them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitCause {
    /// A `stop` or `kill` over `shim.sock` preceded the exit.
    Requested,
    /// SIGTERM to the shim (a unit stop: host shutdown, or an operator).
    Terminated,
    /// Exit status 0 with nothing requested: the guest powered off, or the
    /// hypervisor got a signal it handles gracefully (spec §4, F3/F4).
    CleanExit,
    /// Non-zero exit status or killed by a signal.
    Crashed,
    /// Exited before its API socket answered.
    LaunchFailed,
    HostReboot,
    Lost,
}

impl ExitCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExitCause::Requested => "requested",
            ExitCause::Terminated => "terminated",
            ExitCause::CleanExit => "clean_exit",
            ExitCause::Crashed => "crashed",
            ExitCause::LaunchFailed => "launch_failed",
            ExitCause::HostReboot => "host_reboot",
            ExitCause::Lost => "lost",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopInfo {
    pub requested_at: u64,
    pub grace_secs: u64,
    pub deadline: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    pub at: u64,
    pub cause: ExitCause,
    #[serde(default)]
    pub code: Option<i32>,
    #[serde(default)]
    pub signal: Option<i32>,
    /// For `LaunchFailed`: what the hypervisor printed.
    #[serde(default)]
    pub message: Option<String>,
    /// Final counters for metering (spec/metering.md D12).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ExitUsage>,
}

/// What the unit's cgroup and the hypervisor had counted when the
/// hypervisor exited: the tail since the meter's last sample, which would
/// otherwise be lost with the cgroup and the hypervisor's sockets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitUsage {
    /// The unit cgroup's `cpu.stat usage_usec`, read after reaping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_usage_usec: Option<u64>,
    /// The unit cgroup's `memory.peak`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_peak_bytes: Option<u64>,
    /// Block counters from the last successful poll (at `disks_at`, unix
    /// ms), by launched disk index.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<glidex_hv_client::stats::BlockStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disks_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceFile {
    pub version: u32,
    pub vm_id: String,
    pub instance_id: String,
    pub boot_id: String,
    pub shim_pid: u32,
    pub shim_starttime: u64,
    #[serde(default)]
    pub hypervisor_pid: Option<u32>,
    #[serde(default)]
    pub hypervisor_starttime: Option<u64>,
    pub phase: Phase,
    pub launched_at: u64,
    #[serde(default)]
    pub stop: Option<StopInfo>,
    #[serde(default)]
    pub exit: Option<ExitInfo>,
}

impl InstanceFile {
    /// `Ok(None)` if there is no file.
    pub fn read(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(std::io::Error::other),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::util::write_atomic(path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_example_parses() {
        let v = serde_json::json!({
            "version": 1, "vm_id": "v", "instance_id": "i", "boot_id": "b",
            "shim_pid": 4242, "shim_starttime": 123456,
            "hypervisor_pid": 4250, "hypervisor_starttime": 123470,
            "phase": "exited", "launched_at": 1791000000,
            "stop": { "requested_at": 1791000500, "grace_secs": 30, "deadline": 1791000530 },
            "exit": { "at": 1791000520, "cause": "requested", "code": 0, "signal": null, "message": null }
        });
        let f: InstanceFile = serde_json::from_value(v).unwrap();
        assert_eq!(f.phase, Phase::Exited);
        assert_eq!(f.exit.unwrap().cause, ExitCause::Requested);
        let dir = tempfile::tempdir().unwrap();
        assert!(InstanceFile::read(&dir.path().join("none")).unwrap().is_none());
    }
}
