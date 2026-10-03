//! Where `glidex-vm-shim` runs (spec/reconciliation.md §8.8).
//!
//! - **systemd**: one `glidex-vm@<id>.service` per instance, started over
//!   the system bus (`zbus`) and authorized by the polkit rule the
//!   installer drops in. The instance lives in its own cgroup, so the
//!   control plane can stop, crash or restart without touching it.
//! - **detached** (dev runs, tests): the shim is started in its own
//!   session; it outlives the control plane as an orphan.

use crate::config::VmRunnerKind;
use crate::paths::VmPaths;
use futures_util::StreamExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Mutex;

const SYSTEMD: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";
const UNIT: &str = "org.freedesktop.systemd1.Unit";

/// The run directory the `glidex-vm@` template unit points at.
pub const SYSTEMD_RUN_DIR: &str = "/run/glidex-cp";

/// The run directory the installed template points at: `SYSTEMD_RUN_DIR`,
/// or `GLIDEX_VM_UNIT_RUN_DIR` (tests with their own template).
fn unit_run_dir() -> PathBuf {
    std::env::var_os("GLIDEX_VM_UNIT_RUN_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(SYSTEMD_RUN_DIR))
}

pub fn unit_name(vm_id: &str) -> String {
    format!("glidex-vm@{}.service", vm_id)
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// The system bus could not be reached: nothing can be decided.
    #[error("systemd is unreachable: {0}")]
    Unreachable(String),
    /// polkit said no: the installer's rule is missing.
    #[error("systemd refused (is the glidex polkit rule installed?): {0}")]
    AccessDenied(String),
    #[error("glidex-vm@.service is not installed: {0}")]
    NoUnit(String),
    #[error("{0}")]
    Failed(String),
}

impl From<zbus::Error> for RunnerError {
    fn from(e: zbus::Error) -> Self {
        let text = e.to_string();
        let name = match &e {
            zbus::Error::MethodError(name, _, _) => name.as_str().to_string(),
            zbus::Error::FDO(f) => f.to_string(),
            _ => String::new(),
        };
        if name.contains("AccessDenied") || name.contains("InteractiveAuthorizationRequired") || text.contains("AccessDenied") {
            RunnerError::AccessDenied(text)
        } else if name.contains("NoSuchUnit") || text.contains("not found") {
            RunnerError::NoUnit(text)
        } else if matches!(e, zbus::Error::InputOutput(_) | zbus::Error::Address(_) | zbus::Error::Handshake(_)) {
            RunnerError::Unreachable(text)
        } else {
            RunnerError::Failed(text)
        }
    }
}

pub enum Runner {
    Systemd(SystemdRunner),
    Detached(DetachedRunner),
}

impl Runner {
    /// `auto` is systemd when the control plane itself runs under systemd
    /// (`INVOCATION_ID` is set).
    pub fn new(kind: VmRunnerKind) -> Self {
        let systemd = match kind {
            VmRunnerKind::Systemd => true,
            VmRunnerKind::Detached => false,
            VmRunnerKind::Auto => std::env::var_os("INVOCATION_ID").is_some(),
        };
        if systemd {
            Runner::Systemd(SystemdRunner { conn: Mutex::new(None) })
        } else {
            Runner::Detached(DetachedRunner)
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Runner::Systemd(_) => "systemd",
            Runner::Detached(_) => "detached",
        }
    }

    /// The record stored in `InstanceRef`.
    pub fn record(&self, vm_id: &str) -> crate::models::Runner {
        match self {
            Runner::Systemd(_) => crate::models::Runner::Systemd { unit: unit_name(vm_id) },
            Runner::Detached(_) => crate::models::Runner::Detached,
        }
    }

    /// Start the shim for `vm_id` (its `launch.json` is written).
    pub async fn start(&self, vm_id: &str, paths: &VmPaths) -> Result<(), RunnerError> {
        match self {
            Runner::Systemd(s) => s.start(vm_id, paths).await,
            Runner::Detached(d) => d.start(vm_id, paths),
        }
    }

    /// Whether the VM's unit is up (§8.7 check 1). `None`: the detached
    /// runner has no unit.
    pub async fn unit_active(&self, vm_id: &str) -> Option<Result<bool, String>> {
        match self {
            Runner::Systemd(s) => Some(s.active(vm_id).await.map_err(|e| e.to_string())),
            Runner::Detached(_) => None,
        }
    }

    /// Escalation when the shim does not answer: stop the unit / SIGTERM
    /// the (verified) shim.
    pub async fn stop(&self, vm_id: &str, shim: Option<(u32, u64)>) -> Result<(), RunnerError> {
        match self {
            Runner::Systemd(s) => s.manager_call("StopUnit", &(unit_name(vm_id), "replace")).await.map(|_: zbus::zvariant::OwnedObjectPath| ()),
            Runner::Detached(_) => signal_verified(shim, nix::sys::signal::Signal::SIGTERM),
        }
    }

    /// Last resort: SIGKILL everything in the unit / the verified pids.
    pub async fn kill(&self, vm_id: &str, pids: &[(u32, u64)]) -> Result<(), RunnerError> {
        match self {
            Runner::Systemd(s) => s.manager_call("KillUnit", &(unit_name(vm_id), "all", 9i32)).await,
            Runner::Detached(_) => {
                for p in pids {
                    signal_verified(Some(*p), nix::sys::signal::Signal::SIGKILL)?;
                }
                Ok(())
            }
        }
    }

    /// VM ids with a loaded `glidex-vm@` unit (orphan detection, §9.4).
    pub async fn list_units(&self) -> Result<Vec<String>, RunnerError> {
        match self {
            Runner::Systemd(s) => s.list().await,
            Runner::Detached(_) => Ok(Vec::new()),
        }
    }

    pub async fn bus_connected(&self) -> Option<bool> {
        match self {
            Runner::Systemd(s) => Some(s.connection().await.is_ok()),
            Runner::Detached(_) => None,
        }
    }
}

/// Signal a process only if it is still the one that started at the
/// recorded time (pids are reused).
fn signal_verified(p: Option<(u32, u64)>, sig: nix::sys::signal::Signal) -> Result<(), RunnerError> {
    let Some((pid, start)) = p else { return Ok(()) };
    if glidex_vm_shim::util::same_process(pid, start).unwrap_or(false) {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), sig).map_err(|e| RunnerError::Failed(e.to_string()))?;
    }
    Ok(())
}

pub struct SystemdRunner {
    conn: Mutex<Option<zbus::Connection>>,
}

impl SystemdRunner {
    /// The system bus connection, opened (and `Subscribe`d) on first use
    /// and reopened after an error.
    async fn connection(&self) -> Result<zbus::Connection, RunnerError> {
        let mut guard = self.conn.lock().await;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        // The system manager; `GLIDEX_SYSTEMD_BUS=session` uses the
        // user's own manager instead (tests: no polkit, no root).
        let c = if std::env::var("GLIDEX_SYSTEMD_BUS").as_deref() == Ok("session") {
            zbus::Connection::session().await
        } else {
            zbus::Connection::system().await
        }
        .map_err(|e| RunnerError::Unreachable(e.to_string()))?;
        // Without Subscribe, systemd sends no JobRemoved signals.
        let _: () = manager(&c).await?.call("Subscribe", &()).await.map_err(RunnerError::from)?;
        *guard = Some(c.clone());
        Ok(c)
    }

    async fn reset(&self) {
        *self.conn.lock().await = None;
    }

    async fn manager_call<B, R>(&self, method: &str, body: &B) -> Result<R, RunnerError>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        let c = self.connection().await?;
        match manager(&c).await?.call(method, body).await {
            Ok(r) => Ok(r),
            Err(e) => {
                let err = RunnerError::from(e);
                if matches!(err, RunnerError::Unreachable(_)) {
                    self.reset().await;
                }
                Err(err)
            }
        }
    }

    async fn start(&self, vm_id: &str, paths: &VmPaths) -> Result<(), RunnerError> {
        if paths.dir != unit_run_dir().join("vms").join(vm_id) {
            return Err(RunnerError::Failed(format!(
                "the systemd runner needs the run directory {} (it is {})",
                unit_run_dir().display(),
                paths.dir.parent().and_then(|p| p.parent()).unwrap_or(&paths.dir).display()
            )));
        }
        let c = self.connection().await?;
        let mgr = manager(&c).await?;
        // Subscribe to job results before starting, so none is missed.
        let mut removed = mgr.receive_signal("JobRemoved").await.map_err(RunnerError::from)?;
        let job: zbus::zvariant::OwnedObjectPath =
            mgr.call("StartUnit", &(unit_name(vm_id), "fail")).await.map_err(RunnerError::from)?;
        let wait = async {
            while let Some(msg) = removed.next().await {
                if let Ok((_id, path, _unit, result)) =
                    msg.body().deserialize::<(u32, zbus::zvariant::OwnedObjectPath, String, String)>()
                {
                    if path == job {
                        return Some(result);
                    }
                }
            }
            None
        };
        // TimeoutStartSec of the unit is 60 s.
        match tokio::time::timeout(Duration::from_secs(75), wait).await {
            Ok(Some(r)) if r == "done" => Ok(()),
            Ok(Some(r)) => Err(RunnerError::Failed(format!("starting {} ended with '{}'", unit_name(vm_id), r))),
            Ok(None) => Err(RunnerError::Unreachable("the bus connection closed".into())),
            Err(_) => Err(RunnerError::Failed(format!("starting {} timed out", unit_name(vm_id)))),
        }
    }

    async fn active(&self, vm_id: &str) -> Result<bool, RunnerError> {
        let c = self.connection().await?;
        let path: zbus::zvariant::OwnedObjectPath = match manager(&c).await?.call("GetUnit", &(unit_name(vm_id),)).await {
            Ok(p) => p,
            Err(e) => {
                return match RunnerError::from(e) {
                    // Not loaded: inactive.
                    RunnerError::NoUnit(_) => Ok(false),
                    RunnerError::Unreachable(m) => {
                        self.reset().await;
                        Err(RunnerError::Unreachable(m))
                    }
                    other => Err(other),
                }
            }
        };
        let unit = zbus::proxy::Builder::<zbus::Proxy>::new(&c)
            .destination(SYSTEMD)?
            .path(path)?
            .interface(UNIT)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await?;
        let state: String = unit.get_property("ActiveState").await?;
        Ok(matches!(state.as_str(), "active" | "activating" | "deactivating" | "reloading"))
    }

    async fn list(&self) -> Result<Vec<String>, RunnerError> {
        type UnitRow = (String, String, String, String, String, String, zbus::zvariant::OwnedObjectPath, u32, String, zbus::zvariant::OwnedObjectPath);
        let rows: Vec<UnitRow> = self
            .manager_call("ListUnitsByPatterns", &(Vec::<String>::new(), vec!["glidex-vm@*.service".to_string()]))
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.0.strip_prefix("glidex-vm@").and_then(|s| s.strip_suffix(".service")).map(String::from))
            .collect())
    }
}

impl From<zbus::zvariant::Error> for RunnerError {
    fn from(e: zbus::zvariant::Error) -> Self {
        RunnerError::Failed(e.to_string())
    }
}

async fn manager(c: &zbus::Connection) -> Result<zbus::Proxy<'static>, RunnerError> {
    Ok(zbus::proxy::Builder::<zbus::Proxy>::new(c)
        .destination(SYSTEMD)?
        .path(MANAGER_PATH)?
        .interface(MANAGER)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?)
}

pub struct DetachedRunner;

/// The shim binary: `GLIDEX_VM_SHIM`, next to this executable (or one
/// level up, for test binaries in `target/<profile>/deps`), or `PATH`.
pub fn shim_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GLIDEX_VM_SHIM") {
        return Some(PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    for d in [Some(dir), dir.parent()].into_iter().flatten() {
        let p = d.join("glidex-vm-shim");
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).map(|d| d.join("glidex-vm-shim")).find(|p| p.is_file())
}

impl DetachedRunner {
    fn start(&self, vm_id: &str, paths: &VmPaths) -> Result<(), RunnerError> {
        use std::os::unix::process::CommandExt;
        let bin = shim_binary().ok_or_else(|| RunnerError::Failed("glidex-vm-shim not found (set GLIDEX_VM_SHIM)".into()))?;
        let mut cmd = std::process::Command::new(bin);
        cmd.args(["--dir", &paths.dir.to_string_lossy(), "--vm", vm_id])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                // Its own session: no signal aimed at the control plane's
                // process group or terminal reaches it.
                nix::unistd::setsid().ok();
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| RunnerError::Failed(format!("cannot start glidex-vm-shim: {}", e)))?;
        // Reap it whenever it exits, so it never lingers as a zombie of
        // ours; if we exit first it is reparented to init.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}
