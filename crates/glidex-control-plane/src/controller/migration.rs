//! Live migration (spec/32-live-migration.md): what may move where (§5).

use std::collections::BTreeMap;

use crate::controller::queue::Key;
use crate::datastore::record::NodeRow;
use crate::hypervisor::HypervisorType;
use crate::models::{Migration, MigrationPhase, PowerState, Vm, VmPhase};
use crate::state::{VmManager, VmManagerError};
use crate::store::{Event, EventKind};

/// Why a VM can't migrate, and each target's answer (§5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationCheck {
    /// Reasons that hold whatever the target: the VM itself can't move.
    pub vm: Vec<String>,
    pub targets: Vec<TargetCheck>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TargetCheck {
    pub node: String,
    pub name: String,
    /// Empty: the VM can go there, if `MigrationCheck::vm` is empty too.
    pub reasons: Vec<String>,
}

impl MigrationCheck {
    /// The targets the VM can migrate to now.
    pub fn fitting(&self) -> impl Iterator<Item = &TargetCheck> {
        self.targets
            .iter()
            .filter(|t| self.vm.is_empty() && t.reasons.is_empty())
    }
}

impl VmManager {
    /// `POST /vms/{id}/migrate {check: true}` (§5, §10): the checks,
    /// against `node` or every other node, without moving anything.
    pub async fn migration_check(
        &self,
        vm_id: &str,
        node: Option<&str>,
    ) -> Result<MigrationCheck, VmManagerError> {
        if self.cluster().is_none() {
            return Err(crate::state::invalid(
                "a standalone host has no other node to migrate a VM to",
            ));
        }
        let want = node.map(|n| self.resolve_node(n)).transpose()?;
        let vms = self.vms.read().await;
        let vm = vms
            .get(vm_id)
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;
        let c = vm.config();
        let source = vm.status.placement.as_ref().map(|p| p.node.clone());
        let mut out = MigrationCheck::default();
        let mut no = |r: String| out.vm.push(r);

        if c.hypervisor != HypervisorType::CloudHypervisor {
            no(format!(
                "it runs on {}; only Cloud Hypervisor VMs migrate",
                c.hypervisor
            ));
        }
        if let Some(m) = vm.status.migrating() {
            no(format!("it is already migrating, to node {}", m.target));
        }
        if vm.deletion_requested_at.is_some() {
            no("it is being deleted".into());
        } else if vm.spec.power != PowerState::Running || vm.status.phase != VmPhase::Running {
            no(format!("it is {}, not running", vm.state()));
        }
        if vm.status.boot_cpuid.is_none() {
            no("its node recorded no CPUID when it booted; restart it once".into());
        }
        if c.root_disk.is_none() && !c.rootfs_path.is_empty() {
            no(format!(
                "its root is a host path, {}, on its node",
                c.rootfs_path
            ));
        }
        if !c.vfio_devices.is_empty() || !c.devices.is_empty() || c.gpus.is_some() {
            no("it passes through PCI devices".into());
        }
        let placement = vm.status.placement.as_ref();
        // Decision 17: the received configuration keeps the source's CPUs
        // and NUMA binding.
        if !c.cpu_policy.is_shared() || placement.is_some_and(|p| !p.cpus.is_empty()) {
            no("it has dedicated CPUs".into());
        }
        if !c.numa_policy.is_none()
            || placement.is_some_and(|p| p.numa_node.is_some() || !p.numa.is_empty())
        {
            no("it has a NUMA policy".into());
        }
        if placement.is_some_and(|p| !p.dpu_nics.is_empty()) {
            no("its NICs are on a DPU".into());
        }
        for a in &c.networks {
            if let Ok(Some(n)) = self.networks.get(&a.network)
                && n.scope == crate::network::NetworkScope::Node
            {
                no(format!("network {} exists on its node only", n.name));
            }
        }
        let db = self.store.database();
        let store_name = |id: &str| {
            crate::datastore::record::find(&db, id)
                .ok()
                .flatten()
                .map(|r| r.name)
                .unwrap_or_else(|| id.to_string())
        };
        // §7: each end's hyperstor-vhost must offer the dirty log, the
        // drain and the delegate, as its node last reported.
        let mut hyperstor: Vec<(String, BTreeMap<String, NodeRow>)> = Vec::new();
        let mut disks = Vec::new();
        for id in c.root_disk.iter().chain(c.data_disks.iter()) {
            let d = self.images.get_disk(id)?;
            if !self.images.is_hyperstor(&d) {
                no(format!(
                    "disk {} is on datastore {}; only hyperstor disks migrate",
                    d.name,
                    store_name(&d.datastore)
                ));
            } else if !hyperstor.iter().any(|(ds, _)| *ds == d.datastore) {
                let rows = crate::datastore::record::rows(&db, &d.datastore).unwrap_or_default();
                hyperstor.push((d.datastore.clone(), rows));
            }
            disks.push(d);
        }
        let migrates = |rows: &BTreeMap<String, NodeRow>, node: &str| {
            rows.get(node)
                .is_some_and(|r| r.reachable && r.live_migration)
        };
        for (ds, rows) in &hyperstor {
            if source.as_deref().is_some_and(|n| !migrates(rows, n)) {
                no(format!(
                    "hyperstor-vhost on its node can't hand datastore {}'s disks over; upgrade hyperstor there",
                    store_name(ds)
                ));
            }
        }
        let datapath = |id: &str| {
            self.nodes()
                .get(id)
                .ok()
                .flatten()
                .and_then(|n| n.status.features.br_int_datapath)
        };
        let source_datapath = source.as_deref().and_then(datapath);
        if source_datapath.as_deref() == Some("netdev") && !c.networks.is_empty() {
            no("its NICs are vhost-user (its node's br-int runs DPDK)".into());
        }

        let mut req = self.sched_request_pub(vm, &disks);
        // A pin to its node doesn't hold the VM there: migrating moves it,
        // and the commit pins it to the target.
        req.pin = None;
        for fit in self.evaluate_nodes(&vms, vm_id, &req) {
            if want.as_ref().is_some_and(|w| *w != fit.id) {
                continue;
            }
            let mut reasons = Vec::new();
            if source.as_deref() == Some(fit.id.as_str()) {
                reasons.push("the VM runs there".into());
            } else {
                if let Some(r) = fit.refused {
                    reasons.push(r);
                }
                for (ds, rows) in &hyperstor {
                    if let Some(r) = hyperstor_target_refusal(rows.get(&fit.id), &store_name(ds)) {
                        reasons.push(r);
                    }
                }
                match self.nodes().get(&fit.id).ok().flatten() {
                    Some(n) => {
                        if let Err(r) = crate::node::cpu_fits(vm.status.boot_cpuid.as_deref(), &n) {
                            reasons.push(r);
                        }
                        if !c.networks.is_empty()
                            && n.status.features.br_int_datapath.as_deref() == Some("netdev")
                        {
                            reasons.push("its br-int runs DPDK: NICs there are vhost-user".into());
                        }
                    }
                    None => reasons.push("no such node".into()),
                }
            }
            out.targets.push(TargetCheck {
                node: fit.id,
                name: fit.name,
                reasons,
            });
        }
        out.targets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

/// Why a node's `hyperstor-vhost`, as its row last reported, can't be a
/// migration's target for a datastore's disks, if it can't (§7). Besides
/// the handover (`live_migration`), the target must hold a standby
/// volume's requests until the promotion (`standby_hold`, decision 21):
/// the guest resumes there first, and an older one fails its writes.
fn hyperstor_target_refusal(row: Option<&NodeRow>, datastore: &str) -> Option<String> {
    match row {
        Some(r) if r.reachable && r.live_migration && r.standby_hold => None,
        Some(r) if r.reachable && r.live_migration => Some(format!(
            "hyperstor-vhost there doesn't hold datastore {datastore}'s writes until the handover (no standby_hold); upgrade hyperstor there"
        )),
        _ => Some(format!(
            "hyperstor-vhost there can't take datastore {datastore}'s disks over"
        )),
    }
}

/// The path of a migration's stream from its source to its target (§6).
pub fn stream_path(vm: &str) -> String {
    format!("/cluster/v1/migrations/{vm}/stream")
}

/// Whether node `caller` may relay a migration's stream for a VM placed on
/// `placed_on` to node `me` (§6): only from the VM's node, to the target
/// its migration record names, while it copies.
pub fn may_relay(
    placed_on: Option<&str>,
    migration: Option<&Migration>,
    me: &str,
    caller: &str,
) -> Result<(), &'static str> {
    let Some(m) = migration else {
        return Err("the VM isn't migrating");
    };
    if m.target != me {
        return Err("the VM isn't migrating to this node");
    }
    if placed_on != Some(caller) {
        return Err("only the VM's node sends its migration");
    }
    if m.phase != MigrationPhase::Copying {
        return Err("the migration isn't copying");
    }
    Ok(())
}

/// What a migration's relay carried each way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relayed {
    pub to_target: u64,
    pub from_target: u64,
}

impl VmManager {
    /// The source's end of a migration's relay (§6): accept the sending
    /// Cloud Hypervisor's connection on `listener` (bound at the VM's
    /// `migration.sock` before `send-migration`), open the stream to the
    /// migration's target over the peer connection, and copy both ways
    /// until either side closes. The bytes are Cloud Hypervisor's; the
    /// relay doesn't read them.
    pub async fn relay_migration(
        &self,
        vm_id: &str,
        listener: tokio::net::UnixListener,
    ) -> Result<Relayed, String> {
        let vm = self.get_vm(vm_id).await.map_err(|e| e.to_string())?;
        let me = self.local_node_id();
        let m = vm
            .status
            .migration
            .as_ref()
            .ok_or("the VM isn't migrating")?;
        let placed = vm.status.placement.as_ref().map(|p| p.node.as_str());
        if placed != Some(me.as_str()) {
            return Err("the VM isn't on this node".into());
        }
        if m.phase != MigrationPhase::Copying {
            return Err("the migration isn't copying".into());
        }
        let cluster = self.cluster().ok_or("not in a cluster")?;
        let addr = self
            .nodes()
            .get(&m.target)
            .ok()
            .flatten()
            .and_then(|n| n.status.advertise)
            .ok_or_else(|| format!("node {} has no address", m.target))?;
        let target = crate::cluster::net::Target::node(m.target.clone(), addr);
        let (mut hv, _) = listener
            .accept()
            .await
            .map_err(|e| format!("the sending hypervisor: {e}"))?;
        let mut up = cluster
            .client
            .upgrade(target, &stream_path(vm_id))
            .await
            .map_err(|e| format!("the target's stream: {e}"))?;
        let (to_target, from_target) = tokio::io::copy_bidirectional(&mut hv, &mut up)
            .await
            .map_err(|e| format!("relaying the migration: {e}"))?;
        Ok(Relayed {
            to_target,
            from_target,
        })
    }
}

/// How long the leader lets a migration prepare, or cancel, before it
/// steps in, and how long a node may be silent before the guest has moved
/// (decision 18).
pub const PREPARE_SECS: u64 = 120;
pub const CANCEL_SECS: u64 = 120;
pub const NODE_DOWN_SECS: u64 = 30;
/// How long a guest the source has sent may take to run on the target.
pub const RECEIVE_SECS: u64 = 120;
/// `downtime_ms` when the request names none: Cloud Hypervisor's default.
pub const DEFAULT_DOWNTIME_MS: u32 = 300;

/// What the leader's loop does with a migration in progress (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaderStep {
    Commit,
    Cancel(String),
    Fail(String),
}

/// The leader's next step for `m`, a migration in progress from `source`,
/// at `now`; `down_for` says how long a node hasn't been `Ready` (0 while
/// it is).
pub fn leader_step(
    m: &Migration,
    source: &str,
    now: u64,
    down_for: impl Fn(&str) -> u64,
) -> Option<LeaderStep> {
    let source_down = down_for(source) >= NODE_DOWN_SECS;
    if m.received.is_some() {
        // The guest runs on the target: commit once the source has let
        // go, or without it if the source is gone (§4).
        return (m.source_released || source_down).then_some(LeaderStep::Commit);
    }
    if m.moved() {
        // Sent but not yet received: the target reports it, unless its
        // receive failed and nothing runs the guest (§11).
        if now.saturating_sub(m.updated_at) >= RECEIVE_SECS {
            return Some(LeaderStep::Fail(format!(
                "the guest left node {source} but didn't start on node {} within {RECEIVE_SECS} s",
                m.target
            )));
        }
        return None;
    }
    if source_down {
        return Some(LeaderStep::Fail(format!(
            "node {source} stopped reporting before the guest moved"
        )));
    }
    if m.phase == MigrationPhase::Cancelling {
        if now.saturating_sub(m.updated_at) >= CANCEL_SECS {
            let why = if m.reason.is_empty() {
                "cancelling".to_string()
            } else {
                m.reason.clone()
            };
            return Some(LeaderStep::Fail(format!(
                "{why}; not cleaned up within {CANCEL_SECS} s"
            )));
        }
        return None;
    }
    if down_for(&m.target) >= NODE_DOWN_SECS {
        return Some(LeaderStep::Cancel(format!(
            "node {} stopped reporting",
            m.target
        )));
    }
    if m.phase == MigrationPhase::Preparing && now.saturating_sub(m.started_at) >= PREPARE_SECS {
        return Some(LeaderStep::Cancel(format!(
            "the target wasn't ready within {PREPARE_SECS} s"
        )));
    }
    None
}

/// The record without the fields `allowed` may change, for comparing a
/// node's write against the current record.
fn masked(m: &Migration, allowed: impl Fn(&mut Migration)) -> Migration {
    let mut m = m.clone();
    m.updated_at = 0;
    allowed(&mut m);
    m
}

/// What the target may write to the record (decision 13): `Copying` when
/// ready, `Cancelling` with a reason before the guest moves, `received`,
/// and its own release. Nothing else, and never the VM outside its
/// record.
pub fn target_may_write(cur: &Migration, new: &Migration) -> Result<(), String> {
    use MigrationPhase::*;
    let clear = |m: &mut Migration| {
        m.phase = Checking;
        m.reason.clear();
        m.received = None;
        m.target_released = false;
    };
    if masked(cur, clear) != masked(new, clear) {
        return Err("the target may write only its own steps of the migration".into());
    }
    let phase_ok = cur.phase == new.phase
        || matches!((cur.phase, new.phase), (Preparing, Copying))
        || (matches!(cur.phase, Preparing | Copying) && new.phase == Cancelling && !cur.moved());
    if !phase_ok {
        return Err(format!(
            "the target may not move the migration from {:?} to {:?}",
            cur.phase, new.phase
        ));
    }
    if cur.reason != new.reason && new.phase != Cancelling {
        return Err("the target gives a reason only with a cancel".into());
    }
    if cur.received != new.received
        && !(cur.received.is_none() && matches!(cur.phase, Copying | Switching | Cancelling))
    {
        return Err("the target reports the guest received once, after copying began".into());
    }
    if cur.target_released != new.target_released
        && !(new.target_released && matches!(cur.phase, Cancelling | Failed))
    {
        return Err("the target lets go only after a cancel".into());
    }
    Ok(())
}

/// What the source may write to the record (decision 13): the delegates,
/// progress, `sent` with `Switching`, `Cancelling` before the guest moves,
/// `Failed` after a cancel, and its own release. `None` on both sides: no
/// migration, nothing to check.
pub fn source_may_write(cur: Option<&Migration>, new: Option<&Migration>) -> Result<(), String> {
    use MigrationPhase::*;
    let (cur, new) = match (cur, new) {
        (None, None) => return Ok(()),
        (Some(c), Some(n)) => (c, n),
        _ => return Err("only the leader starts or ends a migration".into()),
    };
    if cur == new {
        return Ok(());
    }
    let clear = |m: &mut Migration| {
        m.phase = Checking;
        m.reason.clear();
        m.delegates.clear();
        m.sent = false;
        m.sent_at = 0;
        m.source_released = false;
        m.bytes = 0;
        m.iteration = 0;
        m.dirty_rate = 0;
    };
    if masked(cur, clear) != masked(new, clear) {
        return Err("the source may write only its own steps of the migration".into());
    }
    let phase_ok = cur.phase == new.phase
        || (matches!(cur.phase, Copying | Cancelling) && new.phase == Switching && new.sent)
        || (matches!(cur.phase, Preparing | Copying) && new.phase == Cancelling && !cur.moved())
        || (cur.phase == Cancelling && new.phase == Failed && !new.moved());
    if !phase_ok {
        return Err(format!(
            "the source may not move the migration from {:?} to {:?}",
            cur.phase, new.phase
        ));
    }
    if cur.delegates != new.delegates && !(cur.delegates.is_empty() && cur.phase == Preparing) {
        return Err("the source names the delegates once, while preparing".into());
    }
    if cur.sent && !new.sent || cur.source_released && !new.source_released {
        return Err("the source's steps aren't undone".into());
    }
    if new.source_released && !cur.source_released && new.received.is_none() {
        return Err("the source lets go after the target has the guest".into());
    }
    Ok(())
}

impl VmManager {
    /// `POST /vms/{id}/migrate` (§4, step 1), on the leader: the checks of
    /// §5, then the record, with the target's placement reserved. Without
    /// `node`, the scheduler's choice among the targets that fit.
    pub async fn start_migration(
        &self,
        vm_id: &str,
        node: Option<&str>,
        downtime_ms: Option<u32>,
        actor: &str,
    ) -> Result<Vm, VmManagerError> {
        if !self.store.database().can_write() {
            return Err(crate::state::invalid(
                "only the cluster's leader starts a migration",
            ));
        }
        if downtime_ms == Some(0) {
            return Err(crate::state::invalid("downtime_ms must be at least 1"));
        }
        {
            let vms = self.vms.read().await;
            let vm = vms
                .get(vm_id)
                .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;
            if vm.status.migrating().is_some() {
                return Err(VmManagerError::Migrating(vm.name.clone()));
            }
        }
        let check = self.migration_check(vm_id, node).await?;
        if !check.vm.is_empty() {
            return Err(crate::state::invalid(format!(
                "it can't migrate: {}",
                check.vm.join("; ")
            )));
        }
        let fitting: Vec<String> = check.fitting().map(|t| t.node.clone()).collect();
        if fitting.is_empty() {
            let why: Vec<String> = check
                .targets
                .iter()
                .map(|t| format!("{}: {}", t.name, t.reasons.join(", ")))
                .collect();
            return Err(crate::state::invalid(format!(
                "no node can take it ({})",
                why.join("; ")
            )));
        }
        let mut vms = self.vms.write().await;
        let mut vm = vms
            .get(vm_id)
            .cloned()
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;
        if vm.status.migrating().is_some() {
            return Err(VmManagerError::Migrating(vm.name.clone()));
        }
        let mut disks = Vec::new();
        for id in vm
            .config()
            .root_disk
            .iter()
            .chain(vm.config().data_disks.iter())
        {
            disks.push(self.images.get_disk(id)?);
        }
        let mut req = self.sched_request_pub(&vm, &disks);
        req.pin = None;
        // The scheduler's order, among the targets the checks let through.
        let fit = self
            .evaluate_nodes(&vms, vm_id, &req)
            .into_iter()
            .find(|f| f.refused.is_none() && fitting.contains(&f.id))
            .ok_or_else(|| crate::state::invalid("no node can take it now"))?;
        let now = crate::tenancy::now();
        let target = fit.id.clone();
        vm.status.migration = Some(Migration {
            target: target.clone(),
            phase: MigrationPhase::Preparing,
            started_at: now,
            updated_at: now,
            downtime_ms: downtime_ms.unwrap_or(DEFAULT_DOWNTIME_MS),
            placement: Some(crate::models::Placement {
                node: target.clone(),
                at: now,
                page_kib: Some(fit.page_kib),
                numa_node: fit.numa_node,
                numa: fit.numa,
                cpus: fit.cpus,
                pool_devices: fit.pool_devices,
                dpu_nics: fit.dpu_nics,
            }),
            ..Default::default()
        });
        vm.resource_version += 1;
        let ev = Event::new(
            actor,
            EventKind::Normal,
            "MigrationStarted",
            format!("migrating to node {}", fit.name),
        );
        let vm = self.put_locked(&mut vms, vm, vec![ev])?;
        self.queue.add(Key::Vm(vm.id.clone()));
        tracing::info!(vm = %vm.name, target = %fit.name, "migration started");
        Ok(vm)
    }

    /// `POST /vms/{id}/migrate/cancel` (§4), on the leader. Once the guest
    /// has moved, or while already cancelling, the record is left as it is:
    /// the migration finishes (decision 14).
    pub async fn cancel_migration(&self, vm_id: &str, actor: &str) -> Result<Vm, VmManagerError> {
        if !self.store.database().can_write() {
            return Err(crate::state::invalid(
                "only the cluster's leader cancels a migration",
            ));
        }
        let mut vms = self.vms.write().await;
        let mut vm = vms
            .get(vm_id)
            .cloned()
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;
        let Some(m) = vm.status.migration.as_mut().filter(|m| m.in_progress()) else {
            return Err(VmManagerError::NotMigrating(vm.name.clone()));
        };
        if m.moved() || m.phase != MigrationPhase::Preparing && m.phase != MigrationPhase::Copying {
            return Ok(vm);
        }
        m.phase = MigrationPhase::Cancelling;
        m.reason = format!("cancelled by {actor}");
        m.cancel_asked = true;
        m.updated_at = crate::tenancy::now();
        vm.resource_version += 1;
        let vm = self.put_locked(&mut vms, vm, vec![])?;
        self.queue.add(Key::Vm(vm.id.clone()));
        Ok(vm)
    }

    /// The leader's loop over migrations in progress (§4): commit, cancel
    /// or fail each as [`leader_step`] says. Every other server returns at
    /// once, so a new leader picks the records up where they are.
    pub async fn migration_round(&self) {
        if self.cluster().is_none() || !self.store.database().can_write() {
            return;
        }
        let now = crate::tenancy::now();
        let nodes = self.nodes().list().unwrap_or_default();
        let down_for = |id: &str| {
            nodes
                .iter()
                .find(|n| n.meta.id == id)
                .map(|n| {
                    if n.status.ready == crate::models::Tristate::True {
                        0
                    } else {
                        now.saturating_sub(n.status.heartbeat.unwrap_or(0))
                    }
                })
                .unwrap_or(u64::MAX)
        };
        let mut vms = self.vms.write().await;
        let moving: Vec<Vm> = vms
            .values()
            .filter(|v| v.status.migrating().is_some())
            .cloned()
            .collect();
        for vm in moving {
            let Some(m) = vm.status.migration.as_ref() else {
                continue;
            };
            let source = vm
                .status
                .placement
                .as_ref()
                .map(|p| p.node.clone())
                .unwrap_or_default();
            let Some(step) = leader_step(m, &source, now, down_for) else {
                continue;
            };
            let name = vm.name.clone();
            let r = match step {
                LeaderStep::Commit => self.commit_migration(&mut vms, vm, now),
                LeaderStep::Cancel(why) => {
                    let mut vm = vm;
                    if let Some(m) = vm.status.migration.as_mut() {
                        m.phase = MigrationPhase::Cancelling;
                        m.reason = why;
                        m.updated_at = now;
                    }
                    vm.resource_version += 1;
                    self.put_locked(&mut vms, vm, vec![]).map(|_| ())
                }
                LeaderStep::Fail(why) => {
                    let mut vm = vm;
                    let mut ev = None;
                    if let Some(m) = vm.status.migration.as_mut() {
                        m.phase = MigrationPhase::Failed;
                        m.reason = why;
                        m.updated_at = now;
                        ev = Some(failed_event(m));
                    }
                    vm.resource_version += 1;
                    self.put_locked(&mut vms, vm, ev.into_iter().collect())
                        .map(|_| ())
                }
            };
            if let Err(e) = r {
                tracing::warn!(vm = %name, "a migration's step: {}", e);
            }
        }
    }

    /// The commit (§4, step 5; decision 5): one write moves the VM to the
    /// target, with the target's placement and instance, its disks bound
    /// there, and the record cleared.
    fn commit_migration(
        &self,
        vms: &mut std::collections::HashMap<String, Vm>,
        mut vm: Vm,
        now: u64,
    ) -> Result<(), VmManagerError> {
        let Some(m) = vm.status.migration.take() else {
            return Ok(());
        };
        let source = vm.status.placement.as_ref().map(|p| p.node.clone());
        // The start always reserves one; a record without it moves the
        // VM's own, which holds no host CPUs or NUMA node (decision 17).
        let Some(mut placement) = m.placement.clone().or_else(|| vm.status.placement.clone())
        else {
            return Ok(());
        };
        placement.node = m.target.clone();
        placement.at = now;
        vm.status.placement = Some(placement);
        vm.status.instance = m.received.clone();
        if vm.spec.node.is_some() {
            // Pinned: pinned where it now runs, as `place` does (LM0).
            vm.spec.node = Some(m.target.clone());
            vm.generation += 1;
        }
        vm.resource_version += 1;
        let mut disks = Vec::new();
        for id in vm
            .config()
            .root_disk
            .iter()
            .chain(vm.config().data_disks.iter())
        {
            let mut d = self.images.get_disk(id)?;
            let shared = self.images.is_shared(&d);
            d.bind_to(&m.target, shared);
            disks.push(d);
        }
        let ev = Event::new(
            "controller",
            EventKind::Normal,
            "Migrated",
            format!(
                "moved from node {} to node {}",
                source.as_deref().unwrap_or("?"),
                m.target
            ),
        );
        self.store.commit(crate::store::Commit {
            put_vm: Some(&vm),
            put_disks: disks.iter().collect(),
            events: vec![(crate::store::event_key("vm", &vm.id), ev)],
            ..Default::default()
        })?;
        for d in &disks {
            self.images.cache_disk(d);
        }
        tracing::info!(vm = %vm.name, target = %m.target, "migration committed");
        vms.insert(vm.id.clone(), vm.clone());
        self.queue.add(Key::Vm(vm.id.clone()));
        self.notify_changed();
        Ok(())
    }
}

impl VmManager {
    /// A node's write of its own step into VM `vm_id`'s record (§4,
    /// decision 13): `f` changes the record, and the write goes to the
    /// leader, which checks it. `None` when there is no record to change.
    pub async fn write_migration(
        &self,
        vm_id: &str,
        f: impl FnOnce(&mut Migration),
    ) -> Result<Option<Vm>, VmManagerError> {
        let mut vms = self.vms.write().await;
        let mut vm = vms
            .get(vm_id)
            .cloned()
            .ok_or_else(|| VmManagerError::VmNotFound(vm_id.to_string()))?;
        let Some(m) = vm.status.migration.as_mut() else {
            return Ok(None);
        };
        let before = m.clone();
        f(m);
        if *m == before {
            return Ok(Some(vm));
        }
        m.updated_at = crate::tenancy::now();
        let ev = (m.phase == MigrationPhase::Failed && before.phase != MigrationPhase::Failed)
            .then(|| failed_event(m));
        vm.resource_version += 1;
        self.put_locked(&mut vms, vm, ev.into_iter().collect())
            .map(Some)
    }
}

/// The event for a migration that ended in `Failed`: cancelled when
/// someone asked, failed otherwise.
pub fn failed_event(m: &Migration) -> Event {
    if m.cancel_asked {
        Event::new(
            "controller",
            EventKind::Normal,
            "MigrationCancelled",
            m.reason.clone(),
        )
    } else {
        Event::new(
            "controller",
            EventKind::Warning,
            "MigrationFailed",
            m.reason.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A target's hyperstor-vhost must hand the disks over and hold a
    /// standby volume's requests (decision 21); unreported is neither.
    #[test]
    fn a_target_without_standby_hold_is_refused() {
        let row = |live_migration, standby_hold| NodeRow {
            reachable: true,
            live_migration,
            standby_hold,
            ..Default::default()
        };
        assert_eq!(hyperstor_target_refusal(Some(&row(true, true)), "hs"), None);
        assert!(
            hyperstor_target_refusal(Some(&row(true, false)), "hs")
                .unwrap()
                .contains("no standby_hold")
        );
        assert!(
            hyperstor_target_refusal(Some(&row(false, false)), "hs")
                .unwrap()
                .contains("can't take datastore hs's disks over")
        );
        assert!(hyperstor_target_refusal(None, "hs").is_some());
        let unreachable = NodeRow {
            reachable: false,
            ..row(true, true)
        };
        assert!(hyperstor_target_refusal(Some(&unreachable), "hs").is_some());
    }

    fn record(target: &str, phase: MigrationPhase) -> Migration {
        Migration {
            target: target.into(),
            phase,
            started_at: 1,
            updated_at: 1,
            ..Default::default()
        }
    }

    fn down(nodes: &'static [(&'static str, u64)]) -> impl Fn(&str) -> u64 {
        move |n| nodes.iter().find(|(id, _)| *id == n).map_or(0, |(_, s)| *s)
    }

    fn received() -> crate::models::InstanceRef {
        serde_json::from_value(serde_json::json!({
            "instance_id": "i-2", "runner": { "kind": "detached" }, "boot_id": "b",
            "launched_generation": 1, "launched_at": 5,
        }))
        .unwrap()
    }

    /// §4's loop: the commit waits for both sides, and each time limit
    /// cancels or fails before the guest has moved, never after.
    #[test]
    fn the_leader_commits_cancels_and_fails_as_the_record_says() {
        let mut m = record("node-b", MigrationPhase::Copying);
        m.started_at = 100;
        m.updated_at = 100;
        let ok = down(&[]);
        assert_eq!(leader_step(&m, "node-a", 110, &ok), None);

        // Received, but the source hasn't let go: wait, unless it is gone.
        m.received = Some(received());
        assert_eq!(leader_step(&m, "node-a", 110, &ok), None);
        assert_eq!(
            leader_step(&m, "node-a", 110, down(&[("node-a", 30)])),
            Some(LeaderStep::Commit)
        );
        m.source_released = true;
        assert_eq!(
            leader_step(&m, "node-a", 110, &ok),
            Some(LeaderStep::Commit)
        );

        // Sent, not yet received: nothing to decide, even with a dead
        // target or a cancel under way, until the target's time is up.
        let mut sent = record("node-b", MigrationPhase::Switching);
        sent.sent = true;
        sent.updated_at = 100;
        assert_eq!(
            leader_step(&sent, "node-a", 101, down(&[("node-b", 99)])),
            None
        );
        sent.phase = MigrationPhase::Cancelling;
        assert_eq!(leader_step(&sent, "node-a", 101, &ok), None);
        assert!(matches!(
            leader_step(&sent, "node-a", 100 + RECEIVE_SECS, &ok),
            Some(LeaderStep::Fail(r)) if r.contains("didn't start")
        ));

        // Preparing too long; the target gone; the source gone.
        let mut prep = record("node-b", MigrationPhase::Preparing);
        prep.started_at = 100;
        prep.updated_at = 100;
        assert_eq!(
            leader_step(&prep, "node-a", 100 + PREPARE_SECS - 1, &ok),
            None
        );
        assert!(matches!(
            leader_step(&prep, "node-a", 100 + PREPARE_SECS, &ok),
            Some(LeaderStep::Cancel(_))
        ));
        assert!(matches!(
            leader_step(&prep, "node-a", 101, down(&[("node-b", NODE_DOWN_SECS)])),
            Some(LeaderStep::Cancel(r)) if r.contains("node-b")
        ));
        assert_eq!(
            leader_step(
                &prep,
                "node-a",
                101,
                down(&[("node-b", NODE_DOWN_SECS - 1)])
            ),
            None
        );
        assert!(matches!(
            leader_step(&prep, "node-a", 101, down(&[("node-a", NODE_DOWN_SECS)])),
            Some(LeaderStep::Fail(r)) if r.contains("node-a")
        ));

        // Cancelling too long fails, keeping the reason.
        let mut c = record("node-b", MigrationPhase::Cancelling);
        c.updated_at = 100;
        c.reason = "cancelled by someone".into();
        assert_eq!(leader_step(&c, "node-a", 100 + CANCEL_SECS - 1, &ok), None);
        assert!(matches!(
            leader_step(&c, "node-a", 100 + CANCEL_SECS, &ok),
            Some(LeaderStep::Fail(r)) if r.starts_with("cancelled by someone")
        ));
        // A dead target while cancelling: the source still drops its
        // delegates, so the leader waits for the cancel's own limit.
        assert_eq!(
            leader_step(&c, "node-a", 101, down(&[("node-b", 99)])),
            None
        );
    }

    /// Decision 13: the target writes only its own steps.
    #[test]
    fn the_target_writes_only_its_steps() {
        let prep = record("node-b", MigrationPhase::Preparing);
        let with = |f: &dyn Fn(&mut Migration)| {
            let mut m = prep.clone();
            f(&mut m);
            m
        };
        // Ready.
        let copying = with(&|m| m.phase = MigrationPhase::Copying);
        assert_eq!(target_may_write(&prep, &copying), Ok(()));
        // A failed preparation, with its reason.
        let cancel = with(&|m| {
            m.phase = MigrationPhase::Cancelling;
            m.reason = "attaching disk d: refused".into();
        });
        assert_eq!(target_may_write(&prep, &cancel), Ok(()));
        // Not the leader's or the source's fields, nor a reason alone.
        for bad in [
            with(&|m| m.phase = MigrationPhase::Switching),
            with(&|m| m.phase = MigrationPhase::Failed),
            with(&|m| m.sent = true),
            with(&|m| m.source_released = true),
            with(&|m| m.target = "node-c".into()),
            with(&|m| {
                m.delegates.insert("d".into(), "x".into());
            }),
            with(&|m| m.reason = "why".into()),
            with(&|m| m.received = Some(received())),
            with(&|m| m.target_released = true),
        ] {
            assert!(target_may_write(&prep, &bad).is_err(), "{bad:?}");
        }
        // Received, once copying began; released after a cancel.
        assert_eq!(
            target_may_write(&copying, &{
                let mut m = copying.clone();
                m.received = Some(received());
                m
            }),
            Ok(())
        );
        assert_eq!(
            target_may_write(&cancel, &{
                let mut m = cancel.clone();
                m.target_released = true;
                m
            }),
            Ok(())
        );
        // No cancel once the guest has moved.
        let mut sent = copying.clone();
        sent.sent = true;
        let mut sent_cancel = sent.clone();
        sent_cancel.phase = MigrationPhase::Cancelling;
        assert!(target_may_write(&sent, &sent_cancel).is_err());
    }

    /// Decision 13: the source writes only its own steps, and never starts
    /// or ends a record.
    #[test]
    fn the_source_writes_only_its_steps() {
        let prep = record("node-b", MigrationPhase::Preparing);
        assert_eq!(source_may_write(None, None), Ok(()));
        assert!(source_may_write(None, Some(&prep)).is_err());
        assert!(source_may_write(Some(&prep), None).is_err());
        assert_eq!(source_may_write(Some(&prep), Some(&prep)), Ok(()));

        let mut delegated = prep.clone();
        delegated
            .delegates
            .insert("d".into(), "0123456789abcdef".into());
        assert_eq!(source_may_write(Some(&prep), Some(&delegated)), Ok(()));
        let mut again = delegated.clone();
        again
            .delegates
            .insert("d".into(), "fedcba9876543210".into());
        assert!(source_may_write(Some(&delegated), Some(&again)).is_err());

        let copying = record("node-b", MigrationPhase::Copying);
        let mut switching = copying.clone();
        switching.phase = MigrationPhase::Switching;
        assert!(
            source_may_write(Some(&copying), Some(&switching)).is_err(),
            "switching without sent"
        );
        switching.sent = true;
        assert_eq!(source_may_write(Some(&copying), Some(&switching)), Ok(()));

        let mut released = switching.clone();
        released.source_released = true;
        assert!(
            source_may_write(Some(&switching), Some(&released)).is_err(),
            "before the target has the guest"
        );
        let mut have = switching.clone();
        have.received = Some(received());
        let mut released = have.clone();
        released.source_released = true;
        assert_eq!(source_may_write(Some(&have), Some(&released)), Ok(()));

        // Not the target's or the leader's fields.
        let mut forged = copying.clone();
        forged.received = Some(received());
        assert!(source_may_write(Some(&copying), Some(&forged)).is_err());
        let mut moved = copying.clone();
        moved.target = "node-c".into();
        assert!(source_may_write(Some(&copying), Some(&moved)).is_err());

        // A cancel ends in Failed; a sent guest doesn't fail.
        let mut cancelling = copying.clone();
        cancelling.phase = MigrationPhase::Cancelling;
        let mut failed = cancelling.clone();
        failed.phase = MigrationPhase::Failed;
        assert_eq!(source_may_write(Some(&copying), Some(&cancelling)), Ok(()));
        assert_eq!(source_may_write(Some(&cancelling), Some(&failed)), Ok(()));
        let mut sent_cancelling = cancelling.clone();
        sent_cancelling.sent = true;
        let mut sent_failed = sent_cancelling.clone();
        sent_failed.phase = MigrationPhase::Failed;
        assert!(source_may_write(Some(&sent_cancelling), Some(&sent_failed)).is_err());
    }

    /// The target relays a stream only from the VM's node, for a migration
    /// to it, while it copies (§6).
    #[test]
    fn only_the_source_relays_to_the_target_while_copying() {
        let copying = record("node-b", MigrationPhase::Copying);
        assert_eq!(
            may_relay(Some("node-a"), Some(&copying), "node-b", "node-a"),
            Ok(())
        );
        assert!(may_relay(Some("node-a"), None, "node-b", "node-a").is_err());
        assert!(
            may_relay(Some("node-a"), Some(&copying), "node-c", "node-a").is_err(),
            "not the target"
        );
        assert!(
            may_relay(Some("node-a"), Some(&copying), "node-b", "node-c").is_err(),
            "not the source"
        );
        assert!(may_relay(None, Some(&copying), "node-b", "node-a").is_err());
        for p in [
            MigrationPhase::Checking,
            MigrationPhase::Preparing,
            MigrationPhase::Switching,
            MigrationPhase::Committed,
            MigrationPhase::Cancelling,
            MigrationPhase::Failed,
        ] {
            assert!(
                may_relay(
                    Some("node-a"),
                    Some(&record("node-b", p)),
                    "node-b",
                    "node-a"
                )
                .is_err(),
                "{p:?}"
            );
        }
    }
}
