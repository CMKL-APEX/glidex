import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import type { AuditEntry } from "../types";
import { LoadingCard } from "../components/Loading";
import { Badge, ErrorBanner, PageHeader, errorMessage, primaryButton, secondaryButton } from "../components/ui";
import { isHostedDirectory, useSession } from "../session";
import { useAllowed } from "../nodes";
import { useDirectory } from "../hooks";

const filterClass = "mt-1 px-2 py-1.5 border border-gray-300 rounded-lg text-sm bg-white";

const PAGE = 100;

const COLUMNS: { order: api.AuditOrder; label: string }[] = [
  { order: "time", label: "Time" },
  { order: "user", label: "Who" },
  { order: "action", label: "Action" },
  { order: "target", label: "Target" },
  { order: "project", label: "Project" },
  { order: "result", label: "Result" },
  { order: "source", label: "From" },
];

function resultKind(r: string): string {
  if (r === "ok") return "ok";
  if (r === "denied") return "denied";
  return "muted";
}

export default function Audit() {
  const { me, host, projects, project: selected, projectName } = useSession();
  const [dir] = useDirectory();
  // A hosted directory's readers see its own audit, all of it (D74).
  const hosted = isHostedDirectory(me.directory_id);
  const all = host.readAudit || (host.readDirectoryAudit && hosted);
  // Otherwise only a project's own entries, of the projects whose audit they
  // may read: asked, since a token narrowed below owner may not.
  const refs = all ? [] : projects.map((p) => ({ type: "Project" as const, id: p.id }));
  const may = useAllowed(["readProjectAudit"], refs);
  const choices = all ? projects : may ? projects.filter((p) => may({ type: "Project", id: p.id }, "readProjectAudit")) : null;
  const [project, setProject] = useState<string>("");
  const [user, setUser] = useState("");
  const [since, setSince] = useState("");
  const [order, setOrder] = useState<api.AuditOrder>("time");
  const [ascending, setAscending] = useState(false);
  // The tokens of the pages shown so far: the first page's is empty.
  const [tokens, setTokens] = useState<string[]>([""]);
  const [next, setNext] = useState("");
  const [truncated, setTruncated] = useState(false);
  const [entries, setEntries] = useState<AuditEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState<string | null>(null);

  const load = useCallback(
    async (page: number, pages: string[], by: api.AuditOrder, up: boolean, from = project) => {
      setError(null);
      try {
        const r = await api.readAudit({
          project: from || undefined,
          user: user.trim() || undefined,
          since: since ? new Date(since).getTime() : undefined,
          limit: PAGE,
          order: by,
          ascending: up,
          page: pages[page],
        });
        setEntries(r.entries);
        setTokens(pages.slice(0, page + 1));
        setNext(r.next);
        setTruncated(r.truncated);
      } catch (e) {
        setEntries([]);
        setNext("");
        setError(errorMessage(e));
      }
    },
    [project, user, since],
  );

  // Load once the choices are known; later loads come from the filter form, a header or a page button.
  const ready = choices !== null;
  useEffect(() => {
    if (!choices || (!all && choices.length === 0)) return;
    const first = all ? "" : (choices.find((p) => p.id === selected) ?? choices[0]).id;
    setProject(first);
    load(0, [""], order, ascending, first);
  }, [ready]);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    load(0, [""], order, ascending);
  };

  // A header sorts by its column; again, the other way. Time starts newest first, the others A–Z.
  const sortBy = (by: api.AuditOrder) => {
    const up = by === order ? !ascending : by !== "time";
    setOrder(by);
    setAscending(up);
    load(0, [""], by, up);
  };
  const page = tokens.length - 1;
  const [back, forward] = order !== "time" ? ["Previous", "Next"] : ascending ? ["Older", "Newer"] : ["Newer", "Older"];

  const userName = (id?: string | null) => (id ? dir?.users.find((u) => u.id === id)?.display_name ?? id.slice(0, 8) : "—");

  if (choices === null) return <LoadingCard />;
  if (!all && choices.length === 0) {
    return (
      <div>
        <PageHeader title="Audit log" />
        <p className="text-gray-500">You can't read the audit log: it is for the cluster's auditors and the owners of a project.</p>
      </div>
    );
  }

  return (
    <div>
      <PageHeader title="Audit log" subtitle="Writes, logins and denied requests, newest first unless sorted by a column." />
      <form onSubmit={submit} className="mb-4 flex flex-wrap items-end gap-3 bg-white rounded-xl shadow-sm border border-gray-200 p-4">
        <div>
          <label htmlFor="audit-project" className="block text-xs text-gray-500">
            Project
          </label>
          <select id="audit-project" className={filterClass} value={project} onChange={(e) => setProject(e.target.value)}>
            {all && <option value="">{hosted ? "All (my directory)" : "All (cluster)"}</option>}
            {choices.map((p) => (
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
      {truncated && (
        <div className="mb-4 rounded-lg border border-amber-200 bg-amber-50 px-4 py-2 text-sm text-amber-800">
          This sort covers only the newest 20,000 matching entries. Narrow the filters (a project or a time) to sort them all.
        </div>
      )}
      {entries === null ? (
        <LoadingCard />
      ) : entries.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">No entries.</div>
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500 border-b border-gray-100">
                {COLUMNS.map((c) => (
                  <th
                    key={c.order}
                    className="px-3 py-2 font-medium"
                    aria-sort={order === c.order ? (ascending ? "ascending" : "descending") : "none"}
                  >
                    <button type="button" className="hover:text-gray-800" onClick={() => sortBy(c.order)}>
                      {c.label}
                      {order === c.order && <span className="ml-1">{ascending ? "▲" : "▼"}</span>}
                    </button>
                  </th>
                ))}
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
      {(page > 0 || next) && (
        <div className="mt-4 flex items-center justify-between text-sm text-gray-500">
          <button type="button" className={secondaryButton} disabled={page === 0} onClick={() => load(page - 1, tokens, order, ascending)}>
            {back}
          </button>
          <span>Page {page + 1}</span>
          <button type="button" className={secondaryButton} disabled={!next} onClick={() => load(page + 1, [...tokens, next], order, ascending)}>
            {forward}
          </button>
        </div>
      )}
    </div>
  );
}
