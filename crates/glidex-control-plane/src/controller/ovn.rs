//! The cluster network controller (spec/clustering.md §8.1, §11): on the
//! leader, keeps the OVN northbound database equal to the cluster's networks,
//! IPAM and placements; on every node, makes the host a chassis; on servers,
//! runs this server's part of `ovn-central`.

use crate::models::Vm;
use crate::network::{NetworkMode, NetworkScope};
use crate::state::VmManager;
use glidex_ovn::{Desired, EdgeSpec, NbConn, NetKind, NetworkSpec, PortSpec};
use glidex_ovs::exec::SystemExec;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

impl VmManager {
    fn nb_conn(&self) -> Option<NbConn> {
        let cluster = self.cluster()?;
        let servers: Vec<String> = self
            .nodes()
            .list()
            .ok()?
            .into_iter()
            .filter(|n| n.spec.role == crate::node::NodeRole::Server && !n.status.phase.is_tombstone())
            .filter_map(|n| n.status.advertise.map(|a| format!("ssl:{}:{}", a.ip(), glidex_ovs::ovn::NB_PORT)))
            .collect();
        if servers.is_empty() {
            return None;
        }
        Some(NbConn { db: servers, key: cluster.files.key(), cert: cluster.files.cert(), ca: cluster.files.trust(), daemon: None })
    }

    /// Whether this node is a gateway of the shared edge or of a VPC router
    /// (it then carries SNAT connections to meter, §13.3).
    pub(crate) fn is_ovn_gateway(&self) -> bool {
        if !self.ovn_enabled() {
            return false;
        }
        let me = self.local_node_id();
        let ovn = self.ovn.load_full();
        let nodes = self.nodes().list().unwrap_or_default();
        let is_me = |w: &String| w == &me || nodes.iter().any(|n| &n.spec.name == w && n.meta.id == me);
        ovn.edge.as_ref().is_some_and(|e| e.gateway_nodes.iter().any(is_me))
            || crate::router::list(&self.store.database()).unwrap_or_default().iter().any(|r| r.spec.gateway_nodes.iter().flatten().any(is_me))
    }

    /// What the northbound database should say right now.
    pub(crate) async fn desired_ovn(&self) -> Result<Desired, crate::state::VmManagerError> {
        let ovn = self.ovn.load_full();
        let nodes = self.nodes().list().unwrap_or_default();
        let subnets = crate::ipam::list_subnets(&self.store.database()).map_err(|e| crate::state::VmManagerError::PersistenceError(e.to_string()))?;
        let reservations = crate::ipam::list_reservations(&self.store.database()).map_err(|e| crate::state::VmManagerError::PersistenceError(e.to_string()))?;
        let mut d = Desired::default();
        for n in self.networks.list()?.into_iter().filter(|n| n.scope == NetworkScope::Cluster && n.deletion_requested_at.is_none()) {
            let sub = subnets.iter().find(|s| s.network == n.name);
            if sub.is_none() && n.mode != NetworkMode::Bridged {
                continue;
            }
            d.networks.push(NetworkSpec {
                name: n.name.clone(),
                kind: match (&n.mode, &n.physnet) {
                    (NetworkMode::Bridged, Some(p)) => NetKind::Provider { physnet: p.clone(), vlan: n.vlan },
                    (NetworkMode::Nat, _) => NetKind::Nat,
                    _ => NetKind::Isolated,
                },
                cidr: sub.and_then(|s| s.cidr.parse().ok()),
                dns: ovn.dns_servers.clone(),
                mtu: n.mtu.unwrap_or(1442),
                router: n.router.clone(),
            });
        }
        let vms = self.vms.read().await;
        for vm in vms.values() {
            let Some(p) = &vm.status.placement else { continue };
            for (i, a) in vm.config().networks.iter().enumerate() {
                if !d.networks.iter().any(|n| n.name == a.network) {
                    continue;
                }
                let mac = a.mac.clone().or_else(|| glidex_ovs::names::mac_address(&vm.id, i as u8).ok());
                let Some(mac) = mac else { continue };
                let provider = d.networks.iter().any(|n| n.name == a.network && matches!(n.kind, NetKind::Provider { .. }));
                let ip = if provider {
                    None
                } else {
                    let Some(r) = reservations.iter().find(|r| r.network == a.network && r.mac.eq_ignore_ascii_case(&mac)) else { continue };
                    Some(r.ip)
                };
                let Ok(lport) = glidex_ovs::names::port_name(&vm.id, i as u8) else { continue };
                d.ports.push(PortSpec { network: a.network.clone(), lport, mac: mac.to_ascii_lowercase(), ip, vm_id: vm.id.clone(), nic: i as u8, chassis: Some(p.node.clone()) });
            }
        }
        drop(vms);
        if let Some(e) = &ovn.edge {
            let cidr: Option<glidex_ovs::net::Ipv4Net> = e.external_cidr.parse().ok();
            if let (IpAddr::V4(ext), IpAddr::V4(gw), Some(cidr)) = (e.external_ip, e.gateway, cidr) {
                let gateway_nodes = e
                    .gateway_nodes
                    .iter()
                    .filter_map(|w| nodes.iter().find(|n| &n.meta.id == w || &n.spec.name == w).map(|n| n.meta.id.clone()))
                    .collect();
                // The shared edge keeps the first zone of the range, routers the rest.
                let zones = ovn.snat_ct_zones.unwrap_or((60000, 64999));
                d.edge = Some(EdgeSpec { physnet: e.physnet.clone(), external_ip: ext, external_prefix: cidr.prefix(), gateway: gw, gateway_nodes, snat_ct_zone: Some(zones.0) });
                let resolve = |names: &[String]| -> Vec<String> { names.iter().filter_map(|w| nodes.iter().find(|n| &n.meta.id == w || &n.spec.name == w).map(|n| n.meta.id.clone())).collect() };
                for r in crate::router::list(&self.store.database()).map_err(crate::state::router_err)? {
                    let external = match (r.spec.external, r.status.external_ip) {
                        (true, Some(ip)) => Some(glidex_ovn::ExternalSpec {
                            physnet: e.physnet.clone(),
                            external_ip: ip,
                            external_prefix: cidr.prefix(),
                            gateway: gw,
                            gateway_nodes: r.spec.gateway_nodes.as_deref().map(resolve).unwrap_or_else(|| d.edge.as_ref().map(|x| x.gateway_nodes.clone()).unwrap_or_default()),
                            snat_ct_zone: r.status.snat_ct_zone,
                        }),
                        _ => None,
                    };
                    d.routers.push(glidex_ovn::RouterSpec { name: r.id.clone(), external });
                }
            }
        }
        d.node_addresses = nodes
            .iter()
            .filter(|n| !n.status.phase.is_tombstone())
            .flat_map(|n| [n.status.advertise.map(|a| a.ip()), n.status.tunnel_ip])
            .flatten()
            .collect::<std::collections::BTreeSet<IpAddr>>()
            .into_iter()
            .collect();
        d.nat_supernet = ovn.nat_supernet.as_deref().and_then(|s| s.parse().ok());
        Ok(d)
    }

    /// What the northbound database should say (tests).
    #[doc(hidden)]
    pub async fn __desired_ovn_for_tests(&self) -> Result<Desired, crate::state::VmManagerError> {
        self.desired_ovn().await
    }

    /// One pass of the leader's network controller.
    pub async fn reconcile_ovn(self: &Arc<Self>) {
        if !self.ovn_enabled() || !self.store.database().can_write() {
            return;
        }
        self.gc_reservations().await;
        let Some(conn) = self.nb_conn() else { return };
        let desired = match self.desired_ovn().await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("planning OVN: {}", e);
                return;
            }
        };
        let r = tokio::task::spawn_blocking(move || {
            let exec = SystemExec::new();
            glidex_ovn::sync(&glidex_ovn::Nb::new(&exec, conn), &desired)
        })
        .await;
        match r {
            Ok(Ok(rep)) => {
                if !rep.changed.is_empty() {
                    tracing::info!(changed = ?rep.changed, "OVN northbound updated");
                }
                self.finish_cluster_network_deletes();
            }
            Ok(Err(e)) => tracing::warn!("OVN northbound sync: {}", e),
            Err(e) => tracing::warn!("OVN sync task: {}", e),
        }
    }

    /// A cluster network asked to be deleted goes once OVN no longer has it.
    fn finish_cluster_network_deletes(&self) {
        for n in self.networks.list().unwrap_or_default() {
            if n.scope == NetworkScope::Cluster && n.deletion_requested_at.is_some() {
                let name = n.name.clone();
                let _ = self.store.database().write(crate::store::Origin::Network, |tx| crate::ipam::free_subnet(tx, &name));
                let _ = self.networks.remove(&name);
                tracing::info!(network = %n.name, "cluster network deleted");
            }
        }
    }

    /// A reservation lives as long as the NIC it was made for (the `vm.ipam`
    /// finalizer of the plan, done as garbage collection): free any whose VM
    /// or NIC is gone.
    async fn gc_reservations(&self) {
        let Ok(res) = crate::ipam::list_reservations(&self.store.database()) else { return };
        let vms = self.vms.read().await;
        let stale: Vec<_> = res
            .into_iter()
            .filter(|r| {
                !vms.get(&r.vm_id).is_some_and(|vm: &Vm| {
                    vm.config().networks.iter().enumerate().any(|(i, a)| {
                        a.network == r.network && a.mac.clone().or_else(|| glidex_ovs::names::mac_address(&vm.id, i as u8).ok()).is_some_and(|m| m.eq_ignore_ascii_case(&r.mac))
                    })
                })
            })
            .collect();
        drop(vms);
        if stale.is_empty() {
            return;
        }
        let _ = self.store.database().write(crate::store::Origin::Network, |tx| -> Result<(), crate::ipam::IpamError> {
            for r in &stale {
                crate::ipam::release(tx, &r.network, &r.mac)?;
            }
            Ok(())
        });
    }

    /// Every node: make this host an OVN chassis, and a server also run its
    /// part of `ovn-central`. Idempotent; called every half minute.
    pub async fn ensure_ovn_node(&self) {
        if !self.ovn_enabled() {
            return;
        }
        let Some(cluster) = self.cluster() else { return };
        let nodes = self.nodes().list().unwrap_or_default();
        let servers: Vec<(String, IpAddr, chrono_like::Stamp)> = nodes
            .iter()
            .filter(|n| n.spec.role == crate::node::NodeRole::Server && !n.status.phase.is_tombstone())
            .filter_map(|n| Some((n.meta.id.clone(), n.status.advertise?.ip(), chrono_like::Stamp(n.meta.created_at))))
            .collect();
        let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap_or_default();
        let certs = glidex_ovs::ovn::ChassisCerts { key_pem: read(cluster.files.key()), cert_pem: read(cluster.files.cert()), ca_pem: read(cluster.files.trust()) };
        if certs.key_pem.is_empty() || servers.is_empty() {
            return;
        }
        let me = self.local_node_id();
        let netd = self.netd.clone();
        let tunnel = nodes.iter().find(|n| n.meta.id == me).and_then(|n| n.status.tunnel_ip).unwrap_or(cluster.identity.advertise.ip());
        // The oldest server makes OVN's databases; the others join it.
        let mut by_age = servers.clone();
        by_age.sort_by_key(|(_, _, t)| t.0);
        let first = by_age.first().map(|(id, ip, _)| (id.clone(), *ip));
        let is_server = cluster.role() == crate::node::NodeRole::Server;
        let all_ips: Vec<IpAddr> = servers.iter().map(|(_, ip, _)| *ip).collect();
        let sb_remotes: Vec<String> = all_ips.iter().map(|ip| format!("ssl:{ip}:{}", glidex_ovs::ovn::SB_PORT)).collect();
        let bridge_mappings = nodes.iter().find(|n| n.meta.id == me).map(|n| n.status.features.physnets.clone()).unwrap_or_default();
        let res = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
            use glidex_netd::proto::{EnsureOvnCentralArgs, EnsureOvnChassisArgs, Op};
            if is_server {
                let join = first.as_ref().filter(|(id, _)| *id != me).map(|(_, ip)| *ip);
                let local = cluster_local_ip(&all_ips, tunnel);
                let spec = glidex_ovs::ovn::CentralSpec { local_ip: local, join, servers: all_ips.clone() };
                netd.call::<serde_json::Value>(Op::EnsureOvnCentral(EnsureOvnCentralArgs { spec, certs: certs.clone() })).map_err(|e| e.to_string())?;
            }
            // `br-int`'s datapath: userspace where DPDK runs (vhost-user needs it).
            let datapath = match netd.probe() {
                (_, Ok(caps)) if caps.get("dpdk_initialized").and_then(|v| v.as_bool()).unwrap_or(false) => glidex_ovs::bridge::Datapath::Netdev,
                _ => glidex_ovs::bridge::Datapath::System,
            };
            let spec = glidex_ovs::ovn::ChassisSpec { chassis: me.clone(), sb_remotes, encap_ip: tunnel, bridge_mappings, datapath };
            let st: glidex_ovs::ovn::OvnStatus = netd.call(Op::EnsureOvnChassis(EnsureOvnChassisArgs { spec, certs })).map_err(|e| e.to_string())?;
            Ok(st.datapath)
        })
        .await;
        match res {
            Ok(Ok(datapath)) => {
                // The scheduler needs to know where vhost-user NICs can go (§9.1).
                let me = self.local_node_id();
                if let Ok(Some(mut n)) = self.nodes().get(&me) {
                    if n.status.features.br_int_datapath != datapath {
                        n.status.features.br_int_datapath = datapath;
                        n.meta.resource_version += 1;
                        let _ = self.nodes().put(&n);
                    }
                }
            }
            Ok(Err(e)) => tracing::debug!("OVN on this node: {}", e),
            Err(e) => tracing::warn!("OVN node task: {}", e),
        }
    }
}

/// This server's address among the servers': the one that is its own tunnel
/// address, else the first.
fn cluster_local_ip(all: &[IpAddr], mine: IpAddr) -> IpAddr {
    if all.contains(&mine) {
        mine
    } else {
        all.first().copied().unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    }
}

mod chrono_like {
    /// A creation time, ordered.
    #[derive(Debug, Clone, Copy)]
    pub struct Stamp(pub u64);
}
