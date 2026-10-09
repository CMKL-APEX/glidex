# VM Networking with Open vSwitch (DPDK / AF_XDP)

**Status: implemented (M1–M8).** All design questions are decided (§1).
This document is the implementation spec: component boundaries, wire
protocol, data models, host commands, and milestones with acceptance
tests (§15). §0 records what was verified on a real host and what host
testing changed.

## 0. Implementation status

Crates: `crates/glidex-ovs` (library), `crates/glidex-netd` (daemon +
protocol/client library), control-plane `network.rs`; packaging in
`packaging/glidex-netd.service`.

| Milestone | Unit / API tests | Verified on a real host (Ubuntu 26.04, OVS 3.7.1) |
|---|---|---|
| M1 core, M2 distro install | yes | `gxctl ovs install` installed OVS 3.7.1 (kernel, then dpdk profile) |
| M3 netd | yes | root netd, both sockets, peer-group auth, restart + reconcile |
| M4 NAT | yes | `nat_network_e2e`: DHCP `10.88.0.2`, NAT to the host's gateway |
| M5 installer + UI | yes (installer command list, UI builds) | UI through the Vite proxy; full installer run not done |
| M6 bridged | yes | `bridged_uplink_e2e`: refused, rolled back, committed, VM on LAN, restored |
| M7 DPDK / vhost-user | yes | `vhost_user_e2e` on OVS-DPDK 25.11.0 (no DPDK *NIC* uplink: no IOMMU on the test host) |
| M8 AF_XDP + source build | yes | `afxdp_uplink_e2e` (generic mode, veth); the source build is unit-tested only |

End-to-end tests are `#[ignore]`d in
`crates/glidex-control-plane/tests/functional_tests.rs` and need a root
netd plus, for M6/M8, a fake LAN (two veth pairs into a netns with a
gateway and dnsmasq) — never real NICs.

### Findings from host testing

Each changed the implementation; the sections below already reflect them.

1. **No `set-name` in `network-config`** (§11.5). cloud-init can't rename
   an interface that is already up ("[busy] Error renaming … ens3 to
   eth0"); networkd then waits for an `eth0` that never appears. Matching
   by MAC alone is enough.
2. **Taps get `multi_queue` only with >1 queue pair** (§9). Cloud
   Hypervisor sets `IFF_MULTI_QUEUE` only when `num_queues > 2`, and
   `TUNSETIFF` fails if the flag differs from how the tap was created.
3. **dnsmasq drops privileges** to `nobody`; lease writing works, and its
   files/dirs are `0755`/`0644` so it can re-read the hosts file on
   `SIGHUP` (§7.1).
4. **Stale dnsmasq after an unclean netd exit** kept the gateway's port.
   netd now stops a dnsmasq named by its own pid file whose command line
   uses its own config before starting a new one; restarts back off
   exponentially (2 s … 60 s).
5. **Ubuntu 26.04's plain `openvswitch-switch` includes AF_XDP**
   (`afxdp`, `afxdp-nonpmd`), so combination D needs no source build there.
6. **AF_XDP over veth needs TX checksum offload off on the peer**
   (`ethtool -K <peer> tx off`). Veths leave checksums partial; AF_XDP in
   generic mode forwards the frame as is and guests drop it (`bad udp
   cksum`). Real NICs finish checksums before the wire. Same root cause as
   upstream's "TCP fails on veth in generic mode".
7. **DPDK port types appear only after `init_dpdk`.** The dpdk profile is
   detected from `ovs-vswitchd --version` (`DPDK …` line), not from
   `iface_types` (§6.1).
8. **Ubuntu ships DPDK's ring mempool driver separately**
   (`librte-mempool-ring<abi>`, only *recommended* by `dpdk`). Without it
   every OVS mempool fails with `EINVAL` and vhost-user ports never poll
   (guest: "TX timeout"). The dpdk profile installs with recommends, and
   installs the `librte-mempool-*` packages `dpdk` recommends by name
   (apt skips recommends of already-installed packages). `probe` reports
   `dpdk_mempool_driver`.
9. **DPDK socket memory ≥ 2048 MB.** OVS's default shared mempool is
   262144 mbufs (~600 MB at MTU 1500); size hugepages for OVS *plus*
   hugepage-backed guests. `init_dpdk` now applies changed settings to an
   already-initialized OVS (restart, with confirmation).
   **Jumbo frames:** at MTU 9000 OVS allocates a second fixed 262144-mbuf
   mempool (~2.5 GB) beside the MTU 1500 one, so the installer reserves
   6144 hugepages (capped at a third of RAM), `dpdk-socket-mem` up to 4096,
   and warns below 3072 pages. With the old 4 GiB pool a second guest's
   vhost-user memory mapping failed (`SET_MEM_TABLE`, ENOMEM).
11. **Throughput tuning (`glidex_ovs::tuning`).** vhost-user VM-to-VM
    speed is decided by five things, all now automatic:
    - *PMD placement:* with no explicit `--pmd-cpu-mask`, one PMD per
      physical core (first hyperthread only) on every NUMA node that has
      hugepages, `ceil(cores/4)` per node (max 4); the first core of node 0
      is reserved for the OS and OVS's other threads (`dpdk-lcore-mask`);
      `isolcpus`/`nohz_full` cores are preferred. `dpdk-socket-mem` is split
      per node (`1024,1024`), so ports on node N get a local mempool.
    - *Userspace TSO:* `other_config:userspace-tso-enable=true` (needs the
      restart `init_dpdk` already does). Without it guests exchange
      MTU-sized frames (`tx_tcp_seg_offload=false` in the interface status).
    - *Queues:* a vhost-user NIC defaults to `min(vCPUs, 4)` queue pairs
      (tap stays 1); a single queue pins the NIC to one PMD.
    - *Ring size:* 1024 descriptors (CH `queue_size`, QEMU
      `rx/tx_queue_size`) instead of 256; 256 shows up as
      `ovs_tx_failure_drops`/`ovs_tx_retries`.
    - *Guest pages:* guests with a vhost-user NIC get hugepage-backed
      memory when the host has 2 MiB pages to spare (free − reserved ≥
      guest RAM + max(25%, 256 MiB)), even if `hugepages` wasn't set.
    Verify with `ovs-appctl dpif-netdev/pmd-rxq-show` (every queue on a PMD
    of its port's NUMA node) and `ovs-vsctl get Interface <port> status`
    (`n_rxq`, `vring_*_size`, `tx_tcp_seg_offload=true`).
10. **vhost-user guests were verified with hugepage-backed memory**
    (`hugepages: true`). memfd-only shared memory was not re-tested after
    fixing (8), so hugepages are the supported setup.
11. **`dhcp_managed` / `active_connections`** are detected from
    systemd-networkd lease files and `ss` (established TCP sessions on the
    NIC's addresses); see §8.4.

Written for Cloud Hypervisor first; QEMU uses the same netd ports (§3a). Facts are from the Cloud Hypervisor
v53 API schema and source (`../ch/cloud-hypervisor`), the Open vSwitch
documentation ([AF_XDP][ovs-afxdp], [DPDK vhost-user][ovs-vhost],
[userspace TSO][ovs-tso], [release FAQ][ovs-releases]) and the Ubuntu
26.04 archive. Items marked **(verify)** must be checked on a real host
during the milestone that uses them.

## 1. Decisions

| # | Topic | Decision |
|---|---|---|
| 1 | Privilege model | A root helper, **`glidex-netd`**, from the first milestone; the control plane runs unprivileged. |
| 2 | NIC IP migration | **In scope**, with confirmation (`confirm: true` / `--force`) and automatic rollback. |
| 3 | Hypervisors | **Cloud Hypervisor and QEMU**, on the same ports (§3, §3a). |
| 4 | Addressing | **DHCP, plus NAT through the host's existing route.** Static addressing is later. |
| 5 | DPDK `vfio-pci` binding | **Re-applied by netd at start**; no udev/`driverctl` rule. |
| 6 | Installing OVS | **`glidex-ovs` installs it**: distro packages by profile. |
| 7 | Read-only access | **Status (`probe`) is available to any local user**; listings and changes need the `glidex` group. |
| 8 | Default NAT subnet | **`10.88.0.0/24`**, or the next free `/24` in `10.88.0.0/16` on collision. |
| 9 | Tap capability | **`cap_net_admin+ep` on `cloud-hypervisor` is acceptable.** |
| 10 | Missing OVS features | **Offer a pinned source build** (OVS 3.7.1 LTS + DPDK 25.11.2, §6.3). |

## 2. Background

Open vSwitch has two datapaths: **kernel** (`datapath_type=system`, the
`openvswitch` module switches packets between kernel interfaces) and
**userspace** (`datapath_type=netdev`, `ovs-vswitchd` switches packets in
PMD threads; DPDK and AF_XDP live here).

| Side | Port type | What it is |
|---|---|---|
| Host (uplink) | kernel NIC | Plain interface on a `system` bridge. |
| | `afxdp` | NIC keeps its kernel driver; XDP redirects frames to an AF_XDP socket read by OVS. Linux ≥ 5.4, libbpf/libxdp, OVS built `--enable-afxdp`. *Experimental* upstream. `netdev` only. |
| | `dpdk` | NIC rebound to `vfio-pci`, driven by a DPDK PMD. Highest throughput; the NIC leaves the kernel. `netdev` only. |
| VM | `dpdkvhostuserclient` | vhost-user: OVS maps the guest's virtio rings in shared guest memory. Needs OVS with DPDK and a `netdev` bridge. |
| | tap | Kernel tap. Native on `system`; on `netdev` OVS uses syscalls (works without DPDK, much slower). |

AF_XDP is an uplink technology; VMs on an AF_XDP bridge still attach via
tap or vhost-user.

| # | Datapath | Uplink | VM port | OVS needs | Perf |
|---|---|---|---|---|---|
| A | `system` | none (NAT/isolated) or kernel NIC | tap | kernel profile | baseline |
| B | `netdev` | `dpdk` | vhost-user | `dpdk` + `dpdkvhostuserclient` | highest |
| C | `netdev` | `afxdp` | vhost-user | `dpdk`, `dpdkvhostuserclient` **and** `afxdp` | high |
| D | `netdev` | `afxdp` | tap | `afxdp` | low–medium |

Everything else is rejected with `unsupported`.

## 3. Cloud Hypervisor requirements

- **`NetConfig`** (v53): `tap`, `mac`, `mtu`, `num_queues` (2 × queue
  pairs), `queue_size`, `vhost_user`, `vhost_socket`, `vhost_mode`
  (`"Client"` default / `"Server"`), `id`, `offload_tso|ufo|csum`.
- **vhost-user roles are crossed.** With `dpdkvhostuserclient` (the
  non-deprecated type) OVS is the client, so CH must use
  `vhost_mode: "Server"`. OVS reconnects on its own, so port creation
  and VM boot may happen in either order.
- **Shared guest memory** for vhost-user: `MemoryConfig.shared = true`,
  or `hugepages = true` (implies shared).
- **Tap mode needs `CAP_NET_ADMIN` in CH.** `open_tap_rx_q_0` always
  calls `tap.enable()` (`SIOCSIFFLAGS`) and sets MAC/MTU, even on a
  pre-created tap. Per decision 9, `glidex-install` sets
  `cap_net_admin+ep` on the installed binary; replacing the binary drops
  the capability, so the installer re-applies it and `probe` reports it
  missing (`getcap`).
- Hotplug (`vm.add-net`, `vm.remove-device` by `id`) is not used yet.

## 3a. QEMU requirements

`hypervisor/qemu.rs::nic_args` turns each `NicBinding` into a
`-netdev` / `-device` pair (`id = "net<i>"`, device `dev-net<i>`):

- **tap:** `-netdev tap,id=net0,ifname=<tap>,script=no,downscript=no,vhost=on[,queues=<pairs>]`.
  `vhost=on` (in-kernel vhost-net) when `/dev/vhost-net` opens (usually
  the `kvm` group), otherwise `vhost=off` with a warning. QEMU opens the
  tap as its owner (`user <uid>` at creation) and needs no capability:
  netd sets the MTU (`mtu_request`) and brings the tap up (§9).
- **vhost-user:** `-chardev socket,id=chr-net0,path=<sock>,server=on,wait=off`
  plus `-netdev vhost-user,id=net0,chardev=chr-net0[,queues=<pairs>]`. QEMU
  serves the socket, like CH's `vhost_mode: "Server"`; `wait=off` is safe
  because the guest is held with `-S` until `start`.
- **device:** `virtio-net-pci,netdev=net0,id=dev-net0,mac=<mac>[,host_mtu=<mtu>][,mq=on,vectors=<2·pairs+2>]`.
- **Shared guest memory** (any vhost-user NIC, or `hugepages`):
  `-object memory-backend-memfd,id=mem,size=<M>M,share=on[,hugetlb=on]`
  and `-machine q35,memory-backend=mem`.
- `-nodefaults` keeps QEMU from adding its default user-mode NIC: a VM
  with no networks has no NIC, as under CH.

## 4. Components

```
 gxctl / web UI
       │ REST
       ▼
 glidex-control-plane (unprivileged, group "glidex")
       │ NDJSON over /run/glidex/netd.sock     (0660 root:glidex, all ops)
       │           or /run/glidex/netd-ro.sock  (0666, status only; any local user)
       ▼
 glidex-netd (root, systemd)  ──uses──▶  glidex-ovs (library)
       ▼
 ovs-vsctl · ip · nft · sysctl · dnsmasq · sysfs · apt/dnf · source build
```

Repository changes:

| Path | What |
|---|---|
| `crates/glidex-ovs/` | **New library.** All host-network logic; no dependency on other glidex crates. |
| `crates/glidex-netd/` | **New crate** (lib + bin). Daemon: sockets, auth, protocol, state DB, reconciliation, dnsmasq supervision. Depends on `glidex-ovs`. |
| `crates/glidex-netd/src/proto.rs` | Wire types, exported from the `glidex_netd` library target so the control plane shares them. |
| `crates/glidex-control-plane/src/network.rs` | **New.** Networks, attachments, netd client, REST handlers. |
| `crates/glidex-control-plane/src/…` | `models.rs`, `state.rs`, `api.rs`, `cloud_init.rs`, `hypervisor/cloud_hypervisor.rs`, `bin/gxctl.rs` changes (§11–§12). |
| `crates/glidex-install/src/main.rs` | OVS install step, netd unit, group, `setcap` (§13). |
| `crates/glidex-ui/ui/src/pages/Networking.tsx` | **New** page; network picker in `CreateVmForm`. |
| `packaging/glidex-netd.service` | systemd unit (§7.6). |

## 5. `glidex-ovs` library

### 5.1 Modules

| Module | Responsibility |
|---|---|
| `exec.rs` | `Exec` trait, `SystemExec`, `SudoExec` (installer), `RecordingExec` (tests). |
| `vsctl.rs` | Typed `ovs-vsctl` builders; parse `--format=json` table output. |
| `host.rs` | `probe()` → `HostCapabilities`. |
| `install.rs` | Distro detection, package profiles, pinned source build. |
| `bridge.rs` | Bridges. |
| `nic.rs` | ifname ↔ BDF, driver bind/restore, NIC classification. |
| `uplink.rs` | Kernel / AF_XDP / DPDK uplinks. |
| `ipmigrate.rs` | Snapshot, move, verify, roll back host IP config. |
| `nat.rs` | Gateway address, sysctl, nftables table, dnsmasq config files, subnet selection. |
| `vm_port.rs` | Tap and vhost-user VM ports. |
| `reconcile.rs` | Diff desired vs actual, produce and apply a plan. |
| `names.rs` | Deterministic MAC/port/socket names and length checks. |

### 5.2 `Exec`

```rust
pub struct Cmd { pub program: Program, pub args: Vec<String>, pub stdin: Option<Vec<u8>>, pub timeout: Duration }
pub struct Output { pub status: i32, pub stdout: Vec<u8>, pub stderr: Vec<u8> }

pub trait Exec: Send + Sync {
    fn run(&self, cmd: &Cmd) -> Result<Output, OvsError>;                   // never via a shell
    fn read_file(&self, path: &Path) -> Result<String, OvsError>;            // sysfs, /etc/os-release
    fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), OvsError>;  // sysfs, config files
}
```

`Program` is an enum of the only programs netd ever runs (`OvsVsctl`,
`OvsAppctl`, `Ip`, `Nft`, `Sysctl`, `AptGet`, `Dnf`,
`UpdateAlternatives`, `Getcap`, `Systemctl`, `Tar`, `Meson`, `Ninja`,
`Make`, `Configure`), so no request can name a program. Arguments are a
vector, never a shell string. `OvsVsctl` resolves to `ovs_bin_dir`
(`/usr/bin`, or the source-build prefix, §6.3).

### 5.3 Core types

```rust
pub enum Datapath { System, Netdev }
pub struct BridgeSpec { pub name: String, pub datapath: Datapath, pub mtu: Option<u16>, pub adopt: bool,
                       pub isolated: bool } // isolated network: fenced off from the host (security.md §8.4)

pub enum UplinkKind {
    Kernel { ifname: String },
    Afxdp  { ifname: String, xdp_mode: XdpMode, n_rxq: u16 },
    Dpdk   { pci: String, n_rxq: u16 },
}
pub enum XdpMode { BestEffort, Native, NativeWithZerocopy, Generic }
pub struct UplinkSpec { pub name: String, pub bridge: String, pub kind: UplinkKind, pub migrate_ip: bool }

pub struct NatSpec {
    pub bridge: String,
    pub subnet: Option<Ipv4Net>,         // None: pick per decision 8
    pub dns: bool,                       // default true
}
pub struct NatState { pub subnet: Ipv4Net, pub gateway: Ipv4Addr, pub pool: (Ipv4Addr, Ipv4Addr), pub dnsmasq_running: bool }

pub enum VmPortKind { Tap, VhostUser }
pub struct VmPortSpec {
    pub bridge: String, pub vm_id: String, pub nic_index: u8,
    pub kind: VmPortKind, pub mac: String,
    pub vlan: Option<u16>, pub mtu: Option<u16>, pub queue_pairs: u8,
}
pub enum VmPortBinding {
    Tap { ifname: String },
    VhostUser { socket: PathBuf },       // CH is the vhost-user server
}
```

All types are `serde` (de)serializable and double as protocol payloads
(§7.3). Validation (lengths, ranges, names `[a-z0-9-]`, MAC unicast,
`queue_pairs` 1–8, VLAN 1–4094) happens in netd before any command runs.
The tap owner uid is **not** in `VmPortSpec`: netd takes it from the
connection's `SO_PEERCRED`.

### 5.4 `HostCapabilities` (`probe`)

| Field | Source |
|---|---|
| `ovs_installed`, `ovs_version`, `install_method` (`distro`/`source`) | `ovs-vsctl --version`; netd install record |
| `ovs_running` | `ovs-vsctl --timeout=2 show` succeeds |
| `iface_types`, `datapath_types`, `dpdk_initialized` | `ovs-vsctl get Open_vSwitch . <column>` |
| `hugepages` (per size: total/free) | `/sys/kernel/mm/hugepages/*/` |
| `iommu` | `/sys/kernel/iommu_groups` non-empty |
| `ch_net_admin` | `getcap <cloud-hypervisor>` contains `cap_net_admin` |
| `dnsmasq`, `nft` | binary present |
| `firewall` | `ufw` / `firewalld` active |
| `network_manager` | `systemd-networkd` / `NetworkManager` active |
| `combinations` | which of A–D are possible now, and what's missing for the rest |

`probe` returns host facts only, no glidex objects, which is why it is
safe to expose to every local user (decision 7).

### 5.5 Errors

```rust
pub enum OvsError {
    InvalidArgument(String),
    NotFound(String),
    NotOwned(String),                              // exists but not tagged by glidex
    Conflict(String),                              // in use / name taken
    ConfirmationRequired { impact: String },       // re-send with confirm
    Unsupported { missing: Vec<String> },          // e.g. ["afxdp"]
    HostInterfaceInUse { ifname: String, reasons: Vec<String> },
    MigrationRolledBack { reason: String },
    CommandFailed { program: String, status: i32, stderr_tail: String },
    Io(String),
}
```

Each variant maps to a protocol error code (§7.3) and REST status
(§11.3).

## 6. Installing Open vSwitch

### 6.1 Profiles (distro packages)

| Profile | Ubuntu / Debian | Fedora / RHEL family |
|---|---|---|
| `kernel` | `openvswitch-switch` | `openvswitch` **(verify)** |
| `dpdk` | `openvswitch-switch-dpdk`; `update-alternatives --set ovs-vswitchd /usr/lib/openvswitch-switch-dpdk/ovs-vswitchd-dpdk` | `openvswitch` with DPDK **(verify)** |

Both also install `dnsmasq-base` (Fedora: `dnsmasq`) and `nftables`.
Distro detection reads `ID`/`ID_LIKE` from `/etc/os-release`; other
distros get `Unsupported { missing: ["distro:<id>"] }` for packages and
may use the source build.

On Ubuntu 26.04 (this host) both packages are OVS 3.7.1;
`openvswitch-switch` depends on `libbpf1` and `libxdp1`, suggesting
AF_XDP is built in, and `-dpdk` depends on the DPDK 26 libraries. After
installing, `install_ovs` re-runs `probe` and fails with `Unsupported`
if the features the profile promises aren't in `iface_types`.

### 6.2 Policy

- **Minimum OVS 3.0.** Older versions are reported, not worked around.
- **Idempotent, no downgrades.** If OVS already meets the minimum and
  has the needed features, `install_ovs` changes nothing and returns
  `{"changed": false}`. `kernel` → `dpdk` adds the package and flips the
  alternative; nothing is ever removed.
- **Restart impact.** If `ovs-vswitchd` is running with any bridges,
  installing, switching profile, or `init_dpdk` returns
  `ConfirmationRequired` listing every bridge (glidex-owned or not).
- Non-interactive (`DEBIAN_FRONTEND=noninteractive apt-get -y`), 30 min
  timeout; stdout/stderr tails returned in `InstallReport`.

### 6.3 Pinned source build (decision 10)

Offered when the distro package lacks a feature a requested combination
needs (typically `afxdp`, or `dpdkvhostuserclient` without a DPDK
package), or the distro is unsupported. Always requires `confirm: true`.

Pinned in `install.rs`. OVS 3.7.x is the current LTS, and the OVS
release FAQ pairs it with DPDK 25.11.2:

| Component | Version | URL | sha256 |
|---|---|---|---|
| Open vSwitch | 3.7.1 | `https://www.openvswitch.org/releases/openvswitch-3.7.1.tar.gz` | `b8936c2e95a024d37123536ca843648bc2f1d2520921f991dd3d06248859b70f` |
| DPDK | 25.11.2 | `https://fast.dpdk.org/rel/dpdk-25.11.2.tar.xz` | `418bfe3212640ee95a1cb10af6ed360cad2387686fe2721f8a3a9cd02d5ef4f2` |

The checksums were computed from the published tarballs; a mismatch
aborts before anything is extracted. The DPDK tarball unpacks to
`dpdk-stable-25.11.2/`.

Steps (in `/var/lib/glidex/build/`, removed on success):

1. Install build dependencies (Ubuntu: `build-essential autoconf
   automake libtool pkg-config python3 python3-pyelftools meson
   ninja-build libnuma-dev libssl-dev libcap-ng-dev libbpf-dev
   libxdp-dev`).
2. DPDK: `meson setup build --prefix=/opt/glidex/dpdk-25.11.2` →
   `ninja -C build` → `ninja -C build install`.
3. OVS: `./configure --prefix=/opt/glidex/ovs-3.7.1 --localstatedir=/var
   --sysconfdir=/etc --enable-afxdp --with-dpdk=shared`, with
   `PKG_CONFIG_PATH` at the DPDK prefix → `make -j$(nproc)` →
   `make install`.
4. Install glidex-owned units `glidex-ovsdb-server.service` and
   `glidex-ovs-vswitchd.service` (running `ovs-ctl` from the prefix, with
   `LD_LIBRARY_PATH` for DPDK); enable and start them.
5. Record `{method: source, ovs: 3.7.1, dpdk: 25.11.2, prefix}`; set
   `ovs_bin_dir` to `/opt/glidex/ovs-3.7.1/bin`.

A distro OVS and the source build can't run at once (same OVSDB and
sockets under `/var/run/openvswitch`). If the distro service is active,
the confirmation impact says it will be **stopped and disabled** (not
uninstalled), and `install_ovs` does that before step 4. Bumping the
pins means updating both versions and checksums together, following the
release FAQ's pairing.

## 7. `glidex-netd`

### 7.1 Files and paths

| Path | Owner / mode | Content |
|---|---|---|
| `/run/glidex/netd.sock` | `root:glidex 0660` | full socket |
| `/run/glidex/netd-admin.sock` | `root:glidex-admin 0660` | full socket for the admin group; bound only if that group exists, removed otherwise |
| `/run/glidex/netd-ro.sock` | `root:root 0666` | status-only socket (decision 7) |
| `/run/glidex/vhost/` | `root:glidex 2770` | vhost-user sockets, created by CH |
| `/run/glidex/dnsmasq/` | `root:root 0755` | per-NAT `*.conf`, `*.hosts`, `*.pid` (readable by dnsmasq after it drops to `nobody`) |
| `/var/lib/glidex/netd.db` | `root:root 0600` | ReDB state (§7.4) |
| `/var/lib/glidex/dnsmasq/` | `root:root 0755` | lease files |
| `/etc/glidex/netd.json` | `root:root 0644` | optional config (§7.5) |

### 7.2 Authorization

- The **full socket** is reachable only by `root` and `glidex` members
  (filesystem permissions). On accept netd also reads `SO_PEERCRED` and
  checks the uid is 0 or its groups (`getgrouplist`) include `glidex`;
  otherwise it closes the connection. The uid becomes the tap owner for
  VM ports created on that connection.
- The **admin socket** (`netd-admin.sock`) is the same for the admin
  group (`admin_group`, default `glidex-admin`), so break-glass
  administrators reach netd without joining `glidex` (security.md §4).
  It is bound only when the group exists.
- **Per-op policy** (security.md §8.1), on both full sockets after the
  accept check: `policy` in `netd.json` maps a group name to the ops its
  members may send: op wire names (`release_vm`), `*`, or a prefix glob
  with a trailing `*` (`list_*`; a `*` anywhere else matches nothing and
  is warned about at startup). A request is allowed if the peer is root,
  or any policy group it belongs to (primary or supplementary, resolved
  once per connection) has a matching entry; otherwise it gets
  `permission_denied`. `hello` and `probe` are always allowed (the
  status socket serves them to anyone). Default:
  `{"glidex": ["*"], "glidex-admin": ["*"]}` (with `group` and
  `admin_group` substituted when renamed). Groups that don't exist
  grant nothing.
- **Ownership:** `detach_vm_port` and `release_vm` compare each stored
  port's `owner_uid` with the peer uid and return `not_owned` on a
  mismatch; root is exempt. `release_vm` checks every port before
  detaching any. A port with no record has no owner to check (detach
  stays idempotent). `sync_vms` is not owner-scoped: it reconciles the
  host, and is how ports left by an earlier control-plane uid are
  removed. NAT reservations carry no owner.
- The **status socket** accepts any local user but only `hello` and
  `probe`; anything else returns `permission_denied`.
- Every mutating request is logged to the journal: peer uid, op,
  arguments (the protocol carries no secrets), and the request's
  `on_behalf_of` (or `-`). Denied requests are logged with uid and op.

### 7.3 Protocol

Newline-delimited JSON, UTF-8, max 1 MiB per line, one request in flight
per connection. The first message must be `hello`.

```json
{"id": 1, "op": "hello", "args": {"protocol": 1}}
{"id": 1, "ok": {"protocol": 1, "netd_version": "0.1.0"}}

{"id": 2, "op": "attach_vm_port", "args": {"bridge": "gxbr-nat", "vm_id": "…", "nic_index": 0, "kind": "tap", "mac": "02:5a:1c:9e:44:00", "queue_pairs": 1}}
{"id": 2, "ok": {"binding": {"tap": {"ifname": "gx1a2b3c4d-0"}}, "ipv4": "10.88.0.2"}}

{"id": 3, "op": "install_ovs", "args": {"profile": "dpdk", "confirm": false}}
{"id": 3, "error": {"code": "confirmation_required", "message": "…", "details": {"impact": "ovs-vswitchd will restart; bridges affected: br-int, gxbr-nat"}}}

{"id": 4, "op": "release_vm", "args": {"vm_id": "…"}, "on_behalf_of": {"user": "alice", "project": "default", "request_id": "…"}}
{"id": 4, "ok": null}
```

Any request may carry a top-level `on_behalf_of`: `user` (required
when present), `project` and `request_id` (optional). It is the
caller's claim about who it acts for (security.md §8.3): netd logs it
and never uses it for authorization. Absent fields are omitted, so old
clients and old netds are unaffected. `glidex_netd::client::Client`
sends it with `call_as` / `call_value_as`; `call` sends none.

| Op | Socket | Args → result |
|---|---|---|
| `hello` | all | `{protocol}` → versions |
| `probe` | all | – → `HostCapabilities` |
| `list_bridges` · `list_uplinks` · `list_nat` · `list_vm_ports` | full | – → records with live state |
| `nat_counters` | full (never the status socket) | – → the `inet glidex_meter` counters for the current NAT state: per network and per reserved MAC, `dir` `in`/`out`, bytes (L2-normalized: + 14 × packets), packets and object `handle`. Read-only. netd keeps the table in line with reservations on `ensure_nat`, `delete_nat`, attach and `release_vm`, always incrementally ([metering.md §5.5](metering.md#55-nat-external-counters-inet-glidex_meter), D14). A failure there is logged and never fails the networking operation. |
| `port_stats` | full (never the status socket) | – → per glidex bridge, every port's `role` (`vm`/`uplink`/`gateway`/`other`), VLAN `tag`, VM id and NIC, and OVS `rx`/`tx` bytes, packets and drops as OVS reports them. The `gateway` (internal) port is in the host's view ([metering.md §5.4](metering.md#54-network-bridge-port-counters)). Read-only. |
| `install_ovs` | full | `{profile, source_build, confirm}` → `InstallReport` |
| `init_dpdk` | full | `{socket_mem, pmd_cpu_mask, confirm}` → – |
| `ensure_bridge` · `delete_bridge` | full | `BridgeSpec` · `{name}` |
| `ensure_uplink` | full | `{spec: UplinkSpec, confirm}` → `UplinkResult {record, phase: active\|pending_commit, live, classification}` |
| `commit_uplink` | full | `{bridge, name, token}` → `UplinkResult` |
| `delete_uplink` | full | `{bridge, name}` |
| `ensure_nat` · `delete_nat` | full | `NatSpec` · `{bridge}` → `NatState` |
| `attach_vm_port` · `detach_vm_port` | full | `VmPortSpec` · `{vm_id, nic_index}` → binding (+ `ipv4` on NAT) |
| `release_vm` | full | `{vm_id}` → – (VM deleted: detach ports, free NAT reservations) |
| `sync_vms` | full | `{running: [vm_id]}` → `ReconcileReport` |

"full" means both full sockets (`netd.sock`, `netd-admin.sock`), subject
to the policy (§7.2).

Error codes: `invalid_argument`, `not_found`, `not_owned`, `conflict`,
`confirmation_required`, `unsupported`, `host_interface_in_use`,
`migration_rolled_back`, `command_failed`, `permission_denied`,
`protocol_error`, `internal`.

Client timeouts: `install_ovs` 45 min, `ensure_uplink` 90 s (covers the
commit window), everything else 30 s.

### 7.4 State database

ReDB tables with JSON values:

| Table | Key | Value |
|---|---|---|
| `meta` | `install` | `{method, profile, ovs, dpdk?, prefix?, at}` |
| `meta` | `ip_forward_set_by_glidex` | bool |
| `bridges` | name | `BridgeSpec` |
| `uplinks` | `bridge/name` | `UplinkSpec` + `orig_driver?`, `ip_snapshot?`, `pending: {token, deadline}?` |
| `nat` | bridge | `NatState` + `dns` + `reservations: {mac → ip}` |
| `vm_ports` | `vm_id/nic` | `VmPortSpec` + port name, binding, `ipv4?`, owner uid |

### 7.5 Configuration (`/etc/glidex/netd.json`, optional)

```json
{ "group": "glidex", "commit_window_secs": 60, "gateway_check_secs": 20,
  "nat_supernet": "10.88.0.0/16", "log_level": "info",
  "admin_group": "glidex-admin",
  "policy": { "glidex": ["*"], "glidex-admin": ["*"] } }
```

All keys are optional; the values above are the defaults. `policy`
replaces the default as a whole (§7.2), e.g. to stop the control plane
from installing packages:
`{"glidex": ["list_*", "ensure_*", "delete_*", "commit_uplink", "attach_vm_port", "detach_vm_port", "release_vm", "sync_vms"], "glidex-admin": ["*"]}`.
A missing file means defaults; a file that can't be read or parsed stops
netd at startup, so a typo never silently restores the default policy.
The effective policy is logged at startup.

### 7.6 systemd units (`packaging/`)

`glidex-netd.service` (root):

```ini
[Unit]
Wants=network-online.target openvswitch-switch.service openvswitch.service glidex-ovs-vswitchd.service
After=network-online.target openvswitch-switch.service openvswitch.service ovs-vswitchd.service glidex-ovs-vswitchd.service
Before=glidex-control-plane.service

[Service]
Type=notify              # READY=1 after reconciliation + sockets bound (glidex_netd::sd)
ExecStart=/usr/local/bin/glidex-netd
RuntimeDirectory=glidex
RuntimeDirectoryMode=0755
# vhost-user sockets of running VMs live under /run/glidex/vhost. They
# must survive a netd restart, or OVS can never reconnect to those VMs.
RuntimeDirectoryPreserve=yes
StateDirectory=glidex
Restart=on-failure
TimeoutStartSec=180
ProtectHome=yes
# No CapabilityBoundingSet: install_ovs runs apt/dnf maintainer scripts.

[Install]
WantedBy=multi-user.target
```

`glidex-control-plane.service.in` is rendered by `glidex-install` for the
installing user (`User=`, `HOME`, `SupplementaryGroups=kvm glidex`,
`Wants=`/`After=glidex-netd.service`). Boot order and rationale:
[installer.md](installer.md#boot-sequence-systemd).

### 7.7 Startup and reconciliation

At start netd loads config and state, creates the socket and runtime
directories, runs `probe`, then **reconciles**:

1. **Bridges/uplinks:** re-create missing glidex bridges and uplinks;
   repair `datapath_type`, `n_rxq`, `mtu`; **re-bind DPDK NICs to
   `vfio-pci`** (decision 5); **re-apply IP migrations** from the stored
   snapshot (a reboot puts the host's own config back on the NIC).
2. **NAT:** gateway address, `ip_forward`, the `inet glidex` table
   (replaced atomically with `nft -f`), `GLIDEX-FORWARD` where iptables
   drops forwarded traffic, dnsmasq processes.
3. **VM ports:** kept until the control plane calls `sync_vms`; then
   ports whose VM isn't in `running` are detached (port, tap, socket).
   `running` is every VM that **owns ports** — whose `status.nics` is
   non-empty — not every VM that is running right now (D16,
   [reconciliation.md](reconciliation.md#94-startup-order)): a VM keeps
   its tap and NAT address while it crash-restarts, and an adopted VM is
   never cut off.
4. **Orphans:** glidex-tagged objects with no record are reported in
   `ReconcileReport`, never deleted.

**Invariant.** The control plane sends `sync_vms` only after it has
observed (and adopted) every VM at startup (`controller/startup.rs`),
and again whenever netd has restarted, always with the same set
(`sync_netd_ports`). A restart is noticed on the resync tick: the
identity (device, inode) of `netd.sock` changed since the last sync.
The call holds the VM controller's ports lock, so it never races a
reconcile that is recording and attaching a port. It is not sent at all
while no VM of this control plane uses networks (no `networks` in any
spec, no `status.nics`): netd's sync is host-wide, and a control plane
with nothing to keep must not drop another's ports (a scratch instance
next to the real one). Why: a `sync_vms` sent before adoption would
detach the ports of VMs that kept running through a control-plane
restart.

## 8. Bridges, uplinks and IP migration

### 8.1 Ownership

| Object | Marker |
|---|---|
| OVS bridge / port / interface | `external_ids:glidex-owner=glidex` plus `glidex-role` (`bridge`/`uplink`/`vm`), `glidex-vm-id`, `glidex-nic`, `glidex-uplink`, `glidex-orig-driver` |
| nftables | only table `inet glidex` |
| iptables | only chain `GLIDEX-FORWARD` and its jump from `DOCKER-USER`/`FORWARD` |
| dnsmasq | only processes whose pid files are in `/run/glidex/dnsmasq/` |
| taps | `gx` prefix **and** a `vm_ports` record |

**Invariant:** netd modifies or deletes only marked objects. An untagged
existing bridge name → `not_owned`, unless `adopt: true` (adds tags,
changes nothing else).

### 8.2 Bridges

- `ensure_bridge`: `ovs-vsctl --may-exist add-br <n> -- set Bridge <n>
  datapath_type=<system|netdev> external_ids:glidex-owner=glidex
  external_ids:glidex-role=bridge` (+ `mtu_request` on the internal
  interface). `Netdev` requires `dpdk_initialized` or `afxdp` in
  `iface_types`.
- Names: `[a-z0-9-]`, ≤ 15 chars (the bridge is also a kernel interface).
- `delete_bridge`: `conflict` while it has uplinks, NAT, or VM ports.

### 8.3 Uplinks

| Kind | Commands |
|---|---|
| `Kernel` | `add-port <br> <ifname>` + tags |
| `Afxdp` | `add-port <br> <ifname> -- set Interface <ifname> type=afxdp options:xdp-mode=<mode> options:n_rxq=<n>` + tags |
| `Dpdk` | read `/sys/bus/pci/devices/<bdf>/driver` → record; write `vfio-pci` to `driver_override`; write `<bdf>` to `driver/unbind`, then to `/sys/bus/pci/drivers_probe`; `add-port <br> <name> -- set Interface <name> type=dpdk options:dpdk-devargs=<bdf> options:n_rxq=<n>` + tags |

`delete_uplink` reverses; for DPDK it clears `driver_override` (writes
`\n`) and re-probes so the recorded driver binds again. `UplinkState`
reports `Interface.error`, `link_state`, and for AF_XDP the effective
XDP mode **(verify** which `ovs-appctl` command reports it**)**, since
`best-effort` may fall back to generic.

**VFIO conflict:** the control plane asks `list_uplinks` for DPDK BDFs
and refuses `attach_device` for them (`409`); `GET /pci-devices` marks
them `in_use: "ovs uplink <name>"`.

### 8.4 NIC classification

Before any uplink, `nic.rs` classifies the NIC:

| Reason | Check |
|---|---|
| `has_addresses` | `ip -j addr show dev <if>` lists global addresses |
| `default_route` | `ip -j route show default` uses `<if>` |
| `active_connections` | an established TCP connection (`ss -Htn state established`) uses one of the NIC's addresses — e.g. the SSH session running the CLI |
| `dhcp_managed` | systemd-networkd has a lease file for the NIC (`/run/systemd/netif/leases/<ifindex>`); NetworkManager is not detected **(verify)** |

No reasons → proceed. Any reason → `host_interface_in_use` with the
list, unless `migrate_ip: true` **and** `confirm: true`.

### 8.5 IP migration (decision 2)

1. Snapshot addresses and routes of `<if>` (`ip -j addr`, `ip -j route
   show dev <if>`, plus the default route) into the uplink record.
2. Add the port (§8.3). Move each address to the bridge's internal
   interface (`ip addr del … dev <if>`, `ip addr add … dev <br>`), bring
   `<br>` up, and re-add routes with `dev <br>`.
3. **Gateway check:** if there was a default gateway, a neighbour probe
   on `<br>` must succeed within `gateway_check_secs` (20 s) **(verify**
   the exact `ip neigh` probe mechanism**)**. Failure → roll back,
   return `migration_rolled_back`.
4. Return `UplinkState { state: pending_commit, token, deadline }`. The
   caller must `commit_uplink` with the token within
   `commit_window_secs` (60 s). `gxctl` and the UI do this automatically
   once a fresh `GET /health` through the API succeeds. No commit →
   roll back.
5. **Rollback** restores exactly the snapshot, deletes the port, and for
   DPDK re-binds the recorded driver.

Limits, returned in `UplinkState.warnings`:

- **Runtime only.** netplan/networkd or NetworkManager re-applies the
  NIC's config at boot; netd re-runs the migration at start (§7.7).
  Persistent netplan/NetworkManager changes are out of scope.
- **DHCP-managed NICs:** the lease client stays on the NIC, so the
  migrated address lasts only until lease expiry **(verify)**. Allowed
  with `confirm`, with a warning.

## 9. VM ports

- **Names** (`names.rs`) for VM id `U` (UUID) and NIC `i`:
  - port/tap `gx<first 8 hex of U>-<i>` (≤ 15 chars);
  - MAC `02:` + 5 bytes of `sha256(U ‖ i)`: locally administered
    unicast, stable across restarts;
  - vhost socket `/run/glidex/vhost/<U>.net<i>.sock` (< 108 bytes).
- **Tap:** `ip tuntap add dev <port> mode tap [multi_queue] user <uid>`
  (`multi_queue` only when `queue_pairs > 1`, matching what the
  hypervisor asks for);
  `ovs-vsctl --may-exist add-port <br> <port> -- set Interface <port>
  [mtu_request=<mtu>] external_ids:…` (+ `set Port <port> tag=<vlan>`);
  then `ip link set dev <port> up`. OVS doesn't bring a tap up, and QEMU,
  unlike CH (decision 9), has no `CAP_NET_ADMIN` to do it itself.
- **VhostUser:** `ovs-vsctl --may-exist add-port <br> <port> -- set
  Interface <port> type=dpdkvhostuserclient
  options:vhost-server-path=<sock> [mtu_request=<mtu>] external_ids:…` (+ tag). Requires a
  `netdev` bridge and `dpdkvhostuserclient`.
- On a NAT bridge, attach allocates the lowest free pool address for the
  MAC (kept for the VM's lifetime, freed when the VM is deleted) and
  rewrites the dnsmasq hosts file (`SIGHUP`).
- `detach_vm_port`: `--if-exists del-port`, `ip link del <tap>`, remove
  the socket. Idempotent.

## 10. NAT networks (decisions 4, 8)

`ensure_nat` on a glidex bridge with no uplink:

1. **Subnet:** the requested one, or the first free `/24` in
   `nat_supernet` (`10.88.0.0/16`) that overlaps no host route (`ip -j
   route`) or existing NAT. Gateway `.1`, pool `.2–.254`.
2. **Address:** `ip addr replace <gw>/24 dev <br>`; `ip link set <br> up`.
3. **Forwarding:** if `net.ipv4.ip_forward` is 0, set it to 1 and record
   `ip_forward_set_by_glidex` (never reset automatically; other services
   may depend on it).
4. **nftables:** regenerate the whole table from all NAT records and
   apply it atomically with `nft -f -`:
   ```
   table inet glidex {
     chain postrouting {
       type nat hook postrouting priority srcnat;
       ip saddr 10.88.0.0/24 ip daddr != 10.88.0.0/24 masquerade
     }
     chain forward {
       type filter hook forward priority filter;
       iifname "gxbr-nat" oifname { "gxbr-p1" } drop      # one per NAT bridge, when there are several
       iifname "gxbr-nat" ip saddr 10.88.0.0/24 accept
       oifname "gxbr-nat" ct state established,related accept
     }
     chain input {
       type filter hook input priority filter;
       iifname { "gxbr-nat", … } ct state established,related accept
       iifname { "gxbr-nat", … } udp dport 67 accept                 # DHCP
       iifname "gxbr-nat" ip daddr 10.88.0.1 udp dport 53 accept    # DNS, when dns = true
       iifname "gxbr-nat" ip daddr 10.88.0.1 tcp dport 53 accept
       iifname "gxbr-nat" ip daddr 10.88.0.1 icmp type echo-request accept
       iifname { "gxbr-nat", … } drop
     }
   }
   ```
   No outgoing interface is named, so traffic follows the host's
   existing route (decision 4). NAT networks are isolated from each
   other and from the host (security.md §8.4): the per-bridge drops come
   before the accepts, and from a NAT bridge the host answers only DHCP,
   DNS on the gateway, ping to the gateway, and replies to connections it
   opened. Every other packet to any host address is dropped (IPv6 too;
   NAT networks are IPv4-only). Other interfaces are untouched (both
   chains are `policy accept`).
   **iptables FORWARD drop:** an accept in `inet glidex` can't overrule a
   drop in another table at the same hook, so when iptables' `filter`
   table drops forwarded traffic netd also maintains chain
   `GLIDEX-FORWARD` (the same two accepts per network, via `iptables
   -w`) and one jump to it at the top of `DOCKER-USER` when that chain
   exists (Docker sets FORWARD to DROP and leaves `DOCKER-USER` to
   admins), else of `FORWARD` when its policy is DROP. Rebuilt with the
   nftables table; removed with the last NAT network or when nothing
   drops. The unit is ordered `After=docker.service` so the chain exists
   at boot.
   **iptables INPUT drop:** likewise for traffic to the host. When
   iptables' `INPUT` policy is DROP (ufw's default-deny), netd maintains
   chain `GLIDEX-INPUT` with the same accepts as the `inet glidex` input
   chain (DHCP, DNS when served, ping on the gateway) and one jump to it
   at the top of `INPUT`. Nothing broader is accepted; the nftables drop
   still covers the rest from the bridges. Rebuilt and removed together
   with `GLIDEX-FORWARD`. The unit is also ordered `After=ufw.service`.
5. **dnsmasq**, a supervised child of netd (restarted if it exits):
   `dnsmasq --keep-in-foreground --conf-file=/run/glidex/dnsmasq/<br>.conf`
   with:
   ```
   interface=<br>
   bind-interfaces
   except-interface=lo
   dhcp-range=<pool-start>,<pool-end>,12h
   dhcp-option=option:router,<gw>
   dhcp-hostsfile=/run/glidex/dnsmasq/<br>.hosts
   dhcp-leasefile=/var/lib/glidex/dnsmasq/<br>.leases
   pid-file=/run/glidex/dnsmasq/<br>.pid
   # dns = true:
   listen-address=<gw>
   resolv-file=/run/systemd/resolve/resolv.conf   # when systemd-resolved is active
   # dns = false:
   # port=0
   ```
   `<br>.hosts` holds one `<mac>,<ip>` line per attached VM port.

**Default network:** at control-plane start, if netd is reachable, OVS
is installed, and no network named `default` exists, the control plane
creates it: bridge `gxbr-nat` (`system`), `ensure_nat` with the default
subnet, and network `default` (tap). Failure is logged, not fatal.

**Firewalls:** `probe` reports active `ufw`/`firewalld`; a drop in their
forward chains still wins over `inet glidex`. (A plain iptables FORWARD
DROP, Docker's included, is handled by `GLIDEX-FORWARD`, and an
`INPUT` DROP such as ufw's default-deny by `GLIDEX-INPUT`, above.) `GET /ovs/status` shows
the command needed to allow the NAT subnet **(verify)**.

## 11. Control plane

### 11.1 Models

```rust
pub enum NetworkMode { Nat, Bridged, Isolated }

/// Control-plane `networks` table: the VM-facing view only.
/// Host state lives in netd.
pub struct Network {
    pub name: String,              // [a-z0-9-], ≤ 32
    pub bridge: String,            // a glidex bridge in netd
    pub mode: NetworkMode,
    pub port_type: VmPortKind,     // Tap | VhostUser
    pub vlan: Option<u16>,
    pub mtu: Option<u16>,
    pub owns_bridge: bool,         // created its bridge (and NAT); removes them on delete
    pub created_at: u64,
    // project, all_projects, grants, shares, share_offers: security.md §6.2
    // Written by the network controller only (§11.2a); defaulted, so
    // older records load as `Ready`:
    pub phase: NetworkPhase,       // Ready | Degraded | NetdUnavailable
    pub conditions: Vec<Condition>,
    pub deletion_requested_at: Option<u64>, // set by DELETE (§11.3); absent otherwise
}

pub struct NetworkAttachment {
    pub network: String,
    #[serde(default)] pub mac: Option<String>,        // default: names.rs
    #[serde(default)] pub queue_pairs: Option<u8>,    // default 1, max 8
}

// VmConfig additions (serde defaults keep old records loadable):
pub networks: Vec<NetworkAttachment>,   // max 8
pub hugepages: bool,
```

`VmResponse` gains `networks: [{network, mac, ipv4?}]`, with `ipv4` from
netd's reservation on NAT networks.

### 11.2 netd client (`network.rs`)

- A blocking `UnixStream` client run in `spawn_blocking`. Each call
  opens a short connection (`hello` + request), so a netd restart needs
  no reconnect logic. It uses `netd.sock`; for `probe` it falls back to
  `netd-ro.sock` when the full socket isn't accessible. `sync_vms`
  follows a netd restart as in §7.7.
- No socket or connection refused → `NetdUnavailable`.

### 11.2a Network controller

`controller/network.rs` ([reconciliation.md §10.3](reconciliation.md#103-network))
keeps netd's records in line with the `networks` table; netd reconciles
the host from its own records. Each round (on create, and every resync)
calls `list_bridges` (and `list_nat` for NAT networks) and records a
`phase` and a `Ready` condition on the network:

| `phase` | When |
|---|---|
| `ready` | the bridge is in netd and live on the host; for NAT, netd has its NAT and, with `dns`, its dnsmasq is running |
| `degraded` | something is missing; `Ready=False/Degraded` names it, event `Degraded` |
| `netd_unavailable` | netd could not be reached (`Ready=Unknown`) |

- A bridge the network owns (`owns_bridge`, NAT and isolated networks)
  that netd lost, as a record or on the host, is re-created with
  `ensure_bridge` (event `BridgeRestored`). An isolated network's bridge
  whose netd record lacks `isolated` (from before the flag) is ensured
  again with it (event `BridgeUpdated`), so netd fences it
  ([security.md §8.4](security.md)).
- A lost NAT is re-created with `ensure_nat` (new subnet; event
  `NatRestored`) **only while no VM uses the network**. With VMs on it it
  is reported, not re-created: its subnet lives only in netd, and a new
  one would renumber them. A dnsmasq that is not running is reported (netd restarts it in its own reconcile).
- A bridged network whose bridge is no longer a glidex bridge is
  reported; glidex never takes over a bridge on its own.

**Deletion.** A network with `deletion_requested_at` is torn down
instead. While a VM still uses it: `Ready=False/InUse`, phase
`degraded`, retried every 10 s. Then, for a network glidex created (`owns_bridge`): netd
`delete_nat` (NAT mode), then `delete_bridge` if netd still has the
bridge; a bridged network only drops its record. With netd unreachable
it waits (`netd_unavailable`, `Ready=Unknown/NetdUnavailable`); a netd
error gives `Ready=False/DeleteFailed` (phase `degraded`, event
`DeleteFailed`); both are
retried every 30 s. Finally the record is removed (event `Deleted`).

Only `phase` and `conditions` are written by the controller; grants and
shares written meanwhile are kept. `GET /networks/{name}/events` returns
the network's event ring.

### 11.3 REST

| Method + path | netd op | Success |
|---|---|---|
| `GET /ovs/status` | `probe` + reachability | 200 |
| `POST /ovs/install` `{profile, source_build?, confirm?}` | `install_ovs` | 200 `InstallReport` |
| `POST /ovs/dpdk-init` `{socket_mem, pmd_cpu_mask, confirm?}` | `init_dpdk` | 204 |
| `GET` · `POST /ovs/bridges`, `DELETE /ovs/bridges/{name}` | `list_bridges` · `ensure_bridge` · `delete_bridge` | 200 · 201 · 204 |
| `GET` · `POST /ovs/bridges/{b}/uplinks`, `DELETE …/{u}`, `POST …/{u}/commit {token}` | uplink ops | 200 · 201 or 202 (pending) · 204 · 200 |
| `GET` · `POST /networks` | control-plane store (+ `ensure_bridge`/`ensure_nat` for NAT) | 200 · 201 |
| `DELETE /networks/{name}[?wait=N]` | records `deletion_requested_at`; the controller calls `delete_nat`/`delete_bridge` (§11.2a) | 204 when gone at once, else 202 with the network; with `wait`, 200 once gone |
| `POST /vms` with `networks` | validation only | 201 |

Error mapping:

| netd / local error | HTTP | `error` |
|---|---|---|
| `invalid_argument` | 400 | `invalid_network` |
| `not_found` | 404 | `not_found` |
| `not_owned`, `conflict` | 409 | `conflict` |
| `host_interface_in_use` | 409 | `host_interface_in_use` (+ `details.reasons`) |
| `confirmation_required` | 409 | `confirmation_required` (+ `details.impact`) |
| `migration_rolled_back` | 409 | `migration_rolled_back` |
| `unsupported` | 422 | `unsupported_on_host` (+ `details.missing`) |
| `command_failed`, `internal`, `protocol_error` | 502 | `netd_error` |
| netd unreachable | 503 | `netd_unavailable` |
| `permission_denied` | 503 | `netd_permission_denied` (control plane not in `glidex`) |

`create_vm` validation: unknown network (400); ≤ 8 attachments; a custom MAC must be unicast;
VhostUser forces `memory.shared = true`. Deleting a network used by any
VM (spec or live NICs) → `409` (`deleteNetwork`, or `deleteProjectNetwork`
for a project network). While a network is being deleted, attaching a VM
to it is `400` ("network 'x' is being deleted") and creating one with
the same name is `409` ("is being deleted; try again once it is gone").

### 11.4 VM lifecycle

VM ports belong to the VM controller
([reconciliation.md §9.1](reconciliation.md#91-reconcile)); no API
handler calls netd for them.

- **Launch** (desired running, no live instance): for each attachment,
  first add `{network, nic_index, mac}` to `status.nics` (a store
  write), then `attach_vm_port`; record `port` and `ipv4`. The bindings
  become `--net` / `-netdev` arguments (§3, §3a) with `id = "net<i>"`
  and `num_queues = 2 × queue_pairs`; vhost-user sets shared memory.
  Ports of a NIC the spec no longer has are detached first. If the
  launch fails before the instance is recorded, the VM's ports are
  detached again (`launch_round`).
- **Ports survive crash-restarts** (D16): between an unexpected exit and
  the relaunch the ports stay attached, and `attach_vm_port` on a port
  whose tap still exists reuses it (`vm_port::attach` skips `ip tuntap
  add` and uses `--may-exist`), so the VM keeps its tap and NAT address.
- **Stop** (desired stopped, no live instance any more):
  `detach_vm_port` per `status.nics` entry, removing each entry only
  after netd confirmed; with netd unavailable the entry stays and the
  next reconcile or netd's own `sync_vms` finishes it.
- **Delete:** the `vm.ports` finalizer calls `release_vm` (ports and NAT
  reservations).
- A guest that powers itself off takes its hypervisor with it; the shim
  records the exit, the controller notices through its exit watch
  (`pidfd`) or the periodic resync, sets the desired state to stopped
  and detaches the ports as above.
- **Port drift** ([reconciliation.md §10.3](reconciliation.md#103-network)):
  each round of a running VM compares its `status.nics` ports with the
  live bridge (`list_bridges`). An OVS port that is gone while its tap
  remains, or a missing vhost-user port, is re-added with
  `attach_vm_port` (event `PortRestored`). A tap that is gone is not
  re-created, because the hypervisor holds an fd to the old device:
  `NetworkReady=False/PortLost` and `RestartRequired=True/PortLost`
  until the VM is restarted. With netd away nothing is compared.

### 11.5 Guest `network-config`

With attachments, `cloud_init::SeedConfig` takes the NIC list and
writes one entry per NIC matched by MAC:

```yaml
version: 2
ethernets:
  net0:
    match: { macaddress: "02:5a:1c:9e:44:00" }
    dhcp4: true
```

No `set-name`: see §0 finding 1.

Without attachments the current "DHCP on `en*`" stays.

## 12. `gxctl` and UI

`gxctl` commands (Tab-completion `COMMANDS` updated):

| Command | Calls |
|---|---|
| `ovs status` | `GET /ovs/status` |
| `ovs install [--profile kernel\|dpdk] [--source] [--force]` | `POST /ovs/install` |
| `ovs dpdk-init --socket-mem 2048 --pmd-cpu-mask 0x4 [--force]` | `POST /ovs/dpdk-init` |
| `bridges` · `bridge-add <name> [--netdev]` · `bridge-rm <name>` | bridges |
| `uplink-add <bridge> <name> (--kernel <if> \| --afxdp <if> [--xdp-mode m] \| --dpdk <bdf>) [--migrate-ip] [--force]` · `uplink-rm <bridge> <name>` | uplinks; auto-commit after a successful `health` |
| `networks` · `network-add <name> (--nat [--subnet s] \| --bridged <bridge> \| --isolated) [--vhost-user] [--vlan n]` · `network-rm <name>` | networks |
| `create` | new prompt: networks (comma list, default `default`) |

On `confirmation_required`, `gxctl` prints `details.impact` and asks for
a re-run with `--force`.

UI: a **Networking** page (status panel with missing-capability hints
and an Install OVS button; bridges with uplinks; networks), a
confirmation dialog for `confirmation_required`, a networks multi-select
in `CreateVmForm`, and NICs/IPs in `VmDetail`. The page also creates
project networks ("Network for"), and without host rights lists the
networks the selected project can use
([web-ui.md](web-ui.md#images-and-disks-pages)).

## 13. `glidex-install`

New step after Cloud-Hypervisor, every part prompted:

1. **Group:** create `glidex` and add the invoking user.
2. **OVS:** `install_ovs` through `SudoExec`, `kernel` profile by
   default or `dpdk`; offers the pinned source build if `probe` shows a
   missing feature.
3. **netd:** install `glidex-netd` to `/usr/local/bin` and
   `packaging/glidex-netd.service`, then `systemctl enable --now`. This
   is an explicit exception to [installer.md](installer.md)'s "does not
   write systemd units".
4. **CH capability:** `setcap cap_net_admin+ep` on the installed
   `cloud-hypervisor` (decision 9), after printing what it grants.

## 14. Security

> Superseded for authentication, authorization and NAT isolation by
> [security.md](security.md) (§4, §8). The notes below describe netd's
> own invariants.

- Only netd is privileged. It runs only the `Program` allow-list with
  vector arguments, validates every field, enforces the ownership
  invariant and host-NIC guard itself, and takes the tap owner from
  `SO_PEERCRED`, never from requests.
- `glidex` group membership = network-admin rights on the host (with
  confirmations). Only host status is public (decision 7).
- **The REST API is unauthenticated**, so anyone reaching port 8841
  gets the control plane's netd rights. M4 therefore changes the
  control plane's default bind from `0.0.0.0:8841` to `127.0.0.1:8841`
  (configurable). Superseded: with authentication and TLS in place
  (security.md §5.1), the default is HTTPS on every address again.
- `cap_net_admin+ep` on `cloud-hypervisor` applies to anyone who can run
  it (decision 9).
- vhost-user: OVS maps all guest RAM and is fully trusted by its VMs.
- Source builds verify the pinned sha256 before extracting.

## 15. Milestones

Each milestone is one PR (or a short series), mergeable on its own, with
its tests passing in CI.

| # | Scope | Acceptance |
|---|---|---|
| **M1** `glidex-ovs` core | `exec`, `vsctl` (+ JSON parsing), `names`, `host::probe`, ownership tags, `bridge` (`system`), `vm_port` (tap) | Unit tests with `RecordingExec`: exact commands, idempotency flags, `not_owned` on an untagged bridge, name/MAC length and stability, probe parsing from canned output. |
| **M2** distro install | `install.rs` profiles, version policy, confirmation impact | Unit tests: package choice per `/etc/os-release` fixture and profile, no-op when satisfied, `confirmation_required` when bridges exist, post-install feature check. |
| **M3** `glidex-netd` | both sockets + permissions, `SO_PEERCRED` auth, protocol, state DB, reconciliation skeleton, systemd unit | Protocol round-trip tests; status socket rejects everything but `hello`/`probe`; non-group peer rejected; `/run/glidex/vhost` survives a netd restart. |
| **M4** NAT networking (first user-visible) | `nat.rs`, dnsmasq supervision, reservations, subnet choice; control-plane `network.rs`, models, REST, lifecycle, `network-config`, default network, `127.0.0.1` bind; `gxctl` network commands | Unit + API tests against a fake netd; ignored e2e (root, OVS): a VM on `default` gets `10.88.0.x` by DHCP and reaches an address outside the test namespace; detach cleans tap, port and reservation. |
| **M5** installer + UI | §13 steps, Networking page, create-form picker | Installer test with `RecordingExec`; UI type-checks and builds; manual run on a fresh VM. |
| **M6** bridged networks | `Kernel` uplinks, classification, IP migration with gateway check, commit/rollback, re-apply at start | Unit tests for classification and every rollback path (gateway check fails, no commit, wrong token); ignored e2e in a netns with a veth "NIC" carrying an address: migrate, commit, flush + reconcile, rollback. |
| **M7** DPDK | `dpdk` profile, `init_dpdk`, `netdev` bridges, `Dpdk` uplinks (bind/restore, re-bind at start), vhost-user ports, `MemoryConfig.shared/hugepages`, VFIO conflict | Unit tests for sysfs writes and restore; ignored e2e with `GLIDEX_TEST_OVS_DPDK=1`: two VMs over vhost-user ping each other. |
| **M8** AF_XDP + source build | `Afxdp` uplinks, effective-mode reporting, pinned source build | Unit tests for build steps and checksum failure; ignored e2e: AF_XDP generic mode over a veth pair. |

Every ignored e2e test runs in a throwaway network namespace, uses veth
pairs instead of real NICs, and tears down its bridges, the `inet
glidex` table and dnsmasq even on failure. CI runs only unit and API
tests.

## Appendix A: QEMU

Implemented; see §3a. Verified on the host below with the ignored
`qemu_nat_network_e2e`, `qemu_vhost_user_e2e` and
`mixed_hypervisors_share_a_network` tests (QEMU 10.2.1).

## Appendix B: OVS notes

- Userspace TSO is off by default (`other_config:userspace-tso-enable`),
  applies to vhost-user ports, and needs an OVS restart plus a guest
  reconnect.
- Don't set `n_rxq` on vhost-user ports; it follows the guest's queues.
  Multiqueue needs ≥ 2 PMD threads.
- AF_XDP upstream limitations: no QoS (bypasses tc), no VLAN offload on
  veth, TCP over generic-mode veth is unreliable.
- Host tuning stays manual: hugepages (kernel command line for
  persistence), IOMMU (`intel_iommu=on` / `amd_iommu=on`), `isolcpus`,
  NUMA locality. `probe` reports the gaps.

## Host observed while writing this

Ubuntu 26.04, `systemd-networkd` via netplan, nftables,
`net.ipv4.ip_forward=1`. Open vSwitch is not installed (archive: 3.7.1
for `openvswitch-switch` and `-dpdk`), nor is `dnsmasq` (archive:
`dnsmasq-base` 2.92). `grout` (a DPDK router) is installed; it could
implement the VM-port operations as an alternative vhost-user backend.

[ovs-afxdp]: https://docs.openvswitch.org/en/latest/intro/install/afxdp/
[ovs-vhost]: https://docs.openvswitch.org/en/latest/topics/dpdk/vhost-user/
[ovs-tso]: https://docs.openvswitch.org/en/latest/topics/userspace-tso/
[ovs-releases]: https://docs.openvswitch.org/en/latest/faq/releases/
