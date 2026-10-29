//! Connector secrets (spec/19-tenancy.md §13, D21).
//!
//! Each directory has a random **data key** that encrypts its connectors'
//! secrets with ChaCha20-Poly1305 (`ring`; a random 96-bit nonce per
//! value, the directory, connector, field and key id as associated data).
//! Data keys are stored wrapped by the host's **KEK**: the systemd
//! credential `secrets-kek`, or the file `auth.secrets_key`, 32 random
//! bytes made on first use, never written to the database, snapshots,
//! feeds or departure bundles. A cluster's servers share it: the leader
//! sends it with the CA key to each new server (14-clustering.md §12.1).

use base64::Engine as _;
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub const KEK_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secrets key: {0}")]
    Key(String),
    #[error("a stored secret can't be decrypted (wrong secrets key, or corrupted)")]
    Decrypt,
}

fn b64(v: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(v)
}

fn unb64(s: &str) -> Result<Vec<u8>, SecretError> {
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(s)
        .map_err(|_| SecretError::Decrypt)
}

/// The host's key-encryption key, and the one before it while a rotation
/// re-wraps the data keys (each wrapped key names its KEK by id).
pub struct Kek {
    bytes: Zeroizing<Vec<u8>>,
    previous: Option<Zeroizing<Vec<u8>>>,
    /// Where it lives, if anywhere (a one-off key has none); the previous
    /// key sits beside it as `<path>.prev`.
    path: Option<PathBuf>,
    /// It came from a systemd credential: read-only, provisioned by the
    /// operator on every server.
    credential: bool,
}

impl Kek {
    /// The systemd credential `secrets-kek`, else `file` (created with 32
    /// random bytes, 0600, when missing), else a one-off key for this
    /// process (embedding and tests; secrets then die with it).
    pub fn load(file: Option<&Path>) -> Result<Kek, SecretError> {
        if let Some(p) = crate::config::credential("secrets-kek") {
            return Ok(Kek {
                credential: true,
                ..Self::read(&p)?
            });
        }
        match file {
            Some(p) if p.exists() => Self::read(p),
            Some(p) => {
                let bytes = Zeroizing::new(crate::auth::random_bytes(KEK_LEN));
                crate::cluster::pki::write_private(p, &bytes)
                    .map_err(|e| SecretError::Key(e.to_string()))?;
                tracing::info!(path = %p.display(), "created the secrets key");
                Ok(Kek {
                    bytes,
                    previous: None,
                    path: Some(p.to_path_buf()),
                    credential: false,
                })
            }
            None => {
                tracing::warn!(
                    "no secrets key file configured: connector secrets last only as long as this process"
                );
                Ok(Kek {
                    bytes: Zeroizing::new(crate::auth::random_bytes(KEK_LEN)),
                    previous: None,
                    path: None,
                    credential: false,
                })
            }
        }
    }

    fn read(p: &Path) -> Result<Kek, SecretError> {
        let read = |p: &Path| -> Result<Zeroizing<Vec<u8>>, SecretError> {
            let b = Zeroizing::new(
                std::fs::read(p).map_err(|e| SecretError::Key(format!("{}: {e}", p.display())))?,
            );
            if b.len() != KEK_LEN {
                return Err(SecretError::Key(format!(
                    "{} must hold {KEK_LEN} bytes",
                    p.display()
                )));
            }
            Ok(b)
        };
        let prev = prev_path(p);
        let previous = if prev.exists() {
            Some(read(&prev)?)
        } else {
            None
        };
        Ok(Kek {
            bytes: read(p)?,
            previous,
            path: Some(p.to_path_buf()),
            credential: false,
        })
    }

    /// Take `bytes` as the key, and `previous` as the one before it (a
    /// new server receiving the cluster's keys, or a rotation), writing
    /// them to their files when there are any. A key from a credential
    /// can't be replaced: it must already be the cluster's.
    pub fn install(&mut self, bytes: &[u8], previous: Option<&[u8]>) -> Result<(), SecretError> {
        if bytes.len() != KEK_LEN || previous.is_some_and(|p| p.len() != KEK_LEN) {
            return Err(SecretError::Key(format!(
                "a secrets key is {KEK_LEN} bytes"
            )));
        }
        if self.credential {
            if *self.bytes != bytes {
                return Err(SecretError::Key(
                    "this server's secrets-kek credential is not the cluster's: give every server the same one".into(),
                ));
            }
            return Ok(());
        }
        if let Some(p) = &self.path {
            let prev = prev_path(p);
            match previous {
                Some(b) => crate::cluster::pki::write_private(&prev, b)
                    .map_err(|e| SecretError::Key(e.to_string()))?,
                None => {
                    let _ = std::fs::remove_file(&prev);
                }
            }
            crate::cluster::pki::write_private(p, bytes)
                .map_err(|e| SecretError::Key(e.to_string()))?;
        }
        self.bytes = Zeroizing::new(bytes.to_vec());
        self.previous = previous.map(|b| Zeroizing::new(b.to_vec()));
        Ok(())
    }

    /// Whether the key comes from a systemd credential (read-only: it is
    /// rotated by changing the credential on every server).
    pub fn is_credential(&self) -> bool {
        self.credential
    }

    pub fn previous(&self) -> Option<&[u8]> {
        self.previous.as_deref().map(|v| v.as_slice())
    }

    /// The key named `id`: the current one or the previous one.
    fn by_id(&self, id: &str) -> Option<&[u8]> {
        if id == self.id() {
            return Some(&self.bytes);
        }
        self.previous
            .as_deref()
            .filter(|p| key_id(p) == id)
            .map(|v| v.as_slice())
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Short public id: which KEK wrapped a data key.
    pub fn id(&self) -> String {
        key_id(&self.bytes)
    }
}

fn key_id(bytes: &[u8]) -> String {
    let d = sha2::Sha256::digest(bytes);
    d[..6].iter().map(|b| format!("{b:02x}")).collect()
}

fn prev_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".prev");
    PathBuf::from(s)
}

/// A data key as stored: wrapped by the KEK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedKey {
    pub directory_id: String,
    pub kek_id: String,
    pub nonce: String,
    pub wrapped: String,
}

/// A secret as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sealed {
    pub nonce: String,
    pub ciphertext: String,
    pub updated_at: u64,
}

fn key(bytes: &[u8]) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, bytes).expect("32-byte key"))
}

fn seal_with(k: &[u8], aad: &[u8], plaintext: &[u8]) -> (String, String) {
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&crate::auth::random_bytes(NONCE_LEN));
    let mut buf = plaintext.to_vec();
    key(k)
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut buf,
        )
        .expect("seal");
    (b64(&nonce), b64(&buf))
}

fn open_with(
    k: &[u8],
    aad: &[u8],
    nonce: &str,
    ciphertext: &str,
) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    let n: [u8; NONCE_LEN] = unb64(nonce)?.try_into().map_err(|_| SecretError::Decrypt)?;
    let mut buf = Zeroizing::new(unb64(ciphertext)?);
    let plain = key(k)
        .open_in_place(Nonce::assume_unique_for_key(n), Aad::from(aad), &mut buf)
        .map_err(|_| SecretError::Decrypt)?;
    let len = plain.len();
    buf.truncate(len);
    Ok(buf)
}

fn wrap_aad(directory: &str) -> Vec<u8> {
    format!("glidex-data-key\0{directory}").into_bytes()
}

fn secret_aad(directory: &str, connector: &str, field: &str) -> Vec<u8> {
    format!("glidex-connector-secret\0{directory}\0{connector}\0{field}").into_bytes()
}

/// A new data key for `directory`, wrapped by `kek`.
pub fn new_data_key(kek: &Kek, directory: &str) -> WrappedKey {
    let dk = Zeroizing::new(crate::auth::random_bytes(32));
    let (nonce, wrapped) = seal_with(kek.bytes(), &wrap_aad(directory), &dk);
    WrappedKey {
        directory_id: directory.into(),
        kek_id: kek.id(),
        nonce,
        wrapped,
    }
}

/// Opens with whichever of `kek`'s keys wrapped `w`.
pub fn unwrap_data_key(kek: &Kek, w: &WrappedKey) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    let k = kek.by_id(&w.kek_id).ok_or(SecretError::Decrypt)?;
    open_with(k, &wrap_aad(&w.directory_id), &w.nonce, &w.wrapped)
}

/// The same data key wrapped by `kek`'s current key (rotation), opened
/// with whichever of its keys wrapped it.
pub fn rewrap(kek: &Kek, w: &WrappedKey) -> Result<WrappedKey, SecretError> {
    let dk = unwrap_data_key(kek, w)?;
    let (nonce, wrapped) = seal_with(kek.bytes(), &wrap_aad(&w.directory_id), &dk);
    Ok(WrappedKey {
        directory_id: w.directory_id.clone(),
        kek_id: kek.id(),
        nonce,
        wrapped,
    })
}

pub fn seal(
    data_key: &[u8],
    directory: &str,
    connector: &str,
    field: &str,
    value: &str,
    now: u64,
) -> Sealed {
    let (nonce, ciphertext) = seal_with(
        data_key,
        &secret_aad(directory, connector, field),
        value.as_bytes(),
    );
    Sealed {
        nonce,
        ciphertext,
        updated_at: now,
    }
}

pub fn open(
    data_key: &[u8],
    directory: &str,
    connector: &str,
    field: &str,
    s: &Sealed,
) -> Result<Zeroizing<String>, SecretError> {
    let bytes = open_with(
        data_key,
        &secret_aad(directory, connector, field),
        &s.nonce,
        &s.ciphertext,
    )?;
    Ok(Zeroizing::new(
        String::from_utf8(bytes.to_vec()).map_err(|_| SecretError::Decrypt)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_binding() {
        let kek = Kek::load(None).unwrap();
        let w = new_data_key(&kek, "local");
        let dk = unwrap_data_key(&kek, &w).unwrap();
        let s = seal(&dk, "local", "c1", "client_secret", "hunter2", 1);
        assert!(!s.ciphertext.contains("hunter2"));
        assert_eq!(
            open(&dk, "local", "c1", "client_secret", &s)
                .unwrap()
                .as_str(),
            "hunter2"
        );
        // Bound to where it belongs: another connector or field can't use it.
        assert!(open(&dk, "local", "c2", "client_secret", &s).is_err());
        assert!(open(&dk, "local", "c1", "other", &s).is_err());
        // Another KEK can't unwrap the data key; after a rotation the
        // previous key still can, and a rewrap moves it to the new one.
        let mut other = Kek::load(None).unwrap();
        assert!(unwrap_data_key(&other, &w).is_err());
        let old = kek.bytes().to_vec();
        let new = other.bytes().to_vec();
        other.install(&new, Some(&old)).unwrap();
        assert_eq!(*unwrap_data_key(&other, &w).unwrap(), *dk);
        let w2 = rewrap(&other, &w).unwrap();
        assert_eq!(w2.kek_id, other.id());
        other.install(&new, None).unwrap();
        assert_eq!(*unwrap_data_key(&other, &w2).unwrap(), *dk);
        assert!(unwrap_data_key(&other, &w).is_err(), "the old key is gone");
    }

    #[test]
    fn kek_file_is_made_once_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("secrets.kek");
        let a = Kek::load(Some(&p)).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let b = Kek::load(Some(&p)).unwrap();
        assert_eq!(a.id(), b.id());
        // A rotation keeps the previous key beside it until the next one.
        let mut c = Kek::load(Some(&p)).unwrap();
        let new = crate::auth::random_bytes(KEK_LEN);
        c.install(&new, Some(a.bytes())).unwrap();
        let d = Kek::load(Some(&p)).unwrap();
        assert_eq!((d.bytes(), d.previous()), (&new[..], Some(a.bytes())));
        assert_eq!(
            std::fs::metadata(dir.path().join("secrets.kek.prev"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
