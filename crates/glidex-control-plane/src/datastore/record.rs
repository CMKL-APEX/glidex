//! The `Datastore` record and each node's view of it (spec/storage.md §4.1),
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

/// The feature level a node needs to keep datastores (spec/clustering.md
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
    /// Off: a disk is refused when it doesn't fit (spec/storage.md §5.2).
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

    /// `node`'s path to it: its override, else the datastore's path.
    pub fn path_for(&self, node: &str) -> &str {
        match &self.driver {
            DriverSpec::Dir { path, node_paths } => node_paths.get(node).unwrap_or(path),
        }
    }
}

/// What a node found when it last looked at a datastore (§4.1),
/// `datastore_nodes/<datastore>/<node>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct NodeRow {
    pub reachable: bool,
    /// Why not, when it isn't: `OutsideRoot`, `Missing`, `NotADirectory`,
    /// `ReadOnly` or `Unsupported`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    pub path: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub checked_at: u64,
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
            "must be a directory under {} (spec/storage.md S11)",
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
    match &ds.driver {
        DriverSpec::Dir { path, node_paths } => {
            check_path(path, root)?;
            for p in node_paths.values() {
                check_path(p, root)?;
            }
        }
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
/// it, checked in the same write so a disk made meanwhile can't be lost.
pub fn delete(db: &Db, id: &str) -> Result<(), DatastoreError> {
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
        tx.open_table(TableId::Datastores.definition())?
            .remove(id)?;
        let mut rows = tx.open_table(TableId::DatastoreNodes.definition())?;
        let prefix = format!("{id}/");
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

/// This node's row about `datastore` (a status write, spec/clustering.md
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
