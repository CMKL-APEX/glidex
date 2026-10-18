//! `glidex.vm.v1` (spec/reconciliation.md §7, §12): VMs. Creating one is a
//! compound request (spec/security.md §7.4): every disk, credential,
//! network, PCI device and host path it names is checked on its own.
//! Writes change the desired state and return at once, or with `wait`,
//! once the VM controller acted (§12.3). The REST twins in `api/vms.rs`
//! and the RPC adapter in `api/rpc/vm.rs` call these.

use super::common::{condition_pb, event_pb, hypervisor_of, hypervisor_pb, ts, ts_opt};
use super::{Call, ServiceError, ServiceResult};
use crate::api::vms::{device_bdf, pci_entities, read_log_tail};
use crate::api::{credential_entities, disk_entities, project_entities, vm_entities};
use crate::authz::{Ent, EntitySet};
use crate::models::{self as m, PowerState, Vm, VmConfig};
use crate::state::{ConfigPatch, VmOptions, VmPatch, Waited};
use axum::http::StatusCode;
use glidex_api::proto::glidex::vm::v1 as pb;
use std::time::Duration;

/// Longest `wait` (and stop grace).
const MAX_WAIT_SECS: u64 = 300;

fn manager_err(e: crate::state::VmManagerError) -> ServiceError {
    crate::api::error_to_response(e).into()
}

// ---- conversions ----------------------------------------------------------

fn state_pb(s: m::VmState) -> pb::VmState {
    use m::VmState as S;
    match s {
        S::Created => pb::VmState::VM_STATE_CREATED,
        S::Starting => pb::VmState::VM_STATE_STARTING,
        S::Running => pb::VmState::VM_STATE_RUNNING,
        S::Paused => pb::VmState::VM_STATE_PAUSED,
        S::Stopping => pb::VmState::VM_STATE_STOPPING,
        S::Stopped => pb::VmState::VM_STATE_STOPPED,
        S::Failed => pb::VmState::VM_STATE_FAILED,
        S::Unknown => pb::VmState::VM_STATE_UNKNOWN,
    }
}

fn power_pb(p: PowerState) -> pb::PowerState {
    match p {
        PowerState::Running => pb::PowerState::POWER_STATE_RUNNING,
        PowerState::Paused => pb::PowerState::POWER_STATE_PAUSED,
        PowerState::Stopped => pb::PowerState::POWER_STATE_STOPPED,
    }
}

fn power_of(p: pb::PowerState) -> Option<PowerState> {
    match p {
        pb::PowerState::POWER_STATE_RUNNING => Some(PowerState::Running),
        pb::PowerState::POWER_STATE_PAUSED => Some(PowerState::Paused),
        pb::PowerState::POWER_STATE_STOPPED => Some(PowerState::Stopped),
        pb::PowerState::POWER_STATE_UNSPECIFIED => None,
    }
}

fn restart_pb(r: m::RestartPolicy) -> pb::RestartPolicy {
    match r {
        m::RestartPolicy::OnFailure => pb::RestartPolicy::RESTART_POLICY_ON_FAILURE,
        m::RestartPolicy::Never => pb::RestartPolicy::RESTART_POLICY_NEVER,
    }
}

fn restart_of(r: pb::RestartPolicy) -> Option<m::RestartPolicy> {
    match r {
        pb::RestartPolicy::RESTART_POLICY_ON_FAILURE => Some(m::RestartPolicy::OnFailure),
        pb::RestartPolicy::RESTART_POLICY_NEVER => Some(m::RestartPolicy::Never),
        pb::RestartPolicy::RESTART_POLICY_UNSPECIFIED => None,
    }
}

fn boot_pb(h: m::HostBootPolicy) -> pb::HostBootPolicy {
    match h {
        m::HostBootPolicy::Resume => pb::HostBootPolicy::HOST_BOOT_POLICY_RESUME,
        m::HostBootPolicy::Stop => pb::HostBootPolicy::HOST_BOOT_POLICY_STOP,
    }
}

fn boot_of(h: pb::HostBootPolicy) -> Option<m::HostBootPolicy> {
    match h {
        pb::HostBootPolicy::HOST_BOOT_POLICY_RESUME => Some(m::HostBootPolicy::Resume),
        pb::HostBootPolicy::HOST_BOOT_POLICY_STOP => Some(m::HostBootPolicy::Stop),
        pb::HostBootPolicy::HOST_BOOT_POLICY_UNSPECIFIED => None,
    }
}

fn exit_pb(e: &m::ExitRecord) -> pb::ExitRecord {
    use m::ExitCause as C;
    pb::ExitRecord {
        at: ts(e.at).into(),
        instance_id: e.instance_id.clone(),
        cause: match e.cause {
            C::Requested => pb::ExitCause::EXIT_CAUSE_REQUESTED,
            C::Terminated => pb::ExitCause::EXIT_CAUSE_TERMINATED,
            C::CleanExit => pb::ExitCause::EXIT_CAUSE_CLEAN_EXIT,
            C::Crashed => pb::ExitCause::EXIT_CAUSE_CRASHED,
            C::LaunchFailed => pb::ExitCause::EXIT_CAUSE_LAUNCH_FAILED,
            C::HostReboot => pb::ExitCause::EXIT_CAUSE_HOST_REBOOT,
            C::Lost => pb::ExitCause::EXIT_CAUSE_LOST,
        }
        .into(),
        code: e.code,
        signal: e.signal,
        message: e.message.clone(),
        launched_at: ts_opt(e.launched_at).into(),
        ..Default::default()
    }
}

fn attachment_of(a: &pb::NetworkAttachment) -> m::NetworkAttachment {
    m::NetworkAttachment {
        network: a.network.clone(),
        mac: a.mac.clone(),
        queue_pairs: a.queue_pairs.map(|q| q.min(u32::from(u8::MAX)) as u8),
    }
}

/// A VM as the API shows it, without the caller's view (its node's name,
/// OVN addresses): for error details. The runtime paths stay private
/// (spec/security.md §9).
pub fn vm_pb_plain(vm: &Vm) -> pb::Vm {
    let config = vm.config();
    pb::Vm {
        id: vm.id.clone(),
        name: vm.name.clone(),
        project: vm.project.clone(),
        state: state_pb(vm.state()).into(),
        desired_state: power_pb(vm.spec.power).into(),
        generation: vm.generation,
        observed_generation: vm.status.observed_generation,
        resource_version: vm.resource_version,
        restart_required: Some(
            vm.condition("RestartRequired")
                .is_some_and(|c| c.status == m::Tristate::True),
        ),
        conditions: vm.status.conditions.iter().map(condition_pb).collect(),
        last_exit: vm.status.last_exit.as_ref().map(exit_pb).into(),
        restart_policy: restart_pb(vm.spec.restart_policy).into(),
        on_host_boot: boot_pb(vm.spec.on_host_boot).into(),
        stop_grace_secs: vm.spec.stop_grace_secs,
        deleting: vm.deletion_requested_at.is_some().then_some(true),
        node: vm.status.placement.as_ref().map(|p| p.node.clone()),
        vcpu_count: u32::from(config.vcpu_count),
        mem_size_mib: config.mem_size_mib,
        hypervisor: hypervisor_pb(config.hypervisor).into(),
        vfio_devices: config.vfio_devices.clone(),
        credential: config.credential.clone(),
        nics: config
            .networks
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let live = vm.status.nics.iter().find(|n| n.nic_index as usize == i);
                pb::Nic {
                    network: a.network.clone(),
                    mac: a.mac.clone().unwrap_or_default(),
                    port: live.and_then(|n| n.port.clone()),
                    ipv4: live.and_then(|n| n.ipv4).map(|ip| ip.to_string()),
                    ..Default::default()
                }
            })
            .collect(),
        firmware: config.firmware_image.clone(),
        root_disk: config.root_disk.clone(),
        data_disks: config.data_disks.clone(),
        ..Default::default()
    }
}

/// A VM as the caller sees it: with its node's name, its networks' names
/// and, on OVN networks, the address the control plane reserved (OVN's
/// DHCP hands it out, so the controller on the VM's node never learns it;
/// the reservations are in the cluster store, which an agent doesn't hold).
pub fn vm_pb(call: &Call, vm: &Vm) -> pb::Vm {
    let mut v = vm_pb_plain(vm);
    v.node_name = v
        .node
        .as_deref()
        .and_then(|n| call.manager().nodes().get(n).ok().flatten())
        .map(|n| n.spec.name);
    if v.nics.iter().any(|n| n.ipv4.is_none()) {
        let res =
            crate::ipam::list_reservations(&call.manager().store.database()).unwrap_or_default();
        for (i, nic) in v
            .nics
            .iter_mut()
            .enumerate()
            .filter(|(_, n)| n.ipv4.is_none())
        {
            nic.ipv4 = res
                .iter()
                .find(|x| x.vm_id == vm.id && x.nic as usize == i && x.network == nic.network)
                .map(|x| x.ip.to_string());
        }
    }
    for n in &mut v.nics {
        n.network_name = call.manager().get_network(&n.network).ok().map(|n| n.name);
    }
    v
}

// ---- answers to writes ----------------------------------------------------

/// Where a VM write left the VM (spec/reconciliation.md §12.3).
pub enum VmReply {
    /// Not waited for: the VM as written.
    Now(pb::Vm),
    /// Waited for, and the controller converged.
    Settled(pb::Vm),
    /// Waited for, and the time ran out.
    Pending(pb::Vm),
    Gone,
}

impl VmReply {
    /// The VM and whether it settled, for an RPC answer.
    pub fn parts(self) -> (Option<pb::Vm>, Option<bool>) {
        match self {
            VmReply::Now(v) => (Some(v), None),
            VmReply::Settled(v) => (Some(v), Some(true)),
            VmReply::Pending(v) => (Some(v), Some(false)),
            VmReply::Gone => (None, Some(true)),
        }
    }
}

/// The error a failed reconcile reports when the caller waited for it
/// (D20): what the call would have returned synchronously.
fn failed_reconcile(vm: &Vm) -> ServiceError {
    let (status, axum::Json(e)) = crate::api::errors::failed_reconcile_response(vm);
    let mut s = ServiceError::new(status, e.error, e.message);
    s.details = e.details;
    s
}

async fn respond(
    call: &Call,
    vm: Vm,
    wait: Option<u32>,
    warnings: Vec<String>,
) -> ServiceResult<VmReply> {
    let with = |vm: &Vm| {
        let mut v = vm_pb(call, vm);
        v.warnings = warnings.clone();
        v
    };
    let Some(secs) = wait else {
        return Ok(VmReply::Now(with(&vm)));
    };
    let secs = u64::from(secs).min(MAX_WAIT_SECS);
    Ok(
        match call
            .manager()
            .wait_converged(&vm.id, vm.generation, Duration::from_secs(secs))
            .await
        {
            Waited::Decided(v) if v.is_converged() => VmReply::Settled(with(&v)),
            Waited::Decided(v) => return Err(failed_reconcile(&v)),
            Waited::Gone | Waited::TimedOut(None) => VmReply::Gone,
            Waited::TimedOut(Some(v)) => VmReply::Pending(with(&v)),
        },
    )
}

/// The VM `id`, if the caller may see it (else not found), and its
/// entities.
async fn visible_vm(call: &Call, id: &str) -> ServiceResult<(Vm, Ent, EntitySet)> {
    let vm = call.manager().get_vm(id).await.map_err(manager_err)?;
    call.set_project(&vm.project);
    call.set_target(format!("vm:{}", vm.id));
    let (e, es) = vm_entities(&vm);
    call.require_visible("readVm", e.clone(), es.clone(), "VM")?;
    Ok((vm, e, es))
}

// ---- reads ----------------------------------------------------------------

pub async fn list_vms(call: &Call, req: pb::ListVmsRequest) -> ServiceResult<pb::ListVmsResponse> {
    let only = match &req.project {
        Some(p) => Some(call.target_project(Some(p))?),
        None => None,
    };
    let visible = call.visible_projects()?;
    let mut vms: Vec<pb::Vm> = Vec::new();
    for vm in call.manager().list_vms().await {
        if only.as_ref().is_some_and(|p| *p != vm.project) || !visible.contains(&vm.project) {
            continue;
        }
        let (e, es) = vm_entities(&vm);
        if call.allowed("readVm", e, es) {
            vms.push(vm_pb(call, &vm));
        }
    }
    vms.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pb::ListVmsResponse {
        vms,
        ..Default::default()
    })
}

pub async fn get_vm(call: &Call, req: pb::GetVmRequest) -> ServiceResult<pb::GetVmResponse> {
    let (vm, _, _) = visible_vm(call, &req.id).await?;
    Ok(pb::GetVmResponse {
        vm: vm_pb(call, &vm).into(),
        ..Default::default()
    })
}

pub async fn vm_events(
    call: &Call,
    req: pb::ListVmEventsRequest,
) -> ServiceResult<pb::ListVmEventsResponse> {
    let (vm, _, _) = visible_vm(call, &req.id).await?;
    let events = call.manager().vm_events(&vm.id).map_err(manager_err)?;
    Ok(pb::ListVmEventsResponse {
        events: events.iter().map(event_pb).collect(),
        ..Default::default()
    })
}

/// Where a VM would go (spec/clustering-ui.md §5.1). Whoever may create a
/// VM in the project may see where it would fit.
pub async fn preview_placement(
    call: &Call,
    req: pb::PreviewPlacementRequest,
) -> ServiceResult<pb::PreviewPlacementResponse> {
    let project = call.target_project(req.project.as_deref())?;
    call.require_action(
        "createVm",
        Ent::Project(project.clone()),
        project_entities(&project),
        &[],
    )?;
    let hypervisor = hypervisor_of(req.hypervisor.as_known().unwrap_or_default());
    let mut cfg = serde_json::json!({
        "vcpu_count": req.vcpu_count.min(u32::from(u8::MAX)), "mem_size_mib": req.mem_size_mib,
        "rootfs_path": "", "kernel_args": "", "hugepages": req.hugepages.unwrap_or(false),
        "networks": req.networks.iter().map(attachment_of).collect::<Vec<_>>(),
        "vfio_devices": req.vfio_devices,
    });
    if let Some(h) = hypervisor {
        cfg["hypervisor"] = serde_json::json!(h);
    }
    let config: VmConfig = serde_json::from_value(cfg)
        .map_err(|e| ServiceError::new(StatusCode::BAD_REQUEST, "invalid", e.to_string()))?;
    let fits = call
        .manager()
        .preview_placement(
            &project,
            &config,
            req.node.clone(),
            &req.data_disks,
            req.image.as_deref(),
        )
        .await
        .map_err(manager_err)?;
    let pick = fits
        .first()
        .filter(|f| f.refused.is_none())
        .map(|f| f.id.clone());
    Ok(pb::PreviewPlacementResponse {
        pick,
        nodes: fits
            .into_iter()
            .map(|f| pb::NodeFit {
                id: f.id,
                name: f.name,
                refused: f.refused,
                vcpus_taken: f.vcpus_taken,
                vcpus_capacity: f.vcpus_capacity,
                mem_mib_taken: f.mem_mib_taken,
                mem_mib_capacity: f.mem_mib_capacity,
                vms: f.vms,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    })
}

// ---- create ---------------------------------------------------------------

/// Whether a create request names a host path (spec/security.md §7.4).
/// Firmware is a managed image (`firmware`) like the disks; a
/// `firmware_path` is a host path like any other.
fn names_host_path(req: &pb::CreateVmRequest) -> bool {
    !req.kernel_image_path.is_empty()
        || !req.rootfs_path.is_empty()
        || req.cloud_init_path.is_some()
        || req.firmware_path.is_some()
}

pub async fn create_vm(call: &Call, req: pb::CreateVmRequest) -> ServiceResult<VmReply> {
    let project = call.target_project(req.project.as_deref())?;
    call.set_project(&project);
    let pent = Ent::Project(project.clone());
    let pes = project_entities(&project);
    call.require(pent.clone(), pes.clone())?;
    call.require_admitted(&project)?;
    let with_project: &[(&'static str, Ent)] = &[("project", pent.clone())];

    for key in req.data_disks.iter().chain(req.root_disk.iter()) {
        if let Ok(d) = call.manager().get_disk_for(&project, key).await {
            let (e, es) = disk_entities(&d.id, &d.project);
            call.require_action("useDisk", e, es, with_project)?;
        }
    }
    if let Some(username) = &req.credential {
        let (e, mut es) = credential_entities(&project, username);
        es.project(&project);
        call.require_action("useCredential", e, es, with_project)?;
    }
    if !req.networks.is_empty() {
        call.require_action("attachNetwork", pent.clone(), pes.clone(), &[])?;
        for att in &req.networks {
            if let Ok(n) = call.manager().find_network(&project, &att.network) {
                let mut es = pes.clone();
                let e = crate::api::add_network(&mut es, &n);
                call.require_action("useNetwork", e, es, with_project)?;
            }
        }
    }
    for dev in &req.vfio_devices {
        let bdf = device_bdf(dev)?;
        let (e, mut es) = pci_entities(call, &bdf);
        es.project(&project);
        call.require_action("usePciDevice", e, es, with_project)?;
    }
    if names_host_path(&req) {
        call.require_action("useHostPath", Ent::Host, EntitySet::new(), &[])?;
    }
    for image in req.image.iter().chain(req.firmware.iter()) {
        let mut es = EntitySet::new();
        es.image(image);
        call.require_action("readImage", Ent::Image(image.clone()), es, &[])?;
    }

    let wait = req.wait_secs;
    let hypervisor =
        hypervisor_of(req.hypervisor.as_known().unwrap_or_default()).unwrap_or_default();
    let sel = m::DiskSelection {
        firmware: req.firmware.clone(),
        image: req.image.clone(),
        root_disk_size_gib: req.root_disk_size_gib.map(u64::from),
        root_disk: req.root_disk.clone(),
        data_disks: req.data_disks.clone(),
    };
    let name = req.name.clone();
    let opts = VmOptions {
        power: power_of(req.power.as_known().unwrap_or_default()),
        restart_policy: restart_of(req.restart_policy.as_known().unwrap_or_default()),
        on_host_boot: boot_of(req.on_host_boot.as_known().unwrap_or_default()),
        stop_grace_secs: req.stop_grace_secs,
        node: req.node.clone(),
    };
    let config = VmConfig {
        vcpu_count: req.vcpu_count.min(u32::from(u8::MAX)) as u8,
        mem_size_mib: req.mem_size_mib,
        kernel_image_path: m::expand_tilde(req.kernel_image_path),
        firmware_path: req.firmware_path.map(m::expand_tilde),
        cloud_init_path: req.cloud_init_path.map(m::expand_tilde),
        credential: req.credential,
        rootfs_path: m::expand_tilde(req.rootfs_path),
        kernel_args: req
            .kernel_args
            .unwrap_or_else(|| hypervisor.default_kernel_args().to_string()),
        hypervisor,
        vfio_devices: req.vfio_devices,
        networks: req.networks.iter().map(attachment_of).collect(),
        hugepages: req.hugepages.unwrap_or(false),
        firmware_image: None,
        nic_bindings: Vec::new(),
        root_disk: None,
        data_disks: Vec::new(),
        owns_root_disk: false,
        root_disk_binding: None,
        firmware_vars_path: None,
        firmware_vars_template: None,
        data_disk_bindings: Vec::new(),
    };
    let quota = call.quota_mode(&project);
    let (vm, warnings, over) = call
        .manager()
        .create_vm_with(&project, name, config, sel, opts, quota, &call.actor())
        .await
        .map_err(manager_err)?;
    call.note_overruns(&over);
    call.set_target(format!("vm:{}", vm.id));
    respond(call, vm, wait, warnings).await
}

// ---- power ----------------------------------------------------------------

async fn set_power(
    call: &Call,
    id: &str,
    power: PowerState,
    grace: Option<u32>,
    wait: Option<u32>,
    if_match: Option<u64>,
) -> ServiceResult<VmReply> {
    let (vm, _, _) = visible_vm(call, id).await?;
    if power == PowerState::Running {
        call.require_admitted(&vm.project)?;
    }
    let quota = call.quota_mode(&vm.project);
    let (vm, over) = call
        .manager()
        .set_power(id, power, grace, quota, &call.actor(), if_match)
        .await
        .map_err(manager_err)?;
    call.note_overruns(&over);
    respond(call, vm, wait, Vec::new()).await
}

pub async fn start_vm(call: &Call, req: pb::StartVmRequest) -> ServiceResult<VmReply> {
    set_power(
        call,
        &req.id,
        PowerState::Running,
        None,
        req.wait_secs,
        req.expected_resource_version,
    )
    .await
}

pub async fn stop_vm(call: &Call, req: pb::StopVmRequest) -> ServiceResult<VmReply> {
    let grace = req
        .graceful_timeout_secs
        .map(|s| u64::from(s).min(MAX_WAIT_SECS) as u32);
    set_power(
        call,
        &req.id,
        PowerState::Stopped,
        grace,
        req.wait_secs,
        req.expected_resource_version,
    )
    .await
}

pub async fn pause_vm(call: &Call, req: pb::PauseVmRequest) -> ServiceResult<VmReply> {
    set_power(
        call,
        &req.id,
        PowerState::Paused,
        None,
        req.wait_secs,
        req.expected_resource_version,
    )
    .await
}

// ---- update ---------------------------------------------------------------

/// The fields `UpdateVm` changes, by their mask path.
pub const UPDATABLE: &[&str] = &[
    "power",
    "restart_policy",
    "on_host_boot",
    "stop_grace_secs",
    "vcpu_count",
    "mem_size_mib",
    "kernel_args",
    "credential",
    "hugepages",
    "vfio_devices",
    "data_disks",
    "networks",
];

/// `change` and `mask` as the manager's merge patch.
fn patch_of(change: &pb::VmChange, mask: &[String]) -> ServiceResult<VmPatch> {
    let mut p = VmPatch::default();
    let mut c = ConfigPatch::default();
    let mut config = false;
    for path in mask {
        match path.as_str() {
            "power" => p.power = power_of(change.power.as_known().unwrap_or_default()),
            "restart_policy" => {
                p.restart_policy = restart_of(change.restart_policy.as_known().unwrap_or_default())
            }
            "on_host_boot" => {
                p.on_host_boot = boot_of(change.on_host_boot.as_known().unwrap_or_default())
            }
            "stop_grace_secs" => p.stop_grace_secs = Some(change.stop_grace_secs),
            other => {
                config = true;
                match other {
                    "vcpu_count" => {
                        c.vcpu_count = Some(change.vcpu_count.min(u32::from(u8::MAX)) as u8)
                    }
                    "mem_size_mib" => c.mem_size_mib = Some(change.mem_size_mib),
                    "kernel_args" => c.kernel_args = Some(change.kernel_args.clone()),
                    // Absent: the VM loses its credential.
                    "credential" => {
                        c.credential = Some(match &change.credential {
                            Some(u) => serde_json::Value::String(u.clone()),
                            None => serde_json::Value::Null,
                        })
                    }
                    "hugepages" => c.hugepages = Some(change.hugepages),
                    "vfio_devices" => c.vfio_devices = Some(change.vfio_devices.clone()),
                    "data_disks" => c.data_disks = Some(change.data_disks.clone()),
                    "networks" => {
                        c.networks = Some(change.networks.iter().map(attachment_of).collect())
                    }
                    other => {
                        return Err(ServiceError::new(
                            StatusCode::BAD_REQUEST,
                            "invalid_config",
                            format!("can't change {other} (immutable, or no such field)"),
                        ));
                    }
                }
            }
        }
    }
    if config {
        p.config = Some(c);
    }
    Ok(p)
}

/// Authorized per changed field (§12.1): power → start/stop/pauseVm,
/// devices → attach/detachDevice + usePciDevice, data disks →
/// attach/detachDisk + useDisk, networks → attach/detachNetwork +
/// useNetwork, credential → updateVm + useCredential, anything else →
/// updateVm.
pub async fn update_vm(call: &Call, req: pb::UpdateVmRequest) -> ServiceResult<VmReply> {
    let mask: Vec<String> = req
        .update_mask
        .as_option()
        .map(|m| m.paths.clone())
        .unwrap_or_default();
    let change = req.change.as_option().cloned().unwrap_or_default();
    let patch = patch_of(&change, &mask)?;
    let vm = call.manager().get_vm(&req.id).await.map_err(manager_err)?;
    call.set_project(&vm.project);
    call.set_target(format!("vm:{}", vm.id));
    let (e, es) = vm_entities(&vm);
    call.require_readable("readVm", e.clone(), es.clone(), "VM")?;
    let project = vm.project.clone();
    let with_project: &[(&'static str, Ent)] = &[("project", Ent::Project(project.clone()))];
    let mut update = false;
    if let Some(p) = patch.power
        && p != vm.spec.power
    {
        let action = match p {
            PowerState::Running => "startVm",
            PowerState::Paused => "pauseVm",
            PowerState::Stopped => "stopVm",
        };
        call.require_action(action, e.clone(), es.clone(), &[])?;
        if p == PowerState::Running {
            call.require_admitted(&project)?;
        }
    }
    update |= patch
        .restart_policy
        .is_some_and(|r| r != vm.spec.restart_policy)
        || patch
            .on_host_boot
            .is_some_and(|h| h != vm.spec.on_host_boot)
        || patch
            .stop_grace_secs
            .is_some_and(|g| g != vm.spec.stop_grace_secs);
    if let Some(cp) = &patch.config {
        let cur = vm.config();
        update |= cp.vcpu_count.is_some_and(|v| v != cur.vcpu_count)
            || cp.mem_size_mib.is_some_and(|v| v != cur.mem_size_mib)
            || cp
                .kernel_args
                .as_ref()
                .is_some_and(|v| *v != cur.kernel_args)
            || cp.hugepages.is_some_and(|v| v != cur.hugepages);
        if let Some(cred) = &cp.credential {
            update = true;
            if let Some(username) = cred.as_str() {
                let (ce, mut ces) = credential_entities(&project, username);
                ces.project(&project);
                call.require_action("useCredential", ce, ces, with_project)?;
            }
        }
        if let Some(devs) = &cp.vfio_devices {
            for d in devs.iter().filter(|d| !cur.vfio_devices.contains(d)) {
                call.require_action("attachDevice", e.clone(), es.clone(), &[])?;
                let bdf = device_bdf(d)?;
                let (pe, mut pes) = pci_entities(call, &bdf);
                pes.project(&project);
                call.require_action("usePciDevice", pe, pes, with_project)?;
            }
            if cur.vfio_devices.iter().any(|d| !devs.contains(d)) {
                call.require_action("detachDevice", e.clone(), es.clone(), &[])?;
            }
        }
        if let Some(keys) = &cp.data_disks {
            let mut ids = Vec::new();
            for key in keys {
                if let Ok(d) = call.manager().get_disk_for(&project, key).await {
                    if !cur.data_disks.contains(&d.id) {
                        call.require_action("attachDisk", e.clone(), es.clone(), &[])?;
                        let (de, des) = disk_entities(&d.id, &d.project);
                        call.require_action("useDisk", de, des, with_project)?;
                    }
                    ids.push(d.id);
                }
            }
            if cur.data_disks.iter().any(|d| !ids.contains(d)) {
                call.require_action("detachDisk", e.clone(), es.clone(), &[])?;
            }
        }
        if let Some(nets) = &cp.networks {
            // NICs name their network by id; a request may give a name.
            let new: Vec<(String, Option<crate::network::Network>)> = nets
                .iter()
                .map(
                    |a| match call.manager().find_network(&project, &a.network) {
                        Ok(n) => (n.id.clone(), Some(n)),
                        Err(_) => (a.network.clone(), None),
                    },
                )
                .collect();
            let added: Vec<_> = new
                .iter()
                .filter(|(id, _)| !cur.networks.iter().any(|b| &b.network == id))
                .collect();
            if !added.is_empty() {
                call.require_action("attachNetwork", e.clone(), es.clone(), &[])?;
                for n in added.into_iter().filter_map(|(_, n)| n.as_ref()) {
                    let mut nes = project_entities(&project);
                    let ne = crate::api::add_network(&mut nes, n);
                    call.require_action("useNetwork", ne, nes, with_project)?;
                }
            }
            if cur
                .networks
                .iter()
                .any(|b| !new.iter().any(|(id, _)| id == &b.network))
            {
                call.require_action("detachNetwork", e.clone(), es.clone(), &[])?;
            }
        }
    }
    if update {
        call.require_action("updateVm", e.clone(), es.clone(), &[])?;
    }
    let quota = call.quota_mode(&project);
    let (vm, over) = call
        .manager()
        .patch_vm(
            &req.id,
            patch,
            quota,
            &call.actor(),
            req.expected_resource_version,
        )
        .await
        .map_err(manager_err)?;
    call.note_overruns(&over);
    respond(call, vm, req.wait_secs, Vec::new()).await
}

// ---- delete ---------------------------------------------------------------

/// `None` once the VM is gone: at once (nothing ran), or with `wait` when
/// the controller finished in time.
pub async fn delete_vm(call: &Call, req: pb::DeleteVmRequest) -> ServiceResult<Option<pb::Vm>> {
    visible_vm(call, &req.id).await?;
    let m = call.manager();
    if m.delete_vm_as(&req.id, req.keep_disk.unwrap_or(false), &call.actor())
        .await
        .map_err(manager_err)?
    {
        return Ok(None);
    }
    let Some(secs) = req.wait_secs else {
        return Ok(m.get_vm(&req.id).await.ok().map(|vm| vm_pb(call, &vm)));
    };
    let generation = m.get_vm(&req.id).await.map(|v| v.generation).unwrap_or(0);
    let secs = u64::from(secs).min(MAX_WAIT_SECS);
    Ok(
        match m
            .wait_converged(&req.id, generation, Duration::from_secs(secs))
            .await
        {
            Waited::Gone | Waited::TimedOut(None) => None,
            Waited::Decided(vm) | Waited::TimedOut(Some(vm)) => Some(vm_pb(call, &vm)),
        },
    )
}

// ---- console --------------------------------------------------------------

pub async fn console_info(
    call: &Call,
    req: pb::GetConsoleInfoRequest,
) -> ServiceResult<pb::GetConsoleInfoResponse> {
    let (vm, _, _) = visible_vm(call, &req.id).await?;
    Ok(pb::GetConsoleInfoResponse {
        // While an instance exists, including after the guest exited and
        // before the shim is released.
        available: Some(vm.status.instance.is_some()),
        websocket: format!("/vms/{}/console/ws", vm.id),
        vm_id: vm.id,
        ..Default::default()
    })
}

pub async fn console_ticket(
    call: &Call,
    req: pb::CreateConsoleTicketRequest,
) -> ServiceResult<pb::CreateConsoleTicketResponse> {
    let (vm, _, _) = visible_vm(call, &req.id).await?;
    Ok(pb::CreateConsoleTicketResponse {
        ticket: call.auth().issue_ticket(&call.p, &vm.id),
        expires_in: crate::auth::TICKET_SECS as u32,
        ..Default::default()
    })
}

/// The VM's captured console output (it can hold anything the guest
/// printed, so it needs `vm.console` like the live console): the last
/// `tail_bytes` (at most 1 MiB), from the node the VM runs on.
pub async fn console_log(
    call: &Call,
    req: pb::GetConsoleLogRequest,
) -> ServiceResult<pb::GetConsoleLogResponse> {
    let (vm, _, _) = visible_vm(call, &req.id).await?;
    const CAP: u64 = 1 << 20;
    let tail = req.tail_bytes.unwrap_or(CAP).min(CAP);
    let previous = req.previous.unwrap_or(false);
    if let Some(addr) = call.manager().remote_node_addr(&vm) {
        let Some(cluster) = call.manager().cluster() else {
            return Err(ServiceError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "no cluster",
            ));
        };
        let path = format!(
            "/cluster/v1/vms/{}/console-log?tail_bytes={}&previous={}",
            vm.id, tail, previous
        );
        let r = cluster
            .client
            .request(
                &addr,
                axum::http::Method::GET,
                &path,
                &[],
                bytes::Bytes::new(),
            )
            .await
            .map_err(|e| {
                ServiceError::new(StatusCode::BAD_GATEWAY, "node_unreachable", e.to_string())
            })?;
        return if r.status.is_success() {
            Ok(pb::GetConsoleLogResponse {
                data: r.body.to_vec(),
                ..Default::default()
            })
        } else {
            Err(ServiceError::new(r.status, "not_found", "no console log"))
        };
    }
    let paths = vm.paths();
    let path = if previous {
        paths.previous_log()
    } else {
        paths.log
    };
    let bytes = read_log_tail(path, tail, previous).await.map_err(|e| {
        ServiceError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("console log: {}", e),
        )
    })?;
    match bytes {
        Some(data) => Ok(pb::GetConsoleLogResponse {
            data,
            ..Default::default()
        }),
        None => Err(ServiceError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no rotated console log",
        )),
    }
}

// ---- devices and disks ----------------------------------------------------

pub async fn attach_device(call: &Call, req: pb::AttachDeviceRequest) -> ServiceResult<VmReply> {
    let (vm, _, _) = visible_vm(call, &req.id).await?;
    let bdf = device_bdf(&req.device_path)?;
    let (e, mut es) = pci_entities(call, &bdf);
    es.project(&vm.project);
    call.require_action(
        "usePciDevice",
        e,
        es,
        &[("project", Ent::Project(vm.project.clone()))],
    )?;
    let vm = call
        .manager()
        .attach_device(&req.id, req.device_path)
        .await
        .map_err(manager_err)?;
    respond(call, vm, req.wait_secs, Vec::new()).await
}

pub async fn detach_device(call: &Call, req: pb::DetachDeviceRequest) -> ServiceResult<VmReply> {
    visible_vm(call, &req.id).await?;
    let vm = call
        .manager()
        .detach_device(&req.id, &req.device_path)
        .await
        .map_err(manager_err)?;
    respond(call, vm, req.wait_secs, Vec::new()).await
}

pub async fn attach_disk(call: &Call, req: pb::AttachDiskRequest) -> ServiceResult<VmReply> {
    let (vm, _, _) = visible_vm(call, &req.id).await?;
    let d = call
        .manager()
        .get_disk_for(&vm.project, &req.disk)
        .await
        .map_err(manager_err)?;
    let (e, es) = disk_entities(&d.id, &d.project);
    call.require_action(
        "useDisk",
        e,
        es,
        &[("project", Ent::Project(vm.project.clone()))],
    )?;
    let vm = call
        .manager()
        .attach_disk(&req.id, &req.disk)
        .await
        .map_err(manager_err)?;
    respond(call, vm, req.wait_secs, Vec::new()).await
}

pub async fn detach_disk(call: &Call, req: pb::DetachDiskRequest) -> ServiceResult<VmReply> {
    visible_vm(call, &req.id).await?;
    let vm = call
        .manager()
        .detach_disk(&req.id, &req.disk)
        .await
        .map_err(manager_err)?;
    respond(call, vm, req.wait_secs, Vec::new()).await
}

// ---- the live console (the WebSocket and `Console`) ---------------------------

/// Where a VM's console is: its shim's socket here, or its node.
pub enum ConsoleTarget {
    Local(String),
    /// On another node: relayed over `/cluster/v1/vms/{id}/console` (§8.4).
    Remote {
        addr: String,
        vm: String,
    },
}

/// The console's byte stream, whichever way it is reached.
pub trait ConsoleIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> ConsoleIo for T {}

/// The console of VM `id` if the caller may open it: they may read the VM
/// (else not found), and a browser session (PAM, OIDC) needs a fresh
/// single-use `ticket` (spec/security.md §5.6). Tokens and the API socket
/// don't.
pub async fn console_target(
    call: &Call,
    id: &str,
    ticket: Option<&str>,
) -> ServiceResult<ConsoleTarget> {
    let vm = call.manager().get_vm(id).await.map_err(manager_err)?;
    call.set_project(&vm.project);
    call.set_target(format!("vm:{}", vm.id));
    let (e, es) = vm_entities(&vm);
    call.require_visible("readVm", e, es, "VM")?;
    use crate::auth::Method;
    if matches!(call.p.method, Method::Pam | Method::Oidc)
        && !ticket.is_some_and(|t| call.auth().redeem_ticket(&call.p, &vm.id, t))
    {
        return Err(ServiceError::new(
            StatusCode::FORBIDDEN,
            "ticket_required",
            "open the console with a fresh ticket",
        ));
    }
    Ok(match call.manager().remote_node_addr(&vm) {
        Some(addr) => ConsoleTarget::Remote { addr, vm: vm.id },
        None => ConsoleTarget::Local(vm.paths().console_socket),
    })
}

/// Connect to the console a target names.
pub async fn console_connect(
    m: &crate::state::VmManager,
    target: ConsoleTarget,
) -> Result<Box<dyn ConsoleIo>, String> {
    match target {
        ConsoleTarget::Local(path) => tokio::net::UnixStream::connect(&path)
            .await
            .map(|s| Box::new(s) as Box<dyn ConsoleIo>)
            .map_err(|e| format!("Failed to connect to the VM console: {e}")),
        ConsoleTarget::Remote { addr, vm } => {
            let cluster = m.cluster().ok_or("no cluster")?;
            cluster
                .client
                .upgrade(&addr, &format!("/cluster/v1/vms/{vm}/console"))
                .await
                .map(|s| Box::new(s) as Box<dyn ConsoleIo>)
                .map_err(|e| format!("the VM's node can't be reached: {e}"))
        }
    }
}
