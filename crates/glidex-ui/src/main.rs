//! glidex-ui: the web UI (spec/web-ui.md, spec/security.md §5.6).
//!
//! - `glidex-ui` serves the built UI (`bun run build` → `ui/dist`, or
//!   `GLIDEX_UI_DIR`) on `GLIDEX_UI_LISTEN` (default `127.0.0.1:5173`) and
//!   proxies `/api/*` to the control plane, WebSocket upgrades (the VM
//!   console) included. This is what `glidex-ui.service` runs.
//! - `glidex-ui --dev` runs the Vite dev server (hot reload) instead.
//!
//! Upstream: the control plane's `ui.sock` (`GLIDEX_API_SOCKET`, default
//! `/run/glidex-cp/ui.sock`) when it exists, else `GLIDEX_API_URL`
//! (default `http://127.0.0.1:8841`). The control plane accepts `ui.sock`
//! connections only from the `glidex-ui` user, and only from that peer
//! trusts the `X-Forwarded-*` headers set here. Over TCP (development) the
//! browser's session cookie is all it goes by.
//!
//! Browser protections (spec §5.6): a `Host` allowlist (`GLIDEX_UI_HOSTS`;
//! anything else gets `421`, which blocks DNS rebinding), security headers
//! on every response, and TLS (`GLIDEX_UI_TLS_CERT` + `GLIDEX_UI_TLS_KEY`
//! or the `ui-tls-key` systemd credential), which a non-loopback
//! `GLIDEX_UI_LISTEN` requires.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, Version};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;
use tokio::signal;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

const DEFAULT_API_SOCKET: &str = "/run/glidex-cp/ui.sock";
const DEFAULT_API_URL: &str = "http://127.0.0.1:8841";
const DEFAULT_LISTEN: &str = "127.0.0.1:5173";

/// `Content-Security-Policy` of everything the UI serves. The built app
/// is a module script plus stylesheets from `/assets`; xterm.js and React
/// set inline styles, hence `'unsafe-inline'` for styles only.
const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
                   frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

fn ui_dir() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir).join("ui")
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let dev = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--dev") => true,
        Some("-h" | "--help") => {
            println!(
                "Usage: glidex-ui [--dev]\n\n  (none)  serve the built UI\n  --dev   run the Vite dev server with hot reload\n\n\
                 Environment:\n  GLIDEX_UI_DIR        built UI (default: ui/dist)\n  GLIDEX_UI_LISTEN     address (default {DEFAULT_LISTEN}; non-loopback needs TLS)\n  \
                 GLIDEX_UI_HOSTS      allowed Host values, host[:port],... (default localhost,127.0.0.1,[::1])\n  \
                 GLIDEX_UI_TLS_CERT   PEM certificate chain\n  GLIDEX_UI_TLS_KEY    PEM key (or the ui-tls-key systemd credential)\n  \
                 GLIDEX_API_SOCKET    control plane ui.sock (default {DEFAULT_API_SOCKET}, used when it exists)\n  \
                 GLIDEX_API_URL       control plane over TCP otherwise (default {DEFAULT_API_URL})"
            );
            return;
        }
        Some(other) => {
            eprintln!("unknown argument '{}' (see --help)", other);
            std::process::exit(2);
        }
    };
    if dev {
        run_dev_server().await
    } else if let Err(e) = serve().await {
        tracing::error!("{}", e);
        std::process::exit(1);
    }
}

// ---- Host allowlist --------------------------------------------------------

/// One `GLIDEX_UI_HOSTS` entry: a host name or address (IPv6 in brackets),
/// optionally with the only port it may be reached on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostRule {
    host: String,
    port: Option<u16>,
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`, lowercased. `None` when
/// malformed (including a bare IPv6 address, which `Host` can't carry).
fn split_host_port(s: &str) -> Option<(String, Option<u16>)> {
    let s = s.trim().to_ascii_lowercase();
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = format!("[{}]", &rest[..end]);
        match &rest[end + 1..] {
            "" => (host, None),
            p => (host, Some(p.strip_prefix(':')?)),
        }
    } else {
        match s.split_once(':') {
            None => (s.clone(), None),
            Some((h, p)) => (h.to_string(), Some(p)),
        }
    };
    if host.is_empty() || host == "[]" || host.contains(['/', '@', ' ']) {
        return None;
    }
    let port = match port {
        None => None,
        Some(p) => Some(p.parse::<u16>().ok()?),
    };
    Some((host, port))
}

fn parse_host_rules(list: &str) -> Result<Vec<HostRule>, String> {
    let rules: Vec<HostRule> = list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            split_host_port(s)
                .map(|(host, port)| HostRule { host, port })
                .ok_or_else(|| format!("GLIDEX_UI_HOSTS: '{}' is not host[:port]", s))
        })
        .collect::<Result<_, _>>()?;
    if rules.is_empty() {
        return Err("GLIDEX_UI_HOSTS is empty".into());
    }
    Ok(rules)
}

fn default_host_rules() -> Vec<HostRule> {
    ["localhost", "127.0.0.1", "[::1]"]
        .into_iter()
        .map(|h| HostRule { host: h.into(), port: None })
        .collect()
}

fn host_allowed(rules: &[HostRule], host: &str) -> bool {
    match split_host_port(host) {
        Some((h, port)) => rules
            .iter()
            .any(|r| r.host == h && (r.port.is_none() || r.port == port)),
        None => false,
    }
}

/// The `Host` the browser asked for (`:authority` for HTTP/2).
fn request_host(req: &Request) -> Option<String> {
    req.headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or_else(|| req.uri().authority().map(|a| a.to_string()))
}

async fn host_check(State(rules): State<Arc<Vec<HostRule>>>, req: Request, next: Next) -> Response {
    match request_host(&req) {
        Some(h) if host_allowed(&rules, &h) => next.run(req).await,
        h => {
            tracing::debug!(host = ?h, "refused Host");
            (StatusCode::MISDIRECTED_REQUEST, "421 Misdirected Request: this Host is not served here\n").into_response()
        }
    }
}

// ---- response headers -------------------------------------------------------

fn add_security_headers(h: &mut HeaderMap, tls: bool) {
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    if tls {
        h.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    }
}

async fn security_headers(State(tls): State<bool>, req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    add_security_headers(resp.headers_mut(), tls);
    resp
}

// ---- proxy -----------------------------------------------------------------

/// Where the control plane is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UpstreamSpec {
    Unix(PathBuf),
    /// `host:port` of an `http://` URL.
    Http(String),
}

/// `GLIDEX_API_SOCKET` (or the default `ui.sock`) when it exists, else
/// `GLIDEX_API_URL`. An explicit `GLIDEX_API_URL` without an explicit
/// socket skips the default socket, so a development UI can point at a
/// scratch control plane on a host that also runs glidex.
fn choose_upstream(socket: Option<&str>, url: Option<&str>, exists: impl Fn(&Path) -> bool) -> Result<UpstreamSpec, String> {
    let try_socket = socket.is_some() || url.is_none();
    let path = PathBuf::from(socket.unwrap_or(DEFAULT_API_SOCKET));
    if try_socket && exists(&path) {
        return Ok(UpstreamSpec::Unix(path));
    }
    if socket.is_some() {
        tracing::warn!("GLIDEX_API_SOCKET {} does not exist; using GLIDEX_API_URL", path.display());
    }
    let url = url.unwrap_or(DEFAULT_API_URL);
    let api: Uri = url.parse().map_err(|e| format!("GLIDEX_API_URL: {}", e))?;
    let authority = api
        .authority()
        .filter(|_| api.scheme_str() == Some("http"))
        .ok_or("GLIDEX_API_URL must be http://host:port")?;
    Ok(UpstreamSpec::Http(authority.to_string()))
}

#[derive(Clone)]
enum Upstream {
    Tcp {
        client: Client<HttpConnector, Body>,
        /// `http://host:port`, without a trailing slash.
        base: String,
        authority: HeaderValue,
    },
    Unix(PathBuf),
}

impl Upstream {
    fn new(spec: UpstreamSpec) -> Result<Self, String> {
        Ok(match spec {
            UpstreamSpec::Unix(p) => Upstream::Unix(p),
            UpstreamSpec::Http(authority) => Upstream::Tcp {
                client: Client::builder(TokioExecutor::new()).build_http(),
                base: format!("http://{}", authority),
                authority: HeaderValue::from_str(&authority).map_err(|e| e.to_string())?,
            },
        })
    }

    fn describe(&self) -> String {
        match self {
            Upstream::Tcp { base, .. } => base.clone(),
            Upstream::Unix(p) => format!("unix:{}", p.display()),
        }
    }

    async fn send(&self, mut req: Request) -> Result<hyper::Response<hyper::body::Incoming>, String> {
        let path = upstream_path(req.uri());
        match self {
            Upstream::Tcp { client, base, authority } => {
                *req.uri_mut() = format!("{}{}", base, path).parse().map_err(|e| format!("{}", e))?;
                req.headers_mut().insert(header::HOST, authority.clone());
                client.request(req).await.map_err(|e| e.to_string())
            }
            Upstream::Unix(sock) => {
                *req.uri_mut() = path.parse().map_err(|e| format!("{}", e))?;
                req.headers_mut().insert(header::HOST, HeaderValue::from_static("localhost"));
                let stream = tokio::net::UnixStream::connect(sock).await.map_err(|e| e.to_string())?;
                let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, Body>(TokioIo::new(stream))
                    .await
                    .map_err(|e| e.to_string())?;
                tokio::spawn(async move {
                    if let Err(e) = conn.with_upgrades().await {
                        tracing::debug!("upstream connection: {}", e);
                    }
                });
                sender.send_request(req).await.map_err(|e| e.to_string())
            }
        }
    }
}

#[derive(Clone)]
struct AppState {
    upstream: Upstream,
    /// The UI itself serves TLS.
    tls: bool,
}

/// The TCP peer of a browser connection.
#[derive(Debug, Clone, Copy)]
struct PeerAddr(SocketAddr);

/// `/api/x?y` → `/x?y`, like the Vite dev proxy.
fn upstream_path(uri: &Uri) -> String {
    let pq = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let rest = pq.strip_prefix("/api").unwrap_or(pq);
    if rest.starts_with('/') {
        rest.to_string()
    } else {
        format!("/{}", rest)
    }
}

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");

/// Replace (never append to) the forwarding headers with what this
/// connection actually is. `Origin`, `Cookie`, `X-Glidex-CSRF` and
/// `Authorization` pass through untouched.
fn rewrite_forwarded(h: &mut HeaderMap, peer: Option<IpAddr>, tls: bool, host: Option<&str>) {
    for n in [&X_FORWARDED_FOR, &X_FORWARDED_PROTO, &X_FORWARDED_HOST, &header::FORWARDED] {
        h.remove(n);
    }
    if let Some(ip) = peer {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        h.insert(X_FORWARDED_FOR, HeaderValue::from_str(&ip.to_string()).expect("an IP is a header value"));
    }
    h.insert(X_FORWARDED_PROTO, HeaderValue::from_static(if tls { "https" } else { "http" }));
    if let Some(v) = host.and_then(|h| HeaderValue::from_str(h).ok()) {
        h.insert(X_FORWARDED_HOST, v);
    }
}

async fn proxy_api(State(s): State<AppState>, mut req: Request) -> Response {
    let peer = req.extensions().get::<PeerAddr>().map(|p| p.0.ip());
    let host = request_host(&req);
    rewrite_forwarded(req.headers_mut(), peer, s.tls, host.as_deref());
    // The upstream connection is HTTP/1.1 whatever the browser spoke.
    *req.version_mut() = Version::HTTP_11;

    // WebSocket (console): forward the upgrade, then splice the two
    // connections together.
    let client_upgrade = req
        .headers()
        .contains_key(header::UPGRADE)
        .then(|| hyper::upgrade::on(&mut req));

    let mut resp = match s.upstream.send(req).await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("control plane unreachable at {}: {}", s.upstream.describe(), e),
            )
                .into_response()
        }
    };
    if let Some(client_upgrade) = client_upgrade {
        if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
            let upstream_upgrade = hyper::upgrade::on(&mut resp);
            tokio::spawn(async move {
                match tokio::try_join!(client_upgrade, upstream_upgrade) {
                    Ok((client, upstream)) => {
                        let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(upstream)).await;
                    }
                    Err(e) => tracing::warn!("WebSocket upgrade failed: {}", e),
                }
            });
        }
    }
    resp.map(Body::new)
}

fn app(dist: &Path, state: AppState, hosts: Vec<HostRule>) -> Router {
    let index = dist.join("index.html");
    let tls = state.tls;
    // Unknown paths get index.html: they are client-side routes.
    Router::new()
        .route("/api", any(proxy_api))
        .route("/api/{*rest}", any(proxy_api))
        .fallback_service(ServeDir::new(dist).fallback(ServeFile::new(&index)))
        .with_state(state)
        .layer(from_fn_with_state(Arc::new(hosts), host_check))
        .layer(from_fn_with_state(tls, security_headers))
}

// ---- listener and TLS --------------------------------------------------------

/// Refuse a non-loopback address without TLS (spec §5.1).
fn check_listen(addr: &SocketAddr, tls: bool) -> Result<(), String> {
    if tls || addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "refusing to serve on {} without TLS: set GLIDEX_UI_TLS_CERT and GLIDEX_UI_TLS_KEY \
             (or the ui-tls-key credential), or listen on a loopback address",
            addr
        ))
    }
}

/// The certificate and key paths, if TLS is configured.
fn tls_paths(cert: Option<String>, key: Option<String>, credential: Option<PathBuf>) -> Result<Option<(PathBuf, PathBuf)>, String> {
    match (cert, key.map(PathBuf::from).or(credential)) {
        (None, None) => Ok(None),
        (Some(c), Some(k)) => Ok(Some((PathBuf::from(c), k))),
        (Some(_), None) => Err("GLIDEX_UI_TLS_CERT needs GLIDEX_UI_TLS_KEY or the ui-tls-key credential".into()),
        (None, Some(_)) => Err("a TLS key is set but GLIDEX_UI_TLS_CERT is not".into()),
    }
}

fn credential(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("CREDENTIALS_DIRECTORY")?).join(name);
    p.exists().then_some(p)
}

fn tls_acceptor(cert: &Path, key: &Path) -> Result<tokio_rustls::TlsAcceptor, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("{}: {}", cert.display(), e))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {}", cert.display(), e))?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {}", key.display(), e))?;
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let mut cfg = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS: {}", e))?;
    // HTTP/1.1 only: the console WebSocket needs it.
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
}

async fn serve_conn<I>(io: I, router: Router, addr: SocketAddr)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
        let router = router.clone();
        req.extensions_mut().insert(PeerAddr(addr));
        async move { router.oneshot(req.map(Body::new)).await }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(io), svc)
        .with_upgrades();
    if let Err(e) = conn.await {
        tracing::debug!(%addr, "connection: {}", e);
    }
}

async fn accept_loop(listener: tokio::net::TcpListener, router: Router, tls: Option<tokio_rustls::TlsAcceptor>) {
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("accept: {}", e);
                continue;
            }
        };
        let router = router.clone();
        match tls.clone() {
            Some(acceptor) => {
                tokio::spawn(async move {
                    match tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(stream)).await {
                        Ok(Ok(s)) => serve_conn(s, router, addr).await,
                        Ok(Err(e)) => tracing::debug!(%addr, "TLS handshake failed: {}", e),
                        Err(_) => tracing::debug!(%addr, "TLS handshake timed out"),
                    }
                });
            }
            None => {
                tokio::spawn(serve_conn(stream, router, addr));
            }
        }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn serve() -> Result<(), String> {
    let dist = std::env::var_os("GLIDEX_UI_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ui_dir().join("dist"));
    if !dist.join("index.html").is_file() {
        return Err(format!(
            "{} not found: build the UI (cd crates/glidex-ui/ui && bun run build), set GLIDEX_UI_DIR, or use --dev",
            dist.join("index.html").display()
        ));
    }
    let listen: SocketAddr = env("GLIDEX_UI_LISTEN")
        .unwrap_or_else(|| DEFAULT_LISTEN.into())
        .parse()
        .map_err(|e| format!("GLIDEX_UI_LISTEN: {}", e))?;
    let tls = match tls_paths(env("GLIDEX_UI_TLS_CERT"), env("GLIDEX_UI_TLS_KEY"), credential("ui-tls-key"))? {
        Some((cert, key)) => Some(tls_acceptor(&cert, &key)?),
        None => None,
    };
    check_listen(&listen, tls.is_some())?;
    let hosts = match env("GLIDEX_UI_HOSTS") {
        Some(list) => parse_host_rules(&list)?,
        None => default_host_rules(),
    };
    let spec = choose_upstream(env("GLIDEX_API_SOCKET").as_deref(), env("GLIDEX_API_URL").as_deref(), |p| p.exists())?;
    let upstream = Upstream::new(spec)?;
    let describe = upstream.describe();
    let router = app(&dist, AppState { upstream, tls: tls.is_some() }, hosts);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| format!("bind {}: {}", listen, e))?;
    let scheme = if tls.is_some() { "https" } else { "http" };
    tracing::info!("GlideX UI on {}://{} (from {}, API {})", scheme, listen, dist.display(), describe);
    tokio::select! {
        _ = accept_loop(listener, router, tls) => {}
        _ = shutdown_signal() => {}
    }
    Ok(())
}

async fn shutdown_signal() {
    let mut term = signal::unix::signal(signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("Shutting down...");
}

async fn run_dev_server() {
    let ui_path = ui_dir();
    tracing::info!("Starting UI dev server from {}", ui_path.display());

    let mut child = Command::new("bun")
        .arg("run")
        .arg("dev")
        .current_dir(&ui_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to start bun dev server. Is bun installed?");

    tracing::info!("GlideX UI available at http://localhost:5173");

    tokio::select! {
        status = child.wait() => {
            match status {
                Ok(s) => tracing::info!("Bun dev server exited with {}", s),
                Err(e) => tracing::error!("Bun dev server error: {}", e),
            }
        }
        _ = signal::ctrl_c() => {
            tracing::info!("Shutting down...");
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn api_prefix_is_stripped() {
        let up = |s: &str| upstream_path(&s.parse().unwrap());
        assert_eq!(up("/api/vms"), "/vms");
        assert_eq!(up("/api/vms/x/console/ws?ticket=t"), "/vms/x/console/ws?ticket=t");
        assert_eq!(up("/api/images?all=1"), "/images?all=1");
        assert_eq!(up("/api"), "/");
    }

    #[test]
    fn host_parsing() {
        assert_eq!(split_host_port("LocalHost:5173"), Some(("localhost".into(), Some(5173))));
        assert_eq!(split_host_port("127.0.0.1"), Some(("127.0.0.1".into(), None)));
        assert_eq!(split_host_port("[::1]:80"), Some(("[::1]".into(), Some(80))));
        assert_eq!(split_host_port("[::1]"), Some(("[::1]".into(), None)));
        assert_eq!(split_host_port("::1"), None);
        assert_eq!(split_host_port("a:b"), None);
        assert_eq!(split_host_port("a:99999"), None);
        assert_eq!(split_host_port("[::1]x"), None);
        assert_eq!(split_host_port(""), None);
        assert_eq!(split_host_port("user@host"), None);
    }

    #[test]
    fn host_matching() {
        let d = default_host_rules();
        for ok in ["localhost", "localhost:5173", "127.0.0.1:8080", "[::1]:5173", "LOCALHOST"] {
            assert!(host_allowed(&d, ok), "{ok}");
        }
        for bad in ["evil.example", "localhost.evil.example", "127.0.0.2", "", "[::2]:5173", "::1"] {
            assert!(!host_allowed(&d, bad), "{bad}");
        }
        let r = parse_host_rules("glidex.example.org, 10.0.0.5:8443").unwrap();
        assert!(host_allowed(&r, "glidex.example.org"));
        assert!(host_allowed(&r, "glidex.example.org:443"));
        assert!(host_allowed(&r, "10.0.0.5:8443"));
        assert!(!host_allowed(&r, "10.0.0.5"));
        assert!(!host_allowed(&r, "10.0.0.5:8444"));
        assert!(!host_allowed(&r, "localhost"));
        assert!(parse_host_rules(" , ").is_err());
        assert!(parse_host_rules("a:b").is_err());
    }

    #[test]
    fn forwarded_headers_are_replaced() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "6.6.6.6".parse().unwrap());
        h.append("x-forwarded-for", "7.7.7.7".parse().unwrap());
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        h.insert("x-forwarded-host", "evil".parse().unwrap());
        h.insert("forwarded", "for=6.6.6.6".parse().unwrap());
        h.insert("origin", "http://localhost:5173".parse().unwrap());
        h.insert("cookie", "gx_session=abc".parse().unwrap());
        h.insert("x-glidex-csrf", "c".parse().unwrap());
        h.insert("authorization", "Bearer gxt_x".parse().unwrap());
        rewrite_forwarded(&mut h, Some("127.0.0.1".parse().unwrap()), false, Some("localhost:5173"));
        fn all(h: &HeaderMap, n: &str) -> Vec<String> {
            h.get_all(n).iter().map(|v| v.to_str().unwrap().to_string()).collect()
        }
        assert_eq!(all(&h, "x-forwarded-for"), ["127.0.0.1"]);
        assert_eq!(all(&h, "x-forwarded-proto"), ["http"]);
        assert_eq!(all(&h, "x-forwarded-host"), ["localhost:5173"]);
        assert!(h.get("forwarded").is_none());
        assert_eq!(all(&h, "origin"), ["http://localhost:5173"]);
        assert_eq!(all(&h, "cookie"), ["gx_session=abc"]);
        assert_eq!(all(&h, "x-glidex-csrf"), ["c"]);
        assert_eq!(all(&h, "authorization"), ["Bearer gxt_x"]);

        rewrite_forwarded(&mut h, Some("::ffff:10.1.2.3".parse().unwrap()), true, None);
        assert_eq!(all(&h, "x-forwarded-for"), ["10.1.2.3"]);
        assert_eq!(all(&h, "x-forwarded-proto"), ["https"]);
        assert!(h.get("x-forwarded-host").is_none());
    }

    #[test]
    fn security_headers_and_hsts_only_under_tls() {
        let mut h = HeaderMap::new();
        add_security_headers(&mut h, false);
        assert!(h["content-security-policy"].to_str().unwrap().contains("frame-ancestors 'none'"));
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert_eq!(h["referrer-policy"], "no-referrer");
        assert_eq!(h["x-frame-options"], "DENY");
        assert!(h.get("strict-transport-security").is_none());
        add_security_headers(&mut h, true);
        assert_eq!(h["strict-transport-security"], "max-age=31536000");
    }

    #[test]
    fn non_loopback_needs_tls() {
        let lo: SocketAddr = "127.0.0.1:5173".parse().unwrap();
        let lo6: SocketAddr = "[::1]:5173".parse().unwrap();
        let any: SocketAddr = "0.0.0.0:5173".parse().unwrap();
        let lan: SocketAddr = "10.0.0.5:443".parse().unwrap();
        assert!(check_listen(&lo, false).is_ok());
        assert!(check_listen(&lo6, false).is_ok());
        let e = check_listen(&any, false).unwrap_err();
        assert!(e.contains("without TLS"), "{e}");
        assert!(check_listen(&lan, false).is_err());
        assert!(check_listen(&lan, true).is_ok());
    }

    #[test]
    fn tls_settings() {
        assert_eq!(tls_paths(None, None, None).unwrap(), None);
        assert_eq!(
            tls_paths(Some("c".into()), None, Some("/cred/ui-tls-key".into())).unwrap(),
            Some(("c".into(), "/cred/ui-tls-key".into()))
        );
        assert_eq!(tls_paths(Some("c".into()), Some("k".into()), Some("/x".into())).unwrap(), Some(("c".into(), "k".into())));
        assert!(tls_paths(Some("c".into()), None, None).is_err());
        assert!(tls_paths(None, Some("k".into()), None).is_err());
    }

    #[test]
    fn upstream_choice() {
        let yes = |_: &Path| true;
        let no = |_: &Path| false;
        assert_eq!(choose_upstream(None, None, yes).unwrap(), UpstreamSpec::Unix(DEFAULT_API_SOCKET.into()));
        assert_eq!(choose_upstream(None, None, no).unwrap(), UpstreamSpec::Http("127.0.0.1:8841".into()));
        assert_eq!(choose_upstream(Some("/s"), Some("http://h:1"), yes).unwrap(), UpstreamSpec::Unix("/s".into()));
        assert_eq!(choose_upstream(Some("/s"), Some("http://h:1"), no).unwrap(), UpstreamSpec::Http("h:1".into()));
        // An explicit URL alone doesn't go through the default socket.
        assert_eq!(choose_upstream(None, Some("http://h:1"), yes).unwrap(), UpstreamSpec::Http("h:1".into()));
        assert!(choose_upstream(None, Some("https://h:1"), no).is_err());
    }

    /// Raw HTTP/1.1 exchange with `addr`.
    async fn exchange(addr: SocketAddr, request: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    /// The UI in front of a fake control plane on a Unix socket that
    /// echoes the headers it got.
    #[tokio::test]
    async fn proxies_over_unix_socket_with_rewritten_headers() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("index.html"), "<html>ui</html>").unwrap();
        let sock = dir.path().join("ui.sock");
        let upstream = tokio::net::UnixListener::bind(&sock).unwrap();
        let echo = Router::new().fallback(|req: Request| async move {
            let mut out = format!("path={}\n", req.uri());
            for (k, v) in req.headers() {
                out.push_str(&format!("{}={}\n", k, v.to_str().unwrap()));
            }
            out
        });
        tokio::spawn(async move { axum::serve(upstream, echo).await.unwrap() });

        let state = AppState { upstream: Upstream::new(UpstreamSpec::Unix(sock)).unwrap(), tls: false };
        let router = app(dir.path(), state, default_host_rules());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(accept_loop(listener, router, None));

        let out = exchange(
            addr,
            "GET /api/vms?project=p HTTP/1.1\r\nHost: localhost:5173\r\nX-Forwarded-For: 6.6.6.6\r\n\
             X-Forwarded-Proto: https\r\nCookie: gx_session=abc\r\nX-Glidex-CSRF: c\r\nOrigin: http://localhost:5173\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.contains("path=/vms?project=p\n"), "{out}");
        assert!(out.contains("x-forwarded-for=127.0.0.1\n"), "{out}");
        assert!(!out.contains("6.6.6.6"), "{out}");
        assert!(out.contains("x-forwarded-proto=http\n"), "{out}");
        assert!(out.contains("x-forwarded-host=localhost:5173\n"), "{out}");
        assert!(out.contains("cookie=gx_session=abc\n"), "{out}");
        assert!(out.contains("x-glidex-csrf=c\n"), "{out}");
        assert!(out.contains("origin=http://localhost:5173\n"), "{out}");
        assert!(out.to_ascii_lowercase().contains("x-frame-options: deny"), "{out}");

        // Static files carry the headers too; a foreign Host gets 421.
        let out = exchange(addr, "GET /projects HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.contains("<html>ui</html>"), "{out}");
        assert!(out.to_ascii_lowercase().contains("content-security-policy: default-src 'self'"), "{out}");
        for host in ["evil.example", "localhost.evil.example:5173"] {
            let out = exchange(addr, &format!("GET /api/vms HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")).await;
            assert!(out.starts_with("HTTP/1.1 421"), "{out}");
            assert!(out.to_ascii_lowercase().contains("x-content-type-options: nosniff"), "{out}");
            let out = exchange(addr, &format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")).await;
            assert!(out.starts_with("HTTP/1.1 421"), "{out}");
        }
    }
}
