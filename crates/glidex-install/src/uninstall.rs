//! `glidex-install uninstall`: undo what the installer and glidex-netd did.
//!
//! Order matters: stop the control plane (stops VMs) and netd, tear down
//! host networking with netd's own code (uplinks put the host's IP back on
//! its NIC and restore NIC drivers; NAT, bridges and VM ports are removed),
//! then remove units, binaries, configuration and state. Kernel settings
//! the installer changed (ip_forward, hugepages) go back to their recorded
//! previous values. OVS packages, DPDK settings (and the hugepages they
//! need), cloud-hypervisor and `~/.glidex` stay unless asked for.

use crate::sysconfig;
use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const UNITS: &[&str] = &["glidex-control-plane.service", "glidex-netd.service"];
/// Units of the pinned OVS source build (removed only with --remove-ovs).
pub const OVS_SOURCE_UNITS: &[&str] = &["glidex-ovs-vswitchd.service", "glidex-ovsdb-server.service"];
pub const UNIT_DIR: &str = "/etc/systemd/system";
pub const NETD_DB: &str = "/var/lib/glidex/netd.db";
pub const SYSTEM_BIN_DIR: &str = "/usr/local/bin";
pub const BINARIES: &[&str] = &["glidex-control-plane", "gxctl", "glidex-netd"];
pub const GROUP: &str = "glidex";
/// Directories owned entirely by glidex.
pub const STATE_DIRS: &[&str] = &["/etc/glidex", "/var/lib/glidex", "/run/glidex"];
pub const SOURCE_PREFIX: &str = "/opt/glidex";
pub const DISTRO_OVS_PACKAGES: &[&str] = &["openvswitch-switch-dpdk", "openvswitch-switch"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub dry_run: bool,
    pub yes: bool,
    pub remove_ovs: bool,
    pub reset_dpdk: bool,
    pub remove_cloud_hypervisor: bool,
    pub purge_user_data: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut o = Options::default();
        for a in args {
            match a.as_str() {
                "--dry-run" | "-n" => o.dry_run = true,
                "--yes" | "-y" => o.yes = true,
                "--remove-ovs" => o.remove_ovs = true,
                "--reset-dpdk" => o.reset_dpdk = true,
                "--remove-cloud-hypervisor" => o.remove_cloud_hypervisor = true,
                "--purge-user-data" => o.purge_user_data = true,
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => bail!("unknown option '{}' (see --help)", other),
            }
        }
        Ok(o)
    }

    fn to_args(&self) -> Vec<&'static str> {
        let mut v = vec!["uninstall"];
        for (on, flag) in [
            (self.yes, "--yes"),
            (self.remove_ovs, "--remove-ovs"),
            (self.reset_dpdk, "--reset-dpdk"),
            (self.remove_cloud_hypervisor, "--remove-cloud-hypervisor"),
            (self.purge_user_data, "--purge-user-data"),
        ] {
            if on {
                v.push(flag);
            }
        }
        v
    }
}

fn print_help() {
    println!(
        "Usage: glidex-install uninstall [options]\n\n\
         Removes glidex's systemd units, binaries, configuration and state,\n\
         and tears down glidex host networking (restoring uplink NICs).\n\n\
         Options:\n\
         \x20 -n, --dry-run                show the plan, change nothing\n\
         \x20 -y, --yes                    don't ask for confirmation\n\
         \x20     --remove-ovs             also remove Open vSwitch (distro packages\n\
         \x20                              or the glidex source build in /opt/glidex)\n\
         \x20     --reset-dpdk             clear DPDK settings glidex put in OVS\n\
         \x20     --remove-cloud-hypervisor also delete the cloud-hypervisor binary\n\
         \x20     --purge-user-data        also delete ~/.glidex (VMs, credentials, firmware, images, disks)"
    );
}

/// What the plan needs to know about the host (fakeable in tests).
pub trait HostView {
    fn exists(&self, path: &Path) -> bool;
    fn unit_known(&self, unit: &str) -> bool;
    fn group_exists(&self, name: &str) -> bool;
    fn has_net_admin(&self, bin: &Path) -> bool;
    fn package_installed(&self, name: &str) -> bool;
    fn read(&self, path: &Path) -> Option<String>;
    /// Files matching `/tmp/cloud-hypervisor-*.cloudinit.img`.
    fn seed_images(&self) -> Vec<PathBuf>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A command run as root.
    Run(Vec<String>),
    /// Tear down glidex networking from netd's state database.
    NetworkTeardown(PathBuf),
    /// Stop dnsmasq processes started by glidex-netd.
    StopGlidexDnsmasq,
    /// Drop netd's `inet glidex` nftables table if it's still there.
    DropNftTable,
    /// Remove a file or a directory tree.
    Remove(PathBuf),
    /// Replace a file's contents.
    Write(PathBuf, String),
    Note(String),
}

impl Step {
    pub fn describe(&self) -> String {
        match self {
            Step::Run(argv) => format!("run: {}", argv.join(" ")),
            Step::NetworkTeardown(db) => format!(
                "tear down glidex networking from {} (VM ports, uplinks → NICs restored, NAT, bridges)",
                db.display()
            ),
            Step::StopGlidexDnsmasq => "stop dnsmasq processes started by glidex-netd".into(),
            Step::DropNftTable => format!("drop nftables table inet {} if present", glidex_ovs::nat::NFT_TABLE),
            Step::Remove(p) => format!("remove: {}", p.display()),
            Step::Write(p, _) => format!("rewrite: {}", p.display()),
            Step::Note(n) => format!("note: {}", n),
        }
    }
}

fn run(argv: &[&str]) -> Step {
    Step::Run(argv.iter().map(|s| s.to_string()).collect())
}

/// Where the installer may have put binaries for `user_home`.
fn bin_dirs(user_home: &Path) -> Vec<PathBuf> {
    vec![PathBuf::from(SYSTEM_BIN_DIR), user_home.join(".local/bin")]
}

/// Build the uninstall plan. Pure: all host facts come from `host`.
pub fn plan(host: &dyn HostView, opts: &Options, user_home: &Path) -> Vec<Step> {
    let mut steps = Vec::new();

    // 1. Services: the control plane first (its shutdown stops VMs), then netd.
    let units: Vec<&str> = UNITS.iter().copied().filter(|u| host.unit_known(u)).collect();
    if !units.is_empty() {
        let mut argv = vec!["systemctl", "disable", "--now"];
        argv.extend(&units);
        steps.push(run(&argv));
    }

    // 2. Host networking, with netd's own teardown logic.
    if host.exists(Path::new(NETD_DB)) {
        steps.push(Step::NetworkTeardown(PathBuf::from(NETD_DB)));
    }
    steps.push(Step::StopGlidexDnsmasq);
    if host.exists(Path::new("/usr/sbin/nft")) {
        // The teardown normally removes it; this catches tables netd's
        // state doesn't know about (missing or damaged netd.db).
        steps.push(Step::DropNftTable);
    }

    // 3. Capabilities / binaries.
    for dir in bin_dirs(user_home) {
        let ch = dir.join("cloud-hypervisor");
        if host.exists(&ch) {
            if opts.remove_cloud_hypervisor {
                steps.push(Step::Remove(ch));
            } else if host.has_net_admin(&ch) {
                steps.push(run(&["setcap", "-r", &ch.to_string_lossy()]));
            }
        }
        for b in BINARIES {
            let p = dir.join(b);
            if host.exists(&p) {
                steps.push(Step::Remove(p));
            }
        }
    }

    // 4. Unit files.
    let mut removed_units = false;
    for u in UNITS {
        let p = Path::new(UNIT_DIR).join(u);
        if host.exists(&p) {
            steps.push(Step::Remove(p));
            removed_units = true;
        }
    }

    // 5. OVS (optional).
    if opts.reset_dpdk && !opts.remove_ovs && host.exists(Path::new("/usr/bin/ovs-vsctl")) {
        steps.push(run(&[
            "ovs-vsctl", "--if-exists", "remove", "Open_vSwitch", ".", "other_config",
            "dpdk-init", "dpdk-socket-mem", "pmd-cpu-mask",
        ]));
        steps.push(Step::Note(
            "restart Open vSwitch for the DPDK change to take effect (e.g. systemctl restart openvswitch-switch)".into(),
        ));
    }
    if opts.remove_ovs {
        let src_units: Vec<&str> = OVS_SOURCE_UNITS.iter().copied().filter(|u| host.unit_known(u)).collect();
        if !src_units.is_empty() {
            let mut argv = vec!["systemctl", "disable", "--now"];
            argv.extend(&src_units);
            steps.push(run(&argv));
        }
        for u in OVS_SOURCE_UNITS {
            let p = Path::new(UNIT_DIR).join(u);
            if host.exists(&p) {
                steps.push(Step::Remove(p));
                removed_units = true;
            }
        }
        if host.exists(Path::new(SOURCE_PREFIX)) {
            steps.push(Step::Remove(PathBuf::from(SOURCE_PREFIX)));
        }
        let pkgs: Vec<&str> = DISTRO_OVS_PACKAGES.iter().copied().filter(|p| host.package_installed(p)).collect();
        if !pkgs.is_empty() {
            let mut argv = vec!["apt-get", "remove", "-y"];
            argv.extend(&pkgs);
            steps.push(run(&argv));
        }
    }
    if removed_units {
        steps.push(run(&["systemctl", "daemon-reload"]));
    }

    // 6. State, configuration, runtime files.
    for d in STATE_DIRS {
        if host.exists(Path::new(d)) {
            steps.push(Step::Remove(PathBuf::from(d)));
        }
    }
    for img in host.seed_images() {
        steps.push(Step::Remove(img));
    }
    if host.group_exists(GROUP) {
        steps.push(run(&["groupdel", GROUP]));
    }
    host_settings(host, opts, &mut steps);

    // 7. User data (optional).
    let user_data = user_home.join(".glidex");
    if host.exists(&user_data) {
        if opts.purge_user_data {
            steps.push(Step::Remove(user_data));
        } else {
            steps.push(Step::Note(format!(
                "keeping {} (VM database, credentials, firmware, images and disks); use --purge-user-data to remove it",
                user_data.display()
            )));
        }
    }
    steps
}

/// Put back the kernel settings the installer changed. Hugepages and
/// vfio-pci stay while OVS keeps its DPDK configuration, which fails to
/// start without them.
fn host_settings(host: &dyn HostView, opts: &Options, steps: &mut Vec<Step>) {
    let keep_dpdk = !(opts.remove_ovs || opts.reset_dpdk);
    if let Some(text) = host.read(Path::new(sysconfig::SYSCTL_DROPIN)) {
        let settings = sysconfig::parse(&text);
        let (restore, keep) = sysconfig::split_for_uninstall(&settings, keep_dpdk);
        for s in &restore {
            match &s.previous {
                Some(prev) if *prev != s.value => {
                    steps.push(run(&["sysctl", "-w", &format!("{}={}", s.key, prev)]));
                }
                Some(_) => {}
                None => steps.push(Step::Note(format!("{} = {} left as is (previous value unknown)", s.key, s.value))),
            }
        }
        if keep.is_empty() {
            steps.push(Step::Remove(PathBuf::from(sysconfig::SYSCTL_DROPIN)));
        } else {
            steps.push(Step::Write(PathBuf::from(sysconfig::SYSCTL_DROPIN), sysconfig::render(&keep)));
            steps.push(Step::Note(format!(
                "keeping {} in {} for OVS-DPDK; use --reset-dpdk or --remove-ovs to release it",
                sysconfig::NR_HUGEPAGES,
                sysconfig::SYSCTL_DROPIN
            )));
        }
    }
    if !keep_dpdk && host.exists(Path::new(sysconfig::MODULES_DROPIN)) {
        steps.push(Step::Remove(PathBuf::from(sysconfig::MODULES_DROPIN)));
    }
}

// ---- real host ---------------------------------------------------------------

struct RealHost;

impl HostView for RealHost {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn unit_known(&self, unit: &str) -> bool {
        Path::new(UNIT_DIR).join(unit).exists()
            || Command::new("systemctl")
                .args(["is-active", "--quiet", unit])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
    }
    fn group_exists(&self, name: &str) -> bool {
        Command::new("getent").args(["group", name]).output().map(|o| o.status.success()).unwrap_or(false)
    }
    fn has_net_admin(&self, bin: &Path) -> bool {
        Command::new("getcap")
            .arg(bin)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("cap_net_admin"))
            .unwrap_or(false)
    }
    fn package_installed(&self, name: &str) -> bool {
        Command::new("dpkg-query")
            .args(["-W", "-f=${Status}", name])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("install ok installed"))
            .unwrap_or(false)
    }
    fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
    fn seed_images(&self) -> Vec<PathBuf> {
        std::fs::read_dir("/tmp")
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                            n.starts_with("cloud-hypervisor-") && n.ends_with(".cloudinit.img")
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Tear down everything glidex-netd created, using its own code on its own
/// state database (netd must be stopped: the database is single-process).
fn network_teardown(db: &Path) -> Result<Vec<String>> {
    use glidex_netd::auth::Peer;
    use glidex_netd::proto::{BridgeRecord, NatInfo, Op, UplinkResult, VmPortRecord};
    use glidex_netd::server::{Config, Netd};
    use glidex_netd::supervisor::FakeSupervisor;
    use std::sync::Arc;

    let netd = Netd::new(
        Arc::new(glidex_ovs::SystemExec::new()),
        Arc::new(FakeSupervisor::default()),
        Config {
            state_path: db.to_path_buf(),
            ..Config::default()
        },
    )
    .map_err(|e| anyhow::anyhow!("{} (is glidex-netd still running?)", e))?;
    let root = Peer { uid: 0, gid: 0, pid: std::process::id() as i32 };
    let call = |op: Op| netd.handle(op, &root).map_err(|e| anyhow::anyhow!(e.to_string()));
    let mut done = Vec::new();
    let mut failed = Vec::new();

    let ports: Vec<VmPortRecord> = serde_json::from_value(call(Op::ListVmPorts)?)?;
    for p in ports {
        match call(Op::ReleaseVm { vm_id: p.spec.vm_id.clone() }) {
            Ok(_) => done.push(format!("VM port {}", p.port)),
            Err(e) => failed.push(format!("VM port {}: {}", p.port, e)),
        }
    }
    let uplinks: Vec<UplinkResult> = serde_json::from_value(call(Op::ListUplinks)?)?;
    for u in uplinks {
        let spec = &u.record.spec;
        match call(Op::DeleteUplink { bridge: spec.bridge.clone(), name: spec.name.clone() }) {
            Ok(_) => done.push(format!("uplink {} (NIC restored)", spec.name)),
            Err(e) => failed.push(format!("uplink {}: {}", spec.name, e)),
        }
    }
    let nats: Vec<NatInfo> = serde_json::from_value(call(Op::ListNat)?)?;
    for n in nats {
        match call(Op::DeleteNat { bridge: n.state.bridge.clone() }) {
            Ok(_) => done.push(format!("NAT on {}", n.state.bridge)),
            Err(e) => failed.push(format!("NAT {}: {}", n.state.bridge, e)),
        }
    }
    let bridges: Vec<BridgeRecord> = serde_json::from_value(call(Op::ListBridges)?)?;
    for b in bridges {
        match call(Op::DeleteBridge { name: b.spec.name.clone() }) {
            Ok(_) => done.push(format!("bridge {}", b.spec.name)),
            Err(e) => failed.push(format!("bridge {}: {}", b.spec.name, e)),
        }
    }
    if !failed.is_empty() {
        bail!("network teardown incomplete:\n  {}", failed.join("\n  "));
    }
    Ok(done)
}

/// dnsmasq started by netd runs with --conf-file=/run/glidex/dnsmasq/*.conf.
fn stop_glidex_dnsmasq() -> usize {
    let mut stopped = 0;
    let Ok(procs) = std::fs::read_dir("/proc") else { return 0 };
    for p in procs.filter_map(|e| e.ok()) {
        let Ok(pid) = p.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Ok(cmdline) = std::fs::read(p.path().join("cmdline")) else { continue };
        let args: Vec<String> = cmdline.split(|b| *b == 0).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        let is_dnsmasq = args.first().is_some_and(|a| a.rsplit('/').next() == Some("dnsmasq"));
        if is_dnsmasq && args.iter().any(|a| a.starts_with("--conf-file=/run/glidex/dnsmasq/")) {
            if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                stopped += 1;
            }
        }
    }
    stopped
}

fn execute(step: &Step) -> Result<()> {
    match step {
        Step::Run(argv) => {
            let status = Command::new(&argv[0]).args(&argv[1..]).status().with_context(|| argv[0].clone())?;
            // Best effort: keep going so the rest of the cleanup happens.
            if !status.success() {
                println!("  {} {} exited with {}", "warning:".yellow(), argv.join(" "), status);
            }
        }
        Step::NetworkTeardown(db) => {
            for d in network_teardown(db)? {
                println!("  removed {}", d);
            }
        }
        Step::DropNftTable => {
            // netd's script for "no NAT networks": create-then-delete, so
            // it succeeds whether or not the table exists.
            let mut child = Command::new("nft")
                .args(["-f", "-"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .context("nft")?;
            use std::io::Write as _;
            child.stdin.take().expect("piped stdin").write_all(glidex_ovs::nat::nft_script(&[]).as_bytes())?;
            let status = child.wait()?;
            if !status.success() {
                bail!("nft -f - exited with {}", status);
            }
        }
        Step::StopGlidexDnsmasq => {
            let n = stop_glidex_dnsmasq();
            if n > 0 {
                println!("  stopped {} dnsmasq process(es)", n);
            }
        }
        Step::Remove(p) => {
            let r = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
            match r {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| p.display().to_string()),
            }
        }
        Step::Write(p, contents) => std::fs::write(p, contents).with_context(|| p.display().to_string())?,
        Step::Note(_) => {}
    }
    Ok(())
}

fn invoking_user_home() -> PathBuf {
    let user = std::env::var("SUDO_USER").ok().filter(|u| u != "root");
    if let Some(u) = user {
        if let Ok(out) = Command::new("getent").args(["passwd", &u]).output() {
            if let Some(home) = String::from_utf8_lossy(&out.stdout).trim().split(':').nth(5) {
                return PathBuf::from(home);
            }
        }
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/root"))
}

pub fn main(args: &[String]) -> Result<()> {
    let opts = Options::parse(args)?;
    let is_root = unsafe { libc::geteuid() } == 0;
    if !is_root && !opts.dry_run {
        // Re-run as root; sudo keeps SUDO_USER so ~ still means the user's home.
        let exe = std::env::current_exe()?;
        let status = Command::new("sudo").arg(exe).args(opts.to_args()).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }

    let home = invoking_user_home();
    let steps = plan(&RealHost, &opts, &home);
    println!("{}", "glidex uninstall plan:".cyan().bold());
    if steps.iter().all(|s| matches!(s, Step::Note(_) | Step::StopGlidexDnsmasq | Step::DropNftTable)) {
        println!("  nothing glidex-owned found");
    }
    for s in &steps {
        println!("  - {}", s.describe());
    }
    if opts.dry_run {
        println!("{}", "dry run: nothing changed".yellow());
        return Ok(());
    }
    if !opts.yes && !crate::confirm_yn("Proceed?", false)? {
        println!("Cancelled");
        return Ok(());
    }
    let mut errors = 0;
    for s in &steps {
        if let Step::Note(n) = s {
            println!("{} {}", "Note:".yellow(), n);
            continue;
        }
        println!("{} {}", "→".cyan(), s.describe());
        if let Err(e) = execute(s) {
            errors += 1;
            println!("  {} {:#}", "error:".red(), e);
            if matches!(s, Step::NetworkTeardown(_)) {
                // Don't delete netd's state while it still describes live
                // host changes (e.g. a migrated IP): it's needed to undo them.
                bail!("stopping before removing state: fix the error above and re-run");
            }
        }
    }
    if errors == 0 {
        println!("{}", "glidex uninstalled".green().bold());
    } else {
        println!("{} {} step(s) failed", "Finished with errors:".yellow(), errors);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[derive(Default)]
    struct FakeHost {
        paths: HashSet<PathBuf>,
        units: HashSet<String>,
        groups: HashSet<String>,
        caps: HashSet<PathBuf>,
        packages: HashSet<String>,
        seeds: Vec<PathBuf>,
        files: HashMap<PathBuf, String>,
    }

    impl HostView for FakeHost {
        fn exists(&self, p: &Path) -> bool {
            self.paths.contains(p)
        }
        fn unit_known(&self, u: &str) -> bool {
            self.units.contains(u)
        }
        fn group_exists(&self, g: &str) -> bool {
            self.groups.contains(g)
        }
        fn has_net_admin(&self, b: &Path) -> bool {
            self.caps.contains(b)
        }
        fn package_installed(&self, n: &str) -> bool {
            self.packages.contains(n)
        }
        fn read(&self, p: &Path) -> Option<String> {
            self.files.get(p).cloned()
        }
        fn seed_images(&self) -> Vec<PathBuf> {
            self.seeds.clone()
        }
    }

    fn full_install() -> FakeHost {
        let mut h = FakeHost::default();
        for p in [
            "/etc/systemd/system/glidex-control-plane.service",
            "/etc/systemd/system/glidex-netd.service",
            "/usr/local/bin/glidex-netd",
            "/usr/local/bin/cloud-hypervisor",
            "/home/alice/.local/bin/glidex-control-plane",
            "/home/alice/.local/bin/gxctl",
            "/var/lib/glidex/netd.db",
            "/var/lib/glidex",
            "/run/glidex",
            "/usr/sbin/nft",
            "/usr/bin/ovs-vsctl",
            "/home/alice/.glidex",
        ] {
            h.paths.insert(PathBuf::from(p));
        }
        h.units.extend(["glidex-control-plane.service".to_string(), "glidex-netd.service".to_string()]);
        h.groups.insert("glidex".into());
        h.caps.insert(PathBuf::from("/usr/local/bin/cloud-hypervisor"));
        h.packages.insert("openvswitch-switch".into());
        h.seeds.push(PathBuf::from("/tmp/cloud-hypervisor-abc.cloudinit.img"));
        h
    }

    fn lines(steps: &[Step]) -> Vec<String> {
        steps.iter().map(Step::describe).collect()
    }

    #[test]
    fn default_plan_order_and_contents() {
        let steps = plan(&full_install(), &Options::default(), Path::new("/home/alice"));
        let l = lines(&steps);
        let pos = |needle: &str| l.iter().position(|s| s.contains(needle)).unwrap_or_else(|| panic!("missing {needle}: {l:#?}"));
        // Services stop before networking is torn down, which happens before
        // netd's state (which describes it) is deleted.
        assert!(pos("systemctl disable --now glidex-control-plane.service glidex-netd.service") < pos("tear down glidex networking"));
        assert!(pos("tear down glidex networking") < pos("remove: /var/lib/glidex"));
        // The nft fallback runs after the teardown, as an idempotent script.
        assert!(pos("tear down glidex networking") < pos("drop nftables table inet glidex if present"));
        assert!(!l.iter().any(|s| s.contains("nft delete table")), "{l:#?}");
        for needle in [
            "setcap -r /usr/local/bin/cloud-hypervisor",
            "remove: /usr/local/bin/glidex-netd",
            "remove: /home/alice/.local/bin/glidex-control-plane",
            "remove: /home/alice/.local/bin/gxctl",
            "remove: /etc/systemd/system/glidex-netd.service",
            "systemctl daemon-reload",
            "remove: /run/glidex",
            "remove: /tmp/cloud-hypervisor-abc.cloudinit.img",
            "groupdel glidex",
            "keeping /home/alice/.glidex",
        ] {
            pos(needle);
        }
        // Not without the flags.
        for absent in ["apt-get remove", "remove: /usr/local/bin/cloud-hypervisor", "remove: /home/alice/.glidex", "dpdk-init"] {
            assert!(!l.iter().any(|s| s.contains(absent)), "unexpected {absent}");
        }
    }

    #[test]
    fn optional_removals() {
        let mut host = full_install();
        host.paths.insert(PathBuf::from("/opt/glidex"));
        host.paths.insert(PathBuf::from("/etc/systemd/system/glidex-ovs-vswitchd.service"));
        host.units.insert("glidex-ovs-vswitchd.service".into());
        let opts = Options { remove_ovs: true, remove_cloud_hypervisor: true, purge_user_data: true, ..Options::default() };
        let l = lines(&plan(&host, &opts, Path::new("/home/alice")));
        for needle in [
            "systemctl disable --now glidex-ovs-vswitchd.service",
            "remove: /etc/systemd/system/glidex-ovs-vswitchd.service",
            "remove: /opt/glidex",
            "apt-get remove -y openvswitch-switch",
            "remove: /usr/local/bin/cloud-hypervisor",
            "remove: /home/alice/.glidex",
        ] {
            assert!(l.iter().any(|s| s.contains(needle)), "missing {needle}: {l:#?}");
        }
        assert!(!l.iter().any(|s| s.contains("setcap -r")), "binary removed instead");
    }

    #[test]
    fn reset_dpdk_only_when_keeping_ovs() {
        let opts = Options { reset_dpdk: true, ..Options::default() };
        let l = lines(&plan(&full_install(), &opts, Path::new("/home/alice")));
        assert!(l.iter().any(|s| s.contains("remove Open_vSwitch . other_config dpdk-init dpdk-socket-mem pmd-cpu-mask")));
    }

    #[test]
    fn clean_host_has_nothing_to_do() {
        let l = lines(&plan(&FakeHost::default(), &Options::default(), Path::new("/home/alice")));
        assert_eq!(l, vec!["stop dnsmasq processes started by glidex-netd"]);
    }

    fn with_sysctls(mut h: FakeHost) -> FakeHost {
        let text = sysconfig::render(&[
            sysconfig::Setting { key: "net.ipv4.ip_forward".into(), value: "1".into(), previous: Some("0".into()) },
            sysconfig::Setting { key: "vm.nr_hugepages".into(), value: "2048".into(), previous: Some("0".into()) },
        ]);
        h.files.insert(PathBuf::from(sysconfig::SYSCTL_DROPIN), text);
        h.paths.insert(PathBuf::from(sysconfig::MODULES_DROPIN));
        h
    }

    #[test]
    fn sysctls_restored_but_hugepages_kept_for_ovs_dpdk() {
        let steps = plan(&with_sysctls(full_install()), &Options::default(), Path::new("/home/alice"));
        let l = lines(&steps);
        assert!(l.contains(&"run: sysctl -w net.ipv4.ip_forward=0".to_string()), "{l:#?}");
        assert!(!l.iter().any(|s| s.contains("vm.nr_hugepages=0")), "{l:#?}");
        let Some(Step::Write(_, kept)) = steps.iter().find(|s| matches!(s, Step::Write(..))) else { panic!("{l:#?}") };
        let kept = sysconfig::parse(kept);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].key, "vm.nr_hugepages");
        assert!(!l.iter().any(|s| s.contains("modules-load.d")), "vfio-pci kept with DPDK");
    }

    #[test]
    fn sysctls_fully_restored_with_reset_dpdk() {
        let opts = Options { reset_dpdk: true, ..Options::default() };
        let l = lines(&plan(&with_sysctls(full_install()), &opts, Path::new("/home/alice")));
        for needle in [
            "run: sysctl -w net.ipv4.ip_forward=0",
            "run: sysctl -w vm.nr_hugepages=0",
            "remove: /etc/sysctl.d/90-glidex.conf",
            "remove: /etc/modules-load.d/glidex.conf",
        ] {
            assert!(l.iter().any(|s| s == needle), "missing {needle}: {l:#?}");
        }
    }

    #[test]
    fn options_round_trip_for_sudo() {
        let args: Vec<String> = ["--yes", "--remove-ovs", "--purge-user-data"].iter().map(|s| s.to_string()).collect();
        let o = Options::parse(&args).unwrap();
        assert_eq!(o.to_args(), vec!["uninstall", "--yes", "--remove-ovs", "--purge-user-data"]);
        assert!(Options::parse(&["--bogus".to_string()]).is_err());
    }
}
