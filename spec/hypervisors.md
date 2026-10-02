# Hypervisor abstraction

Source: `crates/glidex-control-plane/src/hypervisor/`.

## The traits

`Hypervisor` is a **factory** for running VMs. It has no instance
state of its own; it's stateless except for knowing what backend it
represents.

```rust
pub trait Hypervisor: Send + Sync {
    fn spawn(
        &self,
        socket_path: &str,          // hypervisor API / QMP socket
        console_socket_path: &str,  // client-facing console
        log_path: &str,             // append-only captured console
    ) -> Result<Box<dyn HypervisorProcess>, HypervisorError>;

    fn hypervisor_type(&self) -> HypervisorType;
    fn is_available(&self) -> bool;  // is the binary on PATH?
}
```

`HypervisorProcess` is the **running VM handle**. Every method is
`&self` — mutable state (child pid, console thread join handle,
atomic run flag) is behind interior-mutability primitives so the
handle is `Send + Sync`:

```rust
pub trait HypervisorProcess: Send + Sync {
    fn configure(&self, config: &VmConfig) -> Result<(), HypervisorError>;
    fn start(&self) -> Result<(), HypervisorError>;
    fn pause(&self) -> Result<(), HypervisorError>;
    fn resume(&self) -> Result<(), HypervisorError>;
    fn kill(&self) -> Result<(), HypervisorError>;
    fn request_shutdown(&self) -> Result<(), HypervisorError>;                 // default: Unsupported

    fn add_device(&self, device_path: &str) -> Result<(), HypervisorError>;     // default: Unsupported
    fn remove_device(&self, device_path: &str) -> Result<(), HypervisorError>;  // default: Unsupported

    fn is_running(&self) -> bool;
    fn socket_path(&self) -> &str;
    fn console_socket_path(&self) -> &str;
    fn log_path(&self) -> &str;
}
```

### Trait contract

- `spawn` **may or may not** launch the actual hypervisor binary.
  Cloud-Hypervisor launches in `spawn` (its API is HTTP: you talk to
  it while it waits for config). QEMU launches
  in `configure` because QEMU needs the full config on its command
  line; see below.
- `configure` must be called exactly once, before `start`.
- After `kill`, the process handle is done. A fresh `spawn` +
  `configure` + `start` is required to bring the VM back.
- `add_device` / `remove_device` on an already-running VM are
  hot-plug operations and must go through the hypervisor's live
  management API. Backends that don't support it can leave the
  default impls, which return `Unsupported`.
- `request_shutdown` presses the guest's ACPI power button (CH
  `/vm.power-button`, QEMU `system_powerdown`) and returns at once.
- `is_running` is false after `kill` **and** once the hypervisor
  process has exited on its own: both backends exit when the guest
  powers off. `VmManager::reap_exited_vms` (every 2 s, from `main`)
  turns that into `stopped` and releases the NICs.
  `stop_vm_graceful` (`POST /vms/{id}/stop?graceful_timeout_secs=N`)
  requests a shutdown, polls `is_running` without holding the VM lock,
  then stops the VM hard either way.
- **Console listener lifetime** (see [console.md](console.md)): the
  console Unix socket listener must remain bound from `spawn` (or
  `configure`, for QEMU) until `kill`. It must not be dropped just
  because the guest died.

### Backend selection

`VmManager` maintains a `HashMap<HypervisorType, Box<dyn Hypervisor>>`
populated at startup with both backends (Cloud-Hypervisor and QEMU).
Firecracker support was removed; see [data-model.md](data-model.md)
for how old Firecracker records are handled. On `start_vm`, it
looks the VM's `hypervisor` up in that map and delegates to it.
Backends whose binary is missing from `PATH` are still registered —
the error surfaces only at launch time.

## Cloud-Hypervisor

Source: `hypervisor/cloud_hypervisor.rs`. API: CH's HTTP protocol
over a Unix socket. Message framing is hand-rolled to match the
upstream `api_client` format exactly (see `send_request` in that
file).

`spawn` runs `cloud-hypervisor --api-socket <sock>` on a PTY glidex
owns (`console::spawn_on_pty`: the raw slave is CH's stdin/stdout, CH's
stderr goes to the log) and starts the console proxy at once; see
[console.md](console.md#why-glidex-owns-the-master). If the API socket
doesn't appear, the error includes what CH printed.

`configure` does a single `PUT /vm.create` with a full config
payload (CPU, memory, payload, disks, console/serial config, any
VFIO devices). The payload depends on the boot mode:

| Boot mode | `payload` | `console` | `serial` | Guest console |
|---|---|---|---|---|
| Kernel (`firmware_path` unset) | `kernel` + `cmdline` | `Tty` | `Off` | `hvc0` |
| Firmware (`firmware_path` set) | `firmware` only | `Off` | `Tty` | `ttyS0` |

(`boot_payload`.) `Tty` makes the device CH's stdio, i.e. the PTY above.
`start` issues `PUT /vm.boot`.

### Firmware boot

The firmware is Cloud-Hypervisor's EDK2 build (`CLOUDHV.fd` release
asset of <https://github.com/cloud-hypervisor/edk2>), downloaded by
`glidex-install` to `default_firmware_path()` —
`~/.glidex/CLOUDHV.fd` (`CLOUDHV_EFI.fd` on aarch64); see
[installer.md](installer.md#uefi-firmware). It boots the bootloader on
the rootfs disk, so `rootfs_path` must be a full, partitioned,
UEFI-bootable image (e.g. a distro `*-server-cloudimg-amd64.img`, raw or
qcow2, or a managed disk from [images.md](images.md)), not a bare ext4
rootfs. There is no
kernel command line: the guest's own GRUB config applies.

Why serial instead of virtio-console: distro cloud images put
`console=ttyS0` on their kernel command line, so the login prompt only
appears on the emulated 16550 UART.

Disks are `[rootfs, data disks…, seed]` (`disk_configs`). A managed disk
sends its recorded `image_type`, and `backing_files: true` if it is a
linked overlay ([images.md](images.md#10-changes-to-existing-components)).
A user-supplied `rootfs_path` is probed by magic bytes. The seed is
`config.cloud_init_path`, or —
if unset on a firmware boot — the image `VmManager::start_vm`
regenerates at `Vm::default_cloud_init_path()`
(`/tmp/cloud-hypervisor-<id>.cloudinit.img`) before `configure`. It is
attached `readonly`. Contents (`cloud_init.rs`):

- `meta-data`: `instance-id` = VM id (stable, so cloud-init
  provisions once per VM), `local-hostname` = VM name reduced to an
  RFC 1123 label.
- `network-config`: v2, DHCP on `en*`.
- `user-data`: `resize_rootfs: true` (plus a `growpart` block when the
  root disk has `pending_growpart`), and a sudo user. If the VM names a stored credential
  ([credentials.md](credentials.md)), that username, password hash and
  SSH keys are used and nothing else. Otherwise the user is `cloud` with
  the control-plane user's `~/.ssh/*.pub` keys, and a password only from
  `GLIDEX_CLOUD_INIT_PASSWD_HASH` (crypt hash, e.g. `openssl passwd -6`);
  without it password login is locked. Credentials are never baked into
  the source. The seed image is created `0600`.

Why the hash is written twice (`users[].passwd` and `chpasswd.users`):
cloud-init ignores `users[].passwd` for a user that already exists,
which is the case whenever the rootfs was provisioned by an earlier
instance. `chpasswd` applies either way. Its `type` must be lowercase
`hash`; cloud-init compares it case-sensitively and silently skips
`HASH`.

`pause` / `resume` / `kill` map directly to the corresponding CH API
endpoints, `request_shutdown` to `/vm.power-button`. `add_device` / `remove_device` use CH's `/vm.add-device`
and `/vm.remove-device`, with a deterministic device id derived from
the sysfs BDF (`_vfio_0000_41_00_0`).

## QEMU

Source: `hypervisor/qemu.rs`. API: **QMP** (QEMU Machine Protocol)
over a Unix socket. Needs QEMU ≥ 6.0 (`server=on` sockets,
`-machine memory-backend=`); an older one is refused at launch.

### Deferred launch

QEMU is fundamentally different from Cloud-Hypervisor: it takes *all*
its config on the command line. There is no runtime "configure" API
once it's running. Therefore:

- `QemuBackend::spawn` does **nothing but allocate the handle**. No
  `qemu-system-x86_64` process is started.
- `QemuInstance::configure(&config)` is where the process is
  actually launched, with `-S` so the guest is paused at reset.
- `start` then sends QMP `cont` to unfreeze it.

`launch` first resolves the config against the host into a
`LaunchSpec` (firmware files, `O_DIRECT` support per disk,
`/dev/vhost-net` access); `LaunchSpec::args` then builds the command
line without touching the host, which is what the unit tests check:

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
  -S
```

Notes captured in code comments:

- `-nodefaults` drops QEMU's default VGA, CD-ROM and, importantly, its
  default user-mode NIC: the guest gets exactly the devices above, like
  under Cloud-Hypervisor. There is no `-no-reboot`: a guest reboot
  resets in place, as CH does.
- We avoid `-nographic` because it implies `-serial mon:stdio` and
  collides with our explicit `-serial stdio`.
- `-cpu host` first, so guests see the host's CPU features (CH's
  default). Some hosts can't express their CPU to KVM; if QEMU exits
  before QMP is up and its output mentions the CPU or an MSR,
  `launch` retries once without `-cpu` (QEMU's default model).
- Disks use `-blockdev` JSON, so a path never needs escaping. The
  format is always explicit: the recorded format for managed disks,
  glidex's own magic-byte probe for a user-supplied `rootfs_path`,
  never QEMU's probing. A qcow2 gets `"backing": null` unless it is a
  linked overlay glidex created (`backing_files`); otherwise QEMU would
  follow whatever backing file a user-supplied qcow2 names and hand
  that host file to the guest. Disks on filesystems that support
  `O_DIRECT` get `cache.direct=on,aio=native`; others (e.g. a tmpfs
  `/tmp` seed) use the page cache.
- The root disk has `bootindex=1`, so OVMF boots it rather than the
  seed or a data disk.
- QEMU's stderr goes to the VM's log file only (opened `O_APPEND`, as
  is the console proxy's handle), so its warnings never appear in a
  console session but still show up in the launch error below.

### Firmware boot

`firmware_path` is an OVMF code image. Its default
(`qemu::default_firmware_path`, used by `create_vm` for `image` /
`root_disk` VMs and by `gxctl`) is the first of
`/usr/share/OVMF/OVMF_CODE_4M.fd`, `/usr/share/OVMF/OVMF_CODE.fd`,
`/usr/share/edk2/ovmf/OVMF_CODE.fd`, `/usr/share/edk2/x64/OVMF_CODE.4m.fd`,
`/usr/share/edk2-ovmf/x64/OVMF_CODE.fd` that exists. `prepare_firmware`:

- finds the pristine variable store next to it (`CODE` → `VARS` in the
  file name). Each VM gets its own copy at
  `<data dir>/firmware-vars/<vm id>.fd` (`VmConfig.firmware_vars_path`,
  filled in by `start_vm`, never persisted), copied on first boot and
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

`QmpClient` in the same file opens a *new* Unix connection per
command. Each connection:

1. Reads the server greeting line.
2. Sends `{"execute":"qmp_capabilities"}` and reads the `return`.
3. Sends the actual command JSON and waits for the first
   `return`/`error` line, skipping any asynchronous `event` lines
   in between.

Commands used:

| Operation | QMP command |
|---|---|
| `start` / `resume` | `cont` |
| `pause` | `stop` |
| `request_shutdown` | `system_powerdown` |
| `kill` | `quit` (best-effort; child is also killed) |
| `add_device` | `device_add` with `driver=vfio-pci`, `host=<bdf>`, `id=<deterministic>` |
| `remove_device` | `device_del` with `id=<deterministic>` |

`is_running` is false once the `qemu-system-x86_64` child has exited
(the guest powered off), not only after `kill`.

### Launch health check

`launch` loops up to 5 seconds waiting for the QMP socket to appear
**and** respond with a greeting. It also calls `child_exit_status()`
each iteration: if QEMU has already exited, we read the captured
log and return a `ProcessStart` error whose message embeds QEMU's
output. This is what surfaces misconfigured kernel paths, missing
KVM, missing firmware and other launch-time errors.

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

- Used in the hypervisor's attach/detach calls so detach can refer
  to the same device that was attached.
- Not persisted. `Vm.config.vfio_devices` stores the sysfs path —
  the id is rederived when needed.
