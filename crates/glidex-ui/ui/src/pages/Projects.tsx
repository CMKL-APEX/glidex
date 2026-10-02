import { useCallback, useEffect, useState, type FormEvent } from "react";
import { Link } from "react-router-dom";
import * as api from "../api";
import type { ProjectView } from "../types";
import { QUOTA_KEYS, QUOTA_LABELS } from "../types";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";
import { ErrorBanner, Field, PageHeader, errorMessage, inputClass, primaryButton, selectClass } from "../components/ui";
import { useSession } from "../session";
import { useDirectory } from "../hooks";

/** `used / limit` (or `used` when unlimited). */
export function UsageCell({ used, limit }: { used: number; limit: number | null }) {
  const over = limit !== null && used > limit;
  const full = limit !== null && used >= limit;
  return (
    <span className={over ? "text-red-600 font-medium" : full ? "text-amber-700" : "text-gray-700"}>
      {used}
      <span className="text-gray-400"> / {limit === null ? "∞" : limit}</span>
    </span>
  );
}

function CreateProjectForm({ onDone, onCancel }: { onDone: () => void; onCancel: () => void }) {
  const [dir] = useDirectory();
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
      await api.createProject({ name, description, owners: owner ? [owner] : [] });
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
  const [projects, setProjects] = useState<ProjectView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);

  const refresh = useCallback(() => {
    api
      .listProjects()
      .then((p) => setProjects(p.sort((a, b) => a.name.localeCompare(b.name))))
      .catch((e) => setError(errorMessage(e)));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return (
    <div>
      <PageHeader title="Projects" subtitle="Projects own VMs, disks, guest credentials and project networks, within their quotas.">
        {session.host.createProject && (
          <button className={primaryButton} onClick={() => setCreating(true)}>
            + Create Project
          </button>
        )}
      </PageHeader>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {projects === null ? (
        <LoadingCard />
      ) : projects.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">
          You aren't a member of any project yet. Ask a project owner or an administrator to add you.
        </div>
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500 border-b border-gray-100">
                <th className="px-4 py-3 font-medium">Project</th>
                {QUOTA_KEYS.map((k) => (
                  <th key={k} className="px-4 py-3 font-medium">
                    {QUOTA_LABELS[k]}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {projects.map((p) => (
                <tr key={p.id} className="border-b border-gray-50 hover:bg-gray-50">
                  <td className="px-4 py-3">
                    <Link to={`/projects/${p.id}`} className="font-medium text-sky-700 hover:underline">
                      {p.name}
                    </Link>
                    {p.id === session.project && <span className="ml-2 text-xs text-gray-400">(selected)</span>}
                    {p.description && <div className="text-xs text-gray-500">{p.description}</div>}
                  </td>
                  {QUOTA_KEYS.map((k) => (
                    <td key={k} className="px-4 py-3">
                      <UsageCell used={p.usage[k]} limit={p.quotas[k]} />
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
