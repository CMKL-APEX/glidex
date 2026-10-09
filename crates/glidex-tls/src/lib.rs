//! HTTPS for the control plane and glidex-ui (spec/security.md §5.1).
//!
//! - Listeners: every address by default (`0.0.0.0` and `[::]`, the IPv6
//!   socket `IPV6_V6ONLY` so both can bind the same port).
//! - Self-signed certificates (§5.1.1): generated once, kept until they
//!   are missing, unreadable or near expiry, so their fingerprint stays
//!   stable for clients that trust them.
//! - The local names of this host: certificate SANs, the UI's `Host`
//!   allowlist and the control plane's default `Origin`s.
//! - Client trust for gxctl and the UI's TCP fallback: the system store,
//!   explicit PEM files and the control plane's published certificate,
//!   with the fingerprint of a rejected certificate kept for the error.

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio_rustls::rustls;

pub use tokio_rustls;

/// Name of the control plane's published certificate in its run directory.
pub const PUBLISHED_CERT: &str = "tls.crt";

/// How long a generated certificate is valid.
const VALID_DAYS: i64 = 5 * 365;
/// Regenerate a generated certificate this close to its expiry.
const RENEW_DAYS: i64 = 30;

// ---- listeners --------------------------------------------------------------

/// The default listeners: every IPv4 and every IPv6 address on `port`.
pub fn all_addresses(port: u16) -> Vec<SocketAddr> {
    vec![SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)), SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))]
}

/// Parse a comma-separated list of `addr:port`.
pub fn parse_addresses(list: &str, what: &str) -> Result<Vec<SocketAddr>, String> {
    let v: Vec<SocketAddr> = list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|a| a.parse::<SocketAddr>().map_err(|e| format!("{} {}: {}", what, a, e)))
        .collect::<Result<_, _>>()?;
    if v.is_empty() {
        return Err(format!("{} is empty", what));
    }
    Ok(v)
}

pub fn all_loopback(addrs: &[SocketAddr]) -> bool {
    addrs.iter().all(|a| a.ip().is_loopback())
}

/// Bind a TCP listener. An IPv6 socket is IPv6-only, so `[::]` and
/// `0.0.0.0` can share a port whatever `net.ipv6.bindv6only` says.
pub fn bind(addr: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Socket, Type};
    let domain = if addr.is_ipv6() { Domain::IPV6 } else { Domain::IPV4 };
    let s = Socket::new(domain, Type::STREAM, None)?;
    if addr.is_ipv6() {
        s.set_only_v6(true)?;
    }
    s.set_reuse_address(true)?;
    s.set_nonblocking(true)?;
    s.bind(&addr.into())?;
    s.listen(1024)?;
    tokio::net::TcpListener::from_std(s.into())
}

/// The bind error of `[::]` on a host with IPv6 disabled: skip that
/// listener quietly instead of warning.
pub fn ipv6_unavailable(addr: &SocketAddr, e: &io::Error) -> bool {
    addr.is_ipv6()
        && addr.ip().is_unspecified()
        && matches!(e.raw_os_error(), Some(libc::EAFNOSUPPORT) | Some(libc::EADDRNOTAVAIL) | Some(libc::EPROTONOSUPPORT))
}

// ---- local names ------------------------------------------------------------

/// The names and addresses this host answers to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalNames {
    /// FQDN, host name, `localhost` (lowercase, deduplicated).
    pub dns: Vec<String>,
    /// Loopback addresses, then the interfaces' non-link-local addresses.
    pub ips: Vec<IpAddr>,
}

/// The host name as the kernel knows it; `None` when there is none.
pub fn hostname() -> Option<String> {
    nix::unistd::gethostname().ok().and_then(|h| h.into_string().ok()).filter(|h| !h.is_empty())
}

impl LocalNames {
    /// Read them now. Never fails: what can't be found is left out.
    pub fn discover() -> Self {
        let mut n = LocalNames::default();
        let host = nix::unistd::gethostname().ok().and_then(|h| h.into_string().ok()).unwrap_or_default();
        if !host.is_empty() {
            if let Some(f) = canonical_name(&host) {
                n.add_dns(&f);
            }
            n.add_dns(&host);
        }
        n.add_dns("localhost");
        n.add_ip(IpAddr::V4(Ipv4Addr::LOCALHOST));
        n.add_ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
        match interface_ips() {
            Ok(ips) => ips.into_iter().for_each(|ip| n.add_ip(ip)),
            // Not fatal (loopback and names still work), but every LAN
            // address would then get 421 and be missing from certificates.
            Err(e) => tracing::warn!(
                "cannot list network interfaces ({}); only localhost and the host name are known. \
                 Under systemd the unit needs RestrictAddressFamilies=AF_NETLINK",
                e
            ),
        }
        n
    }

    fn add_dns(&mut self, name: &str) {
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        if !name.is_empty() && name.parse::<IpAddr>().is_err() && !self.dns.contains(&name) {
            self.dns.push(name);
        }
    }

    fn add_ip(&mut self, ip: IpAddr) {
        if usable_ip(ip) && !self.ips.contains(&ip) {
            self.ips.push(ip);
        }
    }

    /// Every name and address as it appears in a `Host` header or URL
    /// authority (IPv6 in brackets), names first.
    pub fn hosts(&self) -> Vec<String> {
        self.dns
            .iter()
            .cloned()
            .chain(self.ips.iter().map(|ip| match ip {
                IpAddr::V4(v4) => v4.to_string(),
                IpAddr::V6(v6) => format!("[{}]", v6),
            }))
            .collect()
    }

    /// `https://<host>:<port>` for every host.
    pub fn origins(&self, port: u16) -> Vec<String> {
        self.hosts().into_iter().map(|h| format!("https://{}:{}", h, port)).collect()
    }
}

/// Neither link-local nor unspecified: an address a client can name.
fn usable_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !(v4.is_link_local() || v4.is_unspecified()),
        IpAddr::V6(v6) => !((v6.segments()[0] & 0xffc0) == 0xfe80 || v6.is_unspecified()),
    }
}

/// The addresses of the host's interfaces right now. Needs a netlink
/// socket (`AF_NETLINK`).
pub fn interface_ips() -> io::Result<Vec<IpAddr>> {
    let mut v = Vec::new();
    for ifa in nix::ifaddrs::getifaddrs().map_err(io::Error::from)? {
        let Some(a) = ifa.address else { continue };
        if let Some(v4) = a.as_sockaddr_in() {
            v.push(IpAddr::V4(v4.ip()));
        } else if let Some(v6) = a.as_sockaddr_in6() {
            v.push(IpAddr::V6(v6.ip()));
        }
    }
    Ok(v)
}

/// Whether `ip` is one of this host's addresses now (loopback included,
/// link-local excluded). For names that appear after startup, e.g. a VPN
/// interface that comes up later.
pub fn is_local_ip(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    ip.is_loopback() || (usable_ip(ip) && interface_ips().is_ok_and(|ips| ips.contains(&ip)))
}

/// The resolver's canonical name for `host` (usually its FQDN).
fn canonical_name(host: &str) -> Option<String> {
    let c_host = std::ffi::CString::new(host).ok()?;
    // SAFETY: plain getaddrinfo/freeaddrinfo; the result list is only read
    // before it is freed.
    unsafe {
        let mut hints: libc::addrinfo = std::mem::zeroed();
        hints.ai_flags = libc::AI_CANONNAME;
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut res: *mut libc::addrinfo = std::ptr::null_mut();
        if libc::getaddrinfo(c_host.as_ptr(), std::ptr::null(), &hints, &mut res) != 0 || res.is_null() {
            return None;
        }
        let name = if (*res).ai_canonname.is_null() {
            None
        } else {
            std::ffi::CStr::from_ptr((*res).ai_canonname).to_str().ok().map(String::from)
        };
        libc::freeaddrinfo(res);
        name
    }
}

// ---- certificates -----------------------------------------------------------

pub fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(path)
        .map_err(|e| format!("{}: {}", path.display(), e))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {}", path.display(), e))?;
    if certs.is_empty() {
        return Err(format!("{}: no certificate", path.display()));
    }
    Ok(certs)
}

/// SHA-256 of a DER certificate, as browsers show it (`AB:CD:…`).
pub fn fingerprint_der(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(":")
}

/// Fingerprint of the first certificate in PEM text.
pub fn fingerprint_pem(pem: &[u8]) -> Option<String> {
    let der = CertificateDer::pem_slice_iter(pem).next()?.ok()?;
    Some(fingerprint_der(&der))
}

/// Fingerprint of the first certificate in a PEM file.
pub fn fingerprint(cert: &Path) -> Result<String, String> {
    Ok(fingerprint_der(&load_certs(cert)?[0]))
}

/// A server config from a PEM chain and key, offering `alpn`.
pub fn server_config(cert: &Path, key: &Path, alpn: &[&[u8]]) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs = load_certs(cert)?;
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {}", key.display(), e))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS: {}", e))?;
    cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(Arc::new(cfg))
}

/// A self-signed certificate and its key.
#[derive(Debug, Clone)]
pub struct SelfSigned {
    pub cert: PathBuf,
    pub key: PathBuf,
    /// Made by this call (rather than reused).
    pub generated: bool,
}

/// `<dir>/<stem>.crt` and `<stem>.key`: reused when both load, match and
/// are valid for more than 30 days; otherwise (re)generated for `names`.
/// `dir` is created `0700`; the key is written `0600` before the
/// certificate, each to a temporary file renamed into place.
pub fn ensure_self_signed(dir: &Path, stem: &str, names: &LocalNames) -> Result<SelfSigned, String> {
    let cert = dir.join(format!("{}.crt", stem));
    let key = dir.join(format!("{}.key", stem));
    if reusable(&cert, &key) {
        return Ok(SelfSigned { cert, key, generated: false });
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("{}: {}", dir.display(), e))?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let (cert_pem, key_pem) = generate(names)?;
    write_file(&key, key_pem.as_bytes(), 0o600)?;
    write_file(&cert, cert_pem.as_bytes(), 0o644)?;
    Ok(SelfSigned { cert, key, generated: true })
}

fn reusable(cert: &Path, key: &Path) -> bool {
    let Ok(certs) = load_certs(cert) else { return false };
    if server_config(cert, key, &[]).is_err() {
        return false;
    }
    match x509_parser::parse_x509_certificate(&certs[0]) {
        Ok((_, c)) => {
            let renew_at = time::OffsetDateTime::now_utc() + time::Duration::days(RENEW_DAYS);
            c.validity().not_after.timestamp() > renew_at.unix_timestamp()
        }
        Err(_) => false,
    }
}

/// A certificate for `names` (ECDSA P-256), as PEM certificate and key.
pub fn generate(names: &LocalNames) -> Result<(String, String), String> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, KeyPair, SanType};
    let key = KeyPair::generate().map_err(|e| format!("key generation: {}", e))?;
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::OrganizationName, "glidex (self-signed)");
    dn.push(DnType::CommonName, names.dns.first().map(String::as_str).unwrap_or("localhost"));
    params.distinguished_name = dn;
    for d in &names.dns {
        if let Ok(n) = d.as_str().try_into() {
            params.subject_alt_names.push(SanType::DnsName(n));
        }
    }
    params.subject_alt_names.extend(names.ips.iter().map(|ip| SanType::IpAddress(*ip)));
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let now = time::OffsetDateTime::now_utc();
    // A day of slack for clients whose clock is behind.
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(VALID_DAYS);
    let cert = params.self_signed(&key).map_err(|e| format!("certificate: {}", e))?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// Write `data` to `path` with `mode` via a temporary file and rename.
fn write_file(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let res = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.map_err(|e| format!("{}: {}", path.display(), e))
}

// ---- the published certificate ---------------------------------------------

/// Copy `cert` (never the key) to `<run_dir>/tls.crt`, mode `0644`, for
/// local clients to trust.
pub fn publish(cert: &Path, run_dir: &Path) -> Result<PathBuf, String> {
    let data = std::fs::read(cert).map_err(|e| format!("{}: {}", cert.display(), e))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(run_dir)
        .map_err(|e| format!("{}: {}", run_dir.display(), e))?;
    let dest = run_dir.join(PUBLISHED_CERT);
    write_file(&dest, &data, 0o644)?;
    Ok(dest)
}

/// Remove a stale published certificate (TLS is off).
pub fn unpublish(run_dir: &Path) {
    let _ = std::fs::remove_file(run_dir.join(PUBLISHED_CERT));
}

/// Where a local control plane publishes its certificate, in the order
/// of its run directories: the systemd unit's, then a hand-started one's.
pub fn published_candidates(xdg_runtime_dir: Option<&str>, euid: u32) -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/run/glidex-cp").join(PUBLISHED_CERT)];
    if let Some(x) = xdg_runtime_dir.filter(|x| !x.is_empty()) {
        v.push(Path::new(x).join("glidex").join(PUBLISHED_CERT));
    }
    v.push(PathBuf::from(format!("/tmp/glidex-{}", euid)).join(PUBLISHED_CERT));
    v
}

/// The published certificates that exist and that only a trusted owner
/// could have put there (see `trustworthy`).
pub fn published_certs() -> Vec<PathBuf> {
    let euid = nix::unistd::geteuid().as_raw();
    let xdg = std::env::var("XDG_RUNTIME_DIR").ok();
    let glidex = nix::unistd::User::from_name("glidex").ok().flatten().map(|u| u.uid.as_raw());
    published_candidates(xdg.as_deref(), euid)
        .into_iter()
        .filter(|p| trustworthy(p, euid, glidex))
        .collect()
}

/// A trust anchor read from a shared location must not be plantable: the
/// file and its directory are owned by root, the caller or the `glidex`
/// user, and neither is writable by group or others.
pub fn trustworthy(path: &Path, euid: u32, glidex_uid: Option<u32>) -> bool {
    let ok = |p: &Path| match std::fs::symlink_metadata(p) {
        Ok(m) => {
            let owner = m.uid();
            (owner == 0 || owner == euid || Some(owner) == glidex_uid) && m.mode() & 0o022 == 0
        }
        Err(_) => false,
    };
    let is_file = std::fs::symlink_metadata(path).map(|m| m.is_file()).unwrap_or(false);
    is_file && ok(path) && path.parent().is_some_and(ok)
}

// ---- client trust -----------------------------------------------------------

/// The certificate trust one gxctl login profile asks for
/// (spec/gxctl-auth.md §5.1), fed to [`ClientTls::with_trust`].
#[derive(Debug, Clone, Default)]
pub struct Trust {
    /// PEM files trusted next to the system store. Their access control is
    /// the caller's job, same as for `ca_pem`.
    pub ca_files: Vec<PathBuf>,
    /// Inline PEM text, for profiles that carry the CA with them.
    pub ca_pem: Option<String>,
    /// Leaf-certificate SHA-256 fingerprints the user accepted on first
    /// use, in [`Trust::normalize_pin`] shape. A pin substitutes nothing:
    /// dates, the handshake signature, and (unless `verify_host` is off)
    /// the name keep being checked (gxctl-auth.md A4).
    pub pins: Vec<String>,
    /// Match the certificate's SANs against the dialed name. Only profiles
    /// with a pin may turn this off, for dialing an address that never
    /// made it into the certificate; regenerating the certificate is the
    /// better fix (gxctl-auth.md §2.2).
    pub verify_host: bool,
    /// Verify nothing, for a profile whose owner typed the host name to
    /// confirm (`i_understand`, gxctl-auth.md A9). rustls still binds the
    /// handshake signature to the presented certificate; the caller prints
    /// the loud warning. Audit this field's use before shipping code that
    /// reaches it.
    pub insecure: bool,
}

impl Trust {
    /// A fingerprint in any shape an operator pastes it (`SHA256/AB:CD:…`,
    /// `ab cd …`, bare hex) reduced to 64 lowercase hex digits; `None`
    /// when it is not one.
    pub fn normalize_pin(s: &str) -> Option<String> {
        // Text up to a tag separator (`sha256/`) is dropped whole, so its
        // hex-looking digits cannot inflate the count; every non-hex
        // character left is a separator.
        let low = s.trim().to_lowercase();
        let body = match low.chars().position(|c| c == '/') {
            Some(i) => &low[i + 1..],
            None => &low[..],
        };
        let hex: String = body.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() == 64 { Some(hex) } else { None }
    }
}

/// A client config that remembers the fingerprint of the last certificate
/// it accepted and of the last it rejected — for error messages, the
/// first-use trust prompt, and cross-checking what a server claims about
/// itself (spec/gxctl-auth.md §5.2).
#[derive(Clone)]
pub struct ClientTls {
    pub config: Arc<rustls::ClientConfig>,
    rejected: Arc<Mutex<Option<String>>>,
    accepted: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for ClientTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientTls")
    }
}

impl ClientTls {
    /// Trust the system store plus every certificate in `extra` (PEM
    /// files), offering `alpn`.
    pub fn new(extra: &[PathBuf], alpn: &[&[u8]]) -> Result<Self, String> {
        Self::with_trust(&Trust { ca_files: extra.into_iter().cloned().collect(), verify_host: true, ..Default::default() }, alpn)
    }

    /// gxctl's trust ladder (spec/gxctl-auth.md §5.1), one rung stricter
    /// than the last and each entered only where the profile says so:
    ///
    /// 1. WebPKI against the system store, `ca_files` and `ca_pem`.
    /// 2. `pins`: a leaf whose fingerprint the user accepted on first use
    ///    (TOFU). rustls verifies the chain up to the pinned leaf itself —
    ///    a self-signed certificate is its own trust anchor, the same
    ///    mechanism the published certificate uses today — plus the dates
    ///    and, unless `verify_host` is off, the name. Pins pin a *leaf*;
    ///    a CA-issued chain belongs in `ca_files`, not in a pin.
    /// 3. `insecure`: nothing, and the caller shouted first.
    ///
    /// No rung is ever skipped silently: a pin accepts only a byte-exact
    /// fingerprint match, and only after WebPKI itself refused the peer.
    pub fn with_trust(t: &Trust, alpn: &[&[u8]]) -> Result<Self, String> {
        if t.pins.is_empty() && !t.verify_host && !t.insecure {
            // The loader validates this too; guarding here keeps any
            // caller from assembling the combination by accident.
            return Err("tls.verify_host=false only applies alongside a pin".into());
        }
        let mut roots = rustls::RootCertStore::empty();
        let mut list: Vec<CertificateDer<'static>> = Vec::new();
        for c in rustls_native_certs::load_native_certs().certs {
            if roots.add(c.clone()).is_ok() {
                list.push(c);
            }
        }
        for p in &t.ca_files {
            for c in load_certs(p)? {
                roots.add(c.clone()).map_err(|e| format!("{}: {}", p.display(), e))?;
                list.push(c);
            }
        }
        if let Some(pem) = &t.ca_pem {
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem.as_bytes())
                .collect::<Result<_, _>>()
                .map_err(|e| format!("tls.ca_pem: {e}"))?;
            if certs.is_empty() {
                return Err("tls.ca_pem: no certificate".into());
            }
            for c in certs {
                roots.add(c.clone()).map_err(|e| format!("tls.ca_pem: {e}"))?;
                list.push(c);
            }
        }
        if roots.is_empty() && t.pins.is_empty() && !t.insecure {
            return Err("no trusted CA certificates found (install the system CA bundle or give a certificate file)".into());
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
            .build()
            .map_err(|e| e.to_string())?;
        let rejected = Arc::new(Mutex::new(None));
        let accepted = Arc::new(Mutex::new(None));
        let verifier = Arc::new(Ladder {
            inner,
            roots: list,
            pins: t.pins.iter().filter_map(|p| Trust::normalize_pin(p)).collect(),
            verify_host: t.verify_host,
            insecure: t.insecure,
            rejected: rejected.clone(),
            accepted: accepted.clone(),
        });
        let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        cfg.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        Ok(ClientTls { config: Arc::new(cfg), rejected, accepted })
    }

    /// Fingerprint of the last certificate that failed verification.
    pub fn rejected_fingerprint(&self) -> Option<String> {
        self.rejected.lock().unwrap().clone()
    }

    /// Fingerprint of the last certificate that was accepted, to compare
    /// against what `GET /auth/server-info` claims (spec/gxctl-auth.md
    /// §7.1): a proxy terminating TLS in front of the cluster shows its
    /// own certificate, and the two fingerprints then disagree.
    pub fn accepted_fingerprint(&self) -> Option<String> {
        self.accepted.lock().unwrap().clone()
    }
}

/// The trust ladder as rustls sees it: the audited WebPKI verifier decides
/// first, and a profile's pins may only rescue a certificate WebPKI refused,
/// and only after it has been re-verified against everything a pin is
/// meant to substitute for nothing.
#[derive(Debug)]
struct Ladder {
    /// WebPKI over the system store plus the profile's roots: the normal
    /// path, and the owner of the handshake-signature checks the pins and
    /// the bypass keep delegating.
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    /// The same roots as a list, so a pin hit can rebuild the store with
    /// the presented leaf added to them.
    roots: Vec<CertificateDer<'static>>,
    pins: Vec<String>,
    verify_host: bool,
    insecure: bool,
    rejected: Arc<Mutex<Option<String>>>,
    accepted: Arc<Mutex<Option<String>>>,
}

impl Ladder {
    /// A pinned leaf becomes one more trust anchor of a throwaway WebPKI
    /// build, so chain, dates and name stay checked by rustls's own
    /// audited code — the fingerprint answers "which key", not "is it
    /// valid". Fails closed for leaves that are not their own issuer:
    /// pin is a TOFU device for self-signed certificates, and CA-issued
    /// chains belong in `ca_files`.
    fn verify_pinned(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use rustls::client::danger::ServerCertVerifier; // the trait whose method the field access reaches
        let mut store = rustls::RootCertStore::empty();
        for c in &self.roots {
            let _ = store.add(c.clone());
        }
        store
            .add(CertificateDer::from(end_entity.to_vec()))
            .map_err(|_| rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        let one_shot = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(store), Arc::new(rustls::crypto::ring::default_provider()))
            .build()
            .map_err(|e| rustls::Error::General(format!("pinned verifier: {e}")))?;
        one_shot.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }
}

impl rustls::client::danger::ServerCertVerifier for Ladder {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fp = fingerprint_der(end_entity);
        if self.insecure {
            // "nothing is checked" is the promise `insecure` makes
            // (gxctl-auth.md A9): the profile got there by its owner
            // typing the host name, and the caller warns loudly. The
            // handshake signature below still ties the peer to this
            // certificate, so the answer cannot be replayed by a
            // bystander who merely knows the token.
            *self.accepted.lock().unwrap() = Some(fp);
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }
        let pinned = Trust::normalize_pin(&fp).is_some_and(|f| self.pins.contains(&f));
        if pinned && !self.verify_host {
            // The peer matched a fingerprint the owner approved; the one
            // check rustls would run that the profile waived is the
            // name, and the rest it must keep: dates (A4 — a pin never
            // relaxes them), verified here because WebPKI, which owns
            // them, cannot be asked to skip the name as well.
            check_validity(end_entity, now)?;
            *self.accepted.lock().unwrap() = Some(fp);
            return Ok(rustls::client::danger::ServerCertVerified::assertion());
        }
        let attempt = if pinned {
            self.verify_pinned(end_entity, intermediates, server_name, ocsp_response, now)
        } else {
            self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        };
        match attempt {
            Ok(v) => {
                *self.accepted.lock().unwrap() = Some(fp);
                Ok(v)
            }
            Err(e) => {
                *self.rejected.lock().unwrap() = Some(fp);
                Err(e)
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The validity window of a certificate that was accepted by fingerprint
/// rather than by chain: the one thing WebPKI is skipped for together
/// with the name, so it is checked directly (gxctl-auth.md A4).
fn check_validity(der: &CertificateDer<'_>, now: UnixTime) -> Result<(), rustls::Error> {
    let (_, x) = x509_parser::parse_x509_certificate(der).map_err(|_| rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
    let v = x.validity();
    let now = now.as_secs() as i64;
    if v.not_before.timestamp() > now || v.not_after.timestamp() < now {
        return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::Expired));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> LocalNames {
        LocalNames { dns: vec!["glidex.test".into(), "localhost".into()], ips: vec![IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)] }
    }

    #[test]
    fn local_names_skip_link_local_and_duplicates() {
        let mut n = LocalNames::default();
        n.add_dns("Host.Example.");
        n.add_dns("host.example");
        n.add_dns("10.0.0.1");
        n.add_ip("169.254.1.1".parse().unwrap());
        n.add_ip("fe80::1".parse().unwrap());
        n.add_ip("10.0.0.1".parse().unwrap());
        n.add_ip("10.0.0.1".parse().unwrap());
        n.add_ip("2001:db8::1".parse().unwrap());
        assert_eq!(n.dns, ["host.example"]);
        assert_eq!(n.hosts(), ["host.example", "10.0.0.1", "[2001:db8::1]"]);
        assert_eq!(n.origins(5173)[2], "https://[2001:db8::1]:5173");
        let d = LocalNames::discover();
        assert!(d.dns.contains(&"localhost".to_string()));
        assert!(d.ips.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)));
        // Every discovered address is local now; others aren't.
        for ip in &d.ips {
            assert!(is_local_ip(*ip), "{ip}");
        }
        assert!(is_local_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_local_ip("192.0.2.1".parse().unwrap()));
        assert!(!is_local_ip("fe80::1".parse().unwrap()));
    }

    #[test]
    fn self_signed_is_private_reused_and_regenerated() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("tls");
        let a = ensure_self_signed(&dir, "cp", &names()).unwrap();
        assert!(a.generated);
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&a.key).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&a.cert).unwrap().mode() & 0o777, 0o644);
        let fp = fingerprint(&a.cert).unwrap();
        assert_eq!(fp.len(), 32 * 3 - 1);
        assert_eq!(fingerprint_pem(&std::fs::read(&a.cert).unwrap()), Some(fp.clone()));
        assert_eq!(fingerprint_pem(b"nope"), None);

        // Reused: the fingerprint is stable across restarts.
        let b = ensure_self_signed(&dir, "cp", &names()).unwrap();
        assert!(!b.generated);
        assert_eq!(fingerprint(&b.cert).unwrap(), fp);

        // A broken key is replaced.
        std::fs::write(&a.key, "garbage").unwrap();
        let c = ensure_self_signed(&dir, "cp", &names()).unwrap();
        assert!(c.generated);
        assert_ne!(fingerprint(&c.cert).unwrap(), fp);

        // SANs cover the names and addresses.
        let der = &load_certs(&c.cert).unwrap()[0];
        let (_, x) = x509_parser::parse_x509_certificate(der).unwrap();
        let san = format!("{:?}", x.subject_alternative_name().unwrap().unwrap().value);
        assert!(san.contains("glidex.test") && san.contains("localhost"), "{san}");
    }

    #[test]
    fn publish_and_trust() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = ensure_self_signed(&tmp.path().join("tls"), "cp", &names()).unwrap();
        let run = tmp.path().join("run");
        let p = publish(&s.cert, &run).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), std::fs::read(&s.cert).unwrap());
        assert_eq!(std::fs::metadata(&p).unwrap().mode() & 0o777, 0o644);
        let euid = nix::unistd::geteuid().as_raw();
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(trustworthy(&p, euid, None));
        // Someone else's file, or a world-writable directory, isn't.
        assert!(!trustworthy(&p, euid + 1, None) || euid == 0);
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!trustworthy(&p, euid, None));
        unpublish(&run);
        assert!(!p.exists());
        assert_eq!(
            published_candidates(Some("/run/user/7"), 7),
            [PathBuf::from("/run/glidex-cp/tls.crt"), "/run/user/7/glidex/tls.crt".into(), "/tmp/glidex-7/tls.crt".into()]
        );
    }

    #[test]
    fn addresses() {
        assert_eq!(all_addresses(8841).iter().map(|a| a.to_string()).collect::<Vec<_>>(), ["0.0.0.0:8841", "[::]:8841"]);
        assert!(!all_loopback(&all_addresses(1)));
        let l = parse_addresses("127.0.0.1:1, [::1]:2", "X").unwrap();
        assert!(all_loopback(&l));
        assert!(parse_addresses(" ,", "X").is_err());
        assert!(parse_addresses("nope", "X").is_err());
    }

    /// End to end: a server with a self-signed certificate is verified by
    /// a client that trusts it, and refused (with the fingerprint) by one
    /// that doesn't. IPv4 and IPv6 listeners share a port.
    #[tokio::test]
    async fn handshake_with_published_certificate() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = tempfile::TempDir::new().unwrap();
        let s = ensure_self_signed(&tmp.path().join("tls"), "cp", &names()).unwrap();
        let v4 = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = v4.local_addr().unwrap().port();
        if let Err(e) = bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))) {
            assert!(ipv6_unavailable(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)), &e), "{e}");
        }
        let acceptor = tokio_rustls::TlsAcceptor::from(server_config(&s.cert, &s.key, &[]).unwrap());
        tokio::spawn(async move {
            loop {
                let (tcp, _) = v4.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut t) = acceptor.accept(tcp).await {
                        let _ = t.write_all(b"hi").await;
                        let _ = t.shutdown().await;
                    }
                });
            }
        });
        let connect = |tls: ClientTls| async move {
            let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let name = ServerName::try_from("localhost").unwrap();
            let mut t = tokio_rustls::TlsConnector::from(tls.config.clone()).connect(name, tcp).await?;
            let mut out = String::new();
            t.read_to_string(&mut out).await?;
            Ok::<_, io::Error>(out)
        };
        let trusted = ClientTls::new(std::slice::from_ref(&s.cert), &[]).unwrap();
        assert_eq!(connect(trusted).await.unwrap(), "hi");

        let other = tmp.path().join("other");
        let o = ensure_self_signed(&other, "x", &names()).unwrap();
        let untrusted = ClientTls::new(&[o.cert], &[]).unwrap();
        assert!(connect(untrusted.clone()).await.is_err());
        assert_eq!(untrusted.rejected_fingerprint(), Some(fingerprint(&s.cert).unwrap()));
    }

    /// A server nobody trusts until once: the fingerprint the owner
    /// confirmed on first use (in pasted shape) lets it through, while
    /// WebPKI alone still refuses it, and a wrong pin stays refused with
    /// the observed fingerprint on record for the prompt (spec/gxctl-auth.md
    /// §5.2).
    #[tokio::test]
    async fn pin_accepts_after_first_use_trust() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = tempfile::TempDir::new().unwrap();
        let s = ensure_self_signed(&tmp.path().join("tls"), "cp", &names()).unwrap();
        let fp = fingerprint(&s.cert).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(server_config(&s.cert, &s.key, &[]).unwrap());
        let v4 = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = v4.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = v4.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut t) = acceptor.accept(tcp).await {
                        let _ = t.write_all(b"hi").await;
                        let _ = t.shutdown().await;
                    }
                });
            }
        });
        let connect = |tls: ClientTls| async move {
            let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let name = ServerName::try_from("localhost").unwrap();
            let mut t = tokio_rustls::TlsConnector::from(tls.config.clone()).connect(name, tcp).await?;
            let mut out = String::new();
            t.read_to_string(&mut out).await?;
            Ok::<_, io::Error>(out)
        };
        // Pasted the way a browser shows it, tag and all.
        let pinned = ClientTls::with_trust(&Trust { pins: vec![format!("SHA256/{fp}")], verify_host: true, ..Default::default() }, &[]).unwrap();
        assert_eq!(connect(pinned.clone()).await.unwrap(), "hi");
        assert_eq!(pinned.accepted_fingerprint(), Some(fp.clone()));
        // Without the pin the same server is still untrusted: the pin, not
        // convenience, is what decides.
        let bare = ClientTls::with_trust(&Trust { ca_files: vec![], verify_host: true, ..Default::default() }, &[]);
        if let Ok(bare) = bare {
            // (a host with a populated system store still won't know this
            // certificate; the assertion below holds either way)
            assert!(connect(bare.clone()).await.is_err());
            assert_eq!(bare.rejected_fingerprint(), Some(fp.clone()));
        }
        // A pin for a different certificate rescues nothing.
        let o = ensure_self_signed(&tmp.path().join("other"), "x", &names()).unwrap();
        let wrong = ClientTls::with_trust(&Trust { pins: vec![fingerprint(&o.cert).unwrap()], verify_host: true, ..Default::default() }, &[]).unwrap();
        assert!(connect(wrong.clone()).await.is_err());
        assert_eq!(wrong.rejected_fingerprint(), Some(fp), "the prompt needs the observed fingerprint");
    }

    /// The name check runs on pinned certificates too, and only the
    /// profile's `verify_host: false` waives it — together with the pin,
    /// never alone (gxctl-auth.md A4, §2.2).
    #[tokio::test]
    async fn pinned_certificates_still_meet_the_name_or_an_explicit_waiver() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = tempfile::TempDir::new().unwrap();
        let s = ensure_self_signed(&tmp.path().join("tls"), "cp", &names()).unwrap();
        let fp = fingerprint(&s.cert).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(server_config(&s.cert, &s.key, &[]).unwrap());
        let v4 = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = v4.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = v4.accept().await.unwrap();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if let Ok(mut t) = acceptor.accept(tcp).await {
                        let _ = t.write_all(b"hi").await;
                        let _ = t.shutdown().await;
                    }
                });
            }
        });
        let connect = |tls: ClientTls, name: &'static str| async move {
            let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let name = ServerName::try_from(name).unwrap();
            let mut t = tokio_rustls::TlsConnector::from(tls.config.clone()).connect(name, tcp).await?;
            let mut out = String::new();
            t.read_to_string(&mut out).await?;
            Ok::<_, io::Error>(out)
        };
        let pin = Trust { pins: vec![fp.clone()], verify_host: true, ..Default::default() };
        let strict = ClientTls::with_trust(&pin, &[]).unwrap();
        assert!(connect(strict.clone(), "nope.example").await.is_err(), "a pin is not a name");
        assert_eq!(strict.rejected_fingerprint(), Some(fp.clone()));
        let waived = ClientTls::with_trust(&Trust { verify_host: false, ..pin.clone() }, &[]).unwrap();
        assert_eq!(connect(waived, "nope.example").await.unwrap(), "hi", "the waiver is the profile's, and only with a pin");
        // Without the pin, waiving the name is refused at construction.
        assert!(ClientTls::with_trust(&Trust { verify_host: false, ..Default::default() }, &[]).is_err());
    }

    #[test]
    fn pins_normalize_from_whatever_the_browser_showed() {
        let upper = "A1B2C3D4".repeat(8);
        let hex = upper.to_lowercase();
        let colons = "a1:b2:c3:d4:".repeat(8);
        assert_eq!(Trust::normalize_pin(&format!("SHA256/{colons}")), Some(hex.clone()));
        assert_eq!(Trust::normalize_pin(&colons), Some(hex.clone()));
        assert_eq!(Trust::normalize_pin(&format!("{hex}  ")), Some(hex.clone()));
        assert_eq!(Trust::normalize_pin("sha256/deadbeef"), None);
        assert_eq!(Trust::normalize_pin(&hex[..62]), None);
    }
}
