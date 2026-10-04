# Console subsystem

> Access: the console socket and log live in the VM's private runtime
> directory (`0700`); clients reach them only through the API
> (`/vms/{id}/console/ws`, `/vms/{id}/console/log`, both `vm.console`).

Each running VM's serial console is owned by its `glidex-vm-shim`
([reconciliation.md §8](reconciliation.md#8-instances)), not by the
control plane, so it keeps being captured while the control plane is
down or restarting. It is:

1. **Captured** to an append-only, rotated log file on disk.
2. **Broadcast** to any number of concurrently-connected clients
   over a per-VM Unix socket (`<run dir>/vms/<id>/console.sock`).
3. **Bridged** to browsers via a WebSocket endpoint on the control
   plane.

This document explains the invariants that make that work and why
the code is structured the way it is.

## Physical I/O

Both hypervisors are wired the same way (`glidex-vm-shim`,
`supervisor.rs::spawn`): the shim allocates a `pty(7)` pair (both ends
close-on-exec), puts the slave in raw mode, passes it as the
hypervisor's stdin and stdout, puts the hypervisor's stderr on a pipe,
and keeps the master. QEMU gets `-serial stdio`; Cloud-Hypervisor gets
`--console tty` / `--serial tty` — the virtio console (`hvc0`) for
kernel boots, the serial port (`ttyS0`) for firmware boots, see
[hypervisors.md](hypervisors.md#firmware-boot).

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
   late-joining browsers the pre-boot output.
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

One first-party client connects to the console Unix socket:

- **Control plane's WebSocket bridge** (`api/vms.rs::bridge_console`)
  — the only client of the raw socket. Browsers and `gxctl connect`
  (stdin in raw mode, `Ctrl+]` to detach; [cli.md](cli.md)) both go
  through it.

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
