# The console

Every VM has one console. You can type into it from the browser
or from `gxctl`, as many clients at once as you like, and everything it
prints is logged, so you can read what happened before you connected.
Opening a console or reading its log needs the `operator` role in the
VM's project.

What the console shows depends on the guest: a cloud image booted
through UEFI firmware prints the firmware, the boot loader and the
kernel there, then a login prompt; with [kernel boot](kernel-boot.md)
it starts at the kernel. Log in with the VM's
[guest credential](credentials.md).

Which guest device the console is depends on how the VM boots, the same
on Cloud Hypervisor and QEMU:

| Boot | Console device in the guest |
|---|---|
| UEFI firmware | The first serial port, `ttyS0`. |
| Kernel boot, last `console=` in the kernel arguments is `hvc…` | The virtio console, `hvc0`. The VM then has no serial port. |
| Kernel boot, any other last `console=`, or none | The first serial port, `ttyS0`. |

The default kernel arguments, and kernels extracted from cloud images,
say `console=ttyS0`. A Cloud Hypervisor VM created with kernel boot by
an earlier version of Glidex keeps the `console=hvc0` it was created
with. There is no second console: to move the console, change the VM's
`kernel_args` (see [The console](kernel-boot.md#the-console)); it
applies at the next start.

## In the browser

On the VM's page, click **Open Console**. The console opens in the page
and connects at once; the **Status** at the top says **connecting**,
**connected**, **closed** or **error**. Type into it as into a
terminal: input goes straight to the guest's console device.

When you connect, the console first replays what the VM printed so far
(its current log), then follows live output. The replay leaves out the
questions the guest asked its terminal earlier (such as where the
cursor is), so opening the console doesn't type stale answers like
`^[[38;1R` at the guest's prompt.

If the connection closes (the VM stopped, or the control plane
restarted), click **Reconnect**. **Back to VM** leaves the console;
closing the page detaches without affecting the VM.

### Terminal

The **Terminal:** menu at the top picks the terminal the page draws
the console with:

- **ghostty-web** (the default): Ghostty's terminal engine, run in the
  browser. It draws on a canvas, so screen readers can't read the
  console's text.
- **xterm.js**: keeps the text in the page, so it works with screen
  readers.

The choice is remembered in this browser. Switching reconnects the
console, with a fresh replay of the log.

### Copy and paste

| Keys | What they do |
|---|---|
| **Ctrl+Shift+C** | Copy the selection. |
| **Ctrl+Shift+V** | Paste into the console. |
| **Ctrl+C** | Sent to the guest as `^C`, as in any terminal. |

On macOS, **⌘C** and **⌘V** copy and paste too. ghostty-web also copies
a selection as soon as you make it.

### Terminal size

The terminal fills the page and follows the window's size, but the
size isn't passed to the guest: a serial console has none. Programs in
the guest use their default (often 80×24) or what they find out by
asking the terminal. If a full-screen program draws in only part of the
window, set the size in the guest, for example `stty rows 40 cols 120`,
or run `resize` if the guest has it.

## With gxctl

```
gxctl> connect my-vm
Info: Connected to the VM console via ...
Tip: Press Ctrl+] to detach from console
```

`console` and `attach` are aliases of `connect`. Press **Ctrl+]** to
detach; the VM keeps running. If the console closes from the other end
(the VM stopped), `gxctl` says **Console closed**.

`connect` works over the local socket on the host and, after
`gxctl auth login`, from another machine over HTTPS (see
[Logging in](logging-in.md)). On a cluster it reaches VMs on any node.

`connect` refuses when the VM has no running instance: start it first.

## Several clients

Any number of clients can be connected to one VM at the same time:
browser tabs, `gxctl` sessions, or both. They all see the same output,
and what any of them types goes to the same console. A client that
falls far behind (several MiB of output it hasn't read) is disconnected;
it can reconnect and replay the log.

## The console log

Everything the guest prints on the console is written to a log on the
VM's host, from the first firmware output on, whether or not anyone is
connected. Glidex adds a line when each run of the VM starts and ends,
for example:

```
--- glidex: instance <id> started 2026-10-17T09:12:03Z ---
...
--- glidex: instance <id> exited: clean_exit ---
```

The log is kept across stops and starts, and when the control plane
restarts. When it reaches its size limit (16 MiB by default; an
administrator can change it), it is rotated: the previous log is kept,
and older output is dropped.

Read the log without connecting:

```
gxctl> log my-vm              # the last 1 MiB of the current log
```

With the API, `GET /vms/{id}/console/log` returns the raw bytes: the last
1 MiB by default, `?tail_bytes=<n>` for less, and `?previous=true` for
the rotated log. The web UI has no log view of its own; opening the
console replays the current log.

## After the guest exits

When the guest powers off or the hypervisor exits, the exit line is
logged and live output stops. Until the control plane notices the
exit, a client can still connect and replay the log; then the console
closes, `connect` says the VM is not running, and the browser console
reports an error. The log stays readable with `log` or the API until
the VM is deleted.

## API

Programs reach the console in two ways:

- The RPC `glidex.vm.v1.VmService.Console`, a stream both ways: the
  first message opens the VM's console, later ones carry input, and the
  answers carry output. This is what `gxctl connect` uses. See
  [RPC API](../api/rpc.md).
- A WebSocket at `GET /vms/{id}/console/ws`, which the web UI uses:
  binary messages carry the console's bytes in both directions.
  Browsers first get a single-use ticket
  (`POST /vms/{id}/console/ticket`, valid for 30 seconds) and pass it
  as `?ticket=`. See [Console WebSocket](../api/rest.md#console-websocket).

Clients with an API token or on the local socket need no ticket.
Neither way carries a terminal size.
