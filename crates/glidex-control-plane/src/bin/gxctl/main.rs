//! `gxctl`: interactive CLI for the glidex control plane (spec/cli.md).

mod admin;
mod client;
mod console;

use clap::Parser;
use client::{enc, ApiClient};
use colored::Colorize;
use console::{handle_connect, handle_log, restore_terminal};
use hyper::Method;
use nix::sys::termios::{self, LocalFlags, SetArg};
use rustyline::completion::{unescape, Completer, FilenameCompleter, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Editor, Helper};
use serde::{Deserialize, Serialize};
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use tabled::{Table, Tabled};

#[derive(Parser)]
#[command(name = "gxctl")]
#[command(about = "Interactive CLI for Glidex Control Plane")]
struct Cli {
    /// Control plane Unix socket (default: the first existing of
    /// /run/glidex-cp/api.sock, $XDG_RUNTIME_DIR/glidex/api.sock,
    /// /tmp/glidex-<uid>/api.sock). You're identified by your Unix user.
    #[arg(long, env = "GLIDEX_SOCKET")]
    socket: Option<PathBuf>,

    /// Use TCP instead (wins over --socket), e.g. https://glidex.example.org:8841. Sends the
    /// token from GLIDEX_TOKEN or ~/.config/glidex/token (see `login`). A self-signed
    /// certificate is trusted through GLIDEX_CA_CERT (a PEM file), or automatically
    /// for the local control plane.
    #[arg(short = 's', long, visible_alias = "server")]
    url: Option<String>,

    /// Project (id or name) for creates, lists and name lookups;
    /// default: your default project (`project use`).
    #[arg(short, long, env = "GLIDEX_PROJECT")]
    project: Option<String>,

    /// Run one command and exit instead of starting the shell, e.g.
    /// `gxctl list` or `gxctl --url https://… login --oidc`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

/// Pick the transport (spec/cli.md "Transport"): `--url` → TCP with a
/// token; else `--socket` / `GLIDEX_SOCKET` / the first existing default
/// socket; else TCP to localhost.
fn build_client(cli: &Cli) -> Result<ApiClient, String> {
    let tcp = |url: &str| -> Result<ApiClient, String> {
        let token = match client::load_token() {
            Ok(t) => t,
            Err(e) => {
                println!("{} {}", "Warning:".yellow(), e);
                None
            }
        };
        ApiClient::tcp(url, token)
    };
    let c = match &cli.url {
        Some(url) => tcp(url)?,
        None => {
            let candidates = client::socket_candidates(
                std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
                nix::unistd::geteuid().as_raw(),
            );
            match client::select_socket(cli.socket.clone(), &candidates, client::is_socket) {
                Some(p) => ApiClient::unix(p),
                None => tcp(client::DEFAULT_URL)?,
            }
        }
    };
    c.set_project(cli.project.clone());
    Ok(c)
}

#[derive(Debug, Deserialize)]
struct VmResponse {
    id: String,
    name: String,
    #[serde(default)]
    project: String,
    state: String,
    /// What the VM should be doing; `state` is what it is doing
    /// (spec/reconciliation.md §7.4).
    #[serde(default)]
    desired_state: String,
    #[serde(default)]
    conditions: Vec<serde_json::Value>,
    #[serde(default)]
    last_exit: Option<serde_json::Value>,
    vcpu_count: u8,
    mem_size_mib: u32,
    hypervisor: String,
    #[serde(default)]
    vfio_devices: Vec<String>,
    #[serde(default)]
    nics: Vec<NicInfo>,
    /// The node the VM is placed on (spec/clustering.md §9.1).
    #[serde(default)]
    node: Option<String>,
}

/// A `list` row: the project shown by name when it's known.
#[derive(Tabled)]
struct VmRow {
    id: String,
    name: String,
    project: String,
    state: String,
    node: String,
    vcpu_count: u8,
    mem_size_mib: u32,
    hypervisor: String,
}

/// A `nodes` row.
#[derive(Tabled)]
struct NodeRow {
    id: String,
    name: String,
    role: String,
    phase: String,
    ready: String,
    cpus: u64,
    memory_mib: u64,
}

impl NodeRow {
    fn from_json(n: &serde_json::Value) -> NodeRow {
        let text = |v: &serde_json::Value| v.as_str().unwrap_or("").to_string();
        NodeRow {
            id: text(&n["meta"]["id"]),
            name: text(&n["spec"]["name"]),
            role: text(&n["spec"]["role"]),
            phase: text(&n["status"]["phase"]),
            ready: text(&n["status"]["ready"]),
            cpus: n["status"]["capacity"]["cpus"].as_u64().unwrap_or(0),
            memory_mib: n["status"]["capacity"]["memory_mib"].as_u64().unwrap_or(0),
        }
    }
}

/// Project names by id (best effort: an empty map when not allowed).
async fn project_names(client: &ApiClient) -> std::collections::HashMap<String, String> {
    client
        .request_json::<Vec<serde_json::Value>>(Method::GET, "/projects", None)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| Some((p["id"].as_str()?.to_string(), p["name"].as_str()?.to_string())))
        .collect()
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
    /// Owning project (project networks); host networks have none.
    #[tabled(display_with = "display_option")]
    #[serde(default)]
    project: Option<String>,
}

async fn list_networks(client: &ApiClient) {
    match client.request_json::<Vec<NetworkRow>>(Method::GET, "/networks", None).await {
        Ok(nets) if nets.is_empty() => println!("No networks. Create one with 'network create <name>'."),
        Ok(mut nets) => {
            let names = project_names(client).await;
            for n in &mut nets {
                if let Some(p) = n.project.as_mut() {
                    if let Some(name) = names.get(p.as_str()) {
                        *p = name.clone();
                    }
                }
            }
            println!("{}", Table::new(nets))
        }
        Err(e) => println!("{} {}", "Error:".red(), e),
    }
}

fn display_opt_u16(o: &Option<u16>) -> String {
    o.map(|v| v.to_string()).unwrap_or_else(|| "-".to_string())
}

/// `--flag value` from a command's arguments.
fn flag_value<'a>(args: &'a [&'a str], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| *a == flag).and_then(|i| args.get(i + 1)).copied()
}

/// Every value of a repeatable `--flag value`.
fn flag_values<'a>(args: &[&'a str], flag: &str) -> Vec<&'a str> {
    args.windows(2).filter(|w| w[0] == flag).map(|w| w[1]).collect()
}

/// Split a command line into words; single or double quotes keep spaces
/// (`project create lab --description "Lab VMs"`), backslash escapes.
fn split_line(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err("unterminated quote".into());
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

/// `--graceful [secs]` from a `stop` command's arguments.
fn graceful_stop_secs(args: &[&str]) -> Result<Option<u64>, String> {
    const DEFAULT_SECS: u64 = 60;
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
    match client.request_json::<NetworkRow>(Method::POST, "/networks", Some(body)).await {
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
    let res = match client.request_json::<serde_json::Value>(Method::POST, &path, Some(body)).await {
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
        match client.request_json::<serde_json::Value>(Method::POST, &commit, Some(serde_json::json!({"token": token}))).await {
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
            match client.request_json::<serde_json::Value>(Method::GET, "/ovs/status", None).await {
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
            match client.request_json::<serde_json::Value>(Method::POST, "/ovs/install", Some(body)).await {
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
            match client.request_json::<()>(Method::POST, "/ovs/dpdk-init", Some(body)).await {
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
    firmware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    firmware_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cloud_init_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    rootfs_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_disk_size_gib: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    root_disk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_disks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel_args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hypervisor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vfio_devices: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    networks: Option<Vec<NetworkAttachmentReq>>,
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

/// The API client (transport, auth, project scope) lives in `client.rs`;
/// these are the VM and credential calls the REPL uses.
type CliClient = ApiClient;

impl ApiClient {
    async fn list_vms(&self) -> Result<Vec<VmResponse>, String> {
        self.request_json(Method::GET, &self.scoped("/vms"), None).await
    }

    async fn get_vm(&self, id: &str) -> Result<VmResponse, String> {
        self.request_json(Method::GET, &format!("/vms/{}", enc(id)), None).await
    }

    async fn create_vm(&self, request: CreateVmRequest) -> Result<VmResponse, String> {
        let mut body = serde_json::to_value(&request).map_err(|e| e.to_string())?;
        self.scope_body(&mut body);
        self.request_json(Method::POST, "/vms", Some(body)).await
    }

    /// `?wait=<secs>` unless the caller asked not to wait (`--no-wait`).
    fn waiting(path: String, wait: Option<u64>) -> String {
        match wait {
            Some(w) => client::add_query(&path, "wait", &w.to_string()),
            None => path,
        }
    }

    async fn start_vm(&self, id: &str, wait: Option<u64>) -> Result<VmResponse, String> {
        self.request_json(Method::POST, &Self::waiting(format!("/vms/{}/start", enc(id)), wait), None).await
    }

    /// Stop a VM; with `graceful_secs`, power-button first and wait.
    async fn stop_vm(&self, id: &str, graceful_secs: Option<u64>, wait: Option<u64>) -> Result<VmResponse, String> {
        let mut path = format!("/vms/{}/stop", enc(id));
        if let Some(s) = graceful_secs {
            path = client::add_query(&path, "graceful_timeout_secs", &s.to_string());
        }
        // Waiting must cover the grace.
        let wait = wait.map(|w| w.max(graceful_secs.unwrap_or(0) + 20).min(300));
        self.request_json(Method::POST, &Self::waiting(path, wait), None).await
    }

    async fn pause_vm(&self, id: &str, wait: Option<u64>) -> Result<VmResponse, String> {
        self.request_json(Method::POST, &Self::waiting(format!("/vms/{}/pause", enc(id)), wait), None).await
    }

    async fn delete_vm(&self, id: &str, keep_disk: bool, wait: Option<u64>) -> Result<(), String> {
        let path = format!("/vms/{}{}", enc(id), if keep_disk { "?keep_disk=true" } else { "" });
        self.request_json::<serde_json::Value>(Method::DELETE, &Self::waiting(path, wait), None).await.map(|_| ())
    }

    async fn vm_events(&self, id: &str) -> Result<serde_json::Value, String> {
        self.request_json(Method::GET, &format!("/vms/{}/events", enc(id)), None).await
    }

    async fn list_credentials(&self) -> Result<Vec<CredentialInfo>, String> {
        self.request_json(Method::GET, &self.scoped("/credentials"), None).await
    }

    async fn create_credential(&self, request: &CreateCredentialRequest) -> Result<CredentialInfo, String> {
        let mut body = serde_json::to_value(request).map_err(|e| e.to_string())?;
        self.scope_body(&mut body);
        self.request_json(Method::POST, "/credentials", Some(body)).await
    }

    async fn update_credential(&self, username: &str, request: &UpdateCredentialRequest) -> Result<CredentialInfo, String> {
        let body = serde_json::to_value(request).map_err(|e| e.to_string())?;
        self.request_json(Method::PUT, &self.scoped(&format!("/credentials/{}", enc(username))), Some(body)).await
    }

    async fn delete_credential(&self, username: &str) -> Result<(), String> {
        self.request_json(Method::DELETE, &self.scoped(&format!("/credentials/{}", enc(username))), None).await
    }

    async fn health_check(&self) -> Result<(), String> {
        self.request_bytes(Method::GET, "/health", None).await.map(|_| ()).map_err(|e| e.message)
    }

    async fn list_pci_devices(&self) -> Result<Vec<PciDeviceInfo>, String> {
        self.request_json(Method::GET, "/pci-devices", None).await
    }

    async fn attach_device(&self, vm_id: &str, device_path: &str) -> Result<VmResponse, String> {
        let body = serde_json::json!({ "device_path": device_path });
        self.request_json(Method::POST, &format!("/vms/{}/devices", enc(vm_id)), Some(body)).await
    }

    async fn detach_device(&self, vm_id: &str, device_path: &str) -> Result<VmResponse, String> {
        let body = serde_json::json!({ "device_path": device_path });
        self.request_json(Method::DELETE, &format!("/vms/{}/devices", enc(vm_id)), Some(body)).await
    }

    /// Resolve a VM identifier (name or ID) to an ID.
    /// First tries to use it as an ID, then searches by name (within the
    /// selected project, if any).
    async fn resolve_vm(&self, name_or_id: &str) -> Result<String, String> {
        if let Ok(vm) = self.get_vm(name_or_id).await {
            return Ok(vm.id);
        }
        let vms = self.list_vms().await?;
        let matches: Vec<_> = vms.iter().filter(|vm| vm.name == name_or_id).collect();
        match matches.len() {
            0 => Err(format!("VM '{}' not found", name_or_id)),
            1 => Ok(matches[0].id.clone()),
            _ => {
                let ids: Vec<_> = matches.iter().map(|vm| format!("{} (project {})", vm.id, vm.project)).collect();
                Err(format!(
                    "Multiple VMs found with name '{}'. Use --project or the ID instead: {}",
                    name_or_id,
                    ids.join(", ")
                ))
            }
        }
    }
}

/// `1.5 GiB`-style sizes (binary units).
fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{} B", n) } else { format!("{:.1} {}", v, UNITS[u]) }
}

fn image_status(img: &serde_json::Value) -> String {
    let st = &img["status"];
    match st["state"].as_str().unwrap_or("?") {
        "downloading" => {
            let got = st["received_bytes"].as_u64().unwrap_or(0);
            match st["total_bytes"].as_u64() {
                Some(t) if t > 0 => format!("downloading {:.0}%", got as f64 * 100.0 / t as f64),
                _ => format!("downloading {}", format_bytes(got)),
            }
        }
        "failed" => format!("failed: {}", st["reason"].as_str().unwrap_or("")).red().to_string(),
        "ready" => "ready".green().to_string(),
        other => other.to_string(),
    }
}

async fn handle_image(client: &CliClient, args: &[&str]) {
    let usage = "Usage: image catalog | list | pull <catalog-key|url> [--name N] [--sha256 H] [--firmware-for <cloudhypervisor|qemu> [--vars-url U] [--vars-sha256 H]] | pull --firmware <key> [--name N] | retry <name|id> | rm <name|id>";
    match args.first().copied().unwrap_or("list") {
        "catalog" => match client.request_json::<Vec<serde_json::Value>>(Method::GET, "/images/catalog", None).await {
            Ok(items) => {
                println!("{}", "Cloud images (image pull <key>):".bold());
                for i in items {
                    println!(
                        "  {:<14} {} {}{}",
                        i["key"].as_str().unwrap_or("?").cyan(),
                        i["distro"].as_str().unwrap_or(""),
                        i["release"].as_str().unwrap_or(""),
                        if i["downloaded_image_id"].is_null() { String::new() } else { " (downloaded)".green().to_string() }
                    );
                }
                if let Ok(fw) = client.request_json::<Vec<serde_json::Value>>(Method::GET, "/images/firmware-catalog", None).await {
                    println!("{}", "UEFI firmware (image pull --firmware <key>):".bold());
                    for i in fw {
                        let note = if !i["downloaded_image_id"].is_null() {
                            " (downloaded)".green().to_string()
                        } else if let Some(h) = i["hint"].as_str() {
                            format!(" ({})", h).yellow().to_string()
                        } else {
                            String::new()
                        };
                        println!(
                            "  {:<14} {} [{}] {}{}",
                            i["key"].as_str().unwrap_or("?").cyan(),
                            i["name"].as_str().unwrap_or(""),
                            i["hypervisor"].as_str().unwrap_or(""),
                            i["version"].as_str().unwrap_or(""),
                            note
                        );
                    }
                }
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },
        "list" | "ls" => match client.request_json::<Vec<serde_json::Value>>(Method::GET, "/images", None).await {
            Ok(imgs) if imgs.is_empty() => println!("No images. Pull one with 'image pull <key>' (see 'image catalog')."),
            Ok(imgs) => {
                for i in imgs {
                    let src = &i["source"];
                    let from = match src["kind"].as_str() {
                        Some("catalog") | Some("firmware") => format!("{} {}", src["key"].as_str().unwrap_or(""), src["version"].as_str().unwrap_or("")),
                        _ => src["url"].as_str().unwrap_or("").to_string(),
                    };
                    let kind = match i["hypervisor"].as_str() {
                        Some(h) if i["kind"] == "firmware" => format!("firmware/{}", h),
                        _ => "disk".to_string(),
                    };
                    println!(
                        "  {:<20} {:<20} {:<22} {:>9}  {}{}",
                        i["name"].as_str().unwrap_or("?").cyan(),
                        kind,
                        image_status(&i),
                        format_bytes(i["virtual_size_bytes"].as_u64().unwrap_or(0)),
                        from.trim(),
                        if i["verified"] == true { "" } else { " (unverified)" }
                    );
                }
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },
        "pull" => {
            let firmware = flag_value(args, "--firmware");
            let Some(what) = firmware.or_else(|| args.get(1).copied().filter(|a| !a.starts_with("--"))) else {
                println!("{}", usage.yellow());
                return;
            };
            let mut body = if firmware.is_some() {
                serde_json::json!({ "firmware": what })
            } else if what.contains("://") {
                serde_json::json!({ "url": what })
            } else {
                serde_json::json!({ "catalog": what })
            };
            if let Some(h) = flag_value(args, "--firmware-for") {
                body["kind"] = serde_json::json!("firmware");
                body["hypervisor"] = serde_json::json!(h);
            }
            // A split OVMF build's variable-store template (QEMU).
            if let Some(u) = flag_value(args, "--vars-url") {
                body["vars_url"] = serde_json::json!(u);
            }
            if let Some(h) = flag_value(args, "--vars-sha256") {
                body["vars_sha256"] = serde_json::json!(h);
            }
            if let Some(n) = flag_value(args, "--name") {
                body["name"] = serde_json::json!(n);
            }
            if let Some(h) = flag_value(args, "--sha256") {
                body["sha256"] = serde_json::json!(h);
            }
            let img = match client.request_json::<serde_json::Value>(Method::POST, "/images", Some(body)).await {
                Ok(i) => i,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return;
                }
            };
            let id = img["id"].as_str().unwrap_or("").to_string();
            println!("Pulling {} as {} (Ctrl-C stops watching; the download continues)", what, img["name"].as_str().unwrap_or("?").yellow());
            loop {
                match client.request_json::<serde_json::Value>(Method::GET, &format!("/images/{}", id), None).await {
                    Ok(i) => {
                        let state = i["status"]["state"].as_str().unwrap_or("").to_string();
                        print!("\r  {:<60}", image_status(&i));
                        let _ = io::stdout().flush();
                        if state != "downloading" && state != "verifying" {
                            println!();
                            if state == "ready" {
                                println!("{} {} ({}, sha256 {})", "Image ready:".green(), i["name"].as_str().unwrap_or(""), format_bytes(i["virtual_size_bytes"].as_u64().unwrap_or(0)), i["sha256"].as_str().unwrap_or(""));
                            }
                            break;
                        }
                    }
                    Err(e) => {
                        println!("\n{} {}", "Error:".red(), e);
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        "retry" => match args.get(1) {
            Some(name) => match client.request_json::<serde_json::Value>(Method::POST, &format!("/images/{}/retry", name), None).await {
                Ok(_) => println!("{} {} (follow it with: image list)", "Downloading again:".green(), name),
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            None => println!("{}", usage.yellow()),
        },
        "rm" | "delete" => match args.get(1) {
            Some(name) => match client.request_json::<serde_json::Value>(Method::DELETE, &format!("/images/{}?wait=60", name), None).await {
                Ok(v) => print_delete_result("Image", name, &v),
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            None => println!("{}", usage.yellow()),
        },
        _ => println!("{}", usage.yellow()),
    }
}

/// One line for an object from `/watch`: its name and what it is doing.
fn watch_summary(kind: &str, o: &serde_json::Value) -> (String, String) {
    let name = o["name"].as_str().unwrap_or("?").to_string();
    let deleting = o["deleting"].as_bool().unwrap_or(false) || o["deletion_requested_at"].is_u64();
    let mut what = match kind {
        "vm" => {
            let state = o["state"].as_str().unwrap_or("?");
            match o["desired_state"].as_str() {
                Some(d) if d != state && !(d == "stopped" && state == "created") => format!("{} → {}", state, d),
                _ => state.to_string(),
            }
        }
        "disk" => o["status"].as_str().unwrap_or("?").to_string(),
        "image" => match o["status"]["state"].as_str().unwrap_or("?") {
            "downloading" => match (o["status"]["received_bytes"].as_u64(), o["status"]["total_bytes"].as_u64()) {
                (Some(r), Some(t)) if t > 0 => format!("downloading {}%", r * 100 / t),
                _ => "downloading".to_string(),
            },
            s => s.to_string(),
        },
        _ => o["phase"].as_str().unwrap_or("ready").replace('_', " "),
    };
    if deleting {
        what = format!("deleting ({})", what);
    }
    if let Some(c) = o["conditions"].as_array().into_iter().flatten().find(|c| c["kind"] == "Ready" && c["status"] != "True") {
        what = format!("{} — {}: {}", what, c["reason"].as_str().unwrap_or(""), c["message"].as_str().unwrap_or(""));
    }
    (name, what)
}

/// `watch [kinds]` (spec/reconciliation.md §12.6): print changes to what
/// the caller can see until Ctrl-C. Reconnects when the server ends a
/// stream, printing only what changed meanwhile.
async fn handle_watch(client: &ApiClient, args: &[&str]) {
    let kinds = args.first().copied().unwrap_or("vms,disks,images,networks");
    let path = format!("/watch?kinds={}", kinds);
    // What was last printed per (kind, id), to skip repeats after a reconnect.
    let mut last: std::collections::HashMap<(String, String), String> = std::collections::HashMap::new();
    let mut synced_once = false;
    println!("{} {} (Ctrl-C to stop)", "Watching".green(), kinds);
    let watch = async {
        loop {
            let mut buf = String::new();
            let mut ended = false;
            let res = client
                .stream(&path, |text| {
                    buf.push_str(text);
                    while let Some(end) = buf.find("\n\n") {
                        let block: String = buf.drain(..end + 2).collect();
                        let (mut event, mut data) = (String::new(), String::new());
                        for line in block.lines() {
                            if let Some(v) = line.strip_prefix("event:") {
                                event = v.trim().to_string();
                            } else if let Some(v) = line.strip_prefix("data:") {
                                data.push_str(v.trim_start());
                            }
                        }
                        let v: serde_json::Value = serde_json::from_str(&data).unwrap_or_default();
                        let (kind, id) = (v["kind"].as_str().unwrap_or("").to_string(), v["id"].as_str().unwrap_or("").to_string());
                        match event.as_str() {
                            "synced" => {
                                if !synced_once {
                                    println!("{} {} objects; changes follow", "Synced:".green(), last.len());
                                    synced_once = true;
                                }
                            }
                            "expired" => ended = true,
                            "added" | "modified" => {
                                let (name, what) = watch_summary(&kind, &v["object"]);
                                let line = format!("{:<8} {:<24} {}", kind, name, what);
                                if last.get(&(kind.clone(), id.clone())) != Some(&line) {
                                    if synced_once {
                                        println!("{}  {}", chrono_now(), line);
                                    }
                                    last.insert((kind, id), line);
                                }
                            }
                            "deleted" => {
                                if let Some(prev) = last.remove(&(kind.clone(), id)) {
                                    let name = prev.split_whitespace().nth(1).unwrap_or("?").to_string();
                                    println!("{}  {:<8} {:<24} {}", chrono_now(), kind, name, "deleted".red());
                                }
                            }
                            _ => {}
                        }
                    }
                    !ended
                })
                .await;
            if let Err(e) = res {
                println!("{} {}", "Error:".red(), e.message);
                return;
            }
            // The server ended the stream (lifetime, restart): follow on.
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    };
    tokio::select! {
        _ = watch => {}
        _ = tokio::signal::ctrl_c() => println!(),
    }
}

/// `HH:MM:SS` (UTC, like `events`), for `watch` lines.
fn chrono_now() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let t = now % 86_400;
    format!("{:02}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
}

/// After a `DELETE …?wait`: gone (no body), or still being deleted by its
/// controller (spec/reconciliation.md §6.3), with the reason it waits.
pub(crate) fn print_delete_result(kind: &str, name: &str, v: &serde_json::Value) {
    if v.is_null() {
        println!("{} {}", format!("{} deleted:", kind).green(), name);
        return;
    }
    println!("{} {} (the control plane finishes it)", "Deleting:".yellow(), name);
    if let Some(ready) = v["conditions"].as_array().into_iter().flatten().find(|c| c["kind"] == "Ready" && c["status"] != "True") {
        println!("  {} {}: {}", "Waiting:".yellow(), ready["reason"].as_str().unwrap_or(""), ready["message"].as_str().unwrap_or(""));
    }
}

/// A disk after a write the disk controller carries out (spec/
/// reconciliation.md §10.1): its state, and what is still pending.
fn print_disk_result(d: &serde_json::Value) {
    println!(
        "  {} {} {} {}",
        d["name"].as_str().unwrap_or("?").cyan(),
        format_bytes(d["size_bytes"].as_u64().unwrap_or(0)),
        d["format"].as_str().unwrap_or(""),
        d["status"].as_str().unwrap_or("")
    );
    if let Some(ready) = d["conditions"].as_array().into_iter().flatten().find(|c| c["kind"] == "Ready" && c["status"] != "True") {
        println!("  {} {}: {}", "Not there yet:".yellow(), ready["reason"].as_str().unwrap_or(""), ready["message"].as_str().unwrap_or(""));
    }
    for w in d["warnings"].as_array().into_iter().flatten() {
        println!("  {} {}", "Warning:".yellow(), w.as_str().unwrap_or(""));
    }
}

async fn handle_disk(client: &CliClient, args: &[&str]) {
    let usage = "Usage: disk list | show <disk> | create <name> [--size-gib N] [--image I] [--full] [--raw] [--no-extend] | resize <disk> <GiB> [--no-extend] | extend-root <disk> [--on-boot] | rm <disk>";
    let gib = |s: &str| s.parse::<u64>().map_err(|_| format!("{} is not a whole number of GiB", s));
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => match client.request_json::<Vec<serde_json::Value>>(Method::GET, &client.scoped("/disks"), None).await {
            Ok(disks) if disks.is_empty() => println!("No disks."),
            Ok(disks) => {
                let vms = client.list_vms().await.unwrap_or_default();
                for d in disks {
                    let vm = d["attached_to"].as_str().map(|id| {
                        vms.iter().find(|v| v.id == id).map(|v| v.name.clone()).unwrap_or_else(|| id.to_string())
                    });
                    let origin = match d["origin"]["kind"].as_str() {
                        Some("image") => format!("image ({})", d["origin"]["mode"].as_str().unwrap_or("")),
                        _ => "blank".to_string(),
                    };
                    println!(
                        "  {:<20} {:>9} {:<6} {:<14} {:<7} {}",
                        d["name"].as_str().unwrap_or("?").cyan(),
                        format_bytes(d["size_bytes"].as_u64().unwrap_or(0)),
                        d["format"].as_str().unwrap_or(""),
                        origin,
                        d["status"].as_str().unwrap_or(""),
                        vm.map(|v| format!("→ {}", v)).unwrap_or_default()
                    );
                }
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },
        "show" | "get" => match args.get(1) {
            Some(name) => match client.request_json::<serde_json::Value>(Method::GET, &format!("/disks/{}", name), None).await {
                Ok(d) => {
                    print_disk_result(&d);
                    println!("  path: {}", d["path"].as_str().unwrap_or(""));
                    if let Some(b) = d["info"]["backing-filename"].as_str() {
                        println!("  backing file: {}", b);
                    }
                    if d["pending_growpart"] == true {
                        println!("  root partition grows on next boot");
                    }
                    let t = &d["partition_table"];
                    match t["partitions"].as_array() {
                        Some(parts) => {
                            println!("  partitions ({}):", t["kind"].as_str().unwrap_or(""));
                            for p in parts {
                                println!(
                                    "    {} start {:>9} size {:>9} {}{}",
                                    p["number"],
                                    format_bytes(p["start_bytes"].as_u64().unwrap_or(0)),
                                    format_bytes(p["size_bytes"].as_u64().unwrap_or(0)),
                                    p["type"].as_str().unwrap_or(""),
                                    if p["is_root"] == true { " (root)" } else { "" }
                                );
                            }
                            println!("  free at end: {}", format_bytes(t["free_tail_bytes"].as_u64().unwrap_or(0)));
                        }
                        None => println!("  no partition table"),
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            None => println!("{}", usage.yellow()),
        },
        "create" => {
            let Some(name) = args.get(1).filter(|a| !a.starts_with("--")) else {
                println!("{}", usage.yellow());
                return;
            };
            let mut body = serde_json::json!({ "name": name });
            if let Some(n) = flag_value(args, "--size-gib") {
                match gib(n) {
                    Ok(n) => body["size_gib"] = serde_json::json!(n),
                    Err(e) => return println!("{} {}", "Error:".red(), e),
                }
            }
            if let Some(i) = flag_value(args, "--image") {
                body["image"] = serde_json::json!(i);
            }
            if has_flag(args, "--full") {
                body["clone"] = serde_json::json!("full");
            }
            if has_flag(args, "--raw") {
                body["format"] = serde_json::json!("raw");
            }
            if has_flag(args, "--no-extend") {
                body["extend_root"] = serde_json::json!(false);
            }
            client.scope_body(&mut body);
            // Making it may wait for its image to download.
            match client.request_json::<serde_json::Value>(Method::POST, "/disks?wait=300", Some(body)).await {
                Ok(d) => {
                    println!("{}", "Disk created:".green());
                    print_disk_result(&d);
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }
        "resize" => match (args.get(1), args.get(2)) {
            (Some(name), Some(size)) => {
                let size = match gib(size) {
                    Ok(n) => n,
                    Err(e) => return println!("{} {}", "Error:".red(), e),
                };
                let mut body = serde_json::json!({ "size_gib": size });
                if has_flag(args, "--no-extend") {
                    body["extend_root"] = serde_json::json!(false);
                }
                match client.request_json::<serde_json::Value>(Method::POST, &format!("/disks/{}/resize?wait=120", name), Some(body)).await {
                    Ok(d) => {
                        println!("{}", "Disk resize:".green());
                        print_disk_result(&d);
                    }
                    Err(e) => println!("{} {}", "Error:".red(), e),
                }
            }
            _ => println!("{}", usage.yellow()),
        },
        "extend-root" => match args.get(1) {
            Some(name) => {
                let mode = if has_flag(args, "--on-boot") { "on-boot" } else { "offline" };
                match client
                    .request_json::<serde_json::Value>(Method::POST, &format!("/disks/{}/extend-root?wait=120", name), Some(serde_json::json!({ "mode": mode })))
                    .await
                {
                    Ok(d) => print_disk_result(&d),
                    Err(e) => println!("{} {}", "Error:".red(), e),
                }
            }
            None => println!("{}", usage.yellow()),
        },
        "rm" | "delete" => match args.get(1) {
            Some(name) => {
                if prompt(&format!("Delete disk {} and its data? [y/N]: ", name)).to_lowercase() != "y" {
                    println!("Cancelled");
                    return;
                }
                match client.request_json::<serde_json::Value>(Method::DELETE, &format!("/disks/{}", name), None).await {
                    Ok(serde_json::Value::Null) => println!("{} {}", "Disk deleted:".green(), name),
                    Ok(_) => println!("{} {} (once the operation on it finishes)", "Deleting:".green(), name),
                    Err(e) => println!("{} {}", "Error:".red(), e),
                }
            }
            None => println!("{}", usage.yellow()),
        },
        _ => println!("{}", usage.yellow()),
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
    println!("  {}  - Start a VM (waits until it runs; --no-wait returns at once)", "start <name|id>".cyan());
    println!("  {}   - Stop a VM", "stop <name|id>".cyan());
    println!(
        "  {} - Shut a VM down via its power button (default wait 60 s), then stop it",
        "stop <name|id> --graceful [secs]".cyan()
    );
    println!("  {}  - Pause a VM", "pause <name|id>".cyan());
    println!("  {} - Connect to VM console (interactive)", "connect <name|id>".cyan());
    println!("  {}     - Show VM serial console log", "log <name|id>".cyan());
    println!("  {}  - What happened to a VM (starts, exits, restarts, adoptions)", "events <name|id>".cyan());
    println!("  {}  - Follow VMs, disks, images and networks as they change (Ctrl-C stops)", "watch [vms,disks,images,networks]".cyan());
    println!("  {} - Delete a VM (and its own root disk)", "delete <name|id> [--keep-disk]".cyan());
    println!("  {} <init|join|join-token|status|promote|snapshot> - Form or inspect the cluster", "cluster".cyan());
    println!("  {}             - List the cluster's nodes (a standalone host has one)", "nodes".cyan());
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
    println!("  {}     - Cloud images in the built-in catalog", "image catalog".cyan());
    println!("  {}        - List downloaded images", "image list".cyan());
    println!("  {} - Download and verify an image", "image pull <catalog-key|https-url> [--name N] [--sha256 H]".cyan());
    println!("  {} - Pull UEFI firmware VMs boot through", "image pull --firmware <key> [--name N]".cyan());
    println!("  {} - Download firmware (QEMU: with its variable store)", "image pull <url> --firmware-for <hv> [--vars-url U] [--vars-sha256 H]".cyan());
    println!("  {}   - Delete an image", "image rm <name|id>".cyan());
    println!("  {}         - List disks", "disk list".cyan());
    println!("  {}  - Disk details and partitions", "disk show <name|id>".cyan());
    println!("  {} - Create a disk", "disk create <name> [--size-gib N] [--image I] [--full] [--raw] [--no-extend]".cyan());
    println!("  {} - Grow or shrink a disk", "disk resize <disk> <GiB> [--no-extend]".cyan());
    println!("  {} - Grow the root partition", "disk extend-root <disk> [--on-boot]".cyan());
    println!("  {}    - Delete a disk", "disk rm <name|id>".cyan());
    println!("  {}            - Check API server health", "health".cyan());
    println!();
    println!("{}", "Access control:".bold());
    println!("  {}            - Who you are: method, teams, roles", "whoami".cyan());
    println!("  {} - Log in over TCP (OIDC device flow or a pasted token)", "login --oidc | --token".cyan());
    println!("  {} - Forget the saved token", "logout [--revoke]".cyan());
    println!("  {}                - Print the web UI address", "ui".cyan());
    println!("  {} - Access tokens", "token list | create <name> [--days N] [--service-account] [--role R[@P]] | revoke <id>".cyan());
    println!("  {} - Projects", "project list | show <p> | create <name> [--description D] | delete <p>".cyan());
    println!("  {} - Quotas (none = unlimited)", "project quota <p> vms=10 memory_mib=none ...".cyan());
    println!("  {} - Set your default project", "project use <p>".cyan());
    println!("  {} - Project role bindings", "binding list <p> | add <p> <role> user:<id> | remove <p> <id>".cyan());
    println!("  {} - Host role bindings", "system-binding list | add <role> <principal> | remove <id>".cyan());
    println!("  {}         - Users and their identities", "user list".cyan());
    println!("  {} - Teams", "team list | create <name> | add-member <team> <user> | remove-member <team> <user>".cyan());
    println!("  {} - Site Cedar policies", "policy list | show <id> | put <id> <file> [--disable] | delete <id> | validate <id> <file> | history <id>".cyan());
    println!("  {} - Audit log", "audit [--project P] [--since <unix-ms>] [--limit N] [--user U]".cyan());
    println!(
        "  {} - Resource usage (default: this billing month by project)",
        "usage [--project P] [--from D] [--to D] [--by project,vm,disk,nic,network] [--granularity hour|day|month] [--meters m,…] [--tz Z] [--csv]".cyan()
    );
    println!(
        "  {} - Average, 30-second peak and 95th percentile per billing month",
        "usage bandwidth|disk-io|compute [--month YYYY-MM] [--by project,vm,nic,network|disk] [--csv]".cyan()
    );
    println!("  {} - A VM's current CPU, memory, NIC and disk rates", "stats <vm>".cyan());
    println!("  {} - 5-minute series with the 95th percentile", "bandwidth <vm> | --network N · io <vm> | --disk D · compute <vm>".cyan());
    println!("  {} - Project networks", "network create <name> --project P [--isolated] [--vhost-user] | share <net> <project-id> | unshare <net> <project-id>".cyan());
    println!("  {} - Shares offered to a project", "network shares <p> | accept <p> <net> | leave <p> <net>".cyan());
    println!();
    println!("  {}              - Show this help", "help".cyan());
    println!("  {}              - Exit the CLI", "exit".cyan());
    println!();
    println!(
        "{} You can use either VM name or ID for commands. Global --project P scopes creates, lists and name lookups.",
        "Note:".dimmed()
    );
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

/// The signed-in user's name (the login name for local accounts).
async fn signed_in_name(client: &CliClient) -> Option<String> {
    let w: serde_json::Value = client.request_json(Method::GET, "/auth/whoami", None).await.ok()?;
    w["user"]["display_name"].as_str().filter(|n| !n.is_empty()).map(str::to_string)
}

/// The login-credential answer: empty takes the default (if any),
/// `none` means no login.
fn credential_choice(answer: &str, default: Option<&str>) -> Option<String> {
    match answer.trim() {
        "" => default.map(str::to_string),
        a if a.eq_ignore_ascii_case("none") => None,
        a => Some(a.to_string()),
    }
}

/// Your own public key files (`~/.ssh/*.pub` that hold a public key),
/// offered as the default for a new credential.
fn own_public_key_files() -> Vec<String> {
    match std::env::var_os("HOME") {
        Some(home) => public_key_files_in(std::path::Path::new(&home)),
        None => Vec::new(),
    }
}

fn public_key_files_in(home: &std::path::Path) -> Vec<String> {
    let dir = home.join(".ssh");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut files: Vec<String> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "pub"))
        .filter(|p| read_public_key_files(&p.to_string_lossy()).is_ok_and(|k| !k.is_empty()))
        .map(|p| format!("~/.ssh/{}", p.file_name().unwrap_or_default().to_string_lossy()))
        .collect();
    files.sort();
    files
}

/// The key-files answer for `credential-add`: empty takes the offered
/// defaults, `none` means no keys.
fn chosen_key_files(answer: &str, defaults: &[String]) -> Option<String> {
    match answer.trim() {
        "" if defaults.is_empty() => None,
        "" => Some(defaults.join(",")),
        a if a.eq_ignore_ascii_case("none") => None,
        a => Some(a.to_string()),
    }
}

async fn handle_credential_add(client: &CliClient) {
    let me = std::env::var("USER").unwrap_or_default();
    let username = if me.is_empty() {
        prompt("Username: ")
    } else {
        let u = prompt(&format!("Username [{}]: ", me));
        if u.is_empty() { me } else { u }
    };
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
    // Offer your own ~/.ssh/*.pub by default.
    let defaults = own_public_key_files();
    let question = if defaults.is_empty() {
        "SSH public key files (optional, comma-separated, e.g. ~/.ssh/id_ed25519.pub): ".to_string()
    } else {
        format!("SSH public key files (comma-separated; 'none' for no keys) [{}]: ", defaults.join(", "))
    };
    let keys = match chosen_key_files(&prompt_path(&question), &defaults)
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
    "help", "exit", "quit", "list", "ls", "get", "create", "start", "stop", "pause", "events", "watch",
    "connect", "console", "attach", "log", "logs", "delete", "rm", "pci", "pci-devices", "nodes", "node", "cluster",
    "attach-device", "detach-device", "credentials", "creds", "credential-add",
    "credential-passwd", "credential-keys", "credential-rm", "networks", "network-add",
    "network-rm", "bridges", "uplinks", "uplink-add", "uplink-rm", "ovs", "health",
    "image", "images", "disk", "disks", "whoami", "login", "logout", "ui", "token",
    "tokens", "project", "projects", "binding", "bindings", "system-binding", "user",
    "users", "team", "teams", "policy", "policies", "audit", "usage", "stats", "bandwidth", "io", "compute", "network",
];

/// Subcommands offered by Tab after these commands.
const SUBCOMMANDS: &[(&str, &[&str])] = &[
    ("image", &["catalog", "list", "pull", "rm"]),
    ("disk", &["list", "show", "create", "resize", "extend-root", "rm"]),
    ("ovs", &["status", "install", "dpdk-init"]),
    ("cluster", &["init", "join", "join-token", "status", "promote", "snapshot"]),
    ("login", &["--oidc", "--token"]),
    ("logout", &["--revoke"]),
    ("token", &["list", "create", "revoke"]),
    ("project", &["list", "show", "create", "delete", "quota", "use"]),
    ("binding", &["list", "add", "remove"]),
    ("system-binding", &["list", "add", "remove"]),
    ("user", &["list"]),
    ("team", &["list", "create", "delete", "add-member", "remove-member"]),
    ("policy", &["list", "show", "put", "delete", "validate", "history", "reload"]),
    ("network", &["list", "create", "rm", "share", "unshare", "shares", "accept", "leave"]),
];

/// Commands whose argument at this (0-based, after the command) index is a
/// filesystem path.
const PATH_ARGS: &[(&str, usize)] = &[("attach-device", 1), ("detach-device", 1), ("policy", 2)];

/// The subcommands of `cmd` (aliases share their command's list).
fn subcommands(cmd: &str) -> &'static [&'static str] {
    let cmd = match cmd {
        "images" => "image",
        "disks" => "disk",
        "tokens" => "token",
        "projects" => "project",
        "bindings" => "binding",
        "users" => "user",
        "teams" => "team",
        "policies" => "policy",
        c => c,
    };
    SUBCOMMANDS.iter().find(|(c, _)| *c == cmd).map(|(_, s)| *s).unwrap_or(&[])
}

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
    if arg_index == 0 {
        let prefix = if starting_new_word { "" } else { words[words.len() - 1] };
        let subs = subcommands(words[0]);
        if !subs.is_empty() {
            let candidates = subs
                .iter()
                .filter(|c| c.starts_with(prefix))
                .map(|c| Pair { display: c.to_string(), replacement: format!("{} ", c) })
                .collect();
            return Ok((pos - prefix.len(), candidates));
        }
    }
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

    let vcpu_count: u8 = prompt("vCPU count [1]: ").parse().unwrap_or(1);

    let mem_size_mib: u32 = prompt("Memory (MiB) [512]: ").parse().unwrap_or(512);

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

    // A disk image boots through UEFI firmware: a firmware image built for
    // the hypervisor (spec images.md §7), or a host path (admins only).
    let (firmware, firmware_path) = {
        let ty = match hypervisor.as_deref() {
            Some("qemu") => "qemu",
            _ => "cloudhypervisor",
        };
        let ready: Vec<String> = client
            .request_json::<Vec<serde_json::Value>>(Method::GET, "/images", None)
            .await
            .unwrap_or_default()
            .into_iter()
            .rev() // newest first
            .filter(|i| i["kind"] == "firmware" && i["hypervisor"] == ty && i["status"]["state"] == "ready" && i["deleting"] != true)
            .filter_map(|i| i["name"].as_str().map(str::to_string))
            .collect();
        if ready.is_empty() {
            let key = if ty == "qemu" { "ovmf" } else { "cloudhv-edk2" };
            println!("{} no firmware image for {}; pull one with 'image pull --firmware {}' for firmware boot", "Note:".yellow(), ty, key);
        }
        let default = ready.first().cloned().unwrap_or_else(|| "none".to_string());
        let options = if ready.is_empty() { String::new() } else { format!("{}, ", ready.join(", ")) };
        match prompt_path(&format!("UEFI firmware [{}'none' for kernel boot, or a host path] [{}]: ", options, default)).as_str() {
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
            println!("{}", "Error: kernel image path is required for kernel boot".red());
            return;
        }
        path
    };

    // Boot disk. Firmware boot offers managed images first (spec
    // images.md §10); kernel boot takes a bare root filesystem image.
    let (mut rootfs_path, mut image, mut root_disk_size_gib, mut root_disk) = (String::new(), None, None, None);
    let mut choice = "path".to_string();
    if firmware_boot {
        let ready: Vec<String> = client
            .request_json::<Vec<serde_json::Value>>(Method::GET, "/images", None)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|i| i["status"]["state"] == "ready" && i["kind"] != "firmware")
            .filter_map(|i| i["name"].as_str().map(str::to_string))
            .collect();
        let default = if ready.is_empty() { "path" } else { "image" };
        choice = prompt(&format!("Boot disk from [image/disk/path] [{}]: ", default)).to_lowercase();
        if choice.is_empty() {
            choice = default.to_string();
        }
        match choice.as_str() {
            "image" | "i" => {
                if ready.is_empty() {
                    println!("{}", "Error: no downloaded images; pull one with 'image pull <key>'".red());
                    return;
                }
                let pick = prompt(&format!("Image ({}) [{}]: ", ready.join(", "), ready[0]));
                image = Some(if pick.is_empty() { ready[0].clone() } else { pick });
                root_disk_size_gib = match prompt("Root disk size in GiB [10]: ").as_str() {
                    "" => None,
                    s => match s.parse::<u64>() {
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
    let data_disks = prompt_optional("Data disks (optional, comma-separated disk names): ").map(|s| {
        s.split(',').map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).collect::<Vec<_>>()
    });

    let cloud_init_path = if firmware_boot {
        prompt_path_optional("cloud-init seed image (optional, default: auto-generated): ")
    } else {
        None
    };

    // A stored credential is provisioned through the generated seed only.
    let credential = if firmware_boot && cloud_init_path.is_none() {
        let names: Vec<String> = client
            .list_credentials()
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
                None => prompt(&format!("Login credential (one of: {}; empty: none, no login): ", names.join(", "))),
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
    let networks = {
        let names: Vec<String> = client
            .request_json::<Vec<NetworkRow>>(Method::GET, "/networks", None)
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
        firmware,
        firmware_path,
        cloud_init_path,
        credential,
        rootfs_path,
        image,
        root_disk_size_gib,
        root_disk,
        data_disks: data_disks.filter(|v| !v.is_empty()),
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
        "failed" | "unknown" => state.red().bold().to_string(),
        _ => state.to_string(),
    }
}

/// `state`, plus `→ desired` while the VM is still getting there.
fn format_vm_state(vm: &VmResponse) -> String {
    let settled = match vm.desired_state.as_str() {
        "" => true,
        "stopped" => matches!(vm.state.as_str(), "stopped" | "created"),
        d => d == vm.state,
    };
    if settled {
        format_state(&vm.state)
    } else {
        format!("{} → {}", format_state(&vm.state), vm.desired_state)
    }
}

/// `format_vm_state` without colours, for tables.
fn plain_vm_state(vm: &VmResponse) -> String {
    let settled = match vm.desired_state.as_str() {
        "" => true,
        "stopped" => matches!(vm.state.as_str(), "stopped" | "created"),
        d => d == vm.state,
    };
    if settled {
        vm.state.clone()
    } else {
        format!("{} → {}", vm.state, vm.desired_state)
    }
}

/// What the controller says is in the way (the `Ready` condition), if
/// the VM has not converged.
fn not_ready_reason(vm: &VmResponse) -> Option<String> {
    let ready = vm.conditions.iter().find(|c| c["kind"] == "Ready")?;
    if ready["status"] == "True" {
        return None;
    }
    let msg = ready["message"].as_str().unwrap_or("");
    Some(format!("{}{}", ready["reason"].as_str().unwrap_or(""), if msg.is_empty() { String::new() } else { format!(": {}", msg) }))
}

/// How long lifecycle commands wait for the VM to get there, or `None`
/// with `--no-wait` (spec/reconciliation.md §12.3).
fn wait_secs(args: &[&str]) -> Option<u64> {
    (!has_flag(args, "--no-wait")).then_some(60)
}

fn print_lifecycle_result(vm: &VmResponse) {
    println!("{} VM {} is now {}", "Success:".green(), vm.name, format_vm_state(vm));
    if let Some(r) = not_ready_reason(vm) {
        println!("  {} {}", "Not there yet:".yellow(), r);
    }
}

async fn handle_command(line: &str, client: &CliClient) -> bool {
    match split_line(line) {
        Ok(words) => handle_words(&words, client).await,
        Err(e) => {
            println!("{} {}", "Error:".red(), e);
            true
        }
    }
}

/// Run one command; `false` to leave the shell.
async fn handle_words(words: &[String], client: &CliClient) -> bool {
    let parts: Vec<&str> = words.iter().map(String::as_str).collect();
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
                    let names = project_names(client).await;
                    let rows: Vec<VmRow> = vms
                        .into_iter()
                        .map(|vm| VmRow {
                            state: plain_vm_state(&vm),
                            project: names.get(&vm.project).cloned().unwrap_or(vm.project),
                            node: vm.node.unwrap_or_default(),
                            id: vm.id,
                            name: vm.name,
                            vcpu_count: vm.vcpu_count,
                            mem_size_mib: vm.mem_size_mib,
                            hypervisor: vm.hypervisor,
                        })
                        .collect();
                    println!("{}", Table::new(rows));
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
                    let names = project_names(client).await;
                    println!("  Project:    {}", names.get(&vm.project).map(|n| format!("{} ({})", n, vm.project)).unwrap_or(vm.project.clone()));
                    println!("  State:      {}", format_vm_state(&vm));
                    if let Some(r) = not_ready_reason(&vm) {
                        println!("  Waiting:    {}", r);
                    }
                    if let Some(e) = &vm.last_exit {
                        println!("  Last exit:  {} ({})", e["cause"].as_str().unwrap_or("?"), e["instance_id"].as_str().unwrap_or(""));
                    }
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
                println!("{}", "Usage: start <name|id> [--no-wait]".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.start_vm(&vm_id, wait_secs(&parts[2..])).await {
                Ok(vm) => print_lifecycle_result(&vm),
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "stop" => {
            if parts.len() < 2 {
                println!("{}", "Usage: stop <name|id> [--graceful [secs]] [--no-wait]".yellow());
                return true;
            }
            let flags: Vec<&str> = parts[2..].iter().copied().filter(|p| *p != "--no-wait").collect();
            let graceful = match graceful_stop_secs(&flags) {
                Ok(g) => g,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            if let Some(secs) = graceful {
                println!("Pressing the power button; waiting up to {} s for the guest to shut down...", secs);
            }
            match client.stop_vm(&vm_id, graceful, wait_secs(&parts[2..])).await {
                Ok(vm) => print_lifecycle_result(&vm),
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "pause" => {
            if parts.len() < 2 {
                println!("{}", "Usage: pause <name|id> [--no-wait]".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.pause_vm(&vm_id, wait_secs(&parts[2..])).await {
                Ok(vm) => print_lifecycle_result(&vm),
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

        "events" => {
            if parts.len() < 2 {
                println!("{}", "Usage: events <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.vm_events(&vm_id).await {
                Ok(v) => {
                    for e in v["events"].as_array().into_iter().flatten() {
                        let reason = e["reason"].as_str().unwrap_or("");
                        let reason = if e["kind"] == "warning" { reason.yellow().to_string() } else { reason.to_string() };
                        println!(
                            "{}  {:<12} {:<18} {}",
                            format_unix_time(e["at"].as_u64().unwrap_or(0)),
                            e["actor"].as_str().unwrap_or(""),
                            reason,
                            e["message"].as_str().unwrap_or("")
                        );
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "watch" => handle_watch(client, &parts[1..]).await,

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
            let keep_disk = has_flag(&parts[2..], "--keep-disk");
            let confirm = prompt(&format!(
                "Are you sure you want to delete VM {}{}? [y/N]: ",
                parts[1],
                if keep_disk { "" } else { " (and a root disk created for it)" }
            ));
            if confirm.to_lowercase() == "y" {
                match client.delete_vm(&vm_id, keep_disk, wait_secs(&parts[2..])).await {
                    Ok(()) => println!("{} VM deleted", "Success:".green()),
                    Err(e) => println!("{} {}", "Error:".red(), e),
                }
            } else {
                println!("Cancelled");
            }
        }

        "nodes" | "node" if matches!(parts.get(1), Some(&"drain") | Some(&"undrain")) => {
            let Some(id) = parts.get(2) else {
                println!("{}", format!("Usage: node {} <node>", parts[1]).yellow());
                return true;
            };
            // Accept a node's name as well as its id.
            let nodes: serde_json::Value = client.request_json(Method::GET, "/nodes", None).await.unwrap_or_default();
            let found = nodes.as_array().into_iter().flatten().find(|n| n["meta"]["id"] == *id || n["spec"]["name"] == *id).and_then(|n| n["meta"]["id"].as_str().map(String::from));
            let Some(nid) = found else {
                println!("{} no node {}", "Error:".red(), id);
                return true;
            };
            match client.request_json::<serde_json::Value>(Method::POST, &format!("/nodes/{}/{}", nid, parts[1]), None).await {
                Ok(v) => {
                    println!("{} {} is now {}", "OK".green(), v["node"].as_str().unwrap_or(""), v["phase"].as_str().unwrap_or(""));
                    let r = &v["remaining"];
                    println!("  still on it: {} VMs, {} disks, {} networks", r["vms"].as_array().map_or(0, |a| a.len()), r["disks"].as_array().map_or(0, |a| a.len()), r["networks"].as_array().map_or(0, |a| a.len()));
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }
        "nodes" | "node" => {
            let path = match parts.get(1) {
                Some(id) => format!("/nodes/{}", id),
                None => "/nodes".to_string(),
            };
            match client.request_json::<serde_json::Value>(Method::GET, &path, None).await {
                Ok(v) if v.is_array() => {
                    let rows: Vec<NodeRow> = v.as_array().unwrap().iter().map(NodeRow::from_json).collect();
                    println!("{}", Table::new(rows));
                }
                Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
                Err(e) => println!("{} {}", "Error:".red(), e),
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

        "networks" | "nets" => list_networks(client).await,

        "network" | "net" => admin::network(client, &parts[1..]).await,
        "whoami" => admin::whoami(client).await,
        "cluster" => admin::cluster(client, &parts[1..]).await,
        "login" => admin::login(client, &parts[1..]).await,
        "logout" => admin::logout(client, &parts[1..]).await,
        "ui" => admin::ui(),
        "token" | "tokens" => admin::token(client, &parts[1..]).await,
        "project" | "projects" => admin::project(client, &parts[1..]).await,
        "binding" | "bindings" => admin::binding(client, &parts[1..]).await,
        "system-binding" => admin::system_binding(client, &parts[1..]).await,
        "user" | "users" => admin::user(client, &parts[1..]).await,
        "team" | "teams" => admin::team(client, &parts[1..]).await,
        "policy" | "policies" => admin::policy(client, &parts[1..]).await,
        "audit" => admin::audit(client, &parts[1..]).await,
        "usage" => admin::resource_usage(client, &parts[1..]).await,
        "stats" => admin::stats(client, &parts[1..]).await,
        "bandwidth" => admin::series(client, "bandwidth", &parts[1..]).await,
        "io" => admin::series(client, "io", &parts[1..]).await,
        "compute" => admin::series(client, "compute", &parts[1..]).await,

        "network-add" | "net-add" => handle_network_add(client, &parts[1..]).await,

        "network-rm" | "net-rm" => match parts.get(1) {
            Some(name) => match client
                .request_json::<serde_json::Value>(Method::DELETE, &format!("/networks/{}?wait=60", name), None)
                .await
            {
                Ok(v) => print_delete_result("Network", name, &v),
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            None => println!("{}", "Usage: network-rm <name>".yellow()),
        },

        "bridges" => match client
            .request_json::<Vec<serde_json::Value>>(Method::GET, "/ovs/bridges", None)
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
                .request_json::<Vec<serde_json::Value>>(Method::GET, &format!("/ovs/bridges/{}/uplinks", bridge), None)
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
                .request_json::<()>(Method::DELETE, &format!("/ovs/bridges/{}/uplinks/{}", bridge, name), None)
                .await
            {
                Ok(()) => println!("{} uplink {} (NIC restored)", "Removed:".green(), name),
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            _ => println!("{}", "Usage: uplink-rm <bridge> <name>".yellow()),
        },

        "image" | "images" => handle_image(client, &parts[1..]).await,

        "disk" | "disks" => handle_disk(client, &parts[1..]).await,

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
    let client = match build_client(&cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{} {}", "Error:".red(), e);
            std::process::exit(2);
        }
    };

    if !cli.command.is_empty() {
        handle_words(&cli.command, &client).await;
        return;
    }

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

    println!("Connected to: {}", client.describe().yellow());
    if client.is_unix() {
        println!("{} identified by your Unix user (see 'whoami')", "Auth:".dimmed());
    } else if !client.has_token() {
        println!("{} no token; run 'login --oidc' or 'login --token' (or set GLIDEX_TOKEN)", "Auth:".dimmed());
    }
    if let Some(p) = client.project() {
        println!("Project: {}", p.cyan());
    }
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

    #[test]
    fn credential_choice_defaults_to_mine() {
        assert_eq!(credential_choice("", Some("alice")).as_deref(), Some("alice"));
        assert_eq!(credential_choice("none", Some("alice")), None);
        assert_eq!(credential_choice("bob", Some("alice")).as_deref(), Some("bob"));
        assert_eq!(credential_choice("", None), None);
    }

    #[test]
    fn credential_key_file_answers() {
        let d = vec!["~/.ssh/id_ed25519.pub".to_string(), "~/.ssh/id_rsa.pub".to_string()];
        assert_eq!(chosen_key_files("", &d).as_deref(), Some("~/.ssh/id_ed25519.pub,~/.ssh/id_rsa.pub"));
        assert_eq!(chosen_key_files("none", &d), None);
        assert_eq!(chosen_key_files("NONE", &d), None);
        assert_eq!(chosen_key_files("/tmp/k.pub", &d).as_deref(), Some("/tmp/k.pub"));
        assert_eq!(chosen_key_files("", &[]), None);
    }

    #[test]
    fn own_public_keys_skip_private_and_junk() {
        let dir = tempfile::TempDir::new().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(ssh.join("id_ed25519.pub"), "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA me@host\n").unwrap();
        std::fs::write(ssh.join("id_ed25519"), "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        std::fs::write(ssh.join("bad.pub"), "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        std::fs::write(ssh.join("empty.pub"), "\n").unwrap();
        let files = public_key_files_in(dir.path());
        assert_eq!(files, vec!["~/.ssh/id_ed25519.pub"]);
    }

    #[test]
    fn graceful_stop_arguments() {
        assert_eq!(graceful_stop_secs(&[]), Ok(None));
        assert_eq!(graceful_stop_secs(&["--graceful"]), Ok(Some(60)));
        assert_eq!(graceful_stop_secs(&["--graceful", "15"]), Ok(Some(15)));
        assert!(graceful_stop_secs(&["--graceful", "soon"]).is_err());
        assert!(graceful_stop_secs(&["--force"]).is_err());
    }

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
    fn splits_lines_with_quotes() {
        assert_eq!(split_line("  list  ").unwrap(), vec!["list"]);
        assert_eq!(
            split_line(r#"project create lab --description "Lab VMs" 'a b' c\ d"#).unwrap(),
            vec!["project", "create", "lab", "--description", "Lab VMs", "a b", "c d"]
        );
        assert_eq!(split_line(r#"x """#).unwrap(), vec!["x", ""]);
        assert!(split_line("say 'oops").is_err());
        assert_eq!(flag_values(&["--role", "a", "x", "--role", "b@p"], "--role"), vec!["a", "b@p"]);
    }

    #[test]
    fn completes_subcommands() {
        let files = FilenameCompleter::new();
        let (start, got) = replacements(complete_command_line(&files, "token ", 6));
        assert_eq!(start, 6);
        assert_eq!(got, vec!["create ", "list ", "revoke "]);
        let (start, got) = replacements(complete_command_line(&files, "projects q", 10));
        assert_eq!(start, 9);
        assert_eq!(got, vec!["quota "]);
        // Second argument: no subcommands.
        assert!(replacements(complete_command_line(&files, "token create ", 13)).1.is_empty());
        for (cmd, _) in SUBCOMMANDS {
            assert!(COMMANDS.contains(cmd), "{cmd} is missing from COMMANDS");
        }
    }

    /// A control plane (auth disabled) on a temporary api.sock.
    async fn serve_temp() -> (tempfile::TempDir, std::path::PathBuf, tokio::sync::watch::Sender<bool>) {
        use glidex_control_plane::api::{create_router, Listener};
        use glidex_control_plane::serve::{bind_unix, serve_unix};
        use glidex_control_plane::state::VmManager;
        let dir = tempfile::TempDir::new().unwrap();
        let manager = VmManager::with_db_path(dir.path().join("t.db")).unwrap();
        let sock = dir.path().join("api.sock");
        let l = bind_unix(&sock).unwrap();
        let (tx, rx) = tokio::sync::watch::channel(false);
        tokio::spawn(serve_unix(l, create_router(manager), Listener::Api, rx));
        (dir, sock, tx)
    }

    #[tokio::test]
    async fn api_calls_over_the_unix_socket() {
        let (_dir, sock, stop) = serve_temp().await;
        assert_eq!(client::select_socket(None, std::slice::from_ref(&sock), client::is_socket), Some(sock.clone()));
        let client = ApiClient::unix(sock);

        client.health_check().await.unwrap();
        assert!(client.list_vms().await.unwrap().is_empty());

        let who: serde_json::Value = client.request_json(Method::GET, "/auth/whoami", None).await.unwrap();
        assert_eq!(who["method"], "disabled");

        let req = CreateCredentialRequest {
            username: "alice".into(),
            password: None,
            ssh_authorized_keys: vec!["ssh-ed25519 AAAAC3Nza alice@host".into()],
        };
        let c = client.create_credential(&req).await.unwrap();
        assert_eq!(c.username, "alice");
        assert_eq!(c.ssh_authorized_keys.len(), 1);

        // Scoped to a project by name: ?project= on lists and /credentials/{user}.
        let projects: Vec<serde_json::Value> = client.request_json(Method::GET, "/projects", None).await.unwrap();
        let project = projects[0]["name"].as_str().unwrap().to_string();
        client.set_project(Some(project.clone()));
        let creds = client.list_credentials().await.unwrap();
        assert_eq!(creds.iter().map(|c| c.username.as_str()).collect::<Vec<_>>(), ["alice"]);
        let one: serde_json::Value = client.request_json(Method::GET, &client.scoped("/credentials/alice"), None).await.unwrap();
        assert_eq!(one["username"], "alice");
        assert_eq!(one["project"], projects[0]["id"]);
        assert!(client.list_vms().await.unwrap().is_empty());

        // A project that doesn't exist is an API error, rendered.
        client.set_project(Some("no-such-project".into()));
        let e = client.list_credentials().await.unwrap_err();
        assert!(e.starts_with("not_found:"), "{e}");
        client.set_project(None);

        let e = client.request::<serde_json::Value>(Method::GET, "/vms/nope", None).await.unwrap_err();
        assert_eq!(e.status, Some(hyper::StatusCode::NOT_FOUND));
        assert!(client.resolve_vm("nope").await.unwrap_err().contains("not found"));

        client.update_credential("alice", &UpdateCredentialRequest { password: None, ssh_authorized_keys: Some(vec!["ssh-ed25519 BBBBC3Nza alice@new".into()]) }).await.unwrap();
        client.delete_credential("alice").await.unwrap();
        assert!(client.list_credentials().await.unwrap().is_empty());

        // The console WebSocket goes over the same socket; errors come back
        // as API errors.
        let e = console::open_ws(&client, "/vms/nope/console/ws").await.err().unwrap();
        assert!(e.contains("not_found"), "{e}");
        let e = client.request_bytes(Method::GET, "/vms/nope/console/log", None).await.err().unwrap();
        assert_eq!(e.status, Some(hyper::StatusCode::NOT_FOUND));

        let _ = stop.send(true);
    }

    #[tokio::test]
    async fn unreachable_socket_says_so() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = ApiClient::unix(dir.path().join("api.sock"));
        let e = client.health_check().await.unwrap_err();
        assert!(e.contains("is glidex-control-plane running?"), "{e}");
    }

    #[test]
    fn display_option_renders_dash_for_none() {
        assert_eq!(display_option(&None), "-");
        assert_eq!(display_option(&Some("vfio-pci".to_string())), "vfio-pci");
    }
}
