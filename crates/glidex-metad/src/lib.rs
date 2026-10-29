//! glidex-metad: the metadata endpoint guests reach at
//! `169.254.169.254:80` (spec/29-workload-identity.md W3, W11, §7).
//!
//! netd's rules bring a guest's request here (`glidex_ovs::metadata`), from
//! its VM port's own peer address. metad checks the request, then asks the
//! control plane over `metad.sock`, saying only which peer asked; the
//! control plane finds the VM and signs. metad holds no key, runs
//! unprivileged, and parses guests' HTTP outside the control plane.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// The header a request needs (W3): a guest web application tricked into
/// fetching a URL doesn't send it.
pub const FLAVOR_HEADER: &str = "metadata-flavor";
pub const FLAVOR: &str = "Glidex";
/// Requests a second per VM port, and the burst.
const RATE: f64 = 10.0;

pub struct Config {
    pub listen: SocketAddr,
    /// The control plane's `metad.sock`.
    pub control_plane: PathBuf,
}

/// What a request asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Index,
    Instance,
    Identity { audience: String, ttl: Option<u64> },
    GlidexToken,
}

/// The request's route, or why it is refused (§7).
pub fn route(
    method: &Method,
    path: &str,
    query: Option<&str>,
    headers: &hyper::HeaderMap,
) -> Result<Route, (StatusCode, &'static str)> {
    if method != Method::GET {
        return Err((StatusCode::METHOD_NOT_ALLOWED, "only GET"));
    }
    if headers.contains_key("forwarded") || headers.contains_key("x-forwarded-for") {
        return Err((StatusCode::FORBIDDEN, "a forwarded request is refused"));
    }
    if headers.get(FLAVOR_HEADER).and_then(|v| v.to_str().ok()) != Some(FLAVOR) {
        return Err((
            StatusCode::FORBIDDEN,
            "send the header Metadata-Flavor: Glidex",
        ));
    }
    let q: HashMap<String, String> = form_urlencoded::parse(query.unwrap_or("").as_bytes())
        .into_owned()
        .collect();
    match path.trim_end_matches('/') {
        "" | "/v1" => Ok(Route::Index),
        "/v1/instance" => Ok(Route::Instance),
        "/v1/identity" => {
            let audience = q
                .get("audience")
                .filter(|a| !a.is_empty())
                .ok_or((StatusCode::BAD_REQUEST, "audience is required"))?
                .clone();
            let ttl = match q.get("ttl") {
                None => None,
                Some(t) => Some(
                    t.parse()
                        .map_err(|_| (StatusCode::BAD_REQUEST, "ttl is seconds"))?,
                ),
            };
            Ok(Route::Identity { audience, ttl })
        }
        "/v1/glidex/token" => Ok(Route::GlidexToken),
        _ => Err((StatusCode::NOT_FOUND, "no such endpoint; see /v1/")),
    }
}

/// Per-port token buckets.
#[derive(Default)]
pub struct Limiter {
    buckets: Mutex<HashMap<IpAddr, (Instant, f64)>>,
}

impl Limiter {
    /// Whether `peer` may make one more request now.
    pub fn take(&self, peer: IpAddr) -> bool {
        self.take_at(peer, Instant::now())
    }

    fn take_at(&self, peer: IpAddr, now: Instant) -> bool {
        let mut b = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if b.len() > 65_536 {
            b.clear();
        }
        let (last, tokens) = b.entry(peer).or_insert((now, RATE));
        let refill = now.duration_since(*last).as_secs_f64() * RATE;
        *tokens = (*tokens + refill).min(RATE);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

fn reply(status: StatusCode, content_type: &'static str, body: Bytes) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(body));
    *r.status_mut() = status;
    r.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    r
}

fn error(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    reply(
        status,
        "application/json",
        Bytes::from(json!({ "error": message }).to_string()),
    )
}

/// POST `body` to the control plane's `path` over `sock`.
pub async fn ask_control_plane(
    sock: &Path,
    path: &str,
    body: &Value,
) -> std::io::Result<(StatusCode, Bytes)> {
    let io_err = |e: &dyn std::fmt::Display| std::io::Error::other(e.to_string());
    let stream = tokio::net::UnixStream::connect(sock).await?;
    let (mut send, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|e| io_err(&e))?;
    tokio::spawn(conn);
    let req = Request::post(path)
        .header("host", "glidex-control-plane")
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .map_err(|e| io_err(&e))?;
    let resp = send.send_request(req).await.map_err(|e| io_err(&e))?;
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| io_err(&e))?
        .to_bytes();
    Ok((status, bytes))
}

/// Answer one guest request from `peer`.
pub async fn handle<B>(
    cfg: &Config,
    limiter: &Limiter,
    peer: SocketAddr,
    req: Request<B>,
) -> Response<Full<Bytes>> {
    // Only from a VM port's own peer address: netd's SNAT (§7.1).
    let from_port = match peer.ip() {
        IpAddr::V4(a) => glidex_ovs::metadata::index_of(a).is_some(),
        IpAddr::V6(_) => false,
    };
    if !from_port {
        return error(StatusCode::FORBIDDEN, "not from a VM port");
    }
    if !limiter.take(peer.ip()) {
        return error(StatusCode::TOO_MANY_REQUESTS, "slow down");
    }
    let route = match route(
        req.method(),
        req.uri().path(),
        req.uri().query(),
        req.headers(),
    ) {
        Ok(r) => r,
        Err((s, m)) => return error(s, m),
    };
    let ask = |extra: Value| {
        let mut v = json!({ "peer": peer.ip().to_string() });
        if let (Value::Object(m), Value::Object(e)) = (&mut v, extra) {
            m.extend(e);
        }
        v
    };
    let (path, body) = match &route {
        Route::Index => {
            return reply(
                StatusCode::OK,
                "application/json",
                Bytes::from(
                    json!({ "endpoints": ["/v1/instance", "/v1/identity?audience=…[&ttl=…]", "/v1/glidex/token"] })
                        .to_string(),
                ),
            );
        }
        Route::Instance => ("/v1/instance", ask(json!({}))),
        Route::Identity { audience, ttl } => (
            "/v1/identity",
            ask(json!({ "audience": audience, "ttl": ttl })),
        ),
        Route::GlidexToken => ("/v1/glidex/token", ask(json!({}))),
    };
    match ask_control_plane(&cfg.control_plane, path, &body).await {
        Ok((status, bytes)) if status.is_success() && route_is_identity(&route) => {
            // The token alone, as `application/jwt`.
            let token = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v["token"].as_str().map(str::to_string))
                .unwrap_or_default();
            reply(StatusCode::OK, "application/jwt", Bytes::from(token))
        }
        Ok((status, bytes)) => reply(status, "application/json", bytes),
        Err(e) => {
            tracing::warn!("the control plane: {e}");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "the control plane isn't answering",
            )
        }
    }
}

fn route_is_identity(r: &Route) -> bool {
    matches!(r, Route::Identity { .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> hyper::HeaderMap {
        let mut h = hyper::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn routes_and_refusals() {
        let ok = headers(&[("metadata-flavor", "Glidex")]);
        let get = Method::GET;
        assert_eq!(route(&get, "/v1/", None, &ok), Ok(Route::Index));
        assert_eq!(route(&get, "/v1/instance", None, &ok), Ok(Route::Instance));
        assert_eq!(
            route(
                &get,
                "/v1/identity",
                Some("audience=sts.amazonaws.com&ttl=600"),
                &ok
            ),
            Ok(Route::Identity {
                audience: "sts.amazonaws.com".into(),
                ttl: Some(600)
            })
        );
        assert_eq!(
            route(&get, "/v1/glidex/token", None, &ok),
            Ok(Route::GlidexToken)
        );
        // The header, exactly.
        for h in [headers(&[]), headers(&[("metadata-flavor", "glidex")])] {
            assert_eq!(
                route(&get, "/v1/instance", None, &h).unwrap_err().0,
                StatusCode::FORBIDDEN
            );
        }
        // Forwarded through a proxy in the guest.
        for fwd in ["forwarded", "x-forwarded-for"] {
            let h = headers(&[("metadata-flavor", "Glidex"), (fwd, "10.0.0.1")]);
            assert_eq!(
                route(&get, "/v1/instance", None, &h).unwrap_err().0,
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(
            route(&Method::POST, "/v1/instance", None, &ok)
                .unwrap_err()
                .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            route(&get, "/v1/identity", None, &ok).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            route(&get, "/v1/identity", Some("audience=a&ttl=x"), &ok)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            route(&get, "/v2/x", None, &ok).unwrap_err().0,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn ten_a_second_per_port() {
        let l = Limiter::default();
        let (a, b): (IpAddr, IpAddr) = (
            "169.254.192.1".parse().unwrap(),
            "169.254.192.2".parse().unwrap(),
        );
        let t = Instant::now();
        for _ in 0..10 {
            assert!(l.take_at(a, t));
        }
        assert!(!l.take_at(a, t));
        assert!(l.take_at(b, t), "another port has its own");
        assert!(l.take_at(a, t + std::time::Duration::from_millis(150)));
    }

    #[tokio::test]
    async fn only_vm_ports_and_the_control_plane_answers() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("metad.sock");
        // A control plane that answers an identity token.
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(
                        |req: Request<hyper::body::Incoming>| async move {
                            let path = req.uri().path().to_string();
                            let body = req.into_body().collect().await.unwrap().to_bytes();
                            let asked: Value = serde_json::from_slice(&body).unwrap();
                            assert_eq!(asked["peer"], "169.254.192.3");
                            let out = match path.as_str() {
                                "/v1/identity" => {
                                    json!({ "token": format!("jwt-for-{}", asked["audience"].as_str().unwrap()) })
                                }
                                _ => json!({ "vm_id": "v1" }),
                            };
                            Ok::<_, std::convert::Infallible>(Response::new(Full::new(
                                Bytes::from(out.to_string()),
                            )))
                        },
                    );
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(s), svc)
                        .await;
                });
            }
        });
        let cfg = Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            control_plane: sock,
        };
        let limiter = Limiter::default();
        let get = |uri: &str| {
            Request::get(uri)
                .header("metadata-flavor", "Glidex")
                .body(())
                .unwrap()
        };
        let port: SocketAddr = "169.254.192.3:40000".parse().unwrap();
        let r = handle(&cfg, &limiter, port, get("/v1/identity?audience=vault")).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[CONTENT_TYPE], "application/jwt");
        let body = r.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, "jwt-for-vault");
        // Not a VM port's peer address: refused before the control plane.
        for peer in [
            "10.88.0.5:40000",
            "169.254.169.254:40000",
            "127.0.0.1:40000",
        ] {
            let r = handle(&cfg, &limiter, peer.parse().unwrap(), get("/v1/instance")).await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{peer}");
        }
    }
}
