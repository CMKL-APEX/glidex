// The browser session (spec/security.md §5.5, §5.6): who is logged in,
// the selected project, and what the caller may do on the host. Pages
// read it through useSession(); the server still checks every request.
// The login, step-up and whoami plumbing is the kit's SessionProvider
// (crates/glidex-ui/kit); this adds glidex's projects and capabilities.
import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { SessionProvider as KitSessionProvider, useSession as useKitSession, type AuthApi } from "@glidex/ui-kit";
import * as api from "./api";
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
  // A hosted directory's own audit (spec/tenancy.md D74).
  readDirectoryAudit: "Directory",
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

/** A directory this host keeps for another organization: not its own
 * `local`, nor a trusted directory's replica (spec/tenancy.md D74). */
export function isHostedDirectory(id: string | undefined | null): boolean {
  return !!id && id !== "local" && !id.startsWith("trusted-");
}

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

/** What the kit's session loads for glidex after whoami. */
interface Extra {
  projects: ProjectView[];
  host: Record<HostAction, boolean>;
}

/** glidex's login API for the kit. */
const glidexAuth: AuthApi<WhoAmI, AuthMethods> = {
  methods: () => {
    // `/login/<directory>`: that directory's login page (D61).
    const m = window.location.pathname.match(/^\/login\/([a-z0-9-]{1,32})\/?$/);
    return api.authMethods(m?.[1]);
  },
  whoami: api.whoami,
  login: api.loginPam,
  startSso: (connector, returnTo, reauth) => api.startOidc(returnTo, reauth, connector),
  logout: api.logout,
};

const renderLogin = (methods: AuthMethods, onLoggedIn: () => void) => <Login methods={methods} onLoggedIn={onLoggedIn} />;

const renderReauth = ({ methods, me, onDone, onCancel }: { methods: AuthMethods; me: WhoAmI; onDone: () => void; onCancel: () => void }) => (
  <ReauthDialog methods={methods} username={me.user?.display_name ?? ""} onDone={onDone} onCancel={onCancel} />
);

export function SessionProvider({ children }: { children: ReactNode }) {
  const [project, setProject] = useState<string | null>(null);

  const load = useCallback(async (me: WhoAmI): Promise<Extra> => {
    const [projects, host] = await Promise.all([api.listProjects().catch(() => [] as ProjectView[]), hostCapabilities()]);
    setProject((current) => {
      const known = (id: string | null | undefined) => !!id && projects.some((p) => p.id === id);
      if (known(current)) return current;
      if (known(me.default_project)) return me.default_project ?? null;
      const stored = localStorage.getItem(PROJECT_KEY);
      if (known(stored)) return stored;
      return projects[0]?.id ?? null;
    });
    return { projects, host };
  }, []);

  // Another tab switched accounts (spec/tenancy.md D71): start over as
  // the new one.
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key === api.ACCOUNT_EVENT_KEY) window.location.assign("/");
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  return (
    <KitSessionProvider
      auth={glidexAuth}
      load={load}
      login={renderLogin}
      reauth={renderReauth}
      loading={<Loading />}
      unreachable="Can't reach the control plane"
    >
      <GlidexSession project={project} setProject={setProject}>
        {children}
      </GlidexSession>
    </KitSessionProvider>
  );
}

/** glidex's Session over the kit's, once signed in. */
function GlidexSession({ project, setProject, children }: { project: string | null; setProject: (id: string) => void; children: ReactNode }) {
  const kit = useKitSession<WhoAmI, AuthMethods, Extra>();
  const { me, methods, extra, setExtra, refresh, logout } = kit;

  const refreshCapabilities = useCallback(async () => {
    const host = await hostCapabilities();
    setExtra((x) => ({ ...x, host }));
  }, [setExtra]);

  const session = useMemo<Session>(
    () => ({
      me,
      methods,
      projects: extra.projects,
      project,
      host: extra.host ?? noHost,
      projectName: (id) => {
        if (!id) return "—";
        return extra.projects.find((p) => p.id === id)?.name ?? id;
      },
      selectProject: async (id: string) => {
        setProject(id);
        localStorage.setItem(PROJECT_KEY, id);
        if (me.user && me.method !== "disabled") {
          // Remember it server-side too; not fatal if refused.
          await api.updateMe({ default_project: id }).catch(() => {});
        }
      },
      refresh,
      refreshCapabilities,
      logout,
    }),
    [me, methods, extra, project, setProject, refresh, refreshCapabilities, logout],
  );

  // A directory's authorize request sent the browser here to log in first
  // (spec/tenancy.md D54): signed in, it goes back there.
  useEffect(() => {
    const next = new URLSearchParams(window.location.search).get("next");
    if (next && next.startsWith("/d/") && !next.startsWith("//")) window.location.replace(next);
  }, []);

  return <SessionContext.Provider value={session}>{children}</SessionContext.Provider>;
}
