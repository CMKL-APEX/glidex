//! Hypervisor drivers (spec/reconciliation.md §16).
//!
//! A driver is stateless: it turns a VM's config into the command line the
//! shim runs (`launch_args`, which carries the whole config, D9), and talks
//! to a running instance through its API socket for the runtime operations
//! (`observe`, pause/resume, VFIO hot-plug). Everything it needs is a path,
//! so a restarted control plane manages instances exactly as before.
//! Process lifetime belongs to `glidex-vm-shim`.

pub mod cloud_hypervisor;
pub mod qemu;

use crate::images::qemu_img::{detect_image_type, ImageType};
use crate::models::VmConfig;
use glidex_hv_client::{HvError, Observed};
use glidex_ovs::vm_port::VmPortBinding;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
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

    /// The shim's name for it (`launch.json`).
    pub fn launch_kind(&self) -> glidex_vm_shim::HypervisorKind {
        match self {
            HypervisorType::CloudHypervisor => glidex_vm_shim::HypervisorKind::CloudHypervisor,
            HypervisorType::Qemu => glidex_vm_shim::HypervisorKind::Qemu,
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

impl From<HvError> for HypervisorError {
    fn from(e: HvError) -> Self {
        match e {
            HvError::Connect(m) => HypervisorError::SocketConnection(m),
            HvError::Api(m) => HypervisorError::ApiRequest(m),
            HvError::Io(e) => HypervisorError::ProcessStart(e),
        }
    }
}

/// The command line the shim runs, and QEMU's CPU-model fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchArgs {
    pub argv: Vec<String>,
    pub fallback: Option<glidex_vm_shim::Fallback>,
}

/// One hypervisor backend (spec §16).
pub trait HypervisorDriver: Send + Sync {
    fn hypervisor_type(&self) -> HypervisorType;

    /// Whether the binary is installed.
    fn is_available(&self) -> bool {
        resolve_binary(self.hypervisor_type().binary_name()).is_some()
    }

    /// The command line carrying the whole VM config. `config` has its
    /// non-persisted bindings (disks, NICs, seed, firmware vars) filled in.
    /// Pure apart from host probes (firmware vars, `O_DIRECT`, vhost-net).
    fn launch_args(&self, config: &VmConfig, api_socket: &str) -> Result<LaunchArgs, HypervisorError>;

    fn observe(&self, api_socket: &str) -> Result<Observed, HypervisorError>;
    fn pause(&self, api_socket: &str) -> Result<(), HypervisorError>;
    fn resume(&self, api_socket: &str) -> Result<(), HypervisorError>;
    /// Hot-plug a VFIO device under its deterministic id.
    fn add_device(&self, api_socket: &str, device_path: &str) -> Result<(), HypervisorError>;
    fn remove_device(&self, api_socket: &str, device_path: &str) -> Result<(), HypervisorError>;
}

pub fn driver(ty: HypervisorType) -> &'static dyn HypervisorDriver {
    match ty {
        HypervisorType::CloudHypervisor => &cloud_hypervisor::CloudHypervisorDriver,
        HypervisorType::Qemu => &qemu::QemuDriver,
    }
}

/// The absolute path of a hypervisor binary: `PATH`, then `/usr/local/bin`
/// and `/usr/bin` (the shim's allowlist is checked on canonical paths).
pub fn resolve_binary(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/local/bin", "/usr/bin"].map(PathBuf::from))
        .map(|d| d.join(name))
        .find(|p| p.is_file())
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
pub fn vfio_device_id(path: &str) -> String {
    let bdf = vfio_bdf(path);
    format!("_vfio_{}", bdf.replace([':', '.'], "_"))
}

/// Characters a path must not contain to reach a hypervisor command line
/// (spec §8.2): `"` (CH's quoting), newlines and other control characters.
pub fn check_command_line_path(field: &str, path: &str) -> Result<(), HypervisorError> {
    if path.chars().any(|c| c == '"' || c.is_control()) {
        return Err(HypervisorError::InvalidConfig(format!(
            "{} must not contain double quotes or control characters",
            field
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_name_matches_hypervisor_type() {
        assert_eq!(HypervisorType::CloudHypervisor.binary_name(), "cloud-hypervisor");
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
        assert_eq!(err.to_string(), "Invalid configuration: bad vcpu count");
    }

    #[test]
    fn drivers_report_their_hypervisor_type() {
        for ty in [HypervisorType::CloudHypervisor, HypervisorType::Qemu] {
            assert_eq!(driver(ty).hypervisor_type(), ty);
            let _ = driver(ty).is_available();
        }
    }

    #[test]
    fn command_line_paths() {
        assert!(check_command_line_path("rootfs_path", "/a,b/c d.img").is_ok());
        assert!(check_command_line_path("rootfs_path", "/a\"b").is_err());
        assert!(check_command_line_path("rootfs_path", "/a\nb").is_err());
    }
}
