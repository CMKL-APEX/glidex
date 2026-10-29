//! gxctl's API client (spec/01-cli.md "Transport").
//!
//! Requests go over HTTP/1.1, either on the control plane's Unix socket
//! `api.sock`, where the server identifies the caller by peer uid and no
//! credentials are sent (spec/11-security.md §5.2), or over TCP (TLS for
//! `https://`) with `Authorization: Bearer <token>`. One connection per
//! request; a streaming call (the console) gets an HTTP/2 connection of its
//! own, made the same way. The token is never printed, logged or put in a
//! URL.

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderValue};
use hyper::{Method, Request, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use std::fmt;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::io::{AsyncRead, AsyncWrite};
use zeroize::Zeroizing;

/// TCP fallback when no socket exists and no `--url` was given.
pub const DEFAULT_URL: &str = "https://localhost:8841";

/// A connected byte stream: Unix socket, TCP, or TLS over TCP.
pub trait Conn: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Conn for T {}

// ---- socket selection ------------------------------------------------------

/// Where the control plane's `api.sock` usually is, in order: the systemd
/// unit's runtime directory, then the run directories a control plane
/// started by hand uses (`paths::run_dir`).
/// The UI socket every node serves; on an agent it is the only one.
pub const AGENT_UI_SOCKET: &str = "/run/glidex-cp/ui.sock";

/// What `gxctl` says on a cluster agent, which has no API of its own.
pub const AGENT_HINT: &str = "this host is a cluster agent: it has no local API, and the cluster can't vouch for a Unix user of another host. Log in to a server instead: `gxctl auth login --url https://<server>:8841` (spec/17-gxctl-auth.md)";

pub fn socket_candidates(xdg_runtime_dir: Option<&str>, euid: u32) -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/run/glidex-cp/api.sock")];
    if let Some(x) = xdg_runtime_dir.filter(|x| !x.is_empty()) {
        v.push(Path::new(x).join("glidex").join("api.sock"));
    }
    v.push(PathBuf::from(format!("/tmp/glidex-{}/api.sock", euid)));
    v
}

/// `explicit` (`--socket` / `GLIDEX_SOCKET`) wins; else the first
/// candidate that `exists`.
pub fn select_socket(
    explicit: Option<PathBuf>,
    candidates: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(p) = explicit.filter(|p| !p.as_os_str().is_empty()) {
        return Some(p);
    }
    candidates.iter().find(|p| exists(p)).cloned()
}

pub fn is_socket(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

// ---- the saved token -------------------------------------------------------

/// `~/.config/glidex/token` (honours `XDG_CONFIG_HOME`).
pub fn token_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("glidex").join("token"))
}

/// Read a saved token. Refuses a file that group or others can read, or
/// that someone else owns: the token is as good as a password.
pub fn read_token_file(path: &Path) -> Result<Option<Zeroizing<String>>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {}", path.display(), e)),
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "refusing to use the token in {}: it is accessible by group or others (mode {:03o}). \
             Treat the token as exposed: revoke it ('token revoke <id>'), delete the file and log in again \
             (or run 'chmod 600 {}' if you are sure nobody read it)",
            path.display(),
            mode,
            path.display()
        ));
    }
    let euid = nix::unistd::geteuid().as_raw();
    if meta.uid() != euid {
        return Err(format!(
            "refusing to use the token in {}: it is owned by another user, not you",
            path.display()
        ));
    }
    let text = Zeroizing::new(
        std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path.display(), e))?,
    );
    let t = text.trim();
    Ok((!t.is_empty()).then(|| Zeroizing::new(t.to_string())))
}

/// Save a token: directory 0700, file 0600, written to a temporary file
/// and renamed so a reader never sees a partial or wider-mode file.
pub fn save_token_file(path: &Path, token: &str) -> Result<(), String> {
    let dir = path.parent().ok_or("token path has no directory")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("{}: {}", dir.display(), e))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {}", dir.display(), e))?;
    let tmp = dir.join(format!(".token.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let write = || -> io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(token.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {}", path.display(), e)
    })
}

/// Delete the saved token; `Ok(false)` when there was none.
pub fn delete_token_file(path: &Path) -> Result<bool, String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {}", path.display(), e)),
    }
}

/// The token for TCP: `GLIDEX_TOKEN`, else the token file.
pub fn load_token() -> Result<Option<Zeroizing<String>>, String> {
    if let Some(t) = token_from_env() {
        return Ok(Some(t));
    }
    match token_path() {
        Some(p) => read_token_file(&p),
        None => Ok(None),
    }
}

/// Just the `GLIDEX_TOKEN` environment variable — the top of the
/// credential ladder (17-gxctl-auth.md §3.4) when a profile, not the legacy
/// file, is in effect.
pub fn token_from_env() -> Option<Zeroizing<String>> {
    match std::env::var("GLIDEX_TOKEN") {
        Ok(t) if !t.trim().is_empty() => Some(Zeroizing::new(t.trim().to_string())),
        _ => None,
    }
}

// ---- principals and roles in CLI syntax ------------------------------------

/// `user:<id>`, `team:<id>`, `token:<id>` → the API's entity JSON
/// (`{"type": "User", "id": …}`). Team ids may contain colons
/// (`team:unix:glidex-admin`).
pub fn parse_principal(s: &str) -> Result<serde_json::Value, String> {
    let (kind, id) = s.split_once(':').ok_or_else(|| {
        format!(
            "'{}': write a principal as user:<id>, team:<id> or token:<id>",
            s
        )
    })?;
    let ty = match kind {
        "user" => "User",
        "team" => "Team",
        "token" => "Token",
        _ => {
            return Err(format!(
                "'{}': a principal is user:<id>, team:<id> or token:<id>",
                s
            ));
        }
    };
    if id.is_empty() {
        return Err(format!("'{}': the id is missing", s));
    }
    Ok(serde_json::json!({ "type": ty, "id": id }))
}

/// An entity (`EntityRef`'s type and id) → `user:<id>` etc. (`host` for
/// the host).
pub fn format_entity(ty: &str, id: Option<&str>) -> String {
    let ty = if ty.is_empty() {
        "?".to_string()
    } else {
        ty.to_lowercase()
    };
    match id {
        Some(id) => format!("{}:{}", ty, id),
        None => ty,
    }
}

/// `owner` → `role.owner`; names with a prefix (`grant.host-paths`) stay.
pub fn role_name(r: &str) -> String {
    if r.contains('.') {
        r.to_string()
    } else {
        format!("role.{}", r)
    }
}

/// `ROLE[@PROJECT]` → the role name and the project.
pub fn parse_role_ref(s: &str) -> Result<(String, Option<String>), String> {
    let (role, project) = match s.split_once('@') {
        Some((r, p)) if !p.is_empty() => (r, Some(p)),
        Some(_) => return Err(format!("'{}': project missing after @", s)),
        None => (s, None),
    };
    if role.is_empty() {
        return Err(format!("'{}': role missing", s));
    }
    Ok((role_name(role), project.map(str::to_string)))
}

/// Percent-encode a query or path value.
pub fn enc(s: &str) -> String {
    const SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    percent_encoding::utf8_percent_encode(s, SET).to_string()
}

/// Append `key=value` to a path's query string.
pub fn add_query(path: &str, key: &str, value: &str) -> String {
    let sep = if path.contains('?') { '&' } else { '?' };
    format!("{}{}{}={}", path, sep, key, enc(value))
}

// ---- errors ----------------------------------------------------------------

/// A failed request: transport trouble or an API error, as a message.
#[derive(Debug, Clone)]
pub struct Failure {
    pub message: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Failure {
    fn transport(message: String) -> Self {
        Failure { message }
    }
}

/// Render an error response (`{"error": code, "message": …, "details": …}`)
/// for people. `tcp`: whether `gxctl login` would help.
pub fn render_error(status: StatusCode, body: &[u8], tcp: bool) -> Failure {
    let v: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let (code, message) = match v.as_ref().and_then(|v| v["error"].as_str().map(|c| (c, v))) {
        Some((c, v)) => (
            Some(c.to_string()),
            v["message"].as_str().unwrap_or("").to_string(),
        ),
        None => {
            let text = String::from_utf8_lossy(body);
            let text: String = text.trim().chars().take(300).collect();
            (
                None,
                if text.is_empty() {
                    status.canonical_reason().unwrap_or("").to_string()
                } else {
                    text
                },
            )
        }
    };
    let details = v
        .as_ref()
        .map(|v| v["details"].clone())
        .unwrap_or(serde_json::Value::Null);
    let mut msg = match (status, code.as_deref()) {
        (StatusCode::UNAUTHORIZED, Some("reauth_required")) => {
            format!(
                "log in again: {}{}",
                message,
                if tcp { " (gxctl login)" } else { "" }
            )
        }
        (StatusCode::UNAUTHORIZED, Some("unauthenticated")) if tcp => {
            format!(
                "not logged in: {} (run `gxctl login --oidc` or `gxctl login --token`, or set GLIDEX_TOKEN)",
                message
            )
        }
        (StatusCode::FORBIDDEN, Some(c)) => format!("permission denied ({}): {}", c, message),
        (_, Some(c)) => format!("{}: {}", c, message),
        (_, None) => format!("HTTP {}: {}", status.as_u16(), message),
    };
    if let Some(impact) = details.get("impact").and_then(|v| v.as_str()) {
        msg.push_str(&format!(
            "\n  Impact: {}\n  Re-run with --force to proceed.",
            impact
        ));
    }
    if let Some(min) = details.get("min_size_bytes").and_then(|v| v.as_u64()) {
        msg.push_str(&format!(
            "\n  Minimum size: {} ({} bytes)",
            crate::format_bytes(min),
            min
        ));
    }
    if let Some(missing) = details.get("missing").and_then(|v| v.as_array()) {
        let items: Vec<&str> = missing.iter().filter_map(|m| m.as_str()).collect();
        msg.push_str(&format!("\n  Missing on this host: {}", items.join(", ")));
    }
    if let Some(over) = details.get("quota").and_then(|v| v.as_array()) {
        for o in over {
            msg.push_str(&format!(
                "\n  Quota {}: limit {}, used {}, requested {}",
                o["resource"].as_str().unwrap_or("?"),
                o["limit"],
                o["used"],
                o["requested"]
            ));
        }
    }
    if let Some(errors) = details.get("errors").and_then(|v| v.as_array()) {
        for e in errors {
            msg.push_str(&format!(
                "\n  {}",
                e.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| e.to_string())
            ));
        }
    }
    Failure { message: msg }
}

// ---- the client ------------------------------------------------------------

/// One TCP endpoint of a profile: any node of the same cluster serves the
/// API (14-clustering.md §8), and the profile lists the ones that should.
#[derive(Clone)]
pub struct TcpEp {
    pub tls: Option<glidex_tls::ClientTls>,
    pub host: String,
    pub port: u16,
    /// Path prefix of the URL, without a trailing slash.
    pub base: String,
    /// As given, for errors and for the `last_used` hint (§4.2).
    pub url: String,
}

#[derive(Clone)]
enum Endpoint {
    Unix(PathBuf),
    /// Never empty; ordered `last_used` first (§4.2).
    Tcp(Vec<TcpEp>),
}

/// Where the file-backed profile behind this client lives, so a failover
/// can rewrite `last_used` and the banner can name the profile (the
/// token's name is never part of it).
pub struct ProfileCtx {
    pub name: String,
    pub path: PathBuf,
    /// The §6.2 banner tail, composed once by `build_client`:
    /// `profile 'lab' · cluster lab · system+pinned(1) · token gxctl@lab`.
    pub banner: String,
}

/// What the profile claims about the cluster, checked against
/// `/auth/server-info` before the first command (§4.1).
pub struct Binding {
    pub profile: String,
    pub cluster_id: Option<String>,
}

pub struct Response {
    pub status: StatusCode,
    pub body: Bytes,
}

pub struct ApiClient {
    endpoint: Endpoint,
    token: Mutex<Option<Zeroizing<String>>>,
    project: Mutex<Option<String>>,
    /// Which of a multi-endpoint profile answered last; failover walks
    /// forward from it (§4.2).
    ep: Mutex<usize>,
    profile: Option<ProfileCtx>,
    binding: Option<Binding>,
    /// The TLS config of the connection that last succeeded, to read the
    /// accepted fingerprint off for the proxy cross-check (§5.2).
    last_tls: Mutex<Option<glidex_tls::ClientTls>>,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the token.
        f.debug_struct("ApiClient")
            .field("endpoint", &self.describe())
            .finish()
    }
}

pub fn is_loopback_host(h: &str) -> bool {
    let h = h.trim_start_matches('[').trim_end_matches(']');
    h == "localhost"
        || h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The TLS config for one endpoint: today's ladder (system store,
/// `GLIDEX_CA_CERT`, the published certificate on loopback) extended with
/// the profile's roots and first-use pins (spec/17-gxctl-auth.md §5.1).
/// `insecure` is the declared bypass, reached only through a profile the
/// owner confirmed with the host name typed (§5.4) or the CI-only
/// `GLIDEX_TLS_INSECURE`; every other failure keeps verification total.
fn tls_config_for(
    host: &str,
    tls: Option<&crate::config::Tls>,
) -> Result<glidex_tls::ClientTls, String> {
    let mut t = glidex_tls::Trust {
        verify_host: true,
        ..Default::default()
    };
    if let Some(ca) = std::env::var_os("GLIDEX_CA_CERT").filter(|v| !v.is_empty()) {
        t.ca_files.push(PathBuf::from(ca));
    }
    if let Some(p) = tls {
        t.pins = p.pins.clone();
        t.verify_host = p.verify_host;
        t.insecure = p.insecure;
        t.ca_pem = p.ca_pem.clone();
        if let Some(f) = &p.ca_file {
            // A file someone else could have planted must not become a CA
            // (§2.1): owner and mode are checked before it joins the store.
            let euid = nix::unistd::geteuid().as_raw();
            if !glidex_tls::trustworthy(f, euid, None) {
                return Err(format!(
                    "{}: a CA file must be yours, not group or world writable, and in a directory only you can write",
                    f.display()
                ));
            }
            t.ca_files.push(f.clone());
        }
    }
    if is_loopback_host(host) {
        t.ca_files.extend(glidex_tls::published_certs());
    }
    // `with_trust` reproduces the classic path when the profile adds
    // nothing (A9): same store, same refusal of an empty world, and —
    // 01-cli.md's invariant — there is no configuration that skips
    // verification except the audited `insecure` the profile can only
    // have reached by typing the host name.
    glidex_tls::ClientTls::with_trust(&t, &[b"http/1.1"])
}

/// A failed handshake, with what to do about an untrusted certificate.
fn tls_error(target: &str, tls: &glidex_tls::ClientTls, e: io::Error) -> String {
    match tls.rejected_fingerprint() {
        Some(fp) => format!(
            "TLS with {}: {}\n  the server's certificate has SHA-256 fingerprint {}\n  \
             if that is the control plane's (it prints it at startup), save its certificate and set GLIDEX_CA_CERT to the file\n  \
             or, from a login profile, trust this exact certificate: gxctl auth login … --pin sha256/{}",
            target,
            e,
            fp,
            fp.to_lowercase()
        ),
        None => format!("TLS with {}: {}", target, e),
    }
}

fn parse_tcp_ep(url: &str, tls: Option<&crate::config::Tls>) -> Result<TcpEp, String> {
    let uri: Uri = url.parse().map_err(|e| format!("--url {}: {}", url, e))?;
    let secure = match uri.scheme_str() {
        Some("https") => true,
        Some("http") => false,
        _ => return Err(format!("--url {}: use http:// or https://", url)),
    };
    let host = uri
        .host()
        .ok_or_else(|| format!("--url {}: no host", url))?
        .to_string();
    if !secure && !is_loopback_host(&host) {
        return Err(format!(
            "--url {}: plain http is only allowed to localhost; use https://",
            url
        ));
    }
    let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
    let base = uri.path().trim_end_matches('/').to_string();
    Ok(TcpEp {
        tls: if secure {
            Some(tls_config_for(&host, tls)?)
        } else {
            None
        },
        host,
        port,
        base,
        url: url.to_string(),
    })
}

fn ep_desc(ep: &TcpEp) -> String {
    format!(
        "{}://{}:{}{}",
        if ep.tls.is_some() { "https" } else { "http" },
        ep.host,
        ep.port,
        ep.base
    )
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl ApiClient {
    pub fn unix(path: PathBuf) -> Self {
        ApiClient {
            endpoint: Endpoint::Unix(path),
            token: Mutex::new(None),
            project: Mutex::new(None),
            ep: Mutex::new(0),
            profile: None,
            binding: None,
            last_tls: Mutex::new(None),
        }
    }

    /// `http://` or `https://` URL. A token is never sent in clear text
    /// off the loopback interface.
    pub fn tcp(url: &str, token: Option<Zeroizing<String>>) -> Result<Self, String> {
        let urls: Vec<String> = vec![url.to_string()];
        Self::tcp_multi(&urls, token, None, None, None)
    }

    /// A profile's client: every URL is one endpoint of the same cluster
    /// (A7), dialed in order with the `last_used` hint first, 5 s each
    /// (§4.2). `tls` is the profile's trust; `profile` lets a failover
    /// record what answered; `binding` arms the cluster check (§4.1).
    pub fn tcp_multi(
        urls: &[String],
        token: Option<Zeroizing<String>>,
        tls: Option<&crate::config::Tls>,
        profile: Option<ProfileCtx>,
        binding: Option<Binding>,
    ) -> Result<Self, String> {
        if urls.is_empty() {
            return Err(
                "no url to dial: the profile lists none (give --url or re-run 'auth login')".into(),
            );
        }
        let mut eps: Vec<TcpEp> = Vec::new();
        for url in urls {
            eps.push(parse_tcp_ep(url, tls)?);
        }
        // The endpoint that answered last time is tried first (A7); a
        // config that cannot be read now costs nothing here, so the hint
        // is read best-effort.
        if let Some(ctx) = &profile {
            let hint = crate::config::load_from(&ctx.path)
                .ok()
                .flatten()
                .and_then(|c| {
                    c.profiles
                        .get(&ctx.name)
                        .and_then(|p| p.last_used.as_ref())
                        .and_then(|l| l.url.clone())
                });
            if let Some(hint) = hint
                && let Some(i) = eps.iter().position(|e| e.url == hint)
            {
                let ep = eps.remove(i);
                eps.insert(0, ep);
            }
        }
        Ok(ApiClient {
            endpoint: Endpoint::Tcp(eps),
            token: Mutex::new(token),
            project: Mutex::new(None),
            ep: Mutex::new(0),
            profile,
            binding,
            last_tls: Mutex::new(None),
        })
    }

    fn ep(&self) -> Option<&TcpEp> {
        match &self.endpoint {
            // The list is never empty (tcp_multi refuses that case), so
            // the remainder is always defined.
            Endpoint::Tcp(eps) => eps.get(*self.ep.lock().unwrap() % eps.len()),
            Endpoint::Unix(_) => None,
        }
    }

    pub fn is_unix(&self) -> bool {
        matches!(self.endpoint, Endpoint::Unix(_))
    }

    /// Where requests go, for messages (never the token).
    pub fn describe(&self) -> String {
        match &self.endpoint {
            Endpoint::Unix(p) => format!("unix:{}", p.display()),
            Endpoint::Tcp(eps) => eps.iter().map(ep_desc).collect::<Vec<_>>().join(", "),
        }
    }

    /// The §6.2 banner tail of a profile-backed client, if one is.
    pub fn profile_banner(&self) -> Option<String> {
        self.profile.as_ref().map(|p| p.banner.clone())
    }

    pub fn profile_name(&self) -> Option<String> {
        self.profile.as_ref().map(|p| p.name.clone())
    }

    /// The fingerprints the last TLS attempt's ladder recorded: accepted on
    /// success, rejected on an untrusted certificate (§5.2 needs both).
    pub fn accepted_fingerprint(&self) -> Option<String> {
        self.last_tls
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|t| t.accepted_fingerprint())
    }

    pub fn rejected_fingerprint(&self) -> Option<String> {
        self.last_tls
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|t| t.rejected_fingerprint())
    }

    pub fn has_token(&self) -> bool {
        self.token.lock().unwrap().is_some()
    }

    pub fn set_token(&self, t: Option<Zeroizing<String>>) {
        *self.token.lock().unwrap() = t;
    }

    /// The credential, removed (put it back with `set_token`).
    pub fn take_token(&self) -> Option<Zeroizing<String>> {
        self.token.lock().unwrap().take()
    }

    /// The `--project` / `project use` choice for this session.
    pub fn project(&self) -> Option<String> {
        self.project.lock().unwrap().clone()
    }

    pub fn set_project(&self, p: Option<String>) {
        *self.project.lock().unwrap() = p.filter(|p| !p.is_empty());
    }

    /// One connection, walked across the profile's endpoints in order
    /// (§4.2): the first that answers wins and is remembered as
    /// `last_used`. Authorization failures are answers too — 401/403
    /// never failover, so a firewalled node cannot turn "token revoked"
    /// into three confusing errors against other servers; only transport
    /// failures move to the next endpoint. All down: one error listing
    /// each URL and its reason.
    pub async fn connect(&self) -> Result<Box<dyn Conn>, String> {
        self.connect_alpn(false).await
    }

    /// [`connect`](Self::connect) for HTTP/2: TLS offers only `h2` (with
    /// the same trust), and a server that does not agree is an error.
    pub async fn connect_h2(&self) -> Result<Box<dyn Conn>, String> {
        self.connect_alpn(true).await
    }

    async fn connect_alpn(&self, h2: bool) -> Result<Box<dyn Conn>, String> {
        match &self.endpoint {
            Endpoint::Unix(p) => match tokio::net::UnixStream::connect(p).await {
                Ok(s) => Ok(Box::new(s)),
                Err(e) => Err(match e.kind() {
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                        format!(
                            "cannot reach the control plane at {}: {} (is glidex-control-plane running?)",
                            p.display(),
                            e
                        )
                    }
                    io::ErrorKind::PermissionDenied => format!(
                        "cannot open {}: {} (local users need to be in the glidex-users group; log out and in again after being added)",
                        p.display(),
                        e
                    ),
                    _ => format!("cannot reach the control plane at {}: {}", p.display(), e),
                }),
            },
            Endpoint::Tcp(eps) => {
                let first = *self.ep.lock().unwrap() % eps.len();
                let mut errs: Vec<String> = Vec::new();
                for k in 0..eps.len() {
                    let i = (first + k) % eps.len();
                    match self.connect_one(&eps[i], eps.len() == 1, h2).await {
                        Ok(c) => {
                            if i != first {
                                self.promote(i);
                            }
                            return Ok(c);
                        }
                        Err(e) => errs.push(format!("{}: {e}", ep_desc(&eps[i]))),
                    }
                }
                Err(if errs.len() == 1 {
                    errs.remove(0)
                } else {
                    format!(
                        "no endpoint of this profile answered:\n  {}",
                        errs.join("\n  ")
                    )
                })
            }
        }
    }

    async fn connect_one(&self, ep: &TcpEp, sole: bool, h2: bool) -> Result<Box<dyn Conn>, String> {
        let bare = ep.host.trim_start_matches('[').trim_end_matches(']');
        let addr = if bare.contains(':') {
            format!("[{}]:{}", bare, ep.port)
        } else {
            format!("{}:{}", bare, ep.port)
        };
        // Today's single-endpoint dial keeps its 10 s; a profile's list
        // gets 5 s each so walking the cluster stays quick (§4.2).
        let secs = if sole { 10 } else { 5 };
        let tcp = tokio::time::timeout(
            std::time::Duration::from_secs(secs),
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| {
            format!(
                "cannot reach the control plane at {}: timed out",
                self.describe()
            )
        })?
        .map_err(|e| {
            format!(
                "cannot reach the control plane at {}: {}",
                self.describe(),
                e
            )
        })?;
        let _ = tcp.set_nodelay(true);
        match &ep.tls {
            None => Ok(Box::new(tcp)),
            Some(cfg) => {
                let name = rustls_pki_types::ServerName::try_from(bare.to_string())
                    .map_err(|e| format!("{}: {}", ep.host, e))?;
                // Named before the handshake so the fingerprints the ladder
                // recorded — accepted or rejected — are readable whichever
                // way the attempt went (the TOFU prompt and §5.3 need the
                // rejected one).
                *self.last_tls.lock().unwrap() = Some(cfg.clone());
                // HTTP/2 keeps the verifier (and so the fingerprints it
                // records); only the ALPN offer changes.
                let config = if h2 {
                    let mut c = (*cfg.config).clone();
                    c.alpn_protocols = vec![b"h2".to_vec()];
                    std::sync::Arc::new(c)
                } else {
                    cfg.config.clone()
                };
                let s = tokio_rustls::TlsConnector::from(config)
                    .connect(name, tcp)
                    .await
                    .map_err(|e| tls_error(&self.describe(), cfg, e))?;
                if h2 && s.get_ref().1.alpn_protocol() != Some(b"h2".as_slice()) {
                    return Err(format!(
                        "{} does not speak HTTP/2 over TLS, which streaming calls need",
                        self.describe()
                    ));
                }
                Ok(Box::new(s))
            }
        }
    }

    /// The winner of a failover becomes the starting point and the
    /// `last_used` hint; a config write that cannot happen now costs
    /// nothing, so the metadata note is best-effort (§4.2).
    fn promote(&self, i: usize) {
        *self.ep.lock().unwrap() = i;
        let ctx = match &self.profile {
            Some(c) => c,
            None => return,
        };
        let url = match &self.endpoint {
            Endpoint::Tcp(eps) => eps[i].url.clone(),
            Endpoint::Unix(_) => return,
        };
        let mut cfg = match crate::config::load_from(&ctx.path) {
            Ok(Some(c)) => c,
            _ => return,
        };
        if let Some(p) = cfg.profiles.get_mut(&ctx.name) {
            p.last_used = Some(crate::config::LastUsed {
                at: Some(now_secs()),
                url: Some(url),
            });
            let _ = crate::config::save_to(&ctx.path, &cfg);
        }
    }

    fn host_header(&self) -> String {
        match self.ep() {
            None => "localhost".into(),
            Some(e) => format!("{}:{}", e.host, e.port),
        }
    }

    fn full_path(&self, path: &str) -> String {
        match self.ep() {
            None => path.to_string(),
            Some(e) => format!("{}{}", e.base, path),
        }
    }

    /// `GET /auth/server-info` (spec/17-gxctl-auth.md §7.1), asked without
    /// credentials: the token, if any, is set aside so the answer is the
    /// unauthenticated one every client sees.
    pub async fn server_info(&self) -> Result<serde_json::Value, Failure> {
        let saved = self.token.lock().unwrap().take();
        let r = self.send(Method::GET, "/auth/server-info", None).await;
        *self.token.lock().unwrap() = saved;
        let resp = r?;
        if !resp.status.is_success() {
            return Err(render_error(resp.status, &resp.body, !self.is_unix()));
        }
        serde_json::from_slice(&resp.body)
            .map_err(|e| Failure::transport(format!("server-info: {e}")))
    }

    /// Before the first command of a profile run: the cluster binding
    /// (§4.1). A server-info that cannot be reached is a warning, not a
    /// wall — an older control plane has no such route, and gxctl must
    /// keep working across a rolling upgrade. What *is* a wall: a bound
    /// `cluster_id` the answer disagrees with (§4.3), and a server whose
    /// own fingerprint claim disagrees with the certificate we connected
    /// to — that is a proxy terminating TLS in front of the cluster
    /// (§5.2), never something to prompt through.
    pub async fn preflight(&self) -> Result<(), String> {
        if self.is_unix() {
            return Ok(());
        }
        let b = match &self.binding {
            Some(b) => b,
            None => return Ok(()),
        };
        let v = match self.server_info().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "Warning: server-info unreachable ({e}); continuing without the cluster check"
                );
                return Ok(());
            }
        };
        if let (Some(want), Some(got)) = (b.cluster_id.as_deref(), v["cluster_id"].as_str())
            && want != got
        {
            return Err(format!(
                "refusing to continue: profile '{}' is bound to cluster {} but {} answers for {}.\n  A machine was repurposed, or the profile is stale.\n  Re-point with: gxctl auth login --profile {} --rebind",
                b.profile,
                want,
                self.describe(),
                got,
                b.profile
            ));
        }
        let seen = self
            .last_tls
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|t| t.accepted_fingerprint());
        if let (Some(claimed), Some(seen)) = (v["fingerprint"].as_str(), seen.as_deref()) {
            let same = glidex_tls::Trust::normalize_pin(claimed)
                .is_some_and(|c| glidex_tls::Trust::normalize_pin(seen).is_some_and(|s| c == s));
            if !same {
                return Err(format!(
                    "the TLS in front of {} does not match its own claim: the certificate we connected to has SHA-256 {seen}, the server reports {claimed} — something is terminating TLS before the control plane",
                    self.describe()
                ));
            }
        }
        if let Some(got) = v["version"].as_str() {
            let mine = env!("CARGO_PKG_VERSION");
            if got != mine {
                eprintln!(
                    "Warning: {} runs version {got}, this gxctl is {mine} (fine across a rolling upgrade)",
                    self.describe()
                );
            }
        }
        Ok(())
    }

    /// `Authorization` for TCP requests, marked sensitive.
    pub fn auth_header(&self) -> Option<HeaderValue> {
        if self.is_unix() {
            return None;
        }
        let t = self.token.lock().unwrap();
        let t = t.as_ref()?;
        let mut v =
            HeaderValue::from_str(&Zeroizing::new(format!("Bearer {}", t.as_str()))).ok()?;
        v.set_sensitive(true);
        Some(v)
    }

    /// Send one request and read the whole response.
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Response, Failure> {
        let stream = self.connect().await.map_err(Failure::transport)?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| Failure::transport(format!("connection to {}: {}", self.describe(), e)))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let mut req = Request::builder()
            .method(method)
            .uri(self.full_path(path))
            .header(HOST, self.host_header());
        if let Some(h) = self.auth_header() {
            req = req.header(AUTHORIZATION, h);
        }
        let payload = match body {
            Some(b) => {
                req = req.header(CONTENT_TYPE, "application/json");
                Bytes::from(serde_json::to_vec(b).map_err(|e| Failure::transport(e.to_string()))?)
            }
            None => Bytes::new(),
        };
        let req = req
            .body(Full::new(payload))
            .map_err(|e| Failure::transport(format!("bad request: {}", e)))?;
        let resp = sender.send_request(req).await.map_err(|e| {
            Failure::transport(format!("request to {} failed: {}", self.describe(), e))
        })?;
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| Failure::transport(format!("reading the response: {}", e)))?
            .to_bytes();
        Ok(Response { status, body })
    }

    /// A transport for the generated RPC clients of `glidex-api`
    /// (spec/18-grpc.md §10): one connection, made like every other request
    /// (api.sock with peer identity, or TLS with the profile's trust and
    /// token), speaking the Connect protocol, whose unary calls are plain
    /// HTTP/1.1 POSTs.
    pub async fn rpc_transport(&self) -> Result<RpcTransport, Failure> {
        let stream = self.connect().await.map_err(Failure::transport)?;
        let (sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| Failure::transport(format!("connection to {}: {}", self.describe(), e)))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(RpcTransport {
            sender: std::sync::Arc::new(tokio::sync::Mutex::new(sender)),
            prefix: self.full_path(""),
            host: self.host_header(),
            auth: self.auth_header(),
        })
    }

    /// The base URI of the RPC paths: the endpoint's scheme, authority
    /// and path prefix (`http://localhost` on api.sock).
    pub fn rpc_base(&self) -> Uri {
        let scheme = if self.ep().is_some_and(|e| e.tls.is_some()) {
            "https"
        } else {
            "http"
        };
        format!("{}://{}{}", scheme, self.host_header(), self.full_path(""))
            .parse()
            .unwrap_or_else(|_| Uri::from_static("http://localhost"))
    }

    /// A transport for streaming RPCs (the console), which need HTTP/2:
    /// a dedicated connection, dialed like every other (api.sock with peer
    /// identity, or TCP/TLS with the profile's trust, failover and token),
    /// speaking Connect like the unary calls (full duplex on HTTP/2; over
    /// gRPC the error's `ErrorInfo` reason does not reach the client). It
    /// does not reconnect: once the connection is gone, calls on it fail.
    pub async fn stream_transport(
        &self,
    ) -> Result<
        (
            connectrpc::client::SharedHttp2Connection,
            connectrpc::client::ClientConfig,
        ),
        Failure,
    > {
        let stream = self.connect_h2().await.map_err(Failure::transport)?;
        let slot = std::sync::Arc::new(Mutex::new(Some(stream)));
        let connector = tower::service_fn(move |_: Uri| {
            let s = slot.lock().unwrap().take();
            async move {
                s.map(TokioIo::new).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "the connection is closed")
                })
            }
        });
        let base = self.rpc_base();
        let conn =
            connectrpc::client::Http2Connection::connect_with_connector(connector, base.clone())
                .await
                .map_err(|e| {
                    Failure::transport(format!("connection to {}: {}", self.describe(), e))
                })?;
        let mut config = connectrpc::client::ClientConfig::new(base)
            .with_protocol(connectrpc::Protocol::Connect);
        if let Some(h) = self.auth_header() {
            let mut headers = hyper::HeaderMap::new();
            headers.insert(AUTHORIZATION, h);
            config = config.with_default_headers(headers);
        }
        Ok((conn.shared(16), config))
    }

    /// Request returning the raw body of a successful response.
    pub async fn request_bytes(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Response, Failure> {
        let r = self.send(method, path, body.as_ref()).await?;
        if r.status.is_success() {
            Ok(r)
        } else {
            Err(render_error(r.status, &r.body, !self.is_unix()))
        }
    }

    /// JSON request; an empty body (204) parses as `null`, so `T = ()` works.
    pub async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, Failure> {
        let r = self.request_bytes(method, path, body).await?;
        let v = if r.body.is_empty() || r.status == StatusCode::NO_CONTENT {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&r.body)
                .map_err(|e| Failure::transport(format!("Failed to parse response: {}", e)))?
        };
        serde_json::from_value(v)
            .map_err(|e| Failure::transport(format!("Failed to parse response: {}", e)))
    }

    /// [`request`](Self::request) with the error as a message.
    pub async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, String> {
        self.request(method, path, body)
            .await
            .map_err(|e| e.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_selection_order() {
        let c = socket_candidates(Some("/run/user/1000"), 1000);
        assert_eq!(
            c,
            vec![
                PathBuf::from("/run/glidex-cp/api.sock"),
                PathBuf::from("/run/user/1000/glidex/api.sock"),
                PathBuf::from("/tmp/glidex-1000/api.sock"),
            ]
        );
        assert_eq!(socket_candidates(None, 7).len(), 2);
        assert_eq!(
            socket_candidates(Some(""), 7)[1],
            PathBuf::from("/tmp/glidex-7/api.sock")
        );

        // Explicit always wins, even if it doesn't exist (yet).
        let none = |_: &Path| false;
        assert_eq!(
            select_socket(Some("/x/api.sock".into()), &c, none),
            Some(PathBuf::from("/x/api.sock"))
        );
        assert_eq!(select_socket(None, &c, none), None);
        // Otherwise the first that exists.
        let xdg = |p: &Path| p.starts_with("/run/user") || p.starts_with("/tmp");
        assert_eq!(
            select_socket(None, &c, xdg),
            Some(PathBuf::from("/run/user/1000/glidex/api.sock"))
        );
        let all = |_: &Path| true;
        assert_eq!(
            select_socket(Some(PathBuf::new()), &c, all),
            Some(PathBuf::from("/run/glidex-cp/api.sock"))
        );
    }

    #[test]
    fn real_sockets_are_detected() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock = dir.path().join("api.sock");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let file = dir.path().join("plain");
        std::fs::write(&file, b"").unwrap();
        assert!(is_socket(&sock));
        assert!(!is_socket(&file));
        assert!(!is_socket(&dir.path().join("missing")));
    }

    #[test]
    fn token_file_is_private_and_checked() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("cfg").join("glidex").join("token");
        assert!(read_token_file(&path).unwrap().is_none());

        save_token_file(&path, "gxt_secret").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(
            read_token_file(&path)
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("gxt_secret")
        );

        // Overwriting keeps 0600, and no temporary file is left.
        save_token_file(&path, "gxt_other").unwrap();
        assert_eq!(
            read_token_file(&path)
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("gxt_other")
        );
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );

        for bad in [0o640, 0o604, 0o644] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(bad)).unwrap();
            let err = read_token_file(&path).unwrap_err();
            assert!(err.contains("group or others"), "{err}");
            assert!(
                !err.contains("gxt_other"),
                "the error must not echo the token"
            );
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(read_token_file(&path).unwrap().is_some());

        assert!(delete_token_file(&path).unwrap());
        assert!(!delete_token_file(&path).unwrap());
    }

    #[test]
    fn principals_and_roles() {
        assert_eq!(
            parse_principal("user:abc").unwrap(),
            serde_json::json!({"type": "User", "id": "abc"})
        );
        assert_eq!(
            parse_principal("team:unix:glidex-admin").unwrap(),
            serde_json::json!({"type": "Team", "id": "unix:glidex-admin"})
        );
        assert_eq!(
            parse_principal("token:t1").unwrap(),
            serde_json::json!({"type": "Token", "id": "t1"})
        );
        assert!(parse_principal("abc").is_err());
        assert!(parse_principal("user:").is_err());
        assert!(parse_principal("project:p").is_err());
        assert_eq!(format_entity("Team", Some("x")), "team:x");
        assert_eq!(format_entity("Host", None), "host");

        assert_eq!(role_name("owner"), "role.owner");
        assert_eq!(role_name("grant.host-paths"), "grant.host-paths");
        assert_eq!(
            parse_role_ref("viewer@lab").unwrap(),
            ("role.viewer".to_string(), Some("lab".to_string()))
        );
        assert_eq!(
            parse_role_ref("role.auditor").unwrap(),
            ("role.auditor".to_string(), None)
        );
        assert!(parse_role_ref("viewer@").is_err());
        assert!(parse_role_ref("@lab").is_err());
    }

    #[test]
    fn queries_are_encoded() {
        assert_eq!(
            add_query("/vms", "project", "my lab"),
            "/vms?project=my%20lab"
        );
        assert_eq!(
            add_query("/audit?limit=5", "project", "a&b"),
            "/audit?limit=5&project=a%26b"
        );
    }

    #[test]
    fn errors_read_well() {
        let body = |c: &str, m: &str| {
            serde_json::to_vec(&serde_json::json!({"error": c, "message": m})).unwrap()
        };
        let e = render_error(
            StatusCode::UNAUTHORIZED,
            &body("reauth_required", "needs a recent login"),
            true,
        );
        assert!(
            e.message.starts_with("log in again: needs a recent login"),
            "{e}"
        );
        let e = render_error(
            StatusCode::UNAUTHORIZED,
            &body("unauthenticated", "authentication required"),
            true,
        );
        assert!(e.message.contains("gxctl login"), "{e}");
        let e = render_error(
            StatusCode::UNAUTHORIZED,
            &body("unauthenticated", "authentication required"),
            false,
        );
        assert!(!e.message.contains("gxctl login"), "{e}");
        let e = render_error(
            StatusCode::FORBIDDEN,
            &body("forbidden", "not allowed: createVm"),
            false,
        );
        assert_eq!(
            e.message,
            "permission denied (forbidden): not allowed: createVm"
        );
        let e = render_error(
            StatusCode::NOT_FOUND,
            &body("not_found", "VM not found"),
            false,
        );
        assert_eq!(e.message, "not_found: VM not found");
        let e = render_error(StatusCode::BAD_GATEWAY, b"upstream down", false);
        assert_eq!(e.message, "HTTP 502: upstream down");
    }

    #[test]
    fn plain_http_with_a_token_stays_on_loopback() {
        assert!(ApiClient::tcp("http://localhost:8841", None).is_ok());
        assert!(ApiClient::tcp("http://127.0.0.1:8841", None).is_ok());
        assert!(ApiClient::tcp("http://[::1]:8841", None).is_ok());
        assert!(ApiClient::tcp("http://example.org:8841", None).is_err());
        assert!(ApiClient::tcp("ftp://localhost", None).is_err());
        let c = ApiClient::tcp(
            "http://localhost:8841/api/",
            Some(Zeroizing::new("gxt_s".into())),
        )
        .unwrap();
        assert_eq!(c.describe(), "http://localhost:8841/api");
        assert_eq!(c.rpc_base().to_string(), "http://localhost:8841/api");
        assert!(!format!("{:?}", c).contains("gxt_s"));
        assert!(c.auth_header().unwrap().is_sensitive());
        assert!(ApiClient::unix("/x".into()).auth_header().is_none());
    }

    /// A stand-in control plane over TLS: accepts every connection, answers
    /// every request with the canned body. Enough to exercise failover and
    /// preflight without a hyper server; deliberately dumb, it never even
    /// reads the request (TCP buffers it, the answer flows back over).
    fn canned_tls_server(cert: &Path, key: &Path, body: Vec<u8>) -> u16 {
        let acceptor =
            tokio_rustls::TlsAcceptor::from(glidex_tls::server_config(cert, key, &[]).unwrap());
        let l = glidex_tls::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = l.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let body = body.clone();
                tokio::spawn(async move {
                    if let Ok(mut t) = acceptor.accept(tcp).await {
                        use tokio::io::AsyncWriteExt;
                        let _ = t.write_all(&body).await;
                        let _ = t.shutdown().await;
                    }
                });
            }
        });
        port
    }

    fn canned(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes()
        .to_vec()
    }

    fn lab() -> (tempfile::TempDir, glidex_tls::SelfSigned, String) {
        let dir = tempfile::TempDir::new().unwrap();
        let ss = glidex_tls::ensure_self_signed(dir.path(), "cp", &Default::default()).unwrap();
        let fp = glidex_tls::fingerprint(&ss.cert).unwrap();
        (dir, ss, fp)
    }

    #[tokio::test]
    async fn failover_walks_the_list_and_remembers_the_winner() {
        let (dir, ss, fp) = lab();
        let live = canned_tls_server(&ss.cert, &ss.key, canned("{}"));
        let tls = crate::config::Tls {
            pins: vec![fp],
            verify_host: false,
            ..Default::default()
        };
        let path = dir.path().join("config.json");
        let mut cfg = crate::config::Config::default();
        cfg.profiles.insert(
            "t".into(),
            crate::config::Profile {
                url: vec![
                    "https://127.0.0.1:1".to_string(),
                    format!("https://127.0.0.1:{live}"),
                ],
                tls: tls.clone(),
                ..Default::default()
            },
        );
        crate::config::save_to(&path, &cfg).unwrap();
        let urls: Vec<String> = cfg.profiles.get("t").unwrap().url.clone();
        let c = ApiClient::tcp_multi(
            &urls,
            None,
            Some(&tls),
            Some(ProfileCtx {
                name: "t".into(),
                path: path.clone(),
                banner: String::new(),
            }),
            None,
        )
        .unwrap();
        assert_eq!(
            c.describe(),
            format!("https://127.0.0.1:1, https://127.0.0.1:{live}")
        );
        // The dead endpoint answers "connection refused" and the walk moves
        // on without waiting out its timeout (§4.2).
        let r = c.send(Method::GET, "/vms", None).await.unwrap();
        assert!(r.status.is_success(), "{:?}", r.status);
        // And the winner becomes the starting point for the next run —
        // metadata only, so losing it would cost nothing.
        let back = crate::config::load_from(&path).unwrap().unwrap();
        assert_eq!(
            back.profiles
                .get("t")
                .unwrap()
                .last_used
                .as_ref()
                .and_then(|l| l.url.clone()),
            Some(format!("https://127.0.0.1:{live}"))
        );
    }

    #[tokio::test]
    async fn preflight_refuses_a_machine_that_answers_for_another_cluster() {
        let (dir, ss, fp) = lab();
        // §4.3: a bound profile and a disagreeing server-info is a wall.
        let port = canned_tls_server(
            &ss.cert,
            &ss.key,
            canned(
                r#"{"cluster_id":"bbb","cluster_name":"lab","version":"0.1.0","fingerprint":null,"methods":{}}"#,
            ),
        );
        let tls = crate::config::Tls {
            pins: vec![fp.clone()],
            verify_host: false,
            ..Default::default()
        };
        let mk = |bound: Option<&str>| {
            ApiClient::tcp_multi(
                &[format!("https://127.0.0.1:{port}")],
                None,
                Some(&tls),
                None,
                Some(Binding {
                    profile: "t".into(),
                    cluster_id: bound.map(|b| b.to_string()),
                }),
            )
            .unwrap()
        };
        let e = mk(Some("aaa")).preflight().await.unwrap_err();
        assert!(
            e.contains("answers for bbb") && e.contains("--rebind"),
            "{e}"
        );
        // Never verified: nothing to disagree with (§4.1: the server may be
        // reinitialised, the profile must not invent a binding).
        assert!(mk(None).preflight().await.is_ok());
        // The claimed fingerprint must be the certificate we were handed:
        // a proxy terminating TLS presents its own, and the two disagree
        // (§5.2). A wrong claim is a hard stop; the honest one passes.
        let liar = canned_tls_server(
            &ss.cert,
            &ss.key,
            canned(
                r#"{"cluster_id":null,"cluster_name":"x","version":"0.1.0","fingerprint":"AB:CD:EF:AB:CD:EF:AB:CD:EF:AB:CD:EF:AB:CD:EF:AB:CD:EF:AB:CD:EF:AB:CD:EF:AB:CD:EF:AB:CD","methods":{}}"#,
            ),
        );
        let c = ApiClient::tcp_multi(
            &[format!("https://127.0.0.1:{liar}")],
            None,
            Some(&tls),
            None,
            Some(Binding {
                profile: "t".into(),
                cluster_id: None,
            }),
        )
        .unwrap();
        let e = c.preflight().await.unwrap_err();
        assert!(e.contains("terminating TLS"), "{e}");
        let honest = canned_tls_server(
            &ss.cert,
            &ss.key,
            canned(&format!(
                r#"{{"cluster_id":null,"cluster_name":"x","version":"0.1.0","fingerprint":"{fp}","methods":{{}}"#
            )),
        );
        let c = ApiClient::tcp_multi(
            &[format!("https://127.0.0.1:{honest}")],
            None,
            Some(&tls),
            None,
            Some(Binding {
                profile: "t".into(),
                cluster_id: None,
            }),
        )
        .unwrap();
        assert!(c.preflight().await.is_ok());
        let _ = dir;
    }

    #[tokio::test]
    async fn no_configuration_reaches_for_an_untrusted_certificate() {
        let (dir, ss, fp) = lab();
        let port = canned_tls_server(&ss.cert, &ss.key, canned("{}"));
        // The plain profile: system store only. Our lab certificate is in
        // nobody's store, so the ladder refuses it and records the
        // fingerprint for the prompt and for the error (§5.2).
        let c = ApiClient::tcp_multi(
            &[format!("https://127.0.0.1:{port}")],
            None,
            Some(&crate::config::Tls::default()),
            None,
            None,
        )
        .unwrap();
        let e = match c.send(Method::GET, "/vms", None).await {
            Ok(r) => panic!(
                "the untrusted certificate was accepted (status {})",
                r.status
            ),
            Err(e) => e,
        };
        assert!(e.message.contains("SHA-256"), "{e}");
        assert_eq!(c.rejected_fingerprint().as_deref(), Some(fp.as_str()));
        // The pin converts the refusal into a pass — same bytes, decided
        // by the operator, and dates and the handshake signature are still
        // verified (glidex-tls tests cover that depth; here: the wiring).
        let tls = crate::config::Tls {
            pins: vec![fp.clone()],
            verify_host: false,
            ..Default::default()
        };
        let c = ApiClient::tcp_multi(
            &[format!("https://127.0.0.1:{port}")],
            None,
            Some(&tls),
            None,
            None,
        )
        .unwrap();
        assert!(
            c.send(Method::GET, "/vms", None)
                .await
                .unwrap()
                .status
                .is_success()
        );
        assert_eq!(c.accepted_fingerprint().as_deref(), Some(fp.as_str()));
        // The declared bypass accepts anything — which is exactly why
        // getting to it needs a typed host name (§5.4, enforced by the
        // config loader's validation, not by this transport layer).
        let tls = crate::config::Tls {
            insecure: true,
            ..Default::default()
        };
        let c = ApiClient::tcp_multi(
            &[format!("https://127.0.0.1:{port}")],
            None,
            Some(&tls),
            None,
            None,
        )
        .unwrap();
        assert!(
            c.send(Method::GET, "/vms", None)
                .await
                .unwrap()
                .status
                .is_success()
        );
        let _ = dir;
    }

    /// Streaming calls dial TLS offering only `h2`, with the profile's
    /// trust, the endpoint's path prefix and the token; a server that will
    /// not speak HTTP/2 is refused before any request.
    #[tokio::test]
    async fn streams_get_an_http2_connection_of_their_own() {
        use glidex_api::proto::glidex::vm::v1::{ConsoleRequest, VmServiceClient};
        let (_dir, ss, fp) = lab();
        let tls = crate::config::Tls {
            pins: vec![fp],
            verify_host: false,
            ..Default::default()
        };
        let token = || Some(Zeroizing::new("gxt_secret".to_string()));

        // HTTP/1.1 only (no ALPN): refused.
        let old = canned_tls_server(&ss.cert, &ss.key, canned("{}"));
        let urls = vec![format!("https://127.0.0.1:{old}")];
        let c = ApiClient::tcp_multi(&urls, token(), Some(&tls), None, None).unwrap();
        let e = c.stream_transport().await.err().unwrap().message;
        assert!(e.contains("HTTP/2"), "{e}");

        // An h2 server that records what it was asked.
        let acceptor = tokio_rustls::TlsAcceptor::from(
            glidex_tls::server_config(&ss.cert, &ss.key, &[b"h2"]).unwrap(),
        );
        let l = glidex_tls::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = l.local_addr().unwrap().port();
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (tcp, _) = l.accept().await.unwrap();
            let t = acceptor.accept(tcp).await.unwrap();
            let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                let auth = req
                    .headers()
                    .get(AUTHORIZATION)
                    .map(|v| v.to_str().unwrap().to_string());
                let _ = seen_tx.send((req.version(), req.uri().path().to_string(), auth));
                async {
                    hyper::Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Full::new(Bytes::new()))
                }
            });
            let _ = hyper::server::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection(TokioIo::new(t), svc)
                .await;
        });
        let urls = vec![format!("https://127.0.0.1:{port}/api")];
        let c = ApiClient::tcp_multi(&urls, token(), Some(&tls), None, None).unwrap();
        let (t, config) = c.stream_transport().await.unwrap();
        assert!(c.accepted_fingerprint().is_some());
        let mut stream = VmServiceClient::new(t, config).console().await.unwrap();
        let _ = stream.send(ConsoleRequest::default()).await;
        assert!(stream.message().await.is_err());
        let (version, path, auth) = seen.recv().await.unwrap();
        assert_eq!(version, hyper::Version::HTTP_2);
        assert_eq!(path, "/api/glidex.vm.v1.VmService/Console");
        assert_eq!(auth.as_deref(), Some("Bearer gxt_secret"));
    }

    #[tokio::test]
    async fn server_info_travels_without_credentials() {
        let (dir, ss, fp) = lab();
        let port = canned_tls_server(
            &ss.cert,
            &ss.key,
            canned(
                r#"{"cluster_id":"c1","cluster_name":"lab","version":"0.1.0","fingerprint":null,"methods":{"pam":true,"oidc":false}}"#,
            ),
        );
        let tls = crate::config::Tls {
            pins: vec![fp],
            verify_host: false,
            ..Default::default()
        };
        let c = ApiClient::tcp_multi(
            &[format!("https://127.0.0.1:{port}")],
            Some(Zeroizing::new("gxt_secret".into())),
            Some(&tls),
            None,
            None,
        )
        .unwrap();
        let v = c.server_info().await.unwrap();
        assert_eq!(v["cluster_id"].as_str(), Some("c1"));
        // The token stays off the probe — the invariant that the TOFU
        // bootstrap leans on: nothing authenticated rides a connection that
        // has not passed the ladder.
        assert!(c.has_token());
        let _ = dir;
    }
}

/// See [`ApiClient::rpc_transport`].
#[derive(Clone)]
pub struct RpcTransport {
    sender: std::sync::Arc<
        tokio::sync::Mutex<hyper::client::conn::http1::SendRequest<connectrpc::client::ClientBody>>,
    >,
    prefix: String,
    host: String,
    auth: Option<HeaderValue>,
}

#[derive(Debug)]
pub struct RpcTransportError(String);

impl fmt::Display for RpcTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RpcTransportError {}

impl RpcTransport {
    /// The client configuration to go with it: Connect, binary protobuf.
    pub fn config(&self) -> connectrpc::client::ClientConfig {
        connectrpc::client::ClientConfig::new(Uri::from_static("http://glidex"))
    }
}

impl connectrpc::client::ClientTransport for RpcTransport {
    type ResponseBody = hyper::body::Incoming;
    type Error = RpcTransportError;

    fn send(
        &self,
        request: Request<connectrpc::client::ClientBody>,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<hyper::Response<Self::ResponseBody>, Self::Error>,
    > {
        let t = self.clone();
        Box::pin(async move {
            let (mut parts, body) = request.into_parts();
            let path = parts
                .uri
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            parts.uri = format!("{}{}", t.prefix, path)
                .parse()
                .map_err(|e| RpcTransportError(format!("bad path: {e}")))?;
            parts.headers.insert(
                HOST,
                HeaderValue::from_str(&t.host).map_err(|e| RpcTransportError(e.to_string()))?,
            );
            if let Some(a) = &t.auth {
                parts.headers.insert(AUTHORIZATION, a.clone());
            }
            let mut sender = t.sender.lock().await;
            sender
                .ready()
                .await
                .map_err(|e| RpcTransportError(format!("connection: {e}")))?;
            sender
                .send_request(Request::from_parts(parts, body))
                .await
                .map_err(|e| RpcTransportError(format!("request failed: {e}")))
        })
    }
}
