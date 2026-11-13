//! A cluster's calls to the directories it trusts: the feed (D67), reports
//! (D70), ending trust (D66), logouts (D72), and a browser login's code
//! exchange (D69). Each is signed with the cluster key and pins the
//! directory's certificate.

use crate::key::Signer;
use crate::peer;
use crate::wire::{self, Snapshot, TrustedDirectory};
use serde::Serialize;
use serde_json::{Value, json};

/// The client id a cluster has at its trusted directories (D54).
pub const CLUSTER_CLIENT: &str = "cluster:";

/// Cluster `cluster_id`'s client id at a trusted directory.
pub fn client_id(cluster_id: &str) -> String {
    format!("{CLUSTER_CLIENT}{cluster_id}")
}

/// What a feed request brought.
#[derive(Debug, Clone, PartialEq)]
pub enum Feed {
    /// The revision asked with is current.
    Unchanged,
    /// A new snapshot, its revision checked against its content. Its rows
    /// are the directory's: the cluster still checks they are of the
    /// tenants it admits before applying them (D68).
    Changed(Snapshot),
}

/// Pull trusted directory `d`'s feed for cluster `cluster_id`, which last
/// applied `revision` (D67).
pub async fn pull_feed(
    key: &dyn Signer,
    cluster_id: &str,
    d: &TrustedDirectory,
    revision: &str,
    now: u64,
) -> Result<Feed, String> {
    let a = peer::assertion(key, cluster_id, &d.issuer, wire::FEED, now)?;
    let (status, v) = peer::post(
        &d.api,
        &d.tls_sha256,
        &wire::feed_path(&d.directory),
        &a,
        &json!({ "cluster_id": cluster_id, "revision": revision }),
    )
    .await?;
    match status {
        304 => Ok(Feed::Unchanged),
        200 => {
            let snap: Snapshot = serde_json::from_value(v).map_err(|e| e.to_string())?;
            if wire::revision_of(&snap.jwks, &snap.rows, &snap.ended_sids) != snap.revision {
                return Err("a feed whose revision doesn't match its content".into());
            }
            Ok(Feed::Changed(snap))
        }
        s => Err(format!("the directory answered {s}")),
    }
}

/// Send trusted directory `d` cluster `cluster_id`'s report (D70), with
/// its web UI callback as it is now (D73).
pub async fn push_report<R: Serialize>(
    key: &dyn Signer,
    cluster_id: &str,
    d: &TrustedDirectory,
    report: &R,
    ui_callback: Option<&str>,
    now: u64,
) -> Result<(), String> {
    let a = peer::assertion(key, cluster_id, &d.issuer, wire::REPORT, now)?;
    let body = json!({ "cluster_id": cluster_id, "report": report, "ui_callback": ui_callback });
    match peer::post(
        &d.api,
        &d.tls_sha256,
        &wire::report_path(&d.directory),
        &a,
        &body,
    )
    .await?
    {
        (204, _) | (200, _) => Ok(()),
        (s, _) => Err(format!("the directory answered {s}")),
    }
}

/// Tell trusted directory `d` that cluster `cluster_id` no longer admits
/// `tenant` (D66). The directory's status; it also finds out when it is
/// next refused.
pub async fn end_trust(
    key: &dyn Signer,
    cluster_id: &str,
    d: &TrustedDirectory,
    tenant: &str,
    now: u64,
) -> Result<u16, String> {
    let a = peer::assertion(key, cluster_id, &d.issuer, wire::END, now)?;
    let (s, _) = peer::post(
        &d.api,
        &d.tls_sha256,
        &wire::end_path(&d.directory),
        &a,
        &json!({ "cluster_id": cluster_id, "tenant": tenant }),
    )
    .await?;
    Ok(s)
}

/// Tell trusted directory `d` that cluster `cluster_id`'s session from
/// its chain `sid` ended, so the chain ends there too (D72).
pub async fn logout(
    key: &dyn Signer,
    cluster_id: &str,
    d: &TrustedDirectory,
    sid: &str,
    now: u64,
) -> Result<(), String> {
    let a = peer::assertion(key, cluster_id, &d.issuer, wire::LOGOUT, now)?;
    match peer::post(
        &d.api,
        &d.tls_sha256,
        &wire::logout_path(&d.directory),
        &a,
        &json!({ "cluster_id": cluster_id, "sid": sid }),
    )
    .await?
    {
        (204, _) | (200, _) => Ok(()),
        (s, _) => Err(format!("the directory answered {s}")),
    }
}

/// PKCE's S256 challenge for `verifier` (RFC 7636 §4.2).
pub fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier))
}

/// Where a browser goes to log in to cluster `cluster_id` at trusted
/// directory `d` (D69): its `authorize`, with PKCE and the callback the
/// cluster registered at the handshake.
pub fn authorize_url(
    d: &TrustedDirectory,
    cluster_id: &str,
    callback: &str,
    state: &str,
    nonce: &str,
    verifier: &str,
) -> Result<String, String> {
    let mut url = url::Url::parse(&format!("{}/authorize", d.issuer)).map_err(|e| e.to_string())?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id(cluster_id))
        .append_pair("redirect_uri", callback)
        .append_pair("scope", "openid")
        .append_pair("state", state)
        .append_pair("nonce", nonce)
        .append_pair("code_challenge", &pkce_challenge(verifier))
        .append_pair("code_challenge_method", "S256");
    Ok(url.into())
}

/// Exchange a browser login's `code` at trusted directory `d`'s token
/// endpoint, as cluster `cluster_id`'s client, authenticated with a
/// client assertion signed by `key` (`private_key_jwt`, D69). The status
/// and the directory's answer (an `id_token` on 200; check it with
/// [`crate::tokens::verify_id_token`]).
pub async fn exchange_code(
    key: &dyn Signer,
    cluster_id: &str,
    d: &TrustedDirectory,
    code: &str,
    callback: &str,
    verifier: &str,
    now: u64,
) -> Result<(u16, Value), String> {
    let client = client_id(cluster_id);
    let assertion = peer::client_assertion(key, &client, &d.issuer, now)?;
    peer::post_unsigned(
        &d.api,
        &d.tls_sha256,
        &wire::token_path(&d.directory),
        &json!({
            "grant_type": "authorization_code", "code": code, "redirect_uri": callback,
            "client_id": client, "code_verifier": verifier,
            "client_assertion_type": peer::CLIENT_ASSERTION_TYPE, "client_assertion": assertion,
        }),
    )
    .await
}
