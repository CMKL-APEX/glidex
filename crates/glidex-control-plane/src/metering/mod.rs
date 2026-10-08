//! Resource usage metering (spec/metering.md).
//!
//! `ledger` turns counter and gauge readings into exactly-once hourly
//! usage rows; `sources` reads the host; `sampler` maps VMs and disks to
//! meters. [`Meter`] runs the rounds.

#[cfg(test)]
mod bench;
pub mod ledger;
pub mod net;
pub mod query;
pub mod rates;
pub mod retention;
pub mod sampler;
pub mod sources;
pub mod storage;

pub use ledger::{Delta, Flag, Ledger, LedgerSettings, MeteringError, Origin, Round, Subject, SubjectKind, UsageRecord};

use crate::config::MeteringConfig;
use crate::images::Disk;
use crate::models::Vm;
use crate::network::Network;
use glidex_ovs::nat_meter::NatCounter;
use glidex_ovs::stats::BridgeStats;
use std::sync::Arc;
use std::time::Duration;

/// The meter: one ledger, sampled every `sample_secs`.
pub struct Meter {
    /// What the API reads: the cluster-wide ledger (the replicated store in a
    /// cluster, the only database otherwise).
    ledger: Ledger,
    /// In a cluster, where this node's own rounds are written, before they are
    /// shipped to the leader (spec/clustering.md §13.1).
    local: Option<Ledger>,
    cfg: MeteringConfig,
    host: sources::Host,
    /// This boot's id: NAT counter handles restart after a reboot.
    boot_id: String,
    /// One round at a time: a round and a final sample (M1.3) must not
    /// commit over each other's cursors (D8).
    round_lock: std::sync::Mutex<()>,
    /// The latest live rates per subject, and when (unix ms) (§9.4).
    live: std::sync::Mutex<std::collections::HashMap<String, LiveStats>>,
}

/// Rates of one subject from its last two samples (§9.4).
#[derive(Debug, Clone, serde::Serialize)]
pub struct LiveStats {
    pub subject: Subject,
    pub values: std::collections::BTreeMap<String, f64>,
    pub sampled_at: u64,
}

impl Meter {
    pub fn new(db: Arc<crate::store::Db>, cfg: MeteringConfig) -> Result<Self, MeteringError> {
        let ledger = Ledger::new(db, LedgerSettings::from_secs(cfg.sample_secs, cfg.close_grace_secs))?;
        let boot_id = glidex_vm_shim::util::boot_id().unwrap_or_default();
        Ok(Self {
            ledger,
            local: None,
            cfg,
            host: sources::Host::default(),
            boot_id,
            round_lock: std::sync::Mutex::new(()),
            live: Default::default(),
        })
    }

    /// A node of a cluster: sample into `local`, ship to the leader, read
    /// the cluster's ledger from `central`.
    pub fn new_clustered(central: Arc<crate::store::Db>, local: Arc<crate::store::Db>, cfg: MeteringConfig) -> Result<Self, MeteringError> {
        let mut m = Self::new(central, cfg.clone())?;
        m.local = Some(Ledger::new(local, LedgerSettings::from_secs(cfg.sample_secs, cfg.close_grace_secs))?);
        Ok(m)
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Where rounds are written.
    pub fn writer(&self) -> &Ledger {
        self.local.as_ref().unwrap_or(&self.ledger)
    }

    pub fn local_ledger(&self) -> Option<&Ledger> {
        self.local.as_ref()
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
        let with_cursors = self.writer().cursor_subjects(kinds)?;
        let mut round = self.writer().begin_round(now)?;
        sampler::sample_vms(&mut round, &snap.vms, &snap.disks, &self.host, now)?;
        sampler::sample_disks(&mut round, &snap.disks, now)?;
        if let Some(bridges) = &snap.bridges {
            net::sample_ports(&mut round, bridges, &snap.vms, &snap.networks, now)?;
        }
        if let Some(nat) = &snap.nat {
            net::sample_nat(&mut round, nat, &snap.vms, &snap.networks, &self.boot_id, now)?;
        }
        sampler::forget_gone(&mut round, &with_cursors, &snap.live_subjects());
        let live = round.take_live();
        self.writer().commit(round)?;
        let mut map = self.live.lock().unwrap_or_else(|e| e.into_inner());
        // Subjects with no fresh rate this round (stopped, gone) drop out.
        map.retain(|k, v| live.contains_key(k) || now.saturating_sub(v.sampled_at) < 2 * self.cfg.sample_secs * 1000);
        for (k, (subject, values)) in live {
            map.insert(k, LiveStats { subject, values, sampled_at: now });
        }
        Ok(())
    }

    /// A storage pass (§5.3): `disk.stored` from measured file sizes and
    /// `image.stored` (blocking: the database).
    pub fn sample_storage(&self, disks: &[(Disk, u64)], images: &[crate::images::Image], now: u64) -> Result<(), MeteringError> {
        let _one = self.round_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut round = self.writer().begin_round(now)?;
        storage::record(&mut round, disks, images, now)?;
        // Images that are gone: their cursors go with them.
        let live: std::collections::BTreeSet<String> = images.iter().map(|i| format!("image/{}", i.id)).collect();
        for key in self.writer().cursor_subjects(&[SubjectKind::Image])?.difference(&live) {
            round.forget_subject(key);
        }
        self.writer().commit(round)
    }

    /// Live rates of the subjects `keep` selects.
    pub fn live_stats(&self, keep: impl Fn(&Subject) -> bool) -> Vec<LiveStats> {
        let map = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<LiveStats> = map.values().filter(|l| keep(&l.subject)).cloned().collect();
        v.sort_by(|a, b| (a.subject.kind, &a.subject.id).cmp(&(b.subject.kind, &b.subject.id)));
        v
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
    /// Open the meter and, when enabled, start its sampling loop. Idempotent;
    /// call after `start_controllers`.
    pub fn start_metering(&self, cfg: &MeteringConfig) -> Result<(), MeteringError> {
        if self.meter.load().is_some() {
            return Ok(());
        }
        *self.metering_cfg.lock().unwrap() = Some(cfg.clone());
        self.spawn_meter(cfg)
    }

    /// Build the meter anew: after this host joined or formed a cluster, its
    /// rounds go to a node-local ledger that is shipped (§13.1).
    pub(crate) fn restart_metering(&self) {
        let Some(cfg) = self.metering_cfg.lock().unwrap().clone() else { return };
        for t in std::mem::take(&mut *self.meter_tasks.lock().unwrap()) {
            t.abort();
        }
        self.meter.store(None);
        if let Err(e) = self.spawn_meter(&cfg) {
            tracing::warn!("metering not restarted: {}", e);
        }
    }

    fn build_meter(&self, cfg: &MeteringConfig) -> Result<Meter, MeteringError> {
        let central = self.store.database();
        if self.cluster().is_none() {
            return Meter::new(central, cfg.clone());
        }
        let local = Arc::new(crate::store::Db::create(self.data_dir.join("meter.db")).map_err(|e| MeteringError::Storage(e.to_string()))?);
        let meter = Meter::new_clustered(central.clone(), local.clone(), cfg.clone())?;
        // A host that metered before it joined brings its cursors along, so
        // nothing is counted twice or lost (D8).
        if let Some(l) = meter.local_ledger() {
            if l.is_empty_of_state()? && central.can_write() {
                let old = meter.ledger().node_state()?;
                if !old.cursors.is_empty() || !old.open.is_empty() {
                    l.put_node_state(&old)?;
                    meter.ledger().clear_node_state(&old)?;
                }
            }
        }
        Ok(meter)
    }

    fn spawn_meter(&self, cfg: &MeteringConfig) -> Result<(), MeteringError> {
        let meter = Arc::new(self.build_meter(cfg)?);
        self.meter.store(Some(meter.clone()));
        if !cfg.enabled {
            return Ok(());
        }
        meter.writer().started_at()?;
        let me = self.arc();
        let task = tokio::spawn(async move {
            let period = meter.cfg.sample_secs.max(1) * 1000;
            loop {
                tokio::time::sleep(until_next(now_ms(), period)).await;
                let started = std::time::Instant::now();
                if let Err(e) = me.meter_round(&meter).await {
                    tracing::warn!("metering round failed: {}", e);
                }
                // Daily upkeep of the cluster-wide ledger, where it can be written.
                if meter.ledger.can_write() {
                    let m = meter.clone();
                    let upkeep = tokio::task::spawn_blocking(move || {
                        let tz = query::parse_tz(&m.cfg.billing_timezone).unwrap_or_else(|_| query::Tz::utc());
                        retention::run(&m.ledger, &m.cfg, &tz, now_ms() / 1000)
                    });
                    if let Ok(Err(e)) = upkeep.await {
                        tracing::warn!("metering upkeep failed: {}", e);
                    }
                }
                let took = started.elapsed();
                if took > Duration::from_millis(period / 2) {
                    tracing::warn!(?took, "metering round over its budget (sample_secs / 2)");
                }
            }
        });
        self.meter_tasks.lock().unwrap().push(task);
        // Stored sizes, every storage_secs: `qemu-img info` opens the files.
        let me = self.arc();
        let meter = self.meter.load_full().expect("stored above");
        let for_storage = meter.clone();
        let storage = tokio::spawn(async move {
            let meter = for_storage;
            let period = meter.cfg.storage_secs.max(meter.cfg.sample_secs) * 1000;
            loop {
                tokio::time::sleep(until_next(now_ms(), period)).await;
                let disks: Vec<Disk> = me.images.list_disks().into_iter().filter(|d| storage::measurable(d) && d.node.as_deref().is_none_or(|n| n == me.local_node_id())).collect();
                let images = if meter.ledger.can_write() { me.images.list_images() } else { Vec::new() };
                let paths: Vec<_> = disks.iter().map(|d| me.images.disk_path(d)).collect();
                let m = meter.clone();
                let done = tokio::task::spawn_blocking(move || {
                    let measured: Vec<(Disk, u64)> = disks
                        .into_iter()
                        .zip(paths)
                        .filter_map(|(d, p)| {
                            let info = crate::images::qemu_img::info(&p, Some(d.format)).ok()?;
                            Some((d, info.actual_size))
                        })
                        .collect();
                    m.sample_storage(&measured, &images, now_ms())
                })
                .await;
                if let Ok(Err(e)) = done {
                    tracing::warn!("metering storage pass failed: {}", e);
                }
            }
        });
        self.meter_tasks.lock().unwrap().push(storage);
        if meter.local.is_some() {
            let me = self.arc();
            let m = meter.clone();
            let ship = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    if let Err(e) = me.ship_ledger(&m).await {
                        tracing::warn!("shipping usage to the leader: {}", e);
                    }
                }
            });
            self.meter_tasks.lock().unwrap().push(ship);
        }
        Ok(())
    }

    /// Send this node's closed hours to the leader; once it has them, forget
    /// them locally (§13.1: at-least-once shipping, idempotent merge).
    pub async fn ship_ledger(&self, meter: &Arc<Meter>) -> Result<(), String> {
        let Some(local) = meter.local_ledger() else { return Ok(()) };
        let export = local.export().map_err(|e| e.to_string())?;
        let node = self.local_node_id();
        let cluster = self.cluster().ok_or("not in a cluster")?;
        let leader_here = cluster.is_leader();
        if leader_here {
            let central = meter.ledger().clone_handle();
            let (n, e) = (node.clone(), export.clone());
            tokio::task::spawn_blocking(move || central.import(&n, &e)).await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())?;
        } else {
            let body = bytes::Bytes::from(serde_json::to_vec(&export).map_err(|e| e.to_string())?);
            let reply = match self.node_link() {
                Some(link) => link.post_leader("/cluster/v1/ledger", body).await?,
                None => cluster.post_leader("/cluster/v1/ledger", body).await?,
            };
            if !reply.status.is_success() {
                return Err(format!("the leader answered {}", reply.status));
            }
        }
        let local = meter.local_ledger().expect("checked");
        local.ack_export(&export).map_err(|e| e.to_string())
    }

    /// Snapshot everything and run one round (port counters from netd).
    async fn meter_round(&self, meter: &Arc<Meter>) -> Result<(), MeteringError> {
        let vms: Vec<Vm> = self.vms.read().await.values().filter(|v| self.is_local(v)).cloned().collect();
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
        self.meter.load_full()
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
