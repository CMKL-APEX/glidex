# DPU Integration: virtio-net ports on OVS-DOCA, a chassis of the cluster's OVN

**Status: D0 met; D1 built (`crates/glidex-dpu`); the rest is design.**
§14 says what each milestone has. §0 records what was verified on a real BlueField-3 pair
while writing it; items marked **(verify)** must be checked on hardware
during the milestone that uses them (§14).

Builds on [09-networking.md](09-networking.md) (`glidex-ovs`, `glidex-netd`,
VM ports, vhost-user), [14-clustering.md](14-clustering.md) §11 (the
cluster's OVN: central on the servers, a chassis per node, logical ports
bound by `iface-id` and `requested-chassis`),
[11-security.md](11-security.md) (netd policy, ownership)
and [12-reconciliation.md](12-reconciliation.md) (VM controller, `status.nics`).
Read those first; this document only describes what changes.

A host's DPU is **one more chassis of the cluster's OVN**, next to the
host's own (decisions 5, 15). A VM's NIC on a cluster network is a
virtio-net VF that the DPU emulates, passed to the VM with VFIO; its
representor on the DPU's `br-int` carries the NIC's logical port. The
logical switches, ports, addresses, DHCP, routers and NAT are the
cluster's, unchanged (14-clustering.md §11); only the chassis a port is
bound to differs. When the DPU is unavailable, a network that allows it
puts the NIC on the host's chassis instead, on the same logical switch
(§11). DPU NICs therefore need a cluster: a single DPU host is a
one-server cluster (decision 6).

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
| 10 | The test OVN central services (NB and SB) listened on plain TCP on all interfaces, unauthenticated. | The cluster's OVN is TLS-only with the cluster CA (14-clustering.md §4); a DPU never connects to a central of its own (§8.3). |
| 11 | The DPU runs **OVN 24.03.8** (`ovn-controller`, `ovn-northd`; Ubuntu 24.04 packages), SB schema **20.33.0**. The host's Ubuntu 24.04 archive offers `ovn-central` and `ovn-host` **24.03.8** as well. | A cluster on the distro's OVN matches the DPU's chassis version; the cluster's version rule covers the DPU chassis (§8.3). |
| 12 | The test DPU **already runs its own `ovn-central`** (NB, SB, northd all active) and its OVS `ovn-remote` pointed at it. | A DPU may carry a foreign OVN central. The module reports it and repoints the chassis only with confirmation (§8.3, §9.3). |
| 13 | The host reaches the DPU over a point-to-point management link (`tmfifo_net0`, a pair of private addresses), independent of every OVS bridge. | That link is the agent's transport and survives OVS restarts (§10). The DPU chassis reaches the cluster's SB over the underlay, like any chassis (§8.3). |
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
| 32 | **The host's end of the management link has no persistent address.** On 2026-11-16 the DPU was unreachable (`Network is unreachable`): the host's `rshim` service had restarted on 2026-11-13, which recreates `tmfifo_net0`, and the address it had was set by hand at runtime (the host has only a `.link` file for the interface, and NetworkManager leaves it unmanaged). Re-adding the address restored the link; the DPU had kept its own. The D0 scripts read an SSH failure as an empty value, so a dry run against the unreachable DPU reported an unprepared DPU ("unset", "0 pages", no bridge) instead of an error. | netd must own the host side of each DPU's management link and re-apply its address when `tmfifo_net0` is recreated (§10.1); the agent's transport depends on it. The scripts now check the SSH session and `sudo` before reading anything. |
| 33 | **`--persist`'s tmpfiles entry does not survive a DPU boot.** After a reboot the 2 MB pool was 0 pages and `ovs-vswitchd` failed (`EAL: Cannot get hugepage information`, `Unable to initialize DPDK`), though the tmpfiles entry and the `fstab` mount were in place. The bundle's `mlnx_bf_configure` runs `doca-hugepages reload` at boot, after tmpfiles, and sets each pool to what `doca-hugepages` has stored under `/etc/mellanox/hugepages.d/`: nothing, because it adds its own `ovs-doca (default)` entry only when `OVS_DOCA=yes` in `mlnx-bf.conf`. Storing the pool there (`doca-hugepages config --app glidex-ovs-doca --size 2048 --num 1024`, then `reload`) fixed it. | The pool is kept with `doca-hugepages`, not tmpfiles (§9.3 step 1; `dpu-prepare.sh --persist` does so where the tool exists). |
| 34 | **The virtio-net fix survives a DPU reboot.** `dpu-prepare.sh --yes --virtio-net --int-bridge br-int --persist` made 16 changes (the reinstall of finding 28 had reset OVS to the `system` datapath, and the earlier boot had recreated SF 0 from the unchanged `mlnx-sf.conf`, so the controller had failed again with finding 29's error); a second run reported 0. After two DPU reboots (the first showed finding 33): 1024 × 2 MB pages, `ovs-vswitchd` and `virtio-net-controller` active, `doca_initialized` true, `ovsbr1` and `br-int` on `doca`, no stock SF, and `virtnet list` serving the static PF `61:00.1` (`pci_max_vfs` 0, `enabled_vfs` 0) through the controller's SF 1000. A DPU reboot left the host's networking alone (its traffic does not use the DPU's port) and its virtio PF in place. | D0 acceptance (1) holds with virtio-net on, across reboots. Next is acceptance (4): VFs and the representor bridge. |
| 35 | **A VF, end to end on the host and the DPU (D0 acceptance 4).** The host must create VFs: `sriov_numvfs` on the virtio PF needs a PF driver with SR-IOV support, and `virtio_pci` is built in and kept off the PF (finding 31). The kernel's **`pci-pf-stub`** (a module on Ubuntu 6.8) is that driver: with `sriov_drivers_autoprobe=0`, `driver_override=pci-pf-stub` and `drivers_probe`, the PF bound at once, and `echo 1 > sriov_numvfs` returned in about 1 s with VF 0 at `61:02.3` (`1af4:1041`, offset 18, stride 1), unbound, and no stall. The controller loaded it within 2 s (`virtnet_device_vfs_load`): the PF's `pci_max_vfs` and `enabled_vfs` became 1, and VF 0 got 12 MSI-X, 11 queues, a random MAC, MTU 1500, its own SF **`sf_num 3000`** (RDMA `mlx5_2`) and representor **`en3f0pf0sf3000`**, link up. `virtnet modify -p 0 -v 0 device -m <mac>` and `-t <mtu>` both succeeded on the unbound VF (`-qp`, `-n`, `-q` set queue pairs, MSI-X and queue size). The representor joined **`br-int` and `ovsbr1` alike** as `type=doca` with no options, no error and link up; both bridges are ports of the one `doca@ovs-doca` datapath. Its OVS MTU stayed 1500 after `ip link set mtu 1442`, and again with `mtu_request=1442` (accepted, no error). Writing `sriov_numvfs` 0 unloaded the VF cleanly, but the controller **kept SF 3000** and its representor. The VF's IOMMU group also holds its root port (`60:03.1`), while the PF's group holds only the PF. Everything was undone afterwards (VFs 0, PF unbound, override cleared, autoprobe 1, `pci-pf-stub` unloaded). | netd creates VFs through `pci-pf-stub` with autoprobe off (§7.1 step 3). VF *n*'s representor is `en3f0pf0sf<3000+n>`; MAC, MTU and queues are set with `virtnet modify` before the host binds the VF (step 4); the guest's MTU is the VF's, and the representor's only has to be at least that (the controller's README), which 1500 is for the cluster's 1442 (step 5). O1 leans to `br-int` (one datapath), to be settled by traffic in D4, which also checks that a VF whose group holds a root port can be opened through VFIO. A representor without a VF is the controller's, not a leftover for reconcile to report (§7.4). |
| 36 | **D0 run 6: reinstall with the virtio-net firmware active, then prepare from clean.** `bfb-install` 332 s (exit 0); key-only SSH as `glidex` 101 s after it returned (447 s for the job); `glidex` locked (`L`), `ubuntu` untouched; `BOOT_MODE` `1 (emmc)` before and after; a new SSH host key, as in finding 20. `mlxconfig -e q` (Default, Current, Next Boot) of the §9.1 values was **identical before and after** on the DPU's function and on the host's, with the virtio-net values active (`ENABLE=1`, `NUM_PF=1`, `NUM_VF=16`, `NUM_MSIX=64`, `NUM_VF_MSIX=12`, `PER_PF_NUM_SF=1`, `PF_TOTAL_SF=32`, `PF_SF_BAR_SIZE=8` on the DPU's function; the host function's own `PF_TOTAL_SF`/`PF_SF_BAR_SIZE` are 32/8 current and 0 next boot, its own setting, which it does not need). The clean image came back with the stock SF 0 and a controller that does not serve (finding 29). `dpu-prepare.sh --virtio-net --int-bridge br-int --persist` made 16 changes and verified `virtnet list`; the re-check found one more, because `doca-hugepages config` stores an entry as *inactive* until `doca-hugepages reload` (fixed: the script reloads after `config`; the one-line fix applied, the next check reported 0). After a DPU reboot: 1024 × 2 MB pages, OVS and the controller active, both bridges on `doca`, no SF 0, the PF served. The one-VF check of finding 35 repeated unchanged, and the host was restored. | **O13 answered:** a reinstall neither clears nor changes firmware values, staged or active. The §9.3 sequence, virtio-net included, reaches a working DPU from a stock image. Not yet shown: the fixed script in one pass from clean (it needs another reinstall). |

## 1. Decisions

| # | Topic | Decision |
|---|---|---|
| 1 | Where DPU logic lives | New library crate **`glidex-dpu`** (no dependency on other glidex crates, like `glidex-ovs`); wrapped by **`glidex-netd`**, which already holds root and the ownership model. |
| 2 | How glidex reaches a DPU | A small **DPU agent** (`glidex-dpu-agent`, systemd on the DPU) speaking the netd NDJSON protocol over the DPU's point-to-point management link; **not** `ssh` + `sudo` from netd. Mutual authentication; only netd's root credential can connect (§10). |
| 3 | Guest-visible device | A **virtio-net VF** emulated by the DPU (`virtio-net-controller`), attached to the VM by **VFIO passthrough** in v1. vhost-user via `vfe-vhostd` is v2 (§5.2). |
| 4 | Switching and policy | **OVN**, with `ovn-controller` and `br-int` on the DPU (OVS-DOCA, offloaded). The VM's port is an OVN logical switch port. |
| 5 | OVN central | **The cluster's OVN** (revised 2026-11-16; earlier: glidex's own single-host instance, and before that an external central). The DPU is a chassis of the OVN the cluster's servers run (14-clustering.md §11). glidex runs no OVN central for DPUs and has no external-central mode (§8). |
| 6 | Cluster required | **A DPU NIC exists only on a cluster network** (`scope: cluster`; revised 2026-11-16). A standalone host has no OVN (14-clustering.md D5, D10), so a host with a DPU that wants DPU NICs is at least a one-server cluster (`cluster init`). Probing, preparing and reinitializing a DPU work on a standalone host too (§9). |
| 7 | Fallback | If the DPU is unavailable, a network with `backend: auto` puts the NIC on the **host's own chassis** (tap, or vhost-user on a `netdev` `br-int`), on the same logical switch (revised 2026-11-16; §11). |
| 8 | Backend stickiness | A NIC's backend is decided at attach and **never changes while the VM runs** (the hypervisor holds the device). |
| 9 | Firmware and reset | **Never applied by glidex.** `dpu_probe` reports what is missing and prints the commands (§9.1). The one DPU reset glidex can drive is a **BFB reinstall of the DPU's operating system** over rshim (decision 14, §9.4); it never changes firmware settings and never power-cycles the card or the host. |
| 10 | Live migration | **Out of scope** for v1 (glidex has none yet, 14-clustering.md §2). The design leaves room: a stable VF ↔ port mapping and OVN `requested-chassis` (§15). |
| 11 | Datapath types | `doca` for bridges, `doca` for ports on a DOCA DPU. Never `dpdk`/`netdev` there. |
| 12 | PKI | The DPU chassis's certificate is the **cluster CA's** (revised 2026-11-16). The DPU generates its key and a CSR; the leader signs it. The key never leaves the DPU (§8.3). |
| 13 | NB | The NB is the cluster's, written only by the leader's network controller from the cluster's records (14-clustering.md §11). This module adds no NB state of its own (revised 2026-11-16). |
| 14 | DPU reinitialization | An explicit, confirmed, **destructive** operation, `dpu_reinit`: netd writes an operator-supplied, checksum-verified **BFB** to the DPU through `rshim` (`bfb-install`), then re-bootstraps the agent, re-enrols the chassis certificate and re-runs `dpu_prepare`. It runs as an asynchronous **job**, is admin-only, refuses while any DPU-backed VM is running, and is never triggered automatically (§9.4). |
| 15 | The DPU chassis | Each DPU is a chassis named **`<node id>-dpu`**, next to the host's `<node id>`, enrolled by the leader through `ensure_ovn_chassis` (§8.3). It is **never a gateway**: gateways stay on host `br-int`s with the `system` datapath, which external-traffic metering needs (14-clustering.md D22). |
| 16 | Which chassis a port binds to | A NIC's **backend picks its `requested-chassis`**: `<node>-dpu` for the DPU, `<node>` for the host. The scheduler chooses the backend at placement; a node that falls back at launch reports it, and the leader moves `requested-chassis` to match (§8.4). |

## 2. Scope

**In scope**
- Enrol each DPU as a chassis of the cluster's OVN, with a cluster-CA
  certificate whose key stays on the DPU (§8.3).
- Create, list and delete a virtio-net port for a VM NIC on a cluster
  network, switched by OVS-DOCA on the DPU of the VM's node, bound to
  the NIC's existing logical port (§7, §8.4).
- Probe and (with confirmation) configure the runtime parts of a DPU:
  hugepages, `doca-init`, bridges, the virtio-net controller's
  prerequisites, the chassis settings and certificate.
- Reinitialize a DPU's operating system from a BFB over rshim and bring
  it back to a working chassis (§9.4).
- Fall back to the host's own chassis for networks that allow it (§11).
- Scheduling on DPU capacity, and REST, `gxctl` and UI surfaces
  consistent with cluster networks today (§12).

**Non-goals**
- Firmware (`mlxconfig`) changes, cold resets and power cycles of the
  card or host, and `pci=realloc` / kernel command-line edits: reported,
  never performed. NIC firmware updates through the BFB are off by
  default (§9.4).
- Obtaining BFBs: glidex does not download, build or customize bundles
  beyond the first-boot configuration file it generates (§9.4).
- **An OVN of the DPU's own, or of the host's.** DPU NICs use the
  cluster's OVN; on a standalone host there are none (decision 6).
- Changes to the cluster's NB model: switches, routers, NAT, DHCP and
  IPAM are 14-clustering.md §11's. Security groups are not built there
  yet; when they are, DPU ports get them with no change here (§8.5).
- A DPU chassis as a gateway (decision 15), and provider (`localnet`)
  networks on a DPU chassis in v1 (§8.5, O18).
- SR-IOV VFs of the regular ConnectX PF as VM NICs, SFs as VM NICs,
  `docavdpa`, and hotplug virtio PFs. Static virtio PF + VFs only.
- Live migration (decision 10).
- One host's netd talking to another host's DPU: each node drives only
  its own DPU, through its own agent (§10).

## 3. Topology

```
 Cluster servers: NB / SB / ovn-northd (OVSDB Raft, cluster CA; 14-clustering.md §4)
   ▲ leader's network controller writes NB          ▲ ssl:6642          ▲ ssl:6642
   │                                                 │                   │
 Node (glidex)                                       │      DPU (OVS-DOCA, DPU mode)
┌────────────────────────────────────────────┐      │     ┌──────────────────────────────────────┐
│ control plane ──NDJSON──▶ glidex-netd      │─mgmt─┼────▶│ glidex-dpu-agent                     │
│                                            │ link │     │   ├ ovs-vsctl (br-int, ovsbr-phy)    │
│ ovn-controller, chassis "<node>" ──────────┼──────┘     │   └ virtnet (virtio-net-controller)  │
│ br-int: tap / vhost-user NICs (fallback)   │            │ ovn-controller, chassis "<node>-dpu" ┘
│                                            │            │                                      │
│ VM ── virtio-net VF (vfio-pci) ◀──PCIe─────┼───────────▶│ VF representor ─ br-int ═ Geneve ═▶ …│
│                                            │            │                  └ ovsbr-phy ─ p0 ─▶ wire
└────────────────────────────────────────────┘            └──────────────────────────────────────┘
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
| `crates/glidex-dpu/` | **New library.** DPU-side command builders (`ovs-vsctl`, `virtnet`), VF allocation, DPU probe parsing, the DPU chassis's `Open_vSwitch` settings. |
| `crates/glidex-dpu-agent/` | **New binary**, runs on the DPU. Protocol server around `glidex-dpu`; allow-listed programs only. No OVN central access. |
| `crates/glidex-netd/` | New ops and a `Backend` abstraction (§6); `ensure_ovn_chassis`, `leave_ovn` and `ovn_status` gain a DPU target, relayed to the agent (§8.3); holds the agent credential; does the host-side steps (VF creation, `vfio-pci` binding, and the `rshim`/`bfb-install` reinitialization jobs of §9.4). |
| `crates/glidex-ovs/` | Unchanged API. Gains a `Backend::HostOvs` adapter over today's `vm_port` (§11). |
| `crates/glidex-ovn/` | Unchanged. |
| `crates/glidex-control-plane/` | The DPU chassis's enrolment and CSR signing by the leader, `requested-chassis` by backend, the DPU in node status and the scheduler, VF claims, REST (§8, §12). |
| `packaging/` | `glidex-dpu-agent.service` (DPU); a `dpu.json` example. |

`glidex-dpu` has an `Exec` trait like `glidex-ovs`
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

`VmPortSpec` gains one optional field (the default keeps old records
valid); the logical port stays `ovn_lport`
([14-clustering.md §11.3](14-clustering.md#113-hosts-joining-ovn-netd)),
whose addresses and port security the leader sets in the NB:

```rust
pub struct VmPortSpec {
    /* existing, including ovn_lport */
    pub backend: Backend,                  // Host (default) | Dpu: the one placement chose (§8.4)
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
- Representor: the VF's SF representor, `en3f0pf0sf<3000+n>` for VF *n*
  on the test DPU (finding 35); netd reads the name from `virtnet list`
  rather than computing it.
  Its OVS port name is the representor netdev name; glidex tags it.
- OVN names are the cluster's
  ([14-clustering.md §11.1](14-clustering.md#111-ownership-and-naming)):
  the logical port is `gx-<vm8>-<i>` whatever the backend, and it is the
  representor's `external_ids:iface-id`; the MAC is `02:` + 5 bytes of
  `sha256(vm_id ‖ nic)` (unchanged).

## 6. Backend abstraction in netd

```rust
trait PortBackend {
    fn probe(&self) -> BackendHealth;                    // available? why not?
    fn attach(&self, spec: &VmPortSpec) -> Result<AttachResult, OvsError>;
    fn detach(&self, vm_id: &str, nic: u8) -> Result<(), OvsError>;
    fn list(&self) -> Result<Vec<PortRecord>, OvsError>;
}
// HostOvsBackend: today's vm_port::attach/detach (tap, vhost-user), with `iface-id` on cluster networks, as today.
// DpuBackend:     agent client + host VFIO steps (§7).
```

The network's policy (§12.1) and the DPU's health give the backend.
The scheduler applies it at placement (§8.4); the node applies it again
at launch, when the DPU may have gone away since:

| Network `backend` | DPU healthy | Result |
|---|---|---|
| `host` (default) | n/a | host |
| `dpu` | yes | DPU |
| `dpu` | no | error `dpu_unavailable` |
| `auto` | yes | DPU |
| `auto` | no | host, event `BackendFallback` (§11) |

**Invariant:** the chosen backend is stored in the `vm_ports` record and
re-attach (crash-restart, `attach_vm_port` on an existing record) always
uses the **recorded** backend, even if the DPU has since recovered.
Why: the hypervisor holds the device or socket; only a VM restart can
switch.

"Healthy" means: agent reachable, `ovs-vswitchd` has `doca_initialized`,
the controller reports the PF, a VF is free, and the DPU's
`ovn-controller` is connected to the cluster's SB (§8.3). Cached for 5 s
so a launch burst probes once. The node reports it in its status, which
is what the scheduler reads (§12.3).

## 7. Port lifecycle (DPU backend)

### 7.1 Attach

All steps are idempotent; each records progress in the `vm_ports` record
so a crash resumes.

1. **Validate** (netd, no side effects): ids, MAC, MTU, queue pairs
   (≤ the controller's per-VF maximum, default 4), network, switch name.
2. **Allocate VF** from the DPU inventory (`dpu_list_vfs`); `dpu_capacity`
   if none is free. Store `dpu_vf`.
3. **Host: make sure the VF exists.** The PF is bound to `pci-pf-stub`
   (`driver_override`, then `drivers_probe`) with
   `sriov_drivers_autoprobe=0`, so no host driver takes a new VF (finding
   35; `virtio_pci` stays off the PF, finding 31). Read the PF's
   `sriov_totalvfs`/`sriov_numvfs`; if the VF is not yet enabled write
   `sriov_numvfs` (all-or-nothing change: raising the count with VFs in
   use fails in the kernel, so netd sets it to `total_vfs` once, at the
   first attach, and never lowers it while any port exists).
4. **DPU: configure the VF** through the agent: MAC, MTU, queue count
   (`virtnet modify -p <pf> -v <vf> device -m <mac> -t <mtu> -qp <pairs>`,
   finding 35). The VF's MAC/MTU/MSI-X must be set **before** the host
   binds a driver.
5. **DPU: wait for the representor** (≤ `port_timeout_secs`, default 30),
   then `ovs-vsctl --may-exist add-port <representor_bridge> <rep> -- set
   Interface <rep> type=doca external_ids:iface-id=<lsp>
   external_ids:glidex-owner=glidex external_ids:glidex-role=vm
   external_ids:glidex-vm-id=<id> external_ids:glidex-nic=<n>`;
   `ip link set <rep> up`. The guest's MTU is the VF's (step 4); the
   representor's must be at least that, and neither `ip link` nor
   `mtu_request` changed what OVS reports for a `doca` representor
   (1500, finding 35). Whether a larger MTU needs anything here is D4's.
6. **OVN: wait for the binding.** The logical port already exists: the
   leader made it at placement, with `requested-chassis=<node>-dpu`
   (§8.4). The DPU's `ovn-controller` claims it once the representor
   carries its `iface-id`; the VM controller waits for `ovn_installed`
   as for any cluster NIC
   ([14-clustering.md §11.5](14-clustering.md#115-vm-port-lifecycle),
   10 s, then `NetworkReady=False/PortNotInstalled`). If the
   representor does not appear within `port_timeout_secs`, step 5 is
   rolled back and the result is `dpu_port_timeout`.
7. **Host: bind the VF to `vfio-pci`**: `driver_override`, unbind,
   `drivers_probe` (same sysfs sequence as the DPDK uplink,
   `crates/glidex-ovs/src/uplink.rs`); record the original driver. Return
   `Vfio { bdf }`.

The hypervisor then opens the device through the existing VFIO launch
path ([04-hypervisors.md](04-hypervisors.md)); the only differences from a
user-attached `vfio_devices` entry are that netd owns it and it appears
in `status.nics`, not in the spec's device list.

### 7.2 Detach and release

Reverse order; every step tolerates "already gone": rebind the original
driver (or leave unbound if none), `del-port` on the DPU, free the VF.
The logical port is the leader's and follows the VM, not its instances
(14-clustering.md §11.5 step 3). `release_vm` does this for every NIC of a VM.
`detach_vm_port` and `release_vm` keep the owner-uid check from
[09-networking.md §7.2](09-networking.md#72-authorization).

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
(`dpu_list_ports`) and the host:

| Situation | Action |
|---|---|
| Record present, DPU port missing | re-create (steps 4–6); event `PortRestored` |
| DPU port tagged glidex, no record | **report** in `ReconcileReport`, never delete ([09-networking.md §7.7](09-networking.md#77-startup-and-reconciliation) rule 4) |
| Host rebooted (VFs gone; `sriov_numvfs` = 0) | VMs are stopped anyway; netd recreates VFs at the next attach |
| DPU reset (representors gone, controller restarted) | running VMs lose their NIC: `NetworkReady=False/PortLost`, `RestartRequired=True/PortLost`, same as a lost tap |

The NB side is the leader's: a missing logical port or chassis binding
is re-created by its network controller (14-clustering.md §11.5 step 4).

## 8. OVN: the DPU as a chassis of the cluster's OVN

### 8.1 What the cluster already provides

In a cluster, the servers run the NB and SB databases and `ovn-northd`
in their own OVSDB Raft clusters, over TLS with the cluster CA; only the
leader's network controller writes the NB; every node is a chassis set
up by netd's `ensure_ovn_chassis`; a VM's NIC is a logical port
`gx-<vm8>-<i>` with the control plane's addresses and port security,
bound by `iface-id` and `requested-chassis=<node>`
([14-clustering.md §11](14-clustering.md#11-ovn-networks)). This module
adds one chassis per DPU and changes nothing in the central or the NB
model.

### 8.2 A cluster is required (decision 6)

A NIC with the DPU backend exists only on a `scope: cluster` network. A
standalone host has no OVN, so a network policy asking for the DPU there
is refused (`400 invalid_network`, `details.missing: ["cluster"]`). A
single DPU host becomes a one-server cluster with `cluster init`, whose
server runs the OVN central (14-clustering.md §5.1). `dpu_probe`,
`dpu_prepare` and `dpu_reinit` work without a cluster; they skip the
chassis step (§9.3 step 4).

### 8.3 The DPU chassis (decision 15)

- **Name:** `system-id` `node:<node id>-dpu`; the host's chassis stays
  `node:<node id>`, the form SB RBAC matches against a certificate's CN
  (`glidex_ovs::ovn::chassis_name`). This document writes them
  `<node>-dpu` and `<node>`. The node's status lists both.
- **Enrolment:** when a node reports a prepared DPU, the leader calls
  `ensure_ovn_chassis` with `target: {dpu: <id>}`. netd relays it to
  the agent, which sets the DPU's `Open_vSwitch` `external_ids`
  (`system-id`, `ovn-remote` the same SB remotes as the host's chassis,
  `ovn-encap-type=geneve`, `ovn-encap-ip` from `dpu.json`, no
  `ovn-bridge-mappings` in v1), makes sure `br-int` exists on the `doca`
  datapath, and enables the DPU's `ovn-controller`. Idempotent, as for
  the host.
- **Certificate:** the agent generates the key and a CSR (`csr`); netd
  passes the CSR to the control plane, the leader signs it with the
  cluster CA (subject: the chassis name, which the SB's `ovn-controller`
  role ties to the chassis, **verify** O19), and the agent installs the
  certificate (`install_cert`). The private key never leaves the DPU.
  Renewal and CA rotation take the same path as the node's own chassis
  certificate (14-clustering.md D20).
- **Underlay:** the DPU's `ovn-encap-ip` must reach every other
  chassis's (the servers', the other nodes', its own host's), with the
  cluster's MTU rule (`underlay_mtu − 58`,
  [14-clustering.md §11.6](14-clustering.md#116-mtu-and-offload));
  **verify** O6.
- **Version:** the DPU's `ovn-controller` must not be newer than the
  cluster's `ovn-northd`, the rule the cluster applies to every chassis;
  `dpu_probe` reports `ovn_version`, and a DPU that breaks the rule is
  not enrolled (`unsupported { missing: ["ovn<=<ver>"] }`). On the test
  pair both are 24.03.8 (finding 11). The DOCA bundle ships the DPU's
  OVN; glidex never upgrades it.
- **A DPU's own central** (finding 12): reported as
  `local_ovn_central: active`; stopped and disabled (never uninstalled,
  its NB never read) only when `dpu_prepare` is confirmed with
  `stop_local_central: true`. Until then the DPU is not enrolled,
  because repointing `ovn-remote` strands whatever that central managed.
- **Leaving:** `leave_ovn` with the DPU target clears the settings and
  stops the DPU's `ovn-controller`; a node removed from the cluster
  leaves both chassis (14-clustering.md §5.5), and the leader's clean-up
  of tombstoned nodes' chassis covers `<node>-dpu`. `dpu_reinit` deletes
  it first (§9.4.3 phase 2).
- netd's ownership markers (`glidex-owner=glidex`, `glidex-role=vm`,
  `glidex-vm-id`, `glidex-nic`) apply to the representor ports on the
  DPU's `br-int` as they do on the host's. Neither netd nor the agent
  touches `br-int` flows.

### 8.4 Ports and their chassis (decision 16)

- **NB unchanged.** The switch `gx-<network id>`, the logical port, its
  `addresses` and `port_security` from IPAM, OVN DHCP, routers and SNAT
  are the leader's, whatever the backend (14-clustering.md §11.2–§11.5).
- **Placement** (leader): the scheduler picks the node and, from the
  network's `backend` policy (§12.1) and the node's reported DPU state,
  the NIC's backend; it claims a VF for a DPU NIC (§12.3). The network
  controller creates the logical port with
  `requested-chassis=<node>-dpu` for the DPU, `<node>` for the host, and
  the NIC's placement records the backend.
- **Launch** (node): netd attaches with the recorded backend (§7.1). If
  the DPU is unavailable and the policy is `auto`, it attaches on the
  host instead and the node reports `status.nics[].backend = host`; the
  leader's network controller moves `requested-chassis` to match, the
  same drift path as a missing binding (14-clustering.md §11.5 step 4),
  and the launch waits for `ovn_installed` as usual. Until it moves, the
  port is bound nowhere, so a NIC never has two live bindings.
- **Stickiness:** a NIC's backend does not change while the VM runs
  (decision 8); the next placement or start may choose again.

### 8.5 Rules

- **Never a gateway** (decision 15): the DPU chassis is never in the
  edge's or a VPC router's HA chassis group. A DPU NIC's external traffic
  crosses Geneve to a gateway node, as any NIC's, and is metered there
  (14-clustering.md D22).
- **Network modes:** `isolated` and `nat` cluster networks. A `provider`
  network's `localnet` port needs bridge mappings on the DPU to a bridge
  that holds its uplink; v1 refuses the DPU backend on one
  (`invalid_network`, O18).
- **Security groups** (port groups and ACLs) are later work in the
  cluster (14-clustering.md §18); a DPU port gets them unchanged when
  they come, and whether their conntrack rules offload is O5.

### 8.6 Trust

Enforcement is on the DPU (flows, offload); the **policy source** is the
cluster's NB on the servers, which only the leader writes. On an agent
node, host root cannot change the NB. It can still reach its own DPU's
OVS through the agent, whose credential netd holds, so the DPU protects
the *data path* from a compromised guest or hypervisor process, not from
the host's root user ([11-security.md](11-security.md)). On a server,
host root also holds the cluster CA key and the NB credential
(14-clustering.md §12.1).

### 8.7 When OVN or the leader is away

As for every chassis: `ovn-controller` keeps its installed flows, so
traffic already switched continues. Placements, new logical ports and
`requested-chassis` changes wait for a leader; a launch whose port is
not bound starts with `NetworkReady=False/PortNotInstalled`
(14-clustering.md §11.5).

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
| `chassis` (`name`, `remote`, `sb_connected`) | `Open_vSwitch` `external_ids`, `ovn-appctl -t ovn-controller connection-status` |
| `ovn_cert` (present, expiry, issuer fingerprint) | the DPU's certificate files |
| `bridges` | `ovs-vsctl list Bridge` (datapath type, owned) |
| `combinations` | which of §5.2's modes are possible now, and what's missing |

Host side (`probe` extended): virtio PF present (by PCI id/driver),
`sriov_totalvfs`, `vfio_pci`/`virtio_vfio_pci` modules, IOMMU groups
(reuses `HostCapabilities.iommu`), and `pci=realloc` and the
`virtio_pci` initcall blacklist on the command line. The host's own
chassis is reported by `ovn_status`, as today.

### 9.3 `dpu_prepare` (confirm required)

Applies only runtime and OVSDB state; reports `changed: false` when
nothing differs. Order, with the interruption each step causes:

1. 2 MB hugepage pool (`nr_hugepages` under `hugepages-2048kB`), a
   `hugetlbfs` mount, the pool stored with `doca-hugepages` (the bundle
   reloads it at boot over anything else, finding 33) and the mount in
   `fstab`, so both survive reboot.
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
4. **Chassis** (in a cluster only; skipped on a standalone host): with
   `stop_local_central: true` a DPU-local central is stopped first;
   then the node reports the DPU prepared and the leader enrols the
   chassis (§8.3: CSR, signed certificate, `external_ids`). The step is
   done when the DPU's `ovn-controller` is connected to the cluster's SB.
5. Re-apply the underlay IP and routes that the restart dropped (finding
   6), from `dpu.json`'s `underlay` block. Persistence of these across a
   DPU reboot is the operator's (netplan on the DPU); glidex reports
   when they are runtime-only, as for host NICs
   ([09-networking.md §8.5](09-networking.md#85-ip-migration-decision-2)).

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
    - the cluster CA **certificate** (public, when the host is in a
      cluster) and the DPU's chassis name (§8.3); the `glidex-dpu-agent` unit and binary from
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
| 2 | `quiesce` | detach every port on this DPU in netd (hypervisors are already stopped); the leader deletes the DPU's chassis (`<node>-dpu`) from the cluster's SB; mark the DPU `reinitializing` so §6's health check says `dpu_unavailable` and `auto` networks fall back (§11) while `dpu` networks refuse | roll the markers back; job fails |
| 3 | `install` | `bfb-install -r <dev> -c <bf.cfg> -b <path>`; typically **about 10 minutes** (O15); progress streamed from `/dev/rshim0/misc` (`DISPLAY_LEVEL` 2) and the `console` device into the job log (tail kept, 64 KiB); a hard timeout of `timeout_secs` | on timeout or a nonzero exit: job `failed`, DPU left as is, **no automatic retry**; the console tail is in the job |
| 4 | `wait` | the `console` device shows the boot sequence; wait for the `DPU is ready` message, the management link (`tmfifo_net0`) and the DPU's SSH port; budget 15 min after `bfb-install` returns | `failed`, DPU in an unknown state; `dpu_probe` reports it `unreachable` |
| 5 | `bootstrap` | connect **once** over SSH with netd's bootstrap key (the only SSH use in this design; decision 2 otherwise applies) and run the agent's one-time `enroll`: the agent generates its server key and a CSR, netd signs it, and the agent starts listening with mutual TLS (§10.2); then remove the bootstrap key from the DPU and verify the agent answers `hello` over TLS | the bootstrap key is removed even on failure; the job fails with the step that broke |
| 6 | `prepare` | run `dpu_prepare` (§9.3) with `confirm` carried from the job: hugepages, `doca-init`, `doca` bridges and ports, chassis certificate and `ovn-remote`, underlay address | `failed`; `dpu_prepare` is idempotent and can be re-run on its own |
| 7 | `verify` | `dpu_probe` must report no missing item that the pre-install picture did not already list; in a cluster, the chassis is enrolled again and connected to the SB (§8.3); the agent reports the same bundle version as the BFB's | `failed` with the missing items |
| 8 | `release` | clear `reinitializing`; the §7.4 port reconcile runs; event `DpuReinitialized` | – |

A **completed** job leaves the DPU as `dpu_prepare` would, with a fresh
chassis identity and certificate. A **failed** job never retries by itself,
leaves the DPU marked `reinitializing` (so nothing is placed on it) until
an admin runs `dpu_reinit` again or clears the mark
(`dpu_job` `{abandon: true}`), and keeps the console tail for diagnosis.

#### 9.4.4 What survives, and what it costs

| Survives | Lost |
|---|---|
| `mlxconfig` (firmware) settings, to be verified (O13) | all DPU OS state, including the agent's keys and a DPU-local OVN central's data |
| host-side state: netd DB, `dpu.json`, VF configuration on the host; the cluster's NB, SB and CA, on the servers | the DPU chassis identity (a new certificate is issued; the old chassis row is deleted in `quiesce`) |
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
classification of [09-networking.md §8.4](09-networking.md#84-nic-classification))
if the host's default route goes through a function of this DPU.

## 10. Agent, credentials and configuration

### 10.1 Host configuration

There is no OVN file of this module's own: the SB remotes, the CA and
the host's chassis are the cluster's (14-clustering.md §10.2, §11.3).

**The management link's host side is netd's.** `dpu.json`'s `link`
(`{"iface": "tmfifo_net0", "address": "<host-side-address>/<len>"}`)
names it; netd applies the address at start and again whenever the
interface is recreated (an `rshim` restart does that and drops a runtime
address, finding 32), and `dpu_probe` reports a link without it as
`missing: ["link_address"]`.

`/etc/glidex/dpu.json` (`root:root 0600`):

```json
{ "dpus": [ {
    "id": "dpu0",
    "agent": "tcp://<dpu-mgmt-address>:<port>",
    "link": { "iface": "tmfifo_net0", "address": "<host-side-address>/<len>" },
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

A missing `dpu.json` means "no DPU" (the DPU backend is unavailable;
`auto` networks use the host). v1 supports exactly one DPU entry per host; the list shape
is for later. The agent channel's own certificates (`tls`) are separate
from the chassis certificate of §8.3: they authenticate netd to the agent, and are created
by the operator at first setup (or by `gxctl dpu enroll`, **verify**
O10).

### 10.2 The agent

- Transport: the netd NDJSON protocol ([09-networking.md §7.3](09-networking.md#73-protocol))
  over TLS with **mutual** authentication on the DPU's point-to-point
  management link. It listens only on that link's address, never on the
  out-of-band or underlay interfaces.
- Authorization: the client certificate's identity must match the host's
  netd identity; nothing else is accepted. No per-user rights on the DPU:
  user-level authorization stays in netd and the control plane
  ([11-security.md](11-security.md)).
- Programs: an allow-list enum like `Program` (`OvsVsctl`, `OvsAppctl`,
  `Virtnet`, `Ip`, `Devlink`, `Mlxconfig` (query only), `Systemctl`, and
  sysfs writes for hugepages), vector arguments, no shell. It has **no**
  OVN central programs: all NB/SB work happens on the cluster's servers. Certificate
  signing requests are generated in-process (no `openssl`).
- First start after a reinstall: the agent has no certificate and
  listens for nothing until `enroll` (§9.4.3 phase 5) completes. `enroll`
  is a local command, run once by netd over its bootstrap SSH session, and
  it refuses to run if the agent already has an identity.
- State: none of its own. Tagged OVSDB rows are the record; the agent
  restarts freely.
- Version handshake like `hello`; a protocol or agent version older than
  netd's is `unsupported`.

## 11. Host fallback: the node's own chassis

Applies to cluster networks with `backend: auto` when the node's DPU is
unavailable at placement or at launch (§8.4). The NIC gets the port it
would have on a node with no DPU: a tap, or a vhost-user port where the
node's `br-int` is on the `netdev` datapath, on the host's `br-int` with
the same logical port. The host's chassis is the node's own, which the
cluster already runs ([14-clustering.md §11.3](14-clustering.md#113-hosts-joining-ovn-netd));
nothing is added for the fallback.

Requirements, probed and reported when missing:

- The node is a chassis of the cluster's OVN, as every cluster node is.
- For vhost-user: `spec.br_int_datapath` resolves to `netdev`
  (14-clustering.md §11.6). A tap needs nothing more.
- MTU: both chassis use the cluster's guest MTU, so a NIC's MTU is the
  same on either backend.

Behaviour differences the control plane surfaces (`status.nics[].backend`):

| | DPU | Host fallback |
|---|---|---|
| Switching CPU | DPU eswitch + Arm | host (kernel datapath or PMD cores) |
| Hugepages | on the DPU | host, for vhost-user only (09-networking.md §0 finding 9) |
| Offload | hardware | none |
| Data path under a compromised host | enforced off-host | enforced on the host |

(The policy source is the cluster's NB in both cases, §8.6.) The last
row is why fallback is **opt-in per network** (`backend: auto`; the
default is `host`, and `dpu` never falls back): it silently weakens
isolation. A VM launched on fallback gets event `BackendFallback` and
condition `NetworkDegraded=True/HostBackend`.

Mixed VMs (one NIC on each backend) are allowed. A NIC's backend does
not change at runtime (decision 8); after the DPU recovers, a restart
moves the VM's NICs back. `GET /networks/{name}` lists which NICs are on
the host backend so an operator can find them.

## 12. Control plane

### 12.1 Models

```rust
pub struct Network {
    /* existing */
    pub backend: BackendPolicy,            // Host (default) | Dpu | Auto; cluster networks only
}
pub enum BackendPolicy { Host, Dpu, Auto }
```

No new network mode: `backend` applies to `isolated` and `nat` cluster
networks (§8.5). The default `host` keeps existing networks as they are.
`port_type` is derived: DPU backend → `DpuVirtio`; host → `Tap` or
`VhostUser`, as today. `NicBinding.binding` gains `Vfio { bdf }`
(`crates/glidex-control-plane/src/models.rs`), and a NIC's placement and
`status.nics[]` gain `backend`.

Validation: setting `backend: dpu` or `auto` on a node network, a
`provider` network, or any network of a standalone host is
`400 invalid_network` with `details.missing`. Memory: a `Vfio` NIC does
**not** force shared guest memory; a host vhost-user NIC does
(unchanged).

### 12.2 Hypervisors

- `Vfio { bdf }` uses the existing VFIO device launch path in both
  Cloud Hypervisor and QEMU (04-hypervisors.md). The guest's MAC is the
  one the DPU programmed, so `network-config` matching by MAC
  ([09-networking.md §11.5](09-networking.md#115-guest-network-config)) is
  unchanged.
- The control plane's existing "VFIO conflict" list gains the VF's `bdf`
  (`GET /pci-devices` shows `in_use: "dpu port <vm>/<nic>"`), so a user
  cannot also hand it to a VM with `attach_device`.

### 12.3 Network controller and scheduler

- **Node status** gains `dpus: [{id, state, vfs_total, vfs_free,
  chassis, sb_connected}]`, from netd's `dpu_probe` and the health check
  of §6.
- **Scheduler:** a NIC on a `dpu` network needs a node whose DPU is
  healthy with a free VF; an `auto` network prefers one and accepts any
  node. VFs are claimed per node at placement and freed with the VM,
  like VFIO claims
  ([22-host-topology.md](22-host-topology.md)), and its explanation names
  the DPU when that is why a node was refused.
- **Network controller** (`controller/network.rs`): sets each logical
  port's `requested-chassis` from the NIC's backend, follows a node's
  fallback (§8.4), and enrols each prepared DPU's chassis (§8.3).
  Conditions on the node: `DpuReady`, `DpuChassisRegistered`.
- The control plane's "VFIO conflict" list gains each DPU NIC's `bdf`
  (§12.2).

### 12.4 REST and CLI

| Method + path | netd op |
|---|---|
| `GET /nodes/{id}/dpus` | `dpu_probe` |
| `POST /nodes/{id}/dpus/{dpu}/prepare {confirm?, stop_local_central?}` | `dpu_prepare` |
| `POST /nodes/{id}/dpus/{dpu}/reinit {confirm?}` | `dpu_reinit` (202 with a job id; 409 `confirmation_required` first) |
| `GET /nodes/{id}/dpus/{dpu}/jobs/{job}` · `DELETE …/{job}` | `dpu_job` (status; `abandon`) |
| `GET /nodes/{id}/dpus/{dpu}/ports` | `dpu_list_ports` |
| `POST /networks` · `PATCH /networks/{id}` with `backend` | validation (§12.1), then the network controller |

A standalone host's node id is `local`, as for its other node routes.
`gxctl`: `dpu status [--node n]`, `dpu prepare [--node n]
[--stop-local-central] [--force]`, `dpu reinit <id> [--node n] [--force]`
(prints the job, then follows it until a phase fails or `release`
completes; Ctrl-C detaches and leaves the job running),
`dpu job <id> <job>`, and `--backend host|dpu|auto` on the commands that
create and change cluster networks.
UI: a **DPU** panel on the Node page (probe, missing items, prepare and
reinit with the confirmation dialog, the chassis), a backend choice on
cluster networks, and a backend column on NICs.

Error mapping adds: `dpu_unavailable` → 503 `dpu_unavailable`;
`dpu_capacity` → 409 `dpu_capacity`; `dpu_port_timeout` → 502
`netd_error`.

## 13. Protocol additions

New ops on the **full** sockets only (never the status socket);
`dpu_probe` is read-only but returns tenant-adjacent state:

| Op | Args → result |
|---|---|
| `dpu_probe` | `{id?}` → `DpuCapabilities` |
| `dpu_prepare` | `{id?, confirm, stop_local_central?}` → `PrepareReport` (`ConfirmationRequired` first) |
| `dpu_list_ports` | – → records with live DPU state |
| `dpu_reinit` | `{id?, confirm}` → `{job}` (`ConfirmationRequired` first; §9.4) |
| `dpu_job` | `{job, abandon?}` → `JobStatus {phase, state, log_tail, result?}` |
| `dpu_chassis_csr` | `{id}` → `{csr}` (the agent's, §8.3) |
| `ensure_ovn_chassis` / `leave_ovn` / `ovn_status` | gain `target: host (default) \| {dpu: id}`; the DPU target is relayed to the agent, with the signed certificate on `ensure_ovn_chassis` |
| `attach_vm_port` / `detach_vm_port` / `release_vm` | extended as in §5, no new op names |

Agent-side ops (netd → agent, not on netd's sockets): `hello`, `probe`,
`list_ports`, `ensure_vf`, `attach_rep`, `detach_rep`, `prepare`,
`chassis` (the §8.3 settings), `leave_ovn`, `csr`, `install_cert`.

New error codes: `dpu_unavailable`, `dpu_capacity`, `dpu_port_timeout`,
`dpu_job_failed` (a job's terminal state when it is polled as an error);
`host_interface_in_use` is reused by §9.4.4. `PROTOCOL_VERSION` stays 1:
all additions are optional fields or new ops, and an older netd answers
an unknown op with `protocol_error`. Per-op policy
([09-networking.md §7.2](09-networking.md#72-authorization)) applies; the
default `glidex-admin: ["*"]` covers them, and the default `glidex`
policy gets `dpu_probe`, `dpu_list_ports`, `dpu_chassis_csr`,
`attach_vm_port`, `detach_vm_port`, `release_vm`, `dpu_job` (status
only) and the DPU target of the chassis ops, but **not** `dpu_prepare` or
**`dpu_reinit`** (they restart OVS or erase the DPU).

## 14. Milestones

Each is one PR or a short series. Ignored e2e tests need
`GLIDEX_TEST_DPU=1` (hardware, on a host that is a one-server cluster
at least); CI runs unit, fake-agent and cluster tests.

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
| **D0** hardware bring-up (no glidex code; **met, except traffic through a VF, which is D4's**: acceptance (1) met (findings 16–18), with virtio-net from a stock image in run 6 (finding 36); (3) **met** in run 5 (findings 19-28); (2) **met**: firmware activated, the virtio-net PF served across DPU reboots, and the values unchanged by a reinstall (findings 29-31, 34, 36); (4) **mostly met** (finding 35): a VF created through `pci-pf-stub`, configured and its representor on `br-int`; traffic through it is D4's; (5) **met**: `crates/glidex-dpu/tests/fixtures/` holds the scrubbed outputs with no VF and with one) | Operator-run scripts and a runbook under `scripts/dpu/` (next to `scripts/dev/`): `dpu-probe.sh` collects every §9.2 field as JSON; `dpu-prepare.sh` is the §9.3 sequence as idempotent shell with `--dry-run`, per-step timing, and the restart/underlay handling; `dpu-reinit.sh` generates the §9.4.1 `bf.cfg` and drives `bfb-install -r … -c … -b …`, capturing the rshim `console`. Run on a DPU that may be wiped, from a stock BFB. Scripts hold no secrets and take paths and keys as arguments. | Findings recorded in §0 (and the open questions closed): (1) from a freshly reinstalled DPU, `dpu-prepare.sh` alone reaches a probe with no gaps and a `doca` bridge carrying traffic, and a second run changes nothing; (2) **O13**: stage the §9.1 firmware values (operator decision), follow the cold-reset procedure, record `mlxconfig q`, reinstall with `WITH_NIC_FW_UPDATE=no`, record it again; (3) the `bf.cfg` path: where `authorized_keys` and the agent unit must go under `/mnt/`, unit enablement by symlink, key-only first login and the `DPU is ready` gate (**O17**), the console markers a job can key on, the measured duration (**O15**), and which devices the BFB writes (**O16**); (4) with firmware staged, **O1–O3** (representor bridge, `virtnet` flags, minimal parameter set); (5) every command's raw output saved as fixtures under `crates/glidex-dpu/tests/fixtures/`, scrubbed of addresses, MACs and identifiers, as the canned inputs for D1's probe-parsing tests. |
| **D1** `glidex-dpu` core (**built**: `crates/glidex-dpu`, unit tests and `tests/fixtures.rs` against both D0 fixture sets) | types, `Exec`/`RecordingExec`, command builders (`ovs-vsctl`, `virtnet`), probe parsing, VF allocator | Unit tests: exact commands per port; idempotency flags; `type=doca` with no options; refusal of `dpdk`/`netdev` on a DOCA DPU; probe parsed from canned output of the test DPU; allocator reuse after free. |
| **D2** the DPU chassis in the cluster's OVN | `ensure_ovn_chassis`/`leave_ovn`/`ovn_status` with a DPU target (relayed to a fake agent), CSR signing by the leader with the cluster CA, the version rule, tombstone clean-up of `<node>-dpu`, `requested-chassis` by backend, the backend at placement and the follow-up after a node's fallback | Unit tests: the agent's `external_ids` and `br-int` for a DPU target; a signed certificate's subject and issuer; a DPU newer than `ovn-northd` not enrolled; `requested-chassis` per backend and after a reported fallback. Cluster tests (`cluster_tests.rs`, fake agent): a node reporting a prepared DPU gets its chassis enrolled; removing the node deletes both chassis; the DPU's key never reaches the control plane. |
| **D3** agent + netd backend | agent binary, mTLS, allow-list, `Backend` trait, `HostOvsBackend` adapter, backend selection and stickiness, `dpu.json`, the management link's host side (§10.1) | Fake-agent tests: every §6 table row; recorded backend wins on re-attach; agent down → `dpu_unavailable`/fallback; an unparseable `dpu.json` stops netd. Link tests (`RecordingExec`, netlink events faked): netd applies `link.address` at start; re-applies it when `tmfifo_net0` is recreated (finding 32) and leaves an address it finds already set alone; `dpu_probe` reports `missing: ["link_address"]` while it is absent. On the test host (ignored, `GLIDEX_TEST_DPU`): `systemctl restart rshim` and the agent answers `hello` again without anyone touching the host. |
| **D4** DPU attach (hardware; needs D0) | §7.1 end to end on one DPU, `dpu_probe`, the DPU chassis enrolled on real hardware | Ignored e2e (`GLIDEX_TEST_DPU`, a one-server cluster): a VM gets a virtio NIC, a DHCP address from the cluster's OVN, pings a host-backed VM on the same logical switch and, through `gx-edge`, an address outside; detach leaves no tagged rows; the DPU's private key never appears on the host. **Also settles O1–O3** and trims §9.1. |
| **D5** `dpu_prepare` (needs D0) | §9.3 incl. hugepage persistence, `stop_local_central` | Unit tests per step; e2e: from a stock DPU (firmware already set, local central running) to a probe with no gaps; restart interruption reported; underlay re-applied; DPU-local central stopped only with the flag. |
| **D5b** `dpu_reinit` (needs D0) | §9.4: device identity checks, BFB checksum, `bf.cfg` generation, `bfb-install` under a transient unit, the eight phases, jobs table, admin-only policy, `gxctl dpu reinit` | Unit tests with `RecordingExec`: exact `bfb-install` arguments; refusal on rshim mismatch, bad checksum, running tenant, concurrent job, default route through the DPU; `bf.cfg` contains a public key and the CA certificate and **no** password or private key; each phase's failure rule; a netd restart mid-install reattaches instead of starting a second install. Ignored e2e (`GLIDEX_TEST_DPU_REINIT=1` **and** an explicit DPU id; never in CI): from a running DPU to a verified `release`, with the timing recorded for O15. |
| **D6** control plane + UI | `backend` on cluster networks, the DPU in node status and the scheduler, VF claims, `Vfio` bindings in both hypervisors, conditions, REST/CLI/UI | API tests against a fake netd+agent; scheduler tests for `dpu` and `auto`; hypervisor argument tests (VFIO NIC) for CH and QEMU. |
| **D7** fallback integration | `BackendFallback`, `NetworkDegraded`, mixed VMs, rollback on recovery | e2e: stop the agent, launch a VM on an `auto` network, it binds to the host's chassis and pings a DPU-backed VM on the same switch; a `dpu` network refuses. |
| **D8** (later) vhost-user via `vfe` | §5.2 | after O4 is answered. |

## 15. Open questions (verify on hardware)

| # | Question |
|---|---|
| O1 | **Leaning `br-int` (finding 35):** the representor joins `br-int` and the uplink's bridge alike, and both are ports of one `doca@ovs-doca` datapath. Still open: traffic and offload through it with OVN's flows (D4). |
| O2 | **Answered (finding 35):** `virtnet modify -p <pf> -v <vf> device -m/-t/-qp/-n/-q`; VF *n*'s representor is `en3f0pf0sf<3000+n>` (`sf_num 3000+n`), there within 2 s of `sriov_numvfs`. |
| O3 | **Mostly answered (findings 29-31, 35):** `pci=realloc`, the `virtio_pci` initcall blacklist and no stock SF are needed; VFs come from the host through `pci-pf-stub`. The minimal firmware and host-kernel set for a static PF with VFs and no hotplug (`pci=realloc`, MSI-X counts); how many VFs the B3140L allows (`PF_TOTAL_SF` ceiling; the test card reported 32 external SFs at default). |
| O4 | Where `vfe-vhostd`/`vfe-vhost-cli` come from for this host, and which versions. |
| O5 | Whether OVN SNAT/DNAT and conntrack ACLs offload on OVS-DOCA 3.5 (the doc excludes `ct-ct-nat`). |
| O6 | Underlay reachability and MTU between the DPU chassis and every other chassis, including its own host's, whose traffic crosses the DPU's eswitch (§8.3). |
| O7 | Whether VF state survives a controller restart (`recovery=1` default) and how netd should rediscover VFs after one. |
| O8 | Migration: the guide documents VFIO migration (kernel 6.13+, QEMU 9.1+, `virtio_vfio_pci`) and a vhost-user path; the test host has kernel 6.8 and QEMU 8.2.2. Revisit if glidex ever gets multi-host. |
| O9 | **Dropped** (decision 5, revised): the central is the cluster's; nothing runs one on the DPU. |
| O10 | Enrolling the agent channel (`dpu.json` `tls`): operator-issued certificates at first setup vs a `gxctl dpu enroll` flow over the rshim/management link. |
| O11 | Whether the cluster CA's issuing code (`glidex-tls`) can sign a CSR from the DPU with the subject and key usages OVN requires, or needs a CSR path added. |
| O12 | Whether the host's OVS (which reports `3.5.0052`, a DOCA-host build) can run the node's `br-int` on the `netdev` datapath with `dpdkvhostuserclient`, for vhost-user fallback NICs (§11); taps need nothing. |
| O13 | **Answered (findings 28, 36).** A BFB reinstall with `WITH_NIC_FW_UPDATE=no` leaves `mlxconfig` values as they were, staged (`Next Boot`) and active (`Current`) alike. |
| O14 | **Resolved (findings 26-28).** `bf.cfg` through `bfb-install -c`; the `bfb_modify_os` hook writes a first-boot script as vendor-data; cloud-init's `scripts_vendor` runs it once per instance; the account it creates authenticates by key. |
| O15 | **Answered**: about **5 minutes** for the install (310 s, 304 s and 332 s), plus about 2 minutes until key-only SSH works; the earlier ~10 minute estimate was for the whole cycle. Findings 19, 28, 36. |
| O16 | **Answered (findings 19, 28)**: the installer erases both `/dev/mmcblk0` and `/dev/nvme0n1`; the OS lands on `nvme0n1` (`p1` EFI, `p2` root) as `device=` says; `BOOT_MODE` reads `1 (emmc)` before and after. |
| O17 | **Answered (finding 28)**: the `glidex` account (locked password, key only) accepts a non-interactive SSH session as soon as the first-boot script has run, 115 s after `bfb-install` returned; no expiry or `DPU is ready` interference. |
| O18 | Provider (`localnet`) networks on a DPU chassis: which DPU bridge their physnets map to (the uplink's `ovsbr-phy`, or a bridge per physnet), and whether a VLAN-tagged `localnet` port offloads (§8.5). |
| O19 | Whether the SB's `ovn-controller` RBAC role accepts a chassis whose certificate subject is `<node>-dpu`, as it does for `<node>` (§8.3). |

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
