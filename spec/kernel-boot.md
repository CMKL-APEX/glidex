# Direct kernel boot

Status: **draft**; K0 to K3b implemented (§12). The open questions of the first
draft are settled (§9): kernels come from the cloud image itself (K3,
K9, K13), a catalog pull brings its kernel (K10), a VM's kernel may
change at the next restart (K6), and a VM may opt in to following the
kernels its guest installs, each version kept while used (K11, K12). Extends [images.md](images.md),
[storage.md](storage.md), [hypervisors.md](hypervisors.md),
[console.md](console.md) and [credentials.md](credentials.md). §13 lists
the changes the other documents need once this is accepted.

Every VM boots in one of two ways. **Firmware boot** passes UEFI firmware
to the hypervisor, and the firmware runs the bootloader on the root disk.
**Kernel boot** passes a kernel to the hypervisor, which loads it
directly, with no firmware or bootloader. Both backends have had kernel
boot since the beginning (`--kernel`/`--cmdline` and `-kernel`/`-append`).
Since [images.md](images.md) it has fallen behind firmware boot:

| | Firmware boot today | Kernel boot today |
|---|---|---|
| Boot payload | Managed **firmware image** (catalog with pinned digests, URL, host import, auto-import) or an admin's host path | Host path only (`kernel_image_path`, needs `host.paths`) |
| initramfs | n/a | **none**: only kernels with their disk driver and root filesystem built in can boot |
| Root disk | Managed image, managed disk, or host path | API accepts all three; UI and gxctl offer only a host path; default `root=/dev/vda` can't mount a partitioned cloud image |
| cloud-init seed | Generated (hostname, network, growpart, credential) | **never generated**; credentials refused |
| Console | `ttyS0` on both backends | `hvc0` on Cloud Hypervisor (nothing before virtio comes up), `ttyS0` on QEMU |
| In use / delete | `used_by_vms`, `409` while in use | n/a |
| Clusters | Image cache copies firmware to the VM's node; VM gets the firmware's arch | Host path must already exist on the node; VM has no arch |
| Tests | Functional and UI end-to-end boots | Argv unit tests only |

This document brings kernel boot level with firmware boot. Ordinary
project users get managed kernels the same way they get managed
firmware, with no host paths involved. A distro cloud image boots the
same way through either mode: same seed, same credentials, same console,
same disk handling.

## 1. Scope

- **In:** `initramfs` for both backends. A new image kind `kernel`
  (a kernel, an optional initramfs and a default command line) with the
  same verification, storage, in-use protection and cluster caching as
  firmware images. Its sources:
  - **extracted** from a disk image's own `/boot` by a short-lived
    extractor VM (§10). This works for every catalog distro (F-K6, F-K7),
    and a catalog pull does it by default;
  - a URL;
  - a host file (admins).
- **In:** generated seeds and credentials on kernel boot; the console
  device follows the command line; UI, gxctl, REST/gRPC and docs.
- **Out:** making kernel boot the default for image VMs. Firmware boot
  stays the default (K2).
- **Out:** kexec, guest-driven kernel switches, Secure Boot or measured
  boot for kernel boot, and new architectures beyond what each backend
  already builds for.

## 2. Decisions

| # | Topic | Decision |
|---|---|---|
| K1 | Payload model | A **kernel image** is an `Image` of kind `kernel`. It is made of `<id>.kernel`, an optional `<id>.initrd` and a default `cmdline`. VMs name it with `kernel` (id or name), as they name firmware with `firmware`. `kernel_image_path` stays for admins, alongside a new `initrd_path`. Both are host paths, like `firmware_path`. |
| K2 | Default boot mode | Unchanged. An `image`/`root_disk` VM boots through firmware unless the request names a kernel (`kernel` or `kernel_image_path`). Giving both a kernel and a firmware, from either kind of source, is `400 invalid_config`. Today the firmware silently wins. |
| K3 | Kernel/rootfs match | A kernel for a disk image is **extracted from that image** (§10), so it is the build the image's own bootloader would boot, with its own modules in `/lib/modules/<release>`. No other build is ever substituted. The Ubuntu-only download of `unpacked/` files (first draft) is dropped: extraction gives the same bytes (F-K7) and works for every distro. |
| K4 | Seed | The VM controller generates the cloud-init seed on kernel boot too. The rule is stored on the VM as `generate_seed` (§6.3), so existing kernel-boot VMs don't gain a disk on upgrade. Credentials require a generated seed, not firmware boot. |
| K5 | Console | The **last `console=`** on the command line picks the device: `hvc<n>` → virtio console, anything else (or none) → serial port. Both backends do the same. New VMs default to `console=ttyS0`, so early boot output and panics are visible under Cloud Hypervisor too. |
| K6 | Kernel updates | **Settled: yes.** Kernel boot runs the kernel glidex hands to the hypervisor, so a kernel the guest installs itself doesn't boot on its own. `kernel` may be changed on a stopped or running VM and takes effect at the next launch (`RestartRequired`). A host-side update is a pull (or an extraction from the VM's disk, §10.5) and a PATCH. Switching between firmware and kernel boot stays immutable. |
| K7 | Command line | `kernel_args` defaults to the kernel image's `cmdline`, else the hypervisor default. It is one argv element, at most 2047 bytes (x86 `COMMAND_LINE_SIZE` less the NUL), with no control characters (`400 invalid_config`). |
| K8 | Formats | Kernel: x86_64 bzImage (`HdrS` at 0x202) or ELF `vmlinux`; aarch64 `Image` (`ARM\x64` at 0x38) or EFI-stub `Image`. Anything else is refused at verify. An initramfs is not parsed; only its size is checked. |
| K9 | Extraction | A disk image's filesystems are read **inside a VM**, never on the host: glidex boots a pinned extractor kernel and initramfs with the image attached read-only and no network, and the extractor writes the boot files to a second, raw disk in a simple stream format the host parses with strict limits (§10). No new host packages, no root, no loop devices, and the host never parses an untrusted filesystem. |
| K10 | Catalog pulls | **Settled: yes.** `POST /images {"catalog": …}` also extracts the image's kernel once it is `Ready` (`with_kernel`, default `true` for catalog pulls and `false` for URL pulls). The kernel is a separate image record (`<image name>-kernel`). A failed extraction fails only that record; the disk image stays `Ready` and firmware boot is unaffected. |
| K11 | Guest kernel updates | **Opt-in** per VM (`kernel_from_disk`, §10.5): before each launch glidex extracts the kernel from the VM's own root disk, so kernels the guest installs boot like they would under a bootloader. Off by default: the VM boots the kernel image it names. |
| K12 | Kernel versions | Kernel images are **never replaced in place**. A newer build, or a kernel a guest installed, is a new record; every VM keeps the version it names until it is changed (K6). Identical files are stored once: extraction reuses a kernel image with the same kernel and initrd digests. Kernels extracted for one VM (`owner`) keep its current and previous version, and are deleted when no VM uses them (§10.5). |
| K13 | Extractor source | A Debian cloud kernel (`linux-image-<version>-cloud-amd64`; arm64 later) and `busybox-static` from `snapshot.debian.org`, sha256-pinned, with the initramfs written by glidex (§10.1). |

## 3. Verified facts

Checked on 2026-10-19 on the development host (Cloud Hypervisor v53.0,
the pinned version; QEMU from the host's packages):

- **F-K1.** Cloud Hypervisor v53 boots a compressed **bzImage** with
  `--kernel` (Ubuntu `6.8.0-147-generic`, zstd-compressed), and
  `--initramfs` loads its initrd ("Trying to unpack rootfs image as
  initramfs"). `--help` lists `--initramfs`, and describes `--kernel` as
  a PVH-capable kernel such as `vmlinux` "or architecture equivalent".
  An uncompressed `vmlinux` is therefore **not** required, so distro
  kernels work as published.
- **F-K2.** QEMU boots the same files with `-kernel`/`-initrd`.
- **F-K3.** Ubuntu 24.04 cloud image build `20261017`, as a linked
  qcow2 overlay, boots under both backends with that build's
  `unpacked/…-vmlinuz-generic` and `…-initrd-generic`, with
  `console=ttyS0 root=LABEL=cloudimg-rootfs ro`. The initrd mounts
  `vda1` and systemd starts ("Welcome to Ubuntu 24.04.5 LTS"). Without
  a NIC or seed it then waits in `systemd-networkd-wait-online`, as a
  firmware boot does. The generated seed and NIC are not exercised yet;
  that is a K3 acceptance item.
- **F-K4.** Ubuntu publishes, for every cloud image build,
  `<suite>/<serial>/unpacked/<suite>-server-cloudimg-<arch>-vmlinuz-generic`
  and `-initrd-generic`, listed in that directory's `SHA256SUMS`, with
  `current/` pointing at the newest serial. Dated directories are pruned
  after some months (the oldest noble serial listed is `20260801`).
- **F-K5.** Debian's cloud image directories (`cloud.debian.org/images/cloud/<suite>/latest/`)
  publish no separate kernel or initrd. Fedora and AlmaLinux don't
  either.
- **F-K6.** Where the catalog images keep `/boot` (GPT read over HTTPS
  with QEMU's curl driver, each partition's superblock checked):

  | Image | `/boot` is on | Boot config |
  |---|---|---|
  | Ubuntu 24.04 (`noble`) | its own ext4 partition (XBOOTLDR type, label `BOOT`, p16) | `grub/grub.cfg` |
  | Debian 13 | the ext4 root (p1) | `boot/grub/grub.cfg` |
  | Fedora 44 Cloud Base Generic | the btrfs root (p3) | BLS `boot/loader/entries/*.conf` |
  | AlmaLinux 9 GenericCloud | its own XFS partition (p3) | BLS `loader/entries/*.conf` |

  Fedora also publishes a `UEFI-UKI` variant whose kernel is a unified
  kernel image on the ESP; the catalog uses `Cloud_Base` (Generic).
- **F-K7.** An extractor VM prototype (Ubuntu's `6.8.0-147-generic`
  kernel and initrd, which carry `xfs.ko` and `btrfs.ko`, plus a
  60-line shell script appended as a cpio, run as `rdinit=`) mounted
  each partition read-only (`ext4 noload`, `xfs norecovery`, `btrfs`,
  `vfat`), found the kernels, initrds and boot configs, and wrote them
  to a raw disk. Each image took **2.5–5 s** under Cloud Hypervisor.
  For Ubuntu, the extracted kernel and initrd are **byte-identical** to
  the build's published `unpacked/` files. Each image then booted to a
  `login:` prompt with its extracted kernel, its own initrd and the
  command line from its boot config (with `console=ttyS0`): Debian 13
  (`6.12.113+deb13-amd64`), AlmaLinux 9.8 (`5.14.0-687.53.1.el9_8`,
  root and `/boot` on XFS), Fedora 44 (`6.19.10-300.fc44`, root on
  btrfs with `rootflags=subvol=root`). Ubuntu reached systemd (F-K3).
  The initrd's own `cpio` can only extract and its busybox has no `dd`,
  so the prototype's output is one sequential stream written with
  `cat`; §10.3 keeps that format.
- **F-K8.** Ubuntu 26.04 (`resolute`, build `20260927`, kernel
  `7.0.0-34-generic`) keeps `/boot` on its own ext4 partition, **p13**
  (24.04 uses p16), with `root=LABEL=cloudimg-rootfs ro console=tty1
  console=ttyS0` in `grub.cfg`. The extractor prototype read it in 3 s,
  and the files match the build's published `unpacked/` ones byte for
  byte. Through glidex (K0's `kernel_boot_with_initrd` and
  `qemu_kernel_boot_with_initrd`, image as `rootfs_path`) it boots to
  systemd on both backends with both console devices, and to `login:`
  under Cloud Hypervisor. 26.04 also publishes an `amd64v3` (x86-64-v3)
  kernel next to the generic one; extraction takes whichever kernel the
  image itself has, which is the generic one in the catalog's `amd64`
  image.
- **F-K9.** The Debian appliance of §10.1 (cloud kernel
  `6.12.107+deb13-cloud-amd64`, `busybox-static`, the twelve modules)
  extracts Ubuntu 26.04 in 1.8 s, AlmaLinux 9 (XFS) in 1.8 s, Fedora 44
  (btrfs) in 1.8 s and Debian 13 in 1.7 s under Cloud Hypervisor, and
  Ubuntu 26.04 in 2.2 s under QEMU, reading every partition including
  the ESP. A first build lacked `nls_ascii` and the ESP failed to mount
  ("IO charset ascii not found").

## 4. Kernel images

### 4.1 Data model

`ImageKind` gains `Kernel`. An `Image` of that kind has:

- `hypervisor: None`. A kernel is not tied to a backend (F-K1, F-K2).
- `arch` from the disk image it was extracted from, or the request,
  checked against the kernel's header at verify (K8).
- `format`: unused, recorded as `Raw`. `virtual_size_bytes` is the
  kernel's size. `file_size_bytes` covers the kernel plus the initrd.
- `sha256`: the kernel file's digest.
- New fields, serde-defaulted:
  - `initrd: Option<FileMeta { sha256, size_bytes }>`;
  - `cmdline: Option<String>`, the default `kernel_args` (K7);
  - `kernel_release: Option<String>`, the kernel's version string from
    its header (bzImage `kernel_version` offset; ELF/`Image` don't carry
    one, so `None`). Named so because `Image.release` already lists the
    nodes asked to drop their copy;
  - `for_image: Option<String>`, the disk image (id) this kernel was
    extracted from (K3; not yet implemented);
  - `owner: Option<String>`, the VM a kernel extracted from its own disk
    belongs to (K12, §10.5; with K3b). Owned kernels are listed with the VM's
    project and cleaned up with their last user.

`ImageSource` gains `Extracted { image_id, image_sha256, path }`: the
disk image, the digest of the copy that was read, and the kernel's path
inside it (`vda16:/vmlinuz-6.8.0-147-generic`). URL pulls stay `Url`
with the initrd URL in `DownloadMeta`.

Storage ([images.md §2](images.md#2-storage-layout)):
`images/<id>.kernel` and `images/<id>.initrd`, `0444`, both named only
by id. The `.part` files follow the same rules. On datastores
([storage.md §6](storage.md#6-images-on-datastores)), kernel images
follow the firmware rule: a copy per location, and launch resolves the
files on the placed node.

### 4.2 Sources

**From a disk image** (K3, K9). `POST /images {"kernel_from": "<image id
or name>", name?, nodes?}` records a kernel image in `Extracting`
(`IMAGE_STATE_EXTRACTING`), with `for_image` and an `extracted` source
(`image_id`, and once done `image_sha256` and the kernel's `path` in the
image), and returns `202`. The image controller runs the extractor (§10) once the disk image
is `Ready` (it may still be downloading). The disk image must be of kind
`disk`; a firmware or kernel image is `400 invalid_image`. Default name
`<image name>-kernel`. A second request for the same image while one is
extracting returns that record (`200`). A catalog pull does this itself
(K10).

**Arbitrary URL.** `{"url": …, "sha256"?, "kind": "kernel", "initrd_url"?,
"initrd_sha256"?, "cmdline"?, "arch"?}`. These follow the URL rules of
[images.md](images.md#arbitrary-urls) (https, no private addresses,
every redirect checked). The image is `verified` only when every file
had a digest.

**Host import.** `{"kernel_path": "/boot/…", "initrd_path"?, "cmdline"?}`
copies host files into a kernel image. This needs `host.paths`, as any
host path does (security.md §7.4). Afterwards the VM needs only
`readImage`, so an admin can publish a custom kernel to projects
without handing out host paths. No auto-import: unlike OVMF, there is no
host kernel a guest should get by default.

### 4.3 Download and verify

The [images.md §5](images.md#5-download-pipeline) pipeline handles each
file of a URL pull in turn (kernel, then initrd), with resume, retry and
the download limit as before. `qemu-img` is skipped, as for firmware.
Extracted files come from §10.3 instead. Either way:

- kernel ≤ 128 MiB and initrd ≤ 1 GiB (`GLIDEX_MAX_KERNEL_SIZE`,
  `GLIDEX_MAX_INITRD_SIZE`); a file over the limit fails the image;
- the kernel's format and architecture are checked from its header (K8),
  and `release` is read;
- a checksum mismatch on either file fails the image and removes both
  `.part` files.

### 4.4 Lifecycle

These are the same as firmware ([images.md §6.6](images.md#66-images-delete-and-inspect)),
with `VmConfig.kernel_image` in place of `firmware_image`:

- `409 conflict` (naming the VMs) when deleting a kernel image that VMs
  boot. Image responses list them as `used_by_vms`.
- A kernel image is never the source of a disk (`400 invalid_image`),
  and a disk image is never a VM's `kernel`.
- Ready ↔ Missing checks, events and `retry` apply unchanged, per file.

**Clusters** ([clustering.md](clustering.md)). The image cache
controller's `image_files` adds `("main", <id>.kernel)` and
`("initrd", <id>.initrd)`. `needs_image_here` also counts local VMs
whose `kernel_image` is the image. A VM's architecture (§9.2.1) is that
of its root image, else its firmware **or kernel** image. A kernel and a
root image of different architectures are refused with `400`, as
firmware is.

## 5. Backends

### 5.1 initramfs

`VmConfig.initrd_path: Option<String>` holds the image's `<id>.initrd`,
or an admin's host path. It is added to `check_paths`, with the same
quote and control-character rules and the systemd runner's `/tmp` rule.

| | argv |
|---|---|
| Cloud Hypervisor | `--kernel <k> [--initramfs <i>] --cmdline <args>` |
| QEMU | `-kernel <k> [-initrd <i>] -append <args>` |

### 5.2 Console (K5)

`hypervisor::ConsoleDevice::of(config) -> Serial | Virtio` takes the
last `console=` token of `kernel_args`. A value starting with `hvc`
means `Virtio`; any other value, or no `console=` at all, means
`Serial`. Firmware boot is always `Serial`.

| | Serial | Virtio |
|---|---|---|
| Cloud Hypervisor | `--console off --serial tty` | `--console tty --serial off` (today's kernel boot) |
| QEMU | `-serial stdio` (today) | `-serial none -device virtio-serial-pci,id=vser0 -chardev stdio,id=con0 -device virtconsole,chardev=con0` |

Existing VMs: their stored `kernel_args` already say `console=hvc0`
(Cloud Hypervisor) or `console=ttyS0` (QEMU), so each gets the device
it has today. `HypervisorType::default_kernel_args` becomes
`console=ttyS0 root=/dev/vda reboot=k panic=1` for **both** backends;
the hypervisor-specific default goes away.

The shim, the console proxy and the ring buffer don't change. They see
the hypervisor's stdio either way ([console.md](console.md#physical-io)).

### 5.3 Command line rules

`kernel_args` (K7) is checked at create and PATCH: at most 2047 bytes,
no control characters. It stays a single argv element. Neither backend
sees it through a shell, and neither parses commas in it.

## 6. VM integration

### 6.1 Request fields

`CreateVmRequest` (proto `glidex.vm.v1`) gains:

| Field | Meaning |
|---|---|
| `kernel` | Kernel image id or name: `ready`, not being deleted, and of the VM's architecture. Excludes `kernel_image_path`, `firmware` and `firmware_path`. |
| `initrd_path` | Host path (`useHostPath`). Only with `kernel_image_path`. |
| `generate_seed` | Optional bool, overriding the §6.3 rule. `true` with `cloud_init_path` is `400`. |

`VmConfig` gains `kernel_image: Option<String>` (id), `initrd_path`,
`generate_seed: Option<bool>` and `kernel_from_disk: bool` (K11);
`VmStatus` gains `disk_kernel` (§10.5, written by the VM controller). A
kernel image fills `kernel_image_path`
and `initrd_path` with its files, so backends see one set of fields
either way, as `firmware_image` fills `firmware_path`. `VmResponse`
gains `kernel` (the image id) and `boot` (`"firmware"` or `"kernel"`).

Admission, in addition to today's checks:

- exactly one of {firmware, kernel} after defaults (K2);
- `kernel`: `readImage` on the image, as for `firmware`;
- `kernel_args` omitted: the kernel image's `cmdline`, else the default
  (§5.2);
- a managed root disk with kernel boot and a `kernel_args` that has no
  `root=` gets `warnings: ["kernel_args has no root=; the kernel may not
  find the root disk"]`. Bare-rootfs VMs keep `root=/dev/vda` from the
  default.

### 6.2 Mutability (K6)

| Field | Takes effect |
|---|---|
| `kernel` | next launch, `RestartRequired=True` meanwhile. Must stay a kernel image, so the VM keeps kernel boot. |
| `kernel_from_disk` | next launch (K11). A `kernel` change without it in the mask turns it off. |
| `kernel_args` | next launch (unchanged) |
| `kernel_image_path`, `initrd_path`, `firmware`, `firmware_path`, `generate_seed` | never (`400`, "immutable field") |

PATCH `kernel` needs `updateVm` on the VM and `readImage` on the new
image. The old image's in-use mark goes with the spec. Its file stays
claimed until the running instance (if any) is gone, as with disks (D11).

### 6.3 Seed and credentials (K4)

`generate_seed` is fixed at create:

- `true` for firmware boot (today's rule), and for kernel boot whose
  root disk is managed (`image`, `root_disk`) or whose kernel is a
  kernel image;
- `false` for kernel boot with a host-path kernel and a host-path rootfs
  (today's behaviour);
- always `false` with `cloud_init_path`;
- overridable either way with the request field.

Records without the field load as `firmware_path.is_some() &&
cloud_init_path.is_none()`, which is exactly today's rule.

The VM controller generates the seed when `generate_seed` resolves to
`true` (`controller/vm.rs`, step 5.1), with the same contents, the same
`[root, data…, seed]` order and the same growpart handling. A VM with a
credential needs `generate_seed`. This replaces the "credential requires
firmware boot" check in `check_credential` (credentials.md *Use by VMs*).

## 7. Clients

- **Web UI** (`CreateVmForm`). **Direct kernel boot** gets the same boot
  disk sources as firmware (image / disk / path) and, when the disk is
  seeded, the credential picker. Instead of the firmware picker it has
  a **Kernel** choice: *A kernel image* lists ready kernel images of the
  root image's architecture, the one `for_image` the chosen image first
  ("— from this image"), with an *Extract it from the image* button when
  there is none (`kernel_from`, then it waits for the image); *Kernel
  files on the host* (administrators, `host.paths`) shows kernel and
  initramfs path fields. The arguments field's placeholder is the
  kernel image's `cmdline`, used when left empty. *Follow the guest's
  kernel* sets `kernel_from_disk` (K11) for an image or disk root.
  **Images page**: a note that catalog pulls bring the image's kernel
  (K10); an *Extract kernel* button on disk images that have none;
  kernels are listed with their `release` and `for` image.
  **VM detail** (`VmKernel`): the kernel image, and when following the
  guest the booted and previous kernels (K12); a picker with *Use from
  the next start* (K6), the follow checkbox, and *Extract from its disk
  now* while stopped (§10.5).
- **gxctl.** `image pull <key> [--no-kernel]`, `image extract-kernel
  <image>`, `image pull <url> --kernel [--initrd-url U]
  [--initrd-sha256 H] [--cmdline S]`, `image pull --kernel-path P
  [--initrd-path P]` (host import); pulls wait through `Extracting` and
  print the kernel's release and command line. In `create`, the
  firmware prompt accepts `kernel` and then asks for a kernel image (the
  matched one is the default), or a path when `host.paths` is held,
  then an initramfs path, and whether to follow the guest's kernel.
  `kernel <vm> [--set <image>] [--follow on|off] [--extract]` shows and
  changes a VM's kernel (from the next start) and extracts the kernel on
  its disk.
- **REST/gRPC.** The `PullImageRequest` oneof gains `kernel_from` and
  `kernel_path`, and the request `with_kernel`. `IMAGE_KIND_KERNEL`,
  `IMAGE_STATE_EXTRACTING`.
  The fields above. `grpc.md` and `rest-api.md` tables are updated.

## 8. Security considerations

- **No new host access.** Managed kernels live in the image directory and
  are named by id. Host paths (`kernel_image_path`, `initrd_path`, and
  `kernel_path` on import) still need `useHostPath`. Paths into the image
  directory are still not "managed" (security.md §7.4).
- **Untrusted kernels harm only their own guest.** The hypervisor loads
  the kernel into guest memory: Cloud Hypervisor's loader (Rust) or
  QEMU's. A hostile kernel or initrd is a hostile guest, which is
  already the threat model. The size limits (§4.3) bound host memory and
  disk use. The header check (K8) refuses files that aren't kernels
  before any VM uses them.
- **Supply chain.** An extracted kernel is exactly as trusted as the
  disk image it came from: a catalog image is checked against the
  vendor's checksum, and its kernel records that image's digest. URL
  pulls without a digest are `verified: false`, as for any image. The
  extractor itself is pinned by digest, like firmware (§10.1).
- **Untrusted filesystems stay in a VM** (K9). A disk image, especially
  from a URL or a VM's own disk (§10.5), may hold a filesystem crafted to
  attack its parser. Only the extractor's kernel parses it, inside KVM,
  with no network, the image read-only and the output a fixed-size raw
  file. The host parses only §10.3's stream, with every length, count
  and name checked before use. The worst a hostile image achieves is a
  failed extraction, or a kernel image that boots only its own guest.
- **Command line.** Whoever creates a VM, or may `updateVm` it, controls
  its kernel command line (`init=/bin/sh` and the like). This is the
  guest's own business, as root access in the guest already is. It
  never reaches a host shell.
- **Credentials** reach a kernel-boot guest only through the generated
  seed, the same 0600 file in the VM's runtime directory.

## 9. Settled questions

| Question | Answer |
|---|---|
| Should a catalog pull also bring the image's matching kernel? | Yes (K10): `with_kernel`, on by default for catalog pulls. Extraction reads the kernel from the image itself, so it can never be a mismatched build, and Ubuntu pruning old `unpacked/` directories (F-K4) no longer matters. |
| May a VM's kernel change after create? | Yes (K6): at the next launch, `RestartRequired` meanwhile. Firmware ↔ kernel boot stays immutable. |
| Which distros? Can we take the kernel out of the cloud image? | Yes, all of the catalog (F-K6, F-K7), with the extractor VM (K9, §10). Rejected: host tools per filesystem (`debugfs` for ext4, `btrfs restore`; nothing for XFS) parse untrusted filesystems on the host; libguestfs does it in a VM too, but adds a large host dependency whose appliance needs a readable host kernel; downloading distro kernel packages gives no initrd. |
| Kernel boot as the default for image VMs? | No (K2). Revisit once boot-time numbers exist. |
| Should a VM follow the kernels its guest installs? | Opt-in (K11), with every version kept while a VM uses it (K12). |
| Where does the extractor's own kernel come from? | Debian, pinned from snapshot.debian.org (K13). The exact module set is confirmed in K3. |

## 10. The extractor

### 10.1 Appliance

The extractor is a pinned kernel and initramfs (`images/extractor.rs`),
built on a node the first time it extracts and kept in the image
directory's `.extractor/<release>/` (`kernel`, `initrd`, `0444`). It is
not an image record: it is never offered to VMs, never copied between
nodes (each node builds the same bytes from the same pins), and a
release bump is a new directory.

- **Source (K13):** Debian trixie's **cloud** kernel package
  `linux-image-6.12.107+deb13-cloud-amd64` (34 MB, against 108 MB for the
  generic flavour) and `busybox-static` 1.37.0-6+b9, from
  `snapshot.debian.org`. The packages and every file taken from them
  (the kernel, twelve modules, busybox) are sha256-pinned and unpacked
  with `firmware::extract_deb`, so the archive's pool dropping the
  version never matters.
- **Initramfs:** glidex writes a `newc` cpio itself: `/bin/busybox`,
  `/init` (`extractor-init.sh`) and the modules, decompressed (they ship
  as `.ko.xz`; busybox `insmod` loads them in order). The modules are
  those this build doesn't have built in: `virtio_blk`, `xfs`, `btrfs`,
  `vfat` and `fat`, NLS `cp437`, `iso8859-1`, `utf8` and `ascii` (FAT's
  default iocharset is a module here), and their dependencies
  (`libcrc32c`, `xor`, `raid6_pq`). Debian ships no `modules.dep` (it is
  made at install time), so the load order is pinned and each module's
  own `depends=` is checked against it when the appliance is built. ext4
  is built in. 13 MB in all.
- **Architectures:** x86_64 only so far; an aarch64 pin is a table entry
  (`EXTRACTORS`). Without one, `kernel_from` is refused and catalog pulls
  don't extract.
- `GLIDEX_KERNEL_EXTRACTION=off` turns extraction off on a host (an
  air-gapped one, or tests).

### 10.2 Run

The image controller runs the extractor on a node holding a `Ready`
copy of the disk image, under the extraction limit (one at a time per
node by default, `GLIDEX_MAX_EXTRACTIONS`). It holds the disk image
(`ImageHold`) for the run, so it can't be deleted meanwhile.

- **As a VM instance, not a child process.** The installed control
  plane's sandbox closes `/dev` and sets `no_new_privs`; hypervisors only
  run in `glidex-vm@` units. So the run is a one-shot **job** of the
  shim: `launch.json` with `job: true` (`instance id extract-<image id>`)
  makes the shim report `Running` as soon as the hypervisor starts,
  without waiting for an API socket, and its exit is recorded as a VM's
  (`clean_exit` on status 0). The control plane starts it through the
  runner, polls `instance.json`, kills it after 120 s, then releases the
  shim and removes the instance directory. Startup's orphan scan leaves
  `extract-*` units and directories alone.
- The preferred hypervisor that is installed (Cloud Hypervisor, else
  QEMU), 1 vCPU, 1 GiB, no NIC, no API socket, no seed.
- Disk 0: the image's file, read-only (QEMU: `backing` null).
  Disk 1: `<image dir>/.<kernel image id>.extract.raw`, sparse, 2 GiB,
  `0600`, removed afterwards.
- Kernel command line `console=ttyS0 panic=-1 quiet`; the console log's
  last 4 KiB go into the image's failure reason.

### 10.3 Output stream

The extractor writes one stream to disk 1:

```
GXFILE <partition>:<path> <size>\n<size bytes>
…
GXEND\n
```

It writes, for every partition it can mount read-only: each
`vmlinuz-*`, each `initrd.img-*` and `initramfs-*.img` (no `*rescue*`),
`grub.cfg` files and BLS entries (`loader/entries/*.conf`), at most four
levels deep. A path with a space is skipped: the header is
space-separated. The host reads the stream sequentially and refuses it
(image `Failed`, with the reason) on: a malformed header line, a name
with `..`, a NUL or a control character, more than 64 files, a file over
the K8 size limits, or a total over 2 GiB. It never follows a name to a
path: files are kept in memory or written under the image's id.

### 10.4 Choosing the kernel and its command line

- **Kernel:** the highest version among `vmlinuz-<v>` that has an initrd
  for the same `<v>` (`initrd.img-<v>` or `initramfs-<v>.img`), compared
  segment by segment, numbers numerically. Then K8's header check, and
  `kernel_release` must equal `<v>`.
- **Command line:** the BLS entry for `<v>` (`options`), else the first
  `grub.cfg` `linux` line naming `vmlinuz-<v>`. glidex drops
  `BOOT_IMAGE=`, every `console=` and `earlyprintk=`, and appends
  `console=ttyS0`. No `root=` is found → `Failed` ("no boot
  configuration names the root"); the files are not kept.
- `root=UUID=…`, `PARTUUID=…` or `LABEL=…` stay valid for every disk
  cloned from the image: linked and full clones and offline root growth
  keep the identifiers (F-K7 booted a linked overlay).

### 10.5 From a VM's own disk

Both forms read the VM's managed root disk on a node that reaches it
(its node on a node-local datastore; any node on a shared one,
[storage.md](storage.md) §5), never while it is being moved (S5):
read-only, with `backing_files=on` (or QEMU's qcow2 backing) for a
linked disk glidex made, and the disk busy for the run, so the VM can't
start meanwhile. The result is a kernel image the VM owns (`owner`,
source `extracted` with `disk_id`), never `verified`: the guest writes
that disk.

**On demand.** `POST /vms/{id}/extract-kernel` (`updateVm`; RPC
`ImageService.ExtractVmKernel`) answers `202` with a new kernel image,
`extracting`; poll it, then PATCH the VM's `kernel` to it. The VM must
be stopped (`409` otherwise), boot a kernel image and have a managed
root disk (`400`); in a cluster, a server on a node that reaches the
disk must answer (`409` names the disk otherwise), and a disk being moved
is `409` too. The image is kept even if it duplicates a
stored one: the caller holds its id.

**Every launch (K11, opt-in).** `VmConfig.kernel_from_disk` (default
`false`; next launch) needs a kernel image (`kernel`) and a managed root
disk (`400` otherwise). Before binding the disks for a launch, the VM
controller:

1. Does nothing while the root disk isn't made or is busy: the disk check
   holds the launch, as for any VM.
2. Keeps what it boots when the root disk's file has the same size and
   modification time as at the last extraction, and that kernel image is
   still usable. A VM that was only stopped and started pays nothing.
3. Otherwise runs the extractor (seconds, F-K9). A result whose kernel
   and initramfs digests match a stored kernel image uses that one, and
   the new record is dropped (K12). The VM records what it boots in its
   **status**, not its spec: `status.disk_kernel = {image, previous,
   disk_stamp}`, shown as `booted_kernel` and `previous_kernel`. A change
   logs the event `KernelUpdated` (old → new release). The spec's
   `kernel` stays what the user chose: the kernel until the first
   extraction, and the fallback.
4. If the extraction fails (or finds no kernel), boots the kernel it has
   anyway: condition `KernelExtracted=False` with the reason, event
   `KernelExtractFailed`; the next launch tries again. Only a VM with no
   kernel at all fails to launch (`Ready=False/KernelNotExtracted`).

Why status: the controller writing the spec would bump the generation
and fight `RestartRequired`, and the user's choice would be lost.

**Versions (K12).** A VM uses an image while its spec names it
(`firmware`, `kernel`) or its status boots it or keeps it as `previous`
(`Vm::uses_boot_image`): such an image can't be deleted (`409`), and the
image cache copies it to the VM's node. A kernel image owned by a VM is
deleted by the image controller once no VM uses it, at least five
minutes after it was made (so a new one is never lost before its VM
records it). So guests run different versions side by side, each kept
while one VM needs it. Going back is a PATCH of `kernel` (to the
`previous_kernel`, say): changing `kernel` without `kernel_from_disk` in
the mask turns following off, and the VM boots the spec's kernel again.

## 11. Testing

- **Unit (backends).** Argv for kernel boot with and without initrd, for
  both backends. `ConsoleDevice::of` cases: `console=ttyS0`, `console=hvc0`,
  both orders, `console=ttyS0,115200n8`, none. QEMU virtconsole argv.
  Existing-record defaults keep today's argv. The full command line parsed
  by the pinned Cloud Hypervisor (as
  `cloud_hypervisor_parses_the_full_command_line`) with `--initramfs`.
- **Unit (images).** Kernel header checks: bzImage, ELF, arm64 `Image`,
  junk, wrong architecture, `release` extraction. The §10.3 stream
  parser on malformed and oversized streams, `..` and control characters
  in names, too many files. §10.4's choice of kernel and command line on
  the four layouts of F-K6 (fixtures from their boot configs).
- **API (`tests/image_tests.rs`).** URL pull of a kernel (fixture bzImage
  header) with initrd, a digest mismatch on the initrd, size limits,
  host import needs `host.paths`, deletion is refused while a VM uses
  the kernel, and a catalog pull with `with_kernel` records the kernel
  image. A failed extraction leaves the disk image `Ready`.
- **API (`tests/vm_tests.rs`).** `kernel` + `firmware` → `400`. `kernel` +
  `image` boots in kernel mode with the kernel's `cmdline`. The
  `generate_seed` rule table and old records. A credential on kernel boot
  is accepted. PATCH `kernel` sets `RestartRequired`. `initrd_path` needs
  `useHostPath`. `kernel_args` limits.
- **Functional (`#[ignore]`d, network).** `kernel_boot_with_generated_cloud_init`:
  pull `GLIDEX_TEST_CATALOG` (default `ubuntu-26.04`; also run with
  `debian-13`, `fedora-cloud` and `alma-9`) with its kernel, create with
  `image`, a credential and a NIC, boot under each backend, log in on
  the console, check the address, `uname -r` equal to the kernel's
  `release`, and the grown root. This is the end-to-end form of F-K3 and
  F-K7.
- **UI e2e.** `create-form.spec.ts`: the kernel picker, prefilled
  arguments, host-path fields only with `host.paths`.
  `vm-lifecycle.spec.ts`: a kernel-boot variant of the lifecycle test,
  where the host has the images.
- **K1/K2 tests.** Unit: `images::kernel::tests` (bzImage with release,
  64-bit entry required, bad version pointers, ELF x86_64/aarch64, arm64
  `Image`, junk), `download::tests::kernel_names_keep_the_release`. API
  (`tests/image_tests.rs`): `kernel_images_are_downloaded_and_checked`
  (URL pull with initramfs, digests, `0444` files, default name, a bad
  initramfs digest and a non-kernel fail the image and leave no `.part`,
  kernel-only fields refused, never a disk source, deleted with both
  files), `kernel_images_from_host_files`, `vms_boot_kernel_images` (no
  firmware needed, the kernel's command line by default, a credential,
  the one-boot-mode rules, the `root=` warning, `409` while in use and
  `used_by_vms`, PATCH `kernel`, refused on a firmware VM and to a
  firmware image, `generate_seed` immutable).
- **K2 host test (`#[ignore]`d, `kernel_image_boot_with_generated_cloud_init`
  and its `qemu_` twin).** A kernel image copied from
  `GLIDEX_TEST_KERNEL`/`GLIDEX_TEST_INITRD` with
  `root=LABEL=cloudimg-rootfs ro console=ttyS0`, the image
  `GLIDEX_TEST_IMAGE` as `rootfs_path`, and a stored credential: boot,
  wait for cloud-init, log in on the console, check `uname -r` is the
  kernel image's release and `sudo` works, stop on the power button
  ("reboot: Power down").

  Done on 2026-10-19 with Ubuntu 26.04 build `20260927` (kernel
  `7.0.0-34-generic`) on Cloud Hypervisor v53.0 and QEMU: both passed in
  68 s together.
- **K3 tests.** Unit: `images::extractor::tests` (stream round trip and
  refusals: no end, `..`, control characters, spaces, sizes per kind,
  too many files; version order; the four catalog layouts of F-K6 from
  their real boot configs in `tests/fixtures/boot/`; newest kernel with
  an initramfs; failures without initramfs, config or `root=`; `newc`
  layout; `depends=`; pins), `images::extract::tests`, the extractor
  argv for both backends (Cloud Hypervisor parses it), and the shim's
  `a_job_records_its_exit`. API: `kernel_extraction_requests` (with
  extraction off: `kernel_from` refused with the reason, bad
  combinations refused, `with_kernel` pulls the disk anyway).
- **K3 host test (`#[ignore]`d, network: `extracted_kernel_boots_its_image`
  and its `qemu_` twin).** Pull `GLIDEX_TEST_CATALOG`, which also
  extracts its kernel through the pinned Debian appliance (built on the
  first run) as a shim job; check the kernel image is verified, records
  the image's digest, and has a `root=` command line; boot a VM from the
  image with that kernel, a generated seed and a credential; log in and
  check `uname -r`; stop; delete.

  Done on 2026-10-19, each from a fresh data directory (so including the
  appliance's download and build): Ubuntu 26.04 (`7.0.0-34-generic`,
  `vda13:`) on Cloud Hypervisor in 84 s and QEMU in 78 s; AlmaLinux 9
  (`5.14.0-687.53.1.el9_8`, XFS, BLS) in 61 s; Fedora 44
  (`6.19.10-300.fc44`, btrfs, `rootflags=subvol=root`) in 121 s; Debian
  13 (`6.12.113+deb13-amd64`, `root=PARTUUID=…`) in 105 s, all on Cloud
  Hypervisor.
- **K3b tests.** API: `kernel_from_disk_requests` (needs a kernel image
  and a managed root disk, at create and PATCH; a manual `kernel` turns
  it off, both in the mask keep it; `extract-kernel` refused on a
  firmware VM, an unknown VM, and where extraction is off). Host
  (`#[ignore]`d, network: `guest_kernel_updates_are_followed`): a VM
  from Ubuntu 26.04 with its extracted kernel and `kernel_from_disk`
  first boots that same kernel image (the disk's files match it); the
  guest runs `update-initramfs -u -k all`; the next start extracts a new
  kernel image the VM owns (same release, new initramfs digest),
  `booted_kernel` is it and `previous_kernel` the image's, the event
  `KernelUpdated` is logged, and the guest logs in; a PATCH of `kernel`
  back to the image's turns following off and the VM boots it. Done on
  2026-10-20 on Cloud Hypervisor in 146 s.
- **K0 host test (`#[ignore]`d, `kernel_boot_with_initrd` and
  `qemu_kernel_boot_with_initrd` in `tests/functional_tests.rs`).**
  `GLIDEX_TEST_KERNEL`, `GLIDEX_TEST_INITRD` and `GLIDEX_TEST_IMAGE` are
  host paths. The test creates a VM with `initrd_path` and
  `root=LABEL=cloudimg-rootfs`, starts it through the VM controller and
  shim, and expects the kernel's first line and systemd's banner on the
  console, first with `console=ttyS0` and then with `console=hvc0`.

  Done on 2026-10-19 with Ubuntu 24.04 build `20261017` (its
  `unpacked/` kernel `6.8.0-147-generic` and initrd, the qcow2 image as
  `rootfs_path`), Cloud Hypervisor v53.0 and the host's QEMU. Both
  backends passed on both console devices, including QEMU's new
  `virtconsole`, in 62 s. Not covered by K0: seed, NIC and login,
  which need K2.

## 12. Milestones

| # | Scope | Acceptance |
|---|---|---|
| **K0** backends *(done)* | `initrd_path` (host path), `ConsoleDevice` (K5) and the shared `console=ttyS0` default, `kernel_args` limits (K7), K2 conflict check | §11 backend unit tests; host boot of F-K3 through glidex with host paths. |
| **K1** kernel images *(done)* | `ImageKind::Kernel`, storage (datastore copies), URL pull and host import, verify (K8), lifecycle, image cache | §11 image unit and API tests. |
| **K2** VM integration *(done)* | `kernel`, `generate_seed` (K4), credentials, mutability (K6), arch; the UI keeps kernel images out of its disk-image lists and lists them under *Kernels* | §11 VM API tests; the functional test with a kernel image and a credential. |
| **K3** extraction *(done)* | The extractor appliance (§10.1, K13), runs as shim jobs (§10.2), stream (§10.3), choice (§10.4), `kernel_from`, `with_kernel` on catalog pulls (K10) | Stream and choice unit tests; the functional test on the catalog distros (§11). |
| **K3b** following the guest *(done)* | `POST /vms/{id}/extract-kernel` (§10.5; moved from K3: it makes VM-owned kernel images), `kernel_from_disk` (K11), owned kernel images and their cleanup, `previous_kernel` (K12) | `kernel_from_disk_requests` (API); `guest_kernel_updates_are_followed` (functional, §11). |
| **K4** clients and docs *(done)* | UI, gxctl, user guide (`vms.md`, `images.md`, `credentials.md`, `console.md`) | UI e2e; docs build. |

## 13. Changes to other specs (to apply on acceptance)

- **images.md:** §1 adds kernel images to the list. §2 adds the
  `.kernel`/`.initrd` files. §3 adds `ImageKind::Kernel`, the new
  fields and the `Extracting` state. §4 says a catalog pull brings the
  image's kernel (K10). §6.6 covers in-use kernels. §7 adds the `kernel`
  field and the K2 rule (replacing "a caller who brings a kernel keeps
  kernel boot"). §8 adds the REST rows. §1's non-goal "image building or
  customization" stays: extraction reads, never writes.
- **storage.md:** §6 names kernel images beside firmware.
- **hypervisors.md:** the boot-mode table becomes boot mode × console
  device (§5.2). Both argv listings get initramfs. *Default kernel args*
  becomes one shared default.
- **console.md:** *Physical I/O* says the device follows `console=`.
- **credentials.md:** *Use by VMs* says "generated seed", not "firmware
  boot".
- **data-model.md:** `VmConfig` fields, the `CreateVmRequest` defaults
  and the mutability paragraph.
- **reconciliation.md:** §7.3 mutability table. §8.2 adds the
  `--initramfs` row and changes the console row.
- **security.md:** §7.4 lists `initrd_path`, and `kernel_path` on
  import. The `POST /vms` row adds `readImage` on the kernel image. The
  `PATCH` row adds `kernel`.
- **clustering.md:** §9.2.1 VM architecture includes the kernel image.
- **cli.md, web-ui.md, rest-api.md, grpc.md:** §7.
- **README.md, SUMMARY.md:** this document's entry (added with the
  draft).
