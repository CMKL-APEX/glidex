import type {
  ApiToken,
  AuditEntry,
  AuthMethods,
  Binding,
  CatalogItem,
  FirmwareCatalogItem,
  HypervisorType,
  CreateDiskRequest,
  CreatedToken,
  CreateTokenRequest,
  DiskInfo,
  EntityRef,
  ImageInfo,
  BridgeRecord,
  CreateNetworkRequest,
  Network,
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
  // A real answer means a leader is back; a local read means it isn't yet.
  if (resp.ok) setAvailability(stale ? { available: false, stale: true } : { available: true, stale: false });
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

/** Leave the page for the identity provider. */
export function startOidc(returnTo: string, reauth = false) {
  const q = new URLSearchParams({ return_to: returnTo });
  if (reauth) q.set("reauth", "1");
  window.location.assign(`${API_BASE}/auth/oidc/start?${q}`);
}

export async function logout(): Promise<void> {
  await request<void>("POST", "/auth/logout", {}, { quiet401: true });
  setCsrf(null);
}

export const updateMe = (body: { default_project: string | null }) => patch<User>("/users/me", body);

/** Capability checks for the caller (spec §7.7): one boolean per check. */
export async function checkAccess(checks: { action: string; resource: EntityRef }[]): Promise<boolean[]> {
  if (checks.length === 0) return [];
  const r = await query<{ results: { allowed: boolean }[] }>("/authz/check", { checks });
  return r.results.map((x) => x.allowed);
}

/** The actions allowed on each resource, in one call (spec/clustering-ui.md
 * §3.5): `result[i]` lists those of `actions` allowed on `resources[i]`. An
 * action that doesn't apply to a resource's type is left out. */
export async function allowedActions(actions: string[], resources: EntityRef[]): Promise<string[][]> {
  if (actions.length === 0 || resources.length === 0) return resources.map(() => []);
  const r = await query<{ allowed: string[][] }>("/authz/allowed", { actions, resources });
  return r.allowed;
}

// ---- VMs --------------------------------------------------------------------

export const listVms = (project?: string | null) => get<VmResponse[]>(`/vms${inProject(project)}`);

export async function getVm(id: string): Promise<VmResponse> {
  try {
    return await get<VmResponse>(`/vms/${enc(id)}`);
  } catch (e) {
    if (e instanceof ApiRequestError && e.status === 404) throw new Error("VM not found");
    throw e;
  }
}

export const createVm = (req: CreateVmRequest) => post<VmResponse>("/vms", req);

/** Lifecycle calls change the VM's desired state; `wait` holds the answer
 * until the controller got it there, or reports why it couldn't
 * (spec/reconciliation.md §12.3). */
const WAIT_SECS = 60;

export const startVm = (id: string) => post<VmResponse>(`/vms/${enc(id)}/start?wait=${WAIT_SECS}`);

/** Stop a VM. With `gracefulTimeoutSecs`, press the guest's power button
 * first and stop it hard only if it is still running after that long. */
export function stopVm(id: string, gracefulTimeoutSecs?: number): Promise<VmResponse> {
  const wait = Math.min(300, Math.max(WAIT_SECS, (gracefulTimeoutSecs ?? 0) + 20));
  const grace = gracefulTimeoutSecs === undefined ? "" : `&graceful_timeout_secs=${gracefulTimeoutSecs}`;
  return post<VmResponse>(`/vms/${enc(id)}/stop?wait=${wait}${grace}`);
}

export const pauseVm = (id: string) => post<VmResponse>(`/vms/${enc(id)}/pause?wait=${WAIT_SECS}`);
export const deleteVm = (id: string, keepDisk = false) =>
  del(`/vms/${enc(id)}?wait=${WAIT_SECS}${keepDisk ? "&keep_disk=true" : ""}`);

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

/** A VM's event history: starts, exits, restarts, adoptions. */
export interface VmEvent {
  at: number;
  actor: string;
  kind: "normal" | "warning";
  reason: string;
  message: string;
}
export const vmEvents = (id: string) => get<{ events: VmEvent[] }>(`/vms/${enc(id)}/events`);

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
export const mySshKeys = () => get<MySshKeys>("/users/me/ssh-keys");

export const listCredentials = (project?: string | null) => get<CredentialInfo[]>(`/credentials${inProject(project)}`);
export const createCredential = (req: CreateCredentialRequest) => post<CredentialInfo>("/credentials", req);
export const updateCredential = (username: string, req: UpdateCredentialRequest, project?: string | null) =>
  put<CredentialInfo>(`/credentials/${enc(username)}${inProject(project)}`, req);
export const deleteCredential = (username: string, project?: string | null) =>
  del(`/credentials/${enc(username)}${inProject(project)}`);

// ---- networks ---------------------------------------------------------------

export const listNetworks = () => get<Network[]>("/networks");
export const createNetwork = (req: CreateNetworkRequest) => post<Network>("/networks", req);
/** `undefined` when gone (204), else the network its controller is still deleting (202). */
export const deleteNetwork = (name: string) => del<Network | undefined>(`/networks/${enc(name)}`);
export const createProjectNetwork = (project: string, req: CreateNetworkRequest) =>
  post<Network>(`/projects/${enc(project)}/networks`, req);
export const offerNetworkShare = (network: string, project: string) =>
  post<void>(`/networks/${enc(network)}/shares`, { project });
export const unshareNetwork = (network: string, project: string) =>
  del(`/networks/${enc(network)}/shares/${enc(project)}`);
export const listNetworkShares = (project: string) => get<NetworkShare[]>(`/projects/${enc(project)}/network-shares`);
export const acceptNetworkShare = (project: string, network: string) =>
  post<Network>(`/projects/${enc(project)}/network-shares/${enc(network)}/accept`);
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

// ---- images and disks ---------------------------------------------------------

export const imageCatalog = () => get<CatalogItem[]>("/images/catalog");
export const listImages = () => get<ImageInfo[]>("/images");
export const getImage = (id: string) => get<ImageInfo>(`/images/${enc(id)}`);
export const firmwareCatalog = () => get<FirmwareCatalogItem[]>("/images/firmware-catalog");
export const pullImage = (req: {
  catalog?: string;
  /** Firmware catalog key. */
  firmware?: string;
  url?: string;
  sha256?: string;
  name?: string;
  /** With `url`: the download is UEFI firmware for `hypervisor`. */
  kind?: "firmware";
  hypervisor?: HypervisorType;
  /** With a QEMU firmware `url`: its UEFI variable-store template. */
  vars_url?: string;
  vars_sha256?: string;
}) => post<ImageInfo>("/images", req);
/** `undefined` when gone (204), else the image its controller is still deleting (202). */
export const deleteImage = (id: string) => del<ImageInfo | undefined>(`/images/${enc(id)}`);
/** Download a failed image again (spec/reconciliation.md D19). */
export const retryImage = (id: string) => post<ImageInfo>(`/images/${enc(id)}/retry`);

export const listDisks = (project?: string | null) => get<DiskInfo[]>(`/disks${inProject(project)}`);
export const getDisk = (id: string) => get<DiskInfo>(`/disks/${enc(id)}`);
// Disk writes are carried out by the disk controller (spec/reconciliation.md
// §10.1): wait for it, and show what is still pending if it isn't done.
export const createDisk = (req: CreateDiskRequest) => post<DiskInfo>(`/disks?wait=${WAIT_SECS}`, req);
export const resizeDisk = (id: string, sizeGib: number, extendRoot?: boolean) =>
  post<DiskInfo>(`/disks/${enc(id)}/resize?wait=${WAIT_SECS}`, { size_gib: sizeGib, extend_root: extendRoot });
export const extendRoot = (id: string, mode: "offline" | "on-boot") =>
  post<DiskInfo>(`/disks/${enc(id)}/extend-root?wait=${WAIT_SECS}`, { mode });
/** `undefined` when gone (204), else the disk, deleted once its operation finishes (202). */
export const deleteDisk = (id: string) => del<DiskInfo | undefined>(`/disks/${enc(id)}`);

// ---- projects and role links ------------------------------------------------

export const listProjects = () => get<ProjectView[]>("/projects");
export const getProject = (id: string) => get<ProjectView>(`/projects/${enc(id)}`);
export const createProject = (req: { name: string; description?: string; quotas?: Partial<Quotas>; owners?: string[] }) =>
  post<Project>("/projects", req);
export const updateProject = (id: string, req: { description?: string; quotas?: Quotas }) =>
  patch<Project>(`/projects/${enc(id)}`, req);
export const deleteProject = (id: string) => del(`/projects/${enc(id)}`);

export const listBindings = (project: string) => get<Binding[]>(`/projects/${enc(project)}/bindings`);
export const addBinding = (project: string, role: string, principal: EntityRef) =>
  post<Binding>(`/projects/${enc(project)}/bindings`, { role, principal });
export const removeBinding = (project: string, link: string) => del(`/projects/${enc(project)}/bindings/${enc(link)}`);

export const listSystemBindings = () => get<Binding[]>("/system/bindings");
export const addSystemBinding = (role: string, principal: EntityRef) =>
  post<Binding>("/system/bindings", { role, principal });
export const removeSystemBinding = (link: string) => del(`/system/bindings/${enc(link)}`);

// ---- users, teams and tokens --------------------------------------------------

export const listUsers = () => get<UserView[]>("/users");
export const updateUser = (id: string, req: { display_name?: string; disabled?: boolean; default_project?: string }) =>
  patch<User>(`/users/${enc(id)}`, req);

export const listTeams = () => get<Team[]>("/teams");
export const createTeam = (name: string) => post<Team>("/teams", { name });
export const deleteTeam = (id: string) => del(`/teams/${enc(id)}`);
export const addTeamMember = (team: string, user: string) => put<Team>(`/teams/${enc(team)}/members/${enc(user)}`);
export const removeTeamMember = (team: string, user: string) => del<Team>(`/teams/${enc(team)}/members/${enc(user)}`);

export const listTokens = () => get<ApiToken[]>("/tokens");
export const createToken = (req: CreateTokenRequest) => post<CreatedToken>("/tokens", req);
export const revokeToken = (id: string) => del(`/tokens/${enc(id)}`);

// ---- policies and audit -------------------------------------------------------

export const listPolicies = () => get<{ policies: PolicyInfo[]; site: SitePolicy[] }>("/authz/policies");
export const policyVersions = (id: string) => get<SitePolicyVersion[]>(`/authz/policies/${enc(id)}/versions`);
export const validatePolicy = (id: string, text: string) =>
  post<{ valid: boolean; errors?: string[] }>("/authz/validate", { id, text });
export const simulatePolicy = (changes: PolicyChange[], requests: SimulationRequest[]) =>
  post<{ results: SimulationResult[] }>("/authz/simulate", { changes, requests });
export const putPolicy = (id: string, req: { text: string; description: string; enabled: boolean; version: number }) =>
  put<SitePolicy | null>(`/authz/policies/${enc(id)}`, req);
export const deletePolicy = (id: string, version: number) => del(`/authz/policies/${enc(id)}?version=${version}`);

export function readAudit(q: { project?: string; user?: string; since?: number; limit?: number }): Promise<AuditEntry[]> {
  const p = new URLSearchParams();
  if (q.project) p.set("project", q.project);
  if (q.user) p.set("user", q.user);
  if (q.since) p.set("since", String(q.since));
  if (q.limit) p.set("limit", String(q.limit));
  const s = p.toString();
  return get<AuditEntry[]>(`/audit${s ? `?${s}` : ""}`);
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
}

export interface ClusterStatus {
  clustered?: boolean;
  cluster_id?: string;
  /** The node that answered (the one serving this UI). */
  node_id?: string;
  name?: string;
  role?: string;
  advertise?: string;
  feature_level?: number;
  ca_fingerprint?: string;
  ca?: { signing: string; retiring: string[]; retire_at: number | null; started_at?: number | null } | null;
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
export const listNodes = () => get<NodeRecord[]>("/nodes");
export const getNode = (id: string) => get<NodeRecord>(`/nodes/${enc(id)}`);
export const drainNode = (id: string, drain: boolean) => post<unknown>(`/nodes/${enc(id)}/${drain ? "drain" : "undrain"}`);
