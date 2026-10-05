# REST API

The control plane serves HTTPS on every address, `0.0.0.0:8841` and
`[::]:8841`, by default (`listen` / `GLIDEX_LISTEN` override; a
self-signed certificate unless one is configured; plain HTTP only on
loopback — [security.md](security.md) §5.1), and on its Unix sockets.
All request and
response bodies are JSON except for the console WebSocket.

## Authentication and authorization

Every endpoint except `/health`, `/auth/methods`, `/auth/login` and the
OIDC endpoints needs a principal: a local user on `api.sock` (peer uid),
a bearer token, or a browser session (cookie + `X-Glidex-CSRF` on
writes). Each route maps to one Cedar action; see
[security.md](security.md) §7 and §13 for the actions, the additional
endpoints (projects, bindings, tokens, users, teams, policies, audit) and
the error codes `401 unauthenticated`, `401 reauth_required`,
`403 forbidden`, `403 quota_exceeded`, `409 would_lock_out` and
`422 invalid_policy`.

VMs, disks and guest credentials belong to a project: responses carry
`project`, create bodies accept `project` (id or name; default: the
caller's default project), and lists and `/credentials/{username}`
accept `?project=`. Resources in projects the caller can't read are
reported as `404`.

## Endpoints

| Method | Path | Handler | Purpose |
|---|---|---|---|
| `GET` | `/health` | `health_check` | Liveness probe |
| `GET` | `/vms` | `list` | List VMs |
| `POST` | `/vms` | `create` | Create a new VM (desired state `stopped` unless `power` says otherwise) |
| `GET` | `/vms/{id}[?view=full]` | `get_one` | Get a VM by id; `view=full` returns the stored `{meta, spec, status}` record instead of the flat `VmResponse`; it also needs `readSystemStatus` (`host.read`), since the record carries host paths, PIDs and the boot id |
| `PATCH` | `/vms/{id}` | `patch_one` | JSON merge patch of the VM's spec (below) |
| `DELETE` | `/vms/{id}[?keep_disk=true]` | `delete_one` | Request deletion; the controller stops the VM and cleans up (below) |
| `POST` | `/vms/{id}/start` | `start` | Desired state `running` (from `paused`: resume) |
| `POST` | `/vms/{id}/stop[?graceful_timeout_secs=N]` | `stop` | Desired state `stopped`; `N` (max 300) is stored as `stop_grace_secs`: press the guest's power button and wait up to `N` s before stopping it hard |
| `POST` | `/vms/{id}/pause` | `pause` | Desired state `paused` |
| `GET` | `/vms/{id}/events` | `events` | The VM's last 50 events, oldest first |
| `GET` | `/vms/{id}/console` | `console_info` | Console availability and WebSocket URL |
| `POST` | `/vms/{id}/console/ticket` | `console_ticket` | Single-use ticket for a browser's console WebSocket (security.md §5.6) |
| `GET` | `/vms/{id}/console/ws` | `console_ws` | WebSocket upgrade — see below |
| `GET` | `/vms/{id}/console/log[?tail_bytes=N][&previous=true]` | `console_log` | Last `N` bytes (max and default 1 MiB) of the console log; `previous=true` reads the rotated `console.log.1` (`404` if none) |
| `POST` | `/vms/{id}/devices` | `attach_device` | Add a VFIO PCI device to the spec (hot-plugged while running) |
| `DELETE` | `/vms/{id}/devices` | `detach_device` | Remove a VFIO PCI device from the spec (hot-unplugged while running) |
| `GET` | `/system/reconcile` | `system_reconcile` | Controller status: runner, bus, netd, queue, orphans, unknown VMs |
| `GET` | `/pci-devices` | `list_pci_devices` | Enumerate host PCI devices |
| `GET` | `/credentials` | `list_credentials` | List stored guest logins (no hashes) |
| `POST` | `/credentials` | `create_credential` | Create a login; password is hashed on arrival |
| `GET` | `/credentials/{username}` | `get_credential` | Get one login (no hash) |
| `PUT` | `/credentials/{username}` | `update_credential` | Change password and/or replace SSH keys |
| `DELETE` | `/credentials/{username}` | `delete_credential` | Delete; `409` while a VM uses it |
| `GET` | `/images/catalog` | `image_catalog` | Built-in cloud images for this host's arch |
| `GET` / `POST` | `/images` | `list_images` / `pull_image` | List; download + verify (`202`) |
| `GET` / `DELETE` | `/images/{id}[?wait=N]` | `get_image` / `delete_image` | Details + progress; delete or cancel (`204`, or `202` while the image controller finishes; `409` while a disk uses it; below) |
| `POST` | `/images/{id}/retry` | `retry_image` | Download a failed image again (`202`; needs `pullImage`) |
| `GET` | `/images/{id}/events` | `image_events` | The image's last 50 events (`readImage`) |
| `GET` / `POST` | `/disks[?wait=N]` | `list_disks` / `create_disk` | List; create blank or from an image (`201`, made by the disk controller; below) |
| `GET` / `DELETE` | `/disks/{id}` | `get_disk` / `delete_disk` | Details + partition table; delete (`204`, or `202` while an operation on it finishes; `409` while attached) |
| `POST` | `/disks/{id}/resize[?wait=N]` | `resize_disk` | Grow (extends root) or shrink (never into a partition); `202`, applied by the disk controller |
| `POST` | `/disks/{id}/extend-root[?wait=N]` | `extend_root` | Grow the root partition, `offline` or `on-boot`; `202`, applied by the disk controller |
| `GET` | `/disks/{id}/events` | `disk_events` | The disk's last 50 events (`readDisk`) |
| `GET` | `/networks/{name}/events` | `network_events` | The network's last 50 events (`readNetwork`) |
| `DELETE` | `/networks/{name}[?wait=N]` | `delete_network` | Delete (`204`, or `202` while the network controller tears it down; `409` while a VM uses it; below) |
| `GET` | `/watch[?kinds=…][&project=…]` | `watch` | Live stream of VM, disk, image and network changes (server-sent events; below) |
| `POST` | `/vms/{id}/disks` | `attach_disk` | Attach a data disk; on a running VM it takes effect at the next start (`restart_required`) |
| `DELETE` | `/vms/{id}/disks/{disk}` | `detach_disk` | Detach a data disk; on a running VM at the next start, and the disk stays claimed until then |
| `GET` | `/usage[?project=&from=&to=&granularity=&group_by=&meters=&tz=&format=csv]` | `usage::usage` | Usage records ([metering.md §9.1](metering.md#91-get-usage)): hour/day/month buckets, grouped by project, VM, disk, NIC or network; host readers see every project, others their own (`404` for one they can't read) |
| `GET` | `/projects/{id}/usage`, `/vms/{id}/usage[?include=disks]`, `/disks/{id}/usage` | `usage::*_usage` | The same, for one project, VM (and its NICs) or disk |
| `GET` | `/usage/bandwidth`, `/usage/disk-io` `[?project=&month=YYYY-MM&group_by=&format=csv]` | `rates::bandwidth` / `disk_io` | Per billing month: average, 30-second peak and 5-minute 95th percentile (billable: network max(in, out), disk read+write) ([metering.md §9.3](metering.md#93-bandwidth)) |
| `GET` | `/vms/{id}/bandwidth`, `/networks/{name}/bandwidth`, `/vms/{id}/io`, `/disks/{id}/io` `[?from=&to=&p95=true]` | `rates::*` | 5-minute series for graphs (default: the last 24 h; at most 31 days) |
| `GET` | `/vms/{id}/stats`, `/networks/{name}/stats` | `rates::*_stats` | Current rates from the last two samples (CPU %, memory, Mbps, pps, IOPS, MB/s); not stored |

Image and disk payloads, semantics and invariants are in
[images.md](images.md#8-rest-api).

Handlers live in `crates/glidex-control-plane/src/api/` (VM routes in
`api/vms.rs`; the route table with each route's Cedar action in
`api/mod.rs`).

## Desired state and `?wait`

VM writes record **what the user wants** and return; the VM controller
makes it so ([reconciliation.md §12](reconciliation.md#12-api)).
Admission — validation, authorization, quotas, disk/device claims,
name uniqueness — is still checked when the spec is written and still
fails the request (D5). Host actions (launching, stopping, hot-plug,
VM ports) happen in the controller; their failures show in the VM's
`state`, `conditions` and events.

Every VM write (create, start, stop, pause, `PATCH`, delete, devices,
disks) accepts `?wait=<secs>` (capped at 300; gxctl and the UI send
60):

| Outcome | Response |
|---|---|
| no `wait` | `202` with the VM (`201` for a create) |
| converged (`observed_generation ≥` the written generation and `Ready=True`) | `200` with the VM (`201` for a create) |
| the reconcile of that generation failed (`state: failed`) | the error envelope the call used to return synchronously, chosen from the `Ready` reason, with `details.reason` and the VM in `details.vm` (D20) |
| timeout, including while waiting on a dependency | `202` with the VM |

Failure mapping (`api/errors.rs::failed_reconcile_response`):
`NetdUnavailable` → `503 netd_unavailable`; `DiskBusy` → `409 conflict`;
`DiskMissing`, `DiskNotReady` → `500 image_error`; a missing hypervisor
binary → `503 hypervisor_unavailable`; anything else (`LaunchFailed`,
`ProvisioningFailed`, `HypervisorError`, …) → `500 hypervisor_error`.
The spec stays as written and the controller keeps retrying with
backoff, so a later `GET` may show the VM running.

**Invariant.** A VM's `state` is what the controller last observed, and
`desired_state` what was asked for; a client waits for `Ready=True` at
the current generation, never for `state` alone (a VM that should be
stopped is also "ready" once stopped).

**`If-Match: <resource_version>`** on start, stop, pause and `PATCH`
(the other writes ignore it): `412 precondition_failed` (with
`details.resource_version`) when the VM has been written since. Without
it, the last writer wins. `VmResponse` carries `resource_version`; it
grows on every write, spec or status.

**Deletion** (`DELETE /vms/{id}`): `204` when nothing ever ran (no
instance, no ports: the record is gone at once); otherwise `202` with
the VM (`deleting: true`) while the controller stops the instance and
works off its finalizers, or with `?wait`, `200` once the record is
gone. A deleting VM refuses other writes with `409 conflict`.

### Disks

Disk writes are admitted synchronously (name, size, shrink minimum,
quota, claims: the same `400`/`403`/`409` as before) and carried out by
the disk controller ([images.md §6](images.md#6-disk-operations)).
`POST /disks`, `/disks/{id}/resize` and `/disks/{id}/extend-root` accept
`?wait=<secs>` (capped at 300):

| Outcome | `POST /disks` | resize, extend-root |
|---|---|---|
| no `wait` | `201` with the disk (`phase: pending`) | `202` with the disk |
| settled: made, or the change applied or reported pending | `201` | `200` |
| `Ready` reason `InvalidDisk` / `ResizeInvalid` | `400 invalid_disk` | `400 invalid_disk` |
| phase `failed` or `missing` | `500 image_error` | `500 image_error` |
| timeout (e.g. its image still downloading) | `202` with the disk | `202` with the disk |

A resize or extend-root of a disk a live instance has open settles at
once as pending (`Ready=False/ResizePending` or `ExtendRootPending`) and
is applied once the VM stops. `DELETE /disks/{id}` returns `204` once
the record is gone (normally at once), or `202` with the disk
(`deleting: true`) while an operation on it finishes first.

`DiskResponse` fields besides the record's (`id`, `name`, `project`,
`format`, `size_bytes`, `origin`, `attached_to`, `pending_growpart`,
`path`, `created_at`):

| Field | Meaning |
|---|---|
| `phase` | `pending`, `creating`, `ready`, `resizing`, `missing`, `failed` |
| `status` | `phase`, with `busy` while an operation runs and `missing` when the file is gone (kept for older clients) |
| `conditions` | `Ready` with reasons `Converged`, `Progressing`, `ImageNotReady`, `ResizePending`, `ResizeInvalid`, `ExtendRootPending`, `InvalidDisk`, `ToolUnavailable`, `IoError`, `FileMissing`, `Deleting` |
| `pending_size_bytes` | a resize not applied yet |
| `owner` | the VM the disk was made for (deleted with it unless `keep_disk`) |
| `deleting` | `true` while a deletion is in progress |
| `busy_op` | the operation running on it |
| `info`, `partition_table` | single-disk `GET` of a ready disk only |
| `extend_root`, `warnings` | the outcome of an operation, when the response carries one |

### Images and networks

`DELETE /images/{id}` and `DELETE /networks/{name}` are admitted
synchronously (`409 conflict` while a disk uses the image as its
backing file, waits for it or clones from it; while any VM uses the
network) and record `deletion_requested_at`; the image or network
controller finishes the deletion. Both accept `?wait=<secs>`:

| Outcome | Response |
|---|---|
| gone at once (the normal case for an image) | `204` |
| no `wait`, still deleting | `202` with the image (`deleting: true`) or network (`deletion_requested_at`, `phase`, `conditions`) |
| gone within `wait` | `200` |
| timeout (e.g. netd unreachable) | `202` as above |

While deleting, an image can't be the source of a new disk or be
retried, and a network can't be attached to; recreating either under
the same name is `409 conflict` until it is gone. Details:
[images.md §8](images.md#8-rest-api), [networking.md](networking.md).

Image, disk and network events (`GET /{images,disks}/{id}/events`,
`GET /networks/{name}/events`) have the shape of the VM's below.

## Live stream: `GET /watch`

`GET /watch[?kinds=vms,disks,images,networks][&project=<id>]` answers
`text/event-stream` (server-sent events). Any authenticated caller may
open it; each kind is filtered exactly like its list endpoint (VMs by
`readVm`, disks by `readDisk`, both in visible projects and narrowed by
`project`; images need `readImage` and networks `readNetwork` on
`Host`, with the view of `GET /networks`). `kinds` takes singular or
plural names (default: all four); an unknown kind is `400 invalid`.

| Event | Data |
|---|---|
| `added` | `{"kind": "vm\|disk\|image\|network", "id": "…", "object": {…}}`: one per visible object on connect, then for each new one |
| `synced` | `{}`, once, after the initial `added` events |
| `modified` | as `added`; `object` is exactly what the list endpoint returns |
| `deleted` | `{"kind", "id"}` (no `object`): removed, or no longer visible |
| `expired` | `{}`: the stream ends; reconnect for a fresh snapshot |

A network's `id` is its name. Every store write rings a change bell;
each stream then re-lists (changes within 250 ms go out together, and it
re-lists every 10 s regardless) and sends the differences, so
authorization, including policy changes, applies to every event. A
keep-alive comment goes out every 15 s. A stream lasts at most
5 minutes, then sends `expired` and closes (EventSource reconnects by
itself); it also ends once the caller's access can no longer be resolved
(e.g. the user was disabled). At most 64 streams are open host-wide;
beyond that the call is `503 too_many_watchers` (poll instead).

## Payloads

### `POST /vms`

```json
{
  "name": "my-vm",
  "vcpu_count": 2,
  "mem_size_mib": 1024,
  "kernel_image_path": "/path/to/vmlinux",
  "rootfs_path": "/path/to/rootfs.ext4",
  "kernel_args": "console=ttyS0 root=/dev/vda reboot=k panic=1",
  "hypervisor": "qemu",
  "vfio_devices": ["/sys/bus/pci/devices/0000:41:00.0"]
}
```

- `kernel_args`, `hypervisor`, `vfio_devices` are optional.
- `firmware` boots the disk through UEFI firmware instead of a kernel:
  a firmware image (id or name, [images.md](images.md#7-vm-integration))
  built for the VM's `hypervisor`, e.g.
  `{"hypervisor": "cloudhypervisor", "firmware": "cloudhv-edk2",
  "image": "ubuntu-26.04", ...}`. With `image` or `root_disk` and no
  kernel, the newest ready firmware image for the hypervisor is used
  (`400 invalid_config` if there is none). QEMU keeps a private copy of
  the image's variable store per VM (see
  [hypervisors.md](hypervisors.md#firmware-boot-1)). `firmware_path`
  instead names a host file (needs `useHostPath`; not with `firmware`);
  relative paths resolve against the control plane's working directory,
  so prefer absolute or `~` paths. `kernel_image_path` and `kernel_args`
  may then be omitted; they are ignored if given. The
  console is attached to the guest's serial port (`ttyS0`), which is where
  distro cloud images put their login prompt.
- `cloud_init_path` attaches a NoCloud seed image
  read-only as the second disk. When omitted on a firmware boot, glidex
  generates one at `<run dir>/vms/<id>/cloudinit.img` on every start
  (needs `mkdosfs`/`mcopy` from dosfstools/mtools): hostname from the VM
  name, DHCP on `en*`, and a sudo user `cloud` with no SSH keys
  and a locked password: without `credential` the VM has no login.
  A credential's password hash is applied via `chpasswd`, so it also
  works on a rootfs that was provisioned before.
- `~` is expanded server-side (see [data-model.md](data-model.md)).
- `power` (`"stopped"` by default, `"running"`, `"paused"`) is the
  desired state; with `"running"` one call creates and starts the VM.
  `restart_policy` (`"on_failure"` default, `"never"`), `on_host_boot`
  (`"resume"` / `"stop"`, default from `reconcile.on_host_boot`) and
  `stop_grace_secs` (0–300, default 0 = hard stop) are optional.
- Paths that reach the hypervisor command line must not contain `"` or
  control characters, and with the systemd runner must not be under
  `/tmp` or `/var/tmp` (`400 invalid_config`).
- Response: `201 Created` with a `VmResponse` (see `?wait` above).

- `credential` (firmware boot only, no custom `cloud_init_path`) names a
  stored credential that the generated seed provisions as the guest login.
  Unknown names are `400 invalid_config`. See [credentials.md](credentials.md).

- Managed disks ([images.md](images.md#7-vm-integration)): instead of
  `rootfs_path`, give `image` (+ optional `root_disk_size_gib`) to have a
  root disk created for the VM, or `root_disk` to boot an existing disk.
  Exactly one of the three is required. `data_disks` lists extra disks.
  Without a kernel, these imply firmware boot through the default
  firmware image. The response may carry `warnings` and, for firmware
  boot, `firmware` (the image id).
- `DELETE /vms/{id}?keep_disk=true` keeps a root disk that was created for
  the VM; by default it is deleted with it.

### `VmResponse`

```json
{ "id": "…", "name": "web-1", "project": "…",
  "state": "running", "desired_state": "running",
  "generation": 4, "observed_generation": 4, "resource_version": 17, "restart_required": false,
  "conditions": [ { "kind": "Ready", "status": "True", "reason": "Converged", "message": "", "last_transition_at": 1791000000 } ],
  "last_exit": { "at": 1790990000, "instance_id": "…", "cause": "crashed", "signal": 9 },
  "restart_policy": "on_failure", "on_host_boot": "resume", "stop_grace_secs": 0,
  "vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": "cloudhypervisor", "…": "…" }
```

`state` is one of `created`, `starting`, `running`, `paused`,
`stopping`, `stopped`, `failed`, `unknown`
([data-model.md](data-model.md#powerstate-vmphase-and-the-derived-vmstate));
`desired_state` one of `running`, `paused`, `stopped`. `conditions` and
their reasons are the closed catalogue of
[reconciliation.md §7.5](reconciliation.md#75-exit-handling);
`last_exit.cause` is `requested`, `terminated`, `clean_exit`,
`crashed`, `launch_failed`, `host_reboot` or `lost`. `deleting: true`
appears while a deletion is in progress.

### `PATCH /vms/{id}`

A JSON merge patch of the spec:

```json
{ "power": "running", "stop_grace_secs": 30,
  "config": { "mem_size_mib": 4096, "data_disks": ["data-1"], "credential": null } }
```

Top level: `power`, `restart_policy`, `on_host_boot`,
`stop_grace_secs`. `config`: `vcpu_count`, `mem_size_mib`,
`kernel_args`, `credential` (`null` removes it), `hugepages`,
`vfio_devices`, `data_disks` (ids or names; replaces the list),
`networks`. Unknown fields are `422`; immutable ones
(`hypervisor`, `kernel_image_path`, `firmware_path`, `firmware`, `rootfs_path`,
`cloud_init_path`, `root_disk`, `image`) are refused with `400
invalid_config` ("immutable field(s): …") before anything is authorized.
When each change takes effect is in
[data-model.md](data-model.md) (mutability); changes that need a new
launch set `restart_required` while the VM runs. Each changed field is
authorized separately (security.md §7.4). vCPU and memory increases are
checked against the project quota like a create.

### `GET /vms/{id}/events`

```json
{ "events": [ { "at": 1791000520, "actor": "guest", "kind": "warning",
                "reason": "CleanExit", "message": "guest powered off; desired state set to stopped" } ] }
```

`actor` is `api:<principal>`, `controller`, `guest`, `systemd` or
`host`; `kind` is `normal` or `warning`. Needs `readVm`.

### `GET /system/reconcile`

```json
{ "runner": "systemd", "bus": "connected", "netd": "connected",
  "queue": { "pending": 0, "in_flight": 1 },
  "orphans": [ { "kind": "unit", "id": "glidex-vm@<uuid>.service" },
               { "kind": "runtime_dir", "id": "<uuid>" } ],
  "unknown_vms": ["<uuid>"] }
```

`bus` is `n/a` under the detached runner; `netd` is `connected`,
`status_only` or `unavailable`. Orphans are found at startup and never
touched (reconciliation.md §9.4). Needs `readSystemStatus` on
`Host::"local"`.

### `POST /credentials`, `PUT /credentials/{username}`

```json
{ "username": "alice", "password": "…", "ssh_authorized_keys": ["ssh-ed25519 AAAA… alice@host"] }
```

- `POST` needs a password, at least one key, or both. `PUT` takes the
  same body without `username`; omitted fields are unchanged and an empty
  key list clears the keys.
- Responses are `CredentialInfo`:
  `{ "username", "has_password", "ssh_authorized_keys", "created_at", "updated_at" }`.
  The password hash is never returned.

### `POST /vms/{id}/devices`, `DELETE /vms/{id}/devices`

```json
{ "device_path": "/sys/bus/pci/devices/0000:41:00.0" }
```

Both edit `config.vfio_devices` (as a `PATCH` would) and are accepted
in any state; attaching a device already in the list, or detaching one
that isn't, is `400 invalid_state`, and a device another VM claims is
`409 conflict`. The controller hot-plugs or unplugs it while the guest
is observed running; while it is paused the change waits
(`DevicesPending=True/GuestPaused`); a stopped VM gets it at the next
launch. A hot-plug the hypervisor refuses shows as
`DevicesPending=True/HotplugFailed` (a `?wait` then times out with
`202`), with the spec left as written.

### `GET /vms/{id}/console`

```json
{
  "vm_id": "…",
  "websocket": "/vms/<id>/console/ws",
  "available": true
}
```

`available` is `true` while an instance exists, including after the
guest exited and until the controller releases the shim, so the last
output of a crashed guest stays reachable. gxctl checks it before
opening the WebSocket.

## Error model

All non-2xx responses are:

```json
{ "error": "<code>", "message": "<human readable>" }
```

Status code mapping (`api::error_to_response`):

**Invariant.** Request validation in admission (zero vCPUs or memory,
missing kernel *and* firmware, bad paths, immutable fields) returns
`HypervisorError::InvalidConfig`, which maps to `400`. Failures that are
not the caller's fault — e.g. `mkdosfs`/`mcopy` missing when the seed is
built — happen in the controller and reach the caller only through
`?wait` (`500`).

| `VmManagerError` variant | HTTP | `error` code |
|---|---|---|
| `VmNotFound` | `404` | `not_found` |
| `VmAlreadyExists` | `409` | `conflict` |
| `InvalidState` | `400` | `invalid_state` |
| `HypervisorError(InvalidConfig)` | `400` | `invalid_config` |
| `HypervisorError` (any other) | `500` | `hypervisor_error` |
| `PersistenceError` | `500` | `persistence_error` |
| `HypervisorNotAvailable` | `503` | `hypervisor_unavailable` |
| `Credential(NotFound)` | `404` | `not_found` |
| `Credential(AlreadyExists)`, `CredentialInUse` | `409` | `conflict` |
| `Credential(Invalid)` | `400` | `invalid_credential` |
| `Credential` (storage/hashing) | `500` | `credential_error` |
| `Deleting` (write to a VM being deleted) | `409` | `conflict` |
| `PreconditionFailed` (`If-Match`) | `412` | `precondition_failed` |
| `QuotaExceeded` | `403` | `quota_exceeded` |
| `Image(…)` | `400` / `404` / `409` / `500` / `503` | `invalid_image`, `invalid_disk`, `not_found`, `conflict`, `image_error`, `tool_unavailable`; see [images.md](images.md#8-rest-api) |

## Console WebSocket

### Protocol

`GET /vms/{id}/console/ws` upgrades to a WebSocket. The handler
`console_ws → bridge_console`:

1. Looks up the VM. If `VmNotFound`, responds `404`. Any other
   lookup error responds `500`.
2. Opens a `tokio::net::UnixStream` to the VM's console socket (in its
   private runtime directory; never exposed to clients).
   If the connect fails, sends a `Message::Text` containing the
   error string and then `Message::Close`.
3. Enters a `select!` loop:
   - Bytes read from the Unix socket → sent as `Message::Binary` to
     the browser.
   - `Message::Binary` / `Message::Text` from the browser → written
     to the Unix socket. Close / error / `None` from the browser
     ends the loop.

### Client expectations

- Use `binaryType = "arraybuffer"` on the `WebSocket`. The server
  only ever sends binary frames (except the very first frame in
  the connect-failed case, which is text).
- Writes are sent as binary; the server accepts either binary or
  text (text is treated as the UTF-8 byte sequence of its content).
- No framing — this is a byte stream of terminal data in both
  directions. The *only* transport semantics are WebSocket
  boundaries, which are irrelevant to the consumer.

### Replay-on-connect behavior

The console Unix socket is listened on by a proxy thread in the VM's
`glidex-vm-shim`. That thread replays the current console log (which
spans earlier instances of the VM, separated by `--- glidex: instance …
started … ---` lines) to every newly-accepted client before starting
live broadcast. So opening a WebSocket on a VM that has already
booted will immediately flush the boot-time output into your xterm.
See [console.md](console.md).
