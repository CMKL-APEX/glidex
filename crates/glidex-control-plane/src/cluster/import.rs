//! Joining with resources (spec/clustering.md §5.9): a standalone host with
//! VMs, disks, images, networks and credentials joins a cluster and brings
//! them in, under a new node id.
//!
//! The host uploads a bundle of its records (the same shape a detach writes).
//! The leader builds an **import plan**, checks it against the cluster as it
//! is, and stages the rows in chunks where no controller or API can see them.
//! An administrator approves with the mappings the checks asked for; the
//! approval validates again and then, in **one write**, creates the node, signs
//! its certificate and makes the rows live. Nothing is committed on either side
//! before that write.

use super::departure::{Bundle, Row};
use super::runtime::Cluster;
use super::tokens::{self, TokenKind};
use crate::node::{Node, NodeRole, NodeSpec, NodeStore};
use crate::store::{Db, Origin, TableId};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Invalid(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
}

fn failed(e: impl std::fmt::Display) -> ImportError {
    ImportError::Failed(e.to_string())
}

/// What the approver decides about names that clash (§5.9 step 3 table).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportMappings {
    /// Source project id → an existing project (id or name), or `new:<name>`.
    #[serde(default)]
    pub projects: BTreeMap<String, String>,
    /// Source node-network name → its new name.
    #[serde(default)]
    pub networks: BTreeMap<String, String>,
    /// `<source project id>/<username>` → its new username.
    #[serde(default)]
    pub credentials: BTreeMap<String, String>,
    /// Accept an import that takes a project over its quota.
    #[serde(default)]
    pub over_quota: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub vms: usize,
    pub disks: usize,
    pub images: usize,
    pub networks: usize,
    pub credentials: usize,
    pub projects: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportPlan {
    pub plan: String,
    /// The new node's id, name and address (from the host).
    pub node_id: String,
    pub name: String,
    pub advertise: SocketAddr,
    #[serde(default)]
    pub tunnel_ip: Option<IpAddr>,
    pub raft_id: u64,
    pub csr: String,
    pub created_by: String,
    pub created_at: u64,
    pub expires_at: u64,
    /// `pending`, `committed`, `rejected`.
    pub state: String,
    pub summary: Summary,
    /// What would stop it right now; recomputed on approval.
    pub problems: Vec<String>,
    pub mappings: ImportMappings,
    pub chunks: usize,
    /// SHA-256 of the secret the host polls with.
    pub poll_sha256: String,
    /// The host's way in once committed.
    #[serde(default)]
    pub result: Option<Value>,
}

const PLAN: &str = "import/";
const CHUNK_BYTES: usize = 1 << 20;

fn plan_key(plan: &str) -> String {
    format!("{PLAN}{plan}")
}

fn chunk_key(plan: &str, n: usize) -> String {
    format!("{PLAN}{plan}/rows/{n:06}")
}

pub fn read_plan(db: &Db, plan: &str) -> Option<ImportPlan> {
    let txn = db.begin_read().ok()?;
    let t = txn.open_table(TableId::Meta.definition()).ok()?;
    serde_json::from_slice(t.get(plan_key(plan).as_str()).ok()??.value()).ok()
}

pub fn plans(db: &Db) -> Vec<ImportPlan> {
    let Ok(txn) = db.begin_read() else { return Vec::new() };
    let Ok(t) = txn.open_table(TableId::Meta.definition()) else { return Vec::new() };
    let Ok(it) = t.range(PLAN..) else { return Vec::new() };
    it.flatten().take_while(|(k, _)| k.value().starts_with(PLAN)).filter(|(k, _)| !k.value()[PLAN.len()..].contains('/')).filter_map(|(_, v)| serde_json::from_slice(v.value()).ok()).collect()
}

fn read_rows(db: &Db, plan: &ImportPlan) -> Vec<Row> {
    let Ok(txn) = db.begin_read() else { return Vec::new() };
    let Ok(t) = txn.open_table(TableId::Meta.definition()) else { return Vec::new() };
    (0..plan.chunks).filter_map(|n| t.get(chunk_key(&plan.plan, n).as_str()).ok().flatten()).filter_map(|v| serde_json::from_slice::<Vec<Row>>(v.value()).ok()).flatten().collect()
}

fn table_rows(rows: &[Row], t: TableId) -> Vec<(String, Value)> {
    rows.iter().filter(|r| r.table == t as u16).filter_map(|r| Some((r.key.clone(), serde_json::from_str(&r.value).ok()?))).collect()
}

fn keys_of(db: &Db, t: TableId) -> BTreeSet<String> {
    let Ok(txn) = db.begin_read() else { return BTreeSet::new() };
    let Ok(tab) = txn.open_table(t.definition()) else { return BTreeSet::new() };
    let Ok(it) = tab.iter() else { return BTreeSet::new() };
    it.flatten().map(|(k, _)| k.value().to_string()).collect()
}

fn values_of(db: &Db, t: TableId) -> Vec<(String, Value)> {
    let Ok(txn) = db.begin_read() else { return Vec::new() };
    let Ok(tab) = txn.open_table(t.definition()) else { return Vec::new() };
    let Ok(it) = tab.iter() else { return Vec::new() };
    it.flatten().filter_map(|(k, v)| Some((k.value().to_string(), serde_json::from_slice(v.value()).ok()?))).collect()
}

/// Where each source project lands, and what the checks found.
struct Resolved {
    /// Source project id → destination project id.
    pmap: BTreeMap<String, String>,
    /// Source projects that become new projects here (id kept, name maybe changed).
    new_projects: BTreeMap<String, String>,
    problems: Vec<String>,
}

fn resolve(db: &Db, rows: &[Row], m: &ImportMappings) -> Resolved {
    let mut r = Resolved { pmap: BTreeMap::new(), new_projects: BTreeMap::new(), problems: Vec::new() };
    let projects = values_of(db, TableId::Projects);
    let by_id = |id: &str| projects.iter().find(|(k, _)| k == id).map(|(_, v)| v.clone());
    let by_name = |n: &str| projects.iter().find(|(_, v)| v["name"] == n).map(|(k, _)| k.clone());
    let departed: BTreeSet<String> = values_of(db, TableId::Nodes).into_iter().filter(|(_, n)| n["status"]["phase"] == "Departed").flat_map(|(_, n)| n["status"]["departed_ids"].as_array().cloned().unwrap_or_default()).filter_map(|x| x.as_str().map(String::from)).collect();

    // Ids are kept (D19): a clash is refused unless it is the same resource coming back.
    for (t, what) in [(TableId::Vms, "VM"), (TableId::Disks, "disk"), (TableId::Images, "image")] {
        let have = keys_of(db, t);
        for (k, _) in table_rows(rows, t) {
            if have.contains(&k) && !departed.contains(&k) {
                r.problems.push(format!("{what} {k} already exists in the cluster"));
            }
        }
    }
    for (id, p) in table_rows(rows, TableId::Projects) {
        let name = p["name"].as_str().unwrap_or("").to_string();
        match m.projects.get(&id) {
            Some(to) => {
                if let Some(new_name) = to.strip_prefix("new:") {
                    if by_name(new_name).is_some() {
                        r.problems.push(format!("project name '{new_name}' already exists"));
                    }
                    r.new_projects.insert(id.clone(), new_name.to_string());
                    r.pmap.insert(id.clone(), id);
                } else if let Some(dest) = by_id(to).map(|_| to.clone()).or_else(|| by_name(to)) {
                    r.pmap.insert(id, dest);
                } else {
                    r.problems.push(format!("project '{to}' (mapped from '{name}') does not exist"));
                }
            }
            None if by_id(&id).is_some() => {
                r.pmap.insert(id.clone(), id);
            }
            None if by_name(&name).is_some() => {
                r.problems.push(format!("project '{name}' exists with another id: map it with --project {id}=<existing> or {id}=new:<name>"));
            }
            None => {
                r.new_projects.insert(id.clone(), name);
                r.pmap.insert(id.clone(), id);
            }
        }
    }
    let nets = keys_of(db, TableId::Networks);
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for (name, _) in table_rows(rows, TableId::Networks) {
        let to = m.networks.get(&name).cloned().unwrap_or(name.clone());
        if nets.contains(&to) || !taken.insert(to.clone()) {
            r.problems.push(format!("network '{to}' already exists: rename it with --network {name}=<new name>"));
        }
    }
    let creds = keys_of(db, TableId::Credentials);
    for (key, c) in table_rows(rows, TableId::Credentials) {
        let src_project = c["project"].as_str().unwrap_or("").to_string();
        let dest = r.pmap.get(&src_project).cloned().unwrap_or(src_project.clone());
        let user = c["username"].as_str().unwrap_or("");
        let to = m.credentials.get(&key).cloned().unwrap_or(user.to_string());
        if creds.contains(&format!("{dest}/{to}")) {
            r.problems.push(format!("credential '{to}' already exists in project {dest}: rename it with --credential {key}=<new name>"));
        }
    }
    // Quotas of projects that already exist.
    if !m.over_quota {
        let mut add: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
        for (_, vm) in table_rows(rows, TableId::Vms) {
            let src = vm["meta"]["project"].as_str().unwrap_or("").to_string();
            let dest = r.pmap.get(&src).cloned().unwrap_or(src);
            let e = add.entry(dest).or_default();
            e.0 += 1;
            e.1 += vm.pointer("/spec/config/vcpu_count").and_then(|x| x.as_u64()).unwrap_or(0);
            e.2 += vm.pointer("/spec/config/mem_size_mib").and_then(|x| x.as_u64()).unwrap_or(0);
        }
        let existing_vms = values_of(db, TableId::Vms);
        for (dest, (vms, cpus, mem)) in add {
            let Some(p) = by_id(&dest) else { continue };
            let used = existing_vms.iter().filter(|(_, v)| v["meta"]["project"] == dest.as_str()).fold((0u64, 0u64, 0u64), |a, (_, v)| (a.0 + 1, a.1 + v.pointer("/spec/config/vcpu_count").and_then(|x| x.as_u64()).unwrap_or(0), a.2 + v.pointer("/spec/config/mem_size_mib").and_then(|x| x.as_u64()).unwrap_or(0)));
            for (what, limit, have, more) in [("vms", p["quotas"]["vms"].as_u64(), used.0, vms), ("vcpus", p["quotas"]["vcpus"].as_u64(), used.1, cpus), ("memory_mib", p["quotas"]["memory_mib"].as_u64(), used.2, mem)] {
                if let Some(l) = limit {
                    if have + more > l {
                        r.problems.push(format!("project {dest} would go over its {what} quota ({} of {l}): approve with --over-quota", have + more));
                    }
                }
            }
        }
    }
    r
}

fn summarize(rows: &[Row]) -> Summary {
    let n = |t: TableId| rows.iter().filter(|r| r.table == t as u16).count();
    Summary { vms: n(TableId::Vms), disks: n(TableId::Disks), images: n(TableId::Images), networks: n(TableId::Networks), credentials: n(TableId::Credentials), projects: n(TableId::Projects) }
}

/// The tables an import may bring rows for: resources only (§5.9). Access
/// data (users, links, identities), certificates, nodes and anything else
/// a host could send are refused before anything is staged.
const IMPORT_TABLES: [TableId; 8] = [TableId::Vms, TableId::Events, TableId::Disks, TableId::Networks, TableId::Images, TableId::ImageMeta, TableId::Credentials, TableId::Projects];

fn check_import_rows(b: &Bundle) -> Result<(), ImportError> {
    let vms: BTreeSet<&str> = b.rows.iter().filter(|r| r.table == TableId::Vms as u16).map(|r| r.key.as_str()).collect();
    for r in &b.rows {
        let Some(t) = TableId::from_id(r.table).filter(|t| IMPORT_TABLES.contains(t)) else {
            return Err(ImportError::Invalid(format!("an import can't bring rows of table {}", r.table)));
        };
        let v: Value = serde_json::from_str(&r.value).map_err(|_| ImportError::Invalid(format!("row {} is not JSON", r.key)))?;
        match t {
            TableId::Events if !r.key.strip_prefix("vm/").is_some_and(|id| vms.contains(id)) => return Err(ImportError::Invalid(format!("events {} are not of an imported VM", r.key))),
            TableId::Networks if v["scope"] == "cluster" => return Err(ImportError::Invalid(format!("network {} is a cluster network", r.key))),
            _ => {}
        }
    }
    Ok(())
}

/// What the host sends to ask for an import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRequest {
    pub token: String,
    pub csr: String,
    pub name: String,
    pub node_id: String,
    pub raft_id: u64,
    pub advertise: SocketAddr,
    #[serde(default)]
    pub tunnel_ip: Option<IpAddr>,
    pub bundle: Bundle,
}

fn digest(s: &str) -> String {
    super::pki::hex(&Sha256::digest(s.as_bytes()))
}

impl Cluster {
    /// Step 3 (leader): consume the token, check, and stage. Returns the plan id
    /// and the secret the host polls with.
    pub fn create_import_plan(&self, req: ImportRequest) -> Result<(ImportPlan, String), ImportError> {
        check_import_rows(&req.bundle)?;
        let rec = tokens::consume(&self.db, &req.token).map_err(|e| ImportError::Invalid(e.to_string()))?;
        match rec.kind {
            TokenKind::Join { role: NodeRole::Agent, allow_import: true } => {}
            TokenKind::Join { allow_import: false, .. } => return Err(ImportError::Invalid("this token can't import: create it with --allow-import".into())),
            _ => return Err(ImportError::Invalid("an import joins as an agent; promote it to a server later".into())),
        }
        let existing = NodeStore::new(self.db.clone()).list().map_err(failed)?;
        if existing.iter().any(|n| n.meta.id == req.node_id || (!n.status.phase.is_tombstone() && n.spec.name == req.name)) {
            return Err(ImportError::Conflict(format!("a node named {} or with that id already exists", req.name)));
        }
        let plan_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let secret = uuid::Uuid::new_v4().simple().to_string();
        let now = crate::tenancy::now();
        let rows = req.bundle.rows.clone();
        let resolved = resolve(&self.db, &rows, &ImportMappings::default());
        let mut chunks: Vec<Vec<Row>> = vec![Vec::new()];
        let mut size = 0;
        for r in rows.iter().cloned() {
            let l = r.key.len() + r.value.len();
            if size + l > CHUNK_BYTES && !chunks.last().unwrap().is_empty() {
                chunks.push(Vec::new());
                size = 0;
            }
            size += l;
            chunks.last_mut().unwrap().push(r);
        }
        let plan = ImportPlan {
            plan: plan_id.clone(),
            node_id: req.node_id,
            name: req.name,
            advertise: req.advertise,
            tunnel_ip: req.tunnel_ip,
            raft_id: req.raft_id,
            csr: req.csr,
            created_by: rec.created_by,
            created_at: now,
            expires_at: now + self.config.import_plan_ttl_secs,
            state: "pending".into(),
            summary: summarize(&rows),
            problems: resolved.problems,
            mappings: ImportMappings::default(),
            chunks: chunks.len(),
            poll_sha256: digest(&secret),
            result: None,
        };
        // Staged in writes of their own (each below the entry limit), then the plan that names them.
        for (n, c) in chunks.iter().enumerate() {
            let body = serde_json::to_vec(c).map_err(failed)?;
            let key = chunk_key(&plan_id, n);
            self.db
                .write(Origin::Api, |tx| -> Result<(), crate::store::StoreError> {
                    tx.open_table(TableId::Meta.definition())?.insert(key.as_str(), body.as_slice())?;
                    Ok(())
                })
                .map_err(failed)?;
        }
        self.put_plan(&plan)?;
        Ok((plan, secret))
    }

    fn put_plan(&self, p: &ImportPlan) -> Result<(), ImportError> {
        self.db
            .write(Origin::Api, |tx| -> Result<(), crate::store::StoreError> {
                tx.open_table(TableId::Meta.definition())?.insert(plan_key(&p.plan).as_str(), serde_json::to_vec(p).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?.as_slice())?;
                Ok(())
            })
            .map_err(failed)
    }

    /// Re-run the checks with `mappings` and store them on the plan.
    pub fn check_import(&self, plan_id: &str, mappings: ImportMappings) -> Result<ImportPlan, ImportError> {
        let mut p = read_plan(&self.db, plan_id).ok_or_else(|| ImportError::NotFound(plan_id.into()))?;
        if p.state != "pending" {
            return Err(ImportError::Conflict(format!("the plan is {}", p.state)));
        }
        let rows = read_rows(&self.db, &p);
        p.problems = resolve(&self.db, &rows, &mappings).problems;
        p.mappings = mappings;
        self.put_plan(&p)?;
        Ok(p)
    }

    /// Steps 4–5: validate against the cluster as it is now, then one write.
    pub fn approve_import(&self, plan_id: &str, mappings: ImportMappings) -> Result<ImportPlan, ImportError> {
        let p = read_plan(&self.db, plan_id).ok_or_else(|| ImportError::NotFound(plan_id.into()))?;
        if p.state != "pending" {
            return Err(ImportError::Conflict(format!("the plan is {}", p.state)));
        }
        let ca = self.signing_ca().ok_or_else(|| failed("this server holds no CA key"))?;
        let (cert_pem, info) = ca.sign_node(&p.csr, &p.node_id, false, super::pki::NODE_VALIDITY_DAYS).map_err(|e| ImportError::Invalid(e.to_string()))?;
        let trust_pem = self.ca_state().map(|s| s.trust_pem).or_else(|| self.files.read(self.files.trust()).ok()).unwrap_or_default();
        let cluster_id = self.identity.cluster_id.clone();
        // Validated and committed in one write, against the state of that write:
        // an expiry, a reject or another import can't land in between.
        let refused = std::cell::RefCell::new(None::<(ImportError, Option<ImportPlan>)>);
        let done = self
            .db
            .write(Origin::Api, |tx| -> Result<Option<ImportPlan>, crate::store::StoreError> {
                let io = |e: serde_json::Error| crate::store::StoreError::Io(std::io::Error::other(e.to_string()));
                let mut p: ImportPlan = match tx.open_table(TableId::Meta.definition())?.get(plan_key(plan_id).as_str())?.and_then(|v| serde_json::from_slice(v.value()).ok()) {
                    Some(p) => p,
                    None => {
                        *refused.borrow_mut() = Some((ImportError::NotFound(plan_id.into()), None));
                        return Ok(None);
                    }
                };
                if p.state != "pending" {
                    *refused.borrow_mut() = Some((ImportError::Conflict(format!("the plan is {}", p.state)), None));
                    return Ok(None);
                }
                if crate::tenancy::now() > p.expires_at {
                    *refused.borrow_mut() = Some((ImportError::Conflict("the plan expired".into()), None));
                    return Ok(None);
                }
                let rows = read_rows(&self.db, &p);
                let res = resolve(&self.db, &rows, &mappings);
                if !res.problems.is_empty() {
                    p.problems = res.problems.clone();
                    p.mappings = mappings.clone();
                    *refused.borrow_mut() = Some((ImportError::Conflict(format!("the import can't go ahead: {}", res.problems.join("; "))), Some(p)));
                    return Ok(None);
                }
                let live = rewrite(rows, &res, &mappings, &p.node_id);
                let mut node = Node::new(&p.node_id, NodeSpec { name: p.name.clone(), role: NodeRole::Agent, unschedulable: false, labels: Default::default() });
                node.status.advertise = Some(p.advertise);
                node.status.tunnel_ip = p.tunnel_ip.or(Some(p.advertise.ip()));
                // Draining until the node confirms (§5.9 step 5): its own report makes it Active.
                node.status.phase = crate::node::NodePhase::Draining;
                node.spec.unschedulable = true;
                p.state = "committed".into();
                p.mappings = mappings.clone();
                p.problems.clear();
                p.result = Some(json!({ "cluster_id": cluster_id, "cert_pem": cert_pem, "trust_pem": trust_pem }));
                tx.open_table(TableId::Nodes.definition())?.insert(node.meta.id.as_str(), serde_json::to_vec(&node).map_err(io)?.as_slice())?;
                tx.open_table(TableId::IssuedCerts.definition())?
                    .insert(info.serial.as_str(), serde_json::to_vec(&json!({ "node": info.node_id, "kind": "node", "issuer": info.issuer_fingerprint, "not_after": info.not_after })).map_err(io)?.as_slice())?;
                for r in &live {
                    let t = TableId::from_id(r.table).ok_or_else(|| crate::store::StoreError::Io(std::io::Error::other("unknown table")))?;
                    tx.open_table(t.definition())?.insert(r.key.as_str(), r.value.as_bytes())?;
                }
                let mut meta = tx.open_table(TableId::Meta.definition())?;
                for n in 0..p.chunks {
                    meta.remove(chunk_key(&p.plan, n).as_str())?;
                }
                meta.insert(plan_key(&p.plan).as_str(), serde_json::to_vec(&p).map_err(io)?.as_slice())?;
                Ok(Some(p))
            })
            .map_err(failed)?;
        if let Some((e, plan)) = refused.into_inner() {
            if let Some(plan) = plan {
                let _ = self.put_plan(&plan);
            }
            return Err(e);
        }
        let p = done.ok_or_else(|| failed("the import was not committed"))?;
        tracing::warn!(node = %p.name, vms = p.summary.vms, "an import was approved and committed");
        Ok(p)
    }

    pub fn reject_import(&self, plan_id: &str) -> Result<(), ImportError> {
        let refused = std::cell::RefCell::new(None::<ImportError>);
        self.db
            .write(Origin::Api, |tx| -> Result<(), crate::store::StoreError> {
                let mut meta = tx.open_table(TableId::Meta.definition())?;
                let cur: Option<ImportPlan> = meta.get(plan_key(plan_id).as_str())?.and_then(|v| serde_json::from_slice(v.value()).ok());
                let Some(mut p) = cur else {
                    *refused.borrow_mut() = Some(ImportError::NotFound(plan_id.into()));
                    return Ok(());
                };
                // Re-read inside the write: an approval may have committed since.
                if p.state != "pending" {
                    *refused.borrow_mut() = Some(ImportError::Conflict(format!("the plan is {}", p.state)));
                    return Ok(());
                }
                p.state = "rejected".into();
                for n in 0..p.chunks {
                    meta.remove(chunk_key(plan_id, n).as_str())?;
                }
                meta.insert(plan_key(plan_id).as_str(), serde_json::to_vec(&p).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?.as_slice())?;
                Ok(())
            })
            .map_err(failed)?;
        match refused.into_inner() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// What approving with `mappings` amounts to (§12.2): the existing projects
    /// the import merges into, and whether it creates projects.
    pub fn plan_destinations(&self, plan_id: &str, mappings: &ImportMappings) -> Result<(Vec<String>, bool), ImportError> {
        let p = read_plan(&self.db, plan_id).ok_or_else(|| ImportError::NotFound(plan_id.into()))?;
        let rows = read_rows(&self.db, &p);
        let res = resolve(&self.db, &rows, mappings);
        let merged: BTreeSet<String> = res.pmap.iter().filter(|(src, _)| !res.new_projects.contains_key(*src)).map(|(_, dest)| dest.clone()).collect();
        Ok((merged.into_iter().collect(), !res.new_projects.is_empty()))
    }

    /// The host gave up waiting (§5.9): reject the plan if it is still
    /// pending; if it already committed, say so, so the host switches anyway.
    pub fn cancel_import(&self, plan_id: &str, secret: &str) -> Result<Option<ImportPlan>, ImportError> {
        let p = self.poll_import(plan_id, secret).ok_or_else(|| ImportError::NotFound(plan_id.into()))?;
        match self.reject_import(plan_id) {
            Ok(()) => Ok(None),
            Err(ImportError::Conflict(_)) => Ok(read_plan(&self.db, &p.plan).filter(|p| p.state == "committed")),
            Err(e) => Err(e),
        }
    }

    /// Leader: a plan past its time is rejected and its staged rows dropped.
    pub fn expire_imports(&self) {
        let now = crate::tenancy::now();
        for p in plans(&self.db).into_iter().filter(|p| p.state == "pending" && p.expires_at < now) {
            let _ = self.reject_import(&p.plan);
        }
    }

    /// The host's poll: only with the secret it was given.
    pub fn poll_import(&self, plan_id: &str, secret: &str) -> Option<ImportPlan> {
        let p = read_plan(&self.db, plan_id)?;
        crate::auth::constant_eq(&p.poll_sha256, &digest(secret)).then_some(p)
    }
}

/// The rows as the cluster stores them: ids kept; node, project, network and
/// credential names as the approver mapped them.
fn rewrite(rows: Vec<Row>, res: &Resolved, m: &ImportMappings, node: &str) -> Vec<Row> {
    let pmap = |p: &str| res.pmap.get(p).cloned().unwrap_or_else(|| p.to_string());
    let nmap = |n: &str| m.networks.get(n).cloned().unwrap_or_else(|| n.to_string());
    let cname = |proj: &str, u: &str| m.credentials.get(&format!("{proj}/{u}")).cloned().unwrap_or_else(|| u.to_string());
    let mut out = Vec::new();
    for r in rows {
        let Ok(mut v) = serde_json::from_str::<Value>(&r.value) else { continue };
        let mut key = r.key.clone();
        match TableId::from_id(r.table) {
            Some(TableId::Vms) => {
                let src = v["meta"]["project"].as_str().unwrap_or("").to_string();
                v["meta"]["project"] = pmap(&src).into();
                v["status"]["placement"]["node"] = node.into();
                if let Some(list) = v.pointer_mut("/spec/config/networks").and_then(|l| l.as_array_mut()) {
                    for a in list {
                        if let Some(n) = a["network"].as_str().map(nmap) {
                            a["network"] = n.into();
                        }
                    }
                }
                if let Some(c) = v.pointer("/spec/config/credential").and_then(|c| c.as_str()).map(|u| cname(&src, u)) {
                    v["spec"]["config"]["credential"] = c.into();
                }
            }
            Some(TableId::Disks) => {
                let src = v["project"].as_str().unwrap_or("").to_string();
                v["project"] = pmap(&src).into();
                v["node"] = node.into();
            }
            Some(TableId::Networks) => {
                let name = nmap(&r.key);
                key = name.clone();
                v["name"] = name.into();
                v["node"] = node.into();
                if let Some(p) = v["project"].as_str().map(pmap) {
                    v["project"] = p.into();
                }
            }
            Some(TableId::Credentials) => {
                let src = v["project"].as_str().unwrap_or("").to_string();
                let user = v["username"].as_str().unwrap_or("").to_string();
                let (dest, to) = (pmap(&src), cname(&src, &user));
                key = format!("{dest}/{to}");
                v["project"] = dest.into();
                v["username"] = to.into();
            }
            Some(TableId::Projects) => {
                // A project that already exists is merged into, not replaced.
                match res.new_projects.get(&r.key) {
                    Some(name) => v["name"] = name.clone().into(),
                    None => continue,
                }
            }
            Some(TableId::Events) | Some(TableId::Images) | Some(TableId::ImageMeta) => {}
            // Nothing else is imported (checked at staging; again here).
            _ => continue,
        }
        out.push(Row { table: r.table, key, value: v.to_string() });
    }
    out
}

pub fn is_pending(p: &ImportPlan) -> bool {
    p.state == "pending"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(rows: Vec<(TableId, &str, Value)>) -> Bundle {
        Bundle {
            plan: "p".into(),
            node: "local".into(),
            rows: rows.into_iter().map(|(t, k, v)| Row { table: t as u16, key: k.into(), value: v.to_string() }).collect(),
            vm_ids: vec![],
            disk_ids: vec![],
            image_ids: vec![],
            mapped: Default::default(),
            dropped_networks: vec![],
        }
    }

    #[test]
    fn an_import_brings_resources_and_nothing_else() {
        let vm = json!({ "meta": { "project": "p1" } });
        assert!(check_import_rows(&bundle(vec![(TableId::Vms, "v1", vm.clone()), (TableId::Events, "vm/v1", json!([]))])).is_ok());
        for (t, k) in [(TableId::Users, "u1"), (TableId::PolicyLinks, "l1"), (TableId::IssuedCerts, "s"), (TableId::Nodes, "n"), (TableId::CaBundle, "state"), (TableId::Meta, "default_project")] {
            assert!(check_import_rows(&bundle(vec![(t, k, json!({}))])).is_err(), "{t:?} must be refused");
        }
        assert!(check_import_rows(&bundle(vec![(TableId::Events, "vm/someone-else", json!([]))])).is_err(), "events of a VM it doesn't bring");
        assert!(check_import_rows(&bundle(vec![(TableId::Networks, "n", json!({ "scope": "cluster" }))])).is_err());
    }
}
