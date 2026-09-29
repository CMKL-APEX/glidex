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
}

export interface CreateVmRequest {
  name: string;
  vcpu_count: number;
  mem_size_mib: number;
  kernel_image_path: string;
  rootfs_path: string;
  kernel_args?: string;
  hypervisor?: HypervisorType;
  vfio_devices?: string[];
}

export interface ApiError {
  error: string;
  message: string;
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
