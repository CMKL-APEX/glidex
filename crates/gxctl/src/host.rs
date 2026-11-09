//! Host networking through glidex-netd and the host's PCI devices
//! (spec/09-networking.md §8–§11) on the generated `glidex.host.v1` clients,
//! over the Connect protocol (spec/18-grpc.md §9.1): `ovs …`, `bridges`,
//! `uplinks`, `uplink-add`, `uplink-reapply`, `uplink-rm` and `pci`. These
//! act on the host gxctl is connected to.

use crate::client::{ApiClient, RpcTransport};
use crate::tenancy::{err, rpc_err, usage};
use crate::vm::enum_word;
use crate::{display_option, flag_value, format_unix_time, has_flag};
use buffa::Enumeration;
use colored::Colorize;
use glidex_api::proto::glidex::cluster::v1::{
    ClusterServiceClient, GetPciSettingsRequest, ListPciPoolsRequest, PciGrant, PciPool,
    PciPoolMatch, PciPoolSpec, PutPciSettingsRequest,
};
use glidex_api::proto::glidex::host::v1::*;
use tabled::{Table, Tabled};

pub(crate) type HostNetwork = HostNetworkServiceClient<RpcTransport>;

pub(crate) async fn rpc(client: &ApiClient) -> Result<HostNetwork, String> {
    let t = client.rpc_transport().await.map_err(|e| e.message)?;
    Ok(HostNetworkServiceClient::new(t.clone(), t.config()))
}

pub(crate) async fn pci_rpc(client: &ApiClient) -> Result<PciServiceClient<RpcTransport>, String> {
    let t = client.rpc_transport().await.map_err(|e| e.message)?;
    Ok(PciServiceClient::new(t.clone(), t.config()))
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

/// An enum from the short word gxctl takes (`best_effort` →
/// `XDP_MODE_BEST_EFFORT`); unspecified is not a word.
pub(crate) fn from_word<E: Enumeration>(word: &str, prefix: &str) -> Option<E> {
    E::from_proto_name(&format!("{}{}", prefix, word.to_ascii_uppercase()))
        .filter(|e| e.to_i32() != 0)
}

// ---- PCI devices --------------------------------------------------------------

#[derive(Debug, Tabled, PartialEq)]
pub(crate) struct PciRow {
    address: String,
    vendor_id: String,
    device_id: String,
    class_id: String,
    #[tabled(display = "display_option")]
    driver: Option<String>,
    #[tabled(display = "display_option")]
    iommu_group: Option<String>,
    numa: String,
    /// Ready for passthrough: `yes`, `no`, or `-` for a device no VM gets.
    ready: String,
}

/// `yes`, `no`, or `-`.
pub(crate) fn ready_word(ready: Option<bool>) -> String {
    match ready {
        Some(true) => "yes".into(),
        Some(false) => "no".into(),
        None => "-".into(),
    }
}

impl From<&PciDevice> for PciRow {
    fn from(d: &PciDevice) -> Self {
        PciRow {
            address: d.address.clone(),
            vendor_id: d.vendor_id.clone(),
            device_id: d.device_id.clone(),
            class_id: d.class_id.clone(),
            driver: d.driver.clone(),
            iommu_group: d.iommu_group.clone(),
            numa: d.numa_node.map_or("-".into(), |n| n.to_string()),
            ready: ready_word(d.ready),
        }
    }
}

pub(crate) async fn list_pci_devices(client: &ApiClient) -> Result<Vec<PciDevice>, String> {
    pci_rpc(client)
        .await?
        .list_pci_devices(ListPciDevicesRequest::default())
        .await
        .map(|r| r.into_owned().devices)
        .map_err(rpc_err)
}

/// The details of `pci <address>`, including the sysfs path VFIO needs.
fn pci_details(dev: &PciDevice) -> Vec<String> {
    vec![
        format!("Address:    {}", dev.address),
        format!("Vendor:     {}", dev.vendor_id),
        format!("Device:     {}", dev.device_id),
        format!("Class:      {}", dev.class_id),
        format!("Driver:     {}", display_option(&dev.driver)),
        format!("IOMMU:      {}", display_option(&dev.iommu_group)),
        format!("Sysfs path: {}", dev.sysfs_path),
        format!(
            "NUMA node:  {}",
            dev.numa_node.map_or("-".into(), |n| n.to_string())
        ),
        format!(
            "Ready:      {}{}",
            ready_word(dev.ready),
            if dev.not_ready.is_empty() {
                String::new()
            } else {
                format!(" ({})", dev.not_ready.join("; "))
            }
        ),
    ]
}

/// `pci pools`: each pool with its devices per node, holders and what is
/// free (spec/22-host-topology.md §9.1).
async fn pci_pools(client: &ApiClient, project: Option<&str>) {
    let s = match crate::cluster::node_rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let req = ListPciPoolsRequest {
        project: project.map(String::from),
        ..Default::default()
    };
    let r = match s.list_pci_pools(req).await {
        Ok(r) => r.into_owned(),
        Err(e) => return err(rpc_err(e)),
    };
    if r.pools.is_empty() {
        let none = match project {
            Some(p) => format!("No device pools project {p} may use"),
            None => "No device pools (gxctl pci settings)".into(),
        };
        println!("{}", none.yellow());
    }
    for p in &r.pools {
        println!(
            "{} {}{}{}: {} free",
            "pool".bold(),
            p.name,
            p.class
                .as_deref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default(),
            if p.discovered == Some(true) {
                " [discovered: host roles only until adopted]"
            } else {
                ""
            },
            p.free
        );
        for n in &p.nodes {
            println!("  {} ({} free)", n.node_name, n.free);
            for d in &n.devices {
                let state = match (&d.holder, d.cordoned, d.ready) {
                    (Some(vm), _, _) => format!("held by {vm}"),
                    (None, true, _) => "cordoned".into(),
                    (None, false, false) => "not ready".into(),
                    (None, false, true) => "free".green().to_string(),
                };
                println!(
                    "    {}{}  {}",
                    d.address,
                    d.numa_node
                        .map(|n| format!("  numa {n}"))
                        .unwrap_or_default(),
                    state
                );
            }
        }
    }
}

/// `pci settings [set <file.json>]`: the cluster's device pools and grants
/// (spec/22-host-topology.md H20), shown or replaced from a file holding
/// `{"pools": [...], "allow": [...]}` as `control-plane.json`'s `pci` has them.
async fn pci_settings(client: &ApiClient, args: &[&str]) {
    let s = match crate::cluster::rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let shown = match args {
        [] => s
            .get_pci_settings(GetPciSettingsRequest::default())
            .await
            .map(|r| serde_json::to_value(r.into_owned())),
        ["set", path] => {
            let req: PutPciSettingsRequest = match std::fs::read(path)
                .map_err(|e| e.to_string())
                .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
            {
                Ok(r) => r,
                Err(e) => return err(format!("{path}: {e}")),
            };
            s.put_pci_settings(req)
                .await
                .map(|r| serde_json::to_value(r.into_owned()))
        }
        _ => return err("usage: pci settings [set <file.json>]".to_string()),
    };
    match shown {
        Ok(Ok(v)) => println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default()),
        Ok(Err(e)) => err(e.to_string()),
        Err(e) => err(rpc_err(e)),
    }
}

// ---- `pci pool …` and `pci grant …`: edits of the cluster's setting --------

/// The setting's pools and grants as a PUT that changes nothing yet; the
/// edits below change it and it is written back whole (spec/22-host-topology.md
/// H20). Before a setting is written this is the server's file, which the
/// first write then moves to the setting.
async fn current_setting(
    s: &ClusterServiceClient<RpcTransport>,
) -> Result<PutPciSettingsRequest, String> {
    let r = s
        .get_pci_settings(GetPciSettingsRequest::default())
        .await
        .map_err(rpc_err)?
        .into_owned();
    let st = r.settings.as_option().cloned().unwrap_or_default();
    Ok(PutPciSettingsRequest {
        pools: st.pools,
        allow: st.allow,
        ..Default::default()
    })
}

/// The pools listed now, discovered ones among them.
async fn listed_pools(client: &ApiClient) -> Result<Vec<PciPool>, String> {
    let nodes = crate::cluster::node_rpc(client).await?;
    Ok(nodes
        .list_pci_pools(ListPciPoolsRequest::default())
        .await
        .map_err(rpc_err)?
        .into_owned()
        .pools)
}

/// A defined pool named `name` matching discovered pool `found`'s vendor
/// and device id (§9.1b).
fn adopted(found: &PciPool, name: &str) -> Result<PciPoolSpec, String> {
    let Some((vendor_id, device_id)) = glidex_topology::pci::pool_slug_ids(&found.name) else {
        return Err(format!("{} is not a discovered pool's name", found.name));
    };
    Ok(PciPoolSpec {
        name: name.to_string(),
        class: found.class.clone(),
        r#match: PciPoolMatch {
            vendor_id: Some(vendor_id),
            device_id: Some(device_id),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    })
}

/// `pci pool set`: `spec` replaces the pool of its name, or is added.
fn set_pool(e: &mut PutPciSettingsRequest, spec: PciPoolSpec) {
    match e.pools.iter_mut().find(|p| p.name == spec.name) {
        Some(p) => *p = spec,
        None => e.pools.push(spec),
    }
}

/// `pci pool rm`: the pool and the grants naming it. The server refuses
/// it while VMs or tenant pools use the pool.
fn remove_pool(e: &mut PutPciSettingsRequest, name: &str) -> Result<(), String> {
    let before = e.pools.len();
    e.pools.retain(|p| p.name != name);
    if e.pools.len() == before {
        return Err(format!("no pool {name} in the setting (see 'pci pools')"));
    }
    e.allow.retain(|g| g.pool.as_deref() != Some(name));
    Ok(())
}

/// What `pci grant` edits: a pool's grant, or a device's (by address, on
/// one node or on every node).
#[derive(Debug, Clone, Copy, PartialEq)]
enum GrantTarget<'a> {
    Pool(&'a str),
    Device(&'a str, Option<&'a str>),
}

impl<'a> GrantTarget<'a> {
    /// A PCI address (`0000:41:00.0`) is a device; anything else a pool.
    fn of(target: &'a str, node: Option<&'a str>) -> Self {
        if target.contains(':') {
            GrantTarget::Device(target, node)
        } else {
            GrantTarget::Pool(target)
        }
    }

    fn is(&self, g: &PciGrant) -> bool {
        match *self {
            GrantTarget::Pool(p) => g.bdf.is_none() && g.pool.as_deref() == Some(p),
            GrantTarget::Device(b, n) => g.bdf.as_deref() == Some(b) && g.node.as_deref() == n,
        }
    }
}

/// `pci grant`: `projects` and `tenants` may use the target, added to its
/// grant.
fn grant(e: &mut PutPciSettingsRequest, t: GrantTarget, projects: &[String], tenants: &[String]) {
    let g = match e.allow.iter().position(|g| t.is(g)) {
        Some(i) => &mut e.allow[i],
        None => {
            e.allow.push(match t {
                GrantTarget::Pool(p) => PciGrant {
                    pool: Some(p.to_string()),
                    ..Default::default()
                },
                GrantTarget::Device(b, n) => PciGrant {
                    bdf: Some(b.to_string()),
                    node: n.map(String::from),
                    ..Default::default()
                },
            });
            e.allow.last_mut().expect("just pushed")
        }
    };
    for (have, add) in [(&mut g.projects, projects), (&mut g.tenants, tenants)] {
        for a in add {
            if !have.contains(a) {
                have.push(a.clone());
            }
        }
    }
}

/// `pci grant rm`: the names taken off the target's grants, or its grants
/// whole when none are named. A grant left to no one goes.
fn revoke(
    e: &mut PutPciSettingsRequest,
    t: GrantTarget,
    projects: &[String],
    tenants: &[String],
) -> Result<(), String> {
    if !e.allow.iter().any(|g| t.is(g)) {
        return Err("no such grant (see 'pci grant')".to_string());
    }
    let whole = projects.is_empty() && tenants.is_empty();
    e.allow.retain_mut(|g| {
        if !t.is(g) {
            return true;
        }
        if whole {
            return false;
        }
        g.projects.retain(|p| !projects.contains(p));
        g.tenants.retain(|x| !tenants.contains(x));
        !g.projects.is_empty() || !g.tenants.is_empty()
    });
    Ok(())
}

/// One grant as `pci grant` lists it.
fn grant_line(g: &PciGrant) -> String {
    let target = match (&g.pool, &g.bdf, &g.node) {
        (Some(p), _, _) => format!("pool {p}"),
        (None, Some(b), Some(n)) => format!("device {b} on {n}"),
        (None, Some(b), None) => format!("device {b}"),
        (None, None, _) => "?".to_string(),
    };
    let mut to = Vec::new();
    if !g.projects.is_empty() {
        to.push(format!("projects {}", g.projects.join(", ")));
    }
    if !g.tenants.is_empty() {
        to.push(format!("tenants {}", g.tenants.join(", ")));
    }
    format!("{target}: {}", to.join("; "))
}

/// A `--projects a,b` list.
fn names(args: &[&str], flag: &str) -> Vec<String> {
    flag_value(args, flag)
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

const POOL_USAGE: &str = "usage: pci pool list | show <pool> | set <pool.json> | rm <pool> \
                          | adopt <discovered pool> [--name N]";
const GRANT_USAGE: &str = "usage: pci grant [<pool|address> [--node N] [--projects a,b] \
                           [--tenants t,u]] | pci grant rm <pool|address> [--node N] \
                           [--projects a,b] [--tenants t,u]";

/// `pci pool list|show|set|rm|adopt`: the cluster's pools, one at a time
/// (spec/22-host-topology.md §9.1, §9.1b).
async fn pci_pool(client: &ApiClient, args: &[&str]) {
    if matches!(args, ["list"] | ["ls"]) {
        return pci_pools(client, None).await;
    }
    let s = svc!(crate::cluster::rpc(client));
    let mut e = match current_setting(&s).await {
        Ok(e) => e,
        Err(e) => return err(e),
    };
    let done = match args {
        ["show", name] => {
            let Some(p) = e.pools.iter().find(|p| p.name == *name) else {
                return err(format!(
                    "no pool {name} in the setting (a discovered pool is shown by 'pci pools')"
                ));
            };
            println!("{}", serde_json::to_string_pretty(p).unwrap_or_default());
            for g in e.allow.iter().filter(|g| g.pool.as_deref() == Some(name)) {
                println!("{} {}", "grant".bold(), grant_line(g));
            }
            return;
        }
        ["set", path] => {
            let spec: PciPoolSpec = match std::fs::read(path)
                .map_err(|e| e.to_string())
                .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
            {
                Ok(p) => p,
                Err(e) => return err(format!("{path}: {e}")),
            };
            let name = spec.name.clone();
            set_pool(&mut e, spec);
            format!("pool {name} set")
        }
        ["rm", name] => {
            if let Err(m) = remove_pool(&mut e, name) {
                return err(m);
            }
            format!("pool {name} removed, with its grants")
        }
        ["adopt", slug, ..] => {
            let listed = match listed_pools(client).await {
                Ok(l) => l,
                Err(e) => return err(e),
            };
            let Some(found) = listed
                .iter()
                .find(|p| p.name == *slug && p.discovered == Some(true))
            else {
                return err(format!("no discovered pool {slug} (see 'pci pools')"));
            };
            let name = flag_value(args, "--name").unwrap_or(slug);
            match adopted(found, name) {
                Ok(spec) => e.pools.push(spec),
                Err(m) => return err(m),
            }
            format!("pool {name} adopted from {slug}")
        }
        _ => return usage(POOL_USAGE),
    };
    match s.put_pci_settings(e).await {
        Ok(_) => println!("{} {done}", "OK".green()),
        Err(e) => err(rpc_err(e)),
    }
}

/// `pci grant [rm] …`: who may use a pool or a device (§9.1). Granting a
/// discovered pool adopts it under its name first (§9.1b).
async fn pci_grant(client: &ApiClient, args: &[&str]) {
    let s = svc!(crate::cluster::rpc(client));
    let mut e = match current_setting(&s).await {
        Ok(e) => e,
        Err(e) => return err(e),
    };
    let (rm, rest) = match args {
        [] => {
            if e.allow.is_empty() {
                println!("{}", "No grants".yellow());
            }
            for g in &e.allow {
                println!("{}", grant_line(g));
            }
            return;
        }
        ["rm", rest @ ..] => (true, rest),
        rest => (false, rest),
    };
    let Some(target) = rest.first().filter(|t| !t.starts_with("--")) else {
        return usage(GRANT_USAGE);
    };
    let t = GrantTarget::of(target, flag_value(rest, "--node"));
    let (projects, tenants) = (names(rest, "--projects"), names(rest, "--tenants"));
    let done = if rm {
        if let Err(m) = revoke(&mut e, t, &projects, &tenants) {
            return err(m);
        }
        format!("grant on {target} changed")
    } else {
        if projects.is_empty() && tenants.is_empty() {
            return usage(GRANT_USAGE);
        }
        if let GrantTarget::Pool(p) = t
            && !e.pools.iter().any(|d| d.name == p)
        {
            let listed = match listed_pools(client).await {
                Ok(l) => l,
                Err(e) => return err(e),
            };
            match listed
                .iter()
                .find(|d| d.name == p && d.discovered == Some(true))
            {
                Some(found) => match adopted(found, p) {
                    Ok(spec) => e.pools.push(spec),
                    Err(m) => return err(m),
                },
                None => return err(format!("no pool {p} (see 'pci pools')")),
            }
        }
        grant(&mut e, t, &projects, &tenants);
        format!("{target} granted")
    };
    match s.put_pci_settings(e).await {
        Ok(_) => println!("{} {done}", "OK".green()),
        Err(e) => err(rpc_err(e)),
    }
}

/// `pci`, `pci <address>`, `pci pools [--project P]`, `pci settings …`,
/// `pci pool …`, `pci grant …`
pub async fn pci(client: &ApiClient, args: &[&str]) {
    if args.first() == Some(&"pools") {
        return pci_pools(client, flag_value(args, "--project")).await;
    }
    if args.first() == Some(&"settings") {
        return pci_settings(client, &args[1..]).await;
    }
    if args.first() == Some(&"pool") {
        return pci_pool(client, &args[1..]).await;
    }
    if args.first() == Some(&"grant") {
        return pci_grant(client, &args[1..]).await;
    }
    let devices = match list_pci_devices(client).await {
        Ok(d) => d,
        Err(e) => return err(e),
    };
    if devices.is_empty() {
        println!("{}", "No PCI devices found".yellow());
    } else if let Some(addr) = args.first() {
        match devices.iter().find(|d| d.address == *addr) {
            Some(dev) => {
                for l in pci_details(dev) {
                    println!("{}", l);
                }
            }
            None => println!("{} no PCI device with address {}", "Error:".red(), addr),
        }
    } else {
        let rows: Vec<PciRow> = devices.iter().map(PciRow::from).collect();
        println!("{}", Table::new(rows));
    }
}

// ---- `ovs …` ------------------------------------------------------------------

/// What `ovs status` prints.
fn status_lines(s: &GetOvsStatusResponse) -> Vec<String> {
    let netd = s.netd.as_option().cloned().unwrap_or_default();
    if netd.available != Some(true) {
        return vec![format!(
            "{} glidex-netd unavailable: {}",
            "netd:".bold(),
            netd.error.as_deref().unwrap_or("unknown").red()
        )];
    }
    let access = enum_word(&netd.access, "NETD_ACCESS_");
    let mut out = vec![format!(
        "{} available (access: {})",
        "netd:".bold(),
        if access.is_empty() { "?" } else { &access }
    )];
    let h = s.host.as_option().cloned().unwrap_or_default();
    let ovs = if h.ovs_installed == Some(true) {
        format!(
            "{} ({})",
            h.ovs_version.as_deref().unwrap_or("?"),
            if h.ovs_running == Some(true) {
                "running".green()
            } else {
                "not running".red()
            }
        )
    } else {
        "not installed".red().to_string()
    };
    out.push(format!("{} {}", "Open vSwitch:".bold(), ovs));
    out.push(format!(
        "{} {}",
        "dnsmasq:".bold(),
        if h.dnsmasq == Some(true) {
            "yes".green()
        } else {
            "missing".red()
        }
    ));
    out.push(format!(
        "{} {}",
        "DPDK initialized:".bold(),
        h.dpdk_initialized.unwrap_or(false)
    ));
    if !h.combinations.is_empty() {
        out.push(format!("{}", "Combinations:".bold()));
        for c in &h.combinations {
            out.push(format!(
                "  {} {} {}",
                c.id,
                if c.available == Some(true) {
                    "available".green().to_string()
                } else {
                    format!("missing: {}", c.missing.join(", "))
                        .yellow()
                        .to_string()
                },
                c.description.dimmed()
            ));
        }
    }
    out
}

/// The body of `ovs install [--profile kernel|dpdk] [--source] [--force]`.
fn install_request(args: &[&str]) -> Result<InstallOvsRequest, String> {
    let word = flag_value(args, "--profile").unwrap_or("kernel");
    let profile: OvsProfile = from_word(word, "OVS_PROFILE_")
        .ok_or_else(|| format!("unknown --profile {}: use kernel or dpdk", word))?;
    Ok(InstallOvsRequest {
        profile: profile.into(),
        source_build: has_flag(args, "--source"),
        confirm: has_flag(args, "--force"),
        ..Default::default()
    })
}

/// The body of `ovs dpdk-init [--socket-mem M] [--pmd-cpu-mask X] [--force]`.
fn dpdk_request(args: &[&str]) -> InitDpdkRequest {
    InitDpdkRequest {
        socket_mem: flag_value(args, "--socket-mem")
            .unwrap_or("1024")
            .to_string(),
        pmd_cpu_mask: flag_value(args, "--pmd-cpu-mask").map(str::to_string),
        confirm: has_flag(args, "--force"),
        ..Default::default()
    }
}

/// `ovs status | install | dpdk-init`
pub async fn ovs(client: &ApiClient, args: &[&str]) {
    match args.first().copied() {
        Some("status") | None => {
            match svc!(rpc(client))
                .get_ovs_status(GetOvsStatusRequest::default())
                .await
            {
                Ok(r) => {
                    for l in status_lines(&r.into_owned()) {
                        println!("{}", l);
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        Some("install") => {
            let req = match install_request(args) {
                Ok(r) => r,
                Err(e) => return err(e),
            };
            println!(
                "Installing Open vSwitch ({} profile); this can take a few minutes...",
                enum_word(&req.profile, "OVS_PROFILE_")
            );
            match svc!(rpc(client)).install_ovs(req).await {
                Ok(r) => {
                    let r = r.into_owned();
                    let version = r.ovs_version.as_deref().unwrap_or("?");
                    if r.changed == Some(true) {
                        println!("{} Open vSwitch {}", "Installed:".green(), version)
                    } else {
                        println!(
                            "{} Open vSwitch {} already satisfies the profile",
                            "Nothing to do:".green(),
                            version
                        )
                    }
                }
                Err(e) => err(rpc_err(e)),
            }
        }
        Some("dpdk-init") => {
            let req = dpdk_request(args);
            println!("Enabling DPDK in ovs-vswitchd (restarts it)...");
            match svc!(rpc(client)).init_dpdk(req).await {
                Ok(_) => println!("{} DPDK initialized", "OK:".green()),
                Err(e) => err(rpc_err(e)),
            }
        }
        Some(other) => println!(
            "{} unknown 'ovs {}'; use 'ovs status', 'ovs install' or 'ovs dpdk-init'",
            "Error:".red(),
            other
        ),
    }
}

// ---- bridges ------------------------------------------------------------------

/// One line of `bridges`.
fn bridge_line(b: &Bridge) -> String {
    let spec = b.spec.as_option().cloned().unwrap_or_default();
    let datapath = enum_word(&spec.datapath, "DATAPATH_");
    format!(
        "  {} ({}) {}",
        spec.name.cyan(),
        if datapath.is_empty() { "?" } else { &datapath },
        match b.live.as_option() {
            None => "MISSING on host".red().to_string(),
            Some(live) => format!("ports: {}", live.ports.len()),
        }
    )
}

/// `bridges`
pub async fn bridges(client: &ApiClient) {
    match svc!(rpc(client))
        .list_bridges(ListBridgesRequest::default())
        .await
    {
        Ok(r) => {
            let bridges = r.into_owned().bridges;
            if bridges.is_empty() {
                println!("No glidex bridges.");
            }
            for b in &bridges {
                println!("{}", bridge_line(b));
            }
        }
        Err(e) => err(rpc_err(e)),
    }
}

// ---- uplinks ------------------------------------------------------------------

fn uplink_name(u: &Uplink) -> String {
    u.record
        .as_option()
        .and_then(|r| r.spec.as_option())
        .map(|s| s.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "?".to_string())
}

/// One line of `uplinks <bridge>`.
fn uplink_line(u: &Uplink) -> String {
    let spec = u
        .record
        .as_option()
        .and_then(|r| r.spec.as_option())
        .cloned()
        .unwrap_or_default();
    let or_q = |s: String| if s.is_empty() { "?".to_string() } else { s };
    let live = u.live.as_option().cloned().unwrap_or_default();
    format!(
        "  {} ({}) {} {}",
        uplink_name(u).cyan(),
        or_q(enum_word(&spec.kind, "UPLINK_KIND_")),
        or_q(enum_word(&u.phase, "UPLINK_PHASE_")),
        match live.error {
            Some(e) => e.red().to_string(),
            None => live.link_state.unwrap_or_default(),
        }
    )
}

/// `uplinks <bridge>`
pub async fn uplinks(client: &ApiClient, args: &[&str]) {
    let Some(bridge) = args.first() else {
        return usage("Usage: uplinks <bridge>");
    };
    match svc!(rpc(client))
        .list_uplinks(ListUplinksRequest {
            bridge: bridge.to_string(),
            ..Default::default()
        })
        .await
    {
        Ok(r) => {
            let list = r.into_owned().uplinks;
            if list.is_empty() {
                println!("No uplinks on {}.", bridge);
            }
            for u in &list {
                println!("{}", uplink_line(u));
            }
        }
        Err(e) => err(rpc_err(e)),
    }
}

const UPLINK_ADD_USAGE: &str = "Usage: uplink-add <bridge> (--kernel <if> | --afxdp <if> [--xdp-mode best_effort|native|native_with_zerocopy|generic] [--rxq N] | --dpdk <bdf> --name <port> [--rxq N]) [--migrate-ip] [--force] [--accept-dhcp]";

/// The body of `uplink-add <bridge> …`; `Err` with what to print.
fn uplink_request(args: &[&str]) -> Result<EnsureUplinkRequest, String> {
    let Some(bridge) = args.first().filter(|a| !a.starts_with("--")) else {
        return Err(UPLINK_ADD_USAGE.into());
    };
    let rxq: u32 = flag_value(args, "--rxq")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(1)
        .into();
    let mut req = EnsureUplinkRequest {
        bridge: bridge.to_string(),
        migrate_ip: has_flag(args, "--migrate-ip"),
        confirm: has_flag(args, "--force"),
        accept_dhcp: has_flag(args, "--accept-dhcp"),
        ..Default::default()
    };
    if let Some(ifname) = flag_value(args, "--kernel") {
        req.name = ifname.to_string();
        req.kind = UplinkKind::UPLINK_KIND_KERNEL.into();
        req.ifname = Some(ifname.to_string());
    } else if let Some(ifname) = flag_value(args, "--afxdp") {
        let word = flag_value(args, "--xdp-mode").unwrap_or("best_effort");
        let mode: XdpMode = from_word(word, "XDP_MODE_").ok_or_else(|| {
            format!(
                "unknown --xdp-mode {}: use best_effort, native, native_with_zerocopy or generic",
                word
            )
        })?;
        req.name = ifname.to_string();
        req.kind = UplinkKind::UPLINK_KIND_AFXDP.into();
        req.ifname = Some(ifname.to_string());
        req.xdp_mode = mode.into();
        req.n_rxq = Some(rxq);
    } else if let Some(pci) = flag_value(args, "--dpdk") {
        let Some(name) = flag_value(args, "--name") else {
            return Err(UPLINK_ADD_USAGE.into());
        };
        req.name = name.to_string();
        req.kind = UplinkKind::UPLINK_KIND_DPDK.into();
        req.pci = Some(pci.to_string());
        req.n_rxq = Some(rxq);
    } else {
        return Err(UPLINK_ADD_USAGE.into());
    }
    Ok(req)
}

/// `uplink-add <bridge> …`
pub async fn uplink_add(client: &ApiClient, args: &[&str]) {
    let req = match uplink_request(args) {
        Ok(r) => r,
        Err(u) if u == UPLINK_ADD_USAGE => return usage(&u),
        Err(e) => return err(e),
    };
    let bridge = req.bridge.clone();
    let u = match svc!(rpc(client)).ensure_uplink(req).await {
        Ok(r) => r
            .into_owned()
            .uplink
            .as_option()
            .cloned()
            .unwrap_or_default(),
        Err(e) => {
            let e = rpc_err(e);
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
                err(e);
            }
            return;
        }
    };
    finish_uplink(client, &bridge, &u).await;
}

/// A migration waiting for its commit: the token, and when netd rolls it
/// back (RFC 3339 on the wire, shown as gxctl shows times).
fn pending_commit(u: &Uplink) -> Option<(String, String)> {
    if u.phase.as_known() != Some(UplinkPhase::UPLINK_PHASE_PENDING_COMMIT) {
        return None;
    }
    let p = u
        .record
        .as_option()
        .and_then(|r| r.pending.as_option())
        .cloned()
        .unwrap_or_default();
    let deadline = p
        .deadline
        .as_option()
        .map(|t| format_unix_time(t.seconds.max(0) as u64))
        .unwrap_or_else(|| "?".into());
    Some((p.token, deadline))
}

/// After an uplink was added or re-applied: commit a moved IP once the API
/// still answers (netd rolls it back otherwise).
async fn finish_uplink(client: &ApiClient, bridge: &str, u: &Uplink) {
    let name = uplink_name(u);
    for w in u
        .live
        .as_option()
        .map(|l| l.warnings.as_slice())
        .unwrap_or_default()
    {
        println!("{} {}", "Warning:".yellow(), w);
    }
    let Some((token, deadline)) = pending_commit(u) else {
        return println!("{} uplink {} on {}", "Added:".green(), name, bridge);
    };
    println!(
        "IP configuration moved to {} (rolled back at {} unless committed); checking the API is still reachable...",
        bridge, deadline
    );
    if client.health_check().await.is_err() {
        println!(
            "{} API unreachable; glidex-netd will roll the change back automatically.",
            "Error:".red()
        );
        return;
    }
    match svc!(rpc(client))
        .commit_uplink(CommitUplinkRequest {
            bridge: bridge.to_string(),
            name: name.clone(),
            token,
            ..Default::default()
        })
        .await
    {
        Ok(_) => println!(
            "{} uplink {} on {} (IP migrated)",
            "Committed:".green(),
            name,
            bridge
        ),
        Err(e) => println!(
            "{} {} (the change will be rolled back)",
            "Error:".red(),
            rpc_err(e)
        ),
    }
}

/// `uplink-reapply <bridge> <name>`: a migration netd rolled back at start
/// (spec/16-clustering-ui.md), set up again with the gateway check and commit.
pub async fn uplink_reapply(client: &ApiClient, args: &[&str]) {
    let (Some(bridge), Some(name)) = (args.first(), args.get(1)) else {
        return usage("Usage: uplink-reapply <bridge> <name>");
    };
    match svc!(rpc(client))
        .reapply_uplink(ReapplyUplinkRequest {
            bridge: bridge.to_string(),
            name: name.to_string(),
            ..Default::default()
        })
        .await
    {
        Ok(r) => {
            let u = r
                .into_owned()
                .uplink
                .as_option()
                .cloned()
                .unwrap_or_default();
            finish_uplink(client, bridge, &u).await
        }
        Err(e) => err(rpc_err(e)),
    }
}

/// `uplink-rm <bridge> <name>`
pub async fn uplink_rm(client: &ApiClient, args: &[&str]) {
    let (Some(bridge), Some(name)) = (args.first(), args.get(1)) else {
        return usage("Usage: uplink-rm <bridge> <name>");
    };
    match svc!(rpc(client))
        .delete_uplink(DeleteUplinkRequest {
            bridge: bridge.to_string(),
            name: name.to_string(),
            ..Default::default()
        })
        .await
    {
        Ok(_) => println!("{} uplink {} (NIC restored)", "Removed:".green(), name),
        Err(e) => err(rpc_err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffa_types::google::protobuf::Timestamp;

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
    fn uplink_flags_map_to_the_request() {
        let r = uplink_request(&["br0", "--kernel", "eth1", "--migrate-ip", "--force"]).unwrap();
        assert_eq!((r.bridge.as_str(), r.name.as_str()), ("br0", "eth1"));
        assert_eq!(r.kind.as_known(), Some(UplinkKind::UPLINK_KIND_KERNEL));
        assert_eq!(r.ifname.as_deref(), Some("eth1"));
        assert_eq!(
            (r.migrate_ip, r.confirm, r.accept_dhcp),
            (true, true, false)
        );
        assert_eq!(r.n_rxq, None);
        assert_eq!(r.xdp_mode.as_known(), Some(XdpMode::XDP_MODE_UNSPECIFIED));

        let r = uplink_request(&["br0", "--afxdp", "eth2", "--rxq", "4", "--accept-dhcp"]).unwrap();
        assert_eq!(r.kind.as_known(), Some(UplinkKind::UPLINK_KIND_AFXDP));
        assert_eq!(r.xdp_mode.as_known(), Some(XdpMode::XDP_MODE_BEST_EFFORT));
        assert_eq!(r.n_rxq, Some(4));
        assert!(r.accept_dhcp);
        let r = uplink_request(&[
            "br0",
            "--afxdp",
            "eth2",
            "--xdp-mode",
            "native_with_zerocopy",
        ])
        .unwrap();
        assert_eq!(
            r.xdp_mode.as_known(),
            Some(XdpMode::XDP_MODE_NATIVE_WITH_ZEROCOPY)
        );
        assert_eq!(r.n_rxq, Some(1));
        let e = uplink_request(&["br0", "--afxdp", "eth2", "--xdp-mode", "turbo"]).unwrap_err();
        assert!(e.contains("unknown --xdp-mode turbo"), "{e}");
        let e = uplink_request(&["br0", "--afxdp", "eth2", "--xdp-mode", "unspecified"]);
        assert!(e.is_err());

        let r = uplink_request(&["br0", "--dpdk", "0000:3b:00.0", "--name", "dpdk0"]).unwrap();
        assert_eq!(r.kind.as_known(), Some(UplinkKind::UPLINK_KIND_DPDK));
        assert_eq!(r.name, "dpdk0");
        assert_eq!(r.pci.as_deref(), Some("0000:3b:00.0"));
        assert_eq!(r.ifname, None);

        for bad in [
            &["br0", "--dpdk", "0000:3b:00.0"][..],
            &["br0"][..],
            &["--kernel", "eth1"][..],
            &[][..],
        ] {
            assert_eq!(uplink_request(bad).unwrap_err(), UPLINK_ADD_USAGE);
        }
    }

    #[test]
    fn install_and_dpdk_flags_map_to_the_request() {
        let r = install_request(&["install"]).unwrap();
        assert_eq!(r.profile.as_known(), Some(OvsProfile::OVS_PROFILE_KERNEL));
        assert!(!r.source_build && !r.confirm);
        let r = install_request(&["install", "--profile", "dpdk", "--source", "--force"]).unwrap();
        assert_eq!(r.profile.as_known(), Some(OvsProfile::OVS_PROFILE_DPDK));
        assert!(r.source_build && r.confirm);
        assert!(install_request(&["install", "--profile", "turbo"]).is_err());

        let r = dpdk_request(&["dpdk-init"]);
        assert_eq!(r.socket_mem, "1024");
        assert_eq!(r.pmd_cpu_mask, None);
        assert!(!r.confirm);
        let r = dpdk_request(&[
            "dpdk-init",
            "--socket-mem",
            "1024,1024",
            "--pmd-cpu-mask",
            "0x6",
            "--force",
        ]);
        assert_eq!(r.socket_mem, "1024,1024");
        assert_eq!(r.pmd_cpu_mask.as_deref(), Some("0x6"));
        assert!(r.confirm);
    }

    #[test]
    fn status_shows_netd_and_the_host() {
        let down = GetOvsStatusResponse {
            netd: NetdStatus {
                available: Some(false),
                access: NetdAccess::NETD_ACCESS_NONE.into(),
                error: Some("no socket".into()),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let l: Vec<String> = status_lines(&down).iter().map(|l| plain(l)).collect();
        assert_eq!(l, ["netd: glidex-netd unavailable: no socket"]);

        let up = GetOvsStatusResponse {
            netd: NetdStatus {
                available: Some(true),
                access: NetdAccess::NETD_ACCESS_FULL.into(),
                ..Default::default()
            }
            .into(),
            host: HostCapabilities {
                ovs_installed: Some(true),
                ovs_version: Some("3.3.0".into()),
                ovs_running: Some(true),
                dnsmasq: Some(true),
                combinations: vec![
                    Combination {
                        id: "kernel+tap".into(),
                        description: "kernel datapath".into(),
                        available: Some(true),
                        ..Default::default()
                    },
                    Combination {
                        id: "dpdk+vhost".into(),
                        description: "userspace".into(),
                        missing: vec!["hugepages".into(), "dpdk".into()],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let l: Vec<String> = status_lines(&up).iter().map(|l| plain(l)).collect();
        assert_eq!(
            l,
            [
                "netd: available (access: full)",
                "Open vSwitch: 3.3.0 (running)",
                "dnsmasq: yes",
                "DPDK initialized: false",
                "Combinations:",
                "  kernel+tap available kernel datapath",
                "  dpdk+vhost missing: hugepages, dpdk userspace",
            ]
        );
        // netd answered but said nothing about the host.
        let bare = GetOvsStatusResponse {
            netd: NetdStatus {
                available: Some(true),
                access: NetdAccess::NETD_ACCESS_STATUS.into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let l: Vec<String> = status_lines(&bare).iter().map(|l| plain(l)).collect();
        assert_eq!(l[0], "netd: available (access: status)");
        assert_eq!(l[1], "Open vSwitch: not installed");
        assert_eq!(l[2], "dnsmasq: missing");
    }

    #[test]
    fn bridge_and_uplink_lines_use_the_short_words() {
        let b = Bridge {
            spec: BridgeSpec {
                name: "gxbr-6f1c".into(),
                datapath: Datapath::DATAPATH_SYSTEM.into(),
                ..Default::default()
            }
            .into(),
            live: BridgeInfo {
                ports: vec!["eth1".into(), "tap0".into()],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        assert_eq!(plain(&bridge_line(&b)), "  gxbr-6f1c (system) ports: 2");
        let missing = Bridge {
            spec: BridgeSpec {
                name: "br1".into(),
                datapath: Datapath::DATAPATH_NETDEV.into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        assert_eq!(
            plain(&bridge_line(&missing)),
            "  br1 (netdev) MISSING on host"
        );

        let u = Uplink {
            record: UplinkRecord {
                spec: UplinkSpec {
                    name: "eth1".into(),
                    kind: UplinkKind::UPLINK_KIND_KERNEL.into(),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            }
            .into(),
            phase: UplinkPhase::UPLINK_PHASE_ACTIVE.into(),
            live: UplinkState {
                link_state: Some("up".into()),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        assert_eq!(plain(&uplink_line(&u)), "  eth1 (kernel) active up");
        assert_eq!(pending_commit(&u), None);
        let mut bad = u.clone();
        bad.phase = UplinkPhase::UPLINK_PHASE_PENDING_COMMIT.into();
        bad.live = UplinkState {
            error: Some("link down".into()),
            link_state: Some("down".into()),
            ..Default::default()
        }
        .into();
        assert_eq!(
            plain(&uplink_line(&bad)),
            "  eth1 (kernel) pending_commit link down"
        );
        assert_eq!(plain(&uplink_line(&Uplink::default())), "  ? (?) ? ");
    }

    #[test]
    fn a_pending_uplink_gives_its_token_and_deadline() {
        let u = Uplink {
            record: UplinkRecord {
                spec: UplinkSpec {
                    name: "eth1".into(),
                    ..Default::default()
                }
                .into(),
                pending: PendingCommit {
                    token: "t-1".into(),
                    deadline: Timestamp::from_unix_secs(86_400 + 3_600).into(),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            }
            .into(),
            phase: UplinkPhase::UPLINK_PHASE_PENDING_COMMIT.into(),
            ..Default::default()
        };
        let (token, deadline) = pending_commit(&u).unwrap();
        assert_eq!(token, "t-1");
        assert_eq!(deadline, format_unix_time(90_000));
        assert_eq!(uplink_name(&u), "eth1");
        // The RFC 3339 deadline on the wire.
        let json: Uplink = serde_json::from_str(
            r#"{"record": {"spec": {"name": "eth1", "kind": "UPLINK_KIND_KERNEL"},
                "pending": {"token": "t-2", "deadline": "1970-01-02T01:00:00Z"}},
                "phase": "UPLINK_PHASE_PENDING_COMMIT"}"#,
        )
        .unwrap();
        assert_eq!(
            pending_commit(&json),
            Some(("t-2".to_string(), format_unix_time(90_000)))
        );
    }

    #[test]
    fn pci_rows_and_details() {
        let d = PciDevice {
            address: "0000:00:1f.0".into(),
            vendor_id: "8086".into(),
            device_id: "9d4e".into(),
            class_id: "060100".into(),
            driver: Some("lpc_ich".into()),
            iommu_group: None,
            sysfs_path: "/sys/bus/pci/devices/0000:00:1f.0".into(),
            ..Default::default()
        };
        let row = PciRow::from(&d);
        assert_eq!(row.driver.as_deref(), Some("lpc_ich"));
        let table = Table::new([row]).to_string();
        assert!(table.contains("iommu_group"), "{table}");
        assert!(!table.contains("sysfs"), "{table}");
        let l = pci_details(&d);
        assert_eq!(l[4], "Driver:     lpc_ich");
        assert_eq!(l[5], "IOMMU:      -");
        assert_eq!(l[6], "Sysfs path: /sys/bus/pci/devices/0000:00:1f.0");
        // The proto JSON (the REST twin's answer).
        let j: ListPciDevicesResponse = serde_json::from_str(
            r#"{"devices": [{"address": "0000:00:1f.0", "vendor_id": "8086",
                "device_id": "9d4e", "class_id": "060100",
                "sysfs_path": "/sys/bus/pci/devices/0000:00:1f.0"}]}"#,
        )
        .unwrap();
        assert_eq!(j.devices[0].sysfs_path, d.sysfs_path);
        assert_eq!(j.devices[0].driver, None);
    }

    #[test]
    fn pool_and_grant_edits() {
        let spec = |name: &str, device: &str| PciPoolSpec {
            name: name.into(),
            r#match: PciPoolMatch {
                vendor_id: Some("0x1002".into()),
                device_id: Some(device.into()),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut e = PutPciSettingsRequest::default();
        set_pool(&mut e, spec("r9700", "0x7551"));
        set_pool(&mut e, spec("mi300", "0x74a1"));
        // Same name: replaced in place, not added.
        set_pool(&mut e, spec("r9700", "0x7550"));
        assert_eq!(e.pools.len(), 2);
        assert_eq!(e.pools[0].r#match.device_id.as_deref(), Some("0x7550"));

        let pool = GrantTarget::of("r9700", None);
        assert_eq!(pool, GrantTarget::Pool("r9700"));
        grant(&mut e, pool, &names(&["lab"]), &[]);
        // A second grant on the target joins the first, without repeats.
        grant(&mut e, pool, &names(&["lab", "ml"]), &names(&["acme"]));
        let dev = GrantTarget::of("0000:41:00.0", Some("gpu-dev2"));
        assert_eq!(dev, GrantTarget::Device("0000:41:00.0", Some("gpu-dev2")));
        grant(&mut e, dev, &names(&["lab"]), &[]);
        // The same address on every node is another grant.
        grant(
            &mut e,
            GrantTarget::of("0000:41:00.0", None),
            &names(&["ml"]),
            &[],
        );
        assert_eq!(e.allow.len(), 3);
        assert_eq!(e.allow[0].projects, names(&["lab", "ml"]));
        assert_eq!(e.allow[0].tenants, names(&["acme"]));
        assert_eq!(
            grant_line(&e.allow[0]),
            "pool r9700: projects lab, ml; tenants acme"
        );
        assert_eq!(
            grant_line(&e.allow[1]),
            "device 0000:41:00.0 on gpu-dev2: projects lab"
        );

        revoke(&mut e, pool, &names(&["ml"]), &[]).unwrap();
        assert_eq!(e.allow[0].projects, names(&["lab"]));
        // A grant left to no one goes.
        revoke(&mut e, dev, &names(&["lab"]), &[]).unwrap();
        assert_eq!(e.allow.len(), 2);
        assert!(revoke(&mut e, dev, &[], &[]).is_err());
        grant(&mut e, GrantTarget::Pool("mi300"), &[], &names(&["*"]));

        // Removing a pool takes its grants with it.
        remove_pool(&mut e, "r9700").unwrap();
        assert_eq!(e.pools.len(), 1);
        assert!(e.allow.iter().all(|g| g.pool.as_deref() != Some("r9700")));
        assert_eq!(e.allow.len(), 2);
        assert!(remove_pool(&mut e, "r9700").is_err());
        // Without names, the whole grant goes.
        revoke(&mut e, GrantTarget::Pool("mi300"), &[], &[]).unwrap();
        assert_eq!(e.allow.len(), 1);
    }

    #[test]
    fn adopting_takes_the_discovered_pools_ids() {
        let found = PciPool {
            name: "amd-7551".into(),
            class: Some("gpu".into()),
            discovered: Some(true),
            ..Default::default()
        };
        let p = adopted(&found, "r9700").unwrap();
        assert_eq!(p.name, "r9700");
        assert_eq!(p.class.as_deref(), Some("gpu"));
        assert_eq!(p.r#match.vendor_id.as_deref(), Some("0x1002"));
        assert_eq!(p.r#match.device_id.as_deref(), Some("0x7551"));
        let bad = PciPool {
            name: "r9700".into(),
            ..Default::default()
        };
        assert!(adopted(&bad, "x").is_err());
    }
}
