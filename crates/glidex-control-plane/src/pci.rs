use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const PCI_DEVICES_PATH: &str = "/sys/bus/pci/devices";

#[derive(Debug, Clone, Serialize)]
pub struct PciDeviceInfo {
    pub address: String,
    pub vendor_id: String,
    pub device_id: String,
    pub class_id: String,
    pub driver: Option<String>,
    pub iommu_group: Option<String>,
    /// `None` when the firmware gives none (`-1`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub numa_node: Option<u32>,
    pub sysfs_path: String,
}

/// Read a sysfs attribute file, returning trimmed contents.
fn read_sysfs_attr(device_path: &Path, attr: &str) -> Option<String> {
    fs::read_to_string(device_path.join(attr))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Get the driver name for a PCI device by reading the driver symlink.
fn get_driver(device_path: &Path) -> Option<String> {
    let driver_link = device_path.join("driver");
    fs::read_link(&driver_link)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
}

/// Get the IOMMU group number for a PCI device.
fn get_iommu_group(device_path: &Path) -> Option<String> {
    let iommu_link = device_path.join("iommu_group");
    fs::read_link(&iommu_link)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
}

/// The IOMMU group of each device that is not a PCI bridge (class
/// `0x0604xx`): bridges stay with the host when the rest of a group is
/// passed through.
pub fn iommu_groups(devices: &[PciDeviceInfo]) -> BTreeMap<String, String> {
    devices
        .iter()
        .filter(|d| !d.class_id.to_ascii_lowercase().starts_with("0x0604"))
        .filter_map(|d| {
            d.iommu_group
                .as_ref()
                .map(|g| (d.address.to_ascii_lowercase(), g.clone()))
        })
        .collect()
}

/// Scan all PCI devices from sysfs and return their information.
pub fn scan_pci_devices() -> Vec<PciDeviceInfo> {
    let pci_path = Path::new(PCI_DEVICES_PATH);
    if !pci_path.exists() {
        return Vec::new();
    }

    let mut devices = Vec::new();

    let entries = match fs::read_dir(pci_path) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    for entry in entries.flatten() {
        let device_path = entry.path();
        let address = match entry.file_name().to_str() {
            Some(name) => name.to_string(),
            None => continue,
        };

        let vendor_id = read_sysfs_attr(&device_path, "vendor").unwrap_or_default();
        let device_id = read_sysfs_attr(&device_path, "device").unwrap_or_default();
        let class_id = read_sysfs_attr(&device_path, "class").unwrap_or_default();
        let driver = get_driver(&device_path);
        let iommu_group = get_iommu_group(&device_path);
        let numa_node = read_sysfs_attr(&device_path, "numa_node").and_then(|n| n.parse().ok());

        devices.push(PciDeviceInfo {
            address: address.clone(),
            vendor_id,
            device_id,
            class_id,
            driver,
            iommu_group,
            numa_node,
            sysfs_path: format!("{}/{}", PCI_DEVICES_PATH, address),
        });
    }

    devices.sort_by(|a, b| a.address.cmp(&b.address));
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(address: &str, class: &str, group: Option<&str>) -> PciDeviceInfo {
        PciDeviceInfo {
            address: address.into(),
            vendor_id: "0x10de".into(),
            device_id: "0x2330".into(),
            class_id: class.into(),
            driver: None,
            iommu_group: group.map(Into::into),
            sysfs_path: format!("{PCI_DEVICES_PATH}/{address}"),
        }
    }

    #[test]
    fn groups_leave_out_bridges_and_devices_without_one() {
        let groups = iommu_groups(&[
            dev("0000:40:01.1", "0x060400", Some("31")),
            dev("0000:41:00.0", "0x030200", Some("31")),
            dev("0000:41:00.1", "0x040300", Some("31")),
            dev("0000:42:00.0", "0x020000", None),
        ]);
        assert_eq!(
            groups.into_iter().collect::<Vec<_>>(),
            [
                ("0000:41:00.0".to_string(), "31".to_string()),
                ("0000:41:00.1".to_string(), "31".to_string()),
            ]
        );
    }
}
