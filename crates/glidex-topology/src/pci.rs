//! The PCIe tree as sysfs shows it (spec/22-host-topology.md §3.2): each
//! function's ids, driver, IOMMU group, NUMA node, link and SR-IOV, and
//! where it sits: its kind, its root port and the switch above it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

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

/// AER error counts since boot (spec/22-host-topology.md §5.4).
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
    aer_in(&Path::new("/sys/bus/pci/devices").join(address))
}

fn aer_in(dir: &Path) -> Option<AerCounts> {
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
    /// The driver its first VF is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vf_driver: Option<String>,
    /// VFs bound to `vfio-pci`, which may be passed through.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ready_vfs: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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

/// A PCI-to-PCI bridge (class `0x0604`).
pub fn is_bridge(class: &str) -> bool {
    class
        .trim_start_matches("0x")
        .to_ascii_lowercase()
        .starts_with("0604")
}

/// Every PCI function under `sys` (`/sys` on a host; a copy of its layout
/// in tests).
pub fn read_raw(sys: &Path) -> Vec<RawPci> {
    let base = sys.join("bus/pci/devices");
    let devices = sys.join("devices");
    let attr = |dir: &Path, f: &str| {
        std::fs::read_to_string(dir.join(f))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let link_name = |dir: &Path, f: &str| {
        std::fs::read_link(dir.join(f))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    };
    let mut raw = Vec::new();
    for e in std::fs::read_dir(base).into_iter().flatten().flatten() {
        let dir = e.path();
        let address = e.file_name().to_string_lossy().into_owned();
        let real = std::fs::canonicalize(&dir).unwrap_or_default();
        let devices = std::fs::canonicalize(&devices).unwrap_or_else(|_| devices.clone());
        let path: Vec<String> = real
            .strip_prefix(&devices)
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
                vf_driver: link_name(&dir.join("virtfn0"), "driver"),
                ready_vfs: 0,
            }),
            physfn: link_name(&dir, "physfn"),
            aer: aer_in(&dir),
            address,
        });
    }
    raw
}

/// Where a function sits in the tree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    pub kind: PciKind,
    /// Its root port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_port: Option<String>,
    /// The upstream port of the nearest switch above it: devices under one
    /// switch can talk peer to peer without the root complex (§3.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch: Option<String>,
}

/// Each function's place, by address (pure, for tests).
pub fn places(raw: &[RawPci]) -> BTreeMap<String, Place> {
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
    raw.iter()
        .map(|r| {
            let above: Vec<&str> = r
                .path
                .iter()
                .take(r.path.len().saturating_sub(1))
                .map(String::as_str)
                .filter(|p| by_addr.contains_key(p))
                .collect();
            let place = Place {
                kind: kinds.get(&r.address).copied().unwrap_or_default(),
                root_port: above
                    .iter()
                    .find(|a| kinds.get(**a) == Some(&PciKind::RootPort))
                    .map(|a| a.to_string()),
                switch: above
                    .iter()
                    .rev()
                    .find(|a| kinds.get(**a) == Some(&PciKind::SwitchUpstream))
                    .map(|a| a.to_string()),
            };
            (r.address.clone(), place)
        })
        .collect()
}
