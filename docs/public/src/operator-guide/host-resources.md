# Host resources

Besides vCPUs and memory, Glidex knows how each host is built: its NUMA
nodes and cores, its hugepages, and its PCIe tree with the devices a VM
could be given. It uses that to:

- keep a VM's memory, vCPUs and devices on one NUMA node, and give it
  dedicated host CPUs;
- count hugepages per node and page size, so VMs that need them get
  them;
- hand out devices from **pools** by count, near each other;
- prepare devices for passthrough, watch their health, and cordon the
  ones you don't trust.

All of this works on a single host and in a cluster. On a single host
the node is called `local`. Passing a device through by its address,
and what the host needs for VFIO, is in
[PCI passthrough](pci-passthrough.md).

## Host topology

Each node reads its topology from sysfs. A summary (NUMA nodes, cores,
memory, hugepages, reserved CPUs, assignable devices) goes in the
node's record and is refreshed every two minutes, and right after a
device is prepared or released. The full tree is read fresh on the node
each time you ask for it.

```bash
gxctl node topology <node>             # NUMA nodes with CPUs, memory, hugepages and devices; reserved CPUs; warnings
gxctl node topology <node> --devices   # devices with NUMA node, root port, switch, link, VFs, PCIe errors
gxctl pci                              # this host's PCI devices, with NUMA node and readiness
gxctl pci 0000:41:00.0                 # one device, and why it isn't ready
```

`<node>` is a node name or id. In the web UI, a node's page (**Cluster**
→ a node) has a **Topology** card: one column per NUMA node with its
CPUs, memory, hugepages and devices, then devices with no NUMA node,
the reserved CPUs, and the warnings.

| Shown | Meaning |
| --- | --- |
| CPUs, cores | The NUMA node's CPUs, grouped by core (hyperthreads of one core together). |
| Memory | The node's memory, without its hugepage pool. |
| Hugepages | Pages per size on that NUMA node, less what OVS-DPDK takes there. |
| Devices | The devices a VM could be given: GPUs, NICs, NVMe drives, accelerators, and anything bound to `vfio-pci`. Each with its class, ids, driver, IOMMU group, readiness and, with `--devices`, its root port, the PCIe switch above it, its link speed and width, SR-IOV VFs and PCIe error counts. |
| Reserved CPUs | CPUs no VM gets: OVS-DPDK's PMD and lcore CPUs, and `cluster.node_reserved.cpu_list`. |

Reading a node's topology needs a host role (`auditor`, `net-admin` or
`system-admin`) on that host. Over the API it is
`GET /nodes/{id}/topology` (read on that node) or `GET /host/topology`
(the host that answers).

### Ready devices

A device is **ready** for passthrough when:

- the host has an IOMMU and the device has an IOMMU group;
- it is bound to `vfio-pci`;
- the other functions of its IOMMU group are on `vfio-pci` or unbound
  (bridges stay with the host);
- the `glidex` user can open `/dev/vfio/<group>`;
- it isn't an OVS-DPDK uplink.

A device that isn't says why, for example `not bound to vfio-pci
(driver: nvidia)` or `IOMMU group 30 also holds 0000:07:00.1 on
snd_hda_intel`. [Prepare](#prepare-devices) it to fix the first two.
Pools hand out ready devices only.

### Warnings

The topology reports what it notices. Glidex never acts on these.

| Warning | Meaning | What to do |
| --- | --- | --- |
| `iommu_off` | No IOMMU groups. | Turn on VT-d or AMD-Vi and the kernel option; see [PCI passthrough](pci-passthrough.md#iommu). |
| `numa_unknown` | A device has no NUMA node (firmware reports `-1`) on a host with several. | A firmware setting or update. `NUMA_POLICY_STRICT` VMs can't have it. |
| `group_shared` | A device shares its IOMMU group with a device of another class. | Passing one through takes the other; move the card to another slot. |
| `link_degraded` | A device's PCIe link runs slower or narrower than it can. GPUs aren't flagged: they slow their link when idle. | Reseat the card, or check the slot. |
| `hugepages_unbalanced` | A device sits on a NUMA node with no hugepages while other NUMA nodes have some. | Reserve hugepages on that NUMA node too (see [Hugepages](#hugepages)). |

## NUMA nodes

On a host with several NUMA nodes, a VM is faster when its memory, its
vCPUs and its devices are on one of them. A VM asks for that with
`numa_policy` (see
[NUMA and dedicated CPUs](../user-guide/vms.md#numa-and-dedicated-cpus)):

- `NUMA_POLICY_PREFERRED`: on one NUMA node if one fits, else anywhere
  on the host. Then the VM's `NumaAligned` condition is `False`.
- `NUMA_POLICY_STRICT`: on one NUMA node, or not on that host. It
  refuses devices on two NUMA nodes and devices with no NUMA node.

Glidex picks the NUMA node with most of the VM's devices, then the
least allocated, then the lowest number. Each NUMA node's room is its
memory times `cluster.scheduler.memory_overcommit`, and its hugepages
of each size. VMs bound to a NUMA node count there; VMs without a
policy count on every NUMA node in proportion to its size.

The guest's memory is bound to the NUMA node. Shared vCPUs still float
over the whole host, so no node's CPUs sit idle. The VM shows where it
went: `numa_node`, and for dedicated vCPUs `cpus`.

In a cluster, a strict VM that fits no node waits as **Unschedulable**
with each node's reason (`NUMA node 1: not enough memory (… of … MiB
taken)`, `its PCI devices are on NUMA nodes 0 and 1`). On a single host
the create is refused with `409`. A change to a placed VM's vCPUs,
memory, policies, devices or networks is fitted again on its node;
what no longer fits is refused with `409`.

## Dedicated CPUs

A VM with `cpu_policy: CPU_POLICY_DEDICATED` gets a host CPU of its own
for each vCPU, on one NUMA node if it can (it implies
`NUMA_POLICY_PREFERRED`). Its vCPU threads are pinned there; other VMs
don't run on them.

- **Whole cores.** By default a dedicated VM takes both hyperthreads of
  each core, so no other VM shares a core with it. Turn this off with
  `cluster.scheduler.dedicated_whole_cores: false` to hand out single
  threads.
- **Isolated CPUs first.** CPUs the kernel keeps tasks off (`isolcpus`,
  `nohz_full`) are used first.
- **Reserved CPUs are never given out:** OVS-DPDK's PMD and lcore CPUs,
  and by default the first core of each NUMA node, for the host.
  `cluster.node_reserved.cpu_list` replaces that default with your own
  list; an empty list reserves none.
- **Shared VMs give way.** Once a node holds dedicated CPUs, VMs with
  shared vCPUs run only on the CPUs left, and running ones are moved
  when that set changes.

```json
{
  "cluster": {
    "scheduler": { "dedicated_whole_cores": true },
    "node_reserved": { "cpu_list": "0-1,64-65" }
  }
}
```

`cluster.node_reserved.cpu_list` is per host: set it in each host's
`/etc/glidex/control-plane.json`. The reserved CPUs show in
`gxctl node topology`. Restart the control plane after changing either
key.

<!-- TODO(verify): dedicated_whole_cores is read by the scheduler on the leader; confirm it must be set the same on every server. -->

When no NUMA node has enough free CPUs, the reason says so
(`NUMA node 0: not enough free CPUs (8 dedicated vCPUs, 6 CPUs free)`).

## Hugepages

Hugepages are their own resource, counted per node and page size.
Glidex doesn't reserve them; the kernel does, at boot or later:

```text
# kernel command line: reserve 1G pages at boot, before memory fragments
default_hugepagesz=1G hugepagesz=1G hugepages=64
```

```bash
# 2M pages, at run time, on NUMA node 1
echo 4096 | sudo tee /sys/devices/system/node/node1/hugepages/hugepages-2048kB/nr_hugepages
```

The node reads its pools per size and NUMA node (again within two
minutes of a change). How they are counted:

- **What VMs may take** is the pool less what OVS-DPDK takes for
  `dpdk-socket-mem`. Hugepages aren't ordinary memory: a node's
  allocatable memory leaves the pool out.
- **A VM with `hugepages`** takes its whole memory in pages of its
  `hugepage_size` (`2M` or `1G`; default: the host's default size,
  usually 2M). They are never overcommitted, and a placed VM holds them
  whether it runs or not. Refusals: `not enough 2M hugepages (2048 of
  2048 MiB taken)`, `no 1G hugepages`, `1536 MiB is not a whole number
  of 1G pages`.
- **A VM with a vhost-user NIC** needs shared memory. It gets 2M pages
  without asking when the node has its memory plus a quarter (at least
  256 MiB) to spare, and ordinary memory otherwise.
- **The VM's `page_kib`** says what it got: 4 for ordinary memory, else
  the page size in KiB.
- **Before a VM starts**, its node checks that enough pages of the size
  are free (on its NUMA node, if it has one). If not, the start fails
  with the counts. Hugepages used outside Glidex show up this way.

On a single host, Glidex records the size of asked-for hugepages but
doesn't count them ahead; the check at start is what stops a VM. The
size of vhost-user hugepages is decided at start too.

<!-- TODO(verify): on a single host, do VMs with a NUMA or CPU policy have their hugepages counted against the NUMA node at create (standalone_fit)? -->

## Device pools

A pool is a named set of devices, matched by ids or addresses. Instead
of naming devices by address, a VM asks for a number of devices from a
pool, and Glidex picks them. Pools are defined in
`/etc/glidex/control-plane.json` (on each server, the same):

```json
{
  "pci": {
    "pools": [
      { "name": "a100", "class": "gpu",
        "match": { "vendor_id": "0x10de", "device_id": "0x20b0" },
        "prepare": true },
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

| Key | Meaning |
| --- | --- |
| `name` | The pool's name. Unique. |
| `class` | A label for people and usage reports: `gpu`, `nic`, `nvme`, `dpu`, `accelerator`. |
| `match` | `vendor_id`, `device_id`, `class_code` (or its start, `0x0302`) and `addresses`. Every key given must match. A pool must have a match. |
| `nodes` | Node names or ids the pool is on. Default: every node. |
| `prepare` | Each node binds its devices of the pool to `vfio-pci` when glidex starts (see [Prepare devices](#prepare-devices)). |

The control plane refuses to start with two pools of the same name, a
pool without a match, two pools with the same match on the same nodes,
or a grant naming an unknown pool. Restart the control plane after
changing pools or grants.

### Who may use a pool

A `pci.allow` entry with `pool` grants the pool to projects (ids or
names) or tenants (ids or slugs), like device grants (see
[Who may use a device](pci-passthrough.md#who-may-use-a-device)).
`net-admin` and `system-admin` may use any pool. In policies the
permission is `usePciPool`.

### How VMs get devices

A VM asks with `devices`, for example `[{"pool": "a100", "count": 2,
"same_switch": true}]` (in `gxctl create`, `a100:2:switch`; in the
web UI's VM form, **Device Pools**). Glidex picks devices of the pool
that are ready, not cordoned and not held by another VM (whole IOMMU
groups counted), on the NUMA node of the VM's other devices first, then
by address. With `same_switch`, all of them sit under one PCIe switch,
the one the VM already uses if it can.

The picks are shown on the VM as `pool_devices` and are passed through
at its next start. They are claims like devices named by address: the
VM keeps them while it exists. When a pool is short, the reason says
why: `pool a100: 1 of 2 free`, `pool a100: no 2 free devices under one
PCIe switch`, `pool a100 is not on this node`. In a cluster the VM
waits for a node; on a single host the request is refused with `409`.

### See what pools hold

```bash
gxctl pci pools                 # each pool's devices per node: free, held by a VM, cordoned or not ready
gxctl pci pools --project lab   # the pools project lab may use, with free counts
```

The full listing (`GET /pci-pools`) needs a host role. With
`--project` (`GET /pci-pools?project=<p>`), anyone who can read the
project sees the pools it may use and how many devices are free,
without addresses or holders; it lists the project's tenant pools too.

### Quotas

A project quota `devices.<pool>` limits how many devices of a pool the
project's VMs ask for, counted like `vcpus` over all its VMs:

```bash
gxctl project quota lab devices.a100=2
```

Over the limit, a request is refused with `quota_exceeded` (resource
`devices.a100`). A tenant's cap limits the same over all its projects
(`tenant_quota_exceeded`). A pool without a quota is unlimited. In the
web UI, the project's **Quotas and usage** card shows **Devices:
&lt;pool&gt;** for each pool with a limit or in use, and **Edit quotas**
offers a field for each pool the project may use. See
[Projects](../user-guide/projects.md#quotas).

### Tenant pools

A tenant granted a pool can split its share among its projects. Its
`tenant-admin` creates **tenant pools**, each drawing from a cluster
pool, with a maximum and the projects that may use it:

```bash
gxctl tenant device-pool set acme team-a-gpus --from a100 --max 4 --projects team-a,team-a-ci
gxctl tenant device-pools acme                 # pool, cluster pool, held/max, free, projects
gxctl tenant device-pool remove acme team-a-gpus
```

In the web UI it is the tenant page's **Device pools** card. Over the
API, `PUT /tenants/{id}/device-pools` replaces the tenant's whole set.

- The name is 1-63 lowercase letters, digits and dashes, and can't be a
  cluster pool's name. `--from` must be a cluster pool granted to the
  tenant, `--max` at least 1, and the projects the tenant's.
- A VM of one of those projects asks for `{"pool": "team-a-gpus"}`. It
  gets devices of `a100`, and the VM shows `"pool": "a100",
  "tenant_pool": "team-a-gpus"`. Other projects are refused (`403`).
- The tenant pool's projects together ask for at most `max` devices
  (`tenant_quota_exceeded`, resource `tenant_pool.team-a-gpus`). The
  tenant's cap and the project quota on `a100` still apply.
- If the tenant loses its grant on `a100`, the tenant pool shows
  `ungranted` and gives nothing new. VMs that hold devices keep them,
  as do VMs of a tenant pool you remove.

### Usage

Metering counts `device.alloc.<pool>`: the devices of a pool a VM holds,
in device-hours, from its start to its exit (paused too). Devices of a
tenant pool count under the cluster pool's name. Devices named by
address aren't metered. See [Usage](../user-guide/usage.md).

## Prepare devices

Glidex can bind a device to `vfio-pci` for you, on any node:

```bash
gxctl node prepare-device <node> 0000:41:00.0   # its whole IOMMU group to vfio-pci
gxctl node release-device <node> 0000:41:00.0   # back on the drivers it had
```

In the web UI, the node's **Topology** card has **Prepare** and
**Release** next to each device. Over the API:
`POST /nodes/{id}/devices/{address}/prepare` and `.../release`.

Preparing:

- binds every function of the device's IOMMU group to `vfio-pci`.
  Bridges stay with the host;
- remembers the drivers they had, for release;
- writes `/etc/udev/rules.d/90-glidex-vfio-<group>.rules`, which gives
  `/dev/vfio/<group>` to the `glidex` group with mode `0660`, and
  applies it at once;
- refuses a group holding a NIC the host uses (one with addresses or
  routes), an OVS-DPDK uplink, or a device without an IOMMU group.

The host's driver lets go of the devices: a GPU on `nvidia` stops being
the host's. Release works with any function of the group, and refuses a
device Glidex didn't prepare.

Preparing and releasing need `net-admin` or `system-admin` on that
host. A prepared device goes back to its usual driver at reboot. For
devices that should always be ready, give their pool `prepare: true`:
each node then prepares its devices of the pool that aren't ready when
glidex starts, skipping devices without an IOMMU group and OVS-DPDK
uplinks. A device that can't be prepared stays not ready, and the
control plane's log says why.

## Device health and cordoning

Each node reads its devices' PCIe error counters (AER: correctable,
non-fatal, fatal). `gxctl node topology <node> --devices` shows them as
`aer correctable/nonfatal/fatal`, and the **Topology** card marks a
device with uncorrectable errors (**PCIe errors**).

Glidex watches the devices of running VMs. If one falls off the bus, or
has new uncorrectable errors since the VM started, the VM's
`DeviceHealthy` condition turns `False` with the reason, and a
`DeviceError` event is recorded once (`gxctl events <vm>`). Glidex
doesn't stop the VM or take the device away. Errors inside the guest
(an NVIDIA Xid, say) only the guest sees.

To keep a device you don't trust from new VMs, cordon it:

```bash
gxctl node cordon-device <node> 0000:41:00.0
gxctl node uncordon-device <node> 0000:41:00.0
```

Or **Cordon** and **Uncordon** on the **Topology** card, where a
cordoned device is marked **cordoned**. Over the API:
`POST /nodes/{id}/devices/{address}/cordon` and `.../uncordon`.

A cordoned device is refused to new VMs by address
(`PCI device 0000:41:00.0 is cordoned`), and pools skip it. The VM that
holds it keeps it. Cordoning needs `net-admin` or `system-admin` on
that host. The node records its cordoned devices, so they stay cordoned
across restarts.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| `not enough 2M hugepages (… of … MiB taken)` | The node's pool of that size is used up. Reserve more, or use another size. `gxctl node topology <node>` shows the pool per NUMA node. |
| `no 1G hugepages` | That node has none of that size. 1G pages are reserved on the kernel command line. |
| The start fails with hugepage counts | Pages the scheduler counted as free are in use outside Glidex (another process, or DPDK beyond `dpdk-socket-mem`). |
| `NumaAligned` is `False` | A `PREFERRED` VM didn't fit on one NUMA node and spans the host. Free room on one NUMA node, then stop and start the VM. |
| `its PCI devices are on NUMA nodes 0 and 1`, `PCI device … has no NUMA node` | A `STRICT` VM can't have these devices together. Use `PREFERRED`, or devices on one NUMA node. |
| `not enough free CPUs (… dedicated vCPUs, … CPUs free)` | Dedicated VMs hold the rest, or the CPUs are reserved. See `reserved cpus` in `gxctl node topology`. |
| `pool …: 1 of 2 free` | Other VMs hold the pool's devices, or some aren't ready or are cordoned: `gxctl pci pools`. |
| `no device pool …` (`400`) | No such pool, or a tenant pool of another tenant. |
| `forbidden` asking for a pool | The project has no grant for it in `pci.allow`, or isn't one of the tenant pool's projects. |
| `PCI device … is cordoned` | An administrator cordoned it: `gxctl node uncordon-device`. |
| A VM shows `DeviceHealthy=False` | The device fell off the bus or had PCIe errors: `gxctl node topology <node> --devices`, and the host's `dmesg`. |
| Preparing is refused | The group holds a NIC the host uses or an OVS-DPDK uplink, or the IOMMU is off. |
