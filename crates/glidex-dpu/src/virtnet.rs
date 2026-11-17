//! The virtio-net controller's CLI, `virtnet` (spec/31-dpu.md §5.3, §7.1
//! step 4; finding 35).

use crate::DpuError;
use crate::exec::{Cmd, Program};
use crate::types::{DpuPortSpec, VfId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `virtnet list`: the controller and the functions it serves.
pub fn list_cmd() -> Cmd {
    Cmd::new(Program::Virtnet, ["list"])
}

/// §7.1 step 4: the VF's MAC, MTU and queue pairs, set before the host
/// binds a driver to it.
pub fn modify_vf_cmd(vf: VfId, spec: &DpuPortSpec) -> Result<Cmd, DpuError> {
    spec.validate(u8::MAX)?;
    Ok(Cmd::new(
        Program::Virtnet,
        [
            "modify".to_string(),
            "-p".into(),
            vf.pf.to_string(),
            "-v".into(),
            vf.vf.to_string(),
            "device".into(),
            "-m".into(),
            spec.mac.clone(),
            "-t".into(),
            spec.mtu.to_string(),
            "-qp".into(),
            spec.queue_pairs.to_string(),
        ],
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionType {
    StaticPf,
    HotplugPf,
    Vf,
}

/// One function the controller serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtnetDevice {
    pub function: FunctionType,
    pub pf: u16,
    /// For a VF.
    pub vf: Option<u16>,
    /// The host's PCI address, as the controller prints it (`61:02.3`).
    pub bdf: String,
    pub mac: Option<String>,
    pub mtu: Option<u16>,
    pub max_queue_pairs: Option<u16>,
    /// A PF's VF ceiling (`pci_max_vfs`) and how many are enabled.
    pub max_vfs: Option<u16>,
    pub enabled_vfs: Option<u16>,
    /// The function's SF representor on the DPU (`en3f0pf0sf3000`).
    pub representor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtnetList {
    pub controller_version: Option<String>,
    pub devices: Vec<VirtnetDevice>,
}

impl VirtnetList {
    pub fn pfs(&self) -> impl Iterator<Item = &VirtnetDevice> {
        self.devices
            .iter()
            .filter(|d| d.function != FunctionType::Vf)
    }

    pub fn vf(&self, id: VfId) -> Option<&VirtnetDevice> {
        self.devices
            .iter()
            .find(|d| d.function == FunctionType::Vf && d.pf == id.pf && d.vf == Some(id.vf))
    }

    /// The representor of `id`, read from the controller rather than
    /// computed (§5.3).
    pub fn representor(&self, id: VfId) -> Option<&str> {
        self.vf(id)?.representor.as_deref()
    }
}

/// The controller prints numbers as strings or numbers, depending on the
/// field.
fn num(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn str_of(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string).filter(|s| !s.is_empty())
}

/// Parse `virtnet list` (or `virtnet query`). The CLI first echoes its
/// request (`{"rpc_timeout_sec":100}`), then prints the answer: the last
/// JSON object with `devices` is the one read.
pub fn parse_list(out: &str) -> Result<VirtnetList, DpuError> {
    let answer = serde_json::Deserializer::from_str(out)
        .into_iter::<Value>()
        .map_while(Result::ok)
        .filter(|v| v.get("devices").is_some())
        .last()
        .ok_or_else(|| DpuError::parse("virtnet list", "no `devices` in the output"))?;
    let devices = answer["devices"]
        .as_array()
        .ok_or_else(|| DpuError::parse("virtnet list", "`devices` is not a list"))?
        .iter()
        .map(|d| {
            let function = match d["function_type"].as_str().unwrap_or_default() {
                "static PF" => FunctionType::StaticPf,
                "hotplug PF" => FunctionType::HotplugPf,
                "VF" => FunctionType::Vf,
                other => {
                    return Err(DpuError::parse(
                        "virtnet list",
                        format!("function type `{other}`"),
                    ));
                }
            };
            let pf = if function == FunctionType::Vf {
                num(&d["parent_pf_id"])
            } else {
                num(&d["pf_id"])
            }
            .ok_or_else(|| DpuError::parse("virtnet list", "a function without its PF id"))?;
            Ok(VirtnetDevice {
                function,
                pf: pf as u16,
                vf: (function == FunctionType::Vf)
                    .then(|| num(&d["vf_id"]).map(|v| v as u16))
                    .flatten(),
                bdf: str_of(&d["pci_bdf"]).unwrap_or_default(),
                mac: str_of(&d["mac"]),
                mtu: num(&d["mtu"]).map(|v| v as u16),
                max_queue_pairs: num(&d["max_queue_pairs"]).map(|v| v as u16),
                max_vfs: num(&d["pci_max_vfs"]).map(|v| v as u16),
                enabled_vfs: num(&d["enabled_vfs"]).map(|v| v as u16),
                representor: str_of(&d["sf_rep_net_device"]),
            })
        })
        .collect::<Result<Vec<_>, DpuError>>()?;
    Ok(VirtnetList {
        controller_version: answer.pointer("/controller/version").and_then(str_of),
        devices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modify_sets_mac_mtu_and_queue_pairs() {
        let spec = DpuPortSpec {
            vm_id: "v".into(),
            nic: 0,
            lsp: "gx-v-0".into(),
            mac: "02:11:22:33:44:55".into(),
            mtu: 1442,
            queue_pairs: 4,
        };
        assert_eq!(
            modify_vf_cmd(VfId { pf: 0, vf: 3 }, &spec)
                .unwrap()
                .display(),
            "virtnet modify -p 0 -v 3 device -m 02:11:22:33:44:55 -t 1442 -qp 4"
        );
        assert_eq!(list_cmd().display(), "virtnet list");
    }

    #[test]
    fn a_list_without_devices_is_an_error() {
        assert!(parse_list("{\"rpc_timeout_sec\":100}\n").is_err());
        assert!(parse_list("").is_err());
    }
}
