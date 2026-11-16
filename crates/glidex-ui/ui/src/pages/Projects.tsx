import { useCallback, useEffect, useState, type FormEvent } from "react";
import { Link, useSearchParams } from "react-router-dom";
import * as api from "../api";
import type { ProjectView, Tenant } from "../types";
import { QUOTA_KEYS, QUOTA_LABELS } from "../types";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";
import { ErrorBanner, Field, PageHeader, errorMessage, inputClass, primaryButton, selectClass } from "../components/ui";
import { useSession, useTenantLabel } from "../session";
import { useDirectory, useProjectTenants, useTenants } from "../hooks";

/** `used / limit` (or `used` when unlimited). */
export function UsageCell({ used = 0, limit }: { used?: number; limit?: number | null }) {
  const limited = limit !== undefined && limit !== null;
  const over = limited && used > limit;
  const full = limited && used >= limit;
  return (
    <span className={over ? "text-red-600 font-medium" : full ? "text-amber-700" : "text-gray-700"}>
      {used}
      <span className="text-gray-400"> / {limited ? limit : "∞"}</span>
    </span>
  );
}

/** The tenant a new project goes in: the selected project's, else `default`,
 *  else the first the caller may create projects in. */
function initialTenant(tenants: Tenant[], selected: string | undefined): string {
  const ids = tenants.map((t) => t.id);
  return [selected, "default"].find((id) => id && ids.includes(id)) ?? ids[0] ?? "";
}

function CreateProjectForm({
  tenants,
  selectedTenant,
  onDone,
  onCancel,
}: {
  /** The tenants the caller may create projects in; the field shows for more than one. */
  tenants: Tenant[];
  selectedTenant?: string;
  onDone: () => void;
  onCancel: () => void;
}) {
  const [dir] = useDirectory();
  const label = useTenantLabel();
  const [tenant, setTenant] = useState(() => initialTenant(tenants, selectedTenant));
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [owner, setOwner] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.createProject({ name, description, owners: owner ? [owner] : [], tenant: tenant || undefined });
      onDone();
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <Field label="Name" htmlFor="project-name">
        <input
          id="project-name"
          className={inputClass}
          required
          pattern="[a-z0-9\-]{1,32}"
          title="1-32 lowercase letters, digits and dashes"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
      </Field>
      {tenants.length > 1 && (
        <Field label={label.one} htmlFor="project-tenant">
          <select id="project-tenant" className={selectClass} value={tenant} onChange={(e) => setTenant(e.target.value)}>
            {tenants.map((t) => (
              <option key={t.id} value={t.id}>
                {t.display_name ? `${t.display_name} (${t.slug})` : t.slug}
              </option>
            ))}
          </select>
        </Field>
      )}
      <Field label="Description" htmlFor="project-description">
        <input id="project-description" className={inputClass} value={description} onChange={(e) => setDescription(e.target.value)} />
      </Field>
      {dir && dir.users.length > 0 && (
        <Field label="Owner (optional)" htmlFor="project-owner">
          <select id="project-owner" className={selectClass} value={owner} onChange={(e) => setOwner(e.target.value)}>
            <option value="">No owner yet</option>
            {dir.users.map((u) => (
              <option key={u.id} value={u.id}>
                {u.display_name}
              </option>
            ))}
          </select>
        </Field>
      )}
      <p className="text-xs text-gray-500">New projects get unlimited quotas except 2 project networks; edit them on the project page.</p>
      <div className="flex justify-end gap-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm text-gray-700 hover:bg-gray-100 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primaryButton} disabled={busy}>
          {busy ? "Creating..." : "Create Project"}
        </button>
      </div>
    </form>
  );
}

export default function Projects() {
  const session = useSession();
  const label = useTenantLabel();
  const [projects, setProjects] = useState<ProjectView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const tenants = useTenants();
  const projectTenants = useProjectTenants(tenants);
  const [params, setParams] = useSearchParams();
  const filter = params.get("tenant") ?? "";

  const refresh = useCallback(() => {
    api
      .listProjects()
      .then((p) => setProjects(p.sort((a, b) => a.name.localeCompare(b.name))))
      .catch((e) => setError(errorMessage(e)));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);
  // Tenant slugs, for those the caller may read; the id otherwise.
  const tenantName = (id: string) => tenants.find((t) => t.id === id)?.slug ?? id;
  const showTenant = !!projects?.some((p) => p.tenant_id);
  // The tenants the caller's projects are in, for the filter: shown for two or more.
  const projectTenantIds = [...new Set((projects ?? []).map((p) => p.tenant_id ?? ""))]
    .filter(Boolean)
    .sort((a, b) => tenantName(a).localeCompare(tenantName(b)));
  const shown = filter ? (projects ?? []).filter((p) => p.tenant_id === filter) : projects;
  const selectedTenant = session.projects.find((p) => p.id === session.project)?.tenant_id;
  const canCreate = session.host.createProject || (projectTenants?.length ?? 0) > 0;

  return (
    <div>
      <PageHeader title="Projects" subtitle="Projects own VMs, disks, guest credentials and project networks, within their quotas.">
        {canCreate && (
          <button className={primaryButton} onClick={() => setCreating(true)}>
            + Create Project
          </button>
        )}
      </PageHeader>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {projectTenantIds.length > 1 && (
        <div className="mb-3 flex items-center gap-2 text-sm text-gray-600">
          <label htmlFor="projects-tenant">{label.one}</label>
          <select
            id="projects-tenant"
            className="px-2 py-1 border border-gray-300 rounded-lg bg-white text-sm"
            value={filter}
            onChange={(e) => setParams(e.target.value ? { tenant: e.target.value } : {}, { replace: true })}
          >
            <option value="">All tenants</option>
            {projectTenantIds.map((id) => (
              <option key={id} value={id}>
                {tenantName(id)}
              </option>
            ))}
          </select>
        </div>
      )}
      {projects === null || shown === null ? (
        <LoadingCard />
      ) : projects.length === 0 ? (
        <div className="bg-white rounded-xl shadow-xs border border-gray-200 p-8 text-center text-gray-500">
          You aren't a member of any project yet. Ask a project owner or an administrator to add you.
        </div>
      ) : (
        <div className="bg-white rounded-xl shadow-xs border border-gray-200 overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500 border-b border-gray-100">
                <th className="px-4 py-3 font-medium">Project</th>
                {showTenant && <th className="px-4 py-3 font-medium">{label.one}</th>}
                {QUOTA_KEYS.map((k) => (
                  <th key={k} className="px-4 py-3 font-medium">
                    {QUOTA_LABELS[k]}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {shown.map((p) => (
                <tr key={p.id} className="border-b border-gray-50 hover:bg-gray-50">
                  <td className="px-4 py-3">
                    <Link to={`/projects/${p.id}`} className="font-medium text-sky-700 hover:underline">
                      {p.name}
                    </Link>
                    {p.id === session.project && <span className="ml-2 text-xs text-gray-400">(selected)</span>}
                    {p.description && <div className="text-xs text-gray-500">{p.description}</div>}
                  </td>
                  {showTenant && (
                    <td className="px-4 py-3 text-gray-700" title={p.tenant_id}>
                      {p.tenant_id ? tenantName(p.tenant_id) : "—"}
                    </td>
                  )}
                  {QUOTA_KEYS.map((k) => (
                    <td key={k} className="px-4 py-3">
                      <UsageCell used={p.usage?.[k]} limit={p.quotas?.[k]} />
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {creating && (
        <Modal title="Create Project" onClose={() => setCreating(false)}>
          <CreateProjectForm
            tenants={projectTenants ?? []}
            selectedTenant={selectedTenant}
            onCancel={() => setCreating(false)}
            onDone={() => {
              setCreating(false);
              refresh();
              session.refresh();
            }}
          />
        </Modal>
      )}
    </div>
  );
}
