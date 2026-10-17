//! `gxctl`: interactive CLI for the glidex control plane (spec/cli.md).

mod admin;
mod client;
mod config;
mod console;
mod directory;
mod storage;
mod tenancy;
mod vm;

use clap::Parser;
use client::ApiClient;
use colored::Colorize;
use console::{handle_connect, handle_log, restore_terminal};
use hyper::Method;
use nix::sys::termios::{self, LocalFlags, SetArg};
use rustyline::completion::{Completer, FilenameCompleter, Pair, unescape};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{CompletionType, Config, Context, Editor, Helper};
use serde::Deserialize;
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

    /// Use this profile from ~/.config/glidex/config.json (created by
    /// `gxctl auth login`) instead of its `current` one: it decides the
    /// target URLs, the TLS trust and the credential. `--url` then still
    /// overrides the target for this run only, keeping the profile's
    /// trust; the credential goes along only if the host is one the
    /// profile itself dials.
    #[arg(long, env = "GLIDEX_PROFILE")]
    profile: Option<String>,

    /// Project (id or name) for creates, lists and name lookups;
    /// default: your default project (`project use`).
    #[arg(short, long, env = "GLIDEX_PROJECT")]
    project: Option<String>,

    /// Run one command and exit instead of starting the shell, e.g.
    /// `gxctl list` or `gxctl --url https://… login --oidc`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

/// Pick the transport (spec/cli.md "Transport") under gxctl-auth.md §3's
/// ladder: a profile — from `--profile`/`GLIDEX_PROFILE` or the file's
/// `current` — decides target, trust and credential; with none, today's
/// behaviour exactly: `--url` → TCP, else `--socket`/`GLIDEX_SOCKET`/the
/// first existing default socket, else TCP to localhost.
fn build_client(cli: &Cli) -> Result<ApiClient, String> {
    let cfg = config::load()?;
    if let Some(cfg) = cfg.as_ref().filter(|c| c.has_any())
        && let Some(sel) = config::select(cfg, cli.profile.as_deref())?
    {
        return profile_client(cli, &sel);
    }
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
                // A UI socket but no API socket: a cluster agent, which has
                // no API of its own (spec/clustering-ui.md §3.6).
                None if client::is_socket(std::path::Path::new(client::AGENT_UI_SOCKET)) => {
                    return Err(client::AGENT_HINT.into());
                }
                None => tcp(client::DEFAULT_URL)?,
            }
        }
    };
    c.set_project(cli.project.clone());
    Ok(c)
}

/// A profile is in effect: it decides the endpoints and the trust; the
/// environment still tops the credential (scripts pinning `GLIDEX_TOKEN`
/// keep working unchanged), and an ad-hoc `--url` carries a credential
/// only when its host is one the profile itself dials — so debugging at
/// another address cannot leak the prod token (§3's closing note).
fn profile_client(cli: &Cli, sel: &config::Selected) -> Result<ApiClient, String> {
    let p = sel.profile;
    let urls: Vec<String> = match &cli.url {
        Some(u) => vec![u.clone()],
        None => p.url.clone(),
    };
    let dials_known = urls.iter().all(|u| {
        p.url
            .iter()
            .any(|pu| config::url_host(pu) == config::url_host(u))
    });
    let mut token = client::token_from_env();
    if token.is_none() && dials_known {
        token = p.credential()?;
    }
    if token.is_none() && !dials_known {
        // The point of the rule, said out loud once per run.
        eprintln!(
            "note: --url dials a host outside profile '{}'; no credential is sent to it",
            sel.name
        );
    }
    let loopback_only = urls
        .iter()
        .all(|u| client::is_loopback_host(&config::url_host(u)));
    if token.is_none() && loopback_only {
        // The legacy token file is the implicit localhost profile's
        // credential (§3.5); `auth login` folds a copy into the profile,
        // and until then the banner says where the credential came from.
        token = config::legacy_token()?;
        if token.is_some() {
            eprintln!(
                "note: credential from ~/.config/glidex/token (legacy); 'gxctl auth login --profile {}' folds it into the profile",
                sel.name
            );
        }
    }
    // The CI-only one-run grant (§5.4): never when the profile carries
    // pins or a CA — those are the operator's decision, removable only
    // through `auth trust remove` — and never for a login, which reads
    // its own flags, not this environment.
    let mut tls = p.tls.clone();
    if !tls.insecure
        && tls.pins.is_empty()
        && tls.ca_file.is_none()
        && tls.ca_pem.is_none()
        && let Ok(v) = std::env::var("GLIDEX_TLS_INSECURE")
        && v.trim() == "1"
    {
        tls.insecure = true;
    }
    if tls.insecure {
        // The bypass's loud line, before this invocation's first request
        // (§5.4) — every run, not just the ones that notice it.
        eprintln!(
            "{} UNVERIFIED TLS: the certificate of {} was not checked — a network attacker can read your token",
            "Warning:".red(),
            urls.join(", ")
        );
    }
    let mut banner = format!(
        "profile '{}' · {}",
        sel.name,
        if tls.insecure {
            "UNVERIFIED".to_string()
        } else {
            p.trust_mode()
        }
    );
    if let Some(c) = p.cluster_name.as_ref().or(p.cluster_id.as_ref()) {
        banner.push_str(&format!(" · cluster {c}"));
    }
    if let Some(id) = &p.identity
        && let Some(tn) = id.token_name.as_ref()
    {
        banner.push_str(&format!(" · token {tn}"));
    }
    let c = ApiClient::tcp_multi(
        &urls,
        token,
        Some(&tls),
        Some(client::ProfileCtx {
            name: sel.name.clone(),
            path: config::file().unwrap_or_else(|| PathBuf::from("")),
            banner,
        }),
        Some(client::Binding {
            profile: sel.name.clone(),
            cluster_id: p.cluster_id.clone(),
        }),
    )?;
    c.set_project(cli.project.clone().or_else(|| p.project.clone()));
    Ok(c)
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
        .filter_map(|p| {
            Some((
                p["id"].as_str()?.to_string(),
                p["name"].as_str()?.to_string(),
            ))
        })
        .collect()
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
    match client
        .request_json::<Vec<NetworkRow>>(Method::GET, "/networks", None)
        .await
    {
        Ok(nets) if nets.is_empty() => {
            println!("No networks. Create one with 'network create <name>'.")
        }
        Ok(mut nets) => {
            let names = project_names(client).await;
            for n in &mut nets {
                if let Some(p) = n.project.as_mut()
                    && let Some(name) = names.get(p.as_str())
                {
                    *p = name.clone();
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
    args.iter()
        .position(|a| *a == flag)
        .and_then(|i| args.get(i + 1))
        .copied()
}

/// Every value of a repeatable `--flag value`.
fn flag_values<'a>(args: &[&'a str], flag: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|w| w[0] == flag)
        .map(|w| w[1])
        .collect()
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

fn has_flag(args: &[&str], flag: &str) -> bool {
    args.contains(&flag)
}

async fn handle_network_add(client: &CliClient, args: &[&str]) {
    let Some(name) = args.first().filter(|a| !a.starts_with("--")) else {
        println!("{}", "Usage: network-add <name> [--nat [--subnet CIDR] | --isolated | --bridged <bridge>] [--vhost-user] [--vlan N] [--bridge NAME] [--cluster | --node]".yellow());
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
    // Cluster (OVN) or one node's bridge; default: the cluster's when OVN is enabled.
    if has_flag(args, "--cluster") {
        body["scope"] = serde_json::json!("cluster");
    } else if has_flag(args, "--node") {
        body["scope"] = serde_json::json!("node");
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
    match client
        .request_json::<NetworkRow>(Method::POST, "/networks", Some(body))
        .await
    {
        Ok(n) => println!(
            "{} {} ({} on {})",
            "Network created:".green(),
            n.name.yellow(),
            n.mode,
            n.bridge
        ),
        Err(e) => println!("{} {}", "Error:".red(), e),
    }
}

async fn handle_uplink_add(client: &CliClient, args: &[&str]) {
    let usage = "Usage: uplink-add <bridge> (--kernel <if> | --afxdp <if> [--xdp-mode best_effort|native|native_with_zerocopy|generic] [--rxq N] | --dpdk <bdf> --name <port> [--rxq N]) [--migrate-ip] [--force] [--accept-dhcp]";
    let Some(bridge) = args.first().filter(|a| !a.starts_with("--")) else {
        println!("{}", usage.yellow());
        return;
    };
    let rxq: u16 = flag_value(args, "--rxq")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
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
    body["accept_dhcp"] = serde_json::json!(has_flag(args, "--accept-dhcp"));
    let path = format!("/ovs/bridges/{}/uplinks", bridge);
    let res = match client
        .request_json::<serde_json::Value>(Method::POST, &path, Some(body))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            if e.contains("DHCP-managed") {
                println!(
                    "{} {}\n  Give the NIC a static address first, or re-run with --accept-dhcp to move it anyway.",
                    "Error:".red(),
                    e
                );
            } else if e.contains("host_interface_in_use") {
                println!(
                    "{} {}\n  The NIC is in use by the host. Re-run with --migrate-ip --force to move its IP configuration onto the bridge (rolled back automatically if the gateway stops answering or this CLI can't confirm).",
                    "Error:".red(),
                    e
                );
            } else {
                println!("{} {}", "Error:".red(), e);
            }
            return;
        }
    };
    finish_uplink(client, bridge, res).await;
}

/// After an uplink was added or re-applied: commit a moved IP once the API
/// still answers (netd rolls it back otherwise).
async fn finish_uplink(client: &CliClient, bridge: &str, res: serde_json::Value) {
    let name = res["record"]["spec"]["name"]
        .as_str()
        .unwrap_or("?")
        .to_string();
    for w in res["live"]["warnings"].as_array().into_iter().flatten() {
        println!("{} {}", "Warning:".yellow(), w.as_str().unwrap_or(""));
    }
    if res["phase"] == "pending_commit" {
        println!(
            "IP configuration moved to {}; checking the API is still reachable...",
            bridge
        );
        let token = res["record"]["pending"]["token"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if client.health_check().await.is_err() {
            println!(
                "{} API unreachable; glidex-netd will roll the change back automatically.",
                "Error:".red()
            );
            return;
        }
        let commit = format!("/ovs/bridges/{}/uplinks/{}/commit", bridge, name);
        match client
            .request_json::<serde_json::Value>(
                Method::POST,
                &commit,
                Some(serde_json::json!({"token": token})),
            )
            .await
        {
            Ok(_) => println!(
                "{} uplink {} on {} (IP migrated)",
                "Committed:".green(),
                name,
                bridge
            ),
            Err(e) => println!("{} {} (the change will be rolled back)", "Error:".red(), e),
        }
    } else {
        println!("{} uplink {} on {}", "Added:".green(), name, bridge);
    }
}

async fn handle_ovs(client: &CliClient, args: &[&str]) {
    match args.first().copied() {
        Some("status") | None => {
            match client
                .request_json::<serde_json::Value>(Method::GET, "/ovs/status", None)
                .await
            {
                Ok(s) => {
                    let netd = &s["netd"];
                    if netd["available"] != true {
                        println!(
                            "{} glidex-netd unavailable: {}",
                            "netd:".bold(),
                            netd["error"].as_str().unwrap_or("unknown").red()
                        );
                        return;
                    }
                    println!(
                        "{} available (access: {})",
                        "netd:".bold(),
                        netd["access"].as_str().unwrap_or("?")
                    );
                    let h = &s["host"];
                    let ovs = if h["ovs_installed"] == true {
                        format!(
                            "{} ({})",
                            h["ovs_version"].as_str().unwrap_or("?"),
                            if h["ovs_running"] == true {
                                "running".green()
                            } else {
                                "not running".red()
                            }
                        )
                    } else {
                        "not installed".red().to_string()
                    };
                    println!("{} {}", "Open vSwitch:".bold(), ovs);
                    println!(
                        "{} {}",
                        "dnsmasq:".bold(),
                        if h["dnsmasq"] == true {
                            "yes".green()
                        } else {
                            "missing".red()
                        }
                    );
                    println!("{} {}", "DPDK initialized:".bold(), h["dpdk_initialized"]);
                    if let Some(combos) = h["combinations"].as_array() {
                        println!("{}", "Combinations:".bold());
                        for c in combos {
                            let missing: Vec<&str> = c["missing"]
                                .as_array()
                                .map(|m| m.iter().filter_map(|x| x.as_str()).collect())
                                .unwrap_or_default();
                            println!(
                                "  {} {} {}",
                                c["id"].as_str().unwrap_or("?"),
                                if c["available"] == true {
                                    "available".green().to_string()
                                } else {
                                    format!("missing: {}", missing.join(", "))
                                        .yellow()
                                        .to_string()
                                },
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
            println!(
                "Installing Open vSwitch ({} profile); this can take a few minutes...",
                profile
            );
            match client
                .request_json::<serde_json::Value>(Method::POST, "/ovs/install", Some(body))
                .await
            {
                Ok(r) if r["changed"] == true => println!(
                    "{} Open vSwitch {}",
                    "Installed:".green(),
                    r["ovs_version"].as_str().unwrap_or("?")
                ),
                Ok(r) => println!(
                    "{} Open vSwitch {} already satisfies the profile",
                    "Nothing to do:".green(),
                    r["ovs_version"].as_str().unwrap_or("?")
                ),
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
            match client
                .request_json::<()>(Method::POST, "/ovs/dpdk-init", Some(body))
                .await
            {
                Ok(()) => println!("{} DPDK initialized", "OK:".green()),
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }
        Some(other) => println!(
            "{} unknown 'ovs {}'; use 'ovs status', 'ovs install' or 'ovs dpdk-init'",
            "Error:".red(),
            other
        ),
    }
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
    async fn health_check(&self) -> Result<(), String> {
        self.request_bytes(Method::GET, "/health", None)
            .await
            .map(|_| ())
            .map_err(|e| e.message)
    }

    async fn list_pci_devices(&self) -> Result<Vec<PciDeviceInfo>, String> {
        self.request_json(Method::GET, "/pci-devices", None).await
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
    if u == 0 {
        format!("{} B", n)
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

/// A condition's `status`: `CONDITION_STATUS_TRUE` in the proto shape,
/// `True` in the older REST bodies.
fn condition_true(status: &serde_json::Value) -> bool {
    status == "CONDITION_STATUS_TRUE" || status == "True"
}

/// One line for an object from `/watch`: its name and what it is doing.
fn watch_summary(kind: &str, o: &serde_json::Value) -> (String, String) {
    let name = o["name"].as_str().unwrap_or("?").to_string();
    let deleting = o["deleting"].as_bool().unwrap_or(false) || o["deletion_requested_at"].is_u64();
    let mut what = match kind {
        // The proto shape (spec/grpc.md §9.1): `VM_STATE_*`, `POWER_STATE_*`.
        "vm" => {
            let word = |v: &serde_json::Value, prefix: &str| {
                v.as_str()
                    .map(|s| s.trim_start_matches(prefix).to_ascii_lowercase())
            };
            let state = word(&o["state"], "VM_STATE_").unwrap_or_else(|| "?".to_string());
            let desired = word(&o["desired_state"], "POWER_STATE_").unwrap_or_default();
            vm::plain_state(&state, &desired)
        }
        "disk" => o["status"].as_str().unwrap_or("?").to_string(),
        // The proto shape (spec/grpc.md §9.1): `IMAGE_STATE_*`, 64-bit
        // numbers as strings.
        "image" => {
            let n = |v: &serde_json::Value| {
                v.as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .or(v.as_u64())
            };
            match o["status"]["state"]
                .as_str()
                .unwrap_or("?")
                .trim_start_matches("IMAGE_STATE_")
            {
                "DOWNLOADING" => match (
                    n(&o["status"]["received_bytes"]),
                    n(&o["status"]["total_bytes"]),
                ) {
                    (Some(r), Some(t)) if t > 0 => format!("downloading {}%", r * 100 / t),
                    _ => "downloading".to_string(),
                },
                s => s.to_ascii_lowercase(),
            }
        }
        _ => o["phase"].as_str().unwrap_or("ready").replace('_', " "),
    };
    if deleting {
        what = format!("deleting ({})", what);
    }
    if let Some(c) = o["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["kind"] == "Ready" && !condition_true(&c["status"]))
    {
        what = format!(
            "{} — {}: {}",
            what,
            c["reason"].as_str().unwrap_or(""),
            c["message"].as_str().unwrap_or("")
        );
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
    let mut last: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
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
                        let (kind, id) = (
                            v["kind"].as_str().unwrap_or("").to_string(),
                            v["id"].as_str().unwrap_or("").to_string(),
                        );
                        match event.as_str() {
                            "synced" => {
                                if !synced_once {
                                    println!(
                                        "{} {} objects; changes follow",
                                        "Synced:".green(),
                                        last.len()
                                    );
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
                                    let name =
                                        prev.split_whitespace().nth(1).unwrap_or("?").to_string();
                                    println!(
                                        "{}  {:<8} {:<24} {}",
                                        chrono_now(),
                                        kind,
                                        name,
                                        "deleted".red()
                                    );
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
    println!(
        "{} {} (the control plane finishes it)",
        "Deleting:".yellow(),
        name
    );
    if let Some(ready) = v["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["kind"] == "Ready" && !condition_true(&c["status"]))
    {
        println!(
            "  {} {}: {}",
            "Waiting:".yellow(),
            ready["reason"].as_str().unwrap_or(""),
            ready["message"].as_str().unwrap_or("")
        );
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
    println!(
        "  {}  - Start a VM (waits until it runs; --no-wait returns at once)",
        "start <name|id>".cyan()
    );
    println!("  {}   - Stop a VM", "stop <name|id>".cyan());
    println!(
        "  {} - Shut a VM down via its power button (default wait 60 s), then stop it",
        "stop <name|id> --graceful [secs]".cyan()
    );
    println!("  {}  - Pause a VM", "pause <name|id>".cyan());
    println!(
        "  {} - Connect to VM console (interactive)",
        "connect <name|id>".cyan()
    );
    println!(
        "  {}     - Show VM serial console log",
        "log <name|id>".cyan()
    );
    println!(
        "  {}  - What happened to a VM (starts, exits, restarts, adoptions)",
        "events <name|id>".cyan()
    );
    println!(
        "  {}  - Follow VMs, disks, images and networks as they change (Ctrl-C stops)",
        "watch [vms,disks,images,networks]".cyan()
    );
    println!(
        "  {} - Delete a VM (and its own root disk)",
        "delete <name|id> [--keep-disk]".cyan()
    );
    println!(
        "  {} <init|join|join-token|status|promote|snapshot> - Form or inspect the cluster",
        "cluster".cyan()
    );
    println!(
        "  {}             - List the cluster's nodes (a standalone host has one)",
        "nodes".cyan()
    );
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
    println!(
        "  {}       - List stored guest login credentials",
        "credentials".cyan()
    );
    println!(
        "  {}    - Add a credential (interactive)",
        "credential-add".cyan()
    );
    println!(
        "  {} - Change a credential's password",
        "credential-passwd <user>".cyan()
    );
    println!(
        "  {} - Replace a credential's SSH keys",
        "credential-keys <user>".cyan()
    );
    println!("  {}  - Delete a credential", "credential-rm <user>".cyan());
    println!("  {}          - List networks", "networks".cyan());
    println!(
        "  {} - Create a network (default: NAT)",
        "network-add <name> [--nat|--isolated|--bridged <br>]".cyan()
    );
    println!("  {}  - Delete a network", "network-rm <name>".cyan());
    println!("  {}           - List glidex OVS bridges", "bridges".cyan());
    println!("  {}  - List a bridge's uplinks", "uplinks <bridge>".cyan());
    println!(
        "  {} - Add an uplink (kernel/AF_XDP/DPDK)",
        "uplink-add <bridge> --kernel <if> [--migrate-ip] [--force] [--accept-dhcp]".cyan()
    );
    println!(
        "  {} - Set up again an uplink rolled back at start",
        "uplink-reapply <bridge> <name>".cyan()
    );
    println!(
        "  {} - Remove an uplink, restoring the NIC",
        "uplink-rm <bridge> <name>".cyan()
    );
    println!("  {}        - Host networking status", "ovs status".cyan());
    println!(
        "  {} - Install Open vSwitch",
        "ovs install [--profile kernel|dpdk] [--force]".cyan()
    );
    println!(
        "  {}     - Cloud images in the built-in catalog",
        "image catalog".cyan()
    );
    println!("  {}        - List downloaded images", "image list".cyan());
    println!(
        "  {} - Download and verify an image",
        "image pull <catalog-key|https-url> [--name N] [--sha256 H]".cyan()
    );
    println!(
        "  {} - Pull UEFI firmware VMs boot through",
        "image pull --firmware <key> [--name N]".cyan()
    );
    println!(
        "  {} - Download firmware (QEMU: with its variable store)",
        "image pull <url> --firmware-for <hv> [--vars-url U] [--vars-sha256 H]".cyan()
    );
    println!("  {}   - Delete an image", "image rm <name|id>".cyan());
    println!("  {}         - List disks", "disk list".cyan());
    println!(
        "  {}  - Disk details and partitions",
        "disk show <name|id>".cyan()
    );
    println!(
        "  {} - Create a disk",
        "disk create <name> [--size-gib N] [--image I] [--full] [--raw] [--no-extend]".cyan()
    );
    println!(
        "  {} - Grow or shrink a disk",
        "disk resize <disk> <GiB> [--no-extend]".cyan()
    );
    println!(
        "  {} - Grow the root partition",
        "disk extend-root <disk> [--on-boot]".cyan()
    );
    println!("  {}    - Delete a disk", "disk rm <name|id>".cyan());
    println!("  {}            - Check API server health", "health".cyan());
    println!();
    println!("{}", "Access control:".bold());
    println!(
        "  {}            - Who you are: method, teams, roles",
        "whoami".cyan()
    );
    println!(
        "  {} - Log in over TCP (OIDC device flow or a pasted token)",
        "login --oidc [--connector ID] | --token".cyan()
    );
    println!("  {} - Forget the saved token", "logout [--revoke]".cyan());
    println!(
        "  {} - Login profiles for remote control planes: target, trust and token of a server, under a name",
        "auth login | logout [--revoke] | status | use <P> | profiles | trust list|fetch|remove"
            .cyan()
    );
    println!(
        "  {}                - Print the web UI address",
        "ui".cyan()
    );
    println!(
        "  {} - Access tokens",
        "token list | create <name> [--days N] [--service-account] [--role R[@P]] | revoke <id>"
            .cyan()
    );
    println!("  {} - Tenants", "tenant list | show <t> | create <slug> [--operator] | suspend|resume|delete <t> | members|add-member|remove-member …".cyan());
    println!(
        "  {} - Tenants admitted on this cluster, and their caps",
        "admission list | set <t> [vms=N … | unlimited] [--suspend|--resume] | remove <t>".cyan()
    );
    println!("  {} - Login connectors of this directory", "connector list | show <id> | add oidc|pam <id> … | update <id> … | secret set <id> | test <id> | remove <id>".cyan());
    println!(
        "  {} - Projects",
        "project list | show <p> | create <name> [--description D] | delete <p>".cyan()
    );
    println!(
        "  {} - Quotas (none = unlimited)",
        "project quota <p> vms=10 memory_mib=none ...".cyan()
    );
    println!("  {} - Set your default project", "project use <p>".cyan());
    println!(
        "  {} - Project role bindings",
        "binding list <p> | add <p> <role> user:<id> | remove <p> <id>".cyan()
    );
    println!(
        "  {} - Host role bindings",
        "system-binding list | add <role> <principal> | remove <id>".cyan()
    );
    println!(
        "  {}         - Users and their identities",
        "user list".cyan()
    );
    println!(
        "  {} - Teams",
        "team list | create <name> | add-member <team> <user> | remove-member <team> <user>".cyan()
    );
    println!("  {} - Site Cedar policies", "policy list | show <id> | put <id> <file> [--disable] | delete <id> | validate <id> <file> | history <id>".cyan());
    println!(
        "  {} - Audit log",
        "audit [--project P] [--since <unix-ms>] [--limit N] [--user U]".cyan()
    );
    println!(
        "  {} - Resource usage (default: this billing month by project)",
        "usage [--project P] [--from D] [--to D] [--by project,vm,disk,nic,network] [--granularity hour|day|month] [--meters m,…] [--tz Z] [--csv]".cyan()
    );
    println!(
        "  {} - Average, 30-second peak and 95th percentile per billing month",
        "usage bandwidth|disk-io|compute [--month YYYY-MM] [--by project,vm,nic,network|disk] [--csv]".cyan()
    );
    println!(
        "  {} - A VM's current CPU, memory, NIC and disk rates",
        "stats <vm>".cyan()
    );
    println!(
        "  {} - 5-minute series with the 95th percentile",
        "bandwidth <vm> | --network N · io <vm> | --disk D · compute <vm>".cyan()
    );
    println!("  {} - Project networks", "network create <name> --project P [--isolated] [--vhost-user] | share <net> <project-id> | unshare <net> <project-id>".cyan());
    println!(
        "  {} - Shares offered to a project",
        "network shares <p> | accept <p> <net> | leave <p> <net>".cyan()
    );
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
        let contents = std::fs::read_to_string(&path).map_err(|e| format!("{}: {}", path, e))?;
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
    let w: serde_json::Value = client
        .request_json(Method::GET, "/auth/whoami", None)
        .await
        .ok()?;
    w["user"]["display_name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .map(str::to_string)
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
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<String> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "pub"))
        .filter(|p| read_public_key_files(&p.to_string_lossy()).is_ok_and(|k| !k.is_empty()))
        .map(|p| {
            format!(
                "~/.ssh/{}",
                p.file_name().unwrap_or_default().to_string_lossy()
            )
        })
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

/// REPL command names offered by Tab (aliases included).
const COMMANDS: &[&str] = &[
    "help",
    "exit",
    "quit",
    "list",
    "ls",
    "get",
    "create",
    "start",
    "stop",
    "pause",
    "events",
    "watch",
    "connect",
    "console",
    "attach",
    "log",
    "logs",
    "delete",
    "rm",
    "pci",
    "pci-devices",
    "nodes",
    "node",
    "cluster",
    "attach-device",
    "detach-device",
    "credentials",
    "creds",
    "credential-add",
    "credential-passwd",
    "credential-keys",
    "credential-rm",
    "networks",
    "network-add",
    "network-rm",
    "bridges",
    "uplinks",
    "uplink-add",
    "uplink-reapply",
    "uplink-rm",
    "ovs",
    "health",
    "image",
    "images",
    "disk",
    "disks",
    "whoami",
    "login",
    "logout",
    "ui",
    "token",
    "tokens",
    "project",
    "projects",
    "binding",
    "bindings",
    "system-binding",
    "user",
    "users",
    "team",
    "teams",
    "policy",
    "policies",
    "audit",
    "usage",
    "stats",
    "bandwidth",
    "io",
    "compute",
    "network",
    "auth",
    "tenant",
    "tenants",
    "admission",
    "admissions",
    "connector",
    "connectors",
];

/// Subcommands offered by Tab after these commands.
const SUBCOMMANDS: &[(&str, &[&str])] = &[
    ("image", &["catalog", "list", "pull", "rm"]),
    (
        "tenant",
        &[
            "list",
            "show",
            "create",
            "rename",
            "suspend",
            "resume",
            "delete",
            "members",
            "add-member",
            "remove-member",
            "bindings",
            "bind",
            "unbind",
        ],
    ),
    ("admission", &["list", "set", "remove"]),
    (
        "connector",
        &["list", "show", "add", "update", "secret", "test", "remove"],
    ),
    (
        "disk",
        &["list", "show", "create", "resize", "extend-root", "rm"],
    ),
    ("ovs", &["status", "install", "dpdk-init"]),
    (
        "cluster",
        &[
            "init",
            "join",
            "join-token",
            "status",
            "promote",
            "snapshot",
            "rotate-ca",
            "rotate-secrets-key",
            "rejoin",
            "leave",
            "dissolve",
        ],
    ),
    (
        "node",
        &[
            "drain",
            "undrain",
            "remove",
            "forget",
            "purge",
            "rejoin-token",
            "detach",
            "import",
        ],
    ),
    ("login", &["--oidc", "--token"]),
    ("logout", &["--revoke"]),
    (
        "auth",
        &[
            "login", "logout", "status", "use", "profiles", "trust", "token", "whoami",
        ],
    ),
    ("token", &["list", "create", "revoke"]),
    (
        "project",
        &["list", "show", "create", "delete", "quota", "use"],
    ),
    ("binding", &["list", "add", "remove"]),
    ("system-binding", &["list", "add", "remove"]),
    ("user", &["list"]),
    (
        "team",
        &["list", "create", "delete", "add-member", "remove-member"],
    ),
    (
        "policy",
        &[
            "list", "show", "put", "delete", "validate", "history", "reload",
        ],
    ),
    (
        "network",
        &[
            "list", "create", "rm", "share", "unshare", "shares", "accept", "leave",
        ],
    ),
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
    SUBCOMMANDS
        .iter()
        .find(|(c, _)| *c == cmd)
        .map(|(_, s)| *s)
        .unwrap_or(&[])
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
        let prefix = if starting_new_word {
            ""
        } else {
            words[words.len() - 1]
        };
        let subs = subcommands(words[0]);
        if !subs.is_empty() {
            let candidates = subs
                .iter()
                .filter(|c| c.starts_with(prefix))
                .map(|c| Pair {
                    display: c.to_string(),
                    replacement: format!("{} ", c),
                })
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
    let segment_start = segment_start
        + (before[segment_start..].len() - before[segment_start..].trim_start().len());
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
    if input.is_empty() { None } else { Some(input) }
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
    if input.is_empty() { None } else { Some(input) }
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

        "list" | "ls" => vm::list(client).await,
        "get" => vm::get(client, &parts[1..]).await,
        "create" => vm::create(client).await,
        "start" => vm::start(client, &parts[1..]).await,
        "stop" => vm::stop(client, &parts[1..]).await,
        "pause" => vm::pause(client, &parts[1..]).await,

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

        "events" => vm::events(client, &parts[1..]).await,

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

        "delete" | "rm" => vm::delete(client, &parts[1..]).await,

        "nodes" | "node" if matches!(parts.get(1), Some(&"drain") | Some(&"undrain")) => {
            let Some(id) = parts.get(2) else {
                println!("{}", format!("Usage: node {} <node>", parts[1]).yellow());
                return true;
            };
            // Accept a node's name as well as its id.
            let nodes: serde_json::Value = client
                .request_json(Method::GET, "/nodes", None)
                .await
                .unwrap_or_default();
            let found = nodes
                .as_array()
                .into_iter()
                .flatten()
                .find(|n| n["meta"]["id"] == *id || n["spec"]["name"] == *id)
                .and_then(|n| n["meta"]["id"].as_str().map(String::from));
            let Some(nid) = found else {
                println!("{} no node {}", "Error:".red(), id);
                return true;
            };
            match client
                .request_json::<serde_json::Value>(
                    Method::POST,
                    &format!("/nodes/{}/{}", nid, parts[1]),
                    None,
                )
                .await
            {
                Ok(v) => {
                    println!(
                        "{} {} is now {}",
                        "OK".green(),
                        v["node"].as_str().unwrap_or(""),
                        v["phase"].as_str().unwrap_or("")
                    );
                    let r = &v["remaining"];
                    println!(
                        "  still on it: {} VMs, {} disks, {} networks",
                        r["vms"].as_array().map_or(0, |a| a.len()),
                        r["disks"].as_array().map_or(0, |a| a.len()),
                        r["networks"].as_array().map_or(0, |a| a.len())
                    );
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }
        "nodes" | "node" if parts.get(1) == Some(&"import") => {
            let rest = &parts[2..];
            let sub = rest.first().copied().unwrap_or("list");
            let plan = rest.get(1).copied().filter(|a| !a.starts_with("--"));
            let map_of = |flag: &str| {
                let mut m = serde_json::Map::new();
                for v in flag_values(rest, flag) {
                    if let Some((a, b)) = v.split_once('=') {
                        m.insert(a.to_string(), b.into());
                    }
                }
                m
            };
            let result = match (sub, plan) {
                ("list", _) => {
                    client
                        .request_json::<serde_json::Value>(Method::GET, "/imports", None)
                        .await
                }
                ("show", Some(p)) => {
                    client
                        .request_json::<serde_json::Value>(
                            Method::GET,
                            &format!("/imports/{p}"),
                            None,
                        )
                        .await
                }
                ("approve", Some(p)) => {
                    let body = serde_json::json!({ "projects": map_of("--project"), "networks": map_of("--network"), "credentials": map_of("--credential"), "over_quota": has_flag(rest, "--over-quota"), "dry_run": has_flag(rest, "--dry-run") });
                    client
                        .request_json::<serde_json::Value>(
                            Method::POST,
                            &format!("/imports/{p}/approve"),
                            Some(body),
                        )
                        .await
                }
                ("reject", Some(p)) => client
                    .request_json::<serde_json::Value>(
                        Method::POST,
                        &format!("/imports/{p}/reject"),
                        Some(serde_json::json!({})),
                    )
                    .await
                    .or_else(|e| {
                        if e.to_string().is_empty() {
                            Ok(serde_json::Value::Null)
                        } else {
                            Err(e)
                        }
                    }),
                _ => {
                    println!("{}", "Usage: node import list | show <plan> | approve <plan> [--project src=existing|src=new:name] [--network a=b] [--credential proj/user=new] [--over-quota] [--dry-run] | reject <plan>".yellow());
                    return true;
                }
            };
            match result {
                Ok(v) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }
        "nodes" | "node"
            if matches!(
                parts.get(1),
                Some(&"remove")
                    | Some(&"forget")
                    | Some(&"purge")
                    | Some(&"rejoin-token")
                    | Some(&"detach")
            ) =>
        {
            let Some(id) = parts.get(2).filter(|a| !a.starts_with("--")) else {
                println!(
                    "{}",
                    format!(
                        "Usage: node {} <node> [--force] [--fenced] [--raft-lost]",
                        parts[1]
                    )
                    .yellow()
                );
                return true;
            };
            let rest = &parts[3..];
            let mut maps = serde_json::Map::new();
            for m in flag_values(rest, "--map-network") {
                if let Some((a, b)) = m.split_once('=') {
                    maps.insert(a.to_string(), b.into());
                }
            }
            let body = serde_json::json!({
                "map_networks": maps,
                "with_access": has_flag(rest, "--with-access"),
                "abort": has_flag(rest, "--abort"),
                "departed": has_flag(rest, "--departed"),
                "timeout_secs": flag_value(rest, "--timeout").and_then(|t| t.parse::<u64>().ok()),
                "force": has_flag(rest, "--force"),
                "fenced": has_flag(rest, "--fenced"),
                "raft_intact": !has_flag(rest, "--raft-lost"),
            });
            // The node's name goes in the path as it is: the server resolves names too.
            match client
                .request_json::<serde_json::Value>(
                    Method::POST,
                    &format!("/nodes/{}/{}", id, parts[1]),
                    Some(body),
                )
                .await
            {
                Ok(v) if parts[1] == "rejoin-token" => {
                    println!("{}", v["token"].as_str().unwrap_or(""));
                    eprintln!(
                        "Shown once, for node {} ({}). On the host, as root: glidex-install --rejoin <server:8842>, token on stdin.{}",
                        v["node"].as_str().unwrap_or(""),
                        v["node_id"].as_str().unwrap_or(""),
                        if v["raft_intact"] == false {
                            " Its Raft state will be started afresh."
                        } else {
                            ""
                        }
                    );
                }
                Ok(v) => println!(
                    "{} {}",
                    "OK".green(),
                    serde_json::to_string(&v).unwrap_or_default()
                ),
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }
        "nodes" | "node" => {
            let path = match parts.get(1) {
                Some(id) => format!("/nodes/{}", id),
                None => "/nodes".to_string(),
            };
            match client
                .request_json::<serde_json::Value>(Method::GET, &path, None)
                .await
            {
                Ok(v) if v.is_array() => {
                    let rows: Vec<NodeRow> = v
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(NodeRow::from_json)
                        .collect();
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
                        None => {
                            println!("{} no PCI device with address {}", "Error:".red(), parts[1])
                        }
                    }
                } else {
                    let table = Table::new(&devices).to_string();
                    println!("{}", table);
                }
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },

        "attach-device" => vm::device(client, true, &parts[1..]).await,
        "detach-device" => vm::device(client, false, &parts[1..]).await,

        "credentials" | "creds" => storage::credentials(client).await,

        "credential-add" | "cred-add" => storage::credential_add(client).await,

        "credential-passwd" | "cred-passwd" => {
            storage::credential_passwd(client, parts.get(1).copied()).await
        }

        "credential-keys" | "cred-keys" => {
            storage::credential_keys(client, parts.get(1).copied()).await
        }

        "credential-rm" | "cred-rm" => storage::credential_rm(client, parts.get(1).copied()).await,

        "networks" | "nets" => list_networks(client).await,

        "network" | "net" => admin::network(client, &parts[1..]).await,
        "whoami" => admin::whoami(client).await,
        "cluster" => admin::cluster(client, &parts[1..]).await,
        "auth" => admin::auth(client, &parts[1..]).await,
        "login" => admin::login(client, &parts[1..]).await,
        "logout" => admin::logout(client, &parts[1..]).await,
        "ui" => admin::ui(),
        "token" | "tokens" => admin::token(client, &parts[1..]).await,
        "tenant" | "tenants" => tenancy::tenant(client, &parts[1..]).await,
        "admission" | "admissions" => tenancy::admission(client, &parts[1..]).await,
        "connector" | "connectors" => directory::connector(client, &parts[1..]).await,
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
                .request_json::<serde_json::Value>(
                    Method::DELETE,
                    &format!("/networks/{}?wait=60", name),
                    None,
                )
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
                        if b["live"].is_null() {
                            "MISSING on host".red().to_string()
                        } else {
                            format!(
                                "ports: {}",
                                b["live"]["ports"].as_array().map(|p| p.len()).unwrap_or(0)
                            )
                        }
                    );
                }
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },

        "ovs" => handle_ovs(client, &parts[1..]).await,

        "uplinks" => match parts.get(1) {
            Some(bridge) => match client
                .request_json::<Vec<serde_json::Value>>(
                    Method::GET,
                    &format!("/ovs/bridges/{}/uplinks", bridge),
                    None,
                )
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
                            u["live"]["error"]
                                .as_str()
                                .map(|e| e.red().to_string())
                                .unwrap_or_else(|| u["live"]["link_state"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string())
                        );
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            None => println!("{}", "Usage: uplinks <bridge>".yellow()),
        },

        "uplink-add" => handle_uplink_add(client, &parts[1..]).await,

        // A migration netd rolled back at start (spec/clustering-ui.md):
        // set it up again with the gateway check and commit.
        "uplink-reapply" => match (parts.get(1), parts.get(2)) {
            (Some(bridge), Some(name)) => {
                match client
                    .request_json::<serde_json::Value>(
                        Method::POST,
                        &format!("/ovs/bridges/{}/uplinks/{}/reapply", bridge, name),
                        Some(serde_json::json!({})),
                    )
                    .await
                {
                    Ok(res) => finish_uplink(client, bridge, res).await,
                    Err(e) => println!("{} {}", "Error:".red(), e),
                }
            }
            _ => println!("{}", "Usage: uplink-reapply <bridge> <name>".yellow()),
        },

        "uplink-rm" => match (parts.get(1), parts.get(2)) {
            (Some(bridge), Some(name)) => match client
                .request_json::<()>(
                    Method::DELETE,
                    &format!("/ovs/bridges/{}/uplinks/{}", bridge, name),
                    None,
                )
                .await
            {
                Ok(()) => println!("{} uplink {} (NIC restored)", "Removed:".green(), name),
                Err(e) => println!("{} {}", "Error:".red(), e),
            },
            _ => println!("{}", "Usage: uplink-rm <bridge> <name>".yellow()),
        },

        "image" | "images" => storage::image(client, &parts[1..]).await,

        "disk" | "disks" => storage::disk(client, &parts[1..]).await,

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
    // A profile-bound run checks itself against the cluster it claims
    // before any command runs (§4.1): the binding mismatch is a wall
    // between the operator and a repurposed or hostile machine, so not
    // even a one-shot command slips past it.
    if let Err(e) = client.preflight().await {
        eprintln!("{} {}", "Error:".red(), e);
        std::process::exit(2);
    }

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
    if let Some(b) = client.profile_banner() {
        // Which cluster `delete vm` points at, at a glance (§6.2).
        println!("{} {}", "Profile:".dimmed(), b.cyan());
    }
    if client.is_unix() {
        println!(
            "{} identified by your Unix user (see 'whoami')",
            "Auth:".dimmed()
        );
    } else if !client.has_token() {
        if client.profile_banner().is_some() {
            println!(
                "{} no token in this profile; run 'auth login'",
                "Auth:".dimmed()
            );
        } else {
            println!(
                "{} no token; run 'login --oidc' or 'login --token' (or set GLIDEX_TOKEN)",
                "Auth:".dimmed()
            );
        }
    }
    if let Some(p) = client.project() {
        println!("Project: {}", p.cyan());
    }
    println!("Type {} for available commands\n", "help".cyan());

    let mut rl = new_editor(CompletionMode::Command).expect("Failed to initialize readline");
    println!(
        "{} Tab completes commands and file paths.\n",
        "Tip:".dimmed()
    );

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
        assert_eq!(
            credential_choice("", Some("alice")).as_deref(),
            Some("alice")
        );
        assert_eq!(credential_choice("none", Some("alice")), None);
        assert_eq!(
            credential_choice("bob", Some("alice")).as_deref(),
            Some("bob")
        );
        assert_eq!(credential_choice("", None), None);
    }

    #[test]
    fn credential_key_file_answers() {
        let d = vec![
            "~/.ssh/id_ed25519.pub".to_string(),
            "~/.ssh/id_rsa.pub".to_string(),
        ];
        assert_eq!(
            chosen_key_files("", &d).as_deref(),
            Some("~/.ssh/id_ed25519.pub,~/.ssh/id_rsa.pub")
        );
        assert_eq!(chosen_key_files("none", &d), None);
        assert_eq!(chosen_key_files("NONE", &d), None);
        assert_eq!(
            chosen_key_files("/tmp/k.pub", &d).as_deref(),
            Some("/tmp/k.pub")
        );
        assert_eq!(chosen_key_files("", &[]), None);
    }

    #[test]
    fn own_public_keys_skip_private_and_junk() {
        let dir = tempfile::TempDir::new().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(
            ssh.join("id_ed25519.pub"),
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA me@host\n",
        )
        .unwrap();
        std::fs::write(
            ssh.join("id_ed25519"),
            "-----BEGIN OPENSSH PRIVATE KEY-----\n",
        )
        .unwrap();
        std::fs::write(ssh.join("bad.pub"), "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();
        std::fs::write(ssh.join("empty.pub"), "\n").unwrap();
        let files = public_key_files_in(dir.path());
        assert_eq!(files, vec!["~/.ssh/id_ed25519.pub"]);
    }

    #[test]
    fn watch_summary_reads_proto_vms() {
        let vm = serde_json::json!({
            "name": "web",
            "state": "VM_STATE_STOPPED",
            "desired_state": "POWER_STATE_RUNNING",
            "generation": "3",
            "conditions": [{
                "kind": "Ready",
                "status": "CONDITION_STATUS_FALSE",
                "reason": "Starting",
                "message": "launching"
            }]
        });
        assert_eq!(
            watch_summary("vm", &vm),
            (
                "web".to_string(),
                "stopped → running — Starting: launching".to_string()
            )
        );
        let vm = serde_json::json!({
            "name": "db",
            "state": "VM_STATE_CREATED",
            "desired_state": "POWER_STATE_STOPPED",
            "deleting": true,
            "conditions": [{"kind": "Ready", "status": "CONDITION_STATUS_TRUE"}]
        });
        assert_eq!(watch_summary("vm", &vm).1, "deleting (created)");
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
            [
                "cred-add",
                "credential-add",
                "credential-keys",
                "credential-passwd",
                "credential-rm",
                "credentials",
                "creds"
            ]
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
        assert!(
            replacements(complete_command_line(&files, &line, line.len()))
                .1
                .is_empty()
        );
        let line = format!("start {base}/CLO");
        assert!(
            replacements(complete_command_line(&files, &line, line.len()))
                .1
                .is_empty()
        );
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
        std::fs::write(
            &public,
            "# comment\nssh-ed25519 AAAA a@b\n\nssh-rsa BBBB c@d\n",
        )
        .unwrap();
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
            vec![
                "project",
                "create",
                "lab",
                "--description",
                "Lab VMs",
                "a b",
                "c d"
            ]
        );
        assert_eq!(split_line(r#"x """#).unwrap(), vec!["x", ""]);
        assert!(split_line("say 'oops").is_err());
        assert_eq!(
            flag_values(&["--role", "a", "x", "--role", "b@p"], "--role"),
            vec!["a", "b@p"]
        );
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
        assert!(
            replacements(complete_command_line(&files, "token create ", 13))
                .1
                .is_empty()
        );
        for (cmd, _) in SUBCOMMANDS {
            assert!(COMMANDS.contains(cmd), "{cmd} is missing from COMMANDS");
        }
    }

    /// A control plane (auth disabled) on a temporary api.sock.
    async fn serve_temp() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        tokio::sync::watch::Sender<bool>,
    ) {
        use glidex_control_plane::api::{Listener, create_router};
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
        assert_eq!(
            client::select_socket(None, std::slice::from_ref(&sock), client::is_socket),
            Some(sock.clone())
        );
        let client = ApiClient::unix(sock);

        client.health_check().await.unwrap();
        assert!(client.list_vms().await.unwrap().is_empty());

        let who: serde_json::Value = client
            .request_json(Method::GET, "/auth/whoami", None)
            .await
            .unwrap();
        assert_eq!(who["method"], "disabled");

        let c = storage::create_credential(
            &client,
            "alice".into(),
            None,
            vec!["ssh-ed25519 AAAAC3Nza alice@host".into()],
        )
        .await
        .unwrap();
        assert_eq!(c.username, "alice");
        assert_eq!(c.ssh_authorized_keys.len(), 1);
        assert_eq!(c.has_password, Some(false));

        // Scoped to a project by name: the request's `project`.
        let projects: Vec<serde_json::Value> = client
            .request_json(Method::GET, "/projects", None)
            .await
            .unwrap();
        let project = projects[0]["name"].as_str().unwrap().to_string();
        client.set_project(Some(project.clone()));
        let creds = storage::list_credentials(&client).await.unwrap();
        assert_eq!(
            creds
                .iter()
                .map(|c| c.username.as_str())
                .collect::<Vec<_>>(),
            ["alice"]
        );
        assert_eq!(creds[0].project, projects[0]["id"].as_str().unwrap());
        assert!(creds[0].updated_at.is_set());
        assert!(client.list_vms().await.unwrap().is_empty());

        // A project that doesn't exist is an API error, rendered.
        client.set_project(Some("no-such-project".into()));
        let e = storage::list_credentials(&client).await.unwrap_err();
        assert!(e.contains("not_found"), "{e}");
        client.set_project(None);

        // VMs on their RPC client.
        let e = vm::rpc(&client)
            .await
            .unwrap()
            .get_vm(glidex_api::proto::glidex::vm::v1::GetVmRequest {
                id: "nope".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, connectrpc::ErrorCode::NotFound);
        assert!(
            client
                .get_vm("nope")
                .await
                .unwrap_err()
                .contains("not_found")
        );
        assert!(
            client
                .resolve_vm("nope")
                .await
                .unwrap_err()
                .contains("not found")
        );

        let c = storage::update_credential(
            &client,
            "alice",
            None,
            Some(vec!["ssh-ed25519 BBBBC3Nza alice@new".into()]),
        )
        .await
        .unwrap();
        assert_eq!(c.ssh_authorized_keys, ["ssh-ed25519 BBBBC3Nza alice@new"]);
        // An empty key list clears the keys: refused here, as alice has
        // no password to fall back on.
        let e = storage::update_credential(&client, "alice", None, Some(vec![]))
            .await
            .unwrap_err();
        assert!(e.contains("invalid_credential"), "{e}");
        storage::delete_credential(&client, "alice").await.unwrap();
        assert!(storage::list_credentials(&client).await.unwrap().is_empty());

        // Images and disks on their RPC clients.
        {
            use glidex_api::proto::glidex::storage::v1::*;
            let t = client.rpc_transport().await.unwrap();
            assert!(storage::list_images(&client).await.unwrap().is_empty());
            let images = ImageServiceClient::new(t.clone(), t.config());
            let e = images
                .get_image(GetImageRequest {
                    id: "nope".into(),
                    ..Default::default()
                })
                .await
                .unwrap_err();
            assert_eq!(e.code, connectrpc::ErrorCode::NotFound);
            let disks = DiskServiceClient::new(t.clone(), t.config());
            let list = disks
                .list_disks(ListDisksRequest::default())
                .await
                .unwrap()
                .into_owned();
            assert!(list.disks.is_empty());
            let e = disks
                .get_disk(GetDiskRequest {
                    id: "nope".into(),
                    ..Default::default()
                })
                .await
                .unwrap_err();
            assert_eq!(e.code, connectrpc::ErrorCode::NotFound);
            assert!(tenancy::rpc_err(e).contains("not_found"));
        }

        // The console WebSocket goes over the same socket; errors come back
        // as API errors.
        let e = console::open_ws(&client, "/vms/nope/console/ws")
            .await
            .err()
            .unwrap();
        assert!(e.contains("not_found"), "{e}");
        let e = vm::rpc(&client)
            .await
            .unwrap()
            .get_console_log(glidex_api::proto::glidex::vm::v1::GetConsoleLogRequest {
                id: "nope".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, connectrpc::ErrorCode::NotFound);

        let _ = stop.send(true);
    }

    /// The generated connectrpc clients over gxctl's own connection
    /// (spec/grpc.md §10): the Connect protocol on api.sock.
    #[tokio::test]
    async fn rpc_clients_over_the_unix_socket() {
        use glidex_api::proto::glidex::tenancy::v1::*;
        let (_dir, sock, stop) = serve_temp().await;
        let client = ApiClient::unix(sock);
        let t = client.rpc_transport().await.unwrap();
        let tenants = TenantServiceClient::new(t.clone(), t.config());
        let created = tenants
            .create_tenant(CreateTenantRequest {
                slug: "acme".into(),
                display_name: "Acme".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_owned();
        let acme = created.tenant.as_option().unwrap().id.clone();
        // The same connection carries the next call.
        let list = tenants
            .list_tenants(ListTenantsRequest::default())
            .await
            .unwrap()
            .into_owned();
        assert_eq!(
            list.tenants
                .iter()
                .map(|t| t.slug.as_str())
                .collect::<Vec<_>>(),
            ["acme", "admin", "default"]
        );
        let admissions = AdmissionServiceClient::new(t.clone(), t.config());
        let a = admissions
            .set_admission(SetAdmissionRequest {
                tenant: "acme".into(),
                cap: Quotas {
                    vms: Some(3),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_owned();
        assert_eq!(
            a.admission
                .as_option()
                .unwrap()
                .cap
                .as_option()
                .unwrap()
                .vms,
            Some(3)
        );
        // Errors arrive as connectrpc errors with the server's code.
        let e = tenants
            .create_tenant(CreateTenantRequest {
                slug: "acme".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, connectrpc::ErrorCode::Aborted);
        let e = tenants
            .get_tenant(GetTenantRequest {
                id: "nope".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, connectrpc::ErrorCode::NotFound);
        let got = tenants
            .get_tenant(GetTenantRequest {
                id: acme,
                ..Default::default()
            })
            .await
            .unwrap()
            .into_owned();
        assert_eq!(got.tenant.as_option().unwrap().display_name, "Acme");
        let _ = stop.send(true);
    }

    /// `connector` drives ConnectorService the same way; a secret goes in
    /// and only its status comes back.
    #[tokio::test]
    async fn connector_client_over_the_unix_socket() {
        use glidex_api::proto::glidex::directory::v1::*;
        type Settings =
            glidex_api::proto::glidex::directory::v1::__buffa::oneof::connector::Settings;
        let (_dir, sock, stop) = serve_temp().await;
        let client = ApiClient::unix(sock);
        let t = client.rpc_transport().await.unwrap();
        let svc = ConnectorServiceClient::new(t.clone(), t.config());
        let oidc = OidcSettings {
            issuer: "https://idp.example".into(),
            client_id: "glidex".into(),
            ..Default::default()
        };
        let c = Connector {
            id: "corp".into(),
            display_name: "Corp".into(),
            settings: Some(Settings::Oidc(Box::new(oidc))),
            ..Default::default()
        };
        let made = svc
            .create_connector(CreateConnectorRequest {
                connector: c.into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_owned();
        assert_eq!(
            made.connector.as_option().unwrap().secrets[0].set,
            Some(false)
        );
        let req = SetSecretRequest {
            id: "corp".into(),
            field: "client_secret".into(),
            value: "s3cret-value".into(),
            ..Default::default()
        };
        let r = svc.set_secret(req).await.unwrap().into_owned();
        assert_eq!(r.connector.as_option().unwrap().secrets[0].set, Some(true));
        let got = svc
            .get_connector(GetConnectorRequest {
                id: "corp".into(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_owned();
        assert_eq!(
            got.connector.as_option().unwrap().secrets[0].set,
            Some(true)
        );
        let e = svc
            .get_connector(GetConnectorRequest {
                id: "nope".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, connectrpc::ErrorCode::NotFound);
        svc.delete_connector(DeleteConnectorRequest {
            id: "corp".into(),
            ..Default::default()
        })
        .await
        .unwrap();
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
