//! A host's NUMA topology as the scheduler needs it (spec/host-topology.md
//! §3.1): NUMA nodes with their cores, memory and hugepages, the CPUs no VM
//! gets, and the NUMA node of each device a VM could be given. It goes in
//! the node's replicated status, so it stays small: no PCIe tree here.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologySummary {
    pub numa_nodes: Vec<NumaNode>,
    /// CPUs the kernel keeps tasks off (`isolcpus`, `nohz_full`): preferred
    /// for dedicated vCPUs (H7).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub isolated: Vec<u32>,
    /// CPUs no VM gets, and why: `pmd` and `dpdk_lcore` (OVS-DPDK), `config`
    /// (`cluster.node_reserved.cpu_list`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reserved_cpus: BTreeMap<u32, String>,
    /// `cluster.node_reserved.cpu_list` is set: it replaces the default
    /// host core per NUMA node that dedicated VMs leave free (H7).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reserved_by_config: bool,
    /// The NUMA node of each PCI device a VM could be given (by address);
    /// devices whose firmware gives none (`-1`) are left out (H4).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub device_numa: BTreeMap<String, u32>,
    /// The devices a VM could be given, for pools (§9): ids, place and
    /// readiness.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<SummaryDevice>,
}

/// A device a VM could be given, as the scheduler needs it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryDevice {
    pub address: String,
    pub vendor_id: String,
    pub device_id: String,
    pub class_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numa_node: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ready: bool,
}

impl TopologyTree {
    /// The assignable devices, for the summary.
    pub fn summary_devices(&self) -> Vec<SummaryDevice> {
        self.devices
            .iter()
            .filter(|d| d.assignable)
            .map(|d| SummaryDevice {
                address: d.address.clone(),
                vendor_id: d.vendor_id.clone(),
                device_id: d.device_id.clone(),
                class_id: d.class_id.clone(),
                numa_node: d.numa_node,
                switch: d.switch.clone(),
                ready: d.ready,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumaNode {
    pub id: u32,
    /// Its physical cores, each as its hyperthreads, lowest first.
    pub cores: Vec<Vec<u32>>,
    /// Memory VMs may take here: the node's memory less its hugepage pool.
    pub memory_mib: u64,
    /// Hugepages VMs may take here, by page size in KiB: the node's pool
    /// less what OVS-DPDK takes on it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hugepages: BTreeMap<String, u64>,
}

impl NumaNode {
    /// Its online CPUs.
    pub fn cpus(&self) -> impl Iterator<Item = u32> + '_ {
        self.cores.iter().flatten().copied()
    }

    pub fn pages(&self, page_kib: u64) -> u64 {
        self.hugepages
            .get(&page_kib.to_string())
            .copied()
            .unwrap_or(0)
    }
}

impl TopologySummary {
    pub fn node(&self, id: u32) -> Option<&NumaNode> {
        self.numa_nodes.iter().find(|n| n.id == id)
    }

    /// The NUMA node a CPU is on.
    pub fn node_of_cpu(&self, cpu: u32) -> Option<u32> {
        self.numa_nodes
            .iter()
            .find(|n| n.cpus().any(|c| c == cpu))
            .map(|n| n.id)
    }

    /// Take OVS-DPDK's share out of each node's hugepages: `socket_mem` is
    /// MiB per NUMA node in node order (`1024,1024`), in pages of
    /// `page_kib`.
    pub fn deduct_dpdk(&mut self, socket_mem: &str, page_kib: u64) {
        let page_mib = (page_kib / 1024).max(1);
        for (i, mib) in socket_mem.split(',').enumerate() {
            let Ok(mib) = mib.trim().parse::<u64>() else {
                continue;
            };
            if let Some(n) = self.numa_nodes.iter_mut().find(|n| n.id == i as u32)
                && let Some(p) = n.hugepages.get_mut(&page_kib.to_string())
            {
                *p = p.saturating_sub(mib.div_ceil(page_mib));
            }
        }
    }

    pub fn reserve(&mut self, cpus: impl IntoIterator<Item = u32>, why: &str) {
        for c in cpus {
            self.reserved_cpus
                .entry(c)
                .or_insert_with(|| why.to_string());
        }
    }
}

/// The CPUs of an OVS CPU mask (`0x6`, `6`, any length).
pub fn cpus_of_mask(mask: &str) -> Vec<u32> {
    let hex = mask
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let mut out = Vec::new();
    for (i, ch) in hex.chars().rev().enumerate() {
        let Some(d) = ch.to_digit(16) else {
            return Vec::new();
        };
        for bit in 0..4 {
            if d & (1 << bit) != 0 {
                out.push(i as u32 * 4 + bit);
            }
        }
    }
    out
}

/// `0-3,8,10-11` for a set of CPUs.
pub fn cpu_list(cpus: &[u32]) -> String {
    let set: BTreeSet<u32> = cpus.iter().copied().collect();
    let mut parts = Vec::new();
    let mut it = set.into_iter().peekable();
    while let Some(start) = it.next() {
        let mut end = start;
        while it.peek() == Some(&(end + 1)) {
            end = it.next().unwrap_or(end);
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
    }
    parts.join(",")
}

/// PCI classes of devices a VM could be given (§3.3): display, network,
/// NVMe, processing accelerators, coprocessors.
fn assignable_class(class: &str) -> bool {
    let c = class.trim_start_matches("0x").to_ascii_lowercase();
    c.starts_with("03")
        || c.starts_with("02")
        || c.starts_with("0108")
        || c.starts_with("12")
        || c.starts_with("0b40")
}

/// Build the summary from what sysfs says (`read_topology`'s CPUs, each
/// node's memory and hugepage pool, the PCI devices). Pure, for tests.
pub fn build(
    cpus: &glidex_ovs::tuning::Topology,
    node_memory_mib: &BTreeMap<u32, u64>,
    node_hugepages: &BTreeMap<u32, BTreeMap<String, u64>>,
    devices: &[crate::pci::PciDeviceInfo],
) -> TopologySummary {
    // A core is its set of hyperthreads; every thread names the same set.
    let mut cores: BTreeMap<u32, BTreeSet<Vec<u32>>> = BTreeMap::new();
    for c in &cpus.cpus {
        let mut threads = if c.siblings.contains(&c.id) {
            c.siblings.clone()
        } else {
            vec![c.id]
        };
        threads.sort_unstable();
        cores.entry(c.node).or_default().insert(threads);
    }
    let mut nodes: BTreeMap<u32, NumaNode> = cores
        .into_iter()
        .map(|(id, cores)| {
            (
                id,
                NumaNode {
                    id,
                    cores: cores.into_iter().collect(),
                    ..Default::default()
                },
            )
        })
        .collect();
    for n in nodes.values_mut() {
        let hp = node_hugepages.get(&n.id).cloned().unwrap_or_default();
        let pool = crate::node::hugepage_pool_mib(&hp);
        n.memory_mib = node_memory_mib
            .get(&n.id)
            .copied()
            .unwrap_or(0)
            .saturating_sub(pool);
        n.hugepages = hp.into_iter().filter(|(_, p)| *p > 0).collect();
    }
    TopologySummary {
        numa_nodes: nodes.into_values().collect(),
        isolated: cpus.isolated.iter().copied().collect(),
        device_numa: devices
            .iter()
            .filter(|d| assignable_class(&d.class_id) || d.driver.as_deref() == Some("vfio-pci"))
            .filter_map(|d| d.numa_node.map(|n| (d.address.to_ascii_lowercase(), n)))
            .collect(),
        ..Default::default()
    }
}

/// Read this host's summary from sysfs; `None` without readable CPU
/// topology.
pub fn read() -> Option<TopologySummary> {
    let exec = glidex_ovs::exec::SystemExec::default();
    let cpus = glidex_ovs::tuning::read_topology(&exec);
    if cpus.cpus.is_empty() {
        return None;
    }
    let nodes: BTreeSet<u32> = cpus.cpus.iter().map(|c| c.node).collect();
    let mut memory = BTreeMap::new();
    let mut hugepages = BTreeMap::new();
    for n in nodes {
        let dir = format!("/sys/devices/system/node/node{n}");
        // "Node 0 MemTotal:       131072 kB"
        if let Some(kib) = std::fs::read_to_string(format!("{dir}/meminfo"))
            .ok()
            .and_then(|m| {
                m.lines()
                    .find(|l| l.contains("MemTotal:"))
                    .and_then(|l| l.split_whitespace().rev().nth(1)?.parse::<u64>().ok())
            })
        {
            memory.insert(n, kib / 1024);
        }
        let mut hp = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(format!("{dir}/hugepages")) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if let Some(kib) = name
                    .strip_prefix("hugepages-")
                    .and_then(|s| s.strip_suffix("kB"))
                    && let Some(pages) = std::fs::read_to_string(e.path().join("nr_hugepages"))
                        .ok()
                        .and_then(|s| s.trim().parse::<u64>().ok())
                {
                    hp.insert(kib.to_string(), pages);
                }
            }
        }
        hugepages.insert(n, hp);
    }
    // One NUMA node and no `node0` directory (a kernel without NUMA): all
    // memory is node 0's.
    if memory.is_empty() {
        let total = std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| {
            m.lines()
                .find_map(|l| l.strip_prefix("MemTotal:"))
                .and_then(|r| r.split_whitespace().next()?.parse::<u64>().ok())
        });
        if let Some(kib) = total {
            memory.insert(0, kib / 1024);
        }
    }
    Some(build(
        &cpus,
        &memory,
        &hugepages,
        &crate::pci::scan_pci_devices(),
    ))
}

// ---- the full tree (spec/host-topology.md §3.2, §5) -------------------------

/// What a PCI function is in the PCIe tree. Told from the bridges' places
/// (sysfs paths), not the PCIe capability, which needs root to read: a
/// bridge right under a root complex is a root port, one under a root
/// port or a downstream port a switch's upstream port, one under an
/// upstream port a downstream port.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PciKind {
    #[default]
    Endpoint,
    RootPort,
    SwitchUpstream,
    SwitchDownstream,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PcieLink {
    pub current_speed: String,
    pub current_width: u32,
    pub max_speed: String,
    pub max_width: u32,
}

impl PcieLink {
    /// Slower or narrower than it can be.
    pub fn degraded(&self) -> bool {
        let gts = |s: &str| {
            s.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        self.current_width < self.max_width || gts(&self.current_speed) < gts(&self.max_speed)
    }
}

/// AER error counts since boot (spec/host-topology.md §5.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AerCounts {
    pub correctable: u64,
    pub nonfatal: u64,
    pub fatal: u64,
}

impl AerCounts {
    /// Uncorrectable errors, fatal or not: what makes a device unhealthy.
    pub fn uncorrectable(&self) -> u64 {
        self.nonfatal + self.fatal
    }
}

/// The `TOTAL_ERR_*` line of an `aer_dev_*` file (`TOTAL_ERR_FATAL 0`).
pub fn aer_total(text: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with("TOTAL_ERR"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
}

/// A device's AER counts from sysfs; `None` without AER.
pub fn read_aer(address: &str) -> Option<AerCounts> {
    let dir = std::path::Path::new("/sys/bus/pci/devices").join(address);
    let read = |f: &str| {
        std::fs::read_to_string(dir.join(f))
            .ok()
            .and_then(|t| aer_total(&t))
    };
    Some(AerCounts {
        correctable: read("aer_dev_correctable")?,
        nonfatal: read("aer_dev_nonfatal").unwrap_or(0),
        fatal: read("aer_dev_fatal").unwrap_or(0),
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sriov {
    /// A physical function: VFs it can have, and has.
    pub total_vfs: u32,
    pub enabled_vfs: u32,
}

/// One PCI function in the tree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PciFunction {
    pub address: String,
    pub vendor_id: String,
    pub device_id: String,
    pub class_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iommu_group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numa_node: Option<u32>,
    /// The bridge above it, or its root complex (`pci0000:80`).
    pub parent: String,
    pub kind: PciKind,
    /// Its root port, and the upstream port of the nearest switch above it:
    /// devices under one switch can talk peer to peer without the root
    /// complex (§3.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_port: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<PcieLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sriov: Option<Sriov>,
    /// A virtual function's physical function.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physfn: Option<String>,
    /// A device a VM could be given (§3.3).
    pub assignable: bool,
    /// For an assignable device: ready for passthrough (§5.1), else why not.
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_ready: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aer: Option<AerCounts>,
}

/// A gap the node reports, never acts on (§5.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyWarning {
    /// `iommu_off`, `numa_unknown`, `group_shared`, `link_degraded`,
    /// `hugepages_unbalanced`.
    pub kind: String,
    pub detail: String,
}

/// A host's full topology (`GET /host/topology`): the summary, every PCI
/// function, and the warnings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyTree {
    pub summary: TopologySummary,
    pub devices: Vec<PciFunction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<TopologyWarning>,
}

/// What sysfs says about one PCI function.
#[derive(Debug, Clone, Default)]
pub struct RawPci {
    pub address: String,
    pub vendor_id: String,
    pub device_id: String,
    pub class_id: String,
    pub driver: Option<String>,
    pub iommu_group: Option<String>,
    pub numa_node: Option<u32>,
    /// Its sysfs path under `/sys/devices`, as components: the root
    /// complex, the bridges, itself (`pci0000:80`, `0000:80:01.1`, …).
    pub path: Vec<String>,
    pub link: Option<PcieLink>,
    pub sriov: Option<Sriov>,
    pub physfn: Option<String>,
    pub aer: Option<AerCounts>,
}

/// What readiness depends on besides the device.
#[derive(Debug, Clone, Default)]
pub struct ReadyContext {
    /// The host has an IOMMU.
    pub iommu: bool,
    /// IOMMU groups whose `/dev/vfio/<group>` this user can't open.
    pub closed_groups: BTreeSet<String>,
    /// Addresses that are OVS-DPDK uplinks of glidex bridges.
    pub dpdk_uplinks: BTreeSet<String>,
}

fn is_bridge(class: &str) -> bool {
    class
        .trim_start_matches("0x")
        .to_ascii_lowercase()
        .starts_with("0604")
}

/// Host, PCI and other bridges (class `0x06…`): they stay with the host
/// when the rest of their IOMMU group is passed through.
fn is_bridge_class(class: &str) -> bool {
    class.trim_start_matches("0x").starts_with("06")
}

fn is_gpu(class: &str) -> bool {
    class.trim_start_matches("0x").starts_with("03")
}

/// The tree from raw sysfs entries (pure, for tests): kinds, root ports,
/// switches, which devices are assignable and ready, and the warnings.
pub fn build_tree(summary: TopologySummary, raw: &[RawPci], ctx: &ReadyContext) -> TopologyTree {
    let by_addr: BTreeMap<&str, &RawPci> = raw.iter().map(|r| (r.address.as_str(), r)).collect();
    // Kinds, by depth: a bridge's kind follows from its parent's.
    let mut kinds: BTreeMap<String, PciKind> = BTreeMap::new();
    let mut sorted: Vec<&RawPci> = raw.iter().collect();
    sorted.sort_by_key(|r| r.path.len());
    for r in &sorted {
        let parent = r.path.iter().rev().nth(1);
        let parent_kind = parent.and_then(|p| kinds.get(p)).copied();
        let kind = if !is_bridge(&r.class_id) {
            PciKind::Endpoint
        } else {
            match parent_kind {
                None => PciKind::RootPort,
                Some(PciKind::RootPort | PciKind::SwitchDownstream) => PciKind::SwitchUpstream,
                Some(PciKind::SwitchUpstream) => PciKind::SwitchDownstream,
                Some(PciKind::Endpoint) => PciKind::SwitchUpstream,
            }
        };
        kinds.insert(r.address.clone(), kind);
    }
    let mut groups: BTreeMap<&str, Vec<&RawPci>> = BTreeMap::new();
    for r in raw {
        if let Some(g) = &r.iommu_group {
            groups.entry(g.as_str()).or_default().push(r);
        }
    }
    let multi_numa = summary.numa_nodes.len() > 1;
    let mut warnings = Vec::new();
    if !ctx.iommu {
        warnings.push(TopologyWarning {
            kind: "iommu_off".into(),
            detail: "no IOMMU groups: turn on VT-d / AMD-Vi and intel_iommu=on or amd_iommu=on"
                .into(),
        });
    }
    let mut devices = Vec::new();
    for r in raw {
        let kind = kinds.get(&r.address).copied().unwrap_or_default();
        let above: Vec<&str> = r
            .path
            .iter()
            .take(r.path.len().saturating_sub(1))
            .map(String::as_str)
            .filter(|p| by_addr.contains_key(p))
            .collect();
        let root_port = above
            .iter()
            .find(|a| kinds.get(**a) == Some(&PciKind::RootPort))
            .map(|a| a.to_string());
        let switch = above
            .iter()
            .rev()
            .find(|a| kinds.get(**a) == Some(&PciKind::SwitchUpstream))
            .map(|a| a.to_string());
        let assignable = kind == PciKind::Endpoint
            && (assignable_class(&r.class_id) || r.driver.as_deref() == Some("vfio-pci"));
        let mut not_ready = Vec::new();
        if assignable {
            match &r.iommu_group {
                None => not_ready.push("no IOMMU group (the IOMMU is off)".to_string()),
                Some(g) => {
                    if r.driver.as_deref() != Some("vfio-pci") {
                        not_ready.push(format!(
                            "not bound to vfio-pci (driver: {})",
                            r.driver.as_deref().unwrap_or("none")
                        ));
                    }
                    for peer in groups.get(g.as_str()).into_iter().flatten() {
                        if peer.address == r.address || is_bridge_class(&peer.class_id) {
                            continue;
                        }
                        if let Some(d) = peer.driver.as_deref().filter(|d| *d != "vfio-pci") {
                            not_ready.push(format!(
                                "IOMMU group {g} also holds {} on {d}",
                                peer.address
                            ));
                        }
                        let class4 = |c: &str| {
                            c.trim_start_matches("0x")
                                .chars()
                                .take(4)
                                .collect::<String>()
                        };
                        if class4(&peer.class_id) != class4(&r.class_id) {
                            warnings.push(TopologyWarning {
                                kind: "group_shared".into(),
                                detail: format!(
                                    "{} shares IOMMU group {g} with {} of another class",
                                    r.address, peer.address
                                ),
                            });
                        }
                    }
                    if r.driver.as_deref() == Some("vfio-pci") && ctx.closed_groups.contains(g) {
                        not_ready.push(format!("/dev/vfio/{g} can't be opened by this user"));
                    }
                }
            }
            if ctx.dpdk_uplinks.contains(&r.address) {
                not_ready.push("it is an OVS-DPDK uplink".to_string());
            }
            if r.numa_node.is_none() && multi_numa {
                warnings.push(TopologyWarning {
                    kind: "numa_unknown".into(),
                    detail: format!("{} has no NUMA node (firmware reports -1)", r.address),
                });
            }
            if let Some(l) = &r.link
                && l.degraded()
                && !is_gpu(&r.class_id)
            {
                warnings.push(TopologyWarning {
                    kind: "link_degraded".into(),
                    detail: format!(
                        "{} runs at {} x{} of {} x{}",
                        r.address, l.current_speed, l.current_width, l.max_speed, l.max_width
                    ),
                });
            }
            if let Some(n) = r.numa_node
                && summary.numa_nodes.iter().any(|k| !k.hugepages.is_empty())
                && summary.node(n).is_some_and(|k| k.hugepages.is_empty())
            {
                warnings.push(TopologyWarning {
                    kind: "hugepages_unbalanced".into(),
                    detail: format!("{} is on NUMA node {n}, which has no hugepages", r.address),
                });
            }
        }
        devices.push(PciFunction {
            address: r.address.clone(),
            vendor_id: r.vendor_id.clone(),
            device_id: r.device_id.clone(),
            class_id: r.class_id.clone(),
            driver: r.driver.clone(),
            iommu_group: r.iommu_group.clone(),
            numa_node: r.numa_node,
            parent: r.path.iter().rev().nth(1).cloned().unwrap_or_default(),
            kind,
            root_port,
            switch,
            link: r.link.clone(),
            sriov: r.sriov.clone(),
            physfn: r.physfn.clone(),
            assignable,
            ready: assignable && not_ready.is_empty(),
            not_ready,
            aer: r.aer,
        });
    }
    devices.sort_by(|a, b| a.address.cmp(&b.address));
    warnings.dedup();
    TopologyTree {
        summary,
        devices,
        warnings,
    }
}

/// Read this host's tree from sysfs. `dpdk_uplinks`: addresses netd has as
/// DPDK uplinks.
pub fn read_tree(summary: TopologySummary, dpdk_uplinks: BTreeSet<String>) -> TopologyTree {
    let base = std::path::Path::new("/sys/bus/pci/devices");
    let attr = |dir: &std::path::Path, f: &str| {
        std::fs::read_to_string(dir.join(f))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let link_name = |dir: &std::path::Path, f: &str| {
        std::fs::read_link(dir.join(f))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    };
    let mut raw = Vec::new();
    for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
        let dir = e.path();
        let address = e.file_name().to_string_lossy().into_owned();
        let real = std::fs::canonicalize(&dir).unwrap_or_default();
        let path: Vec<String> = real
            .strip_prefix("/sys/devices")
            .map(|p| {
                p.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let num = |f: &str| attr(&dir, f).and_then(|s| s.parse::<u32>().ok());
        let link = match (
            attr(&dir, "current_link_speed"),
            num("current_link_width"),
            attr(&dir, "max_link_speed"),
            num("max_link_width"),
        ) {
            (Some(cs), Some(cw), Some(ms), Some(mw)) => Some(PcieLink {
                current_speed: cs,
                current_width: cw,
                max_speed: ms,
                max_width: mw,
            }),
            _ => None,
        };
        raw.push(RawPci {
            vendor_id: attr(&dir, "vendor").unwrap_or_default(),
            device_id: attr(&dir, "device").unwrap_or_default(),
            class_id: attr(&dir, "class").unwrap_or_default(),
            driver: link_name(&dir, "driver"),
            iommu_group: link_name(&dir, "iommu_group"),
            numa_node: attr(&dir, "numa_node").and_then(|n| n.parse().ok()),
            path,
            link,
            sriov: num("sriov_totalvfs").filter(|t| *t > 0).map(|t| Sriov {
                total_vfs: t,
                enabled_vfs: num("sriov_numvfs").unwrap_or(0),
            }),
            physfn: link_name(&dir, "physfn"),
            aer: read_aer(&address),
            address,
        });
    }
    let groups: BTreeSet<String> = raw.iter().filter_map(|r| r.iommu_group.clone()).collect();
    let closed_groups = groups
        .into_iter()
        .filter(|g| {
            nix::unistd::access(
                format!("/dev/vfio/{g}").as_str(),
                nix::unistd::AccessFlags::R_OK | nix::unistd::AccessFlags::W_OK,
            )
            .is_err()
        })
        .collect();
    let ctx = ReadyContext {
        iommu: std::path::Path::new("/sys/kernel/iommu_groups/0").exists(),
        closed_groups,
        dpdk_uplinks,
    };
    build_tree(summary, &raw, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glidex_ovs::tuning::{Cpu, Topology};

    /// Two nodes of two cores with two threads each: node 0 CPUs 0,1 and
    /// 4,5 as siblings (0/4, 1/5), node 1 CPUs 2,3,6,7.
    fn host() -> Topology {
        let mut t = Topology::default();
        for (id, node, core) in [
            (0, 0, 0),
            (1, 0, 1),
            (2, 1, 0),
            (3, 1, 1),
            (4, 0, 0),
            (5, 0, 1),
            (6, 1, 0),
            (7, 1, 1),
        ] {
            let sib = if id < 4 { id + 4 } else { id - 4 };
            t.cpus.push(Cpu {
                id,
                package: node,
                core,
                node,
                siblings: vec![id.min(sib), id.max(sib)],
            });
        }
        t.isolated = [3, 7].into();
        t
    }

    fn dev(
        address: &str,
        class: &str,
        driver: Option<&str>,
        numa: Option<u32>,
    ) -> crate::pci::PciDeviceInfo {
        crate::pci::PciDeviceInfo {
            address: address.into(),
            vendor_id: "0x10de".into(),
            device_id: "0x20b0".into(),
            class_id: class.into(),
            driver: driver.map(Into::into),
            iommu_group: None,
            numa_node: numa,
            sysfs_path: String::new(),
        }
    }

    #[test]
    fn nodes_get_their_cores_memory_less_the_pool_and_devices() {
        let mem = [(0, 65536), (1, 65536)].into();
        let hp: BTreeMap<u32, BTreeMap<String, u64>> = [
            (0, [("2048".to_string(), 2048)].into()),
            (1, [("2048".to_string(), 0)].into()),
        ]
        .into();
        let t = build(
            &host(),
            &mem,
            &hp,
            &[
                dev("0000:47:00.0", "0x030200", Some("vfio-pci"), Some(1)),
                dev("0000:09:00.0", "0x010802", Some("nvme"), Some(0)),
                dev("0000:00:01.0", "0x060400", Some("pcieport"), Some(0)),
                dev("0000:63:00.0", "0x030000", Some("ast"), None),
            ],
        );
        assert_eq!(t.numa_nodes.len(), 2);
        let n0 = t.node(0).unwrap();
        assert_eq!(n0.cores, vec![vec![0, 4], vec![1, 5]]);
        assert_eq!(
            n0.memory_mib,
            65536 - 4096,
            "the 2M pool is not ordinary memory"
        );
        assert_eq!(n0.pages(2048), 2048);
        assert!(t.node(1).unwrap().hugepages.is_empty());
        assert_eq!(t.node_of_cpu(6), Some(1));
        assert_eq!(t.isolated, vec![3, 7]);
        assert_eq!(
            t.device_numa.into_iter().collect::<Vec<_>>(),
            [("0000:09:00.0".into(), 0), ("0000:47:00.0".into(), 1)],
            "bridges and devices without a NUMA node are left out"
        );
    }

    #[test]
    fn dpdk_takes_its_socket_memory_per_node() {
        let mut t = TopologySummary {
            numa_nodes: (0..2)
                .map(|id| NumaNode {
                    id,
                    hugepages: [("2048".to_string(), 1024)].into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        t.deduct_dpdk("1024,512", 2048);
        assert_eq!(t.node(0).unwrap().pages(2048), 512);
        assert_eq!(t.node(1).unwrap().pages(2048), 768);
    }

    #[test]
    fn masks_and_lists() {
        assert_eq!(cpus_of_mask("0x6"), vec![1, 2]);
        assert_eq!(cpus_of_mask("1"), vec![0]);
        assert_eq!(
            cpus_of_mask("0x10000000000000000000000000000000001"),
            vec![0, 136]
        );
        assert!(cpus_of_mask("zz").is_empty());
        assert_eq!(cpu_list(&[3, 0, 1, 2, 8, 10, 11]), "0-3,8,10-11");
        assert_eq!(cpu_list(&[]), "");
    }

    /// On a real host: `cargo test -p glidex-control-plane --lib
    /// reads_this_host -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn reads_this_host() {
        let t = read().expect("a CPU topology");
        for n in &t.numa_nodes {
            let cpus: Vec<u32> = n.cpus().collect();
            println!(
                "node {}: {} cores, cpus {}, {} MiB, hugepages {:?}",
                n.id,
                n.cores.len(),
                cpu_list(&cpus),
                n.memory_mib,
                n.hugepages
            );
        }
        println!("isolated {:?}; devices {:?}", t.isolated, t.device_numa);
        assert!(!t.numa_nodes.is_empty());
        let tree = read_tree(t, Default::default());
        for d in tree.devices.iter().filter(|d| d.assignable) {
            println!(
                "{} {} numa {:?} switch {:?} ready {} {:?}",
                d.address, d.class_id, d.numa_node, d.switch, d.ready, d.not_ready
            );
        }
        for w in &tree.warnings {
            println!("warning {}: {}", w.kind, w.detail);
        }
    }

    fn raw(
        path: &str,
        class: &str,
        driver: Option<&str>,
        group: Option<&str>,
        numa: Option<u32>,
    ) -> RawPci {
        let path: Vec<String> = path.split('/').map(String::from).collect();
        RawPci {
            address: path.last().cloned().unwrap(),
            vendor_id: "0x10de".into(),
            device_id: "0x20b0".into(),
            class_id: class.into(),
            driver: driver.map(Into::into),
            iommu_group: group.map(Into::into),
            numa_node: numa,
            path,
            ..Default::default()
        }
    }

    /// A root port, a switch with a GPU and a NIC under it, a GPU on its
    /// own root port, and a GPU sharing its group with an audio function.
    fn tree() -> TopologyTree {
        let summary = TopologySummary {
            numa_nodes: (0..2)
                .map(|id| NumaNode {
                    id,
                    hugepages: if id == 0 {
                        [("2048".to_string(), 512)].into()
                    } else {
                        Default::default()
                    },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let mut nic = raw(
            "pci0000:80/0000:80:01.1/0000:81:00.0/0000:82:00.0/0000:88:00.0",
            "0x020700",
            Some("mlx5_core"),
            Some("21"),
            Some(1),
        );
        nic.link = Some(PcieLink {
            current_speed: "8.0 GT/s PCIe".into(),
            current_width: 8,
            max_speed: "16.0 GT/s PCIe".into(),
            max_width: 16,
        });
        nic.sriov = Some(Sriov {
            total_vfs: 16,
            enabled_vfs: 0,
        });
        let raw = vec![
            raw(
                "pci0000:80/0000:80:01.1",
                "0x060400",
                Some("pcieport"),
                Some("1"),
                Some(1),
            ),
            raw(
                "pci0000:80/0000:80:01.1/0000:81:00.0",
                "0x060400",
                Some("pcieport"),
                Some("2"),
                Some(1),
            ),
            raw(
                "pci0000:80/0000:80:01.1/0000:81:00.0/0000:82:00.0",
                "0x060400",
                Some("pcieport"),
                Some("3"),
                Some(1),
            ),
            raw(
                "pci0000:80/0000:80:01.1/0000:81:00.0/0000:82:00.0/0000:87:00.0",
                "0x030200",
                Some("vfio-pci"),
                Some("20"),
                Some(1),
            ),
            nic,
            raw(
                "pci0000:00/0000:00:03.1",
                "0x060400",
                Some("pcieport"),
                Some("4"),
                Some(0),
            ),
            raw(
                "pci0000:00/0000:00:03.1/0000:07:00.0",
                "0x030200",
                Some("nvidia"),
                Some("30"),
                Some(0),
            ),
            raw(
                "pci0000:00/0000:00:03.1/0000:07:00.1",
                "0x040300",
                Some("snd_hda_intel"),
                Some("30"),
                Some(0),
            ),
            raw(
                "pci0000:00/0000:00:05.1/0000:09:00.0",
                "0x030200",
                Some("vfio-pci"),
                Some("31"),
                None,
            ),
        ];
        let ctx = ReadyContext {
            iommu: true,
            closed_groups: ["31".to_string()].into(),
            dpdk_uplinks: Default::default(),
        };
        build_tree(summary, &raw, &ctx)
    }

    fn func<'a>(t: &'a TopologyTree, a: &str) -> &'a PciFunction {
        t.devices.iter().find(|d| d.address == a).unwrap()
    }

    #[test]
    fn the_tree_knows_root_ports_switches_and_peers() {
        let t = tree();
        assert_eq!(func(&t, "0000:80:01.1").kind, PciKind::RootPort);
        assert_eq!(func(&t, "0000:81:00.0").kind, PciKind::SwitchUpstream);
        assert_eq!(func(&t, "0000:82:00.0").kind, PciKind::SwitchDownstream);
        let gpu = func(&t, "0000:87:00.0");
        let nic = func(&t, "0000:88:00.0");
        assert_eq!(gpu.kind, PciKind::Endpoint);
        assert_eq!(gpu.parent, "0000:82:00.0");
        assert_eq!(
            (gpu.root_port.as_deref(), gpu.switch.as_deref()),
            (Some("0000:80:01.1"), Some("0000:81:00.0"))
        );
        assert_eq!(nic.switch, gpu.switch, "peer-close: one switch");
        assert_eq!(func(&t, "0000:07:00.0").switch, None);
        assert!(!func(&t, "0000:81:00.0").assignable, "bridges aren't");
    }

    #[test]
    fn readiness_says_why_not() {
        let t = tree();
        assert!(func(&t, "0000:87:00.0").ready);
        assert_eq!(
            func(&t, "0000:07:00.0").not_ready,
            vec![
                "not bound to vfio-pci (driver: nvidia)".to_string(),
                "IOMMU group 30 also holds 0000:07:00.1 on snd_hda_intel".to_string(),
            ]
        );
        assert_eq!(
            func(&t, "0000:09:00.0").not_ready,
            vec!["/dev/vfio/31 can't be opened by this user".to_string()]
        );
        assert!(!func(&t, "0000:88:00.0").ready, "the NIC is on mlx5_core");
    }

    #[test]
    fn aer_totals_are_read_from_their_last_line() {
        let text = "Undefined 0\nDLP 0\nTLP 2\nTOTAL_ERR_FATAL 3\n";
        assert_eq!(aer_total(text), Some(3));
        assert_eq!(aer_total("nothing"), None);
        let c = AerCounts {
            correctable: 9,
            nonfatal: 1,
            fatal: 2,
        };
        assert_eq!(c.uncorrectable(), 3);
    }

    #[test]
    fn warnings_are_reported() {
        let t = tree();
        let kinds: Vec<&str> = t.warnings.iter().map(|w| w.kind.as_str()).collect();
        for k in [
            "group_shared",
            "numa_unknown",
            "link_degraded",
            "hugepages_unbalanced",
        ] {
            assert!(kinds.contains(&k), "{k} in {:?}", t.warnings);
        }
        assert!(!kinds.contains(&"iommu_off"));
        // A GPU idling at a slower link is not a warning.
        assert!(
            t.warnings
                .iter()
                .filter(|w| w.kind == "link_degraded")
                .all(|w| w.detail.starts_with("0000:88:00.0")),
            "{:?}",
            t.warnings
        );
    }
}
