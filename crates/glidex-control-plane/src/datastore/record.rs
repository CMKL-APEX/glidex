//! The `Datastore` record and each node's view of it (spec/24-storage.md §4.1),
//! in the replicated store.
//!
//! The built-in `local` datastore is not stored: [`list`] and [`find`]
//! answer with it, and nothing may change or delete it (S3).

use super::LOCAL;
use crate::store::{Db, Origin, StoreError, TableId};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The feature level a node needs to keep datastores (spec/14-clustering.md
/// §6.6): the cluster's level must reach it before any is created, and an
/// agent below it is sent none.
pub const DATASTORE_FEATURE_LEVEL: u32 = 2;

/// Where registered datastores are mounted (S11), unless
/// `GLIDEX_DATASTORE_ROOT` says otherwise.
pub const DEFAULT_ROOT: &str = "/srv/glidex";

pub fn root() -> PathBuf {
    std::env::var_os("GLIDEX_DATASTORE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Datastore {
    pub id: String,
    /// Unique in the cluster, `local` included.
    pub name: String,
    pub driver: DriverSpec,
    /// Nodes it is on; empty: every node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<String>,
    /// New disks go here unless the request names a datastore (S13); at
    /// most one datastore has it, and with none `local` is the default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default_for_disks: bool,
    /// Disks may promise more than the free space (thin, like qcow2 files).
    /// Off: a disk is refused when it doesn't fit (spec/24-storage.md §5.2).
    #[serde(default = "yes")]
    pub overcommit: bool,
    pub created_at: u64,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum DriverSpec {
    /// A directory on each node; each node's own (node-local).
    Dir {
        path: String,
        /// Node id → that node's path, overriding `path` (S14).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        node_paths: BTreeMap<String, String>,
    },
    /// One filesystem every node mounts (NFS, CephFS, a vendor's): the same
    /// files from each node (spec/24-storage.md §5). glidex never mounts it.
    SharedDir {
        path: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        node_paths: BTreeMap<String, String>,
    },
    /// LVM-thin on each node: a volume group and thin pool the operator
    /// made, through glidex-stord (spec/24-storage.md §11.1, S19).
    Lvm {
        vg: String,
        thin_pool: String,
        /// Node id → that node's volume group, overriding `vg`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        node_vgs: BTreeMap<String, String>,
    },
    /// A hyperstor cluster's pool (spec/24-storage.md §11.2): volumes served
    /// to VMs over vhost-user-blk by each node's `hyperstor-vhost`.
    Hyperstor {
        /// The cluster API's addresses (`host:port`).
        api: Vec<String>,
        /// The map service's addresses, as the join gave them.
        map: String,
        pool: u32,
        /// The identity glidex enrolled with (S20): its certificate and the
        /// cluster's root, in clear; its key is sealed apart, under
        /// [`hyperstor_secret_owner`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<HyperstorIdentity>,
        /// Unused: a hyperstor datastore is the same from every node.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        node_paths: BTreeMap<String, String>,
    },
}

/// A hyperstor datastore's certificate (S20): public, renewed by the
/// leader before its 30 days run out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HyperstorIdentity {
    /// The chain, leaf then intermediate, PEM.
    pub cert: String,
    /// hyperstor's root, PEM: what its API's certificate is checked against.
    pub root: String,
    /// The identity it names, `hyperstor://<cluster>/mgmt/<name>`.
    pub name: String,
    /// When it expires (Unix seconds).
    pub not_after: u64,
}

/// Where a hyperstor datastore's key is sealed (`SecretsKey::seal_value`).
pub fn hyperstor_secret_owner(datastore: &str) -> String {
    format!("hyperstor:{datastore}")
}

static NO_PATHS: BTreeMap<String, String> = BTreeMap::new();

/// Where a datastore's volumes can be reached from (S4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// Each node has its own; a volume is bound to one node.
    NodeLocal,
    /// Every node that reaches it sees the same files.
    Shared,
}

impl DriverSpec {
    /// The datastore's path, and each node's own (S14). For `lvm`: its
    /// volume group, and each node's own.
    pub fn paths(&self) -> (&String, &BTreeMap<String, String>) {
        match self {
            DriverSpec::Dir { path, node_paths } | DriverSpec::SharedDir { path, node_paths } => {
                (path, node_paths)
            }
            DriverSpec::Lvm { vg, node_vgs, .. } => (vg, node_vgs),
            // The map service's addresses stand for a path: there is none.
            DriverSpec::Hyperstor { map, .. } => (map, &NO_PATHS),
        }
    }

    pub fn node_paths_mut(&mut self) -> &mut BTreeMap<String, String> {
        match self {
            DriverSpec::Dir { node_paths, .. } | DriverSpec::SharedDir { node_paths, .. } => {
                node_paths
            }
            DriverSpec::Lvm { node_vgs, .. } => node_vgs,
            DriverSpec::Hyperstor { node_paths, .. } => node_paths,
        }
    }

    /// Volumes are block devices (raw), not files.
    pub fn is_block(&self) -> bool {
        matches!(self, DriverSpec::Lvm { .. } | DriverSpec::Hyperstor { .. })
    }

    pub fn is_hyperstor(&self) -> bool {
        matches!(self, DriverSpec::Hyperstor { .. })
    }
}

impl Datastore {
    /// The built-in datastore: each node's disk directory.
    pub fn local() -> Self {
        Datastore {
            id: LOCAL.into(),
            name: LOCAL.into(),
            driver: DriverSpec::Dir {
                path: String::new(),
                node_paths: BTreeMap::new(),
            },
            nodes: Vec::new(),
            default_for_disks: false,
            overcommit: true,
            created_at: 0,
        }
    }

    pub fn is_builtin(&self) -> bool {
        self.id == LOCAL
    }

    /// Whether `node` is meant to have it.
    pub fn on_node(&self, node: &str) -> bool {
        self.nodes.is_empty() || self.nodes.iter().any(|n| n == node)
    }

    /// `node`'s path to it: its override, else the datastore's path (for
    /// `lvm`, the node's volume group).
    pub fn path_for(&self, node: &str) -> &str {
        let (path, node_paths) = self.driver.paths();
        node_paths.get(node).unwrap_or(path)
    }

    pub fn topology(&self) -> Topology {
        match self.driver {
            DriverSpec::Dir { .. } | DriverSpec::Lvm { .. } => Topology::NodeLocal,
            DriverSpec::SharedDir { .. } | DriverSpec::Hyperstor { .. } => Topology::Shared,
        }
    }

    pub fn is_shared(&self) -> bool {
        self.topology() == Topology::Shared
    }

    /// Whether linked disks here overlay the datastore's own copy of their
    /// image file (S9). A hyperstor datastore is shared too, but its
    /// driver makes the image's volume itself (§11.2).
    pub fn holds_image_files(&self) -> bool {
        matches!(self.driver, DriverSpec::SharedDir { .. })
    }

    /// `node`'s path to this datastore's copy of image `id` (S9).
    pub fn image_copy_path(&self, node: &str, id: &str) -> PathBuf {
        crate::paths::file_in(
            &Path::new(self.path_for(node)).join("images"),
            &format!("{}.qcow2", crate::images::safe_stem(id)),
        )
    }
}

/// What a node found when it last looked at a datastore (§4.1),
/// `datastore_nodes/<datastore>/<node>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NodeRow {
    pub reachable: bool,
    /// Why not, when it isn't: `OutsideRoot`, `Missing`, `NotADirectory`,
    /// `ReadOnly`; for a shared one also `NotEmpty` (no marker, files in
    /// it) and `ForeignMarker` (another datastore's or cluster's, S5).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    pub path: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub checked_at: u64,
    /// What is risky about how it is mounted here, though it is reachable
    /// (spec/24-storage.md §4.4): NFS mounted `soft`, or with local locks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// `lvm`: the images this node keeps a volume of in it (`gx-i-<id>`,
    /// spec/24-storage.md §11.1), so the datastore isn't deleted from under
    /// them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_volumes: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DatastoreError {
    #[error("datastore not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<StoreError> for DatastoreError {
    fn from(e: StoreError) -> Self {
        DatastoreError::Storage(e.to_string())
    }
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(impl From<$t> for DatastoreError { fn from(e: $t) -> Self { DatastoreError::Storage(e.to_string()) } })*};
}
storage_from!(
    redb::TableError,
    redb::StorageError,
    redb::TransactionError,
    serde_json::Error
);

/// The stored datastores, without `local`.
pub fn stored(db: &Db) -> Result<Vec<Datastore>, DatastoreError> {
    let txn = db.begin_read()?;
    let t = txn.open_table(TableId::Datastores.definition())?;
    let mut out = Vec::new();
    for r in t.iter()? {
        out.push(serde_json::from_slice(r?.1.value())?);
    }
    Ok(out)
}

/// Every datastore, `local` first.
pub fn list(db: &Db) -> Result<Vec<Datastore>, DatastoreError> {
    let mut out = vec![Datastore::local()];
    out.extend(stored(db)?);
    Ok(out)
}

/// A datastore by id or name.
pub fn find(db: &Db, key: &str) -> Result<Option<Datastore>, DatastoreError> {
    Ok(list(db)?.into_iter().find(|d| d.id == key || d.name == key))
}

/// The datastore new disks go to (S13).
pub fn default_for_disks(db: &Db) -> Result<Datastore, DatastoreError> {
    Ok(stored(db)?
        .into_iter()
        .find(|d| d.default_for_disks)
        .unwrap_or_else(Datastore::local))
}

/// Syntax only: absolute, UTF-8, no `..`, under `root` (S11, S14).
pub fn check_path(path: &str, root: &Path) -> Result<(), DatastoreError> {
    let p = Path::new(path);
    let bad = |why: &str| DatastoreError::Invalid(format!("path {path:?}: {why}"));
    if !p.is_absolute() {
        return Err(bad("must be absolute"));
    }
    crate::paths::checked_path(p).map_err(DatastoreError::Invalid)?;
    if path.chars().any(|c| c.is_control() || c == '"') {
        return Err(bad("may not contain control characters or quotes"));
    }
    if p == root || !p.starts_with(root) {
        return Err(bad(&format!(
            "must be a directory under {} (spec/24-storage.md S11)",
            root.display()
        )));
    }
    Ok(())
}

/// Check `ds` against the others (name, the default) and its own fields.
fn validate(ds: &Datastore, others: &[Datastore], root: &Path) -> Result<(), DatastoreError> {
    crate::images::validate_name("datastore", &ds.name)
        .map_err(|e| DatastoreError::Invalid(e.to_string()))?;
    if others.iter().any(|o| o.id != ds.id && o.name == ds.name) {
        return Err(DatastoreError::Conflict(format!(
            "a datastore named {} already exists",
            ds.name
        )));
    }
    if let DriverSpec::Hyperstor { api, map, .. } = &ds.driver {
        if api.is_empty()
            || api
                .iter()
                .any(|a| a.parse::<std::net::SocketAddr>().is_err() && !a.contains(':'))
        {
            return Err(DatastoreError::Invalid(
                "a hyperstor datastore names its cluster API as host:port".into(),
            ));
        }
        if map.is_empty() {
            return Err(DatastoreError::Invalid(
                "a hyperstor datastore needs its map service's addresses".into(),
            ));
        }
        return Ok(());
    }
    if let DriverSpec::Lvm { thin_pool, .. } = &ds.driver {
        let (vg, node_vgs) = ds.driver.paths();
        for (what, n) in std::iter::once(("volume group", vg))
            .chain(node_vgs.values().map(|v| ("volume group", v)))
            .chain(std::iter::once(("thin pool", thin_pool)))
        {
            glidex_stord::names::lvm_name(what, n)
                .map_err(|e| DatastoreError::Invalid(e.to_string()))?;
        }
        return Ok(());
    }
    let (path, node_paths) = ds.driver.paths();
    check_path(path, root)?;
    for p in node_paths.values() {
        check_path(p, root)?;
    }
    Ok(())
}

/// Write `ds` (new or changed). Making it the default for disks takes that
/// from any other, in the same write.
pub fn put(db: &Db, ds: &Datastore, root: &Path) -> Result<(), DatastoreError> {
    db.write(Origin::Api, |tx| -> Result<(), DatastoreError> {
        let mut t = tx.open_table(TableId::Datastores.definition())?;
        let mut others = Vec::new();
        for r in t.iter()? {
            let o: Datastore = serde_json::from_slice(r?.1.value())?;
            others.push(o);
        }
        let mut all = vec![Datastore::local()];
        all.extend(others.iter().cloned());
        validate(ds, &all, root)?;
        if ds.default_for_disks {
            for mut o in others
                .into_iter()
                .filter(|o| o.id != ds.id && o.default_for_disks)
            {
                o.default_for_disks = false;
                t.insert(o.id.as_str(), serde_json::to_vec(&o)?.as_slice())?;
            }
        }
        t.insert(ds.id.as_str(), serde_json::to_vec(ds)?.as_slice())?;
        Ok(())
    })
}

/// Delete `id` and every node's row about it; refused while a disk is on
/// it, checked in the same write so a disk made meanwhile can't be lost,
/// and while a node keeps image volumes in it (an `lvm` datastore's
/// `gx-i-*`, which nothing would remove afterwards). `node_name` and
/// `image_name` only word the refusal.
pub fn delete(
    db: &Db,
    id: &str,
    node_name: &dyn Fn(&str) -> String,
    image_name: &dyn Fn(&str) -> String,
) -> Result<(), DatastoreError> {
    db.write(Origin::Api, |tx| -> Result<(), DatastoreError> {
        let mut on = Vec::new();
        for r in tx.open_table(TableId::Disks.definition())?.iter()? {
            let (_, v) = r?;
            let d: serde_json::Value = serde_json::from_slice(v.value())?;
            if d["datastore"] == id {
                on.push(d["name"].as_str().unwrap_or_default().to_string());
            }
        }
        if !on.is_empty() {
            on.sort();
            return Err(DatastoreError::Conflict(format!(
                "disks are on it: {}; move or delete them first",
                on.join(", ")
            )));
        }
        let mut rows = tx.open_table(TableId::DatastoreNodes.definition())?;
        let prefix = format!("{id}/");
        let mut kept = Vec::new();
        for r in rows.range(prefix.as_str()..format!("{id}~").as_str())? {
            let (k, v) = r?;
            let row: NodeRow = serde_json::from_slice(v.value())?;
            if !row.image_volumes.is_empty() {
                let node = k.value().split_once('/').map(|(_, n)| n).unwrap_or_default();
                let images: Vec<String> = row.image_volumes.iter().map(|i| image_name(i)).collect();
                kept.push(format!("{} on node {}", images.join(", "), node_name(node)));
            }
        }
        if !kept.is_empty() {
            return Err(DatastoreError::Conflict(format!(
                "it still holds volumes of images {}; delete those images first, or remove their gx-i-* volumes from the volume group",
                kept.join("; ")
            )));
        }
        tx.open_table(TableId::Datastores.definition())?
            .remove(id)?;
        let keys: Vec<String> = rows
            .range(prefix.as_str()..format!("{id}~").as_str())?
            .map(|r| r.map(|(k, _)| k.value().to_string()))
            .collect::<Result<_, _>>()?;
        for k in keys {
            rows.remove(k.as_str())?;
        }
        Ok(())
    })
}

pub fn row_key(datastore: &str, node: &str) -> String {
    format!("{datastore}/{node}")
}

/// Every node's row about `datastore`, by node id.
pub fn rows(db: &Db, datastore: &str) -> Result<BTreeMap<String, NodeRow>, DatastoreError> {
    let txn = db.begin_read()?;
    let t = txn.open_table(TableId::DatastoreNodes.definition())?;
    let prefix = format!("{datastore}/");
    let mut out = BTreeMap::new();
    for r in t.range(prefix.as_str()..format!("{datastore}~").as_str())? {
        let (k, v) = r?;
        if let Some((_, node)) = k.value().split_once('/') {
            out.insert(node.to_string(), serde_json::from_slice(v.value())?);
        }
    }
    Ok(out)
}

/// This node's row about `datastore` (a status write, spec/14-clustering.md
/// §8.3); `None` removes it.
pub fn put_row(
    db: &Db,
    datastore: &str,
    node: &str,
    row: Option<&NodeRow>,
) -> Result<(), DatastoreError> {
    let key = row_key(datastore, node);
    db.write(Origin::Controller, |tx| -> Result<(), DatastoreError> {
        let mut t = tx.open_table(TableId::DatastoreNodes.definition())?;
        match row {
            Some(r) => {
                t.insert(key.as_str(), serde_json::to_vec(r)?.as_slice())?;
            }
            None => {
                t.remove(key.as_str())?;
            }
        }
        Ok(())
    })
}
