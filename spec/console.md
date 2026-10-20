# Console subsystem

> Access: the console socket and log live in the VM's private runtime
> directory (`0700`); clients reach them only through the API: the
> WebSocket `/vms/{id}/console/ws` (browsers), the bidi RPC
> `VmService.Console` (gxctl and other API clients) and
> `/vms/{id}/console/log` (`GetConsoleLog`), all `openConsole`
> (`vm.console`). Browser sessions also need a single-use console ticket
> ([security.md §5.6](security.md)).

Each running VM's serial console is owned by its `glidex-vm-shim`
([reconciliation.md §8](reconciliation.md#8-instances)), not by the
control plane, so it keeps being captured while the control plane is
down or restarting. It is:

1. **Captured** to an append-only, rotated log file on disk.
2. **Broadcast** to any number of concurrently-connected clients
   over a per-VM Unix socket (`<run dir>/vms/<id>/console.sock`).
3. **Bridged** by the control plane to browsers (a WebSocket) and to
   API clients such as gxctl (a bidi gRPC stream), relayed through the
   VM's node in a cluster.

Each VM has **one** guest console: the hypervisor's single stdio-backed
device, `ttyS0` or `hvc0` (below). Any number of clients may share it.

This document explains the invariants that make that work and why
the code is structured the way it is.

## Physical I/O

Both hypervisors are wired the same way (`glidex-vm-shim`,
`supervisor.rs::spawn`): the shim allocates a `pty(7)` pair (both ends
close-on-exec), puts the slave in raw mode, passes it as the
hypervisor's stdin and stdout, puts the hypervisor's stderr on a pipe,
and keeps the master. The guest device on the other end:

| Hypervisor, device | Arguments | Guest console |
|---|---|---|
| QEMU, serial | `-nodefaults … -serial stdio -display none` | serial port, `ttyS0` |
| QEMU, virtio | `-serial none -device virtio-serial-pci,… -chardev stdio,id=con0 -device virtconsole,chardev=con0` | virtio console, `hvc0` |
| Cloud Hypervisor, serial | `--console off --serial tty` | serial port, `ttyS0` |
| Cloud Hypervisor, virtio | `--console tty --serial off` | virtio console, `hvc0` |

The device is `ConsoleDevice::of(config)` (`hypervisor/mod.rs`): the
serial port for firmware boots and, by default, for kernel boots; the
virtio console for a kernel boot whose last `console=` in
`kernel_args` is `hvc…` ([kernel-boot.md §5.2](kernel-boot.md#52-console-k5);
why serial for firmware, in [hypervisors.md](hypervisors.md#firmware-boot)).
There is never a second console device: the kernel arguments pick the
one there is.

In the child, `pre_exec` calls `setsid()` and `TIOCSCTTY`, making the
PTY the hypervisor's controlling terminal, and sets `PR_SET_PDEATHSIG =
SIGKILL`. **Invariant (D8): the hypervisor never outlives its shim.**
When the shim dies the master closes, the kernel hangs the terminal up
and sends the hypervisor SIGHUP (raw mode: no other terminal signal
reaches it). That survives exec of a binary with file capabilities
(`cloud-hypervisor`'s `cap_net_admin`), which clears the death signal
(F7); the shim resets its own `SIG_IGN`s to default first, since ignored
dispositions survive exec.

### Why the shim owns the master

Output the hypervisor wrote to the slave stays readable on the master
after the hypervisor exits. The other way round — CH's `Pty` mode, where
CH owns the master and we opened the slave by the path from `vm.info` —
the hangup when CH exits throws away whatever we hadn't read yet. A
guest that powers off prints its last line ("reboot: Power down") and
CH exits within microseconds, so with a reader polling every 10 ms that
line was routinely lost. Owning the master also means the proxy runs
before the hypervisor starts, so firmware output before boot completes
is captured too.

Why the *shim* and not the control plane: whoever holds the master
decides when the guest's terminal hangs up. While the control plane
held it, a control-plane restart killed or deafened every VM (D1).

## Console proxy thread

Per instance the shim runs **one OS thread** (`proxy.rs::proxy_loop`,
moved from the control plane's former `hypervisor/console.rs`). It is
started before the hypervisor and owns:

- the `UnixListener` bound to the console socket;
- the log (`LogWriter`), of which it is the only writer: guest output,
  the hypervisor's stderr and the shim's separator lines all go through
  it, so rotation never leaves a writer on the old file;
- the PTY master and the stderr pipe, handed over once the hypervisor
  is spawned (again for QEMU's fallback launch).

The loop, every 10 ms:

1. **Non-blocking `accept()`.** A new client gets the *current*
   `console.log` queued as replay, then live output. This is what gives
   late-joining browsers the pre-boot output. The replay leaves out the
   guest's queries to its terminal (see [Log files](#log-files)).
2. **Read the PTY** (non-blocking): append to the log, queue for every
   client. **Read the stderr pipe**: append to the log and keep the last
   64 KiB for a launch error; stderr is never sent to clients.
3. **Flush** each client's queue as far as its socket takes it.
4. **Read from each client** (non-blocking) and write their bytes to the
   PTY master.

Per-client queues: what a client's socket doesn't take now waits, up to
the replay plus 4 MiB. A slow client therefore neither stalls the
console nor loses output; one further behind is dropped and can
reconnect (and replay).

### Invariant: listener outlives the PTY

The listener is held until the shim is released, not until the
hypervisor exits. On EOF or an error from the PTY read, the PTY is
dropped and PTY I/O stops, but the loop keeps accepting connections and
replaying the log; `instance.json` says `phase: exited` and
`GET /vms/{id}/console` keeps `available: true` until the controller
sends `release`.

Why: a crashed guest is the moment you most want to read the log.
If the thread exited on PTY EOF, the listener would drop, the Unix
socket would become inert, and `gxctl connect` / the browser WS
bridge would fail with `Connection refused` — with no way to see
what the kernel printed before dying.

### Shutdown on `release`

The VM controller sends `release` over `shim.sock` once it has recorded
the exit ([reconciliation.md §9.1](reconciliation.md#91-reconcile) step
3); a SIGTERM to the shim (unit stop) releases by itself once the
hypervisor is gone. `release` stops the proxy — which first drains what
is still buffered on the master into the log, so the last output before
a stop is kept — unlinks `console.sock`, `api.sock` and `shim.sock`, and
exits. The logs and `instance.json` stay until the VM is deleted.

The log is the guest's raw byte stream and need not be valid UTF-8;
code that reads it as text (launch errors, tests) decodes it lossily.

## Log files

[reconciliation.md §8.6](reconciliation.md#86-console-log-d14) has the
rules; in short:

- Path: `<run dir>/vms/<id>/console.log`, previous generation
  `console.log.1`. Mode `0600`.
- **Appended across instances, never truncated** (D14). Each launch
  writes `--- glidex: instance <id> started <RFC 3339> ---`, each exit
  `--- glidex: instance <id> exited: <cause>[ (status n | signal n)] ---`,
  so the output before a crash survives the crash-restart.
- **Rotated once**: before a write that would take the file past
  `console.log_max_bytes` (16 MiB by default), it is renamed to
  `console.log.1` (replacing it) and a new one is opened. Writes are
  whole reads, so nothing is split; at most 2 × the limit per VM (tmpfs).
- Replay sends the current file only. Because it spans earlier
  instances, expect-style clients should skip it up to the current
  instance's `started` line. `GET /vms/{id}/console/log?previous=true`
  returns `console.log.1` (`404` if there is none).
- Replay leaves out the guest's **terminal queries**: DSR (`CSI … n`,
  which includes the cursor-position request `CSI 6 n`), DA (`CSI … c`),
  DECRQM (`CSI … $ p`), XTVERSION (`CSI > … q`), the kitty keyboard
  query (`CSI ? u`), XTWINOPS reports (`CSI 11/13-16/18-21 t`),
  XTGETTCAP / DECRQSS (`DCS + q` / `DCS $ q`) and OSC colour and
  clipboard queries (OSC 4, 5, 10-19, 52 with a `?` field). Why: the
  client's terminal answers what it is replayed, and the guest reads
  those stale answers as typed input. systemd probes a serial
  console's size and name at each getty start, so every new browser
  tab would type `^[[38;1R` at the prompt. A query cut off at the end
  of the file is passed on (the guest is waiting for its answer); live
  output and the log file itself are unchanged.
- Removed with the VM's runtime directory when the VM is deleted, not
  at stop. Lost on host reboot (`/run`).

## Console Unix socket

- Path: `<run dir>/vms/<id>/console.sock`
- Type: `SOCK_STREAM` Unix domain socket.
- Bound by the shim, in the VM's `0700` runtime directory; reached only
  by the control plane.
- Multi-client: the proxy thread `accept`s any number of clients
  and broadcasts every byte of output to all of them. Input from
  any client is written to the PTY (so clients can fight for the
  keyboard — accepted trade-off; there's no locking).
- Protocol: **raw byte stream** in both directions. No framing, no
  handshake. Anything fancier would need to be invented on top.

## Clients

Only the control plane connects to the console Unix socket, from
`service/vm.rs`:

- `console_target` checks access: the caller must be able to read the
  VM (else `404`), and a browser session (PAM or OIDC login) must redeem
  a fresh single-use ticket for this VM (else `403 ticket_required`).
  Tokens and peers on the API socket need none. It answers `Local(<run
  dir>/vms/<id>/console.sock)`, or `Remote{node, vm}` when the VM runs on
  another node.
- `console_connect` opens the target: `UnixStream::connect` for a local
  one; for a remote one an upgraded connection to
  `/cluster/v1/vms/{id}/console` on the VM's node, whose
  `cluster/server.rs::relay_console` connects to its own console socket
  and copies bytes both ways ([clustering.md](clustering.md)).

Over that byte stream:

- **Browsers**: the WebSocket bridge, `api/vms.rs::console_ws` and
  `bridge_stream` (below).
- **gxctl and other API clients**: the bidi RPC `VmService.Console`
  (`api/rpc/vm.rs`; [grpc.md](grpc.md)). The first request must be
  `{open: {id, ticket?}}` within 30 s; later `input` messages are
  written to the console and output comes back as `output` messages.
  `gxctl connect` uses it (stdin in raw mode, `Ctrl+]` to detach;
  [cli.md](cli.md)). gRPC-Web can't stream both ways, so browsers stay
  on the WebSocket.
- **The log**: `GET /vms/{id}/console/log` (`GetConsoleLog`) reads
  `console.log` (or `console.log.1` with `previous`) itself, not the
  socket, relayed through the VM's node in a cluster
  (`/cluster/v1/vms/{id}/console-log`).

## Browser bridge

`GET /vms/{id}/console/ws?ticket=…` in the control plane
(`api/vms.rs::console_ws`):

1. `console_target` (above): access and the ticket.
2. A remote console is connected **before** the upgrade, so an
   unreachable node is a `502 node_unreachable` answer. A local one is
   connected **after** it; since the shim keeps its listener after the
   guest exits (until `release`), that rarely fails, and a failure is
   sent in the stream: one `Message::Text` with the error, then `Close`.
3. `bridge_stream` copies both ways in a `tokio::select!` loop: up to
   4 KiB read from the console → one `Message::Binary`; `Binary` or
   `Text` frames from the browser → written as bytes. Close, an error or
   the end of either side ends it.

glidex-ui doesn't look inside the WebSocket: `/api/*` upgrades are
passed to the control plane over `ui.sock` (or TCP) and the two
upgraded connections are spliced with `copy_bidirectional`
([web-ui.md](web-ui.md)).

On the UI side, `crates/glidex-ui/ui/src/pages/VmConsole.tsx` with
`consoleTerminal.ts`:

- **Terminal**: ghostty-web (Ghostty's terminal engine as WebAssembly,
  the default) or xterm.js, chosen on the page and remembered in the
  browser (`localStorage`). Each library is loaded only when chosen;
  ghostty-web's `.wasm` comes from `/assets` (the UI's CSP allows
  `'wasm-unsafe-eval'`, not `data:` URLs).
- **Ticket, then connect**: `CreateConsoleTicket` (`POST
  /vms/{id}/console/ticket` over REST; valid 30 s, single use), then at
  once `ws(s)://<location.host>/api/vms/{id}/console/ws?ticket=<ticket>`
  with `binaryType = "arraybuffer"`.
- **WS → terminal**: binary frames as bytes; a text frame (the
  connect-failure message) as text.
- **Terminal → WS**: `onData`, UTF-8 encoded, as a binary frame.
- **Status** `connecting | connected | closed | error`; on `closed` or
  `error` a **Reconnect** button runs ticket and connect again (the VM
  outlives a connection: restarts of the VM or the control plane close
  it). A failure on a VM whose node stopped reporting says so.
- **Size**: the terminal is fitted to the page on window resize; the
  size is not sent anywhere (below).
- On unmount it detaches the socket's handlers, closes it and disposes
  the terminal (xterm.js only after its open-time timer has run).

### Dev-server proxying

Vite's dev server must forward WebSocket upgrades for this to work.
See `vite.config.ts` — the `/api` proxy is configured with
`ws: true` (to `ui.sock` or the control plane's TCP port).

## Known gaps

- **Resize**: the terminal size is not propagated. The browser fits
  the terminal to the page, but neither the WebSocket nor the RPC
  carries a size, and the shim never sets one on the PTY (`openpty`
  without a size). A serial console has no size of its own: the guest
  uses its default, or what it learns by querying the terminal. A future
  `Message::Text` sub-protocol (and an RPC message) could carry it.
- **No input locking**: every client's input goes to the guest (see
  [Console Unix socket](#console-unix-socket)).
- **One console per VM**: no second serial port or virtio console, and
  no choice between them beyond what the boot mode decides
  ([Physical I/O](#physical-io)).
