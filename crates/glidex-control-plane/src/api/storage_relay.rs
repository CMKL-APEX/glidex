//! The storage relay (spec/19-tenancy.md D86): the web UI reads a tenant's
//! storage on a trusted hyperstor cluster through this server, never from
//! the cluster itself.
//!
//! `POST /tenants/{id}/clusters/{cluster}/storage/{call}`, with the
//! browser's session and CSRF value as any call here, forwards one of
//! [`ALLOWED`] to the cluster's API (`api` from the redeem, its certificate
//! pinned as the trust pins it) with an access token for the session's
//! person (`aud` the cluster's id, D55, D69), minted here and never sent to
//! the browser. The cluster answers under that person's grants; its
//! answer, errors included, comes back as it is, except that its `401`
//! comes back as `403` (a `401` here means this session ended).

use super::{ApiErr, Caller, err};
use crate::trust::TrustedCluster;
use axum::{
    body::{Body, Bytes},
    extract::Path,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The calls the relay forwards: `(call, method at the cluster, path
/// there)`. Reads of what the person's grants reach, nothing else.
pub const ALLOWED: [(&str, &str, &str); 5] = [
    (
        "hyperstor.v1.ClusterService/ListVolumes",
        "POST",
        "/api/hyperstor.v1.ClusterService/ListVolumes",
    ),
    (
        "hyperstor.v1.S3Service/ListS3Buckets",
        "POST",
        "/api/hyperstor.v1.S3Service/ListS3Buckets",
    ),
    (
        "hyperstor.v1.S3Service/ListS3Keys",
        "POST",
        "/api/hyperstor.v1.S3Service/ListS3Keys",
    ),
    (
        "hyperstor.v1.TenantService/ListTenants",
        "POST",
        "/api/hyperstor.v1.TenantService/ListTenants",
    ),
    ("auth/whoami", "GET", "/auth/whoami"),
];
/// The largest request body forwarded.
pub const MAX_REQUEST: usize = 64 << 10;
/// The largest answer taken from the cluster.
pub const MAX_ANSWER: usize = 8 << 20;
/// How long a relayed call may take.
pub const TIMEOUT: Duration = Duration::from_secs(15);
/// A cached token is used while it has this long left.
const TOKEN_MARGIN_SECS: u64 = 60;

/// Tokens by (session, cluster), with when they expire: one per session and
/// cluster until near expiry.
type Tokens = Mutex<HashMap<(String, String), (String, u64)>>;

fn tokens() -> &'static Tokens {
    static T: OnceLock<Tokens> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// An access token for the caller's session at cluster `c`: a cached one,
/// or a new one.
fn token_for(c: &Caller, cluster: &TrustedCluster) -> Result<String, ApiErr> {
    let now = crate::auth::store::now();
    let key = (
        c.p.session_hash.clone().unwrap_or_default(),
        format!("{}/{}", cluster.directory_id, cluster.cluster_id),
    );
    let mut t = tokens().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((tok, exp)) = t.get(&key)
        && *exp > now + TOKEN_MARGIN_SECS
    {
        return Ok(tok.clone());
    }
    let (tok, exp) = super::op::ui_cluster_token(&c.app, cluster, &c.p)?;
    t.retain(|_, (_, e)| *e > now);
    t.insert(key, (tok.clone(), exp));
    Ok(tok)
}

pub async fn relay(
    c: Caller,
    Path((id, cluster, call)): Path<(String, String, String)>,
    body: Bytes,
) -> Result<Response, ApiErr> {
    let Some(&(_, method, path)) = ALLOWED.iter().find(|(name, _, _)| *name == call) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            "not_relayed",
            format!("the storage relay doesn't forward {call}"),
        ));
    };
    if body.len() > MAX_REQUEST {
        return Err(err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!("at most {MAX_REQUEST} bytes"),
        ));
    }
    let target = crate::service::tenancy::relayed_cluster(&c, &id, &cluster)?;
    c.set_target(format!("cluster:{}", target.cluster_id));
    let token = token_for(&c, &target)?;
    let body = if method == "GET" { Bytes::new() } else { body };
    let (status, answer) = forward(&target.api, &target.tls_sha256, method, path, &token, body)
        .await
        .map_err(|e| {
            tracing::info!(cluster = %target.cluster_id, "storage relay: {e}");
            err(
                StatusCode::BAD_GATEWAY,
                "cluster_unreachable",
                format!("cluster {} didn't answer: {e}", target.name),
            )
        })?;
    // The cluster refusing the token isn't this session ending.
    let status = if status == StatusCode::UNAUTHORIZED {
        StatusCode::FORBIDDEN
    } else {
        status
    };
    let mut r = (status, Body::from(answer)).into_response();
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(r)
}

/// One call to the cluster at `base`, its certificate pinned to `pin`:
/// its status and body.
pub async fn forward(
    base: &str,
    pin: &str,
    method: &str,
    path: &str,
    token: &str,
    body: Bytes,
) -> Result<(StatusCode, Bytes), String> {
    let uri: http::Uri = base.parse().map_err(|e| format!("{base}: {e}"))?;
    if uri.scheme_str() != Some("https") {
        return Err(format!("{base}: not an https address"));
    }
    let host = uri
        .host()
        .ok_or("no host")?
        .trim_matches(['[', ']'])
        .to_string();
    let port = uri.port_u16().unwrap_or(443);
    let pin = glidex_tls::Trust::normalize_pin(pin).ok_or("no pinned certificate")?;
    let tls = glidex_tls::ClientTls::with_trust(
        &glidex_tls::Trust {
            pins: vec![pin],
            // As glidex-trust's calls to the cluster: the pin is its
            // identity, whatever names its certificate lists.
            verify_host: false,
            ..Default::default()
        },
        &[b"http/1.1"],
    )?;
    let run = async {
        let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .map_err(|e| format!("{host}:{port}: {e}"))?;
        let name =
            rustls_pki_types::ServerName::try_from(host.clone()).map_err(|e| e.to_string())?;
        let s = tokio_rustls::TlsConnector::from(tls.config.clone())
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS: {e}"))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(s))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = hyper::Request::builder()
            .method(method)
            .uri(format!("{}{path}", base.trim_end_matches('/')))
            .header(
                header::HOST,
                uri.authority().map(|a| a.as_str()).unwrap_or(""),
            )
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("connect-protocol-version", "1")
            .body(Full::new(body))
            .map_err(|e| e.to_string())?;
        let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let bytes = Limited::new(resp.into_body(), MAX_ANSWER)
            .collect()
            .await
            .map_err(|e| format!("its answer: {e}"))?
            .to_bytes();
        Ok::<_, String>((status, bytes))
    };
    tokio::time::timeout(TIMEOUT, run)
        .await
        .map_err(|_| "no answer in time".to_string())?
}
