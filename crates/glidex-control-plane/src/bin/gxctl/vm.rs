//! VMs (spec/reconciliation.md §7, §12) on the generated
//! `glidex.vm.v1.VmServiceClient`, over the Connect protocol
//! (spec/grpc.md §9.1). The live console stays a WebSocket (`console.rs`).

use crate::client::{ApiClient, RpcTransport};
use crate::tenancy::{err, rpc_err, usage};
use crate::{
    NetworkRow, credential_choice, format_unix_time, has_flag, project_names, prompt,
    prompt_optional, prompt_path, prompt_path_optional, signed_in_name, storage,
};
use buffa::{EnumValue, Enumeration};
use colored::Colorize;
use glidex_api::proto::glidex::common::v1::{ConditionStatus, EventKind, Hypervisor};
use glidex_api::proto::glidex::vm::v1::*;
use hyper::Method;
use tabled::{Table, Tabled};

pub(crate) type Client = VmServiceClient<RpcTransport>;

pub(crate) async fn rpc(client: &ApiClient) -> Result<Client, String> {
    let t = client.rpc_transport().await.map_err(|e| e.message)?;
    Ok(VmServiceClient::new(t.clone(), t.config()))
}

/// An enum value as the old lower-case word (`VM_STATE_RUNNING` →
/// `running`); empty when unspecified or unknown.
pub(crate) fn enum_word<E: Enumeration>(e: &EnumValue<E>, prefix: &str) -> String {
    e.as_known()
        .filter(|k| k.to_i32() != 0)
        .map(|k| {
            k.proto_name()
                .trim_start_matches(prefix)
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}

fn state_word(vm: &Vm) -> String {
    enum_word(&vm.state, "VM_STATE_")
}

fn desired_word(vm: &Vm) -> String {
    enum_word(&vm.desired_state, "POWER_STATE_")
}

// ---- calls --------------------------------------------------------------------

impl ApiClient {
    /// The VMs of the selected project (all the caller may see when none
    /// is selected), by name.
    pub(crate) async fn list_vms(&self) -> Result<Vec<Vm>, String> {
        rpc(self)
            .await?
            .list_vms(ListVmsRequest {
                project: self.project(),
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned().vms)
            .map_err(rpc_err)
    }

    pub(crate) async fn get_vm(&self, id: &str) -> Result<Vm, String> {
        rpc(self)
            .await?
            .get_vm(GetVmRequest {
                id: id.to_string(),
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned().vm.as_option().cloned().unwrap_or_default())
            .map_err(rpc_err)
    }

    /// Resolve a VM identifier (name or ID) to an ID.
    /// First tries to use it as an ID, then searches by name (within the
    /// selected project, if any).
    pub(crate) async fn resolve_vm(&self, name_or_id: &str) -> Result<String, String> {
        if let Ok(vm) = self.get_vm(name_or_id).await {
            return Ok(vm.id);
        }
        let vms = self.list_vms().await?;
        let matches: Vec<_> = vms.iter().filter(|vm| vm.name == name_or_id).collect();
        match matches.len() {
            0 => Err(format!("VM '{}' not found", name_or_id)),
            1 => Ok(matches[0].id.clone()),
            _ => {
                let ids: Vec<_> = matches
                    .iter()
                    .map(|vm| format!("{} (project {})", vm.id, vm.project))
                    .collect();
                Err(format!(
                    "Multiple VMs found with name '{}'. Use --project or the ID instead: {}",
                    name_or_id,
                    ids.join(", ")
                ))
            }
        }
    }
}

/// How long lifecycle commands wait for the VM to get there, or `None`
/// with `--no-wait` (spec/reconciliation.md §12.3).
pub(crate) fn wait_secs(args: &[&str]) -> Option<u32> {
    (!has_flag(args, "--no-wait")).then_some(60)
}

/// `--graceful [secs]` from a `stop` command's arguments.
pub(crate) fn graceful_stop_secs(args: &[&str]) -> Result<Option<u32>, String> {
    const DEFAULT_SECS: u32 = 60;
    match args {
        [] => Ok(None),
        ["--graceful"] => Ok(Some(DEFAULT_SECS)),
        ["--graceful", secs] => secs
            .parse()
            .map(Some)
            .map_err(|_| format!("'{}' is not a number of seconds", secs)),
        _ => Err("usage: stop <name|id> [--graceful [secs]]".to_string()),
    }
}

/// The VM a lifecycle write answered with; `None` once it is gone.
async fn start_vm(client: &ApiClient, id: &str, wait: Option<u32>) -> Result<Option<Vm>, String> {
    rpc(client)
        .await?
        .start_vm(StartVmRequest {
            id: id.to_string(),
            wait_secs: wait,
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().vm.as_option().cloned())
        .map_err(rpc_err)
}

/// Stop a VM; with `graceful_secs`, power-button first and wait.
async fn stop_vm(
    client: &ApiClient,
    id: &str,
    graceful_secs: Option<u32>,
    wait: Option<u32>,
) -> Result<Option<Vm>, String> {
    // Waiting must cover the grace.
    let wait = wait.map(|w| w.max(graceful_secs.unwrap_or(0) + 20).min(300));
    rpc(client)
        .await?
        .stop_vm(StopVmRequest {
            id: id.to_string(),
            graceful_timeout_secs: graceful_secs,
            wait_secs: wait,
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().vm.as_option().cloned())
        .map_err(rpc_err)
}

async fn pause_vm(client: &ApiClient, id: &str, wait: Option<u32>) -> Result<Option<Vm>, String> {
    rpc(client)
        .await?
        .pause_vm(PauseVmRequest {
            id: id.to_string(),
            wait_secs: wait,
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().vm.as_option().cloned())
        .map_err(rpc_err)
}

async fn delete_vm(
    client: &ApiClient,
    id: &str,
    keep_disk: bool,
    wait: Option<u32>,
) -> Result<(), String> {
    rpc(client)
        .await?
        .delete_vm(DeleteVmRequest {
            id: id.to_string(),
            keep_disk: keep_disk.then_some(true),
            wait_secs: wait,
            ..Default::default()
        })
        .await
        .map(|_| ())
        .map_err(rpc_err)
}

// ---- display ------------------------------------------------------------------

/// A `list` row: the project shown by name when it's known.
#[derive(Tabled)]
struct VmRow {
    id: String,
    name: String,
    project: String,
    state: String,
    node: String,
    vcpu_count: u32,
    mem_size_mib: u32,
    hypervisor: String,
}

pub(crate) fn format_state(state: &str) -> String {
    match state {
        "running" => state.green().to_string(),
        "stopped" => state.red().to_string(),
        "paused" => state.yellow().to_string(),
        "created" => state.blue().to_string(),
        "failed" | "unknown" => state.red().bold().to_string(),
        _ => state.to_string(),
    }
}

/// Whether the VM is where it should be (`stopped` covers `created`).
fn settled(state: &str, desired: &str) -> bool {
    match desired {
        "" => true,
        "stopped" => matches!(state, "stopped" | "created"),
        d => d == state,
    }
}

/// `state`, plus `→ desired` while the VM is still getting there.
fn format_vm_state(vm: &Vm) -> String {
    let (state, desired) = (state_word(vm), desired_word(vm));
    if settled(&state, &desired) {
        format_state(&state)
    } else {
        format!("{} → {}", format_state(&state), desired)
    }
}

/// `format_vm_state` without colours, for tables and `watch`.
pub(crate) fn plain_state(state: &str, desired: &str) -> String {
    if settled(state, desired) {
        state.to_string()
    } else {
        format!("{} → {}", state, desired)
    }
}

/// What the controller says is in the way (the `Ready` condition), if
/// the VM has not converged.
fn not_ready_reason(vm: &Vm) -> Option<String> {
    let ready = vm.conditions.iter().find(|c| c.kind == "Ready")?;
    if ready.status.as_known() == Some(ConditionStatus::CONDITION_STATUS_TRUE) {
        return None;
    }
    Some(format!(
        "{}{}",
        ready.reason,
        if ready.message.is_empty() {
            String::new()
        } else {
            format!(": {}", ready.message)
        }
    ))
}

fn print_lifecycle_result(vm: Option<&Vm>, name: &str) {
    let Some(vm) = vm else {
        return println!("{} VM {} is gone", "Success:".green(), name);
    };
    println!(
        "{} VM {} is now {}",
        "Success:".green(),
        vm.name,
        format_vm_state(vm)
    );
    if let Some(r) = not_ready_reason(vm) {
        println!("  {} {}", "Not there yet:".yellow(), r);
    }
}

// ---- commands -----------------------------------------------------------------

/// `list`
pub async fn list(client: &ApiClient) {
    match client.list_vms().await {
        Ok(vms) if vms.is_empty() => println!("{}", "No VMs found".yellow()),
        Ok(vms) => {
            let names = project_names(client).await;
            let rows: Vec<VmRow> = vms
                .into_iter()
                .map(|vm| VmRow {
                    state: plain_state(&state_word(&vm), &desired_word(&vm)),
                    project: names.get(&vm.project).cloned().unwrap_or(vm.project),
                    node: vm.node.unwrap_or_default(),
                    hypervisor: storage::hypervisor_name(&vm.hypervisor).to_string(),
                    id: vm.id,
                    name: vm.name,
                    vcpu_count: vm.vcpu_count,
                    mem_size_mib: vm.mem_size_mib,
                })
                .collect();
            println!("{}", Table::new(rows));
        }
        Err(e) => err(e),
    }
}

/// The VM `args[0]` names, or the usage / error printed.
async fn resolve(client: &ApiClient, args: &[&str], u: &str) -> Option<String> {
    let Some(name) = args.first() else {
        usage(u);
        return None;
    };
    match client.resolve_vm(name).await {
        Ok(id) => Some(id),
        Err(e) => {
            err(e);
            None
        }
    }
}

/// `get <name|id>`
pub async fn get(client: &ApiClient, args: &[&str]) {
    let Some(vm_id) = resolve(client, args, "Usage: get <name|id>").await else {
        return;
    };
    let vm = match client.get_vm(&vm_id).await {
        Ok(vm) => vm,
        Err(e) => return err(e),
    };
    println!("{}", "VM Details".bold());
    println!("{}", "-".repeat(40));
    println!("  ID:         {}", vm.id.yellow());
    println!("  Name:       {}", vm.name);
    let names = project_names(client).await;
    println!(
        "  Project:    {}",
        names
            .get(&vm.project)
            .map(|n| format!("{} ({})", n, vm.project))
            .unwrap_or(vm.project.clone())
    );
    println!("  State:      {}", format_vm_state(&vm));
    if let Some(r) = not_ready_reason(&vm) {
        println!("  Waiting:    {}", r);
    }
    if let Some(e) = vm.last_exit.as_option() {
        let cause = enum_word(&e.cause, "EXIT_CAUSE_");
        println!(
            "  Last exit:  {} ({})",
            if cause.is_empty() { "?" } else { &cause },
            e.instance_id
        );
    }
    println!("  Hypervisor: {}", storage::hypervisor_name(&vm.hypervisor));
    println!("  vCPUs:      {}", vm.vcpu_count);
    println!("  Memory:     {} MiB", vm.mem_size_mib);
    if !vm.vfio_devices.is_empty() {
        println!("  VFIO:       {}", vm.vfio_devices.join(", "));
    }
    for (i, nic) in vm.nics.iter().enumerate() {
        println!(
            "  NIC {}:      {} {} {}{}",
            i,
            nic.network.cyan(),
            nic.mac,
            nic.ipv4
                .as_deref()
                .map(|ip| ip.green().to_string())
                .unwrap_or_else(|| "-".dimmed().to_string()),
            nic.port
                .as_deref()
                .map(|p| format!(" ({})", p))
                .unwrap_or_default()
        );
    }
}

/// `start <name|id> [--no-wait]`
pub async fn start(client: &ApiClient, args: &[&str]) {
    let Some(vm_id) = resolve(client, args, "Usage: start <name|id> [--no-wait]").await else {
        return;
    };
    match start_vm(client, &vm_id, wait_secs(&args[1..])).await {
        Ok(vm) => print_lifecycle_result(vm.as_ref(), args[0]),
        Err(e) => err(e),
    }
}

/// `stop <name|id> [--graceful [secs]] [--no-wait]`
pub async fn stop(client: &ApiClient, args: &[&str]) {
    if args.is_empty() {
        return usage("Usage: stop <name|id> [--graceful [secs]] [--no-wait]");
    }
    let flags: Vec<&str> = args[1..]
        .iter()
        .copied()
        .filter(|p| *p != "--no-wait")
        .collect();
    let graceful = match graceful_stop_secs(&flags) {
        Ok(g) => g,
        Err(e) => return err(e),
    };
    let Some(vm_id) = resolve(client, args, "").await else {
        return;
    };
    if let Some(secs) = graceful {
        println!(
            "Pressing the power button; waiting up to {} s for the guest to shut down...",
            secs
        );
    }
    match stop_vm(client, &vm_id, graceful, wait_secs(&args[1..])).await {
        Ok(vm) => print_lifecycle_result(vm.as_ref(), args[0]),
        Err(e) => err(e),
    }
}

/// `pause <name|id> [--no-wait]`
pub async fn pause(client: &ApiClient, args: &[&str]) {
    let Some(vm_id) = resolve(client, args, "Usage: pause <name|id> [--no-wait]").await else {
        return;
    };
    match pause_vm(client, &vm_id, wait_secs(&args[1..])).await {
        Ok(vm) => print_lifecycle_result(vm.as_ref(), args[0]),
        Err(e) => err(e),
    }
}

/// `events <name|id>`
pub async fn events(client: &ApiClient, args: &[&str]) {
    let Some(vm_id) = resolve(client, args, "Usage: events <name|id>").await else {
        return;
    };
    let svc = match rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    match svc
        .list_vm_events(ListVmEventsRequest {
            id: vm_id,
            ..Default::default()
        })
        .await
    {
        Ok(r) => {
            for e in r.into_owned().events {
                let reason = if e.kind.as_known() == Some(EventKind::EVENT_KIND_WARNING) {
                    e.reason.yellow().to_string()
                } else {
                    e.reason.clone()
                };
                println!(
                    "{}  {:<12} {:<18} {}",
                    format_unix_time(
                        e.at.as_option()
                            .map(|t| t.seconds.max(0) as u64)
                            .unwrap_or(0)
                    ),
                    e.actor,
                    reason,
                    e.message
                );
            }
        }
        Err(e) => err(rpc_err(e)),
    }
}

/// `delete <name|id> [--keep-disk] [--no-wait]`
pub async fn delete(client: &ApiClient, args: &[&str]) {
    let Some(vm_id) = resolve(client, args, "Usage: delete <name|id>").await else {
        return;
    };
    let keep_disk = has_flag(&args[1..], "--keep-disk");
    let confirm = prompt(&format!(
        "Are you sure you want to delete VM {}{}? [y/N]: ",
        args[0],
        if keep_disk {
            ""
        } else {
            " (and a root disk created for it)"
        }
    ));
    if confirm.to_lowercase() == "y" {
        match delete_vm(client, &vm_id, keep_disk, wait_secs(&args[1..])).await {
            Ok(()) => println!("{} VM deleted", "Success:".green()),
            Err(e) => err(e),
        }
    } else {
        println!("Cancelled");
    }
}

/// `attach-device <name|id> <device-path>` and `detach-device …`.
pub async fn device(client: &ApiClient, attach: bool, args: &[&str]) {
    let u = if attach {
        "Usage: attach-device <name|id> <device-path>"
    } else {
        "Usage: detach-device <name|id> <device-path>"
    };
    let Some(path) = args.get(1) else {
        return usage(u);
    };
    let Some(vm_id) = resolve(client, args, u).await else {
        return;
    };
    let svc = match rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let res = if attach {
        svc.attach_device(AttachDeviceRequest {
            id: vm_id,
            device_path: path.to_string(),
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().vm.as_option().cloned())
    } else {
        svc.detach_device(DetachDeviceRequest {
            id: vm_id,
            device_path: path.to_string(),
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().vm.as_option().cloned())
    };
    match res {
        Ok(vm) => {
            let vm = vm.unwrap_or_default();
            println!(
                "{} Device {} {} VM {}",
                "Success:".green(),
                path,
                if attach {
                    "attached to"
                } else {
                    "detached from"
                },
                if vm.name.is_empty() {
                    args[0]
                } else {
                    &vm.name
                }
            );
            if !vm.vfio_devices.is_empty() {
                println!("  VFIO devices: {}", vm.vfio_devices.join(", "));
            }
        }
        Err(e) => err(rpc_err(e)),
    }
}

/// `create` (interactive).
pub async fn create(client: &ApiClient) {
    println!("{}", "Create new VM".bold());
    println!("{}", "-".repeat(40));

    let name = prompt("VM name: ");
    if name.is_empty() {
        println!("{}", "Error: VM name is required".red());
        return;
    }

    let vcpu_count: u8 = prompt("vCPU count [1]: ").parse().unwrap_or(1);

    let mem_size_mib: u32 = prompt("Memory (MiB) [512]: ").parse().unwrap_or(512);

    let hypervisor = match prompt("Hypervisor [cloudhypervisor/qemu] (default: cloudhypervisor): ")
        .to_lowercase()
        .as_str()
    {
        "" | "cloudhypervisor" | "cloud-hypervisor" | "ch" => {
            Hypervisor::HYPERVISOR_CLOUD_HYPERVISOR
        }
        "qemu" | "q" => Hypervisor::HYPERVISOR_QEMU,
        other => {
            println!(
                "{} Unknown hypervisor '{}', using cloudhypervisor",
                "Warning:".yellow(),
                other
            );
            Hypervisor::HYPERVISOR_CLOUD_HYPERVISOR
        }
    };

    // A disk image boots through UEFI firmware: a firmware image built for
    // the hypervisor (spec images.md §7), or a host path (admins only).
    let (firmware, firmware_path) = {
        let ty = match hypervisor {
            Hypervisor::HYPERVISOR_QEMU => "qemu",
            _ => "cloudhypervisor",
        };
        let ready: Vec<String> = storage::list_images(client)
            .await
            .unwrap_or_default()
            .into_iter()
            .rev() // newest first
            .filter(|i| {
                storage::is_firmware(i)
                    && storage::hypervisor_name(&i.hypervisor) == ty
                    && storage::is_ready(i)
                    && i.deleting != Some(true)
            })
            .map(|i| i.name)
            .collect();
        if ready.is_empty() {
            let key = if ty == "qemu" { "ovmf" } else { "cloudhv-edk2" };
            println!(
                "{} no firmware image for {}; pull one with 'image pull --firmware {}' for firmware boot",
                "Note:".yellow(),
                ty,
                key
            );
        }
        let default = ready.first().cloned().unwrap_or_else(|| "none".to_string());
        let options = if ready.is_empty() {
            String::new()
        } else {
            format!("{}, ", ready.join(", "))
        };
        match prompt_path(&format!(
            "UEFI firmware [{}'none' for kernel boot, or a host path] [{}]: ",
            options, default
        ))
        .as_str()
        {
            "" if default == "none" => (None, None),
            "" => (Some(default), None),
            "none" => (None, None),
            s if s.starts_with('/') => (None, Some(s.to_string())),
            s => (Some(s.to_string()), None),
        }
    };
    let firmware_boot = firmware.is_some() || firmware_path.is_some();

    let kernel_image_path = if firmware_boot {
        String::new()
    } else {
        let path = prompt_path("Kernel image path: ");
        if path.is_empty() {
            println!(
                "{}",
                "Error: kernel image path is required for kernel boot".red()
            );
            return;
        }
        path
    };

    // Boot disk. Firmware boot offers managed images first (spec
    // images.md §10); kernel boot takes a bare root filesystem image.
    let (mut rootfs_path, mut image, mut root_disk_size_gib, mut root_disk) =
        (String::new(), None, None, None);
    let mut choice = "path".to_string();
    if firmware_boot {
        let ready: Vec<String> = storage::list_images(client)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|i| storage::is_ready(i) && !storage::is_firmware(i))
            .map(|i| i.name)
            .collect();
        let default = if ready.is_empty() { "path" } else { "image" };
        choice =
            prompt(&format!("Boot disk from [image/disk/path] [{}]: ", default)).to_lowercase();
        if choice.is_empty() {
            choice = default.to_string();
        }
        match choice.as_str() {
            "image" | "i" => {
                if ready.is_empty() {
                    println!(
                        "{}",
                        "Error: no downloaded images; pull one with 'image pull <key>'".red()
                    );
                    return;
                }
                let pick = prompt(&format!("Image ({}) [{}]: ", ready.join(", "), ready[0]));
                image = Some(if pick.is_empty() {
                    ready[0].clone()
                } else {
                    pick
                });
                root_disk_size_gib = match prompt("Root disk size in GiB [10]: ").as_str() {
                    "" => None,
                    s => match s.parse::<u32>() {
                        Ok(n) => Some(n),
                        Err(_) => {
                            println!("{}", "Error: size must be a whole number of GiB".red());
                            return;
                        }
                    },
                };
            }
            "disk" | "d" => {
                let d = prompt("Existing disk (name or id; see 'disk list'): ");
                if d.is_empty() {
                    println!("{}", "Error: a disk is required".red());
                    return;
                }
                root_disk = Some(d);
            }
            _ => choice = "path".into(),
        }
    }
    if choice == "path" {
        // Firmware boot needs a disk with its own bootloader (e.g. a raw
        // distro cloud image); kernel boot takes a bare root filesystem image.
        let rootfs_prompt = if firmware_boot {
            "Disk image path (UEFI-bootable, e.g. a raw cloud image): "
        } else {
            "Root filesystem path: "
        };
        rootfs_path = prompt_path(rootfs_prompt);
        if rootfs_path.is_empty() {
            println!("{}", "Error: disk image path is required".red());
            return;
        }
    }
    let data_disks: Vec<String> =
        prompt_optional("Data disks (optional, comma-separated disk names): ")
            .map(|s| {
                s.split(',')
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty())
                    .collect()
            })
            .unwrap_or_default();

    let cloud_init_path = if firmware_boot {
        prompt_path_optional("cloud-init seed image (optional, default: auto-generated): ")
    } else {
        None
    };

    // A stored credential is provisioned through the generated seed only.
    let credential = if firmware_boot && cloud_init_path.is_none() {
        let names: Vec<String> = storage::list_credentials(client)
            .await
            .map(|cs| cs.into_iter().map(|c| c.username).collect())
            .unwrap_or_default();
        if names.is_empty() {
            println!(
                "{} no login available: this project has no credentials, so the VM will have no way to log in (add one with 'credential-add').",
                "Login credential:".dimmed()
            );
            None
        } else {
            // Default to the signed-in user's own credential, if the
            // project has one by that name.
            let mine = signed_in_name(client).await.filter(|n| names.contains(n));
            let answer = match &mine {
                Some(m) => prompt(&format!(
                    "Login credential (one of: {}; 'none' for no login) [{}]: ",
                    names.join(", "),
                    m
                )),
                None => prompt(&format!(
                    "Login credential (one of: {}; empty: none, no login): ",
                    names.join(", ")
                )),
            };
            credential_choice(&answer, mine.as_deref())
        }
    } else {
        None
    };

    let kernel_args = if firmware_boot {
        None
    } else {
        prompt_optional("Kernel arguments (optional, default: root=/dev/vda reboot=k panic=1): ")
    };

    // Networks: offer the ones that exist.
    let networks: Vec<NetworkAttachment> = {
        let names: Vec<String> = client
            .request_json::<Vec<NetworkRow>>(Method::GET, "/networks", None)
            .await
            .map(|ns| ns.into_iter().map(|n| n.name).collect())
            .unwrap_or_default();
        if names.is_empty() {
            Vec::new()
        } else {
            let default = if names.iter().any(|n| n == "default") {
                "default"
            } else {
                "none"
            };
            let answer = prompt(&format!(
                "Networks (comma-separated; available: {}; 'none' for no NIC) [{}]: ",
                names.join(", "),
                default
            ));
            let answer = if answer.is_empty() {
                default.to_string()
            } else {
                answer
            };
            if answer == "none" {
                Vec::new()
            } else {
                answer
                    .split(',')
                    .map(|n| n.trim())
                    .filter(|n| !n.is_empty())
                    .map(|n| NetworkAttachment {
                        network: n.to_string(),
                        ..Default::default()
                    })
                    .collect()
            }
        }
    };

    let vfio_devices: Vec<String> = prompt_path_optional(
        "VFIO PCI devices (comma-separated, e.g. /sys/bus/pci/devices/0000:41:00.0): ",
    )
    .map(|s| {
        s.split(',')
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .collect()
    })
    .unwrap_or_default();

    let request = CreateVmRequest {
        name,
        project: client.project(),
        vcpu_count: vcpu_count.into(),
        mem_size_mib,
        kernel_image_path,
        firmware,
        firmware_path,
        cloud_init_path,
        credential,
        rootfs_path,
        image,
        root_disk_size_gib,
        root_disk,
        data_disks,
        kernel_args,
        hypervisor: hypervisor.into(),
        vfio_devices,
        networks,
        ..Default::default()
    };

    let svc = match rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    match svc.create_vm(request).await {
        Ok(r) => {
            let vm = r.into_owned().vm.as_option().cloned().unwrap_or_default();
            println!("{}", "VM created successfully!".green());
            println!("  ID: {}", vm.id.yellow());
            println!("  Name: {}", vm.name);
            println!("  State: {}", state_word(&vm));
            println!("  Hypervisor: {}", storage::hypervisor_name(&vm.hypervisor));
        }
        Err(e) => err(rpc_err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_words_are_the_old_lower_case_names() {
        let s: EnumValue<VmState> = VmState::VM_STATE_RUNNING.into();
        assert_eq!(enum_word(&s, "VM_STATE_"), "running");
        let s: EnumValue<VmState> = VmState::VM_STATE_UNSPECIFIED.into();
        assert_eq!(enum_word(&s, "VM_STATE_"), "");
        let c: EnumValue<ExitCause> = ExitCause::EXIT_CAUSE_CLEAN_EXIT.into();
        assert_eq!(enum_word(&c, "EXIT_CAUSE_"), "clean_exit");
    }

    #[test]
    fn plain_state_shows_where_the_vm_is_going() {
        assert_eq!(plain_state("created", "stopped"), "created");
        assert_eq!(plain_state("running", "running"), "running");
        assert_eq!(plain_state("stopped", "running"), "stopped → running");
        assert_eq!(plain_state("running", ""), "running");
    }

    #[test]
    fn graceful_stop_arguments() {
        assert_eq!(graceful_stop_secs(&[]), Ok(None));
        assert_eq!(graceful_stop_secs(&["--graceful"]), Ok(Some(60)));
        assert_eq!(graceful_stop_secs(&["--graceful", "15"]), Ok(Some(15)));
        assert!(graceful_stop_secs(&["--graceful", "soon"]).is_err());
        assert!(graceful_stop_secs(&["--force"]).is_err());
    }
}
