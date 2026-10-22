//! Passkeys (spec/passkeys.md): the stored credentials, and the
//! registration and sign-in ceremonies around `auth::webauthn`'s checks.
//! A passkey is also an identity of provider `passkey` (P1), so it is one
//! of its user's sign-in methods.

use super::webauthn::{self, Expect, PublicKey, b64url, unb64url};
use crate::store::Db;
use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const PASSKEYS: TableDefinition<&str, &[u8]> = TableDefinition::new("passkeys");

/// The identity provider of passkeys (P1).
pub const PROVIDER: &str = "passkey";
/// Most passkeys per user (P4).
pub const MAX_PER_USER: usize = 10;
/// How long a ceremony's challenge lives.
pub const CEREMONY_SECS: u64 = 300;

#[derive(Debug, thiserror::Error)]
pub enum PasskeyError {
    /// Every sign-in failure looks the same to the client (§2); the
    /// reason is for the audit log.
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Invalid(String),
    #[error("no such passkey")]
    NotFound,
    #[error("passkey storage error: {0}")]
    Storage(String),
}

macro_rules! storage_from {
    ($($t:ty),*) => {$(
        impl From<$t> for PasskeyError {
            fn from(e: $t) -> Self {
                PasskeyError::Storage(e.to_string())
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

/// A stored passkey (P1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Passkey {
    /// The credential id, base64url.
    pub id: String,
    pub user_id: String,
    pub name: String,
    /// The COSE public key, base64url.
    pub cose: String,
    pub alg: i64,
    pub counter: u32,
    #[serde(default)]
    pub aaguid: String,
    #[serde(default)]
    pub backup_eligible: bool,
    #[serde(default)]
    pub backed_up: bool,
    pub created_at: u64,
    #[serde(default)]
    pub last_used_at: Option<u64>,
}

impl Passkey {
    pub fn key(&self) -> Result<PublicKey, String> {
        PublicKey::from_cose(&webauthn::cbor(&unb64url(&self.cose)?)?)
    }
}

/// The directory's passkeys, and each user's WebAuthn user handle.
pub struct PasskeyStore {
    db: Arc<Db>,
}

fn cred_key(id: &str) -> String {
    format!("c/{id}")
}

fn handle_key(user: &str) -> String {
    format!("h/{user}")
}

impl PasskeyStore {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    fn rows(&self) -> Result<Vec<(String, Vec<u8>)>, PasskeyError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(PASSKEYS) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for e in table.iter()? {
            let (k, v) = e?;
            out.push((k.value().to_string(), v.value().to_vec()));
        }
        Ok(out)
    }

    pub fn list(&self, user: &str) -> Result<Vec<Passkey>, PasskeyError> {
        let mut v: Vec<Passkey> = self
            .rows()?
            .into_iter()
            .filter(|(k, _)| k.starts_with("c/"))
            .filter_map(|(_, v)| serde_json::from_slice::<Passkey>(&v).ok())
            .filter(|p| p.user_id == user)
            .collect();
        v.sort_by_key(|p| p.created_at);
        Ok(v)
    }

    pub fn get(&self, id: &str) -> Result<Option<Passkey>, PasskeyError> {
        Ok(self
            .rows()?
            .into_iter()
            .find(|(k, _)| *k == cred_key(id))
            .and_then(|(_, v)| serde_json::from_slice(&v).ok()))
    }

    /// The user's WebAuthn user handle (32 random bytes, never the user
    /// id), made when it is first needed.
    pub fn handle(&self, user: &str) -> Result<String, PasskeyError> {
        if let Some((_, v)) = self
            .rows()?
            .into_iter()
            .find(|(k, _)| *k == handle_key(user))
        {
            return Ok(String::from_utf8_lossy(&v).into_owned());
        }
        let h = b64url(&crate::auth::random_bytes(32));
        self.db.write(crate::store::Origin::Auth, |tx| {
            tx.open_table(PASSKEYS)?
                .insert(handle_key(user).as_str(), h.as_bytes())?;
            Ok::<_, crate::store::StoreError>(())
        })?;
        Ok(h)
    }

    /// Whose handle `h` is.
    pub fn user_of_handle(&self, h: &str) -> Result<Option<String>, PasskeyError> {
        Ok(self.rows()?.into_iter().find_map(|(k, v)| {
            let user = k.strip_prefix("h/")?;
            crate::auth::constant_eq(&String::from_utf8_lossy(&v), h).then(|| user.to_string())
        }))
    }

    pub fn put(&self, p: &Passkey) -> Result<(), PasskeyError> {
        self.db.write(crate::store::Origin::Auth, |tx| {
            tx.open_table(PASSKEYS)?
                .insert(cred_key(&p.id).as_str(), serde_json::to_vec(p)?.as_slice())?;
            Ok::<_, PasskeyError>(())
        })
    }

    pub fn delete(&self, id: &str) -> Result<(), PasskeyError> {
        self.db.write(crate::store::Origin::Auth, |tx| {
            tx.open_table(PASSKEYS)?.remove(cred_key(id).as_str())?;
            Ok::<_, PasskeyError>(())
        })
    }

    /// Every passkey of `user`, and its handle, go (the user is deleted).
    pub fn forget_user(&self, user: &str) -> Result<(), PasskeyError> {
        let ids: Vec<String> = self.list(user)?.into_iter().map(|p| p.id).collect();
        self.db.write(crate::store::Origin::Auth, |tx| {
            let mut t = tx.open_table(PASSKEYS)?;
            for id in &ids {
                t.remove(cred_key(id).as_str())?;
            }
            t.remove(handle_key(user).as_str())?;
            Ok::<_, PasskeyError>(())
        })
    }
}

// ---- ceremonies (§2) -------------------------------------------------------

/// What a ceremony in progress is for.
#[derive(Debug, Clone)]
pub enum Purpose {
    Register {
        user_id: String,
        name: String,
    },
    Login,
    /// A passkey invitation's first passkey, for a user still to be made
    /// (spec/tenancy.md D60).
    Invite {
        invitation: String,
        user_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct Ceremony {
    pub purpose: Purpose,
    pub challenge: [u8; 32],
    pub rp_id: String,
    /// The origin the ceremony started from: the only one accepted.
    pub origin: String,
    pub expires_at: u64,
}

/// Sign-in attempts a minute per client address (§2).
const ATTEMPTS_PER_MINUTE: f64 = 10.0;

/// Ceremonies in progress, by the cookie that binds them to a browser,
/// and sign-in attempts per client.
#[derive(Default)]
pub struct Ceremonies {
    open: Mutex<HashMap<String, Ceremony>>,
    attempts: Mutex<HashMap<String, (std::time::Instant, f64)>>,
}

impl Ceremonies {
    /// Whether `client` may try one more sign-in now.
    pub fn attempt(&self, client: &str) -> bool {
        let mut b = self.attempts.lock().unwrap();
        let now = std::time::Instant::now();
        if b.len() > 10_000 {
            b.retain(|_, (at, _)| now.duration_since(*at).as_secs() < 120);
        }
        let (at, level) = b
            .entry(client.to_string())
            .or_insert((now, ATTEMPTS_PER_MINUTE));
        *level = (*level + now.duration_since(*at).as_secs_f64() * ATTEMPTS_PER_MINUTE / 60.0)
            .min(ATTEMPTS_PER_MINUTE);
        *at = now;
        if *level >= 1.0 {
            *level -= 1.0;
            true
        } else {
            false
        }
    }

    /// Start one; the cookie value to bind it with.
    pub fn start(&self, purpose: Purpose, rp_id: &str, origin: &str) -> (String, Ceremony) {
        let now = crate::tenancy::now();
        let mut challenge = [0u8; 32];
        challenge.copy_from_slice(&crate::auth::random_bytes(32));
        let c = Ceremony {
            purpose,
            challenge,
            rp_id: rp_id.to_string(),
            origin: origin.to_string(),
            expires_at: now + CEREMONY_SECS,
        };
        let cookie = crate::auth::random_token(24);
        let mut open = self.open.lock().unwrap();
        open.retain(|_, c| c.expires_at > now);
        if open.len() > 10_000 {
            // A flood of starts: drop the oldest half rather than grow.
            let mut v: Vec<(String, u64)> = open
                .iter()
                .map(|(k, c)| (k.clone(), c.expires_at))
                .collect();
            v.sort_by_key(|(_, e)| *e);
            for (k, _) in v.into_iter().take(5_000) {
                open.remove(&k);
            }
        }
        open.insert(cookie.clone(), c.clone());
        (cookie, c)
    }

    /// Take the ceremony `cookie` names: once, before it expires.
    pub fn take(&self, cookie: &str) -> Option<Ceremony> {
        let c = self.open.lock().unwrap().remove(cookie)?;
        (c.expires_at > crate::tenancy::now()).then_some(c)
    }
}

/// The registration options for `navigator.credentials.create` (§2), as
/// WebAuthn's JSON (binary fields base64url).
pub fn creation_options(
    c: &Ceremony,
    rp_name: &str,
    handle: &str,
    user_name: &str,
    display_name: &str,
    exclude: &[Passkey],
) -> serde_json::Value {
    serde_json::json!({
        "publicKey": {
            "rp": { "id": c.rp_id, "name": rp_name },
            "user": { "id": handle, "name": user_name, "displayName": display_name },
            "challenge": b64url(&c.challenge),
            "pubKeyCredParams": webauthn::ALGORITHMS.iter().map(|a| serde_json::json!({"type": "public-key", "alg": a})).collect::<Vec<_>>(),
            "timeout": CEREMONY_SECS * 1000,
            "attestation": "none",
            "authenticatorSelection": { "residentKey": "required", "requireResidentKey": true, "userVerification": "required" },
            "excludeCredentials": exclude.iter().map(|p| serde_json::json!({"type": "public-key", "id": p.id})).collect::<Vec<_>>(),
        }
    })
}

/// The sign-in options for `navigator.credentials.get` (§2).
pub fn request_options(c: &Ceremony) -> serde_json::Value {
    serde_json::json!({
        "publicKey": {
            "challenge": b64url(&c.challenge),
            "rpId": c.rp_id,
            "timeout": CEREMONY_SECS * 1000,
            "userVerification": "required",
            "allowCredentials": [],
        }
    })
}

/// A credential as the browser returns it (`PublicKeyCredential.toJSON`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialJson {
    pub id: String,
    #[serde(default)]
    pub raw_id: Option<String>,
    #[serde(rename = "type", default)]
    pub kind: String,
    pub response: ResponseJson,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseJson {
    #[serde(rename = "clientDataJSON")]
    pub client_data_json: String,
    #[serde(default)]
    pub attestation_object: Option<String>,
    #[serde(default)]
    pub authenticator_data: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub user_handle: Option<String>,
}

fn b(s: &str, what: &str) -> Result<Vec<u8>, PasskeyError> {
    unb64url(s).map_err(|_| PasskeyError::Refused(format!("{what} isn't base64url")))
}

/// Finish a registration: the new passkey for `user`.
pub fn finish_registration(
    c: &Ceremony,
    cred: &CredentialJson,
    user: &str,
    name: &str,
) -> Result<Passkey, PasskeyError> {
    if cred.kind != "public-key" {
        return Err(PasskeyError::Refused("not a public-key credential".into()));
    }
    let cd = b(&cred.response.client_data_json, "clientDataJSON")?;
    let att = b(
        cred.response
            .attestation_object
            .as_deref()
            .ok_or_else(|| PasskeyError::Refused("no attestationObject".into()))?,
        "attestationObject",
    )?;
    let origins = [c.origin.clone()];
    let e = Expect {
        kind: "webauthn.create",
        challenge: &c.challenge,
        origins: &origins,
        rp_id: &c.rp_id,
    };
    let n = webauthn::verify_registration(&cd, &att, &e).map_err(PasskeyError::Refused)?;
    let id = b64url(&n.id);
    if cred.id != id {
        return Err(PasskeyError::Refused(
            "the credential id doesn't match its data".into(),
        ));
    }
    Ok(Passkey {
        id,
        user_id: user.to_string(),
        name: name.to_string(),
        cose: b64url(&n.cose),
        alg: n.key.alg(),
        counter: n.counter,
        aaguid: n.aaguid.iter().map(|x| format!("{x:02x}")).collect(),
        backup_eligible: n.backup.0,
        backed_up: n.backup.1,
        created_at: crate::tenancy::now(),
        last_used_at: None,
    })
}

/// Finish a sign-in against `stored` (its user's passkey with that id):
/// the counter to keep. `handle` is the user's handle.
pub fn finish_login(
    c: &Ceremony,
    cred: &CredentialJson,
    stored: &Passkey,
    handle: &str,
) -> Result<u32, PasskeyError> {
    let refused = |m: &str| PasskeyError::Refused(m.into());
    if cred.kind != "public-key" {
        return Err(refused("not a public-key credential"));
    }
    // The authenticator says whose passkey it signed with (discoverable).
    let uh = cred
        .response
        .user_handle
        .as_deref()
        .ok_or_else(|| refused("no user handle"))?;
    if !crate::auth::constant_eq(uh, handle) {
        return Err(refused("the user handle isn't this passkey's user's"));
    }
    let cd = b(&cred.response.client_data_json, "clientDataJSON")?;
    let ad = b(
        cred.response
            .authenticator_data
            .as_deref()
            .ok_or_else(|| refused("no authenticatorData"))?,
        "authenticatorData",
    )?;
    let sig = b(
        cred.response
            .signature
            .as_deref()
            .ok_or_else(|| refused("no signature"))?,
        "signature",
    )?;
    let key = stored.key().map_err(PasskeyError::Storage)?;
    let origins = [c.origin.clone()];
    let e = Expect {
        kind: "webauthn.get",
        challenge: &c.challenge,
        origins: &origins,
        rp_id: &c.rp_id,
    };
    let counter =
        webauthn::verify_assertion(&cd, &ad, &sig, &key, &e).map_err(PasskeyError::Refused)?;
    // P3: a counter that goes backwards is a cloned authenticator.
    if counter != 0 && stored.counter != 0 && counter <= stored.counter {
        return Err(refused(
            "the signature counter went backwards: a cloned passkey?",
        ));
    }
    Ok(counter)
}
