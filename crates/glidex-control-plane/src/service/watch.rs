//! The watch family (spec/grpc.md §9, G4; spec/reconciliation.md §12.6): a
//! live stream of the VMs, disks, images and networks the caller may list,
//! as `glidex.watch.v1.WatchService.Watch` and, the same events as
//! server-sent events, `GET /watch`.
//! On request (`kinds=`) also the cluster's nodes, import plans, a cluster
//! summary and a policy marker (spec/clustering-ui.md §3.7).
//!
//! On connect the stream sends every visible object (`added`), then
//! `synced`; afterwards `added`, `modified` and `deleted` as they change.
//! Each connection re-lists with the list endpoints' own visibility rules
//! whenever the store's change bell rings, and sends the differences, so
//! authorization (including policy changes) applies to every event. A
//! stream ends after `MAX_LIFETIME`; clients reconnect (EventSource does on
//! its own) and get a fresh snapshot.

use super::{Call, ServiceError, ServiceResult};
use crate::api::{disk_entities, vm_entities};
use crate::authz::{Ent, EntitySet};
use crate::service::network::network_view;
use axum::http::StatusCode;
use glidex_api::proto::glidex::watch::v1 as pb;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

type Caller = Call;
type ApiErr = ServiceError;

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> ServiceError {
    ServiceError::new(status, code, message)
}

/// Open streams, host-wide.
static WATCHERS: AtomicUsize = AtomicUsize::new(0);
const MAX_WATCHERS: usize = 64;
/// Bounds how long a revoked session keeps receiving events.
const MAX_LIFETIME: Duration = Duration::from_secs(300);
/// Changes within this window go out together (download progress rings
/// the bell often).
const COALESCE: Duration = Duration::from_millis(250);
/// Look again even without a ring (a change the bell missed).
const RESYNC: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    Vm,
    Disk,
    Image,
    Network,
    Node,
    Import,
    Cluster,
    Policy,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Vm => "vm",
            Kind::Disk => "disk",
            Kind::Image => "image",
            Kind::Network => "network",
            Kind::Node => "node",
            Kind::Import => "import",
            Kind::Cluster => "cluster",
            Kind::Policy => "policy",
        }
    }
}

/// `kinds=` of `GET /watch`, comma-separated.
pub fn parse_kinds(s: Option<&str>) -> Result<Vec<Kind>, ApiErr> {
    let Some(s) = s.filter(|s| !s.is_empty()) else {
        return Ok(vec![Kind::Vm, Kind::Disk, Kind::Image, Kind::Network]);
    };
    let mut out = Vec::new();
    for k in s.split(',').map(str::trim) {
        let kind = match k {
            "vm" | "vms" => Kind::Vm,
            "disk" | "disks" => Kind::Disk,
            "image" | "images" => Kind::Image,
            "network" | "networks" => Kind::Network,
            "node" | "nodes" => Kind::Node,
            "import" | "imports" => Kind::Import,
            "cluster" => Kind::Cluster,
            "policy" => Kind::Policy,
            other => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "invalid",
                    format!(
                        "unknown kind '{}' (vms, disks, images, networks, nodes, imports, cluster, policy)",
                        other
                    ),
                ));
            }
        };
        if !out.contains(&kind) {
            out.push(kind);
        }
    }
    Ok(out)
}

/// What the caller may see now: (kind, id) → the object as its list
/// endpoint renders it.
type View = BTreeMap<(Kind, String), serde_json::Value>;

async fn snapshot(
    c: &Caller,
    kinds: &[Kind],
    only: Option<&str>,
    viewer: Option<&str>,
) -> Result<View, ApiErr> {
    let m = c.manager();
    let visible = c.visible_projects()?;
    let mut view = View::new();
    for kind in kinds {
        match kind {
            Kind::Vm => {
                for vm in m.list_vms().await {
                    if only.is_some_and(|p| p != vm.project) || !visible.contains(&vm.project) {
                        continue;
                    }
                    let (e, es) = vm_entities(&vm);
                    if c.allowed("readVm", e, es) {
                        let v = serde_json::to_value(crate::service::vm::vm_pb(c, &vm))
                            .unwrap_or_default();
                        view.insert((Kind::Vm, vm.id.clone()), v);
                    }
                }
            }
            Kind::Disk => {
                for d in m.list_disks() {
                    if only.is_some_and(|p| p != d.project) || !visible.contains(&d.project) {
                        continue;
                    }
                    let (e, es) = disk_entities(&d.id, &d.project);
                    if c.allowed("readDisk", e, es) {
                        // The proto shape, as REST answers (spec/grpc.md §9.1).
                        view.insert(
                            (Kind::Disk, d.id.clone()),
                            serde_json::to_value(crate::service::storage::disk_pb(&d))
                                .unwrap_or_default(),
                        );
                    }
                }
            }
            Kind::Image => {
                if c.allowed("readImage", Ent::Cluster, EntitySet::new()) {
                    for img in m.list_images().await {
                        view.insert(
                            (Kind::Image, img.id.clone()),
                            serde_json::to_value(crate::service::storage::image_pb(&img))
                                .unwrap_or_default(),
                        );
                    }
                }
            }
            Kind::Network => {
                if c.allowed("readNetwork", Ent::Cluster, EntitySet::new()) {
                    for n in m
                        .list_networks()
                        .map_err(|e| ServiceError::from(crate::api::error_to_response(e)))?
                    {
                        if let Some(n) = network_view(c, &visible, n) {
                            view.insert(
                                (Kind::Network, n.id.clone()),
                                serde_json::to_value(crate::service::network::network_pb(&n))
                                    .unwrap_or_default(),
                            );
                        }
                    }
                }
            }
            Kind::Node => {
                if c.allowed("listNodes", Ent::Cluster, EntitySet::new()) {
                    for n in m.nodes().list().unwrap_or_default() {
                        view.insert(
                            (Kind::Node, n.meta.id.clone()),
                            crate::service::node::node_pb(&n)
                                .ok()
                                .and_then(|n| serde_json::to_value(n).ok())
                                .unwrap_or_default(),
                        );
                    }
                }
            }
            Kind::Import => {
                if let Some(cl) = m
                    .cluster()
                    .filter(|_| c.allowed("listImports", Ent::Cluster, EntitySet::new()))
                {
                    for p in crate::cluster::import::plans(&cl.db) {
                        view.insert(
                            (Kind::Import, p.plan.clone()),
                            serde_json::to_value(
                                crate::service::node::plan_pb(crate::service::node::plan_view(&p))
                                    .unwrap_or_default(),
                            )
                            .unwrap_or_default(),
                        );
                    }
                }
            }
            Kind::Cluster => {
                if let Some(cl) = m
                    .cluster()
                    .filter(|_| c.allowed("readCluster", Ent::Cluster, EntitySet::new()))
                {
                    view.insert(
                        (Kind::Cluster, cl.identity.cluster_id.clone()),
                        cluster_summary(&cl, viewer),
                    );
                }
            }
            Kind::Policy => {
                view.insert(
                    (Kind::Policy, "policy".into()),
                    serde_json::json!({ "generation": c.auth().engine.generation() }),
                );
            }
        }
    }
    Ok(view)
}

/// `cluster status` without what changes on every write (applied and log
/// indexes, replication progress), so the stream only speaks on real changes.
fn cluster_summary(cl: &crate::cluster::Cluster, viewer: Option<&str>) -> serde_json::Value {
    let mut v = crate::cluster::manage::status_seen_from(cl, viewer);
    if let Some(r) = v.get_mut("raft").and_then(|r| r.as_object_mut()) {
        for k in ["last_applied", "last_log_index", "purged", "replication"] {
            r.remove(k);
        }
    }
    v["clustered"] = serde_json::json!(true);
    // As `GET /cluster/status` answers it (the proto shape).
    crate::service::cluster::status_pb(v)
        .ok()
        .and_then(|s| serde_json::to_value(s).ok())
        .unwrap_or_default()
}

/// What the stream says, whatever carries it.
#[derive(Debug, Clone, PartialEq)]
pub enum EventType {
    Added,
    Modified,
    Deleted,
    Synced,
    Expired,
}

impl EventType {
    pub fn name(&self) -> &'static str {
        match self {
            EventType::Added => "added",
            EventType::Modified => "modified",
            EventType::Deleted => "deleted",
            EventType::Synced => "synced",
            EventType::Expired => "expired",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub ty: EventType,
    /// `None` for `synced` and `expired`.
    pub kind: Option<Kind>,
    pub id: String,
    /// The object as its list operation answers it (proto JSON).
    pub object: Option<serde_json::Value>,
}

fn event(ty: EventType, k: Kind, id: &str, object: Option<&serde_json::Value>) -> Event {
    Event {
        ty,
        kind: Some(k),
        id: id.to_string(),
        object: object.cloned(),
    }
}

fn marker(ty: EventType) -> Event {
    Event {
        ty,
        kind: None,
        id: String::new(),
        object: None,
    }
}

/// The events that turn `old` into `new`.
fn diff(old: &View, new: &View) -> Vec<Event> {
    let mut out = Vec::new();
    for (key, obj) in new {
        match old.get(key) {
            None => out.push(event(EventType::Added, key.0, &key.1, Some(obj))),
            Some(prev) if prev != obj => {
                out.push(event(EventType::Modified, key.0, &key.1, Some(obj)))
            }
            Some(_) => {}
        }
    }
    for key in old.keys().filter(|k| !new.contains_key(k)) {
        out.push(event(EventType::Deleted, key.0, &key.1, None));
    }
    out
}

/// Releases a watcher slot when the stream ends.
struct Slot;
impl Drop for Slot {
    fn drop(&mut self) {
        WATCHERS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Open a stream of `kinds` (only `project`'s VMs and disks, when given);
/// `viewer`: the node whose UI relayed it (the cluster summary is about it).
/// The first snapshot is taken before this returns, so a refusal is an
/// error of the call, not of the stream.
pub async fn open(
    call: Call,
    kinds: Vec<Kind>,
    project: Option<&str>,
    viewer: Option<String>,
) -> ServiceResult<tokio::sync::mpsc::Receiver<Event>> {
    let c = call;
    let only = match project {
        Some(p) => Some(c.target_project(Some(p))?),
        None => None,
    };
    if WATCHERS.fetch_add(1, Ordering::SeqCst) >= MAX_WATCHERS {
        WATCHERS.fetch_sub(1, Ordering::SeqCst);
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "too_many_watchers",
            "too many open watch streams; poll instead",
        ));
    }
    let slot = Slot;
    let first = snapshot(&c, &kinds, only.as_deref(), viewer.as_deref()).await?;

    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(256);
    let mut bell = c.manager().changed.subscribe();
    tokio::spawn(async move {
        let _slot = slot;
        let deadline = tokio::time::Instant::now() + MAX_LIFETIME;
        let mut seen = View::new();
        let mut now = first;
        let mut synced = false;
        loop {
            for e in diff(&seen, &now) {
                if tx.send(e).await.is_err() {
                    return; // client gone
                }
            }
            if !synced {
                if tx.send(marker(EventType::Synced)).await.is_err() {
                    return;
                }
                synced = true;
            }
            seen = now;
            // Wait for the bell (or the resync tick), then let changes settle.
            bell.borrow_and_update();
            let wake = tokio::time::Instant::now() + RESYNC;
            tokio::select! {
                _ = bell.changed() => {}
                _ = tokio::time::sleep_until(wake.min(deadline)) => {}
                _ = tx.closed() => return,
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(COALESCE).await;
            now = match snapshot(&c, &kinds, only.as_deref(), viewer.as_deref()).await {
                Ok(v) => v,
                // Access gone (user disabled, say): end the stream.
                Err(_) => break,
            };
        }
        let _ = tx.send(marker(EventType::Expired)).await;
    });
    Ok(rx)
}

// ---- the message --------------------------------------------------------------

/// The kinds a `WatchRequest` asks for (default: VMs, disks, images, networks).
pub fn kinds_of(req: &pb::WatchRequest) -> Vec<Kind> {
    let mut out = Vec::new();
    for k in &req.kinds {
        let kind = match k.as_known() {
            Some(pb::WatchKind::WATCH_KIND_VM) => Kind::Vm,
            Some(pb::WatchKind::WATCH_KIND_DISK) => Kind::Disk,
            Some(pb::WatchKind::WATCH_KIND_IMAGE) => Kind::Image,
            Some(pb::WatchKind::WATCH_KIND_NETWORK) => Kind::Network,
            Some(pb::WatchKind::WATCH_KIND_NODE) => Kind::Node,
            Some(pb::WatchKind::WATCH_KIND_IMPORT) => Kind::Import,
            Some(pb::WatchKind::WATCH_KIND_CLUSTER) => Kind::Cluster,
            Some(pb::WatchKind::WATCH_KIND_POLICY) => Kind::Policy,
            _ => continue,
        };
        if !out.contains(&kind) {
            out.push(kind);
        }
    }
    if out.is_empty() {
        out = vec![Kind::Vm, Kind::Disk, Kind::Image, Kind::Network];
    }
    out
}

fn decode<T: serde::de::DeserializeOwned + Default>(v: &serde_json::Value) -> T {
    serde_json::from_value(v.clone()).unwrap_or_default()
}

/// An event as `WatchResponse`.
pub fn event_pb(e: &Event) -> pb::WatchResponse {
    use pb::__buffa::oneof::watch_response::Object;
    let ty = match e.ty {
        EventType::Added => pb::WatchEventType::WATCH_EVENT_TYPE_ADDED,
        EventType::Modified => pb::WatchEventType::WATCH_EVENT_TYPE_MODIFIED,
        EventType::Deleted => pb::WatchEventType::WATCH_EVENT_TYPE_DELETED,
        EventType::Synced => pb::WatchEventType::WATCH_EVENT_TYPE_SYNCED,
        EventType::Expired => pb::WatchEventType::WATCH_EVENT_TYPE_EXPIRED,
    };
    let kind = match e.kind {
        Some(Kind::Vm) => pb::WatchKind::WATCH_KIND_VM,
        Some(Kind::Disk) => pb::WatchKind::WATCH_KIND_DISK,
        Some(Kind::Image) => pb::WatchKind::WATCH_KIND_IMAGE,
        Some(Kind::Network) => pb::WatchKind::WATCH_KIND_NETWORK,
        Some(Kind::Node) => pb::WatchKind::WATCH_KIND_NODE,
        Some(Kind::Import) => pb::WatchKind::WATCH_KIND_IMPORT,
        Some(Kind::Cluster) => pb::WatchKind::WATCH_KIND_CLUSTER,
        Some(Kind::Policy) => pb::WatchKind::WATCH_KIND_POLICY,
        None => pb::WatchKind::WATCH_KIND_UNSPECIFIED,
    };
    let object = e.object.as_ref().zip(e.kind).map(|(v, k)| match k {
        Kind::Vm => Object::Vm(Box::new(decode(v))),
        Kind::Disk => Object::Disk(Box::new(decode(v))),
        Kind::Image => Object::Image(Box::new(decode(v))),
        Kind::Network => Object::Network(Box::new(decode(v))),
        Kind::Node => Object::Node(Box::new(decode(v))),
        Kind::Import => Object::ImportPlan(Box::new(decode(v))),
        Kind::Cluster => Object::Cluster(Box::new(decode(v))),
        Kind::Policy => Object::Policy(Box::new(pb::PolicyMarker {
            generation: v["generation"].as_u64().unwrap_or_default(),
            ..Default::default()
        })),
    });
    pb::WatchResponse {
        r#type: ty.into(),
        kind: kind.into(),
        id: e.id.clone(),
        object,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(parse_kinds(None).unwrap().len(), 4);
        assert_eq!(
            parse_kinds(Some("vms,disk,vms")).unwrap(),
            vec![Kind::Vm, Kind::Disk]
        );
        assert!(parse_kinds(Some("pods")).is_err());
    }

    #[test]
    fn diffs() {
        let a: View = [
            ((Kind::Vm, "1".into()), serde_json::json!({"s": 1})),
            ((Kind::Disk, "d".into()), serde_json::json!({})),
        ]
        .into();
        let b: View = [
            ((Kind::Vm, "1".into()), serde_json::json!({"s": 2})),
            ((Kind::Image, "i".into()), serde_json::json!({})),
        ]
        .into();
        assert_eq!(
            diff(&a, &b).len(),
            3,
            "modified vm, added image, deleted disk"
        );
        let e = event_pb(&diff(&a, &b)[0]);
        assert_eq!(e.id, "1");
        assert!(diff(&b, &b).is_empty());
    }
}
