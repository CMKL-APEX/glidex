//! `dpu_probe` (spec/31-dpu.md §9.2): what the DPU has, and what it lacks
//! for the v1 design. Read-only.
//!
//! The raw outputs are kept by name ([`PROBE_NAMES`]), the names
//! `scripts/dpu/dpu-probe.sh` saves them under, so the parsing is tested
//! against the canned outputs of the test DPU (`tests/fixtures/`).

use crate::exec::{Cmd, Exec, Program};
use crate::ovsdb;
use crate::virtnet::{self, FunctionType, VirtnetList};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// The raw outputs a probe reads, by name.
pub const PROBE_NAMES: [&str; 17] = [
    "ovs_version",
    "other_config",
    "doca_init",
    "doca_ver",
    "iface_types",
    "datapath_types",
    "external_ids",
    "bridges",
    "dpif_show",
    "pmd_stats",
    "hugepages",
    "eswitch",
    "mlxconfig",
    "virtnet_list",
    "virtnet_svc",
    "ovn_ctl_ver",
    "ovn_central",
];

/// The firmware parameters of §9.1, queried (never set) with `mlxconfig`.
pub const FIRMWARE_PARAMS: [&str; 12] = [
    "VIRTIO_NET_EMULATION_ENABLE",
    "VIRTIO_NET_EMULATION_NUM_PF",
    "VIRTIO_NET_EMULATION_NUM_VF",
    "VIRTIO_NET_EMULATION_NUM_MSIX",
    "VIRTIO_NET_EMULATION_NUM_VF_MSIX",
    "PCI_SWITCH_EMULATION_ENABLE",
    "PER_PF_NUM_SF",
    "PF_TOTAL_SF",
    "PF_SF_BAR_SIZE",
    "SRIOV_EN",
    "NUM_OF_VFS",
    "INTERNAL_CPU_MODEL",
];

/// The units whose activity says a DPU-local OVN central runs (finding 12),
/// and the DPU's controller last.
pub const OVN_UNITS: [&str; 5] = [
    "ovn-central",
    "ovn-northd",
    "ovn-ovsdb-server-nb",
    "ovn-ovsdb-server-sb",
    "ovn-controller",
];

/// The `mst` device of the test card's PF; `dpu.json` may name another.
pub const DEFAULT_MST_DEVICE: &str = "/dev/mst/mt41692_pciconf0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerState {
    Active,
    Inactive,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HugepagePool {
    pub size_kib: u64,
    pub total: u64,
    pub free: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeInfo {
    pub name: String,
    pub datapath_type: Option<String>,
    pub fail_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChassisInfo {
    /// `system-id`.
    pub name: Option<String>,
    /// `ovn-remote`.
    pub remote: Option<String>,
    pub encap_ip: Option<String>,
    /// Set by glidex (`glidex-chassis`), not left by the image.
    pub glidex: bool,
}

/// What a DPU reports (§9.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpuCapabilities {
    pub ovs_version: Option<String>,
    pub doca_initialized: bool,
    pub doca_version: Option<String>,
    pub iface_types: Vec<String>,
    pub datapath_types: Vec<String>,
    pub other_config: BTreeMap<String, String>,
    pub hugepages: Vec<HugepagePool>,
    pub dpdk_hugepage_dir: Option<String>,
    pub pmd_cpu_mask: Option<String>,
    pub pmd_threads: u32,
    pub offloaded_flows: u64,
    pub eswitch_mode: Option<String>,
    pub virtio_controller: Option<ControllerState>,
    pub virtio_controller_version: Option<String>,
    /// The static PFs' host addresses (`61:00.1`).
    pub virtio_pfs: Vec<String>,
    /// VFs the PFs allow (`pci_max_vfs`) and how many are enabled.
    pub virtio_vfs_total: u32,
    pub virtio_vfs_enabled: u32,
    pub firmware: BTreeMap<String, String>,
    pub ovn_version: Option<String>,
    pub local_ovn_central: bool,
    pub ovn_controller_active: bool,
    pub chassis: Option<ChassisInfo>,
    pub bridges: Vec<BridgeInfo>,
    /// What the v1 design needs and this DPU lacks, each a short key.
    pub missing: Vec<String>,
}

fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// `ovs-vsctl (Open vSwitch) 3.5.0052` → `3.5.0052`; also `ovn-controller
/// 24.03.8` → `24.03.8`.
fn version_of(s: &str) -> Option<String> {
    first_line(s)?.split_whitespace().last().map(str::to_string)
}

fn parse_hugepages(s: &str) -> Vec<HugepagePool> {
    s.lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let size = parts
                .next()?
                .strip_prefix("hugepages-")?
                .strip_suffix("kB")?
                .parse()
                .ok()?;
            let mut total = None;
            let mut free = None;
            for p in parts {
                if let Some(v) = p.strip_prefix("total=") {
                    total = v.parse().ok();
                } else if let Some(v) = p.strip_prefix("free=") {
                    free = v.parse().ok();
                }
            }
            Some(HugepagePool {
                size_kib: size,
                total: total?,
                free: free?,
            })
        })
        .collect()
}

/// `mlxconfig q`: the parameter and its first column (`True(1)` → `True`).
fn parse_mlxconfig(s: &str) -> BTreeMap<String, String> {
    s.lines()
        .filter(|l| l.starts_with(char::is_whitespace))
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let name = parts.next()?;
            if !name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                return None;
            }
            let value = parts.next()?;
            let value = value.split('(').next().unwrap_or(value);
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

impl DpuCapabilities {
    /// Parse the raw outputs (keys of [`PROBE_NAMES`]; a missing one is an
    /// empty output, as for a command that failed).
    pub fn from_raw(raw: &BTreeMap<String, String>) -> Self {
        let get = |k: &str| raw.get(k).map(String::as_str).unwrap_or("");
        let other_config = ovsdb::map(get("other_config"));
        let external_ids = ovsdb::map(get("external_ids"));
        let virtnet: Option<VirtnetList> = virtnet::parse_list(get("virtnet_list")).ok();
        let svc: Vec<&str> = get("virtnet_svc").lines().map(str::trim).collect();
        let virtio_controller = match svc.first().copied() {
            Some("active") => Some(ControllerState::Active),
            // `systemctl is-enabled` of a unit that doesn't exist.
            Some(_)
                if svc
                    .get(1)
                    .is_some_and(|s| s.is_empty() || *s == "not-found") =>
            {
                Some(ControllerState::Missing)
            }
            Some("") | None => None,
            Some(_) => Some(ControllerState::Inactive),
        };
        let units: Vec<&str> = get("ovn_central").lines().map(str::trim).collect();
        let local_ovn_central = units.iter().take(4).any(|s| *s == "active");
        let ovn_controller_active = units.get(4) == Some(&"active");

        let chassis = (external_ids.contains_key("ovn-remote")
            || external_ids.contains_key("glidex-chassis"))
        .then(|| ChassisInfo {
            name: external_ids.get("system-id").cloned(),
            remote: external_ids.get("ovn-remote").cloned(),
            encap_ip: external_ids.get("ovn-encap-ip").cloned(),
            glidex: external_ids.contains_key("glidex-chassis"),
        });

        let pfs: Vec<_> = virtnet
            .as_ref()
            .map(|l| {
                l.pfs()
                    .filter(|d| d.function == FunctionType::StaticPf)
                    .collect()
            })
            .unwrap_or_default();

        let mut caps = DpuCapabilities {
            ovs_version: version_of(get("ovs_version")),
            doca_initialized: ovsdb::scalar(get("doca_init")).as_deref() == Some("true"),
            doca_version: ovsdb::scalar(get("doca_ver")),
            iface_types: ovsdb::set(get("iface_types")),
            datapath_types: ovsdb::set(get("datapath_types")),
            hugepages: parse_hugepages(get("hugepages")),
            dpdk_hugepage_dir: other_config.get("dpdk-hugepage-dir").cloned(),
            pmd_cpu_mask: other_config.get("pmd-cpu-mask").cloned(),
            pmd_threads: get("pmd_stats")
                .lines()
                .filter(|l| l.starts_with("pmd thread"))
                .count() as u32,
            offloaded_flows: get("dpif_show")
                .lines()
                .filter_map(|l| l.trim().strip_prefix("offloaded flows:"))
                .filter_map(|n| n.trim().parse::<u64>().ok())
                .sum(),
            eswitch_mode: get("eswitch")
                .split_whitespace()
                .skip_while(|w| *w != "mode")
                .nth(1)
                .map(str::to_string),
            virtio_controller,
            virtio_controller_version: virtnet.as_ref().and_then(|l| l.controller_version.clone()),
            virtio_pfs: pfs.iter().map(|d| d.bdf.clone()).collect(),
            virtio_vfs_total: pfs.iter().filter_map(|d| d.max_vfs).map(u32::from).sum(),
            virtio_vfs_enabled: pfs
                .iter()
                .filter_map(|d| d.enabled_vfs)
                .map(u32::from)
                .sum(),
            firmware: parse_mlxconfig(get("mlxconfig")),
            ovn_version: version_of(get("ovn_ctl_ver")),
            local_ovn_central,
            ovn_controller_active,
            chassis,
            bridges: ovsdb::records(get("bridges"))
                .iter()
                .filter_map(|r| {
                    Some(BridgeInfo {
                        name: ovsdb::scalar(r.get("name")?)?,
                        datapath_type: r.get("datapath_type").and_then(|v| ovsdb::scalar(v)),
                        fail_mode: r.get("fail_mode").and_then(|v| ovsdb::scalar(v)),
                    })
                })
                .collect(),
            other_config,
            missing: Vec::new(),
        };
        caps.missing = caps.gaps();
        caps
    }

    /// What the v1 design needs (§9.1–§9.3) and this DPU lacks.
    fn gaps(&self) -> Vec<String> {
        let mut m = Vec::new();
        let mut need = |ok: bool, what: &str| {
            if !ok {
                m.push(what.to_string());
            }
        };
        need(self.ovs_version.is_some(), "ovs");
        need(self.doca_initialized, "doca_initialized");
        need(
            self.iface_types.iter().any(|t| t == "doca"),
            "iface_type_doca",
        );
        need(
            self.datapath_types.iter().any(|t| t == "doca"),
            "datapath_type_doca",
        );
        need(
            self.hugepages
                .iter()
                .any(|p| p.size_kib == 2048 && p.total > 0),
            "hugepages_2m",
        );
        need(
            self.eswitch_mode.as_deref() == Some("switchdev"),
            "eswitch_switchdev",
        );
        let fw = |k: &str| self.firmware.get(k).map(String::as_str);
        need(
            matches!(fw("VIRTIO_NET_EMULATION_ENABLE"), Some("True" | "1")),
            "fw:VIRTIO_NET_EMULATION_ENABLE",
        );
        need(matches!(fw("SRIOV_EN"), Some("True" | "1")), "fw:SRIOV_EN");
        need(
            self.virtio_controller == Some(ControllerState::Active),
            "virtio_net_controller",
        );
        need(!self.virtio_pfs.is_empty(), "virtio_static_pf");
        need(!self.local_ovn_central, "no_local_ovn_central");
        m
    }
}

/// The commands behind each raw output that is a plain command.
pub fn probe_cmds(mst_device: &str) -> Vec<(&'static str, Cmd)> {
    let vsctl_get = |col: &str| {
        Cmd::new(
            Program::OvsVsctl,
            ["--if-exists", "get", "Open_vSwitch", ".", col],
        )
    };
    let mut mlx = vec!["-d".to_string(), mst_device.to_string(), "q".to_string()];
    mlx.extend(FIRMWARE_PARAMS.iter().map(|p| p.to_string()));
    vec![
        ("ovs_version", Cmd::new(Program::OvsVsctl, ["--version"])),
        ("other_config", vsctl_get("other_config")),
        ("doca_init", vsctl_get("doca_initialized")),
        ("doca_ver", vsctl_get("doca_version")),
        ("iface_types", vsctl_get("iface_types")),
        ("datapath_types", vsctl_get("datapath_types")),
        ("external_ids", vsctl_get("external_ids")),
        (
            "bridges",
            Cmd::new(
                Program::OvsVsctl,
                ["--columns=name,datapath_type,fail_mode", "list", "Bridge"],
            ),
        ),
        ("dpif_show", Cmd::new(Program::OvsAppctl, ["dpif/show"])),
        (
            "pmd_stats",
            Cmd::new(Program::OvsAppctl, ["dpif-netdev/pmd-stats-show"]),
        ),
        ("mlxconfig", Cmd::new(Program::Mlxconfig, mlx)),
        ("virtnet_list", virtnet::list_cmd()),
        (
            "ovn_ctl_ver",
            Cmd::new(Program::OvnController, ["--version"]),
        ),
        (
            "ovn_central",
            Cmd::new(
                Program::Systemctl,
                std::iter::once("is-active").chain(OVN_UNITS),
            ),
        ),
    ]
}

/// Run the probe on the DPU. Every read tolerates failure (an empty
/// output), so a half-configured DPU still reports what it has.
pub fn probe(exec: &dyn Exec, mst_device: &str) -> DpuCapabilities {
    let mut raw = BTreeMap::new();
    for (name, cmd) in probe_cmds(mst_device) {
        // `systemctl is-active` exits non-zero when a unit is inactive but
        // still prints each state.
        let out = exec.run(&cmd).map(|o| o.stdout_str()).unwrap_or_default();
        raw.insert(name.to_string(), out);
    }

    let hp_dir = Path::new("/sys/kernel/mm/hugepages");
    let hugepages: String = exec
        .list_dir(hp_dir)
        .iter()
        .filter(|d| d.starts_with("hugepages-"))
        .map(|d| {
            let read = |f: &str| {
                exec.read_file(&hp_dir.join(d).join(f))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default()
            };
            format!(
                "{d} total={} free={}\n",
                read("nr_hugepages"),
                read("free_hugepages")
            )
        })
        .collect();
    raw.insert("hugepages".into(), hugepages);

    let dev = exec
        .run(&Cmd::new(Program::Devlink, ["dev", "show"]))
        .map(|o| o.stdout_str())
        .unwrap_or_default();
    let eswitch = dev
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("pci/"))
        .and_then(|d| {
            exec.run(&Cmd::new(Program::Devlink, ["dev", "eswitch", "show", d]))
                .ok()
        })
        .map(|o| o.stdout_str())
        .unwrap_or_default();
    raw.insert("eswitch".into(), eswitch);

    let svc = |verb: &str| {
        exec.run(&Cmd::new(
            Program::Systemctl,
            [verb, "virtio-net-controller"],
        ))
        .map(|o| o.stdout_str().trim().to_string())
        .unwrap_or_default()
    };
    raw.insert(
        "virtnet_svc".into(),
        format!("{}\n{}\n", svc("is-active"), svc("is-enabled")),
    );
    DpuCapabilities::from_raw(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    #[test]
    fn an_empty_dpu_lacks_everything_and_mlxconfig_is_a_query() {
        let caps = DpuCapabilities::from_raw(&BTreeMap::new());
        assert!(caps.missing.contains(&"ovs".to_string()));
        assert!(caps.missing.contains(&"virtio_static_pf".to_string()));
        assert!(!caps.missing.contains(&"no_local_ovn_central".to_string()));
        let mlx = probe_cmds(DEFAULT_MST_DEVICE)
            .into_iter()
            .find(|(n, _)| *n == "mlxconfig")
            .unwrap()
            .1;
        assert!(
            mlx.display().starts_with(
                "mlxconfig -d /dev/mst/mt41692_pciconf0 q VIRTIO_NET_EMULATION_ENABLE"
            )
        );
    }

    #[test]
    fn probe_reads_sysfs_devlink_and_systemd() {
        let exec = RecordingExec::new();
        exec.file(
            "/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages",
            "1024\n",
        );
        exec.file(
            "/sys/kernel/mm/hugepages/hugepages-2048kB/free_hugepages",
            "1000\n",
        );
        exec.on(
            "devlink dev show",
            Output::ok("pci/0000:03:00.0\npci/0000:03:00.1\n"),
        );
        exec.on(
            "devlink dev eswitch show pci/0000:03:00.0",
            Output::ok("pci/0000:03:00.0: mode switchdev inline-mode none encap-mode basic\n"),
        );
        exec.on(
            "systemctl is-active virtio-net-controller",
            Output::ok("active\n"),
        );
        exec.on(
            "systemctl is-active ovn-central",
            Output {
                status: 3,
                stdout: b"inactive\ninactive\ninactive\ninactive\nactive\n".to_vec(),
                stderr: vec![],
            },
        );
        let caps = probe(&exec, DEFAULT_MST_DEVICE);
        assert_eq!(
            caps.hugepages,
            [HugepagePool {
                size_kib: 2048,
                total: 1024,
                free: 1000
            }]
        );
        assert_eq!(caps.eswitch_mode.as_deref(), Some("switchdev"));
        assert_eq!(caps.virtio_controller, Some(ControllerState::Active));
        assert!(!caps.local_ovn_central);
        assert!(caps.ovn_controller_active);
        assert!(!caps.missing.contains(&"hugepages_2m".to_string()));
    }
}
