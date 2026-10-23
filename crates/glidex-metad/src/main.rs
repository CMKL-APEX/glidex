//! `glidex-metad [--listen ADDR:PORT] [--control-plane SOCKET]`: serve the
//! metadata endpoint (spec/workload-identity.md W11). The defaults are
//! `169.254.169.254:8775` (netd's DNAT target) and the control plane's
//! `/run/glidex-cp/metad.sock`.

use glidex_metad::{Config, Limiter, handle};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "glidex_metad=info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().collect();
    let default_listen = SocketAddr::from((
        glidex_ovs::metadata::ADDRESS,
        glidex_ovs::metadata::LISTEN_PORT,
    ));
    let cfg = Arc::new(Config {
        listen: arg(&args, "--listen")
            .map(|a| a.parse().expect("--listen is ADDR:PORT"))
            .unwrap_or(default_listen),
        control_plane: arg(&args, "--control-plane")
            .unwrap_or_else(|| "/run/glidex-cp/metad.sock".into())
            .into(),
    });
    let limiter = Arc::new(Limiter::default());

    // Bound even before netd has put the address on the loopback.
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .expect("socket");
    socket.set_reuse_address(true).expect("SO_REUSEADDR");
    socket.set_freebind_v4(true).expect("IP_FREEBIND");
    socket.set_nonblocking(true).expect("non-blocking");
    socket.bind(&cfg.listen.into()).expect("bind");
    socket.listen(256).expect("listen");
    let listener = tokio::net::TcpListener::from_std(socket.into()).expect("a tokio listener");
    tracing::info!(listen = %cfg.listen, control_plane = %cfg.control_plane.display(), "glidex-metad serving");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!("accept: {e}");
                continue;
            }
        };
        // Replies go no further than the VM's own link (W3).
        let _ = stream.set_ttl(1);
        let (cfg, limiter) = (cfg.clone(), limiter.clone());
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req| {
                let (cfg, limiter) = (cfg.clone(), limiter.clone());
                async move { Ok::<_, std::convert::Infallible>(handle(&cfg, &limiter, peer, req).await) }
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), svc)
                .await
            {
                tracing::debug!("connection from {peer}: {e}");
            }
        });
    }
}
