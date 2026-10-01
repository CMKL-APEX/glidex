# `gxctl` CLI

Source: `crates/glidex-control-plane/src/bin/gxctl.rs`. Binary
produced by the same crate as the control plane.

`gxctl` is an **interactive** shell. Invoking it drops into a
`rustyline` REPL with history and tab completion. Commands talk to
the control plane over HTTP (default `http://localhost:8841`, override
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
| `delete <name\|id> [--keep-disk]` | Confirmation prompt → `DELETE /vms/{id}[?keep_disk=true]` |
| `pci` / `pci-devices` | `GET /pci-devices` + table |
| `attach-device <vm> <path>` | `POST /vms/{id}/devices` |
| `detach-device <vm> <path>` | `DELETE /vms/{id}/devices` |
| `credentials` / `creds` | `GET /credentials` + table (no hashes) |
| `credential-add` | Username, hidden password ×2, `.pub` files → `POST /credentials` |
| `credential-passwd <user>` | Hidden password ×2 → `PUT /credentials/{user}` |
| `credential-keys <user>` | `.pub` files → `PUT /credentials/{user}` (replaces keys) |
| `credential-rm <user>` | `DELETE /credentials/{user}` |
| `image catalog` | `GET /images/catalog` |
| `image list` / `images` | `GET /images` |
| `image pull <key\|url> [--name N] [--sha256 H]` | `POST /images`, then polls `GET /images/{id}` with a progress line until ready or failed (Ctrl-C stops watching only) |
| `image rm <image>` | `DELETE /images/{id}` |
| `disk list` / `disks` | `GET /disks`, attached VM shown by name |
| `disk show <disk>` | `GET /disks/{id}`: path, backing file, partitions |
| `disk create <name> [--size-gib N] [--image I] [--full] [--raw] [--no-extend]` | `POST /disks` |
| `disk resize <disk> <GiB> [--no-extend]` | `POST /disks/{id}/resize`; a refused shrink prints the minimum |
| `disk extend-root <disk> [--on-boot]` | `POST /disks/{id}/extend-root` |
| `disk rm <disk>` | Confirmation prompt → `DELETE /disks/{id}` |
| `health` | `GET /health` |
| `help` / `?` | Command list |
| `exit` | Leave the REPL |

### Tab completion

Both the REPL and the path prompts use a rustyline `Editor` with
`GxHelper` (`CompletionType::List`: Tab completes the common prefix, a
second Tab lists candidates). `CompletionMode` picks the behavior:

- **`Command`** (REPL line): the first word completes against
  `COMMANDS` (aliases included; keep it in sync with `handle_command`).
  Later words complete as file paths only where `PATH_ARGS` says the
  command takes one (`attach-device <vm> <path>`, `detach-device <vm>
  <path>`); other arguments get no candidates.
- **`Path`** (`prompt_path` / `prompt_path_optional`): firmware, kernel,
  disk image, cloud-init seed, VFIO device and SSH key file prompts.
  Completion applies to the segment after the last comma, since commas
  aren't a rustyline word break and several prompts take lists.

Rustyline's `FilenameCompleter` expands `~` and escapes spaces
(`my\ dir/`); `prompt_path` unescapes the answer so callers get the
real path. When stdin isn't a terminal, `prompt_path` falls back to
plain `read_line`, so piped input keeps working. Ctrl-C/Ctrl-D at a path
prompt answer it with an empty line.

### Credential input

Passwords are read with terminal echo off (`prompt_hidden`, termios
`ECHO` cleared; plain `read_line` when stdin isn't a TTY) and asked twice.
SSH keys are read client-side from `.pub` files; a file containing
`PRIVATE KEY` is refused before anything is sent, and the error never
echoes file contents. Request structs carrying a password don't derive
`Debug`.

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
7. Boot disk. For firmware boot: `image` / `disk` / `path`, defaulting to
   `image` when a downloaded image is ready. `image` asks for the image
   (first ready one by default) and a root disk size in GiB (default from
   the server, 10); `disk` asks for an existing disk. `path` (and kernel
   boot) asks for a disk image or rootfs path, as before. Then optional
   data disks, comma-separated names.
8. *(firmware boot only)* cloud-init seed image (optional; empty →
   auto-generated at start, see [hypervisors.md](hypervisors.md#firmware-boot)).
   *(auto-generated seed only)* Login credential: lists stored usernames;
   empty → host SSH keys / `GLIDEX_CLOUD_INIT_PASSWD_HASH` fallback.
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
