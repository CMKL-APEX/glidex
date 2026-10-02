//! systemd socket activation (`sd_listen_fds`), without libsystemd.

use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixListener;

const SD_LISTEN_FDS_START: RawFd = 3;

/// The socket systemd passed us, if this process was socket-activated
/// (`LISTEN_PID` is our pid). Exactly one fd is expected. Clears the
/// `LISTEN_*` variables so children don't inherit them; call it before
/// starting threads.
pub fn listener() -> Option<std::io::Result<UnixListener>> {
    let pid: u32 = std::env::var("LISTEN_PID").ok()?.parse().ok()?;
    if pid != std::process::id() {
        return None;
    }
    let fds = std::env::var("LISTEN_FDS").ok();
    std::env::remove_var("LISTEN_PID");
    std::env::remove_var("LISTEN_FDS");
    std::env::remove_var("LISTEN_FDNAMES");
    if fds.as_deref() != Some("1") {
        return Some(Err(std::io::Error::other(format!(
            "expected LISTEN_FDS=1, got {}",
            fds.as_deref().unwrap_or("nothing")
        ))));
    }
    // SAFETY: LISTEN_PID names this process, so systemd handed us fd 3 and
    // nothing else in the process owns it; ownership moves to the listener.
    let listener = unsafe { UnixListener::from_raw_fd(SD_LISTEN_FDS_START) };
    if let Err(e) = listener.local_addr() {
        return Some(Err(std::io::Error::other(format!("fd 3 is not a Unix socket: {e}"))));
    }
    if let Err(e) = nix::fcntl::fcntl(
        SD_LISTEN_FDS_START,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    ) {
        return Some(Err(e.into()));
    }
    Some(Ok(listener))
}
