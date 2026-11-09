//! `connect` and `log`: the VM console through the API (spec/01-cli.md).
//!
//! The console is the bidi RPC `VmService.Console`, on an HTTP/2
//! connection of its own made like every other (api.sock, TCP or TLS).
//! Local peers and token holders need no ticket; tickets are for browsers.

use crate::client::ApiClient;
use crate::tenancy::rpc_err;
use colored::Colorize;
use connectrpc::client::{BidiStream, SharedHttp2Connection};
use glidex_api::proto::glidex::vm::v1::__buffa::oneof::console_request::Request as R;
use glidex_api::proto::glidex::vm::v1::__buffa::view::ConsoleResponseView;
use glidex_api::proto::glidex::vm::v1::{
    ConsoleOpen, ConsoleRequest, GetConsoleInfoRequest, GetConsoleLogRequest, VmServiceClient,
};
use nix::sys::termios::{self, LocalFlags, SetArg, Termios};
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// An open console stream.
pub type Console = BidiStream<
    <SharedHttp2Connection as connectrpc::client::ClientTransport>::ResponseBody,
    ConsoleRequest,
    ConsoleResponseView<'static>,
>;

/// `Ctrl+]` detaches.
const DETACH: u8 = 0x1d;

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

/// Open the console of `vm_id`: the call, its `open` message, and the
/// server's answer. A refusal (unknown VM, not allowed, no console) is the
/// error; on success, whatever output already came is returned with the
/// stream, to be written once the terminal is set up.
pub async fn open_console(client: &ApiClient, vm_id: &str) -> Result<(Console, Vec<u8>), String> {
    let (t, config) = client.stream_transport().await.map_err(|e| e.message)?;
    let mut stream = VmServiceClient::new(t, config)
        .console()
        .await
        .map_err(rpc_err)?;
    stream
        .send(ConsoleRequest {
            request: Some(R::Open(Box::new(ConsoleOpen {
                id: vm_id.to_string(),
                ..Default::default()
            }))),
            ..Default::default()
        })
        .await
        .map_err(rpc_err)?;
    // The server answers (headers, or the refusal) once the console is
    // reached; there is no call to wait for headers alone, so wait in short
    // slices of the (cancel-safe) `message`, keeping any output it brings.
    let mut early = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(20), stream.message()).await {
            Ok(Ok(Some(m))) => early.extend_from_slice(&m.to_owned_message().output),
            // Opened and closed at once: the loop below reports it.
            Ok(Ok(None)) => return Ok((stream, early)),
            Ok(Err(e)) => return Err(rpc_err(e)),
            Err(_) => {}
        }
        if stream.headers().is_some() {
            return Ok((stream, early));
        }
    }
}

/// Read stdin (raw) without blocking past `running`, forwarding chunks;
/// stops at `Ctrl+]` (sending what came before it).
fn pump_stdin(running: Arc<AtomicBool>, tx: tokio::sync::mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 1024];
    while running.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
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
    // Whether the console is up, from `GetConsoleInfo`.
    let info = match crate::vm::rpc(client).await {
        Ok(svc) => svc
            .get_console_info(GetConsoleInfoRequest {
                id: vm_id.to_string(),
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned())
            .map_err(rpc_err),
        Err(e) => Err(e),
    };
    let info = match info {
        Ok(i) => i,
        Err(e) => return println!("{} {}", "Error:".red(), e),
    };
    if info.available != Some(true) {
        println!(
            "{} VM is not running. Start the VM first with: start {}",
            "Error:".red(),
            vm_id
        );
        return;
    }
    let (mut stream, early) = match open_console(client, vm_id).await {
        Ok(s) => s,
        Err(e) => return println!("{} {}", "Error:".red(), e),
    };
    println!(
        "{} Connected to the VM console via {}",
        "Info:".cyan(),
        client.describe()
    );
    println!(
        "{} Press {} to detach from console\n",
        "Tip:".yellow(),
        "Ctrl+]".bold()
    );

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

    let write_out = |b: &[u8]| {
        let mut out = io::stdout().lock();
        let _ = out.write_all(b);
        let _ = out.flush();
    };
    if !early.is_empty() {
        write_out(&early);
    }
    let mut closed_by_vm = false;
    loop {
        tokio::select! {
            msg = stream.message() => match msg {
                Ok(Some(m)) => write_out(&m.to_owned_message().output),
                // The console closed (or the call failed).
                Ok(None) | Err(_) => {
                    closed_by_vm = true;
                    break;
                }
            },
            input = rx.recv() => match input {
                Some(bytes) => {
                    let req = ConsoleRequest {
                        request: Some(R::Input(bytes)),
                        ..Default::default()
                    };
                    if stream.send(req).await.is_err() {
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
    // Done sending; dropping the stream ends the call.
    stream.close_send();
    drop(stream);
    let _ = reader.join();
    restore_terminal(stdin.as_fd(), &orig);
    if closed_by_vm {
        println!("\n{} Console closed", "Info:".cyan());
    } else {
        println!("\n{} Detached from console", "Info:".cyan());
    }
}

/// `log`: the captured console output (`GetConsoleLog`).
pub async fn handle_log(client: &ApiClient, vm_id: &str) {
    let log = match crate::vm::rpc(client).await {
        Ok(svc) => svc
            .get_console_log(GetConsoleLogRequest {
                id: vm_id.to_string(),
                ..Default::default()
            })
            .await
            .map(|r| r.into_owned().data)
            .map_err(rpc_err),
        Err(e) => Err(e),
    };
    match log {
        Ok(body) if body.is_empty() => {
            println!(
                "{} The console log is empty. Start the VM to see console output.",
                "Info:".yellow()
            )
        }
        Ok(body) => {
            let mut out = io::stdout().lock();
            let _ = out.write_all(&body);
            if !body.ends_with(b"\n") {
                let _ = out.write_all(b"\n");
            }
            let _ = out.flush();
        }
        Err(e) => println!("{} {}", "Error:".red(), e),
    }
}
