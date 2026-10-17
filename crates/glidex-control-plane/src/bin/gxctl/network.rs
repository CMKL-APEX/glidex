//! Networks, their grants and shares (spec/networking.md §11,
//! spec/security.md §6.2) on the generated `glidex.network.v1` clients,
//! over the Connect protocol (spec/grpc.md §9.1): `networks`,
//! `network-add`, `network-rm` and `network …`. Networks are keyed by id;
//! every argument naming one takes an id or a name, which the server
//! resolves (spec/tenancy.md D24). Host networking (`ovs`, `bridges`,
//! uplinks, PCI devices) stays REST, in `main.rs`.

use crate::admin::positional;
use crate::client::{ApiClient, RpcTransport};
use crate::tenancy::{err, rpc_err, usage};
use crate::vm::enum_word;
use crate::{display_opt_u16, display_option, flag_value, flag_values, format_unix_time, has_flag};
use colored::Colorize;
use glidex_api::proto::glidex::common::v1::ConditionStatus;
use glidex_api::proto::glidex::network::v1::*;
use std::collections::HashMap;
use tabled::{Table, Tabled};

pub(crate) type Networks = NetworkServiceClient<RpcTransport>;
pub(crate) type Shares = NetworkShareServiceClient<RpcTransport>;

pub(crate) async fn rpc(client: &ApiClient) -> Result<Networks, String> {
    let t = client.rpc_transport().await.map_err(|e| e.message)?;
    Ok(NetworkServiceClient::new(t.clone(), t.config()))
}

async fn shares_rpc(client: &ApiClient) -> Result<Shares, String> {
    let t = client.rpc_transport().await.map_err(|e| e.message)?;
    Ok(NetworkShareServiceClient::new(t.clone(), t.config()))
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

/// `nat`, `bridged`, `isolated` (the old REST words).
fn mode_word(n: &Network) -> String {
    enum_word(&n.mode, "NETWORK_MODE_")
}

// ---- calls --------------------------------------------------------------------

/// Host networks, and the project networks the caller can see.
pub(crate) async fn list(client: &ApiClient) -> Result<Vec<Network>, String> {
    rpc(client)
        .await?
        .list_networks(ListNetworksRequest::default())
        .await
        .map(|r| r.into_owned().networks)
        .map_err(rpc_err)
}

/// The names to offer for a VM's NICs, each once (two tenants' projects
/// may each have a `web`; the server resolves a name among what the caller
/// can see).
pub(crate) async fn names(client: &ApiClient) -> Vec<String> {
    let mut names: Vec<String> = list(client)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|n| n.name)
        .collect();
    let mut seen = std::collections::HashSet::new();
    names.retain(|n| seen.insert(n.clone()));
    names
}

// ---- networks -----------------------------------------------------------------

#[derive(Debug, Tabled, PartialEq)]
pub(crate) struct NetworkRow {
    id: String,
    name: String,
    mode: String,
    bridge: String,
    port_type: String,
    #[tabled(display_with = "display_opt_u16")]
    vlan: Option<u16>,
    /// Owning project (project networks), by name when known; host
    /// networks have none.
    #[tabled(display_with = "display_option")]
    project: Option<String>,
}

fn rows(nets: Vec<Network>, project_names: &HashMap<String, String>) -> Vec<NetworkRow> {
    nets.into_iter()
        .map(|n| NetworkRow {
            mode: mode_word(&n),
            port_type: enum_word(&n.port_type, "PORT_TYPE_"),
            vlan: n.vlan.map(|v| v as u16),
            project: n
                .project
                .map(|p| project_names.get(&p).cloned().unwrap_or(p)),
            id: n.id,
            name: n.name,
            bridge: n.bridge,
        })
        .collect()
}

/// `networks`, `network list`
pub async fn list_networks(client: &ApiClient) {
    match list(client).await {
        Ok(nets) if nets.is_empty() => {
            println!("No networks. Create one with 'network create <name>'.")
        }
        Ok(nets) => {
            let names = crate::project_names(client).await;
            println!("{}", Table::new(rows(nets, &names)))
        }
        Err(e) => err(e),
    }
}

/// The body of `network-add <name> …` (a host network).
fn host_network_request(name: &str, args: &[&str]) -> Result<CreateNetworkRequest, String> {
    let (mode, bridge) = if let Some(b) = flag_value(args, "--bridged") {
        (NetworkMode::NETWORK_MODE_BRIDGED, Some(b))
    } else if has_flag(args, "--isolated") {
        (
            NetworkMode::NETWORK_MODE_ISOLATED,
            flag_value(args, "--bridge"),
        )
    } else {
        (NetworkMode::NETWORK_MODE_NAT, flag_value(args, "--bridge"))
    };
    // Cluster (OVN) or one node's bridge; default: the cluster's when OVN is enabled.
    let scope = if has_flag(args, "--cluster") {
        NetworkScope::NETWORK_SCOPE_CLUSTER
    } else if has_flag(args, "--node") {
        NetworkScope::NETWORK_SCOPE_NODE
    } else {
        NetworkScope::NETWORK_SCOPE_UNSPECIFIED
    };
    let vlan = match flag_value(args, "--vlan") {
        Some(v) => Some(
            v.parse::<u16>()
                .map_err(|_| "--vlan must be a number".to_string())?
                .into(),
        ),
        None => None,
    };
    Ok(CreateNetworkRequest {
        name: name.to_string(),
        mode: mode.into(),
        port_type: if has_flag(args, "--vhost-user") {
            PortType::PORT_TYPE_VHOST_USER
        } else {
            PortType::PORT_TYPE_TAP
        }
        .into(),
        bridge: bridge.map(str::to_string),
        scope: scope.into(),
        subnet: flag_value(args, "--subnet").map(str::to_string),
        vlan,
        ..Default::default()
    })
}

/// `network-add <name> …`, `network create <name> …` without `--project`
pub async fn create_host(client: &ApiClient, args: &[&str]) {
    let Some(name) = args.first().filter(|a| !a.starts_with("--")) else {
        println!("{}", "Usage: network-add <name> [--nat [--subnet CIDR] | --isolated | --bridged <bridge>] [--vhost-user] [--vlan N] [--bridge NAME] [--cluster | --node]".yellow());
        return;
    };
    let req = match host_network_request(name, args) {
        Ok(r) => r,
        Err(e) => return err(e),
    };
    match svc!(rpc(client)).create_network(req).await {
        Ok(r) => {
            let n = r
                .into_owned()
                .network
                .as_option()
                .cloned()
                .unwrap_or_default();
            println!(
                "{} {} ({} on {})",
                "Network created:".green(),
                n.name.yellow(),
                mode_word(&n),
                n.bridge
            )
        }
        Err(e) => err(rpc_err(e)),
    }
}

/// The body of `network create <name> --project P …` (a project network:
/// NAT or isolated, the bridge is generated).
fn project_network_request(
    project: &str,
    name: &str,
    args: &[&str],
) -> CreateProjectNetworkRequest {
    CreateProjectNetworkRequest {
        project: project.to_string(),
        name: name.to_string(),
        mode: if has_flag(args, "--isolated") {
            NetworkMode::NETWORK_MODE_ISOLATED
        } else {
            NetworkMode::NETWORK_MODE_NAT
        }
        .into(),
        port_type: if has_flag(args, "--vhost-user") {
            PortType::PORT_TYPE_VHOST_USER
        } else {
            PortType::PORT_TYPE_UNSPECIFIED
        }
        .into(),
        subnet: flag_value(args, "--subnet").map(str::to_string),
        mtu: flag_value(args, "--mtu")
            .and_then(|m| m.parse::<u16>().ok())
            .map(u32::from),
        ..Default::default()
    }
}

/// `network-rm <net>`, `network rm <net>`: wait up to a minute for it to
/// be gone.
pub async fn delete(client: &ApiClient, net: &str) {
    match svc!(rpc(client))
        .delete_network(DeleteNetworkRequest {
            id: net.to_string(),
            wait_secs: Some(60),
            ..Default::default()
        })
        .await
    {
        Ok(r) => print_delete_result(net, r.into_owned().network.as_option()),
        Err(e) => err(rpc_err(e)),
    }
}

fn print_delete_result(name: &str, still: Option<&Network>) {
    let Some(n) = still else {
        return println!("{} {}", "Network deleted:".green(), name);
    };
    println!(
        "{} {} (the control plane finishes it)",
        "Deleting:".yellow(),
        name
    );
    if let Some(ready) = n.conditions.iter().find(|c| {
        c.kind == "Ready" && c.status.as_known() != Some(ConditionStatus::CONDITION_STATUS_TRUE)
    }) {
        println!(
            "  {} {}: {}",
            "Waiting:".yellow(),
            ready.reason,
            ready.message
        );
    }
}

/// `network show <net>`
async fn show(client: &ApiClient, net: &str) {
    let n = match svc!(rpc(client))
        .get_network(GetNetworkRequest {
            id: net.to_string(),
            ..Default::default()
        })
        .await
    {
        Ok(r) => r
            .into_owned()
            .network
            .as_option()
            .cloned()
            .unwrap_or_default(),
        Err(e) => return err(rpc_err(e)),
    };
    let names = crate::project_names(client).await;
    let project = |p: &str| {
        names
            .get(p)
            .map(|n| format!("{} ({})", n, p))
            .unwrap_or_else(|| p.to_string())
    };
    let list = |v: &[String]| {
        if v.is_empty() {
            "-".to_string()
        } else {
            v.iter().map(|p| project(p)).collect::<Vec<_>>().join(", ")
        }
    };
    println!("  Name:       {}", n.name.yellow());
    println!("  Id:         {}", n.id);
    println!(
        "  Mode:       {} ({})",
        mode_word(&n),
        enum_word(&n.port_type, "PORT_TYPE_")
    );
    let scope = enum_word(&n.scope, "NETWORK_SCOPE_");
    match &n.node {
        Some(node) => println!("  Scope:      {} {}", scope, node),
        None => println!("  Scope:      {}", scope),
    }
    println!("  Bridge:     {}", n.bridge);
    if let Some(v) = n.vlan {
        println!("  VLAN:       {}", v);
    }
    if let Some(m) = n.mtu {
        println!("  MTU:        {}", m);
    }
    if let Some(r) = &n.router {
        println!("  Router:     {}", r);
    }
    println!(
        "  Phase:      {}",
        enum_word(&n.phase, "NETWORK_PHASE_").replace('_', " ")
    );
    match &n.project {
        Some(p) => {
            println!("  Project:    {}", project(p));
            println!("  Shared with: {}", list(&n.shares));
        }
        None => {
            if n.all_projects == Some(true) {
                println!("  Grants:     every project");
            } else {
                println!("  Grants:     {}", list(&n.grants));
                println!("  Tenants:    {}", n.tenant_grants.join(", "));
            }
        }
    }
    if let Some(ready) = n.conditions.iter().find(|c| {
        c.kind == "Ready" && c.status.as_known() != Some(ConditionStatus::CONDITION_STATUS_TRUE)
    }) {
        println!("  Waiting:    {}: {}", ready.reason, ready.message);
    }
}

/// One line of `network shares <project>`.
fn share_line(sh: &NetworkShare) -> String {
    let status = enum_word(&sh.status, "SHARE_STATUS_");
    let name = if sh.network_name.is_empty() {
        &sh.network
    } else {
        &sh.network_name
    };
    format!(
        "  {:<20} from {:<38} {}{}",
        name.cyan(),
        sh.owner_project.as_deref().unwrap_or("-"),
        if status == "accepted" {
            status.green().to_string()
        } else {
            status.yellow().to_string()
        },
        sh.expires_at
            .as_option()
            .map(|t| format!(" (until {})", format_unix_time(t.seconds.max(0) as u64)))
            .unwrap_or_default()
    )
}

// ---- `network …` ----------------------------------------------------------------

pub async fn network(client: &ApiClient, args: &[&str]) {
    let u = "Usage: network list | show <net> | create <name> [--project P [--isolated] [--vhost-user]] [--subnet CIDR] ... | rm <name> | grant <host-net> [--project P]... [--tenant T]... [--all-projects] | share <net> <project-id> | unshare <net> <project-id> | shares <project> | accept <project> <net> | leave <project> <net>";
    match args.first().copied().unwrap_or("list") {
        "list" | "ls" => list_networks(client).await,
        "show" | "get" => match args.get(1) {
            Some(net) => show(client, net).await,
            None => usage(u),
        },
        "create" | "add" => match flag_value(args, "--project") {
            Some(p) => {
                let pos = positional(&args[1..], &["--project", "--subnet", "--mtu"]);
                let Some(name) = pos.first() else {
                    return usage(u);
                };
                let req = project_network_request(p, name, args);
                let isolated = req.mode.as_known() == Some(NetworkMode::NETWORK_MODE_ISOLATED);
                match svc!(rpc(client)).create_project_network(req).await {
                    Ok(r) => {
                        let n = r
                            .into_owned()
                            .network
                            .as_option()
                            .cloned()
                            .unwrap_or_default();
                        println!(
                            "{} {} ({} on {}, project {})",
                            "Network created:".green(),
                            n.name.yellow(),
                            if isolated { "isolated" } else { "NAT" },
                            n.bridge,
                            p
                        )
                    }
                    Err(e) => err(rpc_err(e)),
                }
            }
            None => create_host(client, &args[1..]).await,
        },
        "rm" | "delete" => match args.get(1) {
            Some(name) => delete(client, name).await,
            None => usage(u),
        },
        // Who may use a host network: projects, whole tenants (D25), or all.
        // The lists replace the network's current grants.
        "grant" => {
            let Some(net) = args.get(1) else {
                return usage(u);
            };
            let req = GrantNetworkRequest {
                id: net.to_string(),
                grants: flag_values(args, "--project")
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                tenants: flag_values(args, "--tenant")
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                all_projects: has_flag(args, "--all-projects"),
                ..Default::default()
            };
            match svc!(rpc(client)).grant_network(req).await {
                Ok(r) => {
                    let n = r
                        .into_owned()
                        .network
                        .as_option()
                        .cloned()
                        .unwrap_or_default();
                    println!("{} {}", "Grants set:".green(), n.name.yellow());
                    if n.all_projects == Some(true) {
                        println!("  every project");
                    }
                    println!("  projects: {}", n.grants.join(", "));
                    println!("  tenants:  {}", n.tenant_grants.join(", "));
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "share" => {
            let (Some(net), Some(project)) = (args.get(1), args.get(2)) else {
                return usage(u);
            };
            match svc!(shares_rpc(client))
                .offer_network_share(OfferNetworkShareRequest {
                    id: net.to_string(),
                    project: project.to_string(),
                    ..Default::default()
                })
                .await
            {
                Ok(_) => println!(
                    "{} {} offered to project {}. Its owner accepts with 'network accept {} {}' within 7 days.",
                    "Offered:".green(),
                    net,
                    project,
                    project,
                    net
                ),
                Err(e) => err(rpc_err(e)),
            }
        }
        "unshare" => {
            let (Some(net), Some(project)) = (args.get(1), args.get(2)) else {
                return usage(u);
            };
            match svc!(shares_rpc(client))
                .unshare_network(UnshareNetworkRequest {
                    id: net.to_string(),
                    project: project.to_string(),
                    ..Default::default()
                })
                .await
            {
                Ok(_) => println!(
                    "{} {} no longer shared with {}",
                    "OK:".green(),
                    net,
                    project
                ),
                Err(e) => err(rpc_err(e)),
            }
        }
        "shares" => {
            let Some(project) = args
                .get(1)
                .map(|s| s.to_string())
                .or_else(|| client.project())
            else {
                return usage(u);
            };
            match svc!(shares_rpc(client))
                .list_network_shares(ListNetworkSharesRequest {
                    project: project.clone(),
                    ..Default::default()
                })
                .await
            {
                Ok(r) => {
                    let shares = r.into_owned().shares;
                    if shares.is_empty() {
                        println!("No network shares for {}.", project);
                    }
                    for sh in &shares {
                        println!("{}", share_line(sh));
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        "accept" => {
            let (Some(project), Some(net)) = (args.get(1), args.get(2)) else {
                return usage(u);
            };
            match svc!(shares_rpc(client))
                .accept_network_share(AcceptNetworkShareRequest {
                    project: project.to_string(),
                    network: net.to_string(),
                    ..Default::default()
                })
                .await
            {
                Ok(_) => println!(
                    "{} project {} can now attach VMs to {}",
                    "Accepted:".green(),
                    project,
                    net
                ),
                Err(e) => err(rpc_err(e)),
            }
        }
        "leave" => {
            let (Some(project), Some(net)) = (args.get(1), args.get(2)) else {
                return usage(u);
            };
            match svc!(shares_rpc(client))
                .leave_network_share(LeaveNetworkShareRequest {
                    project: project.to_string(),
                    network: net.to_string(),
                    ..Default::default()
                })
                .await
            {
                Ok(_) => println!("{} project {} left {}", "OK:".green(), project, net),
                Err(e) => err(rpc_err(e)),
            }
        }
        _ => usage(u),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_network_flags_map_to_the_request() {
        let r = host_network_request(
            "lan",
            &["lan", "--bridged", "br0", "--vlan", "7", "--cluster"],
        )
        .unwrap();
        assert_eq!(r.name, "lan");
        assert_eq!(r.mode.as_known(), Some(NetworkMode::NETWORK_MODE_BRIDGED));
        assert_eq!(r.bridge.as_deref(), Some("br0"));
        assert_eq!(r.vlan, Some(7));
        assert_eq!(
            r.scope.as_known(),
            Some(NetworkScope::NETWORK_SCOPE_CLUSTER)
        );
        assert_eq!(r.port_type.as_known(), Some(PortType::PORT_TYPE_TAP));

        let r = host_network_request(
            "n",
            &["n", "--subnet", "10.9.0.0/24", "--vhost-user", "--node"],
        )
        .unwrap();
        assert_eq!(r.mode.as_known(), Some(NetworkMode::NETWORK_MODE_NAT));
        assert_eq!(r.subnet.as_deref(), Some("10.9.0.0/24"));
        assert_eq!(r.port_type.as_known(), Some(PortType::PORT_TYPE_VHOST_USER));
        assert_eq!(r.scope.as_known(), Some(NetworkScope::NETWORK_SCOPE_NODE));
        assert_eq!(r.bridge, None);

        let r = host_network_request("i", &["i", "--isolated", "--bridge", "gxbr-i"]).unwrap();
        assert_eq!(r.mode.as_known(), Some(NetworkMode::NETWORK_MODE_ISOLATED));
        assert_eq!(r.bridge.as_deref(), Some("gxbr-i"));
        assert_eq!(
            r.scope.as_known(),
            Some(NetworkScope::NETWORK_SCOPE_UNSPECIFIED)
        );

        assert!(host_network_request("x", &["x", "--vlan", "seven"]).is_err());
    }

    #[test]
    fn project_network_flags_map_to_the_request() {
        let r = project_network_request("lab", "web", &["create", "web", "--project", "lab"]);
        assert_eq!((r.project.as_str(), r.name.as_str()), ("lab", "web"));
        assert_eq!(r.mode.as_known(), Some(NetworkMode::NETWORK_MODE_NAT));
        assert_eq!(
            r.port_type.as_known(),
            Some(PortType::PORT_TYPE_UNSPECIFIED)
        );
        assert_eq!((r.subnet.clone(), r.mtu), (None, None));

        let r = project_network_request(
            "lab",
            "db",
            &[
                "create",
                "db",
                "--isolated",
                "--vhost-user",
                "--mtu",
                "9000",
                "--subnet",
                "10.1.0.0/24",
            ],
        );
        assert_eq!(r.mode.as_known(), Some(NetworkMode::NETWORK_MODE_ISOLATED));
        assert_eq!(r.port_type.as_known(), Some(PortType::PORT_TYPE_VHOST_USER));
        assert_eq!(r.mtu, Some(9000));
        assert_eq!(r.subnet.as_deref(), Some("10.1.0.0/24"));
    }

    #[test]
    fn rows_show_words_and_project_names() {
        let n = Network {
            id: "6f1c".into(),
            name: "web".into(),
            bridge: "gxbr-6f1c".into(),
            mode: NetworkMode::NETWORK_MODE_NAT.into(),
            port_type: PortType::PORT_TYPE_VHOST_USER.into(),
            vlan: Some(5),
            project: Some("p1".into()),
            ..Default::default()
        };
        let host = Network {
            id: "aa".into(),
            name: "default".into(),
            mode: NetworkMode::NETWORK_MODE_BRIDGED.into(),
            port_type: PortType::PORT_TYPE_TAP.into(),
            ..Default::default()
        };
        let names = HashMap::from([("p1".to_string(), "lab".to_string())]);
        let r = rows(vec![n, host], &names);
        assert_eq!(
            r[0],
            NetworkRow {
                id: "6f1c".into(),
                name: "web".into(),
                mode: "nat".into(),
                bridge: "gxbr-6f1c".into(),
                port_type: "vhost_user".into(),
                vlan: Some(5),
                project: Some("lab".into()),
            }
        );
        assert_eq!(
            (r[1].mode.as_str(), r[1].port_type.as_str()),
            ("bridged", "tap")
        );
        assert_eq!(r[1].project, None);
        // An unknown project stays its id.
        let r = rows(
            vec![Network {
                project: Some("p9".into()),
                ..Default::default()
            }],
            &names,
        );
        assert_eq!(r[0].project.as_deref(), Some("p9"));
    }

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

    #[test]
    fn share_lines_name_the_network() {
        let sh = NetworkShare {
            network: "6f1c".into(),
            network_name: "web".into(),
            owner_project: Some("p1".into()),
            status: ShareStatus::SHARE_STATUS_ACCEPTED.into(),
            ..Default::default()
        };
        let line = plain(&share_line(&sh));
        assert!(line.starts_with("  web "), "{line}");
        assert!(line.contains("from p1 "), "{line}");
        assert!(line.ends_with("accepted"), "{line}");
        let offer = NetworkShare {
            network: "6f1c".into(),
            status: ShareStatus::SHARE_STATUS_OFFERED.into(),
            expires_at: buffa_types::google::protobuf::Timestamp {
                seconds: 86_400,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let line = plain(&share_line(&offer));
        assert!(line.starts_with("  6f1c "), "{line}");
        assert!(line.contains("from -"), "{line}");
        assert!(line.contains("offered (until "), "{line}");
    }
}
