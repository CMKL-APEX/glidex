# Host topology and device placement

Status: **design**. Nothing here is implemented. Extends
[clustering.md](clustering.md) §9.1 (scheduler),
[hypervisors.md](hypervisors.md) (command lines),
[data-model.md](data-model.md) (`VmConfig`) and
[tenancy.md](tenancy.md) §20 (accelerators, which points here). §14
lists the edits it needs in those documents.

Glidex passes host PCI devices through to VMs (VFIO), and a cluster
places VMs on nodes. But neither a node nor the scheduler knows how a
host is built. A node reports a CPU count, its total memory, host-wide
hugepage counts and a flat list of PCI addresses. A GPU on socket 1 can
go to a VM whose vCPUs run on socket 0 and whose memory sits on
socket 0. Every DMA and interrupt then crosses the socket link. Two GPUs
meant to talk peer-to-peer can sit under different root complexes. A
NIC meant for a VM's RDMA can be on the other socket from that VM's GPU.

This document adds a **host topology model**. Each node describes its
NUMA nodes, CPUs, memory and the PCIe tree under them. On top of it come
NUMA-aware VMs, device placement in the scheduler, and device classes
and pools for GPUs, DPUs and NICs.

## 1. Scope

- **In:** reading and reporting a host's topology (§3, §4). Device
  readiness for passthrough (§5). NUMA policy and CPU pinning in the VM
  spec, and how both hypervisors apply it (§6, §7). Per-NUMA-node
  accounting and filters in the scheduler (§8). Device requests by class
  rather than address, and device pools that tenants are granted (§9).
  SR-IOV virtual functions as devices (§10).
- **Out:** mediated devices and vGPU (NVIDIA vGPU, Intel GVT-g, AMD
  MxGPU). They need vendor host drivers and licensing; §13. Live
  migration of VMs with devices. Changing a placed VM's NUMA binding
  (placement is sticky, clustering.md D6). GPU utilization metering
  (vendor tools, metering.md §16). Configuring firmware (ACS, SR-IOV
  enablement in BIOS, Resizable BAR).

## 2. Decisions

| # | Topic | Decision |
|---|---|---|
| H1 | Source | Each node reads its own topology from sysfs (`/sys/devices/system/node`, `/sys/devices/system/cpu`, `/sys/bus/pci/devices`) with no new host dependency (no `hwloc`, no `lspci`). Reading is pure over a filesystem view, as in `glidex_ovs::tuning::read_topology`, so tests fake any host. The two readers merge into one (`glidex-topology` crate). |
| H2 | Where it lives | Two levels. The **summary** (NUMA nodes, CPU sets, per-node memory and hugepages, and *assignable* devices with their NUMA node) is in the node's replicated `status`, because the scheduler needs it on the leader. The **full tree** (every PCI function, bridges, links) stays on the node and is served on request (§4.2). It keeps Raft state small, and the tree is only needed for display and diagnosis. |
| H3 | Device identity | A device is `(node, address)`. A VFIO claim, a grant and a scheduler match are all per node. Today's claim check compares the address across the whole cluster (§8.4). The VM spec keeps the sysfs path as written today. |
| H4 | Unknown NUMA | A device whose `numa_node` reads `-1` (one-node hosts, some firmware) is reported as `numa_node: null`. On a host with one NUMA node it counts as node 0. On a multi-node host it fits any NUMA policy except `strict`, and the node shows a `topology_warnings` entry. Glidex never guesses from the bus number. |
| H5 | VM NUMA policy | `numa: none \| preferred \| strict` (default `none`, today's behaviour). `preferred` aligns vCPUs, memory and devices on one host NUMA node when one fits and falls back otherwise. `strict` refuses a node where they can't all be on one NUMA node. One host NUMA node per VM in this design; spanning nodes with a guest NUMA topology is §13. |
| H6 | CPU policy | `cpu: shared \| dedicated` (default `shared`). `shared` vCPUs float over the NUMA node's (or host's) shareable CPUs and count against `cpu_overcommit`. `dedicated` pins each vCPU to its own host CPU, exclusively, never overcommitted. |
| H7 | Reserved CPUs | Never given to `dedicated` vCPUs, nor counted as shareable: the OS core OVS-DPDK reserves (`dpdk-lcore-mask`), PMD cores (networking.md §3), and `node.reserved_cpus` from the control-plane config. `isolcpus`/`nohz_full` CPUs are *preferred* for dedicated vCPUs, not reserved. |
| H8 | Who allocates CPUs | The **scheduler** picks the node and NUMA node and writes the exact CPU set into `status.placement`. The node never chooses CPUs itself. The leader is the one writer, so two VMs can't take the same CPU. A standalone host runs the same code locally. |
| H9 | Mechanism | Process-level binding through the VM's systemd unit (`CPUAffinity=`, `NUMAPolicy=bind`, `NUMAMask=`) for every VM with a policy. Per-vCPU pinning on top: Cloud Hypervisor's `--cpus affinity=`, and for QEMU the shim pinning vCPU threads it reads from QMP `query-cpus-fast`. Memory comes from the hypervisor's own NUMA-bound memory (§7). |
| H10 | Device classes | Requests by class (`gpu`, `nic`, `nvme`, `dpu`, `accelerator`) are matched against **pools** the operator defines, not raw PCI class codes. A pool names devices by vendor and device id, address or label, per node or cluster-wide (§9). Raw addresses keep working. |
| H11 | Grants | Pools are what tenants and projects are granted (`pci.allow` gains `pool`). Today's address grants stay (tenancy.md D25). |
| H12 | Preparation | Glidex reports whether each device is ready (§5) and can bind a device to `vfio-pci` through netd, reusing `glidex_ovs::nic::bind_driver`, on request by `net-admin` or for a pool with `prepare: true`. It never changes the kernel command line or firmware. |
| H13 | Who sees topology | The full tree and every node's summary need a host role (`auditor`, `net-admin`, `system-admin`), the same as `/pci-devices`. A project sees only what its VMs hold (the device's class, model and NUMA node), and the pools it may request with their free counts per cluster, not per node. |
| H14 | Refresh | A node reads its topology at start, on a udev `pci`/`vfio` event batch (debounced 5 s), on `gxctl node topology refresh`, and every ten minutes with the rest of node status. A status write happens only when the summary changes. |

## 3. Model

### 3.1 Summary (replicated, in `nodes/<id>.status.topology`)

```rust
pub struct TopologySummary {
    /// Hash of the full tree (§4.2); changes whenever anything does.
    pub digest: String,
    pub numa_nodes: Vec<NumaNode>,
    /// CPUs no VM vCPU uses, and why (H7).
    pub reserved_cpus: BTreeMap<u32, ReservedFor>,  // pmd | dpdk_lcore | config
    /// Devices that could be passed through (§3.3), by address.
    pub devices: Vec<AssignableDevice>,
    /// Firmware or configuration gaps found while reading (§5.2).
    pub warnings: Vec<TopologyWarning>,
}

pub struct NumaNode {
    pub id: u32,
    /// Online CPUs on this node, as a list (`0-15,64-79`).
    pub cpus: CpuSet,
    /// Physical cores: each core's CPUs, the first one first.
    pub cores: Vec<CpuSet>,
    pub memory_mib: u64,
    /// Page size in KiB → pages reserved on this node.
    pub hugepages: BTreeMap<u32, u64>,
    /// SLIT distances to every node, by node id (`distance` in sysfs).
    pub distance: Vec<u32>,
}

pub struct AssignableDevice {
    pub address: String,              // 0000:41:00.0
    pub vendor_id: String,            // 0x10de
    pub device_id: String,            // 0x2330
    pub class: DeviceClass,           // gpu | nic | nvme | dpu | accelerator | other
    pub class_code: String,           // 0x030200
    pub numa_node: Option<u32>,       // H4
    pub iommu_group: Option<u32>,
    /// Other functions in its IOMMU group that are not bridges: they go
    /// with it (§5.1).
    pub group_peers: Vec<String>,
    pub driver: Option<String>,
    pub ready: bool,                  // §5
    /// Root port it hangs off, and the switch, if any (§3.2).
    pub root_port: Option<String>,
    pub switch: Option<String>,
    pub link: Option<PcieLink>,       // current and max speed and width
    pub sriov: Option<SriovInfo>,     // §10
    /// Operator labels from pools and `gxctl node label-device` (§9).
    pub labels: BTreeMap<String, String>,
}
```

Allocation state (which CPUs, hugepages and devices are taken) is not in
the summary. The scheduler derives it from the placements of the VMs on
the node (§8.1), as it derives `Load` today.

### 3.2 Full tree (on the node, `GET /nodes/{id}/topology`)

The summary plus every PCI function, in tree form:

```text
numa 0  cpus 0-31,64-95  memory 257 GiB  hugepages 2M×4096 1G×0
└─ root complex 0000:00 (pci0000:00)
   ├─ root port 0000:00:01.1  Gen5 x16
   │  └─ switch 0000:01:00.0 → 0000:02:00.0, 0000:02:01.0
   │     ├─ 0000:03:00.0  gpu  10de:2330  vfio-pci  group 31   Gen5 x16
   │     └─ 0000:04:00.0  nic  15b3:1021  mlx5_core group 32   Gen5 x16  (pf, 0/16 vfs)
   └─ ...
numa 1  ...
```

Each function node carries its `PciDeviceInfo` (what `/pci-devices`
returns today) plus `numa_node`, `parent`, `children`, `link`
(`current_link_speed`, `current_link_width`, `max_link_speed`,
`max_link_width`), `sriov` and `kind` (`endpoint`, `root_port`,
`switch_upstream`, `switch_downstream`, `bridge`).

Two devices are **peer-close** when they share a switch (or a root
port). The model carries this so §9 can ask for "2 GPUs under one switch"
or "a GPU and a NIC under one switch". GPUDirect and peer-to-peer work
across a switch without crossing the root complex. Glidex reports the
fact and does not judge whether P2P actually works (that depends on ACS,
the IOMMU and the vendor driver).

### 3.3 Assignable devices

The summary lists a function when it is an endpoint (`kind = endpoint`),
not a bridge, and one of these holds:

- its class is display (`0x03`), network (`0x02`), mass storage NVMe
  (`0x0108`), processing accelerator (`0x12`) or coprocessor (`0x0b40`);
- it is bound to `vfio-pci`;
- it is named by a pool or a `pci.allow` entry.

`DeviceClass` comes from the first rule that matches: a pool's `class`;
display → `gpu`; NVMe → `nvme`; a network function with a vendor and id
in the built-in DPU list (BlueField, Pensando, IPU; extendable in config)
→ `dpu`; other network → `nic`; `0x12`/`0x0b40` → `accelerator`; else
`other`.

Built-in lists only name families. The operator's pools are the real
source of truth for what a device is for.

### 3.4 Sources

| Field | sysfs |
|---|---|
| NUMA nodes, CPUs | `/sys/devices/system/node/node<N>/cpulist`, `/sys/devices/system/node/online` |
| Cores | `/sys/devices/system/cpu/cpu<N>/topology/thread_siblings_list`, `core_id`, `physical_package_id` |
| Memory per node | `/sys/devices/system/node/node<N>/meminfo` (`MemTotal`) |
| Hugepages per node | `/sys/devices/system/node/node<N>/hugepages/hugepages-<kB>kB/nr_hugepages` |
| Distances | `/sys/devices/system/node/node<N>/distance` |
| Isolated CPUs | `/sys/devices/system/cpu/isolated`, `nohz_full` |
| Device NUMA node | `/sys/bus/pci/devices/<bdf>/numa_node` |
| Parent | the `realpath` of `/sys/bus/pci/devices/<bdf>`: its path components are the bridges above it |
| Link | `current_link_speed`, `current_link_width`, `max_link_speed`, `max_link_width` |
| Port type | the PCIe capability's device/port type, from `config` (offset of capability `0x10`, bits 4–7); a bridge's `class` is `0x0604` |
| SR-IOV | `sriov_totalvfs`, `sriov_numvfs`, `physfn`, `virtfn<N>` |
| IOMMU group | `iommu_group` link; peers from `/sys/kernel/iommu_groups/<g>/devices` |

A host without `/sys/devices/system/node` (a kernel built without NUMA)
has one node, 0, holding every CPU and all memory.

## 4. Reporting

### 4.1 Node status

`nodes/<id>.status.topology` is the §3.1 summary. `Resources.hugepages`
stays (older readers) and equals the per-node sums. A node of an older
version reports no topology. The scheduler treats it as today: it fits
`numa: none` and `cpu: shared` only, with the reason "it reports no
topology" for anything else.

Size: a two-socket host with 8 GPUs and 4 NICs is about 2 KiB of summary.
A host listing several hundred SR-IOV VFs is bigger, so VFs are reported
as counts per PF (§10) and enumerated only in the full tree.

### 4.2 Full tree

- `GET /nodes/{id}/topology` (cluster) and `GET /host/topology`
  (standalone, and the answering node). A server forwards a request for
  another node to that node over the cluster's node API. JSON, the §3.2
  tree, plus `digest`.
- `GET /pci-devices` stays and gains `numa_node`, `root_port`, `link`
  and `ready` per device.
- RPC: `HostService.GetTopology` (grpc.md), same shape.

### 4.3 CLI and UI

- `gxctl node topology <node>` prints the §3.2 tree.
  `--devices` prints a table with one row per assignable device: address,
  class, model, NUMA node, IOMMU group, driver, ready, holder (the VM, if
  any). `gxctl pci` gains the NUMA and ready columns.
- The UI's node page gets a **Topology** card. NUMA nodes are columns
  showing CPUs (free, shared, dedicated, reserved), memory and hugepages,
  with their devices under each one and the VM holding each device.
  Warnings are shown above it. Clicking a device shows its tree path and
  IOMMU group.
- The Cluster page's node list gains a device summary column
  (`8 gpu · 4 nic`).

## 5. Readiness

### 5.1 Ready

A device is `ready` when all of these hold:

1. the host has an IOMMU (`features.iommu`) and the device has a group;
2. it is bound to `vfio-pci`;
3. every non-bridge peer in its IOMMU group is bound to `vfio-pci` too, or
   unbound;
4. `/dev/vfio/<group>` is readable and writable by `glidex`;
5. it is not a DPDK uplink of a glidex bridge, nor an interface carrying
   the host's own addresses (`glidex_ovs::nic::classify`).

Each failed check is a reason (`not bound to vfio-pci (driver: nvidia)`,
`IOMMU group 31 also holds 0000:41:00.1 on snd_hda_intel`, `/dev/vfio/31
is root:root 0600`). The reasons go in the full tree and in
`gxctl node topology --devices`. The scheduler refuses a device that is
not ready (§8.2), and its reason shows in `POST /scheduler/preview`.

Passing a device through takes its whole group. If a VM asks for one
function of a multi-function group, it is given the group's other
non-bridge functions as well (Cloud Hypervisor and QEMU need them bound,
not attached). A second VM can't take a peer. Claims are by group, not
by function (§8.4).

### 5.2 Warnings

The node reports these in `topology.warnings`. They are shown, never
acted on:

- `numa_unknown`: a multi-node host with devices at `numa_node = -1`;
- `iommu_off`: no IOMMU groups at all;
- `group_shared`: an assignable device shares its group with an endpoint
  of another class (often a missing ACS on a root port);
- `link_degraded`: `current_link_*` below `max_link_*` on an idle device
  (a GPU idles at Gen1 to save power, so only warned for non-GPUs);
- `hugepages_unbalanced`: hugepages on some NUMA nodes and not others
  while devices sit on the ones without;
- `pmd_without_devices`: OVS-DPDK PMDs on a NUMA node whose NICs are on
  another (networking.md §3).

### 5.3 Preparing a device

`POST /nodes/{id}/devices/{address}/prepare` (`gxctl node prepare-device
<node> <address>`), action `host.device.prepare`, under `net-admin` and
`system-admin`. It runs on the node through netd:

- sets `driver_override` to `vfio-pci` and rebinds the device and its
  group peers (`bind_driver`, as for DPDK NICs);
- writes a udev rule so the group device is `glidex:glidex 0660`. This
  fixes the manual step in the operator guide's *Let the `glidex` user
  open it*;
- remembers the previous driver, so `.../release` puts it back
  (`restore_driver`).

It is refused for a device holding host addresses, a DPDK uplink, or a
group with such a device. It does not persist across reboots by itself.
The installer gains `--vfio-devices <pool or addresses>` for the
`modprobe.d` / `driverctl` persistence. A pool with `prepare: true`
(§9.1) is prepared by each node at start.

## 6. VM spec

`VmConfig` gains:

```rust
pub struct Placement {
    /// H5. Default `none`.
    pub numa: NumaPolicy,             // none | preferred | strict
    /// H6. Default `shared`.
    pub cpu: CpuPolicy,               // shared | dedicated
    /// Device requests by pool (§9); `vfio_devices` (addresses) stays.
    pub devices: Vec<DeviceRequest>,
}

pub struct DeviceRequest {
    pub pool: String,                 // "h100", "cx7"
    pub count: u32,                   // default 1
    /// All of this request's devices under one PCIe switch (§3.2).
    #[serde(default)]
    pub same_switch: bool,
}
```

- `numa: strict` with `hugepages` takes the hugepages from the chosen
  NUMA node only.
- `cpu: dedicated` implies `numa: preferred` unless `strict` is given,
  and needs `vcpu_count` free, non-reserved CPUs (§8.1). When the node's
  cores have SMT siblings, it takes **whole cores** (both threads of a
  core to one VM, or the sibling left unused) so another VM's vCPU never
  shares a core. `vcpu_count` odd → one sibling stays idle and is
  counted as taken.
- A VM's devices, from `vfio_devices` and `devices`, define its NUMA
  node: every device must be on one node under `strict`. Under
  `preferred`, the node holding the most of them wins.
- `status.placement` gains the outcome, written by the scheduler (H8):
  `{node, numa_node, cpus: CpuSet?, devices: [address], at}`. `devices`
  is the concrete addresses picked for `devices` requests. The VM's page
  shows them. `cpus` is set for `dedicated` and is the NUMA node's
  shareable set for `shared` with a policy.
- A change to `placement` on an existing VM applies at its next start.
  The scheduler re-picks CPUs and devices only if the VM is stopped and
  the change needs it. The node stays (D6).
- Validation: `count` ≥ 1, a known pool, `vcpu_count` ≤ the largest
  NUMA node's CPUs under `strict`. A request the cluster can never fit is
  `400`. A request that just doesn't fit now waits as `Unschedulable`
  (clustering.md §9.1).

REST and RPC take the same fields under `placement`. The UI's VM form
gets them under **Placement**: a NUMA policy select, a dedicated CPUs
checkbox, and device rows (pool, count, same switch).

## 7. Applying it on the node

The VM controller turns `status.placement` into launch arguments. The
shim and the unit do the binding.

### 7.1 Unit

`glidex-vm@<id>.service` reads `CPUAffinity=` and `NUMAPolicy=` /
`NUMAMask=` from a per-instance drop-in the controller writes before
start (`/run/systemd/system/glidex-vm@<id>.service.d/placement.conf`).
For `numa: none` and `cpu: shared` there is no drop-in, so behaviour is
as today. The hypervisor's own threads (I/O, vhost) stay inside this set.

### 7.2 Cloud Hypervisor

```text
--cpus boot=N,max=N[,affinity=[0@[c0],1@[c1],…]]      # dedicated: one host CPU per vCPU
--memory size=0[,shared=on]
--memory-zone id=mem0,size=<M>M,host_numa_node=<n>[,hugepages=on][,shared=on]
--numa guest_numa_id=0,cpus=[0-<N-1>],memory_zones=[mem0]
```

Devices keep `--device path=…,id=_vfio_…`. One guest NUMA node, so no
`pci_segment` split is needed (§13 for multi-node guests).

### 7.3 QEMU

```text
-object memory-backend-memfd,id=mem,size=<M>M,share=on,host-nodes=<n>,policy=bind[,hugetlb=on]
-machine q35,memory-backend=mem
-numa node,nodeid=0,cpus=0-<N-1>,memdev=mem
```

`memory-backend=mem` already exists for shared memory (hypervisors.md).
The NUMA binding is two extra properties. QEMU has no per-vCPU
affinity option, so for `dedicated` the shim reads `query-cpus-fast`
after launch and calls `sched_setaffinity` on each `thread-id`. The shim
does this again after adoption, and hot-plug doesn't change it. Devices
keep `-device vfio-pci,host=…`. Telling the guest which NUMA node a
device is on (`pxb-pcie,numa_node=`) only matters for multi-node guests
(§13).

### 7.4 Checks at start

Before launching, the node checks that the placement still holds: the
CPUs are online and not reserved, the hugepages are free on that node,
and the devices are ready. If not, the VM gets `Scheduled=False` with the
reason and is not started. The scheduler re-places it (same node, new
CPUs or devices) if it can. The VM never silently starts unbound.

## 8. Scheduler

clustering.md §9.1 and `scheduler.rs` today: filters on pin, phase,
`Ready`, CPU and memory with overcommit, hugepages present anywhere,
hypervisor, architecture, physnets, OVN version, `br-int` datapath,
**VFIO address present on the node**, disks and node networks. The
score is least-allocated by `max(cpu share, memory share)`, then fewest
VMs, then name. It is a pure function, used both at admission and by the
leader's placement loop. This section extends it and keeps those
properties.

### 8.1 Load per NUMA node

`Load` gains a per-NUMA-node part, derived from the placements of the
VMs on the node, running or not, as today:

```rust
pub struct NumaLoad {
    pub shared_vcpus: u64,           // shared vCPUs bound to this node
    pub dedicated: CpuSet,           // CPUs held by dedicated vCPUs
    pub mem_mib: u64,
    pub hugepages: BTreeMap<u32, u64>,
    pub devices: BTreeSet<String>,   // addresses (and their group peers)
}
```

Unbound VMs (`numa: none`, `shared`) keep counting only in the node-wide
`Load`. For capacity, they are spread over the NUMA nodes in proportion
to each node's shareable CPUs and memory. The shareable CPUs of a NUMA
node are its CPUs minus reserved (H7) minus `dedicated`. Shared capacity
is `shareable × cpu_overcommit`.

### 8.2 Filters

Added after today's filters, in this order, each with a reason:

1. **Topology reported** when the VM has a policy or `devices`
   requests: `it reports no topology` (§4.1).
2. **Devices present and ready:** each `vfio_devices` address is on the
   node *and* `ready` (`0000:41:00.0 is not ready: not bound to
   vfio-pci`). This replaces today's "present" check.
3. **Devices free on this node** (§8.4): `0000:41:00.0 is used by VM x`.
4. **Pool requests:** enough free, ready devices of the pool on the node,
   or on one switch for `same_switch`:
   `pool h100: 1 of 2 free`.
5. **NUMA fit**, for `preferred` and `strict`: the candidate NUMA nodes
   are those holding every requested device (`strict`). For `preferred`,
   take the ones holding the most devices, then all. A candidate fits
   if it has `vcpu_count` shareable or dedicated CPUs as the policy
   needs, the memory, and the hugepages. `strict` with no candidate:
   `no NUMA node has the devices, 16 CPUs and 64 GiB together`.
   `preferred` with no candidate falls back to the whole node, and
   the VM gets condition `NumaAligned=False` with the reason.
6. **Dedicated CPUs** under `none`: any NUMA node with enough free whole
   cores; otherwise across nodes (only under `none`).

### 8.3 Score and choice

The node score stays least-allocated, computed on the chosen NUMA node's
shares rather than the whole node's. Ties as today. Within the node:

- **NUMA node:** the most requested devices, then the least allocated,
  then the lowest id.
- **CPUs** (`dedicated`): isolated CPUs first (H7), whole cores, lowest
  ids. The choice is deterministic, so the preview and the placement
  agree.
- **Devices** from a pool: same switch as the VM's other devices, then
  same root port, then lowest address. This keeps peer-close devices for
  requests that need them.

`POST /scheduler/preview` returns, per node, the NUMA node, CPUs and
devices it would take. The UI's Placement shows them.

### 8.4 Claims are per node

Today `device_claimed_by` (`state.rs`) refuses a device whose address any
other VM in the cluster holds. Two nodes each with a GPU at
`0000:41:00.0` (common: identical servers) can therefore hold one such
VM between them, not one each. With H3:

- At admission, a request names an address. The address is checked
  against the VMs **placed on the same node**, plus unplaced VMs pinned to
  that node. The claim check moves into the scheduler filter (§8.2 3),
  which knows the node.
- An unplaced VM with an address is not a claim on any node until it is
  placed. Two such VMs can wait at once; the scheduler places them one
  at a time on different nodes or leaves one `Unschedulable`.
- Claims are by IOMMU group (§5.1). Group peers held by a VM count as
  taken.
- Standalone hosts: one node, so the behaviour is as today.

This is a correctness fix independent of the rest and can ship first
(HT0).

### 8.5 Grants per node

clustering.md §9.1 already plans "each BDF is granted for that node
(`pci.allow[].node`)", and it is not implemented (clustering.md C4).
`pci.allow` entries gain `node` (an id or name, optional: every node)
and `pool` (§9). The `usePciDevice` check then takes the node as well
as the address. Cedar's `PciDevice` entity becomes
`PciDevice::"<node>/<address>"`, with parent `Host::"<node>"`, so a
role granted on one host (clustering.md §12.2) covers that host's
devices.

## 9. Pools and classes

### 9.1 Pools

A pool is a named set of devices that can be requested by count. It is
in the cluster config (`control-plane.json`, replicated with the rest of
`cluster`):

```json
{
  "pci": {
    "pools": [
      { "name": "h100", "class": "gpu",
        "match": { "vendor_id": "0x10de", "device_id": "0x2330" },
        "prepare": true },
      { "name": "cx7", "class": "nic",
        "match": { "vendor_id": "0x15b3", "device_id": "0x1021" },
        "nodes": ["gpu-01", "gpu-02"] },
      { "name": "bf3", "class": "dpu",
        "match": { "addresses": ["0000:c1:00.0"] }, "nodes": ["edge-01"] }
    ],
    "allow": [
      { "pool": "h100", "tenants": ["research"] },
      { "bdf": "0000:41:00.0", "node": "gpu-03", "projects": ["ml-team"] }
    ]
  }
}
```

- `match` takes `vendor_id` and `device_id`, `class_code`, `addresses`
  or node device labels (`labels`); every given key must match.
- `nodes` limits a pool to some nodes; absent means every node.
- A device belongs to at most one pool. Overlap is refused at load,
  naming both pools.
- A pool's free count per cluster is the number of ready, unclaimed
  devices over its nodes. Projects that may use the pool see this count.
- `GET /pci-pools`, `gxctl pci pools`: each pool with its devices per
  node, free counts and holders (holders need a host role).

### 9.2 Classes

`class` labels the pool for the UI and metering. It does not change
behaviour, except that `dpu` and `nic` pools may later carry networking
options (§10.2).

### 9.3 Quotas

Tenancy quotas gain `devices.<pool>` (count), checked in the same
admission write as CPU and memory (tenancy.md caps and quotas). This is
the minimum to make a shared GPU pool usable by several projects without
one taking all of it.

### 9.4 Metering

A placed VM holding devices produces `device.alloc` samples per pool
(device-hours), in the metering.md allocation model, like `cpu.alloc`.
This settles metering.md §16's open question for allocation.
Utilization stays out (vendor tools).

## 10. SR-IOV and DPUs

### 10.1 Virtual functions

A PF with `sriov_totalvfs > 0` is reported with `sriov: {total, enabled,
vf_driver}`. Its VFs are enumerated in the full tree. In the summary
they are counts per PF (§4.1). VFs inherit the PF's NUMA node.

- A pool can match VFs: `match.vf_of` names PFs by vendor and device id
  or address. Each enabled VF is then one pool device.
- Enabling VFs (`sriov_numvfs`) is a netd operation,
  `POST /nodes/{id}/devices/{pf}/sriov {vfs: N}` under `net-admin`. It
  is refused while any VF is held by a VM or the PF is a DPDK uplink.
- A VF passed through to a VM is a VFIO device like any other. Its MAC,
  VLAN and spoof checking are set on the PF through `ip link set <pf> vf
  <n> …`. That is networking, so it belongs to the network a VM's VF NIC
  is on, and this document leaves it to a later networking.md section
  (§13). Until then, VF NICs are plain passthrough with the PF's
  defaults.

### 10.2 DPUs

A DPU (BlueField, Pensando, IPU) appears to the host as one or more
network functions (and often a management function). In this design it
is a device of class `dpu`:

- **Passthrough** of a DPU function or its VFs to a VM works as for NICs.
- **DPU as the host's datapath**, with OVS offloaded to the Arm cores and
  representors on the host, is out of scope. It changes networking.md's
  bridges and netd, not placement.

## 11. Security

- Topology is security-relevant detail (device models, firmware gaps,
  free capacity per node). It is readable with a host role only (H13).
  Projects see their own VMs' devices and pool counts.
- Preparing a device and enabling VFs run as root in netd. The control
  plane only names an address, and netd re-checks it is not a host
  interface or DPDK uplink (§5.3). The udev rule netd writes covers named
  groups only, not every `vfio` device.
- Per-vCPU pinning by the QEMU shim uses `sched_setaffinity` on the
  shim's own children, inside the unit's `CPUAffinity`. It needs no new
  capability.
- A VM holding a device can DMA into its own memory only (IOMMU). Peer
  devices in one IOMMU group are given together (§5.1) and never to two
  VMs.

## 12. Compatibility

- VMs without `placement` behave as today. A node of an older version
  reports no topology and takes only such VMs (§4.1).
- `vfio_devices` (sysfs paths) stays the address form. `devices` (pools)
  is added.
- `GET /pci-devices` gains fields. The proto `PciDevice` gains optional
  `numa_node`, `root_port`, `link`, `ready` (buf breaking passes: new
  optional fields only).
- The per-node claim (§8.4) relaxes a refusal. No previously accepted
  request is refused.

## 13. Later

- **Multi-node guests:** a VM spanning host NUMA nodes with a matching
  guest NUMA topology (Cloud Hypervisor `--numa` per node with
  `pci_segments`, QEMU `-numa` per node with `pxb-pcie,numa_node=`), so
  a guest with 8 GPUs over two sockets sees which GPU is near which
  CPUs.
- **mdev and vGPU:** mediated devices as pool members, with vendor
  types and counts per parent.
- **SR-IOV VF networking:** VLAN, MAC, trust and spoof check from the
  VM's network (networking.md).
- **Hugepage reservation per NUMA node** by the installer, sized from
  pools and policy (today the installer reserves hugepages for DPDK
  only).
- **Topology-aware DPDK:** `glidex_ovs::tuning` consuming the same model
  and placing PMDs on the NUMA nodes of the NICs rather than of the
  hugepages.
- **IOMMU-backed P2P groups:** asking for a set of devices whose P2P is
  verified rather than just switch-local.

## 14. Edits to existing documents

| Document | Edit |
|---|---|
| clustering.md | §9.1 filters and score per §8; §9.1's VFIO filter points to §8.4 (claims per node) and §8.5 (`pci.allow[].node`, `pool`). |
| hypervisors.md | The arguments of §7.2 and §7.3 here; the QEMU shim's vCPU pinning after launch and adoption. |
| data-model.md | `VmConfig.placement`, `status.placement.{numa_node, cpus, devices}`, condition `NumaAligned`. |
| reconciliation.md | The placement drop-in (§7.1) and the start-time check (§7.4). |
| networking.md | netd ops `prepare_device`, `release_device`, `set_sriov_vfs`; Appendix B's "NUMA locality" note points here. |
| rest-api.md, grpc.md, cli.md, web-ui.md | §4.2, §4.3, §5.3, §9.1 endpoints and commands; the Topology card; Placement in the VM form. |
| security.md | `PciDevice::"<node>/<address>"`, action `host.device.prepare`. |
| tenancy.md | §20 points here (done); quotas `devices.<pool>`; `pci.allow` grants by pool. |
| metering.md | §16's GPU item: `device.alloc` per pool (§9.4). |
| operator guide | PCI passthrough: readiness, prepare, pools, NUMA policy. |

## 15. Milestones

| # | Scope | Acceptance |
|---|---|---|
| **HT0** Claims per node | §8.4: the claim moves into the scheduler, per node and by IOMMU group; `pci.allow[].node` (§8.5) | Scheduler unit tests: two nodes with the same address each take one VM; a group peer counts as taken; standalone behaviour unchanged (existing tests). |
| **HT1** Read and report | `glidex-topology` crate (merging `glidex_ovs::tuning::read_topology`); summary in node status; `GET /host/topology`, `/nodes/{id}/topology`; `/pci-devices` fields; `gxctl node topology`; readiness and warnings | Fixture sysfs trees (one socket; two sockets with switches; `numa_node = -1`; shared IOMMU group; SR-IOV PF); the OVS planner gives the same plans from the new reader; UI node page shows the card. |
| **HT2** NUMA and dedicated CPUs | `placement.numa`, `placement.cpu`; scheduler §8.1–8.3; unit drop-in; Cloud Hypervisor and QEMU arguments; QEMU shim vCPU pinning; start-time check | Unit tests per filter and per tie-break; argument tests for both hypervisors; host test on a two-socket machine: a `strict` VM with a GPU has its vCPUs, memory (`numastat -p`) and device on one node, for both hypervisors, and across a control-plane restart. |
| **HT3** Prepare | §5.3 prepare/release through netd, udev rule, installer `--vfio-devices` | netd tests with a fake sysfs; host test: prepare a NIC, pass it through, release it, host driver back. |
| **HT4** Pools | §9: pools, `devices` requests, `same_switch`, grants and quotas by pool, `device.alloc` metering, UI | Scheduler tests for pools across nodes and switches; quota refusal; metering ledger rows per pool. |
| **HT5** SR-IOV VFs | §10.1 VF enumeration, `sriov_numvfs` through netd, VF pools | Host test with an SR-IOV NIC: enable VFs, a pool of VFs, two VMs each with one, NUMA-aligned. |

HT0 is a fix and can merge alone. HT1 changes nothing for VMs. HT2 and
HT4 are the user-visible parts.

## 16. Open questions

- **Shared vCPUs under a NUMA policy:** binding a `shared` VM to one
  node's CPUs can strand capacity on the other node. Should `preferred`
  plus `shared` bind only memory, and leave CPUs free across the host?
- **Whole cores for `dedicated`:** always, or a per-cluster switch for
  hosts where SMT siblings are acceptable?
- **Pool scope:** pools in the cluster config (operator) only, or also
  per tenant on top of a cluster pool (a tenant splitting its GPUs among
  its projects)?
- **Reserved CPUs default:** reserve one core per NUMA node for the host
  when any VM is `dedicated`, or leave it to `node.reserved_cpus`?
- **Device health:** report Xid errors, AER counters or link retraining,
  or leave health to vendor tooling?
