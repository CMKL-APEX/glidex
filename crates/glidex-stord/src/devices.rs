//! Which block devices each VM's unit may open (spec/24-storage.md S18): a
//! drop-in per VM, `glidex-vm@<id>.service.d/50-glidex-volumes.conf`,
//! listing one `DeviceAllow=` each, added to the unit's own list. The
//! drop-ins are under `/run`, so they are also all the state there is.

use crate::lvm::Exec;
use crate::proto::StordError;
use std::path::{Path, PathBuf};

pub const DROP_IN: &str = "50-glidex-volumes.conf";

fn dir(systemd_dir: &Path, vm_id: &str) -> PathBuf {
    systemd_dir.join(format!("glidex-vm@{vm_id}.service.d"))
}

/// The drop-in's text: a device must be one of glidex's volumes,
/// `/dev/<vg>/gx-…`, in an allowed volume group.
pub fn drop_in(devices: &[String], allowed_vgs: &[String]) -> Result<String, StordError> {
    let mut out = String::from(
        "# Written by glidex-stord: the block devices of this VM's disks.\n[Service]\n",
    );
    for d in devices {
        let ok = d
            .strip_prefix("/dev/")
            .and_then(|r| r.split_once('/'))
            .is_some_and(|(vg, lv)| {
                allowed_vgs.iter().any(|a| a == vg) && crate::names::volume(lv).is_ok()
            });
        if !ok {
            return Err(StordError::NotAllowed(format!(
                "{d} is not a glidex volume in an allowed volume group"
            )));
        }
        out.push_str(&format!("DeviceAllow={d} rw\n"));
    }
    Ok(out)
}

/// Make VM `vm_id`'s drop-in list exactly `devices` (none: no drop-in),
/// then have systemd read it. Nothing is reloaded when nothing changed.
pub fn allow(
    ex: &dyn Exec,
    systemd_dir: &Path,
    vm_id: &str,
    devices: &[String],
    allowed_vgs: &[String],
) -> Result<bool, StordError> {
    crate::names::vm_id(vm_id)?;
    let d = dir(systemd_dir, vm_id);
    let file = d.join(DROP_IN);
    let io = |e: std::io::Error| StordError::Failed(format!("{}: {e}", file.display()));
    let old = std::fs::read_to_string(&file).ok();
    let new = if devices.is_empty() {
        None
    } else {
        Some(drop_in(devices, allowed_vgs)?)
    };
    if old == new {
        return Ok(false);
    }
    match &new {
        Some(text) => {
            std::fs::create_dir_all(&d).map_err(io)?;
            let tmp = d.join(format!(".{DROP_IN}.tmp"));
            std::fs::write(&tmp, text).map_err(io)?;
            std::fs::rename(&tmp, &file).map_err(io)?;
        }
        None => {
            let _ = std::fs::remove_file(&file);
            let _ = std::fs::remove_dir(&d);
        }
    }
    reload(ex)?;
    Ok(true)
}

fn reload(ex: &dyn Exec) -> Result<(), StordError> {
    let o = ex.run("systemctl", &["daemon-reload"]);
    if o.ok {
        Ok(())
    } else {
        Err(StordError::Failed(format!(
            "systemctl daemon-reload: {}",
            o.stderr.trim()
        )))
    }
}

/// The VMs that have a drop-in.
pub fn list(systemd_dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(systemd_dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let id = n.strip_prefix("glidex-vm@")?.strip_suffix(".service.d")?;
            e.path().join(DROP_IN).exists().then(|| id.to_string())
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lvm::fake::Fake;

    #[test]
    fn a_vm_gets_exactly_its_own_volumes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = Fake::default();
        let vgs = vec!["vg0".to_string()];
        let vm = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";
        let devs = vec!["/dev/vg0/gx-d-1".to_string(), "/dev/vg0/gx-d-2".to_string()];
        assert!(allow(&f, tmp.path(), vm, &devs, &vgs).unwrap());
        let text = std::fs::read_to_string(dir(tmp.path(), vm).join(DROP_IN)).unwrap();
        assert!(
            text.contains("DeviceAllow=/dev/vg0/gx-d-1 rw\nDeviceAllow=/dev/vg0/gx-d-2 rw\n"),
            "{text}"
        );
        // No reset line: the unit's own devices (/dev/kvm) stay (§19, H5).
        assert!(!text.contains("DeviceAllow=\n"));
        assert_eq!(list(tmp.path()), vec![vm.to_string()]);
        // The same again reloads nothing.
        assert!(!allow(&f, tmp.path(), vm, &devs, &vgs).unwrap());
        let reloads = |f: &Fake| {
            f.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| *c == "systemctl daemon-reload")
                .count()
        };
        assert_eq!(reloads(&f), 1);

        // Not glidex's, or not in an allowed VG: refused, nothing written.
        for bad in [
            "/dev/ubuntu-vg/ubuntu-lv",
            "/dev/vg1/gx-d-1",
            "/dev/sda",
            "/dev/vg0/../sda",
        ] {
            assert!(
                allow(&f, tmp.path(), vm, &[bad.to_string()], &vgs).is_err(),
                "{bad}"
            );
        }
        assert!(allow(&f, tmp.path(), "../x", &devs, &vgs).is_err());

        assert!(allow(&f, tmp.path(), vm, &[], &vgs).unwrap());
        assert!(list(tmp.path()).is_empty());
        assert!(!dir(tmp.path(), vm).exists());
        assert_eq!(reloads(&f), 2);
    }
}
