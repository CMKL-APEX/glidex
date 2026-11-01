# Installation

`glidex-install` installs Glidex and everything it needs on one host, and
brings an existing install up to date. Run it again at any time: it only
changes what is missing or outdated.

## Requirements

- Linux on x86_64 or aarch64, with KVM enabled (`/dev/kvm`). QEMU is
  installed on x86_64 only; on aarch64 VMs run on Cloud Hypervisor.
- systemd, for the services. Without it the installer still builds and
  installs the binaries, and you start them by hand.
- A package manager the installer knows: `apt-get`, `dnf`, `yum` or
  `pacman`.
- `sudo` rights. Run the installer as your normal user; it asks for your
  sudo password when it first needs root.
- `git` and a Rust toolchain (`cargo`) to build and run the installer
  itself. The installer keeps Rust up to date with `rustup` afterwards.
- Internet access to the distribution's package mirrors and to GitHub
  (Cloud Hypervisor, its firmware, Bun). Rustup, Bun and the Cloud
  Hypervisor firmware are checked against a pinned SHA-256 before use;
  the Cloud Hypervisor binary is fetched over HTTPS from its pinned
  release, without a checksum.
  Once running, Glidex itself also connects out for image downloads,
  logins and, if you use them, directory sync and kernel extraction:
  see [Outbound connections](configuration.md#outbound-connections).

For [LVM-thin datastores](datastores.md#lvm-thin) only: LVM with
thin-pool support (`lvm2` and the thin-provisioning tools), and a volume
group with a thin pool that you make. The installer installs neither.

If `/dev/kvm` is missing, the installer says how to enable it (CPU
virtualization in the firmware setup, then `modprobe kvm_intel` or
`kvm_amd`) and carries on; no VM can start until it is there.

## Install

```bash
git clone https://github.com/CMKL-APEX/glidex.git && cd glidex
cargo run -p glidex-install
```

The installer prints its plan, then:

1. Installs or updates system packages: build tools, OpenSSL headers,
   the disk and cloud-init tools (`qemu-img`, `sgdisk`, `growpart`,
   `mkdosfs`, `mtools`), QEMU and OVMF, `setcap`, and OVN and
   `conntrack` for cluster networks.
2. Installs Rust (if missing) and a pinned Bun release, then builds the
   Glidex binaries and the web UI. With `--prebuilt` it skips this step.
3. Creates the service users and groups (see [Users and
   groups](#users-and-groups)).
4. Installs a pinned Cloud Hypervisor release, [Pylon](#pylon) when
   asked for or found, the Glidex binaries and the web UI, and the
   UEFI firmware.
5. Sets up VM networking: Open vSwitch, `glidex-netd` and the boot
   devices, if any (see [Host networking](host-networking.md)).
6. Writes the systemd units and configuration files, then starts or
   restarts the services.
7. Joins a cluster (with `--join`), registers datastores, and on a
   cluster member brings OVN up to the cluster's version (see [OVN on
   cluster members](#ovn-on-cluster-members)).

At the end it prints where the web UI and the API are, and the SHA-256
fingerprints of their certificates:

```text
     web UI:  https://<host>:5173
     API:     /run/glidex-cp/api.sock (members of glidex-users), https://<host>:8841 (tokens)
     web UI certificate SHA-256: <fingerprint>
     API certificate SHA-256: <fingerprint>
```

Compare these with what your browser shows before you accept the
certificate warning (see [TLS certificates](tls.md)). With
`--tailscale-cert`, the summary also gives the
`https://<node>.<tailnet>.ts.net:5173` address, and says whether the
UI already serves the Tailscale certificate there.

The installer added you to `glidex-users` and `glidex-admin`. Log out and
back in so your session has the new groups, then check:

```bash
systemctl status glidex-control-plane glidex-ui glidex-authd.socket
gxctl whoami
```

The installer does not change the firewall. Open what remote clients
need: TCP 5173 for the web UI, 8841 for the API (remote `gxctl` and API
clients), and 8842 between the nodes of a cluster.

## Options

Choices are saved in `/etc/glidex/install.conf` and reused by later runs;
a flag on the command line overrides the saved choice. Pass options after
`--`:

```bash
cargo run -p glidex-install -- --no-qemu --ovs-profile dpdk
```

| Option | Effect |
|---|---|
| `--no-qemu` | Skip QEMU and OVMF (Cloud Hypervisor only). |
| `--no-networking` | Skip Open vSwitch, `glidex-netd` and the host network settings. VMs then have no NICs. |
| `--no-services` | Don't install the systemd units, users and groups. You run `glidex-control-plane` and `glidex-ui` yourself. |
| `--ovs-profile kernel\|dpdk` | Open vSwitch datapath. `kernel` (default) needs nothing special, and turns off DPDK an earlier `dpdk` profile left on; `dpdk` reserves hugepages and PMD cores. See [Back to the kernel profile](host-networking.md#back-to-the-kernel-profile). |
| `--pmd-cpu-mask MASK` | OVS-DPDK PMD CPU mask, in hex. `auto` clears it. |
| `--allow-ovs-restart` | Allow restarting `ovs-vswitchd` while it has bridges (interrupts their traffic), also to turn DPDK off under the kernel profile. Not saved. |
| `--release-hugepages` | Kernel profile only: give back the hugepages an earlier `dpdk` profile reserved, once DPDK is off. Kept by default. Not saved. |
| `--no-ovn` | Skip OVN. Cluster networks need it; by default it is installed with its services off until the host is in a cluster. |
| `--tailscale-cert` | Serve the web UI with this node's Tailscale certificate, trusted by browsers at `https://<node>.<tailnet>.ts.net:5173`. Grants `glidex-ui` `TS_PERMIT_CERT_UID` in a `tailscaled` drop-in and restarts `tailscaled` once; refused, changing nothing, when another user already holds it or there is no `tailscaled`. Off unless chosen: an interactive run on a node with HTTPS certificates asks once. See [Tailscale certificates](tls.md#tailscale-certificates). |
| `--no-tailscale-cert` | Turn it off: removes the drop-ins the installer wrote (a grant you set by hand stays) and restarts `tailscaled` if it removed its grant. |
| `--with-pylon` (or `--pylon`) | Install the [Pylon](../user-guide/vms.md#elastic-cpu) hypervisor and its `pylond` core arbiter. You build them first from the `CMKL-APEX/pylon` repository, checked out next to this one (`cargo build --release -p cloud-hypervisor -p pylond`): the installer takes `target/release/pylon` (or `cloud-hypervisor`) and `pylond` from there, or from this checkout's `target/release`. Without the option, they are installed when found there. Refused when asked for and not found. See [Pylon](#pylon). |
| `--no-pylon` | Skip Pylon and `pylond`. |
| `--qemu`, `--networking`, `--services`, `--ovn` | Undo an earlier `--no-…`. |
| `--datastore NAME=PATH` | Register `PATH` (a directory under `/srv/glidex`, mounted on every node) as shared datastore `NAME`. Repeatable. Not saved. See [Registering at install time](datastores.md#registering-at-install-time). |
| `--local-datastore NAME=PATH` | Register `PATH` as a node-local datastore `NAME`. Repeatable. Not saved. |
| `--datastore-default` | Make the first of these datastores the default for new disks. Not saved. |
| `--stord-allow-vg VG` | Let `glidex-stord` make LVM-thin volumes in volume group `VG`: adds it to `allowed_vgs` in `/etc/glidex/stord.json` (never removes one) and restarts `glidex-stord`. Repeatable. Not saved; the file keeps the list. You make the volume group and its thin pool. See [LVM-thin](datastores.md#lvm-thin). |
| `--vfio-devices LIST` | Bind these to `vfio-pci` at every boot, before glidex and other hypervisors start: pools of `control-plane.json` or PCI addresses, comma-separated, each with its IOMMU group. A device already on `vfio-pci` is left alone. Recorded and enabled only (`glidex-vfio-boot.service`, `/etc/glidex/vfio-devices.json`, `/etc/modules-load.d/glidex-vfio.conf`); applied at the next boot. Not with `--no-networking`. See [Keep it across reboots](pci-passthrough.md#keep-it-across-reboots). |
| `--sriov PF=N` | Give physical function `PF` `N` SR-IOV VFs at every boot, before the binding. Repeatable or comma-separated; the last count for a PF wins. |
| `--no-vfio-devices` | Stop both at boot, and remove the unit and its files (what is bound now stays so until the next boot). |
| `--now` | Apply `--vfio-devices` and `--sriov` now as well, not only at the next boot. Never touches a device on `vfio-pci`, nor the VF count of a PF with a VF on `vfio-pci`. Not saved. |
| `--prebuilt` | Install the binaries already in `target/release` (or `$CARGO_TARGET_DIR/release`) and the built UI in `crates/glidex-ui/ui/dist` (built on another host with the same OS) instead of installing Rust and Bun and building. Refused, naming what is missing, unless every binary and the UI are there. Not saved. Installing one build on several servers keeps their UI files the same, which a load balancer needs (see [Behind a load balancer or tunnel](clusters.md#behind-a-load-balancer-or-tunnel)). |
| `-h`, `--help` | Show the options. |

`/etc/glidex/install.conf` holds `qemu`, `networking`, `services`,
`ovs_profile`, `pmd_cpu_mask`, `ovn`, and, once chosen,
`tailscale_cert`, `with_pylon`, `vfio_devices` and `sriov`. The options
marked *not saved* above apply to the run they are given on only.

Without `--datastore` or `--local-datastore`, an interactive run
offers each filesystem mounted directly under `/srv/glidex` that isn't
a datastore yet: register it as a shared datastore named after its
directory (default no), and make it the default for new disks (default
no). Datastores are registered only when the services are installed
(not with `--no-services`); if that fails, the installer warns and goes
on.

Options for joining or leaving a cluster (`--join`, `--role`,
`--token-file`, `--advertise`, `--node-name`, `--import`, `--rejoin`,
`--node-id`, `--leave`, `--keep-resources --offline`) are described in
[Clusters](clusters.md). None of them is saved.

### OVN on cluster members

A member's OVN must be at least as new as the cluster's `ovn-northd`:
an older `ovn-controller` can't run the flows a newer northd compiles,
and VMs on that node lose their traffic on cluster networks. On a
cluster member with OVN on, the installer checks this after a join and
on every re-run:

1. It reads northd's version from the cluster's southbound database,
   with the node's own certificate. Until the cluster's OVN is on here,
   it says so and skips the check; re-run the installer later.
2. If the local `ovn-controller` is older, on `apt-get` hosts it
   upgrades `ovn-host` (and `ovn-common`, `ovn-central` where
   installed): from the distribution when it has a new enough version,
   else, on Ubuntu, from the Ubuntu Cloud Archive pocket that has one.
   For the Cloud Archive it installs `ubuntu-cloud-keyring` and writes
   `/etc/apt/sources.list.d/glidex-uca-ovn.list` and
   `/etc/apt/preferences.d/glidex-uca-ovn`, pinned so that only OVN
   comes from there.
3. It refuses to upgrade when the upgrade would remove another package
   (a vendor's OVS, say), and on other package managers it asks you to
   install the right OVN version yourself. Both stop the installer with
   the version needed.

An agent installs `ovn-host` only; servers and standalone hosts also get
`ovn-central`.

## What gets installed

| Path | What |
|---|---|
| `/usr/local/bin/glidex-control-plane` | The control plane (API server). |
| `/usr/local/bin/gxctl` | The CLI. |
| `/usr/local/bin/glidex-ui` | The web UI server. |
| `/usr/local/bin/glidex-vm-shim` | Supervises one VM's hypervisor. |
| `/usr/local/bin/glidex-netd` | The host networking helper (unless `--no-networking`). |
| `/usr/local/bin/glidex-authd` | The PAM helper for local passwords (unless `--no-services`). |
| `/usr/local/bin/glidex-stord` | The storage helper for LVM-thin datastores (unless `--no-services`). |
| `/usr/local/bin/glidex-metad` | The metadata endpoint for guests (unless `--no-networking` or `--no-services`). |
| `/usr/local/bin/cloud-hypervisor` | The pinned Cloud Hypervisor. |
| `/usr/local/bin/pylon`, `/usr/local/bin/pylond` | Pylon and its core arbiter, with `--with-pylon`. |
| `/usr/local/share/glidex/ui/` | The built web UI. |
| `/etc/systemd/system/glidex-*.service`, `glidex-authd.socket`, `glidex-vm@.service`, `glidex-vms.slice` | The systemd units (see [Operator guide](README.md#components)). |
| `/etc/systemd/system/glidex-ui.service.d/50-tailscale-cert.conf`, `/etc/systemd/system/tailscaled.service.d/50-glidex-cert.conf` | With `--tailscale-cert`: the drop-ins that turn the UI's Tailscale certificate on and let `glidex-ui` fetch it. |
| `/etc/polkit-1/rules.d/50-glidex-vm.rules` | Lets the `glidex` user start and stop its own `glidex-vm@` units. |
| `/etc/glidex/control-plane.json.example` | Every control plane setting with its default. The installer never writes `control-plane.json` itself. |
| `/etc/glidex/authd.json` | `glidex-authd` settings, written once and then left alone. |
| `/etc/glidex/stord.json` | The volume groups `glidex-stord` may use (`allowed_vgs`). Written only with `--stord-allow-vg`; without it stord allows none. Yours to edit; kept on uninstall like your other files there. |
| `/etc/glidex/vm-shim.json` | The hypervisor binaries VMs may run. |
| `/etc/glidex/policies/` | Site policy files (`root:glidex`, mode 0750), empty. |
| `/etc/glidex/install.conf` | The installer's saved choices. |
| `/etc/pam.d/glidex` | The PAM service for web logins with local passwords. |
| `/etc/sysctl.d/90-glidex.conf` | `net.ipv4.ip_forward=1` for NAT networks and, with `--ovs-profile dpdk`, `vm.nr_hugepages` (kept after going back to the kernel profile unless `--release-hugepages`). The previous values are recorded for the uninstaller. |
| `/etc/modules-load.d/glidex.conf` | Loads `vfio-pci` at boot, with `--ovs-profile dpdk`. |
| `/etc/glidex/vfio-devices.json`, `/etc/modules-load.d/glidex-vfio.conf`, `/etc/systemd/system/glidex-vfio-boot.service` | With `--vfio-devices` or `--sriov`: what to bind and set at boot, and the unit that does it. |
| `/srv/glidex/` | The datastore root (`root:root`, mode 0755), empty: mount or create [datastores](datastores.md) under it. |
| `/var/lib/glidex-control-plane/` | The `glidex` user's home. Its `.glidex/` holds the database, images, disks, firmware and the self-signed certificate. |

The installer also puts a copy of the Cloud Hypervisor firmware in your
own `~/.glidex/`, for a control plane you run by hand.

## Pylon

With `--with-pylon`, the host runs `glidex-pylond.service`. It listens
on `/run/pylon/pylond.sock` (group `glidex`) and starts before the
control plane. Pylon VMs register with it, and it parks and wakes their
vCPUs (see [Elastic CPU](../user-guide/vms.md#elastic-cpu)).

- **Which CPUs it manages.** `pylond` arbitrates the host CPUs in
  `PYLON_CORES`, `2-7` by default. To change it, write
  `PYLON_CORES=<list>` (such as `PYLON_CORES=4-31`) to
  `/etc/default/glidex-pylond` and restart `glidex-pylond`.
- **CPUs it reserves.** While `pylond` runs, the control plane reserves
  the first two cores of the host for it, with their hyperthreads: no VM
  is placed on them, and `gxctl node topology` shows them as reserved
  for `pylon`. See [Host resources](host-resources.md).
- **The `sched_ext` scheduler.** On a kernel built with `sched_ext`
  (6.12 or later), `pylond` attaches Pylon's `scx_pylon` kernel
  scheduler when it starts, and falls back to its own core table if
  that fails. This needs a `pylond` built from Pylon with
  [CMKL-APEX/pylon#9](https://github.com/CMKL-APEX/pylon/pull/9); an
  earlier one never attaches it from this service. `scx_pylon` then
  schedules every task on the host, not only VMs, until `pylond` stops.
  Check it with `cat /sys/kernel/sched_ext/state /sys/kernel/sched_ext/root/ops`
  (`enabled`, `pylon`). To keep it off, start `pylond` with
  `--no-scx` in a drop-in (`sudo systemctl edit glidex-pylond`), if
  your `pylond` has that option:

  ```ini
  [Service]
  ExecStart=
  ExecStart=/usr/local/bin/pylond --socket /run/pylon/pylond.sock --cores ${PYLON_CORES} --no-scx
  ```

A host offers Pylon when `/usr/local/bin/pylon` is there; Pylon VMs also
need `pylond` running to scale. Elastic VMs run on any host with Cloud
Hypervisor or QEMU.

## Users and groups

| Name | Kind | Purpose |
|---|---|---|
| `glidex` | system user and group | Runs the control plane and the VM units. Only this user is in the `glidex` group, which opens `glidex-netd`'s, `glidex-authd`'s and `glidex-stord`'s sockets. |
| `glidex-ui` | system user and group | Runs the web UI. It may only use the control plane's UI socket. |
| `glidex-metad` | system user and group | Runs the metadata endpoint. It may only use the control plane's `metad.sock`. |
| `glidex-users` | group | People who may use `gxctl` on the local socket, `/run/glidex-cp/api.sock`, and log in to the web UI with their local password. |
| `glidex-admin` | group | Break-glass administrators: full rights on the local socket, whatever the policies say. |

The installer adds you to `glidex-users` and `glidex-admin`, and takes you
out of `glidex` if an earlier install put you there. Add other people with
`usermod`:

```bash
sudo usermod -aG glidex-users alice     # gxctl on this host, web UI login
sudo usermod -aG glidex-admin bob       # break-glass administrator
```

Keep `glidex-admin` small. On the first start, every member of
`glidex-admin` becomes a Glidex administrator (see [Access
control](access-control.md#the-first-administrators)); after that, give
people roles instead of adding them to this group.

## Updating

Pull the new source and run the installer again:

```bash
git pull
cargo run -p glidex-install
```

It updates system packages that came from its own list, Rust, Bun,
Cloud Hypervisor, the binaries and the UI, and restarts the services
whose binary or unit changed. Running VMs keep running across a control
plane restart: each runs in its own `glidex-vm@<id>` unit. A running VM
keeps the Cloud Hypervisor binary it started with until it is restarted.
Reload web UI tabs that were open before the update to get the new UI.

With the kernel profile saved, the installer also turns off DPDK it
finds on in `ovs-vswitchd`, for example after `gxctl ovs dpdk-init` or
a profile switch in the web UI, which don't change the saved profile.
With bridges it only says so unless you pass `--allow-ovs-restart`. To
keep DPDK, save the profile once with `--ovs-profile dpdk`.

On a cluster member, a re-run also brings OVN up to the cluster's
`ovn-northd` (see [OVN on cluster members](#ovn-on-cluster-members)).

If something else listens on port 8841 or 5173 (a control plane or UI you
started by hand), the installer leaves the service stopped and tells you
how to start it once the port is free.

## Uninstall

See what would be removed, then remove it:

```bash
cargo run -p glidex-install -- uninstall --dry-run
cargo run -p glidex-install -- uninstall
```

The uninstaller stops every VM (each gets its power button and grace
period), the services and `glidex-netd`; tears down Glidex host
networking (uplinks give the host's address back to its NIC, NAT,
bridges and VM ports are removed); then removes the units, binaries,
installer-written configuration, runtime directories, the users and the
groups. It also removes the boot devices' unit and files (devices stay
on `vfio-pci` until the next boot). Kernel settings the installer
changed go back to their previous values. With `--tailscale-cert` on, it also removes the installer's
`tailscaled` drop-in and restarts `tailscaled`; a `TS_PERMIT_CERT_UID`
you set yourself stays.

By default it keeps your data and site configuration, and says so:

- `/var/lib/glidex-control-plane` and `~/.glidex` (VM database,
  credentials, images, disks, firmware);
- your files in `/etc/glidex` (`control-plane.json`, `netd.json`,
  policies, certificates);
- a locally edited `/etc/pam.d/glidex`;
- Open vSwitch, its DPDK settings and the hugepages they need, and the
  `cloud-hypervisor`, `pylon` and `pylond` binaries.

It never touches `/srv/glidex` and the datastores under it, nor the
volume groups of LVM-thin datastores and the volumes in them, not even
with `--purge-user-data`: unmount and remove them yourself.

| Option | Effect |
|---|---|
| `-n`, `--dry-run` | Show the plan and change nothing. |
| `-y`, `--yes` | Don't ask for confirmation. |
| `--remove-ovs` | Also remove Open vSwitch: the `openvswitch-switch` / `openvswitch-switch-dpdk` packages (via `apt-get`), or the source build in `/opt/glidex`. |
| `--reset-dpdk` | Clear the DPDK settings Glidex put in Open vSwitch. |
| `--remove-cloud-hypervisor` | Also delete the `cloud-hypervisor` binary. |
| `--remove-pylon` | Also delete the `pylon` and `pylond` binaries. |
| `--purge-user-data` | Also delete `~/.glidex`, `/var/lib/glidex-control-plane` (every VM, disk, image and credential) and the site configuration in `/etc/glidex`. This can't be undone. |

Take a host out of its cluster before you uninstall it (see
[Clusters](clusters.md)).
