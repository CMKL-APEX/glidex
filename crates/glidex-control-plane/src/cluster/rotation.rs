//! CA rotation and certificate renewal (spec/clustering.md §12.5).
//!
//! The state is one row of `ca_bundle`: the trust bundle every node installs,
//! which CA signs now and which older ones are still trusted. A rotation is
//! four moves, each visible in that row so a new leader resumes:
//! (1) new CA key to every server, (2) trust {old, new}, (3) every node
//! renews its certificate against the new CA, (4) retire the old CA.

use super::pki::{self, Ca, NodeKey};
use super::runtime::{Cluster, ClusterError};
use super::net::TlsMaterial;
use crate::node::{NodeRole, NodeStore};
use crate::store::{Origin, TableId};
use redb::ReadableTable;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const STATE_KEY: &str = "state";
/// Renew a node certificate this long before it expires.
const RENEW_BEFORE_SECS: i64 = 30 * 86400;
/// Start rotating a CA this long before it expires.
const ROTATE_BEFORE_SECS: i64 = 365 * 86400;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaState {
    /// Public certificates every node trusts.
    pub trust_pem: String,
    /// The CA that signs now.
    pub signing_fp: String,
    /// Older CAs still in the bundle until every node has renewed.
    #[serde(default)]
    pub retiring: Vec<String>,
    pub started_at: u64,
    /// Retire the old CAs at this time even if some node has not renewed.
    #[serde(default)]
    pub retire_at: Option<u64>,
}

fn other(e: impl std::fmt::Display) -> ClusterError {
    ClusterError::Other(e.to_string())
}

fn normalize(pem: &str) -> String {
    pem.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

impl Cluster {
    pub fn ca_state(&self) -> Option<CaState> {
        let txn = self.db.begin_read().ok()?;
        let t = txn.open_table(TableId::CaBundle.definition()).ok()?;
        let v = t.get(STATE_KEY).ok()??;
        serde_json::from_slice(v.value()).ok()
    }

    /// The fingerprint tokens pin and joiners expect: the signing CA.
    pub fn signing_fp(&self) -> String {
        self.ca_state().map(|s| s.signing_fp).unwrap_or_else(|| self.identity.ca_fingerprint.clone())
    }

    fn write_state(&self, st: &CaState) -> Result<(), ClusterError> {
        self.db
            .write(Origin::Api, |tx| -> Result<(), crate::store::StoreError> {
                tx.open_table(TableId::CaBundle.definition())?.insert(STATE_KEY, serde_json::to_vec(st).map_err(|e| crate::store::StoreError::Io(std::io::Error::other(e.to_string())))?.as_slice())?;
                Ok(())
            })
            .map_err(other)
    }

    /// A server takes a new CA: its key becomes the signing key, the old key
    /// is kept until the rotation ends, and the certificate joins the trust file.
    pub fn install_signing_ca(&self, ca: Ca) -> Result<(), ClusterError> {
        let key = self.files.ca_key();
        if key.exists() {
            let old = key.with_extension("key.old");
            if !old.exists() {
                std::fs::rename(&key, &old).map_err(other)?;
            }
        }
        self.files.save_ca_key(ca.key_pem())?;
        let trust = self.files.read(self.files.trust()).unwrap_or_default();
        let fp = ca.public_key_fingerprint()?;
        if !pki::pem_bundle_ders(&trust)?.iter().any(|d| pki::public_key_fingerprint(&pem_of(d)).ok().as_deref() == Some(fp.as_str())) {
            pki::write_private(&self.files.trust(), format!("{}\n{}", trust.trim_end(), ca.cert_pem).as_bytes())?;
        }
        *self.ca.lock().unwrap() = Some(ca);
        self.reload_tls()
    }

    fn reload_tls(&self) -> Result<(), ClusterError> {
        let m = TlsMaterial::from_pem(&self.files.read(self.files.cert())?, &self.files.read(self.files.key())?, &self.files.read(self.files.trust())?)?;
        self.tls.reload(m)?;
        Ok(())
    }

    /// Steps 1–2 (leader): a new CA, its key to every server, then trust both.
    pub async fn rotate_ca(&self, grace_secs: Option<u64>) -> Result<Value, ClusterError> {
        let node = self.node.as_ref().ok_or_else(|| other("only a server rotates the CA"))?;
        if !node.is_leader() {
            return Err(other("this is not the leader"));
        }
        let new = Ca::generate(&self.identity.cluster_id)?;
        let new_fp = new.public_key_fingerprint()?;
        let nodes = NodeStore::new(self.db.clone()).list().map_err(other)?;
        let body = json!({ "key_pem": new.key_pem(), "cert_pem": new.cert_pem }).to_string();
        for n in nodes.iter().filter(|n| n.spec.role == NodeRole::Server && !n.status.phase.is_tombstone() && n.meta.id != self.identity.node_id) {
            let addr = n.status.advertise.ok_or_else(|| other(format!("{} has no address", n.spec.name)))?;
            let r = self.client.request(&addr.to_string(), hyper::Method::PUT, "/cluster/v1/ca", &[("content-type", "application/json".into())], body.clone().into()).await.map_err(other)?;
            if !r.status.is_success() {
                return Err(other(format!("{} did not take the new CA key: {}", n.spec.name, r.status)));
            }
        }
        let prev = self.ca_state();
        let old_bundle = prev.as_ref().map(|s| s.trust_pem.clone()).unwrap_or_else(|| self.files.read(self.files.trust()).unwrap_or_default());
        let old_fps: Vec<String> = pki::pem_bundle_ders(&old_bundle)?.iter().filter_map(|d| pki::public_key_fingerprint(&pem_of(d)).ok()).collect();
        self.install_signing_ca(new.clone())?;
        let now = crate::tenancy::now();
        let grace = grace_secs.unwrap_or(self.config.ca_rotation_grace_secs);
        let st = CaState {
            trust_pem: format!("{}\n{}", new.cert_pem.trim_end(), old_bundle.trim_end()),
            signing_fp: new_fp.clone(),
            retiring: old_fps,
            started_at: now,
            retire_at: Some(now + grace),
        };
        self.write_state(&st)?;
        tracing::warn!(new_ca = %&new_fp[..12], "CA rotation started");
        Ok(json!({ "signing_ca": new_fp, "retiring": st.retiring.len(), "retire_at": st.retire_at }))
    }

    /// Install the replicated trust bundle on this node (step 2), and let
    /// servers drop an old key once nothing is retiring (step 4).
    pub fn trust_sync(&self) {
        let Some(st) = self.ca_state() else { return };
        let have = self.files.read(self.files.trust()).unwrap_or_default();
        if normalize(&have) != normalize(&st.trust_pem) {
            if let Err(e) = pki::write_private(&self.files.trust(), st.trust_pem.as_bytes()).map_err(other).and_then(|_| self.reload_tls()) {
                tracing::warn!("installing the CA bundle: {}", e);
            } else {
                tracing::info!("CA trust bundle updated");
            }
        }
        if st.retiring.is_empty() {
            let old = self.files.ca_key().with_extension("key.old");
            if old.exists() {
                let _ = std::fs::remove_file(old);
            }
        }
    }

    /// Step 3 (every node): get a certificate from the signing CA when ours is
    /// from another or close to expiry.
    pub async fn renew_if_needed(&self) -> Result<bool, ClusterError> {
        let cert_pem = self.files.read(self.files.cert())?;
        let der = pki::pem_bundle_ders(&cert_pem)?.into_iter().next().ok_or_else(|| other("no certificate"))?;
        let info = pki::inspect_node_cert(&der)?;
        let trust = self.files.read(self.files.trust())?;
        let signing = self.signing_fp();
        let stale_issuer = self.ca_state().is_some() && pki::issuer_of(&der, &trust).is_some_and(|fp| fp != signing);
        if !stale_issuer && pki::seconds_left(&info) > RENEW_BEFORE_SECS {
            return Ok(false);
        }
        let key = NodeKey::generate()?;
        let csr = key.csr(&[self.identity.advertise.ip()], &[])?;
        let r = self.post_any("/cluster/v1/renew", json!({ "csr": csr }).to_string().into()).await?;
        if !r.status.is_success() {
            return Err(other(format!("renewal refused: {} {}", r.status, String::from_utf8_lossy(&r.body))));
        }
        let v: Value = serde_json::from_slice(&r.body).map_err(other)?;
        let cert = v["cert_pem"].as_str().ok_or_else(|| other("no certificate in the answer"))?;
        let trust = v["trust_pem"].as_str().unwrap_or(&trust);
        self.files.save_node_cert(cert, &key.key_pem(), trust)?;
        self.reload_tls()?;
        tracing::info!("node certificate renewed");
        Ok(true)
    }

    /// POST to the leader, or to a seed when this node has none of its own.
    pub async fn post_any(&self, path: &str, body: bytes::Bytes) -> Result<super::net::Reply, ClusterError> {
        if let Some(l) = self.leader_addr() {
            let _ = l;
            return self.post_leader(path, body).await.map_err(other);
        }
        let mut last = other("no server to ask");
        for seed in self.identity.seeds.clone() {
            let mut addr = seed;
            for _ in 0..2 {
                match self.client.request_timeout(&addr, hyper::Method::POST, path, &[("content-type", "application/json".into())], body.clone(), Duration::from_secs(30)).await {
                    Ok(r) if r.status == hyper::StatusCode::MISDIRECTED_REQUEST => {
                        match serde_json::from_slice::<Value>(&r.body).ok().and_then(|v| v["leader"].as_str().map(String::from)) {
                            Some(l) if l != addr => addr = l,
                            _ => break,
                        }
                    }
                    Ok(r) => return Ok(r),
                    Err(e) => {
                        last = other(e);
                        break;
                    }
                }
            }
        }
        Err(last)
    }

    /// Step 4 and expiry (leader): retire old CAs when every node renewed or
    /// the grace is over; start a rotation a year before the CA expires.
    pub async fn ca_leader_tick(&self) {
        let Some(node) = &self.node else { return };
        if !node.is_leader() {
            return;
        }
        let Some(st) = self.ca_state() else {
            self.maybe_rotate_for_expiry().await;
            return;
        };
        if st.retiring.is_empty() {
            self.maybe_rotate_for_expiry().await;
            return;
        }
        let now = crate::tenancy::now();
        let nodes = NodeStore::new(self.db.clone()).list().unwrap_or_default();
        let current: std::collections::BTreeSet<String> = {
            let Ok(txn) = self.db.begin_read() else { return };
            let Ok(t) = txn.open_table(TableId::IssuedCerts.definition()) else { return };
            t.iter().into_iter().flatten().flatten().filter_map(|(_, v)| serde_json::from_slice::<Value>(v.value()).ok()).filter(|v| v["issuer"] == st.signing_fp.as_str()).filter_map(|v| v["node"].as_str().map(String::from)).collect()
        };
        let pending = nodes.iter().filter(|n| !n.status.phase.is_tombstone()).any(|n| !current.contains(&n.meta.id) && n.meta.id != self.identity.node_id);
        let own_done = self.own_issuer_is(&st.signing_fp);
        if (pending || !own_done) && st.retire_at.is_none_or(|t| now < t) {
            return;
        }
        let mut next = st.clone();
        next.retiring.clear();
        next.retire_at = None;
        // Only the signing CA stays in the bundle.
        if let Some(own) = self.signing_ca() {
            next.trust_pem = own.cert_pem.clone();
        }
        match self.write_state(&next) {
            Ok(()) => tracing::warn!("CA rotation finished: the old CA is no longer trusted"),
            Err(e) => tracing::warn!("retiring the old CA: {}", e),
        }
    }

    fn own_issuer_is(&self, fp: &str) -> bool {
        let (Ok(cert), Ok(trust)) = (self.files.read(self.files.cert()), self.files.read(self.files.trust())) else { return false };
        pki::pem_bundle_ders(&cert).ok().and_then(|d| d.into_iter().next()).and_then(|d| pki::issuer_of(&d, &trust)).is_some_and(|x| x == fp)
    }

    async fn maybe_rotate_for_expiry(&self) {
        let Some(ca) = self.signing_ca() else { return };
        let Ok(der) = pki::pem_bundle_ders(&ca.cert_pem).map(|mut v| v.remove(0)) else { return };
        let Ok((_, c)) = x509_parser::parse_x509_certificate(&der) else { return };
        if c.validity().not_after.timestamp() - (crate::tenancy::now() as i64) < ROTATE_BEFORE_SECS {
            if let Err(e) = self.rotate_ca(None).await {
                tracing::warn!("rotating the CA before it expires: {}", e);
            }
        }
    }

    /// Run on every node: install the bundle, renew certificates; and on the
    /// leader, finish rotations.
    pub fn start_ca_tasks(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3));
            let mut stop = me.stop_rx();
            loop {
                tokio::select! { _ = tick.tick() => {}, _ = stop.changed() => return }
                let c = me.clone();
                let _ = tokio::task::spawn_blocking(move || c.trust_sync()).await;
                if let Err(e) = me.renew_if_needed().await {
                    tracing::debug!("certificate renewal: {}", e);
                }
                me.ca_leader_tick().await;
            }
        });
    }
}

fn pem_of(der: &[u8]) -> String {
    use base64::Engine;
    let b = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = b.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
    format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"))
}
