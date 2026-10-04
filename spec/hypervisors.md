# Hypervisor abstraction

Source: `crates/glidex-control-plane/src/hypervisor/`.

## The driver

Since the move to detached instances
([reconciliation.md §16](reconciliation.md#16-hypervisor-abstraction))
the control plane has no hypervisor process handles. A backend is a
**stateless driver** (`hypervisor/mod.rs`):

```rust
pub trait HypervisorDriver: Send + Sync {
    fn hypervisor_type(&self) -> HypervisorType;
    fn is_available(&self) -> bool;   // binary on PATH, /usr/local/bin or /usr/bin
    /// The command line carrying the whole VM config (and QEMU's fallback).
    fn launch_args(&self, config: &VmConfig, api_socket: &str) -> Result<LaunchArgs, HypervisorError>;
    fn observe(&self, api_socket: &str) -> Result<Observed, HypervisorError>;  // guest state + device ids
    fn pause(&self, api_socket: &str) -> Result<(), HypervisorError>;
    fn resume(&self, api_socket: &str) -> Result<(), HypervisorError>;
    fn add_device(&self, api_socket: &str, device_path: &str) -> Result<(), HypervisorError>;
    fn remove_device(&self, api_socket: &str, device_path: &str) -> Result<(), HypervisorError>;
}
```

`hypervisor::driver(ty)` returns the static driver for a
`HypervisorType`. Everything a driver needs about a running instance is
a socket path, so a restarted control plane manages it exactly as the
one that launched it.

### Contract

- **Launch = configured (D9).** `launch_args` returns an argv that
  carries the *whole* VM config. The shim runs it; there is no
  "configure" or "boot" call afterwards, so there is no half-configured
  instance and no crash window between "launched" and "booted".
  `config` arrives with its non-persisted bindings (disks, NICs, seed,
  firmware vars) filled in by the VM controller. `launch_args` is pure
  apart from host probes (QEMU firmware vars, `O_DIRECT`, vhost-net) and
  resolves the binary to an absolute path, which the shim checks against
  its allowlist.
- **Process lifetime belongs to `glidex-vm-shim`**
  ([reconciliation.md §8.3](reconciliation.md#83-shim-process)): spawn,
  launch health check, QEMU's CPU fallback, power button, kill, exit
  status. The driver never starts or signals a process.
- `observe` maps the hypervisor's view to `GuestState` (`NotCreated`,
  `Created`, `Running`, `Paused`, `Shutdown`) plus the ids of its
  devices; the controller uses it for drift (pause/resume, VFIO
  hot-plug). A live instance whose guest is `NotCreated`/`Created`
  cannot happen with D9; the controller kills it rather than patching it
  up through the API.
- `add_device` / `remove_device` are hot-plug operations through the
  hypervisor's live API, with the deterministic VFIO id below. The
  controller calls them only while the guest is observed running.
- The clients themselves (`ChClient`, `QmpClient`) are in
  `crates/glidex-hv-client`, shared with the shim. Each call opens its
  own connection, so the shim's and the control plane's never overlap
  for long.

A guest that powers off makes its hypervisor exit with status 0 under
both backends; so does SIGTERM to the hypervisor
([reconciliation.md §4](reconciliation.md#4-verified-host-facts), F3/F4).
The shim records that as `clean_exit` and the controller sets the
desired state to stopped
([reconciliation.md §7.5](reconciliation.md#75-exit-handling)). A guest
*reboot* resets in place and never exits the hypervisor.

### Backend selection

Firecracker support was removed; see [data-model.md](data-model.md)
for how old Firecracker records are handled. Drivers whose binary is
missing are still selectable: `main` warns at startup, and the error
surfaces at launch (`ProvisioningFailed`, "… is not installed").

## Cloud-Hypervisor

Source: `hypervisor/cloud_hypervisor.rs` (command line) and
`crates/glidex-hv-client/src/ch.rs` (API). API: CH's HTTP protocol over
a Unix socket. Message framing is hand-rolled to match the upstream
`api_client` format exactly (`ChClient::send_request`).

`launch_args` builds the command line that replaced the `vm.create`
payload, option for option, in this order
([reconciliation.md §8.2](reconciliation.md#82-cloud-hypervisor-argv-d9)):
`--api-socket path=…`, `--cpus boot=,max=`, `--memory
size=…M[,shared=on][,hugepages=on]`, `--firmware` or `--kernel` +
`--cmdline` (the cmdline is one argv element, never split), one
`--disk` value per disk, one `--net` value per NIC, one `--device` per
VFIO device, then `--console` / `--serial`. The boot mode decides the
console:

| Boot mode | payload | `--console` | `--serial` | Guest console |
|---|---|---|---|---|
| Kernel (`firmware_path` unset) | `--kernel` + `--cmdline` | `tty` | `off` | `hvc0` |
| Firmware (`firmware_path` set) | `--firmware` | `off` | `tty` | `ttyS0` |

`tty` makes the device CH's stdio, i.e. the shim's PTY; see
[console.md](console.md#why-the-shim-owns-the-master).

Command-line rules, verified against the pinned v53.0 (F1, F2) and
pinned by unit tests in `cloud_hypervisor.rs`:

- A value containing `,` is wrapped in double quotes
  (`path="/a,b/x"`); unquoted, CH would parse the rest as options.
  Paths with `"` or control characters are refused at admission, so
  quoting never needs escaping.
- `image_type=` uses CH's lower-case names: `raw`, `qcow2`, `vhdx`, and
  `vhd` for `FixedVhd` (CH rejects `fixedvhd`).
- `--net`: `id=net<i>,mac=…,num_queues=<2·queue_pairs>[,mtu=…]` plus
  `tap=<if>` or `vhost_user=on,socket=…,vhost_mode=server`. `mtu=` is
  accepted although `--help` does not list it.

### Firmware boot

The firmware is Cloud-Hypervisor's EDK2 build (`CLOUDHV.fd` release
asset of <https://github.com/cloud-hypervisor/edk2>), the `cloudhv-edk2`
firmware image ([images.md](images.md#41-firmware-catalog)): pinned, and
copied from the installer's `~/.glidex/CLOUDHV.fd` (`CLOUDHV_EFI.fd` on
aarch64, [installer.md](installer.md#uefi-firmware)) when that matches. It boots the bootloader on
the rootfs disk, so `rootfs_path` must be a full, partitioned,
UEFI-bootable image (e.g. a distro `*-server-cloudimg-amd64.img`, raw or
qcow2, or a managed disk from [images.md](images.md)), not a bare ext4
rootfs. There is no
kernel command line: the guest's own GRUB config applies.

Why serial instead of virtio-console: distro cloud images put
`console=ttyS0` on their kernel command line, so the login prompt only
appears on the emulated 16550 UART.

Disks are `[rootfs, data disks…, seed]` (`vm_disks`). A managed disk
sends its recorded `image_type`, and `backing_files=on` if it is a
linked overlay ([images.md](images.md#10-changes-to-existing-components)).
A user-supplied `rootfs_path` is probed by magic bytes. The seed is
`config.cloud_init_path`, or — if unset on a firmware boot — the image
the VM controller regenerates at `Vm::default_cloud_init_path()`
(`<run dir>/vms/<id>/cloudinit.img`) before every launch. It is
attached `readonly=on`. Contents (`cloud_init.rs`):

- `meta-data`: `instance-id` = VM id (stable, so cloud-init
  provisions once per VM), `local-hostname` = VM name reduced to an
  RFC 1123 label.
- `network-config`: v2, DHCP on `en*`.
- `user-data`: `resize_rootfs: true` (plus a `growpart` block when the
  root disk has `pending_growpart`), and a sudo user. If the VM names a stored credential
  ([credentials.md](credentials.md)), that username, password hash and
  SSH keys are used and nothing else. Otherwise the user is `cloud` with no SSH keys and a locked password:
  there is no way to log in. There are no host-wide defaults (the
  control-plane user's keys or a site password would open VMs in every
  project, spec/security.md §6.2). Credentials are never baked into
  the source. The seed image is created `0600`.

Why the hash is written twice (`users[].passwd` and `chpasswd.users`):
cloud-init ignores `users[].passwd` for a user that already exists,
which is the case whenever the rootfs was provisioned by an earlier
instance. `chpasswd` applies either way. Its `type` must be lowercase
`hash`; cloud-init compares it case-sensitively and silently skips
`HASH`.

Runtime operations (`glidex-hv-client/src/ch.rs`): `observe` is `GET
/vm.info`, `pause` / `resume` are `/vm.pause` / `/vm.resume`,
`add_device` / `remove_device` use `/vm.add-device` and
`/vm.remove-device` with a deterministic device id derived from the
sysfs BDF (`_vfio_0000_41_00_0`). The shim uses `GET /vmm.ping` (launch
health check), `/vm.power-button`, and `PUT /vmm.shutdown` to stop CH
without the guest: it closes the disks first, where a bare SIGKILL can
leave a qcow2 disk CH refuses to reopen (F8).

## QEMU

Source: `hypervisor/qemu.rs` (command line) and
`crates/glidex-hv-client/src/qmp.rs` (API). API: **QMP** (QEMU Machine Protocol)
over a Unix socket. Needs QEMU ≥ 6.0 (`server=on` sockets,
`-machine memory-backend=`); an older one is refused at launch.

### Launch

QEMU takes all its config on the command line, as Cloud Hypervisor now
does too. It is started **without** `-S`: the guest runs from launch
(D9), and nothing is sent over QMP to boot it.

`launch_args` first resolves the config against the host into a
`LaunchSpec` (firmware files, `O_DIRECT` support per disk,
`/dev/vhost-net` access, QEMU ≥ 6.0); `LaunchSpec::args` then builds
the command line without touching the host, which is what the unit
tests check:

```
qemu-system-x86_64
  -nodefaults -no-user-config -enable-kvm
  -machine q35[,memory-backend=mem][,smm=on]
  [-cpu host]
  -m <mem>M -smp <vcpus>
  [-object memory-backend-memfd,id=mem,size=<mem>M,share=on[,hugetlb=on]]
  # kernel boot:
  -kernel <kernel_image_path> -append "<kernel_args>"
  # or firmware boot (see below):
  [-global driver=cfi.pflash01,property=secure,value=on]
  -drive if=pflash,format=raw,unit=0,readonly=on,file=<OVMF code>
  -drive if=pflash,format=raw,unit=1,file=<this VM's vars>
  # per disk i, in the order [root, data disks…, seed]:
  -blockdev <JSON> -device virtio-blk-pci,drive=disk<i>,id=vd<i>[,bootindex=1]
  # per NIC, see networking.md §3a:
  [-netdev … -device virtio-net-pci,…]
  [-device vfio-pci,host=<bdf>,id=<_vfio_xxx> …]
  -object rng-random,id=rng0,filename=/dev/urandom -device virtio-rng-pci,rng=rng0
  -qmp unix:<socket_path>,server=on,wait=off
  -serial stdio -display none
```

Notes captured in code comments:

- `-nodefaults` drops QEMU's default VGA, CD-ROM and, importantly, its
  default user-mode NIC: the guest gets exactly the devices above, like
  under Cloud-Hypervisor. There is no `-no-reboot`: a guest reboot
  resets in place, as CH does.
- We avoid `-nographic` because it implies `-serial mon:stdio` and
  collides with our explicit `-serial stdio`.
- `-cpu host` first, so guests see the host's CPU features (CH's
  default). Some hosts can't express their CPU to KVM: `launch_args`
  also returns a `fallback` argv without `-cpu` (QEMU's default model)
  with the markers `cpu` and `msr`; if QEMU exits before QMP answers
  and its output contains one of them (case-insensitive), the shim runs
  the fallback once (`launch.json`, reconciliation.md §8.1).
- Disks use `-blockdev` JSON, so a path never needs escaping. The
  format is always explicit: the recorded format for managed disks,
  glidex's own magic-byte probe for a user-supplied `rootfs_path`,
  never QEMU's probing. A qcow2 gets `"backing": null` unless it is a
  linked overlay glidex created (`backing_files`); otherwise QEMU would
  follow whatever backing file a user-supplied qcow2 names and hand
  that host file to the guest. Disks on filesystems that support
  `O_DIRECT` get `cache.direct=on,aio=native`; others (e.g. a tmpfs
  runtime-directory seed) use the page cache.
- The root disk has `bootindex=1`, so OVMF boots it rather than the
  seed or a data disk.
- QEMU's stderr goes to a pipe the shim's console proxy writes to the
  VM's log only, so its warnings never appear in a console session but
  still show up in a launch error.

### Firmware boot

`firmware_path` is an OVMF code image, normally the `ovmf` firmware
image: a copy of the first of `/usr/share/OVMF/OVMF_CODE_4M.fd`,
`/usr/share/OVMF/OVMF_CODE.fd`, `/usr/share/edk2/ovmf/OVMF_CODE.fd`,
`/usr/share/edk2/x64/OVMF_CODE.4m.fd`,
`/usr/share/edk2-ovmf/x64/OVMF_CODE.fd` that exists, with its variable
store template (`<id>.vars.fd`). `prepare_firmware`:

- takes the image's template (`VmConfig.firmware_vars_template`, filled
  in by the VM controller), else the pristine variable store next to the
  code file (`CODE` → `VARS` in the file name). The copy is made `0600`:
  the template of a firmware image is read-only. Each VM gets its own copy at
  `<data dir>/firmware-vars/<vm id>.fd` (`VmConfig.firmware_vars_path`,
  filled in by the VM controller, never persisted), copied on first boot and
  again only if the template's size changed. Boot entries the guest
  writes survive restarts; the copy is deleted with the VM;
- treats a code image whose name contains `secboot`, `.ms.` or
  `snakeoil` as a Secure Boot build: `smm=on` and a secure pflash;
- maps a file with no matching variable store (a combined `OVMF.fd`)
  with `-bios`.

The cloud-init seed and stored credentials work exactly as for
Cloud-Hypervisor (above): the seed is attached read-only as the last
disk, and the console is the serial port.

### QMP client

`QmpClient` (`glidex-hv-client/src/qmp.rs`) opens a *new* Unix
connection per command. Each connection:

1. Reads the server greeting line.
2. Sends `{"execute":"qmp_capabilities"}` and reads the `return`.
3. Sends the actual command JSON and waits for the first
   `return`/`error` line, skipping any asynchronous `event` lines
   in between.

A QMP monitor serves one client at a time, so a connect that finds it
busy is retried for up to 2 s (the shim and the control plane may both
talk to it).

| Operation | Used by | QMP command |
|---|---|---|
| launch health check | shim | greeting + `qmp_capabilities` |
| `observe` | controller | `query-status` (`prelaunch` → `Created`, `running` → `Running`, `paused`/… → `Paused`, `shutdown`/`guest-panicked`/… → `Shutdown`) and `qom-list /machine/peripheral` |
| `pause` / `resume` | controller | `stop` / `cont` |
| power button | shim | `system_powerdown` (after `cont` if paused) |
| kill | shim | `quit`; SIGKILL if QEMU is still there after 2 s |
| `add_device` | controller | `device_add` with `driver=vfio-pci`, `host=<bdf>`, `id=<deterministic>` |
| `remove_device` | controller | `device_del` with `id=<deterministic>` |

### Launch health check

Done by the shim for both backends
([reconciliation.md §8.3](reconciliation.md#83-shim-process) step 3):
it waits up to `ready_timeout_secs` (30) for the API socket to answer
(CH `GET /vmm.ping`, QEMU's QMP greeting), watching for early exit. If
the hypervisor exits first (after the QEMU fallback, if any), the shim
records exit cause `launch_failed` with the hypervisor's captured
stderr as the message; the controller reports it as
`Ready=False/LaunchFailed` and `?wait` returns it as `500
hypervisor_error`. This is what surfaces misconfigured kernel paths,
missing KVM, missing firmware and other launch-time errors.

### Default kernel args

`HypervisorType::Qemu::default_kernel_args` is
`"console=ttyS0 root=/dev/vda reboot=k panic=1"`. `root=/dev/vda`
(no partition number) assumes a bare ext4 rootfs image with no
partition table. glidex no longer ships one; bring your own kernel +
rootfs for kernel boot, or use firmware boot with a distro cloud image.

## VFIO device identifiers

Both backends derive a stable *id* for a
device from the sysfs path. Given
`/sys/bus/pci/devices/0000:41:00.0` the id is `_vfio_0000_41_00_0`
(colons/dots replaced with underscores, `_vfio_` prefix). This id is:

- Given to the device on the launch command line (CH `--device
  …,id=`, QEMU `-device vfio-pci,…,id=`) and in hot-plug calls, so
  detach can refer to the same device that was attached.
- Compared with the device ids `observe` reports, so the controller can
  tell which spec devices the running guest has (VFIO drift,
  reconciliation.md §9.1 step 7).
- Not persisted. `Vm.config.vfio_devices` stores the sysfs path —
  the id is rederived when needed.
