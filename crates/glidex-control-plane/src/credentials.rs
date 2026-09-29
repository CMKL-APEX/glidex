//! Credential store: guest login accounts handed to cloud-init.
//!
//! A credential is a Linux username plus a password *hash* and/or SSH
//! public keys. The plaintext password is hashed with SHA-512-crypt
//! (`$6$…`) as soon as it arrives and is never stored, logged or returned;
//! cloud-init only needs the hash (`chpasswd` with `type: hash`). The API
//! exposes [`CredentialInfo`], which omits the hash entirely.
//!
//! Records live in the `credentials` table of the control plane's ReDB
//! file, next to the `vms` table.

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use sha_crypt::{PasswordHasher, ShaCrypt};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const CREDENTIALS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("credentials");

const MIN_PASSWORD_LEN: usize = 8;
const MAX_PASSWORD_LEN: usize = 256;

/// Accounts that already exist in distro images; cloud-init would silently
/// modify them instead of creating a new login.
const RESERVED_USERNAMES: &[&str] = &[
    "root", "daemon", "bin", "sys", "sync", "games", "man", "lp", "mail", "news", "uucp",
    "proxy", "www-data", "backup", "list", "irc", "nobody", "systemd-network",
    "systemd-resolve", "messagebus", "sshd", "syslog", "ubuntu",
];

#[derive(Error, Debug)]
pub enum CredentialError {
    #[error("credential not found: {0}")]
    NotFound(String),

    #[error("credential already exists: {0}")]
    AlreadyExists(String),

    #[error("invalid credential: {0}")]
    Invalid(String),

    #[error("password hashing failed")]
    Hashing,

    #[error("credential storage error: {0}")]
    Storage(String),
}

macro_rules! storage_err {
    ($($t:ty),*) => {$(
        impl From<$t> for CredentialError {
            fn from(e: $t) -> Self {
                CredentialError::Storage(e.to_string())
            }
        }
    )*};
}
storage_err!(
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    serde_json::Error
);

/// Stored credential. Holds a password hash, so it is never serialized to
/// API clients (see [`CredentialInfo`]) and its `Debug` output redacts it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    #[serde(default)]
    pub ssh_authorized_keys: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("username", &self.username)
            .field(
                "password_hash",
                &self.password_hash.as_ref().map(|_| "[REDACTED]"),
            )
            .field("ssh_authorized_keys", &self.ssh_authorized_keys.len())
            .finish()
    }
}

/// API view of a credential: everything except the hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialInfo {
    pub username: String,
    pub has_password: bool,
    pub ssh_authorized_keys: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl From<&Credential> for CredentialInfo {
    fn from(c: &Credential) -> Self {
        Self {
            username: c.username.clone(),
            has_password: c.password_hash.is_some(),
            ssh_authorized_keys: c.ssh_authorized_keys.clone(),
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

/// `POST /credentials` body. `Debug` redacts the password.
#[derive(Deserialize)]
pub struct CreateCredentialRequest {
    pub username: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub ssh_authorized_keys: Option<Vec<String>>,
}

/// `PUT /credentials/{username}` body; omitted fields are left unchanged.
/// An empty key list clears the keys. `Debug` redacts the password.
#[derive(Deserialize)]
pub struct UpdateCredentialRequest {
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub ssh_authorized_keys: Option<Vec<String>>,
}

macro_rules! redacted_debug {
    ($t:ident { $($field:ident),* }) => {
        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($t))
                    $(.field(stringify!($field), &self.$field))*
                    .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
                    .finish()
            }
        }
    };
}
redacted_debug!(CreateCredentialRequest { username, ssh_authorized_keys });
redacted_debug!(UpdateCredentialRequest { ssh_authorized_keys });

pub fn validate_username(username: &str) -> Result<(), CredentialError> {
    let mut chars = username.chars();
    let valid = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && username.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid {
        return Err(CredentialError::Invalid(format!(
            "username '{}' must match ^[a-z_][a-z0-9_-]{{0,31}}$",
            username
        )));
    }
    if RESERVED_USERNAMES.contains(&username) {
        return Err(CredentialError::Invalid(format!(
            "username '{}' is reserved",
            username
        )));
    }
    Ok(())
}

fn validate_password(password: &str) -> Result<(), CredentialError> {
    let len = password.chars().count();
    if !(MIN_PASSWORD_LEN..=MAX_PASSWORD_LEN).contains(&len) {
        return Err(CredentialError::Invalid(format!(
            "password must be {}-{} characters",
            MIN_PASSWORD_LEN, MAX_PASSWORD_LEN
        )));
    }
    if password.chars().any(|c| c.is_control()) {
        return Err(CredentialError::Invalid(
            "password must not contain control characters".to_string(),
        ));
    }
    Ok(())
}

/// Normalize and validate SSH public keys (one key per entry, single line).
fn validate_ssh_keys(keys: Vec<String>) -> Result<Vec<String>, CredentialError> {
    const KEY_TYPES: &[&str] = &["ssh-", "ecdsa-sha2-", "sk-"];
    keys.into_iter()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .map(|k| {
            let well_formed = !k.contains(['\n', '\r'])
                && KEY_TYPES.iter().any(|t| k.starts_with(t))
                && k.split_whitespace().count() >= 2;
            if well_formed {
                Ok(k)
            } else {
                Err(CredentialError::Invalid(
                    "each SSH key must be a single-line OpenSSH public key".to_string(),
                ))
            }
        })
        .collect()
}

/// SHA-512-crypt with a 16-character salt.
///
/// Why not `ShaCrypt::hash_password`: sha-crypt 0.6 writes a 22-character
/// salt into the string but, like glibc, only hashes the first 16. glibc's
/// `crypt()` then returns a 16-character-salt string that never compares
/// equal to the stored one, so the guest rejects every login. 12 random
/// bytes Base64-encode to exactly 16 salt characters.
pub fn hash_password(password: &str) -> Result<String, CredentialError> {
    validate_password(password)?;
    let mut salt = [0u8; 12];
    getrandom::fill(&mut salt).map_err(|_| CredentialError::Hashing)?;
    ShaCrypt::default()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| CredentialError::Hashing)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct CredentialStore {
    db: Arc<Database>,
}

impl CredentialStore {
    pub fn new(db: Arc<Database>) -> Result<Self, CredentialError> {
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(CREDENTIALS_TABLE)?;
        }
        txn.commit()?;
        Ok(Self { db })
    }

    pub fn get(&self, username: &str) -> Result<Credential, CredentialError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CREDENTIALS_TABLE)?;
        let value = table
            .get(username)?
            .ok_or_else(|| CredentialError::NotFound(username.to_string()))?;
        Ok(serde_json::from_slice(value.value())?)
    }

    pub fn list(&self) -> Result<Vec<Credential>, CredentialError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CREDENTIALS_TABLE)?;
        let mut out = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            out.push(serde_json::from_slice(value.value())?);
        }
        Ok(out)
    }

    pub fn create(&self, req: CreateCredentialRequest) -> Result<Credential, CredentialError> {
        validate_username(&req.username)?;
        let password_hash = req.password.as_deref().map(hash_password).transpose()?;
        let ssh_authorized_keys = validate_ssh_keys(req.ssh_authorized_keys.unwrap_or_default())?;
        if password_hash.is_none() && ssh_authorized_keys.is_empty() {
            return Err(CredentialError::Invalid(
                "a password or at least one SSH key is required".to_string(),
            ));
        }
        let ts = now();
        let credential = Credential {
            username: req.username,
            password_hash,
            ssh_authorized_keys,
            created_at: ts,
            updated_at: ts,
        };

        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CREDENTIALS_TABLE)?;
            if table.get(credential.username.as_str())?.is_some() {
                return Err(CredentialError::AlreadyExists(credential.username));
            }
            let bytes = serde_json::to_vec(&credential)?;
            table.insert(credential.username.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(credential)
    }

    pub fn update(
        &self,
        username: &str,
        req: UpdateCredentialRequest,
    ) -> Result<Credential, CredentialError> {
        let password_hash = req.password.as_deref().map(hash_password).transpose()?;
        let ssh_keys = req.ssh_authorized_keys.map(validate_ssh_keys).transpose()?;

        let txn = self.db.begin_write()?;
        let credential = {
            let mut table = txn.open_table(CREDENTIALS_TABLE)?;
            let mut credential: Credential = {
                let value = table
                    .get(username)?
                    .ok_or_else(|| CredentialError::NotFound(username.to_string()))?;
                serde_json::from_slice(value.value())?
            };
            if let Some(hash) = password_hash {
                credential.password_hash = Some(hash);
            }
            if let Some(keys) = ssh_keys {
                credential.ssh_authorized_keys = keys;
            }
            if credential.password_hash.is_none() && credential.ssh_authorized_keys.is_empty() {
                return Err(CredentialError::Invalid(
                    "a credential must keep a password or at least one SSH key".to_string(),
                ));
            }
            credential.updated_at = now();
            let bytes = serde_json::to_vec(&credential)?;
            table.insert(username, bytes.as_slice())?;
            credential
        };
        txn.commit()?;
        Ok(credential)
    }

    pub fn delete(&self, username: &str) -> Result<(), CredentialError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CREDENTIALS_TABLE)?;
            if table.remove(username)?.is_none() {
                return Err(CredentialError::NotFound(username.to_string()));
            }
        }
        txn.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha_crypt::{PasswordHashRef, PasswordVerifier};

    fn store() -> (CredentialStore, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Database::create(dir.path().join("c.db")).unwrap());
        (CredentialStore::new(db).unwrap(), dir)
    }

    fn req(username: &str, password: Option<&str>) -> CreateCredentialRequest {
        CreateCredentialRequest {
            username: username.into(),
            password: password.map(Into::into),
            ssh_authorized_keys: None,
        }
    }

    #[test]
    fn stores_only_a_verifiable_sha512_hash() {
        let (store, _dir) = store();
        let cred = store.create(req("alice", Some("correct horse"))).unwrap();
        let hash = cred.password_hash.clone().unwrap();
        assert!(hash.starts_with("$6$"), "{hash}");
        // glibc only reads 16 salt characters; a longer salt in the string
        // makes every guest login fail (see hash_password).
        let salt = hash.split('$').find(|p| !p.is_empty() && *p != "6" && !p.starts_with("rounds=")).unwrap();
        assert_eq!(salt.len(), 16, "{hash}");
        assert!(!hash.contains("correct horse"));

        let stored = store.get("alice").unwrap();
        let parsed = PasswordHashRef::new(stored.password_hash.as_deref().unwrap()).unwrap();
        assert!(ShaCrypt::default()
            .verify_password(b"correct horse", parsed)
            .is_ok());
        assert!(ShaCrypt::default().verify_password(b"wrong", parsed).is_err());
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let (store, _dir) = store();
        let cred = store.create(req("bob", Some("s3cret-pass"))).unwrap();
        let debug = format!("{:?}", cred);
        assert!(!debug.contains("$6$"), "{debug}");
        let debug = format!("{:?}", req("bob", Some("s3cret-pass")));
        assert!(!debug.contains("s3cret-pass"), "{debug}");
    }

    #[test]
    fn info_never_contains_the_hash() {
        let (store, _dir) = store();
        let cred = store.create(req("carol", Some("password123"))).unwrap();
        let json = serde_json::to_string(&CredentialInfo::from(&cred)).unwrap();
        assert!(!json.contains("$6$"), "{json}");
        assert!(json.contains("\"has_password\":true"));
    }

    #[test]
    fn rejects_bad_input() {
        let (store, _dir) = store();
        for name in ["Root", "root", "9lives", "a b", "", &"x".repeat(33)] {
            assert!(
                matches!(store.create(req(name, Some("password123"))), Err(CredentialError::Invalid(_))),
                "{name:?}"
            );
        }
        assert!(matches!(store.create(req("dave", Some("short"))), Err(CredentialError::Invalid(_))));
        assert!(matches!(store.create(req("dave", None)), Err(CredentialError::Invalid(_))));
        let bad_key = CreateCredentialRequest {
            username: "dave".into(),
            password: None,
            ssh_authorized_keys: Some(vec!["not a key".into()]),
        };
        assert!(matches!(store.create(bad_key), Err(CredentialError::Invalid(_))));
    }

    #[test]
    fn create_update_delete_round_trip() {
        let (store, _dir) = store();
        store.create(req("erin", Some("password123"))).unwrap();
        assert!(matches!(
            store.create(req("erin", Some("password456"))),
            Err(CredentialError::AlreadyExists(_))
        ));

        let before = store.get("erin").unwrap().password_hash;
        let updated = store
            .update(
                "erin",
                UpdateCredentialRequest {
                    password: Some("new-password".into()),
                    ssh_authorized_keys: Some(vec!["ssh-ed25519 AAAAC3Nza erin@host".into()]),
                },
            )
            .unwrap();
        assert_ne!(updated.password_hash, before);
        assert_eq!(updated.ssh_authorized_keys.len(), 1);
        assert_eq!(store.list().unwrap().len(), 1);

        store.delete("erin").unwrap();
        assert!(matches!(store.get("erin"), Err(CredentialError::NotFound(_))));
        assert!(matches!(store.delete("erin"), Err(CredentialError::NotFound(_))));
    }
}
