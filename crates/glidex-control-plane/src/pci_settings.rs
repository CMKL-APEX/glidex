//! Device pools and grants as a cluster setting (spec/22-host-topology.md
//! H20, §9.1): `pci.pools` and `pci.allow` in the replicated `Meta` table,
//! so every server resolves a request the same way. What is per host
//! (`exclude`, `overrides`) stays in each host's file.

use crate::config::{PciConfig, PciGrant, PciPool};
use crate::store::{Db, Origin, StoreError, TableId};
use serde::{Deserialize, Serialize};

const KEY: &str = "pci_settings";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PciSettings {
    #[serde(default)]
    pub pools: Vec<PciPool>,
    #[serde(default)]
    pub allow: Vec<PciGrant>,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: u64,
}

impl PciSettings {
    /// The cluster-wide part of a host's file.
    pub fn from_file(f: &PciConfig, by: &str, at: u64) -> PciSettings {
        PciSettings {
            pools: f.pools.clone(),
            allow: f.allow.clone(),
            updated_by: by.into(),
            updated_at: at,
        }
    }

    /// The pools and grants, in the shape the rest of the code reads.
    pub fn config(&self) -> PciConfig {
        PciConfig {
            pools: self.pools.clone(),
            allow: self.allow.clone(),
            ..Default::default()
        }
    }

    /// Whether a file says the same, ignoring who wrote the setting when.
    pub fn same_as(&self, f: &PciConfig) -> bool {
        self.pools == f.pools && self.allow == f.allow
    }
}

pub fn read(db: &Db) -> Option<PciSettings> {
    let txn = db.begin_read().ok()?;
    let t = txn.open_table(TableId::Meta.definition()).ok()?;
    serde_json::from_slice(t.get(KEY).ok()??.value()).ok()
}

pub fn write(db: &Db, s: &PciSettings) -> Result<(), StoreError> {
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
    use crate::config::PoolMatch;

    fn file() -> PciConfig {
        PciConfig {
            pools: vec![PciPool {
                name: "r9700".into(),
                class: Some("gpu".into()),
                matches: PoolMatch {
                    vendor_id: Some("0x1002".into()),
                    device_id: Some("0x7551".into()),
                    ..Default::default()
                },
                nodes: vec![],
                prepare: false,
                per_device: None,
                bar_mib: Some(16384),
            }],
            allow: vec![PciGrant {
                bdf: None,
                pool: Some("r9700".into()),
                projects: vec![],
                tenants: vec!["*".into()],
                node: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_setting_round_trips_and_compares_with_a_file() {
        let s = PciSettings::from_file(&file(), "migrate:h1", 7);
        let back: PciSettings = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert!(s.same_as(&file()));
        assert_eq!(s.config().pools, file().pools);
        let mut other = file();
        other.pools[0].bar_mib = Some(8192);
        assert!(!s.same_as(&other));
    }
}
