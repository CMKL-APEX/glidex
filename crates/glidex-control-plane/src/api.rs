use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
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
use crate::images::{CreateDiskRequest, ExtendRootRequest, ImageError, PullImageRequest, ResizeDiskRequest};
use crate::network::{CreateNetworkRequest, NetError, Netd, NetdAccess};
use crate::state::{VmManager, VmManagerError};
use glidex_netd::proto::{BridgeRecord, EnsureUplinkArgs, ErrorBody, Op, UplinkPhase, UplinkResult};
use glidex_ovs::uplink::{UplinkKind, UplinkSpec};
use glidex_ovs::bridge::BridgeSpec;
use glidex_ovs::install::{InstallReport, InstallRequest};
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
        .route("/networks", get(list_networks))
        .route("/networks", post(create_network))
        .route("/networks/{name}", get(get_network))
        .route("/networks/{name}", delete(delete_network))
        .route("/ovs/status", get(ovs_status))
        .route("/ovs/install", post(ovs_install))
        .route("/ovs/dpdk-init", post(ovs_dpdk_init))
        .route("/ovs/bridges", get(list_bridges))
        .route("/ovs/bridges", post(create_bridge))
        .route("/ovs/bridges/{name}", delete(delete_bridge))
        .route("/ovs/bridges/{name}/uplinks", get(list_uplinks))
        .route("/ovs/bridges/{name}/uplinks", post(create_uplink))
        .route("/ovs/bridges/{name}/uplinks/{uplink}", delete(delete_uplink))
        .route("/ovs/bridges/{name}/uplinks/{uplink}/commit", post(commit_uplink))
        .route("/vms/{id}/disks", post(attach_disk))
        .route("/vms/{id}/disks/{disk}", delete(detach_disk))
        .route("/images/catalog", get(image_catalog))
        .route("/images", get(list_images))
        .route("/images", post(pull_image))
        .route("/images/{id}", get(get_image))
        .route("/images/{id}", delete(delete_image))
        .route("/disks", get(list_disks))
        .route("/disks", post(create_disk))
        .route("/disks/{id}", get(get_disk))
        .route("/disks/{id}", delete(delete_disk))
        .route("/disks/{id}/resize", post(resize_disk))
        .route("/disks/{id}/extend-root", post(extend_root))
        .route("/pci-devices", get(list_pci_devices))
        .route("/health", get(health_check))
        .with_state(state)
}

type ApiResult<T> = Result<T, (StatusCode, Json<ApiError>)>;

// ---- images and disks (spec/images.md §8) ----------------------------------

async fn image_catalog(State(manager): State<AppState>) -> impl IntoResponse {
    Json(manager.image_catalog())
}

async fn list_images(State(manager): State<AppState>) -> impl IntoResponse {
    Json(manager.list_images())
}

async fn get_image(State(manager): State<AppState>, Path(id): Path<String>) -> ApiResult<impl IntoResponse> {
    Ok(Json(manager.get_image(&id).await.map_err(error_to_response)?))
}

/// `202` for a new download, `200` for one already in flight.
async fn pull_image(
    State(manager): State<AppState>,
    Json(req): Json<PullImageRequest>,
) -> ApiResult<impl IntoResponse> {
    let (img, created) = manager.pull_image(req).await.map_err(error_to_response)?;
    let status = if created { StatusCode::ACCEPTED } else { StatusCode::OK };
    Ok((status, Json(img)))
}

async fn delete_image(State(manager): State<AppState>, Path(id): Path<String>) -> ApiResult<impl IntoResponse> {
    manager.delete_image(&id).map_err(error_to_response)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_disks(State(manager): State<AppState>) -> impl IntoResponse {
    Json(manager.list_disks())
}

async fn get_disk(State(manager): State<AppState>, Path(id): Path<String>) -> ApiResult<impl IntoResponse> {
    Ok(Json(manager.get_disk(&id).await.map_err(error_to_response)?))
}

async fn create_disk(
    State(manager): State<AppState>,
    Json(req): Json<CreateDiskRequest>,
) -> ApiResult<impl IntoResponse> {
    let disk = manager.create_disk(req).await.map_err(error_to_response)?;
    Ok((StatusCode::CREATED, Json(disk)))
}

async fn resize_disk(
    State(manager): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ResizeDiskRequest>,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(manager.resize_disk(&id, req).await.map_err(error_to_response)?))
}

async fn extend_root(
    State(manager): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ExtendRootRequest>>,
) -> ApiResult<impl IntoResponse> {
    let mode = body.map(|Json(b)| b.mode).unwrap_or_default();
    Ok(Json(manager.extend_root(&id, mode).await.map_err(error_to_response)?))
}

async fn delete_disk(State(manager): State<AppState>, Path(id): Path<String>) -> ApiResult<impl IntoResponse> {
    manager.delete_disk(&id).await.map_err(error_to_response)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
struct AttachDiskRequest {
    disk: String,
}

async fn attach_disk(
    State(manager): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<AttachDiskRequest>,
) -> ApiResult<impl IntoResponse> {
    let vm = manager.attach_disk(&id, &req.disk).await.map_err(error_to_response)?;
    Ok(Json(VmResponse::from(&vm)))
}

async fn detach_disk(
    State(manager): State<AppState>,
    Path((id, disk)): Path<(String, String)>,
) -> ApiResult<impl IntoResponse> {
    let vm = manager.detach_disk(&id, &disk).await.map_err(error_to_response)?;
    Ok(Json(VmResponse::from(&vm)))
}

async fn list_networks(State(manager): State<AppState>) -> ApiResult<impl IntoResponse> {
    Ok(Json(manager.list_networks().map_err(error_to_response)?))
}

async fn get_network(State(manager): State<AppState>, Path(name): Path<String>) -> ApiResult<impl IntoResponse> {
    Ok(Json(manager.get_network(&name).map_err(error_to_response)?))
}

async fn create_network(
    State(manager): State<AppState>,
    Json(req): Json<CreateNetworkRequest>,
) -> ApiResult<impl IntoResponse> {
    let net = manager.create_network(req).await.map_err(error_to_response)?;
    Ok((StatusCode::CREATED, Json(net)))
}

async fn delete_network(State(manager): State<AppState>, Path(name): Path<String>) -> ApiResult<impl IntoResponse> {
    manager.delete_network(&name).await.map_err(error_to_response)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Run a blocking netd call off the async runtime.
async fn netd_blocking<T, F>(netd: Netd, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Netd) -> Result<T, NetError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&netd))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError::new("internal", e.to_string())),
            )
        })?
        .map_err(|e| error_to_response(VmManagerError::Network(e)))
}

#[derive(Serialize)]
struct OvsStatus {
    netd: NetdStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct NetdStatus {
    available: bool,
    access: NetdAccess,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn ovs_status(State(manager): State<AppState>) -> ApiResult<impl IntoResponse> {
    let netd = manager.netd().clone();
    let status = tokio::task::spawn_blocking(move || {
        let (access, result) = netd.probe();
        match result {
            Ok(host) => OvsStatus {
                netd: NetdStatus { available: true, access, error: None },
                host: Some(host),
            },
            Err(e) => OvsStatus {
                netd: NetdStatus { available: false, access, error: Some(e.to_string()) },
                host: None,
            },
        }
    })
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiError::new("internal", e.to_string()))))?;
    Ok(Json(status))
}

async fn ovs_install(
    State(manager): State<AppState>,
    Json(req): Json<InstallRequest>,
) -> ApiResult<impl IntoResponse> {
    let report: InstallReport =
        netd_blocking(manager.netd().clone(), move |n| n.call(Op::InstallOvs(req))).await?;
    Ok(Json(report))
}

async fn ovs_dpdk_init(
    State(manager): State<AppState>,
    Json(settings): Json<glidex_ovs::install::DpdkSettings>,
) -> ApiResult<impl IntoResponse> {
    netd_blocking(manager.netd().clone(), move |n| n.call::<serde_json::Value>(Op::InitDpdk(settings))).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_bridges(State(manager): State<AppState>) -> ApiResult<impl IntoResponse> {
    let bridges: Vec<BridgeRecord> = netd_blocking(manager.netd().clone(), |n| n.call(Op::ListBridges)).await?;
    Ok(Json(bridges))
}

async fn create_bridge(
    State(manager): State<AppState>,
    Json(spec): Json<BridgeSpec>,
) -> ApiResult<impl IntoResponse> {
    let bridge: BridgeRecord = netd_blocking(manager.netd().clone(), move |n| n.call(Op::EnsureBridge(spec))).await?;
    Ok((StatusCode::CREATED, Json(bridge)))
}

async fn delete_bridge(State(manager): State<AppState>, Path(name): Path<String>) -> ApiResult<impl IntoResponse> {
    if let Some(net) = manager
        .list_networks()
        .map_err(error_to_response)?
        .into_iter()
        .find(|n| n.bridge == name)
    {
        return Err(error_to_response(VmManagerError::Network(NetError::Conflict(format!(
            "bridge '{}' is used by network '{}'",
            name, net.name
        )))));
    }
    netd_blocking(manager.netd().clone(), move |n| {
        n.call::<serde_json::Value>(Op::DeleteBridge { name })
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_uplinks(State(manager): State<AppState>, Path(bridge): Path<String>) -> ApiResult<impl IntoResponse> {
    let all: Vec<UplinkResult> = netd_blocking(manager.netd().clone(), |n| n.call(Op::ListUplinks)).await?;
    Ok(Json(all.into_iter().filter(|u| u.record.spec.bridge == bridge).collect::<Vec<_>>()))
}

#[derive(serde::Deserialize)]
struct CreateUplinkRequest {
    name: String,
    #[serde(flatten)]
    kind: UplinkKind,
    #[serde(default)]
    migrate_ip: bool,
    #[serde(default)]
    confirm: bool,
}

async fn create_uplink(
    State(manager): State<AppState>,
    Path(bridge): Path<String>,
    Json(req): Json<CreateUplinkRequest>,
) -> ApiResult<impl IntoResponse> {
    let args = EnsureUplinkArgs {
        spec: UplinkSpec { name: req.name, bridge, kind: req.kind, migrate_ip: req.migrate_ip },
        confirm: req.confirm,
    };
    let res: UplinkResult = netd_blocking(manager.netd().clone(), move |n| n.call(Op::EnsureUplink(args))).await?;
    // 202: the IP migration must still be committed within the window.
    let status = match res.phase {
        UplinkPhase::PendingCommit => StatusCode::ACCEPTED,
        UplinkPhase::Active => StatusCode::CREATED,
    };
    Ok((status, Json(res)))
}

#[derive(serde::Deserialize)]
struct CommitRequest {
    token: String,
}

async fn commit_uplink(
    State(manager): State<AppState>,
    Path((bridge, name)): Path<(String, String)>,
    Json(req): Json<CommitRequest>,
) -> ApiResult<impl IntoResponse> {
    let res: UplinkResult = netd_blocking(manager.netd().clone(), move |n| {
        n.call(Op::CommitUplink { bridge, name, token: req.token })
    })
    .await?;
    Ok(Json(res))
}

async fn delete_uplink(
    State(manager): State<AppState>,
    Path((bridge, name)): Path<(String, String)>,
) -> ApiResult<impl IntoResponse> {
    netd_blocking(manager.netd().clone(), move |n| {
        n.call::<serde_json::Value>(Op::DeleteUplink { bridge, name })
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// REST status for an error reported by netd (spec §11.3).
fn netd_error_response(body: &ErrorBody) -> (StatusCode, Json<ApiError>) {
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
    let disks = request.disk_selection();
    let config = VmConfig::from(request);

    match manager.create_vm_with_disks(name, config, disks).await {
        Ok((vm, warnings)) => {
            let mut resp = VmResponse::from(&vm);
            resp.warnings = warnings;
            Ok((StatusCode::CREATED, Json(resp)))
        }
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

#[derive(serde::Deserialize, Default)]
struct DeleteVmQuery {
    #[serde(default)]
    keep_disk: bool,
}

async fn delete_vm(
    State(manager): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DeleteVmQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    match manager.delete_vm_with(&id, q.keep_disk).await {
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

/// Longest a stop request may wait for the guest to power off.
const MAX_GRACEFUL_STOP_SECS: u64 = 300;

#[derive(Debug, serde::Deserialize)]
struct StopParams {
    /// Press the guest's power button and wait up to this long for it to
    /// shut down before stopping it hard. Absent: stop immediately.
    graceful_timeout_secs: Option<u64>,
}

async fn stop_vm(
    State(manager): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<StopParams>,
) -> Result<impl IntoResponse, (StatusCode, Json<ApiError>)> {
    let result = match params.graceful_timeout_secs {
        Some(secs) => {
            let grace = std::time::Duration::from_secs(secs.min(MAX_GRACEFUL_STOP_SECS));
            manager.stop_vm_graceful(&id, grace).await
        }
        None => manager.stop_vm(&id).await,
    };
    match result {
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

fn error_to_response(error: VmManagerError) -> (StatusCode, Json<ApiError>) {
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
    }
}
