//! Directory connectors (spec/tenancy.md §5, D16; milestone T1).
//!
//! A connector is one IdP or user source of a directory: in T1, `pam`
//! (the co-hosted cluster's Unix accounts through glidex-authd) and
//! `oidc` (any OpenID Connect provider). Connectors made through the API
//! live in the `connectors` table; `auth.pam` and `auth.oidc` in
//! `control-plane.json` are **file-defined** connectors (`file-pam`,
//! `file-oidc`), listed with the rest and read-only through the API.
//!
//! Every login, whichever connector, ends in one [`ExternalIdentity`],
//! which `AuthService::external_login` turns into a user (§6).

pub mod secrets;

use crate::config::Config;
use crate::store::Db;
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use zeroize::Zeroizing;

const CONNECTORS: TableDefinition<&str, &[u8]> = TableDefinition::new("connectors");
const SECRETS: TableDefinition<&str, &[u8]> = TableDefinition::new("connector_secrets");
const KEYS: TableDefinition<&str, &[u8]> = TableDefinition::new("directory_keys");

pub const FILE_PAM: &str = "file-pam";
pub const FILE_OIDC: &str = "file-oidc";
/// Longest a login-minted personal token of a connector without
/// provisioning may last (D20).
pub const DEFAULT_TOKEN_MAX_DAYS: u64 = 30;

#[derive(Debug, thiserror::Error)]
pub enum ConnectorError {
    #[error("connector not found: {0}")]
    NotFound(String),
    #[error("connector already exists: {0}")]
    AlreadyExists(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0} is defined in control-plane.json and can't be changed through the API")]
    FileDefined(String),
    #[error(transparent)]
    Secret(#[from] secrets::SecretError),
    #[error("connector storage error: {0}")]
    Storage(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for ConnectorError {
            fn from(e: $t) -> Self {
                ConnectorError::Storage(e.to_string())
            }
        }
    )*};
}
storage_from!(
    crate::store::StoreError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    serde_json::Error
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Pam,
    Oidc,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Pam => "pam",
            Kind::Oidc => "oidc",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "pam" => Some(Kind::Pam),
            "oidc" => Some(Kind::Oidc),
            _ => None,
        }
    }

    /// The secret fields a kind has.
    pub fn secret_fields(&self) -> &'static [&'static str] {
        match self {
            Kind::Pam => &[],
            Kind::Oidc => &["client_secret"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Api,
    File,
}

/// `pam` settings: which Unix groups may log in through it. glidex-authd's
/// own `allowed_groups` stays the outer fence (D23).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PamSettings {
    pub allowed_groups: Vec<String>,
}

/// `oidc` settings (security.md §5.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    #[serde(default = "default_groups_claim")]
    pub groups_claim: String,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Where the IdP sends the browser back; default
    /// `<first allowed origin>/api/auth/c/<id>/callback`.
    #[serde(default)]
    pub redirect_uri: Option<String>,
    /// `acr` values that count as multi-factor, besides `amr` containing
    /// `mfa` (D22).
    #[serde(default)]
    pub mfa_acr_values: Vec<String>,
}

fn default_scopes() -> Vec<String> {
    ["openid", "profile", "email", "groups"]
        .map(String::from)
        .to_vec()
}

fn default_groups_claim() -> String {
    "groups".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connector {
    pub id: String,
    pub directory_id: String,
    pub kind: Kind,
    #[serde(default)]
    pub display_name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Create a user on the first login (when the rules below pass).
    #[serde(default)]
    pub jit: bool,
    /// Login only for members of one of these IdP (or Unix) groups.
    #[serde(default)]
    pub required_groups: Vec<String>,
    #[serde(default)]
    pub require_mfa: bool,
    /// IdP group → team name, synced at every login (D18).
    #[serde(default)]
    pub group_teams: BTreeMap<String, String>,
    /// New users become members of this tenant.
    #[serde(default)]
    pub default_tenant: Option<String>,
    /// Longest personal token a login through it mints (D20).
    #[serde(default)]
    pub token_max_days: Option<u64>,
    /// Kind-specific, see [`PamSettings`] and [`OidcSettings`].
    #[serde(default)]
    pub settings: serde_json::Value,
    pub source: Source,
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

fn yes() -> bool {
    true
}

impl Connector {
    pub fn pam(&self) -> Result<PamSettings, ConnectorError> {
        serde_json::from_value(self.settings.clone())
            .map_err(|e| ConnectorError::Invalid(format!("pam settings: {e}")))
    }

    pub fn oidc(&self) -> Result<OidcSettings, ConnectorError> {
        serde_json::from_value(self.settings.clone())
            .map_err(|e| ConnectorError::Invalid(format!("oidc settings: {e}")))
    }

    /// The `provider` its identities are stored under. The file-defined
    /// connectors keep today's keys (`pam`, `oidc:<issuer>`), so nothing
    /// needs migrating; host accounts are `pam` whichever connector checks
    /// them (D23).
    pub fn identity_provider(&self) -> String {
        match (self.kind, self.id.as_str()) {
            (Kind::Pam, _) => "pam".into(),
            (Kind::Oidc, FILE_OIDC) => {
                format!("oidc:{}", self.oidc().map(|s| s.issuer).unwrap_or_default())
            }
            (Kind::Oidc, id) => format!("conn:{id}"),
        }
    }

    /// What team memberships it manages are marked with.
    pub fn member_source(&self) -> crate::auth::store::MemberSource {
        use crate::auth::store::MemberSource;
        match self.id.as_str() {
            FILE_PAM => MemberSource::Pam,
            FILE_OIDC => MemberSource::Oidc,
            id => MemberSource::Connector(id.into()),
        }
    }

    pub fn token_max_days(&self) -> u64 {
        self.token_max_days.unwrap_or(DEFAULT_TOKEN_MAX_DAYS)
    }

    /// Check settings and rules (§5, D22). `guarded` is whether new users
    /// land in the directory's owner or an operator tenant.
    pub fn validate(&self, guarded: bool) -> Result<(), ConnectorError> {
        let id_ok = !self.id.is_empty()
            && self.id.len() <= 48
            && self
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !self.id.starts_with('-');
        if !id_ok {
            return Err(ConnectorError::Invalid(format!(
                "invalid connector id '{}': use [a-z0-9-], 1-48 characters",
                self.id
            )));
        }
        if self.source == Source::Api && self.id.starts_with("file-") {
            return Err(ConnectorError::Invalid(
                "connector ids starting with file- are reserved".into(),
            ));
        }
        match self.kind {
            Kind::Pam => {
                let s = self.pam()?;
                if s.allowed_groups.is_empty() {
                    return Err(ConnectorError::Invalid(
                        "a pam connector needs allowed_groups".into(),
                    ));
                }
            }
            Kind::Oidc => {
                let s = self.oidc()?;
                if !s.issuer.starts_with("https://")
                    && !s.issuer.starts_with("http://127.0.0.1")
                    && !s.issuer.starts_with("http://localhost")
                {
                    return Err(ConnectorError::Invalid(
                        "the issuer must be an https:// URL".into(),
                    ));
                }
                if s.client_id.is_empty() {
                    return Err(ConnectorError::Invalid("client_id is required".into()));
                }
                if !s.scopes.iter().any(|x| x == "openid") {
                    return Err(ConnectorError::Invalid("scopes must include openid".into()));
                }
            }
        }
        if let Some(d) = self.token_max_days
            && (d == 0 || d > 365)
        {
            return Err(ConnectorError::Invalid("token_max_days is 1-365".into()));
        }
        // D22: who lands in the owner's or an operator's tenant is
        // decided by the IdP; admit nobody by default.
        if guarded && self.jit && self.required_groups.is_empty() {
            return Err(ConnectorError::Invalid(
                "new users of this connector join an owner or operator tenant: set required_groups, or turn jit off".into(),
            ));
        }
        Ok(())
    }
}

/// A verified login, whatever the connector (spec/tenancy.md §6).
#[derive(Debug, Clone)]
pub struct ExternalIdentity {
    pub connector_id: String,
    pub subject: String,
    pub display_name: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
    /// The IdP said the login used more than one factor.
    pub mfa: bool,
}

/// The connectors defined in `control-plane.json`.
pub fn file_connectors(cfg: &Config) -> Vec<Connector> {
    let mut out = Vec::new();
    let pam = &cfg.auth.pam;
    if pam.enabled {
        out.push(Connector {
            id: FILE_PAM.into(),
            directory_id: crate::tenants::LOCAL_DIRECTORY.into(),
            kind: Kind::Pam,
            display_name: "Local accounts".into(),
            enabled: true,
            jit: pam.jit,
            required_groups: Vec::new(),
            require_mfa: false,
            group_teams: pam
                .group_teams
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            default_tenant: None,
            token_max_days: None,
            settings: serde_json::json!({ "allowed_groups": pam.allowed_groups }),
            source: Source::File,
            created_at: 0,
            updated_at: 0,
        });
    }
    let o = &cfg.auth.oidc;
    if o.enabled && !o.issuer.is_empty() && !o.client_id.is_empty() {
        out.push(Connector {
            id: FILE_OIDC.into(),
            directory_id: crate::tenants::LOCAL_DIRECTORY.into(),
            kind: Kind::Oidc,
            display_name: "Single sign-on".into(),
            enabled: true,
            jit: o.jit,
            required_groups: o.required_groups.clone(),
            require_mfa: false,
            group_teams: o.group_teams.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            default_tenant: None,
            token_max_days: None,
            settings: serde_json::json!({
                "issuer": o.issuer, "client_id": o.client_id, "scopes": o.scopes, "groups_claim": o.groups_claim,
                "allowed_domains": o.allowed_domains, "redirect_uri": o.redirect_uri,
            }),
            source: Source::File,
            created_at: 0,
            updated_at: 0,
        });
    }
    out
}

/// Connectors of the directories this control plane hosts, and their
/// secrets.
pub struct ConnectorStore {
    db: Arc<Db>,
    file: Vec<Connector>,
    kek: RwLock<secrets::Kek>,
}

impl ConnectorStore {
    pub fn new(db: Arc<Db>, cfg: &Config) -> Result<Self, ConnectorError> {
        let kek = secrets::Kek::load(cfg.auth.secrets_key.as_deref())?;
        Ok(Self {
            db,
            file: file_connectors(cfg),
            kek: RwLock::new(kek),
        })
    }

    fn read_all<T: serde::de::DeserializeOwned>(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
    ) -> Result<Vec<(String, T)>, ConnectorError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            out.push((k.value().to_string(), serde_json::from_slice(v.value())?));
        }
        Ok(out)
    }

    fn get_raw<T: serde::de::DeserializeOwned>(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
        key: &str,
    ) -> Result<Option<T>, ConnectorError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(def) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(match table.get(key)? {
            Some(v) => Some(serde_json::from_slice(v.value())?),
            None => None,
        })
    }

    fn put_raw<T: Serialize>(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
        key: &str,
        v: &T,
    ) -> Result<(), ConnectorError> {
        let bytes = serde_json::to_vec(v)?;
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut t = txn.open_table(def)?;
            t.insert(key, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    fn remove_raw(
        &self,
        def: TableDefinition<'static, &'static str, &'static [u8]>,
        key: &str,
    ) -> Result<bool, ConnectorError> {
        let txn = self.db.begin(crate::store::Origin::Api)?;
        let removed = {
            let mut t = txn.open_table(def)?;
            t.remove(key)?.is_some()
        };
        txn.commit()?;
        Ok(removed)
    }

    // ---- connectors -----------------------------------------------------

    pub fn list(&self) -> Result<Vec<Connector>, ConnectorError> {
        let mut out = self.file.clone();
        let mut stored: Vec<Connector> = self
            .read_all::<Connector>(CONNECTORS)?
            .into_iter()
            .map(|(_, c)| c)
            .collect();
        stored.sort_by(|a, b| a.id.cmp(&b.id));
        out.extend(stored);
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<Option<Connector>, ConnectorError> {
        if let Some(c) = self.file.iter().find(|c| c.id == id) {
            return Ok(Some(c.clone()));
        }
        self.get_raw(CONNECTORS, id)
    }

    /// The first enabled connector of `kind` (the legacy `/auth/oidc/*`
    /// and `/auth/login` routes, which name none).
    pub fn first(&self, kind: Kind) -> Result<Option<Connector>, ConnectorError> {
        Ok(self
            .list()?
            .into_iter()
            .find(|c| c.kind == kind && c.enabled))
    }

    pub fn create(&self, c: &Connector) -> Result<(), ConnectorError> {
        if self.get(&c.id)?.is_some() {
            return Err(ConnectorError::AlreadyExists(c.id.clone()));
        }
        self.put_raw(CONNECTORS, &c.id, c)
    }

    pub fn update(&self, c: &Connector) -> Result<(), ConnectorError> {
        if self.file.iter().any(|f| f.id == c.id) {
            return Err(ConnectorError::FileDefined(c.id.clone()));
        }
        if self.get_raw::<Connector>(CONNECTORS, &c.id)?.is_none() {
            return Err(ConnectorError::NotFound(c.id.clone()));
        }
        self.put_raw(CONNECTORS, &c.id, c)
    }

    pub fn delete(&self, id: &str) -> Result<(), ConnectorError> {
        if self.file.iter().any(|f| f.id == id) {
            return Err(ConnectorError::FileDefined(id.into()));
        }
        if !self.remove_raw(CONNECTORS, id)? {
            return Err(ConnectorError::NotFound(id.into()));
        }
        // Secrets are wiped with it.
        for (k, _) in self.read_all::<secrets::Sealed>(SECRETS)? {
            if k.starts_with(&format!("{id}/")) {
                self.remove_raw(SECRETS, &k)?;
            }
        }
        Ok(())
    }

    // ---- secrets --------------------------------------------------------

    fn data_key(&self, directory: &str) -> Result<Zeroizing<Vec<u8>>, ConnectorError> {
        let kek = self.kek.read().unwrap();
        let wrapped = match self.get_raw::<secrets::WrappedKey>(KEYS, directory)? {
            Some(w) => w,
            None => {
                let w = secrets::new_data_key(&kek, directory);
                self.put_raw(KEYS, directory, &w)?;
                w
            }
        };
        Ok(secrets::unwrap_data_key(&kek, &wrapped)?)
    }

    pub fn set_secret(
        &self,
        c: &Connector,
        field: &str,
        value: &str,
    ) -> Result<(), ConnectorError> {
        if c.source == Source::File {
            return Err(ConnectorError::FileDefined(c.id.clone()));
        }
        if !c.kind.secret_fields().contains(&field) {
            return Err(ConnectorError::Invalid(format!(
                "a {} connector has no secret {field}; it has {}",
                c.kind.as_str(),
                c.kind.secret_fields().join(", ")
            )));
        }
        if value.is_empty() || value.len() > 8192 {
            return Err(ConnectorError::Invalid("a secret is 1-8192 bytes".into()));
        }
        let dk = self.data_key(&c.directory_id)?;
        let sealed = secrets::seal(
            &dk,
            &c.directory_id,
            &c.id,
            field,
            value,
            crate::tenancy::now(),
        );
        self.put_raw(SECRETS, &format!("{}/{field}", c.id), &sealed)
    }

    /// A secret in clear, for the connector code path only.
    pub fn secret(
        &self,
        c: &Connector,
        field: &str,
    ) -> Result<Option<Zeroizing<String>>, ConnectorError> {
        let Some(sealed) =
            self.get_raw::<secrets::Sealed>(SECRETS, &format!("{}/{field}", c.id))?
        else {
            return Ok(None);
        };
        let dk = self.data_key(&c.directory_id)?;
        Ok(Some(secrets::open(
            &dk,
            &c.directory_id,
            &c.id,
            field,
            &sealed,
        )?))
    }

    /// Which secrets are set, and when: all the API ever shows of them.
    pub fn secret_status(
        &self,
        c: &Connector,
    ) -> Result<BTreeMap<String, Option<u64>>, ConnectorError> {
        let mut out = BTreeMap::new();
        for f in c.kind.secret_fields() {
            let s = self.get_raw::<secrets::Sealed>(SECRETS, &format!("{}/{f}", c.id))?;
            out.insert(f.to_string(), s.map(|s| s.updated_at));
        }
        Ok(out)
    }

    // ---- the secrets key ------------------------------------------------

    /// The KEK, for the leader to send to a new server (never logged).
    pub fn kek_bytes(&self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(self.kek.read().unwrap().bytes().to_vec())
    }

    /// Take the cluster's KEK (a new server). The data keys in the
    /// replicated store are wrapped by it.
    pub fn install_kek(&self, bytes: &[u8]) -> Result<(), ConnectorError> {
        self.kek.write().unwrap().install(bytes)?;
        Ok(())
    }

    pub fn kek_id(&self) -> String {
        self.kek.read().unwrap().id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oidc(id: &str) -> Connector {
        Connector {
            id: id.into(),
            directory_id: "local".into(),
            kind: Kind::Oidc,
            display_name: "Acme".into(),
            enabled: true,
            jit: true,
            required_groups: vec![],
            require_mfa: false,
            group_teams: BTreeMap::new(),
            default_tenant: None,
            token_max_days: None,
            settings: serde_json::json!({ "issuer": "https://idp.example", "client_id": "glidex" }),
            source: Source::Api,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn validation() {
        assert!(oidc("acme").validate(false).is_ok());
        assert!(oidc("Acme").validate(false).is_err());
        assert!(oidc("file-x").validate(false).is_err(), "reserved");
        let mut c = oidc("acme");
        c.settings = serde_json::json!({ "issuer": "http://idp.example", "client_id": "glidex" });
        assert!(c.validate(false).is_err(), "https only");
        c.settings = serde_json::json!({ "issuer": "https://idp.example", "client_id": "glidex", "bogus": 1 });
        assert!(c.validate(false).is_err(), "unknown settings");
        // D22: an owner/operator tenant isn't open to anyone in the IdP.
        assert!(oidc("acme").validate(true).is_err());
        let mut c = oidc("acme");
        c.required_groups = vec!["ops".into()];
        assert!(c.validate(true).is_ok());
    }

    #[test]
    fn secrets_are_stored_sealed_and_wiped_with_the_connector() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Db::create(dir.path().join("c.db")).unwrap());
        let mut cfg = Config::default();
        cfg.auth.secrets_key = Some(dir.path().join("secrets.kek"));
        let s = ConnectorStore::new(db.clone(), &cfg).unwrap();
        let c = oidc("acme");
        s.create(&c).unwrap();
        assert!(
            s.set_secret(&c, "password", "x").is_err(),
            "not a field of oidc"
        );
        s.set_secret(&c, "client_secret", "very-secret-value")
            .unwrap();
        assert_eq!(
            s.secret(&c, "client_secret").unwrap().unwrap().as_str(),
            "very-secret-value"
        );
        assert!(s.secret_status(&c).unwrap()["client_secret"].is_some());
        // The database file never holds it in clear.
        drop(s);
        let raw = std::fs::read(dir.path().join("c.db")).unwrap();
        assert!(!raw.windows(17).any(|w| w == b"very-secret-value"));
        // A restart with the same key file reads it back; deleting wipes it.
        let s = ConnectorStore::new(db, &cfg).unwrap();
        assert_eq!(
            s.secret(&c, "client_secret").unwrap().unwrap().as_str(),
            "very-secret-value"
        );
        s.delete("acme").unwrap();
        assert!(s.secret(&c, "client_secret").unwrap().is_none());
    }
}
