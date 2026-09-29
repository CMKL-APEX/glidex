use super::{Hypervisor, HypervisorError, HypervisorProcess, HypervisorType};
use crate::models::VmConfig;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::setsid;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Client for communicating with QEMU over the QEMU Machine Protocol (QMP)
/// on a Unix socket. Each command opens a fresh connection, performs the
/// capabilities handshake, sends the command, and waits for the reply.
pub struct QmpClient {
    socket_path: String,
}

impl QmpClient {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
        }
    }

    /// Open a QMP connection and complete the qmp_capabilities handshake.
    fn connect(&self) -> Result<(UnixStream, BufReader<UnixStream>), HypervisorError> {
        let stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| HypervisorError::SocketConnection(e.to_string()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(HypervisorError::ProcessStart)?;

        let reader_stream = stream
            .try_clone()
            .map_err(HypervisorError::ProcessStart)?;
        let mut reader = BufReader::new(reader_stream);

        // QEMU sends a greeting line on connect.
        let mut greeting = String::new();
        reader
            .read_line(&mut greeting)
            .map_err(HypervisorError::ProcessStart)?;

        let mut writer = stream.try_clone().map_err(HypervisorError::ProcessStart)?;
        writer
            .write_all(b"{\"execute\":\"qmp_capabilities\"}\r\n")
            .map_err(HypervisorError::ProcessStart)?;
        writer.flush().map_err(HypervisorError::ProcessStart)?;

        // Drain lines until the capabilities reply arrives.
        loop {
            let mut line = String::new();
            let n = reader
                .read_line(&mut line)
                .map_err(HypervisorError::ProcessStart)?;
            if n == 0 {
                return Err(HypervisorError::ApiRequest(
                    "QMP connection closed during handshake".to_string(),
                ));
            }
            if line.contains("\"return\"") {
                break;
            }
            if line.contains("\"error\"") {
                return Err(HypervisorError::ApiRequest(format!(
                    "QMP handshake failed: {}",
                    line
                )));
            }
        }

        Ok((stream, reader))
    }

    fn execute(&self, command: &str) -> Result<(), HypervisorError> {
        let (stream, mut reader) = self.connect()?;
        let mut writer = stream.try_clone().map_err(HypervisorError::ProcessStart)?;

        writer
            .write_all(command.as_bytes())
            .map_err(HypervisorError::ProcessStart)?;
        writer
            .write_all(b"\r\n")
            .map_err(HypervisorError::ProcessStart)?;
        writer.flush().map_err(HypervisorError::ProcessStart)?;

        loop {
            let mut line = String::new();
            let n = reader
                .read_line(&mut line)
                .map_err(HypervisorError::ProcessStart)?;
            if n == 0 {
                return Err(HypervisorError::ApiRequest(
                    "QMP connection closed before reply".to_string(),
                ));
            }
            if line.contains("\"error\"") {
                return Err(HypervisorError::ApiRequest(line.trim().to_string()));
            }
            if line.contains("\"return\"") {
                return Ok(());
            }
            // Otherwise it's an asynchronous event — keep reading for the reply.
        }
    }

    pub fn cont(&self) -> Result<(), HypervisorError> {
        self.execute(r#"{"execute":"cont"}"#)
    }

    pub fn stop(&self) -> Result<(), HypervisorError> {
        self.execute(r#"{"execute":"stop"}"#)
    }

    pub fn quit(&self) -> Result<(), HypervisorError> {
        self.execute(r#"{"execute":"quit"}"#)
    }

    pub fn add_vfio_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        let bdf = vfio_bdf(device_path);
        let id = vfio_device_id(device_path);
        let cmd = format!(
            r#"{{"execute":"device_add","arguments":{{"driver":"vfio-pci","host":"{}","id":"{}"}}}}"#,
            bdf, id
        );
        self.execute(&cmd)
    }

    pub fn remove_vfio_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        let id = vfio_device_id(device_path);
        let cmd = format!(
            r#"{{"execute":"device_del","arguments":{{"id":"{}"}}}}"#,
            id
        );
        self.execute(&cmd)
    }
}

/// Try to open the QMP socket and read the greeting line. Returns true if
/// QEMU responded, false if the socket exists but is dead / not yet ready.
fn probe_qmp(socket_path: &str) -> bool {
    let Ok(stream) = UnixStream::connect(socket_path) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .is_err()
    {
        return false;
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    matches!(reader.read_line(&mut line), Ok(n) if n > 0 && line.contains("QMP"))
}

/// Extract the BDF (e.g. "0000:41:00.0") from a sysfs device path.
fn vfio_bdf(path: &str) -> String {
    path.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Derive a deterministic QEMU device id from a sysfs path.
/// e.g. "/sys/bus/pci/devices/0000:41:00.0" -> "_vfio_0000_41_00_0"
fn vfio_device_id(path: &str) -> String {
    let bdf = vfio_bdf(path);
    format!("_vfio_{}", bdf.replace(':', "_").replace('.', "_"))
}

/// QEMU VM instance implementing HypervisorProcess.
///
/// Unlike Cloud-Hypervisor, QEMU accepts all VM configuration at
/// launch time rather than via runtime API calls. We therefore defer the
/// actual `qemu-system-x86_64` spawn until `configure()` is invoked, and use
/// `-S` to hold the guest in a stopped state until `start()` issues `cont`.
pub struct QemuInstance {
    socket_path: String,
    console_socket_path: String,
    log_path: String,
    child: Mutex<Option<Child>>,
    console_thread: Mutex<Option<thread::JoinHandle<()>>>,
    running: Arc<AtomicBool>,
    client: QmpClient,
}

impl QemuInstance {
    pub fn new(socket_path: &str, console_socket_path: &str, log_path: &str) -> Self {
        let client = QmpClient::new(socket_path);
        Self {
            socket_path: socket_path.to_string(),
            console_socket_path: console_socket_path.to_string(),
            log_path: log_path.to_string(),
            child: Mutex::new(None),
            console_thread: Mutex::new(None),
            running: Arc::new(AtomicBool::new(true)),
            client,
        }
    }

    fn launch(&self, config: &VmConfig) -> Result<(), HypervisorError> {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.console_socket_path);

        let log_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.log_path)?;

        let OpenptyResult { master, slave } = openpty(None, None).map_err(|e| {
            HypervisorError::SocketConnection(format!("Failed to create PTY: {}", e))
        })?;

        let slave_raw = slave.as_raw_fd();
        let stdin_fd = unsafe { File::from_raw_fd(libc::dup(slave_raw)) };
        let stdout_fd = unsafe { File::from_raw_fd(libc::dup(slave_raw)) };
        let stderr_fd = unsafe { File::from_raw_fd(libc::dup(slave_raw)) };

        // Note: we deliberately avoid `-nographic` (which forces
        // `-serial mon:stdio` and conflicts with our explicit serial), and
        // avoid `-cpu host` (which fails on hosts where the feature set
        // isn't expressible). `server,nowait` is accepted by both old and
        // new QEMU, unlike `server=on,wait=off`.
        let mut cmd = Command::new("qemu-system-x86_64");
        cmd.arg("-enable-kvm")
            .arg("-no-reboot")
            .arg("-machine")
            .arg("q35")
            .arg("-m")
            .arg(format!("{}M", config.mem_size_mib))
            .arg("-smp")
            .arg(format!("{}", config.vcpu_count))
            .arg("-kernel")
            .arg(&config.kernel_image_path)
            .arg("-append")
            .arg(&config.kernel_args)
            .arg("-drive")
            .arg(format!(
                "file={},if=virtio,format=raw",
                config.rootfs_path
            ))
            .arg("-qmp")
            .arg(format!("unix:{},server,nowait", self.socket_path))
            .arg("-serial")
            .arg("stdio")
            .arg("-display")
            .arg("none")
            .arg("-S");

        for device in &config.vfio_devices {
            let bdf = vfio_bdf(device);
            let id = vfio_device_id(device);
            cmd.arg("-device")
                .arg(format!("vfio-pci,host={},id={}", bdf, id));
        }

        let child = unsafe {
            cmd.stdin(Stdio::from(stdin_fd))
                .stdout(Stdio::from(stdout_fd))
                .stderr(Stdio::from(stderr_fd))
                .pre_exec(|| {
                    setsid().ok();
                    Ok(())
                })
                .spawn()?
        };

        drop(slave);

        let console_listener = UnixListener::bind(&self.console_socket_path).map_err(|e| {
            HypervisorError::SocketConnection(format!("Failed to create console socket: {}", e))
        })?;
        console_listener.set_nonblocking(true).map_err(|e| {
            HypervisorError::SocketConnection(format!("Failed to set non-blocking: {}", e))
        })?;

        let running = self.running.clone();
        let log_path_clone = self.log_path.clone();

        let console_thread = thread::spawn(move || {
            Self::console_proxy_loop(
                master,
                console_listener,
                log_file,
                &log_path_clone,
                running,
            );
        });

        *self.child.lock().unwrap() = Some(child);
        *self.console_thread.lock().unwrap() = Some(console_thread);

        // Wait for the QMP socket to become usable. The file existing is
        // not sufficient: if QEMU crashes it leaves an orphaned socket
        // that accepts but immediately resets. Probe the greeting to
        // confirm the process is alive and listening.
        for _ in 0..50 {
            if let Some(exit_status) = self.child_exit_status() {
                let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
                self.cleanup_partial();
                return Err(HypervisorError::ProcessStart(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!(
                        "qemu-system-x86_64 exited with {} before QMP was ready.\n--- qemu output ---\n{}",
                        exit_status,
                        log.trim()
                    ),
                )));
            }

            if std::path::Path::new(&self.socket_path).exists() && probe_qmp(&self.socket_path) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        self.cleanup_partial();
        Err(HypervisorError::Timeout(format!(
            "QMP socket not ready after timeout.\n--- qemu output ---\n{}",
            log.trim()
        )))
    }

    /// Return the child's exit status if it has already terminated.
    fn child_exit_status(&self) -> Option<std::process::ExitStatus> {
        let mut guard = self.child.lock().unwrap();
        match guard.as_mut()?.try_wait() {
            Ok(Some(status)) => Some(status),
            _ => None,
        }
    }

    /// Kill the child and join the console thread. Used on failed launches.
    fn cleanup_partial(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.console_thread.lock().unwrap().take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.console_socket_path);
    }

    fn console_proxy_loop(
        master: OwnedFd,
        listener: UnixListener,
        mut log_file: File,
        log_path: &str,
        running: Arc<AtomicBool>,
    ) {
        let master_raw = master.as_raw_fd();
        let mut clients: Vec<UnixStream> = Vec::new();
        let mut buf = [0u8; 4096];
        // PTY reads EOF once QEMU exits. We don't tear down the listener in
        // that case — clients should still be able to connect and read the
        // captured log to see *why* the guest died.
        let mut pty_alive = true;

        unsafe {
            let flags = libc::fcntl(master_raw, libc::F_GETFL);
            libc::fcntl(master_raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        while running.load(Ordering::SeqCst) {
            if let Ok((stream, _)) = listener.accept() {
                stream.set_nonblocking(true).ok();
                if let Ok(mut existing_log) = File::open(log_path) {
                    let mut log_content = Vec::new();
                    if existing_log.read_to_end(&mut log_content).is_ok()
                        && !log_content.is_empty()
                    {
                        let mut s = stream.try_clone().unwrap();
                        let _ = s.write_all(&log_content);
                    }
                }
                clients.push(stream);
            }

            if pty_alive {
                let master_file = unsafe { File::from_raw_fd(libc::dup(master_raw)) };
                let mut master_reader = master_file;
                match master_reader.read(&mut buf) {
                    Ok(0) => pty_alive = false,
                    Ok(n) => {
                        let data = &buf[..n];
                        let _ = log_file.write_all(data);
                        let _ = log_file.flush();
                        clients.retain_mut(|client| client.write_all(data).is_ok());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => pty_alive = false,
                }

                for client in &mut clients {
                    match client.read(&mut buf) {
                        Ok(0) => {}
                        Ok(n) => {
                            let mut master_writer =
                                unsafe { File::from_raw_fd(libc::dup(master_raw)) };
                            let _ = master_writer.write_all(&buf[..n]);
                            let _ = master_writer.flush();
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => {}
                    }
                }
            }

            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl HypervisorProcess for QemuInstance {
    fn configure(&self, config: &VmConfig) -> Result<(), HypervisorError> {
        self.launch(config)
    }

    fn start(&self) -> Result<(), HypervisorError> {
        self.client.cont()
    }

    fn pause(&self) -> Result<(), HypervisorError> {
        self.client.stop()
    }

    fn resume(&self) -> Result<(), HypervisorError> {
        self.client.cont()
    }

    fn kill(&self) -> Result<(), HypervisorError> {
        self.running.store(false, Ordering::SeqCst);

        let _ = self.client.quit();

        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        if let Some(handle) = self.console_thread.lock().unwrap().take() {
            let _ = handle.join();
        }

        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.console_socket_path);
        Ok(())
    }

    fn add_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        self.client.add_vfio_device(device_path)
    }

    fn remove_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        self.client.remove_vfio_device(device_path)
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn socket_path(&self) -> &str {
        &self.socket_path
    }

    fn console_socket_path(&self) -> &str {
        &self.console_socket_path
    }

    fn log_path(&self) -> &str {
        &self.log_path
    }
}

/// QEMU backend factory.
pub struct QemuBackend;

impl Hypervisor for QemuBackend {
    fn spawn(
        &self,
        socket_path: &str,
        console_socket_path: &str,
        log_path: &str,
    ) -> Result<Box<dyn HypervisorProcess>, HypervisorError> {
        // The actual qemu-system process is launched in `configure()` once
        // the VM config is known. Here we only allocate the handle.
        Ok(Box::new(QemuInstance::new(
            socket_path,
            console_socket_path,
            log_path,
        )))
    }

    fn hypervisor_type(&self) -> HypervisorType {
        HypervisorType::Qemu
    }

    fn is_available(&self) -> bool {
        Command::new("qemu-system-x86_64")
            .arg("--version")
            .output()
            .is_ok()
    }
}
