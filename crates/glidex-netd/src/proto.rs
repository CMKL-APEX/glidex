//! Wire protocol between the control plane and glidex-netd (spec §7.3).
//!
//! Newline-delimited JSON over a Unix socket: one request per line, one
//! response per line, one request in flight per connection. The first
//! request on a connection must be `hello`.

use glidex_ovs::bridge::{BridgeInfo, BridgeSpec};
use glidex_ovs::install::InstallRequest;
use glidex_ovs::ipmigrate::Snapshot;
use glidex_ovs::nic::Classification;
use glidex_ovs::uplink::{UplinkSpec, UplinkState};
use glidex_ovs::nat::{NatSpec, NatState};
use glidex_ovs::vm_port::{VmPortBinding, VmPortSpec};
use glidex_ovs::OvsError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::net::Ipv4Addr;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_LINE: usize = 1 << 20;
pub const DEFAULT_RUN_DIR: &str = "/run/glidex";
pub const FULL_SOCKET_NAME: &str = "netd.sock";
pub const STATUS_SOCKET_NAME: &str = "netd-ro.sock";
/// Second full-access socket, `root:glidex-admin 0660`, bound only when
/// the admin group exists (security spec §4).
pub const ADMIN_SOCKET_NAME: &str = "netd-admin.sock";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
    /// Who the caller acts for. Logged, never used for authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<OnBehalfOf>,
}

/// Audit context the control plane attaches to a request: the user it
/// acts for (security spec §8.3). netd can't verify it, so it only goes
/// to the log next to the peer uid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnBehalfOf {
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Op {
    Hello { protocol: u32 },
    Probe,
    ListBridges,
    ListNat,
    ListVmPorts,
    ListUplinks,
    InstallOvs(InstallRequest),
    InitDpdk(glidex_ovs::install::DpdkSettings),
    EnsureUplink(EnsureUplinkArgs),
    CommitUplink { bridge: String, name: String, token: String },
    DeleteUplink { bridge: String, name: String },
    EnsureBridge(BridgeSpec),
    DeleteBridge { name: String },
    EnsureNat(NatSpec),
    DeleteNat { bridge: String },
    AttachVmPort(VmPortSpec),
    DetachVmPort { vm_id: String, nic_index: u8 },
    /// The VM is being deleted: free its NAT reservations too.
    ReleaseVm { vm_id: String },
    SyncVms { running: Vec<String> },
}

impl Op {
    /// Allowed on the world-accessible status socket (spec decision 7).
    pub fn is_status(&self) -> bool {
        matches!(self, Op::Hello { .. } | Op::Probe)
    }

    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Op::Hello { .. }
                | Op::Probe
                | Op::ListBridges
                | Op::ListNat
                | Op::ListVmPorts
                | Op::ListUplinks
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            Op::Hello { .. } => "hello",
            Op::Probe => "probe",
            Op::ListBridges => "list_bridges",
            Op::ListNat => "list_nat",
            Op::ListVmPorts => "list_vm_ports",
            Op::ListUplinks => "list_uplinks",
            Op::EnsureUplink(_) => "ensure_uplink",
            Op::CommitUplink { .. } => "commit_uplink",
            Op::DeleteUplink { .. } => "delete_uplink",
            Op::InstallOvs(_) => "install_ovs",
            Op::InitDpdk(_) => "init_dpdk",
            Op::EnsureBridge(_) => "ensure_bridge",
            Op::DeleteBridge { .. } => "delete_bridge",
            Op::EnsureNat(_) => "ensure_nat",
            Op::DeleteNat { .. } => "delete_nat",
            Op::AttachVmPort(_) => "attach_vm_port",
            Op::DetachVmPort { .. } => "detach_vm_port",
            Op::ReleaseVm { .. } => "release_vm",
            Op::SyncVms { .. } => "sync_vms",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
}

impl ErrorBody {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            details: Value::Null,
        }
    }
}

impl From<&OvsError> for ErrorBody {
    fn from(e: &OvsError) -> Self {
        // The error serializes as {"code": ..., <fields>}; the fields are the details.
        let mut details = serde_json::to_value(e).unwrap_or(Value::Null);
        if let Value::Object(map) = &mut details {
            map.remove("code");
            if map.is_empty() {
                details = Value::Null;
            }
        } else {
            details = Value::Null;
        }
        Self {
            code: e.code().to_string(),
            message: e.to_string(),
            details,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(id: u64, value: Value) -> Self {
        Self {
            id,
            ok: Some(value),
            error: None,
        }
    }

    pub fn err(id: u64, error: ErrorBody) -> Self {
        Self {
            id,
            ok: None,
            error: Some(error),
        }
    }
}

// ---- Result payloads -------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: u32,
    pub netd_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachResult {
    pub port: String,
    pub binding: VmPortBinding,
    /// Reserved address when the bridge is a NAT network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<Ipv4Addr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmPortRecord {
    pub spec: VmPortSpec,
    pub port: String,
    pub binding: VmPortBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<Ipv4Addr>,
    pub owner_uid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NatInfo {
    #[serde(flatten)]
    pub state: NatState,
    pub dnsmasq_running: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeRecord {
    pub spec: BridgeSpec,
    /// Live OVS state; `None` if the bridge is missing on the host.
    pub live: Option<BridgeInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnsureUplinkArgs {
    pub spec: UplinkSpec,
    /// Required to take over a NIC the host is using (CLI `--force`).
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub token: String,
    /// Unix seconds; rolled back if not committed by then.
    pub deadline: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UplinkRecord {
    pub spec: UplinkSpec,
    /// Driver the NIC had before a DPDK uplink rebound it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orig_driver: Option<String>,
    /// The NIC's IP configuration before migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Snapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Pending>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UplinkPhase {
    Active,
    /// IP migrated; call `commit_uplink` with `token` before `deadline`.
    PendingCommit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UplinkResult {
    pub record: UplinkRecord,
    pub phase: UplinkPhase,
    pub live: UplinkState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<Classification>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileReport {
    pub repaired: Vec<String>,
    pub detached: Vec<String>,
    /// glidex-tagged objects without a record: reported, never deleted.
    pub orphans: Vec<String>,
    pub errors: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r: Request = serde_json::from_str(r#"{"id":1,"op":"hello","args":{"protocol":1}}"#).unwrap();
        assert_eq!(r.op, Op::Hello { protocol: 1 });
        let r: Request = serde_json::from_str(r#"{"id":2,"op":"probe"}"#).unwrap();
        assert_eq!(r.op, Op::Probe);
        let r: Request = serde_json::from_str(
            r#"{"id":3,"op":"detach_vm_port","args":{"vm_id":"abc","nic_index":0}}"#,
        )
        .unwrap();
        assert_eq!(r.op.name(), "detach_vm_port");
        assert!(serde_json::from_str::<Request>(r#"{"id":4,"op":"rm_rf"}"#).is_err());
        assert_eq!(
            serde_json::to_string(&Request { id: 5, op: Op::ListNat, on_behalf_of: None }).unwrap(),
            r#"{"id":5,"op":"list_nat"}"#
        );
    }

    #[test]
    fn on_behalf_of_is_optional() {
        let r: Request = serde_json::from_str(
            r#"{"id":1,"op":"release_vm","args":{"vm_id":"abc"},"on_behalf_of":{"user":"alice","project":"p1","request_id":"r-9"}}"#,
        )
        .unwrap();
        assert_eq!(r.op, Op::ReleaseVm { vm_id: "abc".into() });
        let obo = r.on_behalf_of.clone().unwrap();
        assert_eq!(
            (obo.user.as_str(), obo.project.as_deref(), obo.request_id.as_deref()),
            ("alice", Some("p1"), Some("r-9"))
        );
        assert_eq!(serde_json::from_str::<Request>(&serde_json::to_string(&r).unwrap()).unwrap(), r);

        // Only `user` is required; absent fields aren't serialized.
        let r: Request = serde_json::from_str(r#"{"id":2,"op":"list_nat","on_behalf_of":{"user":"bob"}}"#).unwrap();
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"id":2,"op":"list_nat","on_behalf_of":{"user":"bob"}}"#);
        assert!(serde_json::from_str::<Request>(r#"{"id":3,"op":"list_nat","on_behalf_of":{}}"#).is_err());
    }

    #[test]
    fn classification() {
        assert!(Op::Probe.is_status() && !Op::Probe.is_mutating());
        assert!(!Op::ListBridges.is_status() && !Op::ListBridges.is_mutating());
        assert!(Op::SyncVms { running: vec![] }.is_mutating());
    }

    #[test]
    fn error_body_carries_details() {
        let body = ErrorBody::from(&OvsError::ConfirmationRequired { impact: "restart".into() });
        assert_eq!(body.code, "confirmation_required");
        assert_eq!(body.details["impact"], "restart");
        let body = ErrorBody::from(&OvsError::Io("x".into()));
        assert_eq!(body.code, "internal");
    }
}
