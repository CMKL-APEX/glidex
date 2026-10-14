# DPU Integration: virtio-net ports on OVS-DOCA, switched by glidex's own OVN

**Status: design, not implemented.** Nothing in this document exists in
the code yet. §0 records what was verified on a real BlueField-3 pair
while writing it; items marked **(verify)** must be checked on hardware
during the milestone that uses them (§14).

Builds on [networking.md](networking.md) (`glidex-ovs`, `glidex-netd`,
VM ports, vhost-user), [security.md](security.md) (netd policy, ownership)
and [reconciliation.md](reconciliation.md) (VM controller, `status.nics`).
Read those first; this document only describes what changes.

glidex **runs its own OVN instance** (NB DB, SB DB, `ovn-northd`, and a
host `ovn-controller`) next to the OVS it already manages. That instance
is what lets one logical network span a DPU chassis and a host-OVS
chassis, so a VM can be switched by the DPU when it is available and by
host OVS-DPDK when it is not (§11), and it lets overlay networks work on
a host with no DPU at all.

## 0. Verified on hardware

Test pair: two hosts, each with a BlueField-3 B3140L (single 100G port,
DPU mode, firmware 32.50.1002), DOCA 3.5.0 bundle on the DPU (Ubuntu
24.04, kernel `bluefield-64k`), OVS `3.5.0052` (DOCA build), host kernel
6.8 on AMD EPYC with `iommu=pt`. Addresses, hostnames, MACs and system
IDs are deliberately left out of this document.

| # | Finding | Consequence |
|---|---|---|
| 1 | The DPU's OVS lists `iface_types` `doca`, `docavdpa` and `datapath_types` `doca`, `netdev`, `system`. `dpdk` is **not** a supported port type; it is accepted as an alias. | Bridges use `datapath_type=doca`, ports `type=doca` (§9). |
| 2 | `type=doca` ports need **no `dpdk-devargs`**. OVS-DOCA derives it (`<pci>,dv_flow_en=2,dv_xmeta_en=4,probe_opt_en=1,representor=…`). Clearing the options in OVSDB changes nothing until `ovs-vswitchd` restarts. | The module sets only name and type. |
| 3 | OVS-DOCA needs `other_config:doca-init=true` plus a restart; `hw-offload=true`. `doca_initialized` and `doca_version` appear in the `Open_vSwitch` record. | `dpu_probe` reads them (§9.2). |
| 4 | Hugepages: the DPU kernel's default size is **512 MB**; `vm.nr_hugepages` therefore allocates 512 MB pages (13 pages = 6.5 GB of a 15 GB DPU). OVS-DOCA ran from a 2 MB pool via `other_config:dpdk-hugepage-dir=/hugepages-2M` on a dedicated `hugetlbfs` mount; about 140 MB was in use. | Allocate 2 MB pages through `/sys/kernel/mm/hugepages/hugepages-2048kB/` and set `dpdk-hugepage-dir`; never `vm.nr_hugepages`. |
| 5 | `other_config:pmd-cpu-mask` accepted; PMD threads visible in `dpif-netdev/pmd-stats-show`. `pmd-rxq-show` errors on this build ("please specify an existing datapath"). | Use `pmd-stats-show` for verification. |
| 6 | Restarting `ovs-vswitchd` or changing a bridge's datapath type recreates the bridge's internal (tap) port; its IP and routes are lost. | A restart is an outage of the DPU underlay; the module re-applies addresses (§9.3). |
| 7 | Firmware starts with virtio-net emulation **off**: `VIRTIO_NET_EMULATION_ENABLE=0`, `NUM_PF=0`, `PF_TOTAL_SF=0`; `virtio-net-controller` is installed and enabled but inactive. `devlink resource` shows capacity for 32 local and 32 external SFs. | Virtio ports are impossible until firmware is changed and the DPU is cold-reset (§9.1). glidex never does this itself. |
| 8 | Host: `vdpa`, `vhost_vdpa`, `mlx5_vdpa`, `virtio_vdpa`, `virtio_vfio_pci`, `iommufd` modules **exist** for kernel 6.8; QEMU is 8.2.2 and libvirt 10.0. `vfe-vhostd` is not installed. No `pci=realloc`. | VFIO passthrough of a virtio VF works with the host as it is; migration paths need newer QEMU/kernel (§2, §15). |
| 9 | The Geneve tunnel between two DPUs came up with **one** DPU on the kernel datapath and the other on OVS-DOCA. | Mixed datapaths interoperate on the wire; backends may differ per host. |
| 10 | The test OVN central services (NB and SB) listened on plain TCP on all interfaces, unauthenticated. | glidex's own instance is TLS-only for anything that leaves the host, and NB is never on the network (§8.3). |
| 11 | The DPU runs **OVN 24.03.8** (`ovn-controller`, `ovn-northd`; Ubuntu 24.04 packages), SB schema **20.33.0**. The host's Ubuntu 24.04 archive offers `ovn-central` and `ovn-host` **24.03.8** as well. | A distro-installed central matches the DPU's chassis version (§8.8). |
| 12 | The test DPU **already runs its own `ovn-central`** (NB, SB, northd all active) and its OVS `ovn-remote` pointed at it. | A DPU may carry a foreign OVN central. The module reports it and repoints the chassis only with confirmation (§8.6, §9.3). |
| 13 | The host reaches the DPU over a point-to-point management link (`tmfifo_net0`, a pair of private addresses), independent of every OVS bridge. | That link is the agent and SB transport and survives OVS restarts (§10). |
| 14 | The host runs `rshim` 2.8.5 (service active and enabled) with one device, `/dev/rshim0` (`boot`, `console`, `misc`, `rshim`), bound to the SoC management function (`DEV_NAME` is the PCI address of function `.1`). `misc` reports `BF_MODE` `DPU mode`, `BOOT_MODE` `1 (emmc)`, `BOOT_TIMEOUT` 300 s, `SECURE_NIC_MODE` 0. `bfb-install`, `mlxfwreset`, `mlxconfig` and `mlnx-fw-updater` 26.07 are installed on the host. | The reinitialization path (§9.4) uses `bfb-install --rshim rshim0`; the device identity can be checked before anything is written. |
| 15 | A BFB matching the DPU's running bundle (`bf-bundle-3.5.0-89_26.07_ubuntu-24.04_64k_prod`) is already on the host, in two places. The DPU's root filesystem is on NVMe (117 GB, 7% used). No `/etc/bf.cfg` exists on the DPU; its image identity is in `/etc/bfb_version.json`. | glidex does not download BFBs; the operator supplies the path and the expected checksum (§9.4). A reinstall replaces everything on that filesystem. |
| 16 | **D0 run 1.** A DPU on a clean bundle (3.5.0-89) starts with `hw-offload=true` already set, `doca-init` unset, `ovsbr1` on the `system` datapath with ports of unset type, **no IPv4 address on `ovsbr1`** (the underlay address of earlier tests was added by hand), no OVN packages, and eswitch mode already `switchdev`. `iface_types` lists `doca` only **after** `doca-init` and a restart. | A clean DPU needs `dpu_prepare` to supply the underlay address and, if OVN is wanted on the DPU, to install it; the probe must treat "no `doca` in `iface_types`" as "not initialized", not "unsupported". |
| 17 | `dpu-prepare.sh` from that state applied **13 changes in one pass** (1024 × 2 MB pages plus mount and persistence, `doca-init`, `dpdk-hugepage-dir`, `pmd-cpu-mask`, one `ovs-vswitchd` restart, `ovsbr1` and a new `br-int` on `doca`, three ports to `type=doca` with no options), and its own verification was green (`doca_initialized`, `doca@ovs-doca`, no interface errors, 2 PMD threads). A second run with `--check` reported **0 changes**. `br-int` was created on `doca` with `fail_mode=secure` while no OVN package was installed. | D0 acceptance (1) is met on the test DPU. The §9.3 order is proven from clean; `dpu_prepare` (D5) is checked against this script. |
| 18 | After conversion about 70 of the 1024 2 MB pages were in use (about 140 MB), matching the earlier hand-built run; `dpif/show` lists the auto-derived options (`dv_xmeta_en=4,dv_flow_en=2,probe_opt_en=1,representor=…`) for each port. | The 2 GB default pool is comfortably large (finding 4 stands). |
| 19 | **D0 reinstall, two runs.** `bfb-install -r rshim0 -c bf.cfg -b <bfb>` finished in **310 s and 304 s** (exit 0). The rshim console shows the whole sequence: firmware stages (`BL1`, `BL2`, `BL31`, `UEFI`), then `Found bf.cfg`, **`Erasing eMMC drive: /dev/mmcblk0` and `Erasing NVME drive: /dev/nvme0n1`**, `Installing OS image`, `Running bfb_modify_os from bf.cfg`, `Installation finished`, `Rebooting...`. `bfb-install` also printed `Failed to get BMC token` and skipped the BMC upgrade (no BMC credentials were given, as intended). | The install is about **5 minutes**, not 10 (O15). It erases **both** drives, so the pinned `device=` chooses where the OS lands but the eMMC is wiped too (O16). The console log is the reliable progress source: phases can key on `Found bf.cfg`, `Installation finished` and `Rebooting...`. |
| 20 | After the reinstall the DPU answered on the management link with a **new SSH host key** and sshd offered **only `publickey`**, so the sshd drop-in written by `bfb_modify_os` took effect. Key-only login with the key that `bfb_modify_os` installed was nevertheless **rejected** (`Permission denied (publickey)`) in both runs, for the `ubuntu` account (and for `root`), 7 and 20 minutes after the install. A second change (numeric uid/gid, home directory chown) did not fix it. | Hook writes under `/mnt` work, but key placement as written does not authenticate; the cause is **unknown** (O14, O17). The host key changes on every reinstall, so the post-install checks must trust the new key on first use (the script now does). |
| 21 | Output of `bfb_modify_os` (`echo`, `cat` of its own log) **does not appear** in the rshim console capture; only the installer's own `INFO[MISC]` lines do. | A hook cannot diagnose itself through stdout; use the console's own log function or another sink, or read `/var/log/glidex-bfcfg.log` on the DPU. |
| 22 | `bfb-install` could not write `/tmp/bfb-install-rshim0.log` (`Permission denied`: a file left in the sticky `/tmp` by an earlier user's run) and carried on. | `dpu_reinit` (D5b) gives each run a private working directory. |
| 23 | **Root cause of the key rejection (D0 runs 1–3), from the hook's own log and the live DPU.** At `bfb_modify_os` time the `ubuntu` account **does not exist** in the image (`/etc/passwd` has no entry; cloud-init creates it at first boot), `/home/ubuntu` does not exist, and `chage` is not on the installer's `PATH` (`install`, `chroot`, `sed` are). The hook's `install -d` created `/home/ubuntu`, `.ssh` and `authorized_keys` **owned by root (uid 0), mode 0600**. sshd reads `authorized_keys` **as the account**, so a root-owned 0600 file is unreadable and the key is silently ignored. Confirmed on the DPU: as `ubuntu`, a root-owned 0600 file cannot be read, and the same file after `chown` to the account can. The live DPU later showed `.ssh` and the key file owned by the account (re-owned by hand) while `/home/ubuntu` itself was still root-owned, and key login worked: a root-owned *home directory* is acceptable to sshd, the unreadable key file was the problem. The hook's `echo`/`cat` output reaches the DPU's own console/kernel log but not the host's rshim capture. | A hook must not create files that the account has to own or read before the account exists, and cannot clear its password expiry. glidex's account is therefore created at first boot by a vendor-data shell script (findings 26-28), and a fallback key lives in a **world-readable** root-owned path. |
| 24 | **cloud-init runs on first boot** (read from the BFB: `ubuntu/install.sh` and `ubuntu/image.tar.xz` inside its initramfs, bundle 3.5.0-89). The image ships `/usr/bin/cloud-init` with `datasource_list: [ConfigDrive, NoCloud]` and a NoCloud seed at `/var/lib/cloud/seed/nocloud-net/{user-data,meta-data,network-config}`. The seed's `user-data` has `write_files` (iptables rules, including a port-22 limit of 50 new connections per 60 s per source), a `users:` entry for `ubuntu` (groups, passwordless sudo, a plain-text default password, `lock_passwd` false), **`chpasswd: expire: True`**, and a `runcmd`. The installer itself edits that seed (`set_root_password`, for `ubuntu_PASSWORD`) and appends `PasswordAuthentication yes` and `PermitRootLogin yes` to the **end** of `/etc/ssh/sshd_config`, whose `Include /etc/ssh/sshd_config.d/*.conf` is at line 12 (first match wins, so a drop-in overrides). | Confirms finding 23: cloud-init creates the account after the hook has run. The expiring default password is the "first-login gate" (O17): a non-interactive key-only session is likely refused until the password is changed. The installed cloud-init is **26.1**; its NoCloud datasource reads an optional `vendor-data` file next to `user-data` (the live DPU has `vendor-data` slots in `/var/lib/cloud/instance/`, empty). The rate limit is far above any polling rate. |
| 25 | **Superseded by run 4: a second `users:` list in vendor-data does not work.** Read from the live DPU: the shipped seed is `/var/lib/cloud/seed/nocloud-net/user-data` (120 lines; `write_files`, `users:` with only `ubuntu`, `chpasswd`, `runcmd`). cloud-init 26.1 assembles its config as: files passed in, environment, **instance cloud-config (user-data, then vendor2, then vendor-data)**, datasource config, base; the default merge is `list()+dict()+str()`, which **replaces** lists. An earlier test of cloud-init's `mergemanydict` on raw dicts that still contained `merge_how` returned `['ubuntu', 'glidex']`; that input was unrealistic (finding 26). A `users:` list in `cloud.cfg.d` is ignored as well. Vendor-data processing is not disabled in the shipped config. cloud-init decides "first boot" by `instance-id`, which the shipped `meta-data` fixes to the constant `nocloud`: vendor-data is read on the **first boot of a freshly installed image only**; adding it later to a running DPU does nothing (and `cloud-init clean` would redo the image's own setup too), so day-2 changes go through the agent, not cloud-init. | Do not deliver the account as a `users:` entry in any extra config file. |
| 26 | **D0 run 4, from the DPU's own logs.** The hook wrote a 384-byte `vendor-data` correctly (hook log) and cloud-init **read it**: `Reading 384 bytes from /var/lib/cloud/seed/nocloud-net/vendor-data`, `vendordata will be consumed`, and the instance directory holds `vendor-data.txt` and `vendor-cloud-config.txt` (319 bytes). The saved `vendor-cloud-config.txt` contains the `glidex` user entry but **no `merge_how`**: cloud-init consumes that key while parsing the part and strips it. The system-level merge therefore used the default list replacement, `users_groups` logged only `Adding user ubuntu`, and `glidex` was never created. | A cloud-config part cannot carry merge instructions to the user-data/vendor-data merge. Use a key the shipped user-data does not define, or a script. |
| 27 | **A vendor-data shell script is the working route.** cloud-init's `scripts_vendor` module (it ran in the log; semaphore `config_scripts_vendor`) runs a script from vendor-data once per instance, as root, late in first boot, with no merging. The generated script was run as a one-off on the live DPU: it created a **system account** (uid 995, gid 987, home `/var/lib/glidex`), `passwd -S` reported `L` and the shadow field starts with `!` (no usable password), the key file in its home is 0600 and owned by the account, the sudoers entry (0440) passed `visudo -c`, and **key-only login and passwordless sudo worked** as that account. The test account was removed afterwards; `ubuntu` was never touched. | The hook installs the account as a first-boot script (§9.4.1). Still to confirm in run 5: that cloud-init runs it unprompted at first boot. |
| 28 | **D0 run 5: the reinstall works end to end.** `bfb-install` 304 s (exit 0). cloud-init saved the vendor-data script as `scripts/vendor/part-001` and its `scripts_vendor` module ran it about 10 s after `users_groups` had created `ubuntu`; the bootstrap log shows `glidex bootstrap start/done` and `sudoers ... parsed OK`. **Key-only, non-interactive SSH as `glidex` worked 115 s after `bfb-install` returned** (435 s for the whole job). Account checks: `glidex` uid 995 / gid 987, home `/var/lib/glidex`, `passwd -S` `L`, shadow field starts with `!` (no usable password); `ubuntu` exists with its own password (`P`), untouched. Block devices after: eMMC `mmcblk0` present, OS on `nvme0n1` (`p1` 50M `/boot/efi`, `p2` root); `BOOT_MODE` `1 (emmc)` before and after; same bundle before and after. The staged firmware values (`VIRTIO_NET_EMULATION_*`, `PER_PF_NUM_SF`, `PF_TOTAL_SF`, `PF_SF_BAR_SIZE`) were **identical before and after** the reinstall, but the host shows **no virtio-net functions** (`lspci -d 1af4:` is empty): staged, not active. | D0 acceptance (3) is met. The SSH wait must cover the late first-boot script (about 2 minutes here). A BFB reinstall neither clears nor applies staged firmware values; applying them needs the cold reset §9.1 leaves to the operator. |
| 29 | **Firmware activated; the controller will not start while the image's SF 0 exists.** After the operator's reset and host reboot (`pci=realloc initcall_blacklist=virtio_pci_driver_init`), `mlxconfig` reads the virtio-net values as active (`ENABLE=1`, `NUM_PF=1`, `NUM_VF=16`, `NUM_VF_MSIX=12`, `PER_PF_NUM_SF`) on host and DPU alike. `virtio-net-controller` nevertheless reported `active` while its worker `virtio_net_emu` exited at once with `virtnet_ib_ports_init: Failed to init IB port, err(-2)` and no JSON-RPC server came up (`virtnet list`: "Can't connect to virtnet controller"). An `strace` of the start shows the cause: the worker reads the netdevs of the PF (`en3f0pf0sf0`, `phys_port_name` `pf0sf0`) and then scans only `/sys/bus/pci/devices/*/infiniband` for the matching RDMA device; the stock SF's `mlx5_1` lives on the **auxiliary** bus (`mlx5_core.sf.1`), so the scan ends empty (ENOENT). That SF is the image's own: `/etc/mellanox/mlnx-sf.conf` runs `mlnx-sf --action create --device <pf> --sfnum 0 …` at boot and `dpu-prepare.sh` had put it on `ovsbr1` as a port. Removing it (`ovs-vsctl del-port`, then `mlnx-sf --action delete --device <pf> --sfindex pci/<pf>/<index>`) and restarting the controller fixed it: the RPC servers came up and `virtnet list` answered. | **A DPU with virtio-net emulation must not carry the stock SF 0.** `dpu_prepare` removes it, neutralises the `mlnx-sf.conf` line (else a DPU reboot recreates it and the controller breaks again) and restarts the controller (§9.3 step 2b; `dpu-prepare.sh --virtio-net`). `mlnx-sf delete` needs the full `pci/<pf>/<index>` as `--sfindex` and `mlxdevm` on `PATH`, which `sudo` does not provide (`/opt/mellanox/iproute2/sbin`). The probe reports "controller active, worker not serving" as its own state. |
| 30 | **The controller's own SF and the static PF.** Once serving, `virtnet list` shows controller `v26.07.10`, `emulation_manager` `mlx5_0`, `max_virt_net_devices` 1, the virtio feature set (including `VIRTIO_F_SR_IOV`, `VIRTIO_F_ADMIN_VQ`, `VIRTIO_F_RING_PACKED`), and one **static PF**: host address `61:00.1` (`1af4:1041`), 64 MSI-X, 63 queues, link up, MTU 1500, 100000 Mb/s, **`pci_max_vfs` 0 and `enabled_vfs` 0**. It created its own SF (`sf_num` 1000, representor `en3f0pf0sf1000`, RDMA `mlx5_1`) and left it unbridged. On the host the PF is `virtio-pci`-class with no driver bound (the initcall blacklist) and `sriov_totalvfs` 16, `sriov_numvfs` 0. | VFs are **not** enabled by the firmware values alone; how they are enabled (host `sriov_numvfs` vs a `virtnet modify`) and where the representors go (O1) are D0 acceptance (4). A virtio PF's representor is `en3f0pf0sf<1000+n>`. |
| 31 | **Host-side facts after the reset.** The firmware change moved the rshim function from `61:00.1` to `61:00.2` and made `.1` the virtio-net PF; the PCI window above the card was sized for the old layout, so `.2` had no BAR and rshim failed with "unable to read from rshim" until the host rebooted with `pci=realloc`. While the host's `virtio-pci` driver was allowed to probe the new PF, its probe blocked the PCI rescan in uninterruptible sleep (the DPU's controller was not serving it, finding 29); only a reboot cleared it. A level-3 reset (`mlxfwreset`) did **not** reset the DPU's Arm side (it kept its uptime and its `mlx5_0`/`mlx5_1` from before). The DPU's SSH (account `glidex`/`ubuntu`) is reachable over `tmfifo_net0` after the reboot. | The firmware activation procedure (§9.1) is host reboot with `pci=realloc` **and** `initcall_blacklist=virtio_pci_driver_init` (the driver is built in on Ubuntu 6.8), rshim defaults restored (no `rshim.conf` pin: its function moves), and the controller served **before** any virtio PF is probed. Finding 14's `.1` is the old layout; rshim's function is whatever `15b3:c2d5` is. |

## 1. Decisions

| # | Topic | Decision |
|---|---|---|
| 1 | Where DPU logic lives | New library crate **`glidex-dpu`** (no dependency on other glidex crates, like `glidex-ovs`); wrapped by **`glidex-netd`**, which already holds root and the ownership model. |
| 2 | How glidex reaches a DPU | A small **DPU agent** (`glidex-dpu-agent`, systemd on the DPU) speaking the netd NDJSON protocol over the DPU's point-to-point management link; **not** `ssh` + `sudo` from netd. Mutual authentication; only netd's root credential can connect (§10). |
| 3 | Guest-visible device | A **virtio-net VF** emulated by the DPU (`virtio-net-controller`), attached to the VM by **VFIO passthrough** in v1. vhost-user via `vfe-vhostd` is v2 (§5.2). |
| 4 | Switching and policy | **OVN**, with `ovn-controller` and `br-int` on the DPU (OVS-DOCA, offloaded). The VM's port is an OVN logical switch port. |
| 5 | OVN central | **glidex runs its own instance** (decision revised): `glidex-ovn-nb`, `glidex-ovn-sb`, `glidex-ovn-northd`, glidex-owned systemd units with their own run and data directories, installed and maintained by netd. An **external** central stays supported (`ovn.mode: external`, §8.10). |
| 6 | Where the internal central runs | **On the host**, next to netd. NB is reachable only through a local Unix socket; SB listens on a local Unix socket and, over TLS, on the DPU management link (§8.3). Running it on the DPU is deferred (O9). |
| 7 | Fallback | If the DPU is unavailable, a network that allows it falls back to **OVS-DPDK vhost-user on the host** (existing M7 path), with the host as a second chassis of the same internal OVN (§11). |
| 8 | Backend stickiness | A NIC's backend is decided at attach and **never changes while the VM runs** (the hypervisor holds the device). |
| 9 | Firmware and reset | **Never applied by glidex.** `dpu_probe` reports what is missing and prints the commands (§9.1). The one DPU reset glidex can drive is a **BFB reinstall of the DPU's operating system** over rshim (decision 14, §9.4); it never changes firmware settings and never power-cycles the card or the host. |
| 10 | Live migration | **Out of scope** for v1 (glidex is single-host). The design leaves room: a stable VF ↔ port mapping and OVN `requested-chassis` (§15). |
| 11 | Datapath types | `doca` for bridges, `doca` for ports on a DOCA DPU. Never `dpdk`/`netdev` there. |
| 12 | PKI | glidex runs a **private CA** for OVN. Chassis private keys are generated on the chassis and never leave it (§8.3). |
| 13 | NB is derived state | NB rows are generated from netd's own records; a lost or damaged NB is rebuilt, not restored (§8.7). |
| 14 | DPU reinitialization | An explicit, confirmed, **destructive** operation, `dpu_reinit`: netd writes an operator-supplied, checksum-verified **BFB** to the DPU through `rshim` (`bfb-install`), then re-bootstraps the agent, re-enrols the chassis certificate and re-runs `dpu_prepare`. It runs as an asynchronous **job**, is admin-only, refuses while any DPU-backed VM is running, and is never triggered automatically (§9.4). |

## 2. Scope

**In scope**
- Install, configure, supervise and upgrade a single-node OVN central on
  the host, and a host `ovn-controller` chassis, from netd.
- Create, list and delete a virtio-net port for a VM NIC on an OVN-managed
  network, switched by OVS-DOCA on the DPU attached to this host.
- Probe and (with confirmation) configure the runtime parts of a DPU:
  hugepages, `doca-init`, bridges, OVN chassis settings and certificates.
- Reinitialize a DPU's operating system from a BFB over rshim and bring
  it back to a working chassis (§9.4).
- Fall back to host OVS-DPDK vhost-user for networks that allow it, and
  run overlay networks on hosts that have no DPU.
- REST, `gxctl` and UI surfaces consistent with networks today.

**Non-goals**
- Firmware (`mlxconfig`) changes, cold resets and power cycles of the
  card or host, and `pci=realloc` / kernel command-line edits: reported,
  never performed. NIC firmware updates through the BFB are off by
  default (§9.4).
- Obtaining BFBs: glidex does not download, build or customize bundles
  beyond the first-boot configuration file it generates (§9.4).
- **OVN clustering or HA.** One central, on this host. When it is down,
  no port or network changes are possible, but traffic already switched
  keeps flowing (§8.7).
- OVN routers, NAT, gateways, ACLs (§8.11 says what is deferred).
- SR-IOV VFs of the regular ConnectX PF as VM NICs, SFs as VM NICs,
  `docavdpa`, and hotplug virtio PFs. Static virtio PF + VFs only.
- Live migration (decision 10).
- **Multi-host orchestration.** [README.md](README.md) lists it as a
  non-goal, and this module respects it: glidex manages *its own host's*
  OVN, DPU and VMs. The internal SB does not listen on the underlay or
  any address except the ones configured (§8.3), and glidex never talks
  to another host's DPU. A chassis from another host could connect to the
  SB if an operator adds its address to `sb.listen` and issues it a
  certificate by hand; that is unsupported, not forbidden.

## 3. Topology

```
 Host (glidex)                                         DPU (OVS-DOCA, DPU mode)
┌─────────────────────────────────────────────┐      ┌────────────────────────────────────────┐
│ control plane ──NDJSON──▶ glidex-netd       │─mgmt▶│ glidex-dpu-agent                       │
│                            │ ovn-nbctl      │ link │   ├ ovs-vsctl (br-int, ovsbr-phy)      │
│  glidex-ovn-nb  ◀──unix────┘                │      │   └ virtnet (virtio-net-controller)    │
│  glidex-ovn-sb  ◀─unix─ glidex-ovn-controller│      │ ovn-controller ──TLS──▶ host SB ───────┼─(mgmt link)
│  glidex-ovn-northd                          │      │                                        │
│                                             │      │ VF representor ─ br-int ═ Geneve ═▶ …  │
│  VM ── virtio-net VF (vfio) ◀──PCIe─────────┼─────▶│                  └ ovsbr-phy ─ p0 ─▶ wire
│  fallback: VM ── vhost-user ─ host OVS-DPDK br-int (chassis "host") ═ Geneve ═▶ same switch │
└─────────────────────────────────────────────┘      └────────────────────────────────────────┘
```

Two bridges on the DPU, as in NVIDIA's tunnel guidance: **`ovsbr-phy`**
holds the uplink and the underlay IP; **`br-int`** is the OVN integration
bridge. Names are configuration, not constants.

Open (**verify** O1): the virtio-net service guide says an SF
representor must be "in the same OVS bridge of the uplink representor".
That conflicts with OVN, which wants workload ports on `br-int`. The
module therefore treats the bridge a virtio representor joins as
configuration (`representor_bridge`, default `br-int`) and the first
hardware milestone (D4) decides which layout works.

## 4. Components

| Path | What |
|---|---|
| `crates/glidex-ovn/` | **New library.** Installing OVN packages, rendering the three central unit files and the host `ovn-controller` unit, the private CA and certificate issuing, `ovn-nbctl`/`ovn-sbctl`/`ovn-appctl` command builders, the NB model (§8.4) and its reconciler, status parsing. No dependency on other glidex crates. |
| `crates/glidex-dpu/` | **New library.** DPU-side command builders (`ovs-vsctl`, `virtnet`), VF allocation, DPU probe parsing. |
| `crates/glidex-dpu-agent/` | **New binary**, runs on the DPU. Protocol server around `glidex-dpu`; allow-listed programs only. No OVN central access. |
| `crates/glidex-netd/` | New ops and a `Backend` abstraction (§6); owns the OVN instance (§8), holds the DPU credential and the CA key, and does the host-side steps (VF creation, `vfio-pci` binding, and the `rshim`/`bfb-install` reinitialization jobs of §9.4). |
| `crates/glidex-ovs/` | Unchanged API. Gains a `Backend::HostOvs` adapter over today's `vm_port` (§11). |
| `crates/glidex-control-plane/` | Models, REST, controller changes (§12). |
| `packaging/` | `glidex-ovn-nb.service`, `glidex-ovn-sb.service`, `glidex-ovn-northd.service`, `glidex-ovn-controller.service` (host); `glidex-dpu-agent.service` (DPU); `dpu.json` and `ovn.json` examples. |

`glidex-ovn` and `glidex-dpu` have an `Exec` trait like `glidex-ovs`
(`crates/glidex-ovs/src/exec.rs`) and a `RecordingExec` for tests; the
agent supplies the real implementation on the DPU, netd supplies one for
host commands.

## 5. Ports

### 5.1 Port kinds

`VmPortKind` (`crates/glidex-ovs/src/vm_port.rs:15`) gains one variant;
`VmPortBinding` gains one, with the same serde style:

```rust
pub enum VmPortKind    { Tap, VhostUser, DpuVirtio }        // + DpuVirtio
pub enum VmPortBinding {
    Tap { ifname: String },
    VhostUser { socket: PathBuf },
    Vfio { bdf: String },                                    // + host PCI address of the virtio VF
}
```

`VmPortSpec` gains optional fields (defaults keep old records valid):

```rust
pub struct VmPortSpec {
    /* existing */
    pub backend: BackendPolicy,            // Auto (default) | Dpu | Host
    pub ovn: Option<OvnPortSpec>,          // set for overlay networks
}
pub enum BackendPolicy { Auto, Dpu, Host }
pub struct OvnPortSpec {
    pub switch: String,                    // logical switch, e.g. "gx-<project>-<network>"
    pub addresses: Option<String>,         // "<mac> <ip>" or "<mac>"; default: mac only (DHCP)
    pub port_security: bool,               // default true: mac (+ ip when static)
}
```

The result carries the chosen backend:

```rust
pub struct AttachResult { pub binding: VmPortBinding, pub backend: Backend /* Dpu | Host */,
                          pub ipv4: Option<Ipv4Addr> }
```

### 5.2 Attachment modes (DPU backend)

| Mode | Guest sees | Host needs | Status |
|---|---|---|---|
| `vfio` | a PCI virtio-net device (stock `virtio_net`) | `vfio-pci` (kernel 6.8 ok) | **v1** |
| `vhost-user` | virtio-net via a vhost-user socket from `vfe-vhostd` | the NVIDIA `vfe` packages (source unknown, **verify** O4) | v2; reuses today's `VhostUser` binding and CH/QEMU server-mode args |

The mode is a per-DPU setting (`dpu.json`, §10), not per VM.
`dpu_probe` reports which are possible.

### 5.3 VF inventory and naming

- The DPU exposes **one static virtio PF** with up to `NUM_VF` VFs
  (firmware, §9.1). A VF is identified by `(pf, vf)`.
- `glidex-dpu` allocates the lowest free VF, recorded in netd's
  `vm_ports` record as `dpu_vf: {pf, vf}`; freed on detach.
- Representor: the VF's SF representor (`en3f0pf0sf<3000+n>` on the test
  DPU; the exact numbering comes from `virtnet list`, **verify** O2).
  Its OVS port name is the representor netdev name; glidex tags it.
- Names of OVN objects (deterministic, `names.rs` style):
  - logical switch port `gx<first 8 hex of vm id>-<nic>` (same as the tap
    name today, ≤ 15 chars);
  - MAC `02:` + 5 bytes of `sha256(vm_id ‖ nic)` (unchanged).

## 6. Backend abstraction in netd

```rust
trait PortBackend {
    fn probe(&self) -> BackendHealth;                    // available? why not?
    fn attach(&self, spec: &VmPortSpec) -> Result<AttachResult, OvsError>;
    fn detach(&self, vm_id: &str, nic: u8) -> Result<(), OvsError>;
    fn list(&self) -> Result<Vec<PortRecord>, OvsError>;
}
// HostOvsBackend: today's vm_port::attach/detach (tap, vhost-user), plus the OVN iface-id on overlay networks.
// DpuBackend:     agent client + host VFIO steps (§7).
```

`attach_vm_port` selects a backend:

| `backend` | DPU healthy | Result |
|---|---|---|
| `Dpu` | yes | DPU |
| `Dpu` | no | error `dpu_unavailable` |
| `Host` | n/a | host |
| `Auto` | yes | DPU |
| `Auto` | no, network allows fallback (§11) | host, event `BackendFallback` |
| `Auto` | no, no fallback | error `dpu_unavailable` |

**Invariant:** the chosen backend is stored in the `vm_ports` record and
re-attach (crash-restart, `attach_vm_port` on an existing record) always
uses the **recorded** backend, even if the DPU has since recovered.
Why: the hypervisor holds the device or socket; only a VM restart can
switch.

"Healthy" means: agent reachable, `ovs-vswitchd` has `doca_initialized`,
the controller reports the PF, a VF is free, and the DPU chassis is
registered in the internal SB (§8.6). Cached for 5 s so a launch burst
probes once. Every overlay attach also requires the internal OVN to be
healthy (§8.7); otherwise `ovn_unavailable`, whatever the backend.

## 7. Port lifecycle (DPU backend)

### 7.1 Attach

All steps are idempotent; each records progress in the `vm_ports` record
so a crash resumes.

1. **Validate** (netd, no side effects): ids, MAC, MTU, queue pairs
   (≤ the controller's per-VF maximum, default 4), network, switch name.
2. **Allocate VF** from the DPU inventory (`dpu_list_vfs`); `dpu_capacity`
   if none is free. Store `dpu_vf`.
3. **Host: make sure the VF exists.** Read the virtio PF's
   `sriov_totalvfs`/`sriov_numvfs`; if the VF is not yet enabled write
   `sriov_numvfs` (all-or-nothing change: raising the count with VFs in
   use fails in the kernel, so netd sets it to `total_vfs` once, at the
   first attach, and never lowers it while any port exists).
4. **DPU: configure the VF** through the agent: MAC, MTU, queue count
   (`virtnet modify device …`, exact flags **verify** O2). The VF's
   MAC/MTU/MSI-X must be set **before** the host binds a driver.
5. **DPU: wait for the representor** (≤ `port_timeout_secs`, default 30),
   then `ovs-vsctl --may-exist add-port <representor_bridge> <rep> -- set
   Interface <rep> type=doca external_ids:iface-id=<lsp>
   external_ids:glidex-owner=glidex external_ids:glidex-role=vm
   external_ids:glidex-vm-id=<id> external_ids:glidex-nic=<n>`; `ip link set <rep> up`;
   align the representor MTU with the VF's.
6. **OVN: create the logical port** on the **internal NB** (§8.5), from
   netd, with the local `ovn-nbctl`; then wait for the port to be `up`
   in the SB (`lsp-get-up`, ≤ `port_timeout_secs`). On timeout roll back
   steps 5–6 and return `dpu_port_timeout`.
7. **Host: bind the VF to `vfio-pci`**: `driver_override`, unbind,
   `drivers_probe` (same sysfs sequence as the DPDK uplink,
   `crates/glidex-ovs/src/uplink.rs`); record the original driver. Return
   `Vfio { bdf }`.

The hypervisor then opens the device through the existing VFIO launch
path ([hypervisors.md](hypervisors.md)); the only differences from a
user-attached `vfio_devices` entry are that netd owns it and it appears
in `status.nics`, not in the spec's device list.

### 7.2 Detach and release

Reverse order; every step tolerates "already gone": rebind the original
driver (or leave unbound if none), delete the NB port, `del-port` on the
DPU, free the VF. `release_vm` does this for every NIC of a VM.
`detach_vm_port` and `release_vm` keep the owner-uid check from
[networking.md §7.2](networking.md#72-authorization).

### 7.3 Failure rules

- Each step's undo is run in reverse on failure; the result is the
  original error, with undo failures appended to `details`.
- If the DPU becomes unreachable mid-attach the host-side effects (VFIO
  bind) are undone first; DPU-side leftovers are found by the next
  reconcile (§7.4) because they carry `glidex-owner` tags.
- A port record exists only after step 7 returns; before that a
  `pending` marker lets reconcile finish or undo.

### 7.4 Reconciliation (at netd start, on agent reconnect, and every resync)

Netd diffs its `vm_ports` records against the DPU
(`dpu_list_ports`), the internal NB and the host:

| Situation | Action |
|---|---|
| Record present, DPU port or NB port missing | re-create (steps 4–6); event `PortRestored` |
| DPU port or NB row tagged glidex, no record | **report** in `ReconcileReport`, never delete ([networking.md §7.7](networking.md#77-startup-and-reconciliation) rule 4) |
| Host rebooted (VFs gone; `sriov_numvfs` = 0) | VMs are stopped anyway; netd recreates VFs at the next attach |
| DPU reset (representors gone, controller restarted) | running VMs lose their NIC: `NetworkReady=False/PortLost`, `RestartRequired=True/PortLost`, same as a lost tap |

The NB side of this table is the generic NB reconcile of §8.7.

## 8. OVN

### 8.1 Modes and ownership

`ovn.mode` in `/etc/glidex/ovn.json` (§10.1):

| Mode | Who runs central | Used when |
|---|---|---|
| `internal` (**default**) | glidex (§8.2) | always, unless the operator points glidex at a central of their own |
| `external` | the operator (§8.10) | an existing OVN deployment glidex must join |
| `off` | nobody | no overlay networks; netd leaves OVN alone |

Whatever the mode, the NB model (§8.4) and the ownership rule are the
same. **Invariant** (extends [networking.md §8.1](networking.md#81-ownership)):
every NB row glidex creates carries `external_ids:glidex-owner=glidex`
(plus `glidex-project`, `glidex-vm-id`, `glidex-nic`); the module changes
or deletes only tagged rows; a switch name that exists untagged is
`not_owned`. In `internal` mode glidex owns the whole instance, but the
rule still holds: it keeps an operator's own `ovn-nbctl` additions from
being deleted by a reconcile.

### 8.2 The internal instance

Four **glidex-owned systemd units**, rendered by `glidex-ovn` and
installed by `install_ovn` (the same pattern as the source-build OVS
units in [networking.md §6.3](networking.md#63-pinned-source-build-decision-10)):

| Unit | Runs | Notes |
|---|---|---|
| `glidex-ovn-nb.service` | `ovsdb-server` on the NB schema | DB `/var/lib/glidex/ovn/ovnnb_db.db`; socket `/run/glidex/ovn/ovnnb_db.sock` (`root:root 0600`); **no network listener** |
| `glidex-ovn-sb.service` | `ovsdb-server` on the SB schema | DB `/var/lib/glidex/ovn/ovnsb_db.db`; socket `/run/glidex/ovn/ovnsb_db.sock` (`root:root 0600`); TLS listener per §8.3 |
| `glidex-ovn-northd.service` | `ovn-northd` against the two sockets | single instance, always active |
| `glidex-ovn-controller.service` | `ovn-controller` for the **host** chassis (§11) | only when `host_chassis.enabled`; connects to the SB by Unix socket |

Why separate units and directories: a host (or DPU) that already runs the
distro's `ovn-central` keeps working untouched, because glidex's central
uses `/run/glidex/ovn/` and `/var/lib/glidex/ovn/` and never
`/var/run/ovn`. `Restart=on-failure`, `RuntimeDirectoryPreserve=yes`,
`Before=glidex-netd.service`. The units are independent of netd, so a
netd restart does not interrupt OVN, and `glidex-netd.service` gains
`Wants=` on them.

`install_ovn {profile, confirm}` (op, §13):

- Profile `distro`: install `ovn-central` and `ovn-host` (Ubuntu/Debian;
  Fedora family **verify**) with the same non-interactive apt rules as
  `install_ovs`. The packages' postinst starts the distro's `ovn-central`
  and `ovn-controller`; glidex then **stops and disables** those units
  (never uninstalls them) **only if** they have no state of their own, and
  otherwise returns `confirmation_required` naming them. It never starts
  glidex's units against a distro NB DB.
- Idempotent and no downgrades, with the same restart-impact confirmation
  as `install_ovs`.
- Version policy: §8.8.

### 8.3 Listeners and PKI

| Endpoint | Where | Auth |
|---|---|---|
| NB | Unix socket only | filesystem permissions (root) |
| SB, host chassis | Unix socket | filesystem permissions (root) |
| SB, DPU chassis | `pssl:<port>:<addr>` on each address in `sb.listen`, default **only** the host side of each configured DPU management link | mutual TLS (the DPU's chassis certificate) |

**Invariant:** the SB never listens on `0.0.0.0`, on the OVS underlay, or
on any address not named in `ovn.json`; netd refuses a configuration that
asks for a wildcard address (`invalid_argument`) unless
`sb.allow_wildcard=true`, which `probe` reports as a warning. Why: finding 10.

**PKI** (private to glidex, issued in-process by netd; the `glidex-tls`
crate's certificate code is reused where it fits, **verify**):

- CA key and certificate: `/etc/glidex/ovn/pki/` (`root:root 0700`; key
  `0600`). The CA key never leaves the host and is never pushed.
- SB server certificate: SANs are the `sb.listen` addresses.
- Chassis certificates: one per chassis (the host, each DPU). **A chassis
  generates its own key and a CSR; netd signs the CSR and returns the
  certificate** (agent ops `csr` and `install_cert`, §13), so a DPU's
  private key never crosses the link. The DPU stores it root-only and
  wires it into OVS (`ovs-vsctl set-ssl <key> <cert> <ca>`).
- Validity 365 days (`pki.validity_days`); `ovn_status` reports expiry and
  warns 30 days ahead; `ovn_rotate_certs {confirm}` re-issues and
  reloads. Revocation is by removing the chassis from the SB and
  re-issuing the CA only if the CA key was exposed (documented, manual).

### 8.4 NB model

| glidex | OVN NB |
|---|---|
| Network (overlay mode, §12.1) | logical switch `gx-<project>-<network>` |
| NIC of a VM | logical switch port, `type=""`, `addresses`, `port-security` |
| DHCP | OVN native `dhcp_options` on the switch (replaces dnsmasq on overlay networks) |
| `vlan` | `tag_request` on the port when the network sets one |

With DHCP: one `dhcp_options` row per network (`cidr`, router, lease,
DNS) referenced from each port's `dhcpv4_options`. The CIDR is the
network's `subnet` (required for overlay networks with DHCP).
Port-security: MAC always; MAC+IP when the attachment pins an address.

### 8.5 NB operations

Run by netd with the local `ovn-nbctl` against the NB socket (never over
the network): `ls-add --may-exist` on network create; `lsp-add
--may-exist`, `lsp-set-addresses`, `lsp-set-port-security`,
`lsp-set-enabled` on attach; `lsp-del --if-exists` on detach; `ls-del`
when the network goes. After a batch, `ovn-nbctl --wait=sb sync` so the
caller knows northd has processed it. `Program` gains `OvnNbctl`,
`OvnSbctl`, `OvnAppctl`, `OvsdbTool` (schema conversion on upgrade); no
request can name another program.

### 8.6 Chassis

Each chassis registers by setting `external_ids` on its OVS
`Open_vSwitch` record (`ovn-remote`, `ovn-encap-type=geneve`,
`ovn-encap-ip`, `ovn-bridge-mappings`):

- **Host chassis** (`glidex-ovn-controller.service`): `ovn-remote` is the
  SB Unix socket; `ovn-encap-ip` is the host's underlay address
  (`host_chassis.encap_ip`). If the host OVS record already carries a
  foreign `ovn-remote` the result is `conflict` unless `adopt`.
- **DPU chassis**: `ovn-remote` is `ssl:<host-link-address>:<port>`; set
  by `dpu_prepare` (§9.3).
- `dpu_probe` and `ovn_status` report each chassis and whether the SB
  lists it. A DPU that already runs its own central (finding 12) is
  reported as `local_ovn_central: active`; it keeps running unless
  `dpu_prepare` is confirmed with `stop_local_central: true`, which stops
  and disables those units (never uninstalls) and never reads or changes
  their NB. Its OVS `ovn-remote` is repointed to the internal SB only on
  that confirmation, because doing so strands anything the old central
  managed.

### 8.7 State, recovery, availability

- **Source of truth:** netd's state DB. New tables: `overlays`
  (network → subnet, dhcp, mtu, switch name) and `ovn_pki` (CA
  fingerprint; issued certificates with chassis, serial, expiry);
  `vm_ports` records already hold each port's `OvnPortSpec`. `meta`
  gains `ovn_install` `{profile, version, at}`. Two more tables serve §9.4:
  `dpus` (id → pinned rshim identity, last known bundle version, state
  `ready`/`reinitializing`/`unreachable`) and `jobs` (id → op, phase,
  start/end, log tail, result).
- **Invariant: NB is derived.** `ovn_reconcile` (netd start, resync,
  after any unit restart) makes the NB equal to what the records imply:
  missing switches, ports and DHCP rows are created, tagged rows with no
  record are **reported**, never deleted (§7.4, networking.md §7.7 rule
  4). A lost or corrupted NB DB is therefore rebuilt, not restored; the
  SB is runtime state and is never backed up. `install_ovn` copies the NB
  DB aside before a schema conversion (`ovsdb-tool convert`).
- **Startup order:** units active → schemas match the installed version →
  NB reconcile → chassis registrations checked. `glidex-netd` reports
  READY after the reconcile, as today.
- **If netd or the central is down:** each chassis's `ovn-controller`
  keeps its installed flows; traffic already switched continues. No
  attach, detach or network change succeeds (`ovn_unavailable`), the same
  posture as "VMs survive control-plane restarts"
  ([README.md](README.md)).
- **Health** (`ovn_status`): unit states, listener addresses, DB sizes,
  `ovn-northd` role (`ovn-appctl -t ovn-northd status`: active/standby),
  NB↔SB sync (`nb_cfg` against each chassis's `hv_cfg`), chassis list,
  certificate expiry, version.

### 8.8 Versions

- **Rule:** the central's OVN version must be **≥ every chassis's
  `ovn-controller` version** (central is upgraded first). `dpu_probe`
  reports the DPU's `ovn_version`; a DPU newer than the central is
  `unsupported { missing: ["ovn>=<ver>"] }`.
- Distro profile on the test pair: host archive 24.03.8, DPU 24.03.8
  (finding 11); equal, so compatible. If the pair ever diverges, a pinned
  source build (as for OVS, networking.md §6.3) is the escape hatch; its
  pins and checksums would live in `glidex-ovn`. Deferred until needed.
- The DOCA bundle ships the DPU's OVN; glidex never upgrades it.

### 8.9 Trust

Policy enforcement is on the DPU (flows, ACLs, offload); the **policy
source** is the NB on the host. A root user on the host can rewrite the
NB and therefore the policy the DPU enforces, which is a weaker claim
than "policy enforced off-host" ([security.md](security.md)): the DPU
protects the *data path* from a compromised guest or hypervisor
process, not from host root. Mitigations in v1: NB reachable only by
root through a Unix socket, TLS-only SB, a private CA whose key stays on
the host, every NB change logged with peer uid and `on_behalf_of`
(networking.md §7.2). Running the central on the DPU instead, so the
host cannot alter policy, is deferred (O9).

### 8.10 External mode

`ovn.mode: external` keeps the earlier model: glidex runs no OVN
services, `ovn-nbctl` talks to `external.nb_remote`, DPU chassis use
`external.sb_remote`. Remotes must be `ssl:` (with CA, certificate and
key paths) or `unix:`; a `tcp:` remote is refused with `invalid_argument`
unless `external.allow_insecure=true`, which `probe` reports as a warning.
There is no PKI, no `install_ovn`, no host controller unit (the operator
runs a chassis if the host fallback is wanted), and the fallback's
host-chassis step becomes a probe-and-report. Everything in §7, §8.4–8.5
and §11 is otherwise identical.

### 8.11 Deferred

Logical routers, SNAT/DNAT, gateways, floating IPs, ACLs/port groups and
multi-chassis binding (live migration). The NB model leaves room (one
router per project, port groups per security group). The NVIDIA doc says
OVS-DOCA does not support the specialised `ct-ct-nat` mode; whether OVN
NAT offloads on this build is unverified (O5).

## 9. Configuring the DPU

### 9.1 What glidex will not do

Firmware (`mlxconfig`) settings and the cold reset are a maintenance
event: the host must be rebooted first, then the BlueField. `dpu_probe`
reports each missing setting with the exact command, never runs them.
Required for the v1 design (values from the service guide; counts are
defaults, set by the operator):

| Parameter | Needed |
|---|---|
| `VIRTIO_NET_EMULATION_ENABLE` | `1` |
| `VIRTIO_NET_EMULATION_NUM_PF` | ≥ 1 (static) |
| `VIRTIO_NET_EMULATION_NUM_VF` | VFs per PF (guide: up to 126) |
| `VIRTIO_NET_EMULATION_NUM_MSIX` / `_NUM_VF_MSIX` | ≥ 4 each |
| `PER_PF_NUM_SF`, `PF_TOTAL_SF`, `PF_SF_BAR_SIZE` | `PF_TOTAL_SF` ≥ 1 + PFs + VFs |
| `SRIOV_EN` | `1` |

Also on the host: kernel parameter `pci=realloc` (guide, for SR-IOV with
virtio-net devices; **proven necessary**, finding 31) and, where `virtio_pci`
is built in, `initcall_blacklist=virtio_pci_driver_init` so the host does not
probe the DPU's PF before the controller serves it. On the DPU the image's
stock SF 0 must be gone (finding 29, §9.3 step 2b). **Verify** O3: the exact set needed with no hotplug
PFs; the first hardware run (D4) trims this table to what is proven.

### 9.2 `DpuCapabilities` (`dpu_probe`)

| Field | Source on the DPU |
|---|---|
| `ovs_version`, `doca_initialized`, `doca_version` | `ovs-vsctl get Open_vSwitch . …` |
| `iface_types`, `datapath_types` | same; must contain `doca` |
| `hugepages` (per size) and `dpdk_hugepage_dir` | `/sys/kernel/mm/hugepages`, `other_config` |
| `pmd_cpu_mask`, `pmd_threads` | `other_config`, `ovs-appctl dpif-netdev/pmd-stats-show` |
| `offloaded_flows` | `ovs-appctl dpif/show` |
| `eswitch_mode` | `devlink dev eswitch show` (must be `switchdev`) |
| `virtio_controller` (`active`/`inactive`/`missing`), `virtio_pfs`, `virtio_vfs_total`, `virtio_vfs_free` | `systemctl`, `virtnet list` |
| `firmware` (the §9.1 parameters, current and next-boot) | `mlxconfig q` (read-only) |
| `ovn_version`, `local_ovn_central` | `ovn-controller --version`, `systemctl is-active` |
| `chassis` (`registered`, `name`, `remote`) | `Open_vSwitch` `external_ids`; registration is read from the internal SB on the host |
| `ovn_cert` (present, expiry) | the DPU's certificate files |
| `bridges` | `ovs-vsctl list Bridge` (datapath type, owned) |
| `combinations` | which of §5.2's modes are possible now, and what's missing |

Host side (`probe` extended): virtio PF present (by PCI id/driver),
`sriov_totalvfs`, `vfio_pci`/`virtio_vfio_pci` modules, IOMMU groups
(reuses `HostCapabilities.iommu`), `pci=realloc` on the command line, and
the OVN fields: `ovn_central_installed`, `ovn_version`, `ovn_mode`,
`ovn_units` (the four of §8.2), and a foreign central (`ovn-central`
active under the distro's paths).

### 9.3 `dpu_prepare` (confirm required)

Applies only runtime and OVSDB state; reports `changed: false` when
nothing differs. Order, with the interruption each step causes:

1. 2 MB hugepage pool (`nr_hugepages` under `hugepages-2048kB`), a
   `hugetlbfs` mount, and tmpfiles/fstab entries so both survive reboot.
   Default 1024 pages (2 GB), overridable. No interruption.
2. `other_config`: `doca-init=true`, `hw-offload=true`, `dpdk-hugepage-dir`,
   `pmd-cpu-mask` (default: two top cores, leaving the rest for OVN
   and the OS). Needs an `ovs-vswitchd` restart → **underlay interruption**.
2b. **virtio-net prerequisites** (only when the firmware values are active
   and virtio-net is wanted; finding 29): remove the image's stock SF 0
   (`ovs-vsctl del-port`, `mlnx-sf --action delete --sfindex pci/<pf>/<idx>`
   with `mlxdevm` on `PATH`), comment out its line in
   `/etc/mellanox/mlnx-sf.conf` (backup kept), and restart
   `virtio-net-controller` if `virtnet list` does not answer. The DPU loses
   the SF as a bridge port; its address stays on the bridge's internal
   port. Verified by `virtnet list` showing a static PF. It runs before the
   bridge step so the SF is never converted to `doca` first.
3. Bridges with `datapath_type=doca`, uplink and representors `type=doca`
   with no options (finding 2).
4. **Chassis identity:** `csr` on the DPU → netd signs → `install_cert`
   (§8.3); then the OVN `external_ids` (§8.6), `ovn-remote` pointing at the
   internal SB over the management link. With `stop_local_central: true`
   a DPU-local central is stopped first (§8.6).
5. Re-apply the underlay IP and routes that the restart dropped (finding
   6), from `dpu.json`'s `underlay` block. Persistence of these across a
   DPU reboot is the operator's (netplan on the DPU); glidex reports
   when they are runtime-only, as for host NICs
   ([networking.md §8.5](networking.md#85-ip-migration-decision-2)).

`ConfirmationRequired { impact }` lists the restart, the bridges, the
chassis repoint and the tenants affected. Pins and tuning values live in
`glidex-dpu` as constants, like the source-build pins in `install.rs`.

### 9.4 Reinitializing the DPU (`dpu_reinit`)

A fresh DPU, a wedged one, or a DOCA bundle upgrade all come down to the
same step: write a BFB to the card through rshim and rebuild glidex's
state on top. This is **destructive**: the DPU's root filesystem is
replaced, so the agent, its certificate and key, the chassis identity,
hugepage and OVS settings, bridges, and any foreign OVN central on the DPU
are all gone afterwards (finding 15).

**What it does not do:** change `mlxconfig` values (they live in the
card's firmware configuration, not on the DPU's disk; **verify** O13),
update NIC firmware (the BFB's `WITH_NIC_FW_UPDATE` is forced off unless
`reinit.allow_fw_update` is set, and a firmware update still needs the
cold reset §9.1 leaves to the operator), power-cycle the card, or reboot
the host.

#### 9.4.1 Inputs

`dpu.json` (§10.1) gains, per DPU:

```json
"rshim": "rshim0",
"reinit": { "bfb": "/var/lib/glidex/bfb/bf-bundle-<ver>.bfb", "sha256": "<hex>",
            "target_device": "/dev/nvme0n1", "allow_fw_update": false,
            "update_atf_uefi": false, "timeout_secs": 3600 }
```

- `rshim` names the device under `/dev/`. netd **checks it before writing
  anything**: the node exists, `misc` reports `BF_MODE` `DPU mode`,
  `DEV_INFO` starts with `BlueField-3`, and `DEV_NAME` matches the PCI
  function recorded for this DPU id at first probe (pinned in the
  `dpus` state table). A mismatch is `invalid_argument`. Why: two DPUs
  on one host would otherwise be one typo apart.
- The BFB path must be under `/var/lib/glidex/bfb/` (`root:root 0750`,
  files `0640`); netd re-hashes the file and compares it with `sha256`
  immediately before use (a mismatch aborts, nothing is written). The
  operator puts the file there; glidex never fetches one.
- **First-boot configuration file** (`bf.cfg`), generated per run by netd
  (`root:root 0600`, in a `0700` temp directory), passed with
  `bfb-install -r <rshim> -c <bf.cfg> -b <bfb>`, and deleted afterwards.
  The mechanism is NVIDIA's documented one
  ([Customizing BlueField software deployment using bf.cfg][bf-cfg]):
  variables the installer reads, and three shell hooks it calls from
  `install.sh`: `bfb_pre_install` (before partitioning), **`bfb_modify_os`**
  (after the root filesystem is extracted, before reboot, with the target
  mounted at `/mnt/`), and `bfb_post_install` (partitions unmounted).
  glidex uses variables and `bfb_modify_os` only:
  - **Variables:** `WITH_NIC_FW_UPDATE=no` (the documented default is
    **yes**, so it is always written; `yes` only with
    `reinit.allow_fw_update`), `UPDATE_ATF_UEFI` from
    `reinit.update_atf_uefi` (default `no`; the documented default is
    yes), `UPDATE_DPU_OS=yes`, and `device=<reinit.target_device>`
    (default `/dev/nvme0n1`, pinned: O16). No `ubuntu_PASSWORD`, and none
    of `UEFI_PASSWORD`, `BMC_PASSWORD`, `NEW_*`: the page says those are
    stored in clear text, and no credential is ever written to a
    configuration file or to the job log.
  - **`bfb_modify_os`**, writing under `/mnt/`. It leaves the image's
    `ubuntu` account and the shipped `user-data` exactly as they are and
    never creates anything under a home directory:
    - **glidex's own account**, as a first-boot **script** delivered as a
      second cloud-init seed, `/var/lib/cloud/seed/nocloud-net/vendor-data`
      (a `#!/bin/bash` file: cloud-init runs it once per instance, as root,
      through `scripts_vendor`). It creates a system account (default name
      `glidex`, uid below 1000, home `/var/lib/glidex`, bash), runs
      `passwd -l` so the account **has no usable password**, installs the
      bootstrap public key in `~/.ssh/authorized_keys` (0600, owned by the
      account) and a `sudoers.d` entry (0440, checked with `visudo -c` and
      removed if rejected). It never sets a password, is idempotent, and
      logs to `/var/log/glidex-bootstrap.log`. A `users:` entry in
      vendor-data cannot be used (findings 25-26). An existing `vendor-data`
      is never overwritten;
    - the **same key** at `/etc/ssh/authorized_keys.d/<account>` (root-owned,
      **0644**: a root-owned 0600 file is unreadable by the account, the
      cause of runs 1-3) plus an sshd drop-in adding
      `/etc/ssh/authorized_keys.d/%u` to `AuthorizedKeysFile`, as a fallback
      that does not depend on how or when the account is created;
    - sshd: password and keyboard-interactive authentication off in that
      same drop-in, unless the operator asks to keep the image's login for
      investigation (`--keep-password-login`). The shipped `sshd_config`
      includes `sshd_config.d/*.conf` first and the installer appends
      `PasswordAuthentication yes` at its end, so the policy must live in a
      drop-in;
    - the OVN CA **certificate** (public) and the DPU's intended chassis
      name; the `glidex-dpu-agent` unit and binary from
      `reinit.agent_package`, enabled by creating the unit's
      `multi-user.target.wants` link (no `systemctl` in the chroot is
      assumed).
  - Not used: `bfb_pre_install`/`bfb_post_install`, any BMC or UEFI
    variable, and firmware parameters. The page does not cover running
    `mlxconfig` from `bf.cfg`, so glidex does not rely on it (§9.1).
  The first login to the **image's own** `ubuntu` account is gated
  ("logging in will not be possible until all services are up (`DPU is
  ready`)" and its default password expires at first login,
  `chpasswd: expire: True`). glidex does not use that account, so neither
  gate applies to its key-only session; whether the account that the first-boot
  script creates is usable as soon as sshd answers is O17.

#### 9.4.2 Safety rules

- **Admin-only and never implicit.** `dpu_reinit` needs the admin policy
  group, is absent from the default `glidex` policy (§13), and nothing
  (not `dpu_prepare`, not a failed probe, not the controller) calls it.
- **No running tenants.** It refuses with `conflict` if any VM with a
  port on this DPU is running or desired running, listing them. It does
  not stop VMs for the caller. Stopped VMs keep their NIC records; their
  DPU ports are marked `lost` and re-created by reconcile at next launch
  (§7.4).
- **Confirmation** (`ConfirmationRequired { impact }`): names the DPU, the
  BFB file and checksum, the OS and chassis state that will be erased,
  that the DPU is unreachable for the duration, that any local OVN
  central on the DPU is destroyed, and the typical duration.
- **One at a time.** A second `dpu_reinit` or `dpu_prepare` for the same
  DPU while one is running returns `conflict`.
- **Interruption.** `bfb-install` can be killed with its child; netd's
  restart does not stop the install (it runs under its own transient
  systemd unit, `glidex-dpu-reinit@<id>.service`), and on start netd
  reattaches to a running job from the state DB rather than starting a
  second one.

#### 9.4.3 Phases

`dpu_reinit` returns a **job id** at once; progress is `dpu_job` (§13),
one phase at a time, each recorded in the `jobs` table so a netd restart
resumes.

| # | Phase | What happens | On failure |
|---|---|---|---|
| 1 | `preflight` | the §9.4.1 device checks; BFB checksum; no running tenants; `rshim` service active; disk space for the log; record the DPU's current facts (`bfver`, `mlxconfig` values, `dpu_probe`) into the job as a before-picture | job fails, DPU untouched |
| 2 | `quiesce` | detach every port on this DPU in netd (hypervisors are already stopped); remove the DPU's chassis from the internal SB (`chassis-del`, tagged by its recorded name); mark the DPU `reinitializing` so §6's health check says `dpu_unavailable` and `Auto` networks fall back (§11) or refuse | roll the markers back; job fails |
| 3 | `install` | `bfb-install -r <dev> -c <bf.cfg> -b <path>`; typically **about 10 minutes** (O15); progress streamed from `/dev/rshim0/misc` (`DISPLAY_LEVEL` 2) and the `console` device into the job log (tail kept, 64 KiB); a hard timeout of `timeout_secs` | on timeout or a nonzero exit: job `failed`, DPU left as is, **no automatic retry**; the console tail is in the job |
| 4 | `wait` | the `console` device shows the boot sequence; wait for the `DPU is ready` message, the management link (`tmfifo_net0`) and the DPU's SSH port; budget 15 min after `bfb-install` returns | `failed`, DPU in an unknown state; `dpu_probe` reports it `unreachable` |
| 5 | `bootstrap` | connect **once** over SSH with netd's bootstrap key (the only SSH use in this design; decision 2 otherwise applies) and run the agent's one-time `enroll`: the agent generates its server key and a CSR, netd signs it, and the agent starts listening with mutual TLS (§10.2); then remove the bootstrap key from the DPU and verify the agent answers `hello` over TLS | the bootstrap key is removed even on failure; the job fails with the step that broke |
| 6 | `prepare` | run `dpu_prepare` (§9.3) with `confirm` carried from the job: hugepages, `doca-init`, `doca` bridges and ports, chassis certificate and `ovn-remote`, underlay address | `failed`; `dpu_prepare` is idempotent and can be re-run on its own |
| 7 | `verify` | `dpu_probe` must report no missing item that the pre-install picture did not already list; the chassis registers in the internal SB; the agent reports the same bundle version as the BFB's | `failed` with the missing items |
| 8 | `release` | clear `reinitializing`; `ovn_reconcile` and the §7.4 port reconcile run; event `DpuReinitialized` | – |

A **completed** job leaves the DPU as `dpu_prepare` would, with a fresh
chassis identity and certificate. A **failed** job never retries by itself,
leaves the DPU marked `reinitializing` (so nothing is placed on it) until
an admin runs `dpu_reinit` again or clears the mark
(`dpu_job` `{abandon: true}`), and keeps the console tail for diagnosis.

#### 9.4.4 What survives, and what it costs

| Survives | Lost |
|---|---|
| `mlxconfig` (firmware) settings, to be verified (O13) | all DPU OS state, including the agent's keys and a DPU-local OVN central's data |
| host-side state: netd DB, internal NB/SB, the CA and its key, `dpu.json`, VF configuration on the host | the DPU chassis identity (a new certificate is issued; the old chassis row is deleted in `quiesce`) |
| the host's VMs (stopped ones keep their records) | DOCA/OVS settings applied by `dpu_prepare` until phase 6 re-applies them |

Cost: about **10 minutes** for the install on this card (operator
measurement; the rshim `console` shows the boot sequence throughout), and
a few more for bootstrap and `dpu_prepare`, during which the DPU's
networking is down. The BFB may also rewrite the eMMC boot area, which is
how the card then boots the OS from the NVMe (O16). The host's own networking through the DPU's
physical port is unaffected only on hosts where the DPU is not the host's
data path, which is true for this design (VMs use virtio VFs, not the
host PF) but not for a host that uses the DPU's uplink for its own
traffic; `dpu_reinit` refuses with `host_interface_in_use` (the NIC
classification of [networking.md §8.4](networking.md#84-nic-classification))
if the host's default route goes through a function of this DPU.

## 10. Agent, credentials and configuration

### 10.1 Host configuration

`/etc/glidex/ovn.json` (`root:root 0644`; no secrets, key material is
under `pki.dir`). Missing file = defaults; an unreadable or unparseable
file stops netd at startup, as with `netd.json`.

```json
{ "mode": "internal",
  "profile": "distro",
  "sb":  { "listen": ["<host-side-dpu-link-address>:6642"], "allow_wildcard": false },
  "pki": { "dir": "/etc/glidex/ovn/pki", "validity_days": 365 },
  "host_chassis": { "enabled": true, "encap_ip": "<host-underlay-address>", "bridge": "br-int" },
  "external": { "nb_remote": null, "sb_remote": null, "ca": null, "cert": null, "key": null, "allow_insecure": false } }
```

`/etc/glidex/dpu.json` (`root:root 0600`):

```json
{ "dpus": [ {
    "id": "dpu0",
    "agent": "tcp://<dpu-mgmt-address>:<port>",
    "tls": { "ca": "/etc/glidex/dpu/ca.pem", "cert": "/etc/glidex/dpu/client.pem", "key": "/etc/glidex/dpu/client.key" },
    "mode": "vfio",
    "bridges": { "phy": "ovsbr-phy", "int": "br-int", "representor_bridge": "br-int" },
    "ovn": { "encap_ip": "<dpu-underlay-address>" },
    "underlay": { "bridge": "ovsbr-phy", "cidr": "<dpu-underlay-address>/<len>", "gateway": "<gateway>" },
    "hugepages_2m": 1024, "pmd_cpu_mask": "0xc0", "port_timeout_secs": 30,
    "rshim": "rshim0",
    "reinit": { "bfb": "/var/lib/glidex/bfb/bf-bundle-<ver>.bfb", "sha256": "<hex>",
                "agent_package": "/var/lib/glidex/bfb/glidex-dpu-agent-<ver>.tar.gz",
                "allow_fw_update": false, "timeout_secs": 3600 }
} ] }
```

`rshim` and `reinit` are needed only for §9.4; without them `dpu_reinit`
returns `unsupported { missing: ["dpu.reinit"] }`.

A missing `dpu.json` means "no DPU" (backend `Dpu` is unavailable, `Auto`
falls back). v1 supports exactly one DPU entry per host; the list shape
is for later. The agent channel's own certificates (`tls`) are separate
from the OVN PKI: they authenticate netd to the agent, and are created
by the operator at first setup (or by `gxctl dpu enroll`, **verify**
O10).

### 10.2 The agent

- Transport: the netd NDJSON protocol ([networking.md §7.3](networking.md#73-protocol))
  over TLS with **mutual** authentication on the DPU's point-to-point
  management link. It listens only on that link's address, never on the
  out-of-band or underlay interfaces.
- Authorization: the client certificate's identity must match the host's
  netd identity; nothing else is accepted. No per-user rights on the DPU:
  user-level authorization stays in netd and the control plane
  ([security.md](security.md)).
- Programs: an allow-list enum like `Program` (`OvsVsctl`, `OvsAppctl`,
  `Virtnet`, `Ip`, `Devlink`, `Mlxconfig` (query only), `Systemctl`, and
  sysfs writes for hugepages), vector arguments, no shell. It has **no**
  OVN central programs: all NB/SB work happens on the host. Certificate
  signing requests are generated in-process (no `openssl`).
- First start after a reinstall: the agent has no certificate and
  listens for nothing until `enroll` (§9.4.3 phase 5) completes. `enroll`
  is a local command, run once by netd over its bootstrap SSH session, and
  it refuses to run if the agent already has an identity.
- State: none of its own. Tagged OVSDB rows are the record; the agent
  restarts freely.
- Version handshake like `hello`; a protocol or agent version older than
  netd's is `unsupported`.

## 11. Host fallback (OVS-DPDK)

Applies to **overlay** networks with `backend: auto` and `fallback: host`
(§12.1), and to overlay networks with `backend: host` on hosts with no
DPU. The VM gets today's vhost-user port on a host `netdev` bridge
(networking.md §9, combination B), and the host chassis (§8.6) puts that
port on the same logical switch as DPU-backed NICs: the glidex-owned
`ovn-controller` and the internal central are what make this work, and no
external service is required.

Requirements, probed and reported when missing:

- Host OVS with DPDK and `dpdkvhostuserclient` (existing `probe`).
- The internal OVN (§8.2): `ovn-host` installed through `install_ovn`;
  `host_chassis.enabled`.
- A host `br-int` with `datapath_type=netdev` (created by netd, tagged),
  chassis `external_ids` (§8.6). **Verify** O6: the host and DPU underlays
  must reach each other, and with a bridged host NIC the underlay address
  is the bridge's.
- MTU: the host path must carry the same Geneve overhead as the DPU path.

Behaviour differences the control plane surfaces (`status.nics[].backend`):

| | DPU | Host fallback |
|---|---|---|
| Switching CPU | DPU eswitch + Arm | host PMD cores |
| Hugepages | on the DPU | host (networking.md §0 finding 9) |
| Offload | hardware | none |
| Data path under a compromised host | enforced off-host | enforced on the host |

(The policy source is the host in both cases, §8.9.) The last row is why
fallback is **opt-in per network** (`fallback: host`, default `none`): it
silently weakens isolation. A VM launched on fallback gets event
`BackendFallback` and condition `NetworkDegraded=True/HostBackend`.

Mixed VMs (one NIC on each backend) are allowed. A NIC's backend does
not change at runtime (decision 8); after the DPU recovers, a restart
moves the VM's NICs back. `GET /networks/{name}` lists which NICs are on
the host backend so an operator can find them.

## 12. Control plane

### 12.1 Models

```rust
pub enum NetworkMode { Nat, Bridged, Isolated, Overlay }   // + Overlay

pub struct Network {
    /* existing */
    pub backend: BackendPolicy,            // default Auto; only meaningful for Overlay
    pub fallback: FallbackPolicy,          // None (default) | Host
    pub overlay: Option<OverlayConfig>,    // Overlay mode only
}
pub struct OverlayConfig { pub subnet: Ipv4Net, pub dhcp: bool, pub mtu: Option<u16> }
```

`port_type` is derived: DPU backend → `DpuVirtio`; host → `VhostUser`.
`NicBinding.binding` gains `Vfio { bdf }`
(`crates/glidex-control-plane/src/models.rs:186`).

Validation at `create_vm`: an `Overlay` network needs the internal OVN
(or an external one) to be *possible*, and the matching backend to be
possible (DPU configured, or host DPDK present when `backend` is `Host`
or `fallback: host`); otherwise `400 invalid_network` with
`details.missing`. Memory: a `Vfio` NIC does **not** force shared guest
memory; a host vhost-user NIC does (unchanged).

### 12.2 Hypervisors

- `Vfio { bdf }` uses the existing VFIO device launch path in both
  Cloud Hypervisor and QEMU (hypervisors.md). The guest's MAC is the
  one the DPU programmed, so `network-config` matching by MAC
  ([networking.md §11.5](networking.md#115-guest-network-config)) is
  unchanged.
- The control plane's existing "VFIO conflict" list gains the VF's `bdf`
  (`GET /pci-devices` shows `in_use: "dpu port <vm>/<nic>"`), so a user
  cannot also hand it to a VM with `attach_device`.

### 12.3 Network controller

`controller/network.rs` ([reconciliation.md §10.3](reconciliation.md#103-network))
gains, for overlay networks: `ensure_overlay` (switch and DHCP options on
the internal NB), conditions `OvnReady`, `DpuReady`, `ChassisRegistered`,
and phase `degraded` with the missing piece named (`netd_unavailable`
when netd is away, as today). A deleted network removes its tagged switch
**only when no VM uses it**, as for NAT.

### 12.4 REST and CLI

| Method + path | netd op |
|---|---|
| `GET /ovn` | `ovn_status` |
| `POST /ovn/install {profile, confirm?}` | `install_ovn` |
| `POST /ovn/rotate-certs {confirm?}` | `ovn_rotate_certs` |
| `GET /dpus` | `dpu_probe` |
| `POST /dpus/{id}/prepare {confirm?, stop_local_central?}` | `dpu_prepare` |
| `POST /dpus/{id}/reinit {confirm?}` | `dpu_reinit` (202 with a job id; 409 `confirmation_required` first) |
| `GET /dpus/{id}/jobs/{job}` · `DELETE …/{job}` | `dpu_job` (status; `abandon`) |
| `GET /dpus/{id}/ports` | `dpu_list_ports` |
| `POST /networks {mode:"overlay", subnet, dhcp, backend, fallback}` | `ensure_overlay` + store |
| `POST /vms` with `networks` | validation only |

`gxctl`: `ovn status`, `ovn install [--force]`, `dpu status`,
`dpu prepare [--stop-local-central] [--force]`,
`dpu reinit <id> [--force]` (prints the job, then follows it until a phase
fails or `release` completes; Ctrl-C detaches and leaves the job running),
`dpu job <id> <job>`,
`network-add <name> --overlay --subnet s [--dhcp] [--backend auto|dpu|host] [--fallback host]`.
UI: an **OVN** and a **DPU** panel on the Networking page (probe,
missing items, install/prepare with the confirmation dialog), a backend
column on NICs.

Error mapping adds: `dpu_unavailable` → 503 `dpu_unavailable`;
`dpu_capacity` → 409 `dpu_capacity`; `ovn_unavailable` → 503
`ovn_unavailable`; `dpu_port_timeout` → 502 `netd_error`.

## 13. Protocol additions

New ops on the **full** sockets only (never the status socket); `dpu_probe`
and `ovn_status` are read-only but return tenant-adjacent state:

| Op | Args → result |
|---|---|
| `install_ovn` | `{profile, confirm}` → `InstallReport` (`confirmation_required` first when units or state exist) |
| `ovn_status` | – → `OvnStatus` |
| `ovn_reconcile` | – → `ReconcileReport` |
| `ovn_rotate_certs` | `{confirm}` → `RotateReport` |
| `dpu_probe` | `{id?}` → `DpuCapabilities` |
| `dpu_prepare` | `{id?, confirm, stop_local_central?}` → `PrepareReport` (`ConfirmationRequired` first) |
| `dpu_list_ports` | – → records with live DPU + NB state |
| `dpu_reinit` | `{id?, confirm}` → `{job}` (`ConfirmationRequired` first; §9.4) |
| `dpu_job` | `{job, abandon?}` → `JobStatus {phase, state, log_tail, result?}` |
| `ensure_overlay` | `{network, subnet, dhcp, mtu?}` → `OverlayState` |
| `delete_overlay` | `{network}` → – |
| `attach_vm_port` / `detach_vm_port` / `release_vm` | extended as in §5, no new op names |

Agent-side ops (netd → agent, not on netd's sockets): `hello`, `probe`,
`list_ports`, `ensure_vf`, `attach_rep`, `detach_rep`, `prepare`, `csr`,
`install_cert`.

New error codes: `dpu_unavailable`, `dpu_capacity`, `ovn_unavailable`,
`dpu_port_timeout`. `PROTOCOL_VERSION` stays 1: all additions are optional
fields or new ops, and an older netd answers an unknown op with
`protocol_error`. Per-op policy ([networking.md §7.2](networking.md#72-authorization))
applies; the default `glidex-admin: ["*"]` covers them, and the default
`glidex` policy gets `ovn_status`, `dpu_probe`, `dpu_list_ports`,
`attach_vm_port`, `detach_vm_port`, `release_vm`, `ensure_overlay`,
`delete_overlay`, `ovn_reconcile`, `dpu_job` (status only) but **not**
`install_ovn`, `ovn_rotate_certs`, `dpu_prepare` or **`dpu_reinit`** (they
install packages, re-issue certificates, restart OVS, or erase the DPU).
New error codes also include `dpu_job_failed` (a job's terminal state
when it is polled as an error) and `host_interface_in_use` is reused by
§9.4.4.

## 14. Milestones

Each is one PR or a short series. Ignored e2e tests need
`GLIDEX_TEST_DPU=1` (hardware) or `GLIDEX_TEST_OVN=1` (root host with OVN
packages); CI runs unit and fake-agent tests.

**D0 comes first and gates every hardware milestone.** Everything in §9
was worked out by hand, in sequence, on DPUs that already carried other
state (a wrong hugepage size was fixed after the fact, a restart dropped
the underlay address twice, a stray `br-int` was recreated by
`ovn-controller`). None of it has been run from a clean image. D0 proves
the preparation sequence and the reinstall from scratch **before** any
Rust encodes them. D1 and D2 are software-only and may proceed in
parallel; D3 may start against fakes; **D4, D5 and D5b do not start until
D0's findings are in §0.**

| # | Scope | Acceptance |
|---|---|---|
| **D0** hardware bring-up (no glidex code; **in progress**: acceptance (1) met (findings 16–18); (3) **met** in run 5 (findings 19-28); (2) **half met**: firmware activated, the virtio-net PF is served (findings 29-31), still to record `mlxconfig` after a reinstall; (4) pending: VF enablement and the representor bridge) | Operator-run scripts and a runbook under `scripts/dpu/` (next to `scripts/dev/`): `dpu-probe.sh` collects every §9.2 field as JSON; `dpu-prepare.sh` is the §9.3 sequence as idempotent shell with `--dry-run`, per-step timing, and the restart/underlay handling; `dpu-reinit.sh` generates the §9.4.1 `bf.cfg` and drives `bfb-install -r … -c … -b …`, capturing the rshim `console`. Run on a DPU that may be wiped, from a stock BFB. Scripts hold no secrets and take paths and keys as arguments. | Findings recorded in §0 (and the open questions closed): (1) from a freshly reinstalled DPU, `dpu-prepare.sh` alone reaches a probe with no gaps and a `doca` bridge carrying traffic, and a second run changes nothing; (2) **O13**: stage the §9.1 firmware values (operator decision), follow the cold-reset procedure, record `mlxconfig q`, reinstall with `WITH_NIC_FW_UPDATE=no`, record it again; (3) the `bf.cfg` path: where `authorized_keys` and the agent unit must go under `/mnt/`, unit enablement by symlink, key-only first login and the `DPU is ready` gate (**O17**), the console markers a job can key on, the measured duration (**O15**), and which devices the BFB writes (**O16**); (4) with firmware staged, **O1–O3** (representor bridge, `virtnet` flags, minimal parameter set); (5) every command's raw output saved as fixtures under `crates/glidex-dpu/tests/fixtures/`, scrubbed of addresses, MACs and identifiers, as the canned inputs for D1's probe-parsing tests. |
| **D1** `glidex-dpu` core | types, `Exec`/`RecordingExec`, command builders (`ovs-vsctl`, `virtnet`), probe parsing, VF allocator | Unit tests: exact commands per port; idempotency flags; `type=doca` with no options; refusal of `dpdk`/`netdev` on a DOCA DPU; probe parsed from canned output of the test DPU; allocator reuse after free. |
| **D2** `glidex-ovn`: the internal instance | `install_ovn` (distro profile), four unit files, listeners, private CA and chassis-CSR signing, NB model and reconciler, `ovn_status`, host chassis | Unit tests: rendered units and `ovsdb-server` arguments; wildcard SB listener refused; CA/cert issuing and SAN contents; reconciler plan from records (create, report-untagged, rebuild-from-empty); version rule (§8.8). Ignored e2e (`GLIDEX_TEST_OVN`, host OVS-DPDK, **no DPU**): an overlay network with two VMs on host vhost-user ports ping each other through the internal OVN; kill and recreate the NB DB and the same ping works again after `ovn_reconcile`. |
| **D3** agent + netd backend | agent binary, mTLS, allow-list, `Backend` trait, `HostOvsBackend` adapter, backend selection and stickiness, `dpu.json` | Fake-agent tests: every §6 table row; recorded backend wins on re-attach; agent down → `dpu_unavailable`/fallback; unparseable `dpu.json` or `ovn.json` stops netd. |
| **D4** DPU attach (hardware; needs D0) | §7.1 end to end on one DPU, `dpu_probe`, DPU chassis cert enrolment | Ignored e2e (`GLIDEX_TEST_DPU`): a VM gets a virtio NIC, a DHCP address from OVN, pings a host-backed VM on the same logical switch; detach leaves no tagged rows; the DPU's private key never appears on the host. **Also settles O1–O3** and trims §9.1. |
| **D5** `dpu_prepare` (needs D0) | §9.3 incl. hugepage persistence, `stop_local_central` | Unit tests per step; e2e: from a stock DPU (firmware already set, local central running) to a probe with no gaps; restart interruption reported; underlay re-applied; DPU-local central stopped only with the flag. |
| **D5b** `dpu_reinit` (needs D0) | §9.4: device identity checks, BFB checksum, `bf.cfg` generation, `bfb-install` under a transient unit, the eight phases, jobs table, admin-only policy, `gxctl dpu reinit` | Unit tests with `RecordingExec`: exact `bfb-install` arguments; refusal on rshim mismatch, bad checksum, running tenant, concurrent job, default route through the DPU; `bf.cfg` contains a public key and the CA certificate and **no** password or private key; each phase's failure rule; a netd restart mid-install reattaches instead of starting a second install. Ignored e2e (`GLIDEX_TEST_DPU_REINIT=1` **and** an explicit DPU id; never in CI): from a running DPU to a verified `release`, with the timing recorded for O15. |
| **D6** control plane + UI | overlay networks, `Vfio` bindings in both hypervisors, controller conditions, REST/CLI/UI | API tests against a fake netd+agent; hypervisor argument tests (VFIO NIC) for CH and QEMU. |
| **D7** fallback integration | `BackendFallback`, `NetworkDegraded`, mixed VMs, rollback on recovery | e2e: stop the agent, launch a VM on a `fallback: host` network, ping a DPU-backed VM on the same switch; a `fallback: none` network refuses. |
| **D8** (later) vhost-user via `vfe` | §5.2 | after O4 is answered. |

## 15. Open questions (verify on hardware)

| # | Question |
|---|---|
| O1 | Bridge layout for virtio representors: `br-int` (OVN) vs the uplink's bridge (service guide). |
| O2 | Exact `virtnet` flags to set a VF's MAC/MTU/queues, and the representor naming and timing (`sf_num` ranges 1000/2000/3000+ in the guide). |
| O3 | **Partly answered (findings 29-31):** `pci=realloc`, the `virtio_pci` initcall blacklist and no stock SF are needed; VF enablement is still open. The minimal firmware and host-kernel set for a static PF with VFs and no hotplug (`pci=realloc`, MSI-X counts); how many VFs the B3140L allows (`PF_TOTAL_SF` ceiling; the test card reported 32 external SFs at default). |
| O4 | Where `vfe-vhostd`/`vfe-vhost-cli` come from for this host, and which versions. |
| O5 | Whether OVN SNAT/DNAT and conntrack ACLs offload on OVS-DOCA 3.5 (the doc excludes `ct-ct-nat`). |
| O6 | Underlay reachability and MTU between the host chassis and a DPU chassis on the same host (§11). |
| O7 | Whether VF state survives a controller restart (`recovery=1` default) and how netd should rediscover VFs after one. |
| O8 | Migration: the guide documents VFIO migration (kernel 6.13+, QEMU 9.1+, `virtio_vfio_pci`) and a vhost-user path; the test host has kernel 6.8 and QEMU 8.2.2. Revisit if glidex ever gets multi-host. |
| O9 | Running the central on the DPU instead of the host (§8.9): memory and CPU cost on the DPU's 8 Arm cores and 16 GB, and who restarts it. |
| O10 | Enrolling the agent channel (`dpu.json` `tls`): operator-issued certificates at first setup vs a `gxctl dpu enroll` flow over the rshim/management link. |
| O11 | Whether `glidex-tls`'s certificate code can issue CSR-signed client certificates with the SANs and key usages OVN requires, or `glidex-ovn` needs its own issuer. |
| O12 | How the host's OVS (which reports `3.5.0052`, a DOCA-host build) behaves as an OVS-DPDK vhost-user host for §11: whether it has `dpdkvhostuserclient` and DPDK, and how it coexists with `glidex-ovn-controller`. |
| O13 | **Half answered (findings 28, 31).** Staged (`Next Boot`) `mlxconfig` values **survive** a BFB reinstall with `WITH_NIC_FW_UPDATE=no`. Still open: whether they survive **after** they have been activated by a cold reset, and whether any bundle step resets active values. Test after the operator's cold reset (host first, then the BlueField). |
| O14 | **Resolved (findings 26-28).** `bf.cfg` through `bfb-install -c`; the `bfb_modify_os` hook writes a first-boot script as vendor-data; cloud-init's `scripts_vendor` runs it once per instance; the account it creates authenticates by key. |
| O15 | **Answered**: about **5 minutes** for the install (310 s and 304 s), plus the DPU's first boot; the earlier ~10 minute estimate was for the whole cycle. Findings 19. |
| O16 | **Answered (findings 19, 28)**: the installer erases both `/dev/mmcblk0` and `/dev/nvme0n1`; the OS lands on `nvme0n1` (`p1` EFI, `p2` root) as `device=` says; `BOOT_MODE` reads `1 (emmc)` before and after. |
| O17 | **Answered (finding 28)**: the `glidex` account (locked password, key only) accepts a non-interactive SSH session as soon as the first-boot script has run, 115 s after `bfb-install` returned; no expiry or `DPU is ready` interference. |

## Appendix A: DPU commands verified on the test DPU

```
# 2 MB pool + mount (not vm.nr_hugepages: 512 MB default page size)
echo 1024 > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
mount -t hugetlbfs -o pagesize=2M none /hugepages-2M

ovs-vsctl set Open_vSwitch . other_config:doca-init=true other_config:hw-offload=true \
          other_config:dpdk-hugepage-dir=/hugepages-2M other_config:pmd-cpu-mask=0xc0
systemctl restart ovs-vswitchd                    # drops bridge internal-port IPs

ovs-vsctl set bridge ovsbr-phy datapath_type=doca
ovs-vsctl set bridge br-int    datapath_type=doca
ovs-vsctl set Interface p0 type=doca              # uplink
ovs-vsctl set Interface <representor> type=doca   # no options

ovs-appctl dpif/show                    # doca@ovs-doca, per-port options, offloaded flows
ovs-appctl dpif-netdev/pmd-stats-show   # PMD threads (pmd-rxq-show errors on this build)
ovn-controller --version                # 24.03.8; SB schema 20.33.0
```

[bf-cfg]: https://networking-docs.nvidia.com/bsp/480/customizing-bluefield-software-deployment-using-bf-cfg
