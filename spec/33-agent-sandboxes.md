# Agent sandboxes: forking a running VM to try several paths

**Status: draft. Nothing is built.** Tracked in #694. The decisions in §3
are proposals until the owner settles them; §12 lists the open
questions. Items marked **(verify)** are measured or checked on hosts in
SB0 (§11).

An AI agent working in a VM often reaches a point where it could go
several ways: try two fixes, run three experiments, compare two
installers. A **fork** lets it take the VM as it is now, memory and
disks, and continue in several copies at once, keeping the one that
worked. This spec describes forks in glidex: what a fork is, how it is
made on one host and across hosts, how the copies get identities of
their own, and how the control plane keeps up with agents that make and
drop sandboxes all the time.

Builds on:
- [32-live-migration.md](32-live-migration.md): the shim's receive
  mode, the relay between nodes, and the CPUID fit check (LM2);
- [24-storage.md](24-storage.md): the `hyperstor` driver, and linked
  disks as clones of an image's golden snapshot (S28, S33);
- [30-pylon.md](30-pylon.md): parking, scale-to-zero and the guest agent
  (P19);
- [29-workload-identity.md](29-workload-identity.md): tokens for a VM,
  served by `glidex-metad` (W6, W8);
- [13-metering.md](13-metering.md): the node-local ledger;
- [14-clustering.md](14-clustering.md) §4.1: the scale envelope.

Read those first. This document only describes what changes.

**Positioning.** Hosted sandbox services built on Firecracker already
fork in well under a second. Glidex doesn't try to beat them on latency.
What it can offer that they can't:
- sandboxes on the operator's own hosts, where the agent's code and data
  stay;
- cost per sandbox, attributed to the agent's identity, so it can be
  joined with what the agent spent on inference;
- each sandbox under a tenant, project and quota, with Cedar policy and
  audit like any other glidex resource.

## 0. Checked while writing

| # | Finding | Consequence |
|---|---|---|
| 1 | Cloud Hypervisor v55.0, glidex's pin and Pylon's base, restores a snapshot (`vm.restore`, `memory_restore_mode=`) in one of four ways: `copy` (the default: guest RAM filled from the snapshot before resuming), `ondemand` (pages faulted in through userfaultfd; upstream since v52), `ondemand_no_background_fetch_pages`, and `copyonwrite` (upstream since v54). `copyonwrite` maps the snapshot's memory file copy-on-write over guest RAM, so VMs restored from the same snapshot share its pages in the page cache. It falls back to `copy` when guest RAM isn't suitable, and the memory file must stay on disk for the VM's lifetime. | Forking memory on one host needs no VMM change: snapshot once, restore N times with `copyonwrite` (§5.2). Lazy restore on another host exists too (`ondemand`, §5.3). |
| 2 | Cloud Hypervisor has a VM Generation ID device (upstream since v54). A restore changes the ID and notifies the guest. | A Linux guest with the vmgenid driver reseeds its RNG when the ID changes (verify for the guest images glidex offers). |
| 3 | hyperstor has snapshots of a volume, snapshot groups (every volume in a list at one point), copy-on-write clones from a snapshot's epoch, and a trash. glidex already makes every linked disk on a hyperstor datastore as a clone of its image's golden snapshot (24-storage.md S28). | Forking disks across hosts works today: a snapshot group at the pause, then one clone per volume per fork (§6). How long a snapshot and N clones take is not measured (verify). |
| 4 | Live migration (32-live-migration.md, LM0–LM5) built a shim that receives a VM, a relay over the nodes' mTLS, and a one-way CPUID fit. On v55, the two test nodes fit neither way. | A fork restored on another node reuses the relay and the fit check (§5.3). On unlike hosts, a remote fork often won't fit: same-host forks come first. |
| 5 | The cluster's single write lane sustains about 150–300 writes a second; normal load is 5–20 a second at 2,000 VMs (14-clustering.md §4.1). A VM's create and start take several writes today (the create commit with its disks, then disk and VM phases and conditions); the count isn't measured (verify). In the week of 2026-11-23, one disk retrying in a loop wrote about 25 entries a second, and it was noticed cluster-wide (#643). | Recording every fork in the cluster can't keep up with agents. Forks need a host-local lifecycle, with the cluster holding summaries (D5, §7). |
| 6 | `glidex-metad` tells VMs apart by their port, and serves tokens of 15 minutes (at most 1 h). Issued tokens live until they expire (29-workload-identity.md W8). | A fork has its parent's tokens in memory, valid until they expire. Its own requests get its own tokens, as a new VM id with its own port. The leftover window is the TTL (D4, §8). |
| 7 | A VM with VFIO devices can't be snapshotted or restored as a fork: its device state lives in the hardware (verify how Cloud Hypervisor refuses it). | Sandboxes are CPU-only. An agent that needs a GPU calls an inference service over the network (D10). |
| 8 | Pylon parks idle vCPUs and scales a VM down to zero; its guest agent is in every Pylon VM's seed (30-pylon.md P19). `scx_pylon` doesn't attach on kernels before 6.19 without the BPF LSM (pylon#133). | Parked forks cost little while an agent compares them. The guest agent is where the post-fork hook runs (D3). Core management stays limited until hosts run 6.19 or later. |

## 1. Goals and non-goals

Goals:
- An agent, or a person, makes N forks of a running VM. Each continues
  from the same memory and disk state, independently.
- A fork can be kept (it becomes an ordinary VM) or dropped (everything
  it made goes).
- Each fork has its own identity: network, hostname, RNG, keys and
  tokens.
- Each fork is metered and audited, counted against its project's
  quota, and attributed to the identity that made it.
- Agents can make and drop forks often without loading the cluster's
  write lane.

Non-goals (for now):
- latency on par with hosted Firecracker services;
- forks of VMs with VFIO devices (GPUs, VFs);
- QEMU VMs (Cloud Hypervisor and Pylon only);
- forks into another cluster;
- application-consistent checkpoints of every workload (the guest agent
  may quiesce what it knows, D2).

## 2. Words

- **Checkpoint:** a frozen point of a running VM. It holds the VM's
  memory file and device state (a Cloud Hypervisor snapshot), and a
  hyperstor snapshot group of its volumes, both taken during one pause.
  The VM resumes after it.
- **Fork:** a VM restored from a checkpoint, with a new identity. The
  VM the checkpoint was taken from (the **parent**) keeps running.
- **Family:** the forks of one checkpoint. Quota, metering and lifetime
  are reserved and reported for the family as a whole.
- **Cold fork:** restored with `memory_restore_mode=copy`, on any node
  that fits.
- **Warm fork:** restored on the checkpoint's node with `copyonwrite`,
  sharing memory pages with its siblings.
- **Remote fork:** restored on another node with `ondemand`, its memory
  pulled through the relay as it faults.

## 3. Decisions (proposed)

| # | Decision | Why |
|---|---|---|
| D1 | **Forks are Cloud Hypervisor (and Pylon) VMs whose disks are all on hyperstor.** A VM with a disk elsewhere can't be checkpointed. A single-host option on `lvm` thin snapshots is an open question (O2). | hyperstor gives copy-on-write clones that every node sees (§0.3), so a fork may run on any node that fits. |
| D2 | **A checkpoint is taken during one pause:** pause the VM, take the Cloud Hypervisor snapshot and the hyperstor snapshot group, resume. It is crash-consistent. If the guest agent is present, it is asked first to flush and freeze filesystems (`fsfreeze`) and thaws them after the resume, which makes the checkpoint consistent for the filesystems. | Memory and disks must describe the same instant, or the fork's page cache disagrees with its disks. One pause keeps the parent's interruption to the time of the two snapshots (verify). |
| D3 | **A fork starts with its network down, and comes up as itself.** It gets a new VM id, so new MACs (glidex derives them from the VM id), new OVN ports and addresses, and a new hostname and cloud-init instance id. The VM Generation ID changes at restore (§0.2). The guest agent runs a **post-fork hook**: reseed the RNG if the kernel didn't, new SSH host keys and `machine-id`, drop cached tokens, step the clock, then bring the network up. Until the hook reports done, the fork's NICs stay down. | Two VMs with the same RNG state, keys or addresses are a security bug, not a cosmetic one. Keeping the network down until the hook finishes means the fork never talks as its parent. |
| D4 | **Tokens:** the fork gets its own tokens from `glidex-metad`, as any VM does. The tokens copied from the parent's memory stay valid until they expire. The post-fork hook drops them, and a family may ask for a shorter TTL (5 min) for its VMs' tokens. Binding tokens to the VM's generation is an open question (O5). | Short lifetimes are glidex's revocation (29-workload-identity.md W8). The window is bounded, and named here rather than hidden. |
| D5 | **The cluster records the family, not each fork.** Making a family is one write: the checkpoint's record, the family with its count, quota reservation, owner and lifetime. Each fork is a **node-local record** on the node that runs it, with its events and audit, reported to the cluster in batches like the metering ledger. Keeping a fork turns it into an ordinary VM, with one write. | A fork costs several writes as a full VM (§0.5); agents may make tens of forks a second. One write per family, plus one per kept fork, keeps sandboxes inside the write budget. |
| D6 | **A fork runs where its checkpoint is unless asked otherwise.** A remote fork may go only to a node that passes the CPUID fit for the parent's boot CPUID (32-live-migration.md LM2). | A warm fork is the fastest and cheapest (§5.2). The fit check is what keeps a restored guest from losing CPU features. |
| D7 | **Forks are metered like VMs**, into the node-local ledger, and attributed to the family's owner: a person or a service account. Usage is reported per family, with each fork's share. | The owner's identity is what joins a sandbox's cost to its inference spend (§9). |
| D8 | **Quotas:** a family reserves its forks' vCPUs, memory and disk from the project's quota when it is made. A project also has a limit on forks per family and per node. | A family must not be able to fill a node or a project before anyone sees it. |
| D9 | **Lifetimes:** a family has a lifetime (default 1 h). When it ends, or a fork is dropped, the fork's clone volumes and node-local record are deleted. A checkpoint's memory file stays while any fork restored from it with `copyonwrite` runs (§0.1); then it goes. | Agents forget to clean up. Dropping must be cheap and certain. |
| D10 | **CPU-only.** A VM with VFIO devices can't be checkpointed. An agent that needs a GPU calls an inference service. | VFIO device state can't be copied (§0.7). It is also the better design: the GPU serves many sandboxes rather than one. |

## 4. API, clients and permissions (sketch)

- `POST /vms/{id}/checkpoints {quiesce?}` → a checkpoint (Cedar
  `checkpointVm`). `GET`, `DELETE /checkpoints/{id}`.
- `POST /checkpoints/{id}/forks {count, kind: warm|remote|cold, node?, ttl?, token_ttl?}`
  → a family with its forks (Cedar `forkCheckpoint`, on the parent's
  project). `GET /families/{id}` lists the forks, read from the nodes
  that run them.
- `POST /forks/{id}/keep` → the fork becomes a VM in the project (Cedar
  `keepFork`). `DELETE /forks/{id}`, `DELETE /families/{id}`.
- RPC twins and `gxctl checkpoint|fork|family`, as for every other
  resource (18-grpc.md).

An agent inside a VM calls these with its service account's glidex token
(29-workload-identity.md W6). That is how an agent forks its own
sandbox.

## 5. How a fork runs

### 5.1 The checkpoint

1. The parent's node asks the guest agent to freeze filesystems, if
   asked to and if the agent is present (D2).
2. It pauses the VM, takes the Cloud Hypervisor snapshot to a node-local
   directory (memory file and device state), and takes the hyperstor
   snapshot group of the VM's volumes.
3. It resumes the VM and thaws the filesystems.
4. It writes the checkpoint's record (one write; with D5 this is the same
   write as the family's when a fork request names a running VM).

### 5.2 Warm forks (same node)

For each fork: clone each volume from the snapshot group (hyperstor), make
the fork's node-local record, and start a shim that restores the
checkpoint with `memory_restore_mode=copyonwrite`, the fork's new NICs
attached but down. On resume the guest sees a new VM Generation ID. The
guest agent runs the post-fork hook (D3) and reports done. The node then
brings the NICs up and the fork is `running`.

The forks share the checkpoint's memory pages until they write to them.
Memory used is the pages each fork has changed, plus the shared file in
the page cache.

### 5.3 Remote forks (another node)

The target node must pass the CPUID fit (D6). It starts a shim restoring
with `memory_restore_mode=ondemand`, with the checkpoint's memory served
from the checkpoint's node through the relay (32-live-migration.md §6),
page by page as the fork faults. How the relay serves a memory file to
userfaultfd is to be designed in SB4 (O3). Disks are clones as in §5.2;
hyperstor serves them on any node.

### 5.4 Cold forks

As a remote fork, with `memory_restore_mode=copy`: the whole memory file
is copied to the target before the fork resumes. Slower, but it needs
nothing beyond what exists, which is why SB1 builds it first.

### 5.5 Keep and drop

Keeping a fork writes it to the cluster as an ordinary VM (one write) and
takes it out of its family's lifetime. Its memory still maps the
checkpoint's file if it was a warm fork. It stays so until its next
restart, which boots it from its disks (O4).

Dropping a fork stops its shim, deletes its clone volumes (they go to
hyperstor's trash) and its node-local record, and returns its share of
the family's reservation.

## 6. Storage

- Volumes: a hyperstor snapshot group at the checkpoint, then one clone
  per volume per fork, in the parent's project (24-storage.md S33). A
  dropped fork's clones go to the trash; the snapshot group goes when its
  family does and no kept fork's clone still depends on it.
- Memory files: on the checkpoint's node, in a directory beside the
  VMs' runtime state. They are as large as the VM's memory, so a node
  limits the space for them, and a checkpoint is refused when it would
  go over. Where memory files live for remote forks, on the node or as
  hyperstor volumes, is open (O3).

## 7. The control plane

- The cluster holds checkpoints, families and kept forks (D5). A family's
  record says its count, owner, reservation, lifetime and the nodes its
  forks run on.
- Each node holds its forks' records, events and audit lines in its own
  store, beside its metering ledger. It reports them in batches: counts
  and states on each heartbeat; audit and metering with the ledger's
  shipping.
- Reading a family's forks asks each of its nodes (as host-local reads
  do today). A node that is gone takes its forks with it: the family
  shows them lost, and their reservation is returned when the node is
  forgotten.
- The scheduler counts a node's forks against its capacity from the
  heartbeat counts, not from per-fork records.

## 8. Identity and security

- New VM id, MACs, OVN ports, addresses, hostname and instance id for
  each fork (D3).
- RNG: the VM Generation ID change, then the post-fork hook as a second
  step if the guest's kernel didn't reseed.
- SSH host keys and `machine-id`: regenerated by the hook.
- Tokens: new ones from `glidex-metad`; the parent's copies dropped by
  the hook and bounded by their TTL (D4, O5).
- Other secrets in memory (keys the guest's programs hold) are the
  workload's to handle: the guest agent passes the generation change on
  to programs that listen for it (a socket or a file under `/run`), so an
  agent framework can rotate its own keys.
- Audit: making a checkpoint, a family, keeping and dropping forks, each
  with the identity that asked.

## 9. Metering and chargeback

Each fork's CPU, memory and disk go to the node-local ledger, under its
family. Usage is reported per family and per fork, attributed to the
family's owner (D7). When the owner is an agent's service account, the
same identity appears in the inference service's token billing, so the
two can be joined into the cost of one agent's run. Warm forks' shared
memory is counted once, to the family, not to each fork (verify how
the meters see shared pages).

## 10. Forking the model's context too (speculative)

If an agent's inference runs with a prefix (KV) cache that can be
pinned and shared, a family could carry the parent's cache handle with
it. Each fork would continue its conversation with the model from the
same cached prefix, not re-sending it. This needs the inference
service to expose cache handles bound to an identity; it is listed here
so the API leaves room for it (a family may carry opaque handles), not
as something to build now.

## 11. Milestones

| Milestone | What | Acceptance |
|---|---|---|
| **SB0: measurements** | On a test node: Cloud Hypervisor snapshot and restore of a 2, 4 and 8 GiB guest in each restore mode; a hyperstor snapshot group and N clones (N = 1, 10, 50); the parent's pause for a checkpoint; the cluster writes a VM's create, start and delete cost today. | Numbers recorded here (§0), with the build and date. They decide whether D5 is needed before SB3 or only after. |
| **SB1: cold fork** | Checkpoint a running VM (D2) and restore it as N ordinary VMs (`copy`), each recorded in the cluster as a VM. Identity as far as glidex controls it (new VM id, MACs, ports, hostname, instance id); network down until started by hand. | A fork resumes with its parent's processes and files, and runs next to its parent without clashing. |
| **SB2: re-identity** | The guest agent's post-fork hook (D3), the VM Generation ID check in the guest images, NICs up only after the hook, the family's token TTL (D4). | A fork's RNG output, host keys and `machine-id` differ from its parent's; it never sends a packet with the parent's addresses. |
| **SB3: warm forks** | `copyonwrite` restore on the checkpoint's node; families and node-local fork records (D5); quotas and lifetimes (D8, D9); `keep` and drop. | N warm forks start in about the time of one restore (verify against SB0). Making and dropping a family of N writes once or twice to the cluster, whatever N is. |
| **SB4: remote forks** | `ondemand` restore on another node through the relay; the CPUID fit (D6). | A remote fork runs on a node that fits, its memory paged in as it faults; one that doesn't fit is refused before anything starts. |
| **SB5: chargeback and UI** | Usage per family and fork (D7); the family view in the web UI; `gxctl` commands; docs for users and operators. | A family's cost appears under its owner, and matches its forks' sum. |
| **SB6 (speculative)** | Carrying a prefix-cache handle with a family (§10). | Decided after SB5. |

SB1 needs no new storage or VMM work. SB3 is where glidex's control plane
changes most.

## 12. Open questions

- **O1.** Who may fork whose VM: only the VM's own service account (an
  agent forking itself), anyone with `forkCheckpoint` on the project, or
  both?
- **O2.** Should a single-node option exist on `lvm` thin snapshots,
  for operators without hyperstor? It would allow warm forks only.
- **O3.** Where do memory files live for remote forks: served from the
  checkpoint's node through the relay, or written to a hyperstor volume
  that any node reads? The second survives the checkpoint's node going
  away, at the cost of writing the whole memory once.
- **O4.** A kept warm fork maps its checkpoint's memory file until it
  restarts. Should keeping force a memory copy instead, so the
  checkpoint can go at once?
- **O5.** Can tokens be bound to a VM's generation, so a fork's copies
  of its parent's tokens stop working at once? `glidex-metad` would
  need to learn the generation from the shim, and relying parties
  would have to check it, which outside services won't.
- **O6.** Pylon VMs: does parking and scale-to-zero survive a
  `copyonwrite` restore unchanged (verify in Pylon)? Are parked forks
  cheap enough to keep many alive while an agent compares them?
- **O7.** How many forks per node, and per family, by default?

## 13. Status

| | |
|---|---|
| Spec | This draft (#694). Decisions D1–D10 are proposals; O1–O7 open. |
| Code | None. |
| Tests | None. |
| Not yet | Everything: SB0–SB6. |
