pub mod cloud_hypervisor;
pub mod firecracker;
pub mod qemu;

use crate::models::VmConfig;
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

/// Supported hypervisor types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HypervisorType {
    Firecracker,
    #[default]
    CloudHypervisor,
    Qemu,
}

impl HypervisorType {
    /// Get the binary name for this hypervisor
    pub fn binary_name(&self) -> &'static str {
        match self {
            HypervisorType::Firecracker => "firecracker",
            HypervisorType::CloudHypervisor => "cloud-hypervisor",
            HypervisorType::Qemu => "qemu-system-x86_64",
        }
    }

    /// Get the socket path prefix for this hypervisor
    pub fn socket_prefix(&self) -> &'static str {
        match self {
            HypervisorType::Firecracker => "firecracker",
            HypervisorType::CloudHypervisor => "cloud-hypervisor",
            HypervisorType::Qemu => "qemu",
        }
    }

    /// Get the default kernel boot arguments for this hypervisor
    pub fn default_kernel_args(&self) -> &'static str {
        match self {
            HypervisorType::Firecracker => "console=ttyS0 reboot=k panic=1 pci=off",
            HypervisorType::CloudHypervisor => "console=hvc0 root=/dev/vda reboot=k panic=1",
            HypervisorType::Qemu => "console=ttyS0 root=/dev/vda reboot=k panic=1",
        }
    }
}

impl fmt::Display for HypervisorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HypervisorType::Firecracker => write!(f, "firecracker"),
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

    /// Check if the process is still running
    fn is_running(&self) -> bool;

    /// Get the API socket path
    fn socket_path(&self) -> &str;

    /// Get the console socket path
    fn console_socket_path(&self) -> &str;

    /// Get the log file path
    fn log_path(&self) -> &str;
}

/// Create a hypervisor backend for the given type
pub fn create_backend(hypervisor_type: HypervisorType) -> Box<dyn Hypervisor> {
    match hypervisor_type {
        HypervisorType::Firecracker => Box::new(firecracker::FirecrackerBackend),
        HypervisorType::CloudHypervisor => Box::new(cloud_hypervisor::CloudHypervisorBackend),
        HypervisorType::Qemu => Box::new(qemu::QemuBackend),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_name_matches_hypervisor_type() {
        assert_eq!(HypervisorType::Firecracker.binary_name(), "firecracker");
        assert_eq!(
            HypervisorType::CloudHypervisor.binary_name(),
            "cloud-hypervisor"
        );
        assert_eq!(HypervisorType::Qemu.binary_name(), "qemu-system-x86_64");
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
            HypervisorType::Firecracker,
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
