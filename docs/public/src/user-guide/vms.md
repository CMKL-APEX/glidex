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
| **Hypervisor Backend** | Cloud Hypervisor (the default) or QEMU. Fixed once created. |
| **vCPU Count**, **Memory (MiB)** | Defaults 1 vCPU and 512 MiB. |
| **Placement** | Clusters only: **Automatic**, or a node. See [Placement on a cluster](#placement-on-a-cluster). |
| **Boot Mode** | **UEFI firmware (distro cloud image)** or **Direct kernel boot**. |
| **UEFI Firmware** | The firmware image to boot through; the newest one for the hypervisor is preselected. |
| **Boot Disk** | **New disk from an image**, **Existing disk** (an unattached disk of the project), or **Disk image file path**. |
| **Image**, **Root disk (GiB)** | For a new disk from an image: a linked disk is created for this VM, its root partition extended, and deleted with the VM. |
| **Login Credential** | A credential of the project, provisioned by cloud-init on first boot; or none. |
| **Networks** | One NIC per ticked network, in that order. A network named `default` is ticked for you. |
| **VFIO PCI Devices (optional)** | Host PCI devices to pass through. See [Devices](#pci-devices). |
| **If It Crashes** | **Restart it (with backoff)** or **Leave it stopped**. |
| **After a Host Reboot** | **Start it again if it was running** or **Leave it stopped**. |
| **Start it now** | Start the VM as soon as it is created. |

With **Direct kernel boot**, the form asks for a **Kernel** instead of
**UEFI Firmware**: a kernel image (the one taken from the chosen image
comes first; **Extract it from the image** makes it if there is none),
or, for administrators, **Kernel files on the host** (kernel and
initramfs paths). **Kernel Arguments** default to the kernel image's own
command line. **Follow the guest's kernel** boots the kernel the guest
installs (see [Firmware or kernel boot](#firmware-or-kernel-boot)). The
boot disk and login credential work as with firmware.

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
vCPU count [1]: 2
Memory (MiB) [512]: 2048
Hypervisor [cloudhypervisor/qemu] (default: cloudhypervisor):
UEFI firmware [cloudhv-edk2, 'kernel' for direct kernel boot, or a host path] [cloudhv-edk2]:
Boot disk from [image/disk/path] [image]:
Image (ubuntu-26.04) [ubuntu-26.04]:
Root disk size in GiB [10]: 20
Data disks (optional, comma-separated disk names):
cloud-init seed image (optional, default: auto-generated):
Login credential (one of: alice; 'none' for no login) [alice]:
Networks (comma-separated; available: default, lab; 'none' for no NIC) [default]:
VFIO PCI devices (comma-separated, e.g. /sys/bus/pci/devices/0000:41:00.0):
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
    "power": "running"
  }'
```

`name`, `vcpu_count` and `mem_size_mib` are required. See
[REST API](../api/rest.md#vms) for every field.

## VM settings

| Setting | API field | Notes |
|---|---|---|
| Name | `name` | Unique in the project. The generated cloud-init seed makes it the guest's hostname. |
| Project | `project` | Id or name; default: your default project. |
| vCPUs | `vcpu_count` | At least 1. Changeable; applies at the next start. |
| Memory | `mem_size_mib` | In MiB, at least 1. Changeable; applies at the next start. |
| Hypervisor | `hypervisor` | `cloudhypervisor` (default) or `qemu`. Fixed. |
| Boot disk | `image`, `root_disk` or `rootfs_path` | Exactly one. Fixed. See below. |
| Root disk size | `root_disk_size_gib` | With `image` only. Default: the image's size, at least 10 GiB. |
| Firmware | `firmware` | A firmware image (name or id). Fixed. |
| Kernel boot | `kernel`, or `kernel_image_path` and `initrd_path`; `kernel_args` | Instead of firmware (not both). A kernel image (`kernel`) can be changed and applies at the next start; host paths are fixed; arguments changeable. |
| Credential | `credential` | A credential of the project. Firmware boot with the generated cloud-init seed only. |
| Networks | `networks` | Up to 8 NICs, in order. Changeable; applies at the next start. |
| Data disks | `data_disks` | Disks of the project, attached after the boot disk. Changeable; applies at the next start. |
| PCI devices | `vfio_devices` | Changeable while the VM runs. |
| Restart policy | `restart_policy` | `on_failure` (default) or `never`. Changeable. |
| On host boot | `on_host_boot` | `resume` (default) or `stop`. Changeable. |
| Stop grace | `stop_grace_secs` | Seconds a stop waits after the power button; 0 (default) stops at once; at most 300. Changeable. |
| Initial power | `power` | `stopped` (default), `running` or `paused`. |
| Node | `node` | Clusters: pin the VM to a node (name or id). Fixed. |
| Hugepages | `hugepages` | Back guest memory with hugepages. Changeable; applies at the next start. |
| Hugepage size | `hugepage_size` | `2M` or `1G`, with `hugepages`; the memory must be a whole number of pages. Default: the host's default size (usually 2M). Changeable; applies at the next start. |
| NUMA policy | `numa_policy` | `NUMA_POLICY_NONE` (default), `NUMA_POLICY_PREFERRED` or `NUMA_POLICY_STRICT`: keep the VM's memory, CPUs and PCI devices on one host NUMA node. See [NUMA and dedicated CPUs](#numa-and-dedicated-cpus). Changeable; applies at the next start. |
| Device pools | `devices` | `[{"pool": "a100", "count": 1, "same_switch": false}]`: devices the scheduler picks from a pool your project may use; shown as `pool_devices`. See [Device pools](../operator-guide/pci-passthrough.md#device-pools). Changeable; applies at the next start. |
| CPU policy | `cpu_policy` | `CPU_POLICY_SHARED` (default) or `CPU_POLICY_DEDICATED`: each vCPU on a host CPU of its own. Changeable; applies at the next start. |

### Boot disk

Give exactly one of:

- `image`: Glidex creates a linked root disk from the image for this VM
  (named `<vm>-root`), grows it to `root_disk_size_gib`, and extends the
  guest's root partition at first boot. The disk belongs to the VM and
  is deleted with it, unless you delete with `--keep-disk`.
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
needs an initramfs (distro kernels do). The default kernel arguments are
`console=ttyS0 root=/dev/vda reboot=k panic=1` on both hypervisors. The
console is on the serial port, unless the last `console=` in the
arguments is `hvc0`, which selects the virtio console. A distro cloud
image needs its own root, e.g.
`console=ttyS0 root=LABEL=cloudimg-rootfs ro` for Ubuntu.

### Host paths

`rootfs_path`, `kernel_image_path`, `initrd_path`, `cloud_init_path`
and `firmware_path` name files on the host. Using them needs the
`system-admin` role, and the files must be readable by the service
user the control plane runs as. Most users should use images, disks and
firmware images instead. A path under `/tmp` or `/var/tmp` is refused:
VMs have a private `/tmp`.

### Credential and cloud-init

With firmware boot and no `cloud_init_path`, Glidex generates a
cloud-init seed for the VM: its hostname, the root partition grow, and,
if you name a `credential`, that user with its password and SSH keys.
cloud-init applies it on the VM's first boot. Without a credential the
guest has no login. See [Guest credentials](credentials.md).

`cloud_init_path` replaces the generated seed with your own (a host
path, see above); a `credential` can't be combined with it.

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
address `0000:41:00.0`. A device is used by one VM at a time, and using
one needs a grant from an administrator. See
[PCI passthrough](../operator-guide/pci-passthrough.md).

```
gxctl> pci                                          # host PCI devices
gxctl> attach-device my-vm /sys/bus/pci/devices/0000:41:00.0
gxctl> detach-device my-vm /sys/bus/pci/devices/0000:41:00.0
```

On a running VM, devices are hot-plugged. On a paused VM, the change
waits until it runs again.

## Lifecycle

| Action | Web UI | gxctl | API |
|---|---|---|---|
| Start | **Start** | `start <vm>` | `POST /vms/{id}/start` |
| Resume a paused VM | **Resume** | `start <vm>` | `POST /vms/{id}/start` |
| Pause | **Pause** | `pause <vm>` | `POST /vms/{id}/pause` |
| Graceful stop | **Shut down** | `stop <vm> --graceful [secs]` | `POST /vms/{id}/stop?graceful_timeout_secs=60` |
| Immediate stop | **Stop** | `stop <vm>` | `POST /vms/{id}/stop` |
| Delete | **Delete** | `delete <vm> [--keep-disk]` | `DELETE /vms/{id}` |

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
- **The hypervisor crashes**: with the restart policy `on_failure`, the
  VM is started again after 10 seconds, doubling with each crash in a
  row up to 5 minutes; after 10 minutes of running the count starts
  over. With `never`, the desired state becomes stopped.
- **It fails to launch**: the VM is `failed` and Glidex retries with a
  growing delay. Its details and events say why.
- **The host reboots**: with `on_host_boot` `resume`, a VM that was
  running is started again; with `stop`, it stays stopped.

Change these policies with the API:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X PATCH https://host.example.org:8841/vms/<id> \
  -d '{"change": {"restart_policy": "never"}, "update_mask": "restart_policy"}'
```

### Change a VM

Change vCPUs, memory, networks, data disks, PCI devices, kernel
arguments, the credential, hugepages and the policies with
`PATCH /vms/{id}`. `change` holds the new values, `update_mask` lists
the fields to change (comma-separated):

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X PATCH https://host.example.org:8841/vms/<id> \
  -d '{"change": {"vcpu_count": 4, "mem_size_mib": 8192}, "update_mask": "vcpu_count,mem_size_mib"}'
```

`networks` and `data_disks` replace the whole list. The hypervisor, the
boot disk, the firmware and host paths can't be changed.

The web UI and `gxctl` have no edit form; `gxctl` changes PCI devices
only (`attach-device`, `detach-device`).

A change to vCPUs, memory, kernel arguments, the credential, hugepages,
networks or data disks of a running VM takes effect at the next start:
the web UI says **Configuration changes take effect at the next start**,
and the API sets `restart_required`. Stop and start the VM to apply it.
A new credential doesn't change logins in a guest that already booted
(cloud-init runs once).

Pass the VM's `resource_version` in an `If-Match` header to refuse the
change (`409`) if someone else changed the VM since you read it.

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

Each VM keeps a list of events: created, spec changes, launches, exits,
restarts, warnings such as a lost network port.

```
gxctl> events my-vm           # time, who, reason, message
```

In the web UI they are under **Events** on the VM's page. With the API,
`GET /vms/{id}/events`.

To follow every VM, disk, image and network of your projects as they
change:

```
gxctl> watch                  # Ctrl-C stops
gxctl> watch vms
```

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
- `cpu_policy: CPU_POLICY_DEDICATED` gives each vCPU a host CPU of its
  own, which no other VM uses. It implies `NUMA_POLICY_PREFERRED`. By
  default a dedicated VM takes whole cores: both hyperthreads of a core,
  so vCPUs 0 and 1 share a core and an odd count leaves a thread idle.
  The first core of each NUMA node stays with the host, and so do
  OVS-DPDK's CPUs. Shared VMs then run only on the CPUs dedicated VMs
  leave free, and running ones are moved when that changes.

A VM with shared vCPUs and a NUMA policy has its memory on the node but
its vCPUs anywhere on the host, so no node's CPUs sit idle.

The VM shows where it went: `numa_node`, and for dedicated vCPUs `cpus`
(vCPU *i* on `cpus[i]`). A change to its vCPUs, memory, policies,
devices or networks picks again on the same host, and applies when it
next starts.

## Placement on a cluster

On a cluster, the scheduler picks the node for a new VM when it is
created. It considers only nodes that are active, ready and not
draining, that have the hypervisor, enough vCPUs and memory left, the
PCI devices, hugepages and physical networks the VM needs, and that hold
its disks and node networks. Of those, it prefers the least loaded.

Hugepages are counted on their own: a VM with `hugepages` takes its
memory from the node's hugepages of its size, not from ordinary memory,
and needs that much left of them (`not enough 2M hugepages (… of … MiB
taken)`). They are never overcommitted, and stopped VMs hold theirs
too. A VM with a vhost-user NIC gets 2M hugepages without asking when
the node has them to spare (its memory plus a quarter, at least
256 MiB), and ordinary memory otherwise. The VM's `page_kib` says which:
4 for ordinary memory, else the hugepage size in KiB.

A VM stays on its node for its whole life: Glidex doesn't move it, and
doesn't restart it elsewhere if the node fails. If no node can take the
VM, it is still created and waits for one; its page says **Waiting for a
node** with each node's reason.

In the web UI's create form, **Placement** shows **Automatic (now:
&lt;node&gt;)** and each node with its free vCPUs and memory, or why it
can't take the VM. Pick a node to pin the VM there. With the API, set
`node` when you create the VM. To ask the scheduler without creating
anything:

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" -H 'Content-Type: application/json' \
  -X POST https://host.example.org:8841/scheduler/preview \
  -d '{"vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": "qemu"}'
```

The answer has the node it would pick (`pick`) and every node's fit
(`nodes`, with `refused` when it can't take the VM).

The VM's node shows in `gxctl list` and on its page. If the node stops
reporting, the page shows the VM's last known state; the VM may still be
running there.

See [Clusters](../operator-guide/clusters.md) for nodes and draining.
