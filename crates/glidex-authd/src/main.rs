//! glidex-authd daemon entry point. Runs as root, socket-activated by
//! systemd (`packaging/glidex-authd.socket`); without activation it binds
//! `$GLIDEX_AUTHD_RUN_DIR/auth.sock` itself.

use glidex_authd::authenticator::{user_uid, SystemAccounts};
use glidex_authd::config::{Config, DEFAULT_CONFIG_PATH};
use glidex_authd::pam::{Pam, PamAuthenticator};
use glidex_authd::proto::SOCKET_NAME;
use glidex_authd::server::{self, Authd};
use std::path::PathBuf;
use std::sync::Arc;

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("glidex-authd: {msg}");
    std::process::exit(1);
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    // Before any thread starts: it edits the environment.
    let activated = glidex_authd::activation::listener();

    let config_path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
    let config = Config::load(&config_path).unwrap_or_else(|e| die(e));

    let service_uid = user_uid(&config.service_user);
    if service_uid.is_none() {
        tracing::warn!(user = %config.service_user, "service user does not exist; refusing all peers");
    }
    let pam = Pam::load_default().unwrap_or_else(|e| die(e));

    let listener = match activated {
        Some(Ok(l)) => {
            tracing::info!("socket-activated");
            l
        }
        Some(Err(e)) => die(format!("socket activation: {e}")),
        None => {
            if let Err(e) = std::fs::create_dir_all(&config.run_dir) {
                die(format!("{}: {}", config.run_dir.display(), e));
            }
            // The socket's group is the service user's primary group.
            let gid = nix::unistd::User::from_name(&config.service_user)
                .ok()
                .flatten()
                .map(|u| u.gid.as_raw());
            let path = config.run_dir.join(SOCKET_NAME);
            let l = server::bind(&path, 0o660, gid).unwrap_or_else(|e| die(format!("{}: {}", path.display(), e)));
            tracing::info!(socket = %path.display(), "listening");
            l
        }
    };
    tracing::info!(
        service_user = %config.service_user,
        allow_root = config.allow_root,
        allowed_groups = ?config.allowed_groups,
        "ready"
    );
    let authd = Arc::new(Authd::new(
        config,
        service_uid,
        Arc::new(PamAuthenticator::new(pam)),
        Arc::new(SystemAccounts),
    ));
    authd.serve(listener);
}
