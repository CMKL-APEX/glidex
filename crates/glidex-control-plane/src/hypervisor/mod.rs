pub mod cloud_hypervisor;
mod console;
pub mod qemu;

use crate::images::qemu_img::{detect_image_type, ImageType};
use crate::models::VmConfig;
use glidex_ovs::vm_port::VmPortBinding;
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

/// Supported hypervisor types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HypervisorType {
    #[default]
    CloudHypervisor,
    Qemu,
}

impl HypervisorType {
    /// Get the binary name for this hypervisor
    pub fn binary_name(&self) -> &'static str {
        match self {
            HypervisorType::CloudHypervisor => "cloud-hypervisor",
            HypervisorType::Qemu => "qemu-system-x86_64",
        }
    }

    /// Get the socket path prefix for this hypervisor
    pub fn socket_prefix(&self) -> &'static str {
        match self {
            HypervisorType::CloudHypervisor => "cloud-hypervisor",
            HypervisorType::Qemu => "qemu",
        }
    }

    /// Get the default kernel boot arguments for this hypervisor
    pub fn default_kernel_args(&self) -> &'static str {
        match self {
            HypervisorType::CloudHypervisor => "console=hvc0 root=/dev/vda reboot=k panic=1",
            HypervisorType::Qemu => "console=ttyS0 root=/dev/vda reboot=k panic=1",
        }
    }

    /// The UEFI firmware a VM of this type boots from when the caller
    /// doesn't name one: Cloud-Hypervisor's `CLOUDHV.fd`, or the host's
    /// OVMF build for QEMU. The path may not exist.
    pub fn default_firmware_path(&self) -> Option<std::path::PathBuf> {
        match self {
            HypervisorType::CloudHypervisor => cloud_hypervisor::default_firmware_path(),
            HypervisorType::Qemu => qemu::default_firmware_path(),
        }
    }
}

impl fmt::Display for HypervisorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HypervisorType::CloudHypervisor => write!(f, "cloudhypervisor"),
            HypervisorType::Qemu => write!(f, "qemu"),
        }
    }
}

/// Errors that can occur during hypervisor operations
#[derive(Error, Debug)]
pub enum HypervisorError {
    #[error("Failed to start hypervisor process: {0}")]
    ProcessStart(#[from] std::io::Error),

    #[error("Failed to connect to hypervisor socket: {0}")]
    SocketConnection(String),

    #[error("API request failed: {0}")]
    ApiRequest(String),

    #[error("Operation not supported by this hypervisor: {0}")]
    Unsupported(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Timeout waiting for hypervisor: {0}")]
    Timeout(String),

    #[error("Failed to build cloud-init seed: {0}")]
    CloudInit(String),
}

/// Trait for hypervisor backends that can spawn VM processes
pub trait Hypervisor: Send + Sync {
    /// Spawn a new hypervisor process
    fn spawn(
        &self,
        socket_path: &str,
        console_socket_path: &str,
        log_path: &str,
    ) -> Result<Box<dyn HypervisorProcess>, HypervisorError>;

    /// Get the hypervisor type
    fn hypervisor_type(&self) -> HypervisorType;

    /// Check if the hypervisor binary is available on the system
    fn is_available(&self) -> bool;
}

/// Trait for a running hypervisor process instance
pub trait HypervisorProcess: Send + Sync {
    /// Configure the VM with the given configuration
    fn configure(&self, config: &VmConfig) -> Result<(), HypervisorError>;

    /// Start/boot the VM instance
    fn start(&self) -> Result<(), HypervisorError>;

    /// Pause the VM
    fn pause(&self) -> Result<(), HypervisorError>;

    /// Resume a paused VM
    fn resume(&self) -> Result<(), HypervisorError>;

    /// Kill the hypervisor process
    fn kill(&self) -> Result<(), HypervisorError>;

    /// Press the guest's ACPI power button. Returns once the request is
    /// sent; the hypervisor exits when the guest has shut down, which
    /// `is_running` then reports.
    fn request_shutdown(&self) -> Result<(), HypervisorError> {
        Err(HypervisorError::Unsupported(
            "request_shutdown not supported by this hypervisor".to_string(),
        ))
    }

    /// Hot-add a VFIO device to a running VM
    fn add_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        Err(HypervisorError::Unsupported(format!(
            "add_device not supported by this hypervisor (device: {})",
            device_path
        )))
    }

    /// Hot-remove a VFIO device from a running VM
    fn remove_device(&self, device_path: &str) -> Result<(), HypervisorError> {
        Err(HypervisorError::Unsupported(format!(
            "remove_device not supported by this hypervisor (device: {})",
            device_path
        )))
    }

    /// Whether the hypervisor process is still alive: false after `kill`,
    /// and once the guest has powered off.
    fn is_running(&self) -> bool;

    /// Get the API socket path
    fn socket_path(&self) -> &str;

    /// Get the console socket path
    fn console_socket_path(&self) -> &str;

    /// Get the log file path
    fn log_path(&self) -> &str;
}

/// One disk as a backend attaches it, in guest order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VmDisk {
    pub path: String,
    pub read_only: bool,
    /// `None`: a user-supplied image that couldn't be read to probe.
    pub format: Option<ImageType>,
    /// Let the image open its qcow2 backing file. Only set for linked
    /// disks glidex created: a user-supplied qcow2 could otherwise name any
    /// host file as its backing file and hand it to the guest.
    pub backing_files: bool,
}

/// `[root, data disks…, seed]`. Managed disks use the format recorded in
/// the database; only a user-supplied `rootfs_path` is probed. The seed
/// is always the raw FAT image `write_seed_image` builds.
pub(crate) fn vm_disks(config: &VmConfig) -> Vec<VmDisk> {
    let root = match &config.root_disk_binding {
        Some(b) => VmDisk {
            path: b.path.clone(),
            read_only: false,
            format: Some(b.format.into()),
            backing_files: b.backing_files,
        },
        None => VmDisk {
            path: config.rootfs_path.clone(),
            read_only: false,
            format: detect_image_type(&config.rootfs_path),
            backing_files: false,
        },
    };
    std::iter::once(root)
        .chain(config.data_disk_bindings.iter().map(|b| VmDisk {
            path: b.path.clone(),
            read_only: false,
            format: Some(b.format.into()),
            backing_files: b.backing_files,
        }))
        .chain(config.cloud_init_path.iter().map(|path| VmDisk {
            path: path.clone(),
            read_only: true,
            format: Some(ImageType::Raw),
            backing_files: false,
        }))
        .collect()
}

/// vhost-user: OVS maps guest RAM, so it must be shared.
pub(crate) fn needs_shared_memory(config: &VmConfig) -> bool {
    config
        .nic_bindings
        .iter()
        .any(|n| matches!(n.binding, VmPortBinding::VhostUser { .. }))
}

/// Extract the BDF (e.g. "0000:41:00.0") from a sysfs device path.
pub(crate) fn vfio_bdf(path: &str) -> String {
    path.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Derive a deterministic hypervisor device id from a sysfs path.
/// e.g. "/sys/bus/pci/devices/0000:41:00.0" -> "_vfio_0000_41_00_0"
pub(crate) fn vfio_device_id(path: &str) -> String {
    let bdf = vfio_bdf(path);
    format!("_vfio_{}", bdf.replace([':', '.'], "_"))
}

/// Create a hypervisor backend for the given type
pub fn create_backend(hypervisor_type: HypervisorType) -> Box<dyn Hypervisor> {
    match hypervisor_type {
        HypervisorType::CloudHypervisor => Box::new(cloud_hypervisor::CloudHypervisorBackend),
        HypervisorType::Qemu => Box::new(qemu::QemuBackend),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_name_matches_hypervisor_type() {
        assert_eq!(
            HypervisorType::CloudHypervisor.binary_name(),
            "cloud-hypervisor"
        );
        assert_eq!(HypervisorType::Qemu.binary_name(), "qemu-system-x86_64");
    }

    #[test]
    fn vfio_ids_ignore_trailing_slashes() {
        assert_eq!(vfio_bdf("/sys/bus/pci/devices/0000:41:00.0/"), "0000:41:00.0");
        assert_eq!(vfio_device_id("/sys/bus/pci/devices/0000:41:00.0"), "_vfio_0000_41_00_0");
        assert_eq!(vfio_device_id("0000:41:00.0"), "_vfio_0000_41_00_0");
    }

    #[test]
    fn invalid_config_error_renders_message() {
        let err = HypervisorError::InvalidConfig("bad vcpu count".to_string());
        assert_eq!(
            err.to_string(),
            "Invalid configuration: bad vcpu count"
        );
    }

    #[test]
    fn backends_report_their_hypervisor_type() {
        for ty in [
            HypervisorType::CloudHypervisor,
            HypervisorType::Qemu,
        ] {
            let backend = create_backend(ty);
            assert_eq!(backend.hypervisor_type(), ty);
            // Just exercise is_available — return value depends on host.
            let _ = backend.is_available();
        }
    }

    /// Minimal in-memory HypervisorProcess used to exercise trait accessors
    /// without actually spawning a hypervisor.
    struct StubProcess {
        socket_path: String,
        console_socket_path: String,
        log_path: String,
        running: std::sync::atomic::AtomicBool,
    }

    impl HypervisorProcess for StubProcess {
        fn configure(&self, _config: &VmConfig) -> Result<(), HypervisorError> {
            Ok(())
        }
        fn start(&self) -> Result<(), HypervisorError> {
            Ok(())
        }
        fn pause(&self) -> Result<(), HypervisorError> {
            Ok(())
        }
        fn resume(&self) -> Result<(), HypervisorError> {
            Ok(())
        }
        fn kill(&self) -> Result<(), HypervisorError> {
            self.running
                .store(false, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn is_running(&self) -> bool {
            self.running.load(std::sync::atomic::Ordering::SeqCst)
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

    #[test]
    fn hypervisor_process_accessors_round_trip() {
        let proc: Box<dyn HypervisorProcess> = Box::new(StubProcess {
            socket_path: "/tmp/sock".to_string(),
            console_socket_path: "/tmp/console".to_string(),
            log_path: "/tmp/log".to_string(),
            running: std::sync::atomic::AtomicBool::new(true),
        });

        assert_eq!(proc.socket_path(), "/tmp/sock");
        assert_eq!(proc.console_socket_path(), "/tmp/console");
        assert_eq!(proc.log_path(), "/tmp/log");
        assert!(proc.is_running());

        // Default add_device / remove_device should report Unsupported.
        assert!(matches!(
            proc.add_device("0000:00:1f.0"),
            Err(HypervisorError::Unsupported(_))
        ));
        assert!(matches!(
            proc.remove_device("0000:00:1f.0"),
            Err(HypervisorError::Unsupported(_))
        ));

        proc.kill().unwrap();
        assert!(!proc.is_running());
    }
}
