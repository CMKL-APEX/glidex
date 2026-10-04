import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import type { AuditEntry } from "../types";
import { LoadingCard } from "../components/Loading";
import { Badge, ErrorBanner, PageHeader, errorMessage, primaryButton } from "../components/ui";
import { useSession } from "../session";
import { useDirectory } from "../hooks";

const filterClass = "mt-1 px-2 py-1.5 border border-gray-300 rounded-lg text-sm bg-white";

function resultKind(r: string): string {
  if (r === "ok") return "ok";
  if (r === "denied") return "denied";
  return "muted";
}

export default function Audit() {
  const { host, projects, project: selected, projectName } = useSession();
  const [dir] = useDirectory();
  // Without readAudit, only a project's own entries (project owners).
  const [project, setProject] = useState<string>(host.readAudit ? "" : (selected ?? ""));
  const [user, setUser] = useState("");
  const [since, setSince] = useState("");
  const [entries, setEntries] = useState<AuditEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      const e = await api.readAudit({
        project: project || undefined,
        user: user.trim() || undefined,
        since: since ? new Date(since).getTime() : undefined,
        limit: 500,
      });
      setEntries([...e].sort((a, b) => b.time - a.time));
    } catch (e) {
      setEntries([]);
      setError(errorMessage(e));
    }
  }, [project, user, since]);

  useEffect(() => {
    load();
    // Load once on entry; later loads come from the filter form.
  }, []);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    load();
  };

  const userName = (id?: string | null) => (id ? dir?.users.find((u) => u.id === id)?.display_name ?? id.slice(0, 8) : "—");

  return (
    <div>
      <PageHeader title="Audit log" subtitle="Writes, logins and denied requests, newest first." />
      <form onSubmit={submit} className="mb-4 flex flex-wrap items-end gap-3 bg-white rounded-xl shadow-sm border border-gray-200 p-4">
        <div>
          <label htmlFor="audit-project" className="block text-xs text-gray-500">
            Project
          </label>
          <select id="audit-project" className={filterClass} value={project} onChange={(e) => setProject(e.target.value)}>
            {host.readAudit && <option value="">All</option>}
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
        </div>
        <div>
          <label htmlFor="audit-user" className="block text-xs text-gray-500">
            User
          </label>
          {dir && dir.users.length > 0 ? (
            <select id="audit-user" className={filterClass} value={user} onChange={(e) => setUser(e.target.value)}>
              <option value="">Anyone</option>
              {dir.users.map((u) => (
                <option key={u.id} value={u.id}>
                  {u.display_name}
                </option>
              ))}
            </select>
          ) : (
            <input id="audit-user" className={filterClass} placeholder="user id" value={user} onChange={(e) => setUser(e.target.value)} />
          )}
        </div>
        <div>
          <label htmlFor="audit-since" className="block text-xs text-gray-500">
            Since
          </label>
          <input id="audit-since" type="datetime-local" className={filterClass} value={since} onChange={(e) => setSince(e.target.value)} />
        </div>
        <button type="submit" className={primaryButton}>
          Apply
        </button>
      </form>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {entries === null ? (
        <LoadingCard />
      ) : entries.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">No entries.</div>
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500 border-b border-gray-100">
                <th className="px-3 py-2 font-medium">Time</th>
                <th className="px-3 py-2 font-medium">Who</th>
                <th className="px-3 py-2 font-medium">Action</th>
                <th className="px-3 py-2 font-medium">Target</th>
                <th className="px-3 py-2 font-medium">Project</th>
                <th className="px-3 py-2 font-medium">Result</th>
                <th className="px-3 py-2 font-medium">From</th>
              </tr>
            </thead>
            <tbody>
              {entries.map((e) => {
                const key = `${e.time}-${e.request_id}-${e.action}`;
                return [
                  <tr key={key} className="border-b border-gray-50 hover:bg-gray-50 cursor-pointer" onClick={() => setOpen(open === key ? null : key)}>
                    <td className="px-3 py-2 whitespace-nowrap text-gray-600">{new Date(e.time).toLocaleString()}</td>
                    <td className="px-3 py-2">
                      {e.principal == null ? (
                        <span className="text-gray-400">anonymous</span>
                      ) : e.principal.system ? (
                        <span className="text-gray-500">system: {e.principal.system}</span>
                      ) : (
                        e.principal.name ?? userName(e.principal.user)
                      )}
                      {e.principal?.method && <span className="ml-1 text-xs text-gray-400">{e.principal.method}</span>}
                    </td>
                    <td className="px-3 py-2 font-mono text-xs">{e.action}</td>
                    <td className="px-3 py-2 font-mono text-xs">{e.target ?? "—"}</td>
                    <td className="px-3 py-2">{e.project ? projectName(e.project) : "—"}</td>
                    <td className="px-3 py-2">
                      <Badge kind={resultKind(e.result)}>{e.result}</Badge>
                    </td>
                    <td className="px-3 py-2 text-xs text-gray-500">{e.source}</td>
                  </tr>,
                  open === key && (
                    <tr key={`${key}-d`} className="bg-gray-50">
                      <td colSpan={7} className="px-3 py-2">
                        <div className="text-xs text-gray-600 space-y-1">
                          <div>
                            request <code className="font-mono">{e.request_id}</code>
                            {e.error_code && <> · error {e.error_code}</>}
                          </div>
                          {e.policies && e.policies.length > 0 && (
                            <div>
                              policies <code className="font-mono">{e.policies.join(", ")}</code>
                            </div>
                          )}
                          {e.details && <pre className="font-mono whitespace-pre-wrap">{JSON.stringify(e.details, null, 2)}</pre>}
                        </div>
                      </td>
                    </tr>
                  ),
                ];
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
