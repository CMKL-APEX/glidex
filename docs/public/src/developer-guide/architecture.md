# Architecture

This page is a map of Glidex for contributors: which processes run,
how a request travels through them, where state lives and how it turns
into running VMs. Each section names the crates and modules involved and
links to the design spec with the details. For why things are built this
way, see [Design decisions and trade-offs](design-decisions.md); for the
algorithms (scheduling, reconciliation, Raft write sets, metering), see
[Algorithms](algorithms.md); for the repository layout, see
[Repository and code organization](organization.md).

Source paths below are relative to the repository root and link to
`main`. The design specs live in
[`spec/`](https://github.com/CMKL-APEX/glidex/blob/main/spec/README.md).

## The big picture

Glidex is a set of ordinary Linux processes. Nothing is containerized.
The work is split by privilege: the control plane runs as an
unprivileged user, and each thing that needs root is a small helper
daemon with a narrow protocol on a Unix socket.

```text
   Browser                         gxctl
      │ HTTPS :5173                  │ api.sock (peer uid)  or  HTTPS :8841 (token)
      ▼                              │
 ┌──────────────┐  ui.sock           │
 │  glidex-ui   │────────────┐       │
 │ (glidex-ui)  │            ▼       ▼
 └──────────────┘   ┌──────────────────────────────────────────────┐   :8842 mTLS
                    │  glidex-control-plane      (user glidex)     │◀──────────▶ other nodes
                    │  REST + RPC API · auth · Cedar · store       │
                    │  controllers · scheduler · hypervisor drivers│
                    └──┬──────────┬───────────┬───────────┬────────┘
          netd.sock    │ stord.sock│ auth.sock │ D-Bus     │ shim.sock, api.sock,
                       ▼          ▼           ▼ (polkit)  │ console.sock (per VM)
              ┌────────────┐ ┌───────────┐ ┌───────────┐  │
              │glidex-netd │ │glidex-    │ │glidex-    │  │
              │  (root)    │ │stord(root)│ │authd(root)│  │
              └─────┬──────┘ └─────┬─────┘ └─────┬─────┘  │
                    ▼              ▼             ▼        ▼
             OVS, nftables,    LVM-thin        PAM    ┌───────────────────────────────┐
             dnsmasq, OVN      volumes                │ glidex-vm@<id>.service        │
             chassis, VFIO                            │ (user glidex, glidex-vms.slice)│
                                                      │  glidex-vm-shim ── PTY,       │
                                                      │   │  console.sock, console.log│
                                                      │   └─ cloud-hypervisor / qemu  │
                                                      └───────────────────────────────┘
```

| Process | Runs as | Crate | Job |
|---|---|---|---|
| `glidex-control-plane` | `glidex` (`glidex-control-plane.service`) | [`glidex-control-plane`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-control-plane) | API, authentication, authorization, the store, controllers, scheduler, cluster membership |
| `glidex-netd` | root (`glidex-netd.service`) | [`glidex-netd`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-netd), on the [`glidex-ovs`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-ovs) library | Everything privileged on the host network: OVS bridges and uplinks, NAT (nftables + dnsmasq), VM ports, joining OVN, binding devices to `vfio-pci` |
| `glidex-stord` | root (`glidex-stord.service`) | [`glidex-stord`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-stord) | Makes, activates and removes LVM-thin volumes in the volume groups the operator allows, and hands each device to the `glidex` user |
| `glidex-authd` | root, socket-activated (`glidex-authd.socket`) | [`glidex-authd`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-authd) | Checks local users' passwords with PAM; one operation, `authenticate` |
| `glidex-vm-shim` | `glidex`, one `glidex-vm@<id>.service` per running VM | [`glidex-vm-shim`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-vm-shim) | Parent of one hypervisor; owns its PTY, console socket and console log; records how it exited |
| `cloud-hypervisor`, `qemu-system-*`, `pylon` | child of the shim | upstream (Pylon: a Cloud Hypervisor fork, installed with `--with-pylon`) | The VM itself |
| `glidex-metad` | `glidex-metad` (`glidex-metad.service`) | [`glidex-metad`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-metad) | The metadata endpoint guests reach at `169.254.169.254`: checks each request and asks the control plane to answer for the VM behind the port it came from. Holds no key |
| `pylond` | root (`glidex-pylond.service`), only with Pylon | the separate Pylon project | Pylon's core arbiter: Pylon VMs register with it; the control plane reads their statistics and sends policy updates |
| `glidex-ui` | `glidex-ui` (`glidex-ui.service`) | [`glidex-ui`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-ui) | Serves the React UI over HTTPS and proxies `/api/*` (WebSocket upgrades included) to the control plane's `ui.sock` |
| `gxctl` | the user | `crates/glidex-control-plane/src/bin/gxctl` | CLI on the generated RPC clients |
| `glidex-install` | root, once | [`glidex-install`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-install) | Installs binaries, users, units and the polkit rule; creates or joins a cluster |

Units are in
[`packaging/`](https://github.com/CMKL-APEX/glidex/tree/main/packaging).
`glidex-vfio-boot.service` is a one-shot (`glidex-netd --boot-devices`)
that sets SR-IOV VF counts and `vfio-pci` bindings at boot, before netd
and the control plane start.

### Sockets and ports

| Endpoint | Served by | Used by |
|---|---|---|
| TCP `:8841` (HTTPS, every address by default) | control plane | gxctl with a token, the UI's TCP fallback, other API clients |
| `/run/glidex-cp/api.sock` | control plane | gxctl on the host; the caller is identified by its peer uid |
| `/run/glidex-cp/ui.sock` | control plane | `glidex-ui` only (checked by peer uid) |
| TCP `:8842` (HTTPS, mutual TLS with the cluster CA) | control plane, in a cluster | other nodes: Raft, forwarded writes, watch, heartbeats, status writes, console and stats relays |
| TCP `:5173` (HTTPS) | `glidex-ui` | browsers |
| `/run/glidex/netd.sock`, `netd-ro.sock`, `netd-admin.sock` | netd | control plane (full); anyone (status only); the admin group |
| `/run/glidex-stord/stord.sock`, `stord-admin.sock` | stord | control plane; the admin group |
| `/run/glidex-authd/auth.sock` | authd | control plane |
| `/run/glidex-cp/vms/<id>/{shim.sock, api.sock, console.sock}` | shim, hypervisor, shim | control plane only |
| `/run/glidex-cp/metad.sock` | control plane | `glidex-metad` only (checked by peer uid) |
| `169.254.169.254:8775` | `glidex-metad` | guests: netd's rules DNAT `169.254.169.254:80` from each VM port on node NAT networks here, with the port's own peer address |
| `/run/pylon/pylond.sock` | `pylond` | Pylon hypervisors; the control plane (statistics, policy updates) |
| system D-Bus | systemd | control plane starts and stops `glidex-vm@<id>` units (allowed by the polkit rule `50-glidex-vm.rules`) |

Socket and port numbers are configurable; these are the defaults. The
helpers' protocols are newline-delimited JSON; the clients live in the
helper crates (`glidex_netd::client`, `glidex_stord::client`,
`glidex_authd::client`) so the control plane and the daemon share one
definition. The clients block; a call can take seconds (netd up to 30 s,
LVM commands, hyperstor's API), so the control plane makes them through
`glidex_raftstore::wait::blocking`, which on a Tokio worker waits in
`block_in_place` and leaves the runtime free to drive I/O.

### Why VMs survive control-plane restarts

The control plane never spawns a hypervisor and never holds a PTY. For
each VM it starts a `glidex-vm@<id>` unit through systemd. The unit runs
`glidex-vm-shim`, which starts the hypervisor as its child, in the
unit's own cgroup. The control plane reaches a running VM only through
files and sockets in the VM's runtime directory,
`/run/glidex-cp/vms/<id>/`:

| File | Written by | Content |
|---|---|---|
| `launch.json` | control plane | Everything the shim needs: the hypervisor command line (the whole VM config, built by the hypervisor driver), paths, grace periods |
| `instance.json` | shim | What the shim reports: phase, pids with start times, boot id, exit record |
| `shim.sock` | shim | `status`, `stop`, `kill`, `release` |
| `api.sock` | hypervisor | Cloud Hypervisor's HTTP API or QEMU's QMP, reached with [`glidex-hv-client`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-hv-client) |
| `console.sock`, `console.log` | shim | The serial console and its log |

So stopping, crashing or upgrading the control plane (or netd, whose
VM ports and vhost-user sockets also outlive it) leaves guests running.
On the next start it observes every VM and adopts the live instances
before acting on any (see [Reconciliation](#reconciliation)). The
runtime directory survives because the unit sets
`RuntimeDirectoryPreserve=yes`. With the `detached` runner (development
and tests) the shim runs in its own session instead of a unit; the
picture is otherwise the same.

Design:
[12-reconciliation.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/12-reconciliation.md)
§5, §8, and
[00-architecture.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/00-architecture.md).

## The request path

Every API request, REST or RPC, goes through the same layers of one
axum router (`api::router` in
[`api/mod.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/api/mod.rs)).

```text
 listener (serve.rs)        tags the request: Tcp | Api | Ui | Cluster | Relay,
   │                        peer uid (Unix sockets) or client address (TCP)
   ▼
 relay::ui_relay            glidex-ui's self-report; X-Glidex-Target-Node for host-only
   │                        calls; on an agent, ui.sock answers 421 use_a_server
   ▼
 authenticate               Origin/CSRF checks, then a Principal from: peer uid (api.sock),
   │                        session cookie, gxt_ token or directory access token (TCP, ui.sock),
   │                        X-Glidex-Principal (a server forwarding over :8842)
   ▼
 gate::cluster_gate         cluster only: write on a follower → forward to the leader;
   │                        read → ReadIndex barrier, unless X-Glidex-Consistency: local
   ▼
 ┌─────────────────────────────┬──────────────────────────────────┐
 │ REST route (api/*.rs)       │ RPC (api/rpc/*.rs, connectrpc)   │
 │ per-route Cedar action,     │ operation from glidex_api::      │
 │ audit_layer, remote_host    │ OPERATIONS, per-listener rules,  │
 │ decode JSON → proto message │ audit; gRPC, gRPC-Web, Connect   │
 └──────────────┬──────────────┴──────────────┬───────────────────┘
                └──────────────┬──────────────┘
                               ▼
               service::<family>::<operation>(call, request)
               Cedar checks, not-found vs forbidden, quotas, names, store write
                               ▼
               VmManager (state.rs) / other stores → Db::write
```

| Step | Where | Notes |
|---|---|---|
| Listeners | [`serve.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/serve.rs), [`glidex-tls`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-tls) | TCP is HTTPS unless TLS is off, which only loopback listeners allow. The Unix sockets tag each request with the peer's uid from `SO_PEERCRED` |
| Authentication | `authenticate` in `api/mod.rs`, [`auth/`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-control-plane/src/auth) | Every request gets a `Principal`. Credentials accepted depend on the listener: cookies only through `glidex-ui`, peer uid only on `api.sock` |
| Authorization | [`authz.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/authz.rs) (Glidex's `Model` over the engine in [`glidex-auth`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-auth/src/cedar.rs)), policies in [`crates/glidex-control-plane/policies/`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-control-plane/policies) | Cedar. The policy set is the shipped base policies and role templates, one template link per role assignment, and enabled site policies, validated against the schema and swapped atomically. A decision allows only if Cedar allows and no policy errored |
| Operation table | [`glidex-api`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-api/src/lib.rs) | Each RPC in `proto/` carries a `glidex.api.v1.op` option: its Cedar action, `READ` or `WRITE`, and its REST twin. `build.rs` turns them into `OPERATIONS`. Tests check that every RPC and REST route maps to a Cedar action in the schema and that twins agree |
| Service layer | [`service/`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-control-plane/src/service) | One transport-neutral function per operation makes every decision. REST handlers in `api/` and RPC adapters in `api/rpc/` only decode, call it and encode, so the two transports can't drift |
| Audit | `audit_layer` / `write_audit` in `api/mod.rs`; RPCs in `api/rpc/mod.rs` | Every write, every denied request and reads a handler marks, into the `audit` table. Logins are audited by their handlers |
| Cluster gate | [`api/gate.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/api/gate.rs) | See below |

REST bodies are the proto3 JSON form of the same messages the RPCs use,
so there is one type system. Some endpoints are HTTP by nature and stay
REST only: OIDC and OAuth endpoints, SCIM, the console WebSocket, CSV
exports, `/health`.

### Cluster forwarding and consistent reads

In a cluster every server answers the API. The gate decides per request:

- **Writes** (a non-safe HTTP method, or an operation of kind `WRITE`)
  on a follower are forwarded whole to the leader over `:8842`, with
  the already-authenticated principal in `X-Glidex-Principal`. The
  leader trusts that header only on a connection with a server
  certificate, and runs authorization itself.
- **Reads** wait for a ReadIndex barrier (batched round trip to the
  leader), so a read on any server sees every write committed before
  it arrived. Without quorum they fail with `503 cluster_unavailable`,
  unless the client asks for a stale read with
  `X-Glidex-Consistency: local` (`gxctl --stale`). Answers carry
  `X-Glidex-Revision`, the Raft index of the last applied entry.
- **Host-only** routes (`/ovs`, `/pci-devices`, `/system/…`, the
  `glidex.host.v1` services, `cluster init/join/leave`) act on the host
  that receives them and are never forwarded. With
  `X-Glidex-Target-Node`, a server authorizes the route's action on that
  node's `Host` and runs it there over `:8842`
  ([`api/relay.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/api/relay.rs)).

Design: [18-grpc.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/18-grpc.md),
[11-security.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/11-security.md) §5, §7, §10,
[14-clustering.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md) §6.4.
Public reference: [API overview](../api/README.md).

## The data model

### Spec and status

Managed objects (VMs, disks, images, networks, nodes, routers) are
envelopes of `meta`, `spec` and `status`, the Kubernetes pattern:

| Part | Holds | Written by |
|---|---|---|
| `meta` | id, name, project, `generation` (+1 on every spec change), `resource_version` (+1 on every write), deletion request, finalizers | API |
| `spec` | Desired state: what the user asked for (`power: running`, vCPUs, disks, networks) | API, inside admission |
| `status` | Observed state: phase, conditions (`Ready`, `Scheduled`, `DisksReady`, …), `observed_generation`, placement, the live instance, NICs, last exit | Controllers only (and the scheduler for placement) |

The API records the spec before it answers, then returns `202`, or, with
`wait`, once the controller has acted. An object has converged when
`observed_generation == generation` and `Ready=True`. Controllers write
`observed_generation` as the generation they *read*, so a spec change
made during a reconcile is never undone and is picked up next round.
Types are in
[`models.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/models.rs).

### The store

All control-plane state is one [redb](https://github.com/cberner/redb)
database, `glidex.db` under the service user's home
(`/var/lib/glidex-control-plane/.glidex/`). Every change goes through
`Db::write`: a transaction that records each `put` and `delete` as a
**write set**. The store and its Raft integration are the
[`glidex-raftstore`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-raftstore)
crate; glidex's schema (the `TableId` list, each table's scope, and the
write `Origin`s) is
[`store/db.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/store/db.rs).
`TableId` is append-only: a table can be retired, its id never reused.

Writes take one **write lane**, a mutex, and in a cluster hold it until
Raft has committed the write. Raft's round trip needs the Tokio
runtime's I/O, so no worker may block outright on the lane, or on any
other lock held across a store write (the network store's write mutex,
the image cache, the directory key lock): if the worker that blocked
was the one driving I/O, the lane's holder never hears back and every
later request queues behind it. Those waits go through
[`glidex_raftstore::wait`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-raftstore/src/wait.rs):
a free lock is taken at once, a held one is waited for in
`block_in_place`.

`VmManager`
([`state.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/state.rs))
keeps an in-memory cache of every VM. Admission (validation, quotas,
disk and device claims, name uniqueness) runs under that cache's write
lock and ends in one store commit with the spec, the claims and an
event. It never touches the host.

### Standalone and replicated

| | Standalone host | Cluster |
|---|---|---|
| Store | Local redb, writes commit directly | Same file, replicated with Raft (`openraft`) among the servers |
| Who writes | the one process | the leader, on one serialized write lane |
| What travels | — | The write set, as a Raft log entry. A write runs once on the leader against its applied state; every member applies the same bytes |
| Reads | local | local replica after a ReadIndex barrier (or stale on request) |
| Agents | — | A filtered cache of the store: only what is placed or bound on them, fed by list and watch from a server |

"The leader computes, everyone applies bytes" is what keeps admission
(read, check, write) correct without running it on every replica, and
lets servers on different versions apply the same entries. The details
are in [Algorithms](algorithms.md) and
[14-clustering.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md) §6.

**What's replicated and what stays on a host.** Every table of
`glidex.db` is replicated except `raft_meta` (each replica's own
`last_applied` and membership). Each table also has a **scope**:
directory tables (users, teams, projects, credentials, connectors,
tokens, …) follow the organization; cluster tables (VMs, disks, images,
networks, nodes, datastores, metering, …) follow the hardware. Auth
tables replicate to servers only. These stay on each host and are never
replicated:

- the Raft log and the cluster identity (node certificate and key, CA)
  in the control plane's state directory;
- configuration files in `/etc/glidex/`;
- netd's own state database and stord's configuration;
- VM runtime directories, console logs, and disk and image files on
  node-local datastores;
- on agents, the outbox of status writes not yet acknowledged.

Design: [03-data-model.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/03-data-model.md),
[12-reconciliation.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/12-reconciliation.md) §6,
[14-clustering.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md) §6.

## Reconciliation

Controllers are level-triggered loops: each round observes the host,
records what it saw in `status`, and takes the smallest step toward the
spec. Every step is idempotent and safe to interrupt, so a crash at any
point is finished or undone by the next round.

```text
 API spec write ──┐
 exit watch (pidfd per shim and hypervisor) ──┤
 store change (cache_sync, list/watch) ──┤──▶ WorkQueue ──▶ N workers ──▶ reconcile_<kind>(id)
 resync every reconcile.resync_secs ──┤      (dedup by     (reconcile.     observe → act → write status
 netd restart (re-sync ports) ──┘            kind, id)      workers)
```

One deduplicating queue
([`controller/queue.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/queue.rs))
feeds the workers: a key is processed by one worker at a time, failures
back off per key, and the periodic resync means a missed event only
delays convergence.

| Controller | Module | Runs on | Does |
|---|---|---|---|
| VM | [`controller/vm.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/vm.rs) | the VM's node | Observe the instance; finish exits and deletions; stop (with escalation); launch; pause/resume and VFIO hot-plug drift; derive phase and conditions |
| Disk | [`controller/disk.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/disk.rs) | the disk's node, or its worker on a shared datastore | Make pending disks once their image is ready, resize, extend the root partition, notice missing files, finish deletions |
| Image | [`controller/image.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/image.rs), [`image_cache.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/image_cache.rs), [`store_copies.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/store_copies.rs) | every node that needs a copy | Supervise downloads; per-node copies; copies inside shared datastores |
| Node network | [`controller/network.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/network.rs) | the network's node | Keep netd's records in line with the control plane's networks |
| Cluster network (OVN) | [`controller/ovn.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/ovn.rs) | leader (NB database); every node (chassis); servers (OVN central) | Keep OVN's northbound database equal to networks, IPAM and placements |
| Scheduler | [`scheduler.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/scheduler.rs), [`controller/placement.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/placement.rs) | leader | Place VMs that have no node yet |
| Node lifecycle | [`cluster/lifecycle.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/lifecycle.rs) | leader | Mark nodes that stop heartbeating, and their VMs, `Ready=Unknown` |
| Datastore probe | [`datastore/probe.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/datastore/probe.rs) | every node | Report what the node finds at each datastore, every minute |
| Metering | [`metering/`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-control-plane/src/metering) | every node; ledger close on the leader | Sample usage into the hourly ledger |

`VmManager::start_controllers`
([`controller/mod.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/mod.rs))
starts the **node role** tasks (workers, resync, datastore probe, device
pool preparation, self-report, OVN chassis) and, on servers, the
**server role** tasks (scheduler pass, cluster network controller). The
leader-only loops do nothing on a follower.

### Startup and adoption

`main.rs` loads the configuration, opens the store, and calls
`VmManager::initialize`
([`controller/startup.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/startup.rs))
before any controller acts or any listener opens:

1. Observe every VM. A live instance that matches `status.instance` is
   adopted as it is (event `Adopted`); one only `instance.json` knows of
   is adopted from it (`AdoptedUnrecorded`).
2. List units and runtime directories with no VM record as orphans.
   They are reported (`GET /system/reconcile`), never touched.
3. Only then tell netd which VMs own ports, so it never drops a port of
   a VM that is still running.
4. Start the controllers, then the listeners.

**Invariant:** at most one hypervisor instance per VM. Before a launch,
the controller must positively establish that no instance is live (unit
state, recorded pids with their start times, sockets). If it can't
tell, the VM is `Unknown` and nothing is launched.

### Host boot

`glidex-vm@` units are never enabled: relaunching VMs after a reboot is
the control plane's decision. The shim records the host's boot id; a
recorded instance from an earlier boot is an exit with cause
`HostReboot`. The VM's `on_host_boot` policy (`resume` by default, or
`stop`, with a site default in `reconcile.on_host_boot`) then decides
whether the controller launches it again or sets its desired power to
stopped. At host shutdown, systemd stops each unit: the shim presses
the guest's power button, waits the grace period, then stops the
hypervisor.

Design: [12-reconciliation.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/12-reconciliation.md) §9–§10, §13.

## Clustering

```text
              gxctl / UI / API clients   (HTTPS :8841 to any server)
       ┌──────────────────┬───────────────────┬──────────────────┐
       ▼                  ▼                   ▼
 ┌────────────┐     ┌────────────┐     ┌────────────┐
 │ server     │◀───▶│ server     │◀───▶│ server     │   Raft, forwarding,
 │ API · Raft │     │ (leader)   │     │ API · Raft │   relays on :8842 (mTLS)
 │ node role  │     │ node role  │     │ node role  │
 │ netd · OVS │     │ netd · OVS │     │ netd · OVS │
 └─────┬──────┘     └─────┬──────┘     └─────┬──────┘
       │      OVN overlay (Geneve) for cluster networks
       └────────────┬───────────────┬────────┘
              ┌─────┴─────┐   ┌─────┴─────┐
              │ agent     │   │ agent     │   node role only: list/watch,
              │ netd · OVS│   │ netd · OVS│   heartbeats and status writes
              └───────────┘   └───────────┘   to a server on :8842
```

**One binary, two roles.** Every host runs the **node role** (VM, disk,
image and node-network controllers, metering sampler, netd sync).
**Servers** also run the **server role**: the API, a Raft member, and
the cluster controllers, which act only while the server is leader. A
standalone host is both roles with a local store and one implicit node,
`local`. Roles are `node::Roles`
([`node.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/node.rs)).

| Topic | How | Code |
|---|---|---|
| Node API | `:8842`: Raft (`/raft/*`), join and rejoin, read index, list and watch, heartbeats, status writes, ledger shipping, image file copies, console/stats relays, console tickets, metadata questions, host-only relays; anything else is a forwarded API request | [`cluster/server.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/server.rs), [`cluster/net.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/net.rs) |
| PKI | One cluster CA whose key every server holds; node certificates name the node and its advertise address. The CA can be rotated, and is when a server leaves | [`cluster/pki.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/pki.rs), [`cluster/rotation.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/rotation.rs) |
| Joining | `gxctl cluster init` makes a one-voter cluster; hosts join with a single-use join token (only its hash is stored) and get a certificate | [`cluster/manage.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/manage.rs), [`cluster/tokens.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/tokens.rs) |
| Node lifecycle | Phases `Active`, `Draining`, `Departing`, `Departed`, `Removed`, `Forgotten`. Remove an empty node, forget a lost one, rejoin as the same node, detach with its VMs, import a standalone host's VMs | [`cluster/membership.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/membership.rs), [`cluster/departure.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/departure.rs), [`cluster/import.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/import.rs) |
| Liveness | Nodes heartbeat every 5 s; the leader keeps liveness in memory and writes only transitions. A silent node and its VMs become `Ready=Unknown`; nothing is stopped or moved | [`cluster/lifecycle.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/lifecycle.rs) |
| Agents | A cache of the store fed by list and watch; status writes go to the leader, queued in an outbox while it is out of reach. Cut off from every server, an agent keeps running VMs but launches and deletes nothing | [`cluster/agent.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/agent.rs), [`cluster/sync.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/sync.rs) |
| Status writes | A node sends the status it computed with the `resource_version` it read; the leader checks the object is placed or bound on that node (`check_status_write`) and writes it | [`cluster/sync.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cluster/sync.rs) |

### Where the scheduler fits

The scheduler is called inside VM admission: in a cluster, `create_vm_with`
asks `scheduler::pick_node` for a node and writes the placement in the
same commit as the VM. If no node fits, the VM is created with
`Scheduled=False/Unschedulable` and the reason for each node, and the
leader's placement pass (`place_pending`, every few seconds) tries again
as nodes and capacity change. Placement is decided once and then sticks;
an administrator can place a stopped or lost VM again. Filters (capacity,
hugepages, NUMA and CPU policy, hypervisor, devices, disks' datastores,
networks, architecture) and scoring are described in
[Algorithms](algorithms.md).

### Agents, the UI and host pages

An agent serves **no API**: auth tables replicate to servers only, so
an agent has no users, sessions or tokens. It removes its `api.sock`
(gxctl logs in to a server instead) and its `ui.sock` answers API
requests with `421 use_a_server`. Its `glidex-ui` reports itself to the
control plane (`PUT /ui/self`) and learns which server's UI to send
browsers to: a `GET` is redirected (`307`) to that server, `/` to the
agent's own **Host page** (`/hosts/<node>`). Servers publish their UI
address in their node record (`cluster.ui_url`, else what their
`glidex-ui` reports). Host pages work from any server for any node by
sending host-only calls with `X-Glidex-Target-Node` (see
[the request path](#cluster-forwarding-and-consistent-reads)). Code:
[`glidex-ui/src/home.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-ui/src/home.rs),
[`api/relay.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/api/relay.rs).

Design: [14-clustering.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md),
[28-host-page.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/28-host-page.md),
[16-clustering-ui.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/16-clustering-ui.md).
Operating it: [Clusters](../operator-guide/clusters.md).

## Subsystems

### Networking

| | Node networks (one host) | Cluster networks |
|---|---|---|
| Datapath | An OVS bridge per network on the host | OVN logical switches over Geneve, VM ports on `br-int` |
| Who writes it | netd, from its own records, using `glidex-ovs` | The leader writes OVN's northbound database through `glidex-ovn` (`ovn-nbctl`); netd makes each host an OVN chassis (and runs OVN central on servers) |
| NAT and DHCP | nftables NAT and a dnsmasq per NAT network, supervised by netd | OVN routers: a shared HA edge router `gx-edge`, or per-project VPC routers `gxr-<id>`, each SNATing on a gateway chassis group |
| Addresses | dnsmasq leases | Cluster IPAM in the store ([`ipam.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/ipam.rs)), written into OVN port addresses |

Flow: the control plane's network records
([`network.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/network.rs))
are the source of truth. The VM controller asks netd to attach each NIC
(`attach_vm_port`: a tap or vhost-user port on the right bridge) before
the launch and records it in `status.nics` first. netd reconciles the
host from its own state database on start; the control plane re-sends
the set of VMs that own ports whenever netd restarts. Provider networks
map to physical networks through each host's bridge mappings. Everything
glidex creates in OVSDB, nftables and OVN is tagged, and only tagged
objects are ever changed or deleted.

Code: [`glidex-netd`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-netd),
[`glidex-ovs`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-ovs),
[`glidex-ovn`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-ovn),
[`controller/network.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/network.rs),
[`controller/ovn.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/ovn.rs).
Design: [09-networking.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/09-networking.md),
[14-clustering.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md) §11.
Operating it: [Host networking](../operator-guide/host-networking.md),
[Networks](../user-guide/networks.md).

### Storage

A **datastore** is where disks live. Its record is cluster-wide; each
node reports what it finds at it. Disks are reached through a driver
that hands back a **volume binding**: a file (qcow2 or raw) or a raw
block device.

| Driver | Topology | Volumes | Privileged part |
|---|---|---|---|
| `dir` (the built-in `local` is one) | node-local | files named by disk id in a directory on each node | none |
| `shared-dir` | shared (NFS, CephFS, … mounted by the operator) | files every reaching node sees; one **worker** node acts on each disk; a file lock records who has a volume open | none |
| `lvm` | node-local | LVM-thin volumes `gx-d-<disk-id>`; linked disks are thin snapshots of an image volume `gx-i-<image-id>` | `glidex-stord` creates, activates and removes volumes and hands the device to the `glidex` user |
| `hyperstor` | shared (a pool of a hyperstor cluster) | volumes managed through the cluster's API over mutual TLS, from servers only; filled and partitioned with `hyperstorctl`; served to the VM over vhost-user-blk by the node's `hyperstor-vhost`; linked disks are clones of an image volume | none in Glidex; hyperstor's own services |

A node-local disk binds to a node at creation or at its VM's first
placement, and pins that VM's placement; a disk on a shared datastore
binds to none, so its VM may run on any node that reaches the
datastore. **Images** have one catalog record and a verified copy per
node that needs one (`image_caches/<image>/<node>`), fetched from a node
that already has it and checked against the digest; a shared datastore
holding linked disks gets its own copy (`…/ds:<datastore>`). Disks can
move between datastores.

Code: [`datastore/`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-control-plane/src/datastore)
(`file.rs`, `lvm.rs`, `hyperstor.rs`, `record.rs`, `probe.rs`, `manage.rs`),
[`images/`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-control-plane/src/images),
[`glidex-stord`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-stord).
Design: [24-storage.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/24-storage.md),
[10-images.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/10-images.md).
Operating it: [Datastores](../operator-guide/datastores.md),
[Images and disks](../user-guide/images-and-disks.md).

### Console

```text
 browser ─WebSocket─▶ glidex-ui ─ui.sock─▶ control plane ──▶ console.sock ◀── shim proxy thread ── PTY ── guest ttyS0/hvc0
 gxctl ──VmService.Console (bidi RPC)──────▶     │                                  └──▶ console.log (rotated)
                                                 └─(VM on another node)─ :8842 /cluster/v1/vms/{id}/console ─▶ that node
```

Each VM has one serial console on a PTY whose master the shim holds. A
proxy thread in the shim owns the PTY, the console socket and the log:
it fans output out to any number of clients and is the log's only
writer. New clients get the log replayed first (minus the terminal
queries a guest leaves in it), and the listener outlives the PTY, so a
dead guest's last output is still visible. The browser gets a
short-lived single-use ticket (`CreateConsoleTicket`), then opens
`/vms/{id}/console/ws`; `api/vms.rs::console_ws` and the RPC share
`service::vm::console_target`/`console_connect`. In a cluster tickets
live in the leader's memory: a follower forwards ticket requests to the
leader, and the server that takes the WebSocket redeems the ticket there
(`POST /cluster/v1/console-ticket`), so the two requests may reach
different servers. For a VM on another node, the server that authorized
the request relays to that node.

Code: [`glidex-vm-shim/src/proxy.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-vm-shim/src/proxy.rs),
[`api/vms.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/api/vms.rs).
Design: [02-console.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/02-console.md).
Using it: [Console](../user-guide/console.md).

### Images, firmware and kernel extraction

- **Cloud images** come from a built-in catalog or a URL. The download
  pipeline streams to a temporary file while hashing, verifies the
  digest and image header, converts raw to qcow2, makes the file
  read-only and renames it into place
  ([`images/download.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/images/download.rs)).
- **Firmware** (EDK2 for Cloud Hypervisor, OVMF for QEMU) is pinned
  and imported as firmware images
  ([`images/firmware.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/images/firmware.rs)).
- **Kernel images** (kernel, initramfs, command line) for direct kernel
  boot can be extracted from a cloud image. The image's filesystems are
  only ever read inside a short-lived **extractor VM**: a pinned kernel
  with an initramfs glidex builds boots with the image attached
  read-only and writes the boot files to a raw disk, which the host
  parses as a checked stream. The run goes through the same runner and
  unit sandbox as a VM
  ([`images/extractor.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/images/extractor.rs),
  [`controller/extract.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/controller/extract.rs)).
- Firmware-booted guests get a NoCloud seed built per launch
  ([`cloud_init.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/cloud_init.rs))
  with the VM's guest credential
  ([`credentials.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/credentials.rs)).

Design: [10-images.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/10-images.md),
[23-kernel-boot.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/23-kernel-boot.md),
[08-credentials.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/08-credentials.md).
Using it: [Kernel boot](../user-guide/kernel-boot.md).

### Hypervisors

Hypervisor drivers
([`hypervisor/`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-control-plane/src/hypervisor):
`cloud_hypervisor.rs`, `qemu.rs`, `pylon.rs`) are stateless. A driver turns a VM's
config into the full command line the shim runs, and performs runtime
operations (observe, pause/resume, VFIO hot-plug) over the hypervisor's
socket with `glidex-hv-client`. Because everything is a path, a
restarted control plane manages instances exactly as before.
**Pylon** is a Cloud Hypervisor fork with fast CPU core elasticity: its
driver reuses Cloud Hypervisor's arguments and API client, adds
`--pylon` with a shared memory page and `pylond`'s socket, and sends
`pylond` policy updates (minimum and maximum cores, weight) for a running
VM. A node reports whether `pylond` runs. Design:
[04-hypervisors.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/04-hypervisors.md),
[30-pylon.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/30-pylon.md).

### Devices

- **PCI and VFIO**: the control plane scans sysfs read-only
  ([`pci.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/pci.rs));
  binding a device to `vfio-pci` (and SR-IOV VFs) is netd's job, at
  boot through `glidex-vfio-boot.service` or on demand. VM units may
  open only the device nodes VMs use.
- **Topology**: each node's NUMA nodes, CPUs, per-node memory and
  hugepages, and the NUMA node of each device, as a small summary in
  the node's replicated status
  ([`topology.rs`](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-control-plane/src/topology.rs));
  the full PCIe tree on request. The scheduler uses it for NUMA and
  dedicated-CPU policies.
- **Pools**: `pci.pools` in the configuration groups devices (GPUs,
  NICs) that VMs ask for by count; tenants get shares of them
  (`tenant_device_pools`). Pools marked `prepare` are bound to
  `vfio-pci` on each node at start.

Design: [22-host-topology.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/22-host-topology.md),
[27-gpu-vms.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/27-gpu-vms.md).
Operating it: [PCI passthrough](../operator-guide/pci-passthrough.md),
[Host resources](../operator-guide/host-resources.md).

### Identity

Every request ends in a `Principal`, and every login, whatever the
source, ends in one external identity that `AuthService` maps to a
user.

| Source | How | Code |
|---|---|---|
| Local socket | `api.sock` peer uid → a `unix:` identity | `auth/mod.rs` |
| Local accounts | The `pam` connector sends the password to `glidex-authd`, which runs PAM as root and answers yes or no | [`glidex-authd`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-authd), [`directory/`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-control-plane/src/directory) |
| OIDC connectors | Authorization code flow with PKCE against any OIDC provider, or a preset (Google, JumpCloud); gxctl uses the device grant | `directory/oidc.rs` (Glidex's adapter over [`glidex-auth`'s OIDC](https://github.com/CMKL-APEX/glidex/blob/main/crates/glidex-auth/src/oidc.rs)), `directory/presets/` |
| Directory as provider | Each directory is itself an OpenID provider with its own signing keys and JWKS; its access tokens are accepted as bearer tokens | `directory/issuer.rs`, `directory/op.rs`, `api/op.rs` |
| Provisioning | SCIM 2.0 endpoints (`/scim/v2/c/{id}/…`) for an IdP to push users and groups; pull sync reads them from the IdP's directory on the leader | `directory/scim.rs`, `api/scim.rs`, `directory/sync.rs` |
| Passkeys | WebAuthn registration and sign-in; a passkey is one of a user's identities | `auth/passkeys.rs`, `auth/webauthn.rs`, `api/passkeys.rs` |
| Workload federation | A tenant trusts an external JWT issuer (CI systems, Kubernetes); `POST /auth/exchange` (RFC 8693) swaps a matching token for a short-lived token of a service account | `directory/federation.rs`, `directory/exchange.rs` |
| Tokens | Browser sessions (cookie); personal and service-account tokens (`gxt_…`), stored only as hashes | `auth/store.rs` |
| Workload identity | A VM asks the metadata endpoint (`glidex-metad`) for an identity token (a JWT with a SPIFFE ID, signed with its directory's keys, granting nothing in Glidex) or, with a service account attached, a short-lived Glidex token. metad names only the VM port's peer address; the control plane finds the running VM on that node, and answers itself where writes go there, else asks the leader (`/cluster/v1/metadata`) | `directory/workload.rs`, `api/metad.rs`, [`glidex-metad`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-metad), `glidex_ovs::metadata` |

Tenancy sits on top: **directories** hold users, teams and connectors;
**tenants** own projects and need an **admission** on a cluster to
allocate there; a cluster can trust another organization's directory
(`trust.rs`, `directory/peer.rs`). Authorization is Cedar over
principals, tenants, projects, hosts and resources.

Design: [11-security.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/11-security.md),
[19-tenancy.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/19-tenancy.md),
[25-passkeys.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/25-passkeys.md),
[29-workload-identity.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/29-workload-identity.md),
[17-gxctl-auth.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/17-gxctl-auth.md).
Operating it: [Authentication](../operator-guide/authentication.md),
[Access control](../operator-guide/access-control.md),
[Workload federation](../operator-guide/workload-federation.md).

### Metering

Each node samples its VMs and disks every `metering.sample_secs`: CPU
and memory from each VM unit's cgroup (the unit turns on accounting),
disk I/O from the hypervisor's counters, network traffic from OVS bridge
ports, external traffic from NAT counters (conntrack accounting on OVN
gateways), stored size from the volumes. A ledger turns readings into
hourly usage rows exactly once: deltas and the cursors that produced
them commit in one transaction. In a cluster each node accumulates
locally and ships closed hours to the leader (`/cluster/v1/ledger`),
which merges them and rolls up daily totals, 5-minute rate series and
billing-month percentiles. Usage is read through `UsageService` and
`MetricsService` (`/usage`, `/vms/{id}/stats`).

Code: [`metering/`](https://github.com/CMKL-APEX/glidex/tree/main/crates/glidex-control-plane/src/metering)
(`sampler.rs`, `sources.rs`, `net.rs`, `ledger.rs`, `rates.rs`, `retention.rs`, `query.rs`).
Design: [13-metering.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/13-metering.md).
Using it: [Usage](../user-guide/usage.md).

## Life of a VM create

A user creates a VM from an image and starts it. Below, a three-server
cluster with the VM placed on an agent; on a standalone host the
forwarding, Raft and watch steps fall away and everything happens in one
process.

```text
 UI / gxctl        server A          leader B             agent C               shim / hypervisor
 (follower)
   │ CreateVm ────▶ authenticate
   │                gate: write ───▶ authenticate (fwd principal)
   │                                 service::vm::create_vm: Cedar checks
   │                                 VmManager::create_vm_with: admission,
   │                                 scheduler::pick_node → C
   │                                 Db::write: VM + disk + event + IPAM
   │                                 ── Raft ──▶ A, others apply
   │ ◀── 202 / wait ───────────────  audit
   │                                    ── watch :8842 ──▶ cache applies
   │                                                       disk ctrl: image copy,
   │                                                       make root disk
   │                                                       VM ctrl: seed, netd ports,
   │                                                       launch.json, status.instance,
   │                                                       start glidex-vm@<id> ──▶ shim: instance.json,
   │                                                                                 spawn hypervisor,
   │                                                                                 READY
   │                                                       observe: Running ◀───────
   │                                    ◀─ status-write ── 
   │ ◀── watch event / wait answers ── (leader writes status; replicated)
```

1. **Client.** The web UI calls `VmService.CreateVm` over gRPC-Web
   (`createVm` in `crates/glidex-ui/ui/src/api.ts`); `glidex-ui` proxies
   `/api/*` to the control plane's `ui.sock`. `gxctl`'s `create` command calls
   the same RPC through its generated client
   (`src/bin/gxctl/vm.rs`) over `api.sock` or HTTPS `:8841`. A REST
   client would `POST /vms` with the same message as JSON.
2. **Listener and authentication.** `serve.rs` tags the request with its
   listener and peer; `api::authenticate` resolves the principal (the
   session cookie, the peer uid, or a token).
3. **Cluster gate.** `CreateVm` is a `WRITE` in the operation table, so
   on a follower `gate::cluster_gate` forwards it to the leader over
   `:8842` with the principal attached. The leader runs the rest.
4. **RPC adapter.** `api/rpc/mod.rs` finds the operation (action
   `createVm`) and builds the `Call`; `api/rpc/vm.rs` calls
   `service::vm::create_vm`.
5. **Service.** `create_vm` checks Cedar for the compound request:
   `createVm` on the project, then `useDisk`, `useCredential`,
   `attachNetwork`/`useNetwork`, `readImage`, `useHostPath` and device
   pools for everything the request names, and that the project's
   tenant is admitted here. It builds the `VmConfig` and calls
   `VmManager::create_vm_with`.
6. **Admission and placement.** Under the VM cache's write lock,
   `state.rs` validates, checks quotas and claims, plans the root disk
   on the default datastore, and places the VM: on a standalone host
   on `local`, in a cluster with `scheduler::pick_node`. Disks bind to
   the chosen node (unless shared). One `Commit` writes the VM
   (`spec.power` as requested, `stopped` by default), its disks, the
   `Created` event and IPAM reservations.
7. **Store.** `Db::write` commits locally on a standalone host; in a
   cluster the leader proposes the write set to Raft and returns once it
   is committed and applied. The audit entry is written; the call
   answers at once, or with `wait` once the VM converges
   (`service::vm::respond` → `wait_converged`).
8. **Reaching the node.** Servers see the change through the store's
   subscription (`VmManager::cache_sync`); the agent through its watch
   (`cluster/agent.rs`), which applies it to its cache. Both queue the
   disk and the VM on the local work queue; only the node the VM is
   placed on acts (`VmManager::is_local`).
9. **Disk.** The disk controller makes sure the node has a verified copy
   of the image (`controller/image_cache.rs`), then has the datastore
   driver create the root disk (a qcow2 overlay, or an LVM-thin snapshot
   through stord). Its status write goes to the leader
   (`/cluster/v1/status-write`).
10. **Start.** Once the spec says `running` (in the request, or after a
    later `StartVm`), `controller/vm.rs` waits for `DisksReady` and
    `NetworkReady`, establishes that no instance is live, then:
    regenerates the cloud-init seed; for each NIC records it in
    `status.nics` and asks netd to attach the port; writes
    `launch.json` with the driver's command line; records the intent in
    `status.instance` (instance id, boot id, generation); and asks the
    runner (`instance/runner.rs`) to start `glidex-vm@<id>` over D-Bus.
11. **Shim.** `glidex-vm-shim` validates `launch.json`, writes
    `instance.json`, starts the console proxy, spawns the hypervisor on a
    fresh PTY, waits for its API socket, binds `shim.sock` and tells
    systemd it is ready (`supervisor.rs`). The control plane opens a
    `pidfd` exit watch on the shim and the hypervisor.
12. **Status back.** The next round observes the guest through the
    hypervisor socket (Cloud Hypervisor `vm.info`, QEMU `query-status`),
    sets phase `Running`, `Ready=True` and `observed_generation`, and
    sends the status to the leader, which checks and commits it. The
    change replicates; watchers (`WatchService.Watch`, used by the UI's
    `live.tsx`, and `GET /watch`) get a `modified` event, and a waiting
    call answers `200` with the VM.

## Further reading

- [Design decisions and trade-offs](design-decisions.md)
- [Algorithms](algorithms.md)
- [Repository and code organization](organization.md)
- [Concepts](../concepts.md) for the user-facing vocabulary
- The design specs index:
  [spec/README.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/README.md)
