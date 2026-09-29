# `gxctl` CLI

Source: `crates/glidex-control-plane/src/bin/gxctl.rs`. Binary
produced by the same crate as the control plane.

`gxctl` is an **interactive** shell. Invoking it drops into a
`rustyline` REPL with history and tab completion. Commands talk to
the control plane over HTTP (default `http://localhost:8080`, override
with `--server`).

## Command reference

| Command | Effect |
|---|---|
| `list` / `ls` | `GET /vms` + pretty-print |
| `get <name\|id>` | `GET /vms/{id}` with VM-name resolution |
| `create` | Interactive prompts → `POST /vms` |
| `start <name\|id>` | `POST /vms/{id}/start` |
| `stop <name\|id>` | `POST /vms/{id}/stop` |
| `pause <name\|id>` | `POST /vms/{id}/pause` |
| `connect <name\|id>` | Attach local terminal to the VM's console socket |
| `log <name\|id>` | `tail`-like print of the VM's log file |
| `delete <name\|id>` | Confirmation prompt → `DELETE /vms/{id}` |
| `pci` / `pci-devices` | `GET /pci-devices` + table |
| `attach-device <vm> <path>` | `POST /vms/{id}/devices` |
| `detach-device <vm> <path>` | `DELETE /vms/{id}/devices` |
| `health` | `GET /health` |
| `help` / `?` | Command list |
| `exit` | Leave the REPL |

### Name vs id resolution

Every command that takes a `<name|id>` argument goes through
`CliClient::resolve_vm`. It first tries an exact id match by asking
`GET /vms/{arg}`; on 404 it falls back to `GET /vms` and searches for
a unique `name == arg` match. Ambiguous or missing names produce a
clear error before any mutation is attempted.

### `create` prompts

Interactive `handle_create` asks, in order:

1. VM name (required, unique).
2. vCPU count (default 1).
3. Memory in MiB (default 512).
4. Hypervisor choice: `cloudhypervisor | qemu`, default
   `cloudhypervisor`. Aliases: `ch`, `q`. Asked first
   because it decides which of the following prompts appear.
5. *(Cloud-Hypervisor only)* UEFI firmware path. Defaults to
   `default_firmware_path()` (`~/.glidex/CLOUDHV.fd`, downloaded by
   `glidex-install`) when that file exists, otherwise no default;
   `none` selects kernel boot.
6. Kernel image path (required, no default) — skipped for firmware
   boot.
7. Rootfs path (required, no default). For firmware boot the prompt
   asks for a UEFI-bootable disk image instead, since a bare ext4
   rootfs has no bootloader.
8. *(firmware boot only)* cloud-init seed image (optional; empty →
   auto-generated at start, see [hypervisors.md](hypervisors.md#firmware-boot)).
9. Kernel args (optional — server picks per-hypervisor default) —
   skipped for firmware boot.
10. Optional VFIO PCI devices, comma-separated sysfs paths.

The request is `POST /vms`. Tilde in paths is expanded server-side
(see [data-model.md](data-model.md)); the CLI does not do it itself.

### `connect` loop

This is the most intricate command. `gxctl` calls
`GET /vms/{id}/console` to get the `console_socket_path`, then:

1. `UnixStream::connect` to that socket.
2. Clone the stream for a reader thread that copies
   socket→stdout byte-for-byte.
3. Put stdin into raw mode via termios.
4. Main loop reads stdin bytes; if the user hits `0x1D` (`Ctrl+]`)
   the loop exits; otherwise bytes are written to the socket.
5. On exit, termios is restored, the reader thread is signaled,
   and the socket closes.

Because the console Unix socket supports many concurrent clients,
multiple `gxctl connect` sessions on the same VM can coexist, and
so can a browser WS session. They all see the same output; they
all share the same input stream (no locking).

### `log` command

Opens `log_path` (from `GET /vms/{id}/console`) and prints it.
Not a follow; just a dump. To live-tail, use `connect`.

## HTTP client

`CliClient` in the same file wraps `reqwest::Client` and exposes
one method per API endpoint. Error handling parses `ApiError`
bodies and re-renders them as `"<error_code>: <message>"`.

The CLI does **not** open the console WebSocket; that's exclusive
to the browser UI. From `gxctl`, console attach is always local
via the Unix socket.

## Non-REPL usage

`gxctl` always runs as a REPL. It does not accept commands on
argv; there is no `gxctl start my-vm` single-shot. If that's
needed in the future it's a straightforward `clap` subcommand
tree layered on top of the existing `dispatch_command` function.
