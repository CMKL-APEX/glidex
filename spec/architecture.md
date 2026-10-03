# Architecture

## Processes

Glidex is a collection of cooperating OS processes. Nothing here is
containerized; everything runs as ordinary processes on a single Linux
host.

```
 ┌───────────────────────────┐         ┌──────────────────────────┐
 │  Browser (xterm.js)       │         │  gxctl (interactive CLI) │
 └──────────────┬────────────┘         └──────────────┬───────────┘
                │ HTTP + WebSocket (via glidex-ui)    │ HTTP over api.sock / TCP
                ▼                                     ▼
 ┌────────────────────────────────────────────────────────────────┐
 │  glidex-control-plane  (user glidex)                           │
 │  ├── REST API, admission   (api/, state.rs)                    │
 │  ├── Console WS bridge     (api/vms.rs::bridge_console)        │
 │  ├── Store / ReDB          (store.rs; images/, network.rs, …)  │
 │  ├── VM controller + queue (controller/)                       │
 │  ├── Instances, runners    (instance/)                         │
 │  └── Hypervisor drivers    (hypervisor/)                       │
 └──────┬──────────────────┬──────────────────┬───────────────────┘
        │ D-Bus (zbus,     │ api.sock         │ shim.sock
        │ polkit)          │ (CH HTTP / QMP)  │ (status, stop, kill, release)
        ▼                  │                  │
 ┌─────────────────────────┴──────────────────┴────────────────────┐
 │ glidex-vm@<id>.service  (user glidex, own cgroup)               │
 │   glidex-vm-shim ── PTY master ── console.sock + console.log    │
 │        └─ cloud-hypervisor / qemu-system-*  (child)             │
 └─────────────────────────────────────────────────────────────────┘
```

The control plane never spawns a hypervisor and never holds a PTY. It
reaches a running VM only through files and sockets in the VM's runtime
directory `<run dir>/vms/<id>/` (`launch.json`, `instance.json`,
`shim.sock`, `api.sock`, `console.sock`, `console.log`;
[reconciliation.md §5.1](reconciliation.md#51-processes)). With the
detached runner (dev runs, tests) the shim runs in its own session
instead of a unit; the picture is otherwise the same. VM ports, bridges
and NAT go through `glidex-netd` (`netd.sock`,
[networking.md](networking.md)).

Separately, the UI crate (`glidex-ui`) serves the built UI
(`ui/dist`, installed to `/usr/local/share/glidex/ui`) on `:5173` and
proxies `/api/**` (including WebSocket upgrades) to the control plane
on `:8841`; `glidex-ui --dev` runs the Vite dev server instead, whose
proxy does the same. The installer runs it as `glidex-ui.service`.

## Control-plane layers

Reading top-to-bottom inside `crates/glidex-control-plane/src/`:

- **`main.rs`** — process entrypoint. Checks `/dev/kvm`, opens the
  ReDB database at `~/.glidex/glidex.db`, applies the `reconcile` /
  `console` config (`VmManager::configure`), calls
  `VmManager::initialize()` (migrate, load, adopt running instances,
  [reconciliation.md §9.4](reconciliation.md#94-startup-order)), starts
  the controllers, then binds the listeners. On SIGTERM it stops
  accepting requests and exits; VMs keep running.
- **`api/`** — axum `Router` (`api/mod.rs` route table, one Cedar action
  per route). Handlers do authorization and admission, write the spec,
  and answer `202` or, with `?wait`, once the controller has acted
  (`api/vms.rs::respond`). Also the console WebSocket bridge.
- **`state.rs`** — `VmManager`: **admission** over the store. Validates
  requests, checks quotas, disk/VFIO claims and name uniqueness under
  the write lock of an in-memory cache of every VM, and writes `spec`
  (bumping `generation`) together with an event. Holds no process
  handles; host actions on VMs (hypervisors, VM ports) happen in the
  controller, never on the request path.
- **`store.rs`** — `VmStore`: the `vms` table as `{meta, spec, status}`
  envelopes, the `events` rings, the schema version and its migration
  ([data-model.md](data-model.md#persistence-schema)).
- **`controller/`** — the work queue (`queue.rs`), the VM controller
  (`vm.rs`, one reconcile per VM at a time) and startup adoption
  (`startup.rs`). The only writer of VM `status`.
- **`instance/`** — liveness checks ("is an instance of this VM live?",
  `mod.rs`) and the runners that start and escalate-stop shims
  (`runner.rs`: systemd over D-Bus, or detached).
- **`hypervisor/`** — stateless `HypervisorDriver`s: VM config → command
  line, plus runtime operations over the hypervisor's socket. See
  [hypervisors.md](hypervisors.md).
- **`models.rs`** — serde types that cross the API boundary
  (`CreateVmRequest`, `VmResponse`, …) and the internal `Vm`,
  `VmSpec`/`VmStatus`, `VmConfig`, `PowerState`, `VmPhase` types.
- **`credentials.rs`** — `CredentialStore`: guest logins (username,
  SHA-512-crypt hash, SSH keys) in the `credentials` table of the same
  ReDB file. See [credentials.md](credentials.md).
- **`cloud_init.rs`** — builds the default NoCloud seed image
  (`CIDATA` FAT volume) for firmware-booted VMs by shelling out to
  `mkdosfs`/`mcopy`. Called by the VM controller before each launch.
- **`pci.rs`** — read-only sysfs scan of `/sys/bus/pci/devices`,
  exposed via `GET /pci-devices` to help users pick VFIO targets.
- **`images/`** — `ImageManager`: the image catalog, verified downloads,
  and managed disks (create, resize, extend root partition), shelling out
  to `qemu-img`/`qemu-io`/`sgdisk`/`growpart`. Records live in the
  `images` and `disks` tables, files in `~/.glidex/{images,disks}`.
  `VmManager` owns it and does the VM-related checks (claims, live
  instances). See [images.md](images.md).

Outside the control plane: `crates/glidex-vm-shim` (the per-VM
supervisor, [reconciliation.md §8.3](reconciliation.md#83-shim-process))
and `crates/glidex-hv-client` (CH HTTP and QMP clients, used by both).

## Data flow: "create and start a VM"

1. Browser `POST /api/vms` → glidex-ui (or the Vite proxy) forwards to
   the control plane's `POST /vms`.
2. `api::vms::create` authorizes the compound request, builds a
   `VmConfig` via `From<CreateVmRequest>` (`~` is expanded here, see
   `models.rs:expand_tilde`) and calls `VmManager::create_vm_with`.
3. Admission: validation, quotas, claims; then **one** store commit
   writes the VM (`spec.power = stopped` unless the request says
   `running`), its disks' claims and a `Created` event, before the
   response. The VM is queued for the controller.
4. User clicks Start → `POST /api/vms/{id}/start?wait=60`. `set_power`
   writes `spec.power = running` (`generation + 1`) and queues the VM.
5. The VM controller's reconcile sees desired `running` and no live
   instance: regenerates the seed, attaches NIC ports through netd
   (each recorded in `status.nics` first), writes `launch.json`, records
   the intent (`status.instance`), and starts the shim through the
   runner ([reconciliation.md §9.1](reconciliation.md#91-reconcile)
   step 5).
6. The shim spawns the hypervisor with its whole config on the command
   line, waits for its API socket, and writes `instance.json` with
   `phase: running`. The next round observes the guest running and sets
   `phase: Running`, `Ready=True`.
7. The `?wait` handler, watching the cache, answers `200` with the VM
   once `observed_generation ≥` the written generation and `Ready=True`;
   a failed launch answers with the error the synchronous call used to
   return; a timeout answers `202`.

## Data flow: "open console in the browser"

1. Browser navigates to `/vms/:id/console` → React renders `VmConsole`.
2. The page opens a WebSocket to `ws(s)://…/api/vms/:id/console/ws`
   with `binaryType = "arraybuffer"`.
3. Vite proxy forwards the upgrade to the control plane on `:8841`.
4. `api::console_ws` looks up the VM, grabs its `console_socket_path`,
   and on upgrade hands off to `bridge_console`.
5. `bridge_console` opens a `tokio::net::UnixStream` to the console
   socket (bound by the shim's proxy thread — see
   [console.md](console.md)) and enters a `select!` loop copying
   bytes both ways.
6. New clients get the current console log replayed to them at connect
   time by the shim's proxy thread, so the terminal shows history even
   on first connect.

## Shutdown, restart and adoption

- **Control-plane stop, crash or upgrade** leaves every VM running: the
  hypervisor is the shim's child, in the `glidex-vm@<id>` unit's cgroup
  (or its own session with the detached runner), and the shim owns the
  PTY. `main` just stops the listeners. There is no
  `VmManager::shutdown`.
- **Startup** observes every VM before acting on any
  ([reconciliation.md §9.4](reconciliation.md#94-startup-order)):
  instances that are still live are adopted as they are (event
  `Adopted`, or `AdoptedUnrecorded` when only `instance.json` knew of
  them); units and runtime directories with no VM record are listed as
  orphans and never touched; only then is netd told which VMs own ports.
  Nothing is marked stopped because the control plane restarted.
- **Host shutdown** stops the `glidex-vm@` units: each shim presses the
  power button, waits `host_shutdown_grace_secs`, then stops the
  hypervisor, with the control plane possibly already gone.

**Invariant.** At most one hypervisor instance per VM. Before a launch
the controller must positively establish that no instance is live
(unit state, recorded shim and hypervisor pids with start times,
sockets); if it can't tell, the VM is `unknown` and nothing is launched
([reconciliation.md §8.7](reconciliation.md#87-liveness-establishing-no-instance-d8)).

## Threading model

- The control plane is Tokio-multithreaded (`#[tokio::main]`).
- Admission takes the `VmManager` cache's `RwLock` (write) only around
  checks and a store commit; reads take the read lock. No handler holds
  it across hypervisor or netd I/O.
- Host actions run in `reconcile.workers` (4) controller workers fed by
  one deduplicating work queue: one reconcile per VM at a time,
  different VMs in parallel
  ([reconciliation.md §9.3](reconciliation.md#93-work-queue)). Blocking
  calls (hypervisor sockets, netd, `/proc`) go through
  `spawn_blocking`. Exit watches are one task per live shim/hypervisor
  pid (`pidfd`); a resync re-queues every VM every `resync_secs`.
- Each shim is single-process: a main loop serving `shim.sock` and
  reaping the hypervisor, and one OS thread for the console proxy
  ([console.md](console.md)).
