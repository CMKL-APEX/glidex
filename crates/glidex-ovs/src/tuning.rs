//! Host tuning for OVS-DPDK vhost-user throughput: where PMD threads go,
//! how much hugepage memory each NUMA node gets, and how many queues a VM
//! NIC should have.
//!
//! The planner is pure over a [`Topology`]; [`read_topology`] fills one in
//! from sysfs through [`Exec::read_file`], so tests can fake any host and a
//! host without readable sysfs just yields "no plan" (OVS keeps its
//! defaults).
//!
//! Rules (`plan`):
//! - One **physical core** per PMD, using its first hyperthread. PMD
//!   threads poll at 100%, so a sibling thread would only share the core's
//!   execution units; it stays free for the scheduler.
//! - The first physical core of the first NUMA node is **reserved** for the
//!   OS, ovs-vswitchd's handler/revalidator threads and DPDK's non-PMD
//!   threads (`dpdk-lcore-mask`).
//! - PMDs go on **every NUMA node that has hugepages** (guest memory, and
//!   so the vhost-user ports, live there; a port is only polled by a PMD of
//!   its own node without a cross-NUMA penalty). Each node gets
//!   `ceil(candidate cores / 4)` PMDs, at most [`MAX_PMDS_PER_NODE`]: about
//!   a quarter of the cores, never more than the queues VMs will use.
//! - If `isolcpus`/`nohz_full` cores exist, prefer them (the operator
//!   already kept the scheduler off them).
//! - Fewer than two physical cores: no plan.

use crate::exec::Exec;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const MAX_PMDS_PER_NODE: usize = 4;
/// Per-node floor for `dpdk-socket-mem` (MiB): OVS's default shared
/// mempool at MTU 1500 is ~600 MiB.
pub const MIN_SOCKET_MEM_MB: u64 = 1024;
/// Largest default queue-pair count for a vhost-user NIC: matches
/// [`MAX_PMDS_PER_NODE`], since more queues than PMDs only adds lock
/// contention on the OVS side.
pub const MAX_DEFAULT_QUEUE_PAIRS: u8 = MAX_PMDS_PER_NODE as u8;
/// Descriptors per vhost-user virtqueue. The default 256 overflows under
/// bursts (OVS `tx_failure_drops` / `ovs_tx_retries`); 1024 is the largest
/// size OVS's vhost library and QEMU/CH accept for vhost-user.
pub const VHOST_QUEUE_SIZE: u32 = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cpu {
    pub id: u32,
    pub package: u32,
    pub core: u32,
    pub node: u32,
    /// All hyperthreads of this physical core, including `id`.
    pub siblings: Vec<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Topology {
    pub cpus: Vec<Cpu>,
    /// CPUs the kernel was told to keep tasks off (`isolcpus`, `nohz_full`).
    pub isolated: BTreeSet<u32>,
    /// NUMA nodes with hugepages reserved; empty when unknown.
    pub hugepage_nodes: BTreeSet<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub pmd_cpus: Vec<u32>,
    /// Non-PMD DPDK threads (and the OS) stay on these.
    pub lcore_cpus: Vec<u32>,
    /// NUMA nodes carrying PMDs, ascending.
    pub nodes: Vec<u32>,
    /// Highest NUMA node id on the host (for `dpdk-socket-mem` entries).
    pub max_node: u32,
}

impl Plan {
    pub fn pmd_mask(&self) -> String {
        hex_mask(&self.pmd_cpus)
    }
    pub fn lcore_mask(&self) -> String {
        hex_mask(&self.lcore_cpus)
    }
    /// `dpdk-socket-mem` value: `total_mb` split across the PMD nodes
    /// (each at least [`MIN_SOCKET_MEM_MB`]), `0` for the other nodes
    /// below the highest PMD node.
    pub fn socket_mem(&self, total_mb: u64) -> String {
        let n = self.nodes.len().max(1) as u64;
        let each = (total_mb / n).max(MIN_SOCKET_MEM_MB);
        let last = self.nodes.last().copied().unwrap_or(0);
        (0..=last)
            .map(|node| if self.nodes.contains(&node) { each } else { 0 }.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Hex mask with a `0x` prefix, e.g. CPUs {1, 2} → `0x6`.
pub fn hex_mask(cpus: &[u32]) -> String {
    let max = cpus.iter().copied().max().unwrap_or(0) as usize;
    let mut nibbles = vec![0u8; max / 4 + 1];
    for &c in cpus {
        nibbles[c as usize / 4] |= 1 << (c % 4);
    }
    let s: String = nibbles.iter().rev().map(|n| char::from_digit(*n as u32, 16).unwrap()).collect();
    let s = s.trim_start_matches('0');
    format!("0x{}", if s.is_empty() { "0" } else { s })
}

/// Number of CPUs in a hex mask (`0x3c` → 4).
pub fn mask_cpu_count(mask: &str) -> usize {
    mask.trim_start_matches("0x").chars().filter_map(|c| c.to_digit(16)).map(|d| d.count_ones() as usize).sum()
}

/// Parse a kernel CPU list: `0-3,8,10-11`.
pub fn parse_cpu_list(s: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        let mut it = part.splitn(2, '-');
        let a = it.next().and_then(|v| v.trim().parse::<u32>().ok());
        let b = it.next().map(|v| v.trim().parse::<u32>().ok());
        match (a, b) {
            (Some(a), None) => out.push(a),
            (Some(a), Some(Some(b))) if b >= a && b - a < 4096 => out.extend(a..=b),
            _ => {}
        }
    }
    out
}

/// `isolcpus=`/`nohz_full=` CPUs from a kernel command line. Flags such as
/// `isolcpus=managed_irq,domain,2-5` are skipped.
pub fn isolated_from_cmdline(cmdline: &str) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    for tok in cmdline.split_whitespace() {
        let Some(v) = tok.strip_prefix("isolcpus=").or_else(|| tok.strip_prefix("nohz_full=")) else { continue };
        for part in v.split(',') {
            if part.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                out.extend(parse_cpu_list(part));
            }
        }
    }
    out
}

pub fn read_topology(exec: &dyn Exec) -> Topology {
    let read = |p: String| exec.read_file(Path::new(&p)).ok();
    let mut t = Topology::default();
    let Some(online) = read("/sys/devices/system/cpu/online".into()) else { return t };
    let node_online = read("/sys/devices/system/node/online".into()).map(|s| parse_cpu_list(&s)).unwrap_or_else(|| vec![0]);
    let mut node_of = BTreeMap::new();
    for &n in &node_online {
        if let Some(list) = read(format!("/sys/devices/system/node/node{}/cpulist", n)) {
            for c in parse_cpu_list(&list) {
                node_of.insert(c, n);
            }
        }
    }
    for id in parse_cpu_list(&online) {
        let base = format!("/sys/devices/system/cpu/cpu{}/topology", id);
        let num = |f: &str| read(format!("{}/{}", base, f)).and_then(|s| s.trim().parse::<u32>().ok());
        let (Some(core), package) = (num("core_id"), num("physical_package_id").unwrap_or(0)) else { continue };
        let siblings = read(format!("{}/thread_siblings_list", base)).map(|s| parse_cpu_list(&s)).filter(|s| !s.is_empty()).unwrap_or_else(|| vec![id]);
        t.cpus.push(Cpu { id, package, core, node: node_of.get(&id).copied().unwrap_or(0), siblings });
    }
    t.isolated = read("/proc/cmdline".into()).map(|c| isolated_from_cmdline(&c)).unwrap_or_default();
    for &n in &node_online {
        let total = read(format!("/sys/devices/system/node/node{}/hugepages/hugepages-2048kB/nr_hugepages", n))
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if total > 0 {
            t.hugepage_nodes.insert(n);
        }
    }
    t
}

/// A physical core: its lowest hyperthread represents it.
struct Core {
    first_cpu: u32,
    node: u32,
    threads: Vec<u32>,
}

fn cores(t: &Topology) -> Vec<Core> {
    let mut by: BTreeMap<(u32, u32), Core> = BTreeMap::new();
    for c in &t.cpus {
        let e = by.entry((c.package, c.core)).or_insert(Core { first_cpu: c.id, node: c.node, threads: Vec::new() });
        e.first_cpu = e.first_cpu.min(c.id);
        e.threads.extend(c.siblings.iter().copied().chain([c.id]));
    }
    let mut v: Vec<Core> = by.into_values().collect();
    for c in &mut v {
        c.threads.sort_unstable();
        c.threads.dedup();
    }
    v.sort_by_key(|c| c.first_cpu);
    v
}

pub fn plan(t: &Topology) -> Option<Plan> {
    let all = cores(t);
    if all.len() < 2 {
        return None;
    }
    let max_node = all.iter().map(|c| c.node).max().unwrap_or(0);
    let reserved = &all[0];
    let rest: Vec<&Core> = all.iter().skip(1).collect();
    // Prefer cores the operator isolated, when there are any.
    let isolated: Vec<&Core> = rest.iter().copied().filter(|c| c.threads.iter().all(|th| t.isolated.contains(th))).collect();
    let pool = if isolated.is_empty() { rest } else { isolated };

    let mut nodes: BTreeSet<u32> = pool.iter().map(|c| c.node).collect();
    if !t.hugepage_nodes.is_empty() {
        let with_mem: BTreeSet<u32> = nodes.intersection(&t.hugepage_nodes).copied().collect();
        if !with_mem.is_empty() {
            nodes = with_mem;
        }
    }
    let mut pmd = Vec::new();
    for &n in &nodes {
        let cand: Vec<&&Core> = pool.iter().filter(|c| c.node == n).collect();
        let want = cand.len().div_ceil(4).clamp(1, MAX_PMDS_PER_NODE);
        pmd.extend(cand.iter().take(want).map(|c| c.first_cpu));
    }
    pmd.sort_unstable();
    Some(Plan { pmd_cpus: pmd, lcore_cpus: reserved.threads.clone(), nodes: nodes.into_iter().collect(), max_node })
}

/// Default queue pairs for a vhost-user NIC: one per vCPU, capped at
/// [`MAX_DEFAULT_QUEUE_PAIRS`]. Multi-queue is what lets several PMDs (and
/// several guest CPUs) work on one NIC; a single queue pins each direction
/// to one PMD and makes every other PMD take a lock to transmit to it.
pub fn default_vhost_queue_pairs(vcpus: u32) -> u8 {
    vcpus.clamp(1, MAX_DEFAULT_QUEUE_PAIRS as u32) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::RecordingExec;

    fn topo(cores: u32, threads: u32, nodes: u32) -> Topology {
        let mut t = Topology::default();
        let per_node = cores / nodes;
        for th in 0..threads {
            for core in 0..cores {
                let id = th * cores + core;
                t.cpus.push(Cpu {
                    id,
                    package: core / per_node,
                    core,
                    node: core / per_node,
                    siblings: (0..threads).map(|x| x * cores + core).collect(),
                });
            }
        }
        t.hugepage_nodes = (0..nodes).collect();
        t
    }

    #[test]
    fn masks() {
        assert_eq!(hex_mask(&[1, 2]), "0x6");
        assert_eq!(hex_mask(&[2, 3, 4, 5]), "0x3c");
        assert_eq!(hex_mask(&[0]), "0x1");
        assert_eq!(hex_mask(&[64]), format!("0x1{}", "0".repeat(16)));
        assert_eq!(mask_cpu_count("0x3c"), 4);
    }

    #[test]
    fn cpu_lists_and_isolation() {
        assert_eq!(parse_cpu_list("0-3,8,10-11\n"), [0, 1, 2, 3, 8, 10, 11]);
        let iso = isolated_from_cmdline("quiet isolcpus=managed_irq,domain,2-3 nohz_full=2-3,6 ro");
        assert_eq!(iso.into_iter().collect::<Vec<_>>(), [2, 3, 6]);
    }

    #[test]
    fn single_node_smt_host_like_this_one() {
        // 16 cores / 32 threads: core 0 (+ sibling 16) reserved, 15 left →
        // ceil(15/4) = 4 PMDs, one hyperthread each.
        let p = plan(&topo(16, 2, 1)).unwrap();
        assert_eq!(p.pmd_cpus, [1, 2, 3, 4]);
        assert_eq!(p.lcore_cpus, [0, 16]);
        assert_eq!(p.pmd_mask(), "0x1e");
        assert_eq!(p.lcore_mask(), "0x10001");
        assert_eq!(p.socket_mem(2048), "2048");
    }

    #[test]
    fn small_hosts() {
        assert_eq!(plan(&topo(1, 2, 1)), None);
        assert_eq!(plan(&topo(2, 1, 1)).unwrap().pmd_cpus, [1]);
        // 4 cores: 3 candidates → 1 PMD. 8 cores: 7 → 2.
        assert_eq!(plan(&topo(4, 1, 1)).unwrap().pmd_cpus.len(), 1);
        assert_eq!(plan(&topo(8, 1, 1)).unwrap().pmd_cpus.len(), 2);
        // Big hosts stop at 4 per node.
        assert_eq!(plan(&topo(64, 2, 1)).unwrap().pmd_cpus.len(), MAX_PMDS_PER_NODE);
    }

    #[test]
    fn numa_hosts_get_pmds_and_memory_on_every_hugepage_node() {
        let mut t = topo(16, 2, 2); // node 0: cores 0-7, node 1: cores 8-15
        let p = plan(&t).unwrap();
        assert_eq!(p.nodes, [0, 1]);
        // node 0: 7 candidates → 2; node 1: 8 → 2.
        assert_eq!(p.pmd_cpus, [1, 2, 8, 9]);
        assert_eq!(p.socket_mem(2048), "1024,1024");
        // Hugepages only on node 1: PMDs and memory only there.
        t.hugepage_nodes = BTreeSet::from([1]);
        let p = plan(&t).unwrap();
        assert_eq!(p.nodes, [1]);
        assert_eq!(p.pmd_cpus, [8, 9]);
        assert_eq!(p.socket_mem(2048), "0,2048");
    }

    #[test]
    fn isolated_cores_are_preferred() {
        let mut t = topo(8, 2, 1);
        t.isolated = BTreeSet::from([4, 5, 12, 13]); // cores 4,5 with their siblings
        let p = plan(&t).unwrap();
        assert_eq!(p.pmd_cpus, [4]); // 2 candidates → 1 PMD
    }

    #[test]
    fn queue_pairs_follow_vcpus() {
        assert_eq!(default_vhost_queue_pairs(1), 1);
        assert_eq!(default_vhost_queue_pairs(4), 4);
        assert_eq!(default_vhost_queue_pairs(64), 4);
        assert_eq!(default_vhost_queue_pairs(0), 1);
    }

    #[test]
    fn reads_topology_through_exec() {
        let exec = RecordingExec::new();
        exec.file("/sys/devices/system/cpu/online", "0-3\n");
        exec.file("/sys/devices/system/node/online", "0\n");
        exec.file("/sys/devices/system/node/node0/cpulist", "0-3\n");
        exec.file("/proc/cmdline", "ro isolcpus=3\n");
        for (cpu, core, sib) in [(0, 0, "0,2"), (1, 1, "1,3"), (2, 0, "0,2"), (3, 1, "1,3")] {
            let b = format!("/sys/devices/system/cpu/cpu{}/topology", cpu);
            exec.file(&format!("{}/core_id", b), &format!("{}\n", core));
            exec.file(&format!("{}/physical_package_id", b), "0\n");
            exec.file(&format!("{}/thread_siblings_list", b), &format!("{}\n", sib));
        }
        exec.file("/sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages", "1024\n");
        let t = read_topology(&exec);
        assert_eq!(t.cpus.len(), 4);
        assert_eq!(t.isolated, BTreeSet::from([3]));
        assert_eq!(t.hugepage_nodes, BTreeSet::from([0]));
        assert!(plan(&t).is_some());
        // Nothing readable: no plan.
        assert_eq!(plan(&read_topology(&RecordingExec::new())), None);
    }
}
