//! SR-IOV virtual functions of a physical function (spec/host-topology.md
//! §10.1): how many a PF can have and has, and setting that number through
//! `sriov_numvfs`. Runs in glidex-netd (root).

use crate::OvsError;
use crate::exec::Exec;
use crate::nic;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A PF's VFs after a change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SriovState {
    pub address: String,
    pub total: u32,
    pub enabled: u32,
    /// The VFs' PCI addresses, `virtfn0` first.
    pub vfs: Vec<String>,
    /// The number changed: every VF it had before is gone (a change goes
    /// through 0), even where a new one has the same address.
    pub changed: bool,
}

fn dev_dir(bdf: &str) -> PathBuf {
    Path::new("/sys/bus/pci/devices").join(bdf)
}

fn read_u32(exec: &dyn Exec, path: &Path) -> Option<u32> {
    exec.read_file(path).ok()?.trim().parse().ok()
}

/// The PF's VFs, by `virtfn<n>` order.
pub fn vfs_of(exec: &dyn Exec, pf: &str) -> Vec<String> {
    let mut vfs: Vec<(u32, String)> = exec
        .list_dir(&dev_dir(pf))
        .into_iter()
        .filter_map(|name| {
            let n: u32 = name.strip_prefix("virtfn")?.parse().ok()?;
            let addr = exec.link_name(&dev_dir(pf).join(&name))?;
            Some((n, addr))
        })
        .collect();
    vfs.sort();
    vfs.into_iter().map(|(_, a)| a).collect()
}

/// Give PF `pf` `vfs` virtual functions (0 removes them). Refused for a
/// device that isn't SR-IOV capable, more than it supports, or a PF that
/// is an OVS-DPDK uplink. A PF with some VFs is set to 0 first: the
/// kernel takes no other change.
pub fn set_vfs(
    exec: &dyn Exec,
    pf: &str,
    vfs: u32,
    dpdk_uplinks: &[String],
) -> Result<SriovState, OvsError> {
    nic::validate_bdf(pf)?;
    let dir = dev_dir(pf);
    if !exec.exists(&dir) {
        return Err(OvsError::not_found(format!("PCI device {pf}")));
    }
    let total = read_u32(exec, &dir.join("sriov_totalvfs"))
        .filter(|t| *t > 0)
        .ok_or_else(|| OvsError::invalid(format!("{pf} has no SR-IOV virtual functions")))?;
    if vfs > total {
        return Err(OvsError::invalid(format!(
            "{pf} supports at most {total} virtual functions"
        )));
    }
    if dpdk_uplinks.iter().any(|u| u.eq_ignore_ascii_case(pf)) {
        return Err(OvsError::conflict(format!("{pf} is an OVS-DPDK uplink")));
    }
    let numvfs = dir.join("sriov_numvfs");
    let current = read_u32(exec, &numvfs).unwrap_or(0);
    let changed = current != vfs;
    if changed {
        if current > 0 && vfs > 0 {
            exec.write_file(&numvfs, b"0")?;
        }
        exec.write_file(&numvfs, vfs.to_string().as_bytes())?;
    }
    Ok(SriovState {
        address: pf.to_string(),
        total,
        enabled: read_u32(exec, &numvfs).unwrap_or(vfs),
        vfs: vfs_of(exec, pf),
        changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::RecordingExec;

    fn nic(numvfs: &str) -> RecordingExec {
        let exec = RecordingExec::new();
        let pf = "/sys/bus/pci/devices/0000:61:00.0";
        exec.file(pf, "");
        exec.file(&format!("{pf}/sriov_totalvfs"), "16\n");
        exec.file(&format!("{pf}/sriov_numvfs"), numvfs);
        exec
    }

    fn written(exec: &RecordingExec) -> Vec<String> {
        exec.writes()
            .into_iter()
            .map(|(_, d)| String::from_utf8_lossy(&d).into_owned())
            .collect()
    }

    #[test]
    fn vfs_are_set_through_zero_and_listed_in_order() {
        let exec = nic("2\n");
        let pf = "/sys/bus/pci/devices/0000:61:00.0";
        exec.link(&format!("{pf}/virtfn1"), "0000:61:00.3");
        exec.link(&format!("{pf}/virtfn0"), "0000:61:00.2");
        let s = set_vfs(&exec, "0000:61:00.0", 4, &[]).unwrap();
        assert_eq!(written(&exec), ["0", "4"], "a change goes through 0");
        assert_eq!((s.total, s.enabled), (16, 4));
        assert_eq!(s.vfs, ["0000:61:00.2", "0000:61:00.3"]);
        assert!(s.changed);
        // From none: one write; the same number: none.
        let exec = nic("0\n");
        set_vfs(&exec, "0000:61:00.0", 2, &[]).unwrap();
        assert_eq!(written(&exec), ["2"]);
        let exec = nic("2\n");
        set_vfs(&exec, "0000:61:00.0", 2, &[]).unwrap();
        assert!(written(&exec).is_empty());
    }

    #[test]
    fn too_many_uplinks_and_non_sriov_devices_are_refused() {
        let exec = nic("0\n");
        let e = set_vfs(&exec, "0000:61:00.0", 17, &[]).unwrap_err();
        assert!(e.to_string().contains("at most 16"), "{e}");
        let e = set_vfs(&exec, "0000:61:00.0", 2, &["0000:61:00.0".into()]).unwrap_err();
        assert!(e.to_string().contains("OVS-DPDK uplink"), "{e}");
        let bare = RecordingExec::new();
        bare.file("/sys/bus/pci/devices/0000:09:00.0", "");
        let e = set_vfs(&bare, "0000:09:00.0", 1, &[]).unwrap_err();
        assert!(e.to_string().contains("no SR-IOV"), "{e}");
        assert!(set_vfs(&bare, "0000:aa:00.0", 1, &[]).is_err());
        assert!(written(&exec).is_empty() && bare.writes().is_empty());
    }
}
