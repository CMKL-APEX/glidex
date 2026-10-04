# Images and disks

Before this module, a VM's disk was whatever file the caller passed as
`rootfs_path`. The caller downloaded a distro cloud image, converted it to
raw and sized it by hand, and nothing stopped two VMs from booting the same
file. `rootfs_path` still works that way; this module adds two kinds of
managed object alongside it:

- **Images** are read-only base cloud images (qcow2) that glidex downloads
  from a built-in catalog or a URL the caller gives, then checks against a
  checksum.
- **Disks** are writable volumes that VMs boot from or attach. A disk is
  either blank or cloned from an image, and glidex can grow or shrink it and
  extend its root partition.
- **Firmware images** are images of kind `firmware`: the UEFI firmware a
  VM boots a cloud image through, built for one hypervisor, pulled from a
  firmware catalog or a URL ([§4.1](#41-firmware-catalog)). VMs name one
  with `firmware` instead of a host `firmware_path` ([§7](#7-vm-integration)).

Source: `crates/glidex-control-plane/src/images/`: `mod.rs` (records,
`ImageManager`, `ImageError`), `catalog.rs`, `firmware.rs`, `download.rs`, `disk.rs`,
`partition.rs` and `qemu_img.rs`. `VmManager` (`state.rs`) owns the
`ImageManager` and does every check that involves VMs. Since M3 of
[reconciliation.md](reconciliation.md#10-disk-image-and-network-controllers),
the API only *records* disk and image changes; the disk controller
(`controller/disk.rs`) and image controller (`controller/image.rs`)
carry them out.

## 1. Goals and non-goals

Goals:

1. `POST /images {"catalog": "ubuntu-26.04"}` leaves a verified image on
   disk without the user ever opening a browser.
2. Creating a VM from an image is a single call. The root disk is created,
   sized, attached and cleaned up for the caller.
3. Resizing never silently destroys data. Each operation either succeeds
   with every partition's contents intact or fails before it writes
   anything.
4. Everything runs as the control-plane user. No step needs root.

Non-goals:

- Shrinking filesystems. Shrinking only gives back space that no partition
  uses (see [§6.3](#63-shrink)).
- Disk snapshots, export/import, and image building or customization.
- Remote or shared storage (NFS, Ceph, iSCSI). Images and disks live on the
  local filesystem.
- Non-Linux guests, and images that are not cloud images (installer ISOs).
- Live resize of a disk attached to a running VM. This is v1; see
  [§11](#11-future-work).

## 2. Storage layout

```
~/.glidex/
├── glidex.db                     # ReDB: vms, credentials, images, disks
├── images/
│   ├── <image-id>.qcow2          # verified, chmod 0444
│   ├── <image-id>.qcow2.part     # in-flight download (firmware too)
│   ├── <image-id>.fd             # firmware image, chmod 0444
│   └── <image-id>.vars.fd        # its UEFI variable-store template (OVMF), 0444
└── disks/
    └── <disk-id>.qcow2           # chmod 0600
```

The two directories sit next to the database (so a test's temporary
database gets its own). `GLIDEX_IMAGE_DIR` and `GLIDEX_DISK_DIR` override
them. Both are created `0700` at startup. Raw disks are `<disk-id>.raw`.
Operations write temporaries as dot-files (`.<id>.create.qcow2`,
`.<id>.shrink.qcow2`, `.gx-edit*/`) in the same directory, so the final
rename never crosses filesystems.

**Invariant.** Files under `images/` and `disks/` are named only by id. The
user-chosen name exists only in the database. Why: renaming a record never
touches the file, and a name can never escape the directory through `..`
or `/`.

**Invariant.** A file in these directories that has no database record is
an orphan. `ImageManager::initialize` logs it and leaves it alone; it never
deletes it. A database record whose file is missing is marked missing
(image `status: "missing"`, disk `phase: "missing"`) and not deleted. Either side can be fixed by hand, and
glidex never destroys data because it guessed wrong.

## 3. Data model

New ReDB tables in the same file as `vms` (`VmStore::database()`). Each one
is keyed by id, and the value is serde-JSON, as in
[data-model.md](data-model.md#persistence-schema).

### `Image`

```rust
pub struct Image {
    pub id: String,                 // UUIDv4
    pub name: String,               // unique, user-chosen; defaults to catalog key
    pub kind: ImageKind,            // Disk (default: older records) | Firmware
    pub hypervisor: Option<HypervisorType>, // what a firmware image is built for
    pub source: ImageSource,
    pub status: ImageStatus,
    pub format: DiskFormat,         // detected after download; always Qcow2 once Ready
    pub virtual_size_bytes: u64,    // from `qemu-img info`
    pub file_size_bytes: u64,       // on-disk allocation
    pub sha256: String,             // of the stored file, hex
    pub arch: Arch,                 // x86_64 | aarch64
    pub created_at: u64,
    pub download: DownloadMeta,     // resolved URL, expected digest, ETag /
                                    // Last-Modified: what a resume needs
    pub retry_seq: u64,             // bumped by POST /images/{id}/retry (§5)
    pub applied_retry_seq: u64,     // the last retry the controller started
    pub deletion_requested_at: Option<u64>, // set by DELETE; the controller finishes it (§6.6)
}

pub enum ImageSource {
    Catalog { key: String, url: String, version: String },
    Url { url: String, expected_sha256: Option<String> },
    Firmware { key: String, url: String, version: String }, // url: download, or host file copied
}

pub enum ImageStatus {
    Downloading { received_bytes: u64, total_bytes: Option<u64> },
    Verifying,
    Ready,
    Failed { reason: String },
    Missing,
}
```

### `Disk`

```rust
pub struct Disk {
    pub id: String,                 // UUIDv4
    pub name: String,               // unique, user-chosen
    pub format: DiskFormat,         // Qcow2 (default) | Raw
    pub size_bytes: u64,            // virtual size, multiple of 1 MiB
    pub origin: DiskOrigin,
    pub attached_to: Option<String>,// VM id; at most one
    pub pending_growpart: bool,     // next generated seed grows root (§6.4)
    pub created_at: u64,
    // Controller fields (all serde-defaulted: older records load as Ready):
    pub phase: DiskPhase,           // Pending | Creating | Ready | Resizing | Missing | Failed
    pub create: Option<DiskCreateSpec>,   // {size_bytes?, extend_root?} until made
    pub resize: Option<ResizeSpec>,       // {size_bytes, extend_root?} not applied yet
    pub extend_root: Option<ExtendRootSpec>, // {mode, seq} (D19)
    pub applied_extend_root_seq: u64,
    pub owner: Option<String>,      // VM the disk was made for
    pub deletion_requested_at: Option<u64>,
    pub conditions: Vec<Condition>, // `Ready`
}

pub enum DiskOrigin {
    Blank,
    Image { image_id: String, mode: CloneMode },
}

pub enum CloneMode {
    Linked,   // qcow2 overlay with the image as backing file
    Full,     // standalone copy
}
```

The record stays flat (no `{meta, spec, status}` envelope): the request
fields (`create`, `resize`, `extend_root`, `owner`) are written by the
API, the rest by the disk controller. `size_bytes` is the actual size (0
until the file is made). Phases:

| `phase` | Meaning |
|---|---|
| `pending` | recorded; the file is made once the source image is `Ready` (`Ready=False/ImageNotReady` while it downloads) |
| `creating` | the controller is making the file |
| `ready` | the file exists; a resize or extend-root may still be pending |
| `resizing` | reserved; a running operation shows as `status: "busy"` instead |
| `missing` | the file is gone. Never recreated (the data is gone); back to `ready` if the file reappears |
| `failed` | creating it failed for good (`InvalidDisk`); see the `Ready` condition |

`DiskResponse.status` is the phase, except that a running operation
shows as `busy` (from the in-memory busy set, so a crash can never leave
a disk stuck at "busy") and a ready disk without its file as
`missing`.

`DiskFormat` (`qcow2 | raw`) lives in `images/qemu_img.rs` next to
`detect_image_type` and `ImageType`, which moved there from the
Cloud-Hypervisor backend so both backends share one magic-byte probe.

`VmManager` keeps both tables' records cached in memory and writes ReDB
first, as it does for VMs ([data-model.md](data-model.md#write-ordering)).

**Invariant.** A disk is attached to at most one VM at a time, and
`attached_to` is written in the same ReDB transaction as the VM record that
references it. Why: two guests writing one ext4 volume corrupts it, and
this keeps the two tables consistent if the control plane crashes.

**Invariant.** While a `Linked` disk exists, its backing image cannot be
deleted (`409 conflict`, naming the disks). This also covers a disk still
pending or being created: its record already names the image, and
`materialize` takes an `ImageHold` on the image before it writes the
overlay, dropping it only once the made disk is committed and cached. `delete_image` checks holds and linked disks under the same
lock. The image file stays `0444` so that a stray write fails loudly.

## 4. Image catalog

`images/catalog.rs` holds a static table compiled into the binary:

| Key | Distro | URL pattern (x86_64) | Checksum source |
|---|---|---|---|
| `ubuntu-26.04` | Ubuntu Resolute | `cloud-images.ubuntu.com/resolute/current/resolute-server-cloudimg-amd64.img` | `SHA256SUMS` alongside |
| `ubuntu-24.04` | Ubuntu Noble | `cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img` | `SHA256SUMS` |
| `ubuntu-22.04` | Ubuntu Jammy | `cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64.img` | `SHA256SUMS` |
| `debian-13` | Debian Trixie | `cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-amd64.qcow2` | `SHA512SUMS` |
| `debian-12` | Debian Bookworm | `cloud.debian.org/images/cloud/bookworm/latest/debian-12-generic-amd64.qcow2` | `SHA512SUMS` |
| `fedora-cloud` | Fedora Cloud | resolved from `fedoraproject.org/releases.json` | sha256 in `releases.json` |
| `alma-9` | AlmaLinux 9 | `repo.almalinux.org/almalinux/9/cloud/x86_64/images/AlmaLinux-9-GenericCloud-latest.x86_64.qcow2` | `CHECKSUM` |

Each entry also has an `aarch64` URL and the checksum file URL with its
algorithm. One parser reads both checksum line formats (GNU `sha256sum`
lines and BSD `SHA256 (file) = …`). Fedora's `releases.json` carries the
URL and sha256 of every build, so the `fedora-cloud` entry picks the
newest final (non-Beta) `Cloud_Base` qcow2 from it. Entries are filtered by
`std::env::consts::ARCH`, and `GET /images/catalog` lists only the ones
this host can boot.

**Why "current/latest" plus a published checksum, not pinned digests.**
The installer pins the firmware's digest because one tested build is what
we want ([installer.md](installer.md#uefi-firmware)). Cloud images are
rebuilt weekly with security fixes, so pinning would ship stale, vulnerable
guests. Instead the checksum file is fetched over HTTPS from the same
vendor host, and the image is checked against it. The `version` recorded in
`ImageSource::Catalog` identifies the build behind each image:

- Ubuntu: `serial=` from `current/unpacked/build-info.txt` (`20260927`);
- AlmaLinux: the dated file name in `CHECKSUM` with the same digest as the
  `…-latest…` file;
- Fedora: the file name from `releases.json`;
- Debian (no build file in `latest/`): the image's `Last-Modified` header.

**Invariant.** Every catalog URL is `https://`, and every entry has a
checksum source for both architectures. Both are checked by
`catalog::tests::every_entry_is_https_with_checksum_for_both_arches`. A
URL resolved at download time (Fedora) is checked again.

### 4.1 Firmware catalog

`images/firmware.rs` holds the firmware a VM boots cloud images through:

| Key | Hypervisor | Source |
|---|---|---|
| `cloudhv-edk2` | Cloud Hypervisor | `github.com/cloud-hypervisor/edk2/releases/download/ch-811ce5ea35/CLOUDHV.fd` (`CLOUDHV_EFI.fd` on aarch64), sha256 pinned |
| `ovmf` | QEMU (x86_64) | the host's `ovmf` / `edk2-ovmf` package: the first OVMF code image found ([hypervisors.md](hypervisors.md#firmware-boot-1)) |

`GET /images/firmware-catalog` lists the entries this host can use, with
`source` (`download` or `host`), the URL or host file, the pinned
`version`, `available` (a host entry needs its package; `hint` says which)
and `downloaded_image_id`. `POST /images {"firmware": "<key>", name?}`
pulls one as an image of kind `firmware`, named after the key by default:

- a `download` entry is fetched through the §5 pipeline and checked
  against its pinned digest. **Why pinned here, unlike cloud images:** one
  tested firmware build is what guests should boot, as the installer
  pins it ([installer.md](installer.md#uefi-firmware)); the two are bumped
  together. When the installer's `~/.glidex/CLOUDHV.fd` has that digest,
  it is copied instead and the image is `Ready` at once (offline
  installs);
- a `host` entry copies the code image and its variable-store template
  (`<id>.vars.fd`) into the image directory, `0444`, `Ready` at once.
  Not installed: `503 tool_unavailable`, naming the package.

A firmware file can also come from any URL:
`{"url": …, "sha256"?, "kind": "firmware", "hypervisor": "qemu"}`
(the same URL rules). Firmware downloads skip `qemu-img`: they must be
1 byte to 64 MiB, and are stored raw as `<id>.fd`. Without a template, a
QEMU VM maps such firmware with `-bios`.

**Auto-import.** At startup (after `VmManager::initialize`, in `main`),
`ImageManager::auto_import_firmware` pulls each entry that has a local
source: the installer's `CLOUDHV.fd` with the pinned digest, or an
installed OVMF. It never downloads. Each entry is marked done in the
`image_meta` table (`firmware-auto-import/<key>`) once it is imported,
or once a firmware image for its hypervisor already exists, and is never
imported again, so an image the admin deleted stays deleted. An entry
without a local source is not marked, and is tried again at the next
start (OVMF installed later). So a fresh install boots image VMs without
a manual pull.

**Invariant.** A firmware image is never the source of a disk
(`400 invalid_image`), and a VM only boots firmware built for its own
hypervisor.

### Arbitrary URLs

`POST /images {"url": "...", "sha256": "..."}` downloads any `https://`
URL. `sha256` is optional. Without it, glidex still computes and stores the
digest but cannot vouch for the file, and the response carries
`"verified": false`. `file://` and other schemes are rejected with
`400 invalid_image`. So are URLs whose host is, or resolves to, a loopback,
private (RFC 1918, ULA), link-local or CGNAT address. Names that do not
resolve are refused too. `GLIDEX_ALLOW_PRIVATE_IMAGE_URLS=1` allows private
addresses, and only those may then use plain `http://` (a LAN mirror).
Every redirect hop is checked the same way (`download::http_client`), so a
public URL cannot bounce to an internal one. Why: the control plane would
otherwise fetch internal endpoints on a caller's behalf (SSRF).

## 5. Download pipeline

`images/download.rs`, one Tokio task per image:

1. Insert an `Image` record with status `Downloading`, then return
   `202 Accepted` with the record. The client polls `GET /images/{id}`.
2. Stream the body with `reqwest` (feature `stream`) into
   `<id>.qcow2.part`. The file is created `0600` and the digest is
   computed as the bytes arrive (`sha2` crate; sha256 always, plus sha512
   for Debian). Progress is written to the in-memory record, and to ReDB at
   most once every 5 s. An interrupted stream (connection error, or fewer
   bytes than `Content-Length`) is retried up to 4 times, resuming with
   `Range` + `If-Range` when the server sent a strong `ETag` or a
   `Last-Modified` header. A `200` answer to a ranged request restarts from
   zero.
3. Compare the digest with the expected one. On a mismatch, delete `.part`
   and set the status to `Failed { reason: "checksum mismatch" }`.
4. Status `Verifying`: run `qemu-img info --output=json` on the `.part`
   file. Reject the image if any of these hold:
   - the format is neither `qcow2` nor `raw`;
   - a qcow2 image has a `backing-filename` (a downloaded file must not
     point at arbitrary host paths);
   - the virtual size is over `GLIDEX_MAX_IMAGE_SIZE` (default 64 GiB).
5. A `raw` download (some vendors publish `.raw` or `.img` raw files) is
   converted with `qemu-img convert -O qcow2` into a new `.part` file. The
   digest stored is that of the converted file. The upstream digest stays in
   `download.expected`.
6. `fsync`, `chmod 0444`, rename to `<id>.qcow2`, then set status `Ready`.

**Concurrency.** At most `GLIDEX_MAX_DOWNLOADS` (default 2) downloads run
at once, and the rest wait in a FIFO queue. Fetching a catalog key while an
image with that key is already `Downloading` returns the existing record
with `200`, so it never starts a second download.

**Restart.** `ImageManager::initialize` finds `Downloading` and `Verifying`
records, and the image controller resumes any such image whose download
task is gone (checked every resync). If the `.part` file exists and the server sent an `ETag` or
`Last-Modified` header at the start, it resumes with an HTTP `Range` request
and an `If-Range` header. Otherwise it deletes the `.part` file and starts
again. Either way the hash is recomputed over the whole file before it is
verified. A resumed download is never trusted on the strength of its tail
bytes alone.

**Cancel.** `DELETE /images/{id}` on a downloading image is a deletion
(§6.6): the image controller aborts the task, deletes the record and
removes the `.part` file. A failed image keeps its record (with the
reason) until it is deleted, so the failure stays visible.

**Retry** (D19). A failed image is not downloaded again on its own.
`POST /images/{id}/retry` bumps `retry_seq` and returns `202` with the
image; the image controller, seeing `retry_seq > applied_retry_seq`,
sets `applied_retry_seq`, removes any `.part`, sets `Downloading` and
starts the download (event `Retrying`). Retrying an image that is
already downloading is a no-op; a `ready` or `missing` one is
`400 invalid_image`.

**Ready ↔ Missing.** The image controller checks a `Ready` image's file
each round. Gone → `Missing` (event `FileMissing`); back → `Ready`
(event `FileFound`). A missing image is **never downloaded again**:
catalog URLs point at "current/latest", and a different file would sit
under the linked disks written against the old one. A missing or failed
image cannot be the source of a new disk (`409 conflict`).

Events (`Retrying`, `FileMissing`, …) are kept per image and read with
`GET /images/{id}/events`.

## 6. Disk operations

All disk I/O shells out to `qemu-img`, `qemu-io`, `sgdisk` and `growpart`,
through `images/qemu_img.rs` (`Command`, never a shell string, with an
explicit argv and `LC_ALL=C`). It runs on `spawn_blocking`.

Create, resize and extend-root are **asynchronous**. The API validates
the request (name, sizes, shrink minimum, quota, partition layout) and
fails it as before, then records it on the disk and returns; the disk
controller applies it (`controller/disk.rs`, [reconciliation.md
§10.1](reconciliation.md#101-disk)). Each applied operation:

- holds a `BusyGuard` for the disk while it runs, so a second operation
  on the same disk or deleting it gets `409 conflict`, and a VM launch
  waits (`DisksReady=False/DiskBusy`);
- runs only while no live instance has the disk open (no VM's
  `status.instance.disks` contains it, and no VM using it is being
  launched). Otherwise the request stays pending, with
  `Ready=False/ResizePending` or `ExtendRootPending`, and is applied once
  the VM stops. `VmManager::begin_disk_op` checks this and takes the
  guard while holding the VM-map read lock, so a launch cannot slip in
  between;
- works on a temporary file when it can (see each operation below) and
  renames it into place, so a crash leaves the old disk intact.

Each step is recorded as a disk event (`Created`, `Resized`,
`RootExtended`, `ExtendRootFailed`, `FileMissing`, `Released`, …), read
with `GET /disks/{id}/events`.

### 6.1 Create

`POST /disks`:

```json
{ "name": "web-1-root", "size_gib": 20, "image": "<image-id or name>", "clone": "linked" }
```

| Inputs | Command |
|---|---|
| no `image` | `qemu-img create -f qcow2 <tmp> <size>` (or `-f raw`, sparse) |
| `image`, `clone: linked` (default) | `qemu-img create -f qcow2 -b <image path> -F qcow2 <tmp> <size>` |
| `image`, `clone: full` | `qemu-img convert -O qcow2 <image> <tmp>`, then `qemu-img resize <tmp> <size>` |

- `size_gib` is optional when `image` is given. It defaults to
  `max(image virtual size, GLIDEX_DEFAULT_ROOT_GIB)` (default 10 GiB).
  When given, it must be at least the image's virtual size. A disk smaller
  than its image would cut off the image's partitions, so
  `400 invalid_disk` names the minimum.
- `size_bytes` may be given instead of `size_gib`, and is rounded up to a
  whole MiB.
- After creation, when the disk came from an image and is larger than it,
  [§6.4 extend root partition](#64-extend-root-partition) runs
  automatically with `mode: "offline"`, unless `"extend_root": false` is
  passed.
- The image may still be downloading: the disk is recorded `pending`
  (`Ready=False/ImageNotReady`) and made once the image is `Ready`. A
  `failed` or `missing` image is refused (`409 conflict`: retry it
  first). The size check against the image is made at admission when
  the image is ready, otherwise when the disk is made (then
  `phase: failed`, `InvalidDisk`).
- A `raw` disk cannot be `linked` (`400 invalid_disk`).

Response `201 Created` with a `DiskResponse` in phase `pending`. With
`?wait=<secs>` (at most 300) the response waits until the disk is made
(`201`), fails (`400 invalid_disk` or `500 image_error`), or the time
runs out (`202`). A crash while `creating` leaves only the disk's own
dot-file temporary, which the next round replaces.

**Why linked by default.** A linked clone of a 600 MiB Ubuntu image costs
about 200 KiB until the guest writes to it, so ten VMs from one image cost
one image. The cost is a dependency: the image cannot be deleted while
linked disks exist (§3). `full` is for disks that must outlive their image.

### 6.2 Grow

`POST /disks/{id}/resize {"size_gib": 40}` with a size above the current
one:

1. `qemu-img resize <disk> <new size>`. This is in place, because growing a
   qcow2 or raw file is a metadata or `ftruncate` operation that cannot
   lose existing data.
2. On a GPT disk, `sgdisk -e` moves the backup header to the new end of the
   disk (on a qcow2 disk, through the shadow file in §6.4). Without this,
   `growpart` refuses and the guest kernel warns about a corrupt GPT. Disks
   without a GPT are left alone: `sgdisk` on a blank or MBR disk would
   write a new GPT.
3. If `"extend_root": true` (the default for disks with
   `origin: Image`), run §6.4.

The new size is persisted after step 1 succeeds. If step 2 or 3 fails, the
disk keeps its larger size and the error says that the partition was not
extended. The operation can then be retried with
`POST /disks/{id}/extend-root`. If only the partition tools are missing,
the disk falls back to on-boot growth (§6.4). The outcome (`grown`,
`already_full`, `on_boot` or `skipped` for no partition table) and any
warnings are in the `Resized` event.

The request is recorded as `resize` (`pending_size_bytes` in the
response) and returns `202`; `?wait=<secs>` returns `200` once it is
applied or reported pending, `400 invalid_disk` if it no longer fits
(`ResizeInvalid`: the guest grew a partition since; the request stays
until replaced), or `202` on timeout. A disk being deleted, or not
`ready`, is refused (`409 conflict`).

### 6.3 Shrink

`POST /disks/{id}/resize {"size_gib": 8}` with a size below the current
one.

**Invariant.** Shrinking never cuts into a partition. The minimum size is
the end of the last partition, plus 33 sectors for the GPT backup header,
rounded up to 1 MiB. Below that, the request fails with `400 invalid_disk`
and the error includes `"min_size_bytes"`. glidex does not shrink
filesystems. A caller who needs a smaller root partition must shrink the
filesystem and partition inside the guest first, then shrink the disk.

Procedure:

1. Read the partition table: glidex parses GPT and MBR itself
   (`partition.rs`) from the first MiB of the disk (via `qemu-img dd` for
   qcow2). A disk with no partition table (a bare filesystem, or a blank
   disk the guest formatted whole) cannot be shrunk: `400 invalid_disk`,
   because the end of the filesystem is not known.
2. Check that the new size is at or above the minimum.
3. Copy to a temporary file (`cp --reflink=auto --sparse=always`), then
   shrink that copy: `qemu-img resize --shrink <tmp> <size>`, then, on GPT,
   `sgdisk -e` through §6.4's shadow file to rewrite the backup header.
4. `qemu-img check <tmp>` (qcow2 only). Rename it over the original.

Why the copy: `qemu-img resize --shrink` discards data without asking,
and a crash or a wrong minimum in place would be unrecoverable. The copy
costs disk space and time, which is cheap next to losing data.

A `Linked` disk can only shrink down to its backing image's virtual size,
because the overlay must cover every sector the backing file can supply.

### 6.4 Extend root partition

`POST /disks/{id}/extend-root {"mode": "offline" | "on-boot"}`. This grows
the root partition into free space at the end of the disk. The disk must
already be large enough (§6.2). It is a one-shot action (D19): the API
checks that the disk has a growable root partition (else
`400 invalid_disk`), sets `extend_root = {mode, seq: previous + 1}` and
returns `202` (`200` with `?wait` once applied or reported pending). The
controller applies it once `seq > applied_extend_root_seq` and no live
instance has the disk, then records `applied_extend_root_seq = seq`.

Finding the root partition: on the disk's GPT, the partition with the
Discoverable Partitions root type for the host arch:
`4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709` (x86-64) or
`B921B045-1DF0-41C3-AF44-4C6F280D3FAE` (aarch64). If there is none, the
physically last partition, provided it is a Linux filesystem type
(`0FC63DAF-…`; on MBR, type `0x83`). The root must be the physically last
partition on the disk, because only the last partition can grow into the
free space at the end. Otherwise `400 invalid_disk` names the partition
that is in the way. Partition *numbers* do not matter: Ubuntu's root is
partition 1 but sits after partitions 13–15 (`/boot`, BIOS boot, ESP)
(`partition::tests::ubuntu_layout_root_is_partition_one_at_the_end`).

**`offline`** (the default):

1. Get a raw view of the disk. A raw disk is edited in place. For a qcow2
   disk, `partition::edit` builds a *shadow*: a sparse raw file the size of
   the disk, holding copies of the disk's first and last MiB (read with
   `qemu-img dd`). Those windows hold everything the partition tools touch:
   the protective MBR, the primary GPT and the backup GPT.
2. `sgdisk -e <raw>` (GPT only), then `growpart <raw> <partnum>`. growpart
   only edits the partition table. Exit 1 with `NOCHANGE` means the root
   already fills the disk (`already_full`), not an error.
3. qcow2: check (`SEEK_DATA`/`SEEK_HOLE`) that the tools wrote nothing
   outside the two windows, and refuse to write anything back if they did.
   Then write each changed window into the disk with
   `qemu-io -c "write -s <file> <offset> <len>"`.

**Why a shadow file, not an export.** The first design exported the qcow2
disk as a raw file through FUSE (`qemu-storage-daemon --export type=fuse`).
On the development host `fusermount3` was refused even outside any
sandbox, and FUSE is just as often unavailable in containers and under
AppArmor. The shadow approach needs only `qemu-img`/`qemu-io`, works the
same for linked overlays (writes land in the overlay), and copies 2 MiB
whatever the disk size.

**`qemu-img dd` quirk.** Its `count=` is counted from the start of the
input, skipped blocks included (unlike dd(1)). It also exits 0 with an
empty output file when `count` is less than `skip`.
`qemu_img::read_region` passes the end block as `count` and checks the
length of what arrived. Without that, the tail window reads back empty and
an edit would write zeros over the last MiB.

This step does not resize the filesystem. Doing that offline needs the
partition as a block device, and so a loop or NBD device and root, or
libguestfs. The filesystem is grown inside the guest instead: stock cloud
images run cloud-init's `resizefs` module on every boot by default, and
glidex's generated seed sets `resize_rootfs: true` explicitly
([§10](#10-changes-to-existing-components)). The partition is already the
right size, so this is an ordinary online grow that ext4, xfs and btrfs
all support.

**`on-boot`:** changes nothing on disk. It sets a flag on the disk
(`pending_growpart: true`), and the generated cloud-init seed then adds:

```yaml
growpart: { mode: auto, devices: ["/"], ignore_growroot_disabled: false }
resize_rootfs: true
```

so the guest grows both the partition and the filesystem itself. This is
the fallback when `sgdisk`, `growpart` or `qemu-io` is missing, and the
only mode that works for VMs with a custom `cloud_init_path` (in which case
the user's own seed must enable growpart). `create_vm` warns in its
response (`warnings`) when a root disk has `pending_growpart` and the VM
uses a custom seed. The disk controller clears the flag (event
`RootGrownOnBoot`) once the claiming VM has booted a seed carrying the
grow (its `seed_growpart_seq ≥ applied_extend_root_seq`).

**Why both.** Offline mode makes the size the guest sees on first boot
predictable and does not depend on the guest's cloud-init version. On-boot
mode needs no host tools beyond `qemu-img`. Most catalog images would
manage with on-boot mode alone (Ubuntu and Debian enable growpart by
default), but some do not (AlmaLinux sets `growpart` off in
`/etc/cloud/cloud.cfg.d`).

### 6.5 Delete

`DELETE /disks/{id}`:

- refused with `409 conflict` while a VM claims it (its spec references
  it, or a live instance has it open; naming the VM). Detach it, or
  delete the VM, first. Also `409` while an operation runs on it;
- otherwise records `deletion_requested_at` and runs the disk controller
  at once, which deletes the record first, then the file. If the file
  cannot be removed, this is logged and the file stays behind as an
  orphan (§2). The call returns `204` once the record is gone (normally
  at once), or `202` with the disk (`deleting: true`) when an operation
  on it has to finish first.

### 6.6 Images: delete and inspect

- `DELETE /images/{id}` is refused with `409 conflict` while a `Linked`
  disk uses it as backing file (§3), a disk waits for it (`pending` or
  `creating` from it), or a clone from it is in progress. It works
  whether or not finished `Full` clones exist. Otherwise it records
  `deletion_requested_at` (persisted) and the image controller finishes
  it: abort a running download, remove the record, then the image file
  and its `.part`. A file that cannot be removed is logged and left as
  an orphan (§2). The call returns `204` once the image is gone (the
  normal case: at once), else `202` with the image (`deleting: true`);
  with `?wait=<secs>`, `200` once gone (`202` on timeout).
- While it is being deleted, an image cannot be the source of a new disk
  (`409`, "being deleted") or be retried; a pull with the same name is
  `409` ("is being deleted; try again in a moment"); a catalog pull does
  not reuse it and the catalog's `downloaded_image_id` ignores it. A
  stale writer (the download task, say) cannot clear the request or
  bring a deleted image back.
- A firmware image is refused (`409 conflict`, naming them) while VMs
  boot through it (`VmConfig.firmware_image`); `VmManager::delete_image`
  checks under the VM-map read lock, which `create_vm` holds for writing
  while it picks the image. Image responses list those VMs as
  `used_by_vms`.
- `GET /images/{id}` and `GET /disks/{id}` include `qemu-img info` output
  (virtual size, actual size, backing chain) and, for disks, the parsed
  partition table:
  `partitions: [{ number, start_bytes, size_bytes, type, is_root }]` and
  `free_tail_bytes`.

## 7. VM integration

`CreateVmRequest` gains these optional fields, alongside `rootfs_path`:

| Field | Meaning |
|---|---|
| `image` | Image id or name. glidex records a linked root disk named `<vm-name>-root`, sized `root_disk_size_gib` (or the §6.1 default), extended offline, and owned by the VM (`owner`). The image may still be downloading. |
| `firmware` | Firmware image id or name (§4.1) the VM boots through: `ready`, not being deleted, built for the VM's hypervisor. |
| `root_disk` | Id or name of an existing, unattached disk to boot from. |
| `data_disks` | Ids or names of extra unattached disks, attached in order after the root disk. |

Exactly one of `rootfs_path`, `image` or `root_disk` must be given; anything
else is `400 invalid_config`. With `image` or `root_disk` and no
`kernel_image_path`, the VM boots through firmware: `firmware`, or by
default the newest ready firmware image for its hypervisor
(`400 invalid_config` naming the catalog when there is none). A caller
who brings a kernel keeps kernel boot, so QEMU can boot managed disks
too. `firmware_path` (a host file, `useHostPath`) still works and can't
be combined with `firmware`. The root disk created for
`image` is named `<vm-name>-root` (the VM name with invalid characters
replaced by `-`), or `<vm-name>-<id prefix>` if that name is taken.

`VmConfig` records the firmware image as `firmware_image` (id) and its
file as `firmware_path`, so backends see one field either way; for QEMU
the VM controller also fills the non-persisted `firmware_vars_template`
from `<id>.vars.fd`. `VmResponse.firmware` is the image id. Both fields
are immutable.

`VmConfig` gains `root_disk: Option<String>`, `data_disks: Vec<String>`
(disk ids) and `owns_root_disk: bool`. `rootfs_path` is still filled in,
with the disk's file path, so the hypervisor backends see one field whether
or not the disk is managed. `start_vm` fills two non-persisted fields,
`root_disk_binding` and `data_disk_bindings` (`DiskBinding { path, format,
backing_files }`), from the disk records. Backends take a managed disk's
format from these and never probe it. `start_vm` refuses with `409` while
any of the VM's disks is busy, and with `500 image_error` when a disk's
file is missing. `VmResponse` shows `root_disk`, `data_disks` and, on
create, `warnings`.

**Invariant.** `create_vm` records the new disk (phase `pending`,
`owner` = the VM), sets `attached_to`, and saves the VM in a single ReDB
write transaction (`store::Commit`), under the VM-map write lock. No
file is written at create time; the disk controller makes it once the
image is `Ready`. Until every disk is `ready` and not busy, the VM
controller holds the launch with `DisksReady=False` (`DiskNotReady`,
`DiskBusy` or `DiskMissing`), so a VM created with `power: running` from
an image that is still downloading starts once the disk is made.

Deleting a VM with `owns_root_disk = true` deletes the root disk too
(the `vm.owned-disk` finalizer): the VM record, the owned disk's record
and the other disks' cleared `attached_to` go in one transaction, and
then the file is removed. Disks the VM only attached (`root_disk`,
`data_disks`) are detached and kept. `DELETE /vms/{id}?keep_disk=true`
keeps an owned root disk as a detached disk.

Attaching and detaching data disks on a stopped VM (config-only, the same
semantics as VFIO devices in [rest-api.md](rest-api.md)):
`POST /vms/{id}/disks {"disk": "<id>"}` and `DELETE /vms/{id}/disks/{disk}`.
Running or Paused VMs get `400 invalid_state` in v1.

## 8. REST API

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/images/catalog` | Catalog entries for this host's arch, each with `downloaded_image_id` if one exists |
| `GET` | `/images/firmware-catalog` | Firmware catalog entries (§4.1), each with `available`, `hint` and `downloaded_image_id` |
| `GET` | `/images` | List images |
| `POST` | `/images` | `{catalog, name?}`, `{firmware, name?}` or `{url, sha256?, name?, kind?, hypervisor?}`. Returns `202` and an `Image` (`200` with the existing record if that catalog or firmware key is already downloading) |
| `GET` | `/images/{id}` | Image, including download progress |
| `DELETE` | `/images/{id}[?wait=N]` | Delete, or cancel a download (§6.6). `204`, or `202` with the image while the controller finishes; with `wait`, `200` once gone. `409` while a disk uses it |
| `POST` | `/images/{id}/retry` | Download a failed image again (§5). Returns `202` |
| `GET` | `/images/{id}/events` | The image's events |
| `GET` | `/disks` | List disks |
| `POST` | `/disks[?wait=N]` | Create (§6.1). Returns `201` (pending); with `wait`, `201` once made or `202` on timeout |
| `GET` | `/disks/{id}` | Disk, including its partition table |
| `POST` | `/disks/{id}/resize[?wait=N]` | `{size_gib \| size_bytes, extend_root?}` (§6.2, §6.3). `202`; with `wait`, `200` once applied or pending |
| `POST` | `/disks/{id}/extend-root[?wait=N]` | `{mode}` (§6.4). `202`; with `wait`, `200` once applied or pending |
| `DELETE` | `/disks/{id}` | Delete. `204`, or `202` while an operation finishes; `409` while attached |
| `GET` | `/disks/{id}/events` | The disk's events |
| `POST` | `/vms/{id}/disks` | Attach a data disk (stopped VM) |
| `DELETE` | `/vms/{id}/disks/{disk}` | Detach a data disk (stopped VM) |

Image responses carry `kind` (`disk` | `firmware`), `hypervisor` (firmware
only), `used_by_vms` (firmware in use) and `deleting: true` while a
deletion is in progress (omitted otherwise).

For images and disks, `{id}` path segments (and the `image`, `root_disk`,
`data_disks` and `disk` request fields) accept an id or a unique name.
Names are 1–64 characters of `A-Z a-z 0-9 . _ -` and must not start with
`.`. VM routes still take ids only.

New error codes for the `ApiError` envelope (`api::image_error_response`):

| `ImageError` variant | HTTP | `error` |
|---|---|---|
| `NotFound` | `404` | `not_found` |
| `AlreadyExists`, `InUse`, `Busy`, `NotReady` | `409` | `conflict` |
| `InvalidImage` (bad URL, scheme, catalog key, size) | `400` | `invalid_image` |
| `InvalidDisk` (size, layout, shrink below minimum; `details.min_size_bytes`) | `400` | `invalid_disk` |
| `ToolMissing { tool, package }` | `503` | `tool_unavailable` |
| `Io`, `Tool { tool, stderr }`, `Download` | `500` | `image_error` |
| `Storage` | `500` | `persistence_error` |

`ToolMissing` is `503`, not `500`. Like `hypervisor_unavailable`, it
reports a host that is not set up, not a bug, and the message names the
package to install.

## 9. Host dependencies

| Tool | Package (apt / dnf / pacman) | Needed for |
|---|---|---|
| `qemu-img` | `qemu-utils` / `qemu-img` / `qemu-img` | everything |
| `qemu-io` | (same packages as `qemu-img`) | partition edits on qcow2 disks |
| `sgdisk` | `gdisk` / `gdisk` / `gptfdisk` | grow, shrink, extend-root on GPT disks |
| `growpart` | `cloud-guest-utils` / `cloud-utils-growpart` / `cloud-guest-utils` | extend-root offline |

`sgdisk` lives in `/usr/sbin`, which is not on every user's `PATH`;
`Tool::find` also looks in the sbin directories.

The control plane checks for each tool when it starts and prints what is
missing, as it does for the firmware ("Checking disk tools"). It does not
refuse to start. Operations that need a missing tool fail with
`503 tool_unavailable`, except that a missing `sgdisk`, `growpart` or
`qemu-io` makes extend-root (and the extend after a grow or create) fall
back to `on-boot` mode, saying so in `warnings`. Shrinking a GPT disk needs
`sgdisk` (and `qemu-io` for qcow2).

## 10. Changes to existing components

- **QEMU backend.** `-drive …,format=raw` used to be hard-coded in
  `qemu.rs`. The format now comes from the disk record for managed disks,
  and from glidex's own probe for a user-supplied `rootfs_path`. It is
  never left to QEMU's probing. Data disks get one `-drive` each, and
  commas in paths are doubled.
- **Cloud-Hypervisor backend.** Disks are `[rootfs, data_disks…, seed]`
  (`disk_configs`). The seed stays last, so guest device names for existing
  VMs (`vda` root, `vdb` seed) change only when data disks are added.
  Managed disks send their recorded `image_type`. Linked qcow2 disks need
  CH's `backing_files: true` disk option, which is set only for disks
  glidex created, never for a user-supplied `rootfs_path`. Why: CH refuses
  backing files by default because a qcow2 header can name any host file
  as its backing file. Downloaded images are checked to have none (§5),
  and a managed disk's format is never re-probed, so a guest that writes a
  qcow2 header into a raw disk cannot change how it is opened.
- **cloud-init seed** (`cloud_init.rs`). `user-data` always contains
  `resize_rootfs: true`. It also contains the `growpart` block from §6.4
  when the root disk has `pending_growpart`; the flag is cleared once the
  VM booted that seed (§6.4).
- **Installer.** Step 5 also installs the §9 tools, with the right
  package name per package manager (`DISK_TOOLS`).
  [installer.md](installer.md#no-sample-kernel--rootfs) now points at
  "`gxctl image pull ubuntu-26.04`" instead of "bring your own image".
- **Uninstaller.** `~/.glidex/images` and `~/.glidex/disks` are part of
  `~/.glidex`, which is removed only with `--purge-user-data`. The help
  text says so. These hold user data, unlike the seed files in the VM's runtime directory.
- **gxctl.** `image catalog` lists the firmware catalog too, `image list`
  shows each image's kind, and `image pull --firmware <key>` (or
  `<url> --firmware-for <hypervisor>`) pulls firmware; `create` asks for
  a firmware image instead of a path. New `image catalog|list|pull|retry|rm` and
  `disk list|show|create|resize|extend-root|rm` commands (the disk
  writes wait: create up to 300 s, resize and extend-root 120 s), and
  `delete <vm> --keep-disk`. `create` offers "image" as the boot disk
  source when images are downloaded ([cli.md](cli.md)).
- **Web UI.** A new Images page (cloud image and firmware catalogs with
  Pull buttons and progress bars, then downloaded images and firmware) and
  a Disks page; `CreateVmForm` picks a firmware image. `CreateVmForm` gets an
  image picker and a root-size field ([web-ui.md](web-ui.md)).
- **`Cargo.toml`.** `reqwest` gains the `stream` feature, and `sha2`,
  `futures-util` and `tempfile` become regular dependencies.

## 11. Future work

- Live grow of an attached disk: CH `PUT /vm.resize-disk`, QEMU
  `block_resize`, followed by an in-guest `growpart` over the guest agent.
- Offline filesystem grow through libguestfs, for guests without
  cloud-init.
- Periodic catalog refresh: pull a newer build and offer to rebase linked
  disks onto it (`qemu-img rebase`).
- Disk snapshots (`qemu-img snapshot`) and export.

## 12. Testing

Tests that need the §9 tools skip themselves, with a note, where those are
missing.

- **Unit (`images::*::tests`):** catalog invariants and the checksum,
  `releases.json` and `build-info.txt` parsers; root-partition selection on
  GPTs made with `sgdisk` (root type last, Ubuntu's layout, root not last,
  Linux-filesystem fallback, no Linux partition) and on MBR; the shrink
  minimum; a qcow2 shadow edit that grows a partition and keeps data inside
  it and in the tail window intact; URL rules and private address ranges;
  hashing.
- **API (`tests/image_tests.rs`):** a local axum server serves fixtures
  (strong `ETag`, `Range`/`If-Range`, and a `flaky-` path that drops the
  first response half way). Cases: download and verify; `0444` image;
  checksum mismatch leaves no partial file; a backing file is rejected at
  verify; resume after a dropped connection (exactly one ranged retry);
  raw → qcow2 conversion; linked and full disks with offline extension
  checked by `sgdisk -v` and a marker inside the root partition; `409` on
  deleting an image with linked disks; grow / shrink / extend-root on qcow2
  and raw, including shrink below the minimum (`min_size_bytes`) and below
  a linked disk's image; VMs from `image`, `root_disk` and `data_disks`;
  attach/detach; `keep_disk`; owned disk deleted with the VM; records
  surviving a restart; firmware images (`firmware_images`: URL download
  with `kind: firmware`, not a disk source, hypervisor match, in use
  while a VM boots through it; an image VM refused until one exists;
  `ovmf_firmware_is_imported_from_the_host_package`).
- **Functional:** `firmware_boot_with_generated_cloud_init` (and the QEMU
  one) boot through a firmware image pulled from the firmware catalog.
- **Functional (`#[ignore]`d, `catalog_image_boots_and_root_grows` in
  `tests/functional_tests.rs`):** pull `GLIDEX_TEST_CATALOG` (default
  `ubuntu-26.04`) from the vendor and create a VM with `image` and
  `root_disk_size_gib: 12`. Check that the root partition already fills the
  disk before first boot. Boot under Cloud-Hypervisor, log in on the
  console and check `/` is at least 11 GiB. Stop, grow to 16 GiB (expect
  `extend_root: grown`), boot again and check at least 15 GiB. Then delete
  everything.
