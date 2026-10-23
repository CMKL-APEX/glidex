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

use crate::images::catalog::Arch;
use crate::images::qemu_img::{ImageType, detect_image_type};
use crate::models::{VmConfig, VolumeBinding};
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

    /// The kernel arguments a new kernel-boot VM of `arch` gets when it
    /// names none. The same for both backends: the serial port shows the
    /// kernel from its first line (spec/kernel-boot.md K5). VMs created
    /// before this keep the arguments they were stored with.
    pub fn default_kernel_args(&self, arch: Arch) -> String {
        default_kernel_args(arch)
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

/// See `HypervisorType::default_kernel_args`: `console=ttyS0`, or
/// `console=ttyAMA0` on aarch64, then `root=/dev/vda reboot=k panic=1`.
pub fn default_kernel_args(arch: Arch) -> String {
    format!(
        "console={} root=/dev/vda reboot=k panic=1",
        arch.serial_console()
    )
}

/// x86's `COMMAND_LINE_SIZE` (2048) less the terminating NUL.
pub const MAX_KERNEL_ARGS_LEN: usize = 2047;

/// Where the guest's console is (spec/kernel-boot.md §5.2): what the
/// shim's PTY is wired to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleDevice {
    /// The emulated 16550 UART (`ttyS0`).
    Serial,
    /// The virtio console (`hvc0`).
    Virtio,
}

impl ConsoleDevice {
    /// The device a VM's console is on: always the serial port on a
    /// firmware boot (cloud images put their console there), else the one
    /// the kernel's last `console=` names, as the kernel makes the last
    /// one `/dev/console`. `hvc…` is the virtio console; any other name,
    /// or no `console=`, the serial port.
    pub fn of(config: &VmConfig) -> Self {
        if config.firmware_path.is_some() {
            return ConsoleDevice::Serial;
        }
        let last = config
            .kernel_args
            .split_whitespace()
            .rev()
            .find_map(|t| t.strip_prefix("console="));
        match last {
            Some(dev) if dev.starts_with("hvc") => ConsoleDevice::Virtio,
            _ => ConsoleDevice::Serial,
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
    fn launch_args(
        &self,
        config: &VmConfig,
        api_socket: &str,
    ) -> Result<LaunchArgs, HypervisorError>;

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
    /// On a shared datastore: QEMU's image locking must be on, so another
    /// node's QEMU can't open it (spec/storage.md §10).
    pub shared: bool,
    /// A block device (an LVM volume, spec/storage.md §11.1), raw.
    pub block: bool,
    /// A vhost-user-blk socket (a hyperstor volume, spec/storage.md
    /// §11.2): `path` is the socket the hypervisor connects to.
    pub vhost: bool,
}

/// `[root, data disks…, seed]`. Managed disks use the format recorded in
/// the database; only a user-supplied `rootfs_path` is probed. The seed
/// is always the raw FAT image `write_seed_image` builds.
pub(crate) fn vm_disks(config: &VmConfig) -> Vec<VmDisk> {
    let managed = |b: &VolumeBinding| match b {
        VolumeBinding::File {
            path,
            format,
            backing_files,
            lock,
        } => VmDisk {
            path: path.clone(),
            read_only: false,
            format: Some((*format).into()),
            backing_files: *backing_files,
            shared: lock.is_some(),
            block: false,
            vhost: false,
        },
        VolumeBinding::Block { path } => VmDisk {
            path: path.clone(),
            read_only: false,
            format: Some(ImageType::Raw),
            backing_files: false,
            shared: false,
            block: true,
            vhost: false,
        },
        VolumeBinding::VhostUserBlk { socket, .. } => VmDisk {
            path: socket.clone(),
            read_only: false,
            format: Some(ImageType::Raw),
            backing_files: false,
            shared: false,
            block: false,
            vhost: true,
        },
    };
    let root = match &config.root_disk_binding {
        Some(b) => managed(b),
        None => VmDisk {
            path: config.rootfs_path.clone(),
            read_only: false,
            format: detect_image_type(&config.rootfs_path),
            backing_files: false,
            shared: false,
            block: false,
            vhost: false,
        },
    };
    std::iter::once(root)
        .chain(config.data_disk_bindings.iter().map(managed))
        .chain(config.cloud_init_path.iter().map(|path| VmDisk {
            path: path.clone(),
            read_only: true,
            format: Some(ImageType::Raw),
            backing_files: false,
            shared: false,
            block: false,
            vhost: false,
        }))
        .collect()
}

/// MiB of `page_kib` hugepages free and not promised to a process now,
/// on the host or on one NUMA node (which has no reserved count); 0 when
/// they can't be read.
pub(crate) fn hugepages_spare_mib(page_kib: u64, numa_node: Option<u32>) -> u64 {
    let dir = match numa_node {
        Some(n) => format!("/sys/devices/system/node/node{n}/hugepages/hugepages-{page_kib}kB"),
        None => format!("/sys/kernel/mm/hugepages/hugepages-{page_kib}kB"),
    };
    let read = |f: &str| {
        std::fs::read_to_string(format!("{dir}/{f}"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    let free = read("free_hugepages").unwrap_or(0);
    let resv = read("resv_hugepages").unwrap_or(0);
    free.saturating_sub(resv) * page_kib / 1024
}

/// The host's online CPUs.
pub(crate) fn online_cpus() -> Vec<u32> {
    std::fs::read_to_string("/sys/devices/system/cpu/online")
        .map(|s| glidex_ovs::tuning::parse_cpu_list(&s))
        .unwrap_or_default()
}

/// Whether the host has enough unreserved 2 MiB hugepages to back a guest
/// of `mem_mib`, leaving 25% (at least 256 MiB) for what OVS-DPDK and
/// other guests are about to touch. For VMs placed before the scheduler
/// counted pages (spec/host-topology.md §8a.3).
pub(crate) fn hugepages_available_for(mem_mib: u32) -> bool {
    let mem = mem_mib as u64;
    mem.is_multiple_of(2) && hugepages_spare_mib(2048, None) >= mem + (mem / 4).max(256)
}

/// vhost-user: OVS, or `hyperstor-vhost` for a hyperstor disk, maps guest
/// RAM, so it must be shared.
pub(crate) fn needs_shared_memory(config: &VmConfig) -> bool {
    config
        .nic_bindings
        .iter()
        .any(|n| matches!(n.binding, VmPortBinding::VhostUser { .. }))
        || config
            .root_disk_binding
            .iter()
            .chain(config.data_disk_bindings.iter())
            .any(|b| matches!(b, VolumeBinding::VhostUserBlk { .. }))
}

/// Extract the BDF (e.g. "0000:41:00.0") from a sysfs device path.
pub(crate) fn vfio_bdf(path: &str) -> String {
    path.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Derive a deterministic hypervisor device id from a sysfs path.
/// e.g. "/sys/bus/pci/devices/0000:41:00.0" -> "vfio_0000_41_00_0". QEMU
/// takes only ids starting with a letter.
pub fn vfio_device_id(path: &str) -> String {
    let bdf = vfio_bdf(path);
    format!("vfio_{}", bdf.replace([':', '.'], "_"))
}

/// The id a Cloud Hypervisor guest launched before ids started with a
/// letter has for the device (`_vfio_…`).
pub fn legacy_vfio_device_id(path: &str) -> String {
    format!("_{}", vfio_device_id(path))
}

/// A device id a guest reports, as [`vfio_device_id`] makes it; `None` for
/// anything but a VFIO device.
pub fn observed_vfio_id(id: &str) -> Option<String> {
    let id = id.strip_prefix('_').unwrap_or(id);
    id.starts_with("vfio_").then(|| id.to_string())
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

/// Admission rule for `kernel_args` (spec/kernel-boot.md K7): it is one
/// argv element, so only its length and control characters matter.
pub fn check_kernel_args(args: &str) -> Result<(), HypervisorError> {
    if args.len() > MAX_KERNEL_ARGS_LEN {
        return Err(HypervisorError::InvalidConfig(format!(
            "kernel_args must be at most {} bytes",
            MAX_KERNEL_ARGS_LEN
        )));
    }
    if args.chars().any(|c| c.is_control()) {
        return Err(HypervisorError::InvalidConfig(
            "kernel_args must not contain control characters".into(),
        ));
    }
    Ok(())
}

/// An `aligned` VM's guest NUMA nodes (spec/gpu-vms.md §5): for each share
/// of `config.host_numa`, its vCPU range (first, last) and the VFIO devices
/// (as `config.vfio_devices` names them) on that host node.
pub(crate) fn guest_numa_layout(config: &VmConfig) -> Vec<(u32, u32, Vec<String>)> {
    let mut first = 0u32;
    config
        .host_numa
        .iter()
        .map(|s| {
            let last = first + s.vcpus.max(1) - 1;
            let devices = config
                .vfio_devices
                .iter()
                .filter(|p| {
                    s.devices
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(&vfio_bdf(p)))
                })
                .cloned()
                .collect();
            let r = (first, last, devices);
            first = last + 1;
            r
        })
        .collect()
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
        assert_eq!(
            vfio_bdf("/sys/bus/pci/devices/0000:41:00.0/"),
            "0000:41:00.0"
        );
        assert_eq!(
            vfio_device_id("/sys/bus/pci/devices/0000:41:00.0"),
            "vfio_0000_41_00_0"
        );
        assert_eq!(vfio_device_id("0000:41:00.0"), "vfio_0000_41_00_0");
        assert_eq!(
            observed_vfio_id("_vfio_0000_41_00_0").as_deref(),
            Some("vfio_0000_41_00_0"),
            "a device of a guest launched before"
        );
        assert_eq!(observed_vfio_id("vd0"), None);
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

    fn kernel_config(args: &str, firmware: Option<&str>) -> VmConfig {
        serde_json::from_value(serde_json::json!({
            "vcpu_count": 1, "mem_size_mib": 512, "rootfs_path": "/r",
            "kernel_image_path": "/k", "kernel_args": args, "firmware_path": firmware,
        }))
        .unwrap()
    }

    #[test]
    fn console_device_follows_the_last_console() {
        use ConsoleDevice::*;
        for (args, want) in [
            ("console=ttyS0 root=/dev/vda", Serial),
            ("console=hvc0 root=/dev/vda", Virtio),
            ("console=ttyS0 console=hvc0", Virtio),
            ("console=hvc0 console=ttyS0,115200n8", Serial),
            ("console=hvc1", Virtio),
            ("root=/dev/vda", Serial),
            ("", Serial),
            // Only a whole `console=` token counts.
            ("xconsole=hvc0", Serial),
        ] {
            assert_eq!(
                ConsoleDevice::of(&kernel_config(args, None)),
                want,
                "{args}"
            );
        }
        // Firmware boot: the serial port, whatever the arguments say.
        assert_eq!(
            ConsoleDevice::of(&kernel_config("console=hvc0", Some("/fw.fd"))),
            Serial
        );
    }

    #[test]
    fn kernel_args_limits() {
        assert!(check_kernel_args(&default_kernel_args(Arch::X86_64)).is_ok());
        assert!(check_kernel_args(&default_kernel_args(Arch::Aarch64)).is_ok());
        assert!(check_kernel_args(&"a".repeat(MAX_KERNEL_ARGS_LEN)).is_ok());
        assert!(check_kernel_args(&"a".repeat(MAX_KERNEL_ARGS_LEN + 1)).is_err());
        assert!(check_kernel_args("root=/dev/vda init=/bin/sh").is_ok());
        assert!(check_kernel_args("root=/dev/vda\ninit=/bin/sh").is_err());
        assert!(check_kernel_args("a\tb").is_err());
    }

    #[test]
    fn default_kernel_args_are_shared_and_per_architecture() {
        assert_eq!(
            HypervisorType::CloudHypervisor.default_kernel_args(Arch::X86_64),
            HypervisorType::Qemu.default_kernel_args(Arch::X86_64)
        );
        assert_eq!(
            default_kernel_args(Arch::X86_64),
            "console=ttyS0 root=/dev/vda reboot=k panic=1"
        );
        // aarch64's serial port is the PL011.
        assert_eq!(
            default_kernel_args(Arch::Aarch64),
            "console=ttyAMA0 root=/dev/vda reboot=k panic=1"
        );
    }

    #[test]
    fn command_line_paths() {
        assert!(check_command_line_path("rootfs_path", "/a,b/c d.img").is_ok());
        assert!(check_command_line_path("rootfs_path", "/a\"b").is_err());
        assert!(check_command_line_path("rootfs_path", "/a\nb").is_err());
    }
}
