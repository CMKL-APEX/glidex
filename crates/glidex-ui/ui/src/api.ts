import type {
  CatalogItem,
  CreateDiskRequest,
  DiskInfo,
  ImageInfo,
  BridgeRecord,
  CreateNetworkRequest,
  Network,
  OvsStatus,
  CreateCredentialRequest,
  CredentialInfo,
  UpdateCredentialRequest,
  CreateVmRequest,
  HealthResponse,
  VmResponse,
  ApiError,
} from "./types";

const API_BASE = "/api";

/** An API error that keeps the machine-readable code and details. */
export class ApiRequestError extends Error {
  code: string;
  details: ApiError["details"];
  constructor(err: ApiError) {
    super(`${err.error}: ${err.message}`);
    this.code = err.error;
    this.details = err.details;
  }
}

async function handleResponse<T>(resp: Response): Promise<T> {
  if (resp.ok) {
    return resp.json();
  }
  const err: ApiError = await resp.json();
  throw new ApiRequestError(err);
}

export async function healthCheck(): Promise<HealthResponse> {
  const resp = await fetch(`${API_BASE}/health`);
  return handleResponse(resp);
}

export async function listVms(): Promise<VmResponse[]> {
  const resp = await fetch(`${API_BASE}/vms`);
  return handleResponse(resp);
}

export async function getVm(id: string): Promise<VmResponse> {
  const resp = await fetch(`${API_BASE}/vms/${id}`);
  if (resp.status === 404) throw new Error("VM not found");
  return handleResponse(resp);
}

export async function createVm(
  request: CreateVmRequest,
): Promise<VmResponse> {
  const resp = await fetch(`${API_BASE}/vms`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(request),
  });
  return handleResponse(resp);
}

export async function startVm(id: string): Promise<VmResponse> {
  const resp = await fetch(`${API_BASE}/vms/${id}/start`, { method: "POST" });
  return handleResponse(resp);
}

/** Stop a VM. With `gracefulTimeoutSecs`, press the guest's power button
 * first and stop it hard only if it is still running after that long. */
export async function stopVm(id: string, gracefulTimeoutSecs?: number): Promise<VmResponse> {
  const query = gracefulTimeoutSecs === undefined ? "" : `?graceful_timeout_secs=${gracefulTimeoutSecs}`;
  const resp = await fetch(`${API_BASE}/vms/${id}/stop${query}`, { method: "POST" });
  return handleResponse(resp);
}

export async function pauseVm(id: string): Promise<VmResponse> {
  const resp = await fetch(`${API_BASE}/vms/${id}/pause`, { method: "POST" });
  return handleResponse(resp);
}

export async function deleteVm(id: string, keepDisk = false): Promise<void> {
  const resp = await fetch(`${API_BASE}/vms/${id}${keepDisk ? "?keep_disk=true" : ""}`, {
    method: "DELETE",
  });
  if (resp.ok || resp.status === 204) return;
  const err: ApiError = await resp.json();
  throw new Error(`${err.error}: ${err.message}`);
}

export async function listCredentials(): Promise<CredentialInfo[]> {
  const resp = await fetch(`${API_BASE}/credentials`);
  return handleResponse(resp);
}

export async function createCredential(
  request: CreateCredentialRequest,
): Promise<CredentialInfo> {
  const resp = await fetch(`${API_BASE}/credentials`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(request),
  });
  return handleResponse(resp);
}

export async function updateCredential(
  username: string,
  request: UpdateCredentialRequest,
): Promise<CredentialInfo> {
  const resp = await fetch(
    `${API_BASE}/credentials/${encodeURIComponent(username)}`,
    {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(request),
    },
  );
  return handleResponse(resp);
}

export async function deleteCredential(username: string): Promise<void> {
  const resp = await fetch(
    `${API_BASE}/credentials/${encodeURIComponent(username)}`,
    { method: "DELETE" },
  );
  if (resp.ok || resp.status === 204) return;
  const err: ApiError = await resp.json();
  throw new Error(`${err.error}: ${err.message}`);
}

export async function listNetworks(): Promise<Network[]> {
  return handleResponse(await fetch(`${API_BASE}/networks`));
}

export async function createNetwork(req: CreateNetworkRequest): Promise<Network> {
  const resp = await fetch(`${API_BASE}/networks`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
  return handleResponse(resp);
}

export async function deleteNetwork(name: string): Promise<void> {
  const resp = await fetch(`${API_BASE}/networks/${encodeURIComponent(name)}`, {
    method: "DELETE",
  });
  if (resp.ok || resp.status === 204) return;
  throw new ApiRequestError(await resp.json());
}

export async function ovsStatus(): Promise<OvsStatus> {
  return handleResponse(await fetch(`${API_BASE}/ovs/status`));
}

export async function installOvs(
  profile: "kernel" | "dpdk",
  confirm: boolean,
): Promise<{ changed: boolean; ovs_version?: string; warnings: string[] }> {
  const resp = await fetch(`${API_BASE}/ovs/install`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ profile, source_build: false, confirm }),
  });
  return handleResponse(resp);
}

export async function listBridges(): Promise<BridgeRecord[]> {
  return handleResponse(await fetch(`${API_BASE}/ovs/bridges`));
}

// ---- images and disks ---------------------------------------------------------

async function sendJson<T>(method: string, path: string, body?: unknown): Promise<T> {
  const resp = await fetch(`${API_BASE}${path}`, {
    method,
    headers: body === undefined ? undefined : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (resp.status === 204) return undefined as T;
  return handleResponse(resp);
}

export const imageCatalog = () => sendJson<CatalogItem[]>("GET", "/images/catalog");
export const listImages = () => sendJson<ImageInfo[]>("GET", "/images");
export const pullImage = (req: { catalog?: string; url?: string; sha256?: string; name?: string }) =>
  sendJson<ImageInfo>("POST", "/images", req);
export const deleteImage = (id: string) => sendJson<void>("DELETE", `/images/${encodeURIComponent(id)}`);

export const listDisks = () => sendJson<DiskInfo[]>("GET", "/disks");
export const getDisk = (id: string) => sendJson<DiskInfo>("GET", `/disks/${encodeURIComponent(id)}`);
export const createDisk = (req: CreateDiskRequest) => sendJson<DiskInfo>("POST", "/disks", req);
export const resizeDisk = (id: string, sizeGib: number, extendRoot?: boolean) =>
  sendJson<DiskInfo>("POST", `/disks/${encodeURIComponent(id)}/resize`, {
    size_gib: sizeGib,
    extend_root: extendRoot,
  });
export const extendRoot = (id: string, mode: "offline" | "on-boot") =>
  sendJson<DiskInfo>("POST", `/disks/${encodeURIComponent(id)}/extend-root`, { mode });
export const deleteDisk = (id: string) => sendJson<void>("DELETE", `/disks/${encodeURIComponent(id)}`);
