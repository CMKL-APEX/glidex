import type { ErrorJson } from "./gen/glidex/api/v1/errors_pb";

// ---- VMs (glidex.vm.v1, spec/18-grpc.md §9.1) ---------------------------------
//
// The VM routes answer in the proto3 JSON shape (see the storage section
// below for the rules): enums by their full names, 64-bit counters as
// strings, timestamps RFC 3339, defaults left out. The helpers here read
// them back in the short forms the pages show (`VM_STATE_RUNNING` → `running`).

/** What a VM is doing, as the controller last saw it (spec/12-reconciliation.md §7.4), for display. */
export type VmState =
  | "created"
  | "starting"
  | "running"
  | "paused"
  | "stopping"
  | "stopped"
  | "failed"
  | "unknown";

/** What a VM should be doing (`spec.power`), for display. */
export type PowerState = "running" | "paused" | "stopped";

/** `VmState` as the API sends it. */
export type ProtoVmState =
  | "VM_STATE_CREATED"
  | "VM_STATE_STARTING"
  | "VM_STATE_RUNNING"
  | "VM_STATE_PAUSED"
  | "VM_STATE_STOPPING"
  | "VM_STATE_STOPPED"
  | "VM_STATE_FAILED"
  | "VM_STATE_UNKNOWN";

export type ProtoPowerState = "POWER_STATE_RUNNING" | "POWER_STATE_PAUSED" | "POWER_STATE_STOPPED";
export type RestartPolicy = "RESTART_POLICY_ON_FAILURE" | "RESTART_POLICY_NEVER";
export type HostBootPolicy = "HOST_BOOT_POLICY_RESUME" | "HOST_BOOT_POLICY_STOP";

export type ExitCause =
  | "EXIT_CAUSE_REQUESTED"
  | "EXIT_CAUSE_TERMINATED"
  | "EXIT_CAUSE_CLEAN_EXIT"
  | "EXIT_CAUSE_CRASHED"
  | "EXIT_CAUSE_LAUNCH_FAILED"
  | "EXIT_CAUSE_HOST_REBOOT"
  | "EXIT_CAUSE_LOST";

export interface ExitRecord {
  /** RFC 3339. */
  at?: string;
  instance_id?: string;
  cause?: ExitCause;
  code?: number;
  signal?: number;
  message?: string;
  launched_at?: string;
}

export type HypervisorType = "cloudhypervisor" | "qemu" | "pylon";

export const HYPERVISOR_LABELS: Record<HypervisorType, string> = {
  cloudhypervisor: "Cloud Hypervisor",
  qemu: "QEMU",
  pylon: "Pylon",
};

/** A VM (glidex.vm.v1.Vm). */
export interface VmResponse {
  id: string;
  name: string;
  /** Owning project id. */
  project: string;
  /** Read with `vmState`. */
  state?: ProtoVmState;
  /** Read with `desiredState`. */
  desired_state?: ProtoPowerState;
  generation?: U64;
  observed_generation?: U64;
  resource_version?: U64;
  restart_required?: boolean;
  /** Deletion requested; the controller is tearing it down. */
  deleting?: boolean;
  restart_policy?: RestartPolicy;
  on_host_boot?: HostBootPolicy;
  stop_grace_secs?: number;
  conditions?: StorageCondition[];
  last_exit?: ExitRecord;
  /** The node the VM is placed on (spec/14-clustering.md §9.1). */
  node?: string;
  /** That node's name (spec/16-clustering-ui.md §9). */
  node_name?: string;
  vcpu_count: number;
  min_vcpus?: number;
  max_vcpus?: number;
  cpu_weight?: number;
  mem_size_mib: number;
  /** Read with `vmHypervisor`. */
  hypervisor?: ProtoHypervisor;
  vfio_devices?: string[];
  /** Devices asked for by pool, and those the scheduler picked (spec/22-host-topology.md §9). */
  devices?: { pool: string; count?: number; same_switch?: boolean }[];
  pool_devices?: { pool: string; address: string }[];
  /** `aligned` over several host NUMA nodes: guest NUMA node i is numa[i] (spec/27-gpu-vms.md §4). */
  numa?: { node?: number; vcpus?: number; memory_mib?: number; devices?: string[] }[];
  credential?: string;
  /** Its service account (a token id), for workload identity (spec/29-workload-identity.md W5). */
  service_account?: string;
  nics?: NicState[];
  /** Firmware image id the VM boots through. */
  firmware?: string;
  /** Kernel image id it boots directly (spec/23-kernel-boot.md §6). */
  kernel?: string;
  boot?: "BOOT_MODE_FIRMWARE" | "BOOT_MODE_KERNEL";
  /** Following the guest's kernel, and the kernel images extracted from its disk it boots, and booted before. */
  kernel_from_disk?: boolean;
  booted_kernel?: string;
  previous_kernel?: string;
  root_disk?: string;
  data_disks?: string[];
  warnings?: string[];
}

export type Vm = VmResponse;

/** A VM write's answer (`{vm, settled}`); `vm` is absent once the VM is gone. */
export interface VmWriteResponse {
  vm?: VmResponse;
  /** Set when the call waited: whether the VM got where it was asked to. */
  settled?: boolean;
}

/** The VM's observed state: `VM_STATE_RUNNING` → `running` (absent: `unknown`). */
export function vmState(vm: Pick<VmResponse, "state">): VmState {
  return (enumLabel(vm.state, "VM_STATE_") || "unknown") as VmState;
}

/** What the VM should be doing: `POWER_STATE_RUNNING` → `running`. */
export function desiredState(vm: Pick<VmResponse, "desired_state">): PowerState | undefined {
  return (enumLabel(vm.desired_state, "POWER_STATE_") || undefined) as PowerState | undefined;
}

/** The VM's hypervisor in the short form (`cloudhypervisor`, `qemu`). */
export function vmHypervisor(vm: Pick<VmResponse, "hypervisor">): HypervisorType {
  return hypervisorOf(vm.hypervisor) ?? "cloudhypervisor";
}

export interface NicState {
  /** The network's id. */
  network: string;
  /** The network's name (show this; fall back to `network`). */
  network_name?: string;
  mac: string;
  port?: string;
  ipv4?: string;
}

/** A NIC's network for display: its name, else its id. */
export function nicNetworkLabel(nic: Pick<NicState, "network" | "network_name">): string {
  return nic.network_name || nic.network;
}

// ---- networks (glidex.network.v1, proto JSON shape) -------------------------
// Networks are keyed by id (spec/19-tenancy.md D24); enums are their full proto
// names, timestamps RFC 3339, fields at their default left out.

export type NetworkMode = "NETWORK_MODE_NAT" | "NETWORK_MODE_BRIDGED" | "NETWORK_MODE_ISOLATED";
export type PortType = "PORT_TYPE_TAP" | "PORT_TYPE_VHOST_USER";
export type NetworkScope = "NETWORK_SCOPE_NODE" | "NETWORK_SCOPE_CLUSTER";
export type NetworkPhase =
  | "NETWORK_PHASE_PENDING"
  | "NETWORK_PHASE_READY"
  | "NETWORK_PHASE_DEGRADED"
  | "NETWORK_PHASE_NETD_UNAVAILABLE";

/** `NETWORK_MODE_NAT` → `nat` (the short text the UI shows). */
export const networkModeLabel = (m: string | undefined) => enumLabel(m, "NETWORK_MODE_");
/** `PORT_TYPE_VHOST_USER` → `vhost-user`, `PORT_TYPE_TAP` → `tap`. */
export const portTypeLabel = (t: string | undefined) => (t === "PORT_TYPE_VHOST_USER" ? "vhost-user" : "tap");
/** `NETWORK_PHASE_NETD_UNAVAILABLE` → `netd_unavailable` (absent: `ready`). */
export const networkPhaseLabel = (p: string | undefined) => enumLabel(p, "NETWORK_PHASE_") || "ready";
/** Whether a network spans the cluster (on OVN). */
export const isClusterNetwork = (n: Pick<Network, "scope">) => n.scope === "NETWORK_SCOPE_CLUSTER";

export interface ShareOffer {
  project: string;
  offered_by?: string;
  /** RFC 3339. */
  offered_at?: string;
  /** RFC 3339. */
  expires_at?: string;
}

export interface Network {
  /** UUID; paths, keys and VM NICs use it. */
  id: string;
  name: string;
  /** `cluster`: an OVN network every node can use; `node`: an OVS bridge on `node` (spec/14-clustering.md §11). */
  scope?: NetworkScope;
  node?: string;
  physnet?: string;
  /** The id of the VPC router it is on; absent: the shared edge. */
  router?: string;
  bridge?: string;
  mode?: NetworkMode;
  port_type?: PortType;
  vlan?: number;
  mtu?: number;
  dns?: boolean;
  owns_bridge?: boolean;
  /** RFC 3339. */
  created_at?: string;
  /** Project networks belong to a project (spec/11-security.md §6.2). */
  project?: string;
  all_projects?: boolean;
  grants?: string[];
  tenant_grants?: string[];
  /** Projects that accepted a share of this project network. */
  shares?: string[];
  share_offers?: ShareOffer[];
  /** What the network controller last saw in netd (spec/12-reconciliation.md §10.3). */
  phase?: NetworkPhase;
  conditions?: StorageCondition[];
  /** Deletion requested (RFC 3339); the network controller is removing it (from netd, then the record). */
  deletion_requested_at?: string;
}

/** Whether VMs of `project` may attach to `n` (mirrors base.network-grant). */
export function networkUsableBy(n: Network, project: string | null): boolean {
  if (!project) return true;
  if (n.project) return n.project === project || (n.shares ?? []).includes(project);
  return !!n.all_projects || (n.grants ?? []).includes(project);
}

export interface CreateNetworkRequest {
  name: string;
  mode: NetworkMode;
  port_type?: PortType;
  bridge?: string;
  subnet?: string;
  vlan?: number;
  mtu?: number;
  dns?: boolean;
  /** Cluster (OVN) or node; the server's default is the cluster's when OVN is on. */
  scope?: NetworkScope;
  /** A provider network (cluster scope, bridged): its physical network. */
  physnet?: string;
  /** A project NAT network on one of the project's VPC routers (id or name). */
  router?: string;
  /** Host networks: projects (ids or names) that may use it. */
  grants?: string[];
  /** Host networks: tenants (ids or slugs) whose every project may use it. */
  tenants?: string[];
  all_projects?: boolean;
}

/** A project's VPC router (spec/14-clustering.md §11.2a). */
export interface RouterSpec {
  name: string;
  /** Reaches outside: an address from the edge's pool, SNAT. */
  external?: boolean;
  external_ip?: string;
  gateway_nodes?: string[];
}

export interface Router {
  /** Short id; networks record it, OVN calls the router `gxr-<id>`. */
  id: string;
  project: string;
  spec: RouterSpec;
  status?: { external_ip?: string; snat_ct_zone?: number };
  /** RFC 3339. */
  created_at?: string;
}

export interface CreateVmRequest {
  name: string;
  /** Project id or name; default: the caller's default project. */
  project?: string;
  vcpu_count: number;
  min_vcpus?: number;
  max_vcpus?: number;
  cpu_weight?: number;
  mem_size_mib: number;
  kernel_image_path: string;
  /** Firmware image id/name (spec/10-images.md §7); default: the newest for the hypervisor. */
  firmware?: string;
  /** Host path of UEFI firmware (admins only); prefer `firmware`. */
  firmware_path?: string;
  credential?: string;
  /** A service account (token id) for its workload identity (spec/29-workload-identity.md W5). */
  service_account?: string;
  networks?: { network: string }[];
  rootfs_path?: string;
  /** Image id/name: glidex creates and owns a root disk from it. */
  image?: string;
  root_disk_size_gib?: number;
  /** Existing, unattached disk to boot from. */
  root_disk?: string;
  data_disks?: string[];
  kernel_args?: string;
  /** Kernel image id/name to boot directly; host files instead with `kernel_image_path` (+ `initrd_path`). */
  kernel?: string;
  initrd_path?: string;
  /** Boot the kernel the guest installs (spec/23-kernel-boot.md K11). */
  kernel_from_disk?: boolean;
  hypervisor?: ProtoHypervisor;
  vfio_devices?: string[];
  /** Devices asked for by pool; the scheduler picks them (spec/22-host-topology.md §9). */
  devices?: { pool: string; count?: number; same_switch?: boolean }[];
  /** N GPUs from a pool: aligned NUMA placement, sizing from the pool (spec/27-gpu-vms.md §3). */
  gpus?: { count: number; pool?: string; nvlink?: boolean };
  /** Desired power state; default `stopped` (spec/12-reconciliation.md §7.4). */
  power?: ProtoPowerState;
  /** After a crash: restart (default) or leave it stopped. */
  restart_policy?: RestartPolicy;
  /** After a host reboot, if it should be running: start it again (default) or not. */
  on_host_boot?: HostBootPolicy;
  /** Run on this node (id or name); default: the scheduler picks (spec/14-clustering.md §9.1). */
  node?: string;
}

/** One node's answer to `POST /scheduler/preview` (spec/16-clustering-ui.md §5.1). */
export interface NodeFit {
  id: string;
  name: string;
  /** Why the VM can't go there; absent: it fits. */
  refused?: string;
  /** 64-bit counts: read with `num`. */
  vcpus_taken?: U64;
  vcpus_capacity?: U64;
  mem_mib_taken?: U64;
  mem_mib_capacity?: U64;
  vms?: U64;
}

export interface PlacementPreview {
  /** The node the scheduler would pick, if any fits. */
  pick?: string;
  /** Every node, the scheduler's preference first. */
  nodes?: NodeFit[];
}

// ---- images and disks (spec/10-images.md) ------------------------------------
//
// The storage routes (/credentials, /images, /disks) answer in the proto3
// JSON shape of glidex.storage.v1 (spec/18-grpc.md §9.1): 64-bit numbers are
// strings, enums their full names, timestamps RFC 3339, fields at their
// default left out. The helpers below read them back for display.

/** A proto3 JSON 64-bit integer: a decimal string (a number is accepted too). */
export type U64 = string | number;

/** A 64-bit field as a number (absent: 0). */
export function num(v: U64 | null | undefined): number {
  if (v === undefined || v === null || v === "") return 0;
  const n = typeof v === "number" ? v : Number(v);
  return Number.isFinite(n) ? n : 0;
}

/** An RFC 3339 timestamp as Unix seconds (absent: 0). A number passes through. */
export function tsSecs(v: string | number | null | undefined): number {
  if (v === undefined || v === null || v === "") return 0;
  if (typeof v === "number") return v;
  const ms = Date.parse(v);
  return Number.isNaN(ms) ? 0 : Math.floor(ms / 1000);
}

/** An enum value without its prefix, lower-cased: `DISK_PHASE_READY` → `ready`. */
export function enumLabel(v: string | undefined, prefix: string): string {
  if (!v) return "";
  return (v.startsWith(prefix) ? v.slice(prefix.length) : v).toLowerCase();
}

export type Arch = "ARCH_X86_64" | "ARCH_AARCH64";

/** `ARCH_X86_64` → `x86_64` (the form nodes report). */
export const archLabel = (a: string | undefined) => enumLabel(a, "ARCH_");

/** `x86_64` → `ARCH_X86_64` for a request. */
export function archToProto(a: string): Arch {
  return (a.startsWith("ARCH_") ? a : `ARCH_${a.toUpperCase()}`) as Arch;
}

export type ProtoHypervisor = "HYPERVISOR_CLOUD_HYPERVISOR" | "HYPERVISOR_QEMU" | "HYPERVISOR_PYLON";

export const HYPERVISOR_FROM_PROTO: Record<ProtoHypervisor, HypervisorType> = {
  HYPERVISOR_CLOUD_HYPERVISOR: "cloudhypervisor",
  HYPERVISOR_QEMU: "qemu",
  HYPERVISOR_PYLON: "pylon",
};

export const HYPERVISOR_TO_PROTO: Record<HypervisorType, ProtoHypervisor> = {
  cloudhypervisor: "HYPERVISOR_CLOUD_HYPERVISOR",
  qemu: "HYPERVISOR_QEMU",
  pylon: "HYPERVISOR_PYLON",
};

/** A storage object's hypervisor in the VM API's form. */
export function hypervisorOf(h: ProtoHypervisor | undefined): HypervisorType | undefined {
  return h ? HYPERVISOR_FROM_PROTO[h] : undefined;
}

export type ConditionStatus = "CONDITION_STATUS_TRUE" | "CONDITION_STATUS_FALSE" | "CONDITION_STATUS_UNKNOWN";

/** A proto condition (glidex.common.v1.Condition): VMs and storage objects send these. */
export interface StorageCondition {
  kind: string;
  status?: ConditionStatus;
  reason?: string;
  message?: string;
  last_transition_at?: string;
}

/** An entry of an image's or disk's event log (glidex.common.v1.Event). */
export interface StorageEvent {
  at?: string;
  actor?: string;
  kind?: "EVENT_KIND_NORMAL" | "EVENT_KIND_WARNING";
  reason?: string;
  message?: string;
}

export type ImageState =
  | "IMAGE_STATE_DOWNLOADING"
  | "IMAGE_STATE_VERIFYING"
  | "IMAGE_STATE_READY"
  | "IMAGE_STATE_FAILED"
  | "IMAGE_STATE_MISSING";

export interface ImageStatus {
  state?: ImageState;
  received_bytes?: U64;
  total_bytes?: U64;
  reason?: string;
}

export interface CatalogSource {
  key?: string;
  url?: string;
  version?: string;
}

/** Where an image came from (a oneof: exactly one key is set). */
export interface ImageSource {
  catalog?: CatalogSource;
  url?: { url?: string; expected_sha256?: string };
  firmware?: CatalogSource;
  /** A kernel copied from host files. */
  host?: { path?: string };
  /** A kernel extracted from a disk image. */
  extracted?: { image_id?: string; image_sha256?: string; path?: string };
}

/** `IMAGE_KIND_DISK`: a cloud image disks are cloned from; `IMAGE_KIND_FIRMWARE`: UEFI firmware VMs boot through;
 * `IMAGE_KIND_KERNEL`: a kernel (and initramfs) VMs boot directly. */
export type ImageKind = "IMAGE_KIND_DISK" | "IMAGE_KIND_FIRMWARE" | "IMAGE_KIND_KERNEL";

export type DiskFormat = "DISK_FORMAT_QCOW2" | "DISK_FORMAT_RAW";

/** `DISK_FORMAT_QCOW2` → `qcow2`. */
export const formatLabel = (f: DiskFormat | undefined) => enumLabel(f, "DISK_FORMAT_");

/** `qemu-img info` of a file. */
export interface ImgInfo {
  format?: string;
  virtual_size?: U64;
  actual_size?: U64;
  backing_filename?: string;
  backing_format?: string;
}

export interface ImageCopy {
  node: string;
  node_name?: string;
  /** `wanted` is pinned and not started, `releasing` asked to go. */
  state: "downloading" | "ready" | "failed" | "wanted" | "releasing";
  bytes?: U64;
  error?: string;
  pinned?: boolean;
  /** A shared datastore's copy (`node` empty): what linked disks there overlay (spec/24-storage.md S9). */
  datastore?: string;
  datastore_name?: string;
}

export interface ImageInfo {
  id: string;
  name: string;
  kind?: ImageKind;
  /** The hypervisor a firmware image is built for. */
  hypervisor?: ProtoHypervisor;
  source?: ImageSource;
  status?: ImageStatus;
  format?: DiskFormat;
  virtual_size_bytes?: U64;
  file_size_bytes?: U64;
  sha256?: string;
  arch?: Arch;
  created_at?: string;
  verified?: boolean;
  path?: string;
  linked_disks?: string[];
  /** A firmware image carries a UEFI variable-store template (QEMU keeps a per-VM store; without one, `-bios`). */
  vars_template?: boolean;
  /** VMs booting through this firmware or kernel image. */
  used_by_vms?: string[];
  /** A kernel image's release, command line and initramfs. */
  kernel?: { release?: string; cmdline?: string; initrd?: { sha256?: string; size_bytes?: U64 } };
  /** The disk image a kernel image was extracted from. */
  for_image?: string;
  /** Each node's copy (spec/14-clustering.md §9.2.2, spec/16-clustering-ui.md §5.3). */
  copies?: ImageCopy[];
  info?: ImgInfo;
  /** Deletion requested; the image controller is removing it. */
  deleting?: boolean;
}

/** An image's state without its prefix: `ready`, `downloading`, … (`""` when unset). */
export const imageState = (i: ImageInfo) => enumLabel(i.status?.state, "IMAGE_STATE_");

export const isFirmware = (i: ImageInfo) => i.kind === "IMAGE_KIND_FIRMWARE";

export const isKernel = (i: ImageInfo) => i.kind === "IMAGE_KIND_KERNEL";

/** A cloud image disks can be made from (not firmware, not a kernel). */
export const isDiskImage = (i: ImageInfo) => !isFirmware(i) && !isKernel(i);

/** The catalog (or firmware catalog) key an image was pulled from. */
export function sourceKey(i: ImageInfo): { kind: "catalog" | "firmware" | "url"; key?: string } | undefined {
  const s = i.source;
  if (s?.catalog) return { kind: "catalog", key: s.catalog.key };
  if (s?.firmware) return { kind: "firmware", key: s.firmware.key };
  if (s?.url) return { kind: "url" };
  return undefined;
}

export interface CatalogItem {
  key: string;
  distro?: string;
  release?: string;
  arch?: Arch;
  url?: string;
  downloaded_image_id?: string;
}

export interface FirmwareCatalogItem {
  key: string;
  name?: string;
  hypervisor?: ProtoHypervisor;
  arch?: Arch;
  /** `download`: a pinned build; `debian`: a pinned Debian package, unpacked; `host`: copied from the host's package. */
  source?: "download" | "debian" | "host";
  url?: string;
  version?: string;
  /** Whether it can be pulled now (a host entry needs its package). */
  available?: boolean;
  hint?: string;
  downloaded_image_id?: string;
}

export interface PullImageRequest {
  catalog?: string;
  /** Firmware catalog key. */
  firmware?: string;
  url?: string;
  sha256?: string;
  name?: string;
  /** With `url`: the download is UEFI firmware for `hypervisor`. */
  kind?: ImageKind;
  hypervisor?: ProtoHypervisor;
  /** With a QEMU firmware `url`: its UEFI variable-store template. */
  vars_url?: string;
  vars_sha256?: string;
  /** In a cluster (spec/14-clustering.md §9.2.1–2): the architecture, nodes
   * that get a copy, and whether the server keeps its own. */
  arch?: Arch;
  nodes?: string[];
  keep_server_copy?: boolean;
  /** A kernel image (spec/23-kernel-boot.md §4.2): its initramfs, command line,
   * or host files (admins); `kernel_from` extracts one from a disk image. */
  initrd_url?: string;
  initrd_sha256?: string;
  cmdline?: string;
  kernel_path?: string;
  initrd_path?: string;
  kernel_from?: string;
  /** With a catalog or URL pull: also extract the image's kernel (default: on for a catalog pull). */
  with_kernel?: boolean;
}

export interface PullImageResponse {
  image?: ImageInfo;
  /** A new download (false: one was already in flight). */
  created?: boolean;
}

/** Ready firmware images for `hypervisor`, newest first (the server's default). */
export function firmwareFor(images: ImageInfo[], hypervisor: HypervisorType): ImageInfo[] {
  return images
    .filter((i) => {
      const hv = hypervisorOf(i.hypervisor);
      const matchesHv = hv === hypervisor || (hypervisor === "pylon" && hv === "cloudhypervisor");
      return isFirmware(i) && matchesHv && imageState(i) === "ready" && !i.deleting;
    })
    .sort((a, b) => tsSecs(b.created_at) - tsSecs(a.created_at));
}

export interface PartitionInfo {
  number?: number;
  start_bytes?: U64;
  size_bytes?: U64;
  type?: string;
  is_root?: boolean;
}

export interface PartitionTable {
  kind?: "PARTITION_TABLE_KIND_GPT" | "PARTITION_TABLE_KIND_MBR";
  partitions?: PartitionInfo[];
  free_tail_bytes?: U64;
}

export type CloneMode = "CLONE_MODE_LINKED" | "CLONE_MODE_FULL";

/** `CLONE_MODE_LINKED` → `linked`. */
export const cloneLabel = (m: CloneMode | undefined) => enumLabel(m, "CLONE_MODE_");

export type DiskPhase =
  | "DISK_PHASE_PENDING"
  | "DISK_PHASE_CREATING"
  | "DISK_PHASE_READY"
  | "DISK_PHASE_RESIZING"
  | "DISK_PHASE_MISSING"
  | "DISK_PHASE_FAILED";

export type ExtendMode = "EXTEND_MODE_OFFLINE" | "EXTEND_MODE_ON_BOOT";

/** What a disk is made from (a oneof: exactly one key is set). */
export interface DiskOrigin {
  blank?: Record<string, never>;
  image?: { image_id: string; mode?: CloneMode };
}

export interface DiskInfo {
  id: string;
  name: string;
  project?: string;
  format?: DiskFormat;
  size_bytes?: U64;
  origin?: DiskOrigin;
  attached_to?: string;
  /** The node holding the disk's file (spec/14-clustering.md §9.2). */
  node?: string;
  node_name?: string;
  pending_growpart?: boolean;
  /** `pending` waits for its image; `failed` and the `Ready` condition say why. */
  status?: "pending" | "creating" | "ready" | "resizing" | "busy" | "missing" | "failed";
  phase?: DiskPhase;
  conditions?: StorageCondition[];
  /** A resize not applied yet: a running VM has the disk open. */
  pending_size_bytes?: U64;
  /** The VM that owns it (created with the VM from an image). */
  owner?: string;
  deleting?: boolean;
  busy_op?: string;
  path?: string;
  created_at?: string;
  info?: ImgInfo;
  partition_table?: PartitionTable;
  extend_root?: "EXTEND_OUTCOME_GROWN" | "EXTEND_OUTCOME_ALREADY_FULL" | "EXTEND_OUTCOME_ON_BOOT" | "EXTEND_OUTCOME_SKIPPED";
  warnings?: string[];
  /** The datastore holding it: an id, `local` for the built-in one (spec/24-storage.md §4.2). */
  datastore?: string;
  /** On a shared datastore (no `node`): the node acting on it, its VM's when placed. */
  worker?: string;
  worker_name?: string;
  /** A move asked for and not done yet: the datastore it goes to (spec/24-storage.md §8.3). */
  moving_to?: string;
}

/** A disk's phase without its prefix: `ready`, `creating`, … (`""` when unset). */
export const diskPhase = (d: DiskInfo) => enumLabel(d.phase, "DISK_PHASE_");

/** A disk write's answer: `settled` is absent when not waited, else whether it finished in time. */
export interface DiskWriteResponse {
  disk?: DiskInfo;
  settled?: boolean;
}

/** Still changing: the disk controller is working on it. */
export function diskActivity(d: DiskInfo): string | null {
  if (d.deleting) return "Deleting";
  switch (d.status) {
    case "pending":
      return "Waiting for its image";
    case "creating":
      return "Creating";
    case "resizing":
      return "Resizing";
    case "busy":
      return d.busy_op ? `Busy (${d.busy_op})` : "Busy";
  }
  if (d.pending_size_bytes !== undefined) return "Resize waits for its VM to stop";
  return null;
}

/** Waiting on something outside the control plane (a VM to stop), not
 * working: worth showing, not worth polling fast for. */
export function diskWaiting(d: DiskInfo): boolean {
  return !d.deleting && diskPhase(d) === "ready" && d.status === "ready" && d.pending_size_bytes !== undefined;
}

/** A deletion, a download, its verification or a kernel extraction in progress. */
export function imageActivity(i: ImageInfo): string | null {
  if (i.deleting) return "Deleting";
  const state = imageState(i);
  if (state === "downloading") {
    const total = num(i.status?.total_bytes);
    return total ? `Downloading ${Math.floor((num(i.status?.received_bytes) * 100) / total)}%` : "Downloading";
  }
  return state === "verifying" ? "Verifying" : state === "extracting" ? "Extracting" : null;
}

/** A network deletion in progress (it waits for netd, or for a VM to leave). */
export function networkActivity(n: Network): string | null {
  if (!n.deletion_requested_at) return null;
  const why = notReady(n.conditions);
  return why && why.reason !== "Converged" ? `Deleting (${why.reason})` : "Deleting";
}

/** A condition's status in the short form, whichever shape it came in: a
 * proto one (`CONDITION_STATUS_TRUE`) or a legacy one (`True`). Absent: `Unknown`. */
export function conditionStatus(c: { status?: string }): Tristate {
  switch (c.status) {
    case "True":
    case "CONDITION_STATUS_TRUE":
      return "True";
    case "False":
    case "CONDITION_STATUS_FALSE":
      return "False";
    default:
      return "Unknown";
  }
}

/** The `Ready` condition when it isn't True: why an object hasn't converged.
 * Takes either condition shape (see `conditionStatus`). */
export function notReady<C extends { kind: string; status?: string }>(conditions?: C[]): C | undefined {
  return conditions?.find((c) => c.kind === "Ready" && conditionStatus(c) !== "True");
}

export interface CreateDiskRequest {
  name: string;
  project?: string;
  size_gib?: number;
  image?: string;
  clone?: CloneMode;
  format?: DiskFormat;
  extend_root?: boolean;
  node?: string;
  /** Datastore id or name; default: the default for disks. */
  datastore?: string;
}

/** What one node found at its path to a datastore (spec/24-storage.md §4.1). */
export interface DatastoreNode {
  node: string;
  node_name?: string;
  reachable?: boolean;
  /** Why not: `OutsideRoot`, `Missing`, `NotADirectory`, `ReadOnly`. */
  reason?: string;
  message?: string;
  path?: string;
  total_bytes?: U64;
  free_bytes?: U64;
  checked_at?: string;
  /** Risky mount options on a reachable shared datastore (NFS `soft`, local locks). */
  warnings?: string[];
}

/** Where disks live (spec/24-storage.md §13); `local` is built in. */
export interface Datastore {
  id: string;
  name: string;
  driver?: "DATASTORE_DRIVER_DIR" | "DATASTORE_DRIVER_SHARED_DIR" | "DATASTORE_DRIVER_LVM" | "DATASTORE_DRIVER_HYPERSTOR";
  /** `lvm`: the volume group (`node_paths`: each node's own) and its thin pool. */
  volume_group?: string;
  thin_pool?: string;
  /** `hyperstor`: its cluster API (`path` is the map service's addresses), pool, and the identity glidex enrolled with. */
  hyperstor_api?: string[];
  hyperstor_pool?: number;
  hyperstor_identity?: string;
  hyperstor_identity_expires?: string;
  /** `SHARED`: one filesystem every node mounts; disks on it are bound to no node. */
  topology?: "DATASTORE_TOPOLOGY_NODE_LOCAL" | "DATASTORE_TOPOLOGY_SHARED";
  /** The directory on nodes without their own (`node_paths`). */
  path?: string;
  /** Node id → that node's directory. */
  node_paths?: Record<string, string>;
  /** The nodes it is on; empty: every node. */
  nodes?: string[];
  /** New disks go here unless they name a datastore. */
  default_for_disks?: boolean;
  overcommit?: boolean;
  builtin?: boolean;
  created_at?: string;
  node_status?: DatastoreNode[];
  disks?: number;
}

/** `POST /datastores`; `PATCH` takes the same fields but `name` and `path`. */
export interface DatastoreInput {
  name?: string;
  /** Create only: one filesystem every node mounts (`shared-dir`). */
  shared?: boolean;
  /** Create only: LVM-thin (`lvm`), with `volume_group` and `thin_pool` instead of `path`. */
  lvm?: boolean;
  /** Create only: a hyperstor cluster's pool, with `hyperstor_api`, `hyperstor_pool` and a `mgmt` token to enroll with. */
  hyperstor?: boolean;
  hyperstor_api?: string[];
  hyperstor_pool?: number;
  hyperstor_token?: string;
  volume_group?: string;
  thin_pool?: string;
  path?: string;
  node_paths?: Record<string, string>;
  nodes?: string[];
  default_for_disks?: boolean;
  overcommit?: boolean;
}

/** A byte count for display; takes a 64-bit proto field (a string) too. */
export function formatBytes(bytes: U64 | undefined): string {
  const n = num(bytes);
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let v = n;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return u === 0 ? `${n} B` : `${v.toFixed(1)} ${units[u]}`;
}

/** Guest login stored by the control plane. The password hash is never sent to clients. */
export interface CredentialInfo {
  project?: string;
  username: string;
  has_password?: boolean;
  ssh_authorized_keys?: string[];
  /** RFC 3339. */
  created_at?: string;
  updated_at?: string;
}

export interface CreateCredentialRequest {
  project?: string;
  username: string;
  password?: string;
  ssh_authorized_keys?: string[];
}

export interface UpdateCredentialRequest {
  password?: string;
  ssh_authorized_keys?: string[];
}

/** A REST error: `glidex.api.v1.Error` as JSON (spec/18-grpc.md §9.1), its
 * `details` narrowed to the keys the pages read. RPC errors become the
 * same thing (`api.ts::rpcError`). */
export type ApiError = Required<Pick<ErrorJson, "error" | "message">> & {
  details?: {
    impact?: string;
    missing?: string[];
    reasons?: string[];
    min_size_bytes?: number;
    /** 422 invalid_policy: the validator's messages. */
    errors?: string[];
  };
};

export interface HealthResponse {
  status: string;
}

export function stateColor(state: VmState): string {
  switch (state) {
    case "running":
      return "bg-green-500";
    case "stopped":
      return "bg-red-500";
    case "paused":
      return "bg-yellow-500";
    case "created":
      return "bg-blue-500";
    case "starting":
    case "stopping":
      return "bg-sky-400";
    case "failed":
    case "unknown":
      return "bg-red-700";
  }
}

/** Whether the VM is where its desired state says (nothing to wait for). */
export function settled(vm: VmResponse): boolean {
  return vmActivity(vm) === null;
}

/** What the controller is still doing for a VM (spec/12-reconciliation.md
 * §7.4), or `null` once it has converged. */
export function vmActivity(vm: VmResponse): string | null {
  if (vm.deleting) return "Deleting";
  const desired = desiredState(vm);
  if (!desired) return null;
  const state = vmState(vm);
  const there = desired === "stopped" ? state === "stopped" || state === "created" : state === desired;
  if (!there) {
    if (desired === "stopped") return "Stopping";
    if (desired === "paused") return state === "running" ? "Pausing" : "Starting";
    return state === "paused" ? "Resuming" : "Starting";
  }
  if (generationBehind(vm)) return "Applying changes";
  if (notReady(vm.conditions)) return "Reconciling";
  return null;
}

/** The controller hasn't acted on the VM's latest spec yet. Absent
 * generations are 0 now (proto3 leaves defaults out). */
export function generationBehind(vm: VmResponse): boolean {
  return num(vm.observed_generation) < num(vm.generation);
}

/** What keeps the VM from its desired state (the `Ready` condition). */
/** The VM's node stopped reporting (spec/14-clustering.md §7.2): its last
 * known state is shown, but the VM may still be running. */
export function nodeUnreachable(vm: VmResponse): boolean {
  return vm.conditions?.some((c) => c.kind === "Ready" && conditionStatus(c) === "Unknown" && c.reason === "NodeUnreachable") ?? false;
}

/** Why the scheduler hasn't placed the VM, when it hasn't. */
export function unscheduledReason(vm: VmResponse): string | null {
  const s = vm.conditions?.find((c) => c.kind === "Scheduled");
  if (!s || conditionStatus(s) !== "False") return null;
  return s.message || s.reason || null;
}

export function notReadyReason(vm: VmResponse): string | null {
  const ready = notReady(vm.conditions);
  if (!ready) return null;
  return ready.message ? `${ready.reason ?? ""}: ${ready.message}` : (ready.reason ?? "");
}

/** "Crashed (exit 1) 5 min ago": the instance's last exit, for people. */
export function describeExit(e: ExitRecord): string {
  const cause: Record<string, string> = {
    requested: "Stopped on request",
    terminated: "Stopped by the host",
    clean_exit: "Guest powered off",
    crashed: "Crashed",
    launch_failed: "Failed to launch",
    host_reboot: "Host rebooted",
    lost: "Lost",
  };
  const detail = e.signal !== undefined ? `signal ${e.signal}` : e.code !== undefined && e.code !== 0 ? `exit ${e.code}` : null;
  const c = enumLabel(e.cause, "EXIT_CAUSE_");
  return `${cause[c] ?? c}${detail ? ` (${detail})` : ""}, ${new Date(tsSecs(e.at) * 1000).toLocaleString()}${
    e.message ? `: ${e.message}` : ""
  }`;
}

export function stateLabel(state: VmState): string {
  return state.charAt(0).toUpperCase() + state.slice(1);
}

// ---- access control (spec/11-security.md §5–§7, §10, §13) ----------------------

/** A Cedar entity reference, as the API serializes it. */
export type EntityType =
  | "Cluster"
  | "Host"
  | "Node"
  | "User"
  | "Team"
  | "Token"
  | "Project"
  | "Vm"
  | "Disk"
  | "Credential"
  | "Image"
  | "Network"
  | "PciDevice" | "Tenant" | "Directory";

/** Entity types that take no id: the cluster, and the host serving the request. */
export type SingletonEntityType = "Cluster" | "Host";

/** `Cluster` is the root of everything cluster-wide (projects, identity,
 * policy); `Host` is the node serving the request and `Node` another node. */
export type EntityRef = { type: SingletonEntityType } | { type: Exclude<EntityType, SingletonEntityType>; id: string };

export function isSingleton(t: EntityType): t is SingletonEntityType {
  return t === "Cluster" || t === "Host";
}

export function entityLabel(e: EntityRef): string {
  return "id" in e ? `${e.type}:${e.id}` : e.type;
}

export interface LoginConnector {
  id: string;
  kind: "pam" | "oidc" | "google" | "jumpcloud";
  display_name: string;
}

export interface AuthMethods {
  pam: boolean;
  oidc: boolean;
  /** Authentication is off (development): everyone is the system user. */
  disabled: boolean;
  /** The directory's enabled connectors (absent from older servers). */
  connectors?: LoginConnector[];
  /** Passkeys are on (spec/25-passkeys.md), and their domain when set. */
  passkeys?: boolean;
  passkeys_rp_id?: string | null;
  /** The directory these methods sign in to (spec/19-tenancy.md D61). */
  directory?: string;
  /** Other organizations' directories this cluster trusts (D69). */
  trusted_directories?: TrustedLogin[];
}

/** One of the browser's accounts (spec/19-tenancy.md D71). */
export interface BrowserAccount {
  directory: string;
  directory_name: string;
  /** `trusted`: another organization's directory (D69). */
  kind: "hosted" | "trusted";
  user_name?: string | null;
  signed_in: boolean;
  active: boolean;
}

/** "Sign in with <host>": a trusted directory's login (spec/19-tenancy.md D69). */
export interface TrustedLogin {
  /** Its records' id here, `trusted-<hash>`. */
  id: string;
  /** Its issuer's host. */
  host: string;
  /** Its tenants admitted on this cluster. */
  tenants: string[];
}

// The access routes (projects, bindings, users, teams, tokens, policies,
// audit, capability checks, whoami) answer in the proto3 JSON shape of
// glidex.access.v1 (spec/18-grpc.md §9.1): timestamps RFC 3339, enums by their
// full names, 64-bit numbers as strings, fields at their default left out.

export interface Identity {
  provider: string;
  subject: string;
  user_id: string;
  email?: string;
  /** RFC 3339. */
  created_at?: string;
}

/** A user (glidex.access.v1.User). */
export interface User {
  id: string;
  display_name: string;
  disabled?: boolean;
  default_project?: string;
  /** RFC 3339. */
  created_at?: string;
  /** Sent by `GET /users`. */
  identities?: Identity[];
  /** The connector that provisions it (spec/19-tenancy.md §8.3). */
  managed_by?: string;
  external_id?: string;
  /** A managed user's email, from its connector. */
  email?: string;
  /** Why it is disabled. */
  disabled_reason?: "admin" | "scim" | "connector_deleted" | "sync";
  /** Pre-added, not signed in yet: the connector whose first login with
   *  `email` takes it (spec/19-tenancy.md D77). */
  pending_connector?: string;
}

export type UserView = User;

export interface ProjectRole {
  project: string;
  project_name?: string;
  role: string;
  via: EntityRef;
}

/** `GET /auth/whoami` and a PAM login's answer (glidex.system.v1.WhoAmIResponse). */
export interface WhoAmI {
  /** Absent for a service-account token. */
  user_id?: string;
  display_name?: string;
  method: "peer" | "pam" | "oidc" | "token" | "disabled";
  token_id?: string;
  teams?: string[];
  break_glass?: boolean;
  host_roles?: string[];
  project_roles?: ProjectRole[];
  default_project?: string;
  mfa?: boolean;
  directory_id?: string;
  /** Absent for a service-account token. */
  user?: User;
  /** The token the caller presented. */
  token?: { id: string; name: string; device?: string; client?: string };
  /** A browser session's CSRF value. */
  csrf?: string;
}

export type QuotaKey = "vms" | "vcpus" | "memory_mib" | "disk_gib" | "running_vms" | "networks" | "routers" | "external_ips";

/** Limits (glidex.tenancy.v1.Quotas): an absent one is unlimited. VPC routers,
 * and those with an external address, are spec/14-clustering.md §11.2a;
 * `devices` is a limit per device pool (spec/22-host-topology.md §9.3). */
export type Quotas = Partial<Record<QuotaKey, number>> & { devices?: Record<string, number> };

/** What a project uses; a count of zero is left out. */
export type Usage = Partial<Record<QuotaKey, number>> & { devices?: Record<string, number> };

/** Pools named by a quota or a usage, by name. */
export const quotaPools = (q?: Quotas, u?: Usage): string[] =>
  [...new Set([...Object.keys(q?.devices ?? {}), ...Object.keys(u?.devices ?? {})])].sort();

export const QUOTA_KEYS: QuotaKey[] = ["vms", "running_vms", "vcpus", "memory_mib", "disk_gib", "networks", "routers", "external_ips"];

export const QUOTA_LABELS: Record<QuotaKey, string> = {
  vms: "VMs",
  running_vms: "Running VMs",
  vcpus: "vCPUs",
  memory_mib: "Memory (MiB)",
  disk_gib: "Disk (GiB)",
  networks: "Networks",
  routers: "Routers",
  external_ips: "External IPs",
};

/** A project (glidex.access.v1.Project). */
export interface Project {
  id: string;
  name: string;
  /** The tenant owning it (spec/19-tenancy.md D1). */
  tenant_id?: string;
  description?: string;
  quotas?: Quotas;
  /** RFC 3339. */
  created_at?: string;
  /** On list and get only. */
  usage?: Usage;
}

export type ProjectView = Project;

export interface TeamMember {
  user_id: string;
  /** `manual`, `pam`, `oidc` or `connector:<id>`. */
  source: string;
}

export interface Team {
  id: string;
  name: string;
  members?: TeamMember[];
  /** RFC 3339. */
  created_at?: string;
  /** The connector whose group it is (`group_mode: auto`). */
  managed_by?: string;
  external_id?: string;
}

/** A role link (a Cedar template-linked policy; glidex.access.v1.Binding). */
export interface Binding {
  id: string;
  role: string;
  principal: EntityRef;
  /** The principal's display name (user), team or token name, for callers
   * who may read the link but not list users or teams. */
  principal_name?: string;
  resource?: EntityRef;
  created_by?: string;
  /** RFC 3339. */
  created_at?: string;
}

export const PROJECT_ROLES = ["role.viewer", "role.operator", "role.editor", "role.owner"] as const;
export const HOST_ROLES = [
  "role.auditor",
  "role.image-admin",
  "role.net-admin",
  "role.system-admin",
  "grant.host-paths",
] as const;
/** System roles that can be given on one node's host (spec/14-clustering.md §12.2). */
export const PER_HOST_ROLES = ["role.auditor", "role.net-admin", "grant.host-paths"] as const;

export function roleLabel(role: string): string {
  return role.replace(/^(role|grant)\./, "");
}

export type TokenKind = "TOKEN_KIND_PERSONAL" | "TOKEN_KIND_SERVICE_ACCOUNT";

/** An API token (glidex.access.v1.Token); never its secret. */
export interface ApiToken {
  id: string;
  name: string;
  kind?: TokenKind;
  owner?: string;
  project?: string;
  created_by?: string;
  /** RFC 3339, like the other times. */
  created_at?: string;
  expires_at?: string;
  last_used_at?: string;
  last_used_from?: string;
  device?: string;
  client?: string;
  roles?: Binding[];
  /** The clusters it works on (spec/19-tenancy.md §7.4); none: only here. */
  clusters?: string[];
}

export const isServiceToken = (t: Pick<ApiToken, "kind">) => t.kind === "TOKEN_KIND_SERVICE_ACCOUNT";

export interface CreateTokenRequest {
  name: string;
  expires_in_days?: number;
  kind?: TokenKind;
  project?: string;
  roles?: { role: string; project?: string }[];
  device?: string;
  client?: string;
  /** Clusters of other organizations it works on too, or `["all"]`
   *  (spec/19-tenancy.md §7.4); none: only this cluster. */
  clusters?: string[];
}

export interface CreatedToken {
  /** The secret: shown once, never again. */
  token: string;
  record: ApiToken;
}

export type PolicySource = "base" | "role" | "link" | "site" | "file";
export type ProtoPolicySource = `POLICY_SOURCE_${Uppercase<PolicySource>}`;

/** `POLICY_SOURCE_SITE` → `site`. */
export const policySource = (s: ProtoPolicySource | undefined) => enumLabel(s, "POLICY_SOURCE_") as PolicySource | "";

/** A policy the engine has loaded (glidex.access.v1.LoadedPolicy). */
export interface LoadedPolicy {
  id: string;
  source?: ProtoPolicySource;
  template?: boolean;
  text?: string;
}

export type PolicyInfo = LoadedPolicy;

export interface SitePolicy {
  id: string;
  text?: string;
  description?: string;
  enabled?: boolean;
  /** uint64: read with `num`. */
  version?: U64;
  updated_by?: string;
  /** RFC 3339. */
  updated_at?: string;
}

export interface SitePolicyVersion {
  id: string;
  version?: U64;
  text?: string;
  enabled?: boolean;
  author?: string;
  /** RFC 3339. */
  time?: string;
  deleted?: boolean;
}

export interface Decision {
  allowed?: boolean;
  policies?: string[];
  errors?: string[];
}

export interface SimulationRequest {
  principal: EntityRef;
  action: string;
  resource: EntityRef;
}

/** One simulated request: `error`, or the decisions now and with the change. */
export interface SimulationResult {
  error?: string;
  current?: Decision;
  candidate?: Decision;
}

export interface PolicyChange {
  id: string;
  text?: string;
  description?: string;
  enabled?: boolean;
  delete?: boolean;
}

export interface AuditEntry {
  /** RFC 3339, with milliseconds. */
  time: string;
  request_id: string;
  /** Absent for unauthenticated requests; `{ system }` for the control plane's own writes. */
  principal?: { user?: string | null; name?: string | null; token?: string | null; method?: string; system?: string; [k: string]: unknown } | null;
  source: string;
  action: string;
  project?: string;
  target?: string;
  result: string;
  error_code?: string;
  policies?: string[];
  details?: Record<string, unknown>;
}

export interface NetworkShare {
  /** The network's id. */
  network: string;
  network_name?: string;
  owner_project?: string;
  status?: "SHARE_STATUS_OFFERED" | "SHARE_STATUS_ACCEPTED";
  /** An offer's (RFC 3339). */
  expires_at?: string;
}

/** Unix seconds → local date and time. */
export function formatTime(secs: number | undefined | null): string {
  if (!secs) return "—";
  return new Date(secs * 1000).toLocaleString();
}

// ---- usage (spec/13-metering.md §9; glidex.usage.v1, proto JSON shape) ---------
// Fields at their default are left out (empty lists, maps and strings),
// 64-bit numbers are strings, enums are their proto names.

export interface Named {
  id: string;
  name?: string;
}

export interface MeterValue {
  /** uint64, as a string. */
  raw?: string;
  value?: number;
  unit?: string;
}

/** What the UI groups by; requests send these words. */
export type GroupKey = "project" | "vm" | "disk" | "nic" | "network" | "node";
/** The same keys in answers (`GROUP_KEY_VM`). */
export type ProtoGroupKey = `GROUP_KEY_${Uppercase<GroupKey>}`;
export type Granularity = "GRANULARITY_HOUR" | "GRANULARITY_DAY" | "GRANULARITY_MONTH";

export interface UsageRow {
  start: string;
  end: string;
  project?: Named;
  vm?: Named;
  disk?: Named;
  nic?: Named;
  network?: Named;
  node?: Named;
  meters?: Record<string, MeterValue>;
  flags?: string[];
}

export interface UsageResponse {
  from: string;
  to: string;
  granularity?: Granularity;
  timezone?: string;
  group_by?: ProtoGroupKey[];
  rows?: UsageRow[];
  complete_through: string;
  metering_started_at: string;
}

export interface SlotCounts {
  counted?: number;
  interpolated?: number;
}

/** Averages, peaks or p95s of one family; absent when unknown. */
export type Rates = Record<string, number | undefined> & { slots?: SlotCounts };

export interface RateRow {
  project?: Named;
  vm?: Named;
  disk?: Named;
  nic?: Named;
  network?: Named;
  node?: Named;
  avg?: Rates;
  peak?: Rates;
  p95?: Rates;
  slots?: SlotCounts;
  latency_source?: "LATENCY_SOURCE_COUNTER" | "LATENCY_SOURCE_NONE";
}

export interface RateResponse {
  /** Set for a billing month. */
  month?: string;
  from: string;
  to: string;
  timezone?: string;
  final?: boolean;
  from_final_figures?: boolean;
  group_by?: ProtoGroupKey[];
  rows?: RateRow[];
}

/** One 5-minute point of a series; which rates are set depends on the series. */
export type SeriesPoint = { slot: string } & Partial<Record<string, number | string>>;

export interface SeriesResponse {
  from: string;
  to: string;
  points?: SeriesPoint[];
  p95?: Rates;
  /** `GET /vms/{id}/io`: disk id → that disk's series. */
  disks?: Record<string, SeriesResponse>;
}

export interface LiveEntry {
  id: string;
  name?: string;
  network?: string;
  values?: Record<string, number>;
}

export interface VmStats {
  /** Absent when there is no recent sample. */
  sampled_at?: string;
  resolution_secs?: number;
  vm?: Record<string, number>;
  nics?: LiveEntry[];
  disks?: LiveEntry[];
}

// ---- the cluster, nodes, imports and departures (glidex.cluster.v1) -----------
//
// `/cluster/*`, `/nodes*`, `/imports*` and `/departures` answer in the proto3
// JSON shape of glidex.cluster.v1 (spec/18-grpc.md §9.1): enums by their full
// names, times RFC 3339, 64-bit numbers as strings (read with `num`), fields at
// their default left out. The helpers read them back in the words the pages
// show (`NODE_PHASE_ACTIVE` → `Active`, `NODE_ROLE_SERVER` → `server`).

export type NodeRole = "NODE_ROLE_SERVER" | "NODE_ROLE_AGENT";
export type NodePhase =
  | "NODE_PHASE_ACTIVE"
  | "NODE_PHASE_DRAINING"
  | "NODE_PHASE_DEPARTING"
  | "NODE_PHASE_DEPARTED"
  | "NODE_PHASE_REMOVED"
  | "NODE_PHASE_FORGOTTEN";
/** A phase as the pages show it. */
export type NodePhaseLabel = "Active" | "Draining" | "Departing" | "Departed" | "Removed" | "Forgotten";
export type Tristate = "True" | "False" | "Unknown";
export type BrIntDatapath = "BR_INT_DATAPATH_AUTO" | "BR_INT_DATAPATH_SYSTEM" | "BR_INT_DATAPATH_NETDEV";
export type RaftState = "RAFT_STATE_LEADER" | "RAFT_STATE_FOLLOWER" | "RAFT_STATE_CANDIDATE" | "RAFT_STATE_LEARNER" | "RAFT_STATE_SHUTDOWN";
export type ImportState = "IMPORT_STATE_PENDING" | "IMPORT_STATE_COMMITTED" | "IMPORT_STATE_REJECTED";
export type DepartureState = "DEPARTURE_STATE_FROZEN" | "DEPARTURE_STATE_COMMITTED" | "DEPARTURE_STATE_ABORTED";
export type TunnelState = "TUNNEL_STATE_PRESENT" | "TUNNEL_STATE_ERROR" | "TUNNEL_STATE_BFD_DOWN";
export type OvnSettingsSource = "OVN_SETTINGS_SOURCE_CLUSTER" | "OVN_SETTINGS_SOURCE_FILE" | "OVN_SETTINGS_SOURCE_NONE";

/** `bfd_down` → `Bfd_down`: the first letter up. */
const capitalized = (s: string) => (s ? s.charAt(0).toUpperCase() + s.slice(1) : s);

/** `NODE_ROLE_SERVER` → `server`. */
export const nodeRoleLabel = (r: string | undefined) => enumLabel(r, "NODE_ROLE_");
/** `NODE_PHASE_ACTIVE` → `Active` (absent: `Active`; the server always sends it). */
export const nodePhaseLabel = (p: string | undefined) => (capitalized(enumLabel(p, "NODE_PHASE_")) || "Active") as NodePhaseLabel;
/** `RAFT_STATE_LEADER` → `Leader`. */
export const raftStateLabel = (s: string | undefined) => capitalized(enumLabel(s, "RAFT_STATE_")) || "Unknown";
/** `IMPORT_STATE_PENDING` → `pending`. */
export const importStateLabel = (s: string | undefined) => enumLabel(s, "IMPORT_STATE_");
/** `DEPARTURE_STATE_FROZEN` → `frozen`. */
export const departureStateLabel = (s: string | undefined) => enumLabel(s, "DEPARTURE_STATE_");
/** `TUNNEL_STATE_BFD_DOWN` → `bfd_down`. */
export const tunnelStateLabel = (s: string | undefined) => (enumLabel(s, "TUNNEL_STATE_") || "error") as "present" | "error" | "bfd_down";
/** `OVN_SETTINGS_SOURCE_FILE` → `file`. */
export const ovnSourceLabel = (s: string | undefined) => enumLabel(s, "OVN_SETTINGS_SOURCE_") || "none";
/** `BR_INT_DATAPATH_SYSTEM` → `system` (absent: `auto`). */
export const brIntLabel = (d: string | undefined) => enumLabel(d, "BR_INT_DATAPATH_") || "auto";

export interface NodeResources {
  cpus?: number;
  /** uint64: read with `num`. */
  memory_mib?: U64;
  /** Page size in KiB → pages (uint64 strings). */
  hugepages?: Record<string, U64>;
}

/** A file in a node's image directory that no image record names (spec/14-clustering.md §9.2.3). */
export interface SpareFile {
  name: string;
  bytes?: U64;
  /** RFC 3339. */
  mtime?: string;
}

/** A node (glidex.cluster.v1.Node) as `GET /nodes` and the live stream send it. */
export interface NodeRecord {
  meta: { id: string; name?: string; created_at?: string; generation?: U64; resource_version?: U64 };
  spec: {
    name: string;
    role?: NodeRole;
    unschedulable?: boolean;
    labels?: Record<string, string>;
    /** Absent: auto, `netdev` where DPDK runs, else `system` (spec/14-clustering.md §11.6). */
    br_int_datapath?: BrIntDatapath;
    /** physnet → bridge set through the API (the host's file may add more). */
    bridge_mappings?: Record<string, string>;
    drop_spare_files_before?: string;
    /** PCI devices no new VM gets (spec/22-host-topology.md §5.4). */
    cordoned_devices?: string[];
  };
  status: {
    phase?: NodePhase;
    ready?: ConditionStatus;
    ready_reason?: string;
    observed_generation?: U64;
    raft_id?: U64;
    advertise?: string;
    tunnel_ip?: string;
    /** A server's web UI address (spec/28-host-page.md W3). */
    ui_url?: string;
    versions?: { glidex?: string; ovs?: string; ovn?: string };
    capacity?: NodeResources;
    allocatable?: NodeResources;
    features?: {
      kvm?: boolean;
      hypervisors?: string[];
      dpdk?: boolean;
      iommu?: boolean;
      /** `system` or `netdev`, as the node runs br-int now (a plain string). */
      br_int_datapath?: string;
      physnets?: Record<string, string>;
      feature_level?: number;
      ovn_central?: boolean;
      bridge_addrs?: Record<string, { address: string; gateway?: string }>;
      /** `x86_64` or `aarch64` (spec/14-clustering.md §9.2.1). */
      arch?: string;
      spare_image_files?: SpareFile[];
    };
    pci_devices?: string[];
    /** When Ready last changed (RFC 3339). */
    heartbeat?: string;
    departed_ids?: string[];
  };
}

/** A node's readiness in the short form (`True`, `False`, `Unknown`). */
export const nodeReady = (n: NodeRecord): Tristate => conditionStatus({ status: n.status.ready });

/** What a node's `br-int` datapath should be, by its setting. */
export function wantedBrInt(n: NodeRecord): "system" | "netdev" {
  const s = brIntLabel(n.spec.br_int_datapath);
  if (s === "system" || s === "netdev") return s;
  return n.status.features?.dpdk ? "netdev" : "system";
}

/** A node is gone for good (kept as a record only). */
export function isTombstone(phase: NodePhase | undefined): boolean {
  return phase === "NODE_PHASE_DEPARTED" || phase === "NODE_PHASE_REMOVED" || phase === "NODE_PHASE_FORGOTTEN";
}

/** A node's name, with its short id when it is a tombstone: a node that left
 * keeps its name, and a host that joins again takes the same one. */
export function nodeLabel(n: NodeRecord): string {
  return isTombstone(n.status.phase) ? `${n.spec.name} · ${n.meta.id.slice(0, 8)}` : n.spec.name;
}

/** A node as the cluster status lists it (glidex.cluster.v1.NodeSummary). */
export interface ClusterNodeRow {
  id: string;
  name: string;
  role?: NodeRole;
  phase?: NodePhase;
  advertise?: string;
  raft_id?: U64;
  version?: string;
  feature_level?: number;
  ready?: ConditionStatus;
  /** Its newest certificate: signed by the current CA, and until when (RFC 3339). */
  cert?: { current_ca?: boolean; not_after?: string };
}

export interface RaftMember {
  raft_id?: U64;
  name?: string;
  address?: string;
}

/** `GET /cluster/status` (glidex.cluster.v1.GetClusterStatusResponse). */
export interface ClusterStatus {
  /** Always sent by `GET /cluster/status`. */
  clustered?: boolean;
  cluster_id?: string;
  /** The node serving this UI (through an agent's UI: that agent). */
  node_id?: string;
  name?: string;
  role?: NodeRole;
  advertise?: string;
  /** The server that answered, when it isn't the node serving this UI
   * (an agent relays its UI to a server, spec/16-clustering-ui.md §3.6). */
  served_by?: { node_id: string; name: string };
  feature_level?: number;
  ca_fingerprint?: string;
  /** Times RFC 3339. */
  ca?: { signing?: string; retiring?: string[]; retire_at?: string; started_at?: string; not_after?: string };
  /** Absent on an agent. uint64s as strings. */
  raft?: {
    id?: U64;
    state?: RaftState;
    term?: U64;
    leader?: RaftMember;
    voters?: RaftMember[];
    learners?: RaftMember[];
    last_applied?: U64;
    last_log_index?: U64;
    purged?: U64;
  };
  nodes?: ClusterNodeRow[];
  ports?: { node: string; port: number; proto: string; reachable?: boolean }[];
}

/** `"Leader"` → `RAFT_STATE_LEADER`: a legacy enum value in the proto form. */
function protoEnum(v: unknown, prefix: string): unknown {
  if (typeof v !== "string" || !v || v.startsWith(`${prefix}_`)) return v;
  return `${prefix}_${v.toUpperCase()}`;
}

/** Unix seconds → RFC 3339 (0 or null: absent); a string passes through. */
function protoTime(v: unknown): string | undefined {
  if (typeof v === "string") return v || undefined;
  if (typeof v === "number" && v) return new Date(v * 1000).toISOString();
  return undefined;
}

/** Drop nulls, recursively (proto JSON leaves those fields out). */
function withoutNulls(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(withoutNulls);
  if (v && typeof v === "object") {
    return Object.fromEntries(
      Object.entries(v as Record<string, unknown>)
        .filter(([, x]) => x !== null)
        .map(([k, x]) => [k, withoutNulls(x)]),
    );
  }
  return v;
}

/** The live stream's `cluster` object as a `ClusterStatus`. The stream sends
 * the summary in the older shape (`role: "server"`, `raft.state: "Leader"`,
 * unix-second times); this reads either shape. */
export function clusterStatusOf(raw: unknown): ClusterStatus {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const s = withoutNulls(raw) as Record<string, any>;
  if (!s || typeof s !== "object") return {};
  s.role = protoEnum(s.role, "NODE_ROLE");
  if (s.ca) for (const k of ["retire_at", "started_at", "not_after"]) s.ca[k] = protoTime(s.ca[k]);
  if (s.raft) s.raft.state = protoEnum(s.raft.state, "RAFT_STATE");
  for (const n of s.nodes ?? []) {
    n.role = protoEnum(n.role, "NODE_ROLE");
    n.phase = protoEnum(n.phase, "NODE_PHASE");
    n.ready = protoEnum(n.ready, "CONDITION_STATUS");
    if (n.cert) n.cert.not_after = protoTime(n.cert.not_after);
  }
  return s as ClusterStatus;
}

/** What one import plan brings (`GET /imports/{plan}`), for the mapping editor. */
export interface ImportSources {
  projects?: { id: string; name?: string }[];
  /** `mode` is `nat`, `bridged` or `isolated`. */
  networks?: { id: string; name?: string; mode?: string }[];
  /** `key` is `<project>/<username>`. */
  credentials?: { key: string; project?: string; username?: string }[];
  vms?: { id: string; name?: string; project?: string }[];
}

/** The names an import gets here (the editor keeps every key; the API leaves empty ones out). */
export interface ImportMappings {
  projects: Record<string, string>;
  networks: Record<string, string>;
  credentials: Record<string, string>;
  over_quota: boolean;
}

/** A chassis or node reference in OVN views (glidex.cluster.v1.ChassisRef). */
export interface ChassisRef {
  chassis: string;
  node?: string;
  node_name?: string;
  /** int64: read with `num`. */
  priority?: U64;
}

/** The shared edge of NAT cluster networks (spec/14-clustering.md §11.2). */
export interface OvnEdge {
  physnet?: string;
  external_cidr?: string;
  gateway?: string;
  external_ip?: string;
  external_pool?: string;
  gateway_nodes?: string[];
}

/** The cluster's OVN setting (spec/16-clustering-ui.md §8a). */
export interface OvnSettings {
  enabled?: boolean;
  underlay_mtu?: number;
  nat_supernet?: string;
  edge?: OvnEdge;
  /** `[first, last]`. */
  snat_ct_zones?: number[];
  dns_servers?: string[];
  updated_by?: string;
  /** RFC 3339. */
  updated_at?: string;
}

/** `PUT /cluster/settings/ovn`: the whole setting (absent: none / the default). */
export type PutOvnSettings = Omit<OvnSettings, "updated_by" | "updated_at"> & { enabled: boolean };

/** A node's OVN readiness (glidex.cluster.v1.OvnNode). */
export interface OvnNode {
  id: string;
  name: string;
  role?: NodeRole;
  ovn_version?: string;
  ovn_central?: boolean;
  /** What it lacks to run OVN. */
  missing?: string;
  /** Its ovn-controller is older than the cluster's ovn-northd, so it drops traffic of the newer pipeline. */
  outdated?: string;
}

/** `GET /cluster/settings/ovn`: `enabled` for everyone, the rest with `readCluster`. */
export interface OvnSettingsView {
  enabled?: boolean;
  /** The shared edge for NAT networks spanning the nodes is set. */
  edge?: boolean;
  /** The edge has a pool of addresses for VPC routers that reach outside. */
  vpc_pool?: boolean;
  clustered?: boolean;
  /** Read with `ovnSourceLabel`: `cluster`, `file` (control-plane.json, not moved yet) or `none`. */
  source?: OvnSettingsSource;
  settings?: OvnSettings;
  nodes?: OvnNode[];
  /** Cluster networks still there (OVN can't be turned off while any is). */
  cluster_networks?: { id: string; name?: string }[];
}

/** `GET /cluster/settings/ovn/edge-defaults`. */
export interface EdgeDefaults {
  edge?: OvnEdge;
  complete?: boolean;
  notes?: string[];
}

export interface OvnServer {
  address: string;
  reachable?: boolean;
  leader?: boolean;
}

/** An OVN chassis with the glidex node it is. */
export interface OvnChassis {
  chassis: string;
  node?: string;
  node_name?: string;
  hostname?: string;
  encaps?: string[];
  /** When its ovn-controller last caught up with the northbound database (RFC 3339). */
  nb_cfg_at?: string;
}

/** `GET /cluster/ovn` (spec/16-clustering-ui.md §8). */
export interface OvnOverview {
  enabled?: boolean;
  servers?: number;
  nb?: OvnServer[];
  sb?: OvnServer[];
  chassis?: OvnChassis[];
  gateways?: { router: string; active?: ChassisRef; candidates?: ChassisRef[] }[];
}

export interface NodeRef {
  node: string;
  node_name?: string;
}

export interface Tunnel {
  name: string;
  remote_ip?: string;
  /** Read with `tunnelStateLabel`: `present`, `error` (OVS couldn't set it up) or `bfd_down`. */
  state?: TunnelState;
  error?: string;
  bfd_state?: string;
  bfd_diagnostic?: string;
  /** uint64s: read with `num`. */
  rx_packets?: U64;
  tx_packets?: U64;
  rx_bytes?: U64;
  tx_bytes?: U64;
  /** The node at the other end. */
  peer?: NodeRef;
}

/** One node's tunnels (`GET /cluster/ovn/tunnels`, spec/16-clustering-ui.md §8b). */
export interface NodeTunnels {
  node: string;
  node_name?: string;
  answered?: boolean;
  error?: string;
  chassis?: string;
  controller_running?: boolean;
  tunnels?: Tunnel[];
  /** Other nodes it has no tunnel to. */
  missing?: NodeRef[];
}

/** A chassis in a network's OVN view (glidex.network.v1.Chassis): absent fields left out, `priority` an int64 string. */
export interface NetworkOvnChassis {
  chassis: string;
  node?: string;
  node_name?: string;
  priority?: U64;
}

/** `GET /networks/{id}/ovn` (proto JSON: empty lists, false and absent fields left out). */
export interface NetworkOvn {
  switch: string;
  ports?: { name: string; kind?: string; addresses?: string[]; up?: boolean; bound?: NetworkOvnChassis; vm?: { id: string; name: string } }[];
  router?: { name: string; nat?: string[]; active_gateway?: NetworkOvnChassis; gateway_candidates?: NetworkOvnChassis[] };
}

/** A detach (`GET /departures`, 14-clustering.md §5.8). */
export interface Departure {
  plan: string;
  /** The node's id. */
  node: string;
  node_name?: string;
  /** Read with `departureStateLabel`. */
  state?: DepartureState;
  /** RFC 3339. */
  started_at?: string;
  deadline?: string;
  map_networks?: Record<string, string>;
  with_access?: boolean;
  /** The node confirmed it has its bundle. */
  acknowledged?: boolean;
  /** Network id → name, as they were when the detach started. */
  network_names?: Record<string, string>;
}

/** A join or rejoin token, shown once (spec/16-clustering-ui.md §2.3). */
export interface IssuedToken {
  token: string;
  role?: NodeRole;
  /** uint64: read with `num`. */
  ttl_secs?: U64;
  servers?: string[];
  ca_fingerprint?: string;
  node?: string;
  node_id?: string;
  raft_intact?: boolean;
}

export type ImportSummaryKey = "vms" | "disks" | "images" | "networks" | "credentials" | "projects";

/** An import plan (glidex.cluster.v1.ImportPlan) as `GET /imports` and the live stream send it (14-clustering.md §5.9). */
export interface ImportPlanView {
  plan: string;
  /** Read with `importStateLabel`. */
  state?: ImportState;
  /** The joining host's name. */
  node?: string;
  node_id?: string;
  advertise?: string;
  /** A count of zero is left out. */
  summary?: Partial<Record<ImportSummaryKey, number>>;
  problems?: string[];
  mappings?: Partial<ImportMappings>;
  /** Only on `GET /imports/{plan}`. */
  sources?: ImportSources;
  created_by?: string;
  /** RFC 3339. */
  created_at?: string;
  expires_at?: string;
}

// ---- tenants, admissions and login connectors (spec/19-tenancy.md §14) ----------
// These are protobuf messages as JSON: snake_case names, enums by proto
// name, timestamps RFC 3339, and an optional field that is false or unset
// may be left out.

/** A tenant or admission cap; an absent field is unlimited. */
export type TenantQuotas = Quotas;

export type AdmissionStatus = "ADMISSION_STATUS_UNSPECIFIED" | "ADMISSION_STATUS_ADMITTED" | "ADMISSION_STATUS_SUSPENDED";

/** This cluster's admission of a tenant (D9, D19). */
export interface Admission {
  tenant_id: string;
  directory_id: string;
  cap?: TenantQuotas;
  status?: AdmissionStatus;
  by?: string;
  at?: string;
  /** The tenant's slug and name, for operators who can't read tenants. */
  tenant_slug?: string;
  tenant_display_name?: string;
}

export interface Tenant {
  id: string;
  directory_id: string;
  slug: string;
  display_name: string;
  owns_directory?: boolean;
  operator?: boolean;
  suspended?: boolean;
  created_at?: string;
  /** This cluster's admission of the tenant, if any. */
  admission?: Admission;
  /** The tenant operating this cluster: never suspended or unadmitted here. */
  operates_here?: boolean;
}

/** An explicit member of a tenant: a user or a team. */
export interface TenantMember {
  tenant_id: string;
  principal: EntityRef;
  by?: string;
  at?: string;
  principal_name?: string;
}

/** A role link on a tenant. */
export interface TenantBinding {
  id: string;
  role: string;
  principal: EntityRef;
  principal_name?: string;
  created_by?: string;
  created_at?: string;
}

export const TENANT_ROLES = ["role.tenant-admin", "role.tenant-auditor"] as const;

export interface PamSettings {
  allowed_groups?: string[];
}

export interface OidcSettings {
  issuer: string;
  client_id: string;
  scopes?: string[];
  groups_claim?: string;
  allowed_domains?: string[];
  redirect_uri?: string;
  mfa_acr_values?: string[];
  /** The IdP verifies its users' addresses (spec/19-tenancy.md D75). */
  trust_email?: boolean;
  /** With JIT, only these verified addresses get an account (D76). */
  allowed_emails?: string[];
}

/** `google` settings (spec/19-tenancy.md D39, §5.1). */
export interface GoogleSettings {
  /** Workspace by `hd`, or any account for invited and listed people. */
  accounts?: "GOOGLE_ACCOUNTS_WORKSPACE" | "GOOGLE_ACCOUNTS_ANY";
  client_id: string;
  domains?: string[];
  allowed_emails?: string[];
  device_client_id?: string;
  trust_2sv?: boolean;
  redirect_uri?: string;
}

/** `jumpcloud` settings (spec/19-tenancy.md §5.1). */
export interface JumpcloudSettings {
  client_id: string;
  /** The claim JumpCloud's "Include group attribute" names; default `memberOf`. */
  groups_claim?: string;
  allowed_emails?: string[];
  redirect_uri?: string;
}

/** Kinds that log in through the OIDC code: a button on the login page,
 *  and a way in for invitations (a verified email). */
export const isOidcKind = (k?: string) => k === "oidc" || k === "google" || k === "jumpcloud";

/** Whether the login page offers single sign-on: the server says so, or
 *  it lists a connector that logs in through the OIDC code. */
export const offersSso = (m: AuthMethods) => !!m.oidc || !!m.connectors?.some((c) => isOidcKind(c.kind));

/** Which of a connector's secrets are set; never their values. */
export interface SecretStatus {
  field: string;
  set?: boolean;
  updated_at?: string;
}

/** A login connector of the directory (spec/19-tenancy.md §5). Exactly one
 * of `pam` and `oidc` is set; `kind` says which. */
export interface Connector {
  id: string;
  directory_id?: string;
  kind: "pam" | "oidc" | "google" | "jumpcloud";
  display_name: string;
  enabled?: boolean;
  jit?: boolean;
  required_groups?: string[];
  require_mfa?: boolean;
  group_teams?: Record<string, string>;
  default_tenant?: string | null;
  token_max_days?: number | null;
  /** `scim`: the IdP provisions its users and groups (spec/19-tenancy.md §8.1). */
  provisioning?: "none" | "scim" | "pull";
  group_mode?: "mapped" | "auto";
  /** `pull`: what it reads and how often. */
  sync?: SyncSettings | null;
  /** The last pull. Output only. */
  sync_status?: SyncStatus | null;
  pam?: PamSettings;
  oidc?: OidcSettings;
  google?: GoogleSettings;
  jumpcloud?: JumpcloudSettings;
  secrets?: SecretStatus[];
  /** `api`, or `file` for the read-only ones from control-plane.json. */
  source?: "api" | "file";
  created_at?: string;
  updated_at?: string;
}

/** A pull's settings (glidex.directory.v1.SyncSettings). */
export interface SyncSettings {
  interval_minutes?: number;
  max_disable?: number;
  admin_email: string;
  groups: string[];
  ssh_keys_field?: string;
}

/** A pull's last run (glidex.directory.v1.SyncStatus). */
export interface SyncStatus {
  state?: "SYNC_STATE_OK" | "SYNC_STATE_FAILED" | "SYNC_STATE_BLOCKED";
  started_at?: string;
  finished_at?: string;
  changes?: { created?: number; adopted?: number; updated?: number; disabled?: number; enabled?: number };
  error?: string;
  would_disable?: string[];
  last_applied_at?: string;
}

/** A SCIM bearer token's record (glidex.directory.v1.ScimToken); never the token. */
export interface ScimToken {
  id: string;
  name: string;
  created_by: string;
  created_at?: string;
  expires_at?: string;
}

/** The settable part of a connector, as POST and PATCH take it. */
export type ConnectorInput = Omit<Connector, "directory_id" | "kind" | "secrets" | "source" | "created_at" | "updated_at">;

export interface ConnectorTestStep {
  name: string;
  ok?: boolean;
  message: string;
}

/** An RFC 3339 timestamp, for display. */
export function formatTimestamp(ts: string | undefined | null): string {
  if (!ts) return "—";
  const d = new Date(ts);
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleString();
}

// ---- host networking and PCI (glidex.host.v1) ------------------------------
//
// `/ovs/*` and `/pci-devices` answer in the proto3 JSON shape of
// glidex.host.v1: enums by full name, fields at their default left out,
// hugepage numbers as strings (read with `num`), times RFC 3339.

export type NetdAccess = "NETD_ACCESS_FULL" | "NETD_ACCESS_STATUS" | "NETD_ACCESS_NONE";

/** `NETD_ACCESS_FULL` → `full` (the label shown). */
export const netdAccessLabel = (a: NetdAccess | undefined) => enumLabel(a, "NETD_ACCESS_") || "none";

export interface Combination {
  id: string;
  description: string;
  available?: boolean;
  missing?: string[];
}

export interface Hugepages {
  /** uint64: read with `num`. */
  size_kb?: U64;
  total?: U64;
  free?: U64;
}

export interface HostCapabilities {
  ovs_installed?: boolean;
  ovs_version?: string;
  ovs_version_supported?: boolean;
  ovs_running?: boolean;
  iface_types?: string[];
  datapath_types?: string[];
  ovs_dpdk_build?: boolean;
  dpdk_mempool_driver?: boolean;
  dpdk_initialized?: boolean;
  hugepages?: Hugepages[];
  iommu?: boolean;
  ch_net_admin?: boolean;
  dnsmasq?: boolean;
  nft?: boolean;
  ip_forward?: boolean;
  firewalls?: string[];
  network_managers?: string[];
  combinations?: Combination[];
}

export interface OvsStatus {
  netd: { available?: boolean; access?: NetdAccess; error?: string };
  /** What netd found; absent when it didn't answer. */
  host?: HostCapabilities;
}

export type OvsProfile = "OVS_PROFILE_KERNEL" | "OVS_PROFILE_DPDK";

export interface InstallOvsResponse {
  changed?: boolean;
  method?: string;
  packages?: string[];
  ovs_version?: string;
  warnings?: string[];
  output_tail?: string;
}

export type Datapath = "DATAPATH_SYSTEM" | "DATAPATH_NETDEV";

/** `DATAPATH_SYSTEM` → `system` (the label shown). */
export const datapathLabel = (d: Datapath | undefined) => enumLabel(d, "DATAPATH_");

export interface BridgeRecord {
  spec: { name: string; datapath?: Datapath; mtu?: number; adopt?: boolean; isolated?: boolean };
  /** Absent when the bridge is missing on the host. */
  live?: { name: string; datapath?: Datapath; owned?: boolean; ports?: string[] };
}

export type XdpMode = "XDP_MODE_BEST_EFFORT" | "XDP_MODE_NATIVE" | "XDP_MODE_NATIVE_WITH_ZEROCOPY" | "XDP_MODE_GENERIC";

/** How an uplink attaches the NIC (09-networking.md §8). */
export type UplinkKind =
  | { kind: "UPLINK_KIND_KERNEL"; ifname: string }
  | { kind: "UPLINK_KIND_AFXDP"; ifname: string; xdp_mode?: XdpMode; n_rxq?: number }
  | { kind: "UPLINK_KIND_DPDK"; pci: string; n_rxq?: number };

/** `UPLINK_KIND_AFXDP` → `afxdp` (the label shown). */
export const uplinkKindLabel = (k: string | undefined) => enumLabel(k, "UPLINK_KIND_");

export type UplinkPhase = "UPLINK_PHASE_ACTIVE" | "UPLINK_PHASE_PENDING_COMMIT";

/** An uplink as netd keeps it (`GET /ovs/bridges/{b}/uplinks`). */
export interface UplinkResult {
  record: {
    spec: {
      name: string;
      bridge: string;
      kind?: UplinkKind["kind"];
      ifname?: string;
      pci?: string;
      xdp_mode?: XdpMode;
      n_rxq?: number;
      migrate_ip?: boolean;
    };
    orig_driver?: string;
    snapshot?: { ifname: string; mac?: string; addresses?: string[]; routes?: { dst: string; gateway?: string; metric?: number }[] };
    /** The host's address was moved; commit with `token` before `deadline` (RFC 3339). */
    pending?: { token: string; deadline?: string };
    /** Re-applying at start failed and was rolled back: the NIC has its own address again. `at` is RFC 3339. */
    boot_rollback?: { at?: string; reason: string };
    accept_dhcp?: boolean;
  };
  phase?: UplinkPhase;
  live?: { error?: string; link_state?: string; warnings?: string[] };
  classification?: { reasons?: string[]; addresses?: string[] };
}

/** A host PCI device (`GET /pci-devices`). */
export interface PciDevice {
  /** `0000:3b:00.0`. */
  address: string;
  vendor_id: string;
  device_id: string;
  class_id: string;
  driver?: string;
  iommu_group?: string;
  sysfs_path: string;
}

// ---- host topology (spec/22-host-topology.md §3) ----------------------------------

export interface TopologyNumaNode {
  id?: number;
  cores?: { threads?: number[] }[];
  memory_mib?: U64;
  /** Page size in KiB → pages. */
  hugepages?: Record<string, U64>;
}

export interface PciFunction {
  address: string;
  vendor_id?: string;
  device_id?: string;
  class_id?: string;
  driver?: string;
  iommu_group?: string;
  numa_node?: number;
  parent?: string;
  kind?: string;
  root_port?: string;
  switch?: string;
  link?: { current_speed?: string; current_width?: number; max_speed?: string; max_width?: number };
  sriov?: { total_vfs?: number; enabled_vfs?: number; vf_driver?: string; ready_vfs?: number };
  physfn?: string;
  assignable?: boolean;
  /** Part of the host's NVLink fabric (an NVSwitch): shown, never given to a VM (spec/27-gpu-vms.md §7). */
  fabric?: boolean;
  ready?: boolean;
  not_ready?: string[];
  /** Why a VM can't be given it now: a VM holds it or its IOMMU group, or it is cordoned. Unset: free. */
  taken?: string;
  aer?: { correctable?: U64; nonfatal?: U64; fatal?: U64 };
}

export interface HostTopology {
  summary?: {
    numa_nodes?: TopologyNumaNode[];
    isolated?: number[];
    reserved_cpus?: Record<string, string>;
    reserved_by_config?: boolean;
    device_numa?: Record<string, number>;
  };
  devices?: PciFunction[];
  warnings?: { kind?: string; detail?: string }[];
}

/** `0-3,8,10-11` for a set of CPUs. */
export function cpuList(cpus: number[]): string {
  const s = [...new Set(cpus)].sort((a, b) => a - b);
  const out: string[] = [];
  for (let i = 0; i < s.length; ) {
    let j = i;
    while (j + 1 < s.length && s[j + 1] === s[j] + 1) j++;
    out.push(i === j ? `${s[i]}` : `${s[i]}-${s[j]}`);
    i = j + 1;
  }
  return out.join(",");
}

/** A PCI class code's word (spec/22-host-topology.md §3.3). */
export function pciClassWord(cls = ""): string {
  const c = cls.replace(/^0x/, "");
  if (c.startsWith("03")) return "GPU";
  if (c.startsWith("0108")) return "NVMe";
  if (c.startsWith("02")) return "NIC";
  if (c.startsWith("12") || c.startsWith("0b40")) return "Accelerator";
  return "Other";
}
