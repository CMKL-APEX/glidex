// ---- VMs (glidex.vm.v1, spec/grpc.md §9.1) ---------------------------------
//
// The VM routes answer in the proto3 JSON shape (see the storage section
// below for the rules): enums by their full names, 64-bit counters as
// strings, timestamps RFC 3339, defaults left out. The helpers here read
// them back in the short forms the pages show (`VM_STATE_RUNNING` → `running`).

/** What a VM is doing, as the controller last saw it (spec/reconciliation.md §7.4), for display. */
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

export type HypervisorType = "cloudhypervisor" | "qemu";

export const HYPERVISOR_LABELS: Record<HypervisorType, string> = {
  cloudhypervisor: "Cloud Hypervisor",
  qemu: "QEMU",
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
  /** The node the VM is placed on (spec/clustering.md §9.1). */
  node?: string;
  /** That node's name (spec/clustering-ui.md §9). */
  node_name?: string;
  vcpu_count: number;
  mem_size_mib: number;
  /** Read with `vmHypervisor`. */
  hypervisor?: ProtoHypervisor;
  vfio_devices?: string[];
  credential?: string;
  nics?: NicState[];
  /** Firmware image id the VM boots through. */
  firmware?: string;
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
// Networks are keyed by id (spec/tenancy.md D24); enums are their full proto
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
  /** `cluster`: an OVN network every node can use; `node`: an OVS bridge on `node` (spec/clustering.md §11). */
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
  /** Project networks belong to a project (spec/security.md §6.2). */
  project?: string;
  all_projects?: boolean;
  grants?: string[];
  tenant_grants?: string[];
  /** Projects that accepted a share of this project network. */
  shares?: string[];
  share_offers?: ShareOffer[];
  /** What the network controller last saw in netd (spec/reconciliation.md §10.3). */
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

/** A project's VPC router (spec/clustering.md §11.2a). */
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
  mem_size_mib: number;
  kernel_image_path: string;
  /** Firmware image id/name (spec/images.md §7); default: the newest for the hypervisor. */
  firmware?: string;
  /** Host path of UEFI firmware (admins only); prefer `firmware`. */
  firmware_path?: string;
  credential?: string;
  networks?: { network: string }[];
  rootfs_path?: string;
  /** Image id/name: glidex creates and owns a root disk from it. */
  image?: string;
  root_disk_size_gib?: number;
  /** Existing, unattached disk to boot from. */
  root_disk?: string;
  data_disks?: string[];
  kernel_args?: string;
  hypervisor?: ProtoHypervisor;
  vfio_devices?: string[];
  /** Desired power state; default `stopped` (spec/reconciliation.md §7.4). */
  power?: ProtoPowerState;
  /** After a crash: restart (default) or leave it stopped. */
  restart_policy?: RestartPolicy;
  /** After a host reboot, if it should be running: start it again (default) or not. */
  on_host_boot?: HostBootPolicy;
  /** Run on this node (id or name); default: the scheduler picks (spec/clustering.md §9.1). */
  node?: string;
}

/** One node's answer to `POST /scheduler/preview` (spec/clustering-ui.md §5.1). */
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

// ---- images and disks (spec/images.md) ------------------------------------
//
// The storage routes (/credentials, /images, /disks) answer in the proto3
// JSON shape of glidex.storage.v1 (spec/grpc.md §9.1): 64-bit numbers are
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

export type ProtoHypervisor = "HYPERVISOR_CLOUD_HYPERVISOR" | "HYPERVISOR_QEMU";

export const HYPERVISOR_FROM_PROTO: Record<ProtoHypervisor, HypervisorType> = {
  HYPERVISOR_CLOUD_HYPERVISOR: "cloudhypervisor",
  HYPERVISOR_QEMU: "qemu",
};

export const HYPERVISOR_TO_PROTO: Record<HypervisorType, ProtoHypervisor> = {
  cloudhypervisor: "HYPERVISOR_CLOUD_HYPERVISOR",
  qemu: "HYPERVISOR_QEMU",
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
}

/** `IMAGE_KIND_DISK`: a cloud image disks are cloned from; `IMAGE_KIND_FIRMWARE`: UEFI firmware VMs boot through. */
export type ImageKind = "IMAGE_KIND_DISK" | "IMAGE_KIND_FIRMWARE";

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
  /** VMs booting through this firmware image. */
  used_by_vms?: string[];
  /** Each node's copy (spec/clustering.md §9.2.2, spec/clustering-ui.md §5.3). */
  copies?: ImageCopy[];
  info?: ImgInfo;
  /** Deletion requested; the image controller is removing it. */
  deleting?: boolean;
}

/** An image's state without its prefix: `ready`, `downloading`, … (`""` when unset). */
export const imageState = (i: ImageInfo) => enumLabel(i.status?.state, "IMAGE_STATE_");

export const isFirmware = (i: ImageInfo) => i.kind === "IMAGE_KIND_FIRMWARE";

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
  /** In a cluster (spec/clustering.md §9.2.1–2): the architecture, nodes
   * that get a copy, and whether the server keeps its own. */
  arch?: Arch;
  nodes?: string[];
  keep_server_copy?: boolean;
}

export interface PullImageResponse {
  image?: ImageInfo;
  /** A new download (false: one was already in flight). */
  created?: boolean;
}

/** Ready firmware images for `hypervisor`, newest first (the server's default). */
export function firmwareFor(images: ImageInfo[], hypervisor: HypervisorType): ImageInfo[] {
  return images
    .filter((i) => isFirmware(i) && hypervisorOf(i.hypervisor) === hypervisor && imageState(i) === "ready" && !i.deleting)
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
  /** The node holding the disk's file (spec/clustering.md §9.2). */
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

/** A deletion, a download or its verification in progress. */
export function imageActivity(i: ImageInfo): string | null {
  if (i.deleting) return "Deleting";
  const state = imageState(i);
  if (state === "downloading") {
    const total = num(i.status?.total_bytes);
    return total ? `Downloading ${Math.floor((num(i.status?.received_bytes) * 100) / total)}%` : "Downloading";
  }
  return state === "verifying" ? "Verifying" : null;
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

export interface ApiError {
  error: string;
  message: string;
  details?: {
    impact?: string;
    missing?: string[];
    reasons?: string[];
    min_size_bytes?: number;
    /** 422 invalid_policy: the validator's messages. */
    errors?: string[];
  };
}

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

/** What the controller is still doing for a VM (spec/reconciliation.md
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
/** The VM's node stopped reporting (spec/clustering.md §7.2): its last
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

// ---- access control (spec/security.md §5–§7, §10, §13) ----------------------

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
  kind: "pam" | "oidc";
  display_name: string;
}

export interface AuthMethods {
  pam: boolean;
  oidc: boolean;
  /** Authentication is off (development): everyone is the system user. */
  disabled: boolean;
  /** The directory's enabled connectors (absent from older servers). */
  connectors?: LoginConnector[];
}

// The access routes (projects, bindings, users, teams, tokens, policies,
// audit, capability checks, whoami) answer in the proto3 JSON shape of
// glidex.access.v1 (spec/grpc.md §9.1): timestamps RFC 3339, enums by their
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
 * and those with an external address, are spec/clustering.md §11.2a. */
export type Quotas = Partial<Record<QuotaKey, number>>;

/** What a project uses; a count of zero is left out. */
export type Usage = Partial<Record<QuotaKey, number>>;

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
  /** The tenant owning it (spec/tenancy.md D1). */
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

// ---- usage (spec/metering.md §9; glidex.usage.v1, proto JSON shape) ---------
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

// ---- nodes (spec/clustering.md §7) -------------------------------------------

export type NodePhase = "Active" | "Draining" | "Departing" | "Departed" | "Removed" | "Forgotten";
export type Tristate = "True" | "False" | "Unknown";

export interface NodeResources {
  cpus: number;
  memory_mib: number;
  hugepages?: Record<string, number>;
}

/** A node record as `GET /nodes` and the live stream send it. */
export interface NodeRecord {
  meta: { id: string; name: string; created_at: number; generation: number; resource_version: number };
  spec: {
    name: string;
    role: "server" | "agent";
    unschedulable: boolean;
    labels: Record<string, string>;
    /** `auto` (absent): `netdev` where DPDK runs, else `system` (spec/clustering.md §11.6). */
    br_int_datapath?: BrIntDatapath;
    /** physnet → bridge set through the API (the host's file may add more). */
    bridge_mappings?: Record<string, string>;
  };
  status: {
    phase: NodePhase;
    ready: Tristate;
    ready_reason?: string;
    raft_id?: number;
    advertise?: string;
    tunnel_ip?: string;
    versions: { glidex: string; ovs?: string; ovn?: string };
    capacity: NodeResources;
    allocatable: NodeResources;
    features?: {
      kvm: boolean;
      hypervisors: string[];
      dpdk?: boolean;
      iommu?: boolean;
      br_int_datapath?: string;
      physnets?: Record<string, string>;
      /** `x86_64` or `aarch64` (spec/clustering.md §9.2.1). */
      arch?: string;
      /** Files in its image directory that no image record names (§9.2.3). */
      spare_image_files?: { name: string; bytes: number; mtime: number }[];
    };
    pci_devices?: string[];
    heartbeat?: number;
    departed_ids?: string[];
  };
}

export type BrIntDatapath = "auto" | "system" | "netdev";

/** What a node's `br-int` datapath should be, by its setting. */
export function wantedBrInt(n: NodeRecord): "system" | "netdev" {
  const s = n.spec.br_int_datapath ?? "auto";
  if (s === "system" || s === "netdev") return s;
  return n.status.features?.dpdk ? "netdev" : "system";
}

/** A node is gone for good (kept as a record only). */
export function isTombstone(phase: NodePhase): boolean {
  return phase === "Departed" || phase === "Removed" || phase === "Forgotten";
}

/** A node's name, with its short id when it is a tombstone: a node that left
 * keeps its name, and a host that joins again takes the same one. */
export function nodeLabel(n: NodeRecord): string {
  return isTombstone(n.status.phase) ? `${n.spec.name} · ${n.meta.id.slice(0, 8)}` : n.spec.name;
}

/** What one import plan brings (`GET /imports/{plan}`), for the mapping editor. */
export interface ImportSources {
  projects: { id: string; name: string }[];
  networks: { name: string; mode: string }[];
  credentials: { key: string; project: string; username: string }[];
  vms: { id: string; name: string; project: string }[];
}

export interface ImportMappings {
  projects: Record<string, string>;
  networks: Record<string, string>;
  credentials: Record<string, string>;
  over_quota: boolean;
}

/** A chassis or node reference in OVN views. */
export interface ChassisRef {
  chassis: string;
  node: string | null;
  node_name: string | null;
  priority?: number;
}

/** The shared edge of NAT cluster networks (spec/clustering.md §11.2). */
export interface OvnEdge {
  physnet: string;
  external_cidr: string;
  gateway: string;
  external_ip: string;
  external_pool?: string | null;
  gateway_nodes: string[];
}

/** The cluster's OVN setting (spec/clustering-ui.md §8a). */
export interface OvnSettings {
  enabled: boolean;
  underlay_mtu?: number | null;
  nat_supernet?: string | null;
  edge?: OvnEdge | null;
  snat_ct_zones?: [number, number] | null;
  dns_servers?: string[];
  updated_by?: string;
  updated_at?: number;
}

/** `GET /cluster/settings/ovn`: `enabled` for everyone, the rest with `readCluster`. */
export interface OvnSettingsView {
  enabled: boolean;
  /** The shared edge for NAT networks spanning the nodes is set. */
  edge?: boolean;
  /** The edge has a pool of addresses for VPC routers that reach outside. */
  vpc_pool?: boolean;
  clustered?: boolean;
  /** `cluster`: the setting; `file`: control-plane.json (not moved yet); `none`. */
  source?: "cluster" | "file" | "none";
  settings?: OvnSettings;
  /** `outdated`: its ovn-controller is older than the cluster's ovn-northd, so it drops traffic of the newer pipeline. */
  nodes?: { id: string; name: string; role: string; ovn_version: string | null; ovn_central: boolean; missing: string | null; outdated?: string | null }[];
  cluster_networks?: string[];
}

/** `GET /cluster/ovn` (spec/clustering-ui.md §8). */
export interface OvnOverview {
  enabled: boolean;
  nb?: { address: string; reachable: boolean; leader: boolean }[];
  sb?: { address: string; reachable: boolean; leader: boolean }[];
  chassis?: (ChassisRef & { hostname: string; encaps: string[]; nb_cfg_timestamp: number | null })[];
  gateways?: { router: string; active: ChassisRef | null; candidates: ChassisRef[] }[];
}

/** One node's tunnels (`GET /cluster/ovn/tunnels`, spec/clustering-ui.md §8b). */
export interface NodeTunnels {
  node: string;
  node_name: string;
  answered: boolean;
  error?: string;
  chassis?: string | null;
  controller_running?: boolean;
  tunnels?: {
    name: string;
    remote_ip: string;
    /** `present`, `error` (OVS couldn't set it up) or `bfd_down`. */
    state: "present" | "error" | "bfd_down";
    error?: string;
    bfd_state?: string;
    bfd_diagnostic?: string;
    rx_packets: number;
    tx_packets: number;
    rx_bytes: number;
    tx_bytes: number;
    peer: { node: string; node_name: string } | null;
  }[];
  /** Other nodes it has no tunnel to. */
  missing?: { node: string; node_name: string }[];
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

/** A detach (`GET /departures`, clustering.md §5.8). */
export interface Departure {
  plan: string;
  node: string;
  node_name?: string;
  /** `frozen`, `committed` or `aborted`. */
  state: string;
  started_at: number;
  deadline: number;
  map_networks: Record<string, string>;
  with_access: boolean;
  acknowledged: boolean;
}

/** A join or rejoin token, shown once (spec/clustering-ui.md §2.3). */
export interface IssuedToken {
  token: string;
  role?: string;
  ttl_secs: number;
  servers?: string[];
  ca_fingerprint?: string;
  node?: string;
  node_id?: string;
}

/** An import plan as `GET /imports` and the live stream send it (clustering.md §5.9). */
export interface ImportPlanView {
  plan: string;
  state: string;
  node: string;
  node_id: string;
  advertise: string;
  summary: { vms: number; disks: number; images: number; networks: number; credentials: number; projects: number };
  problems: string[];
  mappings: ImportMappings;
  /** Only on `GET /imports/{plan}`. */
  sources?: ImportSources;
  created_by: string;
  created_at: number;
  expires_at: number;
}

// ---- tenants, admissions and login connectors (spec/tenancy.md §14) ----------
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
}

/** Which of a connector's secrets are set; never their values. */
export interface SecretStatus {
  field: string;
  set?: boolean;
  updated_at?: string;
}

/** A login connector of the directory (spec/tenancy.md §5). Exactly one
 * of `pam` and `oidc` is set; `kind` says which. */
export interface Connector {
  id: string;
  directory_id?: string;
  kind: "pam" | "oidc";
  display_name: string;
  enabled?: boolean;
  jit?: boolean;
  required_groups?: string[];
  require_mfa?: boolean;
  group_teams?: Record<string, string>;
  default_tenant?: string | null;
  token_max_days?: number | null;
  pam?: PamSettings;
  oidc?: OidcSettings;
  secrets?: SecretStatus[];
  /** `api`, or `file` for the read-only ones from control-plane.json. */
  source?: "api" | "file";
  created_at?: string;
  updated_at?: string;
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

/** How an uplink attaches the NIC (networking.md §8). */
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
