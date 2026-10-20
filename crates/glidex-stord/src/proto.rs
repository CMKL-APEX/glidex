//! Wire protocol between the control plane and glidex-stord, the same
//! shape as glidex-netd's (spec/networking.md §7.3): newline-delimited
//! JSON over a Unix socket, one request per line and one response per
//! line, one request in flight per connection, `hello` first.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_LINE: usize = 1 << 20;
pub const DEFAULT_RUN_DIR: &str = "/run/glidex-stord";
pub const SOCKET_NAME: &str = "stord.sock";
/// Second socket, `root:glidex-admin 0660`, bound only when the admin
/// group exists.
pub const ADMIN_SOCKET_NAME: &str = "stord-admin.sock";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
    /// Who the caller acts for. Logged, never used for authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Op {
    Hello {
        protocol: u32,
    },
    /// Which LVM tools there are.
    Probe,
    /// Whether `vg` is allowed and it and its thin pool exist; the pool's
    /// size and use.
    PoolStatus {
        vg: String,
        pool: String,
    },
    /// glidex's volumes in `vg`.
    ListVolumes {
        vg: String,
    },
    /// A thin volume of `size_bytes` (rounded up to the VG's extent),
    /// activated and handed to the glidex user.
    CreateVolume {
        vg: String,
        pool: String,
        name: String,
        size_bytes: u64,
    },
    /// A thin snapshot of image volume `origin`, grown to `size_bytes`,
    /// activated and handed to the glidex user.
    SnapshotVolume {
        vg: String,
        origin: String,
        name: String,
        size_bytes: u64,
    },
    /// Make an image volume read-only once it is filled.
    SealVolume {
        vg: String,
        name: String,
    },
    /// Grow a volume to `size_bytes`; never smaller.
    ExtendVolume {
        vg: String,
        name: String,
        size_bytes: u64,
    },
    Activate {
        vg: String,
        name: String,
    },
    Deactivate {
        vg: String,
        name: String,
    },
    /// Refused while it is open or a snapshot uses it.
    DeleteVolume {
        vg: String,
        name: String,
    },
    /// The block devices VM `vm_id`'s unit may open (replacing what it had;
    /// empty: none).
    AllowDevices {
        vm_id: String,
        devices: Vec<String>,
    },
}

impl Op {
    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Op::Hello { .. } | Op::Probe | Op::PoolStatus { .. } | Op::ListVolumes { .. }
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            Op::Hello { .. } => "hello",
            Op::Probe => "probe",
            Op::PoolStatus { .. } => "pool_status",
            Op::ListVolumes { .. } => "list_volumes",
            Op::CreateVolume { .. } => "create_volume",
            Op::SnapshotVolume { .. } => "snapshot_volume",
            Op::SealVolume { .. } => "seal_volume",
            Op::ExtendVolume { .. } => "extend_volume",
            Op::Activate { .. } => "activate",
            Op::Deactivate { .. } => "deactivate",
            Op::DeleteVolume { .. } => "delete_volume",
            Op::AllowDevices { .. } => "allow_devices",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: u32,
    pub stord_version: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeResult {
    /// `lvm version`'s first line; empty without LVM.
    pub lvm: String,
    pub thin_tools: bool,
}

/// Why a datastore's pool can't be used here (spec/storage.md §11.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PoolStatus {
    pub usable: bool,
    /// `NotAllowed`, `NoVolumeGroup` or `NoThinPool` when not usable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    pub size_bytes: u64,
    /// Bytes of the pool in use.
    pub used_bytes: u64,
    pub metadata_percent: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    pub name: String,
    pub size_bytes: u64,
    /// The image volume a snapshot was made from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub active: bool,
    pub open: bool,
    pub read_only: bool,
    /// Bytes of the pool it uses.
    pub used_bytes: u64,
}

/// An active volume: its device for the hypervisor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activated {
    /// `/dev/<vg>/<name>`.
    pub path: String,
    /// The device-mapper node it points at (`/dev/dm-N`).
    pub device: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
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
    pub fn ok(id: u64, v: Value) -> Self {
        Self {
            id,
            ok: Some(v),
            error: None,
        }
    }

    pub fn err(id: u64, e: ErrorBody) -> Self {
        Self {
            id,
            ok: None,
            error: Some(e),
        }
    }
}

/// What can go wrong; `code` goes on the wire.
#[derive(Debug, thiserror::Error)]
pub enum StordError {
    /// A malformed name or size: refused before any command runs.
    #[error("{0}")]
    Invalid(String),
    /// A volume group the operator didn't allow (stord.json `allowed_vgs`).
    #[error("{0}")]
    NotAllowed(String),
    #[error("{0}")]
    NotFound(String),
    /// Open, or used by a snapshot.
    #[error("{0}")]
    Busy(String),
    /// An LVM or systemd command failed.
    #[error("{0}")]
    Failed(String),
}

impl StordError {
    pub fn code(&self) -> &'static str {
        match self {
            StordError::Invalid(_) => "invalid_request",
            StordError::NotAllowed(_) => "not_allowed",
            StordError::NotFound(_) => "not_found",
            StordError::Busy(_) => "busy",
            StordError::Failed(_) => "failed",
        }
    }
}

impl From<&StordError> for ErrorBody {
    fn from(e: &StordError) -> Self {
        ErrorBody::new(e.code(), e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_have_netds_shape() {
        let r = Request {
            id: 7,
            op: Op::CreateVolume {
                vg: "vg0".into(),
                pool: "pool".into(),
                name: "gx-d-1".into(),
                size_bytes: 1 << 30,
            },
            on_behalf_of: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["op"], "create_volume");
        assert_eq!(v["args"]["vg"], "vg0");
        let back: Request = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
        assert!(!Op::ListVolumes { vg: "v".into() }.is_mutating());
        assert!(
            Op::AllowDevices {
                vm_id: "x".into(),
                devices: vec![]
            }
            .is_mutating()
        );
    }
}
