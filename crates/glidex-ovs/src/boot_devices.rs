//! Devices given to VMs from boot (spec/gpu-vms.md §10): SR-IOV VF counts
//! and devices bound to `vfio-pci` before glidex-netd, the control plane
//! and other hypervisors start. The installer writes the settings
//! (`--vfio-devices`, `--sriov`); `glidex-netd --boot-devices` applies them.
//!
//! A device already on `vfio-pci` is never touched: a VM may hold it.

use crate::OvsError;
use crate::exec::Exec;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `/etc/glidex/vfio-devices.json`.
pub const BOOT_DEVICES_FILE: &str = "/etc/glidex/vfio-devices.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootDevices {
    /// VF counts, applied first so the VFs exist to bind.
    #[serde(default)]
    pub sriov: Vec<SriovSetting>,
    /// What to bind: addresses, or a pool's `match` copied as it is.
    #[serde(default)]
    pub select: Vec<Selector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SriovSetting {
    pub pf: String,
    pub vfs: u32,
}

/// A pool's `match` (control-plane.json `pci.pools[].match`): every key
/// given must match; `vf_of` matches a VF's physical function.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// A class code or its start (`0x0302`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_code: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vf_of: Option<Box<Selector>>,
}

fn hex_id(s: &str) -> String {
    s.trim().trim_start_matches("0x").to_ascii_lowercase()
}

fn dev_dir(bdf: &str) -> PathBuf {
    Path::new("/sys/bus/pci/devices").join(bdf)
}

fn attr(exec: &dyn Exec, bdf: &str, f: &str) -> String {
    exec.read_file(&dev_dir(bdf).join(f))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

impl Selector {
    fn is_empty(&self) -> bool {
        self.vendor_id.is_none()
            && self.device_id.is_none()
            && self.class_code.is_none()
            && self.addresses.is_empty()
            && self.vf_of.is_none()
    }

    /// Whether PCI function `bdf` (as sysfs has it) is selected.
    pub fn matches(&self, exec: &dyn Exec, bdf: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        let own = self
            .vendor_id
            .as_deref()
            .is_none_or(|v| hex_id(v) == hex_id(&attr(exec, bdf, "vendor")))
            && self
                .device_id
                .as_deref()
                .is_none_or(|d| hex_id(d) == hex_id(&attr(exec, bdf, "device")))
            && self
                .class_code
                .as_deref()
                .is_none_or(|c| hex_id(&attr(exec, bdf, "class")).starts_with(&hex_id(c)))
            && (self.addresses.is_empty()
                || self.addresses.iter().any(|a| a.eq_ignore_ascii_case(bdf)));
        own && self.vf_of.as_deref().is_none_or(|pf| {
            exec.link_name(&dev_dir(bdf).join("physfn"))
                .is_some_and(|p| pf.matches(exec, &p))
        })
    }
}

/// What applying `cfg` would do to one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Already on `vfio-pci`: left as it is (a VM may hold it).
    AlreadyVfio,
    /// To be bound, from this driver (`None`: unbound).
    Bind { from: Option<String> },
}

/// The selected devices present now, each with what applying would do.
pub fn plan_devices(exec: &dyn Exec, cfg: &BootDevices) -> Vec<(String, Action)> {
    let mut out: Vec<(String, Action)> = exec
        .list_dir(Path::new("/sys/bus/pci/devices"))
        .into_iter()
        .filter(|bdf| cfg.select.iter().any(|s| s.matches(exec, bdf)))
        .map(|bdf| {
            let driver = exec.link_name(&dev_dir(&bdf).join("driver"));
            let a = if driver.as_deref() == Some("vfio-pci") {
                Action::AlreadyVfio
            } else {
                Action::Bind { from: driver }
            };
            (bdf, a)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Why PF `pf`'s VF count may not change now: one of its VFs is on
/// `vfio-pci`, so a VM may hold it (a change removes every VF). At boot no
/// VM runs yet, so `at_boot` skips the check.
pub fn sriov_change_refusal(exec: &dyn Exec, pf: &str, at_boot: bool) -> Option<String> {
    if at_boot {
        return None;
    }
    crate::sriov::vfs_of(exec, pf)
        .into_iter()
        .find(|vf| exec.link_name(&dev_dir(vf).join("driver")).as_deref() == Some("vfio-pci"))
        .map(|vf| format!("its VF {vf} is on vfio-pci and may be in a VM"))
}

/// Read the settings file; `Ok(None)` when there is none.
pub fn load(path: &Path) -> Result<Option<BootDevices>, OvsError> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| OvsError::invalid(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(OvsError::Io(format!("{}: {e}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::RecordingExec;

    /// Two A100s (one already on vfio-pci), a ConnectX PF with a VF on
    /// mlx5_core and one on vfio-pci, and an NVMe drive.
    fn host() -> RecordingExec {
        let exec = RecordingExec::new();
        for (bdf, vendor, device, class, driver) in [
            (
                "0000:87:00.0",
                "0x10de",
                "0x20b0",
                "0x030200",
                Some("nvidia"),
            ),
            (
                "0000:90:00.0",
                "0x10de",
                "0x20b0",
                "0x030200",
                Some("vfio-pci"),
            ),
            (
                "0000:61:00.0",
                "0x15b3",
                "0xa2dc",
                "0x020000",
                Some("mlx5_core"),
            ),
            (
                "0000:61:00.2",
                "0x15b3",
                "0x101e",
                "0x020000",
                Some("mlx5_core"),
            ),
            (
                "0000:61:00.3",
                "0x15b3",
                "0x101e",
                "0x020000",
                Some("vfio-pci"),
            ),
            ("0000:09:00.0", "0x144d", "0xa824", "0x010802", Some("nvme")),
        ] {
            let d = format!("/sys/bus/pci/devices/{bdf}");
            exec.file(&d, "");
            exec.file(&format!("{d}/vendor"), &format!("{vendor}\n"));
            exec.file(&format!("{d}/device"), &format!("{device}\n"));
            exec.file(&format!("{d}/class"), &format!("{class}\n"));
            if let Some(drv) = driver {
                exec.link(&format!("{d}/driver"), drv);
            }
        }
        let pf = "/sys/bus/pci/devices/0000:61:00.0";
        exec.link(&format!("{pf}/virtfn0"), "0000:61:00.2");
        exec.link(&format!("{pf}/virtfn1"), "0000:61:00.3");
        for vf in ["0000:61:00.2", "0000:61:00.3"] {
            exec.link(&format!("/sys/bus/pci/devices/{vf}/physfn"), "0000:61:00.0");
        }
        exec
    }

    #[test]
    fn selectors_are_pool_matches_and_vfio_devices_are_left_alone() {
        let exec = host();
        // A pool's match, as control-plane.json has it.
        let gpus: Selector =
            serde_json::from_str(r#"{"vendor_id":"0x10de","class_code":"0x0302"}"#).unwrap();
        let vfs: Selector =
            serde_json::from_str(r#"{"vf_of":{"vendor_id":"0x15b3","device_id":"0xa2dc"}}"#)
                .unwrap();
        let cfg = BootDevices {
            sriov: vec![],
            select: vec![gpus, vfs],
        };
        assert_eq!(
            plan_devices(&exec, &cfg),
            vec![
                (
                    "0000:61:00.2".to_string(),
                    Action::Bind {
                        from: Some("mlx5_core".into())
                    }
                ),
                ("0000:61:00.3".to_string(), Action::AlreadyVfio),
                (
                    "0000:87:00.0".to_string(),
                    Action::Bind {
                        from: Some("nvidia".into())
                    }
                ),
                ("0000:90:00.0".to_string(), Action::AlreadyVfio),
            ],
            "the PF and the NVMe drive aren't selected"
        );
        // By address.
        let one = BootDevices {
            sriov: vec![],
            select: vec![Selector {
                addresses: vec!["0000:09:00.0".into()],
                ..Default::default()
            }],
        };
        assert_eq!(plan_devices(&exec, &one).len(), 1);
        // An empty selector selects nothing.
        let none = BootDevices {
            sriov: vec![],
            select: vec![Selector::default()],
        };
        assert!(plan_devices(&exec, &none).is_empty());
    }

    #[test]
    fn a_vf_count_with_a_vf_on_vfio_waits_for_boot() {
        let exec = host();
        let why = sriov_change_refusal(&exec, "0000:61:00.0", false).unwrap();
        assert!(why.contains("0000:61:00.3"), "{why}");
        assert_eq!(sriov_change_refusal(&exec, "0000:61:00.0", true), None);
        assert_eq!(
            sriov_change_refusal(&exec, "0000:87:00.0", false),
            None,
            "no VFs"
        );
    }

    #[test]
    fn the_file_is_strict_and_optional() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("vfio-devices.json");
        assert_eq!(load(&p).unwrap(), None);
        std::fs::write(&p, r#"{"sriov":[{"pf":"0000:61:00.0","vfs":8}],"select":[{"addresses":["0000:87:00.0"]}]}"#).unwrap();
        let c = load(&p).unwrap().unwrap();
        assert_eq!(c.sriov[0].vfs, 8);
        std::fs::write(&p, r#"{"selct":[]}"#).unwrap();
        assert!(load(&p).is_err(), "a typo is refused, not ignored");
    }
}
