//! The datastore API's work on the manager (spec/storage.md §13).

use super::record::{self, Datastore, DatastoreError, DriverSpec, NodeRow};
use crate::state::{VmManager, VmManagerError};
use std::collections::BTreeMap;

/// A datastore with what each node finds at it and how many disks it holds.
#[derive(Debug, Clone)]
pub struct DatastoreView {
    pub datastore: Datastore,
    pub rows: BTreeMap<String, NodeRow>,
    pub disks: u32,
    /// Where new disks go: it has `default_for_disks`, or it is `local` and
    /// no other has (S13).
    pub is_default: bool,
    /// Node id → name, for the nodes in `rows`.
    pub node_names: BTreeMap<String, String>,
}

/// `POST /datastores`.
#[derive(Debug, Clone, Default)]
pub struct NewDatastore {
    pub name: String,
    pub path: String,
    /// Node id or name → its path.
    pub node_paths: BTreeMap<String, String>,
    /// Node ids or names; empty: every node.
    pub nodes: Vec<String>,
    pub default_for_disks: bool,
    pub overcommit: Option<bool>,
}

/// `PATCH /datastores/{id}`: what is `Some` changes.
#[derive(Debug, Clone, Default)]
pub struct DatastoreChanges {
    pub node_paths: Option<BTreeMap<String, String>>,
    pub nodes: Option<Vec<String>>,
    pub default_for_disks: Option<bool>,
    pub overcommit: Option<bool>,
}

impl VmManager {
    fn datastore_view(&self, ds: Datastore) -> DatastoreView {
        let db = self.database();
        let rows = record::rows(&db, &ds.id).unwrap_or_default();
        let disks = self
            .images
            .list_disks()
            .iter()
            .filter(|d| d.datastore == ds.id)
            .count() as u32;
        let is_default = ds.default_for_disks
            || (ds.is_builtin() && record::default_for_disks(&db).is_ok_and(|d| d.is_builtin()));
        let node_names = self
            .nodes()
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| rows.contains_key(&n.meta.id))
            .map(|n| (n.meta.id, n.spec.name))
            .collect();
        DatastoreView {
            datastore: ds,
            rows,
            disks,
            is_default,
            node_names,
        }
    }

    pub fn list_datastores(&self) -> Result<Vec<DatastoreView>, VmManagerError> {
        Ok(record::list(&self.database())?
            .into_iter()
            .map(|d| self.datastore_view(d))
            .collect())
    }

    pub fn get_datastore(&self, key: &str) -> Result<DatastoreView, VmManagerError> {
        let ds = record::find(&self.database(), key)?
            .ok_or_else(|| DatastoreError::NotFound(key.to_string()))?;
        Ok(self.datastore_view(ds))
    }

    /// Node ids or names → ids, each a live node.
    fn node_ids(&self, keys: &[String]) -> Result<Vec<String>, VmManagerError> {
        keys.iter().map(|k| self.resolve_node(k)).collect()
    }

    fn node_path_ids(
        &self,
        paths: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, VmManagerError> {
        paths
            .iter()
            .map(|(k, p)| Ok((self.resolve_node(k)?, p.clone())))
            .collect()
    }

    fn require_datastores(&self) -> Result<(), VmManagerError> {
        if self.datastores_supported() {
            return Ok(());
        }
        Err(DatastoreError::Conflict(format!(
            "every server must run a release with datastores (feature level {}) first",
            record::DATASTORE_FEATURE_LEVEL
        ))
        .into())
    }

    /// Look at datastores now (a cluster also does so on every change it
    /// applies, `cache_sync`).
    fn probe_soon(&self) {
        let m = self.arc();
        tokio::task::spawn_blocking(move || m.probe_datastores());
    }

    pub async fn create_datastore(
        &self,
        req: NewDatastore,
    ) -> Result<DatastoreView, VmManagerError> {
        self.require_datastores()?;
        let ds = Datastore {
            id: uuid::Uuid::new_v4().to_string(),
            name: req.name,
            driver: DriverSpec::Dir {
                path: req.path,
                node_paths: self.node_path_ids(&req.node_paths)?,
            },
            nodes: self.node_ids(&req.nodes)?,
            default_for_disks: req.default_for_disks,
            overcommit: req.overcommit.unwrap_or(true),
            created_at: crate::tenancy::now(),
        };
        record::put(&self.database(), &ds, &record::root())?;
        self.probe_soon();
        Ok(self.datastore_view(ds))
    }

    pub async fn update_datastore(
        &self,
        key: &str,
        ch: DatastoreChanges,
    ) -> Result<DatastoreView, VmManagerError> {
        self.require_datastores()?;
        let db = self.database();
        let mut ds =
            record::find(&db, key)?.ok_or_else(|| DatastoreError::NotFound(key.to_string()))?;
        if ds.is_builtin() {
            return Err(DatastoreError::Conflict(
                "the built-in local datastore can't be changed; make another the default for disks instead"
                    .into(),
            )
            .into());
        }
        let disks: Vec<_> = self
            .images
            .list_disks()
            .into_iter()
            .filter(|d| d.datastore == ds.id)
            .collect();
        if let Some(paths) = &ch.node_paths {
            let paths = self.node_path_ids(paths)?;
            let DriverSpec::Dir { path, node_paths } = &mut ds.driver;
            // A hypervisor holds the old path while its VM runs (S14).
            let vms = self.vms.read().await;
            for d in &disks {
                let (Some(node), Some(vm)) = (&d.node, &d.attached_to) else {
                    continue;
                };
                let old = node_paths.get(node).unwrap_or(path);
                let new = paths.get(node).unwrap_or(path);
                if old != new && vms.get(vm).is_some_and(|v| v.status.instance.is_some()) {
                    return Err(DatastoreError::Conflict(format!(
                        "VM {vm} on node {node} has disk {} open at {old}; stop it first",
                        d.name
                    ))
                    .into());
                }
            }
            *node_paths = paths;
        }
        if let Some(nodes) = &ch.nodes {
            let nodes = self.node_ids(nodes)?;
            if !nodes.is_empty()
                && let Some(d) = disks
                    .iter()
                    .find(|d| d.node.as_ref().is_some_and(|n| !nodes.contains(n)))
            {
                return Err(DatastoreError::Conflict(format!(
                    "disk {} is on this datastore on node {}",
                    d.name,
                    d.node.as_deref().unwrap_or_default()
                ))
                .into());
            }
            ds.nodes = nodes;
        }
        if let Some(v) = ch.default_for_disks {
            ds.default_for_disks = v;
        }
        if let Some(v) = ch.overcommit {
            ds.overcommit = v;
        }
        record::put(&db, &ds, &record::root())?;
        self.probe_soon();
        Ok(self.datastore_view(ds))
    }

    pub async fn delete_datastore(&self, key: &str) -> Result<(), VmManagerError> {
        let db = self.database();
        let ds =
            record::find(&db, key)?.ok_or_else(|| DatastoreError::NotFound(key.to_string()))?;
        if ds.is_builtin() {
            return Err(DatastoreError::Conflict(
                "the built-in local datastore can't be deleted".into(),
            )
            .into());
        }
        record::delete(&db, &ds.id)?;
        Ok(())
    }
}
