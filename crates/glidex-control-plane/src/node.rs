//! Nodes (spec/clustering.md §7.1): the hosts a cluster schedules onto. A
//! standalone host is a cluster of one, with one implicit node `local`.

use crate::store::{Db, Meta, Origin, StoreError, TableId};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// The id of the implicit node of a standalone host.
pub const LOCAL_NODE: &str = "local";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    /// Raft voter or learner, API, scheduler and cluster controllers, plus
    /// the node role.
    #[default]
    Server,
    /// The node role only.
    Agent,
}

/// Which halves of the control plane this process runs (D4). A standalone
/// host is both, with a local store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Roles {
    /// VM, disk and image-cache controllers, metering sampler, netd sync.
    pub node: bool,
    /// API, scheduler, cluster controllers, ledger.
    pub server: bool,
}

impl Roles {
    pub const STANDALONE: Roles = Roles {
        node: true,
        server: true,
    };

    pub fn of(role: NodeRole) -> Roles {
        match role {
            NodeRole::Server => Roles::STANDALONE,
            NodeRole::Agent => Roles {
                node: true,
                server: false,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpec {
    /// Hostname by default; unique.
    pub name: String,
    pub role: NodeRole,
    /// Drain: no new placements.
    #[serde(default)]
    pub unschedulable: bool,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// The datapath of this node's `br-int` (spec/clustering.md §11.6):
    /// `auto` (the default) is `netdev` where DPDK runs, else `system`.
    #[serde(default, skip_serializing_if = "BrIntDatapath::is_auto")]
    pub br_int_datapath: BrIntDatapath,
    /// Physical networks this node reaches: physnet → glidex bridge with the
    /// uplink (§11.3). Set through the API; the host's file
    /// (`cluster.ovn.bridge_mappings`) still counts, and this wins per physnet.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bridge_mappings: BTreeMap<String, String>,
    /// Delete the unrecorded image files the node reported that are older
    /// than this (seconds; spec/clustering.md §9.2.3). The node applies
    /// each value once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drop_spare_files_before: Option<u64>,
    /// PCI devices (addresses) no new VM gets (spec/host-topology.md
    /// §5.4); the VM holding one keeps it.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub cordoned_devices: std::collections::BTreeSet<String>,
}

/// What an administrator chose for a node's `br-int` datapath.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrIntDatapath {
    #[default]
    Auto,
    /// The kernel datapath: what a gateway node needs (its SNAT is metered
    /// from kernel conntrack, §13.2). Tap VM ports only.
    System,
    /// The userspace (DPDK) datapath: vhost-user VM ports on cluster networks.
    Netdev,
}

impl BrIntDatapath {
    pub fn is_auto(&self) -> bool {
        *self == BrIntDatapath::Auto
    }
}

impl Node {
    /// The datapath this node's `br-int` should have: its setting, or for
    /// `auto`, `netdev` where DPDK runs.
    pub fn wanted_br_int(&self) -> &'static str {
        match self.spec.br_int_datapath {
            BrIntDatapath::System => "system",
            BrIntDatapath::Netdev => "netdev",
            BrIntDatapath::Auto if self.status.features.dpdk => "netdev",
            BrIntDatapath::Auto => "system",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NodePhase {
    #[default]
    Active,
    Draining,
    Departing,
    Departed,
    Removed,
    Forgotten,
}

impl NodePhase {
    /// A tombstone: the node is gone and its id retired (§5.3).
    pub fn is_tombstone(self) -> bool {
        matches!(
            self,
            NodePhase::Departed | NodePhase::Removed | NodePhase::Forgotten
        )
    }

    pub fn schedulable(self) -> bool {
        self == NodePhase::Active
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    pub cpus: u32,
    /// In `allocatable`, without the hugepage pool: a hugepage is counted
    /// once, as a hugepage (spec/host-topology.md §8a.2).
    pub memory_mib: u64,
    /// Hugepages by page size in KiB: the pool in `capacity`; in
    /// `allocatable`, less what OVS-DPDK takes.
    #[serde(default)]
    pub hugepages: BTreeMap<String, u64>,
}

impl Resources {
    /// Pages of `page_kib` hugepages.
    pub fn pages(&self, page_kib: u64) -> u64 {
        self.hugepages
            .get(&page_kib.to_string())
            .copied()
            .unwrap_or(0)
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// MiB held in hugepages of every size (`page KiB → pages`).
pub fn hugepage_pool_mib(hugepages: &BTreeMap<String, u64>) -> u64 {
    hugepages
        .iter()
        .filter_map(|(kib, n)| kib.parse::<u64>().ok().map(|k| k * n / 1024))
        .sum()
}

/// Pages of `page_kib` OVS-DPDK takes for `dpdk-socket-mem` (MiB per NUMA
/// node, `1024` or `1024,1024`), rounded up per node.
pub fn dpdk_pages(socket_mem: &str, page_kib: u64) -> u64 {
    let page_mib = (page_kib / 1024).max(1);
    socket_mem
        .split(',')
        .filter_map(|m| m.trim().parse::<u64>().ok())
        .map(|mib| mib.div_ceil(page_mib))
        .sum()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFeatures {
    pub kvm: bool,
    pub hypervisors: Vec<String>,
    /// `system` or `netdev`: the datapath of `br-int` (§11.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub br_int_datapath: Option<String>,
    #[serde(default)]
    pub dpdk: bool,
    #[serde(default)]
    pub iommu: bool,
    /// The host's default hugepage size in KiB (`Hugepagesize`), which
    /// OVS-DPDK and a VM asking for no size use; 0 from an older node.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub hugepage_kib: u64,
    /// Provider networks this node maps: physnet → bridge (§11.3).
    #[serde(default)]
    pub physnets: BTreeMap<String, String>,
    /// The highest write-set format and schema this build uses (§6.6).
    #[serde(default)]
    pub feature_level: u32,
    /// OVN's central part (`ovn-northd`) is installed: this server can hold
    /// OVN's databases (spec/clustering-ui.md §8a).
    #[serde(default)]
    pub ovn_central: bool,
    /// The host address an uplink moved onto each bridge, and the default
    /// gateway through it: what the shared edge's defaults are made from
    /// (spec/clustering-ui.md §8a).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bridge_addrs: BTreeMap<String, BridgeAddr>,
    /// `x86_64` or `aarch64` (spec/clustering.md §9.2.1); empty from a
    /// node of an older version, which counts as matching anything.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arch: String,
    /// Files in the node's image directory that no image record names
    /// (spec/clustering.md §9.2.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spare_image_files: Vec<crate::images::spare::SpareFile>,
    /// Whether glidex-pylond daemon is running on this node (spec/pylon.md §8.1).
    #[serde(default)]
    pub pylond: bool,
}

impl NodeFeatures {
    /// Whether a VM of `arch` may run here (an unknown arch matches).
    pub fn runs_arch(&self, arch: &str) -> bool {
        self.arch.is_empty() || arch.is_empty() || self.arch == arch
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeAddr {
    /// `addr/prefix` (IPv4).
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
}

/// What this build supports. A new write-set format, schema migration or
/// table raises it, and is used only once the cluster's level (the minimum
/// over its servers) has reached it. 2: datastores (spec/storage.md S2).
pub const FEATURE_LEVEL: u32 = 2;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versions {
    pub glidex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ovs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ovn: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeStatus {
    pub phase: NodePhase,
    /// `Departed` only: the VM, disk and image ids it took (§5.9 id check).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub departed_ids: Vec<String>,
    /// `Ready`, `Unknown` when the node stops heartbeating (§7.2).
    pub ready: crate::models::Tristate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_reason: Option<String>,
    #[serde(default)]
    pub observed_generation: u64,
    /// The member's Raft id (servers), see `cluster::raft::raft_id_of`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raft_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise: Option<SocketAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_ip: Option<IpAddr>,
    /// A server's web UI address (spec/host-page.md W3); agents publish none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_url: Option<String>,
    #[serde(default)]
    pub versions: Versions,
    #[serde(default)]
    pub capacity: Resources,
    #[serde(default)]
    pub allocatable: Resources,
    #[serde(default)]
    pub features: NodeFeatures,
    /// BDFs of the host's PCI devices (what `GET /pci-devices` reports): the
    /// scheduler places a VM with VFIO devices where they exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pci_devices: Vec<String>,
    /// The IOMMU group of each PCI function that is not a bridge: a VFIO
    /// device claims its group (spec/host-topology.md §8.4). Empty from a
    /// node of an older version, or a host without an IOMMU.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pci_groups: BTreeMap<String, String>,
    /// NUMA nodes, cores, memory and hugepages per node, reserved CPUs and
    /// devices' NUMA nodes (spec/host-topology.md §3.1); `None` from a node
    /// of an older version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology: Option<crate::topology::TopologySummary>,
    /// Last `Ready` transition write only (D7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<u64>,
}

/// A node as stored: `{meta, spec, status}` like every resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub meta: Meta,
    pub spec: NodeSpec,
    pub status: NodeStatus,
}

impl Node {
    pub fn new(id: &str, spec: NodeSpec) -> Node {
        Node {
            meta: Meta {
                id: id.to_string(),
                name: spec.name.clone(),
                project: String::new(),
                created_at: crate::tenancy::now(),
                generation: 1,
                resource_version: 1,
                deletion_requested_at: None,
                finalizers: Vec::new(),
            },
            spec,
            status: NodeStatus {
                phase: NodePhase::Active,
                departed_ids: Vec::new(),
                ready: crate::models::Tristate::True,
                ready_reason: None,
                observed_generation: 1,
                raft_id: None,
                advertise: None,
                tunnel_ip: None,
                ui_url: None,
                versions: Versions {
                    glidex: env!("CARGO_PKG_VERSION").into(),
                    ..Default::default()
                },
                capacity: Resources::default(),
                allocatable: Resources::default(),
                features: NodeFeatures::default(),
                pci_devices: Vec::new(),
                pci_groups: BTreeMap::new(),
                topology: None,
                heartbeat: None,
            },
        }
    }

    pub fn id(&self) -> &str {
        &self.meta.id
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("node {0} not found")]
    NotFound(String),
    #[error("node name {0} is already taken")]
    NameTaken(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for NodeError {
            fn from(e: $t) -> Self { NodeError::Storage(e.to_string()) }
        }
    )*};
}
storage_from!(
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    serde_json::Error
);

/// An OVN release series, `(major, minor)` of "26.03.0" or "24.03.8".
pub fn ovn_series(v: &str) -> Option<(u32, u32)> {
    let mut parts = v.trim().split(|c: char| !c.is_ascii_digit());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The OVN of the cluster's ovn-northd: the newest that the servers running
/// OVN's central part report (one of them runs northd; a newer one may take
/// over). `None` without such a server.
pub fn cluster_northd_ovn(nodes: &[Node]) -> Option<String> {
    nodes
        .iter()
        .filter(|n| {
            n.spec.role == NodeRole::Server
                && n.status.phase == NodePhase::Active
                && n.status.features.ovn_central
        })
        .filter_map(|n| n.status.versions.ovn.clone())
        .filter(|v| ovn_series(v).is_some())
        .max_by_key(|v| ovn_series(v))
}

/// Why `n` can't run the logical flows of an ovn-northd of version `northd`:
/// its ovn-controller is an older release, whose tables stop short of
/// northd's pipeline (spec/installer.md, OVN). `None` when it can.
pub fn ovn_outdated(n: &Node, northd: Option<&str>) -> Option<String> {
    let (want, have) = (
        northd.and_then(ovn_series)?,
        n.status.versions.ovn.as_deref()?,
    );
    (ovn_series(have)? < want).then(|| {
        format!("its ovn-controller {have} is older than the cluster's ovn-northd {}: re-run glidex-install on {} to upgrade OVN", northd.unwrap_or_default(), n.spec.name)
    })
}

/// The `nodes` table.
pub struct NodeStore {
    db: Arc<Db>,
}

impl NodeStore {
    pub fn new(db: Arc<Db>) -> Self {
        NodeStore { db }
    }

    pub fn get(&self, id: &str) -> Result<Option<Node>, NodeError> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(TableId::Nodes.definition())?;
        Ok(match t.get(id)? {
            Some(v) => Some(serde_json::from_slice(v.value())?),
            None => None,
        })
    }

    pub fn list(&self) -> Result<Vec<Node>, NodeError> {
        let txn = self.db.begin_read()?;
        let t = txn.open_table(TableId::Nodes.definition())?;
        let mut out = Vec::new();
        for r in t.iter()? {
            let (_, v) = r?;
            match serde_json::from_slice::<Node>(v.value()) {
                Ok(n) => out.push(n),
                Err(e) => tracing::warn!("skipping unreadable node record: {}", e),
            }
        }
        out.sort_by(|a, b| {
            a.spec
                .name
                .cmp(&b.spec.name)
                .then(a.meta.id.cmp(&b.meta.id))
        });
        Ok(out)
    }

    /// Insert or replace. Names are unique among live nodes.
    pub fn put(&self, n: &Node) -> Result<(), NodeError> {
        if !n.status.phase.is_tombstone()
            && self.list()?.iter().any(|o| {
                o.meta.id != n.meta.id
                    && !o.status.phase.is_tombstone()
                    && o.spec.name == n.spec.name
            })
        {
            return Err(NodeError::NameTaken(n.spec.name.clone()));
        }
        let bytes = serde_json::to_vec(n)?;
        // Not a node-status origin (`Origin::is_node_status`): on a follower
        // server this must fail with `NotLeader` and go the raw way below,
        // which lets a server write all of its own record.
        let r = self
            .db
            .write(Origin::System, |tx| -> Result<(), NodeError> {
                let mut t = tx.open_table(TableId::Nodes.definition())?;
                t.insert(n.meta.id.as_str(), bytes.as_slice())?;
                Ok(())
            });
        match r {
            // A follower server reporting on itself: the leader writes it.
            Err(NodeError::Store(StoreError::NotLeader { .. })) => {
                Ok(self.db.forward_raw(vec![crate::store::Op::Put {
                    table: TableId::Nodes,
                    key: n.meta.id.as_bytes().to_vec(),
                    value: bytes,
                }])?)
            }
            other => other,
        }
    }

    /// Make sure the implicit node of this host exists, and refresh what a
    /// node reports about itself (capacity, features, versions). Writes
    /// nothing when nothing changed (§4.1).
    pub fn ensure_self(&self, id: &str, probe: SelfProbe) -> Result<Node, NodeError> {
        self.report_self(id, probe, true)
            .map(|n| n.expect("created"))
    }

    /// Update this node's own record with what the host has now. `create`:
    /// make the record when there is none (a standalone host). A cluster
    /// member never does: its record is the cluster's (join wrote it), and a
    /// cache that hasn't received it yet must not replace it with a guess.
    pub fn report_self(
        &self,
        id: &str,
        probe: SelfProbe,
        create: bool,
    ) -> Result<Option<Node>, NodeError> {
        let mut node = match self.get(id)? {
            Some(n) => n,
            None if create => Node::new(
                id,
                NodeSpec {
                    name: probe.name.clone(),
                    role: NodeRole::Server,
                    unschedulable: false,
                    labels: BTreeMap::new(),
                    br_int_datapath: Default::default(),
                    bridge_mappings: Default::default(),
                    drop_spare_files_before: None,
                    cordoned_devices: Default::default(),
                },
            ),
            None => return Ok(None),
        };
        let before = node.clone();
        node.status.capacity = probe.capacity;
        node.status.allocatable = probe.allocatable;
        // The OVN loop records br-int's datapath; the probe can't see it.
        let br_int = node.status.features.br_int_datapath.take();
        node.status.features = probe.features;
        node.status.features.br_int_datapath = br_int;
        // The file's mappings, and the node's own setting over them.
        for (p, b) in &node.spec.bridge_mappings {
            node.status.features.physnets.insert(p.clone(), b.clone());
        }
        node.status.pci_devices = probe.pci_devices;
        node.status.pci_groups = probe.pci_groups;
        node.status.topology = probe.topology;
        node.status.versions.glidex = env!("CARGO_PKG_VERSION").into();
        node.status.versions.ovn = probe.ovn_version;
        node.status.ui_url = probe.ui_url;
        if node != before || self.get(id)?.is_none() {
            node.meta.resource_version += 1;
            self.put(&node)?;
        }
        Ok(Some(node))
    }
}

/// What a host reports about itself.
#[derive(Debug, Clone, Default)]
pub struct SelfProbe {
    pub name: String,
    pub capacity: Resources,
    pub allocatable: Resources,
    pub features: NodeFeatures,
    pub pci_devices: Vec<String>,
    pub pci_groups: BTreeMap<String, String>,
    pub topology: Option<crate::topology::TopologySummary>,
    /// `ovn-controller`'s version, when OVN is installed.
    pub ovn_version: Option<String>,
    /// This server's web UI address (spec/host-page.md W3).
    pub ui_url: Option<String>,
}

/// The first line of `<program> --version` (`ovn-controller 24.03.2`), its
/// second word; `None` when the program isn't installed.
fn program_version(program: &str) -> Option<String> {
    let out = std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)
        .map(String::from)
}

impl SelfProbe {
    /// Read the host: hostname, CPUs, memory, hugepages, KVM and the
    /// installed hypervisors. `reserved` is kept back for the host itself
    /// (`cluster.node_reserved`).
    pub fn detect(reserved: &Resources) -> SelfProbe {
        let name = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "localhost".into());
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);
        let memory_mib = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| {
                m.lines()
                    .find_map(|l| l.strip_prefix("MemTotal:"))
                    .and_then(|r| {
                        r.split_whitespace()
                            .next()
                            .and_then(|n| n.parse::<u64>().ok())
                    })
            })
            .map(|kib| kib / 1024)
            .unwrap_or(0);
        let mut hugepages = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir("/sys/kernel/mm/hugepages") {
            for e in rd.flatten() {
                let dir = e.file_name().to_string_lossy().into_owned();
                if let Some(kb) = dir
                    .strip_prefix("hugepages-")
                    .and_then(|s| s.strip_suffix("kB"))
                {
                    let n = std::fs::read_to_string(e.path().join("nr_hugepages"))
                        .ok()
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .unwrap_or(0);
                    if n > 0 {
                        hugepages.insert(kb.to_string(), n);
                    }
                }
            }
        }
        let pool_mib = hugepage_pool_mib(&hugepages);
        let capacity = Resources {
            cpus,
            memory_mib,
            hugepages: hugepages.clone(),
        };
        let allocatable = Resources {
            cpus: cpus.saturating_sub(reserved.cpus).max(1),
            memory_mib: memory_mib
                .saturating_sub(pool_mib)
                .saturating_sub(reserved.memory_mib),
            hugepages,
        };
        let hugepage_kib = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| {
                m.lines()
                    .find_map(|l| l.strip_prefix("Hugepagesize:"))
                    .and_then(|r| r.split_whitespace().next()?.parse::<u64>().ok())
            })
            .unwrap_or(0);
        let hypervisors = [
            crate::hypervisor::HypervisorType::CloudHypervisor,
            crate::hypervisor::HypervisorType::Qemu,
            crate::hypervisor::HypervisorType::Pylon,
        ]
        .into_iter()
        .filter(|t| crate::hypervisor::driver(*t).is_available())
        .map(|t| t.to_string())
        .collect();
        let pylond = std::path::Path::new(crate::hypervisor::pylon::DEFAULT_PYLOND_SOCKET).exists();
        let features = NodeFeatures {
            kvm: std::path::Path::new("/dev/kvm").exists(),
            hypervisors,
            pylond,
            iommu: std::fs::read_dir("/sys/kernel/iommu_groups")
                .map(|mut d| d.next().is_some())
                .unwrap_or(false),
            hugepage_kib,
            ovn_central: program_version("ovn-northd").is_some(),
            arch: crate::images::catalog::Arch::host()
                .map(|a| a.as_str().to_string())
                .unwrap_or_default(),
            ..Default::default()
        };
        let pci = crate::pci::scan_pci_devices();
        let pci_groups = crate::pci::iommu_groups(&pci);
        let pci_devices = pci.into_iter().map(|d| d.address).collect();
        SelfProbe {
            name,
            capacity,
            allocatable,
            features,
            pci_devices,
            pci_groups,
            topology: crate::topology::read(),
            ovn_version: program_version("ovn-controller"),
            ui_url: None,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dpdk_takes_whole_pages_per_numa_node_and_the_pool_is_counted_once() {
        assert_eq!(super::dpdk_pages("1024", 2048), 512);
        assert_eq!(super::dpdk_pages("1024,1024", 2048), 1024);
        assert_eq!(super::dpdk_pages("1025", 2048), 513);
        assert_eq!(super::dpdk_pages("1024", 1_048_576), 1);
        assert_eq!(super::dpdk_pages("", 2048), 0);
        let pool: std::collections::BTreeMap<String, u64> =
            [("2048".to_string(), 1024), ("1048576".to_string(), 2)].into();
        assert_eq!(super::hugepage_pool_mib(&pool), 2048 + 2048);
    }
}
