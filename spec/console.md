# Console subsystem

> Access: the console socket and log live in the VM's private runtime
> directory (`0700`); clients reach them only through the API
> (`/vms/{id}/console/ws`, `/vms/{id}/console/log`, both `vm.console`).

Each running VM's serial console is:

1. **Captured** to an append-only log file on disk.
2. **Broadcast** to any number of concurrently-connected clients
   over a per-VM Unix socket (`<run dir>/vms/<id>/console.sock`).
3. **Bridged** to browsers via a WebSocket endpoint on the control
   plane.

This document explains the invariants that make that work and why
the code is structured the way it is.

## Physical I/O

Both hypervisors are wired the same way (`hypervisor/console.rs`,
`spawn_on_pty`): glidex allocates a `pty(7)` pair, puts the slave in raw
mode, passes it as the hypervisor's stdin and stdout (`setsid()` in
`pre_exec` to detach from our session; stderr goes to the log file),
and keeps the master. QEMU gets `-serial stdio`; Cloud-Hypervisor gets
console/serial mode `Tty` — the virtio console (`hvc0`) for kernel boots,
the serial port (`ttyS0`) for firmware boots, see
[hypervisors.md](hypervisors.md#firmware-boot).

### Why glidex owns the master

Output the hypervisor wrote to the slave stays readable on the master
after the hypervisor exits. The other way round — CH's `Pty` mode, where
CH owns the master and we opened the slave by the path from `vm.info` —
the hangup when CH exits throws away whatever we hadn't read yet. A
guest that powers off prints its last line ("reboot: Power down") and
CH exits within microseconds, so with a reader polling every 10 ms that
line was routinely lost. Owning the master also means the proxy runs
from `spawn`, so firmware output before boot completes is captured too.

`openpty` opens the slave `O_NOCTTY`, so the PTY never becomes the
control plane's controlling terminal (under systemd the control plane
is a session leader; a controlling terminal's hangup at VM exit would
kill it).

## Console proxy thread

Per VM, when the hypervisor process is launched, we spawn **one OS
thread** (not a Tokio task) via `console::start_console_proxy`, shared
by both backends. Inputs:

- The PTY master.
- A `UnixListener` already bound to the console socket path.
- A `File` handle opened append-only on the log file.
- An `Arc<AtomicBool>` "running" flag.

The loop does four things per tick:

1. **Non-blocking `accept()`** on the listener. On a new client:
   - Re-open the log file read-only, read the whole captured
     history, and write it to the client first. This is what gives
     late-joining browsers the pre-boot output.
   - Push the client stream (non-blocking) onto a `Vec<UnixStream>`.
2. **Read from the PTY** (non-blocking). For non-empty reads:
   - Append to the log file.
   - Write to every client; drop clients that error.
3. **Read from each client** (non-blocking) and write their bytes
   back to the PTY master.
4. `thread::sleep(10ms)` to avoid a busy spin.

### Invariant: listener outlives the PTY

The listener is held by the thread until `running.store(false)`.
Specifically, the loop **does not break** when the PTY EOFs. On
`Ok(0)` or an error from the PTY read, the PTY is closed (set to
`None`) and PTY I/O stops, but the loop keeps accepting connections and
replaying the log.

Why: a crashed guest is the moment you most want to read the log.
If the thread exited on PTY EOF, the listener would drop, the Unix
socket would become inert, and `gxctl connect` / the browser WS
bridge would fail with `Connection refused` — with no way to see
what the kernel printed before dying. See `console.rs::proxy_loop`.

### Thread shutdown

`HypervisorProcess::kill` kills the child process, *then* sets the
`running` flag to `false`, joins the console thread, and unlinks the
socket files. The thread notices the flag at the top of the next loop
iteration (worst case ~10 ms later) and, before returning, drains what
is still buffered on the master into the log — so the last output
before a stop is kept.

The log is the guest's raw byte stream and need not be valid UTF-8;
code that reads it as text (launch errors, tests) decodes it lossily
(`console::read_log`).

## Log files

- Path: `<run dir>/vms/<id>/console.log`
- Lifecycle: truncated on hypervisor spawn, appended during the
  VM's lifetime, left on disk after `kill` so the user can still
  `cat` it.
- Replay semantics: every new client read the whole log before
  starting live broadcast. Log files can grow unbounded — no
  rotation. Scale is "the lifetime of a dev microVM", not
  "production logging infrastructure."

## Console Unix socket

- Path: `<run dir>/vms/<id>/console.sock`
- Type: `SOCK_STREAM` Unix domain socket.
- Multi-client: the proxy thread `accept`s any number of clients
  and broadcasts every byte of output to all of them. Input from
  any client is written to the PTY (so clients can fight for the
  keyboard — accepted trade-off; there's no locking).
- Protocol: **raw byte stream** in both directions. No framing, no
  handshake. Anything fancier would need to be invented on top.

## Clients

Two first-party clients connect to the console Unix socket:

- **gxctl `connect` command** — sets stdin to raw mode, spawns a
  reader thread, forwards bytes bidirectionally until the user
  hits `Ctrl+]`. Works *locally* only (needs filesystem access to
  `/tmp`). See [cli.md](cli.md).
- **Control plane's own WebSocket bridge** (`api.rs::bridge_console`)
  — lets remote browsers reach the console. This is the only way
  to attach from a different machine.

## Browser bridge

`GET /vms/:id/console/ws` in the control plane:

1. Validates the VM id.
2. `UnixStream::connect` to the VM's console socket.
3. Upgrades the HTTP request to a WebSocket and enters a
   `tokio::select!` copying bytes both ways.

On the UI side, `crates/glidex-ui/ui/src/pages/VmConsole.tsx`:

- Creates an `@xterm/xterm` `Terminal` with the `@xterm/addon-fit`
  addon.
- Opens `ws(s)://<location.host>/api/vms/:id/console/ws` with
  `binaryType = "arraybuffer"`.
- On `message`, writes the received `ArrayBuffer` (or string) into
  the terminal.
- On `term.onData`, UTF-8 encodes and sends as a binary frame.
- Disposes terminal + socket + listeners on unmount.

### Dev-server proxying

Vite's dev server must forward WebSocket upgrades for this to work.
See `vite.config.ts` — the `/api` proxy is configured with
`ws: true`.

## Observability gaps (intentionally unsolved)

- **Resize**: we don't yet propagate terminal size. xterm.js sends
  a resize event; we currently ignore it. A future `Message::Text`
  sub-protocol could carry PTY ioctls. Not needed for the serial
  console of a microVM, which is 80x24 by default and not reshaped.
- **Authentication**: the WebSocket has none. Anyone who can reach
  `:8841` can read/write every console. This matches the overall
  security model (see [README](README.md) "non-goals").
