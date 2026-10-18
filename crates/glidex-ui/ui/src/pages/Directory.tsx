import { lazy, Suspense, useEffect, useState } from "react";
import { Link, NavLink, Navigate, useParams } from "react-router-dom";
import * as api from "../api";
import type { Tenant } from "../types";
import { Loading } from "../components/Loading";
import { Card, PageHeader } from "../components/ui";
import { useSession } from "../session";

const Tenants = lazy(() => import("./Tenants"));
const Access = lazy(() => import("./Access"));
const Connectors = lazy(() => import("./Connectors"));
const Policies = lazy(() => import("./Policies"));

/** What the directory page's tabs are, and who sees each. */
export function directoryTabs(host: Record<string, boolean>, readsATenant: boolean) {
  return [
    { id: "overview", label: "Overview", show: true },
    { id: "tenants", label: "Tenants", show: host.listTenants || host.createTenant || readsATenant },
    { id: "access", label: "Users & roles", show: host.listUsers || host.listTeams || host.readSystemBindings },
    { id: "connectors", label: "Connectors", show: host.listConnectors },
    { id: "policies", label: "Policies", show: host.readPolicy },
  ].filter((t) => t.show);
}

/** The directory (spec/tenancy.md §15): its tenants, users, teams and
 *  roles, login connectors and policies, one page with a tab each. */
export default function Directory() {
  const { tab = "overview" } = useParams<{ tab: string }>();
  const { host } = useSession();
  const tabs = directoryTabs(host, true);
  if (!tabs.some((t) => t.id === tab)) return <Navigate to="/directory" replace />;
  return (
    <div>
      <nav className="mb-6 flex flex-wrap gap-1 border-b border-gray-200" aria-label="Directory">
        {tabs.map((t) => (
          <NavLink
            key={t.id}
            to={t.id === "overview" ? "/directory" : `/directory/${t.id}`}
            end
            className={({ isActive }) =>
              `px-4 py-2 text-sm -mb-px border-b-2 ${
                isActive ? "border-sky-600 text-sky-700 font-medium" : "border-transparent text-gray-600 hover:text-gray-900"
              }`
            }
          >
            {t.label}
          </NavLink>
        ))}
      </nav>
      <Suspense fallback={<Loading />}>
        {tab === "overview" && <Overview />}
        {tab === "tenants" && <Tenants />}
        {tab === "access" && <Access />}
        {tab === "connectors" && <Connectors />}
        {tab === "policies" && <Policies />}
      </Suspense>
    </div>
  );
}

/** The directory itself: whose it is, and what lives where. */
function Overview() {
  const { me, host } = useSession();
  const [tenants, setTenants] = useState<Tenant[] | null>(null);
  const [counts, setCounts] = useState<{ users?: number; teams?: number; connectors?: number }>({});
  useEffect(() => {
    api
      .listTenants()
      .then(setTenants)
      .catch(() => setTenants(null));
    if (host.listUsers) api.listUsers().then((u) => setCounts((c) => ({ ...c, users: u.length }))).catch(() => {});
    if (host.listTeams) api.listTeams().then((t) => setCounts((c) => ({ ...c, teams: t.length }))).catch(() => {});
    if (host.listConnectors) api.listConnectors().then((x) => setCounts((c) => ({ ...c, connectors: x.length }))).catch(() => {});
  }, [host.listUsers, host.listTeams, host.listConnectors]);
  const owner = tenants?.find((t) => t.owns_directory);
  return (
    <div>
      <PageHeader title="Directory" subtitle="Who can sign in, how, and to which tenants." />
      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <Card title="This directory">
          <dl className="grid grid-cols-2 gap-x-4 gap-y-2 text-sm" data-testid="directory-overview">
            <dt className="text-gray-500">Directory</dt>
            <dd className="font-mono">{me.directory_id ?? "local"}</dd>
            <dt className="text-gray-500">Owned by</dt>
            <dd>{owner ? <Link to={`/tenants/${owner.id}`} className="text-sky-700 hover:underline">{owner.display_name || owner.slug}</Link> : "—"}</dd>
            <dt className="text-gray-500">Tenants</dt>
            <dd>{tenants ? tenants.length : "—"}</dd>
            {counts.users !== undefined && (
              <>
                <dt className="text-gray-500">Users</dt>
                <dd>{counts.users}</dd>
              </>
            )}
            {counts.teams !== undefined && (
              <>
                <dt className="text-gray-500">Teams</dt>
                <dd>{counts.teams}</dd>
              </>
            )}
            {counts.connectors !== undefined && (
              <>
                <dt className="text-gray-500">Login connectors</dt>
                <dd>{counts.connectors}</dd>
              </>
            )}
          </dl>
        </Card>
        <Card title="What lives where">
          <ul className="text-sm text-gray-700 space-y-2 list-disc pl-5">
            <li>
              <strong>The directory</strong> holds users, their sign-in methods, teams, login connectors, tenants and their
              projects, tenant and project roles, and tokens. Every server of a cluster has the same copy.
            </li>
            <li>
              <strong>This cluster</strong> holds the system roles (on the cluster, so on every host), its site policies,
              admissions, and the VMs, disks and networks that run on it.
            </li>
            <li>
              Local accounts belong to one host: in a cluster, <em>alice</em> on one host isn't <em>alice</em> on another
              unless the site shares its accounts.
            </li>
            <li>A control plane hosts one directory today; hosting several, and clusters that trust a directory elsewhere, come later.</li>
          </ul>
        </Card>
      </div>
    </div>
  );
}
