# REST API

The control plane serves HTTPS on every address, `0.0.0.0:8841` and
`[::]:8841`, by default (`listen` / `GLIDEX_LISTEN` override; a
self-signed certificate unless one is configured; plain HTTP only on
loopback — [security.md](security.md) §5.1), and on its Unix sockets.
All request and
response bodies are JSON except the console WebSocket, the console
log (raw bytes) and `GET /watch` (server-sent events).

## Authentication and authorization

Every endpoint except `/health`, `/auth/methods`, `/auth/login`,
`/auth/token`, `/auth/discover` and the OIDC and connector login
endpoints (`/auth/oidc/…`, `/auth/c/{id}/…`) needs a principal: a local user on `api.sock` (peer uid),
a bearer token, or a browser session (cookie + `X-Glidex-CSRF` on
writes). Each route maps to one Cedar action; see
[security.md](security.md) §7 and §13 for the actions, the additional
endpoints (projects, bindings, tokens, users, teams, policies, audit) and
the error codes `401 unauthenticated`, `401 reauth_required`,
`403 forbidden`, `403 quota_exceeded`, `409 would_lock_out` and
`422 invalid_policy`.

The storage and VM families (guest credentials, images, disks; VMs,
their events, console info and tickets, and `/scheduler/preview`) are
served in the proto shape ([grpc.md](grpc.md) §9.1, [images.md](images.md) §8): the
`glidex.storage.v1` and `glidex.vm.v1` messages as JSON, wrapped (`{disks}`, `{disk}`,
`{disk, settled}`, …), with 64-bit numbers as strings, enum names in
full and RFC 3339 times. A body that is JSON but not the message (an
unknown enum value, a wrong type) is `422 invalid`, as before; so is a
`POST /vms` without `name`, `vcpu_count` or `mem_size_mib` (proto3 has
no required fields, so the REST twin checks them).

The usage family (`/usage…`, the `/…/bandwidth`, `/…/io` and
`/…/compute` series, and `/…/stats`) is served in the proto shape too:
the `glidex.usage.v1` messages as JSON (`{from, to, granularity, timezone,
group_by, rows, complete_through, metering_started_at}` and so on), with
`group_by` and `granularity` by enum name (`GROUP_KEY_VM`,
`GRANULARITY_DAY`), a meter's `raw` value as a string, and fields that
are unknown, empty or zero left out (no `null`s). The query parameters are
unchanged (and also take the enum names). `format=csv` still answers the
same CSV ([metering.md](metering.md) §9.5).

The access family (projects and their bindings, users, teams, tokens,
system bindings, site policies, audit, `/authz/check` and
`/authz/allowed`, and `GET /auth/whoami`, `GET`/`PATCH /users/me`,
`GET /users/me/ssh-keys`) is served in the proto shape too: the
`glidex.access.v1` messages, wrapped (`{projects}`, `{project}`,
`{bindings}`, `{binding}`, `{users}`, `{user}`, `{teams}`, `{team}`,
`{tokens}`, `{token, record}`, `{versions}`, `{entries}`). What changed:
- `GET /auth/whoami`, `GET /users/me` and a successful PAM `POST
  /auth/login` answer `WhoAmIResponse`: `user_id`, `display_name`,
  `method`, `token_id`, `teams`, `break_glass`, `host_roles`,
  `project_roles`, `default_project`, `mfa`, `directory_id`, and `user`
  (the access `User`), `token` (`{id, name, device, client}`) and, for a
  session, `csrf`.
- A binding's `template` is `role`; it carries `resource: {type, id}`.
- A token's `kind` is `TOKEN_KIND_PERSONAL` or
  `TOKEN_KIND_SERVICE_ACCOUNT`, with
  `owner` and `project` beside it; its times are RFC 3339.
- Quotas: an absent limit is unlimited (no `null`); a project's
  `networks`, `routers` and `external_ips`, when left out, take their
  defaults ([security.md](security.md) §6.3). `usage` (on list and get)
  leaves out zero counts.
- `PATCH /projects/{id}`, `/users/{id}` and `/teams/{id}` change the
  fields present, or those in `update_mask` (`"description,quotas"`).
- `GET /authz/policies/{id}` answers `{source, site?|loaded?}`; a policy
  `version` is a string. `POST /authz/allowed` answers `{allowed:
  [{actions}], reauth: [{actions}]}`. Audit `time` is RFC 3339 (the
  `since` parameter is still unix ms).

The network family (networks, their grants, events and OVN view,
project networks and their routers, shares, VPC routers) is served in
the proto shape too: the `glidex.network.v1` messages, wrapped
(`{networks}`, `{network}`, `{shares}`, `{routers}`, `{router}`), with
enum names in full (`NETWORK_MODE_NAT`, `PORT_TYPE_TAP`,
`NETWORK_SCOPE_CLUSTER`, `NETWORK_PHASE_READY`, `SHARE_STATUS_OFFERED`)
and RFC 3339 times.
Networks have an `id` (tenancy.md D24): every path and body that names
a network takes its id or its name (a name several visible networks
have is `409 ambiguous`), and answers name networks by id: a share's
`network` is the id, with `network_name`; a VM's `nics[].network` is the
id, with `network_name`. New node networks' default bridge is
`gxbr-<first 8 of the id>`. `DELETE /networks/{id}?wait=N` answers `204`
once gone (was `200`); a share offer answers `202 {}`. Host networking
(`/ovs…`, bridges, uplinks, PCI devices) and `/cluster/ovn…` keep their
shapes.

The host family (`/ovs/status`, `/ovs/install`, `/ovs/dpdk-init`,
bridges, uplinks and `/pci-devices`) is served in the proto shape too:
the `glidex.host.v1` messages, wrapped (`{bridges}`, `{bridge}`,
`{uplinks}`, `{uplink}`, `{devices}`), with enum names in full
(`NETD_ACCESS_FULL`, `DATAPATH_SYSTEM`, `UPLINK_KIND_KERNEL`,
`UPLINK_PHASE_PENDING_COMMIT`, `XDP_MODE_NATIVE`, `OVS_PROFILE_DPDK`), an
uplink's commit
`deadline` and `boot_rollback.at` in RFC 3339, and hugepage sizes and
counts as strings. Status codes are unchanged. These operations act on
the host that answers them; from an agent's UI they run on the agent
(over REST or gRPC-Web), authorized by a server.

The cluster family (`/cluster/…` but the snapshot, `/nodes…`,
`/imports…`, `/departures`) is served in the proto shape too: the
`glidex.cluster.v1` messages, wrapped (`{nodes}`, `{node}`, `{plans}`,
`{plan}`, `{departures}`), with enum names in full (`NODE_PHASE_ACTIVE`,
`NODE_ROLE_SERVER`, `CONDITION_STATUS_TRUE` for a node's `ready`,
`RAFT_STATE_LEADER`, `BR_INT_DATAPATH_SYSTEM`, `IMPORT_STATE_PENDING`,
`DEPARTURE_STATE_FROZEN`, `TUNNEL_STATE_BFD_DOWN`), RFC 3339 times (an OVN chassis' `nb_cfg_timestamp` in
ms is `nb_cfg_at`), and 64-bit numbers (Raft ids, terms and indexes,
generations, memory, bytes, counters) as strings. `GET /cluster/status`
always says `clustered`; `cluster_networks` in the OVN settings are
`[{id, name}]`. `POST /nodes/{id}/detach` answers the detach, or with
`abort` `{node, aborted}`, or with `departed` the departed node (three
RPCs: `DetachNode`, `AbortDetach`, `ForgetDeparted`); the membership
routes (`remove`, `forget`, `purge`, `rejoin-token`, `detach`) ignore body
flags their operation doesn't take, as gxctl sends them all. Status codes
are unchanged. `GET /cluster/snapshot` (binary) stays REST-only.

In every ported family, a body field the message doesn't have is
`422 invalid` ("unknown field `quotas.vm`").

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
| `POST` | `/scheduler/preview` | `vms::preview` | Where a VM would go: `{vcpu_count, mem_size_mib, hypervisor?, node?, networks?, vfio_devices?, data_disks?, project?}` → `{pick, nodes: [{id, name, refused, vcpus_taken, vcpus_capacity, mem_mib_taken, mem_mib_capacity, vms}]}`, the scheduler's preference first; needs `createVm` on the project (spec/clustering-ui.md §5.1) |
| `GET` | `/vms/{id}[?view=full]` | `get_one` | Get a VM by id; `view=full` returns the stored `{meta, spec, status}` record instead of the flat `VmResponse`; it also needs `readSystemStatus` (`host.read`), since the record carries host paths, PIDs and the boot id |
| `PATCH` | `/vms/{id}` | `patch_one` | JSON merge patch of the VM's spec (below) |
| `DELETE` | `/vms/{id}[?keep_disk=true]` | `delete_one` | Request deletion; the controller stops the VM and cleans up (below) |
| `POST` | `/vms/{id}/start` | `start` | Desired state `running` (from `paused`: resume) |
| `POST` | `/vms/{id}/place` | `place` | Body `{node?}`: place a stopped VM, or one whose node was forgotten, again on `node` or where there is room; its disks must be on shared datastores (spec/storage.md §9). `placeVm` |
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
| `GET` | `/pci-devices` | `list_pci_devices` | Enumerate host PCI devices, with NUMA node and readiness (host-topology.md §5.1) |
| `GET` | `/pci-pools` | `nodes::pci_pools` | Device pools (host-topology.md §9.1, action `listPciPools`); `?project=`: the pools that project may use (`usePciPool`), free counts only, for anyone who can read the project (`readProject`) |
| `GET` | `/host/topology` | `host_topology` | This host's topology tree (host-topology.md §3.2, action `readHostTopology`) |
| `GET` | `/credentials` | `list_credentials` | List stored guest logins (no hashes) |
| `POST` | `/credentials` | `create_credential` | Create a login; password is hashed on arrival |
| `GET` | `/credentials/{username}` | `get_credential` | Get one login (no hash) |
| `PUT` | `/credentials/{username}` | `update_credential` | Change password and/or replace SSH keys |
| `DELETE` | `/credentials/{username}` | `delete_credential` | Delete; `409` while a VM uses it |
| `GET` | `/images/catalog` | `image_catalog` | Built-in cloud images for this host's arch |
| `GET` / `POST` | `/images` | `list_images` / `pull_image` | List; download + verify (`202`). In a cluster `POST` also takes `arch` (default: that of `nodes`, else the answering server's), `nodes` (copies placed once downloaded) and `keep_server_copy` (default `true`) (clustering.md §9.2.1–2) |
| `POST` / `DELETE` | `/images/{id}/copies[/{node}]` | `add_image_copy` / `remove_image_copy` | Keep a copy on a node ahead of need (`{node}`; `409` for another architecture), or let one go (`409` while the node needs it or it is the last ready copy); `pullImage` (clustering.md §9.2.2) |
| `POST` | `/nodes/{id}/spare-image-files/delete` | `delete_spare_image_files` | The node deletes the unrecorded image files it has reported (`pullImage`; clustering.md §9.2.3) |
| `GET` / `DELETE` | `/images/{id}[?wait=N]` | `get_image` / `delete_image` | Details + progress; delete or cancel (`204`, or `202` while the image controller finishes; `409` while a disk uses it; below) |
| `POST` | `/images/{id}/retry` | `retry_image` | Download a failed image again (`202`; needs `pullImage`) |
| `GET` | `/images/{id}/events` | `image_events` | The image's last 50 events (`readImage`) |
| `GET` / `POST` | `/disks[?wait=N]` | `list_disks` / `create_disk` | List; create blank or from an image (`201`, made by the disk controller; below) |
| `GET` / `DELETE` | `/disks/{id}` | `get_disk` / `delete_disk` | Details + partition table; delete (`204`, or `202` while an operation on it finishes; `409` while attached) |
| `POST` | `/disks/{id}/resize[?wait=N]` | `resize_disk` | Grow (extends root) or shrink (never into a partition); `202`, applied by the disk controller |
| `POST` | `/disks/{id}/move` | `move_disk` | Body `{datastore, node?, flatten?, wait_secs?}`: copy a disk that no running VM uses to another datastore, by the node that has it; a linked disk is rebased or flattened. Answers `{disk, settled}`; while moving, the disk shows `moving_to` (spec/storage.md §8.3). `moveDisk`, plus `useDatastore` on the destination |
| `POST` | `/disks/{id}/extend-root[?wait=N]` | `extend_root` | Grow the root partition, `offline` or `on-boot`; `202`, applied by the disk controller |
| `GET` | `/disks/{id}/events` | `disk_events` | The disk's last 50 events (`readDisk`) |
| `GET` | `/networks/{id}/events` | `network_events` | The network's last 50 events (`readNetwork`) |
| `DELETE` | `/networks/{id}[?wait=N]` | `delete_network` | Delete (`204`, or `202` while the network controller tears it down; `409` while a VM uses it; below) |
| `GET` / `POST` | `/projects/{id}/routers` | `list_routers` / `create_router` | VPC routers of a project; create one (`201`; `409 external_pool_exhausted`; quotas `routers`, `external_ips`; [clustering.md §11.2a](clustering.md#112a-vpc-routers)) |
| `GET` / `DELETE` | `/projects/{id}/routers/{router}` | `get_router` / `delete_router` | Details; delete (`204`; `409` while a network is attached) |
| `PUT` | `/projects/{id}/networks/{network}/router` | `set_network_router` | Put an existing NAT or isolated cluster network of the project on one of its routers, `{"router": "<name>"}`, or back on the shared edge (an isolated one: not routed), `{"router": null}` (`createRouter`; `400` for another kind of network or an unknown router, `404` for a network that isn't the project's; [clustering-ui.md §8d](clustering-ui.md#8d-a-projects-own-routers)) |
| `GET` | `/cluster/status[?ports=true]` | `cluster::status` | Raft group, nodes with version and feature level, CA rotation, and TCP reachability of the cluster ports from the serving server |
| `POST` | `/cluster/{init,join,import,rejoin,leave,dissolve,rotate-ca,promote,join-tokens}` | `cluster::*` | Forming, joining (with `import` for a host with resources), leaving and operating a cluster ([clustering.md §5](clustering.md#5-cluster-lifecycle), [runbook](cluster-runbook.md)) |
| `GET` | `/cluster/snapshot` | `cluster::snapshot` | A consistent snapshot of the replicated store (0600 material; always audited) |
| `GET` / `PUT` | `/cluster/settings/ovn` | `cluster::{get,put}_ovn_settings` | OVN as a cluster setting (spec/clustering-ui.md §8a). `GET`: `{enabled, edge, vpc_pool}` for anyone signed in (whether the shared edge is set, and has a pool for VPC routers); with `readCluster` also `source` (`cluster`, `file`, `none`), `settings`, each node's OVN readiness (`missing`) and the cluster networks. `PUT` (`configureOvn`, recent login, audited): `{enabled, edge?, dns_servers?, nat_supernet?, underlay_mtu?, snat_ct_zones?}`; `400` for options that don't fit together, `409 ovn_not_installed` naming nodes without OVN, `409 cluster_networks_exist` when turning it off with cluster networks left. Every node follows without a restart |
| `GET` | `/cluster/settings/ovn/edge-defaults` | `cluster::edge_defaults` | A suggested shared edge for this cluster (`readCluster`): `{edge, complete, notes}`, the edge with a pool for VPC routers when the LAN has room (spec/clustering-ui.md §8a) |
| `GET` | `/cluster/ovn/tunnels` | `net::cluster_ovn_tunnels` | Each node's Geneve tunnels from its own Open vSwitch (`readCluster`): per node `answered`, `tunnels` (`peer`, `remote_ip`, `state`: `present` / `error` / `bfd_down`, OVS `error`, BFD state and diagnostic, rx/tx packets and bytes) and `missing` peers (spec/clustering-ui.md §8b) |
| `GET` | `/cluster/ovn` | `net::cluster_ovn` | OVN as it is now (`readCluster`): NB and SB members (reachable, leader), chassis (node, tunnel endpoints), each router's HA gateway (active chassis, candidates by priority); `{"enabled": false}` without OVN (spec/clustering-ui.md §8) |
| `GET` | `/networks/{id}/ovn` | `net::network_ovn` | A cluster network on OVN (`readNetwork`, the network's visibility): its switch, ports (VM, addresses, the chassis each is bound to) and router (NAT rules, active gateway) |
| `POST` | `/ovs/bridges/{b}/uplinks/{u}/reapply` | `net::reapply_uplink` | Set up again an uplink netd rolled back at start (`confirmUplink`, recent login): `202` pending commit, as when added (networking.md §8.5). `POST /ovs/bridges/{b}/uplinks` takes `accept_dhcp` |
| `PUT` | `/nodes/{id}/ovn` | `nodes::put_node_ovn` | A node's `br-int` datapath, `{br_int_datapath: auto\|system\|netdev}`, and/or its physical networks `{bridge_mappings: {physnet: bridge}}` (replaces the node's own; the file's still count, the setting wins; no restart) (`configureNode` on its `Host`, recent login, audited); `409 dpdk_not_running`, `409 gateway_node`, `409 vhost_user_vms` (clustering.md §11.6) |
| `GET` | `/departures` | `nodes::list_departures` | Detaches with their state (`frozen`, `committed`, `aborted`), node, mapped networks (ids, since D24) with `network_names` (id → name as they were when the detach started) and whether the node acknowledged (`readCluster`) |
| `GET` | `/nodes`, `/nodes/{id}` | `nodes::list` / `get_node` | `{id}` here and in the node routes below is the node's id or name (a live node's before a departed one's). Nodes and their phase, capacity and features (image responses carry `copies: [{node, node_name, state, bytes, error?}]` in a cluster: each node's copy of the file) |
| `POST` | `/nodes/{id}/{drain,undrain,remove,forget,purge,rejoin-token,detach}` | `nodes::*` | Node lifecycle (`409` with the reason when refused; `detach` takes `map_networks`, `with_access`, `abort`, `departed`, and answers `mapped` and `dropped_networks` by id with their `network_names`) |
| `GET` / `POST` | `/imports`, `/imports/{plan}`, `/imports/{plan}/{approve,reject}` | `nodes::*_import` | Import plans: show (one plan also lists its `sources`: projects, node networks, credentials and VMs it brings), approve with mappings (`dry_run` checks only), reject |
| `GET` | `/watch[?kinds=…][&project=…]` | `watch` | Live stream of VM, disk, image and network changes (server-sent events; below) |
| `POST` | `/vms/{id}/disks` | `attach_disk` | Attach a data disk; on a running VM it takes effect at the next start (`restart_required`) |
| `DELETE` | `/vms/{id}/disks/{disk}` | `detach_disk` | Detach a data disk; on a running VM at the next start, and the disk stays claimed until then |
| `GET` | `/usage[?project=&from=&to=&month=&granularity=&group_by=&meters=&tz=&format=csv]` | `usage::usage` | `GetUsageResponse`: usage records ([metering.md §9.1](metering.md#91-get-usage)): hour/day/month buckets, grouped by project, VM, disk, NIC or network; host readers see every project, others their own (`404` for one they can't read) |
| `GET` | `/projects/{id}/usage`, `/vms/{id}/usage[?include=disks]`, `/disks/{id}/usage` | `usage::*_usage` | The same, for one project, VM (and its NICs) or disk |
| `GET` | `/usage/bandwidth`, `/usage/disk-io`, `/usage/compute` `[?project=&month=YYYY-MM&group_by=&tz=&format=csv]` | `rates::bandwidth` / `disk_io` / `compute` | Per billing month: average, 30-second peak and 5-minute 95th percentile (billable: network max(in, out), disk read+write; CPU and memory: cores, MiB and utilization) ([metering.md §9.3](metering.md#93-bandwidth)) |
| `GET` | `/vms/{id}/bandwidth`, `/networks/{id}/bandwidth`, `/vms/{id}/io`, `/vms/{id}/compute`, `/disks/{id}/io` `[?from=&to=&p95=true]` | `rates::*` | 5-minute series for graphs, `{from, to, points, p95?}` (`/vms/{id}/io` adds `disks`; default: the last 24 h; at most 31 days) |
| `GET` | `/vms/{id}/stats`, `/networks/{id}/stats` | `rates::*_stats` | Current rates from the last two samples (CPU %, memory, Mbps, pps, IOPS, MB/s), `{sampled_at?, resolution_secs, vm?, nics?, disks?}` / `{sampled_at?, resolution_secs, values?}`; not stored |

Image and disk payloads, semantics and invariants are in
[images.md](images.md#8-rest-api).

Tenants and admissions ([tenancy.md](tenancy.md) §14) are REST twins of
`glidex.tenancy.v1`: their bodies are the proto messages as JSON
(`snake_case` names, enum values by proto name, timestamps RFC 3339), and
the same operations are served as RPCs ([grpc.md](grpc.md)):

| Method | Path | RPC | Purpose |
|---|---|---|---|
| `GET` | `/tenants[?page_size&page_token]` | `TenantService.ListTenants` | Every tenant for directory readers, otherwise the caller's own |
| `POST` | `/tenants` | `CreateTenant` | `{slug, display_name?, operator?}` (`directory.tenants`) |
| `GET` / `PATCH` / `DELETE` | `/tenants/{id}` | `GetTenant` / `UpdateTenant` / `DeleteTenant` | By id or slug; delete refuses `409 tenant_in_use` while projects exist, and needs a recent login |
| `POST` | `/tenants/{id}/suspend`, `/resume` | `SuspendTenant` / `ResumeTenant` | A suspended tenant's members can't act on its resources |
| `GET` / `POST` | `/tenants/{id}/members` | `ListMembers` / `AddMember` | Explicit members (`{principal: {type, id}}`) |
| `DELETE` | `/tenants/{id}/members/{user:<id>\|team:<id>}` | `RemoveMember` | |
| `GET` / `POST` | `/tenants/{id}/bindings` | `ListBindings` / `AddBinding` | Tenant roles: `role.tenant-admin`, `role.tenant-auditor` |
| `DELETE` | `/tenants/{id}/bindings/{link}` | `RemoveBinding` | |
| `GET` / `PUT` | `/tenants/{id}/device-pools` | `ListDevicePools` / `SetDevicePools` | Tenant pools on this cluster ([host-topology.md](host-topology.md) §9.1a): `{pools: [{name, from, max, projects}]}` replaces them all; answered with `used`, the cluster pool's `free` and `ungranted`. Actions `readTenantDevicePools` (tenant-auditor) and `manageTenantDevicePools` (tenant-admin) |
| `GET` / `POST` | `/tenants/{id}/invitations` | `InvitationService.ListInvitations` / `CreateInvitation` | Invitations ([tenancy.md](tenancy.md) §6.1): `{email, role, connector, days?}` (`role`: `member` or a tenant role; `connector`: an `oidc` or `google` one; 1-30 days, default 7) → `201 {invitation, link}`, the link (`…/invite/<id>#<secret>`) answered once; listed without secrets, open ones and taken or expired ones for 30 days |
| `DELETE` | `/invitations/{id}` | `RevokeInvitation` | `204`; revoking deletes it |
| `GET` / `POST` | `/tenants/{id}/federations` | `FederationService.ListFederations` / `CreateFederation` | Workload federations ([tenancy.md](tenancy.md) §9, D49–D51): `{federation: {id, display_name, issuer, audience, enabled, rules: [{match, service_account, ttl_secs}]}}`; `readTenantFederations` / `manageTenantFederations` (step-up). The issuer is read at once (`422 issuer_unreachable`); rules pin `repository`, `project_path` or `sub`, exact values only, and name service accounts of the tenant's projects (`422 invalid_federation`) |
| `GET` / `PATCH` / `DELETE` | `/tenants/{id}/federations/{federation}` | `GetFederation` / `UpdateFederation` / `DeleteFederation` | PATCH `{changes: {…}, update_mask?}`: without a mask, the fields present are the ones changed |
| `GET` | `/directory/keys` | `SigningKeyService.ListSigningKeys` | The directory's issuer URL and signing keys ([tenancy.md](tenancy.md) D52, D53): `{keys: [{kid, alg, created_at, retired_at, published_until, active}], issuer, issuer_fixed}`; `readSigningKeys` |
| `POST` | `/directory/keys/rotate` | `RotateSigningKey` | `{key}`; `rotateSigningKey` (step-up) |
| `GET` | `/d/{dir}/.well-known/openid-configuration`, `/d/{dir}/jwks.json` | — | Public, HTTP only: discovery and JWKS (D53); `404` for a directory this host doesn't serve, `503 no_issuer` when it has no public address |
| `GET` | `/d/{dir}/authorize` | — | Public, HTTP only: the code flow with PKCE for `gxctl` (D54); `303` to the loopback `redirect_uri` with `code` and `state`, or to `/?next=…` without a session |
| `POST` | `/d/{dir}/device_authorization`, `/d/{dir}/token`, `/d/{dir}/revoke` | — | Public, HTTP only, form-encoded, OAuth errors (D55, D57) |
| `POST` | `/d/{dir}/device/inspect`, `/d/{dir}/device/answer` | — | Signed in (a browser session; a token is refused): what a user code is for, and approving or denying it |
| `GET` | `/admissions` | `AdmissionService.ListAdmissions` | The cluster operator's (`listAdmissions`) |
| `POST` / `GET` | `/admissions/offers` | `AdmissionService.OfferAdmission` / `ListOffers` | [tenancy.md](tenancy.md) D64: `{tenant, directory, cap?, days?}` → `201 {offer: {id, tenant, directory, cap, by, created_at, expires_at, redeemed_at?}, token}`; `manageAdmissions` (step-up) / `listAdmissions` |
| `DELETE` | `/admissions/offers/{id}` | `RevokeOffer` | `manageAdmissions` (step-up) |
| `GET` / `POST` | `/directories` | `DirectoryService.ListDirectories` / `CreateDirectory` | The directories this host serves ([tenancy.md](tenancy.md) D59): `listDirectories` / `manageDirectories` (step-up, `cluster.directories` of `role.system-admin`); create `{id, name, owner, admin_email, days?}` → `201 {directory: {id, name, issuer, created_at}, invitation_link}` |
| `DELETE` | `/directories/{id}` | `DeleteDirectory` | Only while it has no user, connector or project (`409 directory_not_empty`); never `local` |
| `PUT` / `DELETE` | `/admissions/{tenant}` | `SetAdmission` / `RemoveAdmission` | `{cap?, suspended?}`; needs a recent login |

Login connectors ([tenancy.md](tenancy.md) §13–§14) are REST twins of
`glidex.directory.v1.ConnectorService` in the same way. Managing them
needs `role.directory-admin` and membership of the directory's owner
tenant (D7); every write needs a recent login. Secrets go in through
`PUT …/secrets/{field}` and never come back: responses carry only
`secrets: [{field, set, updated_at}]`.

| Method | Path | RPC | Purpose |
|---|---|---|---|
| `GET` | `/connectors` | `ConnectorService.ListConnectors` | The directory's connectors, file-defined ones (`file-pam`, `file-oidc`, `source: file`) included |
| `POST` | `/connectors` | `CreateConnector` | `{connector: {id, display_name?, enabled?, jit?, required_groups?, require_mfa?, group_teams?, default_tenant?, token_max_days?, pam: {allowed_groups} \| oidc: {issuer, client_id, scopes?, groups_claim?, allowed_domains?, redirect_uri?, mfa_acr_values?} \| google: {accounts?, client_id, domains?, allowed_emails?, device_client_id?, trust_2sv?, redirect_uri?} \| jumpcloud: {client_id, groups_claim?, allowed_emails?, redirect_uri?}}}`; `422 invalid_connector` (D22: `jit` into the owner or an operator tenant needs `required_groups`; such an OIDC or `google` connector has `require_mfa` on unless set, and a `google` one there needs `GOOGLE_ACCOUNTS_WORKSPACE` with `trust_2sv`. `google`: Workspace needs `domains`, never a public mail domain; `GOOGLE_ACCOUNTS_ANY` with `jit` needs `allowed_emails`; tenancy.md D39, §5.1) |
| `GET` / `PATCH` / `DELETE` | `/connectors/{id}` | `GetConnector` / `UpdateConnector` / `DeleteConnector` | PATCH `{connector: {…}, update_mask?}`: without a mask, the fields present are the ones changed. File-defined connectors are read-only (`409`). `provisioning` (`none`, `scim` for `oidc` and `jumpcloud`, which needs `jit` off) and `group_mode` (`mapped`, `auto` for a provisioned one) are fields like the others (tenancy.md §8.1). `DELETE ?users=disable|detach` (default `disable`) says what becomes of the users and teams it provisions, and answers `{users, teams}`: how many (§8.3) |
| `PUT` | `/connectors/{id}/secrets/{field}` | `SetSecret` | `{value}`; the kind's secret fields only (`oidc`, `jumpcloud`: `client_secret`; `google`: `client_secret`, `device_client_secret` for its device client); sealed with the directory's data key |
| `POST` | `/connectors/{id}/test` | `TestConnector` | `{steps: [{name, ok, message}]}`: PAM reaches authd; OIDC and `google` check the secrets, discovery and the device grant (`google`: whether it has a device client) |
| `POST` | `/connectors/{id}/sync` | `SyncConnector` | `{confirm?}`. A pulled connector's run now (`syncConnector`; `409 not_pulled` otherwise, `409 sync_running` while one runs); answers `{status: {state, started_at, finished_at, changes: {created, adopted, updated, disabled, enabled}, error?, would_disable, last_applied_at}}`. `confirm: true` runs once without the safety stop, and takes `confirmConnectorSync` (step-up) too (tenancy.md §8.2, D46). `GET /connectors/{id}` carries the last `sync_status` |
| `GET` | `/connectors/{id}/scim-tokens` | `ListScimTokens` | `{tokens: [{id, name, created_by, created_at, expires_at}], path}`, never the tokens; `path` is `/scim/v2/c/{id}` on this server (and under the web UI's `/api`), tenancy.md §8.1 |
| `POST` / `DELETE` | `/connectors/{id}/scim-tokens`, `…/{token}` | `CreateScimToken` / `RevokeScimToken` | `manageScimTokens`, step-up. POST `{name?, days?}` (1–365, default 180) answers `201 {token, secret, path}`: the `gxs_` secret once, only its SHA-256 kept; `409 not_provisioned` unless `provisioning: scim`; at most five live per connector. A deleted connector's tokens go with it |
| any | `/scim/v2/c/{id}/…` | — (SCIM 2.0, RFC 7644; HTTP only, D29) | The connector's IdP, with its `gxs_` token (not a principal: the global layer leaves it to the handlers). `/Users`, `/Groups` (GET with `eq` filters and pages ≤ 200, POST, and by id GET, PUT, PATCH, DELETE), `/ServiceProviderConfig`, `/Schemas`, `/ResourceTypes`; `application/scim+json`, SCIM error bodies; `401` for any wrong token, `403` unless `provisioning: scim`, `429` past 20 requests/s, `413` past 1 MiB (tenancy.md §8.1) |

Logging in through a connector, all public: `GET /auth/methods` lists the
enabled ones (`connectors: [{id, kind, display_name}]`); `POST
/auth/discover {hint}` says which to offer for a tenant slug or email;
`GET /auth/c/{id}/start`, `/auth/c/{id}/callback` (browser), `POST
/auth/c/{id}/device`, `/auth/c/{id}/device/poll` (gxctl). `POST
/auth/c/{id}/start {return_to?, invitation?}` answers `{redirect}`
instead of redirecting, for a page that brings an invitation's secret
(tenancy.md §6.1; `404` for an invitation that isn't open or is for
another connector), and `POST /auth/invitation {invitation}` says what
an invitation is for (`LoginService.InspectInvitation`). `POST
/auth/link/c/{id}/start {return_to?}` adds a sign-in through connector
`id` to the signed-in caller (`manageOwnSignIns`, a recent login;
tenancy.md §6.2), answering `{redirect}`; its callback returns to
`return_to` with `?linked=<id>` or `?link_error=<code>`. `DELETE
/users/{id}/identities/{key}[?end_sessions=true]` removes one of a
user's sign-in methods (one's own, or with `manageUsers`), never the
last (`409 last_identity`) unless the user is disabled. `POST
/auth/login` and `POST /auth/token` take `connector` (a PAM connector,
the first one when absent); a token is clamped to the connector's
`token_max_days`. A refused login is `403 login_not_allowed`. The
`/auth/oidc/*` routes remain for the first OIDC connector. A `google`
connector's device grant uses its device client; without one, `POST
/auth/c/{id}/device` answers `400 device_flow_unsupported`.

The login operations are also `glidex.auth.v1.LoginService` RPCs
(`Discover`, `StartDevice`, `PollDevice`, `PasswordToken`), one
implementation with the REST routes above, which take and answer the
same messages: `POST /auth/token` answers `{issued: {token, token_id,
expires_at, user_id, display_name}}`, and a device poll `202
{pending: true}` until the user is done, then `200 {issued: …}`.

Host networks are granted to projects or to whole tenants: `POST
/networks` and `PUT /networks/{id}/grants` take `tenants` (ids or
slugs) next to `grants` and `all_projects`, and networks show
`tenant_grants`. A PCI device's `pci.allow` entry takes `tenants` too.
Project networks are shared only between projects of one tenant: an
offer to another tenant's project answers `202` like one to a missing
project, and records nothing.

`POST /cluster/rotate-secrets-key` (`rotateSecretsKey`, step-up) rotates
the key that wraps the directories' data keys and answers `{kek_id,
servers, rewrapped}`; `409` when the key is the `secrets-kek` credential.

`POST /projects` takes `tenant` (id or slug, default `default`) and needs
`createProject` on that tenant. Creating a VM, disk, project network or
router, and starting a VM, also need the project's tenant admitted here:
`403 not_admitted` otherwise; going over the admission's cap is `403
tenant_quota_exceeded`. Granting a host role through `/system/bindings`
needs the principal to be a member of the operator tenant: `422
not_operator_member` otherwise. With `node` (id or name), the role is
linked to that node's `Host` instead of the `Cluster` (spec/clustering.md
§12.2): only `role.auditor`, `role.net-admin` and `grant.host-paths`
(`400` for the others), `404` for an unknown node, `409 node_departed`
for one that has left. `GET /system/bindings?node=` lists one host's.

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
| no `wait` | `202` with `{vm}` (`201` for a create) |
| converged (`observed_generation ≥` the written generation and `Ready=True`) | `200` with `{vm, settled: true}` (`201` for a create) |
| the reconcile of that generation failed (`state: failed`) | the error envelope the call used to return synchronously, chosen from the `Ready` reason, with `details.reason` and the VM in `details.vm` (D20) |
| timeout, including while waiting on a dependency | `202` with `{vm, settled: false}` |
| the VM went away meanwhile | `204` |

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
works off its finalizers, or with `?wait`, `204` once the record is
gone (`200` before the gRPC port). A deleting VM refuses other writes
with `409 conflict`.

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

`DELETE /images/{id}` and `DELETE /networks/{id}` are admitted
synchronously (`409 conflict` while a disk uses the image as its
backing file, waits for it or clones from it; while any VM uses the
network) and record `deletion_requested_at`; the image or network
controller finishes the deletion. Both accept `?wait=<secs>`:

| Outcome | Response |
|---|---|
| gone at once (the normal case for an image) | `204` |
| no `wait`, still deleting | `202` with the image (`deleting: true`) or network (`deletion_requested_at`, `phase`, `conditions`) |
| gone within `wait` | `200` (a network: `204`) |
| timeout (e.g. netd unreachable) | `202` as above |

While deleting, an image can't be the source of a new disk or be
retried, and a network can't be attached to; recreating either under
the same name is `409 conflict` until it is gone. Details:
[images.md §8](images.md#8-rest-api), [networking.md](networking.md).

Image, disk and network events (`GET /{images,disks}/{id}/events`,
`GET /networks/{id}/events`) have the shape of the VM's below.

## Live stream: `GET /watch`

`GET /watch[?kinds=vms,disks,images,networks,nodes,imports,cluster,policy][&project=<id>]` answers
`text/event-stream` (server-sent events); `glidex.watch.v1.WatchService.Watch`
is the same stream over RPC (grpc.md §9.1). Any authenticated caller may
open it; each kind is filtered exactly like its list endpoint (VMs by
`readVm`, disks by `readDisk`, both in visible projects and narrowed by
`project`; images need `readImage` and networks `readNetwork` on
`Host`, with the view of `GET /networks`). `kinds` takes singular or
plural names (default: the four above); an unknown kind is `400 invalid`.
VM and disk objects (here and in their endpoints) carry `node` and
`node_name`, the name of the node they are on, for callers who can't list
nodes. Asked for by name only (spec/clustering-ui.md §3.7):

| Kind | Sent to | `id`, `object` |
|---|---|---|
| `node` | `listNodes` on `Cluster` | node id, the record of `GET /nodes/{id}` |
| `import` | `listImports` on `Cluster` (clustered hosts) | plan id, the view of `GET /imports` (without `sources`) |
| `cluster` | `readCluster` on `Cluster` (clustered hosts) | cluster id, `GET /cluster/status` without port checks and without the Raft indexes that move on every write |
| `policy` | everyone | `policy`, `{"generation": n}`: changes whenever policies or role links are installed, so clients re-run their capability checks |

| Event | Data |
|---|---|
| `added` | `{"kind": "vm\|disk\|image\|network\|node\|import\|cluster\|policy", "id": "…", "object": {…}}`: one per visible object on connect, then for each new one |
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
  `initrd_path` adds an initramfs to `kernel_image_path` (a host path,
  `useHostPath`). `kernel_args` defaults to `console=ttyS0 root=/dev/vda
  reboot=k panic=1` (`console=ttyAMA0` for a VM on an aarch64 node: a
  kernel image's architecture, else the pinned node's, else the
  host's); at most 2047 bytes, no control characters. The last
  `console=` picks the console device: `hvc0` is the virtio console,
  anything else the serial port.
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
  so prefer absolute or `~` paths. `kernel_image_path` must then be
  omitted (`400 invalid_config` with both); `kernel_args` is ignored. The
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
- `power` (`POWER_STATE_STOPPED` by default, `POWER_STATE_RUNNING`,
  `POWER_STATE_PAUSED`) is the desired state; with running one call
  creates and starts the VM. `restart_policy`
  (`RESTART_POLICY_ON_FAILURE` default, `RESTART_POLICY_NEVER`),
  `on_host_boot` (`HOST_BOOT_POLICY_RESUME` / `HOST_BOOT_POLICY_STOP`,
  default from `reconcile.on_host_boot`) and `stop_grace_secs` (0–300,
  default 0 = hard stop) are optional. `hypervisor` is
  `HYPERVISOR_CLOUD_HYPERVISOR` or `HYPERVISOR_QEMU`.
- Paths that reach the hypervisor command line must not contain `"` or
  control characters, and with the systemd runner must not be under
  `/tmp` or `/var/tmp` (`400 invalid_config`).
- Response: `201 Created` with `{vm}` (see `?wait` above).

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

### `Vm`

The `glidex.vm.v1.Vm` message ([grpc.md](grpc.md) §9.1): lists are
`{vms}`, one VM `{vm}`, writes `{vm, settled}`.

```json
{ "id": "…", "name": "web-1", "project": "…",
  "state": "VM_STATE_RUNNING", "desired_state": "POWER_STATE_RUNNING",
  "generation": "4", "observed_generation": "4", "resource_version": "17", "restart_required": false,
  "conditions": [ { "kind": "Ready", "status": "CONDITION_STATUS_TRUE", "reason": "Converged", "last_transition_at": "2026-10-17T01:00:00Z" } ],
  "last_exit": { "at": "2026-10-16T22:13:20Z", "instance_id": "…", "cause": "EXIT_CAUSE_CRASHED", "signal": 9 },
  "restart_policy": "RESTART_POLICY_ON_FAILURE", "on_host_boot": "HOST_BOOT_POLICY_RESUME",
  "vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": "HYPERVISOR_CLOUD_HYPERVISOR", "…": "…" }
```

`state` is `VM_STATE_` and one of `CREATED`, `STARTING`, `RUNNING`,
`PAUSED`, `STOPPING`, `STOPPED`, `FAILED`, `UNKNOWN`
([data-model.md](data-model.md#powerstate-vmphase-and-the-derived-vmstate));
`desired_state` a `POWER_STATE_`. `conditions` and their reasons are
the closed catalogue of
[reconciliation.md §7.5](reconciliation.md#75-exit-handling);
`last_exit.cause` an `EXIT_CAUSE_` (`REQUESTED`, `TERMINATED`,
`CLEAN_EXIT`, `CRASHED`, `LAUNCH_FAILED`, `HOST_REBOOT`, `LOST`).
`generation`, `observed_generation` and `resource_version` are 64-bit,
so strings. Fields at their default are left out (`stop_grace_secs: 0`,
empty lists); `deleting: true` appears while a deletion is in progress.
`GET /vms/{id}?view=full` still returns the stored record, the store's
own format.

### `PATCH /vms/{id}`

`UpdateVmRequest`: a change and the fields it applies.

```json
{ "change": { "power": "POWER_STATE_RUNNING", "stop_grace_secs": 30, "mem_size_mib": 4096,
              "data_disks": ["data-1"] },
  "update_mask": "power,stop_grace_secs,mem_size_mib,data_disks,credential" }
```

The fields: `power`, `restart_policy`, `on_host_boot`,
`stop_grace_secs`, `vcpu_count`, `mem_size_mib`, `kernel_args`,
`credential` (in the mask but absent: removed), `hugepages`,
`hugepage_size` (`2M`, `1G`, or empty for the node's default),
`numa_policy`, `cpu_policy` (host-topology.md §6),
`vfio_devices`, `data_disks` (ids or names; replaces the list),
`networks`. Without `update_mask` the fields present in `change` are
the ones changed. Another path is `400 invalid_config`.
`expected_resource_version` (or `If-Match`) and `?wait` as above.

When each change takes effect is in
[data-model.md](data-model.md) (mutability); changes that need a new
launch set `restart_required` while the VM runs. Each changed field is
authorized separately (security.md §7.4). vCPU and memory increases are
checked against the project quota like a create.

### `GET /vms/{id}/events`

```json
{ "events": [ { "at": "2026-10-17T01:08:40Z", "actor": "guest", "kind": "EVENT_KIND_WARNING",
                "reason": "CleanExit", "message": "guest powered off; desired state set to stopped" } ] }
```

`actor` is `api:<principal>`, `controller`, `guest`, `systemd` or
`host`; `kind` is `EVENT_KIND_NORMAL` or `EVENT_KIND_WARNING`. Needs
`readVm`.

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
opening the WebSocket. `POST /vms/{id}/console/ticket` answers
`{ticket, expires_in}`; `GET /vms/{id}/console/log` stays raw bytes
(`application/octet-stream`; the RPC `GetConsoleLog` answers `{data}`).
The WebSocket itself stays one until G4 ([grpc.md](grpc.md) §9).

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

(`glidex.vm.v1.VmService.Console` is the same console as a bidi RPC
stream over HTTP/2, for API clients and gxctl; grpc.md §9.1.)

`GET /vms/{id}/console/ws[?ticket=…]` upgrades to a WebSocket
(`api/vms.rs::console_ws`, then `bridge_stream`):

1. Checks access (`openConsole`): an unknown or unreadable VM is `404`.
   A browser session (PAM or OIDC login) needs `?ticket=` with a fresh
   single-use ticket for this VM from `POST /vms/{id}/console/ticket`
   (valid 30 s, security.md §5.6), else `403 ticket_required`; tokens
   and the API socket need none.
2. Connects to the console: the VM's console socket in its private
   runtime directory (never exposed to clients), or, for a VM on another
   node, that node's relay. An unreachable node answers `502
   node_unreachable` before the upgrade; a local connect failure is
   sent after it, as a `Message::Text` with the error and then
   `Message::Close`.
3. Copies both ways in a `select!` loop:
   - Bytes read from the console (up to 4 KiB at a time) → sent as
     `Message::Binary`.
   - `Message::Binary` / `Message::Text` from the client → written to
     the console. Close / error / `None` from the client ends the loop.

### Client expectations

- Use `binaryType = "arraybuffer"` on the `WebSocket`. The server
  only ever sends binary frames (except the very first frame in
  the connect-failed case, which is text).
- Writes are sent as binary; the server accepts either binary or
  text (text is treated as the UTF-8 byte sequence of its content).
- No framing — this is a byte stream of terminal data in both
  directions. The *only* transport semantics are WebSocket
  boundaries, which are irrelevant to the consumer.
- No control messages: there is no resize (the terminal size is not
  carried, console.md "Known gaps").
- A ticket is used up by the connect: each new connection, a reconnect
  included, needs a new one.

### Replay-on-connect behavior

The console Unix socket is listened on by a proxy thread in the VM's
`glidex-vm-shim`. That thread replays the current console log (which
spans earlier instances of the VM, separated by `--- glidex: instance …
started … ---` lines) to every newly-accepted client before starting
live broadcast. So opening a WebSocket on a VM that has already
booted will immediately flush the boot-time output into your terminal.
See [console.md](console.md).
