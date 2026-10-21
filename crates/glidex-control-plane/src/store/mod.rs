//! The VM table as `{meta, spec, status}` envelopes, the schema version and
//! its migration, and the per-object event rings (spec/reconciliation.md
//! §6). Disks, images and networks keep their own tables (`images`,
//! `network`); this module owns `vms` and `events`.

use crate::images::{self, Disk, ImageError};
use crate::models::{
    HostBootPolicy, PowerState, RestartPolicy, Vm, VmConfig, VmPhase, VmSpec, VmStatus,
};
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;

mod db;
pub use db::{
    Applied, Bell, Consistency, Db, Def, Forwarder, Glidex, LAST_APPLIED, LAST_MEMBERSHIP,
    MAX_WRITE_SET_BYTES, MIRROR_REVISION, Mirror, Op, Origin, RecTable, Replicator, SnapshotView,
    StoreError, Submitted, TableId, Tx, WRITE_SET_FORMAT, WriteSet, new_bell, ring,
};

const VMS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("vms");
const EVENTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("events");
/// Shared with `tenancy` (same name and types).
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// `meta.schema_version`: absent = 1 (flat VM records), 2 = envelopes,
/// 3 = nodes: placement, disk and network nodes, `Cluster` entities
/// (spec/clustering.md §17 C1), 4 = networks keyed by id
/// (spec/tenancy.md D24).
pub const SCHEMA_VERSION: u32 = 4;
const SCHEMA_ENVELOPES: u32 = 2;
const SCHEMA_NODES: u32 = 3;
const SCHEMA_KEY: &str = "schema_version";

/// Events kept per object.
pub const EVENTS_PER_OBJECT: usize = 50;

#[derive(Error, Debug)]
pub enum PersistenceError {
    #[error("Database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("Transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("Table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("Storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("Commit error: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("VM not found: {0}")]
    VmNotFound(String),
    #[error(
        "database schema {0} is newer than this glidex (schema {SCHEMA_VERSION}); upgrade glidex"
    )]
    NewerSchema(u32),
    #[error("{0}")]
    Disk(#[from] ImageError),
    #[error("{0}")]
    Ipam(String),
}

/// `meta` of every resource (§6.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub name: String,
    pub project: String,
    pub created_at: u64,
    pub generation: u64,
    pub resource_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_requested_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finalizers: Vec<String>,
}

/// How a [`Vm`] is stored: the envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmRecord {
    pub meta: Meta,
    pub spec: VmSpec,
    #[serde(default)]
    pub status: VmStatus,
}

impl From<Vm> for VmRecord {
    fn from(vm: Vm) -> Self {
        VmRecord {
            meta: Meta {
                id: vm.id,
                name: vm.name,
                project: vm.project,
                created_at: vm.created_at,
                generation: vm.generation,
                resource_version: vm.resource_version,
                deletion_requested_at: vm.deletion_requested_at,
                finalizers: vm.finalizers,
            },
            spec: vm.spec,
            status: vm.status,
        }
    }
}

impl From<VmRecord> for Vm {
    fn from(r: VmRecord) -> Self {
        Vm {
            id: r.meta.id,
            name: r.meta.name,
            project: r.meta.project,
            created_at: r.meta.created_at,
            generation: r.meta.generation,
            resource_version: r.meta.resource_version,
            deletion_requested_at: r.meta.deletion_requested_at,
            finalizers: r.meta.finalizers,
            spec: r.spec,
            status: r.status,
        }
    }
}

/// A schema-1 record: the flat `Vm` of earlier releases.
#[derive(Deserialize)]
struct LegacyVm {
    id: String,
    name: String,
    #[serde(default)]
    project: String,
    state: String,
    config: VmConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Normal,
    Warning,
}

/// One entry of an object's event ring (§6.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub at: u64,
    /// `api:<principal>`, `controller`, `guest`, `systemd` or `host`.
    pub actor: String,
    pub kind: EventKind,
    pub reason: String,
    pub message: String,
}

impl Event {
    pub fn new(
        actor: impl Into<String>,
        kind: EventKind,
        reason: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Event {
            at: crate::tenancy::now(),
            actor: actor.into(),
            kind,
            reason: reason.into(),
            message: message.into(),
        }
    }
}

/// Event ring key: `<kind>/<id>`.
pub fn event_key(kind: &str, id: &str) -> String {
    format!("{}/{}", kind, id)
}

/// One atomic write across the `vms`, `disks` and `events` tables, so a
/// VM and the disks it claims never disagree (spec images.md §3).
#[derive(Default)]
pub struct Commit<'a> {
    pub put_vm: Option<&'a Vm>,
    pub delete_vm: Option<&'a str>,
    pub put_disks: Vec<&'a Disk>,
    pub delete_disks: Vec<&'a str>,
    pub events: Vec<(String, Event)>,
    /// Addresses to reserve on cluster networks, in the same write:
    /// `(network, mac, vm id, nic)` (spec/clustering.md §11.4).
    pub reserve_ips: Vec<(String, String, String, u8)>,
}

pub struct VmStore {
    db: Arc<Db>,
}

impl VmStore {
    /// Open or create the database. Refuses a database written by a newer
    /// schema (§6.6).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Db::create(path.as_ref())?;
        // The database also holds credential hashes (credentials.rs), so
        // keep it readable by the control-plane user only.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path.as_ref(), std::fs::Permissions::from_mode(0o600))?;
        }
        let store = Self { db: Arc::new(db) };
        if let Some(v) = store.schema_version()?
            && v > SCHEMA_VERSION
        {
            return Err(PersistenceError::NewerSchema(v));
        }
        Ok(store)
    }

    pub fn database(&self) -> Arc<Db> {
        self.db.clone()
    }

    pub fn schema_version(&self) -> Result<Option<u32>, PersistenceError> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(META_TABLE)?;
        Ok(t.get(SCHEMA_KEY)?.and_then(|v| {
            std::str::from_utf8(v.value())
                .ok()
                .and_then(|s| s.parse().ok())
        }))
    }

    /// Rewrite schema-1 VM records as envelopes, in one transaction
    /// (§6.6). Every migrated VM is stopped: the release that wrote them
    /// killed its guests when it stopped. Records this build can't read
    /// (e.g. a removed hypervisor) are left untouched. Returns how many
    /// records were rewritten.
    pub fn migrate(&self, on_host_boot: HostBootPolicy) -> Result<usize, PersistenceError> {
        let rewritten = self.migrate_to_envelopes(on_host_boot)?;
        self.migrate_to_nodes()?;
        self.migrate_network_ids()?;
        Ok(rewritten)
    }

    fn migrate_to_envelopes(
        &self,
        on_host_boot: HostBootPolicy,
    ) -> Result<usize, PersistenceError> {
        if self.schema_version()?.is_some() {
            return Ok(0);
        }
        let txn = self.db.begin(crate::store::Origin::Migration)?;
        let count;
        {
            let mut table = txn.open_table(VMS_TABLE)?;
            let mut rewritten = Vec::new();
            for entry in table.iter()? {
                let (key, value) = entry?;
                let bytes = value.value();
                if serde_json::from_slice::<VmRecord>(bytes).is_ok() {
                    continue;
                }
                let legacy: LegacyVm = match serde_json::from_slice(bytes) {
                    Ok(l) => l,
                    Err(e) => {
                        tracing::warn!(
                            vm_id = key.value(),
                            "not migrating unreadable VM record: {}",
                            e
                        );
                        continue;
                    }
                };
                let mut vm = Vm::new(legacy.name, legacy.config);
                vm.id = legacy.id;
                vm.project = legacy.project;
                vm.spec.power = PowerState::Stopped;
                vm.spec.restart_policy = RestartPolicy::OnFailure;
                vm.spec.on_host_boot = on_host_boot;
                vm.status.phase = VmPhase::Stopped;
                vm.status.never_started = legacy.state == "created";
                rewritten.push((key.value().to_string(), serde_json::to_vec(&vm)?));
            }
            for (key, bytes) in &rewritten {
                table.insert(key.as_str(), bytes.as_slice())?;
            }
            count = rewritten.len();
            let mut meta = txn.open_table(META_TABLE)?;
            meta.insert(SCHEMA_KEY, SCHEMA_ENVELOPES.to_string().as_bytes())?;
        }
        txn.commit()?;
        Ok(count)
    }

    /// Schema 2 → 3, in one write (spec/clustering.md §6.6): every VM, disk
    /// and network belongs to the implicit node `local`; role links on the
    /// host move to the cluster; the `nodes` table gets its first row.
    fn migrate_to_nodes(&self) -> Result<(), PersistenceError> {
        if self.schema_version()?.is_some_and(|v| v >= SCHEMA_NODES) {
            return Ok(());
        }
        let local = crate::node::LOCAL_NODE;
        let now = crate::tenancy::now();
        let txn = self.db.begin(Origin::Migration)?;
        // Records this build can't parse are left as they are.
        let rewrite = |table: TableId,
                       f: &dyn Fn(&mut serde_json::Value) -> bool|
         -> Result<(), PersistenceError> {
            let mut t = txn.open_table(table.definition())?;
            let mut changed = Vec::new();
            for r in t.iter()? {
                let (k, v) = r?;
                if let Ok(mut j) = serde_json::from_slice::<serde_json::Value>(v.value())
                    && f(&mut j)
                {
                    changed.push((k.value().to_string(), serde_json::to_vec(&j)?));
                }
            }
            for (k, v) in changed {
                t.insert(k.as_str(), v.as_slice())?;
            }
            Ok(())
        };
        rewrite(TableId::Vms, &|j| {
            let Some(status) = j.get_mut("status").and_then(|s| s.as_object_mut()) else {
                return false;
            };
            if status.contains_key("placement") {
                return false;
            }
            status.insert(
                "placement".into(),
                serde_json::json!({ "node": local, "at": now }),
            );
            true
        })?;
        rewrite(TableId::Disks, &|j| {
            let Some(o) = j.as_object_mut() else {
                return false;
            };
            if o.contains_key("node") {
                return false;
            }
            o.insert("node".into(), local.into());
            true
        })?;
        rewrite(TableId::Networks, &|j| {
            let Some(o) = j.as_object_mut() else {
                return false;
            };
            if o.contains_key("scope") {
                return false;
            }
            o.insert("scope".into(), "node".into());
            o.insert("node".into(), local.into());
            true
        })?;
        // `Host::"local"` was the root of everything; it is now the cluster.
        rewrite(TableId::PolicyLinks, &|j| match j.get_mut("resource") {
            Some(r) if r.get("type").and_then(|t| t.as_str()) == Some("Host") => {
                r["type"] = "Cluster".into();
                true
            }
            _ => false,
        })?;
        {
            let mut nodes = txn.open_table(TableId::Nodes.definition())?;
            if nodes.get(local)?.is_none() {
                let probe = crate::node::SelfProbe::detect(&crate::node::Resources::default());
                let mut n = crate::node::Node::new(
                    local,
                    crate::node::NodeSpec {
                        name: probe.name.clone(),
                        role: crate::node::NodeRole::Server,
                        unschedulable: false,
                        labels: Default::default(),
                        br_int_datapath: Default::default(),
                        bridge_mappings: Default::default(),
                        drop_spare_files_before: None,
                        cordoned_devices: Default::default(),
                    },
                );
                n.status.capacity = probe.capacity;
                n.status.allocatable = probe.allocatable;
                n.status.features = probe.features;
                n.status.pci_devices = probe.pci_devices;
                n.status.pci_groups = probe.pci_groups;
                n.status.topology = probe.topology;
                nodes.insert(local, serde_json::to_vec(&n)?.as_slice())?;
            }
            let mut meta = txn.open_table(META_TABLE)?;
            meta.insert(SCHEMA_KEY, SCHEMA_NODES.to_string().as_bytes())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Schema 3 → 4, in one write (spec/tenancy.md D24): networks get an id
    /// (derived from the name, so every server picks the same one) and are
    /// keyed by it, and so are the VM NICs and IPAM rows that named them.
    /// Their OVN objects keep the name (`ovn_key`), so nothing on the wire
    /// changes.
    fn migrate_network_ids(&self) -> Result<(), PersistenceError> {
        if self.schema_version()?.is_some_and(|v| v >= SCHEMA_VERSION) {
            return Ok(());
        }
        let txn = self.db.begin(Origin::Migration)?;
        let mut ids: std::collections::HashMap<String, String> = Default::default();
        {
            let mut t = txn.open_table(TableId::Networks.definition())?;
            let mut moved = Vec::new();
            for r in t.iter()? {
                let (k, v) = r?;
                let Ok(mut j) = serde_json::from_slice::<serde_json::Value>(v.value()) else {
                    continue;
                };
                let Some(o) = j.as_object_mut() else { continue };
                if o.contains_key("id") {
                    continue;
                }
                let name = k.value().to_string();
                let id = crate::network::legacy_id(&name);
                o.insert("id".into(), id.clone().into());
                o.insert("ovn_key".into(), name.clone().into());
                moved.push((name.clone(), id.clone(), serde_json::to_vec(&j)?));
                ids.insert(name, id);
            }
            for (name, id, v) in moved {
                t.remove(name.as_str())?;
                t.insert(id.as_str(), v.as_slice())?;
            }
        }
        let map = |n: &mut serde_json::Value| -> bool {
            match n.as_str().and_then(|s| ids.get(s)) {
                Some(id) => {
                    *n = id.clone().into();
                    true
                }
                None => false,
            }
        };
        {
            let mut t = txn.open_table(VMS_TABLE)?;
            let mut changed = Vec::new();
            for r in t.iter()? {
                let (k, v) = r?;
                let Ok(mut j) = serde_json::from_slice::<serde_json::Value>(v.value()) else {
                    continue;
                };
                let mut hit = false;
                for path in ["/spec/config/networks", "/status/nics"] {
                    if let Some(a) = j.pointer_mut(path).and_then(|a| a.as_array_mut()) {
                        for nic in a {
                            if let Some(n) = nic.get_mut("network") {
                                hit |= map(n);
                            }
                        }
                    }
                }
                if hit {
                    changed.push((k.value().to_string(), serde_json::to_vec(&j)?));
                }
            }
            for (k, v) in changed {
                t.insert(k.as_str(), v.as_slice())?;
            }
        }
        // IPAM: subnets are keyed `<network>`, reservations `<network>/<mac>`.
        for table in [TableId::IpamSubnets, TableId::IpamReservations] {
            let mut t = txn.open_table(table.definition())?;
            let mut moved = Vec::new();
            for r in t.iter()? {
                let (k, v) = r?;
                let key = k.value().to_string();
                let (net, rest) = match key.split_once('/') {
                    Some((n, m)) => (n.to_string(), Some(m.to_string())),
                    None => (key.clone(), None),
                };
                let Some(id) = ids.get(&net) else { continue };
                let Ok(mut j) = serde_json::from_slice::<serde_json::Value>(v.value()) else {
                    continue;
                };
                if let Some(n) = j.get_mut("network") {
                    map(n);
                }
                let new = match rest {
                    Some(m) => format!("{id}/{m}"),
                    None => id.clone(),
                };
                moved.push((key, new, serde_json::to_vec(&j)?));
            }
            for (old, new, v) in moved {
                t.remove(old.as_str())?;
                t.insert(new.as_str(), v.as_slice())?;
            }
        }
        {
            let mut meta = txn.open_table(META_TABLE)?;
            meta.insert(SCHEMA_KEY, SCHEMA_VERSION.to_string().as_bytes())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Load all VMs. Records this build can't decode are skipped with a
    /// warning and left in the database untouched.
    pub fn load_all(&self) -> Result<Vec<Vm>, PersistenceError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(VMS_TABLE)?;
        let mut vms = Vec::new();
        for result in table.iter()? {
            let (key, value) = result?;
            match serde_json::from_slice::<Vm>(value.value()) {
                Ok(vm) => vms.push(vm),
                Err(e) => {
                    tracing::warn!(vm_id = key.value(), "Skipping unreadable VM record: {}", e)
                }
            }
        }
        Ok(vms)
    }

    pub fn save(&self, vm: &Vm) -> Result<(), PersistenceError> {
        self.commit(Commit {
            put_vm: Some(vm),
            ..Default::default()
        })
    }

    /// Apply a `Commit` in a single write transaction.
    pub fn commit(&self, c: Commit<'_>) -> Result<(), PersistenceError> {
        let write_txn = self.db.begin(crate::store::Origin::Controller)?;
        {
            let mut table = write_txn.open_table(VMS_TABLE)?;
            if let Some(vm) = c.put_vm {
                let serialized = serde_json::to_vec(vm)?;
                table.insert(vm.id.as_str(), serialized.as_slice())?;
            }
            if let Some(id) = c.delete_vm {
                table.remove(id)?;
                let mut events = write_txn.open_table(EVENTS_TABLE)?;
                events.remove(event_key("vm", id).as_str())?;
            }
        }
        for d in c.put_disks {
            images::write_disk(&write_txn, d)?;
        }
        for id in c.delete_disks {
            images::delete_disk_record(&write_txn, id)?;
        }
        for (network, mac, vm_id, nic) in &c.reserve_ips {
            crate::ipam::reserve(&write_txn, network, mac, vm_id, *nic)
                .map_err(|e| PersistenceError::Ipam(e.to_string()))?;
        }
        for (key, event) in c.events {
            if c.delete_vm.is_some_and(|id| key == event_key("vm", id)) {
                continue;
            }
            push_event(&write_txn, &key, event)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// Append to an object's event ring on its own.
    pub fn push_event(&self, key: &str, event: Event) -> Result<(), PersistenceError> {
        let txn = self.db.begin(crate::store::Origin::Controller)?;
        push_event(&txn, key, event)?;
        txn.commit()?;
        Ok(())
    }

    /// An object's events, oldest first.
    pub fn events(&self, key: &str) -> Result<Vec<Event>, PersistenceError> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(EVENTS_TABLE)?;
        match t.get(key)? {
            Some(v) => Ok(serde_json::from_slice(v.value())?),
            None => Ok(Vec::new()),
        }
    }

    /// Drop an object's events (when it is deleted).
    pub fn delete_events(&self, key: &str) -> Result<(), PersistenceError> {
        let txn = self.db.begin(crate::store::Origin::Controller)?;
        {
            let mut t = txn.open_table(EVENTS_TABLE)?;
            t.remove(key)?;
        }
        txn.commit()?;
        Ok(())
    }
}

/// The `audit` table (spec/security.md §10), shared with `auth::store`.
const AUDIT_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("audit");

/// An audit line for a write the control plane made on its own (the VM
/// controller's `spec.power = Stopped`, spec/reconciliation.md §6.2).
pub fn audit_system(
    db: &Db,
    actor: &str,
    action: &str,
    project: &str,
    target: &str,
    details: serde_json::Value,
) {
    let time = crate::auth::store::now_millis();
    let entry = crate::auth::store::AuditEntry {
        time,
        request_id: String::new(),
        principal: serde_json::json!({ "system": actor }),
        source: "-".into(),
        action: action.into(),
        project: Some(project.into()),
        target: Some(target.into()),
        result: "ok".into(),
        error_code: None,
        policies: Vec::new(),
        details,
    };
    tracing::info!(target: "glidex_audit", "{}", serde_json::to_string(&entry).unwrap_or_default());
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // `s`: never collides with auth::store's all-digit sequence keys.
    let key = format!("{:016}-s{:07}", time, seq % 10_000_000);
    let write = || -> Result<(), PersistenceError> {
        let txn = db.begin(crate::store::Origin::Controller)?;
        {
            let mut t = txn.open_table(AUDIT_TABLE)?;
            t.insert(key.as_str(), serde_json::to_vec(&entry)?.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::error!("audit write failed: {}", e);
    }
}

/// Append `event` to the ring at `key` inside `txn`.
pub fn push_event(txn: &Tx<'_>, key: &str, event: Event) -> Result<(), PersistenceError> {
    let mut t = txn.open_table(EVENTS_TABLE)?;
    let mut ring: Vec<Event> = match t.get(key)? {
        Some(v) => serde_json::from_slice(v.value()).unwrap_or_default(),
        None => Vec::new(),
    };
    ring.push(event);
    if ring.len() > EVENTS_PER_OBJECT {
        let extra = ring.len() - EVENTS_PER_OBJECT;
        ring.drain(..extra);
    }
    let bytes = serde_json::to_vec(&ring)?;
    t.insert(key, bytes.as_slice())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(id: &str, state: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "name": format!("vm-{id}"), "project": "p1", "state": state,
            "config": { "vcpu_count": 1, "mem_size_mib": 512, "rootfs_path": "/r", "kernel_args": "" },
            "socket_path": "/x", "console_socket_path": "/y", "log_path": "/z", "hypervisor": "cloudhypervisor"
        })
    }

    fn put_raw(store: &VmStore, id: &str, v: &serde_json::Value) {
        let txn = store.db.begin(crate::store::Origin::Controller).unwrap();
        {
            let mut t = txn.open_table(VMS_TABLE).unwrap();
            t.insert(id, serde_json::to_vec(v).unwrap().as_slice())
                .unwrap();
        }
        txn.commit().unwrap();
    }

    #[test]
    fn schema_one_records_migrate_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::open(dir.path().join("db")).unwrap();
        put_raw(&store, "a", &legacy("a", "running"));
        put_raw(&store, "b", &legacy("b", "created"));
        let mut fc = legacy("c", "stopped");
        fc["config"]["hypervisor"] = "firecracker".into();
        put_raw(&store, "c", &fc);

        assert_eq!(store.migrate(HostBootPolicy::Stop).unwrap(), 2);
        assert_eq!(store.schema_version().unwrap(), Some(SCHEMA_VERSION));
        let mut vms = store.load_all().unwrap();
        vms.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(
            vms.len(),
            2,
            "the firecracker record is skipped, not dropped"
        );
        assert_eq!(
            (
                vms[0].spec.power,
                vms[0].status.phase,
                vms[0].status.never_started
            ),
            (PowerState::Stopped, VmPhase::Stopped, false)
        );
        assert!(vms[1].status.never_started);
        assert_eq!(vms[0].spec.on_host_boot, HostBootPolicy::Stop);
        assert_eq!(vms[0].project, "p1");
        // Idempotent.
        assert_eq!(store.migrate(HostBootPolicy::Resume).unwrap(), 0);
    }

    #[test]
    fn newer_schemas_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let store = VmStore::open(&path).unwrap();
            let txn = store.db.begin(crate::store::Origin::Controller).unwrap();
            {
                let mut t = txn.open_table(META_TABLE).unwrap();
                t.insert(SCHEMA_KEY, b"5".as_slice()).unwrap();
            }
            txn.commit().unwrap();
        }
        assert!(matches!(
            VmStore::open(&path),
            Err(PersistenceError::NewerSchema(5))
        ));
    }

    /// C1 acceptance: a schema-2 database (before nodes) migrates in one
    /// write, and a second run changes nothing.
    #[test]
    fn schema_two_migrates_to_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::open(dir.path().join("db")).unwrap();
        let db = store.database();
        let vm = {
            let config: VmConfig = serde_json::from_value(serde_json::json!({
                "vcpu_count": 1, "mem_size_mib": 512, "rootfs_path": "/r", "kernel_args": ""
            }))
            .unwrap();
            Vm::new("old".into(), config)
        };
        db.write(Origin::System, |tx| -> Result<(), PersistenceError> {
            let mut vms = tx.open_table(TableId::Vms.definition())?;
            vms.insert(&vm.id, serde_json::to_vec(&vm)?.as_slice())?;
            let mut disks = tx.open_table(TableId::Disks.definition())?;
            disks.insert("d1", br#"{"id":"d1","name":"n","format":"qcow2","size_bytes":1048576,"origin":"blank","created_at":1}"#.as_slice())?;
            let mut nets = tx.open_table(TableId::Networks.definition())?;
            nets.insert("n1", br#"{"name":"n1","bridge":"b","mode":"nat","port_type":"tap","created_at":1}"#.as_slice())?;
            let mut links = tx.open_table(TableId::PolicyLinks.definition())?;
            links.insert(
                "link.a",
                br#"{"id":"link.a","template":"role.system-admin","principal":{"type":"User","id":"u"},"resource":{"type":"Host"},"created_by":"t","created_at":1}"#.as_slice(),
            )?;
            links.insert(
                "link.b",
                br#"{"id":"link.b","template":"role.viewer","principal":{"type":"User","id":"u"},"resource":{"type":"Project","id":"p"},"created_by":"t","created_at":1}"#.as_slice(),
            )?;
            let mut meta = tx.open_table(TableId::Meta.definition())?;
            meta.insert(SCHEMA_KEY, b"2".as_slice())?;
            Ok(())
        })
        .unwrap();

        assert_eq!(store.migrate(HostBootPolicy::Stop).unwrap(), 0);
        assert_eq!(store.schema_version().unwrap(), Some(SCHEMA_VERSION));
        let vms = store.load_all().unwrap();
        assert_eq!(vms[0].status.placement.as_ref().unwrap().node, "local");
        let txn = db.begin_read().unwrap();
        let get = |t: TableId, k: &str| -> serde_json::Value {
            let t = txn.open_table(t.definition()).unwrap();
            serde_json::from_slice(t.get(k).unwrap().unwrap().value()).unwrap()
        };
        assert_eq!(get(TableId::Disks, "d1")["node"], "local");
        let net = get(TableId::Networks, &crate::network::legacy_id("n1"));
        assert_eq!(
            (net["scope"].as_str(), net["node"].as_str()),
            (Some("node"), Some("local"))
        );
        assert_eq!(
            get(TableId::PolicyLinks, "link.a")["resource"]["type"],
            "Cluster"
        );
        assert_eq!(
            get(TableId::PolicyLinks, "link.b")["resource"]["type"],
            "Project"
        );
        let node: crate::node::Node = serde_json::from_value(get(TableId::Nodes, "local")).unwrap();
        assert_eq!(node.status.phase, crate::node::NodePhase::Active);
        // The typed readers still load what the migration wrote.
        assert_eq!(
            crate::node::NodeStore::new(db.clone())
                .list()
                .unwrap()
                .len(),
            1
        );
        drop(txn);

        // Idempotent: nothing is written the second time.
        let rev = db.revision();
        store.migrate(HostBootPolicy::Stop).unwrap();
        assert_eq!(db.revision(), rev);
    }

    #[test]
    fn envelopes_round_trip_and_events_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::open(dir.path().join("db")).unwrap();
        store.migrate(HostBootPolicy::Resume).unwrap();
        let config: VmConfig = serde_json::from_value(serde_json::json!({
            "vcpu_count": 1, "mem_size_mib": 512, "rootfs_path": "/r", "kernel_args": ""
        }))
        .unwrap();
        let vm = Vm::new("x".into(), config);
        store.save(&vm).unwrap();
        let raw = serde_json::to_value(&vm).unwrap();
        assert_eq!(raw["meta"]["generation"], 1);
        assert_eq!(raw["spec"]["power"], "stopped");
        assert_eq!(store.load_all().unwrap(), vec![vm.clone()]);

        let key = event_key("vm", &vm.id);
        for i in 0..(EVENTS_PER_OBJECT + 5) {
            store
                .push_event(
                    &key,
                    Event::new("controller", EventKind::Normal, "Tick", i.to_string()),
                )
                .unwrap();
        }
        let ev = store.events(&key).unwrap();
        assert_eq!(ev.len(), EVENTS_PER_OBJECT);
        assert_eq!(ev[0].message, "5");
        store
            .commit(Commit {
                delete_vm: Some(&vm.id),
                ..Default::default()
            })
            .unwrap();
        assert!(store.events(&key).unwrap().is_empty());
    }

    /// D24: a schema-3 database's networks, the VM NICs on them and their
    /// IPAM rows move to the network's id; its OVN objects keep the name.
    #[test]
    fn schema_three_keys_networks_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = VmStore::open(dir.path().join("db")).unwrap();
        let db = store.database();
        let mut vm = {
            let config: VmConfig = serde_json::from_value(serde_json::json!({
                "vcpu_count": 1, "mem_size_mib": 512, "rootfs_path": "/r", "kernel_args": "",
                "networks": [{"network": "lab"}, {"network": "gone"}]
            }))
            .unwrap();
            Vm::new("old".into(), config)
        };
        vm.status.nics.push(crate::models::NicStatus {
            network: "lab".into(),
            nic_index: 0,
            mac: "02:00:00:00:00:01".into(),
            port: None,
            ipv4: None,
            port_ok: false,
        });
        db.write(Origin::System, |tx| -> Result<(), PersistenceError> {
            let mut vms = tx.open_table(TableId::Vms.definition())?;
            vms.insert(&vm.id, serde_json::to_vec(&vm)?.as_slice())?;
            let mut nets = tx.open_table(TableId::Networks.definition())?;
            nets.insert("lab", br#"{"name":"lab","scope":"cluster","bridge":"br-int","mode":"nat","port_type":"tap","created_at":1}"#.as_slice())?;
            let mut subnets = tx.open_table(TableId::IpamSubnets.definition())?;
            subnets.insert("lab", br#"{"network":"lab","cidr":"10.89.0.0/24","gateway":"10.89.0.1","pool":["10.89.0.2","10.89.0.254"]}"#.as_slice())?;
            let mut res = tx.open_table(TableId::IpamReservations.definition())?;
            res.insert("lab/02:00:00:00:00:01", br#"{"network":"lab","mac":"02:00:00:00:00:01","ip":"10.89.0.2","vm_id":"v","nic":0}"#.as_slice())?;
            let mut meta = tx.open_table(TableId::Meta.definition())?;
            meta.insert(SCHEMA_KEY, b"3".as_slice())?;
            Ok(())
        })
        .unwrap();

        store.migrate(HostBootPolicy::Stop).unwrap();
        assert_eq!(store.schema_version().unwrap(), Some(4));
        let id = crate::network::legacy_id("lab");
        let txn = db.begin_read().unwrap();
        let get = |t: TableId, k: &str| -> Option<serde_json::Value> {
            let t = txn.open_table(t.definition()).unwrap();
            t.get(k)
                .unwrap()
                .map(|v| serde_json::from_slice(v.value()).unwrap())
        };
        let net = get(TableId::Networks, &id).unwrap();
        assert_eq!(
            (net["id"].as_str(), net["ovn_key"].as_str()),
            (Some(id.as_str()), Some("lab"))
        );
        assert!(get(TableId::Networks, "lab").is_none());
        let n: crate::network::Network = serde_json::from_value(net).unwrap();
        assert_eq!(n.ovn_key(), "lab");
        assert_eq!(
            get(TableId::IpamSubnets, &id).unwrap()["network"],
            id.as_str()
        );
        assert!(get(TableId::IpamSubnets, "lab").is_none());
        let r = get(
            TableId::IpamReservations,
            &format!("{id}/02:00:00:00:00:01"),
        )
        .unwrap();
        assert_eq!(r["network"], id.as_str());
        drop(txn);
        let vms = store.load_all().unwrap();
        let nets: Vec<&str> = vms[0]
            .config()
            .networks
            .iter()
            .map(|a| a.network.as_str())
            .collect();
        // A NIC on a network that no longer exists keeps what it had.
        assert_eq!(nets, vec![id.as_str(), "gone"]);
        assert_eq!(vms[0].status.nics[0].network, id);
        // Idempotent.
        store.migrate(HostBootPolicy::Stop).unwrap();
        assert_eq!(
            store.load_all().unwrap()[0].config().networks[0].network,
            id
        );
    }
}
