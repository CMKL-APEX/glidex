//! The `cluster` section of `/etc/glidex/control-plane.json` (spec/clustering.md
//! §10.2): tunables and, for a host installed to join, where it will listen.
//! Whether this host *is* in a cluster is not decided here but by the identity
//! `gxctl cluster init` or a join wrote to the state directory
//! (`cluster/identity.json`): the unprivileged control plane can't write
//! `/etc`.

use crate::node::NodeRole;
use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ClusterConfig {
    pub role: NodeRole,
    /// Where the node API (:8842) listens.
    pub listen: SocketAddr,
    /// The address other nodes reach it at. Default: `listen`, with the
    /// first non-loopback address of this host when `listen` is unspecified.
    pub advertise: Option<SocketAddr>,
    /// Geneve encapsulation address (OVN, milestone C5); defaults to the
    /// advertise address.
    pub tunnel_ip: Option<IpAddr>,
    pub node_grace_secs: u64,
    pub shared_local_accounts: bool,
    pub raft: RaftConfig,
    pub scheduler: SchedulerConfig,
    pub node_reserved: NodeReserved,
    pub detach_freeze_secs: u64,
    pub import_plan_ttl_secs: u64,
    pub ca_rotation_grace_secs: u64,
    pub ovn: OvnConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RaftConfig {
    pub heartbeat_ms: u64,
    pub election_ms: (u64, u64),
    pub log_keep_entries: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SchedulerConfig {
    pub cpu_overcommit: f64,
    pub memory_overcommit: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NodeReserved {
    pub cpus: u32,
    pub memory_mib: u64,
}

/// OVN settings (milestone C5); parsed now so one file serves every build.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OvnConfig {
    /// Cluster networks on OVN (spec/clustering.md §11). Off until OVN is
    /// installed on the servers and the chassis.
    pub enabled: bool,
    pub underlay_mtu: Option<u16>,
    pub nat_supernet: Option<String>,
    pub edge: Option<EdgeConfig>,
    pub snat_ct_zones: Option<(u16, u16)>,
    pub dns_servers: Vec<IpAddr>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeConfig {
    pub physnet: String,
    pub external_cidr: String,
    pub gateway: IpAddr,
    pub external_ip: IpAddr,
    pub external_pool: Option<String>,
    pub gateway_nodes: Vec<String>,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        ClusterConfig {
            role: NodeRole::Server,
            listen: "0.0.0.0:8842".parse().unwrap(),
            advertise: None,
            tunnel_ip: None,
            node_grace_secs: 40,
            shared_local_accounts: false,
            raft: RaftConfig::default(),
            scheduler: SchedulerConfig::default(),
            node_reserved: NodeReserved::default(),
            detach_freeze_secs: 600,
            import_plan_ttl_secs: 86400,
            ca_rotation_grace_secs: 604800,
            ovn: OvnConfig::default(),
        }
    }
}

impl Default for RaftConfig {
    fn default() -> Self {
        RaftConfig { heartbeat_ms: 250, election_ms: (1000, 2000), log_keep_entries: 10_000 }
    }
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        SchedulerConfig { cpu_overcommit: 4.0, memory_overcommit: 1.0 }
    }
}

impl Default for NodeReserved {
    fn default() -> Self {
        NodeReserved { cpus: 1, memory_mib: 2048 }
    }
}

impl ClusterConfig {
    pub fn check(&self) -> Result<(), String> {
        let r = &self.raft;
        if !(50..=5000).contains(&r.heartbeat_ms) {
            return Err("cluster.raft.heartbeat_ms must be 50-5000".into());
        }
        if r.election_ms.0 < r.heartbeat_ms * 3 || r.election_ms.1 <= r.election_ms.0 {
            return Err("cluster.raft.election_ms must be [min, max] with min at least 3 heartbeats and max above min".into());
        }
        if r.log_keep_entries < 100 {
            return Err("cluster.raft.log_keep_entries must be at least 100".into());
        }
        if !(5..=3600).contains(&self.node_grace_secs) {
            return Err("cluster.node_grace_secs must be 5-3600".into());
        }
        if self.scheduler.cpu_overcommit < 1.0 || self.scheduler.memory_overcommit < 1.0 {
            return Err("cluster.scheduler overcommit factors must be at least 1.0".into());
        }
        Ok(())
    }

    pub fn raft_settings(&self) -> crate::cluster::raft::RaftSettings {
        crate::cluster::raft::RaftSettings { heartbeat_ms: self.raft.heartbeat_ms, election_ms: self.raft.election_ms, log_keep_entries: self.raft.log_keep_entries }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_example_parses_and_checks() {
        let c: ClusterConfig = serde_json::from_str(
            r#"{
              "role": "server", "listen": "0.0.0.0:8842", "advertise": "192.0.2.11:8842", "tunnel_ip": "192.0.2.11",
              "node_grace_secs": 40, "shared_local_accounts": false,
              "raft": { "heartbeat_ms": 250, "election_ms": [1000, 2000], "log_keep_entries": 10000 },
              "scheduler": { "cpu_overcommit": 4.0, "memory_overcommit": 1.0 },
              "node_reserved": { "cpus": 1, "memory_mib": 2048 },
              "detach_freeze_secs": 600, "import_plan_ttl_secs": 86400, "ca_rotation_grace_secs": 604800,
              "ovn": { "underlay_mtu": 1500, "nat_supernet": "10.89.0.0/16",
                "edge": { "physnet": "uplink", "external_cidr": "192.0.2.0/24", "gateway": "192.0.2.1",
                          "external_ip": "192.0.2.50", "external_pool": "192.0.2.64/26", "gateway_nodes": ["h1", "h2", "h3"] },
                "snat_ct_zones": [60000, 64999], "dns_servers": ["192.0.2.53"] } }"#,
        )
        .unwrap();
        c.check().unwrap();
        assert_eq!(c.raft.election_ms, (1000, 2000));
    }

    #[test]
    fn typos_are_refused() {
        assert!(serde_json::from_str::<ClusterConfig>(r#"{"raft": {"heartbeat": 1}}"#).is_err());
        let mut c = ClusterConfig::default();
        c.raft.election_ms = (300, 400);
        assert!(c.check().is_err());
    }
}
