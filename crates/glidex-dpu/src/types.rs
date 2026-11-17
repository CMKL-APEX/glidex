//! The types a DPU-backed port is described with (spec/31-dpu.md §5).

use crate::DpuError;
use serde::{Deserialize, Serialize};

/// Where a NIC is switched: the node's own `br-int`, or its DPU's
/// (decision 16). Recorded at attach and never changed while the VM runs
/// (decision 8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    #[default]
    Host,
    Dpu,
}

/// A VF of the DPU's static virtio-net PF, as `virtnet` numbers them
/// (`pf_id`, `vf_id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VfId {
    pub pf: u16,
    pub vf: u16,
}

/// The OVS datapath of a bridge on the DPU. Only `doca` is accepted there
/// (decision 11); the others are parsed so a probe can say what it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatapathType {
    Doca,
    System,
    Netdev,
}

impl DatapathType {
    pub fn as_str(self) -> &'static str {
        match self {
            DatapathType::Doca => "doca",
            DatapathType::System => "system",
            DatapathType::Netdev => "netdev",
        }
    }

    /// The datapath for a bridge on a DOCA DPU: `doca`, and nothing else.
    /// `netdev` (and the `dpdk` port type that goes with it) would take the
    /// ports away from the DOCA datapath the eswitch offloads to.
    pub fn require_doca(name: &str) -> Result<DatapathType, DpuError> {
        match name {
            "doca" => Ok(DatapathType::Doca),
            other => Err(DpuError::invalid(format!(
                "datapath `{other}` on a DOCA DPU; only `doca` is used there (spec/31-dpu.md decision 11)"
            ))),
        }
    }
}

/// The DPU's chassis name next to its node's `node:<id>` (decision 15):
/// `node:<id>-dpu`, which is also the CN its certificate carries for SB
/// RBAC (O19).
pub fn chassis_name(node_id: &str) -> String {
    format!("node:{node_id}-dpu")
}

/// The per-VF queue-pair ceiling glidex asks for when a NIC says nothing
/// (§7.1 step 1).
pub const DEFAULT_QUEUE_PAIRS: u8 = 4;

/// What the DPU needs to know about a NIC (§7.1 steps 4–5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpuPortSpec {
    pub vm_id: String,
    pub nic: u8,
    /// The logical port (`gx-<vm8>-<i>`), the representor's `iface-id`.
    pub lsp: String,
    pub mac: String,
    pub mtu: u16,
    #[serde(default = "default_queue_pairs")]
    pub queue_pairs: u8,
}

fn default_queue_pairs() -> u8 {
    DEFAULT_QUEUE_PAIRS
}

/// Names that end up in OVSDB values and command arguments.
fn is_safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub fn is_mac(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

impl DpuPortSpec {
    /// §7.1 step 1: no side effects. `max_queue_pairs` is the controller's
    /// per-VF maximum.
    pub fn validate(&self, max_queue_pairs: u8) -> Result<(), DpuError> {
        if !is_safe_name(&self.vm_id) {
            return Err(DpuError::invalid(format!("vm id `{}`", self.vm_id)));
        }
        if !is_safe_name(&self.lsp) {
            return Err(DpuError::invalid(format!("logical port `{}`", self.lsp)));
        }
        if !is_mac(&self.mac) {
            return Err(DpuError::invalid(format!("mac `{}`", self.mac)));
        }
        let first = u8::from_str_radix(&self.mac[..2], 16).unwrap_or(1);
        if first & 1 != 0 {
            return Err(DpuError::invalid(format!(
                "mac `{}` is a multicast address",
                self.mac
            )));
        }
        if !(68..=9216).contains(&self.mtu) {
            return Err(DpuError::invalid(format!("mtu {} (68 to 9216)", self.mtu)));
        }
        if self.queue_pairs == 0 || self.queue_pairs > max_queue_pairs {
            return Err(DpuError::invalid(format!(
                "{} queue pairs (1 to {max_queue_pairs})",
                self.queue_pairs
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DpuPortSpec {
        DpuPortSpec {
            vm_id: "0123abcd-0000-4000-8000-000000000001".into(),
            nic: 0,
            lsp: "gx-0123abcd-0".into(),
            mac: "02:11:22:33:44:55".into(),
            mtu: 1442,
            queue_pairs: 4,
        }
    }

    #[test]
    fn validates_a_port() {
        spec().validate(5).unwrap();
        for bad in [
            DpuPortSpec {
                mac: "03:11:22:33:44:55".into(),
                ..spec()
            },
            DpuPortSpec {
                mac: "02:11:22:33:44".into(),
                ..spec()
            },
            DpuPortSpec {
                lsp: "gx-0; rm".into(),
                ..spec()
            },
            DpuPortSpec {
                vm_id: "".into(),
                ..spec()
            },
            DpuPortSpec {
                mtu: 9300,
                ..spec()
            },
            DpuPortSpec {
                queue_pairs: 0,
                ..spec()
            },
            DpuPortSpec {
                queue_pairs: 6,
                ..spec()
            },
        ] {
            assert_eq!(bad.validate(5).unwrap_err().code(), "invalid_argument");
        }
    }

    #[test]
    fn only_doca_on_a_dpu() {
        assert_eq!(
            DatapathType::require_doca("doca").unwrap(),
            DatapathType::Doca
        );
        for bad in ["netdev", "dpdk", "system", ""] {
            assert!(DatapathType::require_doca(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn backend_defaults_to_host_and_names() {
        assert_eq!(Backend::default(), Backend::Host);
        assert_eq!(serde_json::to_string(&Backend::Dpu).unwrap(), "\"dpu\"");
        assert_eq!(chassis_name("node-a"), "node:node-a-dpu");
        let s: DpuPortSpec = serde_json::from_str(
            r#"{"vm_id":"v","nic":1,"lsp":"gx-v-1","mac":"02:00:00:00:00:01","mtu":1500}"#,
        )
        .unwrap();
        assert_eq!(s.queue_pairs, DEFAULT_QUEUE_PAIRS);
    }
}
