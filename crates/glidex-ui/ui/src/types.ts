export type VmState = "created" | "running" | "paused" | "stopped";

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
  vcpu_count: number;
  mem_size_mib: number;
  hypervisor: HypervisorType;
  vfio_devices: string[];
  credential?: string;
  nics?: NicState[];
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
  | { kind: "url"; url: string; expected_sha256?: string | null };

export interface ImageInfo {
  id: string;
  name: string;
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
}

export interface CatalogItem {
  key: string;
  distro: string;
  release: string;
  arch: string;
  url: string;
  downloaded_image_id?: string | null;
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
  status: "ready" | "busy" | "missing";
  busy_op?: string;
  path: string;
  project?: string;
  created_at: number;
  partition_table?: { kind: "gpt" | "mbr"; partitions: PartitionInfo[]; free_tail_bytes: number };
  extend_root?: "grown" | "already_full" | "on_boot" | "skipped";
  warnings?: string[];
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
  }
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
  principal: { user?: string | null; name?: string | null; token?: string | null; method?: string; [k: string]: unknown };
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
