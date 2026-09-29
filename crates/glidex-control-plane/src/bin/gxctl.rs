use clap::Parser;
use colored::Colorize;
use glidex_control_plane::hypervisor::cloud_hypervisor::default_firmware_path;
use nix::sys::termios::{self, LocalFlags, SetArg, Termios};
use reqwest::Client;
use rustyline::completion::{unescape, Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Editor, Helper};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tabled::{Table, Tabled};

#[derive(Parser)]
#[command(name = "gxctl")]
#[command(about = "Interactive CLI for Glidex Control Plane")]
struct Cli {
    /// API server URL
    #[arg(short, long, default_value = "http://localhost:8841")]
    server: String,
}

#[derive(Debug, Deserialize, Tabled)]
struct VmResponse {
    id: String,
    name: String,
    state: String,
    vcpu_count: u8,
    mem_size_mib: u32,
    hypervisor: String,
    #[tabled(skip)]
    #[serde(default)]
    vfio_devices: Vec<String>,
    #[tabled(skip)]
    #[serde(default)]
    nics: Vec<NicInfo>,
}

#[derive(Debug, Deserialize, Clone)]
struct NicInfo {
    network: String,
    mac: String,
    #[serde(default)]
    port: Option<String>,
    #[serde(default)]
    ipv4: Option<String>,
}

#[derive(Debug, Deserialize, Tabled)]
struct NetworkRow {
    name: String,
    mode: String,
    bridge: String,
    port_type: String,
    #[tabled(display_with = "display_opt_u16")]
    #[serde(default)]
    vlan: Option<u16>,
}

fn display_opt_u16(o: &Option<u16>) -> String {
    o.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string())
}

/// `--flag value` from a command's arguments.
fn flag_value<'a>(args: &'a [&'a str], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| *a == flag).and_then(|i| args.get(i + 1)).copied()
}

fn has_flag(args: &[&str], flag: &str) -> bool {
    args.contains(&flag)
}

async fn handle_network_add(client: &CliClient, args: &[&str]) {
    let Some(name) = args.first().filter(|a| !a.starts_with("--")) else {
        println!("{}", "Usage: network-add <name> [--nat [--subnet CIDR] | --isolated | --bridged <bridge>] [--vhost-user] [--vlan N] [--bridge NAME]".yellow());
        return;
    };
    let (mode, bridge) = if let Some(b) = flag_value(args, "--bridged") {
        ("bridged", Some(b))
    } else if has_flag(args, "--isolated") {
        ("isolated", flag_value(args, "--bridge"))
    } else {
        ("nat", flag_value(args, "--bridge"))
    };
    let mut body = serde_json::json!({
        "name": name,
        "mode": mode,
        "port_type": if has_flag(args, "--vhost-user") { "vhost_user" } else { "tap" },
    });
    if let Some(b) = bridge {
        body["bridge"] = serde_json::json!(b);
    }
    if let Some(s) = flag_value(args, "--subnet") {
        body["subnet"] = serde_json::json!(s);
    }
    if let Some(v) = flag_value(args, "--vlan") {
        match v.parse::<u16>() {
            Ok(v) => body["vlan"] = serde_json::json!(v),
            Err(_) => {
                println!("{} --vlan must be a number", "Error:".red());
                return;
            }
        }
    }
    match client.request_json::<NetworkRow>(reqwest::Method::POST, "/networks", Some(body)).await {
        Ok(n) => println!("{} {} ({} on {})", "Network created:".green(), n.name.yellow(), n.mode, n.bridge),
        Err(e) => println!("{} {}", "Error:".red(), e),
    }
}

async fn handle_uplink_add(client: &CliClient, args: &[&str]) {
    let usage = "Usage: uplink-add <bridge> (--kernel <if> | --afxdp <if> [--xdp-mode best_effort|native|native_with_zerocopy|generic] [--rxq N] | --dpdk <bdf> --name <port> [--rxq N]) [--migrate-ip] [--force]";
    let Some(bridge) = args.first().filter(|a| !a.starts_with("--")) else {
        println!("{}", usage.yellow());
        return;
    };
    let rxq: u16 = flag_value(args, "--rxq").and_then(|v| v.parse().ok()).unwrap_or(1);
    let mut body = if let Some(ifname) = flag_value(args, "--kernel") {
        serde_json::json!({"name": ifname, "kind": "kernel", "ifname": ifname})
    } else if let Some(ifname) = flag_value(args, "--afxdp") {
        serde_json::json!({"name": ifname, "kind": "afxdp", "ifname": ifname, "n_rxq": rxq,
            "xdp_mode": flag_value(args, "--xdp-mode").unwrap_or("best_effort")})
    } else if let Some(pci) = flag_value(args, "--dpdk") {
        let Some(name) = flag_value(args, "--name") else {
            println!("{}", usage.yellow());
            return;
        };
        serde_json::json!({"name": name, "kind": "dpdk", "pci": pci, "n_rxq": rxq})
    } else {
        println!("{}", usage.yellow());
        return;
    };
    body["migrate_ip"] = serde_json::json!(has_flag(args, "--migrate-ip"));
    body["confirm"] = serde_json::json!(has_flag(args, "--force"));
    let path = format!("/ovs/bridges/{}/uplinks", bridge);
    let res = match client.request_json::<serde_json::Value>(reqwest::Method::POST, &path, Some(body)).await {
        Ok(r) => r,
        Err(e) => {
            if e.contains("host_interface_in_use") {
                println!("{} {}\n  The NIC is in use by the host. Re-run with --migrate-ip --force to move its IP configuration onto the bridge (rolled back automatically if the gateway stops answering or this CLI can't confirm).", "Error:".red(), e);
            } else {
                println!("{} {}", "Error:".red(), e);
            }
            return;
        }
    };
    let name = res["record"]["spec"]["name"].as_str().unwrap_or("?").to_string();
    for w in res["live"]["warnings"].as_array().into_iter().flatten() {
        println!("{} {}", "Warning:".yellow(), w.as_str().unwrap_or(""));
    }
    if res["phase"] == "pending_commit" {
        println!("IP configuration moved to {}; checking the API is still reachable...", bridge);
        let token = res["record"]["pending"]["token"].as_str().unwrap_or("").to_string();
        if client.health_check().await.is_err() {
            println!("{} API unreachable; glidex-netd will roll the change back automatically.", "Error:".red());
            return;
        }
        let commit = format!("/ovs/bridges/{}/uplinks/{}/commit", bridge, name);
        match client.request_json::<serde_json::Value>(reqwest::Method::POST, &commit, Some(serde_json::json!({"token": token}))).await {
            Ok(_) => println!("{} uplink {} on {} (IP migrated)", "Committed:".green(), name, bridge),
            Err(e) => println!("{} {} (the change will be rolled back)", "Error:".red(), e),
        }
    } else {
        println!("{} uplink {} on {}", "Added:".green(), name, bridge);
    }
}

async fn handle_ovs(client: &CliClient, args: &[&str]) {
    match args.first().copied() {
        Some("status") | None => {
            match client.request_json::<serde_json::Value>(reqwest::Method::GET, "/ovs/status", None).await {
                Ok(s) => {
                    let netd = &s["netd"];
                    if netd["available"] != true {
                        println!("{} glidex-netd unavailable: {}", "netd:".bold(), netd["error"].as_str().unwrap_or("unknown").red());
                        return;
                    }
                    println!("{} available (access: {})", "netd:".bold(), netd["access"].as_str().unwrap_or("?"));
                    let h = &s["host"];
                    let ovs = if h["ovs_installed"] == true {
                        format!(
                            "{} ({})",
                            h["ovs_version"].as_str().unwrap_or("?"),
                            if h["ovs_running"] == true { "running".green() } else { "not running".red() }
                        )
                    } else {
                        "not installed".red().to_string()
                    };
                    println!("{} {}", "Open vSwitch:".bold(), ovs);
                    println!("{} {}", "dnsmasq:".bold(), if h["dnsmasq"] == true { "yes".green() } else { "missing".red() });
                    println!("{} {}", "DPDK initialized:".bold(), h["dpdk_initialized"]);
                    if let Some(combos) = h["combinations"].as_array() {
                        println!("{}", "Combinations:".bold());
                        for c in combos {
                            let missing: Vec<&str> = c["missing"].as_array().map(|m| m.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
                            println!(
                                "  {} {} {}",
                                c["id"].as_str().unwrap_or("?"),
                                if c["available"] == true { "available".green().to_string() } else { format!("missing: {}", missing.join(", ")).yellow().to_string() },
                                c["description"].as_str().unwrap_or("").dimmed()
                            );
                        }
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }
        Some("install") => {
            let profile = flag_value(args, "--profile").unwrap_or("kernel");
            let body = serde_json::json!({
                "profile": profile,
                "source_build": has_flag(args, "--source"),
                "confirm": has_flag(args, "--force"),
            });
            println!("Installing Open vSwitch ({} profile); this can take a few minutes...", profile);
            match client.request_json::<serde_json::Value>(reqwest::Method::POST, "/ovs/install", Some(body)).await {
                Ok(r) if r["changed"] == true => println!("{} Open vSwitch {}", "Installed:".green(), r["ovs_version"].as_str().unwrap_or("?")),
                Ok(r) => println!("{} Open vSwitch {} already satisfies the profile", "Nothing to do:".green(), r["ovs_version"].as_str().unwrap_or("?")),
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }
        Some("dpdk-init") => {
            let body = serde_json::json!({
                "socket_mem": flag_value(args, "--socket-mem").unwrap_or("1024"),
                "pmd_cpu_mask": flag_value(args, "--pmd-cpu-mask"),
                "confirm": has_flag(args, "--force"),
            });
            println!("Enabling DPDK in ovs-vswitchd (restarts it)...");
            match client.request_json::<()>(reqwest::Method::POST, "/ovs/dpdk-init", Some(body)).await {
                Ok(()) => println!("{} DPDK initialized", "OK:".green()),
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }
        Some(other) => println!("{} unknown 'ovs {}'; use 'ovs status', 'ovs install' or 'ovs dpdk-init'", "Error:".red(), other),
    }
}

#[derive(Debug, Serialize)]
struct NetworkAttachmentReq {
    network: String,
}

#[derive(Debug, Deserialize)]
struct CredentialInfo {
    username: String,
    has_password: bool,
    #[serde(default)]
    ssh_authorized_keys: Vec<String>,
    updated_at: u64,
}

#[derive(Tabled)]
struct CredentialRow {
    username: String,
    password: &'static str,
    ssh_keys: usize,
    updated: String,
}

impl From<&CredentialInfo> for CredentialRow {
    fn from(c: &CredentialInfo) -> Self {
        Self {
            username: c.username.clone(),
            password: if c.has_password { "set" } else { "-" },
            ssh_keys: c.ssh_authorized_keys.len(),
            updated: format_unix_time(c.updated_at),
        }
    }
}

/// Request bodies carrying a plaintext password deliberately don't derive
/// `Debug`, so the password can't end up in a log line by accident.
#[derive(Serialize)]
struct CreateCredentialRequest {
    username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    ssh_authorized_keys: Vec<String>,
}

#[derive(Serialize)]
struct UpdateCredentialRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ssh_authorized_keys: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct CreateVmRequest {
    name: String,
    vcpu_count: u8,
    mem_size_mib: u32,
    kernel_image_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    firmware_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cloud_init_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<String>,
    rootfs_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel_args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hypervisor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vfio_devices: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    networks: Option<Vec<NetworkAttachmentReq>>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: String,
    message: String,
    #[serde(default)]
    details: serde_json::Value,
}

#[derive(Debug, Deserialize, Tabled)]
struct PciDeviceInfo {
    address: String,
    vendor_id: String,
    device_id: String,
    class_id: String,
    #[tabled(display_with = "display_option")]
    driver: Option<String>,
    #[tabled(display_with = "display_option")]
    iommu_group: Option<String>,
    #[tabled(skip)]
    sysfs_path: String,
}

fn display_option(o: &Option<String>) -> String {
    match o {
        Some(s) => s.clone(),
        None => "-".to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct ConsoleInfo {
    #[allow(dead_code)]
    vm_id: String,
    console_socket_path: String,
    log_path: String,
    available: bool,
}

struct CliClient {
    client: Client,
    base_url: String,
}

impl CliClient {
    fn new(base_url: String) -> Self {
        Self {
            client: Client::new(),
            base_url,
        }
    }

    async fn list_vms(&self) -> Result<Vec<VmResponse>, String> {
        let resp = self
            .client
            .get(format!("{}/vms", self.base_url))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn get_vm(&self, id: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .get(format!("{}/vms/{}", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn create_vm(&self, request: CreateVmRequest) -> Result<VmResponse, String> {
        let resp = self
            .client
            .post(format!("{}/vms", self.base_url))
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn start_vm(&self, id: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .post(format!("{}/vms/{}/start", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn stop_vm(&self, id: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .post(format!("{}/vms/{}/stop", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn pause_vm(&self, id: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .post(format!("{}/vms/{}/pause", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn delete_vm(&self, id: &str) -> Result<(), String> {
        let resp = self
            .client
            .delete(format!("{}/vms/{}", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    /// Generic JSON request; errors include `details.impact` /
    /// `details.missing` when the API provides them.
    async fn request_json<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, String> {
        let mut req = self.client.request(method, format!("{}{}", self.base_url, path));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.map_err(|e| format!("Request failed: {}", e))?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return serde_json::from_value(serde_json::Value::Null)
                .map_err(|e| format!("Failed to parse response: {}", e));
        }
        if resp.status().is_success() {
            resp.json().await.map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let err: ApiError = resp.json().await.map_err(|e| format!("Failed to parse error: {}", e))?;
            let mut msg = format!("{}: {}", err.error, err.message);
            if let Some(impact) = err.details.get("impact").and_then(|v| v.as_str()) {
                msg.push_str(&format!("\n  Impact: {}\n  Re-run with --force to proceed.", impact));
            }
            if let Some(missing) = err.details.get("missing").and_then(|v| v.as_array()) {
                let items: Vec<&str> = missing.iter().filter_map(|m| m.as_str()).collect();
                msg.push_str(&format!("\n  Missing on this host: {}", items.join(", ")));
            }
            Err(msg)
        }
    }

    async fn list_credentials(&self) -> Result<Vec<CredentialInfo>, String> {
        let resp = self
            .client
            .get(format!("{}/credentials", self.base_url))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;
        json_or_api_error(resp).await
    }

    async fn create_credential(
        &self,
        request: &CreateCredentialRequest,
    ) -> Result<CredentialInfo, String> {
        let resp = self
            .client
            .post(format!("{}/credentials", self.base_url))
            .json(request)
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;
        json_or_api_error(resp).await
    }

    async fn update_credential(
        &self,
        username: &str,
        request: &UpdateCredentialRequest,
    ) -> Result<CredentialInfo, String> {
        let resp = self
            .client
            .put(format!("{}/credentials/{}", self.base_url, username))
            .json(request)
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;
        json_or_api_error(resp).await
    }

    async fn delete_credential(&self, username: &str) -> Result<(), String> {
        let resp = self
            .client
            .delete(format!("{}/credentials/{}", self.base_url, username))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(api_error_message(resp).await)
        }
    }

    async fn health_check(&self) -> Result<(), String> {
        let resp = self
            .client
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err("Health check failed".to_string())
        }
    }

    async fn get_console_info(&self, id: &str) -> Result<ConsoleInfo, String> {
        let resp = self
            .client
            .get(format!("{}/vms/{}/console", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn list_pci_devices(&self) -> Result<Vec<PciDeviceInfo>, String> {
        let resp = self
            .client
            .get(format!("{}/pci-devices", self.base_url))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            Err("Failed to list PCI devices".to_string())
        }
    }

    async fn attach_device(&self, vm_id: &str, device_path: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .post(format!("{}/vms/{}/devices", self.base_url, vm_id))
            .json(&serde_json::json!({ "device_path": device_path }))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn detach_device(&self, vm_id: &str, device_path: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .delete(format!("{}/vms/{}/devices", self.base_url, vm_id))
            .json(&serde_json::json!({ "device_path": device_path }))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    /// Resolve a VM identifier (name or ID) to an ID.
    /// First tries to use it as an ID, then searches by name.
    async fn resolve_vm(&self, name_or_id: &str) -> Result<String, String> {
        // First, try to get VM by ID directly
        if let Ok(vm) = self.get_vm(name_or_id).await {
            return Ok(vm.id);
        }

        // If that fails, search by name
        let vms = self.list_vms().await?;
        let matches: Vec<_> = vms.iter().filter(|vm| vm.name == name_or_id).collect();

        match matches.len() {
            0 => Err(format!("VM '{}' not found", name_or_id)),
            1 => Ok(matches[0].id.clone()),
            _ => {
                let ids: Vec<_> = matches.iter().map(|vm| vm.id.as_str()).collect();
                Err(format!(
                    "Multiple VMs found with name '{}'. Use ID instead: {}",
                    name_or_id,
                    ids.join(", ")
                ))
            }
        }
    }
}

fn print_help() {
    println!("{}", "Available commands:".bold());
    println!("  {}              - List all VMs", "list".cyan());
    println!("  {}    - Show VM details", "get <name|id>".cyan());
    println!(
        "  {}           - Create a new VM (interactive)",
        "create".cyan()
    );
    println!("  {}  - Start a VM", "start <name|id>".cyan());
    println!("  {}   - Stop a VM", "stop <name|id>".cyan());
    println!("  {}  - Pause a VM", "pause <name|id>".cyan());
    println!("  {} - Connect to VM console (interactive)", "connect <name|id>".cyan());
    println!("  {}     - Show VM serial console log", "log <name|id>".cyan());
    println!("  {} - Delete a VM", "delete <name|id>".cyan());
    println!("  {}               - List host PCI devices", "pci".cyan());
    println!(
        "  {}     - Show detailed info (incl. sysfs path) for one device",
        "pci <address>".cyan()
    );
    println!(
        "  {} - Attach PCI device to VM",
        "attach-device <name|id> <path>".cyan()
    );
    println!(
        "  {} - Detach PCI device from VM",
        "detach-device <name|id> <path>".cyan()
    );
    println!("  {}       - List stored guest login credentials", "credentials".cyan());
    println!("  {}    - Add a credential (interactive)", "credential-add".cyan());
    println!("  {} - Change a credential's password", "credential-passwd <user>".cyan());
    println!("  {} - Replace a credential's SSH keys", "credential-keys <user>".cyan());
    println!("  {}  - Delete a credential", "credential-rm <user>".cyan());
    println!("  {}          - List networks", "networks".cyan());
    println!("  {} - Create a network (default: NAT)", "network-add <name> [--nat|--isolated|--bridged <br>]".cyan());
    println!("  {}  - Delete a network", "network-rm <name>".cyan());
    println!("  {}           - List glidex OVS bridges", "bridges".cyan());
    println!("  {}  - List a bridge's uplinks", "uplinks <bridge>".cyan());
    println!("  {} - Add an uplink (kernel/AF_XDP/DPDK)", "uplink-add <bridge> --kernel <if> [--migrate-ip] [--force]".cyan());
    println!("  {} - Remove an uplink, restoring the NIC", "uplink-rm <bridge> <name>".cyan());
    println!("  {}        - Host networking status", "ovs status".cyan());
    println!("  {} - Install Open vSwitch", "ovs install [--profile kernel|dpdk] [--force]".cyan());
    println!("  {}            - Check API server health", "health".cyan());
    println!("  {}              - Show this help", "help".cyan());
    println!("  {}              - Exit the CLI", "exit".cyan());
    println!();
    println!(
        "{} You can use either VM name or ID for commands.",
        "Note:".dimmed()
    );
}

async fn api_error_message(resp: reqwest::Response) -> String {
    match resp.json::<ApiError>().await {
        Ok(error) => format!("{}: {}", error.error, error.message),
        Err(e) => format!("Failed to parse error: {}", e),
    }
}

async fn json_or_api_error<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, String> {
    if resp.status().is_success() {
        resp.json()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))
    } else {
        Err(api_error_message(resp).await)
    }
}

fn format_unix_time(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant), to avoid pulling in a date crate.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        year,
        month,
        day,
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Read a line without echoing it when stdin is a terminal.
fn prompt_hidden(msg: &str) -> String {
    print!("{}", msg);
    io::stdout().flush().unwrap();
    let stdin = io::stdin();
    let fd = stdin.as_fd();
    let saved = termios::tcgetattr(fd).ok();
    if let Some(orig) = &saved {
        let mut quiet = orig.clone();
        quiet.local_flags.remove(LocalFlags::ECHO);
        let _ = termios::tcsetattr(fd, SetArg::TCSANOW, &quiet);
    }
    let mut input = String::new();
    let _ = stdin.read_line(&mut input);
    if let Some(orig) = &saved {
        restore_terminal(fd, orig);
        println!();
    }
    input.trim_end_matches(['\r', '\n']).to_string()
}

/// Ask for a new password twice. `Ok(None)` when left empty and `allow_empty`.
fn prompt_new_password(allow_empty: bool) -> Result<Option<String>, String> {
    let first = prompt_hidden("Password: ");
    if first.is_empty() {
        return if allow_empty {
            Ok(None)
        } else {
            Err("password is required".to_string())
        };
    }
    if prompt_hidden("Confirm password: ") != first {
        return Err("passwords do not match".to_string());
    }
    Ok(Some(first))
}

/// Read SSH public keys from comma-separated `.pub` file paths. Refuses
/// anything that looks like a private key so it is never sent anywhere.
fn read_public_key_files(input: &str) -> Result<Vec<String>, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut keys = Vec::new();
    for path in input.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let path = match path.strip_prefix("~/") {
            Some(rest) => format!("{}/{}", home, rest),
            None => path.to_string(),
        };
        let contents =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {}", path, e))?;
        if contents.contains("PRIVATE KEY") {
            return Err(format!(
                "{} is a private key; give the matching .pub file instead",
                path
            ));
        }
        keys.extend(
            contents
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string),
        );
    }
    Ok(keys)
}

async fn handle_credential_add(client: &CliClient) {
    let username = prompt("Username: ");
    if username.is_empty() {
        println!("{}", "Error: username is required".red());
        return;
    }
    println!("{}", "Leave the password empty for SSH-key-only login.".dimmed());
    let password = match prompt_new_password(true) {
        Ok(p) => p,
        Err(e) => {
            println!("{} {}", "Error:".red(), e);
            return;
        }
    };
    let keys = match prompt_path_optional("SSH public key files (optional, comma-separated, e.g. ~/.ssh/id_ed25519.pub): ")
        .map(|s| read_public_key_files(&s))
        .transpose()
    {
        Ok(k) => k.unwrap_or_default(),
        Err(e) => {
            println!("{} {}", "Error:".red(), e);
            return;
        }
    };
    let request = CreateCredentialRequest {
        username,
        password,
        ssh_authorized_keys: keys,
    };
    match client.create_credential(&request).await {
        Ok(c) => println!(
            "{} {} (password: {}, SSH keys: {})",
            "Credential created:".green(),
            c.username.yellow(),
            if c.has_password { "set" } else { "none" },
            c.ssh_authorized_keys.len()
        ),
        Err(e) => println!("{} {}", "Error:".red(), e),
    }
}

async fn handle_credential_update(client: &CliClient, username: &str, request: UpdateCredentialRequest) {
    match client.update_credential(username, &request).await {
        Ok(c) => {
            println!("{} {}", "Credential updated:".green(), c.username.yellow());
            println!(
                "{} Changes apply to VMs on their first boot only; running or already-provisioned VMs keep their current login.",
                "Note:".dimmed()
            );
        }
        Err(e) => println!("{} {}", "Error:".red(), e),
    }
}

/// REPL command names offered by Tab (aliases included).
const COMMANDS: &[&str] = &[
    "help", "exit", "quit", "list", "ls", "get", "create", "start", "stop", "pause",
    "connect", "console", "attach", "log", "logs", "delete", "rm", "pci", "pci-devices",
    "attach-device", "detach-device", "credentials", "creds", "credential-add",
    "credential-passwd", "credential-keys", "credential-rm", "networks", "network-add",
    "network-rm", "bridges", "uplinks", "uplink-add", "uplink-rm", "ovs", "health",
];

/// Commands whose argument at this (0-based, after the command) index is a
/// filesystem path.
const PATH_ARGS: &[(&str, usize)] = &[("attach-device", 1), ("detach-device", 1)];

/// What Tab completes in the line being edited.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CompletionMode {
    /// REPL line: command names, then path arguments.
    Command,
    /// A prompt for one path or a comma-separated list of paths.
    Path,
}

struct GxHelper {
    mode: CompletionMode,
    files: FilenameCompleter,
}

impl GxHelper {
    fn new(mode: CompletionMode) -> Self {
        Self {
            mode,
            files: FilenameCompleter::new(),
        }
    }
}

impl Completer for GxHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        match self.mode {
            CompletionMode::Command => complete_command_line(&self.files, line, pos),
            CompletionMode::Path => complete_path_list(&self.files, line, pos),
        }
    }
}

impl Hinter for GxHelper {
    type Hint = String;
}
impl Highlighter for GxHelper {}
impl Validator for GxHelper {}
impl Helper for GxHelper {}

fn new_editor(mode: CompletionMode) -> rustyline::Result<Editor<GxHelper, DefaultHistory>> {
    // List mode behaves like bash: complete the common prefix, list the rest.
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .auto_add_history(false)
        .build();
    let mut editor = Editor::with_config(config)?;
    editor.set_helper(Some(GxHelper::new(mode)));
    Ok(editor)
}

/// Complete the command word, or a path for commands that take one.
fn complete_command_line(
    files: &FilenameCompleter,
    line: &str,
    pos: usize,
) -> rustyline::Result<(usize, Vec<Pair>)> {
    let before = &line[..pos];
    let words: Vec<&str> = before.split_whitespace().collect();
    let starting_new_word = before.is_empty() || before.ends_with(char::is_whitespace);

    // Still typing the command itself.
    if words.is_empty() || (words.len() == 1 && !starting_new_word) {
        let prefix = words.first().copied().unwrap_or("");
        let start = pos - prefix.len();
        let candidates = COMMANDS
            .iter()
            .filter(|c| c.starts_with(prefix))
            .map(|c| Pair {
                display: c.to_string(),
                replacement: format!("{} ", c),
            })
            .collect();
        return Ok((start, candidates));
    }

    // Index of the argument under the cursor, not counting the command.
    let arg_index = if starting_new_word {
        words.len() - 1
    } else {
        words.len() - 2
    };
    if PATH_ARGS.contains(&(words[0], arg_index)) {
        return files.complete_path(line, pos);
    }
    Ok((pos, Vec::new()))
}

/// Complete the path under the cursor, where the input may be a
/// comma-separated list (commas aren't a word break for rustyline).
fn complete_path_list(
    files: &FilenameCompleter,
    line: &str,
    pos: usize,
) -> rustyline::Result<(usize, Vec<Pair>)> {
    let before = &line[..pos];
    let segment_start = before.rfind(',').map(|i| i + 1).unwrap_or(0);
    let segment_start = segment_start + (before[segment_start..].len() - before[segment_start..].trim_start().len());
    let (start, candidates) = files.complete_path(&before[segment_start..], pos - segment_start)?;
    Ok((segment_start + start, candidates))
}

/// Prompt for a path (or comma-separated paths) with Tab completion when
/// stdin is a terminal. Completion escapes spaces (`my\ dir`); undo that
/// so callers get the real path.
fn prompt_path(msg: &str) -> String {
    if !io::stdin().is_terminal() {
        return prompt(msg);
    }
    let mut editor = match new_editor(CompletionMode::Path) {
        Ok(editor) => editor,
        Err(_) => return prompt(msg),
    };
    match editor.readline(msg) {
        Ok(line) => unescape(line.trim(), Some('\\')).into_owned(),
        // Ctrl-C / Ctrl-D answer the prompt with nothing, like an empty line.
        Err(_) => String::new(),
    }
}

fn prompt_path_optional(msg: &str) -> Option<String> {
    let input = prompt_path(msg);
    if input.is_empty() {
        None
    } else {
        Some(input)
    }
}

fn prompt(msg: &str) -> String {
    print!("{}", msg);
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    input.trim().to_string()
}

fn prompt_optional(msg: &str) -> Option<String> {
    let input = prompt(msg);
    if input.is_empty() {
        None
    } else {
        Some(input)
    }
}

async fn handle_create(client: &CliClient) {
    println!("{}", "Create new VM".bold());
    println!("{}", "-".repeat(40));

    let name = prompt("VM name: ");
    if name.is_empty() {
        println!("{}", "Error: VM name is required".red());
        return;
    }

    let vcpu_count: u8 = match prompt("vCPU count [1]: ").parse() {
        Ok(n) => n,
        Err(_) => 1,
    };

    let mem_size_mib: u32 = match prompt("Memory (MiB) [512]: ").parse() {
        Ok(n) => n,
        Err(_) => 512,
    };

    let hypervisor = match prompt("Hypervisor [cloudhypervisor/qemu] (default: cloudhypervisor): ")
        .to_lowercase()
        .as_str()
    {
        "" | "cloudhypervisor" | "cloud-hypervisor" | "ch" => Some("cloudhypervisor".to_string()),
        "qemu" | "q" => Some("qemu".to_string()),
        other => {
            println!(
                "{} Unknown hypervisor '{}', using cloudhypervisor",
                "Warning:".yellow(),
                other
            );
            Some("cloudhypervisor".to_string())
        }
    };

    // Cloud Hypervisor boots a disk image through UEFI firmware by default,
    // using the CLOUDHV.fd that glidex-install downloads into ~/.glidex.
    let firmware_path = if hypervisor.as_deref() == Some("cloudhypervisor") {
        let default_firmware = default_firmware_path()
            .filter(|p| p.exists())
            .map(|p| p.to_string_lossy().into_owned());
        let hint = default_firmware.as_deref().unwrap_or("none");
        match prompt_path(&format!(
            "UEFI firmware path ['none' for kernel boot] [{}]: ",
            hint
        ))
        .as_str()
        {
            "" => default_firmware,
            "none" => None,
            s => Some(s.to_string()),
        }
    } else {
        None
    };

    let kernel_image_path = if firmware_path.is_some() {
        String::new()
    } else {
        let path = prompt_path("Kernel image path: ");
        if path.is_empty() {
            println!("{}", "Error: kernel image path is required for kernel boot".red());
            return;
        }
        path
    };

    // Firmware boot needs a disk with its own bootloader (e.g. a raw distro
    // cloud image); kernel boot takes a bare root filesystem image.
    let rootfs_prompt = if firmware_path.is_some() {
        "Disk image path (UEFI-bootable, e.g. a raw cloud image): "
    } else {
        "Root filesystem path: "
    };
    let rootfs_path = prompt_path(rootfs_prompt);
    if rootfs_path.is_empty() {
        println!("{}", "Error: disk image path is required".red());
        return;
    }

    let cloud_init_path = if firmware_path.is_some() {
        prompt_path_optional("cloud-init seed image (optional, default: auto-generated): ")
    } else {
        None
    };

    // A stored credential is provisioned through the generated seed only.
    let credential = if firmware_path.is_some() && cloud_init_path.is_none() {
        let names: Vec<String> = client
            .list_credentials()
            .await
            .map(|cs| cs.into_iter().map(|c| c.username).collect())
            .unwrap_or_default();
        if names.is_empty() {
            println!(
                "{} No stored credentials; the guest login falls back to host SSH keys / GLIDEX_CLOUD_INIT_PASSWD_HASH (add one with 'credential-add').",
                "Note:".dimmed()
            );
            None
        } else {
            prompt_optional(&format!(
                "Login credential (optional; one of: {}): ",
                names.join(", ")
            ))
        }
    } else {
        None
    };

    let kernel_args = if firmware_path.is_some() {
        None
    } else {
        prompt_optional("Kernel arguments (optional, default: root=/dev/vda reboot=k panic=1): ")
    };

    // Networks (Cloud Hypervisor only). Offer the ones that exist.
    let networks = if hypervisor.as_deref() == Some("cloudhypervisor") {
        let names: Vec<String> = client
            .request_json::<Vec<NetworkRow>>(reqwest::Method::GET, "/networks", None)
            .await
            .map(|ns| ns.into_iter().map(|n| n.name).collect())
            .unwrap_or_default();
        if names.is_empty() {
            None
        } else {
            let default = if names.iter().any(|n| n == "default") { "default" } else { "none" };
            let answer = prompt(&format!(
                "Networks (comma-separated; available: {}; 'none' for no NIC) [{}]: ",
                names.join(", "),
                default
            ));
            let answer = if answer.is_empty() { default.to_string() } else { answer };
            if answer == "none" {
                None
            } else {
                Some(
                    answer
                        .split(',')
                        .map(|n| n.trim())
                        .filter(|n| !n.is_empty())
                        .map(|n| NetworkAttachmentReq { network: n.to_string() })
                        .collect::<Vec<_>>(),
                )
                .filter(|v| !v.is_empty())
            }
        }
    } else {
        None
    };

    let vfio_devices =
        prompt_path_optional("VFIO PCI devices (comma-separated, e.g. /sys/bus/pci/devices/0000:41:00.0): ")
            .map(|s| {
                s.split(',')
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

    let request = CreateVmRequest {
        name,
        vcpu_count,
        mem_size_mib,
        kernel_image_path,
        firmware_path,
        cloud_init_path,
        credential,
        rootfs_path,
        kernel_args,
        hypervisor,
        vfio_devices,
        networks,
    };

    match client.create_vm(request).await {
        Ok(vm) => {
            println!("{}", "VM created successfully!".green());
            println!("  ID: {}", vm.id.yellow());
            println!("  Name: {}", vm.name);
            println!("  State: {}", vm.state);
            println!("  Hypervisor: {}", vm.hypervisor);
        }
        Err(e) => println!("{} {}", "Error:".red(), e),
    }
}

fn format_state(state: &str) -> String {
    match state {
        "running" => state.green().to_string(),
        "stopped" => state.red().to_string(),
        "paused" => state.yellow().to_string(),
        "created" => state.blue().to_string(),
        _ => state.to_string(),
    }
}

/// Set terminal to raw mode for interactive console
fn set_raw_mode(fd: BorrowedFd<'_>) -> Option<Termios> {
    let orig_termios = termios::tcgetattr(fd).ok()?;
    let mut raw = orig_termios.clone();

    // Disable canonical mode and echo
    raw.local_flags.remove(LocalFlags::ICANON);
    raw.local_flags.remove(LocalFlags::ECHO);
    raw.local_flags.remove(LocalFlags::ISIG);

    termios::tcsetattr(fd, SetArg::TCSANOW, &raw).ok()?;
    Some(orig_termios)
}

/// Restore terminal to original mode
fn restore_terminal(fd: BorrowedFd<'_>, termios: &Termios) {
    let _ = termios::tcsetattr(fd, SetArg::TCSANOW, termios);
}

async fn handle_log(client: &CliClient, vm_id: &str) {
    // Get console info from API
    let console_info = match client.get_console_info(vm_id).await {
        Ok(info) => info,
        Err(e) => {
            println!("{} {}", "Error:".red(), e);
            return;
        }
    };

    let log_path = &console_info.log_path;

    // Try to open and read the log file
    match File::open(log_path) {
        Ok(file) => {
            let reader = BufReader::new(file);
            let mut has_content = false;

            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        println!("{}", l);
                        has_content = true;
                    }
                    Err(e) => {
                        println!("{} Error reading log: {}", "Error:".red(), e);
                        return;
                    }
                }
            }

            if !has_content {
                println!("{} Log file is empty. Start the VM to see console output.", "Info:".yellow());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            println!("{} Log file not found. Start the VM first.", "Info:".yellow());
        }
        Err(e) => {
            println!("{} Failed to open log file: {}", "Error:".red(), e);
        }
    }
}

async fn handle_connect(client: &CliClient, vm_id: &str) {
    // Get console info from API
    let console_info = match client.get_console_info(vm_id).await {
        Ok(info) => info,
        Err(e) => {
            println!("{} {}", "Error:".red(), e);
            return;
        }
    };

    if !console_info.available {
        println!(
            "{} VM is not running. Start the VM first with: start {}",
            "Error:".red(),
            vm_id
        );
        return;
    }

    let socket_path = &console_info.console_socket_path;

    println!(
        "{} Connecting to VM console via {}",
        "Info:".cyan(),
        socket_path
    );
    println!(
        "{} Press {} to detach from console\n",
        "Tip:".yellow(),
        "Ctrl+]".bold()
    );

    // Connect to the console Unix socket
    let stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            println!(
                "{} Failed to connect to console socket {}: {}",
                "Error:".red(),
                socket_path,
                e
            );
            return;
        }
    };

    // Set socket to non-blocking for the reader
    stream.set_nonblocking(true).ok();
    let stream_write = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            println!("{} Failed to clone socket: {}", "Error:".red(), e);
            return;
        }
    };

    // Set up signal handler for Ctrl+C (we'll handle Ctrl+] for detach)
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Save original terminal settings and set raw mode
    let stdin = io::stdin();
    let stdin_fd = stdin.as_fd();
    let orig_termios = match set_raw_mode(stdin_fd) {
        Some(t) => t,
        None => {
            println!("{} Failed to set terminal to raw mode", "Error:".red());
            return;
        }
    };

    // Spawn thread to read from socket and write to stdout
    let running_reader = running.clone();
    let reader_handle = thread::spawn(move || {
        let mut stream = stream;
        let mut buf = [0u8; 1024];

        while running_reader.load(Ordering::SeqCst) {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = io::stdout().write_all(&buf[..n]);
                    let _ = io::stdout().flush();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    // Spawn thread to read from stdin and write to socket
    let running_writer = running.clone();
    let writer_handle = thread::spawn(move || {
        let mut stream = stream_write;
        let mut buf = [0u8; 1];

        while running_writer.load(Ordering::SeqCst) {
            match io::stdin().read(&mut buf) {
                Ok(0) => break,
                Ok(1) => {
                    // Check for Ctrl+] (0x1d) to detach
                    if buf[0] == 0x1d {
                        running_writer.store(false, Ordering::SeqCst);
                        break;
                    }
                    let _ = stream.write_all(&buf);
                    let _ = stream.flush();
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    // Handle Ctrl+C gracefully
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .ok();

    // Wait for threads to finish
    let _ = writer_handle.join();
    running.store(false, Ordering::SeqCst);
    let _ = reader_handle.join();

    // Restore terminal
    restore_terminal(stdin.as_fd(), &orig_termios);

    println!("\n{} Detached from console", "Info:".cyan());
}

async fn handle_command(line: &str, client: &CliClient) -> bool {
    let parts: Vec<&str> = line.trim().split_whitespace().collect();
    if parts.is_empty() {
        return true;
    }

    match parts[0] {
        "help" | "?" => print_help(),

        "exit" | "quit" | "q" => return false,

        "list" | "ls" => match client.list_vms().await {
            Ok(vms) => {
                if vms.is_empty() {
                    println!("{}", "No VMs found".yellow());
                } else {
                    let table = Table::new(&vms).to_string();
                    println!("{}", table);
                }
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },

        "get" => {
            if parts.len() < 2 {
                println!("{}", "Usage: get <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.get_vm(&vm_id).await {
                Ok(vm) => {
                    println!("{}", "VM Details".bold());
                    println!("{}", "-".repeat(40));
                    println!("  ID:         {}", vm.id.yellow());
                    println!("  Name:       {}", vm.name);
                    println!("  State:      {}", format_state(&vm.state));
                    println!("  Hypervisor: {}", vm.hypervisor);
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
                            nic.ipv4.as_deref().map(|ip| ip.green().to_string()).unwrap_or_else(|| "-".dimmed().to_string()),
                            nic.port.as_deref().map(|p| format!(" ({})", p)).unwrap_or_default()
                        );
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "create" => handle_create(client).await,

        "start" => {
            if parts.len() < 2 {
                println!("{}", "Usage: start <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.start_vm(&vm_id).await {
                Ok(vm) => {
                    println!(
                        "{} VM {} is now {}",
                        "Success:".green(),
                        vm.name,
                        format_state(&vm.state)
                    );
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "stop" => {
            if parts.len() < 2 {
                println!("{}", "Usage: stop <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.stop_vm(&vm_id).await {
                Ok(vm) => {
                    println!(
                        "{} VM {} is now {}",
                        "Success:".green(),
                        vm.name,
                        format_state(&vm.state)
                    );
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "pause" => {
            if parts.len() < 2 {
                println!("{}", "Usage: pause <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.pause_vm(&vm_id).await {
                Ok(vm) => {
                    println!(
                        "{} VM {} is now {}",
                        "Success:".green(),
                        vm.name,
                        format_state(&vm.state)
                    );
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "connect" | "console" | "attach" => {
            if parts.len() < 2 {
                println!("{}", "Usage: connect <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            handle_connect(client, &vm_id).await;
        }

        "log" | "logs" => {
            if parts.len() < 2 {
                println!("{}", "Usage: log <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            handle_log(client, &vm_id).await;
        }

        "delete" | "rm" => {
            if parts.len() < 2 {
                println!("{}", "Usage: delete <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            let confirm = prompt(&format!(
                "Are you sure you want to delete VM {}? [y/N]: ",
                parts[1]
            ));
            if confirm.to_lowercase() == "y" {
                match client.delete_vm(&vm_id).await {
                    Ok(()) => println!("{} VM deleted", "Success:".green()),
                    Err(e) => println!("{} {}", "Error:".red(), e),
                }
            } else {
                println!("Cancelled");
            }
        }

        "pci" | "pci-devices" => match client.list_pci_devices().await {
            Ok(devices) => {
                if devices.is_empty() {
                    println!("{}", "No PCI devices found".yellow());
                } else if parts.len() >= 2 {
                    // `pci <address>` shows full details for a single device,
                    // including the sysfs path needed for VFIO attachment.
                    match devices.iter().find(|d| d.address == parts[1]) {
                        Some(dev) => {
                            println!("Address:    {}", dev.address);
                            println!("Vendor:     {}", dev.vendor_id);
                            println!("Device:     {}", dev.device_id);
                            println!("Class:      {}", dev.class_id);
                            println!("Driver:     {}", display_option(&dev.driver));
                            println!("IOMMU:      {}", display_option(&dev.iommu_group));
                            println!("Sysfs path: {}", dev.sysfs_path);
                        }
                        None => println!(
                            "{} no PCI device with address {}",
                            "Error:".red(),
                            parts[1]
                        ),
                    }
                } else {
                    let table = Table::new(&devices).to_string();
                    println!("{}", table);
                }
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },

        "attach-device" => {
            if parts.len() < 3 {
                println!(
                    "{}",
                    "Usage: attach-device <name|id> <device-path>".yellow()
                );
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.attach_device(&vm_id, parts[2]).await {
                Ok(vm) => {
                    println!(
                        "{} Device {} attached to VM {}",
                        "Success:".green(),
                        parts[2],
                        vm.name
                    );
                    if !vm.vfio_devices.is_empty() {
                        println!("  VFIO devices: {}", vm.vfio_devices.join(", "));
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "detach-device" => {
            if parts.len() < 3 {
                println!(
                    "{}",
                    "Usage: detach-device <name|id> <device-path>".yellow()
                );
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.detach_device(&vm_id, parts[2]).await {
                Ok(vm) => {
                    println!(
                        "{} Device {} detached from VM {}",
                        "Success:".green(),
                        parts[2],
                        vm.name
                    );
                    if !vm.vfio_devices.is_empty() {
                        println!("  VFIO devices: {}", vm.vfio_devices.join(", "));
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "credentials" | "creds" => match client.list_credentials().await {
            Ok(creds) if creds.is_empty() => println!("No credentials stored."),
            Ok(creds) => {
                let rows: Vec<CredentialRow> = creds.iter().map(CredentialRow::from).collect();
                println!("{}", Table::new(rows));
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },

        "credential-add" | "cred-add" => handle_credential_add(client).await,

        "credential-passwd" | "cred-passwd" => match parts.get(1) {
            Some(username) => match prompt_new_password(false) {
                Ok(password) => {
                    let request = UpdateCredentialRequest {
                        password,
                        ssh_authorized_keys: None,
                    };
                    handle_credential_update(client, username, request).await
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            None => println!("{}", "Usage: credential-passwd <user>".yellow()),
        },

        "credential-keys" | "cred-keys" => match parts.get(1) {
            Some(username) => {
                let input = prompt_path("SSH public key files (comma-separated; empty to remove all keys): ");
                match read_public_key_files(&input) {
                    Ok(keys) => {
                        let request = UpdateCredentialRequest {
                            password: None,
                            ssh_authorized_keys: Some(keys),
                        };
                        handle_credential_update(client, username, request).await
                    }
                    Err(e) => println!("{} {}", "Error:".red(), e),
                }
            }
            None => println!("{}", "Usage: credential-keys <user>".yellow()),
        },

        "credential-rm" | "cred-rm" => match parts.get(1) {
            Some(username) => match client.delete_credential(username).await {
                Ok(()) => println!("{} {}", "Credential deleted:".green(), username),
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            None => println!("{}", "Usage: credential-rm <user>".yellow()),
        },

        "networks" | "nets" => {
            match client.request_json::<Vec<NetworkRow>>(reqwest::Method::GET, "/networks", None).await {
                Ok(nets) if nets.is_empty() => println!("No networks. Create one with 'network-add <name>'."),
                Ok(nets) => println!("{}", Table::new(nets)),
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "network-add" | "net-add" => handle_network_add(client, &parts[1..]).await,

        "network-rm" | "net-rm" => match parts.get(1) {
            Some(name) => match client
                .request_json::<()>(reqwest::Method::DELETE, &format!("/networks/{}", name), None)
                .await
            {
                Ok(()) => println!("{} {}", "Network deleted:".green(), name),
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            None => println!("{}", "Usage: network-rm <name>".yellow()),
        },

        "bridges" => match client
            .request_json::<Vec<serde_json::Value>>(reqwest::Method::GET, "/ovs/bridges", None)
            .await
        {
            Ok(bridges) if bridges.is_empty() => println!("No glidex bridges."),
            Ok(bridges) => {
                for b in bridges {
                    println!(
                        "  {} ({}) {}",
                        b["spec"]["name"].as_str().unwrap_or("?").cyan(),
                        b["spec"]["datapath"].as_str().unwrap_or("?"),
                        if b["live"].is_null() { "MISSING on host".red().to_string() } else { format!("ports: {}", b["live"]["ports"].as_array().map(|p| p.len()).unwrap_or(0)) }
                    );
                }
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },

        "ovs" => handle_ovs(client, &parts[1..]).await,

        "uplinks" => match parts.get(1) {
            Some(bridge) => match client
                .request_json::<Vec<serde_json::Value>>(reqwest::Method::GET, &format!("/ovs/bridges/{}/uplinks", bridge), None)
                .await
            {
                Ok(list) if list.is_empty() => println!("No uplinks on {}.", bridge),
                Ok(list) => {
                    for u in list {
                        let spec = &u["record"]["spec"];
                        println!(
                            "  {} ({}) {} {}",
                            spec["name"].as_str().unwrap_or("?").cyan(),
                            spec["kind"].as_str().unwrap_or("?"),
                            u["phase"].as_str().unwrap_or("?"),
                            u["live"]["error"].as_str().map(|e| e.red().to_string()).unwrap_or_else(|| u["live"]["link_state"].as_str().unwrap_or("").to_string())
                        );
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            None => println!("{}", "Usage: uplinks <bridge>".yellow()),
        },

        "uplink-add" => handle_uplink_add(client, &parts[1..]).await,

        "uplink-rm" => match (parts.get(1), parts.get(2)) {
            (Some(bridge), Some(name)) => match client
                .request_json::<()>(reqwest::Method::DELETE, &format!("/ovs/bridges/{}/uplinks/{}", bridge, name), None)
                .await
            {
                Ok(()) => println!("{} uplink {} (NIC restored)", "Removed:".green(), name),
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            _ => println!("{}", "Usage: uplink-rm <bridge> <name>".yellow()),
        },

        "health" => match client.health_check().await {
            Ok(()) => println!("{} API server is healthy", "OK:".green()),
            Err(e) => println!("{} {}", "Error:".red(), e),
        },

        _ => println!(
            "{} Unknown command: {}. Type 'help' for available commands.",
            "Error:".red(),
            parts[0]
        ),
    }

    true
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let client = CliClient::new(cli.server.clone());

    println!(
        "{}",
        r#"
   _____ _ _     _
  / ____| (_)   | |
 | |  __| |_  __| | _____  __
 | | |_ | | |/ _` |/ _ \ \/ /
 | |__| | | | (_| |  __/>  <
  \_____|_|_|\__,_|\___/_/\_\
          Control Plane CLI
"#
        .cyan()
    );

    println!("Connected to: {}", cli.server.yellow());
    println!("Type {} for available commands\n", "help".cyan());

    let mut rl = new_editor(CompletionMode::Command).expect("Failed to initialize readline");
    println!("{} Tab completes commands and file paths.\n", "Tip:".dimmed());

    loop {
        match rl.readline("gxctl> ") {
            Ok(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&line);
                if !handle_command(&line, &client).await {
                    println!("Goodbye!");
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("Use 'exit' to quit");
            }
            Err(ReadlineError::Eof) => {
                println!("Goodbye!");
                break;
            }
            Err(err) => {
                println!("Error: {:?}", err);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replacements(result: rustyline::Result<(usize, Vec<Pair>)>) -> (usize, Vec<String>) {
        let (start, pairs) = result.unwrap();
        let mut r: Vec<String> = pairs.into_iter().map(|p| p.replacement).collect();
        r.sort();
        (start, r)
    }

    fn temp_tree() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("CLOUDHV.fd"), b"").unwrap();
        std::fs::write(dir.path().join("cloud.raw"), b"").unwrap();
        std::fs::create_dir(dir.path().join("my dir")).unwrap();
        std::fs::write(dir.path().join("my dir").join("key.pub"), b"").unwrap();
        dir
    }

    #[test]
    fn completes_command_names() {
        let files = FilenameCompleter::new();
        let (start, got) = replacements(complete_command_line(&files, "cred", 4));
        assert_eq!(start, 0);
        assert_eq!(
            got,
            ["cred-add", "credential-add", "credential-keys", "credential-passwd", "credential-rm", "credentials", "creds"]
                .iter()
                .filter(|c| COMMANDS.contains(c))
                .map(|c| format!("{c} "))
                .collect::<Vec<_>>()
        );
        let (_, got) = replacements(complete_command_line(&files, "", 0));
        assert_eq!(got.len(), COMMANDS.len());
    }

    #[test]
    fn completes_paths_only_where_a_command_takes_one() {
        let dir = temp_tree();
        let files = FilenameCompleter::new();
        let base = dir.path().display().to_string();

        // attach-device <vm> <path>: the path argument completes.
        let line = format!("attach-device my-vm {base}/CLO");
        let (start, got) = replacements(complete_command_line(&files, &line, line.len()));
        assert_eq!(start, "attach-device my-vm ".len());
        assert_eq!(got, vec![format!("{base}/CLOUDHV.fd")]);

        // ...but not the VM name, and not arguments of other commands.
        let line = format!("attach-device {base}/CLO");
        assert!(replacements(complete_command_line(&files, &line, line.len())).1.is_empty());
        let line = format!("start {base}/CLO");
        assert!(replacements(complete_command_line(&files, &line, line.len())).1.is_empty());
    }

    #[test]
    fn completes_each_entry_of_a_comma_separated_path_list() {
        let dir = temp_tree();
        let files = FilenameCompleter::new();
        let base = dir.path().display().to_string();

        let line = format!("{base}/CLOUDHV.fd, {base}/clo");
        let (start, got) = replacements(complete_path_list(&files, &line, line.len()));
        assert_eq!(start, format!("{base}/CLOUDHV.fd, ").len());
        assert_eq!(got, vec![format!("{base}/cloud.raw")]);

        // Spaces are escaped for editing and unescaped by prompt_path.
        let line = format!("{base}/my");
        let (_, got) = replacements(complete_path_list(&files, &line, line.len()));
        assert_eq!(got, vec![format!("{base}/my\\ dir/")]);
        assert_eq!(unescape(&got[0], Some('\\')), format!("{base}/my dir/"));
    }

    #[test]
    fn completes_tilde_paths() {
        let files = FilenameCompleter::new();
        let (start, got) = replacements(complete_path_list(&files, "~/", 2));
        assert_eq!(start, 0);
        assert!(got.iter().all(|c| c.starts_with("~/")), "{got:?}");
    }

    #[test]
    fn formats_unix_time_as_utc() {
        assert_eq!(format_unix_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_unix_time(1_790_000_000), "2026-09-21 14:13 UTC");
    }

    #[test]
    fn reads_public_keys_and_refuses_private_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let public = dir.path().join("id.pub");
        std::fs::write(&public, "# comment\nssh-ed25519 AAAA a@b\n\nssh-rsa BBBB c@d\n").unwrap();
        let private = dir.path().join("id");
        std::fs::write(&private, "-----BEGIN OPENSSH PRIVATE KEY-----\nxxx\n").unwrap();

        let keys = read_public_key_files(public.to_str().unwrap()).unwrap();
        assert_eq!(keys, vec!["ssh-ed25519 AAAA a@b", "ssh-rsa BBBB c@d"]);

        let both = format!("{}, {}", public.display(), private.display());
        let err = read_public_key_files(&both).unwrap_err();
        assert!(err.contains("private key"), "{err}");
        assert!(!err.contains("xxx"), "error must not echo key material");
        assert!(read_public_key_files("").unwrap().is_empty());
    }

    #[test]
    fn pci_device_info_deserializes_sysfs_path() {
        let json = r#"{
            "address": "0000:00:1f.0",
            "vendor_id": "8086",
            "device_id": "9d4e",
            "class_id": "060100",
            "driver": "lpc_ich",
            "iommu_group": "5",
            "sysfs_path": "/sys/bus/pci/devices/0000:00:1f.0"
        }"#;

        let info: PciDeviceInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.sysfs_path, "/sys/bus/pci/devices/0000:00:1f.0");
        assert_eq!(info.address, "0000:00:1f.0");
        assert_eq!(info.driver.as_deref(), Some("lpc_ich"));
        assert_eq!(info.iommu_group.as_deref(), Some("5"));
    }

    #[test]
    fn display_option_renders_dash_for_none() {
        assert_eq!(display_option(&None), "-");
        assert_eq!(display_option(&Some("vfio-pci".to_string())), "vfio-pci");
    }
}
