//! netd's desired-state database (spec §7.4): ReDB tables with JSON values.

use glidex_ovs::OvsError;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;

pub const BRIDGES: TableDefinition<&str, &[u8]> = TableDefinition::new("bridges");
pub const NAT: TableDefinition<&str, &[u8]> = TableDefinition::new("nat");
pub const VM_PORTS: TableDefinition<&str, &[u8]> = TableDefinition::new("vm_ports");
pub const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
pub const UPLINKS: TableDefinition<&str, &[u8]> = TableDefinition::new("uplinks");
/// Devices prepared for passthrough, by IOMMU group.
pub const DEVICES: TableDefinition<&str, &[u8]> = TableDefinition::new("devices");
/// ifname → its source-route table (spec §8.6.3).
pub const SOURCE_TABLES: TableDefinition<&str, &[u8]> = TableDefinition::new("source_tables");
/// ifname → the ARP settings it had before netd set them (spec §8.6.6).
pub const ARP_SAVED: TableDefinition<&str, &[u8]> = TableDefinition::new("arp_saved");

pub struct Store {
    db: Database,
}

fn err(e: impl std::fmt::Display) -> OvsError {
    OvsError::Io(format!("state database: {}", e))
}

/// Makes sure the database file exists whole: a new one is built beside it
/// (`<path>.new`), renamed into place and the directory synced. redb writes
/// a new file's header before its magic number and refuses a file without
/// one ever after, so a kill while creating it in place would leave a
/// database netd never opens (#317). A leftover `.new` is half made and
/// holds nothing.
fn create_whole(path: &Path) -> std::io::Result<()> {
    let mut name = path.as_os_str().to_owned();
    name.push(".new");
    let new = std::path::PathBuf::from(name);
    match std::fs::remove_file(&new) {
        Ok(()) => tracing::warn!("removed the half-made state database {}", new.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if path.exists() {
        return Ok(());
    }
    drop(Database::create(&new).map_err(std::io::Error::other)?);
    std::fs::rename(&new, path)?;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    std::fs::File::open(dir)?.sync_all()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, OvsError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
        create_whole(path).map_err(err)?;
        let db = Database::create(path).map_err(err)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(err)?;
        }
        let txn = db.begin_write().map_err(err)?;
        for t in [
            BRIDGES,
            NAT,
            VM_PORTS,
            META,
            UPLINKS,
            DEVICES,
            SOURCE_TABLES,
            ARP_SAVED,
        ] {
            txn.open_table(t).map_err(err)?;
        }
        txn.commit().map_err(err)?;
        Ok(Self { db })
    }

    pub fn put<T: Serialize>(
        &self,
        table: TableDefinition<&str, &[u8]>,
        key: &str,
        value: &T,
    ) -> Result<(), OvsError> {
        let bytes = serde_json::to_vec(value).map_err(err)?;
        let txn = self.db.begin_write().map_err(err)?;
        {
            let mut t = txn.open_table(table).map_err(err)?;
            t.insert(key, bytes.as_slice()).map_err(err)?;
        }
        txn.commit().map_err(err)
    }

    pub fn get<T: DeserializeOwned>(
        &self,
        table: TableDefinition<&str, &[u8]>,
        key: &str,
    ) -> Result<Option<T>, OvsError> {
        let txn = self.db.begin_read().map_err(err)?;
        let t = txn.open_table(table).map_err(err)?;
        match t.get(key).map_err(err)? {
            Some(v) => serde_json::from_slice(v.value()).map(Some).map_err(err),
            None => Ok(None),
        }
    }

    pub fn list<T: DeserializeOwned>(
        &self,
        table: TableDefinition<&str, &[u8]>,
    ) -> Result<Vec<(String, T)>, OvsError> {
        let txn = self.db.begin_read().map_err(err)?;
        let t = txn.open_table(table).map_err(err)?;
        let mut out = Vec::new();
        for entry in t.iter().map_err(err)? {
            let (k, v) = entry.map_err(err)?;
            out.push((
                k.value().to_string(),
                serde_json::from_slice(v.value()).map_err(err)?,
            ));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_half_made_database_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("netd.db");
        // What a kill leaves while redb creates the file (#317).
        let new = dir.path().join("netd.db.new");
        std::fs::write(&new, vec![0u8; 1 << 20]).unwrap();
        let store = Store::open(&path).unwrap();
        assert!(!new.exists(), "the half-made file stays");
        drop(store);
        Store::open(&path).unwrap();
    }
}
