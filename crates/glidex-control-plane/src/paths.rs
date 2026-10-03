//! Where the control plane keeps per-VM runtime files (spec/security.md §9).
//!
//! Each VM gets a private directory `<run dir>/vms/<id>/` (mode 0700) for
//! its hypervisor API socket, console socket, console log and generated
//! cloud-init seed. The hypervisor API socket gives full control of the
//! VM with no glidex checks, so only the control plane may open it;
//! people reach consoles through the API (`vm.console`).

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The control plane's runtime directory:
/// - `RUNTIME_DIRECTORY` (systemd `RuntimeDirectory=glidex-cp`), else
/// - `GLIDEX_RUN_DIR`, else
/// - `$XDG_RUNTIME_DIR/glidex`, else
/// - `/tmp/glidex-<uid>`.
pub fn run_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("RUNTIME_DIRECTORY") {
        // systemd passes a colon-separated list when there are several.
        let first = d.to_string_lossy().split(':').next().unwrap_or_default().to_string();
        if !first.is_empty() {
            return PathBuf::from(first);
        }
    }
    if let Some(d) = std::env::var_os("GLIDEX_RUN_DIR") {
        return PathBuf::from(d);
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("glidex");
    }
    PathBuf::from(format!("/tmp/glidex-{}", nix::unistd::geteuid()))
}

pub fn vm_dir(vm_id: &str) -> PathBuf {
    run_dir().join("vms").join(vm_id)
}

/// Runtime file paths of a VM, inside its private directory
/// (spec/reconciliation.md §5.1).
#[derive(Debug, Clone)]
pub struct VmPaths {
    pub dir: PathBuf,
    pub api_socket: String,
    pub console_socket: String,
    pub log: String,
    pub cloud_init: String,
    pub launch: PathBuf,
    pub instance: PathBuf,
    pub shim_socket: String,
}

impl VmPaths {
    /// The rotated console log (`console.log.1`).
    pub fn previous_log(&self) -> String {
        format!("{}.1", self.log)
    }
}

pub fn vm_paths(vm_id: &str) -> VmPaths {
    let dir = vm_dir(vm_id);
    let p = |name: &str| dir.join(name).to_string_lossy().into_owned();
    VmPaths {
        api_socket: p("api.sock"),
        console_socket: p("console.sock"),
        log: p("console.log"),
        cloud_init: p("cloudinit.img"),
        launch: dir.join(glidex_vm_shim::LAUNCH_FILE),
        instance: dir.join(glidex_vm_shim::INSTANCE_FILE),
        shim_socket: p(glidex_vm_shim::SHIM_SOCKET),
        dir,
    }
}

/// Create `path` (and parents) and make sure it is a directory owned by
/// us with mode 0700. Refuses a directory owned by someone else, so a
/// pre-created directory in a shared location can't be used against us.
pub fn ensure_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Err(std::io::Error::other(format!("{} is not a directory", path.display())));
    }
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(std::io::Error::other(format!(
            "{} is owned by uid {}, not by this process",
            path.display(),
            meta.uid()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Create a VM's private directory. The run directory itself also holds
/// the API sockets, so it stays traversable; `vms/` and below are 0700.
pub fn ensure_vm_dir(vm_id: &str) -> std::io::Result<PathBuf> {
    let run = run_dir();
    std::fs::DirBuilder::new().recursive(true).mode(0o755).create(&run)?;
    ensure_private_dir(&run.join("vms"))?;
    let dir = vm_dir(vm_id);
    ensure_private_dir(&dir)?;
    Ok(dir)
}

pub fn remove_vm_dir(vm_id: &str) {
    let _ = std::fs::remove_dir_all(vm_dir(vm_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dir_is_0700_and_fixes_loose_modes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let d = tmp.path().join("a/b");
        ensure_private_dir(&d).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().mode() & 0o777, 0o700);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&d).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn private_dir_refuses_a_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("f");
        std::fs::write(&f, b"").unwrap();
        assert!(ensure_private_dir(&f).is_err());
    }
}
