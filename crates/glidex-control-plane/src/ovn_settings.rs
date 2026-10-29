//! OVN as a cluster setting (spec/16-clustering-ui.md §8a): whether the cluster
//! runs its networks on OVN, and the cluster-wide OVN options, in the
//! replicated `Meta` table, so every node follows one answer. What is per
//! host (`bridge_mappings`) stays in each host's file.

use crate::cluster::config::{EdgeConfig, OvnConfig};
use crate::store::{Db, Origin, StoreError, TableId};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

const KEY: &str = "ovn_settings";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OvnSettings {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underlay_mtu: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nat_supernet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<EdgeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snat_ct_zones: Option<(u16, u16)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns_servers: Vec<IpAddr>,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: u64,
}

impl OvnSettings {
    /// The cluster-wide part of a host's file.
    pub fn from_file(f: &OvnConfig) -> OvnSettings {
        OvnSettings {
            enabled: f.enabled,
            underlay_mtu: f.underlay_mtu,
            nat_supernet: f.nat_supernet.clone(),
            edge: f.edge.clone(),
            snat_ct_zones: f.snat_ct_zones,
            dns_servers: f.dns_servers.clone(),
            updated_by: "file".into(),
            updated_at: 0,
        }
    }

    /// What this host runs with: these settings, and the file's per-host part.
    pub fn effective(&self, file: &OvnConfig) -> OvnConfig {
        OvnConfig {
            enabled: self.enabled,
            underlay_mtu: self.underlay_mtu,
            nat_supernet: self.nat_supernet.clone(),
            edge: self.edge.clone(),
            snat_ct_zones: self.snat_ct_zones,
            dns_servers: self.dns_servers.clone(),
            bridge_mappings: file.bridge_mappings.clone(),
        }
    }

    /// The same options, ignoring who wrote them when.
    pub fn same_options(&self, other: &OvnSettings) -> bool {
        let strip = |s: &OvnSettings| OvnSettings {
            updated_by: String::new(),
            updated_at: 0,
            ..s.clone()
        };
        strip(self) == strip(other)
    }

    /// Check the options (not the nodes): addresses parse and fit together.
    /// `node_known` says whether a gateway node (id or name) exists.
    pub fn validate(&self, node_known: impl Fn(&str) -> bool) -> Result<(), String> {
        use glidex_ovs::net::Ipv4Net;
        if let Some(m) = self.underlay_mtu
            && !(1280..=9216).contains(&m)
        {
            return Err(format!("underlay_mtu {m} is outside 1280–9216"));
        }
        if let Some(s) = &self.nat_supernet {
            s.parse::<Ipv4Net>()
                .map_err(|_| format!("nat_supernet '{s}' is not an IPv4 network"))?;
        }
        if let Some((a, b)) = self.snat_ct_zones
            && (a == 0 || a >= b)
        {
            return Err(format!(
                "snat_ct_zones [{a}, {b}] must be a range of zones above 0"
            ));
        }
        if let Some(e) = &self.edge {
            if e.physnet.trim().is_empty() {
                return Err("edge.physnet is required".into());
            }
            let cidr: Ipv4Net = e.external_cidr.parse().map_err(|_| {
                format!(
                    "edge.external_cidr '{}' is not an IPv4 network",
                    e.external_cidr
                )
            })?;
            let inside = |ip: &IpAddr| matches!(ip, IpAddr::V4(v4) if cidr.contains(*v4));
            if !inside(&e.gateway) {
                return Err(format!(
                    "edge.gateway {} is not in {}",
                    e.gateway, e.external_cidr
                ));
            }
            if !inside(&e.external_ip) {
                return Err(format!(
                    "edge.external_ip {} is not in {}",
                    e.external_ip, e.external_cidr
                ));
            }
            if e.external_ip == e.gateway {
                return Err("edge.external_ip and edge.gateway are the same address".into());
            }
            if let Some(p) = &e.external_pool {
                let pool: Ipv4Net = p
                    .parse()
                    .map_err(|_| format!("edge.external_pool '{p}' is not an IPv4 network"))?;
                if !cidr.contains(pool.network()) || pool.prefix() < cidr.prefix() {
                    return Err(format!(
                        "edge.external_pool {p} is not in {}",
                        e.external_cidr
                    ));
                }
                if let IpAddr::V4(x) = e.external_ip
                    && pool.contains(x)
                {
                    return Err(format!(
                        "edge.external_pool {p} holds the edge's own address {x}"
                    ));
                }
            }
            if e.gateway_nodes.is_empty() {
                return Err("edge.gateway_nodes needs at least one node".into());
            }
            if let Some(n) = e.gateway_nodes.iter().find(|n| !node_known(n)) {
                return Err(format!("edge.gateway_nodes: no node {n}"));
            }
        }
        Ok(())
    }
}

pub fn read(db: &Db) -> Option<OvnSettings> {
    let txn = db.begin_read().ok()?;
    let t = txn.open_table(TableId::Meta.definition()).ok()?;
    serde_json::from_slice(t.get(KEY).ok()??.value()).ok()
}

pub fn write(db: &Db, s: &OvnSettings) -> Result<(), StoreError> {
    let v =
        serde_json::to_vec(s).map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;
    db.write(Origin::Api, |tx| -> Result<(), StoreError> {
        tx.open_table(TableId::Meta.definition())?
            .insert(KEY, v.as_slice())?;
        Ok(())
    })
}

/// Whether a write touched the setting (`Meta`'s key).
pub fn is_key(table: TableId, key: &[u8]) -> bool {
    table == TableId::Meta && key == KEY.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge() -> EdgeConfig {
        EdgeConfig {
            physnet: "uplink".into(),
            external_cidr: "192.0.2.0/24".into(),
            gateway: "192.0.2.1".parse().unwrap(),
            external_ip: "192.0.2.50".parse().unwrap(),
            external_pool: Some("192.0.2.64/26".into()),
            gateway_nodes: vec!["h1".into()],
        }
    }

    #[test]
    fn the_edge_must_fit_together() {
        let known = |n: &str| n == "h1";
        let mut s = OvnSettings {
            enabled: true,
            edge: Some(edge()),
            ..Default::default()
        };
        assert_eq!(s.validate(known), Ok(()));
        let mut e = edge();
        e.gateway = "198.51.100.1".parse().unwrap();
        s.edge = Some(e);
        assert!(s.validate(known).unwrap_err().contains("gateway"));
        let mut e = edge();
        e.external_pool = Some("192.0.2.32/27".into());
        e.external_ip = "192.0.2.40".parse().unwrap();
        s.edge = Some(e);
        assert!(s.validate(known).unwrap_err().contains("own address"));
        let mut e = edge();
        e.gateway_nodes = vec!["h9".into()];
        s.edge = Some(e);
        assert!(s.validate(known).unwrap_err().contains("h9"));
    }

    #[test]
    fn the_file_keeps_its_per_host_part() {
        let mut file = OvnConfig {
            enabled: false,
            ..Default::default()
        };
        file.bridge_mappings
            .insert("uplink".into(), "gxbr-up".into());
        let s = OvnSettings {
            enabled: true,
            dns_servers: vec!["192.0.2.53".parse().unwrap()],
            ..Default::default()
        };
        let eff = s.effective(&file);
        assert!(eff.enabled);
        assert_eq!(eff.dns_servers, s.dns_servers);
        assert_eq!(
            eff.bridge_mappings.get("uplink").map(String::as_str),
            Some("gxbr-up")
        );
        assert!(s.same_options(&OvnSettings {
            updated_by: "x".into(),
            updated_at: 5,
            ..s.clone()
        }));
    }
}
