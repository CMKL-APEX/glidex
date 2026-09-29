use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

mod sysconfig;
mod uninstall;

const CLOUD_HYPERVISOR_VERSION: &str = "v50.0";
/// Release tag of https://github.com/cloud-hypervisor/edk2/releases to fetch
/// the UEFI firmware from. Bump together with the digests in
/// `firmware_asset`.
const EDK2_FIRMWARE_VERSION: &str = "ch-811ce5ea35";

struct Platform {
    os: &'static str,
    arch: &'static str,
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("uninstall") {
        return uninstall::main(&args[1..]);
    }
    print_banner();

    let platform = detect_platform()?;
    println!(
        "{} {}-{}",
        "Detected platform:".green(),
        platform.os,
        platform.arch
    );

    let install_dir = resolve_install_dir()?;

    print_plan(&install_dir);
    if !confirm_yn("Continue with installation?", true)? {
        println!("Installation cancelled");
        return Ok(());
    }

    install_rust()?;
    install_bun()?;
    install_cloud_hypervisor(&platform, &install_dir)?;
    install_uefi_firmware(&platform)?;
    install_qemu()?;
    check_kvm()?;
    build_project(&install_dir)?;
    setup_networking()?;
    install_services(&install_dir)?;
    install_ui_deps()?;
    print_usage();

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

fn resolve_install_dir() -> Result<PathBuf> {
    let dir = if is_root() {
        PathBuf::from("/usr/local/bin")
    } else {
        dirs::home_dir()
            .context("Could not determine home directory")?
            .join(".local/bin")
    };
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    println!("{} {}", "Install directory:".yellow(), dir.display());
    if !is_root() {
        println!(
            "{} make sure {} is in your PATH",
            "Note:".yellow(),
            dir.display()
        );
    }
    Ok(dir)
}

fn print_plan(install_dir: &Path) {
    println!();
    println!("{}", "This installer will:".cyan().bold());
    println!("  1. Install Rust via rustup (if missing)");
    println!("  2. Install Bun for the UI dev server (if missing)");
    println!(
        "  3. Install Cloud-Hypervisor {} to {}",
        CLOUD_HYPERVISOR_VERSION,
        install_dir.display()
    );
    println!(
        "  4. Download Cloud-Hypervisor UEFI firmware ({}) to ~/.glidex, plus dosfstools/mtools for cloud-init seeds",
        EDK2_FIRMWARE_VERSION
    );
    println!("  5. (Optional) Install QEMU via system package manager");
    println!("  6. Check KVM access");
    println!("  7. Build the control plane (cargo build --release)");
    println!("  8. (Optional) VM networking: Open vSwitch, glidex-netd service, glidex group");
    println!("  9. (Optional) Start glidex at boot: systemd units for glidex-netd and the control plane");
    println!("  10. Install UI npm dependencies (bun install)");
    println!();
}

fn section(title: &str) {
    println!();
    println!("{} {} {}", "===".cyan(), title.cyan().bold(), "===".cyan());
}

// --- Prompting ---

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

fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

/// Install `src` to `dst`. When not running as root, fall back to `sudo install`
/// if a plain copy isn't permitted on the destination.
fn install_binary(src: &Path, dst: &Path) -> Result<()> {
    if is_root() {
        return run(
            "install",
            &[
                "-m",
                "0755",
                src.to_str().unwrap(),
                dst.to_str().unwrap(),
            ],
        );
    }
    // Try a plain copy first (works under $HOME/.local/bin).
    if let Some(parent) = dst.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::copy(src, dst) {
        Ok(_) => set_executable(dst),
        Err(_) => run(
            "sudo",
            &[
                "install",
                "-o",
                "root",
                "-g",
                "root",
                "-m",
                "0755",
                src.to_str().unwrap(),
                dst.to_str().unwrap(),
            ],
        ),
    }
}

// --- Install steps ---

fn install_rust() -> Result<()> {
    section("Rust");
    if command_exists("rustc") {
        let version = run_capture("rustc", &["--version"]).unwrap_or_default();
        println!("{} {}", "Rust is installed:".green(), version.trim());
        if command_exists("rustup") {
            let _ = run("rustup", &["update", "stable"]);
        }
        return Ok(());
    }
    println!("{}", "Rust not found; installing via rustup...".yellow());
    run_sh(
        "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable",
    )?;
    println!(
        "{} run {} or open a new shell before continuing",
        "Rust installed.".green(),
        "source $HOME/.cargo/env".bold()
    );
    Ok(())
}

fn install_bun() -> Result<()> {
    section("Bun (UI dev server)");
    if command_exists("bun") {
        let version = run_capture("bun", &["--version"]).unwrap_or_default();
        println!("{} {}", "Bun is installed:".green(), version.trim());
        return Ok(());
    }
    println!("{}", "Bun not found; installing...".yellow());
    run_sh("curl -fsSL https://bun.sh/install | bash")?;
    println!(
        "{} ensure {} is in your PATH",
        "Bun installed.".green(),
        "$HOME/.bun/bin".bold()
    );
    Ok(())
}

fn install_cloud_hypervisor(platform: &Platform, install_dir: &Path) -> Result<()> {
    section("Cloud-Hypervisor");
    if command_exists("cloud-hypervisor") {
        let v = run_capture("cloud-hypervisor", &["--version"]).unwrap_or_default();
        println!(
            "{} {}",
            "Cloud-Hypervisor is installed:".green(),
            v.lines().next().unwrap_or("").trim()
        );
        if !confirm_yn("Reinstall/update Cloud-Hypervisor?", false)? {
            return Ok(());
        }
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

    let target = install_dir.join("cloud-hypervisor");
    install_binary(&download_path, &target)?;
    println!("{} {}", "Installed:".green(), target.display());
    Ok(())
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
/// images (distro cloud images), and the tools the control plane needs to
/// build cloud-init seed images for them.
fn install_uefi_firmware(platform: &Platform) -> Result<()> {
    section("Cloud-Hypervisor UEFI Firmware");
    let (asset, sha256) = firmware_asset(platform);
    let glidex_dir = dirs::home_dir()
        .context("No home directory")?
        .join(".glidex");
    fs::create_dir_all(&glidex_dir)?;
    let dest = glidex_dir.join(asset);

    let up_to_date = dest.exists() && file_sha256(&dest)? == sha256;
    if up_to_date {
        println!("{} {}", "Firmware already installed:".green(), dest.display());
    } else if dest.exists()
        && !confirm_yn(
            &format!(
                "{} differs from {}. Replace it?",
                dest.display(),
                EDK2_FIRMWARE_VERSION
            ),
            false,
        )?
    {
        println!("Keeping existing {}", dest.display());
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

    ensure_tools(&[("mkdosfs", "dosfstools"), ("mcopy", "mtools")])
}

fn install_qemu() -> Result<()> {
    section("QEMU (Optional)");
    if command_exists("qemu-system-x86_64") {
        let v = run_capture("qemu-system-x86_64", &["--version"]).unwrap_or_default();
        println!(
            "{} {}",
            "QEMU is installed:".green(),
            v.lines().next().unwrap_or("").trim()
        );
        return Ok(());
    }
    if !confirm_yn("Install QEMU via your system package manager?", false)? {
        return Ok(());
    }

    if command_exists("apt-get") {
        run_sh("sudo apt-get update && sudo apt-get install -y qemu-system-x86 qemu-kvm")?;
    } else if command_exists("dnf") {
        run_sh("sudo dnf install -y qemu-kvm")?;
    } else if command_exists("yum") {
        run_sh("sudo yum install -y qemu-kvm")?;
    } else if command_exists("pacman") {
        run_sh("sudo pacman -S --noconfirm qemu-base")?;
    } else {
        println!(
            "{}",
            "Could not detect a package manager; please install QEMU manually.".yellow()
        );
    }
    Ok(())
}

fn check_kvm() -> Result<()> {
    section("KVM Access");
    if !Path::new("/dev/kvm").exists() {
        println!("{}", "/dev/kvm not found — KVM may not be enabled.".red());
        println!("  - Verify your CPU supports Intel VT-x or AMD-V");
        println!("  - Enable virtualization in BIOS/UEFI");
        println!("  - Load the module: sudo modprobe kvm_intel  (or kvm_amd)");
        return Ok(());
    }
    let accessible = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok();
    if accessible {
        println!("{}", "KVM access: OK".green());
    } else {
        let user = env::var("SUDO_USER").or_else(|_| env::var("USER")).unwrap_or_default();
        println!(
            "{}",
            "/dev/kvm exists but is not writable by your user.".yellow()
        );
        if !user.is_empty() && user != "root" && group_exists("kvm") && confirm_yn(&format!("Add {} to the kvm group?", user), true)? {
            sudo(&["usermod".into(), "-aG".into(), "kvm".into(), user.clone()])?;
            println!("{} log out and back in so the group applies.", "Added:".green());
        } else {
            println!("  Run: sudo usermod -aG kvm {}", user);
            println!("  Then log out and back in.");
        }
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    // crates/glidex-install -> crates -> workspace root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn build_project(install_dir: &Path) -> Result<()> {
    section("Building Control Plane");
    let root = workspace_root();
    run_in(
        "cargo",
        &["build", "--release", "-p", "glidex-control-plane", "-p", "glidex-netd"],
        &root,
    )?;
    println!("{}", "Build successful".green());

    let target_dir = env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join("target"));
    let server = target_dir.join("release/glidex-control-plane");
    let cli = target_dir.join("release/gxctl");

    if confirm_yn(
        &format!("Install binaries to {}?", install_dir.display()),
        false,
    )? {
        install_binary(&server, &install_dir.join("glidex-control-plane"))?;
        install_binary(&cli, &install_dir.join("gxctl"))?;
        println!(
            "{} {}",
            "Installed binaries to".green(),
            install_dir.display()
        );
    }
    Ok(())
}

/// Privileged commands for the networking step, in order (spec §13).
/// Kept separate from execution so the exact root actions are testable.
fn networking_commands(user: Option<&str>, netd_src: &Path, unit_src: &Path) -> Vec<Vec<String>> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let mut cmds = vec![s(&["groupadd", "-f", NETD_GROUP])];
    if let Some(user) = user.filter(|u| *u != "root") {
        cmds.push(s(&["usermod", "-aG", NETD_GROUP, user]));
    }
    cmds.push(s(&["install", "-m", "0755", "-o", "root", "-g", "root", &netd_src.to_string_lossy(), NETD_BIN]));
    cmds.push(s(&["install", "-m", "0644", "-o", "root", "-g", "root", &unit_src.to_string_lossy(), NETD_UNIT]));
    cmds.push(s(&["systemctl", "daemon-reload"]));
    cmds.push(s(&["systemctl", "enable", "glidex-netd.service"]));
    // restart (not just start) so an upgraded binary takes effect.
    cmds.push(s(&["systemctl", "restart", "glidex-netd.service"]));
    cmds
}

const NETD_GROUP: &str = "glidex";
const NETD_BIN: &str = "/usr/local/bin/glidex-netd";
const NETD_UNIT: &str = "/etc/systemd/system/glidex-netd.service";

fn sudo(args: &[String]) -> Result<()> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if is_root() {
        run(refs[0], &refs[1..])
    } else {
        run("sudo", &refs)
    }
}

/// VM networking (spec §13): OVS, the glidex-netd service, the glidex
/// group, and CAP_NET_ADMIN for cloud-hypervisor's tap devices.
fn setup_networking() -> Result<()> {
    use glidex_ovs::install::{install, InstallRequest, Profile};
    use glidex_ovs::host::ProbeOptions;
    use glidex_ovs::{OvsError, SystemExec};

    section("VM Networking (Open vSwitch + glidex-netd)");
    println!("glidex-netd is a small root service that manages Open vSwitch bridges,");
    println!("NAT networks (DHCP + masquerade) and VM ports. Members of the '{}'", NETD_GROUP);
    println!("group can use it, which amounts to network-admin rights on this host.");
    if !confirm_yn("Set up VM networking?", true)? {
        return Ok(());
    }
    if !is_root() {
        // Cache sudo credentials up front so a password prompt doesn't
        // count against a command's timeout.
        run("sudo", &["-v"])?;
    }

    // 1. Open vSwitch from distro packages.
    let profile = match prompt_line("Open vSwitch profile [kernel/dpdk] (default: kernel): ")?.as_str() {
        "dpdk" => Profile::Dpdk,
        _ => Profile::Kernel,
    };
    let exec = if is_root() { SystemExec::new() } else { SystemExec::sudo() };
    let ch_binary = run_capture("sh", &["-c", "command -v cloud-hypervisor"]).ok().map(|p| PathBuf::from(p.trim()));
    let probe = ProbeOptions { ch_binary: ch_binary.clone() };
    let mut req = InstallRequest { profile, source_build: false, confirm: false };
    loop {
        match install(&exec, &probe, &req) {
            Ok(report) => {
                if report.changed {
                    println!("{} Open vSwitch {}", "Installed:".green(), report.ovs_version.unwrap_or_default());
                } else {
                    println!("{} Open vSwitch {}", "Already installed:".green(), report.ovs_version.unwrap_or_default());
                }
                for w in report.warnings {
                    println!("{} {}", "Note:".yellow(), w);
                }
                break;
            }
            Err(OvsError::ConfirmationRequired { impact }) if !req.confirm => {
                println!("{} {}", "Warning:".yellow(), impact);
                if !confirm_yn("Proceed?", false)? {
                    println!("Skipping Open vSwitch installation");
                    return Ok(());
                }
                req.confirm = true;
            }
            Err(e) => bail!("Open vSwitch installation failed: {}", e),
        }
    }

    // 2. Host settings: forwarding for NAT; hugepages, vfio-pci and DPDK
    //    init for the dpdk profile.
    configure_host(profile, &exec, &probe)?;

    // 3. glidex-netd service and group.
    let root = workspace_root();
    let target_dir = env::var("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|_| root.join("target"));
    let netd_src = target_dir.join("release/glidex-netd");
    let unit_src = root.join("packaging/glidex-netd.service");
    let user = env::var("SUDO_USER").or_else(|_| env::var("USER")).ok();
    for cmd in networking_commands(user.as_deref(), &netd_src, &unit_src) {
        sudo(&cmd)?;
    }
    println!("{} glidex-netd (systemctl status glidex-netd)", "Running:".green());

    // 4. Tap devices: cloud-hypervisor brings them up itself.
    if let Some(ch) = ch_binary {
        ensure_tools(&[("setcap", setcap_package())])?;
        println!();
        println!("Tap-based VM networking needs CAP_NET_ADMIN on {}.", ch.display());
        println!("Anyone who can run that binary gets the capability for it.");
        if confirm_yn("Grant cap_net_admin to cloud-hypervisor?", true)? {
            sudo(&["setcap".into(), "cap_net_admin+ep".into(), ch.to_string_lossy().into_owned()])?;
        }
    }
    if user.as_deref().is_some_and(|u| u != "root") {
        println!(
            "{} log out and back in (or run `newgrp {}`) so the group applies.",
            "Note:".yellow(),
            NETD_GROUP
        );
    }
    Ok(())
}

fn setcap_package() -> &'static str {
    if command_exists("apt-get") {
        "libcap2-bin"
    } else {
        "libcap"
    }
}

/// Write a root-owned 0644 file (through sudo when not root).
fn sudo_write(path: &str, contents: &str) -> Result<()> {
    let tmp = tempfile::NamedTempFile::new()?;
    fs::write(tmp.path(), contents)?;
    sudo(&[
        "install".into(),
        "-D".into(),
        "-m".into(),
        "0644".into(),
        tmp.path().to_string_lossy().into_owned(),
        path.into(),
    ])
}

fn read_sysctl(key: &str) -> Option<String> {
    fs::read_to_string(sysconfig::proc_path(key)).ok().map(|v| v.trim().to_string())
}

/// Persist and apply the kernel settings VM networking needs (see
/// sysconfig.rs), then initialize OVS-DPDK for the dpdk profile.
fn configure_host(
    profile: glidex_ovs::install::Profile,
    exec: &glidex_ovs::SystemExec,
    probe: &glidex_ovs::host::ProbeOptions,
) -> Result<()> {
    use glidex_ovs::install::{init_dpdk, DpdkSettings, Profile};
    use glidex_ovs::OvsError;
    use sysconfig::{IP_FORWARD, NR_HUGEPAGES};

    println!();
    println!("NAT networks route VM traffic through this host, which needs");
    println!("{}=1 (persisted in {}).", IP_FORWARD, sysconfig::SYSCTL_DROPIN);
    let mut wanted: Vec<(&str, String)> = Vec::new();
    if read_sysctl(IP_FORWARD).as_deref() != Some("1") {
        if confirm_yn("Enable IPv4 forwarding?", true)? {
            wanted.push((IP_FORWARD, "1".into()));
        }
    } else {
        // Already on; still persist it so a reboot doesn't lose it.
        wanted.push((IP_FORWARD, "1".into()));
    }

    let dpdk = profile == Profile::Dpdk;
    if dpdk {
        let mem = fs::read_to_string("/proc/meminfo").ok().and_then(|m| sysconfig::mem_total_kb(&m)).unwrap_or(0);
        let reserved = read_sysctl(NR_HUGEPAGES).and_then(|v| v.parse().ok()).unwrap_or(0);
        let pages = sysconfig::hugepages_for(mem, reserved);
        if pages < sysconfig::MIN_HUGEPAGES {
            println!(
                "{} only {} MiB of RAM; OVS-DPDK needs at least {} MiB of hugepages. Skipping DPDK setup.",
                "Warning:".yellow(),
                mem / 1024,
                sysconfig::MIN_HUGEPAGES * 2
            );
            return write_sysctls(&wanted);
        }
        println!();
        println!("OVS-DPDK and vhost-user guests need 2 MiB hugepages. Reserving {} pages", pages);
        println!("({} MiB) takes that memory away from everything else on this host.", pages * 2);
        if !confirm_yn(&format!("Reserve {} MiB of hugepages?", pages * 2), true)? {
            println!("Skipping DPDK setup (run `gxctl ovs dpdk-init` after reserving hugepages).");
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
    println!("{} {} hugepages", "Reserved:".green(), got);
    if got < sysconfig::MIN_HUGEPAGES {
        println!(
            "{} only {} hugepages could be reserved (memory is fragmented). Reboot to apply {}, then run `gxctl ovs dpdk-init`.",
            "Warning:".yellow(),
            got,
            sysconfig::SYSCTL_DROPIN
        );
        return Ok(());
    }

    // vfio-pci binds physical NICs for DPDK uplinks.
    sudo_write(sysconfig::MODULES_DROPIN, "# Managed by glidex-install\nvfio-pci\n")?;
    if let Err(e) = sudo(&["modprobe".into(), "vfio-pci".into()]) {
        println!("{} could not load vfio-pci: {}", "Note:".yellow(), e);
    }
    let iommu = fs::read_dir("/sys/kernel/iommu_groups").map(|mut d| d.next().is_some()).unwrap_or(false);
    if !iommu {
        println!(
            "{} no IOMMU groups: DPDK NIC uplinks need intel_iommu=on / amd_iommu=on (and VT-d/AMD-Vi in firmware).",
            "Note:".yellow()
        );
        println!("      vhost-user and AF_XDP don't need it.");
    }

    let socket_mem = sysconfig::socket_mem_mb(got);
    let pmd = prompt_line("PMD CPU mask, hex (e.g. 0x2; empty = let OVS choose): ")?;
    let mut settings = DpdkSettings {
        socket_mem: socket_mem.to_string(),
        pmd_cpu_mask: (!pmd.is_empty()).then_some(pmd),
        confirm: false,
    };
    loop {
        match init_dpdk(exec, probe, &settings) {
            Ok(()) => {
                println!("{} OVS-DPDK ({} MiB socket memory)", "Initialized:".green(), socket_mem);
                return Ok(());
            }
            Err(OvsError::ConfirmationRequired { impact }) if !settings.confirm => {
                println!("{} {}", "Warning:".yellow(), impact);
                if !confirm_yn("Proceed?", false)? {
                    println!("Skipping DPDK init (run `gxctl ovs dpdk-init` later).");
                    return Ok(());
                }
                settings.confirm = true;
            }
            Err(e) => bail!("OVS-DPDK init failed: {}", e),
        }
    }
}

/// Merge `wanted` into the sysctl drop-in (keeping previously recorded
/// original values) and apply it.
fn write_sysctls(wanted: &[(&str, String)]) -> Result<()> {
    if wanted.is_empty() {
        return Ok(());
    }
    let existing = fs::read_to_string(sysconfig::SYSCTL_DROPIN).ok();
    let settings = sysconfig::merge(existing.as_deref(), wanted, read_sysctl);
    sudo_write(sysconfig::SYSCTL_DROPIN, &sysconfig::render(&settings))?;
    sudo(&["sysctl".into(), "-q".into(), "-p".into(), sysconfig::SYSCTL_DROPIN.into()])?;
    for (key, value) in wanted {
        println!("{} {} = {}", "Set:".green(), key, value);
    }
    Ok(())
}

const CONTROL_PLANE_UNIT: &str = "/etc/systemd/system/glidex-control-plane.service";

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
    ["kvm", NETD_GROUP].into_iter().filter(|g| exists(g)).collect()
}

fn group_exists(name: &str) -> bool {
    Command::new("getent")
        .args(["group", name])
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// systemd units so glidex comes up at boot: Open vSwitch → glidex-netd
/// (reconciles bridges, DPDK binding, IP migrations, NAT) → control plane.
fn install_services(install_dir: &Path) -> Result<()> {
    section("Start at Boot (systemd)");
    if !Path::new("/run/systemd/system").exists() {
        println!("{}", "systemd is not running here; skipping".yellow());
        return Ok(());
    }
    println!("glidex-netd starts after Open vSwitch and restores host networking;");
    println!("the control plane then starts as your user and stops VMs on shutdown.");
    if !confirm_yn("Start glidex at boot?", true)? {
        return Ok(());
    }

    let root = workspace_root();
    let target_dir = env::var("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|_| root.join("target"));
    let bin = install_dir.join("glidex-control-plane");
    if !bin.exists() {
        println!("The service needs the control plane installed at {}.", bin.display());
        if !confirm_yn("Install the release binaries now?", true)? {
            println!("Skipping: install the binaries first, then re-run the installer.");
            return Ok(());
        }
        install_binary(&target_dir.join("release/glidex-control-plane"), &bin)?;
        install_binary(&target_dir.join("release/gxctl"), &install_dir.join("gxctl"))?;
    }

    let user = env::var("SUDO_USER").or_else(|_| env::var("USER")).unwrap_or_else(|_| "root".into());
    let home = if user == "root" {
        PathBuf::from("/root")
    } else {
        run_capture("getent", &["passwd", &user])
            .ok()
            .and_then(|l| l.trim().split(':').nth(5).map(PathBuf::from))
            .or_else(dirs::home_dir)
            .context("could not determine the user's home directory")?
    };
    let groups = control_plane_groups(group_exists);
    let template = fs::read_to_string(root.join("packaging/glidex-control-plane.service.in"))
        .context("packaging/glidex-control-plane.service.in")?;
    let unit = render_control_plane_unit(&template, &user, &home, &bin, &groups);

    let tmp = TempDir::new()?;
    let rendered = tmp.path().join("glidex-control-plane.service");
    fs::write(&rendered, unit)?;
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    sudo(&s(&["install", "-m", "0644", "-o", "root", "-g", "root", &rendered.to_string_lossy(), CONTROL_PLANE_UNIT]))?;
    if Path::new(NETD_UNIT).exists() {
        // Refresh netd's unit too (e.g. Type=notify ordering).
        sudo(&s(&["install", "-m", "0644", "-o", "root", "-g", "root", &root.join("packaging/glidex-netd.service").to_string_lossy(), NETD_UNIT]))?;
    }
    sudo(&s(&["systemctl", "daemon-reload"]))?;
    let mut enable = s(&["systemctl", "enable", "glidex-control-plane.service"]);
    if Path::new(NETD_UNIT).exists() {
        enable.push("glidex-netd.service".into());
    }
    sudo(&enable)?;
    println!("{} glidex-control-plane.service (runs as {})", "Enabled:".green(), user);
    if confirm_yn("Start it now? (a control plane already running on port 8080 must be stopped first)", false)? {
        sudo(&s(&["systemctl", "restart", "glidex-control-plane.service"]))?;
        println!("{} systemctl status glidex-control-plane", "Started:".green());
    }
    Ok(())
}

fn install_ui_deps() -> Result<()> {
    section("UI Dependencies");
    if !command_exists("bun") {
        println!(
            "{}",
            "bun not in PATH; skipping. Re-run the installer after adding bun to PATH.".yellow()
        );
        return Ok(());
    }
    let ui_dir = workspace_root().join("crates/glidex-ui/ui");
    if !ui_dir.is_dir() {
        println!("{}", "UI directory not found; skipping.".yellow());
        return Ok(());
    }
    run_in("bun", &["install"], &ui_dir)?;
    println!("{}", "UI dependencies installed.".green());
    Ok(())
}

/// Install the packages providing any missing `(command, package)` pairs.
/// Package names are the same on apt, dnf, yum and pacman for everything
/// we need.
fn ensure_tools(tools: &[(&str, &str)]) -> Result<()> {
    let mut packages: Vec<&str> = tools
        .iter()
        .filter(|(cmd, _)| !command_exists(cmd))
        .map(|(_, pkg)| *pkg)
        .collect();
    packages.dedup();
    if packages.is_empty() {
        return Ok(());
    }
    let list = packages.join(" ");
    println!("{} {}", "Installing".yellow(), list);
    if command_exists("apt-get") {
        run_sh(&format!("sudo apt-get update && sudo apt-get install -y {}", list))
    } else if command_exists("dnf") {
        run_sh(&format!("sudo dnf install -y {}", list))
    } else if command_exists("yum") {
        run_sh(&format!("sudo yum install -y {}", list))
    } else if command_exists("pacman") {
        run_sh(&format!("sudo pacman -S --noconfirm {}", list))
    } else {
        bail!("Please install {} manually", list)
    }
}

fn print_usage() {
    section("Quick Start");
    println!();
    println!("1. Start the control plane server:");
    println!("     {}", "glidex-control-plane".green());
    println!();
    println!("2. (Optional) Start the web UI in another terminal:");
    println!("     {}", "cargo run -p glidex-ui".green());
    println!("     Then open http://localhost:5173");
    println!();
    println!("3. Use the interactive CLI:");
    println!("     {}", "gxctl".green());
    println!();
    println!("{}", "Installation complete!".green().bold());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_plane_unit_renders_and_keeps_only_existing_groups() {
        let template = std::fs::read_to_string(workspace_root().join("packaging/glidex-control-plane.service.in")).unwrap();
        let groups = control_plane_groups(|g| g == "kvm");
        assert_eq!(groups, vec!["kvm"], "missing glidex group is left out");
        let unit = render_control_plane_unit(&template, "alice", Path::new("/home/alice"), Path::new("/usr/local/bin/glidex-control-plane"), &groups);
        for line in [
            "User=alice",
            "SupplementaryGroups=kvm",
            "Environment=HOME=/home/alice",
            "WorkingDirectory=/home/alice",
            "ExecStart=/usr/local/bin/glidex-control-plane",
            "After=network-online.target glidex-netd.service",
            "Wants=glidex-netd.service",
        ] {
            assert!(unit.lines().any(|l| l == line), "missing {line:?}");
        }
        assert!(!unit.contains('@'), "all placeholders filled");
    }

    #[test]
    fn netd_unit_is_notify_and_ordered_after_ovs() {
        let unit = std::fs::read_to_string(workspace_root().join("packaging/glidex-netd.service")).unwrap();
        assert!(unit.contains("Type=notify"));
        assert!(unit.lines().any(|l| l.starts_with("After=") && l.contains("openvswitch-switch.service") && l.contains("glidex-ovs-vswitchd.service")));
        assert!(unit.contains("Before=glidex-control-plane.service"));
    }

    #[test]
    fn networking_commands_in_order() {
        let cmds = networking_commands(Some("alice"), Path::new("/src/target/release/glidex-netd"), Path::new("/src/packaging/glidex-netd.service"));
        let lines: Vec<String> = cmds.iter().map(|c| c.join(" ")).collect();
        assert_eq!(
            lines,
            vec![
                "groupadd -f glidex",
                "usermod -aG glidex alice",
                "install -m 0755 -o root -g root /src/target/release/glidex-netd /usr/local/bin/glidex-netd",
                "install -m 0644 -o root -g root /src/packaging/glidex-netd.service /etc/systemd/system/glidex-netd.service",
                "systemctl daemon-reload",
                "systemctl enable glidex-netd.service",
                "systemctl restart glidex-netd.service",
            ]
        );
        let as_root = networking_commands(Some("root"), Path::new("/n"), Path::new("/u"));
        assert!(!as_root.iter().any(|c| c[0] == "usermod"), "root isn't added to the group");
    }

    #[test]
    fn unit_file_preserves_runtime_dir() {
        let unit = std::fs::read_to_string(workspace_root().join("packaging/glidex-netd.service")).unwrap();
        assert!(unit.contains("ExecStart=/usr/local/bin/glidex-netd"));
        assert!(unit.contains("RuntimeDirectory=glidex"));
        assert!(unit.contains("RuntimeDirectoryPreserve=yes"), "vhost-user sockets must survive netd restarts");
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
        std::env::set_var("HOME", home.path());
        let platform = detect_platform().unwrap();
        let (asset, sha256) = firmware_asset(&platform);

        install_uefi_firmware(&platform).unwrap();
        let dest = home.path().join(".glidex").join(asset);
        assert_eq!(file_sha256(&dest).unwrap(), sha256);
        assert!(!home.path().join(".glidex").join(format!("{asset}.part")).exists());

        // Second run sees the verified file and does not re-download.
        let mtime = fs::metadata(&dest).unwrap().modified().unwrap();
        install_uefi_firmware(&platform).unwrap();
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
