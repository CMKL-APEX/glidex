/** What a VM is doing, as the controller last saw it (spec/reconciliation.md §7.4). */
export type VmState =
  | "created"
  | "starting"
  | "running"
  | "paused"
  | "stopping"
  | "stopped"
  | "failed"
  | "unknown";

/** What a VM should be doing (`spec.power`). */
export type PowerState = "running" | "paused" | "stopped";

/** A status condition (`Ready`, `DisksReady`, `CrashLoopBackOff`, …). */
export interface Condition {
  kind: string;
  status: "True" | "False" | "Unknown";
  reason: string;
  message: string;
  last_transition_at: number;
}

export interface ExitRecord {
  at: number;
  instance_id: string;
  cause: string;
  code?: number;
  signal?: number;
  message?: string;
}

export type HypervisorType = "cloudhypervisor" | "qemu";

export const HYPERVISOR_LABELS: Record<HypervisorType, string> = {
  cloudhypervisor: "Cloud Hypervisor",
  qemu: "QEMU",
};

export interface VmResponse {
  id: string;
  name: string;
  /** Owning project id. */
  project: string;
  state: VmState;
  desired_state?: PowerState;
  generation?: number;
  observed_generation?: number;
  resource_version?: number;
  restart_required?: boolean;
  /** Deletion requested; the controller is tearing it down. */
  deleting?: boolean;
  restart_policy?: "on_failure" | "never";
  on_host_boot?: "resume" | "stop";
  stop_grace_secs?: number;
  conditions?: Condition[];
  last_exit?: ExitRecord;
  /** The node the VM is placed on (spec/clustering.md §9.1). */
  node?: string;
  vcpu_count: number;
  mem_size_mib: number;
  hypervisor: HypervisorType;
  vfio_devices: string[];
  credential?: string;
  nics?: NicState[];
  /** Firmware image id the VM boots through. */
  firmware?: string;
  root_disk?: string;
  data_disks?: string[];
  warnings?: string[];
}

export interface NicState {
  network: string;
  mac: string;
  port?: string;
  ipv4?: string;
}

export type NetworkMode = "nat" | "bridged" | "isolated";
export type PortType = "tap" | "vhost_user";

export interface Network {
  name: string;
  bridge: string;
  mode: NetworkMode;
  port_type: PortType;
  vlan?: number;
  mtu?: number;
  owns_bridge: boolean;
  created_at: number;
  /** Project networks belong to a project (spec/security.md §6.2). */
  project?: string;
  all_projects?: boolean;
  grants?: string[];
  /** Projects that accepted a share of this project network. */
  shares?: string[];
  share_offers?: { project: string; offered_by: string; offered_at: number; expires_at: number }[];
  /** What the network controller last saw in netd (spec/reconciliation.md §10.3). */
  phase?: "pending" | "ready" | "degraded" | "netd_unavailable";
  conditions?: Condition[];
  /** Deletion requested; the network controller is removing it (from netd, then the record). */
  deletion_requested_at?: number;
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
}

export interface Combination {
  id: string;
  description: string;
  available: boolean;
  missing: string[];
}

export interface HostCapabilities {
  ovs_installed: boolean;
  ovs_version?: string;
  ovs_running: boolean;
  iface_types: string[];
  dpdk_initialized: boolean;
  dnsmasq: boolean;
  nft: boolean;
  ip_forward: boolean;
  ch_net_admin?: boolean;
  firewalls: string[];
  combinations: Combination[];
}

export interface OvsStatus {
  netd: { available: boolean; access: "full" | "status" | "none"; error?: string };
  host?: HostCapabilities;
}

export interface BridgeRecord {
  spec: { name: string; datapath: "system" | "netdev" };
  live?: { ports: string[] } | null;
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
  hypervisor?: HypervisorType;
  vfio_devices?: string[];
  /** Desired power state; default `stopped` (spec/reconciliation.md §7.4). */
  power?: PowerState;
  /** After a crash: restart (default) or leave it stopped. */
  restart_policy?: "on_failure" | "never";
  /** After a host reboot, if it should be running: start it again (default) or not. */
  on_host_boot?: "resume" | "stop";
}

// ---- images and disks (spec/images.md) ------------------------------------

export type ImageStatus =
  | { state: "downloading"; received_bytes: number; total_bytes?: number | null }
  | { state: "verifying" }
  | { state: "ready" }
  | { state: "failed"; reason: string }
  | { state: "missing" };

export type ImageSource =
  | { kind: "catalog"; key: string; url: string; version: string }
  | { kind: "url"; url: string; expected_sha256?: string | null }
  | { kind: "firmware"; key: string; url: string; version: string };

/** `disk`: a cloud image disks are cloned from; `firmware`: UEFI firmware VMs boot through. */
export type ImageKind = "disk" | "firmware";

export interface ImageInfo {
  id: string;
  name: string;
  kind: ImageKind;
  /** The hypervisor a firmware image is built for. */
  hypervisor?: HypervisorType;
  source: ImageSource;
  status: ImageStatus;
  format: "qcow2" | "raw";
  virtual_size_bytes: number;
  file_size_bytes: number;
  sha256: string;
  arch: string;
  created_at: number;
  verified: boolean;
  path: string;
  linked_disks: string[];
  /** A firmware image carries a UEFI variable-store template (QEMU keeps a per-VM store; without one, `-bios`). */
  vars_template?: boolean;
  /** VMs booting through this firmware image. */
  used_by_vms?: string[];
  /** Deletion requested; the image controller is removing it. */
  deleting?: boolean;
}

export interface CatalogItem {
  key: string;
  distro: string;
  release: string;
  arch: string;
  url: string;
  downloaded_image_id?: string | null;
}

export interface FirmwareCatalogItem {
  key: string;
  name: string;
  hypervisor: HypervisorType;
  arch: string;
  /** `download`: a pinned build; `debian`: a pinned Debian package, unpacked; `host`: copied from the host's package. */
  source: "download" | "debian" | "host";
  url?: string;
  version?: string;
  /** Whether it can be pulled now (a host entry needs its package). */
  available: boolean;
  hint?: string;
  downloaded_image_id?: string | null;
}

/** Ready firmware images for `hypervisor`, newest first (the server's default). */
export function firmwareFor(images: ImageInfo[], hypervisor: HypervisorType): ImageInfo[] {
  return images
    .filter((i) => i.kind === "firmware" && i.hypervisor === hypervisor && i.status.state === "ready" && !i.deleting)
    .sort((a, b) => b.created_at - a.created_at);
}

export interface PartitionInfo {
  number: number;
  start_bytes: number;
  size_bytes: number;
  type: string;
  is_root: boolean;
}

export interface DiskInfo {
  id: string;
  name: string;
  format: "qcow2" | "raw";
  size_bytes: number;
  origin: { kind: "blank" } | { kind: "image"; image_id: string; mode: "linked" | "full" };
  attached_to?: string | null;
  pending_growpart: boolean;
  /** `pending` waits for its image; `failed` and the `Ready` condition say why. */
  status: "pending" | "creating" | "ready" | "resizing" | "busy" | "missing" | "failed";
  phase: "pending" | "creating" | "ready" | "resizing" | "missing" | "failed";
  conditions?: Condition[];
  /** A resize not applied yet: a running VM has the disk open. */
  pending_size_bytes?: number;
  /** The VM that owns it (created with the VM from an image). */
  owner?: string;
  deleting?: boolean;
  busy_op?: string;
  path: string;
  project?: string;
  created_at: number;
  partition_table?: { kind: "gpt" | "mbr"; partitions: PartitionInfo[]; free_tail_bytes: number };
  warnings?: string[];
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
  return !d.deleting && d.phase === "ready" && d.status === "ready" && d.pending_size_bytes !== undefined;
}

/** A deletion, a download or its verification in progress. */
export function imageActivity(i: ImageInfo): string | null {
  if (i.deleting) return "Deleting";
  if (i.status.state === "downloading") {
    const s = i.status;
    return s.total_bytes ? `Downloading ${Math.floor((s.received_bytes * 100) / s.total_bytes)}%` : "Downloading";
  }
  return i.status.state === "verifying" ? "Verifying" : null;
}

/** A network deletion in progress (it waits for netd, or for a VM to leave). */
export function networkActivity(n: Network): string | null {
  if (!n.deletion_requested_at) return null;
  const why = notReady(n.conditions);
  return why && why.reason !== "Converged" ? `Deleting (${why.reason})` : "Deleting";
}

/** The `Ready` condition when it isn't True: why an object hasn't converged. */
export function notReady(conditions?: Condition[]): Condition | undefined {
  return conditions?.find((c) => c.kind === "Ready" && c.status !== "True");
}

export interface CreateDiskRequest {
  name: string;
  project?: string;
  size_gib?: number;
  image?: string;
  clone?: "linked" | "full";
  format?: "qcow2" | "raw";
  extend_root?: boolean;
}

export function formatBytes(n: number): string {
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
  has_password: boolean;
  ssh_authorized_keys: string[];
  created_at: number;
  updated_at: number;
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
  const desired = vm.desired_state;
  if (!desired) return null;
  const there =
    desired === "stopped" ? vm.state === "stopped" || vm.state === "created" : vm.state === desired;
  if (!there) {
    if (desired === "stopped") return "Stopping";
    if (desired === "paused") return vm.state === "running" ? "Pausing" : "Starting";
    return vm.state === "paused" ? "Resuming" : "Starting";
  }
  if (vm.generation !== undefined && vm.observed_generation !== undefined && vm.observed_generation < vm.generation) {
    return "Applying changes";
  }
  const ready = vm.conditions?.find((c) => c.kind === "Ready");
  if (ready && ready.status !== "True") return "Reconciling";
  return null;
}

/** What keeps the VM from its desired state (the `Ready` condition). */
export function notReadyReason(vm: VmResponse): string | null {
  const ready = vm.conditions?.find((c) => c.kind === "Ready");
  if (!ready || ready.status === "True") return null;
  return ready.message ? `${ready.reason}: ${ready.message}` : ready.reason;
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
  return `${cause[e.cause] ?? e.cause}${detail ? ` (${detail})` : ""}, ${new Date(e.at * 1000).toLocaleString()}${
    e.message ? `: ${e.message}` : ""
  }`;
}

export function stateLabel(state: VmState): string {
  return state.charAt(0).toUpperCase() + state.slice(1);
}

// ---- access control (spec/security.md §5–§7, §10, §13) ----------------------

/** A Cedar entity reference, as the API serializes it. */
export type EntityType =
  | "Host"
  | "User"
  | "Team"
  | "Token"
  | "Project"
  | "Vm"
  | "Disk"
  | "Credential"
  | "Image"
  | "Network"
  | "PciDevice";

export type EntityRef = { type: "Host" } | { type: Exclude<EntityType, "Host">; id: string };

export function entityLabel(e: EntityRef): string {
  return e.type === "Host" ? "Host" : `${e.type}:${e.id}`;
}

export interface AuthMethods {
  pam: boolean;
  oidc: boolean;
  /** Authentication is off (development): everyone is the system user. */
  disabled: boolean;
}

export interface User {
  id: string;
  display_name: string;
  disabled: boolean;
  default_project?: string;
  created_at: number;
}

export interface Identity {
  provider: string;
  subject: string;
  user_id: string;
  email?: string;
  created_at: number;
}

export interface UserView extends User {
  identities: Identity[];
}

export interface WhoAmI {
  user: User | null;
  token: { id: string; name: string } | null;
  method: "peer" | "pam" | "oidc" | "token" | "disabled";
  teams: string[];
  break_glass: boolean;
  csrf: string | null;
  host_roles: string[];
  project_roles: { project: string; project_name?: string | null; role: string; via: EntityRef }[];
  default_project: string | null;
}

export interface Quotas {
  vms: number | null;
  vcpus: number | null;
  memory_mib: number | null;
  disk_gib: number | null;
  running_vms: number | null;
  networks: number | null;
}

export type Usage = { [K in keyof Quotas]: number };

export const QUOTA_KEYS: (keyof Quotas)[] = ["vms", "running_vms", "vcpus", "memory_mib", "disk_gib", "networks"];

export const QUOTA_LABELS: Record<keyof Quotas, string> = {
  vms: "VMs",
  running_vms: "Running VMs",
  vcpus: "vCPUs",
  memory_mib: "Memory (MiB)",
  disk_gib: "Disk (GiB)",
  networks: "Networks",
};

export interface Project {
  id: string;
  name: string;
  description: string;
  quotas: Quotas;
  created_at: number;
}

export interface ProjectView extends Project {
  usage: Usage;
}

export interface TeamMember {
  user_id: string;
  source: "manual" | "pam" | "oidc";
}

export interface Team {
  id: string;
  name: string;
  members: TeamMember[];
  created_at: number;
}

/** A role link (a Cedar template-linked policy). */
export interface Binding {
  id: string;
  template: string;
  principal: EntityRef;
  resource: EntityRef;
  created_by: string;
  created_at: number;
  /** The principal's display name (user), team or token name, for callers
   * who may read the link but not list users or teams. */
  principal_name?: string;
}

export const PROJECT_ROLES = ["role.viewer", "role.operator", "role.editor", "role.owner"] as const;
export const HOST_ROLES = [
  "role.auditor",
  "role.image-admin",
  "role.net-admin",
  "role.system-admin",
  "grant.host-paths",
] as const;

export function roleLabel(template: string): string {
  return template.replace(/^(role|grant)\./, "");
}

export interface ApiToken {
  id: string;
  name: string;
  kind: "personal" | "service_account";
  owner?: string;
  project?: string;
  created_by: string;
  created_at: number;
  expires_at: number;
  last_used_at?: number;
  last_used_from?: string;
  roles: Binding[];
}

export interface CreateTokenRequest {
  name: string;
  expires_in_days?: number;
  kind?: "personal" | "service_account";
  project?: string;
  roles?: { role: string; project?: string }[];
}

export interface CreatedToken {
  /** The secret: shown once, never again. */
  token: string;
  record: ApiToken;
}

export type PolicySource = "base" | "role" | "link" | "site" | "file";

export interface PolicyInfo {
  id: string;
  source: PolicySource;
  template: boolean;
  text: string;
}

export interface SitePolicy {
  id: string;
  text: string;
  description: string;
  enabled: boolean;
  version: number;
  updated_by: string;
  updated_at: number;
}

export interface SitePolicyVersion {
  id: string;
  version: number;
  text: string;
  enabled: boolean;
  author: string;
  time: number;
  deleted: boolean;
}

export interface Decision {
  allowed: boolean;
  policies: string[];
  errors?: string[];
}

export interface SimulationRequest {
  principal: EntityRef;
  action: string;
  resource: EntityRef;
}

export type SimulationResult = { current: Decision; candidate: Decision } | { error: string };

export interface PolicyChange {
  id: string;
  text?: string;
  description?: string;
  enabled?: boolean;
  delete?: boolean;
}

export interface AuditEntry {
  /** Unix milliseconds. */
  time: number;
  request_id: string;
  /** Null for unauthenticated requests; `{ system }` for the control plane's own writes. */
  principal: { user?: string | null; name?: string | null; token?: string | null; method?: string; system?: string; [k: string]: unknown } | null;
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
  network: string;
  owner_project: string | null;
  status: "offered" | "accepted";
  expires_at?: number;
}

/** Unix seconds → local date and time. */
export function formatTime(secs: number | undefined | null): string {
  if (!secs) return "—";
  return new Date(secs * 1000).toLocaleString();
}

// ---- usage (spec/metering.md §9) ---------------------------------------------

export interface Named {
  id: string;
  name: string;
}

export interface MeterValue {
  raw: number;
  value: number;
  unit: string;
}

export type GroupKey = "project" | "vm" | "disk" | "nic" | "network";

export interface UsageRow {
  start: string;
  end: string;
  project?: Named;
  vm?: Named;
  disk?: Named;
  nic?: Named;
  network?: Named;
  meters: Record<string, MeterValue>;
  flags?: string[];
}

export interface UsageResponse {
  from: string;
  to: string;
  granularity: string;
  timezone: string;
  group_by: GroupKey[];
  rows: UsageRow[];
  complete_through: string;
  metering_started_at: string;
}

export interface RateRow {
  project?: Named;
  vm?: Named;
  disk?: Named;
  nic?: Named;
  network?: Named;
  avg?: Record<string, number | null>;
  peak?: Record<string, number | null>;
  p95?: Record<string, number | null>;
  slots?: { counted: number; interpolated: number };
  latency_source?: "counter" | "none";
}

export interface RateResponse {
  month: string | null;
  from: string;
  to: string;
  timezone: string;
  final: boolean;
  from_final_figures: boolean;
  group_by: GroupKey[];
  rows: RateRow[];
}

/** One 5-minute point of a series; keys depend on the series. */
export type SeriesPoint = { slot: string } & Record<string, number | string>;

export interface SeriesResponse {
  from: string;
  to: string;
  points: SeriesPoint[];
  p95?: Record<string, number | null | { counted: number; interpolated: number }>;
  disks?: Record<string, SeriesResponse>;
}

export interface LiveEntry {
  id: string;
  name: string;
  network?: string | null;
  values: Record<string, number>;
}

export interface VmStats {
  sampled_at: string | null;
  resolution_secs: number;
  vm: Record<string, number> | null;
  nics: LiveEntry[];
  disks: LiveEntry[];
}
