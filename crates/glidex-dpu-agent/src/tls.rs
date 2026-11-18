//! Mutual TLS between netd and the agent (spec/31-dpu.md §10.2): both
//! sides present a certificate from the agent channel's CA, and the agent
//! accepts only the client identity it was configured with.

use rustls::crypto::CryptoProvider;
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::path::Path;
use std::sync::Arc;

/// PEM text of a CA bundle, a certificate chain and its key.
#[derive(Clone)]
pub struct TlsPem {
    pub ca: String,
    pub cert: String,
    pub key: String,
}

impl std::fmt::Debug for TlsPem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsPem").finish_non_exhaustive()
    }
}

impl TlsPem {
    pub fn load(ca: &Path, cert: &Path, key: &Path) -> Result<Self, String> {
        let read =
            |p: &Path| std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
        Ok(Self {
            ca: read(ca)?,
            cert: read(cert)?,
            key: read(key)?,
        })
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn certs(pem: &str, what: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let certs: Vec<_> = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{what}: {e}"))?;
    if certs.is_empty() {
        return Err(format!("{what}: no certificate"));
    }
    Ok(certs)
}

fn roots(pem: &str) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();
    for c in certs(pem, "ca")? {
        roots.add(c).map_err(|e| format!("ca: {e}"))?;
    }
    Ok(roots)
}

fn key(pem: &str) -> Result<PrivateKeyDer<'static>, String> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).map_err(|_| "key: no usable private key".into())
}

/// The agent's side: a client certificate from `ca` is **required**.
pub fn server_config(pem: &TlsPem) -> Result<Arc<ServerConfig>, String> {
    let provider = provider();
    let verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots(&pem.ca)?), provider.clone())
            .build()
            .map_err(|e| format!("client verifier: {e}"))?;
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs(&pem.cert, "cert")?, key(&pem.key)?)
        .map_err(|e| format!("cert: {e}"))?;
    Ok(Arc::new(config))
}

/// netd's side: the agent's certificate must chain to `ca`, and netd
/// presents its own.
pub fn client_config(pem: &TlsPem) -> Result<Arc<ClientConfig>, String> {
    let config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots(&pem.ca)?)
        .with_client_auth_cert(certs(&pem.cert, "cert")?, key(&pem.key)?)
        .map_err(|e| format!("cert: {e}"))?;
    Ok(Arc::new(config))
}

/// The subject CN of a DER certificate.
pub fn common_name(der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    cert.subject()
        .iter_common_name()
        .next()?
        .as_str()
        .ok()
        .map(str::to_string)
}
