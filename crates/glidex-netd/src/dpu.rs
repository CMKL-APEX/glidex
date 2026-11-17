//! The DPU backend (spec/31-dpu.md §6, §7, §10.1): `dpu.json`, the
//! backend choice, the host's side of a DPU port (the virtio-net PF's VFs
//! and their `vfio-pci` binding) and the management link's host address.
//! The DPU's side is the agent's, reached through [`AgentApi`].

use glidex_dpu::{Backend, DpuCapabilities};
use glidex_dpu_agent::proto::AgentApi;
use glidex_dpu_agent::{AgentError, AgentOp};
use glidex_ovs::OvsError;
use glidex_ovs::exec::{Cmd, Exec, Program};
use glidex_ovs::vm_port::PortBackend;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DPU_CONFIG: &str = "/etc/glidex/dpu.json";

/// How long a health answer is reused, so a launch burst probes once (§6).
pub const HEALTH_TTL: Duration = Duration::from_secs(5);

/// The virtio-net PF's PCI ids on the host.
const VIRTIO_NET_VENDOR: &str = "0x1af4";
const VIRTIO_NET_DEVICE: &str = "0x1041";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkConfig {
    /// `tmfifo_net0` on the test host.
    pub iface: String,
    /// The host side's address, `<address>/<len>`.
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsPaths {
    pub ca: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bridges {
    #[serde(default = "default_br_int")]
    pub int: String,
    #[serde(default = "default_br_int")]
    pub representor_bridge: String,
}

fn default_br_int() -> String {
    "br-int".into()
}

impl Default for Bridges {
    fn default() -> Self {
        Self {
            int: default_br_int(),
            representor_bridge: default_br_int(),
        }
    }
}

fn default_mode() -> String {
    "vfio".into()
}

fn default_port_timeout() -> u64 {
    30
}

/// `dpu.json`'s `ovn` (§10.1): the DPU chassis's own settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpuOvnConfig {
    /// The DPU's underlay address, its `ovn-encap-ip`.
    pub encap_ip: std::net::IpAddr,
}

/// One DPU of `dpu.json` (§10.1). Keys this milestone doesn't use
/// (`underlay`, `rshim`, `reinit`, …) are accepted and left for later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpuConfig {
    pub id: String,
    /// `tcp://<dpu-mgmt-address>:<port>`.
    pub agent: String,
    #[serde(default)]
    pub link: Option<LinkConfig>,
    pub tls: TlsPaths,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub bridges: Bridges,
    #[serde(default = "default_port_timeout")]
    pub port_timeout_secs: u64,
    /// The virtio-net PF on the host (`0000:61:00.1`); found by its PCI ids
    /// when left out.
    #[serde(default)]
    pub host_pf: Option<String>,
    /// The chassis's settings; without them the agent's own `encap_ip`.
    #[serde(default)]
    pub ovn: Option<DpuOvnConfig>,
}

#[derive(Debug, Deserialize)]
struct DpuFile {
    dpus: Vec<DpuConfig>,
}

fn valid_cidr(s: &str) -> bool {
    s.split_once('/').is_some_and(|(ip, len)| {
        ip.parse::<std::net::IpAddr>().is_ok() && len.parse::<u8>().is_ok_and(|l| l <= 128)
    })
}

/// Parse `dpu.json`. v1 has exactly one DPU per host (§10.1).
pub fn parse(text: &[u8]) -> Result<DpuConfig, String> {
    let file: DpuFile = serde_json::from_slice(text).map_err(|e| e.to_string())?;
    let [dpu] = <[DpuConfig; 1]>::try_from(file.dpus)
        .map_err(|d| format!("{} DPUs; this version supports exactly one", d.len()))?;
    if !dpu
        .agent
        .strip_prefix("tcp://")
        .is_some_and(|a| a.parse::<std::net::SocketAddr>().is_ok())
    {
        return Err(format!(
            "agent `{}` must be tcp://<address>:<port>",
            dpu.agent
        ));
    }
    if dpu.mode != "vfio" {
        return Err(format!(
            "mode `{}`: only `vfio` is supported (§5.2)",
            dpu.mode
        ));
    }
    if let Some(l) = &dpu.link {
        // Kernel names, `_` included (`tmfifo_net0`).
        let ok = (1..=glidex_ovs::names::MAX_IFNAME).contains(&l.iface.len())
            && l.iface
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'));
        if !ok {
            return Err(format!("link iface `{}`", l.iface));
        }
        if !valid_cidr(&l.address) {
            return Err(format!(
                "link address `{}` must be <address>/<len>",
                l.address
            ));
        }
    }
    Ok(dpu)
}

/// `dpu.json` at `path`: missing means no DPU; unreadable or invalid is an
/// error, which stops netd (as `netd.json` does).
pub fn load(path: &Path) -> Result<Option<DpuConfig>, String> {
    match std::fs::read(path) {
        Ok(b) => parse(&b)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

// ---- backend choice (§6) ------------------------------------------------

/// The §6 table. `health` is the DPU's (no DPU: unhealthy); `free_vfs` the
/// VFs left. `Ok(Host)` from `Auto` is a fallback.
pub fn choose(
    policy: PortBackend,
    health: &Result<(), String>,
    free_vfs: u16,
) -> Result<Backend, OvsError> {
    match (policy, health) {
        (PortBackend::Host, _) => Ok(Backend::Host),
        (PortBackend::Dpu, Err(reason)) => Err(OvsError::DpuUnavailable {
            reason: reason.clone(),
        }),
        (PortBackend::Dpu, Ok(())) if free_vfs == 0 => Err(OvsError::DpuCapacity {
            message: "no free VF on the DPU".into(),
        }),
        (PortBackend::Dpu, Ok(())) => Ok(Backend::Dpu),
        (PortBackend::Auto, Ok(())) if free_vfs > 0 => Ok(Backend::Dpu),
        (PortBackend::Auto, _) => Ok(Backend::Host),
    }
}

/// An agent error as netd reports it.
pub fn agent_error(e: AgentError) -> OvsError {
    match e {
        AgentError::Unavailable(m) | AgentError::Protocol(m) => {
            OvsError::DpuUnavailable { reason: m }
        }
        AgentError::Remote(b) => match b.code.as_str() {
            "dpu_port_timeout" => OvsError::DpuPortTimeout { message: b.message },
            "dpu_capacity" => OvsError::DpuCapacity { message: b.message },
            "invalid_argument" => OvsError::invalid(b.message),
            "not_owned" => OvsError::not_owned(b.message),
            _ => OvsError::Io(format!("DPU agent: {} ({})", b.message, b.code)),
        },
    }
}

pub struct DpuBackend {
    pub config: DpuConfig,
    agent: Arc<dyn AgentApi>,
    health: Mutex<Option<(Instant, Result<DpuCapabilities, String>)>>,
}

impl DpuBackend {
    pub fn new(config: DpuConfig, agent: Arc<dyn AgentApi>) -> Self {
        Self {
            config,
            agent,
            health: Mutex::new(None),
        }
    }

    pub fn call<T: serde::de::DeserializeOwned>(&self, op: AgentOp) -> Result<T, OvsError> {
        let v: Value = self.agent.call(op).map_err(agent_error)?;
        serde_json::from_value(v).map_err(|e| OvsError::Io(format!("DPU agent answer: {e}")))
    }

    /// The DPU's capabilities, or why it can't be used (§6: the agent
    /// answers, OVS-DOCA is initialized, the controller serves a PF;
    /// whether a VF is free is the caller's). Cached [`HEALTH_TTL`].
    pub fn health(&self) -> Result<DpuCapabilities, String> {
        let mut cached = self.health.lock().unwrap();
        if let Some((at, h)) = cached.as_ref()
            && at.elapsed() < HEALTH_TTL
        {
            return h.clone();
        }
        let h = self
            .call::<DpuCapabilities>(AgentOp::Probe)
            .map_err(|e| e.to_string())
            .and_then(|caps| {
                if caps.missing.is_empty() {
                    Ok(caps)
                } else {
                    Err(format!("the DPU lacks {}", caps.missing.join(", ")))
                }
            });
        *cached = Some((Instant::now(), h.clone()));
        h
    }

    /// Forget the cached health (after a failure that says it changed).
    pub fn forget_health(&self) {
        *self.health.lock().unwrap() = None;
    }

    /// The virtio-net PF on the host.
    pub fn host_pf(&self, exec: &dyn Exec) -> Result<String, OvsError> {
        if let Some(pf) = &self.config.host_pf {
            return Ok(pf.clone());
        }
        let root = Path::new(PCI_DEVICES);
        exec.list_dir(root)
            .into_iter()
            .find(|d| {
                let read = |f: &str| exec.read_file(&root.join(d).join(f)).unwrap_or_default();
                read("vendor").trim() == VIRTIO_NET_VENDOR
                    && read("device").trim() == VIRTIO_NET_DEVICE
                    && !exec.exists(&root.join(d).join("physfn"))
            })
            .ok_or_else(|| OvsError::DpuUnavailable {
                reason: "no virtio-net PF on the host".into(),
            })
    }
}

// ---- the host side of a DPU port (§7.1 steps 3 and 7) --------------------

pub const PCI_DEVICES: &str = "/sys/bus/pci/devices";
const DRIVERS_PROBE: &str = "/sys/bus/pci/drivers_probe";

fn dev(bdf: &str) -> PathBuf {
    Path::new(PCI_DEVICES).join(bdf)
}

fn read_trim(exec: &dyn Exec, path: &Path) -> Result<String, OvsError> {
    exec.read_file(path).map(|s| s.trim().to_string())
}

fn driver(exec: &dyn Exec, bdf: &str) -> Option<String> {
    exec.link_name(&dev(bdf).join("driver"))
}

/// Bind `bdf` to `want` (driver_override, unbind, drivers_probe), as the
/// DPDK uplink does. Returns the driver it had.
fn bind(exec: &dyn Exec, bdf: &str, want: &str) -> Result<Option<String>, OvsError> {
    let had = driver(exec, bdf);
    if had.as_deref() == Some(want) {
        return Ok(had);
    }
    exec.write_file(&dev(bdf).join("driver_override"), want.as_bytes())?;
    if let Some(d) = &had {
        exec.write_file(&dev(bdf).join("driver").join("unbind"), bdf.as_bytes())?;
        tracing::info!(%bdf, from = %d, to = want, "rebinding");
    }
    exec.write_file(Path::new(DRIVERS_PROBE), bdf.as_bytes())?;
    if driver(exec, bdf).as_deref() != Some(want) {
        return Err(OvsError::Io(format!("{bdf} did not bind to {want}")));
    }
    Ok(had)
}

/// Step 3: the PF on `pci-pf-stub`, no host driver autoprobing its VFs,
/// and all its VFs enabled at once (raising the count with VFs in use
/// fails, so it is set once and never lowered while ports exist). Returns
/// the PF's VF count.
pub fn ensure_vfs(exec: &dyn Exec, pf: &str) -> Result<u16, OvsError> {
    exec.check(&Cmd::new(Program::Modprobe, ["pci-pf-stub"]))?;
    bind(exec, pf, "pci-pf-stub")?;
    let p = dev(pf);
    if read_trim(exec, &p.join("sriov_drivers_autoprobe"))? != "0" {
        exec.write_file(&p.join("sriov_drivers_autoprobe"), b"0")?;
    }
    let total: u16 = read_trim(exec, &p.join("sriov_totalvfs"))?
        .parse()
        .map_err(|_| OvsError::Io(format!("{pf}: unreadable sriov_totalvfs")))?;
    if total == 0 {
        return Err(OvsError::Unsupported {
            missing: vec!["fw:VIRTIO_NET_EMULATION_NUM_VF".into()],
        });
    }
    if read_trim(exec, &p.join("sriov_numvfs"))? == "0" {
        exec.write_file(&p.join("sriov_numvfs"), total.to_string().as_bytes())?;
    }
    Ok(total)
}

/// VF `n`'s host address (`virtfn<n>`).
pub fn vf_bdf(exec: &dyn Exec, pf: &str, n: u16) -> Result<String, OvsError> {
    exec.link_name(&dev(pf).join(format!("virtfn{n}")))
        .ok_or_else(|| OvsError::not_found(format!("VF {n} of {pf}")))
}

/// Step 7: the VF to `vfio-pci`, for the hypervisor, and its IOMMU
/// group's device to `vm_group`, as for a passed-through device
/// (spec/22-host-topology.md §5.3): the hypervisor runs as the VM user.
pub fn bind_vfio(exec: &dyn Exec, bdf: &str, vm_group: &str) -> Result<(), OvsError> {
    exec.check(&Cmd::new(Program::Modprobe, ["vfio-pci"]))?;
    bind(exec, bdf, "vfio-pci")?;
    let group = glidex_ovs::vfio::iommu_group(exec, bdf).ok_or_else(|| {
        OvsError::invalid(format!(
            "{bdf} has no IOMMU group: turn the IOMMU on (intel_iommu=on / amd_iommu=on)"
        ))
    })?;
    exec.write_file(
        &glidex_ovs::vfio::udev_rule_path(&group),
        format!(
            "# glidex: IOMMU group {group} ({bdf}) for a DPU port\n\
             SUBSYSTEM==\"vfio\", KERNEL==\"{group}\", GROUP=\"{vm_group}\", MODE=\"0660\"\n"
        )
        .as_bytes(),
    )?;
    let node = PathBuf::from(format!("/dev/vfio/{group}"));
    if exec.exists(&node) {
        exec.set_group_mode(&node, vm_group, 0o660)?;
    }
    Ok(())
}

/// §7.2: off `vfio-pci`, left unbound (no host driver should take it).
/// Already unbound is fine.
pub fn unbind_vf(exec: &dyn Exec, bdf: &str) -> Result<(), OvsError> {
    if !exec.exists(&dev(bdf)) && driver(exec, bdf).is_none() {
        return Ok(());
    }
    exec.write_file(&dev(bdf).join("driver_override"), b"\n")?;
    if driver(exec, bdf).is_some() {
        exec.write_file(&dev(bdf).join("driver").join("unbind"), bdf.as_bytes())?;
    }
    if let Some(group) = glidex_ovs::vfio::iommu_group(exec, bdf) {
        exec.remove_file(&glidex_ovs::vfio::udev_rule_path(&group))?;
    }
    Ok(())
}

// ---- the management link's host side (§10.1) ----------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    /// No such interface (rshim not running).
    Absent,
    /// The address was there; nothing done.
    Present,
    /// The address was added.
    Applied,
}

fn addr_show(exec: &dyn Exec, link: &LinkConfig) -> Result<Option<bool>, OvsError> {
    let out = exec.run(&Cmd::new(
        Program::Ip,
        ["-o", "addr", "show", "dev", link.iface.as_str()],
    ))?;
    if out.status != 0 {
        return Ok(None);
    }
    let text = out.stdout_str();
    let has = text.lines().any(|l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        w.windows(2)
            .any(|p| (p[0] == "inet" || p[0] == "inet6") && p[1] == link.address)
    });
    Ok(Some(has))
}

/// The link's address, if the interface is up to have it. An address
/// already there is left alone.
pub fn ensure_link(exec: &dyn Exec, link: &LinkConfig) -> Result<LinkState, OvsError> {
    match addr_show(exec, link)? {
        None => Ok(LinkState::Absent),
        Some(true) => Ok(LinkState::Present),
        Some(false) => {
            exec.check(&Cmd::new(
                Program::Ip,
                [
                    "addr",
                    "add",
                    link.address.as_str(),
                    "dev",
                    link.iface.as_str(),
                ],
            ))?;
            Ok(LinkState::Applied)
        }
    }
}

/// Whether the link has its address, without changing anything.
pub fn link_has_address(exec: &dyn Exec, link: &LinkConfig) -> bool {
    matches!(addr_show(exec, link), Ok(Some(true)))
}

/// Whether a line of `ip -o monitor link` is about `iface` (its
/// recreation after an rshim restart, finding 32).
pub fn is_link_event(line: &str, iface: &str) -> bool {
    line.split_whitespace()
        .any(|w| w.trim_end_matches(':').split('@').next() == Some(iface))
}

/// What `dpu_probe` answers (§9.2, §10.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DpuProbe {
    pub id: String,
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<DpuCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkConfig>,
    pub vfs_total: u16,
    pub vfs_free: u16,
    /// The DPU's gaps, plus the host's (`link_address`).
    pub missing: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_table() {
        let ok = Ok(());
        let down = Err("agent down".to_string());
        assert_eq!(choose(PortBackend::Host, &down, 0).unwrap(), Backend::Host);
        assert_eq!(choose(PortBackend::Host, &ok, 4).unwrap(), Backend::Host);
        assert_eq!(choose(PortBackend::Dpu, &ok, 4).unwrap(), Backend::Dpu);
        assert_eq!(
            choose(PortBackend::Dpu, &down, 4).unwrap_err().code(),
            "dpu_unavailable"
        );
        assert_eq!(
            choose(PortBackend::Dpu, &ok, 0).unwrap_err().code(),
            "dpu_capacity"
        );
        assert_eq!(choose(PortBackend::Auto, &ok, 4).unwrap(), Backend::Dpu);
        assert_eq!(choose(PortBackend::Auto, &down, 4).unwrap(), Backend::Host);
        assert_eq!(choose(PortBackend::Auto, &ok, 0).unwrap(), Backend::Host);
    }

    #[test]
    fn dpu_json_is_checked() {
        let good = br#"{"dpus":[{"id":"dpu0","agent":"tcp://192.0.2.2:7443",
            "link":{"iface":"tmfifo_net0","address":"192.0.2.1/30"},
            "tls":{"ca":"/etc/glidex/dpu/ca.pem","cert":"/etc/glidex/dpu/client.pem","key":"/etc/glidex/dpu/client.key"},
            "ovn":{"encap_ip":"192.0.2.31"},"hugepages_2m":1024}]}"#;
        let d = parse(good).unwrap();
        assert_eq!(d.bridges.representor_bridge, "br-int");
        assert_eq!(d.port_timeout_secs, 30);
        assert_eq!(d.link.unwrap().iface, "tmfifo_net0");

        for bad in [
            &b"{"[..],
            br#"{"dpus":[]}"#,
            br#"{"dpus":[{"id":"a","agent":"192.0.2.2:7443","tls":{"ca":"c","cert":"c","key":"k"}}]}"#,
            br#"{"dpus":[{"id":"a","agent":"tcp://192.0.2.2:7443","mode":"vhost-user","tls":{"ca":"c","cert":"c","key":"k"}}]}"#,
            br#"{"dpus":[{"id":"a","agent":"tcp://192.0.2.2:7443","link":{"iface":"tmfifo_net0","address":"192.0.2.1"},"tls":{"ca":"c","cert":"c","key":"k"}}]}"#,
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let dir = std::env::temp_dir().join(format!("gx-dpu-json-{}", std::process::id()));
        assert_eq!(load(&dir.join("absent.json")).unwrap(), None);
    }

    #[test]
    fn link_events_name_the_interface() {
        assert!(is_link_event(
            "12: tmfifo_net0: <BROADCAST,MULTICAST> mtu 1500 qdisc noop state DOWN",
            "tmfifo_net0"
        ));
        assert!(is_link_event(
            "Deleted 12: tmfifo_net0: <BROADCAST>",
            "tmfifo_net0"
        ));
        assert!(!is_link_event(
            "13: tmfifo_net1: <BROADCAST>",
            "tmfifo_net0"
        ));
    }
}
