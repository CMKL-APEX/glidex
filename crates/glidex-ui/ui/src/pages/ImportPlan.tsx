import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import * as api from "../api";
import type { ImportMappings, ImportPlanView, ProjectView } from "../types";
import { formatTime } from "../types";
import { useSession } from "../session";
import { useCan } from "../session";
import ConfirmDialog from "../components/ConfirmDialog";
import { LoadingCard } from "../components/Loading";
import { Badge, Card, ErrorBanner, PageHeader, errorMessage, inputClass, primaryButton, secondaryButton, selectClass } from "../components/ui";

const empty: ImportMappings = { projects: {}, networks: {}, credentials: {}, over_quota: false };

/** One import plan (spec/clustering-ui.md §7.4): what a host brings, the
 * conflicts, names for what clashes, a check, then approve or reject. */
export default function ImportPlan() {
  const { plan = "" } = useParams<{ plan: string }>();
  const navigate = useNavigate();
  const { projects, host } = useSession();
  const [view, setView] = useState<ImportPlanView | null>(null);
  const [m, setM] = useState<ImportMappings>(empty);
  const [checked, setChecked] = useState<{ problems: string[]; key: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState<"approve" | "reject" | null>(null);
  const key = JSON.stringify(m);

  const load = useCallback(async () => {
    try {
      const v = await api.getImport(plan);
      setView(v);
      setM((cur) => (cur === empty ? { ...empty, ...v.mappings } : cur));
    } catch (e) {
      setError(errorMessage(e));
    }
  }, [plan]);
  useEffect(() => {
    load();
  }, [load]);

  // Exceeding quotas needs `exceedQuota` on each project it merges into; offer it only to those who may.
  const targets = Object.values(m.projects).filter((t) => t && !t.startsWith("new:"));
  const canExceed = useCan(targets.map((t) => ({ action: "exceedQuota", resource: { type: "Project" as const, id: projects.find((p) => p.name === t || p.id === t)?.id ?? t } })));

  if (!view) return error ? <ErrorBanner error={error} /> : <LoadingCard />;
  const pending = view.state === "pending";
  const problems = checked?.key === key ? checked.problems : view.problems;
  const checkedNow = checked?.key === key;
  const src = view.sources;

  const check = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await api.checkImport(plan, m);
      setChecked({ problems: r.problems, key });
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div>
      <p className="mb-2 text-sm">
        <Link to="/cluster" className="text-sky-700 hover:underline">
          ← Cluster
        </Link>
      </p>
      <PageHeader
        title={`Import ${view.node}`}
        subtitle={
          <span className="inline-flex flex-wrap gap-2 items-center">
            <Badge kind={pending ? "warn" : view.state === "committed" ? "ok" : "muted"}>{view.state}</Badge>
            <span>
              from {view.advertise}, asked by {view.created_by} {formatTime(view.created_at)}; expires {formatTime(view.expires_at)}
            </span>
          </span>
        }
      />
      <ErrorBanner error={error} onDismiss={() => setError(null)} />

      <Card title="What it brings">
        <p className="text-sm text-gray-700">
          {view.summary.vms} VMs, {view.summary.disks} disks, {view.summary.images} images, {view.summary.networks} node networks,{" "}
          {view.summary.credentials} credentials, {view.summary.projects} projects. Nothing is live in the cluster until it is approved; the VMs
          keep running on the host either way.
        </p>
        {src && src.vms.length > 0 && (
          <p className="mt-2 text-xs text-gray-500">VMs: {src.vms.map((v) => v.name).join(", ")}</p>
        )}
      </Card>

      {pending && src && (
        <Card title="Names here">
          <div className="space-y-4 text-sm">
            {src.projects.length > 0 && (
              <div>
                <p className="font-medium mb-1">Projects</p>
                {src.projects.map((p) => (
                  <ProjectMapping key={p.id} source={p} projects={projects} value={m.projects[p.id] ?? ""} onChange={(v) => setM({ ...m, projects: set(m.projects, p.id, v) })} />
                ))}
              </div>
            )}
            {src.networks.length > 0 && (
              <div>
                <p className="font-medium mb-1">Node networks (renamed if the name is taken)</p>
                {src.networks.map((n) => (
                  <div key={n.name} className="grid grid-cols-2 gap-2 items-center mb-1">
                    <span className="font-mono text-xs">{n.name}</span>
                    <input className={inputClass} placeholder={n.name} value={m.networks[n.name] ?? ""} onChange={(e) => setM({ ...m, networks: set(m.networks, n.name, e.target.value) })} />
                  </div>
                ))}
              </div>
            )}
            {src.credentials.length > 0 && (
              <div>
                <p className="font-medium mb-1">Credentials (renamed if the name is taken in its project)</p>
                {src.credentials.map((c) => (
                  <div key={c.key} className="grid grid-cols-2 gap-2 items-center mb-1">
                    <span className="font-mono text-xs">{c.username}</span>
                    <input className={inputClass} placeholder={c.username} value={m.credentials[c.key] ?? ""} onChange={(e) => setM({ ...m, credentials: set(m.credentials, c.key, e.target.value) })} />
                  </div>
                ))}
              </div>
            )}
            {canExceed && canExceed.length > 0 && canExceed.every(Boolean) && (
              <label className="flex items-start gap-2">
                <input type="checkbox" className="mt-1" checked={m.over_quota} onChange={(e) => setM({ ...m, over_quota: e.target.checked })} />
                <span>Accept it even if it takes a project over its quota.</span>
              </label>
            )}
          </div>
        </Card>
      )}

      <Card title={problems.length ? `Problems (${problems.length})` : "Problems"}>
        {problems.length === 0 ? (
          <p className="text-sm text-green-700">{checkedNow ? "The check passed with these names." : "None reported. Check again before approving."}</p>
        ) : (
          <ul className="text-sm text-red-700 list-disc pl-5 space-y-1">
            {problems.map((p) => (
              // The server's hints name gxctl flags; the fields above set the same.
              <li key={p}>{p}</li>
            ))}
          </ul>
        )}
      </Card>

      {pending && (
        <div className="flex gap-2 justify-end">
          {host.rejectImport && (
            <button className={secondaryButton} onClick={() => setConfirm("reject")}>
              Reject
            </button>
          )}
          <button className={secondaryButton} disabled={busy} onClick={check}>
            {busy ? "Checking..." : "Check"}
          </button>
          {host.approveImport && (
            <button className={primaryButton} disabled={!checkedNow || problems.length > 0} onClick={() => setConfirm("approve")} title={checkedNow ? "" : "Check these names first"}>
              Approve
            </button>
          )}
        </div>
      )}

      {confirm === "approve" && (
        <ConfirmDialog
          title={`Approve the import of ${view.node}`}
          consequence="The host joins the cluster and its VMs, disks and networks become the cluster's, under the names above, in one step. Its VMs keep running."
          typeToConfirm={view.node}
          actionLabel="Approve"
          danger={false}
          onConfirm={async () => {
            await api.approveImport(plan, m);
            await load();
          }}
          onClose={() => setConfirm(null)}
        />
      )}
      {confirm === "reject" && (
        <ConfirmDialog
          title={`Reject the import of ${view.node}`}
          consequence="The host is told no and stays as it is, standalone, with everything it has."
          actionLabel="Reject"
          onConfirm={async () => {
            await api.rejectImport(plan);
            navigate("/cluster");
          }}
          onClose={() => setConfirm(null)}
        />
      )}
    </div>
  );
}

function set(map: Record<string, string>, k: string, v: string): Record<string, string> {
  const next = { ...map };
  if (v.trim()) next[k] = v.trim();
  else delete next[k];
  return next;
}

/** A source project lands in an existing project, a new one, or (default) itself. */
function ProjectMapping({ source, projects, value, onChange }: { source: { id: string; name: string }; projects: ProjectView[]; value: string; onChange: (v: string) => void }) {
  const isNew = value.startsWith("new:");
  return (
    <div className="grid grid-cols-3 gap-2 items-center mb-1">
      <span>
        {source.name} <span className="font-mono text-xs text-gray-400">{source.id.slice(0, 8)}</span>
      </span>
      <select aria-label={`Where ${source.name} goes`} className={selectClass} value={isNew ? "__new" : value} onChange={(e) => onChange(e.target.value === "__new" ? `new:${source.name}` : e.target.value)}>
        <option value="">as is (a new project if it isn't here)</option>
        {projects.map((p) => (
          <option key={p.id} value={p.id}>
            merge into {p.name}
          </option>
        ))}
        <option value="__new">a new project named…</option>
      </select>
      {isNew ? <input className={inputClass} value={value.slice(4)} onChange={(e) => onChange(`new:${e.target.value}`)} /> : <span />}
    </div>
  );
}
