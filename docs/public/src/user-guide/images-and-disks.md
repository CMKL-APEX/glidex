# Images and disks

- An **image** is a read-only base: a distro cloud image that VMs'
  disks are made from, or the UEFI **firmware** VMs boot through. Images
  are a library shared by every project.
- A **disk** is a writable volume in a project: a VM's root disk or an
  extra data disk. A disk can start blank or as a copy of an image.

Pulling and deleting images needs the host-wide `image-admin` role;
everyone can see and use the library. Creating, resizing and deleting
disks needs the `editor` role in the disk's project.

## The catalog

Glidex has a built-in catalog of distro cloud images. Each entry points
at the vendor's current build, and every download is checked against
the checksum the vendor publishes next to it.

| Key | Image |
|-----|-------|
| `ubuntu-26.04` | Ubuntu 26.04 LTS |
| `ubuntu-24.04` | Ubuntu 24.04 LTS |
| `ubuntu-22.04` | Ubuntu 22.04 LTS |
| `debian-13` | Debian 13 |
| `debian-12` | Debian 12 |
| `fedora-cloud` | Fedora Cloud Base, latest release |
| `alma-9` | AlmaLinux 9 |

Each is available for x86_64 and aarch64; Glidex fetches the one for the
host.

**Web UI:** **Images**, *Cloud image catalog*. Downloaded entries say
so.

**gxctl:**

```bash
gxctl image catalog
```

## Pulling an image

**Web UI:** **Images**, then **Pull** on a catalog entry. The image
appears under *Cloud images* and shows its download progress, then
**Ready**.

**gxctl:**

```bash
gxctl image pull ubuntu-24.04
```

It follows the download until the image is ready; Ctrl-C stops watching,
not the download. `--name <name>` names the image (default: the catalog key, or
the file name for a URL); names are 1-64 characters of letters, digits, `.`, `_` and
`-`.

### From a URL

Any qcow2 or raw image reachable over HTTPS can be pulled too:

```bash
gxctl image pull https://images.example.org/my-image.qcow2 --name my-image --sha256 <sha256>
```

**Web UI:** **Images**, **Download from URL**: *Type* **Cloud image
(qcow2 or raw)**, the **Image URL (https)**, the **sha256** and an
optional **Name**.

Give the sha256 whenever you can: without it the image is stored but
listed as *unverified*. URLs must be `https://` and, unless the operator
allows it, on a public address.

### Listing, retrying and deleting

```bash
gxctl image list
gxctl image retry my-image     # download a failed image again
gxctl image rm my-image
```

`image list` shows each image's state (`downloading 40%`, `verifying`,
`ready`, `failed: <reason>`, `missing`), size and source. In the web UI
the image tables have **Retry** for a failed download, **Cancel** for
one in progress and **Delete**.

An image can't be deleted while a disk is being made from it or a
linked disk still uses it as its base (see [Clone modes](#clone-modes)).

In a cluster, a server downloads and checks the image, and nodes copy
it from there when they need it. When pulling in the web UI you can pick
**Copy to nodes** to have copies ready sooner; the **Images** page shows
each node's copy and can copy an image to another node or let a copy go.

Linked disks on a [shared datastore](#where-a-disk-lives) use a copy of the
image kept in that datastore. The **Images** page lists these copies
apart from the node copies (*in 1 shared datastore*); they go when the
image is deleted.

## Firmware images

VMs that boot a cloud image start through UEFI firmware built for their
hypervisor. Firmware is managed like the other images:

| Key | Firmware | Hypervisor |
|-----|----------|------------|
| `cloudhv-edk2` | Cloud Hypervisor's EDK2 build (pinned) | Cloud Hypervisor |
| `ovmf` | OVMF from the host's package | QEMU |
| `ovmf-debian` | OVMF from a pinned Debian package | QEMU |

The control plane imports `cloudhv-edk2` (from the installer's copy)
and the host's OVMF by itself, once, at its first start. If they are
missing, add them:

**Web UI:** **Images**, *Firmware catalog*: **Pull** (or **Import** for
the host's OVMF). Firmware you have appears under *Firmware*.

**gxctl:**

```bash
gxctl image pull --firmware cloudhv-edk2
gxctl image pull --firmware ovmf
```

`gxctl image catalog` lists the firmware catalog after the cloud images,
with a hint when a host package is missing.

Other firmware builds can be pulled by URL: in **Download from URL**
choose **UEFI firmware (.fd)** and **For hypervisor**; or

```bash
gxctl image pull https://fw.example.org/OVMF_CODE.fd --firmware-for qemu \
  --sha256 <sha256> --vars-url https://fw.example.org/OVMF_VARS.fd --vars-sha256 <sha256>
```

`--vars-url` is the variable store that goes with a split OVMF build;
each VM gets its own copy of it.

A new VM uses the newest firmware image for its hypervisor unless you
pick another when creating it (see [Virtual machines](vms.md)).

## Kernel images

A VM can also boot a kernel directly, without firmware: a *kernel image*
is a kernel, an optional initramfs and the kernel command line VMs get
by default.

**From a cloud image.** Pulling a catalog image also takes the kernel
out of it: a second image, `<image>-kernel`, appears and becomes ready
a few seconds after the image (state *extracting* meanwhile). It is the
kernel the image's own bootloader would boot, with its initramfs and
command line (`root=…`, then `console=ttyS0`). To skip it, pull with
`"with_kernel": false`; to take the kernel of any other disk image,
including one pulled from a URL:

```bash
curl … -X POST /images -d '{"kernel_from": "my-image"}'
```

or **Extract kernel** on the image under **Images**, or `gxctl image
extract-kernel my-image`. `gxctl image pull <key> --no-kernel` skips it.

glidex reads the image inside a small, network-less VM it builds from
pinned Debian packages the first time (a 35 MB download), never on the
host. Hosts that can't download it can turn this off with
`GLIDEX_KERNEL_EXTRACTION=off`.

**From a URL**:

```bash
curl … -X POST /images -d '{
  "url": "https://example.org/vmlinuz-6.8.0-147-generic", "sha256": "<sha256>",
  "kind": "IMAGE_KIND_KERNEL",
  "initrd_url": "https://example.org/initrd.img-6.8.0-147-generic", "initrd_sha256": "<sha256>",
  "cmdline": "root=LABEL=cloudimg-rootfs ro console=ttyS0"
}'
```

or, for an administrator, from files on the host (`"kernel_path"`,
`"initrd_path"`, `"cmdline"`). Kernel images appear under **Images**,
*Kernels*, with their release; `gxctl image list` shows them as
`kernel <release>`. Like firmware, a kernel image can't be deleted while
a VM boots it or keeps it to go back to (`409 conflict`, naming the VMs).

Use the kernel the cloud image was built with: the guest loads the
kernel's modules from its own disk. Extraction does that for you; for a
URL, Ubuntu publishes each build's kernel and initramfs next to the
image, in `unpacked/`.

Extraction runs only on a host of the image's architecture (x86_64, or
aarch64 with Cloud Hypervisor), and an aarch64 kernel gets
`console=ttyAMA0` instead of `console=ttyS0`; a failed extraction fails only
the kernel image (**Retry**, or `gxctl image retry`). See
[Kernel boot](kernel-boot.md) to boot VMs from kernel images and to
follow the guest's kernel.

## Disks

### Creating a disk

Creating a VM from an image already makes its root disk (see
[Virtual machines](vms.md)). Create disks yourself for data, or to keep
a root disk independent of any one VM.

**Web UI:** **Disks**, **Create Disk**:

- **Name**: 1-64 characters of letters, digits, `.`, `_` and `-`.
  Disk names are unique.
- **Contents**: **Blank disk**, or an image.
- **Size (GiB)**: required for a blank disk. From an image it defaults
  to the image's size or 10 GiB, whichever is larger, and can't be
  smaller than the image.
- **Format**: `qcow2` (default) or `raw`. On an
  [LVM-thin datastore](#disks-on-an-lvm-thin-datastore), choose `raw`.
- **Clone** (from an image): **Linked** or **Full copy**.
- **Datastore**: where the disk lives, shown once the operator has
  registered more than `local` ([Datastores](../operator-guide/datastores.md)).
  The default is preselected.

**gxctl:**

```bash
gxctl disk create data1 --size-gib 50                    # blank qcow2
gxctl disk create web-root --image ubuntu-24.04 --size-gib 20
gxctl disk create web-root2 --image ubuntu-24.04 --full --raw
gxctl disk create data2 --size-gib 50 --datastore nfs1   # on a datastore other than the default
```

When a disk is made from an image and given more space than the image
has, Glidex extends its root partition to fill it (the guest grows the
filesystem when it boots). `--no-extend` leaves the partition as it is.

If the image is still downloading, the disk waits for it. On a shared
datastore, a linked disk also waits while the image is copied into the
datastore.

A datastore whose operator turned overcommit off refuses a disk that
doesn't fit in its free space (`507 insufficient_storage`).

### Clone modes

- **Linked** (default for qcow2): a thin overlay on the image. Fast and
  small, but the image must stay as long as the disk exists.
- **Full**: an independent copy. Needed for `raw` disks, except on an
  LVM-thin datastore, where a linked disk is a raw thin snapshot.

### Listing and inspecting disks

```bash
gxctl disk list
gxctl disk show web-root      # size, format, partitions, free space at the end
```

`disk list` shows the disks of the selected project (see
[Choosing a project](projects.md#choosing-a-project)), with their size,
format, origin, status and the VM each is attached to. In the web UI,
click a disk's name for its details and partitions.

### Attaching a disk to a VM

A disk is attached to one VM at a time.

- **As the root disk** when you create a VM: in the web UI choose
  **Boot Disk**, *Existing disk*; in `gxctl create` answer `disk` at
  *Boot disk from*.
- **As a data disk**: `gxctl create` asks for *Data disks* (disk names,
  comma-separated). For an existing VM, use the API:

  ```bash
  curl --cacert host.crt -H "Authorization: Bearer $GLIDEX_TOKEN" \
    -H 'Content-Type: application/json' \
    -X POST https://host.example.org:8841/vms/<vm-id>/disks -d '{"disk": "data1"}'
  ```

  On a running VM it takes effect the next time the VM starts.
  `DELETE /vms/<vm-id>/disks/<disk>` detaches it, as does **Detach** next
  to the disk on the VM's page in the web UI; the disk and its data stay.
  A disk detached from a running VM stays attached to it until the VM
  stops (or is deleted), and can be deleted from then on.

<!-- TODO(verify): gxctl has no attach/detach command for a data disk on an existing VM, and the web UI only detaches. Update this section if one is added. -->

### Growing and shrinking

**Web UI:** **Disks**, **Resize**, enter the **New size (GiB)**. When
growing a disk made from an image, **Extend the root partition into the
new space** is ticked.

**gxctl:**

```bash
gxctl disk resize web-root 40               # grow, and extend the root partition
gxctl disk resize data1 100 --no-extend     # grow, leave the partitions alone
gxctl disk resize data1 30                  # shrink
```

- Growing counts against the project's `disk_gib` quota.
- Shrinking only gives back unpartitioned space at the end of the disk;
  Glidex never shrinks filesystems. A size that would cut into a
  partition is refused with the smallest size allowed.
- A resize of a disk a running VM uses is recorded and applied once the
  VM stops.
- A disk on an LVM-thin datastore can only grow; shrinking it is refused
  (`400 invalid_disk`).

To grow the root partition into space that is already there (say, after
a resize with `--no-extend`):

```bash
gxctl disk extend-root web-root             # from the host, once no running VM uses the disk
gxctl disk extend-root web-root --on-boot   # by the guest, at its next boot
```

In the web UI it is **Extend root** on the disk's row. `--on-boot` relies
on the cloud-init seed Glidex generates; a VM with a custom seed has to
grow the partition itself (see [Guest credentials](credentials.md)).

### Deleting a disk

**Web UI:** **Disks**, **Delete** (disabled while the disk is attached).

**gxctl:**

```bash
gxctl disk rm data1
```

It asks for confirmation; the disk's data is gone afterwards. A disk
attached to a VM can't be deleted: detach it or delete the VM first. A
root disk that was created along with a VM is deleted with the VM,
unless you delete the VM with `--keep-disk` (in the web UI, **Keep disk**
in the delete dialog); a kept disk is then like any other and can be
deleted. The VM's page lists its disks and marks that one *deleted with
the VM*.

### Where a disk lives

A disk lives on a datastore. On the built-in `local`, or another
datastore each node has its own of, it lives on one node: the node of
the first VM that uses it, and its VMs run there. On a **shared**
datastore (one filesystem every node mounts), it is tied to no node, and
its VM can run on any node that reaches the datastore; disk access then
goes over the network. The **Disks** page has a *Datastore* column once
there is more than `local`; `gxctl disk show` names it too (`local`, or
the datastore's id). The **Datastores** page, and `gxctl datastore
list`, show every datastore, the default, and how much space is free.

In a cluster, the **Disks** page's *Node* column shows a disk's node, or
`shared · <node>` for a disk on a shared datastore (the node currently
working on it).

Over the API, `node` (a node's name or id) binds a new disk on a
node-local datastore to that node when it is created; an unknown node is
refused with `400 invalid_config`, and a disk on a shared datastore
can't be bound to a node (`400 invalid_disk`).

A VM's root disk made along with the VM goes to the default datastore.
To put it on another, create the disk first with `--datastore` (or the
**Datastore** field) and create the VM from that disk.

### Disks on an LVM-thin datastore

An LVM-thin datastore keeps each disk as a raw volume in a volume group
on one node (see [LVM-thin](../operator-guide/datastores.md#lvm-thin)).
Like other node-local disks, it lives on one node and its VMs run
there. What differs:

- **Always raw.** Leave the format out (`gxctl disk create` does) and the
  disk is raw; asking for qcow2 is refused (`400 invalid_disk`, *disks on
  a block datastore (lvm, hyperstor) are raw volumes; leave format out*).
  In the web UI, the form shows **Format** as raw on such a datastore,
  and offers both **Linked** and **Full copy**.
- **Linked or full.** A linked disk (the default with `gxctl`) is a thin
  snapshot of the image on that node: quick to make, and small until
  the guest writes. A full disk is an independent copy. Either way the
  image stays as long as a linked disk uses it.
- **Grow, never shrink.** `gxctl disk resize` to a larger size works as
  usual, root partition included; a smaller size is refused.
- **Moves make full, raw copies.** Moving a disk onto an LVM-thin
  datastore, or off one, always copies it whole: **Flatten** makes no
  difference. On the volume group it becomes raw; moved off to a
  directory it stays raw.

```bash
gxctl disk create db-root --image ubuntu-24.04 --size-gib 40 --datastore thin1   # linked, raw
gxctl disk create db-data --size-gib 200 --datastore thin1
gxctl disk move data1 --datastore thin1
```

### Moving a disk

A disk can move to another datastore while no running VM uses it (stop
the VM first, or detach the disk). Its data is copied and checked, then
the old file is removed; if the move fails, the disk stays where it was.

**Web UI:** **Disks**, **Move** on the disk's row (shown when there is
more than one datastore): pick the datastore. For a linked disk,
**Flatten** makes it a full copy instead of an overlay on the image
there.

**gxctl:**

```bash
gxctl disk move data1 --datastore nfs1
gxctl disk move web-root --datastore fast --flatten
```

It waits up to five minutes and prints the disk, or that it is still
moving; follow it with `gxctl disk show`. A linked disk moved to a
shared datastore waits for the image to be copied there first.

While a disk moves, the **Disks** page shows it *· moving*, with
**Resize** and **Move** disabled. Resizing it is refused
(`409 conflict`), adding it to a VM's data disks is refused
(`400 invalid_config`), and a VM using it waits to start (condition
`DisksReady` `False`, reason `DiskNotReady`) until the move is done.

Moving needs the `editor` role in the disk's project, and the right to
use the destination datastore. It is refused:

- for a disk that is being deleted, already moving, not ready, or used
  by a running VM (`409 conflict`);
- for the datastore the disk is already on, or one that the node with
  the disk, or its VM's node, doesn't reach (`400 invalid_disk`);
- for an unknown datastore (`404 not_found`);
- when the destination has overcommit off and lacks the space
  (`507 insufficient_storage`).

Onto or off an LVM-thin datastore, a disk is always copied whole; see
[Disks on an LVM-thin datastore](#disks-on-an-lvm-thin-datastore).

Moving a disk from one node's own storage to another node's isn't
possible directly: move it to a shared datastore, then from there. A
disk on a shared datastore lets its VM move between nodes (see
[Placement on a cluster](vms.md#placement-on-a-cluster)).

### Over the API

```bash
api() { curl --cacert host.crt -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' "$@"; }
api -X POST https://host.example.org:8841/images -d '{"catalog": "ubuntu-24.04"}'
api -X POST https://host.example.org:8841/disks -d '{"name": "data1", "project": "lab", "size_gib": 50}'
api -X POST https://host.example.org:8841/disks -d '{"name": "data2", "project": "lab", "size_gib": 50, "datastore": "nfs1"}'
api -X POST https://host.example.org:8841/disks/data1/resize -d '{"size_gib": 100}'
api -X POST https://host.example.org:8841/disks/data1/move -d '{"datastore": "nfs1", "wait_secs": 300}'
api -X DELETE https://host.example.org:8841/disks/data1
```

See the [REST API](../api/rest.md) for every field.
