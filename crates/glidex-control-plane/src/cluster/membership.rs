//! Membership changes on the leader (spec/clustering.md §5.5–5.7, §5.11):
//! remove an empty node, forget a lost one, purge what it left, and issue
//! the token that lets a repaired host come back as itself.
//!
//! Each step is written to be repeated: the node record says how far a removal
//! got, and a leader loop finishes the Raft side of any tombstoned server, so
//! a leader that fails half way is replaced by one that completes the work.

use super::runtime::Cluster;
use super::tokens::{self, TokenKind};
use crate::models::Tristate;
use crate::node::{Node, NodePhase, NodeRole, NodeStore};
use crate::state::VmManager;
use crate::store::{Origin, TableId};
use redb::ReadableTable;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum MemberError {
    #[error("node not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error("this is not the leader")]
    NotLeader,
    #[error("{0}")]
    Failed(String),
}

fn failed(e: impl std::fmt::Display) -> MemberError {
    MemberError::Failed(e.to_string())
}

fn resolve(nodes: &[Node], key: &str) -> Result<Node, MemberError> {
    nodes
        .iter()
        .find(|n| n.meta.id == key)
        .or_else(|| nodes.iter().find(|n| n.spec.name == key && !n.status.phase.is_tombstone()))
        .or_else(|| nodes.iter().find(|n| n.spec.name == key))
        .cloned()
        .ok_or_else(|| MemberError::NotFound(key.to_string()))
}

impl Cluster {
    /// Deny-list every certificate issued to `node` (it can no longer connect).
    pub(crate) fn deny_certs(tx: &crate::store::Tx<'_>, node: &str, why: &str) -> Result<usize, crate::store::StoreError> {
        let io = |e: serde_json::Error| crate::store::StoreError::Io(std::io::Error::other(e.to_string()));
        let serials: Vec<String> = {
            let t = tx.open_table(TableId::IssuedCerts.definition())?;
            let rows: Vec<String> = t
                .iter()?
                .flatten()
                .filter(|(_, v)| serde_json::from_slice::<Value>(v.value()).ok().is_some_and(|v| v["node"] == node))
                .map(|(k, _)| k.value().to_string())
                .collect();
            rows
        };
        let mut deny = tx.open_table(TableId::NodeDenylist.definition())?;
        for s in &serials {
            deny.insert(s.as_str(), serde_json::to_vec(&json!({ "node": node, "at": crate::tenancy::now(), "why": why })).map_err(io)?.as_slice())?;
        }
        Ok(serials.len())
    }

    /// Whether the voters left after `leaving` go may still elect and commit (§5.5 step 2).
    pub(crate) fn check_voter_leaves(&self, leaving: u64, force: bool) -> Result<(), MemberError> {
        let Some(node) = &self.node else { return Ok(()) };
        let voters = node.voters();
        if !voters.contains(&leaving) {
            return Ok(());
        }
        let remaining = voters.len() - 1;
        if remaining == 0 || remaining * 2 <= voters.len() {
            return Err(MemberError::Conflict(format!("{remaining} of {} voters would not be a majority of the current group", voters.len())));
        }
        if remaining % 2 == 0 && !force {
            return Err(MemberError::Conflict(format!("that would leave {remaining} voters; voters are 1, 3 or 5 (add a server first, or use --force)")));
        }
        Ok(())
    }

    /// Take tombstoned servers out of the Raft group. Idempotent; run by the
    /// leader whenever it has a moment (and straight after a removal).
    pub async fn finish_raft_removals(&self) -> Result<usize, MemberError> {
        let Some(node) = &self.node else { return Ok(0) };
        if !node.is_leader() {
            return Ok(0);
        }
        let store = NodeStore::new(self.db.clone());
        let gone: BTreeSet<u64> = store.list().map_err(failed)?.iter().filter(|n| n.status.phase.is_tombstone()).filter_map(|n| n.status.raft_id).collect();
        let members: BTreeSet<u64> = node.voters().union(&node.learners()).copied().collect();
        let stale: BTreeSet<u64> = gone.intersection(&members).copied().collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let n = stale.len();
        node.remove_members(stale).await.map_err(failed)?;
        Ok(n)
    }
}

impl Cluster {
    /// A server that leaves takes a copy of the CA key (§12.5): rotate, in the
    /// background, so the request doesn't wait on every server.
    pub fn rotate_after_server_left(self: &Arc<Self>, n: &Node) {
        if n.spec.role != NodeRole::Server {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.rotate_ca(None).await {
                tracing::warn!("rotating the CA after a server left: {}", e);
            }
        });
    }
}

impl VmManager {
    fn member_cluster(&self) -> Result<Arc<Cluster>, MemberError> {
        let c = self.cluster().ok_or_else(|| MemberError::Invalid("this host is not part of a cluster".into()))?;
        match &c.node {
            Some(n) if n.is_leader() => Ok(c),
            _ => Err(MemberError::NotLeader),
        }
    }

    /// What keeps `node` from being removed: its VMs, disks and node networks.
    fn node_contents(&self, id: &str) -> Vec<String> {
        let mut out: Vec<String> = crate::cluster::sync::node_vms(&self.database(), id).into_iter().map(|(v, _)| format!("VM {v}")).collect();
        out.extend(self.images.list_disks().into_iter().filter(|d| d.node.as_deref() == Some(id)).map(|d| format!("disk {}", d.name)));
        out.extend(self.networks.list().unwrap_or_default().into_iter().filter(|n| n.node.as_deref() == Some(id)).map(|n| format!("network {}", n.name)));
        out
    }

    /// The gateway groups (the edge's and VPC routers') that losing `node`
    /// would empty while NAT networks still need them (§5.5 step 1).
    pub(crate) fn gateway_blocker(&self, node: &Node) -> Option<String> {
        let ovn = self.ovn.load_full();
        if !ovn.enabled {
            return None;
        }
        let nodes = NodeStore::new(self.database()).list().unwrap_or_default();
        let lives = |w: &String| nodes.iter().any(|n| (&n.meta.id == w || &n.spec.name == w) && n.meta.id != node.meta.id && !n.status.phase.is_tombstone());
        let is_it = |w: &String| w == &node.meta.id || w == &node.spec.name;
        let nat_networks = self.networks.list().unwrap_or_default().iter().any(|n| n.scope == crate::network::NetworkScope::Cluster && n.mode == crate::network::NetworkMode::Nat && n.deletion_requested_at.is_none());
        let routers = crate::router::list(&self.database()).unwrap_or_default();
        if let Some(e) = &ovn.edge {
            if e.gateway_nodes.iter().any(is_it) && !e.gateway_nodes.iter().any(lives) && nat_networks {
                return Some("it is the only gateway of the shared edge router; add another gateway node to cluster.ovn.edge first".into());
            }
        }
        for r in routers {
            if let Some(g) = &r.spec.gateway_nodes {
                if g.iter().any(is_it) && !g.iter().any(lives) {
                    return Some(format!("it is the only gateway of router {}", r.spec.name));
                }
            }
        }
        None
    }

    /// §5.5: `gxctl node remove`.
    pub async fn remove_node(&self, key: &str, force: bool) -> Result<Value, MemberError> {
        let c = self.member_cluster()?;
        let store = NodeStore::new(self.database());
        let n = resolve(&store.list().map_err(failed)?, key)?;
        match n.status.phase {
            NodePhase::Removed => return Ok(json!({ "node": n.spec.name, "phase": n.status.phase, "already": true })),
            NodePhase::Active | NodePhase::Draining => {}
            p => return Err(MemberError::Conflict(format!("the node is {p:?}"))),
        }
        if n.meta.id == c.identity.node_id {
            return Err(MemberError::Conflict("a server can't remove itself: run this on another server".into()));
        }
        let left = self.node_contents(&n.meta.id);
        if !left.is_empty() {
            return Err(MemberError::Conflict(format!("the node still holds: {}", left.join(", "))));
        }
        if let Some(why) = self.gateway_blocker(&n) {
            return Err(MemberError::Conflict(why));
        }
        if let Some(rid) = n.status.raft_id {
            c.check_voter_leaves(rid, force)?;
        }
        self.leave_ovn_membership(&n, false).await?;
        let id = n.meta.id.clone();
        let denied = self.write_tombstone(&c, &id, NodePhase::Removed, "removed", false)?;
        // The Raft side may need a moment (the leader itself can be going).
        let _ = c.finish_raft_removals().await;
        c.rotate_after_server_left(&n);
        Ok(json!({ "node": n.spec.name, "phase": NodePhase::Removed, "denied_certificates": denied, "server": n.spec.role == NodeRole::Server }))
    }

    /// The one write that retires a node: its phase, its certificates on the
    /// deny-list and, for `lose`, its VMs marked Lost.
    fn write_tombstone(&self, c: &Cluster, id: &str, phase: NodePhase, why: &str, lose: bool) -> Result<usize, MemberError> {
        let now = crate::tenancy::now();
        let ev = |e: serde_json::Error| crate::store::StoreError::Io(std::io::Error::other(e.to_string()));
        self.database()
            .write(Origin::Api, |tx| -> Result<usize, crate::store::StoreError> {
                {
                    let mut nodes = tx.open_table(TableId::Nodes.definition())?;
                    let Some(mut cur): Option<Node> = nodes.get(id)?.and_then(|v| serde_json::from_slice(v.value()).ok()) else { return Ok(0) };
                    cur.status.phase = phase;
                    cur.spec.unschedulable = true;
                    cur.status.ready = Tristate::Unknown;
                    cur.status.ready_reason = Some(format!("{phase:?}"));
                    cur.meta.resource_version += 1;
                    nodes.insert(id, serde_json::to_vec(&cur).map_err(ev)?.as_slice())?;
                }
                let denied = Cluster::deny_certs(tx, id, why)?;
                if lose {
                    let mut vms = tx.open_table(TableId::Vms.definition())?;
                    let mut events = tx.open_table(TableId::Events.definition())?;
                    let rows: Vec<(String, Value)> = vms
                        .iter()?
                        .flatten()
                        .filter_map(|(k, v)| Some((k.value().to_string(), serde_json::from_slice::<Value>(v.value()).ok()?)))
                        .filter(|(_, v)| v.pointer("/status/placement/node").and_then(|x| x.as_str()) == Some(id))
                        .collect();
                    for (vid, mut v) in rows {
                        let conds = v["status"]["conditions"].as_array().cloned().unwrap_or_default();
                        let mut conds: Vec<Value> = conds.into_iter().filter(|c| c["kind"] != "Ready").collect();
                        conds.push(json!({ "kind": "Ready", "status": "False", "reason": "Lost", "message": "its node was forgotten; the record is kept", "last_transition_at": now }));
                        v["status"]["conditions"] = Value::Array(conds);
                        v["meta"]["resource_version"] = json!(v["meta"]["resource_version"].as_u64().unwrap_or(0) + 1);
                        vms.insert(vid.as_str(), serde_json::to_vec(&v).map_err(ev)?.as_slice())?;
                        let key = format!("vm/{vid}");
                        let mut ring: Vec<Value> = events.get(key.as_str())?.and_then(|e| serde_json::from_slice(e.value()).ok()).unwrap_or_default();
                        ring.push(json!({ "at": now, "actor": "controller", "kind": "warning", "reason": "Lost", "message": "its node was forgotten" }));
                        let skip = ring.len().saturating_sub(crate::store::EVENTS_PER_OBJECT);
                        ring.drain(..skip);
                        events.insert(key.as_str(), serde_json::to_vec(&ring).map_err(ev)?.as_slice())?;
                    }
                }
                Ok(denied)
            })
            .map_err(failed)
            .inspect(|_| {
                let _ = c;
            })
    }

    /// §5.6: `gxctl node forget --fenced`: the administrator asserts the host
    /// is off for good.
    pub async fn forget_node(&self, key: &str, fenced: bool, force: bool) -> Result<Value, MemberError> {
        if !fenced {
            return Err(MemberError::Invalid("forgetting a node asserts it is powered off and will not come back by itself: confirm with --fenced".into()));
        }
        let c = self.member_cluster()?;
        let store = NodeStore::new(self.database());
        let n = resolve(&store.list().map_err(failed)?, key)?;
        match n.status.phase {
            NodePhase::Forgotten => return Ok(json!({ "node": n.spec.name, "phase": n.status.phase, "already": true })),
            NodePhase::Active | NodePhase::Draining => {}
            p => return Err(MemberError::Conflict(format!("the node is {p:?}"))),
        }
        if n.meta.id == c.identity.node_id {
            return Err(MemberError::Conflict("this server can't forget itself".into()));
        }
        if n.status.ready == Tristate::True {
            return Err(MemberError::Conflict("the node is still reporting; drain and remove it instead (forget is for hosts that are gone)".into()));
        }
        if let Some(rid) = n.status.raft_id {
            c.check_voter_leaves(rid, force)?;
        }
        let _ = self.leave_ovn_membership(&n, true).await;
        let lost = crate::cluster::sync::node_vms(&self.database(), &n.meta.id).len();
        let denied = self.write_tombstone(&c, &n.meta.id, NodePhase::Forgotten, "forgotten", true)?;
        let _ = c.finish_raft_removals().await;
        c.rotate_after_server_left(&n);
        Ok(json!({ "node": n.spec.name, "phase": NodePhase::Forgotten, "lost_vms": lost, "denied_certificates": denied }))
    }

    /// §5.6: `gxctl node purge`: drop what a forgotten node left and retire it as Removed.
    pub async fn purge_node(&self, key: &str) -> Result<Value, MemberError> {
        let _c = self.member_cluster()?;
        let store = NodeStore::new(self.database());
        let n = resolve(&store.list().map_err(failed)?, key)?;
        if n.status.phase != NodePhase::Forgotten {
            return Err(MemberError::Conflict(format!("only a forgotten node can be purged; this one is {:?}", n.status.phase)));
        }
        let id = n.meta.id.clone();
        let vms: Vec<String> = crate::cluster::sync::node_vms(&self.database(), &id).into_iter().map(|(v, _)| v).collect();
        let disks: Vec<String> = self.images.list_disks().into_iter().filter(|d| d.node.as_deref() == Some(id.as_str())).map(|d| d.id).collect();
        let nets: Vec<String> = self.networks.list().unwrap_or_default().into_iter().filter(|x| x.node.as_deref() == Some(id.as_str())).map(|x| x.name).collect();
        // One write: its records go and it becomes Removed together.
        self.database()
            .write(Origin::Api, |tx| -> Result<(), crate::store::StoreError> {
                {
                    let mut t = tx.open_table(TableId::Vms.definition())?;
                    let mut ev = tx.open_table(TableId::Events.definition())?;
                    for v in &vms {
                        t.remove(v.as_str())?;
                        ev.remove(format!("vm/{v}").as_str())?;
                    }
                    let mut t = tx.open_table(TableId::Disks.definition())?;
                    for d in &disks {
                        t.remove(d.as_str())?;
                    }
                    let mut t = tx.open_table(TableId::Networks.definition())?;
                    for x in &nets {
                        t.remove(x.as_str())?;
                    }
                }
                let mut nodes = tx.open_table(TableId::Nodes.definition())?;
                let Some(mut cur): Option<Node> = nodes.get(id.as_str())?.and_then(|v| serde_json::from_slice(v.value()).ok()) else { return Ok(()) };
                cur.status.phase = NodePhase::Removed;
                cur.status.ready_reason = Some("Purged".into());
                cur.meta.resource_version += 1;
                nodes.insert(id.as_str(), serde_json::to_vec(&cur).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?.as_slice())?;
                Ok(())
            })
            .map_err(failed)?;
        Ok(json!({ "node": n.spec.name, "phase": NodePhase::Removed, "purged_vms": vms.len(), "purged_disks": disks.len(), "purged_networks": nets.len() }))
    }

    /// §5.7: `gxctl node rejoin-token`.
    pub async fn rejoin_token(&self, key: &str, raft_intact: bool, ttl_secs: u64, by: &str) -> Result<Value, MemberError> {
        let c = self.member_cluster()?;
        let store = NodeStore::new(self.database());
        let n = resolve(&store.list().map_err(failed)?, key)?;
        match n.status.phase {
            NodePhase::Removed | NodePhase::Departed => return Err(MemberError::Conflict(format!("the node is {:?}: its id is retired; join as a new node", n.status.phase))),
            NodePhase::Departing => return Err(MemberError::Conflict("a detach is in progress for this node".into())),
            _ => {}
        }
        if n.meta.id == c.identity.node_id {
            return Err(MemberError::Conflict("this is the server issuing the token".into()));
        }
        let token = tokens::create(&c.db, TokenKind::Rejoin { node: n.meta.id.clone(), raft_intact }, ttl_secs, &c.signing_fp(), by).map_err(failed)?;
        Ok(json!({ "token": token, "node": n.spec.name, "node_id": n.meta.id, "raft_intact": raft_intact, "ttl_secs": ttl_secs }))
    }

    /// Hook for the OVN side of leaving (§5.5 steps 3–4): kick the node from
    /// the NB/SB clusters and delete its chassis. A no-op without OVN.
    pub(crate) async fn leave_ovn_membership(&self, n: &Node, _unreachable: bool) -> Result<(), MemberError> {
        if !self.ovn_enabled() {
            return Ok(());
        }
        let Some(addr) = n.status.advertise else { return Ok(()) };
        let netd = self.netd.clone();
        let (ip, chassis, server) = (addr.ip().to_string(), n.meta.id.clone(), n.spec.role == NodeRole::Server);
        tokio::task::spawn_blocking(move || netd.call::<()>(glidex_netd::proto::Op::ForgetOvnMember(glidex_netd::proto::ForgetOvnMemberArgs { address: ip, chassis, server })))
            .await
            .map_err(failed)?
            .map_err(|e| MemberError::Failed(format!("OVN: {e}")))
    }
}
