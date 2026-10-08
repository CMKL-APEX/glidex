//! The controllers (spec/reconciliation.md §9): level-triggered loops that
//! observe the host and take the smallest step toward each object's spec.
//!
//! One [`queue::WorkQueue`] feeds `reconcile.workers` workers. Keys come
//! from API spec writes, exit watches (a `pidfd` per live hypervisor and
//! shim), and a periodic resync of every object, so a missed event only
//! delays convergence until the next resync.

pub mod disk;
pub mod image;
pub mod image_cache;
pub mod network;
pub mod ovn;
pub mod placement;
pub mod queue;
pub mod startup;
pub mod vm;

use crate::config::{Config, VmRunnerKind};
use crate::models::HostBootPolicy;
use crate::state::VmManager;
use queue::Key;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// `reconcile` and `console` settings (spec §14).
#[derive(Debug, Clone)]
pub struct Settings {
    pub runner: VmRunnerKind,
    pub workers: usize,
    pub resync_secs: u64,
    pub on_host_boot: HostBootPolicy,
    pub host_shutdown_grace_secs: u64,
    pub log_max_bytes: u64,
    /// How long a hypervisor gets to answer on its socket (§8.1).
    pub ready_timeout_secs: u64,
    /// The shim's block-counter poll for the exit snapshot (metering D12).
    pub meter_poll_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings::from_config(&Config::default())
    }
}

impl Settings {
    pub fn from_config(cfg: &Config) -> Self {
        Settings {
            runner: cfg.reconcile.vm_runner,
            workers: cfg.reconcile.workers,
            resync_secs: cfg.reconcile.resync_secs,
            on_host_boot: cfg.reconcile.on_host_boot,
            host_shutdown_grace_secs: cfg.reconcile.host_shutdown_grace_secs,
            log_max_bytes: cfg.console.log_max_bytes,
            ready_timeout_secs: 30,
            meter_poll_secs: if cfg.metering.enabled { cfg.metering.sample_secs } else { 0 },
        }
    }
}

impl VmManager {
    /// Start the controllers of the roles this process runs (spec/clustering.md
    /// D4): the node role (workers, resync, exit watches) and the server role
    /// (cluster controllers). Idempotent.
    pub fn start_controllers(&self) {
        if self.controllers_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let roles = self.roles();
        let mut tasks = Vec::new();
        if roles.node {
            tasks.extend(self.node_role_tasks());
        }
        if roles.server {
            tasks.extend(self.server_role_tasks());
        }
        self.tasks.lock().unwrap().extend(tasks);
    }

    /// The node role: VM, disk and image controllers for the objects on this
    /// host (§8.1), as they have always run.
    fn node_role_tasks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        let me = self.arc();
        let mut tasks = Vec::new();
        for _ in 0..self.settings().workers.max(1) {
            let m = me.clone();
            tasks.push(tokio::spawn(async move { m.worker().await }));
        }
        let m = me.clone();
        tasks.push(tokio::spawn(async move { m.resync_loop().await }));
        // This node as an OVN chassis (and a server's part of ovn-central).
        let m = me.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                m.ensure_ovn_node().await;
            }
        }));
        // What this node reports about itself, refreshed every ten minutes.
        let m = me.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(600)).await;
                let m2 = m.clone();
                let _ = tokio::task::spawn_blocking(move || m2.report_self()).await;
            }
        }));
        tasks.push(tokio::spawn(async move {
            for key in me.all_keys().await {
                me.queue.add(key);
            }
            let ids: Vec<String> = me.vms.read().await.values().filter(|v| me.is_local(v)).map(|v| v.id.clone()).collect();
            for id in ids {
                me.watch_instance(&id).await;
            }
        }));
        tasks
    }

    /// The server role: controllers that run only where the API runs. The
    /// scheduler (C4), node lifecycle (C3) and network controller (C5) are
    /// added here, each gated on holding leadership.
    fn server_role_tasks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        // The scheduler: places VMs that wait for a node, again as nodes and
        // capacity change (a pass is cheap when nothing waits).
        let me = self.arc();
        let m2 = me.clone();
        vec![
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    me.place_pending().await;
                }
            }),
            // The cluster network controller (§11): OVN's northbound database.
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    m2.reconcile_ovn().await;
                }
            }),
        ]
    }

    /// Every object the controllers own.
    async fn all_keys(&self) -> Vec<Key> {
        let me = self.local_node_id();
        let mut keys: Vec<Key> = self.vms.read().await.values().filter(|v| self.is_local(v)).map(|v| Key::Vm(v.id.clone())).collect();
        keys.extend(self.images.list_disks().into_iter().filter(|d| self.is_local_disk(d)).map(|d| Key::Disk(d.id)));
        keys.extend(self.images.list_images().into_iter().map(|i| Key::Image(i.id)));
        if let Ok(nets) = self.networks.list() {
            keys.extend(nets.into_iter().filter(|n| n.node.as_deref().is_none_or(|x| x == me)).map(|n| Key::Network(n.name)));
        }
        keys
    }

    /// Stop the controllers (workers finish their current round first is
    /// not guaranteed: every step is safe to interrupt, G5). Instances keep
    /// running. Used by tests that restart the control plane in-process.
    pub async fn stop_controllers(&self) {
        let tasks: Vec<_> = std::mem::take(&mut *self.tasks.lock().unwrap());
        for t in &tasks {
            t.abort();
        }
        for t in tasks {
            let _ = t.await;
        }
        self.watched.lock().unwrap().clear();
        self.controllers_started.store(false, Ordering::SeqCst);
    }

    async fn worker(self: Arc<Self>) {
        loop {
            let key = self.queue.next().await;
            let result = match &key {
                Key::Vm(id) => self.reconcile_vm(id).await,
                Key::Disk(id) => self.reconcile_disk(id).await,
                Key::Image(id) => self.reconcile_image(id).await,
                Key::Network(name) => self.reconcile_network(name).await,
            };
            match result {
                Ok(next) => {
                    self.queue.succeeded(&key);
                    if let Some(after) = next {
                        self.queue.add_after(key.clone(), after);
                    }
                }
                Err(e) => {
                    let after = self.queue.failed(&key);
                    tracing::warn!(kind = key.kind(), id = key.id(), retry_in = ?after, "reconcile failed: {}", e);
                    self.queue.add_after(key.clone(), after);
                }
            }
            self.queue.done(&key);
        }
    }

    /// Every `resync_secs`: queue every object, spread over the interval;
    /// re-sync netd's VM ports when netd has restarted (D16).
    async fn resync_loop(self: Arc<Self>) {
        loop {
            let period = Duration::from_secs(self.settings().resync_secs.max(1));
            let keys = self.all_keys().await;
            let step = period / (keys.len().max(1) as u32 + 1);
            for (i, key) in keys.into_iter().enumerate() {
                self.queue.add_after(key, step * (i as u32 + 1));
            }
            tokio::time::sleep(period).await;
            let now = self.netd.socket_identity();
            let changed = {
                let mut seen = self.netd_seen.lock().unwrap();
                let changed = now.is_some() && *seen != now;
                *seen = now;
                changed
            };
            if changed {
                tracing::info!("glidex-netd restarted; re-syncing VM ports");
                self.sync_netd_ports().await;
            }
        }
    }

    /// Tell netd which VMs own ports: every VM with `status.nics` (§9.4
    /// step 4). Held under `ports_lock`, so it never races a VM that is
    /// adding a port.
    ///
    /// Not sent when no VM here uses networks: netd's sync is host-wide,
    /// and a control plane with nothing to keep must not tell it to drop
    /// everything (e.g. a scratch instance next to the real one).
    pub(crate) async fn sync_netd_ports(&self) {
        let _ports = self.ports_lock.lock().await;
        let (running, networked) = {
            let vms = self.vms.read().await;
            let running: Vec<String> = vms.values().filter(|vm| self.is_local(vm) && !vm.status.nics.is_empty()).map(|vm| vm.id.clone()).collect();
            let networked = vms.values().any(|vm| self.is_local(vm) && (!vm.config().networks.is_empty() || !vm.status.nics.is_empty()));
            (running, networked)
        };
        *self.netd_seen.lock().unwrap() = self.netd.socket_identity();
        if !networked {
            return;
        }
        let netd = self.netd.clone();
        let res = tokio::task::spawn_blocking(move || {
            netd.call::<glidex_netd::proto::ReconcileReport>(glidex_netd::proto::Op::SyncVms { running })
        })
        .await;
        match res {
            Ok(Ok(report)) if !report.detached.is_empty() || !report.orphans.is_empty() => {
                tracing::info!(detached = ?report.detached, orphans = ?report.orphans, "netd sync");
            }
            Ok(Ok(_)) => {}
            Ok(Err(crate::network::NetError::Unavailable(_))) => {}
            Ok(Err(e)) => tracing::warn!("glidex-netd sync failed: {}", e),
            Err(e) => tracing::warn!("glidex-netd sync task failed: {}", e),
        }
    }

    /// Watch the live processes of `vm_id`'s instance: when one exits,
    /// queue the VM (§9.2). One watch per (VM, pid).
    pub(crate) async fn watch_instance(&self, vm_id: &str) {
        let Some(vm) = self.vm(vm_id).await else { return };
        let Some(inst) = vm.status.instance else { return };
        for (pid, start) in [inst.shim_pid.zip(inst.shim_starttime), inst.hypervisor_pid.zip(inst.hypervisor_starttime)]
            .into_iter()
            .flatten()
        {
            if !self.watched.lock().unwrap().insert((vm_id.to_string(), pid)) {
                continue;
            }
            let me = self.arc();
            let id = vm_id.to_string();
            let task = tokio::spawn(async move {
                wait_for_exit(pid, start).await;
                me.watched.lock().unwrap().remove(&(id.clone(), pid));
                me.queue.add(Key::Vm(id));
            });
            let mut tasks = self.tasks.lock().unwrap();
            tasks.retain(|t| !t.is_finished());
            tasks.push(task);
        }
    }
}

/// Resolve when process `pid` (started at `start`) exits. Uses a pidfd
/// (works for non-children); falls back to polling.
async fn wait_for_exit(pid: u32, start: u64) {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd >= 0 {
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        // The pid could have been reused between our check and the open.
        if !glidex_vm_shim::util::same_process(pid, start).unwrap_or(false) {
            return;
        }
        if let Ok(afd) = tokio::io::unix::AsyncFd::with_interest(PidFd(fd), tokio::io::Interest::READABLE) {
            let _ = afd.readable().await;
            return;
        }
    }
    while glidex_vm_shim::util::same_process(pid, start).unwrap_or(false) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

struct PidFd(OwnedFd);

impl AsRawFd for PidFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pidfd_wait_returns_when_the_process_exits() {
        let mut child = std::process::Command::new("sleep").arg("0.3").spawn().unwrap();
        let pid = child.id();
        let start = glidex_vm_shim::util::proc_starttime(pid).unwrap().unwrap();
        let reaper = std::thread::spawn(move || child.wait());
        let t = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), wait_for_exit(pid, start)).await.unwrap();
        assert!(t.elapsed() >= Duration::from_millis(200));
        reaper.join().unwrap().unwrap();
        // An already-gone process returns at once.
        tokio::time::timeout(Duration::from_millis(500), wait_for_exit(pid, start)).await.unwrap();
    }
}
