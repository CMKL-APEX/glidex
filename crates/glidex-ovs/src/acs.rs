//! ACS on the ports above passthrough devices (spec/22-host-topology.md
//! H30, §5.2). The kernel forms IOMMU groups at boot from the ACS it
//! enables then; a port whose ACS is turned off afterwards forwards a
//! device's requests to its neighbours without the IOMMU. Also what
//! switch-local P2P needs (H31): ATS on the devices, and Direct Translated
//! P2P and Translation Blocking on the ports. Glidex reads these, and
//! changes one thing only: Direct Translated P2P on the ports above a
//! trusted pool's devices while one VM holds a pair (H33, H34,
//! [`set_direct_translated`]). Runs in glidex-netd, as only root can read
//! and write a device's extended configuration space.

use crate::Exec;
use crate::power::bridges_above;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// The ACS extended capability.
const ACS_ID: u16 = 0x000d;
/// The ATS extended capability.
const ATS_ID: u16 = 0x000f;
/// ACS Translation Blocking and Direct Translated P2P.
const ACS_TB: u16 = 1 << 1;
const ACS_DT: u16 = 1 << 6;
/// Enable, in the ATS Control register.
const ATS_ENABLE: u16 = 1 << 15;
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

/// Whether a function has a capability, and has it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Unsupported,
    Off,
    On,
}

impl Support {
    fn of(cap: bool, on: bool) -> Self {
        match (cap, on) {
            (false, _) => Support::Unsupported,
            (true, false) => Support::Off,
            (true, true) => Support::On,
        }
    }
}

/// What a port above a device does with translated requests (H31).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortP2p {
    pub address: String,
    pub direct_translated: Support,
    pub translation_blocking: bool,
}

/// A device's ATS (H31).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAts {
    pub address: String,
    pub ats: Support,
}

/// What [`ports_off`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcsReport {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub off: Vec<PortAcs>,
    /// Every root and downstream port read, ACS off or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortP2p>,
    /// The given devices whose configuration space was read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceAts>,
    /// A port or device, and why it couldn't be read.
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

/// A port's Direct Translated P2P and Translation Blocking; a port without
/// ACS has neither.
pub fn port_p2p(config: &[u8]) -> (Support, bool) {
    let Some(at) = extended_capability(config, ACS_ID) else {
        return (Support::Unsupported, false);
    };
    let cap = u16_at(config, at + 4).unwrap_or(0);
    let ctrl = u16_at(config, at + 6).unwrap_or(0);
    (
        Support::of(cap & ACS_DT != 0, ctrl & ACS_DT != 0),
        cap & ACS_TB != 0 && ctrl & ACS_TB != 0,
    )
}

/// A function's ATS, from its whole configuration space.
pub fn ats(config: &[u8]) -> Support {
    match extended_capability(config, ATS_ID).and_then(|at| u16_at(config, at + 6)) {
        Some(ctrl) => Support::of(true, ctrl & ATS_ENABLE != 0),
        None => Support::Unsupported,
    }
}

/// A port whose Direct Translated P2P netd turned on, and whether it was on
/// before (spec/22-host-topology.md §5.3b).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedDt {
    pub address: String,
    pub previous: bool,
}

/// What [`set_direct_translated`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DtReport {
    /// Ports it turned Direct Translated P2P on for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<String>,
    /// Ports given their earlier value back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restored: Vec<String>,
    /// A port, and why it was left alone or couldn't be done.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<(String, String)>,
}

/// Why `port` (its whole configuration space) mustn't get Direct
/// Translated P2P, or the offset of its ACS Control register.
fn dt_control(config: &[u8]) -> Result<usize, String> {
    if port_type(config) != Some(DOWNSTREAM_PORT) {
        return Err("not a switch downstream port".into());
    }
    let at = extended_capability(config, ACS_ID).ok_or("it has no ACS")?;
    let cap = u16_at(config, at + 4).ok_or("its ACS can't be read")?;
    if cap & ACS_DT == 0 {
        return Err("it doesn't support Direct Translated P2P".into());
    }
    // H30: with ACS off, requests already cross without the IOMMU.
    if let Some(off) = acs_off(config)
        && !off.is_empty()
    {
        return Err("its ACS is off".into());
    }
    Ok(at + 6)
}

/// Write `port`'s ACS Control with Direct Translated P2P `on`, every other
/// bit as read; whether it changed.
fn write_dt(
    exec: &dyn Exec,
    port: &str,
    config: &[u8],
    at: usize,
    on: bool,
) -> Result<bool, String> {
    let ctrl = u16_at(config, at).ok_or("its ACS Control can't be read")?;
    let want = if on { ctrl | ACS_DT } else { ctrl & !ACS_DT };
    if want == ctrl {
        return Ok(false);
    }
    let path = Path::new("/sys/bus/pci/devices").join(port).join("config");
    exec.write_bytes_at(&path, at as u64, &want.to_le_bytes())
        .map_err(|e| e.to_string())?;
    let back = read_config(exec, port).and_then(|c| u16_at(&c, at).ok_or("short".into()))?;
    if back & ACS_DT != want & ACS_DT {
        return Err(format!(
            "ACS Control reads {back:#06x} after writing {want:#06x}"
        ));
    }
    Ok(true)
}

/// spec/22-host-topology.md §5.3b: Direct Translated P2P on exactly
/// `ports` (checked PCI addresses of switch downstream ports), changing
/// that bit of ACS Control only. `saved` is what netd recorded before
/// earlier writes; a port that leaves the set gets its earlier value back.
/// A port with ACS off, without the capability, or not a downstream port
/// is left alone and reported. Returns what to keep recorded.
pub fn set_direct_translated(
    exec: &dyn Exec,
    ports: &[String],
    saved: &[SavedDt],
) -> (DtReport, Vec<SavedDt>) {
    let mut report = DtReport::default();
    let mut now = Vec::new();
    let wanted: BTreeSet<&String> = ports.iter().collect();
    for &p in &wanted {
        let prior = saved.iter().find(|s| &s.address == p);
        let done = read_config(exec, p).and_then(|c| {
            let at = dt_control(&c)?;
            let before = u16_at(&c, at).map(|v| v & ACS_DT != 0).unwrap_or(false);
            let changed = write_dt(exec, p, &c, at, true)?;
            Ok((before, changed))
        });
        match done {
            Ok((before, changed)) => {
                now.push(prior.cloned().unwrap_or(SavedDt {
                    address: p.clone(),
                    previous: before,
                }));
                if changed {
                    report.set.push(p.clone());
                }
            }
            Err(why) => {
                // Still ours to put back once it reads again.
                now.extend(prior.cloned());
                report.failed.push((p.clone(), why));
            }
        }
    }
    for s in saved.iter().filter(|s| !wanted.contains(&s.address)) {
        let done = read_config(exec, &s.address).and_then(|c| {
            let at = extended_capability(&c, ACS_ID).ok_or("it has no ACS")? + 6;
            write_dt(exec, &s.address, &c, at, s.previous)
        });
        match done {
            Ok(true) => report.restored.push(s.address.clone()),
            Ok(false) => {}
            Err(why) => {
                now.push(s.clone());
                report.failed.push((s.address.clone(), why));
            }
        }
    }
    (report, now)
}

/// A function's whole configuration space, or why it can't be had: only
/// root reads past the first 64 bytes.
fn read_config(exec: &dyn Exec, address: &str) -> Result<Vec<u8>, String> {
    let path = Path::new("/sys/bus/pci/devices")
        .join(address)
        .join("config");
    let config = exec.read_bytes(&path).map_err(|e| e.to_string())?;
    if config.len() <= 0x100 {
        return Err(format!(
            "only {} bytes of its configuration space are readable",
            config.len()
        ));
    }
    Ok(config)
}

/// The root ports and switch downstream ports above `addresses` (checked
/// PCI addresses): those whose ACS is off (H30), and what each does with
/// translated requests; and each device's ATS (H31).
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
            report.off.push(PortAcs {
                address: p.clone(),
                off,
            });
        }
        let (direct_translated, translation_blocking) = port_p2p(&config);
        report.ports.push(PortP2p {
            address: p,
            direct_translated,
            translation_blocking,
        });
    }
    for a in addresses.iter().collect::<BTreeSet<_>>() {
        match read_config(exec, a) {
            Ok(config) => report.devices.push(DeviceAts {
                address: a.clone(),
                ats: ats(&config),
            }),
            Err(why) => report.failed.push((a.clone(), why)),
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

    /// An endpoint with AER, then ATS with its Control register `ctrl`
    /// when given.
    fn endpoint(ats_ctrl: Option<u16>) -> Vec<u8> {
        let mut c = config(0x0, None);
        if let Some(ctrl) = ats_ctrl {
            c[0x100..0x104].copy_from_slice(&(0x0001u32 | 1 << 16 | 0x180 << 20).to_le_bytes());
            c[0x180..0x184].copy_from_slice(&(u32::from(ATS_ID) | 1 << 16).to_le_bytes());
            c[0x184..0x186].copy_from_slice(&0x0020u16.to_le_bytes());
            c[0x186..0x188].copy_from_slice(&ctrl.to_le_bytes());
        }
        c
    }

    #[test]
    fn ats_is_read_from_its_control_register() {
        assert_eq!(ats(&endpoint(None)), Support::Unsupported);
        assert_eq!(ats(&endpoint(Some(0))), Support::Off);
        // Enable, with a Smallest Translation Unit set.
        assert_eq!(ats(&endpoint(Some(0x8003))), Support::On);
        assert_eq!(ats(&endpoint(Some(0x8000))[..64]), Support::Unsupported);
    }

    #[test]
    fn direct_translated_and_translation_blocking() {
        // DT and TB supported, both on (with the four H30 controls).
        let c = config(DOWNSTREAM_PORT, Some((0x5f, 0x5f)));
        assert_eq!(port_p2p(&c), (Support::On, true));
        // Supported, both off.
        let c = config(DOWNSTREAM_PORT, Some((0x5f, 0x1d)));
        assert_eq!(port_p2p(&c), (Support::Off, false));
        // Not supported: the control bits mean nothing.
        let c = config(DOWNSTREAM_PORT, Some((0x1d, 0x5f)));
        assert_eq!(port_p2p(&c), (Support::Unsupported, false));
        assert_eq!(
            port_p2p(&config(ROOT_PORT, None)),
            (Support::Unsupported, false)
        );
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

    /// A root port, a switch (upstream and downstream ports) and a GPU
    /// without ATS.
    fn host(root: Vec<u8>, down: Vec<u8>) -> RecordingExec {
        host_with(root, down, endpoint(None))
    }

    fn host_with(root: Vec<u8>, down: Vec<u8>, gpu: Vec<u8>) -> RecordingExec {
        let exec = RecordingExec::new();
        let up = "../../../devices/pci0000:40/0000:40:01.1/0000:41:00.0/0000:42:00.0";
        for (bdf, link, config) in [
            ("0000:43:00.0", format!("{up}/0000:43:00.0"), gpu),
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
        let port = |address: &str| PortP2p {
            address: address.into(),
            direct_translated: Support::Unsupported,
            translation_blocking: false,
        };
        assert_eq!(
            r,
            AcsReport {
                off: vec![PortAcs {
                    address: "0000:42:00.0".into(),
                    off: vec![AcsBit::RequestRedirect, AcsBit::CompletionRedirect],
                }],
                ports: vec![port("0000:40:01.1"), port("0000:42:00.0")],
                devices: vec![DeviceAts {
                    address: "0000:43:00.0".into(),
                    ats: Support::Unsupported,
                }],
                failed: vec![],
            }
        );
        // All on: nothing off.
        let exec = host(
            config(ROOT_PORT, Some((0x1d, 0x1d))),
            config(DOWNSTREAM_PORT, Some((0x1d, 0x1d))),
        );
        assert!(ports_off(&exec, &["0000:43:00.0".into()]).off.is_empty());
    }

    #[test]
    fn switch_local_p2p_is_read_on_ports_and_devices() {
        let exec = host_with(
            config(ROOT_PORT, Some((0x1d, 0x1d))),
            config(DOWNSTREAM_PORT, Some((0x5f, 0x5d))),
            endpoint(Some(0x8000)),
        );
        let r = ports_off(&exec, &["0000:43:00.0".into()]);
        assert!(r.off.is_empty(), "{r:?}");
        assert_eq!(
            r.ports,
            [
                PortP2p {
                    address: "0000:40:01.1".into(),
                    direct_translated: Support::Unsupported,
                    translation_blocking: false,
                },
                PortP2p {
                    address: "0000:42:00.0".into(),
                    direct_translated: Support::On,
                    translation_blocking: false,
                },
            ]
        );
        assert_eq!(
            r.devices,
            [DeviceAts {
                address: "0000:43:00.0".into(),
                ats: Support::On,
            }]
        );
        assert!(exec.writes().is_empty());
    }

    #[test]
    fn a_device_read_short_is_reported() {
        let exec = host_with(
            config(ROOT_PORT, Some((0x1d, 0x1d))),
            config(DOWNSTREAM_PORT, Some((0x1d, 0x1d))),
            endpoint(Some(0x8000))[..64].to_vec(),
        );
        let r = ports_off(&exec, &["0000:43:00.0".into()]);
        assert!(r.devices.is_empty());
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert_eq!(r.failed[0].0, "0000:43:00.0");
    }

    #[test]
    fn an_older_answer_has_no_ports_or_devices() {
        let r: AcsReport = serde_json::from_value(serde_json::json!({"off": []})).unwrap();
        assert!(r.ports.is_empty() && r.devices.is_empty());
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

    /// ACS Control of `port` as `exec` holds it now.
    fn control(exec: &RecordingExec, port: &str) -> u16 {
        let c = read_config(exec, port).unwrap();
        u16_at(&c, 0x14e).unwrap()
    }

    const DOWN: &str = "0000:42:00.0";
    const ROOT: &str = "0000:40:01.1";
    /// SV, TB, RR, CR, UF and DT supported.
    const CAP: u16 = 0x5f;

    #[test]
    fn direct_translated_is_set_alone_and_given_back() {
        let exec = host(
            config(ROOT_PORT, Some((CAP, 0x1d))),
            config(DOWNSTREAM_PORT, Some((CAP, 0x1d))),
        );
        let (r, saved) = set_direct_translated(&exec, &[DOWN.into()], &[]);
        assert_eq!(r.set, vec![DOWN.to_string()], "{r:?}");
        assert!(r.failed.is_empty(), "{r:?}");
        // Bit 6 only: SV, RR, CR and UF stay on, TB stays off.
        assert_eq!(control(&exec, DOWN), 0x5d);
        assert_eq!(control(&exec, ROOT), 0x1d, "the root port isn't touched");
        assert_eq!(
            saved,
            vec![SavedDt {
                address: DOWN.into(),
                previous: false
            }]
        );
        let writes = exec.writes().len();
        let (again, saved) = set_direct_translated(&exec, &[DOWN.into()], &saved);
        assert_eq!(again, DtReport::default(), "the same set writes nothing");
        assert_eq!(exec.writes().len(), writes);
        let (back, saved) = set_direct_translated(&exec, &[], &saved);
        assert_eq!(back.restored, vec![DOWN.to_string()], "{back:?}");
        assert_eq!(control(&exec, DOWN), 0x1d);
        assert!(saved.is_empty());
    }

    #[test]
    fn a_port_that_had_it_on_keeps_it() {
        let exec = host(
            config(ROOT_PORT, Some((CAP, 0x1d))),
            config(DOWNSTREAM_PORT, Some((CAP, 0x5d))),
        );
        let (r, saved) = set_direct_translated(&exec, &[DOWN.into()], &[]);
        assert!(r.set.is_empty(), "{r:?}");
        assert!(saved[0].previous);
        let (back, saved) = set_direct_translated(&exec, &[], &saved);
        assert_eq!(back, DtReport::default());
        assert_eq!(control(&exec, DOWN), 0x5d);
        assert!(saved.is_empty());
        assert!(exec.writes().is_empty());
    }

    #[test]
    fn ports_that_mustnt_get_it_are_left_alone() {
        for (down, why) in [
            // H30: SV, RR, CR and UF supported and off.
            (config(DOWNSTREAM_PORT, Some((CAP, 0))), "its ACS is off"),
            (
                config(DOWNSTREAM_PORT, Some((0x1f, 0x1d))),
                "doesn't support Direct Translated P2P",
            ),
            (config(DOWNSTREAM_PORT, None), "it has no ACS"),
        ] {
            let exec = host(config(ROOT_PORT, Some((CAP, 0x1d))), down);
            let (r, saved) = set_direct_translated(&exec, &[DOWN.into()], &[]);
            assert!(r.set.is_empty() && saved.is_empty(), "{r:?}");
            assert_eq!(r.failed.len(), 1, "{r:?}");
            assert!(r.failed[0].1.contains(why), "{r:?}");
            assert!(exec.writes().is_empty());
        }
        let exec = host(
            config(ROOT_PORT, Some((CAP, 0x1d))),
            config(DOWNSTREAM_PORT, Some((CAP, 0x1d))),
        );
        let (r, _) = set_direct_translated(&exec, &[ROOT.into()], &[]);
        assert!(
            r.failed[0].1.contains("not a switch downstream port"),
            "{r:?}"
        );
        assert!(exec.writes().is_empty());
    }

    #[test]
    fn a_port_that_cant_be_read_stays_recorded() {
        let exec = host(
            config(ROOT_PORT, Some((CAP, 0x1d))),
            config(DOWNSTREAM_PORT, Some((CAP, 0x5d)))[..256].to_vec(),
        );
        let saved = vec![SavedDt {
            address: DOWN.into(),
            previous: false,
        }];
        let (r, kept) = set_direct_translated(&exec, &[], &saved);
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert_eq!(kept, saved, "put back once it reads again");
    }
}
