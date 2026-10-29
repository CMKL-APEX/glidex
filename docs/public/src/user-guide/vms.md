# Virtual machines

A VM belongs to one project. You create it stopped (or ask for it to
start at once), then start, pause, stop and delete it. Each of these
changes the VM's *desired state*; the control plane then works until
the VM gets there. See [Concepts](../concepts.md#virtual-machines) for
the model.

In the web UI, the **VMs** page lists the VMs of the selected project.
Click a VM for its details, actions, usage and events.

In `gxctl`, commands take a VM's name or id. Names are unique within a
project; use `--project <p>` (or `project use <p>`) when the same name
exists in two projects you can see.

```
gxctl> list                       # the VMs: id, name, project, state, node, vCPUs, memory, hypervisor
gxctl> get my-vm                  # one VM: state, last exit, NICs and their addresses
```

## Before you start

A VM that boots a cloud image needs:

- an **image** (or an existing disk made from one), and a **firmware
  image** for the hypervisor. See [Images and disks](images-and-disks.md);
- a **guest credential**, or the VM has no way to log in. See
  [Guest credentials](credentials.md);
- a **network** your project may use, or the VM has no NIC. See
  [Networks](networks.md).

You need the `editor` role in the project to create, change and delete
VMs, and `operator` to start, stop and pause them and to open their
consoles. See [Projects](projects.md).

## Create a VM

### Web UI

On the **VMs** page, click **+ Create VM** and fill in the form:

| Field | What it is |
|---|---|
| **VM Name** | The VM's name in the project. |
| **Hypervisor Backend** | Cloud Hypervisor (the default), QEMU or Pylon. Fixed once created. |
| **vCPU Count**, **Memory (MiB)** | Defaults 1 vCPU and 512 MiB. On Pylon with **Elastic CPU scaling**, the vCPU count is the maximum and can't be edited. |
| **Elastic CPU scaling** | Min vCPUs, max vCPUs and weight. See [Elastic CPU](#elastic-cpu). |
| **Placement** | Clusters only: **Automatic**, or a node. See [Placement on a cluster](#placement-on-a-cluster). |
| **Boot Mode** | **UEFI firmware (distro cloud image)** or **Direct kernel boot**. |
| **UEFI Firmware** | The firmware image to boot through; the newest one for the hypervisor is preselected. |
| **Boot Disk** | **New disk from an image**, **Existing disk** (an unattached disk of the project), or **Disk image file path**. |
| **Image**, **Root disk (GiB)** | For a new disk from an image: a linked disk is created for this VM, its root partition extended, and deleted with the VM. |
| **Login Credential** | A credential of the project, provisioned by cloud-init on first boot; or none. |
| **Service Account** | Optional: the identity software in the VM gets glidex tokens for. See [Workload identity](#workload-identity). |
| **Networks** | One NIC per ticked network, in that order. A network named `default` is ticked for you. |
| **GPUs (optional)** | Right after the size. A count from each GPU pool your project may use (it sizes the VM; see [GPUs](#gpus)), and the node's own GPUs to tick: those ready for passthrough and held by no other VM can be ticked, the others say why not. In a cluster, the node's GPUs appear once a node is chosen under **Placement**. |
| **Device Pools (optional)** | How many devices to take from each other pool (NICs, NVMe…) your project may use. |
| **VFIO PCI Devices (optional)** | Under **Advanced options**: other host PCI devices to pass through, by path. See [Devices](#pci-devices). |
| **If It Crashes** | **Restart it (with backoff)** or **Leave it stopped**. |
| **After a Host Reboot** | **Start it again if it was running** or **Leave it stopped**. |
| **Start it now** | Start the VM as soon as it is created. |

With **Direct kernel boot**, the form asks for a **Kernel** instead of
**UEFI Firmware**: a kernel image (the one taken from the chosen image
comes first and is the only one preselected; **Extract it from the
image** makes it if there is none. Another image's kernel usually won't
boot this one: its initramfs waits for that image's root disk),
or, for administrators, **Kernel files on the host** (kernel and
initramfs paths, see [Host paths](#host-paths)). **Kernel Arguments**
default to the kernel image's own command line. **Follow the guest's
kernel** boots the kernel the guest installs (see
[Kernel boot](kernel-boot.md)). The boot disk and login credential work
as with firmware.

On the VM's page, **Kernel** shows the kernel image (and, when following
the guest, the kernel it boots and the one before), lets you pick
another one with **Use from the next start**, and **Extract from its
disk now** while the VM is stopped.

### gxctl

`create` asks for each setting in turn; press Enter to take the default
in brackets.

```
gxctl> create
VM name: web-1
GPUs (count[:pool][:nvlink], e.g. 4:a100; sizes the VM from the pool; optional):
vCPU count [1]: 2
Memory (MiB) [512]: 2048
Hypervisor [cloudhypervisor/qemu/pylon] (default: cloudhypervisor):
Configure elastic CPU scaling (min/max vCPUs, weight)? [y/N] [n]:
UEFI firmware [cloudhv-edk2, 'kernel' for direct kernel boot, or a host path] [cloudhv-edk2]:
Boot disk from [image/disk/path] [image]:
Image (ubuntu-26.04) [ubuntu-26.04]:
Root disk size in GiB [10]: 20
Data disks (optional, comma-separated disk names):
cloud-init seed image (optional, default: auto-generated):
Login credential (one of: alice; 'none' for no login) [alice]:
Networks (comma-separated; available: default, lab; 'none' for no NIC) [default]:
VFIO PCI devices (comma-separated, e.g. /sys/bus/pci/devices/0000:41:00.0):
Device pools (comma-separated pool[:count], e.g. a100:2; optional):
```

For a direct kernel boot, answer `kernel` at the firmware prompt: gxctl
then asks for a kernel image (the one from the boot image is the
default), whether to follow the guest's kernel, and offers the kernel's
command line as the default arguments. Later, `kernel web-1` shows the
VM's kernel; `kernel web-1 --set <image>` and `--follow on|off` change
it from the next start, and `--extract` takes the kernel on its disk.

The VM is created stopped; start it with `start web-1`. `gxctl create`
doesn't ask for a node, a restart policy or a host-boot policy: they
take their defaults (use the web UI or the API to set them).

### API

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X POST https://host.example.org:8841/vms -d '{
    "name": "web-1", "vcpu_count": 2, "mem_size_mib": 2048,
    "image": "ubuntu-26.04", "root_disk_size_gib": 20,
    "credential": "alice", "networks": [{"network": "lab"}],
    "power": "POWER_STATE_RUNNING"
  }'
```

`name`, `vcpu_count` and `mem_size_mib` are required (with `gpus`, only
`name`: see [GPUs](#gpus)). Values from a
fixed list take their full names, as in the table below
(`POWER_STATE_RUNNING`, not `running`); a short name is refused with
`422` (see [Proto JSON](../api/rest.md#proto-json)). See
[REST API](../api/rest.md#vms) for every field.

## VM settings

| Setting | API field | Notes |
|---|---|---|
| Name | `name` | Unique in the project. The generated cloud-init seed makes it the guest's hostname. |
| Project | `project` | Id or name; default: your default project. |
| vCPUs | `vcpu_count` | At least 1. Changeable; applies at the next start, or live within an elastic VM's range. On an elastic Pylon VM it is always `max_vcpus`. See [Elastic CPU](#elastic-cpu). |
| Elastic vCPUs | `min_vcpus`, `max_vcpus` | `min_vcpus` ≤ `vcpu_count` ≤ `max_vcpus`. Absent: both are `vcpu_count` (a fixed size). See [Elastic CPU](#elastic-cpu). |
| CPU weight | `cpu_weight` | 1 to 10000, default 100: the VM's share of contended host CPU. |
| Memory | `mem_size_mib` | In MiB, at least 1. Changeable; applies at the next start. |
| Hypervisor | `hypervisor` | `HYPERVISOR_CLOUD_HYPERVISOR` (default), `HYPERVISOR_QEMU` or `HYPERVISOR_PYLON`. Fixed. |
| Boot disk | `image`, `root_disk` or `rootfs_path` | Exactly one. Fixed. See below. |
| Root disk size | `root_disk_size_gib` | With `image` only. Default: the image's size, at least 10 GiB. |
| Firmware | `firmware` | A firmware image (name or id). Fixed. |
| Kernel boot | `kernel`, or `kernel_image_path` and `initrd_path`; `kernel_args` | Instead of firmware (not both). A kernel image (`kernel`) can be changed and applies at the next start; host paths are fixed; arguments changeable. |
| Follow the guest's kernel | `kernel_from_disk` | With `kernel` and `image` or `root_disk`: boot the kernel the guest installed. See [Following the guest's kernel](kernel-boot.md#following-the-guests-kernel). Changeable; applies at the next start. |
| Generate seed | `generate_seed` | Overrides whether Glidex generates the cloud-init seed; not with `cloud_init_path`. Fixed. |
| Credential | `credential` | A credential of the project. Needs the generated cloud-init seed: firmware boot, or kernel boot of a kernel image or a managed root disk, without `cloud_init_path`. |
| Networks | `networks` | Up to 8 NICs, in order. Changeable; applies at the next start. |
| Data disks | `data_disks` | Disks of the project, attached after the boot disk. Changeable; applies at the next start. |
| PCI devices | `vfio_devices` | Changeable while the VM runs. |
| Restart policy | `restart_policy` | `RESTART_POLICY_ON_FAILURE` (default) or `RESTART_POLICY_NEVER`. Changeable. |
| On host boot | `on_host_boot` | `HOST_BOOT_POLICY_RESUME` (default) or `HOST_BOOT_POLICY_STOP`. Changeable. |
| Stop grace | `stop_grace_secs` | Seconds a stop waits after the power button; 0 (default) stops at once; at most 300. Changeable. |
| Initial power | `power` | `POWER_STATE_STOPPED` (default), `POWER_STATE_RUNNING` or `POWER_STATE_PAUSED`. |
| Node | `node` | Clusters: pin the VM to a node (name or id). Fixed, except by [placing the VM again](#placement-on-a-cluster). |
| Hugepages | `hugepages` | Back guest memory with hugepages. Changeable; applies at the next start. |
| Hugepage size | `hugepage_size` | `2M` or `1G`, with `hugepages`; the memory must be a whole number of pages. Default: the host's default size (usually 2M). Changeable; applies at the next start. |
| NUMA policy | `numa_policy` | `NUMA_POLICY_NONE` (default), `NUMA_POLICY_PREFERRED` or `NUMA_POLICY_STRICT`: keep the VM's memory, CPUs and PCI devices on one host NUMA node; `NUMA_POLICY_ALIGNED`: across exactly its devices' NUMA nodes. See [NUMA and dedicated CPUs](#numa-and-dedicated-cpus). Changeable; applies at the next start. |
| GPUs | `gpus` | `{"count": 4, "pool": "a100"}`: that many GPUs, NUMA-aligned, the VM sized from the pool unless you size it. See [GPUs](#gpus). Fixed: change `devices` and the sizing instead. |
| Device pools | `devices` | `[{"pool": "a100", "count": 1, "same_switch": false}]`: devices the scheduler picks from a pool your project may use; shown as `pool_devices`. See [Devices from a pool](#devices-from-a-pool). Changeable; applies at the next start. |
| CPU policy | `cpu_policy` | `CPU_POLICY_SHARED` (default) or `CPU_POLICY_DEDICATED`: each vCPU on a host CPU of its own. Changeable; applies at the next start. |

### Boot disk

Give exactly one of:

- `image`: Glidex creates a linked root disk from the image for this VM
  (named `<vm>-root`) on the default
  [datastore](../operator-guide/datastores.md), grows it to
  `root_disk_size_gib`, and extends the guest's root partition at first
  boot. The disk belongs to the VM and is deleted with it, unless you
  delete with `--keep-disk`. To put it on another datastore, create the
  disk first (`gxctl disk create … --datastore`) and give it as
  `root_disk`.
- `root_disk`: an existing, unattached disk of the project (name or
  id). It is not deleted with the VM.
- `rootfs_path`: a disk image file on the host, used as is.

The image (or the root disk's image) and the firmware must be for the
same CPU architecture.

### Firmware or kernel boot

A VM with an `image` or `root_disk` boots through UEFI firmware: the
newest firmware image for its hypervisor (and architecture), unless you
name one with `firmware`. If there is none, the create fails and tells
you which one to pull (`cloudhv-edk2` for Cloud Hypervisor, `ovmf` for
QEMU).

For a direct kernel boot, name a kernel image with `kernel` instead.
The VM then needs no firmware image, gets the kernel image's command line
as its default `kernel_args`, and works with `image`, `root_disk` and a
login credential exactly like a firmware boot: glidex generates the same
cloud-init seed. A kernel image comes from a URL (`POST /images` with
`"kind": "IMAGE_KIND_KERNEL"`, `initrd_url` and `cmdline`), or an
administrator copies one from host files (`kernel_path`, `initrd_path`).
It should be the kernel the cloud image was built with: its modules are
on the image's disk.

**Following the guest's kernel.** With `"kernel_from_disk": true` (a
VM booting a kernel image from a managed disk), glidex takes the kernel
out of the VM's disk again before a start whenever the disk changed, so
a kernel the guest installed (`apt upgrade`) boots next time. The VM
shows `booted_kernel` and `previous_kernel`; if extraction fails, it
boots the kernel it has and says so (`KernelExtracted`). To go back,
set `kernel` (to the `previous_kernel`, say), which turns following off.
`POST /vms/<id>/extract-kernel` does one extraction on demand, for a
stopped VM, as a new kernel image you can then set as `kernel`.

An administrator can also give host paths: `kernel_image_path` (and a
root filesystem in `rootfs_path`), plus `initrd_path` if the kernel
needs an initramfs (distro kernels do). With host paths, or a kernel
image without a command line, the default kernel arguments are
`console=ttyS0 root=/dev/vda reboot=k panic=1` on both hypervisors. The
console is on the serial port, unless the last `console=` in the
arguments names `hvc…` (such as `hvc0`), which selects the virtio
console. A distro cloud image needs its own root, e.g.
`console=ttyS0 root=LABEL=cloudimg-rootfs ro` for Ubuntu. See
[Kernel boot](kernel-boot.md) for kernel images, the command line, the
console and following the guest's kernel.

### Host paths

`rootfs_path`, `kernel_image_path`, `initrd_path`, `cloud_init_path`
and `firmware_path` name files on the host. Using them needs the
`system-admin` role, and the files must be readable by the service
user the control plane runs as. Most users should use images, disks and
firmware images instead. A path under `/tmp` or `/var/tmp` is refused:
VMs have a private `/tmp`.

### Credential and cloud-init

Without `cloud_init_path`, Glidex generates a cloud-init seed for a VM
that boots through firmware, or that boots a kernel image or from
`image` or `root_disk` ([kernel boot](kernel-boot.md)): its hostname, the root partition grow, and,
if you name a `credential`, that user with its password and SSH keys.
cloud-init applies it on the VM's first boot. Without a credential the
guest has no login. See [Guest credentials](credentials.md).

`cloud_init_path` replaces the generated seed with your own (a host
path, see above); a `credential` can't be combined with it.

### Workload identity

Software in a VM can prove which VM it is, and act as a **service
account**, without any secret stored in the guest. It asks the metadata
endpoint at `169.254.169.254`, which only answers from inside the VM,
with the header `Metadata-Flavor: Glidex`:

```bash
H='Metadata-Flavor: Glidex'
curl -s -H "$H" http://169.254.169.254/v1/instance          # which VM this is
curl -s -H "$H" 'http://169.254.169.254/v1/identity?audience=sts.amazonaws.com'
curl -s -H "$H" http://169.254.169.254/v1/glidex/token      # needs a service account
```

- **Identity tokens** are short-lived JWTs (15 minutes; ask for 10 to 60
  with `&ttl=<seconds>`) for the audience you name, signed by your
  directory. Their subject is the VM's SPIFFE ID,
  `spiffe://<trust domain>/tenant/<t>/project/<p>/vm/<id>`. Outside
  services that trust your directory's issuer (AWS, Vault, Kubernetes, or
  glidex itself) can verify them against its published keys.
- **Glidex tokens** act as the VM's **service account**, for 15 minutes,
  with that account's roles. Give a VM one in the create form, on its
  page, with `gxctl service-account <vm> --set <account>`, or with
  `"service_account"` in `POST /vms`. That needs the right to manage the
  account's project's service tokens (project owners), and needs no
  restart: the next token is for the new account. The endpoint hands out
  the same token until 80% of its life has passed, then a fresh one, so
  ask again whenever you need one.
- An identity token addressed to your cluster can also be exchanged for a
  glidex token at `POST /auth/exchange` with `federation=<tenant>/vm`.
- Every process in the guest can ask, as on any cloud. Requests through a
  proxy in the guest (`Forwarded`, `X-Forwarded-For`) are refused.

The endpoint serves VMs on single-host NAT networks, on any node of a
cluster; OVN networks and isolated or bridged networks come later. A NAT
network's guests reach the host's own API only if the network allows it
([The host from a NAT network](networks.md#the-host-from-a-nat-network)).

### Networks

Each entry of `networks` adds one NIC, in order (at most 8):

```json
"networks": [
  {"network": "lab"},
  {"network": "storage", "mac": "02:00:00:00:10:01", "queue_pairs": 2}
]
```

`network` is a network's name or id that your project may use. `mac` is
optional: by default each NIC gets a stable MAC derived from the VM's
id. `queue_pairs` (1 to 8) is optional. The address each NIC got shows
in the VM's details. See [Networks](networks.md).

### Data disks

`data_disks` lists disks of the project (names or ids), attached in
order after the boot disk. A disk is attached to one VM at a time.

To attach or detach a disk later:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X POST https://host.example.org:8841/vms/<id>/disks -d '{"disk": "data-1"}'
curl -H "Authorization: Bearer $GLIDEX_TOKEN" \
  -X DELETE https://host.example.org:8841/vms/<id>/disks/data-1
```

On a running VM the change applies at the next start. See
[Images and disks](images-and-disks.md).

### PCI devices

`vfio_devices` passes host PCI devices (a GPU, a NIC) through to the
guest. Give each as `/sys/bus/pci/devices/0000:41:00.0` or the bare
address `0000:41:00.0`. Using one needs a grant from an administrator.
See [PCI passthrough](../operator-guide/pci-passthrough.md).

A device is used by one VM at a time on its host, and it takes its
IOMMU group with it (devices that the host can only pass through
together). Asking for a device another VM on the host holds is refused
with `409 conflict`:

- `PCI device 0000:41:00.0 is used by VM other-vm`: that VM has it, or
  its running instance still does.
- `PCI device 0000:41:00.1 shares IOMMU group 31 with 0000:41:00.0, used
  by VM other-vm`: another device of the group is that VM's. A VM that
  already holds a device of the group may add the others.

```
gxctl> pci                                          # host PCI devices
gxctl> attach-device my-vm /sys/bus/pci/devices/0000:41:00.0
gxctl> detach-device my-vm /sys/bus/pci/devices/0000:41:00.0
```

On a running VM, devices are hot-plugged. On a paused VM, the change
waits until it runs again.

#### Devices from a pool

Instead of naming devices, ask for a number of them from a **device
pool** your project may use; Glidex picks which. To see the pools and
how many devices are free:

```
gxctl> pci pools --project lab        # pool, class, free
```

In the web UI's create form, **Device Pools (optional)** lists them with
their free counts: set how many devices to take from each, and tick
**Under one PCIe switch** to keep several together (for GPUs that talk
to each other). `gxctl create` asks for them as `a100:2` or
`a100:2:switch`. Over the API:

```json
"devices": [{"pool": "a100", "count": 2, "same_switch": true}]
```

The picked devices are on the NUMA node of the VM's other devices when
they can be, and show as `pool_devices` (`gxctl get <vm>` and the
VM's page list each request with what it got). The VM gets them at its
next start and keeps them until the request changes or the VM is
deleted. Asking needs a grant for the pool:

- `no device pool a100` (`400`): no such pool for this project.
- `forbidden` (`403`): the project may not use the pool.
- `quota_exceeded` with resource `devices.a100`: the project's quota
  for the pool is used up (see [Quotas](projects.md#quotas)).
  `tenant_quota_exceeded` with `tenant_pool.<name>`: a tenant pool's
  maximum is reached.
- `pool a100: 1 of 2 free`: not enough free devices. On a cluster the
  VM waits for them; on a single host it is refused (`409`).

A pool your tenant set up for some of its projects (a **tenant pool**)
is asked for by its own name; the VM shows the cluster pool it draws
from and `tenant_pool`. Devices from pools are metered
(`device.alloc.<pool>`, see [Usage](usage.md)).

#### GPUs

For GPUs there's a shorter way: ask for a count, and the pool places
and sizes the VM.

```json
{"name": "train-1", "image": "ubuntu-24.04", "credential": "alice",
 "gpus": {"count": 4, "pool": "a100"}}
```

`gpus` is filled in when the VM is created:

| Field | From `gpus` | When you give it yourself |
|---|---|---|
| `devices` | `{"pool": "a100", "count": 4}` | Refused if `devices` asks the same pool. |
| `numa_policy` | `NUMA_POLICY_ALIGNED` (see [NUMA and dedicated CPUs](#numa-and-dedicated-cpus)) | Yours is kept. |
| `vcpu_count`, `mem_size_mib` | The pool's per-device vCPUs and memory, times the count | Yours are kept. |
| `hugepages`, `cpu_policy` | The pool's, when the operator set them | Yours are kept. |

- **The pool:** a cluster pool or a tenant pool by name. Without
  `pool`, the project's only GPU pool is used.
- **Sizing:** what each GPU brings is set by the operator. The web UI
  shows it next to the pool (*14 vCPUs and 56 GiB each*), and
  `GET /pci-pools?project=<p>` as `per_device_vcpus` and
  `per_device_memory_mib`. A pool without it needs `vcpu_count` and
  `mem_size_mib`.
- **Quotas and metering** are those of the pool's devices
  (`devices.a100`, `device.alloc.a100`).
- **On the VM:** it keeps `gpus` and shows what it came to: `devices`
  and `pool_devices`, the policies, the sizing, and for GPUs on several
  NUMA nodes each node's share (`numa`). `gxctl get <vm>` prints
  `GPUs: 4 from a100` and a `NUMA <i>:` line per guest NUMA node; the
  VM's page shows **NUMA layout**.
- **Changing it:** `gpus` can't be changed (`PATCH` refuses it). Change
  `devices`, `vcpu_count` and `mem_size_mib` instead; they apply at
  the next start.
- **Placement preview:** `POST /scheduler/preview` takes `gpus` too,
  and shows each node's split.

**gxctl:** `create` asks *GPUs* right after the name: `4`, `4:a100`.
With GPUs, an empty vCPU or memory answer takes the pool's sizing.

**Web UI:** the **GPUs** section, right after the VM's size:

- **From a pool:** the count of the first GPU pool is sent as `gpus`,
  and fills in the vCPU and memory fields from the pool until you change
  them by hand. A GPU pool with **Under one PCIe switch** ticked is an
  ordinary pool request instead, without the NUMA alignment or the
  sizing.
- **This node's GPUs:** each display-class device the node could pass
  through, with its address, vendor, ids and NUMA node. A ticked one is
  sent in `vfio_devices`. One not ready is greyed with the reason (for
  example *not bound to vfio-pci (driver: amdgpu)*): an operator
  prepares it on the node's page. One another VM holds is greyed with
  its name (*PCI device 0000:43:00.0 is used by VM gpu-a*), whatever
  project that VM is in; a stopped VM keeps its GPUs. Listing them needs a host role on
  that node; without one, only pools are shown. These GPUs don't size
  the VM.

Refusals (`400 invalid_config`):

| Message | Why |
|---|---|
| `gpus: count must be at least 1` | A count of 0. |
| `gpus: no device pool h100` | No such pool for this project. |
| `gpus: name the pool, one of a100, t4` | The project may use several GPU pools. |
| `gpus: this project may use no GPU pool` | No `pool` given, and no GPU pool is granted to the project. |
| `gpus: give vcpu_count and mem_size_mib, or set per_device on pool a100` | The pool has no per-device sizing. |
| `gpus and devices both ask pool a100` | Ask the pool in one place. |
| `gpus: NVLink needs a fabric …, and pool … has none` | `"nvlink": true` (`4:a100:nvlink` in gxctl). NVLink between passed-through GPUs isn't available yet; leave `nvlink` out, and it is `false`. |

A pool the project may not use is refused as with `devices`
(`403 forbidden`), and so are quotas.

## Lifecycle

| Action | Web UI | gxctl | API |
|---|---|---|---|
| Start | **Start** | `start <vm>` | `POST /vms/{id}/start` |
| Resume a paused VM | **Resume** | `start <vm>` | `POST /vms/{id}/start` |
| Pause | **Pause** | `pause <vm>` | `POST /vms/{id}/pause` |
| Graceful stop | **Shut down** | `stop <vm> --graceful [secs]` | `POST /vms/{id}/stop?graceful_timeout_secs=60` |
| Immediate stop | **Stop** | `stop <vm>` | `POST /vms/{id}/stop` |
| Delete | **Delete** | `delete <vm> [--keep-disk]` | `DELETE /vms/{id}` |
| Place a stopped or lost VM on another node (`system-admin`) | **Place on another node** | `place <vm> [--node N]` | `POST /vms/{id}/place` |

`gxctl` waits up to 60 seconds for the VM to get there (longer for a
graceful stop) and prints the state it reached; `--no-wait` returns at
once. If the VM isn't there yet, it says what it is waiting for. The
web UI follows the VM and shows **In progress** until it gets there.
The API returns at once (`202`), or, with `?wait=<secs>` (at most 300),
once the VM got there. See
[Waiting for the controller](../api/rest.md#waiting-for-the-controller).

Starting a stopped VM counts against the project's running-VM quota.

### Stop

A **graceful** stop presses the guest's power button, waits for the
guest to shut down, and stops the VM hard if it is still running after
the timeout:

```
gxctl> stop my-vm --graceful          # wait up to 60 s
gxctl> stop my-vm --graceful 120      # wait up to 120 s (at most 300)
```

The web UI's **Shut down** waits 60 seconds. **Stop** and `stop` without
`--graceful` stop the VM like pulling the plug.

The timeout you give is kept as the VM's stop grace (`stop_grace_secs`)
and used again by later stops that don't give one, including stops from
the web UI's **Stop** button. Pass `--graceful 0` (or
`graceful_timeout_secs=0`) for an immediate stop after a graceful one.
<!-- TODO(verify): a stop without a grace reuses the stored stop_grace_secs; confirm this is intended rather than a bug before publishing. -->

### Pause and resume

A paused VM keeps its memory but gets no CPU time. **Resume** in the web
UI, or `start` in `gxctl`, runs it again.

### Restart

There is no restart action: stop the VM, then start it. A reboot from
inside the guest restarts the guest and the VM stays running.
<!-- TODO(verify): guest-initiated reboot behaviour on both hypervisors (QEMU runs without -no-reboot; Cloud Hypervisor reboot handling). -->

### When the VM stops on its own

- **The guest powers off** (for example `poweroff` inside it): the
  desired state becomes stopped, and the VM stays stopped whatever its
  restart policy.
- **The hypervisor crashes**: with the restart policy **on failure**, the
  VM is started again after 10 seconds, doubling with each crash in a
  row up to 5 minutes; after 10 minutes of running the count starts
  over. With **never**, the desired state becomes stopped.
- **It fails to launch**: the VM is `failed` and Glidex retries with a
  growing delay. Its details and events say why.
- **The host reboots**: with the host-boot policy **resume**, a VM
  that was running is started again; with **stop**, it stays stopped.

Change these policies with the API:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X PATCH https://host.example.org:8841/vms/<id> \
  -d '{"change": {"restart_policy": "RESTART_POLICY_NEVER"}, "update_mask": "restart_policy"}'
```

### Change a VM

Change vCPUs, memory, networks, data disks, PCI devices, device pool
requests, kernel arguments, the kernel image and following it (`kernel`,
`kernel_from_disk`), the credential, hugepages and the policies
with
`PATCH /vms/{id}`. The body has two fields: `change` holds the new
values, and `update_mask` lists the fields to change (comma-separated;
without it, the fields present in `change` are changed). Settings
outside `change` are refused:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X PATCH https://host.example.org:8841/vms/<id> \
  -d '{"change": {"vcpu_count": 4, "mem_size_mib": 8192}, "update_mask": "vcpu_count,mem_size_mib"}'
```

`networks` and `data_disks` replace the whole list. The hypervisor, the
boot disk, the firmware and host paths can't be changed.

The web UI and `gxctl` have no edit form for most settings. Both change
the vCPUs and the elastic range (**Scale CPU / Elasticity** on the VM's
page, `gxctl scale`; see [Elastic CPU](#elastic-cpu)), and `gxctl`
changes PCI devices (`attach-device`, `detach-device`).

A change to vCPUs, memory, kernel arguments, the credential, hugepages,
policies, device pool requests, networks or data disks of a running VM
takes effect at the next start:
the web UI says **Configuration changes take effect at the next start**,
and the API sets `restart_required`. Stop and start the VM to apply it.
A new credential doesn't change logins in a guest that already booted
(cloud-init runs once).

Pass the VM's `resource_version` in an `If-Match` header to refuse the
change (`409`) if someone else changed the VM since you read it.

A change to vCPUs, memory, hugepages, policies, devices or networks of a
VM that already has a node is fitted again on that node: its hugepages,
NUMA node, dedicated CPUs and pool devices are picked again. If it no
longer fits there, the change is refused with `409` and the reason, for
example `node agent1: not enough 2M hugepages (1024 of 2048 MiB taken)`.

### Delete

```
gxctl> delete my-vm                 # asks for confirmation
gxctl> delete my-vm --keep-disk     # keep the root disk created for it
```

Deleting stops the VM if it runs, releases its NICs and their
addresses, and detaches its disks. A root disk created for the VM from
an image is deleted with it, unless you keep it (`--keep-disk`,
`?keep_disk=true`). Other disks are only detached.

## States and events

The state shows where the VM is; while it is on its way to its desired
state, `gxctl` and the web UI show both, for example `stopped → running`.

| State | Meaning |
|---|---|
| `created` | Never started. |
| `starting` | Being provisioned or launched. |
| `running` | The guest runs. |
| `paused` | The guest is paused. |
| `stopping` | A stop is in progress. |
| `stopped` | Not running. |
| `failed` | It failed to launch, or crashed and waits to be restarted. |
| `unknown` | The control plane can't tell, for example because the VM's node stopped reporting. |

In the API, states are `VM_STATE_RUNNING` and so on, and the desired
state is `desired_state`. The VM's `conditions` say what is in the way
(the `Ready` condition), and `last_exit` says how the last run ended:
stopped on request, stopped by the host, guest powered off, crashed (with
its exit code or signal), failed to launch, host rebooted, or lost.

On a cluster, a VM that boots through a firmware image, or boots a
kernel image, needs that image's copy on its node. Until the node has
fetched it, the VM waits with the condition `DisksReady` `False`
(*firmware &lt;name&gt; has not been copied to this node yet*, or
*kernel &lt;name&gt; has not been copied to this node yet*), and carries
on by itself once the copy is there.

A VM also doesn't start while one of its disks is being moved to
another datastore (`DisksReady` `False`, reason `DiskNotReady`: *disk …
is moving to datastore …*), or while another node holds the lock of a
disk on a shared datastore (reason `DiskBusy`: *disk … is in use
elsewhere (… is locked)*). See
[Volume locks](../operator-guide/datastores.md#volume-locks).

Each VM keeps a list of events: created, spec changes, launches, exits,
restarts, warnings such as a lost network port.

```
gxctl> events my-vm           # time, who, reason, message
```

In the web UI they are under **Events** on the VM's page, where new
ones appear as they happen. With the API, `GET /vms/{id}/events`.

To follow every VM, disk, image and network of your projects as they
change:

```
gxctl> watch                  # Ctrl-C stops
gxctl> watch vms
```

## Hugepages

`hugepages: true` backs the guest's memory with hugepages, which can
make memory-heavy guests faster. `hugepage_size` picks `2M` or `1G`
(default: the host's default size, usually 2M); the memory must be a
whole number of pages. Set them over the API (`POST /vms` or
`PATCH /vms/{id}` with `update_mask` `hugepages,hugepage_size`):

```json
{"name": "db", "vcpu_count": 8, "mem_size_mib": 32768, "hugepages": true, "hugepage_size": "1G", "...": "..."}
```

The VM takes its memory from the host's hugepages, which the
administrator reserves. They are counted per node and size and never
overcommitted, and a VM holds them while it exists, running or not. If
there aren't enough, the VM waits for a node (on a cluster) with a
reason like `not enough 1G hugepages (30720 of 32768 MiB taken)`, or
its start fails with the counts.

A VM with a vhost-user NIC gets 2M hugepages without asking when the
host has them to spare, and ordinary memory otherwise. The VM's
`page_kib` says what it got: 4 for ordinary memory, else the page size
in KiB.

## NUMA and dedicated CPUs

On a host with several NUMA nodes (sockets, or parts of one), a VM runs
faster when its memory, its vCPUs and its PCI devices (a GPU, a NIC) are
on the same node. Two settings ask for that:

- `numa_policy`:
  - `NUMA_POLICY_PREFERRED` puts the VM's memory on one NUMA node: the
    one with its PCI devices, else the one with the most room. When no
    node has room, the VM goes anywhere on the host, and its
    `NumaAligned` condition is `False`.
  - `NUMA_POLICY_STRICT` refuses a host where that isn't possible: PCI
    devices on two NUMA nodes, a device whose firmware gives no NUMA
    node, or no node with the memory (and CPUs) left. On a cluster the
    VM waits for a node; on a single host the create is refused (`409`).
  - `NUMA_POLICY_ALIGNED` is for VMs whose devices are on more than one
    NUMA node, say 4 GPUs on a host with 2 per node; [GPUs](#gpus) ask
    for it. The VM spans exactly its devices' NUMA nodes. Its vCPUs and
    memory are split between them by their device counts: 4 GPUs on
    nodes 5 and 7 with 16 vCPUs and 128 GiB give 8 vCPUs and 64 GiB on
    each (an uneven split gives the remainder to the first nodes).
    - Devices picked from a pool fill as few NUMA nodes as can hold them,
      so 2 GPUs go to one node when one has 2 free.
    - Each node's share must fit that node's free memory, hugepages and
      (dedicated) CPUs, or the host is refused, naming the node.
    - With devices on one node only, it is like `STRICT`. With no
      devices, it is like `PREFERRED`.
    - Every device needs a NUMA node, even on a host with one NUMA node.
      If the firmware gives none, ask for `NUMA_POLICY_PREFERRED`
      yourself (it wins over what `gpus` sets).
    - The VM shows its split as `numa` (one entry per guest node: host
      node, vCPUs, memory, devices), and the web UI shows it as **NUMA
      layout**.
- `cpu_policy: CPU_POLICY_DEDICATED` gives each vCPU a host CPU of its
  own, which no other VM uses. It implies `NUMA_POLICY_PREFERRED`. By
  default a dedicated VM takes whole cores: both hyperthreads of a core,
  so vCPUs 0 and 1 share a core and an odd count leaves a thread idle.
  The first core of each NUMA node stays with the host, and so do
  OVS-DPDK's CPUs. Shared VMs then run only on the CPUs dedicated VMs
  leave free, and running ones are moved when that changes.

A VM with shared vCPUs and a NUMA policy has its memory on the node but
its vCPUs anywhere on the host, so no node's CPUs sit idle.

Set them over the API, at creation or with `PATCH /vms/{id}`
(`update_mask` `numa_policy,cpu_policy`); the web UI and `gxctl` don't
have them yet, except through [GPUs](#gpus), which set
`NUMA_POLICY_ALIGNED`:

```json
{"name": "train", "vcpu_count": 16, "mem_size_mib": 65536,
 "numa_policy": "NUMA_POLICY_STRICT", "cpu_policy": "CPU_POLICY_DEDICATED",
 "devices": [{"pool": "a100", "count": 2}], "...": "..."}
```

The VM shows where it went: `numa_node`, and for dedicated vCPUs `cpus`
(vCPU *i* on `cpus[i]`), in `GET /vms/{id}` and the scheduler preview.
A change to its vCPUs, memory, policies, devices or networks picks
again on the same host, and applies when it next starts.

When a VM can't be placed this way, the reason says why:

| Reason | Meaning |
|---|---|
| `NUMA node 1: not enough memory (… of … MiB taken)` | That NUMA node is full. |
| `NUMA node 0: not enough free CPUs (8 dedicated vCPUs, 6 CPUs free)` | Too few CPUs left for dedicated vCPUs. |
| `its PCI devices are on NUMA nodes 0 and 1` | A strict VM's devices are split. |
| `PCI device … has no NUMA node` | A strict or aligned VM's device has no NUMA node. |
| `its devices are on 2 NUMA nodes, so it needs at least 2 vCPUs` | An aligned VM needs a vCPU on each of its nodes. |
| `NUMA nodes 5 and 7 would back its memory with different page sizes` | An aligned VM's nodes would back its memory with different page sizes. Give it `hugepages` with a `hugepage_size` each of its nodes has. |

A `PREFERRED` VM that didn't fit on one NUMA node runs anyway, with its
`NumaAligned` condition `False`.

### What the guest sees

A VM aligned over several NUMA nodes sees one guest NUMA node per host
node, in the order of `numa`:

- guest node *i* has its share of vCPUs (numbered in order: node 0's
  first) and of memory, bound to host node `numa[i].node`;
- each guest node has its own PCI root with the devices of its host
  node, so the guest sees which GPU is near which vCPUs and memory
  (`numactl -H`, `nvidia-smi topo -m`). Virtio disks and NICs stay on
  the main PCI root;
- with dedicated CPUs, vCPU *i* runs on host CPU `cpus[i]`, on its
  node. Shared vCPUs float over the host, as for one node.

The distances between guest NUMA nodes are the hypervisor's defaults,
not the host's, and a GPU and a NIC that share a PCIe switch on the
host don't share one in the guest. Both hypervisors lay the guest out
this way.

<!-- TODO(verify): large-BAR GPUs (A100: 64 GiB of 64-bit MMIO each) in multi-segment / pxb-pcie layouts; the MMIO apertures aren't sized yet (spec GV1 not done). Confirm a 4-8 GPU VM boots on both hypervisors before promising it. -->

## Elastic CPU

An elastic VM has a range of vCPUs instead of a fixed count:

| Field | Meaning |
|---|---|
| `min_vcpus` | The fewest vCPUs it runs on. At least 1; 0 on Pylon, where an idle guest can park all of them. |
| `max_vcpus` | The most it can have. At most 64 on Pylon. |
| `cpu_weight` | 1 to 10000, default 100: its share of host CPU when VMs compete (cgroup `cpu.weight`; on Pylon also `pylond`'s priority). |

A VM is elastic when `min_vcpus` is less than `max_vcpus`. Without them
it has a fixed size, `vcpu_count`. A VM with dedicated CPUs can't be
elastic.

How the range is used depends on the hypervisor:

- **Pylon.** The guest boots with all `max_vcpus` and keeps them.
  `pylond`, the host's core arbiter, parks the vCPUs the guest doesn't
  need and wakes them on demand, between `min_vcpus` and `max_vcpus`,
  in microseconds and without CPU hotplug. So an elastic Pylon VM's
  `vcpu_count` is always its `max_vcpus`: Glidex sets it, and the web UI
  and `gxctl` show it so. `lscpu` in the guest shows `max_vcpus` CPUs.
- **Cloud Hypervisor and QEMU.** The guest boots with `vcpu_count`
  vCPUs online and room for `max_vcpus`. Change `vcpu_count` within the
  range and Glidex adds or removes vCPUs while the VM runs (CPU
  hotplug); nothing changes the count on its own.

### Make a VM elastic

In the web UI, tick **Elastic CPU scaling** in the create form and set
**Min vCPUs**, **Max vCPUs** and **Weight**. On Pylon, **vCPU Count**
follows **Max vCPUs**.

For a VM that exists, click **Scale CPU / Elasticity** under its vCPU
count. Tick **Elastic CPU scaling** to give a fixed-size VM the range 1
to its vCPU count, then change it; untick it to give the VM a fixed size
again. On Pylon the dialog shows the vCPUs the guest has.

With `gxctl`, `create` asks whether to configure elastic CPU scaling
(the default is yes on Pylon), and `scale` changes a VM:

```
gxctl> scale web-1 --min 1 --max 8 --weight 200
gxctl> scale web-1 --vcpus 4
```

With the API, set `min_vcpus`, `max_vcpus` and `cpu_weight` at create
or in a [change](#change-a-vm). To make an elastic Pylon VM fixed again,
set `min_vcpus` and `max_vcpus` to the same count as `vcpu_count`: a
change of `vcpu_count` alone keeps it elastic, and its count goes back
to `max_vcpus`.

### When a change applies

| Change | Pylon | Cloud Hypervisor, QEMU |
|---|---|---|
| `vcpu_count` within the range | (Always `max_vcpus`.) | At once, on a running VM. |
| `min_vcpus`, lower `max_vcpus` | At once. | At the next start. |
| Raise `max_vcpus` | At the next start: the guest keeps the vCPUs it booted with. | At the next start. |
| `cpu_weight` | At once for `pylond`; the cgroup weight at the next start. | At the next start; the VM isn't marked for a restart. |

A change that waits for the next start sets `restart_required`, and the
web UI says **Configuration changes take effect at the next start**,
except where the table says otherwise. A change that applies at once but
can't yet, because the VM is paused, sets the `VcpuResizePending`
condition until it runs.

Quotas count `vcpu_count`, so an elastic Pylon VM counts its
`max_vcpus`.

## Placement on a cluster

On a cluster, the scheduler picks the node for a new VM when it is
created. It considers only nodes that are active, ready and not
draining, that have the hypervisor, enough vCPUs and memory left, the
PCI devices, hugepages and physical networks the VM needs, and that hold
its disks and node networks, or reach the datastores its disks are on.
Of those, it prefers the least loaded.

Hugepages are counted on their own: a VM with `hugepages` needs that
much left of the node's hugepages of its size (see
[Hugepages](#hugepages)). A VM with a NUMA or CPU policy needs a NUMA
node with room (see
[NUMA and dedicated CPUs](#numa-and-dedicated-cpus)), and a VM asking
for devices from a pool needs a node with enough free devices of it.

PCI devices are claimed per node: nodes with a device at the same
address each take one VM with it. A node where another VM holds the
device, or a device of its IOMMU group, is skipped. If every such node's
device is taken, the VM waits, and the reason names the VM holding it.
A VM pinned to a node where its device is taken is refused instead.
Nodes where a device is cordoned by an administrator, or where your
project has no grant for it, are skipped too (`PCI device … is
cordoned`, `its PCI devices are not granted to the project on this
node`).

A VM stays on its node: Glidex doesn't move it, and doesn't restart it
elsewhere if the node fails. If no node can take the VM, it is still
created and waits for one; its page says **Waiting for a node** with
each node's reason.

A VM whose disks are on shared datastores (see
[Where a disk lives](images-and-disks.md#where-a-disk-lives)) can be
placed on another node by an administrator (`system-admin`): when it is
stopped, or when its node was lost and forgotten. On the VM's page it is
**Place on another node** (**Place on a live node** for a lost VM), with
a node or **Where there is room**; with gxctl, `gxctl place <vm> [--node
N]`; over the API, `POST /vms/{id}/place` with an optional `node`. A
VM with a disk on a node's own storage can go only to that node, and
nowhere once the node is lost; a VM with a host-path root, PCI devices
or a node network can't move.

With a node, the VM is pinned there; without one, its old pin goes.
Start it again afterwards. A QEMU VM's UEFI variables stay on the old
node: it starts with fresh ones from its firmware. See [Placing a
stopped or lost VM
again](../operator-guide/datastores.md#placing-a-stopped-or-lost-vm-again)
for what is refused.

In the web UI's create form, **Placement** shows **Automatic (now:
&lt;node&gt;)** and each node with its free vCPUs and memory, or why it
can't take the VM. Pick a node to pin the VM there. With the API, set
`node` when you create the VM. To ask the scheduler without creating
anything:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X POST https://host.example.org:8841/scheduler/preview \
  -d '{"vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": "HYPERVISOR_QEMU"}'
```

The answer has the node it would pick (`pick`) and every node's fit
(`nodes`, with `refused` when it can't take the VM).

The VM's node shows in `gxctl list` and on its page. If the node stops
reporting, the page shows the VM's last known state; the VM may still be
running there.

See [Clusters](../operator-guide/clusters.md) for nodes and draining.
