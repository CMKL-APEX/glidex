# Metering: resource usage records

> Status: **M0, M1 and M2 implemented** (2026-10-05; branches
> `metering-m1`, `metering-m2`). Unit, integration, security and
> Playwright tests pass, and so do the KVM host acceptance runs on a
> deployed build (§15.4 and §15.5, *As built*). M3 is design only.
>
> Built differently from the first draft, with the reasons in the
> sections named: the cgroup is found from the shim's
> `/proc/<pid>/cgroup` (§5.1); a bridge shared by several networks
> splits its totals (§5.4); NAT counters are named by MAC (§5.5); CSV
> export is in M1, not M2 (§15.4). It changes the
> contracts in [reconciliation.md](reconciliation.md) §8.4 and §13.1,
> [networking.md](networking.md), [hypervisors.md](hypervisors.md),
> [data-model.md](data-model.md), [rest-api.md](rest-api.md),
> [security.md](security.md) §7 and §13, [installer.md](installer.md),
> [cli.md](cli.md) and [web-ui.md](web-ui.md). §14 lists the edit each
> needs. Make those edits in the same PR as the milestone that changes
> the behaviour (§15), not before it.

Source (planned): `crates/glidex-control-plane/src/metering/`
(`mod.rs`: the `Meter` service and its tasks; `sources.rs`: readers for
cgroups, hypervisors and netd; `ledger.rs`: tables, cursors and
roll-ups; `api.rs`: handlers). There are also new read-only ops in
`crates/glidex-netd` and new stats calls in `crates/glidex-hv-client`.

## 1. Goals and non-goals

Glidex enforces **allocation quotas**: `vcpus`, `memory_mib` and
`disk_gib` are checked when a VM or disk is admitted
([security.md](security.md), `tenancy.rs`). Nothing records what was
used over time. This module records it as durable hourly usage records
per VM, disk, NIC and project. Accounting, chargeback, capacity
planning and "who is using the host" all read these records.

Goals:

1. **Usage meters**:
   - CPU time (core-hours)
   - memory (MiB-hours)
   - disk storage (GiB-hours)
   - network traffic in GiB transferred, per VM NIC and per network
     (NAT, bridged or isolated), summed into a **billing-month total**
     (§8.4). On NAT networks it is also split into **external**
     (internet-bound) and **internal** traffic, both per VM and per
     network
   - disk and network operation rates (IOPS and packets/s)
   - network **bandwidth** in Mbps: average, 30-second peak, and the
     **5-minute 95th percentile** per billing month, for burstable
     billing (§8.5)
   - disk **IOPS, throughput (MB/s) and average latency**: average,
     30-second peaks per direction, and a billable 5-minute 95th
     percentile per disk, VM and project (§8.6)
2. **Allocation meters** for each of these: what was *reserved* over
   time (vCPU-hours, MiB-hours of configured memory, GiB-hours of
   provisioned disk). Fixed-price schemes charge for allocation, not use.
3. **Exactly-once attribution.** Each counter increment is attributed
   to exactly one hour and one subject, across control-plane restarts,
   VM restarts and counter resets (§6).
4. **Survives control-plane downtime.** Counters keep accumulating in
   the kernel, OVS and the hypervisor while the control plane is down.
   They are collected when it comes back (§6.4).
5. **Project-scoped access**, like the audit log: project members read
   their project's usage, and auditors and admins read everything (§10).

Non-goals:

- Pricing, invoices or currency. Consumers apply a rate card to the
  records (`GET /usage` with `format=csv`).
- Real-time monitoring and alerting. The live-rate endpoint (§9.4) is
  for the UI. This is not a Prometheus replacement. An optional
  `/metrics` exporter is listed as future work (§16).
- Enforcing limits on usage, such as throttling a VM that used too
  much CPU. Quotas stay allocation-based. Site resource controls go on
  `glidex-vms.slice` ([reconciliation.md](reconciliation.md) §13.1).
- Metering inside the guest. No agent runs in the guest. Every source
  is on the host.
- Multi-host aggregation (see the clustering non-goal in
  [README.md](README.md)).

## 2. Meters

Every meter has a stable **name**, a **kind** and a **base unit**.
Records store integers in the base unit. Hour figures such as
"core-hours" are computed when the records are presented (§8.3).

| Meter | Kind | Subject | Base unit (stored) | Presented as | Source (§5) |
|---|---|---|---|---|---|
| `cpu.used` | counter | VM | µs of CPU time | core-hours | cgroup `cpu.stat usage_usec` |
| `cpu.alloc` | gauge × time | VM | vCPU·s | vCPU-hours | `launch.json` `vcpu_count` × running time |
| `mem.used` | gauge × time | VM | MiB·s | MiB-hours | cgroup `memory.current` (+ hugepages, §5.1) |
| `mem.alloc` | gauge × time | VM | MiB·s | MiB-hours | `launch.json` `mem_size_mib` × live time |
| `mem.peak` | max | VM | MiB | MiB | max of `memory.current` samples in the hour |
| `disk.alloc` | gauge × time | disk | MiB·s | GiB-hours | `Disk.size_bytes` (provisioned) |
| `disk.stored` | gauge × time | disk | MiB·s | GiB-hours | `qemu-img info` `actual-size` (§5.3) |
| `disk.read_ops` / `disk.write_ops` | counter | disk | ops | ops, avg/p95 IOPS | hypervisor block counters |
| `disk.read_bytes` / `disk.write_bytes` | counter | disk | bytes | GiB, avg/p95 MB/s | hypervisor block counters |
| `disk.read_time_ns` / `disk.write_time_ns` | counter | disk | ns | avg latency (ms) | hypervisor block counters, where available (§5.2) |
| `disk.read_iops_peak` / `disk.write_iops_peak` / `disk.iops_peak` | max | disk | ops/s ×1000 | IOPS | max rate over one sample interval: read, write, read+write |
| `disk.read_kBps_peak` / `disk.write_kBps_peak` / `disk.kBps_peak` | max | disk | kB/s (10³ B/s) | MB/s | same, for throughput |
| `net.rx_bytes` / `net.tx_bytes` | counter | NIC | bytes | GiB | the VM's bridge port, OVSDB `Interface.statistics` |
| `net.rx_packets` / `net.tx_packets` | counter | NIC | packets | packets, avg/peak pps | the VM's bridge port, OVSDB `Interface.statistics` |
| `net.pps_peak` | max | NIC | packets/s ×1000 | pps | max (rx+tx) rate over one sample interval |
| `net.rx_kbps_peak` / `net.tx_kbps_peak` | max | NIC | kbit/s | Mbps | max rate per direction over one sample interval (§8.5) |
| `net.ext_rx_kbps_peak` / `net.ext_tx_kbps_peak` | max | NIC (NAT only) | kbit/s | Mbps | same, for external traffic |
| `bridge.kbps_peak` | max | network | kbit/s | Mbps | max rate of traffic entering the bridge over one sample interval |
| `bridge.ext_rx_kbps_peak` / `bridge.ext_tx_kbps_peak` | max | network (NAT only) | kbit/s | Mbps | same, for the network's external traffic |
| `net.ext_rx_bytes` / `net.ext_tx_bytes` | counter | NIC (NAT only) | bytes (L2-normalized) | GiB | netd NAT counters, keyed by the NIC's reservation (§5.5) |
| `net.ext_rx_packets` / `net.ext_tx_packets` | counter | NIC (NAT only) | packets | packets | netd NAT counters (§5.5) |
| `bridge.bytes` / `bridge.packets` | counter | network | bytes / packets | GiB | Σ bytes entering the bridge over **all** its ports (§5.4) |
| `bridge.ext_rx_bytes` / `bridge.ext_tx_bytes` | counter | network (NAT only) | bytes (L2-normalized) | GiB | netd NAT counters, one pair per bridge (§5.5) |
| `bridge.ext_rx_packets` / `bridge.ext_tx_packets` | counter | network (NAT only) | packets | packets | netd NAT counters (§5.5) |
| `vm.running` / `vm.paused` | gauge × time | VM | s | hours | VM phase |

Meter kinds:

- **counter:** monotonic upstream. Glidex records the *delta* (§6.2).
- **gauge × time:** a sampled level multiplied by the time it held,
  integrated with a left Riemann sum: `value(tᵢ) × (tᵢ₊₁ − tᵢ)`.
  Integer MiB·s cannot overflow a `u64` hourly field: 1 PiB for one
  hour is about 3.9·10¹² MiB·s.
- **max:** the largest value seen in the hour. It cannot be summed
  across hours. Roll-ups take the max of the maxima.

Directions are from the **guest's** point of view: `rx` is what the
guest received. Note that OVS and tap counters are from the host's
point of view (`tx` on a tap is the guest's rx). The sources swap them
(§5.4).

"MB", "GB" and "GB-hours" in user-facing text mean MiB, GiB and
GiB-hours. That matches the quota units (`memory_mib`, `disk_gib`) (D9).

**Traffic is volume, not volume-time.** Network meters count GiB
transferred. The hour bucket only places each byte in time, so hours
add up to days and **billing months** (§8.4), and the month total is
the figure that gets billed (D10). The total counts everything a port
carries, whatever the destination. That includes guest-to-guest
traffic and DHCP, DNS and ping to the gateway.

**External and internal (NAT networks only, D6).** Traffic is
*external* when the host forwards it out of a NAT network or into one:
to the internet, the host's LAN, or anywhere else past the gateway. In
practice that means internet traffic. Everything else on the bridge is
*internal*: VM-to-VM traffic and DHCP, DNS and ping to the gateway.
Internal traffic is **derived**, not stored:

```
net.ext_bytes         = net.ext_rx_bytes + net.ext_tx_bytes
net.internal_bytes    = max(0, net.bytes    − net.ext_bytes)
bridge.ext_bytes      = bridge.ext_rx_bytes + bridge.ext_tx_bytes
bridge.internal_bytes = max(0, bridge.bytes − bridge.ext_bytes)
```

Both subtractions are consistent. Every external byte crosses the VM's
port, and enters the bridge exactly once: outbound at the VM's port,
inbound at the gateway port. `ext_*` byte counters are normalized to
Ethernet framing to match OVS (§5.5).

Bridged and isolated networks have no split. Their `ext_*` meters are
absent, not zero, so a report can't mistake "not measured" for "no
internet traffic". On a bridged network every port counts toward the
total only. Isolated networks have no path out.

There are two network views, and they answer different questions:

- **Per VM** (`net.*`, per NIC): what the VM's port carried, in plus
  out. The derived meter `net.bytes = rx_bytes + tx_bytes` is what a
  VM or project is billed for.
- **Per network** (`bridge.*`): how much traffic flowed through the
  bridge. Every frame is counted once, on the port where it entered
  the bridge. Ports include VM ports and the uplink (bridged) or the
  bridge's internal gateway port (NAT).

The two views do not have to agree. A frame from VM A to VM B on the
same bridge counts once in `bridge.bytes`, but it counts in both
`net.bytes` of A (tx) and `net.bytes` of B (rx). Each VM pays for what
its own port moved. External traffic adds up across the views: on a
NAT network, `bridge.ext_*` = Σ `net.ext_*` over its NICs, plus
traffic from addresses that are not reserved (§5.5).

## 3. Decisions

| # | Decision | Why |
|---|---|---|
| D1 | The meter runs **inside the control plane** as a set of tokio tasks, not as a separate daemon. | The control plane already knows the identity of every VM, disk and NIC (`status.instance`, `NicState`) and holds the database. A separate daemon would duplicate adoption logic. Kernel and OVS counters are cumulative, so downtime costs no data (§6.4). |
| D2 | Sample **cumulative counters** and store deltas. Never trust rates computed upstream. | Missing a sample only blurs which hour a delta belongs to (§6.3). No usage is lost. |
| D3 | VM CPU and memory come from the **VM unit's cgroup** (`glidex.slice/glidex-vms.slice/glidex-vm@<id>.service`). | It covers the hypervisor and every thread it spawns (vhost, I/O workers), plus the shim. D2 in reconciliation.md exists for this purpose. The control plane can already read it (`ProtectControlGroups=yes` makes it read-only, not hidden). |
| D4 | Disk operation counters come from the **hypervisor** (CH `GET /vm.counters`, QMP `query-blockstats`), per disk. cgroup `io.stat` is a host-side secondary meter. | Guest-visible operations per disk are what users reason about. `io.stat` is per VM, after the page cache and qcow2 metadata I/O, and is useful for capacity planning only. |
| D5 | All network traffic comes from **OVS bridge-port counters** (OVSDB `Interface.statistics`): VM ports for per-VM meters, and every port of the bridge for per-network meters. | It is the only source that covers tap, `dpdkvhostuserclient`, uplink (kernel, AF_XDP, DPDK) and internal ports alike. vhost-user ports have no kernel netdev ([networking.md](networking.md)). One source and one reset rule keep the two views consistent. |
| D6 | **External/internal split on NAT networks only**, from named **nftables counters** on the host's `forward` hook in a separate table, `inet glidex_meter`: one pair per NIC reservation and one pair per bridge. Totals stay on OVS port counters (D5). Bridged and isolated networks are not split. | Every packet that leaves or enters a NAT network is routed by the host. It passes `forward` before SNAT on the way out and after de-SNAT on the way back, so the VM's private address identifies it. Packets that stay on the bridge never reach nftables. Gateway services (DHCP, DNS, ping) hit `input`, not `forward`, so they are internal. A bridged network's uplink traffic is switched in OVS and never seen by the host stack, so splitting it would need per-port OpenFlow rules. |
| D15 | **95th-percentile bandwidth** uses 5-minute slots built from the same counter deltas as the volume meters, and is computed per billing month with the nearest-rank method on max(in, out). Group figures are the p95 of the **summed** slot series, never a sum of p95s. | 5-minute 95th percentile on the larger direction is the industry convention for burstable billing, so customers can check the figures against their own tools. Building slots from the ledger's deltas keeps one source of truth: a slot's bytes always add up to the hour's bytes. The p95 of a sum is what the group actually consumed at once. Summing p95s over-bills, because peaks don't line up. |
| D16 | **Disk I/O uses the 5-minute slot design of D15**, but its billable p95 is on **read+write combined** (total IOPS, total MB/s), not on the larger direction. Average latency is **op-weighted**: Σtime / Σops. | Reads and writes share one device and one queue, and IOPS and throughput caps (cloud disk tiers, `fio` reports, CH and QEMU throttling) are on the total. Network links are full duplex, so there the larger direction is the bottleneck. An average of per-slot averages would let idle slots outweigh busy ones. |
| D17 | **No disk latency for Cloud Hypervisor VMs** until CH exports a cumulative latency counter. QEMU VMs get latency from `*_total_time_ns`. | CH v53's latency fields are a lifetime running mean that stops moving after about 10⁷ requests, plus lifetime min/max (§5.2). Neither gives a correct average for an hour, a 5-minute slot or a billing month. A wrong latency figure is worse than none. |
| D14 | The meter counters live in **their own table**, and netd changes them **incrementally**. It never deletes the table. The NAT table `inet glidex` is still rebuilt atomically, unchanged. | Deleting a table resets its counters. Named counter objects survive `flush chain`. Keeping them out of `inet glidex` means NAT changes never lose usage, so netd needs no "carry the base across rebuilds" bookkeeping. |
| D7 | Network totals come from **bridge ingress** (Σ rx, from the switch's view, over every port), not from Σ of VM NIC meters. | Every frame enters a bridge exactly once, so ingress counts it once. That includes broadcast and VM-to-VM frames. Summing the VM NICs would count VM-to-VM traffic twice. |
| D8 | The ledger is **hourly, UTC, immutable once closed**, with a per-source **cursor** written in the same transaction as the deltas it produced. | Without that one transaction, a crash between "record delta" and "advance cursor" would count the delta twice or never. UTC hours stay simple across DST and time zones. Days and months are roll-ups (§8.3). |
| D9 | Binary units (MiB, GiB) for **volumes**. **Bandwidth rates** are decimal bit/s (kbps = 10³, Mbps = 10⁶ bit/s). | Volumes match the quota fields and `qemu-img`. Bandwidth is sold and measured in decimal bit/s everywhere (links, transit contracts, `iperf`), so a binary "Mbps" would disagree with every other tool. The two are different quantities with different names (GiB vs Mbps), and each one is consistent across every report. |
| D10 | Network traffic is metered in **GiB transferred**, not GiB-hours, and billed as the **total for the billing month** (§8.4). Hourly rows are the atom. | Transfer is a volume, so time-weighting it has no meaning. Hourly rows add up exactly to any month boundary in a whole-hour time zone. |
| D11 | Allocation meters count **from launch to exit**, using the sizing in `launch.json`, not `spec`. `cpu.alloc` stops while the VM is paused. `mem.alloc` does not stop. | `spec` can change at any time, but sizing only takes effect at the next launch ([reconciliation.md](reconciliation.md) §7.3). A paused guest keeps its memory and gives up its CPUs. |
| D12 | The **shim writes a final counter snapshot** into `instance.json` when the hypervisor exits. | When the unit stops, its cgroup and the hypervisor's counters vanish. Without the snapshot, the usage between the last sample and the exit is lost every time, and all of it is lost for a VM that ran entirely while the control plane was down. |
| D13 | Usage records are **attributed to the project that owned the subject when it was sampled** and keep a snapshot of the subject's name. Records outlive the subject. | Deleting a VM must not delete what it used. A later rename must not rewrite history. |

## 4. Architecture

```
          ┌──────────────── glidex-control-plane ─────────────────────┐
          │  Meter (metering/)                                        │
          │   ├── sampler task  ── every sample_secs (30 s) ──┐       │
          │   │     cgroups · hypervisor API · netd · store    │       │
          │   ├── storage task  ── every storage_secs (15 min)│       │
          │   │     qemu-img info (actual-size)                ▼       │
          │   ├── ledger: cursors + open-hour accumulators ─► ReDB    │
          │   └── retention task (daily)                               │
          │  api/usage.rs  GET /usage, /vms/{id}/usage, …/stats       │
          └───────┬─────────────────┬───────────────────┬─────────────┘
                  │ read-only fs     │ api.sock          │ netd.sock
                  ▼                  ▼                   ▼
   /sys/fs/cgroup/glidex.slice/  CH vm.counters /    glidex-netd: port_stats
   glidex-vms.slice/glidex-vm@*   QMP query-blockstats  (OVSDB Interface.statistics),
                                                       nat_counters (nft inet glidex_meter)
```

- `Meter::start` is called from `main.rs` after `start_controllers()`.
  Like the controllers, its tasks go into `VmManager.tasks` and are
  aborted by `stop_controllers`.
- The sampler only reads the store. It never writes VM, disk or network
  records, and it holds no admission locks: it uses a snapshot of the
  cache.
- It writes only the metering tables (§7).
- A sampling round has a time budget of `sample_secs / 2`. If a source
  times out, that subject is skipped for the round, and the next delta
  covers the gap (D2).

## 5. Sources

### 5.1 CPU and memory: the VM cgroup

The path is `/sys/fs/cgroup/<path>`, where `<path>` comes from the
shim's `/proc/<shim_pid>/cgroup` (`0::/glidex.slice/…`). It is not
built by hand, because the slice nesting could change. The shim is the
unit's main process, so this is the same answer as the unit's D-Bus
`ControlGroup` property (M0), without a D-Bus round trip. The pid check
is that the path's last component is this VM's unit
(`glidex-vm@<id>.service`). A reused pid lives elsewhere and is not
read. Reads need no authorization: cgroup files are world-readable.

| File | Field | Meter |
|---|---|---|
| `cpu.stat` | `usage_usec` (also `user_usec`, `system_usec` for the live view) | `cpu.used` |
| `memory.current` − `memory.stat inactive_file` | bytes | `mem.used`, `mem.peak` (working set: reclaimable page cache from buffered disk I/O is left out) |
| `memory.stat` | `anon`, `file` (live view only) | n/a |
| `io.stat` | `rbytes wbytes rios wios` per device, summed | `vmio.*` host-side secondary meters (D4) |

Rules:

- **Hugepages.** A VM with `hugepages` set (`VmConfig.hugepages`) keeps
  guest RAM in `hugetlb`, not `memory.current`. Its `mem.used` is
  `memory.current + mem_size_mib`. Hugepages are reserved in full at
  launch, so "used" equals "allocated" for that part. The `hugetlb`
  controller is not delegated, so it cannot be read directly.
- **Shared memory** (`shared=on`, memfd) is charged to the cgroup that
  touched it first, which is the hypervisor's. No adjustment is needed.
- **`IOAccounting=yes`** goes on the **VM unit template**
  (`glidex-vm@.service`), not on the slice. Verified 2026-10-04 on
  systemd 259:
  - On the slice, it enables `io` only in the *parent's*
    `subtree_control` (`glidex.slice`). The slice's own stays `memory
    pids`, and VM units get no `io.stat`.
  - On a VM unit, it enables `io` in `glidex-vms.slice` for every
    sibling. Running VMs got `io.stat` at once, with no restart.

  So the installer sets it in the template, and applies it to running
  units with `systemctl set-property --runtime <unit> IOAccounting=yes`
  after `daemon-reload` (§14). `MemoryAccounting=yes` goes in the
  template too.

  `CPUAccounting=` is deprecated on this systemd ("ignoring
  assignment"): CPU usage (`cpu.stat usage_usec`) is always available
  on cgroup v2, so nothing is set for it.
- **Detached runner** (dev runs and tests, `Runner::Detached`): there
  is no unit cgroup. CPU comes from
  `/proc/<hypervisor_pid>/stat` `utime+stime` (in ticks; × 10⁶ /
  `CLK_TCK`). Memory comes from `/proc/<pid>/status` `VmRSS`. pid and
  starttime are checked first (`instance/mod.rs`). The record is marked
  `source=proc` (§7.2), because it leaves out the shim and short-lived
  helper processes.

### 5.2 Disk operations: hypervisor counters

New calls in `glidex-hv-client`:

| Backend | Call | Fields → meter |
|---|---|---|
| Cloud Hypervisor | `GET /api/v1/vm.counters` | per device id `_disk<N>`: `read_ops`, `write_ops`, `read_bytes`, `write_bytes`. The `*_latency_*` fields are **not metered** (see "Latency counters" below). |
| QEMU | QMP `query-blockstats` | per `qdev`/node: `rd_operations`, `wr_operations`, `rd_bytes`, `wr_bytes`, `rd_total_time_ns`, `wr_total_time_ns` (`flush_*` not metered). Skip the firmware `pflash` entries (`/machine/system.flash*`). Guest disks are `/machine/peripheral/<id>/virtio-backend`. |

- Map device ids to disk ids through `status.instance.disks`. That
  mapping already records which file each device slot holds. Add the
  hypervisor device id to it if it is not derivable (§14).
- The counters belong to the instance: they start at 0 when the
  hypervisor launches and vanish when it exits. The reset key is
  `instance_id` (§6.2).
- Both APIs are reached over the instance's `api.sock`. They are
  read-only and need no change to the shim. The CH API is single
  threaded, so the call shares the per-instance client's existing
  timeout.
- A **paused** VM still answers. Its counters do not move.
- **Latency counters: QEMU.** `*_total_time_ns` is the time the block
  layer took to complete requests, from submission to completion,
  summed over requests. Divided by ops, that is the average latency
  the guest's requests saw on the host side. It does not include
  queueing inside the guest.
  - Verified (2026-10-04) on the packaged QEMU 10.2.1 with a running
    glidex VM:
    - The fields are present and cumulative, 0 at launch.
    - The root disk gave read 7191 ops / 3.78 s, about 525 µs, and
      write 3781 ops / 2.97 s, about 787 µs.
    - `flush_total_time_ns` is also reported. It is not metered.
- **Latency counters: Cloud Hypervisor. Not metered (D17).** Checked
  against the v53.0 source (`virtio-devices/src/block.rs`,
  `BlockCounters`, `process_queue_complete`, `counters()`) and
  **verified live on a CH v53.0 VM (2026-10-04)**. Per disk,
  `vm.counters` reports `read_latency_{min,max,avg}` and
  `write_latency_{min,max,avg}` in **µs**. None of them can produce a
  per-slot or per-month latency:
  - **`*_avg` is a running mean over the instance's lifetime**, not a
    counter. It is updated per completed request as
    `avg += (lat·10⁴ − avg) / n`, in integer arithmetic, where `n` is
    the lifetime op count.
    - Once `n` is larger than `|lat·10⁴ − avg|`, a request moves the
      mean by 0, so the mean **stops tracking**. At a steady 500 µs
      (5·10⁶ scaled), requests within ±1 ms of the mean stop
      registering after about 10⁷ ops, which is about 5.5 h at
      500 IOPS.
    - The mean is exported truncated to whole µs.
  - **Recovering a total from it doesn't work.** In theory,
    `Δtime = avg₁·n₁ − avg₀·n₀`. In practice the 1 µs truncation is
    multiplied by `n`: with n = 10⁷ the error is about ±10 s per
    sample, against a real Δtime of a few seconds.
  - **Simulation of the exact v53 update rule.** Integer arithmetic,
    truncating toward zero:
    - 10⁷ requests at 500 µs, then 10⁶ at 900 µs: CH reports 500 µs,
      against a true mean of 536 µs. The change was never registered.
    - 10⁶ at 500 µs, then 10⁶ at 900 µs: CH reports 662 µs, against a
      true mean of 700 µs. It is already 5% low.
  - **Live samples** from a 6-minute-old VM, root disk, 30 s apart:
    - first sample: `write_ops` 253, `write_latency_avg` 2752
    - second sample: `write_ops` 257, `write_latency_avg` 2723
    - Reconstructing gives `2723·257 − 2752·253` = 3555 µs over 4
      writes, about 889 µs each. The 1 µs truncation contributes
      ±510 µs (±14%) already at about 500 ops, and grows linearly
      with `n`.
    - The second disk had no writes yet. It showed the sentinels
      `write_latency_min/max` = 18446744073709551615 and
      `write_latency_avg` = 1844674407370955, exactly as the source
      predicts.
  - **`*_min` / `*_max` are lifetime extremes.** They are never reset,
    so they say nothing about a slot or a month.
  - **Before the first request** each field is the sentinel `u64::MAX`.
    `*_avg` shows it as `1844674407370955`, after the ÷10⁴. A reader
    must treat that as "no data".
  - **The interval measured** runs from request parse
    (`Request::start`, `block/src/io/request.rs`) to completion. That
    is comparable to QEMU's.

  So for CH disks the `*_time_ns` meters are **absent**, not zero.
  `latency_source` is `none`, and the API, CLI and UI show latency as
  "not available (Cloud Hypervisor)". IOPS, MB/s, peaks and p95 are
  unaffected: `read_ops`, `write_ops`, `read_bytes` and `write_bytes`
  are ordinary cumulative counters. `GET /vms/{id}/stats` may show
  CH's lifetime `*_latency_avg` as an "instance lifetime average"
  for information only, but it is never stored. The fix is upstream
  (§16).
- **Peaks** (`*_iops_peak`, `*_kBps_peak`) are `Δcounter / Δt` over one
  sample interval, per direction and for the sum. Their resolution is
  `sample_secs`: a 1-second burst is averaged over 30 s. They are
  labelled "30-second peak", and intervals after a gap are ignored
  (§8.5).
- **Instance exit.** The shim's exit snapshot (D12) carries every
  counter in this table, including the `*_time_ns` counters.

### 5.3 Disk storage

- `disk.alloc` is `Disk.size_bytes` for every disk whose record exists
  and whose `phase` is `Ready | Resizing | Missing`, whether or not it
  is attached and whether or not the VM is running. Storage is reserved
  while the disk exists. `Pending`, `Creating` and `Failed` disks are
  not charged.
- `disk.stored` is `actual-size` from `qemu-img info`. It is run every
  `storage_secs` (default 15 min), not every sample, because it opens
  the file. Between runs the last value is held. A linked disk's stored
  size is its overlay only. Base images are a shared library
  ([security.md](security.md)) and are metered once as host-level
  `image.stored`, with no project.
- A resize changes `size_bytes`. The sampler sees the new value at its
  next round, so the change takes effect with up to `sample_secs` of
  lag. That error is accepted.
- Unmanaged `rootfs_path` disks are not metered, just as they are not
  under quota.

### 5.4 Network: bridge-port counters

One new **read-only** netd op, `port_stats`. It is added to the
read-only set in `proto.rs` and allowed on `netd.sock` for the
`glidex` group. It is **not** exposed on `netd-ro.sock`, because
traffic volumes are tenant data. Totals need no nftables or OpenFlow
changes. Only the NAT external split adds counters (§5.5).

`port_stats` makes one `ovs-vsctl list Interface` call with columns
`_uuid, name, type, external_ids, statistics` and returns every
Interface on a glidex-managed bridge (`glidex-ovs` bridges, from
`list_bridges`), grouped by bridge:

```json
{ "bridges": [
  { "bridge": "gxbr-…", "network": "<network id>",
    "ports": [
      { "uuid": "…", "name": "gx1a2b3c4d-0", "role": "vm", "vm_id": "…", "nic": 0,
        "rx_bytes": …, "tx_bytes": …, "rx_packets": …, "tx_packets": …,
        "rx_dropped": …, "tx_dropped": … },
      { "uuid": "…", "name": "gxbr-…", "role": "gateway", … },
      { "uuid": "…", "name": "enp3s0", "role": "uplink", … } ] } ] }
```

- **Roles.** `vm` comes from `external_ids:glidex-role=vm`
  (`vm_port.rs`). `uplink` is the bridged network's uplink port.
  `gateway` is the bridge's internal port, which holds the NAT gateway
  address. Isolated networks have only `vm` ports.
- **Counter direction depends on the port type.** netd returns them
  as OVS reports them, and the meter maps them per role:

  | Port | OVS `rx` means | Guest/bridge mapping |
  |---|---|---|
  | `vm` (tap; vhost-user assumed the same, unverified) | switch received from the guest | guest tx = `rx`, guest rx = `tx` |
  | `gateway` (`type=internal`) | **host** received from the bridge (host view, reversed) | bridge ingress = `tx` |
  | `uplink` (system NIC, AF_XDP, DPDK; assumed switch view, unverified) | switch received from the wire | bridge ingress = `rx` |

  - **Verified 2026-10-04** (OVS on the test host, NAT bridge
    `gxbr-nat`):
    - 20 pings of 1400 bytes from the host to a VM added about 28.9 KB
      to the VM port's `tx` (switch → guest) and to the internal
      port's **`tx`** (host → bridge).
    - The replies added to the VM port's `rx` and the internal port's
      `rx`.
    - Over the bridge's lifetime, the internal port's `rx` (1.77 MB)
      equals the sum of the VM ports' `rx` (1.74 MB, what the guests
      sent towards the gateway).
  - **Per network (`bridge.*`):** frames entering the bridge (D7) =
    Σ `vm` `rx` + `gateway` `tx` + Σ `uplink` `rx`. Frames OVS drops
    before forwarding are included. That is acceptable, because they
    crossed a port.
- **Freshness.** ovs-vswitchd refreshes `statistics` every 5 s
  (`other_config:stats-update-interval`), which is finer than the
  sampling period.
- **Per-port reset key** is the Interface `_uuid` (§6.2). A detached
  and re-attached VM port, a re-created uplink, an OVS restart that
  re-creates rows, or a host reboot each give a new `_uuid`, and the
  counters restart from 0. A per-network meter is the sum of
  per-port cursors, so the reset of one port does not affect the
  others.
- **Ports that leave.** A VM port's counters stay readable until
  `detach_vm_port` or `release_vm` removes the row. The VM controller
  asks the meter for a final sample of the VM's ports first (§6.4).
  The same applies to uplink changes (`commit_uplink`,
  `delete_uplink`) and `delete_bridge`. Those are rare admin
  operations, so a final sample is taken best-effort. If it is missed,
  the uncounted tail is at most `sample_secs` of traffic.
- **Attribution.** `NicStatus.network` (`models.rs`) gives the NIC's
  network. A per-VM NIC row records `network` too, so per-VM traffic
  can be grouped by network (`group_by=network,vm`). Network rows
  carry the network's project, and host-level (admin) networks carry
  none.
- **Shared bridges.** Several networks can share one bridge, each with
  its own VLAN tag on its VM ports. The bridge totals are then
  divided:
  - Each network gets the ingress of **its own VM ports**.
  - The shared ports (the uplink, or the gateway) go to a host-level
    subject `network/bridge:<bridge>`, with no project.
  - A bridge that no network uses is metered the same way.
  - The per-network figure is still exact for what the VMs sent. What
    arrived through the shared uplink can't be attributed to a VLAN
    without per-port OpenFlow rules (§16).
- **Implementation:** `glidex-ovs` `stats::bridge_stats` (three
  `ovs-vsctl list` calls: Bridge, Port, Interface), netd
  `Op::PortStats`, and `metering/net.rs`. Every port's counters have
  their own cursor (a ledger *part*) inside the network's
  `bridge.bytes`, keyed by the Interface `_uuid`. Parts of ports that
  are gone are pruned.

### 5.5 NAT external counters: `inet glidex_meter`

netd owns a second nftables table, separate from the NAT table
`inet glidex` that `nat.rs::nft_script` rebuilds:

```nft
table inet glidex_meter {
  counter m_<mac hex>_out { }        # guest → beyond the gateway   (ext_tx)
  counter m_<mac hex>_in  { }        # beyond the gateway → guest   (ext_rx)
  counter br_<bridge>_out      { }     # whole network, out           (bridge.ext_tx)
  counter br_<bridge>_in       { }     # whole network, in            (bridge.ext_rx)

  chain forward {
    type filter hook forward priority filter + 10; policy accept;
    iifname "<br>" counter name "br_<bridge>_out"
    oifname "<br>" counter name "br_<bridge>_in"
    iifname "<br>" ether saddr <vm mac> ip saddr <reserved ip> counter name "m_<mac hex>_out"
    oifname "<br>" ip daddr <reserved ip> counter name "m_<mac hex>_in"
  }
}
```

- **Priority `filter + 10`** puts the chain after `inet glidex`'s
  forward chain and after iptables/Docker filter chains at priority 0.
  Packets that the NAT isolation rules or a host firewall drop are
  never counted. The rules have no verdict, so they can't change
  forwarding.
- **Attribution keys.**
  - Outbound is keyed on the NIC's MAC **and** its DHCP reservation
    (`NatState.reservations`). The reservation is fixed for the VM's
    lifetime ([networking.md](networking.md)).
  - Inbound packets have already been de-SNATed when they reach
    `forward`, so `ip daddr` is the private address. Their source MAC
    is the gateway's, so they are keyed on the address only.
  - A guest that uses an address it was not given still shows up in
    `br_*` but not in any `vm_*`. The difference is reported per
    network as `bridge.ext_unattributed_bytes` (derived).
- **Naming.** Per-NIC counters are named by the reserved **MAC**
  (`m_<12 hex>_<in|out>`), because netd's reservations are keyed by
  MAC and the VM id can't be recovered from it. The control plane maps
  MAC → (VM, NIC) through `status.nics[].mac`. Bridge names are
  `[a-z0-9-]`, so `br_<bridge with - → _>` can't collide. The reset
  key is the counter's object handle plus the host's boot id, because
  a table re-created after a reboot can reuse handle numbers.
- **Unattributed traffic on the VM's own port.** Traffic from an
  address the VM was not given still crosses the VM's switch port. So
  it is in that NIC's `net.rx_bytes`/`net.tx_bytes` but not in its
  `net.ext_*`, and appears as *internal* for that VM. It is counted as
  external only at network level (`bridge.ext_*`, unattributed).
  Measured end to end: 10 MiB fetched from an unreserved source added
  10.23 MiB to the network's external traffic and none to the VM's.
- **Spoofing.** A guest that forges a neighbour's MAC and IP makes the
  neighbour pay. It also breaks the neighbour's connectivity (ARP and
  MAC learning conflicts), so this is noticeable. Port security, i.e.
  OVS rules that only accept a VM port's own MAC and IP, would close
  it. That is listed in §16.
- **Changes are incremental (D14).**
  - When a reservation is added, netd runs `add counter` (idempotent)
    and then rebuilds the chain: `flush chain` and re-add the rules,
    in one `nft -f` transaction. Named counters survive the flush.
  - On `release_vm`, netd deletes the VM's counters after the meter
    has taken its final sample (§6.4).
  - On `delete_nat`, netd deletes that bridge's counters, after the
    network controller has asked the meter for a final sample.
  - Nothing ever runs `delete table inet glidex_meter`, except
    `uninstall`.
- **Byte normalization.** nft counts L3 bytes (IP header + payload).
  OVS counts Ethernet frames. Without correction, internal =
  total − external would be overstated by 14 bytes per external
  packet. That is about 1% for full-size packets and up to 25% for
  small ones. So netd reports `ext bytes = nft bytes + 14 × nft
  packets`, which makes both meters count the same unit. VLAN tags on
  NAT bridges don't apply: NAT ports are untagged.
- **IPv4 only**, like NAT networks ([networking.md](networking.md)).
  If NAT gains IPv6, add the same rules with `ip6 saddr`/`ip6 daddr`.
- **The `nat_counters` op.** It is read-only and on `netd.sock` only
  (not `netd-ro.sock`), like `port_stats`. It runs `nft -j list
  counters table inet glidex_meter` and returns
  `[{bridge, network, vm_id?, nic?, dir: in|out, bytes, packets,
  handle}]`, with bytes already normalized. The counter object's
  `handle` is its reset key (§6.2). A counter that is re-created after
  a host reboot, an nftables flush by an admin, or netd re-applying
  state on start gets a new handle and starts from 0.
- **Host check (M0, done 2026-10-04).** `ether saddr` matches in an
  `inet` `forward` chain for packets routed in from the bridge's
  internal port, and named counters survive `flush chain` (§15.3).

### 5.6 Lifecycle: phases and time

`vm.running`, `vm.paused` and the allocation gauges need the VM's phase
at each sample. That is `status.phase` and `status.instance`, read from
the cache. Pause and resume currently write no event (`controller/vm.rs`
`running_round`). The meter does not need one: it samples the phase.
To get transitions exact to the second rather than to `sample_secs`,
the controller sets `status.phase_since` (§14).

## 6. Collection algorithm

### 6.1 Sampling round

Every `sample_secs` (default 30, range 10–300):

```
now ← clock (UTC, ms)
for each VM with status.instance (phase ∈ Starting|Running|Paused|Stopping):
    read cgroup (§5.1), hypervisor counters (§5.2)
for each Ready/Resizing/Missing disk: read size_bytes (§5.3)
netd: port_stats, nat_counters                      # one call each
for each (subject, meter):
    delta ← counter_delta(cursor, reading)           # §6.2
    spread delta over [cursor.at, now] into hour buckets   # §6.3
    gauges: value(cursor.at) × (now − cursor.at), same spread
one write transaction: update open-hour accumulators + all cursors
```

**Invariant (exactly once, D8).** A delta and the cursor that consumed
it are committed in one ReDB transaction. If the commit fails, both are
discarded and the next round recomputes from the old cursor.

### 6.2 Counter deltas and resets

A cursor is `{subject, meter, reset_key, value, at}`. Reset keys:

| Source | reset_key |
|---|---|
| cgroup | `instance_id` + the shim's start time: one unit invocation, since the shim is its main process |
| `/proc` (detached) | `instance_id` + hypervisor pid + starttime |
| hypervisor counters | `instance_id` |
| OVSDB port stats (VM, uplink, gateway ports) | OVS Interface `_uuid`. A re-created port is a new interface. |
| NAT counters (`inet glidex_meter`) | nft counter object `handle` (+ host `boot_id`) |

```
if reading.reset_key == cursor.reset_key and reading.value >= cursor.value:
    delta = reading.value - cursor.value
elif reading.reset_key == cursor.reset_key:          # went backwards: unexpected reset
    delta = reading.value; flag "reset"
else:                                                # new instance / new port
    delta = final_of(cursor) - cursor.value  (if known, §6.4)  +  reading.value
```

A new cursor that has no predecessor (the first sample of a new
instance) counts `reading.value` in full. Counters start at 0 at launch
and are created only after glidex creates the subject, so nothing from
before is double-counted.

### 6.3 Splitting a delta across hours

A delta observed over `[t₀, t₁]` is split across the UTC hours it
spans, in proportion to time. With 30 s samples that almost never
spans an hour boundary, except after a gap (§6.4). Then the delta is
spread linearly and every bucket it touches is flagged `interpolated`.
`max` meters are not interpolated: a peak computed over a gap is
written only to the hour containing `t₁` and is flagged.

### 6.4 Gaps: control-plane downtime and instance exits

- **Same instance still live after the gap.** The counters kept going.
  The first round after restart computes one big delta and spreads it
  (§6.3). Gauges such as `mem.used` are held at their last value across
  the gap and flagged `interpolated`. `cpu.alloc` and `mem.alloc` are
  exact, because sizing did not change within the instance.
- **Instance exited during the gap, or between two samples.** The
  cgroup and the hypervisor are gone. The shim's **exit snapshot**
  (D12) in `instance.json` → `exit.usage` has:
  - `{cpu_usage_usec, memory_peak_bytes, io: {...}}`, read from its own
    cgroup right after reaping the hypervisor, when the cgroup still
    holds the hypervisor's charged usage;
  - per-disk hypervisor counters from its last successful poll. The
    shim polls `vm.counters` or `query-blockstats` every `sample_secs`.
    This is the only new periodic work in the shim.

  The meter consumes the snapshot through `ExitRecord` when the VM
  controller records the exit. The snapshot carries `instance_id`, so
  it is consumed once. An exited VM's port counters stay in its OVSDB
  row until the port is detached, and its NAT counters stay until
  `release_vm`. The VM controller asks the meter for a final
  sample of a VM's ports **before** it calls `detach_vm_port` or
  `release_vm` (§14).
- **Host reboot** (`ExitCause::HostReboot`). No final snapshot exists
  if the host lost power. The usage since the last sample is lost. The
  hour is flagged `incomplete` with `lost_secs`. This is the one
  accepted loss, and it is bounded by `sample_secs` (or by downtime if
  the control plane was down too).
- **Allocation time during a gap** comes from instance timestamps, not
  samples: `launched_at`, `ExitRecord.at` and `phase_since`. So
  vCPU-hours and memory-allocation hours are exact even across
  downtime.

### 6.5 Closing an hour

An hour `[h, h+1)` is **closed** once a round completes after
`h + 1 + close_grace_secs` (default 120). Its accumulators are then
written as immutable `usage_hourly` rows (§7.2) and dropped from the
open set. After that, a late delta, such as one consumed from an exit
snapshot after a long downtime, is written to the hour it spreads into
as an **adjustment row** (`seq > 0`) and never by rewriting. Readers
sum rows, so totals are correct, and the history of corrections is
kept.

## 7. Data model

### 7.1 Tables

The tables are in the shared ReDB database (`~/.glidex/glidex.db`),
serde-JSON values, and declared in `metering/ledger.rs`:

| Table | Key | Value |
|---|---|---|
| `meter_cursors` | `<subject_kind>/<subject_id>/<meter>` | `Cursor` (§6.2) |
| `meter_open` | `<hour>/<subject_kind>/<subject_id>` | `HourAcc`: accumulators of the open hours |
| `usage_hourly` | `<hour:010>/<project>/<subject_kind>/<subject_id>/<seq:04>` | `UsageRecord` (§7.2) |
| `usage_daily` | `<day>/<project>/<subject_kind>/<subject_id>` | `UsageRecord` summed (M2; written when a day closes) |
| `usage_monthly_rates` | `<YYYY-MM>/<project>/<subject_kind>/<subject_id>` | `BandwidthMonth`: final p95/avg/peak for a billing month (§8.5) |
| `rate_5m` | `<hour:010>/<subject_kind>/<subject_id>` | `SlotRow`: the hour's twelve 5-minute slots (§8.5) |
| `meter_meta` | `schema`, `last_round`, `retention_done` | small values |

`hour` is the hour's start in Unix seconds, zero-padded so that keys
sort by time. The key order makes the main query, "project P over
`[from, to)`", a range scan on hour followed by a filter on project.
A secondary index `usage_by_project`
(`<project>/<hour>/<kind>/<id>/<seq>` → `()`) is added only if
profiling shows the scan matters. At about 50 VMs × 3 subjects × 24 h
that is 3.6k rows a day.

`SlotRow` holds **per-slot deltas**, not rates. Rates are derived when
read, so no rounding is stored. Byte and time fields are `u64`: at
100 Gbit/s a 5-minute slot is about 3.75 TB, which overflows `u32`.
There are two kinds of row:

- **NIC and network:** `{slots_present: u16 bitmask, rx: [u64; 12],
  tx: [u64; 12], ext_rx: Option<[u64; 12]>, ext_tx: Option<[u64;
  12]>, flags}`. Network rows use `rx` for bridge ingress and leave
  `tx` empty.
- **Disk:** `{slots_present, attached: u16 bitmask, read_ops: [u32;
  12], write_ops: [u32; 12], read_bytes: [u64; 12], write_bytes: [u64;
  12], read_time_ns: Option<[u64; 12]>, write_time_ns: Option<[u64;
  12]>, flags}`. Disk rows are keyed
  `<hour>/disk/<disk_id>@<vm_id>`, so a disk that moves to another VM
  within an hour has one row per VM. Disk-level figures merge them,
  and VM-level figures take only that VM's rows.

A row is a few hundred bytes of JSON per subject-hour: about 200–300
KiB per NIC or disk per month.

This is a new table set, so `SCHEMA_VERSION` does not change. The
tables are created on first open. Downgrading leaves them unread.

### 7.2 `UsageRecord`

```rust
pub struct UsageRecord {
    pub hour: u64,                 // UTC hour start, unix seconds
    pub project: String,           // project id at sample time (D13)
    pub subject: Subject,          // {kind: Vm|Disk|Nic|Network|Image, id, name, vm_id, nic, network: Option}
    pub seq: u16,                  // 0 = closing row, >0 = adjustment (§6.5)
    pub meters: BTreeMap<String, u64>,  // meter name → value in base unit (§2)
    pub flags: Vec<Flag>,          // interpolated | reset | incomplete{lost_secs} | source_proc
    pub hypervisor: Option<Hypervisor>, // VM rows: CH | QEMU, for rate cards
    pub written_at: u64,
}
```

Keeping `meters` as a map lets a new meter be added without a
migration. Unknown meters pass through the API unchanged.

### 7.3 Retention

`metering.retention_days` defaults to 400 (13 months, so a year can be
compared with the year before). The range is 1–3650. A daily task
deletes `usage_hourly` rows older than that, after M2 has rolled them
into `usage_daily`. Daily rows are kept for `retention_daily_days`
(default 1825). Cursors and open accumulators for subjects that no
longer exist are deleted once their last hour is closed.

`rate_5m` rows are kept for `retention_rate_days` (default 100, range
35–400). That is long enough to recompute the current and the previous
billing month's 95th percentile, and to answer a dispute about an
invoice. The p95 results themselves are not lost when slots expire:
each closed billing month's per-subject p95 figures are written to
`usage_monthly_rates` (`<month>/<project>/<kind>/<id>` → `BandwidthMonth`,
§8.5) when the month becomes final. They are kept as long as
`usage_daily`.

## 8. Aggregation and queries

### 8.1 Dimensions

`group_by` ⊆ {`project`, `vm`, `disk`, `nic`, `network`,
`hypervisor`} × `granularity` ∈ {`hour`, `day`, `month`}. Granularity
is in UTC by default. `tz=<IANA>` regroups the hourly rows into local
days and months at query time, which works because hours are the atom.

### 8.2 Combining

Counters and gauge×time meters are summed. `max` meters take the max.
Average rates (`disk.iops_avg`, `net.pps_avg`) are derived as
`Σops / Σ seconds the subject existed in range`, where that time comes
from `vm.running` for VM-bound meters.

### 8.3 Presentation units

| Base | Presented | Conversion |
|---|---|---|
| µs CPU | core-hours | ÷ 3.6·10⁹ |
| vCPU·s | vCPU-hours | ÷ 3600 |
| MiB·s | MiB-hours / GiB-hours | ÷ 3600 / ÷ (3600·1024) |
| bytes | GiB | ÷ 2³⁰ |

The API returns both: `raw` (integers) and `value` (decimals rounded to
6 places). Consumers should do billing arithmetic on `raw`.

### 8.4 Billing month

The billing figure for network traffic is **GiB transferred in the
billing month**. It is computed the same way for per-VM `net.bytes`,
per-project `net.bytes` (Σ over the project's NICs) and per-network
`bridge.bytes`.

- **Billing month:** a calendar month in `metering.billing_timezone`
  (default `UTC`), `[first 00:00, next first 00:00)`.
  `GET /usage?granularity=month` uses it unless `tz=` is given.
- **Time zones:** only zones whose offset is a whole number of hours
  all year are accepted (for example `Asia/Bangkok`, +07:00). Hourly
  rows then fall wholly inside one month and the sum is exact. Zones
  with DST or half-hour offsets are refused at config load, because an
  hour row would straddle a month boundary.
- **Sum:** the month total is the sum of `raw` bytes over the month's
  `usage_hourly` rows, including adjustment rows (§6.5), converted to
  GiB once at the end. Hourly GiB values are never rounded and then
  summed.
- **Final:** a month is final once `complete_through` passes its end.
  An adjustment that arrives later is still added to the month it
  belongs to. The API marks such a month `revised_at`, so an invoice
  already issued can be reconciled against it.
- **NAT networks:** the month row also has `ext_bytes` and
  `internal_bytes` (derived as in §2 from the month's raw sums, not
  per hour), so a rate card can price internet traffic separately.
- **Partial months:** `meter_meta.started_at` records when metering
  first ran. A month that began before it, or that contains a period
  with `enabled: false`, is returned with `partial: true` (§15.1).
- **Month-to-date:** `GET /usage?granularity=month` for the current
  month returns closed hours plus the open hour as provisional. The UI
  shows it as "month to date".

### 8.5 Bandwidth: average, peak and 95th percentile

All bandwidth figures come from the same byte counters as the volume
meters. There is no separate collector. Units are decimal (D9): 1 Mbps
= 10⁶ bit/s.

| Figure | Definition | Resolution |
|---|---|---|
| **Average** `mbps_avg` | `bytes × 8 / seconds / 10⁶`, per direction. Seconds are the range ∩ the time the subject existed: a NIC from attach to detach, a network from create to delete. | exact, any range |
| **Peak** `mbps_peak` | max of `*_kbps_peak` over the range. Each sample is `Δbytes × 8 / Δt` between two consecutive samples. | one sample interval (30 s): a "30-second peak" |
| **95th percentile** `mbps_p95` | §8.5.1 | 5-minute slots |

A burst shorter than the sample interval is averaged over that
interval: a 2-second line-rate burst shows as about 1/15 of line rate
at 30 s. The figures are labelled "30-second peak" and "5-minute 95th
percentile" everywhere they appear. Burst detection below that
resolution is monitoring, not metering, and is out of scope (§1).
Samples after a gap (§6.3) are spread out, so they can't produce a peak:
`kbps_peak` ignores intervals longer than `2 × sample_secs`.

#### 8.5.1 95th percentile (D15)

- **Slots.** Each subject has fixed 5-minute slots aligned to the
  epoch (so to every hour). Counter deltas are split into slots the way
  they are split into hours (§6.3), and in the same transaction. A
  slot's bytes are part of its hour's bytes, so the slots of an hour
  sum exactly to the hour's `net.*` / `bridge.*` volume.
  `sample_secs` must divide 300 (§11), so samples land on slot
  boundaries apart from scheduling jitter, which proportional splitting
  absorbs.
- **Slot rate** = `slot bytes × 8 / 300` bit/s, per direction:
  - NIC: `rx` and `tx`; on NAT networks also `ext_rx` and `ext_tx`.
  - Network: bridge ingress, a single series; on NAT networks also
    `ext_rx` and `ext_tx`.
- **Which slots count.** For a NIC, only the slots in the billing
  month during which it existed: from attach to detach, including the
  time its VM was stopped (rate 0). For a network, from create to
  delete. A NIC attached for 3 days is not diluted by 27 days of zeros
  it was never present for. Slots inside a control-plane gap hold
  interpolated (flat) rates. They count, and the result is flagged
  `interpolated` with the number of affected slots. Slots lost to a
  host reboot (§6.4) count as 0 and are flagged `incomplete`.
- **Computation**, nearest rank: sort the N slot rates ascending, then
  p95 = value at 1-based rank `⌈0.95 × N⌉`. This drops the top 5%
  (about 36 hours in a 30-day month). Per subject, report
  `rx_p95`, `tx_p95` and **`billable_p95 = max(rx_p95, tx_p95)`**,
  which is the convention. NAT subjects also report `ext_rx_p95`,
  `ext_tx_p95` and `ext_billable_p95`. A network's total has a single
  series, so its `billable_p95` is the p95 of bridge ingress.
- **Groups** (VM = its NICs, project = its NICs, project's external
  traffic). Add the members' rates **slot by slot**, per direction,
  then take the p95 of the summed series (D15). The membership of a
  slot is whoever existed in it. A project's billable p95 is
  `max(p95(Σrx), p95(Σtx))`.
- **Billing month** is the same one as for volumes (§8.4):
  `billing_timezone` has whole-hour offsets, so 5-minute slots never
  straddle a month boundary. Slot counts per month are 8064 to 8928.
- **Finality.** When a billing month becomes final (§8.4), the meter
  writes a `BandwidthMonth` per NIC, VM, network and project:
  `{p95 per direction, billable_p95, avg, peak, slots_counted,
  slots_interpolated, slots_incomplete}`. A later adjustment row
  (§6.5) also updates the slots it touches and recomputes the affected
  `BandwidthMonth` rows, stamped with `revised_at`, as for volumes.

### 8.6 Disk I/O: IOPS, throughput and latency (D16)

These use the same machinery as network bandwidth (§8.5, §8.5.1): the
hypervisor counter deltas (§5.2) are split into the disk's 5-minute
slots, in the same transaction as the hourly rows. The slots of an
hour add up exactly to the hour's `disk.*` counters.

| Figure | Definition |
|---|---|
| **Average IOPS** | `Σops / seconds` per direction and total, over the seconds the disk was **attached** in the range |
| **Average MB/s** | `Σbytes / seconds / 10⁶`, as above |
| **Average latency** | `Σtime_ns / Σops / 10⁶` ms per direction, op-weighted (D16). A range with no ops has no latency (`null`), not 0. |
| **30-second peaks** | `disk.read_*_peak`, `disk.write_*_peak` and the combined `disk.*_peak` for IOPS and MB/s (§5.2) |
| **95th percentile** | per slot, `IOPS = ops / 300` and `MB/s = bytes / 300 / 10⁶`, for read, write and **total**. `p95` uses nearest rank as in §8.5.1. |
| **Billable** | `billable_iops_p95 = p95(read_ops + write_ops)` and `billable_mbps_p95 = p95(read_bytes + write_bytes)`. These are p95s of the **total** series per slot, not the sum of the read and write p95s. |
| **Slot latency** | `time_ns / ops` per slot, for graphs. There is no p95 of latency: one value per slot is an average, so its percentile would be misleading. A real latency percentile needs histograms (§16). |

- **Which slots count.** Only slots in which the disk was **attached
  to a VM** (`attached` bitmask). They count whether or not the VM was
  running, as 0 when stopped, the same rule as NICs. Time a disk spent
  detached is not counted, so a spare disk isn't watered down by
  months of zeros. Interpolated and incomplete slots are handled and
  flagged as in §8.5.1.
- **Groups.**
  - **VM:** its disks' rows for that VM, added slot by slot.
  - **Project:** all its disks, added slot by slot.
  - p95 is taken on the summed series (D15). Average latency for a
    group is `Σtime_ns / Σops` over all members, so busy disks weigh
    more.
- **What one op is.** One op is one guest request as the hypervisor
  saw it. Requests the guest merged count once, and a 1 MiB request
  counts the same as 4 KiB. That is why IOPS and MB/s are both
  reported, and why `billable_*` exists for each.
- **Finality.** When a billing month becomes final, a `DiskIoMonth` is
  written per disk, VM and project:
  `{avg (iops, mbps, latency_ms per direction), peak (iops, mbps per
  direction and total), p95 (read, write, billable for iops and mbps),
  slots_counted, slots_interpolated, slots_incomplete,
  latency_source: counter|none}`. It goes into
  `usage_monthly_rates` next to `BandwidthMonth`, with the same
  `revised_at` rule.
- **Retention:** `rate_5m` disk rows follow `retention_rate_days`.

**As built (M2.3).** Code: `metering/rates.rs`,
`metering/retention.rs`, and `ledger.rs` (`rate_5m`, `usage_daily`,
`usage_monthly_rates`). It differs from §8.5.1 and §8.6 in four ways:

- **A slot counts when the subject was sampled in it**, i.e. while
  its instance was running. A stopped VM's NIC port is detached on
  this system, and its disks have no hypervisor counters, so
  "attached but stopped" has no reading to count. Such time is left
  out rather than counted as 0. That lowers neither p95 nor averages
  for time the VM was off.
- **Final figures are written once.** When a billing month is
  complete, the daily upkeep writes one figure per grouping to
  `usage_monthly_rates` (`bw/{nic,vm,project,network}`,
  `io/{disk,vm,project}`). While a month's slots are still kept
  (`retention_rate_days`), queries compute from the slots, so a late
  adjustment shows up there. The stored figure is what remains after
  the slots expire. It has no `revised_at`.
- **Hours roll up into UTC days** after `retention_days`, and `scan`
  returns those day rows for old ranges.
- **Upkeep runs once a day**, after a sampling round: finalize, roll up,
  then expire slots, days and monthly figures.

## 9. REST API

All routes are added in `api/mod.rs` with one Cedar action each (§10).

### 9.1 `GET /usage`

Query: `project` (id or name; repeatable; omitted means every project
the caller may read), `from`, `to` (RFC 3339 or Unix seconds, rounded
to hours, `to` exclusive, maximum span 400 days), `granularity`,
`group_by`, `meters` (comma list; default all), `format=json|csv`.

```json
{
  "from": "2026-10-01T00:00:00Z", "to": "2026-10-02T00:00:00Z",
  "granularity": "day", "group_by": ["project", "vm"],
  "rows": [
    { "start": "2026-10-01T00:00:00Z", "project": "p-…", "vm": {"id": "…", "name": "web-1"},
      "meters": {
        "cpu.used":        {"raw": 51840000000, "value": 14.4,  "unit": "core-hours"},
        "cpu.alloc":       {"raw": 172800,      "value": 48.0,  "unit": "vCPU-hours"},
        "mem.alloc":       {"raw": 353894400,   "value": 98304, "unit": "MiB-hours"},
        "net.bytes":         {"raw": 3221225472, "value": 3.0,  "unit": "GiB"},
        "net.ext_bytes":     {"raw": 2147483648, "value": 2.0,  "unit": "GiB"},
        "net.internal_bytes":{"raw": 1073741824, "value": 1.0,  "unit": "GiB"}
      },
      "flags": ["interpolated"] }
  ],
  "complete_through": "2026-10-04T09:00:00Z"
}
```

`complete_through` is the end of the last closed hour. Rows after it
are provisional (still open). Errors use the standard
`{error, message, details}` format. A span that is too long gives
`400 range_too_large`.

### 9.2 Per-subject shortcuts

`GET /vms/{id}/usage`, `GET /disks/{id}/usage` and
`GET /projects/{id}/usage` take the same query and are pre-filtered.
A VM's usage includes its NICs. It includes disks only with
`include=disks`, because a disk can move between VMs and is billed as
a disk.

### 9.3 Bandwidth

**`GET /usage/bandwidth`**: billing-month bandwidth.

Query: `project` (as in §9.1), `month=YYYY-MM` (default: the current
billing month, provisional until final) or `from`/`to` (whole hours,
maximum 100 days, within `retention_rate_days`), `group_by` ⊆
{`project`, `vm`, `nic`, `network`}, `format=json|csv`.

```json
{
  "month": "2026-10", "timezone": "Asia/Bangkok", "final": false,
  "rows": [
    { "project": "p-…", "vm": {"id": "…", "name": "web-1"},
      "avg":  {"rx_mbps": 12.4,  "tx_mbps": 48.1},
      "peak": {"rx_mbps": 310.2, "tx_mbps": 870.5, "resolution_secs": 30},
      "p95":  {"rx_mbps": 40.3,  "tx_mbps": 155.0, "billable_mbps": 155.0,
               "ext_rx_mbps": 38.9, "ext_tx_mbps": 150.7, "ext_billable_mbps": 150.7},
      "slots": {"counted": 8928, "interpolated": 12, "incomplete": 0} }
  ]
}
```

Closed months come from `usage_monthly_rates`, and open months from
`rate_5m`. `GET /usage` also accepts the derived meters
`net.mbps_avg`, `net.mbps_peak`, `bridge.mbps_avg` and
`bridge.mbps_peak`, plus their `ext` forms, at any granularity.

**`GET /vms/{id}/bandwidth`** and **`GET /networks/{id}/bandwidth`**
return the 5-minute series for graphs: `from`/`to` with a maximum of
31 days, and `[{slot, rx_mbps, tx_mbps, ext_rx_mbps?, ext_tx_mbps?}]`.
`p95=true` adds the p95 lines over the range.

**`GET /usage/disk-io`**: billing-month disk I/O. It takes the same
query as `/usage/bandwidth`, with `group_by` ⊆ {`project`, `vm`,
`disk`}.

```json
{
  "month": "2026-10", "timezone": "Asia/Bangkok", "final": false,
  "rows": [
    { "project": "p-…", "disk": {"id": "…", "name": "db-data"}, "vm": {"id": "…", "name": "db-1"},
      "avg":  {"read_iops": 420, "write_iops": 180, "read_mbps": 6.9, "write_mbps": 3.1,
               "read_latency_ms": 0.42, "write_latency_ms": 1.10},
      "peak": {"read_iops": 5200, "write_iops": 2100, "iops": 6800,
               "read_mbps": 210.0, "write_mbps": 95.0, "mbps": 280.0, "resolution_secs": 30},
      "p95":  {"read_iops": 1450, "write_iops": 610, "billable_iops": 1980,
               "read_mbps": 31.0, "write_mbps": 12.5, "billable_mbps": 41.2},
      "latency_source": "counter",
      "slots": {"counted": 8928, "interpolated": 0, "incomplete": 0} }
  ]
}
```

**`GET /disks/{id}/io`** and **`GET /vms/{id}/io`** (all the VM's
disks, each separately and summed) return the 5-minute series:
`[{slot, read_iops, write_iops, read_mbps, write_mbps,
read_latency_ms?, write_latency_ms?}]`, with a maximum of 31 days.
`p95=true` adds the billable p95 lines. `GET /usage` also accepts the
derived meters `disk.iops_avg`, `disk.mbps_avg` and
`disk.latency_ms_avg`.

### 9.4 Live rates: `GET /vms/{id}/stats`

These come from the last two samples in memory and are not stored:
CPU % per vCPU, memory used and peak, disk read/write IOPS, MB/s
and average latency per disk over the last interval, rx/tx pps and Mbit/s per NIC (split into external and
internal on NAT networks), and `sampled_at`. The
network detail page gets the same for its bridge
(`GET /networks/{id}/stats`). Under `GET /watch` a new kind `stats` is
added for the UI. It fires on every round, but only for VMs a client
watches by id.

**As built (M2.4).** Code: `api/rates.rs`. Four differences from the
design:
- **No `stats` watch kind.** Live stats change every round, and the
  `/watch` stream carries object changes, so the UI polls
  `GET /vms/{id}/stats` instead. Each round notes its rates in memory
  (`Round::live`), and a subject drops out after two intervals without
  a fresh rate.
- **CSV columns.** `/usage/bandwidth` and `/usage/disk-io` take
  `format=csv` (audited) with one column per JSON leaf.
- **Final figures.** For a month whose slots have expired, they answer
  from the stored monthly figures (`from_final_figures: true`), which
  carry the p95 only.
- **Averages** are over the slots the group was present in (§8.5.1),
  so a VM that ran for a week is not averaged over the whole month.

## 10. Authorization

New permission group `usage.read`, with concrete actions:

| Action | Resource | Granted to |
|---|---|---|
| `readProjectUsage` | `Project` | `role.viewer` and above (members see their own project's bill) |
| `readUsage` | `Host` | `role.auditor`, `role.system-admin` |
| `readVmStats` | `Vm` | same as `vm.read` |
| `readNetworkStats` | `Network` | same as reading the network (`net-admin` for host networks) |

`GET /usage/bandwidth` uses `readUsage` / `readProjectUsage`.
`GET /vms/{id}/bandwidth` uses `readVmStats` and
`GET /networks/{id}/bandwidth` uses `readNetworkStats`. Each route
still gets its own concrete action in the schema (`readBandwidth`,
`readVmBandwidth`, `readNetworkBandwidth`), in the same groups as
those actions. Disk I/O works the same way:
- `GET /usage/disk-io` → `readDiskIoUsage` (with `readUsage` /
  `readProjectUsage`)
- `GET /vms/{id}/io` → `readVmIo` (with `readVmStats`)
- `GET /disks/{id}/io` → `readDiskIo`, granted to whoever can read the
  disk

Like `read_audit` (`api/access.rs`): `readUsage` on `Host` reads
everything, including `image.stored` and rows of deleted projects.
Otherwise every requested project needs `readProjectUsage`, and
projects that can't be read are `404`, not filtered silently. All are
reads and are not audited per request. `format=csv` is audited
(`usage.export`), because it is a bulk export.

Usage data is tenant data: VM names, traffic volumes and activity
patterns. It never goes to `netd-ro.sock` or to unauthenticated
endpoints. It is not written to logs.

## 11. Configuration

New section in `config.rs` (`#[serde(deny_unknown_fields, default)]`),
mirrored in `packaging/control-plane.json.example`:

```json
"metering": {
  "enabled": true,
  "sample_secs": 30,
  "storage_secs": 900,
  "close_grace_secs": 120,
  "retention_days": 400,
  "retention_daily_days": 1825,
  "retention_rate_days": 100,
  "billing_timezone": "UTC"
}
```

`Config::check` enforces these ranges:

- `sample_secs` 10–300, and a divisor of 300 (10, 12, 15, 20, 25, 30,
  50, 60, 75, 100, 150, 300), so samples line up with 5-minute slots
  (§8.5.1)
- `retention_rate_days` 35–400
- `storage_secs` from `sample_secs` to 86400
- `close_grace_secs` from 0 to 3600, and at least `sample_secs`
- `billing_timezone`: an IANA name whose offset is a whole number of
  hours all year (§8.4)

`enabled: false` stops sampling. Cursors are kept, so re-enabling
spreads the gap like a downtime (§6.4).

## 12. CLI and UI

**gxctl**:

| Command | Effect |
|---|---|
| `usage [--from D] [--to D] [--by project\|vm\|disk\|nic] [--granularity hour\|day\|month] [--meters m,…] [--csv]` | `GET /usage`. Prints a table in presentation units. `--csv` writes raw CSV to stdout. |
| `usage bandwidth [--month YYYY-MM] [--by project\|vm\|nic\|network] [--csv]` | `GET /usage/bandwidth`. Avg, 30-second peak, and p95 per direction plus billable p95 (Mbps). |
| `vm bandwidth <vm> [--from D] [--to D]`, `network bandwidth <net> …` | 5-minute series as a text table, with the p95. |
| `usage disk-io [--month YYYY-MM] [--by project\|vm\|disk] [--csv]` | `GET /usage/disk-io`. Avg IOPS, MB/s and latency, 30-second peaks, and read, write and billable p95. |
| `disk io <disk> [--from D] [--to D]`, `vm io <vm> …` | 5-minute IOPS, MB/s and latency series, with the billable p95. |
| `vm stats <vm>` | `GET /vms/{id}/stats`. Prints one screen of current rates. |
| `vm usage <vm> [...]`, `disk usage <disk> [...]` | per-subject shortcuts |

**Web UI**:

- A **Usage** page (`/usage`), shown with `readUsage` or
  `readProjectUsage`. It has a range picker, group-by, a stacked bar
  per day by project or VM, and a totals table with a CSV download.
- **VM detail** gets a "Usage" card (last 24 h sparklines: CPU, memory,
  network traffic, IOPS) fed by `stats` watch events.
- **Networking** shows each network's month-to-date `bridge.bytes`,
  split into external and internal for NAT networks, and its
  month-to-date billable p95.
- **Disk I/O graphs.** VM detail gets a per-disk 5-minute chart (read
  and write IOPS stacked, MB/s on a toggle, latency as a separate
  small chart below rather than a second axis) with the month-to-date
  billable p95 line. The **Disks** page gets columns for
  month-to-date billable p95 IOPS and average latency. The Usage page
  gets a "Disk I/O" tab.
- **Bandwidth graphs.** VM detail and network detail get a 5-minute
  Mbps chart (rx/tx, plus external on NAT) with a horizontal line at
  the month-to-date p95 and a marker on the 30-second peak (follow the
  `dataviz` conventions). The Usage page gets a "Bandwidth" tab
  showing `GET /usage/bandwidth` per project or VM.
- **Projects** shows billing-month-to-date usage (including GiB
  transferred) next to the quotas.

## 13. Testing

- **Unit (ledger):**
  - Counter delta for each reset case in §6.2.
  - Hour splitting at boundaries, with property tests that the sum of
    the parts equals the delta.
  - Gauge integration.
  - Closing and adjustment rows.
  - Retention, including `rate_5m` expiry after `BandwidthMonth` is
    written.
  - Slot splitting: the slots of an hour add up exactly to the hour's
    bytes (property test).
  - p95 nearest rank on known series: N = 8640 with a known top 5%;
    N = 1; all zeros; ties.
  - `billable = max(rx, tx)`.
  - Group p95 is the p95 of the summed series: two NICs with
    non-overlapping peaks must give less than the sum of their p95s.
  - Slot membership for a NIC attached mid-month.
  - Billing-month slot counts in `Asia/Bangkok` and UTC.
  - Peaks ignore intervals after a gap.
  - Disk billable p95 is the p95 of read+write per slot, which is not
    the sum of the read and write p95s (a series where reads and writes
    peak in different slots).
  - Op-weighted latency: one busy slot and many idle ones give the
    busy slot's latency. A range with no ops gives `null`.
  - Disk slot membership: slots while detached are excluded; slots
    while attached to a stopped VM count as 0.
  - A disk moved between VMs mid-hour gives two rows, and the VM
    figures split correctly.
  - `query-blockstats` parsing including `*_total_time_ns`; CH
    `vm.counters`: the `*_latency_*` fields are ignored, the
    `u64::MAX` sentinel never leaks into any meter, and
    `latency_source` is `none`.
  - The atomicity of delta and cursor, by injecting a failed commit and
    checking that nothing is double-counted.
- **Unit (sources):**
  - cgroup parsers against fixture files (`cpu.stat`, `io.stat`).
  - CH `vm.counters` and QMP `query-blockstats` against recorded JSON.
  - `port_stats` parsing of `ovs-vsctl list Interface` output: roles,
    rx/tx mapping to the guest view, per-bridge grouping.
  - `nft -j list counters` parsing and L2 normalization.
  - Golden output for the `inet glidex_meter` script.
- **netd:** the counter-survival invariant (D14), using the fake
  `Exec`. Add a reservation, then remove another one, then
  `ensure_nat`/`delete_nat` on a different network. Check that the
  generated scripts never delete or re-create an existing counter and
  never touch `inet glidex_meter` when rebuilding `inet glidex`.
- **Functional (host, KVM):**
  - Run a VM with `stress-ng --cpu 2` for 5 minutes and check
    `cpu.used` ≈ 10 core-minutes ±5%.
  - Write 1 GiB with `dd oflag=direct` and check `disk.write_bytes`.
  - `curl` a 100 MiB file through NAT and check the VM's `net.rx_bytes`
    ≥ 100 MiB and the network's `bridge.bytes` ≥ 100 MiB. Check that
    `net.ext_rx_bytes` is within 1% of `net.rx_bytes` for that
    transfer.
  - VM-to-VM `iperf` on one NAT bridge adds nothing to `ext_*`, and
    DNS queries to the gateway add nothing either.
  - `iperf -b 100M` for 30 minutes inside a 60-minute window. Check
    that `rx` avg ≈ 50 Mbps, the 30-second peak ≈ 100 Mbps, and that
    the p95 over the window's 12 slots is ≈ 100 Mbps. Then check that
    a 10-minute burst in a 4-hour window (48 slots, 2 burst slots ≤ the
    top 5%) leaves the p95 at the baseline.
  - `fio --direct=1 --rw=randread --bs=4k --rate_iops=500` for 30
    minutes. Check read avg ≈ 500 IOPS over those slots, billable p95
    ≈ 500, MB/s ≈ 2.05. On QEMU, latency > 0. On CH, latency is
    absent. Then run a mixed
    `--rw=randrw --rwmixread=70 --rate_iops=700,300` and check that
    billable p95 ≈ 1000 (total) while read p95 ≈ 700.
  - Add a second NAT network while a transfer runs and check that the
    `ext_*` counters don't drop (D14).
  - A guest that adds an unreserved address and curls out shows up in
    `bridge.ext_unattributed_bytes`, not in any VM's meters.
  - Send 1 GiB VM-to-VM with `iperf` on one bridge and check it counts
    in both VMs' `net.bytes` and once in `bridge.bytes` (D7).
  - Detach and re-attach a NIC mid-transfer and check that no bytes
    are lost or double-counted (new `_uuid`).
  - Restart the control plane mid-run and check that totals are
    unchanged.
  - Stop the VM between samples and check that the exit snapshot covers
    the tail.
- **Security tests:**
  - A viewer of project A gets `404` for project B usage.
  - `netd-ro.sock` refuses `port_stats`.

## 14. Edits to existing documents

Each edit lands in the PR that changes the behaviour (§15), not
before it.

| Document | Edit | PR |
|---|---|---|
| [README.md](README.md) | Index row | done |
| [installer.md](installer.md) | Re-render `glidex-vm@.service` with accounting on, and set it at runtime on running VM units; no new packages (`nft` and `ovs-vsctl` are already present) | M1.2 |
| [reconciliation.md](reconciliation.md) §13.1 | `glidex-vm@.service`: `IOAccounting=yes`, `MemoryAccounting=yes` (not on the slice, §5.1) | M1.2 |
| [reconciliation.md](reconciliation.md) §7.2 | `status.phase_since` | M1.2 |
| [reconciliation.md](reconciliation.md) §7.2 | The controllers request a final meter sample before `detach_vm_port`, `release_vm` and network deletion | M1.3 |
| [data-model.md](data-model.md) | Metering tables (§7.1) | M1.1; `rate_5m`, `usage_monthly_rates` in M2.3 |
| [networking.md](networking.md) | Op `port_stats` (read-only, not on `netd-ro.sock`); port roles `vm`/`uplink`/`gateway`; final sample before detach, `release_vm`, uplink change and bridge/NAT delete | M1.3 |
| [networking.md](networking.md) | Table `inet glidex_meter` with its incremental-update rule (D14); op `nat_counters`; host check for `ether saddr` in `forward` | M1.4 |
| [installer.md](installer.md) | `uninstall` drops `inet glidex_meter` next to `inet glidex` (`uninstall.rs` `DropNftTable`) | M1.4 |
| [rest-api.md](rest-api.md) | `/usage`, `/vms/{id}/usage`, `/projects/{id}/usage`, `/disks/{id}/usage` | M1.5 |
| [rest-api.md](rest-api.md) | `/usage/bandwidth`, `/usage/disk-io`, `/bandwidth` and `/io` series, `/stats`, `stats` watch kind | M2.4 |
| [security.md](security.md) §7, §13 | `usage.read` group and the M1 actions; `metering` config section | M1.5 |
| [security.md](security.md) §7 | Bandwidth, disk I/O and stats actions | M2.4 |
| [cli.md](cli.md) | `usage` | M1.5 |
| [cli.md](cli.md) | `usage bandwidth`, `usage disk-io`, `vm/network bandwidth`, `vm/disk io`, `vm stats` | M2.4 |
| [hypervisors.md](hypervisors.md) | CH `vm.counters` and QMP `query-blockstats` (including `*_total_time_ns`) in the client tables; CH block latency fields not metered (D17), with the v53 findings; the device-id → disk mapping in `status.instance.disks` | M2.1 |
| [reconciliation.md](reconciliation.md) §8.4 | `instance.json` `exit.usage` (D12); the shim's polling of hypervisor counters | M2.2 |
| [web-ui.md](web-ui.md) | §12 pages, cards and charts | M2.6 |

## 15. Milestones

### 15.1 Principles

- **Each PR ships on its own.** Each one leaves `main` releasable and
  is done when its tests (§13) pass in CI and, where marked *(host)*,
  on a KVM host with both hypervisors.
- **Additive storage.** New tables only. `SCHEMA_VERSION` doesn't
  change. A downgrade ignores the tables (§7.1). No PR rewrites an
  existing record format, except optional new fields (`phase_since`,
  `exit.usage`, the device id in `status.instance.disks`), which old
  readers skip.
- **Off switch from the first PR that samples.** `metering.enabled`
  (default `true` from M1.2) stops all sampling and leaves cursors in
  place (§11). Re-enabling resumes without double-counting.
- **No backfill.** Metering starts when the build that has it first
  runs. `meter_meta.started_at` is written once. A billing month that
  began before it is marked `partial: true` in every API response and
  export (§8.4, §8.5.1, §8.6), so a first invoice can't be mistaken
  for a full month.
- **Ledger before sources, sources before API.** Correctness lives in
  the ledger (D8). It lands first, as pure code with property tests,
  so every later source plugs into a tested core.
- **Host facts before code that relies on them.** Each check listed in
  M0 is recorded in the spec (§5) before the PR that depends on it
  merges.

### 15.2 Dependency graph

```
M0 host checks ──────────────┬──────────────┬─────────────┐
                             ▼              ▼             ▼
M1.1 ledger ──► M1.2 service + VM ──► M1.3 ports ──► M1.4 NAT split
                     │                    │               │
                     └────────────► M1.5 query + API + CLI ◄┘
                                          │
            ┌────────────┬────────────────┼─────────────┐
            ▼            ▼                ▼             ▼
      M2.1 disk I/O  M2.5 disk.stored  M2.3 slots + p95 + roll-ups
            │                              │
            ▼                              ▼
      M2.2 exit snapshot ───────────► M2.4 rate API + stats + CLI
                                           │
                                           ▼
                                     M2.6 web UI
                                           │
                                           ▼
                                     M3 io.stat, tz=, tuning
```

M1.3 and M1.4 can be worked on in parallel with M1.5 once M1.2 is in.
M2.1, M2.3 and M2.5 are independent of each other.

### 15.3 M0: host checks (no product code)

Short experiments on a KVM host with the pinned versions. The results
go into §5 in the same style as [networking.md](networking.md) §0.

| Check | Result needed | Feeds |
|---|---|---|
| `ether saddr` matches in an `inet` `forward` chain for packets routed in from an OVS internal port | **done** (2026-10-04): yes. The scratch table `inet glidex_m0test` (priority `filter + 10`, counters only) counted 3463 packets on a MAC + IP rule, the same as the IP-only and `iifname`-only rules. Outbound keys on MAC + IP. | M1.4 |
| Named counters survive `flush chain` plus re-adding rules in one `nft -f` transaction | **done**: yes. The value (3463 packets) and the object handle (3) were unchanged, and `add counter` on an existing counter is a no-op (D14). | M1.4 |
| OVSDB `Interface.statistics` exists and updates for tap, `dpdkvhostuserclient`, the bridge's internal port and each uplink kind (kernel, AF_XDP, DPDK) | **partly done**: tap and internal ports have counters, refreshed in at most 5 s. The internal port reports the host's view, reversed (§5.4). There are no vhost-user or uplink ports on the test host (DPDK is initialized, but unused), so these are still to be checked on a host that has them. | M1.3 |
| `IOAccounting=` placement for per-VM `io.stat` without restarting VMs | **done**: on the VM unit template, not the slice. A runtime `set-property` on a running unit takes effect at once for every sibling. `CPUAccounting=` is deprecated (§5.1). | M1.2 |
| The unit's `ControlGroup` property over D-Bus, read as user `glidex` | **done**: readable as `glidex` with no polkit (`busctl get-property`, outside the sandbox; the sandbox allows `AF_UNIX`) | M1.2 |
| CH `vm.counters` device keys (`_disk<N>`) match the disk order glidex passes on argv; QEMU `qdev` ids match the `id=` glidex assigns | **done** for QEMU: `id=vd<i>` (`hypervisor/qemu.rs`), seen live as `/machine/peripheral/vd<i>/virtio-backend`. For CH, glidex passes no disk `id=`, so CH numbers disks `_disk0…` in argv order (seen live: `_disk0`, `_disk1`). The mapping is the index in the launched disk list, seed disk included. | M2.1 |
| CH v53 block latency | **done** (2026-10-04): not usable (D17) | M2.1 |
| QEMU `*_total_time_ns` | **done** (2026-10-04): present and cumulative | M2.1 |

### 15.4 M1: core ledger, VM and network metering, usage API

**M1.1: ledger core** (`metering/ledger.rs`, no wiring)

- Tables `meter_cursors`, `meter_open`, `usage_hourly` and
  `meter_meta`, declared and opened on the shared `Arc<Database>`, as
  `ProjectStore::new` does.
- `Cursor`, `HourAcc`, `UsageRecord`, `Subject` and `Flag` (§7.2).
- `counter_delta` with every reset case (§6.2). Splitting a delta over
  hours (§6.3). Gauge integration. The `max` meters.
- Closing an hour and adjustment rows (§6.5). One `commit_round(txn,
  …)` that writes deltas and cursors together (D8).
- Range read: `usage_hourly` scan, filter, group, sum or max
  (§8.1–§8.3).

*Accept:*
- Unit and property tests: the sum of the split parts equals the
  delta; no double count when a commit fails; reset cases; close and
  adjust; group and sum.
- The tables can be created and reopened on an existing database
  fixture from the current release.

**M1.2: service, VM CPU and memory, allocation, disk allocation**

- **Config:** the `metering` section in `config.rs` with `check`
  ranges, including `sample_secs` dividing 300 and `billing_timezone`
  being a whole-hour zone (§11). Mirror it in
  `packaging/control-plane.json.example`.
- **Service:** `Meter::start` (`metering/mod.rs`) is called from
  `main.rs` after `start_controllers()`, and its tasks are joined into
  `VmManager.tasks`. The sampler loop has a round budget of
  `sample_secs / 2`. Blocking sources (sysfs, `/proc`, later the
  synchronous `glidex-hv-client`) go through `spawn_blocking`, with a
  bounded join set (8 at a time). `meter_meta.started_at` and
  `last_round` are kept here.
- **cgroup source** (§5.1): the cgroup comes from the shim's
  `/proc/<pid>/cgroup` and must be this VM's unit. It reads `cpu.stat`
  and `memory.current`, with the hugepages rule
  (`metering/sources.rs`).
- **`/proc` source** for `Runner::Detached`, flagged `source_proc`.
- **Phases:** `vm.running` and `vm.paused`. `cpu.alloc` and
  `mem.alloc` come from `launch.json` (D11). `status.phase_since` is
  written by the VM controller (`controller/vm.rs` `write_status`).
- **Disks:** `disk.alloc` from the disk records (§5.3).
- **Installer:** `glidex-vm@.service` gets `IOAccounting=yes` and
  `MemoryAccounting=yes` (§5.1, M0). After `daemon-reload`, running
  `glidex-vm@*` units get `systemctl set-property --runtime <unit>
  IOAccounting=yes`. `systemd-analyze verify` still checks the units.

*Accept:*
- *(host)* `stress-ng --cpu 2` for 5 min gives `cpu.used` ≈ 10
  core-minutes ±5%, on CH and on QEMU.
- *(host)* Restart the control plane mid-run: totals are unchanged and
  the gap is flagged `interpolated`.
- *(host)* Pause and resume a VM: `cpu.alloc` stops while paused,
  `mem.alloc` doesn't.
- A detached-runner test VM is metered from `/proc`.

**M1.3: network totals from bridge ports**

- **netd:** `Op::PortStats` in `glidex-netd/src/proto.rs`, added to the
  read-only set and refused on `netd-ro.sock`. `glidex-ovs` gets a
  `vsctl::list(Interface, [_uuid, name, type, external_ids,
  statistics])` helper and the role classification `vm`, `uplink`,
  `gateway` (§5.4).
- **Meters:** `net.*` per NIC (guest-view swap) and `bridge.*` per
  network (Σ ingress, D7), with `_uuid` reset keys. Bandwidth
  `*_kbps_peak` and the derived `mbps_avg` (§8.5).
- **Final samples:** a `Meter::final_sample_vm(vm_id)` hook is called
  by the VM controller before `detach_vm_port` and `release_vm`, and
  by the network controller before `delete_bridge`, `delete_nat` and
  uplink changes. The call is bounded at 2 s. Missing it costs at most
  one interval and never blocks the controller.

*Accept:*
- `port_stats` parsing tests and the `netd-ro.sock` refusal test.
- *(host)* `curl` 100 MiB through NAT; VM-to-VM `iperf` counts in both
  NICs and once in `bridge.bytes` (D7); detach and re-attach mid-transfer
  loses nothing.
- *(host)* The same on a bridged network, for every uplink kind
  available on the host.

**M1.4: NAT external/internal split**

- **`glidex-ovs`:** `nat::meter_script(nats, reservations)` builds the
  `inet glidex_meter` table and chain (§5.5). It adds counters
  idempotently, deletes only released counters, and never deletes the
  table (D14). Golden tests next to the existing `nft_script` tests.
- **netd:** the counters are applied whenever a reservation or NAT
  changes, in the same place `apply_nft` runs. `Op::NatCounters`
  returns L2-normalized bytes and the counter `handle`. Counters are
  deleted on `release_vm` and `delete_nat`, after the final sample
  (M1.3 hook).
- **Meters:** `net.ext_*` and `bridge.ext_*`. The derived internal and
  unattributed figures (§2, §5.5). Bridged and isolated networks leave
  `ext_*` absent.
- **Uninstall:** `DropNftTable` also drops `inet glidex_meter`.

*Accept:*
- Netd fake-`Exec` tests for D14: no counter is ever re-created or
  deleted when another network or reservation changes.
- *(host)* NAT transfer: `ext_rx` is within 1% of `rx`. VM-to-VM and
  DNS traffic add 0 to `ext`. Adding a second NAT network during a
  transfer doesn't drop counts. An unreserved address shows up as
  unattributed.

**M1.5: query, API, authorization, CLI, CSV**

- `api/usage.rs` (or `metering/api.rs`): `GET /usage`,
  `/vms/{id}/usage`, `/projects/{id}/usage` and `/disks/{id}/usage`
  (§9.1–§9.2), `format=csv` with the `usage.export` audit entry, and
  billing-month grouping in `billing_timezone` (§8.4), with
  `complete_through`, `partial` and `revised_at`.
- **Cedar:** the `usage.read` group, `readUsage`, `readProjectUsage`
  and one concrete action per route in `glidex.cedarschema` and
  `roles.cedar`. `every_route_maps_to_a_schema_action` must pass.
- **gxctl:** `usage` (§12).

*Accept:*
- The security tests in §13: a cross-project read is `404`;
  `readUsage` on `Host` sees deleted projects.
- A CSV round trip: summing raw bytes equals the API total.
- A billing month in `Asia/Bangkok` has exactly the expected hours.

**As built: host acceptance, 2026-10-05.** Run on this host
(two Debian 13 guests, 1 vCPU each: `v1` on Cloud Hypervisor, `q2` on
QEMU; NAT network `default`), deployed with `glidex-install`.

| Check | Result |
|---|---|
| 300 s of CPU load in both guests | `cpu.used` vs cgroup `usage_usec` over the same window: CH +0.03%, QEMU +0.13% (about 1.02 cores) |
| Control-plane restart during 180 s of load | +0.21% vs the cgroup: nothing lost or doubled |
| 100 MB NAT download (`curl`) | `net.rx_bytes` +102.35 MiB (headers included); `net.ext_rx_bytes` 99.99% of it; `bridge.ext_rx_bytes` equal |
| 200 MiB VM to VM (`iperf`) | sender tx +200.27, receiver rx +200.26, `bridge.bytes` +200.45 (once), all `ext_*` +0.00 |
| Pause for 90 s | `vm.paused` +89 s; `cpu.alloc` stops; `mem.alloc` continues |
| Second NAT network added and removed during a download | counters 6 → 8 → 6; the VM's counter never went backwards in 113 polls (D14) |
| 300 DNS lookups through the gateway | no change to `ext_*` |
| 10 MiB from an unreserved source address | +10.23 MiB unattributed at network level, none on the VM (§5.5) |
| Pre-metering usage | not billed. Traffic after the start was billed: about 76 MB of package installs in each guest, 3 min after metering started. |

It found one bug, now fixed: level × ms was truncated to whole units
per round, so level-1 gauges (`vm.running`, 1-vCPU `cpu.alloc`) lost
up to a second per round (11 s in 40 rounds). Cursors now carry the
remainder; over 5 minutes afterwards, `vm.running` = 300 s and
`mem.alloc` ÷ 512 = 299.998 s. Not run on this host, because it has
no such setup: bridged and vhost-user ports, and detaching a NIC
mid-transfer (covered by unit tests).

**M1 done when:** a site can bill CPU, memory, provisioned disk and
network GiB (total, plus external and internal on NAT) per VM, network
and project for a calendar month, from the API or CSV.

### 15.5 M2: disk I/O, exit snapshot, percentiles, roll-ups, UI

**M2.1: hypervisor disk counters**

- **`glidex-hv-client`:**
  - `ChClient::counters()` (`GET /vm.counters`) and
    `QmpClient::query_blockstats()`, both returning typed per-device
    structs.
  - The CH `*_latency_*` fields are ignored, and the `u64::MAX`
    sentinel is never surfaced (D17).
  - QEMU `pflash` entries are skipped.
- **Device mapping:** add the device id to `status.instance.disks`,
  using the mapping M0 confirmed.
- **Meters:** the `disk.*` ops, bytes and `*_time_ns` counters (QEMU
  only), with read, write and total peaks for IOPS and kB/s (§5.2).
  `instance_id` is the reset key.

*Accept:*
- Parser tests on recorded JSON, including the live captures from
  2026-10-04.
- *(host)* `fio` at 500 IOPS gives avg and peak ≈ 500. Latency is
  present on QEMU and absent on CH.

**M2.2: shim exit snapshot (D12)**

- **`glidex-vm-shim`:**
  - The shim polls the hypervisor counters every `sample_secs` (passed
    in `launch.json`).
  - After it reaps the hypervisor, it reads its own cgroup `cpu.stat`,
    `memory.peak` and `io.stat`.
  - It writes `exit.usage` in `instance.json` (`state.rs`
    `ExitInfo`).
- **Control plane:** the exit snapshot is consumed once per
  `instance_id` when the VM controller records the exit (§6.4).

*Accept:*
- *(host)* Stop a VM between two samples: the tail is metered, and
  CPU totals match the unit's `CPUUsageNSec` ±1%.
- *(host)* A VM that ran and exited entirely while the control plane
  was stopped is fully metered.

**M2.3: 5-minute slots, percentiles and roll-ups**

- **Slots:** the `rate_5m` table, with NIC/network and disk `SlotRow`s
  (§7.1). Deltas are split into slots in the same `commit_round` as
  the hours.
- **p95 engine** (§8.5.1, §8.6): nearest rank, slot membership
  (attached or existing), group sums slot by slot, and billable
  figures (network: max(in, out); disk: total).
- **Finalization:** `usage_monthly_rates` with `BandwidthMonth` and
  `DiskIoMonth`. A month is finalized when it becomes final. Late
  adjustments recompute it and set `revised_at`.
- **Roll-up and retention:** `usage_daily`, and the daily retention
  task for `usage_hourly`, `rate_5m` and `usage_daily` (§7.3).

*Accept:*
- The p95 unit tests in §13: group p95 below the sum of p95s; disk
  total p95 is not the sum of the read and write p95s; slot counts per
  month.
- *(host)* The `iperf` 100 Mbps window and burst tests; the `fio`
  mixed read/write test gives billable ≈ 1000.
- Retention leaves `usage_monthly_rates` intact.

**M2.4: rate APIs, live stats, CLI**

- **Routes:** `GET /usage/bandwidth` and `/usage/disk-io`; the series
  endpoints `/vms/{id}/bandwidth`, `/networks/{id}/bandwidth`,
  `/vms/{id}/io` and `/disks/{id}/io`; `GET /vms/{id}/stats` and
  `/networks/{id}/stats`; and the `stats` watch kind (§9.3–§9.4),
  which needs `Kind` and `parse_kinds` in `api/watch.rs`.
- **Cedar:** one action per route (§10).
- **gxctl:** `usage bandwidth`, `usage disk-io`,
  `vm|network bandwidth`, `vm|disk io` and `vm stats`.

*Accept:* API tests for every route, including the authorization
matrix, and `partial` and `revised_at` in the JSON.

**M2.5: `disk.stored`**

- A periodic `qemu-img info` task every `storage_secs`, with bounded
  concurrency (2), skipping disks in `Creating`, `Resizing` or
  `Failed`. Host-level `image.stored` is included (§5.3).

*Accept:* a linked disk's stored size grows after guest writes while
`disk.alloc` stays the same.

**M2.6: web UI**

- **Pages and cards:**
  - the Usage page with Bandwidth and Disk I/O tabs, plus CSV download
  - the VM detail Usage card, with bandwidth and disk I/O charts
  - network detail charts
  - columns on Disks, Networking and Projects
- **Live data:** charts fed by the `stats` watch kind and the series
  endpoints. Follow the `dataviz` skill for chart design.
- **Gating:** nav and sections are shown by `useCan()`.

*Accept:* Playwright specs in `crates/glidex-ui/e2e/tests/usage.spec.ts`:
- the page renders for a viewer and for an auditor
- a project member doesn't see other projects
- CSV download works
- CH disks show "latency not available"

**As built: host acceptance, 2026-10-05.** Deployed with
`glidex-install`. The VMs were restarted to run the new shim. Ten
minutes of parallel load, compared with the fully loaded 5-minute
slot:

| Check | Result |
|---|---|
| `iperf -b 100M`, `v1` → `q2` (iperf: 105 Mbit/s) | `v1` tx = `q2` rx = 105.047 Mbps; p95 and billable 105.047; `ext_*` 0 |
| `fio` 4k randread at 500 IOPS on `q2` (QEMU) | read 499.997 IOPS; 2.048 MB/s (= 500 × 4 KiB); read latency 0.087 ms (host side; fio, in the guest, saw 0.154 ms) |
| `fio` randrw 700/300 IOPS on `v1` (CH) | read 700.0, write 300.4, billable p95 1000.4 (read + write, D16); no latency (D17) |
| Exit snapshot: CPU load, stopped 20 s after a sample | metered `cpu.used` for the instance = the shim's `exit.usage.cpu_usage_usec` exactly (88 168 908 µs), 23.05 s of it from the snapshot's tail; the disk ids map the root disk only (the seed is unmanaged) |
| Web UI | the VM card and the Usage page's three tabs, rendered with this data and reviewed |

Fixed from the review: **memory is the working set.** On the host, a
512 MiB CH VM had a `memory.current` of 913 MiB, 387 MiB of it
`inactive_file`: page cache from its buffered disk I/O, charged to its
cgroup. `mem.used` is now `memory.current − inactive_file` (519 MiB
for that VM), and `mem.peak` no longer comes from the snapshot's
`memory.peak`, which includes cache.

**M2 done when:** bandwidth and disk I/O can be billed on a 95th
percentile per VM and project, users see their usage in the UI, and a
VM's usage is complete even when it exits while the control plane is
down.

### 15.6 M3: host-side I/O, time zones, tuning

- **`io.stat` meters:** expose the per-VM host-side `vmio.*` meters
  (D4) in the API and the capacity view.
- **Report time zones:** `tz=` grouping for reports outside
  `billing_timezone`. Hours stay the atom, and whole-hour zones are
  exact (§8.1).
- **Tuning:** add the `usage_by_project` index if profiling a host
  with 200 VMs and 13 months of data shows `GET /usage` scans over
  200 ms. Add a load test for the sampler round budget with 200 VMs,
  each with 2 NICs and 2 disks.

*Accept:* the round finishes within `sample_secs / 2` at 200 VMs; the
query p95 is under 200 ms for a month of one project.

### 15.7 After M3

These come from §16 and are scheduled separately, each with its own
spec change:

- CH cumulative latency upstream, then CH latency
- port security, which hardens NAT attribution
- provisioned IOPS and throughput tiers
- a Prometheus exporter
- latency histograms
- splitting traffic on bridged networks

## 16. Open questions and future work

- **Prometheus `/metrics`.** Expose cumulative counters (not the
  ledger) for sites that already run Prometheus. It would need its own
  auth story, because it is a scrape endpoint.
- **GPU / VFIO devices.** Meter `device-hours` per VFIO device claim,
  using the same allocation model as `cpu.alloc`. Utilization would
  need vendor tools.
- **Port security.** Add OVS rules that let a VM port send only from
  its own MAC and reserved IP. That makes NAT external attribution
  spoof-proof (§5.5) and is worth having for isolation anyway.
- **Bridged-network split.** Add per-port OpenFlow rules
  (`in_port=<vm>` → uplink and back, with `n_bytes` read through a new
  `Program::OvsOfctl`) if bridged traffic ever needs pricing by
  destination. These would add `ext_*` meters to bridged NICs, and the
  existing meters would keep their meaning.
- **Provisioned IOPS / throughput tiers.** If disks are sold in tiers
  with IOPS or MB/s caps, meter the **provisioned** limit like
  `disk.alloc` (`disk.iops_alloc` in IOPS-hours, `disk.mbps_alloc`),
  next to the usage meters in §8.6. This only makes sense once limits
  are enforced: CH disk `rate_limiter` (`ops_size`/`bw_size`, or
  `rate_limit_groups`) or QEMU `throttle` groups (`iops-total`,
  `bps-total`). That would be a separate feature with its own `Disk`
  fields, quota and admission rules. Usage p95 against the
  provisioned cap then shows whether a tier is right-sized.
- **Cloud Hypervisor latency, upstream.** Propose cumulative
  `read_latency_total_us` / `write_latency_total_us` counters for
  `BlockCounters` upstream: one `fetch_add` per completion next to the
  existing ones. It would also be worth fixing the integer running
  mean that stops tracking (§5.2). When a pinned CH version has the
  counters, CH disks switch to `latency_source: counter` with no
  change to the ledger. The `*_time_ns` meters simply start appearing.
  Until then, the installer's CH version bump checklist re-checks
  this.
- **Latency percentiles.** QEMU can keep per-device latency histograms
  (`block-latency-histogram-set`, read through `query-blockstats`).
  Storing their bucket deltas per slot would give real p95/p99
  latency. Per-slot averages (§8.6) cannot.
- **Rate-card hooks.** Whether glidex should ever compute cost, or
  leave it to consumers of the CSV and JSON. The current answer is to
  leave it (§1).
