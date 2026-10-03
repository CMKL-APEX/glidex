//! The console proxy (spec console.md, reconciliation.md §8.6), moved here
//! from the control plane.
//!
//! One OS thread owns the PTY master, the console listener and the log.
//! It is the log's only writer: the guest's output, the hypervisor's
//! stderr (read from a pipe, logged but never sent to console clients) and
//! the shim's separator lines all go through it, so rotation never leaves
//! a writer on the old file.
//!
//! Invariant: the listener outlives the PTY. When the hypervisor exits the
//! PTY is dropped but clients can still connect and replay the log, until
//! [`Proxy::stop`].

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How much of the hypervisor's stderr is kept for launch errors.
const CAPTURE_MAX: usize = 64 * 1024;

/// Output queued for one console client beyond its socket buffer. A client
/// that falls further behind than this (on top of the log replay) is
/// dropped; it can reconnect and replay the log.
const CLIENT_BACKLOG_MAX: usize = 4 << 20;

/// A console client. Writes never block the proxy: what the socket does
/// not take now waits in `pending` (the log replay too).
struct Client {
    stream: UnixStream,
    pending: std::collections::VecDeque<u8>,
    limit: usize,
}

impl Client {
    fn new(stream: UnixStream, replay: Vec<u8>) -> Self {
        let limit = replay.len() + CLIENT_BACKLOG_MAX;
        Client { stream, pending: replay.into(), limit }
    }

    /// Queue `bytes`; false once the client is gone or too far behind.
    fn send(&mut self, bytes: &[u8]) -> bool {
        self.pending.extend(bytes);
        self.pending.len() <= self.limit && self.flush()
    }

    /// Write what the socket takes now; false once the client is gone.
    fn flush(&mut self) -> bool {
        while !self.pending.is_empty() {
            let (front, _) = self.pending.as_slices();
            match (&self.stream).write(front) {
                Ok(0) => return false,
                Ok(n) => {
                    self.pending.drain(..n);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        // Once caught up, the replay allowance is spent.
        self.limit = CLIENT_BACKLOG_MAX;
        true
    }
}

/// The console log: appended to, rotated once (`<path>.1`) when the next
/// write would take it past `max` bytes.
pub struct LogWriter {
    path: PathBuf,
    file: File,
    size: u64,
    max: u64,
}

impl LogWriter {
    pub fn open(path: &Path, max: u64) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = OpenOptions::new().append(true).create(true).mode(0o600).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { path: path.to_path_buf(), file, size, max: max.max(1) })
    }

    pub fn rotated_path(path: &Path) -> PathBuf {
        let mut p = path.as_os_str().to_owned();
        p.push(".1");
        PathBuf::from(p)
    }

    /// Write one chunk whole; rotate first if it would not fit.
    pub fn write(&mut self, bytes: &[u8]) {
        if self.size > 0 && self.size + bytes.len() as u64 > self.max {
            if let Err(e) = self.rotate() {
                eprintln!("glidex-vm-shim: console log rotation failed: {}", e);
            }
        }
        if self.file.write_all(bytes).is_ok() {
            self.size += bytes.len() as u64;
        }
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::rename(&self.path, Self::rotated_path(&self.path))?;
        self.file = OpenOptions::new().append(true).create(true).mode(0o600).open(&self.path)?;
        self.size = 0;
        Ok(())
    }
}

pub enum ProxyCmd {
    /// A (new) hypervisor's PTY master and stderr pipe.
    Attach { master: File, stderr: File },
    /// A line from the shim itself, logged and shown to clients.
    Note(Vec<u8>),
}

pub struct Proxy {
    tx: Sender<ProxyCmd>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    /// The hypervisor's stderr since the last `Attach` (bounded).
    pub stderr_capture: Arc<Mutex<Vec<u8>>>,
}

impl Proxy {
    /// Bind `console_socket` and start serving it, logging to `log`.
    pub fn start(console_socket: &Path, log: LogWriter) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(console_socket);
        let listener = UnixListener::bind(console_socket)?;
        listener.set_nonblocking(true)?;
        let (tx, rx) = channel();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let capture = Arc::new(Mutex::new(Vec::new()));
        let (s, c) = (stop.clone(), capture.clone());
        let handle = thread::spawn(move || proxy_loop(listener, log, rx, s, c));
        Ok(Self { tx, stop, handle: Some(handle), stderr_capture: capture })
    }

    pub fn attach(&self, master: File, stderr: File) {
        self.stderr_capture.lock().unwrap().clear();
        let _ = self.tx.send(ProxyCmd::Attach { master, stderr });
    }

    pub fn note(&self, line: String) {
        let _ = self.tx.send(ProxyCmd::Note(line.into_bytes()));
    }

    pub fn captured_stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr_capture.lock().unwrap()).into_owned()
    }

    /// Drain what is still buffered into the log, then stop.
    pub fn stop(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn set_nonblocking(f: &File) {
    unsafe {
        let raw = f.as_raw_fd();
        let flags = libc::fcntl(raw, libc::F_GETFL);
        libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

/// Read what `f` has right now. `false` once it is gone (EOF / EIO).
fn drain(f: &File, buf: &mut [u8], mut sink: impl FnMut(&[u8])) -> bool {
    let mut reader = f;
    loop {
        match reader.read(buf) {
            Ok(0) => return false,
            Ok(n) => sink(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // EIO: the slave side is closed and the master drained.
            Err(_) => return false,
        }
    }
}

fn proxy_loop(
    listener: UnixListener,
    mut log: LogWriter,
    rx: Receiver<ProxyCmd>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    capture: Arc<Mutex<Vec<u8>>>,
) {
    let mut pty: Option<File> = None;
    let mut stderr: Option<File> = None;
    let mut clients: Vec<Client> = Vec::new();
    let mut buf = [0u8; 4096];

    let handle_cmds = |pty: &mut Option<File>, stderr: &mut Option<File>, log: &mut LogWriter, clients: &mut Vec<Client>| loop {
        match rx.try_recv() {
            Ok(ProxyCmd::Attach { master, stderr: err }) => {
                set_nonblocking(&master);
                set_nonblocking(&err);
                *pty = Some(master);
                *stderr = Some(err);
            }
            Ok(ProxyCmd::Note(bytes)) => {
                log.write(&bytes);
                clients.retain_mut(|c| c.send(&bytes));
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    };

    let pump = |pty: &mut Option<File>, stderr: &mut Option<File>, log: &mut LogWriter, clients: &mut Vec<Client>, buf: &mut [u8]| {
        if let Some(f) = pty.as_ref() {
            if !drain(f, buf, |b| {
                log.write(b);
                clients.retain_mut(|c| c.send(b));
            }) {
                *pty = None;
            }
        }
        if let Some(f) = stderr.as_ref() {
            if !drain(f, buf, |b| {
                log.write(b);
                let mut cap = capture.lock().unwrap();
                if cap.len() < CAPTURE_MAX {
                    let room = CAPTURE_MAX - cap.len();
                    cap.extend_from_slice(&b[..b.len().min(room)]);
                }
            }) {
                *stderr = None;
            }
        }
    };

    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
        handle_cmds(&mut pty, &mut stderr, &mut log, &mut clients);

        if let Ok((stream, _)) = listener.accept() {
            // Replay the current log, then live output (rotated logs are
            // reachable through the API). Queued, so a slow client never
            // stalls the console.
            let mut content = Vec::new();
            if let Ok(mut existing) = File::open(&log.path) {
                let _ = existing.read_to_end(&mut content);
            }
            stream.set_nonblocking(true).ok();
            let mut c = Client::new(stream, content);
            if c.flush() {
                clients.push(c);
            }
        }

        pump(&mut pty, &mut stderr, &mut log, &mut clients, &mut buf);
        clients.retain_mut(|c| c.flush());

        if let Some(file) = &pty {
            for client in &mut clients {
                if let Ok(n @ 1..) = (&client.stream).read(&mut buf) {
                    let mut writer = file;
                    let _ = writer.write_all(&buf[..n]);
                    let _ = writer.flush();
                }
            }
        }

        thread::sleep(Duration::from_millis(10));
    }

    // Stopping: keep whatever the guest printed last, and any notes.
    handle_cmds(&mut pty, &mut stderr, &mut log, &mut clients);
    pump(&mut pty, &mut stderr, &mut log, &mut clients, &mut buf);
}

#[cfg(test)]
mod client_tests {
    use super::*;

    #[test]
    fn a_slow_client_gets_everything_in_order() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        // More than a socket buffer holds.
        let replay = vec![b'r'; 1 << 20];
        let mut c = Client::new(a, replay.clone());
        assert!(c.flush());
        assert!(c.send(b"live"));
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut b = b;
            b.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            while got.len() < (1 << 20) + 4 {
                let mut buf = [0u8; 65536];
                let n = b.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            got
        });
        while !c.pending.is_empty() {
            assert!(c.flush());
            std::thread::sleep(Duration::from_millis(1));
        }
        let got = reader.join().unwrap();
        assert!(got.starts_with(&replay) && got.ends_with(b"live"));
    }

    #[test]
    fn a_client_too_far_behind_is_dropped() {
        let (a, _b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let mut c = Client::new(a, Vec::new());
        let chunk = vec![0u8; 1 << 20];
        let mut alive = true;
        for _ in 0..8 {
            alive = c.send(&chunk);
        }
        assert!(!alive);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::pty::{openpty, OpenptyResult};
    use nix::sys::termios::{cfmakeraw, tcgetattr, tcsetattr, SetArg};

    fn raw_pty() -> (File, File) {
        let OpenptyResult { master, slave } = openpty(None, None).unwrap();
        let mut t = tcgetattr(&slave).unwrap();
        cfmakeraw(&mut t);
        tcsetattr(&slave, SetArg::TCSANOW, &t).unwrap();
        (File::from(master), File::from(slave))
    }

    fn pipe() -> (File, File) {
        let (r, w) = nix::unistd::pipe().unwrap();
        (File::from(r), File::from(w))
    }

    #[test]
    fn rotation_keeps_every_byte_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("console.log");
        let mut log = LogWriter::open(&path, 100).unwrap();
        let chunks: Vec<Vec<u8>> = (0..30u8).map(|i| vec![b'a' + (i % 26); 9]).collect();
        for c in &chunks {
            log.write(c);
        }
        let old = std::fs::read(LogWriter::rotated_path(&path)).unwrap();
        let cur = std::fs::read(&path).unwrap();
        assert!(old.len() <= 100 && cur.len() <= 100, "{} {}", old.len(), cur.len());
        // The newest bytes are in the current file, whole chunks only.
        let all: Vec<u8> = chunks.concat();
        assert!(all.ends_with(&[old.clone(), cur.clone()].concat()));
        assert_eq!(cur.len() % 9, 0);
    }

    #[test]
    fn notes_output_and_stderr_reach_the_log_and_clients_get_replay() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("console.log");
        std::fs::write(&log_path, b"old instance\n").unwrap();
        let sock = dir.path().join("console.sock");
        let proxy = Proxy::start(&sock, LogWriter::open(&log_path, 1 << 20).unwrap()).unwrap();
        let (master, mut slave) = raw_pty();
        let (err_r, mut err_w) = pipe();
        proxy.note("--- glidex: instance x started ---\r\n".into());
        proxy.attach(master, err_r);
        slave.write_all(b"guest says hi\r\n").unwrap();
        err_w.write_all(b"qemu: warning\n").unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let mut client = UnixStream::connect(&sock).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut got = Vec::new();
        while !String::from_utf8_lossy(&got).contains("guest says hi") {
            let mut b = [0u8; 256];
            let n = client.read(&mut b).unwrap();
            got.extend_from_slice(&b[..n]);
        }
        // Replay includes the earlier instance and the stderr line.
        let text = String::from_utf8_lossy(&got).into_owned();
        assert!(text.starts_with("old instance\n--- glidex: instance x started"), "{text}");

        client.write_all(b"ls\r").unwrap();
        let mut typed = [0u8; 3];
        slave.read_exact(&mut typed).unwrap();
        assert_eq!(&typed, b"ls\r");

        // The guest goes away; the listener stays.
        drop(slave);
        drop(err_w);
        proxy.note("--- glidex: instance x exited: clean_exit ---\r\n".into());
        std::thread::sleep(Duration::from_millis(100));
        assert!(UnixStream::connect(&sock).is_ok());
        assert!(proxy.captured_stderr().contains("qemu: warning"));
        proxy.stop();
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(log.contains("qemu: warning") && log.ends_with("exited: clean_exit ---\r\n"), "{log}");
    }

    #[test]
    fn stopping_drains_what_is_still_buffered() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("console.log");
        let proxy = Proxy::start(&dir.path().join("c.sock"), LogWriter::open(&log_path, 1 << 20).unwrap()).unwrap();
        let (master, mut slave) = raw_pty();
        let (err_r, _err_w) = pipe();
        proxy.attach(master, err_r);
        std::thread::sleep(Duration::from_millis(50));
        slave.write_all(b"last words").unwrap();
        proxy.stop();
        assert!(std::fs::read_to_string(&log_path).unwrap().contains("last words"));
    }
}
