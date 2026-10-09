//! What a host knows about its place in the cluster, on disk (§5.1, §5.2):
//! `<state dir>/cluster/{identity.json, node.crt, node.key, ca.crt, ca.key}`.

use super::pki::{self, Ca, PkiError};
use crate::node::NodeRole;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub cluster_id: String,
    pub node_id: String,
    pub raft_id: u64,
    pub name: String,
    pub role: NodeRole,
    /// The node API address (:8842) other nodes reach this one at.
    pub advertise: SocketAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_ip: Option<IpAddr>,
    /// SHA-256 of the cluster CA's public key.
    pub ca_fingerprint: String,
    /// Servers to ask first (a node, before its cache lists the others).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seeds: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Files {
    pub dir: PathBuf,
}

impl Files {
    /// `<db dir>/cluster`.
    pub fn beside(db_path: &Path) -> Files {
        Files { dir: db_path.parent().unwrap_or(Path::new(".")).join("cluster") }
    }

    pub fn identity(&self) -> PathBuf {
        self.dir.join("identity.json")
    }
    pub fn cert(&self) -> PathBuf {
        self.dir.join("node.crt")
    }
    pub fn key(&self) -> PathBuf {
        self.dir.join("node.key")
    }
    /// The trust bundle: public CA certificates only.
    pub fn trust(&self) -> PathBuf {
        self.dir.join("ca.crt")
    }
    /// Servers only (D20).
    pub fn ca_key(&self) -> PathBuf {
        self.dir.join("ca.key")
    }
    pub fn raft(&self) -> PathBuf {
        self.dir.join("raft")
    }

    pub fn exists(&self) -> bool {
        self.identity().exists()
    }

    pub fn load_identity(&self) -> Result<Option<Identity>, PkiError> {
        match std::fs::read(self.identity()) {
            Ok(b) => Ok(Some(serde_json::from_slice(&b).map_err(|e| PkiError::Invalid(format!("{}: {e}", self.identity().display())))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_identity(&self, id: &Identity) -> Result<(), PkiError> {
        pki::write_private(&self.identity(), &serde_json::to_vec_pretty(id).expect("identity serializes"))
    }

    pub fn save_node_cert(&self, cert_pem: &str, key_pem: &str, trust_pem: &str) -> Result<(), PkiError> {
        pki::write_private(&self.key(), key_pem.as_bytes())?;
        pki::write_private(&self.cert(), cert_pem.as_bytes())?;
        pki::write_private(&self.trust(), trust_pem.as_bytes())
    }

    pub fn read(&self, p: PathBuf) -> Result<String, PkiError> {
        Ok(std::fs::read_to_string(p)?)
    }

    /// The CA key, from a systemd credential when the installer provides one
    /// (`$CREDENTIALS_DIRECTORY/cluster-ca-key`), else the state directory.
    pub fn load_ca(&self) -> Result<Option<Ca>, PkiError> {
        let key = crate::config::credential("cluster-ca-key").filter(|p| p.exists()).unwrap_or_else(|| self.ca_key());
        match (std::fs::read_to_string(&key), std::fs::read_to_string(self.trust())) {
            (Ok(k), Ok(t)) => {
                // The trust bundle may hold the old CA too (during a rotation);
                // the signing CA is the one whose key this is.
                for der_pem in split_pem(&t) {
                    if let Ok(ca) = Ca::from_pem(&der_pem, &k) {
                        if ca_matches_key(&ca) {
                            return Ok(Some(ca));
                        }
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    pub fn save_ca_key(&self, key_pem: &str) -> Result<(), PkiError> {
        pki::write_private(&self.ca_key(), key_pem.as_bytes())
    }

    pub fn delete_all(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn split_pem(bundle: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in bundle.lines() {
        cur.push_str(line);
        cur.push('\n');
        if line.starts_with("-----END CERTIFICATE") {
            out.push(std::mem::take(&mut cur));
        }
    }
    out
}

/// Whether a CA's certificate matches its key (so the right certificate of a
/// bundle is picked).
fn ca_matches_key(ca: &Ca) -> bool {
    rcgen::KeyPair::from_pem(ca.key_pem())
        .ok()
        .and_then(|k| {
            let (_, der) = x509_parser::pem::parse_x509_pem(ca.cert_pem.as_bytes()).ok()?;
            let (_, c) = x509_parser::parse_x509_certificate(&der.contents).ok()?;
            use rcgen::PublicKeyData;
            Some(k.subject_public_key_info() == c.public_key().raw)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trips_and_the_signing_ca_is_found_in_a_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let files = Files { dir: dir.path().join("cluster") };
        assert!(!files.exists());
        let id = Identity {
            cluster_id: "c".into(),
            node_id: "n".into(),
            raft_id: 7,
            name: "h1".into(),
            role: NodeRole::Server,
            advertise: "192.0.2.11:8842".parse().unwrap(),
            tunnel_ip: None,
            ca_fingerprint: "ab".into(),
            seeds: vec![],
        };
        files.save_identity(&id).unwrap();
        assert_eq!(files.load_identity().unwrap(), Some(id));
        let (old, new) = (Ca::generate("c").unwrap(), Ca::generate("c").unwrap());
        files.save_node_cert("x", "y", &format!("{}{}", old.cert_pem, new.cert_pem)).unwrap();
        files.save_ca_key(new.key_pem()).unwrap();
        let found = files.load_ca().unwrap().unwrap();
        assert_eq!(found.public_key_fingerprint().unwrap(), new.public_key_fingerprint().unwrap());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(files.ca_key()).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
