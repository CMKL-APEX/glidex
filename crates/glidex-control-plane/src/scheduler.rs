//! The scheduler (spec/clustering.md §9.1): where a VM runs. It decides once;
//! after that the VM stays (D6).

use crate::cluster::config::SchedulerConfig;
use crate::node::{Node, NodePhase};
use crate::models::Tristate;
use std::collections::HashMap;

/// What a VM asks of a node.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub vcpus: u32,
    pub mem_mib: u64,
    /// `cloudhypervisor` or `qemu`, as nodes report their hypervisors.
    pub hypervisor: String,
    pub hugepages: bool,
    /// `spec.node`: an id or a name.
    pub pin: Option<String>,
    /// The node of each disk the VM uses, `None` for one not bound yet.
    pub disk_nodes: Vec<Option<String>>,
    /// The node of each `scope: node` network it attaches to.
    pub net_nodes: Vec<String>,
    /// PCI addresses of VFIO devices.
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
#[derive(Debug, Clone, Copy, Default)]
pub struct Load {
    pub vcpus: u64,
    pub mem_mib: u64,
    pub vms: u64,
}

fn failing(n: &Node, load: Load, req: &Request, cfg: &SchedulerConfig) -> Option<String> {
    if let Some(pin) = &req.pin {
        if &n.meta.id != pin && &n.spec.name != pin {
            return Some(format!("pinned to {pin}"));
        }
    }
    if n.status.phase != NodePhase::Active || n.spec.unschedulable {
        return Some(if n.spec.unschedulable { "draining" } else { "not active" }.into());
    }
    if n.status.ready != Tristate::True {
        return Some("not ready".into());
    }
    let (cpu_cap, mem_cap) = (n.status.allocatable.cpus as f64 * cfg.cpu_overcommit, n.status.allocatable.memory_mib as f64 * cfg.memory_overcommit);
    if (load.vcpus + req.vcpus as u64) as f64 > cpu_cap {
        return Some(format!("not enough CPU ({} of {:.0} vCPUs taken)", load.vcpus, cpu_cap));
    }
    if (load.mem_mib + req.mem_mib) as f64 > mem_cap {
        return Some(format!("not enough memory ({} of {:.0} MiB taken)", load.mem_mib, mem_cap));
    }
    if req.hugepages && n.status.allocatable.hugepages.is_empty() {
        return Some("no hugepages".into());
    }
    if !req.hypervisor.is_empty() && !n.status.features.hypervisors.contains(&req.hypervisor) {
        return Some(format!("{} is not installed", req.hypervisor));
    }
    if !n.status.features.runs_arch(&req.arch) {
        return Some(format!("it is {}; the VM's image is {}", n.status.features.arch, req.arch));
    }
    for p in &req.physnets {
        if !n.status.features.physnets.contains_key(p) {
            return Some(format!("it does not reach physical network {p}"));
        }
    }
    if let Some(why) = crate::node::ovn_outdated(n, req.ovn_northd.as_deref()) {
        return Some(why);
    }
    if req.vhost_user_on_cluster && n.status.features.br_int_datapath.as_deref() != Some("netdev") {
        return Some("br-int is not on the userspace (netdev) datapath".into());
    }
    for d in &req.vfio {
        if !n.status.pci_devices.iter().any(|p| p.eq_ignore_ascii_case(d)) {
            return Some(format!("no PCI device {d}"));
        }
    }
    for dn in req.disk_nodes.iter().flatten() {
        if dn != &n.meta.id {
            return Some("a disk is bound to another node".into());
        }
    }
    for nn in &req.net_nodes {
        if nn != &n.meta.id {
            return Some("a network exists on another node".into());
        }
    }
    None
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
    /// `max(cpu share, memory share)` after placing: lower is preferred.
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
            let cpu = (load.vcpus + req.vcpus as u64) as f64 / (n.status.allocatable.cpus.max(1) as f64);
            let mem = (load.mem_mib + req.mem_mib) as f64 / (n.status.allocatable.memory_mib.max(1) as f64);
            NodeFit {
                id: n.meta.id.clone(),
                name: n.spec.name.clone(),
                refused: failing(n, *load, req, cfg),
                vcpus_taken: load.vcpus,
                vcpus_capacity: (n.status.allocatable.cpus as f64 * cfg.cpu_overcommit) as u64,
                mem_mib_taken: load.mem_mib,
                mem_mib_capacity: (n.status.allocatable.memory_mib as f64 * cfg.memory_overcommit) as u64,
                vms: load.vms,
                score: cpu.max(mem),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.refused
            .is_some()
            .cmp(&b.refused.is_some())
            .then(if a.refused.is_none() { a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal) } else { std::cmp::Ordering::Equal })
            .then(a.vms.cmp(&b.vms))
            .then(a.name.cmp(&b.name))
    });
    out
}

/// The node a VM goes to, or why each node refused it (the first of
/// [`evaluate`], so the answer is deterministic).
pub fn schedule(req: &Request, nodes: &[(Node, Load)], cfg: &SchedulerConfig) -> Result<String, Vec<(String, String)>> {
    let all = evaluate(req, nodes, cfg);
    match all.first() {
        Some(f) if f.refused.is_none() => Ok(f.id.clone()),
        _ => {
            let mut refused: Vec<(String, String)> = all.into_iter().filter_map(|f| f.refused.map(|r| (f.name, r))).collect();
            refused.sort_by(|a, b| a.0.cmp(&b.0));
            Err(refused)
        }
    }
}

/// `Unschedulable`'s message: each node's reason, capped.
pub fn explain(refused: &[(String, String)]) -> String {
    let mut parts: Vec<String> = refused.iter().take(8).map(|(n, w)| format!("{n}: {w}")).collect();
    if refused.len() > 8 {
        parts.push(format!("and {} more", refused.len() - 8));
    }
    if parts.is_empty() {
        "there are no nodes".into()
    } else {
        parts.join("; ")
    }
}

/// Load per node from `(node id, vcpus, mem MiB)` of every placed VM.
pub fn loads(placed: impl IntoIterator<Item = (String, u64, u64)>) -> HashMap<String, Load> {
    let mut m: HashMap<String, Load> = HashMap::new();
    for (node, cpu, mem) in placed {
        let l = m.entry(node).or_default();
        l.vcpus += cpu;
        l.mem_mib += mem;
        l.vms += 1;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{NodeRole, NodeSpec, Resources};

    fn node(id: &str, cpus: u32, mem: u64) -> Node {
        let mut n = Node::new(id, NodeSpec { name: id.into(), role: NodeRole::Agent, unschedulable: false, labels: Default::default(), br_int_datapath: Default::default(), bridge_mappings: Default::default(), drop_spare_files_before: None });
        n.status.allocatable = Resources { cpus, memory_mib: mem, hugepages: Default::default() };
        n.status.features.hypervisors = vec!["cloudhypervisor".into(), "qemu".into()];
        n
    }

    fn req(cpus: u32, mem: u64) -> Request {
        Request { vcpus: cpus, mem_mib: mem, hypervisor: "cloudhypervisor".into(), ..Default::default() }
    }

    fn cfg() -> SchedulerConfig {
        SchedulerConfig { cpu_overcommit: 4.0, memory_overcommit: 1.0 }
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
        let northd = crate::node::cluster_northd_ovn(&[server.clone(), old.clone(), same.clone(), newer.clone()]);
        assert_eq!(northd.as_deref(), Some("26.03.0"));
        assert_eq!(crate::node::cluster_northd_ovn(std::slice::from_ref(&old)), None);

        let nodes = vec![(server, Load::default()), (old, Load::default()), (same, Load::default()), (newer, Load::default())];
        let mut r = req(2, 1024);
        r.ovn_northd = northd;
        let fits = evaluate(&r, &nodes, &cfg());
        let refused = |id: &str| fits.iter().find(|f| f.id == id).unwrap().refused.clone();
        assert_eq!(refused("a-old").as_deref(), Some("its ovn-controller 24.03.8 is older than the cluster's ovn-northd 26.03.0: re-run glidex-install on a-old to upgrade OVN"));
        for ok in ["s1", "a-same", "a-newer"] {
            assert_eq!(refused(ok), None, "{ok}");
        }
        // A VM on no cluster network doesn't care.
        r.ovn_northd = None;
        assert!(evaluate(&r, &nodes, &cfg()).iter().all(|f| f.refused.is_none()));
    }

    #[test]
    fn a_vm_runs_only_on_nodes_of_its_architecture() {
        let mut x86 = node("x86", 8, 16384);
        x86.status.features.arch = "x86_64".into();
        let mut arm = node("arm", 64, 131072);
        arm.status.features.arch = "aarch64".into();
        let old = node("old", 8, 16384); // an older node: no arch reported
        let nodes = vec![(x86, Load::default()), (arm, Load::default()), (old, Load::default())];
        let mut r = req(2, 1024);
        r.arch = "x86_64".into();
        let fits = evaluate(&r, &nodes, &cfg());
        let refused = |id: &str| fits.iter().find(|f| f.id == id).unwrap().refused.clone();
        assert_eq!(refused("arm").as_deref(), Some("it is aarch64; the VM's image is x86_64"));
        assert_eq!(refused("x86"), None);
        assert_eq!(refused("old"), None, "a node that reports no arch matches");
        // A VM without an image or firmware has no architecture.
        r.arch = String::new();
        assert!(evaluate(&r, &nodes, &cfg()).iter().all(|f| f.refused.is_none()));
    }

    #[test]
    fn the_least_allocated_node_wins_and_ties_break_by_count_then_name() {
        let nodes = vec![
            (node("b", 8, 16384), Load { vcpus: 4, mem_mib: 4096, vms: 2 }),
            (node("a", 8, 16384), Load { vcpus: 4, mem_mib: 4096, vms: 2 }),
            (node("c", 8, 16384), Load { vcpus: 8, mem_mib: 8192, vms: 3 }),
        ];
        assert_eq!(schedule(&req(2, 1024), &nodes, &cfg()).unwrap(), "a", "equal load, equal count: by name");
        let nodes = vec![(node("a", 8, 16384), Load { vcpus: 4, mem_mib: 4096, vms: 5 }), (node("b", 8, 16384), Load { vcpus: 4, mem_mib: 4096, vms: 2 })];
        assert_eq!(schedule(&req(2, 1024), &nodes, &cfg()).unwrap(), "b", "equal load: fewer VMs");
    }

    #[test]
    fn the_preview_lists_every_node_the_choice_first_and_refusals_last() {
        let mut full = node("full", 1, 1024);
        full.status.allocatable.memory_mib = 100;
        let nodes = vec![
            (full, Load::default()),
            (node("busy", 8, 16384), Load { vcpus: 8, mem_mib: 8192, vms: 4 }),
            (node("idle", 8, 16384), Load::default()),
        ];
        let all = evaluate(&req(2, 1024), &nodes, &cfg());
        let names: Vec<&str> = all.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["idle", "busy", "full"]);
        assert!(all[0].refused.is_none() && all[1].refused.is_none());
        assert!(all[2].refused.as_deref().unwrap().contains("memory"));
        assert_eq!((all[1].vcpus_taken, all[1].vcpus_capacity, all[1].vms), (8, 32, 4), "capacity is allocatable times the overcommit");
        assert_eq!(schedule(&req(2, 1024), &nodes, &cfg()).unwrap(), all[0].id, "the scheduler takes the preview's first");
    }

    #[test]
    fn capacity_counts_stopped_vms_and_the_overcommit_factors() {
        let n = node("a", 2, 4096);
        // 2 CPUs × 4.0 = 8 vCPUs, memory not overcommitted.
        assert!(schedule(&req(8, 1024), &[(n.clone(), Load::default())], &cfg()).is_ok());
        assert!(schedule(&req(9, 1024), &[(n.clone(), Load::default())], &cfg()).is_err());
        let err = schedule(&req(1, 2048), &[(n.clone(), Load { vcpus: 0, mem_mib: 3000, vms: 1 })], &cfg()).unwrap_err();
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
        let nodes: Vec<_> = [down, drain, noqemu].into_iter().map(|n| (n, Load::default())).collect();
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
        let nodes = vec![(plain.clone(), Load::default()), (gpu.clone(), Load::default())];
        let mut r = req(1, 512);
        r.vfio = vec!["0000:41:00.0".into()];
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap(), "gpu");
        let mut r = req(1, 512);
        r.disk_nodes = vec![Some("plain".into())];
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap(), "plain");
        let mut r = req(1, 512);
        r.pin = Some("gpu".into());
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap(), "gpu");
        r.net_nodes = vec!["plain".into()];
        assert!(schedule(&r, &nodes, &cfg()).is_err(), "a node network elsewhere and a pin conflict");
        r = req(1, 512);
        r.disk_nodes = vec![Some("plain".into()), Some("gpu".into())];
        assert!(schedule(&r, &nodes, &cfg()).is_err(), "disks on two nodes fit nowhere");
    }

    #[test]
    fn provider_networks_need_a_node_that_reaches_the_physnet() {
        let mut lan = node("lan", 8, 8192);
        lan.status.features.physnets.insert("lan".into(), "gxbr-lan".into());
        let plain = node("plain", 8, 8192);
        let nodes = vec![(plain, Load::default()), (lan, Load::default())];
        let mut r = req(1, 512);
        r.physnets = vec!["lan".into()];
        assert_eq!(schedule(&r, &nodes, &cfg()).unwrap(), "lan");
        r.physnets = vec!["dmz".into()];
        let err = schedule(&r, &nodes, &cfg()).unwrap_err();
        assert!(err.iter().all(|(_, why)| why.contains("physical network dmz")), "{err:?}");
        // vhost-user on a cluster network wants the userspace datapath.
        let mut r = req(1, 512);
        r.vhost_user_on_cluster = true;
        assert!(schedule(&r, &nodes, &cfg()).is_err());
    }

    #[test]
    fn two_nodes_fill_by_capacity() {
        let mut nodes = vec![(node("a", 4, 8192), Load::default()), (node("b", 4, 8192), Load::default())];
        let mut placed = Vec::new();
        for _ in 0..10 {
            let id = schedule(&req(2, 1024), &nodes, &cfg()).unwrap();
            placed.push(id.clone());
            let l = &mut nodes.iter_mut().find(|(n, _)| n.meta.id == id).unwrap().1;
            l.vcpus += 2;
            l.mem_mib += 1024;
            l.vms += 1;
        }
        assert_eq!(placed.iter().filter(|p| *p == "a").count(), 5);
        assert_eq!(placed.iter().filter(|p| *p == "b").count(), 5);
        // 4 CPUs × 4.0 = 16 vCPUs per node: 8 VMs of 2 each fill a node; memory 8192/1024 = 8.
        let full = vec![(node("a", 4, 8192), Load { vcpus: 16, mem_mib: 1024, vms: 8 })];
        assert!(schedule(&req(2, 1024), &full, &cfg()).is_err());
    }
}
