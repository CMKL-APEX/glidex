//! The shim's main loop (spec/reconciliation.md §8.3).
//!
//! Launch: validate `launch.json`, write `instance.json`, start the console
//! proxy, spawn the hypervisor on a fresh PTY (dying with the shim, D8),
//! wait for its API socket, then bind `shim.sock` and tell systemd we are
//! ready. Run: serve `shim.sock`, carry out stops and kills, notice the
//! hypervisor's exit and record why. Release: stop the console and exit.

use crate::launch::{allowed_binaries, HypervisorKind, LaunchFile};
use crate::proto::{self, HelloResult, Op, Request, Response, MAX_LINE};
use crate::proxy::{LogWriter, Proxy};
use crate::state::{ExitCause, ExitInfo, InstanceFile, Phase, StopInfo, INSTANCE_VERSION};
use crate::util::{boot_id, now, proc_starttime};
use glidex_hv_client::{ch::ChClient, qmp::QmpClient, GuestState};
use nix::pty::{openpty, OpenptyResult};
use nix::sys::signal::{kill, signal, SigHandler, Signal};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
use nix::unistd::{setsid, Pid};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

/// Exit status: released normally.
pub const EXIT_OK: i32 = 0;
/// Internal error.
pub const EXIT_INTERNAL: i32 = 1;
/// Unusable `launch.json` or a binary not on the allowlist; nothing ran.
pub const EXIT_BAD_LAUNCH: i32 = 2;

/// The environment hypervisors run with, and nothing else (§8.1).
const HV_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// After QMP `quit` / CH `vmm.shutdown`, how long before SIGKILL.
const QUIT_GRACE: Duration = Duration::from_secs(2);

static TERM: AtomicBool = AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    TERM.store(true, Ordering::SeqCst);
}

pub struct Args {
    pub dir: PathBuf,
    pub vm: Option<String>,
}

pub fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut dir = None;
    let mut vm = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dir" => dir = args.next(),
            "--vm" => vm = args.next(),
            "--version" => return Err(format!("glidex-vm-shim {}", env!("CARGO_PKG_VERSION"))),
            other => return Err(format!("unknown argument {}\nusage: glidex-vm-shim --dir <vm runtime dir> [--vm <id>]", other)),
        }
    }
    Ok(Args { dir: PathBuf::from(dir.ok_or("--dir is required")?), vm })
}

/// Run the shim; returns its exit status.
pub fn run(args: Args) -> i32 {
    unsafe {
        let _ = signal(Signal::SIGTERM, SigHandler::Handler(on_term));
        let _ = signal(Signal::SIGINT, SigHandler::SigIgn);
        let _ = signal(Signal::SIGHUP, SigHandler::SigIgn);
        let _ = signal(Signal::SIGPIPE, SigHandler::SigIgn);
    }
    let launch_path = args.dir.join(crate::LAUNCH_FILE);
    let launch = match LaunchFile::read(&launch_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("glidex-vm-shim: {}", e);
            return EXIT_BAD_LAUNCH;
        }
    };
    let mut sup = match Supervisor::new(args.dir.clone(), launch) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("glidex-vm-shim: {}", e);
            return EXIT_INTERNAL;
        }
    };
    if let Err(e) = sup.launch.validate(args.vm.as_deref(), &allowed_binaries()) {
        eprintln!("glidex-vm-shim: {}", e);
        sup.record_exit(ExitCause::LaunchFailed, None, Some(e.to_string()));
        return EXIT_BAD_LAUNCH;
    }
    sup.run()
}

struct StopState {
    deadline: Instant,
    cause: ExitCause,
}

struct Supervisor {
    launch: LaunchFile,
    instance_path: PathBuf,
    instance: InstanceFile,
    proxy: Option<Proxy>,
    child: Option<Child>,
    stop: Option<StopState>,
    /// SIGKILL deadline after QMP `quit`.
    kill_at: Option<Instant>,
    /// Who asked for the kill, if anyone did.
    kill_cause: Option<ExitCause>,
    /// Release as soon as the hypervisor is gone (SIGTERM).
    auto_release: bool,
}

impl Supervisor {
    fn new(dir: PathBuf, launch: LaunchFile) -> std::io::Result<Self> {
        let pid = std::process::id();
        let instance = InstanceFile {
            version: INSTANCE_VERSION,
            vm_id: launch.vm_id.clone(),
            instance_id: launch.instance_id.clone(),
            boot_id: boot_id()?,
            shim_pid: pid,
            shim_starttime: proc_starttime(pid)?.unwrap_or(0),
            hypervisor_pid: None,
            hypervisor_starttime: None,
            phase: Phase::Launching,
            launched_at: now(),
            stop: None,
            exit: None,
        };
        let s = Self {
            instance_path: dir.join(crate::INSTANCE_FILE),
            launch,
            instance,
            proxy: None,
            child: None,
            stop: None,
            kill_at: None,
            kill_cause: None,
            auto_release: false,
        };
        s.save();
        Ok(s)
    }

    fn save(&self) {
        if let Err(e) = self.instance.write(&self.instance_path) {
            eprintln!("glidex-vm-shim: cannot write {}: {}", self.instance_path.display(), e);
        }
    }

    fn note(&self, text: String) {
        if let Some(p) = &self.proxy {
            p.note(text);
        }
    }

    fn record_exit(&mut self, cause: ExitCause, status: Option<std::process::ExitStatus>, message: Option<String>) {
        let code = status.and_then(|s| s.code());
        let sig = status.and_then(|s| s.signal());
        let detail = match (code, sig) {
            (_, Some(s)) => format!(" (signal {})", s),
            (Some(c), None) if c != 0 => format!(" (status {})", c),
            _ => String::new(),
        };
        self.note(format!(
            "\r\n--- glidex: instance {} exited: {}{} ---\r\n",
            self.instance.instance_id,
            cause.as_str(),
            detail
        ));
        self.instance.exit = Some(ExitInfo { at: now(), cause, code, signal: sig, message });
        self.instance.phase = Phase::Exited;
        self.save();
    }

    fn run(mut self) -> i32 {
        for path in [&self.launch.api_socket, &self.launch.shim_socket] {
            let _ = std::fs::remove_file(path);
        }
        let log = match LogWriter::open(Path::new(&self.launch.log_path), self.launch.log_max_bytes) {
            Ok(l) => l,
            Err(e) => {
                self.record_exit(ExitCause::LaunchFailed, None, Some(format!("console log: {}", e)));
                return EXIT_INTERNAL;
            }
        };
        match Proxy::start(Path::new(&self.launch.console_socket), log) {
            Ok(p) => self.proxy = Some(p),
            Err(e) => {
                self.record_exit(ExitCause::LaunchFailed, None, Some(format!("console socket: {}", e)));
                return EXIT_INTERNAL;
            }
        }
        self.note(format!(
            "\r\n--- glidex: instance {} started {} ---\r\n",
            self.instance.instance_id,
            rfc3339(now())
        ));

        self.launch_hypervisor();

        let listener = match bind_shim_socket(Path::new(&self.launch.shim_socket)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("glidex-vm-shim: shim socket: {}", e);
                if let Some(c) = self.child.as_mut() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
                self.release();
                return EXIT_INTERNAL;
            }
        };
        let (req_tx, req_rx) = channel();
        spawn_acceptor(listener, req_tx);
        crate::sd::notify_ready();
        self.serve(req_rx)
    }

    /// Spawn the hypervisor (and QEMU's fallback, once) and wait for its
    /// socket. Ends in `Running`, or `Exited` with `LaunchFailed`.
    fn launch_hypervisor(&mut self) {
        let mut argv = self.launch.argv.clone();
        let mut fallback = self.launch.fallback.clone();
        loop {
            match self.spawn(&argv) {
                Ok(child) => {
                    self.instance.hypervisor_pid = Some(child.id());
                    self.instance.hypervisor_starttime = proc_starttime(child.id()).ok().flatten();
                    self.child = Some(child);
                    self.save();
                }
                Err(e) => {
                    self.record_exit(ExitCause::LaunchFailed, None, Some(format!("cannot start {}: {}", argv[0], e)));
                    return;
                }
            }
            let deadline = Instant::now() + Duration::from_secs(self.launch.ready_timeout_secs.max(1));
            loop {
                if TERM.load(Ordering::SeqCst) {
                    self.hard_kill();
                    let status = self.child.as_mut().and_then(|c| c.wait().ok());
                    self.child = None;
                    self.record_exit(ExitCause::Terminated, status, None);
                    self.auto_release = true;
                    return;
                }
                if let Some(status) = self.child.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
                    self.child = None;
                    // Let the proxy read what it printed.
                    std::thread::sleep(Duration::from_millis(100));
                    let output = self.proxy.as_ref().map(|p| p.captured_stderr()).unwrap_or_default();
                    if let Some(fb) = fallback.take() {
                        let lower = output.to_ascii_lowercase();
                        if fb.when_output_matches.iter().any(|m| lower.contains(&m.to_ascii_lowercase())) {
                            self.note(format!(
                                "\r\n--- glidex: {} exited at launch; retrying with the fallback command line ---\r\n",
                                argv[0]
                            ));
                            argv = fb.argv;
                            break;
                        }
                    }
                    let message = format!("{} exited with {} before its API socket was ready.\n{}", argv[0], status, output.trim());
                    self.record_exit(ExitCause::LaunchFailed, Some(status), Some(message));
                    return;
                }
                if self.ready() {
                    self.instance.phase = Phase::Running;
                    self.save();
                    return;
                }
                if Instant::now() >= deadline {
                    self.hard_kill();
                    let status = self.child.as_mut().and_then(|c| c.wait().ok());
                    self.child = None;
                    let output = self.proxy.as_ref().map(|p| p.captured_stderr()).unwrap_or_default();
                    let message = format!(
                        "{} API socket not ready after {} s.\n{}",
                        argv[0],
                        self.launch.ready_timeout_secs,
                        output.trim()
                    );
                    self.record_exit(ExitCause::LaunchFailed, status, Some(message));
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// Whether the hypervisor's own socket answers.
    fn ready(&self) -> bool {
        if !Path::new(&self.launch.api_socket).exists() {
            return false;
        }
        match self.launch.hypervisor {
            HypervisorKind::CloudHypervisor => ChClient::new(&self.launch.api_socket).ping().is_ok(),
            HypervisorKind::Qemu => QmpClient::new(&self.launch.api_socket).ping().is_ok(),
        }
    }

    /// Spawn `argv` on a fresh raw PTY (stdin/stdout) with stderr on a pipe,
    /// both handed to the console proxy.
    fn spawn(&self, argv: &[String]) -> std::io::Result<Child> {
        let OpenptyResult { master, slave } = openpty(None, None).map_err(std::io::Error::from)?;
        // openpty's fds are inheritable: a hypervisor holding the master
        // itself would never see the hangup when the shim dies.
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC)).map_err(std::io::Error::from)?;
        }
        let mut termios = tcgetattr(&slave).map_err(std::io::Error::from)?;
        cfmakeraw(&mut termios);
        tcsetattr(&slave, SetArg::TCSANOW, &termios).map_err(std::io::Error::from)?;
        let (err_r, err_w) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).map_err(std::io::Error::from)?;

        let parent = std::process::id() as libc::pid_t;
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env_clear()
            .env("LC_ALL", "C")
            .env("PATH", HV_PATH)
            .stdin(Stdio::from(File::from(slave.try_clone()?)))
            .stdout(Stdio::from(File::from(slave)))
            .stderr(Stdio::from(File::from(err_w)));
        unsafe {
            cmd.pre_exec(move || {
                // D8: the hypervisor never outlives its shim, two ways.
                // The PTY becomes its controlling terminal, so when the shim
                // dies and the master closes, the kernel hangs the terminal
                // up and sends the hypervisor SIGHUP (raw mode: no other
                // terminal signals). That survives exec of a binary with
                // file capabilities (cloud-hypervisor's cap_net_admin),
                // which clears PR_SET_PDEATHSIG; the death signal still
                // covers everything else.
                // Ignored signals stay ignored across exec: undo the
                // shim's own SIG_IGNs, or the hangup would be lost.
                for sig in [libc::SIGHUP, libc::SIGINT, libc::SIGPIPE] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                setsid().ok();
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The shim may have died between fork and prctl.
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // `cmd` holds copies of the slave and the pipe's write end; drop
        // them so EOF arrives once the hypervisor exits.
        drop(cmd);
        if let Some(p) = &self.proxy {
            p.attach(File::from(master), File::from(err_r));
        }
        Ok(child)
    }

    fn hard_kill(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
        }
    }

    fn serve(mut self, requests: Receiver<(Request, Sender<Response>)>) -> i32 {
        if self.instance.phase == Phase::Exited && self.auto_release {
            self.release();
            return EXIT_OK;
        }
        loop {
            // Requests first, so a `stop` racing an exit is still answered.
            while let Ok((req, reply)) = requests.try_recv() {
                let (resp, release) = self.handle(req);
                let _ = reply.send(resp);
                if release {
                    // Let the connection thread send the reply first.
                    std::thread::sleep(Duration::from_millis(50));
                    self.release();
                    return EXIT_OK;
                }
            }

            if TERM.swap(false, Ordering::SeqCst) {
                if self.instance.phase == Phase::Exited {
                    self.release();
                    return EXIT_OK;
                }
                self.auto_release = true;
                self.begin_stop(self.launch.host_shutdown_grace_secs, ExitCause::Terminated);
            }

            if let Some(status) = self.child.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
                self.child = None;
                let cause = match (self.kill_cause, &self.stop) {
                    (Some(c), _) => c,
                    (None, Some(s)) => s.cause,
                    (None, None) if status.code() == Some(0) => ExitCause::CleanExit,
                    (None, None) => ExitCause::Crashed,
                };
                // Let the proxy drain the guest's last words first.
                std::thread::sleep(Duration::from_millis(50));
                self.record_exit(cause, Some(status), None);
                if self.auto_release {
                    self.release();
                    return EXIT_OK;
                }
            }

            if self.child.is_some() {
                let now = Instant::now();
                if self.stop.as_ref().is_some_and(|s| now >= s.deadline) && self.kill_at.is_none() {
                    let cause = self.stop.as_ref().map(|s| s.cause);
                    self.kill(cause.unwrap_or(ExitCause::Requested));
                }
                if self.kill_at.is_some_and(|t| now >= t) {
                    self.hard_kill();
                }
            }

            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Returns the response and whether to release afterwards.
    fn handle(&mut self, req: Request) -> (Response, bool) {
        let id = req.id;
        match req.op {
            Op::Hello { .. } => (Response::err(id, proto::E_PROTOCOL, "hello was already sent"), false),
            Op::Status => (Response::ok(id, serde_json::to_value(&self.instance).unwrap_or_default()), false),
            Op::Stop { grace_secs } => {
                self.begin_stop(grace_secs, ExitCause::Requested);
                (Response::ok(id, serde_json::Value::Null), false)
            }
            Op::Kill => {
                if self.child.is_some() {
                    self.kill(ExitCause::Requested);
                }
                (Response::ok(id, serde_json::Value::Null), false)
            }
            Op::Release => {
                if self.instance.phase == Phase::Exited {
                    (Response::ok(id, serde_json::Value::Null), true)
                } else {
                    (Response::err(id, proto::E_NOT_EXITED, "the hypervisor is still running; stop it first"), false)
                }
            }
        }
    }

    /// Resume if paused, press the power button, kill at the deadline.
    /// Idempotent; a later call can only shorten the deadline.
    fn begin_stop(&mut self, grace_secs: u64, cause: ExitCause) {
        if self.child.is_none() {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(grace_secs);
        let first = self.stop.is_none();
        match &mut self.stop {
            Some(s) => s.deadline = s.deadline.min(deadline),
            None => self.stop = Some(StopState { deadline, cause }),
        }
        let wall_deadline = now() + grace_secs;
        let info = match &self.instance.stop {
            Some(s) if s.deadline <= wall_deadline => s.clone(),
            _ => StopInfo { requested_at: now(), grace_secs, deadline: wall_deadline },
        };
        self.instance.stop = Some(info);
        self.save();
        if grace_secs == 0 {
            self.kill(cause);
            return;
        }
        if first && self.press_power_button().is_err() {
            // No ACPI to talk to: stop it hard.
            self.kill(cause);
        }
    }

    fn press_power_button(&self) -> Result<(), glidex_hv_client::HvError> {
        let sock = &self.launch.api_socket;
        match self.launch.hypervisor {
            HypervisorKind::CloudHypervisor => {
                let c = ChClient::new(sock);
                if c.observe().map(|o| o.guest == GuestState::Paused).unwrap_or(false) {
                    c.resume()?;
                }
                c.power_button()
            }
            HypervisorKind::Qemu => {
                let c = QmpClient::new(sock);
                if c.observe().map(|o| o.guest == GuestState::Paused).unwrap_or(false) {
                    c.cont()?;
                }
                c.system_powerdown()
            }
        }
    }

    /// Stop the hypervisor now, without the guest's cooperation, but let
    /// it close its disks: QEMU `quit`, CH `vmm.shutdown`; SIGKILL if it is
    /// still there after a moment (a bare SIGKILL can leave a qcow2 disk
    /// CH refuses to reopen).
    fn kill(&mut self, cause: ExitCause) {
        if self.kill_cause.is_none() {
            self.kill_cause = Some(cause);
        }
        if self.kill_at.is_some() {
            return;
        }
        let sock = self.launch.api_socket.clone();
        let _ = match self.launch.hypervisor {
            HypervisorKind::Qemu => QmpClient::new(&sock).quit(),
            HypervisorKind::CloudHypervisor => ChClient::new(&sock).shutdown_vmm(),
        };
        self.kill_at = Some(Instant::now() + QUIT_GRACE);
    }

    /// Stop the console (draining it into the log), remove the sockets.
    /// `instance.json` and the logs stay for the control plane.
    fn release(&mut self) {
        if let Some(c) = self.child.as_mut() {
            // Never leave a hypervisor without its shim.
            let _ = kill(Pid::from_raw(c.id() as i32), Signal::SIGKILL);
            let _ = c.wait();
        }
        if let Some(p) = self.proxy.take() {
            p.stop();
        }
        for path in [&self.launch.console_socket, &self.launch.api_socket, &self.launch.shim_socket] {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn bind_shim_socket(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// Accept connections on a thread; each connection gets its own thread
/// that answers `hello` itself and forwards everything else to the main
/// loop.
fn spawn_acceptor(listener: UnixListener, tx: Sender<(Request, Sender<Response>)>) {
    let me = nix::unistd::getuid();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            // §8.5: only our own user (the socket is 0600 anyway).
            let peer_ok = nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
                .map(|c| c.uid() == me.as_raw() || c.uid() == 0)
                .unwrap_or(false);
            if !peer_ok {
                continue;
            }
            let tx = tx.clone();
            std::thread::spawn(move || connection(stream, tx));
        }
    });
}

fn connection(stream: UnixStream, tx: Sender<(Request, Sender<Response>)>) {
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
    let Ok(mut writer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream).take(u64::MAX);
    let mut greeted = false;
    loop {
        let mut line = String::new();
        reader.set_limit(MAX_LINE as u64 + 1);
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE {
            let _ = send(&mut writer, &Response::err(0, proto::E_PROTOCOL, "line too long"));
            return;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = send(&mut writer, &Response::err(0, proto::E_PROTOCOL, e.to_string()));
                return;
            }
        };
        let resp = match (&req.op, greeted) {
            (Op::Hello { protocol }, false) => {
                if *protocol < proto::MIN_PROTOCOL_VERSION {
                    Response::err(req.id, proto::E_PROTOCOL, format!("protocol {} is too old", protocol))
                } else {
                    greeted = true;
                    let r = HelloResult {
                        protocol: (*protocol).min(proto::PROTOCOL_VERSION),
                        shim_version: env!("CARGO_PKG_VERSION").to_string(),
                    };
                    Response::ok(req.id, serde_json::to_value(r).unwrap_or_default())
                }
            }
            (_, false) => Response::err(req.id, proto::E_PROTOCOL, "send hello first"),
            (_, true) => {
                let (rtx, rrx) = channel();
                if tx.send((req.clone(), rtx)).is_err() {
                    return;
                }
                match rrx.recv_timeout(Duration::from_secs(30)) {
                    Ok(r) => r,
                    Err(_) => Response::err(req.id, proto::E_INTERNAL, "no answer from the shim"),
                }
            }
        };
        if send(&mut writer, &resp).is_err() {
            return;
        }
    }
}

fn send(w: &mut UnixStream, r: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(r).map_err(std::io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line)
}

/// `2026-10-03T12:34:56Z` from unix seconds (UTC), without a date crate.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_791_000_000), "2026-10-03T04:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn args() {
        let a = parse_args(["--vm", "x", "--dir", "/run/x"].map(String::from).into_iter()).unwrap();
        assert_eq!((a.dir, a.vm.as_deref()), (PathBuf::from("/run/x"), Some("x")));
        assert!(parse_args(std::iter::empty()).is_err());
        assert!(parse_args(["--bogus".to_string()].into_iter()).is_err());
    }
}
