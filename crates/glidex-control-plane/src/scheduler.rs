//! The scheduler (spec/clustering.md §9.1): where a VM runs. It decides once;
//! after that the VM stays (D6).

use crate::cluster::config::SchedulerConfig;
use crate::models::{CpuPolicy, NumaPolicy, SMALL_PAGE_KIB, Tristate, hugepage_size_name};
use crate::node::{Node, NodePhase};
use crate::topology::TopologySummary;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// What a VM asks of a node.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub vcpus: u32,
    pub mem_mib: u64,
    /// `cloudhypervisor` or `qemu`, as nodes report their hypervisors.
    pub hypervisor: String,
    /// Guest RAM in hugepages, asked for (spec/host-topology.md §8a.3).
    pub hugepages: bool,
    /// Their size in KiB; `None`: the node's default size.
    pub hugepage_kib: Option<u64>,
    /// A vhost-user NIC: hugepages without asking, when the node has them
    /// to spare.
    pub vhost_user: bool,
    /// spec/host-topology.md H5 and H6.
    pub numa: NumaPolicy,
    pub cpu: CpuPolicy,
    /// `spec.node`: an id or a name.
    pub pin: Option<String>,
    /// The node of each disk the VM uses, `None` for one not bound yet.
    pub disk_nodes: Vec<Option<String>>,
    /// For each unbound disk on a registered datastore, the datastore's
    /// name and the nodes that reach it (spec/storage.md §5.2).
    pub disk_reach: Vec<(String, Vec<String>)>,
    /// The node of each `scope: node` network it attaches to.
    pub net_nodes: Vec<String>,
    /// VFIO devices, as the VM names them (sysfs paths or addresses).
    pub vfio: Vec<String>,
    /// The nodes where the project may use them; `None`: any.
    pub device_nodes: Option<Vec<String>>,
    /// Devices asked for by pool (spec/host-topology.md §9).
    pub pools: Vec<PoolRequest>,
    /// Physical networks (provider networks) the VM's NICs sit on.
    pub physnets: Vec<String>,
    /// A NIC on a cluster network is vhost-user: `br-int` must be userspace.
    pub vhost_user_on_cluster: bool,
    /// The cluster's ovn-northd version when a NIC is on a cluster (OVN)
    /// network: the node's ovn-controller must be that release or newer.
    pub ovn_northd: Option<String>,
    /// The VM's architecture (its image's, else its firmware's); empty:
    /// none (spec/clustering.md §9.2.1).
    pub arch: String,
}

/// What a node already carries: every VM placed on it, running or not.
#[derive(Debug, Clone, Default)]
pub struct Load {
    pub vcpus: u64,
    /// Guest RAM in ordinary memory.
    pub mem_mib: u64,
    pub vms: u64,
    /// PCI devices its VMs claim (spec/host-topology.md §8.4): address →
    /// the VM's name.
    pub devices: BTreeMap<String, String>,
    /// Guest RAM in hugepages: page size in KiB → MiB.
    pub hugepage_mib: BTreeMap<u64, u64>,
    /// vCPUs of dedicated VMs: not shared capacity's.
    pub dedicated_vcpus: u64,
    /// Host CPUs dedicated VMs hold.
    pub dedicated: BTreeSet<u32>,
    /// Guest RAM bound to each NUMA node: in ordinary memory, and in
    /// hugepages by page size (spec/host-topology.md §8.1).
    pub numa_mem_mib: BTreeMap<u32, u64>,
    pub numa_hugepage_mib: BTreeMap<u32, BTreeMap<u64, u64>>,
}

impl Load {
    fn hugepages_taken(&self, page_kib: u64) -> u64 {
        self.hugepage_mib.get(&page_kib).copied().unwrap_or(0)
    }
}

/// One VM placed on a node, for [`loads`].
#[derive(Debug, Clone, Default)]
pub struct Placed {
    pub node: String,
    pub vcpus: u64,
    pub mem_mib: u64,
    pub name: String,
    /// Its spec's VFIO devices and those its instance still holds.
    pub devices: Vec<String>,
    /// The page size its RAM is counted in ([`SMALL_PAGE_KIB`]: ordinary
    /// memory).
    pub page_kib: u64,
    /// The NUMA node its RAM is bound to, and the CPUs it holds
    /// (dedicated).
    pub numa_node: Option<u32>,
    pub cpus: Vec<u32>,
    /// `aligned` over several NUMA nodes: its share of each.
    pub numa: Vec<crate::models::NumaShare>,
}

/// A device's PCI address, from a sysfs path or an address: what nodes
/// report and claims compare (`0000:41:00.0`).
pub fn pci_address(device: &str) -> String {
    crate::hypervisor::vfio_bdf(device).to_ascii_lowercase()
}

/// Why `device` can't be given to another VM on a node whose VMs claim
/// `claimed` (address → VM name) and whose IOMMU groups are `groups`
/// (address → group): it is claimed, or it shares its group with a claimed
/// device (spec/host-topology.md §8.4). `None`: free.
pub fn device_taken(
    groups: &BTreeMap<String, String>,
    claimed: &BTreeMap<String, String>,
    device: &str,
) -> Option<String> {
    let addr = pci_address(device);
    if let Some(vm) = claimed.get(&addr) {
        return Some(format!("PCI device {addr} is used by VM {vm}"));
    }
    let group = groups.get(&addr)?;
    claimed.iter().find_map(|(other, vm)| {
        (groups.get(other) == Some(group)).then(|| {
            format!("PCI device {addr} shares IOMMU group {group} with {other}, used by VM {vm}")
        })
    })
}

/// MiB of `page_kib` hugepages VMs may take on `n`.
fn hugepage_capacity(n: &Node, page_kib: u64) -> u64 {
    n.status.allocatable.pages(page_kib) * page_kib / 1024
}

/// The node's default hugepage size; 2 MiB from a node that doesn't say.
fn default_page_kib(n: &Node) -> u64 {
    match n.status.features.hugepage_kib {
        0 => 2048,
        k => k,
    }
}

/// The page size guest RAM of `req` gets on `n` (spec/host-topology.md
/// §8a.3), or why it can't: asked-for hugepages must fit what is left of
/// the node's; a vhost-user VM gets 2 MiB pages when they cover its RAM
/// plus a quarter (at least 256 MiB) to spare, else ordinary memory.
pub fn page_kib(n: &Node, load: &Load, req: &Request) -> Result<u64, String> {
    if req.hugepages {
        let kib = req.hugepage_kib.unwrap_or_else(|| default_page_kib(n));
        let name = hugepage_size_name(kib);
        let cap = hugepage_capacity(n, kib);
        if cap == 0 {
            return Err(format!("no {name} hugepages"));
        }
        if !req.mem_mib.is_multiple_of((kib / 1024).max(1)) {
            return Err(format!(
                "{} MiB is not a whole number of {name} pages",
                req.mem_mib
            ));
        }
        let taken = load.hugepages_taken(kib);
        if taken + req.mem_mib > cap {
            return Err(format!(
                "not enough {name} hugepages ({taken} of {cap} MiB taken)"
            ));
        }
        return Ok(kib);
    }
    if req.vhost_user && req.mem_mib.is_multiple_of(2) {
        let spare = hugepage_capacity(n, 2048).saturating_sub(load.hugepages_taken(2048));
        if spare >= req.mem_mib + (req.mem_mib / 4).max(256) {
            return Ok(2048);
        }
    }
    Ok(SMALL_PAGE_KIB)
}

/// `count` devices of a pool, as the scheduler sees the request.
#[derive(Debug, Clone)]
pub struct PoolRequest {
    pub pool: crate::config::PciPool,
    pub count: u32,
    pub same_switch: bool,
}

/// The devices `req`'s pool requests get on `n` (spec/host-topology.md
/// §8.2 4, §8.3): of each pool's devices there that are ready, not
/// cordoned, and not claimed (with their IOMMU groups), those on the NUMA
/// node of the VM's other devices first, then by address; with
/// `same_switch`, all under one switch, the one shared with the VM's other
/// devices first.
pub fn pick_pool_devices(
    n: &Node,
    load: &Load,
    req: &Request,
) -> Result<Vec<crate::models::PoolDevice>, String> {
    if req.pools.is_empty() {
        return Ok(Vec::new());
    }
    let t = n
        .status
        .topology
        .as_ref()
        .ok_or_else(|| "it reports no topology".to_string())?;
    let mut taken: Vec<String> = req.vfio.iter().map(|d| pci_address(d)).collect();
    let mut numa = taken.iter().find_map(|a| t.device_numa.get(a).copied());
    let mut switches: Vec<String> = t
        .devices
        .iter()
        .filter(|d| taken.contains(&d.address))
        .filter_map(|d| d.switch.clone())
        .collect();
    let mut out = Vec::new();
    for r in &req.pools {
        let p = &r.pool;
        if !(p.nodes.is_empty() || p.nodes.iter().any(|x| *x == n.meta.id || *x == n.spec.name)) {
            return Err(format!("pool {} is not on this node", p.name));
        }
        let mut free: Vec<&crate::topology::SummaryDevice> = t
            .devices
            .iter()
            .filter(|d| p.matches.matches_device(d))
            .filter(|d| {
                d.ready
                    && !n.spec.cordoned_devices.contains(&d.address)
                    && !taken.contains(&d.address)
                    && device_taken(&n.status.pci_groups, &load.devices, &d.address).is_none()
            })
            .collect();
        if effective_numa(req) == NumaPolicy::Aligned {
            // As few NUMA nodes as can hold them: the node of devices it
            // has first, then those with the most free (spec/gpu-vms.md §4).
            let mut per_node: BTreeMap<Option<u32>, usize> = BTreeMap::new();
            for d in &free {
                *per_node.entry(d.numa_node).or_default() += 1;
            }
            free.sort_by_key(|d| {
                (
                    numa.is_some() && d.numa_node != numa,
                    std::cmp::Reverse(per_node.get(&d.numa_node).copied().unwrap_or(0)),
                    d.numa_node,
                    d.address.clone(),
                )
            });
        } else {
            free.sort_by_key(|d| (numa.is_some() && d.numa_node != numa, d.address.clone()));
        }
        let need = r.count as usize;
        if free.len() < need {
            return Err(format!("pool {}: {} of {} free", p.name, free.len(), need));
        }
        let picked: Vec<&crate::topology::SummaryDevice> = if r.same_switch {
            let mut groups: BTreeMap<String, Vec<&crate::topology::SummaryDevice>> =
                BTreeMap::new();
            for d in &free {
                if let Some(s) = &d.switch {
                    groups.entry(s.clone()).or_default().push(d);
                }
            }
            let mut fits: Vec<(String, Vec<&crate::topology::SummaryDevice>)> = groups
                .into_iter()
                .filter(|(_, g)| g.len() >= need)
                .collect();
            fits.sort_by_key(|(s, g)| {
                (
                    !switches.contains(s),
                    numa.is_some() && g[0].numa_node != numa,
                    g[0].address.clone(),
                )
            });
            let Some((_, g)) = fits.into_iter().next() else {
                return Err(format!(
                    "pool {}: no {need} free devices under one PCIe switch",
                    p.name
                ));
            };
            g.into_iter().take(need).collect()
        } else {
            free.into_iter().take(need).collect()
        };
        for d in picked {
            numa = numa.or(d.numa_node);
            if let Some(s) = &d.switch {
                switches.push(s.clone());
            }
            taken.push(d.address.clone());
            out.push(crate::models::PoolDevice {
                pool: p.name.clone(),
                address: d.address.clone(),
            });
        }
    }
    Ok(out)
}

/// Where on a node a VM goes (spec/host-topology.md §8.2–8.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fit {
    /// The page size its RAM is counted in.
    pub page_kib: u64,
    /// The NUMA node its RAM is bound to.
    pub numa_node: Option<u32>,
    /// Dedicated: the host CPUs it holds, vCPU `i` on `cpus[i]`.
    pub cpus: Vec<u32>,
    /// `false`: a `preferred` VM that no single NUMA node could take.
    pub numa_aligned: bool,
    /// The devices picked for its pool requests.
    pub pool_devices: Vec<crate::models::PoolDevice>,
    /// `aligned` over several NUMA nodes: each node's share, in guest NUMA
    /// node order (spec/gpu-vms.md §4).
    pub numa: Vec<crate::models::NumaShare>,
}

/// The NUMA policy that applies: dedicated CPUs imply `preferred`.
fn effective_numa(req: &Request) -> NumaPolicy {
    match (req.numa, req.cpu) {
        (NumaPolicy::None, CpuPolicy::Dedicated) => NumaPolicy::Preferred,
        (p, _) => p,
    }
}

/// CPUs no VM's dedicated vCPUs get on a node: what the node reserves,
/// and, unless the operator listed its own, the first core of each NUMA
/// node for the host (H7).
fn reserved_cpus(t: &TopologySummary) -> BTreeSet<u32> {
    let mut r: BTreeSet<u32> = t.reserved_cpus.keys().copied().collect();
    if !t.reserved_by_config {
        for n in &t.numa_nodes {
            if let Some(core) = n.cores.first() {
                r.extend(core.iter().copied());
            }
        }
    }
    r
}

/// The CPUs a shared VM runs on when a node holds dedicated ones: all but
/// the reserved and the dedicated. Empty: no restriction.
pub fn shared_pool(n: &Node, held: &BTreeSet<u32>) -> Vec<u32> {
    let Some(t) = &n.status.topology else {
        return Vec::new();
    };
    if held.is_empty() {
        return Vec::new();
    }
    let reserved = reserved_cpus(t);
    t.numa_nodes
        .iter()
        .flat_map(|k| k.cpus())
        .filter(|c| !reserved.contains(c) && !held.contains(c))
        .collect()
}

/// Pick `vcpus` dedicated CPUs from `cores` (each its threads): isolated
/// cores first, then by id. With whole cores, every thread of a picked
/// core is held, core by core, so vCPUs `2i` and `2i+1` share a core and
/// an odd count leaves the last sibling idle. `None`: not enough.
fn pick_cpus(
    cores: &[&Vec<u32>],
    vcpus: usize,
    taken: &BTreeSet<u32>,
    isolated: &BTreeSet<u32>,
    whole_cores: bool,
) -> Option<Vec<u32>> {
    let mut cores: Vec<&Vec<u32>> = cores.to_vec();
    cores.sort_by_key(|c| (!c.iter().all(|t| isolated.contains(t)), c.first().copied()));
    if whole_cores {
        let mut held = Vec::new();
        for c in cores
            .iter()
            .filter(|c| c.iter().all(|t| !taken.contains(t)))
        {
            if held.len() >= vcpus {
                break;
            }
            held.extend(c.iter().copied());
        }
        (held.len() >= vcpus).then_some(held)
    } else {
        let free: Vec<u32> = cores
            .iter()
            .flat_map(|c| c.iter().copied())
            .filter(|t| !taken.contains(t))
            .take(vcpus)
            .collect();
        (free.len() == vcpus).then_some(free)
    }
}

/// How `req` fits on NUMA node `k` of `n`: the page size (implicit
/// hugepages fall back to ordinary memory when the node lacks them) and the
/// dedicated CPUs, or why not.
fn fit_on_numa(
    t: &TopologySummary,
    k: u32,
    load: &Load,
    req: &Request,
    cfg: &SchedulerConfig,
    page: u64,
    taken: &BTreeSet<u32>,
) -> Result<(u64, Vec<u32>, f64), String> {
    let node = t.node(k).ok_or_else(|| format!("no NUMA node {k}"))?;
    let total_mem: u64 = t
        .numa_nodes
        .iter()
        .map(|x| x.memory_mib)
        .sum::<u64>()
        .max(1);
    let share = node.memory_mib as f64 / total_mem as f64;
    let bound_mem: u64 = load.numa_mem_mib.values().sum();
    let mem_taken = load.numa_mem_mib.get(&k).copied().unwrap_or(0) as f64
        + load.mem_mib.saturating_sub(bound_mem) as f64 * share;
    let mem_cap = node.memory_mib as f64 * cfg.memory_overcommit;
    let small_fits = mem_taken + req.mem_mib as f64 <= mem_cap;
    let mut page = page;
    let mut used = (mem_taken + req.mem_mib as f64) / mem_cap.max(1.0);
    if page == SMALL_PAGE_KIB {
        if !small_fits {
            return Err(format!(
                "NUMA node {k}: not enough memory ({mem_taken:.0} of {mem_cap:.0} MiB taken)"
            ));
        }
    } else {
        let cap = (node.pages(page) * page / 1024) as f64;
        let total_pages: u64 = t
            .numa_nodes
            .iter()
            .map(|x| x.pages(page))
            .sum::<u64>()
            .max(1);
        let bound: u64 = load
            .numa_hugepage_mib
            .values()
            .filter_map(|m| m.get(&page))
            .sum();
        let taken_hp = load
            .numa_hugepage_mib
            .get(&k)
            .and_then(|m| m.get(&page))
            .copied()
            .unwrap_or(0) as f64
            + load.hugepages_taken(page).saturating_sub(bound) as f64 * node.pages(page) as f64
                / total_pages as f64;
        let spare = cap - taken_hp;
        let fits = if req.hugepages {
            spare >= req.mem_mib as f64
        } else {
            spare >= (req.mem_mib + (req.mem_mib / 4).max(256)) as f64
        };
        if fits {
            used = (taken_hp + req.mem_mib as f64) / cap.max(1.0);
        } else if !req.hugepages && small_fits {
            page = SMALL_PAGE_KIB;
        } else {
            return Err(format!(
                "NUMA node {k}: not enough {} hugepages ({taken_hp:.0} of {cap:.0} MiB taken)",
                hugepage_size_name(page)
            ));
        }
    }
    let cpus = if req.cpu == CpuPolicy::Dedicated {
        let cores: Vec<&Vec<u32>> = node.cores.iter().collect();
        let isolated: BTreeSet<u32> = t.isolated.iter().copied().collect();
        pick_cpus(
            &cores,
            req.vcpus as usize,
            taken,
            &isolated,
            cfg.dedicated_whole_cores,
        )
        .ok_or_else(|| {
            let free = node.cpus().filter(|c| !taken.contains(c)).count();
            format!(
                "NUMA node {k}: not enough free CPUs ({} dedicated vCPUs, {free} CPUs free)",
                req.vcpus
            )
        })?
    } else {
        Vec::new()
    };
    Ok((page, cpus, used))
}

/// `aligned` (spec/gpu-vms.md §4): exactly the NUMA nodes of the VM's
/// devices, its vCPUs and memory split between them by their device counts
/// (largest remainder), each share fitting its node. One node: an ordinary
/// fit there. No devices: as `preferred`.
fn aligned_fit(
    t: &TopologySummary,
    load: &Load,
    req: &Request,
    cfg: &SchedulerConfig,
    page: u64,
) -> Result<Fit, String> {
    let mut by_node: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for d in &req.vfio {
        let addr = pci_address(d);
        match t.device_numa.get(&addr) {
            Some(k) => by_node.entry(*k).or_default().push(addr),
            None => return Err(format!("PCI device {addr} has no NUMA node")),
        }
    }
    let mut taken = reserved_cpus(t);
    taken.extend(load.dedicated.iter().copied());
    if by_node.is_empty() {
        return Err("aligned needs devices to align with".into());
    }
    let weights: Vec<u64> = by_node.values().map(|v| v.len() as u64).collect();
    if (req.vcpus as usize) < by_node.len() {
        return Err(format!(
            "its devices are on {} NUMA nodes, so it needs at least {} vCPUs",
            by_node.len(),
            by_node.len()
        ));
    }
    let vcpus = split_by(req.vcpus as u64, &weights);
    let mems = split_by(req.mem_mib, &weights);
    let one = by_node.len() == 1;
    let (mut pins, mut siblings, mut shares) = (Vec::new(), Vec::new(), Vec::new());
    let mut page_used: Option<(u32, u64)> = None;
    for (i, (k, devices)) in by_node.into_iter().enumerate() {
        let mut sub = req.clone();
        sub.vcpus = vcpus[i] as u32;
        sub.mem_mib = mems[i];
        let (p, cpus, _) = fit_on_numa(t, k, load, &sub, cfg, page, &taken)?;
        if let Some((k0, p0)) = page_used
            && p0 != p
        {
            return Err(format!(
                "NUMA nodes {k0} and {k} would back its memory with different page sizes"
            ));
        }
        page_used = Some((k, p));
        let n = (vcpus[i] as usize).min(cpus.len());
        pins.extend_from_slice(&cpus[..n]);
        siblings.extend_from_slice(&cpus[n..]);
        taken.extend(cpus.iter().copied());
        shares.push(crate::models::NumaShare {
            node: k,
            vcpus: vcpus[i] as u32,
            memory_mib: mems[i],
            devices,
        });
    }
    // vCPU i on cpus[i] in guest-node order; whole cores' siblings last.
    pins.extend(siblings);
    let page_kib = page_used.map(|(_, p)| p).unwrap_or(page);
    Ok(if one {
        Fit {
            page_kib,
            numa_node: Some(shares[0].node),
            cpus: pins,
            numa_aligned: true,
            ..Default::default()
        }
    } else {
        Fit {
            page_kib,
            numa_node: None,
            cpus: pins,
            numa_aligned: true,
            numa: shares,
            ..Default::default()
        }
    })
}

/// `total` split by `weights`, largest remainder first; sums to `total`.
fn split_by(total: u64, weights: &[u64]) -> Vec<u64> {
    let sum: u64 = weights.iter().sum::<u64>().max(1);
    let mut parts: Vec<u64> = weights.iter().map(|w| total * w / sum).collect();
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse((total * weights[i]) % sum), i));
    let mut left = total - parts.iter().sum::<u64>();
    for i in order {
        if left == 0 {
            break;
        }
        parts[i] += 1;
        left -= 1;
    }
    parts
}

/// Where on `n` a VM of `req` goes under its NUMA and CPU policies
/// (spec/host-topology.md §8.2): the NUMA node holding its devices and
/// enough memory and CPUs, the most devices then the least allocated
/// first; `strict` refuses when none can, `preferred` falls back to the
/// whole node.
pub fn numa_fit(
    n: &Node,
    load: &Load,
    req: &Request,
    cfg: &SchedulerConfig,
    page: u64,
) -> Result<Fit, String> {
    let mut policy = effective_numa(req);
    // `aligned` without devices has nothing to align with: as `preferred`.
    if policy == NumaPolicy::Aligned && req.vfio.is_empty() {
        policy = NumaPolicy::Preferred;
    }
    if policy == NumaPolicy::None && req.cpu == CpuPolicy::Shared {
        return Ok(Fit {
            page_kib: page,
            numa_aligned: true,
            ..Default::default()
        });
    }
    let t = n
        .status
        .topology
        .as_ref()
        .ok_or_else(|| "it reports no topology".to_string())?;
    let multi = t.numa_nodes.len() > 1;
    let mut device_nodes: BTreeMap<u32, usize> = BTreeMap::new();
    for d in &req.vfio {
        let addr = pci_address(d);
        match t.device_numa.get(&addr) {
            Some(k) => *device_nodes.entry(*k).or_default() += 1,
            None if multi && policy == NumaPolicy::Strict => {
                return Err(format!("PCI device {addr} has no NUMA node"));
            }
            None => {}
        }
    }
    if policy == NumaPolicy::Aligned {
        return aligned_fit(t, load, req, cfg, page);
    }
    if policy == NumaPolicy::Strict && device_nodes.len() > 1 {
        let ks: Vec<String> = device_nodes.keys().map(|k| k.to_string()).collect();
        return Err(format!(
            "its PCI devices are on NUMA nodes {}",
            ks.join(" and ")
        ));
    }
    let mut taken = reserved_cpus(t);
    taken.extend(load.dedicated.iter().copied());
    let mut candidates: Vec<u32> = t
        .numa_nodes
        .iter()
        .map(|x| x.id)
        .filter(|k| {
            policy != NumaPolicy::Strict || device_nodes.is_empty() || device_nodes.contains_key(k)
        })
        .collect();
    candidates.sort_by_key(|k| std::cmp::Reverse(device_nodes.get(k).copied().unwrap_or(0)));
    let mut best: Option<(usize, f64, u32, u64, Vec<u32>)> = None;
    let mut first_err = None;
    for k in candidates {
        match fit_on_numa(t, k, load, req, cfg, page, &taken) {
            Ok((p, cpus, used)) => {
                let devs = device_nodes.get(&k).copied().unwrap_or(0);
                let better = match &best {
                    None => true,
                    Some((bd, bu, bk, ..)) => {
                        (std::cmp::Reverse(devs), used, k) < (std::cmp::Reverse(*bd), *bu, *bk)
                    }
                };
                if better {
                    best = Some((devs, used, k, p, cpus));
                }
            }
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    if let Some((_, _, k, p, cpus)) = best {
        return Ok(Fit {
            page_kib: p,
            numa_node: Some(k),
            cpus,
            numa_aligned: true,
            pool_devices: Vec::new(),
            numa: Vec::new(),
        });
    }
    if policy == NumaPolicy::Strict {
        return Err(first_err.unwrap_or_else(|| "no NUMA node fits".into()));
    }
    // `preferred` that no NUMA node takes: anywhere on the node.
    let cpus = if req.cpu == CpuPolicy::Dedicated {
        let cores: Vec<&Vec<u32>> = t.numa_nodes.iter().flat_map(|x| x.cores.iter()).collect();
        let isolated: BTreeSet<u32> = t.isolated.iter().copied().collect();
        pick_cpus(
            &cores,
            req.vcpus as usize,
            &taken,
            &isolated,
            cfg.dedicated_whole_cores,
        )
        .ok_or_else(|| format!("not enough free CPUs for {} dedicated vCPUs", req.vcpus))?
    } else {
        Vec::new()
    };
    Ok(Fit {
        page_kib: page,
        numa_node: None,
        cpus,
        numa_aligned: false,
        pool_devices: Vec::new(),
        numa: Vec::new(),
    })
}

/// Where on `n` a VM of `req` goes once `n` passed the node filters: its
/// pool devices, then the NUMA fit with them as the VM's devices
/// (spec/host-topology.md §8.2). Also what a standalone host and a change
/// to a placed VM run.
pub fn place_on(
    n: &Node,
    load: &Load,
    req: &Request,
    cfg: &SchedulerConfig,
    page: u64,
) -> Result<Fit, String> {
    let pool_devices = pick_pool_devices(n, load, req)?;
    let with_pools;
    let req = if pool_devices.is_empty() {
        req
    } else {
        let mut r = req.clone();
        r.vfio
            .extend(pool_devices.iter().map(|d| d.address.clone()));
        with_pools = r;
        &with_pools
    };
    let mut fit = numa_fit(n, load, req, cfg, page)?;
    fit.pool_devices = pool_devices;
    Ok(fit)
}

/// The page size `req` gets on `n`, or why it can't go there.
fn failing(n: &Node, load: &Load, req: &Request, cfg: &SchedulerConfig) -> Result<Fit, String> {
    if let Some(pin) = &req.pin
        && &n.meta.id != pin
        && &n.spec.name != pin
    {
        return Err(format!("pinned to {pin}"));
    }
    if n.status.phase != NodePhase::Active || n.spec.unschedulable {
        return Err(if n.spec.unschedulable {
            "draining"
        } else {
            "not active"
        }
        .into());
    }
    if n.status.ready != Tristate::True {
        return Err("not ready".into());
    }
    // Shared vCPUs go on what dedicated ones leave (spec/host-topology.md
    // §8.1).
    let shared_vcpus = load.vcpus.saturating_sub(load.dedicated_vcpus);
    let shared_cap = |held: usize| {
        n.status.allocatable.cpus.saturating_sub(held as u32) as f64 * cfg.cpu_overcommit
    };
    let mem_cap = n.status.allocatable.memory_mib as f64 * cfg.memory_overcommit;
    if req.cpu == CpuPolicy::Shared
        && (shared_vcpus + req.vcpus as u64) as f64 > shared_cap(load.dedicated.len())
    {
        return Err(format!(
            "not enough CPU ({} of {:.0} vCPUs taken)",
            shared_vcpus,
            shared_cap(load.dedicated.len())
        ));
    }
    let page = page_kib(n, load, req)?;
    // RAM in hugepages doesn't take ordinary memory.
    let mem = if page == SMALL_PAGE_KIB {
        req.mem_mib
    } else {
        0
    };
    if (load.mem_mib + mem) as f64 > mem_cap {
        return Err(format!(
            "not enough memory ({} of {:.0} MiB taken)",
            load.mem_mib, mem_cap
        ));
    }
    if !req.hypervisor.is_empty() && !n.status.features.hypervisors.contains(&req.hypervisor) {
        return Err(format!("{} is not installed", req.hypervisor));
    }
    if !n.status.features.runs_arch(&req.arch) {
        return Err(format!(
            "it is {}; the VM's image is {}",
            n.status.features.arch, req.arch
        ));
    }
    for p in &req.physnets {
        if !n.status.features.physnets.contains_key(p) {
            return Err(format!("it does not reach physical network {p}"));
        }
    }
    if let Some(why) = crate::node::ovn_outdated(n, req.ovn_northd.as_deref()) {
        return Err(why);
    }
    if req.vhost_user_on_cluster && n.status.features.br_int_datapath.as_deref() != Some("netdev") {
        return Err("br-int is not on the userspace (netdev) datapath".into());
    }
    for d in &req.vfio {
        // The VM names a sysfs path; nodes report addresses.
        let addr = pci_address(d);
        if !n
            .status
            .pci_devices
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&addr))
        {
            return Err(format!("no PCI device {addr}"));
        }
        if n.spec.cordoned_devices.contains(&addr) {
            return Err(format!("PCI device {addr} is cordoned"));
        }
        if let Some(why) = device_taken(&n.status.pci_groups, &load.devices, d) {
            return Err(why);
        }
    }
    if req
        .device_nodes
        .as_ref()
        .is_some_and(|ns| !ns.contains(&n.meta.id))
    {
        return Err("its PCI devices are not granted to the project on this node".into());
    }
    for dn in req.disk_nodes.iter().flatten() {
        if dn != &n.meta.id {
            return Err("a disk is bound to another node".into());
        }
    }
    for (ds, nodes) in &req.disk_reach {
        if !nodes.contains(&n.meta.id) {
            return Err(format!("datastore {ds} is not reachable here"));
        }
    }
    for nn in &req.net_nodes {
        if nn != &n.meta.id {
            return Err("a network exists on another node".into());
        }
    }
    let fit = place_on(n, load, req, cfg, page)?;
    // Implicit hugepages the NUMA node lacked: ordinary memory after all.
    if fit.page_kib == SMALL_PAGE_KIB
        && page != SMALL_PAGE_KIB
        && (load.mem_mib + req.mem_mib) as f64 > mem_cap
    {
        return Err(format!(
            "not enough memory ({} of {:.0} MiB taken)",
            load.mem_mib, mem_cap
        ));
    }
    if !fit.cpus.is_empty() {
        let held = load.dedicated.len() + fit.cpus.len();
        if shared_vcpus as f64 > shared_cap(held) {
            return Err(format!(
                "{} dedicated CPUs would leave too few for the shared vCPUs ({} of {:.0})",
                fit.cpus.len(),
                shared_vcpus,
                shared_cap(held)
            ));
        }
    }
    Ok(fit)
}

/// One node's answer to a request (`POST /scheduler/preview`,
/// spec/clustering-ui.md §5.1).
#[derive(Debug, Clone, serde::Serialize)]
pub struct NodeFit {
    pub id: String,
    pub name: String,
    /// Why the VM can't go there; `None`: it fits.
    pub refused: Option<String>,
    /// What VMs placed there already take, and what may be taken (allocatable
    /// times the overcommit ratios).
    pub vcpus_taken: u64,
    pub vcpus_capacity: u64,
    pub mem_mib_taken: u64,
    pub mem_mib_capacity: u64,
    pub vms: u64,
    /// The page size in KiB its RAM would be in ([`SMALL_PAGE_KIB`]:
    /// ordinary memory), and for hugepages, what is taken of that size and
    /// may be taken.
    pub page_kib: u64,
    pub hugepages_mib_taken: u64,
    pub hugepages_mib_capacity: u64,
    /// The NUMA node, dedicated CPUs and alignment it would get
    /// (spec/host-topology.md §8.3).
    pub numa_node: Option<u32>,
    pub cpus: Vec<u32>,
    pub numa_aligned: bool,
    pub pool_devices: Vec<crate::models::PoolDevice>,
    pub numa: Vec<crate::models::NumaShare>,
    /// `max(cpu share, memory share, hugepage share)` after placing: lower
    /// is preferred.
    #[serde(skip)]
    score: f64,
}

/// Every node's answer to `req`, in the order the scheduler prefers them:
/// those that fit by least allocated (`max(cpu share, memory share)`), then
/// fewest VMs, then name; then those that don't, by name.
pub fn evaluate(req: &Request, nodes: &[(Node, Load)], cfg: &SchedulerConfig) -> Vec<NodeFit> {
    let mut out: Vec<NodeFit> = nodes
        .iter()
        .map(|(n, load)| {
            let (refused, fit) = match failing(n, load, req, cfg) {
                Ok(f) => (None, f),
                Err(why) => (
                    Some(why),
                    Fit {
                        page_kib: page_kib(n, load, req).unwrap_or(SMALL_PAGE_KIB),
                        numa_aligned: true,
                        ..Default::default()
                    },
                ),
            };
            let page = fit.page_kib;
            let small = page == SMALL_PAGE_KIB;
            let cpu =
                (load.vcpus + req.vcpus as u64) as f64 / (n.status.allocatable.cpus.max(1) as f64);
            let mem = (load.mem_mib + if small { req.mem_mib } else { 0 }) as f64
                / (n.status.allocatable.memory_mib.max(1) as f64);
            let (hp_taken, hp_cap) = if small {
                (0, 0)
            } else {
                (load.hugepages_taken(page), hugepage_capacity(n, page))
            };
            let hp = if small {
                0.0
            } else {
                (hp_taken + req.mem_mib) as f64 / (hp_cap.max(1) as f64)
            };
            NodeFit {
                id: n.meta.id.clone(),
                name: n.spec.name.clone(),
                refused,
                vcpus_taken: load.vcpus,
                numa_node: fit.numa_node,
                cpus: fit.cpus,
                numa_aligned: fit.numa_aligned,
                pool_devices: fit.pool_devices,
                numa: fit.numa,
                vcpus_capacity: (n.status.allocatable.cpus as f64 * cfg.cpu_overcommit) as u64,
                mem_mib_taken: load.mem_mib,
                mem_mib_capacity: (n.status.allocatable.memory_mib as f64 * cfg.memory_overcommit)
                    as u64,
                vms: load.vms,
                page_kib: page,
                hugepages_mib_taken: hp_taken,
                hugepages_mib_capacity: hp_cap,
                score: cpu.max(mem).max(hp),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.refused
            .is_some()
            .cmp(&b.refused.is_some())
            .then(if a.refused.is_none() {
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            } else {
                std::cmp::Ordering::Equal
            })
            .then(a.vms.cmp(&b.vms))
            .then(a.name.cmp(&b.name))
    });
    out
}

/// Where a VM goes: the node, and the page size its RAM is counted in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub node: String,
    pub page_kib: u64,
    pub numa_node: Option<u32>,
    pub cpus: Vec<u32>,
    pub numa_aligned: bool,
    pub pool_devices: Vec<crate::models::PoolDevice>,
    pub numa: Vec<crate::models::NumaShare>,
}

/// The node a VM goes to, or why each node refused it (the first of
/// [`evaluate`], so the answer is deterministic).
pub fn schedule(
    req: &Request,
    nodes: &[(Node, Load)],
    cfg: &SchedulerConfig,
) -> Result<Choice, Vec<(String, String)>> {
    let all = evaluate(req, nodes, cfg);
    match all.first() {
        Some(f) if f.refused.is_none() => Ok(Choice {
            node: f.id.clone(),
            page_kib: f.page_kib,
            numa_node: f.numa_node,
            cpus: f.cpus.clone(),
            numa_aligned: f.numa_aligned,
            pool_devices: f.pool_devices.clone(),
            numa: f.numa.clone(),
        }),
        _ => {
            let mut refused: Vec<(String, String)> = all
                .into_iter()
                .filter_map(|f| f.refused.map(|r| (f.name, r)))
                .collect();
            refused.sort_by(|a, b| a.0.cmp(&b.0));
            Err(refused)
        }
    }
}

/// `Unschedulable`'s message: each node's reason, capped.
pub fn explain(refused: &[(String, String)]) -> String {
    let mut parts: Vec<String> = refused
        .iter()
        .take(8)
        .map(|(n, w)| format!("{n}: {w}"))
        .collect();
    if refused.len() > 8 {
        parts.push(format!("and {} more", refused.len() - 8));
    }
    if parts.is_empty() {
        "there are no nodes".into()
    } else {
        parts.join("; ")
    }
}

/// Load per node from every placed VM.
pub fn loads(placed: impl IntoIterator<Item = Placed>) -> HashMap<String, Load> {
    let mut m: HashMap<String, Load> = HashMap::new();
    for p in placed {
        let l = m.entry(p.node).or_default();
        l.vcpus += p.vcpus;
        if p.page_kib == SMALL_PAGE_KIB {
            l.mem_mib += p.mem_mib;
        } else {
            *l.hugepage_mib.entry(p.page_kib).or_default() += p.mem_mib;
        }
        let bound: Vec<(u32, u64)> = match p.numa_node {
            Some(k) => vec![(k, p.mem_mib)],
            None => p.numa.iter().map(|s| (s.node, s.memory_mib)).collect(),
        };
        for (k, mib) in bound {
            if p.page_kib == SMALL_PAGE_KIB {
                *l.numa_mem_mib.entry(k).or_default() += mib;
            } else {
                *l.numa_hugepage_mib
                    .entry(k)
                    .or_default()
                    .entry(p.page_kib)
                    .or_default() += mib;
            }
        }
        if !p.cpus.is_empty() {
            l.dedicated_vcpus += p.vcpus;
            l.dedicated.extend(p.cpus.iter().copied());
        }
        l.vms += 1;
        for d in &p.devices {
            l.devices.insert(pci_address(d), p.name.clone());
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{NodeRole, NodeSpec, Resources};

    fn node(id: &str, cpus: u32, mem: u64) -> Node {
        let mut n = Node::new(
            id,
            NodeSpec {
                name: id.into(),
                role: NodeRole::Agent,
                unschedulable: false,
                labels: Default::default(),
                br_int_datapath: Default::default(),
                bridge_mappings: Default::default(),
                drop_spare_files_before: None,
                cordoned_devices: Default::default(),
            },
        );
        n.status.allocatable = Resources {
            cpus,
            memory_mib: mem,
            hugepages: Default::default(),
        };
        n.status.features.hypervisors = vec!["cloudhypervisor".into(), "qemu".into()];
        n
    }

    fn req(cpus: u32, mem: u64) -> Request {
        Request {
            vcpus: cpus,
            mem_mib: mem,
            hypervisor: "cloudhypervisor".into(),
            ..Default::default()
        }
    }

    fn cfg() -> SchedulerConfig {
        SchedulerConfig {
            cpu_overcommit: 4.0,
            memory_overcommit: 1.0,
            dedicated_whole_cores: true,
        }
    }

    #[test]
    fn a_vm_on_a_cluster_network_needs_an_ovn_controller_as_new_as_northd() {
        let ovn = |id: &str, role: NodeRole, v: &str, central: bool| {
            let mut n = node(id, 8, 16384);
            n.spec.role = role;
            n.status.versions.ovn = Some(v.into());
            n.status.features.ovn_central = central;
            n
        };
        let server = ovn("s1", NodeRole::Server, "26.03.0", true);
        let old = ovn("a-old", NodeRole::Agent, "24.03.8", false);
        let same = ovn("a-same", NodeRole::Agent, "26.03.0", false);
        let newer = ovn("a-newer", NodeRole::Agent, "26.09.1", false);
        // An agent's newer OVN doesn't count: only a server runs northd.
        let northd = crate::node::cluster_northd_ovn(&[
            server.clone(),
            old.clone(),
            same.clone(),
            newer.clone(),
        ]);
        assert_eq!(northd.as_deref(), Some("26.03.0"));
        assert_eq!(
            crate::node::cluster_northd_ovn(std::slice::from_ref(&old)),
            None
        );

        let nodes = vec![
            (server, Load::default()),
            (old, Load::default()),
            (same, Load::default()),
            (newer, Load::default()),
        ];
        let mut r = req(2, 1024);
        r.ovn_northd = northd;
        let fits = evaluate(&r, &nodes, &cfg());
        let refused = |id: &str| fits.iter().find(|f| f.id == id).unwrap().refused.clone();
        assert_eq!(
            refused("a-old").as_deref(),
            Some(
                "its ovn-controller 24.03.8 is older than the cluster's ovn-northd 26.03.0: re-run glidex-install on a-old to upgrade OVN"
            )
        );
        for ok in ["s1", "a-same", "a-newer"] {
            assert_eq!(refused(ok), None, "{ok}");
        }
        // A VM on no cluster network doesn't care.
        r.ovn_northd = None;
        assert!(
            evaluate(&r, &nodes, &cfg())
                .iter()
                .all(|f| f.refused.is_none())
        );
    }

    #[test]
    fn a_vm_runs_only_on_nodes_of_its_architecture() {
        let mut x86 = node("x86", 8, 16384);
        x86.status.features.arch = "x86_64".into();
        let mut arm = node("arm", 64, 131072);
        arm.status.features.arch = "aarch64".into();
        let old = node("old", 8, 16384); // an older node: no arch reported
        let nodes = vec![
            (x86, Load::default()),
            (arm, Load::default()),
            (old, Load::default()),
        ];
        let mut r = req(2, 1024);
        r.arch = "x86_64".into();
        let fits = evaluate(&r, &nodes, &cfg());
        let refused = |id: &str| fits.iter().find(|f| f.id == id).unwrap().refused.clone();
        assert_eq!(
            refused("arm").as_deref(),
            Some("it is aarch64; the VM's image is x86_64")
        );
        assert_eq!(refused("x86"), None);
        assert_eq!(refused("old"), None, "a node that reports no arch matches");
        // A VM without an image or firmware has no architecture.
        r.arch = String::new();
        assert!(
            evaluate(&r, &nodes, &cfg())
                .iter()
                .all(|f| f.refused.is_none())
        );
    }

    #[test]
    fn the_least_allocated_node_wins_and_ties_break_by_count_then_name() {
        let nodes = vec![
            (
                node("b", 8, 16384),
                Load {
                    vcpus: 4,
                    mem_mib: 4096,
                    vms: 2,
                    ..Default::default()
                },
            ),
            (
                node("a", 8, 16384),
                Load {
                    vcpus: 4,
                    mem_mib: 4096,
                    vms: 2,
                    ..Default::default()
                },
            ),
            (
                node("c", 8, 16384),
                Load {
                    vcpus: 8,
                    mem_mib: 8192,
                    vms: 3,
                    ..Default::default()
                },
            ),
        ];
        assert_eq!(
            schedule(&req(2, 1024), &nodes, &cfg()).unwrap().node,
            "a",
            "equal load, equal count: by name"
        );
        let nodes = vec![
            (
                node("a", 8, 16384),
                Load {
                    vcpus: 4,
                    mem_mib: 4096,
                    vms: 5,
                    ..Default::default()
                },
            ),
            (
                node("b", 8, 16384),
                Load {
                    vcpus: 4,
                    mem_mib: 4096,
                    vms: 2,
                    ..Default::default()
                },
            ),
        ];
        assert_eq!(
            schedule(&req(2, 1024), &nodes, &cfg()).unwrap().node,
            "b",
            "equal load: fewer VMs"
        );
    }

    #[test]
    fn the_preview_lists_every_node_the_choice_first_and_refusals_last() {
        let mut full = node("full", 1, 1024);
        full.status.allocatable.memory_mib = 100;
        let nodes = vec![
            (full, Load::default()),
            (
                node("busy", 8, 16384),
                Load {
                    vcpus: 8,
                    mem_mib: 8192,
                    vms: 4,
                    ..Default::default()
                },
            ),
            (node("idle", 8, 16384), Load::default()),
        ];
        let all = evaluate(&req(2, 1024), &nodes, &cfg());
        let names: Vec<&str> = all.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["idle", "busy", "full"]);
        assert!(all[0].refused.is_none() && all[1].refused.is_none());
        assert!(all[2].refused.as_deref().unwrap().contains("memory"));
        assert_eq!(
            (all[1].vcpus_taken, all[1].vcpus_capacity, all[1].vms),
            (8, 32, 4),
            "capacity is allocatable times the overcommit"
        );
        assert_eq!(
            schedule(&req(2, 1024), &nodes, &cfg()).unwrap().node,
            all[0].id,
            "the scheduler takes the preview's first"
        );
    }

    #[test]
    fn capacity_counts_stopped_vms_and_the_overcommit_factors() {
        let n = node("a", 2, 4096);
        // 2 CPUs × 4.0 = 8 vCPUs, memory not overcommitted.
        assert!(schedule(&req(8, 1024), &[(n.clone(), Load::default())], &cfg()).is_ok());
        assert!(schedule(&req(9, 1024), &[(n.clone(), Load::default())], &cfg()).is_err());
        let err = schedule(
            &req(1, 2048),
            &[(
                n.clone(),
                Load {
                    vcpus: 0,
                    mem_mib: 3000,
                    vms: 1,
                    ..Default::default()
                },
            )],
            &cfg(),
        )
        .unwrap_err();
        assert!(err[0].1.contains("memory"), "{err:?}");
    }

    #[test]
    fn each_filter_says_why() {
        let mut down = node("down", 8, 8192);
        down.status.ready = Tristate::Unknown;
        let mut drain = node("drain", 8, 8192);
        drain.spec.unschedulable = true;
        let mut noqemu = node("noqemu", 8, 8192);
        noqemu.status.features.hypervisors = vec!["cloudhypervisor".into()];
        let mut r = req(1, 512);
        r.hypervisor = "qemu".into();
        let nodes: Vec<_> = [down, drain, noqemu]
            .into_iter()
            .map(|n| (n, Load::default()))
            .collect();
        let err = schedule(&r, &nodes, &cfg()).unwrap_err();
        let why = |n: &str| err.iter().find(|(x, _)| x == n).unwrap().1.clone();
        assert_eq!(why("down"), "not ready");
        assert_eq!(why("drain"), "draining");
        assert!(why("noqemu").contains("qemu is not installed"));
        assert!(explain(&err).contains("down: not ready"));
    }

    #[test]
    fn pins_disks_networks_and_devices_decide() {
        let mut gpu = node("gpu", 8, 8192);
        gpu.status.pci_devices = vec!["0000:41:00.0".into()];
        let plain = node("plain", 8, 8192);
        let nodes = vec![
            (plain.clone(), Load::default()),
            (gpu.clone(), Load::default()),
        ];
        let mut r = req(1, 512);
        r.vfio = vec!["0000:41:00.0".into()];
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap().node, "gpu");
        let mut r = req(1, 512);
        r.disk_nodes = vec![Some("plain".into())];
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap().node, "plain");
        // An unbound disk on a datastore only some nodes reach.
        let mut r = req(1, 512);
        r.disk_reach = vec![("fast".into(), vec!["gpu".into()])];
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap().node, "gpu");
        r.disk_reach = vec![("fast".into(), Vec::new())];
        let err = schedule(&r, &nodes, &cfg()).unwrap_err();
        assert!(explain(&err).contains("datastore fast is not reachable here"));
        let mut r = req(1, 512);
        r.pin = Some("gpu".into());
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap().node, "gpu");
        r.net_nodes = vec!["plain".into()];
        assert!(
            schedule(&r, &nodes, &cfg()).is_err(),
            "a node network elsewhere and a pin conflict"
        );
        r = req(1, 512);
        r.disk_nodes = vec![Some("plain".into()), Some("gpu".into())];
        assert!(
            schedule(&r, &nodes, &cfg()).is_err(),
            "disks on two nodes fit nowhere"
        );
    }

    fn with_hugepages(id: &str, mem: u64, pages_2m: u64, pages_1g: u64) -> Node {
        let mut n = node(id, 8, mem);
        for (kib, pages) in [(2048u64, pages_2m), (1_048_576, pages_1g)] {
            if pages > 0 {
                n.status
                    .allocatable
                    .hugepages
                    .insert(kib.to_string(), pages);
            }
        }
        n.status.features.hugepage_kib = 2048;
        n
    }

    #[test]
    fn hugepages_are_counted_apart_from_memory_and_never_overcommitted() {
        // 1 GiB of ordinary memory, 4 GiB of 2M hugepages.
        let n = with_hugepages("a", 1024, 2048, 0);
        let mut r = req(1, 2048);
        r.hugepages = true;
        let mut placed = Vec::new();
        for _ in 0..2 {
            let c = schedule(&r, &[(n.clone(), loads_of(&placed))], &cfg()).unwrap();
            assert_eq!(c.page_kib, 2048);
            placed.push(Placed {
                node: "a".into(),
                vcpus: 1,
                mem_mib: 2048,
                page_kib: c.page_kib,
                name: "vm".into(),
                devices: Vec::new(),
                ..Default::default()
            });
        }
        // The pool is full; ordinary memory is untouched.
        let load = loads_of(&placed);
        assert_eq!(load.mem_mib, 0);
        let err = schedule(&r, &[(n.clone(), load.clone())], &cfg()).unwrap_err();
        assert_eq!(err[0].1, "not enough 2M hugepages (4096 of 4096 MiB taken)");
        // A VM in ordinary memory still fits.
        assert_eq!(
            schedule(&req(1, 1024), &[(n.clone(), load)], &cfg())
                .unwrap()
                .page_kib,
            SMALL_PAGE_KIB
        );
        // A size the node doesn't have, or memory that isn't whole pages.
        r.hugepage_kib = Some(1_048_576);
        let err = schedule(&r, &[(n.clone(), Load::default())], &cfg()).unwrap_err();
        assert_eq!(err[0].1, "no 1G hugepages");
        let g = with_hugepages("g", 1024, 0, 4);
        r.mem_mib = 1536;
        let err = schedule(&r, &[(g.clone(), Load::default())], &cfg()).unwrap_err();
        assert_eq!(err[0].1, "1536 MiB is not a whole number of 1G pages");
        r.mem_mib = 2048;
        let fit = &evaluate(&r, &[(g, Load::default())], &cfg())[0];
        assert_eq!(
            (
                fit.page_kib,
                fit.hugepages_mib_taken,
                fit.hugepages_mib_capacity
            ),
            (1_048_576, 0, 4096)
        );
    }

    #[test]
    fn vhost_user_vms_get_2m_pages_while_they_last_then_ordinary_memory() {
        // 2 GiB of 2M pages: a 1 GiB guest needs 1 GiB + 256 MiB spare.
        let n = with_hugepages("a", 8192, 1024, 0);
        let mut r = req(1, 1024);
        r.vhost_user = true;
        let first = schedule(&r, &[(n.clone(), Load::default())], &cfg()).unwrap();
        assert_eq!(first.page_kib, 2048);
        let load = loads_of(&[Placed {
            node: "a".into(),
            vcpus: 1,
            mem_mib: 1024,
            page_kib: 2048,
            name: "vm".into(),
            devices: Vec::new(),
            ..Default::default()
        }]);
        let second = schedule(&r, &[(n.clone(), load)], &cfg()).unwrap();
        assert_eq!(
            second.page_kib, SMALL_PAGE_KIB,
            "1 GiB left: no room to spare"
        );
        // Without hugepages on the node, ordinary memory.
        assert_eq!(
            schedule(&r, &[(node("b", 8, 8192), Load::default())], &cfg())
                .unwrap()
                .page_kib,
            SMALL_PAGE_KIB
        );
    }

    #[test]
    fn the_hugepage_share_counts_in_the_score() {
        let mut r = req(1, 1024);
        r.hugepages = true;
        let busy = loads_of(&[Placed {
            node: "a".into(),
            vcpus: 1,
            mem_mib: 3072,
            page_kib: 2048,
            name: "vm".into(),
            devices: Vec::new(),
            ..Default::default()
        }]);
        let nodes = vec![
            (with_hugepages("a", 8192, 2048, 0), busy),
            (with_hugepages("b", 8192, 2048, 0), Load::default()),
        ];
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap().node, "b");
    }

    fn loads_of(placed: &[Placed]) -> Load {
        loads(placed.iter().map(|p| Placed {
            node: p.node.clone(),
            vcpus: p.vcpus,
            mem_mib: p.mem_mib,
            page_kib: p.page_kib,
            name: p.name.clone(),
            devices: p.devices.clone(),
            ..Default::default()
        }))
        .remove("a")
        .unwrap_or_default()
    }

    /// Two NUMA nodes of four cores with two threads (node 0: CPUs 0-3 and
    /// siblings 8-11; node 1: 4-7 and 12-15), 8 GiB each; a GPU on each.
    fn numa_node() -> Node {
        let mut n = node("numa", 16, 16384);
        n.status.pci_devices = vec!["0000:81:00.0".into(), "0000:41:00.0".into()];
        n.status.topology = Some(TopologySummary {
            numa_nodes: (0..2u32)
                .map(|k| crate::topology::NumaNode {
                    id: k,
                    cores: (0..4).map(|i| vec![k * 4 + i, k * 4 + i + 8]).collect(),
                    memory_mib: 8192,
                    hugepages: Default::default(),
                })
                .collect(),
            device_numa: [
                ("0000:81:00.0".to_string(), 0),
                ("0000:41:00.0".to_string(), 1),
            ]
            .into(),
            ..Default::default()
        });
        n
    }

    fn numa_req(vcpus: u32, mem: u64, numa: NumaPolicy, cpu: CpuPolicy) -> Request {
        let mut r = req(vcpus, mem);
        r.numa = numa;
        r.cpu = cpu;
        r
    }

    fn place(c: &Choice, r: &Request) -> Placed {
        Placed {
            node: c.node.clone(),
            vcpus: r.vcpus as u64,
            mem_mib: r.mem_mib,
            page_kib: c.page_kib,
            numa_node: c.numa_node,
            cpus: c.cpus.clone(),
            numa: c.numa.clone(),
            ..Default::default()
        }
    }

    #[test]
    fn a_strict_vm_goes_to_its_devices_numa_node_and_counts_there() {
        let n = numa_node();
        let mut r = numa_req(2, 6144, NumaPolicy::Strict, CpuPolicy::Shared);
        r.vfio = vec!["/sys/bus/pci/devices/0000:41:00.0".into()];
        let c = schedule(&r, &[(n.clone(), Load::default())], &cfg()).unwrap();
        assert_eq!(
            (c.numa_node, c.cpus.is_empty(), c.numa_aligned),
            (Some(1), true, true)
        );
        // Its 6 GiB are node 1's now: another 4 GiB strict VM there doesn't fit.
        let load = loads([place(&c, &r)]).remove("numa").unwrap();
        let err = schedule(&r, &[(n.clone(), load.clone())], &cfg()).unwrap_err();
        assert_eq!(
            err[0].1,
            "NUMA node 1: not enough memory (6144 of 8192 MiB taken)"
        );
        // Without the device it takes node 0.
        let r2 = numa_req(2, 6144, NumaPolicy::Strict, CpuPolicy::Shared);
        assert_eq!(
            schedule(&r2, &[(n.clone(), load.clone())], &cfg())
                .unwrap()
                .numa_node,
            Some(0)
        );
        // Devices on two NUMA nodes can't be one node's.
        r.vfio.push("0000:81:00.0".into());
        let err = schedule(&r, &[(n, Load::default())], &cfg()).unwrap_err();
        assert_eq!(err[0].1, "its PCI devices are on NUMA nodes 0 and 1");
    }

    #[test]
    fn preferred_falls_back_to_the_whole_host_and_says_so() {
        let n = numa_node();
        let r = numa_req(2, 12288, NumaPolicy::Preferred, CpuPolicy::Shared);
        let c = schedule(&r, &[(n.clone(), Load::default())], &cfg()).unwrap();
        assert_eq!((c.numa_node, c.numa_aligned), (None, false));
        let strict = numa_req(2, 12288, NumaPolicy::Strict, CpuPolicy::Shared);
        assert!(schedule(&strict, &[(n, Load::default())], &cfg()).is_err());
    }

    #[test]
    fn dedicated_vcpus_take_whole_free_cores_and_leave_the_host_core() {
        let n = numa_node();
        let r = numa_req(3, 1024, NumaPolicy::None, CpuPolicy::Dedicated);
        let c = schedule(&r, &[(n.clone(), Load::default())], &cfg()).unwrap();
        // Implied `preferred`: on one node; node 0's first core (0, 8) is the
        // host's; two whole cores, vCPUs on 1, 9, 2 and 10 left idle.
        assert_eq!(c.numa_node, Some(0));
        assert_eq!(c.cpus, vec![1, 9, 2, 10]);
        let load = loads([place(&c, &r)]).remove("numa").unwrap();
        assert_eq!(load.dedicated.len(), 4);
        // Node 0 has one free core left: the next one goes to node 1.
        let c2 = schedule(&r, &[(n.clone(), load.clone())], &cfg()).unwrap();
        assert_eq!(
            (c2.numa_node, c2.cpus.clone()),
            (Some(1), vec![5, 13, 6, 14])
        );
        // Shared VMs run on what is neither reserved nor dedicated.
        let pool = shared_pool(&n, &load.dedicated);
        for c in [0, 8, 4, 12, 1, 9, 2, 10] {
            assert!(!pool.contains(&c), "{c} in {pool:?}");
        }
        assert!(pool.contains(&3) && pool.contains(&15));
        // Single threads when whole cores are off.
        let mut threads = cfg();
        threads.dedicated_whole_cores = false;
        let c = schedule(&r, &[(n, Load::default())], &threads).unwrap();
        assert_eq!(c.cpus, vec![1, 9, 2]);
    }

    #[test]
    fn dedicated_cpus_shrink_what_shared_vcpus_may_take() {
        let n = numa_node();
        // 16 CPUs × 4.0 = 64 shared vCPUs; 60 are placed.
        let shared = Placed {
            node: "numa".into(),
            vcpus: 60,
            mem_mib: 1024,
            page_kib: SMALL_PAGE_KIB,
            ..Default::default()
        };
        let load = loads([shared]).remove("numa").unwrap();
        let r = numa_req(2, 1024, NumaPolicy::None, CpuPolicy::Dedicated);
        let err = schedule(&r, &[(n.clone(), load)], &cfg()).unwrap_err();
        assert_eq!(
            err[0].1,
            "2 dedicated CPUs would leave too few for the shared vCPUs (60 of 56)"
        );
        // A node that reports no topology takes no policy.
        let plain = node("plain", 16, 16384);
        let err = schedule(&r, &[(plain, Load::default())], &cfg()).unwrap_err();
        assert_eq!(err[0].1, "it reports no topology");
    }

    #[test]
    fn shared_vcpus_under_a_numa_policy_bind_memory_only() {
        let n = numa_node();
        let r = numa_req(8, 2048, NumaPolicy::Strict, CpuPolicy::Shared);
        let c = schedule(&r, &[(n, Load::default())], &cfg()).unwrap();
        assert_eq!(c.numa_node, Some(0));
        assert!(c.cpus.is_empty(), "no CPUs held (H15)");
    }

    #[test]
    fn devices_are_claimed_per_node_and_by_iommu_group() {
        let gpu = |id: &str| {
            let mut n = node(id, 8, 8192);
            n.status.pci_devices = vec!["0000:41:00.0".into(), "0000:41:00.1".into()];
            n.status.pci_groups = [("0000:41:00.0", "31"), ("0000:41:00.1", "31")]
                .into_iter()
                .map(|(a, g)| (a.to_string(), g.to_string()))
                .collect();
            n
        };
        let path = "/sys/bus/pci/devices/0000:41:00.0";
        let mut r = req(1, 512);
        r.vfio = vec![path.into()];

        // The VM names a sysfs path; the node reports addresses.
        let free = vec![(gpu("a"), Load::default())];
        assert_eq!(schedule(&r, &free, &cfg()).unwrap().node, "a");

        // Two nodes with a device at the same address hold one VM each.
        let held = loads([Placed {
            node: "a".into(),
            vcpus: 1,
            mem_mib: 512,
            name: "first".into(),
            devices: vec![path.into()],
            page_kib: SMALL_PAGE_KIB,
            ..Default::default()
        }]);
        let nodes = vec![
            (gpu("a"), held.get("a").cloned().unwrap()),
            (gpu("b"), Load::default()),
        ];
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap().node, "b");
        let fits = evaluate(&r, &nodes, &cfg());
        assert_eq!(
            fits.iter()
                .find(|f| f.id == "a")
                .unwrap()
                .refused
                .as_deref(),
            Some("PCI device 0000:41:00.0 is used by VM first")
        );

        // Another function of the same IOMMU group is taken too.
        r.vfio = vec!["0000:41:00.1".into()];
        r.pin = Some("a".into());
        let err = schedule(&r, &nodes, &cfg()).unwrap_err();
        assert!(
            err.iter().any(|(n, why)| n == "a"
                && why
                    == "PCI device 0000:41:00.1 shares IOMMU group 31 with 0000:41:00.0, used by VM first"),
            "{err:?}"
        );
        // A node that reports no groups: claims by address only.
        let mut old = gpu("a");
        old.status.pci_groups.clear();
        let nodes = vec![(old, held.get("a").cloned().unwrap())];
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap().node, "a");
    }

    #[test]
    fn a_vm_goes_where_its_devices_are_granted() {
        let gpu = |id: &str| {
            let mut n = node(id, 8, 8192);
            n.status.pci_devices = vec!["0000:41:00.0".into()];
            n
        };
        let nodes = vec![(gpu("a"), Load::default()), (gpu("b"), Load::default())];
        let mut r = req(1, 512);
        r.vfio = vec!["0000:41:00.0".into()];
        r.device_nodes = Some(vec!["b".into()]);
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap().node, "b");
        r.device_nodes = Some(vec![]);
        let err = schedule(&r, &nodes, &cfg()).unwrap_err();
        assert!(
            err.iter().all(
                |(_, why)| why == "its PCI devices are not granted to the project on this node"
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_cordoned_device_is_refused() {
        let mut n = node("a", 8, 8192);
        n.status.pci_devices = vec!["0000:41:00.0".into()];
        n.spec.cordoned_devices = ["0000:41:00.0".to_string()].into();
        let mut r = req(1, 512);
        r.vfio = vec!["/sys/bus/pci/devices/0000:41:00.0".into()];
        let err = schedule(&r, &[(n, Load::default())], &cfg()).unwrap_err();
        assert_eq!(err[0].1, "PCI device 0000:41:00.0 is cordoned");
    }

    /// A node with four GPUs: two under switch s1 on NUMA node 0, one under
    /// s2 on node 1, one under s3 on node 1; and a NIC.
    fn pool_node() -> Node {
        let mut n = numa_node();
        let gpu = |a: &str, numa: u32, sw: &str| crate::topology::SummaryDevice {
            address: a.into(),
            vendor_id: "0x10de".into(),
            device_id: "0x20b0".into(),
            class_id: "0x030200".into(),
            numa_node: Some(numa),
            switch: Some(sw.into()),
            ready: true,
            ..Default::default()
        };
        let t = n.status.topology.as_mut().unwrap();
        t.devices = vec![
            gpu("0000:07:00.0", 0, "s1"),
            gpu("0000:08:00.0", 0, "s1"),
            gpu("0000:47:00.0", 1, "s2"),
            gpu("0000:4e:00.0", 1, "s3"),
            crate::topology::SummaryDevice {
                address: "0000:41:00.0".into(),
                vendor_id: "0x15b3".into(),
                device_id: "0x101b".into(),
                class_id: "0x020700".into(),
                numa_node: Some(1),
                switch: Some("s2".into()),
                ready: true,
                ..Default::default()
            },
        ];
        for d in t.devices.clone() {
            if let Some(k) = d.numa_node {
                t.device_numa.insert(d.address.clone(), k);
            }
        }
        n
    }

    fn gpus(count: u32, same_switch: bool) -> PoolRequest {
        PoolRequest {
            pool: crate::config::PciPool {
                name: "a100".into(),
                class: Some("gpu".into()),
                matches: crate::config::PoolMatch {
                    vendor_id: Some("0x10de".into()),
                    device_id: Some("0x20b0".into()),
                    ..Default::default()
                },
                nodes: vec![],
                prepare: false,
            },
            count,
            same_switch,
        }
    }

    fn picked(c: &Choice) -> Vec<&str> {
        c.pool_devices.iter().map(|d| d.address.as_str()).collect()
    }

    #[test]
    fn pools_give_devices_near_each_other() {
        let n = pool_node();
        // Two GPUs, lowest addresses: both on NUMA node 0.
        let mut r = req(2, 1024);
        r.pools = vec![gpus(2, false)];
        let c = schedule(&r, &[(n.clone(), Load::default())], &cfg()).unwrap();
        assert_eq!(picked(&c), ["0000:07:00.0", "0000:08:00.0"]);
        // Next to the NIC the VM names: the GPUs on its NUMA node first.
        r.vfio = vec!["0000:41:00.0".into()];
        r.pools = vec![gpus(1, false)];
        let c = schedule(&r, &[(n.clone(), Load::default())], &cfg()).unwrap();
        assert_eq!(picked(&c), ["0000:47:00.0"]);
        // One switch: s2 has the NIC and one GPU, so two GPUs need s1.
        r.pools = vec![gpus(2, true)];
        let c = schedule(&r, &[(n.clone(), Load::default())], &cfg()).unwrap();
        assert_eq!(picked(&c), ["0000:07:00.0", "0000:08:00.0"]);
        // Held and cordoned devices are no one else's.
        let held = loads([Placed {
            node: "numa".into(),
            name: "other".into(),
            devices: vec!["0000:07:00.0".into()],
            page_kib: SMALL_PAGE_KIB,
            ..Default::default()
        }])
        .remove("numa")
        .unwrap();
        let mut cordoned = n.clone();
        cordoned.spec.cordoned_devices = ["0000:08:00.0".to_string()].into();
        let mut r = req(2, 1024);
        r.pools = vec![gpus(3, false)];
        let err = schedule(&r, &[(cordoned.clone(), held.clone())], &cfg()).unwrap_err();
        assert_eq!(err[0].1, "pool a100: 2 of 3 free");
        r.pools = vec![gpus(2, true)];
        let err = schedule(&r, &[(cordoned, held)], &cfg()).unwrap_err();
        assert_eq!(
            err[0].1,
            "pool a100: no 2 free devices under one PCIe switch"
        );
        // A pool limited to other nodes.
        let mut p = gpus(1, false);
        p.pool.nodes = vec!["elsewhere".into()];
        r.pools = vec![p];
        let err = schedule(&r, &[(n, Load::default())], &cfg()).unwrap_err();
        assert_eq!(err[0].1, "pool a100 is not on this node");
    }

    #[test]
    fn a_strict_vm_gets_its_pool_devices_and_memory_on_one_numa_node() {
        let n = pool_node();
        let mut r = numa_req(2, 1024, NumaPolicy::Strict, CpuPolicy::Shared);
        r.vfio = vec!["0000:41:00.0".into()];
        r.pools = vec![gpus(1, false)];
        let c = schedule(&r, &[(n, Load::default())], &cfg()).unwrap();
        assert_eq!((c.numa_node, picked(&c)), (Some(1), vec!["0000:47:00.0"]));
    }

    #[test]
    fn split_by_keeps_the_total_and_follows_the_weights() {
        assert_eq!(split_by(8, &[2, 2]), [4, 4]);
        assert_eq!(split_by(7, &[2, 2]), [4, 3], "the remainder to the first");
        assert_eq!(split_by(10, &[1, 3]), [3, 7]);
        assert_eq!(split_by(1000, &[1, 1, 1]), [334, 333, 333]);
        assert_eq!(split_by(5, &[3]), [5]);
    }

    #[test]
    fn aligned_gpus_take_as_few_numa_nodes_as_they_can() {
        // Two GPUs fit one node: an ordinary NUMA fit there.
        let n = pool_node();
        let mut r = numa_req(4, 2048, NumaPolicy::Aligned, CpuPolicy::Shared);
        r.pools = vec![gpus(2, false)];
        let c = schedule(&r, &[(n, Load::default())], &cfg()).unwrap();
        assert_eq!((c.numa_node, c.numa.len()), (Some(0), 0), "{c:?}");
        assert_eq!(picked(&c), vec!["0000:07:00.0", "0000:08:00.0"]);
    }

    #[test]
    fn aligned_over_two_numa_nodes_splits_cpus_memory_and_devices() {
        let n = pool_node();
        let mut r = numa_req(4, 4096, NumaPolicy::Aligned, CpuPolicy::Dedicated);
        r.pools = vec![gpus(4, false)];
        let c = schedule(&r, &[(n.clone(), Load::default())], &cfg()).unwrap();
        assert_eq!(c.numa_node, None);
        let shares: Vec<(u32, u32, u64, Vec<String>)> = c
            .numa
            .iter()
            .map(|s| (s.node, s.vcpus, s.memory_mib, s.devices.clone()))
            .collect();
        assert_eq!(
            shares,
            vec![
                (
                    0,
                    2,
                    2048,
                    vec!["0000:07:00.0".into(), "0000:08:00.0".into()]
                ),
                (
                    1,
                    2,
                    2048,
                    vec!["0000:47:00.0".into(), "0000:4e:00.0".into()]
                ),
            ]
        );
        // vCPUs 0-1 on node 0's CPUs, 2-3 on node 1's.
        let t = n.status.topology.as_ref().unwrap();
        let on = |k: u32, c: u32| t.node(k).unwrap().cpus().any(|x| x == c);
        assert!(on(0, c.cpus[0]) && on(0, c.cpus[1]), "{:?}", c.cpus);
        assert!(on(1, c.cpus[2]) && on(1, c.cpus[3]), "{:?}", c.cpus);
        // Its memory counts on each node: a second such VM finds node 0
        // with only what the first left.
        let load = loads([place(&c, &r)]).remove("numa").unwrap();
        assert_eq!(load.numa_mem_mib.get(&0), Some(&2048));
        assert_eq!(load.numa_mem_mib.get(&1), Some(&2048));
        // A node whose share doesn't fit: refused, naming that node.
        let busy = Load {
            mem_mib: 7000,
            numa_mem_mib: [(0, 7000)].into(),
            ..Default::default()
        };
        let mut r2 = numa_req(4, 4096, NumaPolicy::Aligned, CpuPolicy::Shared);
        r2.pools = vec![gpus(4, false)];
        let e = schedule(&r2, &[(n.clone(), busy)], &cfg()).unwrap_err();
        assert!(e[0].1.contains("NUMA node 0"), "{e:?}");
        // Fewer vCPUs than NUMA nodes can't be split.
        let mut tiny = numa_req(1, 1024, NumaPolicy::Aligned, CpuPolicy::Shared);
        tiny.pools = vec![gpus(4, false)];
        let e = schedule(&tiny, &[(n, Load::default())], &cfg()).unwrap_err();
        assert!(e[0].1.contains("at least 2 vCPUs"), "{e:?}");
    }

    #[test]
    fn aligned_without_devices_is_preferred() {
        let n = numa_node();
        let r = numa_req(2, 1024, NumaPolicy::Aligned, CpuPolicy::Shared);
        let c = schedule(&r, &[(n, Load::default())], &cfg()).unwrap();
        assert!(c.numa_node.is_some() && c.numa.is_empty(), "{c:?}");
    }

    #[test]
    fn provider_networks_need_a_node_that_reaches_the_physnet() {
        let mut lan = node("lan", 8, 8192);
        lan.status
            .features
            .physnets
            .insert("lan".into(), "gxbr-lan".into());
        let plain = node("plain", 8, 8192);
        let nodes = vec![(plain, Load::default()), (lan, Load::default())];
        let mut r = req(1, 512);
        r.physnets = vec!["lan".into()];
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap().node, "lan");
        r.physnets = vec!["dmz".into()];
        let err = schedule(&r, &nodes, &cfg()).unwrap_err();
        assert!(
            err.iter()
                .all(|(_, why)| why.contains("physical network dmz")),
            "{err:?}"
        );
        // vhost-user on a cluster network wants the userspace datapath.
        let mut r = req(1, 512);
        r.vhost_user_on_cluster = true;
        assert!(schedule(&r, &nodes, &cfg()).is_err());
    }

    #[test]
    fn two_nodes_fill_by_capacity() {
        let mut nodes = vec![
            (node("a", 4, 8192), Load::default()),
            (node("b", 4, 8192), Load::default()),
        ];
        let mut placed = Vec::new();
        for _ in 0..10 {
            let id = schedule(&req(2, 1024), &nodes, &cfg()).unwrap().node;
            placed.push(id.clone());
            let l = &mut nodes.iter_mut().find(|(n, _)| n.meta.id == id).unwrap().1;
            l.vcpus += 2;
            l.mem_mib += 1024;
            l.vms += 1;
        }
        assert_eq!(placed.iter().filter(|p| *p == "a").count(), 5);
        assert_eq!(placed.iter().filter(|p| *p == "b").count(), 5);
        // 4 CPUs × 4.0 = 16 vCPUs per node: 8 VMs of 2 each fill a node; memory 8192/1024 = 8.
        let full = vec![(
            node("a", 4, 8192),
            Load {
                vcpus: 16,
                mem_mib: 1024,
                vms: 8,
                ..Default::default()
            },
        )];
        assert!(schedule(&req(2, 1024), &full, &cfg()).is_err());
    }
}
