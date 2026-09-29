use crate::hypervisor::HypervisorType;
use glidex_ovs::vm_port::VmPortBinding;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use uuid::Uuid;

/// Expand a leading `~` or `~/` to the user's home directory. Hypervisors
/// don't do shell-style expansion themselves, so paths like
/// `~/.glidex/rootfs.ext4` need to be resolved before being passed to
/// qemu-system-x86_64 / cloud-hypervisor.
fn expand_tilde(path: String) -> String {
    if path == "~" {
        return dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or(path);
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    path
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VmState {
    Created,
    Running,
    Paused,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    #[serde(default)]
    pub kernel_image_path: String,
    /// UEFI firmware (e.g. Cloud Hypervisor's `CLOUDHV.fd`). When set, the
    /// guest boots from its disk's bootloader and `kernel_image_path` /
    /// `kernel_args` are ignored. Only Cloud Hypervisor supports this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware_path: Option<String>,
    /// cloud-init NoCloud seed disk attached alongside the rootfs. For
    /// firmware boots without one, a default seed is generated at start
    /// time (see `cloud_init.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_init_path: Option<String>,
    /// Username of a stored credential (`credentials.rs`) that the
    /// generated cloud-init seed provisions as the guest login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    pub rootfs_path: String,
    pub kernel_args: String,
    #[serde(default)]
    pub hypervisor: HypervisorType,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vfio_devices: Vec<String>,
    /// Networks the VM's NICs attach to, in NIC order (spec §11.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<NetworkAttachment>,
    /// Back guest RAM with hugepages (implies shared memory).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hugepages: bool,
    /// Host-side NIC bindings from glidex-netd, filled in by `start_vm`
    /// for the hypervisor; never persisted.
    #[serde(skip)]
    pub nic_bindings: Vec<NicBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkAttachment {
    pub network: String,
    /// Filled in at create time (stable, derived from the VM id) if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_pairs: Option<u8>,
}

/// What the hypervisor needs for one NIC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NicBinding {
    pub id: String,
    pub mac: String,
    pub binding: VmPortBinding,
    pub queue_pairs: u8,
    pub mtu: Option<u16>,
}

/// Last known state of a VM NIC, shown in `VmResponse`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NicState {
    pub network: String,
    pub mac: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<Ipv4Addr>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vm {
    pub id: String,
    pub name: String,
    pub state: VmState,
    pub config: VmConfig,
    pub socket_path: String,
    pub console_socket_path: String,
    pub log_path: String,
    pub hypervisor: HypervisorType,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nics: Vec<NicState>,
}

impl Vm {
    pub fn new(name: String, config: VmConfig) -> Self {
        let id = Uuid::new_v4().to_string();
        let hypervisor = config.hypervisor;
        let prefix = hypervisor.socket_prefix();
        let socket_path = format!("/tmp/{}-{}.sock", prefix, id);
        let console_socket_path = format!("/tmp/{}-{}.console.sock", prefix, id);
        let log_path = format!("/tmp/{}-{}.log", prefix, id);
        Self {
            id,
            name,
            state: VmState::Created,
            config,
            socket_path,
            console_socket_path,
            log_path,
            hypervisor,
            nics: Vec::new(),
        }
    }

    /// Where the auto-generated cloud-init seed for this VM lives.
    pub fn default_cloud_init_path(&self) -> String {
        format!(
            "/tmp/{}-{}.cloudinit.img",
            self.hypervisor.socket_prefix(),
            self.id
        )
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateVmRequest {
    pub name: String,
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    #[serde(default)]
    pub kernel_image_path: String,
    #[serde(default)]
    pub firmware_path: Option<String>,
    #[serde(default)]
    pub cloud_init_path: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
    pub rootfs_path: String,
    #[serde(default)]
    pub kernel_args: Option<String>,
    #[serde(default)]
    pub hypervisor: Option<HypervisorType>,
    #[serde(default)]
    pub vfio_devices: Option<Vec<String>>,
    #[serde(default)]
    pub networks: Option<Vec<NetworkAttachment>>,
    #[serde(default)]
    pub hugepages: bool,
}

impl From<CreateVmRequest> for VmConfig {
    fn from(req: CreateVmRequest) -> Self {
        let hypervisor = req.hypervisor.unwrap_or_default();
        VmConfig {
            vcpu_count: req.vcpu_count,
            mem_size_mib: req.mem_size_mib,
            kernel_image_path: expand_tilde(req.kernel_image_path),
            firmware_path: req.firmware_path.map(expand_tilde),
            cloud_init_path: req.cloud_init_path.map(expand_tilde),
            credential: req.credential,
            rootfs_path: expand_tilde(req.rootfs_path),
            kernel_args: req
                .kernel_args
                .unwrap_or_else(|| hypervisor.default_kernel_args().to_string()),
            hypervisor,
            vfio_devices: req.vfio_devices.unwrap_or_default(),
            networks: req.networks.unwrap_or_default(),
            hugepages: req.hugepages,
            nic_bindings: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct VmResponse {
    pub id: String,
    pub name: String,
    pub state: VmState,
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    pub console_socket_path: String,
    pub log_path: String,
    pub hypervisor: HypervisorType,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vfio_devices: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nics: Vec<NicState>,
}

impl From<&Vm> for VmResponse {
    fn from(vm: &Vm) -> Self {
        VmResponse {
            id: vm.id.clone(),
            name: vm.name.clone(),
            state: vm.state.clone(),
            vcpu_count: vm.config.vcpu_count,
            mem_size_mib: vm.config.mem_size_mib,
            console_socket_path: vm.console_socket_path.clone(),
            log_path: vm.log_path.clone(),
            hypervisor: vm.hypervisor,
            vfio_devices: vm.config.vfio_devices.clone(),
            credential: vm.config.credential.clone(),
            nics: if vm.nics.is_empty() {
                vm.config
                    .networks
                    .iter()
                    .map(|a| NicState {
                        network: a.network.clone(),
                        mac: a.mac.clone().unwrap_or_default(),
                        port: None,
                        ipv4: None,
                    })
                    .collect()
            } else {
                vm.nics.clone()
            },
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DeviceRequest {
    pub device_path: String,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl ApiError {
    pub fn new(error: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            message: message.into(),
            details: serde_json::Value::Null,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}
