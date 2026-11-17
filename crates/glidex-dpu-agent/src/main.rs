//! `glidex-dpu-agent --config /etc/glidex/dpu-agent.json`: serve the agent
//! protocol on the DPU's management link (spec/31-dpu.md §10.2).

use glidex_dpu::SystemExec;
use glidex_dpu_agent::{Agent, server, tls};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// The management link's address on the DPU and the port, never a
    /// wildcard: the agent listens on nothing else.
    listen: std::net::SocketAddr,
    ca: PathBuf,
    cert: PathBuf,
    key: PathBuf,
    /// The CN of the one client accepted: the host's netd.
    client_identity: String,
    #[serde(default)]
    mst_device: Option<String>,
    /// Where the chassis key and certificates live (0700).
    #[serde(default)]
    ovn_dir: Option<PathBuf>,
    /// The DPU's underlay address, when netd's `dpu.json` gives none.
    #[serde(default)]
    encap_ip: Option<std::net::IpAddr>,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let path = std::env::args()
        .skip_while(|a| a != "--config")
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/etc/glidex/dpu-agent.json"));
    let fail = |e: String| -> ! {
        eprintln!("glidex-dpu-agent: {e}");
        std::process::exit(1)
    };
    let config: Config = std::fs::read(&path)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
    if config.listen.ip().is_unspecified() {
        fail("listen must be the management link's address, not a wildcard".into());
    }
    let pem = tls::TlsPem::load(&config.ca, &config.cert, &config.key).unwrap_or_else(|e| fail(e));
    let tls_config = tls::server_config(&pem).unwrap_or_else(|e| fail(e));
    let mut agent = Agent::new(Arc::new(SystemExec::new()));
    if let Some(d) = config.mst_device {
        agent.mst_device = d;
    }
    if let Some(d) = config.ovn_dir {
        agent.chassis.dir = d;
    }
    agent.chassis.encap_ip = config.encap_ip;
    let listener = std::net::TcpListener::bind(config.listen)
        .unwrap_or_else(|e| fail(format!("{}: {e}", config.listen)));
    tracing::info!(listen = %config.listen, "glidex-dpu-agent serving");
    server::serve(
        Arc::new(agent),
        listener,
        tls_config,
        config.client_identity,
    );
}
