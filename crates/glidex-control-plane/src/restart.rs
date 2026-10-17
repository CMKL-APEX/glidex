//! A control plane that must come back in another mode asks to be
//! restarted: a host that just became a cluster agent has no API of its own
//! (its UI is relayed to a server, spec/clustering-ui.md §3.6), but the
//! standalone listeners it started with would otherwise stay up. It shuts
//! down as on SIGTERM and exits with [`EXIT_CODE`]; under systemd,
//! `Restart=on-failure` starts it again.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

/// The exit code of a requested restart (EX_TEMPFAIL).
pub const EXIT_CODE: i32 = 75;

static REQUESTED: AtomicBool = AtomicBool::new(false);

fn notify() -> &'static Notify {
    static N: OnceLock<Notify> = OnceLock::new();
    N.get_or_init(Notify::new)
}

/// Whether systemd runs this process (and so starts it again after a
/// failure exit).
pub fn supervised() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
}

/// Ask for a restart: the process shuts down and exits with [`EXIT_CODE`].
pub fn request(why: &str) {
    tracing::warn!("restarting the control plane: {why}");
    REQUESTED.store(true, Ordering::Release);
    notify().notify_one();
}

/// Resolves once a restart is requested.
pub async fn requested() {
    if is_requested() {
        return;
    }
    notify().notified().await
}

pub fn is_requested() -> bool {
    REQUESTED.load(Ordering::Acquire)
}
