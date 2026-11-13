# PCI passthrough

A VM can take a host PCI device (a GPU, a NIC, an NVMe drive) through
VFIO. Both hypervisors support it: Cloud Hypervisor with `--device`,
QEMU with `-device vfio-pci`. Glidex lists the host's devices, records
which VM claims which device, checks who may use it, and hot-plugs it
into a running guest.

Enabling the IOMMU is up to you. Glidex can bind a device to `vfio-pci`
and give the `glidex` user access to it, or you do that by hand. Device
pools, NUMA placement, device health and cordoning are in
[Host resources](host-resources.md).

## Host prerequisites

### IOMMU

Turn on VT-d (Intel) or AMD-Vi (AMD) in the firmware, then on the kernel
command line:

```text
intel_iommu=on iommu=pt     # Intel
amd_iommu=on iommu=pt       # AMD (often on by default)
```

After a reboot, the IOMMU groups exist:

```bash
ls /sys/kernel/iommu_groups/
```

A device can only be passed through with every other device in its
IOMMU group (except PCI bridges). `gxctl pci` shows each device's group.

### Host kernel

Before a VM starts, the hypervisor maps each passed-through device's
memory BARs into the IOMMU. For a big GPU that is fast only on a recent
kernel, and on 6.17 only with transparent hugepages:

| Host kernel | Card, BAR | Hypervisor, transparent hugepages | Mapping the BAR | Until the guest's kernel starts (4 GiB VM, UEFI) |
|---|---|---|---|---|
| 6.8 | A100 40 GB, 64 GiB | Cloud Hypervisor, `madvise` | 27 s, twice | about 60 s |
| 6.17 | A100 40 GB, 64 GiB | Cloud Hypervisor or Pylon without #76, `madvise` | 27 s, twice | about 60–70 s |
| 6.17 | A100 40 GB, 64 GiB | Cloud Hypervisor, `always` | under 0.5 s | 14 s |
| 6.17 | A100 40 GB, 64 GiB | Pylon with #76, `madvise` | under 0.5 s | 12 s |
| 7.0 | Radeon AI PRO R9700, 16 GiB | Cloud Hypervisor, `madvise` | under 0.3 s | — (the API answered in 6 s) |

The BAR is mapped once when the VM starts and again when the guest's
UEFI firmware moves it. On 6.17, the kernel maps a BAR in large pieces
only where transparent hugepages are allowed, and Ubuntu's default is
`madvise`. So on a 6.17 host, either:

- run passthrough VMs on a Pylon built with
  [CMKL-APEX/pylon#76](https://github.com/CMKL-APEX/pylon/pull/76),
  which asks for hugepages on its BAR mappings; or
- set transparent hugepages to `always`. This affects every process
  on the host:

  ```bash
  cat /sys/kernel/mm/transparent_hugepage/enabled        # [madvise] is the default
  echo always | sudo tee /sys/kernel/mm/transparent_hugepage/enabled
  ```

  Add `transparent_hugepage=always` to the kernel command line to keep
  it across reboots.

6.8 has no fast path for this; the setting wasn't tried there. A BAR's size is a
power of two, so it can be larger than the card's memory. The VM still
starts on a slow host: Glidex waits longer for it (see the
troubleshooting table). Out-of-tree drivers must build on the newer
kernel: NVIDIA OFED 2607.0.100 builds on 6.17 but not on 7.0.

### Bind the device to `vfio-pci`

Glidex can do this and the next step for you, per device, on any node:

```bash
gxctl node prepare-device <node> 0000:41:00.0   # its whole IOMMU group to vfio-pci; /dev/vfio/<group> to glidex
gxctl node release-device <node> 0000:41:00.0   # back on the drivers it had
```

Preparing loads `vfio-pci` if needed, binds every function of the
device's IOMMU group (bridges aside) to it, and writes a udev rule
giving `/dev/vfio/<group>` to `glidex`, so the next step is done too.
For a GPU with a resizable BAR it also shrinks the memory window when
the host's GPU windows would add up to its memory or more (see
[Prepare devices](host-resources.md#prepare-devices)); the boot binding
and pools with `prepare: true` do the same. It refuses a group with a NIC
the host uses or an OVS-DPDK uplink, and needs `net-admin` or
`system-admin` on that host. The host's page in the web UI has **Prepare** and
**Release** buttons. Like the manual way below, it doesn't survive a
reboot; a pool with `prepare: true` prepares its devices each time
glidex starts, and the installer can bind them at boot (see [Keep it
across reboots](#keep-it-across-reboots)). See
[Prepare devices](host-resources.md#prepare-devices).

To do it by hand, load the module, and bind the device to it instead of
its usual driver. For a device at `0000:41:00.0`:

```bash
sudo modprobe vfio-pci
echo vfio-pci | sudo tee /sys/bus/pci/devices/0000:41:00.0/driver_override
echo 0000:41:00.0 | sudo tee /sys/bus/pci/devices/0000:41:00.0/driver/unbind
echo 0000:41:00.0 | sudo tee /sys/bus/pci/drivers_probe
```

This doesn't survive a reboot; see the next section.

### Keep it across reboots

The installer can bind devices to `vfio-pci`, and set SR-IOV VF counts,
at every boot, before glidex, glidex-netd or any other hypervisor
starts:

```bash
cargo run -p glidex-install -- --vfio-devices a100     # a pool
cargo run -p glidex-install -- --vfio-devices amd-7551 # a discovered pool
cargo run -p glidex-install -- --vfio-devices 0000:41:00.0,0000:42:00.0
cargo run -p glidex-install -- --sriov 0000:61:00.0=8 --vfio-devices cx7-vfs
cargo run -p glidex-install -- --no-vfio-devices       # stop doing it
```

| Flag | What it does |
| --- | --- |
| `--vfio-devices LIST` | Pools or PCI addresses, comma-separated. A pool comes from the cluster's setting (asked of the running control plane), else this host's `control-plane.json`, else a discovered pool's name. Each device and the rest of its IOMMU group go to `vfio-pci`, except what this host's `pci.exclude` keeps; its `pci.overrides` set the window size. A new list replaces the old one. |
| `--sriov PF=N` | `N` VFs on physical function `PF`, set before the binding so its VFs can be bound (by a pool with `match.vf_of`). Repeatable, or comma-separated; the last count for a PF wins. |
| `--no-vfio-devices` | Forget both. What is bound now stays so until the next boot. |
| `--now` | Apply them now as well. Not saved. |

The settings are saved with the installer's other choices, so a later
run without these flags keeps them.

At boot, `glidex-vfio-boot.service` (a one-shot unit, there only while
settings exist) runs after the kernel modules load and before
glidex-netd, the control plane and libvirt:

1. It sets the VF counts.
2. It binds each selected device to `vfio-pci`, with the same udev rule,
   window size and refusals as **Prepare**, so **Release** works on it
   later.
3. It prints what it bound, skipped and couldn't do
   (`journalctl -u glidex-vfio-boot`). A device that fails stays on its
   driver; the boot goes on.

- **Claimed before the host driver.** The selected devices get
  `vfio-pci` before their host driver loads, so that driver (`amdgpu`,
  `nvidia`) never takes them. The installer writes
  `/etc/modprobe.d/glidex-vfio.conf`, with a hook for each host driver of
  the selected devices: before the driver loads, `glidex-netd
  --claim-boot-devices` sets `driver_override` to `vfio-pci` on each
  selected device with no driver yet. The driver then loads for its
  other devices. A udev rule, `70-glidex-vfio-claim.rules`, does the
  same for a device added later. VFs need neither: they appear when the
  unit sets the VF counts.
- **At boot, the unit sizes a claimed device's windows.** `vfio-pci` may
  have bound it as soon as it was claimed; nothing can use it yet, so
  the unit unbinds it, sets its windows (a pool's `bar_mib`) and binds it
  again.
- **With `--now`, a device already on `vfio-pci` is never touched**, so a
  running VM keeps its device.
- **With `--now`**, a VF count is left as it is when a VF of that PF is
  on `vfio-pci`, since changing the count would remove that VF from its
  VM. Asking for the count it already has is fine.
- **Pools are copied as their `match`.** Re-run the installer after
  changing a pool's match. A pool needs `/etc/glidex/control-plane.json`
  on that host; addresses don't.
- **Files:** the settings are in `/etc/glidex/vfio-devices.json`,
  `vfio-pci` is loaded from `/etc/modules-load.d/glidex-vfio.conf`, and
  the claim is in `/etc/modprobe.d/glidex-vfio.conf` and
  `/etc/udev/rules.d/70-glidex-vfio-claim.rules`. Uninstall and
  `--no-vfio-devices` remove them and the unit; devices stay where they
  are until the next boot.
- **A host driver in the initramfs** loads before the claim's hook
  exists, so it can still take a selected device; the installer says so
  when it finds the driver's module there. The unit then unbinds the
  device from it, which works once the driver has finished starting.
  Make sure nothing on the host uses the device.
- The boot devices are set up with the host networking: an install with
  `--no-networking` doesn't apply them.

### Let the `glidex` user open it

VMs run in `glidex-vm@<id>.service` units as the `glidex` user. The unit
allows the VFIO devices (`/dev/vfio/vfio` and the per-group
`/dev/vfio/<group>`), but the group device must also be readable and
writable by `glidex` through its owner, group or mode. Glidex reads
those to decide whether a device is ready, as the VM's unit would open
it; ACLs aren't considered. By default the group device belongs to
`root`. **Prepare** and the boot binding write a rule per group; a udev
rule of your own covers every group:

```text
# /etc/udev/rules.d/90-glidex-vfio.rules
SUBSYSTEM=="vfio", GROUP="glidex", MODE="0660"
```

```bash
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=vfio
ls -l /dev/vfio/
```

The VM's unit also lets the hypervisor lock all of the VM's memory
(`LimitMEMLOCK=infinity`): VFIO pins the guest's RAM so the device can
reach it. Without it, Cloud Hypervisor stops at `Failed to DMA map VFIO
device … Out of memory`. A unit installed by an older Glidex lacks the
line: run `glidex-install` again. The VM's memory is still bounded by its
size.

## Find devices

```bash
gxctl pci                    # every host PCI device: address, ids, class, driver, IOMMU group, NUMA node, ready
gxctl pci 0000:41:00.0       # one device, with its sysfs path and why it isn't ready
gxctl node topology <node>   # a node's NUMA nodes with their CPUs, memory, hugepages and devices
gxctl node topology <node> --devices   # its devices with their root port, switch, link and VFs
```

A device is **ready** for passthrough when the host has an IOMMU, the
device is bound to `vfio-pci`, every other device in its IOMMU group
(bridges aside) is too, the `glidex` user can open `/dev/vfio/<group>`,
it isn't an OVS-DPDK uplink, and no other hypervisor on the host is
using it. A device that isn't says why; the reasons are listed in
[Ready devices](host-resources.md#ready-devices). Over the API it is `GET /pci-devices`
for this host, and `GET /nodes/{id}/topology` for any node. Both need a
host role (`auditor`, `net-admin` or `system-admin`) on that host. In
the web UI, a host's page (**Hosts**, or **Cluster** → a node) has a **Topology**
card. The topology's fields and warnings are explained in
[Host topology](host-resources.md#host-topology).

Glidex names a device by its sysfs path,
`/sys/bus/pci/devices/0000:41:00.0`. The API also accepts the bare
address `0000:41:00.0`, but use the full path: it is what Glidex stores
and hands to the hypervisor. Claims and placement compare PCI
addresses, so the path and the bare address are the same device.

## Devices another hypervisor holds

On a host that also runs libvirt, OpenNebula or another hypervisor, a
device can be on `vfio-pci` and still be in a running VM that isn't
Glidex's. Such a VM may not show up with `fuser /dev/vfio/<group>`:
libvirt gives each QEMU its own `/dev`. Glidex looks at every process's
open files instead (through glidex-netd), and:

- marks the device not ready: `in use by qemu-kvm-one (pid 37226)`.
  Pools and the scheduler skip it;
- refuses **Release** of its group, and a VF count change on its PF:
  `IOMMU group 30 is in use by qemu-kvm-one (pid 37226)`;
- leaves it alone in the boot binding and `--now`, since it is on
  `vfio-pci` already;
- shows it greyed out, with that reason, in the web UI's VM form, where
  a node's GPUs are picked, so nobody picks it.

Glidex's own VMs (run as the `glidex` user) don't count against
readiness. Without glidex-netd, nothing is known about other holders.
While another hypervisor keeps a device, cordon it in Glidex too
(`gxctl node cordon-device`), so it stays out of pools once that VM
stops until you hand it over.

## Who may use a device

Passing a device through needs the `usePciDevice` permission on it. By
default:

- `net-admin` and `system-admin` may use any device. Given on one host
  (a per-host role), they cover that host's devices only.
- A project may use the devices granted to it in
  `/etc/glidex/control-plane.json`, by project or by tenant:

```json
{
  "pci": {
    "allow": [
      { "bdf": "0000:41:00.0", "projects": ["ml-team"] },
      { "bdf": "0000:42:00.0", "tenants": ["research"] },
      { "bdf": "0000:87:00.0", "projects": ["vision"], "node": "gpu-02" }
    ]
  }
}
```

`projects` takes project ids, `<tenant>/<project>` (the tenant's id or
slug, then the project's name), or bare project names; `tenants` takes
tenant ids or slugs. A slug is that of a tenant in the `local`
directory, whoever asks; name a tenant of another directory by its id
(slugs are unique within a directory only). Project names are unique
within a tenant only, so
on a host with several tenants name projects as `<tenant>/<project>`. A
bare name grants only while one project has it: the control plane
refuses to start when a grant's bare name matches projects in more than
one tenant, and refuses to create a project whose name such a grant
uses while another project already has it.
In a cluster, the same address on two hosts is two devices: `node` (a
node id or name) grants the one on that node; without it, the grant
covers the device at that address on every node. A VM is then placed
only on a node where the project may use all its devices (otherwise:
`its PCI devices are not granted to the project on this node`); which
nodes those are is worked out when its devices are set, so a later
change to `pci.allow` applies to a waiting VM once its devices are set
again. In Cedar a device is `PciDevice::"<node>/<address>"`, in that
node's `Host`.
Restart the control plane after changing the file. Site policies can
widen or narrow this; see [Access control](access-control.md).

Adding a device to an existing VM also needs `attachDevice` on the VM,
and removing one `detachDevice` (both part of the `editor` role).

## Attach a device

At creation, in the web UI's VM form, **GPUs** lists a node's GPUs (on
a single host, its own; in a cluster, the node chosen under
**Placement**) with their vendor, ids, NUMA node and whether each is
available. Tick the ones the VM gets. A GPU that isn't ready or is held
by a VM is greyed out with the reason; the list needs a host role to
read the node's topology. Other devices go in **VFIO PCI Devices
(optional)** under **Advanced options**, as comma-separated sysfs paths.
Over the API both are `vfio_devices`:

```bash
api -X POST https://localhost:8841/vms -H 'Content-Type: application/json' -d '{
  "name": "gpu-vm", "vcpu_count": 8, "mem_size_mib": 16384,
  "image": "ubuntu-26.04", "root_disk_size_gib": 40, "credential": "alice",
  "vfio_devices": ["/sys/bus/pci/devices/0000:41:00.0"]
}'
```

On an existing VM, stopped or running:

```bash
gxctl attach-device gpu-vm /sys/bus/pci/devices/0000:41:00.0
gxctl detach-device gpu-vm /sys/bus/pci/devices/0000:41:00.0
```

Over the API these are `POST` and `DELETE /vms/{id}/devices`, or a
`PATCH` whose `change` sets `vfio_devices` (with `update_mask`
`vfio_devices`). The VM's page lists its **VFIO PCI Devices**.

What happens:

- A **stopped** VM gets the device at its next start.
- A **running** VM gets it hot-plugged (Cloud Hypervisor's API, QEMU's
  `device_add`), and hot-unplugged on detach. While the guest is
  paused, the change waits (the VM shows `DevicesPending` with reason
  `GuestPaused`) and is applied once it runs. If the hypervisor refuses,
  the VM shows `DevicesPending` with reason `HotplugFailed` and the
  error; stop and start the VM to apply the change at boot.
- One device belongs to one VM on its host. A second VM on the same
  host asking for it is refused (`PCI device … is used by VM …`), and
  the claim lasts until the instance that holds it is gone, not just
  until the spec drops it.
- A device takes its IOMMU group with it: another VM can't have a
  device of the same group (`PCI device … shares IOMMU group … with …,
  used by VM …`). The VM that holds the group may add the group's other
  devices. PCI bridges in the group don't count.
- Both refusals are `409 conflict`, at creation and when a device is
  added (`attach-device`, or a `PATCH` of `vfio_devices`).
- A NIC that is a DPDK uplink of a glidex bridge can't be passed
  through.

The guest sees the device on its PCI bus; install the guest driver
there as on bare metal.

GPUs have large memory windows (BARs), up to the size of their memory.
QEMU's guest has room for 32 GiB of them by default; when a QEMU VM's
devices need more, Glidex gives the guest a 64-bit PCI window that holds
them (`-global q35-pcihost.pci-hole64-size=`), sized from the devices it
has when it starts. A device added to a running QEMU VM must fit what is
left: if it doesn't, stop and start the VM.

## In a cluster

Devices are per host. The scheduler places a VM with `vfio_devices`
only on a node that reports every one of those devices (otherwise:
`no PCI device …`) and where no other VM holds them. Identical hosts
with a GPU at the same address each take a VM with it. When every such
node's device is taken, a new VM waits as **Unschedulable** (its page
says **Waiting for a node**) with the VM holding each one in its
reason, and is placed once one is free. To choose the host, pin the VM:
**Placement** in the VM form, or `node` (a node name or id) in the API
request. A pinned VM asking for a device already taken on that node is
refused. A node is also skipped where the device is cordoned, or where
the project has no grant for it (`its PCI devices are not granted to the
project on this node`). Prepare each host (IOMMU, `vfio-pci`, the udev
rule) the same way.

Claims are per node: a VM on one node doesn't hold the device at the
same address on another. Adding a device to a placed VM is checked
against its node's VMs; a VM still waiting for a node is checked when
the scheduler places it. A node running an older Glidex version reports
no IOMMU groups, so until it is updated its claims are by address only.

`gxctl pci` lists the devices of the host it is connected to. For an
agent's, use `gxctl node topology <agent>`: the server asks the agent.

## NUMA

A device is attached to one NUMA node of the host (`cat
/sys/bus/pci/devices/0000:41:00.0/numa_node`, or the NUMA column of
`gxctl pci`). A VM with `numa_policy` (and, for pinned vCPUs,
`cpu_policy: CPU_POLICY_DEDICATED`) gets its memory and vCPUs on its
devices' NUMA node. With `NUMA_POLICY_STRICT`, devices on two NUMA
nodes, or a device whose firmware reports `-1`, are refused. With
`NUMA_POLICY_ALIGNED` (what a GPU request sets), a VM whose devices are
on several NUMA nodes is split over exactly those, and its guest gets
one NUMA node and PCI root per host node. See
[NUMA nodes](host-resources.md#numa-nodes) and, for users,
[NUMA and dedicated CPUs](../user-guide/vms.md#numa-and-dedicated-cpus).

## Device pools

Instead of naming devices by address, a VM can ask for a number of
devices from a pool (`devices: [{"pool": "a100", "count": 2}]`), and
Glidex picks ready, uncordoned devices no other VM holds, near each
other and the VM's memory. Pools are defined in `pools` and granted
with `allow` entries that name a `pool`:

```json
{ "pool": "a100", "tenants": ["research"] }
```

Pools and grants are a cluster setting: every server reads the same
ones, whichever leads. Show them, or replace them from a JSON file with
the same `pools` and `allow` arrays as a configuration file's `pci`
section:

```bash
gxctl pci settings
gxctl pci settings set pci.json
```

Or change one pool or grant at a time:

```bash
gxctl pci pool show r9700
gxctl pci pool set r9700.json                 # one pool: add it, or replace the pool of its name
gxctl pci pool rm r9700                       # the pool and its grants
gxctl pci grant r9700 --projects lab,ml       # --tenants acme, or --tenants '*' for every tenant
gxctl pci grant rm r9700 --projects ml        # without names, the whole grant
gxctl pci grant                               # list the grants
```

Each reads the setting, changes it and writes it back whole, so the
same checks apply.

The Cluster page's **Device pools** card shows the same, standalone or
clustered. It lists each pool, defined or discovered, with its free
devices per node, its sizing and who it's granted to. A system admin can
also adopt a discovered pool, remove a pool (its grants go with it), or
edit the pools and grants as JSON there. **Grants…** on a pool picks
the projects and tenants that may use it from the directory, or every
admitted tenant; saving a grant on a discovered pool adopts it under
its name.

The call is `PUT /cluster/settings/pci` and needs `system-admin`. It's
refused while a VM or a tenant pool still asks for a pool it removes or
renames. Until the setting is written, a server uses `pci.pools` and
`pci.allow` from its own `control-plane.json`. The leader, or a
standalone host, copies its file's pools and grants into the setting
once. After that, those file keys are ignored, and a server whose file
still differs logs a warning saying so.

### Pool and grant reference

The setting is `{"pools": [...], "allow": [...]}`, the same as a
configuration file's `pci` section. Unknown fields are refused.

```json
{
  "pools": [
    { "name": "r9700", "class": "gpu",
      "match": { "vendor_id": "0x1002", "device_id": "0x7551" },
      "bar_mib": 16384,
      "per_device": { "vcpus": 8, "memory_mib": 32768 } },
    { "name": "cx7-vfs", "class": "nic", "prepare": true,
      "match": { "vf_of": { "vendor_id": "0x15b3", "device_id": "0x1021" } },
      "nodes": ["prism1"] }
  ],
  "allow": [
    { "pool": "r9700", "projects": ["lab"], "tenants": ["acme"] },
    { "pool": "cx7-vfs", "tenants": ["*"] },
    { "bdf": "0000:41:00.0", "node": "prism2", "projects": ["ml"] }
  ]
}
```

A pool (`pools[]`):

| Field | Type | Meaning |
|---|---|---|
| `name` | string, required | Unique. Requests and grants name it. A pool named like a discovered one (`amd-7551`) replaces it. |
| `class` | string | `gpu`, `nic`, `nvme`, `dpu` or `accelerator`: a label for people and metering. A `gpus` request takes a pool of class `gpu`. |
| `match` | object, required | Which devices are the pool's (below). |
| `nodes` | string array | Node ids or names the pool is on. Empty: every node. |
| `prepare` | bool, default `false` | Each node binds the pool's devices to `vfio-pci` when its control plane starts (new VFs also right after `node sriov`). With `false`, a device on its host driver is listed as not ready. |
| `per_device` | object | What a `gpus` request gets per device unless the VM sizes itself: `vcpus` and `memory_mib` (required), `hugepages` (bool) and `cpu_policy` (`"shared"` or `"dedicated"`). Without it, a VM asking `gpus` gives its own `vcpu_count` and `mem_size_mib`. |
| `bar_mib` | integer | The largest GPU memory window, in MiB, a power of two (`16384`). Without it, the window is sized against the host's memory ([AMD GPUs](#amd-gpus)). |

A match (`match`) holds keys that must all match:

| Key | Matches |
|---|---|
| `vendor_id`, `device_id` | Hex ids, `"0x10de"`; case and the `0x` don't matter |
| `class_code` | A PCI class code or its start, `"0x0302"` |
| `addresses` | PCI addresses, `"0000:41:00.0"` |
| `vf_of` | A match of physical functions: the pool takes their SR-IOV VFs, and the other keys then match the VF ([SR-IOV virtual functions](#sr-iov-virtual-functions)) |

A grant (`allow[]`) names one pool or one device:

| Field | Type | Meaning |
|---|---|---|
| `pool` | string | A pool of `pools`. To grant a discovered pool, adopt it first; `gxctl pci grant` and the card's **Grants…** do that for you. |
| `bdf` | string | Instead of `pool`: one device's PCI address. |
| `node` | string | With `bdf`: the node (id or name) whose device it is. Without it, the device at that address on every node. |
| `projects` | string array | Project ids or names that may use it. |
| `tenants` | string array | Tenant ids or slugs: every project of theirs may use it. `["*"]` on a pool: every tenant admitted on the cluster (an open pool). |

The setting is refused when:

- a pool has no name or the name of another pool;
- a pool's `match` is empty (it would match every device), or its
  `vf_of` is;
- `bar_mib` isn't a power of two;
- two pools have the same `match` on nodes they share: a device belongs
  to at most one pool;
- a grant names both `bdf` and `pool`, or neither, or a pool that isn't
  in `pools`;
- `"*"` is with other tenants, or on a `bdf` grant;
- it removes or renames a pool that a VM's `devices` or `gpus`, or a
  tenant pool's `from`, still asks for (`409`, listing them).

A grant isn't the only limit: the tenant's cap on the pool
(`devices.<pool>` in its admission, [Who may use a
pool](host-resources.md#who-may-use-a-pool)) and tenant pools, which
share a tenant's grant among its projects, still apply. `net-admin` and
`system-admin` may use any pool without a grant. What only one host decides, `pci.exclude` and `pci.overrides`,
stays in that host's file ([Per-host settings](#per-host-settings)).

### Discovered pools

GPUs, accelerators and network cards that no pool covers form a pool
per model with no setup, named `<vendor>-<device>`: `amd-7551`,
`nvidia-2330`, or the vendor's hex id for other vendors (`15b3-1021`, a
ConnectX-7). A network card's SR-IOV VFs form a pool of their own,
named after the card with `-vf` (`15b3-1021-vf`). `gxctl pci pools`
marks them `discovered`. A server's BMC console (ASPEED or Matrox
display) is left out.

Discovery never binds a driver, so a device still on its host driver
shows as not ready until you prepare it. That includes the host's own
network ports, which are listed but never taken, and a card whose VFs
are enabled (pass the VFs through instead). To keep a port off the list
entirely, exclude it on its host (below).

Only users with `net-admin` (or `system-admin`) may use a discovered
pool, and projects don't see it. To size it or grant it, adopt it,
optionally under a name of your own:

```bash
gxctl pci pool adopt amd-7551 --name r9700
gxctl pci pool adopt 15b3-1021-vf --name cx7-vfs   # a pool of VFs: matches them by their card (vf_of)
```

This writes a pool matching the model to the cluster setting, where you
edit it as any other pool. A rename is refused while VMs still ask for
the discovered name. `gxctl pci grant amd-7551 …` adopts it under its
own name first. To have Glidex bind an adopted pool's devices to
`vfio-pci` (new VFs come up on the card's driver), set `"prepare": true`
on it with `gxctl pci pool set`.

### Per-host settings

What only one host decides stays in that host's `control-plane.json`:

```json
{ "pci": {
    "exclude": [ { "addresses": ["0000:43:00.0"] } ],
    "overrides": { "r9700": { "bar_mib": 16384 } } } }
```

- `exclude` takes pool matches. A device it matches is kept by the
  host: it is in no pool, Glidex never prepares it, and a VM that names
  its address isn't placed on this host. `gxctl pci pools` leaves it
  out.
- `overrides` set a pool's `bar_mib` and `prepare` on this host only.
  Use it when hosts differ in memory, since the safe GPU window size
  depends on it.

Restart the control plane after changing either.

The picked devices are claimed and passed through like devices named by
address. For GPUs, `"gpus": {"count": 4}` also places and sizes the VM
from the pool's `per_device`. Pools, their grants, quotas, tenant
pools, GPU requests and metering are in
[Device pools](host-resources.md#device-pools).

## NVLink fabric (NVSwitch)

On an NVSwitch board (HGX A100, say), the host page's **Topology** card
lists the NVSwitches under **NVLink fabric**: their driver, IOMMU group
and NUMA node. `gxctl node topology <node>` prints them too. They are
never offered to VMs: NVIDIA Fabric Manager owns them, on the host or in
a service VM (spec/27-gpu-vms.md). Today glidex VMs on such a board get
their GPUs without NVLink between them.

## AMD GPUs

AMD GPUs take the same path as any other device: a pool, a grant, and
`gpus` or `devices` in the VM. Nothing in Glidex depends on the
vendor; only NVIDIA's NVLink fabric (above) is vendor-specific. A pool
for Radeon AI PRO R9700s, prepared by each node at start:

```json
{ "name": "r9700", "class": "gpu",
  "match": { "vendor_id": "0x1002", "device_id": "0x7551" },
  "prepare": true,
  "bar_mib": 16384,
  "per_device": { "vcpus": 4, "memory_mib": 8192 } }
```

- **The memory window.** With resizable BAR on, an R9700's window is as
  large as its memory (32 GiB). Preparing makes it smaller so that the
  host's GPU windows add up to less than its memory. On a host with much
  more memory than that, the window stays at 32 GiB; on one test host
  (247 GiB), a VM with a 32 GiB window froze the host when it started,
  for a reason not yet known, while 16 GiB ran. Set `bar_mib` (16384)
  to keep the pool's windows at most that. The guest's `amdgpu`
  still reaches all of the GPU's memory through it (`Detected VRAM
  RAM=32624M, BAR=16384M`). See
  [Prepare devices](host-resources.md#prepare-devices).
- **Across reboots.** `glidex-install --vfio-devices r9700` binds the
  pool's GPUs to `vfio-pci` at boot, before Glidex and any VM start,
  with the pool's `bar_mib`. They are claimed before `amdgpu` loads, so
  `amdgpu` never takes them. Without it, `prepare: true` binds them when
  Glidex starts, taking them from `amdgpu`.
- **Locked memory.** A VM with a GPU locks its guest memory; the VM
  unit allows it (`LimitMEMLOCK=infinity`). A host installed before
  that setting gets it when `glidex-install` runs again. Until then the
  hypervisor fails with `Failed to DMA map VFIO device … Out of memory`.

### The guest image

Glidex ships no GPU image: the driver and ROCm releases are yours to
choose. What the guest needs:

- **`amdgpu`.** It's in Ubuntu's `generic` kernel. A `virtual` or `kvm`
  kernel needs `linux-modules-extra-$(uname -r)`.
- **The GPU's firmware: `linux-firmware`.** Ubuntu's cloud images don't
  have it, and without it `amdgpu` stops at
  `early_init of IP block <psp> failed -19`.
- **ROCm**, only to run work on the GPU (`rocminfo`, PyTorch).

On a VM from a stock Ubuntu image:

```bash
sudo apt-get install -y linux-firmware   # and linux-modules-extra-$(uname -r) on a virtual kernel
sudo reboot
# then:
sudo dmesg | grep -i amdgpu              # "Detected VRAM RAM=…" and no "failed"
ls /dev/kfd /dev/dri/renderD*
```

To skip that step for every VM, build an image with these packages and
add it to the catalog ([Pulling an image](../user-guide/images-and-disks.md#pulling-an-image)).

## SR-IOV virtual functions

A NIC (or GPU) with SR-IOV can be split into virtual functions (VFs),
each passed through to its own VM. `gxctl node topology <node>
--devices` shows a physical function's `vfs enabled/total` and how many
are ready.

```bash
gxctl node sriov gpu-01 0000:61:00.0 8     # 8 VFs on the PF
gxctl node sriov gpu-01 0000:61:00.0 0     # none
```

(`POST /nodes/{id}/devices/{pf}/sriov` with `{"vfs": 8}`, `net-admin`
or `system-admin`; also the **VFs…** button on the host's page.) A change
removes every VF the PF had, so it is refused while a VM holds the PF or
one of its VFs, and for a PF that is an OVS-DPDK uplink. To keep a VF
count across reboots, give it to the installer: `--sriov
0000:61:00.0=8` (see [Keep it across reboots](#keep-it-across-reboots)).

A PF with VFs enabled is no longer offered itself (passing it through
would take its VFs with it). A pool takes VFs with `match.vf_of`, which
matches their PF:

```json
{ "name": "cx7-vfs", "class": "nic", "prepare": true,
  "match": { "vf_of": { "vendor_id": "0x15b3", "device_id": "0x1021" } } }
```

New VFs come up on the NIC's own driver; `prepare: true` binds a pool's
VFs to `vfio-pci` at start and right after `node sriov`. Only VFs on
`vfio-pci` are listed as pool devices. A VF inherits its PF's NUMA node.
Its MAC, VLAN and spoof checking are the PF's defaults for now.

## Device health and cordoning

Glidex watches the devices of running VMs: if one falls off the bus or
has uncorrectable PCIe errors after the VM started, the VM's
`DeviceHealthy` condition turns `False` and a `DeviceError` event is
recorded. It doesn't stop the VM or take the device away.

A device you no longer trust can be cordoned: no new VM gets it, and the
VM that has it keeps it.

```bash
gxctl node cordon-device <node> 0000:41:00.0     # new VMs asking for it: PCI device 0000:41:00.0 is cordoned
gxctl node uncordon-device <node> 0000:41:00.0
```

See [Device health and cordoning](host-resources.md#device-health-and-cordoning).

## Troubleshooting

| Symptom | Check |
| --- | --- |
| `invalid_config: … is not a PCI device` | Use `/sys/bus/pci/devices/DDDD:BB:DD.F`. |
| The VM fails to start: VFIO group not viable | Another device in the IOMMU group is still on a host driver. Bind all of them to `vfio-pci`, or move the card to another slot. |
| The VM fails to start: permission denied on `/dev/vfio/<n>` | The udev rule above; `ls -l /dev/vfio/`. |
| No IOMMU group in `gxctl pci` | The IOMMU is off: firmware setting and kernel command line. |
| The VM relaunches over and over; the console ends each try with `exited: launch_failed (signal 9): cloud-hypervisor API socket not ready after N s` | The hypervisor was still setting up when Glidex stopped it. Before it answers, Cloud Hypervisor maps the VM's memory and each passthrough device's memory BARs for the device: about 1 s per GiB of memory, and on a 6.8 host, or a 6.17 host with transparent hugepages at `madvise`, about 27 s for the 64 GiB BAR of a 40 GiB A100 (a BAR is a power of two, so it can be larger than the card's memory). Glidex waits 30 s, plus 1 s per GiB of memory and 1 s per 2 GiB of the devices' BARs, up to 15 minutes. If a VM still runs out, give it less memory, use QEMU, whose socket answers before that work, or make the mapping fast (see [Host kernel](#host-kernel)). |
| A VM with a ConnectX card prints nothing for a minute or more before its kernel starts | The guest's UEFI firmware spends that time on the card, on both hypervisors (70–95 s seen with a ConnectX-6 in InfiniBand mode, 2026-11-09). The VM then boots normally. Most of it is the card's own boot ROM (FlexBoot): the firmware reads it and the ROM then busy-waits. Turn it off on the card with `mlxconfig -d <bdf> set EXP_ROM_UEFI_x86_ENABLE=0 EXP_ROM_PXE_ENABLE=0 UEFI_HII_EN=0` and reboot the host (`=1` undoes it). On a test host that took the firmware from 67 s to 29 s; what the rest is wasn't measured. |
| `forbidden` | The project has no grant for the device in `pci.allow`, and you are not `net-admin`. |
| `PCI device … is cordoned` | An administrator cordoned it: `gxctl node uncordon-device`. |
| The VM shows `DeviceHealthy=False` | The device fell off the bus or had PCIe errors: `gxctl node topology <node> --devices`, the host's `dmesg`. |
| `conflict`: `PCI device … is used by VM …` | Another VM on that host holds the device, or its instance still does. Detach it from that VM. If the hot-unplug failed (`DevicesPending`), the device stays held until that VM stops. In a cluster, leave the VM unpinned so the scheduler can pick a node where the device is free. |
| `conflict`: `PCI device … shares IOMMU group … with …` | Another VM holds a device in the same IOMMU group. Give the whole group to one VM, or move the card to a slot with its own group. |
| `in use by qemu-kvm-one (pid …)` on a device, or `IOMMU group … is in use by …` from Release or a VF count | A VM has the group open: another hypervisor's (libvirt, OpenNebula), or for Release and VF counts also Glidex's. Being on `vfio-pci` doesn't mean it's free: check `virsh list` and `virsh dumpxml <vm>` (`<hostdev>`), and cordon the device in Glidex while the other VM keeps it. See [Devices another hypervisor holds](#devices-another-hypervisor-holds). |
| `/dev/vfio/<n> can't be opened by this user` | The group device's owner, group and mode don't give `glidex` read and write. **Prepare** the device, or add the udev rule above; `ls -l /dev/vfio/`. |
| After a reboot, devices are back on their host drivers, or VFs are gone | Keep them with the installer's `--vfio-devices` and `--sriov`. `journalctl -u glidex-vfio-boot` shows what the boot unit did, and `journalctl -b \| grep 'glidex-netd: claim'` what the claim did. |
| `glidex-vfio-boot`: `driver/unbind: Permission denied` for a GPU | The host driver was still starting on the card when the unit ran: a host installed before the claim. Run `glidex-install` again (it writes the claim), or run `glidex-netd --boot-devices` once the driver is idle. |
| `… 64-bit BAR of … MiB doesn't fit PCI segment …'s window` (Cloud Hypervisor, Pylon) | The VM's GPUs on one NUMA node need more address space than the hypervisor gives that node's PCI segment: each large BAR is aligned to its own size. Give the VM fewer GPUs per NUMA node, shrink the GPUs' windows (a pool's `bar_mib`), or use QEMU. |
| `pool …: 1 of 2 free`, `no device pool …` | See [Host resources](host-resources.md#troubleshooting). |
| A VM stays **Waiting for a node** with `PCI device …` in a node's reason | `no PCI device …`: the node doesn't report it. `… is used by VM …` or `… shares IOMMU group …`: every node with it has it taken. The VM is placed once a device is free. |

The hypervisor's own error is in the VM's events (`gxctl events <vm>`)
and in `journalctl -u glidex-vm@<id>`. See also
[Troubleshooting](troubleshooting.md).
