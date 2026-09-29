//! Pinned source build of Open vSwitch + DPDK (spec §6.3, decision 10),
//! for hosts whose distro OVS lacks a needed feature.
//!
//! Pins follow the OVS release FAQ's pairing (OVS 3.7.x LTS ↔ DPDK
//! 25.11.2). Bump versions and checksums together.

use crate::exec::{Cmd, Exec, Program};
use crate::host::{self, ProbeOptions};
use crate::install::{detect_distro, Family, InstallReport};
use crate::OvsError;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Pin {
    pub name: &'static str,
    pub version: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    /// Top-level directory inside the tarball.
    pub dir: &'static str,
}

pub const OVS: Pin = Pin {
    name: "openvswitch",
    version: "3.7.1",
    url: "https://www.openvswitch.org/releases/openvswitch-3.7.1.tar.gz",
    sha256: "b8936c2e95a024d37123536ca843648bc2f1d2520921f991dd3d06248859b70f",
    dir: "openvswitch-3.7.1",
};

pub const DPDK: Pin = Pin {
    name: "dpdk",
    version: "25.11.2",
    url: "https://fast.dpdk.org/rel/dpdk-25.11.2.tar.xz",
    sha256: "418bfe3212640ee95a1cb10af6ed360cad2387686fe2721f8a3a9cd02d5ef4f2",
    dir: "dpdk-stable-25.11.2",
};

pub const BUILD_DIR: &str = "/var/lib/glidex/build";

pub fn ovs_prefix() -> PathBuf {
    PathBuf::from(format!("/opt/glidex/ovs-{}", OVS.version))
}

pub fn dpdk_prefix() -> PathBuf {
    PathBuf::from(format!("/opt/glidex/dpdk-{}", DPDK.version))
}

/// `bin/` of the built OVS, for `SystemExec::set_ovs_bin_dir`.
pub fn ovs_bin_dir() -> PathBuf {
    ovs_prefix().join("bin")
}

fn build_deps(family: Family) -> &'static [&'static str] {
    match family {
        Family::Debian => &[
            "build-essential", "autoconf", "automake", "libtool", "pkg-config", "python3",
            "python3-pyelftools", "meson", "ninja-build", "libnuma-dev", "libssl-dev",
            "libcap-ng-dev", "libbpf-dev", "libxdp-dev", "curl", "xz-utils",
        ],
        // (verify) Fedora package names.
        Family::Fedora => &[
            "gcc", "make", "autoconf", "automake", "libtool", "pkgconf", "python3",
            "python3-pyelftools", "meson", "ninja-build", "numactl-devel", "openssl-devel",
            "libcap-ng-devel", "libbpf-devel", "libxdp-devel", "curl", "xz",
        ],
    }
}

const LONG: Duration = Duration::from_secs(60 * 60);

fn run(exec: &dyn Exec, cmd: Cmd, log: &mut Vec<u8>) -> Result<(), OvsError> {
    let out = exec.check(&cmd)?;
    log.extend(out.stdout);
    Ok(())
}

fn fetch_and_verify(exec: &dyn Exec, pin: &Pin, dir: &Path, log: &mut Vec<u8>) -> Result<PathBuf, OvsError> {
    let file = dir.join(pin.url.rsplit('/').next().unwrap_or(pin.name));
    let file_s = file.to_string_lossy().to_string();
    run(exec, Cmd::new(Program::Curl, ["-fsSL", "--retry", "3", "-o", file_s.as_str(), pin.url]).timeout(LONG), log)?;
    let out = exec.check(&Cmd::new(Program::Sha256sum, [file_s.as_str()]))?;
    let actual = out.stdout_str().split_whitespace().next().unwrap_or("").to_string();
    if actual != pin.sha256 {
        let _ = exec.remove_file(&file);
        return Err(OvsError::Io(format!(
            "checksum mismatch for {} {}: expected {}, got {} (download removed, nothing extracted)",
            pin.name, pin.version, pin.sha256, actual
        )));
    }
    run(exec, Cmd::new(Program::Tar, ["-xf", file_s.as_str(), "-C", &dir.to_string_lossy()]).timeout(LONG), log)?;
    Ok(dir.join(pin.dir))
}

/// Units running the built OVS through its `ovs-ctl` (spec §6.3 step 4).
pub fn units() -> Vec<(&'static str, String)> {
    let ctl = ovs_prefix().join("share/openvswitch/scripts/ovs-ctl");
    let ctl = ctl.display();
    let libs = dpdk_prefix().join("lib/x86_64-linux-gnu");
    let env = format!(
        "Environment=PATH={}/bin:{}/sbin:/usr/sbin:/usr/bin\nEnvironment=LD_LIBRARY_PATH={}\n",
        ovs_prefix().display(),
        ovs_prefix().display(),
        libs.display()
    );
    vec![
        (
            "/etc/systemd/system/glidex-ovsdb-server.service",
            format!(
                "[Unit]\nDescription=glidex Open vSwitch {v} database (source build)\nBefore=glidex-ovs-vswitchd.service\n\n[Service]\nType=forking\n{env}ExecStart={ctl} --no-ovs-vswitchd --no-monitor --system-id=random start\nExecStop={ctl} --no-ovs-vswitchd stop\nRestart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n",
                v = OVS.version
            ),
        ),
        (
            "/etc/systemd/system/glidex-ovs-vswitchd.service",
            format!(
                "[Unit]\nDescription=glidex Open vSwitch {v} switch daemon (source build, DPDK {d})\nRequires=glidex-ovsdb-server.service\nAfter=glidex-ovsdb-server.service network-pre.target\n\n[Service]\nType=forking\n{env}ExecStart={ctl} --no-ovsdb-server --no-monitor start\nExecStop={ctl} --no-ovsdb-server stop\nRestart=on-failure\n\n[Install]\nWantedBy=multi-user.target\n",
                v = OVS.version,
                d = DPDK.version
            ),
        ),
    ]
}

/// Build and install the pinned OVS + DPDK. Needs `confirm`: it stops and
/// disables the distro OVS service (without uninstalling it).
pub fn build(exec: &dyn Exec, probe_opts: &ProbeOptions, confirm: bool) -> Result<InstallReport, OvsError> {
    let family = detect_distro(exec)?.family.ok_or_else(|| OvsError::Unsupported {
        missing: vec!["supported distro for the source build".into()],
    })?;
    let distro_service = match family {
        Family::Debian => "openvswitch-switch",
        Family::Fedora => "openvswitch",
    };
    let distro_active = exec
        .run(&Cmd::new(Program::Systemctl, ["is-active", "--quiet", distro_service]))
        .map(|o| o.status == 0)
        .unwrap_or(false);
    if !confirm {
        let mut impact = format!(
            "builds Open vSwitch {} with DPDK {} from source (takes 15-30 minutes) into {}",
            OVS.version,
            DPDK.version,
            ovs_prefix().display()
        );
        if distro_active {
            impact.push_str(&format!(
                "; the distro service {} will be stopped and disabled (not uninstalled), interrupting all bridges",
                distro_service
            ));
        }
        return Err(OvsError::ConfirmationRequired { impact });
    }

    let mut log = Vec::new();
    let dir = PathBuf::from(BUILD_DIR);
    exec.create_dir(&dir, 0o755)?;

    // 1. Build dependencies.
    let deps: Vec<String> = build_deps(family).iter().map(|s| s.to_string()).collect();
    match family {
        Family::Debian => {
            run(exec, Cmd::new(Program::AptGet, ["update"]).env("DEBIAN_FRONTEND", "noninteractive").timeout(LONG), &mut log)?;
            let mut args = vec!["install".to_string(), "-y".into(), "--no-install-recommends".into()];
            args.extend(deps);
            run(exec, Cmd::new(Program::AptGet, args).env("DEBIAN_FRONTEND", "noninteractive").timeout(LONG), &mut log)?;
        }
        Family::Fedora => {
            let mut args = vec!["install".to_string(), "-y".into()];
            args.extend(deps);
            run(exec, Cmd::new(Program::Dnf, args).timeout(LONG), &mut log)?;
        }
    }

    // 2. Sources, verified before anything is extracted.
    let dpdk_src = fetch_and_verify(exec, &DPDK, &dir, &mut log)?;
    let ovs_src = fetch_and_verify(exec, &OVS, &dir, &mut log)?;
    let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).to_string();

    // 3. DPDK.
    let dpdk_prefix = dpdk_prefix().to_string_lossy().to_string();
    run(exec, Cmd::new(Program::Meson, ["setup", "build", &format!("--prefix={}", dpdk_prefix)]).cwd(&dpdk_src).timeout(LONG), &mut log)?;
    run(exec, Cmd::new(Program::Ninja, ["-C", "build"]).cwd(&dpdk_src).timeout(LONG), &mut log)?;
    run(exec, Cmd::new(Program::Ninja, ["-C", "build", "install"]).cwd(&dpdk_src).timeout(LONG), &mut log)?;

    // 4. OVS against that DPDK, with AF_XDP.
    let pkgconfig = format!("{}/lib/x86_64-linux-gnu/pkgconfig", dpdk_prefix);
    run(
        exec,
        Cmd::new(
            Program::Configure,
            [
                format!("--prefix={}", ovs_prefix().display()),
                "--localstatedir=/var".into(),
                "--sysconfdir=/etc".into(),
                "--enable-afxdp".into(),
                "--with-dpdk=shared".into(),
            ],
        )
        .env("PKG_CONFIG_PATH", &pkgconfig)
        .cwd(&ovs_src)
        .timeout(LONG),
        &mut log,
    )?;
    run(exec, Cmd::new(Program::Make, [format!("-j{}", jobs)]).cwd(&ovs_src).timeout(LONG), &mut log)?;
    run(exec, Cmd::new(Program::Make, ["install"]).cwd(&ovs_src).timeout(LONG), &mut log)?;

    // 5. Replace the distro service with glidex units (same OVSDB paths).
    if distro_active {
        run(exec, Cmd::new(Program::Systemctl, ["disable", "--now", distro_service]).timeout(Duration::from_secs(120)), &mut log)?;
    }
    for (path, contents) in units() {
        exec.write_file(Path::new(path), contents.as_bytes())?;
    }
    run(exec, Cmd::new(Program::Systemctl, ["daemon-reload"]), &mut log)?;
    run(
        exec,
        Cmd::new(Program::Systemctl, ["enable", "--now", "glidex-ovsdb-server.service", "glidex-ovs-vswitchd.service"]).timeout(Duration::from_secs(120)),
        &mut log,
    )?;
    exec.set_ovs_bin_dir(Some(ovs_bin_dir()));

    // 6. Trust what the new OVS reports.
    let after = host::probe(exec, probe_opts);
    let mut missing = Vec::new();
    if !after.ovs_running {
        missing.push("source-built ovs-vswitchd running".to_string());
    }
    if !after.iface_types.iter().any(|t| t == "afxdp") {
        missing.push("afxdp".to_string());
    }
    if !missing.is_empty() {
        return Err(OvsError::Unsupported { missing });
    }
    let text = String::from_utf8_lossy(&log);
    let lines: Vec<&str> = text.lines().collect();
    Ok(InstallReport {
        changed: true,
        method: "source".into(),
        packages: vec![format!("openvswitch {}", OVS.version), format!("dpdk {}", DPDK.version)],
        ovs_version: after.ovs_version,
        warnings: Vec::new(),
        output_tail: lines[lines.len().saturating_sub(20)..].join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    const UBUNTU: &str = "ID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"26.04\"\n";

    fn exec_with_downloads(ovs_sum: &str) -> RecordingExec {
        let exec = RecordingExec::new();
        exec.file("/etc/os-release", UBUNTU);
        exec.on("systemctl is-active --quiet openvswitch-switch", Output::ok(""));
        exec.on("sha256sum /var/lib/glidex/build/dpdk-25.11.2.tar.xz", Output::ok(format!("{}  f\n", DPDK.sha256)));
        exec.on("sha256sum /var/lib/glidex/build/openvswitch-3.7.1.tar.gz", Output::ok(format!("{}  f\n", ovs_sum)));
        exec.on("ovs-vsctl --version", Output::ok("ovs-vsctl (Open vSwitch) 3.7.1\n"));
        exec.on("ovs-vsctl --format=json --columns=iface_types", Output::ok(r#"{"data":[[["set",["afxdp","dpdk","system"]],["set",["netdev","system"]],false]],"headings":["iface_types","datapath_types","dpdk_initialized"]}"#));
        exec
    }

    #[test]
    fn needs_confirmation_and_says_what_happens() {
        let exec = exec_with_downloads(OVS.sha256);
        let err = build(&exec, &ProbeOptions::default(), false).unwrap_err();
        let OvsError::ConfirmationRequired { impact } = err else { panic!("{err:?}") };
        assert!(impact.contains("3.7.1") && impact.contains("25.11.2"));
        assert!(impact.contains("stopped and disabled (not uninstalled)"));
        assert!(!exec.calls().iter().any(|c| c.starts_with("curl") || c.starts_with("apt-get")));
    }

    #[test]
    fn full_build_sequence() {
        let exec = exec_with_downloads(OVS.sha256);
        let report = build(&exec, &ProbeOptions::default(), true).unwrap();
        assert_eq!(report.method, "source");
        let calls = exec.calls();
        let pos = |needle: &str| calls.iter().position(|c| c.contains(needle)).unwrap_or_else(|| panic!("missing {needle}: {calls:#?}"));
        let order = [
            "apt-get install -y --no-install-recommends build-essential",
            "curl -fsSL --retry 3 -o /var/lib/glidex/build/dpdk-25.11.2.tar.xz https://fast.dpdk.org/rel/dpdk-25.11.2.tar.xz",
            "tar -xf /var/lib/glidex/build/dpdk-25.11.2.tar.xz",
            "meson setup build --prefix=/opt/glidex/dpdk-25.11.2 (in /var/lib/glidex/build/dpdk-stable-25.11.2)",
            "ninja -C build install (in /var/lib/glidex/build/dpdk-stable-25.11.2)",
            "./configure --prefix=/opt/glidex/ovs-3.7.1 --localstatedir=/var --sysconfdir=/etc --enable-afxdp --with-dpdk=shared (in /var/lib/glidex/build/openvswitch-3.7.1)",
            "make install (in /var/lib/glidex/build/openvswitch-3.7.1)",
            "systemctl disable --now openvswitch-switch",
            "systemctl enable --now glidex-ovsdb-server.service glidex-ovs-vswitchd.service",
        ];
        let positions: Vec<usize> = order.iter().map(|n| pos(n)).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "out of order: {positions:?}");
        let units: Vec<String> = exec.writes().iter().map(|(p, _)| p.display().to_string()).collect();
        assert!(units.contains(&"/etc/systemd/system/glidex-ovs-vswitchd.service".to_string()));
    }

    #[test]
    fn checksum_mismatch_aborts_before_extracting() {
        let exec = exec_with_downloads(&"0".repeat(64));
        let err = build(&exec, &ProbeOptions::default(), true).unwrap_err();
        assert!(format!("{err}").contains("checksum mismatch for openvswitch 3.7.1"), "{err}");
        let calls = exec.calls();
        assert!(!calls.iter().any(|c| c.starts_with("tar -xf /var/lib/glidex/build/openvswitch")), "OVS not extracted");
        assert!(!calls.iter().any(|c| c.starts_with("./configure")));
        assert!(calls.iter().any(|c| c == "rm /var/lib/glidex/build/openvswitch-3.7.1.tar.gz"), "bad download removed");
    }

    #[test]
    fn units_point_at_the_prefix() {
        let units = units();
        assert!(units[1].1.contains("/opt/glidex/ovs-3.7.1/share/openvswitch/scripts/ovs-ctl --no-ovsdb-server"));
        assert!(units[1].1.contains("LD_LIBRARY_PATH=/opt/glidex/dpdk-25.11.2/lib/x86_64-linux-gnu"));
    }
}
