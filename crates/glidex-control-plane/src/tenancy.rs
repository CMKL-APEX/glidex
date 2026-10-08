//! Projects and quotas (spec/security.md §6).
//!
//! Every VM, disk and guest credential belongs to exactly one project.
//! Projects live in the `projects` table of the control-plane database;
//! the `meta` table records the id of the `default` project and which
//! one-time migrations have run.

use crate::store::Db;
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

const PROJECTS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("projects");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// `meta` key holding the default project's id.
const META_DEFAULT_PROJECT: &str = "default_project";
/// `meta` key set once existing records were assigned to the default project.
pub const META_TENANCY_V1: &str = "tenancy_v1";
/// `meta` key set once the site's default quotas were given to the
/// default project.
const META_DEFAULT_QUOTAS: &str = "default_project_quotas";

pub const DEFAULT_PROJECT_NAME: &str = "default";

#[derive(Debug, Error)]
pub enum TenancyError {
    #[error("project not found: {0}")]
    NotFound(String),
    #[error("project already exists: {0}")]
    AlreadyExists(String),
    #[error("{0}")]
    Invalid(String),
    #[error("project storage error: {0}")]
    Storage(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for TenancyError {
            fn from(e: $t) -> Self {
                TenancyError::Storage(e.to_string())
            }
        }
    )*};
}
storage_from!(
    crate::store::StoreError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    serde_json::Error
);

/// Per-project limits; `None` is unlimited (spec §6.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quotas {
    #[serde(default)]
    pub vms: Option<u64>,
    #[serde(default)]
    pub vcpus: Option<u64>,
    #[serde(default)]
    pub memory_mib: Option<u64>,
    #[serde(default)]
    pub disk_gib: Option<u64>,
    #[serde(default)]
    pub running_vms: Option<u64>,
    /// Project networks (spec §6.2).
    #[serde(default = "default_network_quota")]
    pub networks: Option<u64>,
    /// VPC routers (spec/clustering.md §11.2a).
    #[serde(default = "default_router_quota")]
    pub routers: Option<u64>,
    /// Addresses of the site's external pool, one per VPC router with a gateway.
    #[serde(default = "default_router_quota")]
    pub external_ips: Option<u64>,
}

fn default_network_quota() -> Option<u64> {
    Some(2)
}

fn default_router_quota() -> Option<u64> {
    Some(1)
}

impl Quotas {
    /// Defaults for a new project: unlimited, except 2 project networks.
    pub fn new_project() -> Self {
        Quotas { networks: default_network_quota(), routers: default_router_quota(), external_ips: default_router_quota(), ..Default::default() }
    }
}

/// Current use of a project, compared against its quotas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub vms: u64,
    pub vcpus: u64,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub running_vms: u64,
    pub networks: u64,
    pub routers: u64,
    pub external_ips: u64,
}

/// One limit that a request would go over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuotaOverrun {
    pub resource: &'static str,
    pub limit: u64,
    pub used: u64,
    pub requested: u64,
}

impl std::fmt::Display for QuotaOverrun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} quota exceeded: limit {}, in use {}, requested {}",
            self.resource, self.limit, self.used, self.requested
        )
    }
}

/// Whether the caller may go over quota (`exceedQuota`, spec §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QuotaMode {
    #[default]
    Enforce,
    MayExceed,
}

/// What a request adds to a project's use.
#[derive(Debug, Clone, Copy, Default)]
pub struct Delta {
    pub vms: u64,
    pub vcpus: u64,
    pub memory_mib: u64,
    pub disk_gib: u64,
    pub running_vms: u64,
    pub networks: u64,
    pub routers: u64,
    pub external_ips: u64,
}

/// Limits `delta` would exceed on top of `usage`.
pub fn overruns(quotas: &Quotas, usage: &Usage, delta: &Delta) -> Vec<QuotaOverrun> {
    let mut out = Vec::new();
    let mut check = |resource: &'static str, limit: Option<u64>, used: u64, requested: u64| {
        if let Some(limit) = limit {
            if requested > 0 && used + requested > limit {
                out.push(QuotaOverrun { resource, limit, used, requested });
            }
        }
    };
    check("vms", quotas.vms, usage.vms, delta.vms);
    check("vcpus", quotas.vcpus, usage.vcpus, delta.vcpus);
    check("memory_mib", quotas.memory_mib, usage.memory_mib, delta.memory_mib);
    check("disk_gib", quotas.disk_gib, usage.disk_gib, delta.disk_gib);
    check("running_vms", quotas.running_vms, usage.running_vms, delta.running_vms);
    check("networks", quotas.networks, usage.networks, delta.networks);
    check("routers", quotas.routers, usage.routers, delta.routers);
    check("external_ips", quotas.external_ips, usage.external_ips, delta.external_ips);
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "Quotas::new_project")]
    pub quotas: Quotas,
    pub created_at: u64,
}

pub fn validate_project_name(name: &str) -> Result<(), TenancyError> {
    let ok = !name.is_empty()
        && name.len() <= 32
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(TenancyError::Invalid(format!(
            "invalid project name '{}': use [a-z0-9-], 1-32 characters",
            name
        )))
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct ProjectStore {
    db: Arc<Db>,
}

impl ProjectStore {
    /// Open the tables and make sure the default project exists.
    pub fn new(db: Arc<Db>) -> Result<Self, TenancyError> {
        let store = Self { db };
        // A follower finds the default project in the replicated data; a
        // node that can't write now leaves it to the leader.
        if store.meta(META_DEFAULT_PROJECT)?.is_none() && store.db.can_write() {
            let p = Project {
                id: uuid::Uuid::new_v4().to_string(),
                name: DEFAULT_PROJECT_NAME.into(),
                description: "Created at install; holds resources from before projects existed.".into(),
                quotas: Quotas::new_project(),
                created_at: now(),
            };
            store.put(&p)?;
            store.set_meta(META_DEFAULT_PROJECT, p.id.as_bytes())?;
        }
        Ok(store)
    }

    /// Give the default project the site's default quotas, once: later
    /// changes to its quotas (through the API) are kept.
    pub fn adopt_default_quotas(&self, quotas: &Quotas) -> Result<bool, TenancyError> {
        if self.meta(META_DEFAULT_QUOTAS)?.is_some() {
            return Ok(false);
        }
        let id = self.default_project_id();
        if let Some(mut p) = self.get(&id)? {
            p.quotas = quotas.clone();
            self.put(&p)?;
        }
        self.set_meta(META_DEFAULT_QUOTAS, b"1")?;
        Ok(true)
    }

    pub fn default_project_id(&self) -> String {
        self.meta(META_DEFAULT_PROJECT)
            .ok()
            .flatten()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default()
    }

    pub fn meta(&self, key: &str) -> Result<Option<Vec<u8>>, TenancyError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(META_TABLE)?;
        Ok(table.get(key)?.map(|v| v.value().to_vec()))
    }

    pub fn set_meta(&self, key: &str, value: &[u8]) -> Result<(), TenancyError> {
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut table = txn.open_table(META_TABLE)?;
            table.insert(key, value)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<Project>, TenancyError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PROJECTS_TABLE)?;
        Ok(match table.get(id)? {
            Some(v) => Some(serde_json::from_slice(v.value())?),
            None => None,
        })
    }

    pub fn list(&self) -> Result<Vec<Project>, TenancyError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PROJECTS_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v) = entry?;
            out.push(serde_json::from_slice::<Project>(v.value())?);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// A project by id, or by name.
    pub fn resolve(&self, key: &str) -> Result<Project, TenancyError> {
        if let Some(p) = self.get(key)? {
            return Ok(p);
        }
        self.list()?
            .into_iter()
            .find(|p| p.name == key)
            .ok_or_else(|| TenancyError::NotFound(key.to_string()))
    }

    pub fn put(&self, p: &Project) -> Result<(), TenancyError> {
        let bytes = serde_json::to_vec(p)?;
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut table = txn.open_table(PROJECTS_TABLE)?;
            table.insert(p.id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn create(&self, name: &str, description: String, quotas: Option<Quotas>) -> Result<Project, TenancyError> {
        validate_project_name(name)?;
        if self.list()?.iter().any(|p| p.name == name) {
            return Err(TenancyError::AlreadyExists(name.to_string()));
        }
        let p = Project {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            description,
            quotas: quotas.unwrap_or_else(Quotas::new_project),
            created_at: now(),
        };
        self.put(&p)?;
        Ok(p)
    }

    pub fn delete(&self, id: &str) -> Result<(), TenancyError> {
        if id == self.default_project_id() {
            return Err(TenancyError::Invalid("the default project can't be deleted".into()));
        }
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut table = txn.open_table(PROJECTS_TABLE)?;
            if table.remove(id)?.is_none() {
                return Err(TenancyError::NotFound(id.to_string()));
            }
        }
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (ProjectStore, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Db::create(dir.path().join("p.db")).unwrap());
        (ProjectStore::new(db).unwrap(), dir)
    }

    #[test]
    fn default_project_is_created_once() {
        let (s, _d) = store();
        let id = s.default_project_id();
        assert!(!id.is_empty());
        assert_eq!(s.resolve("default").unwrap().id, id);
        let again = ProjectStore::new(s.db.clone()).unwrap();
        assert_eq!(again.default_project_id(), id);
        assert_eq!(again.list().unwrap().len(), 1);
    }

    #[test]
    fn create_resolve_delete() {
        let (s, _d) = store();
        let p = s.create("lab-1", String::new(), None).unwrap();
        assert_eq!(p.quotas.networks, Some(2));
        assert_eq!(s.resolve("lab-1").unwrap(), p);
        assert_eq!(s.resolve(&p.id).unwrap(), p);
        assert!(matches!(s.create("lab-1", String::new(), None), Err(TenancyError::AlreadyExists(_))));
        assert!(matches!(s.create("Lab", String::new(), None), Err(TenancyError::Invalid(_))));
        assert!(s.delete(&s.default_project_id()).is_err());
        s.delete(&p.id).unwrap();
        assert!(matches!(s.resolve("lab-1"), Err(TenancyError::NotFound(_))));
    }

    #[test]
    fn default_project_takes_site_quotas_once() {
        let (s, _d) = store();
        let site = Quotas { vms: Some(5), networks: Some(3), ..Default::default() };
        assert!(s.adopt_default_quotas(&site).unwrap());
        assert_eq!(s.resolve("default").unwrap().quotas, site);
        // An admin's later change survives the next start.
        let mut p = s.resolve("default").unwrap();
        p.quotas.vms = Some(50);
        s.put(&p).unwrap();
        assert!(!s.adopt_default_quotas(&site).unwrap());
        assert_eq!(s.resolve("default").unwrap().quotas.vms, Some(50));
    }

    #[test]
    fn overrun_detection() {
        let q = Quotas { vms: Some(2), vcpus: Some(4), ..Default::default() };
        let u = Usage { vms: 2, vcpus: 3, ..Default::default() };
        let o = overruns(&q, &u, &Delta { vms: 1, vcpus: 1, ..Default::default() });
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].resource, "vms");
        assert!(overruns(&q, &u, &Delta::default()).is_empty());
        // Unlimited when None.
        assert!(overruns(&Quotas { networks: None, ..Default::default() }, &u, &Delta { networks: 9, ..Default::default() }).is_empty());
    }

    #[test]
    fn router_quotas_default_to_one_each() {
        let q = Quotas::new_project();
        assert_eq!((q.routers, q.external_ips), (Some(1), Some(1)));
        let u = Usage { routers: 1, external_ips: 1, ..Default::default() };
        let o = overruns(&q, &u, &Delta { routers: 1, external_ips: 1, ..Default::default() });
        assert_eq!(o.iter().map(|o| o.resource).collect::<Vec<_>>(), ["routers", "external_ips"]);
        // An internal router counts only against `routers`.
        let o = overruns(&q, &Usage::default(), &Delta { routers: 1, ..Default::default() });
        assert!(o.is_empty());
        let none: Quotas = serde_json::from_str("{}").unwrap();
        assert_eq!((none.routers, none.external_ips), (Some(1), Some(1)), "records from before the router quotas get the default");
    }
}
