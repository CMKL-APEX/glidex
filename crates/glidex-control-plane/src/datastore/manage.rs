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
    /// One filesystem every node mounts (`shared-dir`), else each node's
    /// own directory (`dir`).
    pub shared: bool,
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
            driver: if req.shared {
                DriverSpec::SharedDir {
                    path: req.path,
                    node_paths: self.node_path_ids(&req.node_paths)?,
                }
            } else {
                DriverSpec::Dir {
                    path: req.path,
                    node_paths: self.node_path_ids(&req.node_paths)?,
                }
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
            let (path, node_paths) = ds.driver.paths();
            // A hypervisor holds the old path while its VM runs (S14), on
            // the VM's node (a shared disk names none of its own).
            let vms = self.vms.read().await;
            for d in &disks {
                let Some(vm) = d.attached_to.as_ref().and_then(|v| vms.get(v)) else {
                    continue;
                };
                let Some(node) = d
                    .node
                    .clone()
                    .or_else(|| vm.status.placement.as_ref().map(|p| p.node.clone()))
                else {
                    continue;
                };
                let old = node_paths.get(&node).unwrap_or(path);
                let new = paths.get(&node).unwrap_or(path);
                if old != new && vm.status.instance.is_some() {
                    let vm = &vm.name;
                    return Err(DatastoreError::Conflict(format!(
                        "VM {vm} on node {node} has disk {} open at {old}; stop it first",
                        d.name
                    ))
                    .into());
                }
            }
            *ds.driver.node_paths_mut() = paths;
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

impl VmManager {
    /// `POST /disks/{id}/move` (spec/storage.md §8.3): ask for disk `id` to
    /// go to `datastore`. One node, the mover, makes the move: the one that
    /// holds the file now and reaches the destination. A node-local
    /// destination is on the mover's node, so a disk moves between two
    /// nodes' own datastores in two steps, through a shared one.
    pub async fn move_disk(
        &self,
        id: &str,
        datastore: &str,
        node: Option<&str>,
        flatten: bool,
    ) -> Result<crate::images::DiskResponse, VmManagerError> {
        use crate::images::{DiskPhase, ImageError, MoveSpec};
        let invalid = |m: String| -> VmManagerError { ImageError::invalid_disk(m).into() };
        self.require_datastores()?;
        let db = self.database();
        let vms = self.vms.write().await;
        let mut disk = self.images.get_disk(id)?;
        if disk.deletion_requested_at.is_some() {
            return Err(ImageError::InUse(format!("disk {} is being deleted", disk.name)).into());
        }
        if let Some(m) = &disk.moving {
            return Err(ImageError::InUse(format!(
                "disk {} is already moving to datastore {}",
                disk.name, m.datastore
            ))
            .into());
        }
        if disk.phase != DiskPhase::Ready {
            return Err(
                ImageError::NotReady(format!("disk {} is {:?}", disk.name, disk.phase)).into(),
            );
        }
        if let Some(vm) = Self::disk_in_use(&vms, &disk.id) {
            return Err(ImageError::InUse(format!(
                "disk {} is in use by VM {vm}; stop it first",
                disk.name
            ))
            .into());
        }
        let dst = record::find(&db, datastore)?
            .ok_or_else(|| DatastoreError::NotFound(datastore.to_string()))?;
        if dst.id == disk.datastore {
            return Err(invalid(format!(
                "disk {} is on datastore {} already",
                disk.name, dst.name
            )));
        }
        let me = self.local_node_id();
        let rows = |ds: &str| record::rows(&db, ds).unwrap_or_default();
        let reaches = |ds: &Datastore, n: &str| {
            ds.is_builtin() || (ds.on_node(n) && rows(&ds.id).get(n).is_some_and(|r| r.reachable))
        };
        let shared_src = self.images.is_shared(&disk);
        let vm_node = disk.attached_to.as_ref().and_then(|v| {
            vms.get(v)
                .and_then(|v| v.status.placement.as_ref().map(|p| p.node.clone()))
        });
        // Who holds the file now.
        let holder = if shared_src {
            disk.worker.clone()
        } else {
            disk.node.clone()
        };
        let mover = match (holder, self.cluster().is_some()) {
            (_, false) => me.clone(),
            (Some(h), true) if !shared_src || reaches(&dst, &h) => h,
            // A shared disk: any node reaching both, its VM's if placed.
            (_, true) if shared_src => {
                let src = record::find(&db, &disk.datastore)?
                    .ok_or_else(|| DatastoreError::NotFound(disk.datastore.clone()))?;
                let mut ok: Vec<String> = rows(&src.id)
                    .into_keys()
                    .filter(|n| reaches(&src, n) && reaches(&dst, n))
                    .collect();
                ok.sort();
                match &vm_node {
                    Some(p) => ok.into_iter().find(|n| n == p),
                    None => ok.into_iter().next(),
                }
                .ok_or_else(|| {
                    invalid(format!(
                        "no node reaches both datastore {} and {}",
                        disk.datastore, dst.name
                    ))
                })?
            }
            _ => return Err(invalid(format!("disk {} is on no node", disk.name))),
        };
        if !reaches(&dst, &mover) {
            return Err(invalid(format!(
                "node {mover}, which has disk {}, doesn't reach datastore {}",
                disk.name, dst.name
            )));
        }
        let dest_node = (!dst.is_shared()).then(|| mover.clone());
        if let Some(want) = node {
            let want = self.resolve_node(want)?;
            if dest_node.as_ref() != Some(&want) {
                return Err(invalid(format!(
                    "disk {} can only be moved by node {mover}, which has it: move it to a shared datastore first, then from there",
                    disk.name
                )));
            }
        }
        // Its VM must reach it where it goes.
        if let Some(p) = &vm_node {
            let ok = match &dest_node {
                Some(n) => n == p,
                None => reaches(&dst, p),
            };
            if !ok {
                return Err(invalid(format!(
                    "disk {}'s VM is on node {p}, which wouldn't reach it on datastore {}",
                    disk.name, dst.name
                )));
            }
        }
        if !dst.overcommit {
            let need = self.images.estimated_size(&disk);
            let free = rows(&dst.id).get(&mover).map(|r| r.free_bytes).unwrap_or(0);
            if need > free {
                return Err(ImageError::InsufficientStorage {
                    message: format!(
                        "datastore {} has {free} bytes free on node {mover}, and disk {} needs {need}",
                        dst.name, disk.name
                    ),
                    details: serde_json::json!({ "free_bytes": free, "needed_bytes": need }),
                }
                .into());
            }
        }
        if shared_src {
            disk.worker = Some(mover.clone());
        }
        disk.moving = Some(MoveSpec {
            datastore: dst.id.clone(),
            node: dest_node,
            mover,
            flatten,
        });
        drop(vms);
        self.images.put_disk(&disk)?;
        self.queue
            .add(crate::controller::queue::Key::Disk(disk.id.clone()));
        Ok(self.images.disk_response(&disk, false))
    }
}
