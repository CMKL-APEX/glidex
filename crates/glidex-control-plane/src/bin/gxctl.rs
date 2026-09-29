use clap::Parser;
use colored::Colorize;
use glidex_control_plane::hypervisor::cloud_hypervisor::default_firmware_path;
use nix::sys::termios::{self, LocalFlags, SetArg, Termios};
use reqwest::Client;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tabled::{Table, Tabled};

#[derive(Parser)]
#[command(name = "gxctl")]
#[command(about = "Interactive CLI for Glidex Control Plane")]
struct Cli {
    /// API server URL
    #[arg(short, long, default_value = "http://localhost:8080")]
    server: String,
}

#[derive(Debug, Deserialize, Tabled)]
struct VmResponse {
    id: String,
    name: String,
    state: String,
    vcpu_count: u8,
    mem_size_mib: u32,
    hypervisor: String,
    #[tabled(skip)]
    #[serde(default)]
    vfio_devices: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CreateVmRequest {
    name: String,
    vcpu_count: u8,
    mem_size_mib: u32,
    kernel_image_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    firmware_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cloud_init_path: Option<String>,
    rootfs_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    kernel_args: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hypervisor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vfio_devices: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    error: String,
    message: String,
}

#[derive(Debug, Deserialize, Tabled)]
struct PciDeviceInfo {
    address: String,
    vendor_id: String,
    device_id: String,
    class_id: String,
    #[tabled(display_with = "display_option")]
    driver: Option<String>,
    #[tabled(display_with = "display_option")]
    iommu_group: Option<String>,
    #[tabled(skip)]
    sysfs_path: String,
}

fn display_option(o: &Option<String>) -> String {
    match o {
        Some(s) => s.clone(),
        None => "-".to_string(),
    }
}

#[derive(Debug, Deserialize)]
struct ConsoleInfo {
    #[allow(dead_code)]
    vm_id: String,
    console_socket_path: String,
    log_path: String,
    available: bool,
}

struct CliClient {
    client: Client,
    base_url: String,
}

impl CliClient {
    fn new(base_url: String) -> Self {
        Self {
            client: Client::new(),
            base_url,
        }
    }

    async fn list_vms(&self) -> Result<Vec<VmResponse>, String> {
        let resp = self
            .client
            .get(format!("{}/vms", self.base_url))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn get_vm(&self, id: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .get(format!("{}/vms/{}", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn create_vm(&self, request: CreateVmRequest) -> Result<VmResponse, String> {
        let resp = self
            .client
            .post(format!("{}/vms", self.base_url))
            .json(&request)
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn start_vm(&self, id: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .post(format!("{}/vms/{}/start", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn stop_vm(&self, id: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .post(format!("{}/vms/{}/stop", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn pause_vm(&self, id: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .post(format!("{}/vms/{}/pause", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn delete_vm(&self, id: &str) -> Result<(), String> {
        let resp = self
            .client
            .delete(format!("{}/vms/{}", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn health_check(&self) -> Result<(), String> {
        let resp = self
            .client
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err("Health check failed".to_string())
        }
    }

    async fn get_console_info(&self, id: &str) -> Result<ConsoleInfo, String> {
        let resp = self
            .client
            .get(format!("{}/vms/{}/console", self.base_url, id))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn list_pci_devices(&self) -> Result<Vec<PciDeviceInfo>, String> {
        let resp = self
            .client
            .get(format!("{}/pci-devices", self.base_url))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            Err("Failed to list PCI devices".to_string())
        }
    }

    async fn attach_device(&self, vm_id: &str, device_path: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .post(format!("{}/vms/{}/devices", self.base_url, vm_id))
            .json(&serde_json::json!({ "device_path": device_path }))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    async fn detach_device(&self, vm_id: &str, device_path: &str) -> Result<VmResponse, String> {
        let resp = self
            .client
            .delete(format!("{}/vms/{}/devices", self.base_url, vm_id))
            .json(&serde_json::json!({ "device_path": device_path }))
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if resp.status().is_success() {
            resp.json()
                .await
                .map_err(|e| format!("Failed to parse response: {}", e))
        } else {
            let error: ApiError = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse error: {}", e))?;
            Err(format!("{}: {}", error.error, error.message))
        }
    }

    /// Resolve a VM identifier (name or ID) to an ID.
    /// First tries to use it as an ID, then searches by name.
    async fn resolve_vm(&self, name_or_id: &str) -> Result<String, String> {
        // First, try to get VM by ID directly
        if let Ok(vm) = self.get_vm(name_or_id).await {
            return Ok(vm.id);
        }

        // If that fails, search by name
        let vms = self.list_vms().await?;
        let matches: Vec<_> = vms.iter().filter(|vm| vm.name == name_or_id).collect();

        match matches.len() {
            0 => Err(format!("VM '{}' not found", name_or_id)),
            1 => Ok(matches[0].id.clone()),
            _ => {
                let ids: Vec<_> = matches.iter().map(|vm| vm.id.as_str()).collect();
                Err(format!(
                    "Multiple VMs found with name '{}'. Use ID instead: {}",
                    name_or_id,
                    ids.join(", ")
                ))
            }
        }
    }
}

fn print_help() {
    println!("{}", "Available commands:".bold());
    println!("  {}              - List all VMs", "list".cyan());
    println!("  {}    - Show VM details", "get <name|id>".cyan());
    println!(
        "  {}           - Create a new VM (interactive)",
        "create".cyan()
    );
    println!("  {}  - Start a VM", "start <name|id>".cyan());
    println!("  {}   - Stop a VM", "stop <name|id>".cyan());
    println!("  {}  - Pause a VM", "pause <name|id>".cyan());
    println!("  {} - Connect to VM console (interactive)", "connect <name|id>".cyan());
    println!("  {}     - Show VM serial console log", "log <name|id>".cyan());
    println!("  {} - Delete a VM", "delete <name|id>".cyan());
    println!("  {}               - List host PCI devices", "pci".cyan());
    println!(
        "  {}     - Show detailed info (incl. sysfs path) for one device",
        "pci <address>".cyan()
    );
    println!(
        "  {} - Attach PCI device to VM",
        "attach-device <name|id> <path>".cyan()
    );
    println!(
        "  {} - Detach PCI device from VM",
        "detach-device <name|id> <path>".cyan()
    );
    println!("  {}            - Check API server health", "health".cyan());
    println!("  {}              - Show this help", "help".cyan());
    println!("  {}              - Exit the CLI", "exit".cyan());
    println!();
    println!(
        "{} You can use either VM name or ID for commands.",
        "Note:".dimmed()
    );
}

fn prompt(msg: &str) -> String {
    print!("{}", msg);
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    input.trim().to_string()
}

fn prompt_optional(msg: &str) -> Option<String> {
    let input = prompt(msg);
    if input.is_empty() {
        None
    } else {
        Some(input)
    }
}

async fn handle_create(client: &CliClient) {
    println!("{}", "Create new VM".bold());
    println!("{}", "-".repeat(40));

    let name = prompt("VM name: ");
    if name.is_empty() {
        println!("{}", "Error: VM name is required".red());
        return;
    }

    let vcpu_count: u8 = match prompt("vCPU count [1]: ").parse() {
        Ok(n) => n,
        Err(_) => 1,
    };

    let mem_size_mib: u32 = match prompt("Memory (MiB) [512]: ").parse() {
        Ok(n) => n,
        Err(_) => 512,
    };

    let hypervisor = match prompt("Hypervisor [cloudhypervisor/qemu] (default: cloudhypervisor): ")
        .to_lowercase()
        .as_str()
    {
        "" | "cloudhypervisor" | "cloud-hypervisor" | "ch" => Some("cloudhypervisor".to_string()),
        "qemu" | "q" => Some("qemu".to_string()),
        other => {
            println!(
                "{} Unknown hypervisor '{}', using cloudhypervisor",
                "Warning:".yellow(),
                other
            );
            Some("cloudhypervisor".to_string())
        }
    };

    // Cloud Hypervisor boots a disk image through UEFI firmware by default,
    // using the CLOUDHV.fd that glidex-install downloads into ~/.glidex.
    let firmware_path = if hypervisor.as_deref() == Some("cloudhypervisor") {
        let default_firmware = default_firmware_path()
            .filter(|p| p.exists())
            .map(|p| p.to_string_lossy().into_owned());
        let hint = default_firmware.as_deref().unwrap_or("none");
        match prompt(&format!(
            "UEFI firmware path ['none' for kernel boot] [{}]: ",
            hint
        ))
        .as_str()
        {
            "" => default_firmware,
            "none" => None,
            s => Some(s.to_string()),
        }
    } else {
        None
    };

    let kernel_image_path = if firmware_path.is_some() {
        String::new()
    } else {
        let path = prompt("Kernel image path: ");
        if path.is_empty() {
            println!("{}", "Error: kernel image path is required for kernel boot".red());
            return;
        }
        path
    };

    // Firmware boot needs a disk with its own bootloader (e.g. a raw distro
    // cloud image); kernel boot takes a bare root filesystem image.
    let rootfs_prompt = if firmware_path.is_some() {
        "Disk image path (UEFI-bootable, e.g. a raw cloud image): "
    } else {
        "Root filesystem path: "
    };
    let rootfs_path = prompt(rootfs_prompt);
    if rootfs_path.is_empty() {
        println!("{}", "Error: disk image path is required".red());
        return;
    }

    let cloud_init_path = if firmware_path.is_some() {
        prompt_optional("cloud-init seed image (optional, default: auto-generated): ")
    } else {
        None
    };

    let kernel_args = if firmware_path.is_some() {
        None
    } else {
        prompt_optional("Kernel arguments (optional, default: root=/dev/vda reboot=k panic=1): ")
    };

    let vfio_devices =
        prompt_optional("VFIO PCI devices (comma-separated, e.g. /sys/bus/pci/devices/0000:41:00.0): ")
            .map(|s| {
                s.split(',')
                    .map(|d| d.trim().to_string())
                    .filter(|d| !d.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

    let request = CreateVmRequest {
        name,
        vcpu_count,
        mem_size_mib,
        kernel_image_path,
        firmware_path,
        cloud_init_path,
        rootfs_path,
        kernel_args,
        hypervisor,
        vfio_devices,
    };

    match client.create_vm(request).await {
        Ok(vm) => {
            println!("{}", "VM created successfully!".green());
            println!("  ID: {}", vm.id.yellow());
            println!("  Name: {}", vm.name);
            println!("  State: {}", vm.state);
            println!("  Hypervisor: {}", vm.hypervisor);
        }
        Err(e) => println!("{} {}", "Error:".red(), e),
    }
}

fn format_state(state: &str) -> String {
    match state {
        "running" => state.green().to_string(),
        "stopped" => state.red().to_string(),
        "paused" => state.yellow().to_string(),
        "created" => state.blue().to_string(),
        _ => state.to_string(),
    }
}

/// Set terminal to raw mode for interactive console
fn set_raw_mode(fd: BorrowedFd<'_>) -> Option<Termios> {
    let orig_termios = termios::tcgetattr(fd).ok()?;
    let mut raw = orig_termios.clone();

    // Disable canonical mode and echo
    raw.local_flags.remove(LocalFlags::ICANON);
    raw.local_flags.remove(LocalFlags::ECHO);
    raw.local_flags.remove(LocalFlags::ISIG);

    termios::tcsetattr(fd, SetArg::TCSANOW, &raw).ok()?;
    Some(orig_termios)
}

/// Restore terminal to original mode
fn restore_terminal(fd: BorrowedFd<'_>, termios: &Termios) {
    let _ = termios::tcsetattr(fd, SetArg::TCSANOW, termios);
}

async fn handle_log(client: &CliClient, vm_id: &str) {
    // Get console info from API
    let console_info = match client.get_console_info(vm_id).await {
        Ok(info) => info,
        Err(e) => {
            println!("{} {}", "Error:".red(), e);
            return;
        }
    };

    let log_path = &console_info.log_path;

    // Try to open and read the log file
    match File::open(log_path) {
        Ok(file) => {
            let reader = BufReader::new(file);
            let mut has_content = false;

            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        println!("{}", l);
                        has_content = true;
                    }
                    Err(e) => {
                        println!("{} Error reading log: {}", "Error:".red(), e);
                        return;
                    }
                }
            }

            if !has_content {
                println!("{} Log file is empty. Start the VM to see console output.", "Info:".yellow());
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            println!("{} Log file not found. Start the VM first.", "Info:".yellow());
        }
        Err(e) => {
            println!("{} Failed to open log file: {}", "Error:".red(), e);
        }
    }
}

async fn handle_connect(client: &CliClient, vm_id: &str) {
    // Get console info from API
    let console_info = match client.get_console_info(vm_id).await {
        Ok(info) => info,
        Err(e) => {
            println!("{} {}", "Error:".red(), e);
            return;
        }
    };

    if !console_info.available {
        println!(
            "{} VM is not running. Start the VM first with: start {}",
            "Error:".red(),
            vm_id
        );
        return;
    }

    let socket_path = &console_info.console_socket_path;

    println!(
        "{} Connecting to VM console via {}",
        "Info:".cyan(),
        socket_path
    );
    println!(
        "{} Press {} to detach from console\n",
        "Tip:".yellow(),
        "Ctrl+]".bold()
    );

    // Connect to the console Unix socket
    let stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            println!(
                "{} Failed to connect to console socket {}: {}",
                "Error:".red(),
                socket_path,
                e
            );
            return;
        }
    };

    // Set socket to non-blocking for the reader
    stream.set_nonblocking(true).ok();
    let stream_write = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            println!("{} Failed to clone socket: {}", "Error:".red(), e);
            return;
        }
    };

    // Set up signal handler for Ctrl+C (we'll handle Ctrl+] for detach)
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Save original terminal settings and set raw mode
    let stdin = io::stdin();
    let stdin_fd = stdin.as_fd();
    let orig_termios = match set_raw_mode(stdin_fd) {
        Some(t) => t,
        None => {
            println!("{} Failed to set terminal to raw mode", "Error:".red());
            return;
        }
    };

    // Spawn thread to read from socket and write to stdout
    let running_reader = running.clone();
    let reader_handle = thread::spawn(move || {
        let mut stream = stream;
        let mut buf = [0u8; 1024];

        while running_reader.load(Ordering::SeqCst) {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = io::stdout().write_all(&buf[..n]);
                    let _ = io::stdout().flush();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    // Spawn thread to read from stdin and write to socket
    let running_writer = running.clone();
    let writer_handle = thread::spawn(move || {
        let mut stream = stream_write;
        let mut buf = [0u8; 1];

        while running_writer.load(Ordering::SeqCst) {
            match io::stdin().read(&mut buf) {
                Ok(0) => break,
                Ok(1) => {
                    // Check for Ctrl+] (0x1d) to detach
                    if buf[0] == 0x1d {
                        running_writer.store(false, Ordering::SeqCst);
                        break;
                    }
                    let _ = stream.write_all(&buf);
                    let _ = stream.flush();
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    // Handle Ctrl+C gracefully
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .ok();

    // Wait for threads to finish
    let _ = writer_handle.join();
    running.store(false, Ordering::SeqCst);
    let _ = reader_handle.join();

    // Restore terminal
    restore_terminal(stdin.as_fd(), &orig_termios);

    println!("\n{} Detached from console", "Info:".cyan());
}

async fn handle_command(line: &str, client: &CliClient) -> bool {
    let parts: Vec<&str> = line.trim().split_whitespace().collect();
    if parts.is_empty() {
        return true;
    }

    match parts[0] {
        "help" | "?" => print_help(),

        "exit" | "quit" | "q" => return false,

        "list" | "ls" => match client.list_vms().await {
            Ok(vms) => {
                if vms.is_empty() {
                    println!("{}", "No VMs found".yellow());
                } else {
                    let table = Table::new(&vms).to_string();
                    println!("{}", table);
                }
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },

        "get" => {
            if parts.len() < 2 {
                println!("{}", "Usage: get <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.get_vm(&vm_id).await {
                Ok(vm) => {
                    println!("{}", "VM Details".bold());
                    println!("{}", "-".repeat(40));
                    println!("  ID:         {}", vm.id.yellow());
                    println!("  Name:       {}", vm.name);
                    println!("  State:      {}", format_state(&vm.state));
                    println!("  Hypervisor: {}", vm.hypervisor);
                    println!("  vCPUs:      {}", vm.vcpu_count);
                    println!("  Memory:     {} MiB", vm.mem_size_mib);
                    if !vm.vfio_devices.is_empty() {
                        println!("  VFIO:       {}", vm.vfio_devices.join(", "));
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "create" => handle_create(client).await,

        "start" => {
            if parts.len() < 2 {
                println!("{}", "Usage: start <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.start_vm(&vm_id).await {
                Ok(vm) => {
                    println!(
                        "{} VM {} is now {}",
                        "Success:".green(),
                        vm.name,
                        format_state(&vm.state)
                    );
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "stop" => {
            if parts.len() < 2 {
                println!("{}", "Usage: stop <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.stop_vm(&vm_id).await {
                Ok(vm) => {
                    println!(
                        "{} VM {} is now {}",
                        "Success:".green(),
                        vm.name,
                        format_state(&vm.state)
                    );
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "pause" => {
            if parts.len() < 2 {
                println!("{}", "Usage: pause <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.pause_vm(&vm_id).await {
                Ok(vm) => {
                    println!(
                        "{} VM {} is now {}",
                        "Success:".green(),
                        vm.name,
                        format_state(&vm.state)
                    );
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "connect" | "console" | "attach" => {
            if parts.len() < 2 {
                println!("{}", "Usage: connect <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            handle_connect(client, &vm_id).await;
        }

        "log" | "logs" => {
            if parts.len() < 2 {
                println!("{}", "Usage: log <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            handle_log(client, &vm_id).await;
        }

        "delete" | "rm" => {
            if parts.len() < 2 {
                println!("{}", "Usage: delete <name|id>".yellow());
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            let confirm = prompt(&format!(
                "Are you sure you want to delete VM {}? [y/N]: ",
                parts[1]
            ));
            if confirm.to_lowercase() == "y" {
                match client.delete_vm(&vm_id).await {
                    Ok(()) => println!("{} VM deleted", "Success:".green()),
                    Err(e) => println!("{} {}", "Error:".red(), e),
                }
            } else {
                println!("Cancelled");
            }
        }

        "pci" | "pci-devices" => match client.list_pci_devices().await {
            Ok(devices) => {
                if devices.is_empty() {
                    println!("{}", "No PCI devices found".yellow());
                } else if parts.len() >= 2 {
                    // `pci <address>` shows full details for a single device,
                    // including the sysfs path needed for VFIO attachment.
                    match devices.iter().find(|d| d.address == parts[1]) {
                        Some(dev) => {
                            println!("Address:    {}", dev.address);
                            println!("Vendor:     {}", dev.vendor_id);
                            println!("Device:     {}", dev.device_id);
                            println!("Class:      {}", dev.class_id);
                            println!("Driver:     {}", display_option(&dev.driver));
                            println!("IOMMU:      {}", display_option(&dev.iommu_group));
                            println!("Sysfs path: {}", dev.sysfs_path);
                        }
                        None => println!(
                            "{} no PCI device with address {}",
                            "Error:".red(),
                            parts[1]
                        ),
                    }
                } else {
                    let table = Table::new(&devices).to_string();
                    println!("{}", table);
                }
            }
            Err(e) => println!("{} {}", "Error:".red(), e),
        },

        "attach-device" => {
            if parts.len() < 3 {
                println!(
                    "{}",
                    "Usage: attach-device <name|id> <device-path>".yellow()
                );
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.attach_device(&vm_id, parts[2]).await {
                Ok(vm) => {
                    println!(
                        "{} Device {} attached to VM {}",
                        "Success:".green(),
                        parts[2],
                        vm.name
                    );
                    if !vm.vfio_devices.is_empty() {
                        println!("  VFIO devices: {}", vm.vfio_devices.join(", "));
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "detach-device" => {
            if parts.len() < 3 {
                println!(
                    "{}",
                    "Usage: detach-device <name|id> <device-path>".yellow()
                );
                return true;
            }
            let vm_id = match client.resolve_vm(parts[1]).await {
                Ok(id) => id,
                Err(e) => {
                    println!("{} {}", "Error:".red(), e);
                    return true;
                }
            };
            match client.detach_device(&vm_id, parts[2]).await {
                Ok(vm) => {
                    println!(
                        "{} Device {} detached from VM {}",
                        "Success:".green(),
                        parts[2],
                        vm.name
                    );
                    if !vm.vfio_devices.is_empty() {
                        println!("  VFIO devices: {}", vm.vfio_devices.join(", "));
                    }
                }
                Err(e) => println!("{} {}", "Error:".red(), e),
            }
        }

        "health" => match client.health_check().await {
            Ok(()) => println!("{} API server is healthy", "OK:".green()),
            Err(e) => println!("{} {}", "Error:".red(), e),
        },

        _ => println!(
            "{} Unknown command: {}. Type 'help' for available commands.",
            "Error:".red(),
            parts[0]
        ),
    }

    true
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let client = CliClient::new(cli.server.clone());

    println!(
        "{}",
        r#"
   _____ _ _     _
  / ____| (_)   | |
 | |  __| |_  __| | _____  __
 | | |_ | | |/ _` |/ _ \ \/ /
 | |__| | | | (_| |  __/>  <
  \_____|_|_|\__,_|\___/_/\_\
          Control Plane CLI
"#
        .cyan()
    );

    println!("Connected to: {}", cli.server.yellow());
    println!("Type {} for available commands\n", "help".cyan());

    let mut rl = DefaultEditor::new().expect("Failed to initialize readline");

    loop {
        match rl.readline("gxctl> ") {
            Ok(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&line);
                if !handle_command(&line, &client).await {
                    println!("Goodbye!");
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => {
                println!("Use 'exit' to quit");
            }
            Err(ReadlineError::Eof) => {
                println!("Goodbye!");
                break;
            }
            Err(err) => {
                println!("Error: {:?}", err);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_device_info_deserializes_sysfs_path() {
        let json = r#"{
            "address": "0000:00:1f.0",
            "vendor_id": "8086",
            "device_id": "9d4e",
            "class_id": "060100",
            "driver": "lpc_ich",
            "iommu_group": "5",
            "sysfs_path": "/sys/bus/pci/devices/0000:00:1f.0"
        }"#;

        let info: PciDeviceInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.sysfs_path, "/sys/bus/pci/devices/0000:00:1f.0");
        assert_eq!(info.address, "0000:00:1f.0");
        assert_eq!(info.driver.as_deref(), Some("lpc_ich"));
        assert_eq!(info.iommu_group.as_deref(), Some("5"));
    }

    #[test]
    fn display_option_renders_dash_for_none() {
        assert_eq!(display_option(&None), "-");
        assert_eq!(display_option(&Some("vfio-pci".to_string())), "vfio-pci");
    }
}
