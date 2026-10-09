# Cluster operations in the web UI (implementation plan)

Status: **plan**, not implemented. It extends [web-ui.md](web-ui.md) for
the multi-host clusters of [clustering.md](clustering.md) (referred to as
"C§n" below). The REST endpoints it uses are in C§16 and `api/mod.rs`;
where the UI needs something the API does not yet give, the server
addition is listed with the milestone that needs it (§9).

## 1. Why

C1–C8 made glidex a cluster, but the UI stayed a single-host UI with one
Cluster page (C7: nodes, drain/undrain, Raft, port gaps):

- Nothing outside that page knows about nodes: the VM list has no
  node column, the create form cannot place a VM, disks on other nodes
  are indistinguishable from local ones, images don't show their copies,
  networks can't be created with `scope: cluster`.
- Membership operations (remove, forget, purge, rejoin tokens, promote,
  detach, imports, CA rotation, leave, dissolve) are CLI-only. C§16
  requires the Cluster page to show *pending detaches and imports*.
- The UI does not retry on `503 leader_changed` / `cluster_unavailable`
  (C§14 says it does).
- Capability checks drifted once already: C1 moved the cluster-wide
  actions from `Host` to `Cluster` and the UI kept checking `Host`, which
  hid "+ Create Project" and the identity, policy, audit, usage and
  cluster pages from administrators (fixed in `7774966`, guarded by
  `glidex-control-plane/tests/ui_capabilities.rs`).

## 2. Principles

1. **The server decides; the UI explains.** Every action is shown only
   when `POST /authz/check` allows it on the resource type the Cedar
   schema gives it (`Cluster` for membership, CA and imports; `Host` for a
   host's own networking; `Project` for project resources). Refusals
   (`409 conflict`, `confirmation_required`, `node_departing`,
   `reauth_required`) are shown with the server's message, never
   rephrased into something else.
2. **Same confirmations as `gxctl`.** An operation the CLI guards with
   `--force` or a typed confirmation gets a dialog that states the
   consequence and asks for the node's (or cluster's) name. The request
   then carries the same `force` / `fenced` flags the CLI sends.
3. **Secrets are shown once.** Join and rejoin tokens come back in the
   response body (`token`). The UI shows them once in a copy-only field,
   never puts them in a URL, `localStorage`, a log line, an error report
   or the page title, and drops them from memory when the dialog closes.
   It shows the command to run with the token read from a file or stdin
   (`gxctl cluster join --server … --token-file <file>`), never one with
   the token on the command line.
4. **Works on any server.** The UI may be served by any server (C§6:
   "Clients and the UI may use any server"). Nothing assumes the serving
   host is the leader; requests the leader must handle are forwarded by
   the server.
5. **Degrades to a standalone host.** With `clustered: false` every
   cluster element is hidden or replaced by the standalone wording; no
   page breaks.

## 3. Shared pieces (U0)

### 3.1 Capabilities

`session.tsx` keeps one table, `HOST_ACTION_RESOURCE`, of host-wide
capabilities and the resource type each is checked on. New entries:

| Capability | Resource | Gates |
|---|---|---|
| `listNodes` | Cluster | node names everywhere, node filter, Node page |
| `removeNode`, `forgetNode`, `purgeNode`, `createRejoinToken` | Cluster | Membership menu |
| `detachNode` | Cluster | Detach |
| `promoteNode` | Cluster | Promote |
| `createJoinToken` | Cluster | Add node |
| `listImports`, `approveImport`, `rejectImport` | Cluster | Imports |
| `rotateCa` | Cluster | CA card |
| `dissolveCluster` | Cluster | Danger zone |
| `snapshotCluster` | Cluster | Snapshot download |
| `initCluster`, `leaveCluster` | Host | Initialize (standalone host); Leave (this node) |

`ui_capabilities.rs` already fails when an entry names a resource the
schema's `appliesTo` doesn't list; it needs no change beyond the table.

**Per-node actions.** `drainNode` and `undrainNode` apply to a node's
own `Host` (the server checks `Ent::host_of(id)`), so a site can grant
draining per node. They are not in the session table: the Cluster and
Node pages check them per row with `useCan`, on `{type: "Host"}` for the
serving node and `{type: "Node", id}` for the others. Any later
host-specific action (uplinks, PCI devices) follows the same rule.

### 3.2 Node directory

`useNodes()` (like `useDirectory()`): `GET /nodes` once per session and
every 10 s while a page that uses it is mounted, giving
`id → { name, role, phase, ready, advertise }`. `nodeName(id)` returns the
name, or the first 8 characters of the id when the caller can't list
nodes (`listNodes` denied) — see §9 `node_name` for the proper fix. The
node serving the UI is marked "(this node)" (`node_id` of `GET /cluster/status`).

### 3.3 Leader changes and unavailability

`request()` in `api.ts`:

- `503` with `leader_changed` on an idempotent request (GET, and PUT /
  DELETE / POST endpoints listed as idempotent in rest-api.md): retry up
  to 3 times with 250 ms, 500 ms, 1 s backoff.
- `503 cluster_unavailable` (no quorum): no retry; a global banner
  "The cluster has no leader; changes are refused until a majority of
  servers is back. Reads may be stale." that clears on the next success.
- `X-Glidex-Consistency: stale` on a read: a small "may be out of date"
  marker on the page header.

### 3.4 Confirm dialog

`<ConfirmDialog consequence=… typeToConfirm=name onConfirm=…>`: the
consequence text, a field that must equal `typeToConfirm`, and a
destructive button disabled until it does. It retries once after a
`401 reauth_required` through the existing `ReauthDialog`.

### 3.5 Routes

| Path | Component |
|---|---|
| `/cluster` | `Cluster` (existing; extended §6) |
| `/cluster/nodes/:id` | `NodeDetail` (§5) |
| `/cluster/imports/:plan` | `ImportPlan` (§7.4) |

web-ui.md's route table gains these and the existing `/cluster` row
(missing today).

## 4. Cluster awareness in existing pages (U1)

| Page | Change | API |
|---|---|---|
| Dashboard (VM list) | Node column (hidden when not clustered); filter by node; a VM whose node is `Ready=Unknown` shows a grey "node unreachable" badge instead of its last state | `GET /vms` (`node`), `useNodes()` |
| VM detail | Node name linking to `/cluster/nodes/:id`; the `Ready` condition with reason `NodeUnreachable` shown as "Its node stopped reporting; the VM may still be running. Nothing is restarted elsewhere." | `GET /vms/{id}` conditions |
| VM detail | When `Scheduled=False`: the scheduler's explanation from the condition message, and the VM shown as "waiting for a node" | condition `Scheduled` |
| Disks | Node column; `status` taken from the server (`pending`, `ready`, `missing`), never inferred from the file | `GET /disks` (`node`, `status`, `phase`) |
| Networks | Scope column (`cluster` / node name); create form offers *Cluster (OVN)* when the cluster has OVN on, with mode `isolated` / `nat` and subnet | `POST /networks` `scope: "cluster"` |
| Console | When the VM's node is unreachable, say so instead of a failed WebSocket | `GET /vms/{id}` |

Acceptance: on a two-node cluster, a VM on the agent shows its node, its
console works through the relay, its disk is not "missing", and stopping
the agent's control plane turns the VM's badge to "node unreachable"
within the node grace period.

## 5. Placement and capacity (U2)

### 5.1 Create VM

The create form gains **Placement**: *Automatic* (default; the scheduler
picks) or a node chosen from Ready, schedulable nodes, each with free
vCPUs and memory (`allocatable` minus what its VMs request). Choosing a
node sends `node: <id>`. If the scheduler refuses (`422` or a created
VM with `Scheduled=False`), the form shows the per-node reasons.

Needs §9 `POST /scheduler/preview` to show *before* creating which nodes
fit and why the others don't; until then the form only shows capacity
and the post-create condition.

### 5.2 Node page

`/cluster/nodes/:id`:

- Header: name, role, phase, Ready (with `ready_reason`), advertise and
  tunnel addresses, versions, labels, last heartbeat.
- Capacity: CPUs, memory, hugepages: capacity, allocatable, requested by
  VMs placed there (from `GET /vms` filtered by node).
- VMs and disks on the node (links).
- OVN (when on): chassis name, Geneve peers (§8).
- Actions (§7) in a menu, by capability.

### 5.3 Images

Each image row shows its copies: `n of m nodes`, and per node
`ready` / `copying (p %)` / `failed (reason)`. Needs §9 `copies` on
`GET /images`.

### 5.4 Usage

Usage gains a *By node* breakdown (VM-hours, vCPU-hours, memory,
traffic) for holders of `readUsage`. Needs §9 `group_by=node` on
`/usage`.

## 6. The Cluster page (U3, extends C7)

Sections, top to bottom:

1. **Banner** for port gaps (existing) and for a running CA rotation or
   detach.
2. **Nodes** (existing table) + Ready column, VM count, link to the Node
   page, and the per-row action menu (§7). **Add node** button
   (`createJoinToken`) opens §7.1.
3. **Pending imports** (`GET /imports`, state `pending`): node, summary
   (VMs, disks, images, networks, credentials, projects), problems count,
   expires in; link to §7.4.
4. **Detaches in progress**: nodes in phase `Departing`, with the step
   (freeze, bundle, ack, commit, switch) from the node's status, and
   *Abort* (`POST /nodes/{id}/detach {abort: true}`).
5. **Raft** (existing) + per voter: reachable, match index lag.
6. **CA** (§7.6).
7. **Danger zone** (§7.7), collapsed by default.

Standalone host (`clustered: false`): replace the "run gxctl cluster
init" hint with **Initialize cluster** (§7.8) for holders of
`initCluster`.

## 7. Membership and lifecycle operations (U3–U4)

Every operation below is audited by the server (C§12.4); the UI adds
nothing to that. All use §3.4 unless stated.

### 7.1 Add node (join token)

Dialog: role (`agent` / `server`), lifetime (1 h default, 1 min – 7 d),
*allow import* (C§5.9). `POST /cluster/join-tokens` →
`{ token, role, ttl_secs, servers, ca_fingerprint }`. Shows:

- the token once (§2.3), with Copy;
- the CA fingerprint to compare on the joining host;
- the commands, token from a file:
  `glidex-install --join <server> --role <role> --token-file <file>` or
  `gxctl cluster join --server <server> --role <role> --token-file <file>`.

No confirmation (creating a token changes nothing until used).

### 7.2 Drain / Undrain

Existing. Drain gets a one-line consequence ("No new VMs are placed
here; running VMs stay") without typed confirmation.

### 7.3 Remove, forget, purge, rejoin token, promote, detach

| Action | When offered | Request | Confirmation text |
|---|---|---|---|
| Remove | node empty, not the last gateway | `POST /nodes/{id}/remove` | "Removes the node from Raft and OVN. It must be empty." |
| Forget | node `Ready=Unknown` | `POST /nodes/{id}/forget {fenced}` | "For a node that is gone. Its VMs stay recorded on it until it rejoins or is purged." + checkbox *the node is powered off or fenced* (`fenced`) |
| Purge | forgotten node | `POST /nodes/{id}/purge` | "Deletes the records of its VMs and disks. Cannot be undone." |
| Rejoin token | forgotten node | `POST /nodes/{id}/rejoin-token {raft_intact, ttl_secs}` | as §7.1 (token once) |
| Promote | agent | `POST /cluster/promote {nodes:[id]}` | "Makes it a Raft voter and an OVN database member." |
| Detach | Active node | `POST /nodes/{id}/detach {map_networks, with_access}` | network mapping form (cluster network → node network), *carry access*; "The node leaves with its VMs." |

A `409` whose message asks for `--force` is shown with a second button
*Force* that resends with `force: true`, behind its own typed
confirmation.

### 7.4 Imports

`/cluster/imports/:plan` from `GET /imports/{plan}`:

- summary and `problems` (each conflict: project, network or credential
  name, quota);
- mapping editor: source project → existing project or `new:<name>`;
  node network → new name; `<project>/<username>` → new username;
  *over quota* (shown only with `exceedQuota` on the target project);
- **Check** → `POST /imports/{plan}/approve {…mappings, dry_run: true}`
  and show the remaining problems;
- **Approve** (enabled when the check passes) and **Reject**
  (`POST /imports/{plan}/reject`), both typed confirmation with the node
  name.

The approver needs the per-project rights the server checks (C§12.2);
a `403` names the missing action.

### 7.5 Leave, dissolve

In the danger zone: *Leave cluster* (this node; `POST /cluster/leave`),
*Dissolve cluster* (`POST /cluster/dissolve`, last node). Typed
confirmation with the cluster id.

### 7.6 CA

Card: signing CA fingerprint, expiry, trust bundle entries, nodes whose
certificate is not yet signed by the current CA. **Rotate**
(`POST /cluster/rotate-ca {grace_secs}`) with the step list from C§12.5
and live progress while it runs. Needs §9 CA fields in `/cluster/status`
if absent.

### 7.7 Snapshot

**Download snapshot** (`GET /cluster/snapshot`) for `snapshotCluster`.
The dialog says the file holds credential and token hashes and must be
stored like the database (encrypted, access-controlled); the download is
a browser file save, never kept in the page.

### 7.8 Initialize (standalone)

Form: advertise address (default: the host's primary address, port
8842), tunnel IP (optional), *enable OVN* (writes nothing itself; links
to the config note). `POST /cluster/init {advertise, tunnel_ip, force:
true}` after typed confirmation with the host name: "Re-keys every
record for clustering. A backup is kept, but this cannot be undone from
the UI." After success the session reloads (role links now name the
cluster).

Joining an existing cluster stays CLI/installer-only: it needs a token
from a file and changes the host's identity.

## 8. OVN and cluster networks (U5)

- **Node page / OVN card**: chassis name, encapsulation IP, Geneve
  tunnels to each peer (up / down, packets), `ovn-controller` connected.
- **Cluster page / OVN**: NB/SB database members and leader, gateway
  chassis priorities for `gx-edge` and each VPC router.
- **Network detail** for cluster networks: logical switch, router,
  external IP, active gateway node, ports with their chassis.

All need a read-only server endpoint (§9 `GET /cluster/ovn`,
`GET /networks/{name}/ovn`); the UI never talks to OVN.

## 9. Server additions

| Addition | For | Notes |
|---|---|---|
| `node_name` on `VmResponse` and `DiskResponse` | U1 | Users without `listNodes` still see where their VM runs. Name only, no addresses. |
| `copies: [{node, node_name, state, progress?, reason?}]` on images | U2 §5.3 | From the `image_caches` table. |
| `POST /scheduler/preview` `{vcpu_count, mem_size_mib, image?, networks, node?}` → per node `{fits, reasons[]}` | U2 §5.1 | Same code as `pick_node`; checked against `createVm` on the project. |
| `group_by=node` on `/usage` | U2 §5.4 | |
| CA expiry (`not_after`) and the trust bundle in `/cluster/status` `ca` (which has `signing`, `retiring`, `retire_at`, `started_at` today), and each node's certificate issuer and expiry | U4 §7.6 | |
| Detach progress in node status (`departure: {plan, step}`) | U3 §6.4 | |
| `GET /cluster/ovn`, `GET /networks/{name}/ovn` | U5 | Read-only; the same permission as `GET /cluster/status` / `GET /networks/{name}`. |
| Agent login | U1 | An agent's API cannot record a PAM user ("a node may not write users", found by the nested e2e test). Either forward identity writes to the leader or have the agent's UI redirect to a server; until then the UI on an agent says "log in on a server: <list>". |

## 10. Milestones

| Milestone | Content | Done when |
|---|---|---|
| **U0** Shared pieces | §3: capability table entries, `useNodes`, 503 handling, `ConfirmDialog`, routes; web-ui.md route table | Unit tests for retry/backoff; `ui_capabilities.rs` passes with the new entries |
| **U1** Cluster awareness | §4 + `node_name` (§9) + agent-login message | Playwright: two-node mock shows nodes on VMs/disks/networks; nested e2e: VM on the agent visible with node, console through the relay |
| **U2** Placement and capacity | §5 + preview, copies, usage by node (§9) | Create a VM on a chosen node; refusal reasons shown; image copies visible during a copy |
| **U3** Cluster page and membership | §6, §7.1–7.4 (imports included, C§16) | Playwright with a mocked cluster for every dialog; nested e2e: drain, remove an empty agent, rejoin token shown once |
| **U4** Lifecycle and CA | §7.5–7.8 | Init a standalone host from the UI in the nested e2e; CA rotation progress on a two-node cluster |
| **U5** OVN views | §8 + endpoints | Node page shows the Geneve tunnel the nested e2e test checks |

Order: U0 → U1 → U2 (5.1–5.2) → U3 → U4 → U2 (5.3–5.4) → U5. U0+U1 are
about a day each; U2–U5 a few days each.

## 11. Testing

- **Playwright** (`crates/glidex-ui/e2e`): a mocked control plane that
  serves a two-node cluster (`/cluster/status`, `/nodes`, `/imports`,
  VM/disk `node` fields), with fault cases (`503 leader_changed` then
  success; `cluster_unavailable`; `409 confirmation_required`;
  `reauth_required`). Each dialog's request body is asserted.
- **Secrets**: a test that after the join-token dialog closes, the token
  appears in no `localStorage`/`sessionStorage` key, URL, or console
  message.
- **Capabilities**: `ui_capabilities.rs` (schema ↔ UI table).
- **Nested cluster** (`crates/glidex-e2e`): extend
  `nested_cluster` with a UI smoke step (glidex-ui on h1: log in, see
  both nodes, the agent's VM with its node).

## 12. Open questions

1. Should the UI on an agent proxy to a server instead of its local
   control plane? (Simplest fix for agent login and stale reads.)
2. Live updates: keep 10 s polling, or use the watch API for nodes and
   imports?
3. Per-node checks cost one `/authz/check` item per node and action;
   with many nodes, batch them per page (the endpoint takes up to 200)
   or add a server-side "actions allowed on these nodes" query?
