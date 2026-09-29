use crate::models::{Vm, VmState};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;

const VMS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("vms");

#[derive(Error, Debug)]
pub enum PersistenceError {
    #[error("Database error: {0}")]
    Database(#[from] redb::DatabaseError),

    #[error("Transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),

    #[error("Table error: {0}")]
    Table(#[from] redb::TableError),

    #[error("Storage error: {0}")]
    Storage(#[from] redb::StorageError),

    #[error("Commit error: {0}")]
    Commit(#[from] redb::CommitError),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("VM not found: {0}")]
    VmNotFound(String),
}

pub struct VmStore {
    db: Arc<Database>,
}

impl VmStore {
    /// Open or create the database at the specified path
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        // Ensure parent directory exists
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }

        let db = Database::create(path.as_ref())?;
        // The database also holds credential hashes (credentials.rs), so
        // keep it readable by the control-plane user only.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path.as_ref(), std::fs::Permissions::from_mode(0o600))?;
        }

        // Initialize table on first run
        let write_txn = db.begin_write()?;
        {
            let _ = write_txn.open_table(VMS_TABLE)?;
        }
        write_txn.commit()?;

        Ok(Self { db: Arc::new(db) })
    }

    /// Shared handle to the underlying database, for other tables
    /// (e.g. the credential store) living in the same file.
    pub fn database(&self) -> Arc<Database> {
        self.db.clone()
    }

    /// Load all VMs from the database
    pub fn load_all(&self) -> Result<Vec<Vm>, PersistenceError> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(VMS_TABLE)?;

        let mut vms = Vec::new();
        for result in table.iter()? {
            let (key, value): (redb::AccessGuard<'_, &str>, redb::AccessGuard<'_, &[u8]>) =
                result?;
            // Skip records this build can't decode (e.g. VMs for a removed
            // hypervisor such as Firecracker) rather than refusing to start.
            // They stay in the database untouched.
            match serde_json::from_slice::<Vm>(value.value()) {
                Ok(vm) => vms.push(vm),
                Err(e) => tracing::warn!(
                    vm_id = key.value(),
                    "Skipping unreadable VM record: {}",
                    e
                ),
            }
        }

        Ok(vms)
    }

    /// Save or update a VM
    pub fn save(&self, vm: &Vm) -> Result<(), PersistenceError> {
        let serialized = serde_json::to_vec(vm)?;

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(VMS_TABLE)?;
            table.insert(vm.id.as_str(), serialized.as_slice())?;
        }
        write_txn.commit()?;

        Ok(())
    }

    /// Delete a VM by ID
    pub fn delete(&self, vm_id: &str) -> Result<(), PersistenceError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(VMS_TABLE)?;
            table.remove(vm_id)?;
        }
        write_txn.commit()?;

        Ok(())
    }

    /// Update only the state of a VM (optimized for frequent state changes)
    pub fn update_state(&self, vm_id: &str, new_state: VmState) -> Result<(), PersistenceError> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table(VMS_TABLE)?;

            // Read existing VM data first, then update
            let serialized = {
                let existing = table
                    .get(vm_id)?
                    .ok_or_else(|| PersistenceError::VmNotFound(vm_id.to_string()))?;
                let mut vm: Vm = serde_json::from_slice(existing.value())?;
                vm.state = new_state;
                serde_json::to_vec(&vm)?
            };

            table.insert(vm_id, serialized.as_slice())?;
        }
        write_txn.commit()?;

        Ok(())
    }
}
