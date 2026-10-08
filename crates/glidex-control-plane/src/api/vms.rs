//! VM routes. Creating a VM is a compound request (spec/security.md §7.4):
//! every disk, credential, network, PCI device and host path it names is
//! checked on its own.
//!
//! Writes change the VM's desired state and return at once (`202`), or,
//! with `?wait=<secs>`, once the VM controller has acted on them
//! (spec/reconciliation.md §12.3).

use super::{err, vm_entities, ApiErr, Caller};
use crate::auth::Method;
use crate::authz::{Ent, EntitySet};
use crate::models::{CreateVmRequest, DeviceRequest, PowerState, Vm, VmConfig, VmResponse};
use crate::state::{VmManagerError, VmOptions, VmPatch, Waited};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn manager_err(e: VmManagerError) -> ApiErr {
    super::error_to_response(e)
}

/// Longest `?wait` (and stop grace).
const MAX_WAIT_SECS: u64 = 300;

#[derive(Debug, serde::Deserialize, Default)]
pub struct WaitQuery {
    /// Hold the response until the controller has acted (§12.3).
    #[serde(default)]
    wait: Option<u64>,
}

/// `If-Match: <resource_version>` (§12.1).
fn if_match(headers: &HeaderMap) -> Result<Option<u64>, ApiErr> {
    match headers.get(axum::http::header::IF_MATCH) {
        None => Ok(None),
        Some(v) => v
            .to_str()
            .ok()
            .map(|s| s.trim().trim_matches('"'))
            .and_then(|s| s.parse().ok())
            .map(Some)
            .ok_or_else(|| err(StatusCode::BAD_REQUEST, "invalid_request", "If-Match must be a resource version")),
    }
}

/// The response to a spec write: `202` (or `ok` for a create) at once, or
/// with `?wait`, after the controller decided: converged → `ok`, failed →
/// the error the call would have returned synchronously (D20), timeout →
/// `202`.
async fn respond(c: &Caller, vm: Vm, wait: Option<u64>, ok: StatusCode, warnings: Vec<String>) -> Response {
    let body = |vm: &Vm| {
        let mut r = VmResponse::from(vm);
        r.warnings = warnings.clone();
        Json(r)
    };
    let Some(secs) = wait else {
        let status = if ok == StatusCode::CREATED { ok } else { StatusCode::ACCEPTED };
        return (status, body(&vm)).into_response();
    };
    match c.manager().wait_converged(&vm.id, vm.generation, Duration::from_secs(secs.min(MAX_WAIT_SECS))).await {
        Waited::Decided(v) if v.is_converged() => (ok, body(&v)).into_response(),
        Waited::Decided(v) => super::errors::failed_reconcile_response(&v).into_response(),
        Waited::Gone | Waited::TimedOut(None) => StatusCode::OK.into_response(),
        Waited::TimedOut(Some(v)) => (StatusCode::ACCEPTED, body(&v)).into_response(),
    }
}

/// The VM `id`, if the caller may see it (else 404), and its entities.
async fn visible_vm(c: &Caller, id: &str) -> Result<(Vm, Ent, EntitySet), ApiErr> {
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

/// Whether a create request names a host path (spec/security.md §7.4).
/// Firmware is a managed image (`firmware`) like the disks; a
/// `firmware_path` is a host path like any other.
fn names_host_path(req: &CreateVmRequest) -> bool {
    !req.kernel_image_path.is_empty() || !req.rootfs_path.is_empty() || req.cloud_init_path.is_some() || req.firmware_path.is_some()
}

pub async fn create(c: Caller, Query(w): Query<WaitQuery>, Json(req): Json<CreateVmRequest>) -> Result<Response, ApiErr> {
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
    for image in sel.image.iter().chain(sel.firmware.iter()) {
        let mut es = EntitySet::new();
        es.image(image);
        c.require_action("readImage", Ent::Image(image.clone()), es, &[])?;
    }

    let name = req.name.clone();
    let opts = VmOptions {
        power: req.power,
        restart_policy: req.restart_policy,
        on_host_boot: req.on_host_boot,
        stop_grace_secs: req.stop_grace_secs,
        node: req.node.clone(),
    };
    let config = VmConfig::from(req);
    let quota = c.quota_mode(&project);
    let (vm, warnings, over) = c
        .manager()
        .create_vm_with(&project, name, config, sel, opts, quota, &c.actor())
        .await
        .map_err(manager_err)?;
    c.note_overruns(&over);
    c.set_target(format!("vm:{}", vm.id));
    Ok(respond(&c, vm, w.wait, StatusCode::CREATED, warnings).await)
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ViewQuery {
    /// `full`: the stored `{meta, spec, status}` (§12.4).
    #[serde(default)]
    view: Option<String>,
}

pub async fn get_one(c: Caller, Path(id): Path<String>, Query(q): Query<ViewQuery>) -> Result<Response, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    if q.view.as_deref() == Some("full") {
        // The stored record carries host paths (disk files, kernel, seed in
        // the private runtime directory), PIDs and the boot id, which the
        // flat view leaves out: host readers only (spec/security.md §9).
        c.require_action("readSystemStatus", Ent::Host, EntitySet::new(), &[])?;
        return Ok(Json(serde_json::to_value(&vm).unwrap_or_default()).into_response());
    }
    Ok(Json(VmResponse::from(&vm)).into_response())
}

#[derive(serde::Deserialize, Default)]
pub struct DeleteVmQuery {
    #[serde(default)]
    keep_disk: bool,
    #[serde(default)]
    wait: Option<u64>,
}

/// `204` when the VM is gone at once (nothing ran), else `202` (or, with
/// `?wait`, `200` once the record is gone).
pub async fn delete_one(c: Caller, Path(id): Path<String>, Query(q): Query<DeleteVmQuery>) -> Result<Response, ApiErr> {
    visible_vm(&c, &id).await?;
    let gone = c.manager().delete_vm_as(&id, q.keep_disk, &c.actor()).await.map_err(manager_err)?;
    if gone {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    let Some(secs) = q.wait else {
        let vm = c.manager().get_vm(&id).await.ok();
        return Ok(match vm {
            Some(vm) => (StatusCode::ACCEPTED, Json(VmResponse::from(&vm))).into_response(),
            None => StatusCode::NO_CONTENT.into_response(),
        });
    };
    let gen = c.manager().get_vm(&id).await.map(|v| v.generation).unwrap_or(0);
    match c.manager().wait_converged(&id, gen, Duration::from_secs(secs.min(MAX_WAIT_SECS))).await {
        Waited::Gone | Waited::TimedOut(None) => Ok(StatusCode::OK.into_response()),
        Waited::Decided(vm) | Waited::TimedOut(Some(vm)) => Ok((StatusCode::ACCEPTED, Json(VmResponse::from(&*vm))).into_response()),
    }
}

async fn set_power(c: Caller, id: String, power: PowerState, grace: Option<u32>, wait: Option<u64>, headers: &HeaderMap) -> Result<Response, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    let quota = c.quota_mode(&vm.project);
    let (vm, over) = c
        .manager()
        .set_power(&id, power, grace, quota, &c.actor(), if_match(headers)?)
        .await
        .map_err(manager_err)?;
    c.note_overruns(&over);
    Ok(respond(&c, vm, wait, StatusCode::OK, Vec::new()).await)
}

pub async fn start(c: Caller, Path(id): Path<String>, Query(w): Query<WaitQuery>, headers: HeaderMap) -> Result<Response, ApiErr> {
    set_power(c, id, PowerState::Running, None, w.wait, &headers).await
}

#[derive(Debug, serde::Deserialize)]
pub struct StopParams {
    /// Press the guest's power button and wait up to this long for it to
    /// shut down before stopping it hard (`spec.stop_grace_secs`).
    graceful_timeout_secs: Option<u64>,
    #[serde(default)]
    wait: Option<u64>,
}

pub async fn stop(c: Caller, Path(id): Path<String>, Query(params): Query<StopParams>, headers: HeaderMap) -> Result<Response, ApiErr> {
    let grace = params.graceful_timeout_secs.map(|s| s.min(MAX_WAIT_SECS) as u32);
    set_power(c, id, PowerState::Stopped, grace, params.wait, &headers).await
}

pub async fn pause(c: Caller, Path(id): Path<String>, Query(w): Query<WaitQuery>, headers: HeaderMap) -> Result<Response, ApiErr> {
    set_power(c, id, PowerState::Paused, None, w.wait, &headers).await
}

/// `PATCH /vms/{id}`: a merge patch of `spec`, authorized per changed
/// field (spec §12.1): power → start/stop/pauseVm, devices → attach/
/// detachDevice + usePciDevice, data disks → attach/detachDisk + useDisk,
/// networks → attach/detachNetwork + useNetwork, credential → updateVm +
/// useCredential, anything else → updateVm.
pub async fn patch_one(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    headers: HeaderMap,
    Json(patch): Json<VmPatch>,
) -> Result<Response, ApiErr> {
    let vm = c.manager().get_vm(&id).await.map_err(manager_err)?;
    c.set_project(&vm.project);
    c.set_target(format!("vm:{}", vm.id));
    let (e, es) = vm_entities(&vm);
    c.require_readable("readVm", e.clone(), es.clone(), "VM")?;
    if let Some(fields) = patch.config.as_ref().map(|c| c.immutable_fields()).filter(|f| !f.is_empty()) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid_config", format!("immutable field(s): {}", fields.join(", "))));
    }
    let project = vm.project.clone();
    let with_project: &[(&'static str, Ent)] = &[("project", Ent::Project(project.clone()))];
    let mut update = false;
    if let Some(p) = patch.power {
        if p != vm.spec.power {
            let action = match p {
                PowerState::Running => "startVm",
                PowerState::Paused => "pauseVm",
                PowerState::Stopped => "stopVm",
            };
            c.require_action(action, e.clone(), es.clone(), &[])?;
        }
    }
    update |= patch.restart_policy.is_some_and(|r| r != vm.spec.restart_policy)
        || patch.on_host_boot.is_some_and(|h| h != vm.spec.on_host_boot)
        || patch.stop_grace_secs.is_some_and(|g| g != vm.spec.stop_grace_secs);
    if let Some(cp) = &patch.config {
        let cur = vm.config();
        update |= cp.vcpu_count.is_some_and(|v| v != cur.vcpu_count)
            || cp.mem_size_mib.is_some_and(|v| v != cur.mem_size_mib)
            || cp.kernel_args.as_ref().is_some_and(|v| *v != cur.kernel_args)
            || cp.hugepages.is_some_and(|v| v != cur.hugepages);
        if let Some(cred) = &cp.credential {
            update = true;
            if let Some(username) = cred.as_str() {
                let (ce, mut ces) = super::credential_entities(&project, username);
                ces.project(&project);
                c.require_action("useCredential", ce, ces, with_project)?;
            }
        }
        if let Some(devs) = &cp.vfio_devices {
            for d in devs.iter().filter(|d| !cur.vfio_devices.contains(d)) {
                c.require_action("attachDevice", e.clone(), es.clone(), &[])?;
                let bdf = device_bdf(d)?;
                let (pe, mut pes) = pci_entities(&c, &bdf);
                pes.project(&project);
                c.require_action("usePciDevice", pe, pes, with_project)?;
            }
            if cur.vfio_devices.iter().any(|d| !devs.contains(d)) {
                c.require_action("detachDevice", e.clone(), es.clone(), &[])?;
            }
        }
        if let Some(keys) = &cp.data_disks {
            let mut ids = Vec::new();
            for key in keys {
                if let Ok(d) = c.manager().get_disk(key).await {
                    if !cur.data_disks.contains(&d.id) {
                        c.require_action("attachDisk", e.clone(), es.clone(), &[])?;
                        let (de, des) = super::disk_entities(&d.id, &d.project);
                        c.require_action("useDisk", de, des, with_project)?;
                    }
                    ids.push(d.id);
                }
            }
            if cur.data_disks.iter().any(|d| !ids.contains(d)) {
                c.require_action("detachDisk", e.clone(), es.clone(), &[])?;
            }
        }
        if let Some(nets) = &cp.networks {
            let new: Vec<&str> = nets.iter().map(|a| a.network.as_str()).collect();
            if nets.iter().any(|a| !cur.networks.iter().any(|b| b.network == a.network)) {
                c.require_action("attachNetwork", e.clone(), es.clone(), &[])?;
                for att in nets.iter().filter(|a| !cur.networks.iter().any(|b| b.network == a.network)) {
                    if let Ok(n) = c.manager().get_network(&att.network) {
                        let mut nes = super::project_entities(&project);
                        let ne = super::add_network(&mut nes, &n);
                        c.require_action("useNetwork", ne, nes, with_project)?;
                    }
                }
            }
            if cur.networks.iter().any(|b| !new.contains(&b.network.as_str())) {
                c.require_action("detachNetwork", e.clone(), es.clone(), &[])?;
            }
        }
    }
    if update {
        c.require_action("updateVm", e.clone(), es.clone(), &[])?;
    }
    let quota = c.quota_mode(&project);
    let (vm, over) = c
        .manager()
        .patch_vm(&id, patch, quota, &c.actor(), if_match(&headers)?)
        .await
        .map_err(manager_err)?;
    c.note_overruns(&over);
    Ok(respond(&c, vm, w.wait, StatusCode::OK, Vec::new()).await)
}

pub async fn events(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    let events = c.manager().vm_events(&vm.id).map_err(manager_err)?;
    Ok(Json(serde_json::json!({ "events": events })))
}

/// `GET /system/reconcile` (§12.5).
pub async fn system_reconcile(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Host, EntitySet::new())?;
    let m = c.manager();
    let runner = m.runner();
    let bus = match runner.bus_connected().await {
        None => "n/a",
        Some(true) => "connected",
        Some(false) => "unreachable",
    };
    let netd = match m.netd().probe() {
        (crate::network::NetdAccess::Full, Ok(_)) => "connected",
        (crate::network::NetdAccess::Status, Ok(_)) => "status_only",
        _ => "unavailable",
    };
    let (pending, in_flight) = m.queue_stats();
    let unknown: Vec<String> = m
        .list_vms()
        .await
        .into_iter()
        .filter(|v| v.status.phase == crate::models::VmPhase::Unknown)
        .map(|v| v.id)
        .collect();
    Ok(Json(serde_json::json!({
        "runner": runner.kind_name(),
        "bus": bus,
        "netd": netd,
        "queue": { "pending": pending, "in_flight": in_flight },
        "orphans": m.orphans(),
        "unknown_vms": unknown,
    })))
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
        // While an instance exists, including after the guest exited and
        // before the shim is released.
        available: vm.status.instance.is_some(),
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
    // A VM on another node: the console is relayed through that node (§8.4).
    if let Some(addr) = c.manager().remote_node_addr(&vm) {
        let Some(cluster) = c.manager().cluster() else { return err(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "no cluster").into_response() };
        return match cluster.client.upgrade(&addr, &format!("/cluster/v1/vms/{}/console", vm.id)).await {
            Ok(stream) => ws.on_upgrade(move |socket| bridge_stream(socket, stream)),
            Err(e) => err(StatusCode::BAD_GATEWAY, "node_unreachable", format!("the VM's node can't be reached: {e}")).into_response(),
        };
    }
    let console_path = vm.paths().console_socket;
    ws.on_upgrade(move |socket| bridge_console(socket, console_path))
}

/// Pump bytes in both directions between a browser WebSocket and the VM's
/// console Unix socket until either side closes. The shim keeps the
/// listener alive even after the guest exits, so connecting to a dead VM
/// still succeeds and replays the log.
async fn bridge_console(mut ws: WebSocket, console_path: String) {
    let unix = match UnixStream::connect(&console_path).await {
        Ok(s) => s,
        Err(e) => {
            let _ = ws.send(Message::Text(format!("Failed to connect to the VM console: {}", e).into())).await;
            let _ = ws.send(Message::Close(None)).await;
            return;
        }
    };
    bridge_stream(ws, unix).await
}

/// The same, over any byte stream (a local socket, or a node relay).
pub(crate) async fn bridge_stream<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut ws: WebSocket, stream: S) {
    let (mut unix_rx, mut unix_tx) = tokio::io::split(stream);
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
    /// The rotated log (`console.log.1`) instead (§8.6).
    #[serde(default)]
    previous: bool,
}

/// The VM's captured console output (it can hold anything the guest
/// printed, so it needs `vm.console` like the live console).
pub async fn console_log(c: Caller, Path(id): Path<String>, Query(q): Query<LogQuery>) -> Result<Response, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    const CAP: u64 = 1 << 20;
    let tail = q.tail_bytes.unwrap_or(CAP).min(CAP);
    if let Some(addr) = c.manager().remote_node_addr(&vm) {
        let Some(cluster) = c.manager().cluster() else { return Err(err(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "no cluster")) };
        let path = format!("/cluster/v1/vms/{}/console-log?tail_bytes={}&previous={}", vm.id, tail, q.previous);
        let r = cluster.client.request(&addr, axum::http::Method::GET, &path, &[], bytes::Bytes::new()).await.map_err(|e| err(StatusCode::BAD_GATEWAY, "node_unreachable", e.to_string()))?;
        return if r.status.is_success() {
            Ok(([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], r.body.to_vec()).into_response())
        } else {
            Err(err(r.status, "not_found", "no console log"))
        };
    }
    let paths = vm.paths();
    let path = if q.previous { paths.previous_log() } else { paths.log };
    let previous = q.previous;
    let bytes = read_log_tail(path, tail, previous)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", format!("console log: {}", e)))?;
    match bytes {
        Some(b) => Ok(([(axum::http::header::CONTENT_TYPE, "application/octet-stream")], b).into_response()),
        None => Err(err(StatusCode::NOT_FOUND, "not_found", "no rotated console log")),
    }
}

/// The last `tail` bytes of a console log; `None` for a rotated log that
/// doesn't exist.
pub(crate) async fn read_log_tail(path: String, tail: u64, previous: bool) -> std::io::Result<Option<Vec<u8>>> {
    tokio::task::spawn_blocking(move || -> std::io::Result<Option<Vec<u8>>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(if previous { None } else { Some(Vec::new()) }),
            Err(e) => return Err(e),
        };
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(tail)))?;
        let mut buf = Vec::new();
        f.take(tail).read_to_end(&mut buf)?;
        Ok(Some(buf))
    })
    .await
    .map_err(std::io::Error::other)?
}

pub async fn attach_device(c: Caller, Path(id): Path<String>, Query(w): Query<WaitQuery>, Json(req): Json<DeviceRequest>) -> Result<Response, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    let bdf = device_bdf(&req.device_path)?;
    let (e, mut es) = pci_entities(&c, &bdf);
    es.project(&vm.project);
    c.require_action("usePciDevice", e, es, &[("project", Ent::Project(vm.project.clone()))])?;
    let vm = c.manager().attach_device(&id, req.device_path).await.map_err(manager_err)?;
    Ok(respond(&c, vm, w.wait, StatusCode::OK, Vec::new()).await)
}

pub async fn detach_device(c: Caller, Path(id): Path<String>, Query(w): Query<WaitQuery>, Json(req): Json<DeviceRequest>) -> Result<Response, ApiErr> {
    visible_vm(&c, &id).await?;
    let vm = c.manager().detach_device(&id, &req.device_path).await.map_err(manager_err)?;
    Ok(respond(&c, vm, w.wait, StatusCode::OK, Vec::new()).await)
}

#[derive(serde::Deserialize)]
pub struct AttachDiskRequest {
    disk: String,
}

pub async fn attach_disk(c: Caller, Path(id): Path<String>, Query(w): Query<WaitQuery>, Json(req): Json<AttachDiskRequest>) -> Result<Response, ApiErr> {
    let (vm, _, _) = visible_vm(&c, &id).await?;
    let d = c.manager().get_disk(&req.disk).await.map_err(manager_err)?;
    let (e, es) = super::disk_entities(&d.id, &d.project);
    c.require_action("useDisk", e, es, &[("project", Ent::Project(vm.project.clone()))])?;
    let vm = c.manager().attach_disk(&id, &req.disk).await.map_err(manager_err)?;
    Ok(respond(&c, vm, w.wait, StatusCode::OK, Vec::new()).await)
}

pub async fn detach_disk(c: Caller, Path((id, disk)): Path<(String, String)>, Query(w): Query<WaitQuery>) -> Result<Response, ApiErr> {
    visible_vm(&c, &id).await?;
    let vm = c.manager().detach_disk(&id, &disk).await.map_err(manager_err)?;
    Ok(respond(&c, vm, w.wait, StatusCode::OK, Vec::new()).await)
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

    #[test]
    fn if_match_parses_versions() {
        let mut h = HeaderMap::new();
        assert_eq!(if_match(&h).unwrap(), None);
        h.insert(axum::http::header::IF_MATCH, "\"7\"".parse().unwrap());
        assert_eq!(if_match(&h).unwrap(), Some(7));
        h.insert(axum::http::header::IF_MATCH, "x".parse().unwrap());
        assert!(if_match(&h).is_err());
    }
}
