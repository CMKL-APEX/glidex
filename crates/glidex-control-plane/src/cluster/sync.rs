//! What a server gives its nodes (spec/clustering.md §6.5, §8, §12.3): a
//! filtered copy of the store (`list`), the changes after a revision
//! (`watch`), heartbeats, and the rules for the status writes a node may
//! make. A node sees and changes only what is placed or bound on it.

use super::runtime::Cluster;
use crate::store::{Db, Op, Origin, StoreError, TableId, WRITE_SET_FORMAT, WriteSet};
use redb::ReadableTable;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One write log entry: its revision and the writes.
pub type Entry = (u64, Arc<WriteSet>);

/// Recent applied write sets, for watches. A node that asks for older ones
/// re-lists (`410 Gone`).
pub struct WriteLog {
    inner: Mutex<LogInner>,
    notify: tokio::sync::watch::Sender<u64>,
    cap: usize,
}

struct LogInner {
    entries: VecDeque<(u64, Arc<WriteSet>)>,
    /// Revision up to which the log is complete from `first`.
    last: u64,
    first: u64,
}

impl WriteLog {
    pub fn new(cap: usize) -> Arc<WriteLog> {
        Arc::new(WriteLog {
            inner: Mutex::new(LogInner {
                entries: VecDeque::new(),
                last: 0,
                first: 0,
            }),
            notify: tokio::sync::watch::channel(0).0,
            cap,
        })
    }

    /// Follow `db` from now on.
    pub fn follow(self: &Arc<Self>, db: &Db) {
        let me = self.clone();
        let mut rx = db.subscribe();
        let start = db.revision();
        {
            let mut g = me.inner.lock().unwrap();
            g.first = start;
            g.last = start;
        }
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(a) => me.push(a.revision, a.ws),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => me.reset(),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    fn push(&self, rev: u64, ws: Arc<WriteSet>) {
        let mut g = self.inner.lock().unwrap();
        g.entries.push_back((rev, ws));
        g.last = rev;
        while g.entries.len() > self.cap {
            if let Some((i, _)) = g.entries.pop_front() {
                g.first = i;
            }
        }
        drop(g);
        let _ = self.notify.send(rev);
    }

    fn reset(&self) {
        let mut g = self.inner.lock().unwrap();
        g.entries.clear();
        g.first = g.last + 1_000_000_000;
    }

    /// Entries after `from`, or `None` if `from` is older than the log.
    /// The returned revision is how far the log has seen.
    pub fn after(&self, from: u64, max: usize) -> Option<(u64, Vec<Entry>)> {
        let g = self.inner.lock().unwrap();
        if from < g.first {
            return None;
        }
        let out: Vec<_> = g
            .entries
            .iter()
            .filter(|(i, _)| *i > from)
            .take(max)
            .cloned()
            .collect();
        let upto = out.last().map(|(i, _)| *i).unwrap_or(g.last);
        Some((upto.max(from), out))
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.notify.subscribe()
    }

    pub fn last(&self) -> u64 {
        self.inner.lock().unwrap().last
    }
}

fn get_json(db: &Db, table: TableId, key: &str) -> Option<Value> {
    let txn = db.begin_read().ok()?;
    let t = txn.open_table(table.definition()).ok()?;
    let v = t.get(key).ok()??;
    serde_json::from_slice(v.value()).ok()
}

fn placed_on(rec: &Value, node: &str) -> bool {
    rec.pointer("/status/placement/node")
        .and_then(|n| n.as_str())
        == Some(node)
}

/// Ids of the VMs placed on `node`.
pub fn node_vms(db: &Db, node: &str) -> Vec<(String, Value)> {
    let Ok(txn) = db.begin_read() else {
        return Vec::new();
    };
    let Ok(t) = txn.open_table(TableId::Vms.definition()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Ok(it) = t.iter() {
        for r in it.flatten() {
            if let Ok(v) = serde_json::from_slice::<Value>(r.1.value())
                && placed_on(&v, node)
            {
                out.push((r.0.value().to_string(), v));
            }
        }
    }
    out
}

/// Tables a node receives in full: catalogs and names that hold no secrets.
const SHARED: [TableId; 8] = [
    TableId::Projects,
    TableId::Meta,
    TableId::Images,
    TableId::Networks,
    TableId::Nodes,
    TableId::ImageMeta,
    TableId::ImageCaches,
    TableId::CaBundle,
];

/// Shared like `SHARED`, but only with nodes that know these tables
/// (`DATASTORE_FEATURE_LEVEL`): an older agent can't read a write set that
/// names a table it doesn't know (spec/clustering.md §6.6).
const SHARED_DATASTORES: [TableId; 2] = [TableId::Datastores, TableId::DatastoreNodes];

/// Whether `node` acts on disk `d`: bound to it, or (a shared disk, bound
/// to none) its worker (spec/storage.md S6).
fn disk_is_nodes(d: &Value, node: &str) -> bool {
    d["node"] == node || (d["node"].is_null() && d["worker"] == node)
}

/// The feature level `node` reports (1 when it says nothing).
fn node_level(db: &Db, node: &str) -> u32 {
    get_json(db, TableId::Nodes, node)
        .and_then(|n| n.pointer("/status/features/feature_level")?.as_u64())
        .map(|l| l as u32)
        .unwrap_or(1)
}

/// The part of `ws` a node may see: shared catalogs, and the VMs, disks,
/// events and credentials that are its own. Deletions of keys in its tables
/// pass (an id reveals nothing).
pub fn filter_for(db: &Db, node: &str, ws: &WriteSet) -> WriteSet {
    let mut mine: Option<HashSet<String>> = None;
    let mut creds: Option<HashSet<String>> = None;
    let mut level: Option<u32> = None;
    let mut ops = Vec::new();
    for op in &ws.ops {
        let t = op.table();
        let key = String::from_utf8_lossy(op.key()).into_owned();
        let keep = if SHARED.contains(&t) {
            true
        } else if SHARED_DATASTORES.contains(&t) {
            *level.get_or_insert_with(|| node_level(db, node))
                >= crate::datastore::record::DATASTORE_FEATURE_LEVEL
        } else {
            match (t, op) {
                (TableId::Vms, Op::Put { value, .. }) => serde_json::from_slice::<Value>(value)
                    .map(|v| placed_on(&v, node))
                    .unwrap_or(false),
                (TableId::Vms, Op::Delete { .. }) => true,
                // A shared disk goes to every node: when the leader moves its
                // worker, the old one must hear it is no longer (S6).
                (TableId::Disks, Op::Put { value, .. }) => serde_json::from_slice::<Value>(value)
                    .map(|v| {
                        disk_is_nodes(&v, node) || (v["node"].is_null() && !v["worker"].is_null())
                    })
                    .unwrap_or(false),
                (TableId::Disks, Op::Delete { .. }) => true,
                (TableId::Events, Op::Delete { .. }) => true,
                (TableId::Events, Op::Put { .. }) => {
                    let m = mine.get_or_insert_with(|| {
                        node_vms(db, node).into_iter().map(|(id, _)| id).collect()
                    });
                    let local_put = ws.ops.iter().filter_map(|o| match o {
                        Op::Put {
                            table: TableId::Vms,
                            key,
                            value,
                        } if serde_json::from_slice::<Value>(value)
                            .map(|v| placed_on(&v, node))
                            .unwrap_or(false) =>
                        {
                            Some(String::from_utf8_lossy(key).into_owned())
                        }
                        _ => None,
                    });
                    let ids: HashSet<String> = local_put.collect();
                    match key.split_once('/') {
                        Some(("vm", id)) => m.contains(id) || ids.contains(id),
                        Some(("disk", id)) => get_json(db, TableId::Disks, id)
                            .is_some_and(|d| disk_is_nodes(&d, node)),
                        _ => false,
                    }
                }
                (TableId::Credentials, Op::Delete { .. }) => true,
                (TableId::Credentials, Op::Put { .. }) => {
                    let c = creds.get_or_insert_with(|| {
                        node_vms(db, node)
                            .into_iter()
                            .filter_map(|(_, v)| {
                                Some(format!(
                                    "{}/{}",
                                    v.pointer("/meta/project")?.as_str()?,
                                    v.pointer("/spec/config/credential")?.as_str()?
                                ))
                            })
                            .collect()
                    });
                    c.contains(&key)
                }
                _ => false,
            }
        };
        if keep {
            ops.push(op.clone());
        }
    }
    // A VM placed here brings the credential it names: written before
    // the VM was, that credential concerned no VM of this node then.
    let sent: HashSet<Vec<u8>> = ops
        .iter()
        .filter(|o| o.table() == TableId::Credentials)
        .map(|o| o.key().to_vec())
        .collect();
    let mut brought = Vec::new();
    for op in &ops {
        if let Op::Put {
            table: TableId::Vms,
            value,
            ..
        } = op
        {
            let Ok(v) = serde_json::from_slice::<Value>(value) else {
                continue;
            };
            let (Some(project), Some(user)) = (
                v.pointer("/meta/project").and_then(Value::as_str),
                v.pointer("/spec/config/credential").and_then(Value::as_str),
            ) else {
                continue;
            };
            let key = format!("{project}/{user}");
            if sent.contains(key.as_bytes())
                || brought.iter().any(|o: &Op| o.key() == key.as_bytes())
            {
                continue;
            }
            if let Some(c) = get_raw(db, TableId::Credentials, &key) {
                brought.push(Op::Put {
                    table: TableId::Credentials,
                    key: key.into_bytes(),
                    value: c,
                });
            }
        }
    }
    // Before the VMs, so the credential is there when the VM arrives.
    brought.extend(ops);
    WriteSet {
        format: ws.format,
        origin: ws.origin,
        ops: brought,
    }
}

fn get_raw(db: &Db, table: TableId, key: &str) -> Option<Vec<u8>> {
    let txn = db.begin_read().ok()?;
    let t = txn.open_table(table.definition()).ok()?;
    t.get(key).ok()?.map(|v| v.value().to_vec())
}

/// Everything `node` is entitled to, as one write set (a `list`).
pub fn snapshot_for(db: &Db, node: &str) -> Result<WriteSet, StoreError> {
    let mut all = WriteSet {
        format: WRITE_SET_FORMAT,
        origin: Origin::System,
        ops: Vec::new(),
    };
    let txn = db.begin_read()?;
    for &t in TableId::ALL.iter().filter(|t| !t.is_local()) {
        let relevant = SHARED.contains(&t)
            || SHARED_DATASTORES.contains(&t)
            || matches!(
                t,
                TableId::Vms | TableId::Disks | TableId::Events | TableId::Credentials
            );
        if !relevant {
            continue;
        }
        let table = txn.open_table(t.definition())?;
        for r in table.iter()? {
            let (k, v) = r?;
            all.ops.push(Op::Put {
                table: t,
                key: k.value().as_bytes().to_vec(),
                value: v.value().to_vec(),
            });
        }
    }
    Ok(filter_for(db, node, &all))
}

/// Liveness (§7.2), kept in the leader's memory.
pub struct Liveness {
    seen: Mutex<HashMap<String, Instant>>,
    /// When this leader took over: every node's grace starts then (D7).
    since: Mutex<Instant>,
}

impl Default for Liveness {
    fn default() -> Self {
        Liveness {
            seen: Mutex::new(HashMap::new()),
            since: Mutex::new(Instant::now()),
        }
    }
}

impl Liveness {
    pub fn beat(&self, node: &str) {
        self.seen
            .lock()
            .unwrap()
            .insert(node.to_string(), Instant::now());
    }

    pub fn new_term(&self) {
        *self.since.lock().unwrap() = Instant::now();
    }

    /// How long since `node` was last heard from (or since this leader began).
    pub fn silent_for(&self, node: &str) -> Duration {
        let since = *self.since.lock().unwrap();
        let last = self
            .seen
            .lock()
            .unwrap()
            .get(node)
            .copied()
            .unwrap_or(since)
            .max(since);
        last.elapsed()
    }
}

/// Reject what a node may not write (§8.3, §12.3); run inside the write, so
/// the state it checks is the state the write lands on.
pub fn check_status_write(
    tx: &crate::store::Tx<'_>,
    node: &str,
    ws: &WriteSet,
) -> Result<(), String> {
    let table = |t: TableId| tx.open_table(t.definition()).map_err(|e| e.to_string());
    let get = |t: TableId, key: &str| -> Result<Option<Value>, String> {
        let tb = table(t)?;
        let v = tb.get(key).map_err(|e| e.to_string())?;
        Ok(v.and_then(|v| serde_json::from_slice::<Value>(v.value()).ok()))
    };
    for op in &ws.ops {
        let key = std::str::from_utf8(op.key()).map_err(|_| "non-text key".to_string())?;
        match (op.table(), op) {
            (TableId::Vms, op) => {
                let cur =
                    get(TableId::Vms, key)?.ok_or_else(|| format!("VM {key} does not exist"))?;
                if !placed_on(&cur, node) {
                    return Err(format!("VM {key} is not placed on this node"));
                }
                match op {
                    Op::Delete { .. } => {
                        if cur
                            .pointer("/meta/deletion_requested_at")
                            .is_none_or(|v| v.is_null())
                        {
                            return Err(format!("VM {key} has not been asked to be deleted"));
                        }
                    }
                    Op::Put { value, .. } => {
                        let new: Value =
                            serde_json::from_slice(value).map_err(|e| e.to_string())?;
                        check_vm_status_only(&cur, &new)?;
                    }
                }
            }
            (TableId::Disks, op) => {
                let cur = get(TableId::Disks, key)?
                    .ok_or_else(|| format!("disk {key} does not exist"))?;
                if !disk_is_nodes(&cur, node) {
                    return Err(format!("disk {key} is not bound to this node"));
                }
                if let Op::Put { value, .. } = op {
                    let new: Value = serde_json::from_slice(value).map_err(|e| e.to_string())?;
                    check_disk_status_only(&cur, &new)?;
                }
            }
            (TableId::Networks, op) => {
                let cur = get(TableId::Networks, key)?
                    .ok_or_else(|| format!("network {key} does not exist"))?;
                if cur["node"] != node {
                    return Err(format!("network {key} belongs to another node"));
                }
                match op {
                    Op::Delete { .. } => {
                        if cur.get("deletion_requested_at").is_none_or(|v| v.is_null()) {
                            return Err(format!("network {key} has not been asked to be deleted"));
                        }
                    }
                    Op::Put { value, .. } => {
                        let new: Value =
                            serde_json::from_slice(value).map_err(|e| e.to_string())?;
                        same_except(&cur, &new, &["phase", "conditions", "owns_bridge"])?;
                    }
                }
            }
            (TableId::Events, _) => match key.split_once('/') {
                Some(("vm", id)) => {
                    let cur = get(TableId::Vms, id)?;
                    // The event ring of a VM the same write deletes is deleted with it.
                    let deleted_here = ws.ops.iter().any(|o| matches!(o, Op::Delete { table: TableId::Vms, key } if key == id.as_bytes()));
                    if !deleted_here && !cur.is_some_and(|v| placed_on(&v, node)) {
                        return Err(format!("VM {id} is not placed on this node"));
                    }
                }
                Some(("disk", id)) => {
                    if !get(TableId::Disks, id)?.is_some_and(|d| disk_is_nodes(&d, node)) {
                        return Err(format!("disk {id} is not bound to this node"));
                    }
                }
                _ => return Err(format!("events {key}: not a node's to write")),
            },
            (TableId::ImageCaches, _) => {
                let whose = key.rsplit_once('/').map(|(_, n)| n).unwrap_or_default();
                // A shared datastore's copy: any node that reaches it may say
                // (spec/storage.md S9).
                if let Some(ds) = whose.strip_prefix(crate::controller::store_copies::DS_PREFIX) {
                    let reaches = get(TableId::DatastoreNodes, &format!("{ds}/{node}"))?
                        .is_some_and(|r| r["reachable"] == true);
                    if !reaches {
                        return Err(format!("{key}: a datastore this node doesn't reach"));
                    }
                } else if whose != node {
                    return Err(format!("{key}: another node's image cache"));
                }
            }
            (TableId::DatastoreNodes, _) => {
                if !key.rsplit_once('/').is_some_and(|(_, n)| n == node) {
                    return Err(format!("{key}: another node's datastore row"));
                }
            }
            (TableId::Nodes, op) => {
                if key != node {
                    return Err("a node writes only its own record".into());
                }
                if let Op::Put { value, .. } = op {
                    let cur = get(TableId::Nodes, key)?.ok_or("no such node")?;
                    let new: Value = serde_json::from_slice(value).map_err(|e| e.to_string())?;
                    same_except(&cur, &new, &["status", "meta"])?;
                    for f in ["phase", "departed_ids", "raft_id"] {
                        if cur["status"][f] != new["status"][f] {
                            return Err(format!("a node may not change its own status.{f}"));
                        }
                    }
                }
            }
            (t, _) => return Err(format!("a node may not write {}", t.name())),
        }
    }
    Ok(())
}

/// `new` equals `cur` but for the listed top-level fields.
fn same_except(cur: &Value, new: &Value, except: &[&str]) -> Result<(), String> {
    let (Some(c), Some(n)) = (cur.as_object(), new.as_object()) else {
        return Err("not a record".into());
    };
    let keys: HashSet<&String> = c.keys().chain(n.keys()).collect();
    for k in keys {
        if except.contains(&k.as_str()) {
            continue;
        }
        if c.get(k).unwrap_or(&Value::Null) != n.get(k).unwrap_or(&Value::Null) {
            return Err(format!("field {k} is not the node's to change"));
        }
    }
    Ok(())
}

/// A node writes a VM's `status` (and, per D12, `spec.power = stopped` when
/// the generation it saw is current): never anything else.
fn check_vm_status_only(cur: &Value, new: &Value) -> Result<(), String> {
    let rv = |v: &Value| {
        v.pointer("/meta/resource_version")
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    if rv(new) != rv(cur) + 1 {
        return Err(format!(
            "conflict: the VM changed (resource_version {} is not {}+1)",
            rv(new),
            rv(cur)
        ));
    }
    if cur["status"]["placement"] != new["status"]["placement"] {
        return Err("a node may not move a VM".into());
    }
    let power_stop = cur.pointer("/spec/power") != new.pointer("/spec/power")
        && new.pointer("/spec/power").and_then(|p| p.as_str()) == Some("stopped");
    let generation = |v: &Value| {
        v.pointer("/meta/generation")
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
    };
    if power_stop {
        if generation(new) != generation(cur) + 1 {
            return Err("spec.power changes with a generation bump".into());
        }
        let (mut c, mut n) = (cur.clone(), new.clone());
        for v in [&mut c, &mut n] {
            v["spec"]["power"] = Value::Null;
            v["meta"]["generation"] = Value::Null;
            v["meta"]["resource_version"] = Value::Null;
            v["status"] = Value::Null;
        }
        return if c == n {
            Ok(())
        } else {
            Err("only spec.power may change besides status".into())
        };
    }
    if cur["spec"] != new["spec"] {
        return Err("a node may not change a VM's spec".into());
    }
    let (mut c, mut n) = (cur["meta"].clone(), new["meta"].clone());
    // Finalizers are removed as their cleanup finishes.
    let (cf, nf) = (
        c["finalizers"].as_array().cloned().unwrap_or_default(),
        n["finalizers"].as_array().cloned().unwrap_or_default(),
    );
    if !nf.iter().all(|f| cf.contains(f)) {
        return Err("a node may only remove finalizers".into());
    }
    for m in [&mut c, &mut n] {
        m["finalizers"] = Value::Null;
        m["resource_version"] = Value::Null;
    }
    if c != n {
        return Err("a node may not change a VM's meta".into());
    }
    Ok(())
}

fn check_disk_status_only(cur: &Value, new: &Value) -> Result<(), String> {
    same_except(
        cur,
        new,
        &[
            "size_bytes",
            "phase",
            "conditions",
            "pending_growpart",
            "applied_extend_root_seq",
            "attached_to",
            "create",
            "resize",
            "extend_root",
        ],
    )?;
    // Requests are the API's to make, the node's to complete.
    for f in ["create", "resize", "extend_root"] {
        if !new[f].is_null() && cur[f] != new[f] {
            return Err(format!("a node may only clear {f}"));
        }
    }
    if !new["attached_to"].is_null() && cur["attached_to"] != new["attached_to"] {
        return Err("a node may only release a disk".into());
    }
    Ok(())
}

impl Cluster {
    /// Apply a node's write set after checking it (§8.3). Returns the log index.
    pub fn apply_status_write(&self, node: &str, ws: WriteSet) -> Result<(), String> {
        let wsc = ws.clone();
        self.db
            .write(Origin::Controller, |tx| -> Result<(), StatusWriteError> {
                check_status_write(tx, node, &wsc).map_err(StatusWriteError::Refused)?;
                for op in &wsc.ops {
                    let key = std::str::from_utf8(op.key()).unwrap_or("");
                    let mut t = tx.open_table(op.table().definition())?;
                    match op {
                        Op::Put { value, .. } => t.insert(key, value)?,
                        Op::Delete { .. } => {
                            t.remove(key)?;
                        }
                    }
                }
                Ok(())
            })
            .map_err(|e| match e {
                StatusWriteError::Refused(m) => m,
                StatusWriteError::Store(e) => e,
            })
    }
}

enum StatusWriteError {
    Refused(String),
    Store(String),
}

macro_rules! swe {
    ($($t:ty),*) => {$(impl From<$t> for StatusWriteError { fn from(e: $t) -> Self { StatusWriteError::Store(e.to_string()) } })*};
}
swe!(StoreError, redb::TableError, redb::StorageError);

#[cfg(test)]
mod tests {
    use super::*;

    fn put(table: TableId, key: &str, v: &Value) -> Op {
        Op::Put {
            table,
            key: key.as_bytes().to_vec(),
            value: v.to_string().into_bytes(),
        }
    }

    #[test]
    fn a_vm_placed_on_a_node_brings_the_credential_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("db")).unwrap();
        // Synthetic credential: a placeholder hash, no real secret.
        let cred = serde_json::json!({ "project": "p", "username": "alice", "passwd_hash": "[test-hash]", "ssh_authorized_keys": [] });
        db.write(Origin::Api, |tx| -> Result<(), StoreError> {
            tx.open_table(TableId::Credentials.definition())?
                .insert("p/alice", cred.to_string().as_bytes())?;
            Ok(())
        })
        .unwrap();
        let ws = |ops| WriteSet {
            format: WRITE_SET_FORMAT,
            origin: Origin::Api,
            ops,
        };

        // Written before any VM of n1 uses it: not n1's.
        assert!(
            filter_for(
                &db,
                "n1",
                &ws(vec![put(TableId::Credentials, "p/alice", &cred)])
            )
            .ops
            .is_empty()
        );

        // The VM placed on n1 brings it, ahead of the VM.
        let vm = serde_json::json!({ "meta": { "project": "p" }, "spec": { "config": { "credential": "alice" } }, "status": { "placement": { "node": "n1" } } });
        let f = filter_for(&db, "n1", &ws(vec![put(TableId::Vms, "vm1", &vm)]));
        let tables: Vec<TableId> = f.ops.iter().map(|o| o.table()).collect();
        assert_eq!(tables, [TableId::Credentials, TableId::Vms]);
        assert_eq!(f.ops[0].key(), b"p/alice");
        // Once, though two VMs name it; another node gets neither.
        assert_eq!(
            filter_for(
                &db,
                "n1",
                &ws(vec![
                    put(TableId::Vms, "vm1", &vm),
                    put(TableId::Vms, "vm2", &vm)
                ])
            )
            .ops
            .len(),
            3
        );
        assert!(
            filter_for(&db, "n2", &ws(vec![put(TableId::Vms, "vm1", &vm)]))
                .ops
                .is_empty()
        );
    }

    /// Datastores reach only nodes that know their tables, and a node
    /// writes only its own row (spec/storage.md S2).
    #[test]
    fn datastores_go_to_nodes_that_know_them_and_rows_are_each_nodes_own() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::create(dir.path().join("db")).unwrap();
        let node = |level: u32| serde_json::json!({ "status": { "features": { "feature_level": level } } });
        db.write(Origin::Api, |tx| -> Result<(), StoreError> {
            let mut t = tx.open_table(TableId::Nodes.definition())?;
            t.insert("old", node(1).to_string().as_bytes())?;
            t.insert("new", node(2).to_string().as_bytes())?;
            Ok(())
        })
        .unwrap();
        let ds = serde_json::json!({ "id": "d1" });
        let ws = WriteSet {
            format: WRITE_SET_FORMAT,
            origin: Origin::Api,
            ops: vec![
                put(TableId::Datastores, "d1", &ds),
                put(TableId::DatastoreNodes, "d1/new", &ds),
                put(TableId::Projects, "p", &ds),
            ],
        };
        let tables = |n: &str| -> Vec<TableId> {
            filter_for(&db, n, &ws)
                .ops
                .iter()
                .map(|o| o.table())
                .collect()
        };
        assert_eq!(tables("old"), [TableId::Projects]);
        assert_eq!(
            tables("new"),
            [
                TableId::Datastores,
                TableId::DatastoreNodes,
                TableId::Projects
            ]
        );
        assert_eq!(
            snapshot_for(&db, "old")
                .unwrap()
                .ops
                .iter()
                .filter(|o| SHARED_DATASTORES.contains(&o.table()))
                .count(),
            0
        );

        let row = |key: &str| WriteSet {
            format: WRITE_SET_FORMAT,
            origin: Origin::Controller,
            ops: vec![put(TableId::DatastoreNodes, key, &ds)],
        };
        let check = |n: &str, ws: &WriteSet| {
            db.write(
                Origin::Controller,
                |tx| -> Result<Result<(), String>, StoreError> {
                    Ok(check_status_write(tx, n, ws))
                },
            )
            .unwrap()
        };
        assert!(check("new", &row("d1/new")).is_ok());
        assert!(check("new", &row("d1/old")).is_err());
    }
}
