//! glidex-ui: the web UI.
//!
//! - `glidex-ui` serves the built UI (`bun run build` → `ui/dist`, or
//!   `GLIDEX_UI_DIR`) on `GLIDEX_UI_LISTEN` (default `127.0.0.1:5173`) and
//!   proxies `/api/*` to the control plane at `GLIDEX_API_URL` (default
//!   `http://127.0.0.1:8841`), WebSocket upgrades (the VM console) included.
//!   This is what `glidex-ui.service` runs.
//! - `glidex-ui --dev` runs the Vite dev server (hot reload) instead.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tokio::signal;
use tower_http::services::{ServeDir, ServeFile};

fn ui_dir() -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    PathBuf::from(manifest_dir).join("ui")
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let dev = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--dev") => true,
        Some("-h" | "--help") => {
            println!("Usage: glidex-ui [--dev]\n\n  (none)  serve the built UI (GLIDEX_UI_DIR, GLIDEX_UI_LISTEN, GLIDEX_API_URL)\n  --dev   run the Vite dev server with hot reload");
            return;
        }
        Some(other) => {
            eprintln!("unknown argument '{}' (see --help)", other);
            std::process::exit(2);
        }
    };
    if dev {
        run_dev_server().await
    } else if let Err(e) = serve().await {
        tracing::error!("{}", e);
        std::process::exit(1);
    }
}

#[derive(Clone)]
struct Proxy {
    client: Client<HttpConnector, Body>,
    /// `http://host:port` of the control plane, without a trailing slash.
    base: String,
    authority: HeaderValue,
}

async fn serve() -> Result<(), String> {
    let dist = std::env::var_os("GLIDEX_UI_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ui_dir().join("dist"));
    let index = dist.join("index.html");
    if !index.is_file() {
        return Err(format!(
            "{} not found: build the UI (cd crates/glidex-ui/ui && bun run build), set GLIDEX_UI_DIR, or use --dev",
            index.display()
        ));
    }
    let api: Uri = std::env::var("GLIDEX_API_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8841".into())
        .parse()
        .map_err(|e| format!("GLIDEX_API_URL: {}", e))?;
    let authority = api
        .authority()
        .filter(|_| api.scheme_str() == Some("http"))
        .ok_or("GLIDEX_API_URL must be http://host:port")?
        .to_string();
    let proxy = Proxy {
        client: Client::builder(TokioExecutor::new()).build_http(),
        base: format!("http://{}", authority),
        authority: HeaderValue::from_str(&authority).map_err(|e| e.to_string())?,
    };

    let listen: SocketAddr = std::env::var("GLIDEX_UI_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:5173".into())
        .parse()
        .map_err(|e| format!("GLIDEX_UI_LISTEN: {}", e))?;

    // Unknown paths get index.html: they are client-side routes.
    let app = Router::new()
        .route("/api", any(proxy_api))
        .route("/api/{*rest}", any(proxy_api))
        .fallback_service(ServeDir::new(&dist).fallback(ServeFile::new(&index)))
        .with_state(proxy);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| format!("bind {}: {}", listen, e))?;
    tracing::info!("GlideX UI on http://{} (from {}, API {})", listen, dist.display(), api);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| e.to_string())
}

/// `/api/x?y` → `<control plane>/x?y`, like the Vite dev proxy.
fn upstream_uri(base: &str, uri: &Uri) -> Result<Uri, axum::http::uri::InvalidUri> {
    let pq = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let rest = pq.strip_prefix("/api").unwrap_or(pq);
    let sep = if rest.starts_with('/') { "" } else { "/" };
    format!("{}{}{}", base, sep, rest).parse()
}

async fn proxy_api(State(p): State<Proxy>, mut req: Request) -> Response {
    match upstream_uri(&p.base, req.uri()) {
        Ok(uri) => *req.uri_mut() = uri,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
    req.headers_mut().insert(header::HOST, p.authority.clone());

    // WebSocket (console): forward the upgrade, then splice the two
    // connections together.
    let client_upgrade = req
        .headers()
        .contains_key(header::UPGRADE)
        .then(|| hyper::upgrade::on(&mut req));

    let mut resp = match p.client.request(req).await {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("control plane unreachable at {}: {}", p.base, e)).into_response()
        }
    };
    if let Some(client_upgrade) = client_upgrade {
        if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
            let upstream_upgrade = hyper::upgrade::on(&mut resp);
            tokio::spawn(async move {
                match tokio::try_join!(client_upgrade, upstream_upgrade) {
                    Ok((client, upstream)) => {
                        let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(upstream)).await;
                    }
                    Err(e) => tracing::warn!("WebSocket upgrade failed: {}", e),
                }
            });
        }
    }
    resp.map(Body::new)
}

async fn shutdown_signal() {
    let mut term = signal::unix::signal(signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("Shutting down...");
}

async fn run_dev_server() {
    let ui_path = ui_dir();
    tracing::info!("Starting UI dev server from {}", ui_path.display());

    let mut child = Command::new("bun")
        .arg("run")
        .arg("dev")
        .current_dir(&ui_path)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("Failed to start bun dev server. Is bun installed?");

    tracing::info!("GlideX UI available at http://localhost:5173");

    tokio::select! {
        status = child.wait() => {
            match status {
                Ok(s) => tracing::info!("Bun dev server exited with {}", s),
                Err(e) => tracing::error!("Bun dev server error: {}", e),
            }
        }
        _ = signal::ctrl_c() => {
            tracing::info!("Shutting down...");
            let _ = child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_prefix_is_stripped() {
        let base = "http://127.0.0.1:8841";
        let up = |s: &str| upstream_uri(base, &s.parse().unwrap()).unwrap().to_string();
        assert_eq!(up("/api/vms"), "http://127.0.0.1:8841/vms");
        assert_eq!(up("/api/vms/x/console/ws"), "http://127.0.0.1:8841/vms/x/console/ws");
        assert_eq!(up("/api/images?all=1"), "http://127.0.0.1:8841/images?all=1");
        assert_eq!(up("/api"), "http://127.0.0.1:8841/");
    }
}
