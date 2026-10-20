# Usage

Glidex meters what VMs, disks, images and networks use: running time,
CPU, memory, disk space, disk I/O and network traffic. It keeps the
figures by the hour and reports them per project, VM, disk, NIC,
network or node, by hour, day or billing month. You can also see a VM's
current rates and its last day of activity.

Who sees what:

- Anyone with a role in a project (`viewer` and up) sees that project's
  usage, and the usage of its VMs and disks.
- The host roles `auditor` and `system-admin` see every project,
  including deleted ones, and host-wide figures such as stored images.

## How it is measured

The control plane samples every running VM, every disk and every
network every 30 seconds (an administrator can change the interval),
and adds what it saw to hourly records. An hour is closed a couple of
minutes after it ends; until then its figures are provisional. Reports
say how far they are complete (**Complete through** in the web UI,
`complete through` in `gxctl`).

Usage keeps accruing while the control plane restarts: the next sample
covers the gap, spread over the time it lasted.

A **billing month** is a calendar month in the billing time zone, UTC
unless an administrator set another (a fixed, whole-hour offset).

Hourly records are kept for 90 days, then as daily totals for 5 years;
the 5-minute figures behind the percentiles are kept for 100 days.
Administrators can change these.

## What is measured

| Meter | Of | Shown as | What it is |
|---|---|---|---|
| `vm.running`, `vm.paused` | VM | hours | Time running, and time paused. |
| `cpu.alloc` | VM | vCPU-hours | vCPUs × time running (not paused). |
| `cpu.used` | VM | core-hours | CPU time the VM actually used, the hypervisor's own included. |
| `cpu.cores_peak` | VM | cores | Highest CPU rate over one sample interval. |
| `mem.alloc` | VM | MiB-hours | Memory size × time running or paused. |
| `device.alloc.<pool>` | VM | device-hours | Devices of a pool the VM holds × time running or paused. |
| `mem.used` | VM | MiB-hours | Memory the VM's processes held × time. |
| `mem.peak` | VM | MiB | Most memory held in the hour. |
| `vmio.read_bytes`, `vmio.write_bytes` | VM | GiB | Disk I/O on the host side, after the host's page cache. |
| `disk.alloc` | disk | GiB-hours | Provisioned size × time, attached or not, running or not. |
| `disk.stored` | disk | GiB-hours | Space the disk file takes on the host × time. |
| `image.stored` | image | GiB-hours | Space an image takes × time (host-wide). |
| `disk.read_ops`, `disk.write_ops` | disk | ops | Operations the guest issued. |
| `disk.read_bytes`, `disk.write_bytes` | disk | GiB | Bytes the guest read and wrote. |
| `disk.read_time_ns`, `disk.write_time_ns` | disk | s | Time spent in I/O, for latency (QEMU only). |
| `net.rx_bytes`, `net.tx_bytes` | NIC | GiB | Traffic the VM received and sent on that NIC. |
| `net.rx_packets`, `net.tx_packets` | NIC | packets | The same, in packets. |
| `net.ext_rx_bytes`, `net.ext_tx_bytes` | NIC (NAT) | GiB | The part of it that crossed the NAT gateway (in practice, the internet). |
| `bridge.bytes` | network | GiB | Traffic through the network, each frame counted once. |
| `bridge.ext_rx_bytes`, `bridge.ext_tx_bytes` | network (NAT) | GiB | The network's traffic through its NAT gateway. |

Directions are the guest's: `rx` is what the guest received.

Some meters are derived when you read them:

- `net.bytes` = received + sent; `net.ext_bytes` = the external part;
  `net.internal_bytes` = the rest (VM to VM, DHCP, DNS). Likewise
  `bridge.ext_bytes` and `bridge.internal_bytes` for a network.
- Averages over each bucket: `net.rx_kbps_avg`, `disk.read_iops_avg`,
  `disk.read_kBps_avg` and so on; `disk.read_latency_us`,
  `disk.write_latency_us` where time counters exist.
- Peaks over one sample interval: `net.rx_kbps_peak`, `net.pps_peak`,
  `disk.iops_peak`, `disk.kBps_peak` and so on.

The per-VM and per-network views of traffic differ on purpose: a frame
from one VM to another on the same network counts for both VMs, but
once for the network. A VM or a project is charged by its own NICs
(`net.bytes`).

Only NAT networks split external from internal traffic; on bridged and
isolated networks the `ext` meters are absent, not zero.

A row can carry **flags**: `provisional` (its hour isn't closed yet),
`revised` (corrected after the hour closed), `interpolated` (spread
over a gap between samples), `reset` (a counter went backwards),
`incomplete` (usage was lost, for example at a host reboot),
`source_proc` (CPU and memory read from `/proc` instead of the VM's
cgroup), `ext_gap` (external traffic may be short) and
`network_total_unavailable` (a provider network has per-VM figures but
no network total).

## Rates and percentiles

For bandwidth, disk I/O, and CPU and memory, Glidex also keeps a
5-minute average per VM, NIC, disk and network, and reports per billing
month:

- the **average**;
- the **peak**: the busiest sample interval (30 seconds by default);
- the **95th percentile** (p95) of the 5-minute averages, over the
  slots the subject was running.

**Billable** bandwidth is the larger of in and out; billable disk I/O is
read + write. CPU is reported in cores and as a share of the VM's vCPUs,
memory in MiB and as a share of its size; both include the
hypervisor's own overhead, so they can pass 100 %. Disk latency isn't
available on Cloud Hypervisor.

Once a month is over, its percentiles are stored and no longer change.

## Web UI

### The Usage page

**Usage** in the header opens it. Pick a **Month**, a **Project** (or
**All projects**, with host rights) and **Group by**, then a tab:

| Tab | Shows | Group by |
|---|---|---|
| **Usage** | The month's totals: CPU used, vCPUs and memory allocated, disk provisioned and stored, images stored, network traffic and its internet part, host disk I/O. Grouped by network: traffic through each network. | project, vm, disk, network |
| **Bandwidth** | Average, 30-s peak and p95 in and out, billable p95, internet p95, in Mbps. | project, vm, nic, network |
| **Disk I/O** | IOPS, throughput and latency: average, peak, p95. | project, vm, disk |
| **CPU & memory** | Cores and memory: average, peak, p95, and their share of what is allocated. | project, vm |

The line above the table says how complete the figures are, or whether
the month is **Final** or **Month to date**. **Download CSV** saves the
table's data.

### A VM's usage

The **Usage** section of a VM's page shows its current rates (CPU per
vCPU, memory working set, network in and out, disk reads and writes)
and charts of the last 24 hours in 5-minute averages, with the 95th
percentile as a dashed line. It refreshes every 30 seconds.

## gxctl

```
gxctl> usage                                     # this billing month, by project
gxctl> usage --project my-project --by vm        # one project, by VM
gxctl> usage --from 2026-10-01T00:00Z --to 2026-10-08T00:00Z --granularity day --by vm,nic
gxctl> usage --meters cpu.used,net.bytes --by vm
gxctl> usage --by vm --csv > october.csv
```

| Flag | Meaning |
|---|---|
| `--project P` | Projects (comma-separated); default: the selected project, or all you can see. |
| `--from D`, `--to D` | RFC 3339 or unix seconds; default: from the start of this billing month to now. |
| `--by K,…` | Group by `project`, `vm`, `disk`, `nic`, `network` (and `node`). |
| `--granularity G` | `hour`, `day` or `month` (the default). |
| `--meters M,…` | Only these meters (derived ones included). |
| `--tz Z` | Time zone for day and month buckets: `UTC`, `+07:00`, or a zone with a fixed offset such as `Asia/Bangkok`. |
| `--csv` | CSV instead of a listing. |

Monthly rates and percentiles:

```
gxctl> usage bandwidth                           # this month, by project
gxctl> usage bandwidth --month 2026-09 --by vm,nic
gxctl> usage disk-io --by disk
gxctl> usage compute --by vm --csv
```

For one VM, disk or network:

```
gxctl> stats my-vm                               # current rates: CPU, memory, NICs, disks
gxctl> bandwidth my-vm                           # 5-minute series, last 24 h, with p95
gxctl> bandwidth --network lab
gxctl> io my-vm
gxctl> io --disk data-1
gxctl> compute my-vm
gxctl> compute my-vm --from 2026-10-10T00:00Z --to 2026-10-12T00:00Z
```

## API

| Endpoint | Returns |
|---|---|
| `GET /usage` | Usage rows; host-wide with host rights, else the projects you may read. |
| `GET /projects/{id}/usage` | One project. |
| `GET /vms/{id}/usage` | One VM and its NICs; `?include=disks` adds its attached disks. |
| `GET /disks/{id}/usage` | One disk. |
| `GET /usage/bandwidth`, `/usage/disk-io`, `/usage/compute` | Monthly average, peak and p95. |
| `GET /vms/{id}/stats` | Current rates. |
| `GET /vms/{id}/bandwidth`, `/vms/{id}/io`, `/vms/{id}/compute` | 5-minute series. |
| `GET /disks/{id}/io`, `/networks/{id}/bandwidth`, `/networks/{id}/stats` | The same for a disk or network. |

Usage queries take `project`, `from`, `to` (RFC 3339 or unix seconds),
`month` (`YYYY-MM`), `granularity` (`hour`, `day` (the default) or
`month`), `group_by`, `meters`, `tz`, and `format=csv`. A query covers
at most 400 days. Monthly reports take `project`, `month` (default: this
month), `group_by` and `format=csv`; series take `from` and `to` (default: the
last 24 hours, at most 31 days).

```bash
curl -H "Authorization: Bearer $GLIDEX_TOKEN" \
  'https://host.example.org:8841/usage?project=my-project&month=2026-09&granularity=month&group_by=vm'
```

CSV exports are recorded in the audit log. See
[REST API](../api/rest.md#usage-and-metrics).
