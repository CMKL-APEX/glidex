//! A directory as an OpenID provider (spec/tenancy.md D54–D57): what its
//! endpoints keep between requests. The endpoints are `api::op`.
//!
//! - **Authorization codes** (code flow with PKCE, RFC 7636) and **device
//!   grants** (RFC 8628) live in memory for minutes, single use.
//! - **Refresh tokens** (D55) are stored by their SHA-256, in chains
//!   (`family`): each use replaces the token with a new one of the chain,
//!   and presenting a replaced one again ends the whole chain. A chain
//!   that ends is remembered 12 h (`ended_sids`), for the feeds of the
//!   clusters that made sessions from it (D67, D69).

use crate::auth::{random_token, sha256_hex};
use redb::{ReadableTable as _, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const REFRESH_TOKENS: TableDefinition<&str, &[u8]> = TableDefinition::new("refresh_tokens");
const ENDED_SIDS: TableDefinition<&str, &[u8]> = TableDefinition::new("ended_sids");

/// The built-in public client (D54).
pub const GXCTL: &str = "gxctl";
/// A refresh token's prefix, so leaked ones are recognised.
pub const REFRESH_PREFIX: &str = "gxr_";
/// How long an authorization code may wait for its token request.
pub const CODE_SECS: u64 = 300;
/// How long a device grant may wait for its approval, and how often its
/// client may ask.
pub const DEVICE_SECS: u64 = 600;
pub const DEVICE_INTERVAL: u64 = 5;
/// A chain's lifetime unused, and in all (D55).
pub const REFRESH_IDLE_SECS: u64 = 30 * 86_400;
pub const REFRESH_MAX_SECS: u64 = 90 * 86_400;
/// What a cluster is named as in `resource` (D57).
pub const CLUSTER_URN: &str = "urn:glidex:cluster:";
/// A trusted cluster's client id: `cluster:<id>` (D69).
pub const CLUSTER_CLIENT: &str = "cluster:";
/// How long an ended chain is remembered for feeds.
pub const ENDED_KEEP_SECS: u64 = 12 * 3600;

/// A loopback redirect for a native app (RFC 8252 §7.3): `http` to
/// `127.0.0.1` or `[::1]` with a port, no fragment.
pub fn loopback_redirect(u: &str) -> bool {
    let Ok(uri) = u.parse::<axum::http::Uri>() else {
        return false;
    };
    uri.scheme_str() == Some("http")
        && matches!(uri.host(), Some("127.0.0.1" | "[::1]"))
        && uri.port_u16().is_some()
        && !u.contains('#')
}

/// The PKCE check (S256 only).
pub fn pkce_ok(challenge: &str, verifier: &str) -> bool {
    use base64::Engine as _;
    use sha2::Digest as _;
    let valid = |s: &str| {
        (43..=128).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
    };
    if !valid(verifier) || !valid(challenge) {
        return false;
    }
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    let got = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    crate::auth::constant_eq(&got, challenge)
}

/// Who approved a login, and how they had logged in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub user_id: String,
    /// When they last logged in (`auth_time`).
    pub auth_time: u64,
    /// `amr` (RFC 8176): `pwd`, `otp`, `hwk`, `mfa`, …
    pub amr: Vec<String>,
    pub mfa: bool,
    /// The browser session at the directory that approved it, when its
    /// chain ends with that session (a cluster's browser login, D69).
    pub session: Option<String>,
}

/// What an authorization request asked for.
#[derive(Debug, Clone)]
pub struct CodeRequest {
    pub directory: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub scope: String,
    pub nonce: Option<String>,
}

struct PendingCode {
    req: CodeRequest,
    grant: Grant,
    expires: u64,
}

#[derive(Debug, Clone)]
pub struct DeviceRequest {
    pub directory: String,
    pub client_id: String,
    pub scope: String,
}

struct PendingDevice {
    req: DeviceRequest,
    user_code: String,
    expires: u64,
    approved: Option<Grant>,
    denied: bool,
    last_poll: u64,
}

/// A device grant's state when its client asks (RFC 8628 §3.5).
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    Approved(Grant, DeviceRequest),
    Pending,
    SlowDown,
    Denied,
    Expired,
}

impl PartialEq for DeviceRequest {
    fn eq(&self, o: &Self) -> bool {
        (&self.directory, &self.client_id, &self.scope) == (&o.directory, &o.client_id, &o.scope)
    }
}
impl Eq for DeviceRequest {}

/// Codes and device grants in progress, by the SHA-256 of their secrets.
#[derive(Default)]
pub struct Pending {
    codes: Mutex<HashMap<String, PendingCode>>,
    devices: Mutex<HashMap<String, PendingDevice>>,
}

/// The device grant's user code: 8 letters a person reads and types
/// (RFC 8628 §6.1), without vowels or look-alikes.
fn user_code() -> String {
    const LETTERS: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
    let b = crate::auth::random_bytes(8);
    let s: String = b
        .iter()
        .map(|x| LETTERS[*x as usize % LETTERS.len()] as char)
        .collect();
    format!("{}-{}", &s[..4], &s[4..])
}

/// A user code as typed: upper case, the dash optional.
pub fn normalize_user_code(s: &str) -> String {
    let c: String = s
        .chars()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if c.len() == 8 {
        format!("{}-{}", &c[..4], &c[4..])
    } else {
        c
    }
}

impl Pending {
    fn prune(&self, now: u64) {
        self.codes.lock().unwrap().retain(|_, c| c.expires > now);
        self.devices.lock().unwrap().retain(|_, d| d.expires > now);
    }

    /// A one-time code for `req`, approved by `grant`.
    pub fn issue_code(&self, req: CodeRequest, grant: Grant, now: u64) -> String {
        self.prune(now);
        let code = random_token(32);
        self.codes.lock().unwrap().insert(
            sha256_hex(code.as_bytes()),
            PendingCode {
                req,
                grant,
                expires: now + CODE_SECS,
            },
        );
        code
    }

    /// The code's request and grant, once.
    pub fn take_code(&self, code: &str, now: u64) -> Option<(CodeRequest, Grant)> {
        let c = self
            .codes
            .lock()
            .unwrap()
            .remove(&sha256_hex(code.as_bytes()))?;
        (c.expires > now).then_some((c.req, c.grant))
    }

    /// A device grant: `(device_code, user_code)`.
    pub fn start_device(&self, req: DeviceRequest, now: u64) -> (String, String) {
        self.prune(now);
        let device_code = random_token(32);
        let mut devices = self.devices.lock().unwrap();
        let uc = loop {
            let c = user_code();
            if !devices.values().any(|d| d.user_code == c) {
                break c;
            }
        };
        devices.insert(
            sha256_hex(device_code.as_bytes()),
            PendingDevice {
                req,
                user_code: uc.clone(),
                expires: now + DEVICE_SECS,
                approved: None,
                denied: false,
                last_poll: 0,
            },
        );
        (device_code, uc)
    }

    /// The request a user code is for, while it waits for an answer.
    pub fn device_request(&self, user_code: &str, now: u64) -> Option<DeviceRequest> {
        let uc = normalize_user_code(user_code);
        self.devices
            .lock()
            .unwrap()
            .values()
            .find(|d| d.user_code == uc && d.expires > now && d.approved.is_none() && !d.denied)
            .map(|d| d.req.clone())
    }

    /// Answer a device grant (`grant`: approved by it; `None`: denied).
    /// `false` when no such grant waits.
    pub fn answer_device(&self, user_code: &str, grant: Option<Grant>, now: u64) -> bool {
        let uc = normalize_user_code(user_code);
        let mut devices = self.devices.lock().unwrap();
        let Some(d) = devices
            .values_mut()
            .find(|d| d.user_code == uc && d.expires > now && d.approved.is_none() && !d.denied)
        else {
            return false;
        };
        match grant {
            Some(g) => d.approved = Some(g),
            None => d.denied = true,
        }
        true
    }

    /// The client asking about its device grant.
    pub fn poll(&self, device_code: &str, client_id: &str, now: u64) -> Poll {
        let key = sha256_hex(device_code.as_bytes());
        let mut devices = self.devices.lock().unwrap();
        let Some(d) = devices.get_mut(&key) else {
            return Poll::Expired;
        };
        if d.req.client_id != client_id || d.expires <= now {
            devices.remove(&key);
            return Poll::Expired;
        }
        if d.denied {
            devices.remove(&key);
            return Poll::Denied;
        }
        if let Some(g) = d.approved.clone() {
            let req = devices.remove(&key).map(|d| d.req).expect("present");
            return Poll::Approved(g, req);
        }
        let early = now < d.last_poll + DEVICE_INTERVAL;
        d.last_poll = now;
        if early { Poll::SlowDown } else { Poll::Pending }
    }
}

/// One refresh token as stored, by its SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshToken {
    /// The chain it belongs to (also the access tokens' `sid`).
    pub family: String,
    pub directory_id: String,
    pub client_id: String,
    pub user_id: String,
    pub scope: String,
    #[serde(flatten)]
    pub grant_info: GrantInfo,
    /// When the chain started; it ends `REFRESH_MAX_SECS` later at most.
    pub family_created_at: u64,
    pub issued_at: u64,
    /// Replaced by a newer token of the chain: presenting it again ends
    /// the chain (D55).
    #[serde(default)]
    pub replaced: bool,
    /// When the chain ends: the sooner of 90 days and `token_max_days`.
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantInfo {
    pub auth_time: u64,
    #[serde(default)]
    pub amr: Vec<String>,
    #[serde(default)]
    pub mfa: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

/// A chain that ended, kept `ENDED_KEEP_SECS`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndedSid {
    pub directory_id: String,
    pub client_id: String,
    pub ended_at: u64,
}

impl RefreshToken {
    pub fn grant(&self) -> Grant {
        Grant {
            user_id: self.user_id.clone(),
            auth_time: self.grant_info.auth_time,
            amr: self.grant_info.amr.clone(),
            mfa: self.grant_info.mfa,
            session: self.grant_info.session.clone(),
        }
    }
}

/// Why a refresh token can't be used.
#[derive(Debug, PartialEq, Eq)]
pub enum RefreshRefused {
    Unknown,
    Expired,
    /// A replaced token came back: the chain was ended (D55).
    Reused,
    WrongClient,
}

#[derive(Debug, thiserror::Error)]
#[error("refresh tokens: {0}")]
pub struct StoreError(String);

macro_rules! store_err {
    ($($t:ty),*) => {$(
        impl From<$t> for StoreError {
            fn from(e: $t) -> Self { StoreError(e.to_string()) }
        }
    )*};
}
store_err!(
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    crate::store::StoreError,
    serde_json::Error
);

/// The directory's refresh tokens.
pub struct RefreshTokens {
    db: Arc<crate::store::Db>,
}

impl RefreshTokens {
    pub fn new(db: Arc<crate::store::Db>) -> Self {
        RefreshTokens { db }
    }

    fn all(&self) -> Result<Vec<(String, RefreshToken)>, StoreError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(REFRESH_TOKENS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for e in table.iter()? {
            let (k, v) = e?;
            out.push((k.value().to_string(), serde_json::from_slice(v.value())?));
        }
        Ok(out)
    }

    fn get(&self, hash: &str) -> Result<Option<RefreshToken>, StoreError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(REFRESH_TOKENS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        Ok(match table.get(hash)? {
            Some(v) => Some(serde_json::from_slice(v.value())?),
            None => None,
        })
    }

    /// Write `put` and remove `remove`, in one transaction.
    fn write(&self, put: &[(&str, &RefreshToken)], remove: &[String]) -> Result<(), StoreError> {
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut t = txn.open_table(REFRESH_TOKENS)?;
            for (k, v) in put {
                t.insert(k, serde_json::to_vec(v)?.as_slice())?;
            }
            for k in remove {
                t.remove(k.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// A new chain for `grant`; its first token's secret.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        directory: &str,
        client_id: &str,
        scope: &str,
        grant: &Grant,
        max_secs: u64,
        now: u64,
    ) -> Result<(String, RefreshToken), StoreError> {
        let secret = format!("{REFRESH_PREFIX}{}", random_token(32));
        let r = RefreshToken {
            family: random_token(16),
            directory_id: directory.into(),
            client_id: client_id.into(),
            user_id: grant.user_id.clone(),
            scope: scope.into(),
            grant_info: GrantInfo {
                auth_time: grant.auth_time,
                amr: grant.amr.clone(),
                mfa: grant.mfa,
                session: grant.session.clone(),
            },
            family_created_at: now,
            issued_at: now,
            replaced: false,
            expires_at: now + max_secs.min(REFRESH_MAX_SECS),
        };
        self.write(&[(&sha256_hex(secret.as_bytes()), &r)], &[])?;
        Ok((secret, r))
    }

    /// Use `secret`: its record, and the token that replaces it.
    pub fn rotate(
        &self,
        secret: &str,
        client_id: &str,
        now: u64,
    ) -> Result<Result<(String, RefreshToken), RefreshRefused>, StoreError> {
        let hash = sha256_hex(secret.as_bytes());
        let Some(mut old) = self.get(&hash)? else {
            return Ok(Err(RefreshRefused::Unknown));
        };
        if old.replaced {
            self.end_family(&old.family)?;
            return Ok(Err(RefreshRefused::Reused));
        }
        if old.client_id != client_id {
            return Ok(Err(RefreshRefused::WrongClient));
        }
        if now >= old.expires_at || now >= old.issued_at + REFRESH_IDLE_SECS {
            self.end_family(&old.family)?;
            return Ok(Err(RefreshRefused::Expired));
        }
        let secret = format!("{REFRESH_PREFIX}{}", random_token(32));
        let new = RefreshToken {
            issued_at: now,
            replaced: false,
            ..old.clone()
        };
        old.replaced = true;
        // The replaced token is kept only to recognise its reuse; older
        // replaced tokens of the chain go.
        let stale: Vec<String> = self
            .all()?
            .into_iter()
            .filter(|(_, r)| r.family == old.family && r.replaced)
            .map(|(k, _)| k)
            .collect();
        self.write(
            &[(&hash, &old), (&sha256_hex(secret.as_bytes()), &new)],
            &stale,
        )?;
        Ok(Ok((secret, new)))
    }

    /// The chain is still alive (its tokens, and the access tokens naming
    /// it as `sid`, are good).
    pub fn family_alive(&self, family: &str, now: u64) -> Result<bool, StoreError> {
        Ok(self
            .all()?
            .iter()
            .any(|(_, r)| r.family == family && !r.replaced && now < r.expires_at))
    }

    /// End the chain `secret` belongs to (`revoke`); `false` when unknown.
    pub fn revoke(&self, secret: &str) -> Result<bool, StoreError> {
        match self.get(&sha256_hex(secret.as_bytes()))? {
            Some(r) => {
                self.end_family(&r.family)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn end_family(&self, family: &str) -> Result<(), StoreError> {
        self.end_where(|r| r.family == family).map(|_| ())
    }

    /// End every chain of `user` (disable, logout everywhere).
    pub fn end_user(&self, user: &str) -> Result<usize, StoreError> {
        self.end_where(|r| r.user_id == user)
    }

    /// End chain `family` of `client_id` at directory `dir` (a cluster's
    /// session from it ended, D72); the number of tokens.
    pub fn end_client_family(
        &self,
        dir: &str,
        client_id: &str,
        family: &str,
    ) -> Result<usize, StoreError> {
        self.end_where(|r| r.directory_id == dir && r.client_id == client_id && r.family == family)
    }

    /// End the chains browser session `hash` approved (its logout, D69).
    pub fn end_session(&self, hash: &str) -> Result<usize, StoreError> {
        self.end_where(|r| r.grant_info.session.as_deref() == Some(hash))
    }

    /// Remove the tokens `pick` chooses, and remember their chains as
    /// ended; the number of tokens.
    fn end_where(&self, pick: impl Fn(&RefreshToken) -> bool) -> Result<usize, StoreError> {
        let now = crate::auth::store::now();
        let gone: Vec<(String, RefreshToken)> =
            self.all()?.into_iter().filter(|(_, r)| pick(r)).collect();
        let txn = self.db.begin(crate::store::Origin::Api)?;
        {
            let mut t = txn.open_table(REFRESH_TOKENS)?;
            for (k, _) in &gone {
                t.remove(k.as_str())?;
            }
            let mut e = txn.open_table(ENDED_SIDS)?;
            let old: Vec<String> = e
                .iter()?
                .filter_map(|x| x.ok())
                .filter(|(_, v)| {
                    serde_json::from_slice::<EndedSid>(v.value())
                        .map_or(true, |x| x.ended_at + ENDED_KEEP_SECS < now)
                })
                .map(|(k, _)| k.value().to_string())
                .collect();
            for k in old {
                e.remove(k.as_str())?;
            }
            for (_, r) in &gone {
                let v = EndedSid {
                    directory_id: r.directory_id.clone(),
                    client_id: r.client_id.clone(),
                    ended_at: now,
                };
                e.insert(r.family.as_str(), serde_json::to_vec(&v)?.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(gone.len())
    }

    /// The chains of `client_id` at directory `dir` that ended since
    /// `since`, sorted.
    pub fn ended_since(
        &self,
        dir: &str,
        client_id: &str,
        since: u64,
    ) -> Result<Vec<String>, StoreError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(ENDED_SIDS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for e in table.iter()? {
            let (k, v) = e?;
            let x: EndedSid = serde_json::from_slice(v.value())?;
            if x.directory_id == dir && x.client_id == client_id && x.ended_at >= since {
                out.push(k.value().to_string());
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> Grant {
        Grant {
            user_id: "u1".into(),
            auth_time: 10,
            amr: vec!["pwd".into()],
            mfa: false,
            session: None,
        }
    }

    fn store() -> (RefreshTokens, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let db = Arc::new(crate::store::Db::create(d.path().join("t.db")).unwrap());
        (RefreshTokens::new(db), d)
    }

    #[test]
    fn redirects_and_pkce() {
        assert!(loopback_redirect("http://127.0.0.1:51004/callback"));
        assert!(loopback_redirect("http://[::1]:8080/"));
        for bad in [
            "http://localhost:8080/",
            "https://127.0.0.1:8080/",
            "http://127.0.0.1/",
            "http://10.0.0.1:80/",
            "http://127.0.0.1:80/#x",
        ] {
            assert!(!loopback_redirect(bad), "{bad}");
        }
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(pkce_ok(challenge, verifier));
        assert!(!pkce_ok(
            challenge,
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXX"
        ));
        assert!(!pkce_ok(challenge, "short"));
    }

    #[test]
    fn a_code_is_single_use_and_expires() {
        let p = Pending::default();
        let req = || CodeRequest {
            directory: "local".into(),
            client_id: GXCTL.into(),
            redirect_uri: "http://127.0.0.1:9/cb".into(),
            code_challenge: "c".into(),
            scope: "openid".into(),
            nonce: None,
        };
        let code = p.issue_code(req(), grant(), 100);
        assert!(p.take_code(&code, 101).is_some());
        assert!(p.take_code(&code, 101).is_none(), "once");
        let code = p.issue_code(req(), grant(), 100);
        assert!(p.take_code(&code, 100 + CODE_SECS).is_none(), "expired");
    }

    #[test]
    fn a_device_grant_waits_for_its_answer() {
        let p = Pending::default();
        let req = DeviceRequest {
            directory: "local".into(),
            client_id: GXCTL.into(),
            scope: "openid".into(),
        };
        let (dc, uc) = p.start_device(req.clone(), 100);
        assert_eq!(uc.len(), 9);
        assert_eq!(p.poll(&dc, GXCTL, 100), Poll::Pending);
        assert_eq!(p.poll(&dc, GXCTL, 101), Poll::SlowDown);
        assert_eq!(
            p.device_request(&uc.to_lowercase().replace('-', ""), 102),
            Some(req.clone())
        );
        assert!(p.answer_device(&uc, Some(grant()), 102));
        assert!(!p.answer_device(&uc, Some(grant()), 102), "answered once");
        assert_eq!(
            p.poll(&dc, GXCTL, 110),
            Poll::Approved(grant(), req.clone())
        );
        assert_eq!(p.poll(&dc, GXCTL, 111), Poll::Expired, "taken");
        let (dc, uc) = p.start_device(req.clone(), 200);
        assert!(p.answer_device(&uc, None, 201));
        assert_eq!(p.poll(&dc, GXCTL, 210), Poll::Denied);
        let (dc, _) = p.start_device(req, 300);
        assert_eq!(p.poll(&dc, "other", 301), Poll::Expired, "another client");
    }

    #[test]
    fn refresh_tokens_rotate_and_a_reused_one_ends_the_chain() {
        let (s, _d) = store();
        let (t1, r) = s
            .start("local", GXCTL, "openid", &grant(), REFRESH_MAX_SECS, 100)
            .unwrap();
        assert!(t1.starts_with(REFRESH_PREFIX));
        let (t2, r2) = s.rotate(&t1, GXCTL, 200).unwrap().unwrap();
        assert_eq!(r2.family, r.family);
        assert_eq!(
            s.rotate(&t2, "other", 201).unwrap(),
            Err(RefreshRefused::WrongClient)
        );
        let (t3, _) = s.rotate(&t2, GXCTL, 300).unwrap().unwrap();
        assert!(s.family_alive(&r.family, 300).unwrap());
        // t2 again: a copy is in someone else's hands; the chain ends.
        assert_eq!(
            s.rotate(&t2, GXCTL, 400).unwrap(),
            Err(RefreshRefused::Reused)
        );
        assert_eq!(
            s.rotate(&t3, GXCTL, 401).unwrap(),
            Err(RefreshRefused::Unknown)
        );
        assert!(!s.family_alive(&r.family, 401).unwrap());
    }

    #[test]
    fn chains_end_idle_at_their_limit_or_with_their_user() {
        let (s, _d) = store();
        let (t, _) = s
            .start("local", GXCTL, "openid", &grant(), REFRESH_MAX_SECS, 0)
            .unwrap();
        assert_eq!(
            s.rotate(&t, GXCTL, REFRESH_IDLE_SECS).unwrap(),
            Err(RefreshRefused::Expired)
        );
        let (t, _) = s
            .start("local", GXCTL, "openid", &grant(), 7 * 86_400, 0)
            .unwrap();
        assert_eq!(
            s.rotate(&t, GXCTL, 7 * 86_400).unwrap(),
            Err(RefreshRefused::Expired),
            "token_max_days"
        );
        let (t, r) = s
            .start("local", GXCTL, "openid", &grant(), REFRESH_MAX_SECS, 0)
            .unwrap();
        assert_eq!(s.end_user("u1").unwrap(), 1);
        assert!(!s.family_alive(&r.family, 1).unwrap());
        assert_eq!(
            s.rotate(&t, GXCTL, 1).unwrap(),
            Err(RefreshRefused::Unknown)
        );
        let (t, _) = s
            .start("local", GXCTL, "openid", &grant(), REFRESH_MAX_SECS, 0)
            .unwrap();
        assert!(s.revoke(&t).unwrap());
        assert!(!s.revoke(&t).unwrap());
    }

    #[test]
    fn a_sessions_chains_end_with_it_and_are_remembered() {
        let (s, _d) = store();
        let t = crate::auth::store::now();
        let browser = Grant {
            session: Some("h1".into()),
            ..grant()
        };
        let (_, a) = s
            .start(
                "local",
                "cluster:c1",
                "openid",
                &browser,
                REFRESH_MAX_SECS,
                t,
            )
            .unwrap();
        let (_, b) = s
            .start("local", GXCTL, "openid", &grant(), REFRESH_MAX_SECS, t)
            .unwrap();
        assert_eq!(s.end_session("h1").unwrap(), 1);
        assert!(!s.family_alive(&a.family, t).unwrap());
        assert!(
            s.family_alive(&b.family, t).unwrap(),
            "gxctl's isn't the session's"
        );
        assert_eq!(
            s.ended_since("local", "cluster:c1", t - 1).unwrap(),
            vec![a.family.clone()]
        );
        assert!(s.ended_since("local", GXCTL, t - 1).unwrap().is_empty());
        assert!(
            s.ended_since("other", "cluster:c1", t - 1)
                .unwrap()
                .is_empty()
        );
    }
}
