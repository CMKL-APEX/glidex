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
| `--no-services` / `--services` | skip / include the users and groups, glidex-authd, the service configuration and the systemd units |
| `--ovs-profile dpdk\|kernel` | OVS flavour (default `dpdk`, which reserves hugepages) |
| `--pmd-cpu-mask MASK` | OVS-DPDK PMD CPU mask (`auto` clears it) |
| `--allow-ovs-restart` | allow restarting an ovs-vswitchd that has bridges (interrupts their traffic) |
| `--prebuilt` | use the release binaries and web UI already built in this checkout: no Rust or Bun toolchain, no build (never saved; the nested e2e test uses it) |

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
- The `glidex`, `glidex-ui`, `glidex-users` and `glidex-admin` users and
  groups and the invoking user's memberships, unit states (`systemctl
  is-active` / `is-enabled`), and running VMs (`GET /vms` as root on
  `/run/glidex-cp/api.sock`, falling back to the loopback API of a
  control plane from before authentication).

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
2. **Rust** — if missing, `rustup-init` `RUSTUP_VERSION` (currently
   1.29.1) is downloaded from
   `static.rust-lang.org/rustup/archive/<version>/<target>/rustup-init`
   to a temporary file, checked against the sha256 pinned in
   `rustup_asset` (the `rustup-init.sha256` published next to it), and
   only then run (`-y --default-toolchain stable`). No `curl … | sh`.
   `rustup update stable` on every run (which also updates rustup). The
   build then uses `~/.cargo/bin/cargo` if `cargo` isn't on PATH yet.
3. **Bun** — the release zip `bun-linux-x64.zip` / `bun-linux-aarch64.zip`
   of `BUN_VERSION` (currently 1.4.2) from GitHub, checked against the
   sha256 in `bun_asset` (the release's `SHASUMS256.txt`), unpacked to
   `~/.bun/bin/bun` (+ `bunx`) when Bun is missing or `~/.bun` has an
   older version; a newer one is kept, and a bun installed elsewhere
   (distro, npm) is left to its installer. Bump the version and digests
   together; a mismatch deletes the download and stops the install.
4. **KVM check** — reports a missing `/dev/kvm`. The services get
   access through the `kvm` group; the invoking user only needs it for
   running VMs by hand, which is printed, not changed.
5. **Build** — `cargo build --release -p glidex-control-plane
   -p glidex-netd -p glidex-ui -p glidex-vm-shim` (and `-p glidex-authd` with services), then the UI: `bun install
   --frozen-lockfile` and `bun run build` in `crates/glidex-ui/ui`.
6. **Users and groups** *(with services; spec/security.md §4, §11)*:

   | Identity | Created as | Members / use |
   |---|---|---|
   | user + group `glidex` | `groupadd --system`, `useradd --system --gid glidex`, home `/var/lib/glidex-control-plane`, nologin | **only** the control plane: netd's full socket, authd's socket, `/dev/kvm` (via `kvm`), VFIO |
   | user `glidex-ui` | `useradd --system --user-group`, home `/nonexistent`, nologin | the web UI; uses `ui.sock` only. Not in `glidex` |
   | group `glidex-users` | `groupadd --system` | people allowed to use gxctl on `/run/glidex-cp/api.sock`, and PAM login (authd's `allowed_groups`) |
   | group `glidex-admin` | `groupadd --system` | break-glass administrators |

   The home and its `.glidex` data dir are 0750 `glidex:glidex`; a home
   kept from an earlier install whose user had another uid is chowned
   back. The invoking user is added to `glidex-users` and `glidex-admin`
   and, if an earlier install put them in `glidex`, **removed from it**
   (`gpasswd -d <user> glidex`, after the new groups are added, with a
   note): membership in `glidex` is root-level control of host
   networking through netd, and people now reach glidex through
   `api.sock`. Log out and back in for group changes to apply.
7. **Cloud-Hypervisor** — the static binary at the version pinned in
   `CLOUD_HYPERVISOR_VERSION` (currently `v53.0`), installed to
   `/usr/local/bin` when that copy's version differs. Running VMs keep
   their process, so no restart follows. A replaced `glidex-vm-shim`
   binary likewise only applies to VMs launched afterwards; running
   shims keep running, and the control plane speaks the previous shim
   protocol too (reconciliation.md §8.5).
8. **Binaries** — `glidex-control-plane`, `gxctl`, `glidex-ui`,
   `glidex-vm-shim`, (with networking) `glidex-netd` and (with services) `glidex-authd` to
   `/usr/local/bin`, each only when it
   differs from the build. Copies an earlier installer put in
   `~/.local/bin` are removed so they don't shadow these.
9. **Web UI files** — `ui/dist` swapped into
   `/usr/local/share/glidex/ui` when it differs, root-owned and
   world-readable (`u=rwX,go=rX`, parent `0755`) so `glidex-ui` can
   serve them; wrong modes are fixed even when the files are current (no
   restart needed: files are read per request).
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
    - *dpdk profile:* `vm.nr_hugepages` — 6144 × 2 MiB (12 GiB: 4 GiB for
      OVS-DPDK's mempools, which at MTU 9000 include a ~2.5 GiB jumbo pool
      next to the ~0.8 GiB MTU 1500 one, + 8 GiB for hugepage-backed
      vhost-user guests), capped at a third of RAM and never lowered below
      what's already reserved. Hosts that can't reach 1024 pages skip DPDK
      setup; below 3072 pages the installer warns that jumbo-frame
      networks may not fit. The
      installer reads back what the kernel actually reserved (fragmented
      memory may give fewer; a reboot applies the persisted value).
    - *dpdk profile:* `vfio-pci` loaded now and at boot
      (`/etc/modules-load.d/glidex.conf`), for DPDK NIC uplinks; a missing
      IOMMU (`intel_iommu=on` / `amd_iommu=on`) is reported, not fixed.
    - *dpdk profile:* OVS-DPDK init (`dpdk-init`, `dpdk-socket-mem` =
      min(pool/2, 4096) MiB, `--pmd-cpu-mask`); a no-op when already
      initialized that way. Restarting a vswitchd that has bridges needs
      `--allow-ovs-restart`.

    Sysctls persist in `/etc/sysctl.d/90-glidex.conf` and are applied with
    `sysctl -p` (only when the file or a live value differs). Each setting is preceded by
    `# glidex-previous: key=value`, the value before glidex changed it.
    Re-runs keep the recorded original, so the uninstaller restores the
    pre-glidex value rather than glidex's own. Firewall managers are not
    touched: ufw or firewalld dropping forwarded traffic must allow the NAT
    subnets. A plain iptables FORWARD DROP (e.g. Docker's) is handled by
    netd's `GLIDEX-FORWARD` chain, and an `INPUT` DROP (ufw's default-deny)
    by its `GLIDEX-INPUT` chain (networking.md §10).
12. **Configuration** *(with services)*:
    - `/etc/pam.d/glidex` from `packaging/glidex.pam` (`@include
      common-auth` / `common-account`; `auth/account include system-auth`
      where there is no `common-auth`, e.g. Fedora, Arch). Written when
      absent or when it still has the `# Managed by glidex-install` line;
      an administrator who deletes that line keeps their edits (also on
      uninstall).
    - `/etc/glidex/authd.json`, only if absent:
      `{"service_user":"glidex","allowed_groups":["glidex-users"]}`.
    - `/etc/glidex/control-plane.json.example` from
      `packaging/control-plane.json.example`: valid JSON holding the
      defaults (a control-plane test loads it with the real parser). The
      installer never writes `control-plane.json`; without it the
      defaults apply. Copy the example (`root:glidex 0640`) and edit it
      to change listeners, TLS, PAM or OIDC; secrets go in systemd
      credentials (`LoadCredential=` lines in the unit), not in the file.
    - `/etc/glidex/policies/`, `root:glidex 0750`: read-only site policy
      files (spec/security.md §7.6).
13. **Services** *(unless `--no-services`; skipped without systemd)* —
    renders `packaging/glidex-control-plane.service.in` for the `glidex`
    user and installs `packaging/glidex-ui.service`,
    `packaging/glidex-authd.socket` and `packaging/glidex-authd.service`
    (each only when changed, then one `daemon-reload`), enables
    `glidex-authd.socket`, the control plane and the UI, and starts them.
    For VMs (`install_vm_units`,
    [reconciliation.md §13](reconciliation.md#13-systemd-polkit-installer)):
    - `/etc/systemd/system/glidex-vm@.service`, rendered from
      `packaging/glidex-vm@.service.in` for the `glidex` user (`ExecStart`
      is `/usr/local/bin/glidex-vm-shim --vm %i --dir
      /run/glidex-cp/vms/%i`), and `glidex-vms.slice`. Neither has an
      `[Install]` section and neither is enabled or started: only the
      control plane starts VM units, and relaunching VMs after a host
      reboot is its job (D10). When the template changes, running
      `glidex-vm@*` units get `systemctl set-property --runtime <unit>
      IOAccounting=yes MemoryAccounting=yes` after `daemon-reload`, so
      metering sees their I/O without a restart
      ([metering.md §5.1](metering.md#51-cpu-and-memory-the-vm-cgroup)).
    - `/etc/polkit-1/rules.d/50-glidex-vm.rules` (from
      `packaging/50-glidex-vm.rules.in`): the `glidex` user may `start`,
      `stop` and `kill` units named `glidex-vm@<uuid>.service`, and
      nothing else.
    - `/etc/glidex/vm-shim.json` (`root:root 0644`): the hypervisor
      binaries the shim may run — whichever of
      `/usr/local/bin/cloud-hypervisor`, `/usr/bin/cloud-hypervisor`,
      `/usr/bin/qemu-system-<arch>`, `/usr/local/bin/qemu-system-<arch>`
      exist.

    Then:
    - `glidex-authd.socket` is started if it isn't listening, restarted
      when its unit changed; `glidex-authd.service` is `try-restart`ed
      when its binary or unit changed (otherwise the next connection
      starts it).
    - the control plane is started if it isn't running, and restarted
      after an update; running VMs keep running (they are in their own
      units). **Exception:** if the installed unit is from a release
      before detached VMs (it still carries the comment "Stopping the
      service stops running VMs", `LEGACY_CP_MARKER`), that old control
      plane kills every VM when it stops, so it is restarted only when
      no VM is running or paused; otherwise the installer says that this
      one restart stops them once and to restart it when convenient.
      If something else already listens on port 8841 (a control
      plane started by hand), it isn't started.
    - the UI is (re)started whenever its binary or unit changed or it
      isn't running.
    - the usage blurb prints the UI and API URLs
      (`https://<host name>:5173`, `https://<host name>:8841`) and the
      SHA-256 fingerprints of their self-signed certificates
      (spec/security.md §5.1.1), read once the services have started.
      The installer does not touch the host firewall.
    If the previous unit ran the control plane as the invoking user, a
    note says that their `~/.glidex` data was not moved.
14. **Save options** to `/etc/glidex/install.conf` when they changed,
    then the usage blurb.

## Boot sequence (systemd)

```
network-online.target
  → Open vSwitch (openvswitch-switch / openvswitch / glidex-ovs-vswitchd)
  → glidex-netd.service            root, Type=notify
  → glidex-authd.socket            root:glidex 0660; starts glidex-authd (root) on demand
  → glidex-control-plane.service   user glidex
       └ starts glidex-vm@<id>.service (user glidex, glidex-vms.slice) over D-Bus,
         one per VM whose desired state is running or paused
  → glidex-ui.service              user glidex-ui
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
  refuses a unit naming a missing group). It `Wants=` netd and
  `glidex-authd.socket` but requires neither: without networking, VMs
  without NICs still work; without authd, PAM login fails. Stopping,
  restarting or upgrading the unit leaves running VMs alone; on start it
  adopts them and relaunches VMs that should run (after a host reboot,
  `on_host_boot: resume`). It serves HTTPS on every address, port 8841
  (self-signed certificate unless one is configured), and listens on
  `/run/glidex-cp/api.sock` and `ui.sock`; settings are in
  `/etc/glidex/control-plane.json` (spec/security.md §5.1). Hardening (spec/security.md §9):
  `RuntimeDirectory=glidex-cp` (`0755`) with
  `RuntimeDirectoryPreserve=yes` — required, since the running VMs'
  directories (`vms/<id>/`: shim, hypervisor and console sockets,
  console log) live there —
  `UMask=0077` (per-VM files are private; consoles go through the API),
  `PrivateTmp`, `ProtectSystem=strict`, `ProtectHome=yes`,
  `ReadWritePaths=/var/lib/glidex-control-plane`, `DevicePolicy=closed`
  with no device allowed, `NoNewPrivileges=yes`,
  `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK`,
  `LockPersonality=`, `RestrictSUIDSGID=`,
  `SystemCallFilter=@system-service`, `SystemCallArchitectures=native`,
  `ProtectKernelTunables=`, `ProtectKernelModules=`,
  `ProtectKernelLogs=`, `ProtectControlGroups=`, `RestrictNamespaces=`
  and `RestrictRealtime=` (reconciliation.md §13.3). It runs no
  hypervisor (they run in the `glidex-vm@` units, which keep the
  devices and cloud-hypervisor's `cap_net_admin`), so the detached VM
  runner can't be used under it: the control plane refuses to start
  with `vm_runner` `"detached"` there. Commented `LoadCredential=`
  lines show where the TLS key, OIDC client secret and cloud-init
  password hash go. Paths given to the API (disk images, kernels,
  seeds) must be readable by `glidex` and outside `/home`, `/root` and
  `/run/user`.
- **`glidex-vm@<id>`** runs `glidex-vm-shim` for one VM as `glidex`
  (`SupplementaryGroups=kvm`), `Type=notify` (the shim sends `READY=1`
  once the hypervisor's socket answers, or once it has recorded a
  failed launch), `Restart=no` (restarting is the VM controller's
  decision), `KillMode=mixed` and `TimeoutStopSec=330`: a stop sends
  SIGTERM to the shim only, which presses the power button, waits
  `reconcile.host_shutdown_grace_secs` (default 60 s, at most 300) and
  then stops the hypervisor. It is ordered `After=` netd and Open
  vSwitch so that at host shutdown VMs stop first, but is not
  `PartOf=`/`BindsTo=` the control plane. It carries the hypervisor
  sandbox (spec/security.md §9): `PrivateTmp` (so paths given to the
  API must not be under `/tmp`), `ProtectSystem=strict`, `ProtectHome`,
  `ReadWritePaths=` the service home, its own `/run/glidex-cp/vms/%i`
  and `-/run/glidex/vhost`, `DevicePolicy=closed` with the VM devices,
  and no `NoNewPrivileges=`.
- **glidex-authd** (root, spec/security.md §5.3) is socket-activated:
  `/run/glidex-authd/auth.sock`, `root:glidex 0660`, and it accepts only
  the `glidex` user's uid. It runs PAM with `/etc/pam.d/glidex` for
  accounts in `glidex-users` (`/etc/glidex/authd.json`).
- **glidex-ui** runs as `glidex-ui`, serves `/usr/local/share/glidex/ui`
  over HTTPS on every address, port 5173 (self-signed certificate in
  `StateDirectory=glidex-ui` unless `GLIDEX_UI_TLS_CERT` /
  `GLIDEX_UI_TLS_KEY` are set in a drop-in), and proxies `/api` to the control plane's
  `ui.sock` (`GLIDEX_API_SOCKET=/run/glidex-cp/ui.sock`; see
  [web-ui.md](web-ui.md)). It is sandboxed (`ProtectSystem=strict`,
  `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `NoNewPrivileges`,
  empty `CapabilityBoundingSet=`, `RestrictAddressFamilies=AF_UNIX
  AF_INET AF_INET6 AF_NETLINK`, and `InaccessiblePaths=` for `/run/glidex`,
  `/var/lib/glidex-control-plane` and `/run/glidex-authd`). Users
  always log in.
- Validate with `systemd-analyze verify`; the installer's tests check
  the rendered units (including the VM template, the polkit rule and
  the shim allowlist) and netd's ordering.

## Uninstall

`cargo run -p glidex-install -- uninstall [--dry-run] [--yes]
[--remove-ovs] [--reset-dpdk] [--remove-cloud-hypervisor]
[--purge-user-data]` (source: `crates/glidex-install/src/uninstall.rs`).

It prints a plan first (`--dry-run` stops there; otherwise it asks unless
`--yes`) and re-runs itself through `sudo` when not root. The plan is
built by a pure function over a `HostView`, so tests pin exactly what is
removed. Order:

1. `systemctl stop 'glidex-vm@*.service'` when the template is
   installed: every VM gets its power button and grace (VMs outlive the
   control plane, so stopping it would not stop them). Then `systemctl
   disable --now` the UI, the control plane, glidex-authd (service and
   socket) and glidex-netd.
2. **Network teardown with netd's own code**, in-process on netd's state
   database: release VM ports, delete uplinks (moving migrated IPs back
   onto their NICs and restoring NIC drivers after DPDK), delete NAT
   networks and bridges. Then stop netd's dnsmasq processes and drop the
   `inet glidex` and `inet glidex_meter` nftables tables (the only place the
   metering table is deleted, metering.md D14).
   **Invariant:** if the teardown fails (e.g. netd still running and
   holding the database), it stops before deleting netd's state, which is
   the only record of how to undo those host changes.
3. `setcap -r` on `cloud-hypervisor` (or remove it with
   `--remove-cloud-hypervisor`); remove `glidex-control-plane`, `gxctl`,
   `glidex-netd`, `glidex-ui`, `glidex-authd`, `glidex-vm-shim` from
   `/usr/local/bin` and `~/.local/bin`.
4. Remove the glidex unit files (including `glidex-vm@.service` and
   `glidex-vms.slice`), the polkit rule
   `/etc/polkit-1/rules.d/50-glidex-vm.rules`, and `daemon-reload`.
   `/etc/glidex/vm-shim.json` goes with the installer's other
   configuration files.
5. Optional: `--reset-dpdk` clears `dpdk-init`, `dpdk-socket-mem`,
   `pmd-cpu-mask` from OVS; `--remove-ovs` disables and removes the source
   build (units + `/opt/glidex`) and removes the distro OVS packages
   (`dnsmasq-base` and `nftables` are kept: they are shared).
6. `/etc/glidex`: removed whole when it holds only the installer's files
   (`install.conf`, `authd.json`, `control-plane.json.example`, an empty
   `policies/`) or with `--purge-user-data`; otherwise only those files
   go and the site configuration (`control-plane.json`, `netd.json`,
   policy files, TLS files…) is kept, with a note. `/etc/pam.d/glidex`
   is removed only while it has the `Managed by glidex-install` line.
   Then `/var/lib/glidex`, `/run/glidex`, `/run/glidex-cp`,
   `/run/glidex-authd`, the UI files in `/usr/local/share/glidex`,
   leftover `/tmp/cloud-hypervisor-*.cloudinit.img`, the `glidex` and
   `glidex-ui` users, and the `glidex`, `glidex-ui`, `glidex-users` and
   `glidex-admin` groups (`--dry-run` lists all of it).
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
- **Consumers.** The control plane auto-imports this file as the
  `cloudhv-edk2` firmware image at its first start
  ([images.md](images.md#41-firmware-catalog), which pins the same
  release and digests in `images/firmware.rs`), and a manual pull copies
  it instead of downloading it, when its sha256 matches. VMs boot through that image,
  not this path; the control plane reports at startup whether a firmware
  image exists per hypervisor. The functional tests that pass a host
  `firmware_path` use it unless `GLIDEX_TEST_FIRMWARE` is set. Bump
  `EDK2_FIRMWARE_VERSION` in both crates together. The installer can't depend on the control-plane crate, so
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

- **Supervises only glidex**: units for glidex-netd, the control plane,
  the VM template and the UI (none with `--no-services` /
  `--no-networking`).
- **Does not configure networking itself**: bridges, NAT and uplinks
  are created later through glidex-netd, never by the installer.
- **Does not touch the ReDB file**. If one exists from a previous
  run it is left alone.
- **Modifies `/etc` only through files it owns**: the systemd units,
  the polkit rule `/etc/polkit-1/rules.d/50-glidex-vm.rules`,
  `/etc/glidex/vm-shim.json`, `/etc/sysctl.d/90-glidex.conf`, `/etc/modules-load.d/glidex.conf`,
  `/etc/glidex/install.conf`, `/etc/glidex/authd.json` (once),
  `/etc/glidex/control-plane.json.example`, `/etc/glidex/policies/`
  (the directory), and `/etc/pam.d/glidex` while it carries the marker
  (plus what package installs and `useradd`/`usermod`/`gpasswd` do).
  The uninstaller removes them. It never writes
  `/etc/glidex/control-plane.json`.
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
stack. What is downloaded and then run is pinned by version and sha256
(`download_verified`): the firmware, rustup-init and Bun.
Cloud-Hypervisor is fetched by pinned release tag over HTTPS. The installer itself is therefore small and fast to build.


## Joining a cluster (spec/clustering.md)

`glidex-install --join SERVER:8842 [--role agent|server] [--token-file FILE]
[--advertise IP:PORT] [--node-name NAME]` installs as usual, then asks the
running control plane to join (`gxctl cluster join`, with the token on its
standard input). The token comes from `gxctl cluster join-token` on a server;
it is read from a file (refused unless mode 0600) or stdin, never from argv.
An agent host's UI service is disabled: its servers serve the API and UI.

## OVN

With networking, the installer also installs OVN for cluster networks
(spec/clustering.md §10.1): `ovn-host` on every host, `ovn-central` unless the
run joins as an agent, and `conntrack` for gateway metering. `--no-ovn` skips
them and is remembered in `install.conf`. When this run installed OVN and the
host is in no cluster, OVN's services are stopped and disabled: Debian and
Ubuntu start them on install, and a standalone host would only run empty
databases. glidex-netd starts what a cluster needs (`ovn-host` for the
chassis, `ovn-central` on servers).

