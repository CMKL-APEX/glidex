//! `GET /watch`: the watch stream as server-sent events (the logic is in
//! `service::watch`; `glidex.watch.v1.WatchService.Watch` is the RPC). Each
//! event is named for its type (`added`, `modified`, `deleted`, `synced`,
//! `expired`), its data `{kind, id, object}` with the object in the proto
//! shape.

use super::{ApiErr, Caller};
use crate::service::watch as svc;
use axum::extract::Query;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::Deserialize;
use std::convert::Infallible;
use std::time::Duration;

#[derive(Debug, Deserialize, Default)]
pub struct WatchQuery {
    /// Comma-separated `vms,disks,images,networks` (the default) and
    /// `nodes,imports,cluster,policy`.
    #[serde(default)]
    kinds: Option<String>,
    /// Only this project's VMs and disks (like `?project=` on the lists).
    #[serde(default)]
    project: Option<String>,
}

fn sse(e: &svc::Event) -> Event {
    let data = match e.kind {
        Some(k) => {
            let mut d = serde_json::json!({ "kind": k.name(), "id": e.id });
            if let Some(o) = &e.object {
                d["object"] = o.clone();
            }
            d.to_string()
        }
        None => "{}".to_string(),
    };
    Event::default().event(e.ty.name()).data(data)
}

pub async fn watch(
    c: Caller,
    via: Option<axum::Extension<super::RelayedBy>>,
    Query(q): Query<WatchQuery>,
) -> Result<axum::response::Response, ApiErr> {
    // Through a node's UI, the cluster summary is about that node.
    let viewer = via.map(|v| v.0.0);
    let kinds = svc::parse_kinds(q.kinds.as_deref())?;
    let rx = svc::open(c, kinds, q.project.as_deref(), viewer).await?;
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|e| (Ok::<_, Infallible>(sse(&e)), rx))
    });
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}
