//! The cluster network controller (spec/14-clustering.md §8.1, §11): on the
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
            .filter(|n| {
                n.spec.role == crate::node::NodeRole::Server && !n.status.phase.is_tombstone()
            })
            .filter_map(|n| {
                n.status
                    .advertise
                    .map(|a| format!("ssl:{}:{}", a.ip(), glidex_ovs::ovn::NB_PORT))
            })
            .collect();
        if servers.is_empty() {
            return None;
        }
        Some(NbConn {
            db: servers,
            key: cluster.files.key(),
            cert: cluster.files.cert(),
            ca: cluster.files.trust(),
            daemon: None,
        })
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
        let is_me =
            |w: &String| w == &me || nodes.iter().any(|n| &n.spec.name == w && n.meta.id == me);
        ovn.edge
            .as_ref()
            .is_some_and(|e| e.gateway_nodes.iter().any(is_me))
            || crate::router::list(&self.store.database())
                .unwrap_or_default()
                .iter()
                .any(|r| r.spec.gateway_nodes.iter().flatten().any(is_me))
    }

    /// What the northbound database should say right now.
    pub(crate) async fn desired_ovn(&self) -> Result<Desired, crate::state::VmManagerError> {
        let ovn = self.ovn.load_full();
        let nodes = self.nodes().list().unwrap_or_default();
        let subnets = crate::ipam::list_subnets(&self.store.database())
            .map_err(|e| crate::state::VmManagerError::PersistenceError(e.to_string()))?;
        let reservations = crate::ipam::list_reservations(&self.store.database())
            .map_err(|e| crate::state::VmManagerError::PersistenceError(e.to_string()))?;
        let mut d = Desired::default();
        // Network id → the name its OVN objects have (`ovn_key`).
        let mut keys: std::collections::HashMap<String, String> = Default::default();
        for n in self
            .networks
            .list()?
            .into_iter()
            .filter(|n| n.scope == NetworkScope::Cluster && n.deletion_requested_at.is_none())
        {
            let sub = subnets.iter().find(|s| s.network == n.id);
            if sub.is_none() && n.mode != NetworkMode::Bridged {
                continue;
            }
            keys.insert(n.id.clone(), n.ovn_key().to_string());
            d.networks.push(NetworkSpec {
                name: n.ovn_key().to_string(),
                kind: match (&n.mode, &n.physnet) {
                    (NetworkMode::Bridged, Some(p)) => NetKind::Provider {
                        physnet: p.clone(),
                        vlan: n.vlan,
                    },
                    (NetworkMode::Nat, _) => NetKind::Nat,
                    // An isolated network on a project's router is routed
                    // there like a NAT network (16-clustering-ui.md §8d).
                    (NetworkMode::Isolated, _) if n.router.is_some() => NetKind::Nat,
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
            let Some(p) = &vm.status.placement else {
                continue;
            };
            for (i, a) in vm.config().networks.iter().enumerate() {
                let Some(key) = keys.get(&a.network) else {
                    continue;
                };
                let mac = a
                    .mac
                    .clone()
                    .or_else(|| glidex_ovs::names::mac_address(&vm.id, i as u8).ok());
                let Some(mac) = mac else { continue };
                let provider = d
                    .networks
                    .iter()
                    .any(|n| &n.name == key && matches!(n.kind, NetKind::Provider { .. }));
                let ip = if provider {
                    None
                } else {
                    let Some(r) = reservations
                        .iter()
                        .find(|r| r.network == a.network && r.mac.eq_ignore_ascii_case(&mac))
                    else {
                        continue;
                    };
                    Some(r.ip)
                };
                let Ok(lport) = glidex_ovs::names::port_name(&vm.id, i as u8) else {
                    continue;
                };
                d.ports.push(PortSpec {
                    network: key.clone(),
                    lport,
                    mac: mac.to_ascii_lowercase(),
                    ip,
                    vm_id: vm.id.clone(),
                    nic: i as u8,
                    chassis: Some(glidex_ovs::ovn::chassis_name(&p.node)),
                });
            }
        }
        drop(vms);
        if let Some(e) = &ovn.edge {
            let cidr: Option<glidex_ovs::net::Ipv4Net> = e.external_cidr.parse().ok();
            if let (IpAddr::V4(ext), IpAddr::V4(gw), Some(cidr)) = (e.external_ip, e.gateway, cidr)
            {
                let gateway_nodes = e
                    .gateway_nodes
                    .iter()
                    .filter_map(|w| {
                        nodes
                            .iter()
                            .find(|n| {
                                (&n.meta.id == w || &n.spec.name == w)
                                    && !n.status.phase.is_tombstone()
                            })
                            .map(|n| glidex_ovs::ovn::chassis_name(&n.meta.id))
                    })
                    .collect();
                // The shared edge keeps the first zone of the range, routers the rest.
                let zones = ovn.snat_ct_zones.unwrap_or((60000, 64999));
                d.edge = Some(EdgeSpec {
                    physnet: e.physnet.clone(),
                    external_ip: ext,
                    external_prefix: cidr.prefix(),
                    gateway: gw,
                    gateway_nodes,
                    snat_ct_zone: Some(zones.0),
                });
                let resolve = |names: &[String]| -> Vec<String> {
                    names
                        .iter()
                        .filter_map(|w| {
                            nodes
                                .iter()
                                .find(|n| {
                                    (&n.meta.id == w || &n.spec.name == w)
                                        && !n.status.phase.is_tombstone()
                                })
                                .map(|n| glidex_ovs::ovn::chassis_name(&n.meta.id))
                        })
                        .collect()
                };
                for r in
                    crate::router::list(&self.store.database()).map_err(crate::state::router_err)?
                {
                    let external = match (r.spec.external, r.status.external_ip) {
                        (true, Some(ip)) => Some(glidex_ovn::ExternalSpec {
                            physnet: e.physnet.clone(),
                            external_ip: ip,
                            external_prefix: cidr.prefix(),
                            gateway: gw,
                            gateway_nodes: r
                                .spec
                                .gateway_nodes
                                .as_deref()
                                .map(resolve)
                                .unwrap_or_else(|| {
                                    d.edge
                                        .as_ref()
                                        .map(|x| x.gateway_nodes.clone())
                                        .unwrap_or_default()
                                }),
                            snat_ct_zone: r.status.snat_ct_zone,
                        }),
                        _ => None,
                    };
                    d.routers.push(glidex_ovn::RouterSpec {
                        name: r.id.clone(),
                        external,
                    });
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
        // The supernet subnets come from (create_cluster_network's default):
        // isolation holds whether or not it was set.
        d.nat_supernet = ovn
            .nat_supernet
            .as_deref()
            .unwrap_or("10.89.0.0/16")
            .parse()
            .ok();
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
        let sb_conn = conn.clone();
        let r = tokio::task::spawn_blocking(move || {
            let exec = SystemExec::new();
            glidex_ovn::sync(&glidex_ovn::Nb::new(&exec, conn), &desired)
        })
        .await;
        self.forget_stale_chassis(sb_conn).await;
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

    /// Chassis of nodes that are no longer members. Removal deletes a node's
    /// chassis, but a removed node whose `ovn-controller` still runs registers
    /// it again: its OVN certificate is the cluster CA's, and OVN doesn't read
    /// glidex's deny-list. The leader deletes them again, about once a minute.
    async fn forget_stale_chassis(&self, conn: NbConn) {
        static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = crate::tenancy::now();
        if now.saturating_sub(LAST.load(std::sync::atomic::Ordering::Relaxed)) < 60 {
            return;
        }
        LAST.store(now, std::sync::atomic::Ordering::Relaxed);
        let sb = self.ovn_endpoints(glidex_ovs::ovn::SB_PORT);
        let Ok(Ok(chassis)) = tokio::task::spawn_blocking(move || {
            glidex_ovn::view::chassis(&SystemExec::new(), &conn, &sb)
        })
        .await
        else {
            return;
        };
        let names: Vec<String> = chassis.into_iter().map(|c| c.name).collect();
        for (chassis, node) in stale_chassis(&names, &self.nodes().list().unwrap_or_default()) {
            let netd = self.netd.clone();
            let args = glidex_netd::proto::ForgetOvnMemberArgs {
                address: String::new(),
                chassis: chassis.clone(),
                server: false,
            };
            match tokio::task::spawn_blocking(move || {
                netd.call::<()>(glidex_netd::proto::Op::ForgetOvnMember(args))
            })
            .await
            {
                Ok(Ok(())) => tracing::warn!(
                    node,
                    chassis,
                    "deleted the OVN chassis of a node that is no longer a member: its ovn-controller still runs (on it: `sudo glidex-install --leave`)"
                ),
                Ok(Err(e)) => tracing::warn!(node, chassis, "deleting a stale OVN chassis: {}", e),
                Err(e) => tracing::warn!("deleting a stale OVN chassis: {}", e),
            }
        }
    }

    /// A cluster network asked to be deleted goes once OVN no longer has it.
    fn finish_cluster_network_deletes(&self) {
        for n in self.networks.list().unwrap_or_default() {
            if n.scope == NetworkScope::Cluster && n.deletion_requested_at.is_some() {
                let id = n.id.clone();
                let _ = self
                    .store
                    .database()
                    .write(crate::store::Origin::Network, |tx| {
                        crate::ipam::free_subnet(tx, &id)
                    });
                let _ = self.networks.remove(&id);
                tracing::info!(network = %n.name, id = %n.id, "cluster network deleted");
            }
        }
    }

    /// A reservation lives as long as the NIC it was made for (the `vm.ipam`
    /// finalizer of the plan, done as garbage collection): free any whose VM
    /// or NIC is gone.
    async fn gc_reservations(&self) {
        let Ok(res) = crate::ipam::list_reservations(&self.store.database()) else {
            return;
        };
        let vms = self.vms.read().await;
        let stale: Vec<_> = res
            .into_iter()
            .filter(|r| {
                !vms.get(&r.vm_id).is_some_and(|vm: &Vm| {
                    vm.config().networks.iter().enumerate().any(|(i, a)| {
                        a.network == r.network
                            && a.mac
                                .clone()
                                .or_else(|| glidex_ovs::names::mac_address(&vm.id, i as u8).ok())
                                .is_some_and(|m| m.eq_ignore_ascii_case(&r.mac))
                    })
                })
            })
            .collect();
        drop(vms);
        if stale.is_empty() {
            return;
        }
        let _ = self.store.database().write(
            crate::store::Origin::Network,
            |tx| -> Result<(), crate::ipam::IpamError> {
                for r in &stale {
                    crate::ipam::release(tx, &r.network, &r.mac)?;
                }
                Ok(())
            },
        );
    }

    /// Every node: make this host an OVN chassis, and a server also run its
    /// part of `ovn-central`. Idempotent; called every half minute.
    pub async fn ensure_ovn_node(&self) {
        if !self.ovn_enabled() {
            self.leave_ovn_if_turned_off().await;
            return;
        }
        let Some(cluster) = self.cluster() else {
            return;
        };
        let nodes = self.nodes().list().unwrap_or_default();
        let servers: Vec<(String, IpAddr, chrono_like::Stamp)> = nodes
            .iter()
            .filter(|n| {
                n.spec.role == crate::node::NodeRole::Server && !n.status.phase.is_tombstone()
            })
            .filter_map(|n| {
                Some((
                    n.meta.id.clone(),
                    n.status.advertise?.ip(),
                    chrono_like::Stamp(n.meta.created_at),
                ))
            })
            .collect();
        let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap_or_default();
        let certs = glidex_ovs::ovn::ChassisCerts {
            key_pem: read(cluster.files.key()),
            cert_pem: read(cluster.files.cert()),
            ca_pem: read(cluster.files.trust()),
        };
        if certs.key_pem.is_empty() || servers.is_empty() {
            return;
        }
        let me = self.local_node_id();
        let netd = self.netd.clone();
        let tunnel = nodes
            .iter()
            .find(|n| n.meta.id == me)
            .and_then(|n| n.status.tunnel_ip)
            .unwrap_or(cluster.identity.advertise.ip());
        // The oldest server makes OVN's databases; the others join it.
        let mut by_age = servers.clone();
        by_age.sort_by_key(|(_, _, t)| t.0);
        let first = by_age.first().map(|(id, ip, _)| (id.clone(), *ip));
        let is_server = cluster.role() == crate::node::NodeRole::Server;
        // The databases listen on this server's advertised address (the one the others know).
        let own_ip = cluster.identity.advertise.ip();
        let all_ips: Vec<IpAddr> = servers.iter().map(|(_, ip, _)| *ip).collect();
        let sb_remotes: Vec<String> = all_ips
            .iter()
            .map(|ip| format!("ssl:{ip}:{}", glidex_ovs::ovn::SB_PORT))
            .collect();
        let bridge_mappings = nodes
            .iter()
            .find(|n| n.meta.id == me)
            .map(|n| n.status.features.physnets.clone())
            .unwrap_or_default();
        let setting = nodes
            .iter()
            .find(|n| n.meta.id == me)
            .map(|n| n.spec.br_int_datapath)
            .unwrap_or_default();
        let res = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
            use glidex_netd::proto::{EnsureOvnCentralArgs, EnsureOvnChassisArgs, Op};
            if is_server {
                let join = first.as_ref().filter(|(id, _)| *id != me).map(|(_, ip)| *ip);
                let local = cluster_local_ip(&all_ips, own_ip);
                let spec = glidex_ovs::ovn::CentralSpec { local_ip: local, join, servers: all_ips.clone() };
                netd.call::<serde_json::Value>(Op::EnsureOvnCentral(EnsureOvnCentralArgs { spec, certs: certs.clone() })).map_err(|e| e.to_string())?;
            }
            // `br-int`'s datapath: the node's setting; `auto` is userspace
            // where DPDK runs (vhost-user needs it), else the kernel's.
            let dpdk = matches!(netd.probe(), (_, Ok(caps)) if caps.get("dpdk_initialized").and_then(|v| v.as_bool()).unwrap_or(false));
            let datapath = match setting {
                crate::node::BrIntDatapath::System => glidex_ovs::bridge::Datapath::System,
                crate::node::BrIntDatapath::Netdev => glidex_ovs::bridge::Datapath::Netdev,
                crate::node::BrIntDatapath::Auto if dpdk => glidex_ovs::bridge::Datapath::Netdev,
                crate::node::BrIntDatapath::Auto => glidex_ovs::bridge::Datapath::System,
            };
            let spec = glidex_ovs::ovn::ChassisSpec { chassis: glidex_ovs::ovn::chassis_name(&me), sb_remotes, encap_ip: tunnel, bridge_mappings, datapath };
            let st: glidex_ovs::ovn::OvnStatus = netd.call(Op::EnsureOvnChassis(EnsureOvnChassisArgs { spec, certs })).map_err(|e| e.to_string())?;
            Ok(st.datapath)
        })
        .await;
        match res {
            Ok(Ok(datapath)) => {
                // The scheduler needs to know where vhost-user NICs can go (§9.1).
                let me = self.local_node_id();
                if let Ok(Some(mut n)) = self.nodes().get(&me)
                    && n.status.features.br_int_datapath != datapath
                {
                    n.status.features.br_int_datapath = datapath;
                    n.meta.resource_version += 1;
                    let _ = self.nodes().put(&n);
                }
            }
            // A node that can't become a chassis can't run VMs on cluster networks.
            Ok(Err(e)) => tracing::warn!("making this node an OVN chassis: {}", e),
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
        all.first()
            .copied()
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
    }
}

mod chrono_like {
    /// A creation time, ordered.
    #[derive(Debug, Clone, Copy)]
    pub struct Stamp(pub u64);
}

impl VmManager {
    /// The cluster turned OVN off (spec/16-clustering-ui.md §8a): stop being a
    /// chassis. Only when the cluster setting says so, so a host that never
    /// used OVN is left alone; netd refuses while a VM port is on `br-int`.
    async fn leave_ovn_if_turned_off(&self) {
        if self.cluster().is_none() || !self.ovn_settings().is_some_and(|s| !s.enabled) {
            return;
        }
        let netd = self.netd.clone();
        let r = tokio::task::spawn_blocking(move || -> Result<bool, String> {
            let st: glidex_ovs::ovn::OvnStatus = netd
                .call(glidex_netd::proto::Op::OvnStatus)
                .map_err(|e| e.to_string())?;
            if st.chassis.is_none() && !st.controller_running {
                return Ok(false);
            }
            netd.call::<serde_json::Value>(glidex_netd::proto::Op::LeaveOvn { confirm: true })
                .map_err(|e| e.to_string())?;
            Ok(true)
        })
        .await;
        match r {
            Ok(Ok(true)) => tracing::info!("left OVN: the cluster turned it off"),
            Ok(Ok(false)) => {}
            Ok(Err(e)) => tracing::warn!("leaving OVN (the cluster turned it off): {}", e),
            Err(e) => tracing::warn!("leaving OVN: {}", e),
        }
    }
}

/// Read-only views of OVN for the web UI (spec/16-clustering-ui.md §8).
impl VmManager {
    /// `ssl:<ip>:<port>` of every server, for the NB (`6641`) or SB (`6642`) database.
    fn ovn_endpoints(&self, port: u16) -> Vec<String> {
        self.nodes()
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| {
                n.spec.role == crate::node::NodeRole::Server && !n.status.phase.is_tombstone()
            })
            .filter_map(|n| n.status.advertise.map(|a| format!("ssl:{}:{port}", a.ip())))
            .collect()
    }

    /// A chassis name (`node:<id>`) as `{chassis, node, node_name}`.
    fn chassis_json(&self, chassis: &str) -> serde_json::Value {
        let node = chassis.strip_prefix("node:").map(String::from);
        let name = node
            .as_deref()
            .and_then(|n| self.nodes().get(n).ok().flatten())
            .map(|n| n.spec.name);
        serde_json::json!({ "chassis": chassis, "node": node, "node_name": name })
    }

    /// `GET /cluster/ovn`: who serves the OVN databases and who leads, the
    /// chassis, and each router's gateway.
    pub async fn ovn_overview(&self) -> Result<serde_json::Value, crate::state::VmManagerError> {
        if !self.ovn_enabled() {
            return Ok(serde_json::json!({ "enabled": false }));
        }
        let Some(conn) = self.nb_conn() else {
            return Ok(serde_json::json!({ "enabled": true, "servers": 0 }));
        };
        let (nb, sb) = (
            self.ovn_endpoints(glidex_ovs::ovn::NB_PORT),
            self.ovn_endpoints(glidex_ovs::ovn::SB_PORT),
        );
        let (nb2, sb2) = (nb.clone(), sb.clone());
        let r = tokio::task::spawn_blocking(move || {
            let exec = SystemExec::new();
            let nb_members =
                glidex_ovn::view::members(&exec, glidex_ovs::exec::Program::OvnNbctl, &conn, &nb2);
            let sb_members =
                glidex_ovn::view::members(&exec, glidex_ovs::exec::Program::OvnSbctl, &conn, &sb2);
            let chassis = glidex_ovn::view::chassis(&exec, &conn, &sb2);
            let gateways = glidex_ovn::view::gateways(&exec, &conn, &nb2, &sb2);
            (nb_members, sb_members, chassis, gateways)
        })
        .await
        .map_err(|e| crate::state::VmManagerError::PersistenceError(e.to_string()))?;
        let (nb_members, sb_members, chassis, gateways) = r;
        let chassis: Vec<serde_json::Value> = chassis
            .unwrap_or_default()
            .into_iter()
            .map(|c| {
                let mut v = self.chassis_json(&c.name);
                v["hostname"] = serde_json::json!(c.hostname);
                v["encaps"] = serde_json::json!(c.encaps);
                v["nb_cfg_timestamp"] = serde_json::json!(c.nb_cfg_timestamp);
                v
            })
            .collect();
        let gateways: Vec<serde_json::Value> = gateways
            .unwrap_or_default()
            .into_iter()
            .map(|g| {
                serde_json::json!({
                    "router": g.router,
                    "active": g.active.as_deref().map(|c| self.chassis_json(c)),
                    "candidates": g.candidates.iter().map(|(c, p)| { let mut v = self.chassis_json(c); v["priority"] = serde_json::json!(p); v }).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(
            serde_json::json!({ "enabled": true, "nb": nb_members, "sb": sb_members, "chassis": chassis, "gateways": gateways }),
        )
    }

    /// `GET /networks/{id}/ovn`: a cluster network's switch, its ports and
    /// where they are bound, and its router.
    pub async fn network_ovn(
        &self,
        net: &crate::network::Network,
    ) -> Result<serde_json::Value, crate::state::VmManagerError> {
        if net.scope != NetworkScope::Cluster || !self.ovn_enabled() {
            return Err(crate::hypervisor::HypervisorError::InvalidConfig(format!(
                "{} is not an OVN network",
                net.name
            ))
            .into());
        }
        let Some(conn) = self.nb_conn() else {
            return Err(crate::hypervisor::HypervisorError::InvalidConfig(
                "no OVN server is known".into(),
            )
            .into());
        };
        let (nb, sb) = (
            self.ovn_endpoints(glidex_ovs::ovn::NB_PORT),
            self.ovn_endpoints(glidex_ovs::ovn::SB_PORT),
        );
        let switch = format!("gx-{}", net.ovn_key());
        let router = match (&net.mode, &net.router) {
            (NetworkMode::Nat, Some(r)) => Some(format!("gxr-{r}")),
            (NetworkMode::Nat, None) => Some(glidex_ovn::EDGE.to_string()),
            _ => None,
        };
        let (sw, rt) = (switch.clone(), router.clone());
        let (ports, nat, gws) = tokio::task::spawn_blocking(move || {
            let exec = SystemExec::new();
            let ports = glidex_ovn::view::switch_ports(&exec, &conn, &nb, &sb, &sw);
            let nat = rt
                .as_deref()
                .map(|r| glidex_ovn::view::router_nat(&exec, &conn, &nb, r));
            let gws = rt
                .as_ref()
                .map(|_| glidex_ovn::view::gateways(&exec, &conn, &nb, &sb));
            (ports, nat, gws)
        })
        .await
        .map_err(|e| crate::state::VmManagerError::PersistenceError(e.to_string()))?;
        let ports = ports.map_err(|e| {
            crate::state::VmManagerError::PersistenceError(format!("reading OVN: {e}"))
        })?;
        let vms = self.list_vms().await;
        let ports: Vec<serde_json::Value> = ports
            .into_iter()
            .map(|p| {
                // VM ports are `gx<first 8 of the VM id>-<nic>`.
                let vm = p
                    .name
                    .strip_prefix("gx")
                    .and_then(|r| r.split_once('-'))
                    .and_then(|(pre, _)| vms.iter().find(|v| v.id.starts_with(pre)));
                serde_json::json!({
                    "name": p.name, "kind": p.kind, "addresses": p.addresses, "up": p.up,
                    "bound": p.chassis.as_deref().map(|c| self.chassis_json(c)),
                    "vm": vm.map(|v| serde_json::json!({ "id": v.id, "name": v.name })),
                })
            })
            .collect();
        let gateway = gws
            .and_then(|g| g.ok())
            .and_then(|g| g.into_iter().find(|g| Some(&g.router) == router.as_ref()));
        Ok(serde_json::json!({
            "switch": switch,
            "ports": ports,
            "router": router.map(|r| serde_json::json!({
                "name": r,
                "nat": nat.and_then(|n| n.ok()).unwrap_or_default(),
                "active_gateway": gateway.as_ref().and_then(|g| g.active.as_deref()).map(|c| self.chassis_json(c)),
                "gateway_candidates": gateway.map(|g| g.candidates.iter().map(|(c, p)| { let mut v = self.chassis_json(c); v["priority"] = serde_json::json!(p); v }).collect::<Vec<_>>()).unwrap_or_default(),
            })),
        }))
    }
}

/// Tunnel status (spec/16-clustering-ui.md §8b): each node's Geneve tunnels as
/// its Open vSwitch sees them, gathered on request.
impl VmManager {
    /// This node's OVN chassis and tunnels, from its netd.
    pub async fn local_ovn_status(&self) -> Result<serde_json::Value, String> {
        let netd = self.netd.clone();
        tokio::task::spawn_blocking(move || {
            netd.call::<serde_json::Value>(glidex_netd::proto::Op::OvnStatus)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// `GET /cluster/ovn/tunnels`: every live node's tunnels, matched to the
    /// peer they lead to, the peers a node has no tunnel to, and the nodes
    /// that didn't answer.
    pub async fn ovn_tunnels(&self) -> serde_json::Value {
        use serde_json::json;
        if !self.ovn_enabled() {
            return json!({ "enabled": false });
        }
        let Some(cluster) = self.cluster() else {
            return json!({ "enabled": false });
        };
        let me = self.local_node_id();
        let nodes: Vec<crate::node::Node> = self
            .nodes()
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| !n.status.phase.is_tombstone())
            .collect();
        // A tunnel's remote_ip is the peer's tunnel address (its advertise address by default).
        let endpoint = |n: &crate::node::Node| {
            n.status
                .tunnel_ip
                .or(n.status.advertise.map(|a| a.ip()))
                .map(|ip| ip.to_string())
        };
        let asks = nodes.iter().map(|n| {
            let cluster = cluster.clone();
            let id = n.meta.id.clone();
            let addr = n.status.advertise.map(|a| a.to_string());
            let local = id == me;
            async move {
                let r: Result<serde_json::Value, String> = if local {
                    self.local_ovn_status().await
                } else {
                    match addr {
                        None => Err("no address".into()),
                        Some(a) => match cluster
                            .client
                            .request_timeout(
                                &a,
                                hyper::Method::GET,
                                "/cluster/v1/ovn-status",
                                &[],
                                bytes::Bytes::new(),
                                std::time::Duration::from_secs(4),
                            )
                            .await
                        {
                            Ok(r) if r.status.is_success() => {
                                serde_json::from_slice(&r.body).map_err(|e| e.to_string())
                            }
                            Ok(r) => Err(format!("answered {}", r.status)),
                            Err(e) => Err(e.to_string()),
                        },
                    }
                };
                (id, r)
            }
        });
        let answers = futures_util::future::join_all(asks).await;
        let out: Vec<serde_json::Value> = nodes
            .iter()
            .zip(answers)
            .map(|(n, (_, r))| {
                let base = json!({ "node": n.meta.id, "node_name": n.spec.name });
                let st = match r {
                    Ok(v) => v,
                    Err(e) => return json!({ "node": n.meta.id, "node_name": n.spec.name, "answered": false, "error": e }),
                };
                let tunnels: Vec<serde_json::Value> = st["tunnels"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| {
                        let ip = t["remote_ip"].as_str().unwrap_or("").to_string();
                        let peer = nodes.iter().find(|p| p.meta.id != n.meta.id && endpoint(p).as_deref() == Some(ip.as_str()));
                        let state = if t["error"].is_string() {
                            "error"
                        } else if t["bfd_state"].as_str().is_some_and(|s| s != "up") {
                            "bfd_down"
                        } else {
                            "present"
                        };
                        let mut v = t.clone();
                        v["state"] = json!(state);
                        v["peer"] = json!(peer.map(|p| json!({ "node": p.meta.id, "node_name": p.spec.name })));
                        v
                    })
                    .collect();
                let missing: Vec<serde_json::Value> = nodes
                    .iter()
                    .filter(|p| p.meta.id != n.meta.id)
                    .filter(|p| !tunnels.iter().any(|t| t["peer"]["node"] == p.meta.id.as_str()))
                    .map(|p| json!({ "node": p.meta.id, "node_name": p.spec.name }))
                    .collect();
                let mut v = base;
                v["answered"] = json!(true);
                v["chassis"] = st["chassis"].clone();
                v["controller_running"] = st["controller_running"].clone();
                v["tunnels"] = json!(tunnels);
                v["missing"] = json!(missing);
                v
            })
            .collect();
        json!({ "enabled": true, "nodes": out })
    }
}

/// The glidex chassis (`node:<id>`) whose node is a tombstone (removed,
/// forgotten, departed), with the node's name. A chassis of a node the
/// cluster doesn't know is left alone (one joining, say).
pub(crate) fn stale_chassis(
    chassis: &[String],
    nodes: &[crate::node::Node],
) -> Vec<(String, String)> {
    chassis
        .iter()
        .filter_map(|c| {
            let id = c.strip_prefix("node:")?;
            let n = nodes.iter().find(|n| n.meta.id == id)?;
            n.status
                .phase
                .is_tombstone()
                .then(|| (c.clone(), n.spec.name.clone()))
        })
        .collect()
}

#[cfg(test)]
mod stale_chassis_tests {
    use super::stale_chassis;
    use crate::node::{Node, NodePhase, NodeRole, NodeSpec};

    fn node(id: &str, name: &str, phase: NodePhase) -> Node {
        let mut n = Node::new(
            id,
            NodeSpec {
                name: name.into(),
                role: NodeRole::Agent,
                unschedulable: false,
                labels: Default::default(),
                br_int_datapath: Default::default(),
                bridge_mappings: Default::default(),
                drop_spare_files_before: None,
                cordoned_devices: Default::default(),
            },
        );
        n.status.phase = phase;
        n
    }

    #[test]
    fn only_chassis_of_nodes_that_left_go() {
        let nodes = [
            node("a", "live", NodePhase::Active),
            node("b", "removed", NodePhase::Removed),
            node("c", "forgotten", NodePhase::Forgotten),
            node("d", "draining", NodePhase::Draining),
        ];
        let chassis = [
            "node:a",
            "node:b",
            "node:c",
            "node:d",
            "node:unknown",
            "not-glidex",
        ]
        .map(String::from);
        assert_eq!(
            stale_chassis(&chassis, &nodes),
            vec![
                ("node:b".to_string(), "removed".to_string()),
                ("node:c".to_string(), "forgotten".to_string())
            ]
        );
    }
}
