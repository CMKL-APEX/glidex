//! A directory in-process, for the tests of a cluster that trusts one
//! (feature `fake`): HTTPS on loopback with a self-signed certificate,
//! its own signing key, and the directory's side of the handshake.
//!
//! - It **redeems** a cluster's offer ([`FakeDirectory::redeem`]) and
//!   **ends** trust ([`FakeDirectory::end_trust`]), as a directory's
//!   tenant administrator would.
//! - It **serves** the feed of the rows a test puts in it, takes reports,
//!   logouts and ended trust, and runs a browser login's `authorize` and
//!   `token` for the person a test names ([`FakeDirectory::login_as`]).
//! - It **issues** access tokens addressed to a cluster
//!   ([`FakeDirectory::access_token`]) and API tokens in its feed.
//!
//! It checks what a real directory checks of a cluster (its assertions
//! against the key it pinned at the redeem, its callback, PKCE), but
//! serves its rows as they are: a test may hand a cluster rows outside
//! its tenants to see them refused. [`FakeDirectory::set_down`] stands
//! for a partition.

use crate::feed::Entity;
use crate::key::{ClusterKey, Signer, jwks};
use crate::peer::{self, Replays};
use crate::wire::{
    self, EndAtCluster, OfferToken, RedeemAnswer, RedeemRequest, RemoteTenant, Row, Snapshot,
    normalize_issuer,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// What a handler of [`serve_https`] answers.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    /// A redirect's target.
    pub location: Option<String>,
    pub body: Option<Value>,
}

impl Reply {
    pub fn json(status: u16, body: Value) -> Reply {
        Reply {
            status,
            location: None,
            body: Some(body),
        }
    }

    pub fn status(status: u16) -> Reply {
        Reply {
            status,
            location: None,
            body: None,
        }
    }

    pub fn redirect(to: String) -> Reply {
        Reply {
            status: 303,
            location: Some(to),
            body: None,
        }
    }
}

/// A request as a handler of [`serve_https`] sees it.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: http::Method,
    /// The path, without the query.
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: http::HeaderMap,
    pub body: Bytes,
}

impl Request {
    /// The body as JSON, or as a form, field by field.
    pub fn fields(&self) -> HashMap<String, String> {
        if let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(&self.body) {
            return m
                .into_iter()
                .filter_map(|(k, v)| Some((k, v.as_str()?.to_string())))
                .collect();
        }
        url::form_urlencoded::parse(&self.body)
            .into_owned()
            .collect()
    }
}

/// A handler of [`serve_https`].
pub type Handler =
    Arc<dyn Fn(Request) -> Pin<Box<dyn Future<Output = Reply> + Send>> + Send + Sync>;

/// A server [`serve_https`] started: its address and certificate pin. It
/// stops when dropped.
pub struct Https {
    /// `https://127.0.0.1:<port>`.
    pub base: String,
    /// Its certificate's SHA-256, as offers and pins write it.
    pub pin: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Https {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// HTTPS on a free loopback port with a new self-signed certificate,
/// answering with `handler`: a test's stand-in for either side.
pub async fn serve_https(handler: Handler) -> Result<Https, String> {
    let names = glidex_tls::LocalNames {
        dns: vec!["localhost".into()],
        ips: vec![std::net::Ipv4Addr::LOCALHOST.into()],
    };
    let (cert, key) = glidex_tls::generate(&names)?;
    let ck = glidex_tls::certified_key_pem(format!("{cert}{key}").as_bytes())?;
    let pin = glidex_tls::Trust::normalize_pin(&glidex_tls::fingerprint_certified(&ck))
        .ok_or("the certificate's fingerprint")?;
    let cfg = glidex_tls::resolver_config(
        glidex_tls::Resolver::new(glidex_tls::CertSlot::new(ck)),
        &[b"http/1.1"],
    )?;
    let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                continue;
            };
            let (acceptor, handler) = (acceptor.clone(), handler.clone());
            tokio::spawn(async move {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| {
                        let handler = handler.clone();
                        async move {
                            let (parts, body) = req.into_parts();
                            let body = body
                                .collect()
                                .await
                                .map(|b| b.to_bytes())
                                .unwrap_or_default();
                            let query = parts
                                .uri
                                .query()
                                .map(|q| {
                                    url::form_urlencoded::parse(q.as_bytes())
                                        .into_owned()
                                        .collect()
                                })
                                .unwrap_or_default();
                            let r = handler(Request {
                                method: parts.method,
                                path: parts.uri.path().to_string(),
                                query,
                                headers: parts.headers,
                                body,
                            })
                            .await;
                            let mut resp = hyper::Response::builder().status(r.status);
                            if let Some(l) = r.location {
                                resp = resp.header(http::header::LOCATION, l);
                            }
                            let body = match r.body {
                                Some(v) => {
                                    resp =
                                        resp.header(http::header::CONTENT_TYPE, "application/json");
                                    Bytes::from(v.to_string())
                                }
                                None => Bytes::new(),
                            };
                            Ok::<_, std::convert::Infallible>(
                                resp.body(Full::new(body)).expect("a response"),
                            )
                        }
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), svc)
                    .await;
            });
        }
    });
    Ok(Https {
        base: format!("https://127.0.0.1:{port}"),
        pin,
        task,
    })
}

/// A cluster that redeemed one of its offers, as the fake directory
/// knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct FakeCluster {
    pub cluster_id: String,
    pub api: String,
    pub tls_sha256: String,
    /// Its key, as the redeem's answer brought it.
    pub jwks: Value,
    /// The tenant ids opted in to it.
    pub tenants: Vec<String>,
    pub ui_callback: Option<String>,
    /// Its last report, as it sent it.
    pub report: Option<Value>,
}

struct Code {
    cluster: String,
    user: String,
    mfa: bool,
    nonce: Option<String>,
    challenge: String,
    redirect_uri: String,
}

#[derive(Default)]
struct State {
    clusters: HashMap<String, FakeCluster>,
    rows: Vec<Row>,
    ended_sids: Vec<String>,
    codes: HashMap<String, Code>,
    login: Option<(String, bool)>,
    down: bool,
    logouts: Vec<(String, String)>,
    feed_pulls: u64,
}

struct Inner {
    name: String,
    issuer: String,
    key: ClusterKey,
    replays: Replays,
    state: Mutex<State>,
}

/// A fake directory; it stops when dropped.
pub struct FakeDirectory {
    inner: Arc<Inner>,
    https: Https,
}

/// How long the access tokens it issues live.
pub const ACCESS_SECS: u64 = 600;

impl FakeDirectory {
    /// A directory named `name` (its id at its host) on a free loopback
    /// port: issuer `https://127.0.0.1:<port>/d/<name>`.
    pub async fn start(name: &str) -> Result<FakeDirectory, String> {
        // The issuer names the port, known once listening.
        let slot: Arc<Mutex<Option<Arc<Inner>>>> = Default::default();
        let s2 = slot.clone();
        let handler: Handler = Arc::new(move |req| {
            let inner = s2.lock().unwrap().clone();
            Box::pin(async move {
                match inner {
                    Some(i) => i.handle(req),
                    None => Reply::status(503),
                }
            })
        });
        let https = serve_https(handler).await?;
        let inner = Arc::new(Inner {
            name: name.into(),
            issuer: format!("{}/d/{name}", https.base),
            key: ClusterKey::generate(&format!("{name}:1"))?,
            replays: Replays::default(),
            state: Mutex::default(),
        });
        *slot.lock().unwrap() = Some(inner.clone());
        Ok(FakeDirectory { inner, https })
    }

    /// Its id at its own host.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn issuer(&self) -> &str {
        &self.inner.issuer
    }

    /// Its API, `https://127.0.0.1:<port>`.
    pub fn api(&self) -> &str {
        &self.https.base
    }

    /// Its certificate's SHA-256.
    pub fn tls_sha256(&self) -> &str {
        &self.https.pin
    }

    /// Its signing keys (a JWKS).
    pub fn jwks(&self) -> Value {
        jwks(&[&self.inner.key])
    }

    /// `claims` signed with its key, `iss` set: for a test's own tokens.
    pub fn sign(&self, typ: &str, mut claims: Value) -> Result<String, String> {
        claims["iss"] = self.inner.issuer.clone().into();
        self.inner.key.sign_jwt(typ, &claims)
    }

    /// Redeem cluster offer `offer` (`gxo_…`) for `tenant`, as the
    /// tenant's administrator does (D65). The cluster's answer.
    pub async fn redeem(&self, offer: &str, tenant: &RemoteTenant) -> Result<RedeemAnswer, String> {
        let t = OfferToken::decode(offer).ok_or("not an offer token")?;
        if normalize_issuer(&t.directory).as_deref() != Some(self.inner.issuer.as_str()) {
            return Err(format!("the offer is for directory {}", t.directory));
        }
        if t.tenant != tenant.slug {
            return Err(format!("the offer is for tenant {}", t.tenant));
        }
        let now = crate::now();
        let a = peer::assertion(
            &self.inner.key,
            &self.inner.issuer,
            &t.cluster_id,
            wire::REDEEM,
            now,
        )?;
        let body = RedeemRequest {
            offer: t.offer.clone(),
            secret: t.secret.clone(),
            issuer: self.inner.issuer.clone(),
            directory: self.inner.name.clone(),
            tenant: tenant.clone(),
            jwks: self.jwks(),
            api: self.https.base.clone(),
            tls_sha256: self.https.pin.clone(),
        };
        let (status, v) = peer::post(
            &t.api,
            &t.tls_sha256,
            wire::REDEEM_PATH,
            &a,
            &serde_json::to_value(&body).map_err(|e| e.to_string())?,
        )
        .await?;
        let answer: RedeemAnswer = match status {
            200 => serde_json::from_value(v).map_err(|e| e.to_string())?,
            s => return Err(format!("the cluster refused the offer ({s})")),
        };
        if answer.cluster_id != t.cluster_id {
            return Err("the cluster answered as another".into());
        }
        let mut st = self.inner.state.lock().unwrap();
        let c = st
            .clusters
            .entry(t.cluster_id.clone())
            .or_insert_with(|| FakeCluster {
                cluster_id: t.cluster_id.clone(),
                api: t.api.clone(),
                tls_sha256: t.tls_sha256.clone(),
                jwks: Value::Null,
                tenants: Vec::new(),
                ui_callback: None,
                report: None,
            });
        c.jwks = answer.cluster_key.clone();
        c.ui_callback = answer
            .ui_callback
            .clone()
            .filter(|u| wire::valid_callback(u));
        if !c.tenants.contains(&tenant.id) {
            c.tenants.push(tenant.id.clone());
        }
        Ok(answer)
    }

    /// Tenant `tenant` stops using cluster `cluster_id` (D66): forgotten
    /// here, and the cluster told. The cluster's status.
    pub async fn end_trust(&self, cluster_id: &str, tenant: &str) -> Result<u16, String> {
        let c = {
            let mut st = self.inner.state.lock().unwrap();
            let c = st.clusters.get_mut(cluster_id).ok_or("no such cluster")?;
            c.tenants.retain(|t| t != tenant);
            let c = c.clone();
            if c.tenants.is_empty() {
                st.clusters.remove(cluster_id);
            }
            c
        };
        let a = peer::assertion(
            &self.inner.key,
            &self.inner.issuer,
            cluster_id,
            wire::END,
            crate::now(),
        )?;
        let body = EndAtCluster {
            issuer: self.inner.issuer.clone(),
            tenant: tenant.into(),
        };
        let (s, _) = peer::post(
            &c.api,
            &c.tls_sha256,
            wire::END_PATH,
            &a,
            &serde_json::to_value(&body).map_err(|e| e.to_string())?,
        )
        .await?;
        Ok(s)
    }

    /// Cluster `id` as this directory knows it: its tenants, callback and
    /// last report.
    pub fn cluster(&self, id: &str) -> Option<FakeCluster> {
        self.inner.state.lock().unwrap().clusters.get(id).cloned()
    }

    // ---- the feed's rows --------------------------------------------------------

    /// Replace every row the feed serves.
    pub fn set_rows(&self, rows: Vec<Row>) {
        self.inner.state.lock().unwrap().rows = rows;
    }

    /// Put row `(table, key)`, replacing one there.
    pub fn put_row(&self, table: &str, key: &str, value: Value) {
        let mut st = self.inner.state.lock().unwrap();
        st.rows.retain(|r| !(r.table == table && r.key == key));
        st.rows.push(Row {
            table: table.into(),
            key: key.into(),
            value,
        });
        st.rows
            .sort_by(|a, b| (&a.table, &a.key).cmp(&(&b.table, &b.key)));
    }

    pub fn remove_row(&self, table: &str, key: &str) {
        self.inner
            .state
            .lock()
            .unwrap()
            .rows
            .retain(|r| !(r.table == table && r.key == key));
    }

    /// A tenant, as glidex writes one.
    pub fn add_tenant(&self, id: &str, slug: &str) -> RemoteTenant {
        self.put_row(
            "tenants",
            id,
            json!({ "id": id, "directory_id": self.inner.name, "slug": slug, "display_name": slug,
                    "created_at": 0 }),
        );
        RemoteTenant {
            id: id.into(),
            slug: slug.into(),
            display_name: slug.into(),
        }
    }

    /// A user; `disabled` as their IdP left them.
    pub fn add_user(&self, id: &str, display_name: &str, disabled: bool) {
        self.put_row(
            "users",
            id,
            json!({ "id": id, "directory_id": self.inner.name, "display_name": display_name,
                    "disabled": disabled, "created_at": 0 }),
        );
    }

    pub fn add_project(&self, id: &str, tenant: &str, name: &str) {
        self.put_row(
            "projects",
            id,
            json!({ "id": id, "name": name, "tenant_id": tenant, "description": "", "created_at": 0 }),
        );
    }

    /// `user` a member of `tenant`.
    pub fn add_member(&self, tenant: &str, user: &str) {
        self.put_row(
            "tenant_members",
            &format!("{tenant}/user/{user}"),
            json!({ "tenant_id": tenant, "principal": { "type": "User", "id": user }, "by": "fake", "at": 0 }),
        );
    }

    /// `principal` holds role `template` (`role.tenant-admin`,
    /// `role.viewer`, …) on `resource`; the link's id.
    pub fn link(&self, principal: Entity, resource: Entity, template: &str) -> String {
        let id = crate::random_token(8);
        self.put_row(
            "policy_links",
            &id,
            json!({ "id": id, "template": template, "principal": principal, "resource": resource,
                    "created_by": "fake", "created_at": 0 }),
        );
        id
    }

    /// A personal API token of `owner` good on `cluster_id`, in the feed by
    /// its hash; its secret (`gxt_…`).
    pub fn add_api_token(&self, owner: &str, cluster_id: &str, expires_at: u64) -> String {
        let secret = format!(
            "{}{}",
            crate::tokens::API_TOKEN_PREFIX,
            crate::random_token(24)
        );
        let id = crate::random_token(8);
        self.put_row(
            "api_tokens",
            &crate::tokens::api_token_hash(&secret),
            json!({ "id": id, "name": "fake", "kind": "personal", "owner": owner,
                    "created_by": owner, "created_at": 0, "expires_at": expires_at,
                    "clusters": [cluster_id] }),
        );
        secret
    }

    /// The feed tells clusters that login chain `sid` ended (D68).
    pub fn end_chain(&self, sid: &str) {
        self.inner.state.lock().unwrap().ended_sids.push(sid.into());
    }

    // ---- logins and tokens ------------------------------------------------------

    /// Who `authorize` logs in from now on (`None`: nobody: it refuses),
    /// and whether with a second factor.
    pub fn login_as(&self, user: Option<&str>, mfa: bool) {
        self.inner.state.lock().unwrap().login = user.map(|u| (u.to_string(), mfa));
    }

    /// An access token of `user` addressed to cluster `cluster_id`, from
    /// login chain `sid` (D69).
    pub fn access_token(&self, user: &str, cluster_id: &str, sid: &str) -> Result<String, String> {
        let t = crate::now();
        self.sign(
            crate::tokens::ACCESS_TYP,
            json!({
                "sub": user, "aud": cluster_id, "client_id": "gxctl", "iat": t, "nbf": t,
                "exp": t + ACCESS_SECS, "jti": crate::random_token(16), "sid": sid,
                "auth_time": t, "amr": ["pwd"], "scope": "openid",
                "glidex": { "directory": self.inner.name, "mfa": false },
            }),
        )
    }

    /// A browser's trip through `authorize_url` (what
    /// [`crate::client::authorize_url`] made), without a browser: where
    /// the directory sends it back, the cluster's callback with a `code`
    /// (or an `error`) and the `state`.
    pub fn browse(&self, authorize_url: &str) -> Result<String, String> {
        let u = url::Url::parse(authorize_url).map_err(|e| e.to_string())?;
        if !u
            .as_str()
            .starts_with(&format!("{}/authorize", self.inner.issuer))
        {
            return Err(format!("not this directory's authorize: {u}"));
        }
        let q: HashMap<String, String> = u.query_pairs().into_owned().collect();
        let r = self.inner.authorize(&q);
        r.location
            .ok_or_else(|| format!("authorize answered {}: {:?}", r.status, r.body))
    }

    // ---- what clusters did ------------------------------------------------------

    /// Answer nothing but 503 while `down`: a partition.
    pub fn set_down(&self, down: bool) {
        self.inner.state.lock().unwrap().down = down;
    }

    /// The `(cluster, sid)` of each logout clusters sent (D72).
    pub fn logouts(&self) -> Vec<(String, String)> {
        self.inner.state.lock().unwrap().logouts.clone()
    }

    /// How many feed requests it answered.
    pub fn feed_pulls(&self) -> u64 {
        self.inner.state.lock().unwrap().feed_pulls
    }
}

impl Inner {
    fn handle(&self, req: Request) -> Reply {
        if self.state.lock().unwrap().down {
            return Reply::status(503);
        }
        let Some(rest) = req.path.strip_prefix(&format!("/d/{}", self.name)) else {
            return Reply::status(404);
        };
        match (req.method.as_str(), rest) {
            ("GET", "/.well-known/openid-configuration") => Reply::json(
                200,
                json!({
                    "issuer": self.issuer,
                    "authorization_endpoint": format!("{}/authorize", self.issuer),
                    "token_endpoint": format!("{}/token", self.issuer),
                    "jwks_uri": format!("{}/jwks.json", self.issuer),
                    "id_token_signing_alg_values_supported": ["ES256"],
                }),
            ),
            ("GET", "/jwks.json") => Reply::json(200, jwks(&[&self.key])),
            ("GET", "/authorize") => self.authorize(&req.query),
            ("POST", "/token") => self.token(&req.fields()),
            ("POST", "/feed/v1") => self.peer(&req, wire::FEED, |me, c, b| me.feed(c, b)),
            ("POST", "/feed/v1/report") => self.peer(&req, wire::REPORT, |me, c, b| {
                let mut st = me.state.lock().unwrap();
                if let Some(c) = st.clusters.get_mut(c) {
                    c.report = Some(b["report"].clone());
                    if let Some(u) = b["ui_callback"]
                        .as_str()
                        .filter(|u| wire::valid_callback(u))
                    {
                        c.ui_callback = Some(u.into());
                    }
                }
                Reply::status(204)
            }),
            ("POST", "/trust/v1/end") => self.peer(&req, wire::END, |me, c, b| {
                let mut st = me.state.lock().unwrap();
                if let Some(x) = st.clusters.get_mut(c) {
                    x.tenants
                        .retain(|t| Some(t.as_str()) != b["tenant"].as_str());
                    if x.tenants.is_empty() {
                        st.clusters.remove(c);
                    }
                }
                Reply::status(200)
            }),
            ("POST", "/trust/v1/logout") => self.peer(&req, wire::LOGOUT, |me, c, b| {
                let sid = b["sid"].as_str().unwrap_or_default().to_string();
                me.state.lock().unwrap().logouts.push((c.to_string(), sid));
                Reply::status(204)
            }),
            _ => Reply::status(404),
        }
    }

    /// A cluster's peer request for `purpose`: its assertion checked
    /// against the key it gave at the redeem.
    fn peer(
        &self,
        req: &Request,
        purpose: &str,
        then: impl FnOnce(&Inner, &str, &Value) -> Reply,
    ) -> Reply {
        let refused = || Reply::json(403, json!({ "code": "trust_refused" }));
        let b: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let Some(cluster) = b["cluster_id"].as_str() else {
            return refused();
        };
        let Some(jwks) = self
            .state
            .lock()
            .unwrap()
            .clusters
            .get(cluster)
            .map(|c| c.jwks.clone())
        else {
            return refused();
        };
        let Some(a) = peer::from_headers(&req.headers) else {
            return refused();
        };
        if peer::verify(
            &a,
            &jwks,
            cluster,
            &self.issuer,
            purpose,
            &self.replays,
            crate::now(),
        )
        .is_err()
        {
            return refused();
        }
        then(self, cluster, &b)
    }

    fn feed(&self, _cluster: &str, b: &Value) -> Reply {
        let mut st = self.state.lock().unwrap();
        st.feed_pulls += 1;
        let jwks = jwks(&[&self.key]);
        let snap = Snapshot {
            revision: wire::revision_of(&jwks, &st.rows, &st.ended_sids),
            jwks,
            rows: st.rows.clone(),
            ended_sids: st.ended_sids.clone(),
        };
        if b["revision"].as_str() == Some(snap.revision.as_str()) {
            return Reply::status(304);
        }
        Reply::json(200, serde_json::to_value(snap).unwrap_or_default())
    }

    fn authorize(&self, q: &HashMap<String, String>) -> Reply {
        let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
        let bad = |e: &str| Reply::json(400, json!({ "error": e }));
        let Some(cluster) = get("client_id").strip_prefix(crate::client::CLUSTER_CLIENT) else {
            return bad("invalid_client");
        };
        let mut st = self.state.lock().unwrap();
        let Some(c) = st.clusters.get(cluster) else {
            return bad("invalid_client");
        };
        if c.ui_callback.as_deref() != Some(get("redirect_uri")) {
            return bad("invalid_request");
        }
        if get("response_type") != "code" || get("code_challenge_method") != "S256" {
            return bad("invalid_request");
        }
        let redirect = |pairs: &[(&str, &str)]| {
            let mut u = url::Url::parse(get("redirect_uri")).expect("checked");
            u.query_pairs_mut().extend_pairs(pairs);
            Reply::redirect(u.into())
        };
        let Some((user, mfa)) = st.login.clone() else {
            return redirect(&[("error", "access_denied"), ("state", get("state"))]);
        };
        let code = crate::random_token(24);
        st.codes.insert(
            code.clone(),
            Code {
                cluster: cluster.into(),
                user,
                mfa,
                nonce: q.get("nonce").cloned(),
                challenge: get("code_challenge").into(),
                redirect_uri: get("redirect_uri").into(),
            },
        );
        redirect(&[("code", &code), ("state", get("state"))])
    }

    fn token(&self, f: &HashMap<String, String>) -> Reply {
        let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
        let err = |s: u16, e: &str| Reply::json(s, json!({ "error": e, "error_description": e }));
        let client = get("client_id");
        let Some(cluster) = client.strip_prefix(crate::client::CLUSTER_CLIENT) else {
            return err(401, "invalid_client");
        };
        let Some(jwks) = self
            .state
            .lock()
            .unwrap()
            .clusters
            .get(cluster)
            .map(|c| c.jwks.clone())
        else {
            return err(401, "invalid_client");
        };
        let now = crate::now();
        if get("client_assertion_type") != peer::CLIENT_ASSERTION_TYPE
            || peer::verify_client_assertion(
                get("client_assertion"),
                &jwks,
                client,
                &self.issuer,
                &self.replays,
                now,
            )
            .is_err()
        {
            return err(401, "invalid_client");
        }
        if get("grant_type") != "authorization_code" {
            return err(400, "unsupported_grant_type");
        }
        let Some(code) = self.state.lock().unwrap().codes.remove(get("code")) else {
            return err(400, "invalid_grant");
        };
        if code.cluster != cluster
            || code.redirect_uri != get("redirect_uri")
            || crate::client::pkce_challenge(get("code_verifier")) != code.challenge
        {
            return err(400, "invalid_grant");
        }
        let sid = crate::random_token(16);
        let amr: Vec<&str> = if code.mfa {
            vec!["pwd", "mfa"]
        } else {
            vec!["pwd"]
        };
        let mut id = json!({
            "iss": self.issuer, "sub": code.user, "aud": client, "iat": now, "exp": now + 300,
            "auth_time": now, "amr": amr, "sid": sid,
        });
        if let Some(n) = code.nonce {
            id["nonce"] = n.into();
        }
        let mut out = json!({ "token_type": "Bearer", "scope": "openid" });
        match self.key.sign_jwt("JWT", &id) {
            Ok(j) => out["id_token"] = j.into(),
            Err(e) => return err(500, &e),
        }
        // An access token for the cluster when it asks for one.
        if let Some(aud) = f.get("resource") {
            let at = json!({
                "iss": self.issuer, "sub": code.user, "aud": aud, "client_id": client, "iat": now,
                "nbf": now, "exp": now + ACCESS_SECS, "jti": crate::random_token(16), "sid": sid,
                "auth_time": now, "amr": amr, "scope": "openid",
                "glidex": { "directory": self.name, "mfa": code.mfa },
            });
            match self.key.sign_jwt(crate::tokens::ACCESS_TYP, &at) {
                Ok(j) => {
                    out["access_token"] = j.into();
                    out["expires_in"] = ACCESS_SECS.into();
                }
                Err(e) => return err(500, &e),
            }
        }
        Reply::json(200, out)
    }
}
