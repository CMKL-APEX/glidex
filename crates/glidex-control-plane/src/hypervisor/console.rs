//! Guest console plumbing shared by both backends.
//!
//! glidex owns the PTY: the hypervisor runs with the slave as its stdin
//! and stdout (QEMU `-serial stdio`, Cloud-Hypervisor console/serial mode
//! `Tty`), and the console proxy reads the master. That way nothing the
//! guest printed is lost when the hypervisor exits: output written to a
//! slave stays readable on the master after the slave closes, whereas a
//! PTY the hypervisor owned (Cloud-Hypervisor's `Pty` mode) discards what
//! the reader hadn't read yet the moment its master closes — typically
//! the kernel's last line, "reboot: Power down".
use super::HypervisorError;
use nix::pty::{openpty, OpenptyResult};
use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};
use nix::unistd::setsid;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// A VM's captured console log as text. The log is the guest's raw byte
/// stream, which needn't be valid UTF-8, so decode lossily.
pub(crate) fn read_log(path: &str) -> String {
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// Spawn `cmd` with a fresh raw-mode PTY slave as its stdin and stdout and
/// `stderr` as its stderr. Returns the child and the PTY master.
///
/// The child gets its own session (`setsid`) so terminal signals never
/// reach the control plane. `openpty` opens the slave `O_NOCTTY`, so the
/// PTY doesn't become the control plane's controlling terminal either
/// (which, under systemd, would let the hangup at VM exit kill it).
pub(crate) fn spawn_on_pty(cmd: &mut Command, stderr: File) -> Result<(Child, File), HypervisorError> {
    let OpenptyResult { master, slave } = openpty(None, None)
        .map_err(|e| HypervisorError::SocketConnection(format!("Failed to create PTY: {}", e)))?;
    // Raw: the guest's serial bytes pass through untouched (no echo, no
    // CR/NL translation), whatever the hypervisor does with its stdio.
    let mut termios = tcgetattr(&slave)
        .map_err(|e| HypervisorError::SocketConnection(format!("Failed to read PTY mode: {}", e)))?;
    cfmakeraw(&mut termios);
    tcsetattr(&slave, SetArg::TCSANOW, &termios)
        .map_err(|e| HypervisorError::SocketConnection(format!("Failed to set PTY mode: {}", e)))?;

    let stdin = File::from(slave.try_clone()?);
    let stdout = File::from(slave);
    let child = unsafe {
        cmd.stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .pre_exec(|| {
                setsid().ok();
                Ok(())
            })
            .spawn()?
    };
    // `cmd` still holds the slave copies handed to the child; drop them so
    // the master sees EOF once the hypervisor exits.
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    Ok((child, File::from(master)))
}

/// Serve the console: bind `console_socket_path`, then until `running`
/// is cleared copy the PTY master to `log_file` and every connected
/// client (each new client first gets the log so far), and clients'
/// input back to the PTY.
///
/// The listener outlives the guest — clients can still connect and replay
/// the log to see why it died. Once `running` is cleared the loop drains
/// what is still buffered on the master before returning.
pub(crate) fn start_console_proxy(
    master: File,
    console_socket_path: &str,
    log_path: &str,
    log_file: File,
    running: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, HypervisorError> {
    let _ = std::fs::remove_file(console_socket_path);
    let listener = UnixListener::bind(console_socket_path).map_err(|e| {
        HypervisorError::SocketConnection(format!("Failed to create console socket: {}", e))
    })?;
    listener.set_nonblocking(true).map_err(|e| {
        HypervisorError::SocketConnection(format!("Failed to set non-blocking: {}", e))
    })?;
    let log_path = log_path.to_string();
    Ok(thread::spawn(move || {
        proxy_loop(master, listener, log_file, &log_path, running)
    }))
}

fn proxy_loop(master: File, listener: UnixListener, mut log_file: File, log_path: &str, running: Arc<AtomicBool>) {
    unsafe {
        let raw = master.as_raw_fd();
        let flags = libc::fcntl(raw, libc::F_GETFL);
        libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    // `None` once the hypervisor has gone (EOF / EIO): the PTY is closed
    // so a stopped VM doesn't hold it open.
    let mut pty = Some(master);
    let mut clients: Vec<UnixStream> = Vec::new();
    let mut buf = [0u8; 4096];

    // Copy what the master has right now; false once it's gone.
    let mut pump = |pty: &File, clients: &mut Vec<UnixStream>, buf: &mut [u8]| -> bool {
        let mut reader = pty;
        loop {
            match reader.read(buf) {
                Ok(0) => return false,
                Ok(n) => {
                    let _ = log_file.write_all(&buf[..n]);
                    let _ = log_file.flush();
                    clients.retain_mut(|c| c.write_all(&buf[..n]).is_ok());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                // EIO: the slave side is closed and the master drained.
                Err(_) => return false,
            }
        }
    };

    while running.load(Ordering::SeqCst) {
        if let Ok((stream, _)) = listener.accept() {
            stream.set_nonblocking(true).ok();
            if let Ok(mut existing) = File::open(log_path) {
                let mut content = Vec::new();
                if existing.read_to_end(&mut content).is_ok() && !content.is_empty() {
                    let mut s = &stream;
                    let _ = s.write_all(&content);
                }
            }
            clients.push(stream);
        }

        if let Some(file) = &pty {
            if !pump(file, &mut clients, &mut buf) {
                pty = None;
            }
        }

        if let Some(file) = &pty {
            for client in &mut clients {
                if let Ok(n @ 1..) = client.read(&mut buf) {
                    let mut writer = file;
                    let _ = writer.write_all(&buf[..n]);
                    let _ = writer.flush();
                }
            }
        }

        thread::sleep(Duration::from_millis(10));
    }

    // Stopping: keep whatever the guest printed last.
    if let Some(file) = &pty {
        pump(file, &mut clients, &mut buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    fn proxy(dir: &std::path::Path, master: File) -> (String, String, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let log = dir.join("vm.log").to_string_lossy().into_owned();
        let sock = dir.join("console.sock").to_string_lossy().into_owned();
        File::create(&log).unwrap();
        let log_file = OpenOptions::new().append(true).open(&log).unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let handle = start_console_proxy(master, &sock, &log, log_file, running.clone()).unwrap();
        (log, sock, running, handle)
    }

    #[test]
    fn last_line_survives_the_hypervisor_exiting() {
        let dir = tempfile::tempdir().unwrap();
        // The hypervisor prints its last line and exits at once.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'booting\\nreboot: Power down\\n'"]);
        let stderr = File::create(dir.path().join("stderr")).unwrap();
        let (mut child, master) = spawn_on_pty(&mut cmd, stderr).unwrap();
        child.wait().unwrap();
        // Reading only starts after the exit, as a slow proxy tick would.
        std::thread::sleep(Duration::from_millis(50));
        let (log, _sock, running, handle) = proxy(dir.path(), master);
        std::thread::sleep(Duration::from_millis(100));
        running.store(false, Ordering::SeqCst);
        handle.join().unwrap();
        // Raw mode: no "\r\n" translation either.
        assert_eq!(std::fs::read_to_string(log).unwrap(), "booting\nreboot: Power down\n");
    }

    #[test]
    fn logs_with_invalid_utf8_still_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm.log");
        std::fs::write(&path, b"OVMF \xff\xfe garbage\r\nreboot: Power down\r\n").unwrap();
        assert!(read_log(path.to_str().unwrap()).contains("reboot: Power down"));
        assert_eq!(read_log("/nonexistent/vm.log"), "");
    }

    #[test]
    fn stopping_drains_what_is_still_buffered() {
        let dir = tempfile::tempdir().unwrap();
        let OpenptyResult { master, slave } = openpty(None, None).unwrap();
        let (log, _sock, running, handle) = proxy(dir.path(), File::from(master));
        // Written just as the VM is stopped: the flag is already cleared
        // when the proxy next wakes up.
        let mut slave = File::from(slave);
        running.store(false, Ordering::SeqCst);
        slave.write_all(b"last words").unwrap();
        handle.join().unwrap();
        assert!(std::fs::read_to_string(log).unwrap().contains("last words"));
    }

    #[test]
    fn clients_get_the_log_then_live_output_and_can_type() {
        let dir = tempfile::tempdir().unwrap();
        let OpenptyResult { master, slave } = openpty(None, None).unwrap();
        let mut termios = tcgetattr(&slave).unwrap();
        cfmakeraw(&mut termios);
        tcsetattr(&slave, SetArg::TCSANOW, &termios).unwrap();
        let mut slave = File::from(slave);
        let (_log, sock, running, handle) = proxy(dir.path(), File::from(master));
        slave.write_all(b"early ").unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let mut client = UnixStream::connect(&sock).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        slave.write_all(b"live").unwrap();
        let mut got = Vec::new();
        while !String::from_utf8_lossy(&got).contains("live") {
            let mut b = [0u8; 64];
            let n = client.read(&mut b).unwrap();
            got.extend_from_slice(&b[..n]);
        }
        assert_eq!(String::from_utf8_lossy(&got), "early live");

        client.write_all(b"ls\r").unwrap();
        let mut typed = [0u8; 3];
        slave.read_exact(&mut typed).unwrap();
        assert_eq!(&typed, b"ls\r");

        running.store(false, Ordering::SeqCst);
        handle.join().unwrap();
    }

    #[test]
    fn pty_never_becomes_the_control_planes_terminal() {
        use nix::sys::wait::{waitpid, WaitStatus};
        use nix::unistd::{fork, ForkResult};
        match unsafe { fork() }.unwrap() {
            ForkResult::Child => {
                // Like the control plane under systemd: a session leader
                // with no controlling terminal.
                let code = (|| {
                    setsid().ok()?;
                    let mut cmd = Command::new("sleep");
                    cmd.arg("0.3");
                    let (mut child, master) = spawn_on_pty(&mut cmd, File::open("/dev/null").ok()?).ok()?;
                    if File::open("/dev/tty").is_ok() {
                        return Some(2); // the PTY became our terminal
                    }
                    child.wait().ok()?; // the hangup must not reach us
                    drop(master);
                    Some(0)
                })()
                .unwrap_or(1);
                unsafe { libc::_exit(code) }
            }
            ForkResult::Parent { child } => {
                assert_eq!(waitpid(child, None).unwrap(), WaitStatus::Exited(child, 0));
            }
        }
    }
}
