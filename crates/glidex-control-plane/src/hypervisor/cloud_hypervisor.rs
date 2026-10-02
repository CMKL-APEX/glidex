use super::{
    needs_shared_memory, vfio_device_id, vm_disks, Hypervisor, HypervisorError, HypervisorProcess,
    HypervisorType,
};
use crate::models::VmConfig;
use glidex_ovs::vm_port::VmPortBinding;
use serde::Serialize;
use super::console::{read_log, spawn_on_pty, start_console_proxy};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command};
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
    // CH refuses qcow2 backing files unless asked. Only set for linked
    // disks glidex created: a user-supplied qcow2 could otherwise name any
    // host file as its backing file and hand it to the guest.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    backing_files: bool,
}

/// `[root, data disks…, seed]`, see `vm_disks`.
fn disk_configs(config: &VmConfig) -> Vec<DiskConfig> {
    vm_disks(config)
        .into_iter()
        .map(|d| DiskConfig {
            path: d.path,
            readonly: d.read_only,
            image_type: d.format,
            backing_files: d.backing_files,
        })
        .collect()
}

use crate::images::qemu_img::ImageType;
#[cfg(test)]
use crate::images::qemu_img::detect_image_type;

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

/// Payload plus the `console` / `serial` modes. Firmware boot
/// (CLOUDHV.fd) hands off to the disk's own bootloader, so there is no
/// kernel/cmdline; distro cloud images put their console on ttyS0, so the
/// serial port is the console instead of hvc0. The console device is
/// `Tty`: CH's stdio, the PTY glidex owns (console.rs).
fn boot_payload(config: &VmConfig) -> (PayloadConfig, &'static str, &'static str) {
    match &config.firmware_path {
        Some(firmware) => (
            PayloadConfig {
                firmware: Some(firmware.clone()),
                kernel: None,
                cmdline: None,
            },
            "Off",
            "Tty",
        ),
        None => (
            PayloadConfig {
                firmware: None,
                kernel: Some(config.kernel_image_path.clone()),
                cmdline: Some(config.kernel_args.clone()),
            },
            "Tty",
            "Off",
        ),
    }
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
        let (payload, console_mode, serial_mode) = boot_payload(config);

        let vm_config = VmCreateConfig {
            cpus: CpuConfig {
                boot_vcpus: config.vcpu_count,
                max_vcpus: config.vcpu_count,
            },
            memory: MemoryConfig {
                size: (config.mem_size_mib as u64) * 1024 * 1024,
                shared: needs_shared_memory(config),
                hugepages: config.hugepages,
            },
            net: net_configs(config),
            payload,
            disks: disk_configs(config),
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

    pub fn power_button(&self) -> Result<(), HypervisorError> {
        self.expect_success("PUT", "/vm.power-button", None)?;
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
        let _ = std::fs::remove_file(socket_path);

        // Truncate once; the console proxy and CH's stderr then both
        // append, so neither overwrites the other.
        File::create(log_path)?;
        let log_file = OpenOptions::new().append(true).open(log_path)?;
        let stderr_log = OpenOptions::new().append(true).open(log_path)?;

        // The guest console is CH's stdio (`Tty` mode), on a PTY glidex
        // owns, proxied from the start so no early output is missed.
        let mut cmd = Command::new("cloud-hypervisor");
        cmd.arg("--api-socket").arg(socket_path);
        let (child, master) = spawn_on_pty(&mut cmd, stderr_log)?;
        let running = Arc::new(AtomicBool::new(true));
        let handle = Self {
            child: Mutex::new(Some(child)),
            socket_path: socket_path.to_string(),
            console_socket_path: console_socket_path.to_string(),
            log_path: log_path.to_string(),
            running: running.clone(),
            console_thread: Mutex::new(None),
        };
        match start_console_proxy(master, console_socket_path, log_path, log_file, running) {
            Ok(thread) => *handle.console_thread.lock().unwrap() = Some(thread),
            Err(e) => {
                handle.stop();
                return Err(e);
            }
        }

        // Wait for API socket to be available
        for _ in 0..50 {
            if std::path::Path::new(socket_path).exists() {
                return Ok(handle);
            }
            if !handle.child_alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let log = read_log(log_path);
        handle.stop();
        Err(HypervisorError::Timeout(format!(
            "cloud-hypervisor API socket not available.\n--- cloud-hypervisor output ---\n{}",
            log.trim()
        )))
    }

    /// Kill the process, then stop the console proxy (which first drains
    /// what the guest printed last) and remove the sockets.
    fn stop(&self) {
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.console_thread.lock().unwrap().take() {
            let _ = handle.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
        let _ = std::fs::remove_file(&self.console_socket_path);
    }

    /// Whether the cloud-hypervisor process hasn't exited yet.
    fn child_alive(&self) -> bool {
        match self.child.lock().unwrap().as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
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
        self.client.boot_vm()
    }

    fn pause(&self) -> Result<(), HypervisorError> {
        self.client.pause_vm()
    }

    fn resume(&self) -> Result<(), HypervisorError> {
        self.client.resume_vm()
    }

    fn kill(&self) -> Result<(), HypervisorError> {
        // Shut the VM down first (no-op if it's already gone), then kill.
        let _ = self.client.shutdown_vm();
        self.process.stop();
        Ok(())
    }

    fn add_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        self.client.add_device(device_path)
    }

    fn remove_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        self.client.remove_device(device_path)
    }

    fn request_shutdown(&self) -> Result<(), HypervisorError> {
        self.client.power_button()
    }

    fn is_running(&self) -> bool {
        // Cloud-Hypervisor exits once the guest powers off.
        self.process.running.load(Ordering::SeqCst) && self.process.child_alive()
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
            backing_files: false,
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({"path": "/d", "image_type": "FixedVhd"}));
    }

    #[test]
    fn managed_disks_use_recorded_format_and_seed_stays_last() {
        use crate::images::qemu_img::DiskFormat;
        use crate::models::DiskBinding;
        let mut config: VmConfig = serde_json::from_value(serde_json::json!({
            "vcpu_count": 1, "mem_size_mib": 512, "rootfs_path": "/disks/root.qcow2", "kernel_args": "",
            "cloud_init_path": "/tmp/seed.img"
        }))
        .unwrap();
        config.root_disk_binding = Some(DiskBinding { path: "/disks/root.qcow2".into(), format: DiskFormat::Qcow2, backing_files: true });
        config.data_disk_bindings = vec![DiskBinding { path: "/disks/data.raw".into(), format: DiskFormat::Raw, backing_files: false }];
        let json = serde_json::to_value(disk_configs(&config)).unwrap();
        assert_eq!(json, serde_json::json!([
            {"path": "/disks/root.qcow2", "image_type": "Qcow2", "backing_files": true},
            {"path": "/disks/data.raw", "image_type": "Raw"},
            {"path": "/tmp/seed.img", "readonly": true, "image_type": "Raw"},
        ]));
    }

    #[test]
    fn guest_console_is_chs_stdio() {
        let mut config: VmConfig = serde_json::from_value(serde_json::json!({
            "vcpu_count": 1, "mem_size_mib": 512, "rootfs_path": "/d",
            "kernel_image_path": "/k", "kernel_args": "console=hvc0"
        }))
        .unwrap();
        let (payload, console, serial) = boot_payload(&config);
        assert_eq!((payload.kernel.as_deref(), console, serial), (Some("/k"), "Tty", "Off"));

        config.firmware_path = Some("/fw/CLOUDHV.fd".into());
        let (payload, console, serial) = boot_payload(&config);
        assert_eq!((payload.firmware.as_deref(), payload.kernel, console, serial), (Some("/fw/CLOUDHV.fd"), None, "Off", "Tty"));
    }
}
