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
    /// `lvm`: (volume group, thin pool) instead of `path`; `node_paths`
    /// then name each node's volume group.
    pub lvm: Option<(String, String)>,
    /// `hyperstor`: its cluster API, pool and a `mgmt` token to enroll
    /// with (spec/storage.md S20).
    pub hyperstor: Option<NewHyperstor>,
}

#[derive(Debug, Clone, Default)]
pub struct NewHyperstor {
    pub api: Vec<String>,
    pub pool: u32,
    pub token: zeroize::Zeroizing<String>,
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
        let id = uuid::Uuid::new_v4().to_string();
        // A hyperstor datastore: enroll now, so a token that doesn't work
        // makes no datastore (S20).
        let hyperstor = match req.hyperstor {
            None => None,
            Some(h) => Some(self.enroll_hyperstor(&id, h).await?),
        };
        let ds = Datastore {
            id: id.clone(),
            name: req.name,
            driver: if let Some(spec) = hyperstor {
                spec
            } else if let Some((vg, thin_pool)) = req.lvm {
                DriverSpec::Lvm {
                    vg,
                    thin_pool,
                    node_vgs: self.node_path_ids(&req.node_paths)?,
                }
            } else if req.shared {
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
        if let Err(e) = record::put(&self.database(), &ds, &record::root()) {
            if ds.driver.is_hyperstor() {
                self.forget_hyperstor_key(&id);
            }
            return Err(e.into());
        }
        self.probe_soon();
        Ok(self.datastore_view(ds))
    }

    /// Enrolls with a hyperstor cluster for datastore `id`, checks its pool
    /// as the new identity, and seals the key (S20). Only a server, which
    /// holds the secrets key, can.
    async fn enroll_hyperstor(
        &self,
        id: &str,
        h: NewHyperstor,
    ) -> Result<DriverSpec, VmManagerError> {
        use super::hyperstor;
        let invalid = |m: String| -> VmManagerError { DatastoreError::Invalid(m).into() };
        if h.api.is_empty() {
            return Err(invalid(
                "a hyperstor datastore names its cluster API (host:port)".into(),
            ));
        }
        let secrets = self
            .secrets_key()
            .ok_or_else(|| invalid("only a server can enroll with hyperstor".into()))?;
        let enrolled = hyperstor::enroll(&h.api, &h.token)
            .await
            .map_err(|e| invalid(format!("enrolling with hyperstor: {e}")))?;
        let opened = hyperstor::Identity {
            cert: enrolled.identity.cert.clone(),
            key: enrolled.key.clone(),
            root: enrolled.identity.root.clone(),
        };
        hyperstor::check_pool(&h.api, &opened, h.pool)
            .await
            .map_err(|e| invalid(format!("hyperstor: {e}")))?;
        secrets
            .seal_value(&record::hyperstor_secret_owner(id), "key", &enrolled.key)
            .map_err(|e| invalid(format!("sealing hyperstor's key: {e}")))?;
        Ok(DriverSpec::Hyperstor {
            api: h.api,
            map: enrolled.map,
            pool: h.pool,
            identity: Some(enrolled.identity),
            node_paths: BTreeMap::new(),
        })
    }

    pub(crate) fn forget_hyperstor_key(&self, id: &str) {
        if let Some(s) = self.secrets_key() {
            let _ = s.forget_value(&record::hyperstor_secret_owner(id), "key");
        }
    }

    /// Renews every hyperstor datastore's certificate that is due (a third
    /// of its life left): a new key, sealed, and the record updated. The
    /// leader's, from its probe rounds (S20).
    pub(crate) fn renew_hyperstor_identities(&self) {
        let db = self.database();
        if !db.can_write() {
            return;
        }
        let Some(secrets) = self.secrets_key() else {
            return;
        };
        for mut ds in record::list(&db).unwrap_or_default() {
            let Some(opened) = self.images.hyperstor_identity(&ds) else {
                continue;
            };
            let DriverSpec::Hyperstor { api, identity, .. } = &mut ds.driver else {
                continue;
            };
            match super::hyperstor::renew(api, &opened) {
                Ok(None) => {}
                Ok(Some(new)) => {
                    let owner = record::hyperstor_secret_owner(&ds.id);
                    if let Err(e) = secrets.seal_value(&owner, "key", &new.key) {
                        tracing::warn!(datastore = %ds.name, error = %e, "sealing hyperstor's renewed key");
                        continue;
                    }
                    *identity = Some(new.identity);
                    if let Err(e) = record::put(&db, &ds, &record::root()) {
                        tracing::warn!(datastore = %ds.name, error = %e, "recording hyperstor's renewed certificate");
                    } else {
                        tracing::info!(datastore = %ds.name, "renewed hyperstor's certificate");
                    }
                }
                Err(e) => {
                    tracing::warn!(datastore = %ds.name, error = %e, "renewing hyperstor's certificate");
                }
            }
        }
    }

    /// Removes hyperstor image volumes whose image is gone (S24). The
    /// leader's, from its probe rounds.
    pub(crate) fn collect_hyperstor_image_volumes(&self) {
        if !self.database().can_write() {
            return;
        }
        for ds in record::list(&self.database()).unwrap_or_default() {
            let Some(d) = self
                .images
                .hyperstor_identity(&ds)
                .and_then(|i| super::hyperstor::HyperstorDriver::of(&ds, Some(i)))
            else {
                continue;
            };
            match d.collect_image_volumes(|image| self.images.get_image(image).is_ok()) {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!(datastore = %ds.name, "removed {n} image volumes left behind")
                }
                Err(e) => {
                    tracing::debug!(datastore = %ds.name, error = %e, "looking for image volumes left behind")
                }
            }
        }
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
        let nodes = self.nodes();
        let node_name = |id: &str| {
            nodes
                .get(id)
                .ok()
                .flatten()
                .map(|n| n.spec.name)
                .unwrap_or_else(|| id.to_string())
        };
        let image_name = |id: &str| {
            self.images
                .get_image(id)
                .map(|i| i.name)
                .unwrap_or_else(|_| id.to_string())
        };
        record::delete(&db, &ds.id, &node_name, &image_name)?;
        if ds.driver.is_hyperstor() {
            self.forget_hyperstor_key(&ds.id);
        }
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
        // Onto or off block volumes the copy is standalone, and onto them
        // raw (spec/storage.md §11.1).
        let src_block = record::find(&db, &disk.datastore)?.is_some_and(|d| d.driver.is_block());
        let raw = dst.driver.is_block();
        disk.moving = Some(MoveSpec {
            datastore: dst.id.clone(),
            node: dest_node,
            mover,
            flatten: flatten || raw || src_block,
            raw,
        });
        drop(vms);
        self.images.put_disk(&disk)?;
        self.queue
            .add(crate::controller::queue::Key::Disk(disk.id.clone()));
        Ok(self.images.disk_response(&disk, false))
    }
}
