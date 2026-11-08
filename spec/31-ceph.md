# Ceph RBD datastores

Status: **design** (§11 milestones). Open questions Q1–Q4 (§10) need
the owner's answer before code. Extends [24-storage.md](24-storage.md)
(datastores, the driver trait §7, `glidex-stord` §11.1, exclusive
access §10), and touches [04-hypervisors.md](04-hypervisors.md) (block
disks), [05-installer.md](05-installer.md) (stord's allow-list) and
[13-metering.md](13-metering.md) (`disk.stored`).

Today glidex reaches Ceph only as CephFS, mounted by the operator and
registered as a `shared-dir` datastore. Disks there are qcow2 files on a
filesystem, with CephFS's metadata path in every write, and nothing in
Ceph keeps two nodes from opening one disk: glidex's own file lock
(24-storage.md S16) does that.

This document adds the **`rbd` driver**: a datastore is a pool (and,
optionally, a RADOS namespace) of an existing Ceph cluster, and each
disk is an RBD image in it. Glidex creates, clones, grows and deletes
the images, maps each one on the node that runs its VM, and hands the
device to the hypervisor. The topology is shared: a VM on RBD disks can
be placed on any node that reaches the cluster, and placed again after
its node is lost. RBD's exclusive lock and Ceph's client blocklist give
the fencing that 24-storage.md §10 point 4 left for later.

## 1. Scope

- **In:**
  - the `rbd` driver and its `DriverSpec`;
  - claiming a pool (or namespace) with a marker object, as `shared-dir`
    claims a directory;
  - blank, full and linked disks (linked: RBD clones of an image's
    snapshot), growth, deletion;
  - one image RBD per image and datastore;
  - mapping through `glidex-stord` (krbd), device access per VM;
  - exclusive access through RBD's exclusive lock, and taking it over
    from a fenced node;
  - capacity, fullness, `disk.stored`;
  - moves onto and off `rbd` (flattened, raw);
  - API, gxctl, UI and docs, as for every driver.
- **Out:**
  - deploying or configuring Ceph (cephadm, Rook, MicroCeph): the
    operator brings a cluster, pool and client user;
  - CephFS: it stays `shared-dir`, unchanged;
  - RGW (object storage), RBD mirroring, disk snapshots and backups:
    later, each with its own design;
  - live migration (24-storage.md §1);
  - QEMU's built-in librbd path, unless Q1 says otherwise.

## 2. Decisions

Proposed. D4, D7 and the test host wait on Q1–Q3.

| # | Decision | Why |
|---|---|---|
| D1 | **`rbd` is a shared datastore over one pool, optionally one RADOS namespace in it** (`DriverSpec::Rbd {cluster, user, pool, namespace}`). Every node that reaches the Ceph cluster reaches the datastore. | A pool is Ceph's unit of placement, replication and quota. A namespace lets several glidex clusters, or a glidex cluster and OpenStack, share one pool without seeing each other's images, and the operator can limit the client user's caps to it. |
| D2 | **Glidex uses a Ceph cluster the operator runs; it never creates pools, users or caps.** It needs Ceph Reef (18) or later, with clone format 2. The operator makes the pool (`ceph osd pool create glidex; rbd pool init glidex`) and the user (`ceph auth get-or-create client.glidex mon 'profile rbd' osd 'profile rbd pool=glidex'`). | The same line as `shared-dir` (glidex never mounts) and `lvm` (glidex never touches physical volumes, S19): storage administration stays with the operator. Reef's clone v2 lets an image snapshot with clones be removed without protect/unprotect. Ubuntu 24.04 ships Squid (19). |
| D3 | **Glidex drives Ceph through the `rbd` and `rados` command-line tools with `--format json`**, not by linking librbd. | No C library in the build: the workspace still builds and tests on a machine without Ceph, and tests use a stand-in tool, as the hyperstor driver does with `hyperstorctl`. The `lvm` driver works the same way. Each call costs a process and a monitor connection (tens of ms), which is small next to a disk operation. |
| D4 | **Each node finds the cluster in `/etc/ceph/<cluster>.conf` and `/etc/ceph/<cluster>.client.<user>.keyring`, which the operator installs** (keyring `root:glidex 0640`). No Ceph secret passes through glidex's API, store or logs. *(Pending Q2.)* | It is how every Ceph client is set up, and a node needs the key to map a disk, while agents don't hold glidex's secrets key (19-tenancy.md §13). A key in the API would have to be sent to each agent, which is a new secret-distribution path. Rotation is Ceph's (`ceph auth` and new files), and a node can have a user of its own. |
| D5 | **A node reaches an `rbd` datastore only when it finds the datastore's marker**: a RADOS object `glidex-datastore` in the pool and namespace, holding the datastore and cluster ids. Claiming creates it with an exclusive create; a pool or namespace with RBD images and no marker is refused unless `adopt: true` (its images are reported as orphans, never imported). | The same rule as S5: a typo in the pool name, or two glidex clusters given one pool, must fail at creation, not write disks where another system keeps its own. An exclusive create is atomic in RADOS. |
| D6 | **Images are named `gx-d-<disk uuid>` for disks and `gx-i-<image id>` for images**, and built aside under `<name>.new`, then renamed (`rbd mv`). | RBD names are unique per pool and namespace, so the datastore scopes them; hyperstor's cluster-wide ids (24-storage.md S28) don't apply. A rename makes a complete image appear under the disk's name, as the file and `lvm` drivers do; a crash leaves only a `.new` image, which the next attempt removes. |
| D7 | **A VM's RBD disk is mapped by the kernel client (krbd) through `glidex-stord`, and handed to the hypervisor as a block device** (`VolumeBinding::Block`), for Cloud Hypervisor and QEMU alike. *(Pending Q1.)* | Cloud Hypervisor has no RBD support, so it needs a block device anyway; one path for both hypervisors reuses S6's block binding, `DeviceAllow` drop-ins and hypervisor arguments, which are tested. Mapping needs root, which stord is for (S12). |
| D8 | **stord maps only images in pools and namespaces listed in `/etc/glidex/stord.json` (`allowed_rbd_pools`), and only names of D6's form.** It makes the device `glidex:glidex 0600` and adds it to the VM's `DeviceAllow` drop-in. | S17's rule for LVM: a compromised control plane can map glidex's own images, not the host's or another tenant system's in the same cluster. |
| D9 | **Every disk image has the `exclusive-lock` feature, and is mapped with `-o exclusive`**: the mapping node takes the lock and doesn't give it up while mapped. A map on a second node is refused, and that VM waits (`DiskBusy`), as for a held hyperstor volume. | A lock in Ceph itself holds across nodes even when glidex's own records are wrong, which neither placement nor the volume lock file can promise (24-storage.md §10). With `-o exclusive`, krbd won't hand the lock to another client that asks for it, so a running VM's disk is never written from two places. |
| D10 | **A VM placed again after its node was forgotten as fenced takes its disks' locks over: the new node breaks the old lock, which blocklists the old client** (`takeover_disks`, as S23). Every other refused map waits. A lock whose owner is gone (a node that rebooted) is taken without a takeover, as Ceph does for a dead owner *(verify, H2)*. | Ceph's blocklist is a fence: a partitioned node that comes back can't write the disk again, which is what D8 of 14-clustering.md has lacked. Breaking a live owner's lock only on the operator's `--fenced` keeps D8's rule that nothing moves on its own. |
| D11 | **The worker is any Ready node that reaches the datastore**, as for `shared-dir` (24-storage.md §8.1). Volume operations (create, clone, grow, delete, image import) run as the `glidex` user with the `rbd` tool. Partition edits (extend-root, the GPT backup header) map the image through stord on the worker, run `prepare` on the device, and unmap. | Every node has the keyring (D4), unlike hyperstor's identity (S22), so there is no reason to keep the work on servers. Partition edits need a block device; the exclusive lock (D9) stops the worker from mapping a disk a VM has. |
| D12 | **One image RBD per image and datastore**, filled with `rbd import` from the node's verified image file converted to raw, then snapshotted as `golden`. A linked disk is `rbd clone` of `golden`, grown to its size; a full disk is the same clone, flattened. The leader removes an image RBD once its image is deleted and no clone uses it (`rbd children`), each probe round (as S24). | Clones are copy-on-write in RADOS, so a hundred disks from one image cost one image's space and start at once. `rbd import` skips zeros and needs no root. Cleanup in the probe round gives a clone deleted to Ceph's trash time to go. |
| D13 | **Disks grow (`rbd resize`) and never shrink.** A resize of a disk whose VM runs waits for the VM to stop (`ResizePending`). Online growth (krbd refreshes the size, the hypervisor is told) comes later. | Same rules as `lvm` (S19), with one path. Shrinking a block device under a filesystem loses data. |
| D14 | **Space and fullness come from the pool**: `ceph df` for its `max_avail` and stored bytes, and the pool's `full` and `nearfull` flags and quota. A full pool refuses new disks (`507`, as S30). `overcommit: false` is checked against `max_avail`. *(`ceph df` under `profile rbd`: verify, H5; without it, sizes are 0, as on hyperstor's agents.)* | `max_avail` is after replication or erasure coding, which is what a disk takes. A full Ceph pool blocks writes cluster-wide, so refusing a new disk up front is the cheap part. |
| D15 | **Moves onto and off `rbd` are flattened and raw** (S6c's rule): onto, `qemu-img convert -O raw` and `rbd import`; off, `rbd export` (or the mapped device) into the destination. | A raw RBD has no backing file. The mover (24-storage.md §8.3) needs the keyring, which every node has. |
| D16 | **`disk.stored` is `rbd du`'s used bytes** for the disk (with `fast-diff`, the image's object map makes it cheap), sampled by the worker. | 24-storage.md §13: a shared datastore is counted once. |

## 3. Model

```rust
pub enum DriverSpec {
    // … Dir, SharedDir, Lvm, Hyperstor (24-storage.md §4.1)
    Rbd {
        cluster: String,           // "ceph": /etc/ceph/<cluster>.conf
        user: String,              // "glidex": client.<user>
        pool: String,
        namespace: Option<String>, // RADOS namespace; None = the pool's default
        node_users: BTreeMap<String, String>, // node id -> user, when nodes have their own (D4)
    },
}
```

- Topology `Shared`; `content` disks and images; `linked_clones` true.
- Admission refuses names that aren't plain Ceph names
  (`[A-Za-z0-9_.-]`, no leading `.`), a `node_users` entry for an unknown
  node, and a pool and namespace another datastore of this cluster has.
- `datastore_nodes` rows as 24-storage.md §4.1, with `path` shown as
  `<pool>[/<namespace>]` and `fs_id` the Ceph cluster's `fsid` plus the
  marker's datastore id: two nodes configured for different Ceph
  clusters show as `Diverged`.

Probe reasons, added to §4.1's: `NoCephConfig` (no conf or keyring
readable), `CephUnreachable` (monitors don't answer in 10 s), `NoPool`,
`NoMarker`, `ForeignMarker`, `StordUnavailable`, `NotAllowed` (pool not
in stord's `allowed_rbd_pools`), `NoKrbd` (the `rbd` kernel module
won't load).

## 4. Creating an `rbd` datastore

The operator, once per cluster and node:

```bash
# On the Ceph cluster
ceph osd pool create glidex
rbd pool init glidex
ceph auth get-or-create client.glidex \
  mon 'profile rbd' osd 'profile rbd pool=glidex' \
  -o ceph.client.glidex.keyring     # copy to each node, never paste it

# On each glidex node
sudo install -m 0644 ceph.conf /etc/ceph/ceph.conf
sudo install -o root -g glidex -m 0640 ceph.client.glidex.keyring /etc/ceph/
# stord's allow-list (or the installer's --stord-allow-rbd glidex)
#   /etc/glidex/stord.json: {"allowed_rbd_pools": ["glidex"]}
```

Then `gxctl datastore create ceph1 --rbd glidex[/ns] [--user glidex]
[--cluster ceph]`:

1. The record is made `Pending`.
2. The leader asks a Ready node to claim it (D5): it creates the marker
   with an exclusive create, or adopts its own.
3. Each node's probe, every 60 s: config and keyring readable,
   `rados stat glidex-datastore` names this datastore and cluster,
   stord's `rbd_status` (module loaded, pool allowed), and on one node
   the pool's space and flags (D14).
4. `Ready` once a node reaches it.

Deleting the datastore is refused while it has disks or image RBDs (as
§11.1's datastore deletion); the marker is removed last.

## 5. Volumes

| Operation | Worker does |
|---|---|
| Blank | `rbd create --size <bytes> --image-feature layering,exclusive-lock,object-map,fast-diff,deep-flatten gx-d-<id>.new`, then `rbd mv` |
| Linked | `rbd clone gx-i-<image>@golden gx-d-<id>.new`, `rbd resize`, `prepare` on the mapped device (extend-root), `rbd mv` |
| Full | as linked, then `rbd flatten` before the rename |
| Image RBD | under a per-image lock in the datastore (a RADOS advisory lock on the marker's sibling object `glidex-image-<id>`): `qemu-img convert -O raw` from this node's verified file to a temporary file, `rbd import`, `rbd snap create @golden`, `rbd mv`; a second worker that finds the lock held waits |
| Grow | `rbd resize --size` (never `--allow-shrink`) |
| Delete | `rbd rm`; refused by Ceph while mapped or locked, so it waits for the VM's node to unmap |
| Exists | `rbd info`; an error other than "not found" is an error, never "missing" (S32) |

The image features are the krbd-compatible set: no `journaling`. Every
call names `--cluster`, `--id`, `--pool` and `--namespace`, never a key.

## 6. Staging and access

`glidex-stord` gains these ops (newline JSON, as §11.1):

| Op | Does |
|---|---|
| `rbd_status {cluster, user, pool, namespace}` | Whether the pool is allowed, the module loads and the conf and keyring are there |
| `rbd_map {cluster, user, pool, namespace, image, takeover}` | `rbd device map -o exclusive` (after `rbd lock rm` of the current owner when `takeover`), then `chown glidex:glidex`, `chmod 0600` on the device; answers with `/dev/rbd/<pool>/<ns>/<image>` |
| `rbd_unmap {…, image}` | `rbd device unmap`; refused while the device is open |
| `rbd_list_mapped {}` | glidex's mapped images: pool, namespace, image, device, whether open |

and `allow_devices` takes `/dev/rbdN` devices (major `rbd`) as it takes
LVs.

**Launch** (VM controller, on the placed node): `rbd_map` each RBD disk,
`takeover` set when the VM's `takeover_disks` is (D10); a refusal
because another client holds the lock is `DiskBusy`, retried every 5 s.
Then `allow_devices`, then the unit starts.

**Hypervisors** get the block binding unchanged: Cloud Hypervisor
`--disk path=<dev>,image_type=raw`; QEMU `driver: host_device`,
`cache.direct: on`.

**Stop and delete** unmap the VM's RBD disks, so the next start, here or
elsewhere, finds the lock free. Each node's probe unmaps images it has
mapped for VMs placed elsewhere and not open, once it has loaded its VMs
(the lesson of #322, S26).

**When Ceph is unreachable,** a mapped disk's I/O blocks in the kernel
and the guest stalls; it resumes when the cluster answers. glidex does
not time requests out (`osd_request_timeout` stays 0), because a timed-out
write is a lost write to a filesystem. The probe marks the node's row
`CephUnreachable`, and the VM page shows it.

## 7. API, gxctl, UI

- **API:** `DATASTORE_DRIVER_RBD` with `rbd_cluster`, `rbd_user`,
  `rbd_pool`, `rbd_namespace`; `node_paths` names a node's own user
  (as it names a VG for `lvm`). Additive proto change.
- **gxctl:** `datastore create NAME --rbd POOL[/NAMESPACE] [--user U]
  [--cluster C] [--adopt]`.
- **UI:** the Datastores page's form gets *Ceph RBD pool*; the node table
  shows the reasons above. No secret field: there is no secret (D4).
- **Cedar:** nothing new; `manageDatastore` and `useDatastore` as today.
- **Installer:** `--stord-allow-rbd POOL[/NS]`, repeatable; it checks that
  `ceph-common` is installed and says how to install it if not. It
  doesn't copy keyrings.
- **Docs** (`docs/public/`): the operator guide's datastores page gets
  an RBD section with §4's steps; `comparison.md` and
  `design-decisions.md` stop saying Ceph is CephFS only. Each in the PR
  that ships the behaviour.

## 8. Tests

- **Unit:** the command lines of every volume operation and stord op,
  against a stand-in `rbd`/`rados` that records its arguments and
  answers from fixtures (D3); name and allow-list checks in stord;
  probe reasons; the lock-held answer mapped to `DiskBusy`; `exists`
  errors never `missing`.
- **API:** `rbd_datastores_name_a_pool_and_keep_raw_disks`, with the
  stand-in: create, claim refused on a foreign marker, raw only, no
  shrink, move refusals.
- **Cluster:** a VM on an RBD disk placed again on another node when
  stopped, and when Lost after `forget --fenced` with a takeover
  (stand-in, checking the `takeover` flag reaches stord).
- **Ignored, on a host with a Ceph cluster:** `rbd_disks_on_a_real_pool`
  (blank, linked and full, grown, deleted; image RBD removed with its
  image), `rbd_root_disk_boots` and `qemu_rbd_root_disk_boots` (boot a
  cloud image from a linked 6 GiB disk, root grown, log in), and stord's
  `real_rbd` (map, refuse a second exclusive map, unmap).

## 9. Host checks

On a test Ceph cluster (Q3), recorded here with date, build and result:

| # | Check |
|---|---|
| H1 | A disk mapped `-o exclusive` on node A: mapping it on node B is refused, and `rbd resize` from B is refused or waits. |
| H2 | Node A powered off with the disk mapped: B maps it after A's watch expires, without `lock rm`? And with `lock rm`, A's client is blocklisted, and A, powered on with its old mapping, can't write. |
| H3 | Cloud Hypervisor and QEMU boot from `/dev/rbd*` under the systemd runner (`DeviceAllow` for major `rbd`). |
| H4 | The control plane's unit sandbox reaches the monitors and OSDs, and reads the keyring. |
| H5 | `ceph df --format json` works with `mon 'profile rbd'` caps. |
| H6 | The guest's behaviour while the Ceph cluster is unreachable for 2 minutes, then back (§6). |

## 10. Questions

Open, for the owner:

- **Q1. Data path for QEMU.** Recommended: krbd for both hypervisors
  (D7), one tested path. The alternative gives QEMU librbd
  (`driver: rbd`): no root and no kernel module for QEMU VMs, newer RBD
  features, but a second path, a QEMU with the RBD block module
  (`qemu-block-extra` on Ubuntu) and the keyring readable by each VM's
  unit. A third, `qemu-storage-daemon` exporting RBD over vhost-user-blk
  for both, reuses hyperstor's binding but adds a process per VM.
- **Q2. Where the Ceph key lives.** Recommended: keyring files the
  operator installs on each node (D4). The alternative takes the key in
  `datastore create` (write-only, `--keyring-file`), seals it, and sends
  it to each node's stord; one place to rotate, but a new way for a
  secret to reach agents.
- **Q3. Test cluster.** Which Ceph cluster the host checks and ignored
  tests run against: a single-node MicroCeph on a dev host (three loop
  OSDs), or a cluster the site already has.
- **Q4. Scope of the first release.** Recommended: R1–R3 in v0.0.8 if
  it fits, R4 after; or the whole of §11 in one release.

## 11. Milestones

| # | Content | Depends on |
|---|---|---|
| R0 | This spec, Q1–Q4 settled and recorded as decisions | — |
| R1 | The record, API, gxctl, UI form, claim and marker, probe and space (no disks yet); the `rbd`/`rados` wrapper and its stand-in | R0 |
| R2 | stord's RBD ops and allow-list, installer flag; volumes (§5) and staging (§6); boot tests on both hypervisors | R1 |
| R3 | Exclusive lock, takeover after `--fenced`, the probe's unmapping of stale maps; host checks H1–H3 | R2 |
| R4 | Moves onto and off `rbd`, `disk.stored`, full pools; host checks H4–H6 | R2 |

## 12. Status

| | |
|---|---|
| Spec \| Code | Spec draft; no code |
| Tests | None yet |
| Not yet | Everything; Q1–Q4 open |
