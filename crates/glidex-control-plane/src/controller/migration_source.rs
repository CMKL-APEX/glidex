//! A live migration's source (spec/32-live-migration.md §4): the VM's node
//! names each disk's delegate, sends the guest over the relay, lets go once
//! the target has it, and drops the delegates after a cancel.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use glidex_hv_client::ch::ChClient;
use glidex_vm_shim::state::Phase;

use crate::controller::vm::{blocking, shim_call};
use crate::datastore::StorageDriver;
use crate::datastore::hyperstor::HyperstorDriver;
use crate::images::Disk;
use crate::instance::Liveness;
use crate::models::{Migration, MigrationPhase, Vm};
use crate::state::VmManager;

/// `send-migration`'s limit on the whole copy (decision 18).
pub const SEND_TIMEOUT_SECS: u64 = 900;
/// After the relay ends, how long the source waits for its Cloud
/// Hypervisor to exit before it takes the send as failed.
const EXIT_AFTER_SEND_SECS: u64 = 10;

/// What this node is doing for the migrations it sends, in memory: a
/// restart forgets it, and the steps carry on from the record.
#[derive(Debug, Default)]
pub struct Sends {
    /// VMs whose guest is being sent.
    sending: HashSet<String>,
    /// VMs whose send this node has asked Cloud Hypervisor to cancel.
    cancel_asked: HashSet<String>,
    /// Migrations (VM, start) whose delegates this node has dropped.
    dropped: HashSet<(String, u64)>,
}

/// The source's guest, as its shim reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guest {
    /// Still here.
    Here,
    /// The hypervisor has exited: after a send, the guest has left.
    Gone,
    /// Can't tell now; nothing is decided on it.
    Unknown,
}

/// What the source does next for record `m` (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStep {
    Wait,
    /// Name each hyperstor disk's delegate.
    Delegate,
    /// Start the send.
    Send,
    /// The guest has left: write `sent` and `Switching`.
    Sent,
    /// The target has the guest: detach the disks, remove the taps,
    /// release the shim, write `source_released`.
    Release,
    /// Ask Cloud Hypervisor to cancel the send under way.
    CancelSend,
    /// Drop the delegates; `true`: then write `Failed`.
    DropDelegates(bool),
}

/// The source's step (§4, decisions 14 and 15). `sending`: a send runs
/// here; `cancel_asked`: its cancel was asked; `dropped`: the delegates are
/// gone; `delegates_needed`: the VM has hyperstor disks; `target_down`: the
/// target hasn't been `Ready` for 30 s.
pub fn source_step(
    m: &Migration,
    guest: Guest,
    sending: bool,
    cancel_asked: bool,
    dropped: bool,
    delegates_needed: bool,
    target_down: bool,
) -> SourceStep {
    use MigrationPhase::*;
    if m.received.is_some() {
        return if m.source_released {
            SourceStep::Wait
        } else {
            SourceStep::Release
        };
    }
    let gone = guest == Guest::Gone;
    match m.phase {
        Preparing if delegates_needed && m.delegates.is_empty() => SourceStep::Delegate,
        Copying if gone && !sending => SourceStep::Sent,
        Copying if !sending && guest == Guest::Here => SourceStep::Send,
        Cancelling if m.sent => SourceStep::Wait,
        Cancelling if gone && !sending => SourceStep::Sent,
        Cancelling if sending => {
            if cancel_asked {
                SourceStep::Wait
            } else {
                SourceStep::CancelSend
            }
        }
        // Decision 15: the target detaches its standby first.
        Cancelling if m.target_released || target_down => SourceStep::DropDelegates(true),
        Failed if !m.sent && !m.delegates.is_empty() && !dropped => {
            SourceStep::DropDelegates(false)
        }
        _ => SourceStep::Wait,
    }
}

impl VmManager {
    /// One pass over the migrations of VMs placed on this node, from each
    /// node's loop.
    pub async fn migration_source_round(&self) {
        if self.cluster().is_none() {
            return;
        }
        let vms: Vec<Vm> = self
            .vms
            .read()
            .await
            .values()
            .filter(|v| self.is_local(v) && v.status.migration.is_some())
            .cloned()
            .collect();
        for vm in vms {
            if let Err(e) = self.migration_source_step(&vm).await {
                tracing::warn!(vm = %vm.name, "a migration's source: {}", e);
            }
        }
    }

    async fn migration_source_step(&self, vm: &Vm) -> Result<(), String> {
        let Some(m) = vm.status.migration.clone() else {
            return Ok(());
        };
        let disks = self.hyperstor_disks(vm)?;
        let guest = self.source_guest(vm).await;
        let (sending, cancel_asked, dropped) = {
            let s = self.migration_sends.lock().unwrap();
            (
                s.sending.contains(&vm.id),
                s.cancel_asked.contains(&vm.id),
                s.dropped.contains(&(vm.id.clone(), m.started_at)),
            )
        };
        let target_down = self.nodes().get(&m.target).ok().flatten().is_none_or(|n| {
            n.status.ready != crate::models::Tristate::True
                && crate::tenancy::now().saturating_sub(n.status.heartbeat.unwrap_or(0))
                    >= crate::controller::migration::NODE_DOWN_SECS
        });
        let step = source_step(
            &m,
            guest,
            sending,
            cancel_asked,
            dropped,
            !disks.is_empty(),
            target_down,
        );
        match step {
            SourceStep::Wait => Ok(()),
            SourceStep::Delegate => {
                let mut delegates = std::collections::BTreeMap::new();
                for (d, driver) in &disks {
                    let name = d.name.clone();
                    let (d, driver) = (d.clone(), driver.clone());
                    let id = d.id.clone();
                    let delegate = blocking(move || driver.delegate(&d))
                        .await
                        .map_err(|e| e.to_string())?;
                    match delegate {
                        Ok(x) => {
                            delegates.insert(id, x);
                        }
                        Err(e) => {
                            return self
                                .source_cancels(vm, format!("naming disk {name}'s delegate: {e}"))
                                .await;
                        }
                    }
                }
                self.source_writes(vm, |m| m.delegates = delegates).await
            }
            SourceStep::Send => {
                self.migration_sends
                    .lock()
                    .unwrap()
                    .sending
                    .insert(vm.id.clone());
                let me = self.arc();
                let vm = vm.clone();
                tokio::spawn(async move {
                    let r = me.send_guest(&vm, &m).await;
                    if let Err(why) = r {
                        let _ = me.source_cancels(&vm, why).await;
                    }
                    let mut s = me.migration_sends.lock().unwrap();
                    s.sending.remove(&vm.id);
                    s.cancel_asked.remove(&vm.id);
                });
                Ok(())
            }
            SourceStep::Sent => {
                self.source_writes(vm, |m| {
                    m.sent = true;
                    m.phase = MigrationPhase::Switching;
                })
                .await
            }
            SourceStep::Release => {
                self.release_source(vm, &disks).await;
                self.source_writes(vm, |m| m.source_released = true).await
            }
            SourceStep::CancelSend => {
                self.migration_sends
                    .lock()
                    .unwrap()
                    .cancel_asked
                    .insert(vm.id.clone());
                let api = vm.paths().api_socket.clone();
                blocking(move || ChClient::new(&api).cancel_migration())
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("cancelling the send: {e}"))
            }
            SourceStep::DropDelegates(fail) => {
                for (d, driver) in &disks {
                    let Some(delegate) = m.delegates.get(&d.id).cloned() else {
                        continue;
                    };
                    let name = d.name.clone();
                    let (d, driver) = (d.clone(), driver.clone());
                    if let Ok(Err(e)) =
                        blocking(move || driver.release_delegate(&d, &delegate)).await
                    {
                        tracing::warn!(vm = %vm.name, disk = %name, "dropping a delegate: {}", e);
                    }
                }
                self.migration_sends
                    .lock()
                    .unwrap()
                    .dropped
                    .insert((vm.id.clone(), m.started_at));
                if fail {
                    self.source_writes(vm, |m| m.phase = MigrationPhase::Failed)
                        .await
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn source_writes(&self, vm: &Vm, f: impl FnOnce(&mut Migration)) -> Result<(), String> {
        self.write_migration(&vm.id, f)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The send failed or can't start: cancel, keeping the first reason.
    async fn source_cancels(&self, vm: &Vm, why: String) -> Result<(), String> {
        tracing::warn!(vm = %vm.name, "cancelling a migration from this node: {}", why);
        self.source_writes(vm, |m| {
            if !m.moved() && matches!(m.phase, MigrationPhase::Preparing | MigrationPhase::Copying)
            {
                m.phase = MigrationPhase::Cancelling;
                m.reason = format!("on the source: {why}");
            }
        })
        .await
    }

    /// Whether the guest is still here: the shim of the VM's instance
    /// reports its hypervisor running, or exited.
    async fn source_guest(&self, vm: &Vm) -> Guest {
        let Some(inst) = vm.status.instance.as_ref() else {
            return Guest::Unknown;
        };
        let Ok(seen) = self.observe(vm).await else {
            return Guest::Unknown;
        };
        let ours = seen
            .file
            .as_ref()
            .filter(|f| f.instance_id == inst.instance_id);
        match (&seen.liveness, ours) {
            (Liveness::Unknown(_), _) => Guest::Unknown,
            (Liveness::Dead, _) => Guest::Gone,
            (Liveness::Live, Some(f)) if f.phase == Phase::Exited => Guest::Gone,
            (Liveness::Live, _) => Guest::Here,
        }
    }

    /// §4 step 3: the relay from this node's `migration-send.sock` to the
    /// target, and Cloud Hypervisor's send into it. `Ok` once the guest
    /// has left (its hypervisor exited); the reason otherwise.
    async fn send_guest(&self, vm: &Vm, m: &Migration) -> Result<(), String> {
        let paths = vm.paths();
        let sock = paths.migration_send_socket.clone();
        let _ = std::fs::remove_file(&sock);
        let listener =
            tokio::net::UnixListener::bind(&sock).map_err(|e| format!("binding {sock}: {e}"))?;
        let me = self.arc();
        let id = vm.id.clone();
        let relay = tokio::spawn(async move { me.relay_migration(&id, listener).await });
        let api = paths.api_socket.clone();
        let url = format!("unix:{sock}");
        let downtime = m.downtime_ms;
        let sent =
            blocking(move || ChClient::new(&api).send_migration(&url, downtime, SEND_TIMEOUT_SECS))
                .await
                .map_err(|e| e.to_string())?;
        if let Err(e) = sent {
            relay.abort();
            let _ = std::fs::remove_file(&sock);
            return Err(format!("the send: {e}"));
        }
        // v55's send answered when the copy began; the guest has left
        // when the hypervisor exits (decision 14), whether or not the
        // target has closed its end yet. A relay that ends with the guest
        // still here, for a few seconds, is a failed send.
        let mut relay = relay;
        let mut ended: Option<(tokio::time::Instant, String)> = None;
        loop {
            let cur = self.vm(&vm.id).await;
            let gone = match &cur {
                Some(v) => self.source_guest(v).await == Guest::Gone,
                None => false,
            };
            if gone {
                relay.abort();
                let _ = std::fs::remove_file(&sock);
                return Ok(());
            }
            if ended.is_none() && relay.is_finished() {
                let why = match (&mut relay).await {
                    Ok(Ok(_)) => "the copy ended and the guest is still here".to_string(),
                    Ok(Err(e)) => e,
                    Err(e) => e.to_string(),
                };
                let _ = std::fs::remove_file(&sock);
                ended = Some((
                    tokio::time::Instant::now() + Duration::from_secs(EXIT_AFTER_SEND_SECS),
                    why,
                ));
            }
            if let Some((deadline, why)) = &ended
                && tokio::time::Instant::now() >= *deadline
            {
                return Err(why.clone());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// §4 step 4 on the source, once the target has the guest: its disks
    /// detached here (after the target's promotion, decision 15), its taps
    /// removed, its shim released. Best-effort; what fails is logged.
    async fn release_source(&self, vm: &Vm, disks: &[(Disk, Arc<HyperstorDriver>)]) {
        let paths = vm.paths();
        let _ = shim_call(paths.shim_socket.clone(), |c| c.release()).await;
        for (d, driver) in disks {
            let name = d.name.clone();
            let (d, driver) = (d.clone(), driver.clone());
            if let Ok(Err(e)) = blocking(move || driver.unstage(&d)).await {
                tracing::warn!(vm = %vm.name, disk = %name, "detaching a migrated disk: {}", e);
            }
        }
        self.remove_migration_taps(vm).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(phase: MigrationPhase) -> Migration {
        Migration {
            target: "node-b".into(),
            phase,
            ..Default::default()
        }
    }

    /// §4 on the source: delegates first, then the send; the guest gone is
    /// `sent`; once received, the release; a cancel asks Cloud Hypervisor
    /// once, and drops the delegates after the target's release (or with
    /// the target down), then fails.
    #[test]
    fn the_source_follows_the_record_and_its_guest() {
        use Guest::*;
        use MigrationPhase::*;
        let step = |m: &Migration, g, sending, asked, needed, down| {
            source_step(m, g, sending, asked, false, needed, down)
        };
        assert_eq!(
            step(&record(Preparing), Here, false, false, true, false),
            SourceStep::Delegate
        );
        assert_eq!(
            step(&record(Preparing), Here, false, false, false, false),
            SourceStep::Wait
        );
        let mut named = record(Preparing);
        named.delegates.insert("d".into(), "x".into());
        assert_eq!(
            step(&named, Here, false, false, true, false),
            SourceStep::Wait
        );

        assert_eq!(
            step(&record(Copying), Here, false, false, true, false),
            SourceStep::Send
        );
        assert_eq!(
            step(&record(Copying), Here, true, false, true, false),
            SourceStep::Wait
        );
        assert_eq!(
            step(&record(Copying), Unknown, false, false, true, false),
            SourceStep::Wait
        );
        assert_eq!(
            step(&record(Copying), Gone, false, false, true, false),
            SourceStep::Sent
        );
        // The send in flight decides it.
        assert_eq!(
            step(&record(Copying), Gone, true, false, true, false),
            SourceStep::Wait
        );

        let mut got = record(Switching);
        got.sent = true;
        got.received = Some(
            serde_json::from_value(serde_json::json!({
                "instance_id": "i", "runner": { "kind": "detached" }, "boot_id": "b",
                "launched_generation": 1, "launched_at": 5,
            }))
            .unwrap(),
        );
        assert_eq!(
            step(&got, Gone, false, false, true, false),
            SourceStep::Release
        );
        got.source_released = true;
        assert_eq!(
            step(&got, Gone, false, false, true, false),
            SourceStep::Wait
        );

        let c = record(Cancelling);
        assert_eq!(
            step(&c, Here, true, false, true, false),
            SourceStep::CancelSend
        );
        assert_eq!(step(&c, Here, true, true, true, false), SourceStep::Wait);
        assert_eq!(step(&c, Here, false, false, true, false), SourceStep::Wait);
        assert_eq!(
            step(&c, Here, false, false, true, true),
            SourceStep::DropDelegates(true)
        );
        let mut released = c.clone();
        released.target_released = true;
        assert_eq!(
            step(&released, Here, false, false, true, false),
            SourceStep::DropDelegates(true)
        );
        // The guest left anyway: forwards (decision 14).
        assert_eq!(step(&c, Gone, false, false, true, false), SourceStep::Sent);

        // Failed by the leader: the delegates go once.
        let mut failed = record(Failed);
        failed.delegates.insert("d".into(), "x".into());
        assert_eq!(
            source_step(&failed, Here, false, false, false, true, false),
            SourceStep::DropDelegates(false)
        );
        assert_eq!(
            source_step(&failed, Here, false, false, true, true, false),
            SourceStep::Wait
        );
    }
}
