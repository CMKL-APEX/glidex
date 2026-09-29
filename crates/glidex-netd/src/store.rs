//! netd's desired-state database (spec §7.4): ReDB tables with JSON values.

use glidex_ovs::OvsError;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;

pub const BRIDGES: TableDefinition<&str, &[u8]> = TableDefinition::new("bridges");
pub const NAT: TableDefinition<&str, &[u8]> = TableDefinition::new("nat");
pub const VM_PORTS: TableDefinition<&str, &[u8]> = TableDefinition::new("vm_ports");
pub const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
pub const UPLINKS: TableDefinition<&str, &[u8]> = TableDefinition::new("uplinks");

pub struct Store {
    db: Database,
}

fn err(e: impl std::fmt::Display) -> OvsError {
    OvsError::Io(format!("state database: {}", e))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, OvsError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
        let db = Database::create(path).map_err(err)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(err)?;
        }
        let txn = db.begin_write().map_err(err)?;
        for t in [BRIDGES, NAT, VM_PORTS, META, UPLINKS] {
            txn.open_table(t).map_err(err)?;
        }
        txn.commit().map_err(err)?;
        Ok(Self { db })
    }

    pub fn put<T: Serialize>(&self, table: TableDefinition<&str, &[u8]>, key: &str, value: &T) -> Result<(), OvsError> {
        let bytes = serde_json::to_vec(value).map_err(err)?;
        let txn = self.db.begin_write().map_err(err)?;
        {
            let mut t = txn.open_table(table).map_err(err)?;
            t.insert(key, bytes.as_slice()).map_err(err)?;
        }
        txn.commit().map_err(err)
    }

    pub fn get<T: DeserializeOwned>(&self, table: TableDefinition<&str, &[u8]>, key: &str) -> Result<Option<T>, OvsError> {
        let txn = self.db.begin_read().map_err(err)?;
        let t = txn.open_table(table).map_err(err)?;
        match t.get(key).map_err(err)? {
            Some(v) => serde_json::from_slice(v.value()).map(Some).map_err(err),
            None => Ok(None),
        }
    }

    pub fn list<T: DeserializeOwned>(&self, table: TableDefinition<&str, &[u8]>) -> Result<Vec<(String, T)>, OvsError> {
        let txn = self.db.begin_read().map_err(err)?;
        let t = txn.open_table(table).map_err(err)?;
        let mut out = Vec::new();
        for entry in t.iter().map_err(err)? {
            let (k, v) = entry.map_err(err)?;
            out.push((k.value().to_string(), serde_json::from_slice(v.value()).map_err(err)?));
        }
        Ok(out)
    }

    pub fn delete(&self, table: TableDefinition<&str, &[u8]>, key: &str) -> Result<(), OvsError> {
        let txn = self.db.begin_write().map_err(err)?;
        {
            let mut t = txn.open_table(table).map_err(err)?;
            t.remove(key).map_err(err)?;
        }
        txn.commit().map_err(err)
    }
}

pub fn uplink_key(bridge: &str, name: &str) -> String {
    format!("{}/{}", bridge, name)
}

pub fn vm_port_key(vm_id: &str, nic: u8) -> String {
    format!("{}/{}", vm_id, nic)
}
