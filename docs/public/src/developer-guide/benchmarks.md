# Performance and benchmarks

This page collects the figures a contributor should know: what has been
measured, where, when and how, and how to measure it again. Glidex has
no benchmark suite yet (see [Gaps](#gaps)), so most figures come from
host acceptance runs recorded in the specs and in pull requests, from
CI timings, and from a few read-only measurements on a development host.

## How to read the numbers

- Every figure was taken once, on one date, on one machine. It is a
  data point, not a guarantee or a service level.
- The hardware is stated where it was recorded. Many host runs in the
  specs say only "this host" (the project's development host); for
  those the hardware is given as *not recorded*.
- Some figures are **design estimates**, not measurements. They are
  marked as such. The cluster scale envelope is the main one.
- Test durations (for example "the kernel-boot test passed in 68 s")
  are the wall time of a whole test: pulling or building artifacts,
  booting, logging in, checks and teardown. They are not boot times.
- Fixed rates in the metering runs (`iperf -b 100M`, `fio` at 500 IOPS)
  check that metering counts correctly. They say nothing about the
  maximum throughput of the data path.
- Defaults and limits (timeouts, retry intervals, size caps) are
  configuration, not measurements. Where they help read a figure they
  are listed separately and labelled as defaults.

### Measurement host

The figures marked *measured on 2026-10-22* were taken on this machine:

| | |
|---|---|
| CPU | AMD EPYC 9124, 16 cores, 32 threads, 1 socket, 1 NUMA node |
| Memory | 31,293 MiB |
| Disk | one consumer NVMe SSD (1 TB) |
| Kernel | Ubuntu, `7.0.0-34-generic` |
| Glidex | installed release build of 2026-10-22, the server and Raft leader of a three-node cluster (one server, two agents) holding 1 VM, 8 images, 2 networks and 1 disk |

The host was running its normal workload (load average about 2.8 on 32
threads) during the measurements.

## Key figures

| Figure | Value | Source | Date | Conditions |
|---|---|---|---|---|
| API read latency, `GET /health` | p50 1.6 ms, p95 1.8 ms | measured | 2026-10-22 | `curl` over the local Unix socket, n = 300, measurement host |
| API read latency, `GET /vms` | p50 3.3 ms, p95 4.5 ms | measured | 2026-10-22 | as above, 1 VM |
| API read latency, `GET /images` | p50 7.0 ms, p95 7.5 ms | measured | 2026-10-22 | as above, 8 images, 7.6 kB response |
| Metering sampling round, 200 VMs | 39 ms (budget 15 s) | [metering.md §15.6](https://github.com/CMKL-APEX/glidex/blob/main/spec/metering.md#156-m3-host-side-io-time-zones-tuning), [#13](https://github.com/CMKL-APEX/glidex/pull/13) | 2026-10-05 to 2026-10-07 | `metering/bench.rs`, 200 VMs, 2 NICs and 2 disks each; hardware not recorded |
| Usage query, one project, one month | 146 ms (target 200 ms) | as above | as above | as above, a month of 5-minute slots |
| Metering history per VM | about 12 MB at steady state | [metering.md §15.6](https://github.com/CMKL-APEX/glidex/blob/main/spec/metering.md#156-m3-host-side-io-time-zones-tuning) | 2026-10-05 | default retention, derived from the measured row sizes |
| Kernel extraction, one image | 1.7–1.8 s (Cloud Hypervisor), 2.2 s (QEMU) | [kernel-boot.md §3, F-K9](https://github.com/CMKL-APEX/glidex/blob/main/spec/kernel-boot.md#3-verified-facts) | reported 2026-10-20 | the Debian appliance, four distros; hardware not recorded |
| Pull, extract, boot and log in | 61–121 s per image | [kernel-boot.md §11](https://github.com/CMKL-APEX/glidex/blob/main/spec/kernel-boot.md#11-testing), [#90](https://github.com/CMKL-APEX/glidex/pull/90) | 2026-10-19 | whole ignored test, fresh data directory, network download included |
| UI first page load | 666 kB, 197 kB gzip, 169 kB brotli | measured | 2026-10-22 | installed UI bundle: `index.html`, entry, `react` and `rpc` chunks, CSS |
| Cluster scale | designed for 50 hosts, 2,000 VMs | [clustering.md §4.1](https://github.com/CMKL-APEX/glidex/blob/main/spec/clustering.md#41-scale-envelope) | design estimate | **not measured**: the scale run is still to do |
| CI `build` job (build and all tests) | median 33.5 min (18.9–36.2) | GitHub Actions, workflow `Rust` | 2026-10-21 to 2026-10-22 | 26 successful runs on `main`, GitHub-hosted `ubuntu-26.04` runner |
| Workspace test count | 1,066 passed, 243 ignored | CI log of one run | 2026-10-22 | `GLIDEX_CHECK_WRITE_SETS=1 cargo test` |

## API latency

Measured on 2026-10-22 on the [measurement host](#measurement-host).
Each endpoint was called 300 times (three interleaved rounds of 100,
after five warm-up calls) with `curl` over the control plane's local
API socket, and `curl`'s `time_total` recorded. That includes `curl`'s
connect and the full response, not process start-up. Reads on this
host are served by the Raft leader.

| Request | Response size | p50 | p95 |
|---|---|---|---|
| `GET /health` | 15 B | 1.6 ms | 1.8 ms |
| `GET /cluster/status` | 1.7 kB | 2.7 ms | 4.0 ms |
| `GET /nodes` (3 nodes) | 8.4 kB | 2.9 ms | 4.5 ms |
| `GET /vms` (1 VM) | 1.6 kB | 3.3 ms | 4.5 ms |
| `GET /networks` (2 networks) | 0.9 kB | 5.6 ms | 6.3 ms |
| `GET /images` (8 images) | 7.6 kB | 7.0 ms | 7.5 ms |

Repeated runs moved the p50 of the cheapest calls by up to a millisecond
(a separate run of 100 gave `/health` a p50 of 0.85 ms), so read these
as "a few milliseconds", not to the tenth. Write latency, the RPC API,
TCP with TLS and requests forwarded from a follower were not measured.

## VM lifecycle

Glidex does not measure boot or stop times. The closest figures are the
wall times of the ignored real-boot tests, each of which does more than
boot:

| Test | Time | What it covers | Source |
|---|---|---|---|
| `kernel_boot_with_initrd` and `qemu_` twin | about 62–63 s | Ubuntu 24.04 kernel and initrd, boot to systemd on two console devices, both backends | [kernel-boot.md §11](https://github.com/CMKL-APEX/glidex/blob/main/spec/kernel-boot.md#11-testing), [#77](https://github.com/CMKL-APEX/glidex/pull/77) (2026-10-19) |
| `kernel_image_boot_with_generated_cloud_init` and `qemu_` twin | 68 s | Ubuntu 26.04 as a kernel image, generated seed, credential, login, `sudo`, clean power-off | [#88](https://github.com/CMKL-APEX/glidex/pull/88) (merged 2026-10-20) |
| `guest_kernel_updates_are_followed` | 146 s | two boots, an `update-initramfs` in the guest, re-extraction, a third boot | [kernel-boot.md §11](https://github.com/CMKL-APEX/glidex/blob/main/spec/kernel-boot.md#11-testing), [#99](https://github.com/CMKL-APEX/glidex/pull/99) (2026-10-20) |

Hardware for these runs was not recorded.

Defaults that bound lifecycle operations (configuration, not
measurements):

| Default | Value | Source |
|---|---|---|
| Graceful stop before force | 60 s; `gxctl stop` waits max(60, grace + 20) s, at most 300 s | [cli.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/cli.md) |
| Crash restart back-off | 10 s × 2^n, capped at 300 s; reset after 600 s running | [reconciliation.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/reconciliation.md) |
| Resync of every object | every 30 s (`reconcile.resync_secs`) | [clustering.md §4.1](https://github.com/CMKL-APEX/glidex/blob/main/spec/clustering.md#41-scale-envelope) |
| Kernel extractor VM | killed after 120 s; 1 vCPU, 1 GiB | [kernel-boot.md §10](https://github.com/CMKL-APEX/glidex/blob/main/spec/kernel-boot.md#10-the-extractor) |

## Storage

There is no storage throughput benchmark. What has been measured:

| Figure | Value | Source | Conditions |
|---|---|---|---|
| Metered disk IOPS at a fixed `fio` rate | 499.997 IOPS and 2.048 MB/s for `fio` 4k randread at 500 IOPS | [metering.md §15.5](https://github.com/CMKL-APEX/glidex/blob/main/spec/metering.md#155-m2-disk-io-exit-snapshot-percentiles-roll-ups-ui), [#12](https://github.com/CMKL-APEX/glidex/pull/12) | 2026-10-05, QEMU guest; accuracy check, not peak |
| Read latency at that rate | 0.087 ms on the host side; `fio` in the guest saw 0.154 ms | as above | as above |
| Mixed rate | `fio` randrw 700/300 IOPS metered as 700.0 / 300.4 | as above | Cloud Hypervisor guest |
| LVM-thin boot test | a cloud image boots from a linked volume grown to 6 GiB | [storage.md](https://github.com/CMKL-APEX/glidex/blob/main/spec/storage.md), [#119](https://github.com/CMKL-APEX/glidex/pull/119) | loop-device volume group; pass/fail only, no timing |

Disk moves between datastores (S5) and placement of stopped VMs (S4)
have tests but no recorded timings.

## Networking

There is no network throughput benchmark. The OVS-DPDK tuning (PMD
placement, userspace TSO, queue count, 1024-descriptor rings,
hugepage-backed guests) was set from host testing, but no Gbit/s
figure is recorded. See
[networking.md, findings from host testing](https://github.com/CMKL-APEX/glidex/blob/main/spec/networking.md#findings-from-host-testing).

| Figure | Value | Source | Conditions |
|---|---|---|---|
| Metered VM-to-VM rate at a fixed `iperf` rate | 105.047 Mbps, both ends (iperf reported 105 Mbit/s) | [metering.md §15.5](https://github.com/CMKL-APEX/glidex/blob/main/spec/metering.md#155-m2-disk-io-exit-snapshot-percentiles-roll-ups-ui) | 2026-10-05, one Cloud Hypervisor and one QEMU guest; accuracy check |
| Metered bytes, 200 MiB VM to VM | sender +200.27 MiB, receiver +200.26 MiB, bridge +200.45 MiB | [metering.md §15.4](https://github.com/CMKL-APEX/glidex/blob/main/spec/metering.md#154-m1-core-ledger-vm-and-network-metering-usage-api) | 2026-10-05 |
| OVS-DPDK shared mempool | about 600 MB at MTU 1500; a second, about 2.5 GB, at MTU 9000 | [networking.md, finding 9](https://github.com/CMKL-APEX/glidex/blob/main/spec/networking.md#findings-from-host-testing) | sizing from host testing; the installer reserves 6144 hugepages (capped at a third of RAM) |
| Nested two-node OVN cluster test | passed in 670 s | [#38](https://github.com/CMKL-APEX/glidex/pull/38) | 2026-10-17; whole test, including installing Glidex into two VMs |

## Cluster scale and Raft

The scale envelope in
[clustering.md §4.1](https://github.com/CMKL-APEX/glidex/blob/main/spec/clustering.md#41-scale-envelope)
is a **design estimate**, built from the measured metering sizes and
typical NVMe and LAN latencies. The scale run that would measure it (a
synthetic 2,000 VMs on 50 simulated nodes) is listed as not done in
[clustering.md §20](https://github.com/CMKL-APEX/glidex/blob/main/spec/clustering.md#20-implementation-notes).

| Dimension | Designed for | Ceiling (estimate) | Limited by |
|---|---|---|---|
| Hosts | 50 | about 100 | the leader serving watches, heartbeats, forwarded writes and console relays |
| VMs | 2,000 | about 5,000 | the replicated database on every server (metering history: 24 GB at 2,000 VMs) |
| Sustained writes | about 5–20 per second at 2,000 VMs | about 150–300 per second | one write lane; each write about 3–8 ms on NVMe in a LAN (estimate) |
| Burst | 500 VM creates take 20–40 s of write lane | | same lane |
| Snapshot install on a new server | 24 GB: about 3 min at 1 GbE, about 30 s at 10 GbE | | network, then disk |

Raft defaults (configuration): heartbeat 250 ms, election timeout 1–2 s,
node heartbeat every 5 s.

Measured in tests rather than at scale:

- The slowest single cluster test took about 16.5 s on CI
  ([#43](https://github.com/CMKL-APEX/glidex/pull/43), 2026-10-17).
- `cluster_tests` (42 tests) took 265 s in the CI run of 2026-10-22,
  the longest test binary in the workspace.
- Write-set replay checking (`GLIDEX_CHECK_WRITE_SETS=1`) costs little:
  the control plane's tests took 461.9 s plain and 469.2 s with it, on
  CI ([#43](https://github.com/CMKL-APEX/glidex/pull/43)).

## Metering

From
[metering.md §15.6–§15.7](https://github.com/CMKL-APEX/glidex/blob/main/spec/metering.md#156-m3-host-side-io-time-zones-tuning)
and [#13](https://github.com/CMKL-APEX/glidex/pull/13) (2026-10-07).
The load test is `metering/bench.rs`: 200 VMs in 10 projects, 2 NICs
and 2 disks each, 10 networks, a month of history with every 5-minute
slot filled. Hardware not recorded.

| | First build | Current (per-project key ranges) |
|---|---|---|
| One sampling round | 41 ms | 39 ms (budget 15 s) |
| Usage query, one project, one month, by VM | 972 ms | 146 ms (target 200 ms) |
| Bandwidth p95, one project, one month, by VM | 1.65 s | 254 ms |

With 2 days of history and CPU and memory slots (M4): a round takes
49 ms and the CPU and memory p95 query 20 ms; VM slot rows add about
20 % to the slot table.

Size: one day for this fleet is 23k hourly rows (387 B each) and 19k
slot rows (669 B each). The database file grows to about 3× the live
data (copy-on-write pages). At the default retention that is about
2.5 GB for 200 VMs, roughly 12 MB per VM.

Accuracy on a host (2026-10-05, two Debian 13 guests): metered CPU was
within +0.03 % (Cloud Hypervisor) and +0.13 % (QEMU) of the cgroup's
own counter over 300 s of load, and within +0.21 % across a
control-plane restart.

## Images and kernel extraction

From
[kernel-boot.md §3](https://github.com/CMKL-APEX/glidex/blob/main/spec/kernel-boot.md#3-verified-facts)
and [#82](https://github.com/CMKL-APEX/glidex/pull/82),
[#90](https://github.com/CMKL-APEX/glidex/pull/90). Hardware not
recorded.

| Run | Time |
|---|---|
| Prototype extractor (Ubuntu kernel and initrd), per image | 2.5–5 s under Cloud Hypervisor |
| Debian appliance: Ubuntu 26.04, AlmaLinux 9, Fedora 44 | 1.8 s each, Cloud Hypervisor |
| Debian appliance: Debian 13 | 1.7 s, Cloud Hypervisor |
| Debian appliance: Ubuntu 26.04 | 2.2 s, QEMU |

Whole ignored tests (`extracted_kernel_boots_its_image`), 2026-10-19,
each from a fresh data directory, so including the image download and
the appliance build: pull a catalog image, extract its kernel, boot,
log in, check `uname -r`.

| Image | Backend | Time |
|---|---|---|
| Ubuntu 26.04 | Cloud Hypervisor | 84 s |
| Ubuntu 26.04 | QEMU | 78 s |
| AlmaLinux 9 (XFS) | Cloud Hypervisor | 61 s |
| Debian 13 | Cloud Hypervisor | 105 s |
| Fedora 44 (btrfs) | Cloud Hypervisor | 121 s |

Image download speed and image copies between nodes have no recorded
timings.

## UI and TLS

UI bundle, measured on 2026-10-22 from the installed bundle (1 kB =
1,000 bytes; `.gz` and `.br` files are the build's precompressed
copies):

| | Raw | gzip | brotli |
|---|---|---|---|
| First page load (`index.html`, entry, `react`, `rpc`, CSS) | 666 kB | 197 kB | 169 kB |
| Largest chunk: `ghostty-web` (console page only) | 639 kB | 186 kB | |
| Terminal WebAssembly (console page only) | 423 kB | 124 kB | |
| Whole installed bundle, 40 JS chunks, with compressed copies | 3.5 MiB on disk | | |

For comparison, [#45](https://github.com/CMKL-APEX/glidex/pull/45)
(2026-10-18) took the first load from one 820 kB chunk (234 kB gzip)
to 575 kB (175 kB gzip, 155 kB brotli) by splitting vendor chunks and
lazy-loading pages. It has grown with the pages added since.

TLS: the first Tailscale certificate issuance took about 40 s inside
one request, and `tailscaled` was back to `Running` 6 s after a restart
([tailscale-tls.md §9](https://github.com/CMKL-APEX/glidex/blob/main/spec/tailscale-tls.md#9-testing),
2026-10-18).

## Build and CI

CI timings from the workflow `Rust` on `main`: the 26 successful runs
among the 30 most recent, 2026-10-21 10:13 UTC to 2026-10-22 03:49 UTC,
on GitHub's hosted `ubuntu-26.04` runner. Per-step times come from the
Actions jobs API.

| Job or step | Median | Range |
|---|---|---|
| `build` job | 33.5 min | 18.9–36.2 min |
| `cargo build` (debug) | 5.0 min | 3.0–5.9 min |
| `GLIDEX_CHECK_WRITE_SETS=1 cargo test` (builds test targets, then runs) | 28.2 min | 15.4–30.9 min |
| `fmt` job | 0.1 min | |
| `proto` job | 0.2 min | |
| Queue before the `build` job starts | 0.1 min | |

The `build` job's times fall into two groups (about 19–21 min and about
25–36 min); the workflow's comment of "about 20 minutes" matches only
the first. In the run of 2026-10-22, compiling the test targets took
4 min 40 s, and 1,066 tests passed (243 ignored). The slowest test
binaries in that run:

| Test binary | Tests | Time |
|---|---|---|
| `cluster_tests` | 42 | 265 s |
| `connector_tests` | 37 | 97 s |
| `security_tests` | 31 | 56 s |
| `glidex_control_plane` (unit tests) | 355 | 31 s |
| `image_tests` | 19 | 27 s |
| `api_tests` | 33 | 27 s |

CI does not build the web UI or run any ignored test.

Release binary sizes, measured on 2026-10-22 (installed release build,
not stripped):

| Binary | Size |
|---|---|
| `glidex-control-plane` | 88.4 MiB |
| `gxctl` | 56.1 MiB |
| `glidex-ui` | 8.6 MiB |
| `glidex-netd` | 6.0 MiB |
| `glidex-stord` | 3.2 MiB |
| `glidex-authd` | 2.8 MiB |
| `glidex-vm-shim` | 1.3 MiB |

Release build time was not measured.

## Measure it yourself

All of these read only, except where a test says it creates VMs; run
those on a host you can use for testing.

### API latency

On a host running Glidex, as a user who can use the API socket:

```sh
for p in /health /vms /images; do
  for i in $(seq 100); do
    curl -s -o /dev/null -w '%{time_total}\n' \
      --unix-socket /run/glidex-cp/api.sock "http://x$p"
  done | sort -n | awk -v p=$p '{a[NR]=$1*1000}
    END {printf "%s p50 %.1f ms p95 %.1f ms\n", p, a[int(NR*0.5)], a[int(NR*0.95)]}'
done
```

Note the number of objects each list returns, the node's Raft role and
the host's load with the figures.

### Metering load test

```sh
cargo test --release -p glidex-control-plane --lib metering::bench -- --ignored --nocapture
GLIDEX_BENCH_DAYS=2 cargo test --release -p glidex-control-plane --lib metering::bench -- --ignored --nocapture
```

### Real-boot and host tests

The ignored tests in `crates/glidex-control-plane/tests/functional_tests.rs`
boot real guests and print their progress. They need `/dev/kvm`, Cloud
Hypervisor or QEMU, and a UEFI-bootable image; see the file's header
for the variables (`GLIDEX_TEST_IMAGE`, `GLIDEX_TEST_KERNEL`,
`GLIDEX_TEST_INITRD`, `GLIDEX_TEST_CATALOG`, `GLIDEX_TEST_FIRMWARE`,
`GLIDEX_TEST_GPUS`). Time them with `--nocapture` and the shell:

```sh
GLIDEX_TEST_IMAGE=~/images/resolute-server-cloudimg-amd64.raw \
  cargo test -p glidex-control-plane --test functional_tests -- --ignored --nocapture
# one test
time cargo test -p glidex-control-plane --test functional_tests \
  extracted_kernel_boots_its_image -- --ignored --nocapture
```

They include `firmware_boot_with_generated_cloud_init`,
`kernel_boot_with_initrd`, `kernel_image_boot_with_generated_cloud_init`,
`extracted_kernel_boots_its_image`, `guest_kernel_updates_are_followed`,
`vm_survives_control_plane_restart`, `catalog_image_boots_and_root_grows`,
`nat_network_e2e`, `bridged_uplink_e2e`, `afxdp_uplink_e2e`,
`vhost_user_e2e`, `numa_placement_e2e` and `lvm_root_disk_boots`, most
with a `qemu_` twin.

Other ignored host tests:

| Test | Needs |
|---|---|
| `datastore_tests::lvm_disks_on_a_real_pool` | a running `glidex-stord` and a volume group |
| `glidex-stord` `real_lvm` | as above (`GLIDEX_STORD_SOCKET`) |
| `systemd_runner_tests` | KVM, Cloud Hypervisor, a built shim, `GLIDEX_TEST_IMAGE` |
| `real_ovn` (2 tests), `glidex-ovn` `real_nb` | OVN tools, a throwaway northbound database |
| `topology::tests::reads_this_host` | a real host (prints its CPU topology) |

### Nested cluster

Creates two VMs on this host's Glidex and installs a two-node cluster
into them. Needs nested KVM, about 9 GiB of memory, internet access,
and 10–20 minutes:

```sh
time cargo test -p glidex-e2e --test nested_cluster -- --ignored --nocapture
```

### Web UI end to end

Playwright against a throwaway control plane (see
`crates/glidex-ui/e2e/README.md`). With [Bun](https://bun.sh):

```sh
scripts/dev/ui-e2e.sh -i ~/images/resolute-server-cloudimg-amd64.img -H ch
scripts/dev/ui-e2e-full.sh -i ~/images/resolute-server-cloudimg-amd64.img -H ch   # nothing skipped
```

Playwright prints the duration of each test and of the run.

### UI bundle

```sh
cd crates/glidex-ui/ui && bun install && bun run build
ls -l dist/assets | sort -k5 -n | tail
grep -oE '(src|href)="/assets/[^"]+"' dist/index.html   # the first-load files
```

### CI timings

```sh
gh run list --repo CMKL-APEX/glidex --workflow Rust --branch main --limit 30 \
  --json databaseId,conclusion,createdAt,updatedAt
gh api repos/CMKL-APEX/glidex/actions/runs/<run id>/jobs \
  --jq '.jobs[] | select(.name=="build") | .steps[] | [.name, .started_at, .completed_at]'
```

## Gaps

Not benchmarked yet, stated plainly:

- **No `cargo bench` or Criterion suites.** The only load test is the
  ignored `metering/bench.rs`.
- **No cluster scale run.** The §4.1 envelope is an estimate; Raft
  write latency, leader CPU, database growth and snapshot install time
  have not been measured.
- **No API write latency or throughput figures**, and no figures for
  the RPC API, TLS, or requests forwarded from followers.
- **No VM boot, stop or start-to-network times** separate from test
  wall time.
- **No storage throughput** (IOPS or MB/s ceilings) for `dir`,
  `shared-dir` or `lvm` datastores, and no timings for disk moves or
  image copies.
- **No network throughput** for tap, AF_XDP or vhost-user with OVS-DPDK,
  and none across a Geneve tunnel.
- **Hardware is missing** from most recorded host runs.
- **No regression tracking**: nothing is measured on every change.

Suggested next benchmarks:

1. The §4.1 scale run: 2,000 VMs on 50 simulated nodes with a fake
   netd and real Raft on three hosts; record write rate, write latency
   per entry, leader CPU, database size and snapshot install time.
2. An API latency script in the repository (reads and writes, Unix
   socket and TLS, on the leader and on a follower), with object counts.
3. Time from `POST /vms/{id}/start` to `Running` and to the first
   console line, per backend and boot mode, from the ignored tests'
   own timestamps.
4. `fio` and `iperf3` at full rate per datastore driver and per port
   type, with the hardware recorded.
5. A Criterion suite for hot paths: scheduler filtering and scoring at
   50 nodes, OVN NB diff per round, metering queries.
6. Record the hardware with every host run in the specs and pull
   requests.
