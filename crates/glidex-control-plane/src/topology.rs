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
    }
}
