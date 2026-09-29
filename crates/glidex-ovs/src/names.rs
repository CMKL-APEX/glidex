//! Deterministic names for VM NICs and validation of host names.

use crate::OvsError;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Linux `IFNAMSIZ` minus the terminating NUL.
pub const MAX_IFNAME: usize = 15;
/// Maximum NICs per VM.
pub const MAX_NICS: u8 = 8;
/// Where Cloud Hypervisor creates vhost-user sockets (server mode).
pub const VHOST_DIR: &str = "/run/glidex/vhost";

/// Bridge / uplink / network names: `[a-z0-9-]`, not starting with `-`.
pub fn validate_name(kind: &str, name: &str, max: usize) -> Result<(), OvsError> {
    let ok = !name.is_empty()
        && name.len() <= max
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(OvsError::invalid(format!(
            "{} name '{}' must be 1-{} chars of [a-z0-9-], not starting with '-'",
            kind, name, max
        )))
    }
}

/// Existing host interface names (uplinks) may use the usual kernel
/// charset, e.g. `enp2s0`, `eth0.100`.
pub fn validate_ifname(name: &str) -> Result<(), OvsError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_IFNAME
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(OvsError::invalid(format!("invalid interface name '{}'", name)))
    }
}

/// VM ids are UUIDs; accept only hex and dashes so they are safe in
/// file names and OVSDB values.
pub fn validate_vm_id(vm_id: &str) -> Result<(), OvsError> {
    let hex: String = vm_id.chars().filter(|c| *c != '-').collect();
    if hex.len() >= 8 && hex.len() <= 32 && hex.chars().all(|c| c.is_ascii_hexdigit()) && vm_id.len() <= 36 {
        Ok(())
    } else {
        Err(OvsError::invalid(format!("invalid VM id '{}'", vm_id)))
    }
}

fn check_nic(nic: u8) -> Result<(), OvsError> {
    if nic < MAX_NICS {
        Ok(())
    } else {
        Err(OvsError::invalid(format!("NIC index must be < {}", MAX_NICS)))
    }
}

/// OVS port / tap name: `gx<first 8 hex of the VM id>-<nic>`.
pub fn port_name(vm_id: &str, nic: u8) -> Result<String, OvsError> {
    validate_vm_id(vm_id)?;
    check_nic(nic)?;
    let hex: String = vm_id
        .chars()
        .filter(|c| *c != '-')
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    Ok(format!("gx{}-{}", hex, nic))
}

/// Locally administered unicast MAC, stable for a VM id + NIC index.
pub fn mac_address(vm_id: &str, nic: u8) -> Result<String, OvsError> {
    validate_vm_id(vm_id)?;
    check_nic(nic)?;
    let digest = Sha256::new()
        .chain_update(vm_id.as_bytes())
        .chain_update([nic])
        .finalize();
    let mut mac = String::from("02");
    for b in &digest[..5] {
        mac.push_str(&format!(":{:02x}", b));
    }
    Ok(mac)
}

/// vhost-user socket path for a VM NIC.
pub fn vhost_socket(vm_id: &str, nic: u8) -> Result<PathBuf, OvsError> {
    validate_vm_id(vm_id)?;
    check_nic(nic)?;
    Ok(PathBuf::from(format!("{}/{}.net{}.sock", VHOST_DIR, vm_id, nic)))
}

/// `aa:bb:cc:dd:ee:ff`, unicast (low bit of the first octet clear).
pub fn validate_mac(mac: &str) -> Result<(), OvsError> {
    let octets: Vec<&str> = mac.split(':').collect();
    let well_formed = octets.len() == 6
        && octets
            .iter()
            .all(|o| o.len() == 2 && o.chars().all(|c| c.is_ascii_hexdigit()));
    let unicast = well_formed
        && u8::from_str_radix(octets[0], 16)
            .map(|b| b & 1 == 0)
            .unwrap_or(false);
    if unicast && mac != "00:00:00:00:00:00" {
        Ok(())
    } else {
        Err(OvsError::invalid(format!(
            "'{}' is not a unicast MAC address",
            mac
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VM: &str = "1a2b3c4d-5e6f-4a1b-9c2d-0123456789ab";

    #[test]
    fn port_names_fit_ifnamsiz() {
        for nic in 0..MAX_NICS {
            let name = port_name(VM, nic).unwrap();
            assert!(name.len() <= MAX_IFNAME, "{name}");
            assert_eq!(name, format!("gx1a2b3c4d-{nic}"));
        }
        assert!(port_name(VM, MAX_NICS).is_err());
        assert!(port_name("../../etc", 0).is_err());
    }

    #[test]
    fn macs_are_stable_local_unicast_and_distinct() {
        let a = mac_address(VM, 0).unwrap();
        assert_eq!(a, mac_address(VM, 0).unwrap());
        assert_ne!(a, mac_address(VM, 1).unwrap());
        assert!(a.starts_with("02:"));
        validate_mac(&a).unwrap();
    }

    #[test]
    fn socket_path_under_sun_path_limit() {
        let p = vhost_socket(VM, 7).unwrap();
        assert!(p.to_str().unwrap().len() < 108);
        assert!(p.starts_with(VHOST_DIR));
    }

    #[test]
    fn validates_names_and_macs() {
        validate_name("bridge", "gxbr-nat", MAX_IFNAME).unwrap();
        for bad in ["", "-x", "Br0", "a b", "x;rm", &"a".repeat(16)] {
            assert!(validate_name("bridge", bad, MAX_IFNAME).is_err(), "{bad:?}");
        }
        validate_ifname("enp2s0").unwrap();
        validate_ifname("eth0.100").unwrap();
        assert!(validate_ifname("../x").is_err());
        assert!(validate_mac("03:00:00:00:00:01").is_err(), "multicast");
        assert!(validate_mac("02:00:00:00:00").is_err());
    }
}
