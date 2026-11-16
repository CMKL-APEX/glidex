//! glidex-netd daemon entry point. Runs as root under systemd
//! (`packaging/glidex-netd.service`).

use glidex_netd::auth;
use glidex_netd::proto::{ADMIN_SOCKET_NAME, FULL_SOCKET_NAME, STATUS_SOCKET_NAME};
use glidex_netd::server::{self, Access, Config, Netd};
use glidex_netd::supervisor::DnsmasqSupervisor;
use glidex_ovs::SystemExec;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    group: Option<String>,
    nat_supernet: Option<String>,
    run_dir: Option<PathBuf>,
    state_path: Option<PathBuf>,
    ovs_bin_dir: Option<PathBuf>,
    commit_window_secs: Option<u64>,
    gateway_check_secs: Option<u64>,
    admin_group: Option<String>,
    policy: Option<auth::Policy>,
    /// Router SNAT zones to meter (`ovn.snat_ct_zones` of the control plane).
    ct_zones: Option<(u16, u16)>,
    /// Source routes for ports that share a subnet (spec/09-networking.md §8.6).
    source_routes: Option<glidex_ovs::srcroute::Config>,
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/local/bin".into(), "/usr/bin".into()])
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Keep source routes current (spec/09-networking.md §8.6.4): a pass after
/// each burst of address changes (`ip monitor address`, debounced 1 s),
/// and every 60 s for anything missed.
fn watch_source_routes(netd: Arc<Netd>) {
    use std::io::BufRead;
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::Duration;
    let (tx, rx) = channel::<()>();
    std::thread::spawn(move || {
        loop {
            match std::process::Command::new("ip")
                .args(["-o", "monitor", "address"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(out) = child.stdout.take() {
                        for _ in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                            if tx.send(()).is_err() {
                                return;
                            }
                        }
                    }
                    let _ = child.wait();
                    tracing::warn!("ip monitor address ended; restarting it");
                }
                Err(e) => tracing::warn!(error = %e, "cannot run ip monitor address"),
            }
            std::thread::sleep(Duration::from_secs(5));
        }
    });
    std::thread::spawn(move || {
        loop {
            match rx.recv_timeout(Duration::from_secs(60)) {
                Ok(()) => {
                    std::thread::sleep(Duration::from_secs(1));
                    while rx.try_recv().is_ok() {}
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let pass = netd.source_routes_pass();
            for c in &pass.applied {
                tracing::info!(command = %c, "source routes");
            }
            for e in &pass.errors {
                tracing::warn!(error = %e, "source routes");
            }
        }
    });
}

/// Give the boot devices with no driver yet a `driver_override` of
/// `vfio-pci`; what it did goes to stderr (the journal, through udev).
fn claim_boot_devices() {
    let path = std::path::Path::new(glidex_ovs::boot_devices::BOOT_DEVICES_FILE);
    let cfg = match glidex_ovs::boot_devices::load(path) {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(e) => {
            eprintln!("glidex-netd: claim: {e}");
            return;
        }
    };
    for (bdf, r) in glidex_ovs::boot_devices::claim(&SystemExec::new(), &cfg) {
        match r {
            Ok(()) => eprintln!("glidex-netd: claimed {bdf} for vfio-pci"),
            Err(e) => eprintln!("glidex-netd: claim {bdf}: {e}"),
        }
    }
}

fn main() {
    // `--claim-boot-devices`: from the modprobe hook, before a host driver
    // loads (spec/27-gpu-vms.md G20). It reads nothing else and never fails,
    // so the driver still loads for the devices it isn't kept from.
    if std::env::args().any(|a| a == "--claim-boot-devices") {
        claim_boot_devices();
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let config_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/glidex/netd.json"));
    // A missing file means defaults; an unreadable or invalid one is fatal,
    // so a typo can't silently replace a narrowed policy with the default.
    let file: FileConfig = match std::fs::read(&config_path) {
        Ok(b) => match serde_json::from_slice(&b) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("glidex-netd: {}: {}", config_path.display(), e);
                std::process::exit(1);
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
        Err(e) => {
            eprintln!("glidex-netd: {}: {}", config_path.display(), e);
            std::process::exit(1);
        }
    };

    let mut config = Config::default();
    if let Some(g) = file.group {
        config.group = g;
    }
    if let Some(n) = file.nat_supernet.and_then(|s| s.parse().ok()) {
        config.nat_supernet = n;
    }
    if let Some(d) = file.run_dir {
        config.run_dir = d;
    }
    if let Some(p) = file.state_path {
        config.state_path = p;
    }
    if let Some(z) = file.ct_zones {
        config.ct_zones = z;
    }
    if let Some(s) = file.source_routes {
        config.source_routes = s;
    }
    config.probe.ch_binary = find_in_path("cloud-hypervisor");
    if let Some(s) = file.commit_window_secs {
        config.commit_window = std::time::Duration::from_secs(s);
    }
    if let Some(s) = file.gateway_check_secs {
        config.gateway_check = std::time::Duration::from_secs(s);
    }
    if let Some(g) = file.admin_group {
        config.admin_group = g;
    }
    config.policy = file
        .policy
        .unwrap_or_else(|| auth::default_policy(&config.group, &config.admin_group));
    for p in auth::suspicious_patterns(&config.policy) {
        tracing::warn!(pattern = %p, "policy pattern never matches: only a trailing '*' is a glob");
    }
    tracing::info!(policy = %serde_json::to_string(&config.policy).unwrap_or_default(), "netd policy");

    let exec: Arc<dyn glidex_ovs::Exec> = Arc::new(match file.ovs_bin_dir {
        Some(dir) => SystemExec::with_ovs_bin_dir(dir),
        None => SystemExec::new(),
    });
    let group_gid = auth::group_gid(&config.group);
    if group_gid.is_none() {
        tracing::warn!(group = %config.group, "group does not exist; only root can use the full socket");
    }
    if let Err(e) = std::fs::create_dir_all(&config.run_dir) {
        eprintln!("glidex-netd: {}: {}", config.run_dir.display(), e);
        std::process::exit(1);
    }

    // `--boot-devices [FILE]`: apply the boot settings (VF counts,
    // vfio-pci bindings) and exit (spec/27-gpu-vms.md §10). netd running: its
    // ops, as the installer's `--now`; else directly, as
    // glidex-vfio-boot.service does before netd starts.
    if let Some(i) = std::env::args().position(|a| a == "--boot-devices") {
        let path = std::env::args()
            .nth(i + 1)
            .filter(|a| !a.starts_with("--"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(glidex_ovs::boot_devices::BOOT_DEVICES_FILE));
        let cfg = match glidex_ovs::boot_devices::load(&path) {
            Ok(Some(c)) => c,
            Ok(None) => {
                println!("no boot devices ({})", path.display());
                return;
            }
            Err(e) => {
                eprintln!("glidex-netd: {e}");
                std::process::exit(1);
            }
        };
        let (full, _) = glidex_netd::client::socket_paths(&config.run_dir);
        let report = match glidex_netd::client::Client::connect(&full) {
            Ok(mut client) => glidex_netd::boot::apply_live(exec.as_ref(), &cfg, |op| {
                client
                    .call_value(op, std::time::Duration::from_secs(120))
                    .map_err(|e| e.to_string())
            }),
            Err(_) => match Netd::new(exec, Arc::new(DnsmasqSupervisor::new()), config.clone()) {
                Ok(n) => n.apply_boot_devices(&cfg, true),
                Err(e) => {
                    eprintln!("glidex-netd: {e}");
                    std::process::exit(1);
                }
            },
        };
        // Partly applied is still applied: what failed stays on its driver
        // and is reported; the rest of the boot goes on.
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return;
    }

    let netd = match Netd::new(exec, Arc::new(DnsmasqSupervisor::new()), config.clone()) {
        Ok(n) => Arc::new(n),
        Err(e) => {
            eprintln!("glidex-netd: {}", e);
            std::process::exit(1);
        }
    };
    if let Err(e) = netd.check_source_routes() {
        eprintln!("glidex-netd: {}: {e}", config_path.display());
        std::process::exit(1);
    }

    if let Some(gid) = group_gid {
        let _ = nix::unistd::chown(
            &config.run_dir.join("vhost"),
            None,
            Some(nix::unistd::Gid::from_raw(gid)),
        );
    }
    netd.enable_ct_events();
    let report = netd.reconcile_startup();
    if let Some(gid) = group_gid {
        // reconcile_startup created the vhost dir; hand it to the group.
        let _ = nix::unistd::chown(
            &config.run_dir.join("vhost"),
            None,
            Some(nix::unistd::Gid::from_raw(gid)),
        );
    }
    tracing::info!(report = %server::status_json(&report), "startup reconciliation");

    let full = server::bind(&config.run_dir.join(FULL_SOCKET_NAME), 0o660, group_gid);
    let status = server::bind(&config.run_dir.join(STATUS_SOCKET_NAME), 0o666, None);
    let (full, status) = match (full, status) {
        (Ok(f), Ok(s)) => (f, s),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("glidex-netd: cannot bind sockets: {}", e);
            std::process::exit(1);
        }
    };
    // The admin socket exists only with its group; otherwise drop a stale
    // one (the run directory survives restarts).
    let admin_path = config.run_dir.join(ADMIN_SOCKET_NAME);
    let admin_gid = auth::group_gid(&config.admin_group);
    let admin = match admin_gid {
        Some(gid) => match server::bind(&admin_path, 0o660, Some(gid)) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("glidex-netd: cannot bind {}: {}", admin_path.display(), e);
                std::process::exit(1);
            }
        },
        None => {
            let _ = std::fs::remove_file(&admin_path);
            tracing::info!(group = %config.admin_group, "admin group does not exist; no admin socket");
            None
        }
    };
    tracing::info!(run_dir = %config.run_dir.display(), admin_socket = admin.is_some(), "listening");
    glidex_netd::sd::notify_ready();
    if let Some(admin) = admin {
        let admin_netd = netd.clone();
        std::thread::spawn(move || server::serve(admin_netd, admin, Access::Admin, admin_gid));
    }
    let expiry_netd = netd.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            expiry_netd.expire_pending();
        }
    });
    if !config.source_routes.subnets.is_empty() {
        watch_source_routes(netd.clone());
    }
    // OVS keeps no OpenFlow rules across a restart of ovs-vswitchd: put
    // the VM ports' rules back (spec/09-networking.md §9.1).
    let rules_netd = netd.clone();
    std::thread::spawn(move || {
        // A failure is logged when it first shows, not every pass.
        let mut failing = Vec::new();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let pass = rules_netd.repair_rules();
            for p in &pass.applied {
                tracing::info!(port = %p, "OpenFlow rules put back");
            }
            for e in pass.errors.iter().filter(|e| !failing.contains(*e)) {
                tracing::warn!(error = %e, "OpenFlow rules");
            }
            failing = pass.errors;
        }
    });
    let status_netd = netd.clone();
    std::thread::spawn(move || server::serve(status_netd, status, Access::Status, None));
    server::serve(netd, full, Access::Full, group_gid);
}
