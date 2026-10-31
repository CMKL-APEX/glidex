//! VM networks (spec §11): the VM-facing `Network` records, and the
//! client side of glidex-netd, which owns all host network state.

use crate::store::Db;
use glidex_netd::client::{Client, ClientError};
use glidex_netd::proto::{ErrorBody, FULL_SOCKET_NAME, OnBehalfOf, Op, STATUS_SOCKET_NAME};
use glidex_ovs::names::{MAX_IFNAME, validate_name};
use glidex_ovs::net::Ipv4Net;
use glidex_ovs::vm_port::VmPortKind;
use redb::{ReadableTable, TableDefinition};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const NETWORKS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("networks");

/// Name of the NAT network created automatically (spec §10).
pub const DEFAULT_NETWORK: &str = "default";
pub const DEFAULT_BRIDGE: &str = "gxbr-nat";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// glidex bridge + gateway + DHCP + masquerade.
    Nat,
    /// A bridge with an uplink onto a LAN (DHCP from the LAN).
    Bridged,
    /// VM-to-VM only.
    Isolated,
}

/// Where a network exists (spec/14-clustering.md D10): on one node (today's OVS
/// bridges), or across the cluster (OVN, milestone C5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkScope {
    #[default]
    Node,
    Cluster,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    /// UUID; the key of the record, of Cedar's `Network` entity, of VM
    /// NICs and of IPAM (spec/19-tenancy.md D24).
    pub id: String,
    /// `[a-z0-9-]`, at most 32; a project network's is unique in its
    /// tenant, a host network's on the cluster (D24).
    pub name: String,
    /// What a network from before ids was known by in OVN (its name): its
    /// switch stays `gx-<name>`. Absent: `gx-<id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ovn_key: Option<String>,
    /// `node`: the bridge, NAT and leases are on `node` only.
    #[serde(default)]
    pub scope: NetworkScope,
    /// The node of a `scope: node` network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Cluster networks on a physical network: its name (the VLAN is `vlan`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physnet: Option<String>,
    /// The VPC router a NAT cluster network is on; `None`: the shared edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router: Option<String>,
    pub bridge: String,
    pub mode: NetworkMode,
    pub port_type: VmPortKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlan: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// NAT networks: dnsmasq answers DNS as well as DHCP. Kept so a lost NAT
    /// can be created again the way it was.
    #[serde(default = "default_dns")]
    pub dns: bool,
    /// The network created its bridge (and NAT) and removes them on delete.
    #[serde(default)]
    pub owns_bridge: bool,
    /// Node NAT networks: its guests may reach this host's API (its
    /// `listen` ports), which netd's `input` chain otherwise closes to them
    /// (spec/29-workload-identity.md §15). Set by host network administrators.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub host_api: bool,
    pub created_at: u64,
    /// Project networks (spec/11-security.md §6.2) belong to a project and
    /// are usable by it and the projects in `shares`. Host networks have
    /// no project and are usable by `grants` (or every project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub all_projects: bool,
    #[serde(default)]
    pub grants: Vec<String>,
    /// Tenants whose every project may use this host network
    /// (spec/19-tenancy.md D25).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tenant_grants: Vec<String>,
    /// Projects that accepted a share of this project network.
    #[serde(default)]
    pub shares: Vec<String>,
    /// Pending share offers (spec §6.2.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub share_offers: Vec<ShareOffer>,
    /// What the network controller last saw in netd
    /// (spec/12-reconciliation.md §10.3).
    #[serde(default)]
    pub phase: NetworkPhase,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<crate::models::Condition>,
    /// Deletion requested; the network controller removes netd's records,
    /// then this one (spec/12-reconciliation.md §6.3, §10.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_requested_at: Option<u64>,
}

fn default_dns() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPhase {
    Pending,
    #[default]
    Ready,
    /// netd lacks part of it (bridge, NAT); see its `Ready` condition.
    Degraded,
    NetdUnavailable,
}

/// How long a share offer stays open.
pub const SHARE_OFFER_SECS: u64 = 7 * 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareOffer {
    pub project: String,
    pub offered_by: String,
    pub offered_at: u64,
    pub expires_at: u64,
}

/// The id a network from before ids gets: derived from its name, so every
/// server of a cluster migrating its copy picks the same one.
pub fn legacy_id(name: &str) -> String {
    uuid::Uuid::new_v5(&LEGACY_NAMESPACE, name.as_bytes()).to_string()
}

const LEGACY_NAMESPACE: uuid::Uuid = uuid::uuid!("6b2b8f0e-6f1d-4f0a-9a43-2f6f3b1c9d10");

impl Network {
    /// The name its OVN objects are made under (`gx-<key>`).
    pub fn ovn_key(&self) -> &str {
        self.ovn_key.as_deref().unwrap_or(&self.id)
    }

    /// Whether VMs of `project`, in `tenant`, may attach (mirrors
    /// `base.network-grant`).
    pub fn usable_by(&self, project: &str, tenant: &str) -> bool {
        match &self.project {
            Some(p) => p == project || self.shares.iter().any(|s| s == project),
            None => {
                self.all_projects
                    || self.grants.iter().any(|g| g == project)
                    || self.tenant_grants.iter().any(|t| t == tenant)
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateNetworkRequest {
    pub name: String,
    pub mode: NetworkMode,
    #[serde(default = "default_port_type")]
    pub port_type: VmPortKind,
    /// Defaults to `gxbr-<name>` for NAT/isolated networks.
    #[serde(default)]
    pub bridge: Option<String>,
    /// NAT only; default: first free /24 in 10.88.0.0/16.
    #[serde(default)]
    pub subnet: Option<Ipv4Net>,
    #[serde(default)]
    pub vlan: Option<u16>,
    #[serde(default)]
    pub mtu: Option<u16>,
    #[serde(default = "default_true")]
    pub dns: bool,
    /// `cluster` (OVN) or `node` (an OVS bridge on one host). Default: the
    /// cluster's, when OVN is enabled, else the node's.
    #[serde(default)]
    pub scope: Option<NetworkScope>,
    /// A provider network (cluster scope, mode bridged): the physical
    /// network it sits on (§11.2).
    #[serde(default)]
    pub physnet: Option<String>,
    /// A VPC router to attach a NAT network to (§11.2a); moved later with
    /// `PUT /projects/{id}/networks/{network}/router`.
    #[serde(default)]
    pub router: Option<String>,
}

fn default_port_type() -> VmPortKind {
    VmPortKind::Tap
}

fn default_true() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("glidex-netd unavailable: {0}")]
    Unavailable(String),
    #[error("{}", .0.message)]
    Netd(ErrorBody),
    #[error("glidex-netd protocol error: {0}")]
    Protocol(String),
    #[error("invalid network: {0}")]
    Invalid(String),
    #[error("network not found: {0}")]
    NotFound(String),
    /// A name more than one network in scope has (D24).
    #[error("{0}")]
    Ambiguous(String),
    #[error("{0}")]
    Conflict(String),
    #[error("the external address pool has no free address")]
    ExternalPoolExhausted,
    #[error("network storage error: {0}")]
    Storage(String),
}

impl From<ClientError> for NetError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Unavailable(m) => NetError::Unavailable(m),
            ClientError::Protocol(m) => NetError::Protocol(m),
            ClientError::Remote(body) => NetError::Netd(body),
        }
    }
}

impl From<crate::store::StoreError> for NetError {
    fn from(e: crate::store::StoreError) -> Self {
        NetError::Storage(e.to_string())
    }
}

fn storage(e: impl std::fmt::Display) -> NetError {
    NetError::Storage(e.to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The control plane's `networks` table.
pub struct NetworkStore {
    db: Arc<Db>,
    /// Serializes writes, so `put`'s read-modify-write keeps a deletion.
    write: std::sync::Mutex<()>,
}

impl NetworkStore {
    pub fn new(db: Arc<Db>) -> Result<Self, NetError> {
        Ok(Self {
            db,
            write: std::sync::Mutex::new(()),
        })
    }

    /// Remove a record (its network is gone everywhere).
    pub fn remove(&self, id: &str) -> Result<(), NetError> {
        let _w = glidex_raftstore::wait::lock(&self.write).unwrap();
        self.db.write(
            crate::store::Origin::Network,
            |tx| -> Result<(), NetError> {
                tx.open_table(NETWORKS_TABLE)
                    .map_err(storage)?
                    .remove(id)
                    .map_err(storage)?;
                Ok(())
            },
        )
    }

    pub fn insert(&self, net: &Network) -> Result<(), NetError> {
        let _w = glidex_raftstore::wait::lock(&self.write).unwrap();
        self.write_record(net)
    }

    /// By id.
    pub fn get(&self, id: &str) -> Result<Option<Network>, NetError> {
        let txn = self.db.begin_read().map_err(storage)?;
        let t = txn.open_table(NETWORKS_TABLE).map_err(storage)?;
        match t.get(id).map_err(storage)? {
            Some(v) => serde_json::from_slice(v.value()).map(Some).map_err(storage),
            None => Ok(None),
        }
    }

    pub fn list(&self) -> Result<Vec<Network>, NetError> {
        let txn = self.db.begin_read().map_err(storage)?;
        let t = txn.open_table(NETWORKS_TABLE).map_err(storage)?;
        let mut out = Vec::new();
        for entry in t.iter().map_err(storage)? {
            let (_, v) = entry.map_err(storage)?;
            out.push(serde_json::from_slice(v.value()).map_err(storage)?);
        }
        Ok(out)
    }

    /// Update a network. As for images, a write from an older copy can't
    /// undo a deletion: a network no longer recorded stays gone, and a
    /// requested deletion is kept.
    pub fn put(&self, net: &Network) -> Result<(), NetError> {
        let _w = glidex_raftstore::wait::lock(&self.write).unwrap();
        let Some(current) = self.get(&net.id)? else {
            return Ok(());
        };
        let mut net = net.clone();
        net.deletion_requested_at = net.deletion_requested_at.or(current.deletion_requested_at);
        self.write_record(&net)
    }

    fn write_record(&self, net: &Network) -> Result<(), NetError> {
        let bytes = serde_json::to_vec(net).map_err(storage)?;
        let txn = self
            .db
            .begin(crate::store::Origin::Network)
            .map_err(storage)?;
        {
            let mut t = txn.open_table(NETWORKS_TABLE).map_err(storage)?;
            t.insert(net.id.as_str(), bytes.as_slice())
                .map_err(storage)?;
        }
        txn.commit().map_err(storage)?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), NetError> {
        let _w = glidex_raftstore::wait::lock(&self.write).unwrap();
        let txn = self
            .db
            .begin(crate::store::Origin::Network)
            .map_err(storage)?;
        {
            let mut t = txn.open_table(NETWORKS_TABLE).map_err(storage)?;
            t.remove(id).map_err(storage)?;
        }
        txn.commit().map_err(storage)?;
        Ok(())
    }
}

impl CreateNetworkRequest {
    /// Validate and turn into a record (without touching the host).
    pub fn to_network(&self) -> Result<Network, NetError> {
        let invalid = |e: glidex_ovs::OvsError| NetError::Invalid(e.to_string());
        validate_name("network", &self.name, 32).map_err(invalid)?;
        let id = uuid::Uuid::new_v4().to_string();
        let bridge = match (&self.bridge, self.mode) {
            (Some(b), _) => b.clone(),
            (None, NetworkMode::Bridged) => {
                return Err(NetError::Invalid(
                    "a bridged network needs an existing bridge".into(),
                ));
            }
            // Names repeat across tenants; the id doesn't.
            (None, _) => format!("gxbr-{}", &id[..8]),
        };
        validate_name("bridge", &bridge, MAX_IFNAME).map_err(|_| {
            NetError::Invalid(format!(
                "bridge name '{}' must be ≤ {} chars of [a-z0-9-]; pass `bridge` explicitly",
                bridge, MAX_IFNAME
            ))
        })?;
        if self.subnet.is_some() && self.mode != NetworkMode::Nat {
            return Err(NetError::Invalid(
                "subnet only applies to NAT networks".into(),
            ));
        }
        if let Some(v) = self.vlan
            && !(1..=4094).contains(&v)
        {
            return Err(NetError::Invalid("vlan must be 1-4094".into()));
        }
        Ok(Network {
            id,
            name: self.name.clone(),
            ovn_key: None,
            scope: NetworkScope::Node,
            node: Some(crate::authz::node_id()),
            physnet: None,
            router: None,
            bridge,
            mode: self.mode,
            port_type: self.port_type,
            vlan: self.vlan,
            mtu: self.mtu,
            dns: self.dns,
            owns_bridge: self.mode != NetworkMode::Bridged,
            host_api: false,
            created_at: now(),
            project: None,
            all_projects: false,
            grants: Vec::new(),
            tenant_grants: Vec::new(),
            shares: Vec::new(),
            share_offers: Vec::new(),
            phase: NetworkPhase::Ready,
            conditions: Vec::new(),
            deletion_requested_at: None,
        })
    }
}

/// Access to glidex-netd. Each call opens a short connection (hello +
/// request): netd calls are rare and this survives netd restarts.
#[derive(Debug, Clone)]
pub struct Netd {
    run_dir: PathBuf,
}

/// Which netd socket answered a status request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NetdAccess {
    Full,
    Status,
    None,
}

impl Netd {
    pub fn new(run_dir: impl Into<PathBuf>) -> Self {
        Self {
            run_dir: run_dir.into(),
        }
    }

    /// `GLIDEX_NETD_RUN_DIR`, else `/run/glidex`.
    pub fn from_env() -> Self {
        Self::new(
            std::env::var_os("GLIDEX_NETD_RUN_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(glidex_netd::proto::DEFAULT_RUN_DIR)),
        )
    }

    fn timeout(op: &Op) -> Duration {
        match op {
            Op::InstallOvs(_) => Duration::from_secs(45 * 60),
            Op::InitDpdk(_) => Duration::from_secs(5 * 60),
            // Covers netd's gateway check after an IP migration.
            Op::EnsureUplink(_) => Duration::from_secs(90),
            // Metering asks every round; never hold a round for long.
            Op::PortStats | Op::NatCounters => Duration::from_secs(5),
            _ => Duration::from_secs(30),
        }
    }

    /// Blocking call on the full socket, up to its op's timeout. On a Tokio
    /// worker it waits in `block_in_place`: a worker blocked outright can
    /// leave no thread driving the runtime's I/O for that long.
    pub fn call<T: DeserializeOwned>(&self, op: Op) -> Result<T, NetError> {
        let timeout = Self::timeout(&op);
        glidex_raftstore::wait::blocking(|| {
            let mut client = Client::connect(&self.run_dir.join(FULL_SOCKET_NAME))?;
            Ok(client.call(op, timeout)?)
        })
    }

    /// As `call`, telling netd whom the request is for (logged by netd
    /// next to our uid; never used for authorization, spec §8.3).
    pub fn call_as<T: DeserializeOwned>(
        &self,
        op: Op,
        on_behalf_of: &OnBehalfOf,
    ) -> Result<T, NetError> {
        let timeout = Self::timeout(&op);
        glidex_raftstore::wait::blocking(|| {
            let mut client = Client::connect(&self.run_dir.join(FULL_SOCKET_NAME))?;
            Ok(client.call_as(op, Some(on_behalf_of), timeout)?)
        })
    }

    /// The identity of netd's full socket (device, inode): it changes when
    /// netd restarts and binds it anew. `None` if there is none.
    pub fn socket_identity(&self) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(self.run_dir.join(FULL_SOCKET_NAME))
            .ok()
            .map(|m| (m.dev(), m.ino()))
    }

    /// Host status: full socket if we're allowed, else the status socket.
    /// Up to 30 s: off the worker on a Tokio runtime's, as [`Self::call`].
    pub fn probe(&self) -> (NetdAccess, Result<serde_json::Value, NetError>) {
        glidex_raftstore::wait::blocking(|| self.probe_now())
    }

    fn probe_now(&self) -> (NetdAccess, Result<serde_json::Value, NetError>) {
        match Client::connect(&self.run_dir.join(FULL_SOCKET_NAME)) {
            Ok(mut c) => (
                NetdAccess::Full,
                c.call_value(Op::Probe, Duration::from_secs(30))
                    .map_err(Into::into),
            ),
            Err(full_err) => match Client::connect(&self.run_dir.join(STATUS_SOCKET_NAME)) {
                Ok(mut c) => (
                    NetdAccess::Status,
                    c.call_value(Op::Probe, Duration::from_secs(30))
                        .map_err(Into::into),
                ),
                Err(_) => (NetdAccess::None, Err(full_err.into())),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(name: &str, mode: NetworkMode) -> CreateNetworkRequest {
        CreateNetworkRequest {
            name: name.into(),
            mode,
            port_type: VmPortKind::Tap,
            bridge: None,
            subnet: None,
            vlan: None,
            mtu: None,
            dns: true,
            scope: None,
            physnet: None,
            router: None,
        }
    }

    /// Commits once the runtime's timer fires, as Raft's commit waits on
    /// its network.
    struct OnTimer(tokio::runtime::Handle);

    impl glidex_raftstore::store::Replicator<crate::store::Glidex> for OnTimer {
        fn propose(
            &self,
            db: &crate::store::Db,
            ws: glidex_raftstore::store::WriteSet<crate::store::Glidex>,
        ) -> Result<u64, glidex_raftstore::store::StoreError> {
            let nap = tokio::time::sleep(std::time::Duration::from_millis(50));
            glidex_raftstore::raft::wait_on(&self.0, nap);
            let rev = db.revision() + 1;
            db.apply(&ws, rev, |_| Ok(()))?;
            Ok(rev)
        }
    }

    /// Two network writes at once on a one-worker runtime. The store's own
    /// lock is held across the commit, so the second write waits for it off
    /// the worker; blocked outright, it stops the timer the commit waits on
    /// and neither finishes (prism2's and prism1's hangs, one lock earlier).
    #[test]
    fn concurrent_network_writes_leave_the_runtime_running() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::store::Db::create(dir.path().join("g.db")).unwrap());
        db.set_replicator(Arc::new(OnTimer(rt.handle().clone())));
        let store = Arc::new(NetworkStore::new(db).unwrap());
        let (done, rx) = std::sync::mpsc::channel();
        for name in ["a", "b"] {
            let (store, done) = (store.clone(), done.clone());
            rt.spawn(async move {
                let net = req(name, NetworkMode::Nat).to_network().unwrap();
                let _ = done.send(store.insert(&net).is_ok());
            });
        }
        let results: Vec<_> = (0..2)
            .map(|_| rx.recv_timeout(std::time::Duration::from_secs(10)))
            .collect();
        // Dropping a hung runtime would wait on its blocked threads.
        rt.shutdown_background();
        for r in results {
            assert_eq!(r, Ok(true), "a network write hung or failed");
        }
    }

    /// A slow netd answer on a one-worker runtime: the call waits off the
    /// worker, so the runtime's timer (and in production its I/O) keeps
    /// running meanwhile. Blocked outright, the timer fires only after netd
    /// answers.
    #[test]
    fn a_slow_netd_call_leaves_the_runtime_running() {
        use std::io::{BufRead, Write};
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let listener =
            std::os::unix::net::UnixListener::bind(dir.path().join(FULL_SOCKET_NAME)).unwrap();
        // A netd that greets at once and answers the request after 500 ms.
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut r = std::io::BufReader::new(s.try_clone().unwrap());
            let mut id = || {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["id"].clone()
            };
            let hello = serde_json::json!({
                "protocol": glidex_netd::proto::PROTOCOL_VERSION,
                "netd_version": "test",
            });
            writeln!(s, "{}", serde_json::json!({ "id": id(), "ok": hello })).unwrap();
            let id = id();
            std::thread::sleep(Duration::from_millis(500));
            writeln!(s, "{}", serde_json::json!({ "id": id, "ok": [] })).unwrap();
        });
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let netd = Netd::new(dir.path());
        let (done, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let timer = done.clone();
        rt.spawn(async move {
            let r = netd.call::<serde_json::Value>(Op::ListBridges);
            let _ = done.send(("netd", started.elapsed(), r.is_ok()));
        });
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = timer.send(("timer", started.elapsed(), true));
        });
        let first = rx.recv_timeout(Duration::from_secs(10));
        let second = rx.recv_timeout(Duration::from_secs(10));
        rt.shutdown_background();
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_eq!(
            first.0, "timer",
            "the timer waited for netd: {first:?}, {second:?}"
        );
        assert!(second.2, "the netd call failed: {second:?}");
    }

    #[test]
    fn derives_bridge_names_and_validates() {
        let n = req("lab", NetworkMode::Nat).to_network().unwrap();
        assert_eq!(n.bridge, format!("gxbr-{}", &n.id[..8]));
        assert_eq!(n.ovn_key(), n.id);
        assert!(n.owns_bridge);
        let mut r = req("lab", NetworkMode::Nat);
        r.bridge = Some("averyverylongname".into());
        assert!(r.to_network().is_err(), "bridge would exceed 15 chars");
        assert!(
            req("a-name-of-thirty-two-characters0", NetworkMode::Nat)
                .to_network()
                .is_ok(),
            "the name no longer makes the bridge's"
        );
        assert_eq!(legacy_id("lab"), legacy_id("lab"));
        assert_ne!(legacy_id("lab"), legacy_id("lab2"));
        assert!(
            req("lan", NetworkMode::Bridged).to_network().is_err(),
            "bridged needs a bridge"
        );
        let mut r = req("lan", NetworkMode::Bridged);
        r.bridge = Some("gxbr-up".into());
        assert!(!r.to_network().unwrap().owns_bridge);
        let mut r = req("iso", NetworkMode::Isolated);
        r.subnet = Some("10.9.0.0/24".parse().unwrap());
        assert!(r.to_network().is_err());
        assert!(req("Bad!", NetworkMode::Nat).to_network().is_err());
    }
}
