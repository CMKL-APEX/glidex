//! What crosses between a cluster and a trusted directory, and the
//! records a cluster keeps of one (spec/19-tenancy.md D63–D73).

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a peer request asks for (an assertion's `purpose`, D63).
pub const REDEEM: &str = "redeem";
pub const END: &str = "end";
pub const FEED: &str = "feed";
/// A cluster reporting what it holds of a directory's tenants (D70).
pub const REPORT: &str = "report";
/// A cluster telling a directory that one of its sessions ended (D72).
pub const LOGOUT: &str = "logout";

/// The tables a feed carries rows of (D67), by name.
pub const FEED_TABLES: [&str; 7] = [
    "users",
    "teams",
    "tenants",
    "tenant_members",
    "projects",
    "policy_links",
    // API tokens that list the cluster, by their hash (§7.4).
    "api_tokens",
];

/// A cluster's paths, which a directory calls.
pub const REDEEM_PATH: &str = "/trust/v1/redeem";
pub const END_PATH: &str = "/trust/v1/end";

/// A directory's paths, which a cluster calls: `/d/<directory>` and these.
pub fn feed_path(directory: &str) -> String {
    format!("/d/{directory}/feed/v1")
}
pub fn report_path(directory: &str) -> String {
    format!("/d/{directory}/feed/v1/report")
}
pub fn end_path(directory: &str) -> String {
    format!("/d/{directory}/trust/v1/end")
}
pub fn logout_path(directory: &str) -> String {
    format!("/d/{directory}/trust/v1/logout")
}
pub fn token_path(directory: &str) -> String {
    format!("/d/{directory}/token")
}

/// One record of a feed: the table, its key there, its value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub table: String,
    pub key: String,
    pub value: Value,
}

/// A directory's filtered snapshot for one cluster (D67).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: String,
    pub jwks: Value,
    pub rows: Vec<Row>,
    /// The chains of this cluster's browser logins ended in the last
    /// 12 h: the cluster ends the sessions it made from them (D68, D69).
    #[serde(default)]
    pub ended_sids: Vec<String>,
}

/// A snapshot's revision: the SHA-256 of what it holds.
pub fn revision_of(jwks: &Value, rows: &[Row], ended_sids: &[String]) -> String {
    crate::sha256_hex(
        serde_json::to_string(&(jwks, rows, ended_sids))
            .unwrap_or_default()
            .as_bytes(),
    )
}

/// What the cluster last applied from a trusted directory (D68).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedState {
    pub revision: String,
    /// `(table, key)` of every row it wrote.
    pub rows: Vec<(String, String)>,
    pub applied_at: u64,
    /// When the directory last answered a feed request (new content or
    /// not), to the minute: the replica is that fresh (D13).
    #[serde(default)]
    pub checked_at: u64,
}

/// A browser login at a trusted directory under way (D69): what its
/// callback checks.
#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub trusted: String,
    pub nonce: String,
    pub verifier: String,
    pub return_to: String,
    /// The secret of the browser's cookie that started it.
    pub browser: String,
    pub created: u64,
}

/// How long a browser login may take.
pub const LOGIN_SECS: u64 = 600;

/// Logins under way, by `state`.
#[derive(Default)]
pub struct PendingLogins(std::sync::Mutex<std::collections::HashMap<String, PendingLogin>>);

impl PendingLogins {
    pub fn put(&self, state: String, p: PendingLogin) {
        let mut m = self.0.lock().unwrap();
        m.retain(|_, v| v.created + LOGIN_SECS > p.created);
        m.insert(state, p);
    }

    /// The login `state` names, once, while it is good.
    pub fn take(&self, state: &str, now: u64) -> Option<PendingLogin> {
        self.0
            .lock()
            .unwrap()
            .remove(state)
            .filter(|p| p.created + LOGIN_SECS > now)
    }
}

/// Chains of trusted directories whose cluster sessions ended here, for
/// those directories to end too (D72): `(trusted directory id, sid)`.
#[derive(Default)]
pub struct EndedChains {
    queue: std::sync::Mutex<Vec<(String, String)>>,
    wake: tokio::sync::Notify,
}

/// At most this many wait; older ones lapse at the directory anyway.
const ENDED_CHAINS_MAX: usize = 1000;

impl EndedChains {
    pub fn push(&self, directory: String, sid: String) {
        let mut q = self.queue.lock().unwrap();
        if q.len() < ENDED_CHAINS_MAX {
            q.push((directory, sid));
        }
        self.wake.notify_one();
    }

    pub fn take(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.queue.lock().unwrap())
    }

    /// Until something is pushed.
    pub async fn wait(&self) {
        self.wake.notified().await
    }
}

/// An offer token's prefix.
pub const OFFER_PREFIX: &str = "gxo_";
/// An offer's default and longest life, in days.
pub const OFFER_DAYS: u64 = 7;
pub const OFFER_MAX_DAYS: u64 = 30;

/// What an offer token carries (D64): enough for the directory to reach
/// the cluster, pin it, and redeem the offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferToken {
    pub cluster_id: String,
    pub cluster_name: String,
    /// The cluster's API, `https://host[:port]`.
    pub api: String,
    /// Its certificate's SHA-256, 64 hex digits.
    pub tls_sha256: String,
    /// The tenant's slug and its directory's issuer.
    pub tenant: String,
    pub directory: String,
    pub offer: String,
    pub secret: String,
    pub expires_at: u64,
}

impl OfferToken {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("serializable");
        format!(
            "{OFFER_PREFIX}{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
        )
    }

    pub fn decode(s: &str) -> Option<OfferToken> {
        let b = s.trim().strip_prefix(OFFER_PREFIX)?;
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(b)
            .ok()?;
        serde_json::from_slice(&json).ok()
    }
}

/// The issuer URL as offers and trust records compare it: lower-case
/// scheme and host, no trailing `/`.
pub fn normalize_issuer(s: &str) -> Option<String> {
    let u: http::Uri = s.trim().parse().ok()?;
    let a = u.authority()?;
    (u.scheme_str() == Some("https") && u.query().is_none()).then(|| {
        format!(
            "https://{}{}",
            a.as_str().to_ascii_lowercase(),
            u.path().trim_end_matches('/')
        )
    })
}

/// A tenant of another directory, as a cluster that admits it knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTenant {
    pub id: String,
    pub slug: String,
    #[serde(default)]
    pub display_name: String,
}

/// A directory of another organization this cluster trusts (D65):
/// cluster scope, keyed by its issuer's id (`id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDirectory {
    /// `trusted-<first 16 hex of the issuer's SHA-256>`: the admissions'
    /// `directory_id`, never one of this host's own directory ids.
    pub id: String,
    pub issuer: String,
    /// Its id at its own host.
    pub directory: String,
    /// Its API for this cluster's calls (`/d/<directory>/trust/v1/…`),
    /// and that API's certificate SHA-256.
    pub api: String,
    pub tls_sha256: String,
    /// Its signing keys as the handshake brought them (a JWKS).
    pub jwks: Value,
    pub tenants: Vec<RemoteTenant>,
    pub created_at: u64,
    /// How long its credentials stay good after its last feed (D13);
    /// `None`: the cluster's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_staleness_minutes: Option<u64>,
}

/// The id a trusted directory's records take here.
pub fn trusted_id(issuer: &str) -> String {
    let h = crate::sha256_hex(issuer.as_bytes());
    format!("trusted-{}", &h[..16])
}

/// `POST /trust/v1/redeem`'s body, from the directory (D65).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedeemRequest {
    pub offer: String,
    pub secret: String,
    pub issuer: String,
    pub directory: String,
    pub tenant: RemoteTenant,
    pub jwks: Value,
    pub api: String,
    pub tls_sha256: String,
}

/// The cluster's answer to a redeem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedeemAnswer {
    pub cluster_id: String,
    /// The cluster's key (a JWKS).
    pub cluster_key: Value,
    /// Where the directory sends the cluster's browser logins back (D69).
    #[serde(default)]
    pub ui_callback: Option<String>,
}

/// `POST /trust/v1/end`'s body, at the cluster (D66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndAtCluster {
    pub issuer: String,
    pub tenant: String,
}

/// `POST /d/<dir>/trust/v1/end`'s body, at the directory (D66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndAtDirectory {
    pub cluster_id: String,
    pub tenant: String,
}

/// `POST /d/<dir>/feed/v1`'s body (D67).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedAsk {
    pub cluster_id: String,
    #[serde(default)]
    pub revision: String,
}

/// `POST /d/<dir>/feed/v1/report`'s body (D70, D73): `R` is the report,
/// one entry per admitted tenant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportBody<R> {
    pub cluster_id: String,
    pub report: R,
    #[serde(default)]
    pub ui_callback: Option<String>,
}

/// `POST /d/<dir>/trust/v1/logout`'s body (D72).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogoutBody {
    pub cluster_id: String,
    pub sid: String,
}

/// A web UI callback a cluster may register: an `http(s)` URL.
pub fn valid_callback(u: &str) -> bool {
    url::Url::parse(u).is_ok_and(|u| matches!(u.scheme(), "https" | "http"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_tokens_round_trip_and_refuse_junk() {
        let t = OfferToken {
            cluster_id: "c1".into(),
            cluster_name: "north".into(),
            api: "https://north.example:8841".into(),
            tls_sha256: "ab".repeat(32),
            tenant: "acme".into(),
            directory: "https://id.acme.example/d/acme".into(),
            offer: "o1".into(),
            secret: "s".into(),
            expires_at: 5,
        };
        let s = t.encode();
        assert!(s.starts_with(OFFER_PREFIX));
        assert_eq!(OfferToken::decode(&s), Some(t));
        assert_eq!(OfferToken::decode("gxo_!!"), None);
        assert_eq!(OfferToken::decode("gxt_abc"), None);
        assert_eq!(
            normalize_issuer("https://ID.Acme.example/d/acme/").as_deref(),
            Some("https://id.acme.example/d/acme")
        );
        assert_eq!(normalize_issuer("http://id.acme.example/d/acme"), None);
    }

    #[test]
    fn trusted_ids_and_revisions_are_stable() {
        // Records already stored under these ids keep them.
        let id = trusted_id("https://id.acme.example/d/acme");
        assert!(id.starts_with("trusted-") && id.len() == 24);
        assert_eq!(id, trusted_id("https://id.acme.example/d/acme"));
        let rows = vec![Row {
            table: "users".into(),
            key: "u1".into(),
            value: serde_json::json!({ "id": "u1" }),
        }];
        let r = revision_of(&Value::Null, &rows, &[]);
        assert_eq!(r, revision_of(&Value::Null, &rows, &[]));
        assert_ne!(r, revision_of(&Value::Null, &rows, &["s".into()]));
    }
}
