# Clustering: replicated control plane (Raft) and cluster networks (OVN)

> Status: **being implemented** (2026-10-08); §20 records what each landed milestone does and where it differs from this text. It lifts the
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

- A cluster of **up to 50 hosts and 2,000 VMs** (§4.1): 1, 3 or 5
  **server** hosts (Raft voters, API, cluster controllers), the rest
  **agent-only** hosts.
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
| D11 | **NAT egress is centralized on HA gateway chassis groups.** By default NAT networks share one cluster **edge router** (`gx-edge`). A project can instead create a **VPC router** (§11.2a) with its own external IP and gateway group, and attach several networks to it. Each router's SNAT is active on one gateway node, with standby nodes ready to take over. | Distributed SNAT needs an external IP per VM. Centralized SNAT needs one IP per router, gives every network a stable egress address, and is OVN's standard high-availability gateway pattern. The shared edge costs one external IP for the whole cluster; VPC routers give tenants their own egress address and routing between their own networks, at one external IP each. |
| D12 | **IPAM belongs to the control plane.** Subnets and per-NIC reservations live in the cluster store and are written into OVN port `addresses`. OVN's dynamic addressing is not used. | One source of truth that survives restarts and future live migration, and serves the security rules (port security, address sets). |
| D13 | **Local accounts are host-scoped.** `unix:` and `pam:` identities carry the node they were seen on, unless the site sets `cluster.shared_local_accounts: true` (accounts managed centrally, e.g. SSSD/LDAP). | `alice` on host 1 is not necessarily `alice` on host 2. Without this, any local user on any host could act as a same-named user elsewhere. |
| D14 | **Break-glass is honoured on server hosts only.** On an agent-only host, local root and `glidex-admin` get no cluster admin rights through `api.sock`. | Root on a server host already holds the whole database. Root on an agent host holds only its own VMs, and must not be able to escalate to the cluster. |
| D15 | **No eBPF datapath** (decision record). | OVS/OVN covers every port type glidex uses (tap, vhost-user, DPDK and AF_XDP uplinks) under one model. An eBPF datapath would cover tap only, so glidex would maintain two datapaths. It brings no migration or performance gain for tap VMs. eBPF stays a debugging tool (`retis`, `pwru`); external traffic on OVN is metered with conntrack accounting (D22), not eBPF. |
| D16 | **Resources change cluster only by an explicit handover.** Leaving with resources (detach) and joining with them (import) are two-phase: the losing side freezes, the gaining side commits, and only a commit (a signed receipt for detach, the approval write for import) lets either side act as owner. | At every instant each VM and disk is owned by exactly one database. A half-done move must never leave two control planes both entitled to start or delete a VM (reconciliation.md D8). |
| D17 | **An import needs the target's approval; a token alone can't bring resources in.** Membership and the imported records commit in one write. | A join token proves the host may join, not that its projects, names, credentials and quota use are acceptable to the target. Committing them together means there is never a member with half its resources, or resources with no member. |
| D18 | **A server that lost its consensus state never rejoins as the same voter**, for glidex's Raft and for OVN's NB/SB clusters alike. It is removed and re-added as a learner. | A voter must remember its vote; with an empty log it could vote twice in one term and elect two leaders. |
| D19 | **Ids travel, names are mapped, history stays.** VM, disk and image ids are kept on detach and import; project, network and credential names are mapped when they collide; audit and metering history stay in the database where they were recorded. | Disk and image files are named by id, and `Linked` disks are qcow2 overlays whose backing file is the image file (images.md §3), so changing ids would mean rewriting files under running VMs. Names are only labels (images.md §2). Usage and audit belong to the operator who recorded them; carrying them over would double-report in the target. |
| D20 | **Every server holds the cluster CA key, and the CA is rotated whenever a server leaves** (removed, forgotten or departed) or on demand (§12.5). Until the old CA is retired, a leaf certificate is accepted only if the leader issued it (`issued_certs`). | Any server can sign joins and renewals, so losing one server never stops certificate issuance. A server that leaves takes a copy of the key with it; rotation makes that copy worthless, and the issued-certificate check means it can't mint usable glidex certificates in the meantime. |
| D21 | **VPC routers are opt-in, per project** (§11.2a). Networks attached to the same VPC router route to each other; different routers, and the shared edge, are isolated from each other. Subnets still come from one cluster IPAM (no overlapping CIDRs in this revision). | Tenants get their own egress address and multi-subnet routing without giving up the isolation of §11.2. One address space keeps port security, metering by address and future peering simple. |
| D22 | **External traffic on OVN is metered from conntrack accounting in each router's SNAT zone** on the gateway node (§13.2), not from OVN flow statistics. | A connection is external exactly when it is SNATed, which is metering.md D6's definition. OVN exposes no counters on logical objects; OpenFlow statistics by cookie are ambiguous (32-bit cookie prefixes, cookie 0 for conjunctive flows), reset when flows are reinstalled, and depend on how the pipeline is laid out in each OVN version. |

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
| 6648/tcp | OVSDB over SSL, no RBAC role | servers ↔ servers | SB for `ovn-northd`, which must reach the SB leader; the chassis listener on 6642 only grants the `ovn-controller` role. Keep it from agents (firewall), as for 6643/6644 |
| 6081/udp | Geneve | all nodes | overlay traffic |

The installer still doesn't touch the firewall (README). `gxctl cluster
status` checks each port's reachability between nodes and reports the
gaps.

**Two Raft groups.** glidex's own (D1) and OVSDB's (OVN NB/SB) run on
the same server hosts and fail the same way: both need a majority of
servers. They are kept separate on purpose. glidex never stores its own
state in OVSDB, and OVN never reads the glidex store.

### 4.1 Scale envelope

What this architecture is sized for, and what limits it. Figures are
estimates from the measured metering sizes (metering.md §15.6) and
typical NVMe and LAN latencies; C2 and C3 measure them (§15).

| Dimension | Designed for | Ceiling | What limits it |
|---|---|---|---|
| Hosts | 50 | ~100 | The leader alone serves watches, heartbeats (one every 5 s per node), forwarded writes and console relays. OVN is not the limit: the Geneve mesh is n(n−1)/2 tunnels (4,950 at 100 hosts), and OVN runs at that size. |
| VMs | 2,000 | ~5,000 | The **replicated database on every server**: metering history is about 12 MB per VM at steady state (metering.md §15.6), so 24 GB at 2,000 VMs and 60 GB at 5,000, on each server. Snapshot installs and the network controller's full NB diff per round grow with it. |
| Sustained writes | ~5–20 per second at 2,000 VMs | ~150–300 per second | One write lane (D3). Each write waits for the leader's and a majority's log `fsync` plus the ReDB commit: about 3–8 ms on NVMe in a LAN. |
| Bursts | creating 500 VMs (~5–10 writes each) takes 20–40 s of write lane | | Same lane; admission stays correct, callers see latency. |
| Reads | grow with the number of servers | | ReadIndex round trips are batched by the leader (§6.4). |
| Metering shipping | ~4.5 MB per hour per 1,000 VMs (about 5 hourly rows of 387 B and 4 slot rows of 669 B per VM per hour) | | Negligible: a couple of 4 MiB entries per hour (§13.1). |
| Snapshot install on a new server | 24 GB: ~3 min at 1 GbE, ~30 s at 10 GbE | | Network, then disk. Snapshots are built on demand (§6.2), never periodically. |

**Required for these numbers** (both part of C0):

- **Status is written only when it changes.** Today
  `VmManager::write_status` (`controller/vm.rs:129`) stamps
  `last_reconciled_at` *before* it checks whether the status changed,
  so every resync round (`reconcile.resync_secs`, default 30 s) writes
  every VM. Under Raft that would be N/30 replicated writes per second
  (67 per second at 2,000 VMs) carrying nothing. `last_reconciled_at`
  is excluded from the comparison and written with another change, or
  at most every 10 minutes.
- **OVN NB writes go through `ovn-nbctl` in daemon mode** (§11.1), so a
  command doesn't re-download the NB tables it needs every time.

**Beyond the ceiling**, in order: move metering history out of the
replicated store (a per-server ledger fed by the same shipped hours, or
an external time-series store), serve watches from followers, pipeline
the write lane (D3), and replace `ovn-nbctl` with a native OVSDB client
that applies incremental updates.

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
   as the encrypted systemd credential `cluster-ca-key`
   (`systemd-creds encrypt`, bound to the host key, and to the TPM2 when
   there is one). Every server holds it (D20); agents never do.
   Generate this node's certificate (`CN=node:<node-id>`,
   SAN = advertise address, 1 year, renewed at two thirds of its life
   through the API) and record its serial in `issued_certs`. Built on
   `glidex-tls`.
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
   administrator passes `--force` (§5.11). The leader then sends the CA
   key to the new server over mTLS (`PUT /cluster/v1/ca`, server
   certificates only), which stores it as its own encrypted
   `cluster-ca-key` credential (D20). The key never goes through the
   Raft log, so it is never in a snapshot or in `gxctl cluster snapshot`
   output.

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
   6. **CA rotation:** if the node was a server, the leader starts a CA
      rotation (§12.5), since the node held the CA key.
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
OVN membership with `cluster/kick`, chassis, and CA rotation for a
server, whose disk may still hold the key) are applied to it. A dead
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
   the cluster CA. If the node was a server, the leader then starts a
   CA rotation (§12.5).
4. **Switch** (node): with the receipt in hand, it moves NICs as
   planned, renames the pending database to `glidex.db` (the old
   `node.db` is deleted), removes the `cluster` section, its
   certificate and, on a server, its `cluster-ca-key` credential, and
   restarts standalone. It adopts its running VMs as
   after any restart. Its metering cursors carry on in the new
   database's ledger, so nothing is counted twice or lost.

Before step 3 commits, `gxctl node detach --abort <node>` (or a timeout:
`cluster.detach_freeze_secs`, default 600, or `--timeout` on the
command) returns the node to `Active` and the node discards the pending
database. After a crash between 3 and 4 the node finds the
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
node's private key so its old identity can never be used again. A
server also deletes its `cluster-ca-key` credential. The cluster can't
know that happened, so `forget --departed` of a server always rotates
the CA (§12.5).

The cluster still has the records. Its administrator runs
`gxctl node forget <node> --departed`: one write makes the node
`Departed` (not `Forgotten`: the VMs are not lost, they left), removes
its objects, frees their reservations and logical ports, and applies the
cluster-side steps of §5.5 (including CA rotation for a server). Until
then the cluster shows the node as
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
   | Images | each imported as its own record, even if the catalog has the same checksum | none: `Linked` disks use the image file as their qcow2 backing file, so the image id can't change; duplicates stay separate until a shared image store exists (§18) |
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

`gxctl node import reject <plan>` (or expiry: `cluster.import_plan_ttl_secs`,
default 86400, or `--plan-ttl` on `join-token --allow-import`) discards the
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
| Snapshots | **built on demand, never periodically**: the state machine is already durable in ReDB, so a snapshot is a stream of one ReDB read transaction (with `last_applied` read in the same transaction) when a follower is too far behind or a learner joins. The log keeps the last 10 000 entries (`raft.log_keep_entries`) after every voter has applied them. A periodic full copy of a database of tens of GB (§4.1) would cost more than it saves. How this maps onto openraft's snapshot builder is checked in C2 **(verify)** |
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
| `routers` | router id | `Object<RouterSpec, RouterStatus>` (§11.2a) |
| `ipam_external` | external IP | `{router}`: addresses taken from `ovn.edge.external_pool` |
| `issued_certs` | certificate serial | `{node, kind: node \| ovn-chassis \| ovn-db \| nb-client, issuer, not_after}` (D20) |
| `ca_bundle` | `current` | trusted CA certificates (public only) and rotation state (§12.5) |
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

**OVN 26.03** (checked 2026-10-08): Ubuntu 26.04 ships `ovn-central`,
`ovn-host` and `ovn-common` **26.03.0-2** in `resolute/main`, depending
on `openvswitch-switch (>= 2.17.0~)`, so they run on the archive's OVS
3.7.1. The man pages of that version document everything this design
uses: HA chassis groups, `requested-chassis` with `activation-strategy`
(live migration, §18), `options:snat-ct-zone` on routers (§13.2), and
the `dynamic-routing-*` options (BGP, §18). glidex pins **26.03 as the
minimum** and installs the distro packages. OVN branches an LTS every
two years and the previous LTS was 24.03, so 26.03 should be the
current LTS; confirm on ovn.org's LTS page when pinning.

No OVN source build is needed, including on hosts that use the pinned
OVS source build (networking.md §6.3). That build stops and disables
the distro OVS service but leaves its package installed, so the
`ovn-host` dependency still resolves. It uses `--localstatedir=/var`, so
`ovn-controller` finds OVSDB at the usual
`/var/run/openvswitch/db.sock`. `ovn-host` uses the host's OVS, so the
OVS profile (kernel or DPDK) is unchanged.

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
  "raft": { "heartbeat_ms": 250, "election_ms": [1000, 2000], "log_keep_entries": 10000 },
  "scheduler": { "cpu_overcommit": 4.0, "memory_overcommit": 1.0 },
  "node_reserved": { "cpus": 1, "memory_mib": 2048 },
  "detach_freeze_secs": 600,
  "import_plan_ttl_secs": 86400,
  "ca_rotation_grace_secs": 604800,
  "ovn": {
    "underlay_mtu": 1500,
    "nat_supernet": "10.89.0.0/16",
    "edge": {
      "physnet": "uplink", "external_cidr": "192.0.2.0/24", "gateway": "192.0.2.1",
      "external_ip": "192.0.2.50",
      "external_pool": "192.0.2.64/26",
      "gateway_nodes": ["h1", "h2", "h3"]
    },
    "snat_ct_zones": [60000, 64999],
    "dns_servers": ["192.0.2.53"]
  }
}
```

Without a `cluster` section the host is standalone (D5).
`detach_freeze_secs`, `import_plan_ttl_secs` and `ca_rotation_grace_secs`
are the defaults; `gxctl node detach --timeout`, `join-token
--plan-ttl` and `cluster rotate-ca --grace` override them per operation.
`external_ip` is the shared edge's address; `external_pool` holds the
addresses given to VPC routers (§11.2a). Neither may overlap the other or
the gateway. The
addresses above are documentation examples (RFC 5737).

## 11. OVN networks

### 11.1 Ownership and naming

- Every NB row glidex creates carries `external_ids:glidex-owner=glidex`
  plus `glidex-network`, `glidex-vm-id` and `glidex-nic`, as with OVS
  objects (networking.md §8.1). The ownership invariant carries over:
  **glidex modifies or deletes only NB rows it owns.**
- Names: logical switch `gx-<network id>`, port `gx-<vm8>-<i>`
  (the tap name, so `iface-id` = port name), edge router `gx-edge`,
  VPC routers `gxr-<router id>`, address sets `gx_nodes`,
  `gx_nat_supernet` and, per VPC router, `gx_r_<router8>_nets`.
- The leader's network controller writes NB through `ovn-nbctl
  --db=ssl:<servers> --format=json`, with commands chained by `--` so
  each change is **one NB transaction**. It uses a client certificate
  from the cluster CA (credential `ovn-nb-client`). This lives in a new
  library crate, `glidex-ovn`, mirroring how `glidex-ovs` wraps
  `ovs-vsctl`. `ovn-nbctl` runs in **daemon mode** (`ovn-nbctl
  --detach`, commands sent with `OVN_NB_DAEMON` set), started and
  supervised by the control plane on the leader. It keeps an in-memory
  replica of NB, so a command doesn't re-download the tables it needs
  each time (§4.1). A native OVSDB JSON-RPC client is a later change.
- **Reconcile:** each round, the controller diffs the desired NB rows
  (from networks, IPAM and placements) against the NB rows glidex owns,
  and applies the difference. It reports, and never deletes, NB rows
  that glidex didn't create (same as networking.md §7.7 step 4).

### 11.2 Network modes on OVN

| Mode (`scope: cluster`) | OVN objects | Notes |
|---|---|---|
| **isolated** | one logical switch; DHCP options with no router | No path to the host: `br-int` has no host address, so the networking.md/security.md §8.4 fence holds by construction. |
| **nat** | logical switch, plus a router port on `gx-edge`, or on the network's VPC router when it has one (§11.2a), with gateway `.1` of the subnet; DHCP options (router, DNS = `ovn.dns_servers`, MTU); SNAT `subnet → <router's external IP>` | Each router has one gateway port on the provider switch of `edge.physnet`, scheduled on an **HA chassis group** of `edge.gateway_nodes` or the VPC router's own gateway nodes (D11). Gateway nodes must have `br_int_datapath = system` (§13.2). |
| **provider** (bridged, optional VLAN) | logical switch with a `localnet` port `network_name=<physnet>`, `tag=<vlan>` | Guests use the LAN's own DHCP. Port security is MAC-only (no reservation to enforce). Nodes declare which physnets they map (§11.3). |

**Isolation on `gx-edge`.** The rules use the OVN address sets from
§11.1 (`gx_nodes`, `gx_nat_supernet`). VPC routers use the same rules
with one exception (§11.2a).

| Rule (logical router policy) | Effect |
|---|---|
| `ip4.dst == $gx_nat_supernet` → **drop** | NAT networks can't reach each other. Traffic within a subnet is switched and never reaches the router, so one rule isolates every NAT network. |
| `ip4.dst == $gx_nodes` → **drop**, higher priority | Guests can't reach glidex hosts through SNAT. `gx_nodes` holds every node's advertise, tunnel and LAN addresses. This keeps security.md §8.4's "the host answers only DHCP, DNS, ping": in OVN, DHCP is answered by OVN itself and DNS by the site resolvers. |

Packets addressed to the router itself (ping to the gateway) are
handled before policy routing, so they still work.

**Shared networks** (security.md §6.2.1) keep their meaning. Sharing
grants attach rights; it never routes between networks.

### 11.2a VPC routers

A **VPC router** is a project-owned logical router. NAT networks
attached to it route to each other and share its egress. It is an
alternative to the shared `gx-edge`, not a replacement (D21).

```rust
pub struct RouterSpec {
    pub name: String,                     // [a-z0-9-], ≤ 32, unique per project
    pub external: bool,                   // has a gateway: SNAT to the outside (default true)
    pub external_ip: Option<Ipv4Addr>,    // None: next free address of ovn.edge.external_pool
    pub gateway_nodes: Option<Vec<NodeId>>, // None: ovn.edge.gateway_nodes
}
pub struct RouterStatus {
    pub common: StatusCommon,             // Ready, observed_generation
    pub external_ip: Option<Ipv4Addr>,    // the address actually reserved (ipam_external)
    pub snat_ct_zone: Option<u16>,        // from ovn.snat_ct_zones (§13.2)
    pub active_gateway: Option<NodeId>,   // where SNAT runs now
}
// Network gains: pub router: Option<String>   // nat mode, scope cluster; immutable after create
```

| Router | OVN objects |
|---|---|
| `external: true` | logical router `gxr-<id>` with a gateway port on the `edge.physnet` provider switch holding `external_ip`; an HA chassis group of its gateway nodes; one SNAT entry per attached network (`subnet → external_ip`); `options:snat-ct-zone` from `ovn.snat_ct_zones`; a default route to `edge.gateway` |
| `external: false` | logical router only: its networks route to each other and reach nothing else |

**Isolation.** Each VPC router gets the `gx_nodes` drop, and instead of
`gx-edge`'s supernet drop it gets
`ip4.dst == $gx_nat_supernet && ip4.dst != $gx_r_<router8>_nets` →
**drop**, where `gx_r_<router8>_nets` lists its own networks' subnets.
Its networks therefore reach each other, but not other VPCs, not
networks on `gx-edge`, and not glidex hosts. Other routers' subnets can
only be reached through their external IP, which only accepts replies
(SNAT, no DNAT). Floating IPs and port forwarding are later work (§18).

**Rules.**

- Created and deleted by a project's `network.manage` holders
  (security.md §7.2). Choosing `external_ip` or `gateway_nodes`
  explicitly needs `host.network`, because both are site resources.
- New quotas: `routers` (default 1 per project) and `external_ips`
  (default 1). They are checked in the same admission write that
  reserves the address in `ipam_external` (reconciliation.md D5). The
  external pool is a site resource, and running out of it is
  `409 external_pool_exhausted`.
- A network's `router` is set at creation and immutable. Subnets come
  from the cluster IPAM as for any NAT network (§11.4, no overlap).
  Deleting a router is refused while networks are attached.
- The network controller (§11.1) owns routers like networks: it creates,
  repairs and deletes `gxr-*` rows, and reports `Ready` with the active
  gateway.

### 11.3 Hosts joining OVN (netd)

New netd ops, all on the full socket under the existing policy (§7.2 of
networking.md):

| Op | Args → result | Does |
|---|---|---|
| `ensure_ovn_chassis` | `{chassis: <node id>, sb_remotes, encap_ip, bridge_mappings: {physnet → bridge}, datapath}` → chassis state | Sets the `Open_vSwitch` `external_ids` (`system-id`, `ovn-remote`, `ovn-encap-type=geneve`, `ovn-encap-ip`, `ovn-bridge-mappings`), installs the certificates under `/etc/glidex/ovn/` (root, 0600), ensures `br-int` with the host's `datapath_type`, enables `ovn-controller`. Idempotent. |
| `ovn_status` | – → `{controller_running, sb_connected, br_int, ports: [{lport, ovn_installed}]}` | Read-only. |
| `leave_ovn` | `{confirm}` → – | Removes the glidex `external_ids` from `Open_vSwitch`, stops and disables `ovn-controller`, deletes the OVN certificates. Refused while a glidex VM port is on `br-int` (move or detach it first). `br-int` itself is left in place, as OVN leaves it. |
| `move_vm_port` | `{vm_id, nic_index, to_bridge, ovn_lport?}` → binding (+ `ipv4` on a NAT bridge) | Moves a VM port between `br-int` and a glidex bridge (either way) **without recreating the tap or vhost-user socket**: `del-port` then `add-port` with the new `external_ids` (and `iface-id` when `ovn_lport` is set), MTU re-applied, NAT reservation taken on the new bridge, the stored `vm_ports` record updated in the same step. The hypervisor keeps its tap fd; OVS reconnects a vhost-user client port by itself. Both bridges must have the same datapath type for vhost-user. Owner check as `detach_vm_port`. |
| `ct_external_counters` | – → per `(zone, VM address)`: `ext_tx`/`ext_rx` bytes and packets (L2-normalized), `epoch` | Read-only, full socket only. Gateway nodes (§13.3): conntrack accounting in glidex's SNAT zones, cumulative and persisted. |

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
| Cluster CA key | every server host (D20) | encrypted systemd credential (`systemd-creds`, host key and TPM2 when present); sent only server-to-server over mTLS (§5.2), never through Raft or snapshots; used only for signing CSRs and renewals; rotated when a server leaves (§12.5) |
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
`--force-new-cluster` are always audited, and so is every step of a CA
rotation (§12.5).

### 12.5 CA rotation

Every server holds the CA key (D20), so **a server that leaves takes a
copy of it**, whether it was removed (§5.5), forgotten (its disk may be
recovered, §5.6) or departed (§5.8, including offline). The leader
therefore rotates the CA:

- **automatically**, after the leave of any server commits;
- **on demand**, with `gxctl cluster rotate-ca [--grace <secs>]`
  (`host.cluster`, step-up), e.g. on suspected compromise;
- **before expiry**, starting one year before the CA's `not_after`.

Steps, each recorded in `ca_bundle` so a new leader resumes where the
old one stopped:

1. **New CA.** The leader generates a new CA key and certificate, and
   sends the key to every remaining server (`PUT /cluster/v1/ca`), which
   stores it as `cluster-ca-key` beside the old one. The rotation waits
   until every server has acknowledged.
2. **Trust both.** One write sets the trust bundle to {old, new}. Every
   node installs it for :8842. netd installs it as the CA file of OVN
   (`ovn-controller`; and, on servers, the NB/SB `ovsdb-server`s and
   `ovn-northd`). The OVN processes are restarted if they don't reload
   CA files on their own **(verify)**: ovn-controller keeps forwarding
   on installed flows across a restart (§14).
3. **Re-issue.** Every node renews its node certificate, and netd its OVN
   certificates, by CSR. They are signed with the new CA and recorded in
   `issued_certs`. Nodes report their certificates' issuer in their
   status.
4. **Retire the old CA.** Once every `Ready` node reports only new-CA
   certificates, or after `ca_rotation_grace_secs` (default 7 days),
   one write sets the bundle to {new}. Nodes then stop accepting
   old-CA certificates, and every server deletes the old key. A node
   still on the old CA (unreachable during the whole rotation) can't
   connect any more; it comes back with a rejoin token (§5.7).

**During the window**, glidex's own port (:8842) accepts a leaf
certificate only if its serial is in `issued_certs` and not in
`node_denylist`. A departed server can sign certificates with the old
key, but none of them is in the registry, so they don't work on :8842,
even before step 4. OVN's SSL connections check only the CA, so for OVN
the window stays open until step 4. SB role-based access limits a
chassis to its own rows, and `requested-chassis` keeps the port bindings
of glidex VMs on their own nodes (§11.5). Steps 1–3 are fast; the grace
period only exists for unreachable nodes, so an administrator who wants
the OVN window closed at once runs `rotate-ca --grace 0` and lets those
nodes rejoin.

If another server leaves during a rotation, the rotation restarts from
step 1 with a fresh CA: the new key has been exposed too.

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
- **External/internal split** (metering.md D6): from **conntrack
  accounting in each router's SNAT zone** (D22, §13.3). Outbound bytes of
  a connection SNATed for VM address A are A's `ext_tx`; its reply bytes
  are A's `ext_rx`. Everything else a VM sends or receives is internal.
- **Per-network totals:** metering.md D7 counts bridge ingress, and a
  network that spans hosts has no single bridge. On OVN NAT and isolated
  networks the total is **Σ VM-port tx + Σ external inbound** (the
  conntrack reply bytes of the network's VMs). Every byte is counted
  once, where it entered the network's VMs' address space: traffic
  between two networks of one VPC router counts in the sender's network
  only, so the totals of a VPC's networks add up without double
  counting, as D7 intends for bridges.
- **Provider networks** report per-VM meters only, with no network total
  and no split (flag `network_total_unavailable`). The `localnet` patch
  port is shared by every provider network on the same physnet, so no
  per-network source exists. A wrong total is worse than none
  (metering.md D17).

### 13.3 Why conntrack, and how (O3)

Findings from the OVN 26.03 documentation (`ovn-nb(5)`, `ovn-sb(5)`,
`ovn-northd(8)`, `ovn-architecture(7)`):

| Candidate | Finding | Verdict |
|---|---|---|
| Counters on NB/SB objects | None exist. Router policies, NAT entries, ACLs and ports have no byte or packet counters. | not available |
| IPFIX sampling (`Sample`, `Sample_Collector`, ACL `sample_new`/`sample_est`) | Probability up to 65535/65535 (every packet), exported as IPFIX through OVS. At 100 % that is one export per packet on the datapath. | too costly for billing |
| OpenFlow statistics per logical flow | `ovn-controller` sets each OpenFlow flow's cookie to the first 32 bits of its logical flow's UUID, which is "not necessarily unique"; conjunctive-match flows all use cookie 0. Counters reset when flows are reinstalled (recompute, upgrade). Per-network egress could come from the per-NAT-entry flows of the router's SNAT stage. Per-VM ingress could come from the per-address flows of ARP/ND resolution (`outport == P && reg0 == A`), but on a VPC router those also count traffic routed between its own networks. The layout changes between OVN versions. | rejected as primary |
| **Conntrack in the router's SNAT zone** | Every SNATed connection is committed in the router's SNAT zone on the chassis that runs it: the active gateway, since `snat` entries on a distributed gateway port are chassis-resident (`is_chassis_resident(cr-…)`). The original tuple holds the VM's address. With accounting on, each entry carries bytes and packets for both directions. `options:snat-ct-zone` lets glidex fix the zone per router. | **chosen** (D22) |

**Design.**

- The network controller gives each router (the shared edge and every
  VPC router) a zone from `ovn.snat_ct_zones` (default 60000–64999) and
  sets `options:snat-ct-zone`.
- Gateway nodes need `br_int_datapath = system` (the kernel datapath
  uses netfilter conntrack, which has netlink events). OVS's userspace
  conntrack has no equivalent. Edge configuration and VPC router
  creation refuse other gateway nodes.
- netd on a gateway node sets `net.netfilter.nf_conntrack_acct=1`
  (recorded like `ip_forward`, networking.md §10; it adds a small
  extension to every conntrack entry on the host). It then keeps
  per-(zone, VM address, direction) **cumulative** counters:
  - it subscribes to conntrack `DESTROY` events (netlink, in-process;
    the `conntrack` CLI is only for debugging) to capture the final
    counters of closed connections;
  - it dumps the glidex zones once per metering round to account live
    connections, as deltas against each connection's last seen counters
    (keyed by conntrack id).
- New read-only netd op **`ct_external_counters`** (full socket, never
  the status socket, like `nat_counters`). It returns the cumulative
  counters, L2-normalized like `nat_counters` (+14 bytes per packet),
  with an epoch for metering's reset rule. Before replying, netd
  persists the totals and per-connection baselines to its database, so
  the counters stay monotonic across netd restarts.
- **Known gap:** a connection that opens and closes entirely while netd
  is down is never seen. The metering ledger flags the hour
  (`ext_gap`), as for other collection gaps (metering.md §6.4).
- **Gateway failover** resets connections anyway (§14). Each node's
  counters are a separate metering source, `(node, zone)`, and the
  cluster ledger sums them per VM.
- **Floating IPs** (later, §18) would SNAT on the VM's own chassis. The
  same collector then runs on every node, with no change to the
  design.
- **To check on a real host in C6 (verify):**
  - `snat-ct-zone` is honoured for routers with a distributed gateway
    port, not only for gateway routers;
  - `DESTROY` events carry the counters of entries OVS committed;
  - zones from `snat_ct_zones` don't clash with the zones
    `ovn-controller` allocates for itself;
  - per-VM `ext_*` within 1 % of an `iperf3`/`curl` byte count through
    NAT.

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
| A server leaves (any way) | CA rotation starts (§12.5); certificates still work throughout steps 1–3 | automatic; unreachable nodes rejoin after step 4 |
| Leader changes during a CA rotation | `ca_bundle` records the step; the new leader resumes it | automatic |
| netd down on the active gateway | NAT keeps working (OVN); connections that open and close meanwhile are not metered (`ext_gap`, §13.3) | automatic |
| External pool exhausted | VPC router creation fails with `409 external_pool_exhausted`; existing routers unaffected | site enlarges `ovn.edge.external_pool` |
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
- **CA rotation (§12.5):** remove a server; rotation completes with no
  failed request; a certificate signed with the old key by the removed
  server (simulated) is refused on :8842 before and after step 4; an
  unreachable agent is cut off at step 4 and rejoins; a leader kill at
  each step resumes; OVN keeps forwarding across the CA switch.
- **VPC routers (§11.2a):** two networks on one VPC router reach each
  other; neither reaches a network on `gx-edge`, another VPC, or a host;
  egress leaves from the router's own external IP; gateway failover
  keeps that IP; quotas and pool exhaustion refuse creation.
- **External metering on OVN (§13.3):** per-VM `ext_*` and network
  totals within 1 % of `iperf3`/`curl` byte counts, for the shared edge
  and a VPC router; a netd restart in the middle of a long transfer
  loses nothing; a short connection made while netd is stopped is
  flagged `ext_gap`.
- **Scale (§4.1):** a synthetic load of 2,000 VMs on 50 simulated nodes
  (fake netd, real Raft on three hosts): steady-state write rate, write
  latency per entry, leader CPU, database size, and snapshot install
  time, recorded in this document.

## 16. Edits to existing documents

| Document | Edit |
|---|---|
| README.md | Remove the clustering non-goal (keep: live migration, shared storage, until §18); add this document to the index. |
| architecture.md | Roles (§4), the `Store` layer, node role and watch; single-host picture stays as the standalone case. |
| reconciliation.md | §6.2 status writes through the leader (§8.3); §9.4 startup per role; D8 cluster form (§8.2); finalizer `vm.ipam`. |
| data-model.md | `status.placement`, `Disk.node`, `Network.scope`/`node`, `nodes` and `ipam_*` tables. |
| networking.md | netd ops `ensure_ovn_chassis`, `ovn_status`, `leave_ovn`, `move_vm_port`, `ct_external_counters` (and `nf_conntrack_acct` on gateways); `ovn_lport` on `attach_vm_port`; ownership on `br-int`; §11 network modes by scope. |
| security.md | §5.2 break-glass on servers only (D14); identity keys (D13); §7 `Host`/`Cluster` entities, `host.cluster`, node authorization (§12.3); §8.4 isolation on OVN and VPC routers (§11.2, §11.2a); §6.3 quotas `routers` and `external_ips`; CA on every server and rotation (D20, §12.5). |
| images.md | Per-node image caches; disk binding (§9.2). |
| metering.md | §5.4–5.5 on OVN networks: conntrack-based split (D22, §13.3), network totals, provider networks without totals; node-local accumulation and shipping (§13.1); flags `ext_gap`, `network_total_unavailable`. |
| installer.md | `--join [--import]`, `--rejoin`, `--leave [--keep-resources [--offline]]`, `--reset`, token from stdin or `--token-file`; roles and OVN packages; `cluster init` backup; refusing a plain join on a host with resources. |
| rest-api.md, cli.md, web-ui.md | `/cluster/*`, `/nodes`, `gxctl cluster init\|leave\|dissolve\|status\|snapshot\|join-token`, `gxctl node drain\|undrain\|remove\|forget\|purge\|rejoin-token\|detach\|promote\|import show\|approve\|reject`; a Cluster page with node phases and pending detaches and imports; `503 cluster_unavailable`, `409 node_departing`, `X-Glidex-Consistency`. |
| control-plane config | `cluster` section (§10.2); `pci.allow[].node`. |

## 17. Milestones

Each one is one PR or a short series, mergeable alone, with tests
passing in CI. C0 and C1 change nothing for standalone users.

| # | Scope | Acceptance |
|---|---|---|
| **C0** Store API | `Store`, `Tx`, write sets; all 40 `begin_write` sites moved; `Local` implementation; `subscribe` replaces the `Bell`; status written only on change (§4.1) | Every existing test passes; write-set replay test (§15); a `grep` lint in CI forbids `begin_write` outside `store/`; an idle VM causes no store write across ten resync rounds. |
| **C1** Node identity in the model | `nodes` table with one implicit node `local`; `status.placement`, `Disk.node`, `Network.scope`; `Host::"<node>"` + `Cluster`; identity re-keying code (D13); role split inside the process (node and server controller sets) | Schema migration test from the current schema; API and UI show the node; Cedar tests for host- vs cluster-scoped actions. |
| **C2** Raft | `openraft` with ReDB log and state machine; mTLS transport; `cluster init`, join tokens and CSR signing, learners and voters, forwarding, ReadIndex reads, snapshots, `cluster snapshot`, `--force-new-cluster`, feature level | §15 Raft and linearizability tests; three control planes on one machine (separate directories and ports) survive killing the leader with writes continuing. |
| **C3** Node role over the network | watch, node cache and outbox, status writes (§8.3), heartbeats and liveness, console/stats relay, image caches, ledger shipping; agent-only installs | Two hosts: a VM created on the server runs on the agent; agent restart adopts its VMs; with servers unreachable, the agent's VMs keep running and the outbox drains on reconnect; ledger totals equal the single-host run of the same workload. |
| **C4** Scheduler | filters and score (§9.1), sticky placement, `spec.node`, disk binding, drain | Unit tests over node fixtures; e2e: VMs fill two nodes by capacity; a VM with a node-bound disk lands on that node; `Unschedulable` explains why. |
| **C5** OVN foundation | `ovn-central`/`ovn-host` 26.03 install, OVN PKI, `ensure_ovn_chassis`, `glidex-ovn` crate with `ovn-nbctl` in daemon mode, cluster IPAM, isolated and NAT networks on `gx-edge` with an HA gateway, port security, DHCP and MTU, cluster `default` network | §15 OVN e2e tests 1–4. |
| **C6** Provider networks, VPC routers and metering on OVN | localnet with VLAN tags, physnet mapping onto netd bridges, scheduler physnet filter; VPC routers, external pool, `routers`/`external_ips` quotas (§11.2a); `snat-ct-zone`, `ct_external_counters`, network totals (§13.2–13.3), with the host checks of §13.3 done first | e2e test 5; §15 VPC router and external metering tests. |
| **C7** Operations and membership | node phases (§5.3), drain, remove, forget, purge, rejoin with D18; CA on every server and rotation (§12.5); rolling upgrades with feature-level gating, certificate renewal, `cluster status` port checks, Cluster page in the UI, runbook (recovery of both Raft groups) | §15 upgrade, membership and CA rotation tests; §15 scale run with figures recorded in §4.1; certificate renewal under load; documented recovery exercised on the test cluster. |
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
- **A shared image store**, which also deduplicates images imported
  with the same checksum (§5.9).
- **Floating IPs and port forwarding** on VPC routers (`dnat_and_snat`),
  overlapping CIDRs between VPCs (D21), and peering between VPCs.
- **Metering history outside the replicated store**, follower-served
  watches and the other steps past the §4.1 ceiling.

## 19. Resolved questions

| # | Question | Resolution |
|---|---|---|
| O1 | Where does the cluster CA key live? | On **every server**, as an encrypted credential, and **rotated whenever a server leaves** (D20, §5.2, §12.5). |
| O2 | Which OVN does Ubuntu 26.04 ship? | **26.03.0-2** in `resolute/main`, built for the archive's OVS 3.7.1; it documents every OVN feature used here. Minimum pinned to 26.03; no OVN source build (§10.1). |
| O3 | How to meter edge ingress and the external split on OVN? | **Conntrack accounting in each router's SNAT zone** on the gateway node (D22, §13.2–13.3). OVN has no counters on logical objects; IPFIX sampling and OpenFlow cookie statistics were rejected. Host checks listed in §13.3. |
| O4 | Own egress per network? | **VPC routers**: opt-in, per project, several networks each, own external IP and gateway group (D11, D21, §11.2a). |
| O5 | What scale does this architecture support? | **Designed for 50 hosts and 2,000 VMs; ceiling about 100 hosts and 5,000 VMs**, limited by the replicated metering history and the single leader (§4.1). Requires status writes only on change and `ovn-nbctl` in daemon mode (C0, C5). |
| O6 | Merge imported images with matching checksums? | **No, kept separate** for now; deduplication comes with a shared image store (§5.9, §18). |
| O7 | What does a detach take? | **Only what its VMs reference** (§5.8.1), unchanged. |
| O8 | Detach freeze and import plan expiry? | **Defaults kept** (10 min, 24 h), now **configurable**: `cluster.detach_freeze_secs`, `cluster.import_plan_ttl_secs`, with per-operation overrides (§10.2). |

Items still marked **(verify)** are host checks inside their milestones:
mapping on-demand snapshots onto openraft (§6.2, C2); OVN CA-file reload
(§12.5, C7); `snat-ct-zone`, conntrack events and zone allocation
(§13.3, C6); Cloud Hypervisor vhost-user migration (§18).

## 20. Implementation notes

What shipped, and where the code departs from the plan above. Each
milestone adds its notes here.

### C0 Store API

`store::Db` with `Db::begin`/`Db::write` (a `Tx` records every put/delete
as a `WriteSet`). Tables are the closed `TableId` enum (append-only).
`GLIDEX_CHECK_WRITE_SETS=1 cargo test` replays every database a test opens
and compares dumps (§6.1 invariant); `scripts/lint-store-writes.sh` forbids
`begin_write` outside `store/`. The change bell now belongs to the `Db`.

### C1 Node identity

Schema 3 (`nodes` table, `status.placement`, `Disk.node`,
`Network.scope`/`node`). Cedar has `Cluster` and per-node `Host` entities;
system roles link to the cluster, host-specific actions take a `Host`.
`authz::set_identity` holds this process's cluster and node ids (a
process-wide value: several control planes in one process share it, which
only tests do).

### C2 Raft

- `openraft` 0.9.25 with `generic-snapshot-data`. Log in `raft/log.redb`,
  state machine = `glidex.db` (`raft_meta` holds `last_applied` and
  `last_membership`, applied in the entry's own transaction).
- **Snapshots** are not periodic copies. A snapshot is a ReDB read
  transaction held from the moment it is asked for; it is streamed to a file
  and over HTTP only when a follower needs it (§6.2 "(verify)": resolved).
  `ClusterNode::seal_baseline` purges the log at init so a new member always
  starts from a snapshot of the database (state that predates the log).
- **mTLS** (`cluster::net`): HTTP/2 over rustls, trust = the cluster CA only;
  a joining host pins the CA by the hash in its token. `issued_certs` is
  enforced once a node has data (a node that has never synced has an empty
  registry and must accept the leader's first messages).
- **Writes on followers** are forwarded whole (`/fwd/…`, principal in
  `X-Glidex-Principal`); reads wait for a batched ReadIndex
  (`api::gate`). Authentication writes done by every server (sessions, JIT
  users, audit) go to the leader as raw writes or named operations
  (`/cluster/v1/raw`, `/cluster/v1/auth`, restricted to the auth tables).
- The cluster identity lives in `<state dir>/cluster/` (`identity.json`,
  `node.crt`, `node.key`, `ca.crt`, `ca.key`), not in
  `/etc/glidex/control-plane.json`: the unprivileged control plane can't
  write `/etc`. The CA key is a 0600 file there (or the
  `cluster-ca-key` systemd credential when the installer provides one);
  `systemd-creds encrypt` needs root and moves to the installer in C7.
- Voters change in pairs: a join adds a learner, and two learners are
  promoted together when the voter count stays odd (`gxctl cluster promote`
  for the rest).
- `gxctl cluster init|join|join-token|status|promote|snapshot`;
  `glidex-control-plane --force-new-cluster [--from <snapshot>]`.
- Not yet: feature-level gating (the level is written as 1), certificate
  renewal and rotation (C7), rejoin (C7), agents running the node role (C3).

### C3 Node role over the network

- **Cache and watch** (`cluster::agent`, `cluster::sync`). An agent's
  database is a *mirror* (`Db::set_mirror`): `GET /cluster/v1/list` fills it,
  `GET /cluster/v1/watch?from=` (long poll, 20 s) brings the changes. A server
  keeps the last 20,000 applied write sets in memory; older → `410`, re-list.
  A node receives shared catalogs (projects, images, networks, nodes, image
  caches), and only *its own* VMs, disks, events and credentials
  (`sync::filter_for`).
- **Status writes** go to the leader as write sets
  (`POST /cluster/v1/status-write`), checked inside the write
  (`sync::check_status_write`): a VM's `status` only (resource_version +1; D12's
  `spec.power = stopped` with a generation bump); a disk's and network's status;
  events of its own objects; its image caches; its own node record. Anything
  else is refused with `409`, and the node lists again.
- **Outbox**: when no server answers, the write set is stored in the local
  `raft_meta` table, applied to the cache at once (the VM keeps being
  reconciled), and sent in order when a server is back; the stream never
  overwrites a key with a queued write. A refused entry is dropped and the
  cache re-listed.
- **Liveness** (`cluster::lifecycle`): heartbeats every 5 s to the leader's
  memory; a node silent for `node_grace_secs` becomes `Ready=Unknown`
  (reason `NodeUnreachable`) and so do its VMs (condition and event);
  returning writes `Ready=True`. A new leader gives every node a fresh grace.
- **Placement** until the scheduler (C4): the node that handled the create,
  or `node` in the create request (id or name).
- **Controllers** act only on what is placed or bound on their node
  (`VmManager::is_local`); `sync_vms`, adoption and metering likewise.
- **Relays** (§8.4): `console/log`, `stats` and the console WebSocket of a VM
  on another node go server → node (`/cluster/v1/vms/{id}/…`; the console
  is an HTTP/1.1 upgrade carrying raw bytes). Authorization is the first
  server's.
- **Image caches** (`controller::image_cache`): `image_caches/<image>/<node>`.
  A node that needs an image (a disk bound to it, or a VM's firmware) copies
  the file from a node holding a ready copy and checks it against the record's
  `sha256`. The plan has each node download from the source; copying from a
  holder keeps a single pull. Deleting an image waits for every node to drop
  its copy.
- **Ledger shipping** (§13.1): with a cluster, the meter samples into a
  node-local ledger (`meter.db`) and every minute ships its closed hourly
  rows and slot rows (`POST /cluster/v1/ledger`). The leader merges them
  once (`ledger_inbox`, next free sequence number per subject and hour); the
  cluster ledger is complete only as far as the slowest live node has closed.
  A host that metered before it joined keeps its cursors (moved into the local
  ledger).
- **Agent installs**: `glidex-install --join SERVER:8842 [--role agent|server]
  [--token-file F] [--advertise A] [--node-name N]`; the token is read from a
  file (0600) or stdin. An agent runs the node role and the node API
  (:8842) only: no REST API or UI of its own.
- Not yet: the leader-side scheduler (C4), `spec.node` as a field of the VM
  spec (C4), D14's break-glass rule on agents (they serve no API).

### C4 Scheduler

- `scheduler::schedule` is a pure function over node records and loads
  (§9.1 filters: pin, phase and drain, `Ready`, CPU and memory with
  `cluster.scheduler` overcommit, hugepages, hypervisor, VFIO device present on
  the node, disks bound to one node, node networks on one node; score: least
  allocated by `max(cpu, memory)`, fewest VMs, name). Unit tests over node
  fixtures.
- It runs at admission (`create_vm_with`, under the VM write lock) and, for a
  VM that no node fits, the VM is created *without* a placement and
  `Scheduled=False/Unschedulable` carries each node's reason; the leader's
  placement loop (3 s) retries, so draining, capacity and node changes unstick
  it. A VM without a placement is nobody's in a cluster. A standalone host is
  its own node and needs no scheduling.
- `spec.node` (an id; the create request takes an id or a name) is the pin.
  Disks bind at creation (`node` in `POST /disks`) or to the node of their
  first VM; a VM and its disks always share a node.
- Every node reports capacity, hypervisors, PCI devices and versions to its own
  `nodes` record at start and every ten minutes (a status write; a follower
  server's goes to the leader through the raw-write endpoint, limited to its
  own record).
- `gxctl node drain|undrain <node>` (`POST /nodes/{id}/drain|undrain`, action
  `node.drain` under `role.system-admin`) lists what is still on the node.
- Not yet: `pci.allow[].node` (VFIO grants per node), the `br_int_datapath`
  filter (C5) and provider-network physnet filter (C6).

### C5 OVN foundation

Written without an OVN host to run on: what is tested is every command line
(recorded `ovn-nbctl`, `ovs-vsctl` and `systemctl` calls), the planning and the
IPAM. §15's OVN end-to-end tests 1–4 still have to be run on three hosts, and
`glidex-ovn/tests/real_nb.rs` (ignored) checks the northbound syntax against a
real database. Everything marked "(verify)" below is syntax or behaviour of OVN
26.03 taken from its manual pages.

- **`glidex-ovn`**: `Nb` runs `ovn-nbctl` (directly with `--db`/`-p/-c/-C`, or
  through a `--detach` daemon via `OVN_NB_DAEMON`); `sync(Desired)` makes the
  database say it and touches only rows with `external_ids:glidex-owner`. Per
  network a switch `gx-<name>`, DHCP options (router, DNS, MTU, server id), and
  for NAT a router port with the gateway address, a router-type switch port
  and an SNAT rule on `gx-edge`. The edge is a provider switch (`localnet`), a
  router with a gateway port and default route, an HA chassis group in the
  configured order, and the two isolation policies of §11.2 over the
  `gx_nodes` and `gx_nat_supernet` address sets. A VM port gets addresses and
  **port security** (its MAC and reserved address only), `requested-chassis`
  and its network's DHCP options. Ports and networks that left the plan are
  deleted.
- **IPAM** (`ipam`, tables `ipam_subnets`, `ipam_reservations`): a /24 from
  `ovn.nat_supernet` (default 10.89.0.0/16, never overlapping the node NAT
  range), or the requested subnet; reservations are made in the same write as
  the VM's placement (create, or the scheduler's) and live as long as the NIC
  (garbage-collected by the leader: the plan's `vm.ipam` finalizer).
- **Networks** have `scope: cluster` (default when `cluster.ovn.enabled`):
  `bridge: br-int`, MTU = underlay − 58, no per-node controller. NAT needs
  `cluster.ovn.edge`. On a fresh cluster with an edge, the default network is
  the cluster NAT network `default`, else `cluster-default` (§11.7).
- **netd** gains `ensure_ovn_chassis`, `ovn_status`, `leave_ovn`,
  `ensure_ovn_central`, and `ovn_lport` on `attach_vm_port` (`br-int` takes
  only OVN ports; an OVN port only `br-int`). `ensure_chassis` sets
  `Open_vSwitch` external ids, `set-ssl`, `br-int` (datapath as asked, refused
  to flip with VM ports on it), and enables `ovn-controller` (restarted only
  when its certificates changed). `ensure_central` writes `OVN_CTL_OPTS` in
  `/etc/default/ovn-central` for the Raft groups: the oldest server creates
  the clusters, the others join it; it restarts the service only on change.
- **Controllers**: every node ensures its chassis (and a server its
  `ovn-central`) every 30 s; the leader's network controller syncs the
  northbound database every 5 s. The VM controller waits up to 10 s for
  `ovn-installed` and launches anyway with `NetworkReady=False/PortNotInstalled`.
  Nodes record `br_int_datapath`; the scheduler places vhost-user NICs on
  cluster networks only where it is `netdev`.
- **Certificates**: a node's own certificate is its OVN client certificate,
  for `ovn-controller`, `ovn-northd`, the databases and `ovn-nbctl`. (The plan
  has separate chassis and database certificates; OVN checks only the CA.)
- **Installer** (superseded: OVN is now installed by default, see the validation notes below): `--ovn`, or `--join`, installs `ovn-host`, and `ovn-central`
  on servers.
- Not yet: the NB daemon supervised by the control plane (commands connect
  per call), OVN-aware `move_vm_port` (C8), provider networks and VPC routers
  (C6).

### C6: provider networks, VPC routers and metering on OVN

Built:

- **Provider networks:** `mode: bridged` + `physnet` (+ optional `vlan`), no IPAM. The scheduler places VMs only on nodes whose `ovn.bridge_mappings` has the physnet (`features.physnets`).
- **VPC routers:** tables `routers` (36) and `ipam_external` (37); API `GET|POST /projects/{id}/routers`, `GET|DELETE /projects/{id}/routers/{name}` with Cedar actions `createRouter`/`deleteRouter` (`network.manage`) and `readRouter` (`project.read`). Explicit `external_ip`/`gateway_nodes` need `host.network` (checked as `createNetwork` on `Host`). A router's id is 8 hex characters; `gxr-<id>` is its OVN router and a network's `router` holds the id (the request names it). Quotas `routers` and `external_ips` (default 1 each) are checked in the admission write; the pool, the zone and the record are written in one transaction, so a full pool (`409 external_pool_exhausted`) leaves nothing behind. Zones: the edge keeps the first of `ovn.snat_ct_zones`, routers take the lowest free one after it. Deleting a router is refused while any network, including one still being deleted, names it.
- **glidex-ovn:** one `ensure_gateway` serves the shared edge and every VPC router (provider-side port, HA chassis group, `gx_nodes` and supernet drop policies, SNAT, `snat-ct-zone`); the supernet policy of a VPC router exempts its own `gx_r_<id>_nets`. Routers that left the plan are removed with their provider-side ports.
- **External metering (D22):** `glidex_ovs::ct_meter` keeps cumulative per-(zone, address) counters from a conntrack dump per round plus `DESTROY` events, with per-connection baselines, L2 normalization, persistence before reporting, an `epoch` and a count of `gaps`. netd op `ct_external_counters` (read-only, full socket) turns `nf_conntrack_acct` on at first use. Metering turns them into `net.ext_*` per NIC and `bridge.ext_*` per network, adds the inbound bytes to the network's `bridge.bytes`, and flags `ext_gap`. Provider networks get no network total and the flag `network_total_unavailable`.

Not exercised against real OVN or a real kernel conntrack: command lines, parsing, planning, accounting arithmetic and the metering rules are unit-tested; §13.3's "to check on a real host" list is still open.

Deviations and limits:

- Deleting a router is immediate: its record and address go at once and its OVN objects at the next network-controller pass (not a `deletion_requested_at` round trip). The router status has no `active_gateway` yet (it needs the southbound database).
- A VM's external counters start at the first round that sees them (`Origin::Unknown`), not at launch, because the collector's totals are keyed by address and an address can pass between VMs. At most one round's external traffic of a new VM is lost.
- netd's zone range comes from its own file (`ct_zones`, default 60000–64999) and must match `ovn.snat_ct_zones`; the installer is meant to write both.
- The `DESTROY` reader and the `conntrack` dump use the `conntrack` CLI, not netlink in-process.
- Gateway nodes with `br_int_datapath = system` are not yet enforced when a router is created.

### C7: operations and membership

Built (tests in `cluster_tests.rs`, `ovn.rs`, `ct_meter`):

- **Membership** (`cluster/membership.rs`, API `POST /nodes/{id}/{remove,forget,purge,rejoin-token}`, Cedar `removeNode`, `forgetNode`, `purgeNode`, `createRejoinToken` in `host.cluster.critical`): remove refuses a node that holds VMs, disks or node networks, is the last gateway of a router or the edge, or would break the Raft rules of §5.5 (majority of the old group; odd unless `--force`). One write retires the node and deny-lists its certificates; the Raft side is finished by a leader loop that removes any tombstoned server still in the group, so a leader that dies half way is replaced by one that completes it. Forget needs `--fenced`, a node that has stopped reporting, and marks its VMs `Ready=False/Lost`. Purge deletes a forgotten node's VM, disk and node-network records and makes it `Removed`.
- **Rejoin** (`POST /cluster/v1/rejoin`, `gxctl cluster rejoin`, `glidex-install --rejoin`): single-use token bound to a node id; a new certificate for the same id; with `--raft-lost` (D18) the leader removes the old voter, the host wipes its Raft state and returns as a learner through the join path.
- **OVN membership:** netd op `forget_ovn_member` kicks a departed server from the NB and SB clusters (`cluster/kick`, by address) and deletes its chassis. The edge's gateway list drops tombstoned nodes by itself because the plan filters them.
- **CA rotation and renewal** (`cluster/rotation.rs`, `gxctl cluster rotate-ca`): `ca_bundle` holds the trust bundle, the signing CA and the CAs being retired. Servers receive the new key by `PUT /cluster/v1/ca` (with its certificate), every node installs the replicated bundle, renews by CSR (`POST /cluster/v1/renew`) when its issuer is not the signing CA or it expires within 30 days, and the leader retires old CAs when every node has renewed or the grace has passed. A server leaving triggers a rotation; so does a CA within a year of expiry. Tokens pin the signing CA.
- **Feature level:** each server reports `features.feature_level`; the leader raises `meta.feature_level` to the minimum across servers (never down). Nothing uses a level above 1 yet.
- **`cluster status`:** versions, feature level, CA rotation, and `?ports=true` TCP reachability from the serving server to every other node's 8842 (and 6641-6644 with OVN).
- **UI:** a Cluster page (nodes, drain/undrain, Raft, CA, port gaps). **CLI:** `gxctl node remove|forget|purge|rejoin-token`, `gxctl cluster rejoin|leave|rotate-ca`. **Installer:** `--rejoin`, `--node-id`, `--leave`. **Runbook:** `spec/cluster-runbook.md`.

Deviations and not done:

- A server cannot remove itself: run the removal on another server (openraft 0.9 has no leadership transfer, and the request would die with the leader).
- OVN CA files and netd certificates are not rotated by this step: a node's own certificate still doubles as its OVN certificate (C5), and netd is not told when the bundle changes. OVN's window therefore stays open until a node re-runs `ensure_ovn_chassis`.
- `cluster/leave` by the departing server itself is not used; the leader kicks it.
- Not done: the scale run and figures for §4.1, `pci.allow[].node`, `glidex-install --reset`, `--seal-ca-key`, certificate renewal under load, a rolling-upgrade test with two builds, and the recovery exercise on a test cluster (the runbook describes it; `--force-new-cluster` is tested, the OVN half is not).

### C8: detach and import

Built (tests in `cluster_tests.rs`, `netd_tests.rs`):

- **Detach online** (`cluster/departure.rs`, `POST /nodes/{id}/detach`): freeze (one write: node `Departing`, plan in `meta` as `departure/<id>`), the node fetches `GET /cluster/v1/departure/<plan>`, writes `glidex.db.standalone-pending` (0600, fsynced) and acknowledges with the bundle's SHA-256, the leader rebuilds the bundle, refuses if it changed, and commits in one write (objects removed, reservations freed, node `Departed` with `departed_ids`, certificates deny-listed, receipt signed by the cluster CA). The node verifies the receipt, and at the next start `finish_pending` swaps the databases and removes its cluster identity, certificates, CA key and Raft log. `--abort` and the `cluster.detach_freeze_secs` deadline return the node to `Active` and discard the pending database. Cluster networks are mapped (`--map-network a=b`, the NIC's attachment is renamed) or let go (attachment removed, `RestartRequired=True/NetworkRemoved`).
- **Detach offline** (`glidex-control-plane --detach-offline`, `glidex-install --leave --keep-resources --offline`): the bundle comes from the node's cache, the private key is deleted, the databases are swapped. **`forget --departed`** (`POST /nodes/{id}/detach {"departed": true}`) removes the node's objects, marks it `Departed` and rotates the CA if it was a server.
- **Import** (`cluster/import.rs`, `gxctl cluster join --import`, `gxctl node import list|show|approve|reject`, `glidex-install --join … --import`): the host uploads its records as a bundle with its CSR (token needs `--allow-import`, agents only); the leader makes a plan, stages the rows in `meta` chunks of at most 1 MiB, checks ids (a clash is fine only for ids in a `Departed` node's `departed_ids`), project names, network names, credential names and quotas, and waits. Approval takes the mappings (`--project src=existing|src=new:name`, `--network a=b`, `--credential project/user=new`, `--over-quota`, `--dry-run`), validates again, then one write creates the node, signs its certificate and makes the rows live (projects that exist are merged into). The host polls with a secret, keeps its standalone records as `glidex.db.pre-join-<ts>` (a snapshot), and starts the node role. Rejecting or the `cluster.import_plan_ttl_secs` expiry discards the plan and its rows.
- **`move_vm_port`** (netd op, `vm_port::unplug`): moves a port between `br-int` and a glidex bridge without recreating the tap.
- **`cluster dissolve`** (`gxctl cluster dissolve --force`): the last node writes a final snapshot (`glidex.db.final-snapshot`, 0600), builds its own bundle with access data, signs a receipt with its CA key and finishes at the next start.
- Round trip tested: an agent with a VM detaches from cluster A, the host opens as a standalone host with the VM, and imports into cluster B after an approval that has to map a clashing project name.

Deviations and not done:

- A server can't detach itself online (as for removal). Spec writes to a `Departing` node's objects are not refused with `409 node_departing`; instead the commit refuses if the node's objects changed after the bundle was taken (the SHA-256 differs), and the administrator detaches again.
- `--with-access` on detach copies the project's role links and the users, teams and identities they name verbatim; identity keys scoped to the departing node (D13) are not re-keyed. Import refuses nothing about access data but does not take it: only resources are imported.
- `move_vm_port` is not called by the switch yet: mapped NICs are renamed in the records and the guest keeps its device, but the port stays on `br-int` until the VM restarts. `leave_ovn` runs in `gxctl cluster leave` but not in the offline detach.
- Image records come in as their own records (as the spec says); disks keep backing files in place.
- Staged import rows are chunked at 1 MiB, but the commit is one Raft entry holding every live row: a very large import can exceed `max_payload`. Metering history and audit records are not imported (D19).
- The import host's NIC IP reservations for mapped networks are not made (no node-network to cluster-network mapping on import).

### Validation against real OVN and conntrack (after C8)

Run on Ubuntu 26.04 with OVN 26.03.0 and conntrack 1.4.9, with no OVN or kernel state touched outside throwaway sandboxes:

- `glidex-ovn` `sync` against a real northbound database, with a real `ovn-northd` compiling it (`crates/glidex-ovn/tests/real_nb.rs`, ignored; `GLIDEX_TEST_NB`, `GLIDEX_TEST_SB`, `GLIDEX_TEST_NORTHD_LOG`). It covers isolated, NAT, VPC-routed and provider networks, VM ports, the edge, VPC routers, idempotence, removal and northd's logical flows. Bugs it found:
  - `ha-chassis-group-del` has no `--if-exists`, so groups are now removed with `destroy`.
  - A gateway port still refers to its group, so the reference is cleared first, and a router is deleted before its group, in separate transactions.
  - `dhcp-options-create` takes bare `key=value` pairs, so every sync used to make a new, unmatched DHCP row.
  - Router policies were checked across all routers, so a VPC router never got its `gx_nodes` drop.
- The OVN databases with glidex's `ovn-ctl` options and cluster certificates (`crates/glidex-control-plane/tests/real_ovn.rs`, ignored; it starts its own servers on 127.0.0.1 and 127.0.0.2). It checks SSL with cluster certificates, refusal of certificates from another CA, two servers forming the NB/SB Raft clusters over SSL, `ovn-northd` making flows, and SB RBAC. Bugs it found:
  - Raft ran over plain TCP: `--db-*-cluster-local/remote-proto=ssl` were missing.
  - No client listener existed at all: `ovn-ctl` only makes the Raft ones. `ensure_central` now sets the `Connection` rows: NB `pssl:6641`; SB `role=ovn-controller` on `pssl:6642` and no role on `pssl:6648`. These commands use `--no-leader-only`.
  - `ovn-northd` requires the cluster leader, so it can't use the local socket of a follower. It now talks to every server: NB on 6641 and SB on 6648.
  - SB RBAC requires the chassis name to equal the certificate's CN, so a node's chassis is now `node:<id>` (`chassis_name`) everywhere: the chassis itself, `requested-chassis`, HA chassis groups and `chassis-del`.
  - The databases listen on the server's advertised address, not on whichever server address matched its tunnel IP.
- Packaging: `ovn-controller.service` is static on Debian/Ubuntu, so `enable --now` of it did nothing across reboots. The chassis now enables `ovn-host`, which wants the controller, and leaving disables `ovn-host`.
- Conntrack: real `conntrack -L` and `-E` output from a gateway namespace that SNATs a VM in zone 60001 is now a unit test (`real_kernel_output_is_read_as_the_vms_traffic`). `conntrack -E` writing to a pipe is block-buffered, so the event reader runs it under `stdbuf -oL`.
- Installer: OVN (`ovn-host`, `ovn-central` unless joining as an agent, and `conntrack`) is installed by default with networking, `--no-ovn` skips it, and the choice is saved. When the installer itself installed OVN on a host in no cluster, OVN's services are disabled and stopped, since netd starts what a cluster needs. A real run on this host installed the packages, left every OVN unit inactive and disabled, and restarted the glidex services on the new build.
- Not exercised for real: `ensure_ovn_chassis` on a host's own OVS and `br-int`, an `ovn-controller` binding a VM port, and Geneve between two hosts. They need a second host or changes to this host's live OVS.

Review fixes (C7/C8):

- Departure:
  - The node can fetch its receipt after the commit revoked its certificate, if the issued certificate's plan committed.
  - The pending database is rewritten whenever the bundle changes, and its hash is stored beside it.
  - `finish_pending` checks the receipt's node and bundle hash against that file.
  - The commit re-checks the plan and rebuilds the bundle inside its own write.
  - An abort can't overwrite a commit.
  - A departed server is taken out of OVN and rotates the CA.
- Import:
  - Only resource tables are accepted: VMs, their events, disks, node networks, images, credentials and projects.
  - Approval validates and commits in one write, and so does rejection.
  - A host that stops waiting withdraws its plan, and switches if the plan committed meanwhile.
  - A runtime that fails to start after the commit keeps its identity.
  - The approver needs `createVm`, `createDisk` and `createCredential` on each project merged into, `createProject` when projects are created, and `exceedQuota` with `--over-quota`.
- Rejoin:
  - The token is checked before it is used up.
  - The node's old certificates are deny-listed.
  - A server outside the Raft group comes back through `join/ready` even with intact Raft state, which also gives it the CA key.
- CA rotation:
  - Servers stage a new key and switch only when the replicated state names it.
  - The leader rotates whenever a server was retired after the current CA started signing, retrying a minute apart, and a new leader resumes.
  - Rotation and retirement take one lock, and retirement re-checks the state inside its write.
- Replay check: `install_dump` stops journaling, because an installed snapshot can't be rebuilt from the database's own write sets.

