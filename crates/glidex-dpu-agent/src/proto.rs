//! The agent protocol (spec/31-dpu.md §10.2, §13): netd's NDJSON shape,
//! one request per line, over mutual TLS on the DPU's management link.
//! `{"id":1,"op":"attach_rep","args":{…}}` → `{"id":1,"ok":…}` or
//! `{"id":1,"error":{"code":…,"message":…}}`.

use glidex_dpu::{DpuError, DpuPortSpec, VfId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped on an incompatible change; an agent older than netd's is
/// `unsupported` (§10.2).
pub const AGENT_PROTOCOL_VERSION: u32 = 1;

/// The longest request line the agent reads.
pub const MAX_LINE: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRequest {
    pub id: u64,
    #[serde(flatten)]
    pub op: AgentOp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum AgentOp {
    Hello {
        protocol: u32,
    },
    /// `DpuCapabilities` (§9.2).
    Probe,
    /// The tagged VM ports on the DPU (§7.4).
    ListPorts,
    /// §7.1 steps 4–5: configure the VF, wait for its representor, put it
    /// on `bridge` bound to the NIC's logical port.
    AttachRep {
        vf: VfId,
        spec: DpuPortSpec,
        bridge: String,
        timeout_secs: u64,
    },
    /// §7.2: remove the NIC's representor port.
    DetachRep {
        vm_id: String,
        nic: u8,
    },
    /// §8.3: a CSR for the chassis from the key the DPU holds (made on
    /// first use); the key itself never leaves the DPU.
    Csr {
        chassis: String,
    },
    /// §8.3: the certificate the leader signed and the cluster CA.
    InstallCert {
        cert_pem: String,
        ca_pem: String,
    },
    /// §8.3: the chassis settings, `br-int` and the DPU's `ovn-controller`.
    Chassis(crate::chassis::ChassisArgs),
    /// The chassis's state.
    OvnStatus {
        br_int: String,
    },
    /// Stop being a chassis.
    LeaveOvn,
}

impl AgentOp {
    pub fn name(&self) -> &'static str {
        match self {
            AgentOp::Hello { .. } => "hello",
            AgentOp::Probe => "probe",
            AgentOp::ListPorts => "list_ports",
            AgentOp::AttachRep { .. } => "attach_rep",
            AgentOp::DetachRep { .. } => "detach_rep",
            AgentOp::Csr { .. } => "csr",
            AgentOp::InstallCert { .. } => "install_cert",
            AgentOp::Chassis(_) => "chassis",
            AgentOp::OvnStatus { .. } => "ovn_status",
            AgentOp::LeaveOvn => "leave_ovn",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl From<&DpuError> for ErrorBody {
    fn from(e: &DpuError) -> Self {
        ErrorBody {
            code: e.code().to_string(),
            message: e.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentResponse {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: u32,
    pub agent_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRepResult {
    pub representor: String,
    /// The VF's host PCI address, as the controller reports it (`61:02.3`).
    pub bdf: String,
}

/// What a call to the agent can fail with.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentError {
    /// Not reachable, TLS refused, or the connection broke.
    #[error("DPU agent unavailable: {0}")]
    Unavailable(String),
    #[error("DPU agent protocol error: {0}")]
    Protocol(String),
    #[error("DPU agent: {} ({})", .0.message, .0.code)]
    Remote(ErrorBody),
}

/// How netd reaches an agent: over TLS ([`crate::client::AgentClient`]), or,
/// in tests, in process.
pub trait AgentApi: Send + Sync {
    fn call(&self, op: AgentOp) -> Result<Value, AgentError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_have_netds_wire_shape() {
        let r = AgentRequest {
            id: 7,
            op: AgentOp::DetachRep {
                vm_id: "v".into(),
                nic: 1,
            },
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"id":7,"op":"detach_rep","args":{"vm_id":"v","nic":1}}"#
        );
        let p: AgentRequest = serde_json::from_str(r#"{"id":1,"op":"probe"}"#).unwrap();
        assert_eq!(p.op, AgentOp::Probe);
    }
}
