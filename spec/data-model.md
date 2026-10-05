# Data model

All persisted and API-exposed types live in
`crates/glidex-control-plane/src/models.rs`.

## Core types

### `PowerState`, `VmPhase` and the derived `VmState`

A VM has a **desired** power state, written by the API, and an
**observed** phase, written by the VM controller
([reconciliation.md §7](reconciliation.md#7-vm-resource)):

```rust
pub enum PowerState { Running, Paused, Stopped }            // spec.power; new VMs: Stopped
pub enum VmPhase { Stopped, Provisioning, Starting, Running,
                   Paused, Stopping, Failed, Unknown }       // status.phase
```

There is no transition table: any `power` may be written in any phase
(start/stop/pause are idempotent spec writes), and the controller works
out the steps. A paused VM given `running` is resumed, not relaunched.

The API shows `state` (`VmState`, lower-case), derived by `Vm::state`
(`models.rs`):

| `phase` | `state` |
|---|---|
| `Stopped` and `never_started` | `created` |
| `Stopped` | `stopped` |
| `Provisioning`, `Starting` | `starting` |
| `Running`, `Paused`, `Stopping`, `Failed`, `Unknown` | same name |

`desired_state` is `spec.power`. **Converged** (`Vm::is_converged`,
D13) means `observed_generation ≥ generation` and condition
`Ready=True`; that is what `?wait` and clients wait for, including for a
VM that should be stopped.

Why `unknown` exists: when the controller cannot establish whether an
instance is live (system bus down, `/proc` unreadable, a socket that
answers oddly), it reports `unknown` and neither launches nor stops
anything, because two hypervisors on one disk would corrupt it.

### `VmConfig`

The guest configuration the hypervisor needs to boot:

- `vcpu_count: u8`
- `mem_size_mib: u32`
- `kernel_image_path: String` — empty when booting via firmware
- `firmware_path: Option<String>` — UEFI firmware: normally the file of
  a firmware image (`<image dir>/<id>.fd`, [images.md](images.md#7-vm-integration)),
  else a host path given by an admin; when set, the guest boots its
  disk's bootloader and the kernel fields are ignored.
- `firmware_image: Option<String>` — the firmware image (id) behind
  `firmware_path`; while set, the image can't be deleted.
- `cloud_init_path: Option<String>` — NoCloud seed disk. `None` on a
  firmware boot → the VM controller regenerates a default seed
  (`cloud_init.rs`) at `Vm::default_cloud_init_path()` before every
  launch and puts it on the command line; the persisted config is left
  untouched. The file goes with the VM's runtime directory on delete.
- `credential: Option<String>` — username of a stored credential the
  generated seed provisions; see [credentials.md](credentials.md). Also
  surfaced on `VmResponse`.
- `rootfs_path: String`
- `kernel_args: String`
- `hypervisor: HypervisorType` (`cloudhypervisor` by default; `#[default]`
  on `HypervisorType::CloudHypervisor` in `hypervisor/mod.rs`)
- `vfio_devices: Vec<String>` — sysfs paths
  (e.g. `/sys/bus/pci/devices/0000:41:00.0`), may be empty
- `root_disk: Option<String>`, `data_disks: Vec<String>`,
  `owns_root_disk: bool` — managed disks by id
  ([images.md](images.md#7-vm-integration)). For a managed root disk,
  `rootfs_path` holds that disk's file path. The non-persisted
  `root_disk_binding` / `data_disk_bindings` (and `nic_bindings`,
  `firmware_vars_path`) are filled in by the controller for
  `HypervisorDriver::launch_args`.

**Invariant.** `kernel_image_path`, `firmware_path` and `rootfs_path` are tilde-expanded
at the moment `VmConfig` is built from `CreateVmRequest`. Hypervisors
do not do shell expansion themselves; keeping expansion at the API
boundary means every backend sees a filesystem-ready path.

**Invariant (admission, `VmManager::check_paths`).** Every path that
reaches a hypervisor command line must not contain `"`, a newline or
another control character (`400 invalid_config`): Cloud Hypervisor's
option values are quoted, not escaped. Under the systemd runner, paths
under `/tmp` and `/var/tmp` are refused too, because the VM unit has its
own `PrivateTmp=`.

**Mutability** ([reconciliation.md §7.3](reconciliation.md#73-mutability-d18)).
Spec edits are accepted in any state; what varies is when they take
effect. `power`, `restart_policy`, `on_host_boot`, `stop_grace_secs`:
at once. `vfio_devices`: hot-plugged while the guest runs (pending,
`DevicesPending`, while paused). `data_disks`, `networks`, `vcpu_count`,
`mem_size_mib`, `credential`, `kernel_args`, `hugepages`: at the next
launch, with `RestartRequired=True` meanwhile. `hypervisor`,
`kernel_image_path`, `firmware_path`, `rootfs_path`, `cloud_init_path`,
`image`, `root_disk`: never (`400 invalid_config`, "immutable field").

**Invariant (claims, D11).** A disk or VFIO device is claimed by a VM
while its spec references it **or** its live instance has it open
(`status.instance.disks` / `vfio_devices`). Dropping a disk from a
running VM's spec does not free it for another VM until that instance
is gone (`VmManager::disk_claimed_by`, `device_claimed_by`).

### `Vm`: `{meta, spec, status}`

```rust
pub struct Vm {
    // meta
    pub id: String,                    // UUIDv4
    pub name: String,                  // unique per project
    pub project: String,
    pub created_at: u64,
    pub generation: u64,               // +1 on every spec change
    pub resource_version: u64,         // +1 on every write (If-Match)
    pub deletion_requested_at: Option<u64>,
    pub finalizers: Vec<String>,       // cleanup still owed before the record goes
    pub spec: VmSpec,                  // written by the API only
    pub status: VmStatus,              // written by the VM controller only
}

pub struct VmSpec {
    pub config: VmConfig,
    pub power: PowerState,
    pub restart_policy: RestartPolicy, // on_failure (default) | never
    pub on_host_boot: HostBootPolicy,  // resume | stop; default reconcile.on_host_boot
    pub stop_grace_secs: u32,          // 0..=300; 0 = stop hard
}
```

`VmStatus` holds `observed_generation`, `conditions`, `phase`,
`instance` (an `InstanceRef`: instance id, runner, boot id,
`launched_generation`, claimed disks and devices, shim and hypervisor
pid + start time), `last_exit`, crash/launch backoff counters,
`stop_deadline`, `nics` (the ports this VM owns in netd) and
`never_started`; field by field in
[reconciliation.md §7.2](reconciliation.md#72-status).

`Vm` (de)serializes through `store::VmRecord`, the on-disk envelope
`{meta, spec, status}`. Runtime paths are not stored: they are derived
from the id (`Vm::paths`, `paths::vm_paths`) under `<run dir>/vms/<id>/`:
`api.sock`, `console.sock`, `console.log` (+ `.1`), `cloudinit.img`,
`launch.json`, `instance.json`, `shim.sock`.

**Invariant (D3, D12).** The API writes `meta` and `spec`; only the
controller writes `status`, recording the generation it *read*, so a
spec change made meanwhile is never undone. The one controller-written
spec field is `power = stopped` after an exit nobody in glidex asked
for (guest power-off, an outside unit stop, a crash with
`restart_policy: never`, a host reboot with `on_host_boot: stop`), and
that write is dropped if the generation moved on.

### `HypervisorType`

```rust
pub enum HypervisorType { CloudHypervisor, Qemu }
```

Serialized lowercase (`"cloudhypervisor" | "qemu"`). Default is
`CloudHypervisor`; the installer always installs it.

**Invariant.** Unknown hypervisor names are rejected at the API
boundary (`422`, serde can't deserialize the enum). Records already in
the database with a hypervisor this build doesn't know — e.g.
`"firecracker"` from a build before its removal — are skipped by
`VmStore::load_all` with a warning, not treated as fatal, so the
control plane still starts. They are left in the database
untouched.
Each variant knows its binary name, socket path prefix, and a
sensible default `kernel_args` string (see `hypervisor/mod.rs`).

## Request / response types

`CreateVmRequest` is the JSON body of `POST /vms`. Optional fields:

- `kernel_args` — omitted → use
  `HypervisorType::default_kernel_args()` for the chosen backend.
- `hypervisor` — omitted → `HypervisorType::default()` (currently `cloudhypervisor`).
- `firmware` / `firmware_path` — omitted → direct kernel boot, except
  that an `image` / `root_disk` VM without a kernel gets the newest
  firmware image for its hypervisor. `create_vm` rejects a request that
  ends up with neither a kernel nor firmware.
- `vfio_devices` — omitted → empty list.
- `power` — omitted → `stopped` (create, then start); `"running"`
  creates and starts in one call. `restart_policy`, `on_host_boot`,
  `stop_grace_secs` default as in `VmSpec`.

`VmResponse` is the API projection (`From<&Vm>`):

- `id, name, project, state, vcpu_count, mem_size_mib, hypervisor,
  vfio_devices, credential, nics, root_disk, data_disks`;
- desired state and the controller's view: `desired_state`,
  `generation`, `observed_generation`, `restart_required`, `conditions`,
  `last_exit`, `restart_policy`, `on_host_boot`, `stop_grace_secs`, and
  `deleting` while a deletion is in progress.
- Hides the runtime paths (spec/security.md §9), `status.instance` and
  most of `config` (e.g. `kernel_args`), because clients don't need it.

`PATCH /vms/{id}` takes `VmPatch` (`state.rs`): a JSON merge patch of
the spec (`power`, `restart_policy`, `on_host_boot`, `stop_grace_secs`,
`config.{vcpu_count, mem_size_mib, kernel_args, credential, hugepages,
vfio_devices, data_disks, networks}`); immutable fields are accepted by
the parser only to be refused.

`DeviceRequest` is the body for attach/detach:

```json
{ "device_path": "/sys/bus/pci/devices/0000:41:00.0" }
```

`ApiError` is the uniform error envelope:

```json
{ "error": "not_found", "message": "VM not found: <id>" }
```

`error` values: `not_found | conflict | invalid_state | invalid_config |
invalid_credential | hypervisor_error | persistence_error |
hypervisor_unavailable | credential_error | invalid_image | invalid_disk |
image_error | tool_unavailable | precondition_failed` (plus the
networking and authorization codes). `details`
carries extra data, e.g. `min_size_bytes` for a refused shrink.
See [rest-api.md](rest-api.md) for the HTTP status code mapping.

## Persistence schema

### Storage

**ReDB** single-file database at `~/.glidex/glidex.db`
(override via `VmManager::with_db_path`). ReDB is an embedded,
copy-on-write, ACID key-value store — chosen over SQLite to avoid a
C dependency and over sled for its simpler transactional model.

### Tables and schema

All values are serde-JSON.

| Table | Key | Value |
|---|---|---|
| `vms` | VM id | `VmRecord` = `{meta, spec, status}` |
| `events` | `vm/<id>` | ring of the last 50 `Event {at, actor, kind, reason, message}` |
| `meta` | `schema_version` | `2` (absent = schema 1, flat `Vm` records) |

The same file also holds `credentials`, `networks`, `images`, `disks`
([images.md](images.md#3-data-model)) and the tenancy and auth tables.
Disks, images and networks are not envelopes yet (M3).
Metering adds `meter_cursors`, `meter_open`, `usage_hourly` and
`meter_meta` ([metering.md §7.1](metering.md#71-tables), written only by
the meter, one transaction per sampling round). These are new tables, so
`schema_version` stays 2 and older builds ignore them.
`VmStore::commit` writes a VM, the disks whose claims it changes and its
events in one transaction. Event actors are `api:<principal>`,
`controller`, `guest`, `systemd`, `host`; the ring is deleted with the
VM.

We chose JSON (not bincode / postcard) because on-disk records are
rarely migrated and human-inspectable disk state is useful when
debugging. Performance is not a concern at the numbers of VMs
a single host actually runs.

**Migration (schema 1 → 2,** `VmStore::migrate`, at startup, one
transaction). Each flat record becomes an envelope with `spec.power =
stopped` (the release that wrote it killed its guests when it stopped),
`status.phase = stopped`, `never_started` from the old `created` state,
`on_host_boot` from `reconcile.on_host_boot`, `generation = 1`. Records this build
can't read are left untouched. `VmStore::open` refuses a database whose
`schema_version` is newer than it knows ("database is newer than this
glidex"), so a downgrade fails cleanly instead of misreading envelopes.

### Who writes what

The write rules are [reconciliation.md §6.2](reconciliation.md#62-who-writes-what):

- API handlers (admission in `state.rs`) write `meta` and `spec` in one
  store commit together with every check that needs atomicity (claims,
  quotas, name uniqueness), under the cache's write lock, and bump
  `generation` iff the spec changed. Nothing is done on the host first,
  so there is nothing to roll back.
- The VM controller writes `status` (`VmManager::write_status`), with
  each step's record written **before** the host action it describes:
  a NIC goes into `status.nics` before `attach_vm_port`; the intent
  (`status.instance`) is written before the shim is started. A crash
  between any two steps is finished or undone by the next reconcile.
- Deletion sets `deletion_requested_at` and the finalizers (`vm.instance`,
  `vm.ports`, `vm.disks`, `vm.owned-disk` unless `?keep_disk=true`,
  `vm.runtime`); the controller stops the instance, releases the ports,
  clears claims, deletes an owned root disk and the runtime directory,
  then the record. A deleting VM refuses spec writes (`409`).

### Startup

`VmManager::initialize()` migrates, loads every VM into the cache and
**adopts** live instances; it never marks a VM stopped because the
control plane restarted ([reconciliation.md §9.4](reconciliation.md#94-startup-order)).
A VM whose recorded instance is gone is handled by its first reconcile
like any other exit (`HostReboot` after a reboot, `Lost` for an intent
that never ran). **Stale status never survives a reconcile; the user's
spec always survives a restart.**
