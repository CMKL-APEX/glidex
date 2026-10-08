# Clustering: replicated control plane (Raft) and cluster networks (OVN)

> Status: **design, not implemented** (2026-10-08). It lifts the
> "Clustering / multi-host orchestration" non-goal in
> [README.md](README.md) for the scope in §2. It changes contracts in
> [architecture.md](architecture.md), [reconciliation.md](reconciliation.md),
> [networking.md](networking.md), [security.md](security.md),
> [images.md](images.md), [metering.md](metering.md),
> [installer.md](installer.md), [rest-api.md](rest-api.md) and
> [cli.md](cli.md). §16 lists the edit each needs. As with
> reconciliation.md, those edits land in the PR of the milestone that
> changes the behaviour (§17), not ahead of it.

## 1. Motivation

Glidex runs on one host. Everything it knows is in one ReDB file
(`glidex.db`), its controllers act on that host only, and its networks
are OVS bridges that end at the host's edge. This document plans how
several glidex hosts form **one cluster**:

- one API, one set of users, projects, quotas and policies;
- VMs placed on any host, with their state surviving the loss of any
  one server host;
- networks that span hosts, so VMs on different hosts share a private
  network.

Two building blocks carry the design: an **embedded Raft log** that
replicates the control-plane database, and **OVN** on top of the OVS
installation glidex already manages.

## 2. Goals and non-goals

Goals:

- A cluster of **1–20 hosts**: 1, 3 or 5 **server** hosts (Raft
  voters, API, cluster controllers) and any number of **agent-only**
  hosts.
- **No new external database.** The replicated store is part of the
  control-plane binary.
- **Management survives losing a minority of servers.** VMs keep
  running through *any* control-plane outage, as they do today
  (reconciliation.md D1).
- **Cluster-scoped NAT and isolated networks** on OVN, plus **provider
  (bridged/VLAN) networks** reachable from every host.
- **A single host stays exactly as it is** until an administrator runs
  `gxctl cluster init`.
- **Nodes come and go without touching running guests:** leave empty,
  be forgotten and later rejoin as themselves, or **leave with their
  VMs** (becoming standalone) and **join or form another cluster with
  them** (§5.3–5.10).
- Every existing invariant keeps holding: claims, quotas,
  exactly-once metering, the netd ownership rules, and reconciliation.md D8 (at most one
  instance per VM, ever).

Non-goals for this revision (each in §18):

- Shared storage, live migration and evacuation. Disks stay **local**,
  so a VM is pinned to the host of its disks.
- Automatic restart of a lost host's VMs elsewhere. Without fencing,
  that breaks reconciliation.md D8.
- BGP, routed (no-NAT) networks, floating IPs and load balancers.
- A security-group API. Networks keep today's isolation semantics.
- Several sites, or federation between clusters.
- An eBPF datapath (D15).

## 3. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | The control-plane database is replicated with **Raft embedded in the control plane** (the `openraft` crate, pinned). The state machine is the existing ReDB database. | No new service to install, secure or upgrade, in line with README goal 4. Every table, migration and test keeps working on ReDB. etcd would add a daemon, value-size and per-transaction operation limits, and a rewrite of every table access. dqlite (Incus's choice) would mean C and a move to SQL. |
| D2 | **The leader computes, everyone applies bytes.** A write runs once on the leader against its applied state. Its *write set* (ordered `put`/`delete` operations on table keys) is the Raft log entry. Applying an entry is a pure byte operation, done in the same ReDB transaction that advances `last_applied`. | Admission, quotas and claims (reconciliation.md D5) need serialized read-then-write logic. Replicating *commands* instead would require identical logic, clocks and UUID generation on every replica and every version, which breaks under rolling upgrades. Byte write sets are deterministic by construction and version-independent. |
| D3 | **One write lane on the leader.** Writes are serialized. A write's closure runs only once the previous write has been applied locally. | The closure then reads exactly the state its write set will apply to, so no extra conflict detection is needed. Control-plane write rates (tens per second) fit easily in a LAN Raft round trip. Pipelining is a later optimization (§18). |
| D4 | **One binary, two roles.** `glidex-control-plane` runs the **node** role on every host (VM, disk and image-cache controllers, metering sampler, netd sync), plus the **server** role on server hosts (API, Raft, scheduler, cluster controllers, ledger). | The node role is today's host-facing code, unchanged in substance. A standalone host is both roles in one process with a local store (D5). |
| D5 | **Standalone stays non-Raft.** The store is a trait with two implementations: `Local` (today's direct ReDB) and `Replicated`. `gxctl cluster init` switches a host to `Replicated` with one voter. | Existing single-host users carry no Raft cost and no risk until they opt in. Both implementations take the same write sets (C0), so the code paths stay common. |
| D6 | **Placement is sticky and local-storage-bound.** The scheduler places a VM once (`status.placement`). After that the VM moves only when an administrator explicitly re-places it, which needs the old node to be gone and fenced (§18). A disk is bound to a node at creation or at its first use. | Disks are local files. Moving a VM without its disk is impossible; moving it while the old node may still run it breaks D8. |
| D7 | **Node liveness is held in the leader's memory.** Nodes heartbeat every 5 s over the cluster port. Only *transitions* (`Ready` ↔ `Unknown`) are written through Raft. After a leader change, every node gets a fresh grace period. | A heartbeat per node per 5 s through Raft is pure log churn. A new leader must not declare every node dead because it has not heard from them yet. |
| D8 | **No automatic failover of VMs.** A node that stops heartbeating makes its VMs `Ready=Unknown/NodeUnreachable`. Nothing is restarted elsewhere. | A partitioned node may still run its VMs (reconciliation.md D1). Starting copies elsewhere would put two writers on one disk the moment storage is shared, and with local disks there is nothing to start from anyway. |
| D9 | **OVN for cluster networks**, on the OVS each host already runs. The leader's network controller writes the OVN **northbound** database. netd on each host only joins the host to OVN and plugs VM ports into `br-int`. | OVN provides the overlay, logical switches and routers, DHCP, SNAT gateway high availability, port security, and (later) multi-chassis port binding for live migration (§18). The NB database is a network service reached over TLS, so the unprivileged control plane can write it, while netd keeps owning everything privileged on the host. |
| D10 | **Today's OVS networks remain, as `scope: node`.** OVS bridges with nftables NAT and dnsmasq keep working on each host. New networks in a cluster default to `scope: cluster` (OVN). Converting a node network to a cluster network is out of scope. | Converting means renumbering running VMs (networking.md §11.2a explains why a NAT subnet is never re-created under VMs). Node networks also stay useful for DPDK-only or host-local setups. |
| D11 | **NAT egress is centralized on an HA gateway chassis group:** one cluster **edge router** with SNAT, active on one gateway node, with standby nodes ready to take over. | Distributed SNAT needs an external IP per VM. Centralized SNAT needs one IP for the whole cluster, gives every NAT network a stable egress address, and is OVN's standard high-availability gateway pattern. |
| D12 | **IPAM belongs to the control plane.** Subnets and per-NIC reservations live in the cluster store and are written into OVN port `addresses`. OVN's dynamic addressing is not used. | One source of truth that survives restarts and future live migration, and serves the security rules (port security, address sets). |
| D13 | **Local accounts are host-scoped.** `unix:` and `pam:` identities carry the node they were seen on, unless the site sets `cluster.shared_local_accounts: true` (accounts managed centrally, e.g. SSSD/LDAP). | `alice` on host 1 is not necessarily `alice` on host 2. Without this, any local user on any host could act as a same-named user elsewhere. |
| D14 | **Break-glass is honoured on server hosts only.** On an agent-only host, local root and `glidex-admin` get no cluster admin rights through `api.sock`. | Root on a server host already holds the whole database. Root on an agent host holds only its own VMs, and must not be able to escalate to the cluster. |
| D15 | **No eBPF datapath** (decision record). | OVS/OVN covers every port type glidex uses (tap, vhost-user, DPDK and AF_XDP uplinks) under one model. An eBPF datapath would cover tap only, so glidex would maintain two datapaths. It brings no migration or performance gain for tap VMs. eBPF stays a debugging tool (`retis`, `pwru`), plus an optional tc counter if OVN metering proves too coarse (§13.3). |
| D16 | **Resources change cluster only by an explicit handover.** Leaving with resources (detach) and joining with them (import) are two-phase: the losing side freezes, the gaining side commits, and only a commit (a signed receipt for detach, the approval write for import) lets either side act as owner. | At every instant each VM and disk is owned by exactly one database. A half-done move must never leave two control planes both entitled to start or delete a VM (reconciliation.md D8). |
| D17 | **An import needs the target's approval; a token alone can't bring resources in.** Membership and the imported records commit in one write. | A join token proves the host may join, not that its projects, names, credentials and quota use are acceptable to the target. Committing them together means there is never a member with half its resources, or resources with no member. |
| D18 | **A server that lost its consensus state never rejoins as the same voter**, for glidex's Raft and for OVN's NB/SB clusters alike. It is removed and re-added as a learner. | A voter must remember its vote; with an empty log it could vote twice in one term and elect two leaders. |
| D19 | **Ids travel, names are mapped, history stays.** VM, disk and image ids are kept on detach and import; project, network and credential names are mapped when they collide; audit and metering history stay in the database where they were recorded. | Disk and image files are named by id, and `Linked` disks are qcow2 overlays whose backing file is the image file (images.md §3), so changing ids would mean rewriting files under running VMs. Names are only labels (images.md §2). Usage and audit belong to the operator who recorded them; carrying them over would double-report in the target. |

## 4. Topology

```
                         gxctl / UI / OIDC
                                │ HTTPS :8841 (any server)
       ┌────────────────────────┼─────────────────────────┐
       ▼                        ▼                         ▼
 ┌─────────────┐         ┌─────────────┐          ┌─────────────┐
 │ server h1   │◀─Raft──▶│ server h2   │◀──Raft──▶│ server h3   │   :8842 mTLS
 │ API · Raft  │ (leader)│ API · Raft  │          │ API · Raft  │
 │ node role   │         │ node role   │          │ node role   │
 │ netd · OVS  │         │ netd · OVS  │          │ netd · OVS  │
 │ OVN NB/SB · │◀─ovsdb─▶│ OVN NB/SB · │◀─ovsdb──▶│ OVN NB/SB · │   :6641/:6642, :6643/:6644
 │ northd      │  raft   │ northd      │   raft   │ northd      │
 │ ovn-ctrl    │         │ ovn-ctrl    │          │ ovn-ctrl    │
 └──────┬──────┘         └──────┬──────┘          └──────┬──────┘
        │        Geneve :6081/udp (VM traffic)           │
        └──────────────┬───────────────┬─────────────────┘
                ┌──────┴──────┐ ┌──────┴──────┐
                │ agent h4    │ │ agent h5    │  node role only, netd, OVS,
                │             │ │             │  ovn-controller; heartbeat and
                └─────────────┘ └─────────────┘  watch to any server on :8842
```

| Port | Protocol | Between | Purpose |
|---|---|---|---|
| 8841/tcp | HTTPS | clients → servers | REST API, unchanged (security.md §5.1) |
| 8842/tcp | HTTPS, mTLS (cluster CA) | all nodes | Raft (servers), write forwarding, watch, status writes, heartbeats, console and stats relay (§8.4) |
| 6641/tcp, 6642/tcp | OVSDB over SSL | servers ↔ OVN clients | OVN NB (control plane, northd) and SB (northd, ovn-controller) |
| 6643/tcp, 6644/tcp | OVSDB Raft over SSL | servers | NB and SB database clustering |
| 6081/udp | Geneve | all nodes | overlay traffic |

The installer still doesn't touch the firewall (README). `gxctl cluster
status` checks each port's reachability between nodes and reports the
gaps.

**Two Raft groups.** glidex's own (D1) and OVSDB's (OVN NB/SB) run on
the same server hosts and fail the same way: both need a majority of
servers. They are kept separate on purpose. glidex never stores its own
state in OVSDB, and OVN never reads the glidex store.

## 5. Cluster lifecycle

### 5.1 Init

`gxctl cluster init [--advertise <addr>] [--tunnel-ip <addr>]` on an
existing standalone host. It requires `host.cluster` (a new
`system-admin` action) and confirmation (`--force`), and is refused
while any controller round is in flight.

1. **Backup:** copy `glidex.db` to `glidex.db.pre-cluster-<ts>` (0600).
   It undoes an init that went wrong (§5.12); leaving the cluster later
   is a detach (§5.8).
2. **PKI:** generate the cluster CA (P-256, 10 years) and keep its key
   as the systemd credential `cluster-ca-key` on server hosts only.
   Generate this node's certificate (`CN=node:<node-id>`,
   SAN = advertise address, 1 year, renewed at two thirds of its life
   through the API). Built on `glidex-tls`.
3. **Store:** generate the cluster id and this node's id (UUIDv4). Open
   the Raft log (`raft/log.redb`). Create the initial snapshot from the
   current `glidex.db` at index 0, with membership = {this node, voter}.
4. **Re-key host-scoped data** in a single write:
   - every VM gets `status.placement.node` set to this node;
   - every disk and image file gets `node` set to this node;
   - every network gets `scope: node` with `node` set to this node;
   - every `unix:`/`pam:` identity gets this node as its host (D13);
   - role links on `Host::"local"` move to `Cluster::"<id>"`, so
     today's system admins keep their rights cluster-wide (§12.2).
5. **Restart** in `Replicated` mode. The `cluster` section of
   `/etc/glidex/control-plane.json` is written with `role: server`.

Init doesn't touch VMs: they keep running and are adopted as usual
(reconciliation.md §9.4).

### 5.2 Join

1. On any server: `gxctl cluster join-token --role server|agent [--ttl 1h]`
   prints a token: `<token id>.<secret>` plus the **SHA-256 of the
   cluster CA's public key** and the API address. Only
   `SHA-256(secret)` is stored. The token is single-use and expires.
   It is shown once.
2. On the new host: `glidex-install --join https://<server>:8842`. The
   token is read from **stdin or a 0600 file** (`--token-file`), never
   from `argv`, so it doesn't appear in `ps` or shell history. The
   installer pins the CA by the hash in the token before sending
   anything (same model as kubeadm's discovery hash).
3. The host sends a CSR; the leader checks the token, signs the
   certificate, and records the node (§6.3). The installer then
   installs the role's packages (§10.1) and starts the units.
4. **Server joins** add the node as a Raft **learner**. Once its log
   has caught up, it is promoted to voter. Promotion is refused if it
   would leave the cluster with an even number of voters, unless the
   administrator passes `--force` (§5.11).

A fresh host can also join without ever being standalone: the installer
skips creating a local database. A host that **already has resources**
(a standalone host, or one that left another cluster with its VMs)
joins with `--import` instead (§5.9). A plain join refuses a host that
has any VM, disk, network or credential record, so a join can never
silently abandon or duplicate resources.

### 5.3 Node states

```
           join / join --import (approved)
  (none) ─────────────────────────────────▶ Active ◀──────────── undrain ───┐
                                             │  │                           │
                         drain               │  │ detach (§5.8)        Draining
                    ┌────────────────────────┘  ▼                           ▲
                    ▼                        Departing ── commit ──▶ Departed (tombstone)
                 Draining ── remove, empty (§5.5) ──────────────────▶ Removed  (tombstone)
                                                                      ▲
  Active/Draining + Ready=Unknown ── forget --fenced (§5.6) ──▶ Forgotten ──┤ purge
                                     forget --departed (§5.8.3) ──▶ Departed │
                                             Forgotten ── rejoin, same id (§5.7) ──▶ Active
```

| Phase | Meaning | Schedulable | Certificate |
|---|---|---|---|
| `Active` | member; `Ready` condition says whether it is reachable | yes | valid |
| `Draining` | member; no new placements | no | valid |
| `Departing` | a detach is in progress (§5.8); the node's objects are frozen | no | valid until commit |
| `Departed` | **tombstone**: left with its resources; keeps the ids it took (`departed_ids`) | – | deny-listed |
| `Removed` | **tombstone**: left empty | – | deny-listed |
| `Forgotten` | **tombstone**: declared gone; its VMs are `Lost` | – | deny-listed |

Tombstones keep the node id retired: **a node id is never reused**, except
by the rejoin of §5.7, which is the same node coming back.
`Unreachable` is not a phase; it is `Ready=Unknown/NodeUnreachable` on
an `Active` or `Draining` node (§7.2).

### 5.4 Temporary absence

A reboot, an upgrade or a partition is not leaving. The node keeps its
id, certificate and `node.db`. When it is back, its heartbeats make it
`Ready` again, its watch resumes from the last revision (or re-lists on
`410`), its outbox drains (§8.2) and its VMs are adopted as usual. A
server catches up from the Raft log, or by installing a snapshot; the
OVN NB/SB members catch up the same way. Nothing for the administrator
to do.

If the certificate **expired** while the node was away, it can't
authenticate; it rejoins with its own identity (§5.7).

### 5.5 Removing an empty node

1. `gxctl node drain <node>` makes it `Draining` and lists what is still
   on it: VMs, disks, image caches, node networks, roles (Raft voter,
   OVN NB/SB member, gateway chassis). With local disks glidex never
   moves VMs itself (D6). They are deleted, or the node leaves *with*
   them instead (§5.8).
2. `gxctl node remove <node>` is refused unless the node holds no VM,
   disk or node network. Image caches are simply dropped. Then, in
   order, each step idempotent and resumable:
   1. **Gateway:** remove it from `gx-edge`'s HA chassis group. Refused
      if that would leave the group empty (`--force` only with no NAT
      network left).
   2. **Raft:** if it is a voter, transfer leadership away when it is
      the leader, then demote it. Refused if the remaining voters would
      not be a majority of the old configuration, or would be an even
      number (`--force` for the latter).
   3. **OVN databases:** if it is an NB/SB member, it leaves both
      clusters (`ovs-appctl -t …/ovnnb_db.ctl cluster/leave OVN_Northbound`,
      same for the SB). If it is unreachable, the leader kicks it from a
      remaining member (`cluster/kick`).
   4. **Chassis:** `ovn-sbctl chassis-del <node id>`.
   5. **Record:** one write: node `Removed`, certificate serial on
      `node_denylist`.
3. On the host, `glidex-install --leave` (or `gxctl cluster leave` there,
   run as root) stops the node role, asks netd to `leave_ovn` (§11.3),
   deletes the certificate, key and `node.db`, and removes the `cluster`
   section. The host is then an empty standalone host, or can be
   uninstalled. It refuses until step 2 has committed (it checks its
   node's phase is `Removed`), and points to `gxctl node remove`: local
   root on a host has no cluster rights to start a removal (D14). A host
   whose cluster is gone for good uses the offline path of §5.8.3
   instead.

### 5.6 Lost nodes: forget

`gxctl node forget <node> --fenced` covers a node that is gone for good.
The administrator asserts it is powered off and will not come back by
itself. One write makes it `Forgotten`, deny-lists its certificate and
marks its VMs `Lost` (records kept: they can be deleted, or re-placed
once shared storage exists, §18). The cluster-side steps of §5.5 (gateway, Raft,
OVN membership with `cluster/kick`, chassis) are applied to it. A dead
server can only be forgotten while the other voters still form a
majority; otherwise see §5.12. `gxctl node purge <node>` drops a
`Forgotten` tombstone's `Lost` VMs and disks and makes it `Removed`.

### 5.7 Rejoin with the same identity

A **repaired** host (a `Forgotten` node whose disks survived), or an
`Active` node whose certificate expired or whose `node.db` was lost, comes
back as **the same node**, so its VMs and disks are adopted, not
imported.

1. `gxctl node rejoin-token <node>` issues a single-use token bound to
   that node id (same handling as join tokens: shown once, hash stored,
   TTL). Refused for `Removed` and `Departed` nodes, whose ids are
   retired, and for a `Forgotten` node any of whose VMs has been
   re-placed elsewhere (reconciliation.md D8).
2. On the host: `glidex-install --rejoin https://<server>:8842`, token
   from stdin or a file. The node gets a **new certificate for the same
   node id**; the old serial stays deny-listed.
3. The node re-lists, adopts the VMs and disks it finds on the host
   (reconciliation.md §9.4), and reports files with no record as orphans
   without deleting them (images.md §2). `Lost` VMs become normal again;
   the node becomes `Active` (`Draining` if it was).

**Lost consensus state (D18).** A server whose Raft log store
(`raft/log.redb`) is gone or older than its last vote must **not** rejoin as
the same voter: a voter that forgot its vote can vote twice in one term
and break Raft's safety. The rejoin token records whether the node's
Raft state is intact (the installer checks before asking). If it is not,
the leader first removes the old voter, then adds the node back as a
**learner**, promoted after catching up (§5.2). OVN is treated the same
way: a server whose NB or SB database file is lost is kicked
(`cluster/kick`) and re-added with `ovsdb-tool join-cluster`, never
restarted with an empty file. If the node's advertise or tunnel address
changed, the leader updates the Raft member address, the chassis
encapsulation IP and `gx_nodes` in the same change.

### 5.8 Leaving with resources (detach)

A node can leave **and take what runs on it**: its VMs (running or not),
their disks, the images those disks use, its node networks and the guest
credentials its VMs reference. The cluster forgets them; the host
becomes a **standalone glidex host owning them**, without stopping a
single VM. From there it can stay standalone, form a new cluster
(§5.1) or join another one with its resources (§5.9).

#### 5.8.1 What goes, and what stays

The **departure bundle** is a set of records written into a fresh
standalone `glidex.db` on the node. Ids are kept as they are (D19).

| Goes with the node | Stays in the cluster |
|---|---|
| VMs placed on it, with their event rings | Every other node's resources |
| Disks bound to it (records; the files never move) | Audit log, metering history (D19) |
| Image and firmware catalog records its disks and VMs use (files are already in its cache) | Users, teams, role links, site policies (unless `--with-access`) |
| Its node networks (records; the bridges, NAT and leases are netd's and stay as they are) | **Sessions and API tokens, never** (secrets; re-issued on the other side) |
| Guest credentials referenced by those VMs | Cluster PKI, IPAM of cluster networks |
| Project records (id, name) those resources belong to | Quotas of those projects (the standalone host starts with none set) |
| `--with-access`: role links on those projects, and the users, teams and identities they name | |

Without `--with-access`, only break-glass administrators can manage the
imported projects on the standalone host until someone grants roles,
which is the safe default: access rights are not carried to a host the
cluster no longer controls unless an administrator says so. Identity
keys scoped to the departing node (D13) become plain `unix:<name>` /
`pam:<name>`; identities scoped to other nodes are dropped.

**Cluster networks.** A VM NIC on a `scope: cluster` network can't come
along: that network is the cluster's. For each such network the
administrator either:

- maps it to a node network: `--map-network <cluster-net>=<node-net>`.
  netd moves the port (`move_vm_port`, §11.3) from `br-int` to the node
  network's bridge **without recreating the tap or vhost-user socket**,
  so the running guest keeps its device. The NIC gets a new address from
  the node network; the guest learns it at its next DHCP renewal or
  reboot, and the VM shows `NetworkReady=False/AddressChanged` until netd
  sees the new lease (or `RestartRequired`), or
- lets it go (the default): the attachment is removed from the VM's
  spec (a spec change, so `generation` moves) and the port is unplugged;
  the running guest sees its link go down, and the NIC is gone at the
  next restart (`RestartRequired=True/NetworkRemoved`).

#### 5.8.2 Detach while the cluster is reachable

`gxctl node detach <node> [--map-network a=b …] [--with-access]` by an
administrator with `host.cluster`. The leader drives a **two-phase
handover**, so a crash at any point leaves the resources owned by
exactly one side:

1. **Freeze** (one write): node `Departing{plan id, revision R}`. Spec
   writes to its objects fail with `409 node_departing`; the scheduler
   skips it. Any unshipped metering hours are shipped and merged first
   (§13.1), so the cluster's ledger is complete.
2. **Bundle:** the node fetches `GET /cluster/v1/departure/<plan>`
   (records as of R, plus the plan's network mappings and their effect),
   writes them to `glidex.db.standalone-pending` (0600), `fsync`s it and
   acknowledges with the bundle's SHA-256.
3. **Commit** (one write on the leader, after the remove steps of §5.5
   step 2 for gateway, Raft and OVN membership, and deleting the
   departing VMs' logical ports): the node's objects are removed from
   the cluster store, IPAM reservations freed, node `Departed` with
   `departed_ids`, certificate deny-listed. The response carries a
   **departure receipt** (plan id, bundle hash, revision) signed with
   the cluster CA.
4. **Switch** (node): with the receipt in hand, it moves NICs as
   planned, renames the pending database to `glidex.db` (the old
   `node.db` is deleted), removes the `cluster` section and its
   certificate, and restarts standalone. It adopts its running VMs as
   after any restart. Its metering cursors carry on in the new
   database's ledger, so nothing is counted twice or lost.

Before step 3 commits, `gxctl node detach --abort <node>` (or a timeout,
default 10 min) returns the node to `Active` and the node discards the
pending database. After a crash between 3 and 4 the node finds the
pending database and the receipt on disk and finishes step 4; a pending
database **without** a receipt is only finished once the leader confirms
the commit (`GET /cluster/v1/departure/<plan>`), so the node never
becomes standalone while the cluster still owns its VMs.

#### 5.8.3 Detach while the cluster is unreachable

When no server can be reached (the host was cut off for good, or the
cluster is being abandoned), root on the host can run
`glidex-install --leave --keep-resources --offline`. The bundle is built
from the node's **cache** (`node.db`, §8.2), which holds exactly the
objects of §5.8.1 except `--with-access` data (a node never caches
users or links). The installer prints the revision the cache is at,
warns that changes made in the cluster since then are not included,
asks for confirmation, then performs step 4 of §5.8.2 and destroys the
node's private key so its old identity can never be used again.

The cluster still has the records. Its administrator runs
`gxctl node forget <node> --departed`: one write makes the node
`Departed` (not `Forgotten`: the VMs are not lost, they left), removes
its objects, frees their reservations and logical ports, and applies the
cluster-side steps of §5.5. Until then the cluster shows the node as
unreachable and keeps its VMs as `Unknown`, which is correct: it cannot
tell a departed node from a partitioned one.

**Why root on the node may do this alone:** root on a host already
controls its VMs, their memory and their disks. The offline path only
makes that explicit and keeps glidex's records consistent on the host.
It grants nothing in the cluster: the node's certificate is gone, and
the cluster side changes only through its own administrator.

### 5.9 Joining with resources (import)

A standalone host **with resources** (whether it was always standalone
or departed from another cluster) joins a cluster and brings them in.
The node gets a **new node id**; its VMs, disks, images, networks and
credentials become the target cluster's, placed or bound on it, and keep
running throughout.

1. On a target server: `gxctl cluster join-token --role agent
   --allow-import`. A token without `--allow-import` can't import (a
   plain join is refused for a host with resources, §5.2). Import joins
   are agent joins; `gxctl node promote <node>` makes it a server later.
2. On the host: `glidex-install --join https://<server>:8842 --import`.
   The host stays a fully working standalone host while the rest
   happens: **nothing is committed on either side until approval**.
3. The host uploads a manifest: projects, VMs, disks, images, node
   networks, credentials (with `--with-access`, also its role links,
   users, teams and identities), each with id, name and size. The leader
   builds an **import plan** and checks:

   | Check | Default | Override |
   |---|---|---|
   | Ids (VM, disk, image) already exist | refused, unless the existing record is in a `Departed` tombstone's `departed_ids` (the same resources coming back) | none: ids are kept (D19) |
   | Project names | refused when a project of that name exists | `--project <src>=<existing>` (merge into it) or `--project <src>=new:<name>` |
   | Network names (unique cluster-wide) | refused on conflict | `--network <src>=<new name>`; VM attachments are rewritten to match |
   | Credential names (unique per project) | refused on conflict | `--credential <project>/<src>=<new name>`; VM references rewritten |
   | Images | each imported as its own record, even if the catalog has the same checksum | none: `Linked` disks use the image file as their qcow2 backing file, so the image id can't change; duplicates can be cleaned up later |
   | Quotas of the target projects | refused if the import exceeds them | `--over-quota`: accepted, projects marked over quota as when an admin lowers a quota |
   | Node networks | stay `scope: node` on the new node | `--map-network <node-net>=<cluster-net>`: NICs move to the cluster network (`move_vm_port` to `br-int`, a new reservation; addresses change as in §5.8.1) |
   | `--with-access` identities | mapped by identity key, re-keyed to the new node id (D13); refused if a user with the same identity exists but differs | `--link-users`: map to the existing user |

   The plan is `PendingApproval` and is shown with
   `gxctl node import show <plan>`. Nothing is written yet except the
   plan itself.
4. A target administrator with `host.cluster` (and the per-project
   rights of §12.2 for what the plan merges) approves: `gxctl node import approve <plan>
   [mappings…]`. Validation runs again at approval, against the state at
   that moment.
5. **Commit.** Records are staged in chunks (`import_staging`, each
   write ≤ 4 MiB, §6.2), invisible to the API and controllers. Then
   **one write** creates the node (`Active`, `Draining` until the node
   confirms), signs its certificate, makes the staged records live with
   `status.placement` / `Disk.node` / image caches pointing at the new
   node, and allocates reservations for mapped networks. Membership and
   resources therefore appear together, or not at all.
6. **Switch** (host): it receives its certificate, keeps its standalone
   database as `glidex.db.pre-join-<ts>` (0600), opens the cluster
   store, starts the node role, and adopts its running VMs. netd's
   `sync_vms` is sent only once the watch has delivered the imported VMs
   (the networking.md §7.7 invariant), so no port is dropped in between.
   Mapped NICs are moved. The node confirms and becomes `Active`.

`gxctl node import reject <plan>` (or expiry, default 24 h) discards the
plan and the staged records; the host stays standalone, untouched.
Metering history and audit records are **not** imported (D19); export
them first (`GET /usage` CSV, audit export) if they matter.

### 5.10 Recipes

| Goal | Steps |
|---|---|
| **Form a cluster** from a standalone host | `gxctl cluster init` (§5.1): the host's resources become the cluster's, re-keyed to its node id. |
| **Leave a cluster with my VMs** | `gxctl node detach <node>` (§5.8.2), or `--offline` (§5.8.3). |
| **Move a host to another cluster** | Detach from A, then `join --import` into B (§5.9). Its VMs run throughout; only NICs on A's cluster networks need a mapping or are lost. |
| **Split a cluster** into A and B | Detach the first node of B and `cluster init` it; detach each other node of B and `join --import` into B. |
| **Merge two clusters** | Detach each node of B and `join --import` into A, in turn. Projects are merged or renamed per node by the import plans. |
| **Dissolve a cluster** | Detach every agent, then the servers one by one; the last voter's detach is the cluster's end (`gxctl cluster dissolve` does the last step: it detaches the last node and deletes the Raft log, keeping a final snapshot). |

### 5.11 Membership rules

- Voters: 1, 3 or 5. Other server hosts beyond 5 stay learners, which
  serve reads and can be promoted.
- Raft membership changes go through openraft's joint consensus, one
  change at a time. Removing, forgetting or detaching a voter transfers
  leadership away first when it is the leader.
- OVN NB/SB membership follows glidex's: the same server hosts, changed
  in the same command (§5.5 step 2), never left to drift.
- `gxctl cluster status` shows voters, learners, the leader, each
  node's phase, applied index and lag, heartbeat age, OVN NB/SB cluster
  status, chassis, and pending detaches and imports.

### 5.12 Disaster recovery

- `gxctl cluster snapshot <file>` streams a consistent snapshot (§6.2)
  from the leader. It is 0600 and contains credential and token
  hashes, so it should be stored like the database itself.
- **Quorum lost for good** (a majority of servers destroyed):
  `glidex-control-plane --force-new-cluster --from <snapshot or local
  db>` on one surviving server starts a new single-voter cluster from
  that state, and the other servers re-join with fresh Raft state
  (§5.7, D18). The same holds for OVN: `ovsdb-tool
  cluster-to-standalone`, then re-cluster. The recovery runbook covers
  both.
- Going back to standalone is a detach (§5.8). The `pre-cluster` backup
  of §5.1 is only for undoing an init that went wrong.

## 6. Replicated store

### 6.1 Store API (milestone C0)

Today, 40 write transactions in 7 modules open ReDB directly
(`store.rs` 8, `metering/ledger.rs` 10, `auth/store.rs` 5,
`credentials.rs` 5, `images/mod.rs` 5, `tenancy.rs` 4, `network.rs` 3).
All of them move behind one API:

```rust
pub trait Store: Send + Sync {
    /// Run `f` against the current state; persist exactly the writes it made.
    fn write<R>(&self, origin: Origin, f: impl FnOnce(&mut Tx) -> Result<R, StoreError>) -> Result<R, StoreError>;
    fn read(&self, c: Consistency) -> Result<ReadTx, StoreError>;
    fn revision(&self) -> u64;                       // last applied index
    fn subscribe(&self) -> broadcast::Receiver<Applied>;
}

pub struct Tx { /* wraps a ReDB WriteTransaction and records every put/delete */ }
pub enum Op { Put { table: TableId, key: Vec<u8>, value: Vec<u8> }, Delete { table: TableId, key: Vec<u8> } }
pub struct WriteSet { pub format: u16, pub origin: Origin, pub ops: Vec<Op> }
pub struct Applied { pub revision: u64, pub tables: SmallVec<[TableId; 4]>, pub keys: Vec<(TableId, Vec<u8>)> }
pub enum Consistency { Linearizable, Local }
```

- `Tx` exposes typed table access with the same semantics as ReDB
  (reads see the transaction's own writes) and records the ops.
- **`Local`** commits the ReDB transaction directly, so behaviour is
  unchanged.
- **`Replicated`**, on the leader:
  1. take the write lane (D3);
  2. run `f` on a ReDB write transaction, then **abort** it, keeping
     the write set;
  3. propose the write set; wait until it is committed **and applied
     locally**;
  4. release the lane and return `f`'s result.

  On a follower, `write` returns `NotLeader { leader }` (§6.4).
- `Commit` (`store.rs`) and the existing helpers become thin wrappers
  over `Tx`. `TableId` is a closed enum of today's tables (§6.6).
- A write set that is empty commits nothing and proposes nothing.

**Invariant.** Every change to the database goes through `Store::write`.
Applying the write sets of a `Local` store, in order, to an empty
database gives a byte-identical database. The C0 acceptance test checks
exactly that.

### 6.2 Raft integration

| Item | Choice |
|---|---|
| Library | `openraft`, version pinned in `Cargo.toml`, upgrades reviewed like the CH/OVS pins |
| Log store | `raft/log.redb` (separate file, so purging the log never touches the state machine) |
| State machine | `glidex.db`. Each entry is applied in **one** ReDB transaction together with the `raft_meta` table (`last_applied`, `last_membership`) |
| Entry size | ≤ 4 MiB; larger write sets are refused with `StoreError::TooLarge` (none exist today: images and disks are files, not values) |
| Transport | HTTPS on :8842, mTLS, server certificates only; `POST /raft/{append,vote,snapshot}` |
| Timeouts | heartbeat 250 ms, election 1–2 s (LAN); `cluster.raft.*` settings |
| Snapshots | every 10 000 entries or 64 MiB of log, whichever comes first; the log is purged up to the snapshot |
| fsync | log entries are fsynced before an ack; state-machine applies are durable through ReDB's commit |

Because applying an entry and advancing `last_applied` are one
transaction, a crash never applies an entry twice or skips one.

### 6.3 New tables

| Table | Key | Value |
|---|---|---|
| `raft_meta` | `last_applied`, `last_membership`, `cluster_id`, `feature_level` | — |
| `nodes` | node id | `Object<NodeSpec, NodeStatus>` (§7.1) |
| `join_tokens` | token id | `{sha256, kind: join{role, allow_import} \| rejoin{node, raft_intact}, expires_at, used_at?}` |
| `departures` | plan id | `{node, revision, mappings, with_access, bundle_sha256?, state: frozen \| acked \| committed \| aborted, deadline}` (§5.8.2) |
| `import_plans` | plan id | `{csr, manifest, mappings, checks, state: pending_approval \| approved \| committed \| rejected, expires_at}` (§5.9) |
| `import_staging` | `plan/table/key` | staged records, invisible until the commit write |
| `node_denylist` | certificate serial | `{node, at}` |
| `ipam_subnets` | network id | `{cidr, gateway, pool}` (§11.4) |
| `ipam_reservations` | `network/mac` | `{ip, vm_id, nic}` |
| `image_caches` | `image/node` | `{phase, bytes, verified_at}` (§9.2) |
| `ledger_inbox` | `node/subject/meter/hour` | shipped hourly rows (§13.1) |

### 6.4 Reads, writes and forwarding

- **Writes on a follower** are forwarded to the leader as a whole HTTP
  request over :8842. The follower adds:
  - the authenticated principal, as `X-Glidex-Principal` (user id,
    token id, Unix groups for this request);
  - `X-Glidex-Forwarded-By: <node>`.

  The leader accepts these headers **only** from a server certificate,
  and runs authorization itself. A request never crosses nodes without
  being authenticated first.
- **Reads** use `Consistency::Linearizable` by default: a ReadIndex
  round trip to the leader (openraft `ensure_linearizable`), then a read
  of the local replica. With quorum lost, reads fail with
  `503 cluster_unavailable` unless the caller sends
  `X-Glidex-Consistency: local`. `gxctl --stale` sets that header, and
  the response then carries `X-Glidex-Revision`.
- **Revision.** The Raft index of the last applied entry is the
  cluster-wide revision. `meta.resource_version` keeps its meaning (per
  object, incremented on each write).
- **API on every server.** Clients and the UI may use any server. A
  load balancer or DNS round robin in front is optional.

### 6.5 Watch

`GET /cluster/v1/watch?from=<revision>&kinds=vm,disk,network,…&node=<id>`
(:8842, node certificates) streams `Applied` notifications filtered to
the objects relevant to the caller (§8.2). If `from` is older than the
last snapshot, the server answers `410 Gone` and the node re-lists.
This is the list-then-watch pattern Kubernetes uses. On the server,
watch is fed by `Store::subscribe`; the existing `Bell` (`store.rs`)
becomes one of its subscribers.

### 6.6 Schema and versions

- `TableId` is append-only. A table can be retired but its id is never
  reused.
- **Feature level.** Each server reports the highest write-set
  `format` and schema it supports. The leader writes `feature_level` =
  the minimum across voters. New formats and schema migrations
  (reconciliation.md §6.6) are used only once every voter supports
  them, and run **as a single write** on the leader.
- Version skew: servers at N; agents at N or N−1. The node API on :8842
  is versioned (`/cluster/v1`), like netd's `hello`.

## 7. Nodes

### 7.1 Node resource

```rust
pub struct NodeSpec {
    pub name: String,                 // hostname by default, unique
    pub role: NodeRole,               // Server | Agent
    pub unschedulable: bool,          // drain
    pub labels: BTreeMap<String, String>,
}
pub struct NodeStatus {
    pub phase: NodePhase,             // Active | Draining | Departing | Departed | Removed | Forgotten (§5.3)
    pub departed_ids: Vec<String>,    // Departed only: VM, disk and image ids it took (§5.9 id check)
    pub common: StatusCommon,         // Ready condition, observed_generation
    pub advertise: SocketAddr,
    pub tunnel_ip: IpAddr,            // Geneve encap
    pub versions: Versions,           // glidex, CH, QEMU, OVS, OVN
    pub capacity: Resources,          // cpus, memory MiB, hugepages per size
    pub allocatable: Resources,       // capacity minus reserved (cluster.node_reserved)
    pub features: NodeFeatures,       // kvm, hypervisors, br_int_datapath (system|netdev), dpdk, iommu, physnets
    pub pci_devices: Vec<PciDevice>,  // what GET /pci-devices reports today
    pub heartbeat: Option<u64>,       // last transition write only (D7)
}
```

Nodes refresh `status` when it changes (versions, devices, physnets)
and at least every 10 minutes. Heartbeats themselves are not written
(D7).

### 7.2 Liveness

- A node sends `POST /cluster/v1/heartbeat` every 5 s to the leader, or
  to any server, which forwards it.
- Missing for 40 s (`cluster.node_grace_secs`): the node becomes
  `Ready=Unknown/NodeUnreachable`, and so do its VMs, with event
  `NodeUnreachable`. Nothing is stopped or moved (D8).
- After a leader change, each node's grace starts at the change (D7).

## 8. Controllers

### 8.1 Where each controller runs

| Controller | Runs on | Writes |
|---|---|---|
| Scheduler (§9.1) | leader | `vm.status.placement`, `disk.node` binding |
| Network (cluster, OVN NB) (§11) | leader | network status; OVN NB |
| IPAM (§11.4) | leader, inside admission and network writes | `ipam_*` |
| Node lifecycle | leader | node `Ready` transitions, VM `NodeUnreachable` conditions |
| Ledger close and roll-ups (§13) | leader | `usage_*`, `rate_5m` (closed) |
| VM (reconciliation.md §9) | **node**, for VMs placed on it | VM status (through the API, §8.3) |
| Disk (reconciliation.md §10.1) | **node**, for disks bound to it | disk status |
| Image cache (§9.2) | **node** | `image_caches/<image>/<node>` |
| Network (`scope: node`) (networking.md §11.2a) | **node**, for its node networks | network status |
| Metering sampler | **node** | node-local ledger, then shipped (§13) |

Cluster controllers run only while the node holds leadership. On losing
it they stop at the next await point. Their writes carry the leader's
term, so a deposed leader's late writes are rejected by Raft itself,
with no extra fencing needed for the store.

### 8.2 Node role: cache, watch and partitions

- The node role keeps `node.db` (ReDB, 0600), holding:
  - a **cache** of the objects it needs: its VMs, their disks, the
    networks and IPAM reservations they use, the credentials their
    cloud-init seeds need, its node networks, and the project and image
    catalog records all of these reference. That is exactly the
    offline departure bundle (§5.8.3), so a node cut off for good can
    still leave with consistent records;
  - an **outbox** of status writes not yet acknowledged;
  - its metering accumulators (§13).
- It lists, then watches from the last revision (§6.5). The cache is
  what the node's controllers read.
- **Partitioned from every server**, the node keeps acting on its
  cache, conservatively:
  - running VMs stay running, and crash restarts by restart policy
    continue (ports survive, reconciliation.md D16);
  - no new launches, deletions or disk operations;
  - status writes queue in the outbox.

  On reconnecting it sends the outbox with resource-version
  preconditions; on any conflict it drops that entry and re-lists.
  **Why:** a partition must not take guests down (reconciliation.md
  D1). Acting on intent the node can't confirm (a start, a delete)
  could contradict a decision made meanwhile.
- **Invariant (reconciliation.md D8, cluster form).** A node launches a VM only if
  `status.placement.node` is itself **in a revision it has received
  from a server within the grace period**. Re-placing a VM elsewhere
  requires its old node to be `forgotten --fenced` (§5.6), never merely
  unreachable.

### 8.3 Status writes from nodes

`PUT /cluster/v1/{kind}/{id}/status` (:8842, node certificates). The
body is the status plus the `resource_version` the controller read. The
leader checks, inside the write:

- the object is placed or bound on the calling node (node authorization,
  §12.3);
- the resource version still matches (else `409`, and the node
  re-reads);
- the rules of reconciliation.md §6.2 (`observed_generation`
  semantics; for its D12, the generation-conditional `spec.power` write).

Today's "controllers write status in one transaction" becomes "nodes
send status to the leader, which writes it in one transaction". The
envelope rules don't change.

### 8.4 Console, live stats and logs

A console session reaches the VM's node through the server: a WebSocket
to any server (rest-api.md), which opens
`GET /cluster/v1/vms/{id}/console` to the VM's node on :8842 with its
own certificate and relays both directions. The node checks that the
caller is a server certificate, then attaches to `console.sock` exactly
as the API does today (console.md). `GET /vms/{id}/stats` (metering.md
§9.4) and the console log download relay the same way. Authorization
happens once, on the server that took the client's request.

## 9. Scheduling and local resources

### 9.1 Scheduler

It runs on the leader, for every VM whose spec wants it running and
that has no `status.placement`, and again whenever a node or capacity
changes.

1. **Filters**, all must pass:
   - `spec.node` (an optional pin), if set;
   - `Ready=True` and not `unschedulable`;
   - allocatable CPU and memory minus what is already placed on the
     node. Placed VMs count whether running or not; overcommit is
     `cluster.scheduler.cpu_overcommit`, default 4.0, and memory 1.0;
   - hugepages when the VM needs them (`hugepages`, or vhost-user with
     the networking.md §0 rule);
   - the hypervisor type is installed;
   - vhost-user NICs need `br_int_datapath = netdev` (cluster networks)
     or the node network's bridge;
   - VFIO devices: each BDF is granted for that node
     (`pci.allow[].node`, §16) and free;
   - disks: every attached disk unbound or bound to this node (D6);
   - networks: a `scope: node` network on this node; a provider network
     whose physnet this node maps (§11.3).
2. **Score:** least allocated by `max(cpu share, memory share)`, then
   fewest VMs, then node name, so the outcome is deterministic.
3. **Bind** in one write: set `status.placement = {node, at}`, bind
   unbound disks to the node, and reserve IPAM addresses (§11.4).
   Condition `Scheduled=True`.

No node fits → `Scheduled=False/Unschedulable` with each node's
failing filter in the message (capped), retried on node or capacity
changes. Placement is never changed by the scheduler afterwards (D6).

### 9.2 Images and disks

- **Images:** the catalog record (`images`) stays cluster-wide.
  The files become a per-node cache, `image_caches/<image>/<node>`, with
  phase `Absent | Downloading | Ready | Failed`. A node downloads an
  image when a disk or VM bound to it needs one, with the same verified
  pipeline (images.md §5) and the same file names. Image delete
  finalizers wait for every node's copy to go.
- **Disks:** `Disk` gains `node: Option<NodeId>`. `POST /disks` with
  `node` binds at once; without it, the disk binds to the node of the
  first VM placed with it (like Kubernetes' `WaitForFirstConsumer`).
  The images.md §2 invariants (file named by id, orphans never deleted)
  hold on each node.
- **Quotas** stay cluster-wide, in the same admission write as today.

## 10. Installation

### 10.1 Roles and packages

| Role | Adds to today's install |
|---|---|
| server | the `cluster` config section, the `cluster-ca-key` credential, `ovn-central` (NB, SB, `ovn-northd`), `ovn-host` |
| agent | the `cluster` config section, `ovn-host` |

Minimum **OVN 24.03 LTS**. That covers the HA chassis groups used
here, and the multi-chassis port binding needed for live migration
later (§18). Ubuntu 26.04's packaged version should be checked
**(verify)**. `ovn-host` uses the host's OVS, so the OVS profile
(kernel or DPDK) is unchanged.

### 10.2 Configuration

`/etc/glidex/control-plane.json`:

```json
"cluster": {
  "role": "server",
  "listen": "0.0.0.0:8842",
  "advertise": "192.0.2.11:8842",
  "tunnel_ip": "192.0.2.11",
  "node_grace_secs": 40,
  "shared_local_accounts": false,
  "raft": { "heartbeat_ms": 250, "election_ms": [1000, 2000], "snapshot_entries": 10000 },
  "scheduler": { "cpu_overcommit": 4.0, "memory_overcommit": 1.0 },
  "node_reserved": { "cpus": 1, "memory_mib": 2048 },
  "ovn": {
    "underlay_mtu": 1500,
    "nat_supernet": "10.89.0.0/16",
    "edge": { "physnet": "uplink", "external_cidr": "192.0.2.0/24", "external_ip": "192.0.2.50", "gateway": "192.0.2.1", "gateway_nodes": ["h1", "h2", "h3"] },
    "dns_servers": ["192.0.2.53"]
  }
}
```

Without a `cluster` section the host is standalone (D5). The
addresses above are documentation examples (RFC 5737).

## 11. OVN networks

### 11.1 Ownership and naming

- Every NB row glidex creates carries `external_ids:glidex-owner=glidex`
  plus `glidex-network`, `glidex-vm-id` and `glidex-nic`, as with OVS
  objects (networking.md §8.1). The ownership invariant carries over:
  **glidex modifies or deletes only NB rows it owns.**
- Names: logical switch `gx-<network id>`, port `gx-<vm8>-<i>`
  (the tap name, so `iface-id` = port name), edge router `gx-edge`,
  address sets `gx_nodes` and `gx_nat_supernet`.
- The leader's network controller writes NB through `ovn-nbctl
  --db=ssl:<servers> --format=json`, with commands chained by `--` so
  each change is **one NB transaction**. It uses a client certificate
  from the cluster CA (credential `ovn-nb-client`). This lives in a new
  library crate, `glidex-ovn`, mirroring how `glidex-ovs` wraps
  `ovs-vsctl`. A native OVSDB JSON-RPC client is a later change.
- **Reconcile:** each round, the controller diffs the desired NB rows
  (from networks, IPAM and placements) against the NB rows glidex owns,
  and applies the difference. It reports, and never deletes, NB rows
  that glidex didn't create (same as networking.md §7.7 step 4).

### 11.2 Network modes on OVN

| Mode (`scope: cluster`) | OVN objects | Notes |
|---|---|---|
| **isolated** | one logical switch; DHCP options with no router | No path to the host: `br-int` has no host address, so the networking.md/security.md §8.4 fence holds by construction. |
| **nat** | logical switch, plus a router port on `gx-edge` (gateway `.1` of the subnet); DHCP options (router, DNS = `ovn.dns_servers`, MTU); SNAT `subnet → edge.external_ip` | `gx-edge` has one gateway port on the provider switch of `edge.physnet`, scheduled on an **HA chassis group** of `edge.gateway_nodes` (D11). |
| **provider** (bridged, optional VLAN) | logical switch with a `localnet` port `network_name=<physnet>`, `tag=<vlan>` | Guests use the LAN's own DHCP. Port security is MAC-only (no reservation to enforce). Nodes declare which physnets they map (§11.3). |

**Isolation on `gx-edge`.** The rules use the OVN address sets from
§11.1 (`gx_nodes`, `gx_nat_supernet`).

| Rule (logical router policy) | Effect |
|---|---|
| `ip4.dst == $gx_nat_supernet` → **drop** | NAT networks can't reach each other. Traffic within a subnet is switched and never reaches the router, so one rule isolates every NAT network. |
| `ip4.dst == $gx_nodes` → **drop**, higher priority | Guests can't reach glidex hosts through SNAT. `gx_nodes` holds every node's advertise, tunnel and LAN addresses. This keeps security.md §8.4's "the host answers only DHCP, DNS, ping": in OVN, DHCP is answered by OVN itself and DNS by the site resolvers. |

Packets addressed to the router itself (ping to the gateway) are
handled before policy routing, so they still work.

**Shared networks** (security.md §6.2.1) keep their meaning. Sharing
grants attach rights; it never routes between networks.

### 11.3 Hosts joining OVN (netd)

New netd ops, all on the full socket under the existing policy (§7.2 of
networking.md):

| Op | Args → result | Does |
|---|---|---|
| `ensure_ovn_chassis` | `{chassis: <node id>, sb_remotes, encap_ip, bridge_mappings: {physnet → bridge}, datapath}` → chassis state | Sets the `Open_vSwitch` `external_ids` (`system-id`, `ovn-remote`, `ovn-encap-type=geneve`, `ovn-encap-ip`, `ovn-bridge-mappings`), installs the certificates under `/etc/glidex/ovn/` (root, 0600), ensures `br-int` with the host's `datapath_type`, enables `ovn-controller`. Idempotent. |
| `ovn_status` | – → `{controller_running, sb_connected, br_int, ports: [{lport, ovn_installed}]}` | Read-only. |
| `leave_ovn` | `{confirm}` → – | Removes the glidex `external_ids` from `Open_vSwitch`, stops and disables `ovn-controller`, deletes the OVN certificates. Refused while a glidex VM port is on `br-int` (move or detach it first). `br-int` itself is left in place, as OVN leaves it. |
| `move_vm_port` | `{vm_id, nic_index, to_bridge, ovn_lport?}` → binding (+ `ipv4` on a NAT bridge) | Moves a VM port between `br-int` and a glidex bridge (either way) **without recreating the tap or vhost-user socket**: `del-port` then `add-port` with the new `external_ids` (and `iface-id` when `ovn_lport` is set), MTU re-applied, NAT reservation taken on the new bridge, the stored `vm_ports` record updated in the same step. The hypervisor keeps its tap fd; OVS reconnects a vhost-user client port by itself. Both bridges must have the same datapath type for vhost-user. Owner check as `detach_vm_port`. |

- Provider physnets map to glidex bridges that netd already manages,
  with uplinks and IP migration (networking.md §8). OVN only gets the
  mapping.
- `attach_vm_port` gains `ovn_lport: Option<String>`. When set, the
  bridge is `br-int` and the port gets
  `external_ids:iface-id=<lport>`. Everything else (tap or vhost-user
  creation, owner uid, MTU, queues, crash-restart reuse) is unchanged.
- netd's ownership markers extend to `br-int` ports with
  `glidex-owner=glidex`. netd never touches `br-int` flows; they are
  `ovn-controller`'s.

### 11.4 IPAM

- Subnets: the requested one, or the first free `/24` in
  `ovn.nat_supernet` (default `10.89.0.0/16`, next to netd's node
  `nat_supernet` `10.88.0.0/16`, so node and cluster networks never
  overlap; init refuses overlapping settings). Gateway `.1`, pool
  `.2–.254`. Same rules as networking.md decision 8.
- Reservations: allocated for each VM NIC at **placement** (§9.1 step
  3), never at launch, so a VM keeps its address across stops,
  restarts and, later, live migration. Freed by a new cluster finalizer
  `vm.ipam` when the VM is deleted.
- Each reservation becomes the logical port's `addresses = "<mac>
  <ip>"` and `port_security = "<mac> <ip>"`. This is the port security
  metering.md §16 lists as future work, for cluster networks.

### 11.5 VM port lifecycle

1. **Placement** (leader): IPAM reserves the address, and the network
   controller creates the logical port with `requested-chassis=<node>`.
   That binds the port to the VM's node, so if a VM somehow ran twice,
   the second copy would get no traffic.
2. **Launch** (node): the VM controller calls `attach_vm_port` with
   `ovn_lport`, then waits up to 10 s for `ovn_installed`
   (`ovn-controller` sets it once flows are in place). On timeout it
   launches anyway with `NetworkReady=False/PortNotInstalled`, which
   clears once installed.
3. **Stop, crash restart, delete:** as networking.md §11.4, with
   `release_vm` local to the node. The logical port is deleted when the
   VM is deleted (finalizer `vm.ipam`), so the address and port follow
   the VM, not its instances.
4. **Drift:** a logical port or chassis binding that is missing is
   re-created by the leader's network controller (event
   `PortRestored`). A missing local tap behaves as today
   (`RestartRequired=True/PortLost`).

### 11.6 MTU and offload

- OVN networks: guest MTU = `underlay_mtu − 58` (Geneve with OVN's
  options over IPv4), so 1442 by default. It is sent through the DHCP
  MTU option and set on the tap by netd. A jumbo-frame underlay
  (`underlay_mtu: 9000`) gives 8942.
- vhost-user on `br-int` needs the netdev datapath and everything in
  networking.md §0 findings 7–11 (hugepages, mempool driver, PMD
  placement, TSO). Nodes report `br_int_datapath`, and the scheduler
  filters on it (§9.1).

### 11.7 Default network

On a **fresh** cluster with an `ovn.edge` configured, the leader
creates the cluster NAT network `default`. On a cluster converted from
a standalone host, `default` already exists as that host's node network
(D10), so the cluster network is named `cluster-default`. Network names
are unique cluster-wide.

## 12. Security

### 12.1 Trust and secrets

| Asset | Where | Protection |
|---|---|---|
| Cluster CA key | server hosts | systemd credential, root-only; used only for signing CSRs and renewals |
| Node certificate and key | every host | `/var/lib/glidex-control-plane/cluster/` 0600, owned by the `glidex` user |
| OVN certificates | every host (netd), servers (NB/SB) | `/etc/glidex/ovn/` 0600 root; NB client certificate as a control-plane credential |
| Join and rejoin tokens | shown once | stored as SHA-256; single-use; TTL; read by the installer from stdin or a file only; `allow_import` and rejoin node id are part of the token record, not of what the host claims |
| Departure bundle (§5.8) | the departing host | written only to `glidex.db.standalone-pending` (0600) over mTLS; holds that node's VMs, credentials (hashes and SSH public keys) and, with `--with-access`, users and links. Never written anywhere else, never logged |
| Import manifest and staged records (§5.9) | target store | names, ids and sizes in the plan; full records only in `import_staging`, deleted on reject or expiry |
| Departure receipt | the departing host | signed by the cluster CA; proves the commit, carries no secret |
| `glidex.db` replicas | every server | 0600 as today. **Every server holds every credential hash, token hash and audit record**, so a server host must be trusted as much as today's single host. |
| `node.db` | every host | 0600. It holds only the cached objects of that node's VMs (including the guest credentials their seeds need). |

Raft, forwarding, watch and console relay are mTLS only. OVN NB and SB
connections are SSL only. The SB database uses OVN's role-based access
control (`ovn-controller` role), so a host's chassis can only change its
own rows.

### 12.2 Cedar changes (security.md §7)

- `Host` entities become per node, `Host::"<node id>"`, with parent
  `Cluster::"<cluster id>"`. System roles link to the `Cluster`, so
  they cover every host. Host-specific actions (OVS bridges, uplinks,
  DPDK, PCI devices, host paths) take the node's `Host` as resource, so
  a site can grant `host.network` on one node only.
- New actions: `host.cluster` (init, join and rejoin tokens, node
  remove, forget, purge, detach, import approve and reject, snapshots,
  dissolve) under `system-admin`; `node.drain` under `system-admin`.
  Approving an import also needs, from the approver, what the import
  amounts to (security.md §7.2 groups): `vm.write`, `disk.write` and
  `credential.write` on each existing project it **merges** into
  (`system.projects` for new ones), `quota.exceed` for `--over-quota`,
  and `system.identity` for `--with-access` users and links. An import
  can't add resources to a project its approver couldn't add them to by
  hand. A detach `--with-access` is audited with the full list of users
  and links it carries out.
- D13: identity keys become `unix:<name>@<node>` and
  `pam:<name>@<node>`, unless `shared_local_accounts` is set. The init
  migration (§5.1 step 4) re-keys existing identities to the init node.
- D14: on agent-only hosts, `api.sock` treats uid 0 and `glidex-admin`
  as ordinary users (security.md §5.2's break-glass branch is skipped).

### 12.3 Node authorization

Node certificates authenticate the node role (principal
`Node::"<id>"`). A site policy can't widen what nodes may do, because
it is enforced in code, as for netd ownership:

- status writes only for objects placed or bound on that node;
- watch and list only for those objects and the networks,
  reservations and credentials they reference;
- heartbeats and its own `nodes` status.

Only server certificates may use the Raft endpoints, forward requests
with `X-Glidex-Principal`, or relay consoles.

### 12.4 Audit

Every forwarded request is audited on the leader with the original
principal and `forwarded_by`. Node status writes are audited as
`system:node:<id>` only when they change `spec.power` (reconciliation.md
D12) or fail authorization. Cluster membership operations and
`--force-new-cluster` are always audited.

## 13. Metering in a cluster

### 13.1 Node-local accumulation, shipped hours

The metering ledger writes on every sampling round (metering.md §6.1,
`sample_secs` 30). In a cluster that would be one Raft entry per node
every 30 s with nothing but cursors in it. Instead:

- Each node keeps the **open** state in `node.db`: cursors, open hours
  and open 5-minute slots. The algorithm is the same as metering.md §6,
  run per node.
- When an hour closes (metering.md §6.5), the node ships its closed
  rows to `POST /cluster/v1/ledger`, keyed `node/subject/meter/hour`.
  The leader writes them to `ledger_inbox`, then merges them into
  `usage_hourly`, `rate_5m`, the daily and monthly rows in the same
  write. A key already merged is acknowledged and not merged again.
- The node marks the hour as shipped only after the acknowledgement.
  Shipping is at-least-once, and applying is idempotent by key, so the
  ledger stays **exactly once** (metering.md D8).
- Live rates (`GET /vms/{id}/stats`) come from the node (§8.4).

### 13.2 Meters on OVN networks

- **Per-VM NIC meters:** unchanged. OVS `Interface.statistics` of the
  tap or vhost-user port, now on `br-int` (`port_stats` matches
  `iface-id` to a VM).
- **Per-network totals:** metering.md D7 counts "bridge ingress", which
  doesn't exist for a network that spans hosts. On OVN networks the
  network total is **Σ VM-port tx** (traffic entering the network from
  VMs) **+ edge ingress** (traffic entering from the router or
  localnet), read on the active gateway node.
  *How* to read edge ingress per network is open: per-flow counters on
  the gateway chassis (`ovs-ofctl dump-flows br-int` by OVN flow
  cookie) or an OVN-level counter **(verify)** (O3).
- **External/internal split** (metering.md D6) uses nftables on the
  host's forward path, which OVN traffic never crosses. On OVN NAT
  networks the split is read at `gx-edge`. Until that is built and
  verified, OVN NAT networks report totals without a split, and the
  ledger flags them `split_unavailable`, rather than reporting wrong
  numbers (same principle as metering.md D17).

### 13.3 Fallback

If O3 turns out to have no workable OVN-side counter, the fallback is a
small tc program on each gateway node's localnet interface, counting
per source subnet. That is the only eBPF D15 allows.

## 14. Failure behaviour

| Failure | Effect | Recovery |
|---|---|---|
| Leader host dies | Election within ~2 s; cluster controllers restart on the new leader; in-flight writes fail with `503 leader_changed` (gxctl and the UI retry idempotent requests) | automatic |
| Minority of servers lost | No effect on API or VMs | Replace or re-join the servers |
| Majority of servers lost | API: `503 cluster_unavailable` (local stale reads allowed, §6.4). VMs keep running; nodes act on their cache (§8.2) | Bring servers back, or `--force-new-cluster` (§5.12) |
| Agent host partitioned | Its VMs keep running; cluster shows `NodeUnreachable` (D8) | Reconnects and sends its outbox (§8.2) |
| Agent host dead | VMs `Unknown`; nothing restarted elsewhere | Repair the host and rejoin it (§5.7), or `node forget --fenced` (§5.6) |
| Node crashes during a detach | Before commit: node stays `Departing`, the plan times out back to `Active`. After commit: the node finishes the switch from its receipt (§5.8.2) | automatic |
| Leader changes during an import | The plan and staged records are in the store; approval or commit is retried by the new leader; nothing is live until the commit write (D17) | automatic |
| Host loses its Raft or OVN database file | It must not rejoin as the same voter (D18) | Rejoin as a learner (§5.7) |
| Active gateway node dies | OVN fails SNAT over to the next chassis in the HA group; connections through it are cut, the gateway address stays the same | automatic |
| OVN NB/SB quorum lost | Existing flows keep forwarding (`ovn-controller` keeps its last state); network changes and new ports wait (`NetworkReady=False/OvnUnavailable`) | as for the glidex store |
| netd restart on a node | as today (networking.md §7.7); `br-int` ports and `iface-id`s are preserved | automatic |

## 15. Testing

- **C0:** write-set replay test (§6.1 invariant) over the whole
  existing test suite: run every test against a `Local` store that also
  records write sets, replay them into a fresh database, and compare
  table dumps.
- **Raft:** in-process clusters of 1, 3 and 5 control planes, using a
  transport with fault injection (drop, delay, partition, reorder).
  Tests: leader kill during a write lane; a partitioned leader's writes
  are rejected; snapshot install on a lagging learner; membership
  changes under load; `--force-new-cluster` from a snapshot.
- **Linearizability:** a register workload (quota counter) through the
  API with random partitions, checked by a history checker (the
  `porcupine` model, run offline).
- **Node role:** a fake leader feeding watches; tests for the
  partition outbox, the re-list after `410`, and the reconciliation.md D8 launch guard
  (§8.2).
- **OVN:** `glidex-ovn` unit tests with recorded `ovn-nbctl` command
  lines (as `RecordingExec`). Ignored e2e tests on **three test hosts
  or three nested-KVM glidex VMs**, each with OVS + OVN:
  1. VMs on one isolated network on different hosts ping each other;
     nothing reaches any host address.
  2. NAT network egress through `gx-edge`; kill the active gateway
     node; egress resumes from the same external IP.
  3. Two NAT networks can't reach each other.
  4. Port security drops a guest that changes its MAC or IP.
  5. Provider network on a VLAN via `localnet`.
- **Upgrade:** a rolling upgrade from N−1 to N with VMs running and
  writes continuing.
- **Membership (§5.5–5.7):** remove an empty agent and an empty server
  (Raft and OVN NB/SB membership both shrink); refuse removing the last
  gateway; forget a dead agent, then rejoin it with its disks intact and
  see its VMs adopted; rejoin a server with Raft state intact (same
  voter) and with `raft/log.redb` deleted (re-added as a learner, D18);
  a rejoin after a VM was re-placed is refused.
- **Detach (§5.8)**, each with VMs **running** on tap and on vhost-user
  NICs, checking no guest restarts:
  1. online detach with one cluster network mapped and one dropped: the
     standalone host owns exactly the bundle; the cluster has no record
     of them; ledger totals across both databases equal one host's run;
  2. crash injection at every step of §5.8.2 (before ack, between ack
     and commit, between commit and switch): afterwards each VM is owned
     by exactly one database;
  3. offline detach, then `forget --departed` on the cluster;
  4. `--with-access`: users and links arrive; sessions and tokens never.
- **Import (§5.9):** join a standalone host with running VMs into a
  cluster: rejected with a plain token; pending plan visible and
  nothing live before approval; conflicts in project, network and
  credential names each refused, then resolved by mapping; over-quota
  refused, then accepted with `--over-quota` by a holder of
  `quota.exceed`; reject leaves the host untouched; a leader kill
  between staging and commit leaves nothing live; after commit the VMs
  are adopted without a restart and netd drops no port.
- **Round trip:** detach a node from cluster A, import it into cluster
  B, detach it from B and import it back into A (accepted through A's
  `departed_ids`).

## 16. Edits to existing documents

| Document | Edit |
|---|---|
| README.md | Remove the clustering non-goal (keep: live migration, shared storage, until §18); add this document to the index. |
| architecture.md | Roles (§4), the `Store` layer, node role and watch; single-host picture stays as the standalone case. |
| reconciliation.md | §6.2 status writes through the leader (§8.3); §9.4 startup per role; D8 cluster form (§8.2); finalizer `vm.ipam`. |
| data-model.md | `status.placement`, `Disk.node`, `Network.scope`/`node`, `nodes` and `ipam_*` tables. |
| networking.md | netd ops `ensure_ovn_chassis`, `ovn_status`, `leave_ovn`, `move_vm_port`; `ovn_lport` on `attach_vm_port`; ownership on `br-int`; §11 network modes by scope. |
| security.md | §5.2 break-glass on servers only (D14); identity keys (D13); §7 `Host`/`Cluster` entities, `host.cluster`, node authorization (§12.3); §8.4 isolation on OVN (§11.2). |
| images.md | Per-node image caches; disk binding (§9.2). |
| metering.md | §5.4–5.5 on OVN networks, node-local accumulation and shipping (§13); `split_unavailable` flag. |
| installer.md | `--join [--import]`, `--rejoin`, `--leave [--keep-resources [--offline]]`, `--reset`, token from stdin or `--token-file`; roles and OVN packages; `cluster init` backup; refusing a plain join on a host with resources. |
| rest-api.md, cli.md, web-ui.md | `/cluster/*`, `/nodes`, `gxctl cluster init\|leave\|dissolve\|status\|snapshot\|join-token`, `gxctl node drain\|undrain\|remove\|forget\|purge\|rejoin-token\|detach\|promote\|import show\|approve\|reject`; a Cluster page with node phases and pending detaches and imports; `503 cluster_unavailable`, `409 node_departing`, `X-Glidex-Consistency`. |
| control-plane config | `cluster` section (§10.2); `pci.allow[].node`. |

## 17. Milestones

Each one is one PR or a short series, mergeable alone, with tests
passing in CI. C0 and C1 change nothing for standalone users.

| # | Scope | Acceptance |
|---|---|---|
| **C0** Store API | `Store`, `Tx`, write sets; all 40 `begin_write` sites moved; `Local` implementation; `subscribe` replaces the `Bell` | Every existing test passes; write-set replay test (§15); a `grep` lint in CI forbids `begin_write` outside `store/`. |
| **C1** Node identity in the model | `nodes` table with one implicit node `local`; `status.placement`, `Disk.node`, `Network.scope`; `Host::"<node>"` + `Cluster`; identity re-keying code (D13); role split inside the process (node and server controller sets) | Schema migration test from the current schema; API and UI show the node; Cedar tests for host- vs cluster-scoped actions. |
| **C2** Raft | `openraft` with ReDB log and state machine; mTLS transport; `cluster init`, join tokens and CSR signing, learners and voters, forwarding, ReadIndex reads, snapshots, `cluster snapshot`, `--force-new-cluster`, feature level | §15 Raft and linearizability tests; three control planes on one machine (separate directories and ports) survive killing the leader with writes continuing. |
| **C3** Node role over the network | watch, node cache and outbox, status writes (§8.3), heartbeats and liveness, console/stats relay, image caches, ledger shipping; agent-only installs | Two hosts: a VM created on the server runs on the agent; agent restart adopts its VMs; with servers unreachable, the agent's VMs keep running and the outbox drains on reconnect; ledger totals equal the single-host run of the same workload. |
| **C4** Scheduler | filters and score (§9.1), sticky placement, `spec.node`, disk binding, drain | Unit tests over node fixtures; e2e: VMs fill two nodes by capacity; a VM with a node-bound disk lands on that node; `Unschedulable` explains why. |
| **C5** OVN foundation | `ovn-central`/`ovn-host` install, OVN PKI, `ensure_ovn_chassis`, `glidex-ovn` crate, cluster IPAM, isolated and NAT networks on `gx-edge` with an HA gateway, port security, DHCP and MTU, cluster `default` network | §15 OVN e2e tests 1–4. |
| **C6** Provider networks and metering on OVN | localnet with VLAN tags, physnet mapping onto netd bridges, scheduler physnet filter; §13.2 meters with O3 resolved | e2e test 5; per-VM and per-network meters on OVN networks within 1 % of an `iperf3` byte count. |
| **C7** Operations and membership | node phases (§5.3), drain, remove, forget, purge, rejoin with D18; rolling upgrades with feature-level gating, certificate renewal, `cluster status` port checks, Cluster page in the UI, runbook (recovery of both Raft groups) | §15 upgrade and membership tests; certificate renewal under load; documented recovery exercised on the test cluster. |
| **C8** Detach and import | departure bundle, two-phase detach with receipt, offline detach and `forget --departed`, import plans with checks and mappings, approval, staged commit, `move_vm_port`, `leave_ovn`, `cluster dissolve` | §15 detach, import and round-trip tests, all with running VMs. |

Dependencies: C0 → C1 → C2 → C3 → C4. C5 needs C3 (node role) and
C1 (scope). C6 needs C5. C7 needs C2–C5. C8 needs C7; its parts without
cluster networks (no `move_vm_port` mapping) need only C7 without C5.

## 18. Later (needs its own design)

- **Shared storage classes** (`shared-fs` over NFS or CephFS; `rbd`
  through krbd), followed by **live migration** of tap VMs. With OVN,
  live migration means multi-chassis port binding
  (`requested-chassis=src,dst`) with `activation-strategy=rarp`. It
  also needs a CPU-model policy for Cloud Hypervisor, and the vhost-user
  migration status of Cloud Hypervisor checked **(verify)**.
- **Fencing and automatic evacuation** (IPMI or watchdog, RBD
  exclusive locks), lifting D8 for fenced nodes.
- **Routed networks, floating IPs, OVN load balancers, BGP** (FRR or
  OVN's dynamic routing), **security groups** (OVN port groups and
  ACLs). Together these are the prerequisites for Kubernetes on glidex
  with routable pod CIDRs, and for peering with outside Kubernetes
  clusters.
- **Placement groups / anti-affinity.**
- **A native OVSDB client** in place of `ovn-nbctl`; write-lane
  pipelining (D3).
- **Moving resources between hosts** (copying disk files to another
  node, offline or as part of live migration). Detach and import (§5.8,
  §5.9) move *ownership* with the host; the files never leave it.
- **Keeping addresses across a detach or import** for NICs on mapped
  networks (e.g. moving a cluster subnet with its VMs), instead of
  re-addressing them (§5.8.1).

## 19. Open questions

| # | Question |
|---|---|
| O1 | Should the cluster CA key live on every server (any server can sign) or only on the first (simpler, but signing stops if it is lost)? Current text: every server. |
| O2 | Is OVN 24.03+ what Ubuntu 26.04 ships, or does the pinned source build (networking.md §6.3) need an OVN component? |
| O3 | Per-network edge ingress and the external/internal split on `gx-edge`: which OVN counter, read where (§13.2)? |
| O4 | Should NAT networks be able to have their own egress IP (a router per network) in addition to the shared `gx-edge`? |
| O5 | Upper bound on agent count before watch fan-out from a single leader needs follower-served watches. 20 hosts is far from it; measure in C3. |
| O6 | On import, should images whose checksum matches a target catalog image be merged once no `Linked` disk uses the imported copy, or always kept as separate records (D19)? |
| O7 | Should a detach be able to take resources that are *not* on the node (e.g. a project's credentials no local VM references) for a host that will form a new cluster? Current text: only what its VMs reference. |
| O8 | Default expiry of a pending import plan (24 h) and of a frozen detach (10 min): long enough for a human approval, short enough not to block the node? |
