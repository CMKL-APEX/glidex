//! VM routes: the REST twins of `glidex.vm.v1` (spec/grpc.md §9.1), the
//! proto messages as JSON with path, query and header parameters merged
//! in, on the same `service::vm` functions as the RPCs; the console
//! WebSocket (spec/console.md), which stays one until G4; the stored
//! record (`?view=full`); and `/system/reconcile`.
//!
//! Writes change the VM's desired state and return at once (`202`), or,
//! with `?wait=<secs>`, once the VM controller has acted on them
//! (spec/reconciliation.md §12.3).

use super::{ApiErr, Caller, Proto, err, proto_body, proto_no_content, vm_entities};
use crate::authz::{Ent, EntitySet};
use crate::models::{Vm, VmResponse};
use crate::service::vm::{self as svc, VmReply};
use axum::{
    Json,
    extract::{
        Path, Query,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use glidex_api::proto::glidex::vm::v1 as pb;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn manager_err(e: crate::state::VmManagerError) -> ApiErr {
    super::error_to_response(e)
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct WaitQuery {
    /// Hold the response until the controller has acted (§12.3).
    #[serde(default)]
    wait: Option<u64>,
}

fn secs(w: Option<u64>) -> Option<u32> {
    w.map(|w| w.min(u64::from(u32::MAX)) as u32)
}

/// The old shape's enum values (spec/grpc.md §9.1 step 3).
const ENUMS: &[(&str, &str)] = &[
    ("hypervisor", "HYPERVISOR"),
    ("power", "POWER_STATE"),
    ("restart_policy", "RESTART_POLICY"),
    ("on_host_boot", "HOST_BOOT_POLICY"),
];
const RENAMES: &[(&str, &str, &str)] = &[(
    "hypervisor",
    "cloudhypervisor",
    "HYPERVISOR_CLOUD_HYPERVISOR",
)];

fn body<T: serde::de::DeserializeOwned + buffa::MessageName>(v: Value) -> Result<T, ApiErr> {
    proto_body(v, ENUMS, RENAMES)
}

/// The answer to a VM write: at once `immediate` (`201` for a create,
/// else `202`); with `?wait`, `ok` once the controller converged, `202`
/// when the time ran out; `204` once the VM is gone. The body is `{vm,
/// settled}` (every write's response has that shape).
fn reply(r: VmReply, ok: StatusCode, immediate: StatusCode) -> Response {
    let status = match &r {
        VmReply::Gone => return proto_no_content(),
        VmReply::Now(_) => immediate,
        VmReply::Settled(_) => ok,
        VmReply::Pending(_) => StatusCode::ACCEPTED,
    };
    let (vm, settled) = r.parts();
    Proto(
        status,
        pb::CreateVmResponse {
            vm: vm.into(),
            settled,
            ..Default::default()
        },
    )
    .into_response()
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
            .ok_or_else(|| {
                err(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "If-Match must be a resource version",
                )
            }),
    }
}

/// A VM as the API shows it, with its node's name.
pub(crate) fn vm_response(c: &Caller, vm: &Vm) -> VmResponse {
    let mut r = VmResponse::from(vm);
    r.node_name = r
        .node
        .as_deref()
        .and_then(|n| c.manager().nodes().get(n).ok().flatten())
        .map(|n| n.spec.name);
    // On OVN networks netd hands out no address: OVN's DHCP serves the
    // control plane's reservation, which is then the NIC's address. The
    // reservations are in the cluster store, so this is filled here, not by
    // the controller on the VM's node (an agent doesn't hold them).
    if r.nics.iter().any(|n| n.ipv4.is_none()) {
        let res = crate::ipam::list_reservations(&c.manager().store.database()).unwrap_or_default();
        for (i, nic) in r
            .nics
            .iter_mut()
            .enumerate()
            .filter(|(_, n)| n.ipv4.is_none())
        {
            nic.ipv4 = res
                .iter()
                .find(|x| x.vm_id == vm.id && x.nic as usize == i && x.network == nic.network)
                .map(|x| x.ip);
        }
    }
    r
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
    let req = pb::ListVmsRequest {
        project: q.project,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::list_vms(&c, req).await?))
}

/// `POST /scheduler/preview` (spec/clustering-ui.md §5.1): where a VM would
/// go, node by node, before it is created.
pub async fn preview(c: Caller, Json(v): Json<Value>) -> Result<Response, ApiErr> {
    let req: pb::PreviewPlacementRequest = body(v)?;
    Ok(Proto(StatusCode::OK, svc::preview_placement(&c, req).await?).into_response())
}

pub async fn create(
    c: Caller,
    Query(w): Query<WaitQuery>,
    Json(v): Json<Value>,
) -> Result<Response, ApiErr> {
    // proto3 has no required fields; the REST body says which are missing
    // (`422`, as before the port). A `0` sent is validated like any value.
    let missing: Vec<&str> = ["name", "vcpu_count", "mem_size_mib"]
        .into_iter()
        .filter(|f| v.get(*f).is_none_or(Value::is_null))
        .collect();
    if !missing.is_empty() {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid",
            format!("missing field(s): {}", missing.join(", ")),
        ));
    }
    let mut req: pb::CreateVmRequest = body(v)?;
    req.wait_secs = req.wait_secs.or(secs(w.wait));
    let r = svc::create_vm(&c, req).await?;
    Ok(reply(r, StatusCode::CREATED, StatusCode::CREATED))
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct ViewQuery {
    /// `full`: the stored `{meta, spec, status}` (§12.4).
    #[serde(default)]
    view: Option<String>,
}

pub async fn get_one(
    c: Caller,
    Path(id): Path<String>,
    Query(q): Query<ViewQuery>,
) -> Result<Response, ApiErr> {
    if q.view.as_deref() == Some("full") {
        let (vm, _, _) = visible_vm(&c, &id).await?;
        // The stored record carries host paths (disk files, kernel, seed in
        // the private runtime directory), PIDs and the boot id, which the
        // flat view leaves out: host readers only (spec/security.md §9).
        // It is the store's own format, not an API message.
        c.require_action("readSystemStatus", Ent::Host, EntitySet::new(), &[])?;
        return Ok(Json(serde_json::to_value(&vm).unwrap_or_default()).into_response());
    }
    let req = pb::GetVmRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::get_vm(&c, req).await?).into_response())
}

#[derive(serde::Deserialize, Default)]
pub struct DeleteVmQuery {
    #[serde(default)]
    keep_disk: bool,
    #[serde(default)]
    wait: Option<u64>,
}

/// `204` once the VM is gone (at once when nothing ran, or with `?wait`
/// when the controller finished in time), else `202` with it.
pub async fn delete_one(
    c: Caller,
    Path(id): Path<String>,
    Query(q): Query<DeleteVmQuery>,
) -> Result<Response, ApiErr> {
    let req = pb::DeleteVmRequest {
        id,
        keep_disk: q.keep_disk.then_some(true),
        wait_secs: secs(q.wait),
        ..Default::default()
    };
    Ok(match svc::delete_vm(&c, req).await? {
        None => proto_no_content(),
        Some(vm) => Proto(
            StatusCode::ACCEPTED,
            pb::DeleteVmResponse {
                vm: vm.into(),
                ..Default::default()
            },
        )
        .into_response(),
    })
}

pub async fn start(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiErr> {
    let req = pb::StartVmRequest {
        id,
        expected_resource_version: if_match(&headers)?,
        wait_secs: secs(w.wait),
        ..Default::default()
    };
    Ok(reply(
        svc::start_vm(&c, req).await?,
        StatusCode::OK,
        StatusCode::ACCEPTED,
    ))
}

#[derive(Debug, serde::Deserialize)]
pub struct StopParams {
    /// Press the guest's power button and wait up to this long for it to
    /// shut down before stopping it hard (`spec.stop_grace_secs`).
    graceful_timeout_secs: Option<u64>,
    #[serde(default)]
    wait: Option<u64>,
}

pub async fn stop(
    c: Caller,
    Path(id): Path<String>,
    Query(p): Query<StopParams>,
    headers: HeaderMap,
) -> Result<Response, ApiErr> {
    let req = pb::StopVmRequest {
        id,
        graceful_timeout_secs: secs(p.graceful_timeout_secs),
        expected_resource_version: if_match(&headers)?,
        wait_secs: secs(p.wait),
        ..Default::default()
    };
    Ok(reply(
        svc::stop_vm(&c, req).await?,
        StatusCode::OK,
        StatusCode::ACCEPTED,
    ))
}

pub async fn pause(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiErr> {
    let req = pb::PauseVmRequest {
        id,
        expected_resource_version: if_match(&headers)?,
        wait_secs: secs(w.wait),
        ..Default::default()
    };
    Ok(reply(
        svc::pause_vm(&c, req).await?,
        StatusCode::OK,
        StatusCode::ACCEPTED,
    ))
}

/// The JSON merge patch `PATCH /vms/{id}` took before the port
/// (`{power, restart_policy, on_host_boot, stop_grace_secs, config: {…}}`),
/// as a change and its mask (spec/grpc.md §9.1 step 3). The immutable
/// fields it used to refuse by name are refused the same way.
fn merge_patch(v: &Value) -> Result<(Value, Vec<String>), ApiErr> {
    const IMMUTABLE: &[&str] = &[
        "hypervisor",
        "kernel_image_path",
        "firmware_path",
        "firmware",
        "rootfs_path",
        "cloud_init_path",
        "root_disk",
        "image",
    ];
    let o = v.as_object().cloned().unwrap_or_default();
    let mut change = serde_json::Map::new();
    let mut mask = Vec::new();
    for (k, val) in o {
        if k == "config" {
            let cfg = val.as_object().cloned().unwrap_or_default();
            let fixed: Vec<&str> = IMMUTABLE
                .iter()
                .copied()
                .filter(|f| cfg.contains_key(*f))
                .collect();
            if !fixed.is_empty() {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "invalid_config",
                    format!("immutable field(s): {}", fixed.join(", ")),
                ));
            }
            for (ck, cv) in cfg {
                mask.push(ck.clone());
                if !cv.is_null() {
                    change.insert(ck, cv);
                }
            }
        } else {
            mask.push(k.clone());
            change.insert(k, val);
        }
    }
    Ok((Value::Object(change), mask))
}

/// `PATCH /vms/{id}`: `{change, update_mask}` (`UpdateVmRequest`), or,
/// for one release, the merge patch from before the port. Without a mask
/// the fields of `change` present are the ones changed.
pub async fn patch_one(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    headers: HeaderMap,
    Json(v): Json<Value>,
) -> Result<Response, ApiErr> {
    let proto_shape = v.get("change").is_some() || v.get("update_mask").is_some();
    let (change, mask, rest) = if proto_shape {
        let mask: Vec<String> = match v.get("update_mask").and_then(|m| m.as_str()) {
            // proto3 JSON: a FieldMask is one comma-separated string.
            Some(s) => s
                .split(',')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect(),
            None => v["change"]
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default(),
        };
        let mut rest = v.clone();
        if let Some(o) = rest.as_object_mut() {
            o.remove("change");
            o.remove("update_mask");
        }
        (
            v.get("change")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
            mask,
            rest,
        )
    } else {
        let (change, mask) = merge_patch(&v)?;
        (change, mask, serde_json::json!({}))
    };
    let change: pb::VmChange = body(change)?;
    let mut req: pb::UpdateVmRequest = body(rest)?;
    req.id = id;
    req.change = change.into();
    req.update_mask = buffa_types::google::protobuf::FieldMask {
        paths: mask,
        ..Default::default()
    }
    .into();
    req.expected_resource_version = req.expected_resource_version.or(if_match(&headers)?);
    req.wait_secs = req.wait_secs.or(secs(w.wait));
    Ok(reply(
        svc::update_vm(&c, req).await?,
        StatusCode::OK,
        StatusCode::ACCEPTED,
    ))
}

pub async fn events(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::ListVmEventsRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::vm_events(&c, req).await?))
}

pub async fn console_info(c: Caller, Path(id): Path<String>) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::GetConsoleInfoRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::console_info(&c, req).await?))
}

pub async fn console_ticket(
    c: Caller,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiErr> {
    let req = pb::CreateConsoleTicketRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::console_ticket(&c, req).await?))
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
            format!(
                "{} is not a PCI device (/sys/bus/pci/devices/DDDD:BB:DD.F)",
                path
            ),
        ))
    }
}

/// A PCI device entity with the projects and tenants granted it in the
/// config.
pub(crate) fn pci_entities(c: &Caller, bdf: &str) -> (Ent, EntitySet) {
    let projects = c.manager().projects();
    let allow: Vec<_> = c
        .auth()
        .config
        .pci
        .allow
        .iter()
        .filter(|g| g.bdf.eq_ignore_ascii_case(bdf))
        .collect();
    let grants: Vec<String> = allow
        .iter()
        .flat_map(|g| g.projects.iter())
        .filter_map(|p| projects.resolve(p).ok().map(|p| p.id))
        .collect();
    let tenants: Vec<String> = allow
        .iter()
        .flat_map(|g| g.tenants.iter())
        .filter_map(|t| c.auth().tenants.resolve(t).ok().map(|t| t.id))
        .collect();
    let mut es = EntitySet::new();
    es.pci_device(bdf, &grants, &tenants);
    (Ent::PciDevice(bdf.to_string()), es)
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

#[derive(Debug, serde::Deserialize, Default)]
pub struct TicketQuery {
    #[serde(default)]
    ticket: Option<String>,
}

/// Browser sessions need a fresh single-use ticket (spec §5.6); local
/// peers and token holders don't (they're not browsers).
pub async fn console_ws(
    c: Caller,
    Path(id): Path<String>,
    Query(q): Query<TicketQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let target = match svc::console_target(&c, &id, q.ticket.as_deref()).await {
        Ok(t) => t,
        Err(e) => return ApiErr::from(e).into_response(),
    };
    let m = c.app.manager.clone();
    // A relayed console must answer before the upgrade (its node may be away).
    if let svc::ConsoleTarget::Remote { .. } = &target {
        return match svc::console_connect(&m, target).await {
            Ok(stream) => ws.on_upgrade(move |socket| bridge_stream(socket, stream)),
            Err(e) => err(StatusCode::BAD_GATEWAY, "node_unreachable", e).into_response(),
        };
    }
    // The shim keeps the listener alive even after the guest exits, so a
    // dead VM still replays its log; no socket at all is said in the stream.
    ws.on_upgrade(move |mut socket| async move {
        match svc::console_connect(&m, target).await {
            Ok(stream) => bridge_stream(socket, stream).await,
            Err(e) => {
                let _ = socket.send(Message::Text(e.into())).await;
                let _ = socket.send(Message::Close(None)).await;
            }
        }
    })
}

/// The same, over any byte stream (a local socket, or a node relay).
pub(crate) async fn bridge_stream<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut ws: WebSocket,
    stream: S,
) {
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

/// The VM's captured console output, as the bytes themselves.
pub async fn console_log(
    c: Caller,
    Path(id): Path<String>,
    Query(q): Query<LogQuery>,
) -> Result<Response, ApiErr> {
    let req = pb::GetConsoleLogRequest {
        id,
        tail_bytes: q.tail_bytes,
        previous: q.previous.then_some(true),
        ..Default::default()
    };
    let r = svc::console_log(&c, req).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        r.data,
    )
        .into_response())
}

/// The last `tail` bytes of a console log; `None` for a rotated log that
/// doesn't exist.
pub(crate) async fn read_log_tail(
    path: String,
    tail: u64,
    previous: bool,
) -> std::io::Result<Option<Vec<u8>>> {
    tokio::task::spawn_blocking(move || -> std::io::Result<Option<Vec<u8>>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(if previous { None } else { Some(Vec::new()) });
            }
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

#[derive(serde::Deserialize)]
pub struct DeviceBody {
    device_path: String,
}

pub async fn attach_device(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    Json(b): Json<DeviceBody>,
) -> Result<Response, ApiErr> {
    let req = pb::AttachDeviceRequest {
        id,
        device_path: b.device_path,
        wait_secs: secs(w.wait),
        ..Default::default()
    };
    Ok(reply(
        svc::attach_device(&c, req).await?,
        StatusCode::OK,
        StatusCode::ACCEPTED,
    ))
}

pub async fn detach_device(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    Json(b): Json<DeviceBody>,
) -> Result<Response, ApiErr> {
    let req = pb::DetachDeviceRequest {
        id,
        device_path: b.device_path,
        wait_secs: secs(w.wait),
        ..Default::default()
    };
    Ok(reply(
        svc::detach_device(&c, req).await?,
        StatusCode::OK,
        StatusCode::ACCEPTED,
    ))
}

#[derive(serde::Deserialize)]
pub struct AttachDiskBody {
    disk: String,
}

pub async fn attach_disk(
    c: Caller,
    Path(id): Path<String>,
    Query(w): Query<WaitQuery>,
    Json(b): Json<AttachDiskBody>,
) -> Result<Response, ApiErr> {
    let req = pb::AttachDiskRequest {
        id,
        disk: b.disk,
        wait_secs: secs(w.wait),
        ..Default::default()
    };
    Ok(reply(
        svc::attach_disk(&c, req).await?,
        StatusCode::OK,
        StatusCode::ACCEPTED,
    ))
}

pub async fn detach_disk(
    c: Caller,
    Path((id, disk)): Path<(String, String)>,
    Query(w): Query<WaitQuery>,
) -> Result<Response, ApiErr> {
    let req = pb::DetachDiskRequest {
        id,
        disk,
        wait_secs: secs(w.wait),
        ..Default::default()
    };
    Ok(reply(
        svc::detach_disk(&c, req).await?,
        StatusCode::OK,
        StatusCode::ACCEPTED,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bdf_validation() {
        assert_eq!(
            device_bdf("/sys/bus/pci/devices/0000:41:00.0").unwrap(),
            "0000:41:00.0"
        );
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
