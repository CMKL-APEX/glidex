# Datastores

A **datastore** is a place where disks live. Every host has the built-in
datastore `local`: the control plane's own disk directory on each node.
You can register more:

- a **node-local** datastore (driver `dir`): a directory each node has its
  own of, such as a faster SSD. A disk on it lives on one node, and its
  VM runs there;
- a **shared** datastore (driver `shared-dir`): one filesystem that every
  node mounts, such as NFS or CephFS. A disk on it isn't tied to a node,
  so its VM can run on any node that reaches the datastore, and can be
  placed on another node when it is stopped or its node is lost.

Glidex doesn't mount anything. You mount the filesystem on each node,
and Glidex checks what it finds there.

Registering, changing and deleting datastores needs the `system-admin`
role. Everyone can see them, and put disks on them unless a site policy
says otherwise.

## Mount it under /srv/glidex

The installer creates `/srv/glidex` (`root:root 0755`). The control plane
and the VMs may write only there and in their own directories, so
datastores go below it, one directory each, owned by the `glidex` user:

```bash
sudo mkdir /srv/glidex/nfs1
sudo mount -t nfs4 -o hard nfs.example.org:/export/glidex /srv/glidex/nfs1
sudo chown glidex:glidex /srv/glidex/nfs1
```

Add it to `/etc/fstab` so it comes back after a reboot. A mount made
while Glidex runs is seen at once; nothing needs restarting.

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
for that node.

> **Note:** a filesystem mounted after the services started is writable
> by them wherever it is mounted, even outside `/srv/glidex`: the
> sandbox only covers what was mounted when they started. Use file
> ownership to decide what `glidex` may write.

## Register it

**Web UI:** **Datastores**, **Add datastore**: a **Name**, the **Path**,
and **Shared** for a filesystem every node mounts. You can limit it to
some **Nodes**, give some nodes **Paths of their own**, make it the
default for new disks, and turn overcommit off.

**gxctl:**

```bash
gxctl datastore create nfs1 --path /srv/glidex/nfs1 --shared
gxctl datastore create fast --path /srv/glidex/fast --nodes node1,node2
gxctl datastore create fast2 --path /srv/glidex/fast --node-path node3=/srv/glidex/nvme
```

- `--shared`: one filesystem on every node (`shared-dir`); without it,
  each node has its own directory (`dir`).
- `--nodes N,M`: only these nodes have it (default: every node).
- `--node-path NODE=PATH`: this node mounts it elsewhere (repeatable).
- `--default`: new disks go here unless they name a datastore.
- `--no-overcommit`: refuse a disk that doesn't fit in the free space.
  By default disks are thin files and may promise more than there is.

The installer can register datastores too, once:
`--datastore NAME=PATH` (shared), `--local-datastore NAME=PATH`, and
`--datastore-default`. Run interactively without them, it offers each
unregistered mount point directly under `/srv/glidex`.

### What "reachable" means

Each node checks every datastore it should have, and again every minute
and whenever another node's view changes. A node reaches it when the
directory exists under `/srv/glidex`, is a directory, and is writable.

A shared datastore also has a marker file, `.glidex-datastore`, with the
datastore's and the cluster's id. One node writes it into the empty
directory when the datastore is new (the first ready node, by id), and
every other node must find it. That catches the usual mistake: a node
where the NFS export isn't mounted sees an empty directory on its own
disk, and writing disks there would hide them from every other node. It
shows as unreachable with `NoMarker` instead.

So **mount the filesystem on every node before registering it**. If the
node that writes the marker hadn't mounted it, the marker lands on its
own disk and only that node reaches the datastore ("1 of N"). Then
delete the datastore, remove the marker on that node, mount, and
register it again.

| Reason | Meaning |
|---|---|
| `Missing` | The directory doesn't exist on this node |
| `NotADirectory` | The path is a file |
| `OutsideRoot` | The path isn't under `/srv/glidex` |
| `ReadOnly` | Glidex can't write there (mounted read-only, or the wrong owner) |
| `NoMarker` | Shared: no marker, so probably not mounted on this node |
| `NotEmpty` | Shared: files but no marker; not claimed, since they aren't Glidex's |
| `ForeignMarker` | Shared: another datastore's or another cluster's directory |

```bash
gxctl datastore list          # each datastore, how many nodes reach it, free space
gxctl datastore show nfs1     # each node's path, free space, or why it can't
```

## The default for new disks

New disks go to `local` unless they name a datastore, or another one is
the default:

```bash
gxctl datastore set nfs1 --default
```

In the web UI it is **Make default** on the datastore's row. Disks that
already exist stay where they are.

## Changing and deleting

```bash
gxctl datastore set fast --nodes node1,node2,node4
gxctl datastore set fast --node-path node4=/srv/glidex/ssd
gxctl datastore set nfs1 --no-overcommit
gxctl datastore rm fast
```

A datastore can't be deleted while disks are on it. Deleting it leaves
its directory and files alone.

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
for moving disks, and [Clusters](clusters.md) for nodes.

## Images on a shared datastore

A linked disk (a thin overlay on an image) on a shared datastore needs
the image on that datastore too. Glidex copies it there by itself, as
`images/<image-id>.qcow2` next to `disks/`, the first time a linked disk
needs it, and checks it against the image's checksum. The **Images**
page lists these copies. They go when the image is deleted, which isn't
possible while a linked disk uses it.
