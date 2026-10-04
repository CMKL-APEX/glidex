//! Resource usage metering (spec/metering.md).
//!
//! `ledger` turns counter and gauge readings into exactly-once hourly
//! usage rows; `sources` reads the host; `sampler` maps VMs and disks to
//! meters. [`Meter`] runs the rounds.

pub mod ledger;
pub mod sampler;
pub mod sources;

pub use ledger::{Delta, Flag, Ledger, LedgerSettings, MeteringError, Origin, Round, Subject, SubjectKind, UsageRecord};

use crate::config::MeteringConfig;
use crate::images::Disk;
use crate::models::Vm;
use redb::Database;
use std::sync::Arc;
use std::time::Duration;

/// The meter: one ledger, sampled every `sample_secs`.
pub struct Meter {
    ledger: Ledger,
    cfg: MeteringConfig,
    host: sources::Host,
    /// One round at a time: a round and a final sample (M1.3) must not
    /// commit over each other's cursors (D8).
    round_lock: std::sync::Mutex<()>,
}

impl Meter {
    pub fn new(db: Arc<Database>, cfg: MeteringConfig) -> Result<Self, MeteringError> {
        let ledger = Ledger::new(db, LedgerSettings::from_secs(cfg.sample_secs, cfg.close_grace_secs))?;
        Ok(Self { ledger, cfg, host: sources::Host::default(), round_lock: std::sync::Mutex::new(()) })
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn config(&self) -> &MeteringConfig {
        &self.cfg
    }

    /// One sampling round over snapshots of the VMs and disks (blocking:
    /// sysfs, `/proc` and the database).
    pub fn sample(&self, vms: &[Vm], disks: &[Disk], now: u64) -> Result<(), MeteringError> {
        let _one = self.round_lock.lock().unwrap_or_else(|e| e.into_inner());
        let with_cursors = self.ledger.cursor_subjects(&[SubjectKind::Vm, SubjectKind::Disk])?;
        let mut round = self.ledger.begin_round(now)?;
        sampler::sample_vms(&mut round, vms, &self.host, now)?;
        sampler::sample_disks(&mut round, disks, now)?;
        sampler::forget_gone(&mut round, &with_cursors, vms, disks);
        self.ledger.commit(round)
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
                let vms: Vec<Vm> = me.vms.read().await.values().cloned().collect();
                let disks = me.images.list_disks();
                let m = meter.clone();
                let started = std::time::Instant::now();
                match tokio::task::spawn_blocking(move || m.sample(&vms, &disks, now_ms())).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!("metering round failed: {}", e),
                    Err(e) => tracing::warn!("metering round panicked: {}", e),
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
