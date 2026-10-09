//! The OVN northbound database as glidex uses it (spec/clustering.md §11).
//!
//! The control plane's network controller describes what should exist
//! ([`Desired`]); [`sync`] makes the database say so, touching only rows
//! glidex created (`external_ids:glidex-owner=glidex`) and reporting, never
//! deleting, the rest. Every call is `ovn-nbctl`, through the same [`Exec`]
//! glidex-ovs uses, so tests assert exact command lines.

use glidex_ovs::exec::{Cmd, Exec, Program};
use glidex_ovs::net::Ipv4Net;
use glidex_ovs::vsctl::{self, Row};
use glidex_ovs::OvsError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

pub const OWNER_KEY: &str = "glidex-owner";
pub const OWNER_VALUE: &str = "glidex";
pub const NETWORK_KEY: &str = "glidex-network";
pub const VM_KEY: &str = "glidex-vm-id";
pub const NIC_KEY: &str = "glidex-nic";
pub const ROUTER_KEY: &str = "glidex-router";
/// The shared edge router (§11.2).
pub const EDGE: &str = "gx-edge";
pub const NODES_SET: &str = "gx_nodes";
pub const SUPERNET_SET: &str = "gx_nat_supernet";

/// How to reach the northbound database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NbConn {
    /// `ssl:<ip>:6641` of each server.
    pub db: Vec<String>,
    pub key: PathBuf,
    pub cert: PathBuf,
    pub ca: PathBuf,
    /// A running `ovn-nbctl --detach` daemon's control socket (§11.1):
    /// commands then reuse its in-memory replica.
    pub daemon: Option<PathBuf>,
}

pub struct Nb<'a> {
    exec: &'a dyn Exec,
    conn: NbConn,
}

fn err(e: impl std::fmt::Display) -> OvsError {
    OvsError::Io(e.to_string())
}

impl<'a> Nb<'a> {
    pub fn new(exec: &'a dyn Exec, conn: NbConn) -> Self {
        Nb { exec, conn }
    }

    fn cmd(&self, args: Vec<String>) -> Cmd {
        let mut all: Vec<String> = Vec::new();
        let mut env = Vec::new();
        match &self.conn.daemon {
            Some(sock) => env.push(("OVN_NB_DAEMON".to_string(), sock.display().to_string())),
            None => {
                all.push(format!("--db={}", self.conn.db.join(",")));
                all.extend([
                    "-p".to_string(),
                    self.conn.key.display().to_string(),
                    "-c".into(),
                    self.conn.cert.display().to_string(),
                    "-C".into(),
                    self.conn.ca.display().to_string(),
                ]);
            }
        }
        all.extend(args);
        let mut c = Cmd::new(Program::OvnNbctl, all);
        c.env = env;
        c
    }

    /// Start a daemon (`ovn-nbctl --detach`); returns its control socket.
    pub fn start_daemon(exec: &dyn Exec, conn: &NbConn, pidfile: &str) -> Result<PathBuf, OvsError> {
        let args = vec![
            "--detach".to_string(),
            format!("--pidfile={pidfile}"),
            format!("--db={}", conn.db.join(",")),
            "-p".into(),
            conn.key.display().to_string(),
            "-c".into(),
            conn.cert.display().to_string(),
            "-C".into(),
            conn.ca.display().to_string(),
        ];
        let out = exec.check(&Cmd::new(Program::OvnNbctl, args))?.stdout_str();
        Ok(PathBuf::from(out.trim()))
    }

    /// Run commands as one transaction (`cmd1 -- cmd2 -- …`).
    pub fn txn(&self, cmds: Vec<Vec<String>>) -> Result<String, OvsError> {
        let mut args = Vec::new();
        for (i, c) in cmds.into_iter().enumerate() {
            if i > 0 {
                args.push("--".to_string());
            }
            args.extend(c);
        }
        Ok(self.exec.check(&self.cmd(args))?.stdout_str())
    }

    /// `find <table> <conditions…>` as rows.
    pub fn find(&self, table: &str, columns: &[&str], conditions: &[String]) -> Result<Vec<Row>, OvsError> {
        let mut args = vec!["--format=json".to_string(), format!("--columns={}", columns.join(",")), "find".into(), table.into()];
        args.extend(conditions.iter().cloned());
        vsctl::parse_table(&self.exec.check(&self.cmd(args))?.stdout_str())
    }

    fn owned(&self, table: &str, columns: &[&str]) -> Result<Vec<Row>, OvsError> {
        self.find(table, columns, &[format!("external_ids:{OWNER_KEY}={OWNER_VALUE}")])
    }
}

/// The id in an OVSDB `["uuid", "…"]` cell.
fn uuid(r: &Row, col: &str) -> Option<String> {
    match r.0.get(col)? {
        serde_json::Value::Array(a) if a.first() == Some(&serde_json::Value::String("uuid".into())) => a.get(1)?.as_str().map(String::from),
        serde_json::Value::String(x) => Some(x.clone()),
        _ => None,
    }
}

fn s(x: &str) -> String {
    x.to_string()
}

fn ids(pairs: &[(&str, &str)]) -> Vec<String> {
    let mut v = vec![format!("external_ids:{OWNER_KEY}={OWNER_VALUE}")];
    v.extend(pairs.iter().map(|(k, val)| format!("external_ids:{k}={val}")));
    v
}

// ---- desired state ------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetKind {
    /// A switch and DHCP, no router: no path off the network.
    Isolated,
    /// Routed to the edge (or a VPC router) with SNAT.
    Nat,
    /// A `localnet` port onto a physical network (optionally one VLAN).
    Provider { physnet: String, vlan: Option<u16> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSpec {
    pub name: String,
    pub kind: NetKind,
    /// Isolated and NAT networks.
    pub cidr: Option<Ipv4Net>,
    pub dns: Vec<IpAddr>,
    pub mtu: u16,
    /// A VPC router (§11.2a); `None` attaches a NAT network to the edge.
    pub router: Option<String>,
}

impl NetworkSpec {
    pub fn switch(&self) -> String {
        format!("gx-{}", self.name)
    }
    pub fn gateway(&self) -> Option<Ipv4Addr> {
        self.cidr.and_then(|c| c.host(1))
    }
    /// The MAC of the router port (stable, derived from the name).
    pub fn router_mac(&self) -> String {
        derived_mac(&format!("gxr:{}", self.name))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeSpec {
    pub physnet: String,
    pub external_ip: Ipv4Addr,
    pub external_prefix: u8,
    pub gateway: Ipv4Addr,
    /// Chassis (node ids), highest priority first.
    pub gateway_nodes: Vec<String>,
    /// The edge's conntrack zone for SNAT (§13.3).
    #[serde(default)]
    pub snat_ct_zone: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortSpec {
    pub network: String,
    pub lport: String,
    pub mac: String,
    /// `None` on provider networks (the LAN's DHCP).
    pub ip: Option<Ipv4Addr>,
    pub vm_id: String,
    pub nic: u8,
    /// Where the VM runs: binds the port there (§11.5).
    pub chassis: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Desired {
    pub edge: Option<EdgeSpec>,
    /// VPC routers (§11.2a).
    #[serde(default)]
    pub routers: Vec<RouterSpec>,
    pub networks: Vec<NetworkSpec>,
    pub ports: Vec<PortSpec>,
    /// Every node's addresses: guests may not reach them (§11.2).
    pub node_addresses: Vec<IpAddr>,
    pub nat_supernet: Option<Ipv4Net>,
}

/// A locally administered MAC derived from `seed`.
pub fn derived_mac(seed: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in seed.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let b = h.to_be_bytes();
    format!("02:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2], b[3], b[4])
}

/// What a [`sync`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncReport {
    pub changed: Vec<String>,
    /// Rows glidex doesn't own that sit where its objects go: reported only.
    pub foreign: Vec<String>,
}

fn quoted_set(items: &BTreeSet<String>) -> String {
    items.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(",")
}

fn address_set(nb: &Nb, name: &str, addrs: &BTreeSet<String>, report: &mut SyncReport) -> Result<(), OvsError> {
    let rows = nb.find("Address_Set", &["name", "addresses", "external_ids"], &[format!("name={name}")])?;
    match rows.first() {
        None => {
            let mut args = vec![s("create"), s("Address_Set"), format!("name={name}")];
            if !addrs.is_empty() {
                args.push(format!("addresses={}", quoted_set(addrs)));
            }
            args.extend(ids(&[]));
            nb.txn(vec![args])?;
            report.changed.push(format!("address set {name}"));
        }
        Some(r) => {
            if !r.owned_by_glidex() {
                report.foreign.push(format!("address set {name}"));
                return Ok(());
            }
            let have: BTreeSet<String> = r.set("addresses").into_iter().collect();
            if &have != addrs {
                let value = if addrs.is_empty() { "addresses=[]".to_string() } else { format!("addresses={}", quoted_set(addrs)) };
                nb.txn(vec![vec![s("set"), s("Address_Set"), name.to_string(), value]])?;
                report.changed.push(format!("address set {name}"));
            }
        }
    }
    Ok(())
}

fn dhcp_options(n: &NetworkSpec) -> Vec<String> {
    let mut o = vec!["lease_time=3600".to_string()];
    if let Some(gw) = n.gateway() {
        o.push(format!("server_id={gw}"));
        if n.kind == NetKind::Nat {
            o.push(format!("router={gw}"));
        }
    }
    o.push(format!("server_mac={}", n.router_mac()));
    o.push(format!("mtu={}", n.mtu));
    if !n.dns.is_empty() {
        o.push(format!("dns_server={{{}}}", n.dns.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(",")));
    }
    o
}

/// Make the northbound database match `desired`.
pub fn sync(nb: &Nb, desired: &Desired) -> Result<SyncReport, OvsError> {
    let mut report = SyncReport::default();

    // Address sets (§11.1): what the isolation policies match on.
    let nodes: BTreeSet<String> = desired.node_addresses.iter().map(|a| a.to_string()).collect();
    address_set(nb, NODES_SET, &nodes, &mut report)?;
    if let Some(n) = desired.nat_supernet {
        address_set(nb, SUPERNET_SET, &BTreeSet::from([format!("{}/{}", n.network(), n.prefix())]), &mut report)?;
    }

    let switches = nb.owned("Logical_Switch", &["name", "external_ids", "other_config"])?;
    let have_switches: BTreeSet<String> = switches.iter().filter_map(|r| r.str("name")).collect();
    let routers = nb.owned("Logical_Router", &["name", "external_ids"])?;
    let have_routers: BTreeSet<String> = routers.iter().filter_map(|r| r.str("name")).collect();

    // The edge: provider switch, router, gateway port, HA group, policies, route.
    if let Some(e) = &desired.edge {
        let on_edge: Vec<&NetworkSpec> = desired.networks.iter().filter(|n| n.router.is_none()).collect();
        ensure_gateway(nb, &edge_gateway(e), desired, &have_switches, &have_routers, &on_edge, &mut report)?;
    }
    // VPC routers: each its own gateway (or none), and an address set of its networks.
    let mut want_routers: BTreeSet<String> = BTreeSet::new();
    for r in &desired.routers {
        want_routers.insert(r.lr());
        let mine: Vec<&NetworkSpec> = desired.networks.iter().filter(|n| n.router.as_deref() == Some(r.name.as_str())).collect();
        let nets: BTreeSet<String> = mine.iter().filter_map(|n| n.cidr).map(|c| format!("{}/{}", c.network(), c.prefix())).collect();
        address_set(nb, &r.nets_set(), &nets, &mut report)?;
        match &r.external {
            Some(ext) => ensure_gateway(nb, &vpc_gateway(r, ext), desired, &have_switches, &have_routers, &mine, &mut report)?,
            None => {
                if !have_routers.contains(&r.lr()) {
                    nb.txn(vec![vec![s("--may-exist"), s("lr-add"), r.lr()], [vec![s("set"), s("Logical_Router"), r.lr()], ids(&[("glidex-role", "vpc")])].concat()])?;
                    report.changed.push(format!("router {}", r.lr()));
                }
            }
        }
    }
    for lr in routers.iter().filter_map(|r| r.str("name")).filter(|n| n.starts_with("gxr-") && !want_routers.contains(n)) {
        let ext_ports: Vec<String> = nb
            .find("Logical_Switch_Port", &["name"], &[format!("external_ids:{OWNER_KEY}={OWNER_VALUE}")])?
            .iter()
            .filter_map(|r| r.str("name"))
            .filter(|n| n.contains("-rt-") && n.ends_with(lr.trim_start_matches("gxr-")))
            .collect();
        let mut cmds = vec![vec![s("--if-exists"), s("lr-del"), lr.clone()]];
        cmds.extend(ext_ports.into_iter().map(|p| vec![s("--if-exists"), s("lsp-del"), p]));
        nb.txn(cmds)?;
        // The router's gateway port referred to the group until the router went.
        nb.txn(vec![vec![s("--if-exists"), s("destroy"), s("HA_Chassis_Group"), lr.clone()]])?;
        report.changed.push(format!("removed router {lr}"));
    }

    // No edge wanted: take down the one glidex made.
    if desired.edge.is_none() && have_routers.contains(EDGE) {
        nb.txn(vec![vec![s("--if-exists"), s("lr-del"), EDGE.into()]])?;
        nb.txn(vec![vec![s("--if-exists"), s("destroy"), s("HA_Chassis_Group"), EDGE.into()]])?;
        for sw in switches.iter().filter_map(|r| r.str("name")).filter(|n| n.starts_with("gx-ext-")) {
            nb.txn(vec![vec![s("--if-exists"), s("ls-del"), sw]])?;
        }
        report.changed.push("edge removed".into());
    }

    // Networks.
    let mut want_switches: BTreeSet<String> = BTreeSet::new();
    for n in &desired.networks {
        want_switches.insert(n.switch());
        ensure_network(nb, n, desired, &have_switches, &mut report)?;
    }

    // Ports.
    let have_ports = nb.owned("Logical_Switch_Port", &["name", "external_ids", "addresses", "port_security", "options"])?;
    let mut want_ports: BTreeSet<String> = BTreeSet::new();
    for p in &desired.ports {
        want_ports.insert(p.lport.clone());
        ensure_port(nb, p, desired, have_ports.iter().find(|r| r.str("name").as_deref() == Some(p.lport.as_str())), &mut report)?;
    }
    // Ports gone from the plan (a deleted VM).
    for r in &have_ports {
        let Some(name) = r.str("name") else { continue };
        // Infrastructure ports (router, localnet) carry the network key but no VM.
        if r.map("external_ids").contains_key(VM_KEY) && !want_ports.contains(&name) {
            nb.txn(vec![vec![s("--if-exists"), s("lsp-del"), name.clone()]])?;
            report.changed.push(format!("removed port {name}"));
        }
    }

    // Switches gone from the plan. Their ports go with them in the database.
    for sw in &switches {
        let Some(name) = sw.str("name") else { continue };
        if want_switches.contains(&name) || (name.starts_with("gx-ext-") && desired.edge.is_some()) {
            continue;
        }
        if name.starts_with("gx-ext-") {
            continue; // removed with the edge above
        }
        let net = sw.map("external_ids").get(NETWORK_KEY).cloned().unwrap_or_default();
        let mut cmds = vec![vec![s("--if-exists"), s("ls-del"), name.clone()]];
        if let Some(rp) = routers.iter().find(|_| false) {
            let _ = rp;
        }
        cmds.push(vec![s("--if-exists"), s("lrp-del"), format!("gx-lrp-{net}")]);
        nb.txn(cmds)?;
        // Its DHCP options, and its edge NAT rule.
        for d in nb.find("DHCP_Options", &["_uuid", "external_ids"], &[format!("external_ids:{NETWORK_KEY}={net}")])? {
            if let Some(u) = uuid(&d, "_uuid") {
                nb.txn(vec![vec![s("dhcp-options-del"), u]])?;
            }
        }
        report.changed.push(format!("removed network {net}"));
    }
    Ok(report)
}

/// A router's way out: its address on the provider network and where it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalSpec {
    pub physnet: String,
    pub external_ip: Ipv4Addr,
    pub external_prefix: u8,
    pub gateway: Ipv4Addr,
    /// Chassis (node ids), highest priority first.
    pub gateway_nodes: Vec<String>,
    /// `options:snat-ct-zone`: the conntrack zone of its SNAT, for metering (§13.3).
    pub snat_ct_zone: Option<u16>,
}

/// A project's own router (§11.2a). `external: None` is a router that only
/// joins its networks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterSpec {
    pub name: String,
    pub external: Option<ExternalSpec>,
}

impl RouterSpec {
    pub fn lr(&self) -> String {
        format!("gxr-{}", self.name)
    }
    pub fn nets_set(&self) -> String {
        format!("gx_r_{}_nets", self.name.replace('-', "_"))
    }
}

/// What distinguishes the shared edge from a VPC router in the shared code.
struct Gateway {
    router: String,
    group: String,
    lrp: String,
    lsp: String,
    seed: String,
    ext: ExternalSpec,
    /// The match of the supernet policy: the edge drops all of it, a VPC
    /// router all but its own networks.
    supernet_match: String,
    role: &'static str,
}

fn edge_gateway(e: &EdgeSpec) -> Gateway {
    Gateway {
        router: EDGE.into(),
        group: EDGE.into(),
        lrp: "gx-edge-ext".into(),
        lsp: format!("gx-ext-{}-rt", e.physnet),
        seed: "gxr:edge".into(),
        ext: ExternalSpec { physnet: e.physnet.clone(), external_ip: e.external_ip, external_prefix: e.external_prefix, gateway: e.gateway, gateway_nodes: e.gateway_nodes.clone(), snat_ct_zone: e.snat_ct_zone },
        supernet_match: format!("ip4.dst == ${SUPERNET_SET}"),
        role: "edge",
    }
}

fn vpc_gateway(r: &RouterSpec, ext: &ExternalSpec) -> Gateway {
    Gateway {
        router: r.lr(),
        group: r.lr(),
        lrp: format!("{}-ext", r.lr()),
        lsp: format!("gx-ext-{}-rt-{}", ext.physnet, r.name),
        seed: format!("gxr:vpc:{}", r.name),
        ext: ext.clone(),
        supernet_match: format!("ip4.dst == ${SUPERNET_SET} && ip4.dst != ${}", r.nets_set()),
        role: "vpc",
    }
}

/// Bring one gateway router, its provider-side port, HA chassis group,
/// isolation policies and SNAT rules to what `desired` says.
fn ensure_gateway(nb: &Nb, g: &Gateway, desired: &Desired, have_switches: &BTreeSet<String>, have_routers: &BTreeSet<String>, nets: &[&NetworkSpec], report: &mut SyncReport) -> Result<(), OvsError> {
    let e = &g.ext;
    let ext = format!("gx-ext-{}", e.physnet);
    let ext_mac = derived_mac(&g.seed);
    if !have_switches.contains(&ext) {
        nb.txn(vec![
            vec![s("--may-exist"), s("ls-add"), ext.clone()],
            [vec![s("set"), s("Logical_Switch"), ext.clone()], ids(&[("glidex-role", "provider")])].concat(),
            vec![s("--may-exist"), s("lsp-add"), ext.clone(), format!("{ext}-ln")],
            vec![s("lsp-set-type"), format!("{ext}-ln"), s("localnet")],
            vec![s("lsp-set-addresses"), format!("{ext}-ln"), s("unknown")],
            vec![s("lsp-set-options"), format!("{ext}-ln"), format!("network_name={}", e.physnet)],
            [vec![s("set"), s("Logical_Switch_Port"), format!("{ext}-ln")], ids(&[])].concat(),
        ])?;
        report.changed.push(format!("provider switch {ext}"));
    }
    if !have_routers.contains(&g.router) {
        let mut cmds = vec![
            vec![s("--may-exist"), s("lr-add"), g.router.clone()],
            [vec![s("set"), s("Logical_Router"), g.router.clone()], ids(&[("glidex-role", g.role)])].concat(),
            vec![s("--may-exist"), s("lrp-add"), g.router.clone(), g.lrp.clone(), ext_mac.clone(), format!("{}/{}", e.external_ip, e.external_prefix)],
            [vec![s("set"), s("Logical_Router_Port"), g.lrp.clone()], ids(&[])].concat(),
            vec![s("--may-exist"), s("lsp-add"), ext.clone(), g.lsp.clone()],
            vec![s("lsp-set-type"), g.lsp.clone(), s("router")],
            vec![s("lsp-set-addresses"), g.lsp.clone(), s("router")],
            vec![s("lsp-set-options"), g.lsp.clone(), format!("router-port={}", g.lrp)],
            [vec![s("set"), s("Logical_Switch_Port"), g.lsp.clone()], ids(&[])].concat(),
            vec![s("--may-exist"), s("lr-route-add"), g.router.clone(), s("0.0.0.0/0"), e.gateway.to_string()],
        ];
        if let Some(z) = e.snat_ct_zone {
            cmds.push(vec![s("set"), s("Logical_Router"), g.router.clone(), format!("options:snat-ct-zone={z}")]);
        }
        nb.txn(cmds)?;
        report.changed.push(format!("router {}", g.router));
    } else if let Some(z) = e.snat_ct_zone {
        let rows = nb.find("Logical_Router", &["name", "options"], &[format!("name={}", g.router)])?;
        if rows.first().map(|r| r.map("options").get("snat-ct-zone").cloned()) != Some(Some(z.to_string())) {
            nb.txn(vec![vec![s("set"), s("Logical_Router"), g.router.clone(), format!("options:snat-ct-zone={z}")]])?;
            report.changed.push(format!("{} conntrack zone", g.router));
        }
    }
    // HA gateway group: chassis in priority order, rebuilt when the list changes.
    let groups = nb.find("HA_Chassis_Group", &["_uuid", "name", "external_ids", "ha_chassis"], &[format!("name={}", g.group)])?;
    let mut ha_ok = false;
    if let Some(grp) = groups.first() {
        let members = nb.find("HA_Chassis", &["_uuid", "chassis_name", "priority"], &[])?;
        let want: Vec<(String, i64)> = e.gateway_nodes.iter().enumerate().map(|(i, c)| (c.clone(), 100 - i as i64)).collect();
        let mut have: Vec<(String, i64)> = members
            .iter()
            .filter(|m| uuid(m, "_uuid").is_some_and(|u| grp.0.get("ha_chassis").map(|v| v.to_string().contains(&u)).unwrap_or(false)))
            .filter_map(|m| Some((m.str("chassis_name")?, m.int("priority")?)))
            .collect();
        have.sort_by_key(|x| std::cmp::Reverse(x.1));
        ha_ok = have == want;
        if !ha_ok {
            // The gateway port refers to the group: let go of it in the same transaction.
            nb.txn(vec![vec![s("--if-exists"), s("clear"), s("Logical_Router_Port"), g.lrp.clone(), s("ha_chassis_group")], vec![s("--if-exists"), s("destroy"), s("HA_Chassis_Group"), g.group.clone()]])?;
        }
    }
    if !ha_ok {
        let mut cmds = vec![vec![s("ha-chassis-group-add"), g.group.clone()]];
        for (i, c) in e.gateway_nodes.iter().enumerate() {
            cmds.push(vec![s("ha-chassis-group-add-chassis"), g.group.clone(), c.clone(), (100 - i as i64).to_string()]);
        }
        cmds.push([vec![s("set"), s("HA_Chassis_Group"), g.group.clone()], ids(&[])].concat());
        nb.txn(cmds)?;
        // The gateway port follows the group.
        let uuid = nb.find("HA_Chassis_Group", &["_uuid"], &[format!("name={}", g.group)])?.first().and_then(|r| uuid(r, "_uuid")).ok_or_else(|| err("ha chassis group was not created"))?;
        nb.txn(vec![vec![s("set"), s("Logical_Router_Port"), g.lrp.clone(), format!("ha_chassis_group={uuid}")]])?;
        report.changed.push(format!("{} gateway chassis", g.router));
    }
    // Isolation (§11.2): guests reach neither each other's networks nor any node.
    // Policy rows don't name their router: only this router's count.
    let mine: String = nb.find("Logical_Router", &["policies"], &[format!("name={}", g.router)])?.first().and_then(|r| r.0.get("policies").map(|v| v.to_string())).unwrap_or_default();
    let policies: Vec<Row> = nb.find("Logical_Router_Policy", &["_uuid", "priority", "match", "action"], &[])?.into_iter().filter(|p| uuid(p, "_uuid").is_some_and(|u| mine.contains(&u))).collect();
    let has = |prio: i64, m: &str| policies.iter().any(|p| p.int("priority") == Some(prio) && p.str("match").as_deref() == Some(m));
    let nodes_match = format!("ip4.dst == ${NODES_SET}");
    let mut cmds = Vec::new();
    if !desired.node_addresses.is_empty() && !has(1100, &nodes_match) {
        cmds.push(vec![s("--may-exist"), s("lr-policy-add"), g.router.clone(), s("1100"), nodes_match, s("drop")]);
    }
    if desired.nat_supernet.is_some() && !has(1000, &g.supernet_match) {
        cmds.push(vec![s("--may-exist"), s("lr-policy-add"), g.router.clone(), s("1000"), g.supernet_match.clone(), s("drop")]);
    }
    if !cmds.is_empty() {
        nb.txn(cmds)?;
        report.changed.push(format!("{} isolation policies", g.router));
    }
    // The router's external address, per NAT network on it.
    let nats = nb.find("NAT", &["external_ip", "logical_ip", "type"], &[])?;
    for n in nets.iter().filter(|n| n.kind == NetKind::Nat) {
        let Some(cidr) = n.cidr else { continue };
        let logical = format!("{}/{}", cidr.network(), cidr.prefix());
        if !nats.iter().any(|x| x.str("logical_ip").as_deref() == Some(logical.as_str()) && x.str("type").as_deref() == Some("snat")) {
            nb.txn(vec![vec![s("--may-exist"), s("lr-nat-add"), g.router.clone(), s("snat"), e.external_ip.to_string(), logical]])?;
            report.changed.push(format!("snat {}", n.name));
        }
    }
    Ok(())
}

fn ensure_network(nb: &Nb, n: &NetworkSpec, desired: &Desired, have_switches: &BTreeSet<String>, report: &mut SyncReport) -> Result<(), OvsError> {
    let sw = n.switch();
    let new = !have_switches.contains(&sw);
    let mut cmds = vec![vec![s("--may-exist"), s("ls-add"), sw.clone()]];
    if new {
        cmds.push([vec![s("set"), s("Logical_Switch"), sw.clone()], ids(&[(NETWORK_KEY, &n.name)])].concat());
        if let Some(c) = n.cidr {
            cmds.push(vec![s("set"), s("Logical_Switch"), sw.clone(), format!("other_config:subnet={}/{}", c.network(), c.prefix())]);
        }
    }
    // The provider port.
    if let NetKind::Provider { physnet, vlan } = &n.kind {
        let ln = format!("{sw}-ln");
        cmds.extend([
            vec![s("--may-exist"), s("lsp-add"), sw.clone(), ln.clone()],
            vec![s("lsp-set-type"), ln.clone(), s("localnet")],
            vec![s("lsp-set-addresses"), ln.clone(), s("unknown")],
            vec![s("lsp-set-options"), ln.clone(), format!("network_name={physnet}")],
        ]);
        if let Some(v) = vlan {
            cmds.push(vec![s("set"), s("Logical_Switch_Port"), ln.clone(), format!("tag={v}")]);
        }
        cmds.push([vec![s("set"), s("Logical_Switch_Port"), ln], ids(&[(NETWORK_KEY, &n.name)])].concat());
    }
    // Routing: a NAT network gets a router port with the gateway address.
    if n.kind == NetKind::Nat {
        if let (Some(cidr), Some(gw)) = (n.cidr, n.gateway()) {
            let router = n.router.clone().map(|r| format!("gxr-{r}")).unwrap_or_else(|| EDGE.to_string());
            let lrp = format!("gx-lrp-{}", n.name);
            let lsp = format!("{sw}-rt");
            cmds.extend([
                vec![s("--may-exist"), s("lrp-add"), router.clone(), lrp.clone(), n.router_mac(), format!("{}/{}", gw, cidr.prefix())],
                [vec![s("set"), s("Logical_Router_Port"), lrp.clone()], ids(&[(NETWORK_KEY, &n.name)])].concat(),
                vec![s("--may-exist"), s("lsp-add"), sw.clone(), lsp.clone()],
                vec![s("lsp-set-type"), lsp.clone(), s("router")],
                vec![s("lsp-set-addresses"), lsp.clone(), s("router")],
                vec![s("lsp-set-options"), lsp.clone(), format!("router-port={lrp}")],
                [vec![s("set"), s("Logical_Switch_Port"), lsp], ids(&[(NETWORK_KEY, &n.name)])].concat(),
            ]);
        }
    }
    let was = report.changed.len();
    // DHCP: one options row per network, created once, updated when it differs.
    if n.cidr.is_some() {
        let rows = nb.find("DHCP_Options", &["_uuid", "options"], &[format!("external_ids:{NETWORK_KEY}={}", n.name)])?;
        let want = dhcp_options(n);
        let cidr = n.cidr.map(|c| format!("{}/{}", c.network(), c.prefix())).unwrap_or_default();
        match rows.first() {
            None => {
                // `dhcp-options-create` takes external_ids keys bare (`key=value`).
                cmds.push(vec![s("dhcp-options-create"), cidr, format!("{OWNER_KEY}={OWNER_VALUE}"), format!("{NETWORK_KEY}={}", n.name)]);
            }
            Some(r) => {
                let have = r.map("options");
                let ok = want.iter().all(|kv| kv.split_once('=').is_some_and(|(k, v)| have.get(k).map(|x| x.replace(' ', "")) == Some(v.replace(' ', ""))));
                if !ok {
                    cmds.push([vec![s("dhcp-options-set-options"), uuid(r, "_uuid").unwrap_or_default()], want.clone()].concat());
                }
            }
        }
    }
    nb.txn(cmds)?;
    // A new DHCP row still needs its options set, now that it has a UUID.
    if n.cidr.is_some() {
        let rows = nb.find("DHCP_Options", &["_uuid", "options"], &[format!("external_ids:{NETWORK_KEY}={}", n.name)])?;
        if let Some(r) = rows.first() {
            if r.map("options").is_empty() {
                nb.txn(vec![[vec![s("dhcp-options-set-options"), uuid(r, "_uuid").unwrap_or_default()], dhcp_options(n)].concat()])?;
            }
        }
    }
    let _ = (desired, was);
    if new {
        report.changed.push(format!("network {}", n.name));
    }
    Ok(())
}

fn ensure_port(nb: &Nb, p: &PortSpec, desired: &Desired, have: Option<&Row>, report: &mut SyncReport) -> Result<(), OvsError> {
    let sw = format!("gx-{}", p.network);
    let net = desired.networks.iter().find(|n| n.name == p.network);
    let addr = match p.ip {
        Some(ip) => format!("{} {}", p.mac, ip),
        None => p.mac.clone(),
    };
    let want_opts: BTreeMap<String, String> = p.chassis.iter().map(|c| ("requested-chassis".to_string(), c.clone())).collect();
    let up_to_date = have.is_some_and(|r| {
        r.set("addresses") == vec![addr.clone()]
            && r.set("port_security") == vec![addr.clone()]
            && r.map("options").get("requested-chassis") == want_opts.get("requested-chassis")
    });
    if up_to_date {
        return Ok(());
    }
    let vm = p.vm_id.as_str();
    let nic = p.nic.to_string();
    let mut cmds = vec![
        vec![s("--may-exist"), s("lsp-add"), sw, p.lport.clone()],
        vec![s("lsp-set-addresses"), p.lport.clone(), addr.clone()],
        // Port security (§11.4): the guest may use only its own MAC and address.
        vec![s("lsp-set-port-security"), p.lport.clone(), addr],
        [vec![s("set"), s("Logical_Switch_Port"), p.lport.clone()], ids(&[(NETWORK_KEY, &p.network), (VM_KEY, vm), (NIC_KEY, &nic)])].concat(),
    ];
    match &p.chassis {
        Some(c) => cmds.push(vec![s("lsp-set-options"), p.lport.clone(), format!("requested-chassis={c}")]),
        None => cmds.push(vec![s("remove"), s("Logical_Switch_Port"), p.lport.clone(), s("options"), s("requested-chassis")]),
    }
    // The network's DHCP options, by the UUID of its row.
    if let (Some(_), Some(n)) = (p.ip, net) {
        if n.cidr.is_some() {
            if let Some(u) = nb.find("DHCP_Options", &["_uuid"], &[format!("external_ids:{NETWORK_KEY}={}", p.network)])?.first().and_then(|r| uuid(r, "_uuid")) {
                cmds.push(vec![s("lsp-set-dhcpv4-options"), p.lport.clone(), u]);
            }
        }
    }
    nb.txn(cmds)?;
    report.changed.push(format!("port {}", p.lport));
    Ok(())
}

#[cfg(test)]
mod tests;
