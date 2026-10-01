use crate::cloud_init;
use crate::credentials::{
    Credential, CredentialError, CredentialStore, CreateCredentialRequest, UpdateCredentialRequest,
};
use crate::hypervisor::{create_backend, Hypervisor, HypervisorError, HypervisorProcess, HypervisorType};
use crate::images::{
    self, disk::requested_size, CatalogItem, CreateDiskRequest, Disk, DiskResponse, ExtendMode,
    ImageError, ImageManager, ImageResponse, ImageSettings, PullImageRequest, ResizeDiskRequest,
};
use crate::models::{DiskBinding, DiskSelection, NicBinding, NicState, Vm, VmConfig, VmState};
use crate::network::{self, CreateNetworkRequest, NetError, Netd, Network, NetworkMode, NetworkStore};
use glidex_netd::proto::{AttachResult, BridgeRecord, NatInfo, Op, ReconcileReport};
use glidex_ovs::bridge::{BridgeSpec, Datapath};
use glidex_ovs::nat::NatSpec;
use glidex_ovs::vm_port::{VmPortKind, VmPortSpec};
use crate::persistence::{Commit, PersistenceError, VmStore};
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
    Credential(CredentialError),
    CredentialInUse { username: String, vms: Vec<String> },
    Network(NetError),
    Image(ImageError),
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
            VmManagerError::Credential(e) => write!(f, "{}", e),
            VmManagerError::Network(e) => write!(f, "{}", e),
            VmManagerError::Image(e) => write!(f, "{}", e),
            VmManagerError::CredentialInUse { username, vms } => write!(
                f,
                "credential {} is used by VM(s): {}",
                username,
                vms.join(", ")
            ),
        }
    }
}

impl From<HypervisorError> for VmManagerError {
    fn from(e: HypervisorError) -> Self {
        VmManagerError::HypervisorError(e)
    }
}

impl From<NetError> for VmManagerError {
    fn from(e: NetError) -> Self {
        VmManagerError::Network(e)
    }
}

impl From<ImageError> for VmManagerError {
    fn from(e: ImageError) -> Self {
        VmManagerError::Image(e)
    }
}

impl From<CredentialError> for VmManagerError {
    fn from(e: CredentialError) -> Self {
        VmManagerError::Credential(e)
    }
}

impl From<PersistenceError> for VmManagerError {
    fn from(e: PersistenceError) -> Self {
        match e {
            PersistenceError::Disk(e) => VmManagerError::Image(e),
            e => VmManagerError::PersistenceError(e.to_string()),
        }
    }
}

struct VmEntry {
    vm: Vm,
    process: Option<Box<dyn HypervisorProcess>>,
}

pub struct VmManager {
    vms: RwLock<HashMap<String, VmEntry>>,
    store: VmStore,
    credentials: CredentialStore,
    networks: NetworkStore,
    netd: Netd,
    images: Arc<ImageManager>,
    backends: HashMap<HypervisorType, Box<dyn Hypervisor>>,
}

type RootBinding = (DiskBinding, Disk);

/// A disk name derived from a VM name (`<vm>-root`), made valid.
fn root_disk_name(vm_name: &str) -> String {
    let base: String = vm_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
        .collect();
    let base = base.trim_start_matches('.');
    let base = if base.is_empty() { "vm" } else { base };
    let mut name = format!("{}-root", base);
    name.truncate(64);
    name
}

impl VmManager {
    /// Create a new VmManager with persistence at the default location
    pub fn new() -> Result<Arc<Self>, VmManagerError> {
        Self::with_db_path(Self::default_db_path())
    }

    /// Create a new VmManager with persistence at a custom path
    pub fn with_db_path(db_path: PathBuf) -> Result<Arc<Self>, VmManagerError> {
        Self::with_db_path_and_netd(db_path, Netd::from_env())
    }

    /// As `with_db_path`, talking to the glidex-netd at `netd`.
    pub fn with_db_path_and_netd(db_path: PathBuf, netd: Netd) -> Result<Arc<Self>, VmManagerError> {
        let store = VmStore::open(&db_path)?;
        let base = db_path.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let images = ImageManager::new(store.database(), ImageSettings::from_env(&base))?;

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
            credentials: CredentialStore::new(store.database())?,
            networks: NetworkStore::new(store.database())?,
            netd,
            images,
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
        self.images.initialize();
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

        // Every VM is stopped after a control-plane restart, so any VM
        // ports netd still has are stale.
        if vms.values().any(|e| !e.vm.config.networks.is_empty()) {
            match self.netd.call::<ReconcileReport>(Op::SyncVms { running: Vec::new() }) {
                Ok(report) if !report.detached.is_empty() || !report.orphans.is_empty() => {
                    tracing::info!(detached = ?report.detached, orphans = ?report.orphans, "netd sync");
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("glidex-netd sync skipped: {}", e),
            }
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

    #[allow(dead_code)] // used by the tests through the lib crate
    pub async fn create_vm(&self, name: String, config: VmConfig) -> Result<Vm, VmManagerError> {
        self.create_vm_with_disks(name, config, DiskSelection::default())
            .await
            .map(|(vm, _)| vm)
    }

    /// Create a VM, resolving managed disks (spec images.md §7). Returns
    /// the VM and any warnings.
    pub async fn create_vm_with_disks(
        &self,
        name: String,
        mut config: VmConfig,
        sel: DiskSelection,
    ) -> Result<(Vm, Vec<String>), VmManagerError> {
        let roots = [!config.rootfs_path.is_empty(), sel.image.is_some(), sel.root_disk.is_some()];
        if roots.iter().filter(|b| **b).count() != 1 {
            return Err(HypervisorError::InvalidConfig(
                "give exactly one of rootfs_path, image or root_disk".to_string(),
            )
            .into());
        }
        if sel.root_disk_size_gib.is_some() && sel.image.is_none() {
            return Err(HypervisorError::InvalidConfig("root_disk_size_gib needs image".to_string()).into());
        }
        // A managed root disk is a cloud image: firmware boot unless the
        // caller brought a kernel.
        if (sel.image.is_some() || sel.root_disk.is_some())
            && config.kernel_image_path.is_empty()
            && config.firmware_path.is_none()
        {
            config.firmware_path = crate::hypervisor::cloud_hypervisor::default_firmware_path()
                .map(|p| p.to_string_lossy().into_owned());
        }
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
        self.refuse_dpdk_uplink_devices(&config.vfio_devices)?;
        if !config.networks.is_empty() {
            if config.hypervisor != HypervisorType::CloudHypervisor {
                return Err(HypervisorError::InvalidConfig(
                    "networks are only supported by cloudhypervisor".to_string(),
                )
                .into());
            }
            if config.networks.len() > glidex_ovs::names::MAX_NICS as usize {
                return Err(HypervisorError::InvalidConfig(format!(
                    "at most {} networks per VM",
                    glidex_ovs::names::MAX_NICS
                ))
                .into());
            }
            for att in &config.networks {
                if self.networks.get(&att.network)?.is_none() {
                    return Err(HypervisorError::InvalidConfig(format!(
                        "network not found: {}",
                        att.network
                    ))
                    .into());
                }
                if let Some(mac) = &att.mac {
                    glidex_ovs::names::validate_mac(mac)
                        .map_err(|e| HypervisorError::InvalidConfig(e.to_string()))?;
                }
                if let Some(q) = att.queue_pairs {
                    if !(1..=8).contains(&q) {
                        return Err(HypervisorError::InvalidConfig(
                            "queue_pairs must be 1-8".to_string(),
                        )
                        .into());
                    }
                }
            }
        }
        let mut vms = self.vms.write().await;

        // Checked under the VM write lock so delete_credential (which holds
        // the read lock) can't remove it between the check and the insert.
        if let Some(username) = &config.credential {
            // A credential only takes effect through the generated seed.
            if config.firmware_path.is_none() || config.cloud_init_path.is_some() {
                return Err(HypervisorError::InvalidConfig(
                    "credential requires firmware boot without a custom cloud_init_path"
                        .to_string(),
                )
                .into());
            }
            match self.credentials.get(username) {
                Ok(_) => {}
                Err(CredentialError::NotFound(_)) => {
                    return Err(HypervisorError::InvalidConfig(format!(
                        "credential not found: {}",
                        username
                    ))
                    .into());
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Check if VM with same name exists
        if vms.values().any(|entry| entry.vm.name == name) {
            return Err(VmManagerError::VmAlreadyExists(name));
        }

        let mut vm = Vm::new(name, config);
        // Stable MACs derived from the VM id, so they show up in the API
        // and survive restarts.
        for (i, att) in vm.config.networks.iter_mut().enumerate() {
            if att.mac.is_none() {
                att.mac = glidex_ovs::names::mac_address(&vm.id, i as u8).ok();
            }
        }

        // Managed disks. Checked under the VM write lock, so no other VM
        // can claim the same disk in between.
        let mut disks: Vec<Disk> = Vec::new();
        let mut warnings = Vec::new();
        for key in &sel.data_disks {
            let d = self.attachable_disk(key)?;
            if disks.iter().any(|x| x.id == d.id) {
                return Err(HypervisorError::InvalidConfig(format!("disk {} listed twice", d.name)).into());
            }
            vm.config.data_disks.push(d.id.clone());
            disks.push(d);
        }
        if let Some(key) = &sel.root_disk {
            let d = self.attachable_disk(key)?;
            if disks.iter().any(|x| x.id == d.id) {
                return Err(HypervisorError::InvalidConfig(format!(
                    "disk {} is both root_disk and a data disk",
                    d.name
                ))
                .into());
            }
            vm.config.root_disk = Some(d.id.clone());
            vm.config.rootfs_path = self.images.disk_path(&d).to_string_lossy().into_owned();
            disks.push(d);
        }
        let mut created: Option<Disk> = None;
        let mut image_hold = None;
        if let Some(image) = &sel.image {
            let mut disk_name = root_disk_name(&vm.name);
            if self.images.disk_name_taken(&disk_name) {
                disk_name = format!("{}-{}", disk_name.trim_end_matches("-root"), &vm.id[..8]);
            }
            let req = CreateDiskRequest {
                name: disk_name,
                size_gib: sel.root_disk_size_gib,
                image: Some(image.clone()),
                ..Default::default()
            };
            let mgr = self.images.clone();
            let (d, mut w, hold) = tokio::task::spawn_blocking(move || mgr.create_disk_file(&req))
                .await
                .map_err(|e| ImageError::Io(e.to_string()))??;
            image_hold = hold;
            warnings.append(&mut w);
            vm.config.root_disk = Some(d.id.clone());
            vm.config.owns_root_disk = true;
            vm.config.rootfs_path = self.images.disk_path(&d).to_string_lossy().into_owned();
            created = Some(d.clone());
            disks.push(d);
        }
        for d in &mut disks {
            d.attached_to = Some(vm.id.clone());
            if Some(&d.id) == vm.config.root_disk.as_ref() && d.pending_growpart && vm.config.cloud_init_path.is_some() {
                warnings.push(format!(
                    "disk {} is waiting for an on-boot root partition grow, but this VM uses a custom cloud_init_path; enable growpart in that seed",
                    d.name
                ));
            }
        }

        // Persist BEFORE adding to the in-memory cache; the VM and its
        // disks' attached_to go in one transaction.
        let commit = Commit { put_vm: Some(&vm), put_disks: disks.iter().collect(), ..Default::default() };
        if let Err(e) = self.store.commit(commit) {
            if let Some(d) = &created {
                self.images.remove_disk_file(d);
            }
            return Err(e.into());
        }
        for d in &disks {
            self.images.cache_disk(d);
        }
        drop(image_hold);

        let vm_clone = vm.clone();

        vms.insert(
            vm.id.clone(),
            VmEntry {
                vm,
                process: None,
            },
        );

        Ok((vm_clone, warnings))
    }

    /// A disk a new VM may attach: exists, not attached, not busy.
    fn attachable_disk(&self, key: &str) -> Result<Disk, VmManagerError> {
        let d = self.images.get_disk(key)?;
        if let Some(vm) = &d.attached_to {
            return Err(ImageError::InUse(format!("disk {} is attached to VM {}", d.name, vm)).into());
        }
        if let Some(op) = self.images.busy_op(&d.id) {
            return Err(ImageError::Busy(format!("disk {} is busy ({})", d.name, op)).into());
        }
        Ok(d)
    }

    /// Bindings for a VM's managed disks, checked to be usable right now:
    /// the root disk (with its record) and the data disks.
    fn disk_bindings(&self, vm: &Vm) -> Result<(Option<RootBinding>, Vec<DiskBinding>), VmManagerError> {
        let bind = |id: &str| -> Result<(DiskBinding, Disk), VmManagerError> {
            let d = self.images.get_disk(id)?;
            if let Some(op) = self.images.busy_op(&d.id) {
                return Err(ImageError::Busy(format!("disk {} is busy ({})", d.name, op)).into());
            }
            let path = self.images.disk_path(&d);
            if !path.exists() {
                return Err(ImageError::Io(format!("disk {} file is missing: {}", d.name, path.display())).into());
            }
            Ok((
                DiskBinding {
                    path: path.to_string_lossy().into_owned(),
                    format: d.format,
                    backing_files: d.is_linked(),
                },
                d,
            ))
        };
        let root = vm.config.root_disk.as_deref().map(bind).transpose()?;
        let data = vm
            .config
            .data_disks
            .iter()
            .map(|id| bind(id).map(|(b, _)| b))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((root, data))
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
                let (root_disk, data_disks) = self.disk_bindings(&entry.vm)?;

                // Spawn hypervisor process with console socket and log file
                let process = backend.spawn(
                    &entry.vm.socket_path,
                    &entry.vm.console_socket_path,
                    &entry.vm.log_path,
                )?;

                // Attach NICs through glidex-netd before configuring, so
                // the hypervisor gets real tap names / vhost-user sockets.
                let mut config = entry.vm.config.clone();
                config.root_disk_binding = root_disk.as_ref().map(|(b, _)| b.clone());
                config.data_disk_bindings = data_disks;
                let nics = match self.attach_nics(&entry.vm) {
                    Ok(nics) => nics,
                    Err(e) => {
                        let _ = process.kill();
                        return Err(e);
                    }
                };
                config.nic_bindings = nics.iter().map(|(b, _)| b.clone()).collect();
                let (cleanup_id, cleanup_nics) = (entry.vm.id.clone(), config.networks.len());
                let detach_on_error = |process: &dyn HypervisorProcess| {
                    let _ = process.kill();
                    self.detach_nics(&cleanup_id, cleanup_nics);
                };

                // Firmware-booted cloud images need a cloud-init seed to
                // get a usable login; generate the default one if the VM
                // didn't bring its own. Regenerated on every start so it
                // tracks the host's current SSH keys.
                if config.firmware_path.is_some() && config.cloud_init_path.is_none() {
                    let path = entry.vm.default_cloud_init_path();
                    let seed = match &config.credential {
                        Some(username) => match self.credentials.get(username) {
                            Ok(cred) => cloud_init::SeedConfig::for_credential(
                                &entry.vm.id,
                                &entry.vm.name,
                                &cred,
                            ),
                            Err(e) => {
                                detach_on_error(process.as_ref());
                                return Err(e.into());
                            }
                        },
                        None => cloud_init::SeedConfig::for_vm(&entry.vm.id, &entry.vm.name),
                    };
                    let mut seed = seed;
                    seed.nic_macs = config.nic_bindings.iter().map(|n| n.mac.clone()).collect();
                    seed.growpart = root_disk.as_ref().is_some_and(|(_, d)| d.pending_growpart);
                    if seed.ssh_authorized_keys.is_empty() && seed.passwd_hash.is_none() {
                        tracing::warn!(
                            vm_id = %entry.vm.id,
                            "cloud-init seed has no SSH keys and {} is unset; guest user '{}' will have no way to log in",
                            cloud_init::PASSWD_HASH_ENV,
                            cloud_init::DEFAULT_USER
                        );
                    }
                    if let Err(e) = cloud_init::write_seed_image(&path, &seed) {
                        detach_on_error(process.as_ref());
                        return Err(e.into());
                    }
                    config.cloud_init_path = Some(path);
                }

                // Configure the VM, cleanup process on failure
                if let Err(e) = process.configure(&config) {
                    detach_on_error(process.as_ref());
                    return Err(e.into());
                }

                // Start the VM, cleanup process on failure
                if let Err(e) = process.start() {
                    detach_on_error(process.as_ref());
                    return Err(e.into());
                }

                // Persist state change BEFORE updating in-memory state
                // If persist fails, kill the process to maintain consistency
                entry.vm.nics = nics.into_iter().map(|(_, s)| s).collect();
                entry.vm.state = VmState::Running;
                if let Err(e) = self.store.save(&entry.vm) {
                    entry.vm.state = VmState::Stopped;
                    detach_on_error(process.as_ref());
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

                // The seed now carries the growpart request; cloud-init
                // acts on it during this boot.
                if let Some((_, mut d)) = root_disk.filter(|(_, d)| d.pending_growpart) {
                    if config.cloud_init_path.as_deref() == Some(entry.vm.default_cloud_init_path().as_str()) {
                        d.pending_growpart = false;
                        if let Err(e) = self.images.put_disk(&d) {
                            tracing::warn!(disk = %d.name, "could not clear pending_growpart: {}", e);
                        }
                    }
                }

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
                self.detach_nics(&entry.vm.id, entry.vm.config.networks.len());
                for nic in &mut entry.vm.nics {
                    nic.port = None;
                }

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
        self.refuse_dpdk_uplink_devices(std::slice::from_ref(&device_path))?;
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

    #[allow(dead_code)] // used by the tests through the lib crate
    pub async fn delete_vm(&self, vm_id: &str) -> Result<(), VmManagerError> {
        self.delete_vm_with(vm_id, false).await
    }

    /// Delete a VM. Its owned root disk goes with it unless `keep_disk`;
    /// other disks are detached and kept (spec images.md §7).
    pub async fn delete_vm_with(&self, vm_id: &str, keep_disk: bool) -> Result<(), VmManagerError> {
        let mut vms = self.vms.write().await;

        let entry = vms
            .get_mut(vm_id)
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;

        let mut detach = Vec::new();
        let mut owned = None;
        for id in entry.vm.config.root_disk.iter().chain(entry.vm.config.data_disks.iter()) {
            let Ok(mut d) = self.images.get_disk(id) else { continue };
            if let Some(op) = self.images.busy_op(&d.id) {
                return Err(ImageError::Busy(format!("disk {} is busy ({})", d.name, op)).into());
            }
            d.attached_to = None;
            if entry.vm.config.owns_root_disk && !keep_disk && Some(id) == entry.vm.config.root_disk.as_ref() {
                owned = Some(d);
            } else {
                detach.push(d);
            }
        }

        // Stop the VM if running
        if let Some(ref process) = entry.process {
            let _ = process.kill();
        }
        if !entry.vm.config.networks.is_empty() {
            // Detaches any ports and frees the VM's NAT reservations.
            if let Err(e) = self.netd.call::<serde_json::Value>(Op::ReleaseVm { vm_id: entry.vm.id.clone() }) {
                tracing::warn!(vm_id = %entry.vm.id, "glidex-netd release failed: {}", e);
            }
        }

        // Delete from database BEFORE removing from memory; the disks'
        // records change in the same transaction.
        self.store.commit(Commit {
            delete_vm: Some(vm_id),
            put_disks: detach.iter().collect(),
            delete_disks: owned.iter().map(|d| d.id.as_str()).collect(),
            ..Default::default()
        })?;
        for d in &detach {
            self.images.cache_disk(d);
        }
        if let Some(d) = &owned {
            self.images.uncache_disk(&d.id);
            self.images.remove_disk_file(d);
        }
        let _ = std::fs::remove_file(entry.vm.default_cloud_init_path());

        vms.remove(vm_id);
        Ok(())
    }

    // ---- images and disks (spec/images.md) -----------------------------

    pub fn image_catalog(&self) -> Vec<CatalogItem> {
        self.images.catalog()
    }

    pub fn list_images(&self) -> Vec<ImageResponse> {
        self.images.list_images().iter().map(|i| self.images.image_response(i, false)).collect()
    }

    pub async fn get_image(&self, key: &str) -> Result<ImageResponse, VmManagerError> {
        let img = self.images.get_image(key)?;
        let mgr = self.images.clone();
        Ok(blocking(move || Ok(mgr.image_response(&img, true))).await?)
    }

    pub async fn pull_image(&self, req: PullImageRequest) -> Result<(ImageResponse, bool), VmManagerError> {
        let (img, created) = self.images.pull_image(req).await?;
        Ok((self.images.image_response(&img, false), created))
    }

    pub fn delete_image(&self, key: &str) -> Result<(), VmManagerError> {
        Ok(self.images.delete_image(key)?)
    }

    pub fn list_disks(&self) -> Vec<DiskResponse> {
        self.images.list_disks().iter().map(|d| self.images.disk_response(d, false)).collect()
    }

    pub async fn get_disk(&self, key: &str) -> Result<DiskResponse, VmManagerError> {
        let d = self.images.get_disk(key)?;
        let mgr = self.images.clone();
        Ok(blocking(move || Ok(mgr.disk_response(&d, true))).await?)
    }

    pub async fn create_disk(&self, req: CreateDiskRequest) -> Result<DiskResponse, VmManagerError> {
        let mgr = self.images.clone();
        let (disk, warnings) = blocking(move || {
            let (disk, warnings, _hold) = mgr.create_disk_file(&req)?;
            if let Err(e) = mgr.put_disk(&disk) {
                mgr.remove_disk_file(&disk);
                return Err(e);
            }
            Ok((disk, warnings))
        })
        .await?;
        let mut resp = self.images.disk_response(&disk, false);
        resp.warnings = warnings;
        Ok(resp)
    }

    /// Mark a disk busy for `op`, refusing while a running or paused VM
    /// has it open. Holds the VM read lock while checking, so `start_vm`
    /// (write lock) cannot slip in between the check and the mark.
    async fn begin_disk_op(&self, key: &str, op: &'static str) -> Result<(Disk, images::BusyGuard), VmManagerError> {
        let vms = self.vms.read().await;
        let disk = self.images.get_disk(key)?;
        if let Some(vm_id) = &disk.attached_to {
            if let Some(entry) = vms.get(vm_id) {
                if matches!(entry.vm.state, VmState::Running | VmState::Paused) {
                    return Err(ImageError::InUse(format!(
                        "disk {} is in use by {} VM {}; stop it first",
                        disk.name,
                        format!("{:?}", entry.vm.state).to_lowercase(),
                        entry.vm.name
                    ))
                    .into());
                }
            }
        }
        let guard = self.images.begin(&disk.id, op)?;
        Ok((disk, guard))
    }

    pub async fn resize_disk(&self, key: &str, req: ResizeDiskRequest) -> Result<DiskResponse, VmManagerError> {
        let size = requested_size(req.size_gib, req.size_bytes)?
            .ok_or_else(|| ImageError::invalid_disk("give size_gib or size_bytes"))?;
        let (disk, guard) = self.begin_disk_op(key, "resize").await?;
        let mgr = self.images.clone();
        let (disk, outcome, warnings) = blocking(move || {
            let _guard = guard;
            mgr.resize_disk(&disk.id, size, req.extend_root)
        })
        .await?;
        let mut resp = self.images.disk_response(&disk, false);
        resp.extend_root = outcome;
        resp.warnings = warnings;
        Ok(resp)
    }

    pub async fn extend_root(&self, key: &str, mode: ExtendMode) -> Result<DiskResponse, VmManagerError> {
        let (disk, guard) = self.begin_disk_op(key, "extend-root").await?;
        let mgr = self.images.clone();
        let (disk, outcome, warnings) = blocking(move || {
            let _guard = guard;
            mgr.extend_root(&disk.id, mode)
        })
        .await?;
        let mut resp = self.images.disk_response(&disk, false);
        resp.extend_root = Some(outcome);
        resp.warnings = warnings;
        Ok(resp)
    }

    pub async fn delete_disk(&self, key: &str) -> Result<(), VmManagerError> {
        // Write lock: no VM may attach the disk while it goes away.
        let vms = self.vms.write().await;
        let disk = self.images.get_disk(key)?;
        if let Some(vm_id) = &disk.attached_to {
            let vm = vms.get(vm_id).map(|e| e.vm.name.clone()).unwrap_or_else(|| vm_id.clone());
            return Err(ImageError::InUse(format!("disk {} is attached to VM {}; detach it or delete the VM first", disk.name, vm)).into());
        }
        let _guard = self.images.begin(&disk.id, "delete")?;
        self.store.commit(Commit { delete_disks: vec![disk.id.as_str()], ..Default::default() })?;
        self.images.uncache_disk(&disk.id);
        self.images.remove_disk_file(&disk);
        tracing::info!(disk = %disk.name, "disk deleted");
        Ok(())
    }

    /// Attach a data disk to a Created/Stopped VM (config only).
    pub async fn attach_disk(&self, vm_id: &str, key: &str) -> Result<Vm, VmManagerError> {
        let mut vms = self.vms.write().await;
        let entry = vms.get_mut(vm_id).ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;
        if !matches!(entry.vm.state, VmState::Created | VmState::Stopped) {
            return Err(VmManagerError::InvalidState { current: entry.vm.state.clone(), operation: "attach_disk".into() });
        }
        let mut disk = self.attachable_disk(key)?;
        disk.attached_to = Some(entry.vm.id.clone());
        let mut vm = entry.vm.clone();
        vm.config.data_disks.push(disk.id.clone());
        self.store.commit(Commit { put_vm: Some(&vm), put_disks: vec![&disk], ..Default::default() })?;
        self.images.cache_disk(&disk);
        entry.vm = vm;
        Ok(entry.vm.clone())
    }

    /// Detach a data disk from a Created/Stopped VM (config only).
    pub async fn detach_disk(&self, vm_id: &str, key: &str) -> Result<Vm, VmManagerError> {
        let mut vms = self.vms.write().await;
        let entry = vms.get_mut(vm_id).ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;
        if !matches!(entry.vm.state, VmState::Created | VmState::Stopped) {
            return Err(VmManagerError::InvalidState { current: entry.vm.state.clone(), operation: "detach_disk".into() });
        }
        let mut disk = self.images.get_disk(key)?;
        let pos = entry.vm.config.data_disks.iter().position(|d| *d == disk.id).ok_or_else(|| {
            let msg = if entry.vm.config.root_disk.as_ref() == Some(&disk.id) {
                format!("disk {} is the VM's root disk, not a data disk", disk.name)
            } else {
                format!("disk {} is not attached to this VM", disk.name)
            };
            VmManagerError::Image(ImageError::invalid_disk(msg))
        })?;
        if let Some(op) = self.images.busy_op(&disk.id) {
            return Err(ImageError::Busy(format!("disk {} is busy ({})", disk.name, op)).into());
        }
        disk.attached_to = None;
        let mut vm = entry.vm.clone();
        vm.config.data_disks.remove(pos);
        self.store.commit(Commit { put_vm: Some(&vm), put_disks: vec![&disk], ..Default::default() })?;
        self.images.cache_disk(&disk);
        entry.vm = vm;
        Ok(entry.vm.clone())
    }

    // ---- networks -----------------------------------------------------

    /// A NIC bound to vfio-pci for a DPDK uplink must not also be passed
    /// through to a VM (spec §8.3). Without netd there are no uplinks.
    fn refuse_dpdk_uplink_devices(&self, devices: &[String]) -> Result<(), VmManagerError> {
        if devices.is_empty() {
            return Ok(());
        }
        let Ok(uplinks) = self.netd.call::<Vec<glidex_netd::proto::UplinkResult>>(Op::ListUplinks) else {
            return Ok(());
        };
        for dev in devices {
            let bdf = dev.rsplit('/').next().unwrap_or(dev);
            if let Some(u) = uplinks.iter().find(|u| {
                matches!(&u.record.spec.kind, glidex_ovs::uplink::UplinkKind::Dpdk { pci, .. } if pci == bdf)
            }) {
                return Err(NetError::Conflict(format!(
                    "{} is the DPDK uplink '{}' of bridge '{}'",
                    bdf, u.record.spec.name, u.record.spec.bridge
                ))
                .into());
            }
        }
        Ok(())
    }

    /// Attach every NIC of `vm` through netd. On failure, detaches the NICs
    /// attached so far.
    fn attach_nics(&self, vm: &Vm) -> Result<Vec<(NicBinding, NicState)>, VmManagerError> {
        let mut out = Vec::new();
        for (i, att) in vm.config.networks.iter().enumerate() {
            let result = (|| -> Result<(NicBinding, NicState), VmManagerError> {
                let net = self
                    .networks
                    .get(&att.network)?
                    .ok_or_else(|| NetError::NotFound(att.network.clone()))?;
                let mac = match &att.mac {
                    Some(m) => m.clone(),
                    None => glidex_ovs::names::mac_address(&vm.id, i as u8)
                        .map_err(|e| NetError::Invalid(e.to_string()))?,
                };
                let queue_pairs = att.queue_pairs.unwrap_or(1);
                let spec = VmPortSpec {
                    bridge: net.bridge.clone(),
                    vm_id: vm.id.clone(),
                    nic_index: i as u8,
                    kind: net.port_type,
                    mac: mac.clone(),
                    vlan: net.vlan,
                    mtu: net.mtu,
                    queue_pairs,
                };
                let res: AttachResult = self.netd.call(Op::AttachVmPort(spec))?;
                Ok((
                    NicBinding {
                        id: format!("net{}", i),
                        mac: mac.clone(),
                        binding: res.binding,
                        queue_pairs,
                        mtu: net.mtu,
                    },
                    NicState {
                        network: net.name,
                        mac,
                        port: Some(res.port),
                        ipv4: res.ipv4,
                    },
                ))
            })();
            match result {
                Ok(nic) => out.push(nic),
                Err(e) => {
                    self.detach_nics(&vm.id, i);
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    /// Detach the first `count` NICs of a VM. Errors are logged: the VM is
    /// already stopped, and netd's sync cleans up anything left behind.
    fn detach_nics(&self, vm_id: &str, count: usize) {
        for i in 0..count {
            if let Err(e) = self.netd.call::<serde_json::Value>(Op::DetachVmPort {
                vm_id: vm_id.to_string(),
                nic_index: i as u8,
            }) {
                tracing::warn!(vm_id, nic = i, "glidex-netd detach failed: {}", e);
            }
        }
    }

    pub fn list_networks(&self) -> Result<Vec<Network>, VmManagerError> {
        Ok(self.networks.list()?)
    }

    pub fn get_network(&self, name: &str) -> Result<Network, VmManagerError> {
        Ok(self
            .networks
            .get(name)?
            .ok_or_else(|| NetError::NotFound(name.to_string()))?)
    }

    /// Create a network: host side in netd first, record only on success.
    pub async fn create_network(&self, req: CreateNetworkRequest) -> Result<Network, VmManagerError> {
        let net = req.to_network()?;
        if self.networks.get(&net.name)?.is_some() {
            return Err(NetError::Conflict(format!("network '{}' already exists", net.name)).into());
        }
        if let Some(other) = self.networks.list()?.into_iter().find(|n| n.bridge == net.bridge) {
            return Err(NetError::Conflict(format!(
                "bridge '{}' already belongs to network '{}'",
                net.bridge, other.name
            ))
            .into());
        }
        match net.mode {
            NetworkMode::Nat | NetworkMode::Isolated => {
                let datapath = match net.port_type {
                    VmPortKind::VhostUser => Datapath::Netdev,
                    VmPortKind::Tap => Datapath::System,
                };
                let _: BridgeRecord = self.netd.call(Op::EnsureBridge(BridgeSpec {
                    name: net.bridge.clone(),
                    datapath,
                    mtu: net.mtu,
                    adopt: false,
                }))?;
                if net.mode == NetworkMode::Nat {
                    let res: Result<NatInfo, NetError> = self.netd.call(Op::EnsureNat(NatSpec {
                        bridge: net.bridge.clone(),
                        subnet: req.subnet,
                        dns: req.dns,
                    }));
                    if let Err(e) = res {
                        let _ = self.netd.call::<serde_json::Value>(Op::DeleteBridge { name: net.bridge.clone() });
                        return Err(e.into());
                    }
                }
            }
            NetworkMode::Bridged => {
                let bridges: Vec<BridgeRecord> = self.netd.call(Op::ListBridges)?;
                if !bridges.iter().any(|b| b.spec.name == net.bridge) {
                    return Err(NetError::Invalid(format!(
                        "bridge '{}' is not a glidex bridge; create it (with an uplink) first",
                        net.bridge
                    ))
                    .into());
                }
            }
        }
        self.networks.put(&net)?;
        tracing::info!(network = %net.name, bridge = %net.bridge, mode = ?net.mode, "network created");
        Ok(net)
    }

    pub async fn delete_network(&self, name: &str) -> Result<(), VmManagerError> {
        let net = self.get_network(name)?;
        let vms = self.vms.read().await;
        let users: Vec<String> = vms
            .values()
            .filter(|e| e.vm.config.networks.iter().any(|a| a.network == name))
            .map(|e| e.vm.name.clone())
            .collect();
        if !users.is_empty() {
            return Err(NetError::Conflict(format!(
                "network '{}' is used by VM(s): {}",
                name,
                users.join(", ")
            ))
            .into());
        }
        if net.owns_bridge {
            if net.mode == NetworkMode::Nat {
                self.netd.call::<serde_json::Value>(Op::DeleteNat { bridge: net.bridge.clone() })?;
            }
            self.netd.call::<serde_json::Value>(Op::DeleteBridge { name: net.bridge.clone() })?;
        }
        self.networks.delete(name)?;
        tracing::info!(network = %name, "network deleted");
        Ok(())
    }

    /// Create the `default` NAT network if netd is usable and it's missing.
    pub async fn ensure_default_network(&self) -> Result<Option<Network>, VmManagerError> {
        if self.networks.get(network::DEFAULT_NETWORK)?.is_some() {
            return Ok(None);
        }
        let (access, caps) = self.netd.probe();
        let running = caps
            .ok()
            .and_then(|c| c.get("ovs_running").and_then(|v| v.as_bool()))
            .unwrap_or(false);
        if access != network::NetdAccess::Full || !running {
            return Ok(None);
        }
        let req = CreateNetworkRequest {
            name: network::DEFAULT_NETWORK.into(),
            mode: NetworkMode::Nat,
            port_type: VmPortKind::Tap,
            bridge: Some(network::DEFAULT_BRIDGE.into()),
            subnet: None,
            vlan: None,
            mtu: None,
            dns: true,
        };
        self.create_network(req).await.map(Some)
    }

    pub fn netd(&self) -> &Netd {
        &self.netd
    }

    pub fn list_credentials(&self) -> Result<Vec<Credential>, VmManagerError> {
        Ok(self.credentials.list()?)
    }

    pub fn get_credential(&self, username: &str) -> Result<Credential, VmManagerError> {
        Ok(self.credentials.get(username)?)
    }

    pub fn create_credential(
        &self,
        req: CreateCredentialRequest,
    ) -> Result<Credential, VmManagerError> {
        let cred = self.credentials.create(req)?;
        tracing::info!(username = %cred.username, "Credential created");
        Ok(cred)
    }

    /// Changes reach a VM only on its first boot: cloud-init provisions
    /// users once per instance-id, and a VM keeps its id for life.
    pub fn update_credential(
        &self,
        username: &str,
        req: UpdateCredentialRequest,
    ) -> Result<Credential, VmManagerError> {
        let cred = self.credentials.update(username, req)?;
        tracing::info!(username = %cred.username, "Credential updated");
        Ok(cred)
    }

    /// Refuses to delete a credential still referenced by a VM.
    pub async fn delete_credential(&self, username: &str) -> Result<(), VmManagerError> {
        let vms = self.vms.read().await;
        let users: Vec<String> = vms
            .values()
            .filter(|e| e.vm.config.credential.as_deref() == Some(username))
            .map(|e| e.vm.name.clone())
            .collect();
        if !users.is_empty() {
            return Err(VmManagerError::CredentialInUse {
                username: username.to_string(),
                vms: users,
            });
        }
        self.credentials.delete(username)?;
        tracing::info!(username = %username, "Credential deleted");
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
                self.detach_nics(&entry.vm.id, entry.vm.config.networks.len());

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

/// Run blocking image work off the async runtime.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ImageError> + Send + 'static,
) -> Result<T, ImageError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ImageError::Io(format!("task failed: {}", e)))?
}
