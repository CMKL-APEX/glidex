# Reconciliation: desired state, control loops and detached VMs

> Status: **implemented through M3** (2026-10-03); §18 notes where the
> build differs from this design. Host facts the design depends on were
> verified on the pinned versions (§4). It changes the
> contracts in [architecture.md](architecture.md),
> [data-model.md](data-model.md), [hypervisors.md](hypervisors.md),
> [console.md](console.md), [rest-api.md](rest-api.md),
> [images.md](images.md), [networking.md](networking.md) §7.7 and §11,
> [installer.md](installer.md), [security.md](security.md) §7 and §9,
> [cli.md](cli.md) and [web-ui.md](web-ui.md). §17 lists the edit each
> needs; they are made in the same PR as the milestone that changes the
> behaviour (§18), not ahead of it.

## 1. Motivation

Today the control plane is **imperative**: an API call (`start`, `stop`,
`attach_device`, …) runs the host action inline, under the `VmManager`
write lock, and records the result. Two consequences:

1. **VMs die with the control plane.** Hypervisors are children of
   `glidex-control-plane`, inside its cgroup, on a PTY whose master the
   control plane holds. A clean stop kills them on purpose
   (`VmManager::shutdown`); a crash or `systemctl restart` kills them as
   a side effect (cgroup teardown, PTY hangup). `initialize()` then marks
   every `Running`/`Paused` VM `Stopped`. Upgrading glidex reboots every
   guest.
2. **State only converges at the moment of a call.** If a step fails
   halfway, or the host changes underneath (a hypervisor crashes, netd
   loses a port, an image file disappears, the host reboots), nothing
   notices until a user acts. `reap_exited_vms` is the one ad-hoc loop.

This design makes the control plane a set of **level-triggered control
loops** in the style of Kubernetes controllers: the API records *what
the user wants* (spec); controllers repeatedly *observe what is*
(status) and take the smallest step that moves the host toward the spec.
Each hypervisor moves out of the control plane's process tree into a
per-VM supervisor, so the control plane can crash, restart or be
upgraded while guests keep running.

## 2. Goals and non-goals

Goals:

- **G1. Spec/status split** for every resource: VM, Disk, Image,
  Network (and the VM ports netd holds on the VMs' behalf).
- **G2. Level-triggered reconciliation.** Correctness never depends on
  having seen an event; a periodic resync of every object from scratch
  reaches the same result.
- **G3. VMs outlive the control plane.** Stopping, crashing, restarting
  or upgrading `glidex-control-plane` does not stop, pause or disturb any
  guest, its console capture, or its network ports.
- **G4. Adoption.** A starting control plane finds running instances,
  verifies them, and resumes managing them without restarting them.
- **G5. Crash-consistent steps.** The control plane may die between any
  two steps of any operation; the next reconcile finishes or undoes it.
  No step is ever repeated in a way that harms data (e.g. two hypervisors
  on one disk).

Non-goals:

- Multiple control planes per host, or leader election. One host, one
  control plane (ReDB's exclusive open already enforces this).
- Live migration, multi-host scheduling (spec/README.md non-goals).
- Making netd a client of this loop. netd stays a separate root daemon
  that reconciles host networking (networking.md §7.7); the control
  plane reconciles *netd's records*.
- Live attach of disks and NICs to a running guest (future work; today's
  "stopped VM only" rule becomes "next launch", §7.3).
- Downgrade. Once a database has been migrated (§6.6), older glidex
  versions refuse to open it.

## 3. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | Each running VM is supervised by its own **`glidex-vm-shim`**, which is the hypervisor's parent, owns the PTY master, the console socket and log, and records the hypervisor's exit. | Whatever holds the PTY master and reaps the hypervisor must not be the control plane, or the guest dies (or loses console output) with it. Same split as containerd-shim / conmon. |
| D2 | Under systemd the shim runs as an instance of a template unit **`glidex-vm@<vm-id>.service`**, started by the control plane over systemd's D-Bus API (`zbus`) and authorized by a **polkit** rule. Without systemd (dev runs, tests) the control plane double-forks the shim into its own session. | Only a separate unit escapes the control plane's cgroup. `KillMode=process` on the control plane would leave VMs in a cgroup systemd believes is stopped, without accounting. A unit per VM gives per-VM accounting, journal and shutdown ordering. Polkit, not netd, so netd stays networking-only (security.md §8). D-Bus, not `systemctl`, for typed errors and unit signals. |
| D3 | Every persisted resource is `{meta, spec, status}`. The API writes `spec` (and `meta`); only controllers write `status`. | "The user wants it running" survives "it isn't running right now". |
| D4 | Controllers are level-triggered: a per-object work queue fed by API writes, host events and a periodic full resync. One reconcile per object at a time; different objects in parallel. | Replaces the global write lock held across hypervisor I/O; missed events are harmless. |
| D5 | **Admission stays synchronous; actuation becomes asynchronous.** Validation, authorization, quotas and exclusivity claims are checked when the spec is written and still fail the request. Host actions happen in controllers; their failures appear in `status`. | Data-protecting invariants (exclusive disk use, quotas) must hold at commit time, not eventually. Host actions can be retried. |
| D6 | systemd does **not** restart VMs (`Restart=no`). Restart policy belongs to the VM controller alone. | Two supervisors with restart policies fight, and only the controller knows why a VM exited. |
| D7 | Everything needed to talk to an instance is persisted (`status.instance`) or in its runtime directory (`launch.json`, `instance.json`), never only in process memory. | Adoption after a restart is the normal code path. |
| D8 | At most one hypervisor instance per VM, ever. Before launching, the controller must *positively establish* that no instance is live (§8.7). When it cannot, it reports `Unknown` and does not launch. A hypervisor never outlives its shim. | Two guests on one qcow2 corrupt it; "can't tell" must be safe. |
| D9 | **Launch = configured.** Cloud Hypervisor gets its whole config on its command line (§8.2), as QEMU does; QEMU is no longer started with `-S`. Hypervisor API sockets are used only for runtime operations. | No half-configured instance, so no crash window between "launched" and "booted". |
| D10 | A VM whose spec says `Running`/`Paused` is launched again after a host reboot (`on_host_boot: Resume`, default; `Stop` per VM or site-wide). | That is what desired state means. |
| D11 | Disk and VFIO **claims last until the instance that uses them is gone**, not until the spec drops them. | The running hypervisor still has the file open (D8, one level down). |
| D12 | The only controller-written spec field is `vm.spec.power`, set to `Stopped` after an exit nobody in glidex asked for that means "off"; the write is conditional on the generation. | A newer user `start` always wins over a stale "it powered off". |
| D13 | **Ready = converged to the spec**, including for a VM that should be stopped. | One rule for clients to wait on. |
| D14 | Console logs are **appended** across instances with separator lines and **rotated** once (`console.log.1`) at 16 MiB. | The output before a crash survives the crash-restart. |
| D15 | The shim owns the whole stop (resume if paused, power button, grace, kill). Host shutdown uses its own grace (default 60 s); the VM's `stop_grace_secs` (default 0, today's hard stop) applies to stops glidex asks for. | Host shutdown must work with the control plane down, and must not hard-kill guests by default. |
| D16 | NIC ports survive crash-restarts; netd's `sync_vms` gets the VMs that **own ports**, not the ones running. | A VM keeps its tap and NAT address across restarts, and an adopting or restarting VM is never cut off. |
| D17 | Instances and units with no matching record are adopted (if a VM record exists) or reported as orphans (if not), never stopped. | Same rule as orphan image/disk files (images.md §2). |
| D18 | Spec edits are accepted in any VM state; what varies is when they take effect (§7.3). Hypervisor and boot source stay immutable. | Declarative edits with explicit `RestartRequired`, no surprise reboots. |
| D19 | One-shot actions (extend-root, image retry) are spec counters (`seq`) the controller compares with the last applied value in status. | Actions stay level-triggered and idempotent. |
| D20 | `?wait` returns the same error envelopes as today's synchronous calls when the reconcile fails. | gxctl and the UI keep their error behaviour. |

## 4. Verified host facts

Checked on this repository's pinned versions (Cloud Hypervisor v53.0,
QEMU from the distro, systemd 259) on 2026-10-03. Each one is turned
into a regression test (§19).

| # | Fact | How it was checked | Used by |
|---|---|---|---|
| F1 | CH v53.0 accepts on its command line every option the backend sends to `vm.create` today: `--cpus boot=,max=`, `--memory size=,shared=,hugepages=`, `--firmware`, `--kernel`, `--cmdline`, `--disk path=,readonly=,image_type=,backing_files=`, `--net tap=,mac=,num_queues=,id=,mtu=,vhost_user=,socket=,vhost_mode=server`, `--device path=,id=`, `--serial`, `--console`. `image_type=` takes `raw`, `qcow2`, `vhdx`, `vhd` (not the API's `FixedVhd`). `mtu=` is accepted although `--help` does not list it; an unknown key fails with `ParseNetwork(UnknownOption)` before boot. | ran `cloud-hypervisor` with each option against missing files; parsing passed and it failed only at open time | §8.2 |
| F2 | CH option values containing `,` must be wrapped in double quotes (`path="/a,b/x"`); unquoted, the part after the comma is parsed as an option. `=` inside a value needs no quoting. | same | §8.2, §8.3 |
| F3 | A guest's ACPI power-off makes both CH v53.0 and QEMU exit with status **0** (Ubuntu cloud image, cloud-init `power_state: poweroff`, firmware boot). CH's event monitor logs `vm shutdown`, `vm deleted`, `vmm shutdown`. | booted the image under each hypervisor and recorded the exit status | §7.5 |
| F4 | CH also exits **0** on SIGTERM (QEMU does too: "terminating on signal 15"). Exit status 0 therefore means "a clean stop", not specifically "the guest powered off". | sent SIGTERM to an idle CH | §7.5 |
| F5 | netd's `attach_vm_port` succeeds on a port whose tap already exists: `vm_port::attach` skips `ip tuntap add` when the link exists and adds the OVS port with `--may-exist` (`crates/glidex-ovs/src/vm_port.rs:123`). | code | §10.3 |
| F6 | systemd checks `StartUnit`/`StopUnit`/`KillUnit` from an unprivileged caller against polkit action `org.freedesktop.systemd1.manage-units` with details `unit` and `verb` (`start`, `stop`, `kill`). | systemd source (`src/core/dbus-unit.c`, `dbus-manager.c`); `systemd_runner_tests.rs` in system mode called each method as an unprivileged user allowed only by `50-glidex-vm.rules` (systemd 259) | §13 |
| F7 | `cloud-hypervisor` carries the file capability `cap_net_admin`, so the kernel clears `PR_SET_PDEATHSIG` when the shim execs it: the death signal alone does not stop it outliving its shim. | killed a shim with `-9`; its CH kept running (`shim_tests.rs`) | §8.3 |
| F8 | A Cloud Hypervisor qcow2 disk may not reopen after the VMM is SIGKILLed while running ("not cleanly closed … Invalid cluster index"); `PUT /vmm.shutdown` closes it cleanly. | `restart_e2e` before the fix | §8.5 |

## 5. Components

### 5.1 Processes

```
                 ┌──────────────────────────────────────────┐
  REST / WS ───▶ │ glidex-control-plane   (user glidex)     │
                 │  API (admission) ──▶ store (ReDB)        │
                 │  controllers: vm, disk, image, network   │
                 │  console WS bridge                       │
                 └───┬───────────────┬─────────────┬────────┘
  D-Bus (zbus,       │ hypervisor    │ netd.sock    │ shim.sock
       polkit)       │ API socket    ▼              │ (status, stop, release)
                     │ (CH / QMP) ┌───────────┐     │
                     ▼            │glidex-netd│     │
 ┌─────────────────────────────────────────────────┴───────┐
 │ glidex-vm@<id>.service   (user glidex, own cgroup)      │
 │   glidex-vm-shim ── PTY master ── console.sock + .log   │
 │        └─ cloud-hypervisor / qemu-system-*  (child)     │
 └─────────────────────────────────────────────────────────┘
```

The control plane never spawns a hypervisor and never holds a PTY. It
reaches a running instance only through files and sockets in the VM's
runtime directory `<run dir>/vms/<id>/` (`0700`, as today):

| File | Written by | Purpose |
|---|---|---|
| `launch.json` | control plane, before launch | everything the shim needs (§8.1) |
| `instance.json` | shim | live state and exit record (§8.4) |
| `shim.sock` | shim | control protocol (§8.5) |
| `api.sock` | hypervisor | CH HTTP API / QMP, as today |
| `console.sock`, `console.log`, `console.log.1` | shim | console (§8.6) |
| `cloudinit.img` | control plane | seed, as today |

### 5.2 Code layout

| Crate / module | Content |
|---|---|
| `crates/glidex-hv-client` (sync, no Tokio) | `ch.rs` (CH HTTP client), `qmp.rs` (QMP client), moved out of `hypervisor/cloud_hypervisor.rs` and `hypervisor/qemu.rs`; shared by the shim and the control plane |
| `crates/glidex-vm-shim` (bin + lib, sync, no Tokio/ReDB/axum) | `main.rs` (CLI, §8.3), `supervisor.rs` (lifecycle, signals, `shim.sock` server), `launch.rs` (`launch.json`, allowlist), `proxy.rs` (moved from `hypervisor/console.rs`, plus rotation), `proto.rs` (§8.5), `client.rs` (the control plane's `shim.sock` client), `state.rs` (`instance.json`), `util.rs` (boot id, process identity, socket probes), `sd.rs` (`sd_notify`) |
| `glidex-control-plane/src/store.rs` (replaces `persistence.rs`) | the VM envelope, `Commit` (one write transaction across VMs, disks and events), schema version, migration, event rings (§6) |
| `glidex-control-plane/src/controller/` | `mod.rs` (workers, resync, exit watch, netd sync), `queue.rs` (work queue, backoff), `vm.rs`, `disk.rs`, `image.rs`, `network.rs`, `startup.rs` (§9.4) |
| `glidex-control-plane/src/instance/` | `mod.rs` (§8.7 liveness checks), `runner.rs` (systemd and detached runners, §8.8) |
| `glidex-control-plane/src/hypervisor/` | drivers (§16) producing the launch argv; CH argv builder replaces the `vm.create` payload builder (`controller/vm.rs` writes `launch.json` from it) |
| `glidex-control-plane/src/state.rs` | `VmManager`: admission logic (validation, authz-adjacent checks, quotas, claims) over the store; no process handles |

New dependencies: `zbus` (Tokio integration) in the control plane;
`nix` (already used) in the shim.

## 6. Resource model and store

### 6.1 Envelope

```rust
pub struct Object<S, T> {
    pub meta: Meta,
    pub spec: S,
    pub status: T,
}

pub struct Meta {
    pub id: String,                        // UUIDv4 (networks: name, as today)
    pub name: String,
    pub project: String,
    pub created_at: u64,                   // unix seconds, as today
    pub generation: u64,                   // starts at 1; +1 on every spec change
    pub resource_version: u64,             // +1 on every write (spec or status)
    pub deletion_requested_at: Option<u64>,
    pub finalizers: Vec<Finalizer>,        // cleanup still owed before the record goes
}

pub struct StatusCommon {
    pub observed_generation: u64,          // generation the last finished reconcile read
    pub conditions: Vec<Condition>,
    pub last_reconciled_at: u64,
}

pub struct Condition {
    pub kind: ConditionKind,               // closed enum per resource, §7.5 / §11
    pub status: Tristate,                  // True | False | Unknown
    pub reason: String,                    // CamelCase, from the catalogue
    pub message: String,
    pub last_transition_at: u64,           // changes only when `status` changes
}
```

**Invariant.** `generation` changes only when `spec` changes.
`observed_generation == generation` and `Ready=True` means converged
(D13). `?wait` and clients wait on exactly that (§12.3).

### 6.2 Who writes what

- API handlers write `meta` and `spec` in one ReDB write transaction
  together with every admission check that needs atomicity (claims,
  quotas, name uniqueness), and bump `generation` iff `spec` changed.
- Controllers write `status` only: re-read the record inside the write
  transaction, replace `status`, set `observed_generation` to the
  generation the reconcile *read* (never the current one). A spec change
  made meanwhile is therefore never undone and is picked up next round.
- **Exception (D12):** the VM controller sets `spec.power = Stopped` in
  the cases marked in §7.5, in a transaction that aborts if
  `generation` differs from the one the exit was observed under. Each
  such write produces an event (§6.4) and an audit line with principal
  `system:vm-controller` and action `stopVm` (security.md §10).
- Controllers never write another resource's status: cross-resource
  effects go through a field in the writer's own status that the other
  controller reads (e.g. `seed_growpart_seq`, §10.1).

### 6.3 Deletion and finalizers

`DELETE` sets `deletion_requested_at` and the resource's finalizers, and
returns `202` (`200` with `?wait` once the record is gone; `204` without
`wait` when there is nothing to finalize, e.g. a never-started VM with no
owned disk). The controller completes each finalizer and removes it in
its own write; the record is deleted when the list is empty. A deleting
object rejects spec writes with `409 conflict`.

| Resource | Finalizers |
|---|---|
| VM | `vm.instance` (no live instance), `vm.ports` (netd `release_vm`), `vm.disks` (clear claims), `vm.owned-disk` (delete the owned root disk; left out with `?keep_disk=true`), `vm.runtime` (remove the runtime directory and firmware vars) |
| Disk | `disk.file` (remove the file; a failure is logged and the file left as an orphan, images.md §6.5) |
| Image | `image.download` (abort a running download), `image.file` (remove the file and `.part`; one that can't be removed is logged and left as an orphan) |
| Network | `network.netd` (`delete_nat`, then `delete_bridge` if netd still has it, for networks glidex created; a bridged network only drops its record) |

Admission keeps today's refusals (`409` deleting an attached disk, an
image with linked disks, a network in use): finalizers are for cleanup
the deleter owns, never for waiting on other users.

As built, only VMs carry a `finalizers` list. Disks, images and
networks record the request as `deletion_requested_at`, and their
controller runs the finalizers above in order and then removes the
record:

- **Disk**: the record, then the file (`204`, or `202` while an
  operation on it finishes).
- **Image**: the download is aborted, the record removed, then the files.
  Admission also refuses (`409`) while a disk waits for the image
  (pending or creating from it) or a clone from it is in progress.
- **Network**: netd's NAT and bridge, then the record. It waits, with
  the reason in `Ready`, while netd is unreachable
  (`Unknown/NetdUnavailable`, phase `NetdUnavailable`, retried every
  30 s), while netd refuses (`False/DeleteFailed`, event, every 30 s), or
  while a VM still uses the network (`False/InUse`, every 10 s; admission
  refuses that case, so it only follows a race).

The API runs the controller's round inline, so a delete normally answers
`204`; `202` with the object while it still waits; `?wait` as for VMs.
A deleting image or network takes no new users: it can't be a new
disk's source, be retried, or be attached to a VM (`400 … is being
deleted`), and a create or pull with its name is refused (`409 … is
being deleted`) until it is gone. Writers holding an older copy (the
download task holds one for minutes) can't undo a deletion: an update
keeps `deletion_requested_at`, and an update to a record that is gone is
dropped instead of re-creating it.

### 6.4 Tables

Same ReDB file. Values are serde-JSON.

| Table | Key | Value |
|---|---|---|
| `meta` (new) | `schema_version` | `2` (absent = 1, today's layout) |
| `vms` | id | `{meta, spec, status}` envelope (`store::VmRecord`) |
| `disks`, `images`, `networks` | id (network: name) | today's flat records, with the controller's fields added as serde-defaulted fields: disk `phase`, `create`, `resize`, `extend_root`, `applied_extend_root_seq`, `owner`, `deletion_requested_at`, `conditions`; image `retry_seq`, `applied_retry_seq`, `deletion_requested_at`; network `phase`, `conditions`, `deletion_requested_at`. Records written before them load as `Ready` |
| `events` (new) | `<kind>/<id>` | ring of the last 50 `Event { at, actor, kind: Normal\|Warning, reason, message }` |

Only the VM is nested. Disks, images and networks keep their flat shape
because their spec/status split is a split of fields, not of writers
that need a generation: requests are one-shot or replace-only (`resize`,
`extend_root.seq`, `retry_seq`), and the controller compares them with
what it applied.

Event actors: `api:<principal id>`, `controller`, `guest`, `systemd`,
`host`. Events are written in the same transaction as the status or spec
write they describe; the ring is deleted with the object.

### 6.5 Caching and concurrency

The store keeps an in-memory copy of every object for reads, updated
after each committed write (the store is the only writer). Mutual
exclusion is per object, by the work queue (§9.3). No API handler does
hypervisor or netd I/O, except the console bridge (a socket copy), the
`/ovs/*` passthroughs, and `?wait` (which only reads the cache).

### 6.6 Migration (schema 1 → 2)

At startup, if `schema_version` is absent, one write transaction:

1. Rewrites each VM: `config` → `spec.config`; old `state` →
   `spec.power = Stopped` (the old control plane killed every guest when
   it stopped, and its startup marked them stopped); `spec` defaults
   from §7.1; `status.phase = Stopped`, `never_started = (state ==
   Created)`; `generation = 1`.
2. Leaves disks, images and networks as they are (flat records whose
   new fields default, §6.4); their status is computed by the first
   reconcile.
3. Sets `schema_version = 2`.

A schema-1 binary must not run on a schema-2 database. M1 therefore
already reads `meta.schema_version` and refuses to start when it is
greater than 1 ("database is newer than this glidex"), so rolling back
from M2 to M1 fails cleanly; releases before M1 cannot be protected and
the M2 release notes say so.

If, after migration, a VM's `api.sock` still accepts a connection (an
orphan of a crashed old control plane), §8.7 finds it live with no
`instance.json`: the VM gets `phase: Unknown`, reason `LegacyOrphan`,
and is not launched until an operator stops that process.

The upgrade *to* M1 still stops running VMs once: the old control
plane's shutdown kills them when the installer restarts the service.
`glidex-install` prints this before restarting. Every later upgrade
leaves VMs running.

## 7. VM resource

### 7.1 Spec

```rust
pub struct VmSpec {
    pub config: VmConfig,                 // today's persisted VmConfig fields
    pub power: PowerState,                // Running | Paused | Stopped; new VMs: Stopped
    pub restart_policy: RestartPolicy,    // OnFailure (default) | Never
    pub on_host_boot: HostBootPolicy,     // Resume | Stop; default: reconcile.on_host_boot
    pub stop_grace_secs: u32,             // 0..=300, default 0
}
```

`CreateVmRequest` gains optional `power` (default `Stopped`, so
`create` then `start` keeps working; `"running"` creates and starts in
one call), `restart_policy`, `on_host_boot`, `stop_grace_secs`.

`stop_grace_secs` is spec, not a per-call parameter:
`POST /vms/{id}/stop?graceful_timeout_secs=N` writes `power = Stopped`
and `stop_grace_secs = N` in one write, so a control-plane restart in
the middle of a stop still knows the grace.

### 7.2 Status

```rust
pub struct VmStatus {
    pub common: StatusCommon,
    pub phase: VmPhase,
    pub phase_since: Option<u64>,          // when `phase` last changed (unix s; metering.md §5.6)
    pub instance: Option<InstanceRef>,     // from the intent record until the instance is gone
    pub last_exit: Option<ExitRecord>,
    pub restart_count: u32,                // consecutive crash restarts
    pub next_restart_at: Option<u64>,      // crash-loop backoff
    pub stop_deadline: Option<u64>,        // escalation deadline of a stop in progress
    pub nics: Vec<NicStatus>,              // ports this VM owns in netd (§9.1)
    pub seed_growpart_seq: Option<u64>,    // §10.1
    pub never_started: bool,
}

pub enum VmPhase { Stopped, Provisioning, Starting, Running, Paused, Stopping, Failed, Unknown }

pub struct InstanceRef {
    pub instance_id: String,               // UUIDv4 per launch
    pub runner: Runner,                    // Systemd { unit } | Detached
    pub boot_id: String,                   // /proc/sys/kernel/random/boot_id at launch
    pub launched_generation: u64,          // spec generation launch.json was built from
    pub disks: Vec<String>,                // disk ids it has open (claims, D11)
    pub vfio_devices: Vec<String>,         // host devices it holds; updated on hot-plug
    pub shim_pid: Option<u32>,             // filled from instance.json
    pub shim_starttime: Option<u64>,       // /proc/<pid>/stat field 22
    pub hypervisor_pid: Option<u32>,
    pub hypervisor_starttime: Option<u64>,
    pub launched_at: u64,
}

pub struct NicStatus { pub network: String, pub nic_index: u8, pub mac: String,
                       pub ipv4: Option<String>, pub port_ok: bool }

pub struct ExitRecord { pub at: u64, pub instance_id: String, pub cause: ExitCause,
                        pub code: Option<i32>, pub signal: Option<i32>, pub message: Option<String> }

pub enum ExitCause { Requested, Terminated, CleanExit, Crashed, LaunchFailed, HostReboot, Lost }
```

Phases:

| Phase | Meaning |
|---|---|
| `Stopped` | no live instance |
| `Provisioning` | seed, firmware vars, ports, `launch.json` being prepared |
| `Starting` | runner started; shim has not reported `running` yet |
| `Running` / `Paused` | as observed from the hypervisor |
| `Stopping` | a stop was sent; instance still live |
| `Failed` | the last attempt to reach the spec failed; retried with backoff |
| `Unknown` | §8.7 checks could not be made; nothing is launched or stopped |

### 7.3 Mutability (D18)

| Field | Takes effect |
|---|---|
| `power`, `stop_grace_secs`, `restart_policy`, `on_host_boot` | at once |
| `config.vfio_devices` | hot-plugged/unplugged while the guest is observed `Running`; while `Paused`, pending (`DevicesPending`) |
| `config.data_disks`, `networks`, `vcpu_count`, `mem_size_mib`, `credential`, `kernel_args`, `hugepages` | next launch; `RestartRequired=True` meanwhile |
| `hypervisor`, `kernel_image_path`, `firmware_path`, `firmware`, `rootfs_path`, `cloud_init_path`, `image`, `root_disk` | never: `400 invalid_config` ("immutable field") |

`RestartRequired` compares the spec at `generation` with the spec at
`instance.launched_generation`, ignoring the "at once" and hot-plug
fields. The controller keeps the launched spec in `launch.json`'s
`spec` field (§8.1), so the comparison survives restarts.

**Invariant (claims, D11).** A disk is claimed by a VM while that VM's
spec references it *or* `status.instance.disks` contains it. Claims are
checked at admission: another VM referencing a claimed disk gets `409
conflict`; resize and delete of a claimed disk keep today's rules. The
same holds for VFIO devices (`attach_device` / create on another VM).

### 7.4 Defaults and API projection

`VmResponse` keeps every current field; `state` is derived:

| `phase` | `state` |
|---|---|
| `Stopped` + `never_started` | `created` |
| `Stopped` | `stopped` |
| `Provisioning`, `Starting` | `starting` |
| `Running`, `Paused`, `Stopping`, `Failed`, `Unknown` | lower-case name |

New fields: `desired_state` (`spec.power`), `generation`,
`observed_generation`, `conditions`, `restart_required`, `last_exit`,
`restart_policy`, `on_host_boot`, `stop_grace_secs`. Example:

```json
{ "id": "…", "name": "web-1", "project": "default",
  "state": "running", "desired_state": "running",
  "generation": 4, "observed_generation": 4, "restart_required": false,
  "conditions": [ { "kind": "Ready", "status": "True", "reason": "Converged", "message": "", "last_transition_at": 1791000000 } ],
  "last_exit": { "at": 1790990000, "cause": "crashed", "signal": 9 },
  "restart_policy": "on_failure", "on_host_boot": "resume", "stop_grace_secs": 0,
  "vcpu_count": 2, "mem_size_mib": 2048, "hypervisor": "cloudhypervisor", "…": "…" }
```

### 7.5 Exit handling

The shim records how the hypervisor ended (§8.4); the controller acts:

| Cause | Set by | Controller action |
|---|---|---|
| `Requested` | shim: a `stop`/`kill` over `shim.sock` preceded the exit | none beyond the normal reconcile |
| `Terminated` | shim: SIGTERM to the shim (unit stop: host shutdown, or an operator's `systemctl stop`) | `spec.power = Stopped` (D12), actor `systemd`; a host reboot then shows up as `HostReboot` instead because `/run` is gone |
| `CleanExit` | shim: exit status 0, nothing requested | `spec.power = Stopped`, actor `guest`. By F3/F4 this is a guest power-off or a SIGTERM sent straight to the hypervisor; both mean "off" |
| `Crashed` | shim: non-zero status or a signal | `OnFailure`: relaunch at `next_restart_at` = now + 10 s × 2^(`restart_count`), capped at 300 s; `restart_count` resets after 600 s of `Running`; `CrashLoopBackOff=True` while waiting. `Never`: `spec.power = Stopped`, `Ready=False/Crashed` |
| `LaunchFailed` | shim: the hypervisor exited before its API socket answered | `phase: Failed`, reason `LaunchFailed`, message = captured output (today's launch error); retried with the reconcile backoff (§9.3), not counted as a crash |
| `HostReboot` | controller: `instance.boot_id` ≠ current boot id | `Resume`: launch into `spec.power`. `Stop`: `spec.power = Stopped`, actor `host` |
| `Lost` | controller: an intent record whose launch never happened, or a shim that died without writing an exit | `Lost` from an intent: none. From a dead shim: treated as `Crashed` |

A guest *reboot* resets in place in both backends and never exits the
hypervisor; it is invisible here, as today.

Conditions and reasons (closed catalogue; UI and gxctl map reasons to
text):

| Condition | Reasons |
|---|---|
| `Ready` | `Converged`, `Progressing`, `Crashed`, `LaunchFailed`, `ProvisioningFailed`, `HypervisorError`, `NetdUnavailable`, `Deleting` |
| `DisksReady` | `Ready`, `DiskNotReady`, `DiskBusy`, `DiskMissing` |
| `NetworkReady` | `Ready`, `NetworkNotReady`, `NetdUnavailable`, `PortLost` |
| `HypervisorReachable` | `Reachable`, `Unreachable`, `CannotVerify`, `LegacyOrphan` |
| `ConsoleReady` | `Ready`, `NotRunning` |
| `RestartRequired` | `ConfigChanged`, `PortLost` |
| `DevicesPending` | `GuestPaused`, `HotplugFailed` |
| `CrashLoopBackOff` | `BackingOff` |

### 7.6 Dependencies

The VM controller never creates disks or networks. `POST /vms` with
`image` writes the VM and a new Disk object (spec: linked clone of the
image, size, `extend_root = {mode: offline, seq: 1}`, `owner = vm/<id>`)
and the claim in one transaction (images.md §7 invariant). The VM waits
with `DisksReady=False/DiskNotReady` until every referenced disk is
`Ready`; a VM can therefore be created with `power: running` from an
image that is still downloading. `NetworkReady` requires every
referenced Network to be `Ready`.

## 8. Instances

### 8.1 `launch.json`

Written by the control plane (`0600`, atomic tmp + rename) right before
the intent record:

```json
{
  "version": 1,
  "vm_id": "…",
  "instance_id": "…",
  "hypervisor": "cloudhypervisor",
  "argv": ["/usr/local/bin/cloud-hypervisor", "--api-socket", "path=…/api.sock", "…"],
  "fallback": null,
  "api_socket": "/run/glidex-cp/vms/<id>/api.sock",
  "console_socket": "/run/glidex-cp/vms/<id>/console.sock",
  "shim_socket": "/run/glidex-cp/vms/<id>/shim.sock",
  "log_path": "/run/glidex-cp/vms/<id>/console.log",
  "log_max_bytes": 16777216,
  "ready_timeout_secs": 30,
  "host_shutdown_grace_secs": 60,
  "spec": { "…": "VmSpec at launched_generation (§7.3)" }
}
```

- `fallback` (QEMU only): `{ "argv": [...], "when_output_matches": ["cpu", "msr"] }`,
  today's "retry once without `-cpu host`" rule (hypervisors.md), now
  run by the shim: if the hypervisor exits before its socket answers and
  its captured output contains one of the strings (case-insensitive),
  the shim runs `fallback.argv` once.
- The environment is not configurable: the shim starts the hypervisor
  with `LC_ALL=C`, `PATH=/usr/sbin:/usr/bin:/sbin:/bin` and nothing else.
- No secret is ever in `argv` (it is visible in `/proc`): credentials
  reach the guest only through the seed image, as today.

### 8.2 Cloud Hypervisor argv (D9)

Built by `cloud_hypervisor.rs` from the same inputs as today's
`VmCreateConfig`, in this order (F1, F2):

| Today's `vm.create` field | argv |
|---|---|
| — | `--api-socket path=<api.sock>` |
| `cpus {boot_vcpus, max_vcpus}` | `--cpus boot=<n>,max=<n>` |
| `memory {size, shared, hugepages}` | `--memory size=<mib>M[,shared=on][,hugepages=on]` |
| `payload.firmware` | `--firmware <path>` |
| `payload.kernel`, `payload.cmdline` | `--kernel <path> --cmdline <args>` (a separate argv element; not split) |
| `disks[]` (`[root, data…, seed]`) | one `--disk path=<q>[,readonly=on][,image_type=<t>][,backing_files=on]` each |
| `net[]` | one `--net id=net<i>,mac=<mac>,num_queues=<2·qp>[,mtu=<m>]` plus `tap=<if>` or `vhost_user=on,socket=<q>,vhost_mode=server` |
| `devices[]` (VFIO) | one `--device path=<q>,id=_vfio_…` each |
| `console`, `serial` (boot-mode table, hypervisors.md) | `--console tty --serial off` (kernel boot) or `--console off --serial tty` (firmware boot) |

`<q>` is a path written as `"<path>"` when it contains `,` (F2).
**Admission rule (new):** every path that reaches a hypervisor command
line (`kernel_image_path`, `firmware_path`, `rootfs_path`,
`cloud_init_path`, managed disk files, VFIO sysfs paths) must not
contain `"`, a control character or a newline: `400 invalid_config`.
QEMU's `-blockdev` JSON already needs no escaping; its `-drive`/`-kernel`
values follow QEMU's comma doubling, as today.

Image type names on the command line map from `ImageType` as `Raw` →
`raw`, `Qcow2` → `qcow2`, `Vhdx` → `vhdx`, `FixedVhd` → `vhd`; CH's
parser rejects `FixedVhd`/`fixedvhd` (F1). `vhost_user=on` and `=true`
are both accepted; glidex writes `on`. The argv unit tests pin all of
these.
Paths under `/tmp` and `/var/tmp` are refused at admission when the
runner is `systemd`, because the VM unit has its own `PrivateTmp=`
(§13.1) and would not see the control plane's `/tmp`.

QEMU's `LaunchSpec::args` is unchanged except that `-S` is dropped.

### 8.3 Shim process

```
glidex-vm-shim --dir <vm runtime dir> [--vm <id>]
```

`--vm` (the unit's `%i`) must equal `launch.json`'s `vm_id`. Exit status:
`0` after `release`; `2` unusable `launch.json` or a binary not on the
allowlist (nothing launched); `1` internal error.

**Allowlist.** `argv[0]` (and `fallback.argv[0]`) must canonicalize to a
path listed in `/etc/glidex/vm-shim.json` (`root:root 0644`, written by
the installer):

```json
{ "hypervisors": ["/usr/local/bin/cloud-hypervisor", "/usr/bin/qemu-system-x86_64"] }
```

Without that file the list is every existing `cloud-hypervisor` and
`qemu-system-<arch>` in `/usr/local/bin` and `/usr/bin`. Why: the polkit
rule lets the service user start any `glidex-vm@` instance, and the VM
unit has device access the control-plane unit no longer has (§13.3); the
shim must not become "run anything with `/dev/kvm` and VFIO". Arguments
are not filtered: a compromised control plane can still run any VM it
likes, which it can today.

Lifecycle:

1. Read and validate `launch.json`. Write `instance.json` with
   `phase: launching`, own pid and starttime, boot id.
2. Open the log (§8.6), allocate the PTY (both ends close-on-exec), bind
   `console.sock`, start the console proxy thread, and spawn the
   hypervisor from the main thread with the PTY slave as stdin/stdout and
   its stderr on a pipe the proxy logs. D8, the hypervisor dies with its
   shim, is kept two ways in `pre_exec`: `setsid` + `TIOCSCTTY` make the
   PTY its controlling terminal, so the master closing when the shim dies
   hangs the terminal up and sends it SIGHUP (raw mode: no other terminal
   signal reaches it; the shim's own SIG_IGNs are reset to SIG_DFL first,
   since ignored dispositions survive exec); and `PR_SET_PDEATHSIG =
   SIGKILL` for binaries without file capabilities (F7). Record
   `hypervisor_pid` and its starttime. The environment is cleared to
   `LC_ALL=C` and a fixed `PATH`.
3. Wait up to `ready_timeout_secs` for `api_socket` to accept and answer
   (CH: `GET /vmm.ping`; QEMU: QMP greeting), watching for early exit
   (and running `fallback` once, §8.1). Then `phase: running` — or, if
   the hypervisor exited, write the exit with cause `LaunchFailed` and
   the captured output as `message`, `phase: exited`.
4. Bind `shim.sock`; send `READY=1` (systemd runner). READY is sent in
   both outcomes of step 3, so the unit start job completes and the
   console and log stay inspectable either way.
5. Serve `shim.sock` and reap. On hypervisor exit, classify (§7.5),
   append the exit line to the log, write the exit, `phase: exited`, and
   keep serving the console listener (console.md "listener outlives the
   PTY") until `release`.
6. `release`: stop the proxy (drain the master into the log), unlink
   `console.sock`, `api.sock`, `shim.sock`, exit 0. `instance.json` and
   the logs stay until the VM's `vm.runtime` finalizer.

Signals: SIGTERM → `stop` with `host_shutdown_grace_secs`, cause
`Terminated` unless a `stop`/`kill` arrived first, then `release` once
the hypervisor is gone. SIGINT, SIGHUP and SIGPIPE are ignored.

`GLIDEX_VM_SHIM_ALLOW` (colon-separated paths) adds to the allowlist only
when the shim does not run under systemd (no `INVOCATION_ID`): dev runs
and tests. A unit's environment comes from its unit file, which the
control plane cannot write.

**Invariant.** The shim never writes the database and never contacts
netd or systemd. It knows one VM. It must keep working while the control
plane is down and across control-plane upgrades.

### 8.4 `instance.json`

Written by the shim, atomic tmp + rename, on every phase change:

```json
{
  "version": 1,
  "vm_id": "…",
  "instance_id": "…",
  "boot_id": "…",
  "shim_pid": 4242, "shim_starttime": 123456,
  "hypervisor_pid": 4250, "hypervisor_starttime": 123470,
  "phase": "launching | running | exited",
  "launched_at": 1791000000,
  "stop": { "requested_at": 1791000500, "grace_secs": 30, "deadline": 1791000530 },
  "exit": { "at": 1791000520, "cause": "requested", "code": 0, "signal": null, "message": null }
}
```

### 8.5 `shim.sock` protocol

`0600` (service user only; the VM directory is `0700`). The shim also
checks that the `SO_PEERCRED` uid is its own or 0 (root, e.g. an
operator's diagnostics). Newline-delimited JSON, max
64 KiB per line, one request in flight per connection, any number of
sequential connections; the first message must be `hello`.

| Op | Args → result | Notes |
|---|---|---|
| `hello` | `{protocol: 1}` → `{protocol, shim_version}` | the shim answers with the highest protocol ≤ the client's it speaks |
| `status` | – → `instance.json` contents | |
| `stop` | `{grace_secs}` → – | resume if paused, power button (CH `vm.power-button`, QEMU `system_powerdown`), wait, then `kill`. Idempotent; a second `stop` can only shorten the deadline. `grace_secs = 0` = `kill` |
| `kill` | – → – | stop the hypervisor without the guest: QEMU QMP `quit`, CH `PUT /vmm.shutdown` (closes its disks, F8); SIGKILL if it is still there after 2 s |
| `release` | – → – | error `not_exited` while the hypervisor runs |

Errors: `{"id": n, "error": {"code", "message"}}` with codes
`protocol_error`, `invalid_argument`, `not_exited`, `internal`.
Compatibility: a control plane speaks protocol N and N−1, so shims
started by the previous release stay manageable after an upgrade.

### 8.6 Console log (D14)

- Opened `O_APPEND`, never truncated. Each launch appends
  `\r\n--- glidex: instance <instance_id> started <RFC 3339> ---\r\n`;
  each exit `--- glidex: instance <instance_id> exited: <cause>[ (status <n> | signal <n>)] ---`.
  The hypervisor's stderr goes to the same file, as today.
- Rotation: before a PTY write that would take the file past
  `log_max_bytes`, the proxy thread renames `console.log` to
  `console.log.1` (replacing it) and opens a new `console.log`. Writes
  are whole PTY reads, so nothing is split; at most 2 × `log_max_bytes`
  per VM in the runtime directory (tmpfs).
- Replay on connect sends the current `console.log` only, through a
  per-client queue: what a client's socket does not take now waits (up
  to the replay plus 4 MiB), so a slow client neither stalls the console
  nor loses output; one further behind is dropped and can reconnect.
  Because the replay spans earlier instances, expect-style clients should
  skip it up to the current instance's `started` line.
  `GET /vms/{id}/console/log` gains `?previous=true` for `console.log.1`
  (`404` if none).
- Logs are removed by the `vm.runtime` finalizer, not at stop. They do
  not survive a host reboot (`/run`), as today.

### 8.7 Liveness: establishing "no instance" (D8)

An instance of VM `v` is **live** if any of these holds:

1. the unit `glidex-vm@<v>.service` has `ActiveState` `active`,
   `activating`, `deactivating` or `reloading` (systemd runner);
2. a process exists with pid = `shim_pid` and starttime =
   `shim_starttime`, taken from `status.instance` or, if present, from
   `instance.json`;
3. a process exists with `hypervisor_pid` and `hypervisor_starttime`;
4. `api.sock` or `shim.sock` accepts a connection.

Checks 2 and 3 apply only while `status.instance.boot_id` (or
`instance.json`'s `boot_id`) is the current boot id; after a reboot the
pids name nothing. Check 1 does not apply to the detached runner.

Before a launch the controller needs **every applicable check made and
none true**. It then clears `status.instance`, recording `HostReboot`
(boot id changed) or `Lost` (an intent with no `instance.json`) as
`last_exit` if no exit was recorded. A check that cannot be made — the
system bus unreachable, `/proc` unreadable, a socket path that exists
but returns an error other than `ECONNREFUSED`/`ENOENT` — sets `phase:
Unknown`, `HypervisorReachable=Unknown/CannotVerify`, and blocks the
launch until a later resync can make it.

### 8.8 Runners

`reconcile.vm_runner` = `auto` (default), `systemd`, or `detached`.
`auto` is `systemd` when the control plane runs as its own service:
`INVOCATION_ID` is set **and** `/proc/self/cgroup` names a
`glidex-control-plane*.service` unit; else `detached`. `INVOCATION_ID`
alone marks every service, so a control plane (or its test suite)
started from another one, such as a CI runner or a transient unit, got
the systemd runner and its VMs' paths under `/tmp` refused.

**systemd.** One `zbus` connection to the system bus, opened at startup
and re-opened on error (§9.4 orders it before adoption).

- Launch: `Manager.StartUnit("glidex-vm@<id>.service", "fail")`, then
  wait for the `JobRemoved` signal of that job (subscribe before the
  call). Result `done` → read `instance.json`. Any other result →
  read `instance.json` if it exists (a `LaunchFailed` exit explains it),
  otherwise `Ready=False/ProvisioningFailed` with the job result.
- Stop escalation (§9.1 step 4): `Manager.StopUnit(…, "replace")`, then
  `Manager.KillUnit(…, "all", 9)`.
- Observation: the unit's `ActiveState` (§8.7 check 1);
  `Manager.Subscribe()` so `JobRemoved` is delivered. Exits are noticed
  by the pidfd exit watch and the resync (§9.2), not by unit signals.
- D-Bus errors map to conditions: `org.freedesktop.DBus.Error.AccessDenied`
  / `InteractiveAuthorizationRequired` → `Ready=False/ProvisioningFailed`
  with "polkit rule missing" (installer problem); `NoSuchUnit` →
  "template unit not installed"; bus unreachable → `Unknown` (§8.7).

**detached.** Spawn the shim in its own session (`setsid`) with stdio on
`/dev/null`, and reap it from a thread whenever it exits; if the
control plane exits first, the shim is reparented to init (or the
nearest subreaper) and survives it. The control plane waits up to
`ready_timeout_secs + 5` for `instance.json` to leave `launching`.
Stop escalation: SIGTERM, then SIGKILL, to the verified shim pid (the
hypervisor follows, §8.3).

Test-only overrides: `GLIDEX_SYSTEMD_BUS=session` uses the user's own
systemd manager (no root, no polkit), and `GLIDEX_VM_UNIT_RUN_DIR` names
the run directory a test template points at. Shim binary: `GLIDEX_VM_SHIM`, else
next to the control-plane executable, else `PATH`. Test harnesses must
not kill the control plane's process group expecting VMs to die (§19).

## 9. VM controller

### 9.1 Reconcile

Each step is idempotent and may end the round with "requeue after *t*".

1. **Observe**: §8.7 checks, `instance.json` (or `shim.sock status`),
   the hypervisor (CH `GET /vm.info`: state and device ids; QEMU
   `query-status`, `qom-list /machine/peripheral`), netd `list_vm_ports`
   for the VM, disk records. Write observed fields to status.
2. **Deletion**: if requested, steps 3–4 with desired `Stopped`, then
   the finalizers (§6.3). End.
3. **Exit**: for an exited instance, apply §7.5, append the event,
   `release` the shim (systemd: the unit then goes inactive), set
   `last_exit`, clear `status.instance` (ending its claims, D11).
4. **Desired `Stopped`, instance live**: set `stop_deadline = now +
   stop_grace_secs + 15 s` if unset; send `stop {stop_grace_secs}` every
   round; requeue in `min(5 s, deadline − now)`. Past `stop_deadline`
   or with an unreachable shim: escalate (§8.8). **No live instance**:
   detach NIC ports (`detach_vm_port` per `status.nics`, removing each
   entry after its success), clear `stop_deadline`.
5. **Desired `Running`/`Paused`, no live instance**: wait for
   `next_restart_at`; require `DisksReady`, `NetworkReady` and §8.7
   (running-VM quota was checked at admission and is not re-checked).
   **Provisioning**:
   1. regenerate the cloud-init seed; prepare QEMU firmware vars (as
      `start_vm` does today);
   2. for each NIC: add it to `status.nics` (write), then
      `attach_vm_port` (idempotent; ports kept from a previous instance
      of this VM are reused, F5); record `ipv4`;
   3. build and write `launch.json` (§8.1);
   4. write the intent record `status.instance` (new `instance_id`,
      runner, boot id, `launched_generation`, `disks`, `vfio_devices`);
   5. start the runner (§8.8). `phase: Starting`.

   On failure before step 5.4: detach the ports added in this round,
   `phase: Failed`, requeue with backoff. After 5.4, failures are
   handled by the next round's observation (§7.5 `LaunchFailed`/`Lost`).
   Ports are **not** detached between a crash and its relaunch (D16).
6. **Live instance whose guest is not running** (CH `vm.info` without a
   VM or in `Created`; QEMU `prelaunch`): cannot happen with D9; if it
   does, `kill` it, `Ready=False/HypervisorError`, and let step 5 launch
   again with backoff. Never patched up through the API.
7. **Drift** (guest observed): desired `Paused` and running → `pause`;
   desired `Running` and paused → `resume`. VFIO: hot-plug spec devices
   the hypervisor lacks and unplug the reverse, only while running
   (`DevicesPending` otherwise); update `instance.vfio_devices` after
   each success. If the launched seed had a growpart block, set
   `seed_growpart_seq` once the guest is observed running (§10.1).
8. Derive `phase`, `Ready`, `RestartRequired`, `ConsoleReady`; set
   `observed_generation`.

### 9.2 Triggers

- API spec write → enqueue the object.
- Exit watch: `pidfd_open` on each live `hypervisor_pid` and `shim_pid`
  (non-children are fine; the exit status comes from `instance.json`),
  one task per watched process → enqueue. There is no unit
  `PropertiesChanged` trigger: an exit the watch misses (e.g. one that
  happened while the control plane was down) is found by the resync.
  Replaces the 2 s `reap_exited_vms` poll.
- netd reconnect → enqueue every VM with `status.nics`.
- Disk or Network becoming `Ready`, Image becoming `Ready` → enqueue the
  objects that reference it.
- Resync: every object every `reconcile.resync_secs` (30), jittered.

### 9.3 Work queue

One queue for all kinds, keyed by `(kind, id)`, deduplicating: a key is
queued at most once and processed by at most one worker at a time; a key
enqueued while processing is processed again afterwards.
`reconcile.workers` (4) workers. Per-key error backoff: 1 s doubling to
300 s, reset on a successful round. Explicit "requeue after *t*" is not
an error and does not grow the backoff.

### 9.4 Startup order

1. Open the store; migrate (§6.6). Open the system bus (systemd runner);
   if it cannot be reached, continue — affected VMs become `Unknown`.
2. **Observe every VM** (step 1 of §9.1, no actions). Instances that
   match `status.instance` are adopted as they are. A live instance that
   does not match (a lost status write, a restored database) is adopted
   from its `instance.json` and `launch.json`, event `AdoptedUnrecorded`.
3. Enumerate `glidex-vm@*.service` units and `vms/*/` directories; those
   whose id has no VM record are **orphans** (D17): logged and listed by
   `GET /system/reconcile` (§12.5), never stopped or removed.
4. netd `sync_vms {running}` with `running` = every VM whose
   `status.nics` is non-empty (D16). **Invariant:** never sent before
   step 2 completes; the same set whenever netd restarted (its
   `netd.sock` was bound anew: device and inode changed, checked every
   resync). Not sent at all while no VM of this control plane uses
   networks: netd's sync is host-wide, and a control plane with nothing to
   keep must not drop another's ports (a scratch instance next to the
   real one, as the UI end-to-end suite runs).
5. Start the exit watch, the workers (enqueue everything), then the API
   listeners.

`VmManager::initialize` (mark everything stopped) and
`VmManager::shutdown` (kill every VM) are removed. On SIGTERM the
control plane stops its API listeners and exits without waiting for
in-flight reconciles: every step is safe to interrupt (G5), and the next
start finishes or undoes it. Instances keep running; nothing is
stopped.

## 10. Disk, Image and Network controllers

### 10.1 Disk

Spec: `{format, size_bytes, origin, owner: Option<VmRef>,
extend_root: Option<{mode, seq}>}`. Status: `phase` (`Pending |
Creating | Ready | Resizing | Missing | Failed`), `actual_size_bytes`,
`partitions`, `free_tail_bytes`, `pending_growpart`,
`applied_extend_root_seq`, `attached_to` (mirrors the claim), conditions
`Ready` (`Converged`, `ImageNotReady`, `Busy`, `ResizePending`,
`ResizeInvalid`, `ToolUnavailable`, `IoError`, `FileMissing`).

- **Create.** `Pending` until the source image is `Ready`, then today's
  temp-file + rename. A crash mid-create leaves `Creating` and the
  disk's own dot-file temporary (named by disk id, images.md §2): the
  next round deletes that temporary and starts over. Other orphans are
  never deleted.
- **Resize** is a spec change (`POST /disks/{id}/resize` writes
  `size_bytes`). Admission keeps today's validation (`invalid_disk`,
  `details.min_size_bytes`). Applied only while no live instance has the
  disk (no VM's `status.instance.disks` contains it), otherwise
  `ResizePending`. Re-validated before applying; a no-longer-valid
  shrink → `Ready=False/ResizeInvalid`, spec left as written.
- **Extend-root** (D19). `POST /disks/{id}/extend-root {mode}` writes
  `extend_root = {mode, seq: previous + 1}`. Applied when `seq >
  applied_extend_root_seq` and no live instance has the disk: `offline`
  edits the partition table; `on-boot` sets `pending_growpart`. Then
  `applied_extend_root_seq = seq`. `pending_growpart` is cleared when
  the claiming VM's `seed_growpart_seq ≥ applied_extend_root_seq`.
- **BusyGuard** stays as the in-process lock around file operations.
  The disk controller takes it only after re-checking, under a store
  read transaction, that no live instance holds the disk; the VM
  controller treats a busy disk as `DisksReady=False/DiskBusy` and
  requeues.
- **File gone** → `Missing`, never recreated.

### 10.2 Image

Spec: `{source, expected_sha256, retry_seq}`; `source` and
`expected_sha256` immutable. Status: today's `ImageStatus` plus
`applied_retry_seq`. The download task (images.md §5, resume rules
unchanged) is the controller's action for `Downloading`/`Verifying`;
at most `GLIDEX_MAX_DOWNLOADS` run, the rest stay queued in the work
queue with "requeue after". `Failed` is not retried until
`POST /images/{id}/retry` bumps `retry_seq`. A `Ready` image whose file
disappears becomes `Missing` and is **never** re-downloaded: catalog
URLs point at "current/latest", so a re-download would sit under linked
overlays written against a different file.
A deleting image (`deletion_requested_at`) is finished before anything
else: the download aborted, the record removed, then the files (§6.3).

### 10.3 Network

Spec: today's `Network` fields. Status: `phase` (`Pending | Ready |
Degraded | NetdUnavailable`), netd's bridge/NAT state. The controller
makes netd's records match (`ensure_bridge`, `ensure_nat`; idempotent)
and runs `network.netd` on deletion. netd reconciles the *host*; the
control plane reconciles *netd*.

VM ports belong to the VM controller. Drift on a live instance:

- OVS port missing, tap present → `attach_vm_port` re-adds it (F5);
- vhost-user port missing → re-added; OVS reconnects to the
  hypervisor's socket;
- tap missing → not recreated (the hypervisor holds an fd to the old
  device): `NetworkReady=False/PortLost`, `RestartRequired=True/PortLost`.

## 11. Conditions for Disk, Image, Network

Each has `Ready` (reasons above) and nothing else; their `Ready` follows
D13 (`Ready` when the phase is `Ready` and spec and status agree).

## 12. API

### 12.1 Endpoints

| Endpoint | Becomes | Cedar (security.md §7) |
|---|---|---|
| `POST /vms` | create, `power` default `Stopped` | unchanged (`createVm` + §7.4 compound) |
| `POST /vms/{id}/start` | `spec.power = Running` (from `Paused`: resume) | `startVm` |
| `POST /vms/{id}/stop[?graceful_timeout_secs=N]` | `spec.power = Stopped` [+ `stop_grace_secs = N`] | `stopVm` |
| `POST /vms/{id}/pause` | `spec.power = Paused` | `pauseVm` |
| `POST`/`DELETE /vms/{id}/devices` | edit `vfio_devices` (+ claim) | unchanged |
| `POST /vms/{id}/disks`, `DELETE /vms/{id}/disks/{disk}` | edit `data_disks` (+ claim); no longer refused on a running VM: takes effect next launch | unchanged |
| `PATCH /vms/{id}` (new) | JSON merge patch of `spec` | per changed field, all must allow: `power` → `startVm`/`stopVm`/`pauseVm`; `vfio_devices` → `attachDevice`/`detachDevice` + `usePciDevice`; `data_disks` → `attachDisk`/`detachDisk` + `useDisk`; `networks` → `attachNetwork` + `useNetwork` for added ones, `detachNetwork` when any is removed; `credential` → `updateVm` + `useCredential`; other mutable fields → **`updateVm` (new action)** |
| `DELETE /vms/{id}[?keep_disk=true]` | deletion request | `deleteVm` |
| `POST /disks/{id}/resize`, `/extend-root` | Disk spec writes | unchanged |
| `POST /images/{id}/retry` (new) | bump `retry_seq` | `pullImage` |
| `DELETE /images/{id}[?wait]`, `DELETE /networks/{name}[?wait]` | deletion request (§6.3) | `deleteImage`; `deleteNetwork` / `deleteProjectNetwork` (unchanged) |
| `GET /watch[?kinds=…][&project=…]` (new) | live stream (§12.6) | any authenticated caller; each kind filtered as its list endpoint |
| `GET /{vms,disks,images,networks}/{id}/events` (new) | event ring | `readVm` / `readDisk` / `readImage` / `readNetwork` |
| `GET /system/reconcile` (new) | §12.5 | **`readSystemStatus` (new) on `Host::"local"`**, in the `host.read` group (with `readOvsStatus`, `listBridges`, …) |

`updateVm` joins the permission group of `startVm`/`stopVm`
(security.md §7.2) and the schema in `policies/glidex.cedarschema`.
`PATCH` refuses immutable fields with `400 invalid_config` before
authorization of the remaining fields.

`If-Match: <resource_version>` on a lifecycle action (`start`, `stop`,
`pause`) or `PATCH` → `412 precondition_failed` (with
`details.resource_version`) when it does not match; other writes ignore
it. Without it, last writer wins (today). `VmResponse` carries
`resource_version`.

### 12.2 Quotas

`running_vms` usage counts VMs whose `spec.power` is not `Stopped` (was:
observed `Running`/`Paused`), so crash loops and host reboots can never
exceed it and a VM blocked on a downloading image still counts. Checked
when a write moves `power` away from `Stopped`. `vcpus` and `memory_mib`
deltas of a `PATCH` are checked like today's create. `QuotaMode` and
break-glass (`quota_exceeded`, security.md §6.3) are unchanged.

### 12.3 `?wait=<secs>` (D20)

Accepted on every write above (0–300; gxctl and the UI send 60). The
response is held until `observed_generation ≥` the written generation
and one of:

- `Ready=True` → `200` with the object (for a delete: `200` once the
  record is gone);
- `phase: Failed` for that generation → the error envelope today's
  synchronous call would have returned, chosen from the `Ready` reason:
  `LaunchFailed`/`HypervisorError` → `500 hypervisor_error`,
  `NetdUnavailable` → `503 netd_unavailable`, `ProvisioningFailed` →
  `500 hypervisor_error` (or `503 tool_unavailable` /
  `500 credential_error` by cause), with the object in `details.vm`. The
  spec stays as written and the controller keeps retrying;
- timeout, including while waiting on a dependency → `202` with the
  object.

Without `wait`: `202` with the object.

### 12.4 Responses

Flat fields kept (§7.4); `?view=full` returns `{meta, spec, status}`
to host readers only (`readSystemStatus`): it carries host paths, PIDs
and the boot id.
`GET /vms/{id}/console` `available` is true while an instance exists
(including after the guest exited, until `release`).

Events:

```json
GET /vms/{id}/events
{ "events": [ { "at": 1791000520, "actor": "guest", "kind": "normal",
                "reason": "CleanExit", "message": "guest powered off; desired state set to stopped" } ] }
```

### 12.5 `GET /system/reconcile`

```json
{ "runner": "systemd", "bus": "connected", "netd": "connected",
  "queue": { "pending": 0, "in_flight": 1 },
  "orphans": [ { "kind": "unit", "id": "glidex-vm@<uuid>.service" },
               { "kind": "runtime_dir", "id": "<uuid>" } ],
  "unknown_vms": ["<uuid>"] }
```

`GET /health` stays public and unchanged.

### 12.6 `GET /watch`

A live stream of the objects the caller may list, as server-sent events
(`text/event-stream`), so clients need not poll.

- **Query.** `kinds` (comma-separated `vms`, `disks`, `images`,
  `networks`, singular accepted; default all; unknown → `400 invalid`),
  `project` (VMs and disks of that project only, as `?project=` on the
  lists).
- **Events.** On connect one `added` per visible object, then `synced`
  (data `{}`). Then `added`, `modified`, `deleted` as objects change.
  Data is `{"kind": "vm"|"disk"|"image"|"network", "id": "…", "object":
  {…}}`, where `object` is exactly what the list endpoint returns for it
  (`VmResponse`, `DiskResponse`, `ImageResponse`, the network view);
  `deleted` carries no `object`. A keep-alive comment every 15 s.

```
event: modified
data: {"kind":"vm","id":"<uuid>","object":{"name":"web-1","state":"starting","desired_state":"running",…}}
```

- **How.** Every store write (VM, disk, image, network, and image
  download progress) rings a change bell (a `watch` channel that carries
  no data; `?wait` waits on it too). Each stream re-lists with its
  list endpoints' visibility rules when it rings, after 250 ms to
  coalesce, and at least every 10 s, and sends the differences. So
  authorization, including policy and membership changes, applies to
  every event, and nothing is replayed from a log: a client that
  reconnects gets a fresh snapshot.
- **Limits.** A stream ends after 5 minutes with `event: expired`,
  which bounds how long a revoked session keeps receiving events;
  clients reconnect (EventSource does on its own). A stream also ends
  when the caller's access can no longer be resolved. At most 64 streams
  host-wide; more → `503 too_many_watchers` (poll instead).
- **Clients.** The UI keeps one stream for the selected project and
  refreshes pages from it, polling only while it is not connected
  (web-ui.md). `gxctl watch` prints changes (cli.md).

## 13. systemd, polkit, installer

### 13.1 `packaging/glidex-vm@.service`

```ini
# Rendered by glidex-install. One instance per running VM, started only
# by the control plane (spec/reconciliation.md). Never enabled.
[Unit]
Description=glidex VM %i
# Ordering only, so that at host shutdown VMs stop before netd and Open
# vSwitch. Not PartOf=/BindsTo= the control plane: VMs outlive it.
After=glidex-netd.service openvswitch-switch.service openvswitch.service glidex-ovs-vswitchd.service

[Service]
Type=notify
NotifyAccess=main
User=@USER@
Group=glidex
# kvm, when the group exists.
SupplementaryGroups=@GROUPS@
ExecStart=@BIN@/glidex-vm-shim --vm %i --dir /run/glidex-cp/vms/%i
TimeoutStartSec=60
# SIGTERM goes to the shim only: it presses the power button, waits
# host_shutdown_grace_secs, then kills the hypervisor. Anything left is
# SIGKILLed after TimeoutStopSec, which covers the 300 s maximum grace.
KillMode=mixed
TimeoutStopSec=330
# Restarting is the VM controller's decision, never systemd's.
Restart=no
Slice=glidex-vms.slice
UMask=0077
# The hypervisor sandbox, moved here from glidex-control-plane.service
# (security.md §9). No NoNewPrivileges= nor anything implying it:
# cloud-hypervisor's cap_net_admin file capability would be ignored.
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=@HOME@ /run/glidex-cp/vms/%i -/run/glidex/vhost
DevicePolicy=closed
DeviceAllow=/dev/kvm rw
DeviceAllow=/dev/vfio/vfio rw
DeviceAllow=char-vfio rw
DeviceAllow=/dev/net/tun rw
DeviceAllow=/dev/vhost-net rw
```

The unit also sets `IOAccounting=yes` and `MemoryAccounting=yes` for
per-VM metering ([metering.md §5.1](metering.md#51-cpu-and-memory-the-vm-cgroup)).
They go on the unit, not the slice: on a slice they only enable the
controllers in its parent.

`packaging/glidex-vms.slice`: `[Unit] Description=glidex VMs`, no limits
(a place for site resource controls). No `[Install]` in either:
relaunching after a host reboot is the control plane's job (D10).

### 13.2 Polkit

`/etc/polkit-1/rules.d/50-glidex-vm.rules` (root `0644`, rendered with
the service user):

```js
// glidex: the control plane may start, stop and kill its own VM units
// (spec/reconciliation.md §13.2), and nothing else.
polkit.addRule(function (action, subject) {
  if (action.id == "org.freedesktop.systemd1.manage-units" &&
      subject.user == "@USER@" &&
      /^glidex-vm@[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\.service$/
        .test(action.lookup("unit") || "") &&
      ["start", "stop", "kill"].indexOf(action.lookup("verb")) >= 0) {
    return polkit.Result.YES;
  }
});
```

Reads (properties, signals, `Subscribe`) need no authorization (F6).

### 13.3 `glidex-control-plane.service`

- The comment and behaviour "stopping the service stops running VMs" go.
  `RuntimeDirectoryPreserve=yes` stays and is now required (the shims'
  directories live in `/run/glidex-cp/vms/`).
- No hypervisor children: drop all `DeviceAllow=` (keep
  `DevicePolicy=closed`) and `ReadWritePaths=-/run/glidex/vhost`; add
  `NoNewPrivileges=yes`, `RestrictAddressFamilies=AF_UNIX AF_INET
  AF_INET6 AF_NETLINK`, `LockPersonality=yes`, `RestrictSUIDSGID=yes`,
  `SystemCallFilter=@system-service`, `ProtectKernelTunables=yes`,
  `ProtectKernelModules=yes`, `ProtectKernelLogs=yes`. Its remaining
  children (`qemu-img`, `qemu-io`, `sgdisk`, `growpart`, `mkdosfs`,
  `mcopy`) need none of what is removed. As built (M4) the unit also sets
  `SystemCallArchitectures=native`, `ProtectControlGroups=yes`,
  `RestrictNamespaces=yes` and `RestrictRealtime=yes`.
- The detached runner cannot work inside this sandbox (no_new_privs, a
  closed `/dev`): with `vm_runner` `"detached"` and `NoNewPrivs: 1` in
  `/proc/self/status` the control plane refuses to start. Under the
  systemd runner `/dev/kvm` is checked with `access(2)` and is not fatal
  (the VM units open it); QEMU's vhost-net check uses `access(2)` too,
  since this unit can no longer open the device.

### 13.4 Installer and uninstaller

- Install `glidex-vm-shim` next to the other binaries; render
  `glidex-vm@.service`, `glidex-vms.slice`, the polkit rule and
  `/etc/glidex/vm-shim.json`; `systemd-analyze verify` all units.
- Before restarting `glidex-control-plane` during an upgrade from a
  pre-M1 version, print that running VMs will be stopped once (§6.6).
- `uninstall`: `systemctl stop 'glidex-vm@*.service'` first (each takes
  up to its grace), then everything else; listed by `--dry-run`. Remove
  the polkit rule, `vm-shim.json`, unit and slice.

## 14. Configuration

New keys in `/etc/glidex/control-plane.json` (and the `.example`):

```json
"reconcile": {
  "vm_runner": "auto",
  "workers": 4,
  "resync_secs": 30,
  "on_host_boot": "resume",
  "host_shutdown_grace_secs": 60
},
"console": { "log_max_bytes": 16777216 }
```

| Key | Range | Note |
|---|---|---|
| `reconcile.vm_runner` | `auto`, `systemd`, `detached` | §8.8 |
| `reconcile.workers` | 1–64 | §9.3 |
| `reconcile.resync_secs` | 5–3600 | §9.2 |
| `reconcile.on_host_boot` | `resume`, `stop` | default for new VMs and migrated ones |
| `reconcile.host_shutdown_grace_secs` | 0–300 | written into each `launch.json`; applies to instances launched after a change |
| `console.log_max_bytes` | 1 MiB–256 MiB | same |

Out-of-range values stop the control plane at startup, as for the
existing keys.

## 15. Concurrency summary

- `VmManager`'s `RwLock<HashMap<VmId, VmEntry>>` and `Box<dyn
  HypervisorProcess>` go away; the store cache replaces the map.
- Per-object serialization by the work queue; cross-object atomicity by
  ReDB write transactions at admission.
- `BusyGuard` remains the only in-process lock held across file I/O
  (§10.1).

## 16. Hypervisor abstraction

`Hypervisor` / `HypervisorProcess` are replaced by a stateless driver
(D7, D9):

```rust
pub trait HypervisorDriver: Send + Sync {
    fn hypervisor_type(&self) -> HypervisorType;
    fn is_available(&self) -> bool;
    /// argv (and QEMU's fallback) carrying the whole VM config; pure
    /// apart from host probes (firmware vars, O_DIRECT, vhost-net).
    fn launch_args(&self, vm: &VmSpec, resolved: &Resolved) -> Result<LaunchArgs, HypervisorError>;
    fn observe(&self, inst: &InstanceSockets) -> Result<Observed, HypervisorError>;
    fn pause(&self, inst: &InstanceSockets) -> Result<(), HypervisorError>;
    fn resume(&self, inst: &InstanceSockets) -> Result<(), HypervisorError>;
    fn add_device(&self, inst: &InstanceSockets, device_path: &str) -> Result<(), HypervisorError>;
    fn remove_device(&self, inst: &InstanceSockets, device_path: &str) -> Result<(), HypervisorError>;
}

pub struct Observed { pub guest: GuestState, pub vfio_ids: Vec<String> }
pub enum GuestState { NotCreated, Created, Running, Paused, Shutdown }
```

`InstanceSockets` is the VM's runtime paths (`paths::vm_paths`).
Process lifetime (`spawn`, `kill`, `request_shutdown`, `is_running`)
moves to the shim. QMP keeps one connection per command, so the shim's
and the control plane's connections never overlap for long; both retry
a connect that fails with `EAGAIN`/`ECONNREFUSED` for up to 2 s.

## 17. Edits to existing documents

| Document | Change | Milestone |
|---|---|---|
| spec/README.md | Goal 2: "durable desired state; VMs survive control-plane restarts"; add this document | M1 |
| architecture.md | Process diagram (§5.1); layers → store + controllers; "Shutdown and reconciliation", "Threading model" → §9.4, §15 | M1/M2 |
| data-model.md | `VmState` → `PowerState` + `VmPhase`; envelope; write ordering → §6.2; startup reconciliation → §9.4; mutability → §7.3; schema 2 | M2 |
| hypervisors.md | Trait → §16; CH configured by argv (§8.2), QEMU without `-S` ("Deferred launch" removed); launch health check and CPU fallback move to the shim; `reap_exited_vms` removed | M1 |
| console.md | Proxy in the shim; shutdown on `release`; "Log files" → §8.6 | M1 |
| rest-api.md | `202`/`?wait`, `PATCH`, `/events`, `/images/{id}/retry`, `/system/reconcile`, `412`, response fields, attach-disk on running VMs | M2–M4 |
| images.md | Envelopes; §5 restart and §6 operations under the controllers (§10.1–10.2); extend-root seq | M3 |
| networking.md | §7.7 step 3 and §11.4: `sync_vms` set (D16), ports across restarts, drift (§10.3) | M1 |
| installer.md | Shim, VM unit, slice, polkit rule, `vm-shim.json`; boot sequence; uninstall; control plane no longer stops VMs | M1 |
| security.md | §7: `updateVm`, `readSystemStatus`, `PATCH` compound row; §9 sandbox split; threat model: polkit rule and shim allowlist; §10 controller audit principal | M1/M4 |
| cli.md, web-ui.md | `state` plus `→ desired_state` when different; conditions; start/stop/pause wait by default, `--no-wait`; `events` command and tab; `image retry`; attach-disk on running VMs shows "after restart" | M2–M4 |

## 18. Milestones

Each milestone is one PR (or a short series), ships on its own, and is
done when its tests in §19 pass in CI and on a KVM host.

As built: M1 and M2 landed together for the VM path (the VM controller
directly, rather than an imperative M1 rewritten for M2), followed by M3
and M4.

As built: M3. Disk, image and network controllers (`controller/disk.rs`,
`image.rs`, `network.rs`) on the shared work queue and resync.
Differences from §10:

- Disks: create, resize and extend-root are recorded and answered at
  once (`201` for a create, `202` otherwise; `?wait` up to 300 s, §12.3),
  and a VM created from `image` records a `Pending` disk owned by the VM
  in the same transaction. Resizes and extend-roots wait while a live
  instance has the disk (`ResizePending`, `ExtendRootPending`). The
  `Ready` reasons as built: `Converged`, `Progressing`, `ImageNotReady`,
  `ResizePending`, `ResizeInvalid`, `ExtendRootPending`, `InvalidDisk`,
  `ToolUnavailable`, `IoError`, `FileMissing`, `Deleting`; a running
  operation shows as `status: busy`, not as a `Busy` reason or the
  `Resizing` phase. A missing file goes back to `Ready` if it reappears.
- Images: the controller resumes a `Downloading`/`Verifying` image whose
  task is gone, starts a retry when `retry_seq > applied_retry_seq`, and
  flips `Ready` ↔ `Missing` with the file. `GLIDEX_MAX_DOWNLOADS` is
  still enforced by the download queue, not the work queue.
- Networks: phases `Ready`, `Degraded`, `NetdUnavailable` (`Pending` is
  unused). A lost bridge the network owns is re-created
  (`ensure_bridge`); a lost NAT and a dnsmasq that is not running are
  reported (`Degraded`), never re-created, because the NAT's subnet
  lives only in netd.
- Port drift (§10.3) is checked in the VM controller's round for a
  running VM. netd `sync_vms` is sent only when some VM uses networks,
  at startup and when netd restarted (socket inode changed).
- Events: `GET /{disks,images}/{id}/events` and
  `GET /networks/{name}/events`; `POST /images/{id}/retry`.

**M1 — Detached instances (imperative API unchanged).**
`glidex-hv-client`, `glidex-vm-shim`, both runners, `launch.json` /
`instance.json`, CH argv (D9), QEMU without `-S`, console log append and
rotation, §8.7 liveness, adoption at startup (§9.4 steps 2–4 against
today's `Vm` records, with `InstanceRef` stored in a new optional field),
`sync_vms` ordering, exit watch replacing `reap_exited_vms`, removal of
`shutdown()`, units, polkit, installer, allowlist. `start_vm`/`stop_vm`
call the runner synchronously. *Accept:* survival, crash-point (launch
and stop only), exit-cause, polkit and CH-argv tests.

**M2 — Store envelope and VM controller.** Schema 2 and migration (plus
the schema check in the release before), work queue, VM reconcile,
restart policy, `on_host_boot`, `?wait`, events, `PATCH`, quotas on
`spec.power`, audit principal, `/system/reconcile`. *Accept:* all VM
tests in §19, migration test from a schema-1 fixture.

**M3 — Disk, Image, Network controllers.** Declarative resize,
extend-root seq, VM from a downloading image, image retry, network
finalizers, port drift. *Accept:* disk, image and network tests.

**M4 — Polish and hardening.** UI and gxctl condition display and
`--no-wait`, `watch` (SSE) if wanted, control-plane unit tightening
(§13.3).

As built: M4. The control-plane unit sandbox of §13.3. The disk tools
and the API test suites pass under its syscall filter, no_new_privs and
address-family restrictions (run in a transient unit); the installed
unit with the systemd runner on a KVM host is the remaining check.
`GET /vms/{id}?view=full` requires `readSystemStatus` (`host.read`),
because the stored record carries host paths, PIDs and the boot id that
the flat view leaves out.

As built after M4: `GET /watch` (§12.6), and image and network deletion
through their controllers (§6.3, the `image.*` and `network.netd`
finalizers): until then `delete_image` and `delete_network` ran
synchronously in the API, and a network delete failed outright while
netd was unreachable.

## 19. Testing

Integration tests run real hypervisors (as `functional_tests.rs` does
today) under the detached runner; a separate `systemd` job runs the
same suite against installed units on a KVM host. Every test that
starts VMs deletes them at the end: the harness can no longer rely on
the control plane's exit to kill them.

- **Survival:** start a VM, `kill -9` the control plane, restart it:
  same hypervisor pid, guest uptime (over the console) and NAT address;
  `Running` with the same `instance_id`; console clients connected to
  the socket throughout saw no gap.
- **Crash points:** test-only `GLIDEX_FAULT=<point>` aborts the control
  plane at: after `status.nics` write, after `attach_vm_port`, after
  `launch.json`, after the intent record, after `StartUnit`, mid-stop,
  mid-disk-create, before/after the D12 spec write. After restart the VM
  converges and a watcher asserts at most one hypervisor process per VM
  id at all times.
- **Exit causes:** guest `poweroff` (F3) → `CleanExit`, spec stopped;
  `kill -9` of the hypervisor → `Crashed`, relaunch with backoff,
  `CrashLoopBackOff` on repeat; `restart_policy: never`; `systemctl
  stop glidex-vm@<id>` → `Terminated`, spec stopped; a broken kernel path
  → `LaunchFailed` with QEMU/CH output in the message; shim killed →
  hypervisor dies (PDEATHSIG / KillMode), `Lost` → `Crashed`.
- **Host reboot:** override the boot-id source; `Resume` vs `Stop`.
- **Host shutdown:** stopping a unit presses the power button (a paused
  guest is resumed first) and records `Terminated`.
- **Liveness:** each §8.7 check alone keeps a launch from happening; an
  unreachable bus → `Unknown`, no launch; a legacy `api.sock` →
  `LegacyOrphan`.
- **Adoption:** delete `status.instance` while the VM runs → restart
  adopts it (`AdoptedUnrecorded`), no second instance; a unit with no
  VM record → listed as orphan, still running.
- **Claims (D11):** detach a data disk from a running VM's spec; another
  VM claiming it gets `409` until the first VM's instance is gone.
- **Stale spec write (D12):** guest powers off while a `start` is in
  flight; the newer generation wins.
- **netd:** restart netd and the control plane in either order; adopted
  and crash-restarting VMs keep ports and addresses; a deleted OVS port
  is re-added; a deleted tap yields `PortLost`.
- **`?wait`:** converges → `200`; launch failure → `500
  hypervisor_error` with `details.vm`; image downloading → `202`.
- **Migration:** schema-1 fixture database → schema 2, VMs `Stopped`,
  `never_started` preserved; a schema-2 database refused by the M1
  build.
- **CH argv** (unit): every boot mode, disk kind (incl. a path with a
  comma, F2), NIC kind, VFIO, MTU, hugepages; and an integration test
  that CH v53.0 parses the full argv (F1).
- **Console log:** separators across a crash-restart; rotation at the
  limit loses no bytes (known pattern through the PTY); replay sends the
  current file only; `?previous=true`.
- **Shim protocol:** `hello` negotiation N/N−1, `release` before exit →
  `not_exited`, wrong peer uid refused, allowlist refusal (exit 2).
- **Units and polkit (F6):** `systemd-analyze verify`; as the service
  user, `StartUnit`/`StopUnit`/`KillUnit` on a `glidex-vm@<uuid>`
  succeed; on another unit name, or as another user, `AccessDenied`.

## 20. Decision log

| Item | Outcome |
|---|---|
| Q1 CH configuration | command line (D9); verified F1, F2 |
| Q2 after host reboot | resume (D10) |
| Q3 console log | append + rotate (D14) |
| Q4 systemd interface | D-Bus via `zbus` (D2, §8.8) |
| Q5 who starts units | polkit (D2, §13.2) |
| Review 1 | claims follow the instance (D11), conditional controller spec writes (D12), Ready = converged (D13), shim-owned stop and host-shutdown grace (D15), ports across restarts and `sync_vms` set (D16), orphans (D17), mutability (D18), action counters (D19), `?wait` errors (D20) |
| Review 2 (implementation readiness) | host facts verified (§4): exit status 0 also on SIGTERM, so the cause is `CleanExit` rather than "guest poweroff"; launch failures are their own cause (`LaunchFailed`); hypervisor dies with its shim (PDEATHSIG); VM-unit `PrivateTmp` forbids `/tmp` paths; CH values with `,` are quoted and `"` refused; file formats, shim protocol, D-Bus calls, Cedar actions, quota basis, config keys, schema migration and milestone acceptance specified |
