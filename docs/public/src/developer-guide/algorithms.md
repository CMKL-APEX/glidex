# Algorithms

This page explains how the non-trivial parts of Glidex work, as the code
implements them. Each section covers the problem, the inputs and outputs,
the steps, the invariants, where the code lives, and the tests that pin it.
Section numbers in links point into the design specs, which hold the
reasoning. Where a spec and this page disagree, the code is what this page
describes.

For how the pieces fit together, see [Architecture](architecture.md). For
why they were built this way, see
[Design decisions and trade-offs](design-decisions.md).

Paths are relative to `crates/glidex-control-plane/src/` unless another
crate is named.

- [Scheduler](#scheduler)
- [VM reconciliation](#vm-reconciliation)
- [Replicated store and Raft](#replicated-store-and-raft)
- [Metering](#metering)
- [Images and disks](#images-and-disks)
- [Kernel extraction](#kernel-extraction)
- [Disk move and volume locks](#disk-move-and-volume-locks)
- [Directory pull sync](#directory-pull-sync)
- [Console log replay filter](#console-log-replay-filter)
- [IPAM](#ipam)
- [OVN northbound sync](#ovn-northbound-sync)
- [Rate limiting](#rate-limiting)
- [CA rotation and certificate renewal](#ca-rotation-and-certificate-renewal)

## Scheduler

**Problem.** Choose the node a VM runs on. The choice includes the node's
NUMA node, dedicated CPUs, hugepage size and pool devices. A VM is placed
once and stays where it is placed (spec decision D6). Nothing moves it
automatically afterwards.

**Inputs.**
- A `Request`: vCPUs, memory, hypervisor, architecture, hugepages, NUMA and
  CPU policy, pin, disks and the nodes they are bound to, datastore
  reachability, networks, physnets, VFIO devices, pool requests, and the
  cluster's ovn-northd version.
- Every node, with the `Load` already placed on it. A placed VM counts
  whether it is running or not.

**Output.** A `Choice` (node, `page_kib`, NUMA node or NUMA shares, CPUs,
pool devices), or a list of why each node refused.

**Load.** `loads()` folds every placed VM into a per-node `Load`:
- vCPUs;
- ordinary memory and hugepage memory, kept apart and keyed by page size;
- memory bound to each NUMA node;
- dedicated CPUs held;
- claimed PCI devices (address → VM name).

Claims are not stored separately. They are recomputed from placements each
time.

**Filters.** `failing()` checks one node, in this order. The first failure
is the node's reason.

```text
pin matches (id or name)
phase Active, not draining (unschedulable), Ready=True
shared vCPUs + req.vcpus <= (allocatable cpus - held dedicated) * cpu_overcommit
page size: page_kib()            # explicit hugepages must fit; vhost-user may get 2M pages
ordinary memory <= allocatable * memory_overcommit   # hugepage RAM not counted here
hypervisor installed
architecture matches (empty on either side matches)
every physnet mapped on the node
ovn-controller major.minor >= cluster ovn-northd       # cluster networks only
br-int on the netdev datapath                          # vhost-user NIC on a cluster network
each VFIO device: present, not cordoned, not taken (address or IOMMU group)
devices granted to the project on this node
node-local disks unbound or bound here
every datastore of an unbound disk reachable from here
node-scoped networks are on this node
place_on(): pool devices, then NUMA fit
memory recheck if implicit hugepages fell back to ordinary memory
dedicated CPUs leave enough shared capacity
```

`allocatable` is the node's capacity minus a reserve (1 CPU and 2048 MiB by
default) minus the hugepage pool (`node.rs`). The overcommit defaults are
`cpu_overcommit = 4.0` and `memory_overcommit = 1.0`. Hugepages are never
overcommitted (`cluster/config.rs`, `SchedulerConfig`).

**Device claims.** `device_taken()` refuses a device in two cases: it is
claimed, or another device in its IOMMU group is claimed. Devices in one
group cannot be split between VMs. A node that reports no groups is checked
by address only.

**Hugepages.** `page_kib()` works as follows:
- **Asked-for hugepages:** the node's default size, or the requested size.
  The memory must be a whole number of pages, and `taken + mem <= capacity`.
- **A vhost-user NIC without hugepages asked for:** 2 MiB pages, if spare
  pages cover the memory plus max(memory/4, 256 MiB). Otherwise ordinary
  memory.

**NUMA fit.** `numa_fit()` applies after the filters:
- Dedicated CPUs without a NUMA policy imply `preferred`.
- `aligned` without devices behaves as `preferred`.
- No policy and shared CPUs: no NUMA work is done.

```text
candidates = NUMA nodes (strict: only those holding the VM's devices)
order candidates by (devices held desc, usage after placing asc, id)
for k in candidates:
    fit_on_numa(k)    # memory or hugepages on k; dedicated CPUs from k
take the best fit
none fits: strict -> refuse; preferred -> whole host, NumaAligned=False
```

- `fit_on_numa()` charges each NUMA node its bound load. Load not bound to a
  NUMA node is spread over the NUMA nodes by their share of memory (or of
  pages).
- `pick_cpus()` takes isolated cores first, then cores by id. It takes whole
  SMT cores unless `dedicated_whole_cores` is off.
- The first core of each NUMA node is kept for the host unless the operator
  lists reserved CPUs (`reserved_cpus()`).

**Aligned (GPU) placement.** `aligned_fit()` uses exactly the NUMA nodes
that hold the VM's devices. It splits vCPUs and memory between them in
proportion to their device counts, using the largest-remainder method
(`split_by()`). Each share must fit its NUMA node, and every share must use
the same page size.

**Pools.** `pick_pool_devices()` chooses devices for pool requests:
- Candidates are ready, not cordoned and not taken.
- It prefers devices on the NUMA node of the VM's other devices, then goes
  by address.
- Under `aligned`, it uses as few NUMA nodes as it can.
- With `same_switch`, all devices come from one PCIe switch, preferring a
  switch the VM already uses.

A `gpus` request is expanded before scheduling, in `service/vm.rs`
`expand_gpus()` and `apply_gpus()`:
- It becomes a pool request.
- The NUMA policy defaults to `aligned`.
- vCPUs and memory default to the pool's `per_device` sizing × count.

**Score.** `evaluate()` scores each node that fits:

```text
score = max(cpu share, memory share, hugepage share)   # after placing the VM
order: fits first; then score asc; then fewest VMs; then node name
```

The result is deterministic. `schedule()` returns the first node in that
order. `explain()` joins the refusals, capped at 8 nodes plus "and N more".
The preview endpoint (`POST /scheduler/preview`) returns the full
`evaluate()` list.

**Where it runs.**
- At VM create, under the write lock on the VM map (`state.rs`,
  `pick_node()`).
- Again for VMs created without a placement: the leader runs
  `place_pending()` (`controller/placement.rs`) every 3 s.
- The placement, the disk bindings and the IPAM reservations are recorded in
  one store commit.
- A standalone host runs the same `place_on()` against itself.
- A change to a placed VM re-fits it on its own node (`refit_placement()`).
  What no longer fits is refused with 409.

**Code.** `scheduler.rs`: `schedule`, `evaluate`, `failing`, `place_on`,
`numa_fit`, `aligned_fit`, `pick_pool_devices`, `page_kib`, `device_taken`,
`loads`.

**Tests.** In `scheduler.rs`:
- `the_least_allocated_node_wins_and_ties_break_by_count_then_name`
- `capacity_counts_stopped_vms_and_the_overcommit_factors`
- `devices_are_claimed_per_node_and_by_iommu_group`
- `hugepages_are_counted_apart_from_memory_and_never_overcommitted`
- `the_hugepage_share_counts_in_the_score`
- `preferred_falls_back_to_the_whole_host_and_says_so`
- `aligned_over_two_numa_nodes_splits_cpus_memory_and_devices`
- `pools_give_devices_near_each_other`

Integration tests in `tests/cluster_tests.rs`:
`vfio_devices_are_claimed_per_node_and_by_iommu_group` and
`hugepages_are_counted_per_node_and_fill_up`.

**Spec.**
- [14-clustering.md §9.1](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md#91-scheduler)
- [22-host-topology.md §8](https://github.com/CMKL-APEX/glidex/blob/main/spec/22-host-topology.md#8-scheduler)
- [22-host-topology.md §8a](https://github.com/CMKL-APEX/glidex/blob/main/spec/22-host-topology.md#8a-hugepages)
- [27-gpu-vms.md §4](https://github.com/CMKL-APEX/glidex/blob/main/spec/27-gpu-vms.md#4-placement-aligned)

The code goes slightly beyond §9.1 in two ways: the score also includes the
hugepage share, and placement also happens at VM create, not only on the
leader's pass.

## VM reconciliation

**Problem.** Make each VM's instance (the shim and its hypervisor) match
the VM's spec. This must survive crashes of the control plane, the shim
and the host.

**Desired state.**
- `spec.power`: `Running`, `Paused` or `Stopped`.
- `restart_policy`: `OnFailure` (default) or `Never`.
- `on_host_boot`: `Resume` (default) or `Stop`.

**Observed state.** `status.phase` is one of `Stopped`, `Provisioning`,
`Starting`, `Running`, `Paused`, `Stopping`, `Failed` or `Unknown`, plus
conditions such as `Ready`, `CrashLoopBackOff` and `RestartRequired`.

### Liveness

Before acting, the controller establishes whether an instance exists.
`instance::liveness()` runs these checks, and the first that shows life
wins:

```text
1. the systemd unit is active
2. shim or hypervisor (pid, start time) is alive, from status.instance and
   instance.json, but only if their boot id is the current boot id
3. api.sock or shim.sock accepts a connection
all checks made, none alive   -> Dead
any check could not be made   -> Unknown   # never acted on
```

The start time guards against pid reuse. The boot id makes a host reboot
read as "no instance".

### One round

`controller/vm.rs` `reconcile_vm()` runs one round. Every step is
idempotent. A round ends with "done" or "requeue after *t*".

```text
skip VMs placed on another node
observe liveness
Unknown                      -> phase Unknown, Ready=Unknown, requeue 10 s
Live, instance.json has an id we have no record of -> adopt it (AdoptedUnrecorded)
Live, shim reports Exited    -> release the shim, phase Stopping
not Live, instance recorded  -> finish_exit (exit cause, restart policy)
deleting and not Live        -> run finalizers in order
desired Stopped:  Live -> stop_round   else detach ports, phase Stopped, Ready=True
desired Running/Paused:  not Live -> launch_round   else running_round (drift)
```

- **`stop_round`:** asks the shim to stop with the VM's grace period. The
  deadline is grace + 15 s. Past the deadline it escalates to stopping the
  unit, and 15 s later to killing it.
- **`launch_round`:**
  1. Waits for `next_restart_at`.
  2. Checks that disks, images and network records are there.
  3. Provisions: seed, NIC ports, `launch.json`.
  4. Writes the intent record (`status.instance`) **before** it starts the
     runner.

  A failure before the intent detaches the ports that this round added. A
  failure after it is found by the next round's observation.
- **`running_round`:** corrects drift: pause and resume, VFIO hot-plug,
  ports and CPU affinity. It sets `RestartRequired` when a launch-relevant
  field differs from what was launched.

### Exit handling and restart policy

`finish_exit()` first classifies how the instance ended:

| How it ended | Cause |
|---|---|
| Boot id changed | `HostReboot` |
| The shim recorded an exit | that cause |
| No `instance.json` | `Lost` (an intent that never launched) |
| `instance.json` without an exit record | `Lost` (the shim died) |

Then it acts on the cause:

| Cause | Action |
|---|---|
| `Requested`; `Lost` that never launched | none |
| `Terminated` (unit stopped outside Glidex), `CleanExit` (guest powered off) | set `spec.power = Stopped` |
| `HostReboot` | `Resume`: launch again; `Stop`: set `spec.power = Stopped` |
| `LaunchFailed` | phase `Failed`; retry after `backoff(launch_failures)` |
| `Crashed`; `Lost` from a dead shim | `OnFailure`: restart after `crash_delay(n)`, `CrashLoopBackOff`; `Never`: set `spec.power = Stopped` |

```text
crash_delay(n) = min(10 s << (n-1), 300 s)        # 10, 20, 40, ... 300
restart_count resets after 600 s of running (STABLE_SECS)
backoff(n)     = clamp(1 s << (n-1), 1 s, 300 s)  # queue and launch failures
```

There is no limit on the number of restarts. They continue at most 300 s
apart. When the controller sets the spec to stopped, it writes only if the
generation has not changed since it observed the exit
(`stop_spec_after_exit()`).

### Work queue

`controller/queue.rs` holds one queue for every kind of object, keyed by
`(kind, id)`.

- A key is queued at most once.
- A key is processed by at most one worker at a time. A key added while it
  is being processed is marked dirty and processed again afterwards.
- Failures back off per key with `backoff(n)`. A success resets the count.
  An explicit "requeue after" is not a failure.

Triggers:
- API writes.
- A pidfd exit watch on each live shim and hypervisor (`controller/mod.rs`
  `wait_for_exit()`).
- Cross-object events, such as a disk becoming ready.
- A resync of every object every `resync_secs` (30 s by default), spread
  evenly over the period.

`reconcile.workers` (4 by default) run rounds in parallel.

### Adoption after a restart

`controller/startup.rs` `adopt_instances()` runs before any controller
starts. It launches, stops and changes nothing. For each local VM:
- If the VM is live, it refreshes the recorded pids. If `instance.json`
  names an instance the record lacks, it rebuilds the record from
  `instance.json` and `launch.json`. It emits `Adopted`.
- Runtime directories and units with no VM record are reported as orphans
  and never touched.
- Only after this does it tell netd which VMs own ports.

There are no pid files. A running VM is found again from:
- `instance.json`, which the shim writes;
- the persisted `status.instance`;
- the systemd unit `glidex-vm@<id>.service`.

**Tests.**
- `queue.rs`: `dedupes_and_never_runs_a_key_twice_at_once`,
  `delayed_keys_come_when_due`, `backoff_doubles_and_caps`.
- `vm.rs`: `crash_backoff_is_10s_doubling_to_5min`,
  `an_unchanged_status_writes_nothing`.
- `instance/mod.rs`: `each_check_alone_shows_life`.
- Restart and adoption end to end: `restart_e2e` in
  `tests/functional_tests.rs`, and `systemd_runner_launches_adopts_and_stops`.

<!-- TODO(verify): no test pins on_host_boot (HostReboot Resume vs Stop) or restart_policy Never; behaviour above is from the code only. -->

**Spec.**
- [12-reconciliation.md §9.1](https://github.com/CMKL-APEX/glidex/blob/main/spec/12-reconciliation.md#91-reconcile)
- [§9.3](https://github.com/CMKL-APEX/glidex/blob/main/spec/12-reconciliation.md#93-work-queue)
- [§9.4](https://github.com/CMKL-APEX/glidex/blob/main/spec/12-reconciliation.md#94-startup-order)
- [§7.5](https://github.com/CMKL-APEX/glidex/blob/main/spec/12-reconciliation.md#75-exit-handling)
- [§8.7](https://github.com/CMKL-APEX/glidex/blob/main/spec/12-reconciliation.md#87-liveness-establishing-no-instance-d8)

## Replicated store and Raft

**Problem.** Keep one database consistent across the cluster's servers.
Applying the replicated log must give the same bytes on every member and on
every software version.

**Library.** [openraft](https://docs.rs/openraft/0.9.25/openraft/) 0.9.25,
pinned exactly. It implements [Raft](https://raft.github.io/raft.pdf). The
store is [redb](https://docs.rs/redb). Code: crate `glidex-raftstore`
(`store.rs`, `raft/`) and `cluster/`.

### Write sets

A **write set** is the ordered list of key changes one write made:

```text
WriteSet { format, origin, ops: [ Put{table, key, value} | Delete{table, key} ] }
```

The leader computes write sets. `Db::write(origin, f)` works like this:

```text
take the single write lane
new leader? first apply everything committed before it (ensure_linearizable)
open a real redb write transaction; run f against it
    every insert/remove is recorded as an Op; reads see the transaction's writes
empty write set   -> abort, nothing happens
local mode        -> commit
replicated mode   -> abort the local transaction, propose the write set,
                     return once it is committed and applied
```

Application logic runs only on the leader. The Raft log entry holds the
outcome, not the request.

Followers, and the leader itself, apply each entry with `Db::apply()`. This
is a pure byte operation: the puts and deletes are applied in one redb
transaction, and `last_applied` is written in that same transaction. A
crash therefore never applies an entry twice and never skips one. After
the commit, the revision becomes the Raft index and subscribers are told.

Constraints:
- A write set larger than the schema's limit (4 MiB of JSON) is refused.
- Table ids are append-only, because they are stored in the log.
- The lane is held across the Raft round trip, which needs the runtime's
  I/O. `Db::begin` tries the lane first and, when it is held, waits in
  `block_in_place` on a Tokio worker (`glidex_raftstore::wait`), so the
  worker's other tasks and its I/O move to another thread.

**Replay check.** Setting `GLIDEX_CHECK_WRITE_SETS` enables the replay
check. CI runs `cargo test` with it set. With the variable set:
1. Every fresh database journals the write sets it publishes.
2. When the database is dropped, the journal is replayed into an empty
   database.
3. The two databases must dump identically, otherwise the test panics.

This pins the invariant that the write set alone reproduces the state
(`Drop for Db`, `Db::replay`).

**Snapshots.**
- A snapshot is a read-transaction view of the live database, so building
  one costs nothing.
- It is streamed to a file only when it has to be sent to a member.
- Installing it replaces the replicated tables and sets `last_applied` in
  one transaction.
- The log is a separate redb file, `raft/log.redb`. Entries are durable
  before they are acknowledged.

**Defaults.** Heartbeat 250 ms, election timeout 1000–2000 ms, 10,000 log
entries kept.

### Writes on a follower

- **API writes.** A follower forwards the whole HTTP or RPC request to the
  leader over the cluster port, with the authenticated principal attached
  (`api/gate.rs`, `cluster_gate()` and `forward()`).
  - The leader accepts forwarded requests only from server certificates. It
    authorizes the request itself.
  - With no leader known, the follower waits up to 3 s, then answers `503`.
  - A `421` that names the leader is retried.
- **Node status writes** (controller, images and network origins). These go
  to the leader as write sets. The leader replays them inside its own write,
  under a check of what that node may write (`cluster/sync.rs`,
  `check_status_write()`). The node then waits until its replica reaches
  the returned index (read-your-writes).
- **Agents** (nodes that are not servers) keep a mirror of the store. Status
  writes they can't deliver are queued on disk and resent.

### Reads

Reads are linearizable by default:

```text
leader:   read index = raft.get_read_log_id()
follower: ask the leader for its read index (POST /cluster/v1/read-index)
wait until the local replica has applied that index (<= 10 s)
read the local replica
```

Concurrent reads share one round trip. Without a quorum, a read fails with
`503 cluster_unavailable`, unless the client asks for a possibly stale
local read with `X-Glidex-Consistency: local`. Read responses carry
`X-Glidex-Revision`.

### Liveness and readiness

- Each node sends a heartbeat to the leader every 5 s.
- Every 2 s, the leader compares each node's silence with
  `cluster.node_grace_secs` (40 s by default).
- A silent node becomes `Ready=Unknown/NodeUnreachable`. So do its VMs, in
  the same write. Nothing is stopped or moved.
- A new leader resets every node's grace to the time it took over, so a
  leader change never marks healthy nodes unreachable
  (`cluster/lifecycle.rs` `liveness_round()`, `cluster/sync.rs`
  `Liveness`).

A VM becomes `Lost` only when an operator forgets a fenced node.

### Watch streams

The public watch (`GET /watch` as server-sent events, and the RPC
`WatchService.Watch`) works as follows:
1. It sends every visible object as `added`, then `synced`.
2. When the store's change bell rings, or every 10 s, it waits 250 ms for
   changes to settle, lists again with the list endpoints' own
   authorization, and sends the differences.
3. Keep-alives go out every 15 s.
4. A stream ends after 300 s. The client reconnects and gets a fresh
   snapshot, so policy changes always apply.

Code: `service/watch.rs`, `api/watch.rs`, `api/rpc/watch.rs`.

Agents follow the store with a long poll by revision (`/cluster/v1/watch`).
A revision older than the server's in-memory log gets `410 Gone`, and the
agent lists again.

**Tests.**
- `glidex-raftstore`: `write_sets_replay_to_the_same_bytes`.
- `glidex-raftstore/tests/raft.rs`: `a_lagging_learner_catches_up_from_a_snapshot`,
  `killing_the_leader_keeps_writes_going`,
  `a_partitioned_leaders_writes_are_rejected`.
- `tests/cluster_tests.rs`:
  `reads_without_a_quorum_fail_unless_stale_is_asked_for`,
  `a_follower_server_writes_its_status_through_the_leader`,
  `a_silent_node_is_unreachable_not_gone`.

**Spec.**
- [14-clustering.md §6.2](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md#62-raft-integration)
- [§6.4](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md#64-reads-writes-and-forwarding)
- [§6.5](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md#65-watch)
- [§7.2](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md#72-liveness)

## Metering

**Problem.** Turn host counters and gauges into hourly usage rows that are
counted exactly once. This must hold across crashes, counter resets and
leader changes. Code: `metering/`.

### Sampling

A round runs every `sample_secs`: 30 s by default, which must divide 300,
and rounds align to the wall clock (`metering/mod.rs`,
`metering/sampler.rs`). Each round reads:

| Source | Meters |
|---|---|
| cgroup (or `/proc`) | CPU time and memory |
| Hypervisor | disk I/O |
| OVS port statistics | NIC and bridge bytes |
| nftables counters (NAT networks) | external traffic |
| Conntrack (OVN gateways) | external traffic |
| VM phase | allocation gauges |

### The ledger

`metering/ledger.rs` keeps:
- a **cursor** per counter: last value, time and reset key;
- **open-hour** accumulators;
- **closed** hourly rows;
- **5-minute slots** for percentiles.

```text
counter reading (value, at) with reset key r:
    same r, value >= cursor  -> delta = value - cursor
    same r, value <  cursor  -> delta = value (a reset), flagged
    new r (a new run)        -> counted from 0 (from launch time if known)
    first sight, before metering started -> baseline only, no backfill
gauge reading: integrate level x elapsed time; carry the sub-unit remainder
spread each delta over [from, to] into hours and 5-minute slots by overlap
commit: cursors + accumulators + slots in ONE transaction
close every open hour that ended more than close_grace_secs (120 s) ago
    first row for (hour, subject) has seq 0; a later one is an adjustment, seq n+1
```

Exactly once rests on the commit: a delta and the cursor that produced it
commit together. A failed commit changes nothing, and the next round
recomputes from the old cursor.

`split_by()` splits a delta with cumulative integer arithmetic, so the
parts always sum exactly to the delta. A delta spread over a gap longer
than two sampling intervals is flagged `interpolated` and gives no peak.

### In a cluster

1. Each node keeps its open state in a local `meter.db`.
2. Every 60 s it ships its closed rows to the leader
   (`POST /cluster/v1/ledger`).
3. The leader merges them in one replicated write (`Ledger::import()`). It
   records a key per node and row in `ledger_inbox`. A row already in the
   inbox is skipped.
4. The node deletes its local copies only after the acknowledgement.

Shipping is at least once and merging is idempotent, so the cluster ledger
is exactly once. The cluster's completeness is the least that any live
node has closed.

### Billing month

- A billing month is a calendar month in `metering.billing_timezone`.
- Only zones whose offset is a whole number of hours all year are accepted.
  An hour row therefore never straddles a month boundary, and month totals
  are exact sums of hourly rows, adjustments included (`metering/query.rs`
  `month_bounds()`).
- The open hour is reported as provisional.

### 95th percentile

Bandwidth, disk I/O and compute percentiles use the 5-minute slots
(`metering/rates.rs`):

```text
slots = 5-minute slots in range where any member of the group was present
series[slot] = sum of members' deltas in that slot      # p95 of the sum (D15)
sort series; rank = ceil(0.95 * N); p95 = series[rank - 1]   # nearest rank
Mbps = bytes * 8 / 300 / 1e6
billable = max(p95 rx, p95 tx)
```

Once a billing month is complete, its final percentiles are written once
(`metering/retention.rs`). After retention, hourly rows roll up into daily
rows at the billing zone's midnight.

### Derived meters

Derived meters are computed at read time (`query.rs` `derive()`):
- `net.bytes` = rx + tx.
- `net.internal_bytes` = total − external.
- Average rates over the bucket.
- Disk latency = time / operations.

Metering stops at usage. Glidex applies no prices.

**Tests.**
- `ledger.rs`: `exactly_once_across_rounds_and_failed_commits`,
  `counter_deltas_and_reset_cases`, `no_backfill_before_metering_started`,
  `split_parts_sum_to_amount`,
  `hours_close_with_grace_and_late_deltas_become_adjustments`,
  `shipped_hours_merge_exactly_once`, `gauges_lose_nothing_to_rounding`.
- `rates.rs`: `nearest_rank`, `group_p95_is_of_the_summed_series`.
- `query.rs`: `month_buckets_in_a_whole_hour_zone`.

**Spec.**
- [13-metering.md §6](https://github.com/CMKL-APEX/glidex/blob/main/spec/13-metering.md#6-collection-algorithm)
- [§8.4](https://github.com/CMKL-APEX/glidex/blob/main/spec/13-metering.md#84-billing-month)
- [§8.5.1](https://github.com/CMKL-APEX/glidex/blob/main/spec/13-metering.md#851-95th-percentile-d15)
- [14-clustering.md §13.1](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md#131-node-local-accumulation-shipped-hours)

## Images and disks

### Verified download

`images/download.rs` `run_download()` and `verify_and_install()`:

```text
stream to <id>.qcow2.part, hashing as bytes arrive
    (sha256 always; also the vendor's algorithm, e.g. sha512)
retry up to 4 attempts, backing off 2, 4, 8 s
    resume with a Range request only if the server gave an ETag or Last-Modified;
    the part already on disk is rehashed first
compare digest with the expected one; mismatch -> Failed
qemu-img info: refuse a backing file, refuse over the size limit
qcow2: re-probe with the format forced; raw: convert to qcow2 and rehash
fsync, chmod 0444, rename into place
```

The rename is the commit. A file under its final name is complete and
verified.

### Linked disks

A linked disk shares its image's data and stores only its own writes.

- **File datastores** (`datastore/file.rs`): a linked disk is a qcow2
  overlay whose backing file is the image. The backing path is relative to
  the datastore directory, so it doesn't depend on where the directory is
  mounted. Every change is made on a dot-file and renamed into place.
- **LVM datastores** (`datastore/lvm.rs`): each node builds the image's
  thin volume `gx-i-<image>` once from the verified file, then **seals** it
  (makes it read-only). A linked disk is a thin snapshot of that volume.
  - A half-filled, unsealed image volume left by a crash is deleted and
    rebuilt.
  - Disk volumes are built as `<name>.new` and renamed, so a volume under
    its final name is complete.
  - Privileged LVM operations go through `glidex-stord`.

### Extend root

`images/partition.rs` and `images/disk.rs` `extend_root_at()` grow the root
partition into free space at the end of the disk.

```text
read GPT or MBR from the disk's first MiB
root = partition with the Discoverable Partitions root type for the arch,
       else the physically last partition if it is a Linux filesystem
refuse unless root is the physically last partition
offline mode:
    raw: edit in place
    qcow2 or block: sparse "shadow" raw file the disk's size, holding copies of
        the first and last windows (<= 1 MiB each)
    sgdisk -e (GPT), then growpart <root>
    check the tools wrote nothing outside the windows; write changed windows back
on-boot mode, or partition tools missing: set pending_growpart; the guest grows it
```

Only the partition table changes. The guest grows the filesystem on its
next boot (cloud-init). The shadow file avoids FUSE and NBD exports, so no
root privileges are needed. Partition numbers don't matter; position does.

### Image copies across nodes

- Each node holds its own copy of each image it needs
  (`controller/image_cache.rs`). A node fetches the file from a node that
  holds it, over the cluster port, checks its sha256 and renames it into
  place.
- A shared datastore with linked disks gets its own copy of the image
  (`controller/store_copies.rs`). The copy is made under a lock file, checked
  by digest, made read-only and renamed.

**Tests.**
- `download.rs`: `hashing`.
- `partition.rs`: `gpt_root_by_type_last`,
  `ubuntu_layout_root_is_partition_one_at_the_end`,
  `gpt_root_not_last_is_refused`, `edit_qcow2_through_shadow_keeps_data`.
- `tests/datastore_tests.rs`:
  `a_linked_disk_on_a_shared_datastore_overlays_the_datastores_copy`.

**Spec.**
- [10-images.md §5](https://github.com/CMKL-APEX/glidex/blob/main/spec/10-images.md#5-download-pipeline)
- [10-images.md §6.4](https://github.com/CMKL-APEX/glidex/blob/main/spec/10-images.md#64-extend-root-partition)
- [24-storage.md §11.1](https://github.com/CMKL-APEX/glidex/blob/main/spec/24-storage.md#111-design-s6-glidex-stord-and-lvm)
- [14-clustering.md §9.2](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md#92-images-and-disks)

## Kernel extraction

**Problem.** Boot a VM directly from a kernel, using the kernel, initramfs
and command line found inside a disk image or the VM's own disk. The guest's
filesystems must never be parsed on the host.

**How.** The extractor is a small appliance VM: a pinned Debian kernel with
an initramfs that Glidex builds. It runs as a one-shot shim job with:
- 1 vCPU and 1 GiB of memory;
- no NIC and no seed;
- the image attached read-only;
- an output disk;
- a 120 s timeout.

Code: `images/extractor.rs`, `images/extract.rs` and `controller/extract.rs`.

Inside the VM, the appliance mounts each partition read-only and writes one
stream to the output disk. The stream holds the `vmlinuz-*` files, the
initramfs files, `grub.cfg` and BLS entries:

```text
GXFILE <partition>:<path> <size>\n<bytes> ... GXEND\n
```

The host parses the stream with every length checked. It refuses:
- malformed headers;
- unsafe names;
- more than 64 files;
- oversized files;
- a total over 2 GiB.

**Choosing.** `choose()` picks the kernel and its command line:

```text
candidates = vmlinuz-<v> that have initrd.img-<v> or initramfs-<v>.img
             (same partition preferred)
kernel = highest <v>, compared segment by segment, digit runs numerically
cmdline = "options" of the BLS entry whose "linux" names vmlinuz-<v>,
          else the first grub.cfg "linux"/"linuxefi" line naming it
drop $VARS, BOOT_IMAGE=, console=, earlyprintk=; append the arch's serial console
no root= -> fail
```

Boot loader defaults (GRUB `set default`, `saved_entry`) are not consulted.
The newest kernel that has an initramfs wins.

**Following the guest's kernel.** With `kernel_from_disk`, the VM controller
checks the root disk before each launch:
- It compares the disk file's size and modification time (`disk_stamp`) with
  the values at the last extraction.
- If they are unchanged and the kernel image is still usable, it boots that
  kernel without extracting.
- Otherwise it extracts again. A result whose digests match a stored kernel
  image reuses that image.
- On failure it boots the kernel it already has and sets
  `KernelExtracted=False`.

The kernel that boots is recorded in `status.disk_kernel`, not in the spec.

**Tests.** In `extractor.rs`: `newest_kernel_with_an_initramfs`, `versions`,
`choice_on_the_catalog_layouts`, `bad_streams_are_refused`,
`choice_fails_cleanly`.

**Spec.**
- [23-kernel-boot.md §10.4](https://github.com/CMKL-APEX/glidex/blob/main/spec/23-kernel-boot.md#104-choosing-the-kernel-and-its-command-line)
- [§10.5](https://github.com/CMKL-APEX/glidex/blob/main/spec/23-kernel-boot.md#105-from-a-vms-own-disk)

## Disk move and volume locks

### Volume locks

On a shared datastore, two nodes must never open one volume.

Glidex uses one lock file per volume, `<datastore>/.locks/<disk-id>.lock`,
held with a non-blocking `fcntl(F_OFD_SETLK)` write lock. The lock is
`glidex_vm_shim::util::try_lock_file()`.
- OFD locks belong to the open file, not to the process. The descriptor is
  close-on-exec, so a child process never inherits the lock.
- Over NFSv4 the lock applies cluster-wide.
- Node-local datastores take no lock file. Placement and claims already
  protect them.

Holders:
- **The shim** takes the locks of the VM's shared disks before it starts
  the hypervisor, and holds them until it exits
  (`glidex-vm-shim/src/supervisor.rs`). A held lock fails the launch.
- **Disk operations** take the lock around the work
  (`StorageDriver::lock()`). A held lock returns `Busy`, and the operation
  waits.

The lock is on a separate file, not on the disk itself, because the
hypervisor takes its own lock on the disk file.

### Moving a disk

`POST /disks/{id}/move` is checked in `datastore/manage.rs` `move_disk()` and
carried out by the disk controller's `move_round()` (`controller/disk.rs`)
using `images/disk.rs` `move_volume()`.

```text
preconditions: Ready, not deleting, not already moving, no running VM using it,
               destination reachable from the mover and the VM's node, space
choose one mover node: the disk's node (node-local), else its worker if that
    reaches the destination, else a node reaching both datastores
record disk.moving = {datastore, node, mover, flatten}; the disk stays Ready
mover:
    take source and destination volume locks
    destination already there? done (resume after a crash)
    copy to a temp: qemu-img convert when flattening, else a sparse copy
    linked, not flattened: rebase (-u) onto the destination's image copy
        (not there yet -> MovePending, retry every 5 s)
    qemu-img check (qcow2), rename into place
one write: datastore, node, worker, origin mode; clear moving; event Moved
delete the source file
any other error: clear moving, event MoveFailed; the source is untouched
```

In a cluster, the mover's final write is accepted only if it ends exactly
the move that was asked for (`cluster/sync.rs` `check_move_write()`).
Moving a node-local disk to another node goes through a shared datastore.

**Tests.**
- `datastore/file.rs`: `a_shared_volume_is_locked_once`.
- `glidex-vm-shim`: `a_file_lock_is_held_once`, `a_locked_disk_stops_the_launch`.
- `tests/datastore_tests.rs`: `disks_move_between_datastores`.
- `tests/cluster_tests.rs`:
  `a_node_local_disk_moves_to_a_shared_datastore_and_its_vm_can_go_anywhere`.

**Spec.**
- [24-storage.md §8.3](https://github.com/CMKL-APEX/glidex/blob/main/spec/24-storage.md#83-moving-a-disk-between-datastores)
- [24-storage.md §10](https://github.com/CMKL-APEX/glidex/blob/main/spec/24-storage.md#10-exclusive-access)

## Directory pull sync

**Problem.** Mirror an identity provider's groups into users and teams,
without letting a bad read disable everyone. Code: `directory/sync.rs`.

**Steps.**
1. Read a snapshot of the connector's groups and their members (nested
   members included).
2. Compute a plan with `plan()`, a pure function.
3. Apply the plan in writes of at most 500 changes.

```text
wanted = members of the snapshot's groups that the snapshot has users for
for each wanted source user s (matched by the IdP's user id, never by email):
    a managed user with external_id = s.id  -> Update if fields differ;
                                               Enable if active and disabled by sync;
                                               Disable if suspended or archived
    else a user who signed in with that subject -> Adopt (Disable if inactive)
    else if active -> Create
managed users not wanted and not disabled -> Disable
groups: the snapshot's groups, members mapped to glidex ids
```

An administrator's disable is never undone by sync. Only users that sync
disabled are re-enabled.

**Safety stop.**

```text
limit = max(managed users * max_disable% / 100, 5)    # max_disable defaults to 10
if count(Disable ops) > limit and the run isn't confirmed:
    state Blocked, list who would be disabled, apply nothing
```

A failed read applies nothing.

**Scheduling.** The sync runs on the leader, or on a standalone host. It
runs every `interval_minutes` (15 by default, at least 5) and at most one
run per connector at a time. A blocked run is retried on the next interval.

**Tests.**
- `sync.rs`: `a_plan_follows_the_groups_and_states`,
  `the_disable_limit_has_a_floor`.
- `tests/connector_tests.rs`:
  `a_pull_follows_the_directory_and_stops_before_removing_many`.

**Spec.** [19-tenancy.md §8.2](https://github.com/CMKL-APEX/glidex/blob/main/spec/19-tenancy.md#82-pull-sync)

## Console log replay filter

**Problem.** A new console client is first sent the console log, then live
output. The log holds the guest's queries to its terminal; systemd, for
example, probes a serial console's size at each getty start. A terminal
that receives a replayed query answers it again, and the guest reads the
stale answer as typed input.

**How.** `glidex-vm-shim/src/replay.rs` `strip_queries()` removes queries
from the replayed log only. Live output passes unchanged.

```text
scan the log; at ESC:
    CSI ... final byte  -> drop if a query: DSR (n), DA (c), DECRQM ($p),
                           XTVERSION (>q), kitty flags (?u), XTWINOPS reports (t)
    DCS +q / $q ... ST  -> drop (XTGETTCAP, DECRQSS)
    OSC 4,5,10-19,52 with a "?" field ... ST or BEL -> drop (colour/clipboard queries)
    anything else       -> copy
unterminated string or a query cut off at the end -> copy (the rest arrives live)
```

It runs in one pass, in linear time. It is used in `glidex-vm-shim/src/proxy.rs`
when a client connects.

**Tests.**
- `replay.rs`: `systemds_serial_probes_are_left_out`,
  `every_kind_of_query_is_left_out`, `output_that_only_looks_alike_is_kept`,
  `a_query_cut_off_at_the_end_is_passed_on`,
  `an_abandoned_string_does_not_swallow_what_follows`.
- `proxy.rs`: `replay_leaves_out_the_guests_terminal_queries`.

**Spec.** [02-console.md, log files](https://github.com/CMKL-APEX/glidex/blob/main/spec/02-console.md#log-files)

## IPAM

### Cluster (OVN) networks

`ipam.rs`. All allocations are store writes, so they replicate with
everything else.

```text
allocate_subnet(network):
    already has one -> return it (idempotent)
    requested       -> /16../29, must not overlap the node NAT range or any subnet
    else            -> first /24 in ovn.nat_supernet (default 10.89.0.0/16)
                       that overlaps nothing; none -> SupernetFull
    gateway = .1; pool = .2 .. last host
reserve(network, mac):
    already reserved -> same address (stable across stops and restarts)
    else lowest free address in the pool; none -> PoolFull
```

- Reservations are made at placement, in the same commit as the placement.
- They are freed when the VM or NIC is gone, by the leader's OVN
  reconciler (`controller/ovn.rs` `gc_reservations()`).
- Each reservation becomes the port's `addresses` and `port_security` in
  OVN. Addresses reach guests through OVN's native DHCP.
- Router external addresses come from `allocate_external()`: the requested
  address, or the lowest free one.

### Node-local NAT networks

Node-local NAT networks follow the same rules in `glidex-ovs/src/nat.rs`:
- The first /24 of `10.88.0.0/16` that overlaps no host route and no other
  NAT network.
- dnsmasq serves DHCP with a hosts file of reservations.

MAC addresses are `02:` followed by 5 bytes of SHA-256(VM id, NIC index):
locally administered, and stable per NIC (`glidex-ovs/src/names.rs`).

**Tests.**
- `ipam.rs`: `subnets_come_from_the_supernet_and_never_overlap`,
  `addresses_are_the_lowest_free_and_stable`,
  `external_addresses_come_from_the_pool_once_each`.
- `nat.rs`: `picks_first_free_subnet`.

**Spec.** [14-clustering.md §11.4](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md#114-ipam)

## OVN northbound sync

**Problem.** Keep the OVN northbound database equal to the cluster's
networks, routers and placed VM ports. Glidex must not disturb rows it
didn't create.

**How.** It reconciles declaratively.
1. The leader's network controller builds a `Desired` value from networks,
   IPAM and placements every 5 s (`controller/ovn.rs`).
2. `glidex_ovn::sync()` (`crates/glidex-ovn/src/lib.rs`) reads the rows,
   then creates, updates or deletes **only** rows tagged
   `external_ids:glidex-owner=glidex`.
3. A foreign row where a Glidex object should go is reported, never changed.
4. A row that already matches costs no write.
5. Every call is an `ovn-nbctl` command line, so tests assert exact
   commands.

Supporting details:
- `derived_mac()` gives router ports stable, locally administered MACs
  (FNV-1a of a seed).
- DHCP options carry the gateway, MTU and DNS servers.
- Each port is bound to the chassis of the node its VM is placed on.

**Tests.** In `crates/glidex-ovn/src/tests.rs`:
`a_port_that_is_already_right_costs_nothing`,
`ports_and_networks_gone_from_the_plan_are_removed_and_foreign_rows_are_left`,
`macs_are_stable_and_locally_administered`.

**Spec.** [14-clustering.md §11](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md#11-ovn-networks)

## Rate limiting

Glidex uses two simple in-memory limiters.

**Token bucket.** This limiter is used for passkey sign-in (per client),
workload federation token exchange (per federation) and the SCIM endpoints
(per connector). Each key holds `(last time, level)`:

```text
level = min(capacity, level + elapsed * refill_rate)
if level >= 1: level -= 1; allow
else: refuse (429)
```

Code: `auth/passkeys.rs` `Ceremonies::attempt()`, `directory/exchange.rs`
`take()`, `api/scim.rs` `take()`. See
[token bucket](https://en.wikipedia.org/wiki/Token_bucket).

**Sliding window of failures.** `glidex-authd/src/limiter.rs` keeps the
times of recent failed logins: one queue per user and one global queue.
Old entries are pruned on each check. A login is refused while either queue
is full. A success clears that user's failures. Memory is bounded, because
failures are not recorded while the global window is full.

**Tests.**
- `limiter.rs`: `per_user_window_slides`, `global_window_slides`,
  `success_resets_the_user`.
- `tests/passkey_tests.rs`: `sign_in_attempts_are_limited_by_the_real_client`.

**Spec.**
- [11-security.md §5.3](https://github.com/CMKL-APEX/glidex/blob/main/spec/11-security.md#53-local-pam-glidex-authd)
- [25-passkeys.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/25-passkeys.md)

## CA rotation and certificate renewal

**Problem.** Replace the cluster CA with no downtime, and renew node
certificates before they expire. Code: `cluster/rotation.rs`,
`cluster/pki.rs`.

Keys are ECDSA P-256. A node certificate lasts 365 days and the CA 10
years. The signed subject is set by the CA from the node's identity, never
taken from the request.

**Rotation state.** One replicated row, `CaState`, holds:
- the trust bundle every node installs;
- the signing CA's fingerprint;
- the retiring CAs' fingerprints;
- the time the old CA is retired.

```text
leader: generate a new CA; send it to every live server (any refusal aborts)
one write: trust = new + old, signing = new, retire_at = now + grace (7 days)
every node, every 3 s:
    trust_sync: install the replicated bundle if it differs, reload TLS
    renew if the cert's issuer isn't the signing CA, or < 30 days remain:
        new key, CSR to the leader, save cert + key + bundle
leader: once every member's latest cert is from the new CA (and its own),
        or retire_at has passed:
        one compare-and-swap write: trust = new only, clear retiring
servers then delete the old CA key
```

During the window, every node trusts both CAs. A node with an old
certificate and a node with a new one can therefore still talk to each
other.

Rotation starts:
- on demand (`gxctl cluster rotate-ca`);
- after a server leaves;
- when less than a year of CA validity remains.

**Tests.**
- `cluster/pki.rs`: `a_node_cert_is_signed_for_its_id_and_role`,
  `fingerprints_pin_the_ca`.
- `cluster/identity.rs`:
  `identity_round_trips_and_the_signing_ca_is_found_in_a_bundle`.
- `tests/cluster_tests.rs`:
  `the_ca_rotates_and_every_node_renews_before_the_old_one_is_retired`.

<!-- TODO(verify): spec §12.5 says netd renews OVN certificates on rotation; not found in rotation.rs, glidex-netd or glidex-ovn. -->

**Spec.** [14-clustering.md §12.5](https://github.com/CMKL-APEX/glidex/blob/main/spec/14-clustering.md#125-ca-rotation)

For measured costs of these paths, see
[Performance and benchmarks](benchmarks.md).
