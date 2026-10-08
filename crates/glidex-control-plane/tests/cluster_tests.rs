//! Three control planes on one machine, each with its own directory and
//! ports, joined over real mutual TLS (spec/clustering.md §17 C2): writes
//! through any of them reach the leader, reads see them, and killing the
//! leader doesn't stop the cluster.
//!
//! The process-wide identity in `authz` is shared by the three, which is
//! harmless here because authentication is off.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use glidex_control_plane::api::create_router;
use glidex_control_plane::cluster::manage::{self, InitOptions, JoinOptions};
use glidex_control_plane::cluster::Cluster;
use glidex_control_plane::config::Config;
use glidex_control_plane::node::NodeRole;
use glidex_control_plane::state::VmManager;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

struct Cp {
    manager: Arc<VmManager>,
    router: axum::Router,
    addr: SocketAddr,
    _dir: tempfile::TempDir,
}

/// The identity in `authz` is process-wide, so these tests run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn test_config() -> Config {
    let mut c = Config::default();
    c.cluster.raft.heartbeat_ms = 100;
    c.cluster.raft.election_ms = (600, 1200);
    c
}

fn new_cp() -> Cp {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_test_writer().try_init();
    let dir = tempfile::tempdir().unwrap();
    let manager = VmManager::with_db_path(dir.path().join("glidex.db")).unwrap();
    let router = create_router(manager.clone());
    manager.set_api_router(router.clone());
    Cp { manager, router, addr: format!("127.0.0.1:{}", free_port()).parse().unwrap(), _dir: dir }
}

async fn call(cp: &Cp, method: &str, uri: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = cp.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn cluster(cp: &Cp) -> Arc<Cluster> {
    cp.manager.cluster().expect("clustered")
}

async fn form(n: usize) -> Vec<Cp> {
    let cfg = test_config();
    let first = new_cp();
    manage::init(&first.manager, &cfg, InitOptions { advertise: Some(first.addr), tunnel_ip: None, listen: Some(first.addr) }).await.unwrap();
    first.manager.initialize().await.unwrap();
    let mut cps = vec![first];
    for _ in 1..n {
        let cp = new_cp();
        let token = manage::join_token(&cluster(&cps[0]), NodeRole::Server, 600, false, "test").unwrap();
        manage::join(
            &cp.manager,
            &cfg,
            JoinOptions { server: cps[0].addr, token, role: NodeRole::Server, advertise: Some(cp.addr), tunnel_ip: None, name: Some(format!("cp{}", cps.len() + 1)), listen: Some(cp.addr) },
        )
        .await
        .unwrap();
        cps.push(cp);
    }
    cps
}

async fn leader_index(cps: &[Cp], among: &[usize]) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        for &i in among {
            if cluster(&cps[i]).is_leader() {
                return i;
            }
        }
        assert!(tokio::time::Instant::now() < deadline, "no leader among {among:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// POST /projects until it is accepted (a write right after an election can
/// answer 503 while the new leader settles).
async fn create_project(cp: &Cp, name: &str) -> (StatusCode, Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (s, v) = call(cp, "POST", "/projects", Some(json!({ "name": name })), &[]).await;
        if s != StatusCode::SERVICE_UNAVAILABLE || tokio::time::Instant::now() > deadline {
            return (s, v);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn project_names(cp: &Cp) -> Vec<String> {
    let (s, v) = call(cp, "GET", "/projects", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn three_control_planes_survive_losing_the_leader() {
    let _one = SERIAL.lock().await;
    let cps = form(3).await;

    for (i, cp) in cps.iter().enumerate() {
        let m = cluster(cp).node.as_ref().unwrap().raft.metrics().borrow().clone();
        eprintln!("cp{} snapshot={:?} purged={:?} applied={:?} last_log={:?}", i + 1, m.snapshot, m.purged, m.last_applied, m.last_log_index);
    }
    // The group has three voters and one leader everyone agrees on.
    let status = manage::status_of(&cluster(&cps[0]));
    assert_eq!(status["raft"]["voters"].as_array().unwrap().len(), 3, "{status}");
    let leader = leader_index(&cps, &[0, 1, 2]).await;
    let follower = (0..3).find(|i| *i != leader).unwrap();
    assert!(!cluster(&cps[follower]).is_leader());

    // A write through a follower is forwarded; a read through the other
    // follower sees it (linearizable).
    let (s, v) = call(&cps[follower], "POST", "/projects", Some(json!({ "name": "alpha" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let other = (0..3).find(|i| *i != leader && *i != follower).unwrap();
    assert!(project_names(&cps[other]).await.contains(&"alpha".to_string()));
    assert!(project_names(&cps[leader]).await.contains(&"alpha".to_string()));

    // Kill the leader: the other two elect one and writes continue.
    cluster(&cps[leader]).stop().await;
    let rest: Vec<usize> = (0..3).filter(|i| *i != leader).collect();
    let new_leader = leader_index(&cps, &rest).await;
    assert_ne!(new_leader, leader);
    let via = rest.iter().copied().find(|i| *i != new_leader).unwrap();
    let (s, v) = create_project(&cps[via], "beta").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    for &i in &rest {
        let names = project_names(&cps[i]).await;
        assert!(names.contains(&"alpha".to_string()) && names.contains(&"beta".to_string()), "{i}: {names:?}");
    }
    for (i, cp) in cps.iter().enumerate() {
        let d = cp.manager.database().dump().unwrap();
        let dp = d.iter().flat_map(|(_, r)| r.iter()).find(|(k, _)| k == "default_project").map(|(_, v)| String::from_utf8_lossy(v).into_owned());
        eprintln!("cp{} default_project={:?} leader={} applied={}", i + 1, dp, i == leader, cp.manager.database().revision());
    }
    let (a, b) = (cps[rest[0]].manager.database().dump().unwrap(), cps[rest[1]].manager.database().dump().unwrap());
    if a != b {
        for ((ta, ra), (tb, rb)) in a.iter().zip(b.iter()) {
            for (k, v) in ra {
                if rb.iter().find(|(k2, _)| k2 == k).map(|(_, v2)| v2) != Some(v) {
                    eprintln!("DIFF in {ta:?} {tb:?}: {k} {:?}", String::from_utf8_lossy(v));
                }
            }
        }
        panic!("the replicas differ");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn reads_without_a_quorum_fail_unless_stale_is_asked_for() {
    let _one = SERIAL.lock().await;
    let cps = form(3).await;
    let leader = leader_index(&cps, &[0, 1, 2]).await;
    let (s, _) = call(&cps[leader], "POST", "/projects", Some(json!({ "name": "kept" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED);
    let rest: Vec<usize> = (0..3).filter(|i| *i != leader).collect();
    // Wait for both followers to have it, then take both servers down.
    for &i in &rest {
        assert!(project_names(&cps[i]).await.contains(&"kept".to_string()));
    }
    let survivor = rest[0];
    cluster(&cps[leader]).stop().await;
    cluster(&cps[rest[1]]).stop().await;
    // One server of three: no quorum.
    let (s, v) = call(&cps[survivor], "GET", "/projects", None, &[]).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["error"], "cluster_unavailable");
    // A stale read is allowed and says which revision it saw.
    let resp = cps[survivor]
        .router
        .clone()
        .oneshot(Request::builder().uri("/projects").header("x-glidex-consistency", "local").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("x-glidex-revision"));
    let (s, v) = call(&cps[survivor], "POST", "/projects", Some(json!({ "name": "nope" })), &[]).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_join_needs_the_right_token_and_the_pinned_ca() {
    let _one = SERIAL.lock().await;
    let cfg = test_config();
    let first = new_cp();
    manage::init(&first.manager, &cfg, InitOptions { advertise: Some(first.addr), tunnel_ip: None, listen: Some(first.addr) }).await.unwrap();
    let c = cluster(&first);
    let good = manage::join_token(&c, NodeRole::Server, 600, false, "test").unwrap();

    // A token for another CA's hash is refused before anything is sent.
    let evil = format!("{}.{}", &good[..good.rfind('.').unwrap()], "0".repeat(64));
    let cp = new_cp();
    let err = manage::join(&cp.manager, &cfg, JoinOptions { server: first.addr, token: evil, role: NodeRole::Server, advertise: Some(cp.addr), tunnel_ip: None, name: Some("cp2".into()), listen: Some(cp.addr) })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not the one the token names"), "{err}");

    // An agent token can't bring in a server.
    let agent = manage::join_token(&c, NodeRole::Agent, 600, false, "test").unwrap();
    let err = manage::join(&cp.manager, &cfg, JoinOptions { server: first.addr, token: agent, role: NodeRole::Server, advertise: Some(cp.addr), tunnel_ip: None, name: Some("cp2".into()), listen: Some(cp.addr) })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("token is for"), "{err}");

    // The good token works once.
    manage::join(&cp.manager, &cfg, JoinOptions { server: first.addr, token: good.clone(), role: NodeRole::Server, advertise: Some(cp.addr), tunnel_ip: None, name: Some("cp2".into()), listen: Some(cp.addr) })
        .await
        .unwrap();
    let again = new_cp();
    let err = manage::join(&again.manager, &cfg, JoinOptions { server: first.addr, token: good, role: NodeRole::Server, advertise: Some(again.addr), tunnel_ip: None, name: Some("cp3".into()), listen: Some(again.addr) })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("already used"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_new_cluster_starts_from_a_snapshot() {
    let _one = SERIAL.lock().await;
    let cfg = test_config();
    let first = new_cp();
    manage::init(&first.manager, &cfg, InitOptions { advertise: Some(first.addr), tunnel_ip: None, listen: Some(first.addr) }).await.unwrap();
    let (s, _) = call(&first, "POST", "/projects", Some(json!({ "name": "survivor" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED);
    let old = first.manager.db_path().to_path_buf();
    let snap = old.with_extension("snap");
    manage::export_snapshot(&first.manager.database(), &snap).unwrap();
    cluster(&first).stop().await;

    // Recover on a fresh disk: the identity and certificates are copied, the
    // database is rebuilt from the snapshot.
    let fresh = tempfile::tempdir().unwrap();
    let db_path = fresh.path().join("glidex.db");
    let src = old.parent().unwrap().join("cluster");
    let dst = fresh.path().join("cluster");
    std::fs::create_dir_all(&dst).unwrap();
    for f in ["identity.json", "node.crt", "node.key", "ca.crt", "ca.key"] {
        std::fs::copy(src.join(f), dst.join(f)).unwrap();
    }
    manage::force_new_cluster(&db_path, Some(&snap), &cfg.cluster).await.unwrap();
    let manager = VmManager::open(db_path, glidex_control_plane::network::Netd::from_env(), &cfg).await.unwrap();
    let router = create_router(manager.clone());
    manager.set_api_router(router.clone());
    let cp = Cp { manager, router, addr: "127.0.0.1:1".parse().unwrap(), _dir: fresh };
    let c = cluster(&cp);
    c.wait_for_leader(Duration::from_secs(20)).await.unwrap();
    assert!(project_names(&cp).await.contains(&"survivor".to_string()));
    let (s, v) = create_project(&cp, "after").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}
