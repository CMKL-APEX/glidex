use crate::cloud_init;
use crate::hypervisor::{create_backend, Hypervisor, HypervisorError, HypervisorProcess, HypervisorType};
use crate::models::{Vm, VmConfig, VmState};
use crate::persistence::{PersistenceError, VmStore};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug)]
pub enum VmManagerError {
    VmNotFound(String),
    VmAlreadyExists(String),
    InvalidState { current: VmState, operation: String },
    HypervisorError(HypervisorError),
    PersistenceError(String),
    HypervisorNotAvailable(HypervisorType),
}

impl std::fmt::Display for VmManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmManagerError::VmNotFound(id) => write!(f, "VM not found: {}", id),
            VmManagerError::VmAlreadyExists(name) => write!(f, "VM already exists: {}", name),
            VmManagerError::InvalidState { current, operation } => {
                write!(f, "Invalid state {:?} for operation: {}", current, operation)
            }
            VmManagerError::HypervisorError(e) => write!(f, "Hypervisor error: {}", e),
            VmManagerError::PersistenceError(e) => write!(f, "Persistence error: {}", e),
            VmManagerError::HypervisorNotAvailable(h) => {
                write!(f, "Hypervisor not available: {:?}", h)
            }
        }
    }
}

impl From<HypervisorError> for VmManagerError {
    fn from(e: HypervisorError) -> Self {
        VmManagerError::HypervisorError(e)
    }
}

impl From<PersistenceError> for VmManagerError {
    fn from(e: PersistenceError) -> Self {
        VmManagerError::PersistenceError(e.to_string())
    }
}

struct VmEntry {
    vm: Vm,
    process: Option<Box<dyn HypervisorProcess>>,
}

pub struct VmManager {
    vms: RwLock<HashMap<String, VmEntry>>,
    store: VmStore,
    backends: HashMap<HypervisorType, Box<dyn Hypervisor>>,
}

impl VmManager {
    /// Create a new VmManager with persistence at the default location
    pub fn new() -> Result<Arc<Self>, VmManagerError> {
        Self::with_db_path(Self::default_db_path())
    }

    /// Create a new VmManager with persistence at a custom path
    pub fn with_db_path(db_path: PathBuf) -> Result<Arc<Self>, VmManagerError> {
        let store = VmStore::open(&db_path)?;

        // Initialize hypervisor backends and probe whether each binary is on PATH.
        let mut backends: HashMap<HypervisorType, Box<dyn Hypervisor>> = HashMap::new();
        for ty in [
            HypervisorType::CloudHypervisor,
            HypervisorType::Qemu,
        ] {
            let backend = create_backend(ty);
            if !backend.is_available() {
                tracing::warn!(
                    "Hypervisor {} binary {:?} not found on PATH — VMs configured for it will fail to start",
                    backend.hypervisor_type(),
                    ty.binary_name()
                );
            }
            backends.insert(ty, backend);
        }

        Ok(Arc::new(Self {
            vms: RwLock::new(HashMap::new()),
            store,
            backends,
        }))
    }

    /// Get the backend for a hypervisor type
    fn get_backend(&self, hypervisor: HypervisorType) -> Result<&dyn Hypervisor, VmManagerError> {
        self.backends
            .get(&hypervisor)
            .map(|b| b.as_ref())
            .ok_or(VmManagerError::HypervisorNotAvailable(hypervisor))
    }

    /// Get the default database path (~/.glidex/glidex.db)
    fn default_db_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".glidex")
            .join("glidex.db")
    }

    /// Initialize VmManager by loading persisted VMs and reconciling state
    pub async fn initialize(&self) -> Result<(), VmManagerError> {
        let persisted_vms = self.store.load_all()?;
        let mut vms = self.vms.write().await;

        for mut vm in persisted_vms {
            // Reconcile state: VMs that were Running/Paused are now orphaned
            let reconciled_state = self.reconcile_vm_state(&vm);

            if vm.state != reconciled_state {
                vm.state = reconciled_state;
                // Update DB with reconciled state
                self.store.save(&vm)?;
            }

            vms.insert(
                vm.id.clone(),
                VmEntry {
                    vm,
                    process: None, // Process handles cannot be restored
                },
            );
        }

        Ok(())
    }

    /// Reconcile VM state after restart
    fn reconcile_vm_state(&self, vm: &Vm) -> VmState {
        match vm.state {
            VmState::Running | VmState::Paused => {
                // Check if the hypervisor process is still alive
                if self.is_hypervisor_alive(&vm.socket_path) {
                    // Process exists but we lost the handle - clean up and mark as stopped
                    self.cleanup_orphaned_vm(vm);
                }
                VmState::Stopped
            }
            VmState::Created | VmState::Stopped => vm.state.clone(),
        }
    }

    /// Check if a hypervisor process is still alive by probing its socket
    fn is_hypervisor_alive(&self, socket_path: &str) -> bool {
        std::path::Path::new(socket_path).exists()
    }

    /// Clean up resources from an orphaned VM
    fn cleanup_orphaned_vm(&self, vm: &Vm) {
        // Remove socket files
        let _ = std::fs::remove_file(&vm.socket_path);
        let _ = std::fs::remove_file(&vm.console_socket_path);

        tracing::warn!(
            "Cleaned up orphaned VM resources for {} ({})",
            vm.name,
            vm.id
        );
    }

    pub async fn create_vm(&self, name: String, config: VmConfig) -> Result<Vm, VmManagerError> {
        // Reject obviously broken configurations before persisting them.
        if config.vcpu_count == 0 {
            return Err(HypervisorError::InvalidConfig(
                "vcpu_count must be greater than 0".to_string(),
            )
            .into());
        }
        if config.mem_size_mib == 0 {
            return Err(HypervisorError::InvalidConfig(
                "mem_size_mib must be greater than 0".to_string(),
            )
            .into());
        }
        match (&config.firmware_path, config.hypervisor) {
            (Some(_), HypervisorType::CloudHypervisor) => {}
            (Some(_), other) => {
                return Err(HypervisorError::InvalidConfig(format!(
                    "firmware_path is only supported by cloudhypervisor, not {}",
                    other
                ))
                .into());
            }
            (None, _) if config.kernel_image_path.is_empty() => {
                return Err(HypervisorError::InvalidConfig(
                    "either kernel_image_path or firmware_path is required".to_string(),
                )
                .into());
            }
            (None, _) => {}
        }
        if config.cloud_init_path.is_some()
            && config.hypervisor != HypervisorType::CloudHypervisor
        {
            return Err(HypervisorError::InvalidConfig(
                "cloud_init_path is only supported by cloudhypervisor".to_string(),
            )
            .into());
        }

        let mut vms = self.vms.write().await;

        // Check if VM with same name exists
        if vms.values().any(|entry| entry.vm.name == name) {
            return Err(VmManagerError::VmAlreadyExists(name));
        }

        let vm = Vm::new(name, config);

        // Persist to database BEFORE adding to in-memory cache
        self.store.save(&vm)?;

        let vm_clone = vm.clone();

        vms.insert(
            vm.id.clone(),
            VmEntry {
                vm,
                process: None,
            },
        );

        Ok(vm_clone)
    }

    pub async fn start_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        let mut vms = self.vms.write().await;

        let entry = vms
            .get_mut(vm_id)
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;

        match entry.vm.state {
            VmState::Created | VmState::Stopped => {
                // Get the appropriate backend for this VM's hypervisor
                let backend = self.get_backend(entry.vm.hypervisor)?;

                // Spawn hypervisor process with console socket and log file
                let process = backend.spawn(
                    &entry.vm.socket_path,
                    &entry.vm.console_socket_path,
                    &entry.vm.log_path,
                )?;

                // Firmware-booted cloud images need a cloud-init seed to
                // get a usable login; generate the default one if the VM
                // didn't bring its own. Regenerated on every start so it
                // tracks the host's current SSH keys.
                let mut config = entry.vm.config.clone();
                if config.firmware_path.is_some() && config.cloud_init_path.is_none() {
                    let path = entry.vm.default_cloud_init_path();
                    let seed = cloud_init::SeedConfig::for_vm(&entry.vm.id, &entry.vm.name);
                    if seed.ssh_authorized_keys.is_empty() && seed.passwd_hash.is_none() {
                        tracing::warn!(
                            vm_id = %entry.vm.id,
                            "cloud-init seed has no SSH keys and {} is unset; guest user '{}' will have no way to log in",
                            cloud_init::PASSWD_HASH_ENV,
                            cloud_init::DEFAULT_USER
                        );
                    }
                    if let Err(e) = cloud_init::write_seed_image(&path, &seed) {
                        let _ = process.kill();
                        return Err(e.into());
                    }
                    config.cloud_init_path = Some(path);
                }

                // Configure the VM, cleanup process on failure
                if let Err(e) = process.configure(&config) {
                    let _ = process.kill();
                    return Err(e.into());
                }

                // Start the VM, cleanup process on failure
                if let Err(e) = process.start() {
                    let _ = process.kill();
                    return Err(e.into());
                }

                // Persist state change BEFORE updating in-memory state
                // If persist fails, kill the process to maintain consistency
                if let Err(e) = self.store.update_state(vm_id, VmState::Running) {
                    let _ = process.kill();
                    return Err(e.into());
                }

                tracing::info!(
                    vm_id = %entry.vm.id,
                    running = process.is_running(),
                    socket = process.socket_path(),
                    console = process.console_socket_path(),
                    log = process.log_path(),
                    "VM started"
                );

                entry.process = Some(process);
                entry.vm.state = VmState::Running;

                Ok(entry.vm.clone())
            }
            VmState::Paused => {
                // Resume paused VM
                if let Some(ref process) = entry.process {
                    process.resume()?;
                } else {
                    return Err(VmManagerError::InvalidState {
                        current: VmState::Paused,
                        operation: "start (no process handle)".to_string(),
                    });
                }

                // Persist state change BEFORE updating in-memory state
                // If persist fails, pause again to maintain consistency
                if let Err(e) = self.store.update_state(vm_id, VmState::Running) {
                    if let Some(ref process) = entry.process {
                        let _ = process.pause();
                    }
                    return Err(e.into());
                }

                entry.vm.state = VmState::Running;

                Ok(entry.vm.clone())
            }
            VmState::Running => Err(VmManagerError::InvalidState {
                current: VmState::Running,
                operation: "start".to_string(),
            }),
        }
    }

    pub async fn stop_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        let mut vms = self.vms.write().await;

        let entry = vms
            .get_mut(vm_id)
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;

        match entry.vm.state {
            VmState::Running | VmState::Paused => {
                // Kill the hypervisor process (cannot be undone)
                if let Some(ref process) = entry.process {
                    let _ = process.kill();
                }
                entry.process = None;
                entry.vm.state = VmState::Stopped;

                // Persist state change - log warning if fails since operation already happened
                if let Err(e) = self.store.update_state(vm_id, VmState::Stopped) {
                    tracing::error!(
                        "Failed to persist VM {} state change to Stopped: {}. State will be reconciled on restart.",
                        vm_id, e
                    );
                }

                Ok(entry.vm.clone())
            }
            _ => Err(VmManagerError::InvalidState {
                current: entry.vm.state.clone(),
                operation: "stop".to_string(),
            }),
        }
    }

    pub async fn pause_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        let mut vms = self.vms.write().await;

        let entry = vms
            .get_mut(vm_id)
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;

        if entry.vm.state != VmState::Running {
            return Err(VmManagerError::InvalidState {
                current: entry.vm.state.clone(),
                operation: "pause".to_string(),
            });
        }

        if let Some(ref process) = entry.process {
            process.pause()?;
        } else {
            return Err(VmManagerError::InvalidState {
                current: entry.vm.state.clone(),
                operation: "pause (no process handle)".to_string(),
            });
        }

        // Persist state change BEFORE updating in-memory state
        // If persist fails, resume the VM to maintain consistency
        if let Err(e) = self.store.update_state(vm_id, VmState::Paused) {
            if let Some(ref process) = entry.process {
                let _ = process.resume();
            }
            return Err(e.into());
        }

        entry.vm.state = VmState::Paused;

        Ok(entry.vm.clone())
    }

    pub async fn get_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        let vms = self.vms.read().await;
        vms.get(vm_id)
            .map(|entry| entry.vm.clone())
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))
    }

    pub async fn list_vms(&self) -> Vec<Vm> {
        let vms = self.vms.read().await;
        vms.values().map(|entry| entry.vm.clone()).collect()
    }

    pub async fn attach_device(&self, vm_id: &str, device_path: String) -> Result<Vm, VmManagerError> {
        let mut vms = self.vms.write().await;

        let entry = vms
            .get_mut(vm_id)
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;

        // Reject if device is already attached
        if entry.vm.config.vfio_devices.contains(&device_path) {
            return Err(VmManagerError::InvalidState {
                current: entry.vm.state.clone(),
                operation: format!("attach_device: {} is already attached", device_path),
            });
        }

        match entry.vm.state {
            VmState::Running => {
                // Hot-plug: call hypervisor API, then update config
                if let Some(ref process) = entry.process {
                    process.add_device(&device_path)?;
                } else {
                    return Err(VmManagerError::InvalidState {
                        current: entry.vm.state.clone(),
                        operation: "attach_device (no process handle)".to_string(),
                    });
                }

                entry.vm.config.vfio_devices.push(device_path);

                // Persist updated config. On failure, rollback the hot-plug.
                if let Err(e) = self.store.save(&entry.vm) {
                    let removed = entry.vm.config.vfio_devices.pop();
                    if let (Some(ref process), Some(path)) = (&entry.process, removed) {
                        let _ = process.remove_device(&path);
                    }
                    return Err(e.into());
                }

                Ok(entry.vm.clone())
            }
            VmState::Created | VmState::Stopped => {
                // Config-only: device will be included at next VM start
                entry.vm.config.vfio_devices.push(device_path);
                self.store.save(&entry.vm)?;
                Ok(entry.vm.clone())
            }
            VmState::Paused => Err(VmManagerError::InvalidState {
                current: VmState::Paused,
                operation: "attach_device".to_string(),
            }),
        }
    }

    pub async fn detach_device(&self, vm_id: &str, device_path: &str) -> Result<Vm, VmManagerError> {
        let mut vms = self.vms.write().await;

        let entry = vms
            .get_mut(vm_id)
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;

        // Check that the device is actually attached
        let pos = entry
            .vm
            .config
            .vfio_devices
            .iter()
            .position(|d| d == device_path)
            .ok_or_else(|| VmManagerError::InvalidState {
                current: entry.vm.state.clone(),
                operation: format!("detach_device: {} is not attached", device_path),
            })?;

        match entry.vm.state {
            VmState::Running => {
                // Hot-unplug: call hypervisor API, then update config
                if let Some(ref process) = entry.process {
                    process.remove_device(device_path)?;
                } else {
                    return Err(VmManagerError::InvalidState {
                        current: entry.vm.state.clone(),
                        operation: "detach_device (no process handle)".to_string(),
                    });
                }

                let removed = entry.vm.config.vfio_devices.remove(pos);

                // Persist updated config. On failure, rollback.
                if let Err(e) = self.store.save(&entry.vm) {
                    // Re-insert at original position
                    entry.vm.config.vfio_devices.insert(pos, removed.clone());
                    if let Some(ref process) = entry.process {
                        let _ = process.add_device(&removed);
                    }
                    return Err(e.into());
                }

                Ok(entry.vm.clone())
            }
            VmState::Created | VmState::Stopped => {
                // Config-only: just remove from the device list
                entry.vm.config.vfio_devices.remove(pos);
                self.store.save(&entry.vm)?;
                Ok(entry.vm.clone())
            }
            VmState::Paused => Err(VmManagerError::InvalidState {
                current: VmState::Paused,
                operation: "detach_device".to_string(),
            }),
        }
    }

    pub async fn delete_vm(&self, vm_id: &str) -> Result<(), VmManagerError> {
        let mut vms = self.vms.write().await;

        let entry = vms
            .get_mut(vm_id)
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;

        // Stop the VM if running
        if let Some(ref process) = entry.process {
            let _ = process.kill();
        }

        // Delete from database BEFORE removing from memory
        self.store.delete(vm_id)?;
        let _ = std::fs::remove_file(entry.vm.default_cloud_init_path());

        vms.remove(vm_id);
        Ok(())
    }

    /// Shutdown all running VMs. Called during control-plane termination.
    pub async fn shutdown(&self) {
        let mut vms = self.vms.write().await;
        let mut stopped_count = 0;

        for (vm_id, entry) in vms.iter_mut() {
            if let Some(ref process) = entry.process {
                tracing::info!("Stopping VM {} ({})...", entry.vm.name, vm_id);
                let _ = process.kill();
                stopped_count += 1;

                // Update state in DB - log warning if fails
                if let Err(e) = self.store.update_state(vm_id, VmState::Stopped) {
                    tracing::warn!(
                        "Failed to persist VM {} state change to Stopped: {}",
                        vm_id,
                        e
                    );
                }
            }
            entry.process = None;
            entry.vm.state = VmState::Stopped;
        }

        if stopped_count > 0 {
            tracing::info!("Stopped {} running VM(s)", stopped_count);
        }
    }
}
