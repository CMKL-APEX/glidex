# GPU VMs

Status: **design; GV1, GV2 and GV3 implemented; GV6 (AMD on gpu-dev2)
designed** (§12 milestones, §14 notes). Builds on
[22-host-topology.md](22-host-topology.md) (HT0–HT5: topology, readiness,
NUMA policy, device pools, SR-IOV), [04-hypervisors.md](04-hypervisors.md)
(command lines) and [05-installer.md](05-installer.md).

A GPU VM can be made today, but it takes knowing the host. The user has
to:
- ask a pool for devices;
- pick a NUMA policy and a CPU policy;
- size vCPUs and memory to fit one NUMA node;
- know which GPUs are NVLink peers.

A VM whose GPUs sit on two NUMA nodes can't be aligned at all. `strict`
refuses it, and `preferred` puts all its memory and vCPUs on one node,
so half its GPUs are remote. The guest always sees one NUMA node. On an
NVSwitch board (HGX), multi-GPU VMs also get no NVLink: nothing programs
the NVSwitches for GPUs that are passed through.

This document makes four changes:
1. **One request:** "N GPUs" gives a VM whose vCPUs, memory and GPUs are
   aligned to the host's NUMA nodes, with that layout shown to the guest
   (§3–§6).
2. **NVLink on NVSwitch boards:** NVIDIA Fabric Manager runs in a
   **service VM** that owns the NVSwitches, and glidex activates a
   fabric partition for each GPU VM (§7–§9).
3. **Across reboots:** the installer keeps the passthrough binding and
   the VF counts (`--vfio-devices`, §10).
4. **Any vendor's GPU:** what the first three need from the host
   (locked memory, room for large BARs, binding before the host
   driver) works for AMD as well as NVIDIA, shown on gpu-dev2's Radeon
   cards (§2.3, GV6).

## 1. Scope

In:
- GPU requests by count, with sizing that follows the GPUs.
- Placement that may span NUMA nodes and keeps each GPU next to its
  share of vCPUs and memory.
- A guest NUMA and PCIe layout that mirrors the host's.
- NVLink through Fabric Manager's shared NVSwitch mode.
- A general **system VM** mechanism, used first for Fabric Manager.
- Keeping VFIO bindings and VF counts across reboots.
- Whole GPUs of any vendor; AMD Radeon on gpu-dev2 is the second
  reference host (§2.3).

Out:
- vGPU and MIG (FM's vGPU mode, `FABRIC_MODE=2`).
- NVLink across hosts (NVL72, IMEX).
- Live migration of GPU VMs.
- GPU utilization metering (22-host-topology.md §9.4).
- Programming DPUs.

## 2. Today, and the reference host

### 2.1 What it takes now

A 4-GPU VM on prism1 today takes:

```json
{ "vcpu_count": 56, "mem_size_mib": 229376, "hugepages": true,
  "numa_policy": "preferred", "cpu_policy": "dedicated",
  "devices": [{ "pool": "a100", "count": 4 }] }
```

- The user picks 56 vCPUs and 224 GiB by hand.
- The scheduler picks four free A100s, near each other where it can
  (22-host-topology.md §9).
- Two GPUs per NUMA node means the four land on two nodes. `numa_fit`
  (scheduler.rs) puts all memory and CPUs on the node holding the most
  of them, so two GPUs are remote.
- The guest gets one NUMA node and a flat PCIe bus. NCCL inside it sees
  every GPU pair as `PHB` or `SYS`, not NVLink.

### 2.2 prism1 (HGX A100 8-GPU)

| NUMA | CPUs | Memory | GPUs (A100 SXM4 40GB) | Other |
|---|---|---|---|---|
| 0 | 16 cores | 126 GiB | – | ConnectX 61:00.0, BlueField management 61:00.2, BMC VGA 63:00.0 |
| 1 | 16 cores | 126 GiB | 47:00.0, 4e:00.0 | ConnectX 4b, 54; NVMe 52 |
| 3 | 16 cores | 126 GiB | 07:00.0, 0f:00.0 | ConnectX 0c, 12; NVMe 09 |
| 5 | 16 cores | 126 GiB | b7:00.0, bd:00.0 | ConnectX ba, cc; NVMe ca; **6 NVSwitches c4–c9** |
| 7 | 16 cores | 126 GiB | 87:00.0, 90:00.0 | ConnectX 8d, 94; NVMe 8a |
| 2 | 16 cores | 126 GiB | – | NVMe 22, 23 |
| 4 | 16 cores | 126 GiB | – | ConnectX e1:00.0/.1, Intel e2 |
| 6 | 16 cores | 126 GiB | – | – |

- Each GPU has its own PCIe switch, shared with a ConnectX NIC (and on
  some nodes an NVMe drive).
- The NVSwitches are in their own IOMMU groups, 178–183, on the host's
  `nvidia-nvswitch` driver.
- `nvidia-fabricmanager` 580.178.04 runs **on the host** in
  `FABRIC_MODE=0` (bare metal). With every GPU on `vfio-pci`, its log
  shows it removing all 8 GPUs from the routing and "configuring" the
  NVSwitches for none. So NVLink is not available to any passthrough VM
  on prism1 today.

### 2.3 gpu-dev2 (two AMD Radeon AI PRO R9700)

A small host with no fabric, for whole-GPU passthrough of another
vendor. Read from sysfs on 2026-10-29:

| | |
|---|---|
| CPU, memory | EPYC 9124: 16 cores, 32 threads, **one NUMA node**; 30 GiB |
| GPUs | Navi 48, `1002:7551`, class `0x030000` (VGA, not NVIDIA's `0x0302`), at `03:00.0` and `43:00.0` |
| Path | Each behind the card's own PCIe switch (`01:00.0`→`02:00.0`, `41:00.0`→`42:00.0`), Gen5 x16 |
| IOMMU | Each GPU alone in its group; no audio function |
| Reset | `flr bus` |
| BARs | 32 GiB (resizable BAR on) and 256 MiB, 64-bit prefetchable |
| Host driver | `amdgpu`, loaded from the root file system (not in the initramfs); `vfio-pci` not loaded |
| Console | The BMC's ASPEED (`c6:00.0`, `ast`), so both Radeons can leave the host |
| Hypervisors | Cloud Hypervisor v53, Pylon (Cloud Hypervisor with `--pylon`, the same device flags), QEMU |

What this host shows that prism1 doesn't, and what it can't:
- **It shows** the host-side pieces any passthrough GPU needs, outside
  NVIDIA's: the locked memory a VFIO VM takes (G12), room for 32 GiB
  BARs in the guest (G13), the boot binding against a driver that
  claims the card first (§10, G16), and GPUs with Pylon's elastic CPU
  (G14).
- **It can't show** `aligned` across NUMA nodes (GV1: one node, so the
  single-node path runs), system VMs or Fabric Manager (GV4, GV5:
  NVIDIA only). An AMD pool has no fabric, so its `nvlink` is `false`.
- **Its memory is small.** Two GPU VMs share 30 GiB with the host, so
  the pool's `per_device` is about 6 vCPUs and 10 GiB.

## 2a. Decisions

| # | Topic | Decision | Reason |
|---|---|---|---|
| G1 | Request | A VM asks for `gpus: {count, pool?, nvlink?}`. glidex expands it to a pool request, `numa_policy: aligned` and default sizing; explicit fields win. | Most users want "4 GPUs"; the expansion uses the existing pieces and stays visible in the spec. |
| G2 | Multi-NUMA placement | New `numa_policy: aligned`: the VM spans exactly the NUMA nodes its devices are on, with its vCPUs and memory split between them in proportion to its devices. `strict` and `preferred` keep their meaning. | It's the only layout that keeps every GPU local once a VM outgrows one node. |
| G3 | Guest topology | An `aligned` VM's guest gets one NUMA node per host node, with its vCPUs, memory and devices: a PCI segment per node (Cloud Hypervisor), or a `pxb-pcie` per node (QEMU). | `nvidia-smi topo` and NCCL in the guest then match the host. |
| G4 | Sizing | A pool can carry `per_device: {vcpus, memory_mib, hugepages}`. A GPU request without sizing takes count × that, capped at what its NUMA nodes have free. | The operator knows a sensible share; the user shouldn't have to. |
| G5 | NVLink model | NVIDIA's **shared NVSwitch** model: Fabric Manager in `FABRIC_MODE=1` owns the NVSwitches, and glidex activates a fabric partition for a VM's GPUs before starting it. | It's the only model where VMs of 1, 2, 4 and 8 GPUs share a board with NVLink inside each. |
| G6 | Where FM runs | In a **service VM** that holds the NVSwitches, by default. Running FM on the host in `FABRIC_MODE=1` is an option for hosts already set up that way. | It keeps the NVIDIA driver, FM and their version pairing off the host kernel, and follows NVIDIA's reference. |
| G7 | Service VMs | A general, glidex-owned **system VM**: pinned to its node, in the `system` project, outside quotas, started before tenant VMs, health reported on the node. Fabric Manager is the first one. | DPU control, storage gateways and others follow the same shape. |
| G8 | Talking to FM | A small agent in the service VM (`glidex-fabricd`) drives FM through its API and answers the node over **vsock**. | No network or NVIDIA libraries on the host, and no route from tenants to FM. |
| G9 | Partition choice | With NVLink, the scheduler picks among FM's supported partitions (fixed GPU sets of 1, 2, 4, 8), not arbitrary GPUs. | FM only routes those sets. |
| G10 | Persistence | `glidex-install --vfio-devices <pool or addresses>` writes a boot-time binding (driverctl-style override, modprobe options) and the VF counts, and keeps NVSwitches for the service VM. | Today a reboot leaves devices on their host drivers until glidex starts, and VF counts at 0. |
| G11 | Vendors | Requests, pools, sizing and placement don't depend on the GPU's vendor: a pool's `match` names the GPU, and `gpus` takes a pool of class `gpu` whatever its vendor. Only the fabric (§7–§9) is NVIDIA's. A pool with no fabric resolves an absent `nvlink` to `false`. | AMD (gpu-dev2) works through the same path; nothing to add per vendor but what the host needs (G12, G13). |
| G12 | Locked memory | A VM with VFIO devices may lock all its guest memory. `glidex-vm@.service` sets `LimitMEMLOCK=infinity`. The VM's memory is still bounded by its size and by `glidex-vms.slice`. | VFIO's type1 IOMMU pins the guest's RAM, counted against `RLIMIT_MEMLOCK`, which is 8 MiB for system services. The unit runs as `glidex`, and the control plane can't write a per-VM drop-in. |
| G13 | Room for large BARs | At launch, the hypervisor's 64-bit PCI window is sized from the VM's devices: the sum of their 64-bit BARs (sysfs `resource`), each rounded up to its own alignment, with 1 GiB to spare, as a power of two. QEMU takes it as `-global q35-pcihost.pci-hole64-size=` when it exceeds QEMU's 32 GiB default. Cloud Hypervisor and Pylon place 64-bit BARs above RAM within the host's physical address bits, split between PCI segments (verify v53's split). Glidex checks that each PCI segment's share holds its devices, and refuses the launch naming the device when it doesn't. | Two R9700s need 64 GiB, and a 4-GPU A100 VM 256 GiB. QEMU's default fits neither. |
| G14 | Pylon and dedicated CPUs | A VM with `cpu_policy: dedicated` may be elastic on Pylon: pylond may park its vCPUs within `min_vcpus`..`max_vcpus`. Its cores stay in its placement while parked; glidex doesn't give them back to the shared pool. Nothing is built for this: no check refuses the combination today. | The user's call (Q9). A GPU VM's load comes and goes, and parking an idle vCPU keeps its pin. |
| G15 | Guest image | Glidex ships no GPU image. The docs say how to build or import one: Ubuntu with the generic kernel (or `linux-modules-extra`) for `amdgpu`, and `linux-firmware` for the GPU; ROCm only to run work. | The user's call (Q8). The image follows the driver and ROCm releases, which the operator chooses. |
| G16 | Binding before `amdgpu` | GV3's boot unit as built: it unbinds `amdgpu` and binds `vfio-pci`. Claiming the cards before `amdgpu` loads is built only if gpu-dev2's reboot test shows the unbind failing. | The user's call (Q10). One mechanism while it works; `vfio-pci ids=` would take every R9700, even those kept on the host. |
| G17 | GPU memory window | When glidex prepares a device (**Prepare**, a pool's `prepare: true`, the boot binding), it loads `vfio-pci` if needed. For devices with a resizable BAR, the sum of a VM's GPU windows stays below the host's memory. As the size is set when a device is prepared, before any VM picks it, glidex keeps the total of the host's prepared GPU windows below the host's memory: each gets the largest supported size that keeps the total under (16 GiB for one R9700 on gpu-dev2, 8 GiB each for two). The resize happens between unbinding the host driver and binding `vfio-pci`, as the kernel takes it only on a device with no driver. A pool can name the size (`bar_mib`); release puts the size back. The guest's driver still reaches all of the GPU's memory through the smaller window. | The user's call, after the gpu-dev2 checks (§12): a 32 GiB window on a 32 GiB host froze the host when the VM started; 256 MiB and 16 GiB ran. The total cap is the user's call for multi-GPU VMs, so two windows never add up to the size that froze. |

## 3. The request

```json
{ "name": "train-1", "image": "ubuntu-24.04-cuda",
  "gpus": { "count": 4, "pool": "a100", "nvlink": true } }
```

- `count` (required) and `pool`. `pool` defaults to the only GPU pool
  the project may use; with several, a missing `pool` is `400` naming
  them.
- `nvlink`:
  - `true` asks for one fabric partition, which needs a fabric (§7).
  - `false` takes any free GPUs.
  - Absent means `true` on a node with a fabric and a partition of that
    size, else `false`.
- The `gpus` request expands into:
  - `devices: [{pool, count}]`, with `same_switch` left off;
  - `numa_policy: aligned`;
  - `cpu_policy: dedicated` when the pool's `per_device` says so;
  - `hugepages` from `per_device`;
  - `vcpu_count` and `mem_size_mib` = count × `per_device`, unless
    given.
- The VM keeps `gpus` and shows the expansion. `PATCH` of `gpus` runs
  the expansion again (stopped VMs only, like other sizing changes).
- gxctl: `vm create train-1 --image … --gpus 4[:a100][:nvlink]`.
- UI: a GPU count and a pool picker. The form shows the sizing that
  follows, and where it will land from the placement preview.

## 4. Placement: `aligned`

1. **Pick the devices.** As 22-host-topology.md §9 does, except:
   - With NVLink, the candidates are FM's free partitions of the
     requested size on the node (§9).
   - Otherwise, prefer the fewest NUMA nodes (2 GPUs → one node on
     prism1), then the fewest PCIe switches.
2. **Derive the NUMA set:** the host nodes of the picked devices.
   Devices without a NUMA node make `aligned` fail.
3. **Split the VM:** node *k* gets `vcpus × n_k / n` and `memory × n_k / n`,
   where *n_k* is the number of the VM's devices on *k*, rounded so the
   totals hold.
4. **Fit per node:** each share must fit node *k*'s free CPUs (dedicated:
   whole cores, minus `reserved_cpus`), memory and hugepages
   (22-host-topology.md §8, §8a). If one doesn't, the next candidate set is
   tried. With none left, the refusal names the node short of what.
5. **Record it.** `placement` gains `numa: [{node, cpus, memory_mib,
   devices}]`, which replaces the single `numa_node` for `aligned`.

The placement preview (`POST /scheduler/preview`) shows the split per node (`numa`).

## 5. The guest's layout

| | Cloud Hypervisor | QEMU |
|---|---|---|
| Memory per node | `--memory-zone id=memK,size=…,host_numa_node=K[,hugepages=on]` | `-object memory-backend-…,id=memK,host-nodes=K,policy=bind` |
| Guest NUMA node | `--numa guest_numa_id=i,cpus=[…],memory_zones=memK,pci_segments=[s]` | `-numa node,nodeid=i,cpus=…,memdev=memK` |
| A PCIe root per node | `--platform num_pci_segments=N`; each device `pci_segment=s` | `-device pxb-pcie,id=pxbK,bus_nr=…,numa_node=i`, then a `pcie-root-port` per GPU under it |
| Distances | from host `distance` (verify CH support) | `-numa dist,src=…,dst=…,val=…` from `/sys/devices/system/node/nodeK/distance` |
| vCPU pinning | unchanged: vCPU *j* of guest node *i* → a CPU of host node *K* | same |

- A GPU and the ConnectX NIC under the same host switch go under the
  same guest root port tree when both are passed through. This keeps
  GPUDirect RDMA on one PCIe switch, as in guest `nvidia-smi topo` `PIX`.
  Root port per switch (verify the QEMU layout).
- Large BARs: A100 needs 64 GiB of 64-bit MMIO per GPU. QEMU needs
  `-global q35-pcihost.pci-hole64-size=` sized for the VM's GPUs, and
  Cloud Hypervisor its segment apertures (verify the defaults on prism1).

## 6. Sizing

```json
{ "name": "a100", "class": "gpu", "match": { "vendor_id": "0x10de", "device_id": "0x20b0" },
  "per_device": { "vcpus": 14, "memory_mib": 57344, "hugepages": true, "cpu_policy": "dedicated" } }
```

- On prism1, each GPU node has 16 cores and 126 GiB. With one core per
  node kept for the host (H7), two GPUs per node leave 15 cores, about 7
  each (14 vCPUs with SMT), and about 56 GiB each. The values above are
  illustrative; the installer can propose them from the topology.
- Without `per_device`, a `gpus` request needs `vcpu_count` and
  `mem_size_mib`.

## 7. Fabric Manager

NVIDIA's three virtualization models for NVSwitch boards:

| Model | Where FM runs | VMs | glidex |
|---|---|---|---|
| Full passthrough | inside the one tenant VM (`FABRIC_MODE=0`) | one VM with all 8 GPUs and all 6 NVSwitches | possible today with named devices, but the whole board goes to one VM |
| **Shared NVSwitch** | a service VM holding the NVSwitches (`FABRIC_MODE=1`) | partitions of 1, 2, 4 or 8 GPUs, each its own VM, NVLink inside it | **this design** |
| vGPU | host, `FABRIC_MODE=2` | vGPU guests | out of scope |

In shared NVSwitch mode (FM user guide, verify each against 580.x):
- FM starts with the NVSwitches only. It reads the board's supported
  partitions (`fmGetSupportedFabricPartitions`), each an ID and a GPU
  set given by physical module ID.
- Before a VM gets a partition's GPUs, the GPUs are reset and the
  partition activated (`fmActivateFabricPartition`). After the VM ends,
  it is deactivated.
- An activated partition's GPUs must not be in use by another partition.
- FM keeps activated partitions across its own restart only with
  `FABRIC_MODE_RESTART=1` (verify).
- **Whether a GPU on an NVSwitch board can be used at all without an
  activated partition, a 1-GPU one included** (verify on prism1). If it
  can't, every GPU VM on an HGX node needs the fabric, NVLink or not,
  and §9 always activates a partition.

## 8. System VMs

A system VM is a VM that glidex itself owns and runs for a node.

```json
{ "kind": "fabric-manager", "node": "prism1", "image": "glidex-fabric-580",
  "devices": ["0000:c4:00.0", "…", "0000:c9:00.0"], "vcpus": 2, "memory_mib": 4096 }
```

- **Where it lives.** In the `system` project, pinned to its node, never
  rescheduled. It's created by the node, or by an operator through
  `gxctl node system-vm enable fabric-manager <node>`.
- **Order.** It starts at node boot before tenant VMs (`on_host_boot`
  ordering). Tenant VMs that need it wait (`Pending: FabricNotReady`)
  instead of failing.
- **Accounting.** It's outside project and tenant quotas and metering.
  It's shown on the node page with its own health:
  - the node condition `FabricReady`;
  - events `FabricPartitionActivated` and `FabricError`.
- **Image.** A system image from the image catalog: an Ubuntu cloud
  image with the NVIDIA driver, `nvidia-fabricmanager` and
  `glidex-fabricd`, in matching versions. The installer, or a
  `gxctl image pull glidex-fabric-<driver>`, fetches it.
- **No network.** It has only a **vsock** to the node, where the control
  plane's controller talks to `glidex-fabricd` (cid per VM; CH
  `--vsock`, QEMU `vhost-vsock-pci`). Tenants can't reach FM.
- **Access.** Operators see it (`readSystemVm`). Tenants never see the
  `system` project. Its console is `system-admin` only.

### 8.1 `glidex-fabricd`

A small daemon in the service VM. It drives FM through `libnvfm`, the
FM API, and answers the node over vsock with JSON:

| Op | Does |
|---|---|
| `status` | FM state, driver and FM versions, NVSwitch health |
| `partitions` | supported partitions: id, GPU physical IDs, PCI addresses as FM sees them, active or not |
| `activate {id}` / `deactivate {id}` | as FM's API |
| `set_active {ids}` | tell FM which partitions are active after it or fabricd restarts (FM's "set activated partitions") |
| `events` | NVLink and NVSwitch errors (stream) |

How it is built, and why, is §8.3.

Inside the service VM, the GPUs aren't there. FM names them by physical
module ID, and the node maps those to host PCI addresses from the
board's fixed table (FM's topology file, or `nvidia-smi` on bare metal
once at setup). That map is a property of the board model, kept in node
status (`fabric.gpus: {module_id → address}`). (verify how FM reports
GPU identity in shared mode.)

### 8.2 FM on the host instead

A node's fabric has a **manager**, `service-vm` (§8) or `host`. Both
answer the same `glidex-fabricd` ops (§8.1), so the scheduler (§9) and
the VM controller don't care which.

| | `service-vm` | `host` |
|---|---|---|
| NVSwitches | on `vfio-pci`, passed to the service VM (`--vfio-devices`, §10) | on the host's `nvidia-nvswitch` driver |
| NVIDIA software | in the service VM image: switch driver, FM, `glidex-fabricd` | on the host: the driver's switch module, `nvidia-fabricmanager` (the operator's packages) |
| FM mode | `FABRIC_MODE=1`, set in the image | `FABRIC_MODE=1` in the host's `fabricmanager.cfg`, set by the operator |
| `glidex-fabricd` | in the service VM, on vsock | on the host as `glidex-fabricd.service` (root), on `/run/glidex-fabric/fabric.sock` (`root:glidex 0660`), talking to FM's command socket (`FM_CMD_UNIX_DOMAIN_SOCKET`, or `127.0.0.1:6666`) |
| Versions | FM matches the image's switch driver; tenant VMs bring their own GPU driver | FM matches the host's driver package, and the host upgrade moves both |
| The host kernel | stock | carries NVIDIA's switch module |

- **Choosing.** The operator chooses per node: `glidex-install
  --fabric host|service-vm|off`, saved in install.conf and reported in
  node status as `fabric.manager`.
  - `host` installs and enables `glidex-fabricd.service`, but **never
    changes FM's mode or restarts it**. It checks
    `fabricmanager.cfg`, and while it says `FABRIC_MODE=0` the node
    reports `FabricReady=False` ("host Fabric Manager runs in bare-metal
    mode").
  - `service-vm` needs the NVSwitches in `--vfio-devices`, and refuses
    while the host's `nvidia-fabricmanager` runs (both can't own the
    switches).
- **The same partitions either way.** In both, glidex activates a
  partition before a VM starts and deactivates it after. Only who
  answers changes.
- **Another hypervisor on the host** (prism1's OpenNebula) is where
  `host` needs care. Its VMs share the same FM. In shared mode, a GPU
  may need an activated partition to work at all (§13 question 1). If
  it does, switching the host FM to mode 1 would leave the other
  hypervisor's GPU VMs without one, since it doesn't activate
  partitions. So on such a host, `host` is for when glidex owns the
  board's GPUs, or after question 1 is answered.

### 8.3 The Fabric Manager interface

glidex talks to Fabric Manager through a library and a daemon of its own,
used the same way for both managers (§8, §8.2). The control plane never
links NVIDIA code or reaches FM's command socket itself.

#### Two parts

| Part | What it is | Who uses it |
|---|---|---|
| `glidex-fabric` (crate) | The protocol: the ops of §8.1 as request and response types, a version handshake, a client, and a `FabricBackend` trait. Plain Rust, no NVIDIA code. | The control plane, gxctl (through the control plane's API), tests |
| `glidex-fabricd` (binary) | A `FabricBackend` over NVIDIA's FM API (`libnvfm`, `nv_fm_agent.h`), and a server for the protocol | Runs in the service VM (vsock) or on the host (`/run/glidex-fabric/fabric.sock`, `root:glidex 0660`) |

```rust
pub trait FabricBackend: Send + Sync {
    fn status(&self) -> Result<FabricStatus, FabricError>;
    fn partitions(&self) -> Result<Vec<Partition>, FabricError>;
    fn activate(&self, id: u32) -> Result<(), FabricError>;
    fn deactivate(&self, id: u32) -> Result<(), FabricError>;
    /// After a restart: these, and only these, are active.
    fn set_active(&self, ids: &[u32]) -> Result<(), FabricError>;
}
```

#### `libnvfm`, at run time

- **Loaded at run time.** `glidex-fabricd` `dlopen`s `libnvfm.so.1` (from
  the `nvidia-fabricmanager` package) and resolves the few functions it
  uses: init and shutdown, connect, the partitions list, activate,
  deactivate, set activated. glidex builds and CI need no NVIDIA SDK, and
  glidex never redistributes the library.
- **Missing library.** fabricd answers `status: unavailable` ("no Fabric
  Manager library"), and the node reports `FabricReady=False`.
- **The FFI is one small module.** It covers the C structs of the
  partition list, the error codes, and a check of the library's API
  version against the one the module was written for; a mismatch is
  refused, not guessed at. The rest of fabricd is safe Rust.
- **Connecting.** fabricd connects to FM on its command socket:
  `FM_CMD_UNIX_DOMAIN_SOCKET` when FM sets one, else
  `127.0.0.1:FM_CMD_PORT_NUMBER` (6666 by default). It runs as root, as
  the socket requires.

#### The protocol

- **Like glidex-netd's:** JSON lines, a hello with the protocol version,
  one request at a time, and a log of each changing op.
- **Idempotent.** Activating an active partition, or deactivating an
  inactive one, succeeds.
- **The control plane is the source of truth.** It knows which VMs hold
  which partitions (`placement.fabric_partition`). After FM or fabricd
  restarts, the node's controller sends `set_active` with the
  partitions its running VMs hold, so FM's own state file doesn't
  decide.
- **Errors keep FM's code and text** (`FM_ST_IN_USE`, say), so the VM's
  `FabricPartition=False` condition says what FM said.

#### Where it runs

- **Service VM** (§8): fabricd is in the image, a systemd unit there,
  serving vsock. The node's controller is the only client.
- **Host** (§8.2): `glidex-fabricd.service`, installed by `glidex-install
  --fabric host`. `Requires=` and `After=` `nvidia-fabricmanager.service`,
  and never restarts or reconfigures it. If FM isn't running, it answers
  `unavailable`.
- **gxctl and the UI never reach it directly.** They use the control
  plane's API, which proxies the read-only ops:
  - `GET /nodes/{id}/fabric` (`readHostTopology`) gives status and
    partitions; `gxctl node fabric <node>`.
  - Activating and deactivating by hand isn't offered: the VM
    controller does it as VMs start and stop. Taking a partition away
    from a running VM is what that would risk.

#### Tests without a board

- `glidex-fabric` ships a `FakeFabric` backend with a fixed partition
  table (an HGX A100's 8 GPUs: partitions of 8, 4, 2 and 1). It records
  the calls and can fail on cue.
- The scheduler (§9 step 1) and the VM controller (steps 2–4: activate
  before start, deactivate after exit, `set_active` on node start) are
  tested against it, as netd is with a fake sysfs.
- The FFI module is tested only on hardware (GV5's prism1 test).

#### Not done this way

- **`libnvfm` in the control plane:** every glidex host, GPU or not,
  would depend on a proprietary library. And the `glidex` user would
  need root's FM socket.
- **Driving `fmpm`** (NVIDIA's sample CLI) and parsing its text: it isn't
  a stable interface, and not every package ships it.
- **A second fabric later** (NVLink Subnet Manager on HGX H100 and
  newer, or another vendor's) is another `FabricBackend`, behind the same
  protocol.

## 9. Partitions in the VM's life

1. **Schedule.** With NVLink, the scheduler takes the node's partitions
   from node status (refreshed from `glidex-fabricd`). It keeps those of
   the right size whose GPUs are all free, ready and uncordoned, then
   applies §4 to each (fewest NUMA nodes first). The placement records
   `fabric_partition: <id>`.
2. **Start.** The VM controller checks `FabricReady`. It asks fabricd to
   `activate`, and the VM starts only after success. A failure is the VM
   condition `FabricPartition=False` with FM's reason.
3. **Stop or exit.** After the hypervisor has exited and the VFIO
   devices are released, the controller asks `deactivate`. On node boot,
   partitions active in FM that no VM holds are deactivated.
4. **Reset.** FM requires the GPUs reset between partitions. vfio-pci
   resets on open; whether a bus reset is needed through the switch is
   verify.
5. **Cordons** apply per GPU. A partition with a cordoned GPU isn't
   offered.

## 10. Across reboots: `--vfio-devices`

The installer gains:

```
glidex-install --vfio-devices a100,cx7-vfs        # pools from control-plane.json
glidex-install --vfio-devices 0000:87:00.0,0000:90:00.0
glidex-install --sriov 0000:61:00.0=8
glidex-install --no-vfio-devices                    # remove what it wrote
```

- **Binding at boot.** For each device and its IOMMU group peers: a
  `driver_override` applied at boot by a oneshot unit
  `glidex-vfio-bind.service`, ordered before `glidex-netd` and the
  hypervisors and after `systemd-modules-load`. It uses the same code as
  netd's prepare (`glidex_ovs::vfio`), plus `vfio-pci` in
  `/etc/modules-load.d/glidex.conf`. Not `driverctl`: one mechanism,
  ours, removed by uninstall.
- **VF counts.** `--sriov PF=N` is applied by the same unit before the
  binding, so the VFs exist to bind. Pools with `vf_of` and
  `prepare: true` then take them as today.
- **The udev rules** (`90-glidex-vfio-<group>.rules`) already persist;
  they stay.
- **NVSwitches** of a node with a fabric service VM are bound the same
  way, for the service VM.
- **Saved and removed.** `install.conf` records `vfio_devices=` and
  `sriov=`. Uninstall removes the unit and the module file, and leaves
  devices on their drivers until reboot.
- **Refusals.** As netd's prepare: a device carrying host addresses or
  an OVS-DPDK uplink, and on prism1 the cordoned devices only if named
  explicitly.

## 11. Security

- **The system project.** System VMs run under it. Only `system-admin`
  reads or changes it; tenants can't see or reach it. The vsock path is
  host-local, between the node's control plane and its own system VM.
- **NVSwitches** are never offered to tenant pools: class `0x0680` from
  NVIDIA is excluded from `assignable`, and the topology marks them
  `fabric` (shown on the host page and in `gxctl node topology`). A
  node with a fabric marks them `held by: system/fabric-manager`.
- **FM's command port.** With FM on the host (§8.2), it stays bound to
  `127.0.0.1` or a Unix socket.
- **`--vfio-devices`** writes root-owned files only, and refuses devices
  that carry host addresses, as netd's prepare does.

## 12. Milestones

| | Scope | Test |
|---|---|---|
| **GV1** Aligned placement and guest layout | `numa_policy: aligned`, the per-node split, `placement.numa[]`, CH and QEMU command lines with per-node memory, guest NUMA nodes and PCI roots | Scheduler units for 1, 2, 4, 8 GPUs on a prism1-shaped node. On prism1 (with the user's OK): a 4-GPU VM on 87, 90, b7, bd, guest `numactl -H` and `nvidia-smi topo -m` showing 2 nodes and `NODE`/`SYS` between them. |
| **GV2** GPU requests and sizing | `gpus` in the API, gxctl and UI; pool `per_device`; the preview shows the split | API and UI tests; a `--gpus 2` VM on prism1 gets 2 GPUs on one node and its sizing. |
| **GV3** `--vfio-devices` and `--sriov` | the boot unit, install.conf, uninstall | Installer tests with a fake sysfs; prism1 reboot test (the user's OK: prism1 also runs OpenNebula). |
| **GV4** System VMs | the system project, node-pinned VMs, boot ordering, vsock, `FabricReady` plumbing (no FM yet) | A test system VM that answers `status` over vsock. |
| **GV5** Fabric Manager | the `glidex-fabric` crate (protocol, client, `FabricBackend`, `FakeFabric`) and `glidex-fabricd` (`libnvfm` loaded at run time) (§8.3); both managers (§8, §8.2) and `--fabric`; the fabric image; partitions in node status and `GET /nodes/{id}/fabric`; NVLink scheduling; activate, deactivate and `set_active` in the VM controller | Scheduler and controller tests against `FakeFabric`. On prism1 (moving the NVSwitches from the host FM to the service VM, with the user's OK and OpenNebula's owner): 2-GPU and 4-GPU VMs, guest `nvidia-smi nvlink -s` up, `nccl-tests` all-reduce at NVLink bandwidth. |
| **GV6** AMD on gpu-dev2 (§2.3) | `LimitMEMLOCK` in the VM unit (G12); the 64-bit window sized from the devices (G13) for QEMU, Cloud Hypervisor and Pylon; `nvlink` resolved to `false` without a fabric (G11); dedicated CPUs left elastic on Pylon (G14); docs for an AMD pool and the guest image (G15); binding before `amdgpu` only if the reboot test needs it (G16); prepare loads `vfio-pci` and shrinks a GPU's window below the host's memory (G17) | Units for the window sizing, the unit file and the `nvlink` default. On gpu-dev2 (with the user's OK, taking the R9700s off `amdgpu`): a pool `r9700`; a 1-GPU VM on Cloud Hypervisor, Pylon and QEMU, guest `lspci` and `amdgpu` up (`rocminfo` with ROCm); 10 stops and starts (Q11); a window of 16 GiB, below the host's 32 GiB (Q12); a dedicated, elastic 1-GPU VM on Pylon parking and waking; a 2-GPU VM; `--vfio-devices r9700` and a reboot (GV3's reboot test). |

GV6 checks on gpu-dev2, run by hand on the installed build (`main` at
`fd36a90`), one R9700 (`03:00.0`), VM `ubuntu-26.04` (kernel
7.0.0-38) on Cloud Hypervisor v53:

| Date | Check | Result |
|---|---|---|
| 2026-10-29 | `gxctl node prepare-device` of `03:00.0` | **fail**: reported `OK … on vfio-pci (was amdgpu)`, but `vfio-pci` wasn't loaded, so the device was left with no driver and not ready. `modprobe vfio-pci` and a re-probe bound it. Fixed by G17's load |
| 2026-10-29 | A 12 GiB VM with the GPU, unit limit as installed | **fail** as G12 says: Cloud Hypervisor exited with `Failed to DMA map VFIO device … Out of memory (os error 12)`, three times |
| 2026-10-29 | The same with `LimitMEMLOCK=infinity` (a drop-in), BAR0 32 GiB | **fail, host froze**: `vfio-pci` reset the GPU twice and the unit started; then nothing more in the journal, no pstore, no kdump, no BERT record. The host needed a power cycle. The platform keeps PCIe error handling (`_OSC` without AER), so a fatal PCIe error is a likely cause; not proven |
| 2026-10-29 | BAR0 resized to 256 MiB (`resource0_resize`, no driver bound), VM at 4 vCPUs and 4 GiB | **pass** for passthrough: the host stayed up over two starts and a stop; the guest sees `[1002:7551]` and loads `amdgpu`, which stops at `early_init of IP block <psp> failed -19`: the cloud image has no `linux-firmware` (G15) |
| 2026-10-29 | The same VM, installing `linux-firmware` in the guest, `scx_pylon` attached (`switch_all`) | **fail, host froze** during the download: `WARNING: kernel/sched/sched.h:1758` in `set_next_task_scx`, `put_prev_task_scx` and `update_curr_scx` (on an idle CPU and on an unrelated host task), then nothing; power-cycled. Reported as CMKL-APEX/pylon#15 |
| 2026-10-29 | The same, `glidex-pylond --no-scx` (`scx_pylon` off), kdump and lockup panics on | **pass**: the host stayed up over the download and a guest reboot. `NOHZ tick-stop error: local softirq work is pending` still appeared 4 times, so it isn't `scx_pylon`'s. In the guest, `amdgpu` initialised: `Detected VRAM RAM=32624M, BAR=256M`, `/dev/kfd` and `/dev/dri/renderD128` |
| 2026-10-29 | Q11: 10 stops and starts of that VM, alternating a hard stop and a graceful one (power button, 60 s), `scx_pylon` off | **pass**: each start, `vfio-pci` reset the GPU (FLR) and the guest's `amdgpu` initialised again with `/dev/kfd`, no `amdgpu` error in the guest, no new warning, AER or `vfio-pci` error on the host; the guest answered in 45–57 s |
| 2026-10-29 | Q12: BAR0 resized to 16 GiB (`resource0_resize` 14, below the host's 32 GiB), the same VM started, then 3 stops and starts (hard, graceful, hard), `scx_pylon` off | **pass**: the host stayed up with no new warning, AER or `vfio-pci` error; each time the guest's `amdgpu` reported `Detected VRAM RAM=32624M, BAR=16384M`, initialised, and `/dev/kfd` was there. So the freeze at 32 GiB came from a window as large as the host's memory, not from a large window as such |
| 2026-10-29 | The 16 GiB VM with `scx_pylon` from CMKL-APEX/pylon#16 (plus a verifier fix) attached, and the VM at `core_scheduling=off` (a test build of `fix/ch-core-scheduling-scx`); the download load for 10 minutes | **fail, host reset**: 6 minutes in, 4 × `sched.h:1758` on CPU 23 in the VM's `vcpu0`; about 6 minutes later the platform reset the host. BERT: fatal execution-unit watchdog timeout on CPU 23 (MCA bank 5). No kdump dump, since the kernel didn't panic. Reported on pylon#16; `scx_pylon` off again |
| 2026-10-29 | Control: the same load and build in a VM **without** a GPU (`load-ch`, `core_scheduling=off`), `scx_pylon` attached | **fail, kernel panic**: about a minute after attach, 6 × `sched.h:1758` in the VM's `vcpu0` during a host ext4 write, 3 more on an idle CPU, then `BUG: kernel NULL pointer dereference` in `pick_next_task`; kdump saved a dump and the host rebooted. So these failures are `scx_pylon`'s, not the GPU's: with `scx_pylon` off, the GPU runs above never warned or hung. Reported on pylon#16 |
| 2026-10-29 | QEMU (`-machine q35`, `vfio-pci,host=0000:03:00.0`): a 1-GPU VM, 4 vCPUs and 4 GiB, BAR0 at 16 GiB, `scx_pylon` off; `linux-firmware` installed in the guest, then a guest reboot | **pass**: the guest's `amdgpu` reported `Detected VRAM RAM=32624M, BAR=16384M` and initialised, with `/dev/kfd` and `/dev/dri/renderD128`; no warning, AER or `vfio-pci` error on the host. QEMU's default 64-bit window (32 GiB) holds one 16 GiB BAR, so G13's sizing wasn't needed here |
| 2026-10-29 | 2-GPU VM on Cloud Hypervisor: both R9700s (`03:00.0`, `43:00.0`) at 8 GiB each (G17's total cap, 16 GiB of 32), 6 vCPUs and 6 GiB, `scx_pylon` off; `43:00.0` needed `prepare-device` first for its `/dev/vfio/53` owner | **pass**: the guest's `amdgpu` initialised on both (`BAR=8192M`, `card0` and `card1`, `renderD128` and `renderD129`, `/dev/kfd`); no warning, AER or lockup on the host |

## 13. Open questions

1. Can a GPU on an HGX A100 board be used in a VM without an activated
   partition? This decides whether FM is required for every GPU VM there
   (§7).
2. How does FM in shared mode identify GPUs, and is the module ID → PCI
   address map stable per board model (§8.1)?
3. Is a secondary bus reset through the PCIe switch needed between
   partitions, or is vfio's FLR enough (§9)?
4. Which Cloud Hypervisor version takes `pci_segments` per guest NUMA
   node with VFIO devices, and the MMIO apertures A100 needs (§5)?
5. prism1 today: should FM move into a service VM, or should the host
   FM switch to shared mode (§8.2)? Either stops the host FM, which
   OpenNebula's VMs share (they get no NVLink now, §2.2).
6. Is `libnvfm`'s API stable across driver branches (575, 580, …), or
   does fabricd need a table of versions it knows (§8.3)?
7. Which FM releases offer the Unix command socket
   (`FM_CMD_UNIX_DOMAIN_SOCKET`), so fabricd on the host needs no TCP
   port (§8.3)?
8. *Settled:* the guest image (G15).
9. *Settled:* Pylon with dedicated CPUs (G14).
10. *Settled:* binding before `amdgpu` (G16).
11. *Answered on gpu-dev2* (§12): FLR is enough to reset an R9700
    between VMs, over 10 hard and graceful stops and starts.
12. *Answered on gpu-dev2* (§12): a 16 GiB window, below the host's
    32 GiB, starts and restarts as safely as 256 MiB (G17).


## 14. Implementation notes

- **Before GV5:**
  - **The topology marks NVSwitches.** NVIDIA vendor and class `0x0680`
    set `PciFunction.fabric` (proto `fabric`, 19). The host page's
    Topology card lists them under "NVLink fabric" with driver, IOMMU
    group and NUMA node, and `gxctl node topology` prints an `nvlink
    fabric` section.
  - **`vfio::group_members` always keeps the device asked for.**
    `0x0680` is a bridge class, and the members skipped every `0x06…`
    function, so preparing an NVSwitch bound nothing. Other `0x06…`
    functions of a group still stay with the host.

- **GV2** (§3, §6):
  - **Pool sizing.** Pools take `per_device {vcpus, memory_mib,
    hugepages?, cpu_policy?}`. `ListPciPools` shows it as
    `per_device_vcpus` and `per_device_memory_mib`; tenant pools inherit
    their cluster pool's.
  - **The request.** `gpus: GpuRequest {count, pool?, nvlink?}` on
    `CreateVmRequest` (34), `PreviewPlacementRequest` (15) and `Vm` (43).
    `VmConfig.gpus` keeps the resolved pool.
  - **Expansion.** `service::vm::expand_gpus` resolves the pool: a named
    cluster or tenant pool, or else the project's only usable pool of
    class `gpu`. `apply_gpus` then fills the create or preview request:
    a `devices` entry (refused if `devices` asks for the same pool),
    `NUMA_POLICY_ALIGNED` and `cpu_policy` when unspecified, `hugepages`
    when absent, and `vcpu_count` and `mem_size_mib` when 0. The REST
    create takes a body without sizing when it has `gpus`.
  - **NVLink.** `nvlink: true` is refused until GV5.
  - **gxctl.** `create` asks "GPUs" (`parse_gpus`: `4`, `4:a100`,
    `8:a100:nvlink`), and `vm show` prints `GPUs:` and each NUMA share.
  - **UI.** The first GPU pool with a count is sent as `gpus`, and fills
    the vCPU and memory fields from `per_device` until they're edited.
    The field limits went up from 32 vCPUs and 32 GiB to 255 and 4 TiB.
  - **Not done.** A `PATCH` of `gpus`: change `devices` and the sizing
    directly for now. The installer doesn't propose `per_device` from
    the topology.

- **GV1** (§4, §5):
  - **The model.** `NumaPolicy::Aligned` (`NUMA_POLICY_ALIGNED`), and
    `Placement.numa: [NumaShare {node, vcpus, memory_mib, devices}]` in
    guest-node order (proto `NumaShare`, `Vm.numa`, `NodeFit.numa`).
  - **The scheduler** (`aligned_fit`) splits by device count, largest
    remainder (`split_by`). Each share is fitted with `fit_on_numa`
    against the CPUs taken so far. All shares must back memory with the
    same page size, or the host is refused, naming the two nodes.
    - Devices on one node: an ordinary fit with `numa_node`, no shares.
    - No devices: `preferred`.
    - Fewer vCPUs than nodes: refused.
  - **CPUs and accounting.** The placement's CPUs are every share's pins
    in guest-node order, then their whole-core siblings, so vCPU *i* stays
    on `cpus[i]`. `loads` counts each share's memory on its node.
  - **Pool picks** under `aligned` sort NUMA nodes by free matching
    devices, most first, so the VM spans the fewest.
  - **Launch.** `VmConfig.host_numa` carries the shares.
    `hypervisor::guest_numa_layout` maps them to vCPU ranges and to the
    devices on each node. Free hugepages are checked per share.
    - Cloud Hypervisor: a `--memory-zone` per share,
      `--platform num_pci_segments=N+1`, and `--numa ... pci_segments=`.
      Segment 0 (virtio) belongs to guest node 0; node *i*'s devices are
      on segment *i+1*.
    - QEMU: a memory backend and a `-numa node` per share, and a
      `pxb-pcie` per guest node (bus numbers 0x40 + 0x18·i, `numa_node=i`)
      with a root port per device.
    - Shared vCPUs still float on the shared pool, as for one node (H15).
  - **Not done:**
    - guest NUMA distances (QEMU `-numa dist`; CH the same, verify);
    - the root port per host PCIe switch, so a GPU and its NIC share one
      in the guest;
    - the MMIO aperture sizing for large BARs;
    - the run on prism1 (§12).

- **GV3** (§10):
  - **Where the logic is.** `glidex_ovs::boot_devices` holds the settings
    (`BootDevices`: `sriov` and `select`) and the pure parts: a
    `Selector` with a pool `match`'s JSON shape, the plan of what
    binding would do, and the "VF in use" check. `glidex-netd
    --boot-devices [FILE]` applies them.
  - **Two paths.** With netd not running (boot: the unit is `Before=`
    netd, the control plane, `libvirtd` and `virtqemud`),
    `Netd::apply_boot_devices` runs directly and records each binding in
    netd's store like `prepare_device`. With netd up (the installer's
    `--now`), `glidex_netd::boot::apply_live` sends `set_sriov_vfs` and
    `prepare_device` to netd.
  - **Not `driverctl`.** Bindings use netd's prepare code, so the udev
    rules and the refusals are the same as for **Prepare**.
  - **What is never touched.** A device already on `vfio-pci`, which a
    VM may hold. Outside boot, a VF count whose PF has a VF on
    `vfio-pci`.
  - **The installer** resolves pools to their `match` from
    control-plane.json when it writes `/etc/glidex/vfio-devices.json`,
    so a changed pool needs a re-run. It writes `vfio-pci` to its own
    `/etc/modules-load.d/glidex-vfio.conf`, separate from the DPDK
    profile's file, which that profile removes. It enables the unit
    without starting it.
  - **Uninstall** stops the unit (unbinding nothing) and removes the
    unit, the settings and the module file.
  - **Not done:** the reboot test on prism1 (scheduled with its owner).
    NVSwitches for a service VM come with GV5.
