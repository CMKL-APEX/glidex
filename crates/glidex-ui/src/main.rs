//! glidex-ui: the web UI (spec/web-ui.md, spec/security.md §5.6).
//!
//! - `glidex-ui` serves the built UI (`bun run build` → `ui/dist`, or
//!   `GLIDEX_UI_DIR`) over HTTPS on `GLIDEX_UI_LISTEN` (default every
//!   address, port 5173) and
//!   proxies `/api/*` to the control plane, WebSocket upgrades (the VM
//!   console) included. This is what `glidex-ui.service` runs.
//! - `glidex-ui --dev` runs the Vite dev server (hot reload) instead.
//!
//! Upstream, chosen per request: the control plane's `ui.sock`
//! (`GLIDEX_API_SOCKET`, default `/run/glidex-cp/ui.sock`) when it exists,
//! else `GLIDEX_API_URL` (default `https://127.0.0.1:8841`); an explicit
//! socket alone never falls back to TCP (see `upstream_spec`). The control plane accepts `ui.sock`
//! connections only from the `glidex-ui` user, and only from that peer
//! trusts the `X-Forwarded-*` headers set here. Over TCP (development) the
//! browser's session cookie is all it goes by.
//!
//! Browser protections (spec §5.6): a `Host` allowlist (`GLIDEX_UI_HOSTS`;
//! anything else gets `421`, which blocks DNS rebinding), security headers
//! on every response, and TLS (spec §5.1): `GLIDEX_UI_TLS_CERT` +
//! `GLIDEX_UI_TLS_KEY` (or the `ui-tls-key` systemd credential), else a
//! self-signed certificate. `GLIDEX_UI_TLS=off` serves plain HTTP, only on
//! loopback addresses.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, Version};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use hyper_util::rt::TokioIo;
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
const DEFAULT_API_URL: &str = "https://127.0.0.1:8841";
const PORT: u16 = 5173;

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
                 Environment:\n  GLIDEX_UI_DIR        built UI (default: ui/dist)\n  GLIDEX_UI_LISTEN     addresses, comma-separated (default 0.0.0.0:{PORT},[::]:{PORT})\n  \
                 GLIDEX_UI_HOSTS      allowed Host values, host[:port],... (default: this host's names and addresses)\n  \
                 GLIDEX_UI_TLS        auto (HTTPS, default) or off (plain HTTP, loopback addresses only)\n  \
                 GLIDEX_UI_TLS_CERT   PEM certificate chain (default: a self-signed one)\n  GLIDEX_UI_TLS_KEY    PEM key (or the ui-tls-key systemd credential)\n  \
                 GLIDEX_API_SOCKET    control plane ui.sock (default {DEFAULT_API_SOCKET}, used when it exists)\n  \
                 GLIDEX_API_URL       control plane over TCP otherwise (default {DEFAULT_API_URL})\n  \
                 GLIDEX_API_CA_CERT   PEM file to trust for GLIDEX_API_URL (the local control plane's is trusted already)"
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

/// This host's names and addresses, any port (spec §5.6).
fn default_host_rules(names: &glidex_tls::LocalNames) -> Vec<HostRule> {
    names.hosts().into_iter().map(|host| HostRule { host, port: None }).collect()
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

/// `hsts`: TLS with a configured certificate. Never with a self-signed
/// one: browsers make certificate errors non-bypassable for an HSTS host
/// (spec §5.1.1).
fn add_security_headers(h: &mut HeaderMap, hsts: bool) {
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    if hsts {
        h.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    }
}

async fn security_headers(State(hsts): State<bool>, req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    add_security_headers(resp.headers_mut(), hsts);
    resp
}

// ---- proxy -----------------------------------------------------------------

/// The control plane over TCP: `host`, `port` and whether it is `https://`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TcpSpec {
    host: String,
    port: u16,
    tls: bool,
}

/// Where the control plane may be. Which one a request goes to is decided
/// per request (`Upstream::send`), because the UI may start before the
/// control plane has created `ui.sock` or published its certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UpstreamSpec {
    /// `ui.sock`, used whenever it exists.
    socket: Option<PathBuf>,
    /// Otherwise TCP.
    tcp: Option<TcpSpec>,
}

/// - neither set (development): the default `ui.sock`, else
///   `https://127.0.0.1:8841`;
/// - `GLIDEX_API_SOCKET` alone (the packaged unit): that socket only. Until
///   it exists requests get `502`; they never fall back to TCP, which is a
///   different trust path (spec/security.md §5.6);
/// - `GLIDEX_API_URL` alone: that URL only, skipping the default socket, so
///   a development UI can point at a scratch control plane on a host that
///   also runs glidex;
/// - both: the socket when it exists, else the URL.
fn upstream_spec(socket: Option<&str>, url: Option<&str>) -> Result<UpstreamSpec, String> {
    let socket_path = match (socket, url) {
        (Some(s), _) => Some(PathBuf::from(s)),
        (None, None) => Some(PathBuf::from(DEFAULT_API_SOCKET)),
        (None, Some(_)) => None,
    };
    let tcp_url = match (socket, url) {
        (_, Some(u)) => Some(u),
        (None, None) => Some(DEFAULT_API_URL),
        (Some(_), None) => None,
    };
    let tcp = match tcp_url {
        None => None,
        Some(url) => {
            let api: Uri = url.parse().map_err(|e| format!("GLIDEX_API_URL: {}", e))?;
            let tls = match api.scheme_str() {
                Some("https") => true,
                Some("http") => false,
                _ => return Err("GLIDEX_API_URL must be https://host:port or http://host:port".into()),
            };
            let host = api.host().ok_or("GLIDEX_API_URL has no host")?.to_string();
            let port = api.port_u16().unwrap_or(if tls { 443 } else { 80 });
            if !tls && !is_loopback_host(&host) {
                return Err("GLIDEX_API_URL: plain http is only allowed to a loopback address; use https://".into());
            }
            Some(TcpSpec { host, port, tls })
        }
    };
    Ok(UpstreamSpec { socket: socket_path, tcp })
}

fn is_loopback_host(h: &str) -> bool {
    let h = h.trim_start_matches('[').trim_end_matches(']');
    h == "localhost" || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Trust for an `https://` upstream: the system store,
/// `GLIDEX_API_CA_CERT`, and for a loopback host the local control plane's
/// published certificate. Rebuilt whenever those files appear, change or
/// go away, so a control plane that (re)generates its certificate after
/// the UI started is trusted without restarting the UI.
#[derive(Clone)]
struct UpstreamTrust {
    ca: Option<PathBuf>,
    published: bool,
    cache: Arc<std::sync::Mutex<Option<TrustCache>>>,
}

/// The files a client config was built from, and the config.
type TrustCache = (Vec<FileStamp>, glidex_tls::ClientTls);

/// A trusted file as last seen: path, modification time and length.
type FileStamp = (PathBuf, Option<std::time::SystemTime>, Option<u64>);

impl UpstreamTrust {
    fn new(host: &str, ca: Option<PathBuf>) -> Self {
        UpstreamTrust { ca, published: is_loopback_host(host), cache: Arc::default() }
    }

    fn files(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.ca.iter().cloned().collect();
        if self.published {
            v.extend(glidex_tls::published_certs());
        }
        v
    }

    fn get(&self) -> Result<glidex_tls::ClientTls, String> {
        let files = self.files();
        let stamps: Vec<FileStamp> = files
            .iter()
            .map(|p| {
                let m = std::fs::metadata(p).ok();
                (p.clone(), m.as_ref().and_then(|m| m.modified().ok()), m.map(|m| m.len()))
            })
            .collect();
        let mut cache = self.cache.lock().unwrap();
        if let Some((seen, tls)) = cache.as_ref() {
            if *seen == stamps {
                return Ok(tls.clone());
            }
        }
        let tls = glidex_tls::ClientTls::new(&files, &[b"http/1.1"]).map_err(|e| format!("GLIDEX_API_CA_CERT: {}", e))?;
        *cache = Some((stamps, tls.clone()));
        Ok(tls)
    }
}

#[derive(Clone)]
struct TcpUpstream {
    host: String,
    port: u16,
    tls: Option<UpstreamTrust>,
    authority: HeaderValue,
}

#[derive(Clone)]
struct Upstream {
    socket: Option<PathBuf>,
    tcp: Option<TcpUpstream>,
}

/// One HTTP/1.1 exchange over `io`, upgrades (the console) included.
async fn exchange_on<I>(io: I, req: Request) -> Result<hyper::Response<hyper::body::Incoming>, String>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake::<_, Body>(TokioIo::new(io))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            tracing::debug!("upstream connection: {}", e);
        }
    });
    sender.send_request(req).await.map_err(|e| e.to_string())
}

impl Upstream {
    fn new(spec: UpstreamSpec, ca: Option<String>) -> Result<Self, String> {
        let tcp = match spec.tcp {
            None => None,
            Some(TcpSpec { host, port, tls }) => {
                let bare = host.trim_start_matches('[').trim_end_matches(']');
                let authority = if bare.contains(':') { format!("[{}]:{}", bare, port) } else { format!("{}:{}", bare, port) };
                Some(TcpUpstream {
                    tls: tls.then(|| UpstreamTrust::new(&host, ca.map(PathBuf::from))),
                    host: bare.to_string(),
                    port,
                    authority: HeaderValue::from_str(&authority).map_err(|e| e.to_string())?,
                })
            }
        };
        Ok(Upstream { socket: spec.socket, tcp })
    }

    fn describe_tcp(t: &TcpUpstream) -> String {
        format!("{}://{}", if t.tls.is_some() { "https" } else { "http" }, t.authority.to_str().unwrap_or_default())
    }

    /// Both candidates, for the startup log.
    fn describe(&self) -> String {
        let s = self.socket.as_ref().map(|p| format!("unix:{}", p.display()));
        let t = self.tcp.as_ref().map(Self::describe_tcp);
        match (s, t) {
            (Some(s), Some(t)) => format!("{} when it exists, else {}", s, t),
            (Some(x), None) | (None, Some(x)) => x,
            (None, None) => "nothing".into(),
        }
    }

    /// Where this request goes: the socket if it exists now, else TCP.
    fn target(&self) -> Result<Result<&Path, &TcpUpstream>, String> {
        match (&self.socket, &self.tcp) {
            (Some(p), _) if p.exists() => Ok(Ok(p)),
            (_, Some(t)) => Ok(Err(t)),
            (Some(p), None) => Err(format!("{} does not exist (is glidex-control-plane running?)", p.display())),
            (None, None) => Err("no control plane configured".into()),
        }
    }

    /// The URL or socket a request would go to now, for error messages.
    fn describe_target(&self) -> String {
        match self.target() {
            Ok(Ok(p)) => format!("unix:{}", p.display()),
            Ok(Err(t)) => Self::describe_tcp(t),
            Err(_) => self.describe(),
        }
    }

    async fn send(&self, mut req: Request) -> Result<hyper::Response<hyper::body::Incoming>, String> {
        let path = upstream_path(req.uri());
        *req.uri_mut() = path.parse().map_err(|e| format!("{}", e))?;
        match self.target()? {
            Err(TcpUpstream { host, port, tls, authority }) => {
                req.headers_mut().insert(header::HOST, authority.clone());
                let tcp = tokio::net::TcpStream::connect((host.as_str(), *port)).await.map_err(|e| e.to_string())?;
                let _ = tcp.set_nodelay(true);
                match tls {
                    None => exchange_on(tcp, req).await,
                    Some(trust) => {
                        let t = trust.get()?;
                        let name = rustls_pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
                        let s = glidex_tls::tokio_rustls::TlsConnector::from(t.config.clone())
                            .connect(name, tcp)
                            .await
                            .map_err(|e| match t.rejected_fingerprint() {
                                Some(fp) => format!("TLS: {} (certificate SHA-256 {}; set GLIDEX_API_CA_CERT)", e, fp),
                                None => format!("TLS: {}", e),
                            })?;
                        exchange_on(s, req).await
                    }
                }
            }
            Ok(sock) => {
                req.headers_mut().insert(header::HOST, HeaderValue::from_static("localhost"));
                let stream = tokio::net::UnixStream::connect(sock).await.map_err(|e| e.to_string())?;
                exchange_on(stream, req).await
            }
        }
    }
}

#[derive(Clone)]
struct AppState {
    upstream: Upstream,
    /// The UI itself serves TLS.
    tls: bool,
    /// ... with a configured certificate: send HSTS.
    hsts: bool,
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
                format!("control plane unreachable at {}: {}", s.upstream.describe_target(), e),
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
    let hsts = state.hsts;
    // Unknown paths get index.html: they are client-side routes.
    Router::new()
        .route("/api", any(proxy_api))
        .route("/api/{*rest}", any(proxy_api))
        .fallback_service(ServeDir::new(dist).fallback(ServeFile::new(&index)))
        .with_state(state)
        .layer(from_fn_with_state(Arc::new(hosts), host_check))
        .layer(from_fn_with_state(hsts, security_headers))
}

// ---- listener and TLS --------------------------------------------------------

/// Refuse plain HTTP on a non-loopback address (spec §5.1).
fn check_listen(addrs: &[SocketAddr], tls: bool) -> Result<(), String> {
    match addrs.iter().find(|a| !tls && !a.ip().is_loopback()) {
        None => Ok(()),
        Some(a) => Err(format!(
            "refusing to serve plain HTTP on {}: GLIDEX_UI_TLS=off needs every GLIDEX_UI_LISTEN address on loopback",
            a
        )),
    }
}

/// Configured certificate and key paths, if any.
fn tls_paths(cert: Option<String>, key: Option<String>, credential: Option<PathBuf>) -> Result<Option<(PathBuf, PathBuf)>, String> {
    match (cert, key.map(PathBuf::from).or(credential)) {
        (None, None) => Ok(None),
        (Some(c), Some(k)) => Ok(Some((PathBuf::from(c), k))),
        (Some(_), None) => Err("GLIDEX_UI_TLS_CERT needs GLIDEX_UI_TLS_KEY or the ui-tls-key credential".into()),
        (None, Some(_)) => Err("a TLS key is set but GLIDEX_UI_TLS_CERT is not".into()),
    }
}

/// `GLIDEX_UI_TLS`: `auto` (default) or `off`.
fn tls_enabled(v: Option<String>) -> Result<bool, String> {
    match v.as_deref() {
        None | Some("auto") => Ok(true),
        Some("off") => Ok(false),
        Some(o) => Err(format!("GLIDEX_UI_TLS: '{}' is not auto or off", o)),
    }
}

fn credential(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("CREDENTIALS_DIRECTORY")?).join(name);
    p.exists().then_some(p)
}

/// Where the self-signed certificate lives: `$STATE_DIRECTORY/tls`
/// (systemd `StateDirectory=glidex-ui`), else `~/.glidex/ui-tls`.
fn self_signed_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("STATE_DIRECTORY") {
        let first = d.to_string_lossy().split(':').next().unwrap_or_default().to_string();
        if !first.is_empty() {
            return PathBuf::from(first).join("tls");
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".glidex").join("ui-tls")
}

/// The certificate the UI serves: configured, else self-signed.
struct UiCert {
    cert: PathBuf,
    key: PathBuf,
    self_signed: bool,
    generated: bool,
}

fn ui_cert() -> Result<UiCert, String> {
    match tls_paths(env("GLIDEX_UI_TLS_CERT"), env("GLIDEX_UI_TLS_KEY"), credential("ui-tls-key"))? {
        Some((cert, key)) => Ok(UiCert { cert, key, self_signed: false, generated: false }),
        None => {
            let s = glidex_tls::ensure_self_signed(&self_signed_dir(), "ui", &glidex_tls::LocalNames::discover())?;
            Ok(UiCert { cert: s.cert, key: s.key, self_signed: true, generated: s.generated })
        }
    }
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

/// Plain HTTP on the HTTPS port: `308` to the same URL over `https://`
/// (allowed `Host` values only; others get `421`).
fn redirect_router(hosts: Arc<Vec<HostRule>>) -> Router {
    Router::new()
        .fallback(|req: Request| async move {
            let host = request_host(&req).unwrap_or_default();
            let pq = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/").to_string();
            let mut resp = match HeaderValue::from_str(&format!("https://{}{}", host, pq)) {
                Ok(loc) => (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, loc)], "Use https://\n").into_response(),
                Err(_) => StatusCode::BAD_REQUEST.into_response(),
            };
            add_security_headers(resp.headers_mut(), false);
            resp
        })
        .layer(from_fn_with_state(hosts, host_check))
}

/// A TLS handshake starts with a handshake record (`0x16`); anything else
/// on an HTTPS port is taken for plain HTTP.
async fn is_tls(stream: &tokio::net::TcpStream) -> bool {
    let mut b = [0u8; 1];
    matches!(stream.peek(&mut b).await, Ok(1) if b[0] == 0x16)
}

async fn accept_loop(
    listener: tokio::net::TcpListener,
    router: Router,
    tls: Option<tokio_rustls::TlsAcceptor>,
    redirect: Router,
) {
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("accept: {}", e);
                continue;
            }
        };
        let router = router.clone();
        let redirect = redirect.clone();
        match tls.clone() {
            Some(acceptor) => {
                tokio::spawn(async move {
                    let sniff = tokio::time::timeout(std::time::Duration::from_secs(10), is_tls(&stream)).await;
                    match sniff {
                        Ok(true) => {}
                        Ok(false) => return serve_conn(stream, redirect, addr).await,
                        Err(_) => return tracing::debug!(%addr, "no request"),
                    }
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
    let listen = match env("GLIDEX_UI_LISTEN") {
        Some(l) => glidex_tls::parse_addresses(&l, "GLIDEX_UI_LISTEN")?,
        None => glidex_tls::all_addresses(PORT),
    };
    let tls_on = tls_enabled(env("GLIDEX_UI_TLS"))?;
    check_listen(&listen, tls_on)?;
    let cert = if tls_on { Some(ui_cert()?) } else { None };
    let tls = match &cert {
        Some(c) => Some(tokio_rustls::TlsAcceptor::from(glidex_tls::server_config(&c.cert, &c.key, &[b"http/1.1"])?)),
        None => None,
    };
    let hosts = match env("GLIDEX_UI_HOSTS") {
        Some(list) => parse_host_rules(&list)?,
        None => default_host_rules(&glidex_tls::LocalNames::discover()),
    };
    let spec = upstream_spec(env("GLIDEX_API_SOCKET").as_deref(), env("GLIDEX_API_URL").as_deref())?;
    let upstream = Upstream::new(spec, env("GLIDEX_API_CA_CERT"))?;
    let describe = upstream.describe();
    let hsts = cert.as_ref().is_some_and(|c| !c.self_signed);
    let redirect = redirect_router(Arc::new(hosts.clone()));
    let router = app(&dist, AppState { upstream, tls: tls_on, hsts }, hosts);

    let mut listeners = Vec::new();
    for a in &listen {
        match glidex_tls::bind(*a) {
            Ok(l) => listeners.push(l),
            Err(e) if glidex_tls::ipv6_unavailable(a, &e) => tracing::info!("IPv6 unavailable; not listening on {}", a),
            Err(e) => return Err(format!("bind {}: {}", a, e)),
        }
    }
    if listeners.is_empty() {
        return Err("no usable GLIDEX_UI_LISTEN address".into());
    }
    let scheme = if tls_on { "https" } else { "http" };
    for l in &listeners {
        tracing::info!("GlideX UI on {}://{} (from {}, API {})", scheme, l.local_addr().map_err(|e| e.to_string())?, dist.display(), describe);
    }
    if let Some(c) = &cert {
        tracing::info!(
            cert = %c.cert.display(),
            fingerprint = %glidex_tls::fingerprint(&c.cert)?,
            self_signed = c.self_signed,
            generated = c.generated,
            "TLS certificate"
        );
    }
    let loops: Vec<_> = listeners
        .into_iter()
        .map(|l| tokio::spawn(accept_loop(l, router.clone(), tls.clone(), redirect.clone())))
        .collect();
    shutdown_signal().await;
    for l in loops {
        l.abort();
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

/// Vite with hot reload, over HTTPS with the UI's certificate (configured
/// or self-signed), proxying to the control plane it verifies against the
/// published certificate (spec/web-ui.md).
async fn run_dev_server() {
    let ui_path = ui_dir();
    tracing::info!("Starting UI dev server from {}", ui_path.display());

    let mut cmd = Command::new("bun");
    cmd.arg("run").arg("dev").current_dir(&ui_path).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let scheme = match tls_enabled(env("GLIDEX_UI_TLS")) {
        Ok(true) => match ui_cert() {
            Ok(c) => {
                cmd.env("GLIDEX_UI_TLS_CERT", &c.cert).env("GLIDEX_UI_TLS_KEY", &c.key);
                "https"
            }
            Err(e) => {
                tracing::error!("TLS: {}", e);
                std::process::exit(1);
            }
        },
        Ok(false) => "http",
        Err(e) => {
            tracing::error!("{}", e);
            std::process::exit(1);
        }
    };
    if env("GLIDEX_API_CA_CERT").is_none() {
        if let Some(p) = glidex_tls::published_certs().into_iter().next() {
            cmd.env("GLIDEX_API_CA_CERT", p);
        }
    }
    let mut child = cmd.spawn().expect("Failed to start bun dev server. Is bun installed?");

    tracing::info!("GlideX UI available at {}://localhost:{}", scheme, PORT);

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

    fn local() -> glidex_tls::LocalNames {
        glidex_tls::LocalNames {
            dns: vec!["glidex.example.org".into(), "localhost".into()],
            ips: vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap(), "10.0.0.5".parse().unwrap()],
        }
    }

    #[test]
    fn host_matching() {
        let d = default_host_rules(&local());
        for ok in ["glidex.example.org:5173", "10.0.0.5", "[::1]"] {
            assert!(host_allowed(&d, ok), "{ok}");
        }
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
    fn security_headers_and_hsts_only_with_a_configured_certificate() {
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
    fn plain_http_only_on_loopback() {
        let lo: SocketAddr = "127.0.0.1:5173".parse().unwrap();
        let lo6: SocketAddr = "[::1]:5173".parse().unwrap();
        let any: SocketAddr = "0.0.0.0:5173".parse().unwrap();
        let lan: SocketAddr = "10.0.0.5:443".parse().unwrap();
        assert!(check_listen(&[lo, lo6], false).is_ok());
        let e = check_listen(&[lo, any], false).unwrap_err();
        assert!(e.contains("plain HTTP"), "{e}");
        assert!(check_listen(&[lan], false).is_err());
        assert!(check_listen(&[lan, any], true).is_ok());
        assert!(tls_enabled(None).unwrap());
        assert!(tls_enabled(Some("auto".into())).unwrap());
        assert!(!tls_enabled(Some("off".into())).unwrap());
        assert!(tls_enabled(Some("on".into())).is_err());
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
        let tcp = |host: &str, port, tls| Some(TcpSpec { host: host.into(), port, tls });
        let spec = |socket: Option<&str>, tcp| UpstreamSpec { socket: socket.map(PathBuf::from), tcp };
        // Development: the default socket, else the default URL.
        assert_eq!(upstream_spec(None, None).unwrap(), spec(Some(DEFAULT_API_SOCKET), tcp("127.0.0.1", 8841, true)));
        // The packaged unit: the socket only, never TCP.
        assert_eq!(upstream_spec(Some("/s"), None).unwrap(), spec(Some("/s"), None));
        assert_eq!(upstream_spec(Some("/s"), Some("https://h:1")).unwrap(), spec(Some("/s"), tcp("h", 1, true)));
        // An explicit URL alone doesn't go through the default socket.
        assert_eq!(upstream_spec(None, Some("http://127.0.0.1:1")).unwrap(), spec(None, tcp("127.0.0.1", 1, false)));
        assert_eq!(upstream_spec(None, Some("https://h")).unwrap(), spec(None, tcp("h", 443, true)));
        // Plain http only to loopback.
        assert!(upstream_spec(None, Some("http://h:1")).is_err());
        assert!(upstream_spec(None, Some("ftp://h:1")).is_err());
    }

    /// The UI may start before the control plane: a socket that appears
    /// later is used from then on (it used to fall back to TCP for good).
    #[tokio::test]
    async fn socket_created_after_startup_is_used() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("ui.sock");
        let up = Upstream::new(upstream_spec(Some(sock.to_str().unwrap()), None).unwrap(), None).unwrap();
        let req = || Request::builder().uri("/api/health").body(Body::empty()).unwrap();
        let e = up.send(req()).await.unwrap_err();
        assert!(e.contains("does not exist"), "{e}");

        let l = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move { axum::serve(l, Router::new().fallback(|| async { "up" })).await.unwrap() });
        let resp = up.send(req()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// A trusted certificate file that appears or changes after startup is
    /// picked up on the next request.
    #[test]
    fn upstream_trust_follows_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let names = local();
        let a = glidex_tls::ensure_self_signed(&dir.path().join("a"), "a", &names).unwrap();
        let b = glidex_tls::ensure_self_signed(&dir.path().join("b"), "b", &names).unwrap();
        let ca = dir.path().join("ca.crt");
        let trust = UpstreamTrust::new("h.example", Some(ca.clone()));
        assert!(trust.get().is_err(), "missing file");
        std::fs::copy(&a.cert, &ca).unwrap();
        let t1 = trust.get().unwrap();
        assert!(Arc::ptr_eq(&t1.config, &trust.get().unwrap().config), "cached while unchanged");
        std::fs::write(&ca, [std::fs::read(&b.cert).unwrap(), std::fs::read(&a.cert).unwrap()].concat()).unwrap();
        assert!(!Arc::ptr_eq(&t1.config, &trust.get().unwrap().config), "rebuilt after a change");
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

        let spec = UpstreamSpec { socket: Some(sock), tcp: None };
        let state = AppState { upstream: Upstream::new(spec, None).unwrap(), tls: false, hsts: false };
        let router = app(dir.path(), state, default_host_rules(&local()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(accept_loop(listener, router, None, Router::new()));

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

    /// HTTPS with a self-signed certificate: served to a client that
    /// trusts it, no HSTS, plain HTTP on the same port redirected, and an
    /// https:// upstream verified against the control plane's certificate.
    #[tokio::test]
    async fn https_redirect_and_https_upstream() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("index.html"), "<html>ui</html>").unwrap();
        let names = local();

        // A fake control plane over HTTPS.
        let cp = glidex_tls::ensure_self_signed(&dir.path().join("cp"), "cp", &names).unwrap();
        let cp_acceptor = tokio_rustls::TlsAcceptor::from(glidex_tls::server_config(&cp.cert, &cp.key, &[b"http/1.1"]).unwrap());
        let cp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cp_port = cp_listener.local_addr().unwrap().port();
        let echo = Router::new().fallback(|req: Request| async move { format!("upstream path={}", req.uri()) });
        tokio::spawn(accept_loop(cp_listener, echo, Some(cp_acceptor), Router::new()));
        let spec = UpstreamSpec { socket: None, tcp: Some(TcpSpec { host: "localhost".into(), port: cp_port, tls: true }) };
        let upstream = Upstream::new(spec, Some(cp.cert.to_string_lossy().into_owned())).unwrap();

        let ui = glidex_tls::ensure_self_signed(&dir.path().join("ui"), "ui", &names).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(glidex_tls::server_config(&ui.cert, &ui.key, &[b"http/1.1"]).unwrap());
        let hosts = default_host_rules(&names);
        let router = app(dir.path(), AppState { upstream, tls: true, hsts: false }, hosts.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(accept_loop(listener, router, Some(acceptor), redirect_router(Arc::new(hosts))));

        // Plain HTTP: redirected; a foreign Host isn't.
        let out = exchange(addr, "GET /vms?x=1 HTTP/1.1\r\nHost: glidex.example.org:5173\r\nConnection: close\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 308"), "{out}");
        assert!(out.to_ascii_lowercase().contains("location: https://glidex.example.org:5173/vms?x=1"), "{out}");
        let out = exchange(addr, "GET / HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n").await;
        assert!(out.starts_with("HTTP/1.1 421"), "{out}");

        // HTTPS, trusting the UI's certificate.
        let client = glidex_tls::ClientTls::new(std::slice::from_ref(&ui.cert), &[b"http/1.1"]).unwrap();
        let https = |path: &'static str| {
            let client = client.clone();
            async move {
                let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
                let name = rustls_pki_types::ServerName::try_from("localhost").unwrap();
                let mut s = tokio_rustls::TlsConnector::from(client.config.clone()).connect(name, tcp).await.unwrap();
                s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost:5173\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                let mut out = String::new();
                let _ = s.read_to_string(&mut out).await;
                out
            }
        };
        let out = https("/").await;
        assert!(out.starts_with("HTTP/1.1 200"), "{out}");
        assert!(out.contains("<html>ui</html>"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("strict-transport-security"), "{out}");
        let out = https("/api/vms").await;
        assert!(out.contains("upstream path=/vms"), "{out}");
    }
}
