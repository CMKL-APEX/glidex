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

fn vm_body(name: &str, node: &str) -> Value {
    json!({ "name": name, "vcpu_count": 1, "mem_size_mib": 256, "hypervisor": "cloudhypervisor",
            "firmware_path": "/path/to/CLOUDHV.fd", "rootfs_path": "/path/to/disk.raw", "node": node })
}

async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !f().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_vm_placed_on_an_agent_is_that_agents_and_its_status_comes_back() {
    let _one = SERIAL.lock().await;
    let cfg = test_config();
    let server = new_cp();
    manage::init(&server.manager, &cfg, InitOptions { advertise: Some(server.addr), tunnel_ip: None, listen: Some(server.addr) }).await.unwrap();
    server.manager.initialize().await.unwrap();

    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test").unwrap();
    manage::join(&agent.manager, &cfg, JoinOptions { server: server.addr, token, role: NodeRole::Agent, advertise: Some(agent.addr), tunnel_ip: None, name: Some("agent1".into()), listen: Some(agent.addr) })
        .await
        .unwrap();
    let link = agent.manager.node_link().expect("an agent has a link").clone();
    assert!(link.wait_ready(Duration::from_secs(20)).await);
    agent.manager.initialize().await.unwrap();
    agent.manager.start_controllers();

    // The agent is a node of the cluster, and answers heartbeats.
    let (s, nodes) = call(&server, "GET", "/nodes", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(nodes.as_array().unwrap().iter().any(|n| n["spec"]["name"] == "agent1" && n["spec"]["role"] == "agent"), "{nodes}");

    // A VM pinned to the agent shows up in its cache and is its own.
    let (s, vm) = call(&server, "POST", "/vms", Some(vm_body("on-agent", "agent1")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm["id"].as_str().unwrap().to_string();
    let agent_node = agent.manager.local_node_id();
    assert_eq!(vm["node"], agent_node.as_str());
    wait_for("the VM in the agent's cache", || async { agent.manager.list_vms().await.iter().any(|v| v.id == id) }).await;
    // ... but not in the server's own work: the server's controller leaves it alone.
    assert!(!server.manager.list_vms().await.is_empty());

    // The agent's controller reconciles it and its status reaches the server.
    wait_for("the controller's status on the server", || async {
        let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
        v["observed_generation"].as_u64().unwrap_or(0) >= 1
    })
    .await;

    // A node may write its VMs' status, and nothing else: the server refuses
    // a spec change from the agent's identity.
    let c = cluster(&agent);
    let mut spec_change = agent.manager.list_vms().await.into_iter().find(|v| v.id == id).unwrap();
    spec_change.spec.power = glidex_control_plane::models::PowerState::Running;
    let ws = {
        let rec = serde_json::to_vec(&spec_change).unwrap();
        glidex_control_plane::store::WriteSet {
            format: 1,
            origin: glidex_control_plane::store::Origin::Controller,
            ops: vec![glidex_control_plane::store::Op::Put { table: glidex_control_plane::store::TableId::Vms, key: id.as_bytes().to_vec(), value: rec }],
        }
    };
    let r = c.client.request(&server.addr.to_string(), hyper::Method::POST, "/cluster/v1/status-write", &[("content-type", "application/json".into())], serde_json::to_vec(&ws).unwrap().into()).await.unwrap();
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", String::from_utf8_lossy(&r.body));

    // Partitioned from every server, the agent keeps working from its cache
    // and queues what it writes; when the servers are back, the queue drains.
    link.set_partitioned(true);
    let before = {
        let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
        v["resource_version"].as_u64().unwrap()
    };
    agent.manager.__touch_status_for_tests(&id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
    assert_eq!(v["resource_version"].as_u64().unwrap(), before, "the server saw a write the agent could not send");
    let local = agent.manager.list_vms().await.into_iter().find(|v| v.id == id).unwrap();
    assert_eq!(local.status.launch_failures, 1, "the agent's cache didn't take its own write");
    link.set_partitioned(false);
    wait_for("the queued write to reach the server", || async {
        let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
        v["resource_version"].as_u64().unwrap() > before
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_silent_node_is_unreachable_not_gone() {
    let _one = SERIAL.lock().await;
    let mut cfg = test_config();
    cfg.cluster.node_grace_secs = 5;
    let server = new_cp();
    manage::init(&server.manager, &cfg, InitOptions { advertise: Some(server.addr), tunnel_ip: None, listen: Some(server.addr) }).await.unwrap();
    server.manager.initialize().await.unwrap();
    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test").unwrap();
    manage::join(&agent.manager, &cfg, JoinOptions { server: server.addr, token, role: NodeRole::Agent, advertise: Some(agent.addr), tunnel_ip: None, name: Some("agent1".into()), listen: Some(agent.addr) })
        .await
        .unwrap();
    let link = agent.manager.node_link().unwrap().clone();
    assert!(link.wait_ready(Duration::from_secs(20)).await);
    agent.manager.initialize().await.unwrap();
    let (s, vm) = call(&server, "POST", "/vms", Some(vm_body("quiet", "agent1")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm["id"].as_str().unwrap().to_string();

    let ready = |nodes: &Value| nodes.as_array().unwrap().iter().find(|n| n["spec"]["name"] == "agent1").map(|n| n["status"]["ready"].as_str().unwrap().to_string());
    wait_for("the agent to be Ready", || async { ready(&call(&server, "GET", "/nodes", None, &[]).await.1).as_deref() == Some("True") }).await;

    link.set_partitioned(true);
    wait_for("the agent to be Unknown", || async { ready(&call(&server, "GET", "/nodes", None, &[]).await.1).as_deref() == Some("Unknown") }).await;
    // Its VMs say so, and nothing was stopped or moved.
    let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
    assert!(v["conditions"].as_array().unwrap().iter().any(|c| c["kind"] == "Ready" && c["reason"] == "NodeUnreachable"), "{v}");
    assert_eq!(v["node"], agent.manager.local_node_id().as_str());

    link.set_partitioned(false);
    wait_for("the agent to be Ready again", || async { ready(&call(&server, "GET", "/nodes", None, &[]).await.1).as_deref() == Some("True") }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_server_relays_the_console_log_of_a_vm_on_an_agent() {
    let _one = SERIAL.lock().await;
    let cfg = test_config();
    let server = new_cp();
    manage::init(&server.manager, &cfg, InitOptions { advertise: Some(server.addr), tunnel_ip: None, listen: Some(server.addr) }).await.unwrap();
    server.manager.initialize().await.unwrap();
    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test").unwrap();
    manage::join(&agent.manager, &cfg, JoinOptions { server: server.addr, token, role: NodeRole::Agent, advertise: Some(agent.addr), tunnel_ip: None, name: Some("agent1".into()), listen: Some(agent.addr) })
        .await
        .unwrap();
    assert!(agent.manager.node_link().unwrap().wait_ready(Duration::from_secs(20)).await);
    agent.manager.initialize().await.unwrap();
    let (s, vm) = call(&server, "POST", "/vms", Some(vm_body("chatty", "agent1")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm["id"].as_str().unwrap().to_string();

    // The log lives on the agent's disk (here, the same machine).
    let paths = glidex_control_plane::paths::vm_paths(&id);
    std::fs::create_dir_all(&paths.dir).unwrap();
    std::fs::write(&paths.log, b"booting...\nlogin: ").unwrap();
    let resp = server.router.clone().oneshot(Request::builder().uri(format!("/vms/{id}/console/log")).body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"booting...\nlogin: ");

    // The live console is a raw stream through the node API.
    let listener = tokio::net::UnixListener::bind(&paths.console_socket).unwrap();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let (mut r, mut w) = s.split();
        let _ = tokio::io::copy(&mut r, &mut w).await;
    });
    let mut stream = cluster(&server).client.upgrade(&agent.addr.to_string(), &format!("/cluster/v1/vms/{id}/console")).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream.write_all(b"ls\n").await.unwrap();
    let mut buf = [0u8; 3];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf, b"ls\n");
    // Another VM's console isn't served by this node.
    assert!(cluster(&server).client.upgrade(&agent.addr.to_string(), "/cluster/v1/vms/nope/console").await.is_err());
    std::fs::remove_dir_all(&paths.dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_node_copies_an_image_it_needs_from_a_holder_and_checks_it() {
    use glidex_control_plane::store::{Origin, TableId};
    use sha2::{Digest, Sha256};
    let _one = SERIAL.lock().await;
    let mut cfg = test_config();
    cfg.reconcile.resync_secs = 5;
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(&server.manager, &cfg, InitOptions { advertise: Some(server.addr), tunnel_ip: None, listen: Some(server.addr) }).await.unwrap();
    server.manager.initialize().await.unwrap();
    server.manager.start_controllers();
    let agent = new_cp();
    agent.manager.configure(&cfg);
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test").unwrap();
    manage::join(&agent.manager, &cfg, JoinOptions { server: server.addr, token, role: NodeRole::Agent, advertise: Some(agent.addr), tunnel_ip: None, name: Some("agent1".into()), listen: Some(agent.addr) })
        .await
        .unwrap();
    assert!(agent.manager.node_link().unwrap().wait_ready(Duration::from_secs(20)).await);
    agent.manager.initialize().await.unwrap();
    agent.manager.start_controllers();

    // An image the server holds: record and file.
    let data = vec![7u8; 100_000];
    let sha = glidex_control_plane::cluster::pki::hex(&Sha256::digest(&data));
    let img_id = "img-1";
    let img = json!({
        "id": img_id, "name": "base", "kind": "disk", "source": { "kind": "url", "url": "http://example.invalid/x", "expected_sha256": sha },
        "status": { "state": "ready" }, "format": "qcow2", "virtual_size_bytes": 1048576, "file_size_bytes": data.len(), "sha256": sha,
        "arch": "x86_64", "created_at": 1,
    });
    let server_path = server.manager.images().image_path(img_id);
    std::fs::create_dir_all(server_path.parent().unwrap()).unwrap();
    std::fs::write(&server_path, &data).unwrap();
    let agent_id = agent.manager.local_node_id();
    let disk = json!({ "id": "d1", "name": "d1", "project": "", "format": "qcow2", "size_bytes": 0,
        "origin": { "kind": "image", "image_id": img_id, "mode": "linked" }, "created_at": 1, "phase": "pending", "node": agent_id });
    server
        .manager
        .database()
        .write(Origin::Api, |tx| -> Result<(), glidex_control_plane::store::StoreError> {
            tx.open_table(TableId::Images.definition())?.insert(img_id, img.to_string().as_bytes())?;
            tx.open_table(TableId::Disks.definition())?.insert("d1", disk.to_string().as_bytes())?;
            Ok(())
        })
        .unwrap();

    // The agent fetches it, verifies it, and says it has it.
    let agent_path = agent.manager.images().image_path(img_id);
    wait_for("the agent's copy", || async { agent_path.exists() }).await;
    assert_eq!(std::fs::read(&agent_path).unwrap(), data);
    wait_for("both copies recorded", || async {
        let c = server.manager.image_caches(img_id);
        c.len() == 2 && c.iter().all(|(_, r)| r.phase == glidex_control_plane::controller::image_cache::CachePhase::Ready)
    })
    .await;
}

fn vm_unpinned(name: &str) -> Value {
    json!({ "name": name, "vcpu_count": 1, "mem_size_mib": 64, "hypervisor": "cloudhypervisor",
            "firmware_path": "/path/to/CLOUDHV.fd", "rootfs_path": "/path/to/disk.raw" })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_scheduler_spreads_vms_honours_pins_and_disks_and_explains_itself() {
    let _one = SERIAL.lock().await;
    let cfg = test_config();
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(&server.manager, &cfg, InitOptions { advertise: Some(server.addr), tunnel_ip: None, listen: Some(server.addr) }).await.unwrap();
    server.manager.initialize().await.unwrap();
    server.manager.start_controllers();
    let mut agents = Vec::new();
    for i in 1..=2 {
        let a = new_cp();
        let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test").unwrap();
        manage::join(&a.manager, &cfg, JoinOptions { server: server.addr, token, role: NodeRole::Agent, advertise: Some(a.addr), tunnel_ip: None, name: Some(format!("agent{i}")), listen: Some(a.addr) })
            .await
            .unwrap();
        assert!(a.manager.node_link().unwrap().wait_ready(Duration::from_secs(20)).await);
        a.manager.initialize().await.unwrap();
        agents.push(a);
    }
    let ids: Vec<String> = {
        let (_, n) = call(&server, "GET", "/nodes", None, &[]).await;
        n.as_array().unwrap().iter().map(|n| n["meta"]["id"].as_str().unwrap().to_string()).collect()
    };
    assert_eq!(ids.len(), 3);

    // Six VMs spread two to a node (same capacity: fewest VMs, then name).
    let mut placed = std::collections::HashMap::<String, u32>::new();
    for i in 0..6 {
        let (s, vm) = call(&server, "POST", "/vms", Some(vm_unpinned(&format!("vm{i}"))), &[]).await;
        assert_eq!(s, StatusCode::CREATED, "{vm}");
        *placed.entry(vm["node"].as_str().expect("placed").to_string()).or_default() += 1;
    }
    assert_eq!(placed.len(), 3, "{placed:?}");
    assert!(placed.values().all(|n| *n == 2), "{placed:?}");

    // A disk bound to agent2 pulls its VM there.
    let a2 = agents[1].manager.local_node_id();
    let (_, projects) = call(&server, "GET", "/projects", None, &[]).await;
    let project = projects.as_array().unwrap()[0]["id"].as_str().unwrap().to_string();
    let disk = json!({ "id": "dd", "name": "dd", "project": project, "format": "qcow2", "size_bytes": 1048576, "origin": { "kind": "blank" }, "created_at": 1, "phase": "ready", "node": a2 });
    server
        .manager
        .database()
        .write(glidex_control_plane::store::Origin::Api, |tx| -> Result<(), glidex_control_plane::store::StoreError> {
            tx.open_table(glidex_control_plane::store::TableId::Disks.definition())?.insert("dd", disk.to_string().as_bytes())?;
            Ok(())
        })
        .unwrap();
    let mut body = vm_unpinned("with-disk");
    body["data_disks"] = json!(["dd"]);
    let (s, vm) = call(&server, "POST", "/vms", Some(body), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    assert_eq!(vm["node"], a2.as_str());

    // Pin to a node and it goes there; a pin on a drained node waits;
    // with every node drained a VM waits and says why, then is placed.
    let (s, vm) = call(&server, "POST", "/vms", Some(vm_body("pinned", "agent1")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    assert_eq!(vm["node"], agents[0].manager.local_node_id().as_str());
    for id in &ids {
        let (s, v) = call(&server, "POST", &format!("/nodes/{id}/drain"), None, &[]).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert!(v["remaining"]["vms"].is_array());
    }
    let (s, vm) = call(&server, "POST", "/vms", Some(vm_body("refused", "agent1")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    assert!(vm["node"].is_null(), "a pin on a drained node waits: {vm}");
    let (s, _) = call(&server, "DELETE", &format!("/vms/{}", vm["id"].as_str().unwrap()), None, &[]).await;
    assert!(s.is_success());
    let (s, vm) = call(&server, "POST", "/vms", Some(vm_unpinned("waits")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    assert!(vm["node"].is_null(), "{vm}");
    let waiting = vm["conditions"].as_array().unwrap().iter().find(|c| c["kind"] == "Scheduled").cloned().unwrap();
    assert_eq!(waiting["reason"], "Unschedulable");
    assert!(waiting["message"].as_str().unwrap().contains("draining"), "{waiting}");
    let id = vm["id"].as_str().unwrap().to_string();
    let (s, _) = call(&server, "POST", &format!("/nodes/{}/undrain", ids[0]), None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    wait_for("the waiting VM to be placed", || async {
        let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
        v["node"].as_str() == Some(ids[0].as_str())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn cluster_networks_get_a_subnet_and_vms_get_addresses_that_go_when_they_do() {
    use glidex_control_plane::cluster::config::EdgeConfig;
    let _one = SERIAL.lock().await;
    let mut cfg = test_config();
    cfg.cluster.ovn.enabled = true;
    cfg.cluster.ovn.edge = Some(EdgeConfig {
        physnet: "uplink".into(),
        external_cidr: "192.0.2.0/24".into(),
        gateway: "192.0.2.1".parse().unwrap(),
        external_ip: "192.0.2.50".parse().unwrap(),
        external_pool: None,
        gateway_nodes: vec!["cp1".into()],
    });
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(&server.manager, &cfg, InitOptions { advertise: Some(server.addr), tunnel_ip: None, listen: Some(server.addr) }).await.unwrap();
    server.manager.initialize().await.unwrap();

    // A cluster network: br-int, the underlay MTU minus Geneve, a /24 from the supernet.
    let (s, n) = call(&server, "POST", "/networks", Some(json!({ "name": "web", "mode": "nat", "port_type": "tap", "scope": "cluster" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{n}");
    assert_eq!((n["scope"].as_str(), n["bridge"].as_str(), n["mtu"].as_u64()), (Some("cluster"), Some("br-int"), Some(1442)));
    let (s, n2) = call(&server, "POST", "/networks", Some(json!({ "name": "lab", "mode": "isolated", "port_type": "tap", "scope": "cluster", "subnet": "10.89.200.0/24" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{n2}");
    // Overlap and the node NAT range are refused.
    let (s, _) = call(&server, "POST", "/networks", Some(json!({ "name": "bad", "mode": "isolated", "port_type": "tap", "scope": "cluster", "subnet": "10.89.200.128/25" })), &[]).await;
    assert_ne!(s, StatusCode::CREATED);
    let (s, _) = call(&server, "POST", "/networks", Some(json!({ "name": "bad2", "mode": "isolated", "port_type": "tap", "scope": "cluster", "subnet": "10.88.9.0/24" })), &[]).await;
    assert_ne!(s, StatusCode::CREATED);

    // VMs on it get stable addresses at placement.
    let mut body = vm_unpinned("a");
    body["networks"] = json!([{ "network": "web" }]);
    let (s, a) = call(&server, "POST", "/vms", Some(body.clone()), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{a}");
    body["name"] = json!("b");
    let (s, b) = call(&server, "POST", "/vms", Some(body), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let res = glidex_control_plane::ipam::list_reservations(&server.manager.database()).unwrap();
    assert_eq!(res.len(), 2);
    let ips: std::collections::BTreeSet<_> = res.iter().map(|r| r.ip.to_string()).collect();
    assert_eq!(ips.len(), 2);
    assert!(ips.iter().all(|ip| ip.starts_with("10.89.0.")), "{ips:?}");

    // The plan for OVN has the switch, its subnet, both ports bound to the VM's node, and the edge.
    let d = server.manager.__desired_ovn_for_tests().await.unwrap();
    assert_eq!(d.networks.len(), 2);
    assert_eq!(d.ports.len(), 2);
    assert!(d.ports.iter().all(|p| p.chassis.as_deref() == Some(server.manager.local_node_id().as_str()) && p.ip.is_some()));
    assert!(d.edge.is_some() && !d.node_addresses.is_empty());

    // A network in use can't go; a VM that goes frees its address.
    let (s, _) = call(&server, "DELETE", "/networks/web", None, &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(&server, "DELETE", &format!("/vms/{}", a["id"].as_str().unwrap()), None, &[]).await;
    assert!(s.is_success());
    server.manager.clone().reconcile_ovn().await;
    assert_eq!(glidex_control_plane::ipam::list_reservations(&server.manager.database()).unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn provider_networks_and_vpc_routers() {
    use glidex_control_plane::cluster::config::EdgeConfig;
    let _one = SERIAL.lock().await;
    let mut cfg = test_config();
    cfg.cluster.ovn.enabled = true;
    cfg.cluster.ovn.edge = Some(EdgeConfig {
        physnet: "uplink".into(),
        external_cidr: "192.0.2.0/29".into(),
        gateway: "192.0.2.1".parse().unwrap(),
        external_ip: "192.0.2.2".parse().unwrap(),
        external_pool: Some("192.0.2.0/29".into()),
        gateway_nodes: vec!["cp1".into()],
    });
    cfg.cluster.ovn.bridge_mappings.insert("lan".into(), "br-lan".into());
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(&server.manager, &cfg, InitOptions { advertise: Some(server.addr), tunnel_ip: None, listen: Some(server.addr) }).await.unwrap();
    server.manager.initialize().await.unwrap();

    // Provider networks: a physnet is required, a subnet isn't allowed, vlan is checked.
    let mk = |name: &str, extra: serde_json::Value| {
        let mut b = json!({ "name": name, "mode": "bridged", "port_type": "tap", "scope": "cluster" });
        b.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        b
    };
    let (s, _) = call(&server, "POST", "/networks", Some(mk("p0", json!({}))), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&server, "POST", "/networks", Some(mk("p0", json!({ "physnet": "lan", "subnet": "10.1.0.0/24" }))), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&server, "POST", "/networks", Some(mk("p0", json!({ "physnet": "lan", "vlan": 5000 }))), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, n) = call(&server, "POST", "/networks", Some(mk("p1", json!({ "physnet": "lan", "vlan": 30 }))), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{n}");
    let d = server.manager.__desired_ovn_for_tests().await.unwrap();
    assert!(d.networks.iter().any(|n| n.name == "p1" && matches!(&n.kind, glidex_ovn::NetKind::Provider { physnet, vlan: Some(30) } if physnet == "lan")));
    assert!(glidex_control_plane::ipam::list_subnets(&server.manager.database()).unwrap().is_empty(), "a provider network takes no subnet");

    // VPC routers: one per project by default, an external address each.
    let (s, r) = call(&server, "POST", "/projects/default/routers", Some(json!({ "name": "r1" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    assert_eq!(r["status"]["external_ip"], "192.0.2.3");
    assert_eq!(r["status"]["snat_ct_zone"], 60001);
    // The site's pool (.3-.6 here, the gateway and edge take .1 and .2) runs out; the admin is
    // allowed past quota, so the pool is what stops it.
    for n in ["r2", "r3", "r4"] {
        let (s, r) = call(&server, "POST", "/projects/default/routers", Some(json!({ "name": n })), &[]).await;
        assert_eq!(s, StatusCode::CREATED, "{r}");
    }
    let (s, e) = call(&server, "POST", "/projects/default/routers", Some(json!({ "name": "r5" })), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert_eq!(e["error"], "external_pool_exhausted", "{e}");
    let (s, _) = call(&server, "POST", "/projects/default/routers", Some(json!({ "name": "r1", "external": false })), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // A NAT network on the router: by name, and the plan shows the router and its zone.
    let (s, n) = call(&server, "POST", "/projects/default/networks", Some(json!({ "name": "app", "mode": "nat", "port_type": "tap", "scope": "cluster", "router": "r1" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{n}");
    let (s, n) = call(&server, "POST", "/projects/default/networks", Some(json!({ "name": "bad", "mode": "nat", "port_type": "tap", "scope": "cluster", "router": "nope" })), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{n}");
    let d = server.manager.__desired_ovn_for_tests().await.unwrap();
    assert_eq!(d.routers.len(), 4);
    let r1 = d.routers.iter().find(|r| r.external.as_ref().is_some_and(|e| e.external_ip.to_string() == "192.0.2.3")).unwrap();
    let ext = r1.external.as_ref().unwrap();
    assert_eq!(ext.snat_ct_zone, Some(60001));
    assert_eq!(d.edge.as_ref().unwrap().snat_ct_zone, Some(60000));
    assert!(d.networks.iter().any(|n| n.name == "app" && n.router.as_deref() == Some(r1.name.as_str())));

    // A router with a network on it can't go, not even while that network is only on its way out.
    let (s, _) = call(&server, "DELETE", "/projects/default/routers/r1", None, &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(&server, "DELETE", "/networks/app", None, &[]).await;
    assert!(s.is_success(), "{s}");
    let (s, e) = call(&server, "DELETE", "/projects/default/routers/r1", None, &[]).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    // One with nothing on it goes, and its address is free again.
    let (s, _) = call(&server, "DELETE", "/projects/default/routers/r2", None, &[]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, r) = call(&server, "POST", "/projects/default/routers", Some(json!({ "name": "r6" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    assert_eq!(r["status"]["external_ip"], "192.0.2.4", "the address came back");
    assert_eq!(r["status"]["snat_ct_zone"], 60002, "and so did the zone");
}
