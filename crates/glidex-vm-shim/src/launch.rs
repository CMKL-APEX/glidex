//! `launch.json`: everything the shim needs, written by the control plane
//! before it starts the shim (spec/reconciliation.md §8.1), and the
//! hypervisor allowlist (§8.3).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const LAUNCH_VERSION: u32 = 1;

/// Which hypervisor `argv` runs: decides how the shim checks it is up and
/// how it presses the power button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HypervisorKind {
    CloudHypervisor,
    Qemu,
}

/// QEMU's "retry once without `-cpu host`" (hypervisors.md): run `argv`
/// instead if the first attempt exits before its socket answers and its
/// output contains one of `when_output_matches` (case-insensitive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fallback {
    pub argv: Vec<String>,
    pub when_output_matches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchFile {
    pub version: u32,
    pub vm_id: String,
    pub instance_id: String,
    pub hypervisor: HypervisorKind,
    /// The hypervisor command line, carrying the whole VM config (D9).
    pub argv: Vec<String>,
    #[serde(default)]
    pub fallback: Option<Fallback>,
    pub api_socket: String,
    pub console_socket: String,
    pub shim_socket: String,
    pub log_path: String,
    pub log_max_bytes: u64,
    pub ready_timeout_secs: u64,
    pub host_shutdown_grace_secs: u64,
    /// The VM spec this launch was built from, for the control plane's
    /// `RestartRequired` comparison (§7.3). The shim ignores it.
    #[serde(default)]
    pub spec: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("cannot read {0}: {1}")]
    Read(String, std::io::Error),
    #[error("{0} is not a valid launch file: {1}")]
    Parse(String, serde_json::Error),
    #[error("unsupported launch file version {0}")]
    Version(u32),
    #[error("launch file is for VM {found}, not {expected}")]
    WrongVm { expected: String, found: String },
    #[error("{0}")]
    Invalid(String),
    #[error("{0} is not an allowed hypervisor binary (see /etc/glidex/vm-shim.json)")]
    NotAllowed(String),
}

impl LaunchFile {
    pub fn read(path: &Path) -> Result<Self, LaunchError> {
        let shown = path.display().to_string();
        let bytes = std::fs::read(path).map_err(|e| LaunchError::Read(shown.clone(), e))?;
        let f: LaunchFile = serde_json::from_slice(&bytes).map_err(|e| LaunchError::Parse(shown, e))?;
        if f.version != LAUNCH_VERSION {
            return Err(LaunchError::Version(f.version));
        }
        Ok(f)
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::util::write_atomic(path, &bytes)
    }

    /// Structural checks plus the allowlist for every command it may run.
    pub fn validate(&self, expected_vm: Option<&str>, allowed: &[PathBuf]) -> Result<(), LaunchError> {
        if let Some(vm) = expected_vm {
            if vm != self.vm_id {
                return Err(LaunchError::WrongVm { expected: vm.to_string(), found: self.vm_id.clone() });
            }
        }
        if self.argv.is_empty() {
            return Err(LaunchError::Invalid("argv is empty".into()));
        }
        for path in [&self.api_socket, &self.console_socket, &self.shim_socket, &self.log_path] {
            if !Path::new(path).is_absolute() {
                return Err(LaunchError::Invalid(format!("{} is not an absolute path", path)));
            }
        }
        check_allowed(&self.argv[0], allowed)?;
        if let Some(fb) = &self.fallback {
            if fb.argv.is_empty() {
                return Err(LaunchError::Invalid("fallback argv is empty".into()));
            }
            check_allowed(&fb.argv[0], allowed)?;
        }
        Ok(())
    }
}

fn check_allowed(binary: &str, allowed: &[PathBuf]) -> Result<(), LaunchError> {
    let canon = std::fs::canonicalize(binary).map_err(|_| LaunchError::NotAllowed(binary.to_string()))?;
    if allowed.contains(&canon) {
        Ok(())
    } else {
        Err(LaunchError::NotAllowed(binary.to_string()))
    }
}

/// Where the installer writes the allowlist (root-owned, 0644).
pub const ALLOWLIST_PATH: &str = "/etc/glidex/vm-shim.json";

#[derive(Deserialize)]
struct AllowlistFile {
    hypervisors: Vec<String>,
}

/// The canonical paths of the hypervisor binaries the shim may run:
/// `/etc/glidex/vm-shim.json`, or else every `cloud-hypervisor` and
/// `qemu-system-<arch>` in `/usr/local/bin` and `/usr/bin`.
///
/// Outside systemd (no `INVOCATION_ID`: dev runs and tests), the
/// colon-separated `GLIDEX_VM_SHIM_ALLOW` adds to the list. A unit's
/// environment is fixed by its unit file, so the control plane cannot use
/// this to widen what a `glidex-vm@` unit runs.
pub fn allowed_binaries() -> Vec<PathBuf> {
    allowed_binaries_from(Path::new(ALLOWLIST_PATH))
}

pub fn allowed_binaries_from(file: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read(file) {
        Ok(bytes) => match serde_json::from_slice::<AllowlistFile>(&bytes) {
            Ok(f) => f.hypervisors.iter().filter_map(|p| std::fs::canonicalize(p).ok()).collect(),
            Err(e) => {
                eprintln!("glidex-vm-shim: ignoring unreadable {}: {}", file.display(), e);
                Vec::new()
            }
        },
        Err(_) => default_binaries(),
    };
    if std::env::var_os("INVOCATION_ID").is_none() {
        if let Ok(extra) = std::env::var("GLIDEX_VM_SHIM_ALLOW") {
            out.extend(extra.split(':').filter(|s| !s.is_empty()).filter_map(|p| std::fs::canonicalize(p).ok()));
        }
    }
    out
}

fn default_binaries() -> Vec<PathBuf> {
    let qemu = format!("qemu-system-{}", std::env::consts::ARCH);
    ["/usr/local/bin", "/usr/bin"]
        .iter()
        .flat_map(|dir| ["cloud-hypervisor", qemu.as_str()].map(|name| Path::new(dir).join(name)))
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample(dir: &Path, argv0: &str) -> LaunchFile {
        let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
        LaunchFile {
            version: LAUNCH_VERSION,
            vm_id: "vm-1".into(),
            instance_id: "inst-1".into(),
            hypervisor: HypervisorKind::CloudHypervisor,
            argv: vec![argv0.into(), "--api-socket".into()],
            fallback: None,
            api_socket: p("api.sock"),
            console_socket: p("console.sock"),
            shim_socket: p("shim.sock"),
            log_path: p("console.log"),
            log_max_bytes: 1 << 20,
            ready_timeout_secs: 5,
            host_shutdown_grace_secs: 0,
            spec: serde_json::Value::Null,
        }
    }

    #[test]
    fn round_trips_and_checks_version() {
        let dir = tempfile::tempdir().unwrap();
        let f = sample(dir.path(), "/bin/true");
        let path = dir.path().join(crate::LAUNCH_FILE);
        f.write(&path).unwrap();
        assert_eq!(LaunchFile::read(&path).unwrap(), f);
        let mut v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v["version"] = 2.into();
        std::fs::write(&path, v.to_string()).unwrap();
        assert!(matches!(LaunchFile::read(&path), Err(LaunchError::Version(2))));
    }

    #[test]
    fn allowlist_is_enforced_on_canonical_paths() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("hv");
        std::fs::write(&bin, "").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&bin, &link).unwrap();
        let allowed = vec![std::fs::canonicalize(&bin).unwrap()];

        let f = sample(dir.path(), link.to_str().unwrap());
        f.validate(Some("vm-1"), &allowed).unwrap();
        assert!(matches!(f.validate(Some("vm-2"), &allowed), Err(LaunchError::WrongVm { .. })));
        assert!(matches!(sample(dir.path(), "/bin/sh").validate(None, &allowed), Err(LaunchError::NotAllowed(_))));

        let mut fb = sample(dir.path(), bin.to_str().unwrap());
        fb.fallback = Some(Fallback { argv: vec!["/bin/sh".into()], when_output_matches: vec![] });
        assert!(matches!(fb.validate(None, &allowed), Err(LaunchError::NotAllowed(_))));
    }

    #[test]
    fn allowlist_file_replaces_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("hv");
        std::fs::write(&bin, "").unwrap();
        let file = dir.path().join("vm-shim.json");
        std::fs::write(&file, serde_json::json!({"hypervisors": [bin, "/nonexistent/qemu"]}).to_string()).unwrap();
        let list = allowed_binaries_from(&file);
        assert!(list.contains(&std::fs::canonicalize(&bin).unwrap()));
        assert!(!list.iter().any(|p| p.ends_with("cloud-hypervisor")));
    }
}
