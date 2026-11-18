//! ACS on the ports above passthrough devices (spec/22-host-topology.md
//! H30, §5.2). The kernel forms IOMMU groups at boot from the ACS it
//! enables then; a port whose ACS is turned off afterwards forwards a
//! device's requests to its neighbours without the IOMMU. Read only: glidex
//! never changes ACS. Runs in glidex-netd, as only root can read a device's
//! extended configuration space.

use crate::Exec;
use crate::power::bridges_above;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// The ACS extended capability.
const ACS_ID: u16 = 0x000d;
/// The PCI Express capability, in the standard capability list.
const PCIE_ID: u8 = 0x10;
/// Device/Port Type in the PCI Express Capabilities register.
const ROOT_PORT: u8 = 0x4;
const DOWNSTREAM_PORT: u8 = 0x6;

/// An ACS control a port must have on to keep the devices below it apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcsBit {
    SourceValidation,
    RequestRedirect,
    CompletionRedirect,
    UpstreamForwarding,
}

impl AcsBit {
    const ALL: [AcsBit; 4] = [
        AcsBit::SourceValidation,
        AcsBit::RequestRedirect,
        AcsBit::CompletionRedirect,
        AcsBit::UpstreamForwarding,
    ];

    /// Its bit in the ACS Capability and Control registers.
    fn mask(self) -> u16 {
        match self {
            AcsBit::SourceValidation => 1 << 0,
            AcsBit::RequestRedirect => 1 << 2,
            AcsBit::CompletionRedirect => 1 << 3,
            AcsBit::UpstreamForwarding => 1 << 4,
        }
    }

    /// As a warning says it.
    pub fn words(self) -> &'static str {
        match self {
            AcsBit::SourceValidation => "source validation",
            AcsBit::RequestRedirect => "request redirect",
            AcsBit::CompletionRedirect => "completion redirect",
            AcsBit::UpstreamForwarding => "upstream forwarding",
        }
    }
}

/// A port above a device whose ACS is off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortAcs {
    pub address: String,
    /// The controls the port supports that are off.
    pub off: Vec<AcsBit>,
}

/// What [`ports_off`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcsReport {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub off: Vec<PortAcs>,
    /// A port, and why its ACS couldn't be read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<(String, String)>,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The PCI Express Device/Port Type of a function, from its standard
/// capability list; `None` for a function that isn't PCI Express.
pub fn port_type(config: &[u8]) -> Option<u8> {
    // Status: Capabilities List.
    if u16_at(config, 0x06)? & (1 << 4) == 0 {
        return None;
    }
    let mut at = usize::from(*config.get(0x34)? & 0xfc);
    // 48 capabilities fill the 192 bytes after the header: more is a loop.
    for _ in 0..48 {
        if at < 0x40 {
            return None;
        }
        let id = *config.get(at)?;
        if id == PCIE_ID {
            return Some(((u16_at(config, at + 2)? >> 4) & 0xf) as u8);
        }
        at = usize::from(*config.get(at + 1)? & 0xfc);
    }
    None
}

/// The offset of extended capability `id` in a function's configuration
/// space, walking the list from 0x100.
fn extended_capability(config: &[u8], id: u16) -> Option<usize> {
    let mut at = 0x100;
    // (4096 - 256) / 4 headers at most: more is a loop.
    for _ in 0..960 {
        let header = u32_at(config, at)?;
        if header == 0 || header == 0xffff_ffff {
            return None;
        }
        if header & 0xffff == u32::from(id) {
            return Some(at);
        }
        at = ((header >> 20) & 0xffc) as usize;
        if at < 0x100 {
            return None;
        }
    }
    None
}

/// The ACS controls this port supports that are off, from its whole
/// configuration space. `None` when it has no ACS capability (or its
/// extended configuration space isn't there to read).
pub fn acs_off(config: &[u8]) -> Option<Vec<AcsBit>> {
    let at = extended_capability(config, ACS_ID)?;
    let cap = u16_at(config, at + 4)?;
    let ctrl = u16_at(config, at + 6)?;
    Some(
        AcsBit::ALL
            .into_iter()
            .filter(|b| cap & b.mask() != 0 && ctrl & b.mask() == 0)
            .collect(),
    )
}

/// The root ports and switch downstream ports above `addresses` (checked
/// PCI addresses) whose ACS is off.
pub fn ports_off(exec: &dyn Exec, addresses: &[String]) -> AcsReport {
    let ports: BTreeSet<String> = addresses
        .iter()
        .flat_map(|a| bridges_above(exec, a))
        .collect();
    let mut report = AcsReport::default();
    for p in ports {
        let path = Path::new("/sys/bus/pci/devices").join(&p).join("config");
        let config = match exec.read_bytes(&path) {
            Ok(c) => c,
            Err(e) => {
                report.failed.push((p, e.to_string()));
                continue;
            }
        };
        if !matches!(port_type(&config), Some(ROOT_PORT | DOWNSTREAM_PORT)) {
            continue;
        }
        if config.len() <= 0x100 {
            report.failed.push((
                p,
                format!(
                    "only {} bytes of its configuration space are readable",
                    config.len()
                ),
            ));
            continue;
        }
        if let Some(off) = acs_off(&config)
            && !off.is_empty()
        {
            report.off.push(PortAcs { address: p, off });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RecordingExec;

    /// A PCI Express function's 4 KiB configuration space: a port of
    /// `kind`, with an AER capability first in the extended list, then ACS
    /// with `cap` and `ctrl` when given.
    fn config(kind: u8, acs: Option<(u16, u16)>) -> Vec<u8> {
        let mut c = vec![0u8; 4096];
        c[0x06] = 1 << 4;
        c[0x34] = 0x40;
        // Power management, then PCI Express.
        c[0x40] = 0x01;
        c[0x41] = 0x60;
        c[0x60] = PCIE_ID;
        c[0x62..0x64].copy_from_slice(&(u16::from(kind) << 4 | 2).to_le_bytes());
        // AER at 0x100, then ACS at 0x148 when there is one.
        let next: u32 = if acs.is_some() { 0x148 } else { 0 };
        c[0x100..0x104].copy_from_slice(&(0x0001 | 1 << 16 | next << 20).to_le_bytes());
        if let Some((cap, ctrl)) = acs {
            c[0x148..0x14c].copy_from_slice(&(u32::from(ACS_ID) | 1 << 16).to_le_bytes());
            c[0x14c..0x14e].copy_from_slice(&cap.to_le_bytes());
            c[0x14e..0x150].copy_from_slice(&ctrl.to_le_bytes());
        }
        c
    }

    #[test]
    fn controls_supported_and_off_are_listed() {
        // SV, TB, RR, CR, UF, EC, DT supported; only SV and UF on.
        let c = config(ROOT_PORT, Some((0x7f, 0x11)));
        assert_eq!(
            acs_off(&c).unwrap(),
            [AcsBit::RequestRedirect, AcsBit::CompletionRedirect]
        );
        // All four on (with Egress Control off: not required).
        let c = config(ROOT_PORT, Some((0x7f, 0x1d)));
        assert_eq!(acs_off(&c).unwrap(), []);
    }

    #[test]
    fn a_control_the_port_lacks_is_not_off() {
        // No Upstream Forwarding in the capability, none on.
        let c = config(DOWNSTREAM_PORT, Some((0x0d, 0x0d)));
        assert_eq!(acs_off(&c).unwrap(), []);
        let c = config(DOWNSTREAM_PORT, Some((0x0d, 0x00)));
        assert_eq!(
            acs_off(&c).unwrap(),
            [
                AcsBit::SourceValidation,
                AcsBit::RequestRedirect,
                AcsBit::CompletionRedirect
            ]
        );
    }

    #[test]
    fn no_acs_capability_or_no_extended_space() {
        assert_eq!(acs_off(&config(ROOT_PORT, None)), None);
        // What a user other than root reads: the first 64 bytes.
        let c = config(ROOT_PORT, Some((0x1d, 0)));
        assert_eq!(acs_off(&c[..64]), None);
        assert_eq!(acs_off(&[]), None);
    }

    #[test]
    fn a_looping_capability_list_ends() {
        let mut c = config(ROOT_PORT, None);
        // AER's next points at itself.
        c[0x100..0x104].copy_from_slice(&(0x0001u32 | 1 << 16 | 0x100 << 20).to_le_bytes());
        assert_eq!(acs_off(&c), None);
        // The standard list too: power management points at itself.
        c[0x41] = 0x40;
        assert_eq!(port_type(&c), None);
    }

    #[test]
    fn port_types_are_read_from_the_pcie_capability() {
        assert_eq!(port_type(&config(ROOT_PORT, None)), Some(ROOT_PORT));
        assert_eq!(port_type(&config(0x5, None)), Some(0x5));
        assert_eq!(port_type(&[0u8; 64]), None);
    }

    /// A root port, a switch (upstream and downstream ports) and a GPU.
    fn host(root: Vec<u8>, down: Vec<u8>) -> RecordingExec {
        let exec = RecordingExec::new();
        let up = "../../../devices/pci0000:40/0000:40:01.1/0000:41:00.0/0000:42:00.0";
        for (bdf, link, config) in [
            (
                "0000:43:00.0",
                format!("{up}/0000:43:00.0"),
                config(0x0, None),
            ),
            ("0000:42:00.0", up.to_string(), down),
            (
                "0000:41:00.0",
                "../../../devices/pci0000:40/0000:40:01.1/0000:41:00.0".into(),
                // An upstream port with ACS off is not looked at.
                config(0x5, Some((0x1d, 0))),
            ),
            (
                "0000:40:01.1",
                "../../../devices/pci0000:40/0000:40:01.1".into(),
                root,
            ),
        ] {
            let d = format!("/sys/bus/pci/devices/{bdf}");
            exec.file(&d, "");
            exec.link(&d, &link);
            exec.bytes(&format!("{d}/config"), &config);
        }
        exec
    }

    #[test]
    fn root_and_downstream_ports_above_are_read() {
        let exec = host(
            config(ROOT_PORT, Some((0x1d, 0x1d))),
            config(DOWNSTREAM_PORT, Some((0x1d, 0x11))),
        );
        let r = ports_off(&exec, &["0000:43:00.0".into()]);
        assert_eq!(
            r,
            AcsReport {
                off: vec![PortAcs {
                    address: "0000:42:00.0".into(),
                    off: vec![AcsBit::RequestRedirect, AcsBit::CompletionRedirect],
                }],
                failed: vec![],
            }
        );
        // All on: nothing.
        let exec = host(
            config(ROOT_PORT, Some((0x1d, 0x1d))),
            config(DOWNSTREAM_PORT, Some((0x1d, 0x1d))),
        );
        assert_eq!(
            ports_off(&exec, &["0000:43:00.0".into()]),
            AcsReport::default()
        );
    }

    #[test]
    fn a_port_read_short_is_reported() {
        let exec = host(
            config(ROOT_PORT, Some((0x1d, 0)))[..256].to_vec(),
            config(DOWNSTREAM_PORT, None),
        );
        let r = ports_off(&exec, &["0000:43:00.0".into()]);
        assert!(r.off.is_empty());
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert_eq!(r.failed[0].0, "0000:40:01.1");
    }
}
