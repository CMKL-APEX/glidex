//! `/etc/glidex/control-plane.json` (spec/security.md §13).
//!
//! The file holds no secrets: the TLS key, the OIDC client secret and the
//! cloud-init password hash come from systemd credentials
//! (`$CREDENTIALS_DIRECTORY/<name>`). A missing file means defaults; an
//! invalid one stops the control plane, so a typo can't silently widen
//! access.

use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/glidex/control-plane.json";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// TCP listeners. Non-loopback addresses need `tls`.
    pub listen: Vec<SocketAddr>,
    pub tls: Option<TlsConfig>,
    /// Unix socket for local gxctl users (peer identity, spec §5.2).
    /// Default: `<run dir>/api.sock`.
    pub api_socket: Option<PathBuf>,
    /// Unix socket for glidex-ui (spec §5.6). Default: `<run dir>/ui.sock`.
    pub ui_socket: Option<PathBuf>,
    /// The user glidex-ui runs as; only it may use `ui_socket`.
    pub ui_user: String,
    /// Group whose members may use `api_socket`.
    pub users_group: String,
    /// Group whose members are break-glass administrators on `api_socket`.
    pub admin_group: String,
    pub auth: AuthConfig,
    pub authz: AuthzConfig,
    pub quotas: QuotasConfig,
    pub pci: PciConfig,
    pub audit: AuditConfig,
    pub reconcile: ReconcileConfig,
    pub console: ConsoleConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: vec![
                SocketAddr::from(([127, 0, 0, 1], 8841)),
                SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 8841)),
            ],
            tls: None,
            api_socket: None,
            ui_socket: None,
            ui_user: "glidex-ui".into(),
            users_group: "glidex-users".into(),
            admin_group: "glidex-admin".into(),
            auth: AuthConfig::default(),
            authz: AuthzConfig::default(),
            quotas: QuotasConfig::default(),
            pci: PciConfig::default(),
            audit: AuditConfig::default(),
            reconcile: ReconcileConfig::default(),
            console: ConsoleConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: PathBuf,
    /// Default: the systemd credential `tls-key`.
    #[serde(default)]
    pub key: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    /// Origins allowed on state-changing requests and WebSocket upgrades.
    pub allowed_origins: Vec<String>,
    pub session: SessionConfig,
    pub tokens: TokenConfig,
    pub pam: PamConfig,
    pub oidc: OidcConfig,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            allowed_origins: ["http://localhost:5173", "http://127.0.0.1:5173", "http://[::1]:5173"]
                .map(String::from)
                .to_vec(),
            session: SessionConfig::default(),
            tokens: TokenConfig::default(),
            pam: PamConfig::default(),
            oidc: OidcConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SessionConfig {
    pub idle_minutes: u64,
    pub absolute_hours: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { idle_minutes: 30, absolute_hours: 12 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TokenConfig {
    pub default_days: u64,
    pub max_days: u64,
}

impl Default for TokenConfig {
    fn default() -> Self {
        Self { default_days: 90, max_days: 365 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PamConfig {
    pub enabled: bool,
    /// Must match glidex-authd's own `allowed_groups`.
    pub allowed_groups: Vec<String>,
    /// Create users on first successful login (spec §5.3).
    pub jit: bool,
    /// Unix group → team name.
    pub group_teams: HashMap<String, String>,
    pub authd_socket: PathBuf,
    pub service: String,
}

impl Default for PamConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_groups: vec!["glidex-users".into()],
            jit: true,
            group_teams: HashMap::new(),
            authd_socket: PathBuf::from("/run/glidex-authd/auth.sock"),
            service: "glidex".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OidcConfig {
    pub enabled: bool,
    pub issuer: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub groups_claim: String,
    pub jit: bool,
    pub allowed_domains: Vec<String>,
    pub required_groups: Vec<String>,
    /// IdP group → team name.
    pub group_teams: HashMap<String, String>,
    /// Where the IdP sends the browser back; must be registered with it.
    /// Default: `<first allowed origin>/api/auth/oidc/callback`.
    pub redirect_uri: Option<String>,
}

impl Default for OidcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            issuer: String::new(),
            client_id: String::new(),
            scopes: ["openid", "profile", "email", "groups"].map(String::from).to_vec(),
            groups_claim: "groups".into(),
            jit: true,
            allowed_domains: Vec::new(),
            required_groups: Vec::new(),
            group_teams: HashMap::new(),
            redirect_uri: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthzConfig {
    /// Read-only site policies managed outside glidex (spec §7.6).
    pub policy_files_dir: PathBuf,
    /// Versions kept per site policy (1-1000).
    pub policy_history: usize,
}

impl Default for AuthzConfig {
    fn default() -> Self {
        Self { policy_files_dir: PathBuf::from("/etc/glidex/policies"), policy_history: 50 }
    }
}

/// Site-wide quota defaults (spec/security.md §6.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct QuotasConfig {
    /// Quotas for projects created without explicit ones, and for the
    /// `default` project when it is first created. Omitted limits are
    /// unlimited, except `networks`, which defaults to 2; `null` is
    /// unlimited.
    pub default: crate::tenancy::Quotas,
}

impl Default for QuotasConfig {
    fn default() -> Self {
        Self { default: crate::tenancy::Quotas::new_project() }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PciConfig {
    /// PCI devices projects may pass through without `host.devices`.
    pub allow: Vec<PciGrant>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PciGrant {
    pub bdf: String,
    /// Project ids or names.
    #[serde(default)]
    pub projects: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuditConfig {
    pub retention_days: u64,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self { retention_days: 90 }
    }
}

/// How VM instances are run and reconciled (spec/reconciliation.md §14).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReconcileConfig {
    pub vm_runner: VmRunnerKind,
    pub workers: usize,
    pub resync_secs: u64,
    pub on_host_boot: crate::models::HostBootPolicy,
    pub host_shutdown_grace_secs: u64,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        Self {
            vm_runner: VmRunnerKind::Auto,
            workers: 4,
            resync_secs: 30,
            on_host_boot: crate::models::HostBootPolicy::Resume,
            host_shutdown_grace_secs: 60,
        }
    }
}

/// Where `glidex-vm-shim` runs (spec §8.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VmRunnerKind {
    /// `systemd` when the control plane itself runs under systemd.
    Auto,
    Systemd,
    Detached,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConsoleConfig {
    /// Rotate a VM's console log past this size (one old generation kept).
    pub log_max_bytes: u64,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self { log_max_bytes: 16 << 20 }
    }
}

impl Config {
    /// Load `GLIDEX_CONFIG` or the default path; a missing file is the
    /// defaults. `GLIDEX_LISTEN` (comma-separated) overrides `listen`.
    pub fn load() -> Result<Self, String> {
        let path = std::env::var_os("GLIDEX_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        let mut cfg = Self::load_from(&path)?;
        if let Ok(list) = std::env::var("GLIDEX_LISTEN") {
            cfg.listen = list
                .split(',')
                .map(|a| a.trim().parse::<SocketAddr>().map_err(|e| format!("GLIDEX_LISTEN {}: {}", a, e)))
                .collect::<Result<_, _>>()?;
        }
        Ok(cfg)
    }

    pub fn load_from(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {}", path.display(), e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {}", path.display(), e)),
        }
    }

    /// Refuse values that parse but make no sense.
    pub fn check(&self) -> Result<(), String> {
        if !(1..=1000).contains(&self.authz.policy_history) {
            return Err("authz.policy_history must be 1-1000".into());
        }
        let r = &self.reconcile;
        if !(1..=64).contains(&r.workers) {
            return Err("reconcile.workers must be 1-64".into());
        }
        if !(5..=3600).contains(&r.resync_secs) {
            return Err("reconcile.resync_secs must be 5-3600".into());
        }
        if r.host_shutdown_grace_secs > 300 {
            return Err("reconcile.host_shutdown_grace_secs must be 0-300".into());
        }
        if !((1 << 20)..=(256 << 20)).contains(&self.console.log_max_bytes) {
            return Err("console.log_max_bytes must be 1 MiB-256 MiB".into());
        }
        self.check_listeners()
    }

    /// Refuse a non-loopback listener without TLS (spec §5.1).
    pub fn check_listeners(&self) -> Result<(), String> {
        if self.tls.is_none() {
            if let Some(a) = self.listen.iter().find(|a| !a.ip().is_loopback()) {
                return Err(format!(
                    "refusing to listen on {} without TLS; set \"tls\" in {} or listen on loopback",
                    a, DEFAULT_CONFIG_PATH
                ));
            }
        }
        Ok(())
    }
}

impl Config {
    pub fn api_socket_path(&self) -> PathBuf {
        self.api_socket.clone().unwrap_or_else(|| crate::paths::run_dir().join("api.sock"))
    }

    pub fn ui_socket_path(&self) -> PathBuf {
        self.ui_socket.clone().unwrap_or_else(|| crate::paths::run_dir().join("ui.sock"))
    }
}

/// A systemd credential (`LoadCredential=`), if present.
pub fn credential(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("CREDENTIALS_DIRECTORY")?;
    let p = PathBuf::from(dir).join(name);
    p.exists().then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_unknown_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = Config::load_from(&dir.path().join("none.json")).unwrap();
        assert_eq!(missing.listen.len(), 2);
        assert!(missing.check_listeners().is_ok());
        let p = dir.path().join("c.json");
        std::fs::write(&p, r#"{"auth": {"pam": {"jit": false}}}"#).unwrap();
        let c = Config::load_from(&p).unwrap();
        assert!(!c.auth.pam.jit);
        assert_eq!(c.auth.session.idle_minutes, 30);
        std::fs::write(&p, r#"{"auth": {"pam": {"jitt": false}}}"#).unwrap();
        assert!(Config::load_from(&p).is_err());
    }

    fn parse(json: &str) -> Result<Config, String> {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("c.json");
        std::fs::write(&p, json).unwrap();
        Config::load_from(&p).and_then(|c| c.check().map(|_| c))
    }

    #[test]
    fn quota_defaults() {
        use crate::tenancy::Quotas;
        // Nothing configured: 2 project networks, everything else unlimited.
        let c = parse("{}").unwrap();
        assert_eq!(c.quotas.default, Quotas { networks: Some(2), ..Default::default() });
        // Partial: omitted networks keeps 2.
        let c = parse(r#"{"quotas": {"default": {"vms": 10, "disk_gib": 500}}}"#).unwrap();
        assert_eq!(c.quotas.default, Quotas { vms: Some(10), disk_gib: Some(500), networks: Some(2), ..Default::default() });
        // null is unlimited.
        let c = parse(r#"{"quotas": {"default": {"networks": null}}}"#).unwrap();
        assert_eq!(c.quotas.default.networks, None);
        // Typos are refused, not ignored.
        assert!(parse(r#"{"quotas": {"default": {"network": 4}}}"#).is_err());
        assert!(parse(r#"{"quotas": {"defaults": {}}}"#).is_err());
        assert!(parse(r#"{"quotas": {"default": {"vms": -1}}}"#).is_err());
    }

    #[test]
    fn policy_history_bounds() {
        assert_eq!(parse("{}").unwrap().authz.policy_history, 50);
        assert_eq!(parse(r#"{"authz": {"policy_history": 10}}"#).unwrap().authz.policy_history, 10);
        assert!(parse(r#"{"authz": {"policy_history": 0}}"#).is_err());
    }

    /// The example in spec/security.md §13 must be a loadable config, so
    /// the spec can't document keys the parser refuses.
    #[test]
    fn spec_example_parses() {
        let spec = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/security.md")).unwrap();
        let start = spec.find("`/etc/glidex/control-plane.json` (`root:glidex 0640`").expect("§13 config heading");
        let block = &spec[start..];
        let a = block.find("```json\n").unwrap() + "```json\n".len();
        let b = a + block[a..].find("```").unwrap();
        let c = parse(&block[a..b]).unwrap();
        assert_eq!(c.quotas.default.networks, Some(2));
        assert_eq!(c.authz.policy_history, 50);
        assert!(c.tls.is_some());
    }

    /// The installer ships packaging/control-plane.json.example as
    /// /etc/glidex/control-plane.json.example; it must stay loadable and
    /// match the defaults.
    #[test]
    fn packaged_example_parses_and_matches_defaults() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/control-plane.json.example");
        let c = Config::load_from(&p).unwrap();
        let d = Config::default();
        assert_eq!(c.listen, d.listen);
        assert_eq!((&c.ui_user, &c.users_group, &c.admin_group), (&d.ui_user, &d.users_group, &d.admin_group));
        assert_eq!(c.auth.allowed_origins, d.auth.allowed_origins);
        assert_eq!(c.auth.pam.allowed_groups, d.auth.pam.allowed_groups);
        assert_eq!(c.auth.pam.authd_socket, d.auth.pam.authd_socket);
        assert_eq!(c.auth.oidc.enabled, d.auth.oidc.enabled);
        assert_eq!(c.authz.policy_files_dir, d.authz.policy_files_dir);
        assert_eq!(c.audit.retention_days, d.audit.retention_days);
        assert_eq!(c.authz.policy_history, d.authz.policy_history);
        assert_eq!(c.quotas.default, d.quotas.default);
        assert!(c.check().is_ok());
    }

    #[test]
    fn non_loopback_needs_tls() {
        let mut c = Config { listen: vec!["0.0.0.0:8841".parse().unwrap()], ..Default::default() };
        assert!(c.check_listeners().is_err());
        c.tls = Some(TlsConfig { cert: "/x".into(), key: None });
        assert!(c.check_listeners().is_ok());
    }
}
