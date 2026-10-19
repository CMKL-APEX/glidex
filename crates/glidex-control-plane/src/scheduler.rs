//! The scheduler (spec/clustering.md §9.1): where a VM runs. It decides once;
//! after that the VM stays (D6).

use crate::cluster::config::SchedulerConfig;
use crate::models::{SMALL_PAGE_KIB, Tristate, hugepage_size_name};
use crate::node::{Node, NodePhase};
use std::collections::{BTreeMap, HashMap};

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
    /// `spec.node`: an id or a name.
    pub pin: Option<String>,
    /// The node of each disk the VM uses, `None` for one not bound yet.
    pub disk_nodes: Vec<Option<String>>,
    /// The node of each `scope: node` network it attaches to.
    pub net_nodes: Vec<String>,
    /// VFIO devices, as the VM names them (sysfs paths or addresses).
    pub vfio: Vec<String>,
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
}

impl Load {
    fn hugepages_taken(&self, page_kib: u64) -> u64 {
        self.hugepage_mib.get(&page_kib).copied().unwrap_or(0)
    }
}

/// One VM placed on a node, for [`loads`].
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

/// The page size `req` gets on `n`, or why it can't go there.
fn failing(n: &Node, load: &Load, req: &Request, cfg: &SchedulerConfig) -> Result<u64, String> {
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
    let (cpu_cap, mem_cap) = (
        n.status.allocatable.cpus as f64 * cfg.cpu_overcommit,
        n.status.allocatable.memory_mib as f64 * cfg.memory_overcommit,
    );
    if (load.vcpus + req.vcpus as u64) as f64 > cpu_cap {
        return Err(format!(
            "not enough CPU ({} of {:.0} vCPUs taken)",
            load.vcpus, cpu_cap
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
        let addr = pci_address(d);
        if !n
            .status
            .pci_devices
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&addr))
        {
            return Err(format!("no PCI device {addr}"));
        }
        if let Some(why) = device_taken(&n.status.pci_groups, &load.devices, d) {
            return Err(why);
        }
    }
    for dn in req.disk_nodes.iter().flatten() {
        if dn != &n.meta.id {
            return Err("a disk is bound to another node".into());
        }
    }
    for nn in &req.net_nodes {
        if nn != &n.meta.id {
            return Err("a network exists on another node".into());
        }
    }
    Ok(page)
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
            let (refused, page) = match failing(n, load, req, cfg) {
                Ok(p) => (None, p),
                Err(why) => (Some(why), page_kib(n, load, req).unwrap_or(SMALL_PAGE_KIB)),
            };
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
        }))
        .remove("a")
        .unwrap_or_default()
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
