use super::{Hypervisor, HypervisorError, HypervisorProcess, HypervisorType};
use crate::models::VmConfig;
use glidex_ovs::vm_port::VmPortBinding;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Where `glidex-install` puts Cloud-Hypervisor's EDK2 UEFI firmware:
/// `~/.glidex/CLOUDHV.fd` on x86_64, `~/.glidex/CLOUDHV_EFI.fd` on aarch64.
/// Returns the path even if the file has not been downloaded.
pub fn default_firmware_path() -> Option<std::path::PathBuf> {
    let name = if cfg!(target_arch = "aarch64") {
        "CLOUDHV_EFI.fd"
    } else {
        "CLOUDHV.fd"
    };
    dirs::home_dir().map(|home| home.join(".glidex").join(name))
}

/// Cloud-Hypervisor API request structures
#[derive(Debug, Serialize)]
struct CpuConfig {
    boot_vcpus: u8,
    max_vcpus: u8,
}

#[derive(Debug, Serialize)]
struct MemoryConfig {
    size: u64, // bytes
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    shared: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    hugepages: bool,
}

/// `NetConfig` subset (CH v53 OpenAPI schema).
#[derive(Debug, Serialize, PartialEq)]
struct NetConfig {
    id: String,
    mac: String,
    num_queues: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    tap: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    vhost_user: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    vhost_socket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vhost_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mtu: Option<u16>,
}

fn net_configs(config: &VmConfig) -> Vec<NetConfig> {
    config
        .nic_bindings
        .iter()
        .map(|nic| {
            let mut net = NetConfig {
                id: nic.id.clone(),
                mac: nic.mac.clone(),
                num_queues: 2 * nic.queue_pairs as u16,
                tap: None,
                vhost_user: false,
                vhost_socket: None,
                vhost_mode: None,
                mtu: nic.mtu,
            };
            match &nic.binding {
                VmPortBinding::Tap { ifname } => net.tap = Some(ifname.clone()),
                VmPortBinding::VhostUser { socket } => {
                    // OVS's dpdkvhostuserclient is the client, so CH serves.
                    net.vhost_user = true;
                    net.vhost_socket = Some(socket.to_string_lossy().into_owned());
                    net.vhost_mode = Some("Server".into());
                }
            }
            net
        })
        .collect()
}

#[derive(Debug, Serialize)]
struct PayloadConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    firmware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cmdline: Option<String>,
}

#[derive(Debug, Serialize)]
struct DiskConfig {
    path: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    readonly: bool,
    // CH deprecated image-type auto-detection in v52; `None` (unreadable
    // image) leaves the field out so CH reports the open error itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    image_type: Option<ImageType>,
}

/// Disk image formats CH can open; names match its `ImageType` API enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum ImageType {
    FixedVhd,
    Qcow2,
    Raw,
    Vhdx,
}

/// Identify a disk image by the same magic bytes CH's own probe used.
/// Anything without a recognised header is a raw image.
fn detect_image_type(path: &str) -> Option<ImageType> {
    use std::io::{Seek, SeekFrom};
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 8];
    let n = file.read(&mut header).ok()?;
    if n >= 4 && header[..4] == *b"QFI\xfb" {
        return Some(ImageType::Qcow2);
    }
    if n == 8 && header == *b"vhdxfile" {
        return Some(ImageType::Vhdx);
    }
    // VHD keeps a 512-byte footer at the end: "conectix" cookie, then the
    // big-endian disk type at offset 60 (2 = fixed, the only kind CH runs).
    let mut footer = [0u8; 512];
    if file.seek(SeekFrom::End(-512)).is_ok() && file.read_exact(&mut footer).is_ok() {
        if footer[..8] == *b"conectix" && footer[60..64] == 2u32.to_be_bytes() {
            return Some(ImageType::FixedVhd);
        }
    }
    Some(ImageType::Raw)
}

#[derive(Debug, Serialize)]
struct ConsoleConfig {
    mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file: Option<String>,
}

#[derive(Debug, Serialize)]
struct VfioDeviceConfig {
    path: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    iommu: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Debug, Serialize)]
struct VmCreateConfig {
    cpus: CpuConfig,
    memory: MemoryConfig,
    payload: PayloadConfig,
    disks: Vec<DiskConfig>,
    console: ConsoleConfig,
    serial: ConsoleConfig,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    devices: Vec<VfioDeviceConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    net: Vec<NetConfig>,
}

/// Find the end of HTTP headers (position after the \r\n\r\n separator).
fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

/// Parse Content-Length from raw HTTP header bytes.
fn parse_content_length(headers: &[u8]) -> usize {
    let header_str = String::from_utf8_lossy(headers).to_lowercase();
    for line in header_str.lines() {
        if let Some(val) = line.strip_prefix("content-length:") {
            return val.trim().parse().unwrap_or(0);
        }
    }
    0
}

/// HTTP client for communicating with Cloud-Hypervisor API over Unix socket
pub struct CloudHypervisorClient {
    socket_path: String,
}

impl CloudHypervisorClient {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
        }
    }

    /// Parsed HTTP response with status code and optional body.
    fn send_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, Option<String>), HypervisorError> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| HypervisorError::SocketConnection(e.to_string()))?;

        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .map_err(HypervisorError::ProcessStart)?;

        // Build request matching the official cloud-hypervisor api_client format:
        //   {METHOD} /api/v1/{path} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n
        // With body: add Content-Type and Content-Length headers
        let request = if let Some(body_str) = body {
            format!(
                "{} /api/v1{} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                method, path, body_str.len(), body_str
            )
        } else {
            format!(
                "{} /api/v1{} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n",
                method, path
            )
        };

        stream
            .write_all(request.as_bytes())
            .map_err(HypervisorError::ProcessStart)?;
        stream.flush().map_err(HypervisorError::ProcessStart)?;

        // Read the full response in chunks (matching official client approach)
        let mut raw = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    // Check if we have a complete response (headers + full body)
                    if let Some(header_end) = find_header_end(&raw) {
                        let content_len = parse_content_length(&raw[..header_end]);
                        if raw.len() >= header_end + content_len {
                            break;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                Err(e) => return Err(HypervisorError::ProcessStart(e)),
            }
        }

        let response = String::from_utf8_lossy(&raw);

        // Parse status code from the first line: "HTTP/1.x {code} ..."
        let status_code = response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);

        // Extract body after the header/body separator
        let body = if let Some(pos) = response.find("\r\n\r\n") {
            let b = &response[pos + 4..];
            if b.is_empty() { None } else { Some(b.to_string()) }
        } else {
            None
        };

        Ok((status_code, body))
    }

    /// Check that the response status indicates success (2xx).
    fn expect_success(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Option<String>, HypervisorError> {
        let (status, response_body) = self.send_request(method, path, body)?;
        if (200..300).contains(&status) {
            Ok(response_body)
        } else {
            Err(HypervisorError::ApiRequest(format!(
                "{} /api/v1{} failed with status {}: {}",
                method,
                path,
                status,
                response_body.unwrap_or_default()
            )))
        }
    }

    pub fn create_vm(&self, config: &VmConfig) -> Result<(), HypervisorError> {
        // Firmware boot (CLOUDHV.fd) hands off to the disk's own bootloader,
        // so there is no kernel/cmdline. Distro cloud images put their
        // console on ttyS0, so expose the serial port instead of hvc0.
        let (payload, console_mode, serial_mode) = match &config.firmware_path {
            Some(firmware) => (
                PayloadConfig {
                    firmware: Some(firmware.clone()),
                    kernel: None,
                    cmdline: None,
                },
                "Off",
                "Pty",
            ),
            None => (
                PayloadConfig {
                    firmware: None,
                    kernel: Some(config.kernel_image_path.clone()),
                    cmdline: Some(config.kernel_args.clone()),
                },
                "Pty",
                "Off",
            ),
        };

        let vm_config = VmCreateConfig {
            cpus: CpuConfig {
                boot_vcpus: config.vcpu_count,
                max_vcpus: config.vcpu_count,
            },
            memory: MemoryConfig {
                size: (config.mem_size_mib as u64) * 1024 * 1024,
                // vhost-user: OVS maps guest RAM, so it must be shared.
                shared: config
                    .nic_bindings
                    .iter()
                    .any(|n| matches!(n.binding, VmPortBinding::VhostUser { .. })),
                hugepages: config.hugepages,
            },
            net: net_configs(config),
            payload,
            disks: std::iter::once(DiskConfig {
                path: config.rootfs_path.clone(),
                readonly: false,
                image_type: detect_image_type(&config.rootfs_path),
            })
            // The seed is always the raw FAT image `write_seed_image` builds.
            .chain(config.cloud_init_path.iter().map(|path| DiskConfig {
                path: path.clone(),
                readonly: true,
                image_type: Some(ImageType::Raw),
            }))
            .collect(),
            console: ConsoleConfig {
                mode: console_mode.to_string(),
                file: None,
            },
            serial: ConsoleConfig {
                mode: serial_mode.to_string(),
                file: None,
            },
            devices: config
                .vfio_devices
                .iter()
                .map(|path| VfioDeviceConfig {
                    path: path.clone(),
                    iommu: false,
                    id: Some(vfio_device_id(path)),
                })
                .collect(),
        };

        let body = serde_json::to_string(&vm_config)
            .map_err(|e| HypervisorError::ApiRequest(e.to_string()))?;

        self.expect_success("PUT", "/vm.create", Some(&body))?;
        Ok(())
    }

    /// Extract the PTY path of whichever of `console` / `serial` is in Pty
    /// mode from the vm.info response.
    pub fn get_console_pty_path(&self) -> Result<Option<String>, HypervisorError> {
        let body = self
            .expect_success("GET", "/vm.info", None)?
            .ok_or_else(|| {
                HypervisorError::ApiRequest("vm.info returned no body".to_string())
            })?;

        Ok(pty_path_from_vm_info(&body))
    }

    pub fn boot_vm(&self) -> Result<(), HypervisorError> {
        self.expect_success("PUT", "/vm.boot", None)?;
        Ok(())
    }

    pub fn pause_vm(&self) -> Result<(), HypervisorError> {
        self.expect_success("PUT", "/vm.pause", None)?;
        Ok(())
    }

    pub fn resume_vm(&self) -> Result<(), HypervisorError> {
        self.expect_success("PUT", "/vm.resume", None)?;
        Ok(())
    }

    pub fn shutdown_vm(&self) -> Result<(), HypervisorError> {
        self.expect_success("PUT", "/vm.shutdown", None)?;
        Ok(())
    }

    pub fn add_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        let body = serde_json::json!({
            "path": device_path,
            "iommu": false,
            "id": vfio_device_id(device_path),
        })
        .to_string();
        self.expect_success("PUT", "/vm.add-device", Some(&body))?;
        Ok(())
    }

    pub fn remove_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        let body = serde_json::json!({
            "id": vfio_device_id(device_path),
        })
        .to_string();
        self.expect_success("PUT", "/vm.remove-device", Some(&body))?;
        Ok(())
    }
}

fn pty_path_from_vm_info(body: &str) -> Option<String> {
    let json = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let config = json.get("config")?;
    ["console", "serial"].iter().find_map(|dev| {
        let dev = config.get(*dev)?;
        if dev.get("mode")?.as_str()? != "Pty" {
            return None;
        }
        dev.get("file")?.as_str().map(str::to_string)
    })
}

/// Derive a deterministic Cloud-Hypervisor device ID from a sysfs path.
/// e.g. "/sys/bus/pci/devices/0000:41:00.0" -> "_vfio_0000_41_00_0"
fn vfio_device_id(path: &str) -> String {
    let bdf = path.rsplit('/').next().unwrap_or(path);
    format!("_vfio_{}", bdf.replace(':', "_").replace('.', "_"))
}

/// Manages a running Cloud-Hypervisor process
pub struct CloudHypervisorProcessHandle {
    child: Mutex<Option<Child>>,
    socket_path: String,
    console_socket_path: String,
    log_path: String,
    running: Arc<AtomicBool>,
    console_thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl CloudHypervisorProcessHandle {
    pub fn spawn(
        socket_path: &str,
        console_socket_path: &str,
        log_path: &str,
    ) -> Result<Self, HypervisorError> {
        // Remove existing sockets if present
        let _ = std::fs::remove_file(socket_path);
        let _ = std::fs::remove_file(console_socket_path);

        // Create/truncate log file
        let _log_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(log_path)?;

        // Spawn cloud-hypervisor with API socket
        let child = Command::new("cloud-hypervisor")
            .arg("--api-socket")
            .arg(socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        let running = Arc::new(AtomicBool::new(true));

        // Wait for API socket to be available
        for _ in 0..50 {
            if std::path::Path::new(socket_path).exists() {
                return Ok(Self {
                    child: Mutex::new(Some(child)),
                    socket_path: socket_path.to_string(),
                    console_socket_path: console_socket_path.to_string(),
                    log_path: log_path.to_string(),
                    running,
                    console_thread: Mutex::new(None),
                });
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Cleanup on timeout
        running.store(false, Ordering::SeqCst);
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(socket_path);

        Err(HypervisorError::Timeout(
            "Socket not available after timeout".to_string(),
        ))
    }

    /// Start the console proxy thread that bridges the PTY to a Unix socket
    pub fn start_console_proxy(&self, pty_path: &str) -> Result<(), HypervisorError> {
        // Remove existing console socket if present
        let _ = std::fs::remove_file(&self.console_socket_path);

        // Open the PTY
        let pty_fd = OpenOptions::new()
            .read(true)
            .write(true)
            .open(pty_path)
            .map_err(|e| {
                HypervisorError::SocketConnection(format!("Failed to open PTY {}: {}", pty_path, e))
            })?;

        // Create Unix socket for console connections
        let console_listener = UnixListener::bind(&self.console_socket_path).map_err(|e| {
            HypervisorError::SocketConnection(format!("Failed to create console socket: {}", e))
        })?;
        console_listener.set_nonblocking(true).map_err(|e| {
            HypervisorError::SocketConnection(format!("Failed to set non-blocking: {}", e))
        })?;

        // Open log file for writing
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;

        let running_clone = self.running.clone();
        let log_path_clone = self.log_path.clone();

        // Spawn thread to handle console I/O
        let console_thread = thread::spawn(move || {
            Self::console_proxy_loop(pty_fd, console_listener, log_file, &log_path_clone, running_clone);
        });

        *self.console_thread.lock().unwrap() = Some(console_thread);
        Ok(())
    }

    fn console_proxy_loop(
        pty_file: File,
        listener: UnixListener,
        mut log_file: File,
        log_path: &str,
        running: Arc<AtomicBool>,
    ) {
        let pty_raw = pty_file.as_raw_fd();
        let mut clients: Vec<UnixStream> = Vec::new();
        let mut buf = [0u8; 4096];
        // PTY reads EOF once cloud-hypervisor's virtio console closes. We
        // don't tear down the listener in that case — clients should still
        // be able to connect and replay the captured log to diagnose why
        // the guest died.
        let mut pty_alive = true;

        // Set PTY to non-blocking
        unsafe {
            let flags = libc::fcntl(pty_raw, libc::F_GETFL);
            libc::fcntl(pty_raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        while running.load(Ordering::SeqCst) {
            // Accept new client connections
            if let Ok((stream, _)) = listener.accept() {
                stream.set_nonblocking(true).ok();
                // Send existing log content to new client
                if let Ok(mut existing_log) = File::open(log_path) {
                    let mut log_content = Vec::new();
                    if existing_log.read_to_end(&mut log_content).is_ok() && !log_content.is_empty()
                    {
                        let mut s = stream.try_clone().unwrap();
                        let _ = s.write_all(&log_content);
                    }
                }
                clients.push(stream);
            }

            if pty_alive {
                // Read from PTY and broadcast to clients + log file
                let mut pty_reader = unsafe { File::from_raw_fd(libc::dup(pty_raw)) };
                match pty_reader.read(&mut buf) {
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

                // Read from clients and write to PTY
                for client in &mut clients {
                    match client.read(&mut buf) {
                        Ok(0) => {}
                        Ok(n) => {
                            let mut pty_writer =
                                unsafe { File::from_raw_fd(libc::dup(pty_raw)) };
                            let _ = pty_writer.write_all(&buf[..n]);
                            let _ = pty_writer.flush();
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

/// Cloud-Hypervisor instance that implements HypervisorProcess
pub struct CloudHypervisorInstance {
    process: CloudHypervisorProcessHandle,
    client: CloudHypervisorClient,
}

impl CloudHypervisorInstance {
    pub fn new(process: CloudHypervisorProcessHandle) -> Self {
        let client = CloudHypervisorClient::new(&process.socket_path);
        Self { process, client }
    }
}

impl HypervisorProcess for CloudHypervisorInstance {
    fn configure(&self, config: &VmConfig) -> Result<(), HypervisorError> {
        self.client.create_vm(config)?;
        Ok(())
    }

    fn start(&self) -> Result<(), HypervisorError> {
        self.client.boot_vm()?;

        // The console PTY is allocated during vm.boot (device creation),
        // not during vm.create. Poll for the PTY path to become available.
        for _ in 0..30 {
            match self.client.get_console_pty_path() {
                Ok(Some(pty_path)) => {
                    self.process.start_console_proxy(&pty_path)?;
                    return Ok(());
                }
                Ok(None) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }

        Err(HypervisorError::Timeout(
            "Console PTY path not available after boot".to_string(),
        ))
    }

    fn pause(&self) -> Result<(), HypervisorError> {
        self.client.pause_vm()
    }

    fn resume(&self) -> Result<(), HypervisorError> {
        self.client.resume_vm()
    }

    fn kill(&self) -> Result<(), HypervisorError> {
        self.process.running.store(false, Ordering::SeqCst);

        // Try graceful shutdown first
        let _ = self.client.shutdown_vm();

        // Then force kill if needed
        if let Some(mut child) = self.process.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }

        // Wait for console thread to finish
        if let Some(handle) = self.process.console_thread.lock().unwrap().take() {
            let _ = handle.join();
        }

        let _ = std::fs::remove_file(&self.process.socket_path);
        let _ = std::fs::remove_file(&self.process.console_socket_path);
        Ok(())
    }

    fn add_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        self.client.add_device(device_path)
    }

    fn remove_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        self.client.remove_device(device_path)
    }

    fn is_running(&self) -> bool {
        self.process.running.load(Ordering::SeqCst)
    }

    fn socket_path(&self) -> &str {
        &self.process.socket_path
    }

    fn console_socket_path(&self) -> &str {
        &self.process.console_socket_path
    }

    fn log_path(&self) -> &str {
        &self.process.log_path
    }
}

/// Cloud-Hypervisor backend factory
pub struct CloudHypervisorBackend;

impl Hypervisor for CloudHypervisorBackend {
    fn spawn(
        &self,
        socket_path: &str,
        console_socket_path: &str,
        log_path: &str,
    ) -> Result<Box<dyn HypervisorProcess>, HypervisorError> {
        let process =
            CloudHypervisorProcessHandle::spawn(socket_path, console_socket_path, log_path)?;
        Ok(Box::new(CloudHypervisorInstance::new(process)))
    }

    fn hypervisor_type(&self) -> HypervisorType {
        HypervisorType::CloudHypervisor
    }

    fn is_available(&self) -> bool {
        Command::new("cloud-hypervisor")
            .arg("--version")
            .output()
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_configs_for_tap_and_vhost_user() {
        use crate::models::NicBinding;
        use glidex_ovs::vm_port::VmPortBinding;
        let mut config: VmConfig = serde_json::from_value(serde_json::json!({
            "vcpu_count": 1, "mem_size_mib": 512, "rootfs_path": "/d", "kernel_args": ""
        }))
        .unwrap();
        config.nic_bindings = vec![
            NicBinding { id: "net0".into(), mac: "02:00:00:00:00:01".into(), binding: VmPortBinding::Tap { ifname: "gxabc-0".into() }, queue_pairs: 1, mtu: None },
            NicBinding { id: "net1".into(), mac: "02:00:00:00:00:02".into(), binding: VmPortBinding::VhostUser { socket: "/run/glidex/vhost/x.net1.sock".into() }, queue_pairs: 2, mtu: Some(9000) },
        ];
        let json = serde_json::to_value(net_configs(&config)).unwrap();
        assert_eq!(json[0], serde_json::json!({"id": "net0", "mac": "02:00:00:00:00:01", "num_queues": 2, "tap": "gxabc-0"}));
        assert_eq!(json[1], serde_json::json!({
            "id": "net1", "mac": "02:00:00:00:00:02", "num_queues": 4, "mtu": 9000,
            "vhost_user": true, "vhost_socket": "/run/glidex/vhost/x.net1.sock", "vhost_mode": "Server"
        }));
    }

    #[test]
    fn detect_image_type_from_magic() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path.to_string_lossy().into_owned()
        };

        let mut qcow2 = b"QFI\xfb\x00\x00\x00\x03".to_vec();
        qcow2.resize(4096, 0);
        assert_eq!(detect_image_type(&write("a.qcow2", &qcow2)), Some(ImageType::Qcow2));

        let mut vhdx = b"vhdxfile".to_vec();
        vhdx.resize(4096, 0);
        assert_eq!(detect_image_type(&write("a.vhdx", &vhdx)), Some(ImageType::Vhdx));

        let mut vhd = vec![0u8; 4096];
        let footer = vhd.len() - 512;
        vhd[footer..footer + 8].copy_from_slice(b"conectix");
        vhd[footer + 60..footer + 64].copy_from_slice(&2u32.to_be_bytes());
        assert_eq!(detect_image_type(&write("a.vhd", &vhd)), Some(ImageType::FixedVhd));

        assert_eq!(detect_image_type(&write("a.raw", &[0u8; 4096])), Some(ImageType::Raw));
        assert_eq!(detect_image_type(&write("tiny.raw", b"xy")), Some(ImageType::Raw));
        assert_eq!(detect_image_type("/nonexistent/disk.img"), None);

        let json = serde_json::to_value(DiskConfig {
            path: "/d".into(),
            readonly: false,
            image_type: Some(ImageType::FixedVhd),
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({"path": "/d", "image_type": "FixedVhd"}));
    }

    #[test]
    fn pty_path_prefers_whichever_device_is_pty() {
        let virtio = r#"{"config":{"console":{"mode":"Pty","file":"/dev/pts/3"},"serial":{"mode":"Off","file":null}}}"#;
        assert_eq!(pty_path_from_vm_info(virtio).as_deref(), Some("/dev/pts/3"));

        let serial = r#"{"config":{"console":{"mode":"Off","file":null},"serial":{"mode":"Pty","file":"/dev/pts/7"}}}"#;
        assert_eq!(pty_path_from_vm_info(serial).as_deref(), Some("/dev/pts/7"));

        let pending = r#"{"config":{"console":{"mode":"Pty","file":null},"serial":{"mode":"Off"}}}"#;
        assert_eq!(pty_path_from_vm_info(pending), None);
    }
}
