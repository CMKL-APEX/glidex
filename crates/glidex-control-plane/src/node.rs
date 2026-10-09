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
    pub const STANDALONE: Roles = Roles { node: true, server: true };

    pub fn of(role: NodeRole) -> Roles {
        match role {
            NodeRole::Server => Roles::STANDALONE,
            NodeRole::Agent => Roles { node: true, server: false },
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
        matches!(self, NodePhase::Departed | NodePhase::Removed | NodePhase::Forgotten)
    }

    pub fn schedulable(self) -> bool {
        self == NodePhase::Active
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    pub cpus: u32,
    pub memory_mib: u64,
    /// Free-standing hugepages by page size in KiB.
    #[serde(default)]
    pub hugepages: BTreeMap<String, u64>,
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
    /// Provider networks this node maps: physnet → bridge (§11.3).
    #[serde(default)]
    pub physnets: BTreeMap<String, String>,
    /// The highest write-set format and schema this build uses (§6.6).
    #[serde(default)]
    pub feature_level: u32,
}

/// What this build supports. A new write-set format or schema migration
/// raises it, and is used only once the cluster's level (the minimum over
/// its servers) has reached it.
pub const FEATURE_LEVEL: u32 = 1;

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
                versions: Versions { glidex: env!("CARGO_PKG_VERSION").into(), ..Default::default() },
                capacity: Resources::default(),
                allocatable: Resources::default(),
                features: NodeFeatures::default(),
                pci_devices: Vec::new(),
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
storage_from!(redb::TransactionError, redb::TableError, redb::StorageError, redb::CommitError, serde_json::Error);

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
        out.sort_by(|a, b| a.spec.name.cmp(&b.spec.name).then(a.meta.id.cmp(&b.meta.id)));
        Ok(out)
    }

    /// Insert or replace. Names are unique among live nodes.
    pub fn put(&self, n: &Node) -> Result<(), NodeError> {
        if !n.status.phase.is_tombstone()
            && self.list()?.iter().any(|o| o.meta.id != n.meta.id && !o.status.phase.is_tombstone() && o.spec.name == n.spec.name)
        {
            return Err(NodeError::NameTaken(n.spec.name.clone()));
        }
        let bytes = serde_json::to_vec(n)?;
        let r = self.db.write(Origin::Controller, |tx| -> Result<(), NodeError> {
            let mut t = tx.open_table(TableId::Nodes.definition())?;
            t.insert(n.meta.id.as_str(), bytes.as_slice())?;
            Ok(())
        });
        match r {
            // A follower server reporting on itself: the leader writes it.
            Err(NodeError::Store(StoreError::NotLeader { .. })) => {
                Ok(self.db.forward_raw(vec![crate::store::Op::Put { table: TableId::Nodes, key: n.meta.id.as_bytes().to_vec(), value: bytes }])?)
            }
            other => other,
        }
    }

    /// Make sure the implicit node of this host exists, and refresh what a
    /// node reports about itself (capacity, features, versions). Writes
    /// nothing when nothing changed (§4.1).
    pub fn ensure_self(&self, id: &str, probe: SelfProbe) -> Result<Node, NodeError> {
        self.report_self(id, probe, true).map(|n| n.expect("created"))
    }

    /// Update this node's own record with what the host has now. `create`:
    /// make the record when there is none (a standalone host). A cluster
    /// member never does: its record is the cluster's (join wrote it), and a
    /// cache that hasn't received it yet must not replace it with a guess.
    pub fn report_self(&self, id: &str, probe: SelfProbe, create: bool) -> Result<Option<Node>, NodeError> {
        let mut node = match self.get(id)? {
            Some(n) => n,
            None if create => Node::new(id, NodeSpec { name: probe.name.clone(), role: NodeRole::Server, unschedulable: false, labels: BTreeMap::new() }),
            None => return Ok(None),
        };
        let before = node.clone();
        node.status.capacity = probe.capacity;
        node.status.allocatable = probe.allocatable;
        node.status.features = probe.features;
        node.status.pci_devices = probe.pci_devices;
        node.status.versions.glidex = env!("CARGO_PKG_VERSION").into();
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
        let cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
        let memory_mib = std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| {
                m.lines()
                    .find_map(|l| l.strip_prefix("MemTotal:"))
                    .and_then(|r| r.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()))
            })
            .map(|kib| kib / 1024)
            .unwrap_or(0);
        let mut hugepages = BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir("/sys/kernel/mm/hugepages") {
            for e in rd.flatten() {
                let dir = e.file_name().to_string_lossy().into_owned();
                if let Some(kb) = dir.strip_prefix("hugepages-").and_then(|s| s.strip_suffix("kB")) {
                    let n = std::fs::read_to_string(e.path().join("nr_hugepages")).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
                    if n > 0 {
                        hugepages.insert(kb.to_string(), n);
                    }
                }
            }
        }
        let capacity = Resources { cpus, memory_mib, hugepages: hugepages.clone() };
        let allocatable = Resources {
            cpus: cpus.saturating_sub(reserved.cpus).max(1),
            memory_mib: memory_mib.saturating_sub(reserved.memory_mib),
            hugepages,
        };
        let hypervisors = [crate::hypervisor::HypervisorType::CloudHypervisor, crate::hypervisor::HypervisorType::Qemu]
            .into_iter()
            .filter(|t| crate::hypervisor::driver(*t).is_available())
            .map(|t| t.to_string())
            .collect();
        let features = NodeFeatures {
            kvm: std::path::Path::new("/dev/kvm").exists(),
            hypervisors,
            iommu: std::fs::read_dir("/sys/kernel/iommu_groups").map(|mut d| d.next().is_some()).unwrap_or(false),
            ..Default::default()
        };
        let pci_devices = crate::pci::scan_pci_devices().into_iter().map(|d| d.address).collect();
        SelfProbe { name, capacity, allocatable, features, pci_devices }
    }
}
