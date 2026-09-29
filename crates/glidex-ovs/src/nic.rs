//! Host NICs: classification before use as an uplink (spec §8.4) and PCI
//! driver binding for DPDK (spec §8.3).

use crate::exec::{Cmd, Exec, Program};
use crate::names::validate_ifname;
use crate::OvsError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// Why a NIC is considered in use by the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Classification {
    pub reasons: Vec<String>,
    pub addresses: Vec<String>,
}

impl Classification {
    pub fn in_use(&self) -> bool {
        !self.reasons.is_empty()
    }
}

/// Global addresses (`addr/prefix`) on `ifname`.
pub fn global_addresses(exec: &dyn Exec, ifname: &str) -> Result<Vec<(IpAddr, u8)>, OvsError> {
    let out = exec.check(&Cmd::new(Program::Ip, ["-j", "addr", "show", "dev", ifname]))?;
    let links: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    Ok(links
        .iter()
        .flat_map(|l| l.get("addr_info").and_then(Value::as_array).cloned().unwrap_or_default())
        .filter(|a| a.get("scope").and_then(Value::as_str) == Some("global"))
        .filter_map(|a| {
            let ip: IpAddr = a.get("local")?.as_str()?.parse().ok()?;
            let prefix = a.get("prefixlen")?.as_u64()? as u8;
            Some((ip, prefix))
        })
        .collect())
}

fn default_route_dev(exec: &dyn Exec) -> Vec<String> {
    let Ok(out) = exec.check(&Cmd::new(Program::Ip, ["-j", "route", "show", "default"])) else {
        return Vec::new();
    };
    let routes: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    routes
        .iter()
        .filter_map(|r| r.get("dev").and_then(Value::as_str).map(str::to_string))
        .collect()
}

/// Local addresses of established TCP connections (e.g. the SSH session
/// someone is using to run this).
fn established_local_addrs(exec: &dyn Exec) -> Vec<IpAddr> {
    let Ok(out) = exec.check(&Cmd::new(Program::Ss, ["-Htn", "state", "established"])) else {
        return Vec::new();
    };
    out.stdout_str()
        .lines()
        .filter_map(|line| {
            // Recv-Q Send-Q Local:Port Peer:Port [Process]
            let local = line.split_whitespace().nth(2)?;
            let host = local.rsplit_once(':')?.0;
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .split('%')
                .next()?
                .parse()
                .ok()
        })
        .collect()
}

pub fn ifindex(exec: &dyn Exec, ifname: &str) -> Option<u32> {
    exec.read_file(&Path::new("/sys/class/net").join(ifname).join("ifindex"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Is the NIC's address from DHCP (systemd-networkd keeps a lease file)?
fn dhcp_managed(exec: &dyn Exec, ifname: &str) -> bool {
    ifindex(exec, ifname)
        .is_some_and(|i| exec.exists(&PathBuf::from(format!("/run/systemd/netif/leases/{}", i))))
}

pub fn classify(exec: &dyn Exec, ifname: &str) -> Result<Classification, OvsError> {
    validate_ifname(ifname)?;
    if !exec.exists(&Path::new("/sys/class/net").join(ifname)) {
        return Err(OvsError::not_found(format!("interface '{}'", ifname)));
    }
    let addrs = global_addresses(exec, ifname)?;
    let mut c = Classification {
        addresses: addrs.iter().map(|(ip, p)| format!("{}/{}", ip, p)).collect(),
        ..Default::default()
    };
    if !addrs.is_empty() {
        c.reasons.push("has_addresses".into());
    }
    if default_route_dev(exec).iter().any(|d| d == ifname) {
        c.reasons.push("default_route".into());
    }
    let local: Vec<IpAddr> = addrs.iter().map(|(ip, _)| *ip).collect();
    if established_local_addrs(exec).iter().any(|ip| local.contains(ip)) {
        c.reasons.push("active_connections".into());
    }
    if dhcp_managed(exec, ifname) {
        c.reasons.push("dhcp_managed".into());
    }
    Ok(c)
}

// ---- PCI / DPDK ------------------------------------------------------------

/// `0000:41:00.0`.
pub fn validate_bdf(bdf: &str) -> Result<(), OvsError> {
    let parts: Vec<&str> = bdf.split(|c| c == ':' || c == '.').collect();
    let ok = parts.len() == 4
        && [4, 2, 2, 1]
            .iter()
            .zip(&parts)
            .all(|(n, p)| p.len() == *n && p.chars().all(|c| c.is_ascii_hexdigit()));
    if ok {
        Ok(())
    } else {
        Err(OvsError::invalid(format!("'{}' is not a PCI address (dddd:bb:dd.f)", bdf)))
    }
}

fn pci_dir(bdf: &str) -> PathBuf {
    Path::new("/sys/bus/pci/devices").join(bdf)
}

/// Current driver of a PCI device, if bound.
pub fn pci_driver(exec: &dyn Exec, bdf: &str) -> Result<Option<String>, OvsError> {
    validate_bdf(bdf)?;
    if !exec.exists(&pci_dir(bdf)) {
        return Err(OvsError::not_found(format!("PCI device {}", bdf)));
    }
    // `driver` is a symlink; its target's file name is the driver. Read it
    // via the `uevent` file, which works the same in tests.
    let uevent = exec.read_file(&pci_dir(bdf).join("uevent")).unwrap_or_default();
    Ok(uevent
        .lines()
        .find_map(|l| l.strip_prefix("DRIVER="))
        .map(str::to_string))
}

/// Kernel interfaces backed by the PCI device (its NIC names).
pub fn pci_netdevs(exec: &dyn Exec, bdf: &str) -> Vec<String> {
    exec.read_file(&pci_dir(bdf).join("net_names"))
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Rebind a PCI device to `driver` (e.g. `vfio-pci`), the way
/// `dpdk-devbind.py` does: driver_override, unbind, drivers_probe.
pub fn bind_driver(exec: &dyn Exec, bdf: &str, driver: &str) -> Result<(), OvsError> {
    validate_bdf(bdf)?;
    let dir = pci_dir(bdf);
    exec.write_file(&dir.join("driver_override"), driver.as_bytes())?;
    if pci_driver(exec, bdf)?.is_some() {
        exec.write_file(&dir.join("driver/unbind"), bdf.as_bytes())?;
    }
    exec.write_file(Path::new("/sys/bus/pci/drivers_probe"), bdf.as_bytes())?;
    Ok(())
}

/// Undo `bind_driver`: clear the override and re-probe, so the kernel
/// picks the device's normal driver again.
pub fn restore_driver(exec: &dyn Exec, bdf: &str) -> Result<(), OvsError> {
    validate_bdf(bdf)?;
    let dir = pci_dir(bdf);
    exec.write_file(&dir.join("driver_override"), b"\n")?;
    if pci_driver(exec, bdf)?.is_some() {
        exec.write_file(&dir.join("driver/unbind"), bdf.as_bytes())?;
    }
    exec.write_file(Path::new("/sys/bus/pci/drivers_probe"), bdf.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    fn nic(exec: &RecordingExec, addrs: &str) {
        exec.file("/sys/class/net/gxup0", "");
        exec.file("/sys/class/net/gxup0/ifindex", "7\n");
        exec.on("ip -j addr show dev gxup0", Output::ok(format!(r#"[{{"ifname":"gxup0","addr_info":[{addrs}]}}]"#)));
    }

    #[test]
    fn free_nic_has_no_reasons() {
        let exec = RecordingExec::new();
        nic(&exec, r#"{"family":"inet6","local":"fe80::1","prefixlen":64,"scope":"link"}"#);
        exec.on("ip -j route show default", Output::ok(r#"[{"dst":"default","gateway":"192.168.0.1","dev":"eth0"}]"#));
        let c = classify(&exec, "gxup0").unwrap();
        assert!(!c.in_use(), "{c:?}");
    }

    #[test]
    fn in_use_reasons() {
        let exec = RecordingExec::new();
        nic(&exec, r#"{"family":"inet","local":"192.0.2.10","prefixlen":24,"scope":"global"}"#);
        exec.on("ip -j route show default", Output::ok(r#"[{"dst":"default","gateway":"192.0.2.1","dev":"gxup0"}]"#));
        exec.on("ss -Htn state established", Output::ok("0 0 192.0.2.10:22 192.0.2.50:51234\n0 0 [::1]:8080 [::1]:40000\n"));
        exec.file("/run/systemd/netif/leases/7", "ADDRESS=192.0.2.10\n");
        let c = classify(&exec, "gxup0").unwrap();
        assert_eq!(c.reasons, ["has_addresses", "default_route", "active_connections", "dhcp_managed"]);
        assert_eq!(c.addresses, ["192.0.2.10/24"]);
    }

    #[test]
    fn missing_or_bad_nic() {
        let exec = RecordingExec::new();
        assert!(matches!(classify(&exec, "nope0"), Err(OvsError::NotFound { .. })));
        assert!(matches!(classify(&exec, "../x"), Err(OvsError::InvalidArgument { .. })));
    }

    #[test]
    fn dpdk_bind_and_restore_writes_sysfs() {
        let exec = RecordingExec::new();
        exec.file("/sys/bus/pci/devices/0000:41:00.0", "");
        exec.file("/sys/bus/pci/devices/0000:41:00.0/uevent", "DRIVER=ixgbe\nPCI_ID=8086:10FB\n");
        assert_eq!(pci_driver(&exec, "0000:41:00.0").unwrap().as_deref(), Some("ixgbe"));
        bind_driver(&exec, "0000:41:00.0", "vfio-pci").unwrap();
        let writes: Vec<(String, String)> = exec
            .writes()
            .into_iter()
            .map(|(p, d)| (p.display().to_string(), String::from_utf8(d).unwrap()))
            .collect();
        assert_eq!(
            writes,
            [
                ("/sys/bus/pci/devices/0000:41:00.0/driver_override".to_string(), "vfio-pci".to_string()),
                ("/sys/bus/pci/devices/0000:41:00.0/driver/unbind".to_string(), "0000:41:00.0".to_string()),
                ("/sys/bus/pci/drivers_probe".to_string(), "0000:41:00.0".to_string()),
            ]
        );
        restore_driver(&exec, "0000:41:00.0").unwrap();
        assert!(exec.writes().iter().any(|(p, d)| p.ends_with("driver_override") && d == b"\n"));
        assert!(validate_bdf("0000:41:00.0").is_ok());
        assert!(validate_bdf("41:00.0").is_err());
        assert!(validate_bdf("0000:41:00.0/../x").is_err());
    }
}
