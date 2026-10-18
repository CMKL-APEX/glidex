//! Authentication and the bridge to Cedar authorization
//! (spec/security.md §5, §7).
//!
//! Every request gets a [`Principal`]: a peer-identified local user on
//! `api.sock`, a browser session, an access token, or — only for embedding
//! and tests ([`AuthService::disabled`]) — a synthetic break-glass user.
//! [`AuthService::authorize`] adds the principal's entities to a query and
//! asks the policy engine; a personal token that has links of its own is
//! allowed only what both it and its owner are allowed.

pub mod store;

use crate::authz::{
    self, AuthContext, Decision, Engine, Ent, EntitySet, Link, PolicySource, Query, SiteSource,
};
use crate::config::Config;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use store::{AuditEntry, IdentityStore, LinkRecord, Session, StoreError, Token, TokenKind, User};
use thiserror::Error;

/// Cookie carrying the session id.
pub const SESSION_COOKIE: &str = "gx_session";
/// Header carrying the session's CSRF value on cookie-authenticated writes.
pub const CSRF_HEADER: &str = "x-glidex-csrf";
pub const TOKEN_PREFIX: &str = "gxt_";
/// Clean a client-claimed `device` / `client` string for storage: one
/// trimmed line, ≤ 128 characters, no control characters; `None` on
/// anything else. Display and audit only — never an authorization input
/// (spec/gxctl-auth.md §7.2).
fn stamp(s: Option<&str>) -> Option<String> {
    let s = s
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().any(char::is_control) && s.len() <= 128)?;
    Some(s.to_string())
}
/// How long a console ticket stays valid (spec §5.6).
pub const TICKET_SECS: u64 = 30;
/// `age_secs` for principals that can never satisfy step-up.
const NEVER_FRESH: i64 = i64::MAX / 4;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("authentication required")]
    Unauthenticated,
    #[error("authentication failed")]
    Denied,
    #[error("too many attempts; try again later")]
    RateLimited,
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Invalid(String),
    /// A verified identity the connector's rules don't let in (403).
    #[error("login not allowed: {0}")]
    NotAllowed(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Authz(#[from] authz::AuthzError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Peer,
    Pam,
    Oidc,
    Token,
    Disabled,
}

impl Method {
    pub fn as_str(&self) -> &'static str {
        match self {
            Method::Peer => "peer",
            Method::Pam => "pam",
            Method::Oidc => "oidc",
            Method::Token => "token",
            Method::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Transport {
    Unix,
    Tcp,
}

/// Who is making a request, and how they proved it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Principal {
    /// The user; `None` for service-account tokens.
    pub user: Option<User>,
    pub token: Option<Token>,
    /// The personal token has links of its own (narrowing it).
    pub token_narrowed: bool,
    /// Team ids: stored teams, plus `unix:<group>` for peer users.
    pub teams: Vec<String>,
    pub method: Method,
    pub transport: Transport,
    /// Unix seconds; for step-up.
    pub authenticated_at: u64,
    pub source_ip: Option<IpAddr>,
    /// SHA-256 of the session id, for logout.
    pub session_hash: Option<String>,
    pub csrf: Option<String>,
    /// Peer uid on api.sock.
    pub peer_uid: Option<u32>,
    /// A multi-factor browser login. Tokens, peer and PAM logins never are.
    pub mfa: bool,
}

impl Principal {
    /// The directory the principal belongs to (spec/tenancy.md §6). A host
    /// hosts one directory until directories and clusters come apart (T6).
    pub fn directory_id(&self) -> &str {
        crate::tenants::LOCAL_DIRECTORY
    }

    /// The synthetic principal of [`AuthService::disabled`].
    pub fn system() -> Self {
        Principal {
            user: Some(User {
                id: "system".into(),
                display_name: "system".into(),
                disabled: false,
                default_project: None,
                created_at: 0,
            }),
            token: None,
            token_narrowed: false,
            teams: vec![authz::BREAK_GLASS_TEAM.into()],
            method: Method::Disabled,
            transport: Transport::Unix,
            authenticated_at: store::now(),
            source_ip: None,
            session_hash: None,
            csrf: None,
            peer_uid: None,
            mfa: false,
        }
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user.as_ref().map(|u| u.id.as_str())
    }

    pub fn is_break_glass(&self) -> bool {
        self.token.is_none() && self.teams.iter().any(|t| t == authz::BREAK_GLASS_TEAM)
    }

    /// The Cedar principal: the token when there is one, else the user.
    pub fn cedar(&self) -> Ent {
        match (&self.token, &self.user) {
            (Some(t), _) => Ent::Token(t.id.clone()),
            (None, Some(u)) => Ent::User(u.id.clone()),
            (None, None) => Ent::User(String::new()),
        }
    }

    pub fn display(&self) -> String {
        match (&self.user, &self.token) {
            (Some(u), Some(t)) => format!("{} (token {})", u.display_name, t.name),
            (Some(u), None) => u.display_name.clone(),
            (None, Some(t)) => format!("service account {}", t.name),
            (None, None) => "anonymous".into(),
        }
    }

    /// Who, for the audit log (spec §10): never a secret.
    pub fn audit_json(&self) -> serde_json::Value {
        serde_json::json!({
            "user": self.user.as_ref().map(|u| &u.id),
            "name": self.user.as_ref().map(|u| &u.display_name),
            "token": self.token.as_ref().map(|t| &t.id),
            "method": self.method.as_str(),
            "session": self.session_hash.as_ref().map(|h| &h[..12]),
        })
    }

    pub fn source(&self) -> String {
        match (self.peer_uid, self.source_ip) {
            (Some(uid), _) => format!("uid:{}", uid),
            (None, Some(ip)) => ip.to_string(),
            (None, None) => "local".into(),
        }
    }

    pub fn default_project(&self) -> Option<&str> {
        self.user
            .as_ref()
            .and_then(|u| u.default_project.as_deref())
            .or(match &self.token {
                Some(Token {
                    kind: TokenKind::ServiceAccount { project },
                    ..
                }) => Some(project.as_str()),
                _ => None,
            })
    }

    fn auth_context(&self, as_owner: bool) -> AuthContext {
        let age = match (self.method, &self.token) {
            (Method::Peer | Method::Disabled, _) => 0,
            // A token counts as fresh only through its own links; acting
            // purely as its owner it can't reach step-up actions.
            (Method::Token, Some(_)) if as_owner && !self.token_narrowed => NEVER_FRESH,
            (Method::Token, _) => 0,
            _ => (store::now().saturating_sub(self.authenticated_at)) as i64,
        };
        AuthContext {
            method: self.method.as_str().into(),
            transport: match self.transport {
                Transport::Unix => "unix".into(),
                Transport::Tcp => "tcp".into(),
            },
            age_secs: age,
            mfa: self.mfa,
            source_ip: self.source_ip,
        }
    }
}

/// A short-lived, single-use console ticket (spec §5.6).
struct Ticket {
    principal: Ent,
    vm_id: String,
    expires: u64,
}

pub struct AuthService {
    pub store: IdentityStore,
    /// Directory, tenants and admissions (spec/tenancy.md).
    pub tenants: crate::tenants::TenantStore,
    pub engine: Engine,
    pub config: Config,
    /// No authentication: every request is [`Principal::system`].
    disabled: bool,
    tickets: Mutex<HashMap<String, Ticket>>,
    /// Directory connectors and their secrets (spec/tenancy.md §5).
    pub connectors: crate::directory::ConnectorStore,
    /// Invitations to the directory's tenants (spec/tenancy.md §6.1).
    pub invitations: crate::directory::invitations::InvitationStore,
    /// One OIDC client per connector, rebuilt when its record changes.
    oidc: Mutex<
        HashMap<
            String,
            (
                crate::directory::Connector,
                Arc<crate::directory::oidc::OidcState>,
            ),
        >,
    >,
    audit_count: std::sync::atomic::AtomicU64,
}

impl crate::cluster::runtime::SecretsKey for AuthService {
    fn kek(
        &self,
    ) -> (
        zeroize::Zeroizing<Vec<u8>>,
        Option<zeroize::Zeroizing<Vec<u8>>>,
    ) {
        (self.connectors.kek_bytes(), self.connectors.kek_previous())
    }

    fn install_kek(&self, bytes: &[u8], previous: Option<&[u8]>) -> Result<(), String> {
        self.connectors
            .install_kek(bytes, previous)
            .map_err(|e| e.to_string())
    }

    fn can_rotate(&self) -> Result<(), String> {
        if self.connectors.kek_is_credential() {
            return Err("the secrets key is the secrets-kek credential: rotate it by changing the credential on every server".into());
        }
        Ok(())
    }

    fn rewrap(&self) -> Result<usize, String> {
        self.connectors
            .rewrap_data_keys()
            .map_err(|e| e.to_string())
    }

    fn kek_id(&self) -> String {
        self.connectors.kek_id()
    }
}

pub fn connector_err(e: crate::directory::ConnectorError) -> AuthError {
    use crate::directory::ConnectorError as C;
    match e {
        C::NotFound(m) => AuthError::Store(StoreError::NotFound(m)),
        C::AlreadyExists(m) => AuthError::Store(StoreError::AlreadyExists(m)),
        C::Invalid(m) => AuthError::Invalid(m),
        C::FileDefined(_) => AuthError::Invalid(e.to_string()),
        C::Secret(_) | C::Storage(_) => AuthError::Unavailable(e.to_string()),
    }
}

pub fn tenant_err(e: crate::tenants::TenantError) -> AuthError {
    use crate::tenants::TenantError as T;
    match e {
        T::Storage(m) => AuthError::Unavailable(m),
        other => AuthError::Invalid(other.to_string()),
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    d.iter().map(|b| format!("{:02x}", b)).collect()
}

/// `n` random bytes.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system random source");
    b
}

const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Base62 of `bytes` (big-endian), for tokens.
pub fn base62(bytes: &[u8]) -> String {
    let mut digits: Vec<u8> = Vec::new();
    let mut num: Vec<u8> = bytes.to_vec();
    while num.iter().any(|&b| b != 0) {
        let mut rem: u32 = 0;
        for b in num.iter_mut() {
            let acc = (rem << 8) | *b as u32;
            *b = (acc / 62) as u8;
            rem = acc % 62;
        }
        digits.push(BASE62[rem as usize]);
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    digits.reverse();
    String::from_utf8(digits).expect("ascii")
}

/// URL-safe random string (session ids, CSRF values, tickets).
pub fn random_token(n: usize) -> String {
    base62(&random_bytes(n))
}

/// Compare two strings in time independent of where they differ.
pub fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl AuthService {
    pub fn new(db: Arc<crate::store::Db>, config: Config) -> Result<Arc<Self>, AuthError> {
        config.check().map_err(AuthError::Invalid)?;
        let svc = Arc::new(Self {
            tenants: crate::tenants::TenantStore::new(db.clone()).map_err(tenant_err)?,
            store: IdentityStore::new(db.clone())?,
            engine: Engine::new()?,
            connectors: crate::directory::ConnectorStore::new(db.clone(), &config)
                .map_err(connector_err)?,
            invitations: crate::directory::invitations::InvitationStore::new(db.clone()),
            oidc: Mutex::new(HashMap::new()),
            config,
            disabled: false,
            tickets: Mutex::new(HashMap::new()),
            audit_count: Default::default(),
        });
        svc.store
            .set_policy_history(svc.config.authz.policy_history);
        svc.reload_policies()?;
        svc.migrate_tenancy()?;
        Ok(svc)
    }

    /// Authentication off: every request is the break-glass system user.
    /// For embedding and tests only; the binary never uses it.
    pub fn disabled(db: Arc<crate::store::Db>) -> Result<Arc<Self>, AuthError> {
        Self::disabled_with(db, Config::default())
    }

    /// As [`disabled`](Self::disabled), with the rest of `config` (tests).
    pub fn disabled_with(
        db: Arc<crate::store::Db>,
        mut config: Config,
    ) -> Result<Arc<Self>, AuthError> {
        // Embedding and tests never read the host's policy files.
        config.authz.policy_files_dir = std::path::PathBuf::new();
        let svc = Arc::new(Self {
            tenants: crate::tenants::TenantStore::new(db.clone()).map_err(tenant_err)?,
            store: IdentityStore::new(db.clone())?,
            engine: Engine::new()?,
            connectors: crate::directory::ConnectorStore::new(db.clone(), &config)
                .map_err(connector_err)?,
            invitations: crate::directory::invitations::InvitationStore::new(db.clone()),
            oidc: Mutex::new(HashMap::new()),
            config,
            disabled: true,
            tickets: Mutex::new(HashMap::new()),
            audit_count: Default::default(),
        });
        svc.reload_policies()?;
        Ok(svc)
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    // ---- policies -------------------------------------------------------

    /// Site policies from the read-only directory (spec §7.6).
    fn file_policies(&self) -> Result<Vec<SiteSource>, AuthError> {
        let dir = &self.config.authz.policy_files_dir;
        let mut out = Vec::new();
        if dir.as_os_str().is_empty() {
            return Ok(out);
        }
        let dir = &crate::paths::checked_path(dir)
            .map_err(|e| AuthError::Invalid(format!("authz.policy_files_dir {e}")))?;
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(AuthError::Invalid(format!("{}: {}", dir.display(), e))),
        };
        let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for p in paths
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "cedar"))
        {
            let text = std::fs::read_to_string(&p)
                .map_err(|e| AuthError::Invalid(format!("{}: {}", p.display(), e)))?;
            // A file may hold several policies; each needs its own @id.
            let set: cedar_policy::PolicySet =
                text.parse().map_err(|e: cedar_policy::ParseErrors| {
                    AuthError::Invalid(format!("{}: {}", p.display(), e))
                })?;
            for pol in set.policies() {
                let id = pol
                    .annotation("id")
                    .ok_or_else(|| {
                        AuthError::Invalid(format!("{}: a policy has no @id", p.display()))
                    })?
                    .to_string();
                out.push(SiteSource {
                    id,
                    text: pol.to_string(),
                    source: PolicySource::File,
                });
            }
        }
        Ok(out)
    }

    /// Links, and enabled site policies (API-managed and files).
    pub fn policy_inputs(&self) -> Result<(Vec<Link>, Vec<SiteSource>), AuthError> {
        let links = self.store.links()?.into_iter().map(|l| l.link).collect();
        let mut site: Vec<SiteSource> = self
            .store
            .site_policies()?
            .into_iter()
            .filter(|p| p.enabled)
            .map(|p| SiteSource {
                id: p.id,
                text: p.text,
                source: PolicySource::Site,
            })
            .collect();
        let files = self.file_policies()?;
        if let Some(dup) = files.iter().find(|f| site.iter().any(|s| s.id == f.id)) {
            return Err(AuthError::Invalid(format!(
                "policy id {} is used by a file and the API",
                dup.id
            )));
        }
        site.extend(files);
        Ok((links, site))
    }

    /// Rebuild and publish the policy set from storage.
    /// Keep the policy set in step with links and site policies applied from
    /// the replicated log (spec/clustering.md §6.5), so a role granted
    /// through one server counts on every other.
    pub fn watch_policies(self: &Arc<Self>) {
        let me = self.clone();
        let mut rx = self.store.database().subscribe();
        tokio::spawn(async move {
            use crate::store::TableId;
            loop {
                let relevant = match rx.recv().await {
                    Ok(a) => a
                        .tables
                        .iter()
                        .any(|t| matches!(t, TableId::PolicyLinks | TableId::SitePolicies)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                };
                if relevant && let Err(e) = me.reload_policies() {
                    tracing::warn!("reloading policies: {}", e);
                }
            }
        });
    }

    pub fn reload_policies(&self) -> Result<(), AuthError> {
        let (links, site) = self.policy_inputs()?;
        self.engine.install(&links, &site)?;
        Ok(())
    }

    // ---- bootstrap (spec §11) -------------------------------------------

    /// On first start (no `role.system-admin` link yet), make every member
    /// of the admin group a system administrator and owner of the default
    /// project, so they can also manage glidex from a browser session.
    pub fn bootstrap(&self, default_project: &str) -> Result<Vec<String>, AuthError> {
        let listed = nix::unistd::Group::from_name(&self.config.admin_group)
            .ok()
            .flatten()
            .map(|g| g.mem)
            .unwrap_or_default();
        let me = nix::unistd::User::from_uid(nix::unistd::getuid())
            .ok()
            .flatten()
            .map(|u| {
                let groups = unix_groups(&u);
                (u.name, groups)
            });
        let members = bootstrap_members(&self.config.admin_group, listed, me);
        self.bootstrap_with(&members, default_project)
    }

    pub fn bootstrap_with(
        &self,
        members: &[String],
        default_project: &str,
    ) -> Result<Vec<String>, AuthError> {
        if self
            .store
            .links()?
            .iter()
            .any(|l| l.link.template == "role.system-admin")
        {
            return Ok(Vec::new());
        }
        let mut made = Vec::new();
        for name in members {
            let Some(u) = self.store.user_for_identity(
                "unix",
                &authz::local_subject(name),
                name,
                None,
                true,
            )?
            else {
                continue;
            };
            self.add_link(
                "role.system-admin",
                Ent::User(u.id.clone()),
                Ent::Cluster,
                "bootstrap",
            )?;
            self.add_link(
                "role.owner",
                Ent::User(u.id.clone()),
                Ent::Project(default_project.into()),
                "bootstrap",
            )?;
            self.make_administrator(&Ent::User(u.id.clone()), "bootstrap")?;
            made.push(name.clone());
        }
        Ok(made)
    }

    /// Give a system administrator of a host that is its own directory
    /// what it had before tenants (spec/tenancy.md §16 step 4): membership
    /// of the operator tenant, the directory, and both tenants.
    pub fn make_administrator(&self, principal: &Ent, by: &str) -> Result<(), AuthError> {
        use crate::tenants::{ADMIN_TENANT, DEFAULT_TENANT, LOCAL_DIRECTORY};
        self.tenants
            .add_member(ADMIN_TENANT, principal.clone(), by)
            .map_err(tenant_err)?;
        self.add_link(
            "role.directory-admin",
            principal.clone(),
            Ent::Directory(LOCAL_DIRECTORY.into()),
            by,
        )?;
        for t in [ADMIN_TENANT, DEFAULT_TENANT] {
            self.add_link(
                "role.tenant-admin",
                principal.clone(),
                Ent::Tenant(t.into()),
                by,
            )?;
        }
        Ok(())
    }

    /// Whether `principal` (a user, team or token) is a member of `tenant`
    /// (D3): listed, or linked on the tenant or one of its projects. A
    /// personal token counts as its owner.
    pub fn is_member(&self, principal: &Ent, tenant: &str) -> Result<bool, AuthError> {
        let principal = match principal {
            Ent::Token(id) => match self.store.token_by_id(id)?.map(|(_, t)| t.kind) {
                Some(TokenKind::Personal { owner }) => Ent::User(owner),
                Some(TokenKind::ServiceAccount { project }) => {
                    return Ok(self
                        .tenants
                        .project_tenant(&project)
                        .map_err(tenant_err)?
                        .as_deref()
                        .unwrap_or(crate::tenants::DEFAULT_TENANT)
                        == tenant);
                }
                None => return Ok(false),
            },
            other => other.clone(),
        };
        let mut mine = vec![principal.clone()];
        if let Ent::User(u) = &principal {
            mine.extend(self.store.teams_of(u)?.into_iter().map(Ent::Team));
        }
        if self
            .tenants
            .members(tenant)
            .map_err(tenant_err)?
            .iter()
            .any(|m| mine.contains(&m.principal))
        {
            return Ok(true);
        }
        for l in self
            .store
            .links()?
            .into_iter()
            .filter(|l| mine.contains(&l.link.principal))
        {
            let of = match &l.link.resource {
                Ent::Tenant(t) => Some(t.clone()),
                Ent::Project(p) => Some(
                    self.tenants
                        .project_tenant(p)
                        .map_err(tenant_err)?
                        .unwrap_or_else(|| crate::tenants::DEFAULT_TENANT.into()),
                ),
                _ => None,
            };
            if of.as_deref() == Some(tenant) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// One-time: existing host-role holders become members of the operator
    /// tenant; system administrators get [`Self::make_administrator`]'s links;
    /// auditors also audit the directory (spec/tenancy.md §16 steps 3-4).
    pub fn migrate_tenancy(&self) -> Result<(), AuthError> {
        const KEY: &str = "tenants_links_v1";
        if self.tenants.done(KEY).map_err(tenant_err)? || !self.tenants.can_write() {
            return Ok(());
        }
        let operator = self.tenants.operator().map_err(tenant_err)?;
        for l in self.store.links()? {
            if !matches!(l.link.resource, Ent::Cluster | Ent::Host | Ent::Node(_))
                || l.link.template == "grant.host-paths"
            {
                continue;
            }
            if matches!(l.link.principal, Ent::User(_) | Ent::Team(_)) {
                self.tenants
                    .add_member(&operator, l.link.principal.clone(), "migration")
                    .map_err(tenant_err)?;
            }
            match l.link.template.as_str() {
                "role.system-admin" => self.make_administrator(&l.link.principal, "migration")?,
                "role.auditor" => {
                    self.add_link(
                        "role.directory-auditor",
                        l.link.principal.clone(),
                        Ent::Directory(crate::tenants::LOCAL_DIRECTORY.into()),
                        "migration",
                    )?;
                }
                _ => {}
            }
        }
        self.tenants.mark_done(KEY).map_err(tenant_err)?;
        Ok(())
    }

    // ---- authorization --------------------------------------------------

    /// Add `p`'s entities (user and teams, token and owner) to `es`.
    pub fn principal_entities(&self, p: &Principal, es: &mut EntitySet) {
        if let Some(u) = &p.user {
            es.user(&u.id, u.disabled, &p.teams);
        }
        if let Some(t) = &p.token {
            let owner = match &t.kind {
                TokenKind::Personal { owner } => Some(owner.as_str()),
                TokenKind::ServiceAccount { .. } => None,
            };
            es.token(&t.id, owner, t.expires_at <= store::now());
        }
    }

    /// Fill in what tenancy says about the request (spec/tenancy.md §10):
    /// each project's tenant, the tenants with their attributes, the
    /// cluster's operator and admissions, the principal's memberships;
    /// returns the context fields `cluster`, `directory` and, when the
    /// resource belongs to one, `tenant`.
    fn complete_tenancy(
        &self,
        p: &Principal,
        resource: &Ent,
        es: &mut EntitySet,
    ) -> Result<Vec<(&'static str, Ent)>, AuthError> {
        use crate::tenants::{ADMIN_TENANT, DEFAULT_TENANT, LOCAL_DIRECTORY};
        let snap = self.tenants.snapshot().map_err(tenant_err)?;
        let project_tenant = |id: &str| -> Result<String, AuthError> {
            Ok(self
                .tenants
                .project_tenant(id)
                .map_err(tenant_err)?
                .unwrap_or_else(|| DEFAULT_TENANT.to_string()))
        };
        // Each project's tenant, which the request then needs as an entity
        // (`context.project.tenant.clusters`, base.admission) whether or
        // not the caller is a member of it.
        let mut project_tenants: Vec<String> = Vec::new();
        for pid in es.projects() {
            let t = project_tenant(&pid)?;
            es.set_project_tenant(&pid, &t);
            project_tenants.push(t);
        }
        // Memberships (D3): listed members, and links on a tenant or on one
        // of its projects. Break-glass principals belong to the operator.
        let mut tenants: Vec<String> = Vec::new();
        let mut mine: Vec<Ent> = p.teams.iter().map(|t| Ent::Team(t.clone())).collect();
        if let Some(u) = &p.user {
            mine.push(Ent::User(u.id.clone()));
        }
        for m in snap.members.iter().filter(|m| mine.contains(&m.principal)) {
            tenants.push(m.tenant_id.clone());
        }
        for l in self.links_of(p)? {
            match &l.link.resource {
                Ent::Tenant(t) => tenants.push(t.clone()),
                Ent::Project(pid) => tenants.push(project_tenant(pid)?),
                _ => {}
            }
        }
        if p.teams.iter().any(|t| t == authz::BREAK_GLASS_TEAM) {
            tenants.push(snap.operator.clone());
        }
        if let Some(t) = &p.token
            && let TokenKind::ServiceAccount { project } = &t.kind
        {
            let tid = project_tenant(project)?;
            es.add_parents(&Ent::Token(t.id.clone()), std::slice::from_ref(&tid));
            tenants.push(tid);
        }
        tenants.sort();
        tenants.dedup();
        if let Some(u) = &p.user {
            es.add_parents(&Ent::User(u.id.clone()), &tenants);
        }
        // A personal token is also decided as itself (when narrowed), still
        // capped by its owner's decision.
        if let Some(t) = &p.token {
            es.add_parents(&Ent::Token(t.id.clone()), &tenants);
        }
        // The resource's tenant.
        let owner = match resource {
            Ent::Tenant(t) => Some(t.clone()),
            _ => match es.project_of(resource) {
                Some(pid) => Some(project_tenant(&pid)?),
                None => None,
            },
        };
        // Every tenant the request mentions, as stored.
        let mut wanted: Vec<String> = es.tenants();
        wanted.extend(project_tenants);
        wanted.extend(tenants.iter().cloned());
        wanted.extend(owner.iter().cloned());
        wanted.push(snap.operator.clone());
        wanted.push(snap.owner().to_string());
        wanted.sort();
        wanted.dedup();
        let directory = snap
            .directory
            .as_ref()
            .map(|d| d.id.clone())
            .unwrap_or_else(|| LOCAL_DIRECTORY.to_string());
        es.directory(&directory, snap.owner());
        for t in &wanted {
            match snap.tenant(t) {
                Some(rec) => es.tenant(
                    &rec.id,
                    &rec.directory_id,
                    rec.owns_directory,
                    rec.operator,
                    rec.suspended,
                    snap.admitted(&rec.id),
                ),
                // Unknown to the store (a test's bare entity set, a tenant
                // being created): no rights of its own.
                None => es.tenant(
                    t,
                    &directory,
                    t == ADMIN_TENANT,
                    t == ADMIN_TENANT,
                    false,
                    false,
                ),
            };
        }
        let admitted: Vec<String> = snap
            .admissions
            .iter()
            .filter(|a| snap.admitted(&a.tenant_id))
            .map(|a| a.tenant_id.clone())
            .collect();
        es.cluster(&snap.operator, &admitted);
        let mut ctx = vec![
            ("cluster", Ent::Cluster),
            ("directory", Ent::Directory(directory)),
        ];
        if let Some(t) = owner {
            ctx.push(("tenant", Ent::Tenant(t)));
        }
        Ok(ctx)
    }

    /// Decide `action` on `resource` for `p`. `extra` are context fields
    /// (`project`, `network`); `es` must hold the resource and anything
    /// named in `extra`.
    pub fn authorize(
        &self,
        p: &Principal,
        action: &str,
        resource: Ent,
        es: EntitySet,
        extra: &[(&'static str, Ent)],
    ) -> Decision {
        self.authorize_in(None, p, action, resource, es, extra)
    }

    /// As `authorize`, against `set` instead of the published policies
    /// (lock-out checks and `simulate`, spec §7.6).
    pub fn authorize_in(
        &self,
        set: Option<&cedar_policy::PolicySet>,
        p: &Principal,
        action: &str,
        resource: Ent,
        mut es: EntitySet,
        extra: &[(&'static str, Ent)],
    ) -> Decision {
        self.principal_entities(p, &mut es);
        let tenancy = match self.complete_tenancy(p, &resource, &mut es) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("tenancy lookup failed; denying: {}", e);
                return Decision {
                    allowed: false,
                    policies: Vec::new(),
                    errors: vec![e.to_string()],
                };
            }
        };
        let ask = |principal: Ent, as_owner: bool| {
            let mut q = Query::new(
                principal,
                action,
                resource.clone(),
                p.auth_context(as_owner),
                es.clone(),
            );
            for (k, e) in extra.iter().chain(tenancy.iter()) {
                q = q.with(k, e.clone());
            }
            match set {
                Some(set) => self.engine.check_with(set, &q),
                None => self.engine.check(&q),
            }
        };
        match (&p.token, &p.user) {
            (Some(_), Some(u)) => {
                // Personal token: as its owner, and (when narrowed) as itself.
                let owner = ask(Ent::User(u.id.clone()), true);
                if !p.token_narrowed || !owner.allowed {
                    return owner;
                }
                let own = ask(p.cedar(), false);
                Decision {
                    allowed: own.allowed,
                    policies: own.policies.into_iter().chain(owner.policies).collect(),
                    errors: own.errors.into_iter().chain(owner.errors).collect(),
                }
            }
            _ => ask(p.cedar(), false),
        }
    }

    // ---- principals -----------------------------------------------------

    /// The principal for a peer on `api.sock` (spec §5.2), or `None` if
    /// the peer may not use it.
    pub fn peer_principal(&self, uid: u32) -> Result<Option<Principal>, AuthError> {
        let Some(user) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
            .ok()
            .flatten()
        else {
            return Ok(None);
        };
        let groups = unix_groups(&user);
        let admin = uid == 0 || groups.contains(&self.config.admin_group);
        let member = admin || groups.contains(&self.config.users_group);
        if !member {
            return Ok(None);
        }
        let subject = authz::local_subject(&user.name);
        let first = self.store.identity("unix", &subject)?.is_none();
        let Some(u) = self
            .store
            .user_for_identity("unix", &subject, &user.name, None, true)?
        else {
            return Ok(None);
        };
        // D23: local accounts belong to this host's PAM connector. A user
        // seen for the first time joins its default tenant, as a first
        // PAM login does.
        if first
            && let Ok(c) = self.pam_connector(None)
            && let Some(t) = &c.default_tenant
        {
            self.tenants
                .add_member(t, Ent::User(u.id.clone()), &format!("connector:{}", c.id))
                .map_err(tenant_err)?;
        }
        let mut teams = self.store.teams_of(&u.id)?;
        // Unix groups become `unix:<group>` teams, except that the
        // break-glass team comes only from the configured admin group (or
        // root): a host group that merely shares its name grants nothing.
        teams.extend(
            groups
                .iter()
                .map(|g| format!("unix:{}", g))
                .filter(|t| t != authz::BREAK_GLASS_TEAM),
        );
        if admin {
            teams.push(authz::BREAK_GLASS_TEAM.into());
        }
        Ok(Some(Principal {
            user: Some(u),
            token: None,
            token_narrowed: false,
            teams,
            method: Method::Peer,
            transport: Transport::Unix,
            authenticated_at: store::now(),
            source_ip: None,
            session_hash: None,
            csrf: None,
            peer_uid: Some(uid),
            mfa: false,
        }))
    }

    /// The principal for a session cookie value.
    pub fn session_principal(
        &self,
        cookie: &str,
        transport: Transport,
        ip: Option<IpAddr>,
    ) -> Result<Option<Principal>, AuthError> {
        let hash = sha256_hex(cookie.as_bytes());
        let Some(mut s) = self.store.session(&hash)? else {
            return Ok(None);
        };
        let now = store::now();
        let cfg = &self.config.auth.session;
        if now > s.last_seen + cfg.idle_minutes * 60
            || now > s.created_at + cfg.absolute_hours * 3600
        {
            self.store.remove_session(&hash)?;
            return Ok(None);
        }
        let Some(user) = self.store.user(&s.user_id)? else {
            return Ok(None);
        };
        if user.disabled {
            return Ok(None);
        }
        if now >= s.last_seen + 60 {
            s.last_seen = now;
            self.store.put_session(&hash, &s)?;
        }
        let teams = self.store.teams_of(&user.id)?;
        let method = if s.method == "oidc" {
            Method::Oidc
        } else {
            Method::Pam
        };
        Ok(Some(Principal {
            user: Some(user),
            token: None,
            token_narrowed: false,
            teams,
            method,
            transport,
            authenticated_at: s.authenticated_at,
            source_ip: ip,
            session_hash: Some(hash),
            csrf: Some(s.csrf),
            peer_uid: None,
            mfa: s.mfa,
        }))
    }

    /// The principal for a bearer token.
    pub fn token_principal(
        &self,
        secret: &str,
        transport: Transport,
        ip: Option<IpAddr>,
    ) -> Result<Option<Principal>, AuthError> {
        if !secret.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }
        let hash = sha256_hex(secret.as_bytes());
        let Some(mut t) = self.store.token_by_hash(&hash)? else {
            return Ok(None);
        };
        let now = store::now();
        if t.expires_at <= now {
            return Ok(None);
        }
        let user = match &t.kind {
            TokenKind::Personal { owner } => match self.store.user(owner)? {
                Some(u) if !u.disabled => Some(u),
                _ => return Ok(None),
            },
            TokenKind::ServiceAccount { .. } => None,
        };
        let teams = match &user {
            Some(u) => self.store.teams_of(&u.id)?,
            None => Vec::new(),
        };
        let narrowed = self
            .store
            .links()?
            .iter()
            .any(|l| l.link.principal == Ent::Token(t.id.clone()));
        if t.last_used_at.is_none_or(|at| now >= at + 60) {
            t.last_used_at = Some(now);
            t.last_used_from = ip.map(|i| i.to_string());
            self.store.put_token(&hash, &t)?;
        }
        Ok(Some(Principal {
            user,
            token: Some(t),
            token_narrowed: narrowed,
            teams,
            method: Method::Token,
            transport,
            authenticated_at: now,
            source_ip: ip,
            session_hash: None,
            csrf: None,
            peer_uid: None,
            mfa: false,
        }))
    }

    /// A principal for `e` as if it made a request now (for `simulate`).
    pub fn principal_for(&self, e: &Ent) -> Result<Option<Principal>, AuthError> {
        let base = |user: Option<User>,
                    token: Option<Token>,
                    teams: Vec<String>,
                    narrowed: bool,
                    method: Method| Principal {
            user,
            token,
            token_narrowed: narrowed,
            teams,
            method,
            transport: Transport::Tcp,
            authenticated_at: store::now(),
            source_ip: None,
            session_hash: None,
            csrf: None,
            peer_uid: None,
            mfa: false,
        };
        Ok(match e {
            Ent::User(id) => match self.store.user(id)? {
                Some(u) => {
                    let teams = self.store.teams_of(&u.id)?;
                    Some(base(Some(u), None, teams, false, Method::Pam))
                }
                None => None,
            },
            Ent::Token(id) => match self.store.token_by_id(id)? {
                Some((_, t)) => {
                    let user = match &t.kind {
                        TokenKind::Personal { owner } => self.store.user(owner)?,
                        TokenKind::ServiceAccount { .. } => None,
                    };
                    let teams = match &user {
                        Some(u) => self.store.teams_of(&u.id)?,
                        None => Vec::new(),
                    };
                    let narrowed = self
                        .store
                        .links()?
                        .iter()
                        .any(|l| l.link.principal == Ent::Token(t.id.clone()));
                    Some(base(user, Some(t), teams, narrowed, Method::Token))
                }
                None => None,
            },
            _ => None,
        })
    }

    /// Link records that apply to `p` (its user, teams, token).
    pub fn links_of(&self, p: &Principal) -> Result<Vec<LinkRecord>, AuthError> {
        let mut mine: Vec<Ent> = p.teams.iter().map(|t| Ent::Team(t.clone())).collect();
        if let Some(u) = &p.user {
            mine.push(Ent::User(u.id.clone()));
        }
        if let Some(t) = &p.token {
            mine.push(Ent::Token(t.id.clone()));
        }
        Ok(self
            .store
            .links()?
            .into_iter()
            .filter(|l| mine.contains(&l.link.principal))
            .collect())
    }

    // ---- sessions -------------------------------------------------------

    /// Start a session; returns the cookie value and the CSRF value.
    pub fn create_session(
        &self,
        user: &User,
        method: Method,
    ) -> Result<(String, String), AuthError> {
        self.create_session_with(user, method, false)
    }

    /// A session whose login was multi-factor (`mfa`) or not.
    pub fn create_session_with(
        &self,
        user: &User,
        method: Method,
        mfa: bool,
    ) -> Result<(String, String), AuthError> {
        let id = random_token(32);
        let csrf = random_token(24);
        let now = store::now();
        self.store.put_session(
            &sha256_hex(id.as_bytes()),
            &Session {
                user_id: user.id.clone(),
                method: method.as_str().into(),
                csrf: csrf.clone(),
                created_at: now,
                last_seen: now,
                authenticated_at: now,
                mfa,
            },
        )?;
        let cfg = &self.config.auth.session;
        let _ = self.store.prune_sessions(|s| {
            now > s.last_seen + cfg.idle_minutes * 60
                || now > s.created_at + cfg.absolute_hours * 3600
        });
        Ok((id, csrf))
    }

    pub fn end_session(&self, p: &Principal) -> Result<(), AuthError> {
        if let Some(h) = &p.session_hash {
            self.store.remove_session(h)?;
        }
        Ok(())
    }

    /// PAM login through glidex-authd (spec §5.3). Returns the user.
    pub fn login_pam(&self, username: &str, password: &str) -> Result<User, AuthError> {
        self.login_pam_with(None, username, password)
            .map(|(u, _)| u)
    }

    /// The PAM connector `id`, or the first one.
    pub fn pam_connector(
        &self,
        id: Option<&str>,
    ) -> Result<crate::directory::Connector, AuthError> {
        use crate::directory::Kind;
        let c = match id {
            Some(id) => self
                .connectors
                .get(id)
                .map_err(connector_err)?
                .filter(|c| c.kind == Kind::Pam && c.enabled),
            None => self.connectors.first(Kind::Pam).map_err(connector_err)?,
        };
        c.ok_or_else(|| AuthError::Unavailable("PAM login is disabled".into()))
    }

    /// A PAM login through connector `id` (or the first PAM connector):
    /// glidex-authd checks the password on this host (D23), then the
    /// common path. Returns the user and the connector.
    pub fn login_pam_with(
        &self,
        id: Option<&str>,
        username: &str,
        password: &str,
    ) -> Result<(User, crate::directory::Connector), AuthError> {
        let c = self.pam_connector(id)?;
        let cfg = &self.config.auth.pam;
        let client = glidex_authd::client::AuthdClient::new(&cfg.authd_socket);
        let ok = match client.authenticate(username, password, &cfg.service) {
            Ok(ok) => ok,
            Err(glidex_authd::client::AuthdError::Denied) => return Err(AuthError::Denied),
            Err(glidex_authd::client::AuthdError::RateLimited) => {
                return Err(AuthError::RateLimited);
            }
            Err(e) => return Err(AuthError::Unavailable(format!("glidex-authd: {}", e))),
        };
        let user = self.pam_login_groups(&c, username, &ok.groups)?;
        Ok((user, c))
    }

    /// The user for a successful PAM login with `groups` (first connector).
    pub fn pam_user(&self, username: &str, groups: &[String]) -> Result<User, AuthError> {
        let c = self.pam_connector(None)?;
        self.pam_login_groups(&c, username, groups)
    }

    fn pam_login_groups(
        &self,
        c: &crate::directory::Connector,
        username: &str,
        groups: &[String],
    ) -> Result<User, AuthError> {
        let s = c.pam().map_err(connector_err)?;
        if !groups.iter().any(|g| s.allowed_groups.contains(g)) {
            return Err(AuthError::Denied);
        }
        let id = crate::directory::ExternalIdentity {
            connector_id: c.id.clone(),
            subject: authz::local_subject(username),
            display_name: username.to_string(),
            email: None,
            groups: groups.to_vec(),
            mfa: false,
            email_verified: false,
        };
        // A host account's refusals all look like a wrong password.
        self.external_login(c, &id).map_err(|e| match e {
            AuthError::NotAllowed(_) => AuthError::Denied,
            other => other,
        })
    }

    /// The common login path (spec/tenancy.md §6.2): the connector's rules,
    /// the user (created on first login when `jit`), its managed team
    /// memberships, and a new user's tenant.
    pub fn external_login(
        &self,
        c: &crate::directory::Connector,
        id: &crate::directory::ExternalIdentity,
    ) -> Result<User, AuthError> {
        self.external_login_invited(c, id, None)
    }

    /// [`external_login`](Self::external_login) bringing an invitation
    /// (§6.1): taken when its secret, connector and the verified email
    /// match, it makes the user (even with `jit` off) a member of its
    /// tenant with its role. A login with an invitation it can't take is
    /// refused, like any other.
    pub fn external_login_invited(
        &self,
        c: &crate::directory::Connector,
        id: &crate::directory::ExternalIdentity,
        invite: Option<&crate::directory::invitations::InviteToken>,
    ) -> Result<User, AuthError> {
        if !c.enabled {
            return Err(AuthError::NotAllowed("this connector is disabled".into()));
        }
        if !c.required_groups.is_empty() && !id.groups.iter().any(|g| c.required_groups.contains(g))
        {
            return Err(AuthError::NotAllowed("not in a required group".into()));
        }
        if c.require_mfa && !id.mfa {
            return Err(AuthError::NotAllowed(
                "this connector requires a multi-factor login".into(),
            ));
        }
        let provider = c.identity_provider();
        let existed = self.store.identity(&provider, &id.subject)?.is_some();
        let invited = match invite {
            Some(t) => Some(
                self.invitations
                    .would_take(t, &c.id, id.email.as_deref(), id.email_verified)
                    .map_err(|e| AuthError::Invalid(e.to_string()))?
                    .ok_or_else(|| AuthError::NotAllowed("the invitation can't be taken".into()))?,
            ),
            None => None,
        };
        // A connector with an allowlist gives new accounts to those
        // verified addresses only (D39); an invitation is its own way in.
        let allowed = c.allowed_emails();
        if !existed
            && invited.is_none()
            && !allowed.is_empty()
            && !(id.email_verified
                && id
                    .email
                    .as_deref()
                    .is_some_and(|e| allowed.iter().any(|a| a.eq_ignore_ascii_case(e))))
        {
            return Err(AuthError::NotAllowed(
                "this address may not get an account through this connector".into(),
            ));
        }
        let user = self
            .store
            .user_for_identity(
                &provider,
                &id.subject,
                &id.display_name,
                id.email.clone(),
                c.jit || invited.is_some(),
            )?
            .ok_or_else(|| AuthError::NotAllowed("no glidex account for this identity".into()))?;
        if user.disabled {
            return Err(AuthError::NotAllowed("this account is disabled".into()));
        }
        if let Some(t) = invite {
            let inv = self
                .invitations
                .take(t, &c.id, id.email.as_deref(), id.email_verified, &user.id)
                .map_err(|_| AuthError::NotAllowed("the invitation can't be taken".into()))?;
            let by = format!("invitation:{}", inv.id);
            self.tenants
                .add_member(&inv.tenant_id, Ent::User(user.id.clone()), &by)
                .map_err(tenant_err)?;
            // `member` is membership alone.
            if inv.role != crate::service::invitations::MEMBER {
                self.add_link(
                    &inv.role,
                    Ent::User(user.id.clone()),
                    Ent::Tenant(inv.tenant_id.clone()),
                    &by,
                )?;
            }
        }
        let teams: Vec<String> = id
            .groups
            .iter()
            .filter_map(|g| c.group_teams.get(g).cloned())
            .collect();
        self.store
            .sync_memberships(&user.id, c.member_source(), &teams)?;
        if !existed && let Some(t) = &c.default_tenant {
            self.tenants
                .add_member(
                    t,
                    Ent::User(user.id.clone()),
                    &format!("connector:{}", c.id),
                )
                .map_err(tenant_err)?;
        }
        Ok(user)
    }

    /// Whether session `hash` is still `user_id`'s and live: not ended,
    /// idle or past its lifetime, its user not disabled.
    pub fn session_is_live(&self, hash: &str, user_id: &str) -> bool {
        let Ok(Some(s)) = self.store.session(hash) else {
            return false;
        };
        let now = store::now();
        let cfg = &self.config.auth.session;
        s.user_id == user_id
            && now <= s.last_seen + cfg.idle_minutes * 60
            && now <= s.created_at + cfg.absolute_hours * 3600
            && self
                .store
                .user(user_id)
                .ok()
                .flatten()
                .is_some_and(|u| !u.disabled)
    }

    /// Add a sign-in through connector `c` to user `user_id`, who is
    /// signed in (spec/tenancy.md §6.2): beside the ones it has, never in
    /// place of them. The connector's rules apply as at a login; a sign-in
    /// that already reaches another user is refused, one that reaches this
    /// user is a no-op. Returns whether it was added.
    pub fn link_external(
        &self,
        c: &crate::directory::Connector,
        id: &crate::directory::ExternalIdentity,
        user_id: &str,
    ) -> Result<bool, AuthError> {
        if !c.enabled {
            return Err(AuthError::NotAllowed("this connector is disabled".into()));
        }
        if !c.required_groups.is_empty() && !id.groups.iter().any(|g| c.required_groups.contains(g))
        {
            return Err(AuthError::NotAllowed("not in a required group".into()));
        }
        if c.require_mfa && !id.mfa {
            return Err(AuthError::NotAllowed(
                "this connector requires a multi-factor login".into(),
            ));
        }
        let provider = c.identity_provider();
        if let Some(i) = self.store.identity(&provider, &id.subject)? {
            if i.user_id == user_id {
                return Ok(false);
            }
            return Err(AuthError::NotAllowed(
                "this sign-in already belongs to another user".into(),
            ));
        }
        self.store.put_identity(&store::Identity {
            provider,
            subject: id.subject.clone(),
            user_id: user_id.into(),
            email: id.email.clone(),
            created_at: store::now(),
        })?;
        let teams: Vec<String> = id
            .groups
            .iter()
            .filter_map(|g| c.group_teams.get(g).cloned())
            .collect();
        self.store
            .sync_memberships(user_id, c.member_source(), &teams)?;
        Ok(true)
    }

    /// Any enabled connector that logs in through the OIDC code: `oidc`
    /// or a preset (`google`, `jumpcloud`). The login page shows its
    /// sign-in buttons by it.
    pub fn oidc_enabled(&self) -> bool {
        self.connectors
            .list()
            .unwrap_or_default()
            .iter()
            .any(|c| c.enabled && c.kind.is_oidc())
    }

    /// The OIDC client of connector `id` (or the first OIDC connector, for
    /// the routes from before connectors).
    pub fn oidc_for(
        self: &Arc<Self>,
        id: Option<&str>,
    ) -> Result<
        (
            crate::directory::Connector,
            Arc<crate::directory::oidc::OidcState>,
        ),
        AuthError,
    > {
        self.oidc_client(id, false)
    }

    /// The device-grant client of connector `id`: its own client for a
    /// `google` connector (Google takes the grant only from a *TVs and
    /// Limited Input* client), the browser's otherwise.
    pub fn oidc_device_for(
        self: &Arc<Self>,
        id: Option<&str>,
    ) -> Result<
        (
            crate::directory::Connector,
            Arc<crate::directory::oidc::OidcState>,
        ),
        AuthError,
    > {
        self.oidc_client(id, true)
    }

    fn oidc_client(
        self: &Arc<Self>,
        id: Option<&str>,
        device: bool,
    ) -> Result<
        (
            crate::directory::Connector,
            Arc<crate::directory::oidc::OidcState>,
        ),
        AuthError,
    > {
        use crate::directory::{FILE_OIDC, Kind};
        let c = match id {
            Some(id) => self
                .connectors
                .get(id)
                .map_err(connector_err)?
                .filter(|c| c.kind.is_oidc() && c.enabled),
            None => self.connectors.first(Kind::Oidc).map_err(connector_err)?,
        }
        .ok_or_else(|| AuthError::Store(StoreError::NotFound("no such OIDC connector".into())))?;
        // Google's device client is a client of its own.
        let device = device && c.kind == Kind::Google;
        let key = if device {
            format!("{}#device", c.id)
        } else {
            c.id.clone()
        };
        let mut cache = self.oidc.lock().unwrap();
        if let Some((rec, state)) = cache.get(&key)
            && *rec == c
        {
            return Ok((c, state.clone()));
        }
        let profile = c.oidc_profile(device).map_err(connector_err)?;
        let s = &profile.settings;
        let cfg = crate::config::OidcConfig {
            enabled: true,
            issuer: s.issuer.clone(),
            client_id: s.client_id.clone(),
            scopes: s.scopes.clone(),
            groups_claim: s.groups_claim.clone(),
            jit: c.jit,
            allowed_domains: s.allowed_domains.clone(),
            required_groups: c.required_groups.clone(),
            group_teams: c
                .group_teams
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            redirect_uri: s.redirect_uri.clone(),
        };
        let (secret, path) = if c.id == FILE_OIDC {
            (
                crate::directory::oidc::ClientSecret::Credential,
                "/api/auth/oidc/callback".to_string(),
            )
        } else {
            let me = Arc::downgrade(self);
            let rec = c.clone();
            let field = profile.secret_field;
            let f = move || -> Result<zeroize::Zeroizing<String>, String> {
                let svc = me.upgrade().ok_or("shutting down")?;
                svc.connectors
                    .secret(&rec, field)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("connector {} has no {field}", rec.id))
            };
            (
                crate::directory::oidc::ClientSecret::Stored(Box::new(f)),
                format!("/api/auth/c/{}/callback", c.id),
            )
        };
        let state = Arc::new(
            crate::directory::oidc::OidcState::with(&cfg, secret, s.mfa_acr_values.clone(), &path)
                .with_preset(profile.preset.clone()),
        );
        cache.insert(key, (c.clone(), state.clone()));
        Ok((c, state))
    }

    // ---- tokens ---------------------------------------------------------

    /// Create a token; returns its secret (shown once) and record.
    /// `device` / `client` are what a `gxctl auth login` client says it is;
    /// stored redacted for display and audit only, never trusted
    /// (spec/gxctl-auth.md §7.2).
    pub fn create_token(
        &self,
        name: &str,
        kind: TokenKind,
        created_by: &str,
        days: Option<u64>,
        device: Option<&str>,
        client: Option<&str>,
    ) -> Result<(String, Token), AuthError> {
        let cfg = &self.config.auth.tokens;
        let days = days.unwrap_or(cfg.default_days);
        if days == 0 || days > cfg.max_days {
            return Err(AuthError::Invalid(format!(
                "token lifetime must be 1-{} days",
                cfg.max_days
            )));
        }
        if name.is_empty() || name.len() > 64 {
            return Err(AuthError::Invalid(
                "token name must be 1-64 characters".into(),
            ));
        }
        let secret = format!("{}{}", TOKEN_PREFIX, random_token(32));
        let now = store::now();
        let t = Token {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            kind,
            created_by: created_by.to_string(),
            created_at: now,
            expires_at: now + days * 86400,
            last_used_at: None,
            last_used_from: None,
            device: stamp(device),
            client: stamp(client),
        };
        self.store.put_token(&sha256_hex(secret.as_bytes()), &t)?;
        Ok((secret, t))
    }

    /// Revoke a token and its links.
    pub fn revoke_token(&self, id: &str) -> Result<(), AuthError> {
        let (hash, _) = self
            .store
            .token_by_id(id)?
            .ok_or_else(|| StoreError::NotFound(id.into()))?;
        self.store.remove_token(&hash)?;
        let mut changed = false;
        for l in self.store.links_mentioning(&Ent::Token(id.into()))? {
            self.store.remove_link(&l.link.id)?;
            changed = true;
        }
        if changed {
            self.reload_policies()?;
        }
        Ok(())
    }

    // ---- role links -----------------------------------------------------

    /// Add a role link, validating the template and resource kind, and
    /// republish the policy set.
    pub fn add_link(
        &self,
        template: &str,
        principal: Ent,
        resource: Ent,
        by: &str,
    ) -> Result<LinkRecord, AuthError> {
        let project_role = authz::PROJECT_ROLES.contains(&template);
        let host_role = authz::HOST_ROLES.contains(&template);
        let tenant_role = authz::TENANT_ROLES.contains(&template);
        let directory_role = authz::DIRECTORY_ROLES.contains(&template);
        match &resource {
            Ent::Project(_) if project_role => {}
            Ent::Cluster if host_role => {}
            Ent::Tenant(_) if tenant_role => {}
            Ent::Directory(_) if directory_role => {}
            _ => {
                return Err(AuthError::Invalid(format!(
                    "{} can't be linked to {}: project roles link to a project, host roles to the host, tenant roles to a tenant, directory roles to the directory",
                    template, resource
                )));
            }
        }
        if !matches!(principal, Ent::User(_) | Ent::Team(_) | Ent::Token(_)) {
            return Err(AuthError::Invalid(
                "a role is given to a user, team or token".into(),
            ));
        }
        if let Some(existing) = self.store.links()?.into_iter().find(|l| {
            l.link.template == template
                && l.link.principal == principal
                && l.link.resource == resource
        }) {
            return Ok(existing);
        }
        let rec = LinkRecord {
            link: Link {
                id: format!("link.{}", uuid::Uuid::new_v4()),
                template: template.into(),
                principal,
                resource,
            },
            created_by: by.to_string(),
            created_at: store::now(),
        };
        self.store.put_link(&rec)?;
        if let Err(e) = self.reload_policies() {
            self.store.remove_link(&rec.link.id)?;
            return Err(e);
        }
        Ok(rec)
    }

    pub fn remove_link(&self, id: &str) -> Result<(), AuthError> {
        if !self.store.remove_link(id)? {
            return Err(StoreError::NotFound(id.into()).into());
        }
        self.reload_policies()
    }

    /// Remove every link mentioning `e` (a deleted project, team, user).
    pub fn forget_entity(&self, e: &Ent) -> Result<(), AuthError> {
        let links = self.store.links_mentioning(e)?;
        for l in &links {
            self.store.remove_link(&l.link.id)?;
        }
        if !links.is_empty() {
            self.reload_policies()?;
        }
        Ok(())
    }

    /// Projects in which `p` has any link (directly, through a team, or a
    /// token's own links), or every project for host links (spec §7.7).
    pub fn linked_projects(&self, p: &Principal) -> Result<LinkedProjects, AuthError> {
        if p.is_break_glass() {
            return Ok(LinkedProjects::All);
        }
        let mut mine: Vec<Ent> = p.teams.iter().map(|t| Ent::Team(t.clone())).collect();
        if let Some(u) = &p.user {
            mine.push(Ent::User(u.id.clone()));
        }
        if let Some(t) = &p.token {
            mine.push(Ent::Token(t.id.clone()));
        }
        let mut projects = Vec::new();
        for l in self.store.links()? {
            if !mine.contains(&l.link.principal) {
                continue;
            }
            match l.link.resource {
                Ent::Cluster => return Ok(LinkedProjects::All),
                Ent::Project(id) => projects.push(id),
                _ => {}
            }
        }
        if let Some(TokenKind::ServiceAccount { project }) = p.token.as_ref().map(|t| &t.kind) {
            projects.push(project.clone());
        }
        projects.sort();
        projects.dedup();
        Ok(LinkedProjects::Some(projects))
    }

    // ---- console tickets ------------------------------------------------

    pub fn issue_ticket(&self, p: &Principal, vm_id: &str) -> String {
        let t = random_token(32);
        let now = store::now();
        let mut tickets = self.tickets.lock().unwrap();
        tickets.retain(|_, v| v.expires > now);
        tickets.insert(
            sha256_hex(t.as_bytes()),
            Ticket {
                principal: p.cedar(),
                vm_id: vm_id.into(),
                expires: now + TICKET_SECS,
            },
        );
        t
    }

    /// Consume a ticket for `vm_id` held by `p`.
    pub fn redeem_ticket(&self, p: &Principal, vm_id: &str, ticket: &str) -> bool {
        let now = store::now();
        let mut tickets = self.tickets.lock().unwrap();
        match tickets.remove(&sha256_hex(ticket.as_bytes())) {
            Some(t) => t.expires > now && t.vm_id == vm_id && t.principal == p.cedar(),
            None => false,
        }
    }

    // ---- audit ----------------------------------------------------------

    /// Record one audited event in the journal and the audit table.
    #[allow(clippy::too_many_arguments)]
    pub fn audit(
        &self,
        p: Option<&Principal>,
        request_id: &str,
        action: &str,
        project: Option<&str>,
        target: Option<&str>,
        result: &str,
        error_code: Option<&str>,
        policies: &[String],
        details: serde_json::Value,
    ) {
        let entry = AuditEntry {
            time: store::now_millis(),
            request_id: request_id.to_string(),
            principal: p.map(|p| p.audit_json()).unwrap_or(serde_json::Value::Null),
            source: p.map(|p| p.source()).unwrap_or_else(|| "-".into()),
            action: action.to_string(),
            project: project.map(String::from),
            target: target.map(String::from),
            result: result.to_string(),
            error_code: error_code.map(String::from),
            policies: policies.to_vec(),
            details,
        };
        tracing::info!(target: "glidex_audit", "{}", serde_json::to_string(&entry).unwrap_or_default());
        if let Err(e) = self.store.append_audit(&entry) {
            tracing::error!("audit write failed: {}", e);
        }
        let n = self
            .audit_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n.is_multiple_of(500) {
            let keep = self.config.audit.retention_days * 86_400_000;
            let _ = self
                .store
                .prune_audit(store::now_millis().saturating_sub(keep));
        }
    }
}

pub enum LinkedProjects {
    All,
    Some(Vec<String>),
}

impl LinkedProjects {
    pub fn contains(&self, project: &str) -> bool {
        match self {
            LinkedProjects::All => true,
            LinkedProjects::Some(v) => v.iter().any(|p| p == project),
        }
    }
}

/// Whom bootstrap makes administrators: the members `admin_group` lists,
/// and the user this process runs as (`me`: name and [`unix_groups`]) when
/// it is in the group. The group's member list leaves out those whose
/// primary group it is, while `api.sock` counts them as admins; only this
/// one user is looked up, not every account.
fn bootstrap_members(
    admin_group: &str,
    mut listed: Vec<String>,
    me: Option<(String, Vec<String>)>,
) -> Vec<String> {
    if let Some((name, groups)) = me
        && groups.iter().any(|g| g == admin_group)
        && !listed.contains(&name)
    {
        listed.push(name);
    }
    listed
}

/// Names of every group `user` belongs to (primary first).
pub fn unix_groups(user: &nix::unistd::User) -> Vec<String> {
    let Ok(name) = std::ffi::CString::new(user.name.clone()) else {
        return Vec::new();
    };
    let gids = nix::unistd::getgrouplist(&name, user.gid).unwrap_or_else(|_| vec![user.gid]);
    gids.into_iter()
        .filter_map(|g| {
            nix::unistd::Group::from_gid(g)
                .ok()
                .flatten()
                .map(|g| g.name)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc() -> (Arc<AuthService>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(crate::store::Db::create(dir.path().join("a.db")).unwrap());
        let cfg = Config {
            authz: crate::config::AuthzConfig {
                policy_files_dir: dir.path().join("policies"),
                ..Default::default()
            },
            ..Default::default()
        };
        (AuthService::new(db, cfg).unwrap(), dir)
    }

    fn user_principal(s: &AuthService, name: &str) -> Principal {
        let u = s
            .store
            .user_for_identity("pam", name, name, None, true)
            .unwrap()
            .unwrap();
        Principal {
            user: Some(u),
            method: Method::Pam,
            transport: Transport::Tcp,
            ..Principal::system()
        }
        .with_teams(vec![])
    }

    impl Principal {
        fn with_teams(mut self, t: Vec<String>) -> Self {
            self.teams = t;
            self
        }
    }

    #[test]
    fn base62_round_numbers() {
        assert_eq!(base62(&[0]), "0");
        assert_eq!(base62(&[61]), "z");
        assert_eq!(base62(&[62]), "10");
        assert!(random_token(32).len() >= 40);
    }

    #[test]
    fn personal_token_is_owner_intersect_token() {
        let (s, _d) = svc();
        let p = user_principal(&s, "alice");
        let uid = p.user_id().unwrap().to_string();
        s.add_link(
            "role.editor",
            Ent::User(uid.clone()),
            Ent::Project("pa".into()),
            "t",
        )
        .unwrap();
        let (secret, tok) = s
            .create_token(
                "ci",
                TokenKind::Personal { owner: uid.clone() },
                &uid,
                None,
                None,
                None,
            )
            .unwrap();
        let mut es = EntitySet::new();
        es.in_project(Ent::Vm("v".into()), "pa");
        // Unnarrowed: acts as the owner.
        let tp = s
            .token_principal(&secret, Transport::Tcp, None)
            .unwrap()
            .unwrap();
        let d = s.authorize(&tp, "startVm", Ent::Vm("v".into()), es.clone(), &[]);
        assert!(d.allowed, "{d:?}");
        // Narrowed to viewer: can read, can't start, even though the owner can.
        s.add_link(
            "role.viewer",
            Ent::Token(tok.id.clone()),
            Ent::Project("pa".into()),
            "t",
        )
        .unwrap();
        let tp = s
            .token_principal(&secret, Transport::Tcp, None)
            .unwrap()
            .unwrap();
        assert!(tp.token_narrowed);
        assert!(
            s.authorize(&tp, "readVm", Ent::Vm("v".into()), es.clone(), &[])
                .allowed
        );
        assert!(
            !s.authorize(&tp, "startVm", Ent::Vm("v".into()), es.clone(), &[])
                .allowed
        );
        // Narrowed to owner (more than the user has): still capped by the user.
        s.add_link(
            "role.owner",
            Ent::Token(tok.id.clone()),
            Ent::Project("pa".into()),
            "t",
        )
        .unwrap();
        let tp = s
            .token_principal(&secret, Transport::Tcp, None)
            .unwrap()
            .unwrap();
        assert!(
            !s.authorize(
                &tp,
                "manageBindings",
                Ent::Project("pa".into()),
                es.clone(),
                &[]
            )
            .allowed
        );
        // Revoked: gone.
        s.revoke_token(&tok.id).unwrap();
        assert!(
            s.token_principal(&secret, Transport::Tcp, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unnarrowed_token_never_passes_step_up() {
        let (s, _d) = svc();
        let p = user_principal(&s, "net");
        let uid = p.user_id().unwrap().to_string();
        s.add_link("role.net-admin", Ent::User(uid.clone()), Ent::Cluster, "t")
            .unwrap();
        // Host roles work for members of the operator tenant (spec/tenancy.md D4).
        s.tenants
            .add_member(crate::tenants::ADMIN_TENANT, Ent::User(uid.clone()), "t")
            .unwrap();
        let (secret, tok) = s
            .create_token(
                "t",
                TokenKind::Personal { owner: uid.clone() },
                &uid,
                None,
                None,
                None,
            )
            .unwrap();
        let tp = s
            .token_principal(&secret, Transport::Tcp, None)
            .unwrap()
            .unwrap();
        let d = s.authorize(&tp, "installOvs", Ent::Host, EntitySet::new(), &[]);
        assert!(d.denied_by("base.step-up"), "{:?}", d);
        s.add_link("role.net-admin", Ent::Token(tok.id), Ent::Cluster, "t")
            .unwrap();
        let tp = s
            .token_principal(&secret, Transport::Tcp, None)
            .unwrap()
            .unwrap();
        assert!(
            s.authorize(&tp, "installOvs", Ent::Host, EntitySet::new(), &[])
                .allowed
        );
    }

    #[test]
    fn sessions_expire_and_logout() {
        let (s, _d) = svc();
        let p = user_principal(&s, "bob");
        let (cookie, csrf) = s
            .create_session(p.user.as_ref().unwrap(), Method::Pam)
            .unwrap();
        let sp = s
            .session_principal(&cookie, Transport::Tcp, None)
            .unwrap()
            .unwrap();
        assert_eq!(sp.csrf.as_deref(), Some(csrf.as_str()));
        s.end_session(&sp).unwrap();
        assert!(
            s.session_principal(&cookie, Transport::Tcp, None)
                .unwrap()
                .is_none()
        );
        assert!(
            s.session_principal("nope", Transport::Tcp, None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pam_user_requires_allowed_group_and_syncs_teams() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(crate::store::Db::create(dir.path().join("a.db")).unwrap());
        let mut cfg = Config {
            authz: crate::config::AuthzConfig {
                policy_files_dir: dir.path().join("p"),
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.auth
            .pam
            .group_teams
            .insert("lab-unix".into(), "lab".into());
        let s = AuthService::new(db, cfg).unwrap();
        assert!(matches!(
            s.pam_user("carol", &["staff".into()]),
            Err(AuthError::Denied)
        ));
        s.store
            .put_team(&store::Team {
                id: "t1".into(),
                name: "lab".into(),
                members: vec![],
                created_at: 0,
            })
            .unwrap();
        let u = s
            .pam_user("carol", &["glidex-users".into(), "lab-unix".into()])
            .unwrap();
        assert_eq!(s.store.teams_of(&u.id).unwrap(), vec!["t1"]);
        let u2 = s.pam_user("carol", &["glidex-users".into()]).unwrap();
        assert_eq!(u2.id, u.id);
        assert!(s.store.teams_of(&u.id).unwrap().is_empty());
    }

    #[test]
    fn bootstrap_admins_once() {
        let (s, _d) = svc();
        assert_eq!(
            s.bootstrap_with(&["root2".into()], "pdefault").unwrap(),
            vec!["root2"]
        );
        let links = s.store.links().unwrap();
        // System admin and default-project owner, plus what tenancy adds:
        // directory-admin and tenant-admin of admin and default.
        assert_eq!(links.len(), 5);
        assert!(
            s.tenants
                .members(crate::tenants::ADMIN_TENANT)
                .unwrap()
                .iter()
                .any(|m| matches!(&m.principal, Ent::User(_)))
        );
        assert!(
            links
                .iter()
                .any(|l| l.link.template == "role.system-admin" && l.link.resource == Ent::Cluster)
        );
        // A PAM login of the same name is the same user.
        let u = s
            .store
            .user_for_identity("pam", "root2", "root2", None, true)
            .unwrap()
            .unwrap();
        assert!(
            links
                .iter()
                .all(|l| l.link.principal == Ent::User(u.id.clone()))
        );
        // Only on first start.
        assert!(
            s.bootstrap_with(&["other".into()], "pdefault")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn bootstrap_counts_the_process_user_in_the_admin_group() {
        let me = |groups: &[&str]| {
            Some((
                "tester".to_string(),
                groups.iter().map(|g| g.to_string()).collect(),
            ))
        };
        // Its primary group, which the group's member list leaves out.
        assert_eq!(
            bootstrap_members("tester", vec![], me(&["tester"])),
            vec!["tester"]
        );
        assert_eq!(
            bootstrap_members(
                "glidex-admin",
                vec!["alice".into()],
                me(&["tester", "glidex-admin"])
            ),
            vec!["alice", "tester"]
        );
        // Listed already: once.
        assert_eq!(
            bootstrap_members("glidex-admin", vec!["tester".into()], me(&["glidex-admin"])),
            vec!["tester"]
        );
        // Not in the group (the packaged service user): nobody added.
        assert_eq!(
            bootstrap_members("glidex-admin", vec!["alice".into()], me(&["glidex"])),
            vec!["alice"]
        );
        assert_eq!(
            bootstrap_members("glidex-admin", vec![], None),
            Vec::<String>::new()
        );
    }

    #[test]
    fn tickets_are_single_use_and_bound() {
        let (s, _d) = svc();
        let p = user_principal(&s, "dave");
        let q = user_principal(&s, "erin");
        let t = s.issue_ticket(&p, "vm1");
        assert!(!s.redeem_ticket(&q, "vm1", &t), "bound to the principal");
        let t = s.issue_ticket(&p, "vm1");
        assert!(!s.redeem_ticket(&p, "vm2", &t), "bound to the VM");
        let t = s.issue_ticket(&p, "vm1");
        assert!(s.redeem_ticket(&p, "vm1", &t));
        assert!(!s.redeem_ticket(&p, "vm1", &t), "single use");
    }

    #[test]
    fn link_rules() {
        let (s, _d) = svc();
        assert!(
            s.add_link("role.owner", Ent::User("u".into()), Ent::Cluster, "t")
                .is_err()
        );
        assert!(
            s.add_link(
                "role.net-admin",
                Ent::User("u".into()),
                Ent::Project("p".into()),
                "t"
            )
            .is_err()
        );
        assert!(
            s.add_link("role.nope", Ent::User("u".into()), Ent::Cluster, "t")
                .is_err()
        );
        let a = s
            .add_link(
                "role.viewer",
                Ent::Team("t".into()),
                Ent::Project("p".into()),
                "t",
            )
            .unwrap();
        let b = s
            .add_link(
                "role.viewer",
                Ent::Team("t".into()),
                Ent::Project("p".into()),
                "t",
            )
            .unwrap();
        assert_eq!(a.link.id, b.link.id, "idempotent");
        s.forget_entity(&Ent::Project("p".into())).unwrap();
        assert!(s.store.links().unwrap().is_empty());
    }

    #[test]
    fn file_policies_load_and_collide() {
        let (s, d) = svc();
        let dir = d.path().join("policies");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.cedar"), "@id(\"site.f1\")\nforbid (principal, action == Glidex::Action::\"openConsole\", resource);").unwrap();
        s.reload_policies().unwrap();
        assert!(
            s.engine
                .listing()
                .iter()
                .any(|p| p.id == "site.f1" && p.source == PolicySource::File)
        );
        s.store.write_site_policy("site.f1", 0, Some(("@id(\"site.f1\")\npermit (principal, action == Glidex::Action::\"readVm\", resource);", "", true)), "u").unwrap();
        assert!(s.reload_policies().is_err());
    }
}
