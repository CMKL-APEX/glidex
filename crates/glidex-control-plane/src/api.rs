use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::models::{ApiError, CreateVmRequest, DeviceRequest, VmConfig, VmResponse, VmState};
use crate::credentials::{
    CreateCredentialRequest, CredentialError, CredentialInfo, UpdateCredentialRequest,
};
use crate::hypervisor::HypervisorError;
use crate::state::{VmManager, VmManagerError};
use serde::Serialize;

pub type AppState = Arc<VmManager>;

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/vms", get(list_vms))
        .route("/vms", post(create_vm))
        .route("/vms/{id}", get(get_vm))
        .route("/vms/{id}", delete(delete_vm))
        .route("/vms/{id}/start", post(start_vm))
        .route("/vms/{id}/stop", post(stop_vm))
        .route("/vms/{id}/pause", post(pause_vm))
        .route("/vms/{id}/console", get(get_console_info))
        .route("/vms/{id}/console/ws", get(console_ws))
        .route("/vms/{id}/devices", post(attach_device))
        .route("/vms/{id}/devices", delete(detach_device))
        .route("/credentials", get(list_credentials))
        .route("/credentials", post(create_credential))
        .route("/credentials/{username}", get(get_credential))
        .route("/credentials/{username}", put(update_credential))
        .route("/credentials/{username}", delete(delete_credential))
        .route("/pci-devices", get(list_pci_devices))
        .route("/health", get(health_check))
        .with_state(state)
}

async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn list_vms(State(manager): State<AppState>) -> impl IntoResponse {
    let vms = manager.list_vms().await;
    let response: Vec<VmResponse> = vms.iter().map(VmResponse::from).collect();
    Json(response)
}

async fn create_vm(
    State(manager): State<AppState>,
    Json(request): Json<CreateVmRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let name = request.name.clone();
    let config = VmConfig::from(request);

    match manager.create_vm(name, config).await {
        Ok(vm) => Ok((StatusCode::CREATED, Json(VmResponse::from(&vm)))),
        Err(e) => Err(error_to_response(e)),
    }
}

async fn list_credentials(
    State(manager): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let creds = manager.list_credentials().map_err(error_to_response)?;
    Ok(Json(creds.iter().map(CredentialInfo::from).collect::<Vec<_>>()))
}

async fn create_credential(
    State(manager): State<AppState>,
    Json(request): Json<CreateCredentialRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let cred = manager.create_credential(request).map_err(error_to_response)?;
    Ok((StatusCode::CREATED, Json(CredentialInfo::from(&cred))))
}

async fn get_credential(
    State(manager): State<AppState>,
    Path(username): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let cred = manager.get_credential(&username).map_err(error_to_response)?;
    Ok(Json(CredentialInfo::from(&cred)))
}

async fn update_credential(
    State(manager): State<AppState>,
    Path(username): Path<String>,
    Json(request): Json<UpdateCredentialRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let cred = manager
        .update_credential(&username, request)
        .map_err(error_to_response)?;
    Ok(Json(CredentialInfo::from(&cred)))
}

async fn delete_credential(
    State(manager): State<AppState>,
    Path(username): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    manager
        .delete_credential(&username)
        .await
        .map_err(error_to_response)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_vm(
    State(manager): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    match manager.get_vm(&id).await {
        Ok(vm) => Ok(Json(VmResponse::from(&vm))),
        Err(e) => Err(error_to_response(e)),
    }
}

async fn delete_vm(
    State(manager): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    match manager.delete_vm(&id).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(error_to_response(e)),
    }
}

async fn start_vm(
    State(manager): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    match manager.start_vm(&id).await {
        Ok(vm) => Ok(Json(VmResponse::from(&vm))),
        Err(e) => Err(error_to_response(e)),
    }
}

async fn stop_vm(
    State(manager): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    match manager.stop_vm(&id).await {
        Ok(vm) => Ok(Json(VmResponse::from(&vm))),
        Err(e) => Err(error_to_response(e)),
    }
}

async fn pause_vm(
    State(manager): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    match manager.pause_vm(&id).await {
        Ok(vm) => Ok(Json(VmResponse::from(&vm))),
        Err(e) => Err(error_to_response(e)),
    }
}

#[derive(Debug, Serialize)]
struct ConsoleInfo {
    vm_id: String,
    console_socket_path: String,
    log_path: String,
    available: bool,
}

async fn get_console_info(
    State(manager): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    match manager.get_vm(&id).await {
        Ok(vm) => {
            let available = vm.state == VmState::Running;
            Ok(Json(ConsoleInfo {
                vm_id: vm.id,
                console_socket_path: vm.console_socket_path,
                log_path: vm.log_path,
                available,
            }))
        }
        Err(e) => Err(error_to_response(e)),
    }
}

async fn list_pci_devices() -> impl IntoResponse {
    let devices = crate::pci::scan_pci_devices();
    Json(devices)
}

async fn attach_device(
    State(manager): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<DeviceRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    match manager.attach_device(&id, request.device_path).await {
        Ok(vm) => Ok(Json(VmResponse::from(&vm))),
        Err(e) => Err(error_to_response(e)),
    }
}

async fn detach_device(
    State(manager): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<DeviceRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    match manager.detach_device(&id, &request.device_path).await {
        Ok(vm) => Ok(Json(VmResponse::from(&vm))),
        Err(e) => Err(error_to_response(e)),
    }
}

async fn console_ws(
    State(manager): State<AppState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let vm = match manager.get_vm(&id).await {
        Ok(vm) => vm,
        Err(VmManagerError::VmNotFound(_)) => {
            return (StatusCode::NOT_FOUND, "VM not found").into_response();
        }
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
        }
    };

    let console_path = vm.console_socket_path.clone();
    ws.on_upgrade(move |socket| bridge_console(socket, console_path))
}

/// Pump bytes in both directions between a browser WebSocket and the VM's
/// console Unix socket until either side closes. The console proxy thread in
/// the hypervisor backend keeps the listener alive even after the guest
/// exits, so connecting to a dead VM still succeeds and replays the log.
async fn bridge_console(mut ws: WebSocket, console_path: String) {
    let unix = match UnixStream::connect(&console_path).await {
        Ok(s) => s,
        Err(e) => {
            let _ = ws
                .send(Message::Text(
                    format!("Failed to connect to console socket {}: {}", console_path, e)
                        .into(),
                ))
                .await;
            let _ = ws.send(Message::Close(None)).await;
            return;
        }
    };
    let (mut unix_rx, mut unix_tx) = unix.into_split();
    let mut buf = [0u8; 4096];

    loop {
        tokio::select! {
            read = unix_rx.read(&mut buf) => {
                match read {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk: Vec<u8> = buf[..n].to_vec();
                        if ws.send(Message::Binary(chunk.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            msg = ws.recv() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if unix_tx.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if unix_tx.write_all(text.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

fn error_to_response(error: VmManagerError) -> (StatusCode, Json<ApiError>) {
    match &error {
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
    }
}
