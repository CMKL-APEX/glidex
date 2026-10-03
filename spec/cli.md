# `gxctl` CLI

Source: `crates/glidex-control-plane/src/bin/gxctl/` (`main.rs`: REPL
and VM commands; `client.rs`: transport, token file, error rendering;
`console.rs`: `connect` / `log`; `admin.rs`: access-control commands).
Binary produced by the same crate as the control plane.

`gxctl` is an **interactive** shell. Invoking it drops into a
`rustyline` REPL with history and tab completion. Commands talk to
the control plane's REST API (see "Transport" below).

## Transport and authentication

See [security.md](security.md) §5.1, §5.2, §5.5.

| Option | Effect |
|---|---|
| `--socket <path>` / `GLIDEX_SOCKET` | HTTP over this Unix socket. |
| *(default)* | The first existing socket of `/run/glidex-cp/api.sock` (systemd unit), `$XDG_RUNTIME_DIR/glidex/api.sock`, `/tmp/glidex-<euid>/api.sock` (a control plane started by hand, `paths::run_dir`). |
| `--url http(s)://host:port[/prefix]` (`-s`, alias `--server`) | TCP instead; wins over `--socket`. Plain `http://` is refused unless the host is loopback, so a token never crosses the network in clear text. |
| *(no socket, no `--url`)* | TCP to `http://localhost:8841`. |
| `--project <id\|name>` (`-p`) / `GLIDEX_PROJECT` | Project for this session (below). |
| `gxctl [options] <command…>` | Run one REPL command and exit (`gxctl list`, `gxctl --url https://cp:8841 login --oidc`). |

- **Unix socket:** the control plane identifies the caller by peer uid
  (`SO_PEERCRED`); gxctl sends no credentials. Root and `glidex-admin`
  members are break-glass administrators, `glidex-users` members are
  ordinary users, anyone else gets `403`.
- **TCP:** gxctl sends `Authorization: Bearer <token>` with the token
  from `GLIDEX_TOKEN`, else `~/.config/glidex/token` (`$XDG_CONFIG_HOME`
  honoured). The file is written by `login` (directory `0700`, file
  `0600`, written to a temporary file and renamed). gxctl refuses to
  read a token file that group or others can access, or that another
  user owns, and says so. The token is never printed (except once by
  `token create`), logged, put in a URL or shown by `Debug`; the header
  is marked sensitive.
- **TLS** (`https://`): rustls with the `ring` provider, trusting the
  system store (`rustls-native-certs`) plus `GLIDEX_CA_CERT` (a PEM
  file, for a self-signed control-plane certificate). HTTP/1.1 only.
- **Implementation:** `ApiClient` (`client.rs`) opens one connection per
  request (`tokio::net::UnixStream`, `TcpStream`, or `tokio-rustls` over
  it) and runs a hyper 1 `client::conn::http1` handshake on it. Every
  call goes through `ApiClient::send` / `request` / `request_json`; the
  console WebSocket reuses `ApiClient::connect`.

### Errors

API errors (`{"error": code, "message": …, "details": …}`) are shown as
`<code>: <message>`, plus `details` where present (impact and `--force`
hint, minimum disk size, missing host features, quota overruns, policy
validation errors). Special cases:

| Response | Shown as |
|---|---|
| `401 reauth_required` | `log in again: <message>` (step-up, security.md §7.3) |
| `401 unauthenticated` on TCP | `not logged in: … (run gxctl login --oidc / --token, or set GLIDEX_TOKEN)` |
| `403 *` | `permission denied (<code>): <message>` |
| non-JSON body | `HTTP <status>: <body>` |

### Projects

`--project` (or `project use` for a session where the server can't store
a default) scopes requests:

- create bodies get `"project"`: `POST /vms`, `POST /disks`,
  `POST /credentials`;
- lists get `?project=`: `GET /vms`, `/disks`, `/credentials`, and so do
  `GET/PUT/DELETE /credentials/{username}`;
- VM name lookups (`resolve_vm`) search the scoped list.

Without it the server uses the caller's default project (`PATCH
/users/me {"default_project"}`, set by `project use <p>`) or, if they
have exactly one, that one. `list` and `get` show each VM's project by
name (ids when `/projects` isn't readable).

## Command reference

| Command | Effect |
|---|---|
| `list` / `ls` | `GET /vms` + pretty-print |
| `get <name\|id>` | `GET /vms/{id}` with VM-name resolution |
| `create` | Interactive prompts → `POST /vms` |
| `start <name\|id>` | `POST /vms/{id}/start` |
| `stop <name\|id> [--graceful [secs]]` | `POST /vms/{id}/stop[?graceful_timeout_secs=<secs>]` (power button first; default 60 s) |
| `pause <name\|id>` | `POST /vms/{id}/pause` |
| `connect <name\|id>` | `GET /vms/{id}/console`, then the console WebSocket `GET /vms/{id}/console/ws` |
| `log <name\|id>` | `GET /vms/{id}/console/log` (last 1 MiB) |
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
| `networks` / `network list` | `GET /networks` (project networks show their project) |
| `network create <name> [--project P] [--subnet CIDR] [--mtu N]` | With `--project`: `POST /projects/{P}/networks` (NAT, generated bridge); without: as `network-add` |
| `network rm <name>` | `DELETE /networks/{name}` |
| `network share <net> <project-id>` | `POST /networks/{net}/shares {project}` (offer, valid 7 days) |
| `network unshare <net> <project-id>` | `DELETE /networks/{net}/shares/{project}` |
| `network shares <project>` | `GET /projects/{p}/network-shares` (offered / accepted) |
| `network accept <project> <net>` | `POST /projects/{p}/network-shares/{net}/accept` |
| `network leave <project> <net>` | `DELETE /projects/{p}/network-shares/{net}` |
| `health` | `GET /health` |
| `help` / `?` | Command list |
| `exit` | Leave the REPL |

### Access control

| Command | Effect |
|---|---|
| `whoami` | `GET /auth/whoami`: user, method, token, teams, break-glass, host roles, project roles (with the principal they come through), default project |
| `login --oidc` | TCP only. `POST /auth/oidc/device` → prints the verification URI and user code → polls `POST /auth/oidc/device/poll {device_code, token_name: "gxctl"}` every `interval` s (`202` pending, `429` slow down: +5 s) until it returns `{token}` or `expires_in` passes; the token is checked with `whoami` and saved |
| `login --token` | TCP only. Reads a token from stdin with echo off, checks it with `whoami`, saves it |
| `login` on the Unix socket | Prints that no login is needed |
| `logout [--revoke]` | Deletes the token file (`--revoke`: `DELETE /tokens/{id}` for the current token first) |
| `ui` | Prints the web UI URL (`GLIDEX_UI_URL`, default `http://localhost:5173`) |
| `token list` | `GET /tokens` |
| `token create <name> [--days N] [--service-account [--project P]] [--role ROLE[@PROJECT]]…` | `POST /tokens {name, expires_in_days, kind, project, roles}`; prints the secret once with a warning |
| `token revoke <id>` | `DELETE /tokens/{id}` |
| `project list` / `show <p>` | `GET /projects`, `GET /projects/{p}` (quotas and usage) |
| `project create <name> [--description D]` | `POST /projects` |
| `project delete <p>` | Confirmation → `DELETE /projects/{p}` |
| `project quota <p> [key=value…]` | Without edits: show. With: `GET` the current quotas, apply the edits (`vms`, `vcpus`, `memory_mib`, `disk_gib`, `running_vms`, `networks`; `none` = unlimited) and `PATCH /projects/{p} {"quotas": …}` (the server replaces the whole set) |
| `project use <p>` | `PATCH /users/me {"default_project": p}` (`-`/`none` clears). Where the server has no profile (service-account token, auth disabled) the project is used for this session only |
| `binding list <p>` | `GET /projects/{p}/bindings` |
| `binding add <p> <role> <principal>` | `POST /projects/{p}/bindings {role, principal}` |
| `binding remove <p> <binding-id>` | `DELETE /projects/{p}/bindings/{id}` |
| `system-binding list` / `add <role> <principal>` / `remove <id>` | `GET/POST/DELETE /system/bindings` |
| `user list` | `GET /users` (with identities) |
| `team list` / `create <name>` / `delete <team>` | `GET/POST /teams`, `DELETE /teams/{id}` |
| `team add-member <team> <user-id>` / `remove-member …` | `PUT/DELETE /teams/{id}/members/{user}` (team by id or name) |
| `policy list` / `show <id>` | `GET /authz/policies`, `GET /authz/policies/{id}` |
| `policy put <id> <file> [--disable] [--description D]` | `GET` the current `version` (`0` if new; base/role/file policies are refused), then `PUT /authz/policies/{id} {text, description, enabled, version}` |
| `policy delete <id>` | Confirmation → `DELETE /authz/policies/{id}?version=<current>` |
| `policy validate <id> <file>` | `POST /authz/validate {id, text}` |
| `policy history <id>` | `GET /authz/policies/{id}/versions` |
| `policy reload` | `POST /authz/reload` |
| `audit [--project P] [--since <unix-ms>] [--limit N] [--user U]` | `GET /audit?…` (`--project` defaults to the session project) |

Roles are written as `owner` or `role.owner` (a name with a `.` is
taken as is, e.g. `grant.host-paths`). Principals are `user:<id>`,
`team:<id>` (team ids may contain `:`, e.g. `team:unix:glidex-admin`) or
`token:<id>`, sent as the API's entity JSON `{"type": "User", "id": …}`.

### Argument parsing

The REPL line is split into words with single and double quotes and
backslash escapes (`project create lab --description "Lab VMs"`).

### Tab completion

Both the REPL and the path prompts use a rustyline `Editor` with
`GxHelper` (`CompletionType::List`: Tab completes the common prefix, a
second Tab lists candidates). `CompletionMode` picks the behavior:

- **`Command`** (REPL line): the first word completes against
  `COMMANDS` (aliases included; keep it in sync with `handle_words`).
  The second word completes against `SUBCOMMANDS` for commands that have
  them (`image`, `disk`, `ovs`, `login`, `token`, `project`, `binding`,
  `system-binding`, `user`, `team`, `policy`, `network`, …). Later words
  complete as file paths only where `PATH_ARGS` says the command takes
  one (`attach-device <vm> <path>`, `detach-device <vm> <path>`, `policy
  put|validate <id> <file>`); other arguments get no candidates.
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
5. UEFI firmware path. Defaults to the hypervisor's
   `default_firmware_path()` (`~/.glidex/CLOUDHV.fd`, downloaded by
   `glidex-install`, or the host's OVMF for QEMU) when that file exists,
   otherwise no default; `none` selects kernel boot.
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
   *(auto-generated seed only)* Login credential: lists the project's
   stored usernames; empty → none, and the VM has no way to log in. With
   no credentials in the project gxctl says "no login available" and
   doesn't ask.
9. Kernel args (optional — server picks per-hypervisor default) —
   skipped for firmware boot.
10. Optional VFIO PCI devices, comma-separated sysfs paths.

The request is `POST /vms`. Tilde in paths is expanded server-side
(see [data-model.md](data-model.md)); the CLI does not do it itself.


### `connect` loop

The VM's console socket is in its private `0700` run directory
(`paths::vm_dir`), so gxctl goes through the API like the browser does:

1. `GET /vms/{id}/console` → `{available, websocket}`; a stopped VM is
   refused before anything else.
2. `ApiClient::connect` opens the same kind of stream as any request
   (Unix socket, TCP or TLS) and `tokio_tungstenite::client_async` does
   the WebSocket handshake on it for `GET /vms/{id}/console/ws`, with the
   bearer header on TCP. Local peers and token holders need no console
   ticket (tickets are for browser sessions, security.md §5.6), and no
   `Origin` is sent. A refused handshake shows the API error.
3. stdin goes into raw mode (termios: no `ICANON`, `ECHO`, `ISIG`, so
   Ctrl+C reaches the guest). A thread `poll`s fd 0 every 100 ms and
   `read`s it directly, forwarding chunks over a channel; it stops at
   `0x1D` (`Ctrl+]`), sending what came before it.
4. The async loop writes binary (and text) frames to stdout and sends
   stdin chunks as binary frames, until `Ctrl+]` ("Detached") or the
   server closes ("Console closed").
5. The terminal is restored and a Close frame sent.

The console proxy behind the WebSocket supports many clients, so
several `gxctl connect` sessions and browser sessions can share a VM.
They all see the same output and share one input stream (no locking).

### `log` command

`GET /vms/{id}/console/log` (needs `vm.console`) returns the last 1 MiB
of the captured console output, printed as is. Not a follow; to
live-tail, use `connect`.
