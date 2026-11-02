//! Network ports: each physical netdev with its PCI function, link layer,
//! speed and state, the bond or bridge it belongs to, its RDMA port, and
//! for InfiniBand the fabric it is on (spec/22-host-topology.md §3.5).

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkLayer {
    #[default]
    Ethernet,
    Infiniband,
    Other,
}

/// A port's RDMA side (`/sys/class/infiniband/<device>/ports/<n>`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RdmaPort {
    /// `mlx5_0`.
    pub device: String,
    pub port: u32,
    pub link_layer: LinkLayer,
    /// `ACTIVE`, `DOWN`, … without the number.
    pub state: String,
    /// Gb/s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_gbps: Option<u32>,
    /// InfiniBand: the first 64 bits of GID 0 (`fe80:0000:0000:0000`).
    /// With `sm_lid`, two ports that share both are on one fabric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subnet_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sm_lid: Option<u32>,
}

/// An address on a port, with its prefix length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortAddr {
    pub addr: IpAddr,
    pub prefix: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetPort {
    /// The netdev (`ibp12s0`).
    pub name: String,
    /// Its PCI function.
    pub pci: String,
    pub link_layer: LinkLayer,
    /// `operstate`: `up`, `down`, …
    pub state: String,
    /// Mb/s, when the link reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_mbps: Option<u32>,
    pub mtu: u32,
    /// The bond or bridge it is enslaved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rdma: Option<RdmaPort>,
    /// Addresses on it, or on its master when it has one (a bond's).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<PortAddr>,
}

/// The ports under `sys`, with `addresses` (by netdev) put on them: a
/// port's own, or its master's. Only netdevs on a PCI function count:
/// virtual ones (bonds, bridges, veths, tunnels) have none.
pub fn read(sys: &Path, addresses: &[(String, PortAddr)]) -> Vec<NetPort> {
    let attr = |p: &Path| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let link_name = |p: &Path| {
        std::fs::read_link(p)
            .ok()
            .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()))
    };
    let rdma = rdma_ports(sys);
    let mut out = Vec::new();
    for e in std::fs::read_dir(sys.join("class/net"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let dir = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(pci) = link_name(&dir.join("device")).filter(|p| is_pci_address(p)) else {
            continue;
        };
        // ARPHRD_ETHER 1, ARPHRD_INFINIBAND 32.
        let link_layer = match attr(&dir.join("type")).as_deref() {
            Some("1") => LinkLayer::Ethernet,
            Some("32") => LinkLayer::Infiniband,
            _ => LinkLayer::Other,
        };
        let master = link_name(&dir.join("master"));
        let addresses = addresses
            .iter()
            .filter(|(n, _)| *n == name || Some(n) == master.as_ref())
            .map(|(_, a)| a.clone())
            .collect();
        // A port's RDMA device is the one whose netdev it is, else (a bond's
        // member, under `mlx5_bond_0`) the one on its PCI function.
        let rdma = rdma
            .iter()
            .find(|(nd, _, _)| nd.as_deref() == Some(&name))
            .or_else(|| rdma.iter().find(|(_, p, _)| *p == pci))
            .map(|(_, _, r)| r.clone());
        out.push(NetPort {
            state: attr(&dir.join("operstate")).unwrap_or_default(),
            speed_mbps: attr(&dir.join("speed"))
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|s| *s > 0)
                .map(|s| s as u32),
            mtu: attr(&dir.join("mtu"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            name,
            pci,
            link_layer,
            master,
            rdma,
            addresses,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// `0000:0c:00.0`: a USB network device (a BMC's) has a device too.
fn is_pci_address(s: &str) -> bool {
    let b = s.as_bytes();
    let hex = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_hexdigit);
    b.len() == 12
        && hex(0..4)
        && b[4] == b':'
        && hex(5..7)
        && b[7] == b':'
        && hex(8..10)
        && b[10] == b'.'
        && b[11].is_ascii_digit()
}

/// Every RDMA port: the netdev it names (`gid_attrs/ndevs/0`, RoCE only),
/// its device's PCI function, and the port.
fn rdma_ports(sys: &Path) -> Vec<(Option<String>, String, RdmaPort)> {
    let attr = |p: &Path| {
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let mut out = Vec::new();
    for dev in std::fs::read_dir(sys.join("class/infiniband"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let device = dev.file_name().to_string_lossy().into_owned();
        let dir = dev.path();
        let pci = std::fs::read_link(dir.join("device"))
            .ok()
            .and_then(|l| l.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        for p in std::fs::read_dir(dir.join("ports"))
            .into_iter()
            .flatten()
            .flatten()
        {
            let Ok(port) = p.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let pd = p.path();
            let link_layer = match attr(&pd.join("link_layer")).as_deref() {
                Some("InfiniBand") => LinkLayer::Infiniband,
                Some("Ethernet") => LinkLayer::Ethernet,
                _ => LinkLayer::Other,
            };
            // "4: ACTIVE"
            let state = attr(&pd.join("state"))
                .map(|s| s.rsplit(' ').next().unwrap_or_default().to_string())
                .unwrap_or_default();
            // "200 Gb/sec (4X HDR)"
            let rate_gbps = attr(&pd.join("rate"))
                .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
                .map(|g| g as u32);
            let ib = link_layer == LinkLayer::Infiniband;
            let subnet_prefix = attr(&pd.join("gids/0"))
                .filter(|_| ib)
                .map(|g| g.split(':').take(4).collect::<Vec<_>>().join(":"));
            let sm_lid = attr(&pd.join("sm_lid"))
                .filter(|_| ib)
                .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
            let ndev = attr(&pd.join("gid_attrs/ndevs/0"));
            out.push((
                ndev,
                pci.clone(),
                RdmaPort {
                    device: device.clone(),
                    port,
                    link_layer,
                    state,
                    rate_gbps,
                    subnet_prefix,
                    sm_lid,
                },
            ));
        }
    }
    out
}

/// This host's addresses by netdev (`getifaddrs`).
pub fn addresses() -> Vec<(String, PortAddr)> {
    let Ok(addrs) = nix::ifaddrs::getifaddrs() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for a in addrs {
        let (Some(addr), Some(mask)) = (a.address, a.netmask) else {
            continue;
        };
        let (ip, prefix) =
            if let (Some(ip), Some(m)) = (addr.as_sockaddr_in(), mask.as_sockaddr_in()) {
                (IpAddr::V4(ip.ip()), u32::from(m.ip()).count_ones() as u8)
            } else if let (Some(ip), Some(m)) = (addr.as_sockaddr_in6(), mask.as_sockaddr_in6()) {
                (IpAddr::V6(ip.ip()), u128::from(m.ip()).count_ones() as u8)
            } else {
                continue;
            };
        out.push((a.interface_name, PortAddr { addr: ip, prefix }));
    }
    out
}
