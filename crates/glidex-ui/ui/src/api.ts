import type {
  ApiToken,
  AuditEntry,
  AuthMethods,
  Binding,
  CatalogItem,
  FirmwareCatalogItem,
  CreateDiskRequest,
  CreatedToken,
  CreateTokenRequest,
  DiskInfo,
  DiskWriteResponse,
  ExtendMode,
  PullImageRequest,
  PullImageResponse,
  StorageEvent,
  EntityRef,
  ImageInfo,
  BridgeRecord,
  CreateNetworkRequest,
  Network,
  Router,
  RouterSpec,
  NetworkShare,
  OvsStatus,
  CreateCredentialRequest,
  CredentialInfo,
  PolicyChange,
  PolicyInfo,
  Project,
  ProjectView,
  Quotas,
  SimulationRequest,
  SimulationResult,
  SitePolicy,
  SitePolicyVersion,
  Team,
  UpdateCredentialRequest,
  CreateVmRequest,
  VmWriteResponse,
  HealthResponse,
  User,
  UserView,
  VmResponse,
  WhoAmI,
  ApiError,
  RateResponse,
  SeriesResponse,
  UsageResponse,
  VmStats,
  NodeRecord,
  PlacementPreview,
  ProtoHypervisor,
  Departure,
  ImportMappings,
  ImportPlanView,
  IssuedToken,
  NetworkOvn,
  OvnOverview,
  OvnSettings,
  OvnSettingsView,
  OvnEdge,
  NodeTunnels,
  BrIntDatapath,
  UplinkKind,
  UplinkResult,
  Admission,
  Connector,
  ConnectorInput,
  ConnectorTestStep,
  Tenant,
  TenantBinding,
  TenantMember,
  TenantQuotas,
} from "./types";

const API_BASE = "/api";

/** Dispatched on `window` after every successful write. */
export const CHANGED_EVENT = "glidex:changed";

/** An API error that keeps the HTTP status, the machine-readable code and details. */
export class ApiRequestError extends Error {
  status: number;
  code: string;
  details: ApiError["details"];
  constructor(status: number, err: ApiError) {
    super(`${err.error}: ${err.message}`);
    this.status = status;
    this.code = err.error;
    this.details = err.details;
  }
}

// ---- session plumbing (spec/security.md §5.5, §5.6) -------------------------

/** The session's CSRF value, from `/auth/whoami` or a login response. */
let csrf: string | null = null;

export function setCsrf(value: string | null | undefined) {
  csrf = value ?? null;
}

/** Called when the session is gone (401 unauthenticated). */
let onUnauthenticated: () => void = () => {};
/** Asks the user to log in again (401 reauth_required); resolves true when they did. */
let reauthenticate: () => Promise<boolean> = async () => false;

export function setSessionHandlers(h: { unauthenticated: () => void; reauth: () => Promise<boolean> }) {
  onUnauthenticated = h.unauthenticated;
  reauthenticate = h.reauth;
}

async function errorOf(resp: Response): Promise<ApiRequestError> {
  const text = await resp.text();
  try {
    const body = JSON.parse(text) as ApiError;
    if (body && typeof body.error === "string") return new ApiRequestError(resp.status, body);
  } catch {
    /* not JSON (a proxy error, say) */
  }
  return new ApiRequestError(resp.status, {
    error: `http_${resp.status}`,
    message: text.trim() || resp.statusText || "request failed",
  });
}

interface RequestOptions {
  /** Don't treat a 401 as the end of the session (whoami, login). */
  quiet401?: boolean;
  /** Safe to send again after `503 leader_changed` (GET, PUT and DELETE are). */
  idempotent?: boolean;
}

// ---- cluster availability (spec/clustering-ui.md §3.3) ------------------------

/** Dispatched on `window` when the cluster's availability as seen by the API
 * changes: `detail.available` false after `503 cluster_unavailable` (no
 * leader), true again on the next answer that isn't. `detail.stale` marks a
 * read answered from this server's own copy (`X-Glidex-Consistency: local`). */
export const CLUSTER_EVENT = "glidex:cluster";

export interface ClusterAvailability {
  available: boolean;
  stale: boolean;
}

let availability: ClusterAvailability = { available: true, stale: false };

export function clusterAvailability(): ClusterAvailability {
  return availability;
}

function setAvailability(next: ClusterAvailability) {
  if (next.available === availability.available && next.stale === availability.stale) return;
  availability = next;
  window.dispatchEvent(new CustomEvent<ClusterAvailability>(CLUSTER_EVENT, { detail: next }));
}

/** Backoff before each retry after `503 leader_changed`. */
const LEADER_RETRY_MS = [250, 500, 1000];

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** One API call: same-origin cookies, the CSRF header on writes, and the
 * 401 handling. A step-up (`reauth_required`) asks the user to log in
 * again, then retries once. */
async function request<T>(method: string, path: string, body?: unknown, opts: RequestOptions = {}): Promise<T> {
  const read = method === "GET" || method === "HEAD";
  const idempotent = opts.idempotent ?? (read || method === "PUT" || method === "DELETE");
  const send = (local = false) => {
    const headers: Record<string, string> = {};
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (!read && csrf) headers["X-Glidex-CSRF"] = csrf;
    if (local) headers["X-Glidex-Consistency"] = "local";
    return fetch(`${API_BASE}${path}`, {
      method,
      headers,
      credentials: "same-origin",
      body: body === undefined ? undefined : JSON.stringify(body),
    });
  };
  let resp = await send();
  // A leader election: the request reached no leader. Safe ones go again.
  for (let i = 0; resp.status === 503 && idempotent && i < LEADER_RETRY_MS.length; i++) {
    const code = await resp.clone().json().then((b: ApiError) => b.error).catch(() => "");
    if (code !== "leader_changed") break;
    await sleep(LEADER_RETRY_MS[i]);
    resp = await send();
  }
  let stale = false;
  if (resp.status === 503) {
    const code = await resp.clone().json().then((b: ApiError) => b.error).catch(() => "");
    if (code === "cluster_unavailable") {
      setAvailability({ available: false, stale: availability.stale });
      // No quorum: a read can still come from this server's own copy.
      if (read) {
        const local = await send(true);
        if (local.ok) {
          resp = local;
          stale = true;
        }
      }
    }
  }
  // A local read means there is still no leader. A read that went through
  // the cluster (it carries the revision) or a write that succeeded means
  // one is back; host-local reads succeed either way, so they don't count.
  if (resp.ok) {
    if (stale) setAvailability({ available: false, stale: true });
    // A POST that only asks (capability checks) is a read here.
    else if ((!read && !opts.idempotent) || resp.headers.has("x-glidex-revision")) setAvailability({ available: true, stale: false });
  }
  if (resp.status === 401) {
    let err = await errorOf(resp);
    if (err.code === "reauth_required" && (await reauthenticate())) {
      resp = await send();
      if (resp.status === 401) err = await errorOf(resp);
    }
    if (resp.status === 401) {
      if (err.code === "unauthenticated" && !opts.quiet401) onUnauthenticated();
      throw err;
    }
  }
  if (!resp.ok) throw await errorOf(resp);
  // A write may have started work for the controllers: let the activity
  // indicator look again now rather than at its next poll.
  if (method !== "GET" && method !== "HEAD" && /^\/(vms|disks|images|networks)\b/.test(path) && !path.includes("/console/")) {
    window.dispatchEvent(new Event(CHANGED_EVENT));
  }
  if (resp.status === 204) return undefined as T;
  const text = await resp.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

const get = <T>(path: string) => request<T>("GET", path);
const post = <T>(path: string, body?: unknown) => request<T>("POST", path, body ?? {});
/** A POST that only reads (capability checks): retried after a leader change. */
const query = <T>(path: string, body: unknown) => request<T>("POST", path, body, { idempotent: true });
const put = <T>(path: string, body?: unknown) => request<T>("PUT", path, body ?? {});
const patch = <T>(path: string, body: unknown) => request<T>("PATCH", path, body);
const del = <T = void>(path: string) => request<T>("DELETE", path);

const enc = encodeURIComponent;

/** `?project=<id>` when a project is selected. */
function inProject(project?: string | null): string {
  return project ? `?project=${enc(project)}` : "";
}

// ---- authentication -----------------------------------------------------------

export async function healthCheck(): Promise<HealthResponse> {
  return request("GET", "/health", undefined, { quiet401: true });
}

export const authMethods = () => request<AuthMethods>("GET", "/auth/methods", undefined, { quiet401: true });

export async function whoami(): Promise<WhoAmI> {
  const w = await request<WhoAmI>("GET", "/auth/whoami", undefined, { quiet401: true });
  setCsrf(w.csrf);
  return w;
}

export async function loginPam(username: string, password: string): Promise<WhoAmI> {
  const w = await request<WhoAmI>("POST", "/auth/login", { method: "pam", username, password }, { quiet401: true });
  setCsrf(w.csrf);
  return w;
}

/** Leave the page for the identity provider: connector `connector`, or
 *  the first OIDC connector. */
export function startOidc(returnTo: string, reauth = false, connector?: string) {
  const q = new URLSearchParams({ return_to: returnTo });
  if (reauth) q.set("reauth", "1");
  const path = connector ? `/auth/c/${encodeURIComponent(connector)}/start` : "/auth/oidc/start";
  window.location.assign(`${API_BASE}${path}?${q}`);
}

export async function logout(): Promise<void> {
  await request<void>("POST", "/auth/logout", {}, { quiet401: true });
  setCsrf(null);
}

/** Set (or, with `""`, clear) the caller's default project. */
export const updateMe = (body: { default_project: string }) => patch<{ user?: User }>("/users/me", body).then((r) => r?.user);

/** Capability checks for the caller (spec §7.7): one boolean per check.
 * An action only a fresh login would allow (`reauth`, a step-up action
 * after the step-up window) counts as allowed: the request then asks for
 * the login (ReauthDialog) and is retried. */
export async function checkAccess(checks: { action: string; resource: EntityRef }[]): Promise<boolean[]> {
  if (checks.length === 0) return [];
  const r = await query<{ results?: { allowed?: boolean; reauth?: boolean }[] }>("/authz/check", { checks });
  return checks.map((_, i) => !!r?.results?.[i]?.allowed || !!r?.results?.[i]?.reauth);
}

/** The actions allowed on each resource, in one call (spec/clustering-ui.md
 * §3.5): `result[i]` lists those of `actions` allowed on `resources[i]`. An
 * action that doesn't apply to a resource's type is left out. */
export async function allowedActions(actions: string[], resources: EntityRef[]): Promise<string[][]> {
  if (actions.length === 0 || resources.length === 0) return resources.map(() => []);
  const r = await query<{ allowed?: { actions?: string[] }[]; reauth?: { actions?: string[] }[] }>("/authz/allowed", { actions, resources });
  // As checkAccess: what a fresh login would allow counts.
  return resources.map((_, i) => [...(r?.allowed?.[i]?.actions ?? []), ...(r?.reauth?.[i]?.actions ?? [])]);
}

// ---- VMs --------------------------------------------------------------------

export const listVms = (project?: string | null) =>
  get<{ vms?: VmResponse[] }>(`/vms${inProject(project)}`).then((r) => r?.vms ?? []);

export async function getVm(id: string): Promise<VmResponse> {
  try {
    const r = await get<{ vm?: VmResponse }>(`/vms/${enc(id)}`);
    if (!r?.vm) throw new Error("VM not found");
    return r.vm;
  } catch (e) {
    if (e instanceof ApiRequestError && e.status === 404) throw new Error("VM not found");
    throw e;
  }
}

/** A VM write's answer: `{vm, settled}`, or `{}` when it answered 204 (gone). */
const vmWrite = (p: Promise<VmWriteResponse | undefined>) => p.then((r) => r ?? {});

export const createVm = (req: CreateVmRequest) => vmWrite(post<VmWriteResponse | undefined>("/vms", req));

/** Lifecycle calls change the VM's desired state; `wait` holds the answer
 * until the controller got it there, or reports why it couldn't
 * (spec/reconciliation.md §12.3). */
const WAIT_SECS = 60;

export const startVm = (id: string) => vmWrite(post<VmWriteResponse | undefined>(`/vms/${enc(id)}/start?wait=${WAIT_SECS}`));

/** Stop a VM. With `gracefulTimeoutSecs`, press the guest's power button
 * first and stop it hard only if it is still running after that long. */
export function stopVm(id: string, gracefulTimeoutSecs?: number): Promise<VmWriteResponse> {
  const wait = Math.min(300, Math.max(WAIT_SECS, (gracefulTimeoutSecs ?? 0) + 20));
  const grace = gracefulTimeoutSecs === undefined ? "" : `&graceful_timeout_secs=${gracefulTimeoutSecs}`;
  return vmWrite(post<VmWriteResponse | undefined>(`/vms/${enc(id)}/stop?wait=${wait}${grace}`));
}

export const pauseVm = (id: string) => vmWrite(post<VmWriteResponse | undefined>(`/vms/${enc(id)}/pause?wait=${WAIT_SECS}`));

/** Delete a VM: `undefined` once it is gone (204), else the VM still being torn down (202 `{vm}`). */
export const deleteVm = (id: string, keepDisk = false) =>
  del<{ vm?: VmResponse } | undefined>(`/vms/${enc(id)}?wait=${WAIT_SECS}${keepDisk ? "&keep_disk=true" : ""}`).then((r) => r?.vm);

/** One VM lifecycle action from the UI's buttons. `shutdown` presses the
 * power button and stops the VM hard after 60 s. */
export function vmAction(id: string, action: "start" | "shutdown" | "stop" | "pause" | "delete"): Promise<unknown> {
  switch (action) {
    case "start":
      return startVm(id);
    case "shutdown":
      return stopVm(id, 60);
    case "stop":
      return stopVm(id);
    case "pause":
      return pauseVm(id);
    case "delete":
      return deleteVm(id);
  }
}

/** A VM's event history: starts, exits, restarts, adoptions (glidex.common.v1.Event). */
export type VmEvent = StorageEvent;
export const vmEvents = (id: string) =>
  get<{ events?: VmEvent[] }>(`/vms/${enc(id)}/events`).then((r) => ({ events: r?.events ?? [] }));

/** A single-use console ticket for the WebSocket (spec §5.6). */
export const consoleTicket = (id: string) =>
  post<{ ticket: string; expires_in: number }>(`/vms/${enc(id)}/console/ticket`);

// ---- guest credentials ------------------------------------------------------

/** The caller's own SSH public keys from their home directory (local accounts only). */
export interface MySshKeys {
  available: boolean;
  username?: string;
  keys: string[];
  reason?: string;
}
/** Proto JSON leaves out a false `available` and empty `keys`. */
export const mySshKeys = () =>
  get<Partial<MySshKeys>>("/users/me/ssh-keys").then((r) => ({ ...r, available: !!r?.available, keys: r?.keys ?? [] }));

// The storage routes answer in the proto3 JSON shape (types.ts): each wraps
// its object (`{credential}`, `{disk}`, …) and lists (`{disks}`, `{items}`).

export const listCredentials = (project?: string | null) =>
  get<{ credentials?: CredentialInfo[] }>(`/credentials${inProject(project)}`).then((r) => r?.credentials ?? []);
export const createCredential = (req: CreateCredentialRequest) =>
  post<{ credential?: CredentialInfo }>("/credentials", req).then((r) => r?.credential);
export const updateCredential = (username: string, req: UpdateCredentialRequest, project?: string | null) =>
  put<{ credential?: CredentialInfo }>(`/credentials/${enc(username)}${inProject(project)}`, req).then((r) => r?.credential);
export const deleteCredential = (username: string, project?: string | null) =>
  del(`/credentials/${enc(username)}${inProject(project)}`);

// ---- networks ---------------------------------------------------------------
// glidex.network.v1 (types.ts): networks are keyed by id (a name works in
// paths too); answers wrap their object (`{network}`, `{router}`) and lists
// (`{networks}`, `{routers}`, `{shares}`).

export const listNetworks = () => get<{ networks?: Network[] }>("/networks").then((r) => r?.networks ?? []);
/** By id, or by name (409 `ambiguous` when several networks the caller can see have it). */
export const getNetwork = (id: string) => get<{ network?: Network }>(`/networks/${enc(id)}`).then((r) => r?.network as Network);
export const createNetwork = (req: CreateNetworkRequest) =>
  post<{ network?: Network }>("/networks", req).then((r) => r?.network as Network);
/** `undefined` when gone (204), else the network its controller is still deleting (202). */
export const deleteNetwork = (id: string) =>
  del<{ network?: Network } | undefined>(`/networks/${enc(id)}`).then((r) => r?.network);
export const networkEvents = (id: string) =>
  get<{ events?: StorageEvent[] }>(`/networks/${enc(id)}/events`).then((r) => r?.events ?? []);
/** Which projects and tenants may use a host network (replaces the grants). */
export const grantNetwork = (id: string, req: { grants?: string[]; tenants?: string[]; all_projects?: boolean }) =>
  put<{ network?: Network }>(`/networks/${enc(id)}/grants`, req).then((r) => r?.network as Network);
export const createProjectNetwork = (project: string, req: Omit<CreateNetworkRequest, "bridge" | "physnet" | "grants" | "tenants" | "all_projects">) =>
  post<{ network?: Network }>(`/projects/${enc(project)}/networks`, req).then((r) => r?.network as Network);
export const listRouters = (project: string) =>
  get<{ routers?: Router[] }>(`/projects/${enc(project)}/routers`).then((r) => r?.routers ?? []);
export const getRouter = (project: string, router: string) =>
  get<{ router?: Router }>(`/projects/${enc(project)}/routers/${enc(router)}`).then((r) => r?.router as Router);
export const createRouter = (project: string, spec: RouterSpec) =>
  post<{ router?: Router }>(`/projects/${enc(project)}/routers`, spec).then((r) => r?.router as Router);
/** By id or name. */
export const deleteRouter = (project: string, router: string) => del<void>(`/projects/${enc(project)}/routers/${enc(router)}`);
/** Put an existing NAT network (id) on one of the project's routers (id), or (`null`) back on the shared edge. */
export const setNetworkRouter = (project: string, network: string, router: string | null) =>
  put<{ network?: Network }>(`/projects/${enc(project)}/networks/${enc(network)}/router`, { router }).then(
    (r) => r?.network as Network,
  );
export const offerNetworkShare = (network: string, project: string) =>
  post<void>(`/networks/${enc(network)}/shares`, { project });
export const unshareNetwork = (network: string, project: string) =>
  del(`/networks/${enc(network)}/shares/${enc(project)}`);
export const listNetworkShares = (project: string) =>
  get<{ shares?: NetworkShare[] }>(`/projects/${enc(project)}/network-shares`).then((r) => r?.shares ?? []);
export const acceptNetworkShare = (project: string, network: string) =>
  post<{ network?: Network }>(`/projects/${enc(project)}/network-shares/${enc(network)}/accept`).then((r) => r?.network as Network);
export const leaveNetworkShare = (project: string, network: string) =>
  del(`/projects/${enc(project)}/network-shares/${enc(network)}`);

export const ovsStatus = () => get<OvsStatus>("/ovs/status");

export const installOvs = (profile: "kernel" | "dpdk", confirm: boolean) =>
  post<{ changed: boolean; ovs_version?: string; warnings: string[] }>("/ovs/install", {
    profile,
    source_build: false,
    confirm,
  });

export const listBridges = () => get<BridgeRecord[]>("/ovs/bridges");
export const createBridge = (name: string, datapath: "system" | "netdev") => post<BridgeRecord>("/ovs/bridges", { name, datapath });
export const deleteBridge = (name: string) => del(`/ovs/bridges/${enc(name)}`);
export const listUplinks = (bridge: string) => get<UplinkResult[]>(`/ovs/bridges/${enc(bridge)}/uplinks`);
/** Add an uplink; with `migrate_ip` the host's address moves to the bridge and must be committed. */
export const addUplink = (bridge: string, body: { name: string; migrate_ip: boolean; confirm: boolean; accept_dhcp: boolean } & UplinkKind) =>
  post<UplinkResult>(`/ovs/bridges/${enc(bridge)}/uplinks`, body);
export const commitUplink = (bridge: string, name: string, token: string) => post<UplinkResult>(`/ovs/bridges/${enc(bridge)}/uplinks/${enc(name)}/commit`, { token });
export const reapplyUplink = (bridge: string, name: string) => post<UplinkResult>(`/ovs/bridges/${enc(bridge)}/uplinks/${enc(name)}/reapply`);
export const deleteUplink = (bridge: string, name: string) => del(`/ovs/bridges/${enc(bridge)}/uplinks/${enc(name)}`);

// ---- images and disks ---------------------------------------------------------

/** The cloud image catalog for `arch` (`x86_64`, `aarch64`; default: the server's). */
export const imageCatalog = (arch?: string) =>
  get<{ items?: CatalogItem[] }>(`/images/catalog${arch ? `?arch=${enc(arch)}` : ""}`).then((r) => r?.items ?? []);
export const firmwareCatalog = (arch?: string) =>
  get<{ items?: FirmwareCatalogItem[] }>(`/images/firmware-catalog${arch ? `?arch=${enc(arch)}` : ""}`).then((r) => r?.items ?? []);
export const listImages = () => get<{ images?: ImageInfo[] }>("/images").then((r) => r?.images ?? []);
export const getImage = (id: string) => get<{ image?: ImageInfo }>(`/images/${enc(id)}`).then((r) => r?.image as ImageInfo);
/** 202 for a new download, 200 when one is already in flight: `{image, created}`. */
export const pullImage = (req: PullImageRequest) => post<PullImageResponse>("/images", req).then((r) => r ?? {});
/** Keep a copy on a node ahead of need, or let one go (§9.2.2). */
export const addImageCopy = (id: string, node: string) =>
  post<{ image?: ImageInfo }>(`/images/${enc(id)}/copies`, { node }).then((r) => r?.image);
export const removeImageCopy = (id: string, node: string) =>
  del<{ image?: ImageInfo } | undefined>(`/images/${enc(id)}/copies/${enc(node)}`).then((r) => r?.image);
/** The node deletes the unrecorded image files it reported (§9.2.3). */
export const deleteSpareImageFiles = (node: string) => post<{ node: string; files: number; bytes: number }>(`/nodes/${enc(node)}/spare-image-files/delete`, {});
/** `undefined` when gone (204), else the image its controller is still deleting (202). */
export const deleteImage = (id: string) => del<{ image?: ImageInfo } | undefined>(`/images/${enc(id)}`).then((r) => r?.image);
/** Download a failed image again (spec/reconciliation.md D19). */
export const retryImage = (id: string) => post<{ image?: ImageInfo }>(`/images/${enc(id)}/retry`).then((r) => r?.image);
export const imageEvents = (id: string) =>
  get<{ events?: StorageEvent[] }>(`/images/${enc(id)}/events`).then((r) => r?.events ?? []);

export const listDisks = (project?: string | null) => get<{ disks?: DiskInfo[] }>(`/disks${inProject(project)}`).then((r) => r?.disks ?? []);
/** By id, or by name (unique per tenant; a name two of the caller's tenants share answers 409 `ambiguous`). */
export const getDisk = (id: string) => get<{ disk?: DiskInfo }>(`/disks/${enc(id)}`).then((r) => r?.disk as DiskInfo);
// Disk writes are carried out by the disk controller (spec/reconciliation.md
// §10.1): wait for it, and show what is still pending if it isn't done.
// They answer `{disk, settled}`; a 204 (gone meanwhile) reads `{}`.
export const createDisk = (req: CreateDiskRequest) => post<DiskWriteResponse | undefined>(`/disks?wait=${WAIT_SECS}`, req).then((r) => r ?? {});
export const resizeDisk = (id: string, sizeGib: number, extendRoot?: boolean) =>
  post<DiskWriteResponse | undefined>(`/disks/${enc(id)}/resize?wait=${WAIT_SECS}`, { size_gib: sizeGib, extend_root: extendRoot }).then((r) => r ?? {});
export const extendRoot = (id: string, mode: ExtendMode) =>
  post<DiskWriteResponse | undefined>(`/disks/${enc(id)}/extend-root?wait=${WAIT_SECS}`, { mode }).then((r) => r ?? {});
/** `undefined` when gone (204), else the disk, deleted once its operation finishes (202). */
export const deleteDisk = (id: string) => del<{ disk?: DiskInfo } | undefined>(`/disks/${enc(id)}`).then((r) => r?.disk);
export const diskEvents = (id: string) =>
  get<{ events?: StorageEvent[] }>(`/disks/${enc(id)}/events`).then((r) => r?.events ?? []);

// ---- projects and role links ------------------------------------------------
// The access routes answer in the proto3 JSON shape (types.ts): each wraps
// its object (`{project}`, `{binding}`, …) and lists (`{projects}`, …).

export const listProjects = () => get<{ projects?: ProjectView[] }>("/projects").then((r) => r?.projects ?? []);
export const getProject = (id: string) => get<{ project?: ProjectView }>(`/projects/${enc(id)}`).then((r) => r?.project as ProjectView);
export const createProject = (req: { name: string; description?: string; quotas?: Quotas; owners?: string[]; tenant?: string }) =>
  post<{ project?: Project }>("/projects", req).then((r) => r?.project as Project);
/** Changes the fields present; `quotas` replaces the limits (an absent one is unlimited). */
export const updateProject = (id: string, req: { description?: string; quotas?: Quotas }) =>
  patch<{ project?: Project }>(`/projects/${enc(id)}`, req).then((r) => r?.project as Project);
export const deleteProject = (id: string) => del(`/projects/${enc(id)}`);

export const listBindings = (project: string) =>
  get<{ bindings?: Binding[] }>(`/projects/${enc(project)}/bindings`).then((r) => r?.bindings ?? []);
export const addBinding = (project: string, role: string, principal: EntityRef) =>
  post<{ binding?: Binding }>(`/projects/${enc(project)}/bindings`, { role, principal }).then((r) => r?.binding as Binding);
export const removeBinding = (project: string, link: string) => del(`/projects/${enc(project)}/bindings/${enc(link)}`);

export const listSystemBindings = () => get<{ bindings?: Binding[] }>("/system/bindings").then((r) => r?.bindings ?? []);
export const addSystemBinding = (role: string, principal: EntityRef) =>
  post<{ binding?: Binding }>("/system/bindings", { role, principal }).then((r) => r?.binding as Binding);
export const removeSystemBinding = (link: string) => del(`/system/bindings/${enc(link)}`);

// ---- users, teams and tokens --------------------------------------------------

export const listUsers = () => get<{ users?: UserView[] }>("/users").then((r) => r?.users ?? []);
export const updateUser = (id: string, req: { display_name?: string; disabled?: boolean; default_project?: string }) =>
  patch<{ user?: User }>(`/users/${enc(id)}`, req).then((r) => r?.user as User);

export const listTeams = () => get<{ teams?: Team[] }>("/teams").then((r) => r?.teams ?? []);
export const createTeam = (name: string) => post<{ team?: Team }>("/teams", { name }).then((r) => r?.team as Team);
export const deleteTeam = (id: string) => del(`/teams/${enc(id)}`);
export const addTeamMember = (team: string, user: string) =>
  put<{ team?: Team }>(`/teams/${enc(team)}/members/${enc(user)}`).then((r) => r?.team as Team);
export const removeTeamMember = (team: string, user: string) =>
  del<{ team?: Team }>(`/teams/${enc(team)}/members/${enc(user)}`).then((r) => r?.team as Team);

export const listTokens = () => get<{ tokens?: ApiToken[] }>("/tokens").then((r) => r?.tokens ?? []);
export const createToken = (req: CreateTokenRequest) => post<CreatedToken>("/tokens", req);
export const revokeToken = (id: string) => del(`/tokens/${enc(id)}`);

// ---- policies and audit -------------------------------------------------------

export const listPolicies = () =>
  get<{ policies?: PolicyInfo[]; site?: SitePolicy[] }>("/authz/policies").then((r) => ({
    policies: r?.policies ?? [],
    site: r?.site ?? [],
  }));
export const policyVersions = (id: string) =>
  get<{ versions?: SitePolicyVersion[] }>(`/authz/policies/${enc(id)}/versions`).then((r) => r?.versions ?? []);
export const validatePolicy = (id: string, text: string) =>
  post<{ valid?: boolean; errors?: string[] }>("/authz/validate", { id, text }).then((r) => ({ valid: !!r?.valid, errors: r?.errors }));
export const simulatePolicy = (changes: PolicyChange[], requests: SimulationRequest[]) =>
  post<{ results?: SimulationResult[] }>("/authz/simulate", { changes, requests }).then((r) => ({ results: r?.results ?? [] }));
/** Save a site policy; `version` is the one it replaces (0 for a new one).
 * Answers the saved policy, or nothing when the policy was removed. */
export const putPolicy = (id: string, req: { text: string; description: string; enabled: boolean; version: number }) =>
  put<{ policy?: SitePolicy }>(`/authz/policies/${enc(id)}`, req).then((r) => r?.policy ?? null);
export const deletePolicy = (id: string, version: number) => del(`/authz/policies/${enc(id)}?version=${version}`);

/** `since` is Unix milliseconds. */
export function readAudit(q: { project?: string; user?: string; since?: number; limit?: number }): Promise<AuditEntry[]> {
  const p = new URLSearchParams();
  if (q.project) p.set("project", q.project);
  if (q.user) p.set("user", q.user);
  if (q.since) p.set("since", String(q.since));
  if (q.limit) p.set("limit", String(q.limit));
  const s = p.toString();
  return get<{ entries?: AuditEntry[] }>(`/audit${s ? `?${s}` : ""}`).then((r) => r?.entries ?? []);
}

// ---- usage (spec/metering.md §9) ---------------------------------------------

export interface UsageQuery {
  project?: string;
  from?: string;
  to?: string;
  month?: string;
  granularity?: "hour" | "day" | "month";
  group_by?: string;
  meters?: string;
}

function usageQuery(q: UsageQuery, extra: Record<string, string> = {}): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries({ ...q, ...extra })) if (v) p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
}

export const getUsage = (q: UsageQuery) => get<UsageResponse>(`/usage${usageQuery(q)}`);
export const getBandwidth = (q: UsageQuery) => get<RateResponse>(`/usage/bandwidth${usageQuery(q)}`);
export const getDiskIo = (q: UsageQuery) => get<RateResponse>(`/usage/disk-io${usageQuery(q)}`);
export const getCompute = (q: UsageQuery) => get<RateResponse>(`/usage/compute${usageQuery(q)}`);
/** A same-origin link that downloads the CSV export (sent with the session cookie). */
export const usageCsvUrl = (kind: "usage" | "bandwidth" | "disk-io" | "compute", q: UsageQuery) =>
  `${API_BASE}/usage${kind === "usage" ? "" : `/${kind}`}${usageQuery(q, { format: "csv" })}`;
export const getVmBandwidth = (id: string) => get<SeriesResponse>(`/vms/${enc(id)}/bandwidth?p95=true`);
export const getVmIo = (id: string) => get<SeriesResponse>(`/vms/${enc(id)}/io?p95=true`);
export const getVmCompute = (id: string) => get<SeriesResponse>(`/vms/${enc(id)}/compute?p95=true`);
export const getVmStats = (id: string) => get<VmStats>(`/vms/${enc(id)}/stats`);

// ---- cluster (spec/clustering.md §5, §7) ----------------------------------

export interface ClusterNodeRow {
  id: string;
  name: string;
  role: string;
  phase: string;
  advertise: string | null;
  version?: string;
  feature_level?: number;
  ready?: string;
  /** Its newest certificate: signed by the current CA, and until when. */
  cert?: { current_ca: boolean; not_after: number } | null;
}

export interface ClusterStatus {
  clustered?: boolean;
  cluster_id?: string;
  /** The node serving this UI (through an agent's UI: that agent). */
  node_id?: string;
  name?: string;
  role?: string;
  advertise?: string;
  /** The server that answered, when it isn't the node serving this UI
   * (an agent relays its UI to a server, spec/clustering-ui.md §3.6). */
  served_by?: { node_id: string; name: string };
  feature_level?: number;
  ca_fingerprint?: string;
  ca?: { signing: string; retiring: string[]; retire_at: number | null; started_at?: number | null; not_after?: number | null } | null;
  raft?: {
    state: string;
    term: number;
    leader: { name: string | null; address: string | null } | null;
    voters: { name: string | null; address: string | null }[];
    learners: { name: string | null; address: string | null }[];
    last_applied: number | null;
    last_log_index: number | null;
  } | null;
  nodes: ClusterNodeRow[];
  ports?: { node: string; port: number; proto: string; reachable: boolean }[];
}

export const clusterStatus = () => get<ClusterStatus>("/cluster/status?ports=true");
/** The summary without the port checks (which connect to every node). */
export const clusterSummary = () => get<ClusterStatus>("/cluster/status");
export const listNodes = () => get<NodeRecord[]>("/nodes");
export interface PreviewRequest {
  project?: string;
  vcpu_count: number;
  mem_size_mib: number;
  hypervisor?: ProtoHypervisor;
  node?: string;
  networks?: { network: string }[];
  vfio_devices?: string[];
  data_disks?: string[];
  /** The root disk's image: its architecture counts (spec/clustering.md §9.2.1). */
  image?: string;
}
/** Where a VM would go, node by node (spec/clustering-ui.md §5.1). */
export const previewPlacement = (req: PreviewRequest) => query<PlacementPreview>("/scheduler/preview", req);
export const getNode = (id: string) => get<NodeRecord>(`/nodes/${enc(id)}`);
/** A node's br-int datapath (spec/clustering.md §11.6). */
export const setNodeBrInt = (id: string, br_int_datapath: BrIntDatapath) => put<Record<string, unknown>>(`/nodes/${enc(id)}/ovn`, { br_int_datapath });
/** A node's physical networks: physnet → bridge (replaces its own; the file's stay). */
export const setNodeMappings = (id: string, bridge_mappings: Record<string, string>) => put<Record<string, unknown>>(`/nodes/${enc(id)}/ovn`, { bridge_mappings });

// ---- membership (spec/clustering-ui.md §7) ---------------------------------------

export const createJoinToken = (req: { role: "agent" | "server"; ttl_secs: number; allow_import: boolean }) => post<IssuedToken>("/cluster/join-tokens", req);
export const removeNode = (id: string, force = false) => post<Record<string, unknown>>(`/nodes/${enc(id)}/remove`, { force });
export const forgetNode = (id: string, fenced: boolean, force = false) => post<Record<string, unknown>>(`/nodes/${enc(id)}/forget`, { fenced, force });
export const purgeNode = (id: string) => post<Record<string, unknown>>(`/nodes/${enc(id)}/purge`);
export const rejoinToken = (id: string, req: { raft_intact: boolean; ttl_secs: number }) => post<IssuedToken>(`/nodes/${enc(id)}/rejoin-token`, req);
export const promoteNodes = (nodes: string[], force = false) => post<Record<string, unknown>>("/cluster/promote", { nodes, force });
export const detachNode = (id: string, req: { map_networks: Record<string, string>; with_access: boolean }) => post<Record<string, unknown>>(`/nodes/${enc(id)}/detach`, req);
export const abortDetach = (id: string) => post<Record<string, unknown>>(`/nodes/${enc(id)}/detach`, { abort: true });
export const listDepartures = () => get<Departure[]>("/departures");
export const clusterOvn = () => get<OvnOverview>("/cluster/ovn");
export const clusterTunnels = () => get<{ enabled: boolean; nodes?: NodeTunnels[] }>("/cluster/ovn/tunnels");
/** A shared edge that should work for this cluster, from what nodes report (spec/clustering-ui.md §8a). */
export const getEdgeDefaults = () => get<{ edge: OvnEdge; complete: boolean; notes: string[] }>("/cluster/settings/ovn/edge-defaults");
export const getOvnSettings = () => get<OvnSettingsView>("/cluster/settings/ovn");
export const putOvnSettings = (s: Omit<OvnSettings, "updated_by" | "updated_at">) => put<OvnSettingsView>("/cluster/settings/ovn", s);
export const networkOvn = (id: string) => get<NetworkOvn>(`/networks/${enc(id)}/ovn`);

// ---- cluster lifecycle (spec/clustering-ui.md §7.5–7.8) ---------------------------

export const initCluster = (req: { advertise?: string; tunnel_ip?: string }) => post<Record<string, unknown>>("/cluster/init", { ...req, force: true });
export const rotateCa = (grace_secs?: number) => post<Record<string, unknown>>("/cluster/rotate-ca", grace_secs ? { grace_secs } : {});
export const dissolveCluster = () => post<Record<string, unknown>>("/cluster/dissolve", { force: true });
export const leaveCluster = () => post<Record<string, unknown>>("/cluster/leave");
/** A same-origin link that downloads the cluster snapshot (with the session cookie). */
export const clusterSnapshotUrl = () => `${API_BASE}/cluster/snapshot`;
export const listImports = () => get<ImportPlanView[]>("/imports");
export const getImport = (plan: string) => get<ImportPlanView>(`/imports/${enc(plan)}`);
export const checkImport = (plan: string, m: ImportMappings) => post<ImportPlanView>(`/imports/${enc(plan)}/approve`, { ...m, dry_run: true });
export const approveImport = (plan: string, m: ImportMappings) => post<ImportPlanView>(`/imports/${enc(plan)}/approve`, m);
export const rejectImport = (plan: string) => post<ImportPlanView>(`/imports/${enc(plan)}/reject`);
export const drainNode = (id: string, drain: boolean) => post<unknown>(`/nodes/${enc(id)}/${drain ? "drain" : "undrain"}`);

// ---- tenants and admissions (spec/tenancy.md §14) -----------------------------

/** Every tenant the caller may read, over all pages. */
export async function listTenants(): Promise<Tenant[]> {
  const out: Tenant[] = [];
  let token = "";
  do {
    const r = await get<{ tenants?: Tenant[]; next_page_token?: string }>(
      `/tenants?page_size=500${token ? `&page_token=${enc(token)}` : ""}`,
    );
    out.push(...(r.tenants ?? []));
    token = r.next_page_token ?? "";
  } while (token);
  return out;
}
export const getTenant = (id: string) => get<{ tenant: Tenant }>(`/tenants/${enc(id)}`).then((r) => r.tenant);
export const createTenant = (req: { slug: string; display_name?: string; operator?: boolean }) =>
  post<{ tenant: Tenant }>("/tenants", req).then((r) => r.tenant);
export const updateTenant = (id: string, req: { display_name?: string; operator?: boolean }) =>
  patch<{ tenant: Tenant }>(`/tenants/${enc(id)}`, req).then((r) => r.tenant);
export const suspendTenant = (id: string) => post<{ tenant: Tenant }>(`/tenants/${enc(id)}/suspend`);
export const resumeTenant = (id: string) => post<{ tenant: Tenant }>(`/tenants/${enc(id)}/resume`);
export const deleteTenant = (id: string) => del(`/tenants/${enc(id)}`);

export const listTenantMembers = (id: string) =>
  get<{ members?: TenantMember[] }>(`/tenants/${enc(id)}/members`).then((r) => r.members ?? []);
export const addTenantMember = (id: string, principal: EntityRef) => post(`/tenants/${enc(id)}/members`, { principal });
/** `principal` is a User or Team; the path names it `user:<id>` / `team:<id>`. */
export const removeTenantMember = (id: string, principal: EntityRef) =>
  del(`/tenants/${enc(id)}/members/${enc(`${principal.type.toLowerCase()}:${"id" in principal ? principal.id : ""}`)}`);

export const listTenantBindings = (id: string) =>
  get<{ bindings?: TenantBinding[] }>(`/tenants/${enc(id)}/bindings`).then((r) => r.bindings ?? []);
export const addTenantBinding = (id: string, role: string, principal: EntityRef) =>
  post(`/tenants/${enc(id)}/bindings`, { role, principal });
export const removeTenantBinding = (id: string, link: string) => del(`/tenants/${enc(id)}/bindings/${enc(link)}`);

export const listAdmissions = () => get<{ admissions?: Admission[] }>("/admissions").then((r) => r.admissions ?? []);
/** Admit a tenant or change its admission. An absent `cap` keeps the current
 * one (unlimited for a new admission); `{}` is unlimited. */
export const setAdmission = (tenant: string, req: { cap?: TenantQuotas; suspended?: boolean }) =>
  put<{ admission: Admission }>(`/admissions/${enc(tenant)}`, req).then((r) => r.admission);
export const removeAdmission = (tenant: string) => del(`/admissions/${enc(tenant)}`);

// ---- login connectors (spec/tenancy.md §5, §14) ------------------------------

export const listConnectors = () => get<{ connectors?: Connector[] }>("/connectors").then((r) => r.connectors ?? []);
export const getConnector = (id: string) => get<{ connector: Connector }>(`/connectors/${enc(id)}`).then((r) => r.connector);
export const createConnector = (connector: ConnectorInput) =>
  post<{ connector: Connector }>("/connectors", { connector }).then((r) => r.connector);
/** Only the fields present are changed (the server derives the mask). */
export const updateConnector = (id: string, connector: Partial<ConnectorInput>) =>
  patch<{ connector: Connector }>(`/connectors/${enc(id)}`, { connector }).then((r) => r.connector);
export const deleteConnector = (id: string) => del(`/connectors/${enc(id)}`);
/** Write-only: the value is sealed server-side and never comes back. */
export const setConnectorSecret = (id: string, field: string, value: string) =>
  put<{ connector: Connector }>(`/connectors/${enc(id)}/secrets/${enc(field)}`, { value }).then((r) => r.connector);
/** Checks that need no user; reads only, so safe to retry. */
export const testConnector = (id: string) =>
  query<{ steps?: ConnectorTestStep[] }>(`/connectors/${enc(id)}/test`, {}).then((r) => r.steps ?? []);
