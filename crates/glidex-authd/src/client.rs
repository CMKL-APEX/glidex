//! Blocking client for glidex-authd, used by the control plane (call it
//! from `spawn_blocking`). One connection per call.

pub use crate::proto::AuthOk;
use crate::proto::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zeroize::Zeroizing;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthdError {
    /// Wrong credentials, unknown user, not allowed, or expired account.
    #[error("authentication failed")]
    Denied,
    #[error("too many failed attempts; try again later")]
    RateLimited,
    /// No socket, connection refused or closed (e.g. the peer check
    /// failed), or a timeout.
    #[error("glidex-authd unavailable: {0}")]
    Unavailable(String),
    #[error("glidex-authd protocol error: {0}")]
    Protocol(String),
    /// authd reported `internal` (PAM is broken) or an unknown code.
    #[error("glidex-authd internal error: {0}")]
    Internal(String),
}

#[derive(Debug, Clone)]
pub struct AuthdClient {
    path: PathBuf,
    timeout: Duration,
}

impl Default for AuthdClient {
    fn default() -> Self {
        Self::new(Path::new(DEFAULT_RUN_DIR).join(SOCKET_NAME))
    }
}

impl AuthdClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            path: socket.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Read and write timeout for each call (default 10 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Verify `user`'s `password` with PAM service `service`
    /// (normally [`DEFAULT_SERVICE`]).
    pub fn authenticate(&self, user: &str, password: &str, service: &str) -> Result<AuthOk, AuthdError> {
        let unavailable = |e: std::io::Error| AuthdError::Unavailable(format!("{}: {}", self.path.display(), e));
        let stream = UnixStream::connect(&self.path).map_err(unavailable)?;
        stream.set_read_timeout(Some(self.timeout)).map_err(unavailable)?;
        stream.set_write_timeout(Some(self.timeout)).map_err(unavailable)?;

        let req = RequestRef {
            id: 1,
            op: OP_AUTHENTICATE,
            args: AuthenticateArgsRef { user, password, service },
        };
        // Sized for the worst-case JSON escaping so it never reallocates and
        // leaves an unwiped copy of the password behind.
        let mut line = Zeroizing::new(Vec::with_capacity(6 * (user.len() + password.len() + service.len()) + 128));
        serde_json::to_writer(&mut *line, &req).map_err(|e| AuthdError::Protocol(e.to_string()))?;
        line.push(b'\n');
        (&stream).write_all(&line).map_err(unavailable)?;

        let mut reader = BufReader::new(&stream).take(MAX_LINE as u64 + 1);
        let mut reply = String::new();
        let n = reader.read_line(&mut reply).map_err(unavailable)?;
        if n == 0 {
            return Err(AuthdError::Unavailable("glidex-authd closed the connection".into()));
        }
        let resp: Response = serde_json::from_str(&reply).map_err(|e| AuthdError::Protocol(e.to_string()))?;
        if let Some(err) = resp.error {
            return Err(match err.code.as_str() {
                CODE_DENIED => AuthdError::Denied,
                CODE_RATE_LIMITED => AuthdError::RateLimited,
                CODE_PROTOCOL_ERROR => AuthdError::Protocol(err.message),
                _ => AuthdError::Internal(format!("{}: {}", err.code, err.message)),
            });
        }
        if resp.id != 1 {
            return Err(AuthdError::Protocol(format!("response id {} for request 1", resp.id)));
        }
        resp.ok.ok_or_else(|| AuthdError::Protocol("response has neither ok nor error".into()))
    }
}
