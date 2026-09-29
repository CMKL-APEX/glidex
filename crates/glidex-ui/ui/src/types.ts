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
  rootfs_path: string;
  kernel_args?: string;
  hypervisor?: HypervisorType;
  vfio_devices?: string[];
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
  details?: { impact?: string; missing?: string[]; reasons?: string[] };
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
