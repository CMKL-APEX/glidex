use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::TempDir;

mod sysconfig;
mod uninstall;

const CLOUD_HYPERVISOR_VERSION: &str = "v53.0";
/// Release tag of https://github.com/cloud-hypervisor/edk2/releases to fetch
/// the UEFI firmware from. Bump together with the digests in
/// `firmware_asset`.
const EDK2_FIRMWARE_VERSION: &str = "ch-811ce5ea35";

/// The system user every glidex unit but glidex-netd (the root helper)
/// runs as. Its primary group is also the group allowed to use netd.
const SERVICE_USER: &str = "glidex";
const NETD_GROUP: &str = "glidex";
/// The service user's home: the control plane keeps its database, images,
/// disks and firmware in `<home>/.glidex`, like an interactive run.
pub(crate) const SERVICE_HOME: &str = "/var/lib/glidex-control-plane";
const BIN_DIR: &str = "/usr/local/bin";
/// The built web UI that glidex-ui.service serves.
const UI_ASSET_DIR: &str = "/usr/local/share/glidex/ui";
/// Choices from earlier runs (flags), so a re-run keeps the same setup.
const INSTALL_CONF: &str = "/etc/glidex/install.conf";

const NETD_UNIT: &str = "/etc/systemd/system/glidex-netd.service";
const CONTROL_PLANE_UNIT: &str = "/etc/systemd/system/glidex-control-plane.service";
const UI_UNIT: &str = "/etc/systemd/system/glidex-ui.service";
const API_ADDR: &str = "127.0.0.1:8841";
const UI_ADDR: &str = "127.0.0.1:5173";

struct Platform {
    os: &'static str,
    arch: &'static str,
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("uninstall") {
        return uninstall::main(&args[1..]);
    }
    let saved = fs::read_to_string(INSTALL_CONF).ok();
    let opts = Options::parse(saved.as_deref(), &args)?;
    print_banner();

    let platform = detect_platform()?;
    println!("{} {}-{}", "Detected platform:".green(), platform.os, platform.arch);
    print_plan(&opts);

    install_system_packages(&opts, &platform)?;
    let cargo = install_rust()?;
    let bun = install_bun()?;
    check_kvm();
    build_project(&cargo)?;
    build_ui(&bun)?;

    let user = invoking_user();
    let user_home = invoking_user_home(&user)?;
    let mut changed = Changes::default();
    if opts.services {
        ensure_service_user(user.as_deref())?;
    }
    install_cloud_hypervisor(&platform)?;
    install_binaries(&opts, &user_home, &mut changed)?;
    install_ui_assets()?;
    // Our own home, not SUDO_USER's: a root run mustn't leave root-owned
    // files in the user's ~/.glidex.
    let own_home = dirs::home_dir().context("could not determine the home directory")?;
    install_uefi_firmware(&platform, &own_home, opts.services)?;
    if opts.networking {
        setup_networking(&opts, &changed)?;
    }
    if opts.services {
        install_services(&changed, user.as_deref(), &user_home)?;
    }
    opts.save(saved.as_deref())?;
    print_usage(&opts);
    Ok(())
}

fn print_banner() {
    let banner = r"
   _____ _ _     _
  / ____| (_)   | |
 | |  __| |_  __| | _____  __
 | | |_ | | |/ _` |/ _ \ \/ /
 | |__| | | | (_| |  __/>  <
  \_____|_|_|\__,_|\___/_/\_\
        Control Plane Installer
";
    println!("{}", banner.cyan());
}

fn detect_platform() -> Result<Platform> {
    let os = env::consts::OS;
    if os != "linux" {
        bail!(
            "Cloud-Hypervisor and QEMU only support Linux (detected: {})",
            os
        );
    }
    let arch = match env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => bail!("Unsupported architecture: {}", other),
    };
    Ok(Platform { os, arch })
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

// --- Options ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OvsProfile {
    Dpdk,
    Kernel,
}

impl OvsProfile {
    fn name(self) -> &'static str {
        match self {
            OvsProfile::Dpdk => "dpdk",
            OvsProfile::Kernel => "kernel",
        }
    }
    fn parse(s: &str) -> Result<Self> {
        match s {
            "dpdk" => Ok(OvsProfile::Dpdk),
            "kernel" => Ok(OvsProfile::Kernel),
            other => bail!("OVS profile must be dpdk or kernel, not '{}'", other),
        }
    }
}

/// What to install. Defaults, then what `INSTALL_CONF` recorded, then flags;
/// all but `allow_ovs_restart` are saved for the next run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    qemu: bool,
    networking: bool,
    services: bool,
    ovs_profile: OvsProfile,
    pmd_cpu_mask: Option<String>,
    /// Let OVS installs / DPDK init restart ovs-vswitchd while it has
    /// bridges (interrupts their traffic). Never saved.
    allow_ovs_restart: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            qemu: true,
            networking: true,
            services: true,
            ovs_profile: OvsProfile::Dpdk,
            pmd_cpu_mask: None,
            allow_ovs_restart: false,
        }
    }
}

impl Options {
    fn parse(saved: Option<&str>, args: &[String]) -> Result<Self> {
        let mut o = Options::default();
        for line in saved.unwrap_or_default().lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "qemu" => o.qemu = v == "true",
                "networking" => o.networking = v == "true",
                "services" => o.services = v == "true",
                "ovs_profile" => o.ovs_profile = OvsProfile::parse(v).unwrap_or(OvsProfile::Dpdk),
                "pmd_cpu_mask" => o.pmd_cpu_mask = (!v.is_empty()).then(|| v.to_string()),
                _ => {}
            }
        }
        let mut args = args.iter();
        while let Some(a) = args.next() {
            let mut value = |name: &str| args.next().cloned().with_context(|| format!("{} needs a value", name));
            match a.as_str() {
                "--qemu" => o.qemu = true,
                "--no-qemu" => o.qemu = false,
                "--networking" => o.networking = true,
                "--no-networking" => o.networking = false,
                "--services" => o.services = true,
                "--no-services" => o.services = false,
                "--ovs-profile" => o.ovs_profile = OvsProfile::parse(&value("--ovs-profile")?)?,
                "--pmd-cpu-mask" => {
                    let m = value("--pmd-cpu-mask")?;
                    o.pmd_cpu_mask = (!m.is_empty() && m != "auto").then_some(m);
                }
                "--allow-ovs-restart" => o.allow_ovs_restart = true,
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => bail!("unknown option '{}' (see --help)", other),
            }
        }
        Ok(o)
    }

    fn render(&self) -> String {
        format!(
            "# Written by glidex-install: the choices re-runs reuse (flags override).\n\
             qemu={}\nnetworking={}\nservices={}\novs_profile={}\npmd_cpu_mask={}\n",
            self.qemu,
            self.networking,
            self.services,
            self.ovs_profile.name(),
            self.pmd_cpu_mask.as_deref().unwrap_or("")
        )
    }

    fn save(&self, saved: Option<&str>) -> Result<()> {
        let text = self.render();
        if saved == Some(text.as_str()) {
            return Ok(());
        }
        sudo_write(INSTALL_CONF, &text)
    }
}

fn print_help() {
    println!(
        "Usage: glidex-install [options]\n       glidex-install uninstall --help\n\n\
         Installs glidex and its dependencies, or brings an existing install up\n\
         to date. Safe to re-run; it only asks for your sudo password.\n\
         Choices are remembered in {INSTALL_CONF}.\n\n\
         Options:\n\
         \x20     --no-qemu              skip QEMU and OVMF (Cloud Hypervisor only)\n\
         \x20     --no-networking        skip Open vSwitch, glidex-netd and host settings\n\
         \x20     --no-services          don't install the systemd units / glidex user\n\
         \x20     --ovs-profile P        dpdk (default; reserves hugepages) or kernel\n\
         \x20     --pmd-cpu-mask MASK    OVS-DPDK PMD CPU mask, hex (\"auto\" to clear)\n\
         \x20     --allow-ovs-restart    allow restarting ovs-vswitchd while it has bridges\n\
         \x20     --qemu, --networking, --services   undo an earlier --no-*"
    );
}

fn print_plan(opts: &Options) {
    let on = |b: bool| if b { "yes".green() } else { "no".yellow() };
    println!();
    println!("{}", "Installing / updating:".cyan().bold());
    println!("  - System packages (build tools, OpenSSL headers, disk and cloud-init tools)");
    println!("  - Rust (rustup), Bun");
    println!("  - Cloud-Hypervisor {} and its UEFI firmware ({})", CLOUD_HYPERVISOR_VERSION, EDK2_FIRMWARE_VERSION);
    println!("  - glidex binaries in {}, web UI in {}", BIN_DIR, UI_ASSET_DIR);
    println!("  - QEMU + OVMF: {}", on(opts.qemu));
    println!(
        "  - VM networking (Open vSwitch {}, glidex-netd): {}",
        opts.ovs_profile.name(),
        on(opts.networking)
    );
    println!(
        "  - systemd units (control plane and web UI as user '{}'): {}",
        SERVICE_USER,
        on(opts.services)
    );
    println!();
}

fn section(title: &str) {
    println!();
    println!("{} {} {}", "===".cyan(), title.cyan().bold(), "===".cyan());
}

// --- Prompting (uninstall only) ---

fn prompt_line(msg: &str) -> Result<String> {
    print!("{}", msg);
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn confirm_yn(msg: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let input = prompt_line(&format!("{} {} ", msg, hint))?;
    if input.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(input.to_lowercase().as_str(), "y" | "yes"))
}

// --- Command helpers ---

fn command_exists(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {}", cmd))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("Failed to spawn {}", cmd))?;
    if !status.success() {
        bail!("{} {:?} exited with {}", cmd, args, status);
    }
    Ok(())
}

fn run_in(cmd: &str, args: &[&str], cwd: &Path) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("Failed to spawn {}", cmd))?;
    if !status.success() {
        bail!("{} {:?} exited with {}", cmd, args, status);
    }
    Ok(())
}

fn run_capture(cmd: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("Failed to spawn {}", cmd))?;
    if !output.status.success() {
        bail!(
            "{} {:?} failed: {}",
            cmd,
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Exit status only, output discarded.
fn succeeds(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_sh(script: &str) -> Result<()> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(script)
        .status()
        .with_context(|| format!("Failed to run: {}", script))?;
    if !status.success() {
        bail!("Shell command failed: {}", script);
    }
    Ok(())
}

fn download(url: &str, dest: &Path) -> Result<()> {
    if !command_exists("curl") {
        bail!("curl is required to download {}", url);
    }
    let status = Command::new("curl")
        .args(["-fSL", "--progress-bar", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .context("Failed to invoke curl")?;
    if !status.success() {
        bail!("Failed to download {}", url);
    }
    Ok(())
}

// --- Privilege ---

static SUDO_READY: AtomicBool = AtomicBool::new(false);

/// Ask for the sudo password the first time something needs root, then keep
/// the credentials fresh so a long build doesn't make sudo ask again (or
/// time out a command waiting for the password).
fn ensure_sudo() -> Result<()> {
    if is_root() || SUDO_READY.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    println!("{}", "Administrator access (sudo) is needed for system changes.".yellow());
    if let Err(e) = run("sudo", &["-v"]) {
        SUDO_READY.store(false, Ordering::SeqCst);
        return Err(e);
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
        let _ = Command::new("sudo").args(["-n", "-v"]).stderr(Stdio::null()).status();
    });
    Ok(())
}

/// Run a command as root (through sudo when not root).
fn sudo(args: &[String]) -> Result<()> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if is_root() {
        run(refs[0], &refs[1..])
    } else {
        ensure_sudo()?;
        run("sudo", &refs)
    }
}

fn argv(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Write a root-owned 0644 file (through sudo when not root).
fn sudo_write(path: &str, contents: &str) -> Result<()> {
    let tmp = tempfile::NamedTempFile::new()?;
    fs::write(tmp.path(), contents)?;
    sudo(&argv(&["install", "-D", "-m", "0644", &tmp.path().to_string_lossy(), path]))
}

/// Whether `dst` already has `src`'s contents.
fn same_file(src: &Path, dst: &Path) -> bool {
    match (fs::read(src), fs::read(dst)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Install `src` as root-owned `dst` with `mode` unless it is already
/// identical. Returns whether it changed.
fn install_if_changed(src: &Path, dst: &str, mode: &str) -> Result<bool> {
    if same_file(src, Path::new(dst)) {
        return Ok(false);
    }
    sudo(&argv(&["install", "-D", "-m", mode, "-o", "root", "-g", "root", &src.to_string_lossy(), dst]))?;
    Ok(true)
}

/// `install_if_changed` for generated text.
fn write_if_changed(dst: &str, contents: &str) -> Result<bool> {
    if fs::read_to_string(dst).ok().as_deref() == Some(contents) {
        return Ok(false);
    }
    sudo_write(dst, contents)?;
    Ok(true)
}

/// The user who ran the installer (through sudo too), unless that's root.
fn invoking_user() -> Option<String> {
    env::var("SUDO_USER")
        .or_else(|_| env::var("USER"))
        .ok()
        .filter(|u| !u.is_empty() && u != "root")
}

fn invoking_user_home(user: &Option<String>) -> Result<PathBuf> {
    match user {
        Some(u) => run_capture("getent", &["passwd", u])
            .ok()
            .and_then(|l| l.trim().split(':').nth(5).map(PathBuf::from))
            .or_else(dirs::home_dir)
            .context("could not determine your home directory"),
        None => dirs::home_dir().context("could not determine the home directory"),
    }
}

// --- System packages ---

/// How to tell a dependency is already there.
#[derive(Debug, Clone, Copy)]
enum Probe {
    /// On PATH (or in an sbin directory).
    Cmd(&'static str),
    /// `pkg-config --exists`.
    PkgConfig(&'static str),
    /// Any of these files exists.
    AnyFile(&'static [&'static str]),
}

/// A system dependency and its package for apt, dnf/yum and pacman.
#[derive(Debug, Clone, Copy)]
struct Dep {
    probe: Probe,
    packages: [&'static str; 3],
}

const fn dep(probe: Probe, packages: [&'static str; 3]) -> Dep {
    Dep { probe, packages }
}

/// Build tools (unzip for bun's installer, pkg-config and the OpenSSL
/// headers for `openssl-sys`, clang), the cloud-init seed tools, and the
/// tools the image and disk module shells out to (spec/images.md §9).
const BASE_DEPS: &[Dep] = &[
    dep(Probe::Cmd("curl"), ["curl", "curl", "curl"]),
    dep(Probe::Cmd("unzip"), ["unzip", "unzip", "unzip"]),
    dep(Probe::PkgConfig("openssl"), ["libssl-dev", "openssl-devel", "openssl"]),
    dep(Probe::Cmd("pkg-config"), ["pkg-config", "pkgconfig", "pkgconf"]),
    dep(Probe::Cmd("clang"), ["clang", "clang", "clang"]),
    dep(Probe::Cmd("mkdosfs"), ["dosfstools", "dosfstools", "dosfstools"]),
    dep(Probe::Cmd("mcopy"), ["mtools", "mtools", "mtools"]),
    dep(Probe::Cmd("qemu-img"), ["qemu-utils", "qemu-img", "qemu-img"]),
    dep(Probe::Cmd("qemu-io"), ["qemu-utils", "qemu-img", "qemu-img"]),
    dep(Probe::Cmd("sgdisk"), ["gdisk", "gdisk", "gptfdisk"]),
    dep(Probe::Cmd("growpart"), ["cloud-guest-utils", "cloud-utils-growpart", "cloud-guest-utils"]),
];

/// Where distributions put OVMF (kept in step with the control plane's
/// `hypervisor::qemu::OVMF_CODE_CANDIDATES`).
const OVMF_CODE_PATHS: &[&str] = &[
    "/usr/share/OVMF/OVMF_CODE_4M.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
    "/usr/share/edk2/ovmf/OVMF_CODE.fd",
    "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
];

/// QEMU and the OVMF UEFI firmware its cloud-image VMs boot from.
/// (`qemu-kvm` is no Debian/Ubuntu package any more.)
const QEMU_DEPS: &[Dep] = &[
    dep(Probe::Cmd("qemu-system-x86_64"), ["qemu-system-x86", "qemu-kvm", "qemu-base"]),
    dep(Probe::AnyFile(OVMF_CODE_PATHS), ["ovmf", "edk2-ovmf", "edk2-ovmf"]),
];

/// `setcap`, for cloud-hypervisor's CAP_NET_ADMIN (tap devices).
const NETWORKING_DEPS: &[Dep] = &[dep(Probe::Cmd("setcap"), ["libcap2-bin", "libcap", "libcap"])];

fn wanted_deps(opts: &Options, platform: &Platform) -> Vec<Dep> {
    let mut deps = BASE_DEPS.to_vec();
    if opts.qemu && platform.arch == "x86_64" {
        deps.extend_from_slice(QEMU_DEPS);
    }
    if opts.networking {
        deps.extend_from_slice(NETWORKING_DEPS);
    }
    deps
}

fn manager_column(manager: &str) -> usize {
    match manager {
        "apt-get" => 0,
        "pacman" => 2,
        _ => 1,
    }
}

/// Split dependencies into packages to install (dependency missing) and
/// packages to keep up to date (dependency present and installed from this
/// package; something installed another way is left alone).
fn package_plan(
    manager: &str,
    deps: &[Dep],
    present: impl Fn(&Probe) -> bool,
    installed: impl Fn(&str) -> bool,
) -> (Vec<&'static str>, Vec<&'static str>) {
    let col = manager_column(manager);
    let (mut missing, mut update) = (Vec::new(), Vec::new());
    for d in deps {
        let pkg = d.packages[col];
        let list = if !present(&d.probe) {
            &mut missing
        } else if installed(pkg) {
            &mut update
        } else {
            continue;
        };
        if !list.contains(&pkg) {
            list.push(pkg);
        }
    }
    update.retain(|p| !missing.contains(p));
    (missing, update)
}

/// `command -v`, or the sbin dirs that aren't on every user's PATH
/// (where `sgdisk` lives on Debian).
fn tool_present(cmd: &str) -> bool {
    command_exists(cmd)
        || ["/usr/sbin", "/sbin", "/usr/local/sbin"]
            .iter()
            .any(|d| Path::new(d).join(cmd).exists())
}

fn probe_present(p: &Probe) -> bool {
    match p {
        Probe::Cmd(c) => tool_present(c),
        Probe::PkgConfig(m) => command_exists("pkg-config") && succeeds("pkg-config", &["--exists", m]),
        Probe::AnyFile(paths) => paths.iter().any(|p| Path::new(p).exists()),
    }
}

/// The first supported package manager on PATH.
fn package_manager() -> Option<&'static str> {
    ["apt-get", "dnf", "yum", "pacman"]
        .into_iter()
        .find(|m| command_exists(m))
}

fn package_installed(manager: &str, pkg: &str) -> bool {
    match manager {
        "apt-get" => run_capture("dpkg-query", &["-W", "-f=${Status}", pkg])
            .is_ok_and(|s| s.contains("install ok installed")),
        "pacman" => succeeds("pacman", &["-Q", pkg]),
        _ => succeeds("rpm", &["-q", "--quiet", pkg]),
    }
}

/// Whether any of `pkgs` has a newer version in the package lists, checked
/// without root (apt's lists are as fresh as the system's apt timers keep
/// them). An unanswerable check counts as "maybe": update.
fn packages_upgradable(manager: &str, pkgs: &[&str]) -> bool {
    if pkgs.is_empty() {
        return false;
    }
    match manager {
        "apt-get" => {
            let mut args = vec!["-s", "-q", "install"];
            args.extend(pkgs);
            run_capture("apt-get", &args).map_or(true, |out| out.lines().any(|l| l.starts_with("Inst ")))
        }
        "pacman" => {
            let mut args = vec!["-Qu"];
            args.extend(pkgs);
            run_capture("pacman", &args).is_ok_and(|out| !out.trim().is_empty())
        }
        // dnf/yum check-update: 100 = updates available, 0 = none.
        m => {
            let mut args = vec!["-q", "check-update"];
            args.extend(pkgs);
            Command::new(m)
                .args(&args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_or(true, |s| s.code() != Some(0))
        }
    }
}

/// Root commands that install `missing` and upgrade `update`.
fn package_commands(manager: &str, missing: &[&str], update: &[&str]) -> Vec<Vec<String>> {
    let with = |base: &[&str], pkgs: &[&str]| {
        let mut v = argv(base);
        v.extend(pkgs.iter().map(|p| p.to_string()));
        v
    };
    let all: Vec<&str> = missing.iter().chain(update).copied().collect();
    if all.is_empty() {
        return vec![];
    }
    match manager {
        "apt-get" => vec![
            argv(&["apt-get", "update"]),
            with(&["env", "DEBIAN_FRONTEND=noninteractive", "apt-get", "install", "-y"], &all),
        ],
        "pacman" => vec![with(&["pacman", "-S", "--needed", "--noconfirm"], &all)],
        m => {
            let mut cmds = Vec::new();
            if !missing.is_empty() {
                cmds.push(with(&[m, "install", "-y"], missing));
            }
            if !update.is_empty() {
                cmds.push(with(&[m, "upgrade", "-y"], update));
            }
            cmds
        }
    }
}

fn install_system_packages(opts: &Options, platform: &Platform) -> Result<()> {
    section("System packages");
    let deps = wanted_deps(opts, platform);
    let Some(manager) = package_manager() else {
        let missing: Vec<String> = deps.iter().filter(|d| !probe_present(&d.probe)).map(|d| d.packages[0].to_string()).collect();
        if missing.is_empty() {
            println!("{}", "All dependencies are present (no supported package manager to update them).".green());
            return Ok(());
        }
        bail!("Could not detect a package manager; please install: {}", missing.join(", "));
    };
    if opts.qemu && platform.arch != "x86_64" {
        println!("{} QEMU support is x86_64 only; skipping QEMU.", "Note:".yellow());
    }
    let (missing, installed) = package_plan(manager, &deps, probe_present, |p| package_installed(manager, p));
    let update: Vec<&str> = if packages_upgradable(manager, &installed) { installed } else { vec![] };
    if missing.is_empty() && update.is_empty() {
        println!("{}", "System packages are installed and up to date.".green());
        return Ok(());
    }
    if !missing.is_empty() {
        println!("{} {}", "Installing:".yellow(), missing.join(" "));
    }
    if !update.is_empty() {
        println!("{} {}", "Updating:".yellow(), update.join(" "));
    }
    for cmd in package_commands(manager, &missing, &update) {
        sudo(&cmd)?;
    }
    Ok(())
}

// --- Toolchains ---

/// Rust via rustup, kept on the latest stable. Returns the cargo to build with.
fn install_rust() -> Result<String> {
    section("Rust");
    let home_cargo = dirs::home_dir().map(|h| h.join(".cargo/bin")).unwrap_or_default();
    if !command_exists("rustc") && !home_cargo.join("rustc").exists() {
        println!("{}", "Rust not found; installing via rustup...".yellow());
        run_sh(
            "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable",
        )?;
    }
    let rustup = if command_exists("rustup") { "rustup".to_string() } else { home_cargo.join("rustup").to_string_lossy().into_owned() };
    if Path::new(&rustup).exists() || command_exists(&rustup) {
        // Also updates rustup itself, unless it came from a distro package.
        if let Err(e) = run(&rustup, &["update", "stable"]) {
            println!("{} could not update Rust: {:#}", "Note:".yellow(), e);
        }
    } else {
        println!("{} Rust is not managed by rustup here; update it with your package manager.", "Note:".yellow());
    }
    let cargo = if command_exists("cargo") { "cargo".to_string() } else { home_cargo.join("cargo").to_string_lossy().into_owned() };
    let version = run_capture(&cargo, &["--version"]).unwrap_or_default();
    println!("{} {}", "Rust:".green(), version.trim());
    Ok(cargo)
}

/// Bun (builds the UI), kept up to date when its own installer put it in
/// ~/.bun. Returns the bun to run.
fn install_bun() -> Result<String> {
    section("Bun");
    let home_bun = dirs::home_dir().map(|h| h.join(".bun/bin/bun")).unwrap_or_default();
    if !command_exists("bun") && !home_bun.exists() {
        println!("{}", "Bun not found; installing...".yellow());
        run_sh("curl -fsSL https://bun.sh/install | bash")?;
    } else if home_bun.exists() {
        if let Err(e) = run(&home_bun.to_string_lossy(), &["upgrade"]) {
            println!("{} could not update Bun: {:#}", "Note:".yellow(), e);
        }
    } else {
        println!("{} Bun was not installed by bun.sh; update it the way you installed it.", "Note:".yellow());
    }
    let bun = if command_exists("bun") { "bun".to_string() } else { home_bun.to_string_lossy().into_owned() };
    let version = run_capture(&bun, &["--version"]).unwrap_or_default();
    println!("{} {}", "Bun:".green(), version.trim());
    Ok(bun)
}

// --- Hypervisor ---

/// What changed in this run, to decide which services need a restart.
#[derive(Debug, Default)]
struct Changes {
    control_plane: bool,
    ui: bool,
    netd: bool,
}

/// `cloud-hypervisor v53.0` (possibly with a suffix) → `v53.0`.
fn cloud_hypervisor_version(output: &str) -> Option<&str> {
    output.split_whitespace().nth(1).map(|v| v.split('-').next().unwrap_or(v))
}

/// The pinned static Cloud-Hypervisor in /usr/local/bin. Running VMs keep
/// their binary, so no service needs a restart for it.
fn install_cloud_hypervisor(platform: &Platform) -> Result<()> {
    section("Cloud-Hypervisor");
    let target = format!("{}/cloud-hypervisor", BIN_DIR);
    let current = run_capture(&target, &["--version"]).ok();
    if current.as_deref().and_then(cloud_hypervisor_version) == Some(CLOUD_HYPERVISOR_VERSION) {
        println!("{} {} ({})", "Cloud-Hypervisor is up to date:".green(), CLOUD_HYPERVISOR_VERSION, target);
        warn_shadowed("cloud-hypervisor", &target);
        return Ok(());
    }

    let binary_name = if platform.arch == "x86_64" {
        "cloud-hypervisor-static"
    } else {
        "cloud-hypervisor-static-aarch64"
    };
    let url = format!(
        "https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/{}/{}",
        CLOUD_HYPERVISOR_VERSION, binary_name
    );
    let tmp = TempDir::new()?;
    let download_path = tmp.path().join("cloud-hypervisor");
    println!("Downloading {}", url);
    download(&url, &download_path)?;
    install_if_changed(&download_path, &target, "0755")?;
    println!(
        "{} {} → {}",
        "Installed:".green(),
        current.as_deref().and_then(cloud_hypervisor_version).unwrap_or("none"),
        CLOUD_HYPERVISOR_VERSION
    );
    warn_shadowed("cloud-hypervisor", &target);
    Ok(())
}

/// Note when another copy of `name` comes first on the user's PATH.
fn warn_shadowed(name: &str, ours: &str) {
    if let Ok(found) = run_capture("sh", &["-c", &format!("command -v {}", name)]) {
        let found = found.trim();
        if !found.is_empty() && found != ours {
            println!(
                "{} {} comes before {} on your PATH; the services use {}.",
                "Note:".yellow(),
                found,
                ours,
                ours
            );
        }
    }
}

/// UEFI firmware asset name and its sha256 for `EDK2_FIRMWARE_VERSION`.
/// The control plane / gxctl look for the same file name in `~/.glidex`.
fn firmware_asset(platform: &Platform) -> (&'static str, &'static str) {
    if platform.arch == "x86_64" {
        (
            "CLOUDHV.fd",
            "db5c16e374efab916910a87e0d800fd94b4a55c32bc87e01481a300e5196136b",
        )
    } else {
        (
            "CLOUDHV_EFI.fd",
            "43570f9d7f8f8b87e0218956daa8f652260273d811ff791727215569f5812220",
        )
    }
}

fn file_sha256(path: &Path) -> Result<String> {
    let out = run_capture("sha256sum", &[path.to_str().unwrap()])?;
    out.split_whitespace()
        .next()
        .map(str::to_string)
        .context("sha256sum produced no output")
}

/// Download the EDK2 firmware Cloud-Hypervisor uses to boot UEFI disk
/// images (distro cloud images) into `home`'s `.glidex` (for gxctl and
/// interactive runs) and, with the services, the service user's.
fn install_uefi_firmware(platform: &Platform, home: &Path, services: bool) -> Result<()> {
    section("Cloud-Hypervisor UEFI Firmware");
    let (asset, sha256) = firmware_asset(platform);
    let glidex_dir = home.join(".glidex");
    fs::create_dir_all(&glidex_dir)?;
    let dest = glidex_dir.join(asset);

    if dest.exists() && file_sha256(&dest)? == sha256 {
        println!("{} {}", "Firmware is up to date:".green(), dest.display());
    } else {
        let url = format!(
            "https://github.com/cloud-hypervisor/edk2/releases/download/{}/{}",
            EDK2_FIRMWARE_VERSION, asset
        );
        // Download next to the destination so the final rename is atomic.
        let partial = glidex_dir.join(format!("{}.part", asset));
        println!("Downloading {}", url);
        download(&url, &partial)?;
        let actual = file_sha256(&partial)?;
        if actual != sha256 {
            let _ = fs::remove_file(&partial);
            bail!(
                "Checksum mismatch for {}: expected {}, got {}",
                url,
                sha256,
                actual
            );
        }
        fs::rename(&partial, &dest)?;
        println!("{} {}", "Installed:".green(), dest.display());
    }

    if services {
        let service_dest = format!("{}/.glidex/{}", SERVICE_HOME, asset);
        // Readable once the group membership applies; until then ask root.
        let current = file_sha256(Path::new(&service_dest)).ok().or_else(|| {
            if is_root() || ensure_sudo().is_err() {
                return None;
            }
            run_capture("sudo", &["sha256sum", &service_dest])
                .ok()
                .and_then(|o| o.split_whitespace().next().map(str::to_string))
        });
        if current.as_deref() != Some(sha256) {
            sudo(&argv(&[
                "install", "-m", "0644", "-o", SERVICE_USER, "-g", SERVICE_USER, &dest.to_string_lossy(), &service_dest,
            ]))?;
            println!("{} {}", "Installed:".green(), service_dest);
        }
    }
    Ok(())
}

fn check_kvm() {
    section("KVM Access");
    if !Path::new("/dev/kvm").exists() {
        println!("{}", "/dev/kvm not found — KVM may not be enabled.".red());
        println!("  - Verify your CPU supports Intel VT-x or AMD-V");
        println!("  - Enable virtualization in BIOS/UEFI");
        println!("  - Load the module: sudo modprobe kvm_intel  (or kvm_amd)");
        return;
    }
    println!("{} the services get /dev/kvm through the kvm group.", "KVM: OK;".green());
    let accessible = fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok();
    if !accessible {
        let user = invoking_user().unwrap_or_default();
        println!(
            "{} running VMs as yourself (cargo run, tests) needs: sudo usermod -aG kvm {}",
            "Note:".yellow(),
            user
        );
    }
}

// --- Build and install ---

fn workspace_root() -> PathBuf {
    // crates/glidex-install -> crates -> workspace root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn target_dir() -> PathBuf {
    env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root().join("target"))
}

fn build_project(cargo: &str) -> Result<()> {
    section("Building glidex");
    run_in(
        cargo,
        &["build", "--release", "-p", "glidex-control-plane", "-p", "glidex-netd", "-p", "glidex-ui"],
        &workspace_root(),
    )?;
    println!("{}", "Build successful".green());
    Ok(())
}

/// UI dependencies exactly as locked, then the production build.
fn build_ui(bun: &str) -> Result<()> {
    section("Building the web UI");
    let ui_dir = workspace_root().join("crates/glidex-ui/ui");
    run_in(bun, &["install", "--frozen-lockfile"], &ui_dir)?;
    run_in(bun, &["run", "build"], &ui_dir)?;
    println!("{}", "UI built".green());
    Ok(())
}

/// Binaries the install puts in `BIN_DIR`.
fn binaries(opts: &Options) -> Vec<&'static str> {
    let mut v = vec!["glidex-control-plane", "gxctl", "glidex-ui"];
    if opts.networking {
        v.push("glidex-netd");
    }
    v
}

fn install_binaries(opts: &Options, user_home: &Path, changed: &mut Changes) -> Result<()> {
    section("Installing binaries");
    let release = target_dir().join("release");
    for name in binaries(opts) {
        let dst = format!("{}/{}", BIN_DIR, name);
        if install_if_changed(&release.join(name), &dst, "0755")? {
            println!("{} {}", "Updated:".green(), dst);
            match name {
                "glidex-control-plane" => changed.control_plane = true,
                "glidex-ui" => changed.ui = true,
                "glidex-netd" => changed.netd = true,
                _ => {}
            }
        } else {
            println!("{} {}", "Up to date:".green(), dst);
        }
    }
    // Earlier installers put these in ~/.local/bin, where they'd shadow
    // the current ones.
    for name in ["glidex-control-plane", "gxctl"] {
        let stale = user_home.join(".local/bin").join(name);
        if stale.exists() {
            fs::remove_file(&stale).with_context(|| stale.display().to_string())?;
            println!("{} old copy {}", "Removed:".green(), stale.display());
        }
    }
    Ok(())
}

/// Swap the built UI into `UI_ASSET_DIR` when it changed.
fn install_ui_assets() -> Result<()> {
    let dist = workspace_root().join("crates/glidex-ui/ui/dist");
    if succeeds("diff", &["-rq", &dist.to_string_lossy(), UI_ASSET_DIR]) {
        println!("{} {}", "Up to date:".green(), UI_ASSET_DIR);
        return Ok(());
    }
    let staging = format!("{}.new", UI_ASSET_DIR);
    let parent = Path::new(UI_ASSET_DIR).parent().unwrap().to_string_lossy().into_owned();
    for cmd in [
        argv(&["rm", "-rf", &staging]),
        argv(&["mkdir", "-p", &parent]),
        argv(&["cp", "-r", &dist.to_string_lossy(), &staging]),
        argv(&["chown", "-R", "root:root", &staging]),
        argv(&["chmod", "-R", "u=rwX,go=rX", &staging]),
        argv(&["rm", "-rf", UI_ASSET_DIR]),
        argv(&["mv", &staging, UI_ASSET_DIR]),
    ] {
        sudo(&cmd)?;
    }
    println!("{} {}", "Updated:".green(), UI_ASSET_DIR);
    Ok(())
}

// --- Service user ---

fn group_exists(name: &str) -> bool {
    succeeds("getent", &["group", name])
}

fn user_in_group(user: &str, group: &str) -> bool {
    run_capture("id", &["-nG", user]).is_ok_and(|g| g.split_whitespace().any(|x| x == group))
}

fn nologin_shell() -> &'static str {
    ["/usr/sbin/nologin", "/sbin/nologin", "/usr/bin/nologin"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("/bin/false")
}

/// What the host already has, for `service_user_commands`.
struct UserState<'a> {
    group: bool,
    user: bool,
    /// The home directory exists and belongs to the service user.
    home_owned: bool,
    home_exists: bool,
    data_dir: bool,
    /// The invoking user (not root), and whether it is in the group.
    member: Option<(&'a str, bool)>,
}

/// Root commands that create the service user, its home and data dir, and
/// add the invoking user to its group (gxctl console sockets, netd).
fn service_user_commands(s: &UserState, shell: &str) -> Vec<Vec<String>> {
    let data_dir = format!("{}/.glidex", SERVICE_HOME);
    let mut cmds = Vec::new();
    if !s.group {
        cmds.push(argv(&["groupadd", "--system", NETD_GROUP]));
    }
    if !s.user {
        cmds.push(argv(&[
            "useradd", "--system", "--gid", NETD_GROUP, "--home-dir", SERVICE_HOME, "--no-create-home",
            "--shell", shell, "--comment", "glidex services", SERVICE_USER,
        ]));
    }
    if !s.home_exists {
        cmds.push(argv(&["install", "-d", "-m", "0750", "-o", SERVICE_USER, "-g", NETD_GROUP, SERVICE_HOME]));
    } else if !s.home_owned {
        // Kept from an earlier install whose user had another uid.
        cmds.push(argv(&["chown", "-R", &format!("{}:{}", SERVICE_USER, NETD_GROUP), SERVICE_HOME]));
    }
    if !s.data_dir {
        cmds.push(argv(&["install", "-d", "-m", "0750", "-o", SERVICE_USER, "-g", NETD_GROUP, &data_dir]));
    }
    if let Some((user, false)) = s.member {
        cmds.push(argv(&["usermod", "-aG", NETD_GROUP, user]));
    }
    cmds
}

fn ensure_service_user(invoking: Option<&str>) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    section(&format!("Service user '{}'", SERVICE_USER));
    let uid = run_capture("id", &["-u", SERVICE_USER]).ok().and_then(|u| u.trim().parse::<u32>().ok());
    let home = fs::metadata(SERVICE_HOME).ok();
    let data_dir = Path::new(SERVICE_HOME).join(".glidex");
    let state = UserState {
        group: group_exists(NETD_GROUP),
        user: uid.is_some(),
        home_exists: home.is_some(),
        home_owned: home.as_ref().is_some_and(|m| Some(m.uid()) == uid),
        // Unreadable before our group membership applies: ask root.
        data_dir: match fs::metadata(&data_dir) {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                ensure_sudo().is_ok() && succeeds("sudo", &["test", "-d", &data_dir.to_string_lossy()])
            }
            Err(_) => false,
        },
        member: invoking.map(|u| (u, user_in_group(u, NETD_GROUP))),
    };
    let cmds = service_user_commands(&state, nologin_shell());
    if cmds.is_empty() {
        println!("{} {} (home {})", "Up to date:".green(), SERVICE_USER, SERVICE_HOME);
        return Ok(());
    }
    for cmd in cmds {
        sudo(&cmd)?;
    }
    println!("{} {} (home {})", "Ready:".green(), SERVICE_USER, SERVICE_HOME);
    if let Some((user, false)) = state.member {
        println!(
            "{} added {} to the {} group (gxctl console, networking); log out and back in for it to apply.",
            "Note:".yellow(),
            user,
            NETD_GROUP
        );
    }
    Ok(())
}

// --- Networking ---

/// VM networking (spec §13): OVS, host settings, the glidex-netd service,
/// and CAP_NET_ADMIN for cloud-hypervisor's tap devices.
fn setup_networking(opts: &Options, changed: &Changes) -> Result<()> {
    use glidex_ovs::host::ProbeOptions;
    use glidex_ovs::install::{install, InstallRequest, Profile};
    use glidex_ovs::{OvsError, SystemExec};

    section("VM Networking (Open vSwitch + glidex-netd)");
    // OVS probes run as root too.
    ensure_sudo()?;

    // 1. Open vSwitch from distro packages.
    let profile = match opts.ovs_profile {
        OvsProfile::Kernel => Profile::Kernel,
        OvsProfile::Dpdk => Profile::Dpdk,
    };
    let exec = if is_root() { SystemExec::new() } else { SystemExec::sudo() };
    let ch_binary = PathBuf::from(format!("{}/cloud-hypervisor", BIN_DIR));
    let probe = ProbeOptions { ch_binary: Some(ch_binary.clone()) };
    let req = InstallRequest { profile, source_build: false, confirm: opts.allow_ovs_restart };
    let mut ovs_ok = true;
    match install(&exec, &probe, &req) {
        Ok(report) => {
            let what = if report.changed { "Installed:" } else { "Up to date:" };
            println!("{} Open vSwitch {}", what.green(), report.ovs_version.unwrap_or_default());
            for w in report.warnings {
                println!("{} {}", "Note:".yellow(), w);
            }
        }
        Err(OvsError::ConfirmationRequired { impact }) => {
            ovs_ok = false;
            println!("{} not changing Open vSwitch: {}", "Skipped:".yellow(), impact);
            println!("      Re-run with --allow-ovs-restart to go ahead.");
        }
        Err(e) => bail!("Open vSwitch installation failed: {}", e),
    }

    // 2. Host settings: forwarding for NAT; hugepages, vfio-pci and DPDK
    //    init for the dpdk profile.
    configure_host(profile, opts, ovs_ok, &exec, &probe)?;

    // 3. glidex-netd (root: it is the privileged helper the rest use).
    let netd_unit = workspace_root().join("packaging/glidex-netd.service");
    let unit_changed = install_if_changed(&netd_unit, NETD_UNIT, "0644")?;
    if unit_changed {
        sudo(&argv(&["systemctl", "daemon-reload"]))?;
    }
    enable_unit("glidex-netd.service")?;
    // Restarting netd is safe (RuntimeDirectoryPreserve keeps vhost-user
    // sockets); do it only when something changed.
    if changed.netd || unit_changed || !unit_active("glidex-netd.service") {
        sudo(&argv(&["systemctl", "restart", "glidex-netd.service"]))?;
        println!("{} glidex-netd (systemctl status glidex-netd)", "Started:".green());
    } else {
        println!("{} glidex-netd", "Running:".green());
    }

    // 4. Tap devices: cloud-hypervisor brings them up itself. Replacing the
    //    binary drops the capability, so check every run.
    let has_cap = run_capture("getcap", &[&ch_binary.to_string_lossy()]).is_ok_and(|o| o.contains("cap_net_admin"));
    if !has_cap {
        sudo(&argv(&["setcap", "cap_net_admin+ep", &ch_binary.to_string_lossy()]))?;
        println!("{} cap_net_admin on {}", "Granted:".green(), ch_binary.display());
    }
    Ok(())
}

fn read_sysctl(key: &str) -> Option<String> {
    fs::read_to_string(sysconfig::proc_path(key)).ok().map(|v| v.trim().to_string())
}

/// Persist and apply the kernel settings VM networking needs (see
/// sysconfig.rs), then initialize OVS-DPDK for the dpdk profile.
fn configure_host(
    profile: glidex_ovs::install::Profile,
    opts: &Options,
    ovs_ok: bool,
    exec: &glidex_ovs::SystemExec,
    probe: &glidex_ovs::host::ProbeOptions,
) -> Result<()> {
    use glidex_ovs::install::{init_dpdk, DpdkSettings, Profile};
    use glidex_ovs::OvsError;
    use sysconfig::{IP_FORWARD, NR_HUGEPAGES};

    // NAT networks route VM traffic through this host.
    let mut wanted: Vec<(&str, String)> = vec![(IP_FORWARD, "1".into())];

    let dpdk = profile == Profile::Dpdk;
    if dpdk {
        let mem = fs::read_to_string("/proc/meminfo").ok().and_then(|m| sysconfig::mem_total_kb(&m)).unwrap_or(0);
        let reserved = read_sysctl(NR_HUGEPAGES).and_then(|v| v.parse().ok()).unwrap_or(0);
        let pages = sysconfig::hugepages_for(mem, reserved);
        if pages < sysconfig::MIN_HUGEPAGES {
            println!(
                "{} only {} MiB of RAM; OVS-DPDK needs at least {} MiB of hugepages. Skipping DPDK setup (or use --ovs-profile kernel).",
                "Warning:".yellow(),
                mem / 1024,
                sysconfig::MIN_HUGEPAGES * 2
            );
            return write_sysctls(&wanted);
        }
        wanted.push((NR_HUGEPAGES, pages.to_string()));
    }
    write_sysctls(&wanted)?;
    if !dpdk {
        return Ok(());
    }

    // The kernel reserves what it can find contiguous memory for.
    let got: u64 = read_sysctl(NR_HUGEPAGES).and_then(|v| v.parse().ok()).unwrap_or(0);
    println!("{} {} hugepages ({} MiB)", "Reserved:".green(), got, got * 2);
    if got < sysconfig::MIN_HUGEPAGES {
        println!(
            "{} only {} hugepages could be reserved (memory is fragmented). Reboot to apply {}, then re-run the installer.",
            "Warning:".yellow(),
            got,
            sysconfig::SYSCTL_DROPIN
        );
        return Ok(());
    }

    // vfio-pci binds physical NICs for DPDK uplinks.
    let modules = "# Managed by glidex-install\nvfio-pci\n";
    write_if_changed(sysconfig::MODULES_DROPIN, modules)?;
    if !Path::new("/sys/bus/pci/drivers/vfio-pci").exists() {
        if let Err(e) = sudo(&argv(&["modprobe", "vfio-pci"])) {
            println!("{} could not load vfio-pci: {}", "Note:".yellow(), e);
        }
    }
    let iommu = fs::read_dir("/sys/kernel/iommu_groups").map(|mut d| d.next().is_some()).unwrap_or(false);
    if !iommu {
        println!(
            "{} no IOMMU groups: DPDK NIC uplinks need intel_iommu=on / amd_iommu=on (and VT-d/AMD-Vi in firmware).",
            "Note:".yellow()
        );
        println!("      vhost-user and AF_XDP don't need it.");
    }
    if !ovs_ok {
        return Ok(());
    }

    let socket_mem = sysconfig::socket_mem_mb(got);
    let settings = DpdkSettings {
        socket_mem: socket_mem.to_string(),
        pmd_cpu_mask: opts.pmd_cpu_mask.clone(),
        confirm: opts.allow_ovs_restart,
    };
    match init_dpdk(exec, probe, &settings) {
        Ok(()) => println!("{} OVS-DPDK ({} MiB socket memory)", "Ready:".green(), socket_mem),
        Err(OvsError::ConfirmationRequired { impact }) => {
            println!("{} DPDK init: {}", "Skipped:".yellow(), impact);
            println!("      Re-run with --allow-ovs-restart (or run `gxctl ovs dpdk-init`).");
        }
        Err(e) => bail!("OVS-DPDK init failed: {}", e),
    }
    Ok(())
}

/// Merge `wanted` into the sysctl drop-in (keeping previously recorded
/// original values) and apply it, unless it is already in place.
fn write_sysctls(wanted: &[(&str, String)]) -> Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let existing = fs::read_to_string(sysconfig::SYSCTL_DROPIN).ok();
    let settings = sysconfig::merge(existing.as_deref(), wanted, read_sysctl);
    let rendered = sysconfig::render(&settings);
    let live = wanted.iter().all(|(k, v)| read_sysctl(k).as_deref() == Some(v.as_str()));
    if existing.as_deref() == Some(rendered.as_str()) && live {
        return Ok(());
    }
    sudo_write(sysconfig::SYSCTL_DROPIN, &rendered)?;
    sudo(&argv(&["sysctl", "-q", "-p", sysconfig::SYSCTL_DROPIN]))?;
    for (key, value) in wanted {
        println!("{} {} = {}", "Set:".green(), key, value);
    }
    Ok(())
}

// --- systemd ---

fn unit_active(unit: &str) -> bool {
    succeeds("systemctl", &["is-active", "--quiet", unit])
}

fn enable_unit(unit: &str) -> Result<()> {
    if !succeeds("systemctl", &["is-enabled", "--quiet", unit]) {
        sudo(&argv(&["systemctl", "enable", unit]))?;
    }
    Ok(())
}

/// Fill in the control-plane unit template (packaging/*.service.in).
fn render_control_plane_unit(template: &str, user: &str, home: &Path, bin: &Path, groups: &[&str]) -> String {
    template
        .replace("@USER@", user)
        .replace("@HOME@", &home.to_string_lossy())
        .replace("@BIN@", &bin.to_string_lossy())
        .replace("@GROUPS@", &groups.join(" "))
}

/// Supplementary groups for the control plane: only ones that exist, since
/// systemd refuses to start a unit naming a missing group.
fn control_plane_groups(exists: impl Fn(&str) -> bool) -> Vec<&'static str> {
    ["kvm"].into_iter().filter(|g| exists(g)).collect()
}

/// Something answers on `addr` (e.g. a control plane started by hand).
fn port_in_use(addr: &str) -> bool {
    addr.parse()
        .ok()
        .is_some_and(|a| std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_millis(500)).is_ok())
}

/// Running or paused VMs per the local control plane; `None` if it can't
/// be asked.
fn active_vms() -> Option<usize> {
    let body = run_capture("curl", &["-fsS", "--max-time", "5", &format!("http://{}/vms", API_ADDR)]).ok()?;
    let vms: Vec<serde_json::Value> = serde_json::from_str(&body).ok()?;
    Some(vms.iter().filter(|v| matches!(v["state"].as_str(), Some("running" | "paused"))).count())
}

/// The `User=` of an existing unit file.
fn unit_user(unit_text: &str) -> Option<&str> {
    unit_text.lines().find_map(|l| l.strip_prefix("User=")).map(str::trim)
}

/// systemd units so glidex comes up at boot: Open vSwitch → glidex-netd
/// (reconciles bridges, DPDK binding, IP migrations, NAT) → control plane
/// → web UI, the last two as the service user.
fn install_services(changed: &Changes, invoking: Option<&str>, user_home: &Path) -> Result<()> {
    section("Services (systemd)");
    if !Path::new("/run/systemd/system").exists() {
        println!("{}", "systemd is not running here; skipping".yellow());
        return Ok(());
    }
    let root = workspace_root();
    let previous_user = fs::read_to_string(CONTROL_PLANE_UNIT).ok().and_then(|t| unit_user(&t).map(str::to_string));

    let template = fs::read_to_string(root.join("packaging/glidex-control-plane.service.in"))
        .context("packaging/glidex-control-plane.service.in")?;
    let unit = render_control_plane_unit(
        &template,
        SERVICE_USER,
        Path::new(SERVICE_HOME),
        &Path::new(BIN_DIR).join("glidex-control-plane"),
        &control_plane_groups(group_exists),
    );
    let cp_unit_changed = write_if_changed(CONTROL_PLANE_UNIT, &unit)?;
    let ui_unit_changed = install_if_changed(&root.join("packaging/glidex-ui.service"), UI_UNIT, "0644")?;
    if cp_unit_changed || ui_unit_changed {
        sudo(&argv(&["systemctl", "daemon-reload"]))?;
    }
    enable_unit("glidex-control-plane.service")?;
    enable_unit("glidex-ui.service")?;

    // Control plane: stopping it stops every VM, so never restart it under
    // running VMs.
    let cp = "glidex-control-plane.service";
    let cp_stale = changed.control_plane || cp_unit_changed;
    if !unit_active(cp) {
        if port_in_use(API_ADDR) {
            println!(
                "{} something else listens on {} (a control plane started by hand?). Stop it, then: sudo systemctl start {}",
                "Not started:".yellow(),
                API_ADDR,
                cp
            );
        } else {
            sudo(&argv(&["systemctl", "start", cp]))?;
            println!("{} {} (as {})", "Started:".green(), cp, SERVICE_USER);
        }
    } else if cp_stale {
        match active_vms() {
            Some(0) => {
                sudo(&argv(&["systemctl", "restart", cp]))?;
                println!("{} {} (as {})", "Restarted:".green(), cp, SERVICE_USER);
            }
            n => println!(
                "{} {} has {} running VM(s); restart it when convenient to apply the update (it stops them): sudo systemctl restart {}",
                "Not restarted:".yellow(),
                cp,
                n.map_or("possibly".to_string(), |n| n.to_string()),
                cp
            ),
        }
    } else {
        println!("{} {}", "Running:".green(), cp);
    }

    // Web UI: stateless, restart freely.
    let ui = "glidex-ui.service";
    if !unit_active(ui) && port_in_use(UI_ADDR) {
        println!(
            "{} something else listens on {} (a dev server?). Stop it, then: sudo systemctl start {}",
            "Not started:".yellow(),
            UI_ADDR,
            ui
        );
    } else if !unit_active(ui) || changed.ui || ui_unit_changed {
        sudo(&argv(&["systemctl", "restart", ui]))?;
        println!("{} {} on http://localhost:5173", "Started:".green(), ui);
    } else {
        println!("{} {}", "Running:".green(), ui);
    }

    // The control plane used to run as the installing user, on its ~/.glidex.
    if let (Some(prev), Some(me)) = (previous_user.as_deref(), invoking) {
        if prev == me && user_home.join(".glidex/glidex.db").exists() {
            println!(
                "{} the control plane now runs as '{}' with its data in {}/.glidex. Your earlier VMs, images and credentials in {} were not moved.",
                "Note:".yellow(),
                SERVICE_USER,
                SERVICE_HOME,
                user_home.join(".glidex").display()
            );
        }
    }
    Ok(())
}

fn print_usage(opts: &Options) {
    section("Quick Start");
    println!();
    if opts.services {
        println!("glidex runs as systemd services (user '{}'):", SERVICE_USER);
        println!("     web UI:  {}", "http://localhost:5173".green());
        println!("     API:     {}", "http://localhost:8841".green());
        println!("     status:  {}", "systemctl status glidex-control-plane glidex-ui".green());
    } else {
        println!("1. Start the control plane server:");
        println!("     {}", "glidex-control-plane".green());
        println!();
        println!("2. (Optional) Start the web UI in another terminal:");
        println!("     {}", "glidex-ui".green());
        println!("     Then open http://localhost:5173");
    }
    println!();
    println!("Use the interactive CLI:");
    println!("     {}", "gxctl".green());
    println!();
    println!("Re-run the installer any time to update glidex and its dependencies.");
    println!("{}", "Installation complete!".green().bold());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_defaults_saved_choices_and_flags() {
        let o = Options::parse(None, &[]).unwrap();
        assert_eq!(o, Options::default());
        assert!(o.qemu && o.networking && o.services);

        let o = Options::parse(None, &args(&["--no-qemu", "--ovs-profile", "kernel", "--pmd-cpu-mask", "0x6"])).unwrap();
        let saved = o.render();
        // A re-run without flags keeps them; flags still override.
        let again = Options::parse(Some(&saved), &[]).unwrap();
        assert!(!again.qemu);
        assert_eq!(again.ovs_profile, OvsProfile::Kernel);
        assert_eq!(again.pmd_cpu_mask.as_deref(), Some("0x6"));
        assert_eq!(again.render(), saved);
        let flipped = Options::parse(Some(&saved), &args(&["--qemu", "--pmd-cpu-mask", "auto"])).unwrap();
        assert!(flipped.qemu);
        assert_eq!(flipped.pmd_cpu_mask, None);

        let restart = Options::parse(None, &args(&["--allow-ovs-restart"])).unwrap();
        assert!(restart.allow_ovs_restart);
        assert_eq!(restart.render(), Options::default().render(), "never saved");

        assert!(Options::parse(None, &args(&["--bogus"])).is_err());
        assert!(Options::parse(None, &args(&["--ovs-profile", "fast"])).is_err());
        assert!(Options::parse(None, &args(&["--ovs-profile"])).is_err());
    }

    fn pkgs(manager: &str, deps: &[Dep], absent: &[&str], not_packaged: &[&str]) -> (Vec<&'static str>, Vec<&'static str>) {
        let col = manager_column(manager);
        let present = |p: &Probe| {
            let name = match p {
                Probe::Cmd(c) => *c,
                Probe::PkgConfig(m) => m,
                Probe::AnyFile(_) => "ovmf",
            };
            !absent.contains(&name)
        };
        package_plan(manager, deps, present, |pkg| {
            !deps.iter().any(|d| d.packages[col] == pkg && not_packaged.iter().any(|n| matches!(d.probe, Probe::Cmd(c) if c == *n)))
        })
    }

    #[test]
    fn package_plan_installs_missing_and_updates_the_rest() {
        let (missing, update) = pkgs("apt-get", BASE_DEPS, &["clang", "sgdisk", "openssl"], &[]);
        assert_eq!(missing, ["libssl-dev", "clang", "gdisk"]);
        assert_eq!(
            update,
            ["curl", "unzip", "pkg-config", "dosfstools", "mtools", "qemu-utils", "cloud-guest-utils"],
            "deduplicated (qemu-img and qemu-io share qemu-utils)"
        );
        // A tool installed some other way (no package) isn't touched.
        let (missing, update) = pkgs("dnf", BASE_DEPS, &[], &["clang"]);
        assert!(missing.is_empty());
        assert!(!update.contains(&"clang"));
        assert!(update.contains(&"cloud-utils-growpart"));
        // One missing tool of a shared package installs it, not updates it.
        let (missing, update) = pkgs("pacman", BASE_DEPS, &["qemu-io"], &[]);
        assert_eq!(missing, ["qemu-img"]);
        assert!(!update.contains(&"qemu-img"));
        assert!(update.contains(&"gptfdisk"));
    }

    #[test]
    fn qemu_and_networking_deps_per_manager() {
        let x86 = Platform { os: "linux", arch: "x86_64" };
        let arm = Platform { os: "linux", arch: "aarch64" };
        let names = |m: &str, o: &Options, p: &Platform| {
            let deps = wanted_deps(o, p);
            pkgs(m, &deps, &["qemu-system-x86_64", "ovmf", "setcap"], &[]).0
        };
        let all = Options::default();
        assert_eq!(names("apt-get", &all, &x86), ["qemu-system-x86", "ovmf", "libcap2-bin"]);
        assert_eq!(names("dnf", &all, &x86), ["qemu-kvm", "edk2-ovmf", "libcap"]);
        assert_eq!(names("pacman", &all, &x86), ["qemu-base", "edk2-ovmf", "libcap"]);
        assert_eq!(names("apt-get", &all, &arm), ["libcap2-bin"], "QEMU support is x86_64 only");
        let minimal = Options { qemu: false, networking: false, ..Options::default() };
        assert!(names("apt-get", &minimal, &x86).is_empty());
    }

    #[test]
    fn package_commands_per_manager() {
        let lines = |m: &str, missing: &[&str], update: &[&str]| -> Vec<String> {
            package_commands(m, missing, update).iter().map(|c| c.join(" ")).collect()
        };
        assert!(lines("apt-get", &[], &[]).is_empty());
        assert_eq!(
            lines("apt-get", &["clang"], &["curl"]),
            ["apt-get update", "env DEBIAN_FRONTEND=noninteractive apt-get install -y clang curl"]
        );
        assert_eq!(lines("dnf", &["clang"], &["curl"]), ["dnf install -y clang", "dnf upgrade -y curl"]);
        assert_eq!(lines("yum", &[], &["curl"]), ["yum upgrade -y curl"]);
        assert_eq!(lines("pacman", &["clang"], &["curl"]), ["pacman -S --needed --noconfirm clang curl"]);
    }

    #[test]
    fn cloud_hypervisor_version_parsing() {
        assert_eq!(cloud_hypervisor_version("cloud-hypervisor v53.0\n"), Some("v53.0"));
        assert_eq!(cloud_hypervisor_version("cloud-hypervisor v53.0-dirty"), Some("v53.0"));
        assert_eq!(cloud_hypervisor_version(""), None);
    }

    #[test]
    fn service_user_created_once() {
        let fresh = UserState { group: false, user: false, home_exists: false, home_owned: false, data_dir: false, member: Some(("alice", false)) };
        let lines: Vec<String> = service_user_commands(&fresh, "/usr/sbin/nologin").iter().map(|c| c.join(" ")).collect();
        assert_eq!(
            lines,
            [
                "groupadd --system glidex",
                "useradd --system --gid glidex --home-dir /var/lib/glidex-control-plane --no-create-home --shell /usr/sbin/nologin --comment glidex services glidex",
                "install -d -m 0750 -o glidex -g glidex /var/lib/glidex-control-plane",
                "install -d -m 0750 -o glidex -g glidex /var/lib/glidex-control-plane/.glidex",
                "usermod -aG glidex alice",
            ]
        );
        let done = UserState { group: true, user: true, home_exists: true, home_owned: true, data_dir: true, member: Some(("alice", true)) };
        assert!(service_user_commands(&done, "/usr/sbin/nologin").is_empty(), "a re-run needs no root");
        // A home kept from an earlier install (user recreated with a new uid).
        let kept = UserState { home_owned: false, user: false, member: None, ..done };
        let lines: Vec<String> = service_user_commands(&kept, "/bin/false").iter().map(|c| c.join(" ")).collect();
        assert!(lines[0].starts_with("useradd"));
        assert_eq!(lines[1], "chown -R glidex:glidex /var/lib/glidex-control-plane");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn control_plane_unit_runs_as_the_service_user() {
        let template = std::fs::read_to_string(workspace_root().join("packaging/glidex-control-plane.service.in")).unwrap();
        let groups = control_plane_groups(|g| g == "kvm");
        assert_eq!(groups, vec!["kvm"]);
        assert!(control_plane_groups(|_| false).is_empty(), "missing kvm group is left out");
        let unit = render_control_plane_unit(&template, SERVICE_USER, Path::new(SERVICE_HOME), Path::new("/usr/local/bin/glidex-control-plane"), &groups);
        for line in [
            "User=glidex",
            "Group=glidex",
            "SupplementaryGroups=kvm",
            "Environment=HOME=/var/lib/glidex-control-plane",
            "WorkingDirectory=/var/lib/glidex-control-plane",
            "ExecStart=/usr/local/bin/glidex-control-plane",
            "After=network-online.target glidex-netd.service",
            "Wants=glidex-netd.service",
            "UMask=0007",
        ] {
            assert!(unit.lines().any(|l| l == line), "missing {line:?}");
        }
        assert!(!unit.contains('@'), "all placeholders filled");
        assert_eq!(unit_user(&unit), Some("glidex"));
    }

    #[test]
    fn ui_unit_runs_as_the_service_user_after_the_control_plane() {
        let unit = std::fs::read_to_string(workspace_root().join("packaging/glidex-ui.service")).unwrap();
        for line in [
            "User=glidex",
            "Group=glidex",
            "ExecStart=/usr/local/bin/glidex-ui",
            "Environment=GLIDEX_UI_DIR=/usr/local/share/glidex/ui",
            "After=network-online.target glidex-control-plane.service",
            "Wants=glidex-control-plane.service",
        ] {
            assert!(unit.lines().any(|l| l == line), "missing {line:?}");
        }
    }

    #[test]
    fn netd_unit_is_notify_and_ordered_after_ovs() {
        let unit = std::fs::read_to_string(workspace_root().join("packaging/glidex-netd.service")).unwrap();
        assert!(unit.contains("Type=notify"));
        assert!(unit.lines().any(|l| l.starts_with("After=") && l.contains("openvswitch-switch.service") && l.contains("glidex-ovs-vswitchd.service")));
        assert!(unit.contains("Before=glidex-control-plane.service"));
    }

    #[test]
    fn unit_file_preserves_runtime_dir() {
        let unit = std::fs::read_to_string(workspace_root().join("packaging/glidex-netd.service")).unwrap();
        assert!(unit.contains("ExecStart=/usr/local/bin/glidex-netd"));
        assert!(unit.contains("RuntimeDirectory=glidex"));
        assert!(unit.contains("RuntimeDirectoryPreserve=yes"), "vhost-user sockets must survive netd restarts");
    }

    #[test]
    fn binaries_follow_options() {
        assert_eq!(binaries(&Options::default()), ["glidex-control-plane", "gxctl", "glidex-ui", "glidex-netd"]);
        let no_net = Options { networking: false, ..Options::default() };
        assert!(!binaries(&no_net).contains(&"glidex-netd"));
    }

    #[test]
    fn firmware_asset_matches_arch() {
        let x86 = Platform { os: "linux", arch: "x86_64" };
        let arm = Platform { os: "linux", arch: "aarch64" };
        assert_eq!(firmware_asset(&x86).0, "CLOUDHV.fd");
        assert_eq!(firmware_asset(&arm).0, "CLOUDHV_EFI.fd");
        for p in [&x86, &arm] {
            let sha = firmware_asset(p).1;
            assert_eq!(sha.len(), 64);
            assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    /// Downloads the pinned firmware from GitHub into a throwaway HOME.
    #[test]
    #[ignore = "downloads from github.com"]
    fn downloads_and_verifies_pinned_firmware() {
        let home = TempDir::new().unwrap();
        let platform = detect_platform().unwrap();
        let (asset, sha256) = firmware_asset(&platform);

        install_uefi_firmware(&platform, home.path(), false).unwrap();
        let dest = home.path().join(".glidex").join(asset);
        assert_eq!(file_sha256(&dest).unwrap(), sha256);
        assert!(!home.path().join(".glidex").join(format!("{asset}.part")).exists());

        // Second run sees the verified file and does not re-download.
        let mtime = fs::metadata(&dest).unwrap().modified().unwrap();
        install_uefi_firmware(&platform, home.path(), false).unwrap();
        assert_eq!(fs::metadata(&dest).unwrap().modified().unwrap(), mtime);
    }

    #[test]
    fn file_sha256_hashes_contents() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, b"abc").unwrap();
        assert_eq!(
            file_sha256(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
