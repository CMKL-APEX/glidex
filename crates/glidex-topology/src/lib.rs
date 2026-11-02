//! A host's topology from sysfs, with no host tools (spec/22-host-topology.md
//! H1): NUMA nodes and their sockets, the PCIe tree with each function's
//! root port and switch, and network ports with their RDMA devices.
//! glidex's nodes build their summary and tree on it; hyperstor ranks its
//! reactors' ports by it (hyperstor's spec/26-networks.md §29.3).

pub mod net;
pub mod numa;
pub mod pci;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// How close two PCI functions are, nearest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Closeness {
    /// Under one PCIe switch: peer to peer without the root complex.
    Switch,
    /// On one NUMA node.
    Numa,
    /// On one socket.
    Socket,
    /// On another socket, or unknown.
    Far,
}

/// A host: NUMA nodes, PCI functions with their places, network ports.
#[derive(Debug, Clone, Default)]
pub struct Host {
    pub numa: Vec<numa::Numa>,
    pub pci: Vec<pci::RawPci>,
    pub places: BTreeMap<String, pci::Place>,
    pub ports: Vec<net::NetPort>,
}

impl Host {
    /// This host (`/sys`, and its addresses).
    pub fn read() -> Host {
        Host::read_in(Path::new("/sys"), &net::addresses())
    }

    /// The host under `sys`, with `addresses` by netdev.
    pub fn read_in(sys: &Path, addresses: &[(String, net::PortAddr)]) -> Host {
        let pci = pci::read_raw(sys);
        Host {
            numa: numa::read(sys),
            places: pci::places(&pci),
            ports: net::read(sys, addresses),
            pci,
        }
    }

    pub fn function(&self, address: &str) -> Option<&pci::RawPci> {
        self.pci.iter().find(|p| p.address == address)
    }

    /// A NUMA node's socket.
    pub fn socket_of(&self, node: u32) -> Option<u32> {
        self.numa
            .iter()
            .find(|n| n.id == node)
            .and_then(|n| n.socket)
    }

    /// How close two PCI functions are. Two functions are under one switch
    /// when any switch port is above both, however deep: on an HGX board a
    /// GPU and its NIC meet two switch levels up, below the root port
    /// (`nvidia-smi topo`'s `PXB`).
    pub fn closeness(&self, a: &str, b: &str) -> Closeness {
        if let (Some(fa), Some(fb)) = (self.function(a), self.function(b)) {
            let above = |f: &pci::RawPci| f.path[..f.path.len().saturating_sub(1)].to_vec();
            let (pa, pb) = (above(fa), above(fb));
            let shared = pa.iter().filter(|x| pb.contains(x)).any(|x| {
                matches!(
                    self.places.get(x).map(|p| p.kind),
                    Some(pci::PciKind::SwitchUpstream | pci::PciKind::SwitchDownstream)
                )
            });
            if shared {
                return Closeness::Switch;
            }
        }
        let node = |x: &str| self.function(x).and_then(|f| f.numa_node);
        match (node(a), node(b)) {
            (Some(x), Some(y)) if x == y => Closeness::Numa,
            (Some(x), Some(y)) => match (self.socket_of(x), self.socket_of(y)) {
                (Some(s), Some(t)) if s == t => Closeness::Socket,
                _ => Closeness::Far,
            },
            // One NUMA node, firmware giving none: all of it is local.
            _ if self.numa.len() <= 1 => Closeness::Numa,
            _ => Closeness::Far,
        }
    }
}

#[cfg(test)]
mod tests;
