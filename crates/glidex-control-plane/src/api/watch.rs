//! `GET /watch` (spec/reconciliation.md §12.6): a live stream of the VMs,
//! disks, images and networks the caller may list, as server-sent events.
//!
//! On connect the stream sends every visible object (`added`), then
//! `synced`; afterwards `added`, `modified` and `deleted` as they change.
//! Each connection re-lists with the list endpoints' own visibility rules
//! whenever the store's change bell rings, and sends the differences, so
//! authorization (including policy changes) applies to every event. A
//! stream ends after `MAX_LIFETIME`; clients reconnect (EventSource does on
//! its own) and get a fresh snapshot.

use super::net::network_view;
use super::{disk_entities, err, vm_entities, ApiErr, Caller};
use crate::authz::{Ent, EntitySet};
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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

#[derive(Debug, Deserialize, Default)]
pub struct WatchQuery {
    /// Comma-separated `vms,disks,images,networks` (default: all).
    #[serde(default)]
    kinds: Option<String>,
    /// Only this project's VMs and disks (like `?project=` on the lists).
    #[serde(default)]
    project: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Kind {
    Vm,
    Disk,
    Image,
    Network,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Vm => "vm",
            Kind::Disk => "disk",
            Kind::Image => "image",
            Kind::Network => "network",
        }
    }
}

fn parse_kinds(s: Option<&str>) -> Result<Vec<Kind>, ApiErr> {
    let Some(s) = s.filter(|s| !s.is_empty()) else { return Ok(vec![Kind::Vm, Kind::Disk, Kind::Image, Kind::Network]) };
    let mut out = Vec::new();
    for k in s.split(',').map(str::trim) {
        let kind = match k {
            "vm" | "vms" => Kind::Vm,
            "disk" | "disks" => Kind::Disk,
            "image" | "images" => Kind::Image,
            "network" | "networks" => Kind::Network,
            other => return Err(err(StatusCode::BAD_REQUEST, "invalid", format!("unknown kind '{}' (vms, disks, images, networks)", other))),
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

async fn snapshot(c: &Caller, kinds: &[Kind], only: Option<&str>) -> Result<View, ApiErr> {
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
                        let v = serde_json::to_value(crate::models::VmResponse::from(&vm)).unwrap_or_default();
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
                        view.insert((Kind::Disk, d.id.clone()), serde_json::to_value(&d).unwrap_or_default());
                    }
                }
            }
            Kind::Image => {
                if c.allowed("readImage", Ent::Cluster, EntitySet::new()) {
                    for img in m.list_images().await {
                        view.insert((Kind::Image, img.id.clone()), serde_json::to_value(&img).unwrap_or_default());
                    }
                }
            }
            Kind::Network => {
                if c.allowed("readNetwork", Ent::Cluster, EntitySet::new()) {
                    for n in m.list_networks().map_err(super::storage::manager_err)? {
                        if let Some(n) = network_view(c, &visible, n) {
                            view.insert((Kind::Network, n.name.clone()), serde_json::to_value(&n).unwrap_or_default());
                        }
                    }
                }
            }
        }
    }
    Ok(view)
}

fn event(kind: &str, k: Kind, id: &str, object: Option<&serde_json::Value>) -> Event {
    let mut data = serde_json::json!({ "kind": k.name(), "id": id });
    if let Some(o) = object {
        data["object"] = o.clone();
    }
    Event::default().event(kind).data(data.to_string())
}

/// The events that turn `old` into `new`.
fn diff(old: &View, new: &View) -> Vec<Event> {
    let mut out = Vec::new();
    for (key, obj) in new {
        match old.get(key) {
            None => out.push(event("added", key.0, &key.1, Some(obj))),
            Some(prev) if prev != obj => out.push(event("modified", key.0, &key.1, Some(obj))),
            Some(_) => {}
        }
    }
    for key in old.keys().filter(|k| !new.contains_key(k)) {
        out.push(event("deleted", key.0, &key.1, None));
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

pub async fn watch(c: Caller, Query(q): Query<WatchQuery>) -> Result<axum::response::Response, ApiErr> {
    use axum::response::IntoResponse;
    let kinds = parse_kinds(q.kinds.as_deref())?;
    let only = match &q.project {
        Some(p) => Some(c.target_project(Some(p))?),
        None => None,
    };
    if WATCHERS.fetch_add(1, Ordering::SeqCst) >= MAX_WATCHERS {
        WATCHERS.fetch_sub(1, Ordering::SeqCst);
        return Err(err(StatusCode::SERVICE_UNAVAILABLE, "too_many_watchers", "too many open watch streams; poll instead"));
    }
    let slot = Slot;
    // First snapshot before answering, so an error is a plain HTTP error.
    let first = snapshot(&c, &kinds, only.as_deref()).await?;

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
                if tx.send(Event::default().event("synced").data("{}")).await.is_err() {
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
            now = match snapshot(&c, &kinds, only.as_deref()).await {
                Ok(v) => v,
                // Access gone (user disabled, say): end the stream.
                Err(_) => break,
            };
        }
        let _ = tx.send(Event::default().event("expired").data("{}")).await;
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (Ok::<_, Infallible>(e), rx)) });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(parse_kinds(None).unwrap().len(), 4);
        assert_eq!(parse_kinds(Some("vms,disk,vms")).unwrap(), vec![Kind::Vm, Kind::Disk]);
        assert!(parse_kinds(Some("pods")).is_err());
    }

    #[test]
    fn diffs() {
        let a: View = [((Kind::Vm, "1".into()), serde_json::json!({"s": 1})), ((Kind::Disk, "d".into()), serde_json::json!({}))].into();
        let b: View = [((Kind::Vm, "1".into()), serde_json::json!({"s": 2})), ((Kind::Image, "i".into()), serde_json::json!({}))].into();
        assert_eq!(diff(&a, &b).len(), 3, "modified vm, added image, deleted disk");
        assert!(diff(&b, &b).is_empty());
    }
}
