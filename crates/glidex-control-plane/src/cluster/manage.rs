//! Cluster lifecycle (spec/clustering.md §5): init, join, status, promote,
//! snapshots and recovery.

use super::config::ClusterConfig;
use super::identity::{Files, Identity};
use super::net::{self, PeerClient};
use super::pki::{self, Ca, NodeKey};
use super::raft::raft_id_of;
use super::runtime::{Cluster, ClusterError};
use super::server::{JoinRequest, JoinResponse};
use super::tokens::{self, TokenKind};
use crate::node::{Node, NodeRole, NodeSpec, NodeStore};
use crate::state::VmManager;
use crate::store::{Db, Origin, TableId};
use bytes::Bytes;
use redb::ReadableTable;
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

fn other(e: impl std::fmt::Display) -> ClusterError {
    ClusterError::Other(e.to_string())
}

/// The address other nodes will use for this one: what was asked for, else
/// the configured one, else the listen address if it names a host, else the
/// first non-loopback address of this host.
pub fn resolve_advertise(requested: Option<SocketAddr>, cfg: &ClusterConfig) -> Result<SocketAddr, ClusterError> {
    if let Some(a) = requested.or(cfg.advertise) {
        return Ok(a);
    }
    if !cfg.listen.ip().is_unspecified() {
        return Ok(cfg.listen);
    }
    let ip = glidex_tls::interface_ips()
        .ok()
        .and_then(|v| v.into_iter().find(|i| !i.is_loopback() && !i.is_unspecified() && i.is_ipv4()))
        .ok_or_else(|| other("cannot tell which address other nodes should use: pass --advertise <ip:port>"))?;
    Ok(SocketAddr::new(ip, cfg.listen.port()))
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "localhost".into())
}

pub struct InitOptions {
    pub advertise: Option<SocketAddr>,
    pub tunnel_ip: Option<IpAddr>,
    /// Where the node API binds; default `cluster.listen`.
    pub listen: Option<SocketAddr>,
}

/// Re-key a node's records from `from` to `to` in one write: its VMs'
/// placement, its disks, its node networks and its `nodes` record. Used by
/// init (`local` → the new node id) and as its own inverse.
pub fn rekey_node(db: &Db, from: &str, to: &str, new_node: Option<Node>) -> Result<(), ClusterError> {
    db.write(Origin::Migration, |tx| -> Result<(), ClusterError> {
        let fix = |table: TableId, f: &dyn Fn(&mut Value) -> bool| -> Result<(), ClusterError> {
            let mut t = tx.open_table(table.definition()).map_err(other)?;
            let mut changed = Vec::new();
            for r in t.iter().map_err(other)? {
                let (k, v) = r.map_err(other)?;
                if let Ok(mut j) = serde_json::from_slice::<Value>(v.value()) {
                    if f(&mut j) {
                        changed.push((k.value().to_string(), serde_json::to_vec(&j).map_err(other)?));
                    }
                }
            }
            for (k, v) in changed {
                t.insert(&k, &v).map_err(other)?;
            }
            Ok(())
        };
        fix(TableId::Vms, &|j| {
            let Some(p) = j.pointer_mut("/status/placement") else { return false };
            if p["node"] == from {
                p["node"] = to.into();
                return true;
            }
            false
        })?;
        for t in [TableId::Disks, TableId::Networks] {
            fix(t, &|j| {
                // A standalone host's disk with no node is its own.
                if j["node"] == from || (t == TableId::Disks && j["node"].is_null() && from == crate::node::LOCAL_NODE) {
                    j["node"] = to.into();
                    return true;
                }
                false
            })?;
        }
        let mut nodes = tx.open_table(TableId::Nodes.definition()).map_err(other)?;
        nodes.remove(from).map_err(other)?;
        if let Some(n) = new_node {
            nodes.insert(&n.meta.id, serde_json::to_vec(&n).map_err(other)?.as_slice()).map_err(other)?;
        }
        Ok(())
    })
}

/// §5.1: turn this standalone host into a cluster of one.
pub async fn init(manager: &Arc<VmManager>, cfg: &crate::config::Config, opts: InitOptions) -> Result<Value, ClusterError> {
    let files = Files::beside(manager.db_path());
    if manager.cluster().is_some() || files.exists() {
        return Err(other("this host is already part of a cluster"));
    }
    manager.migrate_schema().map_err(other)?;
    let db = manager.database();
    let ccfg = cfg.cluster.clone();
    let advertise = resolve_advertise(opts.advertise, &ccfg)?;
    let listen = opts.listen.unwrap_or(ccfg.listen);
    let cluster_id = uuid::Uuid::new_v4().to_string();
    let node_id = uuid::Uuid::new_v4().to_string();
    let raft_id = raft_id_of(&node_id);

    // 1. Backup: what an init that went wrong goes back to.
    let backup = manager.db_path().with_extension(format!("db.pre-cluster-{}", crate::tenancy::now()));
    {
        let (src, dst) = (manager.db_path().to_path_buf(), backup.clone());
        db.quiesce(|| -> std::io::Result<()> {
            std::fs::copy(&src, &dst)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o600))
        })
        .map_err(other)?;
    }

    // 2. PKI.
    let ca = Ca::generate(&cluster_id)?;
    let key = NodeKey::generate()?;
    let csr = key.csr(&[advertise.ip()], &[])?;
    let (cert_pem, info) = ca.sign_node(&csr, &node_id, true, pki::NODE_VALIDITY_DAYS)?;
    let identity = Identity {
        cluster_id: cluster_id.clone(),
        node_id: node_id.clone(),
        raft_id,
        name: hostname(),
        role: NodeRole::Server,
        advertise,
        tunnel_ip: opts.tunnel_ip.or(Some(advertise.ip())),
        ca_fingerprint: ca.public_key_fingerprint()?,
        seeds: Vec::new(),
    };

    // 3. Re-key host-scoped data in a single write (§5.1 step 4). A failure
    //    from here on puts it back.
    let nodes = NodeStore::new(db.clone());
    let mut me = match nodes.get(crate::node::LOCAL_NODE).map_err(other)? {
        Some(n) => n,
        None => Node::new(crate::node::LOCAL_NODE, NodeSpec { name: identity.name.clone(), role: NodeRole::Server, unschedulable: false, labels: Default::default() }),
    };
    me.meta.id = node_id.clone();
    me.meta.name = identity.name.clone();
    me.spec.role = NodeRole::Server;
    me.status.raft_id = Some(raft_id);
    me.status.advertise = Some(advertise);
    me.status.tunnel_ip = identity.tunnel_ip;
    rekey_node(&db, crate::node::LOCAL_NODE, &node_id, Some(me))?;
    let rollback = |e: ClusterError| {
        let _ = rekey_node(&db, &node_id, crate::node::LOCAL_NODE, None);
        files.delete_all();
        e
    };

    // 4. Files, identity last: it is what makes the next start clustered.
    files.save_node_cert(&cert_pem, &key.key_pem(), &ca.cert_pem).map_err(|e| rollback(e.into()))?;
    files.save_ca_key(ca.key_pem()).map_err(|e| rollback(e.into()))?;
    files.save_identity(&identity).map_err(|e| rollback(e.into()))?;

    // 5. Raft: a cluster of one, then the writes that only a cluster has.
    crate::authz::set_identity(&cluster_id, &node_id);
    crate::authz::set_local_account_scope((!ccfg.shared_local_accounts).then(|| node_id.clone()));
    let cluster = Cluster::start(db.clone(), files.clone(), identity.clone(), ccfg, listen).await.map_err(|e| rollback(e))?;
    let node = cluster.node.as_ref().expect("a server has a Raft member");
    node.bootstrap(&advertise.to_string()).await.map_err(|e| rollback(e.into()))?;
    node.wait_for_leader(Duration::from_secs(20)).await.map_err(|e| rollback(e.into()))?;
    node.seal_baseline().await.map_err(|e| rollback(e.into()))?;
    let (cid, fp) = (cluster_id.clone(), info.serial.clone());
    let dbc = db.clone();
    tokio::task::spawn_blocking(move || {
        dbc.write(Origin::Migration, |tx| -> Result<(), ClusterError> {
            let mut m = tx.open_table(TableId::Meta.definition()).map_err(other)?;
            m.insert("cluster_id", cid.as_bytes()).map_err(other)?;
            m.insert("feature_level", b"1".as_slice()).map_err(other)?;
            tx.open_table(TableId::IssuedCerts.definition())
                .map_err(other)?
                .insert(&fp, serde_json::to_vec(&json!({ "node": info.node_id, "kind": "node", "issuer": info.issuer_fingerprint, "not_after": info.not_after })).unwrap().as_slice())
                .map_err(other)?;
            Ok(())
        })
    })
    .await
    .map_err(other)??;
    // Identities of this host's local accounts are scoped to it from now on (D13).
    if !cluster.config.shared_local_accounts {
        let n = node_id.clone();
        let auth_db = db.clone();
        tokio::task::spawn_blocking(move || crate::auth::store::IdentityStore::new(auth_db).and_then(|s| s.scope_local_identities(&n)))
            .await
            .map_err(other)?
            .map_err(other)?;
    }
    manager.attach_cluster(cluster.clone());
    manager.reload_vms().await;
    Ok(json!({ "cluster_id": cluster_id, "node_id": node_id, "advertise": advertise.to_string(), "backup": backup.display().to_string(), "ca_fingerprint": identity.ca_fingerprint }))
}

pub struct JoinOptions {
    pub server: SocketAddr,
    pub token: String,
    pub role: NodeRole,
    pub advertise: Option<SocketAddr>,
    pub tunnel_ip: Option<IpAddr>,
    pub name: Option<String>,
    pub listen: Option<SocketAddr>,
}

/// §5.2: join the cluster `token` belongs to, as a new node.
pub async fn join(manager: &Arc<VmManager>, cfg: &crate::config::Config, opts: JoinOptions) -> Result<Value, ClusterError> {
    let files = Files::beside(manager.db_path());
    if manager.cluster().is_some() || files.exists() {
        return Err(other("this host is already part of a cluster"));
    }
    // A plain join refuses a host with resources: it could only abandon or
    // duplicate them (§5.2).
    if manager.has_resources().await {
        return Err(other("this host has VMs, disks, networks or credentials: a plain join would abandon them (importing them is a later feature)"));
    }
    let (_, _, ca_hash) = tokens::parse(&opts.token).map_err(other)?;
    let server = opts.server.to_string();

    // Pin the CA by the hash in the token before sending anything.
    let ca_pem = net::fetch_ca_untrusted(&server).await?;
    let got = pki::public_key_fingerprint(&ca_pem)?;
    if got != ca_hash {
        return Err(other(format!("the server's CA ({}…) is not the one the token names ({}…): refusing to join", &got[..12], &ca_hash[..12])));
    }
    let trusting = net::client_trusting(&ca_pem)?;

    let ccfg = cfg.cluster.clone();
    let advertise = resolve_advertise(opts.advertise, &ccfg)?;
    let listen = opts.listen.unwrap_or(ccfg.listen);
    let node_id = uuid::Uuid::new_v4().to_string();
    let raft_id = raft_id_of(&node_id);
    let name = opts.name.unwrap_or_else(hostname);
    let key = NodeKey::generate()?;
    let csr = key.csr(&[advertise.ip()], &[])?;
    let req = JoinRequest { token: opts.token.clone(), csr, name: name.clone(), role: opts.role, node_id: node_id.clone(), raft_id, advertise, tunnel_ip: opts.tunnel_ip };
    let body = Bytes::from(serde_json::to_vec(&req).map_err(other)?);

    let mut target = server.clone();
    let mut resp = None;
    for _ in 0..2 {
        let r = trusting.request(&target, hyper::Method::POST, "/cluster/v1/join", &[("content-type", "application/json".into())], body.clone()).await?;
        if r.status == hyper::StatusCode::MISDIRECTED_REQUEST {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            match v["leader"].as_str() {
                Some(l) if l != target => {
                    target = l.to_string();
                    continue;
                }
                _ => return Err(other("the cluster has no leader right now; try again")),
            }
        }
        if !r.status.is_success() {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            return Err(other(format!("{}: {}", r.status, v["message"].as_str().unwrap_or("the join was refused"))));
        }
        resp = Some(r);
        break;
    }
    let resp: JoinResponse = serde_json::from_slice(&resp.ok_or_else(|| other("no answer from the cluster"))?.body).map_err(other)?;
    if pki::public_key_fingerprint(&resp.trust_pem)? != ca_hash {
        return Err(other("the cluster sent a different CA than the token names"));
    }

    let identity = Identity {
        cluster_id: resp.cluster_id.clone(),
        node_id: node_id.clone(),
        raft_id,
        name,
        role: opts.role,
        advertise,
        tunnel_ip: opts.tunnel_ip.or(Some(advertise.ip())),
        ca_fingerprint: ca_hash.to_string(),
        seeds: vec![target.clone()],
    };
    files.save_node_cert(&resp.cert_pem, &key.key_pem(), &resp.trust_pem)?;
    files.save_identity(&identity)?;
    crate::authz::set_identity(&identity.cluster_id, &identity.node_id);
    crate::authz::set_local_account_scope((!ccfg.shared_local_accounts).then(|| node_id.clone()));
    let db = manager.database();
    let cluster = match Cluster::start(db, files.clone(), identity.clone(), ccfg, listen).await {
        Ok(c) => c,
        Err(e) => {
            files.delete_all();
            return Err(e);
        }
    };

    // Servers ask the leader to add them to the Raft group; it brings them up
    // to date from a snapshot. This can take as long as the database is big.
    let mut promoted = false;
    if opts.role == NodeRole::Server {
        let r = cluster.client.request_timeout(&target, hyper::Method::POST, "/cluster/v1/join/ready", &[], Bytes::new(), Duration::from_secs(900)).await;
        let ok = match r {
            Ok(r) if r.status.is_success() => {
                promoted = serde_json::from_slice::<Value>(&r.body).ok().and_then(|v| v["promoted"].as_bool()).unwrap_or(false);
                true
            }
            Ok(r) => {
                tracing::error!("the leader refused to add this node: {} {}", r.status, String::from_utf8_lossy(&r.body));
                false
            }
            Err(e) => {
                tracing::error!("could not reach the leader: {}", e);
                false
            }
        };
        if !ok {
            cluster.stop().await;
            files.delete_all();
            return Err(other("the leader could not add this node to the cluster"));
        }
        if let Some(n) = &cluster.node {
            let _ = n.wait_for_leader(Duration::from_secs(20)).await;
        }
    }
    manager.attach_cluster(cluster);
    manager.reload_vms().await;
    Ok(json!({ "cluster_id": identity.cluster_id, "node_id": node_id, "role": opts.role, "promoted": promoted }))
}

pub struct RejoinOptions {
    pub server: SocketAddr,
    pub token: String,
    /// The node's id, for a host that lost its cluster files.
    pub node_id: Option<String>,
    pub advertise: Option<SocketAddr>,
    pub tunnel_ip: Option<IpAddr>,
    pub listen: Option<SocketAddr>,
}

/// §5.7: come back as the same node, with a new certificate. A server whose
/// Raft state can't be trusted (D18) starts it from nothing and returns as a
/// learner; one whose state is intact simply resumes.
pub async fn rejoin(manager: &Arc<VmManager>, cfg: &crate::config::Config, opts: RejoinOptions) -> Result<Value, ClusterError> {
    let files = Files::beside(manager.db_path());
    let old = files.load_identity()?;
    let node_id = opts.node_id.clone().or_else(|| old.as_ref().map(|i| i.node_id.clone())).ok_or_else(|| other("this host has no cluster identity: pass the node id from `gxctl node rejoin-token`"))?;
    let (_, _, ca_hash) = tokens::parse(&opts.token).map_err(other)?;
    let server = opts.server.to_string();
    let ca_pem = net::fetch_ca_untrusted(&server).await?;
    let got = pki::public_key_fingerprint(&ca_pem)?;
    if got != ca_hash {
        return Err(other(format!("the server's CA ({}…) is not the one the token names ({}…): refusing to rejoin", &got[..12], &ca_hash[..12])));
    }
    let trusting = net::client_trusting(&ca_pem)?;
    let ccfg = cfg.cluster.clone();
    let advertise = match (opts.advertise, &old) {
        (Some(a), _) => a,
        (None, Some(i)) => i.advertise,
        (None, None) => resolve_advertise(None, &ccfg)?,
    };
    let listen = opts.listen.unwrap_or(ccfg.listen);
    let key = NodeKey::generate()?;
    let csr = key.csr(&[advertise.ip()], &[])?;
    let tunnel_ip = opts.tunnel_ip.or_else(|| old.as_ref().and_then(|i| i.tunnel_ip));
    let req = JoinRequest {
        token: opts.token.clone(),
        csr,
        name: old.as_ref().map(|i| i.name.clone()).unwrap_or_default(),
        role: old.as_ref().map(|i| i.role).unwrap_or(NodeRole::Server),
        node_id: node_id.clone(),
        raft_id: old.as_ref().map(|i| i.raft_id).unwrap_or_else(|| raft_id_of(&node_id)),
        advertise,
        tunnel_ip,
    };
    let body = Bytes::from(serde_json::to_vec(&req).map_err(other)?);
    let mut target = server.clone();
    let mut resp = None;
    for _ in 0..2 {
        let r = trusting.request(&target, hyper::Method::POST, "/cluster/v1/rejoin", &[("content-type", "application/json".into())], body.clone()).await?;
        if r.status == hyper::StatusCode::MISDIRECTED_REQUEST {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            match v["leader"].as_str() {
                Some(l) if l != target => {
                    target = l.to_string();
                    continue;
                }
                _ => return Err(other("the cluster has no leader right now; try again")),
            }
        }
        if !r.status.is_success() {
            let v: Value = serde_json::from_slice(&r.body).unwrap_or_default();
            return Err(other(format!("{}: {}", r.status, v["message"].as_str().unwrap_or("the rejoin was refused"))));
        }
        resp = Some(r);
        break;
    }
    let resp: JoinResponse = serde_json::from_slice(&resp.ok_or_else(|| other("no answer from the cluster"))?.body).map_err(other)?;
    if pki::public_key_fingerprint(&resp.trust_pem)? != ca_hash {
        return Err(other("the cluster sent a different CA than the token names"));
    }
    let role = resp.role.unwrap_or(req.role);
    let raft_id = resp.raft_id.unwrap_or(req.raft_id);
    let identity = Identity {
        cluster_id: resp.cluster_id.clone(),
        node_id: node_id.clone(),
        raft_id,
        name: resp.name.clone().unwrap_or_else(|| req.name.clone()),
        role,
        advertise,
        tunnel_ip: tunnel_ip.or(Some(advertise.ip())),
        ca_fingerprint: ca_hash.to_string(),
        seeds: vec![target.clone()],
    };
    // The old runtime goes before its files change.
    manager.detach_cluster().await;
    files.save_node_cert(&resp.cert_pem, &key.key_pem(), &resp.trust_pem)?;
    files.save_identity(&identity)?;
    crate::authz::set_identity(&identity.cluster_id, &identity.node_id);
    crate::authz::set_local_account_scope((!ccfg.shared_local_accounts).then(|| node_id.clone()));
    let db = manager.database();
    if resp.raft_fresh {
        let raft_dir = files.raft();
        if raft_dir.exists() {
            std::fs::remove_dir_all(&raft_dir).map_err(other)?;
        }
        reset_raft_meta(&db)?;
    }
    let cluster = Cluster::start(db, files.clone(), identity.clone(), ccfg, listen).await?;
    if role == NodeRole::Server && resp.raft_fresh {
        let r = cluster.client.request_timeout(&target, hyper::Method::POST, "/cluster/v1/join/ready", &[], Bytes::new(), Duration::from_secs(900)).await;
        match r {
            Ok(r) if r.status.is_success() => {}
            Ok(r) => {
                cluster.stop().await;
                return Err(other(format!("the leader refused to add this node back: {} {}", r.status, String::from_utf8_lossy(&r.body))));
            }
            Err(e) => {
                cluster.stop().await;
                return Err(other(format!("could not reach the leader: {e}")));
            }
        }
        if let Some(n) = &cluster.node {
            let _ = n.wait_for_leader(Duration::from_secs(20)).await;
        }
    }
    manager.attach_cluster(cluster);
    manager.reload_vms().await;
    Ok(json!({ "cluster_id": identity.cluster_id, "node_id": node_id, "role": role, "raft_fresh": resp.raft_fresh }))
}

/// A join token for a new node (§5.2). `by` is recorded.
pub fn join_token(cluster: &Cluster, role: NodeRole, ttl_secs: u64, allow_import: bool, by: &str) -> Result<String, ClusterError> {
    tokens::create(&cluster.db, TokenKind::Join { role, allow_import }, ttl_secs, &cluster.identity.ca_fingerprint, by).map_err(other)
}

/// What `gxctl cluster status` shows.
pub fn status_of(c: &Cluster) -> Value {
    let nodes = NodeStore::new(c.db.clone()).list().unwrap_or_default();
    let name_of = |raft_id: u64| nodes.iter().find(|n| n.status.raft_id == Some(raft_id)).map(|n| n.spec.name.clone());
    let addr_of = |raft_id: u64| nodes.iter().find(|n| n.status.raft_id == Some(raft_id)).and_then(|n| n.status.advertise).map(|a| a.to_string());
    let raft = c.node.as_ref().map(|n| {
        let m = n.raft.metrics().borrow().clone();
        let member = |id: u64| json!({ "raft_id": id, "name": name_of(id), "address": addr_of(id) });
        json!({
            "id": n.id,
            "state": format!("{:?}", m.state),
            "term": m.current_term,
            "leader": m.current_leader.map(member),
            "voters": n.voters().into_iter().map(member).collect::<Vec<_>>(),
            "learners": n.learners().into_iter().map(member).collect::<Vec<_>>(),
            "last_log_index": m.last_log_index,
            "last_applied": m.last_applied.map(|l| l.index),
            "purged": m.purged.map(|l| l.index),
            "replication": m.replication.as_ref().map(|r| r.iter().map(|(id, l)| json!({ "raft_id": id, "name": name_of(*id), "matched": l.as_ref().map(|l| l.index) })).collect::<Vec<_>>()),
        })
    });
    json!({
        "cluster_id": c.identity.cluster_id,
        "node_id": c.identity.node_id,
        "name": c.identity.name,
        "role": c.identity.role,
        "advertise": c.identity.advertise.to_string(),
        "ca_fingerprint": c.identity.ca_fingerprint,
        "raft": raft,
        "nodes": nodes.iter().map(|n| json!({
            "id": n.meta.id, "name": n.spec.name, "role": n.spec.role, "phase": n.status.phase,
            "advertise": n.status.advertise.map(|a| a.to_string()), "raft_id": n.status.raft_id,
        })).collect::<Vec<_>>(),
    })
}

/// Promote learners to voters (§5.11): the new voter count must be 1, 3 or 5
/// unless `force`.
pub async fn promote(c: &Cluster, node_names: &[String], force: bool) -> Result<Value, ClusterError> {
    let node = c.node.as_ref().ok_or_else(|| other("this node is not a server"))?;
    if !node.is_leader() {
        return Err(other("this is not the leader"));
    }
    let nodes = NodeStore::new(c.db.clone()).list().map_err(other)?;
    let mut voters = node.voters();
    for name in node_names {
        let n = nodes.iter().find(|n| &n.spec.name == name || &n.meta.id == name).ok_or_else(|| other(format!("no node {name}")))?;
        let id = n.status.raft_id.ok_or_else(|| other(format!("{name} is not a server")))?;
        if !node.learners().contains(&id) && !voters.contains(&id) {
            return Err(other(format!("{name} is not a member yet")));
        }
        voters.insert(id);
    }
    if !force && ![1, 3, 5].contains(&voters.len()) {
        return Err(other(format!("that would make {} voters; voters are 1, 3 or 5 (use --force to override)", voters.len())));
    }
    node.set_voters(voters.clone()).await?;
    Ok(json!({ "voters": voters.len() }))
}

/// Write a consistent snapshot of the replicated tables to `path` (0600),
/// for `gxctl cluster snapshot` (§5.12).
pub fn export_snapshot(db: &Db, path: &std::path::Path) -> Result<u64, ClusterError> {
    use std::os::unix::fs::OpenOptionsExt;
    let view = db.snapshot_view().map_err(other)?;
    let f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path).map_err(other)?;
    let mut w = std::io::BufWriter::new(f);
    view.write_to(&mut w).map_err(other)
}

/// A client that keeps no cluster state: for tests and the CLI's own checks.
pub fn peer_client(cluster: &Cluster) -> Arc<PeerClient> {
    cluster.client.clone()
}

struct NoTransport;

#[async_trait::async_trait]
impl super::raft::Transport for NoTransport {
    async fn call(&self, _: u64, _: &str, _: super::raft::Rpc, _: Vec<u8>) -> Result<Vec<u8>, super::raft::TransportError> {
        Err(super::raft::TransportError("recovery runs alone".into()))
    }
    async fn snapshot(&self, _: u64, _: &str, _: Vec<u8>, _: std::path::PathBuf) -> Result<Vec<u8>, super::raft::TransportError> {
        Err(super::raft::TransportError("recovery runs alone".into()))
    }
}

/// `--force-new-cluster` (§5.12): with a majority of servers destroyed,
/// start a new single-voter cluster on this server from its own database or
/// from a snapshot. The identity, certificates and CA stay; the Raft log is
/// replaced by a fresh one whose only member is this node. Other servers
/// re-join with fresh Raft state (D18). Run while the control plane is stopped.
pub async fn force_new_cluster(db_path: &std::path::Path, from: Option<&std::path::Path>, cfg: &ClusterConfig) -> Result<(), ClusterError> {
    let files = Files::beside(db_path);
    let identity = files.load_identity()?.ok_or_else(|| other("this host has no cluster identity"))?;
    if identity.role != NodeRole::Server {
        return Err(other("only a server can start a new cluster"));
    }
    let db = Arc::new(Db::create(db_path).map_err(other)?);
    if let Some(snap) = from {
        let mut f = std::io::BufReader::new(std::fs::File::open(snap).map_err(other)?);
        db.install_dump(&mut f, 0, |txn| {
            let mut t = txn.open_table(TableId::RaftMeta.definition())?;
            t.remove(crate::store::LAST_APPLIED)?;
            t.remove(crate::store::LAST_MEMBERSHIP)?;
            Ok(())
        })
        .map_err(other)?;
    } else {
        let txn = db.begin_read().map_err(other)?;
        drop(txn);
        // Forget the old log position: the new log starts again.
        reset_raft_meta(&db)?;
    }
    let raft_dir = files.raft();
    if raft_dir.exists() {
        std::fs::remove_dir_all(&raft_dir).map_err(other)?;
    }
    let node = super::raft::ClusterNode::start(db.clone(), &raft_dir, identity.raft_id, &cfg.raft_settings(), Arc::new(NoTransport)).await?;
    node.bootstrap(&identity.advertise.to_string()).await?;
    node.wait_for_leader(Duration::from_secs(20)).await?;
    node.seal_baseline().await?;
    node.shutdown().await;
    Ok(())
}

fn reset_raft_meta(db: &Db) -> Result<(), ClusterError> {
    // `raft_meta` is local: it is written outside the replicated path.
    db.apply(&crate::store::WriteSet { format: crate::store::WRITE_SET_FORMAT, origin: Origin::System, ops: Vec::new() }, 0, |txn| {
        let mut t = txn.open_table(TableId::RaftMeta.definition())?;
        t.remove(crate::store::LAST_APPLIED)?;
        t.remove(crate::store::LAST_MEMBERSHIP)?;
        Ok(())
    })
    .map_err(other)
}
