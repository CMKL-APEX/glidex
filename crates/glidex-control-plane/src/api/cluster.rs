//! `/cluster/*`: forming, joining and inspecting the cluster (spec/clustering.md
//! §5). `init`, `join` and `snapshot` act on the host that receives them.

use super::{err, ApiErr, Caller};
use crate::authz::{Ent, EntitySet};
use crate::cluster::manage;
use crate::node::NodeRole;
use axum::{http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use serde_json::json;

fn cluster_err(e: crate::cluster::ClusterError) -> ApiErr {
    err(StatusCode::CONFLICT, "cluster_error", e.to_string())
}

fn parse_addr(what: &str, v: &Option<String>) -> Result<Option<std::net::SocketAddr>, ApiErr> {
    match v {
        None => Ok(None),
        Some(s) => s.parse().map(Some).map_err(|_| {
            err(
                StatusCode::BAD_REQUEST,
                "invalid",
                format!("{what} must be <ip>:<port>"),
            )
        }),
    }
}

fn parse_ip(v: &Option<String>) -> Result<Option<std::net::IpAddr>, ApiErr> {
    match v {
        None => Ok(None),
        Some(s) => s.parse().map(Some).map_err(|_| {
            err(
                StatusCode::BAD_REQUEST,
                "invalid",
                "tunnel_ip must be an IP address",
            )
        }),
    }
}

#[derive(Deserialize)]
pub struct InitBody {
    #[serde(default)]
    advertise: Option<String>,
    #[serde(default)]
    tunnel_ip: Option<String>,
    /// Init changes how every record is stored and can't be undone but by
    /// restoring the backup it makes (§5.1).
    #[serde(default)]
    force: bool,
}

pub async fn init(c: Caller, Json(b): Json<InitBody>) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.host(&crate::authz::node_id());
    c.require(Ent::Host, es)?;
    if !b.force {
        return Err(err(
            StatusCode::CONFLICT,
            "confirmation_required",
            "this turns the host into a cluster of one and re-keys every record; it keeps a backup but can't be undone: confirm with --force",
        ));
    }
    let opts = manage::InitOptions {
        advertise: parse_addr("advertise", &b.advertise)?,
        tunnel_ip: parse_ip(&b.tunnel_ip)?,
        listen: None,
    };
    let cfg = c.app.auth.config.clone();
    let out = manage::init(&c.app.manager.clone(), &cfg, opts)
        .await
        .map_err(cluster_err)?;
    // Role links now name the new cluster.
    c.auth()
        .reload_policies()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    c.set_target("cluster".to_string());
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Deserialize)]
pub struct JoinBody {
    server: String,
    token: String,
    #[serde(default)]
    role: Option<NodeRole>,
    #[serde(default)]
    advertise: Option<String>,
    #[serde(default)]
    tunnel_ip: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

pub async fn join(c: Caller, Json(b): Json<JoinBody>) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.host(&crate::authz::node_id());
    c.require(Ent::Host, es)?;
    let server = b.server.parse().map_err(|_| {
        err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "server must be <ip>:<port> of a cluster node (port 8842)",
        )
    })?;
    let opts = manage::JoinOptions {
        server,
        token: b.token,
        role: b.role.unwrap_or(NodeRole::Server),
        advertise: parse_addr("advertise", &b.advertise)?,
        tunnel_ip: parse_ip(&b.tunnel_ip)?,
        name: b.name,
        listen: None,
    };
    let cfg = c.app.auth.config.clone();
    let mut out = manage::join(&c.app.manager.clone(), &cfg, opts)
        .await
        .map_err(cluster_err)?;
    c.auth()
        .reload_policies()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    c.set_target("cluster".to_string());
    if let Some(note) = restart_as_agent(&c.app.manager) {
        out["restart"] = json!(note);
    }
    Ok((StatusCode::CREATED, Json(out)))
}

/// A host that just became an agent comes back in agent mode: an agent has
/// no API of its own (spec/clustering-ui.md §3.6), and this process still
/// serves the standalone one, whose logins the cluster refuses. Under
/// systemd it restarts itself once this answer is out; by hand, it says so.
fn restart_as_agent(m: &std::sync::Arc<crate::state::VmManager>) -> Option<&'static str> {
    m.node_link()?;
    if !crate::restart::supervised() {
        return Some("restart the control plane: it runs as a cluster agent from then on");
    }
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        crate::restart::request("this host is now a cluster agent");
    });
    Some("the control plane restarts as a cluster agent in a moment")
}

#[derive(Deserialize)]
pub struct ImportJoinBody {
    server: String,
    token: String,
    #[serde(default)]
    advertise: Option<String>,
    #[serde(default)]
    tunnel_ip: Option<String>,
    #[serde(default)]
    name: Option<String>,
    /// How long to wait for an administrator (default a day).
    #[serde(default)]
    wait_secs: Option<u64>,
}

/// `gxctl cluster join --import`: join with this host's resources (§5.9). The
/// plan waits for approval on a server; this host works as it is until then.
pub async fn import_join(
    c: Caller,
    Json(b): Json<ImportJoinBody>,
) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.host(&crate::authz::node_id());
    c.require(Ent::Host, es)?;
    let server = b.server.parse().map_err(|_| {
        err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "server must be <ip>:<port> of a cluster node (port 8842)",
        )
    })?;
    let opts = manage::ImportOptions {
        server,
        token: b.token,
        advertise: parse_addr("advertise", &b.advertise)?,
        tunnel_ip: parse_ip(&b.tunnel_ip)?,
        name: b.name,
        listen: None,
        wait: std::time::Duration::from_secs(b.wait_secs.unwrap_or(86400)),
    };
    let cfg = c.app.auth.config.clone();
    let m = c.app.manager.clone();
    c.set_target("cluster".to_string());
    c.audit_always();
    tokio::spawn(async move {
        match manage::join_import(&m, &cfg, opts).await {
            Ok(v) => {
                tracing::warn!("import finished: {}", v);
                if let Some(note) = restart_as_agent(&m) {
                    tracing::warn!("{note}");
                }
            }
            Err(e) => tracing::error!("import failed: {}", e),
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(
            json!({ "waiting": true, "message": "the import plan waits for approval on a server: `gxctl node import list`; this host keeps working as it is until then" }),
        ),
    ))
}

#[derive(Deserialize)]
pub struct RejoinBody {
    server: String,
    token: String,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    advertise: Option<String>,
    #[serde(default)]
    tunnel_ip: Option<String>,
}

/// `gxctl cluster rejoin`: come back as the same node (§5.7).
pub async fn rejoin(c: Caller, Json(b): Json<RejoinBody>) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.host(&crate::authz::node_id());
    c.require(Ent::Host, es)?;
    let server = b.server.parse().map_err(|_| {
        err(
            StatusCode::BAD_REQUEST,
            "invalid",
            "server must be <ip>:<port> of a cluster node (port 8842)",
        )
    })?;
    let opts = manage::RejoinOptions {
        server,
        token: b.token,
        node_id: b.node_id,
        advertise: parse_addr("advertise", &b.advertise)?,
        tunnel_ip: parse_ip(&b.tunnel_ip)?,
        listen: None,
    };
    let cfg = c.app.auth.config.clone();
    let mut out = manage::rejoin(&c.app.manager.clone(), &cfg, opts)
        .await
        .map_err(cluster_err)?;
    c.auth()
        .reload_policies()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    c.set_target("cluster".to_string());
    c.audit_always();
    if let Some(note) = restart_as_agent(&c.app.manager) {
        out["restart"] = json!(note);
    }
    Ok(Json(out))
}

/// `gxctl cluster dissolve`: the last node leaves with everything (§5.10).
pub async fn dissolve(
    c: Caller,
    b: Option<Json<serde_json::Value>>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    if !b.is_some_and(|b| b.0["force"] == true) {
        return Err(err(StatusCode::CONFLICT, "confirmation_required", "this ends the cluster and turns this host into a standalone host with everything it held; it keeps a final snapshot: confirm with --force"));
    }
    let cl = require_cluster(&c)?;
    c.set_target("cluster".to_string());
    c.audit_always();
    cl.dissolve()
        .await
        .map(Json)
        .map_err(|e| err(StatusCode::CONFLICT, "cluster_error", e.to_string()))
}

/// `gxctl cluster leave`: this host's node was removed; go back to standalone.
pub async fn leave(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    let mut es = EntitySet::new();
    es.host(&crate::authz::node_id());
    c.require(Ent::Host, es)?;
    c.set_target("cluster".to_string());
    c.audit_always();
    let out = manage::leave(&c.app.manager.clone())
        .await
        .map_err(cluster_err)?;
    Ok(Json(out))
}

fn require_cluster(c: &Caller) -> Result<std::sync::Arc<crate::cluster::Cluster>, ApiErr> {
    c.manager().cluster().ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "not_clustered",
            "this host is not part of a cluster: run `gxctl cluster init` or join one",
        )
    })
}

#[derive(Deserialize, Default)]
pub struct StatusQuery {
    #[serde(default)]
    ports: bool,
}

pub async fn status(
    c: Caller,
    via: Option<axum::Extension<super::RelayedBy>>,
    axum::extract::Query(q): axum::extract::Query<StatusQuery>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    match c.manager().cluster() {
        Some(cl) => {
            // Through a node's UI, "this node" is that node.
            let mut v = manage::status_seen_from(&cl, via.as_ref().map(|v| v.0 .0.as_str()));
            // Port checks open connections to every node: opt in (`?ports=true`).
            if q.ports {
                v["ports"] = json!(manage::port_checks(&cl, c.manager().ovn_enabled()).await);
            }
            Ok(Json(v))
        }
        None => Ok(Json(
            json!({ "clustered": false, "nodes": c.manager().nodes().list().unwrap_or_default().iter().map(|n| json!({ "id": n.meta.id, "name": n.spec.name, "role": n.spec.role, "phase": n.status.phase })).collect::<Vec<_>>() }),
        )),
    }
}

#[derive(Deserialize)]
pub struct TokenBody {
    #[serde(default)]
    role: Option<NodeRole>,
    #[serde(default)]
    ttl_secs: Option<u64>,
    #[serde(default)]
    allow_import: bool,
}

pub async fn create_token(
    c: Caller,
    Json(b): Json<TokenBody>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = require_cluster(&c)?;
    let ttl = b.ttl_secs.unwrap_or(3600).clamp(60, 7 * 86400);
    let role = b.role.unwrap_or(NodeRole::Agent);
    let by = c.actor();
    let token = manage::join_token(&cl, role, ttl, b.allow_import, &by)
        .await
        .map_err(cluster_err)?;
    c.set_target("join-token".to_string());
    let servers: Vec<String> = crate::node::NodeStore::new(cl.db.clone())
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|n| n.spec.role == NodeRole::Server && !n.status.phase.is_tombstone())
        .filter_map(|n| n.status.advertise.map(|a| a.to_string()))
        .collect();
    Ok((
        StatusCode::CREATED,
        Json(
            json!({ "token": token, "role": role, "ttl_secs": ttl, "servers": servers, "ca_fingerprint": cl.signing_fp() }),
        ),
    ))
}

#[derive(Deserialize)]
pub struct PromoteBody {
    nodes: Vec<String>,
    #[serde(default)]
    force: bool,
}

pub async fn promote(c: Caller, Json(b): Json<PromoteBody>) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = require_cluster(&c)?;
    let out = manage::promote(&cl, &b.nodes, b.force)
        .await
        .map_err(cluster_err)?;
    c.set_target("cluster".to_string());
    Ok(Json(out))
}

#[derive(Deserialize, Default)]
pub struct RotateBody {
    #[serde(default)]
    grace_secs: Option<u64>,
}

/// `gxctl cluster rotate-ca [--grace <secs>]` (§12.5).
pub async fn rotate_ca(
    c: Caller,
    b: Option<Json<RotateBody>>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    let cl = require_cluster(&c)?;
    c.set_target("cluster".to_string());
    c.audit_always();
    let out = cl
        .rotate_ca(b.and_then(|b| b.0.grace_secs))
        .await
        .map_err(cluster_err)?;
    Ok(Json(out))
}

/// A consistent snapshot of the replicated tables, streamed to the caller.
/// It holds credential and token hashes: store it like the database itself.
pub async fn snapshot(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.manager().cluster().ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "not_clustered",
            "this host is not part of a cluster",
        )
    })?;
    c.set_target("cluster".to_string());
    // Audited as it leaves: the file holds every secret hash.
    c.audit_always();
    let db = c.manager().database();
    let dir = tempfile::tempdir()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let path = dir.path().join("snapshot.gxsnap");
    let p2 = path.clone();
    tokio::task::spawn_blocking(move || manage::export_snapshot(&db, &p2))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?
        .map_err(cluster_err)?;
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    let stream = tokio_util_stream(file, dir);
    Ok((
        StatusCode::OK,
        [
            ("content-type", "application/octet-stream"),
            (
                "content-disposition",
                "attachment; filename=\"glidex-cluster.gxsnap\"",
            ),
        ],
        axum::body::Body::from_stream(stream),
    ))
}

/// Stream a file, deleting its directory when the stream ends.
fn tokio_util_stream(
    f: tokio::fs::File,
    dir: tempfile::TempDir,
) -> impl futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> {
    use tokio::io::AsyncReadExt;
    futures_util::stream::unfold((f, Some(dir)), |(mut f, dir)| async move {
        let mut buf = vec![0u8; 256 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => {
                drop(dir);
                None
            }
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(bytes::Bytes::from(buf)), (f, dir)))
            }
            Err(e) => Some((Err(e), (f, dir))),
        }
    })
}

// ---- OVN as a cluster setting (spec/clustering-ui.md §8a) -----------------

/// Why a node can't take part in OVN yet, if it can't.
fn ovn_missing(n: &crate::node::Node) -> Option<String> {
    if n.status.versions.ovn.is_none() {
        return Some("OVN is not installed (ovn-controller)".into());
    }
    if n.spec.role == NodeRole::Server && !n.status.features.ovn_central {
        return Some("OVN's central part is not installed (ovn-northd)".into());
    }
    None
}

/// What `GET /cluster/settings/ovn` answers to someone who may read the cluster.
fn ovn_view(c: &Caller) -> serde_json::Value {
    let m = c.manager();
    let settings = m.ovn_settings();
    let source = if settings.is_some() {
        "cluster"
    } else if m.ovn.load().enabled {
        "file"
    } else {
        "none"
    };
    let shown = settings
        .clone()
        .unwrap_or_else(|| crate::ovn_settings::OvnSettings::from_file(&m.ovn.load()));
    let all = m.nodes().list().unwrap_or_default();
    let northd = crate::node::cluster_northd_ovn(&all);
    let nodes: Vec<serde_json::Value> = all
        .into_iter()
        .filter(|n| !n.status.phase.is_tombstone())
        .map(|n| json!({ "id": n.meta.id, "name": n.spec.name, "role": n.spec.role, "ovn_version": n.status.versions.ovn, "ovn_central": n.status.features.ovn_central, "missing": ovn_missing(&n), "outdated": crate::node::ovn_outdated(&n, northd.as_deref()) }))
        .collect();
    let cluster_networks: Vec<String> = m
        .networks_list()
        .into_iter()
        .filter(|n| n.scope == crate::network::NetworkScope::Cluster)
        .map(|n| n.name)
        .collect();
    json!({ "enabled": m.ovn_enabled(), "edge": m.ovn.load().edge.is_some(), "vpc_pool": vpc_pool(c), "clustered": m.cluster().is_some(), "source": source, "settings": shown, "nodes": nodes, "cluster_networks": cluster_networks })
}

/// `GET /cluster/settings/ovn`: whether the cluster runs on OVN (anyone signed
/// in: the network form needs it), and the options and node readiness (with
/// `readCluster`).
pub async fn get_ovn_settings(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    if c.allowed("readCluster", Ent::Cluster, EntitySet::new()) {
        return Ok(Json(ovn_view(&c)));
    }
    // Whether NAT networks can span the nodes, and whether a project's own
    // router can reach outside, are no secret: forms need them.
    Ok(Json(
        json!({ "enabled": c.manager().ovn_enabled(), "edge": c.manager().ovn.load().edge.is_some(), "vpc_pool": vpc_pool(&c) }),
    ))
}

/// Whether VPC routers can have an external address: the edge has a pool.
fn vpc_pool(c: &Caller) -> bool {
    c.manager()
        .ovn
        .load()
        .edge
        .as_ref()
        .is_some_and(|e| e.external_pool.is_some())
}

#[derive(Deserialize)]
pub struct OvnSettingsBody {
    enabled: bool,
    #[serde(default)]
    underlay_mtu: Option<u16>,
    #[serde(default)]
    nat_supernet: Option<String>,
    #[serde(default)]
    edge: Option<crate::cluster::config::EdgeConfig>,
    #[serde(default)]
    snat_ct_zones: Option<(u16, u16)>,
    #[serde(default)]
    dns_servers: Vec<std::net::IpAddr>,
}

/// `PUT /cluster/settings/ovn` (`configureOvn`): turn OVN on or off for the
/// whole cluster and set its options. Every node follows without a restart.
pub async fn put_ovn_settings(
    c: Caller,
    Json(b): Json<OvnSettingsBody>,
) -> Result<impl IntoResponse, ApiErr> {
    c.require(Ent::Cluster, EntitySet::new())?;
    c.set_target("cluster:ovn".to_string());
    c.audit_always();
    let m = c.manager();
    if m.cluster().is_none() {
        return Err(err(
            StatusCode::CONFLICT,
            "not_clustered",
            "OVN runs cluster networks: make this host a cluster first",
        ));
    }
    let nodes: Vec<crate::node::Node> = m
        .nodes()
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|n| !n.status.phase.is_tombstone())
        .collect();
    let next = crate::ovn_settings::OvnSettings {
        enabled: b.enabled,
        underlay_mtu: b.underlay_mtu,
        nat_supernet: b.nat_supernet.filter(|s| !s.trim().is_empty()),
        edge: b.edge,
        snat_ct_zones: b.snat_ct_zones,
        dns_servers: b.dns_servers,
        updated_by: c.actor(),
        updated_at: crate::tenancy::now(),
    };
    next.validate(|w| nodes.iter().any(|n| n.meta.id == w || n.spec.name == w))
        .map_err(|m| err(StatusCode::BAD_REQUEST, "invalid", m))?;
    if let (true, Some(e)) = (next.enabled, &next.edge) {
        super::nodes::check_gateway_nodes(&nodes, &e.gateway_nodes, &e.physnet)
            .map_err(|m| err(StatusCode::CONFLICT, "gateway_not_ready", m))?;
    }
    if next.enabled {
        let missing: Vec<String> = nodes
            .iter()
            .filter_map(|n| ovn_missing(n).map(|why| format!("{}: {why}", n.spec.name)))
            .collect();
        if !missing.is_empty() {
            return Err(err(
                StatusCode::CONFLICT,
                "ovn_not_installed",
                format!(
                    "every node needs OVN first (run `sudo glidex-install --ovn` there): {}",
                    missing.join("; ")
                ),
            ));
        }
    } else {
        let nets: Vec<String> = m
            .networks_list()
            .into_iter()
            .filter(|n| n.scope == crate::network::NetworkScope::Cluster)
            .map(|n| n.name)
            .collect();
        if !nets.is_empty() {
            return Err(err(
                StatusCode::CONFLICT,
                "cluster_networks_exist",
                format!("delete the cluster networks first: {}", nets.join(", ")),
            ));
        }
    }
    crate::ovn_settings::write(&m.database(), &next).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "persistence_error",
            e.to_string(),
        )
    })?;
    m.apply_ovn_settings();
    Ok(Json(ovn_view(&c)))
}

/// `GET /cluster/settings/ovn/edge-defaults` (spec/clustering-ui.md §8a): a
/// shared edge that should work for this cluster, from what the nodes
/// report, and notes on what couldn't be worked out. Nothing is written.
pub async fn edge_defaults(c: Caller) -> Result<impl IntoResponse, ApiErr> {
    c.require_action("readCluster", Ent::Cluster, EntitySet::new(), &[])?;
    let nodes: Vec<crate::node::Node> = c
        .manager()
        .nodes()
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|n| !n.status.phase.is_tombstone())
        .collect();
    Ok(Json(suggest_edge(&nodes)))
}

/// The suggestion itself (pure, for tests).
pub fn suggest_edge(nodes: &[crate::node::Node]) -> serde_json::Value {
    use std::collections::BTreeMap;
    use std::net::Ipv4Addr;
    let mut notes: Vec<String> = Vec::new();
    // The physical network most nodes map; "uplink" when none does.
    let mut count: BTreeMap<&str, usize> = BTreeMap::new();
    for n in nodes {
        for p in n.status.features.physnets.keys() {
            *count.entry(p.as_str()).or_default() += 1;
        }
    }
    let physnet = count
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
        .map(|(p, _)| p.to_string());
    if physnet.is_none() {
        notes.push("No node maps a physical network yet: on a node's page, map one (e.g. uplink) to a bridge that has the uplink NIC.".into());
    }
    let physnet = physnet.unwrap_or_else(|| "uplink".to_string());
    // Gateway nodes: those that map it and run br-int on the kernel datapath, servers first.
    let mapping: Vec<&crate::node::Node> = nodes
        .iter()
        .filter(|n| n.status.features.physnets.contains_key(&physnet))
        .collect();
    let mut gateways: Vec<&crate::node::Node> = mapping
        .iter()
        .copied()
        .filter(|n| n.wanted_br_int() == "system")
        .collect();
    gateways.sort_by(|a, b| {
        (a.spec.role != NodeRole::Server)
            .cmp(&(b.spec.role != NodeRole::Server))
            .then(a.spec.name.cmp(&b.spec.name))
    });
    gateways.truncate(3);
    for n in mapping.iter().filter(|n| n.wanted_br_int() != "system") {
        notes.push(format!("{} maps {physnet} but runs br-int on the userspace datapath; set it to kernel on its page to make it a gateway node.", n.spec.name));
    }
    if gateways.is_empty() && !mapping.is_empty() {
        notes.push("No node can carry the edge yet (see above).".into());
    }
    // The LAN: from the host address an uplink moved onto the mapped bridge.
    let lan = gateways.iter().chain(mapping.iter()).find_map(|n| {
        let bridge = n.status.features.physnets.get(&physnet)?;
        let a = n.status.features.bridge_addrs.get(bridge)?;
        let (ip, prefix) = a.address.split_once('/')?;
        let (ip, prefix): (Ipv4Addr, u8) = (ip.parse().ok()?, prefix.parse().ok()?);
        Some((
            ip,
            prefix,
            a.gateway.clone(),
            n.spec.name.clone(),
            bridge.clone(),
        ))
    });
    let (mut cidr, mut gateway, mut external_ip, mut external_pool) =
        (String::new(), String::new(), String::new(), String::new());
    match &lan {
        Some((ip, prefix, gw, from, bridge)) if *prefix <= 30 => {
            let mask: u32 = if *prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
            let net = u32::from(*ip) & mask;
            let broadcast = net | !mask;
            cidr = format!("{}/{}", Ipv4Addr::from(net), prefix);
            match gw {
                Some(g) => gateway = g.clone(),
                None => notes.push(format!("{from}'s {bridge} has no default gateway: fill in the LAN's router.")),
            }
            // The highest host address nothing here uses (nodes, the gateway).
            let mut taken: Vec<u32> = vec![u32::from(*ip)];
            if let Ok(g) = gateway.parse::<Ipv4Addr>() {
                taken.push(u32::from(g));
            }
            for n in nodes {
                for a in n.status.features.bridge_addrs.values() {
                    if let Some(Ok(x)) = a.address.split('/').next().map(|s| s.parse::<Ipv4Addr>()) {
                        taken.push(u32::from(x));
                    }
                }
                for x in [n.status.advertise.map(|a| a.ip()), n.status.tunnel_ip].into_iter().flatten() {
                    if let std::net::IpAddr::V4(v4) = x {
                        taken.push(u32::from(v4));
                    }
                }
            }
            if let Some(free) = (net + 1..broadcast).rev().find(|h| !taken.contains(h)) {
                external_ip = Ipv4Addr::from(free).to_string();
                notes.push(format!("The edge address {external_ip} is a guess (the highest address no node uses): make sure no other device on {cidr} has it."));
                taken.push(free);
            }
            // VPC routers' addresses: the highest /29 (8 addresses) nothing
            // here uses, on a LAN big enough to spare one.
            if *prefix <= 26 {
                let block = (net..broadcast).step_by(8).rev().find(|b| b + 7 <= broadcast && *b != net && b + 7 != broadcast && !taken.iter().any(|t| (*b..=b + 7).contains(t)));
                if let Some(b) = block {
                    external_pool = format!("{}/29", Ipv4Addr::from(b));
                    notes.push(format!("The pool for VPC routers {external_pool} is a guess too: its 8 addresses must be free on {cidr}, ideally outside its DHCP range."));
                }
            }
        }
        Some((_, prefix, _, from, bridge)) => notes.push(format!("{from}'s {bridge} is on a /{prefix}: too small for an edge address; fill in the LAN.")),
        None if !mapping.is_empty() => notes.push(format!(
            "No node carries the host's address on its {physnet} bridge, so the LAN isn't known: add the uplink with \"move the host's address\", or fill in the external network, gateway and edge address."
        )),
        None => {}
    }
    let complete = !physnet.is_empty()
        && !gateways.is_empty()
        && !cidr.is_empty()
        && !gateway.is_empty()
        && !external_ip.is_empty();
    json!({
        "edge": {
            "physnet": physnet, "external_cidr": cidr, "gateway": gateway, "external_ip": external_ip,
            "external_pool": (!external_pool.is_empty()).then_some(external_pool), "gateway_nodes": gateways.iter().map(|n| n.spec.name.clone()).collect::<Vec<_>>(),
        },
        "complete": complete,
        "notes": notes,
    })
}

#[cfg(test)]
mod edge_tests {
    use super::suggest_edge;
    use crate::node::{BridgeAddr, Node, NodeRole, NodeSpec};

    fn node(
        name: &str,
        role: NodeRole,
        mapped: bool,
        dpdk: bool,
        addr: Option<(&str, Option<&str>)>,
    ) -> Node {
        let mut n = Node::new(
            name,
            NodeSpec {
                name: name.into(),
                role,
                unschedulable: false,
                labels: Default::default(),
                br_int_datapath: Default::default(),
                bridge_mappings: Default::default(),
                drop_spare_files_before: None,
            },
        );
        if mapped {
            n.status
                .features
                .physnets
                .insert("uplink".into(), "gxbr-up".into());
        }
        n.status.features.dpdk = dpdk;
        if let Some((a, gw)) = addr {
            n.status.features.bridge_addrs.insert(
                "gxbr-up".into(),
                BridgeAddr {
                    address: a.into(),
                    gateway: gw.map(String::from),
                },
            );
        }
        n
    }

    #[test]
    fn the_edge_comes_from_the_mapped_bridges_address() {
        let nodes = vec![
            node(
                "h2",
                NodeRole::Agent,
                true,
                true,
                Some(("192.0.2.11/24", Some("192.0.2.1"))),
            ),
            node(
                "h1",
                NodeRole::Server,
                true,
                false,
                Some(("192.0.2.10/24", Some("192.0.2.1"))),
            ),
            node("h3", NodeRole::Agent, false, false, None),
        ];
        let v = suggest_edge(&nodes);
        let e = &v["edge"];
        assert_eq!(e["physnet"], "uplink");
        assert_eq!(e["external_cidr"], "192.0.2.0/24");
        assert_eq!(e["gateway"], "192.0.2.1");
        assert_eq!(
            e["external_ip"], "192.0.2.254",
            "the highest address no node uses"
        );
        // .248/29 holds the edge address and the broadcast: the next /29 down.
        assert_eq!(e["external_pool"], "192.0.2.240/29");
        assert_eq!(
            e["gateway_nodes"],
            serde_json::json!(["h1"]),
            "h2 runs br-int on userspace (DPDK)"
        );
        assert_eq!(v["complete"], true);
        let notes = v["notes"].to_string();
        assert!(
            notes.contains("h2 maps uplink but runs br-int on the userspace datapath"),
            "{notes}"
        );
        assert!(notes.contains("is a guess"), "{notes}");
    }

    #[test]
    fn without_a_mapping_it_says_what_to_do_first() {
        let v = suggest_edge(&[node("h1", NodeRole::Server, false, false, None)]);
        assert_eq!(v["edge"]["physnet"], "uplink");
        assert_eq!(v["complete"], false);
        assert!(
            v["notes"]
                .to_string()
                .contains("No node maps a physical network"),
            "{v}"
        );
        // Mapped but the address was never moved onto the bridge: the LAN is unknown.
        let v = suggest_edge(&[node("h1", NodeRole::Server, true, false, None)]);
        assert_eq!(v["edge"]["gateway_nodes"], serde_json::json!(["h1"]));
        assert_eq!(v["edge"]["external_cidr"], "");
        assert!(v["notes"].to_string().contains("LAN isn't known"), "{v}");
    }
}
