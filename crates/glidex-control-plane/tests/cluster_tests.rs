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
    c.cluster.node_grace_secs = 3;
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

async fn node_json(cp: &Cp, key: &str) -> Value {
    let (s, v) = call(cp, "GET", "/nodes", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v.as_array().unwrap().iter().find(|n| n["meta"]["id"] == key || n["spec"]["name"] == key).cloned().unwrap_or(Value::Null)
}

fn denylist_len(cp: &Cp) -> usize {
    use redb::ReadableTableMetadata;
    let txn = cp.manager.database().begin_read().unwrap();
    txn.open_table(glidex_control_plane::store::TableId::NodeDenylist.definition()).map(|t| t.len().unwrap_or(0) as usize).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn nodes_are_removed_forgotten_and_rejoin_as_themselves() {
    let _one = SERIAL.lock().await;
    let cps = form(4).await;
    let lead = leader_index(&cps, &[0, 1, 2, 3]).await;
    let status = manage::status_of(&cluster(&cps[lead]));
    assert_eq!((status["raft"]["voters"].as_array().unwrap().len(), status["raft"]["learners"].as_array().unwrap().len()), (3, 1), "{status}");
    let lc = cluster(&cps[lead]);
    let learner = (0..4).find(|i| !lc.node.as_ref().unwrap().voters().contains(&cluster(&cps[*i]).identity.raft_id)).unwrap();
    let lname = cluster(&cps[learner]).identity.name.clone();
    let voter = (0..4).find(|i| *i != lead && *i != learner).unwrap();
    let vname = cluster(&cps[voter]).identity.name.clone();

    // A voter can't go if that leaves an even group; nothing changes.
    let (s, e) = call(&cps[lead], "POST", &format!("/nodes/{vname}/remove"), Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert!(e["message"].as_str().unwrap().contains("1, 3 or 5"), "{e}");
    assert_eq!(node_json(&cps[lead], &vname).await["status"]["phase"], "Active");
    // The server issuing the request can't remove itself, and an active node can't be purged.
    let (s, _) = call(&cps[lead], "POST", &format!("/nodes/{}/remove", cluster(&cps[lead]).identity.name), Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(&cps[lead], "POST", &format!("/nodes/{vname}/purge"), None, &[]).await;
    assert_eq!(s, StatusCode::CONFLICT, "an Active node can't be purged");

    // A lost learner: forgetting needs --fenced, and only for a node that has gone silent.
    let (s, _) = call(&cps[lead], "POST", &format!("/nodes/{lname}/forget"), Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "forget without --fenced");
    cluster(&cps[learner]).stop().await;
    wait_until("the node to be reported unreachable", || async { node_json(&cps[lead], &lname).await["status"]["ready"] != "True" }).await;
    let before = denylist_len(&cps[lead]);
    let (s, r) = call(&cps[lead], "POST", &format!("/nodes/{lname}/forget"), Some(json!({ "fenced": true })), &[]).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["phase"], "Forgotten");
    assert_eq!(denylist_len(&cps[lead]), before + 1, "its certificate is revoked");
    wait_until("the forgotten learner to leave the group", || async { !cluster(&cps[lead]).node.as_ref().unwrap().learners().contains(&cluster(&cps[learner]).identity.raft_id) }).await;

    // It comes back as itself, with a new certificate, starting its Raft state afresh (D18).
    let (s, t) = call(&cps[lead], "POST", &format!("/nodes/{lname}/rejoin-token"), Some(json!({ "raft_intact": false })), &[]).await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let cfg = test_config();
    let out = manage::rejoin(
        &cps[learner].manager,
        &cfg,
        manage::RejoinOptions { server: cps[lead].addr, token: t["token"].as_str().unwrap().to_string(), node_id: None, advertise: Some(cps[learner].addr), tunnel_ip: None, listen: Some(cps[learner].addr) },
    )
    .await
    .unwrap();
    assert_eq!(out["raft_fresh"], true, "{out}");
    let n = node_json(&cps[lead], &lname).await;
    assert_eq!(n["status"]["phase"], "Active", "{n}");
    assert_eq!(n["meta"]["id"], cluster(&cps[learner]).identity.node_id, "the same node id");
    wait_until("the rejoined server to be a member again", || async {
        let c = cluster(&cps[lead]);
        let rid = cluster(&cps[learner]).identity.raft_id;
        let l = c.node.as_ref().unwrap();
        l.voters().contains(&rid) || l.learners().contains(&rid)
    })
    .await;

    // An empty node is removed: tombstone, certificate denied, out of the group; twice is fine.
    let before = denylist_len(&cps[lead]);
    let (s, r) = call(&cps[lead], "POST", &format!("/nodes/{lname}/remove"), Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["phase"], "Removed");
    assert!(denylist_len(&cps[lead]) > before);
    wait_until("the removed node to leave the group", || async {
        let c = cluster(&cps[lead]);
        let rid = cluster(&cps[learner]).identity.raft_id;
        let l = c.node.as_ref().unwrap();
        !l.voters().contains(&rid) && !l.learners().contains(&rid)
    })
    .await;
    let (s, r) = call(&cps[lead], "POST", &format!("/nodes/{lname}/remove"), Some(json!({})), &[]).await;
    assert_eq!((s, r["already"].as_bool()), (StatusCode::OK, Some(true)), "removing twice is fine");
    // Its id is retired: no rejoin token.
    let (s, _) = call(&cps[lead], "POST", &format!("/nodes/{lname}/rejoin-token"), Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

async fn wait_until<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !f().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_ca_rotates_and_every_node_renews_before_the_old_one_is_retired() {
    let _one = SERIAL.lock().await;
    let cps = form(3).await;
    let lead = leader_index(&cps, &[0, 1, 2]).await;
    let old_fp = cluster(&cps[lead]).signing_fp();
    // Status shows the feature level and whether the ports answer.
    let (s, st) = call(&cps[lead], "GET", "/cluster/status?ports=true", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{st}");
    assert_eq!(st["ports"].as_array().unwrap().len(), 2, "{st}");
    assert!(st["ports"].as_array().unwrap().iter().all(|p| p["reachable"] == true), "{st}");
    assert!(st["feature_level"].as_u64().unwrap() >= 1);
    let (s, r) = call(&cps[lead], "POST", "/cluster/rotate-ca", Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let new_fp = r["signing_ca"].as_str().unwrap().to_string();
    assert_ne!(new_fp, old_fp);
    // Tokens pin the new CA from now on.
    assert_eq!(cluster(&cps[lead]).signing_fp(), new_fp);
    // Every node ends up trusting only the new CA, with a certificate it signed.
    wait_until("the old CA to be retired everywhere", || async {
        cps.iter().all(|cp| {
            let c = cluster(cp);
            let trust = c.files.read(c.files.trust()).unwrap_or_default();
            let fps: Vec<String> = glidex_control_plane::cluster::pki::pem_bundle_ders(&trust).unwrap().iter().map(|d| glidex_control_plane::cluster::pki::issuer_of(d, &trust).unwrap_or_default()).collect();
            fps == vec![new_fp.clone()] && c.ca_state().is_some_and(|s| s.retiring.is_empty())
        })
    })
    .await;
    // Nothing broke: a write still goes through, from any node, and the CA's old key is gone.
    let (s, _) = create_project(&cps[(lead + 1) % 3], "after-rotation").await;
    assert_eq!(s, StatusCode::CREATED);
    for cp in &cps {
        let c = cluster(cp);
        assert!(!c.files.ca_key().with_extension("key.old").exists());
        let cert = c.files.read(c.files.cert()).unwrap();
        let der = glidex_control_plane::cluster::pki::pem_bundle_ders(&cert).unwrap().remove(0);
        assert_eq!(glidex_control_plane::cluster::pki::issuer_of(&der, &c.files.read(c.files.trust()).unwrap()).as_deref(), Some(new_fp.as_str()));
    }
    // A node can still join, pinned to the new CA.
    let cfg = test_config();
    let extra = new_cp();
    let token = manage::join_token(&cluster(&cps[lead]), NodeRole::Server, 600, false, "test").unwrap();
    assert!(token.ends_with(&new_fp));
    manage::join(&extra.manager, &cfg, JoinOptions { server: cps[lead].addr, token, role: NodeRole::Server, advertise: Some(extra.addr), tunnel_ip: None, name: Some("cp4".into()), listen: Some(extra.addr) }).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_node_detaches_with_its_vms_and_the_cluster_lets_go() {
    use glidex_control_plane::cluster::departure::{finish_pending, pending_db, receipt_file};
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
    let (s, vm) = call(&server, "POST", "/vms", Some(vm_body("travels", "agent1")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm["id"].as_str().unwrap().to_string();
    wait_for("the agent to be ready", || async { node_json(&server, "agent1").await["status"]["ready"] == "True" }).await;

    // Mapping to a network that isn't a node network of it is refused; nothing freezes.
    let (s, e) = call(&server, "POST", "/nodes/agent1/detach", Some(json!({ "map_networks": { "nope": "nada" } })), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert_eq!(node_json(&server, "agent1").await["status"]["phase"], "Active");

    // An abort returns the node to Active.
    let (s, p) = call(&server, "POST", "/nodes/agent1/detach", Some(json!({ "timeout_secs": 600 })), &[]).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["vms"], 1);
    let (s, a) = call(&server, "POST", "/nodes/agent1/detach", Some(json!({ "abort": true })), &[]).await;
    assert_eq!(s, StatusCode::OK, "{a}");
    assert_eq!(node_json(&server, "agent1").await["status"]["phase"], "Active");
    assert!(!pending_db(&agent_state_dir(&agent)).exists() || { wait_until("the pending database to go", || async { !pending_db(&agent_state_dir(&agent)).exists() }).await; true });

    // The real thing: the node writes its bundle, the cluster commits and signs a receipt.
    let (s, p) = call(&server, "POST", "/nodes/agent1/detach", Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let dir = agent_state_dir(&agent);
    wait_until("the receipt", || async { receipt_file(&dir).exists() }).await;
    let n = node_json(&server, "agent1").await;
    assert_eq!(n["status"]["phase"], "Departed", "{n}");
    assert!(n["status"]["departed_ids"].as_array().unwrap().iter().any(|x| *x == id.as_str()), "{n}");
    let (s, _) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the cluster forgot the VM");
    // The departed node's id is retired: no rejoin token.
    let (s, _) = call(&server, "POST", "/nodes/agent1/rejoin-token", Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Restart standalone (on a copy of what the node left on disk): the host owns the VM, as `local`.
    let fresh = tempfile::tempdir().unwrap();
    std::fs::copy(pending_db(&dir), pending_db(fresh.path())).unwrap();
    std::fs::copy(receipt_file(&dir), receipt_file(fresh.path())).unwrap();
    std::fs::create_dir_all(fresh.path().join("cluster")).unwrap();
    std::fs::copy(dir.join("cluster/ca.crt"), fresh.path().join("cluster/ca.crt")).unwrap();
    std::fs::write(fresh.path().join("glidex.db"), b"the old cluster cache").unwrap();
    assert!(finish_pending(&fresh.path().join("glidex.db")).unwrap());
    assert!(!fresh.path().join("cluster").exists(), "identity, certificates and the CA key are gone");
    let standalone = VmManager::with_db_path(fresh.path().join("glidex.db")).unwrap();
    standalone.initialize().await.unwrap();
    let vms = standalone.list_vms().await;
    assert_eq!(vms.len(), 1, "{vms:?}");
    assert_eq!((vms[0].id.as_str(), vms[0].status.placement.as_ref().map(|p| p.node.as_str())), (id.as_str(), Some("local")));
    // A receipt that isn't the cluster's is not honoured.
    let other = tempfile::tempdir().unwrap();
    std::fs::copy(pending_db(&dir), pending_db(other.path())).unwrap();
    let mut r: Value = serde_json::from_slice(&std::fs::read(receipt_file(&dir)).unwrap()).unwrap();
    r["revision"] = json!(1);
    std::fs::write(receipt_file(other.path()), r.to_string()).unwrap();
    std::fs::create_dir_all(other.path().join("cluster")).unwrap();
    std::fs::copy(dir.join("cluster/ca.crt"), other.path().join("cluster/ca.crt")).unwrap();
    assert!(finish_pending(&other.path().join("glidex.db")).is_err());
}

fn agent_state_dir(cp: &Cp) -> std::path::PathBuf {
    cp.manager.db_path().parent().unwrap().to_path_buf()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_node_that_left_on_its_own_is_forgotten_as_departed_and_its_bundle_can_be_built_offline() {
    use glidex_control_plane::cluster::departure::{offline_detach, pending_db};
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
    let (s, vm) = call(&server, "POST", "/vms", Some(vm_body("left-behind", "agent1")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm["id"].as_str().unwrap().to_string();
    wait_for("the VM in the agent's cache", || async { agent.manager.list_vms().await.iter().any(|v| v.id == id) }).await;

    // The cluster can't tell a departed node from a partitioned one while it is still reporting.
    let (s, _) = call(&server, "POST", "/nodes/agent1/detach", Some(json!({ "departed": true })), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // The host builds its bundle from its cache (what the installer's offline path does).
    // Run on a copy of its files: the live manager still has the database open.
    let state = agent_state_dir(&agent);
    let copy = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(copy.path().join("cluster")).unwrap();
    for f in ["identity.json", "ca.crt", "node.key", "node.crt"] {
        std::fs::copy(state.join("cluster").join(f), copy.path().join("cluster").join(f)).unwrap();
    }
    let snap = copy.path().join("snap");
    manage::export_snapshot(&agent.manager.database(), &snap).unwrap();
    let db = glidex_control_plane::store::Db::create(copy.path().join("glidex.db")).unwrap();
    db.install_dump(&mut std::io::BufReader::new(std::fs::File::open(&snap).unwrap()), 0, |_| Ok(())).unwrap();
    drop(db);
    let bundle = offline_detach(&copy.path().join("glidex.db")).unwrap();
    assert_eq!(bundle.vm_ids, vec![id.clone()]);
    assert!(!copy.path().join("cluster").exists() && !pending_db(copy.path()).exists(), "switched and cleaned up");
    let standalone = VmManager::with_db_path(copy.path().join("glidex.db")).unwrap();
    standalone.initialize().await.unwrap();
    assert_eq!(standalone.list_vms().await.len(), 1);

    // Now the agent really is gone: the cluster records that it left, not that it was lost.
    cluster(&agent).stop().await;
    wait_until("the node to be silent", || async { node_json(&server, "agent1").await["status"]["ready"] != "True" }).await;
    let (s, r) = call(&server, "POST", "/nodes/agent1/detach", Some(json!({ "departed": true })), &[]).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!((r["phase"].as_str(), r["vms"].as_u64()), (Some("Departed"), Some(1)));
    let (s, _) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
