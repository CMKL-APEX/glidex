//! Transport-neutral operations (spec/tenancy.md §12.1, spec/grpc.md
//! G-D1).
//!
//! Each operation is one function here. It makes every decision: Cedar
//! requests, not-found versus forbidden, quotas, name rules and the store
//! write. The REST handlers in `api/` and the RPC adapters in `api/rpc/`
//! only decode a request, call one of these functions, and encode the
//! answer, so the two transports can't drift apart.

#![allow(clippy::result_large_err)] // ServiceError mirrors the REST error body, as api/ does

pub mod access;
pub mod cluster;
pub mod common;
pub mod directory;
pub mod federations;
pub mod host;
pub mod invitations;
pub mod login;
pub mod network;
pub mod node;
pub mod signing_keys;
pub mod storage;
pub mod system;
pub mod tenancy;
pub mod usage;
pub mod vm;
pub mod watch;

use crate::api::ApiErr;
use crate::api::errors::ApiError;
use axum::Json;
use axum::http::StatusCode;

/// The caller of an operation: principal, the operation's Cedar action,
/// request id and audit slot. Built by either transport.
pub type Call = crate::api::Caller;

/// An operation's failure, with the stable code both transports send
/// (`ApiError.error` in REST, `ErrorInfo.reason` in RPCs).
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceError {
    /// The REST status. RPCs map `code` (and, for codes they don't list,
    /// this status) to their own status (`api::rpc::rpc_code`).
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    /// JSON object or null, as REST's `details`.
    pub details: serde_json::Value,
}

pub type ServiceResult<T> = Result<T, ServiceError>;

impl ServiceError {
    pub fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        ServiceError {
            status,
            code: code.into(),
            message: message.into(),
            details: serde_json::Value::Null,
        }
    }

    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthenticated", message)
    }

    pub fn into_rest(self) -> ApiErr {
        (
            self.status,
            Json(ApiError::new(self.code, self.message).with_details(self.details)),
        )
    }
}

/// The existing `Caller` checks report REST errors; they carry everything
/// a `ServiceError` needs.
impl From<ApiErr> for ServiceError {
    fn from((status, Json(e)): ApiErr) -> Self {
        ServiceError {
            status,
            code: e.error,
            message: e.message,
            details: e.details,
        }
    }
}

impl From<ServiceError> for ApiErr {
    fn from(e: ServiceError) -> Self {
        e.into_rest()
    }
}
