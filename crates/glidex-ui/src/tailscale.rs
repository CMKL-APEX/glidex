//! The node's Tailscale certificates (spec/tailscale-tls.md §4.2).
//!
//! With `GLIDEX_UI_TLS_TAILSCALE=on`, the UI asks the host's `tailscaled`
//! (its local API on a Unix socket) for the node's certificate domains and
//! a certificate for each, and serves them to clients whose SNI names one
//! (`glidex_tls::Resolver`); everything else keeps the usual certificate.
//! `tailscaled` renews only when asked, so this polls. Nothing here fails
//! a handshake: without a usable Tailscale certificate the ts.net names
//! get the default one (T5). The key never touches disk here.

use axum::body::Body;
use axum::http::{StatusCode, header};
use glidex_tls::{CertSlot, Resolver};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_SOCKET: &str = "/run/tailscale/tailscaled.sock";
/// The `Host` `tailscaled` expects on its local API.
const LOCAL_API_HOST: &str = "local-tailscaled.sock";
/// How often to ask, so `tailscaled` starts renewals when they are due.
const POLL: Duration = Duration::from_secs(6 * 3600);
/// First retry after an error; doubled up to `POLL`.
const RETRY_MIN: Duration = Duration::from_secs(60);
/// Below this much validity, ask for a synchronous renewal...
const RENEW_BELOW_SECS: i64 = 14 * 86400;
/// ...that leaves at least this much (a Go duration, as `tailscaled` reads it).
const MIN_VALIDITY: &str = "336h";
/// An ACME issuance can take a while.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const BODY_LIMIT: usize = 1 << 20;

/// `GLIDEX_UI_TLS_TAILSCALE`: `off` (default) or `on`.
pub fn enabled(v: Option<&str>) -> Result<bool, String> {
    match v {
        None | Some("off") => Ok(false),
        Some("on") => Ok(true),
        Some(o) => Err(format!("GLIDEX_UI_TLS_TAILSCALE: '{o}' is not on or off")),
    }
}

/// The ts.net names a Tailscale certificate is held for right now: the
/// `Host` allowlist takes them, and connections served one get HSTS
/// (spec/tailscale-tls.md §4.3, §4.4).
#[derive(Debug, Default)]
pub struct Held(RwLock<Vec<String>>);

impl Held {
    /// `name`: a host name without port, any case.
    pub fn contains(&self, name: &str) -> bool {
        let name = name.trim_end_matches('.');
        self.0
            .read()
            .unwrap()
            .iter()
            .any(|n| n.eq_ignore_ascii_case(name))
    }

    fn set(&self, names: Vec<String>) {
        *self.0.write().unwrap() = names;
    }
}

/// Why a refresh didn't finish, and when to try again.
#[derive(Debug)]
struct Failure {
    msg: String,
    retry_after: Option<Duration>,
}

impl From<String> for Failure {
    fn from(msg: String) -> Self {
        Failure {
            msg,
            retry_after: None,
        }
    }
}

struct Slot {
    slot: Arc<CertSlot>,
    fingerprint: String,
    not_after: i64,
}

pub struct Tailscale {
    socket: PathBuf,
    resolver: Arc<Resolver>,
    held: Arc<Held>,
    slots: BTreeMap<String, Slot>,
    last_error: Option<String>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `d` ± 10 %, so hosts don't ask in step.
fn jitter(d: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|t| t.subsec_nanos())
        .unwrap_or(0);
    let frac = (nanos % 2001) as f64 / 10_000.0 - 0.1; // -0.1 ..= 0.1
    d.mul_f64(1.0 + frac)
}

/// Whether a certificate DNS name covers `domain` (exact, or a
/// single-label wildcard).
fn covers(names: &[String], domain: &str) -> bool {
    names.iter().any(|n| {
        n == domain
            || n.strip_prefix("*.").is_some_and(|base| {
                domain
                    .split_once('.')
                    .is_some_and(|(label, rest)| !label.is_empty() && rest == base)
            })
    })
}

impl Tailscale {
    pub fn new(socket: PathBuf, resolver: Arc<Resolver>) -> Self {
        Tailscale {
            socket,
            resolver,
            held: Arc::default(),
            slots: BTreeMap::new(),
            last_error: None,
        }
    }

    pub fn held(&self) -> Arc<Held> {
        self.held.clone()
    }

    /// Fetch now, then keep polling. Errors are logged when they change.
    pub async fn run(mut self) {
        let mut backoff = RETRY_MIN;
        loop {
            let wait = match self.refresh().await {
                Ok(()) => {
                    if self.last_error.take().is_some() {
                        tracing::info!("Tailscale certificates: working again");
                    }
                    backoff = RETRY_MIN;
                    jitter(POLL)
                }
                Err(f) => {
                    if self.last_error.as_deref() != Some(f.msg.as_str()) {
                        tracing::warn!(
                            "Tailscale certificates: {} (ts.net names get the default certificate meanwhile)",
                            f.msg
                        );
                        self.last_error = Some(f.msg);
                    }
                    let wait = f.retry_after.unwrap_or(backoff);
                    backoff = (backoff * 2).min(POLL);
                    wait
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// One round: the current domains, a certificate for each, slots for
    /// domains that went away (or whose certificate expired) dropped.
    async fn refresh(&mut self) -> Result<(), Failure> {
        let res = self.refresh_domains().await;
        // Also when tailscaled can't be reached: an expired certificate is
        // worse than the default one.
        let now = now_secs();
        let expired: Vec<String> = self
            .slots
            .iter()
            .filter(|(_, s)| s.not_after <= now)
            .map(|(d, _)| d.clone())
            .collect();
        for d in expired {
            self.drop_slot(&d, "its certificate expired and could not be renewed");
        }
        self.held.set(self.slots.keys().cloned().collect());
        res
    }

    async fn refresh_domains(&mut self) -> Result<(), Failure> {
        let mut domains: Vec<String> = self
            .cert_domains()
            .await?
            .into_iter()
            .map(|d| d.trim_end_matches('.').to_ascii_lowercase())
            .filter(|d| !d.is_empty() && !d.starts_with("*."))
            .collect();
        domains.sort();
        domains.dedup();

        let gone: Vec<String> = self
            .slots
            .keys()
            .filter(|d| !domains.contains(d))
            .cloned()
            .collect();
        for d in gone {
            self.drop_slot(&d, "no longer a certificate domain of this node");
        }

        let mut failed: Option<Failure> = None;
        for d in &domains {
            if let Err(f) = self.refresh_domain(d).await {
                let f = Failure {
                    msg: format!("{d}: {}", f.msg),
                    ..f
                };
                // The first error is reported, unless a later one is a rate
                // limit: that decides when to come back.
                let replace = match &failed {
                    None => true,
                    Some(p) => p.retry_after.is_none() && f.retry_after.is_some(),
                };
                if replace {
                    failed = Some(f);
                }
            }
        }

        if domains.is_empty() {
            return Err(Failure::from(
                "tailscaled reports no certificate domains (HTTPS certificates off for the tailnet, or not logged in)"
                    .to_string(),
            ));
        }
        failed.map_or(Ok(()), Err)
    }

    async fn refresh_domain(&mut self, domain: &str) -> Result<(), Failure> {
        let mut key = self.fetch(domain, None).await?;
        let (_, not_after) = glidex_tls::leaf_names(&key)?;
        if not_after - now_secs() < RENEW_BELOW_SECS {
            key = self.fetch(domain, Some(MIN_VALIDITY)).await?;
        }
        let (names, not_after) = glidex_tls::leaf_names(&key)?;
        if !covers(&names, domain) {
            return Err(format!("the certificate is for {names:?}, not this name").into());
        }
        if not_after <= now_secs() {
            return Err("tailscaled returned an expired certificate"
                .to_string()
                .into());
        }
        let fingerprint = glidex_tls::fingerprint_certified(&key);
        match self.slots.get_mut(domain) {
            Some(s) if s.fingerprint == fingerprint => return Ok(()),
            Some(s) => {
                s.slot.set(key);
                s.fingerprint = fingerprint.clone();
                s.not_after = not_after;
            }
            None => {
                let slot = CertSlot::new(key);
                self.resolver.set_named(domain, Some(slot.clone()));
                self.slots.insert(
                    domain.to_string(),
                    Slot {
                        slot,
                        fingerprint: fingerprint.clone(),
                        not_after,
                    },
                );
            }
        }
        tracing::info!(
            domain,
            %fingerprint,
            expires = not_after,
            "Tailscale certificate"
        );
        Ok(())
    }

    fn drop_slot(&mut self, domain: &str, why: &str) {
        self.slots.remove(domain);
        self.resolver.set_named(domain, None);
        tracing::info!(domain, "Tailscale certificate dropped: {why}");
    }

    /// The node's certificate domains: `cert-domains`, or `CertDomains` in
    /// `status` on a `tailscaled` without it (T11).
    async fn cert_domains(&self) -> Result<Vec<String>, Failure> {
        let (status, _, body) = self.get("/localapi/v0/cert-domains").await?;
        match status {
            StatusCode::OK => serde_json::from_slice::<Option<Vec<String>>>(&body)
                .map(Option::unwrap_or_default)
                .map_err(|e| format!("cert-domains: {e}").into()),
            StatusCode::NOT_FOUND => {
                let (status, _, body) = self.get("/localapi/v0/status").await?;
                if status != StatusCode::OK {
                    return Err(error_of("status", status, &body).into());
                }
                let v: serde_json::Value =
                    serde_json::from_slice(&body).map_err(|e| format!("status: {e}"))?;
                Ok(v["CertDomains"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|d| d.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default())
            }
            s => Err(error_of("cert-domains", s, &body).into()),
        }
    }

    /// `GET /localapi/v0/cert/<domain>?type=pair`: the key, then the chain.
    async fn fetch(
        &self,
        domain: &str,
        min_validity: Option<&str>,
    ) -> Result<Arc<glidex_tls::tokio_rustls::rustls::sign::CertifiedKey>, Failure> {
        let mut path = format!("/localapi/v0/cert/{domain}?type=pair");
        if let Some(v) = min_validity {
            path.push_str(&format!("&min_validity={v}"));
        }
        let (status, headers, body) = self.get(&path).await?;
        match status {
            StatusCode::OK => Ok(glidex_tls::certified_key_pem(&body)?),
            StatusCode::TOO_MANY_REQUESTS => Err(Failure {
                msg: error_of("certificate", status, &body),
                retry_after: headers
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs),
            }),
            StatusCode::FORBIDDEN => Err(format!(
                "{} (tailscaled must run with TS_PERMIT_CERT_UID set to this service's user)",
                error_of("certificate", status, &body)
            )
            .into()),
            s => Err(error_of("certificate", s, &body).into()),
        }
    }

    async fn get(
        &self,
        path: &str,
    ) -> Result<(StatusCode, axum::http::HeaderMap, Vec<u8>), Failure> {
        let sock = self.socket.display().to_string();
        let exchange = async {
            let io = tokio::net::UnixStream::connect(&self.socket)
                .await
                .map_err(|e| format!("{sock}: {e}"))?;
            let req = axum::http::Request::get(path)
                .header(header::HOST, LOCAL_API_HOST)
                .body(Body::empty())
                .map_err(|e| e.to_string())?;
            let resp = crate::exchange_on(io, req)
                .await
                .map_err(|e| format!("{sock}: {e}"))?;
            let (parts, body) = resp.into_parts();
            let body = axum::body::to_bytes(Body::new(body), BODY_LIMIT)
                .await
                .map_err(|e| format!("{sock}: {e}"))?;
            Ok::<_, String>((parts.status, parts.headers, body.to_vec()))
        };
        tokio::time::timeout(REQUEST_TIMEOUT, exchange)
            .await
            .map_err(|_| format!("{sock}: no answer in {}s", REQUEST_TIMEOUT.as_secs()))?
            .map_err(Failure::from)
    }
}

fn error_of(what: &str, status: StatusCode, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    let text: String = text.chars().take(200).collect();
    format!("{what}: {status} {text}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::{Path as UrlPath, Query, State};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// What the fake `tailscaled` answers, changeable mid-test.
    #[derive(Default)]
    pub(crate) struct Fake {
        pub(crate) domains: Option<Vec<String>>,
        /// No `cert-domains` endpoint (an older daemon).
        pub(crate) old: bool,
        /// `domain` → PEM pair; a missing domain gets `403`.
        pub(crate) pairs: HashMap<String, String>,
        /// Served after a `min_validity` request.
        pub(crate) renewed: HashMap<String, String>,
        pub(crate) rate_limited: bool,
        pub(crate) min_validity_seen: Vec<String>,
    }

    pub(crate) type Shared = Arc<Mutex<Fake>>;

    async fn cert_domains(State(f): State<Shared>) -> Response {
        let f = f.lock().unwrap();
        if f.old {
            return StatusCode::NOT_FOUND.into_response();
        }
        axum::Json(f.domains.clone()).into_response()
    }

    async fn status(State(f): State<Shared>) -> Response {
        let f = f.lock().unwrap();
        axum::Json(serde_json::json!({"BackendState": "Running", "CertDomains": f.domains}))
            .into_response()
    }

    async fn cert(
        State(f): State<Shared>,
        UrlPath(domain): UrlPath<String>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        let mut f = f.lock().unwrap();
        assert_eq!(q.get("type").map(String::as_str), Some("pair"));
        if f.rate_limited {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "120")],
                "rate limited",
            )
                .into_response();
        }
        if let Some(v) = q.get("min_validity") {
            f.min_validity_seen.push(v.clone());
            if let Some(p) = f.renewed.get(&domain) {
                return p.clone().into_response();
            }
        }
        match f.pairs.get(&domain) {
            Some(p) => p.clone().into_response(),
            None => (StatusCode::FORBIDDEN, "cert access denied\n").into_response(),
        }
    }

    /// A fake `tailscaled` on a Unix socket in `dir`.
    pub(crate) fn serve_fake(dir: &std::path::Path, fake: Shared) -> PathBuf {
        let sock = dir.join("tailscaled.sock");
        let l = tokio::net::UnixListener::bind(&sock).unwrap();
        let app = Router::new()
            .route("/localapi/v0/cert-domains", get(cert_domains))
            .route("/localapi/v0/status", get(status))
            .route("/localapi/v0/cert/{domain}", get(cert))
            .with_state(fake);
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        sock
    }

    /// A key-first PEM pair for `names`, valid for `days` (from yesterday).
    pub(crate) fn pair(names: &[&str], days: i64) -> String {
        use rcgen::{CertificateParams, KeyPair};
        let key = KeyPair::generate().unwrap();
        let mut p = CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
            .unwrap();
        let now = time::OffsetDateTime::now_utc();
        p.not_before = now - time::Duration::days(1);
        p.not_after = now + time::Duration::days(days);
        let cert = p.self_signed(&key).unwrap();
        format!("{}{}", key.serialize_pem(), cert.pem())
    }

    fn fp_of(pem: &str) -> String {
        glidex_tls::fingerprint_pem(pem.as_bytes()).unwrap()
    }

    fn setup(fake: Fake) -> (tempfile::TempDir, Shared, Tailscale, Arc<Resolver>) {
        let dir = tempfile::TempDir::new().unwrap();
        let shared = Arc::new(Mutex::new(fake));
        let sock = serve_fake(dir.path(), shared.clone());
        let default = glidex_tls::certified_key_pem(pair(&["localhost"], 30).as_bytes()).unwrap();
        let resolver = Resolver::new(CertSlot::new(default));
        let ts = Tailscale::new(sock, resolver.clone());
        (dir, shared, ts, resolver)
    }

    fn served(r: &Resolver, sni: &str) -> String {
        glidex_tls::fingerprint_certified(&r.pick(Some(sni)))
    }

    #[tokio::test]
    async fn fetches_serves_and_follows_renames() {
        let a = pair(&["node.tail.ts.net"], 80);
        let (_d, fake, mut ts, r) = setup(Fake {
            domains: Some(vec![
                "Node.Tail.ts.net.".into(),
                "*.node.tail.ts.net".into(),
            ]),
            pairs: HashMap::from([("node.tail.ts.net".into(), a.clone())]),
            ..Default::default()
        });
        let held = ts.held();
        assert!(!held.contains("node.tail.ts.net"));
        ts.refresh().await.unwrap();
        assert_eq!(served(&r, "node.tail.ts.net"), fp_of(&a));
        assert_ne!(served(&r, "other.example"), fp_of(&a));
        assert!(held.contains("NODE.tail.ts.net"));
        assert!(fake.lock().unwrap().min_validity_seen.is_empty());

        // Renamed: the old name loses its slot, the new one gains one.
        let b = pair(&["box.tail.ts.net"], 80);
        {
            let mut f = fake.lock().unwrap();
            f.domains = Some(vec!["box.tail.ts.net".into()]);
            f.pairs.insert("box.tail.ts.net".into(), b.clone());
        }
        ts.refresh().await.unwrap();
        assert_eq!(served(&r, "box.tail.ts.net"), fp_of(&b));
        assert_ne!(served(&r, "node.tail.ts.net"), fp_of(&a));
        assert!(!held.contains("node.tail.ts.net"));
        assert!(held.contains("box.tail.ts.net"));
    }

    #[tokio::test]
    async fn renews_synchronously_below_fourteen_days() {
        let old = pair(&["node.tail.ts.net"], 5);
        let new = pair(&["node.tail.ts.net"], 90);
        let (_d, fake, mut ts, r) = setup(Fake {
            domains: Some(vec!["node.tail.ts.net".into()]),
            pairs: HashMap::from([("node.tail.ts.net".into(), old)]),
            renewed: HashMap::from([("node.tail.ts.net".into(), new.clone())]),
            ..Default::default()
        });
        ts.refresh().await.unwrap();
        assert_eq!(served(&r, "node.tail.ts.net"), fp_of(&new));
        assert_eq!(fake.lock().unwrap().min_validity_seen, ["336h"]);
    }

    #[tokio::test]
    async fn older_daemon_without_cert_domains() {
        let a = pair(&["node.tail.ts.net"], 80);
        let (_d, _f, mut ts, r) = setup(Fake {
            old: true,
            domains: Some(vec!["node.tail.ts.net".into()]),
            pairs: HashMap::from([("node.tail.ts.net".into(), a.clone())]),
            ..Default::default()
        });
        ts.refresh().await.unwrap();
        assert_eq!(served(&r, "node.tail.ts.net"), fp_of(&a));
    }

    #[tokio::test]
    async fn failures_keep_the_held_certificate() {
        let a = pair(&["node.tail.ts.net"], 80);
        let (_d, fake, mut ts, r) = setup(Fake {
            domains: Some(vec!["node.tail.ts.net".into()]),
            pairs: HashMap::from([("node.tail.ts.net".into(), a.clone())]),
            ..Default::default()
        });
        ts.refresh().await.unwrap();

        // Rate limited: the held one stays, and Retry-After decides.
        fake.lock().unwrap().rate_limited = true;
        let f = ts.refresh().await.unwrap_err();
        assert_eq!(f.retry_after, Some(Duration::from_secs(120)));
        assert_eq!(served(&r, "node.tail.ts.net"), fp_of(&a));
        assert!(ts.held().contains("node.tail.ts.net"));

        // A certificate for another name is refused.
        {
            let mut f = fake.lock().unwrap();
            f.rate_limited = false;
            f.pairs
                .insert("node.tail.ts.net".into(), pair(&["evil.example"], 80));
        }
        let f = ts.refresh().await.unwrap_err();
        assert!(f.msg.contains("not this name"), "{}", f.msg);
        assert_eq!(served(&r, "node.tail.ts.net"), fp_of(&a));

        // tailscaled gone: same.
        std::fs::remove_file(&ts.socket).unwrap();
        assert!(ts.refresh().await.is_err());
        assert_eq!(served(&r, "node.tail.ts.net"), fp_of(&a));

        // ...until the held certificate expires: then the default again.
        ts.slots.get_mut("node.tail.ts.net").unwrap().not_after = now_secs() - 1;
        assert!(ts.refresh().await.is_err());
        assert_ne!(served(&r, "node.tail.ts.net"), fp_of(&a));
        assert!(!ts.held().contains("node.tail.ts.net"));
    }

    #[tokio::test]
    async fn not_permitted_or_no_domains() {
        let (_d, fake, mut ts, r) = setup(Fake {
            domains: Some(vec!["node.tail.ts.net".into()]),
            ..Default::default()
        });
        let default = served(&r, "node.tail.ts.net");
        let f = ts.refresh().await.unwrap_err();
        assert!(f.msg.contains("TS_PERMIT_CERT_UID"), "{}", f.msg);
        assert_eq!(served(&r, "node.tail.ts.net"), default);
        assert!(!ts.held().contains("node.tail.ts.net"));

        fake.lock().unwrap().domains = None;
        let f = ts.refresh().await.unwrap_err();
        assert!(f.msg.contains("no certificate domains"), "{}", f.msg);
    }

    #[test]
    fn names_and_settings() {
        let n = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(covers(&n(&["a.b.ts.net"]), "a.b.ts.net"));
        assert!(covers(&n(&["*.b.ts.net"]), "a.b.ts.net"));
        assert!(!covers(&n(&["*.b.ts.net"]), "x.a.b.ts.net"));
        assert!(!covers(&n(&["*.b.ts.net"]), "b.ts.net"));
        assert!(!covers(&n(&["c.b.ts.net"]), "a.b.ts.net"));
        assert!(!enabled(None).unwrap());
        assert!(!enabled(Some("off")).unwrap());
        assert!(enabled(Some("on")).unwrap());
        assert!(enabled(Some("yes")).is_err());
        for _ in 0..100 {
            let j = jitter(Duration::from_secs(1000)).as_secs_f64();
            assert!((899.0..=1101.0).contains(&j), "{j}");
        }
    }
}
