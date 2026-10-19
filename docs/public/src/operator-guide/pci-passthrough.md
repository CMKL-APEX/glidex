# PCI passthrough

A VM can take a host PCI device (a GPU, a NIC, an NVMe drive) through
VFIO. Both hypervisors support it: Cloud Hypervisor with `--device`,
QEMU with `-device vfio-pci`. Glidex lists the host's devices, records
which VM claims which device, checks who may use it, and hot-plugs it
into a running guest.

Glidex does not prepare the host: enabling the IOMMU, binding the device
to `vfio-pci` and giving the `glidex` user access to it are up to you.

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
aside) to `vfio-pci`, remembers the drivers they had, and writes
`/etc/udev/rules.d/90-glidex-vfio-<group>.rules` giving the group device
to `glidex` (`0660`). It refuses a group with a NIC the host uses (one
with addresses or routes) or an OVS-DPDK uplink. It needs `net-admin`
or `system-admin` on that host (`POST /nodes/{id}/devices/{address}/prepare`
and `.../release`; the web UI's node page has the buttons). Like the
manual way below, it doesn't survive a reboot.

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
and it isn't an OVS-DPDK uplink. A device that isn't says why
(`not bound to vfio-pci (driver: nvidia)`, `IOMMU group 30 also holds
0000:07:00.1 on snd_hda_intel`). Over the API it is `GET /pci-devices`
for this host, and `GET /nodes/{id}/topology` for any node, read on that
node (`GET /host/topology` for the answering host). Both need a host
role (`auditor`, `net-admin` or `system-admin`) on that host. In the web
UI, a node's page (**Cluster** → a node) has a **Topology** card.

The topology also reports what it notices, without acting on it: no
IOMMU (`iommu_off`), a device without a NUMA node on a host with
several (`numa_unknown`), an IOMMU group shared with a device of another
class (`group_shared`), a NIC or disk linked slower or narrower than it
can (`link_degraded`; GPUs slow their link when idle, so they aren't
flagged), and a device on a NUMA node without hugepages when others
have them (`hugepages_unbalanced`).

Glidex names a device by its sysfs path,
`/sys/bus/pci/devices/0000:41:00.0`. The API also accepts the bare
address `0000:41:00.0` where it checks permissions, but use the full
path: it is what Glidex stores and compares.

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
`PATCH` of `vfio_devices`. The VM's page lists its **VFIO PCI Devices**.

What happens:

- A **stopped** VM gets the device at its next start.
- A **running** VM gets it hot-plugged (Cloud Hypervisor's API, QEMU's
  `device_add`), and hot-unplugged on detach. While the guest is
  paused, the change waits (the VM shows `DevicesPending` with reason
  `GuestPaused`) and is applied once it runs. If the hypervisor refuses,
  the VM shows `DevicesPending` with reason `HotplugFailed` and the
  error; stop and start the VM to apply the change at boot.
- One device belongs to one VM. A second VM asking for it is refused
  (`PCI device … is used by VM …`), and the claim lasts until the
  instance that holds it is gone, not just until the spec drops it.
- A device takes its IOMMU group with it: another VM can't have a
  device of the same group (`PCI device … shares IOMMU group … with …,
  used by VM …`). The VM that holds the group may add the group's other
  devices.
- A NIC that is a DPDK uplink of a glidex bridge can't be passed
  through.

The guest sees the device on its PCI bus; install the guest driver
there as on bare metal.

## In a cluster

Devices are per host. The scheduler places a VM with `vfio_devices`
only on a node that reports every one of those devices (otherwise:
`no PCI device …`) and where no other VM holds them. Identical hosts
with a GPU at the same address each take a VM with it. When every such
node's device is taken, a new VM waits as **Unschedulable** with the VM
holding each one in its reason, and is placed once one is free. To
choose the host, pin the VM: **Placement** in the VM form, or `node` (a
node name or id) in the API request. A pinned VM asking for a device
already taken on that node is refused. Prepare each host (IOMMU,
`vfio-pci`, the udev rule) the same way.

`gxctl pci` lists the devices of the host it is connected to. For an
agent's, use `gxctl node topology <agent>`: the server asks the agent.

## NUMA

A device is attached to one NUMA node of the host (`cat
/sys/bus/pci/devices/0000:41:00.0/numa_node`). A VM with
`numa_policy` (and, for pinned vCPUs, `cpu_policy:
CPU_POLICY_DEDICATED`) gets its memory and vCPUs on its devices' node;
see [NUMA and dedicated CPUs](../user-guide/vms.md#numa-and-dedicated-cpus).
With `NUMA_POLICY_STRICT`, devices on two NUMA nodes, or a device whose
firmware reports `-1`, are refused. Each node reports its NUMA nodes,
cores and devices' NUMA nodes in its record (`GET /nodes/{id}`,
`status.topology`).

## Device pools

Instead of naming devices by address, a VM can ask for a number of
devices from a pool, and the scheduler picks them. Pools are defined in
`/etc/glidex/control-plane.json` (on each server, the same):

```json
{
  "pci": {
    "pools": [
      { "name": "a100", "class": "gpu",
        "match": { "vendor_id": "0x10de", "device_id": "0x20b0" } },
      { "name": "cx7", "class": "nic",
        "match": { "vendor_id": "0x15b3", "device_id": "0x1021" },
        "nodes": ["gpu-01", "gpu-02"] }
    ],
    "allow": [
      { "pool": "a100", "tenants": ["research"] },
      { "pool": "cx7", "projects": ["ml-team"] }
    ]
  }
}
```

- `match` takes `vendor_id`, `device_id`, `class_code` (or its start,
  `0x0302`) and `addresses`; every key given must match. `nodes` limits
  the pool to some nodes.
- A grant with `pool` lets a project or tenant ask for that pool's
  devices (`usePciPool`); `net-admin` and `system-admin` may use any.
- A VM asks with `devices`: `[{"pool": "a100", "count": 2,
  "same_switch": true}]` (`gxctl vm create` asks for `a100:2:switch`).
  The scheduler gives it ready, uncordoned devices no other VM holds, on
  the NUMA node of its other devices first; with `same_switch`, all
  under one PCIe switch. The VM shows them as `pool_devices`, and gets
  them at its next start.
- `gxctl pci pools` (`GET /pci-pools`, a host role) lists each pool's
  devices per node, who holds them, and how many are free.

## Device health and cordoning

Each node reads its devices' PCIe error counters (AER), shown in
`gxctl node topology <node> --devices` (`aer correctable/nonfatal/fatal`)
and on the node page. A running VM's devices are watched: if one falls
off the bus or has uncorrectable errors after the VM started, the VM's
`DeviceHealthy` condition turns `False` with the reason, and a
`DeviceError` event is recorded (`gxctl events <vm>`). Glidex doesn't
stop the VM or move the device; errors inside the guest (an NVIDIA Xid,
say) only the guest sees.

A device you no longer trust can be cordoned: no new VM gets it, and the
VM that has it keeps it.

```bash
gxctl node cordon-device <node> 0000:41:00.0     # refused: PCI device 0000:41:00.0 is cordoned
gxctl node uncordon-device <node> 0000:41:00.0
```

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

The hypervisor's own error is in the VM's events (`gxctl events <vm>`)
and in `journalctl -u glidex-vm@<id>`. See also
[Troubleshooting](troubleshooting.md).
