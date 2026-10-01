mod api;
mod network;
mod cloud_init;
mod credentials;
mod hypervisor;
mod models;
mod pci;
mod persistence;
mod state;

use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::signal;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::state::VmManager;

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
    print_status("Checking UEFI firmware");
    match hypervisor::cloud_hypervisor::default_firmware_path() {
        Some(path) if path.exists() => println!("OK ({})", path.display()),
        Some(path) => println!(
            "MISSING ({}; run glidex-install for Cloud-Hypervisor firmware boot)",
            path.display()
        ),
        None => println!("MISSING (no home directory)"),
    }

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

    // Initialize: load persisted VMs and reconcile state
    print_status("Loading VMs");
    if let Err(e) = vm_manager.initialize().await {
        println!("FAILED");
        eprintln!("\nFailed to initialize VMs from database: {}", e);
        std::process::exit(1);
    }
    let vm_count = vm_manager.list_vms().await.len();
    println!("OK ({} VMs)", vm_count);

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

    // Clone vm_manager for the shutdown handler before passing to router
    let vm_manager_shutdown = Arc::clone(&vm_manager);

    // Create router
    let app = api::create_router(vm_manager).layer(TraceLayer::new_for_http());

    // The API is unauthenticated and can reconfigure host networking
    // through glidex-netd, so listen on loopback only unless told otherwise
    // (spec §14). Both address families, since "localhost" may be ::1.
    let addrs: Vec<SocketAddr> = match std::env::var("GLIDEX_LISTEN") {
        Ok(list) => list
            .split(',')
            .filter_map(|a| a.trim().parse().ok())
            .collect(),
        Err(_) => vec![
            SocketAddr::from(([127, 0, 0, 1], 8841)),
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 8841)),
        ],
    };
    let mut listeners = Vec::new();
    for addr in &addrs {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => listeners.push(l),
            Err(e) => eprintln!("  Warning: cannot listen on {}: {}", addr, e),
        }
    }
    if listeners.is_empty() {
        eprintln!("No usable listen address (set GLIDEX_LISTEN, e.g. 127.0.0.1:8841)");
        std::process::exit(1);
    }
    println!();
    for l in &listeners {
        println!("  Listening on http://{}", l.local_addr().unwrap());
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

    // One shutdown signal (which stops VMs) fans out to every listener.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal(vm_manager_shutdown).await;
        let _ = stop_tx.send(true);
    });
    let servers = listeners.into_iter().map(|listener| {
        let app = app.clone();
        let mut rx = stop_rx.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.wait_for(|stop| *stop).await;
                })
                .await
        })
    });
    for server in servers.collect::<Vec<_>>() {
        if let Ok(Err(e)) = server.await {
            eprintln!("server error: {}", e);
        }
    }
}

async fn shutdown_signal(vm_manager: Arc<VmManager>) {
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
    tracing::info!("Shutdown signal received, stopping VMs...");

    // Stop all running hypervisor processes
    vm_manager.shutdown().await;

    tracing::info!("Shutdown complete");
}
