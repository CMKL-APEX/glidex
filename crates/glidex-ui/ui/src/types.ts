export type VmState = "created" | "running" | "paused" | "stopped";

export type HypervisorType = "cloudhypervisor" | "qemu";

export const HYPERVISOR_LABELS: Record<HypervisorType, string> = {
  cloudhypervisor: "Cloud Hypervisor",
  qemu: "QEMU",
};

export interface VmResponse {
  id: string;
  name: string;
  state: VmState;
  vcpu_count: number;
  mem_size_mib: number;
  console_socket_path: string;
  log_path: string;
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
  created_at: number;
  partition_table?: { kind: "gpt" | "mbr"; partitions: PartitionInfo[]; free_tail_bytes: number };
  extend_root?: "grown" | "already_full" | "on_boot" | "skipped";
  warnings?: string[];
}

export interface CreateDiskRequest {
  name: string;
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
  username: string;
  has_password: boolean;
  ssh_authorized_keys: string[];
  created_at: number;
  updated_at: number;
}

export interface CreateCredentialRequest {
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
  details?: { impact?: string; missing?: string[]; reasons?: string[]; min_size_bytes?: number };
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
