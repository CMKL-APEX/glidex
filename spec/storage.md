# Datastores

Status: **design (draft)**. Nothing here is implemented. The milestones
are S0–S7 (§16), and the open questions are in §17. The edits this
document needs in other specs are listed in §18. They are **not applied
yet**.

Today every managed image and disk is a file in one of two directories
on the node that uses it ([images.md](images.md) §2). A disk is bound to
one node, and its VM is pinned to that node
([clustering.md](clustering.md) D6). The specs mark shared storage as a
non-goal (images.md §1) and as later work (clustering.md §18: "shared
storage classes", "a shared image store", "moving resources between
hosts").

This document adds **datastores**. A datastore is a cluster-wide record
that names where disks and images live. A **driver** creates, sizes,
deletes and opens volumes in it. A **topology** says which nodes can
reach it. The current directories become one datastore, `local`, using
the `dir` driver. The first shared driver is `shared-dir`: a filesystem
that the operator or the storage vendor mounts on every node (NFS,
CephFS, GPFS, a NAS appliance). Block drivers and a CSI adapter are
later milestones (§11, §12).

## 1. Scope

- **In:**
  - the `Datastore` resource and its per-node status;
  - the `dir` and `shared-dir` drivers;
  - a driver interface shaped so a CSI adapter fits behind it;
  - placement by datastore topology;
  - image copies per datastore;
  - the node that carries out operations on a shared disk;
  - re-placing a stopped or lost VM whose disks are all on shared
    datastores;
  - moving a disk between datastores offline;
  - the bugs that block all of this (§15).
- **Out:**
  - live migration (clustering.md §18; datastores are its prerequisite);
  - automatic failover and fencing hardware (D8 stays);
  - disk snapshots;
  - encryption at rest;
  - provisioned IOPS (metering.md §16).
- **Out, designed only far enough to keep the interface right:**
  - block drivers (`lvm`, `rbd`);
  - the CSI adapter.

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| S1 | **CSI is not the first interface.** Shared storage starts as filesystems that the vendor or operator mounts (`shared-dir`). glidex never mounts them. glidex's own driver trait follows CSI's split between controller (create/delete/expand/publish) and node (stage/unstage), and uses CSI's topology and capability model. That way a CSI adapter is one more driver (§12, milestone S7), added only when a plugin is actually needed. | **Mounted filesystems:** nearly every vendor and open-source system offers an NFS or POSIX mount (NetApp, Pure, Dell PowerScale, CephFS, GPFS, Lustre). A mount keeps qcow2 and its features: linked clones, sparse files, `qemu-img` resize and shrink. It needs no root, because the mount is done outside glidex (README goal 4, images.md §1 "No step needs root").<br><br>**CSI:** it was designed for container orchestrators. Many plugins assume Kubernetes sidecars and Kubernetes objects such as Secrets, CRDs and node labels. Node plugins run as root and mount things. A VM can only use CSI's `Block` volume mode, which gives up qcow2. Outside Kubernetes, only Nomad and a few others host CSI plugins, and their compatibility differs from plugin to plugin. The cost is high and arrives before any user asks for a particular plugin. |
| S2 | **A datastore is a cluster-scoped record in the replicated store.** Disks and image copies name their datastore by id. | Disks belong to a project and images to the cluster (security.md §7). Storage belongs to the operator, like nodes and networks. In a single-host install the "cluster" is the host (clustering.md D5), so this works the same there. |
| S3 | **The current directories become the built-in datastore `local`** (driver `dir`, on every node, default for disks and images). A schema migration sets `datastore: "local"` on every disk and image copy. No file moves. | No user sees a change until an operator adds a datastore. D19 ("ids travel") still holds, because files stay named by id. |
| S4 | **A datastore's topology says where it is reachable.**<br>- `node-local`: each node has its own separate copy; `dir`. A volume is bound to one node.<br>- `shared`: every node that reaches it sees the same files; `shared-dir`.<br>The scheduler places a VM only on a node that reaches every one of its volumes. | This generalises D6 without changing it for `local`. A shared volume has no node binding, so its VM can move when it is stopped (§9). |
| S5 | **A node reaches a shared datastore only when it finds the datastore's marker file** (`<path>/.glidex-datastore`, holding datastore and cluster id) **on a writable mount**. | An NFS export that isn't mounted leaves an empty directory on the root filesystem. Writing there would quietly put disks on the wrong disk, and different nodes would see different files. The cluster id stops two clusters from sharing one export by mistake. |
| S6 | **Exactly one node acts on a volume's files at a time:** the volume's **worker**. For a node-local volume it is the bound node. For a shared volume the leader chooses it and records it (`Disk.worker`): the VM's placement if the VM is placed, otherwise a Ready node that reaches the datastore. | Today "the disk controller on the bound node" gives this for free. With shared storage, every node's disk controller could see the record and act on it. Two `qemu-img resize` runs on one file corrupt it. |
| S7 | **Disk operations run on the worker, never on the server that answers the API call** (§8). That includes reading the partition table for shrink and extend-root. | This fixes the gaps found in §15: the API currently reads the file locally. It is also the only correct rule once files are not on the server. |
| S8 | **Linked clones need the image copy in the same file datastore as the disk.** The overlay names its backing file by a **relative** path (`../images/<id>.qcow2`). | The qcow2 header stores the backing path. An absolute path breaks as soon as nodes mount the datastore at different paths, or `GLIDEX_IMAGE_DIR` differs from node to node. |
| S9 | **A shared datastore holds one copy of each image it needs** (`image_caches/<image>/ds:<datastore>`). A `node-local` one keeps a copy on each node (`.../<node>`, as today). | This is the "shared image store" of clustering.md §18 without a new service. Images stay cluster records. Only where their files live changes. |
| S10 | **Moving a VM stays explicit and offline**, and needs every disk on a shared datastore. A **stopped** VM can be re-placed (`POST /vms/{id}/place`). A **`Lost`** VM can be re-placed after `node forget --fenced`. Nothing moves automatically. | D8 still holds: a partitioned node may still be running the VM. Today only the operator's `--fenced` statement establishes that it isn't. Hypervisor file locks (§10) are a second check, not the guarantee. |
| S11 | **Mount points live under one root, `/srv/glidex/<name>`**, which both units list as `ReadWritePaths=-/srv/glidex`. Paths outside it are refused unless an operator adds a drop-in. | Registering a datastore through the API must not require editing systemd units on every node, and the control plane can't widen its own sandbox (security.md §10). |
| S12 | **Block volumes and a privileged storage helper come later** (`glidex-stord`, §11), on the netd pattern. | Mapping LUNs, activating LVs and `rbd map` need root. The unprivileged control plane asks a small root daemon, as it does with netd for networking. |

## 3. What changes, at a glance

| Today | With datastores |
|---|---|
| `GLIDEX_IMAGE_DIR` and `GLIDEX_DISK_DIR` (environment only) | the `local` datastore. Its paths still come from those variables. |
| `Disk.node`, the node holding the file | `Disk.datastore`. `node` is set only on node-local datastores. `worker` is new (S6). |
| `image_caches/<image>/<node>` | `image_caches/<image>/<location>`, where a location is a node or `ds:<id>` (S9) |
| `is_local_disk` (`state.rs:889`) | `is_worker(disk)` |
| scheduler filter "disk bound to this node" (`scheduler.rs:114`) | "node reaches every volume's datastore, and node-local volumes are bound here or unbound" |
| `DiskBinding {path, format, backing_files}` | `VolumeBinding::File{…}` now, `Block{dev}` and `Rbd{…}` later (§11) |
| disk files never leave their node (clustering.md §18) | `POST /disks/{id}/move {datastore}` copies offline (§8.3) |

## 4. Model

### 4.1 `Datastore`

```rust
pub struct Datastore {
    pub id: String,                  // UUIDv4; "local" for the built-in one
    pub name: String,                // unique in the cluster
    pub driver: DriverSpec,
    pub topology: Topology,          // fixed by the driver (S4)
    pub content: Vec<Content>,       // Disks | Images; both by default
    pub nodes: NodeSelector,         // All | Only(Vec<node id>); node-local: where it exists
    pub default_for: Vec<Content>,   // at most one datastore is the default per content
    pub linked_clones: bool,         // default true where the driver supports it
    pub direct_io: Option<bool>,     // QEMU cache.direct; None = probe (hypervisors.md)
    pub phase: DatastorePhase,       // Pending | Ready | Draining | Deleting
    pub created_at: u64,
    pub deletion_requested_at: Option<u64>,
}

pub enum DriverSpec {
    Dir { path: Option<String> },    // None = built-in local: env directories
    SharedDir { path: String },      // under /srv/glidex (S11)
    // Later (§11, §12):
    // Lvm { vg: String, thin_pool: Option<String> },
    // Rbd { pool: String, monitors: Vec<String>, credential: String },
    // Csi { plugin: String, parameters: BTreeMap<String, String> },
}

pub enum Topology { NodeLocal, Shared }
```

Per-node status is a replicated table that each node writes about
itself, like `image_caches`:

| Table | Key | Value |
|---|---|---|
| `datastore_nodes` | `<datastore>/<node>` | `{reachable, reason, total_bytes, free_bytes, fs_id, checked_at}` |

- `reason` is one of:
  - `NoMarker` (S5);
  - `ForeignMarker` (another cluster or datastore id);
  - `ReadOnly`;
  - `SandboxDenied` (outside `ReadWritePaths`);
  - `NotADirectory`;
  - `Unsupported` (driver not available on this node).
- `fs_id` is the filesystem's `f_fsid` plus the marker's datastore id. It lets the leader detect two nodes that think they share a datastore but see different filesystems (`reason: Diverged`, set by the leader).
- Nodes check every 60 s and on start.

### 4.2 Changes to `Disk` and `Image`

```rust
pub struct Disk {
    // ...existing fields (images.md §3)...
    pub datastore: String,           // serde default "local" (S3)
    pub node: Option<String>,        // node-local datastores only
    pub worker: Option<String>,      // S6; written by the leader
    pub volume: Option<VolumeHandle>,// driver's opaque id; None for file drivers
}
```

- `Image` itself does not change: it is still one cluster record with
  one sha256.
- `image_caches` rows gain a location: a node id, or `ds:<datastore>`
  for a shared one.
- `Image.pinned` takes locations too.

**Invariant.** A `Linked` disk's datastore holds a Ready copy of its
image before `materialize` runs. That copy can't be dropped while a
linked disk in that datastore uses it. This extends images.md §3's
`ImageHold` from the image to the copy.

**Invariant.** `node` is `None` exactly when the datastore is `Shared`.
`worker` is `node` on node-local datastores.

### 4.3 Paths and layout

```
<datastore root>/
├── .glidex-datastore       # {"datastore": "<id>", "cluster": "<cluster id>", "created_at": …}, 0444
├── images/<image-id>.qcow2 # as images.md §2, 0444
├── disks/<disk-id>.qcow2   # 0600; linked overlays back onto ../images/<image-id>.qcow2
└── .gx-tmp/                # dot temporaries, same filesystem (images.md §2)
```

- The `local` datastore keeps today's two directories and has no marker.
  It is node-local, so the node-local rule applies and nothing else can
  reach it.
- Every invariant of images.md §2 still holds within a datastore:
  - names are by id only;
  - orphan files are reported, never deleted;
  - a record whose file is missing is marked missing.
- On a shared datastore, only the file's **worker** reports it missing.
  A node that can't reach the datastore reports nothing about it.

## 5. Reachability and placement

### 5.1 Creating a shared datastore

1. `POST /datastores {name, driver: {shared-dir: {path}}}` records the
   datastore in phase `Pending`.
2. The leader picks one Ready node that has the path writable and asks
   it to **claim** the directory:
   - An empty directory gets the marker, created with `O_EXCL`, then
     `images/`, `disks/` and `.gx-tmp/`.
   - A directory with this datastore's marker is adopted as is.
   - A directory with a foreign marker, or with content but no marker,
     is refused with `409 datastore_not_empty`.
   - `adopt: true` accepts a non-empty directory without a marker, but
     its files are reported as orphans, never imported.
3. Every node then reports `datastore_nodes`. The datastore becomes
   `Ready` once one node reaches it.

Why `O_EXCL`: two clusters, or two datastores, claiming one export at
the same moment must not both succeed. Over NFSv4, `O_EXCL` create is
atomic **(verify on a host for the NFS versions we support)**.

### 5.2 Scheduler

The scheduler's `Request` (`state.rs:678`) gains each volume's
`(datastore, node)`.

**Filter:** a node passes when, for every volume:
- the volume's datastore is in `nodes` and `datastore_nodes/<ds>/<node>` is reachable; and
- if the datastore is node-local, the volume is unbound or bound to this node.

**Bind** (unchanged for node-local, clustering.md §9.1): unbound node-local volumes bind to the chosen node in the same write. Shared volumes get `worker` set to the chosen node.

**Score:** prefer nodes with free space on node-local datastores of unbound volumes. A shared datastore scores the same on every node.

**Capacity:** disk create is refused with `507 insufficient_storage` when the target datastore's free space minus the not-yet-created disks recorded there is below the new size, unless `overcommit: true` is set on the datastore. Creating a disk works out the free space for a shared datastore from any reachable node (they agree, §4.1). For a node-local one it uses the node it binds to, or the best node when it is unbound.

### 5.3 Host paths

Raw host paths (`rootfs_path` and the others, security.md §7.4) stay as
an admin escape hatch. The scheduler still doesn't know where those files
exist (a gap noted in clustering.md). A VM with host paths should be
pinned with `node`.

A **registered `dir` datastore** is now the recommended way to use
another local directory. It is node-local, under the S11 root, and needs
no `useHostPath`.

## 6. Images on datastores

**Choosing a location.** A disk on datastore `D` built from image `I`
needs a copy of `I` in `D` when it is `Linked`. A `Full` clone can read
from any copy it can reach.

The image-cache controller (`controller/image_cache.rs`) works per
**location**:
- **node-local:** each node fetches its own copy as today
  (`needs_image_here`, `holder_of`);
- **shared:** the worker of the first disk that needs it (or a node the
  leader picks for a pin) fetches one copy into `ds:<id>`. The other
  nodes see the row and do nothing.

**Sources.** A copy can come from any Ready location reachable from
the fetching node: another shared datastore it mounts, or a peer over
the cluster port, as today. sha256 is checked either way.

**Firmware images** follow the same rule. Launch resolves the firmware
path on the placed node (§15, bug 1).

**Linked overlays.** New overlays are created with a relative backing
path (`-b ../images/<id>.qcow2 -F qcow2`, resolved from the overlay's
directory). Existing overlays in `local` keep their absolute paths. They
stay correct as long as `local` never moves, which S3 guarantees. A
`move` (§8.3) rebases them (`qemu-img rebase -u`).

**Dedup.** On import (clustering.md §5.9), an image whose sha256 matches
a Ready copy on a shared datastore can share that file. This settles
clustering.md O6 for shared datastores only. Two records still keep two
files on `local`.

## 7. The driver interface

The interface is in-tree, the first implementations are `dir` and
`shared-dir`, and the method names follow CSI (§12) on purpose:

```rust
#[async_trait]
pub trait StorageDriver: Send + Sync {
    fn capabilities(&self) -> Caps; // file|block, linked_clones, expand, shrink, shared
    // Controller side: any node that reaches the datastore (the worker).
    async fn create_volume(&self, req: CreateVolume) -> Result<Created>; // blank | from image copy | from volume
    async fn delete_volume(&self, v: &VolumeRef) -> Result<()>;
    async fn expand_volume(&self, v: &VolumeRef, bytes: u64) -> Result<()>;
    async fn shrink_volume(&self, v: &VolumeRef, bytes: u64) -> Result<()>;   // file drivers only
    async fn capacity(&self) -> Result<Capacity>;
    // Node side: the node that runs the VM.
    async fn stage(&self, v: &VolumeRef) -> Result<VolumeBinding>;   // path / device for the hypervisor
    async fn unstage(&self, v: &VolumeRef) -> Result<()>;
    // Reachability (§4.1).
    async fn probe(&self) -> Result<NodeReach>;
}

pub enum VolumeBinding {
    File { path: PathBuf, format: DiskFormat, backing_files: bool },
    // Later: Block { dev: PathBuf }, Rbd { pool, image, user }, VhostUserBlk { socket }
}
```

- For file drivers, `stage` and `unstage` only check that the file
  exists and return the path. There is nothing to attach.
- Extend-root and shrink need the partition table. They call
  `stage` to get a binding, then run `images::partition` against it. For
  a `File` binding that is today's code unchanged. For a `Block` binding
  it is the same tools on a device node.
- The current code in `images/disk.rs`, `images/qemu_img.rs` and
  `images/partition.rs` moves behind `dir`/`shared-dir`, which share one
  implementation (`FileDriver { root, shared }`).
- Callers stop building paths. `ImageManager::disk_path` is used only
  inside the file driver.

**Hypervisor handoff.** `vm_disks` (`hypervisor/mod.rs:177`) takes
`VolumeBinding`s. File bindings produce exactly today's CH `--disk path=…`
and QEMU `driver: file` arguments, so a test pins the command lines as
unchanged.

## 8. Disk operations

### 8.1 Who runs them

- The disk controller on each node acts on a disk only when
  `is_worker(disk)`.
- A shared disk with no placed VM gets a worker when an operation is
  recorded. The leader picks the Ready node that reaches the datastore
  and has the fewest operations in flight, and writes the worker in the
  same transaction as the request.
- If the worker goes `Unknown` while the disk is not attached to a
  placed VM, the leader reassigns it after the node grace period. Every
  operation already writes to a dot temporary and renames (images.md §2),
  so a half-done operation on the old worker leaves only a temporary. A
  new worker deletes temporaries older than the reassignment.
- A worker that comes back after losing the role must not finish its
  rename. It re-reads `worker` before the rename and abandons the
  operation if it changed. **Open:** this leaves a tiny window
  (check-then-rename); see §17 Q2.

### 8.2 API side

- `POST /disks/{id}/resize` and `/extend-root` stop reading the file on
  the answering server (§15, bug 2).
- They record the request.
- The checks that need the file (minimum shrink size, growable root
  partition) run on the worker. The result comes back as the `Ready`
  condition (`ResizeInvalid`, already in reconciliation.md §10.1) and the
  `?wait` outcomes (rest-api.md).
- A synchronous early answer, as today, is kept only when the answering
  server is the worker.

### 8.3 Moving a disk between datastores

`POST /disks/{id}/move {datastore, node?}`:
- The disk must be detached, or attached to a stopped VM.
- The disk becomes `Moving`.
- The destination's worker copies the file:
  - local filesystem to local filesystem: `qemu-img convert`;
  - between nodes: from the source worker over the cluster port, like
    an image copy, with a checksum.
- A linked disk is either rebased onto the destination's image copy
  (fetching it first) or flattened (`flatten: true`).
- The record's `datastore`, `node` and `worker` change in one write
  after the copy is verified.
- The source file is then deleted by its old worker. If that worker is
  gone, the file is reported as an orphan, never deleted blindly.

This also gives clustering.md §18's "moving resources between hosts"
for node-local disks: moving from `local` on node A to `local` on node B.

## 9. Placement after creation: re-placing VMs

- **Stopped VM:** `POST /vms/{id}/place {node?}`. Allowed when every disk is on a shared datastore, or is node-local and already on the target node.
  - It clears `status.placement`, and runs the scheduler with the pin if given.
  - It moves each shared disk's `worker` to the new node and re-binds the VM's node-scoped resources (node networks are refused; OVN ports move, as at start).
  - Needs `placeVm` (operators, §13).
- **`Lost` VM** (after `node forget --fenced`, clustering.md §5.6): the same call is allowed, which is what that section anticipated.
- **Running VM:** refused with `409 invalid_state`. Live migration is out of scope.

## 10. Exclusive access

The invariants of images.md §3 and reconciliation.md D11 (one VM per
disk, claims) don't change. On shared storage they are what stops two
nodes from writing one disk, and two backstops are added:

1. **Placement is the lock.** A node launches a VM only if `placement`
   is itself. It acts on a disk only if it is the `worker`. Both are
   Raft writes. A partitioned node doesn't launch (clustering.md §8.2),
   but it can keep a VM that is already running (D8). That's why S10
   needs `--fenced` before a `Lost` VM is re-placed.
2. **Hypervisor file locks.**
   - QEMU takes OFD locks on its image files by default
     (`locking=auto`). On NFSv4, and on CephFS with locking, these are
     cluster-wide. glidex sets `locking=on` for shared datastores, so a
     second QEMU fails to open the disk instead of corrupting it.
   - Cloud Hypervisor: whether it locks disk files, and how, is
     **(verify)**. If it doesn't, the shim takes an `fcntl` OFD write
     lock on each shared disk file before starting the hypervisor and
     holds it while the VM runs.
3. **Leases are later.** Protection that doesn't depend on locks
   (sanlock-style leases, RBD exclusive locks) comes with fencing
   (clustering.md §18).

## 11. Later: block drivers and `glidex-stord`

The design is only sketched here, so the interface (§7) and the record
(§4) stay right:

- **`lvm` (node-local), LVM-thin:**
  - volumes are LVs and are always raw;
  - linked clones are thin snapshots of an image LV;
  - resize uses `lvextend`, and shrinking is refused.
- **`shared-lvm` over a SAN** (iSCSI or FC LUN): the same LVs plus
  `lvmlockd`/`sanlock`. This is the Proxmox pattern.
- **`rbd`:**
  - QEMU opens RBD natively (`driver: rbd`). That needs a QEMU built with
    librbd, which is probed.
  - Cloud Hypervisor needs a block device, so `krbd` map, or
    vhost-user-blk from a separate daemon **(verify)**.
  - Exclusive locks provide fencing.
- **`glidex-stord`:**
  - a root helper on netd's model (networking.md §7): a Unix socket,
    peer-uid authorization, its own state database;
  - it maps and unmaps devices and fixes their ownership for the
    `glidex` user;
  - `glidex-vm@` gains `DeviceAllow=` for exactly the devices staged for
    that VM, through a per-VM drop-in written by stord.
- **Block volumes and partition edits:** with no qcow2, shrink is
  refused, `extend-root offline` works on the device, and metering's
  `disk.stored` comes from the driver (thin-pool usage, `rbd du`).

## 12. CSI

### 12.1 Why not first (S1)

| Concern | Detail |
|---|---|
| Kubernetes assumptions | The CSI spec doesn't depend on any container orchestrator, but plugins often do. Provisioning goes through Kubernetes sidecars (`external-provisioner`, `-attacher`, `-resizer`, `-snapshotter`) that watch Kubernetes objects. Credentials come as Kubernetes Secrets. Some plugins read their own CRDs or node labels. glidex would have to play the sidecars' role, plugin by plugin. |
| Root and containers | Node plugins mount, run `iscsiadm` and `rbd map` as root, usually from a privileged container with host mount propagation. glidex has neither containers nor a root storage daemon (S12). |
| Volume model | VMs need `Block` access mode (as KubeVirt uses it). `Mount` mode gives a filesystem, so glidex would write a raw image file inside it, which wastes the volume. Block mode means raw only: no qcow2 overlays (linked clones become `VolumeContentSource` clones, where the plugin supports them) and no shrink. |
| Operations | Each plugin is a gRPC service per node with its own release cycle, upgrade story and failure modes. Today glidex runs one binary per host (clustering.md D4). |
| Demand | No user has asked for a particular plugin yet. The vendors we would meet first also offer NFS. |

### 12.2 How the adapter would map

| glidex | CSI |
|---|---|
| `probe` | `Identity.Probe`, `GetPluginInfo`, `GetPluginCapabilities`, `Node.NodeGetInfo` (gives `accessible_topology`, mapped to `datastore_nodes`) |
| `capabilities` | `ControllerGetCapabilities`, `NodeGetCapabilities` |
| `create_volume` | `Controller.CreateVolume` (`volume_capabilities: block`, `accessibility_requirements` from the target node, `volume_content_source` for a clone) |
| `delete_volume` | `DeleteVolume` |
| `expand_volume` | `ControllerExpandVolume` + `NodeExpandVolume` |
| `capacity` | `GetCapacity` |
| (placement) | `ControllerPublishVolume` / `Unpublish` to the placed node, which gives exclusive attach where the plugin supports it |
| `stage` / `unstage` | `NodeStageVolume` + `NodePublishVolume` (block target under `/run/glidex-stord/csi/<vol>`) / the reverse |
| images | an image becomes a CSI volume (or snapshot) per datastore, with disks as clones of it. Without clone support: a full copy, and linked clones are off. |

The adapter would run in `glidex-stord`. Plugins run as plain processes
or containers, with their socket under `/run/glidex-stord/csi/`.
Credentials come from glidex's credential store and are passed as CSI
`secrets`, never as files.

### 12.3 When to do it (gate for S7)

Do the adapter when, and only when:
- a user needs a backend that has no NFS/POSIX mount and no native
  driver (§11), and
- that backend's CSI plugin runs without the Kubernetes API.

That second point can be tested in a day with `csc` (the CSI CLI) against
the plugin. Until then, the trait keeps the door open: CSI-shaped
methods, an opaque `VolumeHandle`, topology as data, and capabilities.

## 13. API, authorization, quota and metering

**REST** (and `glidex.storage.v1.DatastoreService`, grpc.md):

| Route | Action |
|---|---|
| `GET /datastores` | `readDatastore` |
| `POST /datastores` | `manageDatastore` |
| `GET /datastores/{id}` (incl. `nodes`) | `readDatastore` |
| `PATCH /datastores/{id}` (`nodes`, `default_for`, `linked_clones`, `overcommit`) | `manageDatastore` |
| `DELETE /datastores/{id}` (refused while volumes or copies exist; `Draining` refuses new ones) | `manageDatastore` |
| `POST /disks {…, datastore?}` (default: `default_for: disks`) | + `useDatastore` |
| `POST /disks/{id}/move` | `updateDisk` + `useDatastore` (destination) |
| `POST /vms/{id}/place` | `placeVm` |
| `POST /images/{id}/copies {datastore \| node}` | `image.manage` (as today) |

- `DiskResponse` gains `datastore` and `datastore_name`, and `path` is
  shown only with `readSystemStatus`.
- The watch kind `datastores` is added.
- gxctl: `datastore list|show|create|set|rm`, `disk create --datastore`,
  `disk move`, `vm place`.
- The UI gets a Datastores page (per-node reachability and capacity),
  a datastore column and selector on Disks, and the copies view on
  Images by location.

**Cedar:**
- `Datastore in [Cluster]`.
- New actions:
  - `manageDatastore` maps to a new `datastore.manage` permission, in
    the operator and system-admin roles;
  - `useDatastore` is permitted to every principal that may create disks
    in a project, unless a site policy forbids it. That lets operators
    keep a fast datastore for certain tenants (tenancy.md);
  - `placeVm` is in the operator role.
- On a shared datastore, host-path rules (security.md §7.4) treat its
  root like the managed directories: a raw path into it needs
  `useHostPath`.

**Quota:** project `disk_gib` stays cluster-wide (clustering.md §9.2).
Per-datastore quotas are §17 Q4.

**Metering** (metering.md):
- `disk.alloc` doesn't change.
- `disk.stored` and `image.stored` are sampled by the volume's worker,
  or for a shared copy by its fetching node. A shared datastore is
  counted once, never once per node.
- `group_by=datastore` is added.

## 14. Migration from today

1. A schema migration creates `local` and sets `datastore: "local"` on
   disks. It rewrites `image_caches/<image>/<node>` keys unchanged,
   because a bare node id is a node location. Standalone hosts get the
   same migration.
2. Nothing on disk moves. `GLIDEX_IMAGE_DIR` and `GLIDEX_DISK_DIR` keep
   working as the paths of `local`.
3. Detach and import (clustering.md §5.8–5.9) carry `local` disks as
   today. A disk on a shared datastore can't be detached with its node:
   the receiving cluster has no such datastore. That is refused with
   `409`, naming the disks, and the operator moves them to `local`
   first (§8.3).

## 15. Bugs to fix first (S0)

These were found while writing this document. Each one assumes every
node looks like the server that answered the request, and datastores
would make each of them worse.

1. **The firmware path is the server's.**
   - `state.rs:1565` stores `firmware_path` as an absolute path under the
     creating server's `image_dir`.
   - `controller/vm.rs:910-923` uses it as is on the placed node.
   - Launch doesn't wait for that node's firmware copy.
   - Fix: store only `firmware_image`, resolve the path on the node at
     launch, and wait on the image copy as disks wait on `DisksReady`.
2. **Shrink and extend-root read a remote file.**
   - `state.rs:2998` (`shrink_minimum`) and `state.rs:3052`
     (`partition::read_table`) read the disk file on the answering
     server.
   - Disk APIs are not relayed to the disk's node, so they fail for a
     disk bound elsewhere.
   - Fix: §8.2.
3. **`CreateDiskRequest.node` isn't resolved.**
   - It is documented as id or name, but stored raw
     (`images/disk.rs:190`).
   - A name never matches `local_node_id()`, so the disk is never made.
   - Fix: resolve and validate it like `VmSpec.node`.
4. **Linked overlays use absolute backing paths.**
   - `qemu_img::create` is passed `image_path` (`images/disk.rs:265`).
   - Correct only while `GLIDEX_IMAGE_DIR` is the same on every node.
   - Fix: a relative backing path for new overlays (S8).
5. **No storage capacity anywhere.** Nodes report no free space and the
   scheduler doesn't look at it. §5.2 adds it.

## 16. Milestones

| # | Content | Depends on |
|---|---|---|
| S0 | §15 fixes 1–4, with tests for each (two nodes with different `GLIDEX_IMAGE_DIR`, shrink through a server that isn't the disk's node, `node` by name) | — |
| S1 | `StorageDriver` trait. Today's code moved into `FileDriver` and `local` created by migration. `Disk.datastore`, `VolumeBinding`. **No behaviour change:** the command-line pin tests and every current storage test pass unchanged. | S0 |
| S2 | The `Datastore` resource, `datastore_nodes`, capacity, the API/gxctl/UI surfaces, Cedar, registered `dir` datastores under `/srv/glidex`, and the unit changes (S11) | S1 |
| S3 | `shared-dir`: marker and claim, reachability, `worker` (§8.1), the scheduler topology filter, image copies per location, linked clones with relative paths, QEMU `locking=on`, and the CH lock **(verify)** | S2 |
| S4 | `POST /vms/{id}/place` for stopped VMs, and for `Lost` VMs after `forget --fenced` | S3 |
| S5 | `POST /disks/{id}/move` (within a node, between nodes, onto or off shared datastores) | S3 |
| S6 | `glidex-stord`, `VolumeBinding::Block`, and the `lvm` driver; then `rbd` | S2 |
| S7 | The CSI adapter, **only if the §12.3 gate is met** | S6 |

## 17. Open questions

1. **Per-node mount paths.**
   - S11 assumes one path on every node.
   - Allowing `paths: {node: path}` is cheap, because §6 makes backing
     paths relative and the marker identifies the filesystem.
   - Is a single path enough in practice?
2. **Worker hand-over window** (§8.1).
   - A paused old worker can still rename after re-checking.
   - Options:
     - do the rename under an OFD lock on a per-disk lock file in the
       datastore;
     - or accept the window, since reassignment needs the old worker
       `Unknown` for the full grace period.
3. **Default for new disks** once a shared datastore exists:
   - keep `local` (safe, fast);
   - or switch to shared (VMs can move)?
   - Proposal: the operator sets `default_for`, and the installer asks.
4. **Per-datastore quotas** (`disk_gib` per project per datastore).
   - Needed if a fast datastore is scarce.
   - It would follow tenancy.md's per-cluster quota model (T6).
5. **NFS client options.**
   - glidex doesn't mount, but QEMU `cache.direct` on NFS needs `O_DIRECT`
     support.
   - qcow2 on NFS wants `hard` mounts.
   - Should the node probe warn on `soft` mounts, and on `actimeo` large
     enough to hide another node's writes to the marker?
6. **Mounts made after the units start.**
   - `ProtectSystem=strict` builds each unit's mount namespace at start.
   - A datastore mounted under `/srv/glidex` later has to propagate in,
     and be writable there.
   - **(verify on a host)** before relying on S11. If it doesn't
     propagate, `datastore_nodes` reports `SandboxDenied` until the
     service restarts.

## 18. Edits to other documents (not applied)

- **README.md:** add `storage.md` to the contents table (done with this
  draft) and a "shared storage" line under non-goals until S3 lands.
- **images.md:**
  - §1: lift "Remote or shared storage (NFS, Ceph, iSCSI)" once S3 lands;
  - §2: describe `local` as a datastore;
  - §3: add `Disk.datastore`, `worker` and `volume`;
  - §6: operations run on the worker;
  - §11: point to here.
- **clustering.md:**
  - D6: "bound to a node" becomes "bound to a node on a node-local
    datastore; placement follows datastore topology";
  - §9.1: the filter in §5.2;
  - §9.2: copies per location;
  - §18: move "shared storage classes", "shared image store" and "moving
    resources between hosts" here.
- **reconciliation.md §10.1:** worker rule; `Moving` phase.
- **security.md:**
  - §7: `Datastore` entity and actions;
  - §10: `ReadWritePaths=-/srv/glidex` in both units.
- **rest-api.md / grpc.md / cli.md / web-ui.md:** §13 surfaces.
- **metering.md:** per-worker sampling and `group_by=datastore`.
- **installer.md:** create `/srv/glidex` (`0755 root`). Explain that
  operators mount datastores under it, owned by `glidex` (NFS: export with
  that uid, root squash is fine).
