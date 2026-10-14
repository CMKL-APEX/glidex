//! Leaving with resources (spec/clustering.md §5.8): the departure bundle, the
//! two-phase handover with a signed receipt, and the switch that makes the
//! host a standalone glidex host owning what it took.
//!
//! The bundle is a set of table rows written into a fresh `glidex.db`. Ids are
//! kept (D19). A VM's placement, a disk's and a network's node become `local`,
//! which is what a standalone host calls itself, so the host opens it like any
//! other standalone database.

use super::pki;
use super::runtime::Cluster;
use crate::models::Tristate;
use crate::node::{Node, NodePhase, NodeRole, NodeStore, LOCAL_NODE};
use crate::state::VmManager;
use crate::store::{Db, Origin, TableId};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum DepartureError {
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
}

fn failed(e: impl std::fmt::Display) -> DepartureError {
    DepartureError::Failed(e.to_string())
}

/// One row of the bundle.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Row {
    pub table: u16,
    pub key: String,
    pub value: String,
}

/// What a detach takes, as the administrator asked for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachOptions {
    /// Cluster network → node network on this node: its NICs move there.
    #[serde(default)]
    pub map_networks: BTreeMap<String, String>,
    /// Carry role links, users, teams and identities of the projects too.
    #[serde(default)]
    pub with_access: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub plan: String,
    pub node: String,
    pub rows: Vec<Row>,
    /// Ids of what the bundle holds (`departed_ids`, §5.9).
    pub vm_ids: Vec<String>,
    pub disk_ids: Vec<String>,
    pub image_ids: Vec<String>,
    /// Cluster networks whose NICs were mapped or let go.
    pub mapped: BTreeMap<String, String>,
    pub dropped_networks: Vec<String>,
}

impl Bundle {
    pub fn sha256(&self) -> String {
        pki::hex(&Sha256::digest(serde_json::to_vec(self).expect("bundle serializes")))
    }
}

/// What the node proves it holds when the cluster commits (§5.8.2 step 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub plan: String,
    pub node: String,
    pub bundle_sha256: String,
    pub revision: u64,
    /// Signature of the other fields by the cluster CA.
    pub signature: String,
}

impl Receipt {
    fn payload(plan: &str, node: &str, sha: &str, revision: u64) -> Vec<u8> {
        format!("glidex departure receipt\n{plan}\n{node}\n{sha}\n{revision}").into_bytes()
    }

    pub fn verify(&self, trust_pem: &str) -> bool {
        pki::verify_bytes(trust_pem, &Self::payload(&self.plan, &self.node, &self.bundle_sha256, self.revision), &self.signature)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub plan: String,
    pub node: String,
    pub options: DetachOptions,
    pub started_at: u64,
    pub deadline: u64,
    /// `frozen`, `committed` or `aborted`.
    pub state: String,
    #[serde(default)]
    pub receipt: Option<Receipt>,
}

const PLAN_PREFIX: &str = "departure/";

fn plan_key(plan: &str) -> String {
    format!("{PLAN_PREFIX}{plan}")
}

pub fn read_plan(db: &Db, plan: &str) -> Option<Plan> {
    let txn = db.begin_read().ok()?;
    let t = txn.open_table(TableId::Meta.definition()).ok()?;
    serde_json::from_slice(t.get(plan_key(plan).as_str()).ok()??.value()).ok()
}

pub fn plans(db: &Db) -> Vec<Plan> {
    let Ok(txn) = db.begin_read() else { return Vec::new() };
    let Ok(t) = txn.open_table(TableId::Meta.definition()) else { return Vec::new() };
    let Ok(it) = t.range(PLAN_PREFIX..) else { return Vec::new() };
    it.flatten().take_while(|(k, _)| k.value().starts_with(PLAN_PREFIX)).filter_map(|(_, v)| serde_json::from_slice(v.value()).ok()).collect()
}

fn write_plan(tx: &crate::store::Tx<'_>, p: &Plan) -> Result<(), crate::store::StoreError> {
    tx.open_table(TableId::Meta.definition())?.insert(plan_key(&p.plan).as_str(), serde_json::to_vec(p).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?.as_slice())?;
    Ok(())
}

/// Every row of `table` as JSON.
fn rows_of(db: &Db, table: TableId) -> Vec<(String, Value)> {
    let Ok(txn) = db.begin_read() else { return Vec::new() };
    let Ok(t) = txn.open_table(table.definition()) else { return Vec::new() };
    let Ok(it) = t.iter() else { return Vec::new() };
    it.flatten().filter_map(|(k, v)| Some((k.value().to_string(), serde_json::from_slice::<Value>(v.value()).ok()?))).collect()
}

fn row(table: TableId, key: &str, v: &Value) -> Row {
    Row { table: table as u16, key: key.to_string(), value: v.to_string() }
}

fn mentions(hay: &str, needle: &str) -> bool {
    !needle.is_empty() && hay.contains(needle)
}

/// Build the bundle of `node`'s resources from `db` (the cluster store, or the
/// node's own cache when offline). Pure: nothing is written.
pub fn build_bundle(db: &Db, plan: &str, node: &str, opts: &DetachOptions) -> Bundle {
    let nets = rows_of(db, TableId::Networks);
    let cluster_nets: BTreeSet<String> = nets.iter().filter(|(_, n)| n["scope"] == "cluster").map(|(k, _)| k.clone()).collect();
    let mut out: Vec<Row> = Vec::new();
    let (mut vm_ids, mut disk_ids) = (Vec::new(), Vec::new());
    let mut projects: BTreeSet<String> = BTreeSet::new();
    let mut blob = String::new();
    let (mut mapped, mut dropped) = (BTreeMap::new(), BTreeSet::new());

    for (id, mut vm) in rows_of(db, TableId::Vms) {
        if vm.pointer("/status/placement/node").and_then(|x| x.as_str()) != Some(node) {
            continue;
        }
        vm["status"]["placement"]["node"] = LOCAL_NODE.into();
        let mut changed = false;
        if let Some(list) = vm.pointer_mut("/spec/config/networks").and_then(|l| l.as_array_mut()) {
            let mut kept = Vec::new();
            for att in list.drain(..) {
                let name = att["network"].as_str().unwrap_or("").to_string();
                if !cluster_nets.contains(&name) {
                    kept.push(att);
                } else if let Some(to) = opts.map_networks.get(&name) {
                    let mut a = att;
                    a["network"] = to.clone().into();
                    mapped.insert(name, to.clone());
                    kept.push(a);
                    changed = true;
                } else {
                    dropped.insert(name);
                    changed = true;
                }
            }
            *list = kept;
        }
        if changed {
            let now = crate::tenancy::now();
            vm["meta"]["generation"] = json!(vm["meta"]["generation"].as_u64().unwrap_or(0) + 1);
            let conds = vm["status"]["conditions"].as_array().cloned().unwrap_or_default();
            let mut conds: Vec<Value> = conds.into_iter().filter(|c| c["kind"] != "RestartRequired").collect();
            conds.push(json!({ "kind": "RestartRequired", "status": "True", "reason": "NetworkRemoved", "message": "its cluster network stays behind; the NIC goes at the next restart", "last_transition_at": now }));
            vm["status"]["conditions"] = Value::Array(conds);
        }
        if let Some(p) = vm["meta"]["project"].as_str() {
            projects.insert(p.to_string());
        }
        blob.push_str(&vm.to_string());
        out.push(row(TableId::Vms, &id, &vm));
        vm_ids.push(id);
    }
    for (id, ring) in rows_of(db, TableId::Events) {
        if let Some(v) = id.strip_prefix("vm/") {
            if vm_ids.iter().any(|x| x == v) {
                out.push(row(TableId::Events, &id, &ring));
            }
        }
    }
    for (id, mut d) in rows_of(db, TableId::Disks) {
        if d["node"] != node {
            continue;
        }
        d["node"] = LOCAL_NODE.into();
        if let Some(p) = d["project"].as_str() {
            projects.insert(p.to_string());
        }
        blob.push_str(&d.to_string());
        out.push(row(TableId::Disks, &id, &d));
        disk_ids.push(id);
    }
    for (id, mut n) in nets {
        if n["scope"] == "cluster" || n["node"] != node {
            continue;
        }
        n["node"] = LOCAL_NODE.into();
        if let Some(p) = n["project"].as_str() {
            projects.insert(p.to_string());
        }
        out.push(row(TableId::Networks, &id, &n));
    }
    // Catalog records the disks and VMs use: their files are in this node's cache already.
    let mut image_ids = Vec::new();
    for table in [TableId::Images, TableId::ImageMeta] {
        for (id, v) in rows_of(db, table) {
            if mentions(&blob, &id) {
                if table == TableId::Images {
                    image_ids.push(id.clone());
                }
                out.push(row(table, &id, &v));
            }
        }
    }
    // Guest credentials the VMs reference, by (project, username).
    let used: BTreeSet<String> = vm_ids
        .iter()
        .filter_map(|id| rows_of_one(db, TableId::Vms, id))
        .filter_map(|vm| Some(format!("{}/{}", vm["meta"]["project"].as_str()?, vm.pointer("/spec/config/credential")?.as_str()?)))
        .collect();
    for (k, c) in rows_of(db, TableId::Credentials) {
        if used.contains(&k) {
            out.push(row(TableId::Credentials, &k, &c));
        }
    }
    for (id, p) in rows_of(db, TableId::Projects) {
        if projects.contains(&id) {
            out.push(row(TableId::Projects, &id, &p));
        }
    }
    if opts.with_access {
        let mut users = BTreeSet::new();
        for (k, l) in rows_of(db, TableId::PolicyLinks) {
            let s = l.to_string();
            if projects.iter().any(|p| mentions(&s, p)) {
                for (uk, _) in rows_of(db, TableId::Users) {
                    if mentions(&s, &uk) {
                        users.insert(uk);
                    }
                }
                out.push(row(TableId::PolicyLinks, &k, &l));
            }
        }
        for (k, u) in rows_of(db, TableId::Users) {
            if users.contains(&k) {
                out.push(row(TableId::Users, &k, &u));
            }
        }
        for (k, i) in rows_of(db, TableId::Identities) {
            if users.iter().any(|u| i.to_string().contains(u.as_str())) {
                out.push(row(TableId::Identities, &k, &i));
            }
        }
    }
    out.sort();
    out.dedup();
    Bundle { plan: plan.to_string(), node: node.to_string(), rows: out, vm_ids, disk_ids, image_ids, mapped, dropped_networks: dropped.into_iter().collect() }
}

fn rows_of_one(db: &Db, table: TableId, key: &str) -> Option<Value> {
    let txn = db.begin_read().ok()?;
    let t = txn.open_table(table.definition()).ok()?;
    serde_json::from_slice(t.get(key).ok()??.value()).ok()
}

/// Write the bundle into a new database at `path` (0600) and flush it.
pub fn write_standalone_db(path: &Path, bundle: &Bundle) -> Result<(), DepartureError> {
    let _ = std::fs::remove_file(path);
    // `VmStore::open` makes the file 0600 and the tables; the host's own
    // startup migrates it (schema, the `local` node, the default project).
    let store = crate::store::VmStore::open(path).map_err(failed)?;
    let db = store.database();
    db.write(Origin::Migration, |tx| -> Result<(), crate::store::StoreError> {
        for r in &bundle.rows {
            let table = TableId::from_id(r.table).ok_or_else(|| crate::store::StoreError::Io(std::io::Error::other(format!("unknown table {}", r.table))))?;
            tx.open_table(table.definition())?.insert(r.key.as_str(), r.value.as_bytes())?;
        }
        Ok(())
    })
    .map_err(failed)?;
    drop(db);
    drop(store);
    std::fs::File::open(path).and_then(|f| f.sync_all()).map_err(failed)?;
    Ok(())
}

/// Where a host keeps the pieces of a departure (next to `glidex.db`).
pub fn pending_db(state_dir: &Path) -> PathBuf {
    state_dir.join("glidex.db.standalone-pending")
}

pub fn receipt_file(state_dir: &Path) -> PathBuf {
    state_dir.join("glidex.db.departure-receipt")
}

/// The SHA-256 of the bundle the pending database was written from.
pub fn pending_sha_file(state_dir: &Path) -> PathBuf {
    state_dir.join("glidex.db.standalone-pending.sha256")
}

fn discard_pending(state_dir: &Path) {
    if !receipt_file(state_dir).exists() {
        let _ = std::fs::remove_file(pending_db(state_dir));
        let _ = std::fs::remove_file(pending_sha_file(state_dir));
    }
}

impl Cluster {
    fn state_dir(&self) -> PathBuf {
        self.files.dir.parent().unwrap_or(Path::new(".")).to_path_buf()
    }

    /// Whether `node`'s plan says its objects are frozen.
    pub fn departing_plan(&self, node: &str) -> Option<Plan> {
        plans(&self.db).into_iter().find(|p| p.node == node && p.state == "frozen")
    }

    /// Phase 3 (leader): the node holds the bundle it was sent, so the cluster
    /// lets go of its objects and signs the receipt, all in one write.
    pub async fn commit_departure(self: &Arc<Self>, plan_id: &str, sha: &str) -> Result<Receipt, DepartureError> {
        let plan = read_plan(&self.db, plan_id).ok_or_else(|| DepartureError::NotFound(plan_id.into()))?;
        if plan.state == "committed" {
            return plan.receipt.ok_or_else(|| failed("committed without a receipt"));
        }
        let ca = self.signing_ca().ok_or_else(|| failed("this server holds no CA key"))?;
        let leaving = NodeStore::new(self.db.clone()).get(&plan.node).map_err(failed)?;
        let node_id = plan.node.clone();
        // Everything is checked inside the write: no other write lands between
        // the checks and the commit (an abort, an expiry, a status write).
        let refused = std::cell::RefCell::new(None::<DepartureError>);
        let receipt = self
            .db
            .write(Origin::Api, |tx| -> Result<Option<Receipt>, crate::store::StoreError> {
                let io = |e: serde_json::Error| crate::store::StoreError::Io(std::io::Error::other(e.to_string()));
                let mut plan: Plan = match tx.open_table(TableId::Meta.definition())?.get(plan_key(plan_id).as_str())?.and_then(|v| serde_json::from_slice(v.value()).ok()) {
                    Some(p) => p,
                    None => {
                        *refused.borrow_mut() = Some(DepartureError::NotFound(plan_id.into()));
                        return Ok(None);
                    }
                };
                if plan.state == "committed" {
                    return Ok(plan.receipt);
                }
                if plan.state != "frozen" {
                    *refused.borrow_mut() = Some(DepartureError::Conflict(format!("the plan is {}", plan.state)));
                    return Ok(None);
                }
                let ids = build_bundle(&self.db, plan_id, &plan.node, &plan.options);
                if ids.sha256() != sha {
                    *refused.borrow_mut() = Some(DepartureError::Conflict("the node's objects changed after the bundle was taken; the node fetches it again".into()));
                    return Ok(None);
                }
                let revision = self.db.revision();
                let signature = ca.sign_bytes(&Receipt::payload(plan_id, &plan.node, sha, revision)).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?;
                let receipt = Receipt { plan: plan_id.into(), node: plan.node.clone(), bundle_sha256: sha.into(), revision, signature };
                plan.state = "committed".into();
                plan.receipt = Some(receipt.clone());
                {
                    let mut vms = tx.open_table(TableId::Vms.definition())?;
                    let mut events = tx.open_table(TableId::Events.definition())?;
                    for v in &ids.vm_ids {
                        vms.remove(v.as_str())?;
                        events.remove(format!("vm/{v}").as_str())?;
                    }
                    let mut disks = tx.open_table(TableId::Disks.definition())?;
                    for d in &ids.disk_ids {
                        disks.remove(d.as_str())?;
                    }
                    let mut nets = tx.open_table(TableId::Networks.definition())?;
                    for r in ids.rows.iter().filter(|r| r.table == TableId::Networks as u16) {
                        nets.remove(r.key.as_str())?;
                    }
                    // Its VMs' addresses on cluster networks; their logical ports
                    // leave OVN with the next network-controller pass (they are no
                    // longer in the plan).
                    let mut res = tx.open_table(TableId::IpamReservations.definition())?;
                    let stale: Vec<String> = res
                        .iter()?
                        .flatten()
                        .filter(|(_, v)| serde_json::from_slice::<Value>(v.value()).ok().is_some_and(|r| r["vm_id"].as_str().is_some_and(|x| ids.vm_ids.iter().any(|v| v == x))))
                        .map(|(k, _)| k.value().to_string())
                        .collect();
                    for k in stale {
                        res.remove(k.as_str())?;
                    }
                }
                {
                    let mut nodes = tx.open_table(TableId::Nodes.definition())?;
                    let cur: Option<Node> = nodes.get(node_id.as_str())?.and_then(|v| serde_json::from_slice(v.value()).ok());
                    if let Some(mut n) = cur {
                        n.status.phase = NodePhase::Departed;
                        n.status.ready = Tristate::Unknown;
                        n.status.ready_reason = Some("Departed".into());
                        n.status.departed_ids = ids.vm_ids.iter().chain(&ids.disk_ids).chain(&ids.image_ids).cloned().collect();
                        n.spec.unschedulable = true;
                        n.meta.resource_version += 1;
                        nodes.insert(node_id.as_str(), serde_json::to_vec(&n).map_err(io)?.as_slice())?;
                    }
                }
                Cluster::deny_certs_pub(tx, &node_id, "departed")?;
                write_plan(tx, &plan)?;
                Ok(Some(receipt))
            })
            .map_err(failed)?;
        if let Some(e) = refused.into_inner() {
            return Err(e);
        }
        let receipt = receipt.ok_or_else(|| failed("committed without a receipt"))?;
        // The cluster-side steps of §5.5 for what left: Raft, OVN membership,
        // and a CA rotation if it was a server (it held the key).
        let _ = self.finish_raft_removals().await;
        if let Some(n) = leaving {
            if let Some(h) = self.handlers.get() {
                h.left_ovn(&n).await;
            }
            self.rotate_after_server_left(&n);
        }
        tracing::warn!(node = %node_id, "node departed with its resources");
        Ok(receipt)
    }

    /// Node side, once per tick: when this node is `Departing`, fetch its
    /// bundle, write it to the pending database, acknowledge it, and keep the
    /// receipt that comes back (§5.8.2 steps 2 and 4).
    pub async fn departure_tick(&self) -> Option<Receipt> {
        let me = &self.identity.node_id;
        let state_dir = self.state_dir();
        if receipt_file(&state_dir).exists() {
            return None;
        }
        // Whatever this node's phase looks like from here: once the cluster has
        // committed, its certificate is revoked and its cache stops following,
        // so the plan, not the phase, says whether there is anything to do.
        let Some(plan) = plans(&self.db).into_iter().find(|p| &p.node == me && p.state != "aborted") else {
            discard_pending(&state_dir);
            return None;
        };
        let path = format!("/cluster/v1/departure/{}", plan.plan);
        let reply = self.get_any(&path).await.ok()?;
        if reply.status == hyper::StatusCode::GONE {
            // Aborted or expired at the leader.
            discard_pending(&state_dir);
            return None;
        }
        let v: Value = serde_json::from_slice(&reply.body).ok()?;
        if let Some(r) = v.get("receipt").filter(|r| !r.is_null()).and_then(|r| serde_json::from_value::<Receipt>(r.clone()).ok()) {
            return self.keep_receipt(&state_dir, r);
        }
        let bundle: Bundle = serde_json::from_value(v.get("bundle")?.clone()).ok()?;
        let sha = bundle.sha256();
        // The file on disk is always the bundle being acknowledged.
        if std::fs::read_to_string(pending_sha_file(&state_dir)).ok().as_deref() != Some(sha.as_str()) || !pending_db(&state_dir).exists() {
            if let Err(e) = write_standalone_db(&pending_db(&state_dir), &bundle) {
                tracing::warn!("writing the departure bundle: {}", e);
                return None;
            }
            pki::write_private(&pending_sha_file(&state_dir), sha.as_bytes()).ok()?;
        }
        let r = self.post_any(&format!("{path}/ack"), json!({ "sha256": sha }).to_string().into()).await.ok()?;
        if !r.status.is_success() {
            tracing::warn!("the cluster did not commit the departure: {}", String::from_utf8_lossy(&r.body));
            return None;
        }
        let receipt: Receipt = serde_json::from_slice(&r.body).ok()?;
        self.keep_receipt(&state_dir, receipt)
    }

    fn keep_receipt(&self, state_dir: &Path, r: Receipt) -> Option<Receipt> {
        let trust = self.files.read(self.files.trust()).ok()?;
        if !r.verify(&trust) {
            tracing::error!("the departure receipt does not verify against the cluster CA: ignoring it");
            return None;
        }
        pki::write_private(&receipt_file(state_dir), &serde_json::to_vec(&r).ok()?).ok()?;
        Some(r)
    }

    /// GET from the leader, or from a seed on a node with no Raft member.
    pub async fn get_any(&self, path: &str) -> Result<super::net::Reply, super::runtime::ClusterError> {
        let mut addrs: Vec<String> = self.leader_addr().into_iter().collect();
        addrs.extend(self.identity.seeds.clone());
        let mut last = super::runtime::ClusterError::Other("no server to ask".into());
        for a in addrs {
            match self.client.request_timeout(&a, hyper::Method::GET, path, &[], bytes::Bytes::new(), Duration::from_secs(30)).await {
                Ok(r) if r.status == hyper::StatusCode::MISDIRECTED_REQUEST => {
                    if let Some(l) = serde_json::from_slice::<Value>(&r.body).ok().and_then(|v| v["leader"].as_str().map(String::from)) {
                        if let Ok(r) = self.client.request_timeout(&l, hyper::Method::GET, path, &[], bytes::Bytes::new(), Duration::from_secs(30)).await {
                            return Ok(r);
                        }
                    }
                }
                Ok(r) => return Ok(r),
                Err(e) => last = super::runtime::ClusterError::Other(e.to_string()),
            }
        }
        Err(last)
    }

    /// Leader: abort plans past their deadline (§5.8.2).
    pub fn expire_departures(&self) {
        let now = crate::tenancy::now();
        for p in plans(&self.db).into_iter().filter(|p| p.state == "frozen" && p.deadline < now) {
            if let Err(e) = abort_plan(&self.db, &p.plan) {
                tracing::warn!("expiring a departure plan: {}", e);
            }
        }
    }
}

/// Return a frozen node to `Active` and drop the plan.
pub fn abort_plan(db: &Db, plan_id: &str) -> Result<(), DepartureError> {
    let mut plan = read_plan(db, plan_id).ok_or_else(|| DepartureError::NotFound(plan_id.into()))?;
    if plan.state == "committed" {
        return Err(DepartureError::Conflict("the departure already committed".into()));
    }
    plan.state = "aborted".into();
    let committed = std::cell::Cell::new(false);
    let r = db.write(Origin::Api, |tx| -> Result<(), crate::store::StoreError> {
        // Re-read inside the write: a commit may have landed since.
        let now: Option<Plan> = tx.open_table(TableId::Meta.definition())?.get(plan_key(plan_id).as_str())?.and_then(|v| serde_json::from_slice(v.value()).ok());
        if now.as_ref().is_some_and(|p| p.state == "committed") {
            committed.set(true);
            return Ok(());
        }
        write_plan(tx, &plan)?;
        let mut nodes = tx.open_table(TableId::Nodes.definition())?;
        let cur = nodes.get(plan.node.as_str())?.and_then(|v| serde_json::from_slice::<Node>(v.value()).ok());
        if let Some(mut n) = cur {
            if n.status.phase == NodePhase::Departing {
                n.status.phase = NodePhase::Active;
                n.spec.unschedulable = false;
                n.meta.resource_version += 1;
                nodes.insert(plan.node.as_str(), serde_json::to_vec(&n).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?.as_slice())?;
            }
        }
        Ok(())
    });
    r.map_err(failed)?;
    if committed.get() {
        return Err(DepartureError::Conflict("the departure already committed".into()));
    }
    Ok(())
}

impl Cluster {
    pub fn deny_certs_pub(tx: &crate::store::Tx<'_>, node: &str, why: &str) -> Result<usize, crate::store::StoreError> {
        Cluster::deny_certs(tx, node, why)
    }
}

impl VmManager {
    /// §5.8.2 step 1 (leader): freeze the node and record the plan.
    pub async fn start_detach(&self, key: &str, opts: DetachOptions, timeout_secs: Option<u64>) -> Result<Value, DepartureError> {
        let c = self.cluster().ok_or_else(|| DepartureError::Invalid("this host is not part of a cluster".into()))?;
        if !c.is_leader() {
            return Err(DepartureError::Conflict("this is not the leader".into()));
        }
        let nodes = NodeStore::new(self.database()).list().map_err(failed)?;
        let n = nodes.iter().find(|n| n.meta.id == key || (n.spec.name == key && !n.status.phase.is_tombstone())).cloned().ok_or_else(|| DepartureError::NotFound(key.into()))?;
        if !matches!(n.status.phase, NodePhase::Active | NodePhase::Draining) {
            return Err(DepartureError::Conflict(format!("the node is {:?}", n.status.phase)));
        }
        if n.meta.id == c.identity.node_id {
            return Err(DepartureError::Conflict("a server can't detach itself online: run this on another server, or detach offline".into()));
        }
        if n.status.ready != Tristate::True {
            return Err(DepartureError::Conflict("the node is not reporting; use `forget --departed` if it already left".into()));
        }
        if let Some(why) = self.gateway_blocker(&n) {
            return Err(DepartureError::Conflict(why));
        }
        if let Some(rid) = n.status.raft_id {
            c.check_voter_leaves(rid, false).map_err(|e| DepartureError::Conflict(e.to_string()))?;
        }
        let known: BTreeSet<String> = self.networks.list().unwrap_or_default().into_iter().filter(|x| x.scope == crate::network::NetworkScope::Cluster).map(|x| x.name).collect();
        let mine: BTreeSet<String> = self.networks.list().unwrap_or_default().into_iter().filter(|x| x.node.as_deref() == Some(n.meta.id.as_str())).map(|x| x.name).collect();
        for (from, to) in &opts.map_networks {
            if !known.contains(from) {
                return Err(DepartureError::Invalid(format!("{from} is not a cluster network")));
            }
            if !mine.contains(to) {
                return Err(DepartureError::Invalid(format!("{to} is not a node network of {}", n.spec.name)));
            }
        }
        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let now = crate::tenancy::now();
        let plan = Plan { plan: id.clone(), node: n.meta.id.clone(), options: opts, started_at: now, deadline: now + timeout_secs.unwrap_or(c.config.detach_freeze_secs), state: "frozen".into(), receipt: None };
        let nid = n.meta.id.clone();
        self.database()
            .write(Origin::Api, |tx| -> Result<(), crate::store::StoreError> {
                write_plan(tx, &plan)?;
                let mut nodes = tx.open_table(TableId::Nodes.definition())?;
                let Some(mut cur): Option<Node> = nodes.get(nid.as_str())?.and_then(|v| serde_json::from_slice(v.value()).ok()) else { return Ok(()) };
                cur.status.phase = NodePhase::Departing;
                cur.spec.unschedulable = true;
                cur.meta.resource_version += 1;
                nodes.insert(nid.as_str(), serde_json::to_vec(&cur).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?.as_slice())?;
                Ok(())
            })
            .map_err(failed)?;
        let b = build_bundle(&self.database(), &id, &n.meta.id, &plan.options);
        Ok(json!({ "plan": id, "node": n.spec.name, "deadline": plan.deadline, "vms": b.vm_ids.len(), "disks": b.disk_ids.len(), "images": b.image_ids.len(), "mapped": b.mapped, "dropped_networks": b.dropped_networks }))
    }

    /// `forget --departed` (§5.8.3): the node left on its own (offline detach).
    /// Its objects go from the cluster, it becomes `Departed`, and a server
    /// that left rotates the CA.
    pub async fn forget_departed(&self, key: &str) -> Result<Value, DepartureError> {
        let c = self.cluster().ok_or_else(|| DepartureError::Invalid("this host is not part of a cluster".into()))?;
        if !c.is_leader() {
            return Err(DepartureError::Conflict("this is not the leader".into()));
        }
        let nodes = NodeStore::new(self.database()).list().map_err(failed)?;
        let n = nodes.iter().find(|n| n.meta.id == key || (n.spec.name == key && !n.status.phase.is_tombstone())).cloned().ok_or_else(|| DepartureError::NotFound(key.into()))?;
        if n.status.phase == NodePhase::Departed {
            return Ok(json!({ "node": n.spec.name, "phase": n.status.phase, "already": true }));
        }
        if !matches!(n.status.phase, NodePhase::Active | NodePhase::Draining | NodePhase::Departing) {
            return Err(DepartureError::Conflict(format!("the node is {:?}", n.status.phase)));
        }
        if n.meta.id == c.identity.node_id {
            return Err(DepartureError::Conflict("this server can't forget itself".into()));
        }
        if n.status.ready == Tristate::True {
            return Err(DepartureError::Conflict("the node is still reporting; detach it online instead".into()));
        }
        if let Some(rid) = n.status.raft_id {
            c.check_voter_leaves(rid, false).map_err(|e| DepartureError::Conflict(e.to_string()))?;
        }
        let id = format!("offline-{}", uuid::Uuid::new_v4().simple());
        let bundle = build_bundle(&self.database(), &id, &n.meta.id, &DetachOptions::default());
        let nid = n.meta.id.clone();
        self.database()
            .write(Origin::Api, |tx| -> Result<(), crate::store::StoreError> {
                let io = |e: serde_json::Error| crate::store::StoreError::Io(std::io::Error::other(e.to_string()));
                {
                    let mut vms = tx.open_table(TableId::Vms.definition())?;
                    let mut events = tx.open_table(TableId::Events.definition())?;
                    for v in &bundle.vm_ids {
                        vms.remove(v.as_str())?;
                        events.remove(format!("vm/{v}").as_str())?;
                    }
                    let mut disks = tx.open_table(TableId::Disks.definition())?;
                    for d in &bundle.disk_ids {
                        disks.remove(d.as_str())?;
                    }
                    let mut nets = tx.open_table(TableId::Networks.definition())?;
                    for r in bundle.rows.iter().filter(|r| r.table == TableId::Networks as u16) {
                        nets.remove(r.key.as_str())?;
                    }
                    let mut res = tx.open_table(TableId::IpamReservations.definition())?;
                    let stale: Vec<String> = res
                        .iter()?
                        .flatten()
                        .filter(|(_, v)| serde_json::from_slice::<Value>(v.value()).ok().is_some_and(|r| r["vm_id"].as_str().is_some_and(|x| bundle.vm_ids.iter().any(|v| v == x))))
                        .map(|(k, _)| k.value().to_string())
                        .collect();
                    for k in stale {
                        res.remove(k.as_str())?;
                    }
                }
                {
                    let mut nodes = tx.open_table(TableId::Nodes.definition())?;
                    let Some(mut cur): Option<Node> = nodes.get(nid.as_str())?.and_then(|v| serde_json::from_slice(v.value()).ok()) else { return Ok(()) };
                    cur.status.phase = NodePhase::Departed;
                    cur.status.ready = Tristate::Unknown;
                    cur.status.ready_reason = Some("Departed".into());
                    cur.status.departed_ids = bundle.vm_ids.iter().chain(&bundle.disk_ids).chain(&bundle.image_ids).cloned().collect();
                    cur.spec.unschedulable = true;
                    cur.meta.resource_version += 1;
                    nodes.insert(nid.as_str(), serde_json::to_vec(&cur).map_err(io)?.as_slice())?;
                }
                Cluster::deny_certs_pub(tx, &nid, "departed")?;
                Ok(())
            })
            .map_err(failed)?;
        let _ = self.leave_ovn_membership(&n, true).await;
        let _ = c.finish_raft_removals().await;
        // Offline, the cluster can't know the key was destroyed: a server always rotates.
        c.rotate_after_server_left(&n);
        Ok(json!({ "node": n.spec.name, "phase": NodePhase::Departed, "vms": bundle.vm_ids.len(), "disks": bundle.disk_ids.len() }))
    }
}

/// On a host's startup (§5.8.2 step 4): finish a departure whose receipt is on
/// disk. The receipt must verify against the cluster CA this host still
/// trusts; a pending database without one stays pending.
pub fn finish_pending(db_path: &Path) -> Result<bool, DepartureError> {
    let dir = db_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let (pending, receipt_path) = (pending_db(&dir), receipt_file(&dir));
    if !pending.exists() || !receipt_path.exists() {
        return Ok(false);
    }
    let r: Receipt = serde_json::from_slice(&std::fs::read(&receipt_path).map_err(failed)?).map_err(failed)?;
    let files = super::identity::Files::beside(db_path);
    let trust = std::fs::read_to_string(files.trust()).map_err(failed)?;
    if !r.verify(&trust) {
        return Err(DepartureError::Invalid("the departure receipt does not verify".into()));
    }
    // The receipt must be for this node and for the very bundle on disk.
    if let Some(id) = files.load_identity().map_err(failed)? {
        if id.node_id != r.node {
            return Err(DepartureError::Invalid("the departure receipt is for another node".into()));
        }
    }
    let sha = std::fs::read_to_string(pending_sha_file(&dir)).map_err(|_| DepartureError::Invalid("the pending database has no recorded bundle hash".into()))?;
    if sha.trim() != r.bundle_sha256 {
        return Err(DepartureError::Invalid("the pending database is not the bundle the receipt names".into()));
    }
    switch_to_standalone(db_path, &pending)?;
    let _ = std::fs::remove_file(receipt_path);
    let _ = std::fs::remove_file(pending_sha_file(&dir));
    Ok(true)
}

/// Replace the cluster's database by the standalone one and forget the cluster:
/// identity, certificates, the CA key and the Raft log all go.
pub fn switch_to_standalone(db_path: &Path, pending: &Path) -> Result<(), DepartureError> {
    let files = super::identity::Files::beside(db_path);
    std::fs::rename(pending, db_path).map_err(failed)?;
    let wal = db_path.with_extension("db.lock");
    let _ = std::fs::remove_file(wal);
    files.delete_all();
    Ok(())
}

/// §5.8.3: leave with resources while no server can be reached. The bundle
/// comes from this node's cache; the private key is destroyed.
pub fn offline_detach(db_path: &Path) -> Result<Bundle, DepartureError> {
    let files = super::identity::Files::beside(db_path);
    let identity = files.load_identity().map_err(failed)?.ok_or_else(|| DepartureError::Invalid("this host is not part of a cluster".into()))?;
    let db = Db::create(db_path).map_err(failed)?;
    let bundle = build_bundle(&db, "offline", &identity.node_id, &DetachOptions::default());
    drop(db);
    let pending = pending_db(db_path.parent().unwrap_or(Path::new(".")));
    write_standalone_db(&pending, &bundle)?;
    let _ = std::fs::remove_file(files.key());
    switch_to_standalone(db_path, &pending)?;
    Ok(bundle)
}

impl Cluster {
    /// `gxctl cluster dissolve` (§5.10): the last node of a cluster becomes a
    /// standalone host that keeps everything the cluster held. A final snapshot of
    /// the store is kept (0600); the departure is committed by this node itself,
    /// with a receipt signed by its own CA key, and finished at the next start.
    pub async fn dissolve(self: &Arc<Self>) -> Result<Value, DepartureError> {
        let node = self.node.as_ref().ok_or_else(|| DepartureError::Invalid("only a server can dissolve a cluster".into()))?;
        if !node.is_leader() {
            return Err(DepartureError::Conflict("this is not the leader".into()));
        }
        let nodes = NodeStore::new(self.db.clone()).list().map_err(failed)?;
        let others: Vec<String> = nodes.iter().filter(|n| n.meta.id != self.identity.node_id && !n.status.phase.is_tombstone()).map(|n| n.spec.name.clone()).collect();
        if !others.is_empty() {
            return Err(DepartureError::Conflict(format!("other nodes are still members: {}; detach or remove them first", others.join(", "))));
        }
        let dir = self.state_dir();
        let snap = dir.join("glidex.db.final-snapshot");
        super::manage::export_snapshot(&self.db, &snap).map_err(failed)?;
        let me = self.identity.node_id.clone();
        let opts = DetachOptions { map_networks: BTreeMap::new(), with_access: true };
        let bundle = build_bundle(&self.db, "dissolve", &me, &opts);
        write_standalone_db(&pending_db(&dir), &bundle)?;
        pki::write_private(&pending_sha_file(&dir), bundle.sha256().as_bytes()).map_err(failed)?;
        let ca = self.signing_ca().ok_or_else(|| failed("this server holds no CA key"))?;
        let revision = self.db.revision();
        let sha = bundle.sha256();
        let signature = ca.sign_bytes(&Receipt::payload("dissolve", &me, &sha, revision)).map_err(failed)?;
        let receipt = Receipt { plan: "dissolve".into(), node: me.clone(), bundle_sha256: sha, revision, signature };
        pki::write_private(&receipt_file(&dir), &serde_json::to_vec(&receipt).map_err(failed)?).map_err(failed)?;
        tracing::warn!("this cluster is dissolved: restart the control plane to continue as a standalone host");
        if let Some(h) = self.handlers.get() {
            h.departed();
        }
        Ok(json!({ "node": self.identity.name, "vms": bundle.vm_ids.len(), "disks": bundle.disk_ids.len(), "snapshot": snap.display().to_string() }))
    }
}

/// Run on every node: take part in a departure.
impl Cluster {
    pub fn start_departure_tasks(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            let mut stop = me.stop_rx();
            loop {
                tokio::select! { _ = tick.tick() => {}, _ = stop.changed() => return }
                let round = async {
                    if me.is_leader() {
                        me.expire_departures();
                        me.expire_imports();
                    }
                    if me.departure_tick().await.is_some() {
                        tracing::warn!("this node's departure committed: restart the control plane to continue as a standalone host");
                        if let Some(h) = me.handlers.get() {
                            h.departed();
                        }
                    }
                };
                tokio::select! { _ = round => {}, _ = stop.changed() => return }
            }
        });
    }
}

/// Set by the daemon: exit once a departure committed, so the service manager
/// restarts it and startup completes the switch.
pub static EXIT_ON_DEPARTURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn is_server(n: &Node) -> bool {
    n.spec.role == NodeRole::Server
}
