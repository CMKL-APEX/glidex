//! Directories, tenants and admissions (spec/tenancy.md §4, T0).
//!
//! A host is a cluster that hosts one directory, `local`. Tenant `admin`
//! owns the directory and operates the cluster; tenant `default` holds the
//! projects from before tenants existed. Every project belongs to one
//! tenant (`Project::tenant_id`); a tenant's projects may allocate on this
//! cluster only while it has an admission here (`base.admission`).
//!
//! Membership (D3): a user or team is a member of a tenant when it is
//! listed in `tenant_members`, or has a role link on the tenant or on one
//! of its projects (`members_of`, computed by the caller from the links).

use crate::authz::Ent;
use crate::store::Db;
use crate::tenancy::Quotas;
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

const DIRECTORIES: TableDefinition<&str, &[u8]> = TableDefinition::new("directories");
const TENANTS: TableDefinition<&str, &[u8]> = TableDefinition::new("tenants");
const ADMISSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("admissions");
const MEMBERS: TableDefinition<&str, &[u8]> = TableDefinition::new("tenant_members");
/// Tenant pools on this cluster, by `<tenant>/<name>` (spec/host-topology.md §9.1a).
const DEVICE_POOLS: TableDefinition<&str, &[u8]> = TableDefinition::new("tenant_device_pools");
const PROJECTS: TableDefinition<&str, &[u8]> = TableDefinition::new("projects");
const OFFERS: TableDefinition<&str, &[u8]> = TableDefinition::new("admission_offers");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// The directory a host hosts (D32).
pub const LOCAL_DIRECTORY: &str = "local";
/// Owns the local directory and operates this cluster.
pub const ADMIN_TENANT: &str = "admin";
/// Holds the projects from before tenants existed.
pub const DEFAULT_TENANT: &str = "default";
/// `meta` key: the tenant operating this cluster.
const META_OPERATOR: &str = "cluster_operator";
/// `meta` key set once the directory and the two tenants exist.
const META_TENANTS_V1: &str = "tenants_v1";

#[derive(Debug, Error)]
pub enum TenantError {
    #[error("tenant not found: {0}")]
    NotFound(String),
    #[error("tenant already exists: {0}")]
    AlreadyExists(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    InUse(String),
    #[error("tenant storage error: {0}")]
    Storage(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for TenantError {
            fn from(e: $t) -> Self {
                TenantError::Storage(e.to_string())
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

fn now() -> u64 {
    crate::tenancy::now()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directory {
    pub id: String,
    pub name: String,
    /// The tenant whose members administer the directory (D7).
    pub owner_tenant: String,
    pub created_at: u64,
    /// Its issuer URL, fixed when it first issued a token (D52).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// Its SPIFFE trust domain, chosen at creation or fixed when it first
    /// issued a workload token (workload-identity.md W13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_domain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenant {
    pub id: String,
    pub directory_id: String,
    /// `[a-z0-9-]{1,32}`, unique in the directory (D24).
    pub slug: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub owns_directory: bool,
    /// Operates clusters (D4).
    #[serde(default)]
    pub operator: bool,
    #[serde(default)]
    pub suspended: bool,
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdmissionStatus {
    Admitted,
    /// No new allocations or starts; running resources stay (D9).
    Suspended,
}

/// A tenant's admission to this cluster, with its cap (D9, D19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub tenant_id: String,
    pub directory_id: String,
    /// The tenant's limits on this cluster; `None` fields are unlimited.
    pub cap: Quotas,
    pub status: AdmissionStatus,
    pub by: String,
    pub at: u64,
}

/// A tenant's share of a cluster device pool, split among some of its
/// projects (spec/host-topology.md §9.1a, H17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantPool {
    pub tenant_id: String,
    pub name: String,
    /// The cluster pool (`pci.pools`) its devices come from.
    pub from: String,
    /// Devices its projects' VMs ask for at once, in all.
    pub max: u32,
    /// Project ids that may ask for it.
    pub projects: Vec<String>,
    pub by: String,
    pub at: u64,
}

/// An explicit member of a tenant (D3): a user or a team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub tenant_id: String,
    pub principal: Ent,
    pub by: String,
    pub at: u64,
}

/// [`TenantStore::add_member`] inside another store's write (a SCIM
/// request, spec/tenancy.md §8.1). A tenant that isn't there is skipped.
pub(crate) fn add_member_in(
    tx: &crate::store::Tx<'_>,
    tenant: &str,
    principal: Ent,
    by: &str,
) -> Result<(), crate::auth::store::StoreError> {
    if tx.open_table(TENANTS)?.get(tenant)?.is_none() {
        tracing::warn!("connector tenant {tenant} is gone: its new user joins no tenant");
        return Ok(());
    }
    let mut t = tx.open_table(MEMBERS)?;
    let key = member_key(tenant, &principal);
    if t.get(key.as_str())?.is_none() {
        let m = Member {
            tenant_id: tenant.into(),
            principal,
            by: by.into(),
            at: now(),
        };
        t.insert(key.as_str(), serde_json::to_vec(&m)?.as_slice())?;
    }
    Ok(())
}

fn member_key(tenant: &str, principal: &Ent) -> String {
    format!("{}/{}:{}", tenant, principal.type_name(), principal.id())
}

pub fn validate_slug(slug: &str) -> Result<(), TenantError> {
    let ok = !slug.is_empty()
        && slug.len() <= 32
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(TenantError::Invalid(format!(
            "invalid tenant slug '{}': use [a-z0-9-], 1-32 characters",
            slug
        )))
    }
}

/// Limits that limit nothing: an admission's cap unless the operator sets one.
pub fn unlimited() -> Quotas {
    Quotas {
        networks: None,
        routers: None,
        external_ips: None,
        ..Default::default()
    }
}

/// An admission offered to a tenant of another directory (spec/tenancy.md
/// D64): its token is shown once; only its secret's hash is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Offer {
    pub id: String,
    /// The tenant's slug at that directory.
    pub tenant: String,
    /// That directory's issuer URL.
    pub directory: String,
    pub cap: Quotas,
    pub secret_hash: String,
    pub by: String,
    pub created_at: u64,
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redeemed_at: Option<u64>,
}

impl Offer {
    pub fn is_open(&self, now: u64) -> bool {
        self.redeemed_at.is_none() && now < self.expires_at
    }
}

/// Everything about tenants this control plane knows, read in one go for
/// an authorization decision.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub directory: Option<Directory>,
    /// Every directory hosted here, `local` included (D27, D58).
    pub directories: Vec<Directory>,
    pub tenants: Vec<Tenant>,
    pub admissions: Vec<Admission>,
    pub members: Vec<Member>,
    pub operator: String,
}

impl Snapshot {
    pub fn tenant(&self, id: &str) -> Option<&Tenant> {
        self.tenants.iter().find(|t| t.id == id)
    }

    /// Admitted (not suspended) here.
    pub fn admitted(&self, tenant: &str) -> bool {
        self.admissions
            .iter()
            .any(|a| a.tenant_id == tenant && a.status == AdmissionStatus::Admitted)
    }

    pub fn owner(&self) -> &str {
        self.directory
            .as_ref()
            .map(|d| d.owner_tenant.as_str())
            .unwrap_or(ADMIN_TENANT)
    }

    /// Directory `id`'s owner tenant.
    pub fn owner_of(&self, id: &str) -> Option<&str> {
        self.directories
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.owner_tenant.as_str())
    }
}

pub struct TenantStore {
    db: Arc<Db>,
}

impl TenantStore {
    /// Open the tables and make sure the local directory, its owner and
    /// the default tenant exist (spec/tenancy.md §16 steps 1-2). A node
    /// that can't write leaves it to the leader.
    pub fn new(db: Arc<Db>) -> Result<Self, TenantError> {
        let store = Self { db };
        if store.meta(META_TENANTS_V1)?.is_none() && store.db.can_write() {
            store.bootstrap()?;
        }
        Ok(store)
    }

    fn bootstrap(&self) -> Result<(), TenantError> {
        let at = now();
        let directory = Directory {
            id: LOCAL_DIRECTORY.into(),
            name: "local".into(),
            owner_tenant: ADMIN_TENANT.into(),
            created_at: at,
            issuer: None,
            trust_domain: None,
        };
        let admin = Tenant {
            id: ADMIN_TENANT.into(),
            directory_id: LOCAL_DIRECTORY.into(),
            slug: ADMIN_TENANT.into(),
            display_name: "Operator of this host".into(),
            owns_directory: true,
            operator: true,
            suspended: false,
            created_at: at,
        };
        let default = Tenant {
            id: DEFAULT_TENANT.into(),
            directory_id: LOCAL_DIRECTORY.into(),
            slug: DEFAULT_TENANT.into(),
            display_name: "Projects from before tenants".into(),
            owns_directory: false,
            operator: false,
            suspended: false,
            created_at: at,
        };
        let admit = |t: &str| Admission {
            tenant_id: t.into(),
            directory_id: LOCAL_DIRECTORY.into(),
            cap: unlimited(),
            status: AdmissionStatus::Admitted,
            by: "bootstrap".into(),
            at,
        };
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut d = txn.open_table(DIRECTORIES)?;
            if d.get(LOCAL_DIRECTORY)?.is_none() {
                d.insert(LOCAL_DIRECTORY, serde_json::to_vec(&directory)?.as_slice())?;
            }
            let mut t = txn.open_table(TENANTS)?;
            for ten in [&admin, &default] {
                if t.get(ten.id.as_str())?.is_none() {
                    t.insert(ten.id.as_str(), serde_json::to_vec(ten)?.as_slice())?;
                }
            }
            let mut a = txn.open_table(ADMISSIONS)?;
            for id in [ADMIN_TENANT, DEFAULT_TENANT] {
                if a.get(id)?.is_none() {
                    a.insert(id, serde_json::to_vec(&admit(id))?.as_slice())?;
                }
            }
            let mut m = txn.open_table(META)?;
            if m.get(META_OPERATOR)?.is_none() {
                m.insert(META_OPERATOR, ADMIN_TENANT.as_bytes())?;
            }
            m.insert(META_TENANTS_V1, b"1".as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// A standalone host's cluster id (D56, D69): random, made once, so
    /// two standalone hosts never take each other's access tokens.
    pub fn standalone_id(&self) -> Result<String, TenantError> {
        const KEY: &str = "standalone_cluster_id";
        if let Some(v) = self.meta(KEY)? {
            return Ok(String::from_utf8_lossy(&v).into_owned());
        }
        let id = format!("standalone-{}", uuid::Uuid::new_v4());
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut m = txn.open_table(META)?;
            if let Some(v) = m.get(KEY)? {
                return Ok(String::from_utf8_lossy(v.value()).into_owned());
            }
            m.insert(KEY, id.as_bytes())?;
        }
        txn.commit()?;
        Ok(id)
    }

    fn meta(&self, key: &str) -> Result<Option<Vec<u8>>, TenantError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(META) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(table.get(key)?.map(|v| v.value().to_vec()))
    }

    fn read_all<T: serde::de::DeserializeOwned>(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
    ) -> Result<Vec<T>, TenantError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, v) = entry?;
            out.push(serde_json::from_slice(v.value())?);
        }
        Ok(out)
    }

    fn put<T: Serialize>(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
        key: &str,
        v: &T,
    ) -> Result<(), TenantError> {
        let bytes = serde_json::to_vec(v)?;
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut table = txn.open_table(def)?;
            table.insert(key, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    fn remove(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
        key: &str,
    ) -> Result<bool, TenantError> {
        let txn = self.db.begin(crate::store::Origin::Api)?;
        let removed = {
            let mut table = txn.open_table(def)?;
            table.remove(key)?.is_some()
        };
        txn.commit()?;
        Ok(removed)
    }

    /// Whether the one-time step `key` has run.
    pub fn done(&self, key: &str) -> Result<bool, TenantError> {
        Ok(self.meta(key)?.is_some())
    }

    pub fn mark_done(&self, key: &str) -> Result<(), TenantError> {
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut m = txn.open_table(META)?;
            m.insert(key, b"1".as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn can_write(&self) -> bool {
        self.db.can_write()
    }

    // ---- reads ----------------------------------------------------------

    pub fn snapshot(&self) -> Result<Snapshot, TenantError> {
        Ok(Snapshot {
            directory: self.directory()?,
            directories: self.directories()?,
            tenants: self.list()?,
            admissions: self.admissions()?,
            members: self.read_all(MEMBERS)?,
            operator: self.operator()?,
        })
    }

    /// Every directory this control plane hosts.
    pub fn directories(&self) -> Result<Vec<Directory>, TenantError> {
        self.read_all::<Directory>(DIRECTORIES)
    }

    pub fn get_directory(&self, id: &str) -> Result<Option<Directory>, TenantError> {
        Ok(self.directories()?.into_iter().find(|d| d.id == id))
    }

    /// A new hosted directory `id` (D27, D59) and its owner tenant `owner`,
    /// in one write.
    pub fn create_directory(
        &self,
        id: &str,
        name: &str,
        owner: &str,
    ) -> Result<(Directory, Tenant), TenantError> {
        validate_slug(id)?;
        validate_slug(owner)?;
        if self.get_directory(id)?.is_some() {
            return Err(TenantError::AlreadyExists(format!("directory {id}")));
        }
        let at = now();
        let t = Tenant {
            id: uuid::Uuid::new_v4().to_string(),
            directory_id: id.into(),
            slug: owner.into(),
            display_name: name.into(),
            owns_directory: true,
            operator: false,
            suspended: false,
            created_at: at,
        };
        let d = Directory {
            id: id.into(),
            name: name.into(),
            owner_tenant: t.id.clone(),
            created_at: at,
            issuer: None,
            trust_domain: None,
        };
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            txn.open_table(DIRECTORIES)?
                .insert(id, serde_json::to_vec(&d)?.as_slice())?;
            txn.open_table(TENANTS)?
                .insert(t.id.as_str(), serde_json::to_vec(&t)?.as_slice())?;
        }
        txn.commit()?;
        Ok((d, t))
    }

    /// Whether any project belongs to `tenant`.
    pub fn has_projects(&self, tenant: &str) -> Result<bool, TenantError> {
        Ok(self
            .read_all::<crate::tenancy::Project>(PROJECTS)?
            .iter()
            .any(|p| p.tenant_id == tenant))
    }

    /// Remove hosted directory `id` and its tenants (the caller checked
    /// that nothing else is left in it). Never `local`.
    pub fn delete_directory(&self, id: &str) -> Result<(), TenantError> {
        if id == LOCAL_DIRECTORY {
            return Err(TenantError::Invalid(
                "directory local can't be deleted".into(),
            ));
        }
        let tenants: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|t| t.directory_id == id)
            .map(|t| t.id)
            .collect();
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut d = txn.open_table(DIRECTORIES)?;
            if d.remove(id)?.is_none() {
                return Err(TenantError::NotFound(format!("directory {id}")));
            }
            let mut t = txn.open_table(TENANTS)?;
            for k in &tenants {
                t.remove(k.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    // ---- offers (D64) ---------------------------------------------------

    pub fn offers(&self) -> Result<Vec<Offer>, TenantError> {
        self.read_all(OFFERS)
    }

    pub fn put_offer(&self, o: &Offer) -> Result<(), TenantError> {
        self.put(OFFERS, &o.id, o)
    }

    pub fn remove_offer(&self, id: &str) -> Result<bool, TenantError> {
        self.remove(OFFERS, id)
    }

    /// Fix `id`'s issuer URL (D52); the first one written stays unless
    /// `replace`.
    pub fn set_issuer(
        &self,
        id: &str,
        issuer: &str,
        replace: bool,
    ) -> Result<Directory, TenantError> {
        let mut d = self
            .get_directory(id)?
            .ok_or_else(|| TenantError::NotFound(format!("directory {id}")))?;
        if d.issuer.is_some() && !replace {
            return Ok(d);
        }
        d.issuer = Some(issuer.to_string());
        self.put(DIRECTORIES, id, &d)?;
        Ok(d)
    }

    /// Fix `id`'s trust domain to `td` unless it already has one, which
    /// never changes (workload-identity.md W13). The directory as it is now.
    pub fn set_trust_domain(&self, id: &str, td: &str) -> Result<Directory, TenantError> {
        let mut d = self
            .get_directory(id)?
            .ok_or_else(|| TenantError::NotFound(format!("directory {id}")))?;
        if d.trust_domain.is_some() {
            return Ok(d);
        }
        if !crate::directory::workload::valid_trust_domain(td) {
            return Err(TenantError::Invalid(format!(
                "invalid trust domain '{td}': lowercase letters, digits, '.', '-' and '_'"
            )));
        }
        d.trust_domain = Some(td.to_string());
        self.put(DIRECTORIES, id, &d)?;
        Ok(d)
    }

    pub fn directory(&self) -> Result<Option<Directory>, TenantError> {
        Ok(self
            .read_all::<Directory>(DIRECTORIES)?
            .into_iter()
            .find(|d| d.id == LOCAL_DIRECTORY))
    }

    /// The tenant operating this cluster.
    pub fn operator(&self) -> Result<String, TenantError> {
        Ok(self
            .meta(META_OPERATOR)?
            .map(|v| String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_else(|| ADMIN_TENANT.into()))
    }

    pub fn list(&self) -> Result<Vec<Tenant>, TenantError> {
        let mut out: Vec<Tenant> = self.read_all(TENANTS)?;
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<Option<Tenant>, TenantError> {
        Ok(self.list()?.into_iter().find(|t| t.id == id))
    }

    /// A tenant by id or by slug, slugs being `local`'s.
    pub fn resolve(&self, key: &str) -> Result<Tenant, TenantError> {
        self.resolve_in(LOCAL_DIRECTORY, key)
    }

    /// A tenant by id, or by its slug in `directory` (D24).
    pub fn resolve_in(&self, directory: &str, key: &str) -> Result<Tenant, TenantError> {
        let all = self.list()?;
        all.iter()
            .find(|t| t.id == key)
            .or_else(|| {
                all.iter()
                    .find(|t| t.slug == key && t.directory_id == directory)
            })
            .cloned()
            .ok_or_else(|| TenantError::NotFound(key.to_string()))
    }

    pub fn admissions(&self) -> Result<Vec<Admission>, TenantError> {
        let mut out: Vec<Admission> = self.read_all(ADMISSIONS)?;
        out.sort_by(|a, b| a.tenant_id.cmp(&b.tenant_id));
        Ok(out)
    }

    pub fn admission(&self, tenant: &str) -> Result<Option<Admission>, TenantError> {
        Ok(self
            .admissions()?
            .into_iter()
            .find(|a| a.tenant_id == tenant))
    }

    pub fn members(&self, tenant: &str) -> Result<Vec<Member>, TenantError> {
        let mut out: Vec<Member> = self
            .read_all::<Member>(MEMBERS)?
            .into_iter()
            .filter(|m| m.tenant_id == tenant)
            .collect();
        out.sort_by_key(|m| member_key(&m.tenant_id, &m.principal));
        Ok(out)
    }

    /// The tenant a project belongs to (records from before tenants have
    /// none and belong to `default`).
    pub fn project_tenant(&self, project: &str) -> Result<Option<String>, TenantError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(PROJECTS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(match table.get(project)? {
            Some(v) => {
                Some(serde_json::from_slice::<crate::tenancy::Project>(v.value())?.tenant_id)
            }
            None => None,
        })
    }

    /// The projects of tenants `tenants`.
    pub fn projects_of(&self, tenants: &[String]) -> Result<Vec<String>, TenantError> {
        if tenants.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .read_all::<crate::tenancy::Project>(PROJECTS)?
            .into_iter()
            .filter(|p| tenants.contains(&p.tenant_id))
            .map(|p| p.id)
            .collect())
    }

    // ---- writes (through the service layer, spec/tenancy.md §14.1) -----

    pub fn create(
        &self,
        slug: &str,
        display_name: String,
        operator: bool,
    ) -> Result<Tenant, TenantError> {
        self.create_in(LOCAL_DIRECTORY, slug, display_name, operator)
    }

    /// A tenant of `directory`; its slug is unique there (D24).
    pub fn create_in(
        &self,
        directory: &str,
        slug: &str,
        display_name: String,
        operator: bool,
    ) -> Result<Tenant, TenantError> {
        validate_slug(slug)?;
        if self
            .list()?
            .iter()
            .any(|t| t.id == slug || (t.slug == slug && t.directory_id == directory))
        {
            return Err(TenantError::AlreadyExists(slug.to_string()));
        }
        let t = Tenant {
            id: uuid::Uuid::new_v4().to_string(),
            directory_id: directory.into(),
            slug: slug.into(),
            display_name,
            owns_directory: false,
            operator,
            suspended: false,
            created_at: now(),
        };
        self.put(TENANTS, &t.id.clone(), &t)?;
        Ok(t)
    }

    pub fn update(&self, t: &Tenant) -> Result<(), TenantError> {
        if self.get(&t.id)?.is_none() {
            return Err(TenantError::NotFound(t.id.clone()));
        }
        self.put(TENANTS, &t.id, t)
    }

    pub fn delete(&self, id: &str) -> Result<(), TenantError> {
        if [ADMIN_TENANT, DEFAULT_TENANT].contains(&id) {
            return Err(TenantError::Invalid(format!(
                "tenant {id} can't be deleted"
            )));
        }
        if !self.remove(TENANTS, id)? {
            return Err(TenantError::NotFound(id.into()));
        }
        self.remove(ADMISSIONS, id)?;
        self.set_device_pools(id, Vec::new())?;
        for m in self.members(id)? {
            self.remove(MEMBERS, &member_key(id, &m.principal))?;
        }
        Ok(())
    }

    pub fn admit(
        &self,
        tenant: &str,
        cap: Quotas,
        status: AdmissionStatus,
        by: &str,
    ) -> Result<Admission, TenantError> {
        let t = self
            .get(tenant)?
            .ok_or_else(|| TenantError::NotFound(tenant.into()))?;
        let a = Admission {
            tenant_id: t.id.clone(),
            directory_id: t.directory_id,
            cap,
            status,
            by: by.into(),
            at: now(),
        };
        self.put(ADMISSIONS, &t.id, &a)?;
        Ok(a)
    }

    /// Admit a tenant of a directory this cluster trusts (D65): it isn't
    /// in this host's tenants, so its admission names the trusted directory.
    pub fn admit_remote(
        &self,
        tenant_id: &str,
        trusted_directory: &str,
        cap: Quotas,
        by: &str,
    ) -> Result<Admission, TenantError> {
        let a = Admission {
            tenant_id: tenant_id.into(),
            directory_id: trusted_directory.into(),
            cap,
            status: AdmissionStatus::Admitted,
            by: by.into(),
            at: now(),
        };
        self.put(ADMISSIONS, tenant_id, &a)?;
        Ok(a)
    }

    pub fn remove_admission(&self, tenant: &str) -> Result<(), TenantError> {
        if !self.remove(ADMISSIONS, tenant)? {
            return Err(TenantError::NotFound(format!("no admission for {tenant}")));
        }
        Ok(())
    }

    pub fn add_member(
        &self,
        tenant: &str,
        principal: Ent,
        by: &str,
    ) -> Result<Member, TenantError> {
        if !matches!(principal, Ent::User(_) | Ent::Team(_)) {
            return Err(TenantError::Invalid(
                "tenant members are users or teams".into(),
            ));
        }
        if self.get(tenant)?.is_none() {
            return Err(TenantError::NotFound(tenant.into()));
        }
        let m = Member {
            tenant_id: tenant.into(),
            principal,
            by: by.into(),
            at: now(),
        };
        self.put(MEMBERS, &member_key(tenant, &m.principal), &m)?;
        Ok(m)
    }

    /// The tenant's pools on this cluster, by name.
    pub fn device_pools(&self, tenant: &str) -> Result<Vec<TenantPool>, TenantError> {
        Ok(self
            .read_all::<TenantPool>(DEVICE_POOLS)?
            .into_iter()
            .filter(|p| p.tenant_id == tenant)
            .collect())
    }

    pub fn device_pool(&self, tenant: &str, name: &str) -> Result<Option<TenantPool>, TenantError> {
        Ok(self
            .device_pools(tenant)?
            .into_iter()
            .find(|p| p.name == name))
    }

    /// Replace the tenant's pools with `pools`, in one write.
    pub fn set_device_pools(
        &self,
        tenant: &str,
        pools: Vec<TenantPool>,
    ) -> Result<(), TenantError> {
        let prefix = format!("{tenant}/");
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut t = txn.open_table(DEVICE_POOLS)?;
            let old: Vec<String> = t
                .iter()?
                .filter_map(|e| e.ok())
                .map(|(k, _)| k.value().to_string())
                .filter(|k| k.starts_with(&prefix))
                .collect();
            for k in old {
                t.remove(k.as_str())?;
            }
            for p in &pools {
                let key = format!("{prefix}{}", p.name);
                t.insert(key.as_str(), serde_json::to_vec(p)?.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    pub fn remove_member(&self, tenant: &str, principal: &Ent) -> Result<(), TenantError> {
        if !self.remove(MEMBERS, &member_key(tenant, principal))? {
            return Err(TenantError::NotFound(format!(
                "{principal} is not a listed member of {tenant}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (TenantStore, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Db::create(dir.path().join("t.db")).unwrap());
        (TenantStore::new(db).unwrap(), dir)
    }

    #[test]
    fn bootstrap_makes_the_directory_and_two_tenants_once() {
        let (s, _d) = store();
        let snap = s.snapshot().unwrap();
        assert_eq!(snap.owner(), ADMIN_TENANT);
        assert_eq!(snap.operator, ADMIN_TENANT);
        assert!(
            snap.tenant(ADMIN_TENANT).unwrap().owns_directory
                && snap.tenant(ADMIN_TENANT).unwrap().operator
        );
        assert!(snap.admitted(DEFAULT_TENANT) && snap.admitted(ADMIN_TENANT));
        let again = TenantStore::new(s.db.clone()).unwrap();
        assert_eq!(again.list().unwrap().len(), 2);
    }

    #[test]
    fn create_resolve_admit_delete() {
        let (s, _d) = store();
        let t = s.create("acme", "Acme".into(), false).unwrap();
        assert_eq!(s.resolve("acme").unwrap(), t);
        assert!(matches!(
            s.create("acme", String::new(), false),
            Err(TenantError::AlreadyExists(_))
        ));
        assert!(matches!(
            s.create("Acme", String::new(), false),
            Err(TenantError::Invalid(_))
        ));
        assert!(!s.snapshot().unwrap().admitted(&t.id));
        s.admit(&t.id, unlimited(), AdmissionStatus::Admitted, "test")
            .unwrap();
        assert!(s.snapshot().unwrap().admitted(&t.id));
        s.admit(&t.id, unlimited(), AdmissionStatus::Suspended, "test")
            .unwrap();
        assert!(!s.snapshot().unwrap().admitted(&t.id));
        s.add_member(&t.id, Ent::User("u1".into()), "test").unwrap();
        assert_eq!(s.members(&t.id).unwrap().len(), 1);
        assert!(s.delete(DEFAULT_TENANT).is_err());
        s.delete(&t.id).unwrap();
        assert!(s.admission(&t.id).unwrap().is_none() && s.members(&t.id).unwrap().is_empty());
    }

    #[test]
    fn tenant_pools_are_replaced_as_a_set_and_go_with_the_tenant() {
        let (s, _d) = store();
        let t = s.create("acme", String::new(), false).unwrap();
        let other = s.create("beta", String::new(), false).unwrap();
        let pool = |tenant: &str, name: &str| TenantPool {
            tenant_id: tenant.into(),
            name: name.into(),
            from: "a100".into(),
            max: 2,
            projects: vec![],
            by: "test".into(),
            at: 0,
        };
        s.set_device_pools(&t.id, vec![pool(&t.id, "x"), pool(&t.id, "y")])
            .unwrap();
        s.set_device_pools(&other.id, vec![pool(&other.id, "x")])
            .unwrap();
        s.set_device_pools(&t.id, vec![pool(&t.id, "y")]).unwrap();
        let names = |id: &str| -> Vec<String> {
            s.device_pools(id)
                .unwrap()
                .into_iter()
                .map(|p| p.name)
                .collect()
        };
        assert_eq!(names(&t.id), ["y"]);
        assert_eq!(names(&other.id), ["x"], "another tenant's stay");
        assert!(s.device_pool(&t.id, "x").unwrap().is_none());
        s.delete(&t.id).unwrap();
        assert!(names(&t.id).is_empty());
    }
}
