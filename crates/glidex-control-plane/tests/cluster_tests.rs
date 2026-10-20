//! Three control planes on one machine, each with its own directory and
//! ports, joined over real mutual TLS (spec/clustering.md §17 C2): writes
//! through any of them reach the leader, reads see them, and killing the
//! leader doesn't stop the cluster.
//!
//! The process-wide identity in `authz` is shared by the three, which is
//! harmless here because authentication is off.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use glidex_control_plane::api::create_router;
use glidex_control_plane::cluster::Cluster;
use glidex_control_plane::cluster::manage::{self, InitOptions, JoinOptions};
use glidex_control_plane::config::Config;
use glidex_control_plane::node::NodeRole;
use glidex_control_plane::state::VmManager;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;

struct Cp {
    manager: Arc<VmManager>,
    router: axum::Router,
    addr: SocketAddr,
    /// Holds this control plane's secrets key (spec/tenancy.md §13), for
    /// the tests that need one (`with_secrets`).
    secrets: Option<Arc<glidex_control_plane::auth::AuthService>>,
    _dir: tempfile::TempDir,
}

/// `cp` with an auth service on its store whose secrets key sits beside
/// it, given to the manager the way the control plane does.
fn with_secrets(mut cp: Cp) -> Cp {
    let mut cfg = test_config();
    cfg.auth.secrets_key = Some(cp._dir.path().join("secrets.kek"));
    let auth =
        glidex_control_plane::auth::AuthService::disabled_with(cp.manager.database(), cfg).unwrap();
    cp.manager.set_secrets_key(auth.clone());
    cp.secrets = Some(auth);
    cp
}

/// Stop the controllers while the test's runtime still runs. One caught in
/// a status write to the leader (`NodeLink::submit` blocks on the runtime)
/// would otherwise keep the runtime's shutdown waiting for ever.
impl Drop for Cp {
    fn drop(&mut self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let m = self.manager.clone();
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                let _ = tokio::time::timeout(Duration::from_secs(20), m.stop_controllers()).await;
            })
        });
    }
}

/// The identity in `authz` is process-wide, so these tests run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// How long a test may take once it has its turn, its runtime's shutdown
/// included: a few times the slowest, and longer than any one wait in it,
/// so a failing test still fails with its own message.
const TEST_DEADLINE: Duration = Duration::from_secs(90);

/// Take this test's turn (`SERIAL`) and give it `TEST_DEADLINE`: a test
/// still running then aborts the run, rather than hang it until CI gives up.
async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    let turn = SERIAL.lock().await;
    // The test's thread holds the sender; it goes when the thread ends,
    // after the runtime has shut down.
    thread_local! {
        static RUNNING: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> =
            const { std::cell::RefCell::new(None) };
    }
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    RUNNING.with(|r| *r.borrow_mut() = Some(tx));
    let name = std::thread::current()
        .name()
        .unwrap_or("a test")
        .to_string();
    std::thread::spawn(move || {
        if rx.recv_timeout(TEST_DEADLINE) == Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            // Not eprintln!: the test harness captures this thread's output.
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "{name} has not finished {TEST_DEADLINE:?} after it started; aborting"
            );
            std::process::abort();
        }
    });
    turn
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn test_config() -> Config {
    let mut c = Config::default();
    c.cluster.raft.heartbeat_ms = 100;
    c.cluster.raft.election_ms = (600, 1200);
    // Longer than an agent's 5 s heartbeat plus the leader's 2 s liveness
    // tick: shorter, and a live node flaps between ready and unreachable.
    c.cluster.node_grace_secs = 8;
    c
}

/// Nodes report the hypervisors they find on `PATH`, and the scheduler only
/// places a VM where its hypervisor is. These tests never launch one, so on
/// a machine without them (a CI runner) stand-ins on `PATH` are enough; a
/// machine that has them keeps its own.
fn hypervisors_on_path() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        use std::os::unix::fs::PermissionsExt;
        let missing: Vec<&str> = ["cloud-hypervisor", "qemu-system-x86_64"]
            .into_iter()
            .filter(|b| glidex_control_plane::hypervisor::resolve_binary(b).is_none())
            .collect();
        if missing.is_empty() {
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("glidex-cluster-tests-bin-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for b in missing {
            let p = dir.join(b);
            std::fs::write(
                &p,
                "#!/bin/sh\necho 'a stand-in for the cluster tests' >&2\nexit 1\n",
            )
            .unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        let joined =
            std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&path))).unwrap();
        // SAFETY: test-only; every test sets it the same way before reading it.
        unsafe { std::env::set_var("PATH", joined) };
    });
}

fn new_cp() -> Cp {
    hypervisors_on_path();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let manager = common::with_db_path(dir.path().join("glidex.db")).unwrap();
    let router = create_router(manager.clone());
    manager.set_api_router(router.clone());
    Cp {
        manager,
        router,
        addr: format!("127.0.0.1:{}", free_port()).parse().unwrap(),
        secrets: None,
        _dir: dir,
    }
}

async fn call(
    cp: &Cp,
    method: &str,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
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
    let resp = cp
        .router
        .clone()
        .oneshot(b.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A proto3 JSON 64-bit integer: a string (`"100000"`) or a number; left
/// out (null) at its default, 0.
fn num(v: &Value) -> u64 {
    match v {
        Value::Null => 0,
        Value::String(s) => s.parse().unwrap_or_else(|_| panic!("not a number: {v}")),
        _ => v.as_u64().unwrap_or_else(|| panic!("not a number: {v}")),
    }
}

/// The copies of the image in a `GET /images/{id}` answer (`{image}`);
/// proto3 JSON leaves an empty list out.
fn image_copies(v: &Value) -> Vec<Value> {
    assert!(v["image"].is_object(), "not an image: {v}");
    v["image"]["copies"].as_array().cloned().unwrap_or_default()
}

/// The VM in a `{vm}` answer (`POST /vms`, `GET /vms/{id}`, ...), in the
/// proto3 JSON shape (spec/grpc.md §9.1).
fn vm_of(v: &Value) -> &Value {
    assert!(v["vm"].is_object(), "not a VM answer: {v}");
    &v["vm"]
}

/// A node's `status.ready` when it is Ready.
const READY: &str = "CONDITION_STATUS_TRUE";

/// The nodes of a `GET /nodes` answer (`{nodes}`); proto3 JSON leaves an
/// empty list out.
fn nodes_of(v: &Value) -> Vec<Value> {
    assert!(v.is_object(), "not a node list: {v}");
    v["nodes"].as_array().cloned().unwrap_or_default()
}

/// An RFC 3339 time in UTC, as proto3 JSON writes a Timestamp
/// (`2026-10-17T08:00:00Z`, maybe with a fraction).
fn is_time(v: &Value) -> bool {
    let Some(t) = v.as_str() else { return false };
    let b = t.as_bytes();
    t.len() >= 20
        && t.ends_with('Z')
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':'
        && t[..4].parse::<u32>().is_ok_and(|y| y >= 1970)
}

fn cluster(cp: &Cp) -> Arc<Cluster> {
    cp.manager.cluster().expect("clustered")
}

async fn form(n: usize) -> Vec<Cp> {
    form_with(n, new_cp).await
}

/// `form`, each control plane made by `make`.
async fn form_with(n: usize, make: impl Fn() -> Cp) -> Vec<Cp> {
    let cfg = test_config();
    let first = make();
    manage::init(
        &first.manager,
        &cfg,
        InitOptions {
            advertise: Some(first.addr),
            tunnel_ip: None,
            listen: Some(first.addr),
        },
    )
    .await
    .unwrap();
    first.manager.initialize().await.unwrap();
    let mut cps = vec![first];
    for _ in 1..n {
        let cp = make();
        let token = manage::join_token(&cluster(&cps[0]), NodeRole::Server, 600, false, "test")
            .await
            .unwrap();
        manage::join(
            &cp.manager,
            &cfg,
            JoinOptions {
                server: cps[0].addr,
                token,
                role: NodeRole::Server,
                advertise: Some(cp.addr),
                tunnel_ip: None,
                name: Some(format!("cp{}", cps.len() + 1)),
                listen: Some(cp.addr),
            },
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
        assert!(
            tokio::time::Instant::now() < deadline,
            "no leader among {among:?}"
        );
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
    v["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn three_control_planes_survive_losing_the_leader() {
    let _one = serial().await;
    let cps = form(3).await;

    for (i, cp) in cps.iter().enumerate() {
        let m = cluster(cp)
            .node
            .as_ref()
            .unwrap()
            .raft
            .metrics()
            .borrow()
            .clone();
        eprintln!(
            "cp{} snapshot={:?} purged={:?} applied={:?} last_log={:?}",
            i + 1,
            m.snapshot,
            m.purged,
            m.last_applied,
            m.last_log_index
        );
    }
    // The group has three voters and one leader everyone agrees on.
    let status = manage::status_of(&cluster(&cps[0]));
    assert_eq!(
        status["raft"]["voters"].as_array().unwrap().len(),
        3,
        "{status}"
    );
    let leader = leader_index(&cps, &[0, 1, 2]).await;
    let follower = (0..3).find(|i| *i != leader).unwrap();
    assert!(!cluster(&cps[follower]).is_leader());

    // A write through a follower is forwarded; a read through the other
    // follower sees it (linearizable).
    let (s, v) = call(
        &cps[follower],
        "POST",
        "/projects",
        Some(json!({ "name": "alpha" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let other = (0..3).find(|i| *i != leader && *i != follower).unwrap();
    assert!(
        project_names(&cps[other])
            .await
            .contains(&"alpha".to_string())
    );
    assert!(
        project_names(&cps[leader])
            .await
            .contains(&"alpha".to_string())
    );

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
        assert!(
            names.contains(&"alpha".to_string()) && names.contains(&"beta".to_string()),
            "{i}: {names:?}"
        );
    }
    for (i, cp) in cps.iter().enumerate() {
        let d = cp.manager.database().dump().unwrap();
        let dp = d
            .iter()
            .flat_map(|(_, r)| r.iter())
            .find(|(k, _)| k == "default_project")
            .map(|(_, v)| String::from_utf8_lossy(v).into_owned());
        eprintln!(
            "cp{} default_project={:?} leader={} applied={}",
            i + 1,
            dp,
            i == leader,
            cp.manager.database().revision()
        );
    }
    let (a, b) = (
        cps[rest[0]].manager.database().dump().unwrap(),
        cps[rest[1]].manager.database().dump().unwrap(),
    );
    if a != b {
        for ((ta, ra), (tb, rb)) in a.iter().zip(b.iter()) {
            for (k, v) in ra {
                if rb.iter().find(|(k2, _)| k2 == k).map(|(_, v2)| v2) != Some(v) {
                    eprintln!(
                        "DIFF in {ta:?} {tb:?}: {k} {:?}",
                        String::from_utf8_lossy(v)
                    );
                }
            }
        }
        panic!("the replicas differ");
    }
}

/// Servers heartbeat the leader like any node (spec/clustering.md §7.2):
/// past the grace, the followers are still Ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn servers_stay_ready_past_the_grace() {
    let _one = serial().await;
    let cps = form(3).await;
    let lead = leader_index(&cps, &[0, 1, 2]).await;
    // The grace and two liveness rounds.
    let grace = test_config().cluster.node_grace_secs;
    tokio::time::sleep(Duration::from_secs(grace + 4)).await;
    let (s, v) = call(&cps[lead], "GET", "/nodes", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let nodes = nodes_of(&v);
    assert_eq!(nodes.len(), 3, "{v}");
    assert!(nodes.iter().all(|n| n["status"]["ready"] == READY), "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn rpc_reads_are_answered_by_the_server_that_takes_them() {
    let _one = serial().await;
    let cps = form(2).await;
    let leader = leader_index(&cps, &[0, 1]).await;
    let follower = 1 - leader;
    // Every RPC is a POST: the operation's kind (READ), not the method,
    // keeps it on the follower (spec/grpc.md G-D6). Forwarded, it would
    // report the leader's node id.
    let (s, v) = call(
        &cps[follower],
        "POST",
        "/glidex.system.v1.SystemService/GetServerInfo",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["node_id"],
        cluster(&cps[follower]).identity.node_id.as_str(),
        "{v}"
    );
    assert_eq!(
        v["cluster_id"],
        cluster(&cps[leader]).identity.cluster_id.as_str(),
        "{v}"
    );
    // Health is always this server's own.
    let (s, _) = call(
        &cps[follower],
        "POST",
        "/grpc.health.v1.Health/Check",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

/// Every server ends up with the cluster's secrets key, so any of them can
/// open the connector secrets in the replicated store (spec/tenancy.md §13).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_new_server_receives_the_secrets_key() {
    let _one = serial().await;
    let cps = form_with(2, || with_secrets(new_cp())).await;
    let secrets = |i: usize| cps[i].secrets.as_ref().unwrap();
    let first = secrets(0).connectors.kek_id();
    wait_for(
        "the second server holds the first one's secrets key",
        || {
            let ok = secrets(1).connectors.kek_id() == first;
            async move { ok }
        },
    )
    .await;
    // The key file on disk is the cluster's too.
    let disk = std::fs::read(cps[1]._dir.path().join("secrets.kek")).unwrap();
    assert_eq!(disk, *secrets(0).connectors.kek_bytes());
}

/// A gRPC write sent to a follower is forwarded to the leader, and the
/// leader's status comes back in the HTTP/2 trailers (spec/grpc.md §8).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn rpc_writes_on_a_follower_reach_the_leader_with_their_trailers() {
    use buffa::Message;
    use glidex_api::proto::glidex::tenancy::v1::{CreateTenantRequest, CreateTenantResponse};
    let _one = serial().await;
    let cps = form(2).await;
    let leader = leader_index(&cps, &[0, 1]).await;
    let follower = 1 - leader;
    let grpc = |cp: &Cp, slug: &str| {
        let msg = CreateTenantRequest {
            slug: slug.into(),
            ..Default::default()
        }
        .encode_to_vec();
        let mut body = vec![0u8];
        body.extend_from_slice(&(msg.len() as u32).to_be_bytes());
        body.extend_from_slice(&msg);
        let req = Request::post("/glidex.tenancy.v1.TenantService/CreateTenant")
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(Body::from(body))
            .unwrap();
        cp.router.clone().oneshot(req)
    };
    // A write right after an election can answer 503 while the new leader
    // settles (as create_project knows); retry those. A 503 after the
    // write was applied makes the retry a duplicate (ABORTED, 10).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut retried = false;
    let (headers, collected) = loop {
        let resp = grpc(&cps[follower], "acme").await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let collected = resp.into_body().collect().await.unwrap();
        if status == StatusCode::SERVICE_UNAVAILABLE && tokio::time::Instant::now() < deadline {
            retried = true;
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&collected.to_bytes())
        );
        break (headers, collected);
    };
    let trailers = collected
        .trailers()
        .cloned()
        .expect("gRPC trailers came back through the forward");
    let grpc_status = trailers
        .get("grpc-status")
        .map(|v| v.to_str().unwrap().to_string());
    let id = if grpc_status.as_deref() == Some("10") && retried {
        let (s, v) = call(&cps[leader], "GET", "/tenants/acme", None, &[]).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["tenant"]["id"].as_str().unwrap().to_string()
    } else {
        assert_eq!(
            grpc_status.as_deref(),
            Some("0"),
            "headers {headers:?} trailers {trailers:?}"
        );
        let b = collected.to_bytes();
        let created = CreateTenantResponse::decode_from_slice(&b[5..]).unwrap();
        created.tenant.as_option().unwrap().id.clone()
    };
    // Written once, through Raft: both servers have it.
    for cp in &cps {
        let (s, v) = call(cp, "GET", &format!("/tenants/{id}"), None, &[]).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["tenant"]["slug"], "acme");
    }
    // An error is forwarded the same way: a duplicate is ABORTED (10).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let resp = loop {
        let resp = grpc(&cps[follower], "acme").await.unwrap();
        if resp.status() == StatusCode::SERVICE_UNAVAILABLE
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        break resp;
    };
    let headers = resp.headers().clone();
    let collected = resp.into_body().collect().await.unwrap();
    let status = collected
        .trailers()
        .and_then(|t| t.get("grpc-status").cloned())
        .or_else(|| headers.get("grpc-status").cloned());
    assert_eq!(status.as_ref().map(|v| v.to_str().unwrap()), Some("10"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn reads_without_a_quorum_fail_unless_stale_is_asked_for() {
    let _one = serial().await;
    let cps = form(3).await;
    let leader = leader_index(&cps, &[0, 1, 2]).await;
    let (s, _) = call(
        &cps[leader],
        "POST",
        "/projects",
        Some(json!({ "name": "kept" })),
        &[],
    )
    .await;
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
        .oneshot(
            Request::builder()
                .uri("/projects")
                .header("x-glidex-consistency", "local")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("x-glidex-revision"));
    let (s, v) = call(
        &cps[survivor],
        "POST",
        "/projects",
        Some(json!({ "name": "nope" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_join_needs_the_right_token_and_the_pinned_ca() {
    let _one = serial().await;
    let cfg = test_config();
    let first = new_cp();
    manage::init(
        &first.manager,
        &cfg,
        InitOptions {
            advertise: Some(first.addr),
            tunnel_ip: None,
            listen: Some(first.addr),
        },
    )
    .await
    .unwrap();
    let c = cluster(&first);
    let good = manage::join_token(&c, NodeRole::Server, 600, false, "test")
        .await
        .unwrap();

    // A token for another CA's hash is refused before anything is sent.
    let evil = format!("{}.{}", &good[..good.rfind('.').unwrap()], "0".repeat(64));
    let cp = new_cp();
    let err = manage::join(
        &cp.manager,
        &cfg,
        JoinOptions {
            server: first.addr,
            token: evil,
            role: NodeRole::Server,
            advertise: Some(cp.addr),
            tunnel_ip: None,
            name: Some("cp2".into()),
            listen: Some(cp.addr),
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("not the one the token names"), "{err}");

    // An agent token can't bring in a server.
    let agent = manage::join_token(&c, NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    let err = manage::join(
        &cp.manager,
        &cfg,
        JoinOptions {
            server: first.addr,
            token: agent,
            role: NodeRole::Server,
            advertise: Some(cp.addr),
            tunnel_ip: None,
            name: Some("cp2".into()),
            listen: Some(cp.addr),
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("token is for"), "{err}");

    // The good token works once.
    manage::join(
        &cp.manager,
        &cfg,
        JoinOptions {
            server: first.addr,
            token: good.clone(),
            role: NodeRole::Server,
            advertise: Some(cp.addr),
            tunnel_ip: None,
            name: Some("cp2".into()),
            listen: Some(cp.addr),
        },
    )
    .await
    .unwrap();
    let again = new_cp();
    let err = manage::join(
        &again.manager,
        &cfg,
        JoinOptions {
            server: first.addr,
            token: good,
            role: NodeRole::Server,
            advertise: Some(again.addr),
            tunnel_ip: None,
            name: Some("cp3".into()),
            listen: Some(again.addr),
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("already used"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_new_cluster_starts_from_a_snapshot() {
    let _one = serial().await;
    let cfg = test_config();
    let first = new_cp();
    manage::init(
        &first.manager,
        &cfg,
        InitOptions {
            advertise: Some(first.addr),
            tunnel_ip: None,
            listen: Some(first.addr),
        },
    )
    .await
    .unwrap();
    let (s, _) = call(
        &first,
        "POST",
        "/projects",
        Some(json!({ "name": "survivor" })),
        &[],
    )
    .await;
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
    manage::force_new_cluster(&db_path, Some(&snap), &cfg.cluster)
        .await
        .unwrap();
    // Not the default :8842: the machine running the tests may be a cluster node itself.
    let mut cfg = cfg.clone();
    cfg.cluster.listen = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let netd = common::no_netd(&db_path);
    let manager = VmManager::open(db_path, netd, &cfg).await.unwrap();
    let router = create_router(manager.clone());
    manager.set_api_router(router.clone());
    let cp = Cp {
        manager,
        router,
        addr: "127.0.0.1:1".parse().unwrap(),
        secrets: None,
        _dir: fresh,
    };
    let c = cluster(&cp);
    c.wait_for_leader(Duration::from_secs(20)).await.unwrap();
    assert!(project_names(&cp).await.contains(&"survivor".to_string()));
    let (s, v) = create_project(&cp, "after").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}

fn vm_body(name: &str, node: &str) -> Value {
    json!({ "name": name, "vcpu_count": 1, "mem_size_mib": 256, "hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR",
            "firmware_path": "/path/to/CLOUDHV.fd", "rootfs_path": "/path/to/disk.raw", "node": node })
}

async fn wait_for<F: std::future::Future<Output = bool>>(what: &str, mut f: impl FnMut() -> F) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !f().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_vm_placed_on_an_agent_is_that_agents_and_its_status_comes_back() {
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();

    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    let link = agent
        .manager
        .node_link()
        .expect("an agent has a link")
        .clone();
    assert!(link.wait_ready(Duration::from_secs(20)).await);
    agent.manager.initialize().await.unwrap();
    agent.manager.start_controllers();

    // The agent is a node of the cluster, and answers heartbeats.
    let (s, nodes) = call(&server, "GET", "/nodes", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        nodes_of(&nodes)
            .iter()
            .any(|n| n["spec"]["name"] == "agent1" && n["spec"]["role"] == "NODE_ROLE_AGENT"),
        "{nodes}"
    );

    // A VM pinned to the agent shows up in its cache and is its own.
    let (s, vm) = call(
        &server,
        "POST",
        "/vms",
        Some(vm_body("on-agent", "agent1")),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let vm = vm_of(&vm).clone();
    let id = vm["id"].as_str().unwrap().to_string();
    let agent_node = agent.manager.local_node_id();
    assert_eq!(vm["node"], agent_node.as_str(), "{vm}");
    wait_for("the VM in the agent's cache", || async {
        agent.manager.list_vms().await.iter().any(|v| v.id == id)
    })
    .await;
    // ... but not in the server's own work: the server's controller leaves it alone.
    assert!(!server.manager.list_vms().await.is_empty());

    // The agent's controller reconciles it and its status reaches the server.
    wait_for("the controller's status on the server", || async {
        let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
        num(&vm_of(&v)["observed_generation"]) >= 1
    })
    .await;

    // A node may write its VMs' status, and nothing else: the server refuses
    // a spec change from the agent's identity.
    let c = cluster(&agent);
    let mut spec_change = agent
        .manager
        .list_vms()
        .await
        .into_iter()
        .find(|v| v.id == id)
        .unwrap();
    spec_change.spec.power = glidex_control_plane::models::PowerState::Running;
    let ws = {
        let rec = serde_json::to_vec(&spec_change).unwrap();
        glidex_control_plane::store::WriteSet {
            format: 1,
            origin: glidex_control_plane::store::Origin::Controller,
            ops: vec![glidex_control_plane::store::Op::Put {
                table: glidex_control_plane::store::TableId::Vms,
                key: id.as_bytes().to_vec(),
                value: rec,
            }],
        }
    };
    let r = c
        .client
        .request(
            &server.addr.to_string(),
            hyper::Method::POST,
            "/cluster/v1/status-write",
            &[("content-type", "application/json".into())],
            serde_json::to_vec(&ws).unwrap().into(),
        )
        .await
        .unwrap();
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "{}",
        String::from_utf8_lossy(&r.body)
    );

    // Partitioned from every server, the agent keeps working from its cache
    // and queues what it writes; when the servers are back, the queue drains.
    link.set_partitioned(true);
    let before = {
        let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
        num(&vm_of(&v)["resource_version"])
    };
    agent.manager.__touch_status_for_tests(&id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
    assert_eq!(
        num(&vm_of(&v)["resource_version"]),
        before,
        "the server saw a write the agent could not send"
    );
    let local = agent
        .manager
        .list_vms()
        .await
        .into_iter()
        .find(|v| v.id == id)
        .unwrap();
    assert_eq!(
        local.status.launch_failures, 1,
        "the agent's cache didn't take its own write"
    );
    link.set_partitioned(false);
    wait_for("the queued write to reach the server", || async {
        let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
        num(&vm_of(&v)["resource_version"]) > before
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_silent_node_is_unreachable_not_gone() {
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    let link = agent.manager.node_link().unwrap().clone();
    assert!(link.wait_ready(Duration::from_secs(20)).await);
    agent.manager.initialize().await.unwrap();
    let (s, vm) = call(
        &server,
        "POST",
        "/vms",
        Some(vm_body("quiet", "agent1")),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();

    let ready = |nodes: &Value| {
        nodes_of(nodes)
            .iter()
            .find(|n| n["spec"]["name"] == "agent1")
            .map(|n| n["status"]["ready"].as_str().unwrap().to_string())
    };
    wait_for("the agent to be Ready", || async {
        ready(&call(&server, "GET", "/nodes", None, &[]).await.1).as_deref() == Some(READY)
    })
    .await;

    link.set_partitioned(true);
    wait_for("the agent to be Unknown", || async {
        ready(&call(&server, "GET", "/nodes", None, &[]).await.1).as_deref()
            == Some("CONDITION_STATUS_UNKNOWN")
    })
    .await;
    // Its VMs say so, and nothing was stopped or moved.
    let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
    let v = vm_of(&v).clone();
    assert!(
        v["conditions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "Ready" && c["reason"] == "NodeUnreachable"),
        "{v}"
    );
    assert_eq!(v["node"], agent.manager.local_node_id().as_str());

    link.set_partitioned(false);
    wait_for("the agent to be Ready again", || async {
        ready(&call(&server, "GET", "/nodes", None, &[]).await.1).as_deref() == Some(READY)
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_server_relays_the_console_log_of_a_vm_on_an_agent() {
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    assert!(
        agent
            .manager
            .node_link()
            .unwrap()
            .wait_ready(Duration::from_secs(20))
            .await
    );
    agent.manager.initialize().await.unwrap();
    let (s, vm) = call(
        &server,
        "POST",
        "/vms",
        Some(vm_body("chatty", "agent1")),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();

    // The log lives on the agent's disk (here, the same machine).
    let paths = glidex_control_plane::paths::vm_paths(&id);
    std::fs::create_dir_all(&paths.dir).unwrap();
    std::fs::write(&paths.log, b"booting...\nlogin: ").unwrap();
    let resp = server
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/vms/{id}/console/log"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"booting...\nlogin: ");

    // The live console is a raw stream through the node API.
    let listener = tokio::net::UnixListener::bind(&paths.console_socket).unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    let mut stream = cluster(&server)
        .client
        .upgrade(
            &agent.addr.to_string(),
            &format!("/cluster/v1/vms/{id}/console"),
        )
        .await
        .unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream.write_all(b"ls\n").await.unwrap();
    let mut buf = [0u8; 3];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"ls\n");
    // Another VM's console isn't served by this node.
    assert!(
        cluster(&server)
            .client
            .upgrade(&agent.addr.to_string(), "/cluster/v1/vms/nope/console")
            .await
            .is_err()
    );

    // `VmService.Console` on the server reaches it through the agent too
    // (a Connect bidi stream; the frames go in as the body streams).
    let frame = |v: serde_json::Value| {
        let msg = v.to_string();
        let mut f = vec![0u8];
        f.extend_from_slice(&(msg.len() as u32).to_be_bytes());
        f.extend_from_slice(msg.as_bytes());
        bytes::Bytes::from(f)
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
    tx.send(Ok(frame(json!({"open": {"id": id}}))))
        .await
        .unwrap();
    // "ls\n", base64 (proto JSON bytes).
    tx.send(Ok(frame(json!({"input": "bHMK"})))).await.unwrap();
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let resp = server
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/glidex.vm.v1.VmService/Console")
                .header("content-type", "application/connect+json")
                .body(body)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    let mut got = Vec::new();
    use base64::Engine;
    while !got.ends_with(b"ls\n") {
        let f = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("console output within 10 s")
            .expect("the stream is open")
            .unwrap();
        let Ok(data) = f.into_data() else { continue };
        let mut at = 0;
        while at + 5 <= data.len() {
            let len = u32::from_be_bytes([data[at + 1], data[at + 2], data[at + 3], data[at + 4]])
                as usize;
            let m: serde_json::Value = serde_json::from_slice(&data[at + 5..at + 5 + len]).unwrap();
            if let Some(o) = m["output"].as_str() {
                got.extend(base64::engine::general_purpose::STANDARD.decode(o).unwrap());
            }
            at += 5 + len;
        }
    }
    drop(tx);
    std::fs::remove_dir_all(&paths.dir).unwrap();
}

/// spec/clustering.md §9.2.1–3: copies on chosen nodes, letting one go, a
/// file a node already has adopted instead of copied, the rest reported and
/// deleted on request, and images of another architecture kept off nodes.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn image_copies_go_where_they_are_wanted_and_spare_files_are_used() {
    use glidex_control_plane::store::{Origin, TableId};
    use sha2::{Digest, Sha256};
    let _one = serial().await;
    let mut cfg = test_config();
    cfg.reconcile.resync_secs = 2;
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    server.manager.start_controllers();
    let agent = new_cp();
    agent.manager.configure(&cfg);
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    assert!(
        agent
            .manager
            .node_link()
            .unwrap()
            .wait_ready(Duration::from_secs(20))
            .await
    );
    agent.manager.initialize().await.unwrap();
    agent.manager.start_controllers();
    let host_arch = glidex_control_plane::images::catalog::Arch::host()
        .unwrap()
        .as_str();
    let other_arch = if host_arch == "x86_64" {
        "aarch64"
    } else {
        "x86_64"
    };
    wait_until("the agent's arch", || async {
        node_json(&server, "agent1").await["status"]["features"]["arch"] == host_arch
    })
    .await;

    // Two images the server holds: one of this host's architecture, one of another.
    let put_image = |id: &str, arch: &str, data: &[u8]| {
        let sha = glidex_control_plane::cluster::pki::hex(&Sha256::digest(data));
        let img = json!({
            "id": id, "name": id, "kind": "disk", "source": { "kind": "url", "url": "http://example.invalid/x", "expected_sha256": sha },
            "status": { "state": "ready" }, "format": "qcow2", "virtual_size_bytes": 1048576, "file_size_bytes": data.len(), "sha256": sha,
            "arch": arch, "created_at": 1,
        });
        let path = server.manager.images().image_path(id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, data).unwrap();
        server
            .manager
            .database()
            .write(
                Origin::Api,
                |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                    tx.open_table(TableId::Images.definition())?
                        .insert(id, img.to_string().as_bytes())?;
                    Ok(())
                },
            )
            .unwrap();
    };
    // The agent already has the first one's file, under another name (a
    // host that joined), and an unrelated file.
    let data = vec![3u8; 50_000];
    let agent_dir = agent
        .manager
        .images()
        .image_path("x")
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("from-before.qcow2"), &data).unwrap();
    std::fs::write(agent_dir.join("stray.qcow2"), b"nothing the cluster has").unwrap();
    put_image("img-here", host_arch, &data);
    put_image("img-other", other_arch, b"another architecture");

    // Adopted, not copied: the file it had becomes its copy (§9.2.3).
    let here = agent.manager.images().image_path("img-here");
    wait_until("the agent to adopt its file", || async { here.exists() }).await;
    assert!(
        !agent_dir.join("from-before.qcow2").exists(),
        "renamed into place"
    );
    wait_until("both copies of img-here ready", || async {
        let c = server.manager.image_caches("img-here");
        c.len() == 2
            && c.iter().all(|(_, r)| {
                r.phase == glidex_control_plane::controller::image_cache::CachePhase::Ready
            })
    })
    .await;

    // A host that left and joined again: a tombstone has the same name, and
    // the live node is the one a name means.
    let store = glidex_control_plane::node::NodeStore::new(server.manager.database());
    let mut ghost = store
        .get(&cluster(&agent).identity.node_id)
        .unwrap()
        .unwrap();
    ghost.meta.id = "00000000-dead-4000-8000-000000000000".into();
    ghost.status.phase = glidex_control_plane::node::NodePhase::Removed;
    store.put(&ghost).unwrap();

    // Another architecture: not copied there, and not placed there (§9.2.1).
    let (s, e) = call(
        &server,
        "POST",
        "/images/img-other/copies",
        Some(json!({ "node": "agent1" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert!(e["message"].as_str().unwrap().contains(other_arch), "{e}");
    let (s, v) = call(
        &server,
        "POST",
        "/scheduler/preview",
        Some(json!({ "vcpu_count": 1, "mem_size_mib": 256, "image": "img-other" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v["pick"].is_null(), "{v}");
    assert!(
        v["nodes"].as_array().unwrap().iter().all(|n| n["refused"]
            .as_str()
            .unwrap_or_default()
            .contains(&format!("the VM's image is {other_arch}"))),
        "{v}"
    );

    // The server lets its copy go; the last one can't (§9.2.2).
    let server_id = cluster(&server).identity.node_id.clone();
    let agent_id = cluster(&agent).identity.node_id.clone();
    let (s, e) = call(
        &server,
        "DELETE",
        &format!("/images/img-here/copies/{server_id}"),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{e}");
    let server_file = server.manager.images().image_path("img-here");
    wait_until("the server's copy to go", || async {
        !server_file.exists() && server.manager.image_caches("img-here").len() == 1
    })
    .await;
    // Another node has the only copy: the image is ready, not missing, even
    // after a restart checked the server's own files.
    let mut img = server.manager.images().get_image("img-here").unwrap();
    img.status = glidex_control_plane::images::ImageStatus::Missing;
    server.manager.images().put_image(&img).unwrap();
    wait_until("the image ready again", || async {
        server
            .manager
            .images()
            .get_image("img-here")
            .unwrap()
            .status
            == glidex_control_plane::images::ImageStatus::Ready
    })
    .await;
    let (s, e) = call(
        &server,
        "DELETE",
        "/images/img-here/copies/agent1",
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert!(
        e["message"].as_str().unwrap().contains("last ready copy"),
        "{e}"
    );
    // And comes back when pinned again.
    let (s, v) = call(
        &server,
        "POST",
        "/images/img-here/copies",
        Some(json!({ "node": server_id })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    wait_until("the server's copy back", || async { server_file.exists() }).await;
    let (_, v) = call(&server, "GET", "/images/img-here", None, &[]).await;
    let copies = image_copies(&v);
    let pinned: Vec<&Value> = copies.iter().filter(|c| c["pinned"] == true).collect();
    assert_eq!(
        (pinned.len(), pinned[0]["node"].as_str()),
        (1, Some(server_id.as_str())),
        "{v}"
    );
    let _ = agent_id;

    // The rest is reported, and deleted when asked (§9.2.3). (Nodes report
    // every two minutes: report now.)
    let m = agent.manager.clone();
    tokio::task::spawn_blocking(move || m.report_self())
        .await
        .unwrap()
        .unwrap();
    wait_until("the stray file reported", || async {
        node_json(&server, "agent1").await["status"]["features"]["spare_image_files"]
            .as_array()
            .is_some_and(|f| f.iter().any(|f| f["name"] == "stray.qcow2"))
    })
    .await;
    let agent_node = node_json(&server, "agent1").await["meta"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, v) = call(
        &server,
        "POST",
        &format!("/nodes/{agent_node}/spare-image-files/delete"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    wait_until("the stray file gone", || async {
        !agent_dir.join("stray.qcow2").exists()
    })
    .await;
    assert!(here.exists(), "the adopted copy stays");

    // The agent leaves with its files and never drops its copy: the copy is
    // no longer counted, and deleting the image doesn't wait for it.
    let (_, v) = call(&server, "GET", "/images/img-here", None, &[]).await;
    assert!(
        image_copies(&v)
            .iter()
            .any(|c| c["node"] == agent_node.as_str()),
        "{v}"
    );
    cluster(&agent).stop().await;
    let store = glidex_control_plane::node::NodeStore::new(server.manager.database());
    let mut gone = store.get(&agent_node).unwrap().unwrap();
    gone.status.phase = glidex_control_plane::node::NodePhase::Departed;
    store.put(&gone).unwrap();
    let (_, v) = call(&server, "GET", "/images/img-here", None, &[]).await;
    assert!(
        !image_copies(&v)
            .iter()
            .any(|c| c["node"] == agent_node.as_str()),
        "a node that left has no copy: {v}"
    );
    let (s, v) = call(&server, "DELETE", "/images/img-here", None, &[]).await;
    assert!(s.is_success(), "{s} {v}");
    wait_until(
        "the image deleted though the agent left with a copy",
        || async {
            call(&server, "GET", "/images/img-here", None, &[]).await.0 == StatusCode::NOT_FOUND
        },
    )
    .await;
    assert!(server.manager.image_caches("img-here").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_node_copies_an_image_it_needs_from_a_holder_and_checks_it() {
    use glidex_control_plane::store::{Origin, TableId};
    use sha2::{Digest, Sha256};
    let _one = serial().await;
    let mut cfg = test_config();
    cfg.reconcile.resync_secs = 5;
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    server.manager.start_controllers();
    let agent = new_cp();
    agent.manager.configure(&cfg);
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    assert!(
        agent
            .manager
            .node_link()
            .unwrap()
            .wait_ready(Duration::from_secs(20))
            .await
    );
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
        .write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Images.definition())?
                    .insert(img_id, img.to_string().as_bytes())?;
                tx.open_table(TableId::Disks.definition())?
                    .insert("d1", disk.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();

    // The agent fetches it, verifies it, and says it has it.
    let agent_path = agent.manager.images().image_path(img_id);
    wait_for("the agent's copy", || async { agent_path.exists() }).await;
    assert_eq!(std::fs::read(&agent_path).unwrap(), data);
    wait_for("both copies recorded", || async {
        let c = server.manager.image_caches(img_id);
        c.len() == 2
            && c.iter().all(|(_, r)| {
                r.phase == glidex_control_plane::controller::image_cache::CachePhase::Ready
            })
    })
    .await;
    // The image says where its copies are (spec/clustering-ui.md §5.3).
    let (s, v) = call(&server, "GET", &format!("/images/{img_id}"), None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let copies = image_copies(&v);
    assert_eq!(copies.len(), 2, "{v}");
    assert!(
        copies
            .iter()
            .all(|c| c["state"] == "ready" && num(&c["bytes"]) == data.len() as u64),
        "{v}"
    );
    assert!(copies.iter().any(|c| c["node_name"] == "agent1"), "{v}");
}

/// A server that isn't the leader does a node's work too: its image
/// copies, disks and VM status are written through the leader, as an
/// agent's are (spec/clustering.md §8.3).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_follower_server_writes_its_status_through_the_leader() {
    use glidex_control_plane::store::{Origin, TableId};
    use sha2::{Digest, Sha256};
    let _one = serial().await;
    let cps = form(2).await;
    for cp in &cps[1..] {
        cp.manager.initialize().await.unwrap();
    }
    for cp in &cps {
        cp.manager.start_controllers();
    }
    let lead = leader_index(&cps, &[0, 1]).await;
    let (leader, follower) = (&cps[lead], &cps[1 - lead]);

    let data = vec![5u8; 64 * 1024];
    let sha = glidex_control_plane::cluster::pki::hex(&Sha256::digest(&data));
    let img_id = "img-f";
    let img = json!({
        "id": img_id, "name": "base-f", "kind": "disk", "source": { "kind": "url", "url": "http://example.invalid/x", "expected_sha256": sha },
        "status": { "state": "ready" }, "format": "qcow2", "virtual_size_bytes": 1048576, "file_size_bytes": data.len(), "sha256": sha,
        "arch": "x86_64", "created_at": 1,
    });
    let path = leader.manager.images().image_path(img_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &data).unwrap();
    let disk = json!({ "id": "df", "name": "df", "project": "", "format": "qcow2", "size_bytes": 0,
        "origin": { "kind": "image", "image_id": img_id, "mode": "linked" }, "created_at": 1, "phase": "pending",
        "node": follower.manager.local_node_id() });
    leader
        .manager
        .database()
        .write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Images.definition())?
                    .insert(img_id, img.to_string().as_bytes())?;
                tx.open_table(TableId::Disks.definition())?
                    .insert("df", disk.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();

    let copy = follower.manager.images().image_path(img_id);
    wait_for("the follower's copy", || async { copy.exists() }).await;
    let me = follower.manager.local_node_id();
    wait_for("the follower's copy recorded", || async {
        leader.manager.image_caches(img_id).iter().any(|(n, r)| {
            *n == me && r.phase == glidex_control_plane::controller::image_cache::CachePhase::Ready
        })
    })
    .await;
    // So are its disks (the image is no real qcow2, so making the disk
    // fails, but the follower's controller says so) and VMs.
    wait_for("the follower's disk controller's status", || async {
        leader
            .manager
            .images()
            .get_disk("df")
            .is_ok_and(|d| d.phase != glidex_control_plane::images::DiskPhase::Pending)
    })
    .await;
    // The scheduler needs the follower's capacity. Its node record brings
    // it, at start once its own record has reached it, then every two
    // minutes; ask now rather than wait.
    wait_for("the follower's capacity on the leader", || async {
        let _ = follower.manager.report_self();
        let (_, v) = call(leader, "GET", "/nodes", None, &[]).await;
        nodes_of(&v)
            .iter()
            .any(|n| n["meta"]["id"] == me.as_str() && num(&n["status"]["capacity"]["cpus"]) > 0)
    })
    .await;
    let (s, vm) = call(
        leader,
        "POST",
        "/vms",
        Some(vm_body("on-follower", &me)),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();
    wait_for("the follower's VM status on the leader", || async {
        let (_, v) = call(leader, "GET", &format!("/vms/{id}"), None, &[]).await;
        num(&vm_of(&v)["observed_generation"]) >= 1
    })
    .await;
}

/// A VM boots through its node's own copy of a firmware image, not the
/// path on the server that created it (each test node has its own image
/// directory), and waits for that copy (spec/storage.md §15).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_vm_boots_through_its_nodes_copy_of_the_firmware() {
    use glidex_control_plane::store::{Origin, TableId};
    use sha2::{Digest, Sha256};
    let _one = serial().await;
    let (server, agent) = server_and_agent().await;

    // A firmware image only the server holds.
    let data = vec![9u8; 4096];
    let sha = glidex_control_plane::cluster::pki::hex(&Sha256::digest(&data));
    let fw_id = "fw-1";
    let img = json!({
        "id": fw_id, "name": "fw-ch", "kind": "firmware", "hypervisor": "cloudhypervisor",
        "source": { "kind": "firmware", "key": "test", "url": "http://example.invalid/fw", "version": "1" },
        "status": { "state": "ready" }, "format": "raw", "virtual_size_bytes": data.len(), "file_size_bytes": data.len(),
        "sha256": sha, "arch": "x86_64", "created_at": 1,
    });
    let server_path = server.manager.images().firmware_path(fw_id);
    std::fs::create_dir_all(server_path.parent().unwrap()).unwrap();
    std::fs::write(&server_path, &data).unwrap();
    server
        .manager
        .database()
        .write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Images.definition())?
                    .insert(fw_id, img.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();
    // The write reaches the server's cache asynchronously.
    wait_for("the firmware record on the server", || async {
        server.manager.images().get_image(fw_id).is_ok()
    })
    .await;

    let (s, vm) = call(
        &server,
        "POST",
        "/vms",
        Some(json!({ "name": "fw-vm", "vcpu_count": 1, "mem_size_mib": 64, "hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR",
                     "firmware": "fw-ch", "rootfs_path": "/path/to/disk.raw", "node": "agent1" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();
    let (s, v) = call(&server, "POST", &format!("/vms/{id}/start"), None, &[]).await;
    assert!(s.is_success(), "{s} {v}");

    // The agent copies the firmware it needs, then launches through it.
    let agent_path = agent.manager.images().firmware_path(fw_id);
    assert_ne!(agent_path, server_path);
    wait_for("the agent's firmware copy", || async {
        agent_path.exists()
    })
    .await;
    let launch = wait_for_launch_file(&agent, &id).await;
    let argv: Vec<String> = serde_json::from_value(launch["argv"].clone()).unwrap();
    let argv = argv.join(" ");
    assert!(
        argv.contains(agent_path.to_str().unwrap()),
        "launched without the agent's copy: {argv}"
    );
    assert!(
        !argv.contains(server_path.to_str().unwrap()),
        "launched through the server's path: {argv}"
    );
}

/// A server and an agent named `agent1`, both running their controllers.
async fn server_and_agent() -> (Cp, Cp) {
    let mut cfg = test_config();
    cfg.reconcile.resync_secs = 5;
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    server.manager.start_controllers();
    let agent = new_cp();
    agent.manager.configure(&cfg);
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    assert!(
        agent
            .manager
            .node_link()
            .unwrap()
            .wait_ready(Duration::from_secs(20))
            .await
    );
    agent.manager.initialize().await.unwrap();
    agent.manager.start_controllers();
    (server, agent)
}

/// Shrinking or extending the root of a disk on another node is checked
/// there, not by reading a file the server doesn't have (spec/storage.md
/// §8.2).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn disk_edits_are_checked_on_the_disks_node() {
    use glidex_control_plane::store::{Origin, TableId};
    let _one = serial().await;
    let (server, agent) = server_and_agent().await;

    // A raw disk with no partition table, on the agent only.
    let (_, projects) = call(&server, "GET", "/projects", None, &[]).await;
    let project = projects["projects"].as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let size = 4 * 1024 * 1024u64;
    let disk = json!({ "id": "dr", "name": "dr", "project": project, "format": "raw", "size_bytes": size,
        "origin": { "kind": "blank" }, "created_at": 1, "phase": "ready", "node": agent.manager.local_node_id() });
    let record: glidex_control_plane::images::Disk = serde_json::from_value(disk.clone()).unwrap();
    let agent_path = agent.manager.images().disk_path(&record);
    std::fs::create_dir_all(agent_path.parent().unwrap()).unwrap();
    std::fs::File::create(&agent_path)
        .unwrap()
        .set_len(size)
        .unwrap();
    assert!(!server.manager.images().disk_path(&record).exists());
    server
        .manager
        .database()
        .write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Disks.definition())?
                    .insert("dr", disk.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();
    wait_for("the disk on the server and the agent", || async {
        server.manager.images().get_disk("dr").is_ok()
            && agent.manager.images().get_disk("dr").is_ok()
    })
    .await;

    // Accepted by the server; refused by the agent, which has the file.
    let (s, v) = call(
        &server,
        "POST",
        "/disks/dr/resize",
        Some(json!({ "size_bytes": size / 2 })),
        &[],
    )
    .await;
    assert!(s.is_success(), "{s} {v}");
    wait_for("the agent's verdict on the shrink", || async {
        server
            .manager
            .images()
            .get_disk("dr")
            .is_ok_and(|d| d.conditions.iter().any(|c| c.reason == "ResizeInvalid"))
    })
    .await;
    let (s, v) = call(
        &server,
        "POST",
        "/disks/dr/extend-root",
        Some(json!({ "mode": "EXTEND_MODE_OFFLINE" })),
        &[],
    )
    .await;
    assert!(s.is_success(), "{s} {v}");
    wait_for("the agent to take up the extend-root", || async {
        server
            .manager
            .images()
            .get_disk("dr")
            .is_ok_and(|d| d.applied_extend_root_seq == 1)
    })
    .await;
    assert_eq!(std::fs::metadata(&agent_path).unwrap().len(), size);
}

/// A datastore on some nodes, at each one's own path: each node says what
/// it finds there through the leader, and a VM whose disk is on it goes to
/// a node that reaches it (spec/storage.md S2, S14).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_disk_on_a_datastore_goes_where_the_datastore_is() {
    let _one = serial().await;
    let root = datastore_root();
    let (server, agent) = server_and_agent().await;
    let agent_id = agent.manager.local_node_id();
    let agent_dir = root.join(format!("agent-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&agent_dir).unwrap();

    // On the agent only, at a path of its own (the default doesn't exist).
    let body = json!({ "name": "fast", "path": root.join("nowhere"), "nodes": ["agent1"],
                       "node_paths": { "agent1": agent_dir } });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let ds = loop {
        let (s, v) = call(&server, "POST", "/datastores", Some(body.clone()), &[]).await;
        // Until the leader has seen every server's feature level.
        if s == StatusCode::CONFLICT && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        assert_eq!(s, StatusCode::CREATED, "{v}");
        break v["datastore"].clone();
    };
    assert_eq!(ds["nodes"], json!([agent_id]), "{ds}");
    wait_for("the agent's row", || async {
        let (_, v) = call(&server, "GET", "/datastores/fast", None, &[]).await;
        v["datastore"]["node_status"].as_array().is_some_and(|r| {
            r.iter()
                .any(|r| r["node"] == agent_id.as_str() && r["reachable"] == true)
        })
    })
    .await;
    let (_, v) = call(&server, "GET", "/datastores/fast", None, &[]).await;
    let rows = v["datastore"]["node_status"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only the agent is on it: {v}");
    assert_eq!(rows[0]["path"], agent_dir.to_str().unwrap(), "{v}");

    // An unbound disk on it pulls its VM to the agent, where it is made.
    let (_, projects) = call(&server, "GET", "/projects", None, &[]).await;
    let project = projects["projects"][0]["id"].as_str().unwrap().to_string();
    let (s, d) = call(
        &server,
        "POST",
        "/disks",
        Some(json!({ "name": "fd", "project": project, "size_gib": 1, "datastore": "fast" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{d}");
    assert_eq!(d["disk"]["datastore"], ds["id"], "{d}");
    let mut vm = vm_unpinned("on-fast");
    vm["data_disks"] = json!(["fd"]);
    let (s, v) = call(&server, "POST", "/vms", Some(vm), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(vm_of(&v)["node"], agent_id.as_str(), "{v}");
    wait_for("the disk's file at the agent's path", || async {
        std::fs::read_dir(agent_dir.join("disks"))
            .map(|d| {
                d.flatten()
                    .any(|e| e.file_name().to_string_lossy().ends_with(".qcow2"))
            })
            .unwrap_or(false)
    })
    .await;
}

/// A shared datastore (spec/storage.md S3): claimed by one node and found
/// by the other through its marker; a disk on it is bound to no node, made
/// by its worker, and its worker follows the VM that uses it (S6).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_shared_datastore_serves_every_node_and_its_disks_follow_their_vm() {
    let _one = serial().await;
    let root = datastore_root();
    let (server, agent) = server_and_agent().await;
    let agent_id = agent.manager.local_node_id();
    let server_id = server.manager.local_node_id();
    let dir = root.join(format!("nfs-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&dir).unwrap();

    let body = json!({ "name": "nfs", "path": dir, "driver": "DATASTORE_DRIVER_SHARED_DIR" });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (s, v) = call(&server, "POST", "/datastores", Some(body.clone()), &[]).await;
        if s == StatusCode::CONFLICT && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        assert_eq!(s, StatusCode::CREATED, "{v}");
        break;
    }
    wait_for("both nodes reaching it", || async {
        let (_, v) = call(&server, "GET", "/datastores/nfs", None, &[]).await;
        v["datastore"]["node_status"]
            .as_array()
            .is_some_and(|r| r.len() == 2 && r.iter().all(|r| r["reachable"] == true))
    })
    .await;
    assert!(dir.join(".glidex-datastore").exists());

    // Made by its first worker, bound to no node.
    let (_, projects) = call(&server, "GET", "/projects", None, &[]).await;
    let project = projects["projects"][0]["id"].as_str().unwrap().to_string();
    let (s, d) = call(
        &server,
        "POST",
        "/disks",
        Some(json!({ "name": "sd", "project": project, "size_gib": 1, "datastore": "nfs" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{d}");
    let id = d["disk"]["id"].as_str().unwrap().to_string();
    assert!(d["disk"]["node"].is_null(), "{d}");
    assert_eq!(
        d["disk"]["worker"],
        server_id.as_str(),
        "the leader reaches it: {d}"
    );
    let file = dir.join("disks").join(format!("{id}.qcow2"));
    wait_for("the disk's file", || async { file.exists() }).await;

    // A VM on the agent takes it: its worker becomes the agent; the disk
    // stays bound to no node.
    let mut vm = vm_body("on-nfs", "agent1");
    vm["data_disks"] = json!(["sd"]);
    let (s, v) = call(&server, "POST", "/vms", Some(vm), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let disk = server.manager.images().get_disk(&id).unwrap();
    assert_eq!(disk.worker.as_deref(), Some(agent_id.as_str()), "{disk:?}");
    assert!(disk.node.is_none());
    // The agent knows it is now its own, and the server that it isn't.
    wait_for("the agent's copy naming it the worker", || async {
        agent
            .manager
            .images()
            .get_disk(&id)
            .is_ok_and(|d| d.worker.as_deref() == Some(agent_id.as_str()))
    })
    .await;

    // Started, the VM's shim holds the disk's lock (S16).
    let vm_id = vm_of(&v)["id"].as_str().unwrap().to_string();
    let (s, r) = call(&server, "POST", &format!("/vms/{vm_id}/start"), None, &[]).await;
    assert!(s.is_success(), "{s} {r}");
    let launch = wait_for_launch_file(&agent, &vm_id).await;
    let lock = dir.join(".locks").join(format!("{id}.lock"));
    assert_eq!(launch["volume_locks"], json!([lock]), "{launch}");
}

/// A linked disk on a shared datastore, made by a node that doesn't have
/// the image (the agent, the only node on the datastore): it brings the
/// image into the datastore from the server, and the disk overlays that
/// copy (spec/storage.md S8, S9).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_worker_puts_the_image_in_a_shared_datastore_for_a_linked_disk() {
    use glidex_control_plane::store::{Origin, TableId};
    let _one = serial().await;
    let root = datastore_root();
    let (server, agent) = server_and_agent().await;
    let dir = root.join(format!("nfs-img-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir(&dir).unwrap();
    // On the agent only: it is the disk's worker from the start, and the
    // image is on the server only.
    let body = json!({ "name": "nfs-img", "path": dir, "driver": "DATASTORE_DRIVER_SHARED_DIR",
                       "nodes": ["agent1"] });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (s, v) = call(&server, "POST", "/datastores", Some(body.clone()), &[]).await;
        if s == StatusCode::CONFLICT && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        assert_eq!(s, StatusCode::CREATED, "{v}");
        break;
    }
    wait_for("the agent reaching it", || async {
        let (_, v) = call(&server, "GET", "/datastores/nfs-img", None, &[]).await;
        v["datastore"]["node_status"]
            .as_array()
            .is_some_and(|r| r.len() == 1 && r[0]["reachable"] == true)
    })
    .await;

    // A real qcow2, on the server only.
    let id = "img-c";
    let base = server.manager.images().image_path(id);
    std::fs::create_dir_all(base.parent().unwrap()).unwrap();
    assert!(
        std::process::Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2"])
            .arg(&base)
            .arg("64M")
            .status()
            .unwrap()
            .success()
    );
    let sha = glidex_control_plane::images::download::sha256_path(&base).unwrap();
    let size = std::fs::metadata(&base).unwrap().len();
    let img = json!({
        "id": id, "name": "base-c", "kind": "disk", "source": { "kind": "url", "url": "http://example.invalid/x", "expected_sha256": sha },
        "status": { "state": "ready" }, "format": "qcow2", "virtual_size_bytes": 64 << 20, "file_size_bytes": size, "sha256": sha,
        "arch": "x86_64", "created_at": 1,
    });
    server
        .manager
        .database()
        .write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Images.definition())?
                    .insert(id, img.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();
    wait_for("the image on both nodes' records", || async {
        server.manager.images().get_image(id).is_ok()
            && agent.manager.images().get_image(id).is_ok()
    })
    .await;

    // A VM on the agent with a linked disk on the shared datastore: the
    // agent is its worker.
    let (_, projects) = call(&server, "GET", "/projects", None, &[]).await;
    let project = projects["projects"][0]["id"].as_str().unwrap().to_string();
    let mut vm = vm_body("linked-nfs", "agent1");
    vm["data_disks"] = json!([]);
    let (s, d) = call(
        &server,
        "POST",
        "/disks",
        Some(json!({ "name": "ld", "project": project, "image": id, "datastore": "nfs-img" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{d}");
    assert_eq!(
        d["disk"]["worker"],
        agent.manager.local_node_id().as_str(),
        "{d}"
    );
    let disk_id = d["disk"]["id"].as_str().unwrap().to_string();
    vm["data_disks"] = json!(["ld"]);
    let (s, v) = call(&server, "POST", "/vms", Some(vm), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");

    let copy = dir.join("images").join(format!("{id}.qcow2"));
    let file = dir.join("disks").join(format!("{disk_id}.qcow2"));
    wait_for("the linked disk made on the datastore", || async {
        server
            .manager
            .images()
            .get_disk(&disk_id)
            .is_ok_and(|d| d.phase == glidex_control_plane::images::DiskPhase::Ready)
    })
    .await;
    assert!(copy.exists() && file.exists());
    let (_, i) = call(&server, "GET", &format!("/images/{id}"), None, &[]).await;
    assert!(
        image_copies(&i)
            .iter()
            .any(|c| c["datastore_name"] == "nfs-img" && c["state"] == "ready"),
        "{i}"
    );
    let info = std::process::Command::new("qemu-img")
        .args(["info", "--output=json"])
        .arg(&file)
        .output()
        .unwrap();
    let info: Value = serde_json::from_slice(&info.stdout).unwrap();
    assert_eq!(
        info["backing-filename"],
        format!("../images/{id}.qcow2"),
        "{info}"
    );
}

/// The datastore root of this test process (spec/storage.md S11), set once.
fn datastore_root() -> &'static std::path::Path {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let d = tempfile::TempDir::new().unwrap();
        // SAFETY: test-only; every test sets it the same way before reading it.
        unsafe { std::env::set_var("GLIDEX_DATASTORE_ROOT", d.path()) };
        d
    })
    .path()
}

/// The `launch.json` a node wrote for `id`, once it has.
async fn wait_for_launch_file(cp: &Cp, id: &str) -> Value {
    let vm = cp
        .manager
        .list_vms()
        .await
        .into_iter()
        .find(|v| v.id == id)
        .expect("the VM is in the node's cache");
    let path = vm.paths().launch;
    wait_for("the launch file", || async {
        std::path::Path::new(&path).exists()
    })
    .await;
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

fn vm_unpinned(name: &str) -> Value {
    json!({ "name": name, "vcpu_count": 1, "mem_size_mib": 64, "hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR",
            "firmware_path": "/path/to/CLOUDHV.fd", "rootfs_path": "/path/to/disk.raw" })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_scheduler_spreads_vms_honours_pins_and_disks_and_explains_itself() {
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    server.manager.start_controllers();
    let mut agents = Vec::new();
    for i in 1..=2 {
        let a = new_cp();
        let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
            .await
            .unwrap();
        manage::join(
            &a.manager,
            &cfg,
            JoinOptions {
                server: server.addr,
                token,
                role: NodeRole::Agent,
                advertise: Some(a.addr),
                tunnel_ip: None,
                name: Some(format!("agent{i}")),
                listen: Some(a.addr),
            },
        )
        .await
        .unwrap();
        assert!(
            a.manager
                .node_link()
                .unwrap()
                .wait_ready(Duration::from_secs(20))
                .await
        );
        a.manager.initialize().await.unwrap();
        agents.push(a);
    }
    let ids: Vec<String> = {
        let (_, n) = call(&server, "GET", "/nodes", None, &[]).await;
        nodes_of(&n)
            .iter()
            .map(|n| n["meta"]["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids.len(), 3);

    // Six VMs spread two to a node (same capacity: fewest VMs, then name).
    let mut placed = std::collections::HashMap::<String, u32>::new();
    for i in 0..6 {
        let (s, vm) = call(
            &server,
            "POST",
            "/vms",
            Some(vm_unpinned(&format!("vm{i}"))),
            &[],
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{vm}");
        *placed
            .entry(vm_of(&vm)["node"].as_str().expect("placed").to_string())
            .or_default() += 1;
    }
    assert_eq!(placed.len(), 3, "{placed:?}");
    assert!(placed.values().all(|n| *n == 2), "{placed:?}");

    // The preview (spec/clustering-ui.md §5.1) answers for every node, the
    // scheduler's choice first, and agrees with where a VM then goes.
    let (s, pv) = call(
        &server,
        "POST",
        "/scheduler/preview",
        Some(json!({ "vcpu_count": 1, "mem_size_mib": 64, "hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{pv}");
    let nodes = pv["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 3, "{pv}");
    assert!(
        nodes.iter().all(|n| n["refused"].is_null()
            && num(&n["vms"]) == 2
            && num(&n["vcpus_capacity"]) > 0),
        "{pv}"
    );
    assert_eq!(pv["pick"], nodes[0]["id"]);
    let (s, pinned) = call(
        &server,
        "POST",
        "/scheduler/preview",
        Some(json!({ "vcpu_count": 1, "mem_size_mib": 64, "node": "agent2" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(pinned["nodes"][0]["name"], "agent2", "{pinned}");
    assert!(
        pinned["nodes"].as_array().unwrap()[1..]
            .iter()
            .all(|n| n["refused"].as_str().unwrap().contains("pinned")),
        "{pinned}"
    );
    let (s, vm) = call(
        &server,
        "POST",
        "/vms",
        Some(vm_unpinned("as-previewed")),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let vm = vm_of(&vm).clone();
    assert_eq!(
        vm["node"], pv["pick"],
        "the preview's pick is where it goes"
    );
    let (s, _) = call(
        &server,
        "DELETE",
        &format!("/vms/{}", vm["id"].as_str().unwrap()),
        None,
        &[],
    )
    .await;
    assert!(s.is_success());

    // A disk bound to agent2 pulls its VM there.
    let a2 = agents[1].manager.local_node_id();
    let (_, projects) = call(&server, "GET", "/projects", None, &[]).await;
    let project = projects["projects"].as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let disk = json!({ "id": "dd", "name": "dd", "project": project, "format": "qcow2", "size_bytes": 1048576, "origin": { "kind": "blank" }, "created_at": 1, "phase": "ready", "node": a2 });
    server
        .manager
        .database()
        .write(
            glidex_control_plane::store::Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(glidex_control_plane::store::TableId::Disks.definition())?
                    .insert("dd", disk.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();
    let mut body = vm_unpinned("with-disk");
    body["data_disks"] = json!(["dd"]);
    let (s, vm) = call(&server, "POST", "/vms", Some(body), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    assert_eq!(vm_of(&vm)["node"], a2.as_str());

    // A disk is bound to a node by name as by id, and stored as its id.
    let (s, d) = call(
        &server,
        "POST",
        "/disks",
        Some(json!({ "name": "by-name", "size_gib": 1, "node": "agent2" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{d}");
    let stored = server
        .manager
        .images()
        .list_disks()
        .into_iter()
        .find(|d| d.name == "by-name")
        .unwrap();
    assert_eq!(stored.node.as_deref(), Some(a2.as_str()), "{d}");
    let (s, e) = call(
        &server,
        "POST",
        "/disks",
        Some(json!({ "name": "nowhere", "size_gib": 1, "node": "no-such-node" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");

    // Pin to a node and it goes there; a pin on a drained node waits;
    // with every node drained a VM waits and says why, then is placed.
    let (s, vm) = call(
        &server,
        "POST",
        "/vms",
        Some(vm_body("pinned", "agent1")),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    assert_eq!(
        vm_of(&vm)["node"],
        agents[0].manager.local_node_id().as_str()
    );
    for id in &ids {
        let (s, v) = call(&server, "POST", &format!("/nodes/{id}/drain"), None, &[]).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["phase"], "NODE_PHASE_DRAINING", "{v}");
        assert!(v["remaining"]["vms"].is_array(), "{v}");
    }
    let (s, vm) = call(
        &server,
        "POST",
        "/vms",
        Some(vm_body("refused", "agent1")),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let vm = vm_of(&vm).clone();
    assert!(vm["node"].is_null(), "a pin on a drained node waits: {vm}");
    let (s, _) = call(
        &server,
        "DELETE",
        &format!("/vms/{}", vm["id"].as_str().unwrap()),
        None,
        &[],
    )
    .await;
    assert!(s.is_success());
    let (s, vm) = call(&server, "POST", "/vms", Some(vm_unpinned("waits")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let vm = vm_of(&vm).clone();
    assert!(vm["node"].is_null(), "{vm}");
    let waiting = vm["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "Scheduled")
        .cloned()
        .unwrap();
    assert_eq!(waiting["reason"], "Unschedulable");
    assert!(
        waiting["message"].as_str().unwrap().contains("draining"),
        "{waiting}"
    );
    let id = vm["id"].as_str().unwrap().to_string();
    // A node is named by its id or its name, here and in a get.
    let agent1 = agents[0].manager.local_node_id();
    let (s, one) = call(&server, "GET", "/nodes/agent1", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{one}");
    assert_eq!(one["node"]["meta"]["id"], agent1.as_str(), "{one}");
    let (s, v) = call(&server, "POST", "/nodes/agent1/undrain", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["phase"], "NODE_PHASE_ACTIVE", "{v}");
    wait_for("the waiting VM to be placed", || async {
        let (_, v) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
        vm_of(&v)["node"].as_str() == Some(agent1.as_str())
    })
    .await;
}

/// Three identical hosts with a GPU at the same address, a second function
/// in its IOMMU group (spec/host-topology.md §8.4).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn vfio_devices_are_claimed_per_node_and_by_iommu_group() {
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    server.manager.start_controllers();
    let mut agents = Vec::new();
    for i in 1..=2 {
        let a = new_cp();
        let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
            .await
            .unwrap();
        manage::join(
            &a.manager,
            &cfg,
            JoinOptions {
                server: server.addr,
                token,
                role: NodeRole::Agent,
                advertise: Some(a.addr),
                tunnel_ip: None,
                name: Some(format!("agent{i}")),
                listen: Some(a.addr),
            },
        )
        .await
        .unwrap();
        assert!(
            a.manager
                .node_link()
                .unwrap()
                .wait_ready(Duration::from_secs(20))
                .await
        );
        a.manager.initialize().await.unwrap();
        agents.push(a);
    }
    let store = glidex_control_plane::node::NodeStore::new(server.manager.database());
    let nodes = store.list().unwrap();
    assert_eq!(nodes.len(), 3);
    for mut n in nodes {
        n.status.pci_devices = vec!["0000:41:00.0".into(), "0000:41:00.1".into()];
        n.status.pci_groups = [("0000:41:00.0", "31"), ("0000:41:00.1", "31")]
            .into_iter()
            .map(|(a, g)| (a.to_string(), g.to_string()))
            .collect();
        store.put(&n).unwrap();
    }
    let gpu = "/sys/bus/pci/devices/0000:41:00.0";
    let with = |name: &str, dev: &str| {
        let mut b = vm_unpinned(name);
        b["vfio_devices"] = json!([dev]);
        b["power"] = json!("POWER_STATE_STOPPED");
        b
    };

    // One VM with the GPU on each node.
    let mut placed = std::collections::BTreeSet::new();
    let mut by_node = std::collections::HashMap::new();
    for i in 0..3 {
        let (s, vm) = call(
            &server,
            "POST",
            "/vms",
            Some(with(&format!("gpu{i}"), gpu)),
            &[],
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{vm}");
        let vm = vm_of(&vm);
        let node = vm["node"].as_str().expect("placed").to_string();
        by_node.insert(node.clone(), vm["id"].as_str().unwrap().to_string());
        placed.insert(node);
    }
    assert_eq!(placed.len(), 3, "{placed:?}");

    // A fourth waits, and says why.
    let (s, vm) = call(&server, "POST", "/vms", Some(with("gpu3", gpu)), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let vm = vm_of(&vm).clone();
    assert!(vm["node"].is_null(), "{vm}");
    let waiting = vm["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "Scheduled")
        .cloned()
        .unwrap();
    assert_eq!(waiting["reason"], "Unschedulable");
    assert!(
        waiting["message"]
            .as_str()
            .unwrap()
            .contains("PCI device 0000:41:00.0 is used by VM gpu"),
        "{waiting}"
    );

    // Pinned to a node: the group's other function is refused there.
    let agent1 = agents[0].manager.local_node_id();
    let mut b = with("audio", "/sys/bus/pci/devices/0000:41:00.1");
    b["node"] = json!("agent1");
    let (s, e) = call(&server, "POST", "/vms", Some(b), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert!(
        e.to_string()
            .contains("shares IOMMU group 31 with 0000:41:00.0"),
        "{e}"
    );

    // The VM holding the group may take the rest of it, and so may a VM on
    // another node.
    let holder = by_node[&agent1].clone();
    let (s, v) = call(
        &server,
        "POST",
        &format!("/vms/{holder}/devices"),
        Some(json!({ "device_path": "/sys/bus/pci/devices/0000:41:00.1" })),
        &[],
    )
    .await;
    assert!(s.is_success(), "{s} {v}");
    let other = by_node
        .iter()
        .find(|(n, _)| **n != agent1)
        .map(|(_, id)| id.clone())
        .unwrap();
    let (s, v) = call(
        &server,
        "POST",
        &format!("/vms/{other}/devices"),
        Some(json!({ "device_path": "/sys/bus/pci/devices/0000:41:00.1" })),
        &[],
    )
    .await;
    assert!(
        s.is_success(),
        "a device at that address on another node: {s} {v}"
    );
}

/// Three nodes with 2 GiB of 2M hugepages each (spec/host-topology.md §8a).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn hugepages_are_counted_per_node_and_fill_up() {
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    server.manager.start_controllers();
    let mut agents = Vec::new();
    for i in 1..=2 {
        let a = new_cp();
        let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
            .await
            .unwrap();
        manage::join(
            &a.manager,
            &cfg,
            JoinOptions {
                server: server.addr,
                token,
                role: NodeRole::Agent,
                advertise: Some(a.addr),
                tunnel_ip: None,
                name: Some(format!("agent{i}")),
                listen: Some(a.addr),
            },
        )
        .await
        .unwrap();
        assert!(
            a.manager
                .node_link()
                .unwrap()
                .wait_ready(Duration::from_secs(20))
                .await
        );
        a.manager.initialize().await.unwrap();
        agents.push(a);
    }
    let store = glidex_control_plane::node::NodeStore::new(server.manager.database());
    let nodes = store.list().unwrap();
    assert_eq!(nodes.len(), 3);
    for mut n in nodes {
        n.status.allocatable.hugepages = [("2048".to_string(), 1024)].into();
        n.status.features.hugepage_kib = 2048;
        store.put(&n).unwrap();
    }
    let huge = |name: &str, mem: u64| {
        let mut b = vm_unpinned(name);
        b["mem_size_mib"] = json!(mem);
        b["hugepages"] = json!(true);
        b["power"] = json!("POWER_STATE_STOPPED");
        b
    };

    // Two 1 GiB guests fill each node's pool.
    let mut per_node = std::collections::HashMap::<String, Vec<String>>::new();
    for i in 0..6 {
        let (s, vm) = call(
            &server,
            "POST",
            "/vms",
            Some(huge(&format!("h{i}"), 1024)),
            &[],
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{vm}");
        let vm = vm_of(&vm);
        assert_eq!(vm["page_kib"], "2048", "{vm}");
        per_node
            .entry(vm["node"].as_str().expect("placed").to_string())
            .or_default()
            .push(vm["id"].as_str().unwrap().to_string());
    }
    assert_eq!(per_node.len(), 3, "{per_node:?}");
    assert!(per_node.values().all(|v| v.len() == 2), "{per_node:?}");

    // The next waits, and says why.
    let (s, vm) = call(&server, "POST", "/vms", Some(huge("h6", 1024)), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let vm = vm_of(&vm).clone();
    assert!(vm["node"].is_null(), "{vm}");
    let waiting = vm["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "Scheduled")
        .cloned()
        .unwrap();
    assert!(
        waiting["message"]
            .as_str()
            .unwrap()
            .contains("not enough 2M hugepages (2048 of 2048 MiB taken)"),
        "{waiting}"
    );

    // Ordinary memory is not the pool's.
    let mut plain = vm_unpinned("plain");
    plain["power"] = json!("POWER_STATE_STOPPED");
    let (s, vm) = call(&server, "POST", "/vms", Some(plain), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    assert!(vm_of(&vm)["node"].is_string(), "{vm}");
    assert_eq!(vm_of(&vm)["page_kib"], "4", "{vm}");

    // Growing a placed guest past its node's pool is refused.
    let id = per_node.values().next().unwrap()[0].clone();
    let (s, e) = call(
        &server,
        "PATCH",
        &format!("/vms/{id}"),
        Some(json!({"change": {"mem_size_mib": 2048}, "update_mask": "mem_size_mib"})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert!(
        e.to_string()
            .contains("not enough 2M hugepages (1024 of 2048 MiB taken)"),
        "{e}"
    );
}

/// spec/host-topology.md §3.2: a server reads an agent's topology on the
/// agent, over the cluster's node API.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_server_reads_a_nodes_topology_on_that_node() {
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    server.manager.start_controllers();
    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    assert!(
        agent
            .manager
            .node_link()
            .unwrap()
            .wait_ready(Duration::from_secs(20))
            .await
    );
    agent.manager.initialize().await.unwrap();
    let me = server.manager.local_node_id();
    for who in ["agent1", me.as_str()] {
        let (s, v) = call(&server, "GET", &format!("/nodes/{who}/topology"), None, &[]).await;
        assert_eq!(s, StatusCode::OK, "{who}: {v}");
        assert!(
            !v["topology"]["summary"]["numa_nodes"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{who}: {v}"
        );
    }
    let (s, v) = call(&server, "GET", "/nodes/nope/topology", None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn cluster_networks_get_a_subnet_and_vms_get_addresses_that_go_when_they_do() {
    use glidex_control_plane::cluster::config::EdgeConfig;
    let _one = serial().await;
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
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();

    // A cluster network: br-int, the underlay MTU minus Geneve, a /24 from the supernet.
    let (s, n) = call(
        &server,
        "POST",
        "/networks",
        Some(json!({ "name": "web", "mode": "NETWORK_MODE_NAT", "port_type": "PORT_TYPE_TAP", "scope": "NETWORK_SCOPE_CLUSTER" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{n}");
    let n = &n["network"];
    assert_eq!(
        (n["scope"].as_str(), n["bridge"].as_str(), n["mtu"].as_u64()),
        (Some("NETWORK_SCOPE_CLUSTER"), Some("br-int"), Some(1442))
    );
    let (s, n2) = call(&server, "POST", "/networks", Some(json!({ "name": "lab", "mode": "NETWORK_MODE_ISOLATED", "port_type": "PORT_TYPE_TAP", "scope": "NETWORK_SCOPE_CLUSTER", "subnet": "10.89.200.0/24" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{n2}");
    // Overlap and the node NAT range are refused.
    let (s, _) = call(&server, "POST", "/networks", Some(json!({ "name": "bad", "mode": "NETWORK_MODE_ISOLATED", "port_type": "PORT_TYPE_TAP", "scope": "NETWORK_SCOPE_CLUSTER", "subnet": "10.89.200.128/25" })), &[]).await;
    assert_ne!(s, StatusCode::CREATED);
    let (s, _) = call(&server, "POST", "/networks", Some(json!({ "name": "bad2", "mode": "NETWORK_MODE_ISOLATED", "port_type": "PORT_TYPE_TAP", "scope": "NETWORK_SCOPE_CLUSTER", "subnet": "10.88.9.0/24" })), &[]).await;
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
    assert!(d.ports.iter().all(|p| p.chassis.as_deref()
        == Some(glidex_ovs::ovn::chassis_name(&server.manager.local_node_id()).as_str())
        && p.ip.is_some()));
    assert!(d.edge.is_some() && !d.node_addresses.is_empty());

    // A network in use can't go; a VM that goes frees its address.
    let (s, _) = call(&server, "DELETE", "/networks/web", None, &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(
        &server,
        "DELETE",
        &format!("/vms/{}", vm_of(&a)["id"].as_str().unwrap()),
        None,
        &[],
    )
    .await;
    assert!(s.is_success());
    server.manager.clone().reconcile_ovn().await;
    assert_eq!(
        glidex_control_plane::ipam::list_reservations(&server.manager.database())
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn provider_networks_and_vpc_routers() {
    use glidex_control_plane::cluster::config::EdgeConfig;
    let _one = serial().await;
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
    cfg.cluster
        .ovn
        .bridge_mappings
        .insert("lan".into(), "br-lan".into());
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();

    // Provider networks: a physnet is required, a subnet isn't allowed, vlan is checked.
    let mk = |name: &str, extra: serde_json::Value| {
        let mut b = json!({ "name": name, "mode": "NETWORK_MODE_BRIDGED", "port_type": "PORT_TYPE_TAP", "scope": "NETWORK_SCOPE_CLUSTER" });
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        b
    };
    let (s, _) = call(&server, "POST", "/networks", Some(mk("p0", json!({}))), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(
        &server,
        "POST",
        "/networks",
        Some(mk(
            "p0",
            json!({ "physnet": "lan", "subnet": "10.1.0.0/24" }),
        )),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(
        &server,
        "POST",
        "/networks",
        Some(mk("p0", json!({ "physnet": "lan", "vlan": 5000 }))),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, n) = call(
        &server,
        "POST",
        "/networks",
        Some(mk("p1", json!({ "physnet": "lan", "vlan": 30 }))),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{n}");
    // OVN knows a network by its id (spec/tenancy.md D24).
    let id_of = |n: &serde_json::Value| n["network"]["id"].as_str().unwrap().to_string();
    let p1 = id_of(&n);
    let d = server.manager.__desired_ovn_for_tests().await.unwrap();
    assert!(d.networks.iter().any(|n| n.name == p1 && matches!(&n.kind, glidex_ovn::NetKind::Provider { physnet, vlan: Some(30) } if physnet == "lan")));
    assert!(
        glidex_control_plane::ipam::list_subnets(&server.manager.database())
            .unwrap()
            .is_empty(),
        "a provider network takes no subnet"
    );

    // A gateway node is a node in the cluster: one that left keeps its
    // record and name, and is refused by either.
    let store = glidex_control_plane::node::NodeStore::new(server.manager.database());
    let mut ghost = store.get(&server.manager.local_node_id()).unwrap().unwrap();
    ghost.meta.id = "0000dead-0000-4000-8000-000000000000".into();
    ghost.spec.name = "gone-host".into();
    ghost.status.phase = glidex_control_plane::node::NodePhase::Removed;
    store.put(&ghost).unwrap();
    for key in ["gone-host", "0000dead-0000-4000-8000-000000000000"] {
        let (s, e) = call(
            &server,
            "POST",
            "/projects/default/routers",
            Some(json!({ "name": "rg", "external": false, "gateway_nodes": [key] })),
            &[],
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
        assert!(
            e["message"]
                .as_str()
                .unwrap()
                .contains("gone-host (0000dead) is Removed"),
            "{e}"
        );
    }

    // VPC routers: one per project by default, an external address each.
    let (s, r) = call(
        &server,
        "POST",
        "/projects/default/routers",
        Some(json!({ "name": "r1" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    assert_eq!(r["router"]["status"]["external_ip"], "192.0.2.3");
    assert_eq!(r["router"]["status"]["snat_ct_zone"], 60001);
    // The site's pool (.3-.6 here, the gateway and edge take .1 and .2) runs out; the admin is
    // allowed past quota, so the pool is what stops it.
    for n in ["r2", "r3", "r4"] {
        let (s, r) = call(
            &server,
            "POST",
            "/projects/default/routers",
            Some(json!({ "name": n })),
            &[],
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{r}");
    }
    let (s, e) = call(
        &server,
        "POST",
        "/projects/default/routers",
        Some(json!({ "name": "r5" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert_eq!(e["error"], "external_pool_exhausted", "{e}");
    let (s, _) = call(
        &server,
        "POST",
        "/projects/default/routers",
        Some(json!({ "name": "r1", "external": false })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // A NAT network on the router: by name, and the plan shows the router and its zone.
    let (s, n) = call(&server, "POST", "/projects/default/networks", Some(json!({ "name": "app", "mode": "NETWORK_MODE_NAT", "port_type": "PORT_TYPE_TAP", "scope": "NETWORK_SCOPE_CLUSTER", "router": "r1" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{n}");
    let app = id_of(&n);
    let (s, n) = call(&server, "POST", "/projects/default/networks", Some(json!({ "name": "bad", "mode": "NETWORK_MODE_NAT", "port_type": "PORT_TYPE_TAP", "scope": "NETWORK_SCOPE_CLUSTER", "router": "nope" })), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{n}");
    let d = server.manager.__desired_ovn_for_tests().await.unwrap();
    assert_eq!(d.routers.len(), 4);
    let r1 = d
        .routers
        .iter()
        .find(|r| {
            r.external
                .as_ref()
                .is_some_and(|e| e.external_ip.to_string() == "192.0.2.3")
        })
        .unwrap();
    let ext = r1.external.as_ref().unwrap();
    assert_eq!(ext.snat_ct_zone, Some(60001));
    assert_eq!(d.edge.as_ref().unwrap().snat_ct_zone, Some(60000));
    assert!(
        d.networks
            .iter()
            .any(|n| n.name == app && n.router.as_deref() == Some(r1.name.as_str()))
    );

    // An existing network moves onto a router and back to the edge.
    let (s, n) = call(
        &server,
        "POST",
        "/projects/default/networks",
        Some(json!({ "name": "web", "mode": "NETWORK_MODE_NAT", "port_type": "PORT_TYPE_TAP", "scope": "NETWORK_SCOPE_CLUSTER" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{n}");
    let web = id_of(&n);
    let (s, n) = call(
        &server,
        "PUT",
        "/projects/default/networks/web/router",
        Some(json!({ "router": "r1" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{n}");
    let d = server.manager.__desired_ovn_for_tests().await.unwrap();
    assert!(
        d.networks
            .iter()
            .any(|n| n.name == web && n.router.as_deref() == Some(r1.name.as_str())),
        "on r1"
    );
    let (s, e) = call(
        &server,
        "PUT",
        "/projects/default/networks/web/router",
        Some(json!({ "router": "nope" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    // An isolated network goes on a router too, routed there like a NAT
    // one, and is isolated again once it leaves.
    let (s, n) = call(
        &server,
        "POST",
        "/projects/default/networks",
        Some(json!({ "name": "iso", "mode": "NETWORK_MODE_ISOLATED", "port_type": "PORT_TYPE_TAP", "scope": "NETWORK_SCOPE_CLUSTER" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{n}");
    let iso = id_of(&n);
    let kind = |d: &glidex_ovn::Desired| {
        d.networks
            .iter()
            .find(|n| n.name == iso)
            .map(|n| (n.kind.clone(), n.router.clone()))
            .unwrap()
    };
    assert_eq!(
        kind(&server.manager.__desired_ovn_for_tests().await.unwrap()),
        (glidex_ovn::NetKind::Isolated, None)
    );
    let (s, n) = call(
        &server,
        "PUT",
        "/projects/default/networks/iso/router",
        Some(json!({ "router": "r1" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{n}");
    assert_eq!(
        n["network"]["mode"], "NETWORK_MODE_ISOLATED",
        "its mode stays: {n}"
    );
    assert_eq!(
        kind(&server.manager.__desired_ovn_for_tests().await.unwrap()),
        (glidex_ovn::NetKind::Nat, Some(r1.name.clone()))
    );
    let (s, n) = call(
        &server,
        "PUT",
        "/projects/default/networks/iso/router",
        Some(json!({ "router": null })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{n}");
    assert_eq!(
        kind(&server.manager.__desired_ovn_for_tests().await.unwrap()),
        (glidex_ovn::NetKind::Isolated, None)
    );
    // Only the project's own networks that span the nodes.
    let (s, e) = call(
        &server,
        "PUT",
        "/projects/default/networks/p1/router",
        Some(json!({ "router": "r1" })),
        &[],
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "a host network isn't the project's: {e}"
    );
    let (s, n) = call(
        &server,
        "PUT",
        "/projects/default/networks/web/router",
        Some(json!({ "router": null })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{n}");
    assert!(n["network"]["router"].is_null(), "back on the edge: {n}");
    for name in ["web", "iso"] {
        let (s, _) = call(&server, "DELETE", &format!("/networks/{name}"), None, &[]).await;
        assert!(s.is_success(), "{s}");
    }

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
    let (s, r) = call(
        &server,
        "POST",
        "/projects/default/routers",
        Some(json!({ "name": "r6" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    assert_eq!(
        r["router"]["status"]["external_ip"], "192.0.2.4",
        "the address came back"
    );
    assert_eq!(
        r["router"]["status"]["snat_ct_zone"], 60002,
        "and so did the zone"
    );
}

async fn node_json(cp: &Cp, key: &str) -> Value {
    let (s, v) = call(cp, "GET", "/nodes", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let all = nodes_of(&v);
    // The live node a name means, before a tombstone of the same name.
    let live = |n: &&Value| {
        !matches!(
            n["status"]["phase"].as_str(),
            Some("NODE_PHASE_REMOVED" | "NODE_PHASE_DEPARTED" | "NODE_PHASE_FORGOTTEN")
        )
    };
    all.iter()
        .find(|n| n["meta"]["id"] == key)
        .or_else(|| all.iter().filter(live).find(|n| n["spec"]["name"] == key))
        .or_else(|| all.iter().find(|n| n["spec"]["name"] == key))
        .cloned()
        .unwrap_or(Value::Null)
}

fn denylist_len(cp: &Cp) -> usize {
    use redb::ReadableTableMetadata;
    let txn = cp.manager.database().begin_read().unwrap();
    txn.open_table(glidex_control_plane::store::TableId::NodeDenylist.definition())
        .map(|t| t.len().unwrap_or(0) as usize)
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn nodes_are_removed_forgotten_and_rejoin_as_themselves() {
    let _one = serial().await;
    let cps = form(4).await;
    let lead = leader_index(&cps, &[0, 1, 2, 3]).await;
    let status = manage::status_of(&cluster(&cps[lead]));
    assert_eq!(
        (
            status["raft"]["voters"].as_array().unwrap().len(),
            status["raft"]["learners"].as_array().unwrap().len()
        ),
        (3, 1),
        "{status}"
    );
    let lc = cluster(&cps[lead]);
    let learner = (0..4)
        .find(|i| {
            !lc.node
                .as_ref()
                .unwrap()
                .voters()
                .contains(&cluster(&cps[*i]).identity.raft_id)
        })
        .unwrap();
    let lname = cluster(&cps[learner]).identity.name.clone();
    let voter = (0..4).find(|i| *i != lead && *i != learner).unwrap();
    let vname = cluster(&cps[voter]).identity.name.clone();

    // A voter can't go if that leaves an even group; nothing changes.
    let (s, e) = call(
        &cps[lead],
        "POST",
        &format!("/nodes/{vname}/remove"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert!(e["message"].as_str().unwrap().contains("1, 3 or 5"), "{e}");
    assert_eq!(
        node_json(&cps[lead], &vname).await["status"]["phase"],
        "NODE_PHASE_ACTIVE"
    );
    // The server issuing the request can't remove itself, and an active node can't be purged.
    let (s, _) = call(
        &cps[lead],
        "POST",
        &format!("/nodes/{}/remove", cluster(&cps[lead]).identity.name),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call(
        &cps[lead],
        "POST",
        &format!("/nodes/{vname}/purge"),
        None,
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "an Active node can't be purged");

    // A lost learner: forgetting needs --fenced, and only for a node that has gone silent.
    let (s, _) = call(
        &cps[lead],
        "POST",
        &format!("/nodes/{lname}/forget"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "forget without --fenced");
    cluster(&cps[learner]).stop().await;
    wait_until("the node to be reported unreachable", || async {
        node_json(&cps[lead], &lname).await["status"]["ready"] != READY
    })
    .await;
    let before = denylist_len(&cps[lead]);
    let (s, r) = call(
        &cps[lead],
        "POST",
        &format!("/nodes/{lname}/forget"),
        Some(json!({ "fenced": true })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["phase"], "NODE_PHASE_FORGOTTEN", "{r}");
    assert_eq!(
        denylist_len(&cps[lead]),
        before + 1,
        "its certificate is revoked"
    );
    wait_until("the forgotten learner to leave the group", || async {
        !cluster(&cps[lead])
            .node
            .as_ref()
            .unwrap()
            .learners()
            .contains(&cluster(&cps[learner]).identity.raft_id)
    })
    .await;

    // It comes back as itself, with a new certificate, starting its Raft state afresh (D18).
    // A server left, so the CA rotates (D20): before the token is issued, so
    // the token pins the CA that signs, not the one about to be replaced.
    let ca_before = cluster(&cps[lead]).signing_fp();
    let (s, t) = call(
        &cps[lead],
        "POST",
        &format!("/nodes/{lname}/rejoin-token"),
        Some(json!({ "raft_intact": false })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{t}");
    {
        use glidex_control_plane::cluster::{net, pki, tokens};
        let pinned = tokens::parse(t["token"].as_str().unwrap())
            .unwrap()
            .2
            .to_string();
        assert_ne!(
            pinned, ca_before,
            "the rotation the forgotten server called for ran before the token"
        );
        let served = pki::public_key_fingerprint(
            &net::fetch_ca_untrusted(&cps[lead].addr.to_string())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            served, pinned,
            "the server already serves the CA the token names"
        );
    }
    let cfg = test_config();
    let out = manage::rejoin(
        &cps[learner].manager,
        &cfg,
        manage::RejoinOptions {
            server: cps[lead].addr,
            token: t["token"].as_str().unwrap().to_string(),
            node_id: None,
            advertise: Some(cps[learner].addr),
            tunnel_ip: None,
            listen: Some(cps[learner].addr),
        },
    )
    .await
    .unwrap();
    assert_eq!(out["raft_fresh"], true, "the rejoin starts Raft afresh");
    let n = node_json(&cps[lead], &lname).await;
    assert_eq!(n["status"]["phase"], "NODE_PHASE_ACTIVE", "{n}");
    assert_eq!(
        n["meta"]["id"],
        cluster(&cps[learner]).identity.node_id,
        "the same node id"
    );
    wait_until("the rejoined server to be a member again", || async {
        let c = cluster(&cps[lead]);
        let rid = cluster(&cps[learner]).identity.raft_id;
        let l = c.node.as_ref().unwrap();
        l.voters().contains(&rid) || l.learners().contains(&rid)
    })
    .await;

    // An empty node is removed: tombstone, certificate denied, out of the group; twice is fine.
    let before = denylist_len(&cps[lead]);
    let (s, r) = call(
        &cps[lead],
        "POST",
        &format!("/nodes/{lname}/remove"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["phase"], "NODE_PHASE_REMOVED", "{r}");
    assert!(denylist_len(&cps[lead]) > before);
    wait_until("the removed node to leave the group", || async {
        let c = cluster(&cps[lead]);
        let rid = cluster(&cps[learner]).identity.raft_id;
        let l = c.node.as_ref().unwrap();
        !l.voters().contains(&rid) && !l.learners().contains(&rid)
    })
    .await;
    let (s, r) = call(
        &cps[lead],
        "POST",
        &format!("/nodes/{lname}/remove"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(
        (s, r["already"].as_bool()),
        (StatusCode::OK, Some(true)),
        "removing twice is fine"
    );
    // Its id is retired: no rejoin token.
    let (s, _) = call(
        &cps[lead],
        "POST",
        &format!("/nodes/{lname}/rejoin-token"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
}

async fn wait_until<F, Fut>(what: &str, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !f().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_ca_rotates_and_every_node_renews_before_the_old_one_is_retired() {
    let _one = serial().await;
    let cps = form(3).await;
    let lead = leader_index(&cps, &[0, 1, 2]).await;
    let old_fp = cluster(&cps[lead]).signing_fp();
    // Status shows the feature level and whether the ports answer.
    let (s, st) = call(&cps[lead], "GET", "/cluster/status?ports=true", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{st}");
    assert_eq!(st["ports"].as_array().unwrap().len(), 2, "{st}");
    assert!(
        st["ports"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["reachable"] == true),
        "{st}"
    );
    assert!(st["feature_level"].as_u64().unwrap() >= 1);
    assert_eq!(st["clustered"], true, "{st}");
    assert_eq!(st["raft"]["state"], "RAFT_STATE_LEADER", "{st}");
    assert!(num(&st["raft"]["term"]) >= 1, "{st}");
    assert!(st["raft"]["id"].is_string(), "a uint64 is a string: {st}");
    // The CA's expiry and each node's certificate (spec/clustering-ui.md §7.6).
    assert!(is_time(&st["ca"]["not_after"]), "{st}");
    // Every node reports ready once it has sent its first report.
    wait_for("every node to report ready", || async {
        let (_, st) = call(&cps[lead], "GET", "/cluster/status", None, &[]).await;
        st["nodes"]
            .as_array()
            .is_some_and(|ns| ns.iter().all(|n| n["ready"] == READY))
    })
    .await;
    let (_, st) = call(&cps[lead], "GET", "/cluster/status", None, &[]).await;
    assert!(
        st["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["cert"]["current_ca"] == true
                && is_time(&n["cert"]["not_after"])
                && n["ready"] == READY
                && n["phase"] == "NODE_PHASE_ACTIVE"
                && n["role"] == "NODE_ROLE_SERVER"),
        "{st}"
    );
    let (s, r) = call(
        &cps[lead],
        "POST",
        "/cluster/rotate-ca",
        Some(json!({})),
        &[],
    )
    .await;
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
            let fps: Vec<String> = glidex_control_plane::cluster::pki::pem_bundle_ders(&trust)
                .unwrap()
                .iter()
                .map(|d| {
                    glidex_control_plane::cluster::pki::issuer_of(d, &trust).unwrap_or_default()
                })
                .collect();
            fps == vec![new_fp.clone()] && c.ca_state().is_some_and(|s| s.retiring.is_empty())
        })
    })
    .await;
    // Every node is now on the new CA as status sees it.
    wait_until("status to show every node renewed", || async {
        let (_, st) = call(&cps[lead], "GET", "/cluster/status", None, &[]).await;
        st["nodes"]
            .as_array()
            .is_some_and(|ns| ns.iter().all(|n| n["cert"]["current_ca"] == true))
    })
    .await;
    // Nothing broke: a write still goes through, from any node, and the CA's old key is gone.
    let (s, _) = create_project(&cps[(lead + 1) % 3], "after-rotation").await;
    assert_eq!(s, StatusCode::CREATED);
    for cp in &cps {
        let c = cluster(cp);
        assert!(!c.files.ca_key().with_extension("key.old").exists());
        let cert = c.files.read(c.files.cert()).unwrap();
        let der = glidex_control_plane::cluster::pki::pem_bundle_ders(&cert)
            .unwrap()
            .remove(0);
        assert_eq!(
            glidex_control_plane::cluster::pki::issuer_of(
                &der,
                &c.files.read(c.files.trust()).unwrap()
            )
            .as_deref(),
            Some(new_fp.as_str())
        );
    }
    // A node can still join, pinned to the new CA.
    let cfg = test_config();
    let extra = new_cp();
    let token = manage::join_token(&cluster(&cps[lead]), NodeRole::Server, 600, false, "test")
        .await
        .unwrap();
    assert!(token.ends_with(&new_fp));
    manage::join(
        &extra.manager,
        &cfg,
        JoinOptions {
            server: cps[lead].addr,
            token,
            role: NodeRole::Server,
            advertise: Some(extra.addr),
            tunnel_ip: None,
            name: Some("cp4".into()),
            listen: Some(extra.addr),
        },
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_node_detaches_with_its_vms_and_the_cluster_lets_go() {
    use glidex_control_plane::cluster::departure::{
        finish_pending, pending_db, pending_sha_file, receipt_file,
    };
    let _one = serial().await;
    let mut cfg = test_config();
    // For a cluster network the detach maps onto one of the node's.
    cfg.cluster.ovn.enabled = true;
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    let link = agent
        .manager
        .node_link()
        .expect("an agent has a link")
        .clone();
    assert!(link.wait_ready(Duration::from_secs(20)).await);
    agent.manager.initialize().await.unwrap();
    agent.manager.start_controllers();
    let (s, cn) = call(
        &server,
        "POST",
        "/networks",
        Some(json!({ "name": "c-web", "mode": "NETWORK_MODE_ISOLATED", "scope": "NETWORK_SCOPE_CLUSTER", "subnet": "10.89.70.0/24" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{cn}");
    let cid = cn["network"]["id"].as_str().unwrap().to_string();
    let mut body = vm_body("travels", "agent1");
    body["networks"] = json!([{ "network": "c-web" }]);
    let (s, vm) = call(&server, "POST", "/vms", Some(body), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();
    wait_for("the agent to be ready", || async {
        node_json(&server, "agent1").await["status"]["ready"] == READY
    })
    .await;

    // Mapping to a network that isn't a node network of it is refused; nothing freezes.
    let (s, e) = call(
        &server,
        "POST",
        "/nodes/agent1/detach",
        Some(json!({ "map_networks": { "nope": "nada" } })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert_eq!(
        node_json(&server, "agent1").await["status"]["phase"],
        "NODE_PHASE_ACTIVE"
    );

    // An abort returns the node to Active. The VM's NIC on a cluster network
    // goes with no network mapped for it, and the plan names the network
    // (NICs name networks by id since D24).
    let (s, p) = call(
        &server,
        "POST",
        "/nodes/agent1/detach",
        Some(json!({ "timeout_secs": 600 })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["vms"], 1, "{p}");
    assert!(is_time(&p["deadline"]), "{p}");
    assert_eq!(p["dropped_networks"], json!([cid]), "{p}");
    assert_eq!(p["network_names"], json!({ cid.as_str(): "c-web" }), "{p}");
    let plan = p["plan"].as_str().unwrap().to_string();
    let (s, a) = call(
        &server,
        "POST",
        "/nodes/agent1/detach",
        Some(json!({ "abort": true })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{a}");
    assert_eq!(a["aborted"], plan.as_str(), "{a}");
    assert_eq!(
        node_json(&server, "agent1").await["status"]["phase"],
        "NODE_PHASE_ACTIVE"
    );
    assert!(
        !pending_db(&agent_state_dir(&agent)).exists() || {
            wait_until("the pending database to go", || async {
                !pending_db(&agent_state_dir(&agent)).exists()
            })
            .await;
            true
        }
    );

    // The real thing: the node writes its bundle, the cluster commits and signs a receipt.
    let (s, p) = call(
        &server,
        "POST",
        "/nodes/agent1/detach",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let dir = agent_state_dir(&agent);
    wait_until("the receipt", || async { receipt_file(&dir).exists() }).await;
    let n = node_json(&server, "agent1").await;
    assert_eq!(n["status"]["phase"], "NODE_PHASE_DEPARTED", "{n}");
    // The detaches are listed with their state (spec/clustering-ui.md §6).
    let (s, deps) = call(&server, "GET", "/departures", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{deps}");
    let deps = deps["departures"].as_array().cloned().unwrap_or_default();
    assert!(
        deps.iter().any(|d| d["node_name"] == "agent1"
            && d["state"] == "DEPARTURE_STATE_COMMITTED"
            && d["acknowledged"] == true
            && is_time(&d["started_at"])),
        "{deps:?}"
    );
    assert!(
        deps.iter().any(|d| d["state"] == "DEPARTURE_STATE_ABORTED"
            && d["plan"] == plan.as_str()
            && d["network_names"][cid.as_str()] == "c-web"),
        "{deps:?}"
    );
    assert!(
        n["status"]["departed_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| *x == id.as_str()),
        "{n}"
    );
    let (s, _) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the cluster forgot the VM");
    // The departed node's id is retired: no rejoin token.
    let (s, _) = call(
        &server,
        "POST",
        "/nodes/agent1/rejoin-token",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Restart standalone (on a copy of what the node left on disk): the host owns the VM, as `local`.
    let fresh = tempfile::tempdir().unwrap();
    std::fs::copy(pending_db(&dir), pending_db(fresh.path())).unwrap();
    std::fs::copy(receipt_file(&dir), receipt_file(fresh.path())).unwrap();
    std::fs::copy(pending_sha_file(&dir), pending_sha_file(fresh.path())).unwrap();
    std::fs::create_dir_all(fresh.path().join("cluster")).unwrap();
    std::fs::copy(
        dir.join("cluster/ca.crt"),
        fresh.path().join("cluster/ca.crt"),
    )
    .unwrap();
    std::fs::write(fresh.path().join("glidex.db"), b"the old cluster cache").unwrap();
    assert!(finish_pending(&fresh.path().join("glidex.db")).unwrap());
    assert!(
        !fresh.path().join("cluster").exists(),
        "identity, certificates and the CA key are gone"
    );
    let standalone = common::with_db_path(fresh.path().join("glidex.db")).unwrap();
    standalone.initialize().await.unwrap();
    let vms = standalone.list_vms().await;
    assert_eq!(vms.len(), 1, "{vms:?}");
    assert_eq!(
        (
            vms[0].id.as_str(),
            vms[0].status.placement.as_ref().map(|p| p.node.as_str())
        ),
        (id.as_str(), Some("local"))
    );
    // A receipt that isn't the cluster's is not honoured.
    let other = tempfile::tempdir().unwrap();
    std::fs::copy(pending_db(&dir), pending_db(other.path())).unwrap();
    let mut r: Value = serde_json::from_slice(&std::fs::read(receipt_file(&dir)).unwrap()).unwrap();
    r["revision"] = json!(1);
    std::fs::write(receipt_file(other.path()), r.to_string()).unwrap();
    std::fs::create_dir_all(other.path().join("cluster")).unwrap();
    std::fs::copy(
        dir.join("cluster/ca.crt"),
        other.path().join("cluster/ca.crt"),
    )
    .unwrap();
    assert!(finish_pending(&other.path().join("glidex.db")).is_err());
    // Nor is a pending database other than the bundle the receipt names.
    let third = tempfile::tempdir().unwrap();
    std::fs::copy(pending_db(&dir), pending_db(third.path())).unwrap();
    std::fs::copy(receipt_file(&dir), receipt_file(third.path())).unwrap();
    std::fs::write(pending_sha_file(third.path()), "0".repeat(64)).unwrap();
    std::fs::create_dir_all(third.path().join("cluster")).unwrap();
    std::fs::copy(
        dir.join("cluster/ca.crt"),
        third.path().join("cluster/ca.crt"),
    )
    .unwrap();
    assert!(finish_pending(&third.path().join("glidex.db")).is_err());
}

fn agent_state_dir(cp: &Cp) -> std::path::PathBuf {
    cp.manager.db_path().parent().unwrap().to_path_buf()
}

/// A copy of a node's files, as the host has them: identity, certificates
/// and its cache. The live manager keeps the database open.
fn copy_node_files(cp: &Cp) -> tempfile::TempDir {
    let state = agent_state_dir(cp);
    let copy = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(copy.path().join("cluster")).unwrap();
    for f in ["identity.json", "ca.crt", "node.key", "node.crt"] {
        std::fs::copy(
            state.join("cluster").join(f),
            copy.path().join("cluster").join(f),
        )
        .unwrap();
    }
    let snap = copy.path().join("snap");
    manage::export_snapshot(&cp.manager.database(), &snap).unwrap();
    let db = glidex_control_plane::store::Db::create(copy.path().join("glidex.db")).unwrap();
    db.install_dump(
        &mut std::io::BufReader::new(std::fs::File::open(&snap).unwrap()),
        0,
        |_| Ok(()),
    )
    .unwrap();
    copy
}

/// A removed agent has no API of its own (the servers refuse it), so it
/// leaves offline: only once the cluster confirms the removal (§5.5 step 3).
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_removed_agent_leaves_offline_once_a_server_confirms_it() {
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    assert!(
        agent
            .manager
            .node_link()
            .unwrap()
            .wait_ready(Duration::from_secs(20))
            .await
    );
    agent.manager.initialize().await.unwrap();
    // Its cache says Active, as a host's does when the removal's write
    // (which also revoked its certificate) never reached it.
    let before = copy_node_files(&agent);

    // Still a member: local root can't make it leave (D14).
    let e = manage::leave_removed_offline(&before.path().join("glidex.db"))
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("still has this node as a member"),
        "{e}"
    );
    assert!(
        before.path().join("cluster/identity.json").exists(),
        "nothing changed"
    );

    let (s, r) = call(
        &server,
        "POST",
        "/nodes/agent1/remove",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    // The servers now refuse the node, and say why.
    let r = cluster(&agent)
        .client
        .request_timeout(
            &server.addr.to_string(),
            hyper::Method::GET,
            "/cluster/v1/list",
            &[],
            bytes::Bytes::new(),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(
        (r.status, body["error"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("node_removed")),
        "{body}"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("glidex-install --leave"),
        "{body}"
    );

    // Offline, from the stale cache: a server confirms it, and the host is standalone and empty.
    let (name, confirmed) = manage::leave_removed_offline(&before.path().join("glidex.db"))
        .await
        .unwrap();
    assert_eq!(name, "agent1");
    assert!(
        confirmed.contains("refuses its certificate as revoked"),
        "{confirmed}"
    );
    assert!(
        !before.path().join("cluster").exists(),
        "identity, certificates and key are gone"
    );
    let standalone = common::with_db_path(before.path().join("glidex.db")).unwrap();
    standalone.initialize().await.unwrap();
    assert!(standalone.cluster().is_none());
    assert!(standalone.list_vms().await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_node_that_left_on_its_own_is_forgotten_as_departed_and_its_bundle_can_be_built_offline()
{
    use glidex_control_plane::cluster::departure::{offline_detach, pending_db};
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    let link = agent
        .manager
        .node_link()
        .expect("an agent has a link")
        .clone();
    assert!(link.wait_ready(Duration::from_secs(20)).await);
    agent.manager.initialize().await.unwrap();
    let (s, vm) = call(
        &server,
        "POST",
        "/vms",
        Some(vm_body("left-behind", "agent1")),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();
    wait_for("the VM in the agent's cache", || async {
        agent.manager.list_vms().await.iter().any(|v| v.id == id)
    })
    .await;

    // The cluster can't tell a departed node from a partitioned one while it is still reporting.
    let (s, _) = call(
        &server,
        "POST",
        "/nodes/agent1/detach",
        Some(json!({ "departed": true })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // The host builds its bundle from its cache (what the installer's offline path does).
    // Run on a copy of its files: the live manager still has the database open.
    let copy = copy_node_files(&agent);
    let bundle = offline_detach(&copy.path().join("glidex.db")).unwrap();
    assert_eq!(bundle.vm_ids, vec![id.clone()]);
    assert!(
        !copy.path().join("cluster").exists() && !pending_db(copy.path()).exists(),
        "switched and cleaned up"
    );
    let standalone = common::with_db_path(copy.path().join("glidex.db")).unwrap();
    standalone.initialize().await.unwrap();
    assert_eq!(standalone.list_vms().await.len(), 1);

    // Now the agent really is gone: the cluster records that it left, not that it was lost.
    cluster(&agent).stop().await;
    wait_until("the node to be silent", || async {
        node_json(&server, "agent1").await["status"]["ready"] != READY
    })
    .await;
    let (s, r) = call(
        &server,
        "POST",
        "/nodes/agent1/detach",
        Some(json!({ "departed": true })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(
        (r["phase"].as_str(), r["vms"].as_u64()),
        (Some("NODE_PHASE_DEPARTED"), Some(1)),
        "{r}"
    );
    let (s, _) = call(&server, "GET", &format!("/vms/{id}"), None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_detached_host_imports_into_another_cluster_with_its_vm_after_approval() {
    use glidex_control_plane::cluster::departure::{
        finish_pending, pending_db, pending_sha_file, receipt_file,
    };
    let _one = serial().await;
    let cfg = test_config();

    // Cluster A, an agent with a VM on it, and the agent detaching with it.
    let a = new_cp();
    manage::init(
        &a.manager,
        &cfg,
        InitOptions {
            advertise: Some(a.addr),
            tunnel_ip: None,
            listen: Some(a.addr),
        },
    )
    .await
    .unwrap();
    a.manager.initialize().await.unwrap();
    let agent = new_cp();
    let token = manage::join_token(&cluster(&a), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: a.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("mover".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    let link = agent
        .manager
        .node_link()
        .expect("an agent has a link")
        .clone();
    assert!(link.wait_ready(Duration::from_secs(20)).await);
    agent.manager.initialize().await.unwrap();
    let (s, vm) = call(&a, "POST", "/vms", Some(vm_body("traveller", "mover")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();
    let src_project = vm_of(&vm)["project"].as_str().unwrap().to_string();
    wait_for("the agent to be ready", || async {
        node_json(&a, "mover").await["status"]["ready"] == READY
    })
    .await;
    let (s, p) = call(&a, "POST", "/nodes/mover/detach", Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let dir = agent_state_dir(&agent);
    wait_until("the receipt", || async { receipt_file(&dir).exists() }).await;
    let fresh = tempfile::tempdir().unwrap();
    std::fs::copy(pending_db(&dir), pending_db(fresh.path())).unwrap();
    std::fs::copy(receipt_file(&dir), receipt_file(fresh.path())).unwrap();
    std::fs::copy(pending_sha_file(&dir), pending_sha_file(fresh.path())).unwrap();
    std::fs::create_dir_all(fresh.path().join("cluster")).unwrap();
    std::fs::copy(
        dir.join("cluster/ca.crt"),
        fresh.path().join("cluster/ca.crt"),
    )
    .unwrap();
    std::fs::write(fresh.path().join("glidex.db"), b"old").unwrap();
    assert!(finish_pending(&fresh.path().join("glidex.db")).unwrap());
    let host = common::with_db_path(fresh.path().join("glidex.db")).unwrap();
    host.initialize().await.unwrap();
    assert_eq!(host.list_vms().await.len(), 1);

    // Cluster B takes it in, if an administrator approves.
    let b = new_cp();
    manage::init(
        &b.manager,
        &cfg,
        InitOptions {
            advertise: Some(b.addr),
            tunnel_ip: None,
            listen: Some(b.addr),
        },
    )
    .await
    .unwrap();
    b.manager.initialize().await.unwrap();
    let plain = manage::join_token(&cluster(&b), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    let host_addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let refused = manage::join_import(
        &host,
        &cfg,
        manage::ImportOptions {
            server: b.addr,
            token: plain,
            advertise: Some(host_addr),
            tunnel_ip: None,
            name: Some("imported".into()),
            listen: Some(host_addr),
            wait: Duration::from_secs(5),
        },
    )
    .await;
    assert!(
        refused.is_err(),
        "a token without --allow-import can't import"
    );
    assert!(host.cluster().is_none(), "the host is untouched");
    let token = manage::join_token(&cluster(&b), NodeRole::Agent, 600, true, "test")
        .await
        .unwrap();
    let (h2, cfg2, b_addr) = (host.clone(), cfg.clone(), b.addr);
    let join = tokio::spawn(async move {
        manage::join_import(
            &h2,
            &cfg2,
            manage::ImportOptions {
                server: b_addr,
                token,
                advertise: Some(host_addr),
                tunnel_ip: None,
                name: Some("imported".into()),
                listen: Some(host_addr),
                wait: Duration::from_secs(90),
            },
        )
        .await
    });
    // The plan appears, with the checks it failed: B has its own project called "default".
    wait_until("the import plan", || async {
        let (_, v) = call(&b, "GET", "/imports", None, &[]).await;
        v["plans"].as_array().is_some_and(|a| !a.is_empty())
    })
    .await;
    let (_, listed) = call(&b, "GET", "/imports", None, &[]).await;
    let plan = listed["plans"][0].clone();
    let pid = plan["plan"].as_str().unwrap().to_string();
    assert_eq!(plan["state"], "IMPORT_STATE_PENDING", "{plan}");
    assert!(
        is_time(&plan["created_at"]) && is_time(&plan["expires_at"]),
        "{plan}"
    );
    assert_eq!(plan["summary"]["vms"], 1, "{plan}");
    assert!(
        plan["problems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p.as_str().unwrap().contains("exists with another id")),
        "{plan}"
    );
    // One plan's view says what it brings, for the mapping editor (spec/clustering-ui.md §7.4).
    let (s, one) = call(&b, "GET", &format!("/imports/{pid}"), None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{one}");
    assert_eq!(one["plan"]["plan"], pid.as_str(), "{one}");
    let one = &one["plan"];
    assert!(
        one["sources"]["projects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == src_project.as_str()),
        "{one}"
    );
    assert_eq!(one["sources"]["vms"].as_array().unwrap().len(), 1, "{one}");
    let (s, e) = call(
        &b,
        "POST",
        &format!("/imports/{pid}/approve"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    let (s, _) = call(&b, "GET", &format!("/vms/{id}"), None, &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "nothing is live before approval");
    // Merge into B's default project; a dry run says it would work.
    let mapping = json!({ "projects": { src_project.clone(): "default" } });
    let (s, d) = call(
        &b,
        "POST",
        &format!("/imports/{pid}/approve"),
        Some(json!({ "projects": { src_project.clone(): "default" }, "dry_run": true })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert!(d["plan"].is_object(), "{d}");
    assert!(d["plan"].get("problems").is_none(), "no problems: {d}");
    assert_eq!(d["plan"]["state"], "IMPORT_STATE_PENDING", "a dry run: {d}");
    let (s, done) = call(
        &b,
        "POST",
        &format!("/imports/{pid}/approve"),
        Some(mapping),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{done}");
    assert_eq!(done["plan"]["state"], "IMPORT_STATE_COMMITTED", "{done}");
    let out = join.await.unwrap().unwrap();
    // The VM is the cluster's now, with its id, placed on the new node, in B's default project.
    let (s, v) = call(&b, "GET", &format!("/vms/{id}"), None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let v = vm_of(&v).clone();
    assert_eq!(v["node"], out["node_id"], "{v}");
    let (_, projects) = call(&b, "GET", "/projects", None, &[]).await;
    let default_id = projects["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "default")
        .unwrap()["id"]
        .clone();
    assert_eq!(
        v["project"], default_id,
        "merged into the existing project: {v}"
    );
    let n = node_json(&b, "imported").await;
    assert_eq!(n["spec"]["role"], "NODE_ROLE_AGENT", "{n}");
    assert!(host.cluster().is_some(), "the host is a node of B now");
    assert!(
        std::path::Path::new(out["backup"].as_str().unwrap()).exists(),
        "its standalone records were kept"
    );
    // The plan can't be approved twice.
    let (s, _) = call(
        &b,
        "POST",
        &format!("/imports/{pid}/approve"),
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_last_node_dissolves_the_cluster_and_keeps_everything() {
    use glidex_control_plane::cluster::departure::{
        finish_pending, pending_db, pending_sha_file, receipt_file,
    };
    let _one = serial().await;
    let cfg = test_config();
    let a = new_cp();
    manage::init(
        &a.manager,
        &cfg,
        InitOptions {
            advertise: Some(a.addr),
            tunnel_ip: None,
            listen: Some(a.addr),
        },
    )
    .await
    .unwrap();
    a.manager.initialize().await.unwrap();
    let (s, vm) = call(&a, "POST", "/vms", Some(vm_unpinned("keeper")), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{vm}");
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();
    wait_for("the VM to be placed", || async {
        call(&a, "GET", &format!("/vms/{id}"), None, &[]).await.1["vm"]["node"].is_string()
    })
    .await;
    // It needs confirmation, and refuses while another node is a member.
    let (s, _) = call(&a, "POST", "/cluster/dissolve", Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let agent = new_cp();
    let token = manage::join_token(&cluster(&a), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: a.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("other".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    let (s, e) = call(
        &a,
        "POST",
        "/cluster/dissolve",
        Some(json!({ "force": true })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");
    assert!(e["message"].as_str().unwrap().contains("other"), "{e}");
    let (s, _) = call(&a, "POST", "/nodes/other/remove", Some(json!({})), &[]).await;
    assert_eq!(s, StatusCode::OK);
    // Now it ends: a final snapshot, and a standalone database with the VM.
    let (s, d) = call(
        &a,
        "POST",
        "/cluster/dissolve",
        Some(json!({ "force": true })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["vms"], 1, "{d}");
    let dir = agent_state_dir(&a);
    assert!(std::path::Path::new(d["snapshot"].as_str().unwrap()).exists());
    let fresh = tempfile::tempdir().unwrap();
    std::fs::copy(pending_db(&dir), pending_db(fresh.path())).unwrap();
    std::fs::copy(receipt_file(&dir), receipt_file(fresh.path())).unwrap();
    std::fs::copy(pending_sha_file(&dir), pending_sha_file(fresh.path())).unwrap();
    std::fs::create_dir_all(fresh.path().join("cluster")).unwrap();
    std::fs::copy(
        dir.join("cluster/ca.crt"),
        fresh.path().join("cluster/ca.crt"),
    )
    .unwrap();
    std::fs::write(fresh.path().join("glidex.db"), b"old").unwrap();
    assert!(finish_pending(&fresh.path().join("glidex.db")).unwrap());
    let host = common::with_db_path(fresh.path().join("glidex.db")).unwrap();
    host.initialize().await.unwrap();
    assert_eq!(
        host.list_vms()
            .await
            .iter()
            .map(|v| v.id.as_str())
            .collect::<Vec<_>>(),
        vec![id.as_str()]
    );
}

/// The UI socket of an agent with `ui_user` set to whoever runs the test,
/// so the relay's peer check passes.
fn agent_ui_router(cp: &Cp, relay: bool) -> axum::Router {
    let mut cfg = Config {
        ui_user: nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .unwrap()
            .name,
        ..Default::default()
    };
    cfg.cluster.ui_relay = relay;
    let auth =
        glidex_control_plane::auth::AuthService::disabled_with(cp.manager.database(), cfg).unwrap();
    glidex_control_plane::api::router(Arc::new(glidex_control_plane::api::App {
        manager: cp.manager.clone(),
        auth,
    }))
}

async fn via_ui(
    router: &axum::Router,
    method: &str,
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(glidex_control_plane::api::Listener::Ui);
    req.extensions_mut()
        .insert(glidex_control_plane::api::PeerUid(
            nix::unistd::getuid().as_raw(),
        ));
    let resp = router.clone().oneshot(req).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// spec/clustering-ui.md §3.6: an agent's UI socket relays to a server, which
/// answers; a host-local path comes back to run on the agent; a relay may not
/// carry a principal; with the relay off the agent points at the servers.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn an_agents_ui_is_relayed_to_a_server_and_host_paths_run_on_the_agent() {
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    let agent = new_cp();
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    assert!(
        agent
            .manager
            .node_link()
            .unwrap()
            .wait_ready(Duration::from_secs(20))
            .await
    );
    agent.manager.initialize().await.unwrap();
    let server_id = cluster(&server).identity.node_id.clone();
    let agent_id = cluster(&agent).identity.node_id.clone();
    let ui = agent_ui_router(&agent, true);

    // Cluster-wide reads are the server's; "this node" is the one whose UI it is.
    let (s, h, v) = via_ui(&ui, "GET", "/cluster/status").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        h["x-glidex-served-by"],
        server_id.as_str(),
        "the server answered"
    );
    assert_eq!(
        (
            v["node_id"].as_str(),
            v["name"].as_str(),
            v["role"].as_str()
        ),
        (
            Some(agent_id.as_str()),
            Some("agent1"),
            Some("NODE_ROLE_AGENT")
        ),
        "{v}"
    );
    assert_eq!(v["served_by"]["node_id"], server_id.as_str(), "{v}");
    // The agent's own API socket (a host that joined without restarting):
    // the cluster won't record a Unix user of another host, so a local login
    // says where to log in instead of failing with a 500.
    let mut cfg = Config::default();
    let group = nix::unistd::Group::from_gid(nix::unistd::getgid())
        .unwrap()
        .unwrap()
        .name;
    (cfg.users_group, cfg.admin_group) = (group.clone(), group);
    let no_site_policies = tempfile::tempdir().unwrap();
    cfg.authz.policy_files_dir = no_site_policies.path().to_path_buf();
    let auth = glidex_control_plane::auth::AuthService::new(agent.manager.database(), cfg).unwrap();
    let api = glidex_control_plane::api::router(Arc::new(glidex_control_plane::api::App {
        manager: agent.manager.clone(),
        auth,
    }));
    let mut req = Request::builder()
        .uri("/cluster/status")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(glidex_control_plane::api::Listener::Api);
    req.extensions_mut()
        .insert(glidex_control_plane::api::PeerUid(
            nix::unistd::getuid().as_raw(),
        ));
    let resp = api.oneshot(req).await.unwrap();
    let status = resp.status();
    let body: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes())
        .unwrap_or(Value::Null);
    assert_eq!(
        (status, body["error"].as_str()),
        (StatusCode::FORBIDDEN, Some("agent_no_local_api")),
        "{body}"
    );
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("gxctl auth login https://127.0.0.1:8841"),
        "{body}"
    );

    // Asked directly, the server is "this node".
    let (_, v) = call(&server, "GET", "/cluster/status", None, &[]).await;
    assert_eq!(v["node_id"], server_id.as_str(), "{v}");
    assert!(v.get("served_by").is_none(), "{v}");
    let (s, _, v) = via_ui(&ui, "GET", "/nodes").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(nodes_of(&v).len(), 2, "{v}");

    // Routes with path parameters work through the relay (the relay's own
    // route parameter must not reach the API's handlers).
    let (s, _, v) = via_ui(&ui, "GET", &format!("/nodes/{agent_id}")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["node"]["meta"]["id"], agent_id.as_str(), "{v}");

    // Host-local paths are about the agent's host and run there.
    let (s, h, v) = via_ui(&ui, "GET", "/system/reconcile").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        h["x-glidex-served-by"],
        agent_id.as_str(),
        "ran on the agent"
    );
    // So do the host family's RPCs (gRPC-Web, the UI socket's protocol).
    let mut frame = vec![0u8];
    frame.extend_from_slice(&2u32.to_be_bytes());
    frame.extend_from_slice(b"{}");
    let mut req = Request::builder()
        .method("POST")
        .uri("/glidex.host.v1.PciService/ListPciDevices")
        .header("content-type", "application/grpc-web+json")
        .header("x-grpc-web", "1")
        .body(Body::from(frame))
        .unwrap();
    req.extensions_mut()
        .insert(glidex_control_plane::api::Listener::Ui);
    req.extensions_mut()
        .insert(glidex_control_plane::api::PeerUid(
            nix::unistd::getuid().as_raw(),
        ));
    let resp = ui.clone().oneshot(req).await.unwrap();
    let (s, h) = (resp.status(), resp.headers().clone());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    let tail = &text[text.len().saturating_sub(200)..];
    assert_eq!(s, StatusCode::OK, "{tail}");
    assert!(text.contains("\"devices\""), "{tail}");
    assert!(text.contains("grpc-status: 0"), "{tail}");
    assert_eq!(
        h["x-glidex-served-by"],
        agent_id.as_str(),
        "the RPC ran on the agent"
    );

    // A relay can't vouch for anyone.
    let r = cluster(&agent)
        .client
        .request(
            &server.addr.to_string(),
            axum::http::Method::GET,
            "/cluster/v1/ui/vms",
            &[("x-glidex-principal", "e30=".to_string())],
            bytes::Bytes::new(),
        )
        .await
        .unwrap();
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // Nor may a node send another node's host what it authorized (servers only).
    let r = cluster(&agent)
        .client
        .request(
            &server.addr.to_string(),
            axum::http::Method::GET,
            "/cluster/v1/host/system/reconcile",
            &[("x-glidex-authorized", "readSystemStatus".to_string())],
            bytes::Bytes::new(),
        )
        .await
        .unwrap();
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // Relay off: the agent names the servers instead.
    let off = agent_ui_router(&agent, false);
    let (s, _, v) = via_ui(&off, "GET", "/vms").await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains(&server.addr.to_string()),
        "{v}"
    );

    // A server's own UI socket is not relayed.
    let (s, h, _) = via_ui(&agent_ui_router(&server, true), "GET", "/cluster/status").await;
    assert_eq!(s, StatusCode::OK);
    assert!(h.get("x-glidex-served-by").is_none());
}

/// spec/clustering-ui.md §8a: OVN as a cluster setting. Enabling is refused
/// while a node lacks OVN, reaches an agent without a restart, and can't be
/// undone while a cluster network exists; a cluster enabled by its file is
/// moved to the setting by the leader.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn ovn_is_a_cluster_setting_every_node_follows() {
    use glidex_control_plane::store::{Origin, TableId};
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    let agent = new_cp();
    agent.manager.configure(&cfg);
    let token = manage::join_token(&cluster(&server), NodeRole::Agent, 600, false, "test")
        .await
        .unwrap();
    manage::join(
        &agent.manager,
        &cfg,
        JoinOptions {
            server: server.addr,
            token,
            role: NodeRole::Agent,
            advertise: Some(agent.addr),
            tunnel_ip: None,
            name: Some("agent1".into()),
            listen: Some(agent.addr),
        },
    )
    .await
    .unwrap();
    assert!(
        agent
            .manager
            .node_link()
            .unwrap()
            .wait_ready(Duration::from_secs(20))
            .await
    );
    agent.manager.initialize().await.unwrap();

    let (s, v) = call(&server, "GET", "/cluster/settings/ovn", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        (
            v["enabled"].as_bool().unwrap_or(false),
            v["source"].as_str()
        ),
        (false, Some("OVN_SETTINGS_SOURCE_NONE")),
        "{v}"
    );

    // The nodes report whether OVN is installed; one that says it isn't blocks enabling.
    let set_ovn = |cp: &Cp, id: &str, version: Option<&str>, central: bool| {
        let db = cp.manager.database();
        let raw = {
            let txn = db.begin_read().unwrap();
            let t = txn.open_table(TableId::Nodes.definition()).unwrap();
            let v = t.get(id).unwrap().unwrap();
            v.value().to_vec()
        };
        let mut n: Value = serde_json::from_slice(&raw).unwrap();
        n["status"]["versions"]["ovn"] = json!(version);
        n["status"]["features"]["ovn_central"] = json!(central);
        db.write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Nodes.definition())?
                    .insert(id, n.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();
    };
    let (sid, aid) = (
        server.manager.local_node_id(),
        agent.manager.local_node_id(),
    );
    set_ovn(&server, &sid, Some("24.03.2"), true);
    set_ovn(&server, &aid, None, false);
    let (s, v) = call(
        &server,
        "PUT",
        "/cluster/settings/ovn",
        Some(json!({ "enabled": true })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["message"].as_str().unwrap().contains("agent1"), "{v}");
    set_ovn(&server, &aid, Some("24.03.2"), false);

    // A bad edge is refused before anything is written.
    let edge = json!({ "physnet": "uplink", "external_cidr": "192.0.2.0/24", "gateway": "198.51.100.1", "external_ip": "192.0.2.50", "gateway_nodes": ["agent1"] });
    let (s, v) = call(
        &server,
        "PUT",
        "/cluster/settings/ovn",
        Some(json!({ "enabled": true, "edge": edge })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");

    // On: written once, followed by the agent without a restart.
    let (s, v) = call(
        &server,
        "PUT",
        "/cluster/settings/ovn",
        Some(json!({ "enabled": true, "dns_servers": ["192.0.2.53"] })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        (v["enabled"].as_bool(), v["source"].as_str()),
        (Some(true), Some("OVN_SETTINGS_SOURCE_CLUSTER")),
        "{v}"
    );
    assert_eq!(v["settings"]["dns_servers"], json!(["192.0.2.53"]), "{v}");
    assert!(is_time(&v["settings"]["updated_at"]), "{v}");
    assert!(server.manager.ovn_enabled());
    wait_for("the agent to follow the setting", || async {
        agent.manager.ovn_enabled()
    })
    .await;

    // Off is refused while a cluster network exists.
    let (s, n) = call(&server, "POST", "/networks", Some(json!({ "name": "c-net", "mode": "NETWORK_MODE_ISOLATED", "scope": "NETWORK_SCOPE_CLUSTER", "subnet": "10.89.40.0/24" })), &[]).await;
    assert_eq!(s, StatusCode::CREATED, "{n}");
    let (s, v) = call(
        &server,
        "PUT",
        "/cluster/settings/ovn",
        Some(json!({ "enabled": false })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["message"].as_str().unwrap().contains("c-net"), "{v}");

    // Anyone signed in may ask whether OVN is on (the network form does).
    let (s, v) = call(&agent, "GET", "/cluster/settings/ovn", None, &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["enabled"], true, "{v}");
    assert!(
        !v["edge"].as_bool().unwrap_or(false),
        "no shared edge yet: NAT networks can't span the nodes: {v}"
    );
    let (s, v) = call(
        &server,
        "POST",
        "/networks",
        Some(json!({ "name": "c-nat", "mode": "NETWORK_MODE_NAT", "scope": "NETWORK_SCOPE_CLUSTER" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(
        v["message"].as_str().unwrap().contains("OVN settings"),
        "says where to set it: {v}"
    );
}

/// A cluster that enabled OVN in its servers' files keeps it: the leader
/// writes the setting from its file.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn a_file_enabled_cluster_moves_to_the_setting() {
    let _one = serial().await;
    let mut cfg = test_config();
    cfg.cluster.ovn.enabled = true;
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    let (_, v) = call(&server, "GET", "/cluster/settings/ovn", None, &[]).await;
    assert_eq!(v["source"], "OVN_SETTINGS_SOURCE_FILE", "{v}");
    server.manager.migrate_ovn_settings();
    let (_, v) = call(&server, "GET", "/cluster/settings/ovn", None, &[]).await;
    assert_eq!(
        (v["source"].as_str(), v["enabled"].as_bool()),
        (Some("OVN_SETTINGS_SOURCE_CLUSTER"), Some(true)),
        "{v}"
    );
    assert_eq!(v["settings"]["updated_by"], "file", "{v}");
}

/// spec/clustering.md §11.6, §13.2: a node's br-int datapath is a setting;
/// gateway nodes must map the edge's physical network and run br-int on the
/// kernel datapath, and the API says which step is missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn gateway_nodes_need_the_physnet_and_the_kernel_datapath() {
    use glidex_control_plane::store::{Origin, TableId};
    let _one = serial().await;
    let cfg = test_config();
    let server = new_cp();
    server.manager.configure(&cfg);
    manage::init(
        &server.manager,
        &cfg,
        InitOptions {
            advertise: Some(server.addr),
            tunnel_ip: None,
            listen: Some(server.addr),
        },
    )
    .await
    .unwrap();
    server.manager.initialize().await.unwrap();
    let id = server.manager.local_node_id();
    let name = node_json(&server, &id).await["spec"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    // What the node reports: OVN installed, DPDK running, no physical network mapped yet.
    let report = |physnets: Value, dpdk: bool| {
        let db = server.manager.database();
        let raw = {
            let txn = db.begin_read().unwrap();
            let t = txn.open_table(TableId::Nodes.definition()).unwrap();
            let v = t.get(id.as_str()).unwrap().unwrap();
            v.value().to_vec()
        };
        let mut n: Value = serde_json::from_slice(&raw).unwrap();
        n["status"]["versions"]["ovn"] = json!("24.03.2");
        n["status"]["features"]["ovn_central"] = json!(true);
        n["status"]["features"]["physnets"] = physnets;
        n["status"]["features"]["dpdk"] = json!(dpdk);
        db.write(
            Origin::Api,
            |tx| -> Result<(), glidex_control_plane::store::StoreError> {
                tx.open_table(TableId::Nodes.definition())?
                    .insert(id.as_str(), n.to_string().as_bytes())?;
                Ok(())
            },
        )
        .unwrap();
    };
    let edge = json!({ "physnet": "uplink", "external_cidr": "192.0.2.0/24", "gateway": "192.0.2.1", "external_ip": "192.0.2.50", "gateway_nodes": [name] });
    let put_edge = || {
        call(
            &server,
            "PUT",
            "/cluster/settings/ovn",
            Some(json!({ "enabled": true, "edge": edge.clone() })),
            &[],
        )
    };

    report(json!({}), true);
    let (s, v) = put_edge().await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("doesn't map physical network uplink"),
        "{v}"
    );

    // Mapped through the API (no file, no restart), but DPDK runs and the
    // setting is auto: br-int would be userspace.
    let (s, v) = call(
        &server,
        "PUT",
        &format!("/nodes/{id}/ovn"),
        Some(json!({ "bridge_mappings": { "uplink": "gxbr-up" } })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["physnets"]["uplink"], "gxbr-up", "{v}");
    let (s, v) = call(
        &server,
        "PUT",
        &format!("/nodes/{id}/ovn"),
        Some(json!({ "bridge_mappings": { "bad name": "x" } })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, v) = put_edge().await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("userspace datapath"),
        "{v}"
    );

    // The node's setting decides: system, then the edge is accepted.
    let (s, v) = call(
        &server,
        "PUT",
        &format!("/nodes/{id}/ovn"),
        Some(json!({ "br_int_datapath": "BR_INT_DATAPATH_SYSTEM" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        (v["br_int_datapath"].as_str(), v["wanted"].as_str()),
        (Some("BR_INT_DATAPATH_SYSTEM"), Some("system")),
        "{v}"
    );
    assert_eq!(
        node_json(&server, &id).await["spec"]["br_int_datapath"],
        "BR_INT_DATAPATH_SYSTEM"
    );
    let (s, v) = put_edge().await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // A gateway node keeps its physical network...
    let (s, v) = call(
        &server,
        "PUT",
        &format!("/nodes/{id}/ovn"),
        Some(json!({ "bridge_mappings": {} })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(
        v["message"]
            .as_str()
            .unwrap()
            .contains("needs physical network uplink"),
        "{v}"
    );
    // ...and can't go back to the userspace datapath...
    let (s, v) = call(
        &server,
        "PUT",
        &format!("/nodes/{id}/ovn"),
        Some(json!({ "br_int_datapath": "BR_INT_DATAPATH_NETDEV" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "gateway_node", "{v}");
    // ...and the userspace datapath needs DPDK anywhere.
    report(json!({ "uplink": "gxbr-up" }), false);
    let (s, v) = call(
        &server,
        "PUT",
        &format!("/nodes/{id}/ovn"),
        Some(json!({ "br_int_datapath": "BR_INT_DATAPATH_NETDEV" })),
        &[],
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "dpdk_not_running", "{v}");
}

/// Rotating the secrets key (spec/tenancy.md §13): every server takes the
/// new key, the data keys are re-wrapped once, and every server still
/// opens the secrets.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn the_secrets_key_rotates_on_every_server() {
    use glidex_control_plane::directory::{Connector, Kind, Source};
    let _one = serial().await;
    let cps = form_with(2, || with_secrets(new_cp())).await;
    let leader = leader_index(&cps, &[0, 1]).await;
    let other = 1 - leader;
    let store = |i: usize| &cps[i].secrets.as_ref().unwrap().connectors;
    let c = Connector {
        id: "corp".into(),
        directory_id: "local".into(),
        kind: Kind::Oidc,
        display_name: "Corp".into(),
        enabled: true,
        jit: false,
        required_groups: vec![],
        require_mfa: false,
        group_teams: Default::default(),
        default_tenant: None,
        token_max_days: None,
        provisioning: Default::default(),
        group_mode: Default::default(),
        sync: None,
        settings: json!({"issuer": "https://idp.example", "client_id": "glidex"}),
        source: Source::Api,
        created_at: 0,
        updated_at: 0,
    };
    // Replicated writes block: off the test's runtime thread.
    let holder = cps[leader].secrets.clone().unwrap();
    let c2 = c.clone();
    tokio::task::spawn_blocking(move || {
        holder.connectors.create(&c2).unwrap();
        holder
            .connectors
            .set_secret(&c2, "client_secret", "s3cret-value")
            .unwrap();
    })
    .await
    .unwrap();
    let before = store(leader).kek_id();
    wait_for("the follower opens the secret", || {
        let ok = store(other)
            .secret(&c, "client_secret")
            .ok()
            .flatten()
            .is_some_and(|v| v.as_str() == "s3cret-value");
        async move { ok }
    })
    .await;

    let (st, out) = call(
        &cps[leader],
        "POST",
        "/cluster/rotate-secrets-key",
        Some(json!({})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{out}");
    assert_eq!(out["servers"], 2, "{out}");
    assert_eq!(out["rewrapped"], 1, "{out}");
    let after = store(leader).kek_id();
    assert_ne!(before, after);
    assert_eq!(
        store(other).kek_id(),
        after,
        "the follower took the new key"
    );
    wait_for("the follower opens the re-wrapped secret", || {
        let ok = store(other)
            .secret(&c, "client_secret")
            .ok()
            .flatten()
            .is_some_and(|v| v.as_str() == "s3cret-value");
        async move { ok }
    })
    .await;
    // A follower doesn't rotate itself.
    assert!(cluster(&cps[other]).rotate_secrets_key().await.is_err());
}
