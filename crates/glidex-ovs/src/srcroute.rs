//! Source routes for ports that share a subnet (spec §8.6, decisions 11–17).
//!
//! When several ports have addresses in one subnet, the kernel sends
//! everything for that subnet out of the first port's route, and strict
//! reverse-path filtering drops what arrives on the others. For each address
//! `A` on port `P` in a managed subnet, netd keeps a table `T(P)` holding the
//! subnet's route through `P` and a rule `from A lookup T(P)`. Every route and
//! rule it adds carries protocol [`PROTOCOL`]; it changes or deletes only
//! those. [`observe`] reads the host, [`plan`] works out what to change (and
//! the report) without touching anything, and [`apply`] runs the plan.

use crate::OvsError;
use crate::exec::{Cmd, Exec, Program};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::str::FromStr;

/// glidex's routing protocol number: one iproute2 doesn't assign.
pub const PROTOCOL: u8 = 77;
/// The name the installer gives [`PROTOCOL`] in [`RT_PROTOS_FILE`].
pub const PROTOCOL_NAME: &str = "glidex";
pub const RT_PROTOS_FILE: &str = "/etc/iproute2/rt_protos.d/glidex.conf";

/// The contents of [`RT_PROTOS_FILE`].
pub fn rt_protos_conf() -> String {
    format!("{PROTOCOL}\t{PROTOCOL_NAME}\n")
}

/// An IPv4 or IPv6 network in CIDR form, stored normalized (host bits cleared).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn new(addr: IpAddr, prefix: u8) -> Result<Self, OvsError> {
        let addr = match addr {
            IpAddr::V4(a) if prefix <= 32 => {
                let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
                IpAddr::V4(Ipv4Addr::from(u32::from(a) & mask))
            }
            IpAddr::V6(a) if prefix <= 128 => {
                let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
                IpAddr::V6(Ipv6Addr::from(u128::from(a) & mask))
            }
            _ => return Err(OvsError::invalid(format!("prefix /{prefix} out of range"))),
        };
        Ok(Self { addr, prefix })
    }

    pub fn is_v6(&self) -> bool {
        self.addr.is_ipv6()
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    /// Whether `ip` is in this network (same family).
    pub fn contains(&self, ip: IpAddr) -> bool {
        ip.is_ipv6() == self.is_v6()
            && Self::new(ip, self.prefix).is_ok_and(|n| n.addr == self.addr)
    }

    /// Whether `other` lies wholly inside this network.
    pub fn contains_net(&self, other: &IpNet) -> bool {
        other.prefix >= self.prefix && self.contains(other.addr)
    }

    /// A single address (`/32`, `/128`): nothing to share.
    fn is_host(&self) -> bool {
        self.prefix == if self.is_v6() { 128 } else { 32 }
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl FromStr for IpNet {
    type Err = OvsError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || OvsError::invalid(format!("'{s}' is not a subnet (address/prefix)"));
        let (a, p) = s.split_once('/').ok_or_else(bad)?;
        Self::new(a.parse().map_err(|_| bad())?, p.parse().map_err(|_| bad())?)
    }
}

impl Serialize for IpNet {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for IpNet {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// `netd.json`'s `source_routes` (spec §8.6.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The subnets netd manages; empty means it only reports (decision 14).
    pub subnets: Vec<IpNet>,
    /// The rules' priority, before `main` (32766).
    pub rule_priority: u32,
    /// The table numbers netd may give ports, inclusive (decision 15).
    pub table_range: [u32; 2],
}

impl Default for Config {
    fn default() -> Self {
        Self {
            subnets: Vec::new(),
            rule_priority: 1000,
            table_range: [4200, 4299],
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), OvsError> {
        if !(1..32766).contains(&self.rule_priority) {
            return Err(OvsError::invalid(format!(
                "source_routes.rule_priority {} must be between 1 and 32765 (before main)",
                self.rule_priority
            )));
        }
        let [lo, hi] = self.table_range;
        if lo == 0 || lo > hi {
            return Err(OvsError::invalid(format!(
                "source_routes.table_range [{lo}, {hi}] must be a range of numbers from 1"
            )));
        }
        if lo <= 255 && hi >= 253 {
            return Err(OvsError::invalid(format!(
                "source_routes.table_range [{lo}, {hi}] takes in tables 253-255 (default, main, local)"
            )));
        }
        Ok(())
    }

    fn manages(&self, net: &IpNet) -> bool {
        self.subnets.iter().any(|s| s.contains_net(net))
    }
}

/// What netd keeps between passes (its `source_tables` and `arp_saved`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// ifname → its table.
    #[serde(default)]
    pub tables: BTreeMap<String, u32>,
    /// ifname → the ARP settings it had before netd set them.
    #[serde(default)]
    pub arp_saved: BTreeMap<String, Arp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Arp {
    pub arp_ignore: u8,
    pub arp_announce: u8,
}

/// The settings netd gives a managed port (decision 17).
pub const MANAGED_ARP: Arp = Arp {
    arp_ignore: 1,
    arp_announce: 2,
};

/// A global address on a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addr {
    pub ifname: String,
    pub ip: IpAddr,
    pub prefix: u8,
}

/// A policy rule, as `ip -j rule` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub v6: bool,
    pub priority: u32,
    /// `None` for `from all`.
    pub src: Option<String>,
    /// Set when the source is a prefix, not one address.
    pub srclen: Option<u8>,
    pub table: Option<String>,
    pub protocol: Option<String>,
}

impl Rule {
    fn ours(&self) -> bool {
        is_ours(self.protocol.as_deref())
    }

    fn describe(&self) -> String {
        let src = match (&self.src, self.srclen) {
            (Some(s), Some(l)) => format!("{s}/{l}"),
            (Some(s), None) => s.clone(),
            (None, _) => "all".into(),
        };
        let mut d = format!("priority {} from {src}", self.priority);
        if let Some(t) = &self.table {
            d.push_str(&format!(" lookup {t}"));
        }
        if let Some(p) = &self.protocol {
            d.push_str(&format!(" protocol {p}"));
        }
        d
    }
}

/// A route, as `ip -j route show table all` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub v6: bool,
    pub dst: String,
    pub dev: Option<String>,
    /// `main` when `ip` leaves it out.
    pub table: String,
    pub protocol: Option<String>,
    pub prefsrc: Option<String>,
}

/// A port's reverse-path and ARP settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortSysctls {
    pub rp_filter: Option<u8>,
    pub arp_ignore: Option<u8>,
    pub arp_announce: Option<u8>,
}

/// The host as [`observe`] read it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Observed {
    /// Every interface, with or without addresses.
    pub links: BTreeSet<String>,
    pub addrs: Vec<Addr>,
    pub rules: Vec<Rule>,
    pub routes: Vec<Route>,
    pub sysctls: BTreeMap<String, PortSysctls>,
}

fn is_ours(protocol: Option<&str>) -> bool {
    protocol.is_some_and(|p| p == PROTOCOL_NAME || p == PROTOCOL.to_string())
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn ip_json(exec: &dyn Exec, args: &[&str]) -> Result<Vec<Value>, OvsError> {
    let out = exec.check(&Cmd::new(Program::Ip, args.iter().copied()))?;
    Ok(serde_json::from_slice(&out.stdout).unwrap_or_default())
}

fn sysctl_path(family: &str, ifname: &str, key: &str) -> String {
    format!("/proc/sys/net/{family}/conf/{ifname}/{key}")
}

/// Read addresses, rules, routes and the ports' settings.
pub fn observe(exec: &dyn Exec) -> Result<Observed, OvsError> {
    let mut obs = Observed::default();
    for link in ip_json(exec, &["-j", "addr", "show"])? {
        let Some(ifname) = str_of(&link, "ifname") else {
            continue;
        };
        obs.links.insert(ifname.clone());
        for a in link
            .get("addr_info")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if a.get("scope").and_then(Value::as_str) != Some("global") {
                continue;
            }
            let ip = a
                .get("local")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok());
            let prefix = a.get("prefixlen").and_then(Value::as_u64);
            if let (Some(ip), Some(prefix)) = (ip, prefix) {
                obs.addrs.push(Addr {
                    ifname: ifname.clone(),
                    ip,
                    prefix: prefix as u8,
                });
            }
        }
    }
    for (v6, family) in [(false, "-4"), (true, "-6")] {
        for r in ip_json(exec, &[family, "-j", "rule", "show"])? {
            obs.rules.push(Rule {
                v6,
                priority: r.get("priority").and_then(Value::as_u64).unwrap_or(0) as u32,
                src: str_of(&r, "src").filter(|s| s != "all"),
                srclen: r.get("srclen").and_then(Value::as_u64).map(|l| l as u8),
                table: str_of(&r, "table"),
                protocol: str_of(&r, "protocol"),
            });
        }
        for r in ip_json(exec, &[family, "-j", "route", "show", "table", "all"])? {
            let Some(dst) = str_of(&r, "dst") else {
                continue;
            };
            obs.routes.push(Route {
                v6,
                dst,
                dev: str_of(&r, "dev"),
                table: str_of(&r, "table").unwrap_or_else(|| "main".into()),
                protocol: str_of(&r, "protocol"),
                prefsrc: str_of(&r, "prefsrc"),
            });
        }
    }
    let v4_ports: BTreeSet<&str> = obs
        .addrs
        .iter()
        .filter(|a| a.ip.is_ipv4())
        .map(|a| a.ifname.as_str())
        .collect();
    for ifname in v4_ports {
        let read = |key| {
            exec.read_file(Path::new(&sysctl_path("ipv4", ifname, key)))
                .ok()
                .and_then(|s| s.trim().parse().ok())
        };
        obs.sysctls.insert(
            ifname.to_string(),
            PortSysctls {
                rp_filter: read("rp_filter"),
                arp_ignore: read("arp_ignore"),
                arp_announce: read("arp_announce"),
            },
        );
    }
    Ok(obs)
}

/// Stop netd when a rule that isn't its own already has its priority.
pub fn check_priority(cfg: &Config, obs: &Observed) -> Result<(), OvsError> {
    match obs
        .rules
        .iter()
        .find(|r| r.priority == cfg.rule_priority && !r.ours())
    {
        Some(r) => Err(OvsError::conflict(format!(
            "source_routes.rule_priority {} is already used by the host's rule `{}`: choose another",
            cfg.rule_priority,
            r.describe()
        ))),
        None => Ok(()),
    }
}

/// One change to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    DelRule {
        v6: bool,
        src: String,
        table: String,
        priority: u32,
    },
    DelRoute {
        v6: bool,
        dst: String,
        dev: Option<String>,
        table: String,
    },
    AddRoute {
        net: IpNet,
        dev: String,
        src: IpAddr,
        table: u32,
    },
    AddRule {
        src: IpAddr,
        table: u32,
        priority: u32,
    },
    SetArp {
        ifname: String,
        key: &'static str,
        value: u8,
    },
}

fn family(v6: bool) -> &'static str {
    if v6 { "-6" } else { "-4" }
}

impl Action {
    pub fn cmd(&self) -> Cmd {
        let proto = PROTOCOL.to_string();
        let args: Vec<String> = match self {
            Action::DelRule {
                v6,
                src,
                table,
                priority,
            } => vec![
                family(*v6).into(),
                "rule".into(),
                "del".into(),
                "from".into(),
                src.clone(),
                "lookup".into(),
                table.clone(),
                "priority".into(),
                priority.to_string(),
                "protocol".into(),
                proto,
            ],
            Action::DelRoute {
                v6,
                dst,
                dev,
                table,
            } => {
                let mut a = vec![
                    family(*v6).into(),
                    "route".into(),
                    "del".into(),
                    dst.clone(),
                ];
                if let Some(d) = dev {
                    a.extend(["dev".into(), d.clone()]);
                }
                a.extend(["table".into(), table.clone(), "proto".into(), proto]);
                a
            }
            Action::AddRoute {
                net,
                dev,
                src,
                table,
            } => vec![
                family(net.is_v6()).into(),
                "route".into(),
                "replace".into(),
                net.to_string(),
                "dev".into(),
                dev.clone(),
                "src".into(),
                src.to_string(),
                "table".into(),
                table.to_string(),
                "proto".into(),
                proto,
            ],
            Action::AddRule {
                src,
                table,
                priority,
            } => vec![
                family(src.is_ipv6()).into(),
                "rule".into(),
                "add".into(),
                "from".into(),
                src.to_string(),
                "lookup".into(),
                table.to_string(),
                "priority".into(),
                priority.to_string(),
                "protocol".into(),
                proto,
            ],
            // The slash form: an IPoIB child (`ib0.8001`) has a dot in its name.
            Action::SetArp { ifname, key, value } => {
                return Cmd::new(
                    Program::Sysctl,
                    [
                        "-w".to_string(),
                        format!("net/ipv4/conf/{ifname}/{key}={value}"),
                    ],
                );
            }
        };
        Cmd::new(Program::Ip, args)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddrState {
    /// Its rule and its port's route are in place.
    Ok,
    /// Managed, not (yet) in place.
    Missing,
    /// In a subnet `source_routes.subnets` doesn't name.
    Unmanaged,
    /// Another manager's rule has this address; netd leaves it alone.
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddrReport {
    pub address: IpAddr,
    pub state: AddrState,
    /// The other manager's rule, for [`AddrState::Conflict`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortReport {
    pub ifname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<u32>,
    pub addresses: Vec<AddrReport>,
    #[serde(default)]
    pub sysctls: PortSysctls,
}

/// A subnet two or more ports have addresses in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubnetReport {
    pub subnet: IpNet,
    pub managed: bool,
    pub ports: Vec<PortReport>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub subnets: Vec<SubnetReport>,
    /// What netd couldn't do or did again (a full table range, a setting
    /// changed outside netd).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// What a pass changes, the state to keep, and the host as it stands
/// before the changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub state: State,
    pub report: Report,
}

/// Work out the changes; touches nothing.
pub fn plan(cfg: &Config, obs: &Observed, state: &State) -> Plan {
    let mut notes = Vec::new();

    // Subnets two or more ports share, by each address's own prefix.
    let mut shared: BTreeMap<IpNet, BTreeMap<String, Vec<IpAddr>>> = BTreeMap::new();
    for a in &obs.addrs {
        let Ok(net) = IpNet::new(a.ip, a.prefix) else {
            continue;
        };
        if net.is_host() {
            continue;
        }
        shared
            .entry(net)
            .or_default()
            .entry(a.ifname.clone())
            .or_default()
            .push(a.ip);
    }
    shared.retain(|_, ports| ports.len() > 1);

    // Another manager's rule for one address.
    let foreign_rule = |ip: &IpAddr| {
        obs.rules
            .iter()
            .filter(|r| !r.ours() && r.v6 == ip.is_ipv6())
            .filter(|r| {
                r.srclen
                    .is_none_or(|l| l == if ip.is_ipv6() { 128 } else { 32 })
            })
            .find(|r| r.src.as_deref().and_then(|s| s.parse::<IpAddr>().ok()) == Some(*ip))
            .map(Rule::describe)
    };

    // Ports that need a table: those with an address netd will manage.
    let mut wanted_ports: BTreeSet<&str> = BTreeSet::new();
    for (net, ports) in &shared {
        if !cfg.manages(net) {
            continue;
        }
        for (ifname, ips) in ports {
            if ips.iter().any(|ip| foreign_rule(ip).is_none()) {
                wanted_ports.insert(ifname);
            }
        }
    }

    // Tables: keep a port's number while it is in range and nobody else's.
    let [lo, hi] = cfg.table_range;
    let foreign_tables: BTreeSet<u32> = obs
        .routes
        .iter()
        .filter(|r| !is_ours(r.protocol.as_deref()))
        .filter_map(|r| r.table.parse().ok())
        .chain(
            obs.rules
                .iter()
                .filter(|r| !r.ours())
                .filter_map(|r| r.table.as_deref()?.parse().ok()),
        )
        .collect();
    let mut tables: BTreeMap<String, u32> = BTreeMap::new();
    for ifname in &wanted_ports {
        if let Some(&t) = state.tables.get(*ifname)
            && (lo..=hi).contains(&t)
            && !foreign_tables.contains(&t)
        {
            tables.insert(ifname.to_string(), t);
        }
    }
    for ifname in &wanted_ports {
        if tables.contains_key(*ifname) {
            continue;
        }
        let taken: BTreeSet<u32> = tables.values().copied().collect();
        match (lo..=hi).find(|t| !foreign_tables.contains(t) && !taken.contains(t)) {
            Some(t) => {
                tables.insert(ifname.to_string(), t);
            }
            None => notes.push(format!(
                "no free table in source_routes.table_range [{lo}, {hi}]: {ifname} gets no rules"
            )),
        }
    }

    // What should be there.
    let mut want_routes: Vec<(IpNet, String, IpAddr, u32)> = Vec::new();
    let mut want_rules: Vec<(IpAddr, u32)> = Vec::new();
    let mut report = Report::default();
    for (net, ports) in &shared {
        let managed = cfg.manages(net);
        let mut port_reports = Vec::new();
        for (ifname, ips) in ports {
            let table = if managed {
                tables.get(ifname).copied()
            } else {
                None
            };
            let free: Vec<IpAddr> = ips
                .iter()
                .copied()
                .filter(|ip| foreign_rule(ip).is_none())
                .collect();
            if let (Some(t), Some(src)) = (table, free.first()) {
                want_routes.push((*net, ifname.clone(), *src, t));
                want_rules.extend(free.iter().map(|ip| (*ip, t)));
            }
            let route_ok = |t: u32, src: &IpAddr| {
                obs.routes.iter().any(|r| {
                    is_ours(r.protocol.as_deref())
                        && r.dst == net.to_string()
                        && r.dev.as_deref() == Some(ifname)
                        && r.table == t.to_string()
                        && r.prefsrc.as_deref() == Some(&src.to_string())
                })
            };
            let addresses = ips
                .iter()
                .map(|ip| {
                    let (state, conflict) = if !managed {
                        (AddrState::Unmanaged, None)
                    } else if let Some(c) = foreign_rule(ip) {
                        (AddrState::Conflict, Some(c))
                    } else {
                        let ok = table.zip(free.first()).is_some_and(|(t, src)| {
                            route_ok(t, src)
                                && obs.rules.iter().any(|r| {
                                    r.ours()
                                        && r.priority == cfg.rule_priority
                                        && r.src.as_deref() == Some(&ip.to_string())
                                        && r.table.as_deref() == Some(&t.to_string())
                                })
                        });
                        (
                            if ok {
                                AddrState::Ok
                            } else {
                                AddrState::Missing
                            },
                            None,
                        )
                    };
                    AddrReport {
                        address: *ip,
                        state,
                        conflict,
                    }
                })
                .collect();
            port_reports.push(PortReport {
                ifname: ifname.clone(),
                table,
                addresses,
                sysctls: obs.sysctls.get(ifname).copied().unwrap_or_default(),
            });
        }
        report.subnets.push(SubnetReport {
            subnet: *net,
            managed,
            ports: port_reports,
        });
    }

    // Rules first out, then routes; routes in before the rules that use them.
    let mut actions = Vec::new();
    for r in obs.rules.iter().filter(|r| r.ours()) {
        let wanted = want_rules.iter().any(|(ip, t)| {
            r.priority == cfg.rule_priority
                && r.src.as_deref() == Some(&ip.to_string())
                && r.table.as_deref() == Some(&t.to_string())
        });
        if !wanted && let (Some(src), Some(table)) = (&r.src, &r.table) {
            actions.push(Action::DelRule {
                v6: r.v6,
                src: src.clone(),
                table: table.clone(),
                priority: r.priority,
            });
        }
    }
    for r in obs.routes.iter().filter(|r| is_ours(r.protocol.as_deref())) {
        let wanted = want_routes.iter().any(|(net, dev, src, t)| {
            r.dst == net.to_string()
                && r.dev.as_deref() == Some(dev)
                && r.table == t.to_string()
                && r.prefsrc.as_deref() == Some(&src.to_string())
        });
        if !wanted {
            actions.push(Action::DelRoute {
                v6: r.v6,
                dst: r.dst.clone(),
                dev: r.dev.clone(),
                table: r.table.clone(),
            });
        }
    }
    for (net, dev, src, table) in &want_routes {
        let present = obs.routes.iter().any(|r| {
            is_ours(r.protocol.as_deref())
                && r.dst == net.to_string()
                && r.dev.as_deref() == Some(dev)
                && r.table == table.to_string()
                && r.prefsrc.as_deref() == Some(&src.to_string())
        });
        if !present {
            actions.push(Action::AddRoute {
                net: *net,
                dev: dev.clone(),
                src: *src,
                table: *table,
            });
        }
    }
    for (src, table) in &want_rules {
        let present = obs.rules.iter().any(|r| {
            r.ours()
                && r.priority == cfg.rule_priority
                && r.src.as_deref() == Some(&src.to_string())
                && r.table.as_deref() == Some(&table.to_string())
        });
        if !present {
            actions.push(Action::AddRule {
                src: *src,
                table: *table,
                priority: cfg.rule_priority,
            });
        }
    }

    // ARP settings on the ports of managed IPv4 subnets (decision 17).
    let arp_ports: BTreeSet<&str> = want_rules
        .iter()
        .filter(|(ip, _)| ip.is_ipv4())
        .filter_map(|(ip, _)| {
            obs.addrs
                .iter()
                .find(|a| a.ip == *ip)
                .map(|a| a.ifname.as_str())
        })
        .collect();
    let mut arp_saved = BTreeMap::new();
    for ifname in &arp_ports {
        let now = obs.sysctls.get(*ifname).copied().unwrap_or_default();
        let saved = state.arp_saved.get(*ifname).copied();
        let current = Arp {
            arp_ignore: now.arp_ignore.unwrap_or(0),
            arp_announce: now.arp_announce.unwrap_or(0),
        };
        arp_saved.insert(ifname.to_string(), saved.unwrap_or(current));
        for (key, have, want) in [
            ("arp_ignore", current.arp_ignore, MANAGED_ARP.arp_ignore),
            (
                "arp_announce",
                current.arp_announce,
                MANAGED_ARP.arp_announce,
            ),
        ] {
            if have != want {
                if saved.is_some() {
                    notes.push(format!(
                        "{key} on {ifname} was changed to {have} outside netd: set to {want} again"
                    ));
                }
                actions.push(Action::SetArp {
                    ifname: ifname.to_string(),
                    key,
                    value: want,
                });
            }
        }
    }
    for (ifname, saved) in &state.arp_saved {
        if arp_ports.contains(ifname.as_str()) || !obs.links.contains(ifname) {
            continue;
        }
        let now = obs.sysctls.get(ifname).copied().unwrap_or_default();
        for (key, have, back) in [
            ("arp_ignore", now.arp_ignore, saved.arp_ignore),
            ("arp_announce", now.arp_announce, saved.arp_announce),
        ] {
            if have != Some(back) {
                actions.push(Action::SetArp {
                    ifname: ifname.clone(),
                    key,
                    value: back,
                });
            }
        }
    }

    report.notes = notes;
    Plan {
        actions,
        state: State { tables, arp_saved },
        report,
    }
}

/// Run a plan's actions, every one even when some fail; the failures, as
/// `command: error`.
pub fn apply(exec: &dyn Exec, actions: &[Action]) -> Vec<String> {
    actions
        .iter()
        .filter_map(|a| {
            let cmd = a.cmd();
            exec.check(&cmd)
                .err()
                .map(|e| format!("{}: {e}", cmd.display()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    fn addr(ifname: &str, cidr: &str) -> Addr {
        let (ip, p) = cidr.split_once('/').unwrap();
        Addr {
            ifname: ifname.into(),
            ip: ip.parse().unwrap(),
            prefix: p.parse().unwrap(),
        }
    }

    fn host(addrs: &[(&str, &str)]) -> Observed {
        let addrs: Vec<Addr> = addrs.iter().map(|(i, c)| addr(i, c)).collect();
        Observed {
            links: addrs.iter().map(|a| a.ifname.clone()).collect(),
            sysctls: addrs
                .iter()
                .map(|a| {
                    (
                        a.ifname.clone(),
                        PortSysctls {
                            rp_filter: Some(1),
                            arp_ignore: Some(0),
                            arp_announce: Some(0),
                        },
                    )
                })
                .collect(),
            addrs,
            ..Default::default()
        }
    }

    fn managing(subnets: &[&str]) -> Config {
        Config {
            subnets: subnets.iter().map(|s| s.parse().unwrap()).collect(),
            ..Default::default()
        }
    }

    fn cmds(p: &Plan) -> Vec<String> {
        p.actions.iter().map(|a| a.cmd().display()).collect()
    }

    /// The host after `p` ran, as `ip` would show it.
    fn after(obs: &Observed, p: &Plan) -> Observed {
        let mut o = obs.clone();
        for a in &p.actions {
            match a {
                Action::DelRule {
                    src,
                    table,
                    priority,
                    ..
                } => o.rules.retain(|r| {
                    !(r.ours()
                        && r.src.as_deref() == Some(src)
                        && r.table.as_deref() == Some(table)
                        && r.priority == *priority)
                }),
                Action::DelRoute {
                    dst, dev, table, ..
                } => o.routes.retain(|r| {
                    !(is_ours(r.protocol.as_deref())
                        && &r.dst == dst
                        && &r.dev == dev
                        && &r.table == table)
                }),
                Action::AddRoute {
                    net,
                    dev,
                    src,
                    table,
                } => o.routes.push(Route {
                    v6: net.is_v6(),
                    dst: net.to_string(),
                    dev: Some(dev.clone()),
                    table: table.to_string(),
                    protocol: Some(PROTOCOL.to_string()),
                    prefsrc: Some(src.to_string()),
                }),
                Action::AddRule {
                    src,
                    table,
                    priority,
                } => o.rules.push(Rule {
                    v6: src.is_ipv6(),
                    priority: *priority,
                    src: Some(src.to_string()),
                    srclen: None,
                    table: Some(table.to_string()),
                    protocol: Some(PROTOCOL.to_string()),
                }),
                Action::SetArp { ifname, key, value } => {
                    let s = o.sysctls.entry(ifname.clone()).or_default();
                    match *key {
                        "arp_ignore" => s.arp_ignore = Some(*value),
                        _ => s.arp_announce = Some(*value),
                    }
                }
            }
        }
        o
    }

    #[test]
    fn subnets_parse_and_contain() {
        let n: IpNet = "10.40.1.7/16".parse().unwrap();
        assert_eq!(n.to_string(), "10.40.0.0/16");
        assert!(n.contains("10.40.200.1".parse().unwrap()));
        assert!(!n.contains("10.41.0.1".parse().unwrap()));
        assert!(n.contains_net(&"10.40.1.0/24".parse().unwrap()));
        assert!(!n.contains_net(&"10.0.0.0/8".parse().unwrap()));
        let v6: IpNet = "fd00::5/64".parse().unwrap();
        assert_eq!(v6.to_string(), "fd00::/64");
        assert!(!v6.contains("10.40.0.1".parse().unwrap()));
        assert!("10.40.0.0".parse::<IpNet>().is_err());
        assert!("10.40.0.0/33".parse::<IpNet>().is_err());
        let cfg: Config =
            serde_json::from_str(r#"{"subnets": ["10.40.0.0/16", "fd00::/48"]}"#).unwrap();
        assert_eq!(cfg.rule_priority, 1000);
        assert_eq!(cfg.table_range, [4200, 4299]);
        assert!(serde_json::from_str::<Config>(r#"{"subnet": []}"#).is_err());
    }

    #[test]
    fn config_is_checked() {
        assert!(Config::default().validate().is_ok());
        for bad in [
            Config {
                table_range: [200, 300],
                ..Default::default()
            },
            Config {
                table_range: [255, 255],
                ..Default::default()
            },
            Config {
                table_range: [10, 5],
                ..Default::default()
            },
            Config {
                rule_priority: 32766,
                ..Default::default()
            },
            Config {
                rule_priority: 0,
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert!(
            Config {
                table_range: [100, 252],
                ..Default::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn one_port_in_a_subnet_gets_nothing() {
        let obs = host(&[("ib0", "10.40.1.5/24"), ("eth0", "192.168.1.2/24")]);
        let p = plan(&managing(&["10.40.0.0/16"]), &obs, &State::default());
        assert!(p.actions.is_empty(), "{:?}", cmds(&p));
        assert!(p.report.subnets.is_empty());
        assert_eq!(p.state, State::default());
    }

    #[test]
    fn two_ports_get_a_table_and_a_rule_each() {
        let obs = host(&[
            ("ib0", "10.40.1.5/24"),
            ("ib1", "10.40.1.6/24"),
            ("eth0", "192.168.1.2/24"),
        ]);
        let cfg = managing(&["10.40.0.0/16"]);
        let p = plan(&cfg, &obs, &State::default());
        assert_eq!(
            cmds(&p),
            vec![
                "ip -4 route replace 10.40.1.0/24 dev ib0 src 10.40.1.5 table 4200 proto 77",
                "ip -4 route replace 10.40.1.0/24 dev ib1 src 10.40.1.6 table 4201 proto 77",
                "ip -4 rule add from 10.40.1.5 lookup 4200 priority 1000 protocol 77",
                "ip -4 rule add from 10.40.1.6 lookup 4201 priority 1000 protocol 77",
                "sysctl -w net/ipv4/conf/ib0/arp_ignore=1",
                "sysctl -w net/ipv4/conf/ib0/arp_announce=2",
                "sysctl -w net/ipv4/conf/ib1/arp_ignore=1",
                "sysctl -w net/ipv4/conf/ib1/arp_announce=2",
            ]
        );
        assert_eq!(
            p.state.tables,
            BTreeMap::from([("ib0".into(), 4200), ("ib1".into(), 4201)])
        );
        let before = Arp {
            arp_ignore: 0,
            arp_announce: 0,
        };
        assert_eq!(
            p.state.arp_saved,
            BTreeMap::from([("ib0".into(), before), ("ib1".into(), before)])
        );
        let s = &p.report.subnets[0];
        assert_eq!(s.subnet.to_string(), "10.40.1.0/24");
        assert!(s.managed);
        assert!(
            s.ports
                .iter()
                .all(|port| port.addresses[0].state == AddrState::Missing)
        );

        // Applied, a second pass changes nothing and reports them in place.
        let done = after(&obs, &p);
        let again = plan(&cfg, &done, &p.state);
        assert!(again.actions.is_empty(), "{:?}", cmds(&again));
        assert_eq!(again.state, p.state);
        assert!(again.report.subnets[0].ports.iter().all(|port| {
            port.addresses[0].state == AddrState::Ok
                && port.sysctls.arp_ignore == Some(1)
                && port.sysctls.arp_announce == Some(2)
        }));
        assert!(again.report.notes.is_empty());
    }

    #[test]
    fn a_removed_address_loses_its_rule_and_its_arp_settings() {
        let obs = host(&[
            ("ib0", "10.40.1.5/24"),
            ("ib1", "10.40.1.6/24"),
            ("ib2", "10.40.1.7/24"),
        ]);
        let cfg = managing(&["10.40.0.0/16"]);
        let first = plan(&cfg, &obs, &State::default());
        let mut done = after(&obs, &first);
        // ib2 loses its address; its link stays.
        done.addrs.retain(|a| a.ifname != "ib2");
        let p = plan(&cfg, &done, &first.state);
        assert_eq!(
            cmds(&p),
            vec![
                "ip -4 rule del from 10.40.1.7 lookup 4202 priority 1000 protocol 77",
                "ip -4 route del 10.40.1.0/24 dev ib2 table 4202 proto 77",
                "sysctl -w net/ipv4/conf/ib2/arp_ignore=0",
                "sysctl -w net/ipv4/conf/ib2/arp_announce=0",
            ]
        );
        assert!(!p.state.tables.contains_key("ib2"));
        assert!(!p.state.arp_saved.contains_key("ib2"));

        // A port that is gone is forgotten without a command.
        let mut gone = after(&obs, &first);
        gone.addrs.retain(|a| a.ifname != "ib2");
        gone.links.remove("ib2");
        gone.sysctls.remove("ib2");
        let p = plan(&cfg, &gone, &first.state);
        assert!(!cmds(&p).iter().any(|c| c.starts_with("sysctl")));
        assert!(!p.state.arp_saved.contains_key("ib2"));
    }

    #[test]
    fn a_subnet_dropped_from_the_config_is_cleaned_up() {
        let obs = host(&[("ib0", "10.40.1.5/24"), ("ib1", "10.40.1.6/24")]);
        let first = plan(&managing(&["10.40.0.0/16"]), &obs, &State::default());
        let done = after(&obs, &first);
        let p = plan(&managing(&[]), &done, &first.state);
        assert_eq!(p.actions.len(), 8, "{:?}", cmds(&p));
        assert_eq!(p.state, State::default());
        let s = &p.report.subnets[0];
        assert!(!s.managed);
        assert!(
            s.ports
                .iter()
                .all(|port| port.addresses[0].state == AddrState::Unmanaged)
        );
        let clean = plan(&managing(&[]), &after(&done, &p), &p.state);
        assert!(clean.actions.is_empty(), "{:?}", cmds(&clean));
    }

    #[test]
    fn unmanaged_subnets_are_reported_and_left_alone() {
        let obs = host(&[("ib0", "10.40.1.5/24"), ("ib1", "10.40.1.6/24")]);
        let p = plan(&managing(&["10.50.0.0/16"]), &obs, &State::default());
        assert!(p.actions.is_empty());
        assert_eq!(p.report.subnets.len(), 1);
        assert!(!p.report.subnets[0].managed);
    }

    #[test]
    fn another_managers_rule_is_a_conflict_and_left_alone() {
        let mut obs = host(&[("ib0", "10.40.1.5/24"), ("ib1", "10.40.1.6/24")]);
        obs.rules.push(Rule {
            v6: false,
            priority: 100,
            src: Some("10.40.1.6".into()),
            srclen: None,
            table: Some("101".into()),
            protocol: Some("static".into()),
        });
        let p = plan(&managing(&["10.40.0.0/16"]), &obs, &State::default());
        assert!(
            !cmds(&p)
                .iter()
                .any(|c| c.contains("10.40.1.6") || c.contains("ib1")),
            "{:?}",
            cmds(&p)
        );
        let ib1 = &p.report.subnets[0].ports[1];
        assert_eq!(ib1.addresses[0].state, AddrState::Conflict);
        assert_eq!(
            ib1.addresses[0].conflict.as_deref(),
            Some("priority 100 from 10.40.1.6 lookup 101 protocol static")
        );
        assert_eq!(p.state.tables, BTreeMap::from([("ib0".into(), 4200)]));
    }

    #[test]
    fn a_table_with_someone_elses_routes_is_skipped() {
        let mut obs = host(&[("ib0", "10.40.1.5/24"), ("ib1", "10.40.1.6/24")]);
        obs.routes.push(Route {
            v6: false,
            dst: "default".into(),
            dev: Some("eth0".into()),
            table: "4200".into(),
            protocol: Some("boot".into()),
            prefsrc: None,
        });
        // A stored number that someone else now uses is given up too.
        let state = State {
            tables: BTreeMap::from([("ib0".into(), 4200)]),
            ..Default::default()
        };
        let p = plan(&managing(&["10.40.0.0/16"]), &obs, &state);
        assert_eq!(
            p.state.tables,
            BTreeMap::from([("ib0".into(), 4201), ("ib1".into(), 4202)])
        );
    }

    #[test]
    fn stored_tables_are_kept_and_a_full_range_is_reported() {
        let obs = host(&[
            ("ib0", "10.40.1.5/24"),
            ("ib1", "10.40.1.6/24"),
            ("ib2", "10.40.1.7/24"),
        ]);
        let mut cfg = managing(&["10.40.0.0/16"]);
        cfg.table_range = [4200, 4201];
        let state = State {
            tables: BTreeMap::from([("ib1".into(), 4200)]),
            ..Default::default()
        };
        let p = plan(&cfg, &obs, &state);
        assert_eq!(
            p.state.tables,
            BTreeMap::from([("ib0".into(), 4201), ("ib1".into(), 4200)])
        );
        assert_eq!(
            p.report.notes,
            vec!["no free table in source_routes.table_range [4200, 4201]: ib2 gets no rules"]
        );
        assert!(
            !cmds(&p)
                .iter()
                .any(|c| c.contains("ib2") || c.contains("10.40.1.7"))
        );
    }

    #[test]
    fn a_priority_someone_else_uses_stops_netd() {
        let mut obs = Observed::default();
        obs.rules.push(Rule {
            v6: false,
            priority: 1000,
            src: None,
            srclen: None,
            table: Some("main".into()),
            protocol: None,
        });
        let err = check_priority(&Config::default(), &obs).unwrap_err();
        assert!(
            err.to_string()
                .contains("priority 1000 from all lookup main")
        );
        obs.rules[0].protocol = Some(PROTOCOL.to_string());
        assert!(check_priority(&Config::default(), &obs).is_ok());
    }

    #[test]
    fn ipv6_gets_ip_dash_6_and_no_arp_settings() {
        let obs = host(&[("ib0", "fd00::5/64"), ("ib1", "fd00::6/64")]);
        let p = plan(&managing(&["fd00::/48"]), &obs, &State::default());
        assert_eq!(
            cmds(&p),
            vec![
                "ip -6 route replace fd00::/64 dev ib0 src fd00::5 table 4200 proto 77",
                "ip -6 route replace fd00::/64 dev ib1 src fd00::6 table 4201 proto 77",
                "ip -6 rule add from fd00::5 lookup 4200 priority 1000 protocol 77",
                "ip -6 rule add from fd00::6 lookup 4201 priority 1000 protocol 77",
            ]
        );
        assert!(p.state.arp_saved.is_empty());
    }

    #[test]
    fn arp_settings_changed_outside_netd_are_set_again() {
        let obs = host(&[("ib0", "10.40.1.5/24"), ("ib1", "10.40.1.6/24")]);
        let cfg = managing(&["10.40.0.0/16"]);
        let first = plan(&cfg, &obs, &State::default());
        let mut done = after(&obs, &first);
        done.sysctls.get_mut("ib0").unwrap().arp_announce = Some(0);
        let p = plan(&cfg, &done, &first.state);
        assert_eq!(cmds(&p), vec!["sysctl -w net/ipv4/conf/ib0/arp_announce=2"]);
        assert_eq!(
            p.report.notes,
            vec!["arp_announce on ib0 was changed to 0 outside netd: set to 2 again"]
        );
        // The saved values stay the first ones.
        assert_eq!(p.state.arp_saved, first.state.arp_saved);
    }

    #[test]
    fn an_ipoib_child_uses_the_slash_form() {
        let a = Action::SetArp {
            ifname: "ib0.8001".into(),
            key: "arp_ignore",
            value: 1,
        };
        assert_eq!(
            a.cmd().display(),
            "sysctl -w net/ipv4/conf/ib0.8001/arp_ignore=1"
        );
    }

    #[test]
    fn observe_reads_ip_json_and_sysctls() {
        let exec = RecordingExec::new();
        exec.on(
            "ip -j addr show",
            Output::ok(
                r#"[{"ifname":"lo","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host"}]},
                    {"ifname":"ib0","addr_info":[{"family":"inet","local":"10.40.1.5","prefixlen":24,"scope":"global"},
                                                 {"family":"inet6","local":"fe80::1","prefixlen":64,"scope":"link"}]},
                    {"ifname":"ib1","addr_info":[]}]"#,
            ),
        );
        exec.on(
            "ip -4 -j rule show",
            Output::ok(
                r#"[{"priority":0,"src":"all","table":"local"},
                    {"priority":1000,"src":"10.40.1.5","table":"4200","protocol":"77"},
                    {"priority":1001,"src":"10.40.0.0","srclen":16,"table":"7"}]"#,
            ),
        );
        exec.on("ip -6 -j rule show", Output::ok("[]"));
        exec.on(
            "ip -4 -j route show table all",
            Output::ok(
                r#"[{"dst":"10.40.1.0/24","dev":"ib0","table":"4200","protocol":"glidex","prefsrc":"10.40.1.5"},
                    {"dst":"default","gateway":"10.0.0.1","dev":"eth0"}]"#,
            ),
        );
        exec.on("ip -6 -j route show table all", Output::ok("[]"));
        exec.file("/proc/sys/net/ipv4/conf/ib0/rp_filter", "1\n");
        exec.file("/proc/sys/net/ipv4/conf/ib0/arp_ignore", "0\n");
        exec.file("/proc/sys/net/ipv4/conf/ib0/arp_announce", "2\n");
        let obs = observe(&exec).unwrap();
        assert_eq!(
            obs.links,
            BTreeSet::from(["lo".into(), "ib0".into(), "ib1".into()])
        );
        assert_eq!(obs.addrs, vec![addr("ib0", "10.40.1.5/24")]);
        assert!(obs.rules[1].ours());
        assert_eq!(obs.rules[2].srclen, Some(16));
        assert_eq!(obs.rules[0].src, None);
        assert!(is_ours(obs.routes[0].protocol.as_deref()));
        assert_eq!(obs.routes[1].table, "main");
        assert_eq!(
            obs.sysctls["ib0"],
            PortSysctls {
                rp_filter: Some(1),
                arp_ignore: Some(0),
                arp_announce: Some(2),
            }
        );
    }

    #[test]
    fn apply_runs_every_action_and_returns_failures() {
        let exec = RecordingExec::new();
        exec.on(
            "ip -4 rule add",
            Output::failed(2, "RTNETLINK answers: File exists"),
        );
        let actions = vec![
            Action::AddRoute {
                net: "10.40.1.0/24".parse().unwrap(),
                dev: "ib0".into(),
                src: "10.40.1.5".parse().unwrap(),
                table: 4200,
            },
            Action::AddRule {
                src: "10.40.1.5".parse().unwrap(),
                table: 4200,
                priority: 1000,
            },
            Action::SetArp {
                ifname: "ib0".into(),
                key: "arp_ignore",
                value: 1,
            },
        ];
        let errors = apply(&exec, &actions);
        assert_eq!(exec.calls().len(), 3);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].starts_with("ip -4 rule add from 10.40.1.5"),
            "{errors:?}"
        );
    }

    /// Real routing, inside a network namespace of its own: two veth ports
    /// in one subnet, bridged to a peer namespace, with strict reverse-path
    /// filtering: each address answers out of its own port, and without its
    /// rule the second address's traffic is dropped. As root:
    ///
    /// ```text
    /// ip netns add gxsr && ip netns exec gxsr <test binary> --ignored in_a_network_namespace; ip netns del gxsr
    /// ```
    #[test]
    #[ignore = "needs root, inside a network namespace"]
    fn in_a_network_namespace() {
        let ns = |pid: &str| std::fs::read_link(format!("/proc/{pid}/ns/net")).ok();
        assert_ne!(
            ns("self"),
            ns("1"),
            "run this inside a network namespace, never on the host"
        );
        let exec = crate::SystemExec::default();
        let ip = |args: &[&str]| {
            exec.check(&Cmd::new(Program::Ip, args.iter().copied()))
                .unwrap()
        };
        let peer = format!("gxsr-peer-{}", std::process::id());
        struct Peer(String);
        impl Drop for Peer {
            fn drop(&mut self) {
                let _ = std::process::Command::new("ip")
                    .args(["netns", "del", &self.0])
                    .status();
            }
        }
        ip(&["netns", "add", &peer]);
        let _peer = Peer(peer.clone());
        let in_peer = |args: &[&str]| {
            let mut a = vec!["netns", "exec", peer.as_str()];
            a.extend_from_slice(args);
            exec.run(&Cmd::new(Program::Ip, a)).unwrap()
        };
        ip(&["link", "set", "lo", "up"]);
        for (port, far, addr) in [("a0", "b0", "10.40.1.5/24"), ("a1", "b1", "10.40.1.6/24")] {
            ip(&[
                "link", "add", port, "type", "veth", "peer", "name", far, "netns", &peer,
            ]);
            ip(&["addr", "add", addr, "dev", port]);
            ip(&["link", "set", port, "up"]);
        }
        // `name`: ip reads a bare `br` as `broadcast`.
        for args in [
            &["link", "add", "name", "gxsrbr", "type", "bridge"][..],
            &["link", "set", "b0", "master", "gxsrbr", "up"],
            &["link", "set", "b1", "master", "gxsrbr", "up"],
            &["addr", "add", "10.40.1.100/24", "dev", "gxsrbr"],
            &["link", "set", "gxsrbr", "up"],
        ] {
            let mut a = vec!["-n", peer.as_str()];
            a.extend_from_slice(args);
            ip(&a);
        }
        // Known ARP settings to restore: a namespace inherits the host's defaults.
        // The kernel filters by the higher of `all` and the port's own value,
        // and both start at the host's (often 2, loose): strict means `all` 0
        // and the ports 1.
        for set in [
            "net/ipv4/conf/all/rp_filter=0",
            "net/ipv4/conf/a0/rp_filter=1",
            "net/ipv4/conf/a1/rp_filter=1",
            "net/ipv4/conf/a0/arp_ignore=0",
            "net/ipv4/conf/a1/arp_ignore=0",
            "net/ipv4/conf/a0/arp_announce=0",
            "net/ipv4/conf/a1/arp_announce=0",
        ] {
            exec.check(&Cmd::new(Program::Sysctl, ["-w", set])).unwrap();
        }

        let cfg = managing(&["10.40.0.0/16"]);
        let obs = observe(&exec).unwrap();
        check_priority(&cfg, &obs).unwrap();
        let p = plan(&cfg, &obs, &State::default());
        assert_eq!(apply(&exec, &p.actions), Vec::<String>::new());

        let arp = |port: &str, key: &str| {
            std::fs::read_to_string(format!("/proc/sys/net/ipv4/conf/{port}/{key}")).unwrap()
        };
        assert_eq!(
            (
                arp("a1", "arp_ignore").trim(),
                arp("a1", "arp_announce").trim()
            ),
            ("1", "2")
        );

        // Each address answers from its own port, and both answer.
        for (from, dev) in [("10.40.1.5", "dev a0"), ("10.40.1.6", "dev a1")] {
            let out = ip(&["-4", "route", "get", "10.40.1.100", "from", from]).stdout_str();
            assert!(out.contains(dev), "from {from}: {out}");
            let ping = in_peer(&["ping", "-c", "2", "-W", "2", from]);
            assert_eq!(ping.status, 0, "ping {from}: {}", ping.stdout_str());
        }
        let obs = observe(&exec).unwrap();
        let again = plan(&cfg, &obs, &p.state);
        assert!(again.actions.is_empty(), "{:?}", cmds(&again));
        assert!(
            again.report.subnets[0]
                .ports
                .iter()
                .all(|port| port.addresses[0].state == AddrState::Ok)
        );

        // Without its rule, .6 answers out of a0, the main table's first
        // route to the subnet, and strict filtering drops what reaches it on
        // a1; the next pass puts the rule back.
        ip(&["-4", "rule", "del", "from", "10.40.1.6", "priority", "1000"]);
        let out = ip(&["-4", "route", "get", "10.40.1.100", "from", "10.40.1.6"]).stdout_str();
        assert!(out.contains("dev a0"), "without the rule: {out}");
        let ping = in_peer(&["ping", "-c", "2", "-W", "1", "10.40.1.6"]);
        assert_ne!(ping.status, 0, "without the rule: {}", ping.stdout_str());
        let p = plan(&cfg, &observe(&exec).unwrap(), &again.state);
        assert_eq!(
            cmds(&p),
            vec!["ip -4 rule add from 10.40.1.6 lookup 4201 priority 1000 protocol 77"]
        );
        assert_eq!(apply(&exec, &p.actions), Vec::<String>::new());
        let out = ip(&["-4", "route", "get", "10.40.1.100", "from", "10.40.1.6"]).stdout_str();
        assert!(out.contains("dev a1"), "with the rule again: {out}");
        let ping = in_peer(&["ping", "-c", "2", "-W", "2", "10.40.1.6"]);
        assert_eq!(ping.status, 0, "with the rule again: {}", ping.stdout_str());

        // An address that goes loses its rule, and its port its ARP settings.
        ip(&["addr", "del", "10.40.1.6/24", "dev", "a1"]);
        let obs = observe(&exec).unwrap();
        let p = plan(&cfg, &obs, &again.state);
        assert_eq!(apply(&exec, &p.actions), Vec::<String>::new());
        let rules = ip(&["-4", "-j", "rule", "show"]).stdout_str();
        assert!(
            !rules.contains("10.40.1.6") && !rules.contains("10.40.1.5"),
            "{rules}"
        );
        assert_eq!(
            (
                arp("a1", "arp_ignore").trim(),
                arp("a1", "arp_announce").trim()
            ),
            ("0", "0")
        );
    }

    #[test]
    fn the_protocol_file_names_glidex() {
        assert_eq!(rt_protos_conf(), "77\tglidex\n");
    }
}
