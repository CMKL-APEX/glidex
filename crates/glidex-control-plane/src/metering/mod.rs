//! Resource usage metering (spec/metering.md).
//!
//! `ledger` turns counter and gauge readings into exactly-once hourly
//! usage rows; `sources` reads the host; `sampler` maps VMs and disks to
//! meters. [`Meter`] runs the rounds.

pub mod ledger;
pub mod net;
pub mod sampler;
pub mod sources;

pub use ledger::{Delta, Flag, Ledger, LedgerSettings, MeteringError, Origin, Round, Subject, SubjectKind, UsageRecord};

use crate::config::MeteringConfig;
use crate::images::Disk;
use crate::models::Vm;
use crate::network::Network;
use glidex_ovs::nat_meter::NatCounter;
use glidex_ovs::stats::BridgeStats;
use redb::Database;
use std::sync::Arc;
use std::time::Duration;

/// The meter: one ledger, sampled every `sample_secs`.
pub struct Meter {
    ledger: Ledger,
    cfg: MeteringConfig,
    host: sources::Host,
    /// This boot's id: NAT counter handles restart after a reboot.
    boot_id: String,
    /// One round at a time: a round and a final sample (M1.3) must not
    /// commit over each other's cursors (D8).
    round_lock: std::sync::Mutex<()>,
}

impl Meter {
    pub fn new(db: Arc<Database>, cfg: MeteringConfig) -> Result<Self, MeteringError> {
        let ledger = Ledger::new(db, LedgerSettings::from_secs(cfg.sample_secs, cfg.close_grace_secs))?;
        let boot_id = glidex_vm_shim::util::boot_id().unwrap_or_default();
        Ok(Self { ledger, cfg, host: sources::Host::default(), boot_id, round_lock: std::sync::Mutex::new(()) })
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn config(&self) -> &MeteringConfig {
        &self.cfg
    }

    /// One sampling round over a snapshot (blocking: sysfs, `/proc` and
    /// the database).
    pub fn sample(&self, snap: &Snapshot, now: u64) -> Result<(), MeteringError> {
        let _one = self.round_lock.lock().unwrap_or_else(|e| e.into_inner());
        // Without port counters (netd unreachable), NIC and network
        // cursors are kept: the next round's deltas cover the gap.
        let kinds: &[SubjectKind] = match snap.bridges {
            Some(_) => &[SubjectKind::Vm, SubjectKind::Disk, SubjectKind::Nic, SubjectKind::Network],
            None => &[SubjectKind::Vm, SubjectKind::Disk],
        };
        let with_cursors = self.ledger.cursor_subjects(kinds)?;
        let mut round = self.ledger.begin_round(now)?;
        sampler::sample_vms(&mut round, &snap.vms, &self.host, now)?;
        sampler::sample_disks(&mut round, &snap.disks, now)?;
        if let Some(bridges) = &snap.bridges {
            net::sample_ports(&mut round, bridges, &snap.vms, &snap.networks, now)?;
        }
        if let Some(nat) = &snap.nat {
            net::sample_nat(&mut round, nat, &snap.vms, &snap.networks, &self.boot_id, now)?;
        }
        sampler::forget_gone(&mut round, &with_cursors, &snap.live_subjects());
        self.ledger.commit(round)
    }
}

/// What one round reads.
pub struct Snapshot {
    pub vms: Vec<Vm>,
    pub disks: Vec<Disk>,
    pub networks: Vec<Network>,
    /// Port counters from netd; `None` when it could not be asked.
    pub bridges: Option<Vec<BridgeStats>>,
    /// NAT external-traffic counters from netd (§5.5).
    pub nat: Option<Vec<NatCounter>>,
}

impl Snapshot {
    /// `kind/id` of every subject that still exists.
    fn live_subjects(&self) -> std::collections::BTreeSet<String> {
        let mut live: std::collections::BTreeSet<String> = self.vms.iter().map(|v| format!("vm/{}", v.id)).collect();
        live.extend(self.disks.iter().map(|d| format!("disk/{}", d.id)));
        for v in &self.vms {
            live.extend(v.status.nics.iter().map(|n| format!("nic/{}.{}", v.id, n.nic_index)));
        }
        live.extend(self.networks.iter().map(|n| format!("network/{}", n.name)));
        if let Some(bridges) = &self.bridges {
            live.extend(bridges.iter().map(|b| format!("network/bridge:{}", b.bridge)));
        }
        live
    }
}

/// Unix milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// How long until the next multiple of `period_ms` (rounds line up with
/// the 5-minute slots, §8.5.1).
pub fn until_next(now: u64, period_ms: u64) -> Duration {
    Duration::from_millis(period_ms - now % period_ms)
}

impl crate::state::VmManager {
    /// Open the meter and, when enabled, start its sampling loop with the
    /// controllers' tasks. Idempotent; call after `start_controllers`.
    pub fn start_metering(&self, cfg: &MeteringConfig) -> Result<(), MeteringError> {
        if self.meter.get().is_some() {
            return Ok(());
        }
        let meter = Arc::new(Meter::new(self.store.database(), cfg.clone())?);
        let _ = self.meter.set(meter.clone());
        if !cfg.enabled {
            return Ok(());
        }
        meter.ledger.started_at()?;
        let me = self.arc();
        let task = tokio::spawn(async move {
            let period = meter.cfg.sample_secs.max(1) * 1000;
            loop {
                tokio::time::sleep(until_next(now_ms(), period)).await;
                let started = std::time::Instant::now();
                if let Err(e) = me.meter_round(&meter).await {
                    tracing::warn!("metering round failed: {}", e);
                }
                let took = started.elapsed();
                if took > Duration::from_millis(period / 2) {
                    tracing::warn!(?took, "metering round over its budget (sample_secs / 2)");
                }
            }
        });
        self.tasks.lock().unwrap().push(task);
        Ok(())
    }

    /// Snapshot everything and run one round (port counters from netd).
    async fn meter_round(&self, meter: &Arc<Meter>) -> Result<(), MeteringError> {
        let vms: Vec<Vm> = self.vms.read().await.values().cloned().collect();
        let disks = self.images.list_disks();
        let networks = self.networks.list().unwrap_or_default();
        let netd = self.netd.clone();
        let m = meter.clone();
        tokio::task::spawn_blocking(move || {
            let bridges = match netd.call::<Vec<BridgeStats>>(glidex_netd::proto::Op::PortStats) {
                Ok(b) => Some(b),
                Err(e) => {
                    tracing::debug!("metering: no port counters this round: {}", e);
                    None
                }
            };
            // Only with port counters: NIC and network cursors are kept
            // or forgotten together (see `sample`).
            let nat = bridges.as_ref().and_then(|_| netd.call::<Vec<NatCounter>>(glidex_netd::proto::Op::NatCounters).ok());
            m.sample(&Snapshot { vms, disks, networks, bridges, nat }, now_ms())
        })
        .await
        .map_err(|e| MeteringError::Storage(format!("metering round panicked: {e}")))?
    }

    /// A final sample before ports or bridges go away (detach, release,
    /// network deletion, uplink changes; spec/metering.md §5.4): bounded
    /// at 2 s, and never an error. Missing it costs at most one interval.
    pub async fn meter_final_sample(&self) {
        let Some(meter) = self.meter().filter(|m| m.cfg.enabled) else { return };
        match tokio::time::timeout(Duration::from_secs(2), self.meter_round(&meter)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("metering: final sample failed: {}", e),
            Err(_) => tracing::warn!("metering: final sample timed out"),
        }
    }

    /// The meter, once [`Self::start_metering`] ran.
    pub fn meter(&self) -> Option<Arc<Meter>> {
        self.meter.get().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_align_to_the_period() {
        assert_eq!(until_next(1_000_000, 30_000), Duration::from_millis(20_000));
        assert_eq!(until_next(1_020_000, 30_000), Duration::from_millis(30_000));
    }
}
