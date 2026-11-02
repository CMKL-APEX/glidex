import { useEffect, useState } from "react";
import * as api from "./api";
import type { Directory } from "./components/ui";
import { useCan, useSession } from "./session";
import type { Tenant } from "./types";

/** Users and teams, when the caller may list them (`listUsers`,
 * `listTeams`); `null` when it may list neither. */
export function useDirectory(): [Directory | null, () => void] {
  const { host } = useSession();
  const [dir, setDir] = useState<Directory | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    if (!host.listUsers && !host.listTeams) {
      setDir(null);
      return;
    }
    let live = true;
    Promise.all([
      host.listUsers ? api.listUsers().catch(() => []) : Promise.resolve([]),
      host.listTeams ? api.listTeams().catch(() => []) : Promise.resolve([]),
    ]).then(([users, teams]) => live && setDir({ users, teams }));
    return () => {
      live = false;
    };
  }, [host.listUsers, host.listTeams, tick]);
  return [dir, () => setTick((t) => t + 1)];
}

/** Connector id → display name, from the login page's public list: who
 *  may see a tenant may see how its people sign in. */
export function useConnectorNames(): Record<string, string> {
  const [names, setNames] = useState<Record<string, string>>({});
  useEffect(() => {
    let live = true;
    api
      .authMethods()
      .then((m) => live && setNames(Object.fromEntries((m.connectors ?? []).map((c) => [c.id, c.display_name || c.id]))))
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, []);
  return names;
}

/** Node id → name, when the caller may list nodes: a local account in a
 *  cluster is one host's. Empty otherwise. */
export function useNodeNames(): Record<string, string> {
  const [names, setNames] = useState<Record<string, string>>({});
  useEffect(() => {
    let live = true;
    api
      .listNodes()
      .then((ns) => live && setNames(Object.fromEntries(ns.map((n) => [n.meta.id, n.spec.name]))))
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, []);
  return names;
}

/** The tenants the caller may read: every one for a directory reader, the
 *  ones it holds a role on otherwise. Empty if it may read none. */
export function useTenants(): Tenant[] {
  const [tenants, setTenants] = useState<Tenant[]>([]);
  useEffect(() => {
    let live = true;
    api
      .listTenants()
      .then((t) => live && setTenants(t))
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, []);
  return tenants;
}

/** Of `tenants`, those the caller may create projects in; `null` while
 *  checking. A tenant waiting for approval takes none yet. */
export function useProjectTenants(tenants: Tenant[]): Tenant[] | null {
  const usable = tenants.filter((t) => !t.pending);
  const allowed = useCan(usable.map((t) => ({ action: "createProject", resource: { type: "Tenant" as const, id: t.id } })));
  return allowed && usable.filter((_, i) => allowed[i]);
}

/** Groups `projects` by tenant, in tenant name order, for a select's
 *  `<optgroup>`s; one group with an empty label when all are in one tenant. */
export function byTenant<P extends { tenant_id?: string }>(
  projects: P[],
  tenantName: (id: string) => string,
): { label: string; projects: P[] }[] {
  const groups = new Map<string, P[]>();
  for (const p of projects) {
    const id = p.tenant_id ?? "";
    groups.set(id, [...(groups.get(id) ?? []), p]);
  }
  if (groups.size <= 1) return [{ label: "", projects }];
  return [...groups.entries()]
    .map(([id, ps]) => ({ label: id ? tenantName(id) : "—", projects: ps }))
    .sort((a, b) => a.label.localeCompare(b.label));
}
