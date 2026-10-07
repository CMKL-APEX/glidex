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
| *(no socket, no `--url`)* | TCP to `https://localhost:8841`. |
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
  system store (`rustls-native-certs`), plus `GLIDEX_CA_CERT` (a PEM
  file: a copy of a remote control plane's self-signed certificate),
  plus the local control plane's published certificate
  (`/run/glidex-cp/tls.crt`, or `tls.crt` in a hand-started one's run
  directory) when it exists, so `https://localhost` works with no
  setup. There is no option to skip verification; an unverified
  certificate is an error that names `GLIDEX_CA_CERT` and the
  fingerprint the server sent. HTTP/1.1 only.
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
| `list` / `ls` | `GET /vms` + table; `STATE` shows `state → desired_state` while they differ |
| `get <name\|id>` | `GET /vms/{id}` with VM-name resolution; also prints the `Ready` reason while not converged (`Waiting:`) and `last_exit` |
| `create` | Interactive prompts → `POST /vms` |
| `start <name\|id> [--no-wait]` | `POST /vms/{id}/start?wait=60` |
| `stop <name\|id> [--graceful [secs]] [--no-wait]` | `POST /vms/{id}/stop?wait=<w>[&graceful_timeout_secs=<secs>]` (power button first; default 60 s); `w` = max(60, secs + 20), at most 300 |
| `pause <name\|id> [--no-wait]` | `POST /vms/{id}/pause?wait=60` |
| `events <name\|id>` | `GET /vms/{id}/events`: time, actor, reason (warnings highlighted), message |
| `watch [vms,disks,images,networks]` | `GET /watch?kinds=…` (default all): follow changes until Ctrl-C (below) |
| `connect <name\|id>` | `GET /vms/{id}/console`, then the console WebSocket `GET /vms/{id}/console/ws` |
| `log <name\|id>` | `GET /vms/{id}/console/log` (last 1 MiB) |
| `delete <name\|id> [--keep-disk] [--no-wait]` | Confirmation prompt → `DELETE /vms/{id}?wait=60[&keep_disk=true]` |
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
| `image retry <name\|id>` | `POST /images/{id}/retry`: download a failed image again (follow it with `image list`) |
| `image rm <image>` | `DELETE /images/{id}?wait=60`; prints `Image deleted:` once gone, else `Deleting:` with the reason it waits |
| `disk list` / `disks` | `GET /disks`, attached VM shown by name |
| `disk show <disk>` | `GET /disks/{id}`: path, backing file, partitions |
| `disk create <name> [--size-gib N] [--image I] [--full] [--raw] [--no-extend]` | `POST /disks?wait=300` (it may wait for its image to download) |
| `disk resize <disk> <GiB> [--no-extend]` | `POST /disks/{id}/resize?wait=120`; a refused shrink prints the minimum |
| `disk extend-root <disk> [--on-boot]` | `POST /disks/{id}/extend-root?wait=120` |
| `disk rm <disk>` | Confirmation prompt → `DELETE /disks/{id}`; prints `Deleting:` instead of `Disk deleted:` when the server deferred it (`202`, an operation on the disk finishes first) |
| `networks` / `network list` | `GET /networks` (project networks show their project) |
| `network create <name> [--project P [--isolated] [--vhost-user]] [--subnet CIDR] [--mtu N]` | With `--project`: `POST /projects/{P}/networks` (NAT, or isolated with `--isolated`; tap ports, or vhost-user with `--vhost-user`; generated bridge); without: as `network-add` |
| `network rm <name>` / `network-rm` / `net-rm` | `DELETE /networks/{name}?wait=60`; prints `Network deleted:` once gone, else `Deleting: <name> (the control plane finishes it)` and `Waiting: <Ready reason>: <message>` (e.g. `NetdUnavailable`, `InUse`) |
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
| `ui` | Prints the web UI URL (`GLIDEX_UI_URL`, default `https://localhost:5173`) |
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
| `usage bandwidth\|disk-io\|compute [--month YYYY-MM] [--by k,…] [--project P] [--csv]` | `GET /usage/bandwidth` / `/usage/disk-io` / `/usage/compute`: average, 30-second peak and 95th percentile per group |
| `stats <vm>` | `GET /vms/{id}/stats`: current CPU, memory, NIC and disk rates |
| `bandwidth <vm> \| --network N`, `io <vm> \| --disk D`, `compute <vm>` `[--from D] [--to D]` | The 5-minute series and its 95th percentile |
| `usage [--project P] [--from D] [--to D] [--by k,…] [--granularity hour\|day\|month] [--meters m,…] [--tz Z] [--csv]` | `GET /usage?…`: this billing month by project unless told otherwise; prints each bucket's meters in presentation units (core-hours, GiB, Mbps…), or the raw CSV with `--csv` ([metering.md §12](metering.md#12-cli-and-ui)) |

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

### Lifecycle commands wait

`start`, `stop`, `pause` and `delete` change the VM's desired state
([reconciliation.md §12.3](reconciliation.md#123-waitsecs-d20)), so by
default they send `?wait` and print the VM as the controller left it:
`Success: VM <name> is now <state>`, plus `Not there yet: <Ready
reason>: <message>` when the wait ended before the VM converged (a
`202`: e.g. still starting, crash-loop backoff, disk busy). A failed
reconcile comes back as the usual API error (`Error: …`), as the
synchronous calls of earlier releases did. `--no-wait` returns at once
with `state → desired_state`; `get` or `events` shows how it went.

`disk create`, `resize` and `extend-root` wait the same way
([images.md §8](images.md#8-rest-api)) and print the disk's size,
format and status, plus `Not there yet: <Ready reason>: <message>` when
it has not converged (e.g. `ImageNotReady` while its image downloads,
`ResizePending` / `ExtendRootPending` while a VM has it open). They have
no `--no-wait`. `events` covers VMs only; disk, image and network events
are on the API (`GET /{disks,images,networks}/{id}/events`).

### `watch`

`watch [kinds]` opens `GET /watch` ([rest-api.md](rest-api.md#live-stream-get-watch))
and prints `Synced: N objects; changes follow`, then one line per
change: `HH:MM:SS  <kind> <name> <what>`, where `what` is a VM's state
(`state → desired` while they differ), a disk's status, an image's state
(`downloading N%` while it downloads) or a network's phase, prefixed
`deleting (…)` while it is being deleted and followed by
` — <Ready reason>: <message>` while `Ready` isn't `True`. A removed
object prints `deleted` in red. When the server ends the stream (after
5 minutes, or a restart) it reconnects and prints only what really
changed meanwhile. Ctrl-C stops it.

`state → desired` (`format_vm_state`) is printed whenever the two
differ, treating `created` as `stopped`; `failed` and `unknown` are
shown in bold red.

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
5. UEFI firmware: a firmware image built for the hypervisor
   ([images.md](images.md#41-firmware-catalog)), defaulting to the newest
   ready one (sent as `firmware`); `none` selects kernel boot; an
   absolute path is sent as `firmware_path` (needs `useHostPath`). With no
   firmware image it says which to pull (`image pull --firmware
   cloudhv-edk2` or `ovmf`) and defaults to `none`.
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
   stored usernames; empty → none, and the VM has no way to log in. If
   the project has a credential named after the signed-in user, that is
   the default instead (`none` for no login). With
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

1. `GET /vms/{id}/console` → `{available, websocket}`; a VM with no
   instance is refused before anything else. A VM whose guest has
   exited but whose shim is not yet released still connects and
   replays its log.
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
live-tail, use `connect`. The log spans the VM's earlier instances,
separated by `--- glidex: instance … ---` lines; the rotated
`console.log.1` (`?previous=true`) has no gxctl command yet.
