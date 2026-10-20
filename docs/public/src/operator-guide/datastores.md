# Datastores

A **datastore** is a place where disks live. Every disk is on exactly
one. Out of the box there is only `local`, and nothing changes until you
register another.

| Kind | Driver | What it is | Disks on it |
|---|---|---|---|
| `local` | built in | Each node's own disk directory under `/var/lib/glidex-control-plane/.glidex/disks/`. | Bound to one node; their VMs run there. |
| Node-local | `dir` | A directory under `/srv/glidex` that each node has its own of, such as a fast local SSD. | Bound to one node, like `local`. |
| Shared | `shared-dir` | One filesystem every node mounts at a directory under `/srv/glidex`: NFS, CephFS, or a storage vendor's. | Bound to no node. A VM can run on any node that reaches the datastore, and a stopped VM can [move to another node](#placing-a-stopped-or-lost-vm-again). |

Datastores belong to the cluster (or to the standalone host). On a
cluster, register one once, from any server, and every node checks it.

Registering, changing and deleting datastores needs the `system-admin`
role. Everyone can see them and choose one for their disks, unless a
[site policy](#who-may-use-a-datastore) forbids it.

## The built-in `local` datastore

- It is listed with every datastore, marked *built in*.
- It can't be changed or deleted (`409 conflict`).
- It is the default for new disks until you
  [make another the default](#the-default-for-new-disks).
- Its disks are where they always were. Nothing moves when you add
  datastores.

## Preparing the directory

Every registered datastore is a directory under `/srv/glidex`, for
example `/srv/glidex/fast`. The installer creates `/srv/glidex`
(owned by root, mode 0755) and leaves it in place on uninstall. Paths
elsewhere, or `/srv/glidex` itself, are refused.

Create the directory on each node that should have the datastore, owned
by the `glidex` user:

```bash
sudo install -d -o glidex -g glidex -m 0750 /srv/glidex/fast
```

Glidex creates what it needs inside (`disks/`, and on a shared
datastore also `images/` and `.locks/`). It never creates the directory
itself, and never mounts anything.

### Mounting shared storage

Mount the shared filesystem yourself, on every node, with your usual
tools (`/etc/fstab`, systemd mount units, the vendor's agent), at a
directory under `/srv/glidex`:

```bash
sudo mkdir -p /srv/glidex/shared1
sudo mount -t nfs4 nfs.example.org:/export/glidex /srv/glidex/shared1
sudo chown glidex:glidex /srv/glidex/shared1        # once, on one node
```

- The mount's top directory must be **empty** when you register it
  (`lost+found` is ignored), and writable by `glidex` on every node.
- Mount it before Glidex needs it at boot, and keep it mounted. A node
  where it isn't mounted only sees an empty directory; Glidex notices
  that (see [The marker](#the-marker)) and won't use it there.
- Files are owned by the `glidex` user. Give that user the same uid and
  gid on every node, or map them on the storage side, so every node can
  open every node's files.

<!-- TODO(verify): the installer creates the glidex user with `useradd --system`, so its uid can differ between nodes. Confirm what sites should do (pin the uid before installing, or idmap on the export) and whether the installer should pin it. -->

## Registering a datastore

**Web UI:** **Datastores** (in the side menu), **Add Datastore**:

| Field | Meaning |
|---|---|
| **Name** | 1-64 characters of letters, digits, `.`, `_` and `-`; unique, `local` included. |
| **Path** | The directory; defaults to `/srv/glidex/<name>`. |
| **Nodes** (cluster) | The nodes that have it. None ticked: every node, including nodes that join later. |
| **Paths of their own** (cluster) | One `node=path` per line, for nodes that mount it somewhere else. |
| **Shared** | One filesystem every node mounts. Leave it off for a node-local datastore. |
| **New disks go here unless they name a datastore** | Make it the [default](#the-default-for-new-disks). |
| **Overcommit** | On by default. See [Capacity](#overcommit-and-capacity). |

**gxctl:**

```bash
gxctl datastore create fast --path /srv/glidex/fast                    # node-local
gxctl datastore create shared1 --path /srv/glidex/shared1 --shared     # shared
gxctl datastore create fast --path /srv/glidex/fast --nodes node1,node2 \
  --node-path node2=/srv/glidex/nvme --no-overcommit
gxctl datastore list
gxctl datastore show shared1
```

| Option | Meaning |
|---|---|
| `--path P` | The directory (required). |
| `--shared` | A shared datastore (`shared-dir`); otherwise node-local (`dir`). |
| `--nodes N,M` | Only these nodes (names or ids); default: every node. |
| `--node-path NODE=P` | That node's own path; repeat for more nodes. |
| `--default` | Make it the default for new disks. |
| `--no-overcommit` | Refuse disks that don't fit. |

Over the API it is `POST /datastores` with `name`, `driver`
(`DATASTORE_DRIVER_DIR` or `DATASTORE_DRIVER_SHARED_DIR`), `path`, and
optionally `node_paths`, `nodes`, `default_for_disks` and `overcommit`.
See the [REST API](../api/rest.md).

In a cluster, datastores can be registered only once every server runs
a release that has them; until then the request is refused with
`409 conflict`.

### Each node's status

Each node checks every datastore it should have: at start, every 60
seconds, and whenever a datastore is added or changed. It checks that
its path is a writable directory under `/srv/glidex`, and reports its
total and free space.

`gxctl datastore list` shows how many nodes reach each datastore
(`NODES OK`, such as `2/3`) and the most free space on one of them. On
the **Datastores** page that is *Nodes reaching it* (`2 of 3`, in amber
when some don't); click a datastore's name for each node's path, status
and free space. `gxctl datastore show <name>` prints the same.

A node that can't use a datastore says why:

| Reason | Meaning | What to do |
|---|---|---|
| `OutsideRoot` | Its path isn't under `/srv/glidex`. | Fix the path or the node's own path. |
| `Missing` | The directory doesn't exist on this node. | Create it, or mount the filesystem. |
| `NotADirectory` | The path is a file. | Fix the path. |
| `ReadOnly` | Glidex can't write there: permissions, or a read-only mount. | Give `glidex` write access. |
| `NoMarker` | Shared: no marker in the directory. *Is the datastore mounted on this node?* | Mount it. |
| `ForeignMarker` | Shared: the marker belongs to another datastore or another cluster. | Use another export or directory. |
| `NotEmpty` | Shared: the directory has files but no marker. Glidex never takes over a directory with files in it. | Register an empty directory. |

The VMs of a disk on a registered datastore are placed only on nodes
that reach it; other nodes are refused with *datastore … is not
reachable here* (see the VM form's **Placement**).

## Shared datastores

### The marker

When a shared datastore is registered, one node claims it: it writes a
small marker file, `.glidex-datastore`, naming the datastore and the
cluster, into the empty directory. Every other node must then find that
marker at its own path before it uses the datastore. That way:

- a node where the filesystem isn't mounted (an empty local directory)
  is never used, and reports `NoMarker`;
- two clusters, or two datastores, can't share one directory by
  mistake (`ForeignMarker`).

Only one node claims, the first of the datastore's ready nodes, and
only while no node reaches the datastore yet. If that node's own mount
is missing, it claims its local directory instead: the datastore shows
*1 of N*, and the other nodes report `NoMarker`. While no disk is on
it, delete the datastore, empty that node's local directory, mount the
filesystem there, and register it again.

<!-- TODO(verify): recovery when the claimer's mount was missing (the marker sits in the local directory under the mount point). Confirm the steps above with the team before publishing. -->

### Workers

A disk on a shared datastore isn't bound to a node. One node at a time,
its **worker**, does the work on its file (making, resizing, deleting
it): the node of its VM once the VM is placed, otherwise a ready node
that reaches the datastore, chosen again when nodes come and go. The
**Disks** page's *Node* column shows it as `shared · node2`.

### Volume locks

Glidex locks every disk on a shared datastore while a VM runs it or the
worker works on it, with a lock file in the datastore's `.locks/`
directory. A VM whose disk is locked by another node doesn't start
(*disk … is in use elsewhere (… is locked)*), and a disk operation waits
until the lock is free. This keeps two nodes from writing one disk, for
example if a node that seemed gone still runs the VM.

The locks are POSIX record locks on the shared filesystem. Use a
filesystem that enforces them between nodes (NFSv4 does).

<!-- TODO(verify): which filesystems besides NFSv4 were tested for cluster-wide OFD locks (CephFS, vendor NAS). -->

### Images in shared datastores

A linked disk (the default for qcow2) is an overlay on its image. On a
shared datastore the image must be there too, so Glidex keeps one copy
of each image the datastore's linked disks use, in its `images/`
directory. The first time a linked disk needs it, the disk's worker
copies the image in and checks its digest; the disk waits meanwhile.

The **Images** page lists these copies apart from the node copies
(*in 1 shared datastore*). They have no **Remove** button: a copy goes
when its image is deleted, and an image can't be deleted while a
linked disk uses it.

Full copies and disks on node-local datastores use the node's own copy
of the image, as before.

## Registering at install time

The installer can register datastores for you, once:

```bash
cargo run -p glidex-install -- --datastore shared1=/srv/glidex/shared1
cargo run -p glidex-install -- --local-datastore fast=/srv/glidex/fast --datastore-default
```

| Option | Meaning |
|---|---|
| `--datastore NAME=PATH` | Register a shared datastore. Repeatable. |
| `--local-datastore NAME=PATH` | Register a node-local datastore. Repeatable. |
| `--datastore-default` | Make the first of them the default for new disks. |

None of these are saved in `install.conf`: a datastore is registered
once, for the whole cluster.

Without them, an interactive run looks for filesystems mounted directly
under `/srv/glidex` that aren't a datastore yet, and asks for each
whether to register it as a shared datastore named after its directory,
and whether it becomes the default for new disks (suggested answer:
no). Unattended runs ask nothing.

If registering fails (an agent has no API of its own; see
[Clusters](clusters.md#roles-and-quorum)), the installer prints a
warning and goes on; register it with `gxctl` on a server.

<!-- TODO(verify): on an agent the installer's gxctl call has no local API; confirm the warning wording and whether the installer should skip the offer there. -->

## The default for new disks

New disks go to the default datastore unless they name one: `local`
until you choose another. A VM's root disk made along with the VM also
goes there. Keeping `local` (fastest, but its disks tie VMs to their
node) or choosing shared storage (VMs can move, at network-storage
speed) is your call.

**Web UI:** **Datastores**, **Make default** on a datastore's row.

**gxctl:**

```bash
gxctl datastore set shared1 --default        # takes it from any other
gxctl datastore set shared1 --no-default     # back to local
```

## Overcommit and capacity

Disk files are thin, so by default (overcommit on) a datastore accepts
disks bigger than its free space, as `local` always has. With overcommit
off, a new disk, or a disk moved in, that doesn't fit in the free space
of the best node (less the disks still being made there) is refused
with `507 insufficient_storage`, with `free_bytes` and `needed_bytes`
in the error's details.

**Web UI:** **Stop overcommit** / **Allow overcommit** on the row.
**gxctl:** `gxctl datastore set fast --no-overcommit` (or
`--overcommit`).

The check happens when a disk is created or moved; files that grow
later can still fill the datastore. Project `disk_gib` quotas count
disks on every datastore together.

## Changing and deleting

```bash
gxctl datastore set fast --nodes node1,node2,node3
gxctl datastore set fast --all-nodes
gxctl datastore set fast --node-path node3=/srv/glidex/nvme
gxctl datastore rm fast
```

You can change the nodes, the nodes' own paths, the default and
overcommit (`PATCH /datastores/{id}`). The name, the path and whether it
is shared can't be changed: register a new datastore and
[move the disks](#moving-a-disk-between-datastores).

Refused, with `409 conflict` and the reason:

- changing a node's path while a running VM there has a disk on the
  datastore open: stop it first;
- taking a node out of `nodes` while a disk on the datastore is bound
  to it;
- deleting a datastore any disk is on: move or delete the disks first.
  On the **Datastores** page, **Delete** is disabled meanwhile.

Deleting a datastore leaves its directory and files in place; clean
them up yourself.

## Placing a stopped or lost VM again

A VM stays on the node it was placed on. A **stopped** VM can be placed
again on another node, and so can a **lost** one, whose node was
forgotten (see [A node is gone for good](clusters.md#a-node-is-gone-for-good)).
Nothing moves by itself.

**Web UI:** the VM's page, **Place on another node** (**Place on a
live node** for a lost VM): pick a ready node, or *Where there is room*.

**gxctl:**

```bash
gxctl place web1 --node node2      # on node2
gxctl place web1                   # wherever the scheduler finds room
```

It prints where the VM went, or *Not placed yet* with the reason it
waits. Over the API: `POST /vms/{id}/place` with an optional `node`.

What can go with the VM:

- disks on shared datastores (their worker follows the VM);
- a disk on a node-local datastore only if it is already on the target
  node, and never for a lost VM: [move it](#moving-a-disk-between-datastores)
  to a shared datastore first;
- no host-path root disk, no PCI devices, and no networks that exist on
  one node only (use a cluster network).

Anything else is refused with `400 invalid_config`, naming the cause.
A VM that is running is refused with `400 invalid_state`: stop it
first. On a standalone host there is nowhere else to go (`400`).

Placing a VM with `--node` pins it to that node, as choosing a node
when creating it does; without `--node` it loses its old pin. Start it
again afterwards. Files the VM kept on its old node stay there: its
runtime directory (console log) and, for QEMU, its UEFI variable store;
on the new node it starts with fresh UEFI variables from its firmware.

Placing VMs needs the `system-admin` role.

## Moving a disk between datastores

A disk can move to another datastore while no running VM uses it: from
`local` to a shared datastore and back, or onto another node-local
datastore. A VM whose disks are all on shared datastores can then be
placed on any node.

**Web UI:** **Disks**, **Move** on the disk's row (shown when there is
more than one datastore). Pick the **Datastore**; for a linked disk,
**Flatten** makes a standalone copy instead of an overlay on the
image there. The disk shows *· moving* until it is done.

**gxctl:**

```bash
gxctl disk move data1 --datastore shared1
gxctl disk move web-root --datastore fast --flatten
```

It waits up to 5 minutes, then says the disk is still moving; follow it
with `gxctl disk show`. Over the API: `POST /disks/{id}/move` with
`datastore`, and optionally `node`, `flatten` and `wait_secs`.

How it works:

- One node, the one that has the disk's file and reaches the
  destination, copies it into the destination, checks the copy, and
  only then switches the disk over and removes the old file. If the copy
  fails, the disk stays where it was (event `MoveFailed`).
- A linked disk that isn't flattened is rebased onto the image's copy
  at the destination; on a shared datastore Glidex makes that copy
  first.
- While the disk moves it can't be attached, resized or booted.
- Moving a disk needs the `editor` role in its project, and the right
  to use the destination datastore.

Refused, with the reason:

- a disk that is being deleted, already moving, not ready, or used by a
  running VM (`409 conflict`);
- the datastore it is already on, a destination the node with the disk
  doesn't reach, or one the disk's VM's node wouldn't reach
  (`400 invalid_disk`);
- an unknown datastore (`404 not_found`);
- a destination without overcommit that lacks the space
  (`507 insufficient_storage`).

A disk can't go straight from one node's datastore to another node's
(for example from `local` on node1 to `local` on node2): move it to a
shared datastore, then from there.

## Who may use a datastore

Every user who can create disks in a project can put them on any
datastore. To keep one for some tenants, add a
[site policy](access-control.md#site-policies) that forbids
`useDatastore` on it:

```cedar
@id("site.fast-for-research")
forbid (principal, action == Glidex::Action::"useDatastore", resource == Glidex::Datastore::"<datastore-id>")
unless { principal in Glidex::Tenant::"<tenant-id>" };
```

`gxctl datastore show <name>` prints the datastore's id. The check
applies when a disk is created on the datastore (including a VM's root
disk on the default datastore) and when a disk is moved to it.

## Limits

- No live migration: a VM moves only while stopped, and only when you
  ask.
- No direct move between two nodes' node-local datastores; go through
  a shared one.
- A node that leaves the cluster with its VMs
  ([detach](clusters.md#detach-a-node-with-its-vms)) can't take disks on
  registered datastores along: move them to `local` first.
- The status rows of a node that left stay listed until the datastore
  is deleted.
- Glidex doesn't take over a directory that already has files in it, so
  existing data can't be registered as a datastore.
- No block storage (LVM, Ceph RBD), no CSI plugins, no disk snapshots,
  and no per-datastore quotas yet.
- A VM's root disk made along with the VM always goes to the default
  datastore. To put it elsewhere, create the disk first
  (`gxctl disk create … --datastore`) and boot the VM from it.
