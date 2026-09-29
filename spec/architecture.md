# Architecture

## Processes

Glidex is a collection of cooperating OS processes. Nothing here is
containerized; everything runs as ordinary processes on a single Linux
host.

```
 ┌───────────────────────────┐         ┌──────────────────────────┐
 │  Browser (xterm.js)       │         │  gxctl (interactive CLI) │
 └──────────────┬────────────┘         └──────────────┬───────────┘
                │ HTTP + WebSocket                    │ HTTP + Unix
                ▼                                     ▼
 ┌────────────────────────────────────────────────────────────────┐
 │  glidex-control-plane  (axum, :8080)                           │
 │  ├── REST API            (api.rs)                              │
 │  ├── Console WS bridge   (api.rs::console_ws)                  │
 │  ├── VmManager           (state.rs)   — in-memory VM registry  │
 │  ├── VmStore / ReDB      (persistence.rs)                      │
 │  ├── Hypervisor backends (hypervisor/{cloud_hypervisor,qemu}.rs)
 │  └── PCI scanner         (pci.rs)                              │
 └──────────────┬──────────────────────────┬──────────────────────┘
                │ Unix socket              │ PTY proxy thread
                ▼                          │ (broadcasts to console socket)
 ┌────────────────────────┐    ┌────────────┴───────────┐
 │ Hypervisor API socket  │    │ /tmp/<prefix>-<id>.console.sock
 │ (CH HTTP, QEMU QMP)    │    │ (serial console + replay log)
 │                        │    └────────────────────────┘
 └──────────┬─────────────┘
            ▼
 ┌────────────────────────┐
 │ Hypervisor process     │
 │ (cloud-hypervisor /     │
 │  qemu-system-x86_64)    │
 └────────────────────────┘
```

Separately, the UI crate (`glidex-ui`) spawns `bun run dev` from
`crates/glidex-ui/ui/` to serve a Vite dev server on `:5173`; Vite
proxies `/api/**` (including WebSocket upgrades) to the control plane
on `:8080`. In production one would build the UI and serve it
statically, but that path is not wired up yet.

## Control-plane layers

Reading top-to-bottom inside `crates/glidex-control-plane/src/`:

- **`main.rs`** — process entrypoint. Checks `/dev/kvm`, opens the
  ReDB database at `~/.glidex/glidex.db`, calls `VmManager::initialize()`
  to reconcile persisted VMs, builds the axum router with
  `TraceLayer`, and binds `:8080`. On shutdown it invokes
  `VmManager::shutdown()` to kill every running hypervisor process
  before exiting.
- **`api.rs`** — axum `Router`. Thin translation between HTTP and
  `VmManager` methods, plus the console WebSocket bridge.
- **`state.rs`** — `VmManager`: the single source of truth for VM
  state at runtime. Holds a `HashMap<VmId, VmEntry>` under a Tokio
  `RwLock`, plus a map of hypervisor backends. Each `VmEntry`
  combines the persisted `Vm` with an optional in-process
  `Box<dyn HypervisorProcess>` handle. Methods are async
  (create/start/stop/pause/attach/detach/delete/list/get/shutdown).
- **`persistence.rs`** — `VmStore` wrapping ReDB. Single table
  `"vms"` keyed by VM id (string), value is serde-JSON-serialized
  `Vm`. Exposes `save`, `load_all`, `delete`, `update_state`.
- **`hypervisor/`** — per-backend implementations of the `Hypervisor`
  and `HypervisorProcess` traits. See [hypervisors.md](hypervisors.md).
- **`models.rs`** — serde types that cross the API boundary
  (`CreateVmRequest`, `VmResponse`, …) and the internal `Vm` /
  `VmConfig` / `VmState` types.
- **`cloud_init.rs`** — builds the default NoCloud seed image
  (`CIDATA` FAT volume) for firmware-booted VMs by shelling out to
  `mkdosfs`/`mcopy`. Called from `VmManager::start_vm`.
- **`pci.rs`** — read-only sysfs scan of `/sys/bus/pci/devices`,
  exposed via `GET /pci-devices` to help users pick VFIO targets.

## Data flow: "create and start a VM"

1. Browser `POST /api/vms` → Vite proxies to control plane `POST /vms`.
2. `api::create_vm` deserializes `CreateVmRequest`, builds a
   `VmConfig` via `From<CreateVmRequest>` (this is where `~` in
   kernel/rootfs paths is expanded — see `models.rs:expand_tilde`),
   and calls `VmManager::create_vm(name, config)`.
3. `VmManager::create_vm` allocates an id (UUID), constructs `Vm`
   with deterministic socket paths (`/tmp/<prefix>-<id>.sock` etc.),
   **persists via `VmStore::save` before** inserting into the
   in-memory map, and returns the `Vm`.
4. User clicks Start → `POST /api/vms/{id}/start` → `VmManager::start_vm`.
5. `start_vm` looks up the VM, selects the registered backend via
   `HypervisorType`, calls `backend.spawn(socket_path, console_socket_path, log_path)`
   to get a `Box<dyn HypervisorProcess>`, then `process.configure(&vm.config)`
   and `process.start()`. Each step cleans up the child on error.
6. After `start()` returns OK, the state update is persisted
   (`VmStore::update_state`) **before** flipping the in-memory state.
   The process handle is stashed on `VmEntry.process`.

See [hypervisors.md](hypervisors.md) for what `spawn`/`configure`/`start`
actually do per backend; they differ a lot (CH takes
runtime config via its HTTP API; QEMU is configured at launch time
and started with `-S`).

## Data flow: "open console in the browser"

1. Browser navigates to `/vms/:id/console` → React renders `VmConsole`.
2. The page opens a WebSocket to `ws(s)://…/api/vms/:id/console/ws`
   with `binaryType = "arraybuffer"`.
3. Vite proxy forwards the upgrade to the control plane on `:8080`.
4. `api::console_ws` looks up the VM, grabs its `console_socket_path`,
   and on upgrade hands off to `bridge_console`.
5. `bridge_console` opens a `tokio::net::UnixStream` to the console
   socket (bound by the hypervisor's proxy thread — see
   [console.md](console.md)) and enters a `select!` loop copying
   bytes both ways.
6. New clients get the captured log replayed to them at connect time
   by the hypervisor's proxy thread, so the terminal shows history
   even on first connect.

## Shutdown and reconciliation

- **Graceful shutdown**: `SIGINT`/`SIGTERM` triggers `VmManager::shutdown()`
  which calls `process.kill()` on every running VM and persists state
  to `Stopped`.
- **Unclean restart**: on next startup, `VmManager::initialize()`
  loads all persisted VMs. Anything in `Running` or `Paused` is
  reconciled: its API socket file may still exist (which means an
  orphaned hypervisor process is out there); we forcibly mark it
  `Stopped`, clean up socket files, and warn. The user's VM config
  survives; only the running process handle is lost.

## Threading model

- The control plane is Tokio-multithreaded (`#[tokio::main]`).
- Each VM, while running, has:
  - One async hypervisor process (sub-child of the control plane).
  - One **OS thread** hosting the console proxy loop
    (`Self::console_proxy_loop`), owning the PTY master fd (or the
    cloud-hypervisor PTY file handle) and the
    `UnixListener` for the console socket. This is a plain
    `std::thread`, not a Tokio task, because the code uses blocking
    `UnixListener`/`File` APIs.
- `VmManager` serializes mutating operations under a `tokio::sync::RwLock`.
  All hypervisor I/O (which can block) happens while holding the
  write lock. This is intentional: it keeps ordering trivial to
  reason about at the cost of throughput. Read paths (`list_vms`,
  `get_vm`) take the read lock and don't block on hypervisor I/O.
