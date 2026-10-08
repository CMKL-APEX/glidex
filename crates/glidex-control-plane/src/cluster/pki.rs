//! The cluster PKI (spec/clustering.md §5.1, §12): one CA (P-256, 10 years)
//! whose key every server holds (D20); node certificates `CN=node:<id>`
//! with the advertise address as SAN, valid one year.

use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType, SerialNumber, PKCS_ECDSA_P256_SHA256,
};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::path::Path;
use thiserror::Error;
use x509_parser::prelude::{parse_x509_certificate, X509Certificate};

pub const CA_VALIDITY_DAYS: i64 = 3650;
pub const NODE_VALIDITY_DAYS: i64 = 365;
/// Organisational unit of node certificates: what the node may do (§12.3).
pub const OU_SERVER: &str = "glidex-server";
pub const OU_AGENT: &str = "glidex-agent";

#[derive(Debug, Error)]
pub enum PkiError {
    #[error("certificate error: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("certificate parse error: {0}")]
    Parse(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Invalid(String),
}

fn now() -> ::time::OffsetDateTime {
    ::time::OffsetDateTime::now_utc()
}

/// A certificate authority: its certificate and signing key (PEM).
#[derive(Clone)]
pub struct Ca {
    pub cert_pem: String,
    key_pem: zeroize::Zeroizing<String>,
}

impl std::fmt::Debug for Ca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Ca({})", self.public_key_fingerprint().unwrap_or_default())
    }
}

/// What a node certificate says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeCert {
    pub node_id: String,
    pub server: bool,
    /// Lower-case hex, no separators.
    pub serial: String,
    pub not_after: i64,
    pub issuer_fingerprint: String,
}

impl Ca {
    /// A new CA for `cluster_id`.
    pub fn generate(cluster_id: &str) -> Result<Ca, PkiError> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let mut params = CertificateParams::default();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, format!("glidex cluster {}", cluster_id));
        dn.push(DnType::OrganizationName, "glidex");
        params.distinguished_name = dn;
        params.not_before = now() - ::time::Duration::minutes(5);
        params.not_after = now() + ::time::Duration::days(CA_VALIDITY_DAYS);
        params.serial_number = Some(random_serial());
        let cert = params.self_signed(&key)?;
        Ok(Ca { cert_pem: cert.pem(), key_pem: zeroize::Zeroizing::new(key.serialize_pem()) })
    }

    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Ca, PkiError> {
        KeyPair::from_pem(key_pem)?;
        parse_pem_cert(cert_pem)?;
        Ok(Ca { cert_pem: cert_pem.to_string(), key_pem: zeroize::Zeroizing::new(key_pem.to_string()) })
    }

    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    /// SHA-256 of the CA's public key (SPKI DER), lower-case hex: what a join
    /// token pins (§5.2).
    pub fn public_key_fingerprint(&self) -> Result<String, PkiError> {
        public_key_fingerprint(&self.cert_pem)
    }

    /// Sign a node's CSR (§5.2 step 3). Whatever the CSR says about its
    /// subject is ignored: the subject is `CN=node:<id>, OU=<role>`; only
    /// its public key and the IP/DNS names it asks for are taken.
    pub fn sign_node(&self, csr_pem: &str, node_id: &str, server: bool, validity_days: i64) -> Result<(String, NodeCert), PkiError> {
        let mut csr = CertificateSigningRequestParams::from_pem(csr_pem)?;
        let sans: Vec<SanType> = csr
            .params
            .subject_alt_names
            .iter()
            .filter(|s| matches!(s, SanType::IpAddress(_) | SanType::DnsName(_)))
            .cloned()
            .collect();
        let mut p = CertificateParams::default();
        p.subject_alt_names = sans;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, format!("node:{}", node_id));
        dn.push(DnType::OrganizationalUnitName, if server { OU_SERVER } else { OU_AGENT });
        p.distinguished_name = dn;
        p.is_ca = IsCa::NoCa;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth];
        p.not_before = now() - ::time::Duration::minutes(5);
        p.not_after = now() + ::time::Duration::days(validity_days);
        let serial = random_serial();
        let serial_hex = hex(serial.as_ref());
        p.serial_number = Some(serial);
        csr.params = p;
        let key = KeyPair::from_pem(&self.key_pem)?;
        let issuer = Issuer::from_ca_cert_pem(&self.cert_pem, key)?;
        let cert = csr.signed_by(&issuer)?;
        let pem = cert.pem();
        let info = NodeCert {
            node_id: node_id.to_string(),
            server,
            serial: serial_hex,
            not_after: (now() + ::time::Duration::days(validity_days)).unix_timestamp(),
            issuer_fingerprint: self.public_key_fingerprint()?,
        };
        Ok((pem, info))
    }
}

/// A node's own key pair and its CSR.
pub struct NodeKey {
    key: KeyPair,
}

impl NodeKey {
    pub fn generate() -> Result<NodeKey, PkiError> {
        Ok(NodeKey { key: KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)? })
    }

    pub fn from_pem(pem: &str) -> Result<NodeKey, PkiError> {
        Ok(NodeKey { key: KeyPair::from_pem(pem)? })
    }

    pub fn key_pem(&self) -> zeroize::Zeroizing<String> {
        zeroize::Zeroizing::new(self.key.serialize_pem())
    }

    /// A CSR naming `addrs` (IP addresses) and `names` (DNS names).
    pub fn csr(&self, addrs: &[IpAddr], names: &[String]) -> Result<String, PkiError> {
        let mut p = CertificateParams::default();
        p.subject_alt_names = addrs.iter().map(|a| SanType::IpAddress(*a)).collect();
        for n in names {
            p.subject_alt_names.push(SanType::DnsName(n.clone().try_into().map_err(|_| PkiError::Invalid(format!("bad DNS name {n}")))?));
        }
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "glidex node (CSR)");
        p.distinguished_name = dn;
        Ok(p.serialize_request(&self.key)?.pem()?)
    }
}

fn random_serial() -> SerialNumber {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("system randomness");
    b[0] &= 0x7f; // positive INTEGER
    if b[0] == 0 {
        b[0] = 1;
    }
    SerialNumber::from_slice(&b)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn parse_pem_cert(pem: &str) -> Result<Vec<u8>, PkiError> {
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).map_err(|e| PkiError::Parse(e.to_string()))?;
    Ok(p.contents)
}

/// Every certificate of a PEM bundle, as DER.
pub fn pem_bundle_ders(pem: &str) -> Result<Vec<Vec<u8>>, PkiError> {
    let mut out = Vec::new();
    for p in x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes()) {
        out.push(p.map_err(|e| PkiError::Parse(e.to_string()))?.contents);
    }
    Ok(out)
}

pub fn public_key_fingerprint(cert_pem: &str) -> Result<String, PkiError> {
    let der = parse_pem_cert(cert_pem)?;
    let (_, c) = parse_x509_certificate(&der).map_err(|e| PkiError::Parse(e.to_string()))?;
    Ok(hex(&Sha256::digest(c.public_key().raw)))
}

/// What a leaf certificate (DER) says about its node.
pub fn inspect_node_cert(der: &[u8]) -> Result<NodeCert, PkiError> {
    let (_, c) = parse_x509_certificate(der).map_err(|e| PkiError::Parse(e.to_string()))?;
    let cn = c
        .subject()
        .iter_common_name()
        .next()
        .and_then(|a| a.as_str().ok())
        .ok_or_else(|| PkiError::Invalid("certificate has no common name".into()))?;
    let node_id = cn.strip_prefix("node:").ok_or_else(|| PkiError::Invalid(format!("common name {cn:?} is not a node")))?.to_string();
    let ou = c.subject().iter_organizational_unit().next().and_then(|a| a.as_str().ok()).unwrap_or("");
    let serial = c.tbs_certificate.raw_serial().iter().skip_while(|b| **b == 0).map(|b| format!("{:02x}", b)).collect::<String>();
    Ok(NodeCert {
        node_id,
        server: ou == OU_SERVER,
        serial,
        not_after: c.validity().not_after.timestamp(),
        issuer_fingerprint: String::new(),
    })
}

/// Seconds until `not_after`, negative once expired.
pub fn seconds_left(cert: &NodeCert) -> i64 {
    cert.not_after - now().unix_timestamp()
}

/// Write `data` to `path` readable by the owner only.
pub fn write_private(path: &Path, data: &[u8]) -> Result<(), PkiError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_cert_is_signed_for_its_id_and_role() {
        let ca = Ca::generate("c1").unwrap();
        let key = NodeKey::generate().unwrap();
        let csr = key.csr(&["192.0.2.11".parse().unwrap()], &[]).unwrap();
        let (pem, info) = ca.sign_node(&csr, "n1", true, NODE_VALIDITY_DAYS).unwrap();
        let der = &pem_bundle_ders(&pem).unwrap()[0];
        let seen = inspect_node_cert(der).unwrap();
        assert_eq!((seen.node_id.as_str(), seen.server), ("n1", true));
        assert_eq!(seen.serial, info.serial);
        assert!(seconds_left(&seen) > 364 * 86400);
        // The CSR can't talk its way into another identity: the CA names the subject.
        let (pem, _) = ca.sign_node(&csr, "other", false, 30).unwrap();
        let seen = inspect_node_cert(&pem_bundle_ders(&pem).unwrap()[0]).unwrap();
        assert_eq!((seen.node_id.as_str(), seen.server), ("other", false));
    }

    #[test]
    fn fingerprints_pin_the_ca() {
        let a = Ca::generate("c1").unwrap();
        let b = Ca::generate("c1").unwrap();
        assert_eq!(a.public_key_fingerprint().unwrap().len(), 64);
        assert_ne!(a.public_key_fingerprint().unwrap(), b.public_key_fingerprint().unwrap());
        let again = Ca::from_pem(&a.cert_pem, a.key_pem()).unwrap();
        assert_eq!(again.public_key_fingerprint().unwrap(), a.public_key_fingerprint().unwrap());
    }

    #[test]
    fn serials_are_unique_and_positive() {
        let ca = Ca::generate("c1").unwrap();
        let key = NodeKey::generate().unwrap();
        let csr = key.csr(&[], &["n1.example".into()]).unwrap();
        let s: std::collections::HashSet<_> = (0..20).map(|_| ca.sign_node(&csr, "n", true, 1).unwrap().1.serial).collect();
        assert_eq!(s.len(), 20);
    }
}
