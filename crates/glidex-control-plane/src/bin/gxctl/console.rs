//! `connect` and `log`: the VM console through the API (spec/cli.md).
//!
//! The console WebSocket `GET /vms/{id}/console/ws` is opened over the
//! same transport as every other request (api.sock, TCP or TLS). Local
//! peers and token holders need no ticket; tickets are for browsers.

use crate::client::{render_error, ApiClient, Conn};
use colored::Colorize;
use futures_util::{SinkExt, StreamExt};
use hyper::header::AUTHORIZATION;
use hyper::{Method, StatusCode};
use nix::sys::termios::{self, LocalFlags, SetArg, Termios};
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::WebSocketStream;

/// `Ctrl+]` detaches.
const DETACH: u8 = 0x1d;

#[derive(Debug, serde::Deserialize)]
struct ConsoleInfo {
    available: bool,
    #[serde(default)]
    websocket: Option<String>,
}

/// Put the terminal in raw mode; returns the settings to restore.
pub fn set_raw_mode(fd: BorrowedFd<'_>) -> Option<Termios> {
    let orig = termios::tcgetattr(fd).ok()?;
    let mut raw = orig.clone();
    raw.local_flags.remove(LocalFlags::ICANON);
    raw.local_flags.remove(LocalFlags::ECHO);
    raw.local_flags.remove(LocalFlags::ISIG);
    termios::tcsetattr(fd, SetArg::TCSANOW, &raw).ok()?;
    Some(orig)
}

pub fn restore_terminal(fd: BorrowedFd<'_>, t: &Termios) {
    let _ = termios::tcsetattr(fd, SetArg::TCSANOW, t);
}

/// Open the console WebSocket at `path` (an API path).
pub async fn open_ws(client: &ApiClient, path: &str) -> Result<WebSocketStream<Box<dyn Conn>>, String> {
    let stream = client.connect().await?;
    let mut req = client.ws_url(path).into_client_request().map_err(|e| format!("console: {}", e))?;
    if let Some(h) = client.auth_header() {
        req.headers_mut().insert(AUTHORIZATION, h);
    }
    match tokio_tungstenite::client_async(req, stream).await {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::Error::Http(resp)) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = resp.body().clone().unwrap_or_default();
            Err(render_error(status, &body, !client.is_unix()).message)
        }
        Err(e) => Err(format!("console: {}", e)),
    }
}

/// Read stdin (raw) without blocking past `running`, forwarding chunks;
/// stops at `Ctrl+]` (sending what came before it).
fn pump_stdin(running: Arc<AtomicBool>, tx: tokio::sync::mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 1024];
    while running.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd for the duration of the call.
        let r = unsafe { libc::poll(&mut pfd, 1, 100) };
        if r < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if r == 0 {
            continue;
        }
        // Read the fd directly: a buffered reader could hold bytes that
        // poll no longer reports.
        // SAFETY: buf is valid for buf.len() bytes.
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        let chunk = &buf[..n as usize];
        match chunk.iter().position(|b| *b == DETACH) {
            Some(i) => {
                if i > 0 {
                    let _ = tx.blocking_send(chunk[..i].to_vec());
                }
                break;
            }
            None => {
                if tx.blocking_send(chunk.to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

pub async fn handle_connect(client: &ApiClient, vm_id: &str) {
    let info: ConsoleInfo = match client.request_json(Method::GET, &format!("/vms/{}/console", vm_id), None).await {
        Ok(i) => i,
        Err(e) => return println!("{} {}", "Error:".red(), e),
    };
    if !info.available {
        println!("{} VM is not running. Start the VM first with: start {}", "Error:".red(), vm_id);
        return;
    }
    let path = info.websocket.unwrap_or_else(|| format!("/vms/{}/console/ws", vm_id));
    let ws = match open_ws(client, &path).await {
        Ok(ws) => ws,
        Err(e) => return println!("{} {}", "Error:".red(), e),
    };
    println!("{} Connected to the VM console via {}", "Info:".cyan(), client.describe());
    println!("{} Press {} to detach from console\n", "Tip:".yellow(), "Ctrl+]".bold());

    let stdin = io::stdin();
    let orig = match set_raw_mode(stdin.as_fd()) {
        Some(t) => t,
        None => {
            println!("{} Failed to set terminal to raw mode", "Error:".red());
            return;
        }
    };
    let running = Arc::new(AtomicBool::new(true));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let r = running.clone();
    let reader = std::thread::spawn(move || pump_stdin(r, tx));

    let (mut sink, mut source) = ws.split();
    let mut closed_by_vm = false;
    loop {
        tokio::select! {
            msg = source.next() => match msg {
                Some(Ok(Message::Binary(b))) => {
                    let mut out = io::stdout().lock();
                    let _ = out.write_all(&b);
                    let _ = out.flush();
                }
                Some(Ok(Message::Text(t))) => {
                    let mut out = io::stdout().lock();
                    let _ = out.write_all(t.as_bytes());
                    let _ = out.flush();
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    closed_by_vm = true;
                    break;
                }
                Some(Ok(_)) => {}
            },
            input = rx.recv() => match input {
                Some(bytes) => {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        closed_by_vm = true;
                        break;
                    }
                }
                // Ctrl+] (or stdin closed).
                None => break,
            },
        }
    }
    running.store(false, Ordering::SeqCst);
    let _ = sink.send(Message::Close(None)).await;
    let _ = reader.join();
    restore_terminal(stdin.as_fd(), &orig);
    if closed_by_vm {
        println!("\n{} Console closed", "Info:".cyan());
    } else {
        println!("\n{} Detached from console", "Info:".cyan());
    }
}

/// `log`: the captured console output (`GET /vms/{id}/console/log`).
pub async fn handle_log(client: &ApiClient, vm_id: &str) {
    match client.request_bytes(Method::GET, &format!("/vms/{}/console/log", vm_id), None).await {
        Ok(r) if r.body.is_empty() => {
            println!("{} The console log is empty. Start the VM to see console output.", "Info:".yellow())
        }
        Ok(r) => {
            let mut out = io::stdout().lock();
            let _ = out.write_all(&r.body);
            if !r.body.ends_with(b"\n") {
                let _ = out.write_all(b"\n");
            }
            let _ = out.flush();
        }
        Err(e) => println!("{} {}", "Error:".red(), e),
    }
}
