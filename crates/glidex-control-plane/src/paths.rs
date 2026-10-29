//! Where the control plane keeps per-VM runtime files (spec/11-security.md §9).
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
        let first = d
            .to_string_lossy()
            .split(':')
            .next()
            .unwrap_or_default()
            .to_string();
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

/// A path the control plane keeps its own files under: UTF-8, with no
/// `..` anywhere, so it can't climb out of the directory it names. The
/// check is on the text, so it also refuses a name like `a..b`.
pub fn checked_path(path: &Path) -> Result<PathBuf, String> {
    let Some(s) = path.to_str() else {
        return Err(format!("{}: not UTF-8", path.display()));
    };
    if s.contains("..") {
        return Err(format!("{s}: may not contain '..'"));
    }
    Ok(PathBuf::from(s))
}

/// `dir/name` for one of the control plane's own files. `dir` passed
/// [`checked_path`] when it was configured and `name` is one this code
/// builds, so a path that fails the check is a bug: this panics rather
/// than touch a file outside `dir`.
pub fn file_in(dir: &Path, name: &str) -> PathBuf {
    assert!(!name.contains('/'), "{name:?} is not a file name");
    checked_path(&dir.join(name)).unwrap_or_else(|e| panic!("{e}"))
}

/// `target` as a path relative to the directory `from`, both resolved
/// first (`None` if either can't be). A linked disk names its image this
/// way, so the overlay stays valid wherever the two directories are
/// mounted, as long as they keep their places relative to each other
/// (spec/24-storage.md S8).
pub fn relative_to(target: &Path, from: &Path) -> Option<PathBuf> {
    let target = target.canonicalize().ok()?;
    let from = from.canonicalize().ok()?;
    let common = target
        .components()
        .zip(from.components())
        .take_while(|(a, b)| a == b)
        .count();
    let mut rel: PathBuf = from.components().skip(common).map(|_| "..").collect();
    rel.extend(target.components().skip(common));
    Some(rel)
}

pub fn vm_dir(vm_id: &str) -> PathBuf {
    run_dir().join("vms").join(vm_id)
}

/// Runtime file paths of a VM, inside its private directory
/// (spec/12-reconciliation.md §5.1).
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
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        return Err(std::io::Error::other(format!(
            "{} is not a directory",
            path.display()
        )));
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
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(&run)?;
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
    fn relative_paths_climb_out_of_the_base() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (images, disks) = (tmp.path().join("images"), tmp.path().join("disks"));
        std::fs::create_dir_all(&images).unwrap();
        std::fs::create_dir_all(disks.join("deep")).unwrap();
        let img = images.join("a.qcow2");
        std::fs::write(&img, b"").unwrap();
        assert_eq!(
            relative_to(&img, &disks).unwrap(),
            Path::new("../images/a.qcow2")
        );
        assert_eq!(
            relative_to(&img, &disks.join("deep")).unwrap(),
            Path::new("../../images/a.qcow2")
        );
        assert_eq!(relative_to(&img, &images).unwrap(), Path::new("a.qcow2"));
        assert_eq!(relative_to(&tmp.path().join("gone"), &disks), None);
    }

    #[test]
    fn checked_paths_stay_put() {
        assert_eq!(
            checked_path(Path::new("/var/lib/glidex/images")).unwrap(),
            Path::new("/var/lib/glidex/images")
        );
        assert!(checked_path(Path::new("/var/lib/glidex/../../etc")).is_err());
        assert_eq!(file_in(Path::new("/d"), "x.qcow2"), Path::new("/d/x.qcow2"));
    }

    #[test]
    #[should_panic]
    fn file_in_refuses_a_climb() {
        file_in(Path::new("/d"), "..");
    }

    #[test]
    #[should_panic]
    fn file_in_refuses_a_subpath() {
        file_in(Path::new("/d"), "a/b");
    }

    #[test]
    fn private_dir_refuses_a_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("f");
        std::fs::write(&f, b"").unwrap();
        assert!(ensure_private_dir(&f).is_err());
    }
}
