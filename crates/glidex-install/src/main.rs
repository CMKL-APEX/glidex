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
/// rustup-init from static.rust-lang.org/rustup/archive/<version>/<target>/,
/// verified against `rustup_asset` (the `rustup-init.sha256` published next
/// to it) before it runs. Only used when Rust is missing; after that
/// `rustup update` keeps it current.
const RUSTUP_VERSION: &str = "1.29.1";
/// Bun release (github.com/oven-sh/bun/releases, tag `bun-v<version>`),
/// verified against `bun_asset` (the release's SHASUMS256.txt). Bun builds
/// the web UI, so it is pinned like the rest of the build.
const BUN_VERSION: &str = "1.4.2";

/// The system user the control plane runs as. Its primary group is the
/// one allowed to use glidex-netd and glidex-authd: nobody else is in it
/// (spec/security.md §4).
const SERVICE_USER: &str = "glidex";
const NETD_GROUP: &str = "glidex";
/// The web UI's own user (and group); not in `glidex`.
const UI_USER: &str = "glidex-ui";
/// Humans allowed to use gxctl on the control plane's api.sock.
const USERS_GROUP: &str = "glidex-users";
/// Break-glass administrators (api.sock as system-admin, netd directly).
const ADMIN_GROUP: &str = "glidex-admin";
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
const AUTHD_SOCKET_UNIT: &str = "/etc/systemd/system/glidex-authd.socket";
const AUTHD_SERVICE_UNIT: &str = "/etc/systemd/system/glidex-authd.service";
/// One instance per running VM, started by the control plane over D-Bus
/// (spec/reconciliation.md §13).
pub(crate) const VM_UNIT: &str = "/etc/systemd/system/glidex-vm@.service";
pub(crate) const VMS_SLICE: &str = "/etc/systemd/system/glidex-vms.slice";
/// Lets the service user start/stop/kill exactly its glidex-vm@ units.
pub(crate) const POLKIT_RULE: &str = "/etc/polkit-1/rules.d/50-glidex-vm.rules";
/// The hypervisor binaries glidex-vm-shim may run (root-owned, 0644).
pub(crate) const SHIM_ALLOWLIST: &str = "/etc/glidex/vm-shim.json";
/// A line only control-plane units from before detached VMs carry: such a
/// control plane still stops every VM when it stops.
const LEGACY_CP_MARKER: &str = "Stopping the service stops running VMs";
/// PAM service glidex-authd authenticates with. Rewritten only while it
/// carries `PAM_MARKER`, so an administrator's edits are kept.
pub(crate) const PAM_FILE: &str = "/etc/pam.d/glidex";
pub(crate) const PAM_MARKER: &str = "Managed by glidex-install";
/// glidex-authd's settings, written once (left alone afterwards).
pub(crate) const AUTHD_CONFIG: &str = "/etc/glidex/authd.json";
const AUTHD_CONFIG_DEFAULT: &str = "{\"service_user\":\"glidex\",\"allowed_groups\":[\"glidex-users\"]}\n";
/// The control plane's settings with their defaults, for reference; the
/// installer never writes control-plane.json itself (defaults apply).
pub(crate) const CP_CONFIG_EXAMPLE: &str = "/etc/glidex/control-plane.json.example";
/// Read-only site policy files (spec/security.md §7.6), root:glidex 0750.
pub(crate) const POLICY_DIR: &str = "/etc/glidex/policies";
/// The control plane's local API socket (RuntimeDirectory=glidex-cp).
const API_SOCKET: &str = "/run/glidex-cp/api.sock";
/// Loopback ends of the default listeners (every address, spec/security.md
/// §5.1), to see whether something already holds the ports.
const API_ADDR: &str = "127.0.0.1:8841";
const UI_ADDR: &str = "127.0.0.1:5173";
/// The control plane's published certificate (self-signed by default).
const CP_CERT: &str = "/run/glidex-cp/tls.crt";
/// The UI's self-signed certificate (StateDirectory=glidex-ui).
const UI_CERT: &str = "/var/lib/glidex-ui/tls/ui.crt";

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
    let cargo = install_rust(&platform)?;
    let bun = install_bun(&platform)?;
    check_kvm();
    build_project(&cargo, &opts)?;
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
        install_service_config()?;
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
         \x20     --no-services          don't install the systemd units, users and groups\n\
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
    println!("  - Rust (rustup {} if missing), Bun {} (checksums verified)", RUSTUP_VERSION, BUN_VERSION);
    println!("  - Cloud-Hypervisor {} and its UEFI firmware ({})", CLOUD_HYPERVISOR_VERSION, EDK2_FIRMWARE_VERSION);
    println!("  - glidex binaries in {}, web UI in {}", BIN_DIR, UI_ASSET_DIR);
    println!("  - QEMU + OVMF: {}", on(opts.qemu));
    println!(
        "  - VM networking (Open vSwitch {}, glidex-netd): {}",
        opts.ovs_profile.name(),
        on(opts.networking)
    );
    println!(
        "  - systemd units (control plane as '{}', web UI as '{}', glidex-authd), groups {} and {}: {}",
        SERVICE_USER,
        UI_USER,
        USERS_GROUP,
        ADMIN_GROUP,
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

/// Download `url` to `dest` and check its sha256; on a mismatch the file
/// is deleted and the install stops (never run or install an unverified
/// download).
fn download_verified(url: &str, dest: &Path, sha256: &str) -> Result<()> {
    println!("Downloading {}", url);
    download(url, dest)?;
    let actual = file_sha256(dest)?;
    if actual != sha256 {
        let _ = fs::remove_file(dest);
        bail!(
            "Checksum mismatch for {}: expected {}, got {}. The download was deleted; \
             check your network (proxy, captive portal) and re-run, or report it if it persists.",
            url,
            sha256,
            actual
        );
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
    // Passwordless sudo needs no prompt. `sudo -v` would still ask when
    // sudoers also has a rule that needs a password (verifypw=all), which
    // fails without a terminal.
    if Command::new("sudo").args(["-n", "true"]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success()) {
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

/// rustup-init's target triple and its sha256 for `RUSTUP_VERSION`.
fn rustup_asset(platform: &Platform) -> (&'static str, &'static str) {
    if platform.arch == "x86_64" {
        ("x86_64-unknown-linux-gnu", "dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71")
    } else {
        ("aarch64-unknown-linux-gnu", "15f6e4ce9f583b929c996c91562bad6d4454f3281de858b02cdfdef615fac433")
    }
}

fn rustup_url(platform: &Platform) -> String {
    format!(
        "https://static.rust-lang.org/rustup/archive/{}/{}/rustup-init",
        RUSTUP_VERSION,
        rustup_asset(platform).0
    )
}

/// Bun's release zip name (without `.zip`) and its sha256 for `BUN_VERSION`.
fn bun_asset(platform: &Platform) -> (&'static str, &'static str) {
    if platform.arch == "x86_64" {
        ("bun-linux-x64", "36368faef7527875d5ffa52e53cd48021741f2a83eb6208a8dd64068d422a913")
    } else {
        ("bun-linux-aarch64", "54328bbc2d9c8e0c9f892c544d66c57a83b84139e34909e5ee81758f1ac8fda7")
    }
}

fn bun_url(platform: &Platform) -> String {
    format!(
        "https://github.com/oven-sh/bun/releases/download/bun-v{}/{}.zip",
        BUN_VERSION,
        bun_asset(platform).0
    )
}

/// `1.4.2` → (1, 4, 2); anything after the numbers (`-canary…`) is ignored.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next().flatten().unwrap_or(0)))
}

/// Rust via rustup, kept on the latest stable. Returns the cargo to build with.
fn install_rust(platform: &Platform) -> Result<String> {
    section("Rust");
    let home_cargo = dirs::home_dir().map(|h| h.join(".cargo/bin")).unwrap_or_default();
    if !command_exists("rustc") && !home_cargo.join("rustc").exists() {
        println!("{} {}", "Rust not found; installing rustup".yellow(), RUSTUP_VERSION);
        // A pinned, checksummed rustup-init instead of `curl … | sh`.
        let tmp = TempDir::new()?;
        let init = tmp.path().join("rustup-init");
        download_verified(&rustup_url(platform), &init, rustup_asset(platform).1)?;
        run("chmod", &["0755", &init.to_string_lossy()])?;
        run(&init.to_string_lossy(), &["-y", "--default-toolchain", "stable"])?;
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

/// What to do about Bun, given the version in ~/.bun (if any) and whether
/// another bun is on PATH.
#[derive(Debug, PartialEq, Eq)]
enum BunAction {
    /// Install (or upgrade to) the pinned release in ~/.bun.
    Install,
    /// ~/.bun has the pinned version or a newer one.
    Keep,
    /// A bun from elsewhere (distro, npm, …): left to whoever installed it.
    Foreign,
}

fn bun_action(home_version: Option<&str>, on_path: bool) -> BunAction {
    match home_version {
        Some(v) => match (parse_version(v), parse_version(BUN_VERSION)) {
            (Some(have), Some(want)) if have >= want => BunAction::Keep,
            _ => BunAction::Install,
        },
        None if on_path => BunAction::Foreign,
        None => BunAction::Install,
    }
}

/// Put the pinned, verified Bun release in `~/.bun/bin` (where bun.sh's
/// installer puts it, so earlier installs are upgraded in place).
fn install_bun_release(platform: &Platform, home_bun: &Path) -> Result<()> {
    let (asset, sha256) = bun_asset(platform);
    let tmp = TempDir::new()?;
    let zip = tmp.path().join(format!("{}.zip", asset));
    download_verified(&bun_url(platform), &zip, sha256)?;
    run("unzip", &["-q", "-o", &zip.to_string_lossy(), "-d", &tmp.path().to_string_lossy()])?;
    let bin_dir = home_bun.parent().context("bun path has no parent")?;
    fs::create_dir_all(bin_dir)?;
    // Replace by rename, so a running bun keeps its file.
    let staged = bin_dir.join(".bun.new");
    fs::copy(tmp.path().join(asset).join("bun"), &staged)?;
    run("chmod", &["0755", &staged.to_string_lossy()])?;
    fs::rename(&staged, home_bun)?;
    let bunx = bin_dir.join("bunx");
    if fs::symlink_metadata(&bunx).is_err() {
        std::os::unix::fs::symlink("bun", &bunx)?;
    }
    Ok(())
}

/// Bun (builds the UI): the pinned release (or newer) in ~/.bun. Returns
/// the bun to run.
fn install_bun(platform: &Platform) -> Result<String> {
    section("Bun");
    let home_bun = dirs::home_dir().map(|h| h.join(".bun/bin/bun")).unwrap_or_default();
    let home_version = home_bun
        .exists()
        .then(|| run_capture(&home_bun.to_string_lossy(), &["--version"]).unwrap_or_default());
    match bun_action(home_version.as_deref(), command_exists("bun")) {
        BunAction::Install => {
            println!("{} {}", "Installing Bun".yellow(), BUN_VERSION);
            install_bun_release(platform, &home_bun)?;
        }
        BunAction::Keep => {}
        BunAction::Foreign => {
            println!("{} Bun was not installed in ~/.bun; update it the way you installed it.", "Note:".yellow())
        }
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
    authd: bool,
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
        download_verified(&url, &partial, sha256)?;
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

/// `cargo build` arguments: the packages whose binaries `binaries` installs.
fn build_args(opts: &Options) -> Vec<&'static str> {
    let mut args = vec![
        "build", "--release", "-p", "glidex-control-plane", "-p", "glidex-netd", "-p", "glidex-ui", "-p", "glidex-vm-shim",
    ];
    if opts.services {
        args.extend(["-p", "glidex-authd"]);
    }
    args
}

fn build_project(cargo: &str, opts: &Options) -> Result<()> {
    section("Building glidex");
    run_in(cargo, &build_args(opts), &workspace_root())?;
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
    let mut v = vec!["glidex-control-plane", "gxctl", "glidex-ui", "glidex-vm-shim"];
    if opts.networking {
        v.push("glidex-netd");
    }
    // The root PAM helper only serves the control plane's unit.
    if opts.services {
        v.push("glidex-authd");
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
                "glidex-authd" => changed.authd = true,
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

/// Whether `path` is readable and traversable by everyone (glidex-ui is
/// neither the owner nor in the group).
fn world_readable_dir(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|m| m.is_dir() && m.permissions().mode() & 0o005 == 0o005)
}

/// Swap the built UI into `UI_ASSET_DIR` when it changed. The files are
/// root-owned and world-readable, so the `glidex-ui` user can serve them.
fn install_ui_assets() -> Result<()> {
    let dist = workspace_root().join("crates/glidex-ui/ui/dist");
    let parent = Path::new(UI_ASSET_DIR).parent().unwrap().to_string_lossy().into_owned();
    let readable = world_readable_dir(Path::new(&parent)) && world_readable_dir(Path::new(UI_ASSET_DIR));
    if readable && succeeds("diff", &["-rq", &dist.to_string_lossy(), UI_ASSET_DIR]) {
        println!("{} {}", "Up to date:".green(), UI_ASSET_DIR);
        return Ok(());
    }
    let staging = format!("{}.new", UI_ASSET_DIR);
    for cmd in [
        argv(&["rm", "-rf", &staging]),
        argv(&["install", "-d", "-m", "0755", "-o", "root", "-g", "root", &parent]),
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

/// The invoking user's current memberships, for `service_user_commands`.
#[derive(Debug, Clone, Copy)]
struct Member<'a> {
    name: &'a str,
    /// In `glidex` (an earlier installer added humans there).
    in_netd_group: bool,
    in_users: bool,
    in_admin: bool,
}

/// What the host already has, for `service_user_commands`.
#[derive(Debug, Clone, Copy)]
struct UserState<'a> {
    group: bool,
    user: bool,
    /// The home directory exists and belongs to the service user.
    home_owned: bool,
    home_exists: bool,
    data_dir: bool,
    users_group: bool,
    admin_group: bool,
    ui_group: bool,
    ui_user: bool,
    /// The invoking user (not root).
    member: Option<Member<'a>>,
}

/// Root commands that create the identities of spec/security.md §4: the
/// `glidex` service user (only the control plane), its home and data dir,
/// the `glidex-users` and `glidex-admin` groups with the invoking user in
/// both, and the `glidex-ui` user (own group, no login, no home). The
/// invoking user is taken out of `glidex`, where earlier installs put it.
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
    if !s.users_group {
        cmds.push(argv(&["groupadd", "--system", USERS_GROUP]));
    }
    if !s.admin_group {
        cmds.push(argv(&["groupadd", "--system", ADMIN_GROUP]));
    }
    if !s.ui_user {
        // Its own group: --user-group, or the group kept from an earlier one.
        let group: &[&str] = if s.ui_group { &["--gid", UI_USER] } else { &["--user-group"] };
        let mut cmd = argv(&["useradd", "--system"]);
        cmd.extend(argv(group));
        cmd.extend(argv(&[
            "--home-dir", "/nonexistent", "--no-create-home", "--shell", shell, "--comment", "glidex web UI", UI_USER,
        ]));
        cmds.push(cmd);
    }
    if let Some(m) = s.member {
        let missing: Vec<&str> = [(m.in_users, USERS_GROUP), (m.in_admin, ADMIN_GROUP)]
            .into_iter()
            .filter(|(member, _)| !member)
            .map(|(_, g)| g)
            .collect();
        if !missing.is_empty() {
            cmds.push(argv(&["usermod", "-aG", &missing.join(","), m.name]));
        }
        if m.in_netd_group {
            cmds.push(argv(&["gpasswd", "-d", m.name, NETD_GROUP]));
        }
    }
    cmds
}

fn ensure_service_user(invoking: Option<&str>) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    section("Users and groups");
    let uid = run_capture("id", &["-u", SERVICE_USER]).ok().and_then(|u| u.trim().parse::<u32>().ok());
    let home = fs::metadata(SERVICE_HOME).ok();
    let data_dir = Path::new(SERVICE_HOME).join(".glidex");
    let state = UserState {
        group: group_exists(NETD_GROUP),
        user: uid.is_some(),
        home_exists: home.is_some(),
        home_owned: home.as_ref().is_some_and(|m| Some(m.uid()) == uid),
        // Unreadable without the glidex group: ask root.
        data_dir: match fs::metadata(&data_dir) {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                ensure_sudo().is_ok() && succeeds("sudo", &["test", "-d", &data_dir.to_string_lossy()])
            }
            Err(_) => false,
        },
        users_group: group_exists(USERS_GROUP),
        admin_group: group_exists(ADMIN_GROUP),
        ui_group: group_exists(UI_USER),
        ui_user: succeeds("getent", &["passwd", UI_USER]),
        member: invoking.map(|u| Member {
            name: u,
            in_netd_group: user_in_group(u, NETD_GROUP),
            in_users: user_in_group(u, USERS_GROUP),
            in_admin: user_in_group(u, ADMIN_GROUP),
        }),
    };
    let cmds = service_user_commands(&state, nologin_shell());
    if cmds.is_empty() {
        println!("{} {} (home {}), {}, {}, {}", "Up to date:".green(), SERVICE_USER, SERVICE_HOME, UI_USER, USERS_GROUP, ADMIN_GROUP);
        return Ok(());
    }
    for cmd in cmds {
        sudo(&cmd)?;
    }
    println!("{} {} (home {}), {}, {}, {}", "Ready:".green(), SERVICE_USER, SERVICE_HOME, UI_USER, USERS_GROUP, ADMIN_GROUP);
    if let Some(m) = state.member {
        if !(m.in_users && m.in_admin) {
            println!(
                "{} added {} to {} and {} (gxctl on {}, administrator); log out and back in for it to apply.",
                "Note:".yellow(),
                m.name,
                USERS_GROUP,
                ADMIN_GROUP,
                API_SOCKET
            );
        }
        if m.in_netd_group {
            println!(
                "{} removed {} from the {} group: it opens glidex-netd's full socket, which is root-level control \
                 of host networking, so only the control plane is in it now. gxctl talks to {} instead.",
                "Note:".yellow(),
                m.name,
                NETD_GROUP,
                API_SOCKET
            );
        }
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
    // An OVS already set to dpdk-init=true (earlier run, or the host
    // rebooted without the sysctl applied) aborts at start without
    // hugepages, so they must be there before the install restarts it.
    if profile == Profile::Dpdk {
        reserve_hugepages()?;
    }
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
/// Reserve the dpdk profile's hugepages (persisted in the sysctl drop-in);
/// a no-op when the host is too small. `configure_host` reports the result.
fn reserve_hugepages() -> Result<()> {
    use sysconfig::NR_HUGEPAGES;
    let mem = fs::read_to_string("/proc/meminfo").ok().and_then(|m| sysconfig::mem_total_kb(&m)).unwrap_or(0);
    let reserved = read_sysctl(NR_HUGEPAGES).and_then(|v| v.parse().ok()).unwrap_or(0);
    let pages = sysconfig::hugepages_for(mem, reserved);
    if pages >= sysconfig::MIN_HUGEPAGES {
        write_sysctls(&[(NR_HUGEPAGES, pages.to_string())])?;
    }
    Ok(())
}

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
    if got < sysconfig::JUMBO_HUGEPAGES {
        println!(
            "{} {} hugepages is too few for jumbo-frame (MTU 9000) vhost-user networks: OVS-DPDK then needs ~3.3 GiB of mempools on top of the guests. Raise vm.nr_hugepages to at least {}.",
            "Note:".yellow(), got, sysconfig::JUMBO_HUGEPAGES
        );
    }
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

    let total_mem = sysconfig::socket_mem_mb(got);
    // Without an explicit mask, spread PMDs and socket memory over the
    // NUMA nodes that have hugepages (glidex_ovs::tuning).
    let plan = glidex_ovs::tuning::plan(&glidex_ovs::tuning::read_topology(exec));
    let (socket_mem, auto_plan) = match (&opts.pmd_cpu_mask, plan) {
        (None, Some(p)) => (p.socket_mem(total_mem), Some(p)),
        _ => (total_mem.to_string(), None),
    };
    if let Some(p) = &auto_plan {
        println!(
            "{} PMD threads on CPUs {:?} (mask {}), NUMA nodes {:?}; CPUs {:?} left for the OS and OVS's other threads",
            "Plan:".green(), p.pmd_cpus, p.pmd_mask(), p.nodes, p.lcore_cpus
        );
    }
    let settings = DpdkSettings {
        socket_mem: socket_mem.clone(),
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
    // As root on api.sock (break-glass: sees every project); the loopback
    // API answers only for a control plane from before authentication.
    let unix = ["curl", "-fsS", "--max-time", "5", "--unix-socket", API_SOCKET, "http://localhost/vms"];
    let body = if is_root() {
        run_capture(unix[0], &unix[1..]).ok()
    } else {
        let mut args = vec!["-n"];
        args.extend(unix);
        run_capture("sudo", &args).ok()
    }
    .or_else(|| run_capture("curl", &["-fsS", "--max-time", "5", &format!("http://{}/vms", API_ADDR)]).ok())?;
    count_active_vms(&body)
}

fn count_active_vms(body: &str) -> Option<usize> {
    let vms: Vec<serde_json::Value> = serde_json::from_str(body).ok()?;
    Some(vms.iter().filter(|v| matches!(v["state"].as_str(), Some("running" | "paused"))).count())
}

/// The `User=` of an existing unit file.
fn unit_user(unit_text: &str) -> Option<&str> {
    unit_text.lines().find_map(|l| l.strip_prefix("User=")).map(str::trim)
}

#[derive(Debug, PartialEq, Eq)]
enum PamAction {
    Write,
    UpToDate,
    /// The file exists without our marker: an administrator's.
    KeepLocal,
}

fn pam_action(existing: Option<&str>, ours: &str) -> PamAction {
    match existing {
        None => PamAction::Write,
        Some(t) if t == ours => PamAction::UpToDate,
        Some(t) if t.contains(PAM_MARKER) => PamAction::Write,
        Some(_) => PamAction::KeepLocal,
    }
}

/// packaging/glidex.pam is written for Debian/Ubuntu (`@include common-*`);
/// distributions without those files (Fedora/RHEL, Arch) include
/// `system-auth` instead.
fn render_pam(template: &str, debian_style: bool) -> String {
    if debian_style {
        return template.to_string();
    }
    template
        .replace("@include common-auth", "auth     include  system-auth")
        .replace("@include common-account", "account  include  system-auth")
}

/// `stat -c '%a %U:%G'` output of the policy directory, if it is right.
fn policy_dir_ok(stat: Option<&str>) -> bool {
    stat.map(str::trim) == Some(&format!("750 root:{}", NETD_GROUP))
}

/// Configuration for the services (spec/security.md §5.3, §13):
/// /etc/pam.d/glidex (unless an administrator took it over), a default
/// /etc/glidex/authd.json (once), control-plane.json.example (never
/// control-plane.json itself: defaults apply), and the site policy
/// directory.
fn install_service_config() -> Result<()> {
    section("Configuration");
    let root = workspace_root();
    let template = fs::read_to_string(root.join("packaging/glidex.pam")).context("packaging/glidex.pam")?;
    let debian_style = Path::new("/etc/pam.d/common-auth").exists() || !Path::new("/etc/pam.d/system-auth").exists();
    let pam = render_pam(&template, debian_style);
    match pam_action(fs::read_to_string(PAM_FILE).ok().as_deref(), &pam) {
        PamAction::Write => {
            sudo_write(PAM_FILE, &pam)?;
            println!("{} {}", "Updated:".green(), PAM_FILE);
        }
        PamAction::UpToDate => println!("{} {}", "Up to date:".green(), PAM_FILE),
        PamAction::KeepLocal => println!(
            "{} {} has local changes (no \"{}\" line); leaving it alone.",
            "Kept:".yellow(),
            PAM_FILE,
            PAM_MARKER
        ),
    }
    if Path::new(AUTHD_CONFIG).exists() {
        println!("{} {} (not overwritten)", "Kept:".green(), AUTHD_CONFIG);
    } else {
        sudo_write(AUTHD_CONFIG, AUTHD_CONFIG_DEFAULT)?;
        println!("{} {}", "Created:".green(), AUTHD_CONFIG);
    }
    if install_if_changed(&root.join("packaging/control-plane.json.example"), CP_CONFIG_EXAMPLE, "0644")? {
        println!("{} {}", "Updated:".green(), CP_CONFIG_EXAMPLE);
    }
    let stat = run_capture("stat", &["-c", "%a %U:%G", POLICY_DIR]).ok();
    if !policy_dir_ok(stat.as_deref()) {
        sudo(&argv(&["install", "-d", "-m", "0750", "-o", "root", "-g", NETD_GROUP, POLICY_DIR]))?;
        println!("{} {} (root:{} 0750)", "Ready:".green(), POLICY_DIR, NETD_GROUP);
    }
    Ok(())
}

/// What to do with glidex-authd's units: (start or restart the socket,
/// try-restart the service).
fn authd_actions(socket_active: bool, socket_changed: bool, service_changed: bool) -> Vec<Vec<String>> {
    let mut cmds = Vec::new();
    if !socket_active {
        cmds.push(argv(&["systemctl", "start", "glidex-authd.socket"]));
    } else if socket_changed {
        cmds.push(argv(&["systemctl", "restart", "glidex-authd.socket"]));
    }
    if service_changed {
        // Only if it is running; otherwise the next connection starts it.
        cmds.push(argv(&["systemctl", "try-restart", "glidex-authd.service"]));
    }
    cmds
}

/// systemd units so glidex comes up at boot: Open vSwitch → glidex-netd
/// (reconciles bridges, DPDK binding, IP migrations, NAT) → control plane
/// (as `glidex`) → web UI (as `glidex-ui`), plus the socket-activated
/// glidex-authd (root).
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
    let previous_unit = fs::read_to_string(CONTROL_PLANE_UNIT).ok();
    let cp_unit_changed = write_if_changed(CONTROL_PLANE_UNIT, &unit)?;
    let vm_units_changed = install_vm_units(&root)?;
    let ui_unit_changed = install_if_changed(&root.join("packaging/glidex-ui.service"), UI_UNIT, "0644")?;
    let authd_socket_changed =
        install_if_changed(&root.join("packaging/glidex-authd.socket"), AUTHD_SOCKET_UNIT, "0644")?;
    let authd_service_changed =
        install_if_changed(&root.join("packaging/glidex-authd.service"), AUTHD_SERVICE_UNIT, "0644")?;
    if cp_unit_changed || ui_unit_changed || authd_socket_changed || authd_service_changed || vm_units_changed {
        sudo(&argv(&["systemctl", "daemon-reload"]))?;
    }
    if vm_units_changed {
        apply_vm_accounting();
    }
    enable_unit("glidex-authd.socket")?;
    enable_unit("glidex-control-plane.service")?;
    enable_unit("glidex-ui.service")?;

    // glidex-authd first, so the control plane finds its socket.
    for cmd in authd_actions(
        unit_active("glidex-authd.socket"),
        authd_socket_changed,
        authd_service_changed || changed.authd,
    ) {
        sudo(&cmd)?;
    }
    println!("{} glidex-authd.socket (/run/glidex-authd/auth.sock)", "Running:".green());

    // Control plane: VMs run in their own units and survive a restart. A
    // control plane from before that still stops every VM when it stops,
    // so that one upgrade waits while VMs run.
    let cp = "glidex-control-plane.service";
    let cp_stale = changed.control_plane || cp_unit_changed;
    let legacy_running = previous_unit.as_deref().is_some_and(|u| u.contains(LEGACY_CP_MARKER));
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
        match (legacy_running, active_vms()) {
            (false, _) | (true, Some(0)) => {
                sudo(&argv(&["systemctl", "restart", cp]))?;
                println!("{} {} (as {}); running VMs keep running", "Restarted:".green(), cp, SERVICE_USER);
            }
            (true, n) => println!(
                "{} {} has {} running VM(s), and the running release stops VMs when it stops. \
                 This upgrade stops them once; later ones leave VMs running. Restart it when convenient: sudo systemctl restart {}",
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
        println!("{} {} on https://{}:5173", "Started:".green(), ui, host_name());
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

/// The VM unit template, its slice, the polkit rule and the shim's
/// allowlist (spec/reconciliation.md §13). Returns whether a unit changed.
fn install_vm_units(root: &Path) -> Result<bool> {
    let template = fs::read_to_string(root.join("packaging/glidex-vm@.service.in")).context("packaging/glidex-vm@.service.in")?;
    let unit = render_control_plane_unit(
        &template,
        SERVICE_USER,
        Path::new(SERVICE_HOME),
        &Path::new(BIN_DIR).join("glidex-vm-shim"),
        &control_plane_groups(group_exists),
    );
    let mut changed = write_if_changed(VM_UNIT, &unit)?;
    changed |= install_if_changed(&root.join("packaging/glidex-vms.slice"), VMS_SLICE, "0644")?;
    let rule = fs::read_to_string(root.join("packaging/50-glidex-vm.rules.in")).context("packaging/50-glidex-vm.rules.in")?;
    if write_if_changed(POLKIT_RULE, &render_polkit_rule(&rule, SERVICE_USER))? {
        println!("{} {}", "Updated:".green(), POLKIT_RULE);
    }
    if write_if_changed(SHIM_ALLOWLIST, &shim_allowlist(|p| Path::new(p).exists()))? {
        println!("{} {}", "Updated:".green(), SHIM_ALLOWLIST);
    }
    if changed {
        println!("{} {} and {}", "Updated:".green(), VM_UNIT, VMS_SLICE);
    }
    // A unit systemd would refuse surfaces now, not at the first VM start.
    // Not fatal: the check also complains about things it can't know yet.
    if let Err(e) = run_capture("systemd-analyze", &["verify", VM_UNIT, VMS_SLICE, CONTROL_PLANE_UNIT]) {
        println!("{} systemd-analyze verify: {}", "Warning:".yellow(), e);
    }
    Ok(changed)
}

/// Running VMs keep the unit settings they started with: give them the
/// template's accounting now, so metering sees their I/O without a
/// restart (`--runtime`: gone at reboot, when the template applies).
fn apply_vm_accounting() {
    let Ok(out) = run_capture("systemctl", &["list-units", "glidex-vm@*", "--state=active", "--no-legend", "--plain"]) else {
        return;
    };
    for unit in running_vm_units(&out) {
        let r = sudo(&argv(&["systemctl", "set-property", "--runtime", &unit, "IOAccounting=yes", "MemoryAccounting=yes"]));
        if let Err(e) = r {
            println!("{} accounting for {}: {}", "Warning:".yellow(), unit, e);
        }
    }
}

/// Unit names from `systemctl list-units --no-legend --plain`.
fn running_vm_units(list: &str) -> Vec<String> {
    list.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|u| u.starts_with("glidex-vm@") && u.ends_with(".service"))
        .map(String::from)
        .collect()
}

fn render_polkit_rule(template: &str, user: &str) -> String {
    template.replace("@USER@", user)
}

/// `/etc/glidex/vm-shim.json`: the installed hypervisors.
fn shim_allowlist(exists: impl Fn(&str) -> bool) -> String {
    let qemu = format!("qemu-system-{}", std::env::consts::ARCH);
    let candidates = [
        "/usr/local/bin/cloud-hypervisor".to_string(),
        "/usr/bin/cloud-hypervisor".to_string(),
        format!("/usr/bin/{}", qemu),
        format!("/usr/local/bin/{}", qemu),
    ];
    let list: Vec<&String> = candidates.iter().filter(|p| exists(p)).collect();
    serde_json::to_string_pretty(&serde_json::json!({ "hypervisors": list })).unwrap_or_default() + "\n"
}

/// The name the services' certificates are made for first (the FQDN when
/// the resolver knows it).
fn host_name() -> String {
    glidex_tls::LocalNames::discover().dns.into_iter().next().unwrap_or_else(|| "localhost".into())
}

/// SHA-256 fingerprint of a service's certificate, waiting a few seconds
/// for a service that is still generating it. The certificate isn't
/// secret, but the UI's state directory is private: read it as root.
fn cert_fingerprint(path: &str) -> Option<String> {
    for _ in 0..10 {
        let pem = std::fs::read(path).ok().or_else(|| {
            if is_root() {
                None
            } else {
                run_capture("sudo", &["-n", "cat", path]).ok().map(String::into_bytes)
            }
        });
        if let Some(fp) = pem.as_deref().and_then(glidex_tls::fingerprint_pem) {
            return Some(fp);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    None
}

fn print_usage(opts: &Options) {
    section("Quick Start");
    println!();
    if opts.services {
        println!("glidex runs as systemd services (control plane as '{}', web UI as '{}'):", SERVICE_USER, UI_USER);
        let host = host_name();
        println!("     web UI:  {}", format!("https://{}:5173", host).green());
        println!(
            "     API:     {} (members of {}), {} (tokens)",
            API_SOCKET.green(),
            USERS_GROUP,
            format!("https://{}:8841", host).green()
        );
        // Self-signed by default: what the browser's warning should show.
        for (what, path) in [("web UI", UI_CERT), ("API", CP_CERT)] {
            if let Some(fp) = cert_fingerprint(path) {
                println!("     {} certificate SHA-256: {}", what, fp);
            }
        }
        println!("     Both listen on every address over HTTPS; the installer does not change the firewall.");
        println!("     status:  {}", "systemctl status glidex-control-plane glidex-ui glidex-authd.socket".green());
        println!("     config:  {} (copy to control-plane.json to change defaults)", CP_CONFIG_EXAMPLE);
    } else {
        println!("1. Start the control plane server:");
        println!("     {}", "glidex-control-plane".green());
        println!();
        println!("2. (Optional) Start the web UI in another terminal:");
        println!("     {}", "glidex-ui".green());
        println!("     Then open https://localhost:5173 (a self-signed certificate; glidex-ui logs its fingerprint)");
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

    fn cmd_lines(s: &UserState, shell: &str) -> Vec<String> {
        service_user_commands(s, shell).iter().map(|c| c.join(" ")).collect()
    }

    const ALICE_NEW: Member = Member { name: "alice", in_netd_group: false, in_users: false, in_admin: false };
    const ALICE_DONE: Member = Member { name: "alice", in_netd_group: false, in_users: true, in_admin: true };
    const DONE: UserState = UserState {
        group: true,
        user: true,
        home_exists: true,
        home_owned: true,
        data_dir: true,
        users_group: true,
        admin_group: true,
        ui_group: true,
        ui_user: true,
        member: Some(ALICE_DONE),
    };

    #[test]
    fn identities_created_once() {
        let fresh = UserState {
            group: false,
            user: false,
            home_exists: false,
            home_owned: false,
            data_dir: false,
            users_group: false,
            admin_group: false,
            ui_group: false,
            ui_user: false,
            member: Some(ALICE_NEW),
        };
        assert_eq!(
            cmd_lines(&fresh, "/usr/sbin/nologin"),
            [
                "groupadd --system glidex",
                "useradd --system --gid glidex --home-dir /var/lib/glidex-control-plane --no-create-home --shell /usr/sbin/nologin --comment glidex services glidex",
                "install -d -m 0750 -o glidex -g glidex /var/lib/glidex-control-plane",
                "install -d -m 0750 -o glidex -g glidex /var/lib/glidex-control-plane/.glidex",
                "groupadd --system glidex-users",
                "groupadd --system glidex-admin",
                "useradd --system --user-group --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin --comment glidex web UI glidex-ui",
                "usermod -aG glidex-users,glidex-admin alice",
            ]
        );
        let all = cmd_lines(&fresh, "/usr/sbin/nologin").join("\n");
        assert!(!all.contains("-aG glidex alice"), "humans are no longer added to glidex");
        assert!(!all.contains("glidex-ui glidex\n") && !all.contains("-G glidex "), "glidex-ui is not in glidex");
        assert!(cmd_lines(&DONE, "/usr/sbin/nologin").is_empty(), "a re-run needs no root");
        // A home kept from an earlier install (user recreated with a new uid).
        let kept = UserState { home_owned: false, user: false, member: None, ..DONE };
        let lines = cmd_lines(&kept, "/bin/false");
        assert!(lines[0].starts_with("useradd"));
        assert_eq!(lines[1], "chown -R glidex:glidex /var/lib/glidex-control-plane");
        assert_eq!(lines.len(), 2);
        // glidex-ui's group left behind by an earlier user: reuse it.
        let group_kept = UserState { ui_user: false, ..DONE };
        assert_eq!(
            cmd_lines(&group_kept, "/bin/false"),
            ["useradd --system --gid glidex-ui --home-dir /nonexistent --no-create-home --shell /bin/false --comment glidex web UI glidex-ui"]
        );
        // Only the missing group is added.
        let half = UserState { member: Some(Member { in_admin: false, ..ALICE_DONE }), ..DONE };
        assert_eq!(cmd_lines(&half, "/bin/false"), ["usermod -aG glidex-admin alice"]);
    }

    #[test]
    fn earlier_install_member_of_glidex_is_moved_out() {
        let upgraded = UserState {
            users_group: false,
            admin_group: false,
            ui_group: false,
            ui_user: false,
            member: Some(Member { in_netd_group: true, ..ALICE_NEW }),
            ..DONE
        };
        let lines = cmd_lines(&upgraded, "/usr/sbin/nologin");
        let pos = |needle: &str| lines.iter().position(|l| l == needle).unwrap_or_else(|| panic!("missing {needle}: {lines:#?}"));
        // Added to the new groups before losing glidex, so access never lapses.
        assert!(pos("usermod -aG glidex-users,glidex-admin alice") < pos("gpasswd -d alice glidex"));
        assert!(pos("groupadd --system glidex-users") < pos("usermod -aG glidex-users,glidex-admin alice"));
        // Root runs (no invoking user) touch no human's groups.
        let root = UserState { member: None, ..upgraded };
        assert!(!cmd_lines(&root, "/bin/false").iter().any(|l| l.starts_with("usermod") || l.starts_with("gpasswd")));
    }

    #[test]
    fn running_vm_units_from_systemctl() {
        let out = "glidex-vm@93c3868e-726e-436b-afcd-95b9c90f6c38.service loaded active running glidex VM 93c3868e\n\
                   glidex-vms.slice loaded active active glidex VMs\n";
        assert_eq!(running_vm_units(out), vec!["glidex-vm@93c3868e-726e-436b-afcd-95b9c90f6c38.service".to_string()]);
        assert!(running_vm_units("").is_empty());
    }

    #[test]
    fn vm_unit_template_has_accounting() {
        let t = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../packaging/glidex-vm@.service.in")).unwrap();
        assert!(t.lines().any(|l| l == "IOAccounting=yes"));
        assert!(t.lines().any(|l| l == "MemoryAccounting=yes"));
        assert!(!t.contains("CPUAccounting"), "deprecated on systemd 259");
    }

    fn rendered_cp_unit() -> String {
        let template = std::fs::read_to_string(workspace_root().join("packaging/glidex-control-plane.service.in")).unwrap();
        let groups = control_plane_groups(|g| g == "kvm");
        render_control_plane_unit(&template, SERVICE_USER, Path::new(SERVICE_HOME), Path::new("/usr/local/bin/glidex-control-plane"), &groups)
    }

    /// Setting lines (`Key=value`) of a unit, comments dropped.
    fn settings(unit: &str) -> Vec<&str> {
        unit.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('[')).collect()
    }

    #[test]
    fn control_plane_unit_runs_as_the_service_user() {
        assert_eq!(control_plane_groups(|g| g == "kvm"), vec!["kvm"]);
        assert!(control_plane_groups(|_| false).is_empty(), "missing kvm group is left out");
        let unit = rendered_cp_unit();
        for line in [
            "User=glidex",
            "Group=glidex",
            "SupplementaryGroups=kvm",
            "Environment=HOME=/var/lib/glidex-control-plane",
            "WorkingDirectory=/var/lib/glidex-control-plane",
            "ExecStart=/usr/local/bin/glidex-control-plane",
            "After=network-online.target glidex-netd.service glidex-authd.socket",
            "Wants=glidex-netd.service glidex-authd.socket",
        ] {
            assert!(unit.lines().any(|l| l == line), "missing {line:?}");
        }
        for p in ["@USER@", "@HOME@", "@BIN@", "@GROUPS@"] {
            assert!(!unit.contains(p), "{p} filled");
        }
        assert_eq!(unit_user(&unit), Some("glidex"));
    }

    #[test]
    fn vm_unit_template_runs_the_shim_as_the_service_user() {
        let template = std::fs::read_to_string(workspace_root().join("packaging/glidex-vm@.service.in")).unwrap();
        let unit = render_control_plane_unit(&template, "glidex", Path::new("/var/lib/glidex-control-plane"), Path::new("/usr/local/bin/glidex-vm-shim"), &["kvm"]);
        let s = settings(&unit);
        for line in [
            "Type=notify",
            "NotifyAccess=main",
            "User=glidex",
            "Group=glidex",
            "SupplementaryGroups=kvm",
            "ExecStart=/usr/local/bin/glidex-vm-shim --vm %i --dir /run/glidex-cp/vms/%i",
            "KillMode=mixed",
            "TimeoutStopSec=330",
            "Restart=no",
            "Slice=glidex-vms.slice",
            "PrivateTmp=yes",
            "ReadWritePaths=/var/lib/glidex-control-plane /run/glidex-cp/vms/%i -/run/glidex/vhost",
            "DeviceAllow=/dev/kvm rw",
        ] {
            assert!(s.contains(&line), "missing {line:?}");
        }
        assert!(!unit.contains("@USER@") && !unit.contains("@BIN@"), "all placeholders filled");
        // Never enabled: only the control plane starts VMs (D10).
        assert!(!unit.contains("[Install]"));
        assert!(!s.iter().any(|l| l.starts_with("NoNewPrivileges=")), "would drop cloud-hypervisor's cap_net_admin");
    }

    #[test]
    fn polkit_rule_and_shim_allowlist() {
        let rule = render_polkit_rule(&std::fs::read_to_string(workspace_root().join("packaging/50-glidex-vm.rules.in")).unwrap(), "glidex");
        assert!(rule.contains(r#"subject.user == "glidex""#) && !rule.contains("@USER@"));
        assert!(rule.contains(r#"["start", "stop", "kill"]"#));
        let list: serde_json::Value = serde_json::from_str(&shim_allowlist(|p| p == "/usr/local/bin/cloud-hypervisor")).unwrap();
        assert_eq!(list, serde_json::json!({ "hypervisors": ["/usr/local/bin/cloud-hypervisor"] }));
    }

    #[test]
    fn control_plane_unit_is_sandboxed_without_breaking_vms() {
        let unit = rendered_cp_unit();
        let s = settings(&unit);
        for line in [
            "RuntimeDirectory=glidex-cp",
            "RuntimeDirectoryMode=0755",
            "RuntimeDirectoryPreserve=yes",
            "UMask=0077",
            "NoNewPrivileges=yes",
            "PrivateTmp=yes",
            "ProtectSystem=strict",
            "ProtectHome=yes",
            "ReadWritePaths=/var/lib/glidex-control-plane",
            "DevicePolicy=closed",
            "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK",
            "LockPersonality=yes",
            "RestrictSUIDSGID=yes",
            "SystemCallFilter=@system-service",
            "ProtectKernelTunables=yes",
            "ProtectKernelModules=yes",
            "ProtectKernelLogs=yes",
        ] {
            assert!(s.contains(&line), "missing {line:?}");
        }
        assert!(!s.contains(&"UMask=0007"), "consoles are reached through the API now");
        // Hypervisors run in the glidex-vm units (spec/reconciliation.md
        // §13.3): this one opens no device and serves no vhost-user socket.
        for key in ["DeviceAllow=", "ReadWritePaths=-/run/glidex/vhost", "KillMode="] {
            assert!(!s.iter().any(|l| l.starts_with(key)), "{key} is for the VM units");
        }
        // Not vetted against the disk tools it runs (qemu-img, sgdisk, …).
        for key in ["MemoryDenyWriteExecute=", "CapabilityBoundingSet="] {
            assert!(!s.iter().any(|l| l.starts_with(key)), "{key}");
        }
        // Firmware is in the service home, which ProtectHome= doesn't cover.
        assert!(SERVICE_HOME.starts_with("/var/lib/"));
        // Secrets come from credentials, offered commented out.
        for cred in ["tls-key", "oidc-client-secret"] {
            assert!(unit.lines().any(|l| l.starts_with(&format!("#LoadCredential={cred}:"))), "{cred}");
        }
        // VMs without a credential get no login: no site-wide password.
        assert!(!unit.contains("cloud-init-passwd-hash"));
        assert!(!unit.contains("unauthenticated"));
    }

    #[test]
    fn ui_unit_runs_as_its_own_user_after_the_control_plane() {
        let unit = std::fs::read_to_string(workspace_root().join("packaging/glidex-ui.service")).unwrap();
        let s = settings(&unit);
        for line in [
            "User=glidex-ui",
            "Group=glidex-ui",
            "ExecStart=/usr/local/bin/glidex-ui",
            "Environment=GLIDEX_UI_DIR=/usr/local/share/glidex/ui",
            "Environment=GLIDEX_API_SOCKET=/run/glidex-cp/ui.sock",
            "After=network-online.target glidex-control-plane.service",
            "Wants=glidex-control-plane.service",
            "CapabilityBoundingSet=",
            "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK",
            "InaccessiblePaths=-/run/glidex -/var/lib/glidex-control-plane -/run/glidex-authd",
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "ProtectHome=yes",
            "PrivateTmp=yes",
            "PrivateDevices=yes",
        ] {
            assert!(s.contains(&line), "missing {line:?}");
        }
        assert_eq!(unit_user(&unit), Some(UI_USER));
        // /run/glidex-cp (ui.sock) must stay reachable: "/run/glidex " is a
        // separate path, not a prefix of it.
        let inaccessible = s.iter().find(|l| l.starts_with("InaccessiblePaths=")).unwrap();
        assert!(!inaccessible.contains("glidex-cp"));
        assert!(unit.contains("GLIDEX_UI_TLS_CERT"));
    }

    #[test]
    fn authd_units_are_socket_activated_for_the_glidex_group() {
        let socket = std::fs::read_to_string(workspace_root().join("packaging/glidex-authd.socket")).unwrap();
        let s = settings(&socket);
        for line in ["ListenStream=/run/glidex-authd/auth.sock", "SocketUser=root", "SocketGroup=glidex", "SocketMode=0660", "WantedBy=sockets.target"] {
            assert!(s.contains(&line), "missing {line:?}");
        }
        let service = std::fs::read_to_string(workspace_root().join("packaging/glidex-authd.service")).unwrap();
        let s = settings(&service);
        for line in ["ExecStart=/usr/local/bin/glidex-authd", "Requires=glidex-authd.socket", "PrivateNetwork=yes", "NoNewPrivileges=yes"] {
            assert!(s.contains(&line), "missing {line:?}");
        }
        assert!(!s.iter().any(|l| l.starts_with("User=")), "root: it runs PAM");
        let lines = |a, b, c| -> Vec<String> { authd_actions(a, b, c).iter().map(|c| c.join(" ")).collect() };
        assert_eq!(lines(false, true, true), ["systemctl start glidex-authd.socket", "systemctl try-restart glidex-authd.service"]);
        assert_eq!(lines(true, true, false), ["systemctl restart glidex-authd.socket"]);
        assert!(lines(true, false, false).is_empty(), "a re-run needs no root");
    }

    #[test]
    fn pam_file_is_ours_until_an_admin_edits_it() {
        let template = std::fs::read_to_string(workspace_root().join("packaging/glidex.pam")).unwrap();
        assert!(template.contains(PAM_MARKER));
        assert!(template.lines().any(|l| l == "@include common-auth"));
        assert!(template.lines().any(|l| l == "@include common-account"));
        let rh = render_pam(&template, false);
        assert!(rh.lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == ["auth", "include", "system-auth"]));
        assert!(rh.lines().any(|l| l.split_whitespace().collect::<Vec<_>>() == ["account", "include", "system-auth"]));
        assert!(!rh.lines().any(|l| l.starts_with("@include")));
        assert!(rh.contains(PAM_MARKER));

        let ours = render_pam(&template, true);
        assert_eq!(pam_action(None, &ours), PamAction::Write);
        assert_eq!(pam_action(Some(&ours), &ours), PamAction::UpToDate);
        // An older glidex version of the file is updated.
        assert_eq!(pam_action(Some(&format!("# {PAM_MARKER}\nauth required pam_unix.so\n")), &ours), PamAction::Write);
        // An administrator's file (marker removed) is kept.
        assert_eq!(pam_action(Some("auth required pam_sss.so\naccount required pam_sss.so\n"), &ours), PamAction::KeepLocal);
    }

    #[test]
    fn default_authd_config_and_policy_dir() {
        let v: serde_json::Value = serde_json::from_str(AUTHD_CONFIG_DEFAULT).unwrap();
        assert_eq!(v["service_user"], SERVICE_USER);
        assert_eq!(v["allowed_groups"], serde_json::json!([USERS_GROUP]));
        let example: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(workspace_root().join("packaging/control-plane.json.example")).unwrap()).unwrap();
        assert_eq!(example["users_group"], USERS_GROUP);
        assert_eq!(example["admin_group"], ADMIN_GROUP);
        assert_eq!(example["ui_user"], UI_USER);
        assert_eq!(example["authz"]["policy_files_dir"], POLICY_DIR);
        assert!(policy_dir_ok(Some("750 root:glidex\n")));
        assert!(!policy_dir_ok(Some("755 root:root")));
        assert!(!policy_dir_ok(None));
    }

    #[test]
    fn active_vms_counts_running_and_paused() {
        let body = r#"[{"state":"running"},{"state":"paused"},{"state":"stopped"}]"#;
        assert_eq!(count_active_vms(body), Some(2));
        assert_eq!(count_active_vms(r#"{"error":{"code":"unauthenticated"}}"#), None);
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
        assert_eq!(binaries(&Options::default()), ["glidex-control-plane", "gxctl", "glidex-ui", "glidex-vm-shim", "glidex-netd", "glidex-authd"]);
        let no_net = Options { networking: false, ..Options::default() };
        assert!(!binaries(&no_net).contains(&"glidex-netd"));
        let no_services = Options { services: false, ..Options::default() };
        assert!(!binaries(&no_services).contains(&"glidex-authd"));
        assert!(build_args(&Options::default()).windows(2).any(|w| w == ["-p", "glidex-authd"]));
        assert!(!build_args(&no_services).contains(&"glidex-authd"));
    }

    #[test]
    fn toolchains_are_pinned_and_verified() {
        let x86 = Platform { os: "linux", arch: "x86_64" };
        let arm = Platform { os: "linux", arch: "aarch64" };
        assert_eq!(
            rustup_url(&x86),
            format!("https://static.rust-lang.org/rustup/archive/{RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init")
        );
        assert!(rustup_url(&arm).contains("/aarch64-unknown-linux-gnu/"));
        assert_eq!(bun_url(&x86), format!("https://github.com/oven-sh/bun/releases/download/bun-v{BUN_VERSION}/bun-linux-x64.zip"));
        assert!(bun_url(&arm).ends_with("/bun-linux-aarch64.zip"));
        for sha in [rustup_asset(&x86).1, rustup_asset(&arm).1, bun_asset(&x86).1, bun_asset(&arm).1] {
            assert_eq!(sha.len(), 64);
            assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
        }
        // No `curl … | sh` left in the installer.
        let src = std::fs::read_to_string(workspace_root().join("crates/glidex-install/src/main.rs")).unwrap();
        for pipe in ["| sh", "| bash"] {
            let code = src.lines().filter(|l| !l.trim_start().starts_with("//") && !l.contains("assert"));
            assert!(!code.into_iter().any(|l| l.contains("curl") && l.contains(pipe)), "{pipe}");
        }
    }

    #[test]
    fn bun_is_upgraded_to_the_pin_but_never_downgraded() {
        assert_eq!(parse_version("1.4.2\n"), Some((1, 4, 2)));
        assert_eq!(parse_version("v1.10"), Some((1, 10, 0)));
        assert_eq!(parse_version("1.5.0-canary.3"), Some((1, 5, 0)));
        assert_eq!(parse_version(""), None);
        assert_eq!(bun_action(None, false), BunAction::Install);
        assert_eq!(bun_action(None, true), BunAction::Foreign, "a distro/npm bun is left alone");
        assert_eq!(bun_action(Some("1.1.0"), true), BunAction::Install);
        assert_eq!(bun_action(Some(BUN_VERSION), true), BunAction::Keep);
        assert_eq!(bun_action(Some("99.0.0"), false), BunAction::Keep);
        assert_eq!(bun_action(Some("garbage"), false), BunAction::Install);
    }

    #[test]
    fn download_verified_rejects_a_bad_checksum() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src");
        fs::write(&src, b"abc").unwrap();
        let url = format!("file://{}", src.display());
        let dest = dir.path().join("dest");
        download_verified(&url, &dest, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad").unwrap();
        assert!(dest.exists());
        let bad = dir.path().join("bad");
        let err = download_verified(&url, &bad, &"0".repeat(64)).unwrap_err();
        assert!(format!("{err:#}").contains("Checksum mismatch"));
        assert!(!bad.exists(), "a mismatching download is deleted");
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
