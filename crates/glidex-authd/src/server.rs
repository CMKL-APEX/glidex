//! The authd socket server: peer check, request handling, rate limits.

use crate::authenticator::{AuthFailure, Accounts, Authenticator};
use crate::config::Config;
use crate::keys::{KeyReader, SystemKeyReader};
use crate::limiter::RateLimiter;
use crate::proto::*;
use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// Upper bound for the dummy delay that mirrors PAM's own failure time.
const MAX_DUMMY_PAM_TIME: Duration = Duration::from_secs(5);
const READ_CHUNK: usize = 4096;

pub struct Authd {
    config: Config,
    /// uid of `config.service_user`; `None` if that user does not exist.
    service_uid: Option<u32>,
    authenticator: Arc<dyn Authenticator>,
    accounts: Arc<dyn Accounts>,
    keys: Arc<dyn KeyReader>,
    limiter: Mutex<RateLimiter>,
    /// How long the last failed PAM transaction took. Denials that never
    /// reach PAM (unknown user, wrong group) wait this long too, so timing
    /// doesn't tell them apart.
    last_pam_failure: Mutex<Duration>,
    active: AtomicUsize,
}

impl Authd {
    pub fn new(
        config: Config,
        service_uid: Option<u32>,
        authenticator: Arc<dyn Authenticator>,
        accounts: Arc<dyn Accounts>,
    ) -> Self {
        let limiter = Mutex::new(RateLimiter::new(config.limits));
        Self {
            config,
            service_uid,
            authenticator,
            accounts,
            keys: Arc::new(SystemKeyReader),
            limiter,
            last_pam_failure: Mutex::new(Duration::ZERO),
            active: AtomicUsize::new(0),
        }
    }

    /// Read public keys with `keys` instead of the host's home directories.
    pub fn with_key_reader(mut self, keys: Arc<dyn KeyReader>) -> Self {
        self.keys = keys;
        self
    }

    /// The `public_keys` op: a login-capable user's own `~/.ssh/*.pub`.
    /// Users outside the allowed groups get the same `denied` as unknown
    /// ones. Public keys aren't secret, so no rate limit or delay.
    pub fn public_keys(&self, user: &str) -> Result<Vec<String>, ErrorBody> {
        if !valid_user_name(user) {
            return Err(ErrorBody::denied());
        }
        match self.accounts.lookup(user) {
            Some(a) if a.groups.iter().any(|g| self.config.allowed_groups.contains(g)) => {}
            _ => return Err(ErrorBody::denied()),
        }
        self.keys.public_keys(user).map_err(|e| {
            tracing::warn!(user, error = %e, "reading public keys");
            ErrorBody::new(CODE_INTERNAL, "could not read the user's public keys")
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Only the service user's uid (not group members), plus root if
    /// `allow_root` is set.
    pub fn peer_allowed(&self, uid: u32) -> bool {
        Some(uid) == self.service_uid || (uid == 0 && self.config.allow_root)
    }

    fn limiter(&self) -> std::sync::MutexGuard<'_, RateLimiter> {
        self.limiter.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record a failure and wait before answering `denied`.
    fn fail(&self, user: Option<&str>, dummy_pam: bool) -> ErrorBody {
        self.limiter().record_failure(user, Instant::now());
        let mut delay = self.config.failure_delay;
        if dummy_pam {
            delay += *self.last_pam_failure.lock().unwrap_or_else(|p| p.into_inner());
        }
        std::thread::sleep(delay);
        ErrorBody::denied()
    }

    /// Handle one `authenticate` request. Blocks for PAM and for the
    /// failure delay.
    pub fn authenticate(&self, args: &AuthenticateArgs) -> Result<AuthOk, ErrorBody> {
        let service = args.service.as_deref().unwrap_or(DEFAULT_SERVICE);
        if !valid_service_name(service) {
            return Err(ErrorBody::new(CODE_PROTOCOL_ERROR, "invalid service name"));
        }
        let user = args.user.as_str();
        if !valid_user_name(user) {
            // Not logged: people type passwords into the user field.
            tracing::info!(service, outcome = "denied", reason = "invalid user name", "authentication");
            return Err(self.fail(None, true));
        }
        if !self.limiter().allowed(user, Instant::now()) {
            tracing::warn!(user, service, outcome = "rate_limited", "authentication");
            std::thread::sleep(self.config.failure_delay);
            return Err(ErrorBody::new(CODE_RATE_LIMITED, "too many failed attempts; try again later"));
        }
        let account = match self.accounts.lookup(user) {
            Some(a) if a.groups.iter().any(|g| self.config.allowed_groups.contains(g)) => a,
            other => {
                let reason = if other.is_none() { "unknown user" } else { "not in an allowed group" };
                tracing::info!(user, service, outcome = "denied", reason, "authentication");
                return Err(self.fail(Some(user), true));
            }
        };
        let started = Instant::now();
        match self.authenticator.authenticate(service, user, args.password.as_str()) {
            Ok(()) => {
                self.limiter().record_success(user);
                tracing::info!(user, service, uid = account.uid, outcome = "success", "authentication");
                Ok(AuthOk {
                    uid: account.uid,
                    groups: account.groups,
                })
            }
            Err(AuthFailure::Denied(reason)) => {
                let took = started.elapsed().min(MAX_DUMMY_PAM_TIME);
                *self.last_pam_failure.lock().unwrap_or_else(|p| p.into_inner()) = took;
                tracing::info!(user, service, outcome = "denied", reason = %reason, "authentication");
                Err(self.fail(Some(user), false))
            }
            Err(AuthFailure::Internal(reason)) => {
                tracing::error!(user, service, outcome = "internal", reason = %reason, "authentication");
                Err(ErrorBody::new(CODE_INTERNAL, "authentication is unavailable"))
            }
        }
    }

    fn handle_line(&self, line: &[u8]) -> Response {
        let req: Request = match serde_json::from_slice(line) {
            Ok(r) => r,
            // serde's message may quote the input; report only the position.
            Err(e) => {
                return Response::err(
                    0,
                    ErrorBody::new(
                        CODE_PROTOCOL_ERROR,
                        format!("malformed request at line {} column {}", e.line(), e.column()),
                    ),
                )
            }
        };
        let Some(args) = req.args.as_ref() else {
            return Response::err(req.id, ErrorBody::new(CODE_PROTOCOL_ERROR, "missing args"));
        };
        match req.op.as_str() {
            OP_AUTHENTICATE => match self.authenticate(args) {
                Ok(ok) => Response::ok(req.id, ok),
                Err(e) => Response::err(req.id, e),
            },
            OP_PUBLIC_KEYS => match self.public_keys(&args.user) {
                Ok(keys) => Response::keys(req.id, keys),
                Err(e) => Response::err(req.id, e),
            },
            _ => Response::err(req.id, ErrorBody::new(CODE_PROTOCOL_ERROR, "unknown op")),
        }
    }

    /// Serve requests on an accepted, already authorized connection until
    /// EOF, an I/O error, an idle timeout or an oversize line.
    pub fn serve_connection(&self, stream: UnixStream) {
        let _ = stream.set_read_timeout(Some(self.config.idle_timeout));
        let _ = stream.set_write_timeout(Some(self.config.idle_timeout));
        let mut writer = match stream.try_clone() {
            Ok(w) => w,
            Err(_) => return,
        };
        let mut reader = LineReader::new(stream);
        loop {
            let resp = match reader.next_line() {
                Ok(Line::Complete(line)) => self.handle_line(&line),
                Ok(Line::TooLong) => {
                    let _ = write_line(
                        &mut writer,
                        &Response::err(0, ErrorBody::new(CODE_PROTOCOL_ERROR, "request too large")),
                    );
                    return;
                }
                Ok(Line::Eof) | Err(_) => return,
            };
            if write_line(&mut writer, &resp).is_err() {
                return;
            }
        }
    }

    /// Accept connections forever: check the peer, cap concurrency, one
    /// thread per connection.
    pub fn serve(self: Arc<Self>, listener: UnixListener) {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    continue;
                }
            };
            let peer = match getsockopt(&stream, PeerCredentials) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "could not read peer credentials");
                    continue;
                }
            };
            if !self.peer_allowed(peer.uid()) {
                tracing::warn!(uid = peer.uid(), pid = peer.pid(), "rejected peer: not the service user");
                continue;
            }
            if self.active.fetch_add(1, Ordering::SeqCst) >= self.config.max_connections {
                self.active.fetch_sub(1, Ordering::SeqCst);
                tracing::warn!(uid = peer.uid(), pid = peer.pid(), "too many connections");
                let mut s = stream;
                let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
                let _ = write_line(&mut s, &Response::err(0, ErrorBody::new(CODE_RATE_LIMITED, "too many connections")));
                continue;
            }
            let authd = self.clone();
            let spawned = std::thread::Builder::new()
                .name("authd-conn".into())
                .spawn(move || {
                    struct Slot<'a>(&'a AtomicUsize);
                    impl Drop for Slot<'_> {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                    let _slot = Slot(&authd.active);
                    authd.serve_connection(stream);
                });
            if let Err(e) = spawned {
                self.active.fetch_sub(1, Ordering::SeqCst);
                tracing::error!(error = %e, "cannot spawn connection thread");
            }
        }
    }
}

fn write_line(stream: &mut UnixStream, resp: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(resp).map_err(std::io::Error::other)?;
    line.push(b'\n');
    stream.write_all(&line)
}

enum Line {
    Complete(Zeroizing<Vec<u8>>),
    Eof,
    TooLong,
}

/// Newline-delimited reader whose buffers are zeroized: request lines
/// carry passwords, and `BufReader`'s buffer would keep copies.
struct LineReader<R> {
    inner: R,
    /// Allocated once at full size so it never reallocates (which would
    /// leave unwiped copies behind).
    buf: Zeroizing<Vec<u8>>,
}

impl<R: Read> LineReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Zeroizing::new(Vec::with_capacity(MAX_LINE + 1 + READ_CHUNK)),
        }
    }

    fn next_line(&mut self) -> std::io::Result<Line> {
        let mut scanned = 0;
        loop {
            if let Some(pos) = self.buf[scanned..].iter().position(|b| *b == b'\n').map(|p| p + scanned) {
                if pos > MAX_LINE {
                    return Ok(Line::TooLong);
                }
                let mut line = Zeroizing::new(Vec::with_capacity(pos));
                line.extend_from_slice(&self.buf[..pos]);
                self.buf.drain(..=pos);
                return Ok(Line::Complete(line));
            }
            scanned = self.buf.len();
            if self.buf.len() > MAX_LINE {
                return Ok(Line::TooLong);
            }
            let start = self.buf.len();
            self.buf.resize(start + READ_CHUNK, 0);
            let n = match self.inner.read(&mut self.buf[start..]) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    self.buf.truncate(start);
                    continue;
                }
                Err(e) => {
                    self.buf.truncate(start);
                    return Err(e);
                }
            };
            self.buf.truncate(start + n);
            if n == 0 {
                // An unterminated last line is dropped.
                return Ok(Line::Eof);
            }
        }
    }
}

/// Bind `path` (replacing a stale socket) with `mode` and, if possible,
/// group `gid` (only root can chown; tests run unprivileged).
pub fn bind(path: &Path, mode: u32, gid: Option<u32>) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    if let Some(gid) = gid {
        let _ = nix::unistd::chown(path, None, Some(nix::unistd::Gid::from_raw(gid)));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Chunks(Vec<Vec<u8>>);
    impl Read for Chunks {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let mut c = self.0.remove(0);
            let n = c.len().min(out.len());
            out[..n].copy_from_slice(&c[..n]);
            if n < c.len() {
                self.0.insert(0, c.split_off(n));
            }
            Ok(n)
        }
    }

    fn lines(r: &mut LineReader<Chunks>) -> Vec<String> {
        let mut v = vec![];
        loop {
            match r.next_line().unwrap() {
                Line::Complete(l) => v.push(String::from_utf8(l.to_vec()).unwrap()),
                Line::TooLong => v.push("<too long>".into()),
                Line::Eof => return v,
            }
        }
    }

    #[test]
    fn line_reader_splits_and_joins() {
        let mut r = LineReader::new(Chunks(vec![b"ab".to_vec(), b"c\nde\n".to_vec(), b"f\n".to_vec()]));
        assert_eq!(lines(&mut r), vec!["abc", "de", "f"]);
    }

    #[test]
    fn line_reader_limits() {
        let mut exact = vec![b'x'; MAX_LINE];
        exact.push(b'\n');
        let mut r = LineReader::new(Chunks(vec![exact]));
        assert_eq!(lines(&mut r).len(), 1);
        let mut r = LineReader::new(Chunks(vec![vec![b'x'; MAX_LINE + 10]]));
        assert!(matches!(r.next_line().unwrap(), Line::TooLong));
        let cap = r.buf.capacity();
        assert!(cap >= MAX_LINE + 1 + READ_CHUNK);
    }
}
