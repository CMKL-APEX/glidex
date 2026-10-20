# Kernel boot

A VM boots in one of two ways:

- **Firmware boot** (the default): the hypervisor runs UEFI firmware,
  and the firmware runs the boot loader on the VM's disk, which picks
  the kernel. See [Virtual machines](vms.md#firmware-or-kernel-boot).
- **Kernel boot** (direct kernel boot): the hypervisor loads a kernel,
  an optional initramfs and a kernel command line that Glidex hands it.
  There is no firmware and no boot loader.

A distro cloud image boots either way: the same root disk, the same
cloud-init seed and credential, and the same console.

## When to use it

Use kernel boot when you want to choose the kernel from outside the
guest:

- to set the kernel command line yourself, for example to debug a
  guest that doesn't boot;
- to boot a kernel the guest doesn't have, or keep a VM on a known
  kernel build;
- to boot a bare root filesystem with no partition table or boot
  loader.

Stay with firmware boot when the guest should manage its own kernel
through its boot loader, as on a physical machine. With kernel boot, a
kernel the guest installs (`apt upgrade`, say) doesn't boot unless you
turn on [following the guest's kernel](#following-the-guests-kernel).

A VM's boot mode is fixed when it is created: you can't switch a VM
between firmware and kernel boot.

## Kernel images

A *kernel image* is an entry in the shared image library holding a
kernel, an optional initramfs and a default command line. VMs name it
with `kernel`, as they name firmware with `firmware`. Like other
images, kernel images are visible to everyone, and adding and deleting
them needs the `image-admin` role. See
[Images and disks](images-and-disks.md#kernel-images).

A kernel image is never replaced: a newer kernel is a new image, and
each VM keeps the one it names until you change it.

### From a cloud image

Pulling a catalog image also takes the kernel out of it: a second
image, `<image>-kernel` (for example `ubuntu-26.04-kernel`), appears in
state *extracting* and becomes ready a few seconds after the image.
It is the kernel, initramfs and command line the image's own boot
loader would boot, with `console=` replaced by `console=ttyS0`. So the
kernel always matches the modules on the image's disk.

- A pull with a `name` names the kernel image `<name>-kernel`.
- `"with_kernel": false` skips the extraction; `"with_kernel": true`
  extracts from a URL pull too (off by default for URLs).
- To take the kernel out of an image you already have, including one
  pulled from a URL:

  ```bash
  curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
    -X POST https://host.example.org:8841/images -d '{"kernel_from": "my-image"}'
  ```

  The answer is `202` with the new kernel image (`200` with the one
  already extracting from that image).

Glidex reads the image inside a small VM without network, never on the
host. It builds that VM the first time it extracts, from pinned Debian
packages (a one-time download of about 35 MB). Extraction works on
x86_64 hosts that have Cloud Hypervisor or QEMU. An administrator can
turn it off (for a host without internet access, say); `kernel_from`
is then refused with `400 invalid_image`, and catalog pulls bring only
the disk image.

If extraction fails, only the kernel image fails, with the reason; the
disk image stays ready and firmware boot is unaffected. **Retry** in
the web UI or `gxctl image retry <name>` runs it again.

### From a URL

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X POST https://host.example.org:8841/images -d '{
    "url": "https://images.example.org/vmlinuz-6.8.0-147-generic", "sha256": "<sha256>",
    "kind": "IMAGE_KIND_KERNEL",
    "initrd_url": "https://images.example.org/initrd.img-6.8.0-147-generic", "initrd_sha256": "<sha256>",
    "cmdline": "root=LABEL=cloudimg-rootfs ro console=ttyS0"
  }'
```

The URL rules are those of any image pull: `https://`, and listed as
unverified unless every file had a `sha256`. Use the kernel the cloud
image was built with: the guest loads the kernel's modules from its own
disk. Ubuntu publishes each build's kernel and initramfs next to the
image, in `unpacked/`.

### From host files

An administrator who may use host paths can copy a kernel from files
on the host into the library:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X POST https://host.example.org:8841/images -d '{
    "kernel_path": "/srv/kernels/vmlinuz-custom", "initrd_path": "/srv/kernels/initrd-custom",
    "cmdline": "root=/dev/vda1 ro console=ttyS0", "name": "custom-kernel"
  }'
```

VMs then boot it like any kernel image; their users need no access to
host paths.

### What a kernel image may hold

- An x86_64 bzImage (64-bit), an ELF `vmlinux` for x86_64 or aarch64,
  or an arm64 `Image`. Anything else fails the image.
- At most 128 MiB for the kernel and 1 GiB for the initramfs, unless an
  administrator changed the limits.
- A `cmdline` of at most 2047 bytes, without control characters.

The kernel's release (`7.0.0-34-generic`) is read from its header when
the format carries one (a bzImage does).

### Listing and deleting

**Web UI:** **Images**, *Kernels* (shown once there is one): each
kernel image with its release, whether it has an initramfs, its
command line, its status (*Extracting…*, *Ready*), its source and the
VMs that boot it.

**gxctl:**

```
gxctl> image list            # kernel images show as "kernel <release>"
gxctl> image rm ubuntu-26.04-kernel
```

The source column says where a kernel came from: its URL, the host
file, or `extracted: <partition>:<path>` (`extracting` until done).

A kernel image can't be deleted while a VM boots it, or keeps it to go
back to (see [Kernel versions](#kernel-versions)): the delete is
refused with `409 conflict`, naming the VMs. The image's `used_by_vms`
lists them.

## Create a VM with kernel boot

Name a kernel image with `kernel` and the root disk as for firmware
boot (`image` or `root_disk`):

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X POST https://host.example.org:8841/vms -d '{
    "name": "web-1", "vcpu_count": 2, "mem_size_mib": 2048,
    "image": "ubuntu-26.04", "kernel": "ubuntu-26.04-kernel",
    "credential": "alice", "networks": [{"network": "lab"}]
  }'
```

- The VM needs no firmware image. Giving both a kernel and a firmware
  (`firmware` or `firmware_path`) is refused with `400 invalid_config`.
- The kernel image must be ready and of the root image's architecture.
  Using it needs read access to the image, as for firmware.
- The VM gets the kernel image's command line as its `kernel_args`,
  unless you give them.
- Glidex generates the cloud-init seed as for firmware boot (the
  hostname, the root partition growth and the credential), so
  `credential` works the same. See [Guest credentials](credentials.md).
- On a cluster, the VM's node gets its own copy of the kernel image.
  Until it has one, the VM waits with the condition `DisksReady`
  `False` (*kernel &lt;name&gt; has not been copied to this node yet*).

<!-- TODO(verify): the web UI's create form and gxctl create offer only host paths for kernel boot (no kernel image picker, no initramfs field); update this page when they gain one. -->
The web UI's **Direct kernel boot** and `gxctl create` (answer `none`
at the firmware prompt) take host paths only: see
[Host paths](#host-paths). To boot a kernel image, use the API.

### Host paths

An administrator who may use host paths can boot files on the host
directly instead of a kernel image:

| Field | What it is |
|---|---|
| `kernel_image_path` | The kernel. |
| `initrd_path` | Its initramfs, optional; distro kernels need one. Only with `kernel_image_path`. |
| `rootfs_path` | A root filesystem or disk image, used as is. Over the API, `image` and `root_disk` work too. |

In the web UI: **Boot Mode** **Direct kernel boot**, then **Kernel
Image Path**, **Root Filesystem Path** and **Kernel Arguments**. In
`gxctl create`, answer `none` at *UEFI firmware*, then give the
*Kernel image path*, *Root filesystem path* and *Kernel arguments*.

Host paths can't be changed after create. A VM with a host-path kernel
and a host-path root filesystem gets no generated seed, so it can't
have a credential; one with `image` or `root_disk` gets the seed.
`generate_seed` (`true` or `false`) overrides this at create, and can't
be combined with `cloud_init_path`.

## The kernel command line

`kernel_args` is the kernel command line:

- **Default:** the kernel image's command line; with host paths, or a
  kernel image without one, `console=ttyS0 root=/dev/vda reboot=k panic=1`.
- **Limits:** at most 2047 bytes, no control characters
  (`400 invalid_config`).
- **The root:** a cloud image's root is on a partition, so
  `root=/dev/vda` doesn't find it. A cloud image needs its own root, for
  example `root=LABEL=cloudimg-rootfs ro` for Ubuntu; an extracted
  kernel's command line has it. A VM with a managed root disk whose
  arguments have no `root=` is created with the warning *kernel_args has
  no root=; the kernel may not find the root disk*.

Change them with `PATCH /vms/{id}` (`kernel_args`); they apply at the
next start. Changing the kernel doesn't change them.

## The console

With kernel boot, the last `console=` in `kernel_args` picks the
device Glidex connects the [console](console.md) to:

| Last `console=` | Guest device | Cloud Hypervisor | QEMU |
|---|---|---|---|
| `hvc…` (for example `hvc0`) | the virtio console | virtio console, no serial port | virtio console, no serial port |
| anything else (`ttyS0`), or none | the serial port | serial port | serial port |

Both hypervisors behave the same. Firmware boot always uses the serial
port. The default arguments and extracted kernels say `console=ttyS0`,
so the console shows the kernel from its first line, and a panic is
visible. The virtio console only starts once the kernel's virtio driver
is up: put `console=hvc0` last only if you want it.

A Cloud Hypervisor VM created by an earlier version of Glidex keeps the
`console=hvc0` it was created with, and so the virtio console.

## Following the guest's kernel

A kernel-booted VM boots the kernel Glidex hands the hypervisor, so a
kernel the guest installs doesn't boot by itself. To boot it anyway,
turn on `kernel_from_disk`:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X PATCH https://host.example.org:8841/vms/<id> \
  -d '{"change": {"kernel_from_disk": true}, "update_mask": "kernel_from_disk"}'
```

It is also a field of `POST /vms`, and needs a kernel image (`kernel`)
and a managed root disk (`image` or `root_disk`): `400 invalid_config`
otherwise. It applies at the next start.

Before each start, Glidex then takes the kernel out of the VM's root
disk again if the disk changed since the last time (its size or
modification time), the same way it extracts from a cloud image. A VM
that was only stopped and started pays nothing.

- The VM shows `booted_kernel`, the kernel image it boots, and
  `previous_kernel`, the one before. Its `kernel` stays what you chose:
  the kernel it starts from, and its fallback.
- A new kernel logs the event `KernelUpdated` (old → new release).
- If extraction fails, the VM boots the kernel it has. Its condition
  `KernelExtracted` is `False` with the reason, the event
  `KernelExtractFailed` is logged, and the next start tries again. Only
  a VM with no kernel at all fails to start (`KernelNotExtracted`).
- The VM keeps its `kernel_args`; the new kernel's command line from
  the disk isn't applied.
- Extraction takes a few seconds on each start after a change, during
  which the disk is busy.

**To go back** to an earlier kernel, set `kernel`, for example to the
`previous_kernel`:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X PATCH https://host.example.org:8841/vms/<id> \
  -d '{"change": {"kernel": "<previous_kernel>"}, "update_mask": "kernel"}'
```

Changing `kernel` without `kernel_from_disk` in the mask turns
following off, so the VM stays on the kernel you set. Put both in the
mask to keep following on.

### Extract once

To take the guest's kernel without following it, extract it once from
a stopped VM:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" \
  -X POST https://host.example.org:8841/vms/<id>/extract-kernel
```

The answer is `202` with a new kernel image, *extracting*. Wait for it
to be ready (`GET /images/<image-id>`), then set it as the VM's
`kernel` with `PATCH`.

- It needs the `editor` role in the VM's project.
- The VM must be stopped (`409 conflict` otherwise), boot a kernel
  image, and have a managed root disk (`400 invalid_config` otherwise).
- On a cluster, send it to a server on a node that reaches the VM's
  root disk; otherwise it is refused with `409`, naming the disk. A disk
  being moved to another datastore is `409` too.

## Kernel versions

Kernels taken from a VM's own disk are kernel images the VM owns: their
`owner` is the VM's id, they are named `<vm>-kernel-<id prefix>`, and
they are never marked verified (the guest writes that disk).

- A launch-time extraction that finds a kernel and initramfs already
  stored (the same files) uses that image instead of keeping a copy. A
  VM following an unchanged guest boots the cloud image's own kernel
  image. `extract-kernel` always keeps its new image.
- A VM uses a kernel image while its `kernel` names it, or it boots it
  or keeps it as `previous_kernel`. Such an image can't be deleted, and
  on a cluster it is copied to the VM's node.
- A VM-owned kernel image is deleted by itself once no VM uses it (at
  least five minutes after it was made).

So VMs of the same image can run different kernels side by side, each
kept while a VM needs it.

## Change a VM's kernel

`kernel` can be changed on a stopped or running VM that boots a kernel
image; it applies at the next start:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X PATCH https://host.example.org:8841/vms/<id> \
  -d '{"change": {"kernel": "ubuntu-26.04-kernel"}, "update_mask": "kernel"}'
```

<!-- TODO(verify): a kernel or kernel_from_disk change isn't counted in restart_required (launch_relevant leaves them out); confirm whether the VM should show RestartRequired. -->
The new kernel must be a ready kernel image of the same architecture.
A firmware VM can't change to a kernel (`400 invalid_config`), and
`kernel_image_path`, `initrd_path` and `generate_seed` can't be
changed.

## Limits

- Kernel boot has no UEFI firmware, so no Secure Boot and no UEFI
  variables.
- Kernel extraction runs on x86_64 hosts only. Elsewhere, add kernel
  images from a URL or host files.
- The web UI and `gxctl` don't yet pick kernel images, show a VM's
  kernel, or extract kernels; use the API for those.
- With a host-path kernel, the file must exist on the VM's node; Glidex
  doesn't copy it.

## API

| Call | What it does |
|---|---|
| `POST /images` with `catalog` (and `with_kernel`) | Pull a cloud image and, by default, its kernel. |
| `POST /images` with `kernel_from` | Extract the kernel of a disk image. |
| `POST /images` with `kind: IMAGE_KIND_KERNEL`, `url`, `initrd_url`, `cmdline` | Download a kernel image. |
| `POST /images` with `kernel_path`, `initrd_path` | Copy a kernel image from host files. |
| `POST /vms` with `kernel`, `kernel_args`, `kernel_from_disk` | Create a kernel-boot VM. |
| `PATCH /vms/{id}` with `kernel`, `kernel_args`, `kernel_from_disk` | Change its kernel, command line or following. |
| `POST /vms/{id}/extract-kernel` | Extract the kernel on a stopped VM's disk. |

A VM shows `boot` (`BOOT_MODE_FIRMWARE` or `BOOT_MODE_KERNEL`),
`kernel`, `kernel_from_disk`, `booted_kernel` and `previous_kernel`. A
kernel image has `kind` `IMAGE_KIND_KERNEL`, its `kernel` (`release`,
`cmdline`, `initrd`), and `for_image` or `owner`. See
[REST API](../api/rest.md) for every field.
