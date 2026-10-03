//! VM routes. Creating a VM is a compound request (spec/security.md §7.4):
//! every disk, credential, network, PCI device and host path it names is
//! checked on its own.

use super::{err, vm_entities, ApiErr, Caller};
use crate::auth::Method;
use crate::authz::{Ent, EntitySet};
use crate::models::{CreateVmRequest, DeviceRequest, VmConfig, VmResponse, VmState};
use crate::state::VmManagerError;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn manager_err(e: VmManagerError) -> ApiErr {
    super::error_to_response(e)
}

/// The VM `id`, if the caller may see it (else 404), and its entities.
async fn visible_vm(c: &Caller, id: &str) -> Result<(crate::models::Vm, Ent, EntitySet), ApiErr> {
    let vm = c.manager().get_vm(id).await.map_err(manager_err)?;
    c.set_project(&vm.project);
    c.set_target(format!("vm:{}", vm.id));
    let (e, es) = vm_entities(&vm);
    c.require_visible("readVm", e.clone(), es.clone(), "VM")?;
    Ok((vm, e, es))
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ProjectFilter {
    #[serde(default)]
    project: Option<String>,
}

pub async fn list(c: Caller, Query(q): Query<ProjectFilter>) -> Result<impl IntoResponse, ApiErr> {
    let only = match &q.project {
        Some(p) => Some(c.target_project(Some(p))?),
        None => None,
    };
    let visible = c.visible_projects()?;
    let mut out: Vec<VmResponse> = Vec::new();
    for vm in c.manager().list_vms().await {
        if only.as_ref().is_some_and(|p| *p != vm.project) || !visible.contains(&vm.project) {
            continue;
        }
        let (e, es) = vm_entities(&vm);
        if c.allowed("readVm", e, es) {
            out.push(VmResponse::from(&vm));
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(out))
}

/// The PCI address in a VFIO device path (`/sys/bus/pci/devices/<bdf>`
/// or a bare `<bdf>`), validated.
pub(crate) fn device_bdf(path: &str) -> Result<String, ApiErr> {
    let bdf = path.strip_prefix("/sys/bus/pci/devices/").unwrap_or(path);
    let ok = bdf.len() == 12
        && bdf.char_indices().all(|(i, ch)| match i {
            4 | 7 => ch == ':',
            10 => ch == '.',
            _ => ch.is_ascii_hexdigit(),
        });
    if ok {
        Ok(bdf.to_ascii_lowercase())
    } else {
        Err(err(
            StatusCode::BAD_REQUEST,
            "invalid_config",
            format!("{} is not a PCI device (/sys/bus/pci/devices/DDDD:BB:DD.F)", path),
        ))
    }
}

/// A PCI device entity with the projects granted it in the config.
pub(crate) fn pci_entities(c: &Caller, bdf: &str) -> (Ent, EntitySet) {
    let projects = c.manager().projects();
    let grants: Vec<String> = c
        .auth()
        .config
        .pci
        .allow
        .iter()
        .filter(|g| g.bdf.eq_ignore_ascii_case(bdf))
        .flat_map(|g| g.projects.iter())
        .filter_map(|p| projects.resolve(p).ok().map(|p| p.id))
        .collect();
    let mut es = EntitySet::new();
    es.pci_device(bdf, &grants);
    (Ent::PciDevice(bdf.to_string()), es)
}

/// Whether a create request names a host path outside what glidex manages
/// (spec/security.md §7.4). Only the default firmware files count as
/// managed: a path into the disk or image directories would reach other
/// projects' disks or the shared base images.
fn names_host_path(req: &CreateVmRequest) -> bool {
    if !req.kernel_image_path.is_empty() || !req.rootfs_path.is_empty() || req.cloud_init_path.is_some() {
        return true;
    }
    match &req.firmware_path {
        None => false,
        Some(fw) => {
            let canon = |p: &std::path::Path| std::fs::canonicalize(p).ok();
            let wanted = canon(std::path::Path::new(fw));
            let defaults = [crate::hypervisor::HypervisorType::CloudHypervisor, crate::hypervisor::HypervisorType::Qemu]
                .iter()
                .filter_map(|t| t.default_firmware_path())
                .filter_map(|p| canon(&p))
                .collect::<Vec<_>>();
            !wanted.is_some_and(|w| defaults.contains(&w))
        }
    }
}

pub async fn create(c: Caller, Json(req): Json<CreateVmRequest>) -> Result<impl IntoResponse, ApiErr> {
    let project = c.target_project(req.project.as_deref())?;
    c.set_project(&project);
    let pent = Ent::Project(project.clone());
    let pes = super::project_entities(&project);
    c.require(pent.clone(), pes.clone())?;
    let with_project: &[(&'static str, Ent)] = &[("project", pent.clone())];

    let sel = req.disk_selection();
    for key in sel.data_disks.iter().chain(sel.root_disk.iter()) {
        if let Ok(d) = c.manager().get_disk(key).await {
            let (e, es) = super::disk_entities(&d.id, &d.project);
            c.require_action("useDisk", e, es, with_project)?;
        }
    }
    if let Some(username) = &req.credential {
        let (e, mut es) = super::credential_entities(&project, username);
        es.project(&project);
        c.require_action("useCredential", e, es, with_project)?;
    }
    if !req.networks.as_ref().is_none_or(|n| n.is_empty()) {
        c.require_action("attachNetwork", pent.clone(), pes.clone(), &[])?;
        for att in req.networks.iter().flatten() {
            if let Ok(n) = c.manager().get_network(&att.network) {
                let mut es = pes.clone();
                let e = super::add_network(&mut es, &n);
                c.require_action("useNetwork", e, es, with_project)?;
            }
        }
    }
    for dev in req.vfio_devices.iter().flatten() {
        let bdf = device_bdf(dev)?;
        let (e, mut es) = pci_entities(&c, &bdf);
        es.project(&project);
        c.require_action("usePciDevice", e, es, with_project)?;
    }
    if names_host_path(&req) {
        c.require_action("useHostPath", Ent::Host, EntitySet::new(), &[])?;
    }
    if let Some(image) = &sel.image {
        let mut es = EntitySet::new();
        es.image(image);
        c.require_action("readImage", Ent::Image(image.clone()), es, &[])?;
    }

    let name = req.name.clone();
    let config = VmConfig::from(req);
    let quota = c.quota_mode(&project);
    let (vm, warnings, over) = c
        .manager()
        .create_vm_in(&project, name, config, sel, quota)
        .await
        .map_err(manager_err)?;
    c.note_overruns(&over);
    c.set_target(format!("vm:{}", vm.id));
    let mut resp = VmResponse::from(&vm);
    resp.warnings = warnings;
    Ok((StatusCode::CREATED, Json(resp)))
}

pub async fn get_one(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    Ok(Json(VmResponse::from(&vm)))
}

#[derive(serde::Deserialize, Default)]
pub struct DeleteVmQuery {
    #[serde(default)]
    keep_disk: bool,
}

pub async fn delete_one(c: Caller, Path(id): Path<String>, Query(q): Query<DeleteVmQuery>) -> Result<impl IntoResponse, ApiErr> {
    visible_vm(&c, &id).await?;
    c.manager().delete_vm_with(&id, q.keep_disk).await.map_err(manager_err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn start(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    let quota = c.quota_mode(&vm.project);
    let (vm, over) = c.manager().start_vm_with(&id, quota).await.map_err(manager_err)?;
    c.note_overruns(&over);
    Ok(Json(VmResponse::from(&vm)))
}

/// Longest a stop request may wait for the guest to power off.
const MAX_GRACEFUL_STOP_SECS: u64 = 300;

#[derive(Debug, serde::Deserialize)]
pub struct StopParams {
    /// Press the guest's power button and wait up to this long for it to
    /// shut down before stopping it hard. Absent: stop immediately.
    graceful_timeout_secs: Option<u64>,
}

pub async fn stop(c: Caller, Path(id): Path<String>, Query(params): Query<StopParams>) -> Result<impl IntoResponse, ApiErr> {
    visible_vm(&c, &id).await?;
    let result = match params.graceful_timeout_secs {
        Some(secs) => {
            let grace = std::time::Duration::from_secs(secs.min(MAX_GRACEFUL_STOP_SECS));
            c.manager().stop_vm_graceful(&id, grace).await
        }
        None => c.manager().stop_vm(&id).await,
    };
    let vm = result.map_err(manager_err)?;
    Ok(Json(VmResponse::from(&vm)))
}

pub async fn pause(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    visible_vm(&c, &id).await?;
    let vm = c.manager().pause_vm(&id).await.map_err(manager_err)?;
    Ok(Json(VmResponse::from(&vm)))
}

#[derive(Debug, Serialize)]
struct ConsoleInfo {
    vm_id: String,
    available: bool,
    /// Relative URL of the console WebSocket.
    websocket: String,
}

pub async fn console_info(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    Ok(Json(ConsoleInfo {
        available: vm.state == VmState::Running,
        websocket: format!("/vms/{}/console/ws", vm.id),
        vm_id: vm.id,
    }))
}

pub async fn console_ticket(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    let ticket = c.auth().issue_ticket(&c.p, &vm.id);
    Ok(Json(serde_json::json!({ "ticket": ticket, "expires_in": crate::auth::TICKET_SECS })))
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct TicketQuery {
    #[serde(default)]
    ticket: Option<String>,
}

/// Browser sessions need a fresh single-use ticket (spec §5.6); local
/// peers and token holders don't (they're not browsers).
pub async fn console_ws(c: Caller, Path(id): Path<String>, Query(q): Query<TicketQuery>, ws: WebSocketUpgrade) -> Response {
    let vm = match visible_vm(&c, &id).await {
        Ok((vm, _, _)) => vm,
        Err(e) => return e.into_response(),
    };
    if matches!(c.p.method, Method::Pam | Method::Oidc)
        && !q.ticket.as_deref().is_some_and(|t| c.auth().redeem_ticket(&c.p, &vm.id, t))
    {
        return err(StatusCode::FORBIDDEN, "ticket_required", "open the console with a fresh ticket").into_response();
    }
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
                .send(Message::Text(format!("Failed to connect to the VM console: {}", e).into()))
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

#[derive(Debug, serde::Deserialize, Default)]
pub struct LogQuery {
    /// Return at most this many bytes from the end (default and cap 1 MiB).
    #[serde(default)]
    tail_bytes: Option<u64>,
}

/// The VM's captured console output (it can hold anything the guest
/// printed, so it needs `vm.console` like the live console).
pub async fn console_log(c: Caller, Path(id): Path<String>, Query(q): Query<LogQuery>) -> Result<impl IntoResponse, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    const CAP: u64 = 1 << 20;
    let tail = q.tail_bytes.unwrap_or(CAP).min(CAP);
    let path = vm.log_path.clone();
    let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(tail)))?;
        let mut buf = Vec::new();
        f.take(tail).read_to_end(&mut buf)?;
        Ok(buf)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", format!("console log: {}", e)))?;
    Ok(([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], bytes))
}

pub async fn attach_device(c: Caller, Path(id): Path<String>, Json(req): Json<DeviceRequest>) -> Result<impl IntoResponse, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    let bdf = device_bdf(&req.device_path)?;
    let (e, mut es) = pci_entities(&c, &bdf);
    es.project(&vm.project);
    c.require_action("usePciDevice", e, es, &[("project", Ent::Project(vm.project.clone()))])?;
    let vm = c.manager().attach_device(&id, req.device_path).await.map_err(manager_err)?;
    Ok(Json(VmResponse::from(&vm)))
}

pub async fn detach_device(c: Caller, Path(id): Path<String>, Json(req): Json<DeviceRequest>) -> Result<impl IntoResponse, ApiErr> {
    visible_vm(&c, &id).await?;
    let vm = c.manager().detach_device(&id, &req.device_path).await.map_err(manager_err)?;
    Ok(Json(VmResponse::from(&vm)))
}

#[derive(serde::Deserialize)]
pub struct AttachDiskRequest {
    disk: String,
}

pub async fn attach_disk(c: Caller, Path(id): Path<String>, Json(req): Json<AttachDiskRequest>) -> Result<impl IntoResponse, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    let d = c.manager().get_disk(&req.disk).await.map_err(manager_err)?;
    let (e, es) = super::disk_entities(&d.id, &d.project);
    c.require_action("useDisk", e, es, &[("project", Ent::Project(vm.project.clone()))])?;
    let vm = c.manager().attach_disk(&id, &req.disk).await.map_err(manager_err)?;
    Ok(Json(VmResponse::from(&vm)))
}

pub async fn detach_disk(c: Caller, Path((id, disk)): Path<(String, String)>) -> Result<impl IntoResponse, ApiErr> {
    visible_vm(&c, &id).await?;
    let vm = c.manager().detach_disk(&id, &disk).await.map_err(manager_err)?;
    Ok(Json(VmResponse::from(&vm)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bdf_validation() {
        assert_eq!(device_bdf("/sys/bus/pci/devices/0000:41:00.0").unwrap(), "0000:41:00.0");
        assert_eq!(device_bdf("0000:AB:1f.7").unwrap(), "0000:ab:1f.7");
        assert!(device_bdf("/dev/sda").is_err());
        assert!(device_bdf("/sys/bus/pci/devices/../../../etc").is_err());
        assert!(device_bdf("0000:41:00.0/../x").is_err());
    }
}
