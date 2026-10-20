# REST API

## Conventions

- **Base URL**: `https://host.example.org:8841`, with no path prefix
  (`https://host.example.org:8841/vms`). Locally, the same paths over the
  Unix socket `/run/glidex-cp/api.sock`.
- **JSON** in and out (`Content-Type: application/json`), except the
  console WebSocket, the console log (raw bytes), CSV exports, the
  cluster snapshot and `/watch` (server-sent events).
- **Ids and names**: VM paths take the VM's id. Disks, images,
  datastores, networks, projects and nodes take an id or a name (for nodes, a live
  node's name before a departed one's), tenants an id or a slug; a name
  that several visible resources share is `409 ambiguous`. `gxctl`
  resolves VM names for you.
- **Projects**: VMs, disks, guest credentials and project networks
  belong to a project. Creates take `project` (id or name) and default to
  your default project; lists take `?project=`.
- **Status codes**: `200` OK, `201` created, `202` accepted (the
  controller is still working on it), `204` no content (gone).

### Proto JSON

Every operation with an RPC twin takes and answers that RPC's message as
proto3 JSON. What that means for a client:

| Rule | Example |
|---|---|
| Field names are `snake_case`, as in the proto | `mem_size_mib`, `root_disk_size_gib` |
| Responses are wrapped in the message's fields | `{"vms": [...]}`, `{"vm": {...}}`, `{"vm": {...}, "settled": true}` |
| 64-bit integers are JSON strings | `"size_bytes": "21474836480"` |
| Enum values are full proto names, in requests too | `"hypervisor": "HYPERVISOR_QEMU"`, `"mode": "NETWORK_MODE_NAT"` |
| Times are RFC 3339 | `"created_at": "2026-10-17T08:00:00Z"` |
| Unset or default fields are left out, not `null` | an unlimited quota is absent |
| A body that isn't the message is refused | `422 invalid`: an unknown field, a wrong type, an unknown enum value |

Lowercase enum values (`"qemu"`, `"nat"`, `"running"`) are not accepted:
they are `422 invalid`. Path and query parameters fill the message's
fields of the same meaning (`?wait=60` is `wait_secs`, `If-Match` is
`expected_resource_version`).

Partial updates (`PATCH`) change the fields present, or those listed in
`update_mask`, a comma-separated string (`"description,quotas"`). A
VM's `PATCH` puts its fields under `change`:

```json
{
  "change": {"power": "POWER_STATE_RUNNING", "mem_size_mib": 4096},
  "update_mask": "power,mem_size_mib"
}
```

Without `update_mask`, the fields present in `change` are the ones
changed. `credential` in the mask but absent from `change` removes the
VM's credential.

Plain JSON bodies of their own: `/health`, `/auth/methods`,
`/auth/login`, `/auth/logout`, the OIDC and connector redirects, and
`/system/reconcile`. The [SCIM endpoints](#scim-20-for-identity-providers)
speak SCIM 2.0 JSON instead.

### Authentication

| Listener | Send |
|---|---|
| HTTPS `:8841` | `Authorization: Bearer <token>` |
| `api.sock` | Nothing; you are your Unix user |

Without a credential, protected endpoints answer `401 unauthenticated`.
A request from a browser `Origin` the site doesn't allow is
`403 origin_not_allowed` on writes and WebSocket upgrades; through the
web UI, the names the UI serves are allowed. See
[API overview](README.md#authentication).

### Errors

Errors are a `glidex.api.v1.Error`: a stable `error` code, a human
`message` and, for some codes, `details`. RPC errors carry the same
code ([RPC errors](rpc.md#errors)).

```json
{
  "error": "quota_exceeded",
  "message": "vms quota exceeded: limit 10, in use 10, requested 1",
  "details": {"quota": [{"resource": "vms", "limit": 10, "used": 10, "requested": 1}]}
}
```

Branch on `error`, never on `message`. Common codes:

| Status | `error` | Meaning |
|---|---|---|
| 400 | `bad_request`, `invalid_request`, `invalid_config`, `invalid_disk`, `invalid_image`, `invalid_datastore`, `invalid_network`, `invalid_credential`, `invalid_project`, `invalid_state`, `project_required`, `range_too_large` | The request is malformed or not valid here |
| 400 | `device_flow_unsupported` | A device login through a `google` connector that has no device client |
| 401 | `unauthenticated` | No credential, or an invalid or expired token |
| 401 | `login_failed` | Wrong username or password |
| 401 | `reauth_required` | The operation needs a recent login; log in again |
| 403 | `forbidden` | Not allowed by your roles and the site's policies |
| 403 | `quota_exceeded`, `tenant_quota_exceeded` | Over a project quota, or the tenant's cap; `details.quota` lists what |
| 403 | `not_admitted`, `tenant_suspended` | The project's tenant isn't admitted on this site, or is suspended |
| 403 | `login_not_allowed`, `tls_required`, `ticket_required`, `origin_not_allowed`, `csrf_failed` | Login or browser rules refused the request |
| 404 | `not_found` | No such resource, or one you can't read |
| 409 | `conflict` | A name already taken, a resource in use or being deleted, a concurrent change |
| 409 | `ambiguous` | A name matches several resources; use the id |
| 409 | `cannot_delete_self`, `last_admin`, `last_identity` | Deleting yourself, the last holder of `system-admin` or `directory-admin`, or a user's last sign-in method |
| 409 | `node_departed` | A per-host role on a node that has left the cluster |
| 409 | `managed_by_connector` | The user or team is managed by a SCIM connector or a directory sync: change it at the IdP (see [Users an IdP manages](../operator-guide/access-control.md#users-an-idp-manages)) |
| 409 | `not_provisioned` | A SCIM token for a connector that isn't `provisioning: scim` |
| 409 | `not_pulled`, `no_source`, `sync_running` | A directory sync of a connector that isn't `provisioning: pull`, of a kind Glidex can't pull from (only Google Workspace), or whose run is already going |
| 409 | `would_lock_out`, `confirmation_required`, `host_interface_in_use`, `not_clustered`, `ovn_not_installed`, `dpdk_not_running`, … | A precondition isn't met; the message says which |
| 412 | `precondition_failed` | `If-Match` didn't match; `details.resource_version` has the current one |
| 413 | `too_large` | The body is too large |
| 507 | `insufficient_storage` | A datastore without overcommit lacks the space for a new or moved disk; `details` has `free_bytes` and `needed_bytes` |
| 421 | `not_leader`, `use_a_server` | Ask a cluster server (clients retry another) |
| 422 | `invalid` | The body isn't the expected message (unknown field, wrong type, unknown enum value) |
| 422 | `invalid_policy`, `invalid_connector`, `unsupported_on_host` | Rejected policy text or connector settings; a feature this host can't do |
| 429 | `rate_limited`, `slow_down` | Back off and retry |
| 502 | `netd_error`, `node_unreachable`, `idp_error` | A helper, node or identity provider failed |
| 503 | `cluster_unavailable`, `netd_unavailable`, `hypervisor_unavailable`, `tool_unavailable`, `too_many_watchers`, `unavailable` | Try again later, or fix the host |
| 500 | `internal`, `persistence_error`, `hypervisor_error`, `image_error` | Server-side failure; quote `x-request-id` |

### Waiting for the controller

VM and disk writes, and image and network deletes, record what you
want and return; controllers carry it out. By default such a write
answers at once: `202` with the resource (`201` for a create, `204` for
a delete with nothing left to do). Add `?wait=<secs>` (at most
300) to hold the answer until the controller has acted:

| Outcome | Answer |
|---|---|
| Done in time | `200` (`201` for a create) with `"settled": true` |
| The time ran out | `202` with `"settled": false` |
| The controller failed | The error it hit, e.g. `500 hypervisor_error`, with the VM in `details.vm` |
| The resource is gone (a delete) | `204` |

`?wait` applies to VM create, start, stop, pause, `PATCH`, delete,
device and disk attach/detach; disk create, resize, move and extend-root;
image delete; network delete. `gxctl` waits 60 s by default.

A VM's `state` is what the controller last saw and `desired_state`
what you asked for. Wait for its `Ready` condition to be `True`, not for
a `state`.

Start, stop, pause and `PATCH` of a VM take `If-Match: <resource_version>`:
the write is refused with `412 precondition_failed` if the VM changed
since you read it.

### Lists and paging

Lists return everything you may see in one answer, except:

- `GET /tenants`: `page_size`, `page_token`; the answer has
  `next_page_token`.
- `GET /audit`: `since`, `limit` (default 500, at most 5000),
  `project`, `user`.

### Live changes: `GET /watch`

A stream of server-sent events (`text/event-stream`) with the VMs,
disks, images and networks you may list:

- `?kinds=vms,disks,images,networks` (the default); also `nodes`,
  `imports`, `cluster`, `policy`, `datastores`.
- `?project=<p>` limits VMs and disks to one project.

The stream starts with an `added` event per object, then `synced`;
after that `added`, `modified` and `deleted` as things change. Each
event's data is `{"kind": "vm", "id": "…", "object": {…}}`, with the
object as its list answers it (no `object` on `deleted`). Authorization
is re-checked for every event. A comment line every 15 s keeps a quiet
stream open through proxies. The server ends a stream after 5 minutes
with `expired`; reconnect for a fresh snapshot (a browser `EventSource`
does this by itself). Too many open streams on a server give
`503 too_many_watchers`: poll instead. `WatchService.Watch` is the same
stream over RPC ([Streams](rpc.md#streams)).

### Console WebSocket

1. `GET /vms/{id}/console` says whether the console is available and
   gives the WebSocket path.
2. A browser with a session cookie first gets a ticket,
   `POST /vms/{id}/console/ticket`, and passes it as `?ticket=`. A
   ticket is good for 30 s and one connection: a reconnect needs a new
   one. Without it a session gets `403 ticket_required`. Token and
   Unix-socket clients don't need one.
3. `GET /vms/{id}/console/ws` upgrades to a WebSocket. An unknown VM, or
   one you can't read, is `404`; a VM on a node that can't be reached is
   `502 node_unreachable`, before the upgrade.

On the WebSocket:

- The server first replays the console log, then sends the guest's
  output live, as binary frames of raw serial bytes. Use
  `binaryType = "arraybuffer"`.
- Send keystrokes as binary or text frames (text is sent as its UTF-8
  bytes).
- There are no control messages: no resize, no framing beyond the
  bytes.
- If the console can't be reached after the upgrade, the server sends
  one text frame with the error, then closes.
- Any number of clients can share a console.

`VmService.Console` is the same console as a bidirectional RPC stream,
for clients that speak HTTP/2 ([Streams](rpc.md#streams)).

`GET /vms/{id}/console/log?tail_bytes=N` returns the end of the console
log as raw bytes (`application/octet-stream`, at most 1 MiB);
`previous=true` reads the rotated one. See
[Console](../user-guide/console.md).

### Cluster reads

In a cluster any server answers reads after catching up with the
leader. Send `x-glidex-consistency: local` to skip that wait and accept
a possibly stale answer. Writes sent to a follower are forwarded to the
leader.

## Examples

```bash
export GLIDEX_TOKEN=<token>
api() { curl --fail-with-body --cacert tls.crt -H "Authorization: Bearer $GLIDEX_TOKEN" "$@"; }

api https://host.example.org:8841/auth/whoami
api 'https://host.example.org:8841/vms?project=lab'

api -X POST https://host.example.org:8841/images \
  -H 'Content-Type: application/json' -d '{"catalog": "ubuntu-26.04"}'

api -X POST 'https://host.example.org:8841/vms?wait=60' \
  -H 'Content-Type: application/json' -d '{
  "name": "web-1", "project": "lab", "vcpu_count": 2, "mem_size_mib": 2048,
  "hypervisor": "HYPERVISOR_QEMU", "image": "ubuntu-26.04",
  "root_disk_size_gib": 20, "credential": "alice",
  "networks": [{"network": "lab-net"}]
}'

api -X POST 'https://host.example.org:8841/vms/<vm-id>/stop?graceful_timeout_secs=60&wait=90'
```

Change a VM, then get a token from a script:

```bash
api -X PATCH 'https://host.example.org:8841/vms/<vm-id>?wait=60' \
  -H 'Content-Type: application/json' \
  -d '{"change": {"vcpu_count": 4}, "update_mask": "vcpu_count"}'

curl --cacert tls.crt -X POST https://host.example.org:8841/auth/token \
  -H 'Content-Type: application/json' \
  -d '{"username": "alice", "password": "<password>", "token_name": "ci"}'
# {"issued": {"token": "<token>", "token_id": "…",
#   "expires_at": "2026-11-17T08:00:00Z", "user_id": "…", "display_name": "Alice"}}
```

On the host itself, no token is needed:

```bash
curl --unix-socket /run/glidex-cp/api.sock http://localhost/vms
```

## Endpoints

Grouped by area. Query parameters in parentheses are optional.
The RPC twin of each operation is in [RPC API](rpc.md#services).

### Service and login

| Method | Path | What it does |
|---|---|---|
| `GET` | `/health` | Liveness: `{"status": "ok"}`; no credential |
| `GET` | `/auth/methods` | Which login methods are on (PAM, OIDC, connectors); no credential |
| `GET` | `/auth/server-info` | The server's identity: version, cluster, certificate fingerprint; no credential |
| `POST` | `/auth/discover` | Which login connectors to offer for a tenant slug or email (`{hint}`) |
| `POST` | `/auth/login` | Password login for the browser: sets a session cookie |
| `POST` | `/auth/token` | Password login for tools: `{issued: {token, token_id, expires_at, user_id, display_name}}` |
| `GET` | `/auth/oidc/start`, `/auth/oidc/callback` | OIDC browser login (redirects) |
| `POST` | `/auth/oidc/device`, `/auth/oidc/device/poll` | OIDC device flow for CLIs: start, then poll: `202 {pending: true}` until the user is done, then `200 {issued: …}` |
| `GET` | `/auth/c/{id}/start`, `/auth/c/{id}/callback` | Browser login through connector `{id}` |
| `POST` | `/auth/c/{id}/start` | Browser login that takes an invitation: `{invitation}` in the body, answers `{redirect}`; `404` for an invitation that isn't open or is for another connector; no credential |
| `POST` | `/auth/invitation` | What an invitation is for (tenant, connector, address, role, expiry): `{invitation: "<id>.<secret>"}`, from the link; `404` unless it is open; no credential |
| `POST` | `/auth/link/c/{id}/start` | Add a sign-in method through connector `{id}` to yourself (`{return_to}`); needs a recent login; answers `{redirect}`. The callback returns to `return_to` with `?linked=<id>` or `?link_error=taken\|not_allowed\|not_signed_in`; a SCIM-provisioned connector is always `not_allowed` |
| `POST` | `/auth/c/{id}/device`, `/auth/c/{id}/device/poll` | Device flow through connector `{id}`, answered the same way; `400 device_flow_unsupported` for a `google` connector without a device client |
| `POST` | `/auth/logout` | End the current session |
| `GET` | `/auth/whoami` | Who you are: user, method, teams, roles, default project |
| `POST` | `/authz/check` | Would these actions be allowed? One answer per check |
| `POST` | `/authz/allowed` | Which actions you may take on each of some resources |

### VMs

| Method | Path | What it does |
|---|---|---|
| `GET` | `/vms` | List VMs (`?project=`) |
| `POST` | `/vms` | Create a VM (`?wait=`). Boots through `firmware`, or a kernel image (`kernel`, with `kernel_from_disk` to follow the guest's kernel); `generate_seed` overrides whether the cloud-init seed is generated; `initrd_path` goes with `kernel_image_path` |
| `POST` | `/scheduler/preview` | Where a VM would be placed, node by node; a node is refused when a requested PCI device, or another device in its IOMMU group, is used by a VM there. Takes the VM's hugepage, NUMA, CPU and `devices` settings; each node's fit has `page_kib`, `hugepages_mib_taken` / `_capacity`, `numa_node`, `cpus`, `numa_aligned` and `pool_devices` |
| `GET` | `/vms/{id}` | Get a VM |
| `PATCH` | `/vms/{id}` | Change a VM's spec (`?wait=`, `If-Match`); also `kernel`, `kernel_from_disk`, `hugepage_size`, `numa_policy`, `cpu_policy` and `devices` |
| `DELETE` | `/vms/{id}` | Delete a VM and its own root disk (`?keep_disk=true`, `?wait=`) |
| `POST` | `/vms/{id}/start` | Start, or resume a paused VM (`?wait=`) |
| `POST` | `/vms/{id}/place` | Place a stopped or lost VM again: on `node` (`{node}`, optional), or where the scheduler finds room; answers `{vm}`. See [Placing a stopped or lost VM again](../operator-guide/datastores.md#placing-a-stopped-or-lost-vm-again) |
| `POST` | `/vms/{id}/stop` | Stop; `?graceful_timeout_secs=N` presses the power button first and waits up to `N` s (`?wait=`) |
| `POST` | `/vms/{id}/pause` | Pause (`?wait=`) |
| `GET` | `/vms/{id}/events` | The VM's last events |
| `POST` | `/vms/{id}/extract-kernel` | Extract the kernel on a stopped VM's root disk as a new kernel image the VM owns: `202 {image}`, extracting. See [Extract once](../user-guide/kernel-boot.md#extract-once) |
| `POST` | `/vms/{id}/devices` | Attach a PCI device (VFIO); hot-plugged if running |
| `DELETE` | `/vms/{id}/devices` | Detach a PCI device |
| `POST` | `/vms/{id}/disks` | Attach a data disk (on a running VM: at its next start) |
| `DELETE` | `/vms/{id}/disks/{disk}` | Detach a data disk |
| `GET` | `/vms/{id}/console` | Console availability and WebSocket path |
| `POST` | `/vms/{id}/console/ticket` | Single-use console ticket for a browser session |
| `GET` | `/vms/{id}/console/ws` | Console WebSocket |
| `GET` | `/vms/{id}/console/log` | Console log tail, raw bytes (`?tail_bytes=`, `?previous=true`) |

### Guest credentials

| Method | Path | What it does |
|---|---|---|
| `GET` | `/credentials` | List guest logins (no password hashes) |
| `POST` | `/credentials` | Create one: username, password and/or SSH keys |
| `GET` | `/credentials/{username}` | Get one |
| `PUT` | `/credentials/{username}` | Change the password and/or replace the SSH keys |
| `DELETE` | `/credentials/{username}` | Delete (`409` while a VM uses it) |

### Images

| Method | Path | What it does |
|---|---|---|
| `GET` | `/images/catalog` | Built-in cloud images for this host's architecture |
| `GET` | `/images/firmware-catalog` | Built-in UEFI firmware images |
| `GET` | `/images` | List images |
| `POST` | `/images` | Download and verify an image (`catalog`, `url` + `sha256`, or `firmware`); `202` for a new one, `200` with one already in flight. Kernel images: `kind: IMAGE_KIND_KERNEL` with `url`, `initrd_url`, `initrd_sha256` and `cmdline`; `kernel_path` and `initrd_path` (host files); `kernel_from` (extract from a disk image); `with_kernel` (also extract a pulled disk image's kernel). See [Kernel boot](../user-guide/kernel-boot.md#kernel-images) |
| `GET` | `/images/{id}` | Get an image and its download progress |
| `DELETE` | `/images/{id}` | Delete, or cancel a download (`?wait=`; `409` while a disk uses it, or a VM boots or keeps a kernel image) |
| `POST` | `/images/{id}/retry` | Download a failed image again, or run a failed kernel extraction again |
| `GET` | `/images/{id}/events` | The image's last events |
| `POST` | `/images/{id}/copies` | Cluster: keep a copy on a node (`{node}`) |
| `DELETE` | `/images/{id}/copies/{node}` | Cluster: drop a node's copy |

### Disks

| Method | Path | What it does |
|---|---|---|
| `GET` | `/disks` | List disks |
| `POST` | `/disks` | Create a blank disk or one from an image (`?wait=`); `node` (name or id) binds it to a node now, an unknown node is `400 invalid_config`; `datastore` (name or id) puts it there instead of the default datastore |
| `GET` | `/disks/{id}` | Get a disk and its partition table |
| `DELETE` | `/disks/{id}` | Delete (`409` while attached) |
| `POST` | `/disks/{id}/move` | Move a disk no running VM uses to another datastore: `{datastore, node, flatten, wait_secs}` (`wait_secs` or `?wait=`, at most 300); `200` once moved, else `202`. See [Moving a disk between datastores](../operator-guide/datastores.md#moving-a-disk-between-datastores) |
| `POST` | `/disks/{id}/resize` | Grow or shrink (`?wait=`) |
| `POST` | `/disks/{id}/extend-root` | Grow the root partition, offline or at next boot (`?wait=`) |
| `GET` | `/disks/{id}/events` | The disk's last events |

A disk has `datastore` (an id, or `local`) and, while it moves,
`moving_to`. On a shared datastore it has no `node`; `worker` and
`worker_name` name the node that works on its file.

### Datastores

See [Datastores](../operator-guide/datastores.md). `{id}` is an id or a
name.

| Method | Path | What it does |
|---|---|---|
| `GET` | `/datastores` | List datastores, with each node's status (everyone) |
| `POST` | `/datastores` | Register one: `name`, `driver` (`DATASTORE_DRIVER_DIR` or `DATASTORE_DRIVER_SHARED_DIR`), `path`, optional `node_paths` (node → path), `nodes`, `default_for_disks`, `overcommit` (default on); `201` |
| `GET` | `/datastores/{id}` | Get a datastore |
| `PATCH` | `/datastores/{id}` | Change `node_paths`, `nodes`, `default_for_disks` or `overcommit` |
| `DELETE` | `/datastores/{id}` | Delete a datastore no disk is on (`409` otherwise); `204` |

A datastore also has `topology` (`DATASTORE_TOPOLOGY_NODE_LOCAL` or
`DATASTORE_TOPOLOGY_SHARED`), `builtin` (`local`), `created_at`,
`disks` (how many are on it) and `node_status`: per node, `node`,
`node_name`, `reachable`, `reason` and `message` when it isn't, `path`,
`total_bytes`, `free_bytes` and `checked_at`. An image's copy in a
shared datastore has `datastore` and `datastore_name` instead of a
`node`.

### Networks

| Method | Path | What it does |
|---|---|---|
| `GET` | `/networks` | List networks |
| `POST` | `/networks` | Create a host or cluster network (NAT, isolated or bridged) |
| `GET` | `/networks/{id}` | Get a network |
| `DELETE` | `/networks/{id}` | Delete (`?wait=`; `409` while a VM uses it) |
| `GET` | `/networks/{id}/events` | The network's last events |
| `GET` | `/networks/{id}/ovn` | A cluster network as OVN sees it: switch, ports, router |
| `PUT` | `/networks/{id}/grants` | Which projects and tenants may use a host network (replaces the grants) |
| `POST` | `/networks/{id}/shares` | Offer a project network to another project |
| `DELETE` | `/networks/{id}/shares/{project}` | Withdraw a share |
| `POST` | `/projects/{id}/networks` | Create a project network |
| `PUT` | `/projects/{id}/networks/{network}/router` | Put a project network on one of the project's routers, or back on the shared edge |
| `GET` | `/projects/{id}/network-shares` | Shares offered to or accepted by a project |
| `POST` | `/projects/{id}/network-shares/{network}/accept` | Accept a share |
| `DELETE` | `/projects/{id}/network-shares/{network}` | Leave a share |
| `GET` | `/projects/{id}/routers` | A project's routers |
| `POST` | `/projects/{id}/routers` | Create a router |
| `GET` | `/projects/{id}/routers/{router}` | Get a router |
| `DELETE` | `/projects/{id}/routers/{router}` | Delete a router (`409` while a network is attached) |

### Host networking and PCI

These act on the host that answers them (in a cluster, the UI sends
them to the node being managed).

| Method | Path | What it does |
|---|---|---|
| `GET` | `/ovs/status` | Open vSwitch and networking helper status |
| `POST` | `/ovs/install` | Install Open vSwitch (kernel or DPDK profile) |
| `POST` | `/ovs/dpdk-init` | Initialize DPDK in Open vSwitch |
| `GET` | `/ovs/bridges` | List glidex bridges |
| `POST` | `/ovs/bridges` | Create a bridge |
| `DELETE` | `/ovs/bridges/{name}` | Delete a bridge |
| `GET` | `/ovs/bridges/{name}/uplinks` | List a bridge's uplinks |
| `POST` | `/ovs/bridges/{name}/uplinks` | Add an uplink (kernel, AF_XDP or DPDK) |
| `DELETE` | `/ovs/bridges/{name}/uplinks/{uplink}` | Remove an uplink, restoring the NIC |
| `POST` | `/ovs/bridges/{name}/uplinks/{uplink}/commit` | Keep a migrated host address (otherwise it is rolled back at the deadline) |
| `POST` | `/ovs/bridges/{name}/uplinks/{uplink}/reapply` | Set up again an uplink that was rolled back at start |
| `GET` | `/pci-devices` | List the host's PCI devices, with `numa_node`, `ready` and, when not ready, `not_ready` (the reasons) |
| `GET` | `/pci-pools` | Device pools: devices per node, holders, free counts; `?project=` the pools that project may use, free counts only |
| `GET` | `/host/topology` | The host's NUMA nodes and PCIe tree, with device readiness and warnings |

### Usage and metrics

| Method | Path | What it does |
|---|---|---|
| `GET` | `/usage` | Usage records by hour, day or month, grouped by project, VM, disk, NIC or network (`format=csv` for CSV) |
| `GET` | `/projects/{id}/usage` | The same, for one project |
| `GET` | `/vms/{id}/usage` | The same, for one VM and its NICs |
| `GET` | `/disks/{id}/usage` | The same, for one disk |
| `GET` | `/usage/bandwidth` | Per billing month: average, 30-second peak, 95th percentile of network traffic |
| `GET` | `/usage/disk-io` | The same for disk I/O |
| `GET` | `/usage/compute` | The same for CPU and memory |
| `GET` | `/vms/{id}/bandwidth` | 5-minute bandwidth series for a VM (`from`, `to`, `p95=true`) |
| `GET` | `/vms/{id}/io` | 5-minute disk I/O series for a VM |
| `GET` | `/vms/{id}/compute` | 5-minute CPU and memory series for a VM |
| `GET` | `/disks/{id}/io` | 5-minute I/O series for a disk |
| `GET` | `/networks/{id}/bandwidth` | 5-minute bandwidth series for a network |
| `GET` | `/vms/{id}/stats` | A VM's current rates: CPU, memory, NICs, disks |
| `GET` | `/networks/{id}/stats` | A network's current rates |

### Projects and access

| Method | Path | What it does |
|---|---|---|
| `GET` | `/projects` | Projects you can see |
| `POST` | `/projects` | Create a project |
| `GET` | `/projects/{id}` | Get a project with its quotas and usage |
| `PATCH` | `/projects/{id}` | Change description or quotas |
| `DELETE` | `/projects/{id}` | Delete a project |
| `GET` | `/projects/{id}/bindings` | A project's role bindings |
| `POST` | `/projects/{id}/bindings` | Grant a role on the project to a user, team or token |
| `DELETE` | `/projects/{id}/bindings/{link}` | Remove a binding |
| `GET` | `/system/bindings` | Host role bindings: the cluster's and every host's (`node`: one node's host only) |
| `POST` | `/system/bindings` | Grant a host role, cluster-wide or, with `node` (id or name), on one node's host (`role.auditor`, `role.net-admin`, `grant.host-paths`) |
| `DELETE` | `/system/bindings/{link}` | Remove a host role binding |
| `GET` | `/tokens` | Your API tokens (all, for administrators) |
| `POST` | `/tokens` | Create a token, personal or for a service account; the secret is returned once |
| `DELETE` | `/tokens/{id}` | Revoke a token |
| `GET` | `/users` | List users and their identities |
| `POST` | `/users` | Create a user |
| `PATCH` | `/users/{id}` | Change a user; for a user a SCIM connector manages (`managed_by`), its display name, and enabling it after its IdP disabled it, are `409 managed_by_connector` |
| `DELETE` | `/users/{id}` | Delete a user with its sign-in methods, sessions, personal tokens, team memberships, role bindings and tenant memberships; answers how many of each went. `409 managed_by_connector` for a managed user that is still enabled |
| `POST` | `/users/{id}/identities` | Link a login identity to a user |
| `DELETE` | `/users/{id}/identities/{key}` | Remove a sign-in method, yours or (as an administrator) anyone's, never the last unless the user is disabled (`end_sessions`) |
| `GET` | `/users/me` | Same as `GET /auth/whoami` |
| `PATCH` | `/users/me` | Change your own profile (e.g. `default_project`) |
| `GET` | `/users/me/ssh-keys` | Your SSH public keys, to prefill a guest credential: your local account's, or, without one here, those of a pulled connector's SSH key field; `source` says which (`local`, `connector:<id>`) |
| `GET` | `/teams` | List teams |
| `POST` | `/teams` | Create a team |
| `PATCH` | `/teams/{id}` | Change a team (`409 managed_by_connector` for a team a SCIM connector manages) |
| `DELETE` | `/teams/{id}` | Delete a team (`409 managed_by_connector` for a managed team) |
| `PUT` | `/teams/{id}/members/{user}` | Add a member |
| `DELETE` | `/teams/{id}/members/{user}` | Remove a member's manual membership; one a SCIM connector gave is `409 managed_by_connector` |
| `GET` | `/authz/policies` | List site Cedar policies |
| `GET` | `/authz/policies/{id}` | Get a policy |
| `GET` | `/authz/policies/{id}/versions` | A policy's history |
| `PUT` | `/authz/policies/{id}` | Create or replace a policy (with its current `version`) |
| `DELETE` | `/authz/policies/{id}` | Delete a policy |
| `POST` | `/authz/validate` | Check a policy without saving it |
| `POST` | `/authz/simulate` | Decide requests under the current policies plus proposed changes |
| `POST` | `/authz/reload` | Reload policies |
| `GET` | `/audit` | The audit log (`since`, `limit`, `project`, `user`) |

### Tenants and login connectors

| Method | Path | What it does |
|---|---|---|
| `GET` | `/tenants` | Tenants you can see (`page_size`, `page_token`) |
| `POST` | `/tenants` | Create a tenant; with `first_admin: {email, connector, days}`, also invite its first `tenant-admin` (the link in `invitation_link`, once) |
| `GET` | `/tenants/{id}` | Get a tenant |
| `PATCH` | `/tenants/{id}` | Change a tenant |
| `DELETE` | `/tenants/{id}` | Delete a tenant (`409 tenant_in_use` while it has projects) |
| `POST` | `/tenants/{id}/suspend` | Suspend a tenant |
| `POST` | `/tenants/{id}/resume` | Resume a tenant |
| `GET` | `/tenants/{id}/federations` | The tenant's workload federations (CI issuers it trusts) |
| `POST` | `/tenants/{id}/federations` | Trust a CI or workload issuer: its audience, and rules mapping its tokens to service accounts |
| `GET` / `PATCH` / `DELETE` | `/tenants/{id}/federations/{federation}` | Read, change (enable, disable) or remove a federation |
| `GET` | `/tenants/{id}/members` | List explicit members |
| `POST` | `/tenants/{id}/members` | Add a member |
| `DELETE` | `/tenants/{id}/members/{principal}` | Remove a member (`user:<id>` or `team:<id>`) |
| `GET` | `/tenants/{id}/bindings` | Tenant role bindings |
| `POST` | `/tenants/{id}/bindings` | Grant a tenant role |
| `DELETE` | `/tenants/{id}/bindings/{link}` | Remove a tenant role binding |
| `GET` | `/tenants/{id}/device-pools` | The tenant's pools on this cluster, with what they hold |
| `PUT` | `/tenants/{id}/device-pools` | Replace the tenant's pools: `{pools: [{name, from, max, projects}]}` |
| `GET` | `/tenants/{id}/invitations` | A tenant's invitations, never their secrets |
| `POST` | `/tenants/{id}/invitations` | Invite an email address (`{email, role, connector, days}`); the link is returned once |
| `DELETE` | `/invitations/{id}` | Revoke an invitation |
| `GET` | `/admissions` | Tenants admitted on this site, with their caps |
| `PUT` | `/admissions/{tenant}` | Admit a tenant or change its cap |
| `DELETE` | `/admissions/{tenant}` | Remove an admission |
| `GET` | `/connectors` | List login connectors |
| `POST` | `/connectors` | Create a connector (`pam`, `oidc`, `google` or `jumpcloud` settings) |
| `GET` | `/connectors/{id}` | Get a connector (secrets show only whether they are set); a pulled one has `sync` (its settings) and `sync_status` (the last run) |
| `PATCH` | `/connectors/{id}` | Change a connector; `provisioning` (`none`; `scim` for `oidc` and `jumpcloud`, or `pull` for a Google Workspace connector, both with `jit` off), `group_mode` (`mapped`, or `auto` when provisioned) and `sync` (`interval_minutes`, `max_disable`, `admin_email`, `groups`, `ssh_keys_field`) are fields like the others |
| `DELETE` | `/connectors/{id}` | Delete a connector and its SCIM tokens; `?users=detach` keeps the users it provisions enabled (the default, `?users=disable`, disables them). Answers `200 {users, teams}`: how many it released |
| `PUT` | `/connectors/{id}/secrets/{field}` | Set a connector secret (write-only) |
| `POST` | `/connectors/{id}/test` | Test a connector step by step |
| `POST` | `/connectors/{id}/sync` | Run a pulled connector's sync now: `{status}`, with `state` (`SYNC_STATE_OK`, `_FAILED`, `_BLOCKED`), `started_at`, `finished_at`, `changes` (`created`, `adopted`, `updated`, `disabled`, `enabled`), `error`, `would_disable` and `last_applied_at`. `{confirm: true}` applies one the safety stop blocked, and needs a recent login |
| `GET` | `/connectors/{id}/scim-tokens` | A SCIM connector's tokens (never the tokens) and its endpoint path |
| `POST` | `/connectors/{id}/scim-tokens` | Make a SCIM token (`{name, days}`, 1–365 days, default 180; at most five live per connector): `201 {token, secret, path}`, the `gxs_` secret shown once. Needs a recent login; `409 not_provisioned` unless the connector is `provisioning: scim` |
| `DELETE` | `/connectors/{id}/scim-tokens/{token}` | Revoke a SCIM token |

### SCIM 2.0, for identity providers

A connector with `provisioning: scim` serves a SCIM 2.0 endpoint for its
IdP (see [Provisioning with
SCIM](../operator-guide/authentication.md#provisioning-with-scim)):

- `https://host.example.org:8841/scim/v2/c/{id}` on the API, and
  `https://host.example.org:5173/api/scim/v2/c/{id}` through the web UI.
- The IdP sends one of the connector's tokens:
  `Authorization: Bearer gxs_<token>`. Nothing else opens it: not an API
  token, not a session, not another connector's SCIM token.
- Bodies and answers are `application/scim+json`; errors are SCIM error
  bodies (`{"schemas": [...], "status": "400", "scimType": "invalidFilter", "detail": "..."}`),
  not Glidex errors.

| Method | Path (under `/scim/v2/c/{id}`) | What it does |
|---|---|---|
| `GET` | `/ServiceProviderConfig`, `/Schemas`, `/ResourceTypes` | What the endpoint supports |
| `GET` | `/Users` | The connector's users (`filter`, `startIndex`, `count`) |
| `POST` | `/Users` | Create a user; adopts a user that logged in through the connector before it was provisioned, with the same user name or email |
| `GET`, `PUT`, `PATCH`, `DELETE` | `/Users/{uid}` | Read, replace, change, or remove a user |
| `GET` | `/Groups` | The connector's groups (`filter`, `startIndex`, `count`) |
| `POST` | `/Groups` | Create a group |
| `GET`, `PUT`, `PATCH`, `DELETE` | `/Groups/{gid}` | Read, replace, change (members too), or delete a group |

- **Filters**: `eq` only. Users by `userName`, `externalId`, `emails`
  (or `emails.value`) and `displayName`; groups by `displayName` and
  `externalId`.
- **Paging**: `startIndex` (from 1) and `count`, at most 200 a page
  (100 by default).
- **PATCH**: `add`, `replace` and `remove`, with or without a `path`, in
  the forms Entra, Okta and JumpCloud send: `active` (also as the string
  `"False"`), `userName`, `externalId`, `displayName`, `name.givenName`,
  `name.familyName`, `emails[type eq "work"].value`; a group's
  `displayName` and `members` (a list, or `members[value eq "<id>"]`).
  Attributes Glidex doesn't keep are ignored.
- **Not supported**: bulk, sorting, ETags, `/Me`, password changes.
- **Users**: `active: false` disables the user (sessions end, personal
  tokens are revoked, it leaves the IdP's teams); `active: true` enables
  it again only if the IdP disabled it, never after an administrator
  did. `DELETE /Users/{uid}` disables the user and ends its link to the
  IdP; the user and its history stay.
- **Groups**: with `group_mode: auto` each group is a managed team;
  with `mapped`, a group named in `group_teams` fills that team.

| Status | `scimType` | Meaning |
|---|---|---|
| 400 | `invalidFilter` | A filter other than those above |
| 400 | `invalidSyntax`, `invalidValue`, `noTarget` | A body that isn't JSON, a bad value, a `remove` without a path |
| 401 | | No token, or a wrong, expired or revoked one (the same answer each time) |
| 403 | | The connector is disabled or isn't `provisioning: scim` |
| 404 | | No such user or group of this connector |
| 409 | `uniqueness` | A `userName` or group the connector already has |
| 413 | | A body over 1 MiB |
| 429 | `tooMany` | Over 20 requests a second (with a short burst) per connector; retry after `Retry-After` seconds |

Every write is in the [audit log](../operator-guide/access-control.md#audit-log)
as `system:scim:<connector-id>`, never with the token.

### Cluster and nodes

Administrator operations; see [Clusters](../operator-guide/clusters.md).
`gxctl cluster …` and `gxctl node …` wrap them. A node's `{id}` is its
id or its name.

| Method | Path | What it does |
|---|---|---|
| `GET` | `/cluster/status` | Raft group, nodes, versions, CA rotation (`?ports=true`: port reachability) |
| `POST` | `/cluster/init` | Turn this host into a cluster of one |
| `POST` | `/cluster/join` | Join this host to a cluster with a join token |
| `POST` | `/cluster/import` | Join with this host's VMs and disks (waits for approval) |
| `POST` | `/cluster/rejoin` | Come back as the same node with a rejoin token |
| `POST` | `/cluster/leave` | Go back to standalone after this node was removed |
| `POST` | `/cluster/dissolve` | The last node ends the cluster, keeping everything |
| `POST` | `/cluster/join-tokens` | Create a one-time join token |
| `POST` | `/cluster/promote` | Promote agents to servers |
| `POST` | `/cluster/rotate-ca` | Rotate the cluster CA |
| `POST` | `/cluster/rotate-secrets-key` | Rotate the key that protects connector secrets |
| `GET` | `/cluster/snapshot` | Download a snapshot of the replicated store (store it like the database) |
| `GET` | `/cluster/settings/ovn` | Whether cluster networks use OVN, and its settings |
| `PUT` | `/cluster/settings/ovn` | Turn OVN on or off and set the shared edge |
| `GET` | `/cluster/settings/ovn/edge-defaults` | A suggested shared edge from what the nodes report |
| `GET` | `/cluster/ovn` | OVN as it is now: databases, chassis, gateways |
| `GET` | `/cluster/ovn/tunnels` | Each node's Geneve tunnels and their state |
| `GET` | `/nodes` | List nodes |
| `GET` | `/nodes/{id}` | Get a node |
| `GET` | `/nodes/{id}/topology` | A node's NUMA nodes and PCIe tree, read on that node |
| `POST` | `/nodes/{id}/devices/{address}/prepare` | Bind a device's IOMMU group to vfio-pci on its node |
| `POST` | `/nodes/{id}/devices/{address}/sriov` | Set a physical function's SR-IOV virtual functions: `{"vfs": N}` (0 removes them) |
| `POST` | `/nodes/{id}/devices/{address}/release` | Put a prepared device back on its drivers |
| `POST` | `/nodes/{id}/devices/{address}/cordon` | No new VM gets the device |
| `POST` | `/nodes/{id}/devices/{address}/uncordon` | Give it to new VMs again |
| `POST` | `/nodes/{id}/drain` | Stop placing VMs on a node |
| `POST` | `/nodes/{id}/undrain` | Place VMs on it again |
| `POST` | `/nodes/{id}/remove` | Remove an empty node |
| `POST` | `/nodes/{id}/forget` | Forget a node that is gone for good |
| `POST` | `/nodes/{id}/purge` | Drop a forgotten node's record |
| `POST` | `/nodes/{id}/rejoin-token` | Token for a node to rejoin as itself |
| `POST` | `/nodes/{id}/detach` | Detach a node with its resources, back to standalone (`map_networks`, `with_access`); the answer names the networks it maps or drops in `network_names` (id → name). With `abort`: cancel a detach; with `departed`: forget the departed node |
| `PUT` | `/nodes/{id}/ovn` | A node's `br-int` datapath and physical network mappings |
| `POST` | `/nodes/{id}/spare-image-files/delete` | Delete image files a node holds that nothing records |
| `GET` | `/departures` | Detaches in progress or done, with `network_names` as the detach saw them |
| `GET` | `/imports` | Import plans |
| `GET` | `/imports/{plan}` | One plan and what it brings |
| `POST` | `/imports/{plan}/approve` | Approve a plan with mappings (`dry_run` checks only) |
| `POST` | `/imports/{plan}/reject` | Reject a plan |

### System

| Method | Path | What it does |
|---|---|---|
| `GET` | `/watch` | Live change events (server-sent events; above) |
| `GET` | `/system/reconcile` | Controller status: runner, networking helper, queue, unknown VMs (host administrators) |
