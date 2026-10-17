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
- **Format**: `qcow2` (default) or `raw`.
- **Clone** (from an image): **Linked** or **Full copy**.

**gxctl:**

```bash
gxctl disk create data1 --size-gib 50                    # blank qcow2
gxctl disk create web-root --image ubuntu-24.04 --size-gib 20
gxctl disk create web-root2 --image ubuntu-24.04 --full --raw
```

When a disk is made from an image and given more space than the image
has, Glidex extends its root partition to fill it (the guest grows the
filesystem when it boots). `--no-extend` leaves the partition as it is.

If the image is still downloading, the disk waits for it.

### Clone modes

- **Linked** (default for qcow2): a thin overlay on the image. Fast and
  small, but the image must stay as long as the disk exists.
- **Full**: an independent copy. Needed for `raw` disks.

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
  `DELETE /vms/<vm-id>/disks/<disk>` detaches it.

<!-- TODO(verify): neither gxctl nor the web UI has an attach/detach command for a data disk on an existing VM; only POST/DELETE /vms/{id}/disks. Update this section if one is added. -->

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
unless you delete the VM with `--keep-disk`.

In a cluster, a disk lives on one node. It is bound to the node of the
first VM that uses it, and its VMs run there.

### Over the API

```bash
api() { curl --cacert host.crt -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' "$@"; }
api -X POST https://host.example.org:8841/images -d '{"catalog": "ubuntu-24.04"}'
api -X POST https://host.example.org:8841/disks -d '{"name": "data1", "project": "lab", "size_gib": 50}'
api -X POST https://host.example.org:8841/disks/data1/resize -d '{"size_gib": 100}'
api -X DELETE https://host.example.org:8841/disks/data1
```

See the [REST API](../api/rest.md) for every field.
