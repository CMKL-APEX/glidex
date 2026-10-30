//! glidex-stord daemon entry point. Runs as root under systemd
//! (`packaging/glidex-stord.service`).

use glidex_stord::auth;
use glidex_stord::lvm::SystemExec;
use glidex_stord::proto::{ADMIN_SOCKET_NAME, SOCKET_NAME};
use glidex_stord::server::{self, Access, Config, Stord};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(serde::Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    group: Option<String>,
    admin_group: Option<String>,
    run_dir: Option<PathBuf>,
    systemd_dir: Option<PathBuf>,
    /// The volume groups glidex may use (spec/24-storage.md S17). None: none.
    allowed_vgs: Vec<String>,
    /// The user that gets the device nodes (default `glidex`).
    owner: Option<String>,
    policy: Option<auth::Policy>,
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("glidex-stord: {msg}");
    std::process::exit(1)
}

fn main() {
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
        .unwrap_or_else(|| PathBuf::from("/etc/glidex/stord.json"));
    // A missing file means defaults (no volume group allowed); an
    // unreadable or invalid one is fatal, so a typo can't widen anything.
    let file: FileConfig = match std::fs::read(&config_path) {
        Ok(b) => serde_json::from_slice(&b)
            .unwrap_or_else(|e| fail(format!("{}: {e}", config_path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
        Err(e) => fail(format!("{}: {e}", config_path.display())),
    };

    let mut config = Config::default();
    if let Some(g) = file.group {
        config.group = g;
    }
    if let Some(g) = file.admin_group {
        config.admin_group = g;
    }
    if let Some(d) = file.run_dir {
        config.run_dir = d;
    }
    if let Some(d) = file.systemd_dir {
        config.systemd_dir = d;
    }
    for vg in &file.allowed_vgs {
        if let Err(e) = glidex_stord::names::lvm_name("volume group", vg) {
            fail(format!("{}: {e}", config_path.display()));
        }
    }
    config.allowed_vgs = file.allowed_vgs;
    let owner = file.owner.unwrap_or_else(|| "glidex".into());
    let Some(user) = nix::unistd::User::from_name(&owner).ok().flatten() else {
        fail(format!("user {owner} does not exist"))
    };
    config.owner = (user.uid.as_raw(), user.gid.as_raw());
    config.policy = file
        .policy
        .unwrap_or_else(|| auth::default_policy(&config.group, &config.admin_group));
    for p in auth::suspicious_patterns(&config.policy) {
        tracing::warn!(pattern = %p, "policy pattern never matches: only a trailing '*' is a glob");
    }
    tracing::info!(
        allowed_vgs = %config.allowed_vgs.join(","),
        policy = %serde_json::to_string(&config.policy).unwrap_or_default(),
        "stord configuration"
    );

    let group_gid = auth::group_gid(&config.group);
    if group_gid.is_none() {
        tracing::warn!(group = %config.group, "group does not exist; only root can use the socket");
    }
    if let Err(e) = std::fs::create_dir_all(&config.run_dir) {
        fail(format!("{}: {e}", config.run_dir.display()));
    }
    let stord = Arc::new(Stord::new(Arc::new(SystemExec), config.clone()));

    let main = server::bind(&config.run_dir.join(SOCKET_NAME), 0o660, group_gid)
        .unwrap_or_else(|e| fail(format!("cannot bind the socket: {e}")));
    let admin_path = config.run_dir.join(ADMIN_SOCKET_NAME);
    let admin_gid = auth::group_gid(&config.admin_group);
    let admin = match admin_gid {
        Some(gid) => Some(
            server::bind(&admin_path, 0o660, Some(gid))
                .unwrap_or_else(|e| fail(format!("cannot bind {}: {e}", admin_path.display()))),
        ),
        None => {
            let _ = std::fs::remove_file(&admin_path);
            None
        }
    };
    tracing::info!(run_dir = %config.run_dir.display(), admin_socket = admin.is_some(), "listening");
    glidex_stord::sd::notify_ready();
    if let Some(admin) = admin {
        let s = stord.clone();
        std::thread::spawn(move || server::serve(s, admin, Access::Admin, admin_gid));
    }
    server::serve(stord, main, Access::Full, group_gid);
}
