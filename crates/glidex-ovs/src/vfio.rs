//! Preparing a PCI device for VM passthrough (spec/host-topology.md §5.3):
//! its whole IOMMU group bound to `vfio-pci`, the group device given to the
//! VMs' group, and back again. Runs in glidex-netd (root).

use crate::OvsError;
use crate::exec::Exec;
use crate::nic;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One function of a prepared group, and the driver it had.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub address: String,
    /// `None`: unbound, or already on `vfio-pci`, so nothing to put back.
    pub previous_driver: Option<String>,
}

/// What `prepare` did, so `release` can undo it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prepared {
    /// The device asked for.
    pub address: String,
    pub iommu_group: String,
    pub members: Vec<Member>,
    /// The udev rule file written for the group device.
    pub udev_rule: String,
}

fn dev_dir(bdf: &str) -> PathBuf {
    Path::new("/sys/bus/pci/devices").join(bdf)
}

/// The udev rule file for IOMMU group `group`.
pub fn udev_rule_path(group: &str) -> PathBuf {
    PathBuf::from(format!("/etc/udev/rules.d/90-glidex-vfio-{group}.rules"))
}

/// A device's IOMMU group.
pub fn iommu_group(exec: &dyn Exec, bdf: &str) -> Option<String> {
    exec.link_name(&dev_dir(bdf).join("iommu_group"))
}

/// The functions of IOMMU group `group` a VM takes with it: all but
/// bridges (class `0x06…`), which stay with the host.
pub fn group_members(exec: &dyn Exec, group: &str) -> Vec<String> {
    exec.list_dir(
        &Path::new("/sys/kernel/iommu_groups")
            .join(group)
            .join("devices"),
    )
    .into_iter()
    .filter(|a| {
        let class = exec
            .read_file(&dev_dir(a).join("class"))
            .unwrap_or_default();
        !class.trim().trim_start_matches("0x").starts_with("06")
    })
    .collect()
}

/// Why `bdf` (and its group) can't be given to VMs: a function that is a
/// DPDK uplink of a glidex bridge, or whose NIC carries the host's own
/// addresses. `None`: it may be.
pub fn refusal(exec: &dyn Exec, bdf: &str, dpdk_uplinks: &[String]) -> Option<String> {
    nic::validate_bdf(bdf)
        .err()
        .map(|e| e.to_string())
        .or_else(|| {
            let group = iommu_group(exec, bdf)?;
            group_members(exec, &group).into_iter().find_map(|m| {
                if dpdk_uplinks.iter().any(|u| u.eq_ignore_ascii_case(&m)) {
                    return Some(format!("{m} is an OVS-DPDK uplink"));
                }
                nic::pci_netdevs(exec, &m).into_iter().find_map(|ifname| {
                    nic::classify(exec, &ifname)
                        .ok()
                        .filter(|c| c.in_use())
                        .map(|c| {
                            format!(
                                "{m} carries {ifname}, which the host uses: {}",
                                c.reasons.join("; ")
                            )
                        })
                })
            })
        })
}

/// Bind `bdf`'s IOMMU group to `vfio-pci` and give `/dev/vfio/<group>` to
/// `vm_group` (0660), through a udev rule that keeps it so and a change
/// now. Not persistent across reboots (the installer's `--vfio-devices`
/// is).
pub fn prepare(
    exec: &dyn Exec,
    bdf: &str,
    dpdk_uplinks: &[String],
    vm_group: &str,
) -> Result<Prepared, OvsError> {
    nic::validate_bdf(bdf)?;
    if !exec.exists(&dev_dir(bdf)) {
        return Err(OvsError::not_found(format!("PCI device {bdf}")));
    }
    let group = iommu_group(exec, bdf).ok_or_else(|| {
        OvsError::invalid(format!(
            "{bdf} has no IOMMU group: turn the IOMMU on (intel_iommu=on / amd_iommu=on)"
        ))
    })?;
    if let Some(why) = refusal(exec, bdf, dpdk_uplinks) {
        return Err(OvsError::conflict(why));
    }
    let mut members = Vec::new();
    for m in group_members(exec, &group) {
        let driver = nic::pci_driver(exec, &m)?;
        if driver.as_deref() != Some("vfio-pci") {
            nic::bind_driver(exec, &m, "vfio-pci")?;
        }
        members.push(Member {
            address: m,
            previous_driver: driver.filter(|d| d != "vfio-pci"),
        });
    }
    let rule = udev_rule_path(&group);
    exec.write_file(
        &rule,
        format!(
            "# glidex: IOMMU group {group} ({bdf}) for VM passthrough\n\
             SUBSYSTEM==\"vfio\", KERNEL==\"{group}\", GROUP=\"{vm_group}\", MODE=\"0660\"\n"
        )
        .as_bytes(),
    )?;
    let node = PathBuf::from(format!("/dev/vfio/{group}"));
    if exec.exists(&node) {
        exec.set_group_mode(&node, vm_group, 0o660)?;
    }
    Ok(Prepared {
        address: bdf.to_string(),
        iommu_group: group,
        members,
        udev_rule: rule.display().to_string(),
    })
}

/// Undo `prepare`: each function back to the driver it had (the kernel's
/// choice again), and the udev rule gone.
pub fn release(exec: &dyn Exec, p: &Prepared) -> Result<(), OvsError> {
    for m in &p.members {
        if m.previous_driver.is_some() {
            nic::restore_driver(exec, &m.address)?;
        }
    }
    exec.remove_file(Path::new(&p.udev_rule))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::RecordingExec;

    /// A GPU and its audio function in group 30 behind a bridge, both on
    /// host drivers.
    fn host() -> RecordingExec {
        let exec = RecordingExec::new();
        for (bdf, class, driver) in [
            ("0000:07:00.0", "0x030200", "nvidia"),
            ("0000:07:00.1", "0x040300", "snd_hda_intel"),
            ("0000:00:03.1", "0x060400", "pcieport"),
        ] {
            exec.file(&format!("/sys/bus/pci/devices/{bdf}"), "");
            exec.file(&format!("/sys/bus/pci/devices/{bdf}/class"), class);
            exec.file(
                &format!("/sys/bus/pci/devices/{bdf}/uevent"),
                &format!("DRIVER={driver}\n"),
            );
            exec.link(&format!("/sys/bus/pci/devices/{bdf}/iommu_group"), "30");
            exec.link(&format!("/sys/kernel/iommu_groups/30/devices/{bdf}"), bdf);
        }
        exec.file("/dev/vfio/30", "");
        exec
    }

    #[test]
    fn the_whole_group_goes_to_vfio_and_comes_back() {
        let exec = host();
        let p = prepare(&exec, "0000:07:00.0", &[], "glidex").unwrap();
        assert_eq!(p.iommu_group, "30");
        assert_eq!(
            p.members,
            vec![
                Member {
                    address: "0000:07:00.0".into(),
                    previous_driver: Some("nvidia".into()),
                },
                Member {
                    address: "0000:07:00.1".into(),
                    previous_driver: Some("snd_hda_intel".into()),
                },
            ],
            "the bridge stays with the host"
        );
        let writes: Vec<String> = exec
            .writes()
            .into_iter()
            .map(|(p, d)| format!("{}={}", p.display(), String::from_utf8_lossy(&d)))
            .collect();
        for bdf in ["0000:07:00.0", "0000:07:00.1"] {
            assert!(
                writes.contains(&format!(
                    "/sys/bus/pci/devices/{bdf}/driver_override=vfio-pci"
                )),
                "{writes:?}"
            );
        }
        assert!(
            writes.iter().any(
                |w| w.starts_with("/etc/udev/rules.d/90-glidex-vfio-30.rules=")
                    && w.contains("KERNEL==\"30\", GROUP=\"glidex\", MODE=\"0660\"")
            ),
            "{writes:?}"
        );
        assert!(
            exec.calls()
                .contains(&"chgrp glidex /dev/vfio/30; chmod 660".to_string())
        );
        exec.clear_calls();
        release(&exec, &p).unwrap();
        assert!(
            exec.calls()
                .contains(&"rm /etc/udev/rules.d/90-glidex-vfio-30.rules".to_string())
        );
        let restored = exec
            .writes()
            .iter()
            .filter(|(p, d)| p.ends_with("driver_override") && d == b"\n")
            .count();
        assert_eq!(restored, 2);
    }

    #[test]
    fn host_devices_and_uplinks_are_refused() {
        let exec = host();
        let e = prepare(&exec, "0000:07:00.0", &["0000:07:00.1".into()], "glidex").unwrap_err();
        assert!(
            e.to_string().contains("0000:07:00.1 is an OVS-DPDK uplink"),
            "{e}"
        );
        // No IOMMU group: refused before anything is touched.
        let bare = RecordingExec::new();
        bare.file("/sys/bus/pci/devices/0000:09:00.0", "");
        let e = prepare(&bare, "0000:09:00.0", &[], "glidex").unwrap_err();
        assert!(e.to_string().contains("no IOMMU group"), "{e}");
        assert!(bare.writes().is_empty());
        assert!(prepare(&bare, "nope", &[], "glidex").is_err());
    }
}
