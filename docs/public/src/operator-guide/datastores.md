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
  placed on another node when it is stopped or its node is lost;
- a **hyperstor** datastore (driver `hyperstor`): a pool of a
  [hyperstor](https://github.com/CMKL-APEX/hyperstor) cluster. A disk on
  it is a hyperstor volume, shared like a disk on a shared filesystem
  (see [Hyperstor](#hyperstor)).

Glidex doesn't mount anything or make volume groups. You mount the
filesystem (or make the volume group) on each node, and Glidex checks
what it finds there. Datastores belong to the cluster
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

This and the next sections are about directories (`dir` and
`shared-dir`). An LVM-thin datastore needs no mount: see
[LVM-thin](#lvm-thin).

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
`DATASTORE_DRIVER_SHARED_DIR`; for LVM-thin see
[Register an LVM-thin datastore](#register-an-lvm-thin-datastore)),
`path`, and optionally `node_paths`,
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

An LVM-thin datastore has reasons of its own; see
[Node status](#node-status).

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

Disk files and LVM-thin volumes are thin, so by default (overcommit on)
a datastore accepts
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
  first. On the **Datastores** page, **Delete** is disabled meanwhile;
- deleting an LVM-thin datastore while a node keeps an image's volume
  in it (`gxctl datastore show` lists them as *image volumes*): delete
  those images first, or remove their `gx-i-…` volumes with `lvremove`.
  It can go once each node has looked again, within a minute (see
  [Deleting an LVM-thin datastore](#deleting-an-lvm-thin-datastore)).

Deleting a datastore leaves its directory and files, or its volume group
and pool, alone; clean them up yourself.

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
datastore and back, or onto another node-local datastore, LVM-thin
included. Users with the
`editor` role in its project move it, on the **Disks** page or with
`gxctl disk move`; see
[Moving a disk](../user-guide/images-and-disks.md#moving-a-disk) for the
commands and what is refused.

- One node, the one that has the disk's file and reaches the
  destination, copies it into the destination, checks the copy, and
  only then switches the disk over and removes the old file. If the copy
  fails, the disk stays where it was (event `MoveFailed`).
- Onto or off an LVM-thin datastore, the copy is always full, and raw
  on it (see [Moving disks on and off](#moving-disks-on-and-off)).
- A linked disk that isn't flattened is rebased onto the image's copy
  at the destination; on a shared datastore Glidex makes that copy
  first (see [Images on a shared datastore](#images-on-a-shared-datastore)).
- A destination without overcommit that lacks the space refuses the
  move (`507 insufficient_storage`).
- A disk can't go straight from one node's datastore to another node's
  (for example from `local` on node1 to `local` on node2): move it to a
  shared datastore, then from there.

## LVM-thin

An LVM-thin datastore (driver `lvm`) keeps each disk as a raw thin
logical volume in a volume group and thin pool on each node. It is
node-local: a disk on it lives on one node, and its VM runs there.

Glidex runs as an unprivileged user, so a small root helper,
`glidex-stord`, makes, activates, grows and removes the volumes for it.
stord acts only on the volume groups you allow, and only on Glidex's
own volumes: `gx-d-<disk-id>` for disks and `gx-i-<image-id>` for
images. It never touches physical volumes, other logical volumes, or a
volume group you didn't list. The installer sets it up on every host
(see [Installation](installation.md)); it keeps no state of its own.

### Prepare the volume group

You make the volume group and its thin pool, on each node that should
have the datastore. Give Glidex a volume group of its own:

```bash
sudo apt-get install lvm2 thin-provisioning-tools   # dnf: lvm2 device-mapper-persistent-data
sudo vgcreate vg-glidex /dev/nvme1n1
sudo lvcreate --type thin-pool -l 95%FREE -n pool vg-glidex
```

<!-- TODO(verify): the installer doesn't install lvm2 or the thin-provisioning tools; check the package names on dnf, yum and pacman hosts. -->

Leaving some of the volume group free lets you grow the pool, or its
metadata, later.

### Allow it in glidex-stord

stord allows no volume group by default. List yours in
`/etc/glidex/stord.json`, on each node:

```bash
cargo run -p glidex-install -- --stord-allow-vg vg-glidex
```

The installer adds the group to the file (repeat the flag for more
than one; it is not saved for later runs, but the file keeps the list)
and restarts `glidex-stord`. Or edit the file and restart stord
yourself:

```json
{ "allowed_vgs": ["vg-glidex"] }
```

```bash
sudo systemctl restart glidex-stord
```

A missing file means no volume group. A file stord can't read or parse
stops it from starting (`journalctl -u glidex-stord`); every node then
shows the datastore as `StordUnavailable`. Restarting stord doesn't
disturb running VMs: they keep their devices open.

### Register an LVM-thin datastore

**Web UI:** **Datastores**, **Add Datastore**, *LVM-thin on each node*:
the **Name**, the **Volume group** and the **Thin pool**. In a cluster,
**Volume groups of their own** takes one `node=volume-group` per line
for nodes whose group is named otherwise.

**gxctl:**

```bash
gxctl datastore create thin1 --lvm vg-glidex/pool
gxctl datastore create thin2 --lvm vg-glidex/pool --nodes node1,node2 --node-path node3=vg-nvme
```

- `--lvm VG/POOL`: the volume group and its thin pool, instead of
  `--path`.
- `--node-path NODE=VG`: this node's own volume group (same pool name).
- `--nodes`, `--default` and `--no-overcommit` work as for a directory.

Over the API: `POST /datastores` with `driver`
`DATASTORE_DRIVER_LVM`, `volume_group` and `thin_pool`, and no `path`
(`400 invalid_datastore` otherwise); `node_paths` then maps nodes to
their volume groups. Volume group and pool names are plain LVM names:
letters, digits, `.`, `_`, `-` and `+`, not starting with `-`.

The volume group and the pool can't be changed afterwards; a node's own
volume group can (`gxctl datastore set thin1 --node-path node3=vg-nvme`),
but Glidex doesn't move volumes from the old group.

### Node status

Each node asks its `glidex-stord` about the pool, as it checks a
directory (see [What "reachable" means](#what-reachable-means)). A node
that reaches it reports the pool's size and free space; `gxctl datastore
show` prints its path as `vg-glidex/pool`.

| Reason | Meaning | What to do |
|---|---|---|
| `StordUnavailable` | `glidex-stord` isn't running on this node, or doesn't answer | `systemctl status glidex-stord`; it isn't installed with `--no-services` |
| `NotAllowed` | The volume group isn't in `/etc/glidex/stord.json` | [Allow it](#allow-it-in-glidex-stord) |
| `NoVolumeGroup` | No volume group of that name on this node, or the LVM tools failed (the message says which) | Create it, fix the node's own volume group, or install `lvm2` |
| `NoThinPool` | The volume group has no thin pool of that name | Create the pool, or fix the name |

It also warns, while the datastore stays usable, when the pool's data
or its metadata is 80% full (see [Thin pool space](#thin-pool-space)).

### What is different from a directory

- **Raw only.** Every disk is a raw volume; asking for qcow2 is refused
  (`400 invalid_disk`).
- **Linked disks are thin snapshots.** The first linked disk from an
  image on a node makes that node's image volume, `gx-i-<image-id>`,
  from the verified image, and seals it read-only. Each linked disk is
  a thin snapshot of it: quick to make, and it takes pool space only
  for what the guest changes. A full disk is a converted copy.
- **Image volumes stay with their image.** A node keeps an image's
  volume until the image is deleted, even when no linked disk uses it
  any more. Deleting the image removes its volumes on every node.
- **Disks grow but never shrink** (`400 invalid_disk`). Growing a disk
  made from an image extends its root partition as usual.
- **Device access per VM.** Before a VM starts, stord lets that VM's
  unit open exactly its own volumes; deleting the VM takes that away.
- **No volume lock of Glidex's own.** The volumes are on one node, and
  the hypervisors' own locks refuse a second instance of a disk.

### Moving disks on and off

A disk moves onto or off an LVM-thin datastore like any other (see
[Moving a disk between datastores](#moving-a-disk-between-datastores)),
always as a full, standalone copy: a raw volume has no backing file.
A disk moved onto it becomes raw. A disk moved off it to a directory
stays raw, as a raw file.

Volumes are on one node, so the usual rules hold: a disk can't go
straight from one node's volume group to another node's; move it
through a shared datastore. A VM with a disk on an LVM-thin datastore
can be placed again only on the node that has the volume.

### Metering

`disk.stored` for a volume is the pool space it holds, not its size.
For a file it is still the file's allocated size. For a linked volume
it is what LVM reports as mapped, which includes the blocks it still
shares with its image volume, so the linked disks of one image together
can count more than the pool holds.

### Thin pool space

Volumes are thin, so the pool, like a directory with overcommit on, can
promise more than it has. With overcommit off, a new or moved disk that
doesn't fit in the pool's free space is refused
(`507 insufficient_storage`). Either way, keep an eye on it:

- when the pool's data or metadata is 80% full, the node shows a
  warning (*reachable, with warnings* on the **Datastores** page,
  `warning:` under the node in `gxctl datastore show`);
- **a full thin pool stops every guest on it at its next write.** Grow
  the pool before that, from free space in the volume group:

  ```bash
  sudo lvextend -L +100G vg-glidex/pool                  # data
  sudo lvextend --poolmetadatasize +1G vg-glidex/pool    # metadata
  ```

  LVM can also grow it by itself (`thin_pool_autoextend_threshold` in
  `lvm.conf`), as long as the volume group has room.

### Deleting an LVM-thin datastore

Besides the disks, its image volumes keep an LVM-thin datastore: the
delete is refused (`409 conflict`, naming the images and nodes) while
any node keeps one. `gxctl datastore show` lists them under each node
as *image volumes* (image ids). Delete those images, or remove the
volumes on the node:

```bash
sudo lvremove vg-glidex/gx-i-<image-id>
```

The delete goes through once each node has looked again, within a
minute. The volume group, the pool and anything else in them stay.

## Hyperstor

A hyperstor datastore (driver `hyperstor`) keeps each disk as a volume
of a hyperstor cluster's pool, replicated across that cluster's nodes.
It is shared: a disk on it isn't tied to a node, and its VM can run on
any node that runs `hyperstor-vhost`, hyperstor's vhost-user-blk
service. The VM's hypervisor opens the volume through that service's
socket, as a disk with 512-byte sectors.

Glidex manages the volumes (create, clone, grow, delete) through the
cluster's API, as a management identity of its own. A hyperstor volume
is held by one node at a time: the node whose VM uses it holds its
lease, and another node can't attach it meanwhile.

A hold can outlive its VM there: the VM's node crashed, or a detach
failed. Each node lets go, within a minute, of the volumes it holds for
VMs placed elsewhere. And a VM starting on a node takes its disks from
another node's `hyperstor-vhost` that still holds them, which cuts off
only those volumes there; the node's other VMs carry on. A disk that a
Glidex disk operation holds (`hyperstorctl`, on a server) is waited for:
the VM shows `DiskBusy` until it ends. This needs hyperstor with
per-volume fencing on every node; with an older `hyperstor-vhost`, a
held disk is waited for as before.

### Prepare the nodes

On each node that should run VMs from hyperstor disks:

- run hyperstor's `hyperstor-vhost` for the cluster's map service, with
  its default directory (`/run/hyperstor/vhost`), letting the `glidex`
  user use its control socket and giving its disks' sockets the
  `glidex` group, so that the hypervisors may open them. hyperstor's
  installer does this: `hyperstor-install install --vhost --vhost-user
  glidex` on a host that already runs a hyperstor role, or
  `hyperstor-install install --role vhost --map ADDR[,ADDR...]
  --vhost-user glidex` on a host that only runs VMs;
- install `hyperstorctl`: Glidex writes images into volumes and edits
  their partitions with it.

Servers also need to reach the cluster's API (port 8850 by default):
they do the volume operations, because only they hold Glidex's
identity.

### Register a hyperstor datastore

Make a single-use management token on the hyperstor cluster, and give
it to Glidex in a file:

```bash
hyperstorctl token create --role mgmt --name glidex > glidex.token
gxctl datastore create hs1 --hyperstor 192.0.2.21:8850,192.0.2.22:8850 --pool 1 --token-file glidex.token
```

- `--hyperstor API,...`: the cluster API's addresses, tried in turn.
- `--pool N`: the hyperstor pool for the disks (`hyperstorctl pools`).
- `--token-file F`: the token. Glidex enrolls with it once, from the
  server that takes the request: it makes a key, gets a certificate for
  the identity the token names, and keeps the key sealed with the
  cluster's secrets key. The token is used up and isn't kept.
- `--nodes`, `--default` and `--no-overcommit` work as for a directory.
  With `--no-overcommit`, a disk larger than the pool's free space is
  refused (`507 insufficient_storage`).

While the pool is **full** (past hyperstor's full ratio), new disks on
the datastore are refused with `507 insufficient_storage`, overcommit
or not, and `gxctl datastore show` says so on each server's line. A
disk whose creation hyperstor refuses for it waits (`DatastoreFull`)
and is tried again each minute, so freeing space (purging hyperstor's
trash, adding a device) lets it finish. Disks already there keep
working. A nearly full pool is a warning only.

**Web UI:** **Datastores**, **Add Datastore**, *hyperstor cluster*: the
**Name**, the **Cluster API** addresses, the **Pool** and the **Token**.

Over the API: `POST /datastores` with `driver`
`DATASTORE_DRIVER_HYPERSTOR`, `hyperstor_api`, `hyperstor_pool` and
`hyperstor_token`, and no `path`. `gxctl datastore show` prints the
identity and when its certificate expires; the leader renews it when a
third of its life is left.

### Node status

Each node reports whether its `hyperstor-vhost` answers
(`VhostUnavailable` otherwise), and each server whether the cluster's
API answers with Glidex's identity (`ApiUnavailable`).

Free and total space are the pool's, as hyperstor reports them: usable
space, after its copies or erasure coding. Servers read them from the
cluster's API; other nodes report none (`0` in `gxctl datastore show`,
`—` on the **Datastores** page), and so does a server whose hyperstor
doesn't report space yet (a hyperstor too old to report it, or while one of the
pool's devices is yet to report). `gxctl datastore list` and the
**Datastores** page show the most any node reports. Pools of one hyperstor cluster that share devices each
count the same free space.

### What is different from a directory

- **Disks are raw.** A request for qcow2 is refused; a linked disk is a
  hyperstor clone of the image's volume, made at once.
- **Images** are written into one volume per image and datastore, the
  first time a linked disk needs it, and kept as its snapshot `golden`.
- **Disks grow, and never shrink.** Growing and the root partition's
  extension work as for a file; the partition is edited through
  `hyperstorctl`, not a block device.
- **Moves** onto or off a hyperstor datastore aren't supported yet.
- **Placing a lost VM again** (`--fenced`) takes its disks' leases from
  the forgotten node at its next start. Otherwise a VM takes only holds
  that another node's `hyperstor-vhost` left behind (see above); a hold
  of a disk operation is waited for.
- **Metering** reports the disks' size, not the space they use.

### Deleting

A deleted disk's volume goes to hyperstor's trash, where hyperstor's
operators can bring it back until its grace period ends. A volume left
by a disk or image that failed to be made holds no one's data: Glidex
deletes it for good (purging it from the trash), so that a retry can
make it again. A clone there
still needs its image's volume, so when the image is deleted that
volume stays until the clone is purged; the leader then removes it,
within a minute. Deleting the datastore removes Glidex's sealed key; it
needs no disks on it, and leaves the identity's certificate to expire
(revoke it with `hyperstorctl cert revoke`).

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
- A node that leaves the cluster drops out of each datastore's status
  within a minute; a node that is only down keeps its last status.
- Existing data can't be registered as a datastore: Glidex doesn't take
  over a directory that already has files in it.
- Block storage is node-local LVM-thin only: no shared LVM, no Ceph
  RBD, no CSI plugins, no disk snapshots, and no per-datastore quotas
  yet.
- A VM's root disk made along with the VM always goes to the default
  datastore. To put it elsewhere, create the disk first
  (`gxctl disk create … --datastore`) and boot the VM from it.
