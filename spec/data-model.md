# Data model

All persisted and API-exposed types live in
`crates/glidex-control-plane/src/models.rs`.

## Core types

### `VmState`

```rust
pub enum VmState { Created, Running, Paused, Stopped }
```

Serialized lower-case. Valid transitions:

```
Created ─start──▶ Running
Stopped ─start──▶ Running
Running ─pause──▶ Paused
Paused  ─start──▶ Running     (treated as "resume" internally)
Running ─stop───▶ Stopped
Paused  ─stop───▶ Stopped
```

Any other transition is rejected with
`VmManagerError::InvalidState { current, operation }`.

### `VmConfig`

The guest configuration the hypervisor needs to boot:

- `vcpu_count: u8`
- `mem_size_mib: u32`
- `kernel_image_path: String` — empty when booting via firmware
- `firmware_path: Option<String>` — UEFI firmware, normally
  `~/.glidex/CLOUDHV.fd` as downloaded by `glidex-install`;
  when set, the guest boots its disk's bootloader and the kernel fields are
  ignored. Accepted only for `cloudhypervisor`.
- `cloud_init_path: Option<String>` — NoCloud seed disk. `None` on a
  firmware boot → `start_vm` generates a default seed (`cloud_init.rs`) at
  `Vm::default_cloud_init_path()` and passes it to the backend; the
  persisted config is left untouched. The file is removed on delete.
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
  `root_disk_binding` / `data_disk_bindings` carry each disk's recorded
  format to the backend at start.

**Invariant.** `kernel_image_path`, `firmware_path` and `rootfs_path` are tilde-expanded
at the moment `VmConfig` is built from `CreateVmRequest`. Hypervisors
do not do shell expansion themselves; keeping expansion at the API
boundary means every backend sees a filesystem-ready path.

**Invariant.** Once the VM is created, `kernel_image_path` /
`rootfs_path` / `kernel_args` / `hypervisor` do not change — only
`vfio_devices` can be mutated at runtime via
attach/detach-device.

### `Vm`

The persistent record:

```rust
pub struct Vm {
    pub id: String,                    // UUIDv4
    pub name: String,                  // unique, user-chosen
    pub state: VmState,
    pub config: VmConfig,
    pub socket_path: String,           // hypervisor API socket
    pub console_socket_path: String,   // client-facing console
    pub log_path: String,              // captured serial output
    pub hypervisor: HypervisorType,    // duplicated from config for quick access
}
```

`Vm::new` derives the three paths deterministically:

```
/tmp/<prefix>-<id>.sock
/tmp/<prefix>-<id>.console.sock
/tmp/<prefix>-<id>.log
```

where `<prefix>` comes from `HypervisorType::socket_prefix()`:
`cloud-hypervisor` or `qemu`.

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
- `firmware_path` — omitted → direct kernel boot. `create_vm` rejects a
  request that has neither a `kernel_image_path` nor a `firmware_path`.
- `vfio_devices` — omitted → empty list.

`VmResponse` is the API projection — a strict subset of `Vm`:

- `id, name, state, vcpu_count, mem_size_mib, console_socket_path,
  log_path, hypervisor, vfio_devices`.
- Intentionally hides `socket_path` and the full `config` (e.g.
  `kernel_args` is not surfaced), because clients don't need it.

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
image_error | tool_unavailable` (plus the networking codes). `details`
carries extra data, e.g. `min_size_bytes` for a refused shrink.
See [rest-api.md](rest-api.md) for the HTTP status code mapping.

## Persistence schema

### Storage

**ReDB** single-file database at `~/.glidex/glidex.db`
(override via `VmManager::with_db_path`). ReDB is an embedded,
copy-on-write, ACID key-value store — chosen over SQLite to avoid a
C dependency and over sled for its simpler transactional model.

### Table

`vms: TableDefinition<&str, &[u8]>`

- **Key**: `Vm.id` as a `&str`.
- **Value**: `serde_json::to_vec(&vm)` — the whole `Vm` struct.

The same file also holds `credentials`, `networks`, `images` and `disks`
([images.md](images.md#3-data-model)). `VmStore::commit` writes a VM and
the disks it references in one transaction.

We chose JSON (not bincode / postcard) because on-disk records are
rarely migrated and human-inspectable disk state is useful when
debugging. Performance is not a concern at the numbers of VMs
a single host actually runs.

### Write ordering

`VmManager` writes to ReDB **before** updating in-memory state and
**before** taking any externally-visible action.

- `create_vm`: `store.save` → insert into map.
- `start_vm`: spawn + configure + start the hypervisor, then
  `store.update_state(Running)` before flipping `entry.vm.state`.
  If the persist fails, the hypervisor process is killed to keep
  on-disk and process state consistent.
- `pause_vm`: call hypervisor pause, `store.update_state(Paused)`,
  then flip in-memory state. If the persist fails, resume the VM
  via the hypervisor to roll back.
- `stop_vm`: kill the hypervisor process (irreversible), flip
  in-memory state, best-effort `store.update_state(Stopped)` —
  log-and-continue on failure because the process is already gone;
  reconciliation will converge on restart.
- `attach_device` / `detach_device` (running VM): invoke hypervisor
  hot-plug API first, then `store.save` the updated `Vm`. If
  persist fails, roll the hot-plug back.
- `delete_vm`: kill the process, then one `store.commit` that deletes
  the VM, detaches its disks and deletes a disk it owns, then remove from
  the in-memory map and delete the owned disk's file.

### Reconciliation on startup

`VmManager::initialize()` is called once after `main` opens the DB:

1. Load all `Vm`s from ReDB.
2. For each, `reconcile_vm_state`:
   - If persisted state was `Created` or `Stopped`: keep as-is.
   - If `Running` / `Paused`: the control plane has no process
     handle for it anymore. If the hypervisor API socket file
     still exists, an *orphaned* hypervisor process is presumed
     running; we clean up socket files and forcibly mark the VM
     `Stopped`. If the socket file is gone, the hypervisor is
     assumed dead; still mark `Stopped`.
3. If state changed, persist the new state.
4. Insert into the in-memory map with `process: None`.

The effect is: **stale in-memory state never survives a restart.**
The user's config always does.
