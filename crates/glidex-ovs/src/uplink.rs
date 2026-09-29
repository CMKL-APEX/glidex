//! Host-side uplink ports: kernel NIC, AF_XDP, DPDK (spec §8.3).

use crate::bridge::{self, Datapath};
use crate::exec::Exec;
use crate::host::HostCapabilities;
use crate::names::{validate_ifname, validate_name, MAX_IFNAME};
use crate::nic;
use crate::vsctl::{self, owned_tags, tag_args};
use crate::OvsError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum XdpMode {
    BestEffort,
    Native,
    NativeWithZerocopy,
    Generic,
}

impl XdpMode {
    pub fn as_ovs(self) -> &'static str {
        match self {
            XdpMode::BestEffort => "best-effort",
            XdpMode::Native => "native",
            XdpMode::NativeWithZerocopy => "native-with-zerocopy",
            XdpMode::Generic => "generic",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UplinkKind {
    Kernel { ifname: String },
    Afxdp {
        ifname: String,
        #[serde(default = "default_xdp_mode")]
        xdp_mode: XdpMode,
        #[serde(default = "one")]
        n_rxq: u16,
    },
    Dpdk {
        pci: String,
        #[serde(default = "one")]
        n_rxq: u16,
    },
}

fn default_xdp_mode() -> XdpMode {
    XdpMode::BestEffort
}

fn one() -> u16 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UplinkSpec {
    /// OVS port name; for kernel/AF_XDP uplinks it must equal the NIC name.
    pub name: String,
    pub bridge: String,
    #[serde(flatten)]
    pub kind: UplinkKind,
    /// Move the NIC's IP configuration onto the bridge.
    #[serde(default)]
    pub migrate_ip: bool,
}

impl UplinkSpec {
    pub fn validate(&self) -> Result<(), OvsError> {
        validate_name("bridge", &self.bridge, MAX_IFNAME)?;
        match &self.kind {
            UplinkKind::Kernel { ifname } | UplinkKind::Afxdp { ifname, .. } => {
                validate_ifname(ifname)?;
                if &self.name != ifname {
                    return Err(OvsError::invalid("kernel/AF_XDP uplink name must equal the NIC name"));
                }
            }
            UplinkKind::Dpdk { pci, .. } => {
                nic::validate_bdf(pci)?;
                validate_name("uplink", &self.name, MAX_IFNAME)?;
                if self.migrate_ip {
                    return Err(OvsError::invalid(
                        "a DPDK NIC leaves the kernel; its IP can't be migrated with migrate_ip (move it to the bridge by hand)",
                    ));
                }
            }
        }
        if let UplinkKind::Afxdp { n_rxq, .. } | UplinkKind::Dpdk { n_rxq, .. } = &self.kind {
            if !(1..=64).contains(n_rxq) {
                return Err(OvsError::invalid("n_rxq must be 1-64"));
            }
        }
        Ok(())
    }

    /// The kernel interface this uplink takes over, if any.
    pub fn ifname(&self) -> Option<&str> {
        match &self.kind {
            UplinkKind::Kernel { ifname } | UplinkKind::Afxdp { ifname, .. } => Some(ifname),
            UplinkKind::Dpdk { .. } => None,
        }
    }

    fn required_datapath(&self) -> Datapath {
        match self.kind {
            UplinkKind::Kernel { .. } => Datapath::System,
            _ => Datapath::Netdev,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UplinkState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_state: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Check the spec against the bridge and host (no changes).
pub fn check(exec: &dyn Exec, caps: &HostCapabilities, spec: &UplinkSpec) -> Result<(), OvsError> {
    spec.validate()?;
    let br = bridge::get(exec, &spec.bridge)?
        .ok_or_else(|| OvsError::not_found(format!("bridge '{}'", spec.bridge)))?;
    if !br.owned {
        return Err(OvsError::not_owned(format!("bridge '{}'", spec.bridge)));
    }
    let need = spec.required_datapath();
    if br.datapath != need {
        return Err(OvsError::Unsupported {
            missing: vec![format!("{} datapath on bridge '{}'", need.as_str(), spec.bridge)],
        });
    }
    let iface_type = match spec.kind {
        UplinkKind::Afxdp { .. } => Some("afxdp"),
        UplinkKind::Dpdk { .. } => Some("dpdk"),
        UplinkKind::Kernel { .. } => None,
    };
    if let Some(t) = iface_type {
        if !caps.iface_types.iter().any(|x| x == t) {
            return Err(OvsError::Unsupported { missing: vec![t.into()] });
        }
    }
    if let UplinkKind::Dpdk { .. } = spec.kind {
        if !caps.dpdk_initialized {
            return Err(OvsError::Unsupported { missing: vec!["dpdk initialized".into()] });
        }
        if !caps.iommu {
            return Err(OvsError::Unsupported { missing: vec!["iommu".into()] });
        }
    }
    Ok(())
}

/// Add the OVS port (after any DPDK driver binding, done by the caller).
pub fn add_port(exec: &dyn Exec, spec: &UplinkSpec, orig_driver: Option<&str>) -> Result<(), OvsError> {
    let mut extra = vec![("glidex-uplink", spec.name.as_str())];
    if let Some(d) = orig_driver {
        extra.push(("glidex-orig-driver", d));
    }
    let tags = tag_args(&owned_tags("uplink", &extra));
    let mut args = vec![
        "--may-exist".to_string(),
        "add-port".into(),
        spec.bridge.clone(),
        spec.name.clone(),
        "--".into(),
        "set".into(),
        "Interface".into(),
        spec.name.clone(),
    ];
    match &spec.kind {
        UplinkKind::Kernel { .. } => {}
        UplinkKind::Afxdp { xdp_mode, n_rxq, .. } => {
            args.push("type=afxdp".into());
            args.push(format!("options:xdp-mode={}", xdp_mode.as_ovs()));
            args.push(format!("options:n_rxq={}", n_rxq));
        }
        UplinkKind::Dpdk { pci, n_rxq } => {
            args.push("type=dpdk".into());
            args.push(format!("options:dpdk-devargs={}", pci));
            args.push(format!("options:n_rxq={}", n_rxq));
        }
    }
    args.extend(tags);
    vsctl::run(exec, args)
}

/// Remove the port if glidex owns it.
pub fn del_port(exec: &dyn Exec, name: &str) -> Result<(), OvsError> {
    if let Some(row) = vsctl::find_by_name(exec, "Interface", name, &["name", "external_ids"])? {
        if !row.owned_by_glidex() {
            return Err(OvsError::not_owned(format!("interface '{}'", name)));
        }
        vsctl::run(exec, vec!["--if-exists".into(), "del-port".into(), name.into()])?;
    }
    Ok(())
}

/// Live state from OVSDB.
pub fn state(exec: &dyn Exec, name: &str) -> Result<UplinkState, OvsError> {
    let row = vsctl::find_by_name(exec, "Interface", name, &["name", "error", "link_state"])?;
    Ok(match row {
        Some(r) => UplinkState {
            error: r.str("error"),
            link_state: r.str("link_state"),
            warnings: Vec::new(),
        },
        None => UplinkState {
            error: Some("port missing on host".into()),
            ..Default::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    const FIND_BR: &str = "ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge name=gxbr-up";

    fn bridge(exec: &RecordingExec, datapath: &str) {
        exec.on(FIND_BR, Output::ok(format!(r#"{{"data":[["gxbr-up","{datapath}",["map",[["glidex-owner","glidex"]]],["set",[]]]],"headings":["name","datapath_type","external_ids","ports"]}}"#)));
    }

    fn kernel() -> UplinkSpec {
        UplinkSpec { name: "gxup0".into(), bridge: "gxbr-up".into(), kind: UplinkKind::Kernel { ifname: "gxup0".into() }, migrate_ip: false }
    }

    #[test]
    fn wire_format() {
        let spec: UplinkSpec = serde_json::from_str(r#"{"name":"up0","bridge":"gxbr-up","kind":"dpdk","pci":"0000:41:00.0"}"#).unwrap();
        assert_eq!(spec.kind, UplinkKind::Dpdk { pci: "0000:41:00.0".into(), n_rxq: 1 });
        let spec: UplinkSpec = serde_json::from_str(r#"{"name":"enp2s0","bridge":"gxbr-up","kind":"afxdp","ifname":"enp2s0","xdp_mode":"generic"}"#).unwrap();
        assert!(matches!(spec.kind, UplinkKind::Afxdp { xdp_mode: XdpMode::Generic, .. }));
    }

    #[test]
    fn validation() {
        kernel().validate().unwrap();
        let mut s = kernel();
        s.name = "other".into();
        assert!(s.validate().is_err(), "name must match NIC");
        let dpdk = UplinkSpec { name: "up0".into(), bridge: "gxbr-up".into(), kind: UplinkKind::Dpdk { pci: "0000:41:00.0".into(), n_rxq: 2 }, migrate_ip: true };
        assert!(dpdk.validate().is_err(), "can't migrate IP off a DPDK NIC");
    }

    #[test]
    fn check_datapath_and_features() {
        let exec = RecordingExec::new();
        bridge(&exec, "system");
        check(&exec, &HostCapabilities::default(), &kernel()).unwrap();
        let afxdp = UplinkSpec { name: "gxup0".into(), bridge: "gxbr-up".into(), kind: UplinkKind::Afxdp { ifname: "gxup0".into(), xdp_mode: XdpMode::Generic, n_rxq: 1 }, migrate_ip: false };
        assert!(matches!(check(&exec, &HostCapabilities::default(), &afxdp), Err(OvsError::Unsupported { .. })), "afxdp needs netdev");

        let exec = RecordingExec::new();
        bridge(&exec, "netdev");
        let caps = HostCapabilities { iface_types: vec!["afxdp".into()], ..Default::default() };
        check(&exec, &caps, &afxdp).unwrap();
        assert!(check(&exec, &caps, &kernel()).is_err(), "kernel uplink needs system datapath");
    }

    #[test]
    fn port_commands() {
        let exec = RecordingExec::new();
        add_port(&exec, &kernel(), None).unwrap();
        let afxdp = UplinkSpec { name: "enp2s0".into(), bridge: "gxbr-up".into(), kind: UplinkKind::Afxdp { ifname: "enp2s0".into(), xdp_mode: XdpMode::Native, n_rxq: 4 }, migrate_ip: false };
        add_port(&exec, &afxdp, None).unwrap();
        let dpdk = UplinkSpec { name: "up0".into(), bridge: "gxbr-up".into(), kind: UplinkKind::Dpdk { pci: "0000:41:00.0".into(), n_rxq: 2 }, migrate_ip: false };
        add_port(&exec, &dpdk, Some("ixgbe")).unwrap();
        let calls = exec.calls();
        assert_eq!(calls[0], "ovs-vsctl --may-exist add-port gxbr-up gxup0 -- set Interface gxup0 external_ids:glidex-owner=glidex external_ids:glidex-role=uplink external_ids:glidex-uplink=gxup0");
        assert!(calls[1].contains("type=afxdp options:xdp-mode=native options:n_rxq=4"));
        assert!(calls[2].contains("type=dpdk options:dpdk-devargs=0000:41:00.0 options:n_rxq=2") && calls[2].contains("external_ids:glidex-orig-driver=ixgbe"));
    }
}
