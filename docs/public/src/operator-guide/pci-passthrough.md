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

### Bind the device to `vfio-pci`

Glidex can do this and the next step for you, per device, on any node:

```bash
gxctl node prepare-device <node> 0000:41:00.0   # its whole IOMMU group to vfio-pci; /dev/vfio/<group> to glidex
gxctl node release-device <node> 0000:41:00.0   # back on the drivers it had
```

Preparing binds every function of the device's IOMMU group (bridges
aside) to `vfio-pci` and writes a udev rule giving `/dev/vfio/<group>`
to `glidex`, so the next step is done too. It refuses a group with a NIC
the host uses or an OVS-DPDK uplink, and needs `net-admin` or
`system-admin` on that host. The web UI's node page has **Prepare** and
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
glidex-install --vfio-devices a100                 # a pool from control-plane.json
glidex-install --vfio-devices 0000:41:00.0,0000:42:00.0
glidex-install --sriov 0000:61:00.0=8 --vfio-devices cx7-vfs
glidex-install --no-vfio-devices                   # stop doing it
```

| Flag | What it does |
| --- | --- |
| `--vfio-devices LIST` | Pools of `control-plane.json` or PCI addresses, comma-separated. Each device and the rest of its IOMMU group go to `vfio-pci`. A new list replaces the old one. |
| `--sriov PF=N` | `N` VFs on physical function `PF`, set before the binding so its VFs can be bound (by a pool with `match.vf_of`). Repeatable, or comma-separated; the last count for a PF wins. |
| `--no-vfio-devices` | Forget both. What is bound now stays so until the next boot. |
| `--now` | Apply them now as well. Not saved. |

The settings are saved with the installer's other choices, so a later
run without these flags keeps them.

At boot, `glidex-vfio-boot.service` (a one-shot unit, there only while
settings exist) runs after the kernel modules load and before
glidex-netd, the control plane and libvirt:

1. It sets the VF counts.
2. It binds each selected device not yet on `vfio-pci`, with the same
   udev rule and refusals as **Prepare**, so **Release** works on it
   later.
3. It prints what it bound, skipped and couldn't do
   (`journalctl -u glidex-vfio-boot`). A device that fails stays on its
   driver; the boot goes on.

- **A device already on `vfio-pci` is never touched**, at boot or with
  `--now`, so a running VM keeps its device.
- **With `--now`**, a VF count is left as it is when a VF of that PF is
  on `vfio-pci`, since changing the count would remove that VF from its
  VM. Asking for the count it already has is fine.
- **Pools are copied as their `match`.** Re-run the installer after
  changing a pool's match. A pool needs `/etc/glidex/control-plane.json`
  on that host; addresses don't.
- **Files:** the settings are in `/etc/glidex/vfio-devices.json`, and
  `vfio-pci` is loaded from `/etc/modules-load.d/glidex-vfio.conf`.
  Uninstall and `--no-vfio-devices` remove both and the unit; devices
  stay where they are until the next boot.
- **Host drivers:** for a GPU whose host driver loads first (`nvidia`,
  `nouveau`), binding at boot takes it from that driver. Make sure
  nothing on the host uses it.
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
the web UI, a node's page (**Cluster** → a node) has a **Topology**
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
  `vfio-pci` already.

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

`projects` takes project ids or names, `tenants` tenant ids or slugs.
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

At creation, in the web UI's VM form, **VFIO PCI Devices (optional)**
takes comma-separated sysfs paths. Over the API, `vfio_devices`:

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
other and the VM's memory. Pools are defined in `pci.pools` and granted
with `pci.allow` entries that name a `pool`:

```json
{ "pool": "a100", "tenants": ["research"] }
```

The picked devices are claimed and passed through like devices named by
address. For GPUs, `"gpus": {"count": 4}` also places and sizes the VM
from the pool's `per_device`. Pools, their grants, quotas, tenant
pools, GPU requests and metering are in
[Device pools](host-resources.md#device-pools).

## NVLink fabric (NVSwitch)

On an NVSwitch board (HGX A100, say), the node page's **Topology** card
lists the NVSwitches under **NVLink fabric**: their driver, IOMMU group
and NUMA node. `gxctl node topology <node>` prints them too. They are
never offered to VMs: NVIDIA Fabric Manager owns them, on the host or in
a service VM (spec/27-gpu-vms.md). Today glidex VMs on such a board get
their GPUs without NVLink between them.

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
or `system-admin`; also the **VFs…** button on the node page.) A change
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
| `forbidden` | The project has no grant for the device in `pci.allow`, and you are not `net-admin`. |
| `PCI device … is cordoned` | An administrator cordoned it: `gxctl node uncordon-device`. |
| The VM shows `DeviceHealthy=False` | The device fell off the bus or had PCIe errors: `gxctl node topology <node> --devices`, the host's `dmesg`. |
| `conflict`: `PCI device … is used by VM …` | Another VM on that host holds the device, or its instance still does. Detach it from that VM. If the hot-unplug failed (`DevicesPending`), the device stays held until that VM stops. In a cluster, leave the VM unpinned so the scheduler can pick a node where the device is free. |
| `conflict`: `PCI device … shares IOMMU group … with …` | Another VM holds a device in the same IOMMU group. Give the whole group to one VM, or move the card to a slot with its own group. |
| `in use by qemu-kvm-one (pid …)` on a device, or `IOMMU group … is in use by …` from Release or a VF count | A VM has the group open: another hypervisor's (libvirt, OpenNebula), or for Release and VF counts also Glidex's. Being on `vfio-pci` doesn't mean it's free: check `virsh list` and `virsh dumpxml <vm>` (`<hostdev>`), and cordon the device in Glidex while the other VM keeps it. See [Devices another hypervisor holds](#devices-another-hypervisor-holds). |
| `/dev/vfio/<n> can't be opened by this user` | The group device's owner, group and mode don't give `glidex` read and write. **Prepare** the device, or add the udev rule above; `ls -l /dev/vfio/`. |
| After a reboot, devices are back on their host drivers, or VFs are gone | Keep them with the installer's `--vfio-devices` and `--sriov`. `journalctl -u glidex-vfio-boot` shows what the boot unit did. |
| `pool …: 1 of 2 free`, `no device pool …` | See [Host resources](host-resources.md#troubleshooting). |
| A VM stays **Waiting for a node** with `PCI device …` in a node's reason | `no PCI device …`: the node doesn't report it. `… is used by VM …` or `… shares IOMMU group …`: every node with it has it taken. The VM is placed once a device is free. |

The hypervisor's own error is in the VM's events (`gxctl events <vm>`)
and in `journalctl -u glidex-vm@<id>`. See also
[Troubleshooting](troubleshooting.md).
