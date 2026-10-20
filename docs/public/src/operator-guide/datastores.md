# Datastores

A **datastore** is a place where disks live. Every disk is on exactly
one. Every host has the built-in datastore `local`: the control plane's
own disk directory on each node. You can register more:

- a **node-local** datastore (driver `dir`): a directory each node has its
  own of, such as a faster SSD. A disk on it lives on one node, and its
  VM runs there;
- an **LVM-thin** datastore (driver `lvm`): a volume group and thin pool
  on each node. A disk on it is a raw logical volume on one node, and
  its VM runs there (see [LVM-thin](#lvm-thin));
- a **shared** datastore (driver `shared-dir`): one filesystem that every
  node mounts, such as NFS or CephFS. A disk on it isn't tied to a node,
  so its VM can run on any node that reaches the datastore, and can be
  placed on another node when it is stopped or its node is lost.

Glidex doesn't mount anything. You mount the filesystem on each node,
and Glidex checks what it finds there. Datastores belong to the cluster
(or to the standalone host): register one once, from any server, and
every node checks it.

Registering, changing and deleting datastores needs the `system-admin`
role. Everyone can see them, and put disks on them unless a
[site policy](#who-may-use-a-datastore) says otherwise.

`local` is listed with the others, marked *built in*. It can't be
changed or deleted (`409 conflict`), and it is the default for new disks
until you [make another the default](#the-default-for-new-disks). Its
disks stay where they always were: nothing moves when you add
datastores.

## Mount it under /srv/glidex

The installer creates `/srv/glidex` (`root:root 0755`) and leaves it in
place on uninstall. The control plane and the VMs may write only there
and in their own directories, so datastores go below it, one directory
each, owned by the `glidex` user. A path elsewhere, or `/srv/glidex`
itself, is refused (`400 invalid_datastore`).

```bash
sudo mkdir /srv/glidex/nfs1
sudo mount -t nfs4 -o hard nfs.example.org:/export/glidex /srv/glidex/nfs1
sudo chown glidex:glidex /srv/glidex/nfs1
```

Add it to `/etc/fstab` so it comes back after a reboot. A mount made
while Glidex runs is seen at once; nothing needs restarting. For a
node-local datastore, create the directory on each node that should have
it:

```bash
sudo install -d -o glidex -g glidex -m 0750 /srv/glidex/fast
```

Glidex never creates the datastore's directory itself. It creates what it
needs inside: `disks/`, and on a shared datastore also `images/` and
`.locks/`. A shared datastore's directory must be **empty** when you
register it (`lost+found` is ignored); Glidex never takes over a
directory with files in it.

For NFS:

- export the directory owned by the `glidex` uid, which must be the same
  number on every node. Root squash is fine: Glidex never writes as root;
- mount it **hard**. With `soft`, a server outage fails the guests' disk
  writes, which can leave a qcow2 disk corrupt;
- keep NFS locking on (no `nolock`, no `local_lock=`). Glidex locks each
  disk file so that two nodes can't run it at once, and that only works
  when the lock reaches the server.

Glidex warns about `soft` and local locks: the datastore stays usable,
but `gxctl datastore show` and the **Datastores** page show the warning
for that node (see [Mount warnings](#mount-warnings)).

> **Note:** a filesystem mounted after the services started is writable
> by them wherever it is mounted, even outside `/srv/glidex`: the
> sandbox only covers what was mounted when they started. Use file
> ownership to decide what `glidex` may write.

## Register it

**Web UI:** **Datastores**, **Add Datastore**: a **Name**, the **Path**
(default `/srv/glidex/<name>`), and **Shared** for a filesystem every
node mounts. You can limit it to some **Nodes** (none ticked: every
node, including nodes that join later), give some nodes **Paths of
their own** (one `node=path` per line), make it the default for new
disks, and turn overcommit off.

**gxctl:**

```bash
gxctl datastore create nfs1 --path /srv/glidex/nfs1 --shared
gxctl datastore create fast --path /srv/glidex/fast --nodes node1,node2
gxctl datastore create fast2 --path /srv/glidex/fast --node-path node3=/srv/glidex/nvme
```

- `--shared`: one filesystem on every node (`shared-dir`); without it,
  each node has its own directory (`dir`).
- `--nodes N,M`: only these nodes have it (names or ids; default: every
  node).
- `--node-path NODE=PATH`: this node mounts it elsewhere (repeatable).
- `--default`: new disks go here unless they name a datastore.
- `--no-overcommit`: refuse a disk that doesn't fit in the free space
  (see [Overcommit and capacity](#overcommit-and-capacity)). By default
  disks are thin files and may promise more than there is.

A name is 1-64 letters, digits, `.`, `_` and `-`, not starting with `.`,
and unique (`local` included). Over the API it is `POST /datastores`
with `name`, `driver` (`DATASTORE_DRIVER_DIR` or
`DATASTORE_DRIVER_SHARED_DIR`), `path`, and optionally `node_paths`,
`nodes`, `default_for_disks` and `overcommit`; see the
[REST API](../api/rest.md). In a cluster, datastores can be registered
only once every server runs a release that has them; until then the
request is refused with `409 conflict`.

### Registering at install time

The installer can register datastores too, once:
`--datastore NAME=PATH` (shared), `--local-datastore NAME=PATH`, and
`--datastore-default` (the first of them becomes the default). They are
not saved for later runs. Run interactively without them, it offers each
unregistered mount point directly under `/srv/glidex` as a shared
datastore named after its directory, and asks whether it becomes the
default (suggested answer: no). Unattended runs ask nothing. If
registering fails, the installer prints a warning and goes on; register
the datastore with `gxctl` on a server. See
[Installation](installation.md).

### What "reachable" means

Each node checks every datastore it should have: at start, every
minute, when a datastore is added or changed, and when another node's
view changes. A node reaches it when the directory exists under
`/srv/glidex`, is a directory, and is writable; it also reports the
filesystem's total and free space.

A shared datastore also has a marker file, `.glidex-datastore`, with the
datastore's and the cluster's id. One node writes it into the empty
directory when the datastore is new (the first ready node, by id, and
only while no node reaches the datastore yet), and every other node
must find it. That catches the usual mistake: a node where the NFS
export isn't mounted sees an empty directory on its own disk, and
writing disks there would hide them from every other node. It shows as
unreachable with `NoMarker` instead. The marker also keeps two clusters,
or two datastores, from sharing one directory by mistake
(`ForeignMarker`).

So **mount the filesystem on every node before registering it**. If the
node that writes the marker hadn't mounted it, the marker lands on its
own disk and only that node reaches the datastore ("1 of N"). Then,
while no disk is on it, delete the datastore, remove the marker on that
node, mount, and register it again.

| Reason | Meaning | What to do |
|---|---|---|
| `Missing` | The directory doesn't exist on this node | Create it, or mount the filesystem |
| `NotADirectory` | The path is a file | Fix the path |
| `OutsideRoot` | The path isn't under `/srv/glidex` | Fix the path or the node's own path |
| `ReadOnly` | Glidex can't write there (mounted read-only, or the wrong owner) | Give `glidex` write access |
| `NoMarker` | Shared: no marker, so probably not mounted on this node | Mount it |
| `NotEmpty` | Shared: files but no marker; not claimed, since they aren't Glidex's | Register an empty directory |
| `ForeignMarker` | Shared: another datastore's or another cluster's directory | Use another export or directory |

```bash
gxctl datastore list          # each datastore, how many nodes reach it, free space
gxctl datastore show nfs1     # each node's path, free space, or why it can't
```

`gxctl datastore list` shows how many nodes reach each datastore
(`NODES OK`, such as `2/3`) and the most free space on one of them. On
the **Datastores** page that is *Nodes reaching it* (`2 of 3`, in amber
when some don't); click a datastore's name for each node's path, status
and free space.

The VMs of a disk on a registered datastore are placed only on nodes
that reach it; other nodes are refused with *datastore … is not
reachable here* (see the VM form's **Placement**).

### Mount warnings

A node that reaches a shared datastore on NFS also looks at the mount's
options, and records a warning for:

- `soft` or `softerr`: *mounted soft: an NFS server outage fails the
  guests' disk writes; mount it hard*;
- node-local locks, `nolock` or `local_lock=` other than `none`: *NFS
  locks are local to this node (…): disk locks don't keep other nodes
  off a disk*. The [volume locks](#volume-locks) then no longer keep two
  nodes' hypervisors off one disk.

The datastore stays reachable. `gxctl datastore show` prints the
warnings under the node; on the **Datastores** page the node shows
*reachable, with warnings*, with the warnings listed. Remount with the
right options; the warning goes at the node's next check.

## Disks on a shared datastore

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
(reason `DiskBusy`: *disk … is in use elsewhere (… is locked)*), and a
disk operation waits until the lock is free. This keeps two nodes from
writing one disk, for example if a node that seemed gone still runs the
VM.

The locks are POSIX record locks on the shared filesystem. Use a
filesystem that enforces them between nodes (NFSv4 does, with locking
on; see [Mount warnings](#mount-warnings)).

<!-- TODO(verify): which filesystems besides NFSv4 were tested for cluster-wide OFD locks (CephFS, vendor NAS). -->

## The default for new disks

New disks go to `local` unless they name a datastore, or another one is
the default. A VM's root disk made along with the VM goes there too.
Keeping `local` (fastest, but its disks tie VMs to their node) or
choosing shared storage (VMs can move, at network-storage speed) is
your call.

```bash
gxctl datastore set nfs1 --default        # takes it from any other
gxctl datastore set nfs1 --no-default     # back to local
```

In the web UI it is **Make default** on the datastore's row (on
`local`'s row to go back). Disks that already exist stay where they are.

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
gxctl datastore set fast --nodes node1,node2,node4
gxctl datastore set fast --all-nodes
gxctl datastore set fast --node-path node4=/srv/glidex/ssd
gxctl datastore set nfs1 --no-overcommit
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
- deleting a datastore while disks are on it: move or delete them
  first. On the **Datastores** page, **Delete** is disabled meanwhile.

Deleting a datastore leaves its directory and files alone; clean them up
yourself.

## Moving VMs and disks

- A stopped VM whose disks are all on shared datastores can be placed on
  another node: **Place on another node** on its page, or
  `gxctl place <vm> --node node2`.
- When a node is gone for good, `gxctl node forget <node> --fenced` marks
  its VMs lost; those on shared datastores can then be placed on a live
  node the same way (**Place on a live node**).
- A disk can be moved to another datastore while no running VM uses it:
  `gxctl disk move <disk> --datastore nfs1`. Moving a node-local disk to
  a shared datastore is how a VM on a node's own disk becomes free to
  move. Moving it straight to another node's own disk isn't possible:
  move it to a shared datastore first, then from there.

See [Images and disks](../user-guide/images-and-disks.md#moving-a-disk)
for moving disks, and [Clusters](clusters.md) for nodes. Nothing moves
by itself, and running VMs don't move.

### Placing a stopped or lost VM again

A VM stays on the node it was placed on. A **stopped** VM can be placed
again on another node, and so can a **lost** one, whose node was
forgotten (see [A node is gone for good](clusters.md#a-node-is-gone-for-good)).

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

Anything else is refused with `400 invalid_config`, naming the cause. A
VM that isn't stopped is refused with `400 invalid_state`: stop it
first. On a standalone host there is nowhere else to go
(`400 invalid_config`).

Placing a VM with `--node` pins it to that node, as choosing a node
when creating it does; without `--node` it loses its old pin. Start it
again afterwards. Files the VM kept on its old node stay there: its
runtime directory (console log) and, for QEMU, its UEFI variable store;
on the new node it starts with fresh UEFI variables from its firmware.

Placing VMs needs the `system-admin` role.

### Moving a disk between datastores

A disk moves while no running VM uses it: from `local` to a shared
datastore and back, or onto another node-local datastore. Users with the
`editor` role in its project move it, on the **Disks** page or with
`gxctl disk move`; see
[Moving a disk](../user-guide/images-and-disks.md#moving-a-disk) for the
commands and what is refused.

- One node, the one that has the disk's file and reaches the
  destination, copies it into the destination, checks the copy, and
  only then switches the disk over and removes the old file. If the copy
  fails, the disk stays where it was (event `MoveFailed`).
- A linked disk that isn't flattened is rebased onto the image's copy
  at the destination; on a shared datastore Glidex makes that copy
  first (see [Images on a shared datastore](#images-on-a-shared-datastore)).
- A destination without overcommit that lacks the space refuses the
  move (`507 insufficient_storage`).
- A disk can't go straight from one node's datastore to another node's
  (for example from `local` on node1 to `local` on node2): move it to a
  shared datastore, then from there.

## LVM-thin

An LVM-thin datastore keeps each disk as a raw thin volume in a volume
group you made on every node, through `glidex-stord`, a small root helper
the installer sets up. glidex never touches physical volumes, and stord
acts only on the volume groups you allow and only on its own volumes
(named `gx-d-…` and `gx-i-…`).

1. On each node, make a volume group and a thin pool for glidex alone:

   ```bash
   sudo vgcreate vg-glidex /dev/nvme1n1
   sudo lvcreate --type thin-pool -l 95%FREE -n pool vg-glidex
   ```

2. Allow it, by re-running the installer with `--stord-allow-vg
   vg-glidex`, or by adding it to `/etc/glidex/stord.json` and
   restarting `glidex-stord`:

   ```json
   { "allowed_vgs": ["vg-glidex"] }
   ```

3. Register it:

   ```bash
   gxctl datastore create fast --lvm vg-glidex/pool
   ```

   In the web UI: **Add datastore**, *LVM-thin on each node*. A node
   whose volume group is named otherwise: `--node-path node3=vg-nvme`.

What is different from a directory:

- disks are always raw; a linked disk is a thin snapshot of the image's
  volume on that node, made from the image the first time;
- disks grow but never shrink;
- each VM's unit may open exactly its own volumes;
- when the pool's metadata is 80% full, the datastore shows a warning:
  extend it with `lvextend --poolmetadatasize`.

| Reason | Meaning |
|---|---|
| `StordUnavailable` | `glidex-stord` isn't running on this node |
| `NotAllowed` | The volume group isn't in `/etc/glidex/stord.json` |
| `NoVolumeGroup` | No volume group of that name on this node |
| `NoThinPool` | The volume group has no thin pool of that name |

## Images on a shared datastore

A linked disk (a thin overlay on an image) on a shared datastore needs
the image on that datastore too. Glidex copies it there by itself, as
`images/<image-id>.qcow2` next to `disks/`, the first time a linked disk
needs it, and checks it against the image's checksum; the disk waits
meanwhile (*image … has not been copied into datastore … yet*). Full
copies, and disks on node-local datastores, use the node's own copy of
the image.

The **Images** page lists these copies apart from the node copies (*in
1 shared datastore*). They have no **Remove** button: they go when the
image is deleted, which isn't possible while a linked disk uses it.

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
- A node that leaves the cluster with its VMs
  ([detach](clusters.md#detach-a-node-with-its-vms)) can't take its disks
  on node-local datastores other than `local` along: move them to
  `local` first.
- The status rows of a node that left stay listed until the datastore
  is deleted.
- Existing data can't be registered as a datastore: Glidex doesn't take
  over a directory that already has files in it.
- No block storage (LVM, Ceph RBD), no CSI plugins, no disk snapshots,
  and no per-datastore quotas yet.
- A VM's root disk made along with the VM always goes to the default
  datastore. To put it elsewhere, create the disk first
  (`gxctl disk create … --datastore`) and boot the VM from it.
