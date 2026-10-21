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
glidex starts. See [Prepare devices](host-resources.md#prepare-devices).

To do it by hand, load the module, and bind the device to it instead of
its usual driver. For a device at `0000:41:00.0`:

```bash
sudo modprobe vfio-pci
echo vfio-pci | sudo tee /sys/bus/pci/devices/0000:41:00.0/driver_override
echo 0000:41:00.0 | sudo tee /sys/bus/pci/devices/0000:41:00.0/driver/unbind
echo 0000:41:00.0 | sudo tee /sys/bus/pci/drivers_probe
```

This doesn't survive a reboot. To make it permanent, use your
distribution's way (for example `options vfio-pci ids=10de:2204` in
`/etc/modprobe.d/`, with the vendor and device ids from `gxctl pci`, and
`vfio-pci` in `/etc/modules-load.d/`). For a GPU, also keep the host's
own driver from claiming it first.

The installer loads `vfio-pci` at boot only for the `dpdk` OVS profile
(for DPDK NICs); for passthrough, load it yourself.

### Let the `glidex` user open it

VMs run in `glidex-vm@<id>.service` units as the `glidex` user. The unit
allows the VFIO devices (`/dev/vfio/vfio` and the per-group
`/dev/vfio/<group>`), but the group device must also be readable and
writable by `glidex`. By default it belongs to `root`. A udev rule fixes
that for every group:

```text
# /etc/udev/rules.d/90-glidex-vfio.rules
SUBSYSTEM=="vfio", GROUP="glidex", MODE="0660"
```

```bash
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=vfio
ls -l /dev/vfio/
```

<!-- TODO(verify): glidex ships no udev rule for /dev/vfio/<group>; confirm this rule is enough on its own (no memlock limit needed in glidex-vm@.service) for both hypervisors. -->

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
using it (`in use by qemu-kvm-one (pid 37226)`: a libvirt or OpenNebula
VM holding it). A device that isn't says why
(`not bound to vfio-pci (driver: nvidia)`, `IOMMU group 30 also holds
0000:07:00.1 on snd_hda_intel`). Over the API it is `GET /pci-devices`
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
nodes, or a device whose firmware reports `-1`, are refused. See
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
address. Pools, their grants, quotas, tenant pools and metering are in
[Device pools](host-resources.md#device-pools).

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
one of its VFs, and for a PF that is an OVS-DPDK uplink. VFs don't
survive a reboot by themselves.

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
| `in use by qemu-kvm-one (pid …)` on a device, or `IOMMU group … is in use by …` from Release or a VF count | Another hypervisor on the host (libvirt, OpenNebula) has the device in a running VM. Being on `vfio-pci` doesn't mean it's free: check `virsh list` and `virsh dumpxml <vm>` (`<hostdev>`), and cordon the device in glidex while the other VM keeps it. |
| `pool …: 1 of 2 free`, `no device pool …` | See [Host resources](host-resources.md#troubleshooting). |
| A VM stays **Waiting for a node** with `PCI device …` in a node's reason | `no PCI device …`: the node doesn't report it. `… is used by VM …` or `… shares IOMMU group …`: every node with it has it taken. The VM is placed once a device is free. |

The hypervisor's own error is in the VM's events (`gxctl events <vm>`)
and in `journalctl -u glidex-vm@<id>`. See also
[Troubleshooting](troubleshooting.md).
