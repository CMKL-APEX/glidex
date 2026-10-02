use super::{
    needs_shared_memory, vfio_bdf, vfio_device_id, vm_disks, Hypervisor, HypervisorError,
    HypervisorProcess, HypervisorType, VmDisk,
};
use crate::images::qemu_img::ImageType;
use crate::models::{NicBinding, VmConfig};
use glidex_ovs::vm_port::VmPortBinding;
use nix::pty::{openpty, OpenptyResult};
use nix::unistd::setsid;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// OVMF code images glidex looks for, in order: Debian/Ubuntu (`ovmf`),
/// Fedora/RHEL (`edk2-ovmf`), Arch (`edk2-ovmf`).
const OVMF_CODE_CANDIDATES: &[&str] = &[
    "/usr/share/OVMF/OVMF_CODE_4M.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
    "/usr/share/edk2/ovmf/OVMF_CODE.fd",
    "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
];

/// The host's OVMF code image: the first candidate that exists, else the
/// first candidate (so the error names the path to install).
pub fn default_firmware_path() -> Option<PathBuf> {
    OVMF_CODE_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .or_else(|| OVMF_CODE_CANDIDATES.first().map(PathBuf::from))
}

/// The pristine variable store shipped next to an OVMF code image
/// (`OVMF_CODE_4M.fd` -> `OVMF_VARS_4M.fd`), if there is one.
pub fn ovmf_vars_template(code: &Path) -> Option<PathBuf> {
    let name = code.file_name()?.to_str()?;
    let vars = if name.contains("CODE") {
        name.replace("CODE", "VARS")
    } else if name.contains("code") {
        name.replace("code", "vars")
    } else {
        return None;
    };
    let path = code.with_file_name(vars);
    path.exists().then_some(path)
}

/// Lowest QEMU glidex drives: `server=on` sockets, `memory-backend`.
const MIN_QEMU_VERSION: (u32, u32) = (6, 0);

/// `(major, minor)` from `qemu-system-x86_64 --version`.
fn parse_qemu_version(output: &str) -> Option<(u32, u32)> {
    let rest = output.split("version ").nth(1)?;
    let mut parts = rest.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn qemu_version() -> Option<(u32, u32)> {
    static VERSION: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *VERSION.get_or_init(|| {
        let out = Command::new("qemu-system-x86_64")
            .arg("--version")
            .output()
            .ok()?;
        parse_qemu_version(&String::from_utf8_lossy(&out.stdout))
    })
}

/// In-kernel virtio-net datapath, if we may open `/dev/vhost-net`
/// (usually the `kvm` group).
fn vhost_net_usable() -> bool {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/vhost-net")
        .is_ok()
}

/// Whether the filesystem holding `path` supports `O_DIRECT` (tmpfs and
/// some FUSE filesystems don't).
fn supports_direct_io(path: &str) -> bool {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .is_ok()
}

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

    pub fn system_powerdown(&self) -> Result<(), HypervisorError> {
        self.execute(r#"{"execute":"system_powerdown"}"#)
    }

    pub fn add_vfio_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        let cmd = serde_json::json!({
            "execute": "device_add",
            "arguments": {
                "driver": "vfio-pci",
                "host": vfio_bdf(device_path),
                "id": vfio_device_id(device_path),
            }
        });
        self.execute(&cmd.to_string())
    }

    pub fn remove_vfio_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        let cmd = serde_json::json!({
            "execute": "device_del",
            "arguments": { "id": vfio_device_id(device_path) }
        });
        self.execute(&cmd.to_string())
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

/// Double the commas in a value for QEMU's `key=value,…` option parser.
fn qopt(value: &str) -> String {
    value.replace(',', ",,")
}

/// How the guest's UEFI firmware is mapped.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Firmware {
    /// Read-only code plus this VM's writable copy of the variable store.
    /// `secure_boot` builds need SMM and a secure flash.
    Pflash {
        code: String,
        vars: String,
        secure_boot: bool,
    },
    /// A combined image (`OVMF.fd`) with no separate variable store.
    Bios(String),
}

/// One `-blockdev` / `virtio-blk-pci` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QemuDisk {
    disk: VmDisk,
    /// `cache.direct=on,aio=native`: bypass the host page cache.
    direct: bool,
}

impl QemuDisk {
    /// The `-blockdev` JSON. JSON rather than `key=value` so a path never
    /// needs escaping.
    fn blockdev(&self, node: &str) -> String {
        let read_only = self.disk.read_only;
        let mut file = serde_json::json!({
            "driver": "file",
            "filename": self.disk.path,
            "read-only": read_only,
        });
        if self.direct {
            file["cache"] = serde_json::json!({"direct": true});
            file["aio"] = "native".into();
        }
        // A disk we couldn't probe is opened raw; never left to QEMU's own
        // probing, which would let a guest that writes a qcow2 header into
        // a raw disk change how it is opened next boot.
        let format = self.disk.format.unwrap_or(ImageType::Raw);
        let mut fmt = serde_json::json!({
            "driver": format.qemu_format(),
            "node-name": node,
            "read-only": read_only,
            "file": file,
        });
        if self.direct {
            fmt["cache"] = serde_json::json!({"direct": true});
        }
        // QEMU follows a qcow2's backing file on its own unless told not to.
        if format == ImageType::Qcow2 && !self.disk.backing_files {
            fmt["backing"] = serde_json::Value::Null;
        }
        fmt.to_string()
    }
}

/// Everything the `qemu-system-x86_64` command line depends on, resolved
/// against the host by `QemuInstance::launch`. `args` itself is pure.
#[derive(Debug, Clone)]
struct LaunchSpec<'a> {
    config: &'a VmConfig,
    qmp_socket: String,
    firmware: Option<Firmware>,
    disks: Vec<QemuDisk>,
    /// `-cpu host`; dropped on a retry if the host CPU can't be expressed.
    cpu_host: bool,
    /// Tap NICs use the in-kernel vhost-net datapath.
    vhost_net: bool,
}

impl LaunchSpec<'_> {
    fn args(&self) -> Vec<String> {
        let config = self.config;
        // No default NIC, VGA, floppy or CD-ROM: the guest gets exactly the
        // devices below, as under Cloud-Hypervisor.
        let mut args: Vec<String> =
            ["-nodefaults", "-no-user-config", "-enable-kvm"].map(String::from).to_vec();
        let push = |args: &mut Vec<String>, a: &str, b: String| {
            args.push(a.to_string());
            args.push(b);
        };

        let shared = needs_shared_memory(config) || config.hugepages;
        let secure_boot = matches!(self.firmware, Some(Firmware::Pflash { secure_boot: true, .. }));
        let mut machine = "q35".to_string();
        if shared {
            machine.push_str(",memory-backend=mem");
        }
        if secure_boot {
            machine.push_str(",smm=on");
        }
        push(&mut args, "-machine", machine);
        if self.cpu_host {
            push(&mut args, "-cpu", "host".into());
        }
        push(&mut args, "-m", format!("{}M", config.mem_size_mib));
        push(&mut args, "-smp", config.vcpu_count.to_string());
        if shared {
            // vhost-user: OVS maps guest RAM, so it must be shared.
            push(
                &mut args,
                "-object",
                format!(
                    "memory-backend-memfd,id=mem,size={}M,share=on{}",
                    config.mem_size_mib,
                    if config.hugepages { ",hugetlb=on" } else { "" }
                ),
            );
        }

        match &self.firmware {
            Some(Firmware::Pflash { code, vars, secure_boot }) => {
                if *secure_boot {
                    push(&mut args, "-global", "driver=cfi.pflash01,property=secure,value=on".into());
                }
                push(
                    &mut args,
                    "-drive",
                    format!("if=pflash,format=raw,unit=0,readonly=on,file={}", qopt(code)),
                );
                push(&mut args, "-drive", format!("if=pflash,format=raw,unit=1,file={}", qopt(vars)));
            }
            Some(Firmware::Bios(path)) => push(&mut args, "-bios", path.clone()),
            None => {
                push(&mut args, "-kernel", config.kernel_image_path.clone());
                push(&mut args, "-append", config.kernel_args.clone());
            }
        }

        // Root first, so it stays /dev/vda and is what the firmware boots.
        for (i, disk) in self.disks.iter().enumerate() {
            let node = format!("disk{}", i);
            push(&mut args, "-blockdev", disk.blockdev(&node));
            let boot = if i == 0 { ",bootindex=1" } else { "" };
            push(&mut args, "-device", format!("virtio-blk-pci,drive={},id=vd{}{}", node, i, boot));
        }

        for nic in &config.nic_bindings {
            for (a, b) in nic_args(nic, self.vhost_net) {
                push(&mut args, a, b);
            }
        }

        for device in &config.vfio_devices {
            push(
                &mut args,
                "-device",
                format!("vfio-pci,host={},id={}", vfio_bdf(device), vfio_device_id(device)),
            );
        }

        push(&mut args, "-object", "rng-random,id=rng0,filename=/dev/urandom".into());
        push(&mut args, "-device", "virtio-rng-pci,rng=rng0".into());

        push(&mut args, "-qmp", format!("unix:{},server=on,wait=off", qopt(&self.qmp_socket)));
        push(&mut args, "-serial", "stdio".into());
        push(&mut args, "-display", "none".into());
        // Hold the guest at reset until `start` sends `cont`.
        args.push("-S".into());
        args
    }
}

/// `-netdev` / `-device` pairs for one NIC. vhost-user sockets are served
/// by QEMU (OVS's `dpdkvhostuserclient` connects); `wait=off` because the
/// guest is held with `-S` until OVS has had a chance to connect.
fn nic_args(nic: &NicBinding, vhost_net: bool) -> Vec<(&'static str, String)> {
    let id = &nic.id;
    let pairs = nic.queue_pairs.max(1);
    let queues = if pairs > 1 { format!(",queues={}", pairs) } else { String::new() };
    let mut out = Vec::new();
    match &nic.binding {
        VmPortBinding::Tap { ifname } => out.push((
            "-netdev",
            format!(
                "tap,id={},ifname={},script=no,downscript=no,vhost={}{}",
                id,
                qopt(ifname),
                if vhost_net { "on" } else { "off" },
                queues
            ),
        )),
        VmPortBinding::VhostUser { socket } => {
            out.push((
                "-chardev",
                format!(
                    "socket,id=chr-{},path={},server=on,wait=off",
                    id,
                    qopt(&socket.to_string_lossy())
                ),
            ));
            out.push(("-netdev", format!("vhost-user,id={},chardev=chr-{}{}", id, id, queues)));
        }
    }
    let mut device = format!("virtio-net-pci,netdev={},id=dev-{},mac={}", id, id, nic.mac);
    if let Some(mtu) = nic.mtu {
        device.push_str(&format!(",host_mtu={}", mtu));
    }
    if pairs > 1 {
        // One vector per queue plus config and control.
        device.push_str(&format!(",mq=on,vectors={}", 2 * pairs as u32 + 2));
    }
    out.push(("-device", device));
    out
}

/// Whether a failed launch looks like `-cpu host` was the problem.
fn cpu_model_rejected(log: &str) -> bool {
    let log = log.to_ascii_lowercase();
    log.contains("cpu") || log.contains("msr")
}

/// Why a launch attempt failed.
enum LaunchFailure {
    /// QEMU exited before QMP came up; `log` is what it printed.
    Exited { log: String, error: HypervisorError },
    Other(HypervisorError),
}

impl From<LaunchFailure> for HypervisorError {
    fn from(f: LaunchFailure) -> Self {
        match f {
            LaunchFailure::Exited { error, .. } | LaunchFailure::Other(error) => error,
        }
    }
}

impl From<HypervisorError> for LaunchFailure {
    fn from(e: HypervisorError) -> Self {
        LaunchFailure::Other(e)
    }
}

impl From<std::io::Error> for LaunchFailure {
    fn from(e: std::io::Error) -> Self {
        LaunchFailure::Other(e.into())
    }
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

    /// Resolve the config against the host, then launch; retried once
    /// without `-cpu host` if QEMU rejects the host CPU model.
    fn launch(&self, config: &VmConfig) -> Result<(), HypervisorError> {
        if let Some(version) = qemu_version() {
            if version < MIN_QEMU_VERSION {
                return Err(HypervisorError::Unsupported(format!(
                    "QEMU {}.{} is too old; glidex needs {}.{} or newer",
                    version.0, version.1, MIN_QEMU_VERSION.0, MIN_QEMU_VERSION.1
                )));
            }
        }
        let firmware = match &config.firmware_path {
            Some(code) => Some(self.prepare_firmware(code, config)?),
            None => None,
        };
        let disks = vm_disks(config)
            .into_iter()
            .map(|disk| QemuDisk {
                direct: supports_direct_io(&disk.path),
                disk,
            })
            .collect();
        let mut spec = LaunchSpec {
            config,
            qmp_socket: self.socket_path.clone(),
            firmware,
            disks,
            cpu_host: true,
            vhost_net: vhost_net_usable(),
        };
        if !spec.vhost_net && config.nic_bindings.iter().any(|n| matches!(n.binding, VmPortBinding::Tap { .. })) {
            tracing::warn!("/dev/vhost-net is not usable; tap NICs fall back to QEMU's userspace virtio-net");
        }

        match self.launch_with(&spec) {
            Err(LaunchFailure::Exited { log, .. }) if cpu_model_rejected(&log) => {
                tracing::warn!(
                    "qemu-system-x86_64 rejected -cpu host; retrying with QEMU's default CPU model:\n{}",
                    log.trim()
                );
                spec.cpu_host = false;
                self.running.store(true, Ordering::SeqCst);
                self.launch_with(&spec).map_err(Into::into)
            }
            other => other.map_err(Into::into),
        }
    }

    /// Map the firmware: OVMF code read-only, plus this VM's own variable
    /// store, copied from the pristine template on first boot (or when the
    /// template changed size, e.g. after a 2M -> 4M OVMF upgrade).
    fn prepare_firmware(&self, code: &str, config: &VmConfig) -> Result<Firmware, HypervisorError> {
        let code_path = Path::new(code);
        if !code_path.exists() {
            return Err(HypervisorError::InvalidConfig(format!(
                "UEFI firmware {} not found; install OVMF (ovmf / edk2-ovmf) or set firmware_path",
                code
            )));
        }
        let Some(template) = ovmf_vars_template(code_path) else {
            return Ok(Firmware::Bios(code.to_string()));
        };
        let vars = config
            .firmware_vars_path
            .clone()
            .unwrap_or_else(|| format!("{}.ovmf-vars.fd", self.log_path.trim_end_matches(".log")));
        let template_len = std::fs::metadata(&template)?.len();
        let current_len = std::fs::metadata(&vars).map(|m| m.len()).ok();
        if current_len != Some(template_len) {
            if let Some(dir) = Path::new(&vars).parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::copy(&template, &vars)?;
        }
        let name = code_path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let secure_boot = ["secboot", ".ms.", "snakeoil"].iter().any(|s| name.contains(s));
        Ok(Firmware::Pflash {
            code: code.to_string(),
            vars,
            secure_boot,
        })
    }

    fn launch_with(&self, spec: &LaunchSpec<'_>) -> Result<(), LaunchFailure> {
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.console_socket_path);

        // Truncate once; the console proxy and QEMU's stderr then both
        // append, so neither overwrites the other.
        File::create(&self.log_path)?;
        let log_file = OpenOptions::new().append(true).open(&self.log_path)?;
        let stderr_log = OpenOptions::new().append(true).open(&self.log_path)?;

        let OpenptyResult { master, slave } = openpty(None, None).map_err(|e| {
            HypervisorError::SocketConnection(format!("Failed to create PTY: {}", e))
        })?;

        let slave_raw = slave.as_raw_fd();
        let stdin_fd = unsafe { File::from_raw_fd(libc::dup(slave_raw)) };
        let stdout_fd = unsafe { File::from_raw_fd(libc::dup(slave_raw)) };

        // The serial console is the PTY; QEMU's own messages go to the log
        // only, so they never land in a guest's console session.
        let mut cmd = Command::new("qemu-system-x86_64");
        cmd.args(spec.args());
        let child = unsafe {
            cmd.stdin(Stdio::from(stdin_fd))
                .stdout(Stdio::from(stdout_fd))
                .stderr(Stdio::from(stderr_log))
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
                return Err(LaunchFailure::Exited {
                    error: HypervisorError::ProcessStart(std::io::Error::other(format!(
                        "qemu-system-x86_64 exited with {} before QMP was ready.\n--- qemu output ---\n{}",
                        exit_status,
                        log.trim()
                    ))),
                    log,
                });
            }

            if Path::new(&self.socket_path).exists() && probe_qmp(&self.socket_path) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        self.cleanup_partial();
        Err(LaunchFailure::Other(HypervisorError::Timeout(format!(
            "QMP socket not ready after timeout.\n--- qemu output ---\n{}",
            log.trim()
        ))))
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
        let mut clients: Vec<UnixStream> = Vec::new();
        let mut buf = [0u8; 4096];
        // PTY reads EOF once QEMU exits. We don't tear down the listener in
        // that case — clients should still be able to connect and read the
        // captured log to see *why* the guest died. The PTY itself is
        // closed then (`None`).
        let mut pty = Some(File::from(master));

        if let Some(pty) = &pty {
            unsafe {
                let raw = pty.as_raw_fd();
                let flags = libc::fcntl(raw, libc::F_GETFL);
                libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }

        while running.load(Ordering::SeqCst) {
            if let Ok((stream, _)) = listener.accept() {
                stream.set_nonblocking(true).ok();
                if let Ok(mut existing_log) = File::open(log_path) {
                    let mut log_content = Vec::new();
                    if existing_log.read_to_end(&mut log_content).is_ok()
                        && !log_content.is_empty()
                    {
                        let mut s = &stream;
                        let _ = s.write_all(&log_content);
                    }
                }
                clients.push(stream);
            }

            if let Some(file) = &pty {
                let mut reader = file;
                match reader.read(&mut buf) {
                    Ok(0) => pty = None,
                    Ok(n) => {
                        let data = &buf[..n];
                        let _ = log_file.write_all(data);
                        let _ = log_file.flush();
                        clients.retain_mut(|client| client.write_all(data).is_ok());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => pty = None,
                }
            }

            if let Some(file) = &pty {
                for client in &mut clients {
                    match client.read(&mut buf) {
                        Ok(0) => {}
                        Ok(n) => {
                            let mut writer = file;
                            let _ = writer.write_all(&buf[..n]);
                            let _ = writer.flush();
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

    fn request_shutdown(&self) -> Result<(), HypervisorError> {
        self.client.system_powerdown()
    }

    fn add_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        self.client.add_vfio_device(device_path)
    }

    fn remove_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        self.client.remove_vfio_device(device_path)
    }

    fn is_running(&self) -> bool {
        // QEMU exits once the guest powers off.
        self.running.load(Ordering::SeqCst)
            && self
                .child
                .lock()
                .unwrap()
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::qemu_img::DiskFormat;
    use crate::models::DiskBinding;

    fn config(extra: serde_json::Value) -> VmConfig {
        let mut base = serde_json::json!({
            "vcpu_count": 2, "mem_size_mib": 1024, "rootfs_path": "/disks/root.raw",
            "kernel_image_path": "/boot/vmlinux", "kernel_args": "console=ttyS0"
        });
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn spec(config: &VmConfig) -> LaunchSpec<'_> {
        LaunchSpec {
            config,
            qmp_socket: "/tmp/qmp.sock".into(),
            firmware: None,
            disks: vm_disks(config)
                .into_iter()
                .map(|disk| QemuDisk { disk, direct: false })
                .collect(),
            cpu_host: true,
            vhost_net: true,
        }
    }

    /// The values that follow each occurrence of `flag`.
    fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].as_str())
            .collect()
    }

    fn tap(id: &str, ifname: &str, mac: &str, queue_pairs: u8, mtu: Option<u16>) -> NicBinding {
        NicBinding {
            id: id.into(),
            mac: mac.into(),
            binding: VmPortBinding::Tap { ifname: ifname.into() },
            queue_pairs,
            mtu,
        }
    }

    #[test]
    fn kernel_boot_command_line() {
        let config = config(serde_json::json!({}));
        let args = spec(&config).args();
        assert_eq!(&args[..3], ["-nodefaults", "-no-user-config", "-enable-kvm"]);
        assert_eq!(values(&args, "-machine"), ["q35"]);
        assert_eq!(values(&args, "-cpu"), ["host"]);
        assert_eq!(values(&args, "-m"), ["1024M"]);
        assert_eq!(values(&args, "-smp"), ["2"]);
        assert_eq!(values(&args, "-kernel"), ["/boot/vmlinux"]);
        assert_eq!(values(&args, "-append"), ["console=ttyS0"]);
        assert_eq!(values(&args, "-qmp"), ["unix:/tmp/qmp.sock,server=on,wait=off"]);
        assert!(values(&args, "-netdev").is_empty());
        assert!(!args.iter().any(|a| a == "-no-reboot"));
        assert_eq!(args.last().map(String::as_str), Some("-S"));
        assert!(values(&args, "-device").contains(&"virtio-rng-pci,rng=rng0"));
    }

    #[test]
    fn cpu_host_can_be_dropped() {
        let config = config(serde_json::json!({}));
        let mut spec = spec(&config);
        spec.cpu_host = false;
        assert!(values(&spec.args(), "-cpu").is_empty());
    }

    #[test]
    fn disks_root_data_seed_in_order_and_backing_files_blocked() {
        let mut config = config(serde_json::json!({"cloud_init_path": "/tmp/seed,1.img"}));
        config.root_disk_binding = Some(DiskBinding { path: "/disks/root.qcow2".into(), format: DiskFormat::Qcow2, backing_files: true });
        config.data_disk_bindings = vec![
            DiskBinding { path: "/disks/data.qcow2".into(), format: DiskFormat::Qcow2, backing_files: false },
            DiskBinding { path: "/disks/data.raw".into(), format: DiskFormat::Raw, backing_files: false },
        ];
        let args = spec(&config).args();
        let blockdevs: Vec<serde_json::Value> = values(&args, "-blockdev")
            .iter()
            .map(|b| serde_json::from_str(b).unwrap())
            .collect();
        assert_eq!(blockdevs.len(), 4);
        assert_eq!(blockdevs[0], serde_json::json!({
            "driver": "qcow2", "node-name": "disk0", "read-only": false,
            "file": {"driver": "file", "filename": "/disks/root.qcow2", "read-only": false}
        }));
        // A glidex overlay may open its base image; nothing else may.
        assert_eq!(blockdevs[1]["backing"], serde_json::Value::Null);
        assert!(blockdevs[1].as_object().unwrap().contains_key("backing"));
        assert!(!blockdevs[2].as_object().unwrap().contains_key("backing"));
        assert_eq!(blockdevs[2]["driver"], "raw");
        assert_eq!(blockdevs[3]["file"]["filename"], "/tmp/seed,1.img");
        assert_eq!(blockdevs[3]["read-only"], true);
        let devices: Vec<&str> = values(&args, "-device")
            .into_iter()
            .filter(|d| d.starts_with("virtio-blk-pci"))
            .collect();
        assert_eq!(devices, [
            "virtio-blk-pci,drive=disk0,id=vd0,bootindex=1",
            "virtio-blk-pci,drive=disk1,id=vd1",
            "virtio-blk-pci,drive=disk2,id=vd2",
            "virtio-blk-pci,drive=disk3,id=vd3",
        ]);
    }

    #[test]
    fn user_supplied_qcow2_root_never_follows_backing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evil.qcow2");
        let mut qcow2 = b"QFI\xfb\x00\x00\x00\x03".to_vec();
        qcow2.resize(4096, 0);
        std::fs::write(&path, &qcow2).unwrap();
        let config = config(serde_json::json!({"rootfs_path": path.to_str().unwrap()}));
        let args = spec(&config).args();
        let root: serde_json::Value = serde_json::from_str(values(&args, "-blockdev")[0]).unwrap();
        assert_eq!(root["driver"], "qcow2");
        assert_eq!(root["backing"], serde_json::Value::Null);
    }

    #[test]
    fn direct_io_disks_bypass_the_page_cache() {
        let config = config(serde_json::json!({}));
        let mut spec = spec(&config);
        spec.disks[0].direct = true;
        let root: serde_json::Value = serde_json::from_str(values(&spec.args(), "-blockdev")[0]).unwrap();
        assert_eq!(root["cache"], serde_json::json!({"direct": true}));
        assert_eq!(root["file"]["aio"], "native");
    }

    #[test]
    fn tap_nics() {
        let mut config = config(serde_json::json!({}));
        config.nic_bindings = vec![
            tap("net0", "gxabc-0", "02:00:00:00:00:01", 1, None),
            tap("net1", "gxabc-1", "02:00:00:00:00:02", 4, Some(9000)),
        ];
        let mut spec = spec(&config);
        let args = spec.args();
        assert_eq!(values(&args, "-netdev"), [
            "tap,id=net0,ifname=gxabc-0,script=no,downscript=no,vhost=on",
            "tap,id=net1,ifname=gxabc-1,script=no,downscript=no,vhost=on,queues=4",
        ]);
        let nics: Vec<&str> = values(&args, "-device")
            .into_iter()
            .filter(|d| d.starts_with("virtio-net-pci"))
            .collect();
        assert_eq!(nics, [
            "virtio-net-pci,netdev=net0,id=dev-net0,mac=02:00:00:00:00:01",
            "virtio-net-pci,netdev=net1,id=dev-net1,mac=02:00:00:00:00:02,host_mtu=9000,mq=on,vectors=10",
        ]);
        // Tap NICs alone don't need shared guest memory.
        assert_eq!(values(&args, "-machine"), ["q35"]);

        spec.vhost_net = false;
        assert!(values(&spec.args(), "-netdev")[0].contains("vhost=off"));
    }

    #[test]
    fn vhost_user_nic_shares_guest_memory() {
        let mut config = config(serde_json::json!({}));
        config.nic_bindings = vec![NicBinding {
            id: "net0".into(),
            mac: "02:00:00:00:00:01".into(),
            binding: VmPortBinding::VhostUser { socket: "/run/glidex/vhost/x.net0.sock".into() },
            queue_pairs: 2,
            mtu: None,
        }];
        let args = spec(&config).args();
        assert_eq!(values(&args, "-machine"), ["q35,memory-backend=mem"]);
        assert!(values(&args, "-object").contains(&"memory-backend-memfd,id=mem,size=1024M,share=on"));
        assert_eq!(values(&args, "-chardev"), ["socket,id=chr-net0,path=/run/glidex/vhost/x.net0.sock,server=on,wait=off"]);
        assert_eq!(values(&args, "-netdev"), ["vhost-user,id=net0,chardev=chr-net0,queues=2"]);
        assert!(values(&args, "-device").contains(&"virtio-net-pci,netdev=net0,id=dev-net0,mac=02:00:00:00:00:01,mq=on,vectors=6"));
    }

    #[test]
    fn hugepages_back_guest_memory() {
        let config = config(serde_json::json!({"hugepages": true}));
        let args = spec(&config).args();
        assert_eq!(values(&args, "-machine"), ["q35,memory-backend=mem"]);
        assert!(values(&args, "-object").contains(&"memory-backend-memfd,id=mem,size=1024M,share=on,hugetlb=on"));
    }

    #[test]
    fn firmware_boot_maps_code_and_vars() {
        let config = config(serde_json::json!({}));
        let mut spec = spec(&config);
        spec.firmware = Some(Firmware::Pflash {
            code: "/usr/share/OVMF/OVMF_CODE_4M.fd".into(),
            vars: "/var/lib/glidex/a,b.fd".into(),
            secure_boot: false,
        });
        let args = spec.args();
        assert!(values(&args, "-kernel").is_empty());
        assert!(values(&args, "-append").is_empty());
        assert_eq!(values(&args, "-drive"), [
            "if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd",
            "if=pflash,format=raw,unit=1,file=/var/lib/glidex/a,,b.fd",
        ]);
        assert_eq!(values(&args, "-machine"), ["q35"]);

        spec.firmware = Some(Firmware::Pflash { code: "c".into(), vars: "v".into(), secure_boot: true });
        let args = spec.args();
        assert_eq!(values(&args, "-machine"), ["q35,smm=on"]);
        assert_eq!(values(&args, "-global"), ["driver=cfi.pflash01,property=secure,value=on"]);

        spec.firmware = Some(Firmware::Bios("/usr/share/qemu/OVMF.fd".into()));
        assert_eq!(values(&spec.args(), "-bios"), ["/usr/share/qemu/OVMF.fd"]);
    }

    #[test]
    fn vfio_devices_on_the_command_line() {
        let config = config(serde_json::json!({"vfio_devices": ["/sys/bus/pci/devices/0000:41:00.0"]}));
        assert!(values(&spec(&config).args(), "-device").contains(&"vfio-pci,host=0000:41:00.0,id=_vfio_0000_41_00_0"));
    }

    #[test]
    fn vars_template_sits_next_to_code() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd", "OVMF_CODE.4m.fd", "OVMF.fd", "edk2-x86_64-code.fd"] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        assert_eq!(ovmf_vars_template(&dir.path().join("OVMF_CODE_4M.fd")), Some(dir.path().join("OVMF_VARS_4M.fd")));
        // No OVMF_VARS.4m.fd shipped alongside.
        assert_eq!(ovmf_vars_template(&dir.path().join("OVMF_CODE.4m.fd")), None);
        assert_eq!(ovmf_vars_template(&dir.path().join("OVMF.fd")), None);
        assert_eq!(ovmf_vars_template(&dir.path().join("edk2-x86_64-code.fd")), None);
    }

    #[test]
    fn firmware_gets_a_private_vars_copy() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("OVMF_CODE_4M.fd");
        std::fs::write(&code, b"code").unwrap();
        std::fs::write(dir.path().join("OVMF_VARS_4M.fd"), b"pristine").unwrap();
        let vars = dir.path().join("vm/vars.fd");
        let mut config = config(serde_json::json!({}));
        config.firmware_vars_path = Some(vars.to_string_lossy().into_owned());
        let qemu = QemuInstance::new("/tmp/x.sock", "/tmp/x.console.sock", "/tmp/x.log");

        let fw = qemu.prepare_firmware(code.to_str().unwrap(), &config).unwrap();
        assert_eq!(fw, Firmware::Pflash {
            code: code.to_string_lossy().into_owned(),
            vars: vars.to_string_lossy().into_owned(),
            secure_boot: false,
        });
        assert_eq!(std::fs::read(&vars).unwrap(), b"pristine");

        // Variables the guest wrote survive the next boot.
        std::fs::write(&vars, b"BOOTVARS").unwrap();
        qemu.prepare_firmware(code.to_str().unwrap(), &config).unwrap();
        assert_eq!(std::fs::read(&vars).unwrap(), b"BOOTVARS");

        assert!(matches!(
            qemu.prepare_firmware("/nonexistent/OVMF_CODE.fd", &config),
            Err(HypervisorError::InvalidConfig(_))
        ));
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_qemu_version("QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.2)\n"), Some((10, 2)));
        assert_eq!(parse_qemu_version("QEMU emulator version 5.2.0\n"), Some((5, 2)));
        assert_eq!(parse_qemu_version("garbage"), None);
        assert!((5, 2) < MIN_QEMU_VERSION && (10, 2) >= MIN_QEMU_VERSION);
    }

    #[test]
    fn cpu_errors_trigger_the_fallback() {
        assert!(cpu_model_rejected("qemu-system-x86_64: error: failed to set MSR 0x345 to 0x2000"));
        assert!(cpu_model_rejected("host doesn't support requested feature: CPUID.80000001H:ECX.svm"));
        assert!(!cpu_model_rejected("Could not open '/disks/root.raw': No such file or directory"));
    }
}
