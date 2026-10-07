//! Installing Open vSwitch from distribution packages (spec §6).
//!
//! Distro packages pair OVS with a DPDK the distro built and tested
//! together, so they are the default. The pinned source build for
//! features a distro lacks (spec §6.3) lives in `source_build`.

use crate::bridge;
use crate::exec::{Cmd, Exec, Program};
use crate::host::{self, HostCapabilities, ProbeOptions};
use crate::OvsError;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

const INSTALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Kernel datapath (combinations A, D).
    Kernel,
    /// OVS built with DPDK (combinations B, C).
    Dpdk,
}

impl Profile {
    /// Does the installed ovs-vswitchd provide this profile? (DPDK port
    /// types only show up after `init_dpdk`, so check the binary instead.)
    fn provided_by(self, caps: &HostCapabilities) -> bool {
        match self {
            Profile::Kernel => true,
            Profile::Dpdk => caps.ovs_dpdk_build && caps.dpdk_mempool_driver != Some(false),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    Debian,
    Fedora,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Distro {
    pub id: String,
    pub version_id: String,
    pub family: Option<Family>,
}

pub fn parse_os_release(contents: &str) -> Distro {
    let mut id = String::new();
    let mut id_like = String::new();
    let mut version_id = String::new();
    for line in contents.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            match k.trim() {
                "ID" => id = v,
                "ID_LIKE" => id_like = v,
                "VERSION_ID" => version_id = v,
                _ => {}
            }
        }
    }
    let words: Vec<&str> = std::iter::once(id.as_str())
        .chain(id_like.split_whitespace())
        .collect();
    let family = if words.iter().any(|w| matches!(*w, "debian" | "ubuntu")) {
        Some(Family::Debian)
    } else if words
        .iter()
        .any(|w| matches!(*w, "fedora" | "rhel" | "centos" | "rocky" | "almalinux"))
    {
        Some(Family::Fedora)
    } else {
        None
    };
    Distro {
        id,
        version_id,
        family,
    }
}

pub fn detect_distro(exec: &dyn Exec) -> Result<Distro, OvsError> {
    Ok(parse_os_release(&exec.read_file(Path::new("/etc/os-release"))?))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallRequest {
    pub profile: Profile,
    #[serde(default)]
    pub source_build: bool,
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReport {
    pub changed: bool,
    pub method: String,
    pub packages: Vec<String>,
    pub ovs_version: Option<String>,
    pub warnings: Vec<String>,
    pub output_tail: String,
}

/// What an install would do, computed without changing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub family: Family,
    pub packages: Vec<String>,
    /// Commands run after the packages are in (alternatives, services).
    pub post: Vec<Cmd>,
    /// Bridges whose traffic an `ovs-vswitchd` restart would interrupt.
    pub affected_bridges: Vec<String>,
}

fn packages_for(family: Family, profile: Profile) -> Vec<String> {
    let v: &[&str] = match (family, profile) {
        (Family::Debian, Profile::Kernel) => &["openvswitch-switch", "dnsmasq-base", "nftables"],
        (Family::Debian, Profile::Dpdk) => &[
            "openvswitch-switch",
            "openvswitch-switch-dpdk",
            "dnsmasq-base",
            "nftables",
        ],
        // (verify) Fedora's openvswitch package and its DPDK support.
        (Family::Fedora, _) => &["openvswitch", "dnsmasq", "nftables"],
    };
    v.iter().map(|s| s.to_string()).collect()
}

fn service_for(family: Family) -> &'static str {
    match family {
        Family::Debian => "openvswitch-switch",
        Family::Fedora => "openvswitch",
    }
}

/// Is the profile already satisfied by what's installed and running?
pub fn satisfied(caps: &HostCapabilities, profile: Profile) -> bool {
    caps.ovs_installed
        && caps.ovs_version_supported
        && caps.ovs_running
        && caps.dnsmasq
        && caps.nft
        && profile.provided_by(caps)
}

pub fn plan(exec: &dyn Exec, caps: &HostCapabilities, req: &InstallRequest) -> Result<InstallPlan, OvsError> {
    let distro = detect_distro(exec)?;
    let family = distro.family.ok_or_else(|| OvsError::Unsupported {
        missing: vec![format!("package install on distro '{}'", distro.id)],
    })?;
    let mut post = Vec::new();
    if family == Family::Debian && req.profile == Profile::Dpdk {
        post.push(Cmd::new(
            Program::UpdateAlternatives,
            [
                "--set",
                "ovs-vswitchd",
                "/usr/lib/openvswitch-switch-dpdk/ovs-vswitchd-dpdk",
            ],
        ));
    }
    post.push(Cmd::new(
        Program::Systemctl,
        ["enable", "--now", service_for(family)],
    ));
    if req.profile == Profile::Dpdk || caps.ovs_running {
        // Pick up a switched ovs-vswitchd binary.
        post.push(Cmd::new(Program::Systemctl, ["restart", service_for(family)]));
    }
    let affected_bridges = if caps.ovs_running {
        bridge::list_all_names(exec).unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(InstallPlan {
        family,
        packages: packages_for(family, req.profile),
        post,
        affected_bridges,
    })
}

/// `librte-mempool-*` driver packages the installed `dpdk` package
/// recommends (e.g. `librte-mempool-ring26`).
fn dpdk_mempool_drivers(exec: &dyn Exec) -> Vec<String> {
    let Ok(out) = exec.check(&Cmd::new(Program::DpkgQuery, ["-W", "-f=${Recommends}", "dpdk"])) else {
        return Vec::new();
    };
    let mut names: Vec<String> = out
        .stdout_str()
        .split(|c| c == ',' || c == '|')
        .filter_map(|dep| dep.split_whitespace().next())
        .filter(|name| name.starts_with("librte-mempool-"))
        .filter(|name| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '+'))
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

fn tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

/// Install (or switch) OVS for `req.profile`. No-op when already
/// satisfied; never removes or downgrades packages.
pub fn install(exec: &dyn Exec, probe_opts: &ProbeOptions, req: &InstallRequest) -> Result<InstallReport, OvsError> {
    if req.source_build {
        return crate::source_build::build(exec, probe_opts, req.confirm);
    }
    let caps = host::probe(exec, probe_opts);
    if satisfied(&caps, req.profile) {
        return Ok(InstallReport {
            changed: false,
            method: "distro".into(),
            ovs_version: caps.ovs_version,
            ..Default::default()
        });
    }
    if caps.ovs_installed && !caps.ovs_version_supported {
        return Err(OvsError::Unsupported {
            missing: vec![format!(
                "openvswitch >= {}.{} (installed: {})",
                host::MIN_OVS_VERSION.0,
                host::MIN_OVS_VERSION.1,
                caps.ovs_version.as_deref().unwrap_or("unknown")
            )],
        });
    }

    let plan = plan(exec, &caps, req)?;
    if !plan.affected_bridges.is_empty() && !req.confirm {
        return Err(OvsError::ConfirmationRequired {
            impact: format!(
                "ovs-vswitchd will restart, interrupting traffic on bridges: {}",
                plan.affected_bridges.join(", ")
            ),
        });
    }

    let mut output = Vec::new();
    let install_cmd = match plan.family {
        Family::Debian => {
            let update = Cmd::new(Program::AptGet, ["update"])
                .env("DEBIAN_FRONTEND", "noninteractive")
                .timeout(INSTALL_TIMEOUT);
            output.extend(exec.check(&update)?.stdout);
            let mut args = vec!["install".to_string(), "-y".into()];
            // DPDK's drivers (mempool ring, PMDs) are only *recommended*
            // by the dpdk package; without the mempool driver every OVS
            // mempool fails with EINVAL. Keep the kernel profile lean.
            if req.profile == Profile::Kernel {
                args.push("--no-install-recommends".into());
            }
            args.extend(plan.packages.iter().cloned());
            Cmd::new(Program::AptGet, args).env("DEBIAN_FRONTEND", "noninteractive")
        }
        Family::Fedora => {
            let mut args = vec!["install".to_string(), "-y".into()];
            args.extend(plan.packages.iter().cloned());
            Cmd::new(Program::Dnf, args)
        }
    }
    .timeout(INSTALL_TIMEOUT);
    output.extend(exec.check(&install_cmd)?.stdout);
    if plan.family == Family::Debian && req.profile == Profile::Dpdk {
        // apt skips recommends of packages that are already installed, so
        // install dpdk's mempool drivers explicitly. Their names carry the
        // DPDK ABI (librte-mempool-ring26), so read them from dpdk itself.
        let drivers = dpdk_mempool_drivers(exec);
        if !drivers.is_empty() {
            let mut args = vec!["install".to_string(), "-y".into()];
            args.extend(drivers);
            output.extend(
                exec.check(&Cmd::new(Program::AptGet, args).env("DEBIAN_FRONTEND", "noninteractive").timeout(INSTALL_TIMEOUT))?
                    .stdout,
            );
        }
    }
    for cmd in &plan.post {
        output.extend(exec.check(cmd)?.stdout);
    }

    // Trust what OVS reports, not package names.
    let after = host::probe(exec, probe_opts);
    let missing: Vec<String> = (!req.profile.provided_by(&after))
        .then(|| {
            if after.ovs_dpdk_build {
                "DPDK mempool driver (librte-mempool-ring)".to_string()
            } else {
                "dpdk build of ovs-vswitchd".to_string()
            }
        })
        .into_iter()
        .chain((!after.ovs_running).then(|| "ovs-vswitchd running".to_string()))
        .collect();
    if !missing.is_empty() {
        return Err(OvsError::Unsupported { missing });
    }
    let mut warnings = Vec::new();
    if !after.iface_types.iter().any(|t| t == "afxdp") {
        warnings.push("this OVS build has no AF_XDP support (combinations C/D need the pinned source build)".into());
    }
    Ok(InstallReport {
        changed: true,
        method: "distro".into(),
        packages: plan.packages,
        ovs_version: after.ovs_version,
        warnings,
        output_tail: tail(&output),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpdkSettings {
    /// MiB of hugepage memory per NUMA node, e.g. "1024" or "1024,1024".
    pub socket_mem: String,
    /// CPUs for PMD threads, hex mask like "0x2".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pmd_cpu_mask: Option<String>,
    #[serde(default)]
    pub confirm: bool,
}

impl DpdkSettings {
    pub fn validate(&self) -> Result<(), OvsError> {
        let mem_ok = !self.socket_mem.is_empty()
            && self.socket_mem.split(',').all(|p| !p.is_empty() && p.len() <= 7 && p.chars().all(|c| c.is_ascii_digit()));
        if !mem_ok {
            return Err(OvsError::invalid("socket_mem must be MiB per NUMA node, e.g. 1024 or 1024,1024"));
        }
        if let Some(mask) = &self.pmd_cpu_mask {
            let hex = mask.strip_prefix("0x").unwrap_or("");
            if hex.is_empty() || hex.len() > 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(OvsError::invalid("pmd_cpu_mask must be a hex mask like 0x2"));
            }
        }
        Ok(())
    }
}

/// Enable DPDK in ovs-vswitchd (spec §6.2): needs a DPDK-built vswitchd
/// (profile `dpdk`) and hugepages. Restarts ovs-vswitchd.
pub fn init_dpdk(exec: &dyn Exec, probe_opts: &ProbeOptions, s: &DpdkSettings) -> Result<(), OvsError> {
    s.validate()?;
    let caps = host::probe(exec, probe_opts);
    if !caps.ovs_running {
        return Err(OvsError::Unsupported { missing: vec!["ovs-vswitchd running".into()] });
    }
    // Already initialized with these settings: nothing to do. Different
    // settings (e.g. more socket memory) need a restart like the first time.
    // Unreadable current settings just mean "apply what was asked".
    let current = crate::vsctl::list(exec, "Open_vSwitch", &["other_config"])
        .ok()
        .and_then(|rows| rows.first().map(|r| r.map("other_config")))
        .unwrap_or_default();
    let wanted = dpdk_config(exec, s);
    if caps.dpdk_initialized && wanted.iter().all(|(k, v)| current.get(*k) == Some(v)) {
        return Ok(());
    }
    if caps.hugepages.iter().all(|h| h.total == 0) {
        return Err(OvsError::Unsupported { missing: vec!["hugepages (e.g. sysctl vm.nr_hugepages=1024)".into()] });
    }
    let bridges = bridge::list_all_names(exec).unwrap_or_default();
    if !bridges.is_empty() && !s.confirm {
        return Err(OvsError::ConfirmationRequired {
            impact: format!(
                "ovs-vswitchd will restart with DPDK, interrupting traffic on bridges: {}",
                bridges.join(", ")
            ),
        });
    }
    let family = detect_distro(exec)?.family.ok_or_else(|| OvsError::Unsupported {
        missing: vec!["supported distro".into()],
    })?;
    let mut args = vec!["set".to_string(), "Open_vSwitch".into(), ".".into(), "other_config:dpdk-init=true".into()];
    args.extend(wanted.iter().map(|(k, v)| format!("other_config:{}={}", k, v)));
    crate::vsctl::run(exec, args)?;
    exec.check(&Cmd::new(Program::Systemctl, ["restart", service_for(family)]).timeout(Duration::from_secs(120)))?;
    // DPDK EAL init happens in the restarted daemon; trust OVSDB, not the restart.
    for _ in 0..60 {
        let after = host::probe(exec, probe_opts);
        if after.dpdk_initialized {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(OvsError::Unsupported {
        missing: vec!["dpdk initialized (is the dpdk profile installed and are hugepages free? see the ovs-vswitchd log)".into()],
    })
}

/// The `other_config` keys DPDK mode wants (besides `dpdk-init`), as
/// `(key, value)`. An explicit `pmd_cpu_mask` wins; otherwise the PMD and
/// non-PMD (lcore) CPUs come from `tuning::plan`, and are left to OVS's
/// defaults when the topology can't be read.
///
/// `userspace-tso-enable` lets vhost-user guests send and receive large
/// TSO segments instead of MTU-sized frames: without it every 1500-byte
/// frame between two VMs is processed separately, which is what holds
/// VM-to-VM throughput down. `pmd-auto-lb` rebalances queues across PMDs
/// by load, which only matters with more than one.
fn dpdk_config(exec: &dyn Exec, s: &DpdkSettings) -> Vec<(&'static str, String)> {
    let mut v = vec![("dpdk-socket-mem", s.socket_mem.clone()), ("userspace-tso-enable", "true".to_string())];
    let plan = crate::tuning::plan(&crate::tuning::read_topology(exec));
    let pmds = match (&s.pmd_cpu_mask, &plan) {
        (Some(mask), _) => {
            v.push(("pmd-cpu-mask", mask.clone()));
            crate::tuning::mask_cpu_count(mask)
        }
        (None, Some(p)) => {
            v.push(("pmd-cpu-mask", p.pmd_mask()));
            v.push(("dpdk-lcore-mask", p.lcore_mask()));
            p.pmd_cpus.len()
        }
        (None, None) => 0,
    };
    if pmds > 1 {
        v.push(("pmd-auto-lb", "true".to_string()));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    const UBUNTU: &str = "PRETTY_NAME=\"Ubuntu 26.04 LTS\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"26.04\"\n";
    const FEDORA: &str = "NAME=\"Fedora Linux\"\nID=fedora\nVERSION_ID=42\n";
    const ROCKY: &str = "ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=\"9.4\"\n";
    const ARCH: &str = "ID=arch\n";
    const OVS_ROW: &str = r#"{"data":[[["set",["afxdp","internal","system","tap"]],["set",["netdev","system"]],false]],"headings":["iface_types","datapath_types","dpdk_initialized"]}"#;
    const OVS_DPDK_ROW: &str = r#"{"data":[[["set",["afxdp","dpdk","dpdkvhostuserclient","internal","system","tap"]],["set",["netdev","system"]],false]],"headings":["iface_types","datapath_types","dpdk_initialized"]}"#;

    #[test]
    fn dpdk_settings_validation_and_commands() {
        let ok = DpdkSettings { socket_mem: "1024,1024".into(), pmd_cpu_mask: Some("0x6".into()), confirm: false };
        ok.validate().unwrap();
        for bad in [
            DpdkSettings { socket_mem: "1G".into(), pmd_cpu_mask: None, confirm: false },
            DpdkSettings { socket_mem: "1024".into(), pmd_cpu_mask: Some("6; reboot".into()), confirm: false },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }

        let exec = RecordingExec::new();
        exec.file("/etc/os-release", UBUNTU);
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(OVS_ROW));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(OVS_DPDK_ROW.replace("false]]", "true]]")));
        exec.file("/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages", "1024\n");
        exec.on("ovs-vsctl list-br", Output::ok("br-int\n"));
        let err = init_dpdk(&exec, &ProbeOptions::default(), &ok).unwrap_err();
        assert!(matches!(err, OvsError::ConfirmationRequired { .. }), "{err:?}");
        assert!(!exec.calls().iter().any(|c| c.contains("dpdk-init=true")), "nothing changed without confirm");

        let exec = RecordingExec::new();
        exec.file("/etc/os-release", UBUNTU);
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(OVS_ROW));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(OVS_DPDK_ROW.replace("false]]", "true]]")));
        exec.file("/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages", "1024\n");
        exec.on("ovs-vsctl list-br", Output::ok("br-int\n"));
        init_dpdk(&exec, &ProbeOptions::default(), &DpdkSettings { confirm: true, ..ok }).unwrap();
        let calls = exec.calls();
        assert!(calls.contains(&"ovs-vsctl set Open_vSwitch . other_config:dpdk-init=true other_config:dpdk-socket-mem=1024,1024 other_config:userspace-tso-enable=true other_config:pmd-cpu-mask=0x6 other_config:pmd-auto-lb=true".to_string()), "{calls:#?}");
        assert!(calls.contains(&"systemctl restart openvswitch-switch".to_string()));
    }

    #[test]
    fn missing_mempool_driver_is_not_satisfied() {
        let exec = RecordingExec::new();
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
        exec.on("ovs-vswitchd --version", Output::ok("ovs-vswitchd (Open vSwitch) 3.7.1\nDPDK 25.11.0\n"));
        exec.on("dpkg-query -W", Output::failed(1, "dpkg-query: no packages found matching librte-mempool-ring*"));
        exec.file("/var/lib/dpkg/status", "");
        exec.on("dnsmasq --version", Output::ok("Dnsmasq 2.92\n"));
        exec.on("nft --version", Output::ok("nftables\n"));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(OVS_ROW));
        let caps = host::probe(&exec, &ProbeOptions::default());
        assert_eq!(caps.dpdk_mempool_driver, Some(false));
        assert!(!satisfied(&caps, Profile::Dpdk), "broken DPDK install must be repaired");
        assert!(satisfied(&caps, Profile::Kernel));
    }

    #[test]
    fn dpdk_settings_change_restarts() {
        let initialized = OVS_DPDK_ROW.replace("false]]", "true]]");
        let exec = RecordingExec::new();
        exec.file("/etc/os-release", UBUNTU);
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(initialized.clone()));
        exec.on("ovs-vsctl --format=json --columns=other_config", Output::ok(r#"{"data":[[["map",[["dpdk-init","true"],["dpdk-socket-mem","1024"],["userspace-tso-enable","true"]]]]],"headings":["other_config"]}"#));
        exec.file("/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages", "2048\n");
        let same = DpdkSettings { socket_mem: "1024".into(), pmd_cpu_mask: None, confirm: false };
        init_dpdk(&exec, &ProbeOptions::default(), &same).unwrap();
        assert!(!exec.calls().iter().any(|c| c.contains("systemctl restart")), "same settings: no restart");

        let more = DpdkSettings { socket_mem: "2048".into(), pmd_cpu_mask: None, confirm: true };
        init_dpdk(&exec, &ProbeOptions::default(), &more).unwrap();
        let calls = exec.calls();
        assert!(calls.iter().any(|c| c.contains("other_config:dpdk-socket-mem=2048")), "{calls:#?}");
        assert!(calls.contains(&"systemctl restart openvswitch-switch".to_string()));
    }

    #[test]
    fn dpdk_without_tso_is_reconfigured_with_auto_pmd_placement() {
        let exec = RecordingExec::new();
        exec.file("/etc/os-release", UBUNTU);
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(OVS_DPDK_ROW.replace("false]]", "true]]")));
        exec.on("ovs-vsctl --format=json --columns=other_config", Output::ok(r#"{"data":[[["map",[["dpdk-init","true"],["dpdk-socket-mem","1024"]]]]],"headings":["other_config"]}"#));
        exec.file("/sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages", "2048\n");
        // 8 cores, no SMT, one NUMA node.
        exec.file("/sys/devices/system/cpu/online", "0-7\n");
        exec.file("/sys/devices/system/node/online", "0\n");
        exec.file("/sys/devices/system/node/node0/cpulist", "0-7\n");
        for c in 0..8 {
            let b = format!("/sys/devices/system/cpu/cpu{}/topology", c);
            exec.file(&format!("{}/core_id", b), &format!("{}\n", c));
            exec.file(&format!("{}/physical_package_id", b), "0\n");
            exec.file(&format!("{}/thread_siblings_list", b), &format!("{}\n", c));
        }
        let s = DpdkSettings { socket_mem: "1024".into(), pmd_cpu_mask: None, confirm: true };
        init_dpdk(&exec, &ProbeOptions::default(), &s).unwrap();
        let calls = exec.calls();
        assert!(calls.contains(&"ovs-vsctl set Open_vSwitch . other_config:dpdk-init=true other_config:dpdk-socket-mem=1024 other_config:userspace-tso-enable=true other_config:pmd-cpu-mask=0x6 other_config:dpdk-lcore-mask=0x1 other_config:pmd-auto-lb=true".to_string()), "{calls:#?}");
        assert!(calls.contains(&"systemctl restart openvswitch-switch".to_string()));
    }

    #[test]
    fn dpdk_needs_hugepages() {
        let exec = RecordingExec::new();
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(OVS_ROW));
        let err = init_dpdk(&exec, &ProbeOptions::default(), &DpdkSettings { socket_mem: "1024".into(), pmd_cpu_mask: None, confirm: true }).unwrap_err();
        assert!(format!("{err}").contains("hugepages"), "{err}");
    }

    #[test]
    fn detects_families() {
        assert_eq!(parse_os_release(UBUNTU).family, Some(Family::Debian));
        assert_eq!(parse_os_release(UBUNTU).version_id, "26.04");
        assert_eq!(parse_os_release(FEDORA).family, Some(Family::Fedora));
        assert_eq!(parse_os_release(ROCKY).family, Some(Family::Fedora));
        assert_eq!(parse_os_release(ARCH).family, None);
    }

    fn not_installed(exec: &RecordingExec) {
        exec.file("/etc/os-release", UBUNTU);
        exec.on("ovs-vsctl --version", Output::failed(127, ""));
        exec.on("dnsmasq --version", Output::failed(127, ""));
    }

    fn installed_after(exec: &RecordingExec, row: &str) {
        // First probe: missing; after install: present.
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
        exec.on("dnsmasq --version", Output::ok("Dnsmasq version 2.92\n"));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(row));
    }

    #[test]
    fn installs_kernel_profile_on_ubuntu() {
        let exec = RecordingExec::new();
        not_installed(&exec);
        installed_after(&exec, OVS_ROW);
        let report = install(&exec, &ProbeOptions::default(), &InstallRequest { profile: Profile::Kernel, source_build: false, confirm: false }).unwrap();
        assert!(report.changed);
        assert_eq!(report.ovs_version.as_deref(), Some("3.7.1"));
        let calls = exec.calls();
        assert!(calls.contains(&"apt-get install -y --no-install-recommends openvswitch-switch dnsmasq-base nftables".to_string()), "{calls:?}");
        assert!(calls.contains(&"systemctl enable --now openvswitch-switch".to_string()));
        assert!(!calls.iter().any(|c| c.contains("update-alternatives")));
    }

    #[test]
    fn dpdk_profile_switches_alternative_and_checks_features() {
        let exec = RecordingExec::new();
        not_installed(&exec);
        installed_after(&exec, OVS_ROW);
        exec.on("ovs-vswitchd --version", Output::ok("ovs-vswitchd (Open vSwitch) 3.7.1\nDPDK 25.11.0\n"));
        exec.on("dpkg-query -W", Output::ok("install ok installed\n"));
        install(&exec, &ProbeOptions::default(), &InstallRequest { profile: Profile::Dpdk, source_build: false, confirm: false }).unwrap();
        assert!(exec.calls().contains(&"update-alternatives --set ovs-vswitchd /usr/lib/openvswitch-switch-dpdk/ovs-vswitchd-dpdk".to_string()));
        let apt = exec.calls().into_iter().find(|c| c.starts_with("apt-get install")).unwrap();
        assert!(!apt.contains("--no-install-recommends"), "dpdk needs its recommended driver packages: {apt}");

        // Already-installed dpdk: its mempool drivers are installed by name.
        let exec = RecordingExec::new();
        not_installed(&exec);
        installed_after(&exec, OVS_ROW);
        exec.on("ovs-vswitchd --version", Output::ok("ovs-vswitchd (Open vSwitch) 3.7.1\nDPDK 25.11.0\n"));
        exec.on("dpkg-query -W -f=${Recommends} dpdk", Output::ok("librte-mempool26, librte-mempool-ring26, librte-net-ixgbe26 | librte-net-i40e26"));
        exec.on("dpkg-query -W -f=${Status}", Output::ok("install ok installed\n"));
        install(&exec, &ProbeOptions::default(), &InstallRequest { profile: Profile::Dpdk, source_build: false, confirm: false }).unwrap();
        assert!(exec.calls().contains(&"apt-get install -y librte-mempool-ring26".to_string()), "{:#?}", exec.calls());

        // If the installed vswitchd isn't a DPDK build, fail instead of trusting packages.
        let exec = RecordingExec::new();
        not_installed(&exec);
        installed_after(&exec, OVS_ROW);
        exec.on("ovs-vswitchd --version", Output::ok("ovs-vswitchd (Open vSwitch) 3.7.1\n"));
        let err = install(&exec, &ProbeOptions::default(), &InstallRequest { profile: Profile::Dpdk, source_build: false, confirm: false }).unwrap_err();
        let OvsError::Unsupported { missing } = err else { panic!("{err:?}") };
        assert!(missing.contains(&"dpdk build of ovs-vswitchd".to_string()));
    }

    #[test]
    fn no_op_when_satisfied() {
        let exec = RecordingExec::new();
        exec.file("/etc/os-release", UBUNTU);
        installed_after(&exec, OVS_ROW);
        exec.on("nft --version", Output::ok("nftables v1.1\n"));
        let report = install(&exec, &ProbeOptions::default(), &InstallRequest { profile: Profile::Kernel, source_build: false, confirm: false }).unwrap();
        assert!(!report.changed);
        assert!(!exec.calls().iter().any(|c| c.starts_with("apt-get")));
    }

    #[test]
    fn restart_with_bridges_needs_confirmation() {
        let exec = RecordingExec::new();
        exec.file("/etc/os-release", UBUNTU);
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(OVS_ROW));
        exec.on("ovs-vsctl list-br", Output::ok("br-int\ngxbr-nat\n"));
        let err = install(&exec, &ProbeOptions::default(), &InstallRequest { profile: Profile::Dpdk, source_build: false, confirm: false }).unwrap_err();
        let OvsError::ConfirmationRequired { impact } = err else { panic!("{err:?}") };
        assert!(impact.contains("br-int") && impact.contains("gxbr-nat"));
        assert!(!exec.calls().iter().any(|c| c.starts_with("apt-get")));
    }

    #[test]
    fn unsupported_distro_and_old_ovs() {
        let exec = RecordingExec::new();
        exec.file("/etc/os-release", ARCH);
        exec.on("ovs-vsctl --version", Output::failed(127, ""));
        assert!(matches!(install(&exec, &ProbeOptions::default(), &InstallRequest { profile: Profile::Kernel, source_build: false, confirm: false }), Err(OvsError::Unsupported { .. })));

        let exec = RecordingExec::new();
        exec.file("/etc/os-release", UBUNTU);
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 2.17.9\n"));
        let err = install(&exec, &ProbeOptions::default(), &InstallRequest { profile: Profile::Kernel, source_build: false, confirm: false }).unwrap_err();
        assert!(format!("{err}").contains("2.17.9"));
    }
}
