//! VPC routers (spec/clustering.md §11.2a): project-owned logical routers
//! on OVN. The record is the source of truth; the leader's network
//! controller makes the `gxr-<id>` objects from it.

use crate::ipam::{self, IpamError};
use crate::store::{Db, Origin, StoreError, TableId};
use glidex_ovs::net::Ipv4Net;
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterSpec {
    /// `[a-z0-9-]`, at most 32, unique per project.
    pub name: String,
    /// Has a gateway: SNAT to the outside.
    #[serde(default = "yes")]
    pub external: bool,
    /// Asked for explicitly (a site resource); otherwise the next free pool address.
    #[serde(default)]
    pub external_ip: Option<Ipv4Addr>,
    /// Chassis nodes (ids or names); default `ovn.edge.gateway_nodes`.
    #[serde(default)]
    pub gateway_nodes: Option<Vec<String>>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterStatus {
    /// The address actually reserved (`ipam_external`).
    pub external_ip: Option<Ipv4Addr>,
    /// Conntrack zone of the router's SNAT (§13.2).
    pub snat_ct_zone: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Router {
    /// Short id; the OVN router is `gxr-<id>`.
    pub id: String,
    pub project: String,
    pub spec: RouterSpec,
    pub status: RouterStatus,
    pub created_at: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("router not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("external_pool_exhausted: the external address pool has no free address")]
    PoolExhausted,
    #[error("no free conntrack zone is left in the configured range")]
    NoZone,
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<StoreError> for RouterError {
    fn from(e: StoreError) -> Self {
        RouterError::Storage(e.to_string())
    }
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(impl From<$t> for RouterError { fn from(e: $t) -> Self { RouterError::Storage(e.to_string()) } })*};
}
storage_from!(redb::TableError, redb::StorageError, redb::TransactionError, serde_json::Error);

/// Names are DNS-label-like, at most 32 characters.
pub fn validate_name(name: &str) -> Result<(), RouterError> {
    if name.is_empty() || name.len() > 32 || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') || name.starts_with('-') || name.ends_with('-') {
        return Err(RouterError::Invalid("a router name is 1-32 characters of a-z, 0-9 and '-'".into()));
    }
    Ok(())
}

/// What the admission write needs from the site configuration.
pub struct Site<'a> {
    pub pool: Option<Ipv4Net>,
    /// Addresses the pool must not hand out (the edge's, the gateway's).
    pub exclude: &'a [Ipv4Addr],
    /// Zones available to routers (the edge keeps the first of the range).
    pub zones: (u16, u16),
}

pub fn list(db: &Db) -> Result<Vec<Router>, RouterError> {
    let txn = db.begin_read()?;
    let t = txn.open_table(TableId::Routers.definition())?;
    let mut out = Vec::new();
    for r in t.iter()? {
        out.push(serde_json::from_slice(r?.1.value())?);
    }
    Ok(out)
}

pub fn get(db: &Db, id: &str) -> Result<Option<Router>, RouterError> {
    let txn = db.begin_read()?;
    let t = txn.open_table(TableId::Routers.definition())?;
    Ok(t.get(id)?.map(|v| serde_json::from_slice(v.value())).transpose()?)
}

pub fn find(db: &Db, project: &str, name: &str) -> Result<Option<Router>, RouterError> {
    Ok(list(db)?.into_iter().find(|r| r.project == project && r.spec.name == name))
}

/// Create a router: name uniqueness, the external address and the
/// conntrack zone in one write, so a pool that runs out leaves nothing behind.
pub fn create(db: &Db, project: &str, spec: RouterSpec, site: &Site<'_>) -> Result<Router, RouterError> {
    validate_name(&spec.name)?;
    if !spec.external && (spec.external_ip.is_some() || spec.gateway_nodes.is_some()) {
        return Err(RouterError::Invalid("a router without a gateway has no external_ip or gateway_nodes".into()));
    }
    let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    db.write(Origin::Api, |tx| -> Result<Router, RouterError> {
        let mut t = tx.open_table(TableId::Routers.definition())?;
        let mut zones_used = std::collections::BTreeSet::new();
        for r in t.iter()? {
            let r: Router = serde_json::from_slice(r?.1.value())?;
            if r.project == project && r.spec.name == spec.name {
                return Err(RouterError::Conflict(format!("router '{}' already exists", spec.name)));
            }
            zones_used.extend(r.status.snat_ct_zone);
        }
        let (mut external_ip, mut zone) = (None, None);
        if spec.external {
            let pool = site.pool.ok_or_else(|| RouterError::Invalid("external routers need cluster.ovn.edge.external_pool".into()))?;
            external_ip = Some(ipam::allocate_external(tx, &id, spec.external_ip, pool, site.exclude).map_err(|e| match e {
                IpamError::ExternalPoolFull => RouterError::PoolExhausted,
                IpamError::ExternalTaken(a) => RouterError::Conflict(format!("external address {a} is taken")),
                IpamError::Invalid(m) => RouterError::Invalid(m),
                e => RouterError::Storage(e.to_string()),
            })?);
            zone = Some((site.zones.0.saturating_add(1)..=site.zones.1).find(|z| !zones_used.contains(z)).ok_or(RouterError::NoZone)?);
        }
        let r = Router { id: id.clone(), project: project.into(), spec: spec.clone(), status: RouterStatus { external_ip, snat_ct_zone: zone }, created_at: crate::tenancy::now() };
        t.insert(id.as_str(), serde_json::to_vec(&r)?.as_slice())?;
        Ok(r)
    })
}

pub fn delete(db: &Db, id: &str) -> Result<(), RouterError> {
    db.write(Origin::Api, |tx| -> Result<(), RouterError> {
        tx.open_table(TableId::Routers.definition())?.remove(id)?;
        ipam::free_external(tx, id).map_err(|e| RouterError::Storage(e.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(pool: &str) -> (Ipv4Net, [Ipv4Addr; 1]) {
        (pool.parse().unwrap(), ["192.0.2.241".parse().unwrap()])
    }

    fn spec(name: &str) -> RouterSpec {
        RouterSpec { name: name.into(), external: true, external_ip: None, gateway_nodes: None }
    }

    #[test]
    fn a_router_gets_its_own_address_and_zone_and_gives_them_back() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::create(d.path().join("t.db")).unwrap();
        let (pool, ex) = site("192.0.2.240/29");
        let s = Site { pool: Some(pool), exclude: &ex, zones: (60000, 60010) };
        let a = create(&db, "p1", spec("web"), &s).unwrap();
        let b = create(&db, "p1", spec("db"), &s).unwrap();
        assert_ne!(a.status.external_ip, b.status.external_ip);
        assert_eq!((a.status.snat_ct_zone, b.status.snat_ct_zone), (Some(60001), Some(60002)), "the edge keeps the first zone");
        assert!(matches!(create(&db, "p1", spec("web"), &s), Err(RouterError::Conflict(_))));
        // The same name in another project is a different router.
        create(&db, "p2", spec("web"), &s).unwrap();
        assert_eq!(find(&db, "p1", "db").unwrap().unwrap().id, b.id);
        delete(&db, &a.id).unwrap();
        let again = create(&db, "p1", spec("web2"), &s).unwrap();
        assert_eq!(again.status.external_ip, a.status.external_ip);
        assert_eq!(again.status.snat_ct_zone, a.status.snat_ct_zone);
    }

    #[test]
    fn a_full_pool_leaves_nothing_behind() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::create(d.path().join("t.db")).unwrap();
        let (pool, ex) = site("192.0.2.240/30");
        let s = Site { pool: Some(pool), exclude: &ex, zones: (60000, 60010) };
        create(&db, "p", spec("a"), &s).unwrap();
        assert!(matches!(create(&db, "p", spec("b"), &s), Err(RouterError::PoolExhausted)));
        assert_eq!(list(&db).unwrap().len(), 1);
    }

    #[test]
    fn internal_routers_take_nothing_from_the_pool_and_names_are_checked() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::create(d.path().join("t.db")).unwrap();
        let s = Site { pool: None, exclude: &[], zones: (60000, 60010) };
        let r = create(&db, "p", RouterSpec { external: false, ..spec("inner") }, &s).unwrap();
        assert_eq!((r.status.external_ip, r.status.snat_ct_zone), (None, None));
        assert!(matches!(create(&db, "p", spec("web"), &s), Err(RouterError::Invalid(_))), "an external router needs a pool");
        assert!(create(&db, "p", RouterSpec { external: false, external_ip: Some("192.0.2.9".parse().unwrap()), ..spec("x") }, &s).is_err());
        for bad in ["", "-a", "a-", "A", "a_b", &"x".repeat(33)] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
    }
}
