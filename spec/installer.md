# `glidex-install` bootstrapper

Source: `crates/glidex-install/src/main.rs`. Run it with
`cargo run -p glidex-install` from the repo root. It replaces the
old `install.sh` shell script.

## Philosophy

This is the one place where "check if something is missing and fix
it" logic lives. Other crates assume their dependencies are present.
The installer is a **linear** script — no plugins, no reusable
library, one file of top-to-bottom procedural code.

It is **non-interactive and convergent**: every run detects what is
missing or outdated and fixes just that, so the same command installs
glidex and later keeps it up to date. The only prompt is sudo's
password, asked once (`sudo -v`, then refreshed in the background so a
long build doesn't make it ask again) and only when a step actually
needs root. Each root step first checks whether it is needed (files
compared before installing, users/groups/units/capabilities checked
before creating), so a re-run with nothing to do changes nothing.

## Options

Defaults install everything. Flags opt out; all but
`--allow-ovs-restart` are saved to `/etc/glidex/install.conf` and reused
by later runs (flags override the saved values):

| Flag | Effect |
|---|---|
| `--no-qemu` / `--qemu` | skip / include QEMU and OVMF |
| `--no-networking` / `--networking` | skip / include OVS, glidex-netd and host settings |
| `--no-services` / `--services` | skip / include the `glidex` user and the systemd units |
| `--ovs-profile dpdk\|kernel` | OVS flavour (default `dpdk`, which reserves hugepages) |
| `--pmd-cpu-mask MASK` | OVS-DPDK PMD CPU mask (`auto` clears it) |
| `--allow-ovs-restart` | allow restarting an ovs-vswitchd that has bridges (interrupts their traffic) |

## Inputs it discovers

- `std::env::consts::OS` — rejects anything other than `linux`
  because none of the hypervisors target other OSes.
- `std::env::consts::ARCH` — accepts `x86_64` and `aarch64`,
  rejects the rest (QEMU support is x86_64 only).
- The package manager (`apt-get | dnf | yum | pacman`), and per
  dependency whether it is present (command, `pkg-config` module or
  file), installed from its package, and upgradable.
- `cloud-hypervisor --version` of `/usr/local/bin/cloud-hypervisor`,
  the firmware's sha256, `getcap` on cloud-hypervisor.
- The `glidex` user and group, unit states (`systemctl is-active` /
  `is-enabled`), and running VMs (`GET /vms` on the local API).

## Steps, in order

1. **System packages** — one transaction for every dependency
   (`BASE_DEPS`, `QEMU_DEPS`, `NETWORKING_DEPS`; each maps to its apt,
   dnf/yum and pacman package): `curl`; `unzip` (bun's installer);
   `pkg-config` and the OpenSSL headers (`openssl-sys`); `clang`;
   `dosfstools` + `mtools` for cloud-init seeds; `qemu-img`, `qemu-io`,
   `sgdisk`, `growpart` for images and disks (spec/images.md §9);
   QEMU with OVMF (`qemu-system-x86 ovmf` on apt — there is no
   `qemu-kvm` package there any more — `qemu-kvm edk2-ovmf` on dnf/yum,
   `qemu-base edk2-ovmf` on pacman); `setcap` (`libcap2-bin` /
   `libcap`) for networking.
   `package_plan` splits them into *missing* (install) and *present and
   installed from that package* (keep up to date); a tool present but
   installed some other way is left alone. Upgrades are detected without
   root (`apt-get -s install`, `dnf check-update`, `pacman -Qu`), so
   sudo is only needed when something will change. Then: `apt-get
   update` + `apt-get install -y` (installs and upgrades), `dnf install`
   + `dnf upgrade`, or `pacman -S --needed`.
   Open vSwitch is not upgraded here: an OVS upgrade restarts
   ovs-vswitchd, which interrupts VM traffic, so it is left to the
   distro's normal updates.
2. **Rust** — installed with rustup if missing; `rustup update stable`
   on every run (which also updates rustup). The build then uses
   `~/.cargo/bin/cargo` if `cargo` isn't on PATH yet.
3. **Bun** — installed from bun.sh if missing; `bun upgrade` when it
   lives in `~/.bun` (other installs are left to their installer).
4. **KVM check** — reports a missing `/dev/kvm`. The services get
   access through the `kvm` group; the invoking user only needs it for
   running VMs by hand, which is printed, not changed.
5. **Build** — `cargo build --release -p glidex-control-plane
   -p glidex-netd -p glidex-ui`, then the UI: `bun install
   --frozen-lockfile` and `bun run build` in `crates/glidex-ui/ui`.
6. **Service user** *(with services)* — the `glidex` system group and
   user (primary group `glidex`, home `/var/lib/glidex-control-plane`,
   no login shell), the home and its `.glidex` data dir (0750), and the
   invoking user added to the `glidex` group (`gxctl connect` to console
   sockets, netd). A home kept from an earlier install whose user had
   another uid is chowned back.
7. **Cloud-Hypervisor** — the static binary at the version pinned in
   `CLOUD_HYPERVISOR_VERSION` (currently `v53.0`), installed to
   `/usr/local/bin` when that copy's version differs. Running VMs keep
   their process, so no restart follows.
8. **Binaries** — `glidex-control-plane`, `gxctl`, `glidex-ui` and
   (with networking) `glidex-netd` to `/usr/local/bin`, each only when it
   differs from the build. Copies an earlier installer put in
   `~/.local/bin` are removed so they don't shadow these.
9. **Web UI files** — `ui/dist` swapped into
   `/usr/local/share/glidex/ui` when it differs (no restart needed:
   files are read per request).
10. **UEFI firmware** — see below; into the invoking user's
    `~/.glidex` and, with services, the service user's.
11. **VM networking** *(unless `--no-networking`)* — installs Open
    vSwitch through `glidex-ovs` (`dpdk` profile by default, or `kernel`,
    via sudo; this also pulls in `dnsmasq` and `nftables`). If that
    would restart a vswitchd that has bridges, it is skipped with a note
    unless `--allow-ovs-restart`. Then the host settings below, then
    `glidex-netd` (root: it is the privileged helper) with
    `packaging/glidex-netd.service`, enabled and restarted only when its
    binary or unit changed (or it isn't running). Finally
    `cap_net_admin+ep` on `/usr/local/bin/cloud-hypervisor` for tap
    devices, re-applied whenever the binary was replaced.
    See [networking.md](networking.md) §13.

    **Host settings** (no prompts):
    - `net.ipv4.ip_forward=1` for NAT networks.
    - *dpdk profile:* `vm.nr_hugepages` — 2048 × 2 MiB (4 GiB: 2 GiB of
      DPDK socket memory + 2 GiB for hugepage-backed vhost-user guests),
      capped at a quarter of RAM and never lowered below what's already
      reserved. Hosts that can't reach 1024 pages skip DPDK setup. The
      installer reads back what the kernel actually reserved (fragmented
      memory may give fewer; a reboot applies the persisted value).
    - *dpdk profile:* `vfio-pci` loaded now and at boot
      (`/etc/modules-load.d/glidex.conf`), for DPDK NIC uplinks; a missing
      IOMMU (`intel_iommu=on` / `amd_iommu=on`) is reported, not fixed.
    - *dpdk profile:* OVS-DPDK init (`dpdk-init`, `dpdk-socket-mem` =
      min(pool/2, 2048) MiB, `--pmd-cpu-mask`); a no-op when already
      initialized that way. Restarting a vswitchd that has bridges needs
      `--allow-ovs-restart`.

    Sysctls persist in `/etc/sysctl.d/90-glidex.conf` and are applied with
    `sysctl -p` (only when the file or a live value differs). Each setting is preceded by
    `# glidex-previous: key=value`, the value before glidex changed it.
    Re-runs keep the recorded original, so the uninstaller restores the
    pre-glidex value rather than glidex's own. Firewall managers are not
    touched: ufw or firewalld dropping forwarded traffic must allow the NAT
    subnets. A plain iptables FORWARD DROP (e.g. Docker's) is handled by
    netd's `GLIDEX-FORWARD` chain (networking.md §10).
12. **Services** *(unless `--no-services`; skipped without systemd)* —
    renders `packaging/glidex-control-plane.service.in` for the `glidex`
    user and installs `packaging/glidex-ui.service` (each only when
    changed, then `daemon-reload`), enables both, and starts them:
    - the control plane is started if it isn't running, and restarted
      after an update **only when no VM is running or paused** (stopping
      it stops them); otherwise the installer says to restart it later.
      If something else already listens on `127.0.0.1:8841` (a control
      plane started by hand), it isn't started.
    - the UI is (re)started whenever its binary or unit changed or it
      isn't running.
    If the previous unit ran the control plane as the invoking user, a
    note says that their `~/.glidex` data was not moved.
13. **Save options** to `/etc/glidex/install.conf` when they changed,
    then the usage blurb.

## Boot sequence (systemd)

```
network-online.target
  → Open vSwitch (openvswitch-switch / openvswitch / glidex-ovs-vswitchd)
  → glidex-netd.service            root, Type=notify
  → glidex-control-plane.service   user glidex
  → glidex-ui.service              user glidex
```

- **glidex-netd** is where host networking is initialized at boot: its
  startup reconciliation re-creates glidex bridges, re-binds DPDK NICs to
  `vfio-pci`, re-applies IP migrations and restores NAT (address,
  nftables, dnsmasq). It sends `READY=1` (`sd_notify`, no libsystemd)
  only after that and after binding both sockets, so the control plane
  never races it. `Wants=` lists every OVS unit name; missing ones are
  ignored. It is the one glidex unit that runs as root: it exists so
  nothing else has to.
- **glidex-control-plane** runs as `glidex` (`User=`, `Group=glidex`,
  which also opens netd's socket), with `HOME` and
  `WorkingDirectory=/var/lib/glidex-control-plane`, so its database and
  firmware are in `/var/lib/glidex-control-plane/.glidex`.
  `SupplementaryGroups=kvm` (only if the group exists, since systemd
  refuses a unit naming a missing group). `UMask=0007` makes console
  sockets and logs usable by the `glidex` group (`gxctl connect`). It
  `Wants=` netd but doesn't require it: without networking, VMs without
  NICs still work. Stopping the unit stops running VMs (the control
  plane's shutdown path). It listens on loopback; expose it only via a
  drop-in setting `GLIDEX_LISTEN`, since the API is unauthenticated.
  Paths given to the API (disk images, kernels, seeds) must be readable
  by `glidex`.
- **glidex-ui** runs as `glidex`, serves `/usr/local/share/glidex/ui`
  on `127.0.0.1:5173` and proxies `/api` to the control plane (see
  [web-ui.md](web-ui.md)). It is sandboxed (`ProtectSystem=strict`,
  `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `NoNewPrivileges`).
  Exposing it (`GLIDEX_UI_LISTEN` in a drop-in) exposes the API.
- Validate with `systemd-analyze verify`; the installer's tests check
  the rendered units and netd's ordering.

## Uninstall

`cargo run -p glidex-install -- uninstall [--dry-run] [--yes]
[--remove-ovs] [--reset-dpdk] [--remove-cloud-hypervisor]
[--purge-user-data]` (source: `crates/glidex-install/src/uninstall.rs`).

It prints a plan first (`--dry-run` stops there; otherwise it asks unless
`--yes`) and re-runs itself through `sudo` when not root. The plan is
built by a pure function over a `HostView`, so tests pin exactly what is
removed. Order:

1. `systemctl disable --now` the UI, the control plane (its shutdown
   stops VMs) and glidex-netd.
2. **Network teardown with netd's own code**, in-process on netd's state
   database: release VM ports, delete uplinks (moving migrated IPs back
   onto their NICs and restoring NIC drivers after DPDK), delete NAT
   networks and bridges. Then stop netd's dnsmasq processes and drop the
   `inet glidex` nftables table.
   **Invariant:** if the teardown fails (e.g. netd still running and
   holding the database), it stops before deleting netd's state, which is
   the only record of how to undo those host changes.
3. `setcap -r` on `cloud-hypervisor` (or remove it with
   `--remove-cloud-hypervisor`); remove `glidex-control-plane`, `gxctl`,
   `glidex-netd`, `glidex-ui` from `/usr/local/bin` and `~/.local/bin`.
4. Remove the glidex unit files and `daemon-reload`.
5. Optional: `--reset-dpdk` clears `dpdk-init`, `dpdk-socket-mem`,
   `pmd-cpu-mask` from OVS; `--remove-ovs` disables and removes the source
   build (units + `/opt/glidex`) and removes the distro OVS packages
   (`dnsmasq-base` and `nftables` are kept: they are shared).
6. Remove `/etc/glidex`, `/var/lib/glidex`, `/run/glidex`, the UI files
   in `/usr/local/share/glidex`, leftover
   `/tmp/cloud-hypervisor-*.cloudinit.img`, and the `glidex` user and
   group.
7. Host settings from `/etc/sysctl.d/90-glidex.conf`: each goes back to
   its recorded previous value (`sysctl -w`), then the drop-in is removed.
   While OVS keeps its DPDK configuration (no `--reset-dpdk` /
   `--remove-ovs`), `vm.nr_hugepages` and `vfio-pci` stay — OVS-DPDK
   would fail to start without them — and the drop-in is rewritten to
   hold just the hugepage setting.
8. `/var/lib/glidex-control-plane` (the service user's home: VM
   database, credentials, images, disks, firmware) and `~/.glidex` are
   kept unless `--purge-user-data`. A later install re-owns a kept home
   for the recreated user.

Not reverted: `kvm` group membership, and settings with no recorded
previous value (left as they are, with a note).

## Binary installation helper

`install_if_changed(src, dst, mode)` compares the two files and, only
if they differ, runs `install -D -m <mode> -o root -g root src dst`
through sudo; `write_if_changed` does the same for generated text (the
rendered unit). Everything goes to `/usr/local/bin`, even for a
non-root run: the services' user can't read a home directory's
`~/.local/bin`.

## UEFI firmware

Firmware boot (see [hypervisors.md](hypervisors.md#firmware-boot)) needs
Cloud-Hypervisor's EDK2 build. It is downloaded rather than committed
to the repo, to keep a 4 MB binary out of git history.

- **Pinned, not `latest`.** The edk2 fork publishes a release per
  build (`ch-<commit>`), several a month. Pinning keeps installs
  reproducible and ties the version to one that has been boot-tested;
  bump `EDK2_FIRMWARE_VERSION` and the digests in `firmware_asset`
  together.
- **Verified.** The file is downloaded to `<name>.part` next to the
  destination, checked against the sha256 in `firmware_asset` (the
  digests GitHub publishes for the release assets), then renamed into
  place. A mismatch deletes the partial file and aborts.
- **Idempotent.** If the destination already has the expected sha256
  the download is skipped; if it differs (an older pin, or a
  hand-placed build) it is replaced.
- **Two copies.** The invoking user's `~/.glidex` (gxctl's default,
  runs by hand, the tests) and, with services,
  `/var/lib/glidex-control-plane/.glidex` (owned by `glidex`), copied
  from the first when its sha256 differs.
- **Consumers.** `hypervisor::cloud_hypervisor::default_firmware_path()`
  returns the same path; `gxctl create` offers it as the default (and,
  when the default is taken for an image or managed disk, leaves
  `firmware_path` unset so the server uses its own copy),
  the control plane reports whether it exists at startup, and the
  end-to-end functional test uses it unless `GLIDEX_TEST_FIRMWARE`
  is set. The installer can't depend on the control-plane crate, so
  the file name is duplicated there.

## No sample kernel / rootfs

Earlier versions downloaded a kernel and built an ext4 rootfs from
Firecracker's CI artifacts. That was removed together with Firecracker
support. The out-of-the-box path is now firmware boot: the installer
provides `CLOUDHV.fd`, and `gxctl image pull ubuntu-26.04` (or any catalog
key; see [images.md](images.md#4-image-catalog)) fetches a verified cloud
image at run time, not install time, so it is always the current build.
It boots with an auto-generated cloud-init seed. Kernel boot still works
but needs a user-supplied kernel + rootfs.

## What it deliberately does not do

- **Supervises only glidex**: units for glidex-netd, the control plane
  and the UI (none with `--no-services` / `--no-networking`).
- **Does not configure networking itself**: bridges, NAT and uplinks
  are created later through glidex-netd, never by the installer.
- **Does not touch the ReDB file**. If one exists from a previous
  run it is left alone.
- **Modifies `/etc` only through files it owns**: the systemd units,
  `/etc/sysctl.d/90-glidex.conf`, `/etc/modules-load.d/glidex.conf`,
  `/etc/glidex/install.conf` (plus what package installs and
  `useradd`/`usermod` do). The uninstaller removes them.
- **Does not move data** from an earlier per-user install's `~/.glidex`
  to the service user's.

## Dependencies

Intentionally minimal:

```toml
anyhow = "1"
colored = "3"
dirs = "6"
libc = "0.2"
tempfile = "3"
```

Downloads are done by shelling to `curl`, not by pulling in a HTTP
stack. The installer itself is therefore small and fast to build.
