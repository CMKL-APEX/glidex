//! VMs (spec/12-reconciliation.md §7, §12) on the generated
//! `glidex.vm.v1.VmServiceClient`, over the Connect protocol
//! (spec/18-grpc.md §9.1). The live console is the `Console` bidi RPC (`console.rs`).

use crate::client::{ApiClient, RpcTransport};
use crate::tenancy::{err, rpc_err, usage};
use crate::{
    credential_choice, flag_value, format_unix_time, has_flag, project_names, prompt,
    prompt_optional, prompt_path, prompt_path_optional, signed_in_name, storage,
};
use buffa::{EnumValue, Enumeration};
use colored::Colorize;
use glidex_api::proto::glidex::common::v1::{ConditionStatus, EventKind, Hypervisor};
use glidex_api::proto::glidex::storage::v1::Image;
use glidex_api::proto::glidex::vm::v1::*;
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
/// with `--no-wait` (spec/12-reconciliation.md §12.3).
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
    if let Some(c) = vm.conditions.iter().find(|c| {
        c.kind == "PylonManaged"
            && c.status.as_known() == Some(ConditionStatus::CONDITION_STATUS_FALSE)
    }) {
        println!("  Pylon:      {} ({})", "not managed".yellow(), c.message);
    }
    if let (Some(min), Some(max)) = (vm.min_vcpus, vm.max_vcpus) {
        if min < max {
            println!(
                "  vCPUs:      {} (elastic: {}–{}, weight: {})",
                vm.vcpu_count,
                min,
                max,
                vm.cpu_weight.unwrap_or(100)
            );
        } else {
            println!("  vCPUs:      {}", vm.vcpu_count);
        }
    } else {
        println!("  vCPUs:      {}", vm.vcpu_count);
    }
    println!("  Memory:     {} MiB", vm.mem_size_mib);
    if let Some(sa) = &vm.service_account {
        println!(
            "  Service account: {}",
            service_account_label(&service_accounts(client).await, sa)
        );
    }
    if !vm.vfio_devices.is_empty() {
        println!("  VFIO:       {}", vm.vfio_devices.join(", "));
    }
    if let Some(g) = vm.gpus.as_option() {
        println!(
            "  GPUs:       {} from {}{}",
            g.count,
            g.pool.as_deref().unwrap_or("-"),
            if g.nvlink == Some(true) {
                " (NVLink)"
            } else {
                ""
            }
        );
    }
    for (i, s) in vm.numa.iter().enumerate() {
        println!(
            "  NUMA {i}:     host node {}, {} vCPUs, {} MiB{}",
            s.node,
            s.vcpus,
            s.memory_mib,
            if s.devices.is_empty() {
                String::new()
            } else {
                format!(", {}", s.devices.join(", "))
            }
        );
    }
    for r in &vm.devices {
        let got: Vec<&str> = vm
            .pool_devices
            .iter()
            .filter(|d| d.pool == r.pool)
            .map(|d| d.address.as_str())
            .collect();
        println!(
            "  Pool:       {} × {}{} → {}",
            r.count.max(1),
            r.pool,
            if r.same_switch { " (one switch)" } else { "" },
            if got.is_empty() {
                "-".to_string()
            } else {
                got.join(", ")
            }
        );
    }
    for (i, nic) in vm.nics.iter().enumerate() {
        println!(
            "  NIC {}:      {} {} {}{}{}",
            i,
            nic.network_name.as_deref().unwrap_or(&nic.network).cyan(),
            nic.mac,
            nic.ipv4
                .as_deref()
                .map(|ip| ip.green().to_string())
                .unwrap_or_else(|| "-".dimmed().to_string()),
            nic.port
                .as_deref()
                .map(|p| format!(" ({})", p))
                .unwrap_or_default(),
            if nic.port_security == Some(false) {
                " port security off".yellow().to_string()
            } else {
                String::new()
            }
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

/// `place <name|id> [--node N]`: place a stopped or lost VM again
/// (spec/24-storage.md S10).
pub async fn place(client: &ApiClient, args: &[&str]) {
    let Some(vm_id) = resolve(client, args, "Usage: place <name|id> [--node N]").await else {
        return;
    };
    let t = match rpc(client).await {
        Ok(t) => t,
        Err(e) => return err(e),
    };
    let r = t
        .place_vm(PlaceVmRequest {
            id: vm_id,
            node: crate::flag_value(args, "--node").map(String::from),
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().vm.as_option().cloned());
    match r {
        Ok(Some(vm)) if !vm.node.as_deref().unwrap_or_default().is_empty() => println!(
            "{} {} on {}",
            "Placed:".green(),
            vm.name,
            vm.node_name
                .as_deref()
                .or(vm.node.as_deref())
                .unwrap_or_default()
        ),
        Ok(Some(vm)) => println!(
            "{} {} waits for a node: {}",
            "Not placed yet:".yellow(),
            vm.name,
            vm.conditions
                .iter()
                .find(|c| c.kind == "Scheduled")
                .map(|c| c.message.clone())
                .unwrap_or_default()
        ),
        Ok(None) => {}
        Err(e) => err(rpc_err(e)),
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

/// `a100:2, cx7` → pool requests (count 1 by default; `:switch` after the
/// count keeps them under one PCIe switch, spec/22-host-topology.md §9).
pub(crate) fn parse_device_requests(s: &str) -> Vec<DeviceRequest> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|x| {
            let mut parts = x.split(':');
            let pool = parts.next().unwrap_or_default().to_string();
            let count = parts.next().and_then(|c| c.parse().ok()).unwrap_or(1);
            let same_switch = parts.next() == Some("switch");
            DeviceRequest {
                pool,
                count,
                same_switch,
                ..Default::default()
            }
        })
        .collect()
}

/// `4`, `4:a100`, `8:a100:nvlink` → a `gpus` request (spec/27-gpu-vms.md §3).
pub(crate) fn parse_gpus(s: &str) -> Result<Option<GpuRequest>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let mut parts = s.split(':').map(str::trim);
    let count: u32 = parts
        .next()
        .unwrap_or_default()
        .parse()
        .map_err(|_| format!("'{s}': give count[:pool][:nvlink], e.g. 4:a100"))?;
    let mut g = GpuRequest {
        count,
        ..Default::default()
    };
    for p in parts {
        match p {
            "nvlink" => g.nvlink = Some(true),
            "" => {}
            pool if g.pool.is_none() => g.pool = Some(pool.to_string()),
            other => return Err(format!("'{s}': unexpected '{other}'")),
        }
    }
    Ok(Some(g))
}

/// `create`: interactive; `create <name> [flags]`: without prompts.
pub async fn create(client: &ApiClient, args: &[&str]) {
    if !args.is_empty() {
        return create_unprompted(client, args).await;
    }
    println!("{}", "Create new VM".bold());
    println!("{}", "-".repeat(40));

    let name = prompt("VM name: ");
    if name.is_empty() {
        return err("VM name is required");
    }

    let gpus = match parse_gpus(&prompt(
        "GPUs (count[:pool][:nvlink], e.g. 4:a100; sizes the VM from the pool; optional): ",
    )) {
        Ok(g) => g,
        Err(e) => return err(e),
    };
    // With GPUs, an empty answer takes the pool's per-device sizing (0).
    let (vcpu_default, mem_default, hint) = match gpus {
        Some(_) => (0, 0, "from the pool"),
        None => (1, 512, ""),
    };
    let vcpu_count: u8 = prompt(&format!(
        "vCPU count [{}]: ",
        if gpus.is_some() {
            hint.to_string()
        } else {
            vcpu_default.to_string()
        }
    ))
    .parse()
    .unwrap_or(vcpu_default);

    let mem_size_mib: u32 = prompt(&format!(
        "Memory (MiB) [{}]: ",
        if gpus.is_some() {
            hint.to_string()
        } else {
            mem_default.to_string()
        }
    ))
    .parse()
    .unwrap_or(mem_default);

    let hypervisor =
        match prompt("Hypervisor [cloudhypervisor/qemu/pylon] (default: cloudhypervisor): ")
            .as_str()
        {
            "" => Hypervisor::HYPERVISOR_CLOUD_HYPERVISOR,
            word => hypervisor_word(word).unwrap_or_else(|| {
                println!(
                    "{} Unknown hypervisor '{}', using cloudhypervisor",
                    "Warning:".yellow(),
                    word
                );
                Hypervisor::HYPERVISOR_CLOUD_HYPERVISOR
            }),
        };

    let (min_vcpus, max_vcpus, cpu_weight, vcpu_count) = {
        let is_pylon = hypervisor == Hypervisor::HYPERVISOR_PYLON;
        let default_ans = if is_pylon { "y" } else { "n" };
        let elastic_prompt = prompt(&format!(
            "Configure elastic CPU scaling (min/max vCPUs, weight)? [y/N] [{}]: ",
            default_ans
        ));
        let want_elastic = if elastic_prompt.is_empty() {
            is_pylon
        } else {
            elastic_prompt.to_lowercase().starts_with('y')
        };
        if want_elastic {
            let min_default = 1u32;
            let min: u32 = prompt(&format!("Min vCPUs [{}]: ", min_default))
                .parse()
                .unwrap_or(min_default);
            let max_default = (vcpu_count as u32).max(4);
            let max: u32 = prompt(&format!("Max vCPUs [{}]: ", max_default))
                .parse()
                .unwrap_or(max_default);
            let weight: u32 = prompt("CPU weight (1-10000) [100]: ")
                .parse()
                .unwrap_or(100);
            let vcpus = if is_pylon { max as u8 } else { vcpu_count };
            (Some(min), Some(max), Some(weight), vcpus)
        } else {
            (None, None, None, vcpu_count)
        }
    };

    // A disk image boots through UEFI firmware: a firmware image built for
    // the hypervisor (spec 10-images.md §7), or a host path (admins only).
    let (firmware, firmware_path) = {
        let ready = ready_firmware(client, hypervisor).await;
        let default = ready.first().cloned().unwrap_or_else(|| "none".to_string());
        let options = if ready.is_empty() {
            String::new()
        } else {
            format!("{}, ", ready.join(", "))
        };
        match prompt_path(&format!(
            "UEFI firmware [{}'kernel' for direct kernel boot, or a host path] [{}]: ",
            options, default
        ))
        .as_str()
        {
            "" if default == "none" => (None, None),
            "" => (Some(default), None),
            "none" | "kernel" => (None, None),
            s if s.starts_with('/') => (None, Some(s.to_string())),
            s => (Some(s.to_string()), None),
        }
    };
    let firmware_boot = firmware.is_some() || firmware_path.is_some();
    let all_images = storage::list_images(client).await.unwrap_or_default();

    // Boot disk: a managed image or disk first (spec 10-images.md §10), for
    // either boot mode (spec 23-kernel-boot.md §6).
    let (mut rootfs_path, mut image, mut root_disk_size_gib, mut root_disk) =
        (String::new(), None, None, None);
    let mut choice;
    {
        let ready: Vec<String> = all_images
            .iter()
            .filter(|i| storage::is_ready(i) && !storage::is_firmware(i) && !storage::is_kernel(i))
            .map(|i| i.name.clone())
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
                    return err("no downloaded images; pull one with 'image pull <key>'");
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
                            return err("size must be a whole number of GiB");
                        }
                    },
                };
            }
            "disk" | "d" => {
                let d = prompt("Existing disk (name or id; see 'disk list'): ");
                if d.is_empty() {
                    return err("a disk is required");
                }
                root_disk = Some(d);
            }
            _ => choice = "path".into(),
        }
    }
    if choice == "path" {
        // Firmware boot needs a disk with its own bootloader (e.g. a raw
        // distro cloud image); kernel boot takes a root filesystem or disk.
        let rootfs_prompt = if firmware_boot {
            "Disk image path (UEFI-bootable, e.g. a raw cloud image): "
        } else {
            "Root filesystem or disk image path: "
        };
        rootfs_path = prompt_path(rootfs_prompt);
        if rootfs_path.is_empty() {
            return err("disk image path is required");
        }
    }

    // The kernel (kernel boot): a kernel image, the one extracted from the
    // chosen image first (spec 23-kernel-boot.md K3), or host files.
    let (mut kernel, mut kernel_image_path, mut initrd_path, mut kernel_from_disk) =
        (None, String::new(), None, None);
    let mut kernel_cmdline = None;
    if !firmware_boot {
        let chosen = image
            .as_deref()
            .and_then(|k| all_images.iter().find(|i| i.name == k || i.id == k));
        let (kernels, default) = kernel_choices(&all_images, chosen);
        let names: Vec<&str> = kernels.iter().map(|k| k.name.as_str()).collect();
        let answer = prompt_path(&format!(
            "Kernel [{}a host path] [{}]: ",
            if names.is_empty() {
                String::new()
            } else {
                format!("{}, or ", names.join(", "))
            },
            default
        ));
        let answer = if answer.is_empty() {
            default.to_string()
        } else {
            answer
        };
        if answer.is_empty() {
            return err("a kernel is required for kernel boot");
        }
        if answer.starts_with('/') {
            kernel_image_path = answer;
            initrd_path = prompt_path_optional("Initramfs path (optional): ");
        } else {
            let picked = kernels.iter().find(|k| k.name == answer || k.id == answer);
            warn_foreign_kernel(&all_images, chosen, picked.copied());
            kernel_cmdline = picked
                .and_then(|k| k.kernel.as_option())
                .and_then(|k| k.cmdline.clone());
            kernel = Some(answer);
            if choice != "path" {
                let follow = prompt(
                    "Follow the guest's kernel (use the one installed on the disk, before each start) [y/N]: ",
                );
                kernel_from_disk =
                    matches!(follow.to_lowercase().as_str(), "y" | "yes").then_some(true);
            }
        }
    }
    // The generated seed (and so a credential) comes with firmware boot, a
    // kernel image or a managed root disk (spec 23-kernel-boot.md K4).
    let seeded = firmware_boot || kernel.is_some() || choice != "path";
    let data_disks: Vec<String> =
        prompt_optional("Data disks (optional, comma-separated disk names): ")
            .map(|s| {
                s.split(',')
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty())
                    .collect()
            })
            .unwrap_or_default();

    let cloud_init_path = if seeded {
        prompt_path_optional("cloud-init seed image (optional, default: auto-generated): ")
    } else {
        None
    };

    // A stored credential is provisioned through the generated seed only.
    let credential = if seeded && cloud_init_path.is_none() {
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
        prompt_optional(&format!(
            "Kernel arguments (optional, default: {}): ",
            kernel_cmdline
                .as_deref()
                .unwrap_or("console=ttyS0 (ttyAMA0 on aarch64) root=/dev/vda reboot=k panic=1")
        ))
    };

    // Networks: offer the ones that exist.
    let networks: Vec<NetworkAttachment> = {
        let names = crate::network::names(client).await;
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

    let devices = parse_device_requests(&prompt(
        "Device pools (comma-separated pool[:count], e.g. a100:2; optional): ",
    ));

    let request = CreateVmRequest {
        gpus: gpus.into(),
        name,
        project: client.project(),
        vcpu_count: vcpu_count.into(),
        min_vcpus,
        max_vcpus,
        cpu_weight,
        mem_size_mib,
        kernel_image_path,
        initrd_path,
        kernel,
        kernel_from_disk,
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
        devices,
        ..Default::default()
    };
    send_create(client, request).await;
}

async fn send_create(client: &ApiClient, request: CreateVmRequest) {
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

/// `ch`, `qemu`, `pylon` and their longer names.
fn hypervisor_word(s: &str) -> Option<Hypervisor> {
    match s.to_lowercase().as_str() {
        "cloudhypervisor" | "cloud-hypervisor" | "ch" => {
            Some(Hypervisor::HYPERVISOR_CLOUD_HYPERVISOR)
        }
        "qemu" | "q" => Some(Hypervisor::HYPERVISOR_QEMU),
        "pylon" | "p" => Some(Hypervisor::HYPERVISOR_PYLON),
        _ => None,
    }
}

/// The ready firmware images `hypervisor` can boot through, newest first;
/// none is said, with how to pull one.
async fn ready_firmware(client: &ApiClient, hypervisor: Hypervisor) -> Vec<String> {
    let ty = match hypervisor {
        Hypervisor::HYPERVISOR_QEMU => "qemu",
        Hypervisor::HYPERVISOR_PYLON => "pylon",
        _ => "cloudhypervisor",
    };
    let ready: Vec<String> = storage::list_images(client)
        .await
        .unwrap_or_default()
        .into_iter()
        .rev() // newest first
        .filter(|i| {
            let matches_hv = storage::hypervisor_name(&i.hypervisor) == ty
                || (ty == "pylon" && storage::hypervisor_name(&i.hypervisor) == "cloudhypervisor");
            storage::is_firmware(i)
                && matches_hv
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
    ready
}

/// The ready kernel images, those taken from `chosen` first, and the
/// default: `chosen`'s own kernel, or without an image the first.
fn kernel_choices<'a>(all: &'a [Image], chosen: Option<&Image>) -> (Vec<&'a Image>, &'a str) {
    let mut kernels: Vec<&Image> = all
        .iter()
        .filter(|i| storage::is_kernel(i) && storage::is_ready(i) && i.deleting != Some(true))
        .collect();
    kernels.sort_by_key(|k| k.for_image.as_deref() != chosen.map(|c| c.id.as_str()));
    if let Some(c) = chosen
        && !kernels
            .iter()
            .any(|k| k.for_image.as_deref() == Some(c.id.as_str()))
    {
        println!(
            "{} no kernel from {} yet; extract one with 'image extract-kernel {}'",
            "Note:".yellow(),
            c.name,
            c.name
        );
    }
    // Another image's kernel is never the default: its initramfs looks
    // for that image's root, and its modules aren't on this disk.
    let default = match chosen {
        Some(c) => kernels
            .iter()
            .find(|k| k.for_image.as_deref() == Some(c.id.as_str()))
            .map_or("", |k| k.name.as_str()),
        None => kernels.first().map_or("", |k| k.name.as_str()),
    };
    (kernels, default)
}

fn warn_foreign_kernel(all: &[Image], chosen: Option<&Image>, picked: Option<&Image>) {
    if let (Some(c), Some(k)) = (chosen, picked)
        && let Some(other) = k.for_image.as_deref().filter(|f| *f != c.id)
    {
        let from = all
            .iter()
            .find(|i| i.id == other)
            .map_or(other, |i| i.name.as_str());
        println!(
            "{} {} was taken from {from}; {} may not boot with it",
            "Warning:".yellow(),
            k.name,
            c.name
        );
    }
}

/// Where a VM made without prompts boots from.
#[derive(Debug, PartialEq)]
enum BootDisk {
    Image(String),
    Disk(String),
    Path(String),
}

/// `create <name> [flags]`, read before the defaults the prompts would
/// offer are filled in (those need the server).
#[derive(Debug)]
struct CreateFlags {
    name: String,
    boot: BootDisk,
    disk_size_gib: Option<u32>,
    vcpus: Option<u32>,
    memory_mib: Option<u32>,
    hypervisor: Option<Hypervisor>,
    gpus: Option<GpuRequest>,
    min_vcpus: Option<u32>,
    max_vcpus: Option<u32>,
    cpu_weight: Option<u32>,
    /// A firmware image, a host path, or `none`/`kernel` for kernel boot.
    firmware: Option<String>,
    kernel: Option<String>,
    initrd: Option<String>,
    follow_kernel: bool,
    kernel_args: Option<String>,
    /// `Some(empty)`: `--network none`.
    networks: Option<Vec<String>>,
    credential: Option<String>,
    data_disks: Vec<String>,
    cloud_init: Option<String>,
    devices: Vec<DeviceRequest>,
    vfio: Vec<String>,
    service_account: Option<String>,
    node: Option<String>,
    start: bool,
}

const CREATE_USAGE: &str = "Usage: create <name> (--image I [--disk-size GiB] | --root-disk D | --rootfs PATH) [--vcpus N] [--memory MiB|NG] [--hypervisor ch|qemu|pylon] [--firmware F|PATH|none] [--kernel K|PATH] [--initrd PATH] [--follow-kernel] [--kernel-args A] [--network N|none]... [--credential C|none] [--data-disk D]... [--cloud-init PATH] [--gpus N[:pool][:nvlink]] [--devices pool[:n],...] [--vfio PATH]... [--min-vcpus N] [--max-vcpus N] [--cpu-weight N] [--service-account SA] [--node N] [--start]";

const CREATE_VALUE_FLAGS: &[&str] = &[
    "--image",
    "--disk-size",
    "--root-disk",
    "--rootfs",
    "--vcpus",
    "--memory",
    "--hypervisor",
    "--firmware",
    "--kernel",
    "--initrd",
    "--kernel-args",
    "--network",
    "--credential",
    "--data-disk",
    "--cloud-init",
    "--gpus",
    "--devices",
    "--vfio",
    "--min-vcpus",
    "--max-vcpus",
    "--cpu-weight",
    "--service-account",
    "--node",
];

/// `a, b,,c` → `[a, b, c]`.
fn list_words(s: &str) -> impl Iterator<Item = String> + '_ {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
}

/// Reads `create`'s arguments: the name, then flags. An unknown flag is
/// refused, so a typo doesn't quietly give a VM other than the one asked.
fn parse_create(args: &[&str]) -> Result<CreateFlags, String> {
    let (name, rest) = match args.split_first() {
        Some((n, rest)) if !n.starts_with('-') => (n.to_string(), rest),
        _ => return Err("give the VM's name first".into()),
    };
    let mut boot = Vec::new();
    let mut f = CreateFlags {
        name,
        boot: BootDisk::Path(String::new()),
        disk_size_gib: None,
        vcpus: None,
        memory_mib: None,
        hypervisor: None,
        gpus: None,
        min_vcpus: None,
        max_vcpus: None,
        cpu_weight: None,
        firmware: None,
        kernel: None,
        initrd: None,
        follow_kernel: false,
        kernel_args: None,
        networks: None,
        credential: None,
        data_disks: Vec::new(),
        cloud_init: None,
        devices: Vec::new(),
        vfio: Vec::new(),
        service_account: None,
        node: None,
        start: false,
    };
    let mut i = 0;
    while i < rest.len() {
        let flag = rest[i];
        match flag {
            "--start" => {
                f.start = true;
                i += 1;
                continue;
            }
            "--follow-kernel" => {
                f.follow_kernel = true;
                i += 1;
                continue;
            }
            _ if !CREATE_VALUE_FLAGS.contains(&flag) => {
                return Err(format!("unknown option '{flag}'"));
            }
            _ => {}
        }
        let Some(v) = rest.get(i + 1).map(|v| v.to_string()) else {
            return Err(format!("{flag} needs a value"));
        };
        i += 2;
        let num = |v: &str| {
            v.parse::<u32>()
                .map_err(|_| format!("{flag}: '{v}' is not a whole number"))
        };
        match flag {
            "--image" => boot.push(BootDisk::Image(v)),
            "--root-disk" => boot.push(BootDisk::Disk(v)),
            "--rootfs" => boot.push(BootDisk::Path(v)),
            "--disk-size" => f.disk_size_gib = Some(num(&v)?),
            "--vcpus" => f.vcpus = Some(num(&v)?),
            "--memory" => f.memory_mib = Some(parse_mib(&v)?),
            "--hypervisor" => {
                f.hypervisor = Some(
                    hypervisor_word(&v)
                        .ok_or_else(|| format!("unknown hypervisor '{v}' (ch, qemu or pylon)"))?,
                )
            }
            "--firmware" => f.firmware = Some(v),
            "--kernel" => f.kernel = Some(v),
            "--initrd" => f.initrd = Some(v),
            "--kernel-args" => f.kernel_args = Some(v),
            "--network" => f.networks.get_or_insert_default().extend(list_words(&v)),
            "--credential" => f.credential = Some(v),
            "--data-disk" => f.data_disks.extend(list_words(&v)),
            "--cloud-init" => f.cloud_init = Some(v),
            "--gpus" => f.gpus = parse_gpus(&v)?,
            "--devices" => f.devices.extend(parse_device_requests(&v)),
            "--vfio" => f.vfio.extend(list_words(&v)),
            "--min-vcpus" => f.min_vcpus = Some(num(&v)?),
            "--max-vcpus" => f.max_vcpus = Some(num(&v)?),
            "--cpu-weight" => f.cpu_weight = Some(num(&v)?),
            "--service-account" => f.service_account = Some(v),
            "--node" => f.node = Some(v),
            _ => unreachable!("listed in CREATE_VALUE_FLAGS"),
        }
    }
    f.boot = match boot.len() {
        0 => return Err("give the boot disk: --image, --root-disk or --rootfs".into()),
        1 => boot.remove(0),
        _ => return Err("give only one of --image, --root-disk and --rootfs".into()),
    };
    if f.disk_size_gib.is_some() && !matches!(f.boot, BootDisk::Image(_)) {
        return Err("--disk-size goes with --image".into());
    }
    if let Some(n) = &f.networks
        && n.iter().any(|n| n == "none")
    {
        if n.len() > 1 {
            return Err("--network none goes alone".into());
        }
        f.networks = Some(Vec::new());
    }
    let kernel_path = f.kernel.as_deref().is_some_and(|k| k.starts_with('/'));
    if f.initrd.is_some() && !kernel_path {
        return Err("--initrd goes with a --kernel given as a host path".into());
    }
    if f.follow_kernel && (kernel_path || matches!(f.boot, BootDisk::Path(_))) {
        return Err(
            "--follow-kernel needs a kernel image and a disk (--image or --root-disk)".into(),
        );
    }
    Ok(f)
}

/// `create <name> [flags]`: the VM the prompts would make, without them.
/// What isn't given takes the default the prompt offers.
async fn create_unprompted(client: &ApiClient, args: &[&str]) {
    let f = match parse_create(args) {
        Ok(f) => f,
        Err(e) => {
            err(e);
            return usage(CREATE_USAGE);
        }
    };
    let hypervisor = f
        .hypervisor
        .unwrap_or(Hypervisor::HYPERVISOR_CLOUD_HYPERVISOR);
    let is_pylon = hypervisor == Hypervisor::HYPERVISOR_PYLON;
    // With GPUs, sizes default to the pool's per-device sizing (0).
    let sized = |v: Option<u32>, plain: u32| v.unwrap_or(if f.gpus.is_some() { 0 } else { plain });
    let mut vcpu_count = sized(f.vcpus, 1);
    let mem_size_mib = sized(f.memory_mib, 512);
    // Elastic CPU: on for Pylon, or when any of its settings is given.
    let (min_vcpus, max_vcpus, cpu_weight) =
        if is_pylon || f.min_vcpus.is_some() || f.max_vcpus.is_some() || f.cpu_weight.is_some() {
            let max = f.max_vcpus.unwrap_or(vcpu_count.max(4));
            if is_pylon {
                vcpu_count = max;
            }
            (
                Some(f.min_vcpus.unwrap_or(1)),
                Some(max),
                Some(f.cpu_weight.unwrap_or(100)),
            )
        } else {
            (None, None, None)
        };

    let (firmware, firmware_path) = match f.firmware.as_deref() {
        Some("none" | "kernel") => (None, None),
        Some(p) if p.starts_with('/') => (None, Some(p.to_string())),
        Some(n) => (Some(n.to_string()), None),
        // Without --kernel, the newest firmware for the hypervisor, as the prompt.
        None if f.kernel.is_none() => (
            ready_firmware(client, hypervisor).await.into_iter().next(),
            None,
        ),
        None => (None, None),
    };
    let firmware_boot = firmware.is_some() || firmware_path.is_some();
    if firmware_boot && (f.kernel.is_some() || f.kernel_args.is_some()) {
        err("--kernel and --kernel-args are for kernel boot: add --firmware none");
        return;
    }

    let (mut rootfs_path, mut image, mut root_disk) = (String::new(), None, None);
    match &f.boot {
        BootDisk::Image(i) => image = Some(i.clone()),
        BootDisk::Disk(d) => root_disk = Some(d.clone()),
        BootDisk::Path(p) => rootfs_path = p.clone(),
    }

    let (mut kernel, mut kernel_image_path, mut kernel_from_disk) = (None, String::new(), None);
    let mut initrd_path = None;
    if !firmware_boot {
        let all_images = storage::list_images(client).await.unwrap_or_default();
        let chosen = image
            .as_deref()
            .and_then(|k| all_images.iter().find(|i| i.name == k || i.id == k));
        let (kernels, default) = kernel_choices(&all_images, chosen);
        let k = f.kernel.clone().unwrap_or_else(|| default.to_string());
        if k.is_empty() {
            err("kernel boot needs a kernel: give --kernel, or --firmware for firmware boot");
            return;
        }
        if k.starts_with('/') {
            kernel_image_path = k;
            initrd_path = f.initrd.clone();
        } else {
            let picked = kernels.iter().find(|i| i.name == k || i.id == k).copied();
            warn_foreign_kernel(&all_images, chosen, picked);
            kernel = Some(k);
            kernel_from_disk = f.follow_kernel.then_some(true);
        }
    }

    // The generated seed (and so a credential) comes with firmware boot, a
    // kernel image or a managed root disk (spec 23-kernel-boot.md K4).
    let seeded = firmware_boot || kernel.is_some() || !matches!(f.boot, BootDisk::Path(_));
    if !seeded && (f.cloud_init.is_some() || f.credential.is_some()) {
        err(
            "--cloud-init and --credential need firmware boot, a kernel image, or --image or --root-disk",
        );
        return;
    }
    let credential = if seeded && f.cloud_init.is_none() {
        match f.credential.as_deref() {
            Some(c) => credential_choice(c, None),
            // The signed-in user's own credential, if the project has one.
            None => {
                let names: Vec<String> = storage::list_credentials(client)
                    .await
                    .map(|cs| cs.into_iter().map(|c| c.username).collect())
                    .unwrap_or_default();
                signed_in_name(client).await.filter(|n| names.contains(n))
            }
        }
    } else {
        None
    };

    let networks = match f.networks {
        Some(n) => n,
        None => crate::network::names(client)
            .await
            .into_iter()
            .filter(|n| n == "default")
            .collect(),
    };

    let request = CreateVmRequest {
        gpus: f.gpus.into(),
        name: f.name,
        project: client.project(),
        vcpu_count,
        min_vcpus,
        max_vcpus,
        cpu_weight,
        mem_size_mib,
        kernel_image_path,
        initrd_path,
        kernel,
        kernel_from_disk,
        firmware,
        firmware_path,
        cloud_init_path: f.cloud_init,
        credential,
        service_account: f.service_account,
        rootfs_path,
        image,
        root_disk_size_gib: f.disk_size_gib,
        root_disk,
        data_disks: f.data_disks,
        kernel_args: f.kernel_args,
        hypervisor: hypervisor.into(),
        vfio_devices: f.vfio,
        networks: networks
            .into_iter()
            .map(|network| NetworkAttachment {
                network,
                ..Default::default()
            })
            .collect(),
        devices: f.devices,
        node: f.node,
        power: if f.start {
            PowerState::POWER_STATE_RUNNING.into()
        } else {
            Default::default()
        },
        ..Default::default()
    };
    send_create(client, request).await;
}

/// `kernel <name|id> [--set <image>] [--follow on|off] [--extract]`: a
/// kernel-boot VM's kernel image, the one it boots when following the
/// guest's kernel, and changing them from the next start
/// (spec/23-kernel-boot.md K6, K11, §10.5).
pub async fn kernel(client: &ApiClient, args: &[&str]) {
    let u = "Usage: kernel <name|id> [--set <kernel image>] [--follow on|off] [--extract]";
    let Some(vm_id) = resolve(client, args, u).await else {
        return;
    };
    let rest = &args[1..];
    let set = flag_value(rest, "--set");
    let follow = match flag_value(rest, "--follow") {
        None => None,
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some(_) => return usage(u),
    };
    let images = || async {
        crate::storage::list_images(client)
            .await
            .unwrap_or_default()
    };
    let label = |all: &[glidex_api::proto::glidex::storage::v1::Image], id: &str| {
        all.iter()
            .find(|i| i.id == id)
            .map(
                |i| match i.kernel.as_option().and_then(|k| k.release.as_deref()) {
                    Some(r) => format!("{} ({r})", i.name),
                    None => i.name.clone(),
                },
            )
            .unwrap_or_else(|| id.to_string())
    };

    if has_flag(rest, "--extract") {
        let t = match client.rpc_transport().await {
            Ok(t) => t,
            Err(e) => return err(e.message),
        };
        let svc =
            glidex_api::proto::glidex::storage::v1::ImageServiceClient::new(t.clone(), t.config());
        let img = match svc
            .extract_vm_kernel(
                glidex_api::proto::glidex::storage::v1::ExtractVmKernelRequest {
                    id: vm_id.clone(),
                    ..Default::default()
                },
            )
            .await
        {
            Ok(r) => r
                .into_owned()
                .image
                .as_option()
                .cloned()
                .unwrap_or_default(),
            Err(e) => return err(rpc_err(e)),
        };
        println!(
            "Extracting the kernel on its disk as {}…",
            img.name.yellow()
        );
        for _ in 0..90 {
            let all = images().await;
            match all.iter().find(|i| i.id == img.id) {
                Some(i) if crate::storage::is_ready(i) => {
                    println!(
                        "{} {}; use it with 'kernel {} --set {}'",
                        "Kernel ready:".green(),
                        label(&all, &i.id),
                        args[0],
                        i.name
                    );
                    return;
                }
                Some(i) if i.status.as_option().is_some_and(|s| {
                    s.state.as_known()
                        != Some(glidex_api::proto::glidex::storage::v1::ImageState::IMAGE_STATE_EXTRACTING)
                }) =>
                {
                    return err(format!(
                        "extraction failed: {}",
                        i.status.as_option().map(|s| s.reason.clone()).unwrap_or_default()
                    ));
                }
                None => return err("the kernel image is gone"),
                _ => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
            }
        }
        return err("still extracting; see 'image list'");
    }

    if set.is_some() || follow.is_some() {
        let mut paths = Vec::new();
        let mut change = VmChange::default();
        if let Some(k) = set {
            change.kernel = Some(k.to_string());
            paths.push("kernel".to_string());
        }
        if let Some(f) = follow {
            change.kernel_from_disk = Some(f);
            paths.push("kernel_from_disk".to_string());
        }
        let svc = match rpc(client).await {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let req = UpdateVmRequest {
            id: vm_id.clone(),
            change: change.into(),
            update_mask: buffa_types::google::protobuf::FieldMask {
                paths,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        if let Err(e) = svc.update_vm(req).await {
            return err(rpc_err(e));
        }
        println!("{} from the next start", "Changed:".green());
    }

    let vm = match client.get_vm(&vm_id).await {
        Ok(vm) => vm,
        Err(e) => return err(e),
    };
    let Some(k) = vm.kernel.as_deref() else {
        println!("{} doesn't boot a kernel image", vm.name);
        return;
    };
    let all = images().await;
    println!("Kernel:  {}", label(&all, k));
    if vm.kernel_from_disk == Some(true) {
        println!(
            "Follows the guest's kernel: boots {}",
            label(&all, vm.booted_kernel.as_deref().unwrap_or(k))
        );
        if let Some(p) = vm.previous_kernel.as_deref() {
            println!("Previous: {}", label(&all, p));
        }
    }
}

/// The service accounts the caller can list.
async fn list_service_accounts(
    t: &RpcTransport,
) -> Vec<glidex_api::proto::glidex::access::v1::Token> {
    use glidex_api::proto::glidex::access::v1 as access;
    access::TokenServiceClient::new(t.clone(), t.config())
        .list_tokens(access::ListTokensRequest::default())
        .await
        .map(|r| r.into_owned().tokens)
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.kind.as_known() == Some(access::TokenKind::TOKEN_KIND_SERVICE_ACCOUNT))
        .collect()
}

async fn service_accounts(client: &ApiClient) -> Vec<glidex_api::proto::glidex::access::v1::Token> {
    match client.rpc_transport().await {
        Ok(t) => list_service_accounts(&t).await,
        Err(_) => Vec::new(),
    }
}

/// What to call service account `id`: `name (project …)` from the
/// caller's own list, or that it's one they can't list. A VM names its
/// service account by the token's public id; people read its name (as
/// `federation show` does), and the id itself is never printed.
fn service_account_label(
    accounts: &[glidex_api::proto::glidex::access::v1::Token],
    id: &str,
) -> String {
    match accounts.iter().find(|a| a.id == id) {
        Some(a) => match a.project.as_deref() {
            Some(p) if !p.is_empty() => format!("{} (project {p})", a.name),
            _ => a.name.clone(),
        },
        None => "a service account you can't list".into(),
    }
}

/// `service-account <name|id> [--set <service account> | --none]`: the
/// service account whose glidex tokens the VM's guest gets
/// (spec/29-workload-identity.md W5); a change applies to its next token.
pub async fn service_account(client: &ApiClient, args: &[&str]) {
    let u = "Usage: service-account <name|id> [--set <service account name|id> | --none]";
    let Some(vm_id) = resolve(client, args, u).await else {
        return;
    };
    let rest = &args[1..];
    let vm = match client.get_vm(&vm_id).await {
        Ok(vm) => vm,
        Err(e) => return err(e),
    };
    let t = match client.rpc_transport().await {
        Ok(t) => t,
        Err(e) => return err(e.message),
    };
    let accounts = list_service_accounts(&t).await;
    let label = |id: &str| service_account_label(&accounts, id);
    let change = if has_flag(rest, "--none") {
        Some(None)
    } else if let Some(want) = flag_value(rest, "--set") {
        // By id, or by name in the VM's project.
        let id = accounts
            .iter()
            .find(|a| a.id == want)
            .or_else(|| {
                accounts
                    .iter()
                    .find(|a| a.name == want && a.project.as_deref() == Some(vm.project.as_str()))
            })
            .map(|a| a.id.clone())
            .unwrap_or_else(|| want.to_string());
        Some(Some(id))
    } else if rest.is_empty() {
        None
    } else {
        return usage(u);
    };
    if let Some(c) = change {
        let svc = match rpc(client).await {
            Ok(s) => s,
            Err(e) => return err(e),
        };
        let req = UpdateVmRequest {
            id: vm_id.clone(),
            change: VmChange {
                service_account: c.clone(),
                ..Default::default()
            }
            .into(),
            update_mask: buffa_types::google::protobuf::FieldMask {
                paths: vec!["service_account".into()],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        if let Err(e) = svc.update_vm(req).await {
            return err(rpc_err(e));
        }
        match c {
            Some(id) => println!(
                "{} {} now acts as {}",
                "Changed:".green(),
                vm.name,
                label(&id)
            ),
            None => println!("{} {} has no service account", "Changed:".green(), vm.name),
        }
        return;
    }
    match &vm.service_account {
        Some(id) => println!("Service account: {}", label(id)),
        None => println!("{} has no service account", vm.name),
    }
}

/// `scale <name|id> [--vcpus N] [--min N] [--max N] [--weight N]`:
/// show or update VM CPU sizing and microsecond elasticity policy.
pub async fn scale(client: &ApiClient, args: &[&str]) {
    let u = "Usage: scale <name|id> [--vcpus N] [--min N] [--max N] [--weight N]";
    let Some(vm_id) = resolve(client, args, u).await else {
        return;
    };
    let rest = &args[1..];
    let vm = match client.get_vm(&vm_id).await {
        Ok(vm) => vm,
        Err(e) => return err(e),
    };

    let parse_flag = |name: &str| -> Result<Option<u32>, String> {
        if let Some(val) = flag_value(rest, name) {
            val.parse::<u32>()
                .map(Some)
                .map_err(|_| format!("Invalid value for {name}: {val}"))
        } else {
            Ok(None)
        }
    };

    let vcpus_opt = match parse_flag("--vcpus") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let min_opt = match parse_flag("--min") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let max_opt = match parse_flag("--max") {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let weight_opt = match parse_flag("--weight") {
        Ok(v) => v,
        Err(e) => return err(e),
    };

    let has_any_flag =
        vcpus_opt.is_some() || min_opt.is_some() || max_opt.is_some() || weight_opt.is_some();

    if !has_any_flag {
        if !rest.is_empty() {
            return usage(u);
        }
        println!("VM:         {}", vm.name);
        println!("Hypervisor: {}", storage::hypervisor_name(&vm.hypervisor));
        if let (Some(min), Some(max)) = (vm.min_vcpus, vm.max_vcpus) {
            if min < max {
                println!(
                    "vCPUs:      {} (elastic: {}–{}, weight: {})",
                    vm.vcpu_count,
                    min,
                    max,
                    vm.cpu_weight.unwrap_or(100)
                );
            } else {
                println!("vCPUs:      {}", vm.vcpu_count);
            }
        } else {
            println!("vCPUs:      {}", vm.vcpu_count);
        }
        return;
    }

    let svc = match rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };

    let mut change = VmChange::default();
    let mut paths = Vec::new();

    if let Some(v) = vcpus_opt {
        change.vcpu_count = v;
        paths.push("vcpu_count".into());
    }
    if let Some(min) = min_opt {
        change.min_vcpus = Some(min);
        paths.push("min_vcpus".into());
    }
    if let Some(max) = max_opt {
        change.max_vcpus = Some(max);
        paths.push("max_vcpus".into());
    }
    if let Some(w) = weight_opt {
        change.cpu_weight = Some(w);
        paths.push("cpu_weight".into());
    }

    let req = UpdateVmRequest {
        id: vm_id.clone(),
        change: change.into(),
        update_mask: buffa_types::google::protobuf::FieldMask {
            paths,
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };

    if let Err(e) = svc.update_vm(req).await {
        return err(rpc_err(e));
    }

    println!("{} CPU policy updated for {}", "Success:".green(), vm.name);
}

/// `4096`, `4096M`, `8G`, `8GiB` → MiB; 0 and fractions refused.
fn parse_mib(s: &str) -> Result<u32, String> {
    let t = s.trim();
    let lower = t.to_ascii_lowercase();
    let (num, mul) = if let Some(n) = lower.strip_suffix("gib").or(lower.strip_suffix("g")) {
        (n, 1024)
    } else if let Some(n) = lower.strip_suffix("mib").or(lower.strip_suffix("m")) {
        (n, 1)
    } else {
        (lower.as_str(), 1)
    };
    match num
        .trim()
        .parse::<u32>()
        .ok()
        .and_then(|n| n.checked_mul(mul))
    {
        Some(m) if m > 0 => Ok(m),
        _ => Err(format!("Invalid memory size: {t} (MiB, or e.g. 8G)")),
    }
}

/// Said after a change that waits for the VM's next start.
fn note_next_start(vm: &Vm) {
    if vm.restart_required == Some(true) {
        println!(
            "  {}",
            "The VM is running: this applies at its next start (stop and start it).".yellow()
        );
    }
}

/// `memory <name|id> [<size>]`: show, or set `mem_size_mib`.
pub async fn memory(client: &ApiClient, args: &[&str]) {
    let u = "Usage: memory <name|id> [<MiB>|<N>G]";
    if args.len() > 2 {
        return usage(u);
    }
    let Some(vm_id) = resolve(client, args, u).await else {
        return;
    };
    let Some(size) = args.get(1) else {
        match client.get_vm(&vm_id).await {
            Ok(vm) => println!("{}: {} MiB", vm.name, vm.mem_size_mib),
            Err(e) => err(e),
        }
        return;
    };
    let mib = match parse_mib(size) {
        Ok(m) => m,
        Err(e) => return err(e),
    };
    let svc = match rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let req = UpdateVmRequest {
        id: vm_id,
        change: VmChange {
            mem_size_mib: mib,
            ..Default::default()
        }
        .into(),
        update_mask: buffa_types::google::protobuf::FieldMask {
            paths: vec!["mem_size_mib".into()],
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    match svc.update_vm(req).await {
        Ok(r) => {
            let vm = r.into_owned().vm.as_option().cloned().unwrap_or_default();
            println!(
                "{} {} has {} MiB",
                "Changed:".green(),
                if vm.name.is_empty() {
                    args[0]
                } else {
                    &vm.name
                },
                vm.mem_size_mib
            );
            note_next_start(&vm);
        }
        Err(e) => err(rpc_err(e)),
    }
}

/// `rename <name|id> <new-name>` (spec/12-reconciliation.md §7.7).
pub async fn rename(client: &ApiClient, args: &[&str]) {
    let u = "Usage: rename <name|id> <new-name>";
    let [_, name] = args else {
        return usage(u);
    };
    let Some(vm_id) = resolve(client, args, u).await else {
        return;
    };
    let svc = match rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let req = UpdateVmRequest {
        id: vm_id,
        change: VmChange {
            name: Some(name.to_string()),
            ..Default::default()
        }
        .into(),
        update_mask: buffa_types::google::protobuf::FieldMask {
            paths: vec!["name".into()],
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    match svc.update_vm(req).await {
        Ok(r) => {
            let vm = r.into_owned().vm.as_option().cloned().unwrap_or_default();
            println!("{} {} is now {}", "Renamed:".green(), args[0], vm.name);
        }
        Err(e) => err(rpc_err(e)),
    }
}

/// `attach-disk <name|id> <disk>` and `detach-disk …`: a data disk, by
/// name or id.
pub async fn disk(client: &ApiClient, attach: bool, args: &[&str]) {
    let u = if attach {
        "Usage: attach-disk <name|id> <disk>"
    } else {
        "Usage: detach-disk <name|id> <disk>"
    };
    let (Some(disk), 2) = (args.get(1), args.len()) else {
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
        svc.attach_disk(AttachDiskRequest {
            id: vm_id,
            disk: disk.to_string(),
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().vm.as_option().cloned())
    } else {
        svc.detach_disk(DetachDiskRequest {
            id: vm_id,
            disk: disk.to_string(),
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().vm.as_option().cloned())
    };
    match res {
        Ok(vm) => {
            let vm = vm.unwrap_or_default();
            println!(
                "{} Disk {} {} {}",
                "Success:".green(),
                disk,
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
            note_next_start(&vm);
        }
        Err(e) => err(rpc_err(e)),
    }
}

/// `attach-network <name|id> <network> [--mac <mac>] [--no-port-security]`
/// and `detach-network <name|id> <network> [--mac <mac>]`: a NIC, by its
/// network's name or id (and its MAC, among several on one network).
/// `--no-port-security`: no checks on the new NIC's port
/// (spec/09-networking.md decision 19).
pub async fn network(client: &ApiClient, attach: bool, args: &[&str]) {
    let u = if attach {
        "Usage: attach-network <name|id> <network> [--mac <mac>] [--no-port-security]"
    } else {
        "Usage: detach-network <name|id> <network> [--mac <mac>]"
    };
    if has_flag(args, "--mac") && flag_value(args, "--mac").is_none() {
        return usage(u);
    }
    let mac = flag_value(args, "--mac").map(String::from);
    let no_port_security = attach && has_flag(args, "--no-port-security");
    let pos = crate::admin::positional(args, &["--mac"]);
    let (Some(net), 2) = (pos.get(1).copied(), pos.len()) else {
        return usage(u);
    };
    let Some(vm_id) = resolve(client, &pos, u).await else {
        return;
    };
    let svc = match rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let res = if attach {
        svc.attach_network(AttachNetworkRequest {
            id: vm_id,
            network: net.to_string(),
            mac,
            port_security: no_port_security.then_some(false),
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().vm.as_option().cloned())
    } else {
        svc.detach_network(DetachNetworkRequest {
            id: vm_id,
            network: net.to_string(),
            mac,
            ..Default::default()
        })
        .await
        .map(|r| r.into_owned().vm.as_option().cloned())
    };
    match res {
        Ok(vm) => {
            let vm = vm.unwrap_or_default();
            println!(
                "{} Network {} {} {}",
                "Success:".green(),
                net,
                if attach {
                    "attached to"
                } else {
                    "detached from"
                },
                if vm.name.is_empty() { pos[0] } else { &vm.name }
            );
            note_next_start(&vm);
        }
        Err(e) => err(rpc_err(e)),
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn device_requests_parse() {
        let r = parse_device_requests("a100:2:switch, cx7 ,");
        assert_eq!(r.len(), 2);
        assert_eq!(
            (r[0].pool.as_str(), r[0].count, r[0].same_switch),
            ("a100", 2, true)
        );
        assert_eq!(
            (r[1].pool.as_str(), r[1].count, r[1].same_switch),
            ("cx7", 1, false)
        );
        assert!(parse_device_requests("").is_empty());
    }

    #[test]
    fn gpu_requests_parse() {
        assert_eq!(parse_gpus("").unwrap(), None);
        let g = parse_gpus("4").unwrap().unwrap();
        assert_eq!((g.count, g.pool.clone(), g.nvlink), (4, None, None));
        let g = parse_gpus("8:a100:nvlink").unwrap().unwrap();
        assert_eq!(
            (g.count, g.pool.as_deref(), g.nvlink),
            (8, Some("a100"), Some(true))
        );
        let g = parse_gpus("2:nvlink").unwrap().unwrap();
        assert_eq!((g.pool.clone(), g.nvlink), (None, Some(true)));
        assert!(parse_gpus("four").is_err());
        assert!(parse_gpus("2:a100:t4").is_err());
    }

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
    fn memory_sizes() {
        assert_eq!(parse_mib("4096"), Ok(4096));
        assert_eq!(parse_mib("4096M"), Ok(4096));
        assert_eq!(parse_mib("8G"), Ok(8192));
        assert_eq!(parse_mib("8gib"), Ok(8192));
        assert!(parse_mib("0").is_err());
        assert!(parse_mib("1.5G").is_err());
        assert!(parse_mib("lots").is_err());
        assert!(parse_mib("5000000G").is_err());
    }

    #[test]
    fn create_flags() {
        let f = parse_create(&[
            "web",
            "--image",
            "noble",
            "--disk-size",
            "20",
            "--vcpus",
            "4",
            "--memory",
            "8G",
            "--hypervisor",
            "qemu",
            "--network",
            "a,b",
            "--network",
            "c",
            "--data-disk",
            "d1",
            "--gpus",
            "2:a100",
            "--start",
        ])
        .unwrap();
        assert_eq!(f.name, "web");
        assert_eq!(f.boot, BootDisk::Image("noble".into()));
        assert_eq!(
            (f.disk_size_gib, f.vcpus, f.memory_mib),
            (Some(20), Some(4), Some(8192))
        );
        assert_eq!(f.hypervisor, Some(Hypervisor::HYPERVISOR_QEMU));
        assert_eq!(
            f.networks.as_deref(),
            Some(&["a", "b", "c"].map(String::from)[..])
        );
        assert_eq!(f.data_disks, ["d1"]);
        assert_eq!(f.gpus.map(|g| g.count), Some(2));
        assert!(f.start);
        // Defaults are left to the server's answers.
        let f = parse_create(&["web", "--rootfs", "/srv/root.img"]).unwrap();
        assert_eq!((f.vcpus, f.networks, f.firmware), (None, None, None));
        let f = parse_create(&["web", "--root-disk", "d", "--network", "none"]).unwrap();
        assert_eq!(f.networks, Some(Vec::new()));
        let f = parse_create(&[
            "k",
            "--rootfs",
            "/r",
            "--kernel",
            "/boot/vmlinuz",
            "--initrd",
            "/boot/initrd",
        ])
        .unwrap();
        assert_eq!(f.initrd.as_deref(), Some("/boot/initrd"));
    }

    #[test]
    fn create_flags_refused() {
        let refused = |args: &[&str]| parse_create(args).unwrap_err();
        assert!(refused(&[]).contains("name"));
        assert!(refused(&["--image", "x"]).contains("name"));
        assert!(refused(&["web"]).contains("boot disk"));
        assert!(refused(&["web", "--image", "a", "--rootfs", "/b"]).contains("only one"));
        assert!(
            refused(&["web", "--image", "a", "--vcpu", "2"]).contains("unknown option '--vcpu'")
        );
        assert!(refused(&["web", "--image", "a", "--vcpus"]).contains("needs a value"));
        assert!(refused(&["web", "--image", "a", "--vcpus", "two"]).contains("whole number"));
        assert!(refused(&["web", "--image", "a", "--memory", "lots"]).contains("memory"));
        assert!(refused(&["web", "--image", "a", "--hypervisor", "xen"]).contains("hypervisor"));
        assert!(refused(&["web", "--root-disk", "d", "--disk-size", "9"]).contains("--disk-size"));
        assert!(refused(&["web", "--image", "a", "--network", "none,b"]).contains("alone"));
        assert!(refused(&["web", "--image", "a", "--initrd", "/i"]).contains("--initrd"));
        assert!(refused(&["web", "--rootfs", "/r", "--follow-kernel"]).contains("--follow-kernel"));
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
