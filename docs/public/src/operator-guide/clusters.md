# Clusters

Several Glidex hosts can run as one cluster: one API, one set of
projects, users and images, and a scheduler that places VMs on the
nodes. This page covers forming a cluster, adding and removing nodes,
recovery, upgrades, and turning on cluster networks (OVN).

Cluster operations need the `system-admin` role. On the local socket,
members of `glidex-admin` have it. Most of them (init, join tokens,
removals, snapshots, CA rotation, OVN settings) also need a recent
login: over TCP or in the web UI, you may be asked to log in again.

## Roles and quorum

Every node runs the same software and runs VMs. A node is one of:

- **Server**: keeps a copy of the cluster's database, replicated with
  Raft, and serves the API. Servers sign node certificates and run OVN's
  databases when OVN is on.
- **Agent**: only runs VMs. It has no API of its own: `gxctl` on an
  agent says so and points you to a server. Its web UI still works; it
  relays every request to a server, which authenticates it, so you log
  in there with the servers' users.

The servers that vote in Raft are *voters*. A cluster keeps working
while a majority of voters is up, so run 1, 3 or 5 servers: 3 survive
one lost server, 5 survive two. Glidex refuses changes that leave an
even number of voters or no majority, unless you `--force` the even
case.

Nodes talk to each other on TCP 8842 (the node API), with mutual TLS
under the cluster's own certificate authority. The installer doesn't
open firewall ports; see [the port list](host-networking.md#ports-between-nodes).

## Before you start

Install Glidex on every host as in [Installation](installation.md). OVN
is installed too unless you pass `--no-ovn`; its services stay off until
the host is in a cluster. Give each host a stable address that the
others can reach.

## Create a cluster

On the first host, which becomes the first server:

```bash
gxctl cluster init --force
```

`--force` is required: init re-keys every record for clustering and
can't be undone from Glidex. It keeps a copy of the database first
(`glidex.db.pre-cluster-<time>` in the data directory) and prints the
cluster id and the CA's SHA-256 fingerprint. VMs keep running.

The node advertises its first non-loopback address on port 8842. To
choose, pass `--advertise 192.0.2.11:8842`; `--tunnel-ip` sets the
address for Geneve tunnels (default: the advertised address).

In the web UI: **Cluster** → **Make this host a cluster** →
**Initialize cluster…**, with the same two fields.

Check it:

```bash
gxctl cluster status
gxctl nodes
```

## Join tokens

A host joins with a one-time token issued by a server. The token is a
secret: it is shown once, works once, and expires (1 hour by default,
up to 7 days). Write it straight to a private file:

```bash
umask 077
gxctl cluster join-token --role agent > join.token           # an agent
gxctl cluster join-token --role server --ttl 24h > join.token
gxctl cluster join-token --role agent --allow-import > join.token
```

`--role` is `agent` (default) or `server`, and the joining host must ask
for the same role. `--allow-import` lets the host bring its existing VMs
along (see [Import](#import-an-existing-hosts-vms)). The command prints
the servers to join through on standard error.

In the web UI: **Cluster** → **+ Add node**, pick the **Role**, how long
the **Token valid for**, and whether the host has VMs to bring along.
The dialog shows the token once with the commands to run.

Copy the token to the new host over a secure channel (for example
`scp`), keep it mode 0600, and delete it after use. The token also pins
the cluster's CA: the joining host refuses a server whose CA doesn't
match it.

## Join a host

On the new host, from your Glidex checkout, with the token file:

```bash
cargo run -p glidex-install -- --join 192.0.2.11:8842 --role agent --token-file join.token
```

The installer installs or updates Glidex as usual, then joins. The
token is read from the file (which must not be readable by others) or
from standard input, never from the command line. Options:

- `--role server` for a server (the token must be a server token).
- `--advertise IP:PORT`: the address other nodes use for this one.
- `--node-name NAME`: default is the host name.

A host that already runs Glidex can join with `gxctl` instead:

```bash
gxctl cluster join --server 192.0.2.11:8842 --role agent --token-file join.token
```

`gxctl cluster join` defaults to `--role server`, so pass the role the
token was made for.

A host that has VMs, disks, networks or credentials can't do a plain
join: that would abandon them. Remove them, or join with `--import`.

## Add servers

A new server joins as a Raft *learner*, then becomes a voter. To keep
the voter count odd, learners are promoted in pairs: join two servers
to go from 1 to 3 voters, two more to reach 5. A single extra server
stays a learner until the next one joins, or until you promote it:

```bash
gxctl cluster promote node2 node3        # learners to voters; --force for an even count
```

`gxctl cluster status` lists voters and learners; the **Raft** card on
the Cluster page shows the same.

An agent can't become a server in place. To turn an agent into a
server, take it out of the cluster and join it again with a server
token.

## Import an existing host's VMs

A standalone host can join with its VMs, disks, networks and
credentials:

1. On a server: `gxctl cluster join-token --role agent --allow-import > join.token`
   (or tick **The host already has VMs to bring along** in **+ Add node**).
2. On the host:
   `cargo run -p glidex-install -- --join 192.0.2.11:8842 --import --token-file join.token`.
   Nothing changes yet: the host sends a plan and waits.
3. An administrator reviews and approves the plan. On the Cluster page,
   **Pending imports** lists it; its page shows what the host brings,
   lets you map its projects, node networks and credentials to names in
   the cluster, and reports problems. **Check**, then **Approve** (or
   **Reject**). From the CLI:

```bash
gxctl node import list
gxctl node import show <plan>
gxctl node import approve <plan> --project lab=new:lab-imported --dry-run
gxctl node import approve <plan> --project lab=new:lab-imported --network default=lab-net
gxctl node import reject <plan>
```

`--project src=existing` puts a project's resources into an existing
project, `src=new:name` into a new one. `--network a=b` and
`--credential project/user=new` rename networks and credentials whose
name is taken. `--over-quota` accepts an import that takes a project
over its quota. A plan expires after a day.

On approval the host joins and its resources become the cluster's in
one step. Its VMs keep running.

## Placement

The scheduler picks a node when a VM is created. A node is a candidate
only if it is `Active`, ready, not draining, has room, and has what the
VM needs: the hypervisor, the image's architecture, hugepages if asked,
its PCI devices, the physical networks of its provider networks, a
userspace `br-int` for vhost-user NICs on cluster networks, an OVN
version as new as the cluster's, the node of any node-local disk or node
network it uses, and every datastore its disks are on
([Datastores](datastores.md)). Among candidates it picks the least allocated
(the largest of the CPU, memory and, for a VM in hugepages, hugepage
shares after placing), then the one with fewest VMs.

A node's hugepages are its own resource: the pool the kernel reserved
(`vm.nr_hugepages`, and per size under `/sys/kernel/mm/hugepages`), less
what OVS-DPDK takes for `dpdk-socket-mem`. They don't count as ordinary
memory, and a node's allocatable memory leaves them out. VMs placed on
the node hold theirs whether running or not. Before a VM in hugepages
starts, its node checks that enough of them are free; if not, the start
fails with the counts. Hugepages used outside Glidex (another process,
or DPDK beyond `dpdk-socket-mem`) show up that way.

Room is each node's CPUs and memory minus a reserve, times an
overcommit ratio. The defaults, in `/etc/glidex/control-plane.json`:

```json
{
  "cluster": {
    "scheduler": { "cpu_overcommit": 4.0, "memory_overcommit": 1.0 },
    "node_reserved": { "cpus": 1, "memory_mib": 2048 }
  }
}
```

The VM form's **Placement** shows the scheduler's choice
(**Automatic (now: …)**) and each node's free vCPUs and memory, or why
it can't take the VM. Pick a node to pin the VM there (`node` in the
API). A VM no node can take is still created and waits, with each
node's reason in its status.

A VM stays on its node once placed. Glidex doesn't move running VMs.
A stopped VM whose disks are on shared datastores can be placed on
another node: `gxctl place <vm> [--node N]`, or **Place on another
node** on its page.

## Node lifecycle

Each node has a phase: `Active`, `Draining`, `Departing` (a detach in
progress), or one of the end states `Departed`, `Removed`, `Forgotten`.
The Cluster page's **Nodes** card shows phase and readiness; a node's
page has an **Operations…** menu for everything below. The `gxctl node`
commands take a node's name or its id.

### A node is down for a while

Nothing to do. Every node, servers included, reports to the leader
every 5 seconds. A node that stops reporting for 40 seconds
(`cluster.node_grace_secs`) becomes not ready, and so do its VMs
(`NodeUnreachable`). Nothing is restarted elsewhere. The node comes back
by itself with its identity.

### Drain

```bash
gxctl node drain node3
gxctl node undrain node3
```

A draining node gets no new VMs. VMs already there keep running; the
command prints what is still on the node. Stop, delete or move them
before removing it.

### Remove an empty node

1. `gxctl node drain node3`, then delete its VMs, disks and node
   networks (or [detach](#detach-a-node-with-its-vms) it with them).
2. `gxctl node remove node3`. Refused while the node holds a VM, disk
   or node network, if it is the only gateway of the shared edge or of a
   project router, or if the Raft voters left would have no majority or
   an even count (`--force` for the even case only).
3. On that host, from your checkout:
   `cargo run -p glidex-install -- --leave`. It refuses until the removal
   has committed, then makes the host standalone again. (On a removed
   server whose control plane still runs, `gxctl cluster leave` does the
   same; an agent has no local API for it.)

Removing a server rotates the CA afterwards: it held the CA key.

### Detach a node with its VMs

```bash
gxctl node detach node3 --map-network lab=lab-standalone
gxctl node detach node3 --abort         # before the node acknowledged
```

The node leaves *with* its VMs, disks and node networks and becomes a
standalone host that owns them. `--with-access` also carries the
projects' users, teams and role bindings (never sessions or tokens).
The node is frozen while the handover runs. The Cluster page shows it
under **Detaches**, with the networks it maps or drops by the names
they had when it started, even if they are gone since.

If no server can be reached at all, a node can leave on its own:

```bash
cargo run -p glidex-install -- --leave --keep-resources --offline
```

It asks for confirmation: changes made in the cluster since the node
last synced are lost. Then, on a server, run
`gxctl node detach node3 --departed` so the cluster lets go of it.

### A node is gone for good

1. Make sure it is powered off or fenced. Then:
   `gxctl node forget node3 --fenced` (add `--force` if that leaves an
   even number of voters). Its VMs stay recorded, marked lost; its
   certificate is refused from now on; a forgotten server is rotated out
   of the CA. Lost VMs whose disks are on shared datastores can be
   placed on a live node: `gxctl place <vm>`.
2. When you no longer want its records:
   `gxctl node purge node3`. This deletes the records of its VMs, disks
   and networks, not any files.

### A repaired host comes back as itself

A forgotten node, or one whose certificate expired, can rejoin with the
same identity, disks and VMs:

```bash
umask 077
gxctl node rejoin-token node3 > rejoin.token
gxctl node rejoin-token node3 --raft-lost > rejoin.token   # a server whose Raft state is gone
```

Use `--raft-lost` for a server whose `cluster/raft` directory is gone or
older than its last vote: it then returns as a learner with fresh Raft
state. On the host:

```bash
cargo run -p glidex-install -- --rejoin 192.0.2.11:8842 --token-file rejoin.token
```

Add `--node-id <id>` if the host lost its cluster files. A removed or
departed node's id is retired; it joins as a new node instead.

### End the cluster

On the last node, after every other node has left:

```bash
gxctl cluster dissolve --force
```

The host becomes standalone with every VM, disk, network and user, and
keeps a final snapshot. Restart the control plane afterwards. The
Cluster page's **Danger zone** has **Leave** (for a removed node) and
**Dissolve**.

## Certificates and the CA

The cluster CA is valid for 10 years, node certificates for one. Nodes
renew their certificates by themselves 30 days before they expire. The
CA rotates by itself after a server leaves and a year before it
expires. To rotate on demand:

```bash
gxctl cluster rotate-ca                  # old CA retires after 7 days, or once every node renewed
gxctl cluster rotate-ca --grace 0        # retire at once
```

`--grace` is in seconds (default 604800, `cluster.ca_rotation_grace_secs`).
Every node trusts both CAs, renews its certificate under the new one,
and the old one retires. A node that was unreachable the whole time
needs a rejoin token. The **Certificate authority** card on the Cluster
page shows the signing CA, its expiry, a rotation in progress, and
nodes not yet renewed; **Rotate** starts one (grace in days there).

Join tokens pin the CA. A token issued before a rotation fails with
"the server's CA … is not the one the token names": issue a new one.

### The secrets key

Connector secrets are sealed with a key every server holds. Rotate it
with:

```bash
gxctl cluster rotate-secrets-key
```

If a server can't be reached, it stops before re-wrapping anything; run
it again.

## Rolling upgrades

Upgrade one node at a time: agents first, then servers. On each host,
update the checkout and re-run the installer (see
[Maintenance](maintenance.md#updating)). VMs keep running while the
control plane restarts. An agent stopped in the middle of reporting a
status sends that report once it is back.

`gxctl cluster status` shows each node's version and feature level. The
cluster runs at the lowest feature level any server reports, and uses
new formats only once every server has them. Keep servers within one
release of each other while you upgrade, and finish the round before
starting the next.

<!-- TODO(verify): the "servers at N, agents at N or N-1" rule is not enforced in code; confirm the supported version skew. -->

The installer also brings a node's OVN up to the version the cluster's
`ovn-northd` runs. A node whose OVN is older gets no VMs on cluster
networks; the Cluster page names it.

## Backups and lost quorum

On a server, take a snapshot of the replicated database regularly:

```bash
gxctl cluster snapshot glidex-$(date +%F).gxsnap
```

The file is written 0600: it holds credential and token hashes. Store it
like the database itself. The Cluster page's **Certificate authority**
card also offers **Download a cluster snapshot**.

If a majority of servers is destroyed, the cluster can't elect a
leader. Recover from one surviving server:

1. On that server, stop the control plane (VMs keep running):
   `sudo systemctl stop glidex-control-plane`.
2. Start a single-voter cluster from its own database, or from a
   snapshot. Copy the snapshot where the `glidex` user can read it
   (`sudo install -o glidex -g glidex -m 0600 glidex-….gxsnap
   /var/lib/glidex-control-plane/latest.gxsnap`), then:

   ```bash
   sudo -u glidex env HOME=/var/lib/glidex-control-plane \
     /usr/local/bin/glidex-control-plane --force-new-cluster --from /var/lib/glidex-control-plane/latest.gxsnap
   ```

   Leave out `--from` to use its local database. Then
   `sudo systemctl start glidex-control-plane`.
3. Re-join the other servers with `--rejoin` and tokens made with
   `gxctl node rejoin-token <node> --raft-lost`.
4. With OVN on, OVN's own databases need the same: on a surviving
   server, convert its northbound and southbound database files to
   standalone (`ovsdb-tool cluster-to-standalone`), then re-cluster the
   servers.

<!-- TODO(verify): step 4 (OVN NB/SB recovery) is not exercised by the test suite; confirm the exact procedure and file locations before publishing more detail. -->

## Cluster networks on OVN

With OVN off, every network lives on one node. With OVN on, *cluster
networks* span every node: VMs on different nodes share them over
Geneve tunnels, and NAT cluster networks leave through a shared edge.
Node networks and their VMs are not affected either way.

### Turn OVN on

Every node needs OVN installed (the default). If the Cluster page's
**OVN** card lists a node as missing it, re-run the installer there with
`--ovn`. Then **Enable OVN…** on that card. Every node follows within a
minute, without a restart: servers start OVN's databases, every node
becomes a chassis.

You can instead set it in a server's `/etc/glidex/control-plane.json`
(`"cluster": {"ovn": {"enabled": true}}`) and restart the control plane;
saving in the UI makes the setting the cluster's.

With OVN on, the **OVN** card shows tunnels between nodes, and nodes
whose OVN is out of date. **OVN settings…** holds the rest. **Disable
OVN…** is there too, once no cluster network is left.

### OVN settings

| Field | Meaning |
| --- | --- |
| DNS servers for guests | Resolvers handed to guests on cluster networks; default: the hosts'. |
| NAT supernet | Where NAT cluster networks get their subnets; default `10.89.0.0/16`. |
| Underlay MTU | The MTU of the network between nodes; default 1500. Cluster networks get 58 bytes less, for the Geneve header. |

### The shared edge

NAT cluster networks reach the outside through a logical router on
*gateway nodes*, with one outside address. Tick **Shared edge for NAT
cluster networks** in **OVN settings…** and fill in:

| Field | Example | Meaning |
| --- | --- | --- |
| Physical network | `uplink` | The LAN the edge sits on; each gateway node maps it to a bridge. |
| External network | `192.0.2.0/24` | That LAN's subnet. |
| Gateway | `192.0.2.1` | The LAN's router. |
| Edge address | `192.0.2.50` | The edge's own address on the LAN; NAT traffic leaves with it. |
| Pool for VPC routers (optional) | `192.0.2.64/26` | Outside addresses for project routers that get their own. |
| Gateway nodes, by priority | node1, node2 | Where the edge runs; the next takes over if one fails. |

The fields are prefilled from what the nodes report (their uplinks,
addresses and routes); **Suggest values** fills them again, with notes
on anything uncertain. Each gateway node must map the physical network
and have `br-int` on the kernel datapath; see
[Host networking](host-networking.md#clusters-ovn-on-the-host). Use
addresses on the LAN that nothing else uses.

The same settings can be given in a server's `control-plane.json`
under `cluster.ovn` (`edge` with `physnet`, `external_cidr`, `gateway`,
`external_ip`, `external_pool`, `gateway_nodes`); the OVN settings
dialog says when it is showing the file's values.

Projects can have their own routers, each with its own outside address
from the pool, and attach their NAT or isolated networks to them; see
[Networks](../user-guide/networks.md).

## The Cluster page

The web UI's **Cluster** page (in the side menu) shows, on a cluster:

- the cluster id and feature level, and **+ Add node**;
- warnings for ports that don't answer, a CA rotation under way, and
  detaches in progress;
- **Nodes**: role, phase, readiness, address, version and VM count;
  click a node for its page (capacity, versions, KVM / IOMMU / DPDK,
  `br-int` datapath, physical networks, its VMs and disks, and the
  **Operations…** menu);
- **Pending imports** and **Detaches**;
- **Raft**: state, term, leader, voters and learners;
- **OVN**: on/off, tunnels, settings;
- **Certificate authority**, and the **Danger zone**.

On a standalone host it offers **Make this host a cluster**.
