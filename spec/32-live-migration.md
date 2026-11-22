# Migration: starting a stopped VM on another host, then moving a running one

**Status: LM0–LM3 and LM4 tasks 1–3 built; the rest is design** (tracked in #573). LM0, the cold move, comes first: it needs only
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
| 11 | **Cloud Hypervisor v55.0** (2026-11-19), read for the move from v54.1: it compares four more CPUID registers on a migration's target, all AMD leaves, and KVM's values are what reach the guest for each (no `required_common_cpuid_updates` patch). 0x8000_0008 EBX and 0x8000_0021 ECX are subsets (the EBX one ignoring the obsolete `EFER_LMSLE_UNSUPPORTED`, bit 20). 0x8000_000a EDX, SVM, is a subset only for a VM with nested virtualization, which every glidex VM has (decision 10). 0x8000_0021 EAX has its own rule: bits 4, 6, 16, 25 and 26 equal; bits 1 and 9 (defeatures) on the target only if the VM had them; bit 14 ignored; the rest a subset. Its release notes say these "may reject migrations that appeared to work before". It also needs host kernel 6.8 or later (was 5.13); fixes live migration hanging on an unresponsive peer; lets `receive-migration` change the serial and console outputs; takes replacement descriptors as `external_fds`; ships CPU profiles from a file (`--cpus profile=path:`), still experimental; and deprecates legacy VFIO, ACPI memory hotplug and `--no-shutdown`, none of which glidex passes but legacy VFIO for passthrough. It removes `--platform serial_number` and `uuid`, `--memory file` and vhost-user `virtio_id`, which glidex doesn't pass. `vm.counters` gains a `device_type` string, which glidex's parser ignores. | Pin v55.0 (decision 12). The fit check takes v55's list. The test cluster's hosts are AMD EPYC on kernels 6.8 and 6.17, so whether their KVMs differ in leaf 0x8000_0021 is checked from the nodes' reports after the rollout. |

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
| 11 | **A root step at boot reads KVM's CPUID** (O6, settled 2026-11-19). `glidex-kvm-cpuid.service`, a oneshot run as root before the control plane (and again by each install), runs `glidex-vm-shim --kvm-cpuid`: it calls `KVM_GET_SUPPORTED_CPUID` and writes `/run/glidex/kvm-cpuid.json`, with the host's CPU and kernel. The control plane reads the file for each node report, and at each launch records the same registers as the VM's `boot_cpuid`. Without the file the node's CPU is unknown, and live migration to it is refused; cold moves don't need it. | The control plane runs as `glidex`, outside the `kvm` group, and `/dev/kvm` would let it create VMs, which only the shim does. What KVM offers changes only with the kernel or microcode, which needs a reboot, so a file written at boot doesn't go stale. Its own unit, not `glidex-vfio-boot.service`, which exists only on hosts with boot devices. The shim already holds the KVM code; netd is for networking. |
| 12 | **Cloud Hypervisor v55.0** (asked by the person steering, 2026-11-22). The installer's pin moves from v54.1, with `nested=on` kept (decision 10). The fit check compares v55's 32 registers (§0 #11, §9). | Before live migration ships, so the first release that migrates runs the release that fixes hangs on an unresponsive peer and checks AMD's leaves. A v55 target receives from v54 and v53 (n-2), so a cluster is upgraded host by host and migrates from a not-yet-upgraded node to an upgraded one, not back. Host kernels must be 6.8 or later. |
| 13 | **Each step is written into the record by the node that did it** (§4). The leader writes the start, the target's reserved placement, the commit, and the cancels and failures it decides. The source writes its delegates, that it sent the guest, and its own release. The target writes that it is ready, that it received the guest, and its own release. A node may write a VM's `status.migration`, and nothing else of the VM, when the record names it as the target. The leader checks that each such write moves the record only as §4 allows. | A node acts on what only it can see: its `hyperstor-vhost`, its netd and its Cloud Hypervisor. The record is all a new leader has (decision 5), so each step's result must be in it. Only the leader writes placement, so only the leader commits. |
| 14 | **The target's guest decides the outcome, not the source's send.** Cloud Hypervisor v55's `send-migration` answers once the copy has started, and reports the result only in its events. The target's `receive-migration` answers when the guest runs there, or fails. So the migration has succeeded when the target reports its instance running (`received`). On the source, Cloud Hypervisor exiting after the send is its own sign that the guest has left. Once either is seen, the migration finishes forwards, even if a cancel was asked for. | glidex already polls every instance's shim, the target's included. Reading Cloud Hypervisor's event monitor on the source would be a second channel for the same answer. |
| 15 | **Release order on each side.** On success, the target promotes each disk before it reports `received`, and the source detaches its disks only after it sees `received`. This is hyperstor's tested order: a promote, then the old holder's detach, fences nothing of the new holder's. On a cancel, the target detaches its standby disks (`detach --force`) before the source drops its delegates (`undelegate`), also as tested. | Two writers on a volume must never happen (decision 6). These are the orders the handover was checked in (the test cluster, 2026-11-20). |
| 16 | **A migrating VM can't be stopped, restarted, re-placed or deleted.** Until the record is gone or `Failed`, the API answers `409` ("migrating; cancel it first"). Settled by the person steering, 2026-11-22. | Each would change what the source or the target is doing mid-step. A refusal changes nothing behind the caller's back, and cancelling is one call. |
| 17 | **In v1, a VM with dedicated CPUs or a NUMA policy doesn't migrate** (settled 2026-11-22). The check gives the reason. A VM on the shared CPU pool migrates; after the commit, the target moves its threads onto the target's pool, as it does when the pool changes (`cpu_drift`). | Cloud Hypervisor's received configuration keeps the source's CPU affinity and memory binding. Those host CPUs and NUMA nodes may not be free, or may not exist, on the target. Pinned VMs need the target's own CPUs and NUMA nodes in the received configuration: `receive-migration`'s `zone_updates` for memory, and an affinity Cloud Hypervisor can't yet replace. That is a later milestone. |
| 18 | **Time limits.** `send-migration` gets `timeout_s` 900 with `timeout_strategy: cancel`, and the request's `downtime_ms` (300 by default). The leader cancels a migration still `Preparing` after 120 s, and fails one still `Cancelling` after 120 s. Before the guest has moved, a target that isn't `Ready` for 30 s cancels the migration, and a source that isn't fails it. | A migration holds the target's CPU and memory, and the VM can't be stopped meanwhile (decision 16), so every phase must end. 900 s copies about 100 GiB at the test cluster's 1 Gb/s. |

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
- `target`, `phase`, `started_at`, `updated_at`;
- `downtime_ms`, from the request;
- `placement`: the target's placement, reserved by the leader. The
  scheduler counts it on the target while the migration is in progress;
- `delegates`: each hyperstor disk's delegate id, from the source;
- `received`: the target's instance, once the guest runs there;
- `sent`: the source's Cloud Hypervisor has exited after the send;
- `source_released` and `target_released`: each side has let go of its
  part;
- progress: `bytes`, `iteration`, `dirty_rate` (not filled in v1:
  Cloud Hypervisor v55 reports them only as events);
- `reason`, when it fails or is cancelled.

Phases: `Checking` → `Preparing` → `Copying` → `Switching` →
`Committed`, or `Cancelling` → `Failed`. `Checking` and `Committed` are
never stored: the check runs inside the request, and the commit clears
the record. A migration is **in progress** while its phase is
`Preparing`, `Copying`, `Switching` or `Cancelling`. A `Failed` record
stays, with its reason, until the next migration replaces it.

Who writes what (decision 13):

| Step | Node | Writes |
|---|---|---|
| Start | Leader | the record: `Preparing`, `target`, `placement`; event `MigrationStarted` |
| Delegates | Source | `delegates` |
| Ready | Target | `Copying` |
| Sent | Source | `sent`, and `Switching` |
| Received | Target | `received` |
| Source released | Source | `source_released` |
| Commit | Leader | the VM's placement and instance; the record cleared; event `Migrated` |
| Cancel | Leader, source or target | `Cancelling` and `reason` |
| Target released | Target | `target_released` |
| Failed | Source, or leader | `Failed`; event `MigrationFailed` or `MigrationCancelled` |

1. **Checking (leader).** The checks of §5. The leader picks the
   target's placement as the scheduler does for a new VM, and writes the
   record in `Preparing`.
2. **Preparing.**
   - The source makes each hyperstor disk's delegate (`delegate`) and
     writes their ids.
   - Then the target:
     - attaches each disk as a delegate, standing by (decision 6);
     - creates the taps (§8);
     - writes the cloud-init seed;
     - starts the shim in **receive mode**: `cloud-hypervisor
       --api-socket` with no VM, then `vm.receive-migration
       unix:<vm_dir>/migration.sock`.

     It then writes `Copying`. A step that fails writes `Cancelling`
     with its reason.
   - Meanwhile, the leader binds each port to both hosts (§8).
3. **Copying (source).** The source's control plane binds
   `<vm_dir>/migration.sock` and relays it to the target (§6). It calls
   `vm.send-migration unix:<vm_dir>/migration.sock` with `downtime_ms`,
   `timeout_s` and `timeout_strategy` (decision 18). Then it waits for
   one of these:
   - its Cloud Hypervisor exits: the guest has left, so it writes `sent`
     and `Switching`;
   - the relay ends and Cloud Hypervisor still runs the guest: the send
     failed, so it writes `Cancelling` with the reason.
4. **Switching.** Cloud Hypervisor pauses the guest, sends the last
   dirty pages and the device state, and resumes the guest on the
   target. The target's receive answers, and its shim reports the
   instance running. The target then promotes each disk (decision 15)
   and writes `received`. When the source sees `received`, it:
   - detaches its disks;
   - removes its taps;
   - releases its shim's instance;
   - writes `source_released`.
5. **Committed (leader).** When the record has `received` and
   `source_released`, one write:
   - sets `placement` to the record's (the target's);
   - sets the instance to the target's;
   - binds the disks to the target, as a placement does;
   - pins the VM to the target, if it was pinned (`spec.node`), as
     `place` does (LM0);
   - clears `status.migration`;
   - records an event `Migrated`.

   The target's VM controller then owns the VM. It finds its own
   instance running, and puts the threads on its CPU pool (decision 17).
   The leader binds the ports to the target alone (§8).

If the source is down for 30 s after `received`, the leader commits
without `source_released`. Its hyperstor hold is let go by S26 when the
source comes back.

**Cancelling.** Any node may write `Cancelling` while the guest hasn't
moved: the leader on `POST /vms/{id}/migrate/cancel` or a time limit,
the target when preparing fails, the source when the send fails. Then:
- the source asks its Cloud Hypervisor to cancel the send
  (`vm.cancel-migration`), if one is running;
- the target stops its receiving shim, detaches its standby disks
  (`detach --force`), removes its taps and seed, and writes
  `target_released`;
- the source then drops each delegate (`undelegate`), and writes
  `Failed` with the reason (decision 15). It doesn't wait for the
  target's release when the target hasn't been `Ready` for 30 s, and it
  drops any delegates left when it finds the record `Failed`.

If the source's Cloud Hypervisor has exited (`sent`), or the target
has `received`, the migration doesn't cancel: it finishes forwards
(decision 14).

**The leader's loop** runs every 2 s over the VMs with a migration in
progress:
- it commits a record with `received` and `source_released`, as in step 5;
- it cancels a migration still `Preparing` after 120 s (decision 18);
- before the guest has moved, it cancels a migration whose target hasn't
  been `Ready` for 30 s, and fails one whose source hasn't, since no
  one else can then cancel the send;
- it fails a migration still `Cancelling` after 120 s.

A new leader runs the same loop from the records, so nothing is kept
only in a leader's memory.

**Both nodes' controllers.**
- The source's VM controller treats the VM as usual while it is
  `Preparing` or `Copying`. From `Switching` on, it doesn't restart it
  or record its exit as a crash: the guest runs elsewhere.
- The target runs the receiving instance outside its VM controller
  (decision 8), from the record, which an agent receives (§6).
- Each node's netd keeps a migrating VM's taps after a netd restart, on
  both nodes.
- Neither node's hyperstor probe lets go of the disks of a VM with a
  migration in progress (S26).

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
- has no dedicated CPUs and no NUMA policy (decision 17);
- has a target that is `Ready`, whose CPU fits the VM (§9), with a
  Cloud Hypervisor release the source can send to (decision 10), and
  room for the VM's CPU and memory (hugepages too, when the VM uses them).

## 6. The relay

- Each side's Cloud Hypervisor uses `<vm_dir>/migration.sock`. On the
  target it listens there (`receive-migration`). On the source the
  control plane listens there, and Cloud Hypervisor connects to it
  (`send-migration`).
- The source's control plane accepts that connection. It opens a stream
  to the target, `GET /cluster/v1/migrations/{vm}/stream` upgraded to a
  raw byte stream (`Upgrade: glidex-stream`, as the console relay is),
  through `PeerClient` with the target's node id (C11). It then copies
  bytes both ways.
- The target's control plane accepts the stream only from the VM's node,
  for a VM whose migration record names it as the target, in phase
  `Copying` (`409 not_migrating` otherwise). It connects to its own
  `migration.sock` before answering, so a target where nothing receives
  answers `503 not_receiving`.
- An agent receives a VM's record while the VM is placed on it or its
  migration names it as the target (with the credential the VM names),
  so a target agent has the record to prepare from and to check the
  stream against. A failed migration's last write the target sees says
  `Failed`; its copy goes at its next list.
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

Both are merged in hyperstor (CMKL-APEX/hyperstor#399 and #400, its
D349–D352), in `hyperstor-vhost`'s control-socket protocol 3. Verified:
there is no cluster-wide feature flag. `hello` answers `"features":
["live_migration"]` for the node's `hyperstor-vhost`, and whether the
storage nodes can hand a lease over is checked by the target's standby
`attach`, which an older cluster refuses (`invalid`) before any memory
is copied.

glidex's side (LM3):
- the driver's `delegate` (source), `attach_delegate` (target, standing
  by), `promote` (target, at the switch; answers the leases being
  fenced), `release_delegate` (source, on cancel) and `detach_forced`
  (target, on cancel, its receiving hypervisor connected);
- the S26 probe lets go only of holds: a disk `list`ed with `"role":
  "delegate"` (a target standing by) is left alone. From the promotion
  until the commit the target holds a disk whose VM is still placed on
  the source, so the probe also skips the disks of a VM with a
  migration in progress (§4);
- the guest resumes on the target before glidex sees the receive end
  and promotes the disks (§4, step 4), so for that moment its disk
  requests reach a volume still standing by. hyperstor holds them until
  the promotion, or the guest sees errors (**verify** in LM6, and ask
  hyperstor if they fail);
- each node reports, per hyperstor datastore, `live_migration` from its
  `hyperstor-vhost`'s `hello` (the datastore's node rows), and the check
  of §5 wants it on the source and the target.

## 8. Networking

- **Before the copy.** The target's netd creates the taps (with the same
  names; they are on another host) and plugs them into `br-int` with the
  same `iface-id`. While a migration is in progress, the leader sets each
  port's `requested-chassis` to `<source>,<target>` and
  `options:activation-strategy=rarp`.
- **After the guest moves.** The source's netd removes its taps
  (`detach_vm_port`) when the source releases (§4, step 4). At the
  commit, the leader sets `requested-chassis` to the target alone and
  clears the strategy. After a cancel, it goes back to the source alone.
- **The guest's announcement.** Cloud Hypervisor's virtio-net announces
  the guest after resume only when the guest negotiated
  `VIRTIO_NET_F_GUEST_ANNOUNCE`. Without it, OVN's `rarp` activation
  waits for the guest's first packet from the target (**verify** with
  the Ubuntu and Debian images, and an idle guest).
- Port security, address sets and DHCP options are on the logical port,
  so they move with it unchanged.

## 9. CPU compatibility and versions

- **Each node reports** in its status (`cpu`):
  - the 32 CPUID registers Cloud Hypervisor v55.0 compares on a migration's target (`check_cpuid_compatibility`: its `checked_feature_entry_list` and AMD's leaf 0x8000_0021 EAX), from what its KVM offers (`KVM_GET_SUPPORTED_CPUID`);
  - vendor, family, model, stepping, microcode and the host kernel, to show people.
- **Each VM records** the same registers from its node at each launch (`status.boot_cpuid`). A migration keeps it, since the guest keeps it.
- **A target fits** when, register by register, it passes Cloud Hypervisor's comparison against the VM's boot CPUID: most are a bitwise subset, the highest subleaves are compared as numbers, KVM's signature must be equal, and AMD's leaf 0x8000_0021 EAX has its own rule (§0 #11). A register the VM's record lacks, recorded at a launch before glidex knew v55's list, isn't compared: Cloud Hypervisor's own check, which sees the VM's CPUID, decides it. Its release must also receive from the source's (decision 10). This compares what each KVM offers, not the CPUID Cloud Hypervisor then gives the guest; Cloud Hypervisor adjusts both the same way from the same configuration, and its own check on the target stays the last word. The exception is the bits Cloud Hypervisor sets in every guest whatever KVM offers: leaf `0x1` ECX HYPERVISOR and TSC_DEADLINE_TIMER, and EDX MTRR. Those count as set on both sides. Kernel 6.8's KVM doesn't list the two ECX bits and 6.17's does, so comparing them would refuse a 6.17-booted VM a 6.8 target that takes it (#585).
- The scheduler's choice for `migrate` without a node, and the check, keep to targets that fit. `gxctl node <id>` and the host page show the CPU and kernel, and the check names the first register that doesn't fit, and the bits the target lacks.
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
- **The record** is `status.migration` in the VM (REST and RPC
  `Vm.status.migration`): the target, the phase, its times and the
  reason. The node-to-node fields of §4 (delegates, the reserved
  placement, the instances) aren't in the API.
- **While a migration is in progress**, `stop`, `restart`, `place`,
  `start --node` and `delete` answer `409 migrating` (decision 16), and a
  second `migrate` answers `409` too. `migrate/cancel` on a VM with no
  migration in progress answers `409 not_migrating`.
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
| Preparing fails (delegate, taps, shim) | `Cancelling` with the reason. The target cleans up, the source drops the delegates, and the record goes `Failed`. The VM never stopped. |
| Preparing takes over 120 s | The leader writes `Cancelling`, and the same follows. |
| The copy doesn't converge in `timeout_s`, the relay breaks, or someone cancels | `vm.cancel-migration` on the source (decision 10). The guest keeps running there. The target cleans up, and the delegate is dropped. `Failed`, with event `MigrationCancelled` for a cancel asked for, `MigrationFailed` otherwise. |
| A cancel after the guest has moved | Ignored: the migration finishes forwards (decision 14). |
| The target's Cloud Hypervisor refuses (CPUID, version) | Cloud Hypervisor resumes the source. `Failed` with its reason. |
| The source node dies while Copying | The target's receive fails. The VM is the source's, which is down: it is `Lost` as today, and recovering it needs a fenced forget (S10, S23). After 30 s the leader writes `Failed`, and the target cleans up. |
| The source node dies after the target has `received` | After 30 s the leader commits without `source_released` (§4). |
| The target dies while Copying | The send fails, and the source resumes the guest. After 30 s the leader writes `Cancelling`; the source drops its delegates and writes `Failed`. The target cleans up when it comes back and reads the record. |
| The guest reboots during the copy | On v53 the source's Cloud Hypervisor stops with a fatal error, and the target aborts (§0 #9): the VM is `Failed` on the source, and glidex says why. **Verify** on v54.1. |
| The leader changes | The new leader runs the same loop from the records (§4). A target running the guest has written `received`, so the leader commits; otherwise each time limit still holds. |
| The target runs the VM, but the commit write fails | The target keeps it, and the leader retries the commit on its next round. The source's controller sees a migration in `Switching` and doesn't restart the VM. |
| A node restarts mid-way | It reads the record and carries on from its phase: each step checks what is already done (a delegate made, a disk attached, a shim running) before doing it. |

## 12. Milestones

| Milestone | What | Tests and checks |
|---|---|---|
| **LM0** Cold move | §3: `StartVmRequest.node`; `start --node`; the UI's **Place and start**; docs | Cluster tests of §3. Host check: a hyperstor VM from node-a to node-b, stopped (the test cluster). |
| **LM1** Spike and versions | Cloud Hypervisor v54.1 in the installer, with `nested=on` (decision 10). By hand on node-a and node-b: a migration of a VM booted from a kernel and initramfs with no disk, over a unix socket bridged between the hosts and over TCP. | Findings in §0 #7–#10 (spike run 2026-11-19 on v53.0). The functional tests on v54.1. Not yet: the relay against TCP with a guest that writes its memory, a tap NIC, and the spike on v54.1. |
| **LM2** CPU compatibility | `cpu` in node status (KVM's CPUID and the host's CPU and kernel), `boot_cpuid` on VMs, the fit check; the host page and `node show` | Unit tests: the superset check on recorded CPUID. Host: node-b's CPUID fits node-a, and node-a's doesn't fit node-b, as §0 #7 found. |
| **LM3** hyperstor | The driver's delegate calls; S26's probe leaves a target standing by alone; the feature check (§7) | Unit tests with a fake vhost control socket. hyperstor's issues are done (#399, #400). |
| **LM4** The migration | `status.migration`, the leader's steps, receive mode in the shim, the relay, OVN two-chassis binding, the commit, cancel and recovery; the API, RPC and gxctl | Cluster tests: the record's steps and who may write them, cancel, each time limit, a leader change mid-way; a migration end to end with a fake Cloud Hypervisor. |
| **LM5** UI, metering, docs | The Migrate dialog, progress, events; metering's split; operator docs | UI e2e. Unit tests of the ledger split. |
| **LM6** Host checks | On the test cluster, node-a ↔ node-b: an idle guest; a guest under memory and disk load (`fio` with verify); a ping across the move (the loss is reported); cancel; the target's control plane killed mid-copy | Results in this spec's status, with the date, the build and pass/fail. |

LM4's tasks, one PR each:
1. **The checks**: `MigrateVm` with `check` (§5), the Cedar action `migrateVm`, `gxctl migrate --check`. Without `check` the API answers `501` until task 6.
2. **Receive mode**: the shim starts Cloud Hypervisor with no VM and calls `vm.receive-migration` (`launch.json` `receive`, phase `receiving`, exit cause `migration_failed`; 12-reconciliation.md §8.1). Also the Cloud Hypervisor client's call, the receiving command line and the VM's `migration.sock`. The target's preparing and starting that instance, which belongs to the migration and not the VM's controller (decision 8), is task 5.
3. **The relay** (§6): the stream endpoint and both ends.
4. **The record and the leader** (§4): the record's fields, `status.migration` in the API, and the target's writes to it, checked by the leader (decision 13). The leader's start (the reserved target placement, counted by the scheduler), its loop (the commit, the time limits, the dead nodes), and `migrate/cancel` (REST, RPC `CancelMigration`, `gxctl migrate --cancel`). OVN's two-chassis binding (§8). The `409` while migrating (decision 16), the check for dedicated CPUs and NUMA (decision 17), and S26 and netd keeping a migrating VM's disks and taps. `migrate` without `check` still answers `501`, because no node acts on the record yet.
5. **The target's steps**: preparing (delegate attachments, taps, seed, the receiving shim), `Copying`, the promotion and `received`, and its cleanup on a cancel or failure (`target_released`).
6. **The source's steps and the whole migration**: the delegates, the relay and `send-migration` (the Cloud Hypervisor client's `send_migration` and `cancel_migration`), `sent` and `Switching`, the source's release; its VM controller leaving a sent VM alone; `migrate` without `check`; and a migration end to end in a cluster test with a fake Cloud Hypervisor.

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
| **Spec \| Code** | LM0 built: `start` with a node (API, RPC, gxctl, the UI's **Place and start**, docs). LM1: the spike run (§0 #7–#10); Cloud Hypervisor pinned at v54.1 with `nested=on`. LM2 built: `glidex-kvm-cpuid.service` and `glidex-vm-shim --kvm-cpuid` (decision 11); `cpu` in node status (REST, RPC `NodeCpu`, `gxctl node <id>`, the host page's CPU and Kernel rows); `boot_cpuid` on VMs at each launch; the fit check (`node::cpu_fits`), which LM4 will call. LM4 task 1 built: `POST /vms/{id}/migrate {check: true}` (RPC `MigrateVm`, Cedar `migrateVm` in `node.drain` and `host-scoped`, `gxctl migrate --check`), with every check of §5 but the Cloud Hypervisor release; without `check` it answers `501`. Cloud Hypervisor v55.0 pinned (decision 12), and the fit check on v55's list. LM4 task 5 built (§4): the target's steps, each node's loop over the records naming it (`controller/migration_target.rs`): it waits for the source's delegates, attaches each disk standing by, plugs the taps, writes the seed and starts a shim in receive mode, then writes `Copying`; when the guest runs there it promotes the disks and writes `received` (also during a cancel, decision 14); a failed preparation or receive before the guest moved writes `Cancelling`; after a cancel or failure it stops the receiving shim, detaches the standby disks, removes the taps and its files, and writes `target_released`. LM4 task 4 built (§4): the record's fields, `migration` in the API (REST and RPC `Vm.migration`, `gxctl get`), the leader's start (`VmManager::start_migration`, with the target's placement reserved and counted by the scheduler; not yet served: `migrate` without `check` still answers `501`), its loop (commit, time limits, dead nodes), `migrate/cancel` (RPC `CancelMigration`, `gxctl migrate --cancel`), each node's writes checked by the leader (decision 13), OVN's two-chassis binding with `activation-strategy=rarp` (§8), the `409 migrating` (decision 16), the check's dedicated-CPU and NUMA reasons (decision 17), and S26 and netd keeping a migrating VM's disks and taps. LM4 task 3 built (§6): `status.migration`, the relay endpoint on the target and `VmManager::relay_migration` on the source, and agents receiving the VMs migrating to them. LM4 task 2 built: the shim's receive mode (`launch.json` `receive`; instance phase `receiving`; exit cause `migration_failed`, RPC `EXIT_CAUSE_MIGRATION_FAILED`), `ChClient::receive_migration`, `CloudHypervisorDriver::receive_args` and the VM's `migration.sock`. LM3 built (§7): the hyperstor driver's `delegate`, `attach_delegate`, `promote`, `release_delegate` and `detach_forced`; `held_volumes` for S26, which leaves a delegate alone; `live_migration` in a hyperstor datastore's node rows (REST, RPC `DatastoreNode.live_migration`), from `hello`, which the migration check wants on both ends. LM4 task 6 (§4), LM5 and LM6 are design. |
| **Tests** | LM0: a cluster test (start on the other node; a node-local disk refused) and a security test (`placeVm` needed). LM1: on node-b with v54.1 (2026-11-19), the Cloud Hypervisor functional boot tests passed: firmware and cloud-init, catalog image with root growth, extracted kernel, guest kernel updates, NAT network, mixed hypervisors on one network, and a VM surviving a control-plane restart. LM4 task 5: a cluster test (a server, the target agent, and a source agent whose controllers don't run; a stand-in receiving Cloud Hypervisor run by the real shim): the target starts receiving and writes `Copying`; the guest arriving gives `received` with the shim's instance, the commit, and the target's controller keeping that instance; a cancel stops the receiving shim, removes its launch file and gives `target_released`. A unit test of the target's step for each phase and receiver. LM4 task 4: a cluster test (a server and an agent: the record in the API, `409 migrating` for stop, place and delete, the target's steps written through the leader and its others refused, the commit once the target has the guest and the source has let go, with the VM placed and pinned on the target and `Migrated`; a cancel to `Failed`, with `MigrationCancelled`, after which the VM stops); unit tests of the leader's steps (each time limit and dead node, the commit's two conditions, nothing after the guest moved), of what the source and the target may write, of a migrating port's two chassis and of `lsp-set-options` with `rarp`. LM2: unit tests of each comparison, of one-way compatibility with the spike's registers, of the bits Cloud Hypervisor sets itself not being compared (#585, with node-a's and node-b's leaf 0x1), of the file and of `cpu_fits`. `glidex-vm-shim --kvm-cpuid` run by hand on node-b: 59 entries, leaf 0x7 EDX `0xa8000000`, as the spike found. LM4 task 1: a cluster test (a stopped VM on a shared-directory disk: its reasons, each node's, one node by name, `501` without `check`, nothing moved) and a security test (a project owner is refused `migrate`, check or not). Cloud Hypervisor v55.0: on node-b (2026-11-22) the same functional boot tests as v54.1's passed (seven; `numa_placement_e2e` needs two GPUs and didn't run); unit tests of the four new registers' comparisons, with Cloud Hypervisor's own Zen 4 and Zen 5 values of leaf 0x8000_0021 EAX (Zen 4 fits Zen 5, not back), and of a VM recorded under the older list. LM4 task 3: a cluster test (a server and an agent: bytes relayed both ways between a stand-in sender and receiver, counted; refused when the VM isn't migrating or isn't copying; `503` when nothing receives) and unit tests of who may relay and of an agent receiving a VM migrating to it. LM4 task 2: the shim against a fake Cloud Hypervisor (`receiving`, then `running` when the receive succeeds; a refused receive stops the VMM with `migration_failed` and Cloud Hypervisor's error; a stop while receiving ends it at once); unit tests of the client's request, the launch file's `receive` and the receiving command line. LM3: unit tests against a fake `hyperstor-vhost` control socket (each call's request, the standby not a hold, the probe's `live_migration`) and of `hello`'s feature. |
| **Not yet** | LM4 task 5: the target's hyperstor and netd calls aren't exercised by a test (the test's VM has no disks or NICs); they are task 6's and LM6's. A guest the source has sent (`sent`) whose receive then fails has no step yet: nothing runs it, and the record waits in `Switching` (task 6 decides it, with §11's guest-reboot row). LM4 task 4: the scheduler's reservation and the S26 and netd changes have no test of their own yet. LM0's host check on the test cluster (not run yet). Cloud Hypervisor v55.0: `numa_placement_e2e` and the vhost-user, LVM and hyperstor functional tests (not run: they need GPUs, OVS-DPDK, a volume group or a hyperstor token), and the nested e2e (not run); after the rollout, whether the test cluster's KVMs differ in the AMD leaves v55 compares (§0 #11). LM1: the spike on v54.1, a guest that writes its memory, a tap NIC; `numa_placement_e2e` and the QEMU, vhost-user, LVM and hyperstor functional tests on v54.1 (not run: they need GPUs, OVS-DPDK, a volume group or a hyperstor token). The nested e2e on v54.1 (not run). LM2: the Cloud Hypervisor release in node status; the host check, node-b's CPUID fitting node-a and not the reverse, from the nodes' reports (not run yet: it needs a rollout). LM4's check of the Cloud Hypervisor release; LM3: the handover against a real `hyperstor-vhost` (needs a hyperstor release with #399 and #400 on the test cluster; LM6). LM4 task 2 against a real Cloud Hypervisor (it needs a source, task 3's relay or a socket by hand); what a received VM's configuration carries from the source: its paths are the same on every node, and its CPU affinity is the source's, which decision 17 handles; its `core_scheduling` is the source's too, so a target whose `sched_ext` state differs may refuse the guest, which cancels the migration (verify in LM6). The relay's throughput against Cloud Hypervisor's direct TCP (§6, LM6). LM4 task 4, LM5, LM6. |
