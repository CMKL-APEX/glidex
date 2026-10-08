//! Mapping of manager errors to REST responses (spec/rest-api.md).

use crate::credentials::CredentialError;
use crate::hypervisor::HypervisorError;
use crate::images::ImageError;
use crate::models::ApiError;
use crate::network::NetError;
use crate::state::VmManagerError;
use crate::tenancy::TenancyError;
use axum::{http::StatusCode, Json};
use glidex_netd::proto::ErrorBody;

pub type ApiErr = (StatusCode, Json<ApiError>);

/// REST status for an error reported by netd (spec §11.3).
pub(crate) fn netd_error_response(body: &ErrorBody) -> (StatusCode, Json<ApiError>) {
    let (status, code) = match body.code.as_str() {
        "invalid_argument" => (StatusCode::BAD_REQUEST, "invalid_network"),
        "not_found" => (StatusCode::NOT_FOUND, "not_found"),
        "not_owned" | "conflict" => (StatusCode::CONFLICT, "conflict"),
        "host_interface_in_use" => (StatusCode::CONFLICT, "host_interface_in_use"),
        "confirmation_required" => (StatusCode::CONFLICT, "confirmation_required"),
        "migration_rolled_back" => (StatusCode::CONFLICT, "migration_rolled_back"),
        "unsupported" => (StatusCode::UNPROCESSABLE_ENTITY, "unsupported_on_host"),
        "permission_denied" => (StatusCode::SERVICE_UNAVAILABLE, "netd_permission_denied"),
        _ => (StatusCode::BAD_GATEWAY, "netd_error"),
    };
    (
        status,
        Json(ApiError::new(code, body.message.clone()).with_details(body.details.clone())),
    )
}

fn image_error_response(e: &ImageError) -> (StatusCode, Json<ApiError>) {
    let (status, code) = match e {
        ImageError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
        ImageError::AlreadyExists(_) | ImageError::InUse(_) | ImageError::Busy(_) | ImageError::NotReady(_) => {
            (StatusCode::CONFLICT, "conflict")
        }
        ImageError::InvalidImage(_) => (StatusCode::BAD_REQUEST, "invalid_image"),
        ImageError::InvalidDisk { .. } => (StatusCode::BAD_REQUEST, "invalid_disk"),
        // A host that is not set up, not a bug (like hypervisor_unavailable).
        ImageError::ToolMissing { .. } => (StatusCode::SERVICE_UNAVAILABLE, "tool_unavailable"),
        ImageError::Io(_) | ImageError::Tool { .. } | ImageError::Download(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "image_error")
        }
        ImageError::Storage(_) => (StatusCode::INTERNAL_SERVER_ERROR, "persistence_error"),
    };
    (status, Json(ApiError::new(code, e.to_string()).with_details(e.details())))
}

/// The error envelope a failed reconcile maps to under `?wait`
/// (spec/reconciliation.md §12.3, D20): the one today's synchronous call
/// would have returned, chosen from the `Ready` reason, with the VM in
/// `details.vm`.
pub fn failed_reconcile_response(vm: &crate::models::Vm) -> (StatusCode, Json<ApiError>) {
    let ready = vm.condition("Ready");
    let reason = ready.map(|c| c.reason.as_str()).unwrap_or("");
    let message = ready.map(|c| c.message.clone()).unwrap_or_default();
    let (status, code) = match reason {
        "NetdUnavailable" => (StatusCode::SERVICE_UNAVAILABLE, "netd_unavailable"),
        "ToolUnavailable" => (StatusCode::SERVICE_UNAVAILABLE, "tool_unavailable"),
        "CredentialError" => (StatusCode::INTERNAL_SERVER_ERROR, "credential_error"),
        "DiskBusy" => (StatusCode::CONFLICT, "conflict"),
        "DiskMissing" | "DiskNotReady" => (StatusCode::INTERNAL_SERVER_ERROR, "image_error"),
        _ if message.contains("is not installed") => (StatusCode::SERVICE_UNAVAILABLE, "hypervisor_unavailable"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "hypervisor_error"),
    };
    let details = serde_json::json!({ "reason": reason, "vm": crate::models::VmResponse::from(vm) });
    (status, Json(ApiError::new(code, message).with_details(details)))
}

pub fn error_to_response(error: VmManagerError) -> (StatusCode, Json<ApiError>) {
    match &error {
        VmManagerError::Image(e) => image_error_response(e),
        VmManagerError::Network(NetError::Netd(body)) => netd_error_response(body),
        VmManagerError::Network(NetError::Unavailable(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::new("netd_unavailable", error.to_string())),
        ),
        VmManagerError::Network(NetError::Protocol(_)) => (
            StatusCode::BAD_GATEWAY,
            Json(ApiError::new("netd_error", error.to_string())),
        ),
        VmManagerError::Network(NetError::Invalid(_)) => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("invalid_network", error.to_string())),
        ),
        VmManagerError::Network(NetError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", error.to_string())),
        ),
        VmManagerError::Network(NetError::Conflict(_)) => (
            StatusCode::CONFLICT,
            Json(ApiError::new("conflict", error.to_string())),
        ),
        VmManagerError::Network(NetError::ExternalPoolExhausted) => (
            StatusCode::CONFLICT,
            Json(ApiError::new("external_pool_exhausted", error.to_string())),
        ),
        VmManagerError::Network(NetError::Storage(_)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new("persistence_error", error.to_string())),
        ),
        VmManagerError::VmNotFound(_) => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", error.to_string())),
        ),
        VmManagerError::VmAlreadyExists(_) => (
            StatusCode::CONFLICT,
            Json(ApiError::new("conflict", error.to_string())),
        ),
        VmManagerError::InvalidState { .. } => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("invalid_state", error.to_string())),
        ),
        VmManagerError::HypervisorError(HypervisorError::InvalidConfig(_)) => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("invalid_config", error.to_string())),
        ),
        VmManagerError::HypervisorError(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new("hypervisor_error", error.to_string())),
        ),
        VmManagerError::PersistenceError(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new("persistence_error", error.to_string())),
        ),
        VmManagerError::Credential(CredentialError::NotFound(_)) => (
            StatusCode::NOT_FOUND,
            Json(ApiError::new("not_found", error.to_string())),
        ),
        VmManagerError::Credential(CredentialError::AlreadyExists(_))
        | VmManagerError::CredentialInUse { .. } => (
            StatusCode::CONFLICT,
            Json(ApiError::new("conflict", error.to_string())),
        ),
        VmManagerError::Credential(CredentialError::Invalid(_)) => (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new("invalid_credential", error.to_string())),
        ),
        VmManagerError::Credential(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError::new("credential_error", error.to_string())),
        ),
        VmManagerError::HypervisorNotAvailable(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError::new("hypervisor_unavailable", error.to_string())),
        ),
        VmManagerError::QuotaExceeded(over) => (
            StatusCode::FORBIDDEN,
            Json(ApiError::new("quota_exceeded", error.to_string()).with_details(serde_json::json!({ "quota": over }))),
        ),
        VmManagerError::Deleting(_) => (
            StatusCode::CONFLICT,
            Json(ApiError::new("conflict", error.to_string())),
        ),
        VmManagerError::PreconditionFailed { actual, .. } => (
            StatusCode::PRECONDITION_FAILED,
            Json(ApiError::new("precondition_failed", error.to_string()).with_details(serde_json::json!({ "resource_version": actual }))),
        ),
        VmManagerError::Tenancy(e) => {
            let (status, code) = match e {
                TenancyError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
                TenancyError::AlreadyExists(_) => (StatusCode::CONFLICT, "conflict"),
                TenancyError::Invalid(_) => (StatusCode::BAD_REQUEST, "invalid_project"),
                TenancyError::Storage(_) => (StatusCode::INTERNAL_SERVER_ERROR, "persistence_error"),
            };
            (status, Json(ApiError::new(code, error.to_string())))
        }
    }
}
