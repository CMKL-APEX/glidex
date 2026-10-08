//! VM-side ports: tap devices and vhost-user sockets on glidex bridges.

use crate::bridge::{self, Datapath};
use crate::exec::{Cmd, Exec, Program};
use crate::host::HostCapabilities;
use crate::names::{self, validate_mac, validate_name, validate_vm_id, MAX_IFNAME, MAX_NICS};
use crate::net::link_exists;
use crate::vsctl::{self, owned_tags, tag_args, NIC_KEY, VM_ID_KEY};
use crate::OvsError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmPortKind {
    Tap,
    VhostUser,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmPortSpec {
    pub bridge: String,
    pub vm_id: String,
    pub nic_index: u8,
    pub kind: VmPortKind,
    pub mac: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlan: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    #[serde(default = "default_queue_pairs")]
    pub queue_pairs: u8,
    /// A port on an OVN network: the logical port this interface is
    /// (`external_ids:iface-id`); `bridge` must be `br-int`
    /// (spec/clustering.md §11.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ovn_lport: Option<String>,
}

fn default_queue_pairs() -> u8 {
    1
}

impl VmPortSpec {
    pub fn validate(&self) -> Result<(), OvsError> {
        validate_name("bridge", &self.bridge, MAX_IFNAME)?;
        validate_vm_id(&self.vm_id)?;
        if self.nic_index >= MAX_NICS {
            return Err(OvsError::invalid(format!("nic_index must be < {}", MAX_NICS)));
        }
        validate_mac(&self.mac)?;
        if let Some(vlan) = self.vlan {
            if !(1..=4094).contains(&vlan) {
                return Err(OvsError::invalid("vlan must be 1-4094"));
            }
        }
        if let Some(l) = &self.ovn_lport {
            validate_name("ovn_lport", l, 63)?;
            if self.bridge != crate::ovn::BR_INT {
                return Err(OvsError::invalid("an OVN port goes on br-int"));
            }
            if self.vlan.is_some() {
                return Err(OvsError::invalid("an OVN port has no VLAN tag: the logical switch decides"));
            }
        } else if self.bridge == crate::ovn::BR_INT {
            return Err(OvsError::invalid("br-int is OVN's: give ovn_lport"));
        }
        if !(1..=8).contains(&self.queue_pairs) {
            return Err(OvsError::invalid("queue_pairs must be 1-8"));
        }
        if let Some(mtu) = self.mtu {
            if !(576..=9216).contains(&mtu) {
                return Err(OvsError::invalid("mtu must be 576-9216"));
            }
        }
        Ok(())
    }

    pub fn port_name(&self) -> Result<String, OvsError> {
        names::port_name(&self.vm_id, self.nic_index)
    }
}

/// What the hypervisor needs to use the port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmPortBinding {
    Tap { ifname: String },
    /// The hypervisor must run as the vhost-user *server* on this path.
    VhostUser { socket: PathBuf },
}

/// Create the port. `owner_uid` owns the tap (the control plane's uid,
/// taken from the connection's peer credentials, never from a request).
pub fn attach(
    exec: &dyn Exec,
    caps: &HostCapabilities,
    spec: &VmPortSpec,
    owner_uid: u32,
) -> Result<VmPortBinding, OvsError> {
    spec.validate()?;
    let port = spec.port_name()?;
    let br = bridge::get(exec, &spec.bridge)?
        .ok_or_else(|| OvsError::not_found(format!("bridge '{}'", spec.bridge)))?;
    if !br.owned {
        return Err(OvsError::not_owned(format!("bridge '{}'", spec.bridge)));
    }
    if spec.kind == VmPortKind::VhostUser {
        let mut missing = Vec::new();
        if br.datapath != Datapath::Netdev {
            missing.push(format!("netdev datapath on bridge '{}'", spec.bridge));
        }
        if !caps.iface_types.iter().any(|t| t == "dpdkvhostuserclient") {
            missing.push("dpdkvhostuserclient".into());
        }
        if !missing.is_empty() {
            return Err(OvsError::Unsupported { missing });
        }
    }

    let nic = spec.nic_index.to_string();
    let tags = tag_args(&owned_tags(
        "vm",
        &[(VM_ID_KEY, spec.vm_id.as_str()), (NIC_KEY, nic.as_str())],
    ));
    let mut args = vec![
        "--may-exist".to_string(),
        "add-port".to_string(),
        spec.bridge.clone(),
        port.clone(),
        "--".to_string(),
        "set".to_string(),
        "Interface".to_string(),
        port.clone(),
    ];

    let binding = match spec.kind {
        VmPortKind::Tap => {
            if !link_exists(exec, &port) {
                let mut tuntap = vec![
                    "tuntap".to_string(),
                    "add".into(),
                    "dev".into(),
                    port.clone(),
                    "mode".into(),
                    "tap".into(),
                ];
                if spec.queue_pairs > 1 {
                    tuntap.push("multi_queue".into());
                }
                tuntap.extend(["user".to_string(), owner_uid.to_string()]);
                exec.check(&Cmd::new(Program::Ip, tuntap))?;
            }
            VmPortBinding::Tap {
                ifname: port.clone(),
            }
        }
        VmPortKind::VhostUser => {
            let socket = names::vhost_socket(&spec.vm_id, spec.nic_index)?;
            args.push("type=dpdkvhostuserclient".into());
            args.push(format!("options:vhost-server-path={}", socket.display()));
            VmPortBinding::VhostUser { socket }
        }
    };
    // OVS sets (and keeps) the port's MTU, so the hypervisor doesn't need
    // the privilege to change it.
    if let Some(mtu) = spec.mtu {
        args.push(format!("mtu_request={}", mtu));
    }
    args.extend(tags);
    if let Some(l) = &spec.ovn_lport {
        args.push(format!("external_ids:iface-id={l}"));
    }
    if let Some(vlan) = spec.vlan {
        args.extend([
            "--".to_string(),
            "set".into(),
            "Port".into(),
            port.clone(),
            format!("tag={}", vlan),
        ]);
    }
    if let Err(e) = vsctl::run(exec, args) {
        // Don't leave a tap behind that no OVS port references.
        if spec.kind == VmPortKind::Tap {
            let _ = exec.run(&Cmd::new(Program::Ip, ["link", "del", port.as_str()]));
        }
        return Err(e);
    }
    // OVS doesn't bring a tap up, and only a hypervisor with
    // CAP_NET_ADMIN could (QEMU, as packaged, can't).
    if spec.kind == VmPortKind::Tap {
        exec.check(&Cmd::new(Program::Ip, ["link", "set", "dev", port.as_str(), "up"]))?;
    }
    Ok(binding)
}

/// Remove the port, its tap and its socket. Idempotent; refuses to touch
/// a port of that name that glidex didn't tag.
pub fn detach(exec: &dyn Exec, vm_id: &str, nic_index: u8) -> Result<(), OvsError> {
    let port = names::port_name(vm_id, nic_index)?;
    if let Some(row) = vsctl::find_by_name(exec, "Interface", &port, &["name", "external_ids", "type"])? {
        if !row.owned_by_glidex() {
            return Err(OvsError::not_owned(format!("interface '{}'", port)));
        }
        vsctl::run(exec, vec!["--if-exists".into(), "del-port".into(), port.clone()])?;
    }
    if link_exists(exec, &port) {
        exec.check(&Cmd::new(Program::Ip, ["link", "del", port.as_str()]))?;
    }
    exec.remove_file(&names::vhost_socket(vm_id, nic_index)?)?;
    Ok(())
}

/// `(vm_id, nic_index, port)` of every glidex VM interface in OVSDB.
pub fn list_owned(exec: &dyn Exec) -> Result<Vec<(String, u8, String)>, OvsError> {
    Ok(vsctl::list(exec, "Interface", &["name", "external_ids"])?
        .into_iter()
        .filter(|r| r.owned_by_glidex())
        .filter_map(|r| {
            let ids = r.map("external_ids");
            if ids.get(vsctl::ROLE_KEY).map(String::as_str) != Some("vm") {
                return None;
            }
            Some((
                ids.get(VM_ID_KEY)?.clone(),
                ids.get(NIC_KEY)?.parse().ok()?,
                r.str("name")?,
            ))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    const VM: &str = "1a2b3c4d-5e6f-4a1b-9c2d-0123456789ab";
    const FIND_BR: &str = "ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge name=gxbr-nat";
    const FIND_IF: &str = "ovs-vsctl --format=json --columns=name,external_ids,type find Interface name=gx1a2b3c4d-0";

    fn bridge(datapath: &str, owned: bool) -> Output {
        let ids = if owned { r#"["map",[["glidex-owner","glidex"]]]"# } else { r#"["map",[]]"# };
        Output::ok(format!(r#"{{"data":[["gxbr-nat","{datapath}",{ids},["set",[]]]],"headings":["name","datapath_type","external_ids","ports"]}}"#))
    }

    fn spec(kind: VmPortKind) -> VmPortSpec {
        VmPortSpec {
            bridge: "gxbr-nat".into(),
            vm_id: VM.into(),
            nic_index: 0,
            kind,
            mac: names::mac_address(VM, 0).unwrap(),
            vlan: None,
            mtu: None,
            queue_pairs: 1,
            ovn_lport: None,
        }
    }

    #[test]
    fn tap_port_commands() {
        let exec = RecordingExec::new();
        exec.on(FIND_BR, bridge("system", true));
        let binding = attach(&exec, &HostCapabilities::default(), &VmPortSpec { vlan: Some(10), ..spec(VmPortKind::Tap) }, 1000).unwrap();
        assert_eq!(binding, VmPortBinding::Tap { ifname: "gx1a2b3c4d-0".into() });
        let calls = exec.calls();
        assert!(calls.contains(&"ip tuntap add dev gx1a2b3c4d-0 mode tap user 1000".to_string()), "{calls:?}");
        assert_eq!(calls.last().map(String::as_str), Some("ip link set dev gx1a2b3c4d-0 up"), "{calls:?}");
        assert!(calls.contains(&format!("ovs-vsctl --may-exist add-port gxbr-nat gx1a2b3c4d-0 -- set Interface gx1a2b3c4d-0 external_ids:glidex-owner=glidex external_ids:glidex-role=vm external_ids:glidex-vm-id={VM} external_ids:glidex-nic=0 -- set Port gx1a2b3c4d-0 tag=10")), "{calls:?}");
    }

    #[test]
    fn an_ovn_port_goes_on_br_int_with_its_iface_id() {
        let exec = RecordingExec::new();
        exec.on(
            "ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge name=br-int",
            Output::ok(r#"{"data":[["br-int","system",["map",[["glidex-owner","glidex"]]],["set",[]]]],"headings":["name","datapath_type","external_ids","ports"]}"#),
        );
        let s = VmPortSpec { bridge: "br-int".into(), ovn_lport: Some("gx-1a2b3c4d-0".into()), mtu: Some(1442), ..spec(VmPortKind::Tap) };
        attach(&exec, &HostCapabilities::default(), &s, 1000).unwrap();
        let add = exec.calls().into_iter().find(|c| c.contains("add-port br-int")).unwrap();
        assert!(add.contains("external_ids:iface-id=gx-1a2b3c4d-0") && add.contains("mtu_request=1442"), "{add}");
        // br-int takes only OVN ports, and an OVN port only br-int.
        assert!(VmPortSpec { ovn_lport: None, ..s.clone() }.validate().is_err());
        assert!(VmPortSpec { bridge: "gxbr-nat".into(), ..s.clone() }.validate().is_err());
        assert!(VmPortSpec { vlan: Some(5), ..s }.validate().is_err());
    }

    #[test]
    fn port_mtu_is_requested_from_ovs() {
        let exec = RecordingExec::new();
        exec.on(FIND_BR, bridge("system", true));
        attach(&exec, &HostCapabilities::default(), &VmPortSpec { mtu: Some(9000), ..spec(VmPortKind::Tap) }, 1000).unwrap();
        assert!(exec.calls().iter().any(|c| c.contains("set Interface gx1a2b3c4d-0 mtu_request=9000 external_ids:")), "{:?}", exec.calls());
        let bad = VmPortSpec { mtu: Some(100), ..spec(VmPortKind::Tap) };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn existing_tap_is_reused() {
        let exec = RecordingExec::new();
        exec.on(FIND_BR, bridge("system", true));
        exec.file("/sys/class/net/gx1a2b3c4d-0", "");
        attach(&exec, &HostCapabilities::default(), &spec(VmPortKind::Tap), 1000).unwrap();
        assert!(!exec.calls().iter().any(|c| c.contains("tuntap")));
    }

    #[test]
    fn failed_ovs_port_removes_the_new_tap() {
        let exec = RecordingExec::new();
        exec.on(FIND_BR, bridge("system", true));
        exec.on("ovs-vsctl --may-exist add-port", Output::failed(1, "boom"));
        assert!(attach(&exec, &HostCapabilities::default(), &spec(VmPortKind::Tap), 1000).is_err());
        assert!(exec.calls().contains(&"ip link del gx1a2b3c4d-0".to_string()));
    }

    #[test]
    fn vhost_user_needs_netdev_and_dpdk() {
        let exec = RecordingExec::new();
        exec.on(FIND_BR, bridge("system", true));
        let err = attach(&exec, &HostCapabilities::default(), &spec(VmPortKind::VhostUser), 1000).unwrap_err();
        let OvsError::Unsupported { missing } = err else { panic!() };
        assert_eq!(missing.len(), 2);

        let exec = RecordingExec::new();
        exec.on(FIND_BR, bridge("netdev", true));
        let caps = HostCapabilities { iface_types: vec!["dpdkvhostuserclient".into()], ..Default::default() };
        let binding = attach(&exec, &caps, &spec(VmPortKind::VhostUser), 1000).unwrap();
        let VmPortBinding::VhostUser { socket } = binding else { panic!() };
        assert_eq!(socket, names::vhost_socket(VM, 0).unwrap());
        assert!(exec.calls().iter().any(|c| c.contains("type=dpdkvhostuserclient") && c.contains(&format!("options:vhost-server-path={}", socket.display()))));
        assert!(!exec.calls().iter().any(|c| c.contains("tuntap")));
    }

    #[test]
    fn refuses_foreign_bridge_and_bad_input() {
        let exec = RecordingExec::new();
        exec.on(FIND_BR, bridge("system", false));
        assert!(matches!(attach(&exec, &HostCapabilities::default(), &spec(VmPortKind::Tap), 1000), Err(OvsError::NotOwned { .. })));
        let bad = VmPortSpec { mac: "01:00:5e:00:00:01".into(), ..spec(VmPortKind::Tap) };
        assert!(matches!(attach(&exec, &HostCapabilities::default(), &bad, 1000), Err(OvsError::InvalidArgument { .. })));
    }

    #[test]
    fn detach_is_idempotent_and_ownership_checked() {
        let exec = RecordingExec::new();
        exec.on(FIND_IF, Output::ok(r#"{"data":[["gx1a2b3c4d-0",["map",[["glidex-owner","glidex"]]],""]],"headings":["name","external_ids","type"]}"#));
        exec.file("/sys/class/net/gx1a2b3c4d-0", "");
        detach(&exec, VM, 0).unwrap();
        let calls = exec.calls();
        assert!(calls.contains(&"ovs-vsctl --if-exists del-port gx1a2b3c4d-0".to_string()));
        assert!(calls.contains(&"ip link del gx1a2b3c4d-0".to_string()));

        let exec = RecordingExec::new();
        exec.on(FIND_IF, Output::ok(r#"{"data":[],"headings":["name","external_ids","type"]}"#));
        detach(&exec, VM, 0).unwrap();
        assert!(!exec.calls().iter().any(|c| c.contains("del-port") || c.contains("link del")));

        let exec = RecordingExec::new();
        exec.on(FIND_IF, Output::ok(r#"{"data":[["gx1a2b3c4d-0",["map",[]],""]],"headings":["name","external_ids","type"]}"#));
        assert!(matches!(detach(&exec, VM, 0), Err(OvsError::NotOwned { .. })));
    }
}
