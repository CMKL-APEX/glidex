use anyhow::{bail, Context, Result};
use colored::Colorize;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

const CLOUD_HYPERVISOR_VERSION: &str = "v50.0";
const FIRECRACKER_VERSION: &str = "v1.14.0";
/// Release tag of https://github.com/cloud-hypervisor/edk2/releases to fetch
/// the UEFI firmware from. Bump together with the digests in
/// `firmware_asset`.
const EDK2_FIRMWARE_VERSION: &str = "ch-811ce5ea35";

struct Platform {
    os: &'static str,
    arch: &'static str,
}

fn main() -> Result<()> {
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
    install_firecracker(&platform, &install_dir)?;
    install_qemu()?;
    check_kvm()?;
    build_project(&install_dir)?;
    install_ui_deps()?;
    download_samples(&platform)?;
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
            "Cloud-Hypervisor, Firecracker, and QEMU only support Linux (detected: {})",
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
    println!("  5. (Optional) Install Firecracker {}", FIRECRACKER_VERSION);
    println!("  6. (Optional) Install QEMU via system package manager");
    println!("  7. Check KVM access");
    println!("  8. Build the control plane (cargo build --release)");
    println!("  9. Install UI npm dependencies (bun install)");
    println!("  10. (Optional) Download sample kernel and rootfs");
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

fn run_sh_capture(script: &str) -> Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(script)
        .output()
        .with_context(|| format!("Failed to run: {}", script))?;
    if !output.status.success() {
        bail!(
            "Shell command failed: {}: {}",
            script,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

fn install_firecracker(platform: &Platform, install_dir: &Path) -> Result<()> {
    section("Firecracker (Optional)");
    if command_exists("firecracker") {
        let v = run_capture("firecracker", &["--version"]).unwrap_or_default();
        println!(
            "{} {}",
            "Firecracker is installed:".green(),
            v.lines().next().unwrap_or("").trim()
        );
        return Ok(());
    }
    if !confirm_yn("Install Firecracker?", false)? {
        return Ok(());
    }

    let url = format!(
        "https://github.com/firecracker-microvm/firecracker/releases/download/{ver}/firecracker-{ver}-{arch}.tgz",
        ver = FIRECRACKER_VERSION,
        arch = platform.arch
    );
    let tmp = TempDir::new()?;
    let tgz = tmp.path().join("firecracker.tgz");
    println!("Downloading {}", url);
    download(&url, &tgz)?;

    run(
        "tar",
        &[
            "-xzf",
            tgz.to_str().unwrap(),
            "-C",
            tmp.path().to_str().unwrap(),
        ],
    )?;

    let release_dir_name = format!("release-{}-{}", FIRECRACKER_VERSION, platform.arch);
    let release_dir = tmp.path().join(&release_dir_name);
    if !release_dir.is_dir() {
        bail!("Firecracker tarball missing {}", release_dir_name);
    }

    let fc_src = release_dir.join(format!(
        "firecracker-{}-{}",
        FIRECRACKER_VERSION, platform.arch
    ));
    install_binary(&fc_src, &install_dir.join("firecracker"))?;
    println!("{} firecracker", "Installed:".green());

    let jailer_src = release_dir.join(format!(
        "jailer-{}-{}",
        FIRECRACKER_VERSION, platform.arch
    ));
    if jailer_src.is_file() {
        install_binary(&jailer_src, &install_dir.join("jailer"))?;
        println!("{} jailer", "Installed:".green());
    }
    Ok(())
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
        let user = env::var("USER").unwrap_or_default();
        println!(
            "{}",
            "/dev/kvm exists but is not writable by your user.".yellow()
        );
        println!("  Run: sudo usermod -aG kvm {}", user);
        println!("  Then log out and back in.");
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
        &["build", "--release", "-p", "glidex-control-plane"],
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

fn download_samples(platform: &Platform) -> Result<()> {
    section("Sample Kernel and RootFS");
    if !confirm_yn("Download sample kernel and rootfs?", false)? {
        return Ok(());
    }

    let sample_dir = dirs::home_dir()
        .context("No home directory")?
        .join(".glidex");
    fs::create_dir_all(&sample_dir)?;

    ensure_squashfs_tools()?;

    // The Firecracker CI S3 bucket is keyed by major.minor — derive that from
    // the redirect URL of the "latest" GitHub release.
    println!("Discovering latest Firecracker CI version...");
    let redirect_url = run_sh_capture(
        "curl -fsSLI -o /dev/null -w %{url_effective} https://github.com/firecracker-microvm/firecracker/releases/latest",
    )?;
    let latest_tag = redirect_url
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string();
    // e.g. "v1.14.0" -> "v1.14"
    let ci_version = latest_tag
        .rsplitn(2, '.')
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Could not derive CI version from tag {}", latest_tag))?
        .to_string();
    println!("Using Firecracker CI version: {}", ci_version);

    let kernel_path = sample_dir.join("vmlinux.bin");
    if !kernel_path.exists() {
        let prefix = format!("firecracker-ci/{}/{}/vmlinux-", ci_version, platform.arch);
        let listing = run_sh_capture(&format!(
            "curl -fsSL 'http://spec.ccfc.min.s3.amazonaws.com/?prefix={}&list-type=2'",
            prefix
        ))?;
        let kernel_key =
            pick_latest_key_matching(&listing, &prefix, |k| !k.ends_with(".config"))
                .context("Could not find kernel image in S3 listing")?;
        let url = format!("https://s3.amazonaws.com/spec.ccfc.min/{}", kernel_key);
        println!("Downloading kernel: {}", kernel_key);
        download(&url, &kernel_path)?;
    } else {
        println!("Kernel already exists: {}", kernel_path.display());
    }

    let rootfs_path = sample_dir.join("rootfs.ext4");
    if !rootfs_path.exists() {
        let prefix = format!("firecracker-ci/{}/{}/ubuntu-", ci_version, platform.arch);
        let listing = run_sh_capture(&format!(
            "curl -fsSL 'http://spec.ccfc.min.s3.amazonaws.com/?prefix={}&list-type=2'",
            prefix
        ))?;
        let ubuntu_key = pick_latest_key_matching(&listing, &prefix, |k| k.ends_with(".squashfs"))
            .context("Could not find Ubuntu squashfs in S3 listing")?;
        let url = format!("https://s3.amazonaws.com/spec.ccfc.min/{}", ubuntu_key);

        let tmp = TempDir::new()?;
        let squashfs = tmp.path().join("ubuntu.squashfs");
        println!("Downloading rootfs: {}", ubuntu_key);
        download(&url, &squashfs)?;

        println!("Extracting squashfs...");
        let squashfs_root = tmp.path().join("squashfs-root");
        run(
            "sudo",
            &[
                "unsquashfs",
                "-d",
                squashfs_root.to_str().unwrap(),
                squashfs.to_str().unwrap(),
            ],
        )?;

        let ssh_key = sample_dir.join("vm_key");
        if !ssh_key.exists() {
            run(
                "ssh-keygen",
                &["-f", ssh_key.to_str().unwrap(), "-N", "", "-q"],
            )?;
            println!("{} {}", "Generated SSH key:".green(), ssh_key.display());
        }

        let ssh_dir = squashfs_root.join("root/.ssh");
        run("sudo", &["mkdir", "-p", ssh_dir.to_str().unwrap()])?;
        let authorized_keys = ssh_dir.join("authorized_keys");
        run(
            "sudo",
            &[
                "cp",
                sample_dir.join("vm_key.pub").to_str().unwrap(),
                authorized_keys.to_str().unwrap(),
            ],
        )?;
        run(
            "sudo",
            &["chmod", "600", authorized_keys.to_str().unwrap()],
        )?;

        run(
            "truncate",
            &["-s", "1G", rootfs_path.to_str().unwrap()],
        )?;
        run(
            "sudo",
            &[
                "mkfs.ext4",
                "-d",
                squashfs_root.to_str().unwrap(),
                "-F",
                rootfs_path.to_str().unwrap(),
            ],
        )?;
        run(
            "sudo",
            &[
                "rm",
                "-rf",
                squashfs_root.to_str().unwrap(),
                squashfs.to_str().unwrap(),
            ],
        )?;
        println!("{} {}", "Built rootfs:".green(), rootfs_path.display());
    } else {
        println!("RootFS already exists: {}", rootfs_path.display());
    }

    println!();
    println!("{}", "Sample files:".cyan().bold());
    println!("  Kernel:  {}", sample_dir.join("vmlinux.bin").display());
    println!("  RootFS:  {}", sample_dir.join("rootfs.ext4").display());
    let ssh_key = sample_dir.join("vm_key");
    if ssh_key.exists() {
        println!("  SSH Key: {}", ssh_key.display());
        println!(
            "{} ssh -i {} root@<vm-ip>",
            "Connect via:".yellow(),
            ssh_key.display()
        );
    }
    Ok(())
}

fn ensure_squashfs_tools() -> Result<()> {
    ensure_tools(&[("unsquashfs", "squashfs-tools")])
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

/// Pull all `<Key>…</Key>` entries from an S3 XML listing that start with
/// `prefix` and pass `filter`, then return the one with the highest numeric
/// version.
fn pick_latest_key_matching(
    xml: &str,
    prefix: &str,
    filter: impl Fn(&str) -> bool,
) -> Option<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Key>") {
        rest = &rest[start + 5..];
        if let Some(end) = rest.find("</Key>") {
            let key = &rest[..end];
            if key.starts_with(prefix) && filter(key) {
                keys.push(key.to_string());
            }
            rest = &rest[end + 6..];
        } else {
            break;
        }
    }
    keys.sort_by(|a, b| version_cmp(a, b));
    keys.pop()
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let digits = |s: &str| -> Vec<u64> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter_map(|p| p.parse::<u64>().ok())
            .collect()
    };
    digits(a).cmp(&digits(b))
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
