# Pylon and CPU elasticity

Status: **design** (§10 milestones). Extends
[hypervisors.md](hypervisors.md) (drivers, command lines, live resize),
[host-topology.md](host-topology.md) (CPU policies, reserved CPUs,
scheduler), [data-model.md](data-model.md) (`VmConfig`, `HypervisorType`),
[reconciliation.md](reconciliation.md) (live vCPU drift, VM sandbox) and
[installer.md](installer.md).

A VM in glidex today gets a fixed `vcpu_count` at launch, with one of two
CPU policies ([host-topology.md](host-topology.md) H6):

- `cpu_policy: shared`: every vCPU thread floats over the host's
  shareable CPUs under Linux EEVDF (`sched_fair`). When two VMs burst on
  the same physical CPU, the host time-slices their vCPU threads, causing
  lock-holder preemption, IPI spin wait and cache thrashing.
- `cpu_policy: dedicated`: each vCPU owns a physical CPU exclusively.
  There is no double-scheduling tax, but idle cores sit unused and a VM
  cannot burst beyond its static allocation.

Changing `vcpu_count` on a running VM always sets `RestartRequired=True`
([data-model.md](data-model.md)) and waits for a reboot, even though all
three hypervisor backends can change a running guest's active CPU count.

This document makes two changes:

1. **Generalized CPU elasticity across all hypervisors (§3–§5):** a VM
   can specify `min_vcpus`, `max_vcpus` and `cpu_weight` alongside
   `vcpu_count`. On **Pylon**, the guest and `pylond` scale active
   physical cores between `min_vcpus` (`0..=max_vcpus`) and `max_vcpus`
   autonomously in ~3–6 µs. On **Cloud Hypervisor** and **QEMU**, the
   same fields configure hotpluggable vCPU slots (`boot=vcpu_count,
   max=max_vcpus`), apply `cpu_weight` through cgroup v2 `cpu.weight`,
   and actuate live `vcpu_count` edits through **ACPI CPU hotplug**
   (`PUT /vm.resize` on Cloud Hypervisor, QMP `device_add` / `device_del`
   on QEMU) without restarting the VM.
2. **Pylon (`HypervisorType::Pylon`) as a third hypervisor backend
   (§6–§9):** integrates `CMKL-APEX/pylon` (the Pylon VMM binary,
   `pylond` node arbiter daemon and `scx_pylon` `sched_ext` BPF
   dispatcher) into glidex's driver trait, shim allowlist, systemd
   sandbox, node discovery, scheduler and installer.

## 1. Scope

- **In:**
  - Generalized elasticity fields (`min_vcpus`, `max_vcpus`,
    `cpu_weight`) on `VmConfig` and the REST, RPC, CLI and UI layers.
  - Live vCPU resize (`HypervisorDriver::resize_vcpus`) in the VM
    controller for running VMs when `vcpu_count` changes within
    `[min_vcpus, max_vcpus]`.
  - `HypervisorType::Pylon` (`pylon` binary), its `--pylon` launch
    arguments, 8 KiB shared control page (`PylonSharedPage`), and
    `ChClient` reuse.
  - Host arbiter daemon (`glidex-pylond.service`) and `scx_pylon` BPF
    scheduler lifecycle, core pool partitioning with
    [host-topology.md](host-topology.md), and fallback when Pylon or
    `sched_ext` is unavailable on a host.
  - Quotas and metering for elastic VMs.
- **Out:**
  - Guest kernel driver internals (`pv-pylon` Linux kmod and Asterinas
    `ostd` hooks live in `CMKL-APEX/pylon`).
  - Live memory resize (virtio-mem / ACPI memory hotplug).
  - Cross-host live migration of Pylon VMs.

## 2. Decisions

| # | Topic | Decision | Reason |
|---|---|---|---|
| P1 | Generalized parameters | `VmConfig` gains `min_vcpus: Option<u8>`, `max_vcpus: Option<u8>` and `cpu_weight: Option<u32>` (1..=10000, default 100). They are valid on **every** hypervisor (`pylon`, `cloudhypervisor`, `qemu`). When omitted, `min_vcpus = max_vcpus = vcpu_count` and `cpu_weight = 100`. | Callers express workload elasticity once (`min_vcpus`, `max_vcpus`, `cpu_weight`); the backend chooses the actuation mechanism (µs shared-memory parking on Pylon, 10–100 ms ACPI hotplug + cgroup v2 weight on Cloud Hypervisor and QEMU). |
| P2 | Actuation per backend | On `pylon`, all `max_vcpus` are created at boot (`--cpus boot=max,max=max`), `vcpu_id >= vcpu_count` start parked in the VMM (`initial_active_vcpus=vcpu_count`), and `pylond` scales active cores in `[min_vcpus, max_vcpus]` by guest demand and `cpu_weight`. On `cloudhypervisor` and `qemu`, the guest boots with `vcpu_count` online CPUs and `max_vcpus` ACPI slots; `PATCH` of `vcpu_count` within `[min_vcpus, max_vcpus]` resizes live over the hypervisor API without setting `RestartRequired`. | Pylon avoids ACPI `stop_machine` overhead by parking already-online vCPUs in the VMM; stock Cloud Hypervisor and QEMU have no shared-page park loop, so ACPI hotplug + cgroup weight is their native mechanism. |
| P3 | Scale-to-zero (`min_vcpus = 0`) | `min_vcpus = 0` is allowed on `pylon` (0-core hibernation; `pylond` wakes a vCPU on guest timer expiry or host virtio RX doorbell). On `cloudhypervisor` and `qemu`, `min_vcpus` is clamped to at least 1 (or rejected at admission when `hypervisor != pylon`), because x86/ARM guests cannot ACPI-offline their bootstrap processor (`CPU0`). | Preserves Pylon's scale-to-zero capability while respecting the architectural floor of ACPI hotplug. |
| P4 | Hypervisor identity & fallback | `HypervisorType` gains `Pylon` (`"pylon"`, proto `HYPERVISOR_PYLON = 3`, binary `/usr/local/bin/pylon`). In addition, a node without `pylon` or `pylond` can still run any elastic VM configured for `cloudhypervisor` or `qemu` via ACPI hotplug; if a VM requests `cpu_policy: elastic` without pinning `hypervisor`, the scheduler prefers nodes advertising feature `pylon` and falls back to `cloudhypervisor` ACPI hotplug when none is present. | Lets clusters mix Pylon-capable nodes (kernel ≥ 6.12 with `CONFIG_SCHED_ext`) and standard KVM hosts seamlessly. |
| P5 | Firmware sharing | `HypervisorType::Pylon` shares the `cloudhv-edk2` (`CLOUDHV.fd` / `CLOUDHV_EFI.fd`) firmware catalog entry and images with `HypervisorType::CloudHypervisor`. | Pylon is a Cloud Hypervisor fork and boots the exact same EDK2 build; duplicating downloads or image records wastes disk and operator steps. |
| P6 | Shim & client reuse | `PylonDriver` uses `ChClient` (`glidex-hv-client`) for all control-socket operations (`ping`, `observe`, `pause`, `resume`, `add_device`, `remove_device`, `resize_vcpus`, `power_button`, `shutdown_vmm`, `block_stats`). `HypervisorType::Pylon::launch_kind()` maps to `HypervisorKind::CloudHypervisor` in `launch.json`, and `/usr/local/bin/pylon` is added to the shim's default binary allowlist. | Pylon's HTTP API over `--api-socket` is byte-for-byte identical to Cloud Hypervisor's. |
| P7 | Shared page & socket paths in sandbox | Each Pylon VM's 8,192-byte shared page is placed at `/run/glidex-cp/vms/<vm-id>/pylon.shm` (inside the VM's existing tmpfs runtime directory, `0600` `glidex:glidex`), not `/dev/shm`. `glidex-vm@.service` adds `-/run/pylon` to `ReadWritePaths` so the VMM can connect to `/run/pylon/pylond.sock` and send its `shm` and `eventfd` descriptors via `SCM_RIGHTS`. | Keeps `/dev/shm` closed under `DevicePolicy=closed` and avoids world-visible files in `/dev/shm`. |
| P8 | Host core partitioning | On a node running `pylond`, Core 0 runs `pylond` Plane 1 (control socket) + OS/housekeeping, Core 1 (or the first sibling-free core reserved for it) runs `pylond` Plane 2 (the `_rdtsc`/`_tpause` matrix engine), and both are recorded in `TopologySummary.reserved_cpus` (`ReservedFor::Pylon`). `pylond` manages the node's shareable CPU pool (online CPUs minus `reserved_cpus` minus `dedicated` VMs' CPUs). | Prevents `dedicated` VMs, OVS-DPDK PMDs and `pylond`'s polling engine from stepping on each other. |
| P9 | Quotas and scheduler accounting | Quotas and scheduler load count `vcpu_count` as the VM's baseline CPU request (matching non-elastic VMs) and check that `max_vcpus` does not exceed the node's (or NUMA node's) shareable CPU count. Metering samples actual cgroup v2 `cpu.stat` (`usage_usec`), which reflects the exact physical CPU time consumed under both Pylon and ACPI hotplug. | Tenants are billed for the CPU time they actually consume and cannot create a VM whose `max_vcpus` can never fit on its host. |

## 3. Generalized CPU Elasticity Model

### 3.1 `VmConfig` and API fields

`VmConfig` (`crates/glidex-control-plane/src/models.rs`) gains three
optional fields:

```rust
pub struct VmConfig {
    pub vcpu_count: u8,
    /// Minimum active vCPUs (`0..=vcpu_count` on Pylon; `1..=vcpu_count`
    /// on Cloud Hypervisor and QEMU). Absent: `vcpu_count` (fixed size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_vcpus: Option<u8>,
    /// Maximum active vCPUs (`vcpu_count..=64` on Pylon; `vcpu_count..=255`
    /// on Cloud Hypervisor and QEMU). Absent: `vcpu_count` (fixed size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vcpus: Option<u8>,
    /// Relative CPU scheduling weight (`1..=10000`, default `100`).
    /// Drives `pylond` `priority_weight` on Pylon and cgroup v2
    /// `cpu.weight` on all hypervisors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_weight: Option<u32>,
    // ...
}
```

Helper methods on `VmConfig`:

- `effective_min_vcpus(&self) -> u8`: `self.min_vcpus.unwrap_or(self.vcpu_count)`
- `effective_max_vcpus(&self) -> u8`: `self.max_vcpus.unwrap_or(self.vcpu_count)`
- `effective_cpu_weight(&self) -> u32`: `self.cpu_weight.unwrap_or(100)`
- `is_elastic(&self) -> bool`: `self.effective_min_vcpus() < self.effective_max_vcpus()`

### 3.2 Admission invariants

Checked at `POST /vms` and `PATCH /vms/{id}` (`state.rs`):

1. `vcpu_count >= 1`.
2. `min_vcpus <= vcpu_count <= max_vcpus` (`400 invalid_config` if
   violated).
3. `min_vcpus == 0` is accepted only when `hypervisor == Pylon` (or when
   `cpu_policy == Elastic` and the cluster has a Pylon node); on
   `cloudhypervisor` and `qemu`, `min_vcpus == 0` returns `400
   invalid_config` (`"min_vcpus = 0 (scale-to-zero) requires the pylon
   hypervisor; cloudhypervisor and qemu require min_vcpus >= 1"`).
4. On `pylon`, `max_vcpus <= 64` (`pylon_abi::MAX_VCPUS`).
5. `cpu_weight` must be in `1..=10000` (matching Linux cgroup v2
   `cpu.weight`).
6. `cpu_policy: dedicated` requires `min_vcpus == max_vcpus == vcpu_count`
   (`400 invalid_config`: dedicated VMs pin 1:1 to host CPUs and do not
   scale elastically).

### 3.3 Cross-hypervisor translation matrix

| Parameter / Operation | `pylon` (`CMKL-APEX/pylon`) | `cloudhypervisor` (upstream CH) | `qemu` (`qemu-system-*`) |
|---|---|---|---|
| **Boot topology** | `--cpus boot=<max>,max=<max>` | `--cpus boot=<vcpu_count>,max=<max>` | `-smp cpus=<vcpu_count>,maxcpus=<max>,sockets=1,cores=<max>,threads=1` |
| **Initial active vCPUs** | `--pylon …,initial_active_vcpus=<vcpu_count>` (`vcpu_id >= vcpu_count` start in `VCPU_STATE_PARKED`) | `boot=<vcpu_count>` (higher vCPUs are offline in ACPI MADT until hotplugged) | `cpus=<vcpu_count>` (higher CPU slots are unpopulated until `device_add`) |
| **Elasticity mechanism** | Cooperative shared-memory (`GPA 0xFEDF_0000`) + `pylond` + `scx_pylon` BPF (`~3–6 µs` handoff, zero guest `stop_machine`) | Host-driven ACPI CPU hotplug (`PUT /api/v1/vm.resize`, `10–100 ms`) | Host-driven ACPI CPU hotplug (QMP `device_add` / `device_del` `host-x86_64-cpu`, `10–100 ms`) |
| **Scale-to-zero (`min_vcpus = 0`)** | Supported: all vCPUs park; `pylond` wakes on `next_timer_tsc` or virtio RX doorbell | Not supported (`min_vcpus >= 1`; `CPU0` stays online) | Not supported (`min_vcpus >= 1`; `CPU0` stays online) |
| **Relative priority (`cpu_weight`)** | `--pylon …,priority_weight=<cpu_weight>` in `pylond` Matrix Engine + cgroup v2 `cpu.weight` | cgroup v2 `cpu.weight` on `glidex-vm@<id>.service` | cgroup v2 `cpu.weight` on `glidex-vm@<id>.service` |
| **Live `PATCH` of `vcpu_count` within `[min, max]`** | Updates `pylond` / shared page target without restart (`RestartRequired=False`) | Controller calls `PUT /api/v1/vm.resize` (`{"desired_vcpus": N}`); `RestartRequired=False` | Controller calls QMP `device_add` / `device_del` to match `N`; `RestartRequired=False` |
| **Live `PATCH` of `max_vcpus`** | Sets `RestartRequired=True` (applies at next boot) | Sets `RestartRequired=True` (ACPI MADT size is fixed at boot) | Sets `RestartRequired=True` (`maxcpus` is fixed at boot) |

## 4. Live vCPU Resize on Cloud Hypervisor and QEMU (ACPI Hotplug Fallback)

When a VM runs on `cloudhypervisor` or `qemu` (either by explicit choice
or because `pylon` is not installed on the node), `max_vcpus > vcpu_count`
prepares the guest's ACPI tables at boot for CPU hotplug.

### 4.1 Driver trait extension

`HypervisorDriver` (`crates/glidex-control-plane/src/hypervisor/mod.rs`)
and `Observed` (`crates/glidex-hv-client/src/lib.rs`) gain:

```rust
pub struct Observed {
    pub guest: GuestState,
    pub device_ids: Vec<String>,
    /// Currently online/allocated vCPUs reported by the VMM, when known.
    pub vcpu_count: Option<u8>,
}

pub trait HypervisorDriver: Send + Sync {
    // ... existing methods ...
    /// Resize the running guest's active vCPU count to `desired`
    /// (`min_vcpus..=max_vcpus`).
    fn resize_vcpus(&self, api_socket: &str, desired: u8) -> Result<(), HypervisorError>;
}
```

### 4.2 Per-backend implementation of `resize_vcpus` and `observe`

1. **Cloud Hypervisor (`ChClient`):**
   - `observe`: reads `/config/cpus/boot_vcpus` from `GET /api/v1/vm.info`
     into `Observed.vcpu_count` (CH updates `boot_vcpus` in `vm.info` on
     every `/vm.resize`).
   - `resize_vcpus(desired)`: sends `PUT /api/v1/vm.resize` with body
     `{"desired_vcpus": desired}`. Cloud Hypervisor updates ACPI CPU
     status and injects an ACPI GPE interrupt; the guest kernel onlines
     or offlines the vCPUs.
2. **QEMU (`QmpClient`):**
   - Launch: when `max_vcpus > vcpu_count`, QEMU is started with
     `-smp cpus=<vcpu_count>,maxcpus=<max_vcpus>,sockets=1,cores=<max_vcpus>,threads=1`.
   - `observe`: `query-cpus-fast` returns the currently attached vCPUs;
     `Observed.vcpu_count` is its length.
   - `resize_vcpus(desired)`: runs `query-hotpluggable-cpus` to inspect
     populated and unpopulated core slots.
     - To grow (`current < desired`): for each unpopulated slot up to
       `desired`, sends `device_add` with `driver` from the slot's
       `type` (e.g. `host-x86_64-cpu`), `socket-id`, `core-id`,
       `thread-id` and deterministic `id = "vcpu_<core_id>"`.
     - To shrink (`current > desired`): for each hotplugged slot
       `vcpu_<i>` where `i >= desired`, sends `device_del` with
       `id = "vcpu_<i>"`.
3. **Pylon (`PylonDriver`):**
   - Autonomous scaling within `[min_vcpus, max_vcpus]` happens in
     `pylond` without controller intervention.
   - If the user changes `vcpu_count`, `min_vcpus` or `cpu_weight` via
     `PATCH /vms/{id}`, `resize_vcpus` sends an updated registration /
     policy frame to `/run/pylon/pylond.sock` (or updates the VM's
     shared page floor/target), taking effect immediately.

### 4.3 Controller reconciliation (`reconcile_running`)

In `crates/glidex-control-plane/src/controller/vm.rs`:

- **Drift check (step 7b, alongside VFIO hotplug):** when a running VM's
  `observed.vcpu_count` differs from `vm.spec.config.vcpu_count` and the
  target `vcpu_count` is within `1..=launched_max_vcpus`, the controller
  calls `driver.resize_vcpus(&paths.api_socket, vm.spec.config.vcpu_count)`.
- **`RestartRequired` (`Vm::restart_required`):** a change to
  `vcpu_count` or `min_vcpus` or `cpu_weight` while running does **not**
  set `RestartRequired=True` as long as the new `vcpu_count` is within
  `[1, launched_max_vcpus]` (or `[0, launched_max_vcpus]` on Pylon) and
  `max_vcpus` itself has not changed.

## 5. Cgroup `cpu.weight` Actuation

`launch.json` (`crates/glidex-vm-shim/src/launch.rs`) gains:

```rust
#[serde(default = "default_cpu_weight")]
pub cpu_weight: u32, // 1..=10000, default 100
```

At launch (and on a live `SetCpuWeight` shim request when `cpu_weight`
is patched), `glidex-vm-shim` writes `cpu_weight` to its unit's cgroup v2
`cpu.weight` file (`/proc/self/cgroup` →
`/sys/fs/cgroup/glidex-vms.slice/glidex-vm@<id>.service/cpu.weight`) when
running under the systemd runner. This gives every hypervisor (`pylon`,
`cloudhypervisor`, `qemu`) proportional host CPU share out of the box.

## 6. Pylon VMM Driver (`HypervisorType::Pylon`)

### 6.1 Command line (`hypervisor/pylon.rs`)

`PylonDriver` builds on `cloud_hypervisor::args(config, api_socket)`
with three differences:

1. **Binary:** resolves `pylon` (`/usr/local/bin/pylon` or `/usr/bin/pylon`).
2. **`--cpus`:** sets `boot=<max_vcpus>,max=<max_vcpus>` so all possible
   vCPUs exist in the guest's MADT at boot and can be parked/unparked in
   microseconds without ACPI hotplug.
3. **`--pylon`:** appends:
   ```text
   --pylon shm=/run/glidex-cp/vms/<vm-id>/pylon.shm,socket=/run/pylon/pylond.sock,gpa=0xfedf0000,initial_active_vcpus=<vcpu_count>,min_cores=<min_vcpus>,max_cores=<max_vcpus>,priority_weight=<cpu_weight>,vm_id=<u64_id>
   ```
   where:
   - `shm` is `<vm_runtime_dir>/pylon.shm` (derived from `api_socket`'s
     parent directory `/run/glidex-cp/vms/<vm-id>`), quoted with `q()` if
     needed;
   - `vm_id` is the lower 64 bits of the VM's UUIDv4 (non-zero; `1` if
     the fold is `0`), giving `pylond` and `PylonHeader.vm_id` a
     deterministic numeric identifier.

### 6.2 Shared memory ABI & Guest telemetry (`pylon-abi`)

The Pylon VMM maps `/run/glidex-cp/vms/<vm-id>/pylon.shm` (8,192 bytes,
`MAP_SHARED`, `MemoryVisibility::Shared`) at Guest Physical Address
`0xFEDF_0000`:

- `0x0000..0x0040`: `PylonHeader` (`magic = 0x50594C4F` `"PYLO"`,
  `version = 1`, `num_vcpus`, `vm_id`, `tsc_khz`).
- `0x0040..0x0080`: `PylonTelemetry` (guest-written `runnable_threads`,
  `head_wait_tsc`, `next_timer_tsc`, `rx_ring_cons_idx`,
  `stranded_work_bitmap`; host-written `allocated_cores`,
  `host_rx_doorbell`, `forced_preempt_count`).
- `0x1000..0x2000`: `[PylonVcpuSlot; 64]` (64-byte cache-line-aligned
  slots holding `state`, `in_critical_section`, `yield_count`,
  `revoke_requested`, `assigned_pcpu`, `host_tid`,
  `revoke_deadline_tsc`).

Unmodified guests without `pv-pylon` still boot and run normally on
Pylon: `initial_active_vcpus` (`vcpu_count`) start in
`VCPU_STATE_RUNNING`, and the guest can still be resized via `/vm.resize`
or `pylond` floor changes. Guests with `pv-pylon` write demand into
`PylonTelemetry` and respond to `revoke_requested` IPIs by parking at
safe preemption points (`in_critical_section == 0`).

## 7. Host Daemon (`glidex-pylond`) and Sandbox

### 7.1 `packaging/glidex-pylond.service`

`pylond` runs as a root systemd service on Pylon-enabled hosts (root is
required to load the `scx_pylon` eBPF `sched_ext_ops` program and pin its
polling engine thread):

```ini
[Unit]
Description=glidex Pylon microsecond core arbiter daemon
Documentation=https://github.com/CMKL-APEX/pylon
Before=glidex-control-plane.service

[Service]
Type=simple
User=root
Group=glidex
UMask=0007
RuntimeDirectory=pylon
RuntimeDirectoryMode=0750
ExecStart=/usr/local/bin/pylond --socket /run/pylon/pylond.sock --cores ${PYLON_CORES}
Restart=on-failure
RestartSec=2
```

- `Group=glidex` and `UMask=0007` make `/run/pylon/pylond.sock` `0770
  root:glidex`, so per-VM `glidex-vm@<id>.service` units (which run as
  `User=glidex`, `Group=glidex`) can connect and send their `SCM_RIGHTS`
  file descriptors, while unprivileged users on the host cannot.
- If `pylond` is stopped or restarted, running Pylon VMs stay up (their
  currently running vCPUs continue executing in KVM).

### 7.2 VM Unit Sandbox (`packaging/glidex-vm@.service.in`)

`glidex-vm@.service.in` adds `-/run/pylon` to `ReadWritePaths`:

```ini
ReadWritePaths=@HOME@ /run/glidex-cp/vms/%i -/run/glidex/vhost -/run/pylon -/srv/glidex
```

The `-` prefix means `systemd` ignores the path on hosts where
`glidex-pylond` is not installed. Because `pylon.shm` lives inside
`/run/glidex-cp/vms/%i`, `/dev/shm` remains inaccessible to VMs.

## 8. Node Discovery, Topology & Scheduler

### 8.1 Node features (`node.rs`)

`SelfProbe` (`crates/glidex-control-plane/src/node.rs`) checks
`[HypervisorType::CloudHypervisor, HypervisorType::Qemu, HypervisorType::Pylon]`.
`PylonDriver::is_available()` returns `true` when `resolve_binary("pylon")`
is `Some`. In addition, `NodeFeatures` reports `pylond: bool`
(`Path::new("/run/pylon/pylond.sock").exists()`).

### 8.2 Core pool coordination with `host-topology.md`

- `pylond` requires two host CPUs for itself (Plane 1 control thread and
  Plane 2 polling engine) and manages a set of tenant pCPUs.
- When `glidex-pylond` is active, its two engine CPUs are included in
  `TopologySummary.reserved_cpus`.
- Whenever a `dedicated` VM starts or stops on the node
  ([host-topology.md §7.1](host-topology.md#71-cpu-binding)), the VM
  controller updates the shared CPU pool (`local_shared_pool()`), which
  rebinds shared hypervisors and updates `pylond`'s tenant pCPU mask so
  `pylond` never dispatches elastic vCPUs onto a `dedicated` VM's pinned
  cores.

### 8.3 Scheduler filters

1. A VM with `hypervisor: pylon` requires a node whose
   `features.hypervisors` contains `"pylon"`.
2. For any elastic VM (`max_vcpus > vcpu_count`), the node's shareable
   CPUs (or chosen NUMA node's shareable CPUs under `numa_policy:
   strict`) must be at least `max_vcpus`, while overcommit accounting
   counts `vcpu_count`.

## 9. Installer (`glidex-install`)

- `glidex-install` gains `--with-pylon` (or installs `pylon` and `pylond`
  automatically when present in the release bundle / local build):
  - Installs `/usr/local/bin/pylon` (`0755`, with `cap_net_admin=ep` set
    via `setcap`, matching `cloud-hypervisor` so tap devices work inside
    non-root `glidex-vm@` units).
  - Installs `/usr/local/bin/pylond` (`0755`) and
    `/etc/systemd/system/glidex-pylond.service`.
  - Updates `/etc/glidex/vm-shim.json` (when written) and
    `glidex-install uninstall` (`--remove-pylon` / `--remove-cloud-hypervisor`).

## 10. Milestones

Stacked PRs on `main`:

| Milestone | Scope | Key files |
|---|---|---|
| **P0: Spec** | This document (`spec/pylon.md`), linked in `spec/README.md`, `spec/SUMMARY.md`, `spec/progress.md`, and cross-referenced in `spec/hypervisors.md` and `spec/data-model.md`. | `spec/pylon.md`, `spec/hypervisors.md`, `spec/README.md`, `spec/SUMMARY.md`, `spec/progress.md` |
| **P1: `HypervisorType::Pylon` driver & shim** | Add `HYPERVISOR_PYLON = 3` in `common.proto`; add `HypervisorType::Pylon`, `PylonDriver` (`--pylon shm=…`), firmware sharing with `cloudhv-edk2`, shim allowlist (`pylon`), `glidex-vm@.service.in` `-/run/pylon` path, `gxctl` and UI picker. | `crates/glidex-api/proto/glidex/common/v1/common.proto`, `crates/glidex-vm-shim/src/launch.rs`, `crates/glidex-control-plane/src/hypervisor/`, `packaging/glidex-vm@.service.in`, `crates/glidex-ui/` |
| **P2: Generalized elasticity (`min_vcpus`, `max_vcpus`, `cpu_weight`) & ACPI hotplug fallback** | Add `min_vcpus`, `max_vcpus`, `cpu_weight` to `VmConfig` and protos; pass `max_vcpus` to CH (`--cpus boot=,max=`) and QEMU (`-smp cpus=,maxcpus=`); implement `HypervisorDriver::resize_vcpus` (CH `PUT /vm.resize`, QEMU QMP `device_add`/`device_del`) and controller live drift without `RestartRequired`; apply `cpu_weight` to cgroup v2 `cpu.weight` in `glidex-vm-shim`. | `crates/glidex-api/proto/glidex/vm/v1/vm.proto`, `crates/glidex-hv-client/src/{ch,qmp}.rs`, `crates/glidex-control-plane/src/{models,state,hypervisor,controller/vm}.rs`, `crates/glidex-vm-shim/src/{launch,supervisor}.rs` |
| **P3: `glidex-pylond` daemon, installer & topology integration** | Package `glidex-pylond.service`; add `pylon` and `pylond` installation and capabilities to `glidex-install`; report `pylond` status in `NodeFeatures` and reserve `pylond` engine cores in `host-topology`. | `packaging/glidex-pylond.service`, `crates/glidex-install/src/{main,uninstall}.rs`, `crates/glidex-control-plane/src/node.rs` |
| **P4: End-to-end tests** | Unit and functional tests covering `PylonDriver` argv, live vCPU resize on CH and QEMU, cgroup `cpu.weight`, and Pylon VM registration against `pylond`. | `crates/glidex-control-plane/src/hypervisor/`, `crates/glidex-e2e/` |

## 11. Implementation status

| Milestone | Spec | Code | Tests | Not yet |
|---|---|---|---|---|
| P0 | Done | — | — | — |
| P1 | Done | Done | Done | — |
| P2 | Done | Done | Done | — |
| P3 | Done | Done | Done | — |
| P4 | Done | Not yet | Not yet | Host e2e |
