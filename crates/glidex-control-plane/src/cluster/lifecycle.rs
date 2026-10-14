//! Node liveness on the leader (spec/clustering.md §7.2, D7, D8): a node that
//! stops heartbeating becomes `Ready=Unknown/NodeUnreachable`, and so do its
//! VMs. Nothing is stopped or moved. Only the transitions are written.

use super::runtime::Cluster;
use crate::models::Tristate;
use crate::node::Node;
use crate::store::{Origin, TableId};
use redb::ReadableTable;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

impl Cluster {
    /// Run while this node leads (spawned once; idle on followers).
    pub fn start_lifecycle(self: &Arc<Self>) {
        if self.node.is_none() {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let mut was_leader = false;
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            let mut stop = me.stop_rx();
            loop {
                tokio::select! { _ = tick.tick() => {}, _ = stop.changed() => return }
                let leader = me.is_leader();
                if leader && !was_leader {
                    // A new leader hasn't heard from anyone yet: everyone gets a fresh grace.
                    me.liveness.new_term();
                }
                was_leader = leader;
                if leader {
                    let c = me.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        c.liveness_round();
                        c.feature_level_round();
                    })
                    .await;
                    // A removal that was interrupted finishes on whoever leads now.
                    if let Err(e) = me.finish_raft_removals().await {
                        tracing::warn!("finishing a membership removal: {}", e);
                    }
                }
            }
        });
    }

    /// The cluster's feature level is the lowest any server reports; it only
    /// goes up (§6.6). New formats are used once it reaches them.
    fn feature_level_round(&self) {
        let Ok(nodes) = crate::node::NodeStore::new(self.db.clone()).list() else { return };
        let servers = nodes.iter().filter(|n| n.spec.role == crate::node::NodeRole::Server && !n.status.phase.is_tombstone());
        let Some(min) = servers.map(|n| n.status.features.feature_level).min() else { return };
        let cur = self.feature_level();
        if min > cur {
            let r = self.db.write(Origin::Controller, |tx| -> Result<(), crate::store::StoreError> {
                tx.open_table(TableId::Meta.definition())?.insert("feature_level", min.to_string().as_bytes())?;
                Ok(())
            });
            match r {
                Ok(()) => tracing::info!(from = cur, to = min, "cluster feature level raised"),
                Err(e) => tracing::warn!("raising the feature level: {}", e),
            }
        }
    }

    /// The level every server supports; 1 before anything was written.
    pub fn feature_level(&self) -> u32 {
        let Ok(txn) = self.db.begin_read() else { return 1 };
        let Ok(t) = txn.open_table(TableId::Meta.definition()) else { return 1 };
        t.get("feature_level").ok().flatten().and_then(|v| std::str::from_utf8(v.value()).ok().and_then(|s| s.parse().ok())).unwrap_or(1)
    }

    fn liveness_round(&self) {
        let grace = Duration::from_secs(self.config.node_grace_secs);
        let store = crate::node::NodeStore::new(self.db.clone());
        let Ok(nodes) = store.list() else { return };
        for n in nodes {
            if n.meta.id == self.identity.node_id || n.status.phase.is_tombstone() {
                continue;
            }
            let silent = self.liveness.silent_for(&n.meta.id);
            let reachable = silent <= grace;
            let ready = n.status.ready == Tristate::True;
            if reachable != ready {
                if let Err(e) = self.set_ready(&n, reachable) {
                    tracing::warn!(node = %n.spec.name, "recording the node's liveness: {}", e);
                }
            }
        }
    }

    fn set_ready(&self, n: &Node, ready: bool) -> Result<(), crate::store::StoreError> {
        let id = n.meta.id.clone();
        let now = crate::tenancy::now();
        tracing::info!(node = %n.spec.name, ready, "node liveness changed");
        self.db.write(Origin::Controller, |tx| -> Result<(), crate::store::StoreError> {
            let mut nodes = tx.open_table(TableId::Nodes.definition())?;
            let Some(mut cur): Option<Node> = nodes.get(id.as_str())?.and_then(|v| serde_json::from_slice(v.value()).ok()) else { return Ok(()) };
            cur.status.ready = if ready { Tristate::True } else { Tristate::Unknown };
            cur.status.ready_reason = (!ready).then(|| "NodeUnreachable".to_string());
            cur.status.heartbeat = Some(now);
            cur.meta.resource_version += 1;
            nodes.insert(id.as_str(), serde_json::to_vec(&cur).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?.as_slice())?;
            drop(nodes);
            if ready {
                // The node's own controller restates its VMs' conditions.
                return Ok(());
            }
            let mut vms = tx.open_table(TableId::Vms.definition())?;
            let mut events = tx.open_table(TableId::Events.definition())?;
            let rows: Vec<(String, Value)> = vms
                .iter()?
                .flatten()
                .filter_map(|(k, v)| Some((k.value().to_string(), serde_json::from_slice::<Value>(v.value()).ok()?)))
                .filter(|(_, v)| v.pointer("/status/placement/node").and_then(|x| x.as_str()) == Some(id.as_str()))
                .collect();
            for (vid, mut v) in rows {
                let conds = v["status"]["conditions"].as_array().cloned().unwrap_or_default();
                let mut conds: Vec<Value> = conds.into_iter().filter(|c| c["kind"] != "Ready").collect();
                conds.push(json!({ "kind": "Ready", "status": "Unknown", "reason": "NodeUnreachable", "message": "the node has stopped reporting; its VMs may still be running", "last_transition_at": now }));
                v["status"]["conditions"] = Value::Array(conds);
                v["meta"]["resource_version"] = json!(v["meta"]["resource_version"].as_u64().unwrap_or(0) + 1);
                vms.insert(vid.as_str(), serde_json::to_vec(&v).unwrap().as_slice())?;
                let key = format!("vm/{vid}");
                let mut ring: Vec<Value> = events.get(key.as_str())?.and_then(|e| serde_json::from_slice(e.value()).ok()).unwrap_or_default();
                ring.push(json!({ "at": now, "actor": "controller", "kind": "warning", "reason": "NodeUnreachable", "message": "the node stopped heartbeating; nothing is restarted elsewhere" }));
                let skip = ring.len().saturating_sub(crate::store::EVENTS_PER_OBJECT);
                ring.drain(..skip);
                events.insert(key.as_str(), serde_json::to_vec(&ring).unwrap().as_slice())?;
            }
            Ok(())
        })
    }
}
