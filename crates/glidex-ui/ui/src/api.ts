import type {
  SyncStatus,
  ScimToken,
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
  OvsProfile,
  InstallOvsResponse,
  Datapath,
  PciDevice,
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
  ClusterStatus,
  PlacementPreview,
  ProtoHypervisor,
  Departure,
  ImportMappings,
  ImportPlanView,
  IssuedToken,
  NetworkOvn,
  OvnOverview,
  PutOvnSettings,
  OvnSettingsView,
  EdgeDefaults,
  NodeTunnels,
  BrIntDatapath,
  NodeRole,
  NodePhase,
  U64,
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
  HostTopology,
  Datastore,
  DatastoreInput,
} from "./types";

import { fromJson, toJson, type DescMethodUnary, type JsonValue, type MessageJsonType } from "@bufbuild/protobuf";
import { Code, ConnectError, createClient, createContextKey, createContextValues, type Interceptor } from "@connectrpc/connect";
import { createGrpcWebTransport } from "@connectrpc/connect-web";
import { ErrorDetailsSchema } from "./gen/glidex/api/v1/errors_pb";
import { ErrorInfoSchema } from "./gen/google/rpc/error_details_pb";
import { VmService, type CreateVmRequestJson } from "./gen/glidex/vm/v1/vm_pb";
import { WatchEventsResponseSchema, WatchKind, WatchService, type WatchEventsResponseJson } from "./gen/glidex/watch/v1/watch_pb";
import {
  AuditService,
  AuthzService,
  PolicyService,
  ProjectService,
  SystemBindingService,
  TeamService,
  TokenService,
  UserService,
  type CreateProjectRequestJson,
  type UpdateProjectRequestJson,
} from "./gen/glidex/access/v1/access_pb";
import { CredentialService, DatastoreService, DiskService, ImageService } from "./gen/glidex/storage/v1/storage_pb";
import { NetworkService, NetworkShareService, RouterService } from "./gen/glidex/network/v1/network_pb";
import { HostNetworkService, PciService } from "./gen/glidex/host/v1/host_pb";
import { ClusterService, ImportService, NodeService, type PciPoolJson, type PutOvnSettingsRequestJson } from "./gen/glidex/cluster/v1/cluster_pb";
import { AdmissionService, TenantService, type SetAdmissionRequestJson, type TenantDevicePoolJson } from "./gen/glidex/tenancy/v1/tenancy_pb";
import { InvitationService, type InvitationJson } from "./gen/glidex/directory/v1/invitations_pb";
import type { InspectInvitationResponseJson } from "./gen/glidex/auth/v1/login_pb";
import { ConnectorService, type ConnectorJson } from "./gen/glidex/directory/v1/connectors_pb";
import {
  MetricsService,
  UsageService,
  type GetBandwidthReportRequestJson,
  type GetUsageRequestJson,
  type GranularityJson,
  type GroupKeyJson,
} from "./gen/glidex/usage/v1/usage_pb";
import { archToProto } from "./types";

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

// ---- RPCs (spec/grpc.md G-D8): Connect-ES over gRPC-Web ----------------------

/** Read with `local`: answered from this server's own copy when there is no
 * quorum (`X-Glidex-Consistency: local`). */
const LOCAL = createContextKey(false);
/** Set by a call's interceptor when the answer carried a revision (it went
 * through the cluster). */
const REVISION = createContextKey<{ seen: boolean }>({ seen: false });

/** The session's CSRF value on every RPC (all are POSTs, spec/grpc.md §6). */
const sessionHeaders: Interceptor = (next) => async (req) => {
  if (csrf) req.header.set("X-Glidex-CSRF", csrf);
  if (req.contextValues.get(LOCAL)) req.header.set("X-Glidex-Consistency", "local");
  const res = await next(req);
  if (!res.stream && res.header.has("x-glidex-revision")) req.contextValues.get(REVISION).seen = true;
  return res;
};

/** gRPC-Web, with JSON messages (readable in the browser's tools), through
 * glidex-ui's `/api` proxy to the control plane's UI socket. */
export const transport = createGrpcWebTransport({
  baseUrl: API_BASE,
  useBinaryFormat: false,
  interceptors: [sessionHeaders],
  fetch: (input, init) => fetch(input, { ...init, credentials: "same-origin" }),
});

/** HTTP statuses for RPC codes, when the error doesn't carry REST's own. */
const CODE_STATUS: Partial<Record<Code, number>> = {
  [Code.InvalidArgument]: 400,
  [Code.Unauthenticated]: 401,
  [Code.PermissionDenied]: 403,
  [Code.ResourceExhausted]: 429,
  [Code.NotFound]: 404,
  [Code.AlreadyExists]: 409,
  [Code.Aborted]: 409,
  [Code.FailedPrecondition]: 409,
  [Code.Unimplemented]: 501,
  [Code.Unavailable]: 503,
  [Code.DeadlineExceeded]: 504,
};

/** An RPC error as REST's: its code (`ErrorInfo.reason`), REST status and
 * `details` (`glidex.api.v1.ErrorDetails`). */
export function rpcError(e: unknown): ApiRequestError {
  const err = ConnectError.from(e);
  let code = `rpc_${Code[err.code]?.toLowerCase() ?? "error"}`;
  let status = CODE_STATUS[err.code] ?? 500;
  let details: ApiError["details"];
  for (const info of err.findDetails(ErrorInfoSchema)) {
    if (info.reason) code = info.reason;
    const s = Number(info.metadata.http_status);
    if (s) status = s;
  }
  for (const d of err.findDetails(ErrorDetailsSchema)) {
    try {
      details = JSON.parse(d.json);
    } catch {
      /* not JSON */
    }
  }
  return new ApiRequestError(status, { error: code, message: err.rawMessage, details });
}

/** The JSON form of a method's request (what the REST twin takes). */
type RequestJson<M extends DescMethodUnary> = MessageJsonType<M["input"]>;

/** One RPC, with `request()`'s behaviour: the CSRF header, retries after a
 * leader change (reads), local reads without a quorum, the 401 handling and
 * step-up. The answer is its proto JSON, as the REST twin answers it. */
export async function rpc<M extends DescMethodUnary>(
  method: M,
  req: RequestJson<M> = {} as RequestJson<M>,
  opts: RequestOptions = {},
): Promise<MessageJsonType<M["output"]>> {
  const op = method.parent;
  const read = isRead(method);
  const idempotent = opts.idempotent ?? read;
  const client = createClient(op, transport) as unknown as Record<string, (i: unknown, o: unknown) => Promise<unknown>>;
  // As REST's body: fields set to `undefined` are left out. A field the
  // message doesn't have is REST's `422 invalid`.
  let input: unknown;
  try {
    input = fromJson(method.input, JSON.parse(JSON.stringify(req)) as JsonValue);
  } catch (e) {
    throw new ApiRequestError(422, { error: "invalid", message: e instanceof Error ? e.message : String(e) });
  }
  const send = async (local = false) => {
    const revision = { seen: false };
    const contextValues = createContextValues().set(LOCAL, local).set(REVISION, revision);
    const out = await client[method.localName](input, { contextValues });
    return { out, revision: revision.seen };
  };
  const attempt = async (): Promise<{ out: unknown; revision: boolean; stale: boolean }> => {
    for (let i = 0; ; i++) {
      try {
        return { ...(await send()), stale: false };
      } catch (e) {
        const err = rpcError(e);
        if (err.code === "leader_changed" && idempotent && i < LEADER_RETRY_MS.length) {
          await sleep(LEADER_RETRY_MS[i]);
          continue;
        }
        if (err.code === "cluster_unavailable") {
          setAvailability({ available: false, stale: availability.stale });
          // No quorum: a read can still come from this server's own copy.
          if (read) return { ...(await send(true)), stale: true };
        }
        throw err;
      }
    }
  };
  let result: { out: unknown; revision: boolean; stale: boolean };
  try {
    result = await attempt();
  } catch (e) {
    let err = e instanceof ApiRequestError ? e : rpcError(e);
    if (err.status === 401) {
      if (err.code === "reauth_required" && (await reauthenticate())) {
        try {
          result = await attempt();
          return finish(method, read, opts, result);
        } catch (e2) {
          err = e2 instanceof ApiRequestError ? e2 : rpcError(e2);
        }
      }
      if (err.code === "unauthenticated" && !opts.quiet401) onUnauthenticated();
    }
    throw err;
  }
  return finish(method, read, opts, result);
}

function finish<M extends DescMethodUnary>(
  method: M,
  read: boolean,
  opts: RequestOptions,
  r: { out: unknown; revision: boolean; stale: boolean },
): MessageJsonType<M["output"]> {
  // As request(): a stale read means no leader yet; a read through the
  // cluster or a write that worked means one is back.
  if (r.stale) setAvailability({ available: false, stale: true });
  else if ((!read && !opts.idempotent) || r.revision) setAvailability({ available: true, stale: false });
  if (!read && /\.(vm|storage|network)\.v1\./.test(method.parent.typeName)) {
    window.dispatchEvent(new Event(CHANGED_EVENT));
  }
  return toJson(method.output, r.out as never) as MessageJsonType<M["output"]>;
}

/** Whether a method only reads (`glidex.api.v1.op` kind), by its name: the
 * operation option isn't kept in the generated code, and the API's reads
 * are named so (spec/grpc.md §4), but for a few (ReadAudit, Allowed, WhoAmI). */
function isRead(m: DescMethodUnary): boolean {
  return /^(Get|List|Check|Validate|Simulate|Preview|Test|Read|Allowed$|WhoAmI$)/.test(m.name);
}

/** What a function answered when its REST twin answered 204: nothing. */
const none = (): void => undefined;

/** An update's `update_mask` (FieldMask JSON: lowerCamelCase paths): the
 * fields `req` sets, as REST derives it from the body's keys. */
function maskOf(req: object): string {
  return Object.entries(req)
    .filter(([, v]) => v !== undefined)
    .map(([k]) => k.replace(/_([a-z0-9])/g, (_, c: string) => c.toUpperCase()))
    .join(",");
}

/** A uint64 field (JSON string); absent when `n` is. */
const u64 = (n: number | undefined) => (n === undefined ? undefined : String(n));

// ---- authentication -----------------------------------------------------------
// Health, the login methods, PAM login, logout and the OIDC redirects have
// no RPC twin: they stay REST.

export async function healthCheck(): Promise<HealthResponse> {
  return request("GET", "/health", undefined, { quiet401: true });
}

export const authMethods = () => request<AuthMethods>("GET", "/auth/methods", undefined, { quiet401: true });

/** REST, not `SystemService.WhoAmI`: every cookie-authenticated RPC needs
 * the CSRF value (spec/grpc.md §6), and this is where the session learns it
 * (it is kept in memory only, so a fresh page load has none yet). */
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
/** Add a sign-in through `connector` to the signed-in user (spec/tenancy.md
 *  §6.2): a recent login first (the re-auth dialog asks), then the IdP; it
 *  comes back to /account with `?linked=<id>` or `?link_error=<code>`. */
export async function startLink(connector: string): Promise<void> {
  const { redirect } = await request<{ redirect: string }>("POST", `/auth/link/c/${encodeURIComponent(connector)}/start`, {
    return_to: "/account",
  });
  window.location.assign(redirect);
}

/** Remove one of a user's sign-in methods (`<provider>:<subject>`); never
 *  the last. `endSessions` (an administrator's): sign the user out too. */
/** Delete a user and what is its alone (spec/tenancy.md §6.3); the counts
 *  of what went with it. */
export const deleteUser = (id: string) =>
  rpc(UserService.method.deleteUser, { id }).then((r) => ({
    identities: r.identities ?? 0,
    sessions: r.sessions ?? 0,
    tokens: r.tokens ?? 0,
    links: r.links ?? 0,
    teams: r.teams ?? 0,
    tenants: r.tenants ?? 0,
  }));

export const unlinkIdentity = (user: string, key: string, endSessions = false) =>
  rpc(UserService.method.unlinkIdentity, { id: user, key, end_sessions: endSessions || undefined }).then(none);

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
export const updateMe = (body: { default_project: string }) =>
  rpc(UserService.method.updateMe, body).then((r) => r.user as User | undefined);

/** Capability checks for the caller (spec §7.7): one boolean per check.
 * An action only a fresh login would allow (`reauth`, a step-up action
 * after the step-up window) counts as allowed: the request then asks for
 * the login (ReauthDialog) and is retried. */
export async function checkAccess(checks: { action: string; resource: EntityRef }[]): Promise<boolean[]> {
  if (checks.length === 0) return [];
  const r = await rpc(AuthzService.method.check, { checks }, { idempotent: true });
  return checks.map((_, i) => !!r.results?.[i]?.allowed || !!r.results?.[i]?.reauth);
}

/** The actions allowed on each resource, in one call (spec/clustering-ui.md
 * §3.5): `result[i]` lists those of `actions` allowed on `resources[i]`. An
 * action that doesn't apply to a resource's type is left out. */
export async function allowedActions(actions: string[], resources: EntityRef[]): Promise<string[][]> {
  if (actions.length === 0 || resources.length === 0) return resources.map(() => []);
  const r = await rpc(AuthzService.method.allowed, { actions, resources }, { idempotent: true });
  // As checkAccess: what a fresh login would allow counts.
  return resources.map((_, i) => [...(r.allowed?.[i]?.actions ?? []), ...(r.reauth?.[i]?.actions ?? [])]);
}

// ---- VMs --------------------------------------------------------------------

export const listVms = (project?: string | null) =>
  rpc(VmService.method.listVms, project ? { project } : {}).then((r) => (r.vms ?? []) as VmResponse[]);

export async function getVm(id: string): Promise<VmResponse> {
  try {
    const r = (await rpc(VmService.method.getVm, { id })) as { vm?: VmResponse };
    if (!r?.vm) throw new Error("VM not found");
    return r.vm;
  } catch (e) {
    if (e instanceof ApiRequestError && e.status === 404) throw new Error("VM not found");
    throw e;
  }
}

/** A VM write's answer: `{vm, settled}`, or `{}` when it answered 204 (gone). */
const vmWrite = (p: Promise<VmWriteResponse | undefined>) => p.then((r) => r ?? {});

export const createVm = (req: CreateVmRequest) =>
  vmWrite(rpc(VmService.method.createVm, req as CreateVmRequestJson) as Promise<VmWriteResponse>);

/** Lifecycle calls change the VM's desired state; `wait` holds the answer
 * until the controller got it there, or reports why it couldn't
 * (spec/reconciliation.md §12.3). */
const WAIT_SECS = 60;

/** Place a stopped (or lost) VM on `node`, or where there is room (spec/storage.md S10). */
export const placeVm = (id: string, node?: string) =>
  rpc(VmService.method.placeVm, { id, node }).then((r) => r.vm as VmResponse);

export const startVm = (id: string) =>
  vmWrite(rpc(VmService.method.startVm, { id, wait_secs: WAIT_SECS }) as Promise<VmWriteResponse>);

/** Stop a VM. With `gracefulTimeoutSecs`, press the guest's power button
 * first and stop it hard only if it is still running after that long. */
export function stopVm(id: string, gracefulTimeoutSecs?: number): Promise<VmWriteResponse> {
  const wait = Math.min(300, Math.max(WAIT_SECS, (gracefulTimeoutSecs ?? 0) + 20));
  return vmWrite(
    rpc(VmService.method.stopVm, { id, wait_secs: wait, graceful_timeout_secs: gracefulTimeoutSecs }) as Promise<VmWriteResponse>,
  );
}

export const pauseVm = (id: string) =>
  vmWrite(rpc(VmService.method.pauseVm, { id, wait_secs: WAIT_SECS }) as Promise<VmWriteResponse>);

/** Another kernel image, and/or following the guest's kernel, from the
 * next start (spec/kernel-boot.md K6, K11). Setting `kernel` without
 * `follow` turns following off. */
export function setVmKernel(id: string, change: { kernel?: string; follow?: boolean }) {
  const vm = { kernel: change.kernel, kernel_from_disk: change.follow };
  return vmWrite(rpc(VmService.method.updateVm, { id, change: vm, update_mask: maskOf(vm) }) as Promise<VmWriteResponse>);
}

/** Extract the kernel on a stopped VM's disk as a kernel image it owns (§10.5). */
export const extractVmKernel = (id: string) =>
  rpc(ImageService.method.extractVmKernel, { id }).then((r) => r.image as ImageInfo);

/** Delete a VM: `undefined` once it is gone, else the VM still being torn down. */
export const deleteVm = (id: string, keepDisk = false) =>
  rpc(VmService.method.deleteVm, { id, wait_secs: WAIT_SECS, keep_disk: keepDisk }).then((r) => r.vm as VmResponse | undefined);

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
  rpc(VmService.method.listVmEvents, { id }).then((r) => ({ events: (r.events ?? []) as VmEvent[] }));

/** Follow an object's events (`WatchService.WatchEvents`): `onEvents` gets
 * the history, then the list again with each new event, until `signal`
 * aborts. Reconnects (with a fresh history) when the stream ends. */
export async function followEvents(kind: "vm" | "disk" | "image" | "network", id: string, onEvents: (events: VmEvent[]) => void, signal: AbortSignal): Promise<void> {
  const client = createClient(WatchService, transport);
  const kinds = { vm: WatchKind.VM, disk: WatchKind.DISK, image: WatchKind.IMAGE, network: WatchKind.NETWORK };
  while (!signal.aborted) {
    let events: VmEvent[] = [];
    let synced = false;
    try {
      for await (const msg of client.watchEvents({ kind: kinds[kind], id }, { signal })) {
        const r = toJson(WatchEventsResponseSchema, msg) as WatchEventsResponseJson;
        if (r.type === "WATCH_EVENT_TYPE_SYNCED") {
          synced = true;
          onEvents(events);
        } else if (r.type === "WATCH_EVENT_TYPE_ADDED" && r.event) {
          events = [...events, r.event as VmEvent];
          if (synced) onEvents(events);
        } else if (r.type === "WATCH_EVENT_TYPE_EXPIRED") break;
      }
    } catch {
      /* gone, refused or cut off: try again below */
    }
    if (signal.aborted) return;
    await new Promise((r) => setTimeout(r, 3000));
  }
}

/** A single-use console ticket for the WebSocket (spec §5.6). */
export const consoleTicket = (id: string) =>
  rpc(VmService.method.createConsoleTicket, { id }).then((r) => ({ ticket: r.ticket ?? "", expires_in: r.expires_in ?? 0 }));

// ---- guest credentials ------------------------------------------------------

/** The caller's own SSH public keys from their home directory (local accounts only). */
export interface MySshKeys {
  available: boolean;
  username?: string;
  keys: string[];
  reason?: string;
  /** `local`, or `connector:<id>`: keys its directory holds. */
  source?: string;
}
/** Proto JSON leaves out a false `available` and empty `keys`. */
export const mySshKeys = () =>
  rpc(UserService.method.getMySshKeys).then((r): MySshKeys => ({ ...r, available: !!r.available, keys: r.keys ?? [] }));

// The storage answers are in the proto3 JSON shape (types.ts): each wraps
// its object (`{credential}`, `{disk}`, …) and lists (`{disks}`, `{items}`).

export const listCredentials = (project?: string | null) =>
  rpc(CredentialService.method.listCredentials, { project: project || undefined }).then((r) => (r.credentials ?? []) as CredentialInfo[]);
export const createCredential = (req: CreateCredentialRequest) =>
  rpc(CredentialService.method.createCredential, req).then((r) => r.credential as CredentialInfo | undefined);
/** The fields `req` sets are changed (an empty key list clears the keys). */
export const updateCredential = (username: string, req: UpdateCredentialRequest, project?: string | null) =>
  rpc(CredentialService.method.updateCredential, { ...req, username, project: project || undefined, update_mask: maskOf(req) }).then(
    (r) => r.credential as CredentialInfo | undefined,
  );
export const deleteCredential = (username: string, project?: string | null) =>
  rpc(CredentialService.method.deleteCredential, { username, project: project || undefined }).then(none);

// ---- networks ---------------------------------------------------------------
// glidex.network.v1 (types.ts): networks are keyed by id (a name works
// too); answers wrap their object (`{network}`, `{router}`) and lists
// (`{networks}`, `{routers}`, `{shares}`).

export const listNetworks = () => rpc(NetworkService.method.listNetworks).then((r) => (r.networks ?? []) as Network[]);
/** By id, or by name (409 `ambiguous` when several networks the caller can see have it). */
export const getNetwork = (id: string) => rpc(NetworkService.method.getNetwork, { id }).then((r) => r.network as Network);
export const createNetwork = (req: CreateNetworkRequest) =>
  rpc(NetworkService.method.createNetwork, req).then((r) => r.network as Network);
/** `undefined` when gone, else the network its controller is still deleting. */
export const deleteNetwork = (id: string) =>
  rpc(NetworkService.method.deleteNetwork, { id }).then((r) => r.network as Network | undefined);
export const networkEvents = (id: string) =>
  rpc(NetworkService.method.listNetworkEvents, { id }).then((r) => (r.events ?? []) as StorageEvent[]);
/** Which projects and tenants may use a host network (replaces the grants). */
export const grantNetwork = (id: string, req: { grants?: string[]; tenants?: string[]; all_projects?: boolean }) =>
  rpc(NetworkService.method.grantNetwork, { ...req, id }).then((r) => r.network as Network);
export const createProjectNetwork = (project: string, req: Omit<CreateNetworkRequest, "bridge" | "physnet" | "grants" | "tenants" | "all_projects">) =>
  rpc(NetworkService.method.createProjectNetwork, { ...req, project }).then((r) => r.network as Network);
export const listRouters = (project: string) =>
  rpc(RouterService.method.listRouters, { project }).then((r) => (r.routers ?? []) as Router[]);
export const getRouter = (project: string, router: string) =>
  rpc(RouterService.method.getRouter, { project, router }).then((r) => r.router as Router);
export const createRouter = (project: string, spec: RouterSpec) =>
  rpc(RouterService.method.createRouter, { ...spec, project }).then((r) => r.router as Router);
/** By id or name. */
export const deleteRouter = (project: string, router: string) => rpc(RouterService.method.deleteRouter, { project, router }).then(none);
/** Put an existing NAT network (id) on one of the project's routers (id), or (`null`) back on the shared edge. */
export const setNetworkRouter = (project: string, network: string, router: string | null) =>
  rpc(NetworkService.method.setNetworkRouter, { project, network, router: router ?? undefined }).then((r) => r.network as Network);
export const offerNetworkShare = (network: string, project: string) =>
  rpc(NetworkShareService.method.offerNetworkShare, { id: network, project }).then(none);
export const unshareNetwork = (network: string, project: string) =>
  rpc(NetworkShareService.method.unshareNetwork, { id: network, project }).then(none);
export const listNetworkShares = (project: string) =>
  rpc(NetworkShareService.method.listNetworkShares, { project }).then((r) => (r.shares ?? []) as NetworkShare[]);
export const acceptNetworkShare = (project: string, network: string) =>
  rpc(NetworkShareService.method.acceptNetworkShare, { project, network }).then((r) => r.network as Network);
export const leaveNetworkShare = (project: string, network: string) =>
  rpc(NetworkShareService.method.leaveNetworkShare, { project, network }).then(none);

export const ovsStatus = () => rpc(HostNetworkService.method.getOvsStatus).then((r) => r as OvsStatus);

export const installOvs = (profile: OvsProfile, confirm: boolean) =>
  rpc(HostNetworkService.method.installOvs, { profile, confirm }).then((r) => r as InstallOvsResponse);

/** Initialize DPDK: hugepage memory per NUMA node (`1024` or `1024,1024`). */
export const initDpdk = (socket_mem: string, confirm: boolean, pmd_cpu_mask?: string) =>
  rpc(HostNetworkService.method.initDpdk, { socket_mem, pmd_cpu_mask, confirm }).then(none);

export const listBridges = () => rpc(HostNetworkService.method.listBridges).then((r) => (r.bridges ?? []) as BridgeRecord[]);
export const createBridge = (name: string, datapath: Datapath) =>
  rpc(HostNetworkService.method.createBridge, { name, datapath }).then((r) => r.bridge as BridgeRecord);
export const deleteBridge = (name: string) => rpc(HostNetworkService.method.deleteBridge, { name }).then(none);
export const listUplinks = (bridge: string) =>
  rpc(HostNetworkService.method.listUplinks, { bridge }).then((r) => (r.uplinks ?? []) as UplinkResult[]);
const uplinkOf = (r: { uplink?: unknown }) => r.uplink as UplinkResult;
/** Add an uplink; with `migrate_ip` the host's address moves to the bridge and must be committed. */
export const addUplink = (bridge: string, body: { name: string; migrate_ip: boolean; confirm: boolean; accept_dhcp: boolean } & UplinkKind) =>
  rpc(HostNetworkService.method.ensureUplink, { ...body, bridge }).then(uplinkOf);
export const commitUplink = (bridge: string, name: string, token: string) =>
  rpc(HostNetworkService.method.commitUplink, { bridge, name, token }).then(uplinkOf);
export const reapplyUplink = (bridge: string, name: string) =>
  rpc(HostNetworkService.method.reapplyUplink, { bridge, name }).then(uplinkOf);
export const deleteUplink = (bridge: string, name: string) => rpc(HostNetworkService.method.deleteUplink, { bridge, name }).then(none);

/** The host's PCI devices (glidex.host.v1: `{devices}`). */
export const listPciDevices = () => rpc(PciService.method.listPciDevices).then((r) => (r.devices ?? []) as PciDevice[]);

// ---- images and disks ---------------------------------------------------------

/** The cloud image catalog for `arch` (`x86_64`, `aarch64`; default: the server's). */
export const imageCatalog = (arch?: string) =>
  rpc(ImageService.method.getImageCatalog, { arch: arch ? archToProto(arch) : undefined }).then((r) => (r.items ?? []) as CatalogItem[]);
export const firmwareCatalog = (arch?: string) =>
  rpc(ImageService.method.getFirmwareCatalog, { arch: arch ? archToProto(arch) : undefined }).then(
    (r) => (r.items ?? []) as FirmwareCatalogItem[],
  );
export const listImages = () => rpc(ImageService.method.listImages).then((r) => (r.images ?? []) as ImageInfo[]);
export const getImage = (id: string) => rpc(ImageService.method.getImage, { id }).then((r) => r.image as ImageInfo);
/** A new download, or the one already in flight: `{image, created}`. */
export const pullImage = (req: PullImageRequest) => rpc(ImageService.method.pullImage, req).then((r) => r as PullImageResponse);
/** Keep a copy on a node ahead of need, or let one go (§9.2.2). */
export const addImageCopy = (id: string, node: string) =>
  rpc(ImageService.method.addImageCopy, { id, node }).then((r) => r.image as ImageInfo | undefined);
export const removeImageCopy = (id: string, node: string) =>
  rpc(ImageService.method.removeImageCopy, { id, node }).then((r) => r.image as ImageInfo | undefined);
/** The node deletes the unrecorded image files it reported (§9.2.3). */
export const deleteSpareImageFiles = (node: string) =>
  rpc(NodeService.method.deleteSpareImageFiles, { id: node }).then((r) => r as { node?: string; files?: number; bytes?: U64 });
/** `undefined` when gone, else the image its controller is still deleting. */
export const deleteImage = (id: string) => rpc(ImageService.method.deleteImage, { id }).then((r) => r.image as ImageInfo | undefined);
/** Download a failed image again (spec/reconciliation.md D19). */
export const retryImage = (id: string) => rpc(ImageService.method.retryImage, { id }).then((r) => r.image as ImageInfo | undefined);
export const imageEvents = (id: string) =>
  rpc(ImageService.method.listImageEvents, { id }).then((r) => (r.events ?? []) as StorageEvent[]);

export const listDatastores = () =>
  rpc(DatastoreService.method.listDatastores).then((r) => (r.datastores ?? []) as Datastore[]);
export const getDatastore = (id: string) => rpc(DatastoreService.method.getDatastore, { id }).then((r) => r.datastore as Datastore);
export const createDatastore = ({ shared, ...req }: DatastoreInput) =>
  rpc(DatastoreService.method.createDatastore, { ...req, driver: shared ? "DATASTORE_DRIVER_SHARED_DIR" : "DATASTORE_DRIVER_DIR" }).then(
    (r) => r.datastore as Datastore,
  );
/** Changes the fields `req` has. */
export const updateDatastore = (id: string, req: DatastoreInput) =>
  rpc(DatastoreService.method.updateDatastore, { id, ...req, update_mask: maskOf(req) }).then((r) => r.datastore as Datastore);
export const deleteDatastore = (id: string) => rpc(DatastoreService.method.deleteDatastore, { id }).then(() => undefined);
/** Move a disk to another datastore, offline (spec/storage.md §8.3). */
export const moveDisk = (id: string, datastore: string, flatten?: boolean) =>
  diskWrite(rpc(DiskService.method.moveDisk, { id, datastore, flatten: flatten || undefined, wait_secs: WAIT_SECS }));
export const listDisks = (project?: string | null) =>
  rpc(DiskService.method.listDisks, { project: project || undefined }).then((r) => (r.disks ?? []) as DiskInfo[]);
/** By id, or by name (unique per tenant; a name two of the caller's tenants share answers 409 `ambiguous`). */
export const getDisk = (id: string) => rpc(DiskService.method.getDisk, { id }).then((r) => r.disk as DiskInfo);
// Disk writes are carried out by the disk controller (spec/reconciliation.md
// §10.1): wait for it, and show what is still pending if it isn't done.
// They answer `{disk, settled}`; `{}` when it is gone meanwhile.
const diskWrite = (p: Promise<unknown>) => p.then((r) => r as DiskWriteResponse);
export const createDisk = (req: CreateDiskRequest) =>
  diskWrite(rpc(DiskService.method.createDisk, { ...req, size_gib: u64(req.size_gib), wait_secs: WAIT_SECS }));
export const resizeDisk = (id: string, sizeGib: number, extendRoot?: boolean) =>
  diskWrite(rpc(DiskService.method.resizeDisk, { id, size_gib: u64(sizeGib), extend_root: extendRoot, wait_secs: WAIT_SECS }));
export const extendRoot = (id: string, mode: ExtendMode) =>
  diskWrite(rpc(DiskService.method.extendRoot, { id, mode, wait_secs: WAIT_SECS }));
/** `undefined` when gone, else the disk, deleted once its operation finishes. */
export const deleteDisk = (id: string) => rpc(DiskService.method.deleteDisk, { id }).then((r) => r.disk as DiskInfo | undefined);
export const diskEvents = (id: string) =>
  rpc(DiskService.method.listDiskEvents, { id }).then((r) => (r.events ?? []) as StorageEvent[]);

// ---- projects and role links ------------------------------------------------
// The access answers are in the proto3 JSON shape (types.ts): each wraps
// its object (`{project}`, `{binding}`, …) and lists (`{projects}`, …).

export const listProjects = () => rpc(ProjectService.method.listProjects).then((r) => (r.projects ?? []) as ProjectView[]);
export const getProject = (id: string) => rpc(ProjectService.method.getProject, { id }).then((r) => r.project as ProjectView);
export const createProject = (req: { name: string; description?: string; quotas?: Quotas; owners?: string[]; tenant?: string }) =>
  rpc(ProjectService.method.createProject, req as CreateProjectRequestJson).then((r) => r.project as Project);
/** Changes the fields present; `quotas` replaces the limits (an absent one is unlimited). */
export const updateProject = (id: string, req: { description?: string; quotas?: Quotas }) =>
  rpc(ProjectService.method.updateProject, { ...(req as UpdateProjectRequestJson), id, update_mask: maskOf(req) }).then(
    (r) => r.project as Project,
  );
export const deleteProject = (id: string) => rpc(ProjectService.method.deleteProject, { id }).then(none);

export const listBindings = (project: string) =>
  rpc(ProjectService.method.listProjectBindings, { id: project }).then((r) => (r.bindings ?? []) as Binding[]);
export const addBinding = (project: string, role: string, principal: EntityRef) =>
  rpc(ProjectService.method.addProjectBinding, { id: project, role, principal }).then((r) => r.binding as Binding);
export const removeBinding = (project: string, link: string) =>
  rpc(ProjectService.method.removeProjectBinding, { id: project, link }).then(none);

/** Every system binding, or with `node` only those on that node's host. */
export const listSystemBindings = (node?: string) =>
  rpc(SystemBindingService.method.listSystemBindings, node ? { node } : {}).then((r) => (r.bindings ?? []) as Binding[]);
/** A system role, cluster-wide or (with `node`) on one node's host only. */
export const addSystemBinding = (role: string, principal: EntityRef, node?: string) =>
  rpc(SystemBindingService.method.addSystemBinding, node ? { role, principal, node } : { role, principal }).then(
    (r) => r.binding as Binding,
  );
export const removeSystemBinding = (link: string) => rpc(SystemBindingService.method.removeSystemBinding, { link }).then(none);

// ---- users, teams and tokens --------------------------------------------------

export const listUsers = () => rpc(UserService.method.listUsers).then((r) => (r.users ?? []) as UserView[]);
export const updateUser = (id: string, req: { display_name?: string; disabled?: boolean; default_project?: string }) =>
  rpc(UserService.method.updateUser, { ...req, id, update_mask: maskOf(req) }).then((r) => r.user as User);

export const listTeams = () => rpc(TeamService.method.listTeams).then((r) => (r.teams ?? []) as Team[]);
export const createTeam = (name: string) => rpc(TeamService.method.createTeam, { name }).then((r) => r.team as Team);
export const deleteTeam = (id: string) => rpc(TeamService.method.deleteTeam, { id }).then(none);
export const addTeamMember = (team: string, user: string) =>
  rpc(TeamService.method.addTeamMember, { id: team, user }).then((r) => r.team as Team);
export const removeTeamMember = (team: string, user: string) =>
  rpc(TeamService.method.removeTeamMember, { id: team, user }).then((r) => r.team as Team);

export const listTokens = () => rpc(TokenService.method.listTokens).then((r) => (r.tokens ?? []) as ApiToken[]);
export const createToken = (req: CreateTokenRequest) => rpc(TokenService.method.createToken, req).then((r) => r as CreatedToken);
export const revokeToken = (id: string) => rpc(TokenService.method.revokeToken, { id }).then(none);

// ---- policies and audit -------------------------------------------------------

export const listPolicies = () =>
  rpc(PolicyService.method.listPolicies).then((r) => ({
    policies: (r.policies ?? []) as PolicyInfo[],
    site: (r.site ?? []) as SitePolicy[],
  }));
export const policyVersions = (id: string) =>
  rpc(PolicyService.method.listPolicyVersions, { id }).then((r) => (r.versions ?? []) as SitePolicyVersion[]);
export const validatePolicy = (id: string, text: string) =>
  rpc(PolicyService.method.validatePolicy, { id, text }).then((r) => ({ valid: !!r.valid, errors: r.errors }));
export const simulatePolicy = (changes: PolicyChange[], requests: SimulationRequest[]) =>
  rpc(PolicyService.method.simulatePolicy, { changes, requests }).then((r) => ({ results: (r.results ?? []) as SimulationResult[] }));
/** Save a site policy; `version` is the one it replaces (0 for a new one).
 * Answers the saved policy, or nothing when the policy was removed. */
export const putPolicy = (id: string, req: { text: string; description: string; enabled: boolean; version: number }) =>
  rpc(PolicyService.method.putPolicy, { ...req, id, version: String(req.version) }).then((r) => (r.policy as SitePolicy | undefined) ?? null);
export const deletePolicy = (id: string, version: number) =>
  rpc(PolicyService.method.deletePolicy, { id, version: String(version) }).then(none);

/** `since` is Unix milliseconds. */
export function readAudit(q: { project?: string; user?: string; since?: number; limit?: number }): Promise<AuditEntry[]> {
  return rpc(AuditService.method.readAudit, {
    project: q.project || undefined,
    user: q.user || undefined,
    since: q.since ? new Date(q.since).toISOString() : undefined,
    limit: q.limit || undefined,
  }).then((r) => (r.entries ?? []) as AuditEntry[]);
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

/** The CSV export's query string (REST only). */
function usageQuery(q: UsageQuery, extra: Record<string, string> = {}): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries({ ...q, ...extra })) if (v) p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
}

/** A comma-separated list (`a,b`), as REST's query takes it. */
const listOf = (s?: string) => (s ? s.split(",").map((x) => x.trim()).filter(Boolean) : undefined);
/** A query time: RFC 3339, or Unix seconds as REST also took. */
const timeOf = (t?: string) => (t ? (/^\d+$/.test(t) ? new Date(Number(t) * 1000).toISOString() : t) : undefined);
/** `vm` → `GROUP_KEY_VM`. */
const groupKeys = (g?: string) =>
  listOf(g)?.map((k) => (k.startsWith("GROUP_KEY_") ? k : `GROUP_KEY_${k.toUpperCase()}`) as GroupKeyJson);

function usageRequest(q: UsageQuery): GetUsageRequestJson {
  return {
    projects: listOf(q.project),
    query: {
      from: timeOf(q.from),
      to: timeOf(q.to),
      month: q.month || undefined,
      granularity: q.granularity ? (`GRANULARITY_${q.granularity.toUpperCase()}` as GranularityJson) : undefined,
      group_by: groupKeys(q.group_by),
      meters: listOf(q.meters),
    },
  };
}

/** The bandwidth, disk I/O and compute reports' request (they take no granularity or meters). */
function reportRequest(q: UsageQuery): GetBandwidthReportRequestJson {
  return {
    projects: listOf(q.project),
    query: { month: q.month || undefined, from: timeOf(q.from), to: timeOf(q.to), group_by: groupKeys(q.group_by) },
  };
}

export const getUsage = (q: UsageQuery) => rpc(UsageService.method.getUsage, usageRequest(q)).then((r) => r as UsageResponse);
export const getBandwidth = (q: UsageQuery) =>
  rpc(UsageService.method.getBandwidthReport, reportRequest(q)).then((r) => r as unknown as RateResponse);
export const getDiskIo = (q: UsageQuery) =>
  rpc(UsageService.method.getDiskIoReport, reportRequest(q)).then((r) => r as unknown as RateResponse);
export const getCompute = (q: UsageQuery) =>
  rpc(UsageService.method.getComputeReport, reportRequest(q)).then((r) => r as unknown as RateResponse);
/** A same-origin link that downloads the CSV export (sent with the session cookie; REST only). */
export const usageCsvUrl = (kind: "usage" | "bandwidth" | "disk-io" | "compute", q: UsageQuery) =>
  `${API_BASE}/usage${kind === "usage" ? "" : `/${kind}`}${usageQuery(q, { format: "csv" })}`;
export const getVmBandwidth = (id: string) =>
  rpc(MetricsService.method.getVmBandwidth, { id, query: { p95: true } }).then((r) => r as unknown as SeriesResponse);
export const getVmIo = (id: string) =>
  rpc(MetricsService.method.getVmIo, { id, query: { p95: true } }).then((r) => r as unknown as SeriesResponse);
export const getVmCompute = (id: string) =>
  rpc(MetricsService.method.getVmCompute, { id, query: { p95: true } }).then((r) => r as unknown as SeriesResponse);
export const getVmStats = (id: string) => rpc(MetricsService.method.getVmStats, { id }).then((r) => r as unknown as VmStats);

// ---- cluster (spec/clustering.md §5, §7; glidex.cluster.v1) ------------------
// The cluster, node, import and departure answers are in the proto3 JSON
// shape (types.ts): lists and records are wrapped (`{nodes}`, `{node}`,
// `{plans}`, `{plan}`, `{departures}`); requests send enums by full name.

export type { ClusterNodeRow, ClusterStatus } from "./types";

/** `ConfigureNodeOvn`'s answer; `wanted`/`current` are `system` or `netdev`. */
export interface ConfigureNodeOvnResponse {
  node?: string;
  br_int_datapath?: BrIntDatapath;
  wanted?: string;
  current?: string;
  bridge_mappings?: Record<string, string>;
  physnets?: Record<string, string>;
}

export const clusterStatus = () => rpc(ClusterService.method.getClusterStatus, { ports: true }).then((r) => r as unknown as ClusterStatus);
/** The summary without the port checks (which connect to every node). */
export const clusterSummary = () => rpc(ClusterService.method.getClusterStatus).then((r) => r as unknown as ClusterStatus);
export const listNodes = () => rpc(NodeService.method.listNodes).then((r) => (r.nodes ?? []) as unknown as NodeRecord[]);
export interface PreviewRequest {
  project?: string;
  vcpu_count: number;
  mem_size_mib: number;
  hypervisor?: ProtoHypervisor;
  node?: string;
  networks?: { network: string }[];
  vfio_devices?: string[];
  /** Devices asked for by pool (spec/host-topology.md §9). */
  devices?: { pool: string; count?: number; same_switch?: boolean }[];
  data_disks?: string[];
  /** The root disk's image: its architecture counts (spec/clustering.md §9.2.1). */
  image?: string;
}
/** Where a VM would go, node by node (spec/clustering-ui.md §5.1). */
export const previewPlacement = (req: PreviewRequest) =>
  rpc(VmService.method.previewPlacement, req).then((r) => r as unknown as PlacementPreview);
export const getNode = (id: string) => rpc(NodeService.method.getNode, { id }).then((r) => r.node as unknown as NodeRecord);
/** A node's NUMA and PCIe topology, read on that node (spec/host-topology.md §3.2). */
export const getNodeTopology = (id: string) =>
  rpc(NodeService.method.getNodeTopology, { id }).then((r) => r.topology as unknown as HostTopology);
/** The device pools a project may use and how many are free (spec/host-topology.md §9.1). */
export const listProjectPciPools = (project: string) =>
  rpc(NodeService.method.listPciPools, { project }).then((r) => (r.pools ?? []) as PciPoolJson[]);
/** Bind a device's IOMMU group to vfio-pci on its node, or put it back (spec/host-topology.md §5.3). */
export const prepareDevice = (id: string, address: string) => rpc(NodeService.method.prepareDevice, { id, address });
export const releaseDevice = (id: string, address: string) => rpc(NodeService.method.releaseDevice, { id, address });
/** Give a PF `vfs` SR-IOV virtual functions on its node (§10.1); 0 removes them. */
export const setSriovVfs = (id: string, address: string, vfs: number) => rpc(NodeService.method.setSriovVfs, { id, address, vfs });
/** No new VM gets a cordoned device (§5.4). */
export const cordonDevice = (id: string, address: string) => rpc(NodeService.method.cordonDevice, { id, address });
export const uncordonDevice = (id: string, address: string) => rpc(NodeService.method.uncordonDevice, { id, address });
/** A node's br-int datapath (spec/clustering.md §11.6). */
export const setNodeBrInt = (id: string, br_int_datapath: BrIntDatapath) =>
  rpc(NodeService.method.configureNodeOvn, { id, br_int_datapath }).then((r) => r as ConfigureNodeOvnResponse);
/** A node's physical networks: physnet → bridge (replaces its own; the file's stay). */
export const setNodeMappings = (id: string, bridge_mappings: Record<string, string>) =>
  rpc(NodeService.method.configureNodeOvn, { id, bridge_mappings, set_bridge_mappings: true }).then((r) => r as ConfigureNodeOvnResponse);

// ---- membership (spec/clustering-ui.md §7) ---------------------------------------

export const createJoinToken = (req: { role: NodeRole; ttl_secs: number; allow_import: boolean }) =>
  rpc(ClusterService.method.createJoinToken, { ...req, ttl_secs: u64(req.ttl_secs) }).then((r) => r as unknown as IssuedToken);
export const removeNode = (id: string, force = false) =>
  rpc(NodeService.method.removeNode, { id, force }).then((r) => r as Record<string, unknown>);
export const forgetNode = (id: string, fenced: boolean, force = false) =>
  rpc(NodeService.method.forgetNode, { id, fenced, force }).then((r) => r as Record<string, unknown>);
export const purgeNode = (id: string) => rpc(NodeService.method.purgeNode, { id }).then((r) => r as Record<string, unknown>);
export const rejoinToken = (id: string, req: { raft_intact: boolean; ttl_secs: number }) =>
  rpc(NodeService.method.createRejoinToken, { ...req, id, ttl_secs: u64(req.ttl_secs) }).then((r) => r as unknown as IssuedToken);
export const promoteNodes = (nodes: string[], force = false) =>
  rpc(ClusterService.method.promoteNodes, { nodes, force }).then((r) => r as Record<string, unknown>);
export const detachNode = (id: string, req: { map_networks: Record<string, string>; with_access: boolean }) =>
  rpc(NodeService.method.detachNode, { ...req, id }).then((r) => r as Record<string, unknown>);
export const abortDetach = (id: string) => rpc(NodeService.method.abortDetach, { id }).then((r) => r as Record<string, unknown>);
export const listDepartures = () => rpc(NodeService.method.listDepartures).then((r) => (r.departures ?? []) as unknown as Departure[]);
export const clusterOvn = () => rpc(ClusterService.method.getOvnOverview).then((r) => r as unknown as OvnOverview);
export const clusterTunnels = () =>
  rpc(ClusterService.method.getOvnTunnels).then((r) => r as unknown as { enabled?: boolean; nodes?: NodeTunnels[] });
/** A shared edge that should work for this cluster, from what nodes report (spec/clustering-ui.md §8a). */
export const getEdgeDefaults = () => rpc(ClusterService.method.getEdgeDefaults).then((r) => r as unknown as EdgeDefaults);
export const getOvnSettings = () => rpc(ClusterService.method.getOvnSettings).then((r) => r as unknown as OvnSettingsView);
/** Replaces the whole setting; only the request's fields are sent. */
export const putOvnSettings = (s: PutOvnSettings) =>
  rpc(ClusterService.method.putOvnSettings, {
    enabled: s.enabled,
    underlay_mtu: s.underlay_mtu,
    nat_supernet: s.nat_supernet,
    edge: s.edge,
    snat_ct_zones: s.snat_ct_zones,
    dns_servers: s.dns_servers,
  } as PutOvnSettingsRequestJson).then((r) => r as unknown as OvnSettingsView);
export const networkOvn = (id: string) => rpc(NetworkService.method.getNetworkOvn, { id }).then((r) => r as unknown as NetworkOvn);

// ---- cluster lifecycle (spec/clustering-ui.md §7.5–7.8) ---------------------------

export const initCluster = (req: { advertise?: string; tunnel_ip?: string }) =>
  rpc(ClusterService.method.initCluster, { ...req, force: true }).then((r) => r as Record<string, unknown>);
export const rotateCa = (grace_secs?: number) =>
  rpc(ClusterService.method.rotateCa, { grace_secs: grace_secs ? String(grace_secs) : undefined }).then((r) => r as Record<string, unknown>);
export const dissolveCluster = () => rpc(ClusterService.method.dissolveCluster, { force: true }).then((r) => r as Record<string, unknown>);
export const leaveCluster = () => rpc(ClusterService.method.leaveCluster).then((r) => r as Record<string, unknown>);
/** A same-origin link that downloads the cluster snapshot (with the session cookie; REST only). */
export const clusterSnapshotUrl = () => `${API_BASE}/cluster/snapshot`;
export const listImports = () => rpc(ImportService.method.listImports).then((r) => (r.plans ?? []) as unknown as ImportPlanView[]);
export const getImport = (plan: string) => rpc(ImportService.method.getImport, { plan }).then((r) => r.plan as unknown as ImportPlanView);
export const checkImport = (plan: string, m: ImportMappings) =>
  rpc(ImportService.method.approveImport, { ...m, plan, dry_run: true }).then((r) => r.plan as unknown as ImportPlanView);
export const approveImport = (plan: string, m: ImportMappings) =>
  rpc(ImportService.method.approveImport, { ...m, plan }).then((r) => r.plan as unknown as ImportPlanView);
export const rejectImport = (plan: string) => rpc(ImportService.method.rejectImport, { plan }).then(none);
/** `{node, phase, remaining}`: what still keeps it from being removed. */
export const drainNode = (id: string, drain: boolean) =>
  rpc(drain ? NodeService.method.drainNode : NodeService.method.undrainNode, { id }).then(
    (r) => r as { node?: string; phase?: NodePhase; remaining?: { vms?: string[]; disks?: string[]; networks?: string[] } },
  );

// ---- tenants and admissions (spec/tenancy.md §14) -----------------------------

/** Every tenant the caller may read, over all pages. */
export async function listTenants(): Promise<Tenant[]> {
  const out: Tenant[] = [];
  let token = "";
  do {
    const r = await rpc(TenantService.method.listTenants, { page_size: 500, page_token: token || undefined });
    out.push(...((r.tenants ?? []) as Tenant[]));
    token = r.next_page_token ?? "";
  } while (token);
  return out;
}
export const getTenant = (id: string) => rpc(TenantService.method.getTenant, { id }).then((r) => r.tenant as Tenant);
/** With `first_admin`, the first tenant-admin's invitation link comes back once. */
export const createTenant = (req: {
  slug: string;
  display_name?: string;
  operator?: boolean;
  first_admin?: { email: string; connector: string };
}) => rpc(TenantService.method.createTenant, req).then((r) => ({ tenant: r.tenant as Tenant, link: r.invitation_link }));
export const updateTenant = (id: string, req: { display_name?: string; operator?: boolean }) =>
  rpc(TenantService.method.updateTenant, { ...req, id }).then((r) => r.tenant as Tenant);
export const suspendTenant = (id: string) => rpc(TenantService.method.suspendTenant, { id }).then((r) => r as { tenant: Tenant });
export const resumeTenant = (id: string) => rpc(TenantService.method.resumeTenant, { id }).then((r) => r as { tenant: Tenant });
export const deleteTenant = (id: string) => rpc(TenantService.method.deleteTenant, { id }).then(none);

// ---- invitations (spec/tenancy.md §6.1) -----------------------------------

export type Invitation = InvitationJson;
export const listInvitations = (tenant: string) =>
  rpc(InvitationService.method.listInvitations, { tenant }).then((r) => (r.invitations ?? []) as Invitation[]);
/** The link is answered once; it carries the secret in its fragment. */
export const createInvitation = (req: { tenant: string; email: string; role: string; connector: string; days?: number }) =>
  rpc(InvitationService.method.createInvitation, req).then((r) => ({ invitation: r.invitation as Invitation, link: r.link ?? "" }));
export const revokeInvitation = (id: string) => rpc(InvitationService.method.revokeInvitation, { id }).then(none);
/** What an invitation link is for (public; needs `<id>.<secret>`). REST,
 *  like the rest of the login page. */
export const inspectInvitation = (invitation: string) =>
  request<InspectInvitationResponseJson>("POST", "/auth/invitation", { invitation }, { quiet401: true });
/** Start the invitation's login: the secret goes in the body, never a URL. */
export const startInvitedLogin = (connector: string, invitation: string) =>
  request<{ redirect: string }>("POST", `/auth/c/${encodeURIComponent(connector)}/start`, { invitation, return_to: "/" }, { quiet401: true });

/** A tenant's shares of cluster device pools on this cluster (spec/host-topology.md §9.1a). */
export type TenantDevicePool = TenantDevicePoolJson;
export const listTenantDevicePools = (id: string) =>
  rpc(TenantService.method.listDevicePools, { id }).then((r) => (r.pools ?? []) as TenantDevicePool[]);
/** Replaces the tenant's pools; only `name`, `from`, `max` and `projects` are read. */
export const setTenantDevicePools = (id: string, pools: TenantDevicePool[]) =>
  rpc(TenantService.method.setDevicePools, {
    id,
    pools: pools.map(({ name, from, max, projects }) => ({ name, from, max, projects })),
  }).then((r) => (r.pools ?? []) as TenantDevicePool[]);

export const listTenantMembers = (id: string) =>
  rpc(TenantService.method.listMembers, { id }).then((r) => (r.members ?? []) as TenantMember[]);
export const addTenantMember = (id: string, principal: EntityRef) => rpc(TenantService.method.addMember, { id, principal });
/** `principal` is a User or Team; the request names it `user:<id>` / `team:<id>`. */
export const removeTenantMember = (id: string, principal: EntityRef) =>
  rpc(TenantService.method.removeMember, {
    id,
    principal: `${principal.type.toLowerCase()}:${"id" in principal ? principal.id : ""}`,
  }).then(none);

export const listTenantBindings = (id: string) =>
  rpc(TenantService.method.listBindings, { id }).then((r) => (r.bindings ?? []) as TenantBinding[]);
export const addTenantBinding = (id: string, role: string, principal: EntityRef) =>
  rpc(TenantService.method.addBinding, { id, role, principal });
export const removeTenantBinding = (id: string, link: string) => rpc(TenantService.method.removeBinding, { id, link }).then(none);

export const listAdmissions = () => rpc(AdmissionService.method.listAdmissions).then((r) => (r.admissions ?? []) as Admission[]);
/** Admit a tenant or change its admission. An absent `cap` keeps the current
 * one (unlimited for a new admission); `{}` is unlimited. */
export const setAdmission = (tenant: string, req: { cap?: TenantQuotas; suspended?: boolean }) =>
  rpc(AdmissionService.method.setAdmission, { ...(req as SetAdmissionRequestJson), tenant }).then((r) => r.admission as Admission);
export const removeAdmission = (tenant: string) => rpc(AdmissionService.method.removeAdmission, { tenant }).then(none);

// ---- login connectors (spec/tenancy.md §5, §14) ------------------------------

export const listConnectors = () => rpc(ConnectorService.method.listConnectors).then((r) => (r.connectors ?? []) as Connector[]);
export const getConnector = (id: string) => rpc(ConnectorService.method.getConnector, { id }).then((r) => r.connector as Connector);
export const createConnector = (connector: ConnectorInput) =>
  rpc(ConnectorService.method.createConnector, { connector: connector as ConnectorJson }).then((r) => r.connector as Connector);
/** Only the fields present are changed (the mask is the fields `connector` sets, as REST derives it). */
export const updateConnector = (id: string, connector: Partial<ConnectorInput>) =>
  rpc(ConnectorService.method.updateConnector, { id, connector: connector as ConnectorJson, update_mask: maskOf(connector) }).then(
    (r) => r.connector as Connector,
  );
/** A pulled connector's run now (spec/tenancy.md §8.2); `confirm` applies one the safety stop blocked. */
export const syncConnector = (id: string, confirm = false) =>
  rpc(ConnectorService.method.syncConnector, confirm ? { id, confirm } : { id }).then((r) => (r.status ?? {}) as SyncStatus);
export const listScimTokens = (id: string) =>
  rpc(ConnectorService.method.listScimTokens, { id }).then((r) => ({ tokens: (r.tokens ?? []) as ScimToken[], path: r.path ?? "" }));
/** The token is in `secret`, once. */
export const createScimToken = (id: string, name: string, days?: number) =>
  rpc(ConnectorService.method.createScimToken, days ? { id, name, days } : { id, name }).then((r) => ({
    token: r.token as ScimToken,
    secret: r.secret ?? "",
    path: r.path ?? "",
  }));
export const revokeScimToken = (id: string, token: string) => rpc(ConnectorService.method.revokeScimToken, { id, token }).then(none);
/** With `users: "detach"`, the users and teams it provisions stay, as the directory's (§8.3). */
export const deleteConnector = (id: string, users?: "disable" | "detach") =>
  rpc(ConnectorService.method.deleteConnector, users ? { id, users } : { id }).then((r) => ({ users: r.users ?? 0, teams: r.teams ?? 0 }));
/** Write-only: the value is sealed server-side and never comes back. */
export const setConnectorSecret = (id: string, field: string, value: string) =>
  rpc(ConnectorService.method.setSecret, { id, field, value }).then((r) => r.connector as Connector);
/** Checks that need no user; reads only, so safe to retry. */
export const testConnector = (id: string) =>
  rpc(ConnectorService.method.testConnector, { id }).then((r) => (r.steps ?? []) as ConnectorTestStep[]);
