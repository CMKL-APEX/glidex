//! Keeping passthrough devices at D0 (spec/22-host-topology.md H28, §5.3a):
//! runtime power management off and D3cold disallowed on each device, and
//! D3cold disallowed on the bridges above it up to its root port. What was
//! there before the first write is kept, so a device that leaves the set,
//! and a bridge with no kept device below it, gets it back. Runs in
//! glidex-netd (root).

use crate::OvsError;
use crate::exec::Exec;
use crate::nic;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A device's runtime power management (`on`: never suspended; `auto`).
pub const CONTROL: &str = "power/control";
/// Whether the kernel may put the function in D3cold (`1`, `0`).
pub const D3COLD: &str = "d3cold_allowed";

/// One attribute glidex changed, and its value before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub address: String,
    pub attr: String,
    pub previous: String,
}

impl Saved {
    /// Its key in netd's store.
    pub fn key(&self) -> String {
        format!("{} {}", self.address, self.attr)
    }
}

/// What [`keep_on`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepReport {
    /// `<address> <attr>=<value>` written to keep a device on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub written: Vec<String>,
    /// `<address> <attr>=<value>` put back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restored: Vec<String>,
    /// An address or attribute, and why it couldn't be done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<(String, String)>,
}

fn dev_dir(bdf: &str) -> PathBuf {
    Path::new("/sys/bus/pci/devices").join(bdf)
}

/// The bridges on `bdf`'s path, from the one just above it up to its root
/// port, as its sysfs link names them
/// (`../../../devices/pci0000:40/0000:40:01.1/0000:41:00.0/…/<bdf>`).
pub fn bridges_above(exec: &dyn Exec, bdf: &str) -> Vec<String> {
    let Some(target) = exec.read_link(&dev_dir(bdf)) else {
        return Vec::new();
    };
    let mut path: Vec<String> = target
        .split('/')
        .filter(|c| nic::validate_bdf(c).is_ok())
        .map(str::to_ascii_lowercase)
        .collect();
    if path.last().map(String::as_str) == Some(bdf) {
        path.pop();
    }
    path.reverse();
    path
}

/// The value each attribute should have to keep `addresses` on.
fn wanted(exec: &dyn Exec, addresses: &[String]) -> BTreeMap<(String, String), &'static str> {
    let mut want = BTreeMap::new();
    for a in addresses {
        want.insert((a.clone(), CONTROL.to_string()), "on");
        want.insert((a.clone(), D3COLD.to_string()), "0");
        for b in bridges_above(exec, a) {
            want.insert((b, D3COLD.to_string()), "0");
        }
    }
    want
}

/// Keep `addresses` (checked PCI addresses) at D0, and put back what
/// `saved` records for anything no longer needed. Returns what it did and
/// the records to keep: a value is recorded only when glidex first changes
/// it, and stays recorded while it is wanted.
pub fn keep_on(exec: &dyn Exec, addresses: &[String], saved: &[Saved]) -> (KeepReport, Vec<Saved>) {
    let mut report = KeepReport::default();
    let mut now = Vec::new();
    let mut gone: BTreeSet<&str> = BTreeSet::new();
    for a in addresses {
        if !exec.exists(&dev_dir(a)) {
            report.failed.push((a.clone(), "not on the bus".into()));
            gone.insert(a);
        }
    }
    let present: Vec<String> = addresses
        .iter()
        .filter(|a| !gone.contains(a.as_str()))
        .cloned()
        .collect();
    let want = wanted(exec, &present);
    for ((address, attr), value) in &want {
        let before = saved
            .iter()
            .find(|s| s.address == *address && s.attr == *attr);
        let path = dev_dir(address).join(attr);
        let current = match exec.read_file(&path) {
            Ok(c) => c.trim().to_string(),
            Err(e) => {
                report
                    .failed
                    .push((format!("{address} {attr}"), e.to_string()));
                now.extend(before.cloned());
                continue;
            }
        };
        if current == *value {
            now.extend(before.cloned());
            continue;
        }
        match exec.write_file(&path, value.as_bytes()) {
            Ok(()) => {
                report.written.push(format!("{address} {attr}={value}"));
                now.push(before.cloned().unwrap_or(Saved {
                    address: address.clone(),
                    attr: attr.clone(),
                    previous: current,
                }));
            }
            Err(e) => {
                report
                    .failed
                    .push((format!("{address} {attr}"), e.to_string()));
                now.extend(before.cloned());
            }
        }
    }
    for s in saved {
        if want.contains_key(&(s.address.clone(), s.attr.clone())) {
            continue;
        }
        // Gone from the bus: the kernel gives a device that comes back
        // its defaults.
        if !exec.exists(&dev_dir(&s.address)) {
            continue;
        }
        let path = dev_dir(&s.address).join(&s.attr);
        match exec.write_file(&path, s.previous.as_bytes()) {
            Ok(()) => report
                .restored
                .push(format!("{} {}={}", s.address, s.attr, s.previous)),
            Err(e) => {
                report.failed.push((s.key(), e.to_string()));
                now.push(s.clone());
            }
        }
    }
    (report, now)
}

/// Refuse anything but PCI addresses: they become sysfs paths.
pub fn checked(addresses: &[String]) -> Result<Vec<String>, OvsError> {
    let mut out: Vec<String> = addresses
        .iter()
        .map(|a| {
            let a = a.to_ascii_lowercase();
            nic::validate_bdf(&a).map(|_| a)
        })
        .collect::<Result<_, _>>()?;
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::RecordingExec;

    const GPU: &str = "0000:43:00.0";
    const GPU2: &str = "0000:44:00.0";

    /// A root port, a switch (upstream and downstream ports) and two GPUs
    /// under it, each attribute at the kernel's default.
    fn host() -> RecordingExec {
        let exec = RecordingExec::new();
        let up = "../../../devices/pci0000:40/0000:40:01.1/0000:41:00.0/0000:42:00.0";
        for (bdf, link) in [
            (GPU, format!("{up}/{GPU}")),
            (GPU2, format!("{up}/{GPU2}")),
            ("0000:42:00.0", up.to_string()),
            (
                "0000:41:00.0",
                "../../../devices/pci0000:40/0000:40:01.1/0000:41:00.0".into(),
            ),
            (
                "0000:40:01.1",
                "../../../devices/pci0000:40/0000:40:01.1".into(),
            ),
        ] {
            let d = format!("/sys/bus/pci/devices/{bdf}");
            exec.file(&d, "");
            exec.link(&d, &link);
            exec.file(&format!("{d}/{CONTROL}"), "auto\n");
            exec.file(&format!("{d}/{D3COLD}"), "1\n");
        }
        exec
    }

    fn written(exec: &RecordingExec) -> Vec<String> {
        exec.writes()
            .into_iter()
            .map(|(p, v)| {
                format!(
                    "{}={}",
                    p.display()
                        .to_string()
                        .trim_start_matches("/sys/bus/pci/devices/"),
                    String::from_utf8_lossy(&v)
                )
            })
            .collect()
    }

    #[test]
    fn bridges_are_listed_from_the_nearest_to_the_root_port() {
        assert_eq!(
            bridges_above(&host(), GPU),
            ["0000:42:00.0", "0000:41:00.0", "0000:40:01.1"]
        );
        assert!(bridges_above(&host(), "0000:99:00.0").is_empty());
    }

    #[test]
    fn a_device_and_its_bridges_are_kept_on_and_recorded_once() {
        let exec = host();
        let (r, saved) = keep_on(&exec, &[GPU.into()], &[]);
        assert!(r.failed.is_empty(), "{r:?}");
        assert_eq!(
            written(&exec),
            [
                "0000:40:01.1/d3cold_allowed=0",
                "0000:41:00.0/d3cold_allowed=0",
                "0000:42:00.0/d3cold_allowed=0",
                "0000:43:00.0/d3cold_allowed=0",
                "0000:43:00.0/power/control=on",
            ]
        );
        assert_eq!(saved.len(), 5);
        assert!(
            saved
                .iter()
                .any(|s| s.address == GPU && s.attr == CONTROL && s.previous == "auto")
        );
        // A second call writes nothing and keeps the records.
        let before = exec.writes().len();
        let (r2, again) = keep_on(&exec, &[GPU.into()], &saved);
        assert_eq!(exec.writes().len(), before, "{r2:?}");
        assert_eq!(again, saved);
    }

    #[test]
    fn a_value_already_as_wanted_is_not_recorded() {
        let exec = host();
        exec.file(&format!("/sys/bus/pci/devices/{GPU}/{CONTROL}"), "on\n");
        let (_, saved) = keep_on(&exec, &[GPU.into()], &[]);
        assert!(!saved.iter().any(|s| s.address == GPU && s.attr == CONTROL));
        // Nothing to put back for it when it leaves.
        let (r, left) = keep_on(&exec, &[], &saved);
        assert!(left.is_empty());
        assert!(
            !r.restored.iter().any(|w| w.contains("power/control")),
            "{r:?}"
        );
    }

    #[test]
    fn a_device_leaving_gets_its_values_back_and_shared_bridges_wait() {
        let exec = host();
        let (_, saved) = keep_on(&exec, &[GPU.into(), GPU2.into()], &[]);
        let before = exec.writes().len();
        let (r, saved) = keep_on(&exec, &[GPU2.into()], &saved);
        assert_eq!(
            r.restored,
            [
                "0000:43:00.0 d3cold_allowed=1",
                "0000:43:00.0 power/control=auto"
            ]
        );
        assert_eq!(
            exec.writes().len(),
            before + 2,
            "the bridges stay: GPU2 is below them"
        );
        let (r, saved) = keep_on(&exec, &[], &saved);
        assert_eq!(r.restored.len(), 5, "{r:?}");
        assert!(saved.is_empty());
        let st = |bdf: &str, a: &str| {
            exec.read_file(Path::new(&format!("/sys/bus/pci/devices/{bdf}/{a}")))
                .unwrap()
        };
        assert_eq!(st("0000:40:01.1", D3COLD), "1");
        assert_eq!(st(GPU2, CONTROL), "auto");
    }

    #[test]
    fn a_device_off_the_bus_is_reported_and_its_records_dropped() {
        let exec = host();
        let (_, saved) = keep_on(&exec, &[GPU.into()], &[]);
        let (r, now) = keep_on(&exec, &["0000:99:00.0".into()], &saved);
        assert_eq!(r.failed, [("0000:99:00.0".into(), "not on the bus".into())]);
        assert_eq!(r.restored.len(), 5);
        assert!(now.is_empty());
    }

    #[test]
    fn only_pci_addresses_are_taken() {
        assert_eq!(
            checked(&["0000:43:00.0".into(), "0000:43:00.0".into()]).unwrap(),
            ["0000:43:00.0"]
        );
        assert!(checked(&["../../../etc".into()]).is_err());
    }
}
