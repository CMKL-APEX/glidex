//! The network controller (spec/reconciliation.md §10.3): keeps netd's
//! records in line with the control plane's networks. netd reconciles the
//! host (OVS, nftables, dnsmasq) from its own records; this re-creates a
//! bridge netd lost, and reports what is missing. A lost NAT is reported,
//! not re-created: its subnet lives only in netd, and a new one would
//! renumber every VM on it.

use crate::controller::vm::set_cond;
use crate::models::Tristate;
use crate::network::{NetError, NetworkMode, NetworkPhase};
use crate::state::{VmManager, VmManagerError};
use crate::store::{event_key, Event, EventKind};
use glidex_netd::proto::{BridgeRecord, NatInfo, Op};
use glidex_ovs::bridge::{BridgeSpec, Datapath};
use glidex_ovs::vm_port::VmPortKind;
use std::time::Duration;

type Next = Result<Option<Duration>, VmManagerError>;

impl VmManager {
    /// One round for network `name`.
    pub async fn reconcile_network(&self, name: &str) -> Next {
        let Some(mut net) = self.networks.get(name)? else { return Ok(None) };
        let netd = self.netd.clone();
        let bridges = tokio::task::spawn_blocking(move || netd.call::<Vec<BridgeRecord>>(Op::ListBridges))
            .await
            .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?;
        let bridges = match bridges {
            Ok(b) => b,
            Err(NetError::Unavailable(m)) => {
                return self.set_network_status(net, NetworkPhase::NetdUnavailable, Tristate::Unknown, "NetdUnavailable", m, Duration::from_secs(30));
            }
            Err(e) => return Err(e.into()),
        };
        let record = bridges.iter().find(|b| b.spec.name == net.bridge);
        let mut problems: Vec<String> = Vec::new();
        match (net.mode, record) {
            (NetworkMode::Nat | NetworkMode::Isolated, rec) if net.owns_bridge && rec.is_none_or(|r| r.live.is_none()) => {
                // netd lost the bridge (record or on the host): ensure it.
                let spec = BridgeSpec {
                    name: net.bridge.clone(),
                    datapath: match net.port_type {
                        VmPortKind::VhostUser => Datapath::Netdev,
                        VmPortKind::Tap => Datapath::System,
                    },
                    mtu: net.mtu,
                    adopt: false,
                };
                let netd = self.netd.clone();
                match tokio::task::spawn_blocking(move || netd.call::<BridgeRecord>(Op::EnsureBridge(spec)))
                    .await
                    .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?
                {
                    Ok(_) => {
                        let _ = self.store.push_event(
                            &event_key("network", &net.name),
                            Event::new("controller", EventKind::Warning, "BridgeRestored", format!("re-created bridge {}", net.bridge)),
                        );
                    }
                    Err(e) => problems.push(format!("bridge {}: {}", net.bridge, e)),
                }
            }
            (NetworkMode::Bridged, None) => problems.push(format!("bridge {} is not a glidex bridge any more", net.bridge)),
            (_, Some(r)) if r.live.is_none() => problems.push(format!("bridge {} is missing on the host", net.bridge)),
            _ => {}
        }
        if net.mode == NetworkMode::Nat {
            let netd = self.netd.clone();
            match tokio::task::spawn_blocking(move || netd.call::<Vec<NatInfo>>(Op::ListNat))
                .await
                .map_err(|e| VmManagerError::PersistenceError(e.to_string()))?
            {
                Ok(nats) => match nats.iter().find(|n| n.state.bridge == net.bridge) {
                    None => problems.push("netd has no NAT for it".into()),
                    Some(n) if n.state.dns && !n.dnsmasq_running => problems.push("its DHCP/DNS server (dnsmasq) is not running".into()),
                    Some(_) => {}
                },
                Err(e) => problems.push(format!("NAT: {}", e)),
            }
        }
        if problems.is_empty() {
            self.set_network_status(net, NetworkPhase::Ready, Tristate::True, "Converged", String::new(), Duration::ZERO)
        } else {
            let msg = problems.join("; ");
            if net.phase != NetworkPhase::Degraded {
                let _ = self.store.push_event(&event_key("network", &net.name), Event::new("controller", EventKind::Warning, "Degraded", msg.clone()));
            }
            net.phase = NetworkPhase::Degraded;
            self.set_network_status(net, NetworkPhase::Degraded, Tristate::False, "Degraded", msg, Duration::from_secs(30))
        }
    }

    fn set_network_status(
        &self,
        mut net: crate::network::Network,
        phase: NetworkPhase,
        ready: Tristate,
        reason: &str,
        message: String,
        again: Duration,
    ) -> Next {
        let before = (net.phase, net.conditions.clone());
        net.phase = phase;
        set_cond(&mut net.conditions, "Ready", ready, reason, message);
        if before != (net.phase, net.conditions.clone()) {
            // Keep grants and shares written meanwhile: only the status
            // fields come from this round.
            if let Some(mut current) = self.networks.get(&net.name)? {
                current.phase = net.phase;
                current.conditions = net.conditions;
                self.networks.put(&current)?;
            }
        }
        Ok((!again.is_zero()).then_some(again))
    }
}
