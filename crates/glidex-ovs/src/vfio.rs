//! Preparing a PCI device for VM passthrough (spec/22-host-topology.md §5.3):
//! its whole IOMMU group bound to `vfio-pci`, the group device given to the
//! VMs' group, and back again. Runs in glidex-netd (root).

use crate::OvsError;
use crate::exec::{Cmd, Exec, Program};
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
    /// GPU windows (resizable BARs) made smaller, for `release` to put back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resized: Vec<Resized>,
}

/// A BAR `prepare` resized (spec/27-gpu-vms.md G17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resized {
    pub address: String,
    /// The BAR's number (`resource<N>_resize`).
    pub bar: u8,
    pub previous_mib: u64,
    pub mib: u64,
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

/// The functions of IOMMU group `group` a VM takes with it when it is
/// given `bdf`: `bdf` itself, whatever its class (an NVSwitch is class
/// `0x0680`, a bridge class), and the others but bridges and other
/// platform functions (class `0x06…`), which stay with the host.
pub fn group_members(exec: &dyn Exec, group: &str, bdf: &str) -> Vec<String> {
    exec.list_dir(
        &Path::new("/sys/kernel/iommu_groups")
            .join(group)
            .join("devices"),
    )
    .into_iter()
    .filter(|a| {
        if a.eq_ignore_ascii_case(bdf) {
            return true;
        }
        let class = exec
            .read_file(&dev_dir(a).join("class"))
            .unwrap_or_default();
        !class.trim().trim_start_matches("0x").starts_with("06")
    })
    .collect()
}

/// A process that has an IOMMU group's `/dev/vfio/<group>` open: a VM
/// (glidex's, libvirt's, anyone's) holding its devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub pid: u32,
    pub uid: u32,
    pub comm: String,
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (pid {})", self.comm, self.pid)
    }
}

/// Every process with one of `groups` open, by group. Read from each
/// process's open files, not by matching `/dev/vfio/<group>` here: libvirt
/// gives QEMU a private `/dev`, where the same path is another inode, so
/// `fuser` misses it. Needs root to read other users' processes.
pub fn group_holders(
    exec: &dyn Exec,
    groups: &[String],
) -> std::collections::BTreeMap<String, Vec<Holder>> {
    let mut out: std::collections::BTreeMap<String, Vec<Holder>> = Default::default();
    if groups.is_empty() {
        return out;
    }
    for pid in exec.list_dir(Path::new("/proc")) {
        let Ok(n) = pid.parse::<u32>() else { continue };
        let dir = Path::new("/proc").join(&pid);
        let mut held: Vec<&String> = Vec::new();
        for fd in exec.list_dir(&dir.join("fd")) {
            if let Some(target) = exec.read_link(&dir.join("fd").join(&fd))
                && let Some(g) = target.strip_prefix("/dev/vfio/")
                && let Some(g) = groups.iter().find(|x| *x == g)
                && !held.contains(&g)
            {
                held.push(g);
            }
        }
        if held.is_empty() {
            continue;
        }
        let status = exec.read_file(&dir.join("status")).unwrap_or_default();
        let uid = status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(u32::MAX);
        let comm = exec
            .read_file(&dir.join("comm"))
            .map(|c| c.trim().to_string())
            .unwrap_or_default();
        for g in held {
            out.entry(g.clone()).or_default().push(Holder {
                pid: n,
                uid,
                comm: comm.clone(),
            });
        }
    }
    out
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
            group_members(exec, &group, bdf).into_iter().find_map(|m| {
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

/// The host's memory in MiB, from `/proc/meminfo`.
fn mem_total_mib(exec: &dyn Exec) -> Option<u64> {
    exec.read_file(Path::new("/proc/meminfo"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kib| kib / 1024)
}

/// A display device's resizable BARs: `(bar, current MiB, supported
/// sizes as a bitmask, bit n = 2^n MiB)`.
fn resizable_bars(exec: &dyn Exec, bdf: &str) -> Vec<(u8, u64, u64)> {
    let dir = dev_dir(bdf);
    let class = exec.read_file(&dir.join("class")).unwrap_or_default();
    if !class.trim().trim_start_matches("0x").starts_with("03") {
        return Vec::new();
    }
    let ranges = exec.read_file(&dir.join("resource")).unwrap_or_default();
    let ranges: Vec<&str> = ranges.lines().collect();
    (0u8..6)
        .filter_map(|bar| {
            let mask = exec
                .read_file(&dir.join(format!("resource{bar}_resize")))
                .ok()?;
            let mask = u64::from_str_radix(mask.trim().trim_start_matches("0x"), 16).ok()?;
            let mut f = ranges.get(usize::from(bar))?.split_whitespace();
            let hex = |v: &str| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok();
            let (start, end) = (hex(f.next()?)?, hex(f.next()?)?);
            (start != 0 && end > start).then(|| (bar, (end - start + 1) >> 20, mask))
        })
        .collect()
}

/// The resize G17 asks of `bdf`'s windows: the total of the host's GPU
/// windows on `vfio-pci` stays below its memory, each window as large as
/// that allows. Errs when even the smallest size doesn't fit.
fn window_resizes(exec: &dyn Exec, bdf: &str) -> Result<Vec<Resized>, OvsError> {
    let bars = resizable_bars(exec, bdf);
    if bars.is_empty() {
        return Ok(Vec::new());
    }
    let Some(mem) = mem_total_mib(exec) else {
        return Ok(Vec::new());
    };
    let mut used: u64 = exec
        .list_dir(Path::new("/sys/bus/pci/devices"))
        .into_iter()
        .filter(|a| !a.eq_ignore_ascii_case(bdf))
        .filter(|a| nic::pci_driver(exec, a).ok().flatten().as_deref() == Some("vfio-pci"))
        .flat_map(|a| resizable_bars(exec, &a))
        .map(|(_, mib, _)| mib)
        .sum();
    let mut out = Vec::new();
    for (bar, mib, mask) in bars {
        if used + mib < mem {
            used += mib;
            continue;
        }
        let fit = (0..64u32)
            .rev()
            .filter(|n| mask & (1 << n) != 0)
            .map(|n| 1u64 << n)
            .find(|size| used + size < mem)
            .ok_or_else(|| {
                OvsError::conflict(format!(
                    "{bdf}'s BAR {bar} has no size that keeps the host's GPU windows \
                     ({used} MiB on vfio-pci) below its memory ({mem} MiB)"
                ))
            })?;
        used += fit;
        out.push(Resized {
            address: bdf.to_string(),
            bar,
            previous_mib: mib,
            mib: fit,
        });
    }
    Ok(out)
}

/// Set a BAR's size; the device must have no driver.
fn write_bar_size(exec: &dyn Exec, bdf: &str, bar: u8, mib: u64) -> Result<(), OvsError> {
    exec.write_file(
        &dev_dir(bdf).join(format!("resource{bar}_resize")),
        mib.trailing_zeros().to_string().as_bytes(),
    )
}

/// Bind `bdf` to `vfio-pci`, resizing its BARs while it has no driver.
fn bind_resized(exec: &dyn Exec, bdf: &str, resizes: &[Resized]) -> Result<(), OvsError> {
    exec.write_file(&dev_dir(bdf).join("driver_override"), b"vfio-pci")?;
    if nic::pci_driver(exec, bdf)?.is_some() {
        exec.write_file(&dev_dir(bdf).join("driver/unbind"), bdf.as_bytes())?;
    }
    for r in resizes {
        write_bar_size(exec, bdf, r.bar, r.mib)?;
    }
    exec.write_file(Path::new("/sys/bus/pci/drivers_probe"), bdf.as_bytes())
}

/// Load `vfio-pci` if its driver isn't there: without it a bind leaves
/// the device with no driver at all.
fn load_vfio_pci(exec: &dyn Exec) -> Result<(), OvsError> {
    let driver = Path::new("/sys/bus/pci/drivers/vfio-pci");
    if exec.exists(driver) {
        return Ok(());
    }
    exec.check(&Cmd::new(Program::Modprobe, ["vfio-pci"]))?;
    if exec.exists(driver) {
        Ok(())
    } else {
        Err(OvsError::Io(
            "the vfio-pci driver isn't there after modprobe vfio-pci".into(),
        ))
    }
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
    load_vfio_pci(exec)?;
    let mut members = Vec::new();
    let mut resized = Vec::new();
    for m in group_members(exec, &group, bdf) {
        let driver = nic::pci_driver(exec, &m)?;
        // A device already on vfio-pci keeps its size: a VM may hold it.
        if driver.as_deref() != Some("vfio-pci") {
            let r = window_resizes(exec, &m)?;
            if r.is_empty() {
                nic::bind_driver(exec, &m, "vfio-pci")?;
            } else {
                bind_resized(exec, &m, &r)?;
            }
            resized.extend(r);
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
        resized,
    })
}

/// Undo `prepare`: each function back to the driver it had (the kernel's
/// choice again), and the udev rule gone.
pub fn release(exec: &dyn Exec, p: &Prepared) -> Result<(), OvsError> {
    for m in &p.members {
        let sizes: Vec<&Resized> = p
            .resized
            .iter()
            .filter(|r| r.address == m.address)
            .collect();
        if sizes.is_empty() {
            if m.previous_driver.is_some() {
                nic::restore_driver(exec, &m.address)?;
            }
            continue;
        }
        // The window back to its size, between unbinding and the probe.
        let dir = dev_dir(&m.address);
        if m.previous_driver.is_some() {
            exec.write_file(&dir.join("driver_override"), b"\n")?;
        }
        if nic::pci_driver(exec, &m.address)?.is_some() {
            exec.write_file(&dir.join("driver/unbind"), m.address.as_bytes())?;
        }
        for r in sizes {
            write_bar_size(exec, &m.address, r.bar, r.previous_mib)?;
        }
        exec.write_file(
            Path::new("/sys/bus/pci/drivers_probe"),
            m.address.as_bytes(),
        )?;
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
        exec.file("/sys/bus/pci/drivers/vfio-pci", "");
        exec
    }

    /// gpu-dev2: 30.5 GiB of memory and two R9700s on amdgpu, each alone
    /// in its group, BAR 0 resizable from 256 MiB to 32 GiB and at 32 GiB.
    fn gpu_host() -> RecordingExec {
        let exec = RecordingExec::new();
        exec.file(
            "/proc/meminfo",
            "MemTotal:       32044516 kB\nMemFree: 1 kB\n",
        );
        exec.file("/sys/bus/pci/drivers/vfio-pci", "");
        for (bdf, group) in [("0000:03:00.0", "39"), ("0000:43:00.0", "53")] {
            let dir = format!("/sys/bus/pci/devices/{bdf}");
            exec.file(&dir, "");
            exec.file(&format!("{dir}/class"), "0x030000\n");
            exec.file(&format!("{dir}/uevent"), "DRIVER=amdgpu\n");
            exec.file(
                &format!("{dir}/resource"),
                "0x0000005000000000 0x00000057ffffffff 0x000000000014220c\n\
                 0x00000000f6000000 0x00000000f60fffff 0x0000000000040200\n",
            );
            exec.file(&format!("{dir}/resource0_resize"), "000000000000ff00\n");
            exec.link(&format!("{dir}/iommu_group"), group);
            exec.link(
                &format!("/sys/kernel/iommu_groups/{group}/devices/{bdf}"),
                bdf,
            );
        }
        exec
    }

    /// What was written to `path`, in order.
    fn written(exec: &RecordingExec, path: &str) -> Vec<String> {
        exec.writes()
            .into_iter()
            .filter(|(p, _)| p == Path::new(path))
            .map(|(_, d)| String::from_utf8_lossy(&d).into_owned())
            .collect()
    }

    #[test]
    fn a_gpu_window_is_kept_below_the_host_s_memory() {
        let exec = gpu_host();
        let a = "/sys/bus/pci/devices/0000:03:00.0";
        let p = prepare(&exec, "0000:03:00.0", &[], "glidex").unwrap();
        assert_eq!(
            p.resized,
            vec![Resized {
                address: "0000:03:00.0".into(),
                bar: 0,
                previous_mib: 32768,
                mib: 16384,
            }],
            "32 GiB isn't below 30.5 GiB; 16 GiB is"
        );
        // Resized with no driver: after the unbind, before the probe.
        let order: Vec<String> = exec
            .writes()
            .into_iter()
            .map(|(p, _)| p.display().to_string())
            .filter(|p| p.starts_with(a) || p.ends_with("drivers_probe"))
            .collect();
        assert_eq!(
            order,
            [
                format!("{a}/driver_override"),
                format!("{a}/driver/unbind"),
                format!("{a}/resource0_resize"),
                "/sys/bus/pci/drivers_probe".to_string(),
            ]
        );
        assert_eq!(written(&exec, &format!("{a}/resource0_resize")), ["14"]);

        // The second GPU counts the first's 16 GiB: 8 GiB is what fits.
        exec.file(&format!("{a}/uevent"), "DRIVER=vfio-pci\n");
        exec.file(
            &format!("{a}/resource"),
            "0x0000005000000000 0x00000053ffffffff 0x000000000014220c\n",
        );
        let q = prepare(&exec, "0000:43:00.0", &[], "glidex").unwrap();
        assert_eq!(q.resized[0].mib, 8192);

        // Release puts the size back while the device has no driver.
        exec.clear_calls();
        release(&exec, &p).unwrap();
        assert_eq!(
            written(&exec, &format!("{a}/resource0_resize")),
            ["14", "15"]
        );
        assert_eq!(
            written(&exec, &format!("{a}/driver_override"))
                .last()
                .unwrap(),
            "\n"
        );
    }

    #[test]
    fn a_gpu_already_on_vfio_pci_keeps_its_window() {
        let exec = gpu_host();
        exec.file(
            "/sys/bus/pci/devices/0000:03:00.0/uevent",
            "DRIVER=vfio-pci\n",
        );
        let p = prepare(&exec, "0000:03:00.0", &[], "glidex").unwrap();
        assert!(p.resized.is_empty(), "a VM may hold it");
        assert!(written(&exec, "/sys/bus/pci/devices/0000:03:00.0/resource0_resize").is_empty());
    }

    #[test]
    fn a_window_with_no_size_that_fits_is_refused() {
        let exec = gpu_host();
        exec.file("/proc/meminfo", "MemTotal: 204800 kB\n");
        let e = prepare(&exec, "0000:03:00.0", &[], "glidex").unwrap_err();
        assert!(e.to_string().contains("below its memory (200 MiB)"), "{e}");
        assert!(written(&exec, "/sys/bus/pci/devices/0000:03:00.0/driver_override").is_empty());
    }

    #[test]
    fn vfio_pci_is_loaded_before_anything_is_bound() {
        let exec = gpu_host();
        let bare = RecordingExec::new();
        for (p, c) in [
            ("/sys/bus/pci/devices/0000:03:00.0", ""),
            ("/sys/bus/pci/devices/0000:03:00.0/class", "0x030000"),
            (
                "/sys/bus/pci/devices/0000:03:00.0/uevent",
                "DRIVER=amdgpu\n",
            ),
        ] {
            bare.file(p, c);
        }
        bare.link("/sys/bus/pci/devices/0000:03:00.0/iommu_group", "39");
        bare.link(
            "/sys/kernel/iommu_groups/39/devices/0000:03:00.0",
            "0000:03:00.0",
        );
        // The driver doesn't appear: an error, not a device left driverless.
        let e = prepare(&bare, "0000:03:00.0", &[], "glidex").unwrap_err();
        assert!(e.to_string().contains("vfio-pci driver isn't there"), "{e}");
        assert!(bare.calls().contains(&"modprobe vfio-pci".to_string()));
        assert!(bare.writes().is_empty());
        // Loaded already: no modprobe.
        prepare(&exec, "0000:03:00.0", &[], "glidex").unwrap();
        assert!(!exec.calls().iter().any(|c| c.starts_with("modprobe")));
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
    fn holders_are_found_by_each_process_s_open_files() {
        let exec = host();
        // libvirt's QEMU (another uid, a private /dev: only its fds tell).
        exec.file(
            "/proc/37226/status",
            "Name:\tqemu-kvm-one\nUid:\t9869\t9869\t9869\t9869\n",
        );
        exec.file("/proc/37226/comm", "qemu-kvm-one\n");
        exec.link("/proc/37226/fd/3", "/dev/null");
        exec.link("/proc/37226/fd/30", "/dev/vfio/30");
        exec.link("/proc/37226/fd/31", "/dev/vfio/30");
        exec.link("/proc/37226/fd/32", "/dev/vfio/vfio");
        // A process holding nothing of interest.
        exec.file("/proc/1/status", "Uid:\t0\t0\t0\t0\n");
        exec.link("/proc/1/fd/0", "/dev/vfio/99");
        exec.file("/proc/self/status", "");
        let h = group_holders(&exec, &["30".into(), "40".into()]);
        assert_eq!(
            h.get("30").unwrap(),
            &vec![Holder {
                pid: 37226,
                uid: 9869,
                comm: "qemu-kvm-one".into()
            }],
            "once, however many fds"
        );
        assert!(!h.contains_key("40") && !h.contains_key("99"));
        assert!(group_holders(&exec, &[]).is_empty());
        assert_eq!(h["30"][0].to_string(), "qemu-kvm-one (pid 37226)");
    }

    #[test]
    fn an_nvswitch_is_prepared_though_its_class_is_a_bridge_s() {
        // Class 0x0680 ("other bridge"), alone in its group: preparing it
        // binds it, where skipping every 0x06 class bound nothing.
        let exec = RecordingExec::new();
        let bdf = "0000:c4:00.0";
        exec.file(&format!("/sys/bus/pci/devices/{bdf}"), "");
        exec.file(&format!("/sys/bus/pci/devices/{bdf}/class"), "0x068000");
        exec.file(
            &format!("/sys/bus/pci/devices/{bdf}/uevent"),
            "DRIVER=nvidia-nvswitch\n",
        );
        exec.link(&format!("/sys/bus/pci/devices/{bdf}/iommu_group"), "178");
        exec.link(&format!("/sys/kernel/iommu_groups/178/devices/{bdf}"), bdf);
        exec.file("/sys/bus/pci/drivers/vfio-pci", "");
        let p = prepare(&exec, bdf, &[], "glidex").unwrap();
        assert_eq!(
            p.members,
            vec![Member {
                address: bdf.into(),
                previous_driver: Some("nvidia-nvswitch".into()),
            }]
        );
        // Asked for something else in the group, a 0x06 function stays.
        let host = host();
        assert_eq!(
            group_members(&host, "30", "0000:07:00.0"),
            ["0000:07:00.0", "0000:07:00.1"],
            "the root port (0x0604) stays with the host"
        );
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
