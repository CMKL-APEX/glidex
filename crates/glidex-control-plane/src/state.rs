//! `VmManager`: admission over the store (spec/reconciliation.md D5).
//!
//! The API writes desired state here: validation, authorization-adjacent
//! checks, quotas and exclusivity claims happen synchronously and still
//! fail the request; a write that passes changes `spec`, bumps the VM's
//! `generation` and queues it for the VM controller (`controller::vm`),
//! which does everything that touches the host. Nothing here holds a
//! hypervisor handle or talks to a hypervisor.
//!
//! The in-memory map is a cache of the `vms` table: every write goes
//! through it under its lock and is persisted before the cache changes.

use crate::controller::queue::{Key, WorkQueue};
use crate::controller::Settings;
use crate::credentials::{
    Credential, CredentialError, CredentialStore, CreateCredentialRequest, UpdateCredentialRequest,
};
use crate::hypervisor::{check_command_line_path, HypervisorError, HypervisorType};
use crate::images::{
    self, disk::requested_size, CatalogItem, CreateDiskRequest, Disk, DiskResponse, ExtendMode,
    ImageError, ImageManager, ImageResponse, ImageSettings, PullImageRequest, ResizeDiskRequest,
};
use crate::instance::runner::Runner;
use crate::models::{
    DiskBinding, DiskSelection, HostBootPolicy, NetworkAttachment, PowerState, RestartPolicy, Vm, VmConfig, VmPhase,
    VmState, MAX_STOP_GRACE_SECS,
};
use crate::network::{self, CreateNetworkRequest, NetError, Netd, Network, NetworkMode, NetworkStore};
use crate::store::{event_key, Commit, Event, EventKind, PersistenceError, VmStore};
use crate::tenancy::{self, Delta, ProjectStore, QuotaMode, QuotaOverrun, TenancyError, Usage};
use arc_swap::ArcSwap;
use glidex_netd::proto::{BridgeRecord, NatInfo, Op};
use glidex_ovs::bridge::{BridgeSpec, Datapath};
use glidex_ovs::nat::NatSpec;
use glidex_ovs::vm_port::VmPortKind;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;
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
    /// The request would go over these project quotas.
    QuotaExceeded(Vec<QuotaOverrun>),
    Tenancy(TenancyError),
    /// The VM is being deleted: its spec no longer changes (§6.3).
    Deleting(String),
    /// `If-Match` named another resource version (§12.1).
    PreconditionFailed { expected: u64, actual: u64 },
}

impl std::fmt::Display for VmManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmManagerError::VmNotFound(id) => write!(f, "VM not found: {}", id),
            VmManagerError::VmAlreadyExists(name) => write!(f, "VM already exists: {}", name),
            VmManagerError::InvalidState { current, operation } => {
                write!(f, "Invalid state {} for operation: {}", current, operation)
            }
            VmManagerError::HypervisorError(e) => write!(f, "Hypervisor error: {}", e),
            VmManagerError::PersistenceError(e) => write!(f, "Persistence error: {}", e),
            VmManagerError::HypervisorNotAvailable(h) => write!(f, "Hypervisor not available: {:?}", h),
            VmManagerError::Credential(e) => write!(f, "{}", e),
            VmManagerError::Network(e) => write!(f, "{}", e),
            VmManagerError::Image(e) => write!(f, "{}", e),
            VmManagerError::QuotaExceeded(o) => {
                write!(f, "{}", o.iter().map(|o| o.to_string()).collect::<Vec<_>>().join("; "))
            }
            VmManagerError::Tenancy(e) => write!(f, "{}", e),
            VmManagerError::CredentialInUse { username, vms } => {
                write!(f, "credential {} is used by VM(s): {}", username, vms.join(", "))
            }
            VmManagerError::Deleting(name) => write!(f, "VM {} is being deleted", name),
            VmManagerError::PreconditionFailed { expected, actual } => {
                write!(f, "resource version is {}, not {}", actual, expected)
            }
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

impl From<TenancyError> for VmManagerError {
    fn from(e: TenancyError) -> Self {
        VmManagerError::Tenancy(e)
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

/// An orphan found at startup (§9.4, D17): reported, never touched.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Orphan {
    pub kind: &'static str,
    pub id: String,
}

/// How [`VmManager::wait_converged`] ended.
#[derive(Debug)]
pub enum Waited {
    /// Converged, or failed (see its `Ready` condition).
    Decided(Box<Vm>),
    /// The VM is gone (a deletion finished).
    Gone,
    TimedOut(Option<Box<Vm>>),
}

/// Desired-state options of a new VM (§7.1).
#[derive(Debug, Clone, Default)]
pub struct VmOptions {
    pub power: Option<PowerState>,
    pub restart_policy: Option<RestartPolicy>,
    pub on_host_boot: Option<HostBootPolicy>,
    pub stop_grace_secs: Option<u32>,
}

/// `PATCH /vms/{id}`: a JSON merge patch of `spec` (§12.1). Immutable
/// fields are accepted by the parser only to be refused with a clear
/// error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmPatch {
    #[serde(default)]
    pub power: Option<PowerState>,
    #[serde(default)]
    pub restart_policy: Option<RestartPolicy>,
    #[serde(default)]
    pub on_host_boot: Option<HostBootPolicy>,
    #[serde(default)]
    pub stop_grace_secs: Option<u32>,
    #[serde(default)]
    pub config: Option<ConfigPatch>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigPatch {
    #[serde(default)]
    pub vcpu_count: Option<u8>,
    #[serde(default)]
    pub mem_size_mib: Option<u32>,
    #[serde(default)]
    pub kernel_args: Option<String>,
    /// `null` removes the credential.
    #[serde(default)]
    pub credential: Option<serde_json::Value>,
    #[serde(default)]
    pub hugepages: Option<bool>,
    #[serde(default)]
    pub vfio_devices: Option<Vec<String>>,
    /// Disk ids or names, replacing the list.
    #[serde(default)]
    pub data_disks: Option<Vec<String>>,
    #[serde(default)]
    pub networks: Option<Vec<NetworkAttachment>>,
    // Immutable (§7.3): present only to be refused.
    #[serde(default)]
    pub hypervisor: Option<serde_json::Value>,
    #[serde(default)]
    pub kernel_image_path: Option<serde_json::Value>,
    #[serde(default)]
    pub firmware_path: Option<serde_json::Value>,
    #[serde(default)]
    pub firmware: Option<serde_json::Value>,
    #[serde(default)]
    pub rootfs_path: Option<serde_json::Value>,
    #[serde(default)]
    pub cloud_init_path: Option<serde_json::Value>,
    #[serde(default)]
    pub root_disk: Option<serde_json::Value>,
    #[serde(default)]
    pub image: Option<serde_json::Value>,
}

impl ConfigPatch {
    pub fn immutable_fields(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        for (name, present) in [
            ("hypervisor", self.hypervisor.is_some()),
            ("kernel_image_path", self.kernel_image_path.is_some()),
            ("firmware_path", self.firmware_path.is_some()),
            ("firmware", self.firmware.is_some()),
            ("rootfs_path", self.rootfs_path.is_some()),
            ("cloud_init_path", self.cloud_init_path.is_some()),
            ("root_disk", self.root_disk.is_some()),
            ("image", self.image.is_some()),
        ] {
            if present {
                v.push(name);
            }
        }
        v
    }
}

pub struct VmManager {
    pub(crate) vms: RwLock<HashMap<String, Vm>>,
    pub(crate) store: VmStore,
    pub(crate) credentials: CredentialStore,
    pub(crate) networks: NetworkStore,
    pub(crate) projects: ProjectStore,
    pub(crate) netd: Netd,
    pub(crate) images: Arc<ImageManager>,
    /// Where per-VM state outside the database lives (next to it).
    pub(crate) data_dir: PathBuf,
    pub(crate) settings: ArcSwap<Settings>,
    pub(crate) runner: ArcSwap<Runner>,
    pub(crate) queue: Arc<WorkQueue>,
    /// Bumped on every VM write; `?wait` and tests watch it.
    /// Rung after every write to a VM, disk, image or network.
    pub(crate) changed: crate::store::Bell,
    /// Serializes "write `status.nics` + attach a port" with `sync_vms`, so
    /// a sync never detaches a port a reconcile is adding (D16).
    pub(crate) ports_lock: tokio::sync::Mutex<()>,
    /// `(vm id, pid)` pairs with an exit watch running.
    pub(crate) watched: std::sync::Mutex<std::collections::HashSet<(String, u32)>>,
    pub(crate) orphans: std::sync::Mutex<Vec<Orphan>>,
    /// netd's socket identity at the last port sync (restart detection).
    pub(crate) netd_seen: std::sync::Mutex<Option<(u64, u64)>>,
    pub(crate) controllers_started: std::sync::atomic::AtomicBool,
    /// The controllers' tasks, aborted by `stop_controllers`.
    pub(crate) tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Resource usage metering (spec/metering.md), once started.
    pub(crate) meter: std::sync::OnceLock<Arc<crate::metering::Meter>>,
    pub(crate) me: Weak<VmManager>,
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

fn invalid(msg: impl Into<String>) -> VmManagerError {
    HypervisorError::InvalidConfig(msg.into()).into()
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
        let changed = store.database().bell();
        let images = ImageManager::new(store.database(), ImageSettings::from_env(&base))?;
        for ty in [HypervisorType::CloudHypervisor, HypervisorType::Qemu] {
            if !crate::hypervisor::driver(ty).is_available() {
                tracing::warn!("Hypervisor {} binary {:?} not found — VMs configured for it will fail to start", ty, ty.binary_name());
            }
        }
        let settings = Settings::default();
        let runner = Runner::new(settings.runner);
        let credentials = CredentialStore::new(store.database())?;
        let networks = NetworkStore::new(store.database())?;
        let projects = ProjectStore::new(store.database())?;
        Ok(Arc::new_cyclic(|me| Self {
            vms: RwLock::new(HashMap::new()),
            credentials,
            networks,
            projects,
            netd,
            images,
            store,
            data_dir: base,
            settings: ArcSwap::from_pointee(settings),
            runner: ArcSwap::from_pointee(runner),
            queue: Arc::new(WorkQueue::new()),
            changed,
            ports_lock: tokio::sync::Mutex::new(()),
            watched: std::sync::Mutex::new(Default::default()),
            orphans: std::sync::Mutex::new(Vec::new()),
            netd_seen: std::sync::Mutex::new(None),
            controllers_started: std::sync::atomic::AtomicBool::new(false),
            tasks: std::sync::Mutex::new(Vec::new()),
            meter: std::sync::OnceLock::new(),
            me: me.clone(),
        }))
    }

    /// Apply `control-plane.json`'s `reconcile` and `console` sections.
    /// Call before [`Self::initialize`].
    pub fn configure(&self, cfg: &crate::config::Config) {
        let s = Settings::from_config(cfg);
        self.runner.store(Arc::new(Runner::new(s.runner)));
        self.settings.store(Arc::new(s));
    }

    pub fn settings(&self) -> Arc<Settings> {
        self.settings.load_full()
    }

    pub fn runner(&self) -> Arc<Runner> {
        self.runner.load_full()
    }

    pub(crate) fn arc(&self) -> Arc<VmManager> {
        self.me.upgrade().expect("VmManager is alive")
    }

    /// Get the default database path (~/.glidex/glidex.db)
    fn default_db_path() -> PathBuf {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".glidex").join("glidex.db")
    }

    /// Load the store (migrating it, §6.6), adopt running instances
    /// (§9.4 steps 1-4). [`Self::start_controllers`] then starts the loops.
    pub async fn initialize(&self) -> Result<(), VmManagerError> {
        let migrated = self.store.migrate(self.settings().on_host_boot)?;
        if migrated > 0 {
            tracing::info!(count = migrated, "migrated VM records to the desired-state schema");
        }
        let persisted = self.store.load_all()?;
        self.images.initialize();
        {
            let default_project = self.projects.default_project_id();
            let mut vms = self.vms.write().await;
            for mut vm in persisted {
                if vm.project.is_empty() {
                    vm.project = default_project.clone();
                    self.store.save(&vm)?;
                }
                vms.insert(vm.id.clone(), vm);
            }
        }
        self.adopt_into_default_project()?;
        self.adopt_instances().await;
        Ok(())
    }

    // ---- cache and writes ------------------------------------------------

    pub(crate) async fn vm(&self, id: &str) -> Option<Vm> {
        self.vms.read().await.get(id).cloned()
    }

    pub(crate) fn notify_changed(&self) {
        crate::store::ring(&self.changed);
    }

    /// Persist `vm` (with `events`) and update the cache. The caller holds
    /// the map's write lock (`vms`).
    pub(crate) fn put_locked(&self, vms: &mut HashMap<String, Vm>, vm: Vm, events: Vec<Event>) -> Result<Vm, VmManagerError> {
        let key = event_key("vm", &vm.id);
        self.store.commit(Commit {
            put_vm: Some(&vm),
            events: events.into_iter().map(|e| (key.clone(), e)).collect(),
            ..Default::default()
        })?;
        vms.insert(vm.id.clone(), vm.clone());
        self.notify_changed();
        Ok(vm)
    }

    /// A spec write: bump generation and resource version, persist with an
    /// event, queue the VM for its controller.
    fn spec_write(
        &self,
        vms: &mut HashMap<String, Vm>,
        mut vm: Vm,
        actor: &str,
        reason: &str,
        message: String,
    ) -> Result<Vm, VmManagerError> {
        vm.generation += 1;
        vm.resource_version += 1;
        let ev = Event::new(actor, EventKind::Normal, reason, message);
        let vm = self.put_locked(vms, vm, vec![ev])?;
        self.queue.add(Key::Vm(vm.id.clone()));
        Ok(vm)
    }

    fn writable<'a>(vms: &'a HashMap<String, Vm>, id: &str, if_match: Option<u64>) -> Result<&'a Vm, VmManagerError> {
        let vm = vms.get(id).ok_or_else(|| VmManagerError::VmNotFound(id.to_string()))?;
        if vm.deletion_requested_at.is_some() {
            return Err(VmManagerError::Deleting(vm.name.clone()));
        }
        if let Some(expected) = if_match {
            if expected != vm.resource_version {
                return Err(VmManagerError::PreconditionFailed { expected, actual: vm.resource_version });
            }
        }
        Ok(vm)
    }

    // ---- create ----------------------------------------------------------

    #[allow(dead_code)] // used by the tests through the lib crate
    pub async fn create_vm(&self, name: String, config: VmConfig) -> Result<Vm, VmManagerError> {
        self.create_vm_with_disks(name, config, DiskSelection::default()).await.map(|(vm, _)| vm)
    }

    /// Create a VM in the default project, quotas not enforced (embedding
    /// and tests). The API uses [`Self::create_vm_with`].
    pub async fn create_vm_with_disks(
        &self,
        name: String,
        config: VmConfig,
        sel: DiskSelection,
    ) -> Result<(Vm, Vec<String>), VmManagerError> {
        let project = self.default_project_id();
        self.create_vm_with(&project, name, config, sel, VmOptions::default(), QuotaMode::MayExceed, "api")
            .await
            .map(|(vm, w, _)| (vm, w))
    }

    pub async fn create_vm_in(
        &self,
        project: &str,
        name: String,
        config: VmConfig,
        sel: DiskSelection,
        quota: QuotaMode,
    ) -> Result<(Vm, Vec<String>, Vec<QuotaOverrun>), VmManagerError> {
        self.create_vm_with(project, name, config, sel, VmOptions::default(), quota, "api").await
    }

    /// Admission paths the VM's command line will carry (§8.2).
    fn check_paths(&self, config: &VmConfig) -> Result<(), VmManagerError> {
        let systemd = matches!(*self.runner(), Runner::Systemd(_));
        let mut paths: Vec<(&str, &str)> = vec![("kernel_image_path", &config.kernel_image_path), ("rootfs_path", &config.rootfs_path)];
        if let Some(p) = &config.firmware_path {
            paths.push(("firmware_path", p));
        }
        if let Some(p) = &config.cloud_init_path {
            paths.push(("cloud_init_path", p));
        }
        for d in &config.vfio_devices {
            paths.push(("vfio_devices", d));
        }
        for (field, p) in paths {
            check_command_line_path(field, p)?;
            // The VM unit has its own /tmp (PrivateTmp=, §13.1).
            if systemd && (p.starts_with("/tmp/") || p.starts_with("/var/tmp/")) {
                return Err(invalid(format!(
                    "{} {} is under /tmp, which VMs cannot see (they have a private /tmp); use another directory",
                    field, p
                )));
            }
        }
        Ok(())
    }

    fn check_options(opts: &VmOptions) -> Result<(), VmManagerError> {
        if opts.stop_grace_secs.is_some_and(|g| g > MAX_STOP_GRACE_SECS) {
            return Err(invalid(format!("stop_grace_secs must be 0-{}", MAX_STOP_GRACE_SECS)));
        }
        Ok(())
    }

    fn check_networks(&self, project: &str, networks: &[NetworkAttachment]) -> Result<(), VmManagerError> {
        if networks.len() > glidex_ovs::names::MAX_NICS as usize {
            return Err(invalid(format!("at most {} networks per VM", glidex_ovs::names::MAX_NICS)));
        }
        for att in networks {
            match self.networks.get(&att.network)? {
                None => return Err(invalid(format!("network not found: {}", att.network))),
                Some(n) if !n.usable_by(project) => {
                    return Err(invalid(format!("network '{}' is not available to this project", att.network)))
                }
                Some(n) if n.deletion_requested_at.is_some() => {
                    return Err(invalid(format!("network '{}' is being deleted", att.network)))
                }
                Some(_) => {}
            }
            if let Some(mac) = &att.mac {
                glidex_ovs::names::validate_mac(mac).map_err(|e| invalid(e.to_string()))?;
            }
            if let Some(q) = att.queue_pairs {
                if !(1..=8).contains(&q) {
                    return Err(invalid("queue_pairs must be 1-8"));
                }
            }
        }
        Ok(())
    }

    fn check_credential(&self, project: &str, config: &VmConfig) -> Result<(), VmManagerError> {
        if let Some(username) = &config.credential {
            // A credential only takes effect through the generated seed.
            if config.firmware_path.is_none() || config.cloud_init_path.is_some() {
                return Err(invalid("credential requires firmware boot without a custom cloud_init_path"));
            }
            match self.credentials.get(project, username) {
                Ok(_) => {}
                Err(CredentialError::NotFound(_)) => return Err(invalid(format!("credential not found: {}", username))),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Create a VM in `project`, resolving managed disks (spec images.md
    /// §7). Returns the VM, any warnings, and the quota limits it went
    /// over (only with [`QuotaMode::MayExceed`]; to be audited).
    #[allow(clippy::too_many_arguments)]
    pub async fn create_vm_with(
        &self,
        project: &str,
        name: String,
        mut config: VmConfig,
        sel: DiskSelection,
        opts: VmOptions,
        quota: QuotaMode,
        actor: &str,
    ) -> Result<(Vm, Vec<String>, Vec<QuotaOverrun>), VmManagerError> {
        let project_rec = self.projects.get(project)?.ok_or_else(|| TenancyError::NotFound(project.to_string()))?;
        let roots = [!config.rootfs_path.is_empty(), sel.image.is_some(), sel.root_disk.is_some()];
        if roots.iter().filter(|b| **b).count() != 1 {
            return Err(invalid("give exactly one of rootfs_path, image or root_disk"));
        }
        if sel.root_disk_size_gib.is_some() && sel.image.is_none() {
            return Err(invalid("root_disk_size_gib needs image"));
        }
        if sel.firmware.is_some() && config.firmware_path.is_some() {
            return Err(invalid("give firmware (an image) or firmware_path, not both"));
        }
        // A managed root disk is a cloud image: firmware boot unless the
        // caller brought a kernel, through the newest firmware image for
        // the hypervisor unless one is named.
        let firmware = match &sel.firmware {
            Some(key) => Some(self.images.get_image(key)?),
            None if (sel.image.is_some() || sel.root_disk.is_some())
                && config.kernel_image_path.is_empty()
                && config.firmware_path.is_none() =>
            {
                Some(self.images.default_firmware(config.hypervisor).ok_or_else(|| {
                    invalid(format!(
                        "no firmware image for {}: pull one from the firmware catalog (GET /images/firmware-catalog), or give kernel_image_path",
                        config.hypervisor
                    ))
                })?)
            }
            None => None,
        };
        if config.vcpu_count == 0 {
            return Err(invalid("vcpu_count must be greater than 0"));
        }
        if config.mem_size_mib == 0 {
            return Err(invalid("mem_size_mib must be greater than 0"));
        }
        if firmware.is_none() && config.firmware_path.is_none() && config.kernel_image_path.is_empty() {
            return Err(invalid("either kernel_image_path or firmware is required"));
        }
        Self::check_options(&opts)?;
        self.check_paths(&config)?;
        self.refuse_dpdk_uplink_devices(&config.vfio_devices)?;
        self.check_networks(project, &config.networks)?;

        let mut vms = self.vms.write().await;

        // Checked under the VM write lock, so delete_image (which holds the
        // read lock) can't remove it between the check and the insert.
        if let Some(fw) = &firmware {
            let fw = self.images.get_image(&fw.id)?;
            self.check_firmware(&fw, config.hypervisor)?;
            config.firmware_path = Some(self.images.firmware_path(&fw.id).to_string_lossy().into_owned());
            config.firmware_image = Some(fw.id);
        }

        // Checked under the VM write lock so delete_credential (which holds
        // the read lock) can't remove it between the check and the insert.
        self.check_credential(project, &config)?;

        // Names are unique per project.
        if vms.values().any(|vm| vm.name == name && vm.project == project) {
            return Err(VmManagerError::VmAlreadyExists(name));
        }
        for d in &config.vfio_devices {
            if let Some(other) = Self::device_claimed_by(&vms, d, None) {
                return Err(ImageError::InUse(format!("PCI device {} is used by VM {}", d, other)).into());
            }
        }

        let power = opts.power.unwrap_or_default();
        // Quotas, checked under the write lock so concurrent creates can't
        // both pass. Disk use is checked again once the root disk's real
        // size is known.
        let mut bypassed = Vec::new();
        let delta = Delta {
            vms: 1,
            vcpus: config.vcpu_count as u64,
            memory_mib: config.mem_size_mib as u64,
            disk_gib: sel.root_disk_size_gib.unwrap_or(0),
            running_vms: u64::from(power != PowerState::Stopped),
            ..Default::default()
        };
        let usage = self.usage_locked(project, &vms)?;
        Self::apply_quota(&project_rec.quotas, &usage, &delta, quota, &mut bypassed)?;

        let mut vm = Vm::new(name, config);
        vm.project = project.to_string();
        vm.spec.power = power;
        vm.spec.restart_policy = opts.restart_policy.unwrap_or_default();
        vm.spec.on_host_boot = opts.on_host_boot.unwrap_or(self.settings().on_host_boot);
        vm.spec.stop_grace_secs = opts.stop_grace_secs.unwrap_or(0);
        // Stable MACs derived from the VM id, so they show up in the API
        // and survive restarts.
        for (i, att) in vm.spec.config.networks.iter_mut().enumerate() {
            if att.mac.is_none() {
                att.mac = glidex_ovs::names::mac_address(&vm.id, i as u8).ok();
            }
        }

        // Managed disks. Checked under the VM write lock, so no other VM
        // can claim the same disk in between.
        let mut disks: Vec<Disk> = Vec::new();
        let mut warnings = Vec::new();
        for key in &sel.data_disks {
            let d = self.attachable_disk_in(&vms, key, project, None)?;
            if disks.iter().any(|x| x.id == d.id) {
                return Err(invalid(format!("disk {} listed twice", d.name)));
            }
            vm.spec.config.data_disks.push(d.id.clone());
            disks.push(d);
        }
        if let Some(key) = &sel.root_disk {
            let d = self.attachable_disk_in(&vms, key, project, None)?;
            if disks.iter().any(|x| x.id == d.id) {
                return Err(invalid(format!("disk {} is both root_disk and a data disk", d.name)));
            }
            vm.spec.config.root_disk = Some(d.id.clone());
            vm.spec.config.rootfs_path = self.images.disk_path(&d).to_string_lossy().into_owned();
            disks.push(d);
        }
        let mut created: Option<Disk> = None;
        if let Some(image) = &sel.image {
            // The root disk is recorded with the VM, pending; the disk
            // controller makes it once the image is ready (§7.6), so the
            // image may still be downloading.
            let mut disk_name = root_disk_name(&vm.name);
            if self.images.disk_name_taken(&disk_name) {
                disk_name = format!("{}-{}", disk_name.trim_end_matches("-root"), &vm.id[..8]);
            }
            let req = CreateDiskRequest {
                name: disk_name,
                project: Some(project.to_string()),
                size_gib: sel.root_disk_size_gib,
                image: Some(image.clone()),
                ..Default::default()
            };
            let mut d = self.images.new_disk_record(&req)?;
            if sel.root_disk_size_gib.is_none() {
                let disk_delta = Delta { disk_gib: gib_ceil(self.estimated_size(&d)), ..Default::default() };
                Self::apply_quota(&project_rec.quotas, &usage, &disk_delta, quota, &mut bypassed)?;
            }
            d.owner = Some(vm.id.clone());
            vm.spec.config.root_disk = Some(d.id.clone());
            vm.spec.config.owns_root_disk = true;
            vm.spec.config.rootfs_path = self.images.disk_path(&d).to_string_lossy().into_owned();
            created = Some(d.clone());
            disks.push(d);
        }
        for d in &mut disks {
            d.attached_to = Some(vm.id.clone());
            if Some(&d.id) == vm.spec.config.root_disk.as_ref() && d.pending_growpart && vm.spec.config.cloud_init_path.is_some() {
                warnings.push(format!(
                    "disk {} is waiting for an on-boot root partition grow, but this VM uses a custom cloud_init_path; enable growpart in that seed",
                    d.name
                ));
            }
        }

        // The VM, its disks' claims and its first event in one transaction.
        let ev = Event::new(actor, EventKind::Normal, "Created", format!("desired state {}", power_name(power)));
        let commit = Commit {
            put_vm: Some(&vm),
            put_disks: disks.iter().collect(),
            events: vec![(event_key("vm", &vm.id), ev)],
            ..Default::default()
        };
        self.store.commit(commit)?;
        for d in &disks {
            self.images.cache_disk(d);
        }
        vms.insert(vm.id.clone(), vm.clone());
        self.notify_changed();
        if let Some(d) = &created {
            self.queue.add(Key::Disk(d.id.clone()));
        }
        self.queue.add(Key::Vm(vm.id.clone()));
        Ok((vm, warnings, bypassed))
    }

    /// A firmware image a new VM of `hypervisor` may boot through.
    fn check_firmware(&self, fw: &images::Image, hypervisor: crate::hypervisor::HypervisorType) -> Result<(), VmManagerError> {
        if !fw.is_firmware() {
            return Err(invalid(format!("image {} is not a firmware image", fw.name)));
        }
        if fw.hypervisor != Some(hypervisor) {
            return Err(invalid(format!(
                "firmware {} is built for {}, not {}",
                fw.name,
                fw.hypervisor.map(|h| h.to_string()).unwrap_or_default(),
                hypervisor
            )));
        }
        if fw.deletion_requested_at.is_some() {
            return Err(ImageError::NotReady(format!("firmware {} is being deleted", fw.name)).into());
        }
        if fw.status != images::ImageStatus::Ready {
            return Err(ImageError::NotReady(format!("firmware {} is not ready ({:?})", fw.name, fw.status)).into());
        }
        Ok(())
    }

    /// Names of the VMs booting through firmware image `id`.
    fn firmware_users(vms: &HashMap<String, Vm>, id: &str) -> Vec<String> {
        let mut v: Vec<String> =
            vms.values().filter(|vm| vm.config().firmware_image.as_deref() == Some(id)).map(|vm| vm.name.clone()).collect();
        v.sort();
        v
    }

    // ---- claims (D11) ------------------------------------------------------

    /// The VM (other than `except`) claiming disk `disk_id`: its spec
    /// references it, or a live instance of it has it open.
    pub(crate) fn disk_claimed_by(vms: &HashMap<String, Vm>, disk_id: &str, except: Option<&str>) -> Option<String> {
        vms.values()
            .filter(|vm| Some(vm.id.as_str()) != except)
            .find(|vm| {
                let c = vm.config();
                c.root_disk.as_deref() == Some(disk_id)
                    || c.data_disks.iter().any(|d| d == disk_id)
                    || vm.status.instance.as_ref().is_some_and(|i| i.disks.iter().any(|d| d == disk_id))
            })
            .map(|vm| vm.name.clone())
    }

    fn device_claimed_by(vms: &HashMap<String, Vm>, device: &str, except: Option<&str>) -> Option<String> {
        let bdf = crate::hypervisor::vfio_device_id(device);
        vms.values()
            .filter(|vm| Some(vm.id.as_str()) != except)
            .find(|vm| {
                vm.config().vfio_devices.iter().any(|d| crate::hypervisor::vfio_device_id(d) == bdf)
                    || vm.status.instance.as_ref().is_some_and(|i| i.vfio_devices.iter().any(|d| crate::hypervisor::vfio_device_id(d) == bdf))
            })
            .map(|vm| vm.name.clone())
    }

    /// A disk a VM of `project` may claim: exists, in that project,
    /// unclaimed (by anyone but `vm_id`), not busy.
    fn attachable_disk_in(&self, vms: &HashMap<String, Vm>, key: &str, project: &str, vm_id: Option<&str>) -> Result<Disk, VmManagerError> {
        let d = self.images.get_disk(key)?;
        if d.project != project {
            return Err(invalid(format!("disk {} belongs to another project", d.name)));
        }
        if let Some(other) = Self::disk_claimed_by(vms, &d.id, vm_id) {
            return Err(ImageError::InUse(format!("disk {} is attached to VM {}", d.name, other)).into());
        }
        if let Some(owner) = d.attached_to.as_deref().filter(|o| Some(*o) != vm_id && vms.contains_key(*o)) {
            return Err(ImageError::InUse(format!("disk {} is attached to VM {}", d.name, owner)).into());
        }
        if let Some(op) = self.images.busy_op(&d.id) {
            return Err(ImageError::Busy(format!("disk {} is busy ({})", d.name, op)).into());
        }
        Ok(d)
    }

    /// Bindings for a VM's managed disks, checked to be usable right now:
    /// the root disk (with its record) and the data disks.
    pub(crate) fn disk_bindings(&self, vm: &Vm) -> Result<(Option<RootBinding>, Vec<DiskBinding>), VmManagerError> {
        let bind = |id: &str| -> Result<(DiskBinding, Disk), VmManagerError> {
            let d = self.images.get_disk(id)?;
            if d.phase != images::DiskPhase::Ready {
                let why = d.conditions.iter().find(|c| c.kind == "Ready").map(|c| format!(": {}", c.message)).unwrap_or_default();
                return Err(ImageError::NotReady(format!("disk {} is {:?}{}", d.name, d.phase, why)).into());
            }
            if let Some(op) = self.images.busy_op(&d.id) {
                return Err(ImageError::Busy(format!("disk {} is busy ({})", d.name, op)).into());
            }
            let path = self.images.disk_path(&d);
            if !path.exists() {
                return Err(ImageError::Io(format!("disk {} file is missing: {}", d.name, path.display())).into());
            }
            Ok((DiskBinding { path: path.to_string_lossy().into_owned(), format: d.format, backing_files: d.is_linked() }, d))
        };
        let root = vm.config().root_disk.as_deref().map(bind).transpose()?;
        let data = vm.config().data_disks.iter().map(|id| bind(id).map(|(b, _)| b)).collect::<Result<Vec<_>, _>>()?;
        Ok((root, data))
    }

    // ---- desired power state ----------------------------------------------

    /// Start a VM, quotas not enforced (embedding and tests); returns at
    /// once, see [`Self::wait_converged`].
    pub async fn start_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        self.set_power(vm_id, PowerState::Running, None, QuotaMode::MayExceed, "api", None).await.map(|(vm, _)| vm)
    }

    pub async fn stop_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        self.set_power(vm_id, PowerState::Stopped, None, QuotaMode::MayExceed, "api", None).await.map(|(vm, _)| vm)
    }

    pub async fn pause_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        self.set_power(vm_id, PowerState::Paused, None, QuotaMode::MayExceed, "api", None).await.map(|(vm, _)| vm)
    }

    /// `spec.power = power` (and `stop_grace_secs` when given), checking the
    /// project's `running_vms` quota when the VM leaves `Stopped` (§12.2).
    pub async fn set_power(
        &self,
        vm_id: &str,
        power: PowerState,
        grace: Option<u32>,
        quota: QuotaMode,
        actor: &str,
        if_match: Option<u64>,
    ) -> Result<(Vm, Vec<QuotaOverrun>), VmManagerError> {
        if grace.is_some_and(|g| g > MAX_STOP_GRACE_SECS) {
            return Err(invalid(format!("graceful_timeout_secs must be 0-{}", MAX_STOP_GRACE_SECS)));
        }
        let mut vms = self.vms.write().await;
        let cur = Self::writable(&vms, vm_id, if_match)?.clone();
        let mut bypassed = Vec::new();
        if cur.spec.power == PowerState::Stopped && power != PowerState::Stopped {
            if let Some(p) = self.projects.get(&cur.project)? {
                let usage = self.usage_locked(&cur.project, &vms)?;
                Self::apply_quota(&p.quotas, &usage, &Delta { running_vms: 1, ..Default::default() }, quota, &mut bypassed)?;
            }
        }
        let grace = grace.unwrap_or(cur.spec.stop_grace_secs);
        if cur.spec.power == power && cur.spec.stop_grace_secs == grace {
            return Ok((cur, bypassed));
        }
        let mut vm = cur;
        vm.spec.power = power;
        vm.spec.stop_grace_secs = grace;
        let msg = format!("desired state {}", power_name(power));
        let vm = self.spec_write(&mut vms, vm, actor, "PowerChanged", msg)?;
        Ok((vm, bypassed))
    }

    // ---- other spec edits (§7.3) -------------------------------------------

    /// Apply a merge patch of `spec`. Immutable fields are refused before
    /// anything else.
    pub async fn patch_vm(
        &self,
        vm_id: &str,
        patch: VmPatch,
        quota: QuotaMode,
        actor: &str,
        if_match: Option<u64>,
    ) -> Result<(Vm, Vec<QuotaOverrun>), VmManagerError> {
        if let Some(c) = &patch.config {
            let fields = c.immutable_fields();
            if !fields.is_empty() {
                return Err(invalid(format!("immutable field(s): {}", fields.join(", "))));
            }
        }
        Self::check_options(&VmOptions { stop_grace_secs: patch.stop_grace_secs, ..Default::default() })?;
        let mut vms = self.vms.write().await;
        let cur = Self::writable(&vms, vm_id, if_match)?.clone();
        let project = cur.project.clone();
        let mut vm = cur.clone();
        let mut disk_writes: Vec<Disk> = Vec::new();
        let mut delta = Delta::default();

        if let Some(p) = patch.power {
            if cur.spec.power == PowerState::Stopped && p != PowerState::Stopped {
                delta.running_vms = 1;
            }
            vm.spec.power = p;
        }
        if let Some(r) = patch.restart_policy {
            vm.spec.restart_policy = r;
        }
        if let Some(h) = patch.on_host_boot {
            vm.spec.on_host_boot = h;
        }
        if let Some(g) = patch.stop_grace_secs {
            vm.spec.stop_grace_secs = g;
        }
        if let Some(c) = patch.config {
            let config = &mut vm.spec.config;
            if let Some(v) = c.vcpu_count {
                if v == 0 {
                    return Err(invalid("vcpu_count must be greater than 0"));
                }
                delta.vcpus = (v as u64).saturating_sub(config.vcpu_count as u64);
                config.vcpu_count = v;
            }
            if let Some(m) = c.mem_size_mib {
                if m == 0 {
                    return Err(invalid("mem_size_mib must be greater than 0"));
                }
                delta.memory_mib = (m as u64).saturating_sub(config.mem_size_mib as u64);
                config.mem_size_mib = m;
            }
            if let Some(a) = c.kernel_args {
                config.kernel_args = a;
            }
            if let Some(h) = c.hugepages {
                config.hugepages = h;
            }
            if let Some(cred) = c.credential {
                config.credential = match cred {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s),
                    _ => return Err(invalid("credential must be a username or null")),
                };
            }
            if let Some(devs) = c.vfio_devices {
                self.refuse_dpdk_uplink_devices(&devs)?;
                for d in &devs {
                    if let Some(other) = Self::device_claimed_by(&vms, d, Some(vm_id)) {
                        return Err(ImageError::InUse(format!("PCI device {} is used by VM {}", d, other)).into());
                    }
                }
                config.vfio_devices = devs;
            }
            if let Some(nets) = c.networks {
                let mut nets = nets;
                for (i, att) in nets.iter_mut().enumerate() {
                    if att.mac.is_none() {
                        att.mac = glidex_ovs::names::mac_address(&vm.id, i as u8).ok();
                    }
                }
                self.check_networks(&project, &nets)?;
                vm.spec.config.networks = nets;
            }
            if let Some(keys) = c.data_disks {
                let mut ids = Vec::new();
                for key in &keys {
                    let d = self.attachable_disk_in(&vms, key, &project, Some(vm_id))?;
                    if ids.contains(&d.id) || vm.spec.config.root_disk.as_ref() == Some(&d.id) {
                        return Err(invalid(format!("disk {} listed twice", d.name)));
                    }
                    ids.push(d.id.clone());
                    if d.attached_to.as_deref() != Some(vm_id) {
                        let mut d = d;
                        d.attached_to = Some(vm_id.to_string());
                        disk_writes.push(d);
                    }
                }
                for old in &cur.spec.config.data_disks {
                    if !ids.contains(old) {
                        if let Some(d) = self.release_claim_if_unused(&cur, old) {
                            disk_writes.push(d);
                        }
                    }
                }
                vm.spec.config.data_disks = ids;
            }
            self.check_paths(&vm.spec.config)?;
            self.check_credential(&project, &vm.spec.config)?;
        }
        if vm.spec == cur.spec {
            return Ok((cur, Vec::new()));
        }
        let mut bypassed = Vec::new();
        if delta.running_vms + delta.vcpus + delta.memory_mib > 0 {
            if let Some(p) = self.projects.get(&project)? {
                let usage = self.usage_locked(&project, &vms)?;
                Self::apply_quota(&p.quotas, &usage, &delta, quota, &mut bypassed)?;
            }
        }
        vm.generation += 1;
        vm.resource_version += 1;
        let ev = Event::new(actor, EventKind::Normal, "SpecChanged", "spec updated");
        self.store.commit(Commit {
            put_vm: Some(&vm),
            put_disks: disk_writes.iter().collect(),
            events: vec![(event_key("vm", &vm.id), ev)],
            ..Default::default()
        })?;
        for d in &disk_writes {
            self.images.cache_disk(d);
        }
        vms.insert(vm.id.clone(), vm.clone());
        self.notify_changed();
        self.queue.add(Key::Vm(vm.id.clone()));
        Ok((vm, bypassed))
    }

    /// A disk `vm` stops referencing keeps its claim while `vm`'s instance
    /// has it open (D11); otherwise its `attached_to` is cleared. Returns
    /// the disk record to write, if it changes.
    fn release_claim_if_unused(&self, vm: &Vm, disk_id: &str) -> Option<Disk> {
        if vm.status.instance.as_ref().is_some_and(|i| i.disks.iter().any(|d| d == disk_id)) {
            return None;
        }
        let mut d = self.images.get_disk(disk_id).ok()?;
        if d.attached_to.as_deref() == Some(vm.id.as_str()) {
            d.attached_to = None;
            Some(d)
        } else {
            None
        }
    }

    pub async fn attach_device(&self, vm_id: &str, device_path: String) -> Result<Vm, VmManagerError> {
        let vm = self.get_vm(vm_id).await?;
        if vm.config().vfio_devices.contains(&device_path) {
            return Err(VmManagerError::InvalidState {
                current: vm.state(),
                operation: format!("attach_device: {} is already attached", device_path),
            });
        }
        let mut devs = vm.config().vfio_devices.clone();
        devs.push(device_path);
        let patch = VmPatch { config: Some(ConfigPatch { vfio_devices: Some(devs), ..Default::default() }), ..Default::default() };
        self.patch_vm(vm_id, patch, QuotaMode::MayExceed, "api", None).await.map(|(vm, _)| vm)
    }

    pub async fn detach_device(&self, vm_id: &str, device_path: &str) -> Result<Vm, VmManagerError> {
        let vm = self.get_vm(vm_id).await?;
        let mut devs = vm.config().vfio_devices.clone();
        let Some(pos) = devs.iter().position(|d| d == device_path) else {
            return Err(VmManagerError::InvalidState {
                current: vm.state(),
                operation: format!("detach_device: {} is not attached", device_path),
            });
        };
        devs.remove(pos);
        let patch = VmPatch { config: Some(ConfigPatch { vfio_devices: Some(devs), ..Default::default() }), ..Default::default() };
        self.patch_vm(vm_id, patch, QuotaMode::MayExceed, "api", None).await.map(|(vm, _)| vm)
    }

    /// Attach a data disk; on a running VM it takes effect at the next
    /// launch (`RestartRequired`, §7.3).
    pub async fn attach_disk(&self, vm_id: &str, key: &str) -> Result<Vm, VmManagerError> {
        let vm = self.get_vm(vm_id).await?;
        let d = self.images.get_disk(key)?;
        let mut keys = vm.config().data_disks.clone();
        keys.push(d.id);
        let patch = VmPatch { config: Some(ConfigPatch { data_disks: Some(keys), ..Default::default() }), ..Default::default() };
        self.patch_vm(vm_id, patch, QuotaMode::MayExceed, "api", None).await.map(|(vm, _)| vm)
    }

    pub async fn detach_disk(&self, vm_id: &str, key: &str) -> Result<Vm, VmManagerError> {
        let vm = self.get_vm(vm_id).await?;
        let disk = self.images.get_disk(key)?;
        let mut keys = vm.config().data_disks.clone();
        let Some(pos) = keys.iter().position(|d| *d == disk.id) else {
            let msg = if vm.config().root_disk.as_ref() == Some(&disk.id) {
                format!("disk {} is the VM's root disk, not a data disk", disk.name)
            } else {
                format!("disk {} is not attached to this VM", disk.name)
            };
            return Err(VmManagerError::Image(ImageError::invalid_disk(msg)));
        };
        if let Some(op) = self.images.busy_op(&disk.id) {
            return Err(ImageError::Busy(format!("disk {} is busy ({})", disk.name, op)).into());
        }
        keys.remove(pos);
        let patch = VmPatch { config: Some(ConfigPatch { data_disks: Some(keys), ..Default::default() }), ..Default::default() };
        self.patch_vm(vm_id, patch, QuotaMode::MayExceed, "api", None).await.map(|(vm, _)| vm)
    }

    // ---- delete (§6.3) ---------------------------------------------------------

    #[allow(dead_code)] // used by the tests through the lib crate
    pub async fn delete_vm(&self, vm_id: &str) -> Result<bool, VmManagerError> {
        self.delete_vm_with(vm_id, false).await
    }

    /// Request deletion. Returns `true` when the VM is already gone (it had
    /// no instance, ports or anything else to clean up), `false` when the
    /// controller finishes it.
    pub async fn delete_vm_with(&self, vm_id: &str, keep_disk: bool) -> Result<bool, VmManagerError> {
        self.delete_vm_as(vm_id, keep_disk, "api").await
    }

    pub async fn delete_vm_as(&self, vm_id: &str, keep_disk: bool, actor: &str) -> Result<bool, VmManagerError> {
        let vm = {
            let mut vms = self.vms.write().await;
            let cur = vms.get(vm_id).ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?.clone();
            if cur.deletion_requested_at.is_some() {
                return Ok(false);
            }
            for id in cur.config().root_disk.iter().chain(cur.config().data_disks.iter()) {
                if let Some(op) = self.images.busy_op(id) {
                    return Err(ImageError::Busy(format!("disk {} is busy ({})", id, op)).into());
                }
            }
            let mut vm = cur;
            vm.deletion_requested_at = Some(tenancy::now());
            vm.finalizers = crate::controller::vm::FINALIZERS
                .iter()
                .filter(|f| !(keep_disk && **f == crate::controller::vm::FINALIZER_OWNED_DISK))
                .map(|f| f.to_string())
                .collect();
            vm.resource_version += 1;
            let ev = Event::new(actor, EventKind::Normal, "Deleting", if keep_disk { "deletion requested (keeping its root disk)" } else { "deletion requested" });
            self.put_locked(&mut vms, vm, vec![ev])?
        };
        // Nothing ran: finish now, so a never-started VM is gone at once.
        if vm.status.instance.is_none() && vm.status.nics.is_empty() && matches!(vm.status.phase, VmPhase::Stopped | VmPhase::Failed) {
            let runner = self.runner();
            let unit = runner.unit_active(&vm.id).await;
            let v = vm.clone();
            let seen = tokio::task::spawn_blocking(move || crate::instance::liveness(&v, unit))
                .await
                .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
            if seen.liveness == crate::instance::Liveness::Dead {
                self.finalize_deletion(&vm).await?;
                return Ok(true);
            }
        }
        self.queue.add(Key::Vm(vm.id.clone()));
        Ok(false)
    }

    // ---- reads -------------------------------------------------------------

    pub async fn get_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        self.vm(vm_id).await.ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))
    }

    pub async fn list_vms(&self) -> Vec<Vm> {
        self.vms.read().await.values().cloned().collect()
    }

    pub fn vm_events(&self, vm_id: &str) -> Result<Vec<Event>, VmManagerError> {
        Ok(self.store.events(&event_key("vm", vm_id))?)
    }

    /// Wait until the VM's reconcile of `generation` is decided (§12.3):
    /// converged or failed, gone (a deletion), or timed out.
    pub async fn wait_converged(&self, vm_id: &str, generation: u64, timeout: Duration) -> Waited {
        let mut rx = self.changed.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let vm = self.vm(vm_id).await;
            match &vm {
                None => return Waited::Gone,
                Some(v)
                    if v.deletion_requested_at.is_none()
                        && v.status.observed_generation >= generation
                        && (v.is_converged() || v.status.phase == VmPhase::Failed) =>
                {
                    return Waited::Decided(Box::new(v.clone()));
                }
                _ => {}
            }
            if tokio::time::timeout_at(deadline, rx.changed()).await.is_err() {
                return Waited::TimedOut(vm.map(Box::new));
            }
        }
    }

    /// Wait until disk `key` has settled (§12.3): made or failed, its
    /// resize and extend-root applied or reported pending. Returns whether
    /// it settled, and the disk (`None` once deleted).
    pub async fn wait_disk(&self, key: &str, timeout: Duration) -> (bool, Option<DiskResponse>) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let Ok(d) = self.images.get_disk(key) else { return (true, None) };
            let ready = d.conditions.iter().find(|c| c.kind == "Ready");
            let decided_pending = ready.is_some_and(|c| {
                matches!(c.reason.as_str(), "ResizePending" | "ResizeInvalid" | "ExtendRootPending" | "InvalidDisk" | "FileMissing")
            });
            let settled = d.deletion_requested_at.is_none()
                && match d.phase {
                    images::DiskPhase::Failed | images::DiskPhase::Missing => true,
                    images::DiskPhase::Ready => {
                        decided_pending
                            || (d.resize.is_none() && d.extend_root.is_none_or(|e| e.seq <= d.applied_extend_root_seq))
                    }
                    _ => false,
                };
            if settled || tokio::time::Instant::now() >= deadline {
                return (settled, Some(self.images.disk_response(&d, false)));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Wait (on the change bell) until `look` returns `None`, i.e. the
    /// object is gone, or `timeout` passes; returns the last thing seen.
    pub async fn wait_gone<T>(&self, timeout: Duration, mut look: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.changed.subscribe();
        loop {
            rx.borrow_and_update();
            let seen = look()?;
            // The bell, or a slow tick in case a change was missed.
            let tick = tokio::time::Instant::now() + Duration::from_secs(1);
            if tokio::time::timeout_at(deadline.min(tick), rx.changed()).await.is_err() && tokio::time::Instant::now() >= deadline {
                return Some(seen);
            }
        }
    }

    pub fn object_events(&self, kind: &str, id: &str) -> Result<Vec<Event>, VmManagerError> {
        Ok(self.store.events(&event_key(kind, id))?)
    }

    /// Set every VM to stopped and wait (tests, and tools that need a
    /// clean host). Not called on shutdown: VMs outlive the control plane.
    pub async fn stop_all(&self, timeout: Duration) {
        let ids: Vec<String> = self.vms.read().await.keys().cloned().collect();
        let mut gens = Vec::new();
        for id in ids {
            if let Ok((vm, _)) = self.set_power(&id, PowerState::Stopped, Some(0), QuotaMode::MayExceed, "controller", None).await {
                gens.push((id, vm.generation));
            }
        }
        for (id, g) in gens {
            self.wait_converged(&id, g, timeout).await;
        }
    }

    /// `(pending, in_flight)` of the controllers' work queue.
    pub fn queue_stats(&self) -> (usize, usize) {
        self.queue.stats()
    }

    /// Orphans found at startup.
    pub fn orphans(&self) -> Vec<Orphan> {
        self.orphans.lock().unwrap().clone()
    }

    // ---- images and disks (spec/images.md) -----------------------------

    pub fn image_catalog(&self) -> Vec<CatalogItem> {
        self.images.catalog()
    }

    /// Import the firmware available locally, once per catalog entry
    /// (`ImageManager::auto_import_firmware`); called at startup.
    pub async fn auto_import_firmware(&self) -> Vec<(&'static str, images::AutoImport)> {
        self.images.auto_import_firmware().await
    }

    /// The firmware image VMs of `hypervisor` boot through by default.
    pub fn default_firmware_name(&self, hypervisor: crate::hypervisor::HypervisorType) -> Option<String> {
        self.images.default_firmware(hypervisor).map(|i| i.name)
    }

    pub fn firmware_catalog(&self) -> Vec<images::FirmwareCatalogItem> {
        self.images.firmware_catalog()
    }

    /// An image response with the VMs booting through it.
    async fn with_users(&self, mut r: ImageResponse) -> ImageResponse {
        if r.kind == images::ImageKind::Firmware {
            r.used_by_vms = Self::firmware_users(&*self.vms.read().await, &r.id);
        }
        r
    }

    pub async fn list_images(&self) -> Vec<ImageResponse> {
        let vms = self.vms.read().await;
        self.images
            .list_images()
            .iter()
            .map(|i| {
                let mut r = self.images.image_response(i, false);
                if i.is_firmware() {
                    r.used_by_vms = Self::firmware_users(&vms, &i.id);
                }
                r
            })
            .collect()
    }

    pub async fn get_image(&self, key: &str) -> Result<ImageResponse, VmManagerError> {
        let img = self.images.get_image(key)?;
        let mgr = self.images.clone();
        let r = blocking(move || Ok(mgr.image_response(&img, true))).await?;
        Ok(self.with_users(r).await)
    }

    pub async fn pull_image(&self, req: PullImageRequest) -> Result<(ImageResponse, bool), VmManagerError> {
        let (img, created) = self.images.pull_image(req).await?;
        Ok((self.images.image_response(&img, false), created))
    }

    /// Delete an image (or cancel its download): refused while a disk
    /// depends on it; otherwise a deletion request the image controller
    /// finishes. `None` once gone, else the image still being deleted.
    pub async fn delete_image(&self, key: &str) -> Result<Option<ImageResponse>, VmManagerError> {
        let img = {
            // Held across the check and the request, so create_vm (which
            // holds the write lock) can't pick it up in between.
            let vms = self.vms.read().await;
            let img = self.images.get_image(key)?;
            let users = Self::firmware_users(&vms, &img.id);
            if !users.is_empty() {
                return Err(ImageError::InUse(format!("firmware {} is used by VM(s): {}", img.name, users.join(", "))).into());
            }
            self.images.request_image_delete(key)?
        };
        self.reconcile_image(&img.id).await?;
        Ok(self.images.get_image(&img.id).ok().map(|i| self.images.image_response(&i, false)))
    }

    pub fn list_disks(&self) -> Vec<DiskResponse> {
        self.images.list_disks().iter().map(|d| self.images.disk_response(d, false)).collect()
    }

    /// Ids of the disks attached to a VM, from the records alone (no
    /// per-disk response or file check).
    pub fn attached_disk_ids(&self, vm_id: &str) -> std::collections::BTreeSet<String> {
        self.images.list_disks().into_iter().filter(|d| d.attached_to.as_deref() == Some(vm_id)).map(|d| d.id).collect()
    }

    pub async fn get_disk(&self, key: &str) -> Result<DiskResponse, VmManagerError> {
        let d = self.images.get_disk(key)?;
        let mgr = self.images.clone();
        Ok(blocking(move || Ok(mgr.disk_response(&d, true))).await?)
    }

    /// Create a disk in the default project (embedding and tests).
    pub async fn create_disk(&self, req: CreateDiskRequest) -> Result<DiskResponse, VmManagerError> {
        let project = self.default_project_id();
        self.create_disk_in(&project, req, QuotaMode::MayExceed).await.map(|(d, _)| d)
    }

    /// What a pending disk will take, for quotas: its requested size, else
    /// its image's size or the default root size.
    fn estimated_size(&self, d: &Disk) -> u64 {
        let requested = d.create.as_ref().and_then(|c| c.size_bytes);
        match &d.origin {
            images::DiskOrigin::Blank => requested.unwrap_or(d.size_bytes),
            images::DiskOrigin::Image { image_id, .. } => {
                let image_size = self.images.get_image(image_id).map(|i| i.virtual_size_bytes).unwrap_or(0);
                self.images.root_size(requested, image_size)
            }
        }
    }

    /// Create a disk in `project`, checking its `disk_gib` quota. The disk
    /// is recorded pending; the disk controller makes its file
    /// (spec/reconciliation.md §10.1).
    pub async fn create_disk_in(
        &self,
        project: &str,
        mut req: CreateDiskRequest,
        quota: QuotaMode,
    ) -> Result<(DiskResponse, Vec<QuotaOverrun>), VmManagerError> {
        let p = self.projects.get(project)?.ok_or_else(|| TenancyError::NotFound(project.to_string()))?;
        req.project = Some(project.to_string());
        // The VM lock serializes quota checks with VM creation.
        let vms = self.vms.write().await;
        let disk = self.images.new_disk_record(&req)?;
        let usage = self.usage_locked(project, &vms)?;
        let mut bypassed = Vec::new();
        let delta = Delta { disk_gib: gib_ceil(self.estimated_size(&disk)), ..Default::default() };
        Self::apply_quota(&p.quotas, &usage, &delta, quota, &mut bypassed)?;
        self.store.commit(Commit {
            put_disks: vec![&disk],
            events: vec![(event_key("disk", &disk.id), Event::new("api", EventKind::Normal, "Created", "disk recorded; its file is made next"))],
            ..Default::default()
        })?;
        self.images.cache_disk(&disk);
        drop(vms);
        self.queue.add(Key::Disk(disk.id.clone()));
        Ok((self.images.disk_response(&disk, false), bypassed))
    }

    /// Whether a live instance (or a VM being launched) has disk `id` open:
    /// then it is not resized or edited (§10.1).
    pub(crate) fn disk_in_use(vms: &HashMap<String, Vm>, id: &str) -> Option<String> {
        vms.values()
            .find(|vm| {
                vm.status.instance.as_ref().is_some_and(|i| i.disks.iter().any(|d| d == id))
                    || (matches!(vm.status.phase, VmPhase::Provisioning | VmPhase::Starting)
                        && (vm.config().root_disk.as_deref() == Some(id) || vm.config().data_disks.iter().any(|d| d == id)))
            })
            .map(|vm| vm.name.clone())
    }

    /// Mark a disk busy for `op`, refusing while a live instance has it
    /// open. Holds the VM read lock while checking, so a launch (which
    /// records the instance's disks under the write lock) cannot slip in
    /// between the check and the mark.
    pub(crate) async fn begin_disk_op(&self, key: &str, op: &'static str) -> Result<(Disk, images::BusyGuard), VmManagerError> {
        let vms = self.vms.read().await;
        let disk = self.images.get_disk(key)?;
        if let Some(vm) = Self::disk_in_use(&vms, &disk.id) {
            return Err(ImageError::InUse(format!("disk {} is in use by VM {}; stop it first", disk.name, vm)).into());
        }
        let guard = self.images.begin(&disk.id, op)?;
        Ok((disk, guard))
    }

    pub async fn resize_disk(&self, key: &str, req: ResizeDiskRequest) -> Result<DiskResponse, VmManagerError> {
        self.resize_disk_with(key, req, QuotaMode::MayExceed).await.map(|(d, _)| d)
    }

    /// Resize a disk (a spec write, §10.1), checking its project's
    /// `disk_gib` quota on growth and, for a shrink, that it would not cut
    /// into a partition. Applied by the disk controller once no instance
    /// has the disk open.
    pub async fn resize_disk_with(
        &self,
        key: &str,
        req: ResizeDiskRequest,
        quota: QuotaMode,
    ) -> Result<(DiskResponse, Vec<QuotaOverrun>), VmManagerError> {
        let size = requested_size(req.size_gib, req.size_bytes)?.ok_or_else(|| ImageError::invalid_disk("give size_gib or size_bytes"))?;
        let current = self.images.get_disk(key)?;
        if current.deletion_requested_at.is_some() {
            return Err(ImageError::InUse(format!("disk {} is being deleted", current.name)).into());
        }
        if !matches!(current.phase, images::DiskPhase::Ready | images::DiskPhase::Resizing) {
            return Err(ImageError::NotReady(format!("disk {} is {:?}", current.name, current.phase)).into());
        }
        let mut bypassed = Vec::new();
        let grow = gib_ceil(size).saturating_sub(gib_ceil(current.size_bytes));
        if grow > 0 {
            if let Some(p) = self.projects.get(&current.project)? {
                let vms = self.vms.read().await;
                let usage = self.usage_locked(&current.project, &vms)?;
                Self::apply_quota(&p.quotas, &usage, &Delta { disk_gib: grow, ..Default::default() }, quota, &mut bypassed)?;
            }
        }
        if size < current.size_bytes {
            let (mgr, d) = (self.images.clone(), current.clone());
            let (min, _) = blocking(move || mgr.shrink_minimum(&d)).await?;
            if size < min {
                return Err(ImageError::InvalidDisk {
                    message: format!("{} bytes would cut into a partition; the minimum is {} bytes", size, min),
                    details: serde_json::json!({ "min_size_bytes": min }),
                }
                .into());
            }
        }
        let mut disk = current;
        disk.resize = (size != disk.size_bytes).then_some(images::ResizeSpec { size_bytes: size, extend_root: req.extend_root });
        self.store.commit(Commit {
            put_disks: vec![&disk],
            events: vec![(event_key("disk", &disk.id), Event::new("api", EventKind::Normal, "ResizeRequested", format!("size {} bytes", size)))],
            ..Default::default()
        })?;
        self.images.cache_disk(&disk);
        self.queue.add(Key::Disk(disk.id.clone()));
        Ok((self.images.disk_response(&disk, false), bypassed))
    }

    /// Request a root-partition extend (a one-shot action, D19): bumps the
    /// disk's `extend_root.seq`. A disk without a growable root partition
    /// is refused here.
    pub async fn extend_root(&self, key: &str, mode: ExtendMode) -> Result<DiskResponse, VmManagerError> {
        let current = self.images.get_disk(key)?;
        if current.phase != images::DiskPhase::Ready || current.deletion_requested_at.is_some() {
            return Err(ImageError::NotReady(format!("disk {} is {:?}", current.name, current.phase)).into());
        }
        let (mgr, d) = (self.images.clone(), current.clone());
        blocking(move || {
            let path = mgr.disk_path(&d);
            let table = images::partition::read_table(&path, d.format, d.size_bytes)?
                .ok_or_else(|| ImageError::invalid_disk("disk has no partition table"))?;
            images::partition::growable_root(&table).map(|_| ())
        })
        .await?;
        let mut disk = current;
        let seq = disk.extend_root.map(|e| e.seq).unwrap_or(0).max(disk.applied_extend_root_seq) + 1;
        disk.extend_root = Some(images::ExtendRootSpec { mode, seq });
        self.store.commit(Commit {
            put_disks: vec![&disk],
            events: vec![(event_key("disk", &disk.id), Event::new("api", EventKind::Normal, "ExtendRootRequested", format!("{:?}, request {}", mode, seq)))],
            ..Default::default()
        })?;
        self.images.cache_disk(&disk);
        self.queue.add(Key::Disk(disk.id.clone()));
        Ok(self.images.disk_response(&disk, false))
    }

    /// Delete a disk: refused while a VM claims it; otherwise a deletion
    /// request the disk controller finishes (its record, then its file).
    /// Returns once it is gone, unless an operation on it is running.
    pub async fn delete_disk(&self, key: &str) -> Result<(), VmManagerError> {
        let disk = {
            // Write lock: no VM may claim the disk while it goes away.
            let vms = self.vms.write().await;
            let disk = self.images.get_disk(key)?;
            if let Some(vm) = Self::disk_claimed_by(&vms, &disk.id, None) {
                return Err(ImageError::InUse(format!("disk {} is attached to VM {}; detach it or delete the VM first", disk.name, vm)).into());
            }
            if let Some(op) = self.images.busy_op(&disk.id) {
                return Err(ImageError::Busy(format!("disk {} is busy ({})", disk.name, op)).into());
            }
            let mut d = disk;
            d.deletion_requested_at.get_or_insert(tenancy::now());
            self.images.put_disk(&d)?;
            d
        };
        self.reconcile_disk(&disk.id).await?;
        Ok(())
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

    pub fn list_networks(&self) -> Result<Vec<Network>, VmManagerError> {
        Ok(self.networks.list()?)
    }

    pub fn get_network(&self, name: &str) -> Result<Network, VmManagerError> {
        Ok(self.networks.get(name)?.ok_or_else(|| NetError::NotFound(name.to_string()))?)
    }

    /// Create a network: host side in netd first, record only on success.
    pub async fn create_network(&self, req: CreateNetworkRequest) -> Result<Network, VmManagerError> {
        let net = req.to_network()?;
        if let Some(other) = self.networks.get(&net.name)? {
            return Err(NetError::Conflict(if other.deletion_requested_at.is_some() {
                format!("network '{}' is being deleted; try again once it is gone", net.name)
            } else {
                format!("network '{}' already exists", net.name)
            })
            .into());
        }
        if let Some(other) = self.networks.list()?.into_iter().find(|n| n.bridge == net.bridge) {
            return Err(NetError::Conflict(format!("bridge '{}' already belongs to network '{}'", net.bridge, other.name)).into());
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
                    // netd fences an isolated network's bridge off from the host.
                    isolated: net.mode == NetworkMode::Isolated,
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
        self.networks.insert(&net)?;
        tracing::info!(network = %net.name, bridge = %net.bridge, mode = ?net.mode, "network created");
        self.queue.add(Key::Network(net.name.clone()));
        Ok(net)
    }

    /// VMs whose spec or live NICs use network `name`.
    pub(crate) fn network_users(vms: &HashMap<String, Vm>, name: &str) -> Vec<String> {
        vms.values()
            .filter(|vm| vm.config().networks.iter().any(|a| a.network == name) || vm.status.nics.iter().any(|n| n.network == name))
            .map(|vm| vm.name.clone())
            .collect()
    }

    /// Delete a network: refused while a VM uses it; otherwise a deletion
    /// request the network controller finishes (netd's NAT and bridge for a
    /// network glidex created, then the record). Returns the network while
    /// that is still under way (netd unreachable, say), `None` once gone.
    pub async fn delete_network(&self, name: &str) -> Result<Option<Network>, VmManagerError> {
        {
            // Write lock: no VM may attach to it while it starts going away.
            let vms = self.vms.write().await;
            let mut net = self.get_network(name)?;
            let users = Self::network_users(&vms, name);
            if !users.is_empty() {
                return Err(NetError::Conflict(format!("network '{}' is used by VM(s): {}", name, users.join(", "))).into());
            }
            if net.deletion_requested_at.is_none() {
                net.deletion_requested_at = Some(tenancy::now());
                self.networks.put(&net)?;
            }
        }
        self.reconcile_network(name).await?;
        Ok(self.networks.get(name)?)
    }

    /// Create a host network usable by `grants` (or every project).
    pub async fn create_host_network(
        &self,
        req: CreateNetworkRequest,
        grants: Vec<String>,
        all_projects: bool,
    ) -> Result<Network, VmManagerError> {
        for g in &grants {
            if self.projects.get(g)?.is_none() {
                return Err(TenancyError::NotFound(g.clone()).into());
            }
        }
        let mut net = self.create_network(req).await?;
        net.grants = grants;
        net.all_projects = all_projects;
        self.networks.put(&net)?;
        Ok(net)
    }

    /// Create a network private to `project` (spec/security.md §6.2):
    /// NAT or isolated (no uplink), either port type, generated bridge
    /// name, counted against the `networks` quota.
    pub async fn create_project_network(
        &self,
        project: &str,
        mut req: CreateNetworkRequest,
        quota: QuotaMode,
    ) -> Result<(Network, Vec<QuotaOverrun>), VmManagerError> {
        let p = self.projects.get(project)?.ok_or_else(|| TenancyError::NotFound(project.to_string()))?;
        if req.mode == NetworkMode::Bridged {
            return Err(NetError::Invalid("project networks are NAT or isolated; bridged networks are host networks".into()).into());
        }
        if req.bridge.is_some() || req.vlan.is_some() {
            return Err(NetError::Invalid("project networks choose their own bridge; bridge and vlan can't be set".into()).into());
        }
        let vms = self.vms.write().await;
        let mut bypassed = Vec::new();
        let usage = self.usage_locked(project, &vms)?;
        Self::apply_quota(&p.quotas, &usage, &Delta { networks: 1, ..Default::default() }, quota, &mut bypassed)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        req.bridge = Some(format!("gxp-{}", &id[..8]));
        let mut net = self.create_network(req).await?;
        net.project = Some(project.to_string());
        self.networks.put(&net)?;
        drop(vms);
        Ok((net, bypassed))
    }

    /// Set which projects may use a host network.
    pub fn grant_network(&self, name: &str, grants: Vec<String>, all_projects: bool) -> Result<Network, VmManagerError> {
        let mut net = self.get_network(name)?;
        if net.project.is_some() {
            return Err(NetError::Invalid("project networks are shared, not granted".into()).into());
        }
        for g in &grants {
            if self.projects.get(g)?.is_none() {
                return Err(TenancyError::NotFound(g.clone()).into());
            }
        }
        net.grants = grants;
        net.all_projects = all_projects;
        self.networks.put(&net)?;
        Ok(net)
    }

    /// Offer a project network to `target`. Silently succeeds when the
    /// target doesn't exist, so offers can't probe for projects.
    pub fn offer_network_share(&self, name: &str, target: &str, by: &str) -> Result<(), VmManagerError> {
        let mut net = self.get_network(name)?;
        if net.project.is_none() {
            return Err(NetError::Invalid("only project networks can be shared".into()).into());
        }
        if self.projects.get(target)?.is_none() || net.shares.iter().any(|s| s == target) {
            return Ok(());
        }
        let now = tenancy::now();
        net.share_offers.retain(|o| o.project != target && o.expires_at > now);
        net.share_offers.push(network::ShareOffer {
            project: target.to_string(),
            offered_by: by.to_string(),
            offered_at: now,
            expires_at: now + network::SHARE_OFFER_SECS,
        });
        self.networks.put(&net)?;
        Ok(())
    }

    /// Networks with an open offer to, or an accepted share with, `project`.
    pub fn network_shares_for(&self, project: &str) -> Result<Vec<Network>, VmManagerError> {
        let now = tenancy::now();
        Ok(self
            .networks
            .list()?
            .into_iter()
            .filter(|n| n.shares.iter().any(|s| s == project) || n.share_offers.iter().any(|o| o.project == project && o.expires_at > now))
            .collect())
    }

    /// Accept an open offer of `name` to `project`.
    pub fn accept_network_share(&self, project: &str, name: &str) -> Result<Network, VmManagerError> {
        let mut net = self.get_network(name)?;
        let now = tenancy::now();
        let Some(pos) = net.share_offers.iter().position(|o| o.project == project && o.expires_at > now) else {
            return Err(NetError::NotFound(format!("no open share offer of network '{}' to this project", name)).into());
        };
        net.share_offers.remove(pos);
        if !net.shares.iter().any(|s| s == project) {
            net.shares.push(project.to_string());
        }
        self.networks.put(&net)?;
        Ok(net)
    }

    /// End a share (unshare by the owner, or leave by the target).
    /// Refused while VMs of `project` use the network.
    pub async fn end_network_share(&self, name: &str, project: &str) -> Result<(), VmManagerError> {
        let vms = self.vms.read().await;
        let mut net = self.get_network(name)?;
        let users: Vec<String> = vms
            .values()
            .filter(|vm| vm.project == project && vm.config().networks.iter().any(|a| a.network == name))
            .map(|vm| vm.name.clone())
            .collect();
        if !users.is_empty() {
            return Err(NetError::Conflict(format!("network '{}' is used by VM(s) of that project: {}", name, users.join(", "))).into());
        }
        let before = (net.shares.len(), net.share_offers.len());
        net.shares.retain(|s| s != project);
        net.share_offers.retain(|o| o.project != project);
        if before == (net.shares.len(), net.share_offers.len()) {
            return Err(NetError::NotFound(format!("network '{}' isn't shared with that project", name)).into());
        }
        self.networks.put(&net)?;
        Ok(())
    }

    /// Create the `default` NAT network if netd is usable and it's missing.
    pub async fn ensure_default_network(&self) -> Result<Option<Network>, VmManagerError> {
        if self.networks.get(network::DEFAULT_NETWORK)?.is_some() {
            return Ok(None);
        }
        let (access, caps) = self.netd.probe();
        let running = caps.ok().and_then(|c| c.get("ovs_running").and_then(|v| v.as_bool())).unwrap_or(false);
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
        let default_project = self.default_project_id();
        self.create_host_network(req, vec![default_project], false).await.map(Some)
    }

    /// The control-plane database (shared with the identity store).
    pub fn database(&self) -> Arc<crate::store::Db> {
        self.store.database()
    }

    pub fn netd(&self) -> &Netd {
        &self.netd
    }

    // ---- credentials -----------------------------------------------------

    /// Credentials of `project`, or of every project.
    pub fn list_credentials_in(&self, project: Option<&str>) -> Result<Vec<Credential>, VmManagerError> {
        Ok(self.credentials.list(project)?)
    }

    pub fn list_credentials(&self) -> Result<Vec<Credential>, VmManagerError> {
        self.list_credentials_in(Some(&self.default_project_id()))
    }

    pub fn get_credential_in(&self, project: &str, username: &str) -> Result<Credential, VmManagerError> {
        Ok(self.credentials.get(project, username)?)
    }

    pub fn get_credential(&self, username: &str) -> Result<Credential, VmManagerError> {
        self.get_credential_in(&self.default_project_id(), username)
    }

    pub fn create_credential_in(&self, project: &str, req: CreateCredentialRequest) -> Result<Credential, VmManagerError> {
        if self.projects.get(project)?.is_none() {
            return Err(TenancyError::NotFound(project.to_string()).into());
        }
        let cred = self.credentials.create(project, req)?;
        tracing::info!(username = %cred.username, project, "Credential created");
        Ok(cred)
    }

    pub fn create_credential(&self, req: CreateCredentialRequest) -> Result<Credential, VmManagerError> {
        self.create_credential_in(&self.default_project_id(), req)
    }

    /// Changes reach a VM only on its first boot: cloud-init provisions
    /// users once per instance-id, and a VM keeps its id for life.
    pub fn update_credential_in(&self, project: &str, username: &str, req: UpdateCredentialRequest) -> Result<Credential, VmManagerError> {
        let cred = self.credentials.update(project, username, req)?;
        tracing::info!(username = %cred.username, project, "Credential updated");
        Ok(cred)
    }

    pub fn update_credential(&self, username: &str, req: UpdateCredentialRequest) -> Result<Credential, VmManagerError> {
        self.update_credential_in(&self.default_project_id(), username, req)
    }

    /// Refuses to delete a credential still referenced by a VM.
    pub async fn delete_credential_in(&self, project: &str, username: &str) -> Result<(), VmManagerError> {
        let vms = self.vms.read().await;
        let users: Vec<String> = vms
            .values()
            .filter(|vm| vm.project == project && vm.config().credential.as_deref() == Some(username))
            .map(|vm| vm.name.clone())
            .collect();
        if !users.is_empty() {
            return Err(VmManagerError::CredentialInUse { username: username.to_string(), vms: users });
        }
        self.credentials.delete(project, username)?;
        tracing::info!(username = %username, project, "Credential deleted");
        Ok(())
    }

    pub async fn delete_credential(&self, username: &str) -> Result<(), VmManagerError> {
        self.delete_credential_in(&self.default_project_id(), username).await
    }

    // ---- projects and quotas (spec/security.md §6) ---------------------

    pub fn projects(&self) -> &ProjectStore {
        &self.projects
    }

    pub fn default_project_id(&self) -> String {
        self.projects.default_project_id()
    }

    /// Assign records from before projects to the default project, once.
    fn adopt_into_default_project(&self) -> Result<(), VmManagerError> {
        if self.projects.meta(tenancy::META_TENANCY_V1)?.is_some() {
            return Ok(());
        }
        let default = self.default_project_id();
        for mut d in self.images.list_disks() {
            if d.project.is_empty() {
                d.project = default.clone();
                self.images.put_disk(&d)?;
            }
        }
        let moved = self.credentials.adopt_unscoped(&default)?;
        let mut nets = 0;
        for mut n in self.networks.list()? {
            if n.project.is_none() && !n.all_projects && n.grants.is_empty() {
                n.grants.push(default.clone());
                self.networks.put(&n)?;
                nets += 1;
            }
        }
        self.projects.set_meta(tenancy::META_TENANCY_V1, b"1")?;
        tracing::info!(credentials = moved, networks = nets, "assigned existing resources to the default project");
        Ok(())
    }

    /// A project's current use. `running_vms` counts VMs whose desired
    /// state is not `stopped` (§12.2), so crash loops and host reboots can
    /// never exceed it.
    fn usage_locked(&self, project: &str, vms: &HashMap<String, Vm>) -> Result<Usage, VmManagerError> {
        let mut u = Usage::default();
        for vm in vms.values().filter(|vm| vm.project == project) {
            u.vms += 1;
            u.vcpus += vm.config().vcpu_count as u64;
            u.memory_mib += vm.config().mem_size_mib as u64;
            if vm.spec.power != PowerState::Stopped {
                u.running_vms += 1;
            }
        }
        u.disk_gib = self.images.list_disks().iter().filter(|d| d.project == project).map(|d| gib_ceil(d.size_bytes)).sum();
        u.networks = self.networks.list()?.iter().filter(|n| n.project.as_deref() == Some(project)).count() as u64;
        Ok(u)
    }

    pub async fn usage(&self, project: &str) -> Result<Usage, VmManagerError> {
        let vms = self.vms.read().await;
        self.usage_locked(project, &vms)
    }

    /// Refuse `delta` if it goes over `quotas`, unless the caller may
    /// exceed them; then record what was exceeded in `bypassed`.
    fn apply_quota(
        quotas: &tenancy::Quotas,
        usage: &Usage,
        delta: &Delta,
        mode: QuotaMode,
        bypassed: &mut Vec<QuotaOverrun>,
    ) -> Result<(), VmManagerError> {
        let over = tenancy::overruns(quotas, usage, delta);
        if over.is_empty() {
            return Ok(());
        }
        match mode {
            QuotaMode::Enforce => Err(VmManagerError::QuotaExceeded(over)),
            QuotaMode::MayExceed => {
                bypassed.extend(over);
                Ok(())
            }
        }
    }

    /// Whether a project still owns anything (it can't be deleted then).
    pub async fn project_in_use(&self, project: &str) -> Result<Option<String>, VmManagerError> {
        let vms = self.vms.read().await;
        if let Some(vm) = vms.values().find(|vm| vm.project == project) {
            return Ok(Some(format!("VM {}", vm.name)));
        }
        if let Some(d) = self.images.list_disks().iter().find(|d| d.project == project) {
            return Ok(Some(format!("disk {}", d.name)));
        }
        if let Some(c) = self.credentials.list(Some(project))?.first() {
            return Ok(Some(format!("credential {}", c.username)));
        }
        if let Some(n) = self.networks.list()?.iter().find(|n| n.project.as_deref() == Some(project)) {
            return Ok(Some(format!("network {}", n.name)));
        }
        Ok(None)
    }

    /// Remove a deleted project from network grants and shares.
    pub fn forget_project(&self, project: &str) -> Result<(), VmManagerError> {
        for mut n in self.networks.list()? {
            let before = (n.grants.len(), n.shares.len(), n.share_offers.len());
            n.grants.retain(|p| p != project);
            n.shares.retain(|p| p != project);
            n.share_offers.retain(|o| o.project != project);
            if before != (n.grants.len(), n.shares.len(), n.share_offers.len()) {
                self.networks.put(&n)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn power_name(p: PowerState) -> &'static str {
    match p {
        PowerState::Running => "running",
        PowerState::Paused => "paused",
        PowerState::Stopped => "stopped",
    }
}

/// Run blocking image work off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ImageError> + Send + 'static) -> Result<T, ImageError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ImageError::Io(format!("task failed: {}", e)))?
}

/// Size in GiB, rounded up.
fn gib_ceil(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 30)
}
