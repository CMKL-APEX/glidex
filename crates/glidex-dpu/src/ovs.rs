//! OVS-DOCA on the DPU: VF representor ports, bridges and the chassis
//! settings (spec/31-dpu.md §7.1 step 5, §8.3, §9.3 step 3).
//!
//! Every port and bridge glidex makes carries the ownership tags
//! `glidex-owner=glidex` and `glidex-role`; nothing untagged is changed or
//! removed.

use crate::DpuError;
use crate::exec::{Cmd, Exec, Program};
use crate::ovsdb;
use crate::types::{DatapathType, DpuPortSpec, is_mac};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub const OWNER_KEY: &str = "glidex-owner";
pub const OWNER_VALUE: &str = "glidex";
pub const ROLE_KEY: &str = "glidex-role";
pub const VM_ID_KEY: &str = "glidex-vm-id";
pub const NIC_KEY: &str = "glidex-nic";
/// Marks the `Open_vSwitch` settings glidex set for the DPU's chassis, so
/// leaving clears only those.
pub const CHASSIS_TAG: &str = "glidex-chassis";

/// Interface names the kernel accepts.
const MAX_IFNAME: usize = 15;

fn validate_ifname(what: &str, name: &str) -> Result<(), DpuError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_IFNAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(DpuError::invalid(format!("{what} name `{name}`")))
    }
}

fn vsctl<I, S>(args: I) -> Cmd
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    Cmd::new(Program::OvsVsctl, args)
}

/// §7.1 step 5: the VF's representor on `bridge`, `type=doca` with no
/// options (finding 2), bound to its logical port by `iface-id` and tagged.
pub fn add_representor(bridge: &str, rep: &str, spec: &DpuPortSpec) -> Result<Cmd, DpuError> {
    validate_ifname("bridge", bridge)?;
    validate_ifname("representor", rep)?;
    spec.validate(u8::MAX)?;
    Ok(vsctl([
        "--may-exist".to_string(),
        "add-port".into(),
        bridge.into(),
        rep.into(),
        "--".into(),
        "set".into(),
        "Interface".into(),
        rep.into(),
        "type=doca".into(),
        format!("external_ids:iface-id={}", spec.lsp),
        format!("external_ids:{OWNER_KEY}={OWNER_VALUE}"),
        format!("external_ids:{ROLE_KEY}=vm"),
        format!("external_ids:{VM_ID_KEY}={}", spec.vm_id),
        format!("external_ids:{NIC_KEY}={}", spec.nic),
    ]))
}

/// `ip link set <rep> up`.
pub fn link_up(rep: &str) -> Result<Cmd, DpuError> {
    validate_ifname("representor", rep)?;
    Ok(Cmd::new(Program::Ip, ["link", "set", rep, "up"]))
}

/// A glidex-tagged VM port on the DPU, as `list_ports` reads it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpuPortRow {
    pub name: String,
    pub vm_id: String,
    pub nic: u8,
    pub lsp: Option<String>,
}

const LIST_PORTS: [&str; 6] = [
    "--format=json",
    "--columns=name,external_ids",
    "find",
    "Interface",
    "external_ids:glidex-owner=glidex",
    "external_ids:glidex-role=vm",
];

/// The tagged VM ports on the DPU (§7.4).
pub fn list_ports(exec: &dyn Exec) -> Result<Vec<DpuPortRow>, DpuError> {
    let out = exec.check(&vsctl(LIST_PORTS))?;
    let mut ports: Vec<DpuPortRow> = ovsdb::json_rows(&out.stdout_str())?
        .iter()
        .filter_map(|r| {
            let ids = ovsdb::json_map(r, "external_ids");
            Some(DpuPortRow {
                name: ovsdb::json_str(r, "name")?,
                vm_id: ids.get(VM_ID_KEY)?.clone(),
                nic: ids.get(NIC_KEY)?.parse().ok()?,
                lsp: ids.get("iface-id").cloned(),
            })
        })
        .collect();
    ports.sort_by(|a, b| (&a.vm_id, a.nic).cmp(&(&b.vm_id, b.nic)));
    Ok(ports)
}

/// §7.2: remove the representor port of `(vm_id, nic)`. Already gone is
/// fine; a port with that name glidex didn't tag is refused.
pub fn remove_representor(
    exec: &dyn Exec,
    rep: &str,
    vm_id: &str,
    nic: u8,
) -> Result<bool, DpuError> {
    validate_ifname("representor", rep)?;
    let out = exec.check(&vsctl([
        "--format=json",
        "--columns=name,external_ids",
        "find",
        "Interface",
        &format!("name={rep}"),
    ]))?;
    let rows = ovsdb::json_rows(&out.stdout_str())?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let ids = ovsdb::json_map(row, "external_ids");
    let ours = ids.get(OWNER_KEY).map(String::as_str) == Some(OWNER_VALUE)
        && ids.get(VM_ID_KEY).map(String::as_str) == Some(vm_id)
        && ids.get(NIC_KEY).map(String::as_str) == Some(&nic.to_string());
    if !ours {
        return Err(DpuError::NotOwned {
            message: format!("port {rep} is not glidex's port for {vm_id}/{nic}"),
        });
    }
    exec.check(&vsctl(["--if-exists", "del-port", rep]))?;
    Ok(true)
}

/// A bridge on the DOCA datapath (§9.3 step 3). `datapath` must be `doca`.
pub fn ensure_bridge(name: &str, datapath: &str, role: &str) -> Result<Cmd, DpuError> {
    validate_ifname("bridge", name)?;
    let dp = DatapathType::require_doca(datapath)?;
    Ok(vsctl([
        "--may-exist".to_string(),
        "add-br".into(),
        name.into(),
        "--".into(),
        "set".into(),
        "Bridge".into(),
        name.into(),
        format!("datapath_type={}", dp.as_str()),
        format!("external_ids:{OWNER_KEY}={OWNER_VALUE}"),
        format!("external_ids:{ROLE_KEY}={role}"),
    ]))
}

/// `br-int` for OVN on the DPU: `doca`, `fail_mode=secure` as
/// `ovn-controller` makes it (§8.3).
pub fn ensure_br_int(name: &str) -> Result<Cmd, DpuError> {
    let mut cmd = ensure_bridge(name, "doca", "ovn-int")?;
    cmd.args.extend([
        "fail_mode=secure".to_string(),
        "other-config:disable-in-band=true".to_string(),
    ]);
    Ok(cmd)
}

/// A physical or representor port on a DOCA bridge: `type=doca` and **no
/// options** (finding 2: `dpdk-devargs` on a `doca` port breaks it).
pub fn add_doca_port(bridge: &str, port: &str) -> Result<Cmd, DpuError> {
    validate_ifname("bridge", bridge)?;
    validate_ifname("port", port)?;
    Ok(vsctl([
        "--may-exist",
        "add-port",
        bridge,
        port,
        "--",
        "set",
        "Interface",
        port,
        "type=doca",
    ]))
}

/// Refuse a port type other than `doca` on the DPU (decision 11).
pub fn require_doca_port_type(port_type: &str) -> Result<(), DpuError> {
    match port_type {
        "doca" | "internal" | "patch" | "geneve" => Ok(()),
        other => Err(DpuError::invalid(format!(
            "port type `{other}` on a DOCA DPU; ports there are `doca` (spec/31-dpu.md decision 11)"
        ))),
    }
}

/// The DPU chassis's `Open_vSwitch` settings (§8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChassisSettings {
    /// `node:<node id>-dpu` ([`crate::types::chassis_name`]).
    pub system_id: String,
    /// The cluster's SB remotes, `ssl:<ip>:<port>`, the same as the host's.
    pub sb_remotes: Vec<String>,
    pub encap_ip: IpAddr,
    /// The integration bridge, `br-int` unless `dpu.json` says otherwise.
    pub br_int: String,
}

impl ChassisSettings {
    pub fn validate(&self) -> Result<(), DpuError> {
        let id = self
            .system_id
            .strip_prefix("node:")
            .unwrap_or(&self.system_id);
        let id_ok = id.ends_with("-dpu")
            && id.len() <= 68
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if !id_ok {
            return Err(DpuError::invalid(format!(
                "chassis `{}` (want `node:<node id>-dpu`)",
                self.system_id
            )));
        }
        if self.sb_remotes.is_empty() {
            return Err(DpuError::invalid(
                "at least one southbound remote is needed",
            ));
        }
        for r in &self.sb_remotes {
            let ok = r.strip_prefix("ssl:").is_some_and(|hp| {
                hp.rsplit_once(':').is_some_and(|(h, p)| {
                    !h.is_empty()
                        && p.parse::<u16>().is_ok()
                        && h.chars()
                            .all(|c| c.is_ascii_hexdigit() || ".:[]".contains(c))
                })
            });
            if !ok {
                return Err(DpuError::invalid(format!(
                    "remote `{r}` must look like ssl:<ip>:<port>"
                )));
            }
        }
        validate_ifname("bridge", &self.br_int)
    }
}

/// The `ovs-vsctl` commands that make the DPU a chassis: settings, and no
/// `ovn-bridge-mappings` in v1 (§8.3, O18).
pub fn chassis_cmds(s: &ChassisSettings) -> Result<Vec<Cmd>, DpuError> {
    s.validate()?;
    Ok(vec![
        vsctl([
            "set".to_string(),
            "Open_vSwitch".into(),
            ".".into(),
            format!("external_ids:system-id={}", s.system_id),
            format!("external_ids:ovn-remote={}", s.sb_remotes.join(",")),
            "external_ids:ovn-encap-type=geneve".into(),
            format!("external_ids:ovn-encap-ip={}", s.encap_ip),
            format!("external_ids:ovn-bridge={}", s.br_int),
            format!("external_ids:{CHASSIS_TAG}=1"),
        ]),
        vsctl([
            "--if-exists",
            "remove",
            "Open_vSwitch",
            ".",
            "external_ids",
            "ovn-bridge-mappings",
        ]),
    ])
}

/// Leaving OVN (§8.3): remove what [`chassis_cmds`] set. The `system-id`
/// goes too, so a later enrolment can't reuse a stale identity.
pub fn leave_cmds() -> Cmd {
    vsctl([
        "--if-exists",
        "remove",
        "Open_vSwitch",
        ".",
        "external_ids",
        "system-id",
        "ovn-remote",
        "ovn-encap-type",
        "ovn-encap-ip",
        "ovn-bridge",
        CHASSIS_TAG,
    ])
}

/// `ovs-vsctl set-ssl` for the chassis certificate the agent installed.
pub fn set_ssl(key: &str, cert: &str, ca: &str) -> Cmd {
    vsctl(["set-ssl", key, cert, ca])
}

/// Accept only a unicast MAC (a VF's address).
pub fn validate_mac(mac: &str) -> Result<(), DpuError> {
    if is_mac(mac) {
        Ok(())
    } else {
        Err(DpuError::invalid(format!("mac `{mac}`")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    const VM: &str = "1a2b3c4d-5e6f-4a1b-9c2d-0123456789ab";

    fn spec() -> DpuPortSpec {
        DpuPortSpec {
            vm_id: VM.into(),
            nic: 0,
            lsp: "gx-1a2b3c4d-0".into(),
            mac: "02:11:22:33:44:55".into(),
            mtu: 1442,
            queue_pairs: 4,
        }
    }

    #[test]
    fn a_representor_is_doca_with_no_options_tagged_and_bound() {
        let cmd = add_representor("br-int", "en3f0pf0sf3000", &spec()).unwrap();
        assert_eq!(
            cmd.display(),
            format!(
                "ovs-vsctl --may-exist add-port br-int en3f0pf0sf3000 -- set Interface en3f0pf0sf3000 \
                 type=doca external_ids:iface-id=gx-1a2b3c4d-0 external_ids:glidex-owner=glidex \
                 external_ids:glidex-role=vm external_ids:glidex-vm-id={VM} external_ids:glidex-nic=0"
            )
        );
        assert!(!cmd.display().contains("options:"));
        assert_eq!(
            link_up("en3f0pf0sf3000").unwrap().display(),
            "ip link set en3f0pf0sf3000 up"
        );
        assert!(add_representor("br-int", "en3f0pf0sf3000; x", &spec()).is_err());
        assert!(add_representor("br-int-with-a-long-name", "r", &spec()).is_err());
    }

    #[test]
    fn bridges_and_ports_are_doca_and_idempotent() {
        assert_eq!(
            ensure_bridge("ovsbr-phy", "doca", "phy").unwrap().display(),
            "ovs-vsctl --may-exist add-br ovsbr-phy -- set Bridge ovsbr-phy datapath_type=doca \
             external_ids:glidex-owner=glidex external_ids:glidex-role=phy"
        );
        assert!(
            ensure_br_int("br-int")
                .unwrap()
                .display()
                .ends_with("fail_mode=secure other-config:disable-in-band=true")
        );
        for dp in ["netdev", "dpdk", "system"] {
            assert_eq!(
                ensure_bridge("br-int", dp, "ovn-int").unwrap_err().code(),
                "invalid_argument"
            );
        }
        assert_eq!(
            add_doca_port("ovsbr-phy", "p0").unwrap().display(),
            "ovs-vsctl --may-exist add-port ovsbr-phy p0 -- set Interface p0 type=doca"
        );
        require_doca_port_type("doca").unwrap();
        assert!(require_doca_port_type("dpdk").is_err());
        assert!(require_doca_port_type("dpdkvhostuserclient").is_err());
    }

    #[test]
    fn tagged_ports_are_listed_and_only_ours_removed() {
        let exec = RecordingExec::new();
        exec.on(
            &format!("ovs-vsctl {}", LIST_PORTS.join(" ")),
            Output::ok(format!(
                r#"{{"data":[["en3f0pf0sf3001",["map",[["glidex-nic","1"],["glidex-owner","glidex"],["glidex-role","vm"],["glidex-vm-id","{VM}"],["iface-id","gx-1a2b3c4d-1"]]]],["en3f0pf0sf3000",["map",[["glidex-nic","0"],["glidex-owner","glidex"],["glidex-role","vm"],["glidex-vm-id","{VM}"]]]]],"headings":["name","external_ids"]}}"#
            )),
        );
        let ports = list_ports(&exec).unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0].name, "en3f0pf0sf3000");
        assert_eq!(ports[1].lsp.as_deref(), Some("gx-1a2b3c4d-1"));

        let find = "ovs-vsctl --format=json --columns=name,external_ids find Interface name=en3f0pf0sf3000";
        let exec = RecordingExec::new();
        exec.on(
            find,
            Output::ok(r#"{"data":[],"headings":["name","external_ids"]}"#),
        );
        assert!(!remove_representor(&exec, "en3f0pf0sf3000", VM, 0).unwrap());
        assert!(!exec.calls().iter().any(|c| c.contains("del-port")));

        let exec = RecordingExec::new();
        exec.on(
            find,
            Output::ok(
                r#"{"data":[["en3f0pf0sf3000",["map",[]]]],"headings":["name","external_ids"]}"#,
            ),
        );
        assert_eq!(
            remove_representor(&exec, "en3f0pf0sf3000", VM, 0)
                .unwrap_err()
                .code(),
            "not_owned"
        );

        let exec = RecordingExec::new();
        exec.on(
            find,
            Output::ok(format!(
                r#"{{"data":[["en3f0pf0sf3000",["map",[["glidex-nic","0"],["glidex-owner","glidex"],["glidex-vm-id","{VM}"]]]]],"headings":["name","external_ids"]}}"#
            )),
        );
        assert!(remove_representor(&exec, "en3f0pf0sf3000", VM, 0).unwrap());
        assert!(
            exec.calls()
                .contains(&"ovs-vsctl --if-exists del-port en3f0pf0sf3000".to_string())
        );
    }

    #[test]
    fn chassis_settings_have_no_bridge_mappings() {
        let s = ChassisSettings {
            system_id: "node:node-a-dpu".into(),
            sb_remotes: vec!["ssl:192.0.2.11:6642".into(), "ssl:192.0.2.12:6642".into()],
            encap_ip: "192.0.2.31".parse().unwrap(),
            br_int: "br-int".into(),
        };
        let cmds: Vec<String> = chassis_cmds(&s).unwrap().iter().map(Cmd::display).collect();
        assert_eq!(
            cmds,
            [
                "ovs-vsctl set Open_vSwitch . external_ids:system-id=node:node-a-dpu \
                 external_ids:ovn-remote=ssl:192.0.2.11:6642,ssl:192.0.2.12:6642 \
                 external_ids:ovn-encap-type=geneve external_ids:ovn-encap-ip=192.0.2.31 \
                 external_ids:ovn-bridge=br-int external_ids:glidex-chassis=1",
                "ovs-vsctl --if-exists remove Open_vSwitch . external_ids ovn-bridge-mappings",
            ]
        );
        for bad in [
            ChassisSettings {
                system_id: "node-a".into(),
                ..s.clone()
            },
            ChassisSettings {
                sb_remotes: vec![],
                ..s.clone()
            },
            ChassisSettings {
                sb_remotes: vec!["tcp:192.0.2.11:6642".into()],
                ..s.clone()
            },
        ] {
            assert!(chassis_cmds(&bad).is_err());
        }
        assert!(leave_cmds().display().contains("system-id ovn-remote"));
    }
}
