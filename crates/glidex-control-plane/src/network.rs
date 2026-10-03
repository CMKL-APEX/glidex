//! VM networks (spec §11): the VM-facing `Network` records, and the
//! client side of glidex-netd, which owns all host network state.

use glidex_netd::client::{Client, ClientError};
use glidex_netd::proto::{ErrorBody, OnBehalfOf, Op, FULL_SOCKET_NAME, STATUS_SOCKET_NAME};
use glidex_ovs::names::{validate_name, MAX_IFNAME};
use glidex_ovs::net::Ipv4Net;
use glidex_ovs::vm_port::VmPortKind;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    pub name: String,
    pub bridge: String,
    pub mode: NetworkMode,
    pub port_type: VmPortKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vlan: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// The network created its bridge (and NAT) and removes them on delete.
    #[serde(default)]
    pub owns_bridge: bool,
    pub created_at: u64,
    /// Project networks (spec/security.md §6.2) belong to a project and
    /// are usable by it and the projects in `shares`. Host networks have
    /// no project and are usable by `grants` (or every project).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub all_projects: bool,
    #[serde(default)]
    pub grants: Vec<String>,
    /// Projects that accepted a share of this project network.
    #[serde(default)]
    pub shares: Vec<String>,
    /// Pending share offers (spec §6.2.1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub share_offers: Vec<ShareOffer>,
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

impl Network {
    /// Whether VMs of `project` may attach (mirrors `base.network-grant`).
    pub fn usable_by(&self, project: &str) -> bool {
        match &self.project {
            Some(p) => p == project || self.shares.iter().any(|s| s == project),
            None => self.all_projects || self.grants.iter().any(|g| g == project),
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
    #[error("{0}")]
    Conflict(String),
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
    db: Arc<Database>,
}

impl NetworkStore {
    pub fn new(db: Arc<Database>) -> Result<Self, NetError> {
        let txn = db.begin_write().map_err(storage)?;
        txn.open_table(NETWORKS_TABLE).map_err(storage)?;
        txn.commit().map_err(storage)?;
        Ok(Self { db })
    }

    pub fn get(&self, name: &str) -> Result<Option<Network>, NetError> {
        let txn = self.db.begin_read().map_err(storage)?;
        let t = txn.open_table(NETWORKS_TABLE).map_err(storage)?;
        match t.get(name).map_err(storage)? {
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

    pub fn put(&self, net: &Network) -> Result<(), NetError> {
        let bytes = serde_json::to_vec(net).map_err(storage)?;
        let txn = self.db.begin_write().map_err(storage)?;
        {
            let mut t = txn.open_table(NETWORKS_TABLE).map_err(storage)?;
            t.insert(net.name.as_str(), bytes.as_slice()).map_err(storage)?;
        }
        txn.commit().map_err(storage)
    }

    pub fn delete(&self, name: &str) -> Result<(), NetError> {
        let txn = self.db.begin_write().map_err(storage)?;
        {
            let mut t = txn.open_table(NETWORKS_TABLE).map_err(storage)?;
            t.remove(name).map_err(storage)?;
        }
        txn.commit().map_err(storage)
    }
}

impl CreateNetworkRequest {
    /// Validate and turn into a record (without touching the host).
    pub fn to_network(&self) -> Result<Network, NetError> {
        let invalid = |e: glidex_ovs::OvsError| NetError::Invalid(e.to_string());
        validate_name("network", &self.name, 32).map_err(invalid)?;
        let bridge = match (&self.bridge, self.mode) {
            (Some(b), _) => b.clone(),
            (None, NetworkMode::Bridged) => {
                return Err(NetError::Invalid("a bridged network needs an existing bridge".into()))
            }
            (None, _) => format!("gxbr-{}", self.name),
        };
        validate_name("bridge", &bridge, MAX_IFNAME).map_err(|_| {
            NetError::Invalid(format!(
                "bridge name '{}' must be ≤ {} chars of [a-z0-9-]; pass `bridge` explicitly",
                bridge, MAX_IFNAME
            ))
        })?;
        if self.subnet.is_some() && self.mode != NetworkMode::Nat {
            return Err(NetError::Invalid("subnet only applies to NAT networks".into()));
        }
        if let Some(v) = self.vlan {
            if !(1..=4094).contains(&v) {
                return Err(NetError::Invalid("vlan must be 1-4094".into()));
            }
        }
        Ok(Network {
            name: self.name.clone(),
            bridge,
            mode: self.mode,
            port_type: self.port_type,
            vlan: self.vlan,
            mtu: self.mtu,
            owns_bridge: self.mode != NetworkMode::Bridged,
            created_at: now(),
            project: None,
            all_projects: false,
            grants: Vec::new(),
            shares: Vec::new(),
            share_offers: Vec::new(),
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
            _ => Duration::from_secs(30),
        }
    }

    /// Blocking call on the full socket. Call from `spawn_blocking` or code
    /// that already blocks (the VM lifecycle does).
    pub fn call<T: DeserializeOwned>(&self, op: Op) -> Result<T, NetError> {
        let timeout = Self::timeout(&op);
        let mut client = Client::connect(&self.run_dir.join(FULL_SOCKET_NAME))?;
        Ok(client.call(op, timeout)?)
    }

    /// As `call`, telling netd whom the request is for (logged by netd
    /// next to our uid; never used for authorization, spec §8.3).
    pub fn call_as<T: DeserializeOwned>(&self, op: Op, on_behalf_of: &OnBehalfOf) -> Result<T, NetError> {
        let timeout = Self::timeout(&op);
        let mut client = Client::connect(&self.run_dir.join(FULL_SOCKET_NAME))?;
        Ok(client.call_as(op, Some(on_behalf_of), timeout)?)
    }

    /// The identity of netd's full socket (device, inode): it changes when
    /// netd restarts and binds it anew. `None` if there is none.
    pub fn socket_identity(&self) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(self.run_dir.join(FULL_SOCKET_NAME)).ok().map(|m| (m.dev(), m.ino()))
    }

    /// Host status: full socket if we're allowed, else the status socket.
    pub fn probe(&self) -> (NetdAccess, Result<serde_json::Value, NetError>) {
        match Client::connect(&self.run_dir.join(FULL_SOCKET_NAME)) {
            Ok(mut c) => (NetdAccess::Full, c.call_value(Op::Probe, Duration::from_secs(30)).map_err(Into::into)),
            Err(full_err) => match Client::connect(&self.run_dir.join(STATUS_SOCKET_NAME)) {
                Ok(mut c) => (NetdAccess::Status, c.call_value(Op::Probe, Duration::from_secs(30)).map_err(Into::into)),
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
        }
    }

    #[test]
    fn derives_bridge_names_and_validates() {
        let n = req("lab", NetworkMode::Nat).to_network().unwrap();
        assert_eq!(n.bridge, "gxbr-lab");
        assert!(n.owns_bridge);
        assert!(req("averyverylongname", NetworkMode::Nat).to_network().is_err(), "bridge would exceed 15 chars");
        assert!(req("lan", NetworkMode::Bridged).to_network().is_err(), "bridged needs a bridge");
        let mut r = req("lan", NetworkMode::Bridged);
        r.bridge = Some("gxbr-up".into());
        assert!(!r.to_network().unwrap().owns_bridge);
        let mut r = req("iso", NetworkMode::Isolated);
        r.subnet = Some("10.9.0.0/24".parse().unwrap());
        assert!(r.to_network().is_err());
        assert!(req("Bad!", NetworkMode::Nat).to_network().is_err());
    }
}
