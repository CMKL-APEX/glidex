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
 * the host's own networking `Host`. A check on a type the schema doesn't
 * list is never allowed, so a wrong entry hides the page from everyone
 * (tests/ui_capabilities.rs in glidex-control-plane compares the two). */
const HOST_ACTION_RESOURCE = {
  createProject: "Cluster",
  listUsers: "Cluster",
  manageUsers: "Cluster",
  listTeams: "Cluster",
  manageTeams: "Cluster",
  readSystemBindings: "Cluster",
  manageSystemBindings: "Cluster",
  readPolicy: "Cluster",
  writePolicy: "Cluster",
  readAudit: "Cluster",
  readUsage: "Cluster",
  manageAnyTokens: "Cluster",
  createNetwork: "Host",
  readCluster: "Cluster",
} as const satisfies Record<string, SingletonEntityType>;

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
      methods = await api.authMethods();
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
    const [projects, allowed] = await Promise.all([
      api.listProjects().catch(() => [] as ProjectView[]),
      api.checkAccess(HOST_ACTIONS.map((action) => ({ action, resource: { type: HOST_ACTION_RESOURCE[action] } }))).catch(() =>
        HOST_ACTIONS.map(() => false),
      ),
    ]);
    const host = Object.fromEntries(HOST_ACTIONS.map((a, i) => [a, allowed[i]])) as Record<HostAction, boolean>;
    setProject((current) => {
      const known = (id: string | null | undefined) => !!id && projects.some((p) => p.id === id);
      if (known(current)) return current;
      if (known(me.default_project)) return me.default_project;
      const stored = localStorage.getItem(PROJECT_KEY);
      if (known(stored)) return stored;
      return projects[0]?.id ?? null;
    });
    setState({ kind: "ready", me, methods, projects, host });
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
      logout: async () => {
        await api.logout().catch(() => {});
        setState({ kind: "anonymous", methods: state.methods });
      },
    };
  }, [state, project, load]);

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
