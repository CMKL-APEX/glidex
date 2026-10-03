//! Configuration: `/etc/glidex/authd.json` plus environment overrides.
//!
//! ```json
//! {
//!   "service_user": "glidex",
//!   "allow_root": false,
//!   "allowed_groups": ["glidex-users"],
//!   "failure_delay_ms": 1000,
//!   "max_connections": 8
//! }
//! ```
//!
//! Environment: `GLIDEX_AUTHD_RUN_DIR` (socket directory when not
//! socket-activated), `GLIDEX_AUTHD_SERVICE_USER`.

use crate::limiter::Limits;
use crate::proto::DEFAULT_RUN_DIR;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/glidex/authd.json";
pub const DEFAULT_SERVICE_USER: &str = "glidex";
pub const ENV_RUN_DIR: &str = "GLIDEX_AUTHD_RUN_DIR";
pub const ENV_SERVICE_USER: &str = "GLIDEX_AUTHD_SERVICE_USER";

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    service_user: Option<String>,
    allow_root: Option<bool>,
    allowed_groups: Option<Vec<String>>,
    failure_delay_ms: Option<u64>,
    max_connections: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// The only user (by uid) allowed to connect: the control plane's.
    pub service_user: String,
    /// Also accept uid 0 (debugging only).
    pub allow_root: bool,
    /// A user must be in one of these groups to log in.
    pub allowed_groups: Vec<String>,
    /// Fixed delay before every failure response.
    pub failure_delay: Duration,
    /// Connections served at once; more are refused.
    pub max_connections: usize,
    /// How long a connection may sit idle between requests.
    pub idle_timeout: Duration,
    pub limits: Limits,
    pub run_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            service_user: DEFAULT_SERVICE_USER.to_string(),
            allow_root: false,
            allowed_groups: vec!["glidex-users".to_string()],
            failure_delay: Duration::from_secs(1),
            max_connections: 8,
            idle_timeout: Duration::from_secs(30),
            limits: Limits::default(),
            run_dir: PathBuf::from(DEFAULT_RUN_DIR),
        }
    }
}

impl Config {
    /// Read `path` (a missing file means defaults; an unreadable or invalid
    /// one is an error), then apply the environment overrides.
    pub fn load(path: &Path) -> Result<Self, String> {
        let file = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<FileConfig>(&bytes).map_err(|e| format!("{}: {}", path.display(), e))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileConfig::default(),
            Err(e) => return Err(format!("{}: {}", path.display(), e)),
        };
        let mut config = Config::default();
        if let Some(u) = file.service_user {
            config.service_user = u;
        }
        if let Some(r) = file.allow_root {
            config.allow_root = r;
        }
        if let Some(g) = file.allowed_groups {
            config.allowed_groups = g;
        }
        if let Some(ms) = file.failure_delay_ms {
            config.failure_delay = Duration::from_millis(ms);
        }
        if let Some(n) = file.max_connections {
            config.max_connections = n.max(1);
        }
        if let Some(d) = std::env::var_os(ENV_RUN_DIR).filter(|d| !d.is_empty()) {
            config.run_dir = PathBuf::from(d);
        }
        if let Some(u) = std::env::var(ENV_SERVICE_USER).ok().filter(|u| !u.is_empty()) {
            config.service_user = u;
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_defaults() {
        let c = Config::load(Path::new("/nonexistent/authd.json")).unwrap();
        assert_eq!(c.allowed_groups, vec!["glidex-users"]);
        assert!(!c.allow_root);
        assert_eq!(c.failure_delay, Duration::from_secs(1));
    }

    #[test]
    fn file_values_and_unknown_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("authd.json");
        std::fs::write(&p, r#"{"allow_root":true,"allowed_groups":["a","b"],"failure_delay_ms":5}"#).unwrap();
        let c = Config::load(&p).unwrap();
        assert!(c.allow_root);
        assert_eq!(c.allowed_groups, vec!["a", "b"]);
        assert_eq!(c.failure_delay, Duration::from_millis(5));
        std::fs::write(&p, r#"{"allowed_group":["typo"]}"#).unwrap();
        assert!(Config::load(&p).is_err());
    }
}
