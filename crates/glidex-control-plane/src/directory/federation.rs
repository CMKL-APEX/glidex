//! Workload federations (spec/tenancy.md §9, D48–D51): a tenant trusts a
//! JWT issuer (GitHub Actions, GitLab CI, Kubernetes, …) and maps tokens
//! whose claims match a rule to one of its service accounts. This module
//! holds the records, their checks, and the issuer's keys; the exchange
//! is in `directory::exchange`.

use crate::store::Db;
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FEDERATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("federations");

pub const MAX_PER_TENANT: usize = 20;
pub const MAX_RULES: usize = 50;
pub const DEFAULT_TTL_SECS: u32 = 900;
pub const MAX_TTL_SECS: u32 = 3600;
const MIN_TTL_SECS: u32 = 60;
/// At least one of these is pinned by every rule (D50).
pub const PINNED: [&str; 3] = ["repository", "project_path", "sub"];

#[derive(Debug, thiserror::Error)]
pub enum FederationError {
    #[error("no such federation")]
    NotFound,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("federation storage error: {0}")]
    Storage(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for FederationError {
            fn from(e: $t) -> Self {
                FederationError::Storage(e.to_string())
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FederationKind {
    #[default]
    Jwt,
}

/// One mapping (D50): claims that must hold, exactly, and the service
/// account a matching token acts as.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(rename = "match")]
    pub claims: BTreeMap<String, String>,
    /// A service account's token id.
    pub service_account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<u32>,
}

impl Rule {
    pub fn ttl_secs(&self) -> u32 {
        self.ttl_secs.unwrap_or(DEFAULT_TTL_SECS)
    }

    /// Whether `claims` (a token's) has every value this rule asks for.
    pub fn matches(&self, claims: &serde_json::Value) -> bool {
        self.claims
            .iter()
            .all(|(k, v)| claims.get(k).and_then(|x| x.as_str()) == Some(v.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Federation {
    pub id: String,
    pub tenant_id: String,
    #[serde(default)]
    pub kind: FederationKind,
    #[serde(default)]
    pub display_name: String,
    pub issuer: String,
    pub audience: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub rules: Vec<Rule>,
    pub created_by: String,
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

fn yes() -> bool {
    true
}

/// A test mock's issuer: plain HTTP on the loopback only.
pub fn is_loopback(url: &str) -> bool {
    url.starts_with("http://127.0.0.1:") || url.starts_with("http://localhost:")
}

impl Federation {
    /// The record's own checks (D49, D50); the service accounts are the
    /// caller's to check (they need the tenant's projects).
    pub fn validate(&self) -> Result<(), FederationError> {
        let bad = |m: &str| Err(FederationError::Invalid(m.into()));
        let id_ok = !self.id.is_empty()
            && self.id.len() <= 48
            && self
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !id_ok {
            return bad("a federation's id is [a-z0-9-], 1-48 characters");
        }
        if self.display_name.chars().count() > 128 {
            return bad("display_name is at most 128 characters");
        }
        let https = self.issuer.starts_with("https://") && self.issuer.len() > "https://".len();
        if !(https || is_loopback(&self.issuer)) || self.issuer.chars().any(char::is_whitespace) {
            return bad("issuer is an https URL (the issuer's own, as its tokens carry it)");
        }
        if self.audience.trim().is_empty() || self.audience.len() > 256 {
            return bad(
                "audience is required (what the workload asks its issuer for), at most 256 characters",
            );
        }
        if self.rules.len() > MAX_RULES {
            return Err(FederationError::Invalid(format!(
                "at most {MAX_RULES} rules"
            )));
        }
        for (i, r) in self.rules.iter().enumerate() {
            let n = i + 1;
            if r.claims.is_empty() || !r.claims.keys().any(|k| PINNED.contains(&k.as_str())) {
                return Err(FederationError::Invalid(format!(
                    "rule {n} must name one of {} (D50)",
                    PINNED.join(", ")
                )));
            }
            for (k, v) in &r.claims {
                let key_ok = !k.is_empty()
                    && k.len() <= 64
                    && k.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "_-.:".contains(c));
                if !key_ok || v.is_empty() || v.len() > 256 {
                    return Err(FederationError::Invalid(format!(
                        "rule {n}: claim {k:?} needs a name and a value (exact, no wildcards)"
                    )));
                }
                if v.contains('*') {
                    return Err(FederationError::Invalid(format!(
                        "rule {n}: {k} is matched exactly; * isn't a wildcard here (D50)"
                    )));
                }
            }
            if r.service_account.is_empty() {
                return Err(FederationError::Invalid(format!(
                    "rule {n} names no service account"
                )));
            }
            if r.ttl_secs
                .is_some_and(|t| !(MIN_TTL_SECS..=MAX_TTL_SECS).contains(&t))
            {
                return Err(FederationError::Invalid(format!(
                    "rule {n}: ttl_secs is {MIN_TTL_SECS}-{MAX_TTL_SECS}"
                )));
            }
        }
        Ok(())
    }
}

fn key(tenant: &str, id: &str) -> String {
    format!("{tenant}/{id}")
}

/// The directory's federations.
pub struct FederationStore {
    db: Arc<Db>,
}

impl FederationStore {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    fn all(&self) -> Result<Vec<Federation>, FederationError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(FEDERATIONS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for e in table.iter()? {
            let (_, v) = e?;
            out.push(serde_json::from_slice(v.value())?);
        }
        Ok(out)
    }

    pub fn list(&self, tenant: &str) -> Result<Vec<Federation>, FederationError> {
        let mut v: Vec<Federation> = self
            .all()?
            .into_iter()
            .filter(|f| f.tenant_id == tenant)
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(v)
    }

    /// Every federation one of whose rules names `service_account`.
    pub fn naming(&self, service_account: &str) -> Result<Vec<Federation>, FederationError> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|f| f.rules.iter().any(|r| r.service_account == service_account))
            .collect())
    }

    pub fn get(&self, tenant: &str, id: &str) -> Result<Option<Federation>, FederationError> {
        Ok(self
            .all()?
            .into_iter()
            .find(|f| f.tenant_id == tenant && f.id == id))
    }

    fn put(&self, f: &Federation) -> Result<(), FederationError> {
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut t = txn.open_table(FEDERATIONS)?;
            t.insert(
                key(&f.tenant_id, &f.id).as_str(),
                serde_json::to_vec(f)?.as_slice(),
            )?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn create(&self, f: &Federation) -> Result<(), FederationError> {
        f.validate()?;
        let mine = self.list(&f.tenant_id)?;
        if mine.iter().any(|x| x.id == f.id) {
            return Err(FederationError::Conflict(format!(
                "federation {} exists",
                f.id
            )));
        }
        if mine.len() >= MAX_PER_TENANT {
            return Err(FederationError::Invalid(format!(
                "a tenant has at most {MAX_PER_TENANT} federations"
            )));
        }
        self.put(f)
    }

    pub fn update(&self, f: &Federation) -> Result<(), FederationError> {
        f.validate()?;
        if self.get(&f.tenant_id, &f.id)?.is_none() {
            return Err(FederationError::NotFound);
        }
        self.put(f)
    }

    pub fn delete(&self, tenant: &str, id: &str) -> Result<Federation, FederationError> {
        let f = self.get(tenant, id)?.ok_or(FederationError::NotFound)?;
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut t = txn.open_table(FEDERATIONS)?;
            t.remove(key(tenant, id).as_str())?;
        }
        txn.commit()?;
        Ok(f)
    }
}

// ---- the issuer's keys --------------------------------------------------

/// An issuer's signing keys, read through its discovery document (D49).
#[derive(Clone)]
pub struct IssuerKeys {
    pub jwks: jsonwebtoken::jwk::JwkSet,
    fetched_at: u64,
}

/// Keys are read again after this long, or when a token names a key id
/// they don't have (at most once a minute).
const KEYS_TTL_SECS: u64 = 3600;
const REFETCH_SECS: u64 = 60;

/// Discovery and JWKS of federation issuers, cached per issuer.
pub struct Issuers {
    http: reqwest::Client,
    cache: Mutex<HashMap<String, IssuerKeys>>,
}

impl Default for Issuers {
    fn default() -> Self {
        Issuers {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl Issuers {
    async fn get_json(&self, url: &str) -> Result<serde_json::Value, String> {
        let r = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| format!("can't reach {url}: {}", e.without_url()))?;
        if !r.status().is_success() {
            return Err(format!("{url} answered {}", r.status().as_u16()));
        }
        let body = r.bytes().await.map_err(|e| e.without_url().to_string())?;
        if body.len() > 1 << 20 {
            return Err(format!("{url} answered more than 1 MiB"));
        }
        serde_json::from_slice(&body).map_err(|_| format!("{url} didn't answer JSON"))
    }

    /// Read `issuer`'s discovery and keys now.
    pub async fn fetch(&self, issuer: &str) -> Result<IssuerKeys, String> {
        let base = issuer.trim_end_matches('/');
        let d = self
            .get_json(&format!("{base}/.well-known/openid-configuration"))
            .await?;
        let said = d.get("issuer").and_then(|v| v.as_str()).unwrap_or("");
        if said.trim_end_matches('/') != base {
            return Err(format!(
                "its discovery document names issuer {said:?}, not {issuer:?}"
            ));
        }
        let jwks_uri = d
            .get("jwks_uri")
            .and_then(|v| v.as_str())
            .ok_or("its discovery document has no jwks_uri")?;
        if !(jwks_uri.starts_with("https://") || is_loopback(jwks_uri)) {
            return Err("its jwks_uri isn't https".into());
        }
        let jwks: jsonwebtoken::jwk::JwkSet =
            serde_json::from_value(self.get_json(jwks_uri).await?)
                .map_err(|_| "its JWKS can't be read".to_string())?;
        if jwks.keys.is_empty() {
            return Err("its JWKS has no keys".into());
        }
        let k = IssuerKeys {
            jwks,
            fetched_at: crate::tenancy::now(),
        };
        self.cache
            .lock()
            .unwrap()
            .insert(issuer.to_string(), k.clone());
        Ok(k)
    }

    /// `issuer`'s keys, from the cache when fresh; read again when `kid`
    /// isn't among them (an issuer rotating its keys).
    pub async fn keys(&self, issuer: &str, kid: Option<&str>) -> Result<IssuerKeys, String> {
        let now = crate::tenancy::now();
        let cached = self.cache.lock().unwrap().get(issuer).cloned();
        if let Some(k) = cached {
            let fresh = now < k.fetched_at + KEYS_TTL_SECS;
            let has = kid.is_none_or(|kid| k.jwks.find(kid).is_some());
            if fresh && (has || now < k.fetched_at + REFETCH_SECS) {
                return Ok(k);
            }
        }
        self.fetch(issuer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed() -> Federation {
        Federation {
            id: "gh".into(),
            tenant_id: "t1".into(),
            kind: FederationKind::Jwt,
            display_name: "GitHub".into(),
            issuer: "https://token.actions.githubusercontent.com".into(),
            audience: "glidex".into(),
            enabled: true,
            rules: vec![Rule {
                claims: [
                    ("repository".to_string(), "acme/infra".to_string()),
                    ("ref".to_string(), "refs/heads/main".to_string()),
                ]
                .into(),
                service_account: "tok1".into(),
                ttl_secs: None,
            }],
            created_by: "u".into(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn rules_are_exact_and_pinned() {
        assert!(fed().validate().is_ok());
        let r = &fed().rules[0];
        let claims =
            serde_json::json!({"repository": "acme/infra", "ref": "refs/heads/main", "sub": "x"});
        assert!(r.matches(&claims));
        assert!(!r.matches(
            &serde_json::json!({"repository": "acme/infra-evil", "ref": "refs/heads/main"})
        ));
        assert!(
            !r.matches(&serde_json::json!({"repository": "acme/infra"})),
            "every claim must hold"
        );
        assert!(
            !r.matches(
                &serde_json::json!({"repository": ["acme/infra"], "ref": "refs/heads/main"})
            )
        );
        let mut f = fed();
        f.rules[0].claims = [("ref".to_string(), "refs/heads/main".to_string())].into();
        assert!(f.validate().is_err(), "no pinned claim");
        let mut f = fed();
        f.rules[0]
            .claims
            .insert("repository".into(), "acme/*".into());
        assert!(f.validate().is_err(), "no wildcards");
        let mut f = fed();
        f.rules[0].ttl_secs = Some(7200);
        assert!(f.validate().is_err());
        let mut f = fed();
        f.issuer = "http://ci.example".into();
        assert!(f.validate().is_err(), "https only");
        f.issuer = "http://127.0.0.1:8080".into();
        assert!(f.validate().is_ok(), "a loopback mock");
        let mut f = fed();
        f.audience = " ".into();
        assert!(f.validate().is_err());
        let mut f = fed();
        f.id = "Bad Id".into();
        assert!(f.validate().is_err());
    }
}
