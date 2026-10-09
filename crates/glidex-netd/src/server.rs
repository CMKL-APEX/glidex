//! Request handling and socket serving for glidex-netd.

use crate::auth::{self, Peer};
use crate::proto::*;
use crate::store::{self, Store, BRIDGES, META, NAT, UPLINKS, VM_PORTS};
use glidex_ovs::ipmigrate;
use glidex_ovs::nic;
use glidex_ovs::uplink::{self, UplinkKind};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use crate::supervisor::Supervisor;
use glidex_ovs::bridge::{self, BridgeSpec};
use glidex_ovs::host::{self, ProbeOptions};
use glidex_ovs::install;
use glidex_ovs::names;
use glidex_ovs::nat::{self, NatSpec, NatState};
use glidex_ovs::net::Ipv4Net;
use glidex_ovs::vm_port::{self, VmPortSpec};
use glidex_ovs::{Exec, OvsError};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Config {
    /// Where the chassis keeps its OVN certificates.
    pub ovn_dir: PathBuf,
    pub run_dir: PathBuf,
    pub state_path: PathBuf,
    pub group: String,
    pub nat_supernet: Ipv4Net,
    pub probe: ProbeOptions,
    /// Time the caller has to `commit_uplink` after an IP migration.
    pub commit_window: Duration,
    /// Time the default gateway has to answer after an IP migration.
    pub gateway_check: Duration,
    /// Group of the admin socket ([`ADMIN_SOCKET_NAME`]).
    pub admin_group: String,
    /// Which ops each group may send on the full sockets.
    pub policy: auth::Policy,
    /// Router SNAT zones to meter; the same range as `ovn.snat_ct_zones`.
    pub ct_zones: (u16, u16),
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ovn_dir: PathBuf::from(glidex_ovs::ovn::CERT_DIR),
            run_dir: PathBuf::from(DEFAULT_RUN_DIR),
            state_path: PathBuf::from("/var/lib/glidex/netd.db"),
            group: "glidex".into(),
            nat_supernet: nat::DEFAULT_SUPERNET.parse().unwrap(),
            probe: ProbeOptions::default(),
            commit_window: Duration::from_secs(60),
            gateway_check: Duration::from_secs(20),
            admin_group: "glidex-admin".into(),
            policy: auth::default_policy("glidex", "glidex-admin"),
            ct_zones: (60000, 64999),
        }
    }
}

pub struct Netd {
    exec: Arc<dyn Exec>,
    supervisor: Arc<dyn Supervisor>,
    store: Store,
    pub config: Config,
    /// Serializes every host-changing operation.
    mutate: Mutex<()>,
    /// Conntrack counters of router zones, built on first use from the saved state.
    ct: Mutex<Option<Arc<glidex_ovs::ct_meter::CtMeter>>>,
    /// Whether the collector follows conntrack events itself (the daemon: yes, tests: no).
    ct_events: std::sync::atomic::AtomicBool,
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, OvsError> {
    serde_json::to_value(v).map_err(|e| OvsError::Io(e.to_string()))
}

impl Netd {
    pub fn new(
        exec: Arc<dyn Exec>,
        supervisor: Arc<dyn Supervisor>,
        config: Config,
    ) -> Result<Self, OvsError> {
        let store = Store::open(&config.state_path)?;
        if let Some(dir) = store.get::<std::path::PathBuf>(META, "ovs_bin_dir")? {
            exec.set_ovs_bin_dir(Some(dir));
        }
        Ok(Self {
            exec,
            supervisor,
            store,
            config,
            mutate: Mutex::new(()),
            ct: Mutex::new(None),
            ct_events: false.into(),
        })
    }

    /// The conntrack collector; the first call turns accounting on.
    fn ct_meter(&self) -> Result<Arc<glidex_ovs::ct_meter::CtMeter>, OvsError> {
        let mut g = self.ct.lock().unwrap();
        if let Some(m) = g.as_ref() {
            return Ok(m.clone());
        }
        if glidex_ovs::ct_meter::ensure_accounting(self.ex())? {
            self.store.put(META, "ct_acct_set_by_glidex", &true)?;
        }
        let saved: Option<String> = self.store.get(META, "ct_counters")?;
        let m = Arc::new(glidex_ovs::ct_meter::CtMeter::new(self.config.ct_zones, saved.as_deref()));
        if self.ct_events.load(std::sync::atomic::Ordering::Relaxed) {
            glidex_ovs::ct_meter::spawn_event_reader(m.clone());
        }
        *g = Some(m.clone());
        Ok(m)
    }

    /// The daemon follows conntrack `DESTROY` events from the first
    /// collection on, and from startup on a node that collected before.
    pub fn enable_ct_events(&self) {
        self.ct_events.store(true, std::sync::atomic::Ordering::Relaxed);
        if self.store.get::<String>(META, "ct_counters").ok().flatten().is_some() {
            if let Err(e) = self.ct_meter() {
                tracing::warn!("conntrack counters: {}", e);
            }
        }
    }

    fn ex(&self) -> &dyn Exec {
        self.exec.as_ref()
    }

    /// Handle one operation for `peer` (already authorized for its socket).
    pub fn handle(&self, op: Op, peer: &Peer) -> Result<Value, OvsError> {
        let _guard = op.is_mutating().then(|| self.mutate.lock().unwrap());
        match op {
            Op::Hello { .. } => to_value(HelloResult {
                protocol: PROTOCOL_VERSION,
                netd_version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            Op::Probe => to_value(host::probe(self.ex(), &self.config.probe)),
            Op::ListBridges => to_value(self.list_bridges()?),
            Op::ListNat => to_value(self.list_nat()?),
            Op::ListVmPorts => to_value(self.vm_ports()?),
            Op::ListUplinks => to_value(self.list_uplinks()?),
            Op::PortStats => to_value(glidex_ovs::stats::bridge_stats(self.ex())?),
            Op::NatCounters => to_value(glidex_ovs::nat_meter::nat_counters(self.ex(), &self.nats()?)?),
            Op::CtExternalCounters => {
                let m = self.ct_meter()?;
                // Persist before reporting, so what metering saw can't go backwards.
                to_value(glidex_ovs::ct_meter::collect(self.ex(), &m, |s| self.store.put(META, "ct_counters", &s.to_string()))?)
            }
            Op::EnsureUplink(args) => to_value(self.ensure_uplink(args.spec, args.confirm)?),
            Op::CommitUplink { bridge, name, token } => to_value(self.commit_uplink(&bridge, &name, &token)?),
            Op::DeleteUplink { bridge, name } => self.delete_uplink(&bridge, &name).map(|_| Value::Null),
            Op::InstallOvs(req) => {
                let report = install::install(self.ex(), &self.config.probe, &req)?;
                if report.method == "source" {
                    // Use the built OVS's tools from now on, also after restarts.
                    let dir = glidex_ovs::source_build::ovs_bin_dir();
                    self.store.put(META, "ovs_bin_dir", &dir)?;
                    self.exec.set_ovs_bin_dir(Some(dir));
                }
                to_value(report)
            }
            Op::InitDpdk(settings) => install::init_dpdk(self.ex(), &self.config.probe, &settings).map(|_| Value::Null),
            Op::EnsureBridge(spec) => to_value(self.ensure_bridge(spec)?),
            Op::DeleteBridge { name } => self.delete_bridge(&name).map(|_| Value::Null),
            Op::EnsureNat(spec) => to_value(self.ensure_nat(spec)?),
            Op::DeleteNat { bridge } => self.delete_nat(&bridge).map(|_| Value::Null),
            Op::AttachVmPort(spec) => to_value(self.attach(spec, peer.uid)?),
            Op::MoveVmPort(spec) => to_value(self.move_port(spec, peer.uid)?),
            Op::DetachVmPort { vm_id, nic_index } => {
                self.detach(&vm_id, nic_index, peer.uid).map(|_| Value::Null)
            }
            Op::ReleaseVm { vm_id } => self.release_vm(&vm_id, peer.uid).map(|_| Value::Null),
            Op::SyncVms { running } => to_value(self.sync_vms(&running)?),
            Op::EnsureOvnChassis(args) => {
                let caps = host::probe(self.ex(), &self.config.probe);
                if !caps.ovs_running {
                    return Err(OvsError::Unsupported { missing: vec!["ovs-vswitchd running".into()] });
                }
                let st = glidex_ovs::ovn::ensure_chassis(self.ex(), &args.spec, &args.certs, &self.config.ovn_dir)?;
                tracing::info!(chassis = %args.spec.chassis, "OVN chassis ensured");
                to_value(st)
            }
            Op::EnsureOvnCentral(args) => {
                let changed = glidex_ovs::ovn::ensure_central(self.ex(), &args.spec, &args.certs, &self.config.ovn_dir)?;
                to_value(serde_json::json!({ "changed": changed }))
            }
            Op::ForgetOvnMember(a) => {
                glidex_ovs::ovn::forget_member(self.ex(), &a.address, &a.chassis, a.server)?;
                tracing::info!(chassis = %a.chassis, "forgot OVN member");
                Ok(Value::Null)
            }
            Op::OvnStatus => to_value(glidex_ovs::ovn::status(self.ex())?),
            Op::LeaveOvn { confirm } => {
                glidex_ovs::ovn::leave(self.ex(), confirm, &self.config.ovn_dir)?;
                tracing::info!("left OVN");
                Ok(Value::Null)
            }
        }
    }

    // ---- bridges -------------------------------------------------------

    fn list_bridges(&self) -> Result<Vec<BridgeRecord>, OvsError> {
        self.store
            .list::<BridgeSpec>(BRIDGES)?
            .into_iter()
            .map(|(_, spec)| {
                let live = bridge::get(self.ex(), &spec.name)?;
                Ok(BridgeRecord { spec, live })
            })
            .collect()
    }

    fn ensure_bridge(&self, spec: BridgeSpec) -> Result<BridgeRecord, OvsError> {
        let caps = host::probe(self.ex(), &self.config.probe);
        if !caps.ovs_running {
            return Err(OvsError::Unsupported {
                missing: vec!["ovs-vswitchd running".into()],
            });
        }
        if spec.isolated && self.store.get::<NatState>(NAT, &spec.name)?.is_some() {
            return Err(OvsError::conflict(format!("bridge '{}' is a NAT network, not an isolated one", spec.name)));
        }
        if spec.isolated && self.uplinks()?.iter().any(|u| u.spec.bridge == spec.name) {
            return Err(OvsError::conflict(format!("bridge '{}' has uplinks; an isolated bridge can't", spec.name)));
        }
        let before = self.isolated_bridges()?;
        let live = bridge::ensure(self.ex(), &caps, &spec)?;
        // Stored after success only, so the store never claims more than OVS has.
        self.store.put(BRIDGES, &spec.name, &BridgeSpec { adopt: false, ..spec.clone() })?;
        if self.isolated_bridges()? != before {
            self.apply_firewall(&self.nats()?)?;
        }
        tracing::info!(bridge = %spec.name, "bridge ensured");
        Ok(BridgeRecord {
            spec,
            live: Some(live),
        })
    }

    fn delete_bridge(&self, name: &str) -> Result<(), OvsError> {
        if self.uplinks()?.iter().any(|u| u.spec.bridge == name) {
            return Err(OvsError::conflict(format!("bridge '{}' has uplinks; delete them first", name)));
        }
        if self.store.get::<NatState>(NAT, name)?.is_some() {
            return Err(OvsError::conflict(format!("bridge '{}' has a NAT network; delete it first", name)));
        }
        let users: Vec<String> = self
            .vm_ports()?
            .into_iter()
            .filter(|r| r.spec.bridge == name)
            .map(|r| r.port)
            .collect();
        if !users.is_empty() {
            return Err(OvsError::conflict(format!(
                "bridge '{}' still has VM ports: {}",
                name,
                users.join(", ")
            )));
        }
        let was_isolated = self.store.get::<BridgeSpec>(BRIDGES, name)?.is_some_and(|b| b.isolated);
        bridge::delete(self.ex(), name)?;
        self.store.delete(BRIDGES, name)?;
        if was_isolated {
            self.apply_firewall(&self.nats()?)?;
        }
        tracing::info!(bridge = %name, "bridge deleted");
        Ok(())
    }

    fn require_bridge(&self, name: &str) -> Result<BridgeSpec, OvsError> {
        self.store
            .get::<BridgeSpec>(BRIDGES, name)?
            .ok_or_else(|| OvsError::not_found(format!("glidex bridge '{}'", name)))
    }

    /// Bridges of isolated networks, sorted: fenced off in `inet glidex`.
    fn isolated_bridges(&self) -> Result<Vec<String>, OvsError> {
        let mut v: Vec<String> =
            self.store.list::<BridgeSpec>(BRIDGES)?.into_iter().filter(|(_, b)| b.isolated).map(|(n, _)| n).collect();
        v.sort();
        Ok(v)
    }

    /// Rebuild `inet glidex` for `nats` and the isolated bridges (security
    /// spec §8.4): one atomic script.
    fn apply_firewall(&self, nats: &[NatState]) -> Result<(), OvsError> {
        let isolated = self.isolated_bridges()?;
        let names: Vec<&str> = isolated.iter().map(String::as_str).collect();
        nat::apply_nft(self.ex(), nats, &names)
    }

    // ---- uplinks -------------------------------------------------------

    fn uplinks(&self) -> Result<Vec<UplinkRecord>, OvsError> {
        Ok(self.store.list::<UplinkRecord>(UPLINKS)?.into_iter().map(|(_, r)| r).collect())
    }

    fn list_uplinks(&self) -> Result<Vec<UplinkResult>, OvsError> {
        self.uplinks()?
            .into_iter()
            .map(|record| {
                Ok(UplinkResult {
                    phase: if record.pending.is_some() { UplinkPhase::PendingCommit } else { UplinkPhase::Active },
                    live: uplink::state(self.ex(), &record.spec.name)?,
                    record,
                    classification: None,
                })
            })
            .collect()
    }

    fn ensure_uplink(&self, spec: uplink::UplinkSpec, confirm: bool) -> Result<UplinkResult, OvsError> {
        spec.validate()?;
        if self.store.get::<NatState>(NAT, &spec.bridge)?.is_some() {
            return Err(OvsError::conflict(format!(
                "bridge '{}' is a NAT network; uplinks go on a separate bridge",
                spec.bridge
            )));
        }
        if self.require_bridge(&spec.bridge)?.isolated {
            return Err(OvsError::conflict(format!(
                "bridge '{}' is an isolated network; uplinks go on a separate bridge",
                spec.bridge
            )));
        }
        let key = store::uplink_key(&spec.bridge, &spec.name);
        if let Some(existing) = self.store.get::<UplinkRecord>(UPLINKS, &key)? {
            if existing.spec == spec {
                // Idempotent: already there (maybe still pending).
                return Ok(UplinkResult {
                    phase: if existing.pending.is_some() { UplinkPhase::PendingCommit } else { UplinkPhase::Active },
                    live: uplink::state(self.ex(), &spec.name)?,
                    record: existing,
                    classification: None,
                });
            }
            return Err(OvsError::conflict(format!("uplink '{}' exists with different settings", spec.name)));
        }
        // One uplink per NIC / PCI device, across all bridges.
        for other in self.uplinks()? {
            let same_nic = other.spec.ifname().is_some() && other.spec.ifname() == spec.ifname();
            let same_pci = matches!((&other.spec.kind, &spec.kind),
                (UplinkKind::Dpdk { pci: a, .. }, UplinkKind::Dpdk { pci: b, .. }) if a == b);
            if same_nic || same_pci {
                return Err(OvsError::conflict(format!("already used by uplink '{}' on '{}'", other.spec.name, other.spec.bridge)));
            }
        }
        let caps = host::probe(self.ex(), &self.config.probe);
        uplink::check(self.ex(), &caps, &spec)?;

        let mut warnings = Vec::new();
        let mut classification = None;
        let mut snapshot = None;
        if let Some(ifname) = spec.ifname() {
            let c = nic::classify(self.ex(), ifname)?;
            if c.in_use() {
                if !(spec.migrate_ip && confirm) {
                    return Err(OvsError::HostInterfaceInUse {
                        ifname: ifname.to_string(),
                        reasons: c.reasons.clone(),
                    });
                }
                if c.reasons.iter().any(|r| r == "dhcp_managed") {
                    warnings.push(format!(
                        "{} is DHCP-managed: the migrated address lasts until the lease expires",
                        ifname
                    ));
                }
                snapshot = Some(ipmigrate::snapshot(self.ex(), ifname)?);
            }
            classification = Some(c);
        }

        let mut orig_driver = None;
        if let UplinkKind::Dpdk { pci, .. } = &spec.kind {
            for dev in nic::pci_netdevs(self.ex(), pci) {
                let c = nic::classify(self.ex(), &dev)?;
                if c.in_use() && !confirm {
                    return Err(OvsError::HostInterfaceInUse { ifname: dev, reasons: c.reasons });
                }
            }
            orig_driver = nic::pci_driver(self.ex(), pci)?;
            if orig_driver.as_deref() != Some("vfio-pci") {
                nic::bind_driver(self.ex(), pci, "vfio-pci")?;
            }
        }
        let undo_driver = |netd: &Netd| {
            if let UplinkKind::Dpdk { pci, .. } = &spec.kind {
                if orig_driver.as_deref() != Some("vfio-pci") {
                    let _ = nic::restore_driver(netd.ex(), pci);
                }
            }
        };
        if let Err(e) = uplink::add_port(self.ex(), &spec, orig_driver.as_deref()) {
            undo_driver(self);
            return Err(e);
        }

        let mut record = UplinkRecord {
            spec: spec.clone(),
            orig_driver: orig_driver.clone(),
            snapshot: None,
            pending: None,
        };
        if let Some(snap) = snapshot.filter(|s| !s.addresses.is_empty()) {
            let rollback = |netd: &Netd, reason: String| -> OvsError {
                let _ = ipmigrate::restore(netd.ex(), &snap, &spec.bridge);
                let _ = uplink::del_port(netd.ex(), &spec.name);
                tracing::warn!(uplink = %spec.name, %reason, "IP migration rolled back");
                OvsError::MigrationRolledBack { reason }
            };
            if let Err(e) = ipmigrate::apply(self.ex(), &snap, &spec.bridge) {
                return Err(rollback(self, format!("applying failed: {}", e)));
            }
            if let Some(gw) = snap.default_gateway() {
                if !ipmigrate::gateway_reachable(self.ex(), gw, &spec.bridge, self.config.gateway_check) {
                    return Err(rollback(self, format!("default gateway {} did not answer through {}", gw, spec.bridge)));
                }
            }
            record.pending = Some(Pending {
                token: random_token(),
                deadline: unix_now() + self.config.commit_window.as_secs(),
            });
            record.snapshot = Some(snap);
        }
        self.store.put(UPLINKS, &key, &record)?;
        tracing::info!(uplink = %spec.name, bridge = %spec.bridge, pending = record.pending.is_some(), "uplink added");
        let mut live = uplink::state(self.ex(), &spec.name)?;
        live.warnings = warnings;
        Ok(UplinkResult {
            phase: if record.pending.is_some() { UplinkPhase::PendingCommit } else { UplinkPhase::Active },
            record,
            live,
            classification,
        })
    }

    fn commit_uplink(&self, bridge: &str, name: &str, token: &str) -> Result<UplinkResult, OvsError> {
        let key = store::uplink_key(bridge, name);
        let mut record = self
            .store
            .get::<UplinkRecord>(UPLINKS, &key)?
            .ok_or_else(|| OvsError::not_found(format!("uplink '{}' on '{}' (it may have been rolled back)", name, bridge)))?;
        match &record.pending {
            None => {}
            Some(p) if p.token == token => {
                record.pending = None;
                self.store.put(UPLINKS, &key, &record)?;
                tracing::info!(uplink = %name, "IP migration committed");
            }
            Some(_) => return Err(OvsError::invalid("commit token does not match")),
        }
        Ok(UplinkResult {
            phase: UplinkPhase::Active,
            live: uplink::state(self.ex(), name)?,
            record,
            classification: None,
        })
    }

    /// Remove an uplink, giving the NIC its IP config / driver back.
    fn remove_uplink(&self, record: &UplinkRecord) -> Result<(), OvsError> {
        uplink::del_port(self.ex(), &record.spec.name)?;
        if let Some(snap) = &record.snapshot {
            ipmigrate::restore(self.ex(), snap, &record.spec.bridge)?;
        }
        if let UplinkKind::Dpdk { pci, .. } = &record.spec.kind {
            if record.orig_driver.as_deref() != Some("vfio-pci") {
                nic::restore_driver(self.ex(), pci)?;
            }
        }
        self.store
            .delete(UPLINKS, &store::uplink_key(&record.spec.bridge, &record.spec.name))
    }

    fn delete_uplink(&self, bridge: &str, name: &str) -> Result<(), OvsError> {
        let Some(record) = self.store.get::<UplinkRecord>(UPLINKS, &store::uplink_key(bridge, name))? else {
            return Ok(());
        };
        self.remove_uplink(&record)?;
        tracing::info!(uplink = %name, bridge = %bridge, "uplink deleted");
        Ok(())
    }

    /// Roll back migrations whose commit deadline passed. Returns the
    /// uplinks rolled back. netd calls this every second.
    pub fn expire_pending(&self) -> Vec<String> {
        let _guard = self.mutate.lock().unwrap();
        let now = unix_now();
        let mut rolled = Vec::new();
        for record in self.uplinks().unwrap_or_default() {
            if record.pending.as_ref().is_some_and(|p| p.deadline <= now) {
                match self.remove_uplink(&record) {
                    Ok(()) => {
                        tracing::warn!(uplink = %record.spec.name, "not committed in time; rolled back");
                        rolled.push(record.spec.name.clone());
                    }
                    Err(e) => tracing::error!(uplink = %record.spec.name, error = %e, "rollback failed"),
                }
            }
        }
        rolled
    }

    // ---- NAT -----------------------------------------------------------

    fn nats(&self) -> Result<Vec<NatState>, OvsError> {
        Ok(self.store.list::<NatState>(NAT)?.into_iter().map(|(_, n)| n).collect())
    }

    fn list_nat(&self) -> Result<Vec<NatInfo>, OvsError> {
        Ok(self
            .nats()?
            .into_iter()
            .map(|state| NatInfo {
                dnsmasq_running: self.supervisor.running(&state.bridge),
                state,
            })
            .collect())
    }

    fn apply_nat(&self, state: &NatState, all: &[NatState]) -> Result<(), OvsError> {
        let ex = self.ex();
        ex.create_dir(Path::new(nat::DNSMASQ_RUN_DIR), 0o755)?;
        ex.create_dir(Path::new(nat::DNSMASQ_LEASE_DIR), 0o755)?;
        nat::apply_address(ex, state)?;
        if nat::ensure_ip_forward(ex)? {
            self.store.put(META, "ip_forward_set_by_glidex", &true)?;
        }
        self.apply_firewall(all)?;
        self.apply_meter(all);
        nat::apply_iptables(ex, all)?;
        nat::apply_iptables_input(ex, all)?;
        nat::write_dnsmasq_files(ex, state)?;
        self.stop_stale_dnsmasq(state);
        self.supervisor.start(&state.bridge, state.dnsmasq_args())
    }

    /// Bring the metering counters in line with the NAT state (D14 in
    /// spec/metering.md). Metering never fails a networking operation.
    fn apply_meter(&self, all: &[NatState]) {
        if let Err(e) = glidex_ovs::nat_meter::apply_meter(self.ex(), all) {
            tracing::warn!("NAT metering counters not updated: {}", e);
        }
    }

    /// A dnsmasq left over from a previous netd (e.g. killed without its
    /// service cgroup) still holds the gateway's sockets. Stop it — but
    /// only if its pid file is ours and its command line uses our config.
    fn stop_stale_dnsmasq(&self, state: &NatState) {
        if self.supervisor.running(&state.bridge) {
            return;
        }
        let Some(pid) = self
            .ex()
            .read_file(&state.pid_path())
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        else {
            return;
        };
        let cmdline = self
            .ex()
            .read_file(Path::new(&format!("/proc/{}/cmdline", pid)))
            .unwrap_or_default();
        let conf = state.conf_path().display().to_string();
        if !cmdline.split('\0').any(|arg| arg.ends_with(&conf)) {
            return;
        }
        tracing::warn!(bridge = %state.bridge, pid, "stopping stale dnsmasq");
        let pid = nix::unistd::Pid::from_raw(pid);
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
        for _ in 0..30 {
            if nix::sys::signal::kill(pid, None).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    }

    fn ensure_nat(&self, spec: NatSpec) -> Result<NatInfo, OvsError> {
        if self.require_bridge(&spec.bridge)?.isolated {
            return Err(OvsError::conflict(format!("bridge '{}' is an isolated network, not a NAT one", spec.bridge)));
        }
        let existing = self.store.get::<NatState>(NAT, &spec.bridge)?;
        let others: Vec<NatState> = self
            .nats()?
            .into_iter()
            .filter(|n| n.bridge != spec.bridge)
            .collect();
        let state = nat::plan(self.ex(), &spec, self.config.nat_supernet, &others, existing.as_ref())?;
        let mut all = others;
        all.push(state.clone());
        self.apply_nat(&state, &all)?;
        self.store.put(NAT, &state.bridge, &state)?;
        tracing::info!(bridge = %state.bridge, subnet = %state.subnet, "NAT network ensured");
        Ok(NatInfo {
            dnsmasq_running: self.supervisor.running(&state.bridge),
            state,
        })
    }

    fn delete_nat(&self, bridge: &str) -> Result<(), OvsError> {
        let Some(state) = self.store.get::<NatState>(NAT, bridge)? else {
            return Ok(());
        };
        if self.vm_ports()?.iter().any(|r| r.spec.bridge == bridge) {
            return Err(OvsError::conflict(format!("NAT network on '{}' still has VM ports", bridge)));
        }
        self.supervisor.stop(bridge);
        let rest: Vec<NatState> = self.nats()?.into_iter().filter(|n| n.bridge != bridge).collect();
        self.apply_firewall(&rest)?;
        self.apply_meter(&rest);
        nat::apply_iptables(self.ex(), &rest)?;
        nat::apply_iptables_input(self.ex(), &rest)?;
        nat::remove_address(self.ex(), &state)?;
        nat::remove_dnsmasq_files(self.ex(), &state)?;
        self.store.delete(NAT, bridge)?;
        tracing::info!(bridge = %bridge, "NAT network deleted");
        Ok(())
    }

    // ---- VM ports ------------------------------------------------------

    fn vm_ports(&self) -> Result<Vec<VmPortRecord>, OvsError> {
        Ok(self
            .store
            .list::<VmPortRecord>(VM_PORTS)?
            .into_iter()
            .map(|(_, r)| r)
            .collect())
    }

    fn attach(&self, spec: VmPortSpec, owner_uid: u32) -> Result<AttachResult, OvsError> {
        spec.validate()?;
        // br-int is OVN's, not a glidex bridge record (§11.3).
        if spec.ovn_lport.is_none() {
            self.require_bridge(&spec.bridge)?;
        }
        let key = store::vm_port_key(&spec.vm_id, spec.nic_index);
        if let Some(existing) = self.store.get::<VmPortRecord>(VM_PORTS, &key)? {
            if existing.spec.bridge != spec.bridge {
                return Err(OvsError::conflict(format!(
                    "{} is attached to bridge '{}'",
                    existing.port, existing.spec.bridge
                )));
            }
        }
        let caps = host::probe(self.ex(), &self.config.probe);
        let binding = vm_port::attach(self.ex(), &caps, &spec, owner_uid)?;
        let port = spec.port_name()?;

        let ipv4 = match self.store.get::<NatState>(NAT, &spec.bridge)? {
            Some(mut state) => {
                // Unreadable/missing lease file: nothing leased yet.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let leases = self
                    .ex()
                    .read_file(&state.lease_path())
                    .map(|l| nat::active_leases(&l, now))
                    .unwrap_or_default();
                let ip = state.allocate_avoiding(&spec.mac, &leases)?;
                nat::write_dnsmasq_files(self.ex(), &state)?;
                self.store.put(NAT, &state.bridge, &state)?;
                self.supervisor.reload(&state.bridge);
                self.apply_meter(&self.nats()?);
                Some(ip)
            }
            None => None,
        };
        let record = VmPortRecord {
            spec,
            port: port.clone(),
            binding: binding.clone(),
            ipv4,
            owner_uid,
        };
        self.store.put(VM_PORTS, &key, &record)?;
        tracing::info!(port = %port, vm = %record.spec.vm_id, ?ipv4, "VM port attached");
        Ok(AttachResult { port, binding, ipv4 })
    }

    /// §11.3 `move_vm_port`: unplug from the old bridge, keeping the tap or
    /// socket, then attach to the new one (a NAT bridge reserves an address).
    fn move_port(&self, spec: VmPortSpec, owner_uid: u32) -> Result<AttachResult, OvsError> {
        spec.validate()?;
        let key = store::vm_port_key(&spec.vm_id, spec.nic_index);
        let old = self.store.get::<VmPortRecord>(VM_PORTS, &key)?.ok_or_else(|| OvsError::not_found(format!("port {}", spec.port_name().unwrap_or_default())))?;
        Self::check_owner(&old, owner_uid)?;
        if old.spec.bridge != spec.bridge {
            vm_port::unplug(self.ex(), &spec.vm_id, spec.nic_index)?;
            self.store.delete(VM_PORTS, &key)?;
        }
        self.attach(spec, owner_uid)
    }

    /// A port belongs to the uid that attached it (security spec §8.2);
    /// root may detach anyone's.
    fn check_owner(record: &VmPortRecord, uid: u32) -> Result<(), OvsError> {
        if uid == 0 || record.owner_uid == uid {
            return Ok(());
        }
        Err(OvsError::not_owned(format!(
            "VM port {} belongs to uid {}",
            record.port, record.owner_uid
        )))
    }

    /// `detach_vm_port` from `uid`. Without a record (already detached, or
    /// an untracked port) there is no owner to check.
    fn detach(&self, vm_id: &str, nic_index: u8, uid: u32) -> Result<(), OvsError> {
        names::validate_vm_id(vm_id)?;
        if let Some(record) = self.store.get::<VmPortRecord>(VM_PORTS, &store::vm_port_key(vm_id, nic_index))? {
            Self::check_owner(&record, uid)?;
        }
        self.remove_port(vm_id, nic_index)
    }

    fn remove_port(&self, vm_id: &str, nic_index: u8) -> Result<(), OvsError> {
        vm_port::detach(self.ex(), vm_id, nic_index)?;
        // The NAT reservation is kept: the VM gets the same address next start.
        self.store.delete(VM_PORTS, &store::vm_port_key(vm_id, nic_index))?;
        tracing::info!(vm = %vm_id, nic = nic_index, "VM port detached");
        Ok(())
    }

    fn release_vm(&self, vm_id: &str, uid: u32) -> Result<(), OvsError> {
        names::validate_vm_id(vm_id)?;
        let mut records = Vec::new();
        for nic in 0..names::MAX_NICS {
            if let Some(record) = self.store.get::<VmPortRecord>(VM_PORTS, &store::vm_port_key(vm_id, nic))? {
                records.push(record);
            }
        }
        // All or nothing: check every port before detaching any.
        for record in &records {
            Self::check_owner(record, uid)?;
        }
        for record in &records {
            self.remove_port(vm_id, record.spec.nic_index)?;
        }
        let macs: Vec<String> = (0..names::MAX_NICS)
            .filter_map(|nic| names::mac_address(vm_id, nic).ok())
            .collect();
        for mut state in self.nats()? {
            let before = state.reservations.len();
            for mac in &macs {
                state.release(mac);
            }
            if state.reservations.len() != before {
                nat::write_dnsmasq_files(self.ex(), &state)?;
                self.store.put(NAT, &state.bridge, &state)?;
                self.supervisor.reload(&state.bridge);
            }
        }
        self.apply_meter(&self.nats()?);
        Ok(())
    }

    // ---- reconciliation ------------------------------------------------

    /// Detach ports of VMs that aren't running; report untracked glidex ports.
    /// Not owner-scoped: it reconciles the whole host, and it is how ports
    /// left by an earlier control-plane uid get cleaned up.
    fn sync_vms(&self, running: &[String]) -> Result<ReconcileReport, OvsError> {
        let mut report = ReconcileReport::default();
        for r in self.vm_ports()? {
            if !running.contains(&r.spec.vm_id) {
                match self.remove_port(&r.spec.vm_id, r.spec.nic_index) {
                    Ok(()) => report.detached.push(r.port),
                    Err(e) => report.errors.push(format!("{}: {}", r.port, e)),
                }
            }
        }
        let known: Vec<String> = self.vm_ports()?.into_iter().map(|r| r.port).collect();
        if let Ok(owned) = vm_port::list_owned(self.ex()) {
            for (_, _, port) in owned {
                if !known.contains(&port) {
                    report.orphans.push(port);
                }
            }
        }
        Ok(report)
    }

    /// Bring the host back to the stored state (netd start).
    pub fn reconcile_startup(&self) -> ReconcileReport {
        let _guard = self.mutate.lock().unwrap();
        let mut report = ReconcileReport::default();
        let ex = self.ex();
        if let Err(e) = ex.create_dir(&self.config.run_dir.join("vhost"), 0o2770) {
            report.errors.push(format!("vhost dir: {}", e));
        }
        let caps = host::probe(ex, &self.config.probe);
        if !caps.ovs_running {
            if !self.store.list::<BridgeSpec>(BRIDGES).unwrap_or_default().is_empty() {
                report.errors.push("ovs-vswitchd is not running; bridges not reconciled".into());
            }
            return report;
        }
        for (_, spec) in self.store.list::<BridgeSpec>(BRIDGES).unwrap_or_default() {
            match bridge::ensure(ex, &caps, &spec) {
                Ok(_) => report.repaired.push(format!("bridge {}", spec.name)),
                Err(e) => report.errors.push(format!("bridge {}: {}", spec.name, e)),
            }
        }
        for record in self.uplinks().unwrap_or_default() {
            if record.pending.is_some() {
                // netd stopped during the commit window: nobody committed.
                match self.remove_uplink(&record) {
                    Ok(()) => report.repaired.push(format!("rolled back uncommitted uplink {}", record.spec.name)),
                    Err(e) => report.errors.push(format!("uplink {}: {}", record.spec.name, e)),
                }
                continue;
            }
            match self.reapply_uplink(&record) {
                Ok(()) => report.repaired.push(format!("uplink {}", record.spec.name)),
                Err(e) => report.errors.push(format!("uplink {}: {}", record.spec.name, e)),
            }
        }
        let nats = self.nats().unwrap_or_default();
        for state in &nats {
            match self.apply_nat(state, &nats) {
                Ok(()) => report.repaired.push(format!("nat {}", state.bridge)),
                Err(e) => report.errors.push(format!("nat {}: {}", state.bridge, e)),
            }
        }
        // Without NAT networks nothing above rebuilt the table: fence the
        // isolated bridges here.
        if nats.is_empty() && !self.isolated_bridges().unwrap_or_default().is_empty() {
            match self.apply_firewall(&[]) {
                Ok(()) => report.repaired.push("isolated bridges fenced".into()),
                Err(e) => report.errors.push(format!("isolated bridges: {}", e)),
            }
        }
        if let Ok(owned) = bridge::list_owned(ex) {
            let known: Vec<String> = self
                .store
                .list::<BridgeSpec>(BRIDGES)
                .unwrap_or_default()
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            for b in owned {
                if !known.contains(&b.name) {
                    report.orphans.push(format!("bridge {}", b.name));
                }
            }
        }
        report
    }
}

impl Netd {
    /// Startup: DPDK NICs back on vfio-pci (spec decision 5), port present,
    /// and IP migrations re-applied (the host's own config returns on boot).
    fn reapply_uplink(&self, record: &UplinkRecord) -> Result<(), OvsError> {
        if let UplinkKind::Dpdk { pci, .. } = &record.spec.kind {
            if nic::pci_driver(self.ex(), pci)?.as_deref() != Some("vfio-pci") {
                nic::bind_driver(self.ex(), pci, "vfio-pci")?;
            }
        }
        uplink::add_port(self.ex(), &record.spec, record.orig_driver.as_deref())?;
        if let Some(snap) = &record.snapshot {
            ipmigrate::apply(self.ex(), snap, &record.spec.bridge)?;
        }
        Ok(())
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 128-bit random hex token for commit confirmation.
fn random_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

// ---- socket serving ------------------------------------------------------

/// Which socket a connection arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// `netd.sock`: root and the `group` (the control plane).
    Full,
    /// `netd-admin.sock`: root and the `admin_group`. Same ops as `Full`.
    Admin,
    Status,
}

impl Access {
    /// Socket file name under the run directory.
    pub fn socket_name(self) -> &'static str {
        match self {
            Access::Full => FULL_SOCKET_NAME,
            Access::Admin => ADMIN_SOCKET_NAME,
            Access::Status => STATUS_SOCKET_NAME,
        }
    }
}

fn write_line(stream: &mut UnixStream, resp: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_string(resp).unwrap_or_default();
    line.push('\n');
    stream.write_all(line.as_bytes())
}

/// Serve one connection until it closes. `group_gid` is the group a
/// peer must be in to connect to a full socket (`group` for `Full`,
/// `admin_group` for `Admin`; unused for `Status`).
pub fn serve_connection(netd: &Netd, stream: UnixStream, access: Access, group_gid: Option<u32>) {
    let peer = match auth::peer(&stream) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "could not read peer credentials");
            return;
        }
    };
    if access != Access::Status && !auth::authorized(&peer, group_gid) {
        let group = if access == Access::Admin { &netd.config.admin_group } else { &netd.config.group };
        tracing::warn!(uid = peer.uid, pid = peer.pid, socket = access.socket_name(), "rejected peer not in group {}", group);
        return;
    }
    // Policy groups the peer is in, looked up once per connection.
    let policy_groups = if access == Access::Status || peer.uid == 0 {
        Vec::new()
    } else {
        auth::policy_groups(&netd.config.policy, &peer)
    };
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    let mut said_hello = false;
    loop {
        let mut line = String::new();
        match reader.by_ref().take(MAX_LINE as u64 + 1).read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(n) if n > MAX_LINE => {
                let _ = write_line(&mut writer, &Response::err(0, ErrorBody::new("protocol_error", "request too large")));
                return;
            }
            Ok(_) => {}
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_line(&mut writer, &Response::err(0, ErrorBody::new("protocol_error", e.to_string())));
                continue;
            }
        };
        let resp = if !said_hello && !matches!(req.op, Op::Hello { .. }) {
            Response::err(req.id, ErrorBody::new("protocol_error", "first request must be hello"))
        } else if access == Access::Status && !req.op.is_status() {
            Response::err(
                req.id,
                ErrorBody::new("permission_denied", format!("'{}' is not available on the status socket", req.op.name())),
            )
        } else if access != Access::Status
            && !auth::allows(&netd.config.policy, peer.uid, req.op.name(), |g| policy_groups.iter().any(|m| m == g))
        {
            tracing::warn!(uid = peer.uid, op = req.op.name(), "denied by policy");
            Response::err(
                req.id,
                ErrorBody::new("permission_denied", format!("netd policy does not allow '{}' for uid {}", req.op.name(), peer.uid)),
            )
        } else {
            if let Op::Hello { protocol } = &req.op {
                if *protocol != PROTOCOL_VERSION {
                    let _ = write_line(
                        &mut writer,
                        &Response::err(req.id, ErrorBody::new("protocol_error", format!("unsupported protocol {}", protocol))),
                    );
                    return;
                }
                said_hello = true;
            }
            if req.op.is_mutating() {
                // on_behalf_of is the caller's claim, logged for audit only.
                tracing::info!(
                    uid = peer.uid,
                    op = req.op.name(),
                    args = %serde_json::to_string(&req.op).unwrap_or_default(),
                    on_behalf_of = %req.on_behalf_of.as_ref().and_then(|o| serde_json::to_string(o).ok()).unwrap_or_else(|| "-".into()),
                    "request"
                );
            }
            match netd.handle(req.op, &peer) {
                Ok(v) => Response::ok(req.id, v),
                Err(e) => Response::err(req.id, ErrorBody::from(&e)),
            }
        };
        if write_line(&mut writer, &resp).is_err() {
            return;
        }
    }
}

/// Bind a socket, replacing a stale one, with the given mode and group.
pub fn bind(path: &Path, mode: u32, gid: Option<u32>) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    if let Some(gid) = gid {
        // Only root can give the socket to another group; tests run unprivileged.
        let _ = nix::unistd::chown(path, None, Some(nix::unistd::Gid::from_raw(gid)));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

/// Accept connections on `listener` forever, one thread per connection.
pub fn serve(netd: Arc<Netd>, listener: UnixListener, access: Access, group_gid: Option<u32>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let netd = netd.clone();
                std::thread::spawn(move || serve_connection(&netd, stream, access, group_gid));
            }
            Err(e) => tracing::warn!(error = %e, "accept failed"),
        }
    }
}

pub fn status_json(report: &ReconcileReport) -> Value {
    json!(report)
}
