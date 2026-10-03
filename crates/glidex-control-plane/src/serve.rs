//! Listeners (spec/security.md §5.1): TCP (TLS required off loopback),
//! `api.sock` for local users and `ui.sock` for glidex-ui.
//!
//! Each connection is served by hyper with upgrades (the console
//! WebSocket). Before the router sees a request, it is tagged with the
//! listener it came in on and the peer's identity: the uid from
//! `SO_PEERCRED` on Unix sockets, the address on TCP. The API's
//! authentication layer decides from those.

use crate::api::{ClientAddr, Listener, PeerUid};
use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tower::ServiceExt;

/// Serve one connection until it closes or `stop` fires.
async fn serve_conn<I>(io: I, router: Router, listener: Listener, peer: Option<u32>, addr: Option<SocketAddr>, mut stop: watch::Receiver<bool>)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
        let router = router.clone();
        req.extensions_mut().insert(listener);
        if let Some(uid) = peer {
            req.extensions_mut().insert(PeerUid(uid));
        }
        if let Some(a) = addr {
            req.extensions_mut().insert(ClientAddr(a));
        }
        async move { router.oneshot(req.map(axum::body::Body::new)).await }
    });
    let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    let conn = builder.serve_connection_with_upgrades(TokioIo::new(io), svc);
    tokio::pin!(conn);
    tokio::select! {
        r = conn.as_mut() => {
            if let Err(e) = r {
                tracing::debug!("connection error: {}", e);
            }
        }
        _ = async { let _ = stop.wait_for(|s| *s).await; } => {
            conn.as_mut().graceful_shutdown();
            let _ = conn.await;
        }
    }
}

/// Load the TLS certificate chain and key (the key defaults to the
/// systemd credential `tls-key`).
pub fn tls_acceptor(cert: &Path, key: Option<&Path>) -> Result<tokio_rustls::TlsAcceptor, String> {
    let key_path: PathBuf = match key {
        Some(k) => k.to_path_buf(),
        None => crate::config::credential("tls-key").ok_or("TLS key: set tls.key or the tls-key credential")?,
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("{}: {}", cert.display(), e))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {}", cert.display(), e))?;
    let key = PrivateKeyDer::from_pem_file(&key_path).map_err(|e| format!("{}: {}", key_path.display(), e))?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut cfg = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS: {}", e))?;
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
}

pub async fn serve_tcp(
    listener: tokio::net::TcpListener,
    router: Router,
    tls: Option<tokio_rustls::TlsAcceptor>,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        let (stream, addr) = tokio::select! {
            r = listener.accept() => match r {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("accept: {}", e);
                    continue;
                }
            },
            _ = async { let _ = stop.wait_for(|s| *s).await; } => return,
        };
        let router = router.clone();
        let stop = stop.clone();
        match tls.clone() {
            Some(acceptor) => {
                tokio::spawn(async move {
                    match tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(stream)).await {
                        Ok(Ok(s)) => serve_conn(s, router, Listener::Tcp, None, Some(addr), stop).await,
                        Ok(Err(e)) => tracing::debug!(%addr, "TLS handshake failed: {}", e),
                        Err(_) => tracing::debug!(%addr, "TLS handshake timed out"),
                    }
                });
            }
            None => {
                tokio::spawn(serve_conn(stream, router, Listener::Tcp, None, Some(addr), stop));
            }
        }
    }
}

/// Bind a Unix socket at `path`, replacing a stale one. The socket is
/// connectable by everyone: access is decided per request from the peer
/// uid (api.sock: `glidex-users`/`glidex-admin` members; ui.sock: the
/// glidex-ui user), so its file mode isn't the access control.
pub fn bind_unix(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let l = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(l)
}

pub async fn serve_unix(listener: tokio::net::UnixListener, router: Router, kind: Listener, mut stop: watch::Receiver<bool>) {
    loop {
        let stream = tokio::select! {
            r = listener.accept() => match r {
                Ok((s, _)) => s,
                Err(e) => {
                    tracing::warn!("accept: {}", e);
                    continue;
                }
            },
            _ = async { let _ = stop.wait_for(|s| *s).await; } => return,
        };
        let uid = match stream.peer_cred() {
            Ok(c) => c.uid(),
            Err(e) => {
                tracing::warn!("peer credentials: {}", e);
                continue;
            }
        };
        tokio::spawn(serve_conn(stream, router.clone(), kind, Some(uid), None, stop.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::create_router;
    use crate::state::VmManager;

    #[tokio::test]
    async fn unix_socket_requests_carry_the_peer_uid() {
        let dir = tempfile::TempDir::new().unwrap();
        let manager = VmManager::with_db_path(dir.path().join("t.db")).unwrap();
        let router = create_router(manager);
        let sock = dir.path().join("api.sock");
        let l = bind_unix(&sock).unwrap();
        let (tx, rx) = watch::channel(false);
        tokio::spawn(serve_unix(l, router, Listener::Api, rx));
        let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        s.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.contains("x-request-id"), "{out}");
        let _ = tx.send(true);
    }
}
