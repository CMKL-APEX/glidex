//! The `shim.sock` protocol (spec/reconciliation.md §8.5): newline-
//! delimited JSON, one request in flight per connection, `hello` first.

use serde::{Deserialize, Serialize};

/// Protocol versions this build speaks: the current one and the one before,
/// so shims started by the previous release stay manageable (§8.5).
pub const PROTOCOL_VERSION: u32 = 1;
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// Longest request line accepted.
pub const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
pub enum Op {
    Hello { protocol: u32 },
    Status,
    /// Resume if paused, press the power button, wait up to `grace_secs`,
    /// then kill. `grace_secs = 0` kills at once.
    Stop { grace_secs: u64 },
    Kill,
    /// Shut the console down and exit; only once the hypervisor is gone.
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(id: u64, v: serde_json::Value) -> Self {
        Self { id, ok: Some(v), error: None }
    }

    pub fn err(id: u64, code: &str, message: impl Into<String>) -> Self {
        Self { id, ok: None, error: Some(ErrorBody { code: code.into(), message: message.into() }) }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: u32,
    pub shim_version: String,
}

/// Error codes.
pub const E_PROTOCOL: &str = "protocol_error";
pub const E_INVALID: &str = "invalid_argument";
pub const E_NOT_EXITED: &str = "not_exited";
pub const E_INTERNAL: &str = "internal";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let r = Request { id: 3, op: Op::Stop { grace_secs: 30 } };
        assert_eq!(serde_json::to_value(&r).unwrap(), serde_json::json!({"id": 3, "op": "stop", "args": {"grace_secs": 30}}));
        let r: Request = serde_json::from_str(r#"{"id": 1, "op": "status"}"#).unwrap();
        assert_eq!(r.op, Op::Status);
        assert_eq!(
            serde_json::to_value(Response::err(2, E_NOT_EXITED, "running")).unwrap(),
            serde_json::json!({"id": 2, "error": {"code": "not_exited", "message": "running"}})
        );
    }
}
