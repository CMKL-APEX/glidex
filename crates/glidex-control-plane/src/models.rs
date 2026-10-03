use crate::hypervisor::HypervisorType;
use crate::images::qemu_img::DiskFormat;
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

/// A VM's state as the API shows it, derived from the observed
/// [`VmPhase`] (spec/reconciliation.md §7.4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VmState {
    Created,
    Starting,
    Running,
    Paused,
    Stopping,
    Stopped,
    Failed,
    Unknown,
}

impl std::fmt::Display for VmState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        f.write_str(&s)
    }
}

/// What the user wants the VM to be doing (`spec.power`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PowerState {
    Running,
    Paused,
    #[default]
    Stopped,
}

/// What to do when the hypervisor dies without being asked to (§7.5).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RestartPolicy {
    #[default]
    OnFailure,
    Never,
}

/// What to do with a VM that should be running after a host reboot (D10).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum HostBootPolicy {
    #[default]
    Resume,
    Stop,
}

/// What the VM controller last observed (§7.2).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum VmPhase {
    #[default]
    Stopped,
    Provisioning,
    Starting,
    Running,
    Paused,
    Stopping,
    Failed,
    Unknown,
}

/// The longest grace a stop may give the guest (§7.1).
pub const MAX_STOP_GRACE_SECS: u32 = 300;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmConfig {
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    #[serde(default)]
    pub kernel_image_path: String,
    /// UEFI firmware: Cloud Hypervisor's `CLOUDHV.fd`, or an OVMF code
    /// image (`OVMF_CODE*.fd`) for QEMU. When set, the guest boots from its
    /// disk's bootloader and `kernel_image_path` / `kernel_args` are
    /// ignored.
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
    /// Path of the boot disk. For a managed root disk (`root_disk`) this
    /// is that disk's file, so backends see one field either way.
    pub rootfs_path: String,
    pub kernel_args: String,
    #[serde(default)]
    pub hypervisor: HypervisorType,
    /// Managed disk (id) the VM boots from; see spec/images.md §7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_disk: Option<String>,
    /// Managed data disks (ids), attached in order after the root disk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_disks: Vec<String>,
    /// The root disk was created for this VM and is deleted with it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owns_root_disk: bool,
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
    /// Format etc. of a managed root disk, filled in by `start_vm`; never
    /// persisted. `None`: user-supplied `rootfs_path`, format probed.
    #[serde(skip)]
    pub root_disk_binding: Option<DiskBinding>,
    /// Managed data disks, filled in by `start_vm`; never persisted.
    #[serde(skip)]
    pub data_disk_bindings: Vec<DiskBinding>,
    /// This VM's own copy of the OVMF variable store (QEMU firmware boot),
    /// filled in by `start_vm`; never persisted.
    #[serde(skip)]
    pub firmware_vars_path: Option<String>,
}

/// What a hypervisor needs to open one managed disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskBinding {
    pub path: String,
    pub format: DiskFormat,
    /// qcow2 overlay on an image: the backend must allow backing files.
    /// Only ever set for disks glidex created.
    pub backing_files: bool,
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

/// The VM resource (spec/reconciliation.md §6.1, §7): what the user asked
/// for (`spec`) and what the VM controller last saw (`status`). Stored as
/// the `{meta, spec, status}` envelope (`store::VmRecord`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(into = "crate::store::VmRecord", from = "crate::store::VmRecord")]
pub struct Vm {
    pub id: String,
    pub name: String,
    /// Owning project id (spec/security.md §6).
    pub project: String,
    pub created_at: u64,
    pub generation: u64,
    pub resource_version: u64,
    pub deletion_requested_at: Option<u64>,
    pub finalizers: Vec<String>,
    pub spec: VmSpec,
    pub status: VmStatus,
}

/// `spec`: the only thing the API writes (D3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmSpec {
    pub config: VmConfig,
    #[serde(default)]
    pub power: PowerState,
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    #[serde(default)]
    pub on_host_boot: HostBootPolicy,
    /// Power-button wait when glidex stops the VM; 0 = stop hard.
    #[serde(default)]
    pub stop_grace_secs: u32,
}

/// `status`: written only by the VM controller.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VmStatus {
    #[serde(default)]
    pub observed_generation: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub last_reconciled_at: u64,
    #[serde(default)]
    pub phase: VmPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<InstanceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<ExitRecord>,
    #[serde(default)]
    pub restart_count: u32,
    /// Consecutive failed launches (provisioning errors, `LaunchFailed`):
    /// backs off like a crash loop but is not one (§7.5).
    #[serde(default)]
    pub launch_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_deadline: Option<u64>,
    /// The ports this VM owns in netd: written before `attach_vm_port`,
    /// removed after `detach_vm_port` (§9.1, D16).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nics: Vec<NicStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_growpart_seq: Option<u64>,
    #[serde(default)]
    pub never_started: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tristate {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condition {
    pub kind: String,
    pub status: Tristate,
    pub reason: String,
    #[serde(default)]
    pub message: String,
    pub last_transition_at: u64,
}

/// Where the instance runs (§8.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Runner {
    Systemd { unit: String },
    Detached,
}

/// Everything needed to find and verify a running instance (D7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceRef {
    pub instance_id: String,
    pub runner: Runner,
    pub boot_id: String,
    pub launched_generation: u64,
    /// Disk ids the instance has open: they stay claimed until it is gone.
    #[serde(default)]
    pub disks: Vec<String>,
    /// Host PCI devices it holds.
    #[serde(default)]
    pub vfio_devices: Vec<String>,
    #[serde(default)]
    pub shim_pid: Option<u32>,
    #[serde(default)]
    pub shim_starttime: Option<u64>,
    #[serde(default)]
    pub hypervisor_pid: Option<u32>,
    #[serde(default)]
    pub hypervisor_starttime: Option<u64>,
    pub launched_at: u64,
    /// The root disk whose `pending_growpart` this launch's seed carries;
    /// cleared once the guest is seen running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub growpart_disk: Option<String>,
    /// That disk's `applied_extend_root_seq` when the seed was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub growpart_seq: Option<u64>,
}

pub use glidex_vm_shim::state::ExitCause;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    pub at: u64,
    pub instance_id: String,
    pub cause: ExitCause,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NicStatus {
    pub network: String,
    pub nic_index: u8,
    pub mac: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<Ipv4Addr>,
    #[serde(default)]
    pub port_ok: bool,
}

impl Vm {
    pub fn new(name: String, config: VmConfig) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            project: String::new(),
            created_at: crate::tenancy::now(),
            generation: 1,
            resource_version: 1,
            deletion_requested_at: None,
            finalizers: Vec::new(),
            spec: VmSpec {
                config,
                power: PowerState::Stopped,
                restart_policy: RestartPolicy::OnFailure,
                on_host_boot: HostBootPolicy::Resume,
                stop_grace_secs: 0,
            },
            status: VmStatus { never_started: true, ..Default::default() },
        }
    }

    pub fn config(&self) -> &VmConfig {
        &self.spec.config
    }

    pub fn hypervisor(&self) -> HypervisorType {
        self.spec.config.hypervisor
    }

    pub fn paths(&self) -> crate::paths::VmPaths {
        crate::paths::vm_paths(&self.id)
    }

    /// Where the auto-generated cloud-init seed for this VM lives.
    pub fn default_cloud_init_path(&self) -> String {
        self.paths().cloud_init
    }

    /// The state the API shows (§7.4).
    pub fn state(&self) -> VmState {
        match self.status.phase {
            VmPhase::Stopped if self.status.never_started => VmState::Created,
            VmPhase::Stopped => VmState::Stopped,
            VmPhase::Provisioning | VmPhase::Starting => VmState::Starting,
            VmPhase::Running => VmState::Running,
            VmPhase::Paused => VmState::Paused,
            VmPhase::Stopping => VmState::Stopping,
            VmPhase::Failed => VmState::Failed,
            VmPhase::Unknown => VmState::Unknown,
        }
    }

    pub fn condition(&self, kind: &str) -> Option<&Condition> {
        self.status.conditions.iter().find(|c| c.kind == kind)
    }

    /// Converged (D13): the reconcile of the current generation is done
    /// and nothing blocks the spec.
    pub fn is_converged(&self) -> bool {
        self.status.observed_generation >= self.generation
            && self.condition("Ready").is_some_and(|c| c.status == Tristate::True)
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateVmRequest {
    pub name: String,
    /// Project id or name; default: the caller's default project.
    #[serde(default)]
    pub project: Option<String>,
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
    #[serde(default)]
    pub rootfs_path: String,
    /// Image id/name: create a linked root disk for this VM from it.
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub root_disk_size_gib: Option<u64>,
    /// Existing, unattached disk (id/name) to boot from.
    #[serde(default)]
    pub root_disk: Option<String>,
    #[serde(default)]
    pub data_disks: Option<Vec<String>>,
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
    /// Desired power state; default `stopped` (create, then start).
    #[serde(default)]
    pub power: Option<PowerState>,
    #[serde(default)]
    pub restart_policy: Option<RestartPolicy>,
    #[serde(default)]
    pub on_host_boot: Option<HostBootPolicy>,
    #[serde(default)]
    pub stop_grace_secs: Option<u32>,
}

/// The managed-disk part of `CreateVmRequest`, resolved by `create_vm`.
#[derive(Debug, Clone, Default)]
pub struct DiskSelection {
    pub image: Option<String>,
    pub root_disk_size_gib: Option<u64>,
    pub root_disk: Option<String>,
    pub data_disks: Vec<String>,
}

impl CreateVmRequest {
    pub fn disk_selection(&self) -> DiskSelection {
        DiskSelection {
            image: self.image.clone(),
            root_disk_size_gib: self.root_disk_size_gib,
            root_disk: self.root_disk.clone(),
            data_disks: self.data_disks.clone().unwrap_or_default(),
        }
    }
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
            root_disk: None,
            data_disks: Vec::new(),
            owns_root_disk: false,
            root_disk_binding: None,
            firmware_vars_path: None,
            data_disk_bindings: Vec::new(),
        }
    }
}

/// The API projection of a [`Vm`]: today's flat fields plus the desired
/// state and the controller's view (§7.4). The runtime paths stay private
/// (spec/security.md §9).
#[derive(Debug, Serialize)]
pub struct VmResponse {
    pub id: String,
    pub name: String,
    pub project: String,
    pub state: VmState,
    pub desired_state: PowerState,
    pub generation: u64,
    pub observed_generation: u64,
    /// For `If-Match` (spec/reconciliation.md §12.1).
    pub resource_version: u64,
    pub restart_required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<ExitRecord>,
    pub restart_policy: RestartPolicy,
    pub on_host_boot: HostBootPolicy,
    pub stop_grace_secs: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleting: bool,
    pub vcpu_count: u8,
    pub mem_size_mib: u32,
    pub hypervisor: HypervisorType,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vfio_devices: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nics: Vec<NicState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_disk: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_disks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl From<&Vm> for VmResponse {
    fn from(vm: &Vm) -> Self {
        let config = vm.config();
        VmResponse {
            id: vm.id.clone(),
            name: vm.name.clone(),
            project: vm.project.clone(),
            state: vm.state(),
            desired_state: vm.spec.power,
            generation: vm.generation,
            observed_generation: vm.status.observed_generation,
            resource_version: vm.resource_version,
            restart_required: vm.condition("RestartRequired").is_some_and(|c| c.status == Tristate::True),
            conditions: vm.status.conditions.clone(),
            last_exit: vm.status.last_exit.clone(),
            restart_policy: vm.spec.restart_policy,
            on_host_boot: vm.spec.on_host_boot,
            stop_grace_secs: vm.spec.stop_grace_secs,
            deleting: vm.deletion_requested_at.is_some(),
            vcpu_count: config.vcpu_count,
            mem_size_mib: config.mem_size_mib,
            hypervisor: config.hypervisor,
            vfio_devices: config.vfio_devices.clone(),
            credential: config.credential.clone(),
            root_disk: config.root_disk.clone(),
            data_disks: config.data_disks.clone(),
            warnings: Vec::new(),
            nics: config
                .networks
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let live = vm.status.nics.iter().find(|n| n.nic_index as usize == i);
                    NicState {
                        network: a.network.clone(),
                        mac: a.mac.clone().unwrap_or_default(),
                        port: live.and_then(|n| n.port.clone()),
                        ipv4: live.and_then(|n| n.ipv4),
                    }
                })
                .collect(),
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
