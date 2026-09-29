//! Read-only host capability probe.
//!
//! `probe` reports host facts only (no glidex objects), which is why netd
//! serves it on the world-readable status socket (spec decision 7).

use crate::exec::{Cmd, Exec, Program};
use crate::vsctl;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Oldest supported Open vSwitch (spec §6.2).
pub const MIN_OVS_VERSION: (u32, u32) = (3, 0);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hugepages {
    pub size_kb: u64,
    pub total: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Combination {
    /// `A`–`D` from spec §2.
    pub id: String,
    pub description: String,
    pub available: bool,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCapabilities {
    pub ovs_installed: bool,
    pub ovs_version: Option<String>,
    pub ovs_version_supported: bool,
    pub ovs_running: bool,
    pub iface_types: Vec<String>,
    pub datapath_types: Vec<String>,
    /// ovs-vswitchd is built with DPDK (the dpdk profile's binary). The
    /// DPDK port types only appear in `iface_types` after `init_dpdk`.
    pub ovs_dpdk_build: bool,
    /// DPDK's ring mempool driver is installed (Debian/Ubuntu ship it as
    /// a separate, only-recommended package; without it every mempool
    /// fails with EINVAL). `None` where we can't tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpdk_mempool_driver: Option<bool>,
    pub dpdk_initialized: bool,
    pub hugepages: Vec<Hugepages>,
    pub iommu: bool,
    /// `None` when the Cloud Hypervisor binary wasn't found.
    pub ch_net_admin: Option<bool>,
    pub dnsmasq: bool,
    pub nft: bool,
    pub ip_forward: bool,
    pub firewalls: Vec<String>,
    pub network_managers: Vec<String>,
    pub combinations: Vec<Combination>,
}

#[derive(Debug, Clone, Default)]
pub struct ProbeOptions {
    pub ch_binary: Option<PathBuf>,
}

fn ok(exec: &dyn Exec, cmd: Cmd) -> Option<String> {
    let out = exec.run(&cmd.timeout(Duration::from_secs(5))).ok()?;
    (out.status == 0).then(|| out.stdout_str())
}

fn active_units(exec: &dyn Exec, units: &[&str]) -> Vec<String> {
    units
        .iter()
        .filter(|u| {
            ok(exec, Cmd::new(Program::Systemctl, ["is-active", "--quiet", u])).is_some()
        })
        .map(|u| u.to_string())
        .collect()
}

/// Debian/Ubuntu: is a `librte-mempool-ring*` package installed?
fn dpdk_mempool_driver(exec: &dyn Exec) -> Option<bool> {
    let out = exec
        .run(&Cmd::new(Program::DpkgQuery, ["-W", "-f=${Status}\\n", "librte-mempool-ring*"]).timeout(Duration::from_secs(5)))
        .ok()?;
    if out.status != 0 && out.stdout.is_empty() {
        // dpkg-query missing (not Debian) or no such package known.
        return if exec.exists(Path::new("/var/lib/dpkg/status")) { Some(false) } else { None };
    }
    Some(out.stdout_str().lines().any(|l| l.trim() == "install ok installed"))
}

/// "ovs-vsctl (Open vSwitch) 3.7.1" → "3.7.1".
pub fn parse_ovs_version(output: &str) -> Option<String> {
    output
        .lines()
        .next()?
        .split_whitespace()
        .last()
        .filter(|v| v.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

pub fn version_at_least(version: &str, min: (u32, u32)) -> bool {
    let mut parts = version.split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse::<u32>().ok());
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    (major, minor) >= min
}

pub fn probe(exec: &dyn Exec, opts: &ProbeOptions) -> HostCapabilities {
    let mut caps = HostCapabilities::default();

    if let Some(v) = ok(exec, Cmd::new(Program::OvsVsctl, ["--version"])) {
        caps.ovs_installed = true;
        caps.ovs_version = parse_ovs_version(&v);
        caps.ovs_version_supported = caps
            .ovs_version
            .as_deref()
            .is_some_and(|v| version_at_least(v, MIN_OVS_VERSION));
    }
    if caps.ovs_installed {
        caps.ovs_dpdk_build = ok(exec, Cmd::new(Program::OvsVswitchd, ["--version"]))
            .is_some_and(|v| v.lines().any(|l| l.trim_start().starts_with("DPDK")));
        if caps.ovs_dpdk_build {
            caps.dpdk_mempool_driver = dpdk_mempool_driver(exec);
        }
    }
    if caps.ovs_installed
        && ok(exec, Cmd::new(Program::OvsVsctl, ["--timeout=2", "show"])).is_some()
    {
        caps.ovs_running = true;
        if let Ok(rows) = vsctl::list(
            exec,
            "Open_vSwitch",
            &["iface_types", "datapath_types", "dpdk_initialized"],
        ) {
            if let Some(row) = rows.first() {
                caps.iface_types = row.set("iface_types");
                caps.datapath_types = row.set("datapath_types");
                caps.dpdk_initialized = row.bool("dpdk_initialized").unwrap_or(false);
            }
        }
    }

    for size_kb in [2048u64, 1_048_576] {
        let dir = PathBuf::from(format!("/sys/kernel/mm/hugepages/hugepages-{}kB", size_kb));
        let read = |f: &str| {
            exec.read_file(&dir.join(f))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
        };
        if let Some(total) = read("nr_hugepages") {
            caps.hugepages.push(Hugepages {
                size_kb,
                total,
                free: read("free_hugepages").unwrap_or(0),
            });
        }
    }
    caps.iommu = exec.exists(Path::new("/sys/kernel/iommu_groups/0"));

    caps.ch_net_admin = opts.ch_binary.as_ref().map(|bin| {
        ok(exec, Cmd::new(Program::Getcap, [bin.to_string_lossy().to_string()]))
            .is_some_and(|out| out.contains("cap_net_admin"))
    });
    caps.dnsmasq = ok(exec, Cmd::new(Program::Dnsmasq, ["--version"])).is_some();
    caps.nft = ok(exec, Cmd::new(Program::Nft, ["--version"])).is_some();
    caps.ip_forward = exec
        .read_file(Path::new("/proc/sys/net/ipv4/ip_forward"))
        .map(|s| s.trim() == "1")
        .unwrap_or(false);
    caps.firewalls = active_units(exec, &["ufw", "firewalld"]);
    caps.network_managers = active_units(exec, &["systemd-networkd", "NetworkManager"]);

    caps.combinations = combinations(&caps);
    caps
}

fn combinations(caps: &HostCapabilities) -> Vec<Combination> {
    let base = |missing: &mut Vec<String>| {
        if !caps.ovs_installed {
            missing.push("openvswitch".into());
        } else {
            if !caps.ovs_version_supported {
                missing.push(format!(
                    "openvswitch >= {}.{}",
                    MIN_OVS_VERSION.0, MIN_OVS_VERSION.1
                ));
            }
            if !caps.ovs_running {
                missing.push("ovs-vswitchd running".into());
            }
        }
    };
    let has = |t: &str| caps.iface_types.iter().any(|x| x == t);
    let mut out = Vec::new();

    let mut a = Vec::new();
    base(&mut a);
    if caps.ch_net_admin == Some(false) {
        a.push("cap_net_admin on cloud-hypervisor".into());
    }
    let mut b = Vec::new();
    base(&mut b);
    if caps.ovs_installed && !caps.ovs_dpdk_build {
        b.push("dpdk build of ovs-vswitchd (install profile dpdk)".into());
    } else if caps.dpdk_mempool_driver == Some(false) {
        b.push("DPDK mempool driver (librte-mempool-ring; re-run install profile dpdk)".into());
    } else if !caps.dpdk_initialized {
        b.push("dpdk initialized".into());
    }
    if caps.ovs_running && !has("dpdkvhostuserclient") {
        b.push("dpdkvhostuserclient".into());
    }
    if !caps.iommu {
        b.push("iommu".into());
    }
    if caps.hugepages.iter().all(|h| h.total == 0) {
        b.push("hugepages".into());
    }
    let mut c = b.clone();
    if caps.ovs_running && !has("afxdp") {
        c.push("afxdp".into());
    }
    let mut d = Vec::new();
    base(&mut d);
    if caps.ovs_running && !has("afxdp") {
        d.push("afxdp".into());
    }
    if caps.ch_net_admin == Some(false) {
        d.push("cap_net_admin on cloud-hypervisor".into());
    }
    for (id, description, missing) in [
        ("A", "kernel datapath, tap VM ports (NAT or kernel uplink)", a),
        ("B", "userspace datapath, DPDK uplink, vhost-user VM ports", b),
        ("C", "userspace datapath, AF_XDP uplink, vhost-user VM ports", c),
        ("D", "userspace datapath, AF_XDP uplink, tap VM ports", d),
    ] {
        out.push(Combination {
            id: id.into(),
            description: description.into(),
            available: missing.is_empty(),
            missing,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    #[test]
    fn version_parsing_and_policy() {
        assert_eq!(
            parse_ovs_version("ovs-vsctl (Open vSwitch) 3.7.1\nDB Schema 8.8.0\n").as_deref(),
            Some("3.7.1")
        );
        assert!(version_at_least("3.7.1", (3, 0)));
        assert!(version_at_least("3.0.0", (3, 0)));
        assert!(!version_at_least("2.17.9", (3, 0)));
    }

    #[test]
    fn probe_without_ovs() {
        let exec = RecordingExec::new();
        exec.on("ovs-vsctl --version", Output::failed(127, "not found"));
        let caps = probe(&exec, &ProbeOptions::default());
        assert!(!caps.ovs_installed);
        let a = &caps.combinations[0];
        assert!(!a.available);
        assert_eq!(a.missing, vec!["openvswitch"]);
    }

    #[test]
    fn probe_with_kernel_ovs() {
        let exec = RecordingExec::new();
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
        exec.on(
            "ovs-vsctl --format=json --columns=iface_types,datapath_types,dpdk_initialized list Open_vSwitch",
            Output::ok(r#"{"data":[[["set",["afxdp","geneve","internal","system","tap"]],["set",["netdev","system"]],false]],"headings":["iface_types","datapath_types","dpdk_initialized"]}"#),
        );
        exec.on("getcap", Output::ok("/usr/local/bin/cloud-hypervisor cap_net_admin=ep\n"));
        exec.on("systemctl is-active --quiet ufw", Output::failed(3, ""));
        exec.on("systemctl is-active --quiet firewalld", Output::failed(3, ""));
        exec.on("systemctl is-active --quiet NetworkManager", Output::failed(3, ""));
        exec.file("/proc/sys/net/ipv4/ip_forward", "1\n");
        let caps = probe(
            &exec,
            &ProbeOptions {
                ch_binary: Some("/usr/local/bin/cloud-hypervisor".into()),
            },
        );
        assert!(caps.ovs_running && caps.ovs_version_supported);
        assert!(caps.iface_types.contains(&"afxdp".to_string()));
        assert_eq!(caps.ch_net_admin, Some(true));
        assert!(caps.ip_forward);
        assert_eq!(caps.network_managers, vec!["systemd-networkd"]);
        let by_id = |id: &str| caps.combinations.iter().find(|c| c.id == id).unwrap().clone();
        assert!(by_id("A").available);
        assert!(by_id("D").available);
        assert!(by_id("B").missing.iter().any(|m| m.starts_with("dpdk build of ovs-vswitchd")), "{:?}", by_id("B"));
    }
}
