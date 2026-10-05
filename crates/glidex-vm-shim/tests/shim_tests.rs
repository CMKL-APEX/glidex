//! The shim binary against a real, idle Cloud Hypervisor (a VMM with no VM:
//! its API answers, nothing boots). Skipped when `cloud-hypervisor` is not
//! installed. Booting real guests is covered by the control plane's
//! ignored end-to-end tests.

use glidex_vm_shim::client::ShimClient;
use glidex_vm_shim::launch::{HypervisorKind, LaunchFile, LAUNCH_VERSION};
use glidex_vm_shim::state::{ExitCause, InstanceFile, Phase};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn cloud_hypervisor() -> Option<PathBuf> {
    ["/usr/local/bin/cloud-hypervisor", "/usr/bin/cloud-hypervisor"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
}

fn launch_file(dir: &Path, argv: Vec<String>) -> LaunchFile {
    let p = |n: &str| dir.join(n).to_string_lossy().into_owned();
    LaunchFile {
        version: LAUNCH_VERSION,
        vm_id: "vm-test".into(),
        instance_id: "inst-1".into(),
        hypervisor: HypervisorKind::CloudHypervisor,
        argv,
        fallback: None,
        api_socket: p("api.sock"),
        console_socket: p("console.sock"),
        shim_socket: p("shim.sock"),
        log_path: p("console.log"),
        log_max_bytes: 1 << 20,
        ready_timeout_secs: 10,
        host_shutdown_grace_secs: 1,
        spec: serde_json::Value::Null,
        meter_poll_secs: 0,
    }
}

fn ch_launch(dir: &Path, ch: &Path) -> LaunchFile {
    let api = dir.join("api.sock");
    launch_file(dir, vec![ch.to_string_lossy().into_owned(), "--api-socket".into(), format!("path={}", api.display())])
}

/// The shim, killed (with its hypervisor) when the test ends, pass or fail.
struct Shim {
    child: Child,
    dir: PathBuf,
}

impl std::ops::Deref for Shim {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl std::ops::DerefMut for Shim {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        if let Some(pid) = instance(&self.dir).and_then(|i| i.hypervisor_pid) {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_shim(dir: &Path, launch: &LaunchFile) -> Shim {
    launch.write(&dir.join(glidex_vm_shim::LAUNCH_FILE)).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_glidex-vm-shim"))
        .args(["--dir", dir.to_str().unwrap(), "--vm", &launch.vm_id])
        .env_remove("INVOCATION_ID")
        .env_remove("NOTIFY_SOCKET")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    Shim { child, dir: dir.to_path_buf() }
}

fn instance(dir: &Path) -> Option<InstanceFile> {
    InstanceFile::read(&dir.join(glidex_vm_shim::INSTANCE_FILE)).ok().flatten()
}

fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_running(dir: &Path) -> InstanceFile {
    wait_for("running", Duration::from_secs(15), || instance(dir).is_some_and(|i| i.phase != Phase::Launching));
    let i = instance(dir).unwrap();
    assert_eq!(i.phase, Phase::Running, "{:?}", i.exit);
    wait_for("shim.sock", Duration::from_secs(5), || dir.join("shim.sock").exists());
    i
}

fn wait_exited(dir: &Path) -> InstanceFile {
    wait_for("exit", Duration::from_secs(15), || instance(dir).is_some_and(|i| i.phase == Phase::Exited));
    instance(dir).unwrap()
}

fn alive(pid: u32) -> bool {
    glidex_vm_shim::util::proc_starttime(pid).unwrap().is_some()
        && !std::fs::read_to_string(format!("/proc/{}/stat", pid)).unwrap_or_default().contains(") Z ")
}

fn wait_exit_status(child: &mut Shim) -> i32 {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(s) = child.try_wait().unwrap() {
            return s.code().unwrap_or(-1);
        }
        assert!(Instant::now() < end, "shim did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn requested_stop_then_release() {
    let Some(ch) = cloud_hypervisor() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut shim = start_shim(dir.path(), &ch_launch(dir.path(), &ch));
    let inst = wait_running(dir.path());
    let hv = inst.hypervisor_pid.unwrap();
    assert!(alive(hv));
    assert!(dir.path().join("console.sock").exists());

    let mut c = ShimClient::connect(&dir.path().join("shim.sock")).unwrap();
    assert_eq!(c.status().unwrap().instance_id, "inst-1");
    // Release refuses while the hypervisor runs.
    assert!(c.release().is_err());
    // An idle VMM has no ACPI: the power button fails and it is killed.
    c.stop(30).unwrap();
    let inst = wait_exited(dir.path());
    assert_eq!(inst.exit.as_ref().unwrap().cause, ExitCause::Requested);
    assert!(!alive(hv));
    // The console stays up until release.
    assert!(std::os::unix::net::UnixStream::connect(dir.path().join("console.sock")).is_ok());

    let mut c = ShimClient::connect(&dir.path().join("shim.sock")).unwrap();
    c.release().unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
    for s in ["shim.sock", "console.sock", "api.sock"] {
        assert!(!dir.path().join(s).exists(), "{s} left behind");
    }
    let log = std::fs::read_to_string(dir.path().join("console.log")).unwrap();
    assert!(log.contains("--- glidex: instance inst-1 started"), "{log}");
    assert!(log.contains("--- glidex: instance inst-1 exited: requested"), "{log}");
}

#[test]
fn a_killed_hypervisor_is_a_crash() {
    let Some(ch) = cloud_hypervisor() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut shim = start_shim(dir.path(), &ch_launch(dir.path(), &ch));
    let hv = wait_running(dir.path()).hypervisor_pid.unwrap();
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(hv as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
    let exit = wait_exited(dir.path()).exit.unwrap();
    assert_eq!((exit.cause, exit.signal), (ExitCause::Crashed, Some(9)));
    ShimClient::connect(&dir.path().join("shim.sock")).unwrap().release().unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
}

#[test]
fn sigterm_stops_and_releases_on_its_own() {
    let Some(ch) = cloud_hypervisor() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut shim = start_shim(dir.path(), &ch_launch(dir.path(), &ch));
    let hv = wait_running(dir.path()).hypervisor_pid.unwrap();
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(shim.id() as i32), nix::sys::signal::Signal::SIGTERM).unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
    assert!(!alive(hv));
    assert_eq!(instance(dir.path()).unwrap().exit.unwrap().cause, ExitCause::Terminated);
    assert!(!dir.path().join("shim.sock").exists());
}

#[test]
fn the_hypervisor_dies_with_its_shim() {
    let Some(ch) = cloud_hypervisor() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut shim = start_shim(dir.path(), &ch_launch(dir.path(), &ch));
    let hv = wait_running(dir.path()).hypervisor_pid.unwrap();
    shim.child.kill().unwrap();
    shim.child.wait().unwrap();
    wait_for("hypervisor death", Duration::from_secs(5), || !alive(hv));
}

#[test]
fn binaries_off_the_allowlist_never_run() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let launch = launch_file(dir.path(), vec!["/bin/sh".into(), "-c".into(), format!("touch {}", marker.display())]);
    let mut shim = start_shim(dir.path(), &launch);
    assert_eq!(wait_exit_status(&mut shim), 2);
    assert!(!marker.exists());
    let exit = instance(dir.path()).unwrap().exit.unwrap();
    assert_eq!(exit.cause, ExitCause::LaunchFailed);
    assert!(exit.message.unwrap().contains("not an allowed hypervisor"));
}

#[test]
fn a_failed_launch_reports_the_hypervisors_output() {
    let Some(ch) = cloud_hypervisor() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut launch = ch_launch(dir.path(), &ch);
    launch.argv.extend(["--kernel".into(), "/nonexistent".into(), "--net".into(), "bogus=1".into()]);
    let mut shim = start_shim(dir.path(), &launch);
    let inst = wait_exited(dir.path());
    let exit = inst.exit.unwrap();
    assert_eq!(exit.cause, ExitCause::LaunchFailed);
    assert!(exit.message.as_deref().unwrap_or("").contains("bogus"), "{:?}", exit.message);
    // The console and shim.sock stay up so the failure can be inspected.
    ShimClient::connect(&dir.path().join("shim.sock")).unwrap().release().unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
    assert!(std::fs::read_to_string(dir.path().join("console.log")).unwrap().contains("bogus"));
}
