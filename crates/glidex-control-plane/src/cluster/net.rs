//! Mutual TLS between nodes on :8842 (spec/clustering.md §4, §12): a server
//! that asks every client for a certificate from the cluster CA, and a client
//! that trusts only that CA. Raft, forwarding, watch and relays all run over
//! it.

use arc_swap::ArcSwap;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use tokio_rustls::rustls::server::WebPkiClientVerifier;
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, RootCertStore, ServerConfig, SignatureScheme};

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("tls: {0}")]
    Tls(String),
    #[error("connect {0}: {1}")]
    Connect(String, String),
    #[error("request to {0} failed: {1}")]
    Request(String, String),
    #[error("{0}")]
    Other(String),
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A node's certificate chain and key, and the CAs it trusts.
#[derive(Clone)]
pub struct TlsMaterial {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: Arc<PrivateKeyDer<'static>>,
    pub trust: Vec<CertificateDer<'static>>,
}

impl TlsMaterial {
    pub fn from_pem(chain_pem: &str, key_pem: &str, trust_pem: &str) -> Result<TlsMaterial, NetError> {
        let ders = |pem: &str| super::pki::pem_bundle_ders(pem).map_err(|e| NetError::Tls(e.to_string()));
        let key = PrivateKeyDer::try_from(
            x509_parser::pem::parse_x509_pem(key_pem.as_bytes()).map_err(|e| NetError::Tls(format!("key: {e}")))?.1.contents,
        )
        .map_err(|e| NetError::Tls(format!("key: {e}")))?;
        Ok(TlsMaterial {
            chain: ders(chain_pem)?.into_iter().map(CertificateDer::from).collect(),
            key: Arc::new(key),
            trust: ders(trust_pem)?.into_iter().map(CertificateDer::from).collect(),
        })
    }

    fn roots(&self) -> Result<Arc<RootCertStore>, NetError> {
        let mut roots = RootCertStore::empty();
        for c in &self.trust {
            roots.add(c.clone()).map_err(|e| NetError::Tls(e.to_string()))?;
        }
        Ok(Arc::new(roots))
    }

    fn server_config(&self) -> Result<Arc<ServerConfig>, NetError> {
        let verifier = WebPkiClientVerifier::builder_with_provider(self.roots()?, provider())
            .allow_unauthenticated()
            .build()
            .map_err(|e| NetError::Tls(e.to_string()))?;
        let mut cfg = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| NetError::Tls(e.to_string()))?
            .with_client_cert_verifier(verifier)
            .with_single_cert(self.chain.clone(), self.key.clone_key())
            .map_err(|e| NetError::Tls(e.to_string()))?;
        cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Arc::new(cfg))
    }

    fn client_config_h1(&self) -> Result<Arc<ClientConfig>, NetError> {
        let mut cfg = (*self.client_config()?).clone();
        cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(cfg))
    }

    fn client_config(&self) -> Result<Arc<ClientConfig>, NetError> {
        let mut cfg = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| NetError::Tls(e.to_string()))?
            .with_root_certificates(self.roots()?)
            .with_client_auth_cert(self.chain.clone(), self.key.clone_key())
            .map_err(|e| NetError::Tls(e.to_string()))?;
        cfg.alpn_protocols = vec![b"h2".to_vec()];
        Ok(Arc::new(cfg))
    }
}

/// Accepts any server certificate. Used only to fetch the CA certificate
/// before it is trusted; the caller checks the CA against a pinned hash and
/// then connects again with it as the only trust anchor.
#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Swappable TLS state: a node's material changes when its certificate is
/// renewed or the CA rotates (§12.5).
pub struct ClusterTls {
    server: ArcSwap<ServerConfig>,
    client: ArcSwap<ClientConfig>,
    /// The same identity speaking HTTP/1.1, for connections that upgrade
    /// into a raw byte stream (console relay).
    client_h1: ArcSwap<ClientConfig>,
    material: ArcSwap<TlsMaterial>,
}

impl ClusterTls {
    pub fn new(m: TlsMaterial) -> Result<Arc<ClusterTls>, NetError> {
        Ok(Arc::new(ClusterTls { server: ArcSwap::new(m.server_config()?), client: ArcSwap::new(m.client_config()?), client_h1: ArcSwap::new(m.client_config_h1()?), material: ArcSwap::from_pointee(m) }))
    }

    pub fn reload(&self, m: TlsMaterial) -> Result<(), NetError> {
        self.server.store(m.server_config()?);
        self.client.store(m.client_config()?);
        self.client_h1.store(m.client_config_h1()?);
        self.material.store(Arc::new(m));
        Ok(())
    }

    pub fn material(&self) -> Arc<TlsMaterial> {
        self.material.load_full()
    }

    pub fn acceptor(&self) -> tokio_rustls::TlsAcceptor {
        tokio_rustls::TlsAcceptor::from(self.server.load_full())
    }
}

/// An HTTP/2 client for node-to-node requests. Connections are kept per
/// address and re-made when they break.
pub struct PeerClient {
    tls: ClientTls,
    conns: Mutex<HashMap<String, hyper::client::conn::http2::SendRequest<Full<Bytes>>>>,
    timeout: Duration,
}

enum ClientTls {
    /// The node's own, swapped on renewal.
    Node(Arc<ClusterTls>),
    /// No certificate; a joining host before it has one.
    Fixed(Arc<ClientConfig>),
}

impl ClientTls {
    fn config(&self) -> Arc<ClientConfig> {
        match self {
            ClientTls::Node(t) => t.client.load_full(),
            ClientTls::Fixed(c) => c.clone(),
        }
    }
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl PeerClient {
    pub fn new(tls: Arc<ClusterTls>) -> Arc<PeerClient> {
        Arc::new(PeerClient { tls: ClientTls::Node(tls), conns: Mutex::new(HashMap::new()), timeout: Duration::from_secs(10) })
    }

    async fn connect(&self, addr: &str, config: Arc<ClientConfig>) -> Result<hyper::client::conn::http2::SendRequest<Full<Bytes>>, NetError> {
        let ce = |e: &dyn std::fmt::Display| NetError::Connect(addr.to_string(), e.to_string());
        let sock: SocketAddr = addr.parse().map_err(|e| ce(&e))?;
        let tcp = tokio::time::timeout(self.timeout, tokio::net::TcpStream::connect(sock)).await.map_err(|_| ce(&"timed out"))?.map_err(|e| ce(&e))?;
        let _ = tcp.set_nodelay(true);
        let name = ServerName::IpAddress(sock.ip().into());
        let tls = tokio_rustls::TlsConnector::from(config).connect(name, tcp).await.map_err(|e| ce(&e))?;
        let (sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls)).await.map_err(|e| ce(&e))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        Ok(sender)
    }

    /// One request to `https://addr<path>`. Retries once on a broken
    /// pooled connection.
    pub async fn request(&self, addr: &str, method: Method, path: &str, headers: &[(&str, String)], body: Bytes) -> Result<Reply, NetError> {
        self.request_timeout(addr, method, path, headers, body, self.timeout).await
    }

    /// As [`request`](Self::request), with a limit for the whole exchange.
    pub async fn request_timeout(&self, addr: &str, method: Method, path: &str, headers: &[(&str, String)], body: Bytes, timeout: Duration) -> Result<Reply, NetError> {
        for attempt in 0..2 {
            let pooled = self.conns.lock().unwrap().get(addr).cloned();
            let mut sender = match pooled {
                Some(s) if !s.is_closed() => s,
                _ => {
                    let s = self.connect(addr, self.tls.config()).await?;
                    self.conns.lock().unwrap().insert(addr.to_string(), s.clone());
                    s
                }
            };
            let mut req = Request::builder().method(method.clone()).uri(format!("https://{addr}{path}"));
            for (k, v) in headers {
                req = req.header(*k, v.as_str());
            }
            let req = req.body(Full::new(body.clone())).map_err(|e| NetError::Other(e.to_string()))?;
            let sent = tokio::time::timeout(timeout, async {
                let resp = sender.send_request(req).await?;
                let (parts, inc) = resp.into_parts();
                let bytes = inc.collect().await?.to_bytes();
                Ok::<_, hyper::Error>(Reply { status: parts.status, headers: parts.headers, body: bytes })
            })
            .await;
            match sent {
                Ok(Ok(r)) => return Ok(r),
                Ok(Err(e)) => {
                    self.conns.lock().unwrap().remove(addr);
                    if attempt == 1 || !e.is_closed() && !e.is_canceled() {
                        return Err(NetError::Request(addr.to_string(), e.to_string()));
                    }
                }
                Err(_) => {
                    self.conns.lock().unwrap().remove(addr);
                    return Err(NetError::Request(addr.to_string(), "timed out".into()));
                }
            }
        }
        unreachable!()
    }

    /// GET `path` into the file `dest`, streamed. Returns the status; the file
    /// is written only for `200`.
    pub async fn get_to_file(&self, addr: &str, path: &str, dest: &std::path::Path) -> Result<u16, NetError> {
        use tokio::io::AsyncWriteExt;
        let ce = |e: &dyn std::fmt::Display| NetError::Connect(addr.to_string(), e.to_string());
        let sock: SocketAddr = addr.parse().map_err(|e| ce(&e))?;
        let tcp = tokio::net::TcpStream::connect(sock).await.map_err(|e| ce(&e))?;
        let tls = tokio_rustls::TlsConnector::from(self.tls.config()).connect(ServerName::IpAddress(sock.ip().into()), tcp).await.map_err(|e| ce(&e))?;
        let (mut sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls)).await.map_err(|e| ce(&e))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = Request::builder().uri(format!("https://{addr}{path}")).body(Full::new(Bytes::new())).map_err(|e| NetError::Other(e.to_string()))?;
        let resp = sender.send_request(req).await.map_err(|e| NetError::Request(addr.to_string(), e.to_string()))?;
        let status = resp.status().as_u16();
        let mut body = resp.into_body();
        if status != 200 {
            let _ = body.collect().await;
            return Ok(status);
        }
        let mut f = tokio::fs::File::create(dest).await.map_err(|e| NetError::Other(e.to_string()))?;
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| NetError::Request(addr.to_string(), e.to_string()))?;
            if let Some(d) = frame.data_ref() {
                f.write_all(d).await.map_err(|e| NetError::Other(e.to_string()))?;
            }
        }
        f.sync_all().await.map_err(|e| NetError::Other(e.to_string()))?;
        Ok(200)
    }

    /// Open a raw byte stream to `path` on `addr` by upgrading an HTTP/1.1
    /// request (`Upgrade: glidex-stream`). Used to relay consoles.
    pub async fn upgrade(&self, addr: &str, path: &str) -> Result<TokioIo<hyper::upgrade::Upgraded>, NetError> {
        let ClientTls::Node(t) = &self.tls else { return Err(NetError::Other("not a cluster node".into())) };
        let ce = |e: &dyn std::fmt::Display| NetError::Connect(addr.to_string(), e.to_string());
        let sock: SocketAddr = addr.parse().map_err(|e| ce(&e))?;
        let tcp = tokio::time::timeout(self.timeout, tokio::net::TcpStream::connect(sock)).await.map_err(|_| ce(&"timed out"))?.map_err(|e| ce(&e))?;
        let tls = tokio_rustls::TlsConnector::from(t.client_h1.load_full()).connect(ServerName::IpAddress(sock.ip().into()), tcp).await.map_err(|e| ce(&e))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls)).await.map_err(|e| ce(&e))?;
        tokio::spawn(async move {
            let _ = conn.with_upgrades().await;
        });
        let req = Request::builder()
            .uri(format!("https://{addr}{path}"))
            .header(hyper::header::HOST, addr)
            .header(hyper::header::CONNECTION, "upgrade")
            .header(hyper::header::UPGRADE, "glidex-stream")
            .body(Full::new(Bytes::new()))
            .map_err(|e| NetError::Other(e.to_string()))?;
        let resp = sender.send_request(req).await.map_err(|e| NetError::Request(addr.to_string(), e.to_string()))?;
        if resp.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Err(NetError::Request(addr.to_string(), format!("status {}", resp.status())));
        }
        let up = hyper::upgrade::on(resp).await.map_err(|e| NetError::Request(addr.to_string(), e.to_string()))?;
        Ok(TokioIo::new(up))
    }

    /// POST a file as the request body, streamed (snapshots).
    pub async fn post_file(&self, addr: &str, path: &str, headers: &[(&str, String)], file: &std::path::Path) -> Result<Reply, NetError> {
        use http_body_util::StreamBody;
        use hyper::body::Frame;
        use tokio::io::AsyncReadExt;
        let ce = |e: &dyn std::fmt::Display| NetError::Connect(addr.to_string(), e.to_string());
        let sock: SocketAddr = addr.parse().map_err(|e| ce(&e))?;
        let tcp = tokio::net::TcpStream::connect(sock).await.map_err(|e| ce(&e))?;
        let tls = tokio_rustls::TlsConnector::from(self.tls.config()).connect(ServerName::IpAddress(sock.ip().into()), tcp).await.map_err(|e| ce(&e))?;
        let (mut sender, conn) = hyper::client::conn::http2::handshake::<_, _, StreamBody<std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>> + Send>>>>(TokioExecutor::new(), TokioIo::new(tls))
            .await
            .map_err(|e| ce(&e))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let f = tokio::fs::File::open(file).await.map_err(|e| NetError::Other(e.to_string()))?;
        let stream = futures_util::stream::unfold(f, |mut f| async move {
            let mut buf = vec![0u8; 256 * 1024];
            match f.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(Frame::data(Bytes::from(buf))), f))
                }
                Err(e) => Some((Err(e), f)),
            }
        });
        let body = StreamBody::new(Box::pin(stream) as std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<Frame<Bytes>, std::io::Error>> + Send>>);
        let mut req = Request::builder().method(Method::POST).uri(format!("https://{addr}{path}"));
        for (k, v) in headers {
            req = req.header(*k, v.as_str());
        }
        let req = req.body(body).map_err(|e| NetError::Other(e.to_string()))?;
        let resp = sender.send_request(req).await.map_err(|e| NetError::Request(addr.to_string(), e.to_string()))?;
        let (parts, inc) = resp.into_parts();
        let body = inc.collect().await.map_err(|e| NetError::Request(addr.to_string(), e.to_string()))?.to_bytes();
        Ok(Reply { status: parts.status, headers: parts.headers, body })
    }
}

/// Fetch a server's CA certificate over a connection that trusts nobody, for
/// a joining host that has only a pinned hash (§5.2). The caller must verify
/// the result against the hash before using it.
pub async fn fetch_ca_untrusted(addr: &str) -> Result<String, NetError> {
    let ce = |e: &dyn std::fmt::Display| NetError::Connect(addr.to_string(), e.to_string());
    let cfg = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| NetError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny(provider())))
        .with_no_client_auth();
    let mut cfg = cfg;
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    let sock: SocketAddr = addr.parse().map_err(|e| ce(&e))?;
    let tcp = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect(sock)).await.map_err(|_| ce(&"timed out"))?.map_err(|e| ce(&e))?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(ServerName::IpAddress(sock.ip().into()), tcp).await.map_err(|e| ce(&e))?;
    let (mut sender, conn) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(tls)).await.map_err(|e| ce(&e))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = Request::builder().uri(format!("https://{addr}/cluster/v1/ca")).body(Full::new(Bytes::new())).unwrap();
    let resp: Response<Incoming> = sender.send_request(req).await.map_err(|e| NetError::Request(addr.to_string(), e.to_string()))?;
    if !resp.status().is_success() {
        return Err(NetError::Request(addr.to_string(), format!("status {}", resp.status())));
    }
    let body = resp.into_body().collect().await.map_err(|e| NetError::Request(addr.to_string(), e.to_string()))?.to_bytes();
    String::from_utf8(body.to_vec()).map_err(|e| NetError::Other(e.to_string()))
}

/// A client with no certificate that trusts exactly `ca_pem`: the joining
/// host, once it has verified the CA against the token's hash.
pub fn client_trusting(ca_pem: &str) -> Result<Arc<PeerClient>, NetError> {
    let ders = super::pki::pem_bundle_ders(ca_pem).map_err(|e| NetError::Tls(e.to_string()))?;
    let mut roots = RootCertStore::empty();
    for d in ders {
        roots.add(CertificateDer::from(d)).map_err(|e| NetError::Tls(e.to_string()))?;
    }
    let mut cfg = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| NetError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Ok(Arc::new(PeerClient { tls: ClientTls::Fixed(Arc::new(cfg)), conns: Mutex::new(HashMap::new()), timeout: Duration::from_secs(10) }))
}

/// Who is on the other end of an accepted connection (§12.3), from its
/// certificate. `None` means no certificate was presented.
#[derive(Debug, Clone)]
pub struct PeerCert {
    pub node_id: String,
    pub server: bool,
    pub serial: String,
}

/// Serve `router` on `listener` over TLS until `stop` fires. Every request
/// carries the connection's [`PeerCert`] (if any) as an extension.
pub async fn serve(listener: tokio::net::TcpListener, tls: Arc<ClusterTls>, router: axum::Router, mut stop: tokio::sync::watch::Receiver<bool>) {
    use tower::ServiceExt;
    loop {
        let (stream, addr) = tokio::select! {
            r = listener.accept() => match r { Ok(v) => v, Err(e) => { tracing::warn!("cluster accept: {}", e); continue; } },
            _ = async { let _ = stop.wait_for(|s| *s).await; } => return,
        };
        let acceptor = tls.acceptor();
        let router = router.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let _ = stream.set_nodelay(true);
            let tls = match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream)).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::debug!(%addr, "cluster TLS handshake failed: {}", e);
                    return;
                }
                Err(_) => return,
            };
            let peer = tls
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .and_then(|c| super::pki::inspect_node_cert(c.as_ref()).ok())
                .map(|c| PeerCert { node_id: c.node_id, server: c.server, serial: c.serial });
            let svc = hyper::service::service_fn(move |mut req: Request<Incoming>| {
                let router = router.clone();
                if let Some(p) = &peer {
                    req.extensions_mut().insert(p.clone());
                }
                req.extensions_mut().insert(crate::api::ClientAddr(addr));
                async move { router.oneshot(req.map(axum::body::Body::new)).await }
            });
            let builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            let conn = builder.serve_connection_with_upgrades(TokioIo::new(tls), svc);
            tokio::pin!(conn);
            let mut stop = stop;
            tokio::select! {
                r = conn.as_mut() => { if let Err(e) = r { tracing::debug!("cluster connection: {}", e); } }
                _ = async { let _ = stop.wait_for(|s| *s).await; } => { conn.as_mut().graceful_shutdown(); let _ = conn.await; }
            }
        });
    }
}
