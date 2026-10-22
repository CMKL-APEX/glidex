//! Boot devices with netd already running (spec/gpu-vms.md §10): the
//! installer's `--now`, through netd's own ops so netd keeps its records.
//! At boot, before netd, [`crate::server::Netd::apply_boot_devices`] does
//! the same directly.

use crate::proto::Op;
use crate::server::BootReport;
use glidex_ovs::Exec;
use glidex_ovs::boot_devices::{Action, BootDevices, plan_devices, sriov_change_refusal};
use serde_json::Value;

/// Apply `cfg` through `call` (a netd client): VF counts, unless a VF of
/// the PF is on `vfio-pci`, then each selected device not on `vfio-pci`.
pub fn apply_live(
    exec: &dyn Exec,
    cfg: &BootDevices,
    mut call: impl FnMut(Op) -> Result<Value, String>,
) -> BootReport {
    let mut report = BootReport::default();
    for s in &cfg.sriov {
        let pf = s.pf.to_ascii_lowercase();
        let current = glidex_ovs::sriov::vfs_of(exec, &pf).len() as u32;
        if current == s.vfs {
            report.sriov.push((pf, current));
            continue;
        }
        if let Some(why) = sriov_change_refusal(exec, &pf, false) {
            report
                .skipped
                .push((pf, format!("VF count left at {current}: {why}")));
            continue;
        }
        match call(Op::SetSriovVfs {
            address: pf.clone(),
            vfs: s.vfs,
        }) {
            Ok(v) => report
                .sriov
                .push((pf, v["enabled"].as_u64().unwrap_or(0) as u32)),
            Err(e) => report.failed.push((pf, e)),
        }
    }
    for (address, action) in plan_devices(exec, cfg) {
        match action {
            Action::AlreadyVfio => report.skipped.push((address, "already on vfio-pci".into())),
            Action::Bind { .. } => match call(Op::PrepareDevice {
                address: address.clone(),
            }) {
                Ok(_) => report.bound.push(address),
                Err(e) => report.failed.push((address, e)),
            },
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use glidex_ovs::boot_devices::{Selector, SriovSetting};
    use glidex_ovs::exec::RecordingExec;

    #[test]
    fn live_application_goes_through_netd_ops_and_spares_vfio_devices() {
        let exec = RecordingExec::new();
        for (bdf, driver) in [("0000:87:00.0", "nvidia"), ("0000:90:00.0", "vfio-pci")] {
            let d = format!("/sys/bus/pci/devices/{bdf}");
            exec.file(&d, "");
            exec.file(&format!("{d}/vendor"), "0x10de\n");
            exec.link(&format!("{d}/driver"), driver);
        }
        let pf = "/sys/bus/pci/devices/0000:61:00.0";
        exec.file(pf, "");
        exec.link(&format!("{pf}/virtfn0"), "0000:61:00.2");
        let cfg = BootDevices {
            sriov: vec![
                SriovSetting {
                    pf: "0000:61:00.0".into(),
                    vfs: 4,
                },
                SriovSetting {
                    pf: "0000:62:00.0".into(),
                    vfs: 0,
                },
            ],
            select: vec![Selector {
                vendor_id: Some("0x10de".into()),
                ..Default::default()
            }],
        };
        let mut sent = Vec::new();
        let r = apply_live(&exec, &cfg, |op| {
            sent.push(op.name().to_string());
            match op {
                Op::SetSriovVfs { vfs, .. } => Ok(serde_json::json!({ "enabled": vfs })),
                _ => Ok(Value::Null),
            }
        });
        assert_eq!(sent, ["set_sriov_vfs", "prepare_device"], "{r:?}");
        assert_eq!(
            r.sriov,
            [
                ("0000:61:00.0".to_string(), 4),
                ("0000:62:00.0".to_string(), 0)
            ]
        );
        assert_eq!(r.bound, ["0000:87:00.0"]);
        assert!(r.skipped.iter().any(|(a, _)| a == "0000:90:00.0"), "{r:?}");
        // A VF on vfio-pci holds the count; nothing is sent for it.
        exec.file("/sys/bus/pci/devices/0000:61:00.2", "");
        exec.link("/sys/bus/pci/devices/0000:61:00.2/driver", "vfio-pci");
        let mut sent = Vec::new();
        let r = apply_live(&exec, &cfg, |op| {
            sent.push(op.name().to_string());
            Ok(Value::Null)
        });
        assert!(!sent.contains(&"set_sriov_vfs".to_string()), "{sent:?}");
        assert!(
            r.skipped
                .iter()
                .any(|(a, why)| a == "0000:61:00.0" && why.contains("0000:61:00.2")),
            "{r:?}"
        );
        // An error from netd is reported, not fatal.
        let r = apply_live(&exec, &cfg, |_| Err("refused".into()));
        assert_eq!(r.failed.len(), 1, "{r:?}");
    }
}
