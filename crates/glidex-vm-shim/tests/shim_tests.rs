//! The shim binary against a real, idle Cloud Hypervisor (a VMM with no VM:
//! its API answers, nothing boots). Skipped when `cloud-hypervisor` is not
//! installed. Booting real guests is covered by the control plane's
//! ignored end-to-end tests.

use glidex_vm_shim::client::ShimClient;
use glidex_vm_shim::launch::{HypervisorKind, LAUNCH_VERSION, LaunchFile};
use glidex_vm_shim::state::{ExitCause, InstanceFile, Phase};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn cloud_hypervisor() -> Option<PathBuf> {
    [
        "/usr/local/bin/cloud-hypervisor",
        "/usr/bin/cloud-hypervisor",
    ]
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
        cpu_affinity: vec![],
        vcpu_pins: vec![],
        job: false,
        volume_locks: vec![],
    }
}

fn ch_launch(dir: &Path, ch: &Path) -> LaunchFile {
    let api = dir.join("api.sock");
    launch_file(
        dir,
        vec![
            ch.to_string_lossy().into_owned(),
            "--api-socket".into(),
            format!("path={}", api.display()),
        ],
    )
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
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_shim(dir: &Path, launch: &LaunchFile) -> Shim {
    launch
        .write(&dir.join(glidex_vm_shim::LAUNCH_FILE))
        .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_glidex-vm-shim"))
        .args(["--dir", dir.to_str().unwrap(), "--vm", &launch.vm_id])
        .env_remove("INVOCATION_ID")
        .env("GLIDEX_VM_SHIM_ALLOW", PYTHON)
        .env_remove("NOTIFY_SOCKET")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    Shim {
        child,
        dir: dir.to_path_buf(),
    }
}

fn instance(dir: &Path) -> Option<InstanceFile> {
    InstanceFile::read(&dir.join(glidex_vm_shim::INSTANCE_FILE))
        .ok()
        .flatten()
}

fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {}", what);
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_running(dir: &Path) -> InstanceFile {
    wait_for("running", Duration::from_secs(15), || {
        instance(dir).is_some_and(|i| i.phase != Phase::Launching)
    });
    let i = instance(dir).unwrap();
    assert_eq!(i.phase, Phase::Running, "{:?}", i.exit);
    wait_for("shim.sock", Duration::from_secs(5), || {
        dir.join("shim.sock").exists()
    });
    i
}

fn wait_exited(dir: &Path) -> InstanceFile {
    wait_for("exit", Duration::from_secs(15), || {
        instance(dir).is_some_and(|i| i.phase == Phase::Exited)
    });
    instance(dir).unwrap()
}

fn alive(pid: u32) -> bool {
    glidex_vm_shim::util::proc_starttime(pid).unwrap().is_some()
        && !std::fs::read_to_string(format!("/proc/{}/stat", pid))
            .unwrap_or_default()
            .contains(") Z ")
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
    assert!(
        log.contains("--- glidex: instance inst-1 exited: requested"),
        "{log}"
    );
}

#[test]
fn a_killed_hypervisor_is_a_crash() {
    let Some(ch) = cloud_hypervisor() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut shim = start_shim(dir.path(), &ch_launch(dir.path(), &ch));
    let hv = wait_running(dir.path()).hypervisor_pid.unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(hv as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    let exit = wait_exited(dir.path()).exit.unwrap();
    assert_eq!((exit.cause, exit.signal), (ExitCause::Crashed, Some(9)));
    ShimClient::connect(&dir.path().join("shim.sock"))
        .unwrap()
        .release()
        .unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
}

#[test]
fn sigterm_stops_and_releases_on_its_own() {
    let Some(ch) = cloud_hypervisor() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut shim = start_shim(dir.path(), &ch_launch(dir.path(), &ch));
    let hv = wait_running(dir.path()).hypervisor_pid.unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(shim.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
    assert!(!alive(hv));
    assert_eq!(
        instance(dir.path()).unwrap().exit.unwrap().cause,
        ExitCause::Terminated
    );
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
    let launch = launch_file(
        dir.path(),
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("touch {}", marker.display()),
        ],
    );
    let mut shim = start_shim(dir.path(), &launch);
    assert_eq!(wait_exit_status(&mut shim), 2);
    assert!(!marker.exists());
    let exit = instance(dir.path()).unwrap().exit.unwrap();
    assert_eq!(exit.cause, ExitCause::LaunchFailed);
    assert!(exit.message.unwrap().contains("not an allowed hypervisor"));
}

/// A disk another holder has locked keeps the hypervisor from starting at
/// all (spec/storage.md S16); a free one is held for the shim's life.
#[test]
fn a_locked_disk_stops_the_launch() {
    let Some(ch) = cloud_hypervisor() else { return };
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join("d.lock");
    let held = glidex_vm_shim::util::try_lock_file(&lock).unwrap().unwrap();
    let mut launch = ch_launch(dir.path(), &ch);
    launch.volume_locks = vec![lock.to_string_lossy().into_owned()];
    let mut shim = start_shim(dir.path(), &launch);
    assert_eq!(wait_exit_status(&mut shim), 2);
    let exit = instance(dir.path()).unwrap().exit.unwrap();
    assert_eq!(exit.cause, ExitCause::LaunchFailed);
    assert!(
        exit.message.as_deref().unwrap_or("").contains("is locked"),
        "{:?}",
        exit.message
    );
    assert!(
        !dir.path().join("api.sock").exists(),
        "the hypervisor never ran"
    );
    drop(held);
}

#[test]
fn a_failed_launch_reports_the_hypervisors_output() {
    let Some(ch) = cloud_hypervisor() else { return };
    let dir = tempfile::tempdir().unwrap();
    let mut launch = ch_launch(dir.path(), &ch);
    launch.argv.extend([
        "--kernel".into(),
        "/nonexistent".into(),
        "--net".into(),
        "bogus=1".into(),
    ]);
    let mut shim = start_shim(dir.path(), &launch);
    let inst = wait_exited(dir.path());
    let exit = inst.exit.unwrap();
    assert_eq!(exit.cause, ExitCause::LaunchFailed);
    assert!(
        exit.message.as_deref().unwrap_or("").contains("bogus"),
        "{:?}",
        exit.message
    );
    // The console and shim.sock stay up so the failure can be inspected.
    ShimClient::connect(&dir.path().join("shim.sock"))
        .unwrap()
        .release()
        .unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
    assert!(
        std::fs::read_to_string(dir.path().join("console.log"))
            .unwrap()
            .contains("bogus")
    );
}

/// A fake QEMU: `vcpus` idle threads standing in for vCPUs and a QMP
/// monitor that answers `query-cpus-fast` with their thread ids. Runs
/// without KVM, so the placement tests run everywhere.
const FAKE_QEMU: &str = r#"
import json, os, socket, sys, threading, time
path, n = sys.argv[1], int(sys.argv[2])
ids, hold = [], threading.Event()
def vcpu():
    ids.append(threading.get_native_id())
    hold.wait()
for _ in range(n):
    threading.Thread(target=vcpu, daemon=True).start()
while len(ids) < n:
    time.sleep(0.01)
s = socket.socket(socket.AF_UNIX)
s.bind(path)
s.listen(4)
while True:
    c, _ = s.accept()
    f = c.makefile("rwb", buffering=0)
    f.write(b'{"QMP": {"version": {}}}\r\n')
    for line in f:
        cmd = json.loads(line).get("execute")
        if cmd == "quit":
            os._exit(0)
        r = [{"cpu-index": i, "thread-id": t} for i, t in enumerate(ids)] if cmd == "query-cpus-fast" else {}
        f.write((json.dumps({"return": r}) + "\r\n").encode())
    c.close()
"#;

const PYTHON: &str = "/usr/bin/python3";

fn fake_qemu_launch(dir: &Path, vcpus: u32) -> Option<LaunchFile> {
    if !Path::new(PYTHON).exists() {
        return None;
    }
    let api = dir.join("api.sock");
    let mut l = launch_file(
        dir,
        vec![
            PYTHON.into(),
            "-c".into(),
            FAKE_QEMU.into(),
            api.to_string_lossy().into_owned(),
            vcpus.to_string(),
        ],
    );
    l.hypervisor = HypervisorKind::Qemu;
    Some(l)
}

/// `Cpus_allowed_list` of every thread of `pid`, by thread id.
fn allowed_lists(pid: u32) -> Vec<(i32, String)> {
    std::fs::read_dir(format!("/proc/{}/task", pid))
        .unwrap()
        .filter_map(|t| {
            let t = t.unwrap();
            let status = std::fs::read_to_string(t.path().join("status")).ok()?;
            let list = status
                .lines()
                .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))?
                .trim()
                .to_string();
            Some((t.file_name().to_str()?.parse().ok()?, list))
        })
        .collect()
}

/// Two CPUs this test may use (the same one twice on a 1-CPU host).
fn two_cpus() -> (u32, u32) {
    let mine = glidex_vm_shim::util::own_affinity().unwrap();
    (mine[0], *mine.last().unwrap())
}

#[test]
fn the_hypervisor_runs_on_its_cpus_and_can_be_rebound() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut launch) = fake_qemu_launch(dir.path(), 2) else {
        return;
    };
    let (first, last) = two_cpus();
    launch.cpu_affinity = vec![first];
    let mut shim = start_shim(dir.path(), &launch);
    let hv = wait_running(dir.path()).hypervisor_pid.unwrap();
    let lists = allowed_lists(hv);
    assert!(lists.len() >= 3, "{lists:?}");
    assert!(
        lists.iter().all(|(_, l)| *l == first.to_string()),
        "{lists:?}"
    );
    // The shim itself stays unbound.
    let mine = glidex_vm_shim::util::cpu_list(&glidex_vm_shim::util::own_affinity().unwrap());
    assert_eq!(allowed_lists(shim.id())[0].1, mine);

    let mut c = ShimClient::connect(&dir.path().join("shim.sock")).unwrap();
    let n = c.set_affinity(&[last]).unwrap();
    let lists = allowed_lists(hv);
    assert_eq!(n as usize, lists.len());
    assert!(
        lists.iter().all(|(_, l)| *l == last.to_string()),
        "{lists:?}"
    );
    assert!(matches!(
        c.set_affinity(&[]),
        Err(glidex_vm_shim::client::ShimError::Remote { code, .. }) if code == "invalid_argument"
    ));

    c.kill().unwrap();
    wait_exited(dir.path());
    let mut c = ShimClient::connect(&dir.path().join("shim.sock")).unwrap();
    assert!(matches!(
        c.set_affinity(&[last]),
        Err(glidex_vm_shim::client::ShimError::Remote { code, .. }) if code == "not_running"
    ));
    c.release().unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
}

#[test]
fn qemu_vcpus_are_pinned_once_qmp_answers() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut launch) = fake_qemu_launch(dir.path(), 2) else {
        return;
    };
    let (first, last) = two_cpus();
    launch.cpu_affinity = vec![first, last];
    launch.vcpu_pins = vec![last, first];
    let mut shim = start_shim(dir.path(), &launch);
    let hv = wait_running(dir.path()).hypervisor_pid.unwrap();
    let lists = allowed_lists(hv);
    let on = |cpu: u32| lists.iter().filter(|(_, l)| *l == cpu.to_string()).count();
    if first != last {
        assert_eq!((on(first), on(last)), (1, 1), "{lists:?}");
        let both = glidex_vm_shim::util::cpu_list(&[first, last]);
        assert_eq!(
            lists.iter().filter(|(_, l)| *l == both).count(),
            lists.len() - 2
        );
    }
    let log = std::fs::read_to_string(dir.path().join("console.log")).unwrap();
    assert!(log.contains("pinned 2 vCPUs to CPUs"), "{log}");

    let mut c = ShimClient::connect(&dir.path().join("shim.sock")).unwrap();
    // Dedicated VMs are not rebound wholesale.
    assert!(matches!(
        c.set_affinity(&[first]),
        Err(glidex_vm_shim::client::ShimError::Remote { code, .. }) if code == "invalid_argument"
    ));
    c.kill().unwrap();
    wait_exited(dir.path());
    c.release().unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
}

#[test]
fn a_vcpu_pinning_mismatch_fails_the_launch() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut launch) = fake_qemu_launch(dir.path(), 2) else {
        return;
    };
    let (first, _) = two_cpus();
    launch.vcpu_pins = vec![first; 3];
    let mut shim = start_shim(dir.path(), &launch);
    let inst = wait_exited(dir.path());
    let exit = inst.exit.unwrap();
    assert_eq!(exit.cause, ExitCause::LaunchFailed);
    assert!(
        exit.message
            .as_deref()
            .unwrap_or("")
            .contains("QEMU has 2 vCPUs"),
        "{:?}",
        exit.message
    );
    assert!(!alive(inst.hypervisor_pid.unwrap()));
    ShimClient::connect(&dir.path().join("shim.sock"))
        .unwrap()
        .release()
        .unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
}

#[test]
fn an_unbindable_affinity_fails_the_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let Some(mut launch) = fake_qemu_launch(dir.path(), 1) else {
        return;
    };
    // Valid for a cpu_set_t, but no such CPU here: EINVAL before exec.
    launch.cpu_affinity = vec![1023];
    if glidex_vm_shim::util::own_affinity()
        .unwrap()
        .contains(&1023)
    {
        return;
    }
    let mut shim = start_shim(dir.path(), &launch);
    let inst = wait_exited(dir.path());
    let exit = inst.exit.unwrap();
    assert_eq!(exit.cause, ExitCause::LaunchFailed);
    assert!(
        exit.message
            .as_deref()
            .unwrap_or("")
            .contains("cannot start"),
        "{:?}",
        exit.message
    );
    assert!(inst.hypervisor_pid.is_none());
    ShimClient::connect(&dir.path().join("shim.sock"))
        .unwrap()
        .release()
        .unwrap();
    assert_eq!(wait_exit_status(&mut shim), 0);
}

/// A one-shot job (spec/kernel-boot.md §10.2): no API socket to wait for;
/// its exit is recorded as a VM's would be, and its output is in the log.
#[test]
fn a_job_records_its_exit() {
    let Some(ch) = cloud_hypervisor() else { return };
    for (args, cause) in [
        (vec!["--version"], ExitCause::CleanExit),
        (vec!["--no-such-option"], ExitCause::Crashed),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut argv = vec![ch.to_string_lossy().into_owned()];
        argv.extend(args.iter().map(|a| a.to_string()));
        let mut launch = launch_file(dir.path(), argv);
        launch.job = true;
        let mut shim = start_shim(dir.path(), &launch);
        let exit = wait_exited(dir.path()).exit.unwrap();
        assert_eq!(exit.cause, cause, "{args:?}: {:?}", exit.message);
        wait_for("shim.sock", Duration::from_secs(5), || {
            dir.path().join("shim.sock").exists()
        });
        ShimClient::connect(&dir.path().join("shim.sock"))
            .unwrap()
            .release()
            .unwrap();
        assert_eq!(wait_exit_status(&mut shim), 0);
        if cause == ExitCause::CleanExit {
            let log = std::fs::read_to_string(dir.path().join("console.log")).unwrap();
            assert!(log.contains("cloud-hypervisor v"), "{log}");
        }
    }
}
