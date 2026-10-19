use crate::hypervisor::HypervisorType;
use glidex_ovs::vm_port::VmPortBinding;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use uuid::Uuid;

/// Expand a leading `~` or `~/` to the user's home directory. Hypervisors
/// don't do shell-style expansion themselves, so paths like
/// `~/.glidex/rootfs.ext4` need to be resolved before being passed to
/// qemu-system-x86_64 / cloud-hypervisor.
pub(crate) fn expand_tilde(path: String) -> String {
    if path == "~" {
        return dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or(path);
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).to_string_lossy().into_owned();
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
        let s = serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default();
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
    /// ignored. For a firmware image (`firmware_image`) this is the image's
    /// file, so backends see one field either way: stored as the creating
    /// server's path, and replaced at launch with the placed node's copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware_path: Option<String>,
    /// Firmware image (id) the VM boots through; see spec/images.md §7.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firmware_image: Option<String>,
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
    /// `2M` or `1G`: the hugepage size for `hugepages`; absent: the node's
    /// default size (spec/host-topology.md §8a.3). At launch, the size the
    /// scheduler counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hugepage_size: Option<String>,
    /// Keep the VM's memory, CPUs and devices on one host NUMA node
    /// (spec/host-topology.md H5).
    #[serde(default, skip_serializing_if = "NumaPolicy::is_none")]
    pub numa_policy: NumaPolicy,
    /// Dedicated: each vCPU on a host CPU of its own (H6).
    #[serde(default, skip_serializing_if = "CpuPolicy::is_shared")]
    pub cpu_policy: CpuPolicy,
    /// At launch: the host NUMA node guest RAM is bound to, and the host
    /// CPU of each vCPU (dedicated), from the placement. Never persisted.
    #[serde(skip)]
    pub host_numa_node: Option<u32>,
    #[serde(skip)]
    pub vcpu_pins: Vec<u32>,
    /// Host-side NIC bindings from glidex-netd, filled in by `start_vm`
    /// for the hypervisor; never persisted.
    #[serde(skip)]
    pub nic_bindings: Vec<NicBinding>,
    /// Format etc. of a managed root disk, filled in by `start_vm`; never
    /// persisted. `None`: user-supplied `rootfs_path`, format probed.
    #[serde(skip)]
    pub root_disk_binding: Option<VolumeBinding>,
    /// Managed data disks, filled in by `start_vm`; never persisted.
    #[serde(skip)]
    pub data_disk_bindings: Vec<VolumeBinding>,
    /// This VM's own copy of the OVMF variable store (QEMU firmware boot),
    /// filled in by `start_vm`; never persisted.
    #[serde(skip)]
    pub firmware_vars_path: Option<String>,
    /// The pristine variable store of a firmware image, filled in by
    /// `start_vm`; never persisted. `None`: found next to the code file.
    #[serde(skip)]
    pub firmware_vars_template: Option<String>,
}

/// What a hypervisor needs to open one managed disk (spec/storage.md §7).
pub use crate::datastore::VolumeBinding;

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
    /// Pin the VM to this node (id), spec/clustering.md §9.1. Immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

/// Where a VM runs (spec/clustering.md §9.1): set once, by the scheduler,
/// and kept for the VM's life (D6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub node: String,
    pub at: u64,
    /// The page size in KiB the scheduler counted guest RAM in: 4 for
    /// ordinary memory, else a hugepage size (spec/host-topology.md §8a.3).
    /// Absent: placed before it was counted; the node decides at start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_kib: Option<u64>,
    /// The host NUMA node guest RAM is bound to (spec/host-topology.md
    /// §6), under a NUMA policy that fitted one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numa_node: Option<u32>,
    /// Dedicated: the host CPUs the VM holds, vCPU `i` on `cpus[i]`; with
    /// whole cores, the idle siblings follow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cpus: Vec<u32>,
}

/// KiB of ordinary memory pages.
pub const SMALL_PAGE_KIB: u64 = 4;

/// spec/host-topology.md H5: `preferred` keeps memory (and CPUs and
/// devices) on one host NUMA node when one fits, `strict` refuses a node
/// where they can't be.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NumaPolicy {
    #[default]
    None,
    Preferred,
    Strict,
}

impl NumaPolicy {
    pub fn is_none(&self) -> bool {
        *self == NumaPolicy::None
    }
}

/// spec/host-topology.md H6.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CpuPolicy {
    #[default]
    Shared,
    Dedicated,
}

impl CpuPolicy {
    pub fn is_shared(&self) -> bool {
        *self == CpuPolicy::Shared
    }
}

/// `2M` / `1G` → KiB; `None` for anything else.
pub fn hugepage_size_kib(size: &str) -> Option<u64> {
    match size {
        "2M" => Some(2048),
        "1G" => Some(1_048_576),
        _ => None,
    }
}

/// KiB → `2M` / `1G` (or `<n>K`).
pub fn hugepage_size_name(kib: u64) -> String {
    match kib {
        2048 => "2M".into(),
        1_048_576 => "1G".into(),
        k => format!("{k}K"),
    }
}

/// `status`: written only by the VM controller.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VmStatus {
    /// The node this VM is placed on. A standalone host is one implicit
    /// node (`local`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    #[serde(default)]
    pub observed_generation: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub last_reconciled_at: u64,
    #[serde(default)]
    pub phase: VmPhase,
    /// When `phase` last changed (unix s), for metering transitions
    /// exactly (spec/metering.md §5.6). Absent in records from before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_since: Option<u64>,
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
    /// The host CPUs its threads are bound to; empty: unbound
    /// (spec/host-topology.md §7.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cpu_affinity: Vec<u32>,
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
    /// The shim's final counters (spec/metering.md D12), consumed once by
    /// the meter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<glidex_vm_shim::state::ExitUsage>,
    /// The managed disk at each launched disk index, for `usage.disks`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disk_ids: Vec<Option<String>>,
    /// When the instance was launched (unix s); 0 in older records.
    #[serde(default)]
    pub launched_at: u64,
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
                node: None,
            },
            status: VmStatus {
                never_started: true,
                ..Default::default()
            },
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
            && self
                .condition("Ready")
                .is_some_and(|c| c.status == Tristate::True)
    }
}

/// The managed disks a new VM asks for, resolved by `create_vm`.
#[derive(Debug, Clone, Default)]
pub struct DiskSelection {
    /// Firmware image id/name.
    pub firmware: Option<String>,
    pub image: Option<String>,
    pub root_disk_size_gib: Option<u64>,
    pub root_disk: Option<String>,
    pub data_disks: Vec<String>,
}
