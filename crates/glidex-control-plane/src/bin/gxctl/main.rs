//! `gxctl`: interactive CLI for the glidex control plane (spec/cli.md).

// Doc comments on the clap types are `--help` text, printed verbatim:
// `<uid>` and bare URLs there are meant for the terminal, not rustdoc.
#![allow(rustdoc::invalid_html_tags, rustdoc::bare_urls)]

mod access;
mod admin;
mod client;
mod cluster;
mod config;
mod console;
mod directory;
mod dirlogin;
mod host;
mod network;
mod storage;
mod tenancy;
mod usage;
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
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;

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

use access::project_names;

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
    // A node (the proto Node) is named in its spec, an import plan
    // (the proto ImportPlan) by the joining host.
    let name = match kind {
        "node" => o["spec"]["name"].as_str().or(o["meta"]["name"].as_str()),
        "import" => o["node"].as_str(),
        _ => o["name"].as_str(),
    }
    .unwrap_or("?")
    .to_string();
    // `NODE_PHASE_ACTIVE` → `Active`, as `nodes` prints it.
    let title = |v: &serde_json::Value, prefix: &str| {
        let w = v
            .as_str()
            .unwrap_or("")
            .trim_start_matches(prefix)
            .to_ascii_lowercase();
        let mut c = w.chars();
        c.next()
            .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
            .unwrap_or_else(|| "?".to_string())
    };
    // `deletion_requested_at`: RFC 3339 in the proto shape (networks), unix
    // seconds in the older bodies.
    let deleting = o["deleting"].as_bool().unwrap_or(false)
        || o["deletion_requested_at"].is_u64()
        || o["deletion_requested_at"].is_string();
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
        // Nodes in the proto shape: `NODE_PHASE_*`, ready a `CONDITION_STATUS_*`.
        "node" => {
            let phase = title(&o["status"]["phase"], "NODE_PHASE_");
            if condition_true(&o["status"]["ready"]) {
                phase
            } else {
                format!("{}, not ready", phase)
            }
        }
        // Import plans: `IMPORT_STATE_*`.
        "import" => title(&o["state"], "IMPORT_STATE_").to_ascii_lowercase(),
        // Networks in the proto shape: `NETWORK_PHASE_*`.
        _ => o["phase"]
            .as_str()
            .unwrap_or("ready")
            .trim_start_matches("NETWORK_PHASE_")
            .to_ascii_lowercase()
            .replace('_', " "),
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
/// the caller can see until Ctrl-C, over `glidex.watch.v1.WatchService.Watch`.
/// Reconnects when the server ends a stream, printing only what changed
/// meanwhile.
async fn handle_watch(client: &ApiClient, args: &[&str]) {
    use glidex_api::proto::glidex::watch::v1 as w;
    let kinds = args.first().copied().unwrap_or("vms,disks,images,networks");
    let mut req = w::WatchRequest::default();
    for k in kinds.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        let kind = match k {
            "vm" | "vms" => w::WatchKind::WATCH_KIND_VM,
            "disk" | "disks" => w::WatchKind::WATCH_KIND_DISK,
            "image" | "images" => w::WatchKind::WATCH_KIND_IMAGE,
            "network" | "networks" => w::WatchKind::WATCH_KIND_NETWORK,
            "node" | "nodes" => w::WatchKind::WATCH_KIND_NODE,
            "import" | "imports" => w::WatchKind::WATCH_KIND_IMPORT,
            "cluster" => w::WatchKind::WATCH_KIND_CLUSTER,
            "policy" => w::WatchKind::WATCH_KIND_POLICY,
            "datastore" | "datastores" => w::WatchKind::WATCH_KIND_DATASTORE,
            other => {
                println!(
                    "{} unknown kind '{other}' (vms, disks, images, networks, nodes, imports, cluster, policy, datastores)",
                    "Error:".red()
                );
                return;
            }
        };
        req.kinds.push(kind.into());
    }
    let t = match client.rpc_transport().await {
        Ok(t) => t,
        Err(e) => {
            println!("{} {}", "Error:".red(), e.message);
            return;
        }
    };
    let svc = w::WatchServiceClient::new(t.clone(), t.config());
    // What was last printed per (kind, id), to skip repeats after a reconnect.
    let mut last: std::collections::HashMap<(String, String), String> =
        std::collections::HashMap::new();
    let mut synced_once = false;
    println!("{} {} (Ctrl-C to stop)", "Watching".green(), kinds);
    let watch = async {
        loop {
            let mut stream = match svc.watch(req.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    println!("{} {}", "Error:".red(), tenancy::rpc_err(e));
                    return;
                }
            };
            loop {
                let e: w::WatchResponse = match stream.message().await {
                    Ok(Some(m)) => m.to_owned_message(),
                    // The server ended the stream (lifetime, restart): follow on.
                    Ok(None) | Err(_) => break,
                };
                let (kind, id, object) = watch_event(&e);
                use w::WatchEventType as T;
                match e.r#type.as_known() {
                    Some(T::WATCH_EVENT_TYPE_SYNCED) => {
                        if !synced_once {
                            println!(
                                "{} {} objects; changes follow",
                                "Synced:".green(),
                                last.len()
                            );
                            synced_once = true;
                        }
                    }
                    Some(T::WATCH_EVENT_TYPE_EXPIRED) => break,
                    Some(T::WATCH_EVENT_TYPE_ADDED | T::WATCH_EVENT_TYPE_MODIFIED) => {
                        let (name, what) = watch_summary(&kind, &object);
                        let line = format!("{:<8} {:<24} {}", kind, name, what);
                        if last.get(&(kind.clone(), id.clone())) != Some(&line) {
                            if synced_once {
                                println!("{}  {}", chrono_now(), line);
                            }
                            last.insert((kind, id), line);
                        }
                    }
                    Some(T::WATCH_EVENT_TYPE_DELETED) => {
                        if let Some(prev) = last.remove(&(kind.clone(), id)) {
                            let name = prev.split_whitespace().nth(1).unwrap_or("?").to_string();
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
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    };
    tokio::select! {
        _ = watch => {}
        _ = tokio::signal::ctrl_c() => println!(),
    }
}

/// A watch event as the kind's word, the id and the object's proto JSON.
fn watch_event(
    e: &glidex_api::proto::glidex::watch::v1::WatchResponse,
) -> (String, String, serde_json::Value) {
    use glidex_api::proto::glidex::watch::v1::{
        __buffa::oneof::watch_response::Object, WatchKind as K,
    };
    let kind = match e.kind.as_known() {
        Some(K::WATCH_KIND_VM) => "vm",
        Some(K::WATCH_KIND_DISK) => "disk",
        Some(K::WATCH_KIND_IMAGE) => "image",
        Some(K::WATCH_KIND_NETWORK) => "network",
        Some(K::WATCH_KIND_NODE) => "node",
        Some(K::WATCH_KIND_IMPORT) => "import",
        Some(K::WATCH_KIND_CLUSTER) => "cluster",
        Some(K::WATCH_KIND_POLICY) => "policy",
        Some(K::WATCH_KIND_DATASTORE) => "datastore",
        _ => "",
    };
    let json = |r: Result<serde_json::Value, serde_json::Error>| r.unwrap_or_default();
    let object = match &e.object {
        Some(Object::Vm(v)) => json(serde_json::to_value(v.as_ref())),
        Some(Object::Disk(v)) => json(serde_json::to_value(v.as_ref())),
        Some(Object::Image(v)) => json(serde_json::to_value(v.as_ref())),
        Some(Object::Network(v)) => json(serde_json::to_value(v.as_ref())),
        Some(Object::Node(v)) => json(serde_json::to_value(v.as_ref())),
        Some(Object::ImportPlan(v)) => json(serde_json::to_value(v.as_ref())),
        Some(Object::Cluster(v)) => json(serde_json::to_value(v.as_ref())),
        Some(Object::Policy(v)) => json(serde_json::to_value(v.as_ref())),
        Some(Object::Datastore(v)) => json(serde_json::to_value(v.as_ref())),
        None => serde_json::Value::Null,
    };
    (kind.to_string(), e.id.clone(), object)
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
        "  {} - Place a stopped or lost VM on another node (its disks on shared datastores)",
        "place <name|id> [--node N]".cyan()
    );
    println!(
        "  {} - Shut a VM down via its power button (default wait 60 s), then stop it",
        "stop <name|id> --graceful [secs]".cyan()
    );
    println!("  {}  - Pause a VM", "pause <name|id>".cyan());
    println!(
        "  {} - A kernel-boot VM's kernel; change it from the next start",
        "kernel <name|id> [--set <kernel image>] [--follow on|off] [--extract]".cyan()
    );
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
    println!(
        "  {} - Pull a kernel image, or take it from a disk image",
        "image pull <url> --kernel [--initrd-url U] [--cmdline C] | --kernel-from <image>".cyan()
    );
    println!(
        "  {} - Take the kernel out of a disk image",
        "image extract-kernel <name|id>".cyan()
    );
    println!("  {}   - Delete an image", "image rm <name|id>".cyan());
    println!(
        "  {}    - Datastores, and what each node finds",
        "datastore list".cyan()
    );
    println!(
        "  {} - A datastore and each node's view",
        "datastore show <name|id>".cyan()
    );
    println!(
        "  {} - Register a directory under /srv/glidex",
        "datastore create <name> (--path P [--shared] | --lvm VG/POOL) [--node-path NODE=P] [--nodes N,M] [--default] [--no-overcommit]"
            .cyan()
    );
    println!(
        "  {} - Change paths, nodes, the default, overcommit",
        "datastore set <name|id> [--node-path NODE=P] [--nodes N,M|--all-nodes] [--default|--no-default] [--overcommit|--no-overcommit]"
            .cyan()
    );
    println!(
        "  {}  - Delete a datastore no disk is on",
        "datastore rm <name|id>".cyan()
    );
    println!("  {}         - List disks", "disk list".cyan());
    println!(
        "  {}  - Disk details and partitions",
        "disk show <name|id>".cyan()
    );
    println!(
        "  {} - Create a disk",
        "disk create <name> [--size-gib N] [--image I] [--full] [--raw] [--no-extend] [--datastore D]".cyan()
    );
    println!(
        "  {} - Grow or shrink a disk",
        "disk resize <disk> <GiB> [--no-extend]".cyan()
    );
    println!(
        "  {} - Move a disk to another datastore (offline)",
        "disk move <disk> --datastore D [--node N] [--flatten]".cyan()
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
        "admission list | set <t> [vms=N … | unlimited] [--suspend|--resume] | remove <t> | offer <t> --directory ISSUER [--days N] | offers | revoke-offer <id>".cyan()
    );
    println!("  {} - Login connectors of this directory", "connector list | show <id> | add oidc|pam|google|jumpcloud <id> … | update <id> … | secret set <id> | test <id> | remove <id> | scim-token list|create|revoke <id>".cyan());
    println!("  {} - Invite an email address to a tenant (a link, shown once)", "invite list <tenant> | create <tenant> <email> --connector C [--role R] [--days N] | revoke <id>".cyan());
    println!("  {} - Workload federations: CI jobs exchange their OIDC tokens for a service account's", "federation list <tenant> | show <tenant> <id> | add <tenant> <id> --issuer URL --audience A (--match K=V... --service-account TOKEN [--ttl S] | --rules-file F) | enable|disable|remove <tenant> <id>".cyan());
    println!(
        "  {} - Directories this host serves (operator), and the token signing keys of yours (rotating needs a fresh login)",
        "directory list | create <id> --admin-email EMAIL [--name T] [--owner SLUG] | delete <id> | keys [rotate]".cyan()
    );
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
        "system-binding list [--node N] | add <role> <principal> [--node N] | remove <id>".cyan()
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
    println!(
        "  {} - A network's id, mode, grants or shares",
        "network show <net>".cyan()
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
    let w = access::who_am_i(client).await.ok()?;
    access::user_name(&w)
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
    "place",
    "stop",
    "pause",
    "kernel",
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
    "datastore",
    "datastores",
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
    "invite",
    "federation",
    "directory",
];

/// Subcommands offered by Tab after these commands.
const SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "image",
        &["catalog", "list", "pull", "extract-kernel", "rm"],
    ),
    ("datastore", &["list", "show", "create", "set", "rm"]),
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
            "device-pools",
            "device-pool",
        ],
    ),
    (
        "admission",
        &["list", "set", "remove", "offer", "offers", "revoke-offer"],
    ),
    (
        "connector",
        &["list", "show", "add", "update", "secret", "test", "remove"],
    ),
    ("invite", &["list", "create", "revoke"]),
    (
        "federation",
        &["list", "show", "add", "enable", "disable", "remove"],
    ),
    ("directory", &["list", "create", "delete", "keys"]),
    (
        "disk",
        &[
            "list",
            "show",
            "create",
            "resize",
            "move",
            "extend-root",
            "rm",
        ],
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
            "topology",
            "prepare-device",
            "release-device",
            "sriov",
            "cordon-device",
            "uncordon-device",
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
            "list", "show", "create", "rm", "grant", "share", "unshare", "shares", "accept",
            "leave",
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
        "datastores" => "datastore",
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
        "place" => vm::place(client, &parts[1..]).await,
        "pause" => vm::pause(client, &parts[1..]).await,
        "kernel" => vm::kernel(client, &parts[1..]).await,

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

        "nodes" | "node" => cluster::node(client, &parts[1..]).await,

        "pci" | "pci-devices" => host::pci(client, &parts[1..]).await,

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

        "networks" | "nets" => network::list_networks(client).await,

        "network" | "net" => network::network(client, &parts[1..]).await,
        "whoami" => access::whoami(client).await,
        "cluster" => cluster::cluster(client, &parts[1..]).await,
        "auth" => admin::auth(client, &parts[1..]).await,
        "login" => admin::login(client, &parts[1..]).await,
        "logout" => admin::logout(client, &parts[1..]).await,
        "ui" => admin::ui(),
        "token" | "tokens" => access::token(client, &parts[1..]).await,
        "tenant" | "tenants" => tenancy::tenant(client, &parts[1..]).await,
        "admission" | "admissions" => tenancy::admission(client, &parts[1..]).await,
        "connector" | "connectors" => directory::connector(client, &parts[1..]).await,
        "invite" | "invites" | "invitation" | "invitations" => {
            directory::invite(client, &parts[1..]).await
        }
        "federation" | "federations" => directory::federation(client, &parts[1..]).await,
        "directory" => directory::directory(client, &parts[1..]).await,
        "project" | "projects" => access::project(client, &parts[1..]).await,
        "binding" | "bindings" => access::binding(client, &parts[1..]).await,
        "system-binding" => access::system_binding(client, &parts[1..]).await,
        "user" | "users" => access::user(client, &parts[1..]).await,
        "team" | "teams" => access::team(client, &parts[1..]).await,
        "policy" | "policies" => access::policy(client, &parts[1..]).await,
        "audit" => access::audit(client, &parts[1..]).await,
        "usage" => usage::resource_usage(client, &parts[1..]).await,
        "stats" => usage::stats(client, &parts[1..]).await,
        "bandwidth" => usage::series(client, "bandwidth", &parts[1..]).await,
        "io" => usage::series(client, "io", &parts[1..]).await,
        "compute" => usage::series(client, "compute", &parts[1..]).await,

        "network-add" | "net-add" => network::create_host(client, &parts[1..]).await,

        "network-rm" | "net-rm" => match parts.get(1) {
            Some(name) => network::delete(client, name).await,
            None => println!("{}", "Usage: network-rm <name>".yellow()),
        },

        "bridges" => host::bridges(client).await,

        "ovs" => host::ovs(client, &parts[1..]).await,

        "uplinks" => host::uplinks(client, &parts[1..]).await,

        "uplink-add" => host::uplink_add(client, &parts[1..]).await,

        "uplink-reapply" => host::uplink_reapply(client, &parts[1..]).await,

        "uplink-rm" => host::uplink_rm(client, &parts[1..]).await,

        "image" | "images" => storage::image(client, &parts[1..]).await,

        "disk" | "disks" => storage::disk(client, &parts[1..]).await,

        "datastore" | "datastores" => storage::datastore(client, &parts[1..]).await,

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

/// Commands that never use the local session: on a cluster agent, which
/// has no API of its own, they run anyway; `auth login` is how one gets a
/// server to talk to.
fn needs_no_session(words: &[String]) -> bool {
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    matches!(
        w.as_slice(),
        ["auth", "login" | "use" | "profiles" | "trust", ..]
    )
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let client = match build_client(&cli) {
        Ok(c) => c,
        // The session isn't used: any client will do.
        Err(e) if e == client::AGENT_HINT && needs_no_session(&cli.command) => {
            match ApiClient::tcp(client::DEFAULT_URL, None) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{} {}", "Error:".red(), e);
                    std::process::exit(2);
                }
            }
        }
        Err(e) => {
            eprintln!("{} {}", "Error:".red(), e);
            std::process::exit(2);
        }
    };
    // A profile-bound run checks itself against the cluster it claims
    // before any command runs (§4.1): the binding mismatch is a wall
    // between the operator and a repurposed or hostile machine, so not
    // even a one-shot command slips past it.
    if !needs_no_session(&cli.command) {
        dirlogin::refresh_if_due(&client, cli.profile.as_deref()).await;
    }
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
    fn auth_login_runs_without_a_local_session() {
        let w = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert!(needs_no_session(&w(
            "auth login --url https://192.0.2.1:8841"
        )));
        assert!(needs_no_session(&w("auth use prod")));
        assert!(needs_no_session(&w("auth profiles")));
        assert!(!needs_no_session(&w("auth status")));
        assert!(!needs_no_session(&w("node list")));
        assert!(!needs_no_session(&[]));
        assert!(client::AGENT_HINT.contains("auth login --url https://"));
    }

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
    fn watch_events_carry_the_kind_id_and_object() {
        use glidex_api::proto::glidex::watch::v1 as w;
        let e = w::WatchResponse {
            r#type: w::WatchEventType::WATCH_EVENT_TYPE_ADDED.into(),
            kind: w::WatchKind::WATCH_KIND_VM.into(),
            id: "v1".into(),
            object: Some(w::__buffa::oneof::watch_response::Object::Vm(Box::new(
                glidex_api::proto::glidex::vm::v1::Vm {
                    name: "web".into(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        };
        let (kind, id, object) = watch_event(&e);
        assert_eq!((kind.as_str(), id.as_str()), ("vm", "v1"));
        assert_eq!(watch_summary(&kind, &object).0, "web");
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

    #[test]
    fn watch_summary_reads_proto_nodes_and_imports() {
        let n = serde_json::json!({
            "meta": {"id": "n-1", "name": "alpha", "generation": "2"},
            "spec": {"name": "alpha", "role": "NODE_ROLE_SERVER"},
            "status": {"phase": "NODE_PHASE_DRAINING", "ready": "CONDITION_STATUS_TRUE"},
        });
        assert_eq!(
            watch_summary("node", &n),
            ("alpha".to_string(), "Draining".to_string())
        );
        let n = serde_json::json!({
            "spec": {"name": "beta"},
            "status": {"phase": "NODE_PHASE_ACTIVE", "ready": "CONDITION_STATUS_UNKNOWN"},
        });
        assert_eq!(watch_summary("node", &n).1, "Active, not ready");
        let p = serde_json::json!({
            "plan": "p-1",
            "node": "edge",
            "state": "IMPORT_STATE_PENDING",
        });
        assert_eq!(
            watch_summary("import", &p),
            ("edge".to_string(), "pending".to_string())
        );
    }

    #[test]
    fn watch_summary_reads_proto_networks() {
        let n = serde_json::json!({
            "id": "6f1c0000-0000-4000-8000-000000000000",
            "name": "web",
            "phase": "NETWORK_PHASE_NETD_UNAVAILABLE",
        });
        assert_eq!(
            watch_summary("network", &n),
            ("web".to_string(), "netd unavailable".to_string())
        );
        let n = serde_json::json!({
            "name": "web",
            "phase": "NETWORK_PHASE_READY",
            "deletion_requested_at": "2026-10-17T10:00:00Z",
        });
        assert_eq!(watch_summary("network", &n).1, "deleting (ready)");
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
        // The standalone host's own node record, as `initialize` writes it.
        manager.report_self().unwrap();
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

        let who = access::who_am_i(&client).await.unwrap();
        assert_eq!(who.method, "disabled");

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
        let projects = access::list_projects(&client).await.unwrap();
        let project = projects[0].name.clone();
        assert_eq!(
            access::project_names(&client).await.get(&projects[0].id),
            Some(&project)
        );
        // Teams on their RPC client, by name.
        access::team(&client, &["create", "ops"]).await;
        let t = client.rpc_transport().await.unwrap();
        let teams =
            glidex_api::proto::glidex::access::v1::TeamServiceClient::new(t.clone(), t.config());
        let id = access::resolve_team(&teams, "ops").await.unwrap();
        assert_eq!(access::resolve_team(&teams, &id).await.unwrap(), id);
        assert!(access::resolve_team(&teams, "nope").await.is_err());
        client.set_project(Some(project.clone()));
        let creds = storage::list_credentials(&client).await.unwrap();
        assert_eq!(
            creds
                .iter()
                .map(|c| c.username.as_str())
                .collect::<Vec<_>>(),
            ["alice"]
        );
        assert_eq!(creds[0].project, projects[0].id);
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
            // The built-in datastore is there from the start.
            let ds = DatastoreServiceClient::new(t.clone(), t.config())
                .list_datastores(ListDatastoresRequest::default())
                .await
                .unwrap()
                .into_owned()
                .datastores;
            assert_eq!(ds.len(), 1);
            assert!(ds[0].builtin && ds[0].default_for_disks);
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

        // The console RPC goes over the same socket, on HTTP/2 of its own;
        // a refusal is the call's error.
        {
            let (t, config) = client.stream_transport().await.unwrap();
            let mut stream = glidex_api::proto::glidex::vm::v1::VmServiceClient::new(t, config)
                .console()
                .await
                .unwrap();
            stream
            .send(glidex_api::proto::glidex::vm::v1::ConsoleRequest {
                request: Some(
                    glidex_api::proto::glidex::vm::v1::__buffa::oneof::console_request::Request::Open(
                        Box::new(glidex_api::proto::glidex::vm::v1::ConsoleOpen {
                            id: "nope".into(),
                            ..Default::default()
                        }),
                    ),
                ),
                ..Default::default()
            })
            .await
            .unwrap();
            let e = stream.message().await.unwrap_err();
            assert_eq!(e.code, connectrpc::ErrorCode::NotFound, "{e:?}");
        }
        let e = console::open_console(&client, "nope").await.err().unwrap();
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

        // Networks on their RPC clients: by name or id.
        let before = network::list(&client).await.unwrap().len();
        let e = network::rpc(&client)
            .await
            .unwrap()
            .get_network(glidex_api::proto::glidex::network::v1::GetNetworkRequest {
                id: "nope".into(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, connectrpc::ErrorCode::NotFound);
        assert!(tenancy::rpc_err(e).contains("not_found"));
        // (Creating one needs glidex-netd, which this test doesn't run.)
        assert_eq!(network::list(&client).await.unwrap().len(), before);
        let shares = glidex_api::proto::glidex::network::v1::NetworkShareServiceClient::new(
            t.clone(),
            t.config(),
        )
        .list_network_shares(
            glidex_api::proto::glidex::network::v1::ListNetworkSharesRequest {
                project: project.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .into_owned();
        assert!(shares.shares.is_empty());
        let e = network::rpc(&client)
            .await
            .unwrap()
            .delete_network(
                glidex_api::proto::glidex::network::v1::DeleteNetworkRequest {
                    id: "nope".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(e.code, connectrpc::ErrorCode::NotFound);

        // Host networking and PCI devices on their RPC clients: the host
        // this socket belongs to.
        host::list_pci_devices(&client).await.unwrap();
        let status = host::rpc(&client)
            .await
            .unwrap()
            .get_ovs_status(glidex_api::proto::glidex::host::v1::GetOvsStatusRequest::default())
            .await
            .unwrap()
            .into_owned();
        let netd = status.netd.as_option().unwrap();
        // Without glidex-netd (as usually under test) the status says why.
        if netd.available != Some(true) {
            assert!(netd.error.is_some(), "{netd:?}");
            assert!(status.host.as_option().is_none());
        }

        // The cluster and its nodes on their RPC clients: a standalone
        // host says so and lists its one node.
        {
            use glidex_api::proto::glidex::cluster::v1::*;
            let status = cluster::rpc(&client)
                .await
                .unwrap()
                .get_cluster_status(GetClusterStatusRequest::default())
                .await
                .unwrap()
                .into_owned();
            assert_eq!(status.clustered, Some(false));
            assert_eq!(status.nodes.len(), 1, "{status:?}");
            let nodes = cluster::list_nodes(&client).await.unwrap();
            assert_eq!(nodes.len(), 1);
            let id = nodes[0].meta.as_option().unwrap().id.clone();
            let got = cluster::node_rpc(&client)
                .await
                .unwrap()
                .get_node(GetNodeRequest {
                    id: id.clone(),
                    ..Default::default()
                })
                .await
                .unwrap()
                .into_owned();
            assert_eq!(
                got.node.as_option().unwrap().meta.as_option().unwrap().id,
                id
            );
            let e = cluster::node_rpc(&client)
                .await
                .unwrap()
                .get_node(GetNodeRequest {
                    id: "nope".into(),
                    ..Default::default()
                })
                .await
                .unwrap_err();
            assert_eq!(e.code, connectrpc::ErrorCode::NotFound);
            // Imports need a cluster.
            let e = cluster::import_rpc(&client)
                .await
                .unwrap()
                .list_imports(ListImportsRequest::default())
                .await
                .unwrap_err();
            assert!(tenancy::rpc_err(e).contains("not_clustered"));
        }

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
