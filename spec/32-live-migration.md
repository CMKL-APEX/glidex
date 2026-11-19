# Migration: starting a stopped VM on another host, then moving a running one

**Status: LM0 built; the rest is design** (tracked in #573). LM0, the cold move, comes first: it needs only
glidex, and most of it exists. Live migration (LM1–LM6) needs hyperstor
work first, which its issues track (§7). §0 records what was checked
while writing this. Items marked **(verify)** are checked on hosts in
the milestone that uses them.

Builds on:
- [24-storage.md](24-storage.md): §9 re-placing a stopped VM (S4, S10),
  §10 exclusive access, and §11.2 the `hyperstor` driver (S23, S26, S31);
- [14-clustering.md](14-clustering.md): §8 who runs a VM, §11 the
  cluster's OVN, and §18, where live migration was left for its own
  design;
- [12-reconciliation.md](12-reconciliation.md): the VM controller;
- hyperstor's 11-frontends.md §11.4: a migration's destination attaches
  as a delegate (hyperstor D33).

Read those first. This document only describes what changes.

The scope is a VM on **Cloud Hypervisor**, moving to Cloud Hypervisor on
another host of the same cluster. Its disks are on a **shared
datastore**, hyperstor first.

## 0. Checked while writing

| # | Finding | Consequence |
|---|---|---|
| 1 | Moving a stopped VM already works: `POST /vms/{id}/place` (24-storage.md §9) re-places it when every disk is on a shared datastore, and a hyperstor datastore is shared. Its disks' workers move with it. At launch the cloud-init seed is rebuilt. If the old node's `hyperstor-vhost` still holds a volume, the hold is preempted (S26); each node's probe also lets go of holds for VMs placed elsewhere. | LM0 only adds a one-step "start on that node" and a host check (§3). |
| 2 | Cloud Hypervisor v53, which glidex pins, has `vm.send-migration` and `vm.receive-migration`. They work over `unix:<path>`, or `tcp:<host>:<port>` with an optional `tls_dir`, and take `downtime_ms`, `timeout_s` and `timeout_strategy`. v54 adds `vm.cancel-migration` and migration events. It also guarantees that a release accepts migrations from the two releases before it ("n-2"); going back to an older release isn't supported. | Pin v54.1 before live migration (decision 10). |
| 3 | **CPU profiles** exist only on Cloud Hypervisor's main branch: not in v53 or v54.1, and marked experimental. They are x86-64 with KVM only. A profile is a JSON file of CPUID and MSR masks, checked against the host when the VM is created, restored or received. Every host must have the file at the same path with the same contents. Nothing generates one from a set of hosts (upstream #7068 is open). Without a profile, the guest sees the host's CPU (`host`). | No profiles in v1. A VM moves only to a host whose KVM offers its CPUID (decision 4); profiles come later (§13 O1). |
| 4 | Every test host is AMD EPYC, but not all the same model. Two of them (node-a and node-b below) are the same model, with the same microcode. Their `/proc/cpuinfo` flags still differ by one each (`ibpb_exit_to_user`, `xtopology`), which are flags the kernel reports, not CPUID. The others differ in model. | Neither the model nor the flags decide; #7 shows what does (§9). |
| 5 | `hyperstor-vhost` offers the vhost-user protocol features `MQ`, `INFLIGHT_SHMFD`, `REPLY_ACK`, `CONFIG` and `RESET_DEVICE`. It doesn't offer `LOG_SHMFD`, and `set_log_base` is unsupported. Its I161: a ring stopped by `GET_VRING_BASE` doesn't wait for requests in flight. | Disk reads land in guest memory without Cloud Hypervisor seeing the pages change. A live copy would leave the target with stale memory. hyperstor has to add dirty logging and drain in-flight requests first (§7). |
| 6 | glidex passes no CPU model to Cloud Hypervisor (`--cpus` has only `boot`, `max`, `affinity` and `core_scheduling`). Kernels and firmware are at pinned paths, the same on every host. Each VM's runtime directory is `/run/glidex-cp/vms/<id>/` on every host, so the paths match. The volume's vhost-user socket is `/run/hyperstor/vhost/<volume>.sock` on each host. | The configuration Cloud Hypervisor sends along with the VM is valid on the target as it stands (**verify** in LM1). |
| 7 | **The LM1 spike** (2026-11-19, Cloud Hypervisor v53.0, node-a and node-b, by hand outside glidex): a 2 GiB VM booted from the host's kernel and initramfs, no disk or NIC. **node-a → node-b was refused** by Cloud Hypervisor's CPUID check on the target: leaf `0x7` EDX was `0xac000000` on the source and `0xa8000000` on the target (bit 26). The VM kept running on node-a. The two hosts have the same CPU model and microcode but different kernels (6.8 and 6.17), and KVM offers different CPUID on each. **node-b → node-a worked**, both over a unix socket forwarded by SSH (4.9 s) and over direct TCP (18.5 s). The guest carried on: its `rootdelay` ran out on the target, which then showed its shell. | Compatibility is one-way: the target's KVM must offer at least the CPUID the VM booted with. Decision 4 compares that, not models (§9). |
| 8 | In the same spike, both paths ran over 1 Gb/s links. 2 GiB at line rate takes about 17 s, as the TCP run did, so the 4.9 s run sent less (likely pages the guest never touched). | Relay against direct TCP isn't measured yet: it needs a guest that writes its memory (LM6). The cluster's peer network is 1 Gb/s on the test cluster, which bounds every migration's copy (§13 O2). |
| 9 | Also in the spike: the guest rebooted during the copy (a test command line with `panic=-1` and no root). The source's Cloud Hypervisor stopped with a fatal error, `VmReboot(VmMigrating)`, and the target aborted. **The VM was lost.** | **Verify** on v54.1. Until it is fixed upstream, glidex records it: the migration fails, and the VM is `Failed`, not running anywhere (§11). |
| 10 | Cloud Hypervisor v54.0 turns nested virtualization off by default; v53 had it on (`cpus.nested: true` in a v53 VM's config, which also has `cpus.profile: "Host"`). v54.0 also removes disk image autodetection, which glidex never relied on (it passes `image_type` for every file disk), and deprecates `iommu=on`, which glidex doesn't use. | The v54.1 pin passes `nested=on` so no VM loses nested virtualization with the upgrade (decision 10). |

## 1. Goals and non-goals

Goals:
- **LM0:** start a stopped VM on another host in one step, from the API,
  `gxctl` and the UI.
- **LM1–LM6:** move a running Cloud Hypervisor VM to another host with
  its disks on hyperstor and its NICs taps on cluster networks. The guest
  pauses for about `downtime_ms` (300 ms by default) and keeps its
  addresses, disks and console.

Not in v1:
- **Devices that stay on the host:** passthrough PCI devices (GPUs and
  other pool devices), DPU NICs, and vhost-user-net NICs (OVS-DPDK).
  Cloud Hypervisor v54 can move some VFIO devices, but not GPUs.
- **Other VMs:** QEMU VMs; Pylon VMs (30-pylon.md); VMs with disks on
  node-local datastores, since disks don't move in v1.
- **Shared-directory datastores:** the shim holds the volume's lock file
  for the VM's whole life (S16), so a second holder needs its own design.
- **Automatic moves:** draining a node by migration, and evacuation;
  migration only moves a VM when someone asks (§13 O4).

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| 1 | **A cold move is `start` with a node.** `POST /vms/{id}/start?node=N` re-places a stopped VM on `N` (24-storage.md §9's rules, unchanged), then starts it. If the VM is already placed on that node, it is a plain start. Naming a node needs `placeVm` as well as `startVm`, even when the VM is already there, so a caller without it learns nothing about the nodes. | The two-step `place`, then `start`, already works. One call is what an operator means, and no one sees the VM placed but not started. Re-placing stays a cluster operation (S10). |
| 2 | **Live migration is explicit, and one at a time per VM.** `POST /vms/{id}/migrate {node?}`, with the Cedar action `migrateVm` in `node.drain` and `host-scoped` (as `placeVm` is). Without a node, the scheduler picks one whose CPU fits (decision 4). | Matches D8 and S10: nothing moves by itself. Moving VMs between hosts is the cluster operator's job, not the tenant's. |
| 3 | **Transport: a relay over the nodes' mTLS** (settled by the person steering, 2026-11-19). On each side, Cloud Hypervisor talks to a unix socket in the VM's runtime directory. The two control planes join those sockets with one stream between the nodes on the peer connection they already have, `/cluster/v1/migrations/{vm}/stream`. | No new port, firewall rule or certificate: the stream carries the node identities the cluster already checks (C11). Cloud Hypervisor's own TLS would need a certificate per destination name and a port open for each migration. If the relay limits throughput, the next step is Cloud Hypervisor's TCP with several `connections` (§13 O2). |
| 4 | **CPU: the target offers what the VM booted with; no profiles in v1.** Each node reports the CPUID its KVM offers (§9). A VM records the CPUID of the node it booted on. It may migrate to a node whose CPUID is a superset of that one in the leaves Cloud Hypervisor checks. Cloud Hypervisor's own check on the target is the last check; if it refuses, the VM stays on the source. | Profiles are unreleased and experimental (§0 #3). The CPU model doesn't decide: two hosts of one model refused each other one way, because their kernels differ (§0 #7). What decides is what KVM offers, and the check is one-way. A VM keeps its boot CPUID wherever it moves, so it can also go back. |
| 5 | **The commit point is one Raft write**: `status.placement.node` set to the target, after the target reports the VM running. Until then the VM belongs to the source, and the migration record (`status.migration`) says how far it got. | One owner at every moment. A new leader, or a restarted node, finishes or rolls back from the record (§11). |
| 6 | **Disks: the target attaches as hyperstor's delegate while the memory is copied, and takes over the lease at the switch** (hyperstor 11-frontends.md §11.4, its D33). `hyperstor-vhost` logs the guest pages it writes (`LOG_SHMFD`), and lets the requests in flight finish when a ring stops. Until hyperstor has both, a hyperstor VM can't migrate live, and the check says so. | Without the log, the target's memory misses disk reads made during the copy, and the guest's data is silently corrupt (§0 #5). Two writers on a volume must never happen. The delegate is hyperstor's own design for this. |
| 7 | **OVN: a port bound to both hosts during the move.** `requested-chassis=<source>,<target>` with `activation-strategy=rarp`, then the target alone after the commit. | OVN moves traffic when the guest announces itself from the target, not before (14-clustering.md §18). The address and MAC stay, because IPAM is the control plane's (D12). |
| 8 | **Which node acts.** The source's VM controller keeps the VM until the commit. The target runs only the receiving instance, which belongs to the migration. Neither node's hyperstor probe lets go of a migrating VM's disks (S26). | Today the target would see a VM placed elsewhere, and the probe would detach the volume being handed over. |
| 9 | **A failure before the commit cancels.** The VM keeps running on the source, and the target's instance, ports and delegate attachments are removed. A failure after the target is running is finished forwards (§11). | Cloud Hypervisor resumes the source when the VM hasn't moved yet (v54's cancel, and its handling of a failed migration). Once the target runs the guest, the source has stopped it. |
| 10 | **Cloud Hypervisor v54.1 before LM4, with nested virtualization kept on.** The installer's pin moves from v53.0, and `--cpus` gets `nested=on`. In a cluster, both ends of a migration must run the same release, or the target a newer one within two releases. | v54 has `cancel-migration`, migration events, and the n-2 guarantee. Going back to an older release isn't supported upstream. v54 turned nested off by default (§0 #10); keeping it on means the upgrade changes nothing a guest sees. On by default was settled on 2026-11-19 (§13 O5); a per-VM or per-project switch to turn it off can come later. |
| 11 | **A root step at boot reads KVM's CPUID** (O6, settled 2026-11-19). A oneshot run as root with `glidex-vfio-boot.service` calls `KVM_GET_SUPPORTED_CPUID` and writes `/run/glidex/kvm-cpuid.json`; the control plane reads the file for each node report. Without the file the node's CPUID is unknown, and live migration to it is refused; cold moves don't need it. A VM's boot CPUID comes from Cloud Hypervisor's API. | The control plane runs as `glidex`, outside the `kvm` group, and `/dev/kvm` would let it create VMs, which only the shim does. What KVM offers changes only with the kernel or microcode, which needs a reboot, so a file written at boot doesn't go stale. netd is for networking, and the shim only reads it once a VM has run on the node, so an empty node, the obvious target, would report nothing. |

## 3. LM0: the cold move

What is built (24-storage.md §9, S4):
- `place` checks the VM:
  - it is stopped, or Lost;
  - every disk is on a shared datastore;
  - no host-path root, no `vfio_devices`, no `scope: node` network.
- It clears the placement and pins the target.
- It runs a placement pass, which picks the target's pool devices and
  NUMA layout as it would for a new VM, and moves the disks' workers.

The launch on the target:
- rebuilds the cloud-init seed (`controller/vm.rs`, "regenerated on
  every launch");
- creates the NICs' taps through netd;
- preempts a stale `hyperstor-vhost` hold left on the old node (S26).

LM0 adds (**built**):
- **API:** `StartVmRequest.node` (optional; a node id or name), and
  `POST /vms/{id}/start?node=N` over REST.
  - With a node, `start_vm` checks `placeVm` on the VM. Then
    `VmManager::start_on` checks `If-Match` and, unless the VM is
    already placed on `N`, calls `place_vm(N)`. Last, it sets the power
    to running.
  - Nothing changes if any check fails: `403` without `placeVm`, `400`
    with `place`'s reason, `409` when the VM isn't stopped, `412` on a
    stale `If-Match`.
  - If the scheduler finds no room on the node, the VM is placed nowhere
    and waits, as `place` leaves it today. Its power is still set to
    running, so it starts once room is there; the answer shows it
    waiting.
  - If a quota refuses the start, the VM stays placed on `N`, stopped.
- **gxctl:** `start <vm> --node N`.
- **UI:** the "Place on another node" dialog gets **Place and start**,
  enabled once a node is chosen. It is shown, as the dialog is, to those
  with `placeVm`.
- **Docs:** `operator-guide/datastores.md` (placing and starting in one
  step), `clusters.md`, and the REST reference.
- **Tests:**
  - `cluster_tests.rs::a_vm_on_shared_disks_is_placed_again_when_stopped_or_lost`:
    - a stopped VM on shared disks started with a node is placed there,
      its power set to running, and its disk's worker moved with it;
    - one with a node-local disk is refused with `400` and stays stopped
      where it was;
  - `security_tests.rs::starting_on_a_node_takes_place_vm`: a project
    owner, who may start the VM, gets `403` for a start naming any node,
    and the VM stays stopped.
- **Host check (the test cluster, when the person steering agrees):**
  - a VM with a hyperstor root disk and a cluster-network NIC, stopped
    on node-a and started with `--node node-b`;
  - it boots, a file written before the move is intact, and the MAC and
    address are the same;
  - the volume's hold is `vhost:node-b`, and node-a's `hyperstor-vhost`
    has let go of it.

## 4. Live migration, step by step

The migration record `status.migration` holds:
- `target`;
- `phase`;
- `started_at`;
- `updated_at`;
- progress: `bytes`, `iteration`, `dirty_rate`;
- `reason` when it fails.

Phases: `Checking` → `Preparing` → `Copying` → `Switching` →
`Committed`, or `Cancelling` → `Failed`.

1. **Checking (leader).** The checks of §5. The leader reserves the
   target's CPU and memory as the scheduler does for a placement, and
   writes the record.
2. **Preparing (target).** The target node:
   - attaches each disk as a delegate (decision 6);
   - creates the taps and binds the ports with `requested-chassis` naming
     both hosts (§8);
   - writes the cloud-init seed;
   - starts the shim in **receive mode**: `cloud-hypervisor
     --api-socket` with no VM, then `vm.receive-migration
     unix:<vm_dir>/migration.sock`.

   It then reports itself ready in the record.
3. **Copying (source).** The source's control plane opens the stream to
   the target (§6) and calls `vm.send-migration unix:<vm_dir>/migration.sock`
   with `downtime_ms`, `timeout_s` and `timeout_strategy=cancel`.
   Progress is written to the record at most once every 2 s.
4. **Switching.** Cloud Hypervisor pauses the guest, sends the last
   dirty pages and the device state, and resumes the guest on the
   target. hyperstor hands the lease to the delegate. The source's
   Cloud Hypervisor exits.
5. **Committed (leader).** On the target's report that the VM is
   running, one write sets:
   - `placement.node` to the target;
   - the target's instance;
   - the disks' `worker` to the target;
   - `status.migration` cleared;
   - an event `Migrated`.

   Then:
   - the source's netd removes its taps;
   - the ports' `requested-chassis` becomes the target alone;
   - the source's metering ledger closes the VM at the switch time;
   - open consoles reconnect through the relay to the target.

## 5. What can migrate

`gxctl vm migrate <vm> --check [--node N]` and the UI show these checks
without moving anything. The VM:
- runs on Cloud Hypervisor (not QEMU, not Pylon), and is `Running`, not
  paused, with no migration in progress;
- has every disk on a hyperstor datastore whose cluster supports the
  delegate and the dirty log (§7). It has no host-path root and no
  cloud-init seed on a host path;
- has no PCI devices (`vfio_devices` or pool devices), no DPU NICs and no
  vhost-user-net NICs, and no `scope: node` network;
- has a target that is `Ready`, whose CPU fits the VM (§9), with a
  Cloud Hypervisor release the source can send to (decision 10), and
  room for the VM's CPU and memory (hugepages too, when the VM uses them)
  and its NUMA policy.

## 6. The relay

- Each side's Cloud Hypervisor uses `<vm_dir>/migration.sock`. On the
  target it listens there (`receive-migration`). On the source the
  control plane listens there, and Cloud Hypervisor connects to it
  (`send-migration`).
- The source's control plane accepts that connection. It opens an
  HTTP/2 stream to the target, `POST /cluster/v1/migrations/{vm}/stream`,
  through `PeerClient` with the target's node id (C11). It then copies
  bytes both ways.
- The target's control plane accepts the stream only for a VM whose
  migration record names it as the target, in phase `Copying`. It
  connects the stream to its own `migration.sock`.
- The relay never parses Cloud Hypervisor's protocol. It ends when
  either side closes. A broken relay fails the migration, and Cloud
  Hypervisor's own timeouts (120 s on a dead peer) back it up.
- Throughput is measured in LM1 (**verify**): pages of a guest under
  load over the relay, against Cloud Hypervisor's direct TCP between the
  same hosts.

## 7. Disks on hyperstor

The prerequisites are on the hyperstor side, and each has its own issue
in the hyperstor repository:

1. **Dirty logging and draining in `hyperstor-vhost`** (CMKL-APEX/hyperstor#397).
   - Offer `LOG_SHMFD` and implement `set_log_base`.
   - Mark every guest page a request writes, and the used rings, while
     logging is on.
   - When a ring stops with logging on, finish the requests in flight
     before answering `GET_VRING_BASE`, so the target doesn't lose
     them (I161).
2. **The delegate and the handover** (11-frontends.md §11.4, D33; CMKL-APEX/hyperstor#398).
   - The target's `hyperstor-vhost` attaches the volume as the source
     holder's delegate: the socket is open, and it writes nothing.
   - At the switch, the lease passes to it with no blocklist round.
   - If the migration is cancelled, the delegate is dropped, and the
     source keeps the lease.
   - glidex needs this through the vhost control socket (`attach` with a
     delegate-of argument, `promote`, `drop`) and in `list`, so S26's
     probe can tell a delegate from a stale hold.

glidex's side (LM3):
- the driver's `attach_delegate`, `promote` and `release_delegate`;
- the S26 probe skips the disks of a migrating VM;
- the check of §5 asks the datastore whether its cluster supports both
  (a `GetStatus` feature flag; **verify** what hyperstor offers).

## 8. Networking

- **Before the copy.** The target's netd creates the taps (with the same
  names; they are on another host) and plugs them into `br-int` with the
  same `iface-id`. The leader sets each port's `requested-chassis` to
  `<source>,<target>` and `options:activation-strategy=rarp`.
- **After the commit.** The leader sets `requested-chassis` to the
  target alone and clears the strategy. The source's netd removes its
  taps (`detach_vm_port`).
- **The guest's announcement.** Cloud Hypervisor's virtio-net announces
  the guest after resume only when the guest negotiated
  `VIRTIO_NET_F_GUEST_ANNOUNCE`. Without it, OVN's `rarp` activation
  waits for the guest's first packet from the target (**verify** with
  the Ubuntu and Debian images, and an idle guest).
- Port security, address sets and DHCP options are on the logical port,
  so they move with it unchanged.

## 9. CPU compatibility and versions

- **Each node reports** in its status:
  - the CPUID its KVM offers (`KVM_GET_SUPPORTED_CPUID`), as a digest and as the leaves Cloud Hypervisor checks;
  - vendor, family, model, stepping, microcode and the host kernel, to show people;
  - the Cloud Hypervisor release.
- **Each VM records** the CPUID of the node it booted on (`status.boot_cpuid`), at each launch. A migration keeps it, since the guest keeps it.
- **A target fits** when its offered CPUID is a bitwise superset of the VM's boot CPUID in those leaves (Cloud Hypervisor's `BitwiseSubset`), and its release can receive from the source's (decision 10).
- The scheduler's choice for `migrate` without a node, and the check, keep to targets that fit. `gxctl node show` and the host page show the CPU and kernel, and the check names the first leaf that doesn't fit.
- KVM's CPUID is read as root at boot into `/run/glidex/kvm-cpuid.json` (decision 11). A kernel upgrade, which changes what KVM offers, needs a reboot, which writes the file again.
- When CPU profiles are released and lose "experimental", a later milestone can let a cluster pick a profile, so every host offers the same CPUID (§13 O1).

## 10. API, clients and permissions

- **REST:**
  - `POST /vms/{id}/start?node=N` (LM0);
  - `POST /vms/{id}/migrate {node?, downtime_ms?, check?}`;
  - `POST /vms/{id}/migrate/cancel`.
- **RPC:** `VmService.MigrateVm` and `CancelMigration`, and
  `StartVmRequest.node`.
- **gxctl:** `start <vm> --node N`, `migrate <vm> [--node N] [--check]
  [--downtime-ms MS]`, `migrate <vm> --cancel`. It follows progress, as
  `start --wait` does.
- **UI:** "Migrate…" on a running VM's page, for those with
  `migrateVm`: a node list with each node's reason when it can't take
  the VM, the downtime, then progress and a Cancel button.
- **Cedar:** `migrateVm` in `node.drain` and `host-scoped`.
- **Events:** `MigrationStarted`, `Migrated` (with the downtime Cloud
  Hypervisor reports), `MigrationFailed` (with the reason) and
  `MigrationCancelled`.
- **Metering:** the VM's usage moves at the commit. Source and target
  each record their part, with no overlap and no gap.

## 11. Failures

| When | What happens |
|---|---|
| A check fails | Nothing starts. The answer names the reason. |
| Preparing fails (delegate, taps, shim) | The target cleans up. The record goes `Failed`. The VM never stopped. |
| The copy doesn't converge in `timeout_s`, the relay breaks, or someone cancels | `vm.cancel-migration` on the source (decision 10). The guest keeps running there. The target cleans up, and the delegate is dropped. `Failed` or `Cancelled`. |
| The target's Cloud Hypervisor refuses (CPUID, version) | Cloud Hypervisor resumes the source. `Failed` with its reason. |
| The source node dies while Copying | The target's receive fails. The VM is the source's, which is down: it is `Lost` as today, and recovering it needs a fenced forget (S10, S23). The target cleans up. |
| The target dies while Copying | The send fails, and the source resumes the guest. |
| The guest reboots during the copy | On v53 the source's Cloud Hypervisor stops with a fatal error, and the target aborts (§0 #9): the VM is `Failed` on the source, and glidex says why. **Verify** on v54.1. |
| The leader changes | The new leader reads the record and asks the target whether the VM is running there. If it is, the leader commits. If not, it cancels. |
| The target runs the VM, but the commit write fails | The target keeps it, and the leader retries the commit. The source's controller sees a migration in `Switching` and doesn't restart the VM. |

## 12. Milestones

| Milestone | What | Tests and checks |
|---|---|---|
| **LM0** Cold move | §3: `StartVmRequest.node`; `start --node`; the UI's **Place and start**; docs | Cluster tests of §3. Host check: a hyperstor VM from node-a to node-b, stopped (the test cluster). |
| **LM1** Spike and versions | Cloud Hypervisor v54.1 in the installer, with `nested=on` (decision 10). By hand on node-a and node-b: a migration of a VM booted from a kernel and initramfs with no disk, over a unix socket bridged between the hosts and over TCP. | Findings in §0 #7–#10 (spike run 2026-11-19 on v53.0). The functional tests on v54.1. Not yet: the relay against TCP with a guest that writes its memory, a tap NIC, and the spike on v54.1. |
| **LM2** CPU compatibility | `cpu` in node status (KVM's CPUID and the host's CPU and kernel), `boot_cpuid` on VMs, the fit check; the host page and `node show` | Unit tests: the superset check on recorded CPUID. Host: node-b's CPUID fits node-a, and node-a's doesn't fit node-b, as §0 #7 found. |
| **LM3** hyperstor | The driver's delegate calls; S26's probe skips migrating VMs; the feature check | Unit tests with a fake vhost control socket. Needs hyperstor's issues done. |
| **LM4** The migration | `status.migration`, the leader's steps, receive mode in the shim, the relay, OVN two-chassis binding, the commit, cancel and recovery; the API, RPC and gxctl | Cluster tests with fake hypervisors: each phase, cancel, each failure in §11, a leader change mid-way. |
| **LM5** UI, metering, docs | The Migrate dialog, progress, events; metering's split; operator docs | UI e2e. Unit tests of the ledger split. |
| **LM6** Host checks | On the test cluster, node-a ↔ node-b: an idle guest; a guest under memory and disk load (`fio` with verify); a ping across the move (the loss is reported); cancel; the target's control plane killed mid-copy | Results in this spec's status, with the date, the build and pass/fail. |

Dependencies: LM0 needs nothing. LM1 → LM2 → LM4. LM3 needs hyperstor's
issues. LM4 needs LM2 and LM3. LM5 and LM6 need LM4.

## 13. Open questions

- **O1 CPU profiles.** When they ship in a release and lose
  "experimental", should glidex generate a profile for a chosen set
  of hosts (for example, all of a cluster's EPYCs) and place it on every host?
  Decision 4 doesn't use them until then.
- **O2 Direct transport.** If LM1 shows the relay too slow for large
  guests, should Cloud Hypervisor's own TCP with `tls_dir` and several
  `connections` come next?
- **O3 Shared-directory datastores.** Moving the shim's lock file (S16)
  to the target at the switch, and NFS's own behaviour during a move.
- **O4 Drain and evacuation.** Should `node drain` migrate running VMs
  that can migrate, and stop the rest?
- **O5 Nested virtualization.** Settled 2026-11-19: on by default
  (decision 10).
- **O6 Who reads KVM's CPUID (LM2).** Settled 2026-11-19: a root step
  at boot (decision 11).

## 14. Status

| | |
|---|---|
| **Spec \| Code** | LM0 built: `start` with a node (API, RPC, gxctl, the UI's **Place and start**, docs). LM1: the spike run (§0 #7–#10); Cloud Hypervisor pinned at v54.1 with `nested=on`. LM2–LM6 are design. |
| **Tests** | LM0: a cluster test (start on the other node; a node-local disk refused) and a security test (`placeVm` needed). LM1: on node-b with v54.1 (2026-11-19), the Cloud Hypervisor functional boot tests passed: firmware and cloud-init, catalog image with root growth, extracted kernel, guest kernel updates, NAT network, mixed hypervisors on one network, and a VM surviving a control-plane restart. |
| **Not yet** | LM0's host check on the test cluster (not run yet). LM1: the spike on v54.1, a guest that writes its memory, a tap NIC; `numa_placement_e2e` and the QEMU, vhost-user, LVM and hyperstor functional tests on v54.1 (not run: they need GPUs, OVS-DPDK, a volume group or a hyperstor token). The nested e2e on v54.1 (not run). LM2–LM6. hyperstor's dirty log and delegate (CMKL-APEX/hyperstor#397, #398). |
