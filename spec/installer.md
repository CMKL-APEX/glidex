# `glidex-install` bootstrapper

Source: `crates/glidex-install/src/main.rs`. Run it with
`cargo run -p glidex-install` from the repo root. It replaces the
old `install.sh` shell script.

## Philosophy

This is the one place where "check if something is missing and fix
it" logic lives. Other crates assume their dependencies are present.
The installer is a **linear** script — no plugins, no reusable
library, one file of top-to-bottom procedural code — because its
only invocation mode is interactive bootstrap.

## Inputs it discovers

- `std::env::consts::OS` — rejects anything other than `linux`
  because none of the hypervisors target other OSes.
- `std::env::consts::ARCH` — accepts `x86_64` and `aarch64`,
  rejects the rest.
- `libc::geteuid()` — if root, installs binaries to `/usr/local/bin`;
  otherwise to `~/.local/bin` (created if missing).
- Presence of `curl`, `apt-get | dnf | yum | pacman`, `bun`,
  `rustup`, `mkdosfs`, `mcopy`, `cloud-hypervisor`,
  `qemu-system-x86_64`.

All detection goes through a `command_exists` helper that shells out
to `command -v`.

## Steps, in order

1. **Rust** — if `rustc` is missing, curls the rustup installer and
   runs it non-interactively. If `rustup` is present, runs
   `rustup update stable` for good measure. The installer does
   not re-exec into a new shell; a fresh `source $HOME/.cargo/env`
   is needed for subsequent shell invocations.
2. **Bun** — installed via `curl -fsSL https://bun.sh/install | bash`
   if missing. Bun is the UI's package manager and Vite runner.
3. **Cloud-Hypervisor** — downloads the arch-appropriate static
   binary from GitHub releases at the version pinned in
   `CLOUD_HYPERVISOR_VERSION` (currently `v53.0`), installs it via
   `install_binary` helper.
4. **UEFI firmware** — downloads Cloud-Hypervisor's EDK2 firmware
   from <https://github.com/cloud-hypervisor/edk2/releases> at the tag
   pinned in `EDK2_FIRMWARE_VERSION` (currently `ch-811ce5ea35`) into
   `~/.glidex/CLOUDHV.fd` (`CLOUDHV_EFI.fd` on aarch64), then ensures
   `dosfstools` + `mtools` are installed for cloud-init seed images.
   See below.
5. **QEMU** *(optional, prompts)* — delegates to the system
   package manager (`apt-get install qemu-system-x86 qemu-kvm`,
   `dnf install qemu-kvm`, etc). We don't ship a QEMU binary
   because its distribution story is already well handled by
   every Linux distro and the resulting tree is large.
6. **KVM access check** — verifies `/dev/kvm` exists and is
   read-writable. On failure it offers `usermod -aG kvm $USER` (when the
   `kvm` group exists), or prints the instruction if declined.
7. **Build** — runs `cargo build --release -p glidex-control-plane
   -p glidex-netd` and offers to install `glidex-control-plane` and
   `gxctl` into the install dir.
8. **VM networking** *(optional, prompts)* — creates the `glidex` group
   (adds the invoking user), installs Open vSwitch through
   `glidex-ovs` (`dpdk` profile by default, or `kernel`, via sudo; this also pulls
   in `dnsmasq` and `nftables`), applies the host settings below,
   installs `glidex-netd` to `/usr/local/bin` with
   `packaging/glidex-netd.service`, and offers `cap_net_admin+ep` on
   `cloud-hypervisor` for tap devices (installing `setcap` —
   `libcap2-bin` / `libcap` — if missing).
   See [networking.md](networking.md) §13.

   **Host settings** (each prompts):
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
     min(pool/2, 2048) MiB, optional `pmd-cpu-mask`), confirming before
     restarting a vswitchd that already has bridges.

   Sysctls persist in `/etc/sysctl.d/90-glidex.conf` and are applied with
   `sysctl -p`. Each setting is preceded by
   `# glidex-previous: key=value`, the value before glidex changed it.
   Re-runs keep the recorded original, so the uninstaller restores the
   pre-glidex value rather than glidex's own. Firewalls are not touched:
   netd's nftables table only adds masquerade, so a host firewall that
   drops forwarded traffic must allow the NAT subnets.
9. **Start at boot** *(optional, prompts; skipped without systemd)* —
   renders `packaging/glidex-control-plane.service.in` for the invoking
   user (§ below), installs it (and refreshes netd's unit), and enables
   both; offers to start the control plane now.
10. **UI dependencies** — runs `bun install` inside
   `crates/glidex-ui/ui`. Skipped with a warning if bun isn't
   on PATH.
11. **Usage blurb** — prints the 1-2-3 for starting the server,
   UI, and CLI.

## Boot sequence (systemd)

```
network-online.target
  → Open vSwitch (openvswitch-switch / openvswitch / glidex-ovs-vswitchd)
  → glidex-netd.service      root, Type=notify
  → glidex-control-plane.service   the installing user
```

- **glidex-netd** is where host networking is initialized at boot: its
  startup reconciliation re-creates glidex bridges, re-binds DPDK NICs to
  `vfio-pci`, re-applies IP migrations and restores NAT (address,
  nftables, dnsmasq). It sends `READY=1` (`sd_notify`, no libsystemd)
  only after that and after binding both sockets, so the control plane
  never races it. `Wants=` lists every OVS unit name; missing ones are
  ignored.
- **glidex-control-plane** runs as the installing user (`User=`,
  `HOME`, `WorkingDirectory=`), so it uses the same `~/.glidex` database
  and firmware as an interactive run. `SupplementaryGroups=` adds `kvm`
  and `glidex` — only the ones that exist, since systemd refuses a unit
  naming a missing group — so no re-login is needed. It `Wants=` netd but
  doesn't require it: without networking, VMs without NICs still work.
  Stopping the unit stops running VMs (the control plane's shutdown path).
  It listens on loopback; expose it only via a drop-in setting
  `GLIDEX_LISTEN`, since the API is unauthenticated.
- Validate with `systemd-analyze verify`; the installer's tests check
  the rendered unit and netd's ordering.

## Uninstall

`cargo run -p glidex-install -- uninstall [--dry-run] [--yes]
[--remove-ovs] [--reset-dpdk] [--remove-cloud-hypervisor]
[--purge-user-data]` (source: `crates/glidex-install/src/uninstall.rs`).

It prints a plan first (`--dry-run` stops there; otherwise it asks unless
`--yes`) and re-runs itself through `sudo` when not root. The plan is
built by a pure function over a `HostView`, so tests pin exactly what is
removed. Order:

1. `systemctl disable --now` the control plane (its shutdown stops VMs)
   and glidex-netd.
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
   `glidex-netd` from `/usr/local/bin` and `~/.local/bin`.
4. Remove the glidex unit files and `daemon-reload`.
5. Optional: `--reset-dpdk` clears `dpdk-init`, `dpdk-socket-mem`,
   `pmd-cpu-mask` from OVS; `--remove-ovs` disables and removes the source
   build (units + `/opt/glidex`) and removes the distro OVS packages
   (`dnsmasq-base` and `nftables` are kept: they are shared).
6. Remove `/etc/glidex`, `/var/lib/glidex`, `/run/glidex`, leftover
   `/tmp/cloud-hypervisor-*.cloudinit.img`, and the `glidex` group.
7. Host settings from `/etc/sysctl.d/90-glidex.conf`: each goes back to
   its recorded previous value (`sysctl -w`), then the drop-in is removed.
   While OVS keeps its DPDK configuration (no `--reset-dpdk` /
   `--remove-ovs`), `vm.nr_hugepages` and `vfio-pci` stay — OVS-DPDK
   would fail to start without them — and the drop-in is rewritten to
   hold just the hugepage setting.
8. `~/.glidex` (VM database, credentials, firmware) is kept unless
   `--purge-user-data`.

Not reverted: `kvm` group membership, and settings with no recorded
previous value (left as they are, with a note).

## Binary installation helper

`install_binary(src, dst)` behaves differently based on privilege:

- Root: `install -m 0755 src dst`.
- Non-root: `fs::copy(src, dst)` first (works under `~/.local/bin`);
  on permission error, falls back to `sudo install -o root -g root
  -m 0755 src dst`.

This accommodates both dev setups (`~/.local/bin`) and system
installs (`/usr/local/bin`) without requiring the user to choose
up front.

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
  the download is skipped; if it differs (e.g. a hand-placed newer
  build) the user is asked before it is replaced, default no.
- **Consumers.** `hypervisor::cloud_hypervisor::default_firmware_path()`
  returns the same path; `gxctl create` offers it as the default,
  the control plane reports whether it exists at startup, and the
  end-to-end functional test uses it unless `GLIDEX_TEST_FIRMWARE`
  is set. The installer can't depend on the control-plane crate, so
  the file name is duplicated there.

## No sample kernel / rootfs

Earlier versions downloaded a kernel and built an ext4 rootfs from
Firecracker's CI artifacts. That was removed together with Firecracker
support. The out-of-the-box path is now firmware boot: the installer
provides `CLOUDHV.fd`, and any distro cloud image (converted to raw)
boots with an auto-generated cloud-init seed. Kernel boot still works
but needs a user-supplied kernel + rootfs.

## What it deliberately does not do

- **Writes systemd units only when asked** (steps 8–9), for
  glidex-netd and the control plane; nothing else is supervised.
- **Does not configure networking itself**: bridges, NAT and uplinks
  are created later through glidex-netd, never by the installer.
- **Does not touch the ReDB file**. If one exists from a previous
  run it is left alone.
- **Modifies `/etc` only through files it owns**: the systemd units,
  `/etc/sysctl.d/90-glidex.conf`, `/etc/modules-load.d/glidex.conf`
  (plus what package installs do). The uninstaller removes them.

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
