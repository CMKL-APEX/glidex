// The browser session (spec/security.md §5.5, §5.6): who is logged in,
// the selected project, and what the caller may do on the host. Pages
// read it through useSession(); the server still checks every request.
import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import * as api from "./api";
import { ApiRequestError } from "./api";
import type { AuthMethods, EntityRef, ProjectView, SingletonEntityType, WhoAmI } from "./types";
import Login from "./pages/Login";
import ReauthDialog from "./components/ReauthDialog";
import { Loading } from "./components/Loading";

/** Host-wide capabilities the UI shows or hides pages for, each with the
 * resource type it is checked on, as `appliesTo` in
 * policies/glidex.cedarschema has it: cluster-wide actions take `Cluster`,
 * the host's own networking `Host`, users and teams the host's
 * `Directory`, project creation a `Tenant` (spec/tenancy.md). A check on a
 * type the schema doesn't
 * list is never allowed, so a wrong entry hides the page from everyone
 * (tests/ui_capabilities.rs in glidex-control-plane compares the two). */
/** The entity a host capability is checked on: the host's own directory
 * (`local`), and the `default` tenant for creating projects. */
type HostResourceType = SingletonEntityType | "Directory" | "Tenant";

function hostResource(type: HostResourceType): EntityRef {
  if (type === "Directory") return { type, id: "local" };
  if (type === "Tenant") return { type, id: "default" };
  return { type };
}

const HOST_ACTION_RESOURCE = {
  createProject: "Tenant",
  listUsers: "Directory",
  manageUsers: "Directory",
  listTeams: "Directory",
  manageTeams: "Directory",
  readSystemBindings: "Cluster",
  manageSystemBindings: "Cluster",
  readPolicy: "Cluster",
  writePolicy: "Cluster",
  readAudit: "Cluster",
  readUsage: "Cluster",
  manageAnyTokens: "Directory",
  createNetwork: "Host",
  readCluster: "Cluster",
  // Datastores (spec/storage.md §13).
  readDatastore: "Cluster",
  manageDatastore: "Cluster",
  // Cluster operations (spec/clustering-ui.md §3.1).
  listNodes: "Cluster",
  removeNode: "Cluster",
  forgetNode: "Cluster",
  purgeNode: "Cluster",
  createRejoinToken: "Cluster",
  detachNode: "Cluster",
  promoteNode: "Cluster",
  createJoinToken: "Cluster",
  listImports: "Cluster",
  approveImport: "Cluster",
  rejectImport: "Cluster",
  rotateCa: "Cluster",
  dissolveCluster: "Cluster",
  snapshotCluster: "Cluster",
  initCluster: "Host",
  leaveCluster: "Host",
  // Directories this host serves (spec/tenancy.md D59).
  listDirectories: "Cluster",
  manageDirectories: "Cluster",
  configureOvn: "Cluster",
  // Tenants, admissions and login connectors (spec/tenancy.md §14); the
  // per-tenant actions are checked on each tenant by its page.
  listTenants: "Directory",
  createTenant: "Directory",
  listAdmissions: "Cluster",
  manageAdmissions: "Cluster",
  listConnectors: "Directory",
  createConnector: "Directory",
  updateConnector: "Directory",
  deleteConnector: "Directory",
  setConnectorSecret: "Directory",
  testConnector: "Directory",
  readScimTokens: "Directory",
  manageScimTokens: "Directory",
  syncConnector: "Directory",
  confirmConnectorSync: "Directory",
} as const satisfies Record<string, HostResourceType>;

export type HostAction = keyof typeof HOST_ACTION_RESOURCE;
const HOST_ACTIONS = Object.keys(HOST_ACTION_RESOURCE) as HostAction[];

export interface Session {
  me: WhoAmI;
  methods: AuthMethods;
  projects: ProjectView[];
  /** Selected project id (create bodies and `?project=` on lists). */
  project: string | null;
  selectProject: (id: string) => Promise<void>;
  projectName: (id: string | null | undefined) => string;
  host: Record<HostAction, boolean>;
  refresh: () => Promise<void>;
  /** Re-run the host capability checks only (after a policy change). */
  refreshCapabilities: () => Promise<void>;
  logout: () => Promise<void>;
}

const SessionContext = createContext<Session | null>(null);

export function useSession(): Session {
  const s = useContext(SessionContext);
  if (!s) throw new Error("useSession outside SessionProvider");
  return s;
}

/** Capability checks for the caller; `null` while loading. Re-runs when
 * the checks change. A failed check call counts as "not allowed". */
export function useCan(checks: { action: string; resource: EntityRef }[]): boolean[] | null {
  const key = JSON.stringify(checks);
  const [result, setResult] = useState<{ key: string; allowed: boolean[] } | null>(null);
  useEffect(() => {
    let live = true;
    const parsed = JSON.parse(key) as { action: string; resource: EntityRef }[];
    api
      .checkAccess(parsed)
      .then((allowed) => live && setResult({ key, allowed }))
      .catch(() => live && setResult({ key, allowed: parsed.map(() => false) }));
    return () => {
      live = false;
    };
  }, [key]);
  return result && result.key === key ? result.allowed : null;
}

/** What the caller may do host- and cluster-wide; all false if the check fails. */
async function hostCapabilities(): Promise<Record<HostAction, boolean>> {
  const allowed = await api
    .checkAccess(HOST_ACTIONS.map((action) => ({ action, resource: hostResource(HOST_ACTION_RESOURCE[action]) })))
    .catch(() => HOST_ACTIONS.map(() => false));
  return Object.fromEntries(HOST_ACTIONS.map((a, i) => [a, allowed[i]])) as Record<HostAction, boolean>;
}

const noHost = Object.fromEntries(HOST_ACTIONS.map((a) => [a, false])) as Record<HostAction, boolean>;
const PROJECT_KEY = "glidex.project";

type State =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "anonymous"; methods: AuthMethods }
  | { kind: "ready"; me: WhoAmI; methods: AuthMethods; projects: ProjectView[]; host: Record<HostAction, boolean> };

export function SessionProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<State>({ kind: "loading" });
  const [project, setProject] = useState<string | null>(null);
  const [reauth, setReauth] = useState<((ok: boolean) => void) | null>(null);

  const load = useCallback(async () => {
    let methods: AuthMethods;
    try {
      // `/login/<directory>`: that directory's login page (D61).
      const m = window.location.pathname.match(/^\/login\/([a-z0-9-]{1,32})\/?$/);
      methods = await api.authMethods(m?.[1]);
    } catch (e) {
      setState({ kind: "error", message: e instanceof Error ? e.message : String(e) });
      return;
    }
    let me: WhoAmI;
    try {
      me = await api.whoami();
    } catch (e) {
      if (e instanceof ApiRequestError && e.status === 401) {
        setState({ kind: "anonymous", methods });
      } else {
        setState({ kind: "error", message: e instanceof Error ? e.message : String(e) });
      }
      return;
    }
    const [projects, host] = await Promise.all([api.listProjects().catch(() => [] as ProjectView[]), hostCapabilities()]);
    setProject((current) => {
      const known = (id: string | null | undefined) => !!id && projects.some((p) => p.id === id);
      if (known(current)) return current;
      if (known(me.default_project)) return me.default_project ?? null;
      const stored = localStorage.getItem(PROJECT_KEY);
      if (known(stored)) return stored;
      return projects[0]?.id ?? null;
    });
    setState({ kind: "ready", me, methods, projects, host });
  }, []);

  const refreshCapabilities = useCallback(async () => {
    const host = await hostCapabilities();
    setState((s) => (s.kind === "ready" ? { ...s, host } : s));
  }, []);

  useEffect(() => {
    api.setSessionHandlers({
      unauthenticated: () => {
        api.setCsrf(null);
        setState((s) => (s.kind === "ready" ? { kind: "anonymous", methods: s.methods } : s));
      },
      reauth: () => new Promise<boolean>((resolve) => setReauth(() => resolve)),
    });
    load();
  }, [load]);

  const finishReauth = (ok: boolean) => {
    reauth?.(ok);
    setReauth(null);
  };

  const session = useMemo<Session | null>(() => {
    if (state.kind !== "ready") return null;
    return {
      me: state.me,
      methods: state.methods,
      projects: state.projects,
      project,
      host: state.host ?? noHost,
      projectName: (id) => {
        if (!id) return "—";
        return state.projects.find((p) => p.id === id)?.name ?? id;
      },
      selectProject: async (id: string) => {
        setProject(id);
        localStorage.setItem(PROJECT_KEY, id);
        if (state.me.user && state.me.method !== "disabled") {
          // Remember it server-side too; not fatal if refused.
          await api.updateMe({ default_project: id }).catch(() => {});
        }
      },
      refresh: load,
      refreshCapabilities,
      logout: async () => {
        await api.logout().catch(() => {});
        setState({ kind: "anonymous", methods: state.methods });
      },
    };
  }, [state, project, load, refreshCapabilities]);

  // Another tab switched accounts (spec/tenancy.md D71): start over as
  // the new one.
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key === api.ACCOUNT_EVENT_KEY) window.location.assign("/");
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  // A directory's authorize request sent the browser here to log in first
  // (spec/tenancy.md D54): signed in, it goes back there.
  useEffect(() => {
    if (state.kind !== "ready") return;
    const next = new URLSearchParams(window.location.search).get("next");
    if (next && next.startsWith("/d/") && !next.startsWith("//")) window.location.replace(next);
  }, [state.kind]);

  let body: ReactNode;
  switch (state.kind) {
    case "loading":
      body = <Loading />;
      break;
    case "error":
      body = (
        <div className="max-w-lg mx-auto mt-24 p-6 bg-white rounded-xl shadow-md border border-red-200">
          <h1 className="text-lg font-semibold text-red-700">Can't reach the control plane</h1>
          <p className="mt-2 text-sm text-gray-600">{state.message}</p>
          <button
            className="mt-4 px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg"
            onClick={() => {
              setState({ kind: "loading" });
              load();
            }}
          >
            Retry
          </button>
        </div>
      );
      break;
    case "anonymous":
      body = <Login methods={state.methods} onLoggedIn={load} />;
      break;
    case "ready":
      body = <SessionContext.Provider value={session}>{children}</SessionContext.Provider>;
      break;
  }
  return (
    <>
      {body}
      {reauth && state.kind === "ready" && (
        <ReauthDialog
          methods={state.methods}
          username={state.me.user?.display_name ?? ""}
          onDone={() => finishReauth(true)}
          onCancel={() => finishReauth(false)}
        />
      )}
    </>
  );
}
