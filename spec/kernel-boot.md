# Direct kernel boot

Status: **draft**. Nothing here is implemented yet. Extends
[images.md](images.md), [hypervisors.md](hypervisors.md),
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
  (a kernel, an optional initramfs and a default command line), pulled
  from a catalog, a URL or the host, with the same verification,
  storage, in-use protection and cluster caching as firmware images. A
  kernel catalog matched to the cloud image catalog. Generated seeds and
  credentials on kernel boot. The console device follows the command
  line. UI, gxctl, REST/gRPC and docs.
- **Out:** taking the kernel out of a cloud image's own `/boot`. This
  needs a reader for ext4, XFS and btrfs inside qcow2, and without root
  access (no `qemu-nbd`, no loop mounts). §10 covers it as later work.
  Until then, only distros that publish their kernels separately get a
  catalog kernel (§3).
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
| K3 | Kernel/rootfs match | A catalog kernel is pulled **for a cloud image** (`kernel_for`) and must match that image's build exactly (`ImageSource::Catalog.version`). The guest loads modules from `/lib/modules/<release>` on its root disk, so a kernel from another build may find no modules. A newer build is never substituted silently. |
| K4 | Seed | The VM controller generates the cloud-init seed on kernel boot too. The rule is stored on the VM as `generate_seed` (§6.3), so existing kernel-boot VMs don't gain a disk on upgrade. Credentials require a generated seed, not firmware boot. |
| K5 | Console | The **last `console=`** on the command line picks the device: `hvc<n>` → virtio console, anything else (or none) → serial port. Both backends do the same. New VMs default to `console=ttyS0`, so early boot output and panics are visible under Cloud Hypervisor too. |
| K6 | Kernel updates | Kernel boot runs the kernel glidex hands to the hypervisor. A kernel the guest installs itself never boots. `kernel` may be changed on a stopped or running VM and takes effect at the next launch (`RestartRequired`), so a host-side update is a pull and a PATCH. Switching between firmware and kernel boot stays immutable. |
| K7 | Command line | `kernel_args` defaults to the kernel image's `cmdline`, else the hypervisor default. It is one argv element, at most 2047 bytes (x86 `COMMAND_LINE_SIZE` less the NUL), with no control characters (`400 invalid_config`). |
| K8 | Formats | Kernel: x86_64 bzImage (`HdrS` at 0x202) or ELF `vmlinux`; aarch64 `Image` (`ARM\x64` at 0x38) or EFI-stub `Image`. Anything else is refused at verify. An initramfs is not parsed; only its size is checked. |

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
  either. These distros have no catalog kernel until §10.

## 4. Kernel images

### 4.1 Data model

`ImageKind` gains `Kernel`. An `Image` of that kind has:

- `hypervisor: None`. A kernel is not tied to a backend (F-K1, F-K2).
- `arch` from the catalog entry or the request, checked against the
  kernel's header at verify (K8).
- `format`: unused, recorded as `Raw`. `virtual_size_bytes` is the
  kernel's size. `file_size_bytes` covers the kernel plus the initrd.
- `sha256`: the kernel file's digest.
- New fields, serde-defaulted:
  - `initrd: Option<FileMeta { sha256, size_bytes }>`;
  - `cmdline: Option<String>`, the default `kernel_args` (K7);
  - `release: Option<String>`, the kernel's version string from its
    header (bzImage `kernel_version` offset; ELF/`Image` don't carry one,
    so `None`);
  - `for_image: Option<ForImage { catalog_key, version }>`, the cloud
    image build this kernel matches (K3).

`ImageSource` gains `Kernel { key, url, initrd_url, version }` for
catalog pulls. URL pulls stay `Url` with the initrd URL in
`DownloadMeta`.

Storage ([images.md §2](images.md#2-storage-layout)):
`images/<id>.kernel` and `images/<id>.initrd`, `0444`, both named only
by id. The `.part` files follow the same rules.

### 4.2 Kernel catalog

`images/kernel.rs`. Each entry belongs to one cloud catalog entry:

| Key | For | URL (x86_64; arm64 likewise) | Checksum | cmdline |
|---|---|---|---|---|
| `ubuntu-26.04-kernel` | `ubuntu-26.04` | `cloud-images.ubuntu.com/resolute/<serial>/unpacked/resolute-server-cloudimg-amd64-{vmlinuz,initrd}-generic` | that directory's `SHA256SUMS` | `root=LABEL=cloudimg-rootfs ro console=ttyS0` |
| `ubuntu-24.04-kernel` | `ubuntu-24.04` | `…/noble/<serial>/unpacked/…` | same | same |
| `ubuntu-22.04-kernel` | `ubuntu-22.04` | `…/jammy/<serial>/unpacked/…` | same | same |

`GET /images/kernel-catalog` lists them with `for`, `available` and
`downloaded_image_id`, as the firmware catalog does. This is not a pinned
digest like firmware. The kernel follows the cloud image, which is
"current plus published checksum"
([images.md §4](images.md#4-image-catalog)). The checksum is fetched
over HTTPS from the same host.

**Pull.** `POST /images {"kernel_for": "<image id or name>", name?}`:

1. The image must be a `ready` or downloading **disk** image from a
   catalog entry that has a kernel entry. Otherwise the answer is
   `400 invalid_image`, listing the catalog keys that have one, and
   saying that firmware boot works for every image.
2. `<serial>` is that image's `ImageSource::Catalog.version`. Both files
   come from that serial's directory, never `current/` (K3). If the
   directory is gone (F-K4, pruned), the image is marked `Failed`
   ("kernel for build <serial> no longer published; pull a newer image
   and its kernel, or use firmware boot").
3. Default name `<image name>-kernel`. A second pull for the same image
   while one is downloading returns the existing record (`200`).

`POST /images {"kernel": "<catalog key>"}` pulls `current/`, for a
`rootfs_path` or a disk whose build glidex doesn't know. The response
carries `warnings: ["not matched to a cloud image build"]`.

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
file in turn (kernel, then initrd), with resume, retry and the download
limit as before. `qemu-img` is skipped, as for firmware. Then:

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

`hypervisor::console_device(kernel_args) -> Serial | Virtio` takes the
last `console=` token. The `console=` value up to the first `,` (or the
whole value) starting with `hvc` means `Virtio`; any other value, or no
`console=` at all, means `Serial`. Firmware boot is always `Serial`.

| | Serial | Virtio |
|---|---|---|
| Cloud Hypervisor | `--console off --serial tty` | `--console tty --serial off` (today's kernel boot) |
| QEMU | `-serial stdio` (today) | `-serial none -device virtio-serial-pci,id=vser0 -chardev stdio,id=con0,signal=off -device virtconsole,chardev=con0` |

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

`VmConfig` gains `kernel_image: Option<String>` (id), `initrd_path`, and
`generate_seed: Option<bool>`. A kernel image fills `kernel_image_path`
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

- **Web UI** (`CreateVmForm`). **Direct kernel boot** gets the same root
  disk sources as firmware (image / disk / path) and the credential
  picker. Instead of the firmware picker it shows a **Kernel** picker:
  kernel images of the root image's architecture, preferring the one
  `for_image` the chosen image, with a "Pull kernel for this image"
  button when there is none. **Host path…** shows kernel, initrd and
  arguments fields, only with `host.paths`. The arguments field is
  prefilled with the kernel's `cmdline`. **Images page**: a kernel catalog
  section, and kernels listed with their `release` and `for` image.
  **VM detail**: boot mode and kernel.
- **gxctl.** `image pull --kernel-for <image>`, `image pull --kernel
  <key>`, `image pull <url> --kernel [--initrd-url U]
  [--initrd-sha256 H] [--cmdline S]`. In `create`, the firmware prompt
  accepts `kernel` and then asks for a kernel image (the matched one is
  the default), or a path when `host.paths` is held. Initrd is a prompt
  after the kernel path. `vm set <vm> --kernel <image>`.
- **REST/gRPC.** `GET /images/kernel-catalog`. The `PullImageRequest`
  oneof gains `kernel_for`, `kernel` and `kernel_path`. `IMAGE_KIND_KERNEL`.
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
- **Supply chain.** Catalog kernels are checked against the vendor's
  `SHA256SUMS` over HTTPS, the same trust as the cloud image they match.
  URL pulls without a digest are `verified: false`, as for any image.
- **Command line.** Whoever creates a VM, or may `updateVm` it, controls
  its kernel command line (`init=/bin/sh` and the like). This is the
  guest's own business, as root access in the guest already is. It
  never reaches a host shell.
- **Credentials** reach a kernel-boot guest only through the generated
  seed, the same 0600 file in the VM's runtime directory.

## 9. Open questions

| Question | Leaning |
|---|---|
| Should `POST /images {"catalog": …}` take `with_kernel: true` and pull the matched kernel too? | Yes: one call for UI "Pull" with a checkbox. Cheap once §4.2 exists. |
| Ubuntu prunes old serials (F-K4). Mirror the matched kernel at image pull time to be safe? | Covered by `with_kernel`. Otherwise the pull fails with guidance (§4.2), never a mismatched kernel. |
| Offer kernel boot as the **default** for image VMs whose image has a matched kernel (faster boot, no UEFI)? | No (K2). Revisit once boot-time numbers exist. |
| Should a guest `apt upgrade` be able to tell the host about a new kernel? | Out of scope. K6 makes the host-side update one PATCH. |

## 10. Later: kernels from the image itself

Debian, Fedora and AlmaLinux publish no separate kernels (F-K5). A
`{"kernel_from": "<image>"}` pull would read `/boot/vmlinuz-*` and the
matching initrd from the root partition (found as in
[images.md §6.4](images.md#64-extend-root-partition)). It would have to
do so as the control-plane user and without loop devices, using
`debugfs -R dump` (e2fsprogs) on a temporary raw copy for ext4. Debian
uses ext4; Fedora (btrfs) and Alma (XFS) need another reader,
libguestfs being the general one. This is gated on picking that reader,
and it has to treat the image's filesystem as untrusted input.

## 11. Testing

- **Unit (backends).** Argv for kernel boot with and without initrd, for
  both backends. `console_device` cases: `console=ttyS0`, `console=hvc0`,
  both orders, `console=ttyS0,115200n8`, none. QEMU virtconsole argv.
  Existing-record defaults keep today's argv. The full command line parsed
  by the pinned Cloud Hypervisor (as
  `cloud_hypervisor_parses_the_full_command_line`) with `--initramfs`.
- **Unit (images).** Kernel header checks: bzImage, ELF, arm64 `Image`,
  junk, wrong architecture, `release` extraction. Kernel catalog
  invariants (https, a checksum source per architecture, every entry's
  `for` exists in the cloud catalog).
- **API (`tests/image_tests.rs`).** URL pull of a kernel (fixture bzImage
  header) with initrd, a digest mismatch on the initrd, size limits,
  `kernel_for` on a fixture catalog image picks that build's directory
  and fails cleanly when it is gone, host import needs `host.paths`, and
  deletion is refused while a VM uses the kernel.
- **API (`tests/vm_tests.rs`).** `kernel` + `firmware` → `400`. `kernel` +
  `image` boots in kernel mode with the kernel's `cmdline`. The
  `generate_seed` rule table and old records. A credential on kernel boot
  is accepted. PATCH `kernel` sets `RestartRequired`. `initrd_path` needs
  `useHostPath`. `kernel_args` limits.
- **Functional (`#[ignore]`d, network).** `kernel_boot_with_generated_cloud_init`:
  pull `ubuntu-26.04` and `kernel_for` it, create with `image`, a
  credential and a NIC, boot under each backend, log in on the console,
  check the address, `uname -r` equal to the kernel's `release`, and the
  grown root. This is the end-to-end form of F-K3.
- **UI e2e.** `create-form.spec.ts`: the kernel picker, prefilled
  arguments, host-path fields only with `host.paths`.
  `vm-lifecycle.spec.ts`: a kernel-boot variant of the lifecycle test,
  where the host has the images.

## 12. Milestones

| # | Scope | Acceptance |
|---|---|---|
| **K0** backends | `initrd_path` (host path), `console_device` (K5) and the shared `console=ttyS0` default, `kernel_args` limits (K7), K2 conflict check | §11 backend unit tests; host boot of F-K3 through glidex with host paths. |
| **K1** kernel images | `ImageKind::Kernel`, storage, URL pull and host import, verify (K8), lifecycle, image cache | §11 image unit and API tests. |
| **K2** VM integration | `kernel`, `generate_seed` (K4), credentials, mutability (K6), arch | §11 VM API tests; the functional test with a URL-pulled kernel. |
| **K3** kernel catalog | `GET /images/kernel-catalog`, `kernel_for`, `kernel`, `with_kernel` if accepted | Catalog unit tests; the functional test as written. |
| **K4** clients and docs | UI, gxctl, user guide (`vms.md`, `images.md`, `credentials.md`, `console.md`) | UI e2e; docs build. |

## 13. Changes to other specs (to apply on acceptance)

- **images.md:** §1 adds kernel images to the list. §2 adds the
  `.kernel`/`.initrd` files. §3 adds `ImageKind::Kernel` and the new
  fields. §4.2 links here for the kernel catalog. §6.6 covers in-use
  kernels. §7 adds the `kernel` field and the K2 rule (replacing "a
  caller who brings a kernel keeps kernel boot"). §8 adds the REST rows.
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
