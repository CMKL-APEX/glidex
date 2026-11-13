# Host topology and device placement

Status: **design; HT0, HT0b, HT1, HT1b, HT2, HT3 and most of HT4 implemented; HT6 (replicated and discovered pools, with discovered NICs, H24) implemented, its acceptance run on gpu-dev2 passed (2026-11-09)** (§15). §17 records
where the implementation differs from the design. Extends
[14-clustering.md](14-clustering.md) §9.1 (scheduler),
[04-hypervisors.md](04-hypervisors.md) (command lines),
[03-data-model.md](03-data-model.md) (`VmConfig`) and
[19-tenancy.md](19-tenancy.md) §20 (accelerators, which points here). §14
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
  Hugepage accounting per node, page size and NUMA node (§8a). Device
  health (§5.4). SR-IOV virtual functions as devices (§10).
- **Out:** mediated devices and vGPU (NVIDIA vGPU, Intel GVT-g, AMD
  MxGPU). They need vendor host drivers and licensing; §13. Live
  migration of VMs with devices. Changing a placed VM's NUMA binding
  (placement is sticky, 14-clustering.md D6). GPU utilization metering
  (vendor tools, 13-metering.md §16). Configuring firmware (ACS, SR-IOV
  enablement in BIOS, Resizable BAR).

## 2. Decisions

| # | Topic | Decision |
|---|---|---|
| H1 | Source | Each node reads its own topology from sysfs (`/sys/devices/system/node`, `/sys/devices/system/cpu`, `/sys/bus/pci/devices`) with no new host dependency (no `hwloc`, no `lspci`). Reading is pure over a filesystem view, as in `glidex_ovs::tuning::read_topology`, so tests fake any host. The two readers merge into one (`glidex-topology` crate). |
| H2 | Where it lives | Two levels. The **summary** (NUMA nodes, CPU sets, per-node memory and hugepages, and *assignable* devices with their NUMA node) is in the node's replicated `status`, because the scheduler needs it on the leader. The **full tree** (every PCI function, bridges, links) stays on the node and is served on request (§4.2). It keeps Raft state small, and the tree is only needed for display and diagnosis. |
| H3 | Device identity | A device is `(node, address)`. A VFIO claim, a grant and a scheduler match are all per node. Today's claim check compares the address across the whole cluster (§8.4). The VM spec keeps the sysfs path as written today. |
| H4 | Unknown NUMA | A device whose `numa_node` reads `-1` (one-node hosts, some firmware) is reported as `numa_node: null`. On a host with one NUMA node it counts as node 0. On a multi-node host it fits any NUMA policy except `strict`, and the node shows a `topology_warnings` entry. Glidex never guesses from the bus number. |
| H5 | VM NUMA policy | `numa: none \| preferred \| strict` (default `none`, today's behaviour). `preferred` aligns vCPUs, memory and devices on one host NUMA node when one fits and falls back otherwise. `strict` refuses a node where they can't all be on one NUMA node. One host NUMA node per VM in this design; spanning nodes with a guest NUMA topology is `aligned`, in [27-gpu-vms.md](27-gpu-vms.md) §4. |
| H6 | CPU policy | `cpu: shared \| dedicated` (default `shared`). `shared` vCPUs float over the NUMA node's (or host's) shareable CPUs and count against `cpu_overcommit`. `dedicated` pins each vCPU to its own host CPU, exclusively, never overcommitted. |
| H7 | Reserved CPUs | Never given to `dedicated` vCPUs, nor counted as shareable: the OS core OVS-DPDK reserves (`dpdk-lcore-mask`), PMD cores (09-networking.md §3), and CPUs kept for the host. By default, once a node holds any `dedicated` VM, the first physical core of each NUMA node is reserved for the host (the OVS-DPDK OS core counts as node 0's). `cluster.node_reserved.cpu_list` in the control-plane config overrides that default, including with an empty list. `isolcpus`/`nohz_full` CPUs are *preferred* for dedicated vCPUs, not reserved. |
| H8 | Who allocates CPUs | The **scheduler** picks the node and NUMA node and writes the exact CPU set into `status.placement`. The node never chooses CPUs itself. The leader is the one writer, so two VMs can't take the same CPU. A standalone host runs the same code locally. |
| H9 | Mechanism | Process-level CPU binding by the shim: it binds the hypervisor (every thread) before exec, and moves a running VM's threads on request (§7.1). Per-vCPU pinning on top: Cloud Hypervisor's `--cpus affinity=`, and for QEMU the shim pinning vCPU threads it reads from QMP `query-cpus-fast`. Memory comes from the hypervisor's own NUMA-bound memory (§7). (The design had systemd drop-ins; the control plane runs unprivileged and can't write them, §17.) |
| H10 | Device classes | Requests by class (`gpu`, `nic`, `nvme`, `dpu`, `accelerator`) are matched against **pools** the operator defines (or, for GPUs, accelerators and NICs, discovers: H21, H24), not raw PCI class codes. A pool names devices by vendor and device id, address or label, per node or cluster-wide (§9). Raw addresses keep working. |
| H11 | Grants | Pools are what tenants and projects are granted (`pci.allow` gains `pool`). Today's address grants stay (19-tenancy.md D25). |
| H12 | Preparation | Glidex reports whether each device is ready (§5) and can bind a device to `vfio-pci` through netd, reusing `glidex_ovs::nic::bind_driver`, on request by `net-admin` or for a pool with `prepare: true`. It never changes the kernel command line or firmware. |
| H13 | Who sees topology | The full tree and every node's summary need a host role (`auditor`, `net-admin`, `system-admin`), the same as `/pci-devices`. A project sees only what its VMs hold (the device's class, model and NUMA node), and the pools it may request with their free counts per cluster, not per node. |
| H15 | Shared vCPUs under a NUMA policy | `cpu: shared` with `numa: preferred` or `strict` binds **memory** (and hugepages) to the chosen NUMA node, not CPUs. The vCPUs float over every shareable CPU of the host, so binding never strands CPU capacity on one socket. Devices still decide the NUMA node. |
| H16 | Whole cores | Whether `dedicated` takes whole SMT cores is a per-cluster switch, `cluster.scheduler.dedicated_whole_cores`, default `true` (§6). A standalone host reads the same key from its config. |
| H17 | Pool scope | Two levels. The operator defines **cluster pools** (§9.1) and grants them to tenants and projects. A tenant granted a cluster pool can define **tenant pools** on top of it (§9.1a), splitting its share among its projects. A tenant pool never names more than its tenant's grant on the cluster pool. |
| H18 | Device health | Each node reports device errors (AER counters, link changes, reset failures, devices gone from the bus) and shows them on the device and on the VM holding it (§5.4). Reporting only: the scheduler does not refuse a device for its error history. An operator can cordon a device by hand. |
| H19 | Hugepages | Hugepages are accounted as their own resource, per page size and NUMA node: what the node reserved, what OVS-DPDK takes, and what placed VMs hold. They are never overcommitted and don't count as ordinary memory. The scheduler, not the node at start, decides when a vhost-user VM gets hugepages implicitly (§8a). |
| H14 | Refresh | A node reads its topology at start, on a udev `pci`/`vfio` event batch (debounced 5 s), on `gxctl node topology refresh`, and every ten minutes with the rest of node status. A status write happens only when the summary changes. |
| H20 | Where pools live | Cluster pools and grants (`pci.allow`, by pool and by address) are a **replicated cluster setting** (`Meta` key `pci_settings`, §9.1), written through the API, gxctl and the UI, so every server answers a request the same way. A server's `control-plane.json` keeps only what concerns that host (H23). The leader copies a file's `pci.pools` and `pci.allow` into the setting once; after that, file pools and grants are ignored with a warning. | The user's call (2026-11-09). HT4 read pools and grants from each server's file, so a pool set up on one server was unknown when another server led (`gpus: no device pool r9700` on gpu-dev2 with prism2 leading, 27-gpu-vms.md §12). The same migration as the OVN setting (16-clustering-ui.md §8a). |
| H21 | Discovered pools | Devices of class `gpu` or `accelerator` that no defined pool covers and no host excludes form a **discovered pool** per vendor and device id, across every node that has one, named `<vendor>-<device>` (`amd-7551`, `nvidia-2330`; §9.1b). Discovery only groups devices: it never binds a driver, and a device is ready as before (§5). NVMe drives and DPUs aren't discovered: they are mostly the host's own (NICs are, since H24). Nor are BMCs' display controllers (ASPEED `0x1a03`, Matrox `0x102b`): the server's console. | The user's call (2026-11-09): a GPU should be usable without writing config first. The slug is stable and unique, unlike a marketing name from `pci.ids`, which depends on the host's copy. |
| H22 | Who uses a discovered pool | Only holders of a host role with `usePciPool` (`host.devices`) until an operator grants or adopts it. A tenant never sees or gets a device because a card was plugged in. | The user's call (2026-11-09). |
| H23 | Host overrides | Each server's file may keep devices out of discovery and every pool on that host (`pci.exclude`), and set a pool's `bar_mib` and `prepare` for that host (`pci.overrides`). The node marks the devices it keeps `excluded` in its status, so the leader never reads another server's file. | The user's call (2026-11-09): the file overrides discovery and availability. Per host because what it covers is: a card kept for host work, and G17's safe window, which depends on the host's memory. |
| H24 | Discovered NICs | Network controllers (class `02xx`: Ethernet, InfiniBand) form discovered pools too, class `nic`: every function, the host's own ports among them, named as H21's (`15b3-1021`). A NIC's SR-IOV VFs form a pool per PF model, `<vendor>-<device>-vf` of the PF (`15b3-1021-vf`), matching `vf_of`. Discovery still binds nothing: a port on its host driver, and a PF whose VFs are enabled, are listed as not ready (§5), and adopting with `prepare: true` binds them. Only NICs' VFs are discovered. | The user's call (2026-11-09): every NIC function, the VF slug, and no binding by discovery. Taking a host's port stays impossible without an operator: discovery never prepares, and H22 keeps the pools to host roles until adopted or granted. |
| H25 | Tenant pools bind the tenant's grant | Once a tenant has tenant pools on a cluster pool, its tenant grant (`tenant_grants`) lets its projects use the cluster pool only through those tenant pools: a request naming the cluster pool itself is refused (`403`), `GET /pci-pools?project=` leaves it out, and `gpus` without a pool picks the tenant pool. A grant made to the project itself (`pci.allow` with `projects`) and host roles with `usePciPool` (`host.devices`) still allow it directly. VMs created before keep their requests. | Otherwise a tenant pool's `max` and project list bind nothing: any project of the tenant could ask for the cluster pool by name. |

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

### 3.5 The `glidex-topology` crate and network ports

The reading above (except glidex's pools and readiness) lives in its own
crate, `glidex-topology`, with no glidex dependency, so hyperstor reads
hosts the same way (hyperstor's D204, its `spec/26-networks.md` §29.3). The
control plane re-exports its types; nothing changes in the summary or
the tree.

- **`pci`:** the raw functions (`read_raw`) and each one's place
  (`places`: kind, root port, the nearest switch above it).
- **`numa`:** NUMA nodes with their CPUs and socket.
- **`net`:** every netdev on a PCI function (bonds, bridges and USB
  devices such as a BMC's have none): link layer (`type` 1 Ethernet, 32
  InfiniBand), `operstate`, `speed`, `mtu`, the bond or bridge it is in
  (`master`), its addresses and those of each master up its chain, port →
  bond → bridge (`getifaddrs`), and its RDMA
  port from `/sys/class/infiniband/<dev>/ports/<n>`: `link_layer`,
  `state`, `rate`, and for InfiniBand the subnet prefix (the first 64 bits
  of `gids/0`) and `sm_lid`, which together name the fabric. A RoCE
  port is found by `gid_attrs/ndevs/0`, a bond's member by its PCI
  function (only the first member carries the bond's RDMA device).
- **`Host::closeness`:** `switch` when any switch port is above both
  functions, however deep (on an HGX board a GPU and its NIC meet two
  levels up, `nvidia-smi topo`'s `PXB`), then `numa`, `socket`, `far`.

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
- RPC: `HostService.GetTopology` (18-grpc.md), same shape.

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
   the host's own addresses (`glidex_ovs::nic::classify`);
6. no process other than this host's glidex VMs has its group open
   (`in use by qemu-kvm-one (pid 37226)`): another hypervisor on the same
   host, libvirt or OpenNebula, holding it. netd answers
   `device_holders` from every process's open files (`/proc/<pid>/fd`),
   since libvirt gives QEMU a private `/dev` where `fuser` can't see it;
   glidex's own VMs run as the control plane's user and are left out.
   netd also refuses `release_device` and a VF-count change while a
   group involved is open.

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
  another (09-networking.md §3).

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

### 5.4 Health

Each node reads, per assignable device:

- AER counters: `aer_dev_correctable`, `aer_dev_nonfatal`,
  `aer_dev_fatal` (per error type);
- link changes: `current_link_speed` or `current_link_width` different
  from the last read while held by a VM, and link down;
- device gone: the address disappears from `/sys/bus/pci/devices` (a
  surprise removal, or a GPU that fell off the bus);
- reset failures: the hypervisor's VFIO errors at start, stop and
  hot-plug (from the shim's log), and a function-level reset that times
  out.

Deltas since the last read become **device events**:
`DeviceError {address, kind, count, detail}`. They go in the node's
events, and in the events of the VM holding the device
(`gxctl events <vm>`, the VM page). The full tree and
`gxctl node topology --devices` show the counters and the last error. A
fatal error or a missing device sets condition
`DeviceHealthy=False` on the VM holding it, with the device and the
error. Guest-side errors (NVIDIA Xid and other vendor errors) are only
visible inside the guest and stay out of scope.

No automatic action (H18). A device the operator no longer trusts is
cordoned with `gxctl node cordon-device <node> <address>`
(`POST /nodes/{id}/devices/{address}/cordon`, under `net-admin` and
`system-admin`), which the scheduler refuses for new placements
(`0000:41:00.0 is cordoned`). The VM holding it keeps it.

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
  and needs `vcpu_count` free, non-reserved CPUs (§8.1). With
  `dedicated_whole_cores` on (H16, the default) and SMT siblings on the
  node, it takes **whole cores**: both threads of a core go to one VM, or
  the sibling is left unused, so another VM's vCPU never shares a core.
  An odd `vcpu_count` leaves one sibling idle and counted as taken. With
  the switch off, single threads are given out, and the siblings of
  dedicated CPUs may go to other dedicated VMs.
- `cpu: shared` with a NUMA policy binds memory only (H15).
- A VM's devices, from `vfio_devices` and `devices`, define its NUMA
  node: every device must be on one node under `strict`. Under
  `preferred`, the node holding the most of them wins.
- `status.placement` gains the outcome, written by the scheduler (H8):
  `{node, numa_node, cpus: CpuSet?, devices: [address], at}`. `devices`
  is the concrete addresses picked for `devices` requests. The VM's page
  shows them. `cpus` is set for `dedicated` only. `numa_node` is set
  whenever a policy binds memory.
- A change to `placement` on an existing VM applies at its next start.
  The scheduler re-picks CPUs and devices only if the VM is stopped and
  the change needs it. The node stays (D6).
- Validation: `count` ≥ 1, a known pool, `vcpu_count` ≤ the largest
  NUMA node's CPUs under `strict`. A request the cluster can never fit is
  `400`. A request that just doesn't fit now waits as `Unschedulable`
  (14-clustering.md §9.1).

REST and RPC take the same fields under `placement`. The UI's VM form
gets them under **Placement**: a NUMA policy select, a dedicated CPUs
checkbox, and device rows (pool, count, same switch).

## 7. Applying it on the node

The VM controller turns `status.placement` into launch arguments. The
shim and the unit do the binding.

### 7.1 CPU binding

`launch.json` carries `cpu_affinity`, the CPUs the hypervisor and all its
threads run on, which the shim sets in the child before exec; a failure
fails the launch. The shim itself stays unbound.

- `dedicated`: the CPUs the VM holds (with whole cores, the idle siblings
  too). The hypervisor's own threads (I/O, vhost) stay inside that set.
- `shared`, once the node holds any dedicated VM: the shared pool, every
  online CPU less the reserved and the dedicated (§8.1). The controller
  compares a running shared VM's binding (`status.instance.cpu_affinity`)
  with the pool on each round, and asks the shim to move its threads
  (`set_affinity` on `shim.sock`) when they differ; back to every online
  CPU once no dedicated VM is left.
- Otherwise empty: today's behaviour.

Memory binding is the hypervisor's (§7.2, §7.3), not the process's: a
`shared` VM with a NUMA policy has its memory bound and its vCPUs
unbound (H15).

### 7.2 Cloud Hypervisor

```text
--cpus boot=N,max=N[,affinity=[0@[c0],1@[c1],…]]      # dedicated: one host CPU per vCPU
--memory size=0[,shared=on]
--memory-zone id=mem0,size=<M>M,host_numa_node=<n>[,hugepages=on,hugepage_size=<2M|1G>][,shared=on]
--numa guest_numa_id=0,cpus=[0-<N-1>],memory_zones=[mem0]
```

Devices keep `--device path=…,id=_vfio_…`. One guest NUMA node, so no
`pci_segment` split is needed (§13 for multi-node guests).

### 7.3 QEMU

```text
-object memory-backend-memfd,id=mem,size=<M>M,share=on,host-nodes=<n>,policy=bind[,hugetlb=on,hugetlbsize=<2M|1G>]
-machine q35,memory-backend=mem
-numa node,nodeid=0,cpus=0-<N-1>,memdev=mem
```

`memory-backend=mem` already exists for shared memory (04-hypervisors.md).
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

14-clustering.md §9.1 and `scheduler.rs` today: filters on pin, phase,
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

A `shared` VM with a NUMA policy counts its memory and hugepages on its
NUMA node and its vCPUs node-wide (H15). Unbound VMs (`numa: none`,
`shared`) keep counting only in the node-wide `Load`. For capacity, they are spread over the NUMA nodes in proportion
to each node's shareable CPUs and memory. The shareable CPUs of a NUMA
node are its CPUs minus reserved (H7) minus `dedicated`. Shared capacity
is `shareable × cpu_overcommit`.

### 8.2 Filters

Added after today's filters, in this order, each with a reason:

1. **Topology reported** when the VM has a policy or `devices`
   requests: `it reports no topology` (§4.1).
2. **Devices present, ready and not cordoned:** each `vfio_devices`
   address is on the node, `ready`, and not cordoned (§5.4)
   (`0000:41:00.0 is not ready: not bound to vfio-pci`). This replaces
   today's "present" check.
3. **Devices free on this node** (§8.4): `0000:41:00.0 is used by VM x`.
4. **Pool requests:** enough free, ready devices of the pool on the node,
   or on one switch for `same_switch`:
   `pool h100: 1 of 2 free`.
5. **NUMA fit**, for `preferred` and `strict`: the candidate NUMA nodes
   are those holding every requested device (`strict`). For `preferred`,
   take the ones holding the most devices, then all. A candidate fits
   if it has the memory and the hugepages and, for `dedicated`,
   `vcpu_count` free CPUs. A `shared` VM's vCPUs are checked node-wide
   (H15). `strict` with no candidate:
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

- A node's **claims** are the devices of the VMs placed on it: each VM's
  `vfio_devices` and the devices its instance still holds (a claim lasts
  until the instance is gone, as today). Devices are compared by PCI
  address (lowercase BDF), never by the sysfs path as written.
- An unplaced VM claims nothing. Two such VMs can wait at once; the
  scheduler places them one at a time (it holds the VM write lock and
  counts each placement before the next), on different nodes or
  leaving one `Unschedulable`.
- **Create:** on a standalone host, and in a cluster for a VM pinned to
  a node (`spec.node`), a device claimed on that node is refused with
  `409` (`PCI device … is used by VM …`), as today. In a cluster
  without a pin, the scheduler filter (§8.2 3) skips nodes where it is
  claimed, and the VM waits as `Unschedulable` if none is left.
- **Change** (`PATCH vfio_devices`, attach): for a placed VM, checked
  against its node's claims, other than its own, with `409`. For an
  unplaced VM, the scheduler checks when it places it.
- Claims are by IOMMU group (§5.1). Until HT1 reports topology, a node
  reports the IOMMU group of each non-bridge PCI function
  (`status.pci_groups`). A device whose group holds a claimed device
  counts as taken (`0000:41:00.1 shares IOMMU group 31 with
  0000:41:00.0, used by VM x`). A node of an older version reports no
  groups, and claims are then per address only.
- The scheduler's presence filter compares BDFs too. Before HT0 it
  compared the stored sysfs path with the node's bare addresses, so in a
  cluster a VM with VFIO devices fitted no node.

This is a correctness fix independent of the rest and can ship first
(HT0).

### 8.5 Grants per node

14-clustering.md §9.1 plans "each BDF is granted for that node
(`pci.allow[].node`)"; HT0b implements it. `pci.allow` entries gain `node` (an id or name, optional: every node)
and `pool` (§9). The `usePciDevice` check then takes the node as well
as the address. Cedar's `PciDevice` entity becomes
`PciDevice::"<node>/<address>"`, with parent `Host::"<node>"`, so a
role granted on one host (14-clustering.md §12.2) covers that host's
devices.

The check runs at admission, but for an unpinned VM the node is only
known when the scheduler places it, possibly later in the leader's
placement loop, where there is no caller to authorize. So admission
evaluates `usePciDevice` for each requested device on every node that
reports it, and keeps the nodes where every device is allowed:

- None → `403`, as today.
- All of them, or the caller's role covers the whole cluster → no
  constraint.
- Otherwise the set is stored with the VM, as `spec.device_nodes`, and
  the scheduler adds a filter: `devices not granted on this node`. It
  is recomputed whenever `vfio_devices` changes. A later change to
  `pci.allow` doesn't move or unplace a VM. For a VM still waiting, it
  takes effect when the VM's devices are next changed. This is HT0b.

## 8a. Hugepages

### 8a.1 Today

- A node reports `nr_hugepages` per page size, host-wide, as both
  `capacity.hugepages` and `allocatable.hugepages` (`node.rs`).
- `allocatable.memory_mib` is `MemTotal` minus `node_reserved`.
  `MemTotal` includes the hugepage pool, so memory reserved as
  hugepages is counted twice: as ordinary memory and as hugepages.
- The scheduler's filter for `hugepages: true` is "the node has some
  hugepages". It does not count what placed VMs hold.
- OVS-DPDK's `dpdk-socket-mem` takes hugepages per NUMA node from the
  same pool, and nothing deducts it.
- A VM with a vhost-user NIC is given hugepages at start when the host
  has 2 MiB pages to spare (`hugepages_available_for`: free minus
  reserved, keeping 25% and at least 256 MiB). The scheduler never knows,
  and the outcome depends on what is free at that moment. Since
  `config.hugepages` stays `false`, metering's hugepage correction
  (`mem.used` adds guest RAM when `hugepages`, sampler.rs) may not see
  it either.
- Page size: Cloud Hypervisor's `hugepages=on` and QEMU's
  `hugetlb=on` use the host's default size. 1 GiB pages are never chosen
  explicitly.

### 8a.2 Model

Per node, per NUMA node (§3.1 `NumaNode.hugepages`) and page size:

| Field | Meaning | Source |
|---|---|---|
| `total` | pages in the pool | `nr_hugepages` per NUMA node |
| `dpdk` | pages OVS-DPDK takes | `dpdk-socket-mem` (MiB per NUMA node), rounded up to pages of the size DPDK uses (its default size) |
| `reserved` | pages kept back for the host | `node.reserved_hugepages` in config, default none |
| `allocatable` | `total − dpdk − reserved` | |

`free` and `resv` (in use or promised now) are observations. They are in
the full tree (§4.2) and used by the start check (§8a.5), never
replicated, because they change all the time.

Node-wide sums stay in `Resources.hugepages` for older readers. Ordinary
memory changes to
`allocatable.memory_mib = MemTotal − hugepage pool (all sizes) − node_reserved.memory_mib`,
so a hugepage is counted once.

### 8a.3 VM

- `hugepages: bool` stays. `hugepage_size: "2M" | "1G"` is added
  (optional). Without it, the smallest size the node has allocatable
  pages of is used, so 2 MiB on most hosts.
- `mem_size_mib` must be a multiple of the page size (`400`, `memory
  must be a multiple of 1 GiB for 1G hugepages`).
- **Implicit hugepages** for vhost-user (09-networking.md §0) move from the
  node at start to the scheduler. If the chosen node has the allocatable
  hugepages, the placement records them. Otherwise the VM gets 4 KiB
  shared memory, as today's fallback. The 25% / 256 MiB headroom stays,
  applied to allocatable rather than free pages.
- `status.placement` gains `hugepages: {size_kib, pages, numa_node?}`,
  which is what the scheduler counted. The node launches with exactly
  that: Cloud Hypervisor `hugepage_size=`, QEMU `hugetlbsize=`. A VM
  placed before this change has none recorded and keeps today's
  behaviour until it is next stopped and started, when the node asks
  the scheduler for a placement update (same node, D6).
- Metering reads the placement's hugepages, so implicit ones are
  counted in `mem.used` too.

### 8a.4 Scheduler

- `Load` gains pages held per size (node-wide in HT1b, per NUMA node in
  HT2), from placed VMs running or not, like memory. Hugepages are
  never overcommitted (`memory_overcommit` does not apply).
- A hugepage VM counts against hugepages only, not against ordinary
  memory. Its non-RAM overhead stays in `node_reserved`.
- Filter: `not enough 2M hugepages (3072 of 4096 MiB taken)`; for an
  explicit size the node lacks: `no 1G hugepages`.
- Score: hugepage share joins the score as a third share,
  `max(cpu, memory, hugepages)`, for VMs that use them.
- `POST /scheduler/preview` shows hugepages taken and capacity per size.
- Under a NUMA policy (§8.2 5), the pages come from the chosen NUMA node
  (H15: also for `shared`).

### 8a.5 Start check

Before launch, the node checks that `free − resv` pages of the size (on
the NUMA node, when bound) cover the VM. If not, the VM is not started
and gets `HugepagesUnavailable` with the counts. Pages used outside
glidex (DPDK beyond `socket-mem`, another process) make the observed free
count fall below what the accounting expects. The node then shows the
warning `hugepages_in_use_outside` with the difference (§5.2).

### 8a.6 Reservation

Glidex does not change `nr_hugepages` at run time. The installer
reserves them (today for DPDK only; §13 sizes them for VMs per NUMA
node). The pool is read again on each refresh (H14), so an operator's
change shows up within ten minutes or on `gxctl node topology refresh`.

## 9. Pools and classes

### 9.1 Pools

A pool is a named set of devices that can be requested by count. Pools
and grants are the cluster setting `pci_settings`, in the replicated
`Meta` table (H20), with `updated_by` and `updated_at` as for the OVN
setting. Devices no pool covers may form discovered pools (§9.1b).

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
- An `allow` entry with `"tenants": ["*"]` grants a pool to every tenant
  with an admission on the cluster, an **open pool** (19-tenancy.md
  D80): VM requests and tenant pools (§9.1a) treat it as a grant to
  each, and the tenant's cap `devices.<pool>` bounds its share. `"*"`
  goes alone, on a pool.
- `GET /cluster/settings/pci` (`readCluster`) returns the pools and
  grants in force and their source (the setting, or this server's file
  until one is written); `PUT /cluster/settings/pci`
  (`configurePciPools`, in `host.cluster.critical`: system admins, with
  step-up) replaces them, checked as a file's pools are (overlap, an
  empty `vf_of`) and refused (`409`) when it removes or renames a pool
  that VMs' `devices` or `gpus` requests, or a tenant pool's `from`,
  name, listing them. `gxctl pci pool
  list|show|set|rm|adopt`, `gxctl pci grant …`; the Cluster page's
  Devices card.
- **Migration.** On the leader of a cluster with no `pci_settings`, the
  file's `pci.pools` and `pci.allow` are written to the setting once
  (`updated_by: "migrate:<node>"`). Once the setting exists, a server
  whose file still has pools or grants that differ logs once that they
  are ignored, naming H20. A standalone host has the setting too, in
  its own store, so joining a cluster changes nothing about where pools
  live.

### 9.1a Tenant pools

A tenant granted a cluster pool (by `pci.allow` with `pool` and
`tenants`) can split its share among its projects (H17). Its owners
(19-tenancy.md roles) define tenant pools in the tenant, per cluster:

```json
{ "name": "team-a-gpus", "from": "h100", "max": 4,
  "projects": ["team-a", "team-a-ci"] }
```

- `from` names a cluster pool the tenant is granted. `max` is how many
  devices of it the tenant pool's projects hold at once, in total.
- Members are by count, not specific devices: the devices stay the
  cluster's, and the scheduler picks them as for the cluster pool.
- The sum of a tenant's tenant pools' `max` may exceed the tenant's
  grant. The tenant's cap on the cluster pool (`tenant_grants` with
  `max`, and the `devices.<pool>` quota) is still checked as well, so
  oversubscribing tenant pools only shares the tenant's own share.
- A device request may name a tenant pool. Admission resolves it to the
  cluster pool and checks the tenant pool's `max` and the project's
  membership in the same write as quotas (§9.3).
- Once the tenant has a tenant pool on a cluster pool, its grant reaches
  its projects only through its tenant pools (H25): a request naming the
  cluster pool needs a grant to the project itself or a host role.
- `GET/PUT /tenants/{id}/device-pools` (RPC `TenantService`), under the
  tenant's owner role; `gxctl tenant device-pool …`. Each lists its
  cluster pool's free count, so projects see what they can get.
- A tenant pool on a cluster pool the tenant loses its grant on stays,
  holds nothing new, and shows `ungranted`.

### 9.1b Discovered pools and host overrides

A cluster with GPUs and no pools set up still has pools to request
from (H21). Every server works them out the same way from the
replicated node summaries, so they need no write:

- **Members.** Each summary device of class `gpu`, `accelerator` or
  `nic` (§3.3; `nic` since H24) that no defined pool's match covers on
  its node, and that its node doesn't exclude, joins the discovered pool
  of its vendor and device id. A NIC's VF joins the pool of its PF's
  vendor and device id, of VFs (`vf_of`); other VFs don't. Nor do BMCs'
  display controllers (ASPEED `0x1a03`, Matrox `0x102b`).
- **Name.** `<vendor>-<device>`: the vendor is `nvidia` (`0x10de`),
  `amd` (`0x1002`), `intel` (`0x8086`), else its four hex digits; the
  device is its four hex digits (`amd-7551`, `15b3-1021`); a pool of
  VFs adds `-vf` to its PF's (`15b3-1021-vf`). When a
  defined pool already has that name, no discovered pool is formed: the
  defined pool's match decides what is in it.
- **Behaviour.** `class` from the devices; no `per_device`, so a `gpus`
  request on one gives its own vCPUs and memory; `prepare: false`
  always, so a device on its host driver is listed as not ready and is
  never taken from the host; `bar_mib` automatic (G17) unless the host
  overrides it.
- **Who sees it.** `GET /pci-pools` lists it with `discovered: true`.
  `?project=` lists it only to a caller `usePciPool` allows, which for a
  discovered pool is a host role (H22).
- **Adoption.** `gxctl pci pool adopt amd-7551 [--name r9700]` (the
  Devices card's Adopt) writes a pool matching its vendor and device id
  to the setting, under the slug or a new name, then edited as any
  pool. Renaming is refused (`409`) while VMs request the slug, listing
  them. Granting a discovered pool adopts it under its slug first.

A server's own file says what only that host decides (H23):

```json
{
  "pci": {
    "exclude": [
      { "addresses": ["0000:43:00.0"] },
      { "vendor_id": "0x1a03" }
    ],
    "overrides": {
      "r9700": { "bar_mib": 16384 },
      "amd-7551": { "prepare": true }
    }
  }
}
```

- `exclude` entries are `match`es (§9.1). An excluded device is in no
  pool, defined or discovered, and a VM naming its address isn't placed
  on that node: the host keeps it. It stays in the summary with
  `excluded: true`, so operators see why.
- `overrides` set `bar_mib` and `prepare` for the named pool's devices
  on this host; the setting's values are the default elsewhere. G19's
  rule (the smaller `bar_mib` of two pools) still applies after it.
- The node marks each summary device it keeps `excluded`; the leader
  reads that, not the file, and never needs a host's `bar_mib` (only the
  node prepares devices). The node's own `prepare: true` pass and the
  installer's `--vfio-devices <pool>` (through the local control plane,
  a discovered slug included) apply the overrides.
- An override naming no pool is logged once, as a typo would be.

### 9.2 Classes

`class` labels the pool for the UI and metering. It does not change
behaviour, except that `dpu` and `nic` pools may later carry networking
options (§10.2).

### 9.3 Quotas

Tenancy quotas gain `devices.<pool>` (count), checked in the same
admission write as CPU and memory (19-tenancy.md caps and quotas). This is
the minimum to make a shared GPU pool usable by several projects without
one taking all of it.

### 9.4 Metering

A placed VM holding devices produces `device.alloc` samples per pool
(device-hours), in the 13-metering.md allocation model, like `cpu.alloc`.
This settles 13-metering.md §16's open question for allocation.
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
  is on, and this document leaves it to a later 09-networking.md section
  (§13). Until then, VF NICs are plain passthrough with the PF's
  defaults.

### 10.2 DPUs

A DPU (BlueField, Pensando, IPU) appears to the host as one or more
network functions (and often a management function). In this design it
is a device of class `dpu`:

- **Passthrough** of a DPU function or its VFs to a VM works as for NICs.
- **DPU as the host's datapath**, with OVS offloaded to the Arm cores and
  representors on the host, is out of scope. It changes 09-networking.md's
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
  guest NUMA topology. Designed in [27-gpu-vms.md](27-gpu-vms.md) §4–§5
  (`numa_policy: aligned`), with NVLink through Fabric Manager (§7–§9).
- **mdev and vGPU:** mediated devices as pool members, with vendor
  types and counts per parent.
- **SR-IOV VF networking:** VLAN, MAC, trust and spoof check from the
  VM's network (09-networking.md).
- **Hugepage reservation per NUMA node** by the installer, sized from
  pools and policy (today the installer reserves hugepages for DPDK
  only). §8a accounts for whatever is reserved.
- **Topology-aware DPDK:** `glidex_ovs::tuning` consuming the same model
  and placing PMDs on the NUMA nodes of the NICs rather than of the
  hugepages.
- **IOMMU-backed P2P groups:** asking for a set of devices whose P2P is
  verified rather than just switch-local.

## 14. Edits to existing documents

| Document | Edit |
|---|---|
| 14-clustering.md | §9.1 filters and score per §8; §9.1's VFIO filter points to §8.4 (claims per node) and §8.5 (`pci.allow[].node`, `pool`). |
| 04-hypervisors.md | The arguments of §7.2 and §7.3 here; the QEMU shim's vCPU pinning after launch and adoption. |
| 03-data-model.md | `VmConfig.placement`, `hugepage_size`, `spec.device_nodes`; `status.placement.{numa_node, cpus, devices, hugepages}`; conditions `NumaAligned`, `DeviceHealthy`, `HugepagesUnavailable`; node `status.pci_groups`. |
| 12-reconciliation.md | The placement drop-in (§7.1) and the start-time check (§7.4). |
| 09-networking.md | netd ops `prepare_device`, `release_device`, `set_sriov_vfs`; Appendix B's "NUMA locality" note points here. |
| 06-rest-api.md, 18-grpc.md, 01-cli.md, 07-web-ui.md | §4.2, §4.3, §5.3, §9.1 endpoints and commands; the Topology card; Placement in the VM form. |
| 11-security.md | `PciDevice::"<node>/<address>"`, actions `host.device.prepare` and `host.device.cordon`. |
| 19-tenancy.md | §20 points here (done); quotas `devices.<pool>`; `pci.allow` grants by pool; tenant pools (§9.1a). |
| 13-metering.md | §16's GPU item: `device.alloc` per pool (§9.4); `mem.used` from the placement's hugepages (§8a.3). |
| operator guide | PCI passthrough: readiness, prepare, pools, NUMA policy. |

## 15. Milestones

| # | Scope | Acceptance |
|---|---|---|
| **HT0** Claims per node | §8.4: claims per node and by IOMMU group (`status.pci_groups`), compared by BDF; the scheduler's presence filter by BDF | Scheduler unit tests: two nodes with the same address each take one VM; a group peer counts as taken; a sysfs path matches the node's address; standalone behaviour unchanged (existing tests). |
| **HT0b** Grants per node | §8.5: `pci.allow[].node`, `PciDevice::"<node>/<address>"` under `Host::"<node>"`, `spec.device_nodes` and its filter | Cedar tests for a per-node grant and a per-host role; an unpinned VM with a grant on one node is placed there. |
| **HT1** Read and report | `glidex-topology` crate (merging `glidex_ovs::tuning::read_topology`); summary in node status; `GET /host/topology`, `/nodes/{id}/topology`; `/pci-devices` fields; `gxctl node topology`; readiness and warnings | Fixture sysfs trees (one socket; two sockets with switches; `numa_node = -1`; shared IOMMU group; SR-IOV PF); the OVS planner gives the same plans from the new reader; UI node page shows the card. |
| **HT1b** Hugepages | §8a: pools per size and NUMA node in node status, DPDK's share deducted, ordinary memory without the pool, `hugepage_size`, accounting and filter node-wide, implicit vhost-user hugepages decided by the scheduler, placement-driven launch, start check, metering from the placement | Scheduler tests: hugepage VMs fill the pool and not memory; DPDK's share is excluded; vhost-user VMs fall back to 4 KiB when full. Argument tests for both sizes on both hypervisors. |
| **HT2** NUMA and dedicated CPUs | `placement.numa`, `placement.cpu`; scheduler §8.1–8.3; unit drop-in; Cloud Hypervisor and QEMU arguments; QEMU shim vCPU pinning; start-time check | Unit tests per filter and per tie-break; argument tests for both hypervisors; host test on a two-socket machine: a `strict` VM with a GPU has its vCPUs, memory (`numastat -p`) and device on one node, for both hypervisors, and across a control-plane restart. |
| **HT3** Prepare and health | §5.3 prepare/release through netd, udev rule, installer `--vfio-devices`; §5.4 error counters, device events, `DeviceHealthy`, cordon | netd tests with a fake sysfs; host test: prepare a NIC, pass it through, release it, host driver back. |
| **HT4** Pools | §9: cluster pools, `devices` requests, `same_switch`, grants and quotas by pool, `device.alloc` metering, UI | Scheduler tests for pools across nodes and switches; quota refusal; metering ledger rows per pool. |
| **HT4b** Tenant pools | §9.1a: tenant pools over cluster pools, their API, CLI and UI | Admission tests: a tenant pool's `max`, membership, and the tenant's own cap all apply. |
| **HT5** SR-IOV VFs | §10.1 VF enumeration, `sriov_numvfs` through netd, VF pools | Host test with an SR-IOV NIC: enable VFs, a pool of VFs, two VMs each with one, NUMA-aligned. |
| **HT6** Replicated and discovered pools | H20–H23: `pci_settings` with its API, gxctl and Cluster page card; the file's migration; discovered pools; `exclude` and `overrides` and their summary fields; the installer's `--vfio-devices` through the setting | Unit tests: the migration writes once and a differing file is ignored; every server resolves the same pools from the same summaries; a defined pool, an exclude and a slug clash each remove a device from discovery; a tenant can't see or request a discovered pool, a host role can; a rename with VMs requesting the pool is refused. Cluster test: a pool set on the leader is requested through a follower and after a leader change. On gpu-dev2 in the three-server cluster: `amd-7551` discovered with both R9700s, adopted as `r9700`, a 1-GPU VM created while prism2 leads. |

HT0 is a fix and can merge alone. HT1 changes nothing for VMs. HT1b
fixes hugepage accounting without NUMA and can follow HT0 directly. HT2
and HT4 are the user-visible parts.

Done: **HT0** (#63), **HT0b** (#76), **HT1** (#78), **HT1b** (#67), **HT2** (#71), **HT6** (#399–#420, H24 in #418; the acceptance run below), **HT3**, with the part of HT1 it
needs: the topology summary in node status (§3.1, in the reduced shape
of §17). HT2's acceptance test (`numa_placement_e2e`,
`qemu_numa_placement_e2e`) passes on a two-socket host with eight NUMA
nodes and A100 GPUs, for both hypervisors.

HT6's acceptance run on gpu-dev2, in the three-server prism cluster
with prism2 leading (term 1052), main at `5b56b25` on the three servers:

| Date | Check | Result |
|---|---|---|
| 2026-11-09 | Discovery with no pools set up (`gxctl pci settings`: source the file, empty) | **pass**: `gxctl pci pools` from prism1 lists `amd-7551` (gpu, discovered, host roles only) with `03:00.0` and `43:00.0` on gpu-dev2, both free; and `nvidia-20b0` with prism1's and prism2's A100s |
| 2026-11-09 | Adopt, from prism1: `gxctl pci pool adopt amd-7551 --name r9700`, then `pci pool set` with `bar_mib: 16384` | **pass**: the setting (source `cluster`) holds `r9700`; prism2, the leader, lists `r9700` with both cards free and no discovered `amd-7551` |
| 2026-11-09 | A 1-GPU VM created through prism1 while prism2 leads (`POST /vms`, `gpus: {count: 1, pool: r9700}`, 4 vCPUs, 4 GiB, `ubuntu-26.04`, Cloud Hypervisor) | **pass**: placed on gpu-dev2 with `03:00.0`, started and running; see 27-gpu-vms.md §12 for the passthrough |

## 16. Open questions

Settled (see the decisions): shared vCPUs under a NUMA policy bind
memory only (H15); whole cores is a per-cluster switch (H16); pools at
the cluster and per tenant (H17); one host core per NUMA node reserved
by default once a node holds a `dedicated` VM, overridden by
`cluster.node_reserved.cpu_list` (H7); device errors reported, not acted
on (H18).

Still open:

- **1 GiB pages and DPDK:** DPDK uses the host's default page size. When
  a host has both sizes, should the installer pin DPDK to one size
  (`--huge-dir`) so the accounting knows which pool it draws from?
- **Tenant pool caps by node:** should a tenant pool be able to say
  "at most 2 on any one node", for spreading a team's GPUs?
- **Cordon scope:** cordon a device per node (as written) or by model
  across the cluster (a firmware recall)?

## 17. Implementation notes

Where HT2 differs from the design above, and why:

- **Binding by the shim, not systemd** (H9, §7.1). The control plane runs
  as an unprivileged user and can't write unit drop-ins or call
  `SetUnitProperties` on `glidex-vm@` units. The shim runs inside the
  unit as the same user, so it binds the hypervisor itself.
- **Reserved CPUs key** (H7): `cluster.node_reserved.cpu_list`, next to
  the existing `cpus` and `memory_mib` reservations, instead of
  `node.reserved_cpus`.
- **Topology summary** (§3.1): `status.topology` has the NUMA nodes
  (cores, memory less the hugepage pool, hugepages less OVS-DPDK's
  share), `isolated`, `reserved_cpus` (OVS-DPDK's `pmd-cpu-mask` and
  `dpdk-lcore-mask`, now in netd's probe, and the config list),
  `reserved_by_config`, and `device_numa` (address → NUMA node) in
  place of `devices`, `distance` and `warnings`. The rest of §3–§5 is
  HT1 and HT3.
- **Score**: the node is still chosen by its node-wide shares; within it,
  the NUMA node by most devices, then least memory share, then id.
- **Changes to a placed VM** (§6): its CPUs and NUMA node are picked again
  at once, against what the other VMs on the node hold; a running VM
  keeps its old binding until its next start.
- **Standalone hosts** (H8) run the same fit against their own node
  record; a `strict` VM that fits nowhere is refused with `409`. Their
  `page_kib` stays unset for ordinary or implicit pages (HT1b).
- **Found on the way**: QEMU refused glidex's VFIO device ids
  (`_vfio_…`, which don't start with a letter), so VFIO passthrough never
  launched on QEMU. They are now `vfio_…` (04-hypervisors.md).
- **HT0b** (§8.5): `spec.device_nodes` is worked out by the service
  layer when the VM's devices are set (create, update, attach), from the
  nodes that report every device (the VM's own node once it has one, or
  its pin; on a standalone host, itself), keeping those where
  `usePciDevice` allows each device. A device no node reports is checked
  on the answering node, as before. A changed device list checks every
  device again, so a VM whose grant was revoked can't take a new device
  until the grant is back.
- **HT1**: the reader is `crate::topology` in the control plane, built on
  `glidex_ovs::tuning::read_topology` for CPUs, rather than a crate of
  its own; `glidex_ovs::tuning` still reads for itself. Port kinds are
  told from where bridges sit in sysfs paths (a bridge under a root
  complex is a root port, then upstream and downstream ports alternate),
  not from the PCIe capability, which needs root to read. The tree is
  served as `GET /host/topology` (`TopologyService`, a host path the
  agent UI relay runs on the agent) and `GET /nodes/{id}/topology`
  (`NodeService.GetNodeTopology`, in the cluster family so the relay
  doesn't treat it as host-local), both under the new `readHostTopology`
  action in `host.read`; a server reads another node's over
  `/cluster/v1/topology` (servers only). Warnings are in the tree, not
  the replicated summary. The summary keeps the reduced shape of HT2.
  Not done from §4: refreshing on udev events and `gxctl node topology
  refresh` (H14): the summary is refreshed with the rest of node status,
  and the tree is read fresh on each request.
- **HT3** (§5.3, §5.4): netd ops `prepare_device`, `release_device`,
  `list_prepared_devices`, with a `devices` table keyed by IOMMU group;
  the udev rule is one file per group, and the group device is given to
  the VMs' group at once as well. The actions are `prepareDevice`,
  `releaseDevice` and `cordonDevice` (also uncordon) in `host.devices`;
  a server runs prepare and release on another node over
  `/cluster/v1/devices/{address}/{op}` (servers only). Cordoned devices
  are `spec.cordoned_devices` of the node. Health: AER counts in the
  tree; the VM controller compares each held device's uncorrectable
  count with the one recorded at launch (`status.instance.device_errors`)
  and whether it is still on the bus, setting `DeviceHealthy` and
  recording `DeviceError` once per turn to false; link changes and
  reset failures aren't watched separately (a reset failure fails the
  start, as before). Not done: the installer's `--vfio-devices`
  persistence, and node events for devices no VM holds. Found on the
  way: `glidex_ovs::nic::pci_netdevs` read a `net_names` file sysfs
  doesn't have, so netd's host-NIC check before a DPDK bind never saw a
  NIC; it reads the device's `net/` directory now.
- **HT4** (§9): pools are `pci.pools` of each server's control-plane.json,
  like `pci.allow`, not a replicated setting; overlap is refused for two
  pools with the same match on shared nodes (a device two different
  matches cover goes to the first pool listed). `match.labels` is not
  done (there are no device labels yet). Nodes report their assignable
  devices with ids, NUMA node, switch and readiness in
  `status.topology.devices`, refreshed after a prepare or release. Pool
  grants are a `PciPool` Cedar entity under the cluster with
  `usePciPool` (in `host.devices` and `shared`); a pool request checks
  the pool, not each device. Picked devices are
  `status.placement.pool_devices`, claims like named devices, added to
  the VM's VFIO devices at launch and in device drift. `GET /pci-pools`
  (`listPciPools`, `host.read`) needs a host role.
- **HT4 follow-up**: `GET /pci-pools?project=` lists the pools a
  project may use (`usePciPool`), with free counts and no devices, for
  anyone who can read the project (`readProject`); the UI's VM form
  offers them. `devices.<pool>` quotas (§9.3) are `Quotas.devices` (a
  map; a pool not named is unlimited), counted over the project's VMs'
  `devices` requests whether running or not, like `vcpus`, and summed
  over a tenant's projects for its cap; the overrun's resource is
  `devices.<pool>`. Metering (§9.4) is one meter per pool,
  `device.alloc.<pool>`, from the placement's `pool_devices`, held while
  paused (the devices stay the VM's). `prepare: true` runs once when the
  node's controller starts, for devices not ready for a reason a
  `vfio-pci` bind can fix (not for no IOMMU group or an OVS-DPDK
  uplink); failures are logged, not reported in the node record.
- **HT4b** (§9.1a): tenant pools are a cluster-scoped table
  (`tenant_device_pools`, by `<tenant>/<name>`), set as a whole with
  `PUT /tenants/{id}/device-pools` (`manageTenantDevicePools` in
  `tenant.projects`, so tenant admins; `readTenantDevicePools` in
  `tenant.read`). A name may not be a cluster pool's; `from` must be a
  cluster pool granted to the tenant (`pci.allow` with `tenants`).
  Admission resolves a request naming a tenant pool to its cluster pool
  and records both (`DeviceRequest.tenant_pool`); the project must be
  one of the pool's and the tenant still granted. `max` counts the
  requests of all the tenant's VMs naming the pool, running or not
  (like quotas), checked under the VM lock with the quotas, as
  `tenant_quota_exceeded` / `tenant_pool.<name>`; `exceedQuota` may go
  over. `GET /pci-pools?project=` lists the project's tenant pools too,
  with `from`, `max`, `used` and `free` = what it may still take. Not
  done: `max` on `tenant_grants` (a tenant's grant has no size yet, so a
  tenant pool can't be checked against one); the `devices.<pool>` cap
  is the bound.
- **H25**: `api/vms.rs::direct_pool_entities` builds the pool entity for
  a request naming the cluster pool, leaving out the project's tenant
  from `tenant_grants` when that tenant has tenant pools on it, so
  Cedar's `base.pci-pool-grant` allows only a grant to the project and
  host roles still allow it by their own policies.
  `service/vm.rs::check_pools` (create and update) refuses with `403
  forbidden` naming the tenant pools; `expand_gpus` and `GET
  /pci-pools?project=` use the same decision (`may_use_pool_directly`).
  It applies at admission only: VMs created before keep their requests.
  Tests:
  `tenancy_tests.rs::tenant_pools_are_the_tenants_only_way_to_its_granted_pool`
  (before and after the split, a project grant, a host role, `gpus`
  picking the tenant pool, the pool list).
- **HT5** (§10.1): netd op `set_sriov_vfs` (`glidex_ovs::sriov`) writes
  `sriov_numvfs`, through 0 when the PF had some, and forgets prepared
  groups of the VFs a change removes (every one: the kernel recreates
  them). `POST /nodes/{id}/devices/{pf}/sriov {vfs}` (`setSriovVfs`,
  `host.devices`) runs on the PF's node through the device-op path
  (`/cluster/v1/devices/{pf}/sriov-<N>`), refused (`409`) while a VM's
  claims hold the PF or a VF the node's summary lists; afterwards the
  node runs `prepare: true` again and reports. The tree's `sriov` gains
  `vf_driver` and `ready_vfs`; a PF with VFs enabled is not ready ("pass
  those through instead"). The summary keeps VFs only once ready (on
  `vfio-pci`): unbound ones are the PF's counts, which bounds the
  summary to what an operator dedicated. Summary devices carry
  `physfn` (address and ids) and PFs `sriov`, and pools match VFs with
  `match.vf_of`, a nested match of the PF (empty is refused). Not done:
  per-VF MAC, VLAN and spoof checking (§13), and VFs across reboots
  (the installer's `--vfio-devices`, with `sriov_numvfs`).
- **HT6, the setting** (H20, §9.1): `pci_settings` in `Meta`. Pool
  resolution, the scheduler, `GET /pci-pools`, the node's `prepare: true`
  pass and the `usePciPool` and `usePciDevice` grants read the pools and
  grants in force (`VmManager::pci_pools`, `pci_grants`): the setting
  once written, else this host's file. Every node recomputes them when
  the key is applied. The leader's controller writes the setting from
  its file once, when the file has pools or grants (`updated_by:
  "migrate:<node id>"`); a server whose file differs from the setting
  logs once that it is ignored. A standalone host migrates the same way.
  `GET/PUT /cluster/settings/pci` (`GetPciSettings`, `PutPciSettings`)
  as §9.1 says; gxctl has `pci settings [set <file.json>]` until the
  pool commands come. Host overrides (H23, §9.1b): `pci.exclude` and
  `pci.overrides` stay in `PciConfig` but are read from this host's
  file only, never from the setting; a summary device the host keeps is
  `excluded`, skipped by the scheduler's pool picks, the pool listing
  and the node's `prepare: true` pass, and a VM naming its address is
  refused on that node ("kept by its host"). `prepare` applies the
  host's `bar_mib` and `prepare` to the named pool; an override naming
  no pool is logged when the pools change. A changed file takes a
  control-plane restart, as other file keys do. Discovered pools (H21,
  H22, `pci_discovery.rs`): `VmManager::pci_pools()` is the defined
  pools and the discovered ones, recomputed when the setting is applied
  and when a node record changes; `defined_pools()` is the setting's
  (or file's) alone. A discovered pool's `nodes` are those with a
  device of the model no defined pool covers there. It has no grants,
  so only `host.devices` (net-admins, system admins) passes
  `usePciPool`. `GET /pci-pools` marks it `discovered`; the PUT counts a
  pool still discovered under the new pools as kept. `gxctl pci pool
  adopt <slug> [--name N]` appends a pool matching the slug's ids to
  the setting. The installer's `--vfio-devices <pool>` (task 6) asks the
  running control plane for the setting (`gxctl pci settings`), else
  reads the host's file, else takes a discovered pool's name as its ids
  (`glidex_topology::pci::pool_slug_ids`, shared with discovery and gxctl);
  the host's `pci.overrides` set `bar_mib`. `vfio-devices.json` gains
  `exclude` (the host's `pci.exclude`): the boot unit's plan and the
  G20 claim skip those devices, and the claim's udev rules jump past
  them (`GOTO`). The Cluster page's **Device pools** card
  (`components/DevicePools.tsx`, standalone or clustered, for
  `readCluster`): each pool from `GET /pci-pools` with free devices per
  node, sizing, `bar_mib` and grants from the setting, discovered ones
  marked; with `configurePciPools`, Adopt (a pool matching the slug's
  ids, under its name or another), Remove (with its grants) and the
  whole setting as JSON. `gxctl pci pool show|set|rm|list` and `pci
  grant [rm]` edit one pool or grant: each reads the setting, changes
  it and writes it back whole; granting a discovered pool adopts it
  under its slug in the same write. Discovered NICs (H24): class `02xx`
  is `nic`, and a VF whose PF is a NIC joins `<PF slug>-vf`, matching
  `vf_of` (`glidex_topology::pci::{pool_vf_slug, pool_vf_slug_ids}`);
  gxctl's and the card's Adopt, and the installer's `--vfio-devices`,
  read a `-vf` name as that `vf_of`. The card's **Grants…** (per pool,
  discovered ones too): projects and tenants ticked from the directory
  (`ListProjects`, `ListTenants`), or every admitted tenant (`"*"`, the
  tenant list then disabled); saving replaces the pool's grants with
  one and, for a discovered pool, adopts it under its slug in the same
  write. Device (`bdf`) grants stay with the JSON editor and gxctl. The
  pool and grant reference is in the PCI passthrough guide. The
  acceptance run on gpu-dev2 passed on 2026-11-09 (§15).
- **Launch time and the host kernel** (2026-11-10). Before its API
  answers, Cloud Hypervisor maps the VM's memory into the IOMMU, then each
  device's memory BARs. Traced with `strace` on Cloud Hypervisor v53, 16
  vCPUs, 4 GiB:
  - prism2, Linux 6.8.0-138, an A100 40 GB (64 GiB BAR 1) and a
    ConnectX-6: one 27.2 s `VFIO_IOMMU_MAP_DMA` for the A100's BAR, 31 s
    in all.
  - gpu-dev2, Linux 7.0.0-42, R9700s (16 GiB BAR 0): no map call over
    0.3 s besides the guest memory's 2.2 s; 6.0 s in all with one card,
    6.7 s with two.

  After prism2 moved to Linux 6.17.0-42 (2026-11-10), the A100's map
  was still 27 s, done twice: once at launch and once after the guest's
  UEFI firmware moves the BAR. A kernel profile showed vfio-pci's fault
  handler inserting 4 KiB entries (`vfio_pci_mmap_huge_fault` →
  `vmf_insert_pfn`) though the mapping is 1 GiB-aligned: on 6.17 a huge
  entry is tried only where transparent hugepages allow it, and the host
  has them at `madvise`. Same A100, 6.17, UEFI guest:
  - Cloud Hypervisor v53 or Pylon `main`, `madvise`: Linux after 60–70 s.
  - Cloud Hypervisor v53, `always`: 13.6 s, no map over 0.5 s besides
    the guest memory's.
  - Pylon with CMKL-APEX/pylon#76, which madvises its BAR mappings
    `MADV_HUGEPAGE`, `madvise`: 12.0 s.

  7.0 needs neither. 6.8 wasn't tried with either. So a passthrough host with large-BAR GPUs runs 6.17 with
  Pylon #76 or transparent hugepages `always`, or 7.0
  (pci-passthrough.md, *Host kernel*). prism2's NVIDIA OFED, 2607.0.100,
  builds on 6.17 but not on 7.0 (`xsk_buff_get_metadata`). The launch
  deadline (12-reconciliation.md D22) covers the slow mapping.
