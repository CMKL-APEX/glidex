import { useCallback, useEffect, useMemo, useState } from "react";
import * as api from "../api";
import { ApiRequestError } from "../api";
import type {
  EntityRef,
  EntityType,
  PolicyInfo,
  PolicySource,
  SimulationRequest,
  SimulationResult,
  SitePolicy,
  SitePolicyVersion,
} from "../types";
import { formatTime } from "../types";
import { Loading } from "../components/Loading";
import Modal from "../components/Modal";
import { Badge, Card, ErrorBanner, PageHeader, errorMessage, inputClass, primaryButton, secondaryButton } from "../components/ui";
import { useSession } from "../session";
import { useDirectory } from "../hooks";

/** Actions offered by the simulator's request builder (the schema has them all). */
const ACTIONS = [
  "readVm", "listVms", "createVm", "deleteVm", "startVm", "stopVm", "pauseVm", "openConsole",
  "attachDisk", "attachDevice", "useDisk", "useCredential", "usePciDevice", "useHostPath",
  "listDisks", "readDisk", "createDisk", "deleteDisk", "resizeDisk",
  "listCredentials", "readCredential", "createCredential", "updateCredential", "deleteCredential",
  "attachNetwork", "useNetwork", "createProjectNetwork", "deleteProjectNetwork",
  "offerNetworkShare", "acceptNetworkShare", "createNetwork", "grantNetwork",
  "readProject", "readBindings", "manageBindings", "updateProject", "deleteProject", "createProject",
  "manageSystemBindings", "exceedQuota", "pullImage", "deleteImage", "readImage",
  "listUsers", "manageUsers", "manageTeams", "manageOwnTokens", "manageServiceTokens",
  "readAudit", "readPolicy", "writePolicy", "installOvs", "readOvsStatus",
];

const RESOURCE_TYPES: EntityType[] = ["Host", "Project", "Vm", "Disk", "Credential", "Network", "Image", "PciDevice", "User"];
const SOURCES: PolicySource[] = ["base", "role", "link", "site", "file"];

const NEW_TEMPLATE = (id: string) => `@id("${id}")
forbid (principal, action == Glidex::Action::"openConsole", resource)
when { context.auth has source_ip && !context.auth.source_ip.isInRange(ip("10.0.0.0/8")) };
`;

interface Row {
  id: string;
  source: PolicySource;
  template: boolean;
  text: string;
  /** Stored site policy (also when disabled, so not loaded). */
  site?: SitePolicy;
}

interface Draft {
  id: string;
  text: string;
  description: string;
  enabled: boolean;
  /** Version being replaced; 0 for a new policy. */
  version: number;
}

function DecisionBadge({ allowed }: { allowed: boolean }) {
  return <Badge kind={allowed ? "ok" : "denied"}>{allowed ? "allow" : "deny"}</Badge>;
}

function RequestBuilder({ requests, onChange }: { requests: SimulationRequest[]; onChange: (r: SimulationRequest[]) => void }) {
  const [dir] = useDirectory();
  const { me, project } = useSession();
  const [ptype, setPtype] = useState<"User" | "Token">("User");
  const [pid, setPid] = useState(me.user?.id ?? "");
  const [action, setAction] = useState("openConsole");
  const [rtype, setRtype] = useState<EntityType>("Project");
  const [rid, setRid] = useState(project ?? "");

  const add = () => {
    if (!pid.trim() || !action.trim()) return;
    const resource: EntityRef = rtype === "Host" ? { type: "Host" } : { type: rtype, id: rid.trim() };
    onChange([...requests, { principal: { type: ptype, id: pid.trim() }, action: action.trim(), resource }]);
  };

  return (
    <div className="space-y-2">
      <div className="grid grid-cols-1 md:grid-cols-6 gap-2 items-end text-sm">
        <div>
          <label className="block text-xs text-gray-500">Principal</label>
          <select className="w-full px-2 py-1 border border-gray-300 rounded-lg bg-white" value={ptype} onChange={(e) => setPtype(e.target.value as "User" | "Token")}>
            <option value="User">User</option>
            <option value="Token">Token</option>
          </select>
        </div>
        <div className="md:col-span-2">
          <label className="block text-xs text-gray-500">{ptype} id</label>
          {ptype === "User" && dir && dir.users.length > 0 ? (
            <select className="w-full px-2 py-1 border border-gray-300 rounded-lg bg-white" value={pid} onChange={(e) => setPid(e.target.value)}>
              {!dir.users.some((u) => u.id === pid) && <option value={pid}>{pid || "choose…"}</option>}
              {dir.users.map((u) => (
                <option key={u.id} value={u.id}>
                  {u.display_name}
                </option>
              ))}
            </select>
          ) : (
            <input className="w-full px-2 py-1 border border-gray-300 rounded-lg font-mono" value={pid} onChange={(e) => setPid(e.target.value)} />
          )}
        </div>
        <div>
          <label className="block text-xs text-gray-500">Action</label>
          <input
            list="policy-actions"
            className="w-full px-2 py-1 border border-gray-300 rounded-lg font-mono"
            value={action}
            onChange={(e) => setAction(e.target.value)}
          />
          <datalist id="policy-actions">
            {ACTIONS.map((a) => (
              <option key={a} value={a} />
            ))}
          </datalist>
        </div>
        <div>
          <label className="block text-xs text-gray-500">Resource</label>
          <select className="w-full px-2 py-1 border border-gray-300 rounded-lg bg-white" value={rtype} onChange={(e) => setRtype(e.target.value as EntityType)}>
            {RESOURCE_TYPES.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </select>
        </div>
        <div>
          <label className="block text-xs text-gray-500">Resource id</label>
          <input
            className="w-full px-2 py-1 border border-gray-300 rounded-lg font-mono disabled:bg-gray-100"
            disabled={rtype === "Host"}
            value={rtype === "Host" ? "local" : rid}
            onChange={(e) => setRid(e.target.value)}
          />
        </div>
      </div>
      <button type="button" className={secondaryButton} onClick={add}>
        + Add request
      </button>
    </div>
  );
}

function History({ id, onRestore }: { id: string; onRestore: (v: SitePolicyVersion) => void }) {
  const [versions, setVersions] = useState<SitePolicyVersion[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api
      .policyVersions(id)
      .then((v) => setVersions([...v].sort((a, b) => b.version - a.version)))
      .catch((e) => setError(errorMessage(e)));
  }, [id]);
  if (error) return <p className="text-sm text-red-600">{error}</p>;
  if (!versions) return <Loading />;
  if (versions.length === 0) return <p className="text-sm text-gray-500">No history.</p>;
  return (
    <div className="max-h-[60vh] overflow-y-auto space-y-3">
      {versions.map((v) => (
        <div key={v.version} className="border border-gray-200 rounded-lg p-3">
          <div className="flex items-center justify-between text-sm">
            <span>
              <span className="font-medium">v{v.version}</span> · {v.author.slice(0, 8)} · {new Date(v.time * 1000).toLocaleString()}{" "}
              {v.deleted ? <Badge kind="denied">deleted</Badge> : !v.enabled && <Badge kind="muted">disabled</Badge>}
            </span>
            {!v.deleted && (
              <button className="text-sm text-sky-700 hover:underline" onClick={() => onRestore(v)}>
                Load into editor
              </button>
            )}
          </div>
          {!v.deleted && <pre className="mt-2 text-xs font-mono bg-gray-50 p-2 rounded overflow-x-auto">{v.text}</pre>}
        </div>
      ))}
    </div>
  );
}

export default function Policies() {
  const { host } = useSession();
  const [data, setData] = useState<{ policies: PolicyInfo[]; site: SitePolicy[] } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [shown, setShown] = useState<Set<PolicySource>>(new Set(["base", "role", "site", "file"]));
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [status, setStatus] = useState<{ kind: "ok" | "error"; text: string; errors?: string[] } | null>(null);
  const [requests, setRequests] = useState<SimulationRequest[]>([]);
  const [results, setResults] = useState<SimulationResult[] | null>(null);
  const [history, setHistory] = useState(false);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setData(await api.listPolicies());
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const rows = useMemo<Row[]>(() => {
    if (!data) return [];
    const out: Row[] = data.policies.map((p) => ({ ...p, site: data.site.find((s) => s.id === p.id) }));
    for (const s of data.site) {
      if (!out.some((r) => r.id === s.id)) out.push({ id: s.id, source: "site", template: false, text: s.text, site: s });
    }
    const order = (s: PolicySource) => SOURCES.indexOf(s);
    return out.sort((a, b) => order(a.source) - order(b.source) || a.id.localeCompare(b.id));
  }, [data]);

  const visible = rows.filter((r) => shown.has(r.source) && (!filter || r.id.includes(filter) || r.text.includes(filter)));
  const row = rows.find((r) => r.id === selected) ?? null;
  const editable = host.writePolicy && row?.source === "site";

  const select = (r: Row) => {
    setSelected(r.id);
    setStatus(null);
    setResults(null);
    setDraft(
      r.source === "site"
        ? {
            id: r.id,
            text: r.site?.text ?? r.text,
            description: r.site?.description ?? "",
            enabled: r.site?.enabled ?? true,
            version: r.site?.version ?? 0,
          }
        : null,
    );
  };

  const startNew = () => {
    const id = "site.new-policy";
    setSelected(null);
    setStatus(null);
    setResults(null);
    setDraft({ id, text: NEW_TEMPLATE(id), description: "", enabled: true, version: 0 });
  };

  const explain = (e: unknown) => {
    if (e instanceof ApiRequestError) {
      if (e.code === "conflict") return { kind: "error" as const, text: `${e.message}. Reload to see the current version.` };
      if (e.code === "would_lock_out")
        return { kind: "error" as const, text: "Refused: after this change you could no longer manage policies." };
      if (e.status === 422 || e.code === "invalid_policy")
        return { kind: "error" as const, text: e.message, errors: e.details?.errors };
    }
    return { kind: "error" as const, text: errorMessage(e) };
  };

  const validate = async () => {
    if (!draft) return;
    setBusy(true);
    try {
      const r = await api.validatePolicy(draft.id, draft.text);
      setStatus(r.valid ? { kind: "ok", text: "Valid against the schema." } : { kind: "error", text: "Invalid", errors: r.errors });
    } catch (e) {
      setStatus(explain(e));
    } finally {
      setBusy(false);
    }
  };

  const simulate = async () => {
    if (!draft || requests.length === 0) return;
    setBusy(true);
    try {
      const r = await api.simulatePolicy([{ id: draft.id, text: draft.text, enabled: draft.enabled }], requests);
      setResults(r.results);
      setStatus(null);
    } catch (e) {
      setStatus(explain(e));
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    if (!draft) return;
    setBusy(true);
    try {
      const saved = await api.putPolicy(draft.id, {
        text: draft.text,
        description: draft.description,
        enabled: draft.enabled,
        version: draft.version,
      });
      setStatus({ kind: "ok", text: `Saved${saved ? ` as version ${saved.version}` : ""}.` });
      if (saved) setDraft({ ...draft, version: saved.version });
      setSelected(draft.id);
      await refresh();
    } catch (e) {
      setStatus(explain(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!draft || draft.version === 0) return;
    if (!confirm(`Delete site policy ${draft.id}?`)) return;
    setBusy(true);
    try {
      await api.deletePolicy(draft.id, draft.version);
      setDraft(null);
      setSelected(null);
      setStatus(null);
      await refresh();
    } catch (e) {
      setStatus(explain(e));
    } finally {
      setBusy(false);
    }
  };

  if (!host.readPolicy) {
    return (
      <div>
        <PageHeader title="Policies" />
        <p className="text-gray-500">You can't read the authorization policies.</p>
      </div>
    );
  }

  return (
    <div>
      <PageHeader
        title="Policies"
        subtitle="Cedar policies deciding every request. Site policies add rules; forbid always wins, and they can't relax the base policies."
      >
        {host.writePolicy && (
          <button className={primaryButton} onClick={startNew}>
            + New site policy
          </button>
        )}
      </PageHeader>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {!data ? (
        <Loading />
      ) : (
        <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
          <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-4 lg:col-span-1">
            <div className="flex flex-wrap gap-2 mb-3">
              {SOURCES.map((s) => (
                <label key={s} className="inline-flex items-center gap-1 text-sm">
                  <input
                    type="checkbox"
                    checked={shown.has(s)}
                    onChange={(e) => {
                      const next = new Set(shown);
                      if (e.target.checked) next.add(s);
                      else next.delete(s);
                      setShown(next);
                    }}
                  />
                  <Badge kind={s}>{s}</Badge>
                </label>
              ))}
            </div>
            <input className={`${inputClass} mb-3`} placeholder="Filter by id or text" value={filter} onChange={(e) => setFilter(e.target.value)} />
            <ul className="max-h-[65vh] overflow-y-auto divide-y divide-gray-50">
              {visible.map((r) => (
                <li key={r.id}>
                  <button
                    className={`w-full text-left px-2 py-1.5 rounded text-sm flex items-center justify-between gap-2 ${
                      r.id === selected ? "bg-sky-50" : "hover:bg-gray-50"
                    }`}
                    onClick={() => select(r)}
                  >
                    <span className="font-mono truncate">{r.id}</span>
                    <span className="flex items-center gap-1 shrink-0">
                      {r.template && <Badge kind="muted">template</Badge>}
                      {r.site && !r.site.enabled && <Badge kind="muted">disabled</Badge>}
                      <Badge kind={r.source}>{r.source}</Badge>
                    </span>
                  </button>
                </li>
              ))}
              {visible.length === 0 && <li className="text-sm text-gray-500 px-2 py-2">Nothing matches.</li>}
            </ul>
          </div>

          <div className="lg:col-span-2">
            {draft && (editable || draft.version === 0) && host.writePolicy ? (
              <Card
                title={draft.version === 0 ? "New site policy" : `Edit ${draft.id}`}
                actions={draft.version > 0 && <span className="text-xs text-gray-500">version {draft.version}</span>}
              >
                <div className="space-y-3">
                  <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
                    <div>
                      <label htmlFor="policy-id" className="block text-sm font-medium text-gray-700">
                        Id (must start with site. and match @id)
                      </label>
                      <input
                        id="policy-id"
                        className={`${inputClass} font-mono`}
                        readOnly={draft.version > 0}
                        value={draft.id}
                        onChange={(e) => setDraft({ ...draft, id: e.target.value })}
                      />
                    </div>
                    <div>
                      <label htmlFor="policy-description" className="block text-sm font-medium text-gray-700">
                        Description
                      </label>
                      <input
                        id="policy-description"
                        className={inputClass}
                        value={draft.description}
                        onChange={(e) => setDraft({ ...draft, description: e.target.value })}
                      />
                    </div>
                  </div>
                  <label className="inline-flex items-center gap-2 text-sm">
                    <input type="checkbox" checked={draft.enabled} onChange={(e) => setDraft({ ...draft, enabled: e.target.checked })} />
                    Enabled
                  </label>
                  <textarea
                    aria-label="Policy text"
                    className="w-full h-72 px-3 py-2 font-mono text-sm border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500"
                    spellCheck={false}
                    value={draft.text}
                    onChange={(e) => setDraft({ ...draft, text: e.target.value })}
                  />
                  {status && (
                    <div
                      className={`p-3 rounded-lg text-sm ${
                        status.kind === "ok" ? "bg-green-50 text-green-800 border border-green-200" : "bg-red-50 text-red-700 border border-red-200"
                      }`}
                    >
                      {status.text}
                      {status.errors && status.errors.length > 0 && (
                        <ul className="mt-2 list-disc list-inside font-mono text-xs">
                          {status.errors.map((e, i) => (
                            <li key={i}>{e}</li>
                          ))}
                        </ul>
                      )}
                    </div>
                  )}
                  <div className="flex flex-wrap gap-2">
                    <button className={secondaryButton} disabled={busy} onClick={validate}>
                      Validate
                    </button>
                    <button className={primaryButton} disabled={busy} onClick={save}>
                      Save
                    </button>
                    {draft.version > 0 && (
                      <>
                        <button className={secondaryButton} onClick={() => setHistory(true)}>
                          History
                        </button>
                        <button className={`${secondaryButton} text-red-600`} disabled={busy} onClick={remove}>
                          Delete
                        </button>
                      </>
                    )}
                  </div>
                </div>
              </Card>
            ) : row ? (
              <Card
                title={row.id}
                actions={
                  <>
                    <Badge kind={row.source}>{row.source}</Badge>
                    {row.source === "site" && row.site && (
                      <button className={secondaryButton} onClick={() => setHistory(true)}>
                        History
                      </button>
                    )}
                  </>
                }
              >
                {row.site?.description && <p className="mb-2 text-sm text-gray-600">{row.site.description}</p>}
                {row.source === "file" && <p className="mb-2 text-xs text-gray-500">From a policy file on the host; read-only here.</p>}
                <pre className="text-xs font-mono bg-gray-50 p-3 rounded-lg overflow-x-auto whitespace-pre-wrap">{row.text}</pre>
                {row.site && (
                  <p className="mt-2 text-xs text-gray-500">
                    version {row.site.version} · updated {formatTime(row.site.updated_at)} by {row.site.updated_by.slice(0, 8)}
                  </p>
                )}
              </Card>
            ) : (
              <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">
                Choose a policy to view it{host.writePolicy ? ", or create a site policy" : ""}.
              </div>
            )}

            {draft && host.writePolicy && (
              <Card title="Simulate">
                <p className="mb-3 text-sm text-gray-500">
                  Decisions under the current policies and with this draft, before saving.
                </p>
                <RequestBuilder requests={requests} onChange={setRequests} />
                {requests.length > 0 && (
                  <table className="mt-4 w-full text-sm">
                    <thead>
                      <tr className="text-left text-gray-500 border-b border-gray-100">
                        <th className="py-2 font-medium">Request</th>
                        <th className="py-2 font-medium">Current</th>
                        <th className="py-2 font-medium">With draft</th>
                        <th className="py-2" />
                      </tr>
                    </thead>
                    <tbody>
                      {requests.map((q, i) => {
                        const r = results?.[i];
                        return (
                          <tr key={i} className="border-b border-gray-50 align-top">
                            <td className="py-2 font-mono text-xs">
                              {q.principal.type}:{"id" in q.principal ? q.principal.id.slice(0, 8) : ""} → {q.action} →{" "}
                              {q.resource.type}
                              {"id" in q.resource ? `:${q.resource.id.slice(0, 12)}` : ""}
                            </td>
                            {r && "error" in r ? (
                              <td colSpan={2} className="py-2 text-red-600 text-xs">
                                {r.error}
                              </td>
                            ) : r ? (
                              <>
                                <td className="py-2">
                                  <DecisionBadge allowed={r.current.allowed} />
                                  <div className="text-xs text-gray-500 font-mono">{r.current.policies.join(", ")}</div>
                                </td>
                                <td className="py-2">
                                  <DecisionBadge allowed={r.candidate.allowed} />
                                  {r.candidate.allowed !== r.current.allowed && (
                                    <span className="ml-1 text-xs font-medium text-amber-700">changed</span>
                                  )}
                                  <div className="text-xs text-gray-500 font-mono">{r.candidate.policies.join(", ")}</div>
                                </td>
                              </>
                            ) : (
                              <td colSpan={2} className="py-2 text-xs text-gray-400">
                                not run
                              </td>
                            )}
                            <td className="py-2 text-right">
                              <button
                                className="text-xs text-red-600 hover:underline"
                                onClick={() => {
                                  setRequests(requests.filter((_, j) => j !== i));
                                  setResults(null);
                                }}
                              >
                                Remove
                              </button>
                            </td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                )}
                <button className={`${primaryButton} mt-4`} disabled={busy || requests.length === 0} onClick={simulate}>
                  Run simulation
                </button>
              </Card>
            )}
          </div>
        </div>
      )}
      {history && (draft?.id ?? row?.id) && (
        <Modal title={`History of ${draft?.id ?? row?.id}`} onClose={() => setHistory(false)}>
          <History
            id={(draft?.id ?? row?.id) as string}
            onRestore={(v) => {
              const base = draft ?? {
                id: v.id,
                description: row?.site?.description ?? "",
                enabled: v.enabled,
                version: row?.site?.version ?? 0,
                text: v.text,
              };
              setDraft({ ...base, text: v.text, enabled: v.enabled });
              setHistory(false);
            }}
          />
        </Modal>
      )}
    </div>
  );
}
