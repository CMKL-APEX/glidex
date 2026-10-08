//! Cluster IPAM (spec/clustering.md §11.4, D12): subnets and per-NIC address
//! reservations for OVN networks, in the cluster store. OVN's own dynamic
//! addressing isn't used: the control plane is the one source of truth, so
//! an address survives restarts and, later, live migration, and port
//! security can pin it.
//!
//! All allocation happens on the leader, under the VM write lock, so two
//! allocations never race.

use crate::store::{Db, StoreError, TableId, Tx};
use glidex_ovs::net::Ipv4Net;
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subnet {
    pub network: String,
    pub cidr: String,
    pub gateway: Ipv4Addr,
    /// First and last assignable address (`.2` to the last host).
    pub pool: (Ipv4Addr, Ipv4Addr),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub network: String,
    pub mac: String,
    pub ip: Ipv4Addr,
    pub vm_id: String,
    pub nic: u8,
}

#[derive(Debug, thiserror::Error)]
pub enum IpamError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("no free /24 left in {0}")]
    SupernetFull(String),
    #[error("subnet {0} overlaps network {1}")]
    Overlap(String, String),
    #[error("network {0} has no free address")]
    PoolFull(String),
    #[error("network {0} has no subnet")]
    NoSubnet(String),
    #[error("{0}")]
    Invalid(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(impl From<$t> for IpamError { fn from(e: $t) -> Self { IpamError::Storage(e.to_string()) } })*};
}
storage_from!(redb::TableError, redb::StorageError, redb::TransactionError, serde_json::Error);

fn parse_net(s: &str) -> Result<Ipv4Net, IpamError> {
    s.parse().map_err(|e| IpamError::Invalid(format!("{s}: {e}")))
}

fn subnets(tx: &Tx<'_>) -> Result<Vec<Subnet>, IpamError> {
    let t = tx.open_table(TableId::IpamSubnets.definition())?;
    let mut out = Vec::new();
    for r in t.iter()? {
        out.push(serde_json::from_slice(r?.1.value())?);
    }
    Ok(out)
}

/// Give `network` a subnet: the requested one, or the first free `/24` of
/// `supernet`. `reserved` are ranges that must not be used (the node NAT
/// supernet, so node and cluster networks never overlap). Idempotent for a
/// network that has one.
pub fn allocate_subnet(tx: &Tx<'_>, network: &str, requested: Option<Ipv4Net>, supernet: Ipv4Net, reserved: &[Ipv4Net]) -> Result<Subnet, IpamError> {
    let existing = subnets(tx)?;
    if let Some(s) = existing.iter().find(|s| s.network == network) {
        return Ok(s.clone());
    }
    let taken = |c: &Ipv4Net| -> Option<String> {
        if let Some(r) = reserved.iter().find(|r| r.overlaps(c)) {
            return Some(format!("the node NAT range {}/{}", r.network(), r.prefix()));
        }
        existing.iter().find(|s| parse_net(&s.cidr).map(|n| n.overlaps(c)).unwrap_or(false)).map(|s| s.network.clone())
    };
    let cidr = match requested {
        Some(c) => {
            if c.prefix() > 29 || c.prefix() < 16 {
                return Err(IpamError::Invalid("a network's subnet is between /16 and /29".into()));
            }
            if let Some(who) = taken(&c) {
                return Err(IpamError::Overlap(format!("{}/{}", c.network(), c.prefix()), who));
            }
            c
        }
        None => {
            let mut found = None;
            let mut i = 0u32;
            while let Some(base) = supernet.host(i * 256) {
                if !supernet.contains(base) {
                    break;
                }
                let c = Ipv4Net::new(base, 24).map_err(|e| IpamError::Invalid(e.to_string()))?;
                if taken(&c).is_none() {
                    found = Some(c);
                    break;
                }
                i += 1;
            }
            found.ok_or_else(|| IpamError::SupernetFull(format!("{}/{}", supernet.network(), supernet.prefix())))?
        }
    };
    let gateway = cidr.host(1).ok_or_else(|| IpamError::Invalid("subnet too small".into()))?;
    let first = cidr.host(2).ok_or_else(|| IpamError::Invalid("subnet too small".into()))?;
    let last = cidr.last_host().ok_or_else(|| IpamError::Invalid("subnet too small".into()))?;
    let s = Subnet { network: network.into(), cidr: format!("{}/{}", cidr.network(), cidr.prefix()), gateway, pool: (first, last) };
    tx.open_table(TableId::IpamSubnets.definition())?.insert(network, serde_json::to_vec(&s)?.as_slice())?;
    Ok(s)
}

pub fn free_subnet(tx: &Tx<'_>, network: &str) -> Result<(), IpamError> {
    tx.open_table(TableId::IpamSubnets.definition())?.remove(network)?;
    let mut res = tx.open_table(TableId::IpamReservations.definition())?;
    let prefix = format!("{network}/");
    let keys: Vec<String> = res.range(prefix.as_str()..format!("{prefix}~").as_str())?.filter_map(|r| r.ok()).map(|(k, _)| k.value().to_string()).collect();
    for k in keys {
        res.remove(k.as_str())?;
    }
    Ok(())
}

fn key(network: &str, mac: &str) -> String {
    format!("{network}/{}", mac.to_ascii_lowercase())
}

/// An address for `(network, mac)`: the one it already holds, else the lowest
/// free address of the pool.
pub fn reserve(tx: &Tx<'_>, network: &str, mac: &str, vm_id: &str, nic: u8) -> Result<Reservation, IpamError> {
    let sub: Subnet = {
        let t = tx.open_table(TableId::IpamSubnets.definition())?;
        let v = t.get(network)?.ok_or_else(|| IpamError::NoSubnet(network.into()))?;
        serde_json::from_slice(v.value())?
    };
    let mut res = tx.open_table(TableId::IpamReservations.definition())?;
    let k = key(network, mac);
    if let Some(v) = res.get(k.as_str())? {
        let r: Reservation = serde_json::from_slice(v.value())?;
        return Ok(r);
    }
    let prefix = format!("{network}/");
    let mut used = std::collections::BTreeSet::new();
    for r in res.range(prefix.as_str()..format!("{prefix}~").as_str())? {
        let r: Reservation = serde_json::from_slice(r?.1.value())?;
        used.insert(u32::from(r.ip));
    }
    let (lo, hi) = (u32::from(sub.pool.0), u32::from(sub.pool.1));
    let ip = (lo..=hi).find(|a| !used.contains(a)).map(Ipv4Addr::from).ok_or_else(|| IpamError::PoolFull(network.into()))?;
    let r = Reservation { network: network.into(), mac: mac.to_ascii_lowercase(), ip, vm_id: vm_id.into(), nic };
    res.insert(k.as_str(), serde_json::to_vec(&r)?.as_slice())?;
    Ok(r)
}

pub fn release(tx: &Tx<'_>, network: &str, mac: &str) -> Result<(), IpamError> {
    tx.open_table(TableId::IpamReservations.definition())?.remove(key(network, mac).as_str())?;
    Ok(())
}

pub fn list_subnets(db: &Db) -> Result<Vec<Subnet>, IpamError> {
    let txn = db.begin_read()?;
    let t = txn.open_table(TableId::IpamSubnets.definition())?;
    let mut out = Vec::new();
    for r in t.iter()? {
        out.push(serde_json::from_slice(r?.1.value())?);
    }
    Ok(out)
}

pub fn list_reservations(db: &Db) -> Result<Vec<Reservation>, IpamError> {
    let txn = db.begin_read()?;
    let t = txn.open_table(TableId::IpamReservations.definition())?;
    let mut out = Vec::new();
    for r in t.iter()? {
        out.push(serde_json::from_slice(r?.1.value())?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Origin;

    fn db() -> (tempfile::TempDir, Db) {
        let d = tempfile::tempdir().unwrap();
        let db = Db::create(d.path().join("t.db")).unwrap();
        (d, db)
    }

    fn sup() -> Ipv4Net {
        "10.89.0.0/16".parse().unwrap()
    }

    fn node_nat() -> Vec<Ipv4Net> {
        vec!["10.88.0.0/16".parse().unwrap()]
    }

    #[test]
    fn subnets_come_from_the_supernet_and_never_overlap() {
        let (_d, db) = db();
        let w = |name: &str, req: Option<&str>| db.write(Origin::Network, |tx| allocate_subnet(tx, name, req.map(|r| r.parse().unwrap()), sup(), &node_nat()));
        let a = w("a", None).unwrap();
        assert_eq!((a.cidr.as_str(), a.gateway, a.pool.0, a.pool.1), ("10.89.0.0/24", "10.89.0.1".parse().unwrap(), "10.89.0.2".parse().unwrap(), "10.89.0.254".parse().unwrap()));
        assert_eq!(w("b", None).unwrap().cidr, "10.89.1.0/24");
        // Idempotent for a network that has one.
        assert_eq!(w("a", None).unwrap().cidr, "10.89.0.0/24");
        // A requested subnet that overlaps another, or the node NAT range, is refused.
        assert!(matches!(w("c", Some("10.89.1.128/25")), Err(IpamError::Overlap(..))));
        assert!(matches!(w("c", Some("10.88.4.0/24")), Err(IpamError::Overlap(..))));
        assert_eq!(w("c", Some("192.168.50.0/24")).unwrap().gateway, "192.168.50.1".parse::<Ipv4Addr>().unwrap());
        // Freeing makes room again.
        db.write(Origin::Network, |tx| free_subnet(tx, "a")).unwrap();
        assert_eq!(w("d", None).unwrap().cidr, "10.89.0.0/24");
    }

    #[test]
    fn addresses_are_the_lowest_free_and_stable() {
        let (_d, db) = db();
        db.write(Origin::Network, |tx| allocate_subnet(tx, "n", Some("10.89.9.0/29".parse().unwrap()), sup(), &[])).unwrap();
        let r = |mac: &str| db.write(Origin::Api, |tx| reserve(tx, "n", mac, "vm", 0));
        let a = r("52:54:00:00:00:01").unwrap();
        assert_eq!(a.ip, "10.89.9.2".parse::<Ipv4Addr>().unwrap());
        assert_eq!(r("52:54:00:00:00:01").unwrap().ip, a.ip, "the same NIC keeps its address");
        assert_eq!(r("52:54:00:00:00:02").unwrap().ip, "10.89.9.3".parse::<Ipv4Addr>().unwrap());
        // A /29 has .2-.6 assignable.
        for i in 3..=5 {
            r(&format!("52:54:00:00:00:1{i}")).unwrap();
        }
        assert!(matches!(r("52:54:00:00:00:99"), Err(IpamError::PoolFull(_))));
        db.write(Origin::Api, |tx| release(tx, "n", "52:54:00:00:00:01")).unwrap();
        assert_eq!(r("52:54:00:00:00:99").unwrap().ip, a.ip, "a freed address is reused");
        assert!(matches!(db.write(Origin::Api, |tx| reserve(tx, "none", "52:54:00:00:00:01", "vm", 0)), Err(IpamError::NoSubnet(_))));
    }
}
