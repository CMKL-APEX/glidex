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

const DETACHED_IN_SANDBOX: &str = "The detached VM runner (reconcile.vm_runner \"detached\", or \"auto\" outside a \
glidex-control-plane*.service unit) runs hypervisors as children of this process, but it \
runs with no_new_privs (a sandboxed unit), which drops cloud-hypervisor's CAP_NET_ADMIN, \
and the unit closes /dev/kvm. Set vm_runner \"systemd\" (\"auto\" picks it in the \
installed glidex-control-plane.service).";

/// `Some(())` when this process runs with `no_new_privs`, as under the
/// installed unit's sandbox.
fn no_new_privs() -> Option<()> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .any(|l| l.split_once(':').is_some_and(|(k, v)| k == "NoNewPrivs" && v.trim() == "1"))
        .then_some(())
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

    // `--force-new-cluster [--from <snapshot>]` (spec/clustering.md §5.12): recovery, then exit.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--force-new-cluster") {
        let from = args.iter().position(|a| a == "--from").and_then(|i| args.get(i + 1)).map(std::path::PathBuf::from);
        let cfg = config::Config::load().unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1);
        });
        let db = state::VmManager::default_db_path_pub();
        match glidex_control_plane::cluster::manage::force_new_cluster(&db, from.as_deref(), &cfg.cluster).await {
            Ok(()) => {
                println!("A new single-voter cluster was started from {}. Start the control plane; re-join the other servers with fresh state.", from.map(|f| f.display().to_string()).unwrap_or_else(|| db.display().to_string()));
                return;
            }
            Err(e) => {
                eprintln!("force-new-cluster failed: {e}");
                std::process::exit(1);
            }
        }
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
    let vm_manager = match state::VmManager::open_default(&cfg).await {
        Ok(manager) => manager,
        Err(e) => {
            println!("FAILED");
            eprintln!("\nFailed to initialize VM manager: {}", e);
            std::process::exit(1);
        }
    };
    println!("OK");
    vm_manager.configure(&cfg);

    // KVM (spec/reconciliation.md §13.3). Under the systemd runner the
    // hypervisors open /dev/kvm in their glidex-vm units, and this unit's
    // sandbox closes /dev: a missing device is reported, not fatal. Under
    // the detached runner they are this process's children and need it.
    print_status("Checking KVM access");
    if vm_manager.runner().kind_name() == "systemd" {
        // access(2): the VM units run as this user with these groups.
        let rw = nix::unistd::AccessFlags::R_OK | nix::unistd::AccessFlags::W_OK;
        if nix::unistd::access("/dev/kvm", rw).is_ok() {
            println!("OK (VMs open it in their glidex-vm units)");
        } else if Path::new("/dev/kvm").exists() {
            println!("NO ACCESS (this user is not in the kvm group; no VM can start)");
        } else {
            println!("MISSING (/dev/kvm not found; no VM can start until KVM is enabled)");
        }
    } else if let Err(e) = no_new_privs().map_or(Ok(()), |_| Err(DETACHED_IN_SANDBOX.to_string())).and_then(|_| check_kvm_access()) {
        println!("FAILED");
        eprintln!("\n{}", e);
        std::process::exit(1);
    } else {
        println!("OK");
    }

    // Load VMs and adopt the instances still running (spec/reconciliation.md
    // §9.4): nothing is launched or stopped before every VM was observed.
    // An agent starts from its cache: wait for it to be listed so that "not
    // listed yet" is never read as "no VMs" (networking.md §7.7).
    if let Some(link) = vm_manager.node_link() {
        print_status("Waiting for the cluster");
        if link.wait_ready(std::time::Duration::from_secs(20)).await {
            println!("OK");
        } else {
            println!("NO SERVER (running from the cache)");
        }
    }
    print_status("Loading VMs");
    if let Err(e) = vm_manager.initialize().await {
        println!("FAILED");
        eprintln!("\nFailed to initialize VMs from database: {}", e);
        std::process::exit(1);
    }
    let vms = vm_manager.list_vms().await;
    let running = vms.iter().filter(|v| v.status.instance.is_some()).count();
    println!("OK ({} VMs, {} running, VM runner: {})", vms.len(), running, vm_manager.runner().kind_name());

    // Firmware the installer or the host's packages left: imported as
    // firmware images once, so image VMs boot out of the box.
    for (key, result) in vm_manager.auto_import_firmware().await {
        use images::AutoImport;
        match result {
            AutoImport::Imported(name) => tracing::info!(firmware = key, image = %name, "imported firmware"),
            AutoImport::Failed(e) => tracing::warn!(firmware = key, "could not import firmware: {}", e),
            _ => {}
        }
    }

    // Not fatal: kernel-boot VMs don't need it.
    for (ty, label, key) in [
        (hypervisor::HypervisorType::CloudHypervisor, "Cloud-Hypervisor", "cloudhv-edk2"),
        (hypervisor::HypervisorType::Qemu, "QEMU", "ovmf"),
    ] {
        print_status(&format!("Checking UEFI firmware ({})", label));
        match vm_manager.default_firmware_name(ty) {
            Some(name) => println!("OK (image {})", name),
            None => println!("NONE (pull it for firmware boot: gxctl image pull --firmware {})", key),
        }
    }

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
    if let Err(e) = vm_manager.start_metering(&cfg.metering) {
        tracing::warn!("metering not started: {}", e);
    }

    // An agent has no API or UI of its own: the servers' are the cluster's
    // (spec/clustering.md §4).
    if vm_manager.node_link().is_some() {
        println!("  Agent node: serving VMs for the cluster; node API on {}", vm_manager.cluster().map(|c| c.identity.advertise.to_string()).unwrap_or_default());
        println!("  Press Ctrl+C to shutdown");
        shutdown_signal().await;
        return;
    }

    print_status("Loading authorization policies");
    let auth = match auth::AuthService::new(vm_manager.database(), cfg.clone()) {
        Ok(a) => a,
        Err(e) => {
            println!("FAILED");
            eprintln!("\n{}", e);
            std::process::exit(1);
        }
    };
    // Only a node that can write does first-start setup; a cluster's other
    // servers find it in the replicated data.
    if vm_manager.database().can_write() {
        if let Err(e) = vm_manager.projects().adopt_default_quotas(&cfg.quotas.default) {
            println!("WARNING (default project quotas: {})", e);
        }
        match auth.bootstrap(&vm_manager.default_project_id()) {
            Ok(made) if !made.is_empty() => println!("OK (administrators: {})", made.join(", ")),
            Ok(_) => println!("OK"),
            Err(e) => println!("WARNING (bootstrap: {})", e),
        }
    } else {
        println!("OK (follower: setup is the leader's)");
    }
    auth.watch_policies();

    let router = api::router(Arc::new(api::App { manager: vm_manager.clone(), auth }));
    vm_manager.set_api_router(router.clone());
    let app = router.layer(TraceLayer::new_for_http());

    let tls = match serve::tls_setup(&cfg.tls) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("TLS: {}", e);
            std::process::exit(1);
        }
    };
    // Local clients (gxctl, a hand-started UI) trust the published copy
    // (spec/security.md §5.1.1).
    let run_dir = glidex_control_plane::paths::run_dir();
    match &tls {
        Some(t) => {
            if let Err(e) = glidex_tls::publish(&t.cert, &run_dir) {
                eprintln!("  Warning: cannot publish the TLS certificate: {}", e);
            }
        }
        None => glidex_tls::unpublish(&run_dir),
    }
    let (tcp, failed) = serve::bind_tcp(&cfg.listen);
    for (addr, e) in failed {
        eprintln!("  Warning: cannot listen on {}: {}", addr, e);
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
    if let Some(t) = &tls {
        println!(
            "  TLS:       {} ({}{})",
            t.cert.display(),
            if t.self_signed { "self-signed" } else { "configured" },
            if t.generated { ", generated now" } else { "" }
        );
        println!("  SHA-256:   {}", t.fingerprint);
        tracing::info!(cert = %t.cert.display(), fingerprint = %t.fingerprint, self_signed = t.self_signed, "TLS certificate");
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
        .map(|l| tokio::spawn(serve::serve_tcp(l, app.clone(), tls.as_ref().map(|t| t.acceptor.clone()), stop_rx.clone())))
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
