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
    /// A NIC on a cluster network is vhost-user: `br-int` must be userspace.
    pub vhost_user_on_cluster: bool,
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
    if !req.hypervisor.is_empty() && !n.status.features.hypervisors.iter().any(|h| *h == req.hypervisor) {
        return Some(format!("{} is not installed", req.hypervisor));
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

/// The node a VM goes to, or why each node refused it. Least allocated by
/// `max(cpu share, memory share)`, then fewest VMs, then name, so the answer
/// is deterministic.
pub fn schedule(req: &Request, nodes: &[(Node, Load)], cfg: &SchedulerConfig) -> Result<String, Vec<(String, String)>> {
    let mut fits: Vec<(f64, u64, &str, &str)> = Vec::new();
    let mut refused = Vec::new();
    for (n, load) in nodes {
        match failing(n, *load, req, cfg) {
            None => {
                let cpu = (load.vcpus + req.vcpus as u64) as f64 / (n.status.allocatable.cpus.max(1) as f64);
                let mem = (load.mem_mib + req.mem_mib) as f64 / (n.status.allocatable.memory_mib.max(1) as f64);
                fits.push((cpu.max(mem), load.vms, n.spec.name.as_str(), n.meta.id.as_str()));
            }
            Some(why) => refused.push((n.spec.name.clone(), why)),
        }
    }
    fits.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
    match fits.first() {
        Some(f) => Ok(f.3.to_string()),
        None => Err(refused),
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
        let mut n = Node::new(id, NodeSpec { name: id.into(), role: NodeRole::Agent, unschedulable: false, labels: Default::default() });
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
