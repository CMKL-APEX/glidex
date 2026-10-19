//! The cluster, its nodes and import plans (spec/clustering.md §5–§11) on
//! the generated `glidex.cluster.v1` clients, over the Connect protocol
//! (spec/grpc.md §9.1): `cluster …` and `node …`. Init, join and leave act
//! on the host gxctl is connected to (on an unclustered host: its local
//! socket). `cluster snapshot` (a binary download) stays on REST.

use crate::admin::positional;
use crate::client::{ApiClient, RpcTransport};
use crate::host::from_word;
use crate::tenancy::{err, rpc_err, usage};
use crate::vm::enum_word;
use crate::{flag_value, flag_values, format_unix_time, has_flag};
use buffa::{EnumValue, Enumeration};
use buffa_types::google::protobuf::Timestamp;
use colored::Colorize;
use glidex_api::proto::glidex::cluster::v1::*;
use hyper::Method;
use tabled::{Table, Tabled};
use zeroize::Zeroizing;

pub(crate) async fn rpc(client: &ApiClient) -> Result<ClusterServiceClient<RpcTransport>, String> {
    let t = client.rpc_transport().await.map_err(|e| e.message)?;
    Ok(ClusterServiceClient::new(t.clone(), t.config()))
}

pub(crate) async fn node_rpc(
    client: &ApiClient,
) -> Result<NodeServiceClient<RpcTransport>, String> {
    let t = client.rpc_transport().await.map_err(|e| e.message)?;
    Ok(NodeServiceClient::new(t.clone(), t.config()))
}

pub(crate) async fn import_rpc(
    client: &ApiClient,
) -> Result<ImportServiceClient<RpcTransport>, String> {
    let t = client.rpc_transport().await.map_err(|e| e.message)?;
    Ok(ImportServiceClient::new(t.clone(), t.config()))
}

/// A service client, or the error shown and the command returned.
macro_rules! svc {
    ($e:expr) => {
        match $e.await {
            Ok(s) => s,
            Err(e) => return err(e),
        }
    };
}

/// An answer, or the error shown and the command returned.
macro_rules! answer {
    ($e:expr) => {
        match $e.await {
            Ok(r) => r.into_owned(),
            Err(e) => return err(rpc_err(e)),
        }
    };
}

// ---- words --------------------------------------------------------------------

/// `server` / `agent`.
fn role_word(r: &EnumValue<NodeRole>) -> String {
    enum_word(r, "NODE_ROLE_")
}

/// The capitalized word gxctl prints for phases, readiness and Raft states
/// (`Active`, `True`, `Leader`).
fn title_word<E: Enumeration>(e: &EnumValue<E>, prefix: &str) -> String {
    let w = enum_word(e, prefix);
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

fn phase_word(p: &EnumValue<NodePhase>) -> String {
    title_word(p, "NODE_PHASE_")
}

fn ready_word(r: &EnumValue<glidex_api::proto::glidex::common::v1::ConditionStatus>) -> String {
    title_word(r, "CONDITION_STATUS_")
}

fn when(t: Option<&Timestamp>) -> String {
    t.map(|t| format_unix_time(t.seconds.max(0) as u64))
        .unwrap_or_else(|| "-".into())
}

fn num<T: std::fmt::Display>(n: Option<T>) -> String {
    n.map(|n| n.to_string()).unwrap_or_else(|| "-".into())
}

fn text(s: &Option<String>) -> &str {
    s.as_deref().unwrap_or("")
}

/// `--flag a=b`, repeatable, as a map.
fn pairs<M: FromIterator<(String, String)>>(args: &[&str], flag: &str) -> M {
    flag_values(args, flag)
        .into_iter()
        .filter_map(|v| v.split_once('='))
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

/// A proto message as its JSON.
fn json<T: serde::Serialize>(m: &T, pretty: bool) -> String {
    if pretty {
        serde_json::to_string_pretty(m).unwrap_or_default()
    } else {
        serde_json::to_string(m).unwrap_or_default()
    }
}

// ---- cluster ------------------------------------------------------------------

/// A token read from a 0600 file or stdin, never from argv.
fn read_token(args: &[&str]) -> Result<Zeroizing<String>, String> {
    use std::io::Read;
    let raw = match flag_value(args, "--token-file") {
        Some(p) => {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(p)
                .map_err(|e| format!("{p}: {e}"))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(format!("{p} is readable by others; chmod 600 it"));
            }
            std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))?
        }
        None => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| e.to_string())?;
            s
        }
    };
    let t = raw.trim().to_string();
    if t.is_empty() {
        return Err("no token given (pass --token-file <file> or pipe it on stdin)".into());
    }
    Ok(Zeroizing::new(t))
}

/// `--role server|agent` (default `default`).
fn role_flag(args: &[&str], default: &str) -> Result<NodeRole, String> {
    let word = flag_value(args, "--role").unwrap_or(default);
    from_word(word, "NODE_ROLE_")
        .ok_or_else(|| format!("unknown --role {word}: use server or agent"))
}

/// The body of `cluster join` (without `--import`).
fn join_request(args: &[&str], server: &str, token: &str) -> Result<JoinClusterRequest, String> {
    Ok(JoinClusterRequest {
        server: server.to_string(),
        token: token.to_string(),
        role: role_flag(args, "server")?.into(),
        advertise: flag_value(args, "--advertise").map(str::to_string),
        tunnel_ip: flag_value(args, "--tunnel-ip").map(str::to_string),
        name: flag_value(args, "--name").map(str::to_string),
        ..Default::default()
    })
}

/// The body of `cluster join --import`.
fn import_join_request(args: &[&str], server: &str, token: &str) -> ImportJoinRequest {
    ImportJoinRequest {
        server: server.to_string(),
        token: token.to_string(),
        advertise: flag_value(args, "--advertise").map(str::to_string),
        tunnel_ip: flag_value(args, "--tunnel-ip").map(str::to_string),
        name: flag_value(args, "--name").map(str::to_string),
        ..Default::default()
    }
}

/// The body of `cluster join-token [--role server|agent] [--ttl <hours>h] [--allow-import]`.
fn join_token_request(args: &[&str]) -> Result<CreateJoinTokenRequest, String> {
    Ok(CreateJoinTokenRequest {
        role: role_flag(args, "agent")?.into(),
        ttl_secs: flag_value(args, "--ttl").and_then(|t| {
            t.trim_end_matches('h')
                .parse::<u64>()
                .ok()
                .map(|h| h * 3600)
        }),
        allow_import: has_flag(args, "--allow-import"),
        ..Default::default()
    })
}

/// What `cluster status` prints.
fn status_lines(v: &GetClusterStatusResponse) -> Vec<String> {
    if v.clustered != Some(true) {
        return vec!["This host is standalone (not in a cluster).".into()];
    }
    let name = text(&v.name);
    let mut out = vec![format!(
        "Cluster {}  node {} ({}, {})",
        text(&v.cluster_id),
        name,
        role_word(&v.role),
        text(&v.advertise)
    )];
    if let Some(r) = v.raft.as_option() {
        out.push(format!(
            "  Raft: {} term {}  leader: {}  applied {}  last log {}",
            title_word(&r.state, "RAFT_STATE_"),
            r.term,
            match r.leader.as_option() {
                None => "none".into(),
                Some(l) => format!("{} ({})", text(&l.name), text(&l.address)),
            },
            num(r.last_applied),
            num(r.last_log_index)
        ));
        for (kind, members) in [("voter", &r.voters), ("learner", &r.learners)] {
            for m in members {
                out.push(format!(
                    "    {kind:<8} {}  {}",
                    text(&m.name),
                    text(&m.address)
                ));
            }
        }
    }
    for n in &v.nodes {
        out.push(format!(
            "  node {:<16} {:<7} {:<10} {}  v{} level {}",
            n.name,
            role_word(&n.role),
            phase_word(&n.phase),
            text(&n.advertise),
            text(&n.version),
            num(n.feature_level)
        ));
    }
    out.push(format!("  Feature level: {}", num(v.feature_level)));
    if let Some(ca) = v.ca.as_option() {
        out.push(format!(
            "  CA rotation: signing {}…, {} old CA(s) still trusted",
            ca.signing.chars().take(12).collect::<String>(),
            ca.retiring.len()
        ));
    }
    let gaps: Vec<&PortCheck> = v
        .ports
        .iter()
        .filter(|p| p.reachable == Some(false))
        .collect();
    for p in &gaps {
        out.push(format!(
            "  {} port {}/{} on {} is not reachable from {}",
            "!".yellow(),
            p.port,
            p.proto,
            p.node,
            name
        ));
    }
    // `status` always asks for the port check.
    if gaps.is_empty() {
        out.push("  Ports: every checked port answers".into());
    }
    out
}

pub async fn cluster(client: &ApiClient, args: &[&str]) {
    match args.first().copied() {
        Some("init") => {
            if !has_flag(args, "--force") {
                return usage(
                    "Usage: cluster init [--advertise <ip:port>] [--tunnel-ip <ip>] --force\n  Turns this host into a cluster of one and re-keys its records. It keeps a backup but can't be undone.",
                );
            }
            let req = InitClusterRequest {
                advertise: flag_value(args, "--advertise").map(str::to_string),
                tunnel_ip: flag_value(args, "--tunnel-ip").map(str::to_string),
                force: true,
                ..Default::default()
            };
            let v = answer!(svc!(rpc(client)).init_cluster(req));
            println!(
                "{} cluster {} (this node: {})",
                "Created".green(),
                v.cluster_id,
                v.node_id
            );
            println!("  Backup:      {}", v.backup);
            println!("  CA SHA-256:  {}", v.ca_fingerprint);
        }
        Some("join") => {
            let Some(server) = flag_value(args, "--server") else {
                return usage(
                    "Usage: cluster join --server <ip:8842> [--role server|agent] [--advertise <ip:port>] [--name <name>] [--token-file <file>]\n  The token is read from the file or stdin, never from the command line.",
                );
            };
            let token = match read_token(args) {
                Ok(t) => t,
                Err(e) => return err(e),
            };
            if has_flag(args, "--import") {
                let req = import_join_request(args, server, &token);
                let v = answer!(svc!(rpc(client)).import_join(req));
                println!("{} {}", "Waiting:".green(), v.message);
                return;
            }
            let req = match join_request(args, server, &token) {
                Ok(r) => r,
                Err(e) => return err(e),
            };
            let v = answer!(svc!(rpc(client)).join_cluster(req));
            println!(
                "{} cluster {} as node {}",
                "Joined".green(),
                v.cluster_id,
                v.node_id
            );
        }
        Some("join-token") => {
            let req = match join_token_request(args) {
                Ok(r) => r,
                Err(e) => return err(e),
            };
            let v = answer!(svc!(rpc(client)).create_join_token(req));
            println!("{}", v.token);
            eprintln!(
                "Shown once. Valid {} s; for a {} node. Servers: {}",
                v.ttl_secs,
                role_word(&v.role),
                v.servers.join(", ")
            );
        }
        Some("status") | None => {
            let v = answer!(
                svc!(rpc(client)).get_cluster_status(GetClusterStatusRequest {
                    ports: true,
                    ..Default::default()
                })
            );
            for l in status_lines(&v) {
                println!("{}", l);
            }
        }
        Some("promote") => {
            let nodes: Vec<&str> = positional(&args[1..], &[]);
            if nodes.is_empty() {
                return usage("Usage: cluster promote <node>… [--force]");
            }
            let req = PromoteNodesRequest {
                nodes: nodes.iter().map(|n| n.to_string()).collect(),
                force: has_flag(args, "--force"),
                ..Default::default()
            };
            let v = answer!(svc!(rpc(client)).promote_nodes(req));
            println!("{} {} voters", "OK".green(), v.voters);
        }
        Some("rejoin") => {
            let Some(server) = flag_value(args, "--server") else {
                return usage(
                    "Usage: cluster rejoin --server <ip:8842> [--node-id <id>] [--advertise <ip:port>] [--token-file <file>]\n  Comes back as the same node. The token is from `gxctl node rejoin-token` and is read from the file or stdin.",
                );
            };
            let token = match read_token(args) {
                Ok(t) => t,
                Err(e) => return err(e),
            };
            let req = RejoinClusterRequest {
                server: server.to_string(),
                token: token.to_string(),
                node_id: flag_value(args, "--node-id").map(str::to_string),
                advertise: flag_value(args, "--advertise").map(str::to_string),
                tunnel_ip: flag_value(args, "--tunnel-ip").map(str::to_string),
                ..Default::default()
            };
            let v = answer!(svc!(rpc(client)).rejoin_cluster(req));
            println!(
                "{} node {} (Raft state {})",
                "Rejoined".green(),
                v.node_id,
                if v.raft_fresh == Some(true) {
                    "started afresh"
                } else {
                    "kept"
                }
            );
        }
        Some("dissolve") => {
            if !has_flag(args, "--force") {
                return usage(
                    "Usage: cluster dissolve --force\n  The last node ends the cluster and becomes a standalone host with everything it held. Other nodes must have left. A final snapshot is kept.",
                );
            }
            let v = answer!(svc!(rpc(client)).dissolve_cluster(DissolveClusterRequest {
                force: true,
                ..Default::default()
            }));
            println!(
                "{} {} VM(s), {} disk(s) kept; snapshot {}. Restart the control plane to continue standalone.",
                "Dissolved:".green(),
                v.vms,
                v.disks,
                v.snapshot
            );
        }
        Some("leave") => {
            let v = answer!(svc!(rpc(client)).leave_cluster(LeaveClusterRequest::default()));
            println!("{} {}: {}", "Left".green(), v.left, v.restart);
        }
        Some("rotate-ca") => {
            let req = RotateCaRequest {
                grace_secs: flag_value(args, "--grace").and_then(|g| g.parse::<u64>().ok()),
                ..Default::default()
            };
            let v = answer!(svc!(rpc(client)).rotate_ca(req));
            println!(
                "{} new CA {}; {} old CA(s) retire when every node renewed (or at {})",
                "Rotating".green(),
                v.signing_ca,
                v.retiring,
                when(v.retire_at.as_option())
            );
        }
        // A new key for the connector secrets' data keys (spec/tenancy.md §13).
        Some("rotate-secrets-key") => {
            let v =
                answer!(svc!(rpc(client)).rotate_secrets_key(RotateSecretsKeyRequest::default()));
            println!(
                "{} secrets key {} on {} server(s); {} data key(s) re-wrapped",
                "Rotated:".green(),
                v.kek_id,
                v.servers,
                v.rewrapped
            );
        }
        Some("snapshot") => {
            let Some(file) = args.get(1) else {
                return usage("Usage: cluster snapshot <file>");
            };
            match client
                .request_bytes(Method::GET, "/cluster/snapshot", None)
                .await
            {
                Ok(r) => {
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    match std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(file)
                        .and_then(|mut f| f.write_all(&r.body))
                    {
                        Ok(()) => println!(
                            "{} {} ({} bytes, 0600: it holds credential and token hashes; store it like the database)",
                            "Wrote".green(),
                            file,
                            r.body.len()
                        ),
                        Err(e) => err(format!("{file}: {e}")),
                    }
                }
                Err(e) => err(e.message),
            }
        }
        Some(other) => usage(&format!(
            "Unknown cluster command '{other}'. Try: init, join, join-token, status, promote, snapshot, rotate-ca, rotate-secrets-key, rejoin, leave, dissolve"
        )),
    }
}

// ---- nodes --------------------------------------------------------------------

/// A `nodes` row.
#[derive(Debug, Tabled, PartialEq)]
pub(crate) struct NodeRow {
    id: String,
    name: String,
    role: String,
    phase: String,
    ready: String,
    cpus: u32,
    memory_mib: u64,
}

impl From<&Node> for NodeRow {
    fn from(n: &Node) -> Self {
        let meta = n.meta.as_option().cloned().unwrap_or_default();
        let spec = n.spec.as_option().cloned().unwrap_or_default();
        let status = n.status.as_option().cloned().unwrap_or_default();
        let cap = status.capacity.as_option().cloned().unwrap_or_default();
        NodeRow {
            id: meta.id,
            name: spec.name,
            role: role_word(&spec.role),
            phase: phase_word(&status.phase),
            ready: ready_word(&status.ready),
            cpus: cap.cpus,
            memory_mib: cap.memory_mib,
        }
    }
}

pub(crate) async fn list_nodes(client: &ApiClient) -> Result<Vec<Node>, String> {
    node_rpc(client)
        .await?
        .list_nodes(ListNodesRequest::default())
        .await
        .map(|r| r.into_owned().nodes)
        .map_err(rpc_err)
}

/// The id of the node `key` (an id or a name) names.
fn find_node(nodes: &[Node], key: &str) -> Option<String> {
    nodes
        .iter()
        .filter_map(|n| n.meta.as_option().map(|m| (m, n.spec.as_option())))
        .find(|(m, s)| m.id == key || s.is_some_and(|s| s.name == key))
        .map(|(m, _)| m.id.clone())
}

/// What `node drain|undrain` prints.
fn drain_lines(node: &str, phase: &EnumValue<NodePhase>, r: Option<&NodeContents>) -> [String; 2] {
    let r = r.cloned().unwrap_or_default();
    [
        format!("{} {} is now {}", "OK".green(), node, phase_word(phase)),
        format!(
            "  still on it: {} VMs, {} disks, {} networks",
            r.vms.len(),
            r.disks.len(),
            r.networks.len()
        ),
    ]
}

/// The membership operations of `node remove|forget|purge|rejoin-token|detach <node> …`.
#[derive(Debug, PartialEq)]
enum MemberOp {
    Remove(RemoveNodeRequest),
    Forget(ForgetNodeRequest),
    Purge(PurgeNodeRequest),
    RejoinToken(CreateRejoinTokenRequest),
    Detach(DetachNodeRequest),
    AbortDetach(AbortDetachRequest),
    ForgetDeparted(ForgetDepartedRequest),
}

/// The request of `node <op> <node> [flags]`; flags an operation doesn't
/// take are ignored. The node's name goes as it is: the server resolves
/// names too.
fn member_op(op: &str, id: &str, rest: &[&str]) -> Option<MemberOp> {
    let id = id.to_string();
    Some(match op {
        "remove" => MemberOp::Remove(RemoveNodeRequest {
            id,
            force: has_flag(rest, "--force"),
            ..Default::default()
        }),
        "forget" => MemberOp::Forget(ForgetNodeRequest {
            id,
            fenced: has_flag(rest, "--fenced"),
            force: has_flag(rest, "--force"),
            ..Default::default()
        }),
        "purge" => MemberOp::Purge(PurgeNodeRequest {
            id,
            ..Default::default()
        }),
        "rejoin-token" => MemberOp::RejoinToken(CreateRejoinTokenRequest {
            id,
            raft_intact: Some(!has_flag(rest, "--raft-lost")),
            ..Default::default()
        }),
        "detach" if has_flag(rest, "--abort") => MemberOp::AbortDetach(AbortDetachRequest {
            id,
            ..Default::default()
        }),
        "detach" if has_flag(rest, "--departed") => {
            MemberOp::ForgetDeparted(ForgetDepartedRequest {
                id,
                ..Default::default()
            })
        }
        "detach" => MemberOp::Detach(DetachNodeRequest {
            id,
            map_networks: pairs(rest, "--map-network"),
            with_access: has_flag(rest, "--with-access"),
            timeout_secs: flag_value(rest, "--timeout").and_then(|t| t.parse::<u64>().ok()),
            ..Default::default()
        }),
        _ => return None,
    })
}

/// Run a membership operation: `OK <the answer as JSON>`, or the rejoin
/// token on stdout with its note on stderr.
async fn run_member_op(client: &ApiClient, op: MemberOp) {
    let s = svc!(node_rpc(client));
    let out = match op {
        MemberOp::Remove(r) => json(&answer!(s.remove_node(r)), false),
        MemberOp::Forget(r) => json(&answer!(s.forget_node(r)), false),
        MemberOp::Purge(r) => json(&answer!(s.purge_node(r)), false),
        MemberOp::Detach(r) => json(&answer!(s.detach_node(r)), false),
        MemberOp::AbortDetach(r) => json(&answer!(s.abort_detach(r)), false),
        MemberOp::ForgetDeparted(r) => json(&answer!(s.forget_departed(r)), false),
        MemberOp::RejoinToken(r) => {
            let v = answer!(s.create_rejoin_token(r));
            println!("{}", v.token);
            eprintln!(
                "Shown once, for node {} ({}). On the host, as root: glidex-install --rejoin <server:8842>, token on stdin.{}",
                v.node,
                v.node_id,
                if v.raft_intact == Some(false) {
                    " Its Raft state will be started afresh."
                } else {
                    ""
                }
            );
            return;
        }
    };
    println!("{} {}", "OK".green(), out);
}

/// A PCI class code's word (spec/host-topology.md §3.3).
fn class_word(class: &str) -> &'static str {
    let c = class.trim_start_matches("0x");
    if c.starts_with("03") {
        "gpu"
    } else if c.starts_with("0108") {
        "nvme"
    } else if c.starts_with("02") {
        "nic"
    } else if c.starts_with("12") || c.starts_with("0b40") {
        "accel"
    } else {
        "other"
    }
}

/// `node topology`: each NUMA node (CPUs, memory, hugepages) with the
/// devices a VM could be given on it, then the warnings; `--devices`: the
/// devices only, with where they sit in the PCIe tree.
pub(crate) fn topology_lines(
    t: &glidex_api::proto::glidex::host::v1::Topology,
    devices_only: bool,
) -> Vec<String> {
    use glidex_api::proto::glidex::host::v1::PciFunction;
    let dev = |d: &PciFunction| {
        let ready = if d.ready {
            "ready".green().to_string()
        } else {
            format!("{} ({})", "not ready".yellow(), d.not_ready.join("; "))
        };
        let mut l = format!(
            "{}  {:<5} {}:{}  {}",
            d.address,
            class_word(&d.class_id),
            d.vendor_id.trim_start_matches("0x"),
            d.device_id.trim_start_matches("0x"),
            d.driver.as_deref().unwrap_or("-")
        );
        if let Some(g) = &d.iommu_group {
            l.push_str(&format!("  group {g}"));
        }
        if devices_only {
            if let Some(n) = d.numa_node {
                l.push_str(&format!("  numa {n}"));
            }
            if let Some(r) = &d.root_port {
                l.push_str(&format!("  root {r}"));
            }
            if let Some(s) = &d.switch {
                l.push_str(&format!("  switch {s}"));
            }
            if let Some(link) = d.link.as_option() {
                l.push_str(&format!("  {} x{}", link.current_speed, link.current_width));
            }
            if let Some(v) = d.sriov.as_option() {
                l.push_str(&format!("  vfs {}/{}", v.enabled_vfs, v.total_vfs));
            }
        }
        format!("{l}  {ready}")
    };
    let assignable: Vec<&PciFunction> = t.devices.iter().filter(|d| d.assignable).collect();
    let mut out = Vec::new();
    if devices_only {
        out.extend(assignable.iter().map(|d| dev(d)));
    } else {
        let s = t.summary.as_option().cloned().unwrap_or_default();
        for n in &s.numa_nodes {
            let cpus: Vec<u32> = n
                .cores
                .iter()
                .flat_map(|c| c.threads.iter().copied())
                .collect();
            let mut hp: Vec<String> = n
                .hugepages
                .iter()
                .map(|(kib, pages)| format!("{pages}×{kib}kB"))
                .collect();
            hp.sort();
            out.push(format!(
                "{} {}  {} cores, cpus {}  {} MiB{}",
                "numa".bold(),
                n.id,
                n.cores.len(),
                glidex_control_plane::topology::cpu_list(&cpus),
                n.memory_mib,
                if hp.is_empty() {
                    String::new()
                } else {
                    format!("  hugepages {}", hp.join(" "))
                }
            ));
            for d in assignable.iter().filter(|d| d.numa_node == Some(n.id)) {
                out.push(format!("  {}", dev(d)));
            }
        }
        let loose: Vec<&&PciFunction> = assignable
            .iter()
            .filter(|d| d.numa_node.is_none())
            .collect();
        if !loose.is_empty() {
            out.push(format!("{} (no NUMA node)", "numa".bold()));
            out.extend(loose.iter().map(|d| format!("  {}", dev(d))));
        }
        let mut reserved: Vec<(u32, String)> = s.reserved_cpus.clone().into_iter().collect();
        reserved.sort();
        if !reserved.is_empty() {
            let cpus: Vec<u32> = reserved.iter().map(|(c, _)| *c).collect();
            out.push(format!(
                "reserved cpus {}",
                glidex_control_plane::topology::cpu_list(&cpus)
            ));
        }
    }
    for w in &t.warnings {
        out.push(format!("{} {}: {}", "warning".yellow(), w.kind, w.detail));
    }
    out
}

/// `nodes`, `node <id>`, `node drain|undrain|remove|forget|purge|rejoin-token|detach|import|topology …`
pub async fn node(client: &ApiClient, args: &[&str]) {
    match args.first().copied() {
        Some(op @ ("drain" | "undrain")) => {
            let Some(key) = args.get(1) else {
                return usage(&format!("Usage: node {op} <node>"));
            };
            // Accept a node's name as well as its id.
            let nodes = match list_nodes(client).await {
                Ok(n) => n,
                Err(e) => return err(e),
            };
            let Some(id) = find_node(&nodes, key) else {
                return err(format!("no node {key}"));
            };
            let s = svc!(node_rpc(client));
            let lines = if op == "drain" {
                let v = answer!(s.drain_node(DrainNodeRequest {
                    id,
                    ..Default::default()
                }));
                drain_lines(&v.node, &v.phase, v.remaining.as_option())
            } else {
                let v = answer!(s.undrain_node(UndrainNodeRequest {
                    id,
                    ..Default::default()
                }));
                drain_lines(&v.node, &v.phase, v.remaining.as_option())
            };
            for l in lines {
                println!("{}", l);
            }
        }
        Some("import") => imports(client, &args[1..]).await,
        Some("topology") => {
            let Some(key) = args.get(1).filter(|a| !a.starts_with("--")) else {
                return usage("Usage: node topology <node> [--devices]");
            };
            let s = svc!(node_rpc(client));
            let v = answer!(s.get_node_topology(GetNodeTopologyRequest {
                id: key.to_string(),
                ..Default::default()
            }));
            let t = v.topology.as_option().cloned().unwrap_or_default();
            for l in topology_lines(&t, has_flag(args, "--devices")) {
                println!("{}", l);
            }
        }
        Some(op @ ("remove" | "forget" | "purge" | "rejoin-token" | "detach")) => {
            let Some(id) = args.get(1).filter(|a| !a.starts_with("--")) else {
                return usage(&format!(
                    "Usage: node {op} <node> [--force] [--fenced] [--raft-lost]"
                ));
            };
            if let Some(req) = member_op(op, id, &args[2..]) {
                run_member_op(client, req).await;
            }
        }
        Some(id) => {
            let v = answer!(svc!(node_rpc(client)).get_node(GetNodeRequest {
                id: id.to_string(),
                ..Default::default()
            }));
            println!(
                "{}",
                json(&v.node.as_option().cloned().unwrap_or_default(), true)
            );
        }
        None => match list_nodes(client).await {
            Ok(nodes) => {
                let rows: Vec<NodeRow> = nodes.iter().map(NodeRow::from).collect();
                println!("{}", Table::new(rows));
            }
            Err(e) => err(e),
        },
    }
}

// ---- imports ------------------------------------------------------------------

const IMPORT_USAGE: &str = "Usage: node import list | show <plan> | approve <plan> [--project src=existing|src=new:name] [--network a=b] [--credential proj/user=new] [--over-quota] [--dry-run] | reject <plan>";

/// `pending`, `committed`, `rejected`.
fn import_state(p: &ImportPlan) -> String {
    enum_word(&p.state, "IMPORT_STATE_")
}

/// A `node import list` row.
#[derive(Debug, Tabled, PartialEq)]
struct ImportRow {
    plan: String,
    node: String,
    state: String,
    vms: u32,
    disks: u32,
    networks: u32,
    problems: usize,
    expires: String,
}

impl From<&ImportPlan> for ImportRow {
    fn from(p: &ImportPlan) -> Self {
        let s = p.summary.as_option().cloned().unwrap_or_default();
        ImportRow {
            plan: p.plan.clone(),
            node: p.node.clone(),
            state: import_state(p),
            vms: s.vms,
            disks: s.disks,
            networks: s.networks,
            problems: p.problems.len(),
            expires: when(p.expires_at.as_option()),
        }
    }
}

fn sorted<'a>(
    m: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> Vec<(&'a String, &'a String)> {
    let mut v: Vec<_> = m.into_iter().collect();
    v.sort();
    v
}

/// What `node import show <plan>` prints.
fn plan_lines(p: &ImportPlan) -> Vec<String> {
    let s = p.summary.as_option().cloned().unwrap_or_default();
    let mut out = vec![
        format!("Plan:      {}", p.plan),
        format!("Node:      {} ({})", p.node, p.node_id),
        format!("Advertise: {}", p.advertise),
        format!("State:     {}", import_state(p)),
        format!(
            "Created:   {} by {}",
            when(p.created_at.as_option()),
            p.created_by
        ),
        format!("Expires:   {}", when(p.expires_at.as_option())),
        format!(
            "Brings:    {} VM(s), {} disk(s), {} image(s), {} network(s), {} credential(s), {} project(s)",
            s.vms, s.disks, s.images, s.networks, s.credentials, s.projects
        ),
    ];
    if !p.problems.is_empty() {
        out.push("Problems:".into());
        out.extend(p.problems.iter().map(|x| format!("  - {x}")));
    }
    if let Some(m) = p.mappings.as_option() {
        let mut lines = Vec::new();
        for (kind, map) in [
            ("project", &m.projects),
            ("network", &m.networks),
            ("credential", &m.credentials),
        ] {
            for (a, b) in sorted(map) {
                lines.push(format!("  {kind} {a} → {b}"));
            }
        }
        if m.over_quota {
            lines.push("  over quota: accepted".into());
        }
        if !lines.is_empty() {
            out.push("Mappings:".into());
            out.extend(lines);
        }
    }
    if let Some(src) = p.sources.as_option() {
        out.push("Sources:".into());
        for x in &src.projects {
            out.push(format!("  project    {} {}", x.id, text(&x.name)));
        }
        for x in &src.networks {
            out.push(format!(
                "  network    {} {} ({})",
                x.id,
                text(&x.name),
                text(&x.mode)
            ));
        }
        for x in &src.credentials {
            out.push(format!("  credential {}", x.key));
        }
        for x in &src.vms {
            out.push(format!(
                "  vm         {} {} (project {})",
                x.id,
                text(&x.name),
                text(&x.project)
            ));
        }
    }
    out
}

/// The body of `node import approve <plan> …`.
fn approve_request(plan: &str, rest: &[&str]) -> ApproveImportRequest {
    ApproveImportRequest {
        plan: plan.to_string(),
        projects: pairs(rest, "--project"),
        networks: pairs(rest, "--network"),
        credentials: pairs(rest, "--credential"),
        over_quota: has_flag(rest, "--over-quota"),
        dry_run: has_flag(rest, "--dry-run"),
        ..Default::default()
    }
}

/// `node import list | show | approve | reject`
async fn imports(client: &ApiClient, rest: &[&str]) {
    let sub = rest.first().copied().unwrap_or("list");
    let plan = rest.get(1).copied().filter(|a| !a.starts_with("--"));
    match (sub, plan) {
        ("list", _) => {
            let v = answer!(svc!(import_rpc(client)).list_imports(ListImportsRequest::default()));
            if v.plans.is_empty() {
                println!("No import plans.");
            } else {
                let rows: Vec<ImportRow> = v.plans.iter().map(ImportRow::from).collect();
                println!("{}", Table::new(rows));
            }
        }
        ("show", Some(p)) => {
            let v = answer!(svc!(import_rpc(client)).get_import(GetImportRequest {
                plan: p.to_string(),
                ..Default::default()
            }));
            for l in plan_lines(&v.plan.as_option().cloned().unwrap_or_default()) {
                println!("{}", l);
            }
        }
        ("approve", Some(p)) => {
            let v = answer!(svc!(import_rpc(client)).approve_import(approve_request(p, rest)));
            println!("{}", json(&v, true));
        }
        ("reject", Some(p)) => {
            answer!(svc!(import_rpc(client)).reject_import(RejectImportRequest {
                plan: p.to_string(),
                ..Default::default()
            }));
            println!("{} import plan {} rejected", "OK".green(), p);
        }
        _ => usage(IMPORT_USAGE),
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn topology_lists_numa_nodes_their_devices_and_warnings() {
        use glidex_api::proto::glidex::host::v1 as h;
        colored::control::set_override(false);
        let gpu = |a: &str, numa: Option<u32>, ready: bool| h::PciFunction {
            address: a.into(),
            vendor_id: "0x10de".into(),
            device_id: "0x20b0".into(),
            class_id: "0x030200".into(),
            driver: Some("vfio-pci".into()),
            iommu_group: Some("226".into()),
            numa_node: numa,
            switch: Some("0000:85:00.0".into()),
            assignable: true,
            ready,
            not_ready: if ready {
                vec![]
            } else {
                vec!["not bound to vfio-pci (driver: nvidia)".into()]
            },
            ..Default::default()
        };
        let t = h::Topology {
            summary: h::TopologySummary {
                numa_nodes: vec![h::NumaNode {
                    id: 7,
                    cores: vec![h::CpuCore {
                        threads: vec![112, 240],
                        ..Default::default()
                    }],
                    memory_mib: 129000,
                    hugepages: [("2048".to_string(), 512)].into_iter().collect(),
                    ..Default::default()
                }],
                reserved_cpus: [(112, "config".to_string())].into_iter().collect(),
                ..Default::default()
            }
            .into(),
            devices: vec![
                gpu("0000:87:00.0", Some(7), true),
                gpu("0000:09:00.0", None, false),
            ],
            warnings: vec![h::TopologyWarning {
                kind: "numa_unknown".into(),
                detail: "0000:09:00.0 has no NUMA node (firmware reports -1)".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let l = topology_lines(&t, false);
        assert_eq!(
            l[0],
            "numa 7  1 cores, cpus 112,240  129000 MiB  hugepages 512×2048kB"
        );
        assert_eq!(
            l[1],
            "  0000:87:00.0  gpu   10de:20b0  vfio-pci  group 226  ready"
        );
        assert_eq!(l[2], "numa (no NUMA node)");
        assert!(
            l[3].contains("not ready (not bound to vfio-pci (driver: nvidia))"),
            "{l:?}"
        );
        assert_eq!(l[4], "reserved cpus 112");
        assert!(l[5].starts_with("warning numa_unknown:"), "{l:?}");
        let d = topology_lines(&t, true);
        assert!(d[0].contains("numa 7  switch 0000:85:00.0"), "{d:?}");
    }

    use super::*;
    use glidex_api::proto::glidex::common::v1::ConditionStatus;

    /// A line without its ANSI colours.
    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in s.chars() {
            match (esc, c) {
                (false, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }

    fn node(id: &str, name: &str) -> Node {
        Node {
            meta: NodeMeta {
                id: id.into(),
                name: name.into(),
                ..Default::default()
            }
            .into(),
            spec: NodeSpec {
                name: name.into(),
                role: NodeRole::NODE_ROLE_SERVER.into(),
                ..Default::default()
            }
            .into(),
            status: NodeStatus {
                phase: NodePhase::NODE_PHASE_DRAINING.into(),
                ready: ConditionStatus::CONDITION_STATUS_TRUE.into(),
                capacity: Resources {
                    cpus: 8,
                    memory_mib: 16384,
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn words_are_the_ones_gxctl_printed() {
        assert_eq!(role_word(&NodeRole::NODE_ROLE_AGENT.into()), "agent");
        assert_eq!(phase_word(&NodePhase::NODE_PHASE_ACTIVE.into()), "Active");
        assert_eq!(
            phase_word(&NodePhase::NODE_PHASE_FORGOTTEN.into()),
            "Forgotten"
        );
        assert_eq!(
            ready_word(&ConditionStatus::CONDITION_STATUS_UNKNOWN.into()),
            "Unknown"
        );
        assert_eq!(
            title_word(
                &EnumValue::from(RaftState::RAFT_STATE_LEADER),
                "RAFT_STATE_"
            ),
            "Leader"
        );
        assert_eq!(phase_word(&NodePhase::NODE_PHASE_UNSPECIFIED.into()), "");
        let p = ImportPlan {
            state: ImportState::IMPORT_STATE_COMMITTED.into(),
            ..Default::default()
        };
        assert_eq!(import_state(&p), "committed");
    }

    #[test]
    fn node_rows_and_lookup() {
        let n = node("n-1", "alpha");
        assert_eq!(
            NodeRow::from(&n),
            NodeRow {
                id: "n-1".into(),
                name: "alpha".into(),
                role: "server".into(),
                phase: "Draining".into(),
                ready: "True".into(),
                cpus: 8,
                memory_mib: 16384,
            }
        );
        let nodes = [n, node("n-2", "beta")];
        assert_eq!(find_node(&nodes, "beta").as_deref(), Some("n-2"));
        assert_eq!(find_node(&nodes, "n-1").as_deref(), Some("n-1"));
        assert_eq!(find_node(&nodes, "gamma"), None);
        // The proto JSON (the REST twin's answer): 64-bit numbers as strings.
        let j: ListNodesResponse = serde_json::from_str(
            r#"{"nodes": [{"meta": {"id": "n-3", "name": "c", "generation": "2"},
                "spec": {"name": "c", "role": "NODE_ROLE_AGENT"},
                "status": {"phase": "NODE_PHASE_ACTIVE", "ready": "CONDITION_STATUS_FALSE",
                    "capacity": {"cpus": 4, "memory_mib": "8192"}}}]}"#,
        )
        .unwrap();
        let r = NodeRow::from(&j.nodes[0]);
        assert_eq!(
            (r.role.as_str(), r.phase.as_str(), r.ready.as_str()),
            ("agent", "Active", "False")
        );
        assert_eq!((r.cpus, r.memory_mib), (4, 8192));
        let table = Table::new([r]).to_string();
        assert!(table.contains("memory_mib"), "{table}");
    }

    #[test]
    fn drain_lines_count_what_remains() {
        let r = NodeContents {
            vms: vec!["v1".into(), "v2".into()],
            networks: vec!["lan".into()],
            ..Default::default()
        };
        let l = drain_lines("alpha", &NodePhase::NODE_PHASE_DRAINING.into(), Some(&r));
        assert_eq!(plain(&l[0]), "OK alpha is now Draining");
        assert_eq!(l[1], "  still on it: 2 VMs, 0 disks, 1 networks");
        let l = drain_lines("alpha", &NodePhase::NODE_PHASE_ACTIVE.into(), None);
        assert_eq!(l[1], "  still on it: 0 VMs, 0 disks, 0 networks");
    }

    #[test]
    fn member_flags_map_to_the_requests() {
        assert_eq!(
            member_op("remove", "alpha", &["--force"]),
            Some(MemberOp::Remove(RemoveNodeRequest {
                id: "alpha".into(),
                force: true,
                ..Default::default()
            }))
        );
        assert_eq!(
            member_op("forget", "alpha", &["--fenced", "--raft-lost"]),
            Some(MemberOp::Forget(ForgetNodeRequest {
                id: "alpha".into(),
                fenced: true,
                ..Default::default()
            }))
        );
        assert_eq!(
            member_op("purge", "alpha", &["--force"]),
            Some(MemberOp::Purge(PurgeNodeRequest {
                id: "alpha".into(),
                ..Default::default()
            }))
        );
        let Some(MemberOp::RejoinToken(r)) = member_op("rejoin-token", "a", &[]) else {
            panic!()
        };
        assert_eq!(r.raft_intact, Some(true));
        let Some(MemberOp::RejoinToken(r)) = member_op("rejoin-token", "a", &["--raft-lost"])
        else {
            panic!()
        };
        assert_eq!(r.raft_intact, Some(false));
        let Some(MemberOp::Detach(d)) = member_op(
            "detach",
            "alpha",
            &[
                "--map-network",
                "web=local-web",
                "--map-network",
                "db=local-db",
                "--with-access",
                "--timeout",
                "600",
            ],
        ) else {
            panic!()
        };
        assert_eq!(d.map_networks.len(), 2);
        assert_eq!(d.map_networks["web"], "local-web");
        assert!(d.with_access);
        assert_eq!(d.timeout_secs, Some(600));
        assert!(matches!(
            member_op("detach", "alpha", &["--abort"]),
            Some(MemberOp::AbortDetach(AbortDetachRequest { ref id, .. })) if id == "alpha"
        ));
        assert!(matches!(
            member_op("detach", "alpha", &["--departed"]),
            Some(MemberOp::ForgetDeparted(_))
        ));
        assert_eq!(member_op("drain", "alpha", &[]), None);
    }

    #[test]
    fn join_and_token_flags_map_to_the_requests() {
        let r = join_request(
            &[
                "join",
                "--server",
                "10.0.0.1:8842",
                "--role",
                "agent",
                "--name",
                "n2",
            ],
            "10.0.0.1:8842",
            "tok",
        )
        .unwrap();
        assert_eq!(r.role.as_known(), Some(NodeRole::NODE_ROLE_AGENT));
        assert_eq!(
            (r.server.as_str(), r.token.as_str()),
            ("10.0.0.1:8842", "tok")
        );
        assert_eq!(r.name.as_deref(), Some("n2"));
        assert_eq!(r.advertise, None);
        let r = join_request(&["join"], "s", "t").unwrap();
        assert_eq!(r.role.as_known(), Some(NodeRole::NODE_ROLE_SERVER));
        let e = join_request(&["join", "--role", "boss"], "s", "t").unwrap_err();
        assert!(e.contains("unknown --role boss"), "{e}");

        let r = import_join_request(&["join", "--import", "--tunnel-ip", "10.1.0.2"], "s", "t");
        assert_eq!(r.tunnel_ip.as_deref(), Some("10.1.0.2"));

        let r = join_token_request(&["join-token"]).unwrap();
        assert_eq!(r.role.as_known(), Some(NodeRole::NODE_ROLE_AGENT));
        assert_eq!(r.ttl_secs, None);
        assert!(!r.allow_import);
        let r = join_token_request(&[
            "join-token",
            "--role",
            "server",
            "--ttl",
            "2h",
            "--allow-import",
        ])
        .unwrap();
        assert_eq!(r.role.as_known(), Some(NodeRole::NODE_ROLE_SERVER));
        assert_eq!(r.ttl_secs, Some(7200));
        assert!(r.allow_import);
    }

    #[test]
    fn status_shows_raft_nodes_and_ports() {
        let standalone = GetClusterStatusResponse {
            clustered: Some(false),
            ..Default::default()
        };
        assert_eq!(
            status_lines(&standalone),
            ["This host is standalone (not in a cluster)."]
        );
        let v: GetClusterStatusResponse = serde_json::from_str(
            r#"{"clustered": true, "cluster_id": "c-1", "node_id": "n-1", "name": "alpha",
                "role": "NODE_ROLE_SERVER", "advertise": "10.0.0.1:8842", "feature_level": 3,
                "ca": {"signing": "0123456789abcdef", "retiring": ["old"]},
                "raft": {"id": "1", "state": "RAFT_STATE_LEADER", "term": "4",
                    "leader": {"raft_id": "1", "name": "alpha", "address": "10.0.0.1:8843"},
                    "voters": [{"raft_id": "1", "name": "alpha", "address": "10.0.0.1:8843"}],
                    "learners": [{"raft_id": "2", "name": "beta", "address": "10.0.0.2:8843"}],
                    "last_log_index": "120", "last_applied": "118"},
                "nodes": [{"id": "n-1", "name": "alpha", "role": "NODE_ROLE_SERVER",
                    "phase": "NODE_PHASE_ACTIVE", "advertise": "10.0.0.1:8842", "version": "0.9.0",
                    "feature_level": 3, "ready": "CONDITION_STATUS_TRUE"},
                    {"id": "n-2", "name": "beta", "role": "NODE_ROLE_AGENT",
                    "phase": "NODE_PHASE_DRAINING", "ready": "CONDITION_STATUS_FALSE"}],
                "ports": [{"node": "beta", "port": 6642, "proto": "tcp", "reachable": false},
                    {"node": "beta", "port": 8842, "proto": "tcp", "reachable": true}]}"#,
        )
        .unwrap();
        let l: Vec<String> = status_lines(&v).iter().map(|l| plain(l)).collect();
        assert_eq!(
            l,
            [
                "Cluster c-1  node alpha (server, 10.0.0.1:8842)",
                "  Raft: Leader term 4  leader: alpha (10.0.0.1:8843)  applied 118  last log 120",
                "    voter    alpha  10.0.0.1:8843",
                "    learner  beta  10.0.0.2:8843",
                "  node alpha            server  Active     10.0.0.1:8842  v0.9.0 level 3",
                "  node beta             agent   Draining     v level -",
                "  Feature level: 3",
                "  CA rotation: signing 0123456789ab…, 1 old CA(s) still trusted",
                "  ! port 6642/tcp on beta is not reachable from alpha",
            ]
        );
        // An agent: no Raft; every port answers.
        let v = GetClusterStatusResponse {
            clustered: Some(true),
            cluster_id: Some("c-1".into()),
            name: Some("beta".into()),
            role: NodeRole::NODE_ROLE_AGENT.into(),
            feature_level: Some(3),
            ..Default::default()
        };
        let l: Vec<String> = status_lines(&v).iter().map(|l| plain(l)).collect();
        assert_eq!(
            l,
            [
                "Cluster c-1  node beta (agent, )",
                "  Feature level: 3",
                "  Ports: every checked port answers",
            ]
        );
    }

    #[test]
    fn import_plans_list_show_and_approve() {
        let p: ImportPlan = serde_json::from_str(
            r#"{"plan": "p-1", "state": "IMPORT_STATE_PENDING", "node": "edge", "node_id": "n-9",
                "advertise": "10.0.0.9:8842", "summary": {"vms": 2, "disks": 3, "networks": 1},
                "problems": ["project lab exists"],
                "mappings": {"projects": {"s1": "new:lab2"}},
                "created_by": "alice", "created_at": "1970-01-02T00:00:00Z",
                "expires_at": "1970-01-03T00:00:00Z",
                "sources": {"projects": [{"id": "s1", "name": "lab"}],
                    "networks": [{"id": "x", "name": "web", "mode": "nat"}],
                    "credentials": [{"key": "s1/bob"}],
                    "vms": [{"id": "v1", "name": "db", "project": "s1"}]}}"#,
        )
        .unwrap();
        let row = ImportRow::from(&p);
        assert_eq!(row.state, "pending");
        assert_eq!(
            (row.vms, row.disks, row.networks, row.problems),
            (2, 3, 1, 1)
        );
        assert_eq!(row.expires, format_unix_time(2 * 86_400));
        let l = plan_lines(&p);
        assert_eq!(l[3], "State:     pending");
        assert_eq!(
            l[4],
            format!("Created:   {} by alice", format_unix_time(86_400))
        );
        assert!(l.contains(&"  - project lab exists".to_string()), "{l:?}");
        assert!(l.contains(&"  project s1 → new:lab2".to_string()), "{l:?}");
        assert!(l.contains(&"  network    x web (nat)".to_string()), "{l:?}");
        assert!(
            l.contains(&"  vm         v1 db (project s1)".to_string()),
            "{l:?}"
        );

        let r = approve_request(
            "p-1",
            &[
                "approve",
                "p-1",
                "--project",
                "s1=new:lab2",
                "--network",
                "web=web2",
                "--credential",
                "s1/bob=bob2",
                "--dry-run",
            ],
        );
        assert_eq!(r.plan, "p-1");
        assert_eq!(r.projects["s1"], "new:lab2");
        assert_eq!(r.networks["web"], "web2");
        assert_eq!(r.credentials["s1/bob"], "bob2");
        assert!(r.dry_run && !r.over_quota);
    }
}
