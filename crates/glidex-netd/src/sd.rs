//! Minimal systemd readiness notification (`Type=notify`), without libsystemd.

use std::os::unix::net::UnixDatagram;

/// Send `READY=1` to `$NOTIFY_SOCKET` if systemd set one. Returns whether a
/// notification was sent.
pub fn notify_ready() -> bool {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return false;
    };
    notify(&path.to_string_lossy(), b"READY=1")
}

/// Send `msg` to a notify socket path (`@name` = abstract namespace).
pub fn notify(path: &str, msg: &[u8]) -> bool {
    let Ok(sock) = UnixDatagram::unbound() else {
        return false;
    };
    let sent = match path.strip_prefix('@') {
        Some(name) => {
            use std::os::linux::net::SocketAddrExt;
            std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
                .and_then(|addr| sock.send_to_addr(msg, &addr))
        }
        None => sock.send_to(msg, path),
    };
    match sent {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(error = %e, "sd_notify failed");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifies_a_path_socket() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("notify.sock");
        let rx = UnixDatagram::bind(&path).unwrap();
        assert!(notify(path.to_str().unwrap(), b"READY=1"));
        let mut buf = [0u8; 16];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
    }

    #[test]
    fn notifies_an_abstract_socket() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("glidex-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let rx = UnixDatagram::bind_addr(&addr).unwrap();
        assert!(notify(&format!("@{name}"), b"READY=1"));
        let mut buf = [0u8; 16];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1");
    }

    #[test]
    fn no_socket_is_a_no_op() {
        assert!(!notify("/nonexistent/notify.sock", b"READY=1"));
    }
}
