//! The VM console end to end (spec/rest-api.md "Console WebSocket",
//! spec/grpc.md §9.1): the WebSocket the browser uses and the
//! `VmService.Console` stream API clients use, against a fake shim on the
//! VM's console socket. The shim side is what glidex-vm-shim does: on
//! connect it replays the console log, then relays the guest's output, and
//! it takes keystrokes; it keeps listening after the guest exits.

use axum::Router;
use futures_util::{SinkExt, StreamExt};
use glidex_control_plane::api::create_router;
use glidex_control_plane::state::VmManager;
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio_tungstenite::tungstenite::Message;

/// The VMs' runtime directories (console sockets) for this test binary:
/// one directory, set before any manager starts.
fn run_dir() -> &'static std::path::Path {
    static DIR: OnceLock<TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = TempDir::new().unwrap();
        // SAFETY: set once, before any thread reads it (first use).
        unsafe { std::env::set_var("RUNTIME_DIRECTORY", d.path()) };
        d
    })
    .path()
}

struct Served {
    app: Router,
    addr: std::net::SocketAddr,
    _dir: TempDir,
}

/// The API on a loopback port (auth off: the caller is the system).
async fn serve() -> Served {
    run_dir();
    let dir = TempDir::new().unwrap();
    let manager = VmManager::with_db_path(dir.path().join("glidex.db")).unwrap();
    let app = create_router(manager);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let a = app.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            a.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    Served {
        app,
        addr,
        _dir: dir,
    }
}

async fn create_vm(app: &Router, name: &str) -> String {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/vms")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({ "name": name, "vcpu_count": 1, "mem_size_mib": 256,
                    "kernel_image_path": "/k", "rootfs_path": "/r" })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CREATED);
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["vm"]["id"].as_str().unwrap().to_string()
}

/// What the fake shim saw: the keystrokes it was sent.
type Typed = tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>;

/// A fake shim on `vm`'s console socket: each connection gets `replay`,
/// then `then` after the first keystroke, and the shim hangs up when told
/// to (`hang_up`).
fn fake_shim(
    vm: &str,
    replay: &'static [u8],
    then: &'static [u8],
) -> (Typed, tokio::sync::oneshot::Sender<()>) {
    let paths = glidex_control_plane::paths::vm_paths(vm);
    std::fs::create_dir_all(&paths.dir).unwrap();
    let _ = std::fs::remove_file(&paths.console_socket);
    let listener = UnixListener::bind(&paths.console_socket).unwrap();
    let (typed_tx, typed) = tokio::sync::mpsc::unbounded_channel();
    let (hang_up, mut hung_up) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        s.write_all(replay).await.unwrap();
        let mut buf = [0u8; 1024];
        let mut first = true;
        loop {
            tokio::select! {
                n = s.read(&mut buf) => {
                    let n = n.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let _ = typed_tx.send(buf[..n].to_vec());
                    if first {
                        first = false;
                        s.write_all(then).await.unwrap();
                    }
                }
                _ = &mut hung_up => return,
            }
        }
    });
    (typed, hang_up)
}

async fn next_bytes(
    ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> Option<Vec<u8>> {
    let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("a console message within 10 s")?
        .ok()?;
    match msg {
        Message::Binary(b) => Some(b.to_vec()),
        Message::Text(t) => Some(t.as_bytes().to_vec()),
        Message::Close(_) => None,
        _ => Some(Vec::new()),
    }
}

async fn open_ws(
    s: &Served,
    vm: &str,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    let tcp = tokio::net::TcpStream::connect(s.addr).await.unwrap();
    let url = format!("ws://{}/vms/{vm}/console/ws", s.addr);
    let (ws, resp) = tokio_tungstenite::client_async(url, tcp).await.unwrap();
    assert_eq!(resp.status(), 101);
    ws
}

/// The WebSocket replays the log on connect, carries the guest's output
/// and the keystrokes, and closes when the shim hangs up.
#[tokio::test(flavor = "multi_thread")]
async fn the_console_websocket_relays_the_shim_both_ways() {
    let s = serve().await;
    let vm = create_vm(&s.app, "ws-console").await;
    let (mut typed, hang_up) = fake_shim(&vm, b"boot log\r\n", b"root@vm:~# ");
    let mut ws = open_ws(&s, &vm).await;

    // Replay-on-connect: the log so far, first.
    let mut got = Vec::new();
    while !got.ends_with(b"boot log\r\n") {
        got.extend(next_bytes(&mut ws).await.expect("the replay"));
    }
    // A keystroke reaches the shim; the guest's answer comes back.
    ws.send(Message::Binary(b"ls\r".to_vec().into()))
        .await
        .unwrap();
    let k = tokio::time::timeout(Duration::from_secs(10), typed.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(k, b"ls\r");
    let mut got = Vec::new();
    while !got.ends_with(b"root@vm:~# ") {
        got.extend(next_bytes(&mut ws).await.expect("the guest's output"));
    }
    // Text frames are keystrokes too (xterm.js sends them).
    ws.send(Message::Text("x".into())).await.unwrap();
    let k = tokio::time::timeout(Duration::from_secs(10), typed.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(k, b"x");

    // The shim hangs up: the WebSocket closes.
    hang_up.send(()).unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("the close within 10 s")
        {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
            Some(Ok(_)) => continue,
        }
    }
}

/// No console socket (the VM never ran here): the WebSocket says so and closes.
#[tokio::test(flavor = "multi_thread")]
async fn a_console_without_its_socket_says_so_and_closes() {
    let s = serve().await;
    let vm = create_vm(&s.app, "ws-nosock").await;
    let mut ws = open_ws(&s, &vm).await;
    let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(&msg, Message::Text(t) if t.starts_with("Failed to connect to the VM console")),
        "{msg:?}"
    );
    assert!(next_bytes(&mut ws).await.is_none(), "then it closes");
}

/// A VM the caller can't see (or doesn't exist) is refused before any upgrade.
#[tokio::test(flavor = "multi_thread")]
async fn a_console_for_a_missing_vm_is_refused() {
    let s = serve().await;
    let tcp = tokio::net::TcpStream::connect(s.addr).await.unwrap();
    let url = format!("ws://{}/vms/nope/console/ws", s.addr);
    let e = tokio_tungstenite::client_async(url, tcp).await.unwrap_err();
    assert!(
        matches!(&e, tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 404),
        "{e:?}"
    );
}

// ---- `glidex.vm.v1.VmService.Console`: the same, as a bidi stream ------------

use glidex_api::proto::glidex::vm::v1::{
    __buffa::oneof::console_request::Request as R, ConsoleOpen, ConsoleRequest, VmServiceClient,
};

/// A client over HTTP/2 (a bidi stream needs it), gRPC.
fn rpc(s: &Served) -> VmServiceClient<connectrpc::client::HttpClient> {
    let base: axum::http::Uri = format!("http://{}", s.addr).parse().unwrap();
    let config =
        connectrpc::client::ClientConfig::new(base).with_protocol(connectrpc::Protocol::Grpc);
    VmServiceClient::new(
        connectrpc::client::HttpClient::plaintext_http2_only(),
        config,
    )
}

fn open(id: &str) -> ConsoleRequest {
    ConsoleRequest {
        request: Some(R::Open(Box::new(ConsoleOpen {
            id: id.into(),
            ..Default::default()
        }))),
        ..Default::default()
    }
}

fn input(b: &[u8]) -> ConsoleRequest {
    ConsoleRequest {
        request: Some(R::Input(b.to_vec())),
        ..Default::default()
    }
}

/// What the stream says until `want` has arrived (or it ends: `None`).
macro_rules! read_until {
    ($stream:expr, $want:expr) => {{
        let want: &[u8] = $want;
        let mut got: Vec<u8> = Vec::new();
        let mut ended = false;
        while !got.ends_with(want) {
            match tokio::time::timeout(Duration::from_secs(10), $stream.message())
                .await
                .expect("console output within 10 s")
            {
                Ok(Some(m)) => got.extend_from_slice(&m.to_owned_message().output),
                _ => {
                    ended = true;
                    break;
                }
            }
        }
        (!ended).then_some(got)
    }};
}

/// The RPC replays the log, carries both ways, and ends when the shim hangs up.
#[tokio::test(flavor = "multi_thread")]
async fn the_console_rpc_relays_the_shim_both_ways() {
    let s = serve().await;
    let vm = create_vm(&s.app, "rpc-console").await;
    let (mut typed, hang_up) = fake_shim(&vm, b"boot log\r\n", b"root@vm:~# ");
    let client = rpc(&s);
    let mut stream = client.console().await.unwrap();
    stream.send(open(&vm)).await.unwrap();
    assert!(read_until!(stream, b"boot log\r\n").is_some(), "the replay");
    stream.send(input(b"ls\r")).await.unwrap();
    let k = tokio::time::timeout(Duration::from_secs(10), typed.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(k, b"ls\r");
    assert!(
        read_until!(stream, b"root@vm:~# ").is_some(),
        "the guest's output"
    );
    hang_up.send(()).unwrap();
    // The console closed: the stream ends, without an error.
    let end = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("the end within 10 s");
    assert!(matches!(end, Ok(None)), "{end:?}");
}

/// Refusals come as the call's error: no opening message, or an unknown VM.
#[tokio::test(flavor = "multi_thread")]
async fn the_console_rpc_refuses_without_an_open_or_a_vm() {
    let s = serve().await;
    let client = rpc(&s);
    let mut stream = client.console().await.unwrap();
    stream.send(input(b"ls\r")).await.unwrap();
    let e = stream.message().await.unwrap_err();
    assert_eq!(e.code, connectrpc::ErrorCode::InvalidArgument, "{e:?}");

    let mut stream = client.console().await.unwrap();
    stream.send(open("nope")).await.unwrap();
    let e = stream.message().await.unwrap_err();
    assert_eq!(e.code, connectrpc::ErrorCode::NotFound, "{e:?}");
}

/// A client that stops sending (half-close) ends the call, as closing the
/// WebSocket does.
#[tokio::test(flavor = "multi_thread")]
async fn the_console_rpc_ends_when_the_client_stops_sending() {
    let s = serve().await;
    let vm = create_vm(&s.app, "rpc-halfclose").await;
    let (_typed, _hang_up) = fake_shim(&vm, b"boot log\r\n", b"");
    let client = rpc(&s);
    let mut stream = client.console().await.unwrap();
    stream.send(open(&vm)).await.unwrap();
    assert!(read_until!(stream, b"boot log\r\n").is_some(), "the replay");
    stream.close_send();
    let end = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("the end within 10 s");
    assert!(matches!(end, Ok(None)), "{end:?}");
}
