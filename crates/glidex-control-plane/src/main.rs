use glidex_control_plane::{api, auth, config, hypervisor, images, network, serve, state};

use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use tokio::signal;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};


const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_status(msg: &str) {
    print!("  {}... ", msg);
    let _ = io::stdout().flush();
}

fn print_banner() {
    let db_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".glidex")
        .join("glidex.db");

    println!();
    println!("  ╔═══════════════════════════════════════════╗");
    println!("  ║            GlideX Control Plane           ║");
    println!("  ╚═══════════════════════════════════════════╝");
    println!();
    println!("  Version:   {}", VERSION);
    println!("  Database:  {}", db_path.display());
    println!();
}

fn check_kvm_access() -> Result<(), String> {
    let kvm_path = Path::new("/dev/kvm");

    if !kvm_path.exists() {
        return Err(
            "/dev/kvm not found. KVM may not be enabled.\n\
             To enable KVM:\n  \
             1. Check if your CPU supports virtualization (Intel VT-x or AMD-V)\n  \
             2. Enable virtualization in BIOS/UEFI\n  \
             3. Load the KVM module: sudo modprobe kvm_intel (or kvm_amd)"
                .to_string(),
        );
    }

    // Check read/write access
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(kvm_path)
    {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(
            "/dev/kvm exists but you don't have permission to access it.\n\
             To fix this, add your user to the kvm group:\n  \
             sudo usermod -aG kvm $USER\n\
             Then log out and log back in for the change to take effect."
                .to_string(),
        ),
        Err(e) => Err(format!("Failed to access /dev/kvm: {}", e)),
    }
}

#[tokio::main]
async fn main() {
    // Print startup banner
    print_banner();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "glidex_control_plane=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Check KVM access before starting
    print_status("Checking KVM access");
    if let Err(e) = check_kvm_access() {
        println!("FAILED");
        eprintln!("\n{}", e);
        std::process::exit(1);
    }
    println!("OK");

    // Not fatal: kernel-boot VMs don't need it.
    for (ty, label, fix) in [
        (
            hypervisor::HypervisorType::CloudHypervisor,
            "Cloud-Hypervisor",
            "run glidex-install for Cloud-Hypervisor firmware boot",
        ),
        (
            hypervisor::HypervisorType::Qemu,
            "QEMU",
            "install the ovmf / edk2-ovmf package for QEMU firmware boot",
        ),
    ] {
        print_status(&format!("Checking UEFI firmware ({})", label));
        match ty.default_firmware_path() {
            Some(path) if path.exists() => println!("OK ({})", path.display()),
            Some(path) => println!("MISSING ({}; {})", path.display(), fix),
            None => println!("MISSING (no home directory)"),
        }
    }

    // Not fatal either: only image/disk operations need these.
    print_status("Checking disk tools");
    let missing: Vec<&str> = images::qemu_img::ALL_TOOLS
        .iter()
        .filter(|t| !t.available())
        .map(|t| t.name)
        .collect();
    if missing.is_empty() {
        println!("OK");
    } else {
        println!(
            "MISSING ({}; image and disk operations that need them will fail; run glidex-install)",
            missing.join(", ")
        );
    }

    // Configuration and identity (spec/security.md §5, §13). A bad config
    // file stops startup rather than falling back to defaults.
    print_status("Loading configuration");
    let cfg = match config::Config::load().and_then(|c| c.check().map(|_| c)) {
        Ok(c) => c,
        Err(e) => {
            println!("FAILED");
            eprintln!("\n{}", e);
            std::process::exit(1);
        }
    };
    println!("OK");

    // Create VM manager with persistence
    print_status("Opening database");
    let vm_manager = match state::VmManager::new() {
        Ok(manager) => manager,
        Err(e) => {
            println!("FAILED");
            eprintln!("\nFailed to initialize VM manager: {}", e);
            std::process::exit(1);
        }
    };
    println!("OK");
    vm_manager.configure(&cfg);

    // Load VMs and adopt the instances still running (spec/reconciliation.md
    // §9.4): nothing is launched or stopped before every VM was observed.
    print_status("Loading VMs");
    if let Err(e) = vm_manager.initialize().await {
        println!("FAILED");
        eprintln!("\nFailed to initialize VMs from database: {}", e);
        std::process::exit(1);
    }
    let vms = vm_manager.list_vms().await;
    let running = vms.iter().filter(|v| v.status.instance.is_some()).count();
    println!("OK ({} VMs, {} running, VM runner: {})", vms.len(), running, vm_manager.runner().kind_name());

    // Networking is optional: without glidex-netd, VMs just have no NICs.
    print_status("Checking networking");
    match vm_manager.ensure_default_network().await {
        Ok(Some(net)) => println!("OK (created network '{}' on {})", net.name, net.bridge),
        Ok(None) => match vm_manager.netd().probe() {
            (network::NetdAccess::Full, Ok(_)) => println!("OK"),
            (network::NetdAccess::Status, Ok(_)) => {
                println!("LIMITED (not in the glidex group; status only)")
            }
            (_, Err(e)) => println!("UNAVAILABLE ({})", e),
            (network::NetdAccess::None, Ok(_)) => println!("UNAVAILABLE"),
        },
        Err(e) => println!("WARNING (default network: {})", e),
    }

    // The control loops (§9): from here on the VM controller drives every
    // VM toward its desired state.
    vm_manager.start_controllers();

    print_status("Loading authorization policies");
    let auth = match auth::AuthService::new(vm_manager.database(), cfg.clone()) {
        Ok(a) => a,
        Err(e) => {
            println!("FAILED");
            eprintln!("\n{}", e);
            std::process::exit(1);
        }
    };
    if let Err(e) = vm_manager.projects().adopt_default_quotas(&cfg.quotas.default) {
        println!("WARNING (default project quotas: {})", e);
    }
    match auth.bootstrap(&vm_manager.default_project_id()) {
        Ok(made) if !made.is_empty() => println!("OK (administrators: {})", made.join(", ")),
        Ok(_) => println!("OK"),
        Err(e) => println!("WARNING (bootstrap: {})", e),
    }

    let app = api::router(Arc::new(api::App { manager: vm_manager, auth }))
        .layer(TraceLayer::new_for_http());

    let tls = match &cfg.tls {
        Some(t) => match serve::tls_acceptor(&t.cert, t.key.as_deref()) {
            Ok(a) => Some(a),
            Err(e) => {
                eprintln!("TLS: {}", e);
                std::process::exit(1);
            }
        },
        None => None,
    };
    let mut tcp = Vec::new();
    for addr in &cfg.listen {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => tcp.push(l),
            Err(e) => eprintln!("  Warning: cannot listen on {}: {}", addr, e),
        }
    }
    let api_sock = cfg.api_socket_path();
    let ui_sock = cfg.ui_socket_path();
    let unix: Vec<(tokio::net::UnixListener, api::Listener, std::path::PathBuf)> = [
        (api_sock, api::Listener::Api),
        (ui_sock, api::Listener::Ui),
    ]
    .into_iter()
    .filter_map(|(path, kind)| match serve::bind_unix(&path) {
        Ok(l) => Some((l, kind, path)),
        Err(e) => {
            eprintln!("  Warning: cannot listen on {}: {}", path.display(), e);
            None
        }
    })
    .collect();
    if tcp.is_empty() && unix.is_empty() {
        eprintln!("No usable listen address");
        std::process::exit(1);
    }
    println!();
    let scheme = if tls.is_some() { "https" } else { "http" };
    for l in &tcp {
        println!("  Listening on {}://{}", scheme, l.local_addr().unwrap());
    }
    for (_, kind, path) in &unix {
        println!("  Listening on {} ({:?})", path.display(), kind);
    }
    println!("  Press Ctrl+C to shutdown");
    println!();

    // SIGHUP's default action would end the process, and systemd counts
    // that as a clean exit (no restart). A hangup only ever comes from a
    // terminal going away, never a request to stop, so log and carry on.
    #[cfg(unix)]
    {
        let mut hangup = signal::unix::signal(signal::unix::SignalKind::hangup())
            .expect("Failed to install SIGHUP handler");
        tokio::spawn(async move {
            while hangup.recv().await.is_some() {
                tracing::warn!("SIGHUP received; ignoring (use SIGTERM to stop)");
            }
        });
    }

    // One shutdown signal fans out to every listener. VMs keep running:
    // they belong to their glidex-vm-shim, not to us (D1).
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let servers: Vec<_> = tcp
        .into_iter()
        .map(|l| tokio::spawn(serve::serve_tcp(l, app.clone(), tls.clone(), stop_rx.clone())))
        .chain(unix.into_iter().map(|(l, kind, _)| tokio::spawn(serve::serve_unix(l, app.clone(), kind, stop_rx.clone()))))
        .collect();
    shutdown_signal().await;
    let _ = stop_tx.send(true);
    for s in servers {
        let _ = s.await;
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    println!();
    tracing::info!("Shutdown signal received; VMs keep running");
}
