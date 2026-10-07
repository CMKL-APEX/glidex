//! OVS bridges owned by glidex.

use crate::exec::Exec;
use crate::host::HostCapabilities;
use crate::names::{validate_name, MAX_IFNAME};
use crate::vsctl::{self, owned_tags, tag_args, Row};
use crate::OvsError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Datapath {
    System,
    Netdev,
}

impl Datapath {
    pub fn as_str(self) -> &'static str {
        match self {
            Datapath::System => "system",
            Datapath::Netdev => "netdev",
        }
    }

    fn from_ovsdb(s: Option<&str>) -> Datapath {
        // An empty datapath_type means the default, "system".
        match s {
            Some("netdev") => Datapath::Netdev,
            _ => Datapath::System,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeSpec {
    pub name: String,
    pub datapath: Datapath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// Take ownership of an existing untagged bridge (adds tags only).
    #[serde(default)]
    pub adopt: bool,
    /// The bridge of an isolated network: fenced off from the host and
    /// from forwarding in `inet glidex`, and never given an uplink
    /// (security spec §8.4).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub isolated: bool,
}

impl BridgeSpec {
    pub fn validate(&self) -> Result<(), OvsError> {
        validate_name("bridge", &self.name, MAX_IFNAME)?;
        if let Some(mtu) = self.mtu {
            if !(576..=9216).contains(&mtu) {
                return Err(OvsError::invalid("mtu must be 576-9216"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeInfo {
    pub name: String,
    pub datapath: Datapath,
    pub owned: bool,
    pub ports: Vec<String>,
}

const COLUMNS: &[&str] = &["name", "datapath_type", "external_ids", "ports"];

fn info_from_row(exec: &dyn Exec, row: &Row) -> Result<BridgeInfo, OvsError> {
    let name = row.str("name").unwrap_or_default();
    Ok(BridgeInfo {
        datapath: Datapath::from_ovsdb(row.str("datapath_type").as_deref()),
        owned: row.owned_by_glidex(),
        ports: port_names(exec, &name)?,
        name,
    })
}

/// Port names on a bridge, excluding the bridge's own internal port.
fn port_names(exec: &dyn Exec, bridge: &str) -> Result<Vec<String>, OvsError> {
    let out = exec.check(&crate::exec::Cmd::new(
        crate::exec::Program::OvsVsctl,
        ["list-ports", bridge],
    ))?;
    Ok(out
        .stdout_str()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

pub fn get(exec: &dyn Exec, name: &str) -> Result<Option<BridgeInfo>, OvsError> {
    match vsctl::find_by_name(exec, "Bridge", name, COLUMNS)? {
        Some(row) => Ok(Some(info_from_row(exec, &row)?)),
        None => Ok(None),
    }
}

/// All bridges glidex owns.
pub fn list_owned(exec: &dyn Exec) -> Result<Vec<BridgeInfo>, OvsError> {
    vsctl::list(exec, "Bridge", COLUMNS)?
        .iter()
        .filter(|r| r.owned_by_glidex())
        .map(|r| info_from_row(exec, r))
        .collect()
}

/// Names of every bridge on the host, owned or not (for restart impact).
pub fn list_all_names(exec: &dyn Exec) -> Result<Vec<String>, OvsError> {
    let out = exec.check(&crate::exec::Cmd::new(crate::exec::Program::OvsVsctl, ["list-br"]))?;
    Ok(out
        .stdout_str()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Create the bridge, or bring an owned one in line with `spec`.
pub fn ensure(exec: &dyn Exec, caps: &HostCapabilities, spec: &BridgeSpec) -> Result<BridgeInfo, OvsError> {
    spec.validate()?;
    if spec.datapath == Datapath::Netdev
        && !caps.dpdk_initialized
        && !caps.iface_types.iter().any(|t| t == "afxdp")
    {
        return Err(OvsError::Unsupported {
            missing: vec!["dpdk initialized or afxdp (userspace datapath)".into()],
        });
    }

    let tags = tag_args(&owned_tags("bridge", &[]));
    match get(exec, &spec.name)? {
        Some(existing) if !existing.owned && !spec.adopt => {
            return Err(OvsError::not_owned(format!(
                "bridge '{}' exists and was not created by glidex (set adopt to take it over)",
                spec.name
            )));
        }
        Some(existing) if !existing.owned => {
            // Adopt: add tags, change nothing else.
            let mut args = vec!["set".to_string(), "Bridge".to_string(), spec.name.clone()];
            args.extend(tags);
            vsctl::run(exec, args)?;
        }
        Some(existing) => {
            if existing.datapath != spec.datapath {
                vsctl::run(
                    exec,
                    vec![
                        "set".into(),
                        "Bridge".into(),
                        spec.name.clone(),
                        format!("datapath_type={}", spec.datapath.as_str()),
                    ],
                )?;
            }
        }
        None => {
            let mut args = vec![
                "--may-exist".to_string(),
                "add-br".to_string(),
                spec.name.clone(),
                "--".to_string(),
                "set".to_string(),
                "Bridge".to_string(),
                spec.name.clone(),
                format!("datapath_type={}", spec.datapath.as_str()),
            ];
            args.extend(tags);
            vsctl::run(exec, args)?;
        }
    }
    if let Some(mtu) = spec.mtu {
        vsctl::run(
            exec,
            vec![
                "set".into(),
                "Interface".into(),
                spec.name.clone(),
                format!("mtu_request={}", mtu),
            ],
        )?;
    }
    get(exec, &spec.name)?.ok_or_else(|| OvsError::Io(format!("bridge {} vanished", spec.name)))
}

/// Delete an owned bridge. Absent bridges are fine; unowned ones are refused.
pub fn delete(exec: &dyn Exec, name: &str) -> Result<(), OvsError> {
    validate_name("bridge", name, MAX_IFNAME)?;
    match get(exec, name)? {
        None => Ok(()),
        Some(b) if !b.owned => Err(OvsError::not_owned(format!("bridge '{}'", name))),
        Some(_) => vsctl::run(exec, vec!["--if-exists".into(), "del-br".into(), name.into()]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    fn caps() -> HostCapabilities {
        HostCapabilities {
            ovs_running: true,
            ..Default::default()
        }
    }

    const FIND: &str = "ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge name=gxbr-nat";
    const EMPTY: &str = r#"{"data":[],"headings":["name","datapath_type","external_ids","ports"]}"#;
    const OWNED: &str = r#"{"data":[["gxbr-nat","system",["map",[["glidex-owner","glidex"],["glidex-role","bridge"]]],["set",[]]]],"headings":["name","datapath_type","external_ids","ports"]}"#;
    const FOREIGN: &str = r#"{"data":[["gxbr-nat","",["map",[]],["set",[]]]],"headings":["name","datapath_type","external_ids","ports"]}"#;

    fn spec() -> BridgeSpec {
        BridgeSpec {
            name: "gxbr-nat".into(),
            datapath: Datapath::System,
            mtu: None,
            adopt: false,
            isolated: false,
        }
    }

    #[test]
    fn creates_tagged_bridge() {
        let exec = RecordingExec::new();
        exec.on(FIND, Output::ok(EMPTY));
        exec.on(FIND, Output::ok(OWNED));
        let info = ensure(&exec, &caps(), &spec()).unwrap();
        assert!(info.owned);
        assert!(exec.calls().contains(&"ovs-vsctl --may-exist add-br gxbr-nat -- set Bridge gxbr-nat datapath_type=system external_ids:glidex-owner=glidex external_ids:glidex-role=bridge".to_string()));
    }

    #[test]
    fn existing_owned_bridge_is_a_no_op() {
        let exec = RecordingExec::new();
        exec.on(FIND, Output::ok(OWNED));
        ensure(&exec, &caps(), &spec()).unwrap();
        assert!(!exec.calls().iter().any(|c| c.contains("add-br") || c.contains(" set ")));
    }

    #[test]
    fn refuses_foreign_bridge_unless_adopted() {
        let exec = RecordingExec::new();
        exec.on(FIND, Output::ok(FOREIGN));
        assert!(matches!(ensure(&exec, &caps(), &spec()), Err(OvsError::NotOwned { .. })));
        assert!(matches!(delete(&exec, "gxbr-nat"), Err(OvsError::NotOwned { .. })));
        assert!(!exec.calls().iter().any(|c| c.contains("del-br")));

        let exec = RecordingExec::new();
        exec.on(FIND, Output::ok(FOREIGN));
        exec.on(FIND, Output::ok(OWNED));
        ensure(&exec, &caps(), &BridgeSpec { adopt: true, ..spec() }).unwrap();
        assert!(exec.calls().contains(&"ovs-vsctl set Bridge gxbr-nat external_ids:glidex-owner=glidex external_ids:glidex-role=bridge".to_string()));
        assert!(!exec.calls().iter().any(|c| c.contains("datapath_type=")), "adopt must not change the datapath");
    }

    #[test]
    fn netdev_needs_userspace_support() {
        let exec = RecordingExec::new();
        let err = ensure(&exec, &caps(), &BridgeSpec { datapath: Datapath::Netdev, ..spec() }).unwrap_err();
        assert!(matches!(err, OvsError::Unsupported { .. }));
        assert!(exec.calls().is_empty());
    }

    #[test]
    fn delete_owned_and_absent() {
        let exec = RecordingExec::new();
        exec.on(FIND, Output::ok(OWNED));
        delete(&exec, "gxbr-nat").unwrap();
        assert!(exec.calls().contains(&"ovs-vsctl --if-exists del-br gxbr-nat".to_string()));

        let exec = RecordingExec::new();
        exec.on(FIND, Output::ok(EMPTY));
        delete(&exec, "gxbr-nat").unwrap();
        assert!(!exec.calls().iter().any(|c| c.contains("del-br")));
    }
}
