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

use crate::controller::Settings;
use crate::controller::queue::{Key, WorkQueue};
use crate::credentials::{
    CreateCredentialRequest, Credential, CredentialError, CredentialStore, UpdateCredentialRequest,
};
use crate::hypervisor::{HypervisorError, HypervisorType, check_command_line_path};
use crate::images::{
    self, CatalogItem, CreateDiskRequest, Disk, DiskResponse, ExtendMode, ImageError, ImageManager,
    ImageResponse, ImageSettings, PullImageRequest, ResizeDiskRequest, disk::requested_size,
};
use crate::instance::runner::Runner;
use crate::models::{
    DiskSelection, HostBootPolicy, MAX_STOP_GRACE_SECS, NetworkAttachment, PowerState,
    RestartPolicy, Vm, VmConfig, VmPhase, VmState, VolumeBinding,
};
use crate::network::{
    self, CreateNetworkRequest, NetError, Netd, Network, NetworkMode, NetworkStore,
};
use crate::store::{Commit, Event, EventKind, PersistenceError, VmStore, event_key};
use crate::tenancy::{self, Delta, ProjectStore, QuotaMode, QuotaOverrun, TenancyError, Usage};
use arc_swap::ArcSwap;
use glidex_netd::proto::{BridgeRecord, NatInfo, Op};
use glidex_ovs::bridge::{BridgeSpec, Datapath};
use glidex_ovs::nat::NatSpec;
use glidex_ovs::vm_port::VmPortKind;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::RwLock;

#[derive(Debug)]
pub enum VmManagerError {
    VmNotFound(String),
    VmAlreadyExists(String),
    InvalidState {
        current: VmState,
        operation: String,
    },
    HypervisorError(HypervisorError),
    PersistenceError(String),
    HypervisorNotAvailable(HypervisorType),
    Credential(CredentialError),
    CredentialInUse {
        username: String,
        vms: Vec<String>,
    },
    Network(NetError),
    Image(ImageError),
    Datastore(crate::datastore::record::DatastoreError),
    /// The request would go over these project quotas.
    QuotaExceeded(Vec<QuotaOverrun>),
    /// The request would go over the tenant's cap on this cluster
    /// (spec/tenancy.md D19).
    TenantQuotaExceeded(Vec<QuotaOverrun>),
    Tenancy(TenancyError),
    /// The VM is being deleted: its spec no longer changes (§6.3).
    Deleting(String),
    /// `If-Match` named another resource version (§12.1).
    PreconditionFailed {
        expected: u64,
        actual: u64,
    },
}

pub(crate) fn router_err(e: crate::router::RouterError) -> VmManagerError {
    use crate::router::RouterError as R;
    VmManagerError::Network(match e {
        R::NotFound(m) => NetError::NotFound(m),
        R::Invalid(m) => NetError::Invalid(m),
        R::Conflict(m) => NetError::Conflict(m),
        R::PoolExhausted => NetError::ExternalPoolExhausted,
        R::NoZone => NetError::Conflict(e.to_string()),
        R::Storage(m) => NetError::Storage(m),
    })
}

impl From<crate::store::StoreError> for VmManagerError {
    fn from(e: crate::store::StoreError) -> Self {
        VmManagerError::PersistenceError(e.to_string())
    }
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
            VmManagerError::HypervisorNotAvailable(h) => {
                write!(f, "Hypervisor not available: {:?}", h)
            }
            VmManagerError::Credential(e) => write!(f, "{}", e),
            VmManagerError::Network(e) => write!(f, "{}", e),
            VmManagerError::Image(e) => write!(f, "{}", e),
            VmManagerError::Datastore(e) => write!(f, "{}", e),
            VmManagerError::QuotaExceeded(o) => {
                write!(
                    f,
                    "{}",
                    o.iter()
                        .map(|o| o.to_string())
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            }
            VmManagerError::TenantQuotaExceeded(o) => {
                write!(
                    f,
                    "tenant cap on this cluster: {}",
                    o.iter()
                        .map(|o| o.to_string())
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            }
            VmManagerError::Tenancy(e) => write!(f, "{}", e),
            VmManagerError::CredentialInUse { username, vms } => {
                write!(
                    f,
                    "credential {} is used by VM(s): {}",
                    username,
                    vms.join(", ")
                )
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

impl From<crate::datastore::record::DatastoreError> for VmManagerError {
    fn from(e: crate::datastore::record::DatastoreError) -> Self {
        VmManagerError::Datastore(e)
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
    /// Pin the VM to a node (id or name).
    pub node: Option<String>,
    /// `VmSpec::device_nodes`, from the caller's grants.
    pub device_nodes: Option<Vec<String>>,
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
    /// `VmSpec::device_nodes` again, set by the service with new
    /// `vfio_devices`; never from the request body.
    #[serde(skip)]
    pub device_nodes: Option<Option<Vec<String>>>,
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
    /// `2M`, `1G`, or `null` for the node's default size.
    #[serde(default)]
    pub hugepage_size: Option<serde_json::Value>,
    #[serde(default)]
    pub numa_policy: Option<crate::models::NumaPolicy>,
    #[serde(default)]
    pub cpu_policy: Option<crate::models::CpuPolicy>,
    /// Pool requests, replacing the list.
    #[serde(default)]
    pub devices: Option<Vec<crate::models::DeviceRequest>>,
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
    pub(crate) tenants: crate::tenants::TenantStore,
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
    sched: ArcSwap<crate::cluster::config::SchedulerConfig>,
    reserved: ArcSwap<crate::cluster::config::NodeReserved>,
    /// `pci.pools` of the control-plane config (spec/host-topology.md §9).
    pci_pools: ArcSwap<Vec<crate::config::PciPool>>,
    /// What this host runs OVN with: the cluster setting (if written) over
    /// the file's per-host part (spec/clustering-ui.md §8a).
    pub(crate) ovn: ArcSwap<crate::cluster::config::OvnConfig>,
    /// `cluster.ovn` as this host's file has it.
    ovn_file: ArcSwap<crate::cluster::config::OvnConfig>,
    /// The file's cluster-wide OVN fields were reported as ignored.
    ovn_file_warned: std::sync::atomic::AtomicBool,
    nodes: crate::node::NodeStore,
    roles: std::sync::Mutex<crate::node::Roles>,
    db_path: PathBuf,
    /// This host's cluster membership, once it has one (spec/clustering.md).
    cluster: arc_swap::ArcSwapOption<crate::cluster::Cluster>,
    /// The API router, handed to the cluster for forwarded requests.
    api_router: std::sync::OnceLock<axum::Router>,
    secrets_key: std::sync::OnceLock<Arc<dyn crate::cluster::runtime::SecretsKey>>,
    node_link: std::sync::OnceLock<Arc<crate::cluster::agent::NodeLink>>,
    /// The controllers' tasks, aborted by `stop_controllers`.
    pub(crate) tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Resource usage metering (spec/metering.md), once started.
    pub(crate) meter: arc_swap::ArcSwapOption<crate::metering::Meter>,
    pub(crate) meter_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub(crate) metering_cfg: std::sync::Mutex<Option<crate::config::MeteringConfig>>,
    pub(crate) me: Weak<VmManager>,
}

type RootBinding = (VolumeBinding, Disk);

/// A disk name derived from a VM name (`<vm>-root`), made valid.
fn root_disk_name(vm_name: &str) -> String {
    let base: String = vm_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '-'
            }
        })
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
    pub fn with_db_path_and_netd(
        db_path: PathBuf,
        netd: Netd,
    ) -> Result<Arc<Self>, VmManagerError> {
        // The cluster files live beside it (paths::file_in).
        crate::paths::checked_path(&db_path).map_err(VmManagerError::PersistenceError)?;
        let store = VmStore::open(&db_path)?;
        Self::build(db_path, store, netd, None)
    }

    /// As [`with_db_path_and_netd`](Self::with_db_path_and_netd), for a host
    /// that may be in a cluster: if it has a cluster identity, its Raft
    /// member is started before anything touches the store, so no write ever
    /// bypasses the log (spec/clustering.md §6.1).
    pub async fn open(
        db_path: PathBuf,
        netd: Netd,
        cfg: &crate::config::Config,
    ) -> Result<Arc<Self>, VmManagerError> {
        crate::paths::checked_path(&db_path).map_err(VmManagerError::PersistenceError)?;
        // A departure that committed before the last restart finishes first (§5.8.2).
        match crate::cluster::departure::finish_pending(&db_path) {
            Ok(true) => tracing::warn!(
                "finished leaving the cluster: this host is standalone and owns its VMs"
            ),
            Ok(false) => {}
            Err(e) => tracing::error!("a pending departure could not be finished: {}", e),
        }
        let store = VmStore::open(&db_path)?;
        let files = crate::cluster::identity::Files::beside(&db_path);
        let cluster = match files
            .load_identity()
            .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?
        {
            Some(identity) => {
                crate::authz::set_identity(&identity.cluster_id, &identity.node_id);
                crate::authz::set_local_account_scope(
                    (!cfg.cluster.shared_local_accounts).then(|| identity.node_id.clone()),
                );
                let listen = cfg.cluster.listen;
                let c = crate::cluster::Cluster::start(
                    store.database(),
                    files,
                    identity,
                    cfg.cluster.clone(),
                    listen,
                )
                .await
                .map_err(|e| VmManagerError::PersistenceError(format!("cluster: {e}")))?;
                // A restarting node waits briefly for a leader; without one the
                // control plane still comes up and serves stale reads.
                if c.wait_for_leader(std::time::Duration::from_secs(10))
                    .await
                    .is_none()
                {
                    tracing::warn!(
                        "no cluster leader yet: serving local, possibly stale, state until a quorum is back"
                    );
                }
                Some(c)
            }
            None => None,
        };
        Self::build(db_path, store, netd, cluster)
    }

    fn build(
        db_path: PathBuf,
        store: VmStore,
        netd: Netd,
        cluster: Option<Arc<crate::cluster::Cluster>>,
    ) -> Result<Arc<Self>, VmManagerError> {
        let base = db_path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let changed = store.database().bell();
        let images = ImageManager::new(store.database(), ImageSettings::from_env(&base))?;
        for ty in [HypervisorType::CloudHypervisor, HypervisorType::Qemu] {
            if !crate::hypervisor::driver(ty).is_available() {
                tracing::warn!(
                    "Hypervisor {} binary {:?} not found — VMs configured for it will fail to start",
                    ty,
                    ty.binary_name()
                );
            }
        }
        let settings = Settings::default();
        let runner = Runner::new(settings.runner);
        let credentials = CredentialStore::new(store.database())?;
        let networks = NetworkStore::new(store.database())?;
        let projects = ProjectStore::new(store.database())?;
        let tenants = crate::tenants::TenantStore::new(store.database())
            .map_err(|e| VmManagerError::Tenancy(TenancyError::Storage(e.to_string())))?;
        let nodes = crate::node::NodeStore::new(store.database());
        let mgr = Arc::new_cyclic(|me| Self {
            vms: RwLock::new(HashMap::new()),
            credentials,
            networks,
            projects,
            tenants,
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
            sched: ArcSwap::from_pointee(Default::default()),
            reserved: ArcSwap::from_pointee(Default::default()),
            pci_pools: ArcSwap::from_pointee(Vec::new()),
            ovn: ArcSwap::from_pointee(Default::default()),
            ovn_file: ArcSwap::from_pointee(Default::default()),
            ovn_file_warned: std::sync::atomic::AtomicBool::new(false),
            nodes,
            roles: std::sync::Mutex::new(crate::node::Roles::STANDALONE),
            db_path,
            cluster: arc_swap::ArcSwapOption::empty(),
            api_router: std::sync::OnceLock::new(),
            secrets_key: std::sync::OnceLock::new(),
            node_link: std::sync::OnceLock::new(),
            tasks: std::sync::Mutex::new(Vec::new()),
            meter: arc_swap::ArcSwapOption::empty(),
            meter_tasks: std::sync::Mutex::new(Vec::new()),
            metering_cfg: std::sync::Mutex::new(None),
            me: me.clone(),
        });
        if let Some(c) = cluster {
            mgr.attach_cluster(c);
        }
        Ok(mgr)
    }

    /// Apply `control-plane.json`'s `reconcile` and `console` sections.
    /// Call before [`Self::initialize`].
    pub fn configure(&self, cfg: &crate::config::Config) {
        let s = Settings::from_config(cfg);
        self.runner.store(Arc::new(Runner::new(s.runner)));
        self.settings.store(Arc::new(s));
        self.sched.store(Arc::new(cfg.cluster.scheduler.clone()));
        self.reserved
            .store(Arc::new(cfg.cluster.node_reserved.clone()));
        self.pci_pools.store(Arc::new(cfg.pci.pools.clone()));
        self.ovn_file.store(Arc::new(cfg.cluster.ovn.clone()));
        self.apply_ovn_settings();
    }

    /// Recompute the OVN configuration from the cluster setting and the
    /// file (at start, and whenever the setting changes).
    pub fn apply_ovn_settings(&self) {
        let file = self.ovn_file.load_full();
        let next = match crate::ovn_settings::read(&self.store.database()) {
            Some(s) => {
                let from_file = crate::ovn_settings::OvnSettings::from_file(&file);
                let file_says_something = from_file.enabled
                    || from_file.edge.is_some()
                    || from_file.nat_supernet.is_some()
                    || !from_file.dns_servers.is_empty();
                if file_says_something
                    && !s.same_options(&from_file)
                    && !self
                        .ovn_file_warned
                        .swap(true, std::sync::atomic::Ordering::AcqRel)
                {
                    tracing::warn!(
                        "control-plane.json's cluster.ovn options are ignored: the cluster's OVN setting decides (spec/clustering-ui.md §8a); only bridge_mappings is read from the file"
                    );
                }
                s.effective(&file)
            }
            None => (*file).clone(),
        };
        if *self.ovn.load_full() != next {
            if self.ovn.load().enabled != next.enabled {
                tracing::info!(enabled = next.enabled, "OVN setting changed");
            }
            self.ovn.store(Arc::new(next));
        }
    }

    /// The cluster's OVN setting, if one was written.
    pub fn ovn_settings(&self) -> Option<crate::ovn_settings::OvnSettings> {
        crate::ovn_settings::read(&self.store.database())
    }

    /// On the leader of a cluster enabled the old way (the file), write the
    /// setting from the file once, so every node follows it.
    pub fn migrate_ovn_settings(&self) {
        if self.cluster().is_none()
            || !self.store.database().can_write()
            || self.ovn_settings().is_some()
        {
            return;
        }
        let file = self.ovn_file.load_full();
        if !file.enabled {
            return;
        }
        let s = crate::ovn_settings::OvnSettings::from_file(&file);
        match crate::ovn_settings::write(&self.store.database(), &s) {
            Ok(()) => tracing::info!(
                "wrote the cluster's OVN setting from this server's control-plane.json"
            ),
            Err(e) => tracing::warn!("writing the cluster's OVN setting: {}", e),
        }
    }

    /// Where `req` goes, from the nodes' records and what each already carries.
    pub(crate) fn pick_node(
        &self,
        vms: &HashMap<String, Vm>,
        req: &crate::scheduler::Request,
    ) -> Result<crate::scheduler::Choice, Vec<(String, String)>> {
        crate::scheduler::schedule(req, &self.node_loads(vms), &self.sched.load())
    }

    /// The page size `vm`'s RAM is counted in on its node: what the
    /// scheduler recorded, else (placed before it counted) hugepages of the
    /// asked-for or default size when asked for, ordinary memory otherwise.
    fn counted_page_kib(vm: &Vm) -> u64 {
        if let Some(k) = vm.status.placement.as_ref().and_then(|p| p.page_kib) {
            return k;
        }
        let c = vm.config();
        if c.hugepages {
            c.hugepage_size
                .as_deref()
                .and_then(crate::models::hugepage_size_kib)
                .unwrap_or(2048)
        } else {
            crate::models::SMALL_PAGE_KIB
        }
    }

    /// The page size to record for a VM of `config` on a standalone host,
    /// which runs no scheduler: asked-for hugepages in their size (the
    /// start check enforces that they are there); otherwise `None`, so the
    /// node decides vhost-user hugepages at start from what is free.
    fn standalone_page_kib(&self, config: &VmConfig) -> Option<u64> {
        if !config.hugepages {
            return None;
        }
        config
            .hugepage_size
            .as_deref()
            .and_then(crate::models::hugepage_size_kib)
            .or_else(|| {
                self.nodes
                    .get(&self.local_node_id())
                    .ok()
                    .flatten()
                    .map(|n| n.status.features.hugepage_kib)
                    .filter(|k| *k > 0)
            })
            .or(Some(2048))
    }

    /// The configured device pools (spec/host-topology.md §9).
    pub fn pci_pools(&self) -> Arc<Vec<crate::config::PciPool>> {
        self.pci_pools.load_full()
    }

    /// Every node with what is placed on it, for the pool listing.
    pub(crate) async fn loads_by_node(&self) -> Vec<(crate::node::Node, crate::scheduler::Load)> {
        let vms = self.vms.read().await;
        self.node_loads(&vms)
    }

    /// The CPUs shared VMs on this node run on: all but the reserved and
    /// those the dedicated VMs placed here hold; empty while none are
    /// (spec/host-topology.md §8.1).
    pub(crate) async fn local_shared_pool(&self) -> Vec<u32> {
        let local = self.local_node_id();
        let Ok(Some(n)) = self.nodes.get(&local) else {
            return Vec::new();
        };
        let held: std::collections::BTreeSet<u32> = self
            .vms
            .read()
            .await
            .values()
            .filter_map(|v| v.status.placement.as_ref())
            .filter(|p| p.node == local)
            .flat_map(|p| p.cpus.iter().copied())
            .collect();
        crate::scheduler::shared_pool(&n, &held)
    }

    /// Where on this standalone host a VM goes (spec/host-topology.md H8):
    /// asked-for hugepages in their size and, under a NUMA or CPU policy,
    /// the NUMA node and CPUs the scheduler's fit picks against what the
    /// other VMs here hold. A `strict` VM that fits no NUMA node is refused.
    fn standalone_fit(
        &self,
        vms: &HashMap<String, Vm>,
        vm: &Vm,
        disks: &[Disk],
    ) -> Result<crate::scheduler::Fit, VmManagerError> {
        let page = self.standalone_page_kib(vm.config());
        let c = vm.config();
        let fit = crate::scheduler::Fit {
            page_kib: page.unwrap_or(crate::models::SMALL_PAGE_KIB),
            numa_aligned: true,
            ..Default::default()
        };
        if c.numa_policy.is_none() && c.cpu_policy.is_shared() && c.devices.is_empty() {
            return Ok(fit);
        }
        let local = self.local_node_id();
        let Some((n, load)) = self
            .node_loads_except(vms, Some(&vm.id))
            .into_iter()
            .find(|(n, _)| n.meta.id == local)
        else {
            return Err(VmManagerError::Network(NetError::Conflict(
                "this host has no node record to place the VM by".into(),
            )));
        };
        let req = self.sched_request(vm, disks);
        crate::scheduler::place_on(&n, &load, &req, &self.sched.load(), fit.page_kib)
            .map_err(|why| VmManagerError::Network(NetError::Conflict(why)))
    }

    /// Record `fit` in `vm`'s placement. `page_kib` stays unset on a
    /// standalone host for ordinary or implicit pages (the node decides
    /// vhost-user hugepages at start).
    fn apply_fit(&self, vm: &mut Vm, fit: crate::scheduler::Fit) {
        let standalone = self.cluster().is_none();
        let page = if standalone {
            self.standalone_page_kib(vm.config())
        } else {
            Some(fit.page_kib)
        };
        let policy = !vm.config().numa_policy.is_none() || !vm.config().cpu_policy.is_shared();
        if let Some(p) = vm.status.placement.as_mut() {
            p.page_kib = page;
            p.numa_node = fit.numa_node;
            p.cpus = fit.cpus;
            p.pool_devices = fit.pool_devices;
        }
        Self::note_numa_alignment(vm, policy, fit.numa_aligned);
    }

    /// `NumaAligned` for a VM with a NUMA or CPU policy: `False` when no
    /// single NUMA node could take a `preferred` VM.
    pub(crate) fn note_numa_alignment(vm: &mut Vm, policy: bool, aligned: bool) {
        if !policy {
            vm.status.conditions.retain(|c| c.kind != "NumaAligned");
            return;
        }
        let (status, reason, msg) = if aligned {
            (crate::models::Tristate::True, "Aligned", String::new())
        } else {
            (
                crate::models::Tristate::False,
                "NoNumaNodeFits",
                "no single NUMA node has its devices, memory and CPUs together; it spans the host"
                    .to_string(),
            )
        };
        crate::controller::vm::set_cond(
            &mut vm.status.conditions,
            "NumaAligned",
            status,
            reason,
            msg,
        );
    }

    /// Fit `vm` again on its node after a change to its memory, hugepages,
    /// CPUs, policies, devices or networks (spec/host-topology.md §6,
    /// §8a.3): its pages, NUMA node and dedicated CPUs. What no longer fits
    /// there is refused with 409. A running VM keeps its old binding until
    /// its next start.
    fn refit_placement(
        &self,
        vms: &HashMap<String, Vm>,
        vm: &mut Vm,
        disks: &[Disk],
    ) -> Result<(), VmManagerError> {
        let Some(node) = vm.status.placement.as_ref().map(|p| p.node.clone()) else {
            return Ok(());
        };
        let fit = if self.cluster().is_none() {
            self.standalone_fit(vms, vm, disks)?
        } else {
            let Some((n, load)) = self
                .node_loads_except(vms, Some(&vm.id))
                .into_iter()
                .find(|(n, _)| n.meta.id == node)
            else {
                return Ok(());
            };
            let req = self.sched_request(vm, disks);
            let conflict = |why: String| {
                VmManagerError::Network(NetError::Conflict(format!(
                    "node {}: {}",
                    n.spec.name, why
                )))
            };
            let page = crate::scheduler::page_kib(&n, &load, &req).map_err(conflict)?;
            crate::scheduler::place_on(&n, &load, &req, &self.sched.load(), page)
                .map_err(conflict)?
        };
        self.apply_fit(vm, fit);
        Ok(())
    }

    /// Every node that may take VMs, with what is placed on it.
    fn node_loads(
        &self,
        vms: &HashMap<String, Vm>,
    ) -> Vec<(crate::node::Node, crate::scheduler::Load)> {
        self.node_loads_except(vms, None)
    }

    /// [`Self::node_loads`] without what VM `except` takes.
    fn node_loads_except(
        &self,
        vms: &HashMap<String, Vm>,
        except: Option<&str>,
    ) -> Vec<(crate::node::Node, crate::scheduler::Load)> {
        let nodes = self.nodes.list().unwrap_or_default();
        let others = vms.values().filter(|v| Some(v.id.as_str()) != except);
        let mut loads = crate::scheduler::loads(others.filter_map(|v| {
            v.status
                .placement
                .as_ref()
                .map(|p| crate::scheduler::Placed {
                    node: p.node.clone(),
                    vcpus: v.config().vcpu_count as u64,
                    mem_mib: v.config().mem_size_mib as u64,
                    name: v.name.clone(),
                    devices: Self::devices_held(v).cloned().collect(),
                    page_kib: Self::counted_page_kib(v),
                    numa_node: p.numa_node,
                    cpus: p.cpus.clone(),
                })
        }));
        nodes
            .into_iter()
            .filter(|n| !n.status.phase.is_tombstone())
            .map(|n| {
                let l = loads.remove(&n.meta.id).unwrap_or_default();
                (n, l)
            })
            .collect()
    }

    /// Where a VM of `config` (pinned to `pin`, using `data_disks`) could go,
    /// node by node, the scheduler's choice first (spec/clustering-ui.md §5.1).
    pub async fn preview_placement(
        &self,
        project: &str,
        config: &VmConfig,
        pin: Option<String>,
        data_disks: &[String],
        image: Option<&str>,
    ) -> Result<Vec<crate::scheduler::NodeFit>, VmManagerError> {
        let mut disks = Vec::new();
        for key in data_disks {
            disks.push(self.images.get_disk_in(key, self.tenant_scope(project))?);
        }
        let mut req = self.sched_request_for(config, pin, &disks);
        if let Some(key) = image {
            req.arch = self.images.get_image(key)?.arch.as_str().to_string();
        }
        let vms = self.vms.read().await;
        Ok(crate::scheduler::evaluate(
            &req,
            &self.node_loads(&vms),
            &self.sched.load(),
        ))
    }

    /// What `vm` asks of a node, given the disks it uses.
    pub(crate) fn sched_request_pub(&self, vm: &Vm, disks: &[Disk]) -> crate::scheduler::Request {
        self.sched_request(vm, disks)
    }

    fn sched_request(&self, vm: &Vm, disks: &[Disk]) -> crate::scheduler::Request {
        let mut r = self.sched_request_for(vm.config(), vm.spec.node.clone(), disks);
        r.device_nodes = vm.spec.device_nodes.clone();
        r
    }

    fn sched_request_for(
        &self,
        c: &VmConfig,
        pin: Option<String>,
        disks: &[Disk],
    ) -> crate::scheduler::Request {
        let nets = self.networks.list().unwrap_or_default();
        crate::scheduler::Request {
            vcpus: c.vcpu_count as u32,
            mem_mib: c.mem_size_mib as u64,
            hypervisor: c.hypervisor.to_string(),
            hugepages: c.hugepages,
            hugepage_kib: c
                .hugepage_size
                .as_deref()
                .and_then(crate::models::hugepage_size_kib),
            vhost_user: c
                .networks
                .iter()
                .filter_map(|a| nets.iter().find(|n| n.id == a.network))
                .any(|n| n.port_type == glidex_ovs::vm_port::VmPortKind::VhostUser),
            numa: c.numa_policy,
            cpu: c.cpu_policy,
            pin,
            disk_nodes: disks.iter().map(|d| d.node.clone()).collect(),
            disk_reach: self.disk_reach(disks),
            net_nodes: c
                .networks
                .iter()
                .filter_map(|a| nets.iter().find(|n| n.id == a.network))
                .filter(|n| n.scope == crate::network::NetworkScope::Node)
                .filter_map(|n| n.node.clone())
                .collect(),
            vfio: c.vfio_devices.clone(),
            device_nodes: None,
            pools: {
                let defs = self.pci_pools.load();
                c.devices
                    .iter()
                    .filter_map(|r| {
                        defs.iter().find(|p| p.name == r.pool).map(|p| {
                            crate::scheduler::PoolRequest {
                                pool: p.clone(),
                                count: r.count,
                                same_switch: r.same_switch,
                            }
                        })
                    })
                    .collect()
            },
            physnets: c
                .networks
                .iter()
                .filter_map(|a| nets.iter().find(|n| n.id == a.network))
                .filter(|n| n.scope == crate::network::NetworkScope::Cluster)
                .filter_map(|n| n.physnet.clone())
                .collect(),
            vhost_user_on_cluster: c
                .networks
                .iter()
                .filter_map(|a| nets.iter().find(|n| n.id == a.network))
                .any(|n| {
                    n.scope == crate::network::NetworkScope::Cluster
                        && n.port_type == glidex_ovs::vm_port::VmPortKind::VhostUser
                }),
            ovn_northd: c
                .networks
                .iter()
                .filter_map(|a| nets.iter().find(|n| n.id == a.network))
                .any(|n| n.scope == crate::network::NetworkScope::Cluster)
                .then(|| crate::node::cluster_northd_ovn(&self.nodes().list().unwrap_or_default()))
                .flatten(),
            arch: self
                .vm_arch(c, disks)
                .map(|a| a.as_str().to_string())
                .unwrap_or_default(),
        }
    }

    /// For each unbound disk on a registered datastore, the nodes that can
    /// make it: on the datastore's nodes and reaching it now (spec/storage.md
    /// §5.2).
    fn disk_reach(&self, disks: &[Disk]) -> Vec<(String, Vec<String>)> {
        use crate::datastore::record;
        let db = self.store.database();
        disks
            .iter()
            .filter(|d| d.node.is_none() && d.datastore != crate::datastore::LOCAL)
            .map(|d| match record::find(&db, &d.datastore).ok().flatten() {
                Some(ds) => {
                    let nodes = record::rows(&db, &ds.id)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|(n, r)| r.reachable && ds.on_node(n))
                        .map(|(n, _)| n)
                        .collect();
                    (ds.name, nodes)
                }
                None => (d.datastore.clone(), Vec::new()),
            })
            .collect()
    }

    /// A VM's architecture (spec/clustering.md §9.2.1): that of the image a
    /// disk of it comes from, else of its firmware image; `None` for one
    /// with neither (direct kernel boot).
    pub(crate) fn vm_arch(
        &self,
        c: &VmConfig,
        disks: &[Disk],
    ) -> Option<crate::images::catalog::Arch> {
        let from_disk = disks.iter().find_map(|d| match &d.origin {
            crate::images::DiskOrigin::Image { image_id, .. } => {
                self.images.get_image(image_id).ok().map(|i| i.arch)
            }
            _ => None,
        });
        from_disk.or_else(|| {
            c.firmware_image
                .as_deref()
                .and_then(|f| self.images.get_image(f).ok())
                .map(|i| i.arch)
        })
    }

    /// Whether this host has any VM, disk, network or credential record.
    pub async fn has_resources(&self) -> bool {
        !self.vms.read().await.is_empty()
            || !self.images.list_disks().is_empty()
            || self
                .networks
                .list()
                .map(|n| n.iter().any(|n| !Self::is_stock_default(n)))
                .unwrap_or(false)
            || self
                .credentials
                .list(None)
                .map(|c| !c.is_empty())
                .unwrap_or(false)
    }

    /// The `default` NAT network older versions made for a host at first
    /// start (glidex no longer creates one): not something a join would
    /// abandon.
    fn is_stock_default(n: &Network) -> bool {
        n.name == network::DEFAULT_NETWORK
            && n.bridge == network::DEFAULT_BRIDGE
            && n.scope == network::NetworkScope::Node
            && n.project.is_none()
    }

    /// Before a plain join: take down the stock `default` network (its bridge
    /// and NAT on the host), which the cluster's records would otherwise leave
    /// behind as an orphan. No VM can use it: `has_resources` said none exist.
    pub async fn drop_stock_default_network(&self) -> Result<(), VmManagerError> {
        if let Some(n) = self
            .networks
            .list()?
            .into_iter()
            .find(Self::is_stock_default)
        {
            self.delete_network(&n.id).await?;
        }
        Ok(())
    }

    /// Whether datastores may be written: on a standalone host, or once
    /// every server of the cluster supports them (spec/clustering.md §6.6).
    pub fn datastores_supported(&self) -> bool {
        match self.cluster() {
            None => true,
            Some(c) => c.feature_level() >= crate::datastore::record::DATASTORE_FEATURE_LEVEL,
        }
    }

    /// Look at every datastore this node is on and say what it finds
    /// (`datastore_nodes`, spec/storage.md §4.1); drop its rows about the
    /// ones it is no longer on. Writes only what changed. Blocks.
    pub fn probe_datastores(&self) {
        use crate::datastore::{probe, record};
        if !self.datastores_supported() {
            return;
        }
        let db = self.store.database();
        let me = self.local_node_id();
        let Ok(all) = record::list(&db) else { return };
        let root = record::root();
        let now = crate::tenancy::now();
        for ds in all {
            let old = record::rows(&db, &ds.id)
                .ok()
                .and_then(|mut r| r.remove(&me));
            let new = if ds.is_builtin() {
                // `local` is the disk directory, wherever it is.
                probe::probe_dir(&self.images.settings.disk_dir, now)
            } else if ds.on_node(&me) {
                probe::probe(&ds, &me, &root, now)
            } else {
                if old.is_some() {
                    let _ = record::put_row(&db, &ds.id, &me, None);
                }
                continue;
            };
            if probe::worth_writing(old.as_ref(), &new)
                && let Err(e) = record::put_row(&db, &ds.id, &me, Some(&new))
            {
                tracing::warn!(datastore = %ds.name, "recording what this node finds: {}", e);
            }
        }
    }

    /// Refresh this node's own record with what the host has now. Writes
    /// nothing when nothing changed.
    pub fn report_self(&self) -> Result<(), VmManagerError> {
        let reserved = {
            let c = &self.reserved.load();
            crate::node::Resources {
                cpus: c.cpus,
                memory_mib: c.memory_mib,
                hugepages: Default::default(),
            }
        };
        let mut probe = crate::node::SelfProbe::detect(&reserved);
        // Unrecorded image files (spec/clustering.md §9.2.3): deleted when
        // asked (once per request), the rest reported.
        if let Some(before) = self
            .nodes
            .get(&self.local_node_id())
            .ok()
            .flatten()
            .and_then(|n| n.spec.drop_spare_files_before)
        {
            self.images.drop_spare_files(before);
        }
        probe.features.spare_image_files = self.images.spare_files();
        probe.features.physnets = self.ovn.load().bridge_mappings.clone();
        probe.features.feature_level = crate::node::FEATURE_LEVEL;
        // Whether OVS runs DPDK here: what `auto` picks br-int's datapath by.
        if let (_, Ok(caps)) = self.netd.probe() {
            probe.features.dpdk = caps
                .get("dpdk_initialized")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            // OVS-DPDK's hugepages are not the VMs' (spec/host-topology.md
            // §8a.2): DPDK takes them in the default page size.
            if let (true, Some(mem)) = (
                probe.features.dpdk,
                caps.get("dpdk_socket_mem").and_then(|v| v.as_str()),
            ) {
                let kib = match probe.features.hugepage_kib {
                    0 => 2048,
                    k => k,
                };
                let taken = crate::node::dpdk_pages(mem, kib);
                if let Some(n) = probe.allocatable.hugepages.get_mut(&kib.to_string()) {
                    *n = n.saturating_sub(taken);
                }
                if let Some(t) = probe.topology.as_mut() {
                    t.deduct_dpdk(mem, kib);
                }
            }
            // OVS-DPDK's polling and housekeeping CPUs are not the VMs'.
            if let Some(t) = probe.topology.as_mut() {
                for (key, why) in [
                    ("dpdk_pmd_cpu_mask", "pmd"),
                    ("dpdk_lcore_mask", "dpdk_lcore"),
                ] {
                    if let Some(mask) = caps.get(key).and_then(|v| v.as_str()) {
                        t.reserve(crate::topology::cpus_of_mask(mask), why);
                    }
                }
            }
        }
        if let (Some(t), Some(list)) = (probe.topology.as_mut(), &self.reserved.load().cpu_list) {
            t.reserve(glidex_ovs::tuning::parse_cpu_list(list), "config");
            t.reserved_by_config = true;
        }
        // The devices a VM could be given, with their readiness, for pools
        // (spec/host-topology.md §9).
        if let Some(t) = probe.topology.as_mut() {
            t.devices =
                crate::topology::read_tree(t.clone(), self.dpdk_uplink_bdfs()).summary_devices();
        }
        // The address each migrated uplink carries on its bridge.
        if let Ok(ups) = self
            .netd
            .call::<Vec<glidex_netd::proto::UplinkResult>>(Op::ListUplinks)
        {
            for u in ups
                .into_iter()
                .filter(|u| u.record.boot_rollback.is_none() && u.record.pending.is_none())
            {
                let Some(snap) = &u.record.snapshot else {
                    continue;
                };
                if let Some(a) = snap.addresses.iter().find(|a| {
                    a.split('/')
                        .next()
                        .is_some_and(|ip| ip.parse::<std::net::Ipv4Addr>().is_ok())
                }) {
                    probe.features.bridge_addrs.insert(
                        u.record.spec.bridge.clone(),
                        crate::node::BridgeAddr {
                            address: a.clone(),
                            gateway: snap.default_gateway().map(String::from),
                        },
                    );
                }
            }
        }
        self.nodes
            .report_self(&self.local_node_id(), probe, self.cluster().is_none())
            .map(|_| ())
            .map_err(|e| VmManagerError::PersistenceError(e.to_string()))
    }

    /// Every network record.
    pub fn networks_list(&self) -> Vec<Network> {
        self.networks.list().unwrap_or_default()
    }

    /// The image and disk records and files of this host.
    pub fn images(&self) -> &Arc<ImageManager> {
        &self.images
    }

    /// This host's node id: its cluster identity, else the standalone `local`.
    pub fn local_node_id(&self) -> String {
        match self.cluster() {
            Some(c) => c.identity.node_id.clone(),
            None => crate::authz::node_id(),
        }
    }

    /// Whether `vm` runs here. A VM without a placement is this host's (a
    /// record from before nodes).
    /// The node API address of the node `vm` runs on, if that is not this one.
    pub fn remote_node_addr(&self, vm: &Vm) -> Option<String> {
        if self.is_local(vm) {
            return None;
        }
        let node = vm.status.placement.as_ref()?.node.clone();
        self.nodes
            .get(&node)
            .ok()
            .flatten()
            .and_then(|n| n.status.advertise)
            .map(|a| a.to_string())
    }

    pub(crate) fn is_local(&self, vm: &Vm) -> bool {
        match &vm.status.placement {
            Some(p) => p.node == self.local_node_id(),
            // A record from before nodes is this host's; in a cluster a VM
            // without a placement is nobody's until the scheduler places it.
            None => self.cluster().is_none(),
        }
    }

    /// The id of a live node named by id or name.
    pub(crate) fn resolve_node(&self, want: &str) -> Result<String, VmManagerError> {
        let nodes = self
            .nodes
            .list()
            .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
        nodes
            .iter()
            .find(|n| (n.meta.id == want || n.spec.name == want) && !n.status.phase.is_tombstone())
            .map(|n| n.meta.id.clone())
            .ok_or_else(|| invalid(format!("no node {want}")))
    }

    /// Whether this node owns `disk`'s file: bound here, or (standalone) unbound.
    pub(crate) fn is_local_disk(&self, disk: &Disk) -> bool {
        match &disk.node {
            Some(n) => *n == self.local_node_id(),
            None => self.cluster().is_none(),
        }
    }

    /// An agent's link to the servers, once it has one.
    pub fn node_link(&self) -> Option<&Arc<crate::cluster::agent::NodeLink>> {
        self.node_link.get()
    }

    pub fn db_path(&self) -> &std::path::Path {
        &self.db_path
    }

    /// This host's cluster membership, if it is in a cluster.
    pub fn cluster(&self) -> Option<Arc<crate::cluster::Cluster>> {
        self.cluster.load_full()
    }

    /// Record the API router, so a cluster started now or later can serve
    /// forwarded requests with it.
    pub fn set_api_router(&self, router: axum::Router) {
        if let Some(c) = self.cluster() {
            c.set_api(router.clone());
        }
        let _ = self.api_router.set(router);
    }

    /// Record the secrets key's holder, for a cluster to send to and
    /// receive from other servers (spec/tenancy.md §13).
    pub fn set_secrets_key(&self, s: Arc<dyn crate::cluster::runtime::SecretsKey>) {
        if let Some(c) = self.cluster() {
            c.set_secrets(s.clone());
        }
        let _ = self.secrets_key.set(s);
    }

    /// Hand `cluster` the secrets key's holder before it joins, so the CA
    /// push that ends a server join can install the key.
    pub(crate) fn lend_secrets_key(&self, cluster: &crate::cluster::Cluster) {
        if let Some(s) = self.secrets_key.get() {
            cluster.set_secrets(s.clone());
        }
    }

    /// Take part in `cluster`: keep the in-memory VM cache in step with what
    /// the log applies, and give the cluster the API for forwarded requests.
    pub fn attach_cluster(&self, cluster: Arc<crate::cluster::Cluster>) {
        if let Some(r) = self.api_router.get() {
            cluster.set_api(r.clone());
        }
        if let Some(s) = self.secrets_key.get() {
            cluster.set_secrets(s.clone());
        }
        let _ = cluster.handlers.set(Arc::new(HandlersOf(self.me.clone())));
        if cluster.identity.role == crate::node::NodeRole::Agent {
            // An agent runs the node role only, from a cache of the store.
            self.set_roles(crate::node::Roles {
                node: true,
                server: false,
            });
            let link = crate::cluster::agent::NodeLink::start(
                cluster.clone(),
                self.database(),
                cluster.identity.seeds.clone(),
            );
            let _ = self.node_link.set(link);
        }
        self.images.set_node_id(&cluster.identity.node_id);
        self.cluster.store(Some(cluster));
        // The meter now samples into a node-local ledger and ships it.
        self.restart_metering();
        let me = self.arc();
        tokio::spawn(async move { me.cache_sync().await });
    }

    /// Stop this host's cluster runtime and forget it (a rejoin starts a new one).
    pub async fn detach_cluster(&self) {
        if let Some(c) = self.cluster.swap(None) {
            self.images.set_node_id(&crate::authz::node_id());
            c.stop().await;
            // Its Raft log is open until every holder lets go: wait for them, so a
            // runtime started next can open the same files.
            let weak = Arc::downgrade(&c);
            drop(c);
            for _ in 0..100 {
                if weak.strong_count() == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }

    /// Keep `vms` equal to the `vms` table as entries are applied (a
    /// follower's cache changes when the leader's write reaches it).
    async fn cache_sync(self: Arc<Self>) {
        let mut rx = self.store.database().subscribe();
        loop {
            match rx.recv().await {
                // A cache re-list replaces everything: no keys to say which.
                Ok(a) if a.keys.is_empty() => {
                    self.apply_ovn_settings();
                    self.reload_vms().await
                }
                // One write set may hold several tables (a VM is created with
                // its root disk): images and disks first, so the VM
                // controller finds the disk the VM waits for.
                Ok(a) => {
                    for table in [crate::store::TableId::Images, crate::store::TableId::Disks] {
                        if a.tables.contains(&table) {
                            let keys: Vec<String> = a
                                .keys
                                .iter()
                                .filter(|(t, _)| *t == table)
                                .filter_map(|(_, k)| String::from_utf8(k.clone()).ok())
                                .collect();
                            self.images.sync_keys(table, &keys);
                            // What arrived from elsewhere (the leader, a node's
                            // cache) is this node's controllers' work too: an
                            // image to copy here, a disk to make here, and the
                            // VM waiting for that disk.
                            for k in &keys {
                                match table {
                                    crate::store::TableId::Images => self
                                        .queue
                                        .add(crate::controller::queue::Key::Image(k.clone())),
                                    _ => {
                                        self.queue
                                            .add(crate::controller::queue::Key::Disk(k.clone()));
                                        if let Some(vm) =
                                            self.images.get_disk(k).ok().and_then(|d| d.attached_to)
                                        {
                                            self.queue.add(crate::controller::queue::Key::Vm(vm));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // This node's own record changed (a request to delete its
                    // unrecorded image files, spec/clustering.md §9.2.3):
                    // report now rather than at the next interval.
                    let me = self.local_node_id();
                    if a.keys.iter().any(|(t, k)| {
                        *t == crate::store::TableId::Nodes && k.as_slice() == me.as_bytes()
                    }) {
                        let m = self.clone();
                        tokio::task::spawn_blocking(move || {
                            let wanted = m
                                .nodes
                                .get(&m.local_node_id())
                                .ok()
                                .flatten()
                                .and_then(|n| n.spec.drop_spare_files_before);
                            if wanted.is_some_and(|w| m.images.spare_drop_pending(w)) {
                                let _ = m.report_self();
                            }
                        });
                    }
                    // A datastore added or changed: look at it now rather
                    // than at the next minute.
                    if a.tables.contains(&crate::store::TableId::Datastores) {
                        let m = self.clone();
                        tokio::task::spawn_blocking(move || m.probe_datastores());
                    }
                    // A copy of an image becoming ready on some node: nodes
                    // waiting to fetch it can go now.
                    for (_, k) in a
                        .keys
                        .iter()
                        .filter(|(t, _)| *t == crate::store::TableId::ImageCaches)
                    {
                        if let Some(image) = String::from_utf8_lossy(k).split('/').next() {
                            self.queue
                                .add(crate::controller::queue::Key::Image(image.to_string()));
                        }
                    }
                    if a.tables.contains(&crate::store::TableId::Images)
                        || a.tables.contains(&crate::store::TableId::Disks)
                    {
                        self.notify_changed();
                    }
                    if a.keys
                        .iter()
                        .any(|(t, k)| crate::ovn_settings::is_key(*t, k))
                    {
                        self.apply_ovn_settings();
                    }
                    if a.tables.contains(&crate::store::TableId::Vms) {
                        let keys: Vec<String> = a
                            .keys
                            .iter()
                            .filter(|(t, _)| *t == crate::store::TableId::Vms)
                            .filter_map(|(_, k)| String::from_utf8(k.clone()).ok())
                            .collect();
                        self.refresh_vms(&keys).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    self.apply_ovn_settings();
                    self.reload_vms().await
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    async fn refresh_vms(&self, ids: &[String]) {
        let Ok(all) = self.store.load_all() else {
            return;
        };
        let mut vms = self.vms.write().await;
        for id in ids {
            match all.iter().find(|v| &v.id == id) {
                Some(v) => {
                    if vms.get(id) != Some(v) {
                        vms.insert(id.clone(), v.clone());
                        self.queue
                            .add(crate::controller::queue::Key::Vm(id.clone()));
                    }
                }
                None => {
                    vms.remove(id);
                }
            }
        }
        drop(vms);
        self.notify_changed();
    }

    /// Replace the cache with the table's contents (after a snapshot, or a
    /// missed notification).
    pub async fn reload_vms(&self) {
        let Ok(all) = self.store.load_all() else {
            return;
        };
        let mut vms = self.vms.write().await;
        *vms = all.into_iter().map(|v| (v.id.clone(), v)).collect();
        drop(vms);
        self.images.reload_all();
        self.notify_changed();
    }

    /// Bring the store to the current schema (what `initialize` does first),
    /// for operations that need it before the controllers run.
    pub fn migrate_schema(&self) -> Result<(), VmManagerError> {
        self.store.migrate(self.settings().on_host_boot)?;
        Ok(())
    }

    /// The nodes of the cluster; a standalone host has one, `local`.
    pub fn nodes(&self) -> &crate::node::NodeStore {
        &self.nodes
    }

    /// Which halves of the control plane this process runs (D4).
    pub fn roles(&self) -> crate::node::Roles {
        *self.roles.lock().unwrap()
    }

    pub fn set_roles(&self, roles: crate::node::Roles) {
        *self.roles.lock().unwrap() = roles;
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

    /// As [`open`](Self::open), at the default database path.
    pub async fn open_default(cfg: &crate::config::Config) -> Result<Arc<Self>, VmManagerError> {
        Self::open(Self::default_db_path(), Netd::from_env(), cfg).await
    }

    pub fn default_db_path_pub() -> PathBuf {
        Self::default_db_path()
    }

    /// Get the default database path (~/.glidex/glidex.db)
    fn default_db_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".glidex")
            .join("glidex.db")
    }

    /// Load the store (migrating it, §6.6), adopt running instances
    /// (§9.4 steps 1-4). [`Self::start_controllers`] then starts the loops.
    pub async fn initialize(&self) -> Result<(), VmManagerError> {
        // In a cluster only the leader writes, and a follower finds all of
        // this already done in the replicated data.
        let writable = self.store.database().can_write();
        if writable {
            let migrated = self.store.migrate(self.settings().on_host_boot)?;
            if migrated > 0 {
                tracing::info!(
                    count = migrated,
                    "migrated VM records to the desired-state schema"
                );
            }
        }
        // Every node says what it has (capacity, devices, hypervisors).
        if let Err(e) = self.report_self() {
            tracing::warn!("could not report this node: {}", e);
        }
        let persisted = self.store.load_all()?;
        // A copy on another node is where the file is, not a missing one.
        let me = self.local_node_id();
        let clustered = self.cluster().is_some();
        self.images.initialize(|img| {
            clustered
                && self.image_caches(&img.id).iter().any(|(n, r)| {
                    *n != me && r.phase == crate::controller::image_cache::CachePhase::Ready
                })
        });
        {
            let default_project = self.projects.default_project_id();
            let mut vms = self.vms.write().await;
            for mut vm in persisted {
                if vm.project.is_empty() && writable {
                    vm.project = default_project.clone();
                    self.store.save(&vm)?;
                }
                vms.insert(vm.id.clone(), vm);
            }
        }
        if writable {
            self.adopt_into_default_project()?;
        }
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
    pub(crate) fn put_locked(
        &self,
        vms: &mut HashMap<String, Vm>,
        vm: Vm,
        events: Vec<Event>,
    ) -> Result<Vm, VmManagerError> {
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

    fn writable<'a>(
        vms: &'a HashMap<String, Vm>,
        id: &str,
        if_match: Option<u64>,
    ) -> Result<&'a Vm, VmManagerError> {
        let vm = vms
            .get(id)
            .ok_or_else(|| VmManagerError::VmNotFound(id.to_string()))?;
        if vm.deletion_requested_at.is_some() {
            return Err(VmManagerError::Deleting(vm.name.clone()));
        }
        if let Some(expected) = if_match
            && expected != vm.resource_version
        {
            return Err(VmManagerError::PreconditionFailed {
                expected,
                actual: vm.resource_version,
            });
        }
        Ok(vm)
    }

    // ---- create ----------------------------------------------------------

    #[allow(dead_code)] // used by the tests through the lib crate
    pub async fn create_vm(&self, name: String, config: VmConfig) -> Result<Vm, VmManagerError> {
        self.create_vm_with_disks(name, config, DiskSelection::default())
            .await
            .map(|(vm, _)| vm)
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
        self.create_vm_with(
            &project,
            name,
            config,
            sel,
            VmOptions::default(),
            QuotaMode::MayExceed,
            "api",
        )
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
        self.create_vm_with(
            project,
            name,
            config,
            sel,
            VmOptions::default(),
            quota,
            "api",
        )
        .await
    }

    /// Admission paths the VM's command line will carry (§8.2).
    fn check_paths(&self, config: &VmConfig) -> Result<(), VmManagerError> {
        let systemd = matches!(*self.runner(), Runner::Systemd(_));
        let mut paths: Vec<(&str, &str)> = vec![
            ("kernel_image_path", &config.kernel_image_path),
            ("rootfs_path", &config.rootfs_path),
        ];
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

    /// `hugepage_size` is `2M` or `1G`, goes with `hugepages`, and divides
    /// the VM's memory.
    fn check_hugepages(config: &VmConfig) -> Result<(), VmManagerError> {
        let Some(size) = &config.hugepage_size else {
            return Ok(());
        };
        let kib = crate::models::hugepage_size_kib(size)
            .ok_or_else(|| invalid(format!("hugepage_size {size}: use 2M or 1G")))?;
        if !config.hugepages {
            return Err(invalid("hugepage_size needs hugepages"));
        }
        if !(config.mem_size_mib as u64).is_multiple_of(kib / 1024) {
            return Err(invalid(format!(
                "mem_size_mib must be a multiple of {} MiB for {size} hugepages",
                kib / 1024
            )));
        }
        Ok(())
    }

    fn check_options(opts: &VmOptions) -> Result<(), VmManagerError> {
        if opts
            .stop_grace_secs
            .is_some_and(|g| g > MAX_STOP_GRACE_SECS)
        {
            return Err(invalid(format!(
                "stop_grace_secs must be 0-{}",
                MAX_STOP_GRACE_SECS
            )));
        }
        Ok(())
    }

    /// Check a VM's NICs, and name each network by its id (a request may
    /// give a name, which resolves in the project's tenant).
    fn check_networks(
        &self,
        project: &str,
        networks: &mut [NetworkAttachment],
    ) -> Result<(), VmManagerError> {
        if networks.len() > glidex_ovs::names::MAX_NICS as usize {
            return Err(invalid(format!(
                "at most {} networks per VM",
                glidex_ovs::names::MAX_NICS
            )));
        }
        let tenant = self
            .projects
            .get(project)?
            .map(|p| p.tenant_id)
            .unwrap_or_else(|| crate::tenants::DEFAULT_TENANT.to_string());
        for att in networks {
            let found = match self.find_network(project, &att.network) {
                Ok(n) => Some(n),
                Err(VmManagerError::Network(NetError::NotFound(_))) => None,
                Err(e) => return Err(e),
            };
            if let Some(n) = &found {
                att.network = n.id.clone();
            }
            match found {
                None => return Err(invalid(format!("network not found: {}", att.network))),
                Some(n) if !n.usable_by(project, &tenant) => {
                    return Err(invalid(format!(
                        "network '{}' is not available to this project",
                        att.network
                    )));
                }
                Some(n) if n.deletion_requested_at.is_some() => {
                    return Err(invalid(format!(
                        "network '{}' is being deleted",
                        att.network
                    )));
                }
                Some(_) => {}
            }
            if let Some(mac) = &att.mac {
                glidex_ovs::names::validate_mac(mac).map_err(|e| invalid(e.to_string()))?;
            }
            if let Some(q) = att.queue_pairs
                && !(1..=8).contains(&q)
            {
                return Err(invalid("queue_pairs must be 1-8"));
            }
        }
        Ok(())
    }

    fn check_credential(&self, project: &str, config: &VmConfig) -> Result<(), VmManagerError> {
        if let Some(username) = &config.credential {
            // A credential only takes effect through the generated seed.
            if config.firmware_path.is_none() || config.cloud_init_path.is_some() {
                return Err(invalid(
                    "credential requires firmware boot without a custom cloud_init_path",
                ));
            }
            match self.credentials.get(project, username) {
                Ok(_) => {}
                Err(CredentialError::NotFound(_)) => {
                    return Err(invalid(format!("credential not found: {}", username)));
                }
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
        let project_rec = self
            .projects
            .get(project)?
            .ok_or_else(|| TenancyError::NotFound(project.to_string()))?;
        let roots = [
            !config.rootfs_path.is_empty(),
            sel.image.is_some(),
            sel.root_disk.is_some(),
        ];
        if roots.iter().filter(|b| **b).count() != 1 {
            return Err(invalid(
                "give exactly one of rootfs_path, image or root_disk",
            ));
        }
        if sel.root_disk_size_gib.is_some() && sel.image.is_none() {
            return Err(invalid("root_disk_size_gib needs image"));
        }
        if sel.firmware.is_some() && config.firmware_path.is_some() {
            return Err(invalid(
                "give firmware (an image) or firmware_path, not both",
            ));
        }
        // The VM's architecture: its root disk's image's (§9.2.1).
        let root_arch = match (&sel.image, &sel.root_disk) {
            (Some(key), _) => Some(self.images.get_image(key)?.arch),
            (None, Some(key)) => match self
                .images
                .get_disk_in(key, self.tenant_scope(project))?
                .origin
            {
                crate::images::DiskOrigin::Image { image_id, .. } => {
                    self.images.get_image(&image_id).ok().map(|i| i.arch)
                }
                _ => None,
            },
            _ => None,
        };
        // A managed root disk is a cloud image: firmware boot unless the
        // caller brought a kernel, through the newest firmware image for
        // the hypervisor and the VM's architecture unless one is named.
        let firmware = match &sel.firmware {
            Some(key) => Some(self.images.get_image(key)?),
            None if (sel.image.is_some() || sel.root_disk.is_some())
                && config.kernel_image_path.is_empty()
                && config.firmware_path.is_none() =>
            {
                Some(self.images.default_firmware(config.hypervisor, root_arch).ok_or_else(|| {
                    invalid(format!(
                        "no firmware image for {}{}: pull one from the firmware catalog (GET /images/firmware-catalog){}, or give kernel_image_path",
                        config.hypervisor,
                        root_arch.map(|a| format!(" on {}", a.as_str())).unwrap_or_default(),
                        root_arch.map(|a| format!(" with arch {}", a.as_str())).unwrap_or_default(),
                    ))
                })?)
            }
            None => None,
        };
        if let (Some(fw), Some(a)) = (&firmware, root_arch)
            && fw.arch != a
        {
            return Err(invalid(format!(
                "firmware {} is {}, the root disk's image is {}: they must match",
                fw.name,
                fw.arch.as_str(),
                a.as_str()
            )));
        }
        if config.vcpu_count == 0 {
            return Err(invalid("vcpu_count must be greater than 0"));
        }
        if config.mem_size_mib == 0 {
            return Err(invalid("mem_size_mib must be greater than 0"));
        }
        Self::check_hugepages(&config)?;
        if firmware.is_none()
            && config.firmware_path.is_none()
            && config.kernel_image_path.is_empty()
        {
            return Err(invalid("either kernel_image_path or firmware is required"));
        }
        Self::check_options(&opts)?;
        self.check_paths(&config)?;
        self.refuse_dpdk_uplink_devices(&config.vfio_devices)?;
        self.check_networks(project, &mut config.networks)?;

        let mut vms = self.vms.write().await;

        // Checked under the VM write lock, so delete_image (which holds the
        // read lock) can't remove it between the check and the insert.
        if let Some(fw) = &firmware {
            let fw = self.images.get_image(&fw.id)?;
            self.check_firmware(&fw, config.hypervisor)?;
            config.firmware_path = Some(
                self.images
                    .firmware_path(&fw.id)
                    .to_string_lossy()
                    .into_owned(),
            );
            config.firmware_image = Some(fw.id);
        }

        // Checked under the VM write lock so delete_credential (which holds
        // the read lock) can't remove it between the check and the insert.
        self.check_credential(project, &config)?;

        // Names are unique per project.
        if vms
            .values()
            .any(|vm| vm.name == name && vm.project == project)
        {
            return Err(VmManagerError::VmAlreadyExists(name));
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
            devices: pool_counts(&config.devices),
            ..Default::default()
        };
        let usage = self.usage_locked(project, &vms)?;
        self.apply_quotas(&project_rec, &usage, &delta, quota, &mut bypassed, &vms)?;

        let mut vm = Vm::new(name, config);
        vm.project = project.to_string();
        // `spec.node`: an optional pin, by id or name.
        if let Some(want) = &opts.node {
            vm.spec.node = Some(self.resolve_node(want)?);
        }
        // A device another VM on the node claims (spec/host-topology.md
        // §8.4). Unpinned in a cluster, the node isn't known yet: the
        // scheduler skips nodes where it is taken.
        let claims_node = if self.cluster().is_none() {
            Some(self.local_node_id())
        } else {
            vm.spec.node.clone()
        };
        if let Some(node) = claims_node {
            self.refuse_claimed_devices(&vms, &node, &vm.spec.config.vfio_devices, None)?;
        }
        vm.spec.power = power;
        vm.spec.device_nodes = opts.device_nodes.clone();
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
                return Err(invalid(format!(
                    "disk {} is both root_disk and a data disk",
                    d.name
                )));
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
            if self
                .images
                .disk_name_taken_in(&disk_name, self.tenant_scope(&vm.project))
            {
                disk_name = format!("{}-{}", disk_name.trim_end_matches("-root"), &vm.id[..8]);
            }
            let req = CreateDiskRequest {
                name: disk_name,
                project: Some(project.to_string()),
                size_gib: sel.root_disk_size_gib,
                image: Some(image.clone()),
                ..Default::default()
            };
            let mut d = self
                .images
                .new_disk_record(&req, self.tenant_scope(&vm.project))?;
            if sel.root_disk_size_gib.is_none() {
                let disk_delta = Delta {
                    disk_gib: gib_ceil(self.estimated_size(&d)),
                    ..Default::default()
                };
                self.apply_quotas(
                    &project_rec,
                    &usage,
                    &disk_delta,
                    quota,
                    &mut bypassed,
                    &vms,
                )?;
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
            if Some(&d.id) == vm.spec.config.root_disk.as_ref()
                && d.pending_growpart
                && vm.spec.config.cloud_init_path.is_some()
            {
                warnings.push(format!(
                    "disk {} is waiting for an on-boot root partition grow, but this VM uses a custom cloud_init_path; enable growpart in that seed",
                    d.name
                ));
            }
        }

        // Where it runs, decided once (D6). A standalone host is its own node.
        if self.cluster().is_none() {
            let node = vm.spec.node.clone().unwrap_or_else(|| self.local_node_id());
            if node != self.local_node_id() {
                return Err(invalid(
                    "this host is not part of a cluster: no other node to run on",
                ));
            }
            vm.status.placement = Some(crate::models::Placement {
                node,
                at: crate::tenancy::now(),
                page_kib: None,
                numa_node: None,
                cpus: Vec::new(),
                pool_devices: Vec::new(),
            });
            let fit = self.standalone_fit(&vms, &vm, &disks)?;
            self.apply_fit(&mut vm, fit);
        } else {
            let req = self.sched_request(&vm, &disks);
            match self.pick_node(&vms, &req) {
                Ok(choice) => {
                    vm.status.placement = Some(crate::models::Placement {
                        node: choice.node,
                        at: crate::tenancy::now(),
                        page_kib: Some(choice.page_kib),
                        numa_node: choice.numa_node,
                        cpus: choice.cpus,
                        pool_devices: choice.pool_devices,
                    });
                    let policy =
                        !vm.config().numa_policy.is_none() || !vm.config().cpu_policy.is_shared();
                    Self::note_numa_alignment(&mut vm, policy, choice.numa_aligned);
                    crate::controller::vm::set_cond(
                        &mut vm.status.conditions,
                        "Scheduled",
                        crate::models::Tristate::True,
                        "Scheduled",
                        "",
                    );
                }
                // Created without a placement; the leader keeps trying.
                Err(refused) => crate::controller::vm::set_cond(
                    &mut vm.status.conditions,
                    "Scheduled",
                    crate::models::Tristate::False,
                    "Unschedulable",
                    crate::scheduler::explain(&refused),
                ),
            }
        }
        if let Some(p) = &vm.status.placement {
            for d in &mut disks {
                d.node.get_or_insert_with(|| p.node.clone());
            }
            if let Some(c) = created.as_mut() {
                c.node.get_or_insert_with(|| p.node.clone());
            }
        }

        // The VM, its disks' claims and its first event in one transaction.
        let ev = Event::new(
            actor,
            EventKind::Normal,
            "Created",
            format!("desired state {}", power_name(power)),
        );
        let reserve_ips = if vm.status.placement.is_some() {
            self.cluster_nics(&vm)
        } else {
            Vec::new()
        };
        let commit = Commit {
            put_vm: Some(&vm),
            put_disks: disks.iter().collect(),
            events: vec![(event_key("vm", &vm.id), ev)],
            reserve_ips,
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
    fn check_firmware(
        &self,
        fw: &images::Image,
        hypervisor: crate::hypervisor::HypervisorType,
    ) -> Result<(), VmManagerError> {
        if !fw.is_firmware() {
            return Err(invalid(format!(
                "image {} is not a firmware image",
                fw.name
            )));
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
            return Err(
                ImageError::NotReady(format!("firmware {} is being deleted", fw.name)).into(),
            );
        }
        if fw.status != images::ImageStatus::Ready {
            return Err(ImageError::NotReady(format!(
                "firmware {} is not ready ({:?})",
                fw.name, fw.status
            ))
            .into());
        }
        Ok(())
    }

    /// Names of the VMs booting through firmware image `id`.
    fn firmware_users(vms: &HashMap<String, Vm>, id: &str) -> Vec<String> {
        let mut v: Vec<String> = vms
            .values()
            .filter(|vm| vm.config().firmware_image.as_deref() == Some(id))
            .map(|vm| vm.name.clone())
            .collect();
        v.sort();
        v
    }

    // ---- claims (D11) ------------------------------------------------------

    /// The VM (other than `except`) claiming disk `disk_id`: its spec
    /// references it, or a live instance of it has it open.
    pub(crate) fn disk_claimed_by(
        vms: &HashMap<String, Vm>,
        disk_id: &str,
        except: Option<&str>,
    ) -> Option<String> {
        vms.values()
            .filter(|vm| Some(vm.id.as_str()) != except)
            .find(|vm| {
                let c = vm.config();
                c.root_disk.as_deref() == Some(disk_id)
                    || c.data_disks.iter().any(|d| d == disk_id)
                    || vm
                        .status
                        .instance
                        .as_ref()
                        .is_some_and(|i| i.disks.iter().any(|d| d == disk_id))
            })
            .map(|vm| vm.name.clone())
    }

    /// The VFIO devices `vm` claims: its spec's, and those its instance
    /// still holds (a claim lasts until the instance is gone).
    fn devices_held(vm: &Vm) -> impl Iterator<Item = &String> {
        vm.config()
            .vfio_devices
            .iter()
            .chain(
                vm.status
                    .instance
                    .iter()
                    .flat_map(|i| i.vfio_devices.iter()),
            )
            .chain(
                vm.status
                    .placement
                    .iter()
                    .flat_map(|p| p.pool_devices.iter().map(|d| &d.address)),
            )
    }

    /// Refuse `devices` that a VM other than `except` claims on `node`,
    /// directly or through a shared IOMMU group (spec/host-topology.md
    /// §8.4). On a standalone host every VM is the node's.
    fn refuse_claimed_devices(
        &self,
        vms: &HashMap<String, Vm>,
        node: &str,
        devices: &[String],
        except: Option<&str>,
    ) -> Result<(), VmManagerError> {
        if devices.is_empty() {
            return Ok(());
        }
        let standalone = self.cluster().is_none();
        let mut claimed = BTreeMap::new();
        for vm in vms.values().filter(|v| Some(v.id.as_str()) != except) {
            let here = standalone || vm.status.placement.as_ref().is_some_and(|p| p.node == node);
            if here {
                for d in Self::devices_held(vm) {
                    claimed.insert(crate::scheduler::pci_address(d), vm.name.clone());
                }
            }
        }
        // A node that reports no groups: claims are by address only.
        let rec = self.nodes.get(node).ok().flatten();
        let cordoned = rec
            .as_ref()
            .map(|n| n.spec.cordoned_devices.clone())
            .unwrap_or_default();
        let groups = rec.map(|n| n.status.pci_groups).unwrap_or_default();
        // A cordoned device stays with the VM that has it (spec/host-topology.md §5.4).
        let had: Vec<String> = except
            .and_then(|id| vms.get(id))
            .map(|v| {
                Self::devices_held(v)
                    .map(|d| crate::scheduler::pci_address(d))
                    .collect()
            })
            .unwrap_or_default();
        for d in devices {
            let addr = crate::scheduler::pci_address(d);
            if cordoned.contains(&addr) && !had.contains(&addr) {
                return Err(ImageError::InUse(format!("PCI device {addr} is cordoned")).into());
            }
            if let Some(why) = crate::scheduler::device_taken(&groups, &claimed, d) {
                return Err(ImageError::InUse(why).into());
            }
        }
        Ok(())
    }

    /// A disk a VM of `project` may claim: exists, in that project,
    /// unclaimed (by anyone but `vm_id`), not busy.
    fn attachable_disk_in(
        &self,
        vms: &HashMap<String, Vm>,
        key: &str,
        project: &str,
        vm_id: Option<&str>,
    ) -> Result<Disk, VmManagerError> {
        let d = self.images.get_disk_in(key, self.tenant_scope(project))?;
        if d.project != project {
            return Err(invalid(format!(
                "disk {} belongs to another project",
                d.name
            )));
        }
        if let Some(other) = Self::disk_claimed_by(vms, &d.id, vm_id) {
            return Err(
                ImageError::InUse(format!("disk {} is attached to VM {}", d.name, other)).into(),
            );
        }
        if let Some(owner) = d
            .attached_to
            .as_deref()
            .filter(|o| Some(*o) != vm_id && vms.contains_key(*o))
        {
            return Err(
                ImageError::InUse(format!("disk {} is attached to VM {}", d.name, owner)).into(),
            );
        }
        if let Some(op) = self.images.busy_op(&d.id) {
            return Err(ImageError::Busy(format!("disk {} is busy ({})", d.name, op)).into());
        }
        Ok(d)
    }

    /// Bindings for a VM's managed disks, checked to be usable right now:
    /// the root disk (with its record) and the data disks.
    pub(crate) fn disk_bindings(
        &self,
        vm: &Vm,
    ) -> Result<(Option<RootBinding>, Vec<VolumeBinding>), VmManagerError> {
        let bind = |id: &str| -> Result<(VolumeBinding, Disk), VmManagerError> {
            let d = self.images.get_disk(id)?;
            if d.phase != images::DiskPhase::Ready {
                let why = d
                    .conditions
                    .iter()
                    .find(|c| c.kind == "Ready")
                    .map(|c| format!(": {}", c.message))
                    .unwrap_or_default();
                return Err(ImageError::NotReady(format!(
                    "disk {} is {:?}{}",
                    d.name, d.phase, why
                ))
                .into());
            }
            if let Some(op) = self.images.busy_op(&d.id) {
                return Err(ImageError::Busy(format!("disk {} is busy ({})", d.name, op)).into());
            }
            let b = self.images.driver(&d)?.stage(&d)?;
            Ok((b, d))
        };
        let root = vm.config().root_disk.as_deref().map(bind).transpose()?;
        let data = vm
            .config()
            .data_disks
            .iter()
            .map(|id| bind(id).map(|(b, _)| b))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((root, data))
    }

    /// This node's copy of a VM's firmware image, checked to exist: the
    /// stored `firmware_path` is the creating server's, which another node
    /// need not share (spec/storage.md §15). `None` without a firmware image.
    pub(crate) fn firmware_binding(&self, vm: &Vm) -> Result<Option<String>, VmManagerError> {
        let Some(id) = vm.config().firmware_image.as_deref() else {
            return Ok(None);
        };
        let path = self.images.firmware_path(id);
        if !path.exists() {
            let name = self
                .images
                .get_image(id)
                .map(|i| i.name)
                .unwrap_or_else(|_| id.to_string());
            return Err(ImageError::NotReady(format!(
                "firmware {name} has not been copied to this node yet"
            ))
            .into());
        }
        Ok(Some(path.to_string_lossy().into_owned()))
    }

    // ---- desired power state ----------------------------------------------

    /// Start a VM, quotas not enforced (embedding and tests); returns at
    /// once, see [`Self::wait_converged`].
    pub async fn start_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        self.set_power(
            vm_id,
            PowerState::Running,
            None,
            QuotaMode::MayExceed,
            "api",
            None,
        )
        .await
        .map(|(vm, _)| vm)
    }

    pub async fn stop_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        self.set_power(
            vm_id,
            PowerState::Stopped,
            None,
            QuotaMode::MayExceed,
            "api",
            None,
        )
        .await
        .map(|(vm, _)| vm)
    }

    pub async fn pause_vm(&self, vm_id: &str) -> Result<Vm, VmManagerError> {
        self.set_power(
            vm_id,
            PowerState::Paused,
            None,
            QuotaMode::MayExceed,
            "api",
            None,
        )
        .await
        .map(|(vm, _)| vm)
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
            return Err(invalid(format!(
                "graceful_timeout_secs must be 0-{}",
                MAX_STOP_GRACE_SECS
            )));
        }
        let mut vms = self.vms.write().await;
        let cur = Self::writable(&vms, vm_id, if_match)?.clone();
        let mut bypassed = Vec::new();
        if cur.spec.power == PowerState::Stopped
            && power != PowerState::Stopped
            && let Some(p) = self.projects.get(&cur.project)?
        {
            let usage = self.usage_locked(&cur.project, &vms)?;
            self.apply_quotas(
                &p,
                &usage,
                &Delta {
                    running_vms: 1,
                    ..Default::default()
                },
                quota,
                &mut bypassed,
                &vms,
            )?;
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
                return Err(invalid(format!(
                    "immutable field(s): {}",
                    fields.join(", ")
                )));
            }
        }
        Self::check_options(&VmOptions {
            stop_grace_secs: patch.stop_grace_secs,
            ..Default::default()
        })?;
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
        if let Some(dn) = patch.device_nodes {
            vm.spec.device_nodes = dn;
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
            if let Some(p) = c.numa_policy {
                config.numa_policy = p;
            }
            if let Some(p) = c.cpu_policy {
                config.cpu_policy = p;
            }
            if let Some(d) = c.devices {
                let (old, new) = (pool_counts(&config.devices), pool_counts(&d));
                delta.devices = new
                    .into_iter()
                    .map(|(pool, n)| {
                        let more = n.saturating_sub(old.get(&pool).copied().unwrap_or(0));
                        (pool, more)
                    })
                    .filter(|&(_, more)| more > 0)
                    .collect();
                config.devices = d;
            }
            match c.hugepage_size {
                None => {}
                Some(serde_json::Value::Null) => config.hugepage_size = None,
                Some(serde_json::Value::String(s)) => config.hugepage_size = Some(s),
                Some(_) => return Err(invalid("hugepage_size must be 2M, 1G or null")),
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
                // Against the VM's node; an unplaced VM's are checked when
                // the scheduler places it.
                let node = if self.cluster().is_none() {
                    Some(self.local_node_id())
                } else {
                    cur.status.placement.as_ref().map(|p| p.node.clone())
                };
                if let Some(node) = node {
                    self.refuse_claimed_devices(&vms, &node, &devs, Some(vm_id))?;
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
                self.check_networks(&project, &mut nets)?;
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
                    // A disk is a local file: it goes where its VM is (D6).
                    let vm_node = cur.status.placement.as_ref().map(|p| p.node.clone());
                    if let (Some(dn), Some(vn)) = (&d.node, &vm_node)
                        && dn != vn
                    {
                        return Err(invalid(format!(
                            "disk {} is on another node than the VM",
                            d.name
                        )));
                    }
                    let needs_bind = d.node.is_none() && vm_node.is_some();
                    if d.attached_to.as_deref() != Some(vm_id) || needs_bind {
                        let mut d = d;
                        d.attached_to = Some(vm_id.to_string());
                        if needs_bind {
                            d.node = vm_node;
                        }
                        disk_writes.push(d);
                    }
                }
                for old in &cur.spec.config.data_disks {
                    if !ids.contains(old)
                        && let Some(d) = self.release_claim_if_unused(&cur, old)
                    {
                        disk_writes.push(d);
                    }
                }
                vm.spec.config.data_disks = ids;
            }
            self.check_paths(&vm.spec.config)?;
            self.check_credential(&project, &vm.spec.config)?;
            Self::check_hugepages(&vm.spec.config)?;
            let (a, b) = (cur.config(), vm.config());
            let fitted = |c: &VmConfig| {
                (
                    c.vcpu_count,
                    c.mem_size_mib,
                    c.hugepages,
                    c.hugepage_size.clone(),
                    c.networks.clone(),
                    c.vfio_devices.clone(),
                    c.numa_policy,
                    c.cpu_policy,
                    c.devices.clone(),
                )
            };
            if fitted(a) != fitted(b) {
                self.refit_placement(&vms, &mut vm, &[])?;
            }
        }
        if vm.spec == cur.spec {
            return Ok((cur, Vec::new()));
        }
        let mut bypassed = Vec::new();
        if (delta.running_vms + delta.vcpus + delta.memory_mib > 0 || !delta.devices.is_empty())
            && let Some(p) = self.projects.get(&project)?
        {
            let usage = self.usage_locked(&project, &vms)?;
            self.apply_quotas(&p, &usage, &delta, quota, &mut bypassed, &vms)?;
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
        if vm
            .status
            .instance
            .as_ref()
            .is_some_and(|i| i.disks.iter().any(|d| d == disk_id))
        {
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

    /// Add a VFIO device; `device_nodes` is where the project may use all
    /// of the VM's devices then (spec/host-topology.md §8.5).
    pub async fn attach_device(
        &self,
        vm_id: &str,
        device_path: String,
        device_nodes: Option<Vec<String>>,
    ) -> Result<Vm, VmManagerError> {
        let vm = self.get_vm(vm_id).await?;
        if vm.config().vfio_devices.contains(&device_path) {
            return Err(VmManagerError::InvalidState {
                current: vm.state(),
                operation: format!("attach_device: {} is already attached", device_path),
            });
        }
        let mut devs = vm.config().vfio_devices.clone();
        devs.push(device_path);
        let patch = VmPatch {
            config: Some(ConfigPatch {
                vfio_devices: Some(devs),
                ..Default::default()
            }),
            device_nodes: Some(device_nodes),
            ..Default::default()
        };
        self.patch_vm(vm_id, patch, QuotaMode::MayExceed, "api", None)
            .await
            .map(|(vm, _)| vm)
    }

    pub async fn detach_device(
        &self,
        vm_id: &str,
        device_path: &str,
    ) -> Result<Vm, VmManagerError> {
        let vm = self.get_vm(vm_id).await?;
        let mut devs = vm.config().vfio_devices.clone();
        let Some(pos) = devs.iter().position(|d| d == device_path) else {
            return Err(VmManagerError::InvalidState {
                current: vm.state(),
                operation: format!("detach_device: {} is not attached", device_path),
            });
        };
        devs.remove(pos);
        let patch = VmPatch {
            config: Some(ConfigPatch {
                vfio_devices: Some(devs),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.patch_vm(vm_id, patch, QuotaMode::MayExceed, "api", None)
            .await
            .map(|(vm, _)| vm)
    }

    /// Attach a data disk; on a running VM it takes effect at the next
    /// launch (`RestartRequired`, §7.3).
    pub async fn attach_disk(&self, vm_id: &str, key: &str) -> Result<Vm, VmManagerError> {
        let vm = self.get_vm(vm_id).await?;
        let d = self
            .images
            .get_disk_in(key, self.tenant_scope(&vm.project))?;
        let mut keys = vm.config().data_disks.clone();
        keys.push(d.id);
        let patch = VmPatch {
            config: Some(ConfigPatch {
                data_disks: Some(keys),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.patch_vm(vm_id, patch, QuotaMode::MayExceed, "api", None)
            .await
            .map(|(vm, _)| vm)
    }

    pub async fn detach_disk(&self, vm_id: &str, key: &str) -> Result<Vm, VmManagerError> {
        let vm = self.get_vm(vm_id).await?;
        let disk = self
            .images
            .get_disk_in(key, self.tenant_scope(&vm.project))?;
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
        let patch = VmPatch {
            config: Some(ConfigPatch {
                data_disks: Some(keys),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.patch_vm(vm_id, patch, QuotaMode::MayExceed, "api", None)
            .await
            .map(|(vm, _)| vm)
    }

    // ---- delete (§6.3) ---------------------------------------------------------

    #[allow(dead_code)] // used by the tests through the lib crate
    pub async fn delete_vm(&self, vm_id: &str) -> Result<bool, VmManagerError> {
        self.delete_vm_with(vm_id, false).await
    }

    /// Request deletion. Returns `true` when the VM is already gone (it had
    /// no instance, ports or anything else to clean up), `false` when the
    /// controller finishes it.
    pub async fn delete_vm_with(
        &self,
        vm_id: &str,
        keep_disk: bool,
    ) -> Result<bool, VmManagerError> {
        self.delete_vm_as(vm_id, keep_disk, "api").await
    }

    pub async fn delete_vm_as(
        &self,
        vm_id: &str,
        keep_disk: bool,
        actor: &str,
    ) -> Result<bool, VmManagerError> {
        let vm = {
            let mut vms = self.vms.write().await;
            let cur = vms
                .get(vm_id)
                .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?
                .clone();
            if cur.deletion_requested_at.is_some() {
                return Ok(false);
            }
            for id in cur
                .config()
                .root_disk
                .iter()
                .chain(cur.config().data_disks.iter())
            {
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
            let ev = Event::new(
                actor,
                EventKind::Normal,
                "Deleting",
                if keep_disk {
                    "deletion requested (keeping its root disk)"
                } else {
                    "deletion requested"
                },
            );
            self.put_locked(&mut vms, vm, vec![ev])?
        };
        // Nothing ran: finish now, so a never-started VM is gone at once.
        if vm.status.instance.is_none()
            && vm.status.nics.is_empty()
            && matches!(vm.status.phase, VmPhase::Stopped | VmPhase::Failed)
        {
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
        self.vm(vm_id)
            .await
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))
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
            if tokio::time::timeout_at(deadline, rx.changed())
                .await
                .is_err()
            {
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
            let Ok(d) = self.images.get_disk(key) else {
                return (true, None);
            };
            let ready = d.conditions.iter().find(|c| c.kind == "Ready");
            let decided_pending = ready.is_some_and(|c| {
                matches!(
                    c.reason.as_str(),
                    "ResizePending"
                        | "ResizeInvalid"
                        | "ExtendRootPending"
                        | "InvalidDisk"
                        | "FileMissing"
                )
            });
            let settled = d.deletion_requested_at.is_none()
                && match d.phase {
                    images::DiskPhase::Failed | images::DiskPhase::Missing => true,
                    images::DiskPhase::Ready => {
                        decided_pending
                            || (d.resize.is_none()
                                && d.extend_root
                                    .is_none_or(|e| e.seq <= d.applied_extend_root_seq))
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
    pub async fn wait_gone<T>(
        &self,
        timeout: Duration,
        mut look: impl FnMut() -> Option<T>,
    ) -> Option<T> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.changed.subscribe();
        loop {
            rx.borrow_and_update();
            let seen = look()?;
            // The bell, or a slow tick in case a change was missed.
            let tick = tokio::time::Instant::now() + Duration::from_secs(1);
            if tokio::time::timeout_at(deadline.min(tick), rx.changed())
                .await
                .is_err()
                && tokio::time::Instant::now() >= deadline
            {
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
            if let Ok((vm, _)) = self
                .set_power(
                    &id,
                    PowerState::Stopped,
                    Some(0),
                    QuotaMode::MayExceed,
                    "controller",
                    None,
                )
                .await
            {
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

    pub fn image_catalog(&self, arch: Option<crate::images::catalog::Arch>) -> Vec<CatalogItem> {
        self.images.catalog(arch)
    }

    /// Import the firmware available locally, once per catalog entry
    /// (`ImageManager::auto_import_firmware`); called at startup.
    pub async fn auto_import_firmware(&self) -> Vec<(&'static str, images::AutoImport)> {
        self.images.auto_import_firmware().await
    }

    /// The firmware image VMs of `hypervisor` boot through by default.
    pub fn default_firmware_name(
        &self,
        hypervisor: crate::hypervisor::HypervisorType,
    ) -> Option<String> {
        self.images
            .default_firmware(hypervisor, crate::images::catalog::Arch::host())
            .map(|i| i.name)
    }

    pub fn firmware_catalog(
        &self,
        arch: Option<crate::images::catalog::Arch>,
    ) -> Vec<images::FirmwareCatalogItem> {
        self.images.firmware_catalog(arch)
    }

    /// An image response with the VMs booting through it.
    async fn with_users(&self, mut r: ImageResponse) -> ImageResponse {
        if r.kind == images::ImageKind::Firmware {
            r.used_by_vms = Self::firmware_users(&*self.vms.read().await, &r.id);
        }
        r.copies = self.image_copies(&r.id);
        r
    }

    /// Each node's copy of image `id`, in a cluster.
    fn image_copies(&self, id: &str) -> Vec<images::ImageCopy> {
        if self.cluster().is_none() {
            return Vec::new();
        }
        let mut out: Vec<images::ImageCopy> = self
            .image_caches(id)
            .into_iter()
            .map(|(node, row)| images::ImageCopy {
                node_name: self.nodes.get(&node).ok().flatten().map(|n| n.spec.name),
                node,
                state: match row.phase {
                    crate::controller::image_cache::CachePhase::Downloading => "downloading",
                    crate::controller::image_cache::CachePhase::Ready => "ready",
                    crate::controller::image_cache::CachePhase::Failed => "failed",
                }
                .to_string(),
                bytes: row.bytes,
                error: row.error,
                pinned: false,
            })
            .collect();
        // Copies wanted on chosen nodes, and copies asked to go (§9.2.2).
        if let Ok(img) = self.images.get_image(id) {
            for c in out.iter_mut() {
                c.pinned = img.pinned.contains(&c.node);
                if img.release.contains(&c.node) {
                    c.state = "releasing".into();
                }
            }
            let missing: Vec<&String> = img
                .pinned
                .iter()
                .filter(|p| !out.iter().any(|c| c.node == **p))
                .collect();
            for p in missing {
                out.push(images::ImageCopy {
                    node_name: self.nodes.get(p).ok().flatten().map(|n| n.spec.name),
                    node: p.clone(),
                    state: "wanted".into(),
                    bytes: 0,
                    error: String::new(),
                    pinned: true,
                });
            }
        }
        out.sort_by(|a, b| a.node_name.cmp(&b.node_name));
        out
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
                r.copies = self.image_copies(&i.id);
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

    pub async fn pull_image(
        &self,
        mut req: PullImageRequest,
    ) -> Result<(ImageResponse, bool), VmManagerError> {
        // In a cluster: copies on chosen nodes, and their architecture (§9.2.1–2).
        if !req.nodes.is_empty() || req.keep_server_copy == Some(false) {
            if self.cluster().is_none() {
                return Err(ImageError::InvalidImage(
                    "nodes and keep_server_copy are for a cluster".into(),
                )
                .into());
            }
            if req.nodes.is_empty() {
                return Err(ImageError::InvalidImage(
                    "keep_server_copy: false needs nodes to keep the copies".into(),
                )
                .into());
            }
            let nodes: Vec<crate::node::Node> = req
                .nodes
                .iter()
                .map(|n| self.copy_node(n))
                .collect::<Result<_, _>>()?;
            let archs: std::collections::BTreeSet<&str> = nodes
                .iter()
                .map(|n| n.status.features.arch.as_str())
                .filter(|a| !a.is_empty())
                .collect();
            if archs.len() > 1 {
                return Err(ImageError::InvalidImage(format!(
                    "the nodes are of different architectures ({}): pull once per architecture",
                    archs.into_iter().collect::<Vec<_>>().join(", ")
                ))
                .into());
            }
            match (&req.arch, archs.first()) {
                (Some(a), Some(n)) if a != n => {
                    return Err(ImageError::InvalidImage(format!(
                        "arch {a}, but the nodes are {n}"
                    ))
                    .into());
                }
                (None, Some(n)) => req.arch = Some(n.to_string()),
                _ => {}
            }
            req.nodes = nodes.into_iter().map(|n| n.meta.id).collect();
        }
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
                return Err(ImageError::InUse(format!(
                    "firmware {} is used by VM(s): {}",
                    img.name,
                    users.join(", ")
                ))
                .into());
            }
            self.images.request_image_delete(key)?
        };
        self.reconcile_image(&img.id).await?;
        Ok(self
            .images
            .get_image(&img.id)
            .ok()
            .map(|i| self.images.image_response(&i, false)))
    }

    pub fn list_disks(&self) -> Vec<DiskResponse> {
        self.images
            .list_disks()
            .iter()
            .map(|d| self.images.disk_response(d, false))
            .collect()
    }

    /// Ids of the disks attached to a VM, from the records alone (no
    /// per-disk response or file check).
    pub fn attached_disk_ids(&self, vm_id: &str) -> std::collections::BTreeSet<String> {
        self.images
            .list_disks()
            .into_iter()
            .filter(|d| d.attached_to.as_deref() == Some(vm_id))
            .map(|d| d.id)
            .collect()
    }

    /// Which projects share `project`'s tenant: the scope disk names are
    /// unique and resolved in (spec/tenancy.md D24).
    pub fn tenant_scope(&self, project: &str) -> impl Fn(&str) -> bool + use<> {
        let all = self.projects.list().unwrap_or_default();
        let tenant = all
            .iter()
            .find(|p| p.id == project)
            .map(|p| p.tenant_id.clone());
        let ids: std::collections::HashSet<String> = all
            .into_iter()
            .filter(|p| Some(&p.tenant_id) == tenant.as_ref())
            .map(|p| p.id)
            .collect();
        let me = project.to_string();
        move |p: &str| p == me || ids.contains(p)
    }

    /// Disk `key` (id, or a name in `project`'s tenant), as the API shows it.
    pub async fn get_disk_for(
        &self,
        project: &str,
        key: &str,
    ) -> Result<DiskResponse, VmManagerError> {
        let d = self.images.get_disk_in(key, self.tenant_scope(project))?;
        let mgr = self.images.clone();
        Ok(blocking(move || Ok(mgr.disk_response(&d, true))).await?)
    }

    pub async fn get_disk(&self, key: &str) -> Result<DiskResponse, VmManagerError> {
        let d = self.images.get_disk(key)?;
        let mgr = self.images.clone();
        Ok(blocking(move || Ok(mgr.disk_response(&d, true))).await?)
    }

    /// Create a disk in the default project (embedding and tests).
    pub async fn create_disk(
        &self,
        req: CreateDiskRequest,
    ) -> Result<DiskResponse, VmManagerError> {
        let project = self.default_project_id();
        self.create_disk_in(&project, req, QuotaMode::MayExceed)
            .await
            .map(|(d, _)| d)
    }

    /// What a pending disk will take, for quotas: its requested size, else
    /// its image's size or the default root size.
    fn estimated_size(&self, d: &Disk) -> u64 {
        self.images.estimated_size(d)
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
        let p = self
            .projects
            .get(project)?
            .ok_or_else(|| TenancyError::NotFound(project.to_string()))?;
        req.project = Some(project.to_string());
        // Bound to a node by id or name, like a VM's pin; stored as the id,
        // which is what `is_local_disk` compares (spec/storage.md §15).
        if let Some(want) = &req.node {
            req.node = Some(self.resolve_node(want)?);
        }
        // The VM lock serializes quota checks with VM creation.
        let vms = self.vms.write().await;
        let disk = self
            .images
            .new_disk_record(&req, self.tenant_scope(project))?;
        let usage = self.usage_locked(project, &vms)?;
        let mut bypassed = Vec::new();
        let delta = Delta {
            disk_gib: gib_ceil(self.estimated_size(&disk)),
            ..Default::default()
        };
        self.apply_quotas(&p, &usage, &delta, quota, &mut bypassed, &vms)?;
        self.store.commit(Commit {
            put_disks: vec![&disk],
            events: vec![(
                event_key("disk", &disk.id),
                Event::new(
                    "api",
                    EventKind::Normal,
                    "Created",
                    "disk recorded; its file is made next",
                ),
            )],
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
                vm.status
                    .instance
                    .as_ref()
                    .is_some_and(|i| i.disks.iter().any(|d| d == id))
                    || (matches!(vm.status.phase, VmPhase::Provisioning | VmPhase::Starting)
                        && (vm.config().root_disk.as_deref() == Some(id)
                            || vm.config().data_disks.iter().any(|d| d == id)))
            })
            .map(|vm| vm.name.clone())
    }

    /// Mark a disk busy for `op`, refusing while a live instance has it
    /// open. Holds the VM read lock while checking, so a launch (which
    /// records the instance's disks under the write lock) cannot slip in
    /// between the check and the mark.
    pub(crate) async fn begin_disk_op(
        &self,
        key: &str,
        op: &'static str,
    ) -> Result<(Disk, images::BusyGuard), VmManagerError> {
        let vms = self.vms.read().await;
        let disk = self.images.get_disk(key)?;
        if let Some(vm) = Self::disk_in_use(&vms, &disk.id) {
            return Err(ImageError::InUse(format!(
                "disk {} is in use by VM {}; stop it first",
                disk.name, vm
            ))
            .into());
        }
        let guard = self.images.begin(&disk.id, op)?;
        Ok((disk, guard))
    }

    pub async fn resize_disk(
        &self,
        key: &str,
        req: ResizeDiskRequest,
    ) -> Result<DiskResponse, VmManagerError> {
        self.resize_disk_with(key, req, QuotaMode::MayExceed)
            .await
            .map(|(d, _)| d)
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
        let size = requested_size(req.size_gib, req.size_bytes)?
            .ok_or_else(|| ImageError::invalid_disk("give size_gib or size_bytes"))?;
        let current = self.images.get_disk(key)?;
        if current.deletion_requested_at.is_some() {
            return Err(
                ImageError::InUse(format!("disk {} is being deleted", current.name)).into(),
            );
        }
        if !matches!(
            current.phase,
            images::DiskPhase::Ready | images::DiskPhase::Resizing
        ) {
            return Err(ImageError::NotReady(format!(
                "disk {} is {:?}",
                current.name, current.phase
            ))
            .into());
        }
        let mut bypassed = Vec::new();
        let grow = gib_ceil(size).saturating_sub(gib_ceil(current.size_bytes));
        if grow > 0
            && let Some(p) = self.projects.get(&current.project)?
        {
            let vms = self.vms.read().await;
            let usage = self.usage_locked(&current.project, &vms)?;
            self.apply_quotas(
                &p,
                &usage,
                &Delta {
                    disk_gib: grow,
                    ..Default::default()
                },
                quota,
                &mut bypassed,
                &vms,
            )?;
        }
        // Answered early only where the file is: elsewhere the disk's node
        // checks it and reports `ResizeInvalid` (spec/storage.md §8.2).
        if size < current.size_bytes && self.is_local_disk(&current) {
            let (mgr, d) = (self.images.clone(), current.clone());
            let (min, _) = blocking(move || mgr.shrink_minimum(&d)).await?;
            if size < min {
                return Err(ImageError::InvalidDisk {
                    message: format!(
                        "{} bytes would cut into a partition; the minimum is {} bytes",
                        size, min
                    ),
                    details: serde_json::json!({ "min_size_bytes": min }),
                }
                .into());
            }
        }
        let mut disk = current;
        disk.resize = (size != disk.size_bytes).then_some(images::ResizeSpec {
            size_bytes: size,
            extend_root: req.extend_root,
        });
        self.store.commit(Commit {
            put_disks: vec![&disk],
            events: vec![(
                event_key("disk", &disk.id),
                Event::new(
                    "api",
                    EventKind::Normal,
                    "ResizeRequested",
                    format!("size {} bytes", size),
                ),
            )],
            ..Default::default()
        })?;
        self.images.cache_disk(&disk);
        self.queue.add(Key::Disk(disk.id.clone()));
        Ok((self.images.disk_response(&disk, false), bypassed))
    }

    /// Request a root-partition extend (a one-shot action, D19): bumps the
    /// disk's `extend_root.seq`. A disk without a growable root partition
    /// is refused here.
    pub async fn extend_root(
        &self,
        key: &str,
        mode: ExtendMode,
    ) -> Result<DiskResponse, VmManagerError> {
        let current = self.images.get_disk(key)?;
        if current.phase != images::DiskPhase::Ready || current.deletion_requested_at.is_some() {
            return Err(ImageError::NotReady(format!(
                "disk {} is {:?}",
                current.name, current.phase
            ))
            .into());
        }
        // As for a shrink: elsewhere the disk's node checks it and reports
        // `ExtendRootFailed` (spec/storage.md §8.2).
        if self.is_local_disk(&current) {
            let (mgr, d) = (self.images.clone(), current.clone());
            blocking(move || {
                let path = mgr.disk_path(&d);
                let table = images::partition::read_table(&path, d.format, d.size_bytes)?
                    .ok_or_else(|| ImageError::invalid_disk("disk has no partition table"))?;
                images::partition::growable_root(&table).map(|_| ())
            })
            .await?;
        }
        let mut disk = current;
        let seq = disk
            .extend_root
            .map(|e| e.seq)
            .unwrap_or(0)
            .max(disk.applied_extend_root_seq)
            + 1;
        disk.extend_root = Some(images::ExtendRootSpec { mode, seq });
        self.store.commit(Commit {
            put_disks: vec![&disk],
            events: vec![(
                event_key("disk", &disk.id),
                Event::new(
                    "api",
                    EventKind::Normal,
                    "ExtendRootRequested",
                    format!("{:?}, request {}", mode, seq),
                ),
            )],
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
                return Err(ImageError::InUse(format!(
                    "disk {} is attached to VM {}; detach it or delete the VM first",
                    disk.name, vm
                ))
                .into());
            }
            if let Some(op) = self.images.busy_op(&disk.id) {
                return Err(
                    ImageError::Busy(format!("disk {} is busy ({})", disk.name, op)).into(),
                );
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
    /// The addresses of this host's OVS-DPDK uplinks, from netd.
    fn dpdk_uplink_bdfs(&self) -> std::collections::BTreeSet<String> {
        self.netd
            .call::<Vec<glidex_netd::proto::UplinkResult>>(Op::ListUplinks)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|u| match u.record.spec.kind {
                glidex_ovs::uplink::UplinkKind::Dpdk { pci, .. } => Some(pci.to_ascii_lowercase()),
                _ => None,
            })
            .collect()
    }

    /// This host's full topology (spec/host-topology.md §3.2): its node
    /// record's summary (OVS-DPDK's share and the reserved CPUs applied)
    /// and the PCIe tree read now.
    pub fn local_topology(&self) -> crate::topology::TopologyTree {
        let summary = self
            .nodes
            .get(&self.local_node_id())
            .ok()
            .flatten()
            .and_then(|n| n.status.topology)
            .or_else(crate::topology::read)
            .unwrap_or_default();
        crate::topology::read_tree(summary, self.dpdk_uplink_bdfs())
    }

    /// Bind this node's devices of `prepare: true` pools that aren't
    /// ready to `vfio-pci` (spec/host-topology.md §5.3, §9.1), then
    /// report. A device netd refuses stays not ready; the pool is short of
    /// it and the warning says why.
    pub fn prepare_pools(&self) {
        let me = self.local_node_id();
        let name = self
            .nodes
            .get(&me)
            .ok()
            .flatten()
            .map(|n| n.spec.name)
            .unwrap_or_default();
        let pools: Vec<_> = self
            .pci_pools()
            .iter()
            .filter(|p| p.prepare)
            .filter(|p| p.nodes.is_empty() || p.nodes.iter().any(|n| *n == me || *n == name))
            .cloned()
            .collect();
        if pools.is_empty() {
            return;
        }
        let mut prepared = 0;
        for address in self.local_topology().to_prepare(&pools) {
            match device_op_on(&self.netd, &address, "prepare") {
                Ok(_) => {
                    tracing::info!("prepared {address} for its pool");
                    prepared += 1;
                }
                Err(e) => tracing::warn!("preparing {address} for its pool: {e}"),
            }
        }
        if prepared > 0
            && let Err(e) = self.report_self()
        {
            tracing::warn!("reporting this node after preparing pool devices: {e}");
        }
    }

    /// `prepare` or `release` this host's PCI device `address` through
    /// netd (spec/host-topology.md §5.3).
    pub fn local_device_op(
        &self,
        address: &str,
        op: &str,
    ) -> Result<serde_json::Value, VmManagerError> {
        let r = device_op_on(&self.netd, address, op)?;
        // Readiness changed: pools see it at once (§9).
        if let Err(e) = self.report_self() {
            tracing::warn!("reporting this node after a device {op}: {e}");
        }
        Ok(r)
    }

    /// `prepare` or `release` node `id`'s device `address`: here, or on that
    /// node over the cluster's node API (`/cluster/v1/devices/...`).
    pub async fn node_device_op(
        &self,
        id: &str,
        address: &str,
        op: &str,
    ) -> Result<serde_json::Value, VmManagerError> {
        if id == self.local_node_id() {
            let (netd, a, o) = (self.netd.clone(), address.to_string(), op.to_string());
            let r = tokio::task::spawn_blocking(move || device_op_on(&netd, &a, &o))
                .await
                .map_err(|e| VmManagerError::Network(NetError::Unavailable(e.to_string())))??;
            if let Err(e) = self.report_self() {
                tracing::warn!("reporting this node after a device {op}: {e}");
            }
            return Ok(r);
        }
        let unavailable = |m: String| VmManagerError::Network(NetError::Unavailable(m));
        let cluster = self
            .cluster()
            .ok_or_else(|| unavailable("this host is not part of a cluster".into()))?;
        let node = self
            .nodes
            .get(id)
            .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?
            .ok_or_else(|| VmManagerError::Network(NetError::NotFound(format!("node {id}"))))?;
        let addr = node
            .status
            .advertise
            .ok_or_else(|| unavailable(format!("node {} has no address", node.spec.name)))?;
        let r = cluster
            .client
            .request_timeout(
                &addr.to_string(),
                hyper::Method::POST,
                &format!("/cluster/v1/devices/{address}/{op}"),
                &[],
                bytes::Bytes::new(),
                std::time::Duration::from_secs(60),
            )
            .await
            .map_err(|e| unavailable(format!("node {}: {e}", node.spec.name)))?;
        let body: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or_default();
        if r.status.is_success() {
            return Ok(body);
        }
        // The node's netd refused: pass its answer on.
        Err(VmManagerError::Network(NetError::Netd(
            glidex_netd::proto::ErrorBody {
                code: body["error"].as_str().unwrap_or("unavailable").to_string(),
                message: format!(
                    "node {}: {}",
                    node.spec.name,
                    body["message"].as_str().unwrap_or("no answer")
                ),
                details: serde_json::Value::Null,
            },
        )))
    }

    /// Node `id`'s full topology: this host's, or read on that node over
    /// the cluster's node API (`/cluster/v1/topology`).
    pub async fn node_topology(
        &self,
        id: &str,
    ) -> Result<crate::topology::TopologyTree, VmManagerError> {
        if id == self.local_node_id() {
            return Ok(self.local_topology());
        }
        let unavailable = |m: String| VmManagerError::Network(NetError::Unavailable(m));
        let cluster = self
            .cluster()
            .ok_or_else(|| unavailable("this host is not part of a cluster".into()))?;
        let node = self
            .nodes
            .get(id)
            .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?
            .ok_or_else(|| VmManagerError::Network(NetError::NotFound(format!("node {id}"))))?;
        let addr = node
            .status
            .advertise
            .ok_or_else(|| unavailable(format!("node {} has no address", node.spec.name)))?;
        let r = cluster
            .client
            .request_timeout(
                &addr.to_string(),
                hyper::Method::GET,
                "/cluster/v1/topology",
                &[],
                bytes::Bytes::new(),
                std::time::Duration::from_secs(10),
            )
            .await
            .map_err(|e| unavailable(format!("node {}: {e}", node.spec.name)))?;
        if !r.status.is_success() {
            return Err(unavailable(format!(
                "node {} answered {}",
                node.spec.name, r.status
            )));
        }
        serde_json::from_slice(&r.body).map_err(|e| unavailable(e.to_string()))
    }

    fn refuse_dpdk_uplink_devices(&self, devices: &[String]) -> Result<(), VmManagerError> {
        if devices.is_empty() {
            return Ok(());
        }
        let Ok(uplinks) = self
            .netd
            .call::<Vec<glidex_netd::proto::UplinkResult>>(Op::ListUplinks)
        else {
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

    /// By id.
    pub fn get_network(&self, id: &str) -> Result<Network, VmManagerError> {
        Ok(self
            .networks
            .get(id)?
            .ok_or_else(|| NetError::NotFound(id.to_string()))?)
    }

    /// Network `key` as `project` sees it (spec/tenancy.md D24): its id,
    /// or a name: of a host network, or of a project network in the
    /// project's tenant.
    pub fn find_network(&self, project: &str, key: &str) -> Result<Network, VmManagerError> {
        self.find_network_in(key, self.tenant_scope(project))
    }

    /// Network `key`: its id, or the name of one network among the host
    /// networks and the project networks of the projects `in_scope` accepts
    /// (`409 ambiguous` when several have it).
    pub fn find_network_in(
        &self,
        key: &str,
        in_scope: impl Fn(&str) -> bool,
    ) -> Result<Network, VmManagerError> {
        if let Some(n) = self.networks.get(key)? {
            return Ok(n);
        }
        let all = self.networks.list()?;
        let mut named = all
            .into_iter()
            .filter(|n| n.name == key && n.project.as_deref().is_none_or(&in_scope));
        match (named.next(), named.next()) {
            (Some(n), None) => Ok(n),
            (Some(_), Some(_)) => Err(NetError::Ambiguous(format!(
                "more than one network is named {key}: give its id"
            ))
            .into()),
            (None, _) => Err(NetError::NotFound(key.to_string()).into()),
        }
    }

    /// Why `name` can't be a new network's: a host network has it, or (for
    /// a project network) one in the same tenant does; a host network's
    /// name is unique among all networks (D24).
    fn network_name_taken(
        &self,
        name: &str,
        project: Option<&str>,
    ) -> Result<Option<String>, VmManagerError> {
        let scope = project.map(|p| self.tenant_scope(p));
        let taken = self.networks.list()?.into_iter().find(|n| {
            n.name == name
                && match (&n.project, &scope) {
                    (None, _) | (_, None) => true,
                    (Some(p), Some(in_tenant)) => in_tenant(p),
                }
        });
        Ok(taken.map(|other| {
            if other.deletion_requested_at.is_some() {
                format!("network '{name}' is being deleted; try again once it is gone")
            } else {
                format!("network '{name}' already exists")
            }
        }))
    }

    /// Whether networks are cluster networks (OVN) unless they say otherwise.
    pub fn ovn_enabled(&self) -> bool {
        self.cluster().is_some() && self.ovn.load().enabled
    }

    /// A cluster network (§11.2): a subnet from the cluster IPAM and a record;
    /// the leader's network controller makes the OVN objects.
    async fn create_cluster_network(
        &self,
        req: CreateNetworkRequest,
        project: Option<&str>,
    ) -> Result<Network, VmManagerError> {
        if !self.ovn_enabled() {
            return Err(NetError::Invalid(
                "cluster networks need OVN: enable cluster.ovn on a cluster".into(),
            )
            .into());
        }
        let provider = req.mode == NetworkMode::Bridged;
        if provider && req.physnet.is_none() {
            return Err(NetError::Invalid("a provider network (mode bridged) needs a physnet: the physical network it sits on".into()).into());
        }
        if !provider && (req.physnet.is_some() || req.vlan.is_some()) {
            return Err(NetError::Invalid(
                "physnet and vlan belong to provider networks (mode bridged)".into(),
            )
            .into());
        }
        if req.bridge.is_some() {
            return Err(
                NetError::Invalid("a cluster network has no bridge of its own".into()).into(),
            );
        }
        if provider && req.subnet.is_some() {
            return Err(NetError::Invalid(
                "a provider network's addresses come from its LAN".into(),
            )
            .into());
        }
        if req.router.is_some() && req.mode == NetworkMode::Bridged {
            return Err(NetError::Invalid(
                "only a NAT or isolated network attaches to a VPC router".into(),
            )
            .into());
        }
        if let Some(id) = &req.router
            && crate::router::get(&self.store.database(), id)
                .map_err(router_err)?
                .is_none()
        {
            return Err(NetError::Invalid(format!("router {id} not found")).into());
        }
        // The subnet comes from the IPAM below; `to_network` only checks the rest.
        if req.mode == NetworkMode::Nat && self.ovn.load().edge.is_none() {
            return Err(NetError::Invalid("a NAT network that spans every node needs the cluster's shared edge (its external address and gateway nodes), which isn't set: an administrator sets it on the Cluster page under OVN settings (PUT /cluster/settings/ovn). Meanwhile use an isolated network, or a NAT network on this node only (scope node)".into()).into());
        }
        let mut net = CreateNetworkRequest {
            subnet: None,
            bridge: Some("br-int".into()),
            vlan: None,
            physnet: None,
            ..req.clone()
        }
        .to_network()?;
        net.vlan = req.vlan;
        net.physnet = req.physnet.clone();
        net.router = req.router.clone();
        net.scope = crate::network::NetworkScope::Cluster;
        net.node = None;
        net.bridge = glidex_ovs::ovn::BR_INT.to_string();
        net.owns_bridge = false;
        let ovn = self.ovn.load_full();
        // Guest MTU: the underlay minus Geneve's overhead (§11.6).
        net.mtu = if provider {
            req.mtu
        } else {
            Some(
                req.mtu
                    .unwrap_or(ovn.underlay_mtu.unwrap_or(1500).saturating_sub(58)),
            )
        };
        net.project = project.map(str::to_string);
        if let Some(why) = self.network_name_taken(&net.name, project)? {
            return Err(NetError::Conflict(why).into());
        }
        if provider {
            if let Some(v) = req.vlan
                && !(1..=4094).contains(&v)
            {
                return Err(NetError::Invalid("vlan must be 1-4094".into()).into());
            }
            self.networks.insert(&net)?;
            tracing::info!(network = %net.name, physnet = ?net.physnet, vlan = ?net.vlan, "provider network created");
            return Ok(net);
        }
        let supernet: glidex_ovs::net::Ipv4Net = ovn
            .nat_supernet
            .as_deref()
            .unwrap_or("10.89.0.0/16")
            .parse()
            .map_err(|e: glidex_ovs::OvsError| NetError::Invalid(e.to_string()))?;
        let node_nat: glidex_ovs::net::Ipv4Net = glidex_ovs::nat::DEFAULT_SUPERNET
            .parse()
            .map_err(|e: glidex_ovs::OvsError| NetError::Invalid(e.to_string()))?;
        let id = net.id.clone();
        let requested = req.subnet;
        self.store
            .database()
            .write(crate::store::Origin::Network, |tx| {
                crate::ipam::allocate_subnet(tx, &id, requested, supernet, &[node_nat])
            })
            .map_err(|e| match e {
                crate::ipam::IpamError::Overlap(..) | crate::ipam::IpamError::Invalid(_) => {
                    VmManagerError::Network(NetError::Invalid(e.to_string()))
                }
                e => VmManagerError::PersistenceError(e.to_string()),
            })?;
        if let Err(e) = self.networks.insert(&net) {
            let _ = self
                .store
                .database()
                .write(crate::store::Origin::Network, |tx| {
                    crate::ipam::free_subnet(tx, &id)
                });
            return Err(e.into());
        }
        tracing::info!(network = %net.name, mode = ?net.mode, "cluster network created");
        Ok(net)
    }

    /// Reservations to make for `vm`'s NICs on cluster networks.
    pub(crate) fn cluster_nics(&self, vm: &Vm) -> Vec<(String, String, String, u8)> {
        let nets = self.networks.list().unwrap_or_default();
        vm.config()
            .networks
            .iter()
            .enumerate()
            .filter(|(_, a)| {
                nets.iter().any(|n| {
                    n.id == a.network
                        && n.scope == crate::network::NetworkScope::Cluster
                        && n.mode != NetworkMode::Bridged
                })
            })
            .filter_map(|(i, a)| {
                Some((
                    a.network.clone(),
                    a.mac
                        .clone()
                        .or_else(|| glidex_ovs::names::mac_address(&vm.id, i as u8).ok())?,
                    vm.id.clone(),
                    i as u8,
                ))
            })
            .collect()
    }

    /// Create a network: host side in netd first, record only on success.
    /// `project`: a project network's (its name is unique in the project's
    /// tenant); `None`: a host network.
    pub async fn create_network(
        &self,
        req: CreateNetworkRequest,
        project: Option<&str>,
    ) -> Result<Network, VmManagerError> {
        if req.scope == Some(crate::network::NetworkScope::Cluster)
            || (req.scope.is_none() && self.ovn_enabled())
        {
            return self.create_cluster_network(req, project).await;
        }
        let mut net = req.to_network()?;
        net.project = project.map(str::to_string);
        if let Some(why) = self.network_name_taken(&net.name, project)? {
            return Err(NetError::Conflict(why).into());
        }
        if let Some(other) = self
            .networks
            .list()?
            .into_iter()
            .find(|n| n.bridge == net.bridge)
        {
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
                        let _ = self.netd.call::<serde_json::Value>(Op::DeleteBridge {
                            name: net.bridge.clone(),
                        });
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
        tracing::info!(network = %net.name, id = %net.id, bridge = %net.bridge, mode = ?net.mode, "network created");
        self.queue.add(Key::Network(net.id.clone()));
        Ok(net)
    }

    /// VMs whose spec or live NICs use network `id`.
    pub(crate) fn network_users(vms: &HashMap<String, Vm>, id: &str) -> Vec<String> {
        vms.values()
            .filter(|vm| {
                vm.config().networks.iter().any(|a| a.network == id)
                    || vm.status.nics.iter().any(|n| n.network == id)
            })
            .map(|vm| vm.name.clone())
            .collect()
    }

    /// Delete a network: refused while a VM uses it; otherwise a deletion
    /// request the network controller finishes (netd's NAT and bridge for a
    /// network glidex created, then the record). Returns the network while
    /// that is still under way (netd unreachable, say), `None` once gone.
    pub async fn delete_network(&self, id: &str) -> Result<Option<Network>, VmManagerError> {
        {
            // Write lock: no VM may attach to it while it starts going away.
            let vms = self.vms.write().await;
            let mut net = self.get_network(id)?;
            let users = Self::network_users(&vms, id);
            if !users.is_empty() {
                return Err(NetError::Conflict(format!(
                    "network '{}' is used by VM(s): {}",
                    net.name,
                    users.join(", ")
                ))
                .into());
            }
            if net.deletion_requested_at.is_none() {
                net.deletion_requested_at = Some(tenancy::now());
                self.networks.put(&net)?;
            }
        }
        self.reconcile_network(id).await?;
        Ok(self.networks.get(id)?)
    }

    /// Create a host network usable by `grants` (or every project).
    pub async fn create_host_network(
        &self,
        req: CreateNetworkRequest,
        grants: Vec<String>,
        tenant_grants: Vec<String>,
        all_projects: bool,
    ) -> Result<Network, VmManagerError> {
        for g in &grants {
            if self.projects.get(g)?.is_none() {
                return Err(TenancyError::NotFound(g.clone()).into());
            }
        }
        if req.router.is_some() {
            return Err(NetError::Invalid(
                "a VPC router belongs to a project: attach project networks to it".into(),
            )
            .into());
        }
        let mut net = self.create_network(req, None).await?;
        net.grants = grants;
        net.tenant_grants = tenant_grants;
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
        let p = self
            .projects
            .get(project)?
            .ok_or_else(|| TenancyError::NotFound(project.to_string()))?;
        if req.mode == NetworkMode::Bridged {
            return Err(NetError::Invalid(
                "project networks are NAT or isolated; bridged networks are host networks".into(),
            )
            .into());
        }
        if req.bridge.is_some() || req.vlan.is_some() {
            return Err(NetError::Invalid(
                "project networks choose their own bridge; bridge and vlan can't be set".into(),
            )
            .into());
        }
        if let Some(name) = req.router.clone() {
            // By name in the project; the record holds the router's id.
            let r = crate::router::find(&self.store.database(), project, &name)
                .map_err(router_err)?
                .ok_or_else(|| {
                    NetError::Invalid(format!("router '{name}' not found in this project"))
                })?;
            req.router = Some(r.id);
        }
        let vms = self.vms.write().await;
        let mut bypassed = Vec::new();
        let usage = self.usage_locked(project, &vms)?;
        self.apply_quotas(
            &p,
            &usage,
            &Delta {
                networks: 1,
                ..Default::default()
            },
            quota,
            &mut bypassed,
            &vms,
        )?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        // Cluster networks have no bridge of their own: they sit on br-int.
        if !(req.scope == Some(crate::network::NetworkScope::Cluster)
            || (req.scope.is_none() && self.ovn_enabled()))
        {
            req.bridge = Some(format!("gxp-{}", &id[..8]));
        }
        let net = self.create_network(req, Some(project)).await?;
        drop(vms);
        Ok((net, bypassed))
    }

    // ---- VPC routers (spec/clustering.md §11.2a) -------------------------

    pub fn list_routers(
        &self,
        project: &str,
    ) -> Result<Vec<crate::router::Router>, VmManagerError> {
        Ok(crate::router::list(&self.store.database())
            .map_err(router_err)?
            .into_iter()
            .filter(|r| r.project == project)
            .collect())
    }

    pub fn get_router(
        &self,
        project: &str,
        name: &str,
    ) -> Result<crate::router::Router, VmManagerError> {
        crate::router::find(&self.store.database(), project, name)
            .map_err(router_err)?
            .ok_or_else(|| NetError::NotFound(format!("router {name}")).into())
    }

    /// Create a router, counting it (and its external address) against the
    /// project's quotas in the same admission write as the address.
    pub async fn create_router(
        &self,
        project: &str,
        spec: crate::router::RouterSpec,
        quota: QuotaMode,
    ) -> Result<(crate::router::Router, Vec<QuotaOverrun>), VmManagerError> {
        if !self.ovn_enabled() {
            return Err(NetError::Invalid(
                "VPC routers need OVN: enable cluster.ovn on a cluster".into(),
            )
            .into());
        }
        let p = self
            .projects
            .get(project)?
            .ok_or_else(|| TenancyError::NotFound(project.to_string()))?;
        let ovn = self.ovn.load_full();
        let edge = ovn.edge.as_ref();
        let pool: Option<glidex_ovs::net::Ipv4Net> = edge
            .and_then(|e| e.external_pool.as_deref())
            .and_then(|p| p.parse().ok());
        let mut exclude = Vec::new();
        if let Some(e) = edge {
            exclude.extend(
                [e.external_ip, e.gateway]
                    .into_iter()
                    .filter_map(|a| match a {
                        std::net::IpAddr::V4(a) => Some(a),
                        _ => None,
                    }),
            );
        }
        let vms = self.vms.write().await;
        let mut bypassed = Vec::new();
        let usage = self.usage_locked(project, &vms)?;
        self.apply_quotas(
            &p,
            &usage,
            &Delta {
                routers: 1,
                external_ips: spec.external as u64,
                ..Default::default()
            },
            quota,
            &mut bypassed,
            &vms,
        )?;
        let site = crate::router::Site {
            pool,
            exclude: &exclude,
            zones: ovn.snat_ct_zones.unwrap_or((60000, 64999)),
        };
        let r = crate::router::create(&self.store.database(), project, spec, &site)
            .map_err(router_err)?;
        drop(vms);
        tracing::info!(router = %r.id, project, external_ip = ?r.status.external_ip, "VPC router created");
        Ok((r, bypassed))
    }

    /// Delete a router: refused while networks are attached. Its OVN
    /// objects go with the next network-controller pass.
    pub async fn delete_router(&self, project: &str, name: &str) -> Result<(), VmManagerError> {
        let r = self.get_router(project, name)?;
        // The VM lock keeps a network from attaching while it goes.
        let _vms = self.vms.write().await;
        if let Some(n) = self
            .networks
            .list()?
            .iter()
            .find(|n| n.router.as_deref() == Some(r.id.as_str()))
        {
            return Err(NetError::Conflict(format!(
                "router '{name}' still has network '{}' attached",
                n.name
            ))
            .into());
        }
        crate::router::delete(&self.store.database(), &r.id).map_err(router_err)?;
        Ok(())
    }

    /// Put an existing NAT or isolated network of `project` on one of its
    /// routers, or (`None`) back on the shared edge (an isolated network:
    /// back to no router). The network keeps its gateway
    /// address and MAC, so its VMs keep working; the network controller
    /// moves its router port and SNAT on its next pass.
    pub async fn set_network_router(
        &self,
        project: &str,
        network: &str,
        router: Option<&str>,
    ) -> Result<Network, VmManagerError> {
        // The VM lock keeps a router from going while a network attaches.
        let _vms = self.vms.write().await;
        let mut net = self
            .networks
            .get(network)?
            .filter(|n| n.project.as_deref() == Some(project))
            .ok_or_else(|| NetError::NotFound(network.to_string()))?;
        // An isolated network on a router is routed like a NAT one there,
        // and isolated again once it leaves.
        let network = net.name.clone();
        if net.scope != network::NetworkScope::Cluster || net.mode == NetworkMode::Bridged {
            return Err(NetError::Invalid(format!("only a NAT or isolated network that spans the nodes goes on a router; '{network}' is {:?} on {:?} scope", net.mode, net.scope).to_lowercase()).into());
        }
        if net.deletion_requested_at.is_some() {
            return Err(NetError::Conflict(format!("network '{network}' is being deleted")).into());
        }
        let id = match router {
            Some(name) => Some(
                crate::router::find(&self.store.database(), project, name)
                    .map_err(router_err)?
                    .ok_or_else(|| {
                        NetError::Invalid(format!("router '{name}' not found in this project"))
                    })?
                    .id,
            ),
            None => None,
        };
        if net.router != id {
            tracing::info!(network = %net.id, from = ?net.router, to = ?id, "network moves router");
            net.router = id;
            self.networks.put(&net)?;
        }
        Ok(net)
    }

    /// Set which projects may use a host network.
    pub fn grant_network(
        &self,
        id: &str,
        grants: Vec<String>,
        tenant_grants: Vec<String>,
        all_projects: bool,
    ) -> Result<Network, VmManagerError> {
        let mut net = self.get_network(id)?;
        if net.project.is_some() {
            return Err(
                NetError::Invalid("project networks are shared, not granted".into()).into(),
            );
        }
        for g in &grants {
            if self.projects.get(g)?.is_none() {
                return Err(TenancyError::NotFound(g.clone()).into());
            }
        }
        net.grants = grants;
        net.tenant_grants = tenant_grants;
        net.all_projects = all_projects;
        self.networks.put(&net)?;
        Ok(net)
    }

    /// Offer a project network to `target`. Silently succeeds when the
    /// target doesn't exist, so offers can't probe for projects.
    pub fn offer_network_share(
        &self,
        id: &str,
        target: &str,
        by: &str,
    ) -> Result<(), VmManagerError> {
        let mut net = self.get_network(id)?;
        if net.project.is_none() {
            return Err(NetError::Invalid("only project networks can be shared".into()).into());
        }
        if self.projects.get(target)?.is_none() || net.shares.iter().any(|s| s == target) {
            return Ok(());
        }
        let now = tenancy::now();
        net.share_offers
            .retain(|o| o.project != target && o.expires_at > now);
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
            .filter(|n| {
                n.shares.iter().any(|s| s == project)
                    || n.share_offers
                        .iter()
                        .any(|o| o.project == project && o.expires_at > now)
            })
            .collect())
    }

    /// Accept an open offer of network `id` to `project`.
    pub fn accept_network_share(&self, project: &str, id: &str) -> Result<Network, VmManagerError> {
        let mut net = self.get_network(id)?;
        let now = tenancy::now();
        let Some(pos) = net
            .share_offers
            .iter()
            .position(|o| o.project == project && o.expires_at > now)
        else {
            return Err(NetError::NotFound(format!(
                "no open share offer of network '{}' to this project",
                net.name
            ))
            .into());
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
    pub async fn end_network_share(&self, id: &str, project: &str) -> Result<(), VmManagerError> {
        let vms = self.vms.read().await;
        let mut net = self.get_network(id)?;
        let name = net.name.clone();
        let users: Vec<String> = vms
            .values()
            .filter(|vm| {
                vm.project == project && vm.config().networks.iter().any(|a| a.network == id)
            })
            .map(|vm| vm.name.clone())
            .collect();
        if !users.is_empty() {
            return Err(NetError::Conflict(format!(
                "network '{}' is used by VM(s) of that project: {}",
                name,
                users.join(", ")
            ))
            .into());
        }
        let before = (net.shares.len(), net.share_offers.len());
        net.shares.retain(|s| s != project);
        net.share_offers.retain(|o| o.project != project);
        if before == (net.shares.len(), net.share_offers.len()) {
            return Err(NetError::NotFound(format!(
                "network '{}' isn't shared with that project",
                name
            ))
            .into());
        }
        self.networks.put(&net)?;
        Ok(())
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
    pub fn list_credentials_in(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<Credential>, VmManagerError> {
        Ok(self.credentials.list(project)?)
    }

    pub fn list_credentials(&self) -> Result<Vec<Credential>, VmManagerError> {
        self.list_credentials_in(Some(&self.default_project_id()))
    }

    pub fn get_credential_in(
        &self,
        project: &str,
        username: &str,
    ) -> Result<Credential, VmManagerError> {
        Ok(self.credentials.get(project, username)?)
    }

    pub fn get_credential(&self, username: &str) -> Result<Credential, VmManagerError> {
        self.get_credential_in(&self.default_project_id(), username)
    }

    pub fn create_credential_in(
        &self,
        project: &str,
        req: CreateCredentialRequest,
    ) -> Result<Credential, VmManagerError> {
        if self.projects.get(project)?.is_none() {
            return Err(TenancyError::NotFound(project.to_string()).into());
        }
        let cred = self.credentials.create(project, req)?;
        tracing::info!(username = %cred.username, project, "Credential created");
        Ok(cred)
    }

    pub fn create_credential(
        &self,
        req: CreateCredentialRequest,
    ) -> Result<Credential, VmManagerError> {
        self.create_credential_in(&self.default_project_id(), req)
    }

    /// Changes reach a VM only on its first boot: cloud-init provisions
    /// users once per instance-id, and a VM keeps its id for life.
    pub fn update_credential_in(
        &self,
        project: &str,
        username: &str,
        req: UpdateCredentialRequest,
    ) -> Result<Credential, VmManagerError> {
        let cred = self.credentials.update(project, username, req)?;
        tracing::info!(username = %cred.username, project, "Credential updated");
        Ok(cred)
    }

    pub fn update_credential(
        &self,
        username: &str,
        req: UpdateCredentialRequest,
    ) -> Result<Credential, VmManagerError> {
        self.update_credential_in(&self.default_project_id(), username, req)
    }

    /// Refuses to delete a credential still referenced by a VM.
    pub async fn delete_credential_in(
        &self,
        project: &str,
        username: &str,
    ) -> Result<(), VmManagerError> {
        let vms = self.vms.read().await;
        let users: Vec<String> = vms
            .values()
            .filter(|vm| {
                vm.project == project && vm.config().credential.as_deref() == Some(username)
            })
            .map(|vm| vm.name.clone())
            .collect();
        if !users.is_empty() {
            return Err(VmManagerError::CredentialInUse {
                username: username.to_string(),
                vms: users,
            });
        }
        self.credentials.delete(project, username)?;
        tracing::info!(username = %username, project, "Credential deleted");
        Ok(())
    }

    pub async fn delete_credential(&self, username: &str) -> Result<(), VmManagerError> {
        self.delete_credential_in(&self.default_project_id(), username)
            .await
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
        tracing::info!(
            credentials = moved,
            networks = nets,
            "assigned existing resources to the default project"
        );
        Ok(())
    }

    /// A project's current use. `running_vms` counts VMs whose desired
    /// state is not `stopped` (§12.2), so crash loops and host reboots can
    /// never exceed it.
    fn usage_locked(
        &self,
        project: &str,
        vms: &HashMap<String, Vm>,
    ) -> Result<Usage, VmManagerError> {
        let mut u = Usage::default();
        for vm in vms.values().filter(|vm| vm.project == project) {
            u.vms += 1;
            u.vcpus += vm.config().vcpu_count as u64;
            u.memory_mib += vm.config().mem_size_mib as u64;
            if vm.spec.power != PowerState::Stopped {
                u.running_vms += 1;
            }
            for d in &vm.config().devices {
                *u.devices.entry(d.pool.clone()).or_default() += d.count as u64;
            }
        }
        u.disk_gib = self
            .images
            .list_disks()
            .iter()
            .filter(|d| d.project == project)
            .map(|d| gib_ceil(d.size_bytes))
            .sum();
        u.networks = self
            .networks
            .list()?
            .iter()
            .filter(|n| n.project.as_deref() == Some(project))
            .count() as u64;
        for r in crate::router::list(&self.store.database())
            .map_err(router_err)?
            .iter()
            .filter(|r| r.project == project)
        {
            u.routers += 1;
            u.external_ips += r.status.external_ip.is_some() as u64;
        }
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

    /// The project's quotas, then its tenant's cap on this cluster: the
    /// tenant's use is the sum over its projects here (spec/tenancy.md D19).
    #[allow(clippy::too_many_arguments)]
    fn apply_quotas(
        &self,
        p: &tenancy::Project,
        usage: &Usage,
        delta: &Delta,
        mode: QuotaMode,
        bypassed: &mut Vec<QuotaOverrun>,
        vms: &HashMap<String, Vm>,
    ) -> Result<(), VmManagerError> {
        Self::apply_quota(&p.quotas, usage, delta, mode, bypassed)?;
        let Some(admission) = self
            .tenants
            .admission(&p.tenant_id)
            .map_err(|e| VmManagerError::Tenancy(TenancyError::Storage(e.to_string())))?
        else {
            return Ok(());
        };
        let mut total = Usage::default();
        for other in self
            .projects
            .list()?
            .iter()
            .filter(|o| o.tenant_id == p.tenant_id)
        {
            total.add(&self.usage_locked(&other.id, vms)?);
        }
        let over = tenancy::overruns(&admission.cap, &total, delta);
        if over.is_empty() {
            return Ok(());
        }
        match mode {
            QuotaMode::Enforce => Err(VmManagerError::TenantQuotaExceeded(over)),
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
        if let Some(d) = self
            .images
            .list_disks()
            .iter()
            .find(|d| d.project == project)
        {
            return Ok(Some(format!("disk {}", d.name)));
        }
        if let Some(c) = self.credentials.list(Some(project))?.first() {
            return Ok(Some(format!("credential {}", c.username)));
        }
        if let Some(n) = self
            .networks
            .list()?
            .iter()
            .find(|n| n.project.as_deref() == Some(project))
        {
            return Ok(Some(format!("network {}", n.name)));
        }
        if let Some(r) = crate::router::list(&self.store.database())
            .map_err(router_err)?
            .iter()
            .find(|r| r.project == project)
        {
            return Ok(Some(format!("router {}", r.spec.name)));
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
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ImageError> + Send + 'static,
) -> Result<T, ImageError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ImageError::Io(format!("task failed: {}", e)))?
}

/// Size in GiB, rounded up.
/// Devices asked of each pool.
fn pool_counts(devices: &[crate::models::DeviceRequest]) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for d in devices {
        *out.entry(d.pool.clone()).or_default() += d.count as u64;
    }
    out
}

fn gib_ceil(bytes: u64) -> u64 {
    bytes.div_ceil(1 << 30)
}

/// The node-side answers to relayed requests, backed by the manager.
struct HandlersOf(Weak<VmManager>);

impl crate::cluster::runtime::NodeHandlers for HandlersOf {
    fn vm_stats(&self, vm_id: &str) -> Option<serde_json::Value> {
        let m = self.0.upgrade()?;
        let meter = m.meter()?;
        Some(crate::api::live_vm_stats(&meter, vm_id))
    }

    fn image_file(&self, id: &str, part: &str) -> Option<std::path::PathBuf> {
        let m = self.0.upgrade()?;
        let img = m.images.get_image(id).ok()?;
        let p = match (part, img.kind) {
            ("main", _) => m.images.image_file(&img),
            ("vars", crate::images::ImageKind::Firmware) => {
                m.images.firmware_vars_template(&img.id)
            }
            _ => return None,
        };
        p.exists().then_some(p)
    }

    fn ovn_status<'a>(
        &'a self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            let m = self.0.upgrade().ok_or("shutting down")?;
            m.local_ovn_status().await
        })
    }

    fn topology(&self) -> Option<serde_json::Value> {
        let m = self.0.upgrade()?;
        serde_json::to_value(m.local_topology()).ok()
    }

    fn device_op(&self, address: &str, op: &str) -> Result<serde_json::Value, (String, String)> {
        let m = self
            .0
            .upgrade()
            .ok_or(("unavailable".to_string(), "shutting down".to_string()))?;
        m.local_device_op(address, op).map_err(|e| match e {
            VmManagerError::Network(NetError::Netd(b)) => (b.code, b.message),
            e => ("internal".to_string(), e.to_string()),
        })
    }

    fn left_ovn<'a>(
        &'a self,
        node: &'a crate::node::Node,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            if let Some(m) = self.0.upgrade()
                && let Err(e) = m.leave_ovn_membership(node, true).await
            {
                tracing::warn!(node = %node.spec.name, "taking a departed node out of OVN: {}", e);
            }
        })
    }

    fn departed(&self) {
        if crate::cluster::departure::EXIT_ON_DEPARTURE.load(std::sync::atomic::Ordering::Relaxed) {
            // The service manager starts it again, and startup finishes the switch.
            tracing::warn!("restarting to continue as a standalone host");
            // A non-zero status: the unit restarts on failure.
            std::process::exit(75);
        }
    }
}

/// `prepare` or `release` PCI device `address` through `netd`
/// (spec/host-topology.md §5.3).
fn device_op_on(netd: &Netd, address: &str, op: &str) -> Result<serde_json::Value, VmManagerError> {
    let address = address.to_string();
    let r = match op {
        "prepare" => netd.call(Op::PrepareDevice { address }),
        "release" => netd.call(Op::ReleaseDevice { address }),
        _ => {
            return Err(VmManagerError::Network(NetError::Invalid(format!(
                "no device op {op}"
            ))));
        }
    };
    r.map_err(VmManagerError::Network)
}
