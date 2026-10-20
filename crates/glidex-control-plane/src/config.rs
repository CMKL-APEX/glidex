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
/// The control plane's TCP port.
pub const DEFAULT_PORT: u16 = 8841;
/// The web UI's port, for the default allowed origins.
pub const UI_PORT: u16 = 5173;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// TCP listeners. Default: every address, port 8841.
    pub listen: Vec<SocketAddr>,
    /// HTTPS on the TCP listeners (spec §5.1). Default: a self-signed
    /// certificate.
    pub tls: TlsSetting,
    /// Display name `GET /auth/server-info` reports and gxctl shows in
    /// profile banners (spec/gxctl-auth.md §7.1). Default: the host name.
    pub server_name: Option<String>,
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
    pub metering: MeteringConfig,
    /// Clustering tunables (spec/clustering.md §10.2).
    pub cluster: crate::cluster::config::ClusterConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: glidex_tls::all_addresses(DEFAULT_PORT),
            tls: TlsSetting::Mode(TlsMode::Auto),
            server_name: None,
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
            metering: MeteringConfig::default(),
            cluster: Default::default(),
        }
    }
}

/// `"auto"`, `"off"` or `{"cert": …, "key": …}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum TlsSetting {
    Mode(TlsMode),
    Files(TlsConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// A self-signed certificate, generated on first start (spec §5.1.1).
    Auto,
    /// Plain HTTP: only when every listener is loopback.
    Off,
}

impl TlsSetting {
    /// Whether the TCP listeners serve HTTPS.
    pub fn enabled(&self) -> bool {
        *self != TlsSetting::Mode(TlsMode::Off)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub cert: PathBuf,
    /// Default: the systemd credential `tls-key`.
    #[serde(default)]
    pub key: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    /// Origins allowed on state-changing requests and WebSocket upgrades.
    /// Empty (the default): `https://<h>:5173` for every name and address
    /// of this host, filled in by `Config::load` (spec §5.6).
    pub allowed_origins: Vec<String>,
    /// `allowed_origins` were computed from this host's names: also allow
    /// `https://<ip>:5173` for an address the host has now (spec §5.6).
    #[serde(skip)]
    pub origins_from_host: bool,
    pub session: SessionConfig,
    pub tokens: TokenConfig,
    pub pam: PamConfig,
    pub oidc: OidcConfig,
    /// File holding the host's secrets key (spec/tenancy.md §13); the
    /// systemd credential `secrets-kek` wins when present. The control
    /// plane defaults it to `secrets.kek` beside its database.
    pub secrets_key: Option<PathBuf>,
    pub federation: FederationConfig,
}

/// Workload federations (spec/tenancy.md §9).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FederationConfig {
    /// Let a federation's issuer be on this host or the site's own
    /// network: plain-HTTP loopback issuers, and issuers or JWKS whose
    /// addresses are loopback, private, link-local or unique-local. Off:
    /// a tenant administrator can't make the control plane fetch from
    /// addresses that aren't public.
    pub allow_private_issuers: bool,
}

impl AuthConfig {
    /// Whether a browser `Origin` may use the API.
    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|a| a == origin)
            || (self.origins_from_host && ui_origin_ip(origin).is_some_and(glidex_tls::is_local_ip))
    }
}

/// The IP address of an `https://<ip>:5173` origin.
fn ui_origin_ip(origin: &str) -> Option<std::net::IpAddr> {
    let host = origin
        .strip_prefix("https://")?
        .strip_suffix(&format!(":{}", UI_PORT))?;
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SessionConfig {
    pub idle_minutes: u64,
    pub absolute_hours: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            idle_minutes: 30,
            absolute_hours: 12,
        }
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
        Self {
            default_days: 90,
            max_days: 365,
        }
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
            scopes: ["openid", "profile", "email", "groups"]
                .map(String::from)
                .to_vec(),
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
        Self {
            policy_files_dir: PathBuf::from("/etc/glidex/policies"),
            policy_history: 50,
        }
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
        Self {
            default: crate::tenancy::Quotas::new_project(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PciConfig {
    /// PCI devices and pools projects may pass through without
    /// `host.devices`.
    pub allow: Vec<PciGrant>,
    /// Named sets of devices a VM asks for by count
    /// (spec/host-topology.md §9.1).
    pub pools: Vec<PciPool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PciGrant {
    /// The device's address; or `pool`, a pool's name.
    #[serde(default)]
    pub bdf: Option<String>,
    #[serde(default)]
    pub pool: Option<String>,
    /// Project ids or names.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Tenant ids or slugs: every project of theirs may use the device
    /// (spec/tenancy.md D25).
    #[serde(default)]
    pub tenants: Vec<String>,
    /// The node (id or name) whose device this is; absent: the device at
    /// that address on every node (spec/host-topology.md §8.5).
    #[serde(default)]
    pub node: Option<String>,
}

/// A pool of devices (spec/host-topology.md §9.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PciPool {
    pub name: String,
    /// `gpu`, `nic`, `nvme`, `dpu`, `accelerator`: a label for people and
    /// metering.
    #[serde(default)]
    pub class: Option<String>,
    #[serde(rename = "match")]
    pub matches: PoolMatch,
    /// Node ids or names the pool is on; empty: every node.
    #[serde(default)]
    pub nodes: Vec<String>,
    /// Each node binds its devices of the pool to `vfio-pci` at start
    /// (spec/host-topology.md §5.3).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prepare: bool,
}

/// Which devices are a pool's: every key given must match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PoolMatch {
    #[serde(default)]
    pub vendor_id: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    /// A PCI class code or its start (`0x0302`).
    #[serde(default)]
    pub class_code: Option<String>,
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Virtual functions of the physical functions this matches
    /// (spec/host-topology.md §10.1); the other keys then match the VF.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vf_of: Option<Box<PoolMatch>>,
}

fn hex_id(s: &str) -> String {
    s.trim().trim_start_matches("0x").to_ascii_lowercase()
}

impl PoolMatch {
    pub fn is_empty(&self) -> bool {
        self.vendor_id.is_none()
            && self.device_id.is_none()
            && self.class_code.is_none()
            && self.addresses.is_empty()
            && self.vf_of.is_none()
    }

    /// Whether a device of a node's summary is the pool's: its own ids,
    /// and with `vf_of`, a VF of a matching PF.
    pub fn matches_device(&self, d: &crate::topology::SummaryDevice) -> bool {
        self.matches(&d.address, &d.vendor_id, &d.device_id, &d.class_id)
            && self.vf_of.as_deref().is_none_or(|pf| {
                d.physfn.as_ref().is_some_and(|p| {
                    !pf.is_empty()
                        && pf.matches(&p.address, &p.vendor_id, &p.device_id, &p.class_id)
                })
            })
    }

    /// Whether a device with these ids at `address` is the pool's.
    pub fn matches(&self, address: &str, vendor_id: &str, device_id: &str, class_id: &str) -> bool {
        self.vendor_id
            .as_deref()
            .is_none_or(|v| hex_id(v) == hex_id(vendor_id))
            && self
                .device_id
                .as_deref()
                .is_none_or(|d| hex_id(d) == hex_id(device_id))
            && self
                .class_code
                .as_deref()
                .is_none_or(|c| hex_id(class_id).starts_with(&hex_id(c)))
            && (self.addresses.is_empty()
                || self
                    .addresses
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(address)))
    }
}

impl PciConfig {
    /// Refuse what can't work: unnamed, duplicate or matchless pools, two
    /// pools matching the same way, and grants naming neither one device
    /// nor one known pool.
    pub fn check(&self) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for p in &self.pools {
            if p.name.is_empty() || !seen.insert(p.name.as_str()) {
                return Err(format!(
                    "pci.pools: pool names must be set and unique ({:?})",
                    p.name
                ));
            }
            if p.matches.is_empty() {
                return Err(format!(
                    "pci.pools: pool {} matches every device; give it a match",
                    p.name
                ));
            }
            if p.matches.vf_of.as_deref().is_some_and(|pf| pf.is_empty()) {
                return Err(format!(
                    "pci.pools: pool {}: vf_of matches every PF; give it ids or addresses",
                    p.name
                ));
            }
            if let Some(o) = self.pools.iter().find(|o| {
                o.name != p.name
                    && o.matches == p.matches
                    && (o.nodes.is_empty()
                        || p.nodes.is_empty()
                        || o.nodes.iter().any(|n| p.nodes.contains(n)))
            }) {
                return Err(format!(
                    "pci.pools: pools {} and {} match the same devices",
                    o.name, p.name
                ));
            }
        }
        for g in &self.allow {
            match (&g.bdf, &g.pool) {
                (Some(_), None) => {}
                (None, Some(p)) if seen.contains(p.as_str()) => {}
                (None, Some(p)) => return Err(format!("pci.allow: no pool {p}")),
                _ => return Err("pci.allow: each grant names one bdf or one pool".into()),
            }
        }
        Ok(())
    }
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
        Self {
            log_max_bytes: 16 << 20,
        }
    }
}

/// Resource usage metering (spec/metering.md §11).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MeteringConfig {
    /// Sample at all. Off keeps the cursors: turning it back on resumes
    /// without double counting (the gap is spread, §6.4).
    pub enabled: bool,
    /// Seconds between rounds; divides 300 so samples line up with the
    /// 5-minute slots (§8.5.1).
    pub sample_secs: u64,
    /// Seconds between `qemu-img info` passes for `disk.stored` (§5.3).
    pub storage_secs: u64,
    /// An hour is closed this long after it ends (§6.5).
    pub close_grace_secs: u64,
    pub retention_days: u64,
    pub retention_daily_days: u64,
    pub retention_rate_days: u64,
    /// Billing months are calendar months here: `UTC`, `±HH:00`, or an
    /// IANA zone with a fixed whole-hour offset (§8.4).
    pub billing_timezone: String,
}

impl Default for MeteringConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_secs: 30,
            storage_secs: 900,
            close_grace_secs: 120,
            retention_days: 90,
            retention_daily_days: 1825,
            retention_rate_days: 100,
            billing_timezone: "UTC".into(),
        }
    }
}

impl MeteringConfig {
    pub fn check(&self) -> Result<(), String> {
        if !(10..=300).contains(&self.sample_secs) || 300 % self.sample_secs != 0 {
            return Err("metering.sample_secs must be 10-300 and divide 300 (10, 12, 15, 20, 25, 30, 50, 60, 75, 100, 150, 300)".into());
        }
        if !(self.sample_secs..=86400).contains(&self.storage_secs) {
            return Err("metering.storage_secs must be sample_secs-86400".into());
        }
        if self.close_grace_secs > 3600 || self.close_grace_secs < self.sample_secs {
            return Err("metering.close_grace_secs must be sample_secs-3600".into());
        }
        if !(1..=3650).contains(&self.retention_days) {
            return Err("metering.retention_days must be 1-3650".into());
        }
        if !(1..=3650).contains(&self.retention_daily_days) {
            return Err("metering.retention_daily_days must be 1-3650".into());
        }
        if !(35..=400).contains(&self.retention_rate_days) {
            return Err("metering.retention_rate_days must be 35-400".into());
        }
        crate::metering::query::parse_tz(&self.billing_timezone)
            .map_err(|e| format!("metering.billing_timezone: {e}"))?;
        Ok(())
    }
}

impl Config {
    /// Load `GLIDEX_CONFIG` or the default path; a missing file is the
    /// defaults. `GLIDEX_LISTEN` (comma-separated) overrides `listen`.
    /// Empty `auth.allowed_origins` become this host's UI origins.
    pub fn load() -> Result<Self, String> {
        let path = std::env::var_os("GLIDEX_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH));
        let mut cfg = Self::load_from(&path)?;
        if let Ok(list) = std::env::var("GLIDEX_LISTEN") {
            cfg.listen = glidex_tls::parse_addresses(&list, "GLIDEX_LISTEN")?;
        }
        if cfg.auth.allowed_origins.is_empty() {
            cfg.auth.allowed_origins = glidex_tls::LocalNames::discover().origins(UI_PORT);
            cfg.auth.origins_from_host = true;
        }
        Ok(cfg)
    }

    pub fn load_from(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| format!("{}: {}", path.display(), e))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {}", path.display(), e)),
        }
    }

    /// Refuse values that parse but make no sense.
    pub fn check(&self) -> Result<(), String> {
        self.pci.check()?;
        if !self.authz.policy_files_dir.as_os_str().is_empty() {
            crate::paths::checked_path(&self.authz.policy_files_dir)
                .map_err(|e| format!("authz.policy_files_dir {e}"))?;
        }
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
        self.metering.check()?;
        self.cluster.check()?;
        self.check_listeners()
    }

    /// Refuse plain HTTP on a non-loopback listener (spec §5.1).
    pub fn check_listeners(&self) -> Result<(), String> {
        if !self.tls.enabled()
            && let Some(a) = self.listen.iter().find(|a| !a.ip().is_loopback())
        {
            return Err(format!(
                "refusing to serve plain HTTP on {}: \"tls\": \"off\" needs every listener on loopback ({})",
                a, DEFAULT_CONFIG_PATH
            ));
        }
        Ok(())
    }
}

impl Config {
    pub fn api_socket_path(&self) -> PathBuf {
        self.api_socket
            .clone()
            .unwrap_or_else(|| crate::paths::run_dir().join("api.sock"))
    }

    pub fn ui_socket_path(&self) -> PathBuf {
        self.ui_socket
            .clone()
            .unwrap_or_else(|| crate::paths::run_dir().join("ui.sock"))
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

    #[test]
    fn pools_are_checked_and_match_by_ids_class_and_address() {
        let pool = |name: &str, vendor: Option<&str>| PciPool {
            name: name.into(),
            class: None,
            matches: PoolMatch {
                vendor_id: vendor.map(Into::into),
                device_id: None,
                class_code: Some("0x0302".into()),
                addresses: vec![],
                vf_of: None,
            },
            nodes: vec![],
            prepare: false,
        };
        let mut c = PciConfig {
            pools: vec![pool("a100", Some("0x10de"))],
            allow: vec![],
        };
        assert!(c.check().is_ok());
        let m = &c.pools[0].matches;
        assert!(m.matches("0000:87:00.0", "0x10de", "0x20b0", "0x030200"));
        assert!(!m.matches("0000:87:00.0", "0x15b3", "0x20b0", "0x030200"));
        assert!(!m.matches("0000:87:00.0", "0x10de", "0x20b0", "0x020000"));
        c.pools.push(pool("again", Some("10DE")));
        assert!(
            c.check().is_ok(),
            "a different spelling is a different match"
        );
        c.pools.push(pool("a100", None));
        assert!(c.check().unwrap_err().contains("unique"));
        c.pools.pop();
        c.pools.push(PciPool {
            matches: PoolMatch::default(),
            ..pool("all", None)
        });
        assert!(c.check().unwrap_err().contains("matches every device"));
        c.pools.pop();
        c.pools.push(pool("same", Some("0x10de")));
        assert!(c.check().unwrap_err().contains("match the same devices"));
        c.pools.pop();
        let grant = |bdf: Option<&str>, pool: Option<&str>| PciGrant {
            bdf: bdf.map(Into::into),
            pool: pool.map(Into::into),
            projects: vec!["p".into()],
            tenants: vec![],
            node: None,
        };
        c.allow = vec![grant(None, Some("a100")), grant(Some("0000:41:00.0"), None)];
        assert!(c.check().is_ok());
        c.allow.push(grant(None, Some("nope")));
        assert!(c.check().unwrap_err().contains("no pool nope"));
        c.allow.pop();
        c.allow.push(grant(Some("0000:41:00.0"), Some("a100")));
        assert!(c.check().unwrap_err().contains("one bdf or one pool"));
    }

    use super::*;

    #[test]
    fn metering_ranges() {
        let ok = MeteringConfig::default();
        assert!(ok.check().is_ok());
        for bad in [
            MeteringConfig {
                sample_secs: 40,
                ..ok.clone()
            }, // does not divide 300
            MeteringConfig {
                sample_secs: 5,
                ..ok.clone()
            },
            MeteringConfig {
                close_grace_secs: 10,
                ..ok.clone()
            },
            MeteringConfig {
                storage_secs: 20,
                ..ok.clone()
            },
            MeteringConfig {
                retention_rate_days: 10,
                ..ok.clone()
            },
            MeteringConfig {
                billing_timezone: "+05:30".into(),
                ..ok.clone()
            },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        assert!(
            MeteringConfig {
                sample_secs: 60,
                close_grace_secs: 60,
                ..ok
            }
            .check()
            .is_ok()
        );
    }

    #[test]
    fn defaults_and_unknown_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = Config::load_from(&dir.path().join("none.json")).unwrap();
        assert_eq!(missing.listen, glidex_tls::all_addresses(8841));
        assert_eq!(missing.tls, TlsSetting::Mode(TlsMode::Auto));
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
        // Nothing configured: 2 project networks, 1 router with 1 external address, everything else unlimited.
        let c = parse("{}").unwrap();
        assert_eq!(
            c.quotas.default,
            Quotas {
                networks: Some(2),
                routers: Some(1),
                external_ips: Some(1),
                ..Default::default()
            }
        );
        // Partial: omitted networks keeps 2.
        let c = parse(r#"{"quotas": {"default": {"vms": 10, "disk_gib": 500}}}"#).unwrap();
        assert_eq!(
            c.quotas.default,
            Quotas {
                vms: Some(10),
                disk_gib: Some(500),
                networks: Some(2),
                routers: Some(1),
                external_ips: Some(1),
                ..Default::default()
            }
        );
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
        assert_eq!(
            parse(r#"{"authz": {"policy_history": 10}}"#)
                .unwrap()
                .authz
                .policy_history,
            10
        );
        assert!(parse(r#"{"authz": {"policy_history": 0}}"#).is_err());
    }

    /// The example in spec/security.md §13 must be a loadable config, so
    /// the spec can't document keys the parser refuses.
    #[test]
    fn spec_example_parses() {
        let spec = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/security.md"),
        )
        .unwrap();
        let start = spec
            .find("`/etc/glidex/control-plane.json` (`root:glidex 0640`")
            .expect("§13 config heading");
        let block = &spec[start..];
        let a = block.find("```json\n").unwrap() + "```json\n".len();
        let b = a + block[a..].find("```").unwrap();
        let c = parse(&block[a..b]).unwrap();
        assert_eq!(c.quotas.default.networks, Some(2));
        assert_eq!(c.authz.policy_history, 50);
        assert_eq!(c.tls, TlsSetting::Mode(TlsMode::Auto));
    }

    /// The installer ships packaging/control-plane.json.example as
    /// /etc/glidex/control-plane.json.example; it must stay loadable and
    /// match the defaults.
    #[test]
    fn packaged_example_parses_and_matches_defaults() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packaging/control-plane.json.example");
        let c = Config::load_from(&p).unwrap();
        let d = Config::default();
        assert_eq!(c.listen, d.listen);
        assert_eq!(
            (&c.ui_user, &c.users_group, &c.admin_group),
            (&d.ui_user, &d.users_group, &d.admin_group)
        );
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
    fn plain_http_only_on_loopback() {
        let mut c = Config {
            listen: vec!["0.0.0.0:8841".parse().unwrap()],
            ..Default::default()
        };
        assert!(c.check_listeners().is_ok());
        c.tls = TlsSetting::Mode(TlsMode::Off);
        assert!(c.check_listeners().is_err());
        c.listen = vec![
            "127.0.0.1:8841".parse().unwrap(),
            "[::1]:8841".parse().unwrap(),
        ];
        assert!(c.check_listeners().is_ok());
    }

    #[test]
    fn origins_from_this_host() {
        let mut a = AuthConfig {
            allowed_origins: vec!["https://glidex.example.org:5173".into()],
            ..Default::default()
        };
        assert!(a.origin_allowed("https://glidex.example.org:5173"));
        assert!(!a.origin_allowed("https://127.0.0.1:5173"));
        a.origins_from_host = true;
        assert!(a.origin_allowed("https://127.0.0.1:5173"));
        assert!(a.origin_allowed("https://[::1]:5173"));
        assert!(!a.origin_allowed("http://127.0.0.1:5173"));
        assert!(!a.origin_allowed("https://127.0.0.1:5174"));
        assert!(!a.origin_allowed("https://192.0.2.1:5173"));
        assert!(!a.origin_allowed("https://evil.example:5173"));
        // A config file can't switch it on.
        assert!(parse(r#"{"auth": {"origins_from_host": true}}"#).is_err());
    }

    #[test]
    fn tls_settings_parse() {
        assert_eq!(
            parse(r#"{"tls": "auto"}"#).unwrap().tls,
            TlsSetting::Mode(TlsMode::Auto)
        );
        assert!(parse(r#"{"tls": "off"}"#).is_err());
        assert_eq!(
            parse(r#"{"tls": "off", "listen": ["127.0.0.1:1"]}"#)
                .unwrap()
                .tls,
            TlsSetting::Mode(TlsMode::Off)
        );
        assert_eq!(
            parse(r#"{"tls": {"cert": "/c"}}"#).unwrap().tls,
            TlsSetting::Files(TlsConfig {
                cert: "/c".into(),
                key: None
            })
        );
        assert!(parse(r#"{"tls": "on"}"#).is_err());
        assert!(parse(r#"{"tls": {"crt": "/c"}}"#).is_err());
    }
}
