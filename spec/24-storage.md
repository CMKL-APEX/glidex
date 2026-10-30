# Datastores

Status: **S0–S6 and S8 implemented; S7 design.** Decisions S20–S24 and
host checks H8–H9 are open for the owner's review. Implemented:
- S0's fixes 1–4 (§15);
- S1, the driver trait (§7);
- S2, the `Datastore` resource with registered `dir` datastores (§4.3),
  with its API, gxctl, UI and watch kind (§13);
- S3, shared datastores (`shared-dir`) with workers and volume locks
  (§4.4), and images in them for linked disks plus the installer's
  registration (§4.5);
- S4, placing a stopped or lost VM again (§9);
- S5, moving a disk between datastores (§8.3);
- S6a, `glidex-stord` (§11.1), its unit and the installer's
  `--stord-allow-vg`;
- S6b, `lvm` datastores: the driver, block bindings and device access
  (§11.1 as built);
- S6c, moves to and from `lvm`, metering of volumes, and VMs booted from
  them (§11.1 as built);
- S8, `hyperstor` datastores: volumes of a hyperstor cluster, served to
  VMs over vhost-user-blk (§11.2).

S7 is still design. Decisions S25 and S27 are open for review too. The milestones are S0–S8
(§16). Decisions S13–S16 and the host checks in §19 settle the first
draft's questions (§17). The edits this document needed in other specs
are applied (§18).

Before datastores, every managed image and disk was a file in one of two
directories on the node that used it ([10-images.md](10-images.md) §2). A disk
was bound to one node, and its VM was pinned to that node
([14-clustering.md](14-clustering.md) D6). The specs marked shared storage as a
non-goal (10-images.md §1) and as later work (14-clustering.md §18: "shared
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
  - live migration (14-clustering.md §18; datastores are its prerequisite);
  - automatic failover and fencing hardware (D8 stays);
  - disk snapshots;
  - encryption at rest;
  - provisioned IOPS (13-metering.md §16);
  - per-datastore quotas (queued for later, S15).
- **Out, designed only far enough to keep the interface right:**
  - block drivers (`lvm`, `rbd`);
  - the CSI adapter.

## 2. Decisions

| # | Decision | Why |
|---|---|---|
| S1 | **CSI is not the first interface.** Shared storage starts as filesystems that the vendor or operator mounts (`shared-dir`). glidex never mounts them. glidex's own driver trait follows CSI's split between controller (create/delete/expand/publish) and node (stage/unstage), and uses CSI's topology and capability model. That way a CSI adapter is one more driver (§12, milestone S7), added only when a plugin is actually needed. | **Mounted filesystems:** nearly every vendor and open-source system offers an NFS or POSIX mount (NetApp, Pure, Dell PowerScale, CephFS, GPFS, Lustre). A mount keeps qcow2 and its features: linked clones, sparse files, `qemu-img` resize and shrink. It needs no root, because the mount is done outside glidex (README goal 4, 10-images.md §1 "No step needs root").<br><br>**CSI:** it was designed for container orchestrators. Many plugins assume Kubernetes sidecars and Kubernetes objects such as Secrets, CRDs and node labels. Node plugins run as root and mount things. A VM can only use CSI's `Block` volume mode, which gives up qcow2. Outside Kubernetes, only Nomad and a few others host CSI plugins, and their compatibility differs from plugin to plugin. The cost is high and arrives before any user asks for a particular plugin. |
| S2 | **A datastore is a cluster-scoped record in the replicated store.** Disks and image copies name their datastore by id. | Disks belong to a project and images to the cluster (11-security.md §7). Storage belongs to the operator, like nodes and networks. In a single-host install the "cluster" is the host (14-clustering.md D5), so this works the same there. |
| S3 | **The current directories become the built-in datastore `local`** (driver `dir`, on every node, default for disks and images). A disk record that names no datastore is on `local`. No file moves. As built (S1), this is a serde default rather than a schema migration: `local` is never written into a record, so records stay as older releases wrote them, and nothing needs a schema bump. The stored `local` record comes with the resource in S2. | No user sees a change until an operator adds a datastore. D19 ("ids travel") still holds, because files stay named by id. |
| S4 | **A datastore's topology says where it is reachable.**<br>- `node-local`: each node has its own separate copy; `dir`. A volume is bound to one node.<br>- `shared`: every node that reaches it sees the same files; `shared-dir`.<br>The scheduler places a VM only on a node that reaches every one of its volumes. | This generalises D6 without changing it for `local`. A shared volume has no node binding, so its VM can move when it is stopped (§9). |
| S5 | **A node reaches a shared datastore only when it finds the datastore's marker file** (`<path>/.glidex-datastore`, holding datastore and cluster id) **on a writable mount**. | An NFS export that isn't mounted leaves an empty directory on the root filesystem. Writing there would quietly put disks on the wrong disk, and different nodes would see different files. The cluster id stops two clusters from sharing one export by mistake. |
| S6 | **Exactly one node acts on a volume's files at a time:** the volume's **worker**. For a node-local volume it is the bound node. For a shared volume the leader chooses it and records it (`Disk.worker`): the VM's placement if the VM is placed, otherwise a Ready node that reaches the datastore. | Today "the disk controller on the bound node" gives this for free. With shared storage, every node's disk controller could see the record and act on it. Two `qemu-img resize` runs on one file corrupt it. |
| S7 | **Disk operations run on the worker, never on the server that answers the API call** (§8). That includes reading the partition table for shrink and extend-root. | This fixes the gaps found in §15: the API currently reads the file locally. It is also the only correct rule once files are not on the server. |
| S8 | **Linked clones need the image copy in the same file datastore as the disk.** The overlay names its backing file by a **relative** path (`../images/<id>.qcow2`). | The qcow2 header stores the backing path. An absolute path breaks as soon as nodes mount the datastore at different paths, or `GLIDEX_IMAGE_DIR` differs from node to node. |
| S9 | **A shared datastore holds one copy of each image it needs** (`image_caches/<image>/ds:<datastore>`). A `node-local` one keeps a copy on each node (`.../<node>`, as today). | This is the "shared image store" of 14-clustering.md §18 without a new service. Images stay cluster records. Only where their files live changes. |
| S10 | **Moving a VM stays explicit and offline**, and needs every disk on a shared datastore. A **stopped** VM can be re-placed (`POST /vms/{id}/place`). A **`Lost`** VM can be re-placed after `node forget --fenced`. Nothing moves automatically. | D8 still holds: a partitioned node may still be running the VM. Today only the operator's `--fenced` statement establishes that it isn't. Hypervisor file locks (§10) are a second check, not the guarantee. |
| S11 | **Mount points live under one root, `/srv/glidex/<name>`**, which both units list as `ReadWritePaths=-/srv/glidex`. Paths outside it are refused unless an operator adds a drop-in. | Registering a datastore through the API must not require editing systemd units on every node, and the control plane can't widen its own sandbox (11-security.md §10). |
| S12 | **Block volumes and a privileged storage helper come with S6** (`glidex-stord`, §11, S17–S19), on the netd pattern. | Mapping LUNs, activating LVs and `rbd map` need root. The unprivileged control plane asks a small root daemon, as it does with netd for networking. |
| S13 | **New disks default to `local`, even once a shared datastore exists.** The operator moves the default with `default_for` (`gxctl datastore set <name> --default disks`). When the installer registers a shared datastore, it asks whether that datastore should become the default; the suggested answer is no. | `local` is what every existing install uses, and it is the fastest store. Choosing shared storage (VMs can move, at network-storage speed) is the operator's call, made once, not something glidex decides for them. |
| S14 | **A datastore has one path, which each node may override** (`node_paths: {node: path}`). A node without an override uses `path`. Every path, default or override, is under the S11 root. | Most sites mount the same export at the same place everywhere, so one path covers them. Some nodes can't (an existing mount, a different vendor agent layout). Overrides cost little: backing paths are relative (S8), and the marker plus `fs_id` (§4.1) prove that every node's path reaches the same filesystem. |
| S15 | **Per-datastore quotas are queued, not built now.** Project `disk_gib` stays cluster-wide across all datastores. | No site has a scarce fast datastore yet. When one does, the quota follows 19-tenancy.md's per-cluster model (T6): `disk_gib` per project per datastore, checked where `disk_gib` is checked today. |
| S16 | **glidex locks shared volumes itself** (§10): an OFD lock on `<root>/.locks/<disk-id>.lock`, taken by the shim before the hypervisor starts and by the worker around disk operations. | Placement and claims can't stop a node that was partitioned and kept running (D8). Cloud Hypervisor's own lock comes too late for qcow2, because it repairs a dirty image before locking it (§19, H2). A separate lock file works the same for both hypervisors and for `qemu-img`. |
| S17 | **Block volumes go through `glidex-stord`, a root helper on netd's model** (§11.1): a Unix socket for the `glidex` group, newline JSON with a `hello` handshake, peer-uid authorization and a per-op policy. It acts only on volume groups the operator lists in `/etc/glidex/stord.json` (`allowed_vgs`), and only on logical volumes named `gx-d-<disk-id>` or `gx-i-<image-id>`. | Creating and activating LVs needs root, and the control plane must stay unprivileged. The allow-list and the name rule mean a compromised control plane can make and remove glidex's own volumes, but can't touch the host's root LV or another application's. |
| S18 | **Device access is per VM** (§11.1). At activation stord makes the LV's device node `glidex:glidex 0600`. Before a VM starts, stord writes a drop-in for its `glidex-vm@<id>` unit, `DeviceAllow=` for exactly its volumes, and reloads systemd. The control plane's own unit gets `DeviceAllow=block-device-mapper rw`, to make and edit glidex-owned volumes; file ownership is what limits it. | `DevicePolicy=closed` on the VM unit then still keeps each hypervisor to its own disks. `systemctl set-property` is not used: its reset line drops the unit's own devices (`/dev/kvm`) (§19, H5). |
| S19 | **The first block driver is `lvm`: node-local LVM-thin** in a volume group and thin pool the operator made (§11.1). Volumes are raw. A linked disk is a thin snapshot of the node's image LV; a full one is a copy. Disks grow, and never shrink. glidex takes no volume lock of its own on them: on one node the hypervisors' own locks refuse a second instance (§19, H6). `shared-lvm` and `rbd` come later. | Thin pools give the same thin, overcommitted, fast-clone behaviour as qcow2 files, on the device a site gives VMs. Raw block devices carry no qcow2 metadata, so H2's refcount rewrite can't happen. |
| S20 | **Glidex enrolls with a hyperstor cluster as a management identity of its own** (§11.2): the operator makes a single-use `mgmt` token (`hyperstorctl token create --role mgmt --name glidex`) and gives it to `datastore create`. The server that takes the request makes a key, joins with the token (TLS pinned to the root the token names) and seals the key with the cluster's secrets key; the record keeps the certificate. The leader renews it when a third of its life is left. | No hyperstor password or admin certificate passes through glidex, and the key never leaves it unsealed. The token is single-use and short-lived, as glidex's own join tokens are. hyperstor's audit log and access rules then see glidex as itself. |
| S21 | **Disk bytes go through `hyperstorctl write-volume` and `read-volume`**, straight to the map service and the storage nodes, holding the volume's lease: filling an image's volume (zeros skipped) and the partition code's reads and writes (§11.2). For the partition code a volume is the path `hyperstor://<map>/<volume>`. | There is no block device to give `qemu-img` or `sgdisk`, and making one (ublk) needs root. Glidex already edits partitions through a shadow file (H7); only the region reads and writes change. Holding the lease means a volume a VM has attached is refused, not corrupted. |
| S22 | **A hyperstor disk's worker is a server.** Its volume operations (create, clone, grow, delete) run where glidex's identity can be opened, whatever node its VM runs on; staging is the VM's node's, through its `hyperstor-vhost`, and needs no identity. | Agents don't hold the secrets key (19-tenancy.md §13). Staging talks only to the node's local `hyperstor-vhost`, which the node's own certificate lets in. |
| S23 | **A VM's disks preempt their leases when it was placed again after its node was forgotten as fenced** (`takeover_disks`, set by `place` for a `Lost` VM, cleared at the next launch), **and when another node's `hyperstor-vhost` holds them as the VM starts (S26)**. Every other attach waits for the holder. | hyperstor's preemption cuts off that volume's lease only (hyperstor D150): the old node's other VMs go on. A normal restart elsewhere finds the volume detached by the stop. |
| S24 | **Image volumes are named for their datastore, and the leader removes those whose image is gone.** `glidex image <image> on <datastore id>`; each probe round, the leader deletes this datastore's image volumes whose image no longer exists; one a clone still needs is left for the next round. | A deleted disk's volume goes to hyperstor's trash, where its clone still needs the image's snapshot, so deleting the image's volume with the image fails until the trash is purged. Naming them by datastore keeps two glidex clusters sharing a hyperstor pool from removing each other's. |
| S25 | **A volume a failed creation left is discarded for good, not kept in hyperstor's trash** (open for review): the driver deletes it and purges it from the trash, and before making a volume it frees the id (a leftover under it deleted, a trashed one purged and waited for, up to 2 min). The image volume is converted before it is made, so it exists only while being written, holding its lease. Retrying an image volume is bounded. A disk a person deletes still goes to the trash. | hyperstor keeps a deleted volume's id taken until it is purged, and glidex's volume ids are derived from the disk's or image's: a failed fill, deleted, made every retry fail with "exists" (on a pilot cluster the image loop retried without pause, about 100 refused calls a second into hyperstor's audit log). A failed creation's volume never held a disk's data, so the trash protects nothing there. |
| S26 | **A disk held by another node's `hyperstor-vhost` when its VM starts is a stale hold, and is preempted; each node lets go of the holds it finds for VMs placed elsewhere** (hyperstor D153): the driver names its node at every attach (`holder`), so a refused attach says who holds the volume. A VM starting here is recorded as running nowhere, so a hold labelled `vhost:<another node>` was left by a crash or a failed detach: it is preempted. A tool's hold (`hyperstorctl`, a disk operation on a server) is waited for. Each node's datastore probe (every minute) detaches the volumes its `hyperstor-vhost` holds for disks whose VM isn't placed on it; volumes glidex doesn't know are left alone. | Waiting for a hold nobody will release left the VM stuck (`DiskBusy` every 5 s) until an operator stepped in. A preemption now cuts off only that volume's lease (hyperstor D150), and a fenced old writer can't touch the disk. glidex knows a disk operation in progress only on the node running it, so the holder's label is what tells a stale VM hold from a tool's. The node's own clean-up covers a hold whose VM moved while the node was up, without a preemption. |
| S27 | **A hyperstor datastore's space is its pool's, as hyperstor reports it, and it may refuse to overcommit** (hyperstor D156; open for review): each server's probe asks the cluster API's `GetStatus` for the pool's usable `total_bytes` and `free_bytes`, trying each address until one gives them, since only the map service's leader hears the devices' reports. Agents, and a server whose cluster doesn't report them (hyperstor before D156, or a device yet to report), show 0. `overcommit: false` is accepted, and disk creation checks it against the most free space any node reports, as for any shared datastore (§5.2). | Only servers hold glidex's identity (S22), and a shared datastore's free space is the most any reachable node reports, so the servers' figures are enough. The pool's space is after its copies or erasure coding, which is what a disk takes. Pools that share devices each count the same free bytes (hyperstor D156), so refusing to overcommit protects against glidex's own disks, not against other users of the devices. |

## 3. What changes, at a glance

| Today | With datastores |
|---|---|
| `GLIDEX_IMAGE_DIR` and `GLIDEX_DISK_DIR` (environment only) | the `local` datastore. Its paths still come from those variables. |
| `Disk.node`, the node holding the file | `Disk.datastore`. `node` is set only on node-local datastores. `worker` is new (S6). |
| `image_caches/<image>/<node>` | `image_caches/<image>/<location>`, where a location is a node or `ds:<id>` (S9) |
| `is_local_disk` (`state.rs:889`) | `is_worker(disk)` |
| scheduler filter "disk bound to this node" (`scheduler.rs:114`) | "node reaches every volume's datastore, and node-local volumes are bound here or unbound" |
| `DiskBinding {path, format, backing_files}` | `VolumeBinding::File{…}` now, `Block{dev}` and `Rbd{…}` later (§11) |
| disk files never leave their node (14-clustering.md §18) | `POST /disks/{id}/move {datastore}` copies offline (§8.3) |

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
    pub direct_io: Option<bool>,     // QEMU cache.direct; None = probe (04-hypervisors.md)
    pub phase: DatastorePhase,       // Pending | Ready | Draining | Deleting
    pub created_at: u64,
    pub deletion_requested_at: Option<u64>,
}

pub enum DriverSpec {
    Dir {
        path: Option<String>,        // None = built-in local: env directories
        node_paths: BTreeMap<String, String>, // node id -> path (S14)
    },
    SharedDir {
        path: String,                // under /srv/glidex (S11)
        node_paths: BTreeMap<String, String>, // node id -> path (S14)
    },
    // Later (§11, §12):
    // Lvm { vg: String, thin_pool: Option<String> },
    // Rbd { pool: String, monitors: Vec<String>, credential: String },
    // Csi { plugin: String, parameters: BTreeMap<String, String> },
}

pub enum Topology { NodeLocal, Shared }
```

**A node's path** is `node_paths[node]` when set, else `path` (S14). The
built-in `local` takes no overrides, since its paths come from each node's
`GLIDEX_IMAGE_DIR` and `GLIDEX_DISK_DIR` already. Admission refuses:
- an override for a node that doesn't exist;
- any path outside the S11 root, or containing `..`.

A node uses only its own path. Records never store resolved paths (no
`firmware_path`-style copies, §15), so changing an override needs no
rewrite. A change takes effect at the node's next probe (§4.1). It is
refused with `409` while a running VM on that node has a volume in the
datastore open, because the hypervisor holds the old path.

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

### 4.3 As built (S2a)

`src/datastore/record.rs`, `probe.rs`, `manage.rs`.

**The record is smaller than §4.1 sketches:**
```rust
pub struct Datastore {
    id, name,
    driver: DriverSpec,          // Dir { path, node_paths } only
    nodes: Vec<String>,          // empty: every node
    default_for_disks: bool,     // S13: at most one; none means `local`
    overcommit: bool,            // default true (thin files)
    created_at,
}
```
- Not built yet: `topology` (always node-local), `content` (disks
  only), `linked_clones`, `direct_io`, `phase`. They come with the
  milestones that need them.

**`local` is still built in, not stored:**
- `list` and `find` answer with it.
- `PATCH` and `DELETE` on it are `409`.
- It is the default whenever no stored datastore has
  `default_for_disks`.
- So there is no schema migration (§14 as built).

**Per-node rows** (`datastore_nodes/<datastore>/<node>`):
- Fields: `reachable`, `reason`, `message`, `path`, `total_bytes`,
  `free_bytes`, `checked_at`.
- `reason` is one of:
  - `OutsideRoot`;
  - `Missing`;
  - `NotADirectory`;
  - `ReadOnly`, which includes a sandbox that won't let it write.
- **The probe** (`probe::probe`):
  - checks the node's path is under the root and is a directory;
  - creates `disks/` there (0700);
  - writes and removes a test file;
  - reads `statvfs` for the sizes.
- **`local` gets a row too**, for capacity: its disk directory.
- **Rows are written** only when something but the time changed:
  free space counts only when it moved by more than 1 % of the total.
- **Rows are dropped** when the node leaves the datastore's `nodes`.
- **When nodes probe:** every 60 s, when a datastore is written (in a
  cluster, `cache_sync`), and right after create and update.
- **Row writes are status writes** (spec/14-clustering.md §8.3). The leader
  accepts only the node's own key (`check_status_write`). A server that
  isn't the leader sends them like an agent (`fix/follower-status-writes`).

**Feature level:**
- `FEATURE_LEVEL` is 2. A datastore can be created only once the
  cluster's level (the lowest any server reports) is 2; until then it
  is `409`.
- The two tables reach only nodes reporting level 2
  (`SHARED_DATASTORES` in `cluster/sync.rs`), so an agent one release
  behind never gets a write set naming a table it doesn't know.

**Disks:**
- `CreateDiskRequest.datastore` takes an id or a name; the default is
  as S13 says.
- A VM's owned root disk goes to the default.
- A disk bound to a node outside the datastore's `nodes` is `400`.
- `ImageManager::driver` resolves a registered datastore to a
  `FileDriver` over `<this node's path>/disks`.
- `disk_path`, for responses and metering, goes through it too.

**Scheduler:** for each unbound disk on a registered datastore, the
`Request` gets the nodes whose row is reachable and that are in its
`nodes` (`disk_reach`). Any other node is refused with "datastore X is
not reachable here".

**Capacity** (§15 fix 5):
- Applies only to datastores with `overcommit: false`.
- The disk's expected size must fit in the best reachable node's free
  bytes, minus the disks still pending or being made there.
- Otherwise the create is `507 insufficient_storage` (RPC
  `ResourceExhausted`), with `free_bytes` and `needed_bytes` in
  `details`.
- `local` overcommits, so it behaves as before.

**Changing a datastore:**
- A `node_paths` change is refused (`409`) while a running VM on that
  node has a disk of it open.
- Dropping a node from `nodes` is refused while a disk of it is bound
  there.
- Delete is refused while any disk names it, checked in the same write.

**Detach:** a node holding disks on a registered datastore can't leave
with them (`409`). The other cluster has no record of the datastore.

**Not yet:** a departed or forgotten node's rows stay until its
datastore is deleted. The leader should prune them as it does image
copies (`prune_departed_copies`).
- Until it does, a stale row can make the scheduler try a node that is
  gone. That node is refused there anyway, as not ready.

### 4.4 As built (S3a): shared datastores

**Driver** `shared-dir` (`DriverSpec::SharedDir { path, node_paths }`,
topology `Shared`):
- proto `DATASTORE_DRIVER_SHARED_DIR` and `DATASTORE_TOPOLOGY_SHARED`;
- gxctl `datastore create --shared`;
- the UI form's "Shared" box.

**Marker and claim** (`probe::claim`, S5):
- `.glidex-datastore` holds `{datastore, cluster, created_at}`.
- **Who claims:** one node only, the first by id among the datastore's
  ready nodes, and only while no node reaches it.
- **How:** it writes the marker into an empty directory (`lost+found`
  aside) with `O_EXCL`.
- **What other nodes find:**
  - the marker of this datastore and cluster: reachable;
  - no marker: `NoMarker`, "is it mounted on this node?";
  - another datastore's or cluster's marker: `ForeignMarker`;
  - files but no marker, even for the claimer: `NotEmpty`. It is never
    claimed; `adopt` is not built.
- **Why only one claimer:** an unmounted mount point is an empty local
  directory. If every node could claim, a node whose mount is missing
  would put disks only it sees. With one claimer, if the claimer's own
  mount is missing it reaches the datastore alone ("1 of N" in the UI),
  which is visible and fixable.
- **Re-probing:** a node probes again when another node's row changes,
  so the rest find the marker at once rather than a minute later.
- The probe also makes `.locks/`.
- **Mount warnings** (§17): a node that reaches a shared datastore
  looks up its mount in `/proc/self/mountinfo`. On NFS it warns, in the
  row's `warnings`, about `soft`/`softerr` (a server outage fails the
  guests' disk writes) and about locks kept on the node (`nolock`,
  `local_lock=` other than `none`), with which the volume locks (S16)
  no longer keep two nodes off one disk. The datastore stays reachable;
  gxctl `datastore show` and the Datastores page show the warnings.

**Disks** (S6):
- A disk on a shared datastore has no `node` (pinning one is `400`) and
  a `worker`.
- **Setting the worker:**
  - at create: this node if it reaches the datastore, else the first by
    id that does;
  - at placement, VM create or attach: the VM's node (`Disk::bind_to`);
  - otherwise the leader moves it in its disk round (`pick_worker`): to
    the VM's node while placed; it keeps the worker while that node is
    ready and reaches the datastore; else to this node, then the first
    that reaches it.
- `is_local_disk` means worker == this node.
- **Node sync** (`disk_is_nodes`):
  - a worker may write its disk's status, as a bound node may;
  - shared disk records go to every node, so a former worker hears it
    no longer is.
- The proto `Disk` gains `worker` and `worker_name`; the UI's Node
  column shows `shared · <worker>`.
- **Scheduler:** the existing `disk_reach` (§4.3) covers shared disks,
  since they are always unbound: a VM goes to a node whose row
  reaches the datastore.
- **Linked clones:** in S3a a disk from an image on a shared datastore
  was made `Full`. S3b adds linked clones there (§4.5).

**Volume lock** (S16, §10):
- `VolumeBinding::File` carries `lock: Option<String>`,
  `<path>/.locks/<disk-id>.lock` on a shared datastore.
- `StorageDriver::lock` takes an OFD write lock without waiting
  (`glidex_vm_shim::util::try_lock_file`), and returns `Busy` when it
  is held.
- **Disk operations hold it:** create (`materialize`), resize, shrink,
  extend-root and delete.
- **The shim holds it:** `LaunchFile.volume_locks` lists the VM's shared
  disks' locks. The shim takes each after validating the launch and
  before the hypervisor starts, holds them until it exits, and refuses
  the launch (`LaunchFailed`, "... is locked") if one is held.
- QEMU opens shared disks with `locking: on`.
- **Found while testing:** a child forked by another thread holds a
  copy of the lock's descriptor until it execs (close-on-exec acts only
  then), so a just-released lock can read as held for a moment. A disk
  operation then gets `Busy` and its controller retries.

**Tests:**
- unit: `a_shared_directory_is_claimed_once_and_found_by_its_marker`,
  `a_shared_volume_is_locked_once`, the QEMU `locking` assertion, and
  the shim's `a_file_lock_is_held_once`;
- shim: `a_locked_disk_stops_the_launch`;
- API: `a_shared_datastore_is_claimed_and_its_disks_are_bound_to_no_node`;
- cluster: `a_shared_datastore_serves_every_node_and_its_disks_follow_their_vm`.
  Both nodes reach one directory, claimed once. The disk is made by the
  leader as its first worker. Its worker becomes the agent when a VM on
  the agent takes it, and the agent's launch file carries its lock.

**Not built in S3:**
- the leader comparing nodes' `fs_id` (`Diverged`);
- `adopt` for a non-empty directory;
- re-placing a VM (S4) and moving a disk (S5).

### 4.5 As built (S3b): images in shared datastores, the installer

**The datastore's copy** (`controller/store_copies.rs`, S8, S9):
- Path: `<path>/images/<image-id>.qcow2` (0444), from
  `Datastore::image_copy_path`.
- Row: `image_caches/<image>/ds:<datastore>`. Rows of this kind are
  kept out of the per-node functions (`image_caches`, the leader's
  pruning, `tend_image_copies`), so node copies behave as before.
- **Who makes it:** the worker of a linked disk waiting on the datastore
  (`stores_needing`), when the image is ready. It:
  - takes the datastore's lock for the image
    (`.locks/image-<id>.lock`); another node copying makes it wait;
  - takes a file already there if its digest matches;
  - otherwise copies from this node's own copy, else from a node with a
    ready one (as node copies are fetched);
  - checks the digest, renames into place, and writes the row:
    `Downloading`, then `Ready` or `Failed`.
- **What triggers it:** the image controller runs this on every node
  for a ready image. A disk whose `materialize` is `NotReady` queues its
  image's key, so the copy starts as soon as a disk needs it.
- **Node sync:** any node whose row reaches the datastore may write
  that datastore's copy row (`check_status_write`).

**Linked disks:**
- `materialize` overlays the datastore's copy for a linked disk on a
  shared datastore. Until that copy exists the disk is `NotReady`
  ("not copied into datastore D yet").
- The overlay names the copy by the relative path
  `../images/<id>.qcow2`, which is right from any node's mount.
- Full clones still read this node's own copy.
- The image can't be deleted while a linked disk uses it (10-images.md §3).

**Deleting an image:**
- Every node that reaches a datastore holding a copy removes the file
  and the row (`drop_store_copies`).
- The leader removes the row of a datastore that no longer exists.
- In a cluster, the image record goes only once both node copies and
  datastore copies are gone.

**API and UI:** `ImageCopy` gains `datastore` and `datastore_name`,
with `node` empty. The Images page lists datastore copies apart from
node copies, with no Remove button: they go with the image.

**Installer** (S13):
- `--datastore NAME=PATH` registers a shared datastore;
  `--local-datastore NAME=PATH` registers one each node has its own of.
- `--datastore-default` makes the first of them the default for new
  disks.
- None of these are saved: registering is done once.
- **Interactive run** without them: it finds the mount points directly
  under `/srv/glidex` (`/proc/self/mountinfo`) without a marker, and
  asks for each:
  - whether to register it as a shared datastore;
  - whether it becomes the default (suggested answer: no).
- It runs `gxctl datastore create` on the local socket, retrying while
  the control plane starts. gxctl exits 0 on errors, so the installer
  reads its output.

**Fixed from S3a:** a worker that isn't the leader couldn't record the
events of a disk it made. The status-write rule for
`events/disk/<id>` still checked only `node`, and now follows the
worker too (`disk_is_nodes`). S3a's test didn't hit it, because the
leader made the disk there.

**Tests:**
- API: `a_linked_disk_on_a_shared_datastore_overlays_the_datastores_copy`
  checks the copy, the overlay's relative backing path, the copy in
  `GET /images/{id}`, and the copy going with the image;
- cluster: `a_worker_puts_the_image_in_a_shared_datastore_for_a_linked_disk`.
  The image is only on the server, and the agent, the disk's worker,
  brings it into the datastore from the server;
- installer: `datastores_to_register_are_one_time` and
  `unregistered_mounts_are_those_under_the_root`.

### 4.2 Changes to `Disk` and `Image`

```rust
pub struct Disk {
    // ...existing fields (10-images.md §3)...
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
linked disk in that datastore uses it. This extends 10-images.md §3's
`ImageHold` from the image to the copy.

**Invariant.** `node` is `None` exactly when the datastore is `Shared`.
`worker` is `node` on node-local datastores.

### 4.3 Paths and layout

```
<datastore root>/
├── .glidex-datastore       # {"datastore": "<id>", "cluster": "<cluster id>", "created_at": …}, 0444
├── images/<image-id>.qcow2 # as 10-images.md §2, 0444
├── disks/<disk-id>.qcow2   # 0600; linked overlays back onto ../images/<image-id>.qcow2
└── .gx-tmp/                # dot temporaries, same filesystem (10-images.md §2)
```

- The `local` datastore keeps today's two directories and has no marker.
  It is node-local, so the node-local rule applies and nothing else can
  reach it.
- Every invariant of 10-images.md §2 still holds within a datastore:
  - names are by id only;
  - orphan files are reported, never deleted;
  - a record whose file is missing is marked missing.
- On a shared datastore, only the file's **worker** reports it missing.
  A node that can't reach the datastore reports nothing about it.

## 5. Reachability and placement

### 5.1 Creating a shared datastore

1. `POST /datastores {name, driver: {shared-dir: {path}}}` records the
   datastore in phase `Pending`.
2. The leader picks one Ready node that has its path (S14) writable and
   asks it to **claim** the directory:
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
atomic: checked on a host across two NFSv4.2 clients (§19, H3).

### 5.2 Scheduler

The scheduler's `Request` (`state.rs:678`) gains each volume's
`(datastore, node)`.

**Filter:** a node passes when, for every volume:
- the volume's datastore is in `nodes` and `datastore_nodes/<ds>/<node>` is reachable; and
- if the datastore is node-local, the volume is unbound or bound to this node.

**Bind** (unchanged for node-local, 14-clustering.md §9.1): unbound node-local volumes bind to the chosen node in the same write. Shared volumes get `worker` set to the chosen node.

**Score:** prefer nodes with free space on node-local datastores of unbound volumes. A shared datastore scores the same on every node.

**Capacity:** disk create is refused with `507 insufficient_storage` when the target datastore's free space minus the not-yet-created disks recorded there is below the new size, unless `overcommit: true` is set on the datastore. Creating a disk works out the free space for a shared datastore from the reachable node with the most (they agree, §4.1, except that a hyperstor datastore's agents report 0: S27). For a node-local one it uses the node it binds to, or the best node when it is unbound.

### 5.3 Host paths

Raw host paths (`rootfs_path` and the others, 11-security.md §7.4) stay as
an admin escape hatch. The scheduler still doesn't know where those files
exist (a gap noted in 14-clustering.md). A VM with host paths should be
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

**Firmware and kernel images** ([23-kernel-boot.md](23-kernel-boot.md) §4) follow the same rule. Launch resolves the firmware
path on the placed node (§15, bug 1).

**Linked overlays.** New overlays are created with a relative backing
path (`-b ../images/<id>.qcow2 -F qcow2`, resolved from the overlay's
directory). Existing overlays in `local` keep their absolute paths. They
stay correct as long as `local` never moves, which S3 guarantees. A
`move` (§8.3) rebases them (`qemu-img rebase -u`).

**Dedup.** On import (14-clustering.md §5.9), an image whose sha256 matches
a Ready copy on a shared datastore can share that file. This settles
14-clustering.md O6 for shared datastores only. Two records still keep two
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

**As built (S1)** (`src/datastore/`):
- **Blocking methods.** The trait's methods block and are called from
  the same `spawn_blocking` contexts as before, not `async` as sketched
  above. A CSI or RBD driver can still wrap async calls behind them.
- **Methods so far:** `capabilities`, `binding`, `exists`,
  `create_volume`, `delete_volume`, `expand_volume`, `shrink_volume` and
  `stage`.
  - `create_volume` and `shrink_volume` take a `prepare` callback, run on
    the new or shrunk volume before it becomes the disk's. Extend-root and
    the GPT backup-header move run there. On the file driver that is
    today's temporary-then-rename; a failure leaves nothing behind.
  - `unstage`, `capacity`, `probe` and `publish` come with the milestones
    that use them (S2, S3, S6).
- **Driver lookup.** `ImageManager::driver(disk)` returns the driver of
  the disk's datastore. A disk on any datastore but `local` is refused
  (`NotReady`) rather than looked for in the local directory.
- **What goes through the driver:** every change to a disk (create,
  grow, shrink, extend-root, delete), staging at launch, and the missing
  check in the disk controller.
- **What still reads the `local` file path directly**
  (`ImageManager::disk_path`): responses (`path`, `info`, the partition
  table), metering's `disk.stored`, the startup orphan scan, the API's
  early shrink and extend-root checks, and the `rootfs_path` filled in for
  managed root disks. These are file-only until block drivers (S6).
- **`DiskBinding` is gone;** `VmConfig` carries `VolumeBinding`s.

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
  operation already writes to a dot temporary and renames (10-images.md §2),
  so a half-done operation on the old worker leaves only a temporary. A
  new worker deletes temporaries older than the reassignment.
- A worker that comes back after losing the role must not finish its
  rename:
  - every operation runs under the volume lock (S16), so a new worker
    can't start until the old one's lock is gone: released, or dropped
    by the kernel or the NFS server when the process or its lease dies;
  - before the rename, the old worker re-reads `worker` under that
    lock, and abandons the operation if it changed.

### 8.2 API side

- `POST /disks/{id}/resize` and `/extend-root` stop reading the file on
  the answering server (§15, bug 2).
- They record the request.
- The checks that need the file (minimum shrink size, growable root
  partition) run on the worker. The result comes back as the `Ready`
  condition (`ResizeInvalid`, already in 12-reconciliation.md §10.1) and the
  `?wait` outcomes (06-rest-api.md).
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

This also gives 14-clustering.md §18's "moving resources between hosts"
for node-local disks: moving from `local` on node A to `local` on node B.

**As built (S5).** One node, the **mover**, does the whole move: it
holds the source file and reaches the destination. There is no copy
over the cluster port.
- **Who moves it** (`VmManager::move_disk`):
  - standalone: this node;
  - a node-local disk: its `node`;
  - a shared disk: its `worker` if that reaches the destination, else a
    node reaching both datastores (the VM's node when the disk's VM is
    placed).
- **Node-local to another node's node-local is refused**, with the hint
  to move it to a shared datastore first and then from there. `node`
  may only name the mover. The between-nodes copy above is not built.
- **Refused also:** a disk being deleted, already moving, not `Ready`,
  or in use by a running VM; the datastore it is on already; a VM whose
  node wouldn't reach the disk where it goes; a destination without
  overcommit that lacks the space on the mover.
- **The record:** `Disk.moving = {datastore, node, mover, flatten}`
  instead of a `Moving` phase; the API shows it as `moving_to`. The
  disk stays `Ready`. Attach, resize, extend-root and boot refuse a
  moving disk. A shared source's `worker` becomes the mover, and the
  leader doesn't pick another worker while it moves.
- **The copy** (`StorageDriver::copy_in`, `images::disk::move_volume`):
  - under both volume locks (S16);
  - into a temporary file in the destination, by `qemu-img convert`
    when flattening, else a sparse copy;
  - checked (`qemu-img check`) and renamed into place;
  - a linked disk not flattened is rebased (`qemu-img rebase -u`) onto
    the destination's image: the shared datastore's copy, by the
    relative path `../images/<id>.qcow2`, or the mover's own copy. A
    copy into a shared datastore is made first, as for a new linked
    disk (§4.5); until then the move waits (`MovePending`, retried
    every 5 s).
- **Then:** one write sets `datastore`, `node`, `worker` (the mover on
  a shared destination), the origin's mode (`full` when flattened) and
  clears `moving`; the event is `Moved`. The mover then deletes the
  source file. A failed copy clears `moving` with a `MoveFailed` event
  and leaves the disk where it was.
- **Node sync:** the mover writes the record as status
  (`check_move_write`): only to the exact `moving` spec, with no other
  change.
- **API:** `POST /disks/{id}/move {datastore, node?, flatten?,
  wait_secs?}` (`moveDisk`, in `disk.write`; the caller also needs
  `useDatastore` on the destination). gxctl `disk move NAME
  --datastore D [--node N] [--flatten]` waits up to 300 s. The Disks
  page has a Move form when there is more than one datastore.
- **Tests:** API `disks_move_between_datastores` (blank disk to shared
  and back, a linked disk rebased then flattened, refusals); cluster
  `a_node_local_disk_moves_to_a_shared_datastore_and_its_vm_can_go_anywhere`.

## 9. Placement after creation: re-placing VMs

- **Stopped VM:** `POST /vms/{id}/place {node?}`. Allowed when every disk is on a shared datastore, or is node-local and already on the target node.
  - It clears `status.placement`, and runs the scheduler with the pin if given.
  - It moves each shared disk's `worker` to the new node and re-binds the VM's node-scoped resources (node networks are refused; OVN ports move, as at start).
  - Needs `placeVm` (operators, §13).
- **`Lost` VM** (after `node forget --fenced`, 14-clustering.md §5.6): the same call is allowed, which is what that section anticipated.
- **Running VM:** refused with `409 invalid_state`. Live migration is out of scope.

**As built (S4)** (`VmManager::place_vm`, `controller/placement.rs`):
- **Which VMs:**
  - a stopped VM: desired power stopped, no live instance, phase
    `Stopped` or `Failed`;
  - a Lost one: its placement names a node that is a tombstone or gone.
  - Anything else is `409 invalid_state`.
  - A standalone host is `400`: there is nowhere else to go.
- **What can go with it** (else `400`, naming the cause):
  - every disk on a shared datastore, or node-local and already on the
    target node (never for a Lost VM: its node's disks are gone);
  - no host-path root;
  - no PCI devices;
  - no `scope: node` network.
- **What it does:**
  - clears `status.placement` and the `Scheduled` condition, and the
    `Ready/Lost` condition too;
  - for a Lost VM, also clears the dead node's instance record and sets
    the phase `Stopped`;
  - sets `spec.node` to the target, or clears it, since a pin to the old
    node would keep it there. It writes all this as a spec change
    (`Replacing` event);
  - runs a placement pass at once (`place_pending`), which places the VM
    as it would a new one and moves its shared disks' workers
    (`Disk::bind_to`). The answer is the VM: placed, or waiting with its
    `Scheduled` reason.
- **API:**
  - REST `POST /vms/{id}/place {node?}` (node id or name) and RPC
    `VmService.PlaceVm`;
  - gxctl `place <vm> [--node N]`;
  - the VM page's "Place on another node" ("on a live node" when Lost)
    for those with `placeVm`.
- **Cedar:** `placeVm` is in `node.drain` (system-admin) and is
  `host-scoped`. Moving a VM between nodes is cluster operations, not
  the tenant's.
- **Left on the old node:** files under the VM's runtime directory, and
  for QEMU its UEFI variable store (`firmware-vars/<vm>.fd`). On the new
  node the VM starts with a fresh store from its firmware image's
  template, so UEFI variables don't move.
- **Test:** `cluster_tests.rs::a_vm_on_shared_disks_is_placed_again_when_stopped_or_lost`:
  - a stopped VM moves from the agent to the server, with its disk's
    worker;
  - one with a node-local disk is refused;
  - after the agent is stopped and forgotten, its Lost VM is placed on
    the server.

## 10. Exclusive access

The invariants of 10-images.md §3 and 12-reconciliation.md D11 (one VM per
disk, claims) don't change. On shared storage they are what stops two
nodes from writing one disk. Three more layers back them up:

1. **Placement is the lock.** A node launches a VM only if `placement`
   is itself. It acts on a disk only if it is the `worker`. Both are
   Raft writes. A partitioned node doesn't launch (14-clustering.md §8.2),
   but it can keep a VM that is already running (D8). That's why S10
   needs `--fenced` before a `Lost` VM is re-placed.
2. **glidex's own volume lock (S16).**
   - Each volume on a shared datastore has a lock file,
     `<root>/.locks/<disk-id>.lock`.
   - The shim takes an `fcntl` OFD write lock on it **before** it starts
     the hypervisor, and holds it until the hypervisor exits.
   - The worker takes the same lock around every disk operation
     (create, resize, shrink, extend-root, move, delete).
   - If the lock is taken, the launch fails with `Ready=False/DiskLocked`
     and the operation waits. Nothing opens the disk file.
   - The lock is on a separate file, not the disk itself. The hypervisor
     takes its own OFD lock on the disk file, and a lock held by the shim
     on that same file would refuse it.
   - OFD locks over NFSv4 are cluster-wide (§19, H2). On a node-local
     datastore, glidex keeps today's protection (placement and claims)
     and takes no lock file.
3. **Hypervisor file locks, the last layer.**
   - QEMU takes an OFD lock on its image files before it reads or writes
     them (`locking=auto`, which is on for local files and NFS). glidex
     sets `locking=on` for shared datastores. A second QEMU, even on
     another NFS client, fails with `Failed to get "write" lock` and
     writes nothing (H2).
   - Cloud Hypervisor (v53) also takes an OFD write lock on every disk
     file (`lock_granularity=byte-range` by default), and a second
     instance fails with `AlreadyLocked` (H2). It is **not a safe
     backstop for qcow2**, though:
     - it opens and parses the image before it locks;
     - an image another instance has open is marked dirty, so it
       rebuilds and **writes** the refcount tables and header first;
     - it writes the header again on its way out.

     That is metadata written under a running VM (H2). Raw images have
     no metadata, so the lock alone is enough for them. This is why
     layer 2 exists: the shim's lock keeps a second Cloud Hypervisor from
     ever opening the file. We should report it upstream (lock before
     the qcow2 parser runs).
4. **Leases are later.** Protection that doesn't depend on locks
   (sanlock-style leases, RBD exclusive locks) comes with fencing
   (14-clustering.md §18).

## 11. Block drivers and `glidex-stord`

S6 builds `glidex-stord` and the `lvm` driver (§11.1). The rest of this
section is still a sketch, so the interface (§7) and the record (§4)
stay right:

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
  - a root helper on netd's model (09-networking.md §7): a Unix socket,
    peer-uid authorization, its own state database;
  - it maps and unmaps devices and fixes their ownership for the
    `glidex` user;
  - `glidex-vm@` gains `DeviceAllow=` for exactly the devices staged for
    that VM, through a per-VM drop-in written by stord.
- **Block volumes and partition edits:** with no qcow2, shrink is
  refused, `extend-root offline` works on the device, and metering's
  `disk.stored` comes from the driver (thin-pool usage, `rbd du`).

### 11.1 Design (S6): `glidex-stord` and `lvm`

**The operator prepares the volume group** (S19). glidex never touches
physical volumes:

```bash
sudo vgcreate vg-glidex /dev/nvme1n1
sudo lvcreate --type thin-pool -l 95%FREE -n pool vg-glidex
```

and lists it in `/etc/glidex/stord.json` (`{"allowed_vgs":
["vg-glidex"]}`, or the installer's `--stord-allow-vg vg-glidex`).

**The datastore** (S2): driver `lvm`, topology `node-local`, with
`vg` and `thin_pool` instead of `path`; `nodes` limits it as for `dir`.
Every node on it must have a VG of that name (`node_vgs` overrides it
per node, as `node_paths` does for `dir`).

**`glidex-stord`** (S17), `crates/glidex-stord`:
- **Sockets:** `/run/glidex-stord/stord.sock` (`root:glidex 0660`; its own runtime directory, apart from netd's `/run/glidex`), and
  `stord-admin.sock` for `glidex-admin` when that group exists, as netd.
- **Protocol:** netd's (§7.3 of 09-networking.md): one request per line,
  `{id, op, args, on_behalf_of?}`, `hello {protocol}` first.
- **State:** none of its own. The volumes are in LVM, and the devices
  each VM may open are its drop-ins, under `/run`, which a reboot clears.
- **Checks on every op:** the VG is in `allowed_vgs`; a volume name is
  `gx-d-<uuid>` or `gx-i-<id>` (`[A-Za-z0-9._-]`, no `..`); a thin pool
  name is a plain LVM name. Anything else is refused before any command
  runs.
- **Ops:**

  | Op | Does |
  |---|---|
  | `hello`, `probe` | Handshake; which LVM tools there are, and their version |
  | `pool_status {vg, pool}` | Whether the VG is allowed and both exist; pool size, data and metadata use |
  | `list_volumes {vg}` | glidex's LVs: name, size, origin, active, open, data use |
  | `create_volume {vg, pool, name, size_bytes}` | A thin LV, not skipped at activation, activated and handed to `glidex` |
  | `snapshot_volume {vg, origin, name, size_bytes}` | A thin snapshot of an image LV (`--setactivationskip n`), grown to `size_bytes` |
  | `seal_volume {vg, name}` | Make an image LV read-only once filled |
  | `extend_volume {vg, name, size_bytes}` | `lvextend`; never smaller |
  | `activate {vg, name}`, `deactivate {vg, name}` | `lvchange -ay -K` and `chown glidex:glidex`, `chmod 0600`; or `-an` |
  | `delete_volume {vg, name}` | Refused while open, or while a snapshot uses it; then `lvremove` |
  | `allow_devices {vm_id, devices}` | Write `/run/systemd/system/glidex-vm@<id>.service.d/50-glidex-volumes.conf` with one `DeviceAllow=<dev> rw` each, then `systemctl daemon-reload`; an empty list removes it |

**The driver** (`datastore/lvm.rs`, the §7 trait), run on the disk's
node:
- `create_volume`, blank: `create_volume`. From an image, **full**: a
  new volume, then `qemu-img convert -n --target-is-zero -O raw` from
  this node's verified image file into it. **Linked**: a thin snapshot of
  the node's image LV `gx-i-<image>`, which is made the first time from
  the image file the same way and sealed (under a per-image lock on the
  node). `prepare` (extend-root) runs on the device.
- `expand_volume`: `extend_volume`. `shrink_volume`: refused.
- `delete_volume`: `deactivate`, then `delete_volume`.
- `stage`: `activate`, which returns the device:
  `VolumeBinding::Block { dev: /dev/<vg>/<lv> }`.
- `copy_in` (moves, §8.3): a new volume, then `qemu-img convert` into it
  (always flattened); moving off `lvm` converts the device into a file.
- `capacity`: `pool_status`. `disk.stored` (metering): the LV's size ×
  its data use.
- **Partition edits** (extend-root, shrink checks): on a block binding,
  `partition::edit` uses its shadow file for raw too: `growpart` can't
  work on a device-mapper device that has no partition devices (§19, H7).

**Launch** (VM controller, on the node): `stage` each block volume,
then `allow_devices` for the VM before the unit starts. Stopping a VM
leaves its volumes active; deleting a disk deactivates and removes its
volume. The VM's drop-in goes with the VM. Under the process runner
(tests), ownership alone gives access.

**Hypervisors:** Cloud Hypervisor `--disk path=<dev>,image_type=raw`;
QEMU `driver: host_device`, `locking: on`, `cache.direct: on`.

**The probe** (§4.1): a node asks stord for `pool_status`. Reasons:
`StordUnavailable`, `NotAllowed` (VG not in `allowed_vgs`),
`NoVolumeGroup`, `NoThinPool`. Free space is the pool's.

**Image deletion:** every node removes its image LVs that no snapshot
uses (`list_volumes`), as `drop_store_copies` does for shared copies.
Linked disks keep the image from being deleted (10-images.md §3).

**Datastore deletion:** each node's probe lists the image volumes it
keeps in the datastore (`NodeRow.image_volumes`, the API's
`DatastoreNode.image_volumes`). Deleting the datastore is refused
(`409`, naming the images and nodes) while any row lists one, in the
same write that checks for disks: nothing would remove them afterwards.
They go when their images are deleted, or by hand (`lvremove`); the
next probe (within a minute) then lets the delete through. Found on the
host check after S6.

**As built (S6b):**
- **API:** driver `DATASTORE_DRIVER_LVM` with `volume_group` and
  `thin_pool` (`path` empty); `node_paths` names each node's own volume
  group. Record: `DriverSpec::Lvm {vg, thin_pool, node_vgs}`. gxctl
  `datastore create NAME --lvm VG/POOL`; the Datastores page's form has
  *LVM-thin on each node*.
- **Disks** on it are raw: a request for qcow2 is refused, and a linked
  disk may be raw there (a thin snapshot).
- **Volumes are built under `gx-d-<id>.new`** and renamed into place
  (stord `rename_volume`), so a volume under the disk's own name is
  complete, as a file is after its rename: a crash part way leaves the
  `.new` one, which the next attempt removes. An image volume left
  unsealed is made again.
- **`exists`** asks stord (an inactive volume has no device node);
  `stage` activates.
- **Launch:** the VM controller sends `allow_devices` with the VM's
  block volumes before the unit starts (with none, only where stord
  runs); deleting the VM sends an empty list.
- **The control plane's unit** has `DeviceAllow=block-device-mapper rw`
  (S18).
- **Probe:** a node's row path is `<vg>/<pool>`; free space is the
  pool's; a pool whose metadata is 80% full gets a warning.
- **Not checked:** a VM booted from a volume under the systemd runner of
  a host install (its drop-in and the hypervisors on LVs were checked
  apart, §19 H5–H6; the boot tests below use the process runner).
- **Tests:** API `lvm_datastores_name_a_volume_group_and_keep_raw_disks`;
  ignored `lvm_disks_on_a_real_pool` (with a running stord and a VG: a
  blank volume grown and not shrunk, a linked disk as a snapshot with
  its root partition extended through the shadow file, both removed and
  the image volume with the image) and stord's `real_lvm`, both passed
  on a loop-device VG; hypervisor argument tests for block disks.

**As built (S6c):**
- **Moves** (§8.3) onto or off an `lvm` datastore are always flattened
  (a raw volume has no backing file), and onto one make the disk raw:
  `MoveSpec.raw`, which the mover's status write may apply and nothing
  else (`check_move_write`). `copy_in` takes the source's format.
- **Metering:** `StorageDriver::stored_bytes` gives `disk.stored`; for a
  volume it is the pool's blocks it holds (stord's `data_percent` × its
  size), for a file still `qemu-img`'s actual size.
- **Probe:** a pool 80% full gets a warning too: a full thin pool stalls
  every guest on it at its next write (seen when two test runs shared a
  3 GiB pool).
- **Tests:** `lvm_disks_on_a_real_pool` also moves a qcow2 disk from
  `local` onto the volume group and back (raw both times, partitions
  intact); `lvm_root_disk_boots` and `qemu_lvm_root_disk_boots` boot a
  cloud image from a linked volume grown to 6 GiB, log in, see all of
  it, and remove the disk, then the image's volume with the image. All
  passed on the loop-device VG.

### 11.2 The `hyperstor` driver (S8)

A `hyperstor` datastore is a pool of a
[hyperstor](https://github.com/CMKL-APEX/hyperstor) cluster: a disk is
a replicated hyperstor volume, served to its VM by the node's
`hyperstor-vhost` over vhost-user-blk. Its topology is shared: a VM can
run on any node with `hyperstor-vhost`, and one placed again after its
node is lost reaches the same volume.

**Record.** `DriverSpec::Hyperstor { api, map, pool, identity }`:
- `api`: the cluster API's addresses, tried in turn;
- `map`: the map service's addresses, from the join's answer;
- `pool`: the data pool, checked at enrollment (`GetStatus`; not the
  map's pool 0);
- `identity`: the certificate, its root, the identity's URI and its
  expiry. The key is sealed with the secrets key under
  `hyperstor:<datastore id>` (S20) and removed with the datastore.

**Volumes:**
- A disk's volume id is its UUID's 32 hex digits; its serial (what the
  guest sees in `/sys/block/vda/serial`) the first 20.
- **Blank**: `CreateVolume`. **Full** from an image: a volume filled
  from the image converted to raw (`qemu-img convert`, then
  `hyperstorctl write-volume --sparse`). **Linked**: a clone of the
  image volume's snapshot `golden`, then `ExpandVolume` to the disk's
  size.
- **The image volume** (one per image and datastore, its id derived
  from the image's) is made by the first linked disk that needs it:
  filled, then snapshotted. A second worker finding it waits while its
  filler holds the lease, and redoes one left unfinished (no `golden`,
  not attached).
- There is no rename, so a volume can't be built aside. A volume under
  the disk's id before the disk is ready is a failed attempt's, and is
  deleted first.
- `prepare` (extend the root partition) runs on the tool binding: the
  partition code reads and writes regions through `hyperstorctl` with
  `--map` (S21).
- Shrinking is refused; moves onto and off hyperstor are refused
  (`copy_in`, `move_volume`) until a copy path exists.

**Staging** (the VM's node):
- `attach_for_vm` asks `hyperstor-vhost`'s control socket
  (`/run/hyperstor/vhost/control.sock`, newline JSON) to attach the
  volume with the disk's serial and this node's id (`holder`), `take:
  "preempt"` for a VM placed again from a fenced node (S23); it answers
  with the socket the hypervisor opens. A volume held by a tool is
  `Busy`, so the VM waits; one held by another node's `hyperstor-vhost`
  is preempted (S26). Each node's probe detaches what its
  `hyperstor-vhost` holds for VMs placed elsewhere (S26).
- `VolumeBinding::VhostUserBlk { socket, volume, map }`. Cloud
  Hypervisor gets `--disk vhost_user=on,socket=…,num_queues=1`; QEMU a
  `socket` chardev that reconnects (H8) and `vhost-user-blk-pci` with
  `num-queues=1` (its default is one per vCPU, more than
  `hyperstor-vhost` offers by default). Both need shared guest memory,
  which the VM gets whenever a disk is vhost-user.
- The VM's staged disks are recorded (`staged_disks`); stopping and
  deleting the VM detach them.
- The disk is 512e: 512-byte logical blocks on the volume's 4 KiB
  blocks (hyperstor D126), so cloud images' partition tables and
  firmware work (H9).

**Probe.** Each node: `hyperstor-vhost` answers `hello`
(`VhostUnavailable` otherwise). Each server also: the API answers with
the identity (`ApiUnavailable`), and gives the pool's usable total and
free bytes (S27), from the first address whose member knows them. Sizes
are 0 on agents, and on servers when hyperstor doesn't report them (before
hyperstor D156, or while a device is yet to report). `overcommit: false`
is accepted and checked against them.

**Images on hyperstor** use the image volume, not a copy of the image
file: `Datastore::holds_image_files` is true only for `shared-dir`
(store copies and a linked disk's backing file).

**Depends on** hyperstor's `mgmt` tokens, `ExpandVolume` and
`hyperstorctl write-volume`/`read-volume` (hyperstor D122–D124), and
512e in `hyperstor-vhost` (D126). glidex depends on `hyperstor-api` and
`hyperstor-pki` as pinned git dependencies.

**As built (S8):**
- **API, gxctl, UI:** `driver: DATASTORE_DRIVER_HYPERSTOR` with
  `hyperstor_api`, `hyperstor_pool`, `hyperstor_token`; `gxctl
  datastore create NAME --hyperstor API,… --pool N --token-file F`; the
  Datastores page's *hyperstor cluster*. The answer shows the identity
  and its expiry, never the key or the token.
- **Tests:** unit tests for the volume names, the control-socket client
  (attach naming the node, preempt when asked or for another node's
  stale hold, a tool's hold is busy, detach, list) and the stale-hold
  rules (S26), and region
  I/O through a stand-in `hyperstorctl`; hypervisor argument tests for
  vhost-user disks; ignored `hyperstor_root_disk_boots` and
  `qemu_hyperstor_root_disk_boots`, which enroll with a real cluster,
  boot a cloud image from a linked 6 GiB disk (root grown, serial
  checked), delete it all and, given an admin certificate, purge the
  disk's clone and see the leader remove the image volume. Both passed
  on a scratch all-in-one cluster (3 nodes, 8 GiB devices) with
  Ubuntu 24.04's cloud image, Cloud Hypervisor's `CLOUDHV.fd` and QEMU
  8.2.2's OVMF.
- **Not checked:** a multi-node glidex cluster (agents staging, a
  server as worker); `hyperstor-vhost` as a service with `--allow-uid`
  and `--group` for glidex under the systemd runner; renewal (the
  certificates last longer than the tests); a stale hold preempted
  between two real nodes, and a node's probe letting go of a hold
  (S26: unit tests only).
- **Failed creations (S25), found installing on a pilot host** against a
  three-member map service: `hyperstorctl --map` took one address
  (hyperstor I176), so the fill failed; the deleted image volume then
  sat in hyperstor's trash, and the image loop retried "exists" without
  pause. Now leftovers are discarded for good and ids freed first; the
  boot tests plant a trashed volume under the image's volume id (with an
  admin certificate) and the disk is still made. Without freeing it, the
  disk never gets ready (checked).

## 12. CSI

### 12.1 Why not first (S1)

| Concern | Detail |
|---|---|
| Kubernetes assumptions | The CSI spec doesn't depend on any container orchestrator, but plugins often do. Provisioning goes through Kubernetes sidecars (`external-provisioner`, `-attacher`, `-resizer`, `-snapshotter`) that watch Kubernetes objects. Credentials come as Kubernetes Secrets. Some plugins read their own CRDs or node labels. glidex would have to play the sidecars' role, plugin by plugin. |
| Root and containers | Node plugins mount, run `iscsiadm` and `rbd map` as root, usually from a privileged container with host mount propagation. glidex has neither containers nor a root storage daemon (S12). |
| Volume model | VMs need `Block` access mode (as KubeVirt uses it). `Mount` mode gives a filesystem, so glidex would write a raw image file inside it, which wastes the volume. Block mode means raw only: no qcow2 overlays (linked clones become `VolumeContentSource` clones, where the plugin supports them) and no shrink. |
| Operations | Each plugin is a gRPC service per node with its own release cycle, upgrade story and failure modes. Today glidex runs one binary per host (14-clustering.md D4). |
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

**REST** (and `glidex.storage.v1.DatastoreService`, 18-grpc.md):

| Route | Action |
|---|---|
| `GET /datastores` | `readDatastore` |
| `POST /datastores` | `manageDatastore` |
| `GET /datastores/{id}` (incl. `nodes`) | `readDatastore` |
| `PATCH /datastores/{id}` (`nodes`, `node_paths`, `default_for`, `linked_clones`, `overcommit`) | `manageDatastore` |
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
    keep a fast datastore for certain tenants (19-tenancy.md);
  - `placeVm` is in the operator role.
- On a shared datastore, host-path rules (11-security.md §7.4) treat its
  root like the managed directories: a raw path into it needs
  `useHostPath`.

**As built (S2a):**
- **REST** (`api/storage.rs`): `GET /datastores`, `GET /datastores/{id}`
  (id or name), `POST /datastores` (`201`), `PATCH /datastores/{id}`
  (the fields present: `node_paths`, `nodes`, `default_for_disks`,
  `overcommit`) and `DELETE /datastores/{id}` (`204`).
- **RPC:** `glidex.storage.v1.DatastoreService`, with the proto messages
  `Datastore` and `DatastoreNode`.
- `Disk` and `CreateDiskRequest` gain `datastore`.
- **Cedar** (`policies/glidex.cedarschema`, `base.cedar`):
  - `entity Datastore in [Cluster]`;
  - `readDatastore` is `shared`, and every user may read datastores
    (`base.read-shared`, like images), so a user can choose one;
  - `manageDatastore` is in a new `datastore.manage` permission held by
    `role.system-admin` only, and is `host-scoped`. "Operator" above
    meant the operator tenant, and the marker already limits it to that
    tenant; `role.operator` is a project role and doesn't apply to a
    cluster-scoped record;
  - `useDatastore` is `shared` and permitted to all by
    `base.use-datastore`; a site policy `forbid` narrows it. It is
    checked on disk create and on a VM create that makes a root disk.
- **Units:** both units have `ReadWritePaths=… -/srv/glidex`. The
  installer creates `/srv/glidex` (`root:root 0755`), and uninstall
  leaves it alone: operators' mounts are under it.

**As built (S2b):**
- **Watch:** kind `datastores` (`WATCH_KIND_DATASTORE`), with `datastore`
  in the `WatchResponse` oneof. It lists what `readDatastore` on the
  cluster may see. A free-space change is news only past the 1 % row
  threshold (§4.3).
- **gxctl:** `datastore list | show | create | set | rm`, and
  `disk create --datastore`. `disk show` names the disk's datastore.
  See 01-cli.md.
- **UI:** `pages/Datastores.tsx`, in the nav with `readDatastore`; its
  actions need `manageDatastore`. The Disks page gets a Datastore column
  and a create-form select once there is more than `local`. See
  07-web-ui.md.
- **Tests:**
  - UI e2e: `datastores.spec.ts` (the `ui` project). The scratch
    control plane sets `GLIDEX_DATASTORE_ROOT`;
  - gxctl: `node_paths_are_node_equals_path`, and the in-process RPC
    test lists `local`.

**Quota:** project `disk_gib` stays cluster-wide (14-clustering.md §9.2),
counting disks on every datastore. Per-datastore quotas are queued
(S15).

**Metering** (13-metering.md):
- `disk.alloc` doesn't change.
- `disk.stored` and `image.stored` are sampled by the volume's worker,
  or for a shared copy by its fetching node. A shared datastore is
  counted once, never once per node.
- `group_by=datastore` is added.

## 14. Migration from today

As built (S1, S2a), nothing migrates: `local` isn't stored, and a disk
without a datastore is on it (S3). Point 3 below is built for every
registered datastore, not only shared ones (§4.3). The plan was:

1. A schema migration creates `local` and sets `datastore: "local"` on
   disks. It rewrites `image_caches/<image>/<node>` keys unchanged,
   because a bare node id is a node location. Standalone hosts get the
   same migration.
2. Nothing on disk moves. `GLIDEX_IMAGE_DIR` and `GLIDEX_DISK_DIR` keep
   working as the paths of `local`.
3. Detach and import (14-clustering.md §5.8–5.9) carry `local` disks as
   today. A disk on a shared datastore can't be detached with its node:
   the receiving cluster has no such datastore. That is refused with
   `409`, naming the disks, and the operator moves them to `local`
   first (§8.3).

## 15. Bugs to fix first (S0)

These were found while writing this document. Each one assumes every
node looks like the server that answered the request, and datastores
would make each of them worse. Fixes 1–4 are **implemented**, as
described below; fix 5 is part of S2.

1. **The firmware path is the server's.**
   - `state.rs:1565` stores `firmware_path` as an absolute path under the
     creating server's `image_dir`.
   - `controller/vm.rs:910-923` uses it as is on the placed node.
   - Launch doesn't wait for that node's firmware copy.
   - Fix, as built: the stored `firmware_path` is kept (the API shows
     it), but launch replaces it with the placed node's copy
     (`firmware_binding`, `state.rs`). Until that copy exists, the VM
     waits with `DisksReady=False`, like a disk that isn't ready.
   - Test: `cluster_tests.rs::a_vm_boots_through_its_nodes_copy_of_the_firmware`.
2. **Shrink and extend-root read a remote file.**
   - `state.rs:2998` (`shrink_minimum`) and `state.rs:3052`
     (`partition::read_table`) read the disk file on the answering
     server.
   - Disk APIs are not relayed to the disk's node, so they fail for a
     disk bound elsewhere.
   - Fix, as built: the early check runs only where the file is
     (`is_local_disk`). Elsewhere the request is recorded, and the disk's
     node reports `ResizeInvalid` or the `ExtendRootFailed` event, which
     it already did. The worker rule of §8.1 comes with S3.
   - Test: `cluster_tests.rs::disk_edits_are_checked_on_the_disks_node`.
3. **`CreateDiskRequest.node` isn't resolved.**
   - It is documented as id or name, but stored raw
     (`images/disk.rs:190`).
   - A name never matches `local_node_id()`, so the disk is never made.
   - Fix, as built: resolved like `VmSpec.node` (`resolve_node`), and an
     unknown node is `400`.
   - Test: in `cluster_tests.rs::the_scheduler_spreads_vms_honours_pins_and_disks_and_explains_itself`.
4. **Linked overlays use absolute backing paths.**
   - `qemu_img::create` is passed `image_path` (`images/disk.rs:265`).
   - Correct only while `GLIDEX_IMAGE_DIR` is the same on every node.
   - Fix, as built: new overlays name the image relative to the disk
     directory (`paths::relative_to`), and existing overlays are left as
     they are.
   - Checked on a host: Cloud Hypervisor v53 and QEMU resolve the path
     from the overlay's directory, not their working directory. The
     ignored boot tests `catalog_image_boots_and_root_grows` (both
     hypervisors) pass through it.
   - Test: `image_tests.rs::download_verify_then_linked_and_full_disks`.
5. **No storage capacity anywhere.** Nodes report no free space and the
   scheduler doesn't look at it. §5.2 adds it.

## 16. Milestones

| # | Content | Depends on |
|---|---|---|
| S0 | §15 fixes 1–4, with tests for each (two nodes with different `GLIDEX_IMAGE_DIR`, shrink through a server that isn't the disk's node, `node` by name). **Implemented.** | — |
| S1 | `StorageDriver` trait. Today's code moved into `FileDriver`, with `local` built in (serde default, no migration: S3). `Disk.datastore`, `VolumeBinding`. **No behaviour change:** the command-line pin tests and every current storage test pass unchanged. **Implemented** (as built: §7). | S0 |
| S2 | The `Datastore` resource with `default_for` (S13) and `node_paths` (S14), `datastore_nodes`, capacity, the API/gxctl/UI surfaces, Cedar, registered `dir` datastores under `/srv/glidex`, and the unit changes (S11). **Implemented** (§4.3, §13 as built), as S2a (resource, API) and S2b (gxctl, UI, watch). | S1 |
| S3 | `shared-dir`: marker and claim, reachability, `worker` (§8.1), the scheduler topology filter, image copies per location, linked clones with relative paths, QEMU `locking=on`, glidex's volume lock (S16), and the installer's register-and-ask step (S13). **Implemented** (§4.4, §4.5), as S3a (driver, workers, locks) and S3b (images in shared datastores, installer). | S2 |
| S4 | `POST /vms/{id}/place` for stopped VMs, and for `Lost` VMs after `forget --fenced`. **Implemented** (§9 as built). | S3 |
| S5 | `POST /disks/{id}/move` (within a node, between nodes, onto or off shared datastores). **Implemented** (§8.3 as built); between nodes goes through a shared datastore. | S3 |
| S6 | `glidex-stord`, `VolumeBinding::Block`, and the `lvm` driver (§11.1): S6a stord (crate, unit, installer), S6b the `lvm` datastore, block bindings and device access, S6c moves, metering and VM boots from volumes. **Implemented** (§11.1 as built). Then `shared-lvm` and `rbd` | S2 |
| S7 | The CSI adapter, **only if the §12.3 gate is met** | S6 |
| S8 | The `hyperstor` driver (§11.2, S20–S24): enrollment, volumes, images as cloned volumes, vhost-user disks for both hypervisors. **Implemented** (§11.2 as built). | S3, hyperstor HS3 |
| — | Per-datastore quotas (S15): queued, no milestone yet | S2 |

## 17. Questions

Settled:
- **Default for new disks** once a shared datastore exists: `local`,
  and the operator changes it; the installer asks (S13).
- **Per-node mount paths:** one path with per-node overrides (S14).
- **Per-datastore quotas:** queued for later (S15).
- **Mounts made after the units start** work (§19, H1).

- **Worker hand-over window** (§8.1): closed by the volume lock (S16).

- **NFS client options:** the probe warns on `soft` mounts and on
  node-local locks (§4.4, mount warnings). It doesn't warn on `actimeo`:
  the marker is written once, before other nodes look for it; image
  copies are checked against their digest; disk records come from the
  cluster store, not the filesystem; and a disk's data is opened by one
  hypervisor at a time under the volume lock, which NFS's close-to-open
  consistency covers. glidex doesn't mount, so `cache.direct` (QEMU,
  `O_DIRECT`) stays the operator's mount choice.

Open: none.

## 18. Edits to other documents (applied)

Applied once S5 was in, as the list below says, except:
- 12-reconciliation.md got no `Moving` phase: a move is the disk's
  `moving` field, and the phase stays `Ready` (§8.3);
- the hardening note went to 11-security.md §9 (process and host
  hardening), not §10;
- 13-metering.md records that disks are sampled by their node or worker,
  and that `group_by=datastore` and metering datastore image copies
  are later work.

- **README.md:** add `24-storage.md` to the contents table (done with this
  draft) and a "shared storage" line under non-goals until S3 lands.
- **10-images.md:**
  - §1: lift "Remote or shared storage (NFS, Ceph, iSCSI)" once S3 lands;
  - §2: describe `local` as a datastore;
  - §3: add `Disk.datastore`, `worker` and `volume`;
  - §6: operations run on the worker;
  - §11: point to here.
- **14-clustering.md:**
  - D6: "bound to a node" becomes "bound to a node on a node-local
    datastore; placement follows datastore topology";
  - §9.1: the filter in §5.2;
  - §9.2: copies per location;
  - §18: move "shared storage classes", "shared image store" and "moving
    resources between hosts" here.
- **12-reconciliation.md §10.1:** worker rule; `Moving` phase.
- **11-security.md:**
  - §7: `Datastore` entity and actions;
  - §10: `ReadWritePaths=-/srv/glidex` in both units, and the H1
    hardening note: a mount made after a unit starts is writable inside
    it, whatever `ReadWritePaths` says.
- **06-rest-api.md / 18-grpc.md / 01-cli.md / 07-web-ui.md:** §13 surfaces.
- **13-metering.md:** per-worker sampling and `group_by=datastore`.
- **05-installer.md:** create `/srv/glidex` (`0755 root`). Explain that
  operators mount datastores under it, owned by `glidex` (NFS: export with
  that uid, root squash is fine). Add the optional step that registers a
  mounted shared datastore and asks whether it becomes the default for new
  disks (S13; suggested answer no; `--datastore-default` when
  non-interactive).

## 19. Host findings

Checked on the dev host on 2026-10-19:
- Ubuntu, kernel 7.0.0, systemd 259;
- Cloud Hypervisor v53, QEMU 10.2.1;
- nfs-kernel-server 2.8.5, NFSv4.2.

The "two clients" were the host and a network namespace with its own NFS
client identifier, connected by a veth pair. The NFS server saw them as
two separate clients. Everything was removed afterwards.

**H1. Mounts made after a unit starts (S11, §17).**
- Setup: a unit with the control plane's sandbox (`ProtectSystem=strict`,
  `ProtectHome`, `PrivateTmp`, `ReadWritePaths=-/srv/glidex-probe`), plus
  tmpfs mounts made after it started.
- A mount made later under the listed root is visible inside the unit
  and writable. It still works after an unmount and remount.
- A unit started after the mount sees it writable too.
- So S11 works, and nothing needs a restart.
- **Hardening note:**
  - a mount made after start is writable inside the unit **even when it
    is not under any `ReadWritePaths` entry**. `ProtectSystem=strict`
    only makes read-only what was mounted when the unit started;
  - checked with an unlisted `/srv/gx-unlisted`: writable when mounted
    after start, read-only for a unit started after it;
  - normal file permissions still apply. The sandbox just isn't the
    boundary for late mounts, which 11-security.md §10 should say.

**H2. Disk locks.**
- **Cloud Hypervisor, same host:**
  - it holds `F_OFD_SETLK` `F_WRLCK` over the whole disk file
    (`/proc/locks`: `OFDLCK ADVISORY WRITE`);
  - a second instance on the same qcow2 or raw file fails:
    `LockDiskImage { error: AlreadyLocked }`.
- **Across two NFS clients:** the same result for Cloud Hypervisor
  (qcow2 and raw) and for QEMU (`Failed to get "write" lock`).
- **The Cloud Hypervisor problem, from `strace` of the refused second
  instance on a qcow2 the first had open:**
  - it logged `QCOW2 image not cleanly closed, rebuilding refcounts`;
  - it then wrote the header (offset 0), the refcount table and blocks
    (65536, 131072…) and the header again, **before** its
    `F_OFD_SETLK`, which returned `EAGAIN`;
  - it wrote the header's feature bits (offset 72) once more while
    exiting;
  - on a local file the image's contents changed;
  - across NFS clients it made 7 such writes before trying the lock.
    The hash matched afterwards, but the writes still happened.
- **QEMU's refused instance** wrote nothing (`pwrite64`: 0), and the
  image was unchanged.
- **Raw images:** no metadata, so nothing like this happens.
- **Conclusion:** S16.

**H3. `O_EXCL` across NFS clients (§5.1).**
- Two processes, one per client, released at the same instant, each
  tried to create the same 2000 names with
  `O_CREAT | O_EXCL`.
- Every name had exactly one winner (1000 each, none created twice and
  none lost), and every file held its winner's mark.

**H4. LVM-thin (S6, on a 4 GiB loop VG).**
- A thin snapshot of a thin LV is created with the activation-skip flag
  (`lvs` attr `…k`). It needs `lvchange -ay -K`, or
  `--setactivationskip n` when it is made.
- A snapshot of a read-only (`lvchange -p r`) image LV is writable, and
  can be grown with `lvextend` past its origin's size.
- `qemu-img convert -n` into a fresh thin LV writes the zeros too (the
  pool's use grew by the image's whole size); `--target-is-zero` skips
  them.

**H5. Device ownership and `DeviceAllow`.**
- A `chown`ed LV device node keeps its owner across `lvextend` and a
  udev `change` event, but a new activation makes a fresh `root:disk`
  node, possibly with another minor. So stord sets the owner at every
  activation.
- With `DevicePolicy=closed`, `DeviceAllow=/dev/<vg>/<lv> rw` (the
  symlink), `/dev/dm-N` and `block-device-mapper` all let a `glidex`
  process open the LV; without one, `Permission denied`.
- A drop-in in `/run/systemd/system/<unit>.service.d/` adds to the unit's
  own `DeviceAllow=` list, after `daemon-reload`.
- `systemctl set-property --runtime` writes `DeviceAllow=` (reset)
  before its entries: clearing it removed the unit's own devices too.
  Not used (S18).

**H6. Hypervisors on an LV.**
- Cloud Hypervisor (`path=/dev/<vg>/<lv>,image_type=raw`) and QEMU
  (`host_device`, `locking=on`), as `glidex` in a unit allowed only
  `/dev/kvm` and the LV, booted a kernel that saw it as `vda`.
- A second instance on the same LV is refused by both: Cloud Hypervisor
  `LockDiskImage { error: AlreadyLocked }`, QEMU `Failed to get "write"
  lock`.

**H7. Partition tools on an LV.**
- `sgdisk` and `qemu-img`/`qemu-io` work on the device as `glidex`.
- `growpart` on the device fails ("failed to get start and end for
  /dev/dm-61"): it looks for the partition's own device, which an
  unmapped LV doesn't have. On a regular file it works, so block
  volumes take `partition::edit`'s shadow-file path.


**H8. QEMU's reconnecting socket chardev (open).**
- QEMU 8.2.2 (Ubuntu 24.04) accepts `reconnect=1` on a `socket` chardev
  and refuses `reconnect-ms` ("Invalid parameter"). Checked on the dev
  host on 2026-10-23.
- **To check:** that QEMU 9.2 and later take `reconnect-ms` (which glidex
  passes from 9.2, by `qemu --version`) and that `reconnect` is
  deprecated there; and that a VM's disk survives `hyperstor-vhost`
  restarting under both hypervisors.

**H9. Firmware and 512-byte sectors on vhost-user-blk.**
- With 4 KiB logical blocks (hyperstor-vhost before D126), Cloud
  Hypervisor's `CLOUDHV.fd` (edk2 `ch-811ce5ea35`) found no bootable
  device on a volume holding Ubuntu 24.04's cloud image, whose GPT is
  in 512-byte sectors; the same image booted from a file.
- With 512e (512-byte logical, 4 KiB physical blocks) it boots under
  Cloud Hypervisor and under QEMU 8.2.2 with OVMF, and the guest sees
  `logical_block_size` 512 and `physical_block_size` 4096. Checked on
  the dev host on 2026-10-23, with `hyperstor-vhost` from hyperstor's
  512e change.
