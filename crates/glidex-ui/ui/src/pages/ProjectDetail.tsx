import { useCallback, useEffect, useState, type FormEvent } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import * as api from "../api";
import type { Binding, Network, NetworkShare, ProjectView, Quotas } from "../types";
import { PROJECT_ROLES, QUOTA_KEYS, QUOTA_LABELS, formatTime, notReady } from "../types";
import { useLiveRefresh } from "../live";
import { Loading } from "../components/Loading";
import AddBindingForm from "../components/AddBindingForm";
import {
  Badge,
  BindingTable,
  Card,
  ErrorBanner,
  dangerLink,
  errorMessage,
  inputClass,
  primaryButton,
  secondaryButton,
} from "../components/ui";
import { UsageCell } from "./Projects";
import { useCan, useSession } from "../session";
import { useDirectory } from "../hooks";

const CAPS = [
  "readBindings",
  "manageBindings",
  "updateProject",
  "deleteProject",
  "createProjectNetwork",
  "listNetworkShares",
] as const;
type Cap = (typeof CAPS)[number];

function QuotaEditor({ quotas, onSave, onCancel }: { quotas: Quotas; onSave: (q: Quotas) => Promise<void>; onCancel: () => void }) {
  const [values, setValues] = useState<Record<keyof Quotas, string>>(
    () => Object.fromEntries(QUOTA_KEYS.map((k) => [k, quotas[k] === null ? "" : String(quotas[k])])) as Record<keyof Quotas, string>,
  );
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const q = Object.fromEntries(
        QUOTA_KEYS.map((k) => [k, values[k].trim() === "" ? null : Number(values[k])]),
      ) as unknown as Quotas;
      await onSave(q);
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-3">
      <div className="grid grid-cols-2 md:grid-cols-3 gap-3">
        {QUOTA_KEYS.map((k) => (
          <div key={k}>
            <label htmlFor={`quota-${k}`} className="block text-sm font-medium text-gray-700">
              {QUOTA_LABELS[k]}
            </label>
            <input
              id={`quota-${k}`}
              className={inputClass}
              type="number"
              min={0}
              placeholder="unlimited"
              value={values[k]}
              onChange={(e) => setValues({ ...values, [k]: e.target.value })}
            />
          </div>
        ))}
      </div>
      <p className="text-xs text-gray-500">Leave a field empty for no limit.</p>
      {error && <p className="text-sm text-red-600">{error}</p>}
      <div className="flex gap-2">
        <button type="submit" className={primaryButton} disabled={busy}>
          Save quotas
        </button>
        <button type="button" className={secondaryButton} onClick={onCancel}>
          Cancel
        </button>
      </div>
    </form>
  );
}

function NetworkRow({
  net,
  manage,
  share,
  projectName,
  onChange,
  onError,
}: {
  net: Network;
  manage: boolean;
  share: boolean;
  projectName: (id: string) => string;
  onChange: () => void;
  onError: (e: string) => void;
}) {
  const [target, setTarget] = useState("");
  const [note, setNote] = useState<string | null>(null);

  const run = async (f: () => Promise<unknown>) => {
    try {
      await f();
      onChange();
    } catch (e) {
      onError(errorMessage(e));
    }
  };

  return (
    <div className="py-3 border-b border-gray-100 last:border-0">
      <div className="flex items-center justify-between gap-4">
        <div>
          <span className="font-medium text-gray-900">{net.name}</span>{" "}
          <span className="text-xs text-gray-500">
            {net.mode} on {net.bridge}
          </span>
          {net.deletion_requested_at && (
            <span className="ml-2 text-xs text-gray-500" title={notReady(net.conditions)?.message}>
              deleting{notReady(net.conditions) ? ` (${notReady(net.conditions)!.reason})` : ""}
            </span>
          )}
        </div>
        {manage && !net.deletion_requested_at && (
          <button
            className={dangerLink}
            onClick={() => confirm(`Delete network ${net.name}?`) && run(() => api.deleteNetwork(net.name))}
          >
            Delete
          </button>
        )}
      </div>
      {((net.shares ?? []).length > 0 || (net.share_offers ?? []).length > 0) && (
        <ul className="mt-2 space-y-1 text-sm">
          {(net.shares ?? []).map((p) => (
            <li key={`s-${p}`} className="flex items-center gap-2">
              <Badge kind="ok">shared</Badge>
              <span title={p}>{projectName(p)}</span>
              {share && (
                <button className={dangerLink} onClick={() => run(() => api.unshareNetwork(net.name, p))}>
                  Unshare
                </button>
              )}
            </li>
          ))}
          {(net.share_offers ?? []).map((o) => (
            <li key={`o-${o.project}`} className="flex items-center gap-2">
              <Badge kind="muted">offered</Badge>
              <span className="font-mono text-xs" title={o.project}>
                {projectName(o.project)}
              </span>
              <span className="text-xs text-gray-500">until {formatTime(o.expires_at)}</span>
              {share && (
                <button className={dangerLink} onClick={() => run(() => api.unshareNetwork(net.name, o.project))}>
                  Withdraw
                </button>
              )}
            </li>
          ))}
        </ul>
      )}
      {share && (
        <form
          className="mt-2 flex items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            const to = target.trim();
            if (!to) return;
            run(async () => {
              await api.offerNetworkShare(net.name, to);
              setTarget("");
              setNote(`Offered to ${to}. The other project's owner must accept it.`);
            });
          }}
        >
          <input
            className="px-2 py-1 border border-gray-300 rounded-lg text-sm font-mono w-80"
            placeholder="target project id"
            value={target}
            onChange={(e) => setTarget(e.target.value)}
          />
          <button type="submit" className={secondaryButton}>
            Offer share
          </button>
          {note && <span className="text-xs text-gray-500">{note}</span>}
        </form>
      )}
    </div>
  );
}

export default function ProjectDetail() {
  const { id = "" } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const session = useSession();
  const [dir] = useDirectory();
  const [project, setProject] = useState<ProjectView | null>(null);
  const [bindings, setBindings] = useState<Binding[] | null>(null);
  const [networks, setNetworks] = useState<Network[]>([]);
  const [shares, setShares] = useState<NetworkShare[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [editingQuotas, setEditingQuotas] = useState(false);
  const [newNetwork, setNewNetwork] = useState("");

  const allowed = useCan(CAPS.map((action) => ({ action, resource: { type: "Project" as const, id } })));
  const can = (c: Cap) => !!allowed?.[CAPS.indexOf(c)];

  const refresh = useCallback(() => {
    api
      .getProject(id)
      .then((p) => {
        setProject(p);
        setLoadError(null);
      })
      .catch((e) => setLoadError(errorMessage(e)));
    api
      .listNetworks()
      .then((ns) => setNetworks(ns.filter((n) => n.project === id)))
      .catch(() => setNetworks([]));
  }, [id]);

  const refreshBindings = useCallback(() => {
    api
      .listBindings(id)
      .then(setBindings)
      .catch((e) => setError(errorMessage(e)));
  }, [id]);

  const refreshShares = useCallback(() => {
    api
      .listNetworkShares(id)
      .then(setShares)
      .catch(() => setShares(null));
  }, [id]);

  useEffect(() => {
    refresh();
  }, [refresh]);
  // A network being deleted (waiting for netd, say) goes away by itself.
  useLiveRefresh(["network"], refresh);
  const canReadBindings = can("readBindings");
  const canShare = can("listNetworkShares");
  useEffect(() => {
    if (canReadBindings) refreshBindings();
  }, [canReadBindings, refreshBindings]);
  useEffect(() => {
    if (canShare) refreshShares();
  }, [canShare, refreshShares]);

  if (loadError) {
    return (
      <div className="text-center py-12">
        <p className="text-red-500">{loadError}</p>
        <Link to="/projects" className="mt-4 inline-block text-sky-700 hover:underline">
          Back to projects
        </Link>
      </div>
    );
  }
  if (!project || allowed === null) return <Loading />;

  const run = async (f: () => Promise<unknown>, after: () => void) => {
    setError(null);
    try {
      await f();
      after();
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const projectName = (pid: string) => session.projectName(pid);

  return (
    <div>
      <Link to="/projects" className="text-sky-600 hover:text-sky-700 text-sm">
        ← Projects
      </Link>
      <div className="flex items-center justify-between mt-2 mb-6 gap-4">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">{project.name}</h1>
          {project.description && <p className="text-gray-500 mt-1">{project.description}</p>}
          <p className="mt-1 text-xs text-gray-500">
            Project id <code className="font-mono bg-gray-100 px-1 rounded select-all">{project.id}</code> · created{" "}
            {formatTime(project.created_at)}
          </p>
        </div>
        <div className="flex items-center gap-2">
          {session.project !== project.id && (
            <button className={secondaryButton} onClick={() => session.selectProject(project.id)}>
              Select this project
            </button>
          )}
          {can("deleteProject") && (
            <button
              className={secondaryButton}
              onClick={() =>
                confirm(`Delete project ${project.name}? It must not own any VMs, disks or credentials.`) &&
                run(
                  () => api.deleteProject(project.id),
                  () => {
                    session.refresh();
                    navigate("/projects");
                  },
                )
              }
            >
              Delete project
            </button>
          )}
        </div>
      </div>

      <ErrorBanner error={error} onDismiss={() => setError(null)} />

      <Card
        title="Quotas and usage"
        actions={
          can("updateProject") && !editingQuotas ? (
            <button className={secondaryButton} onClick={() => setEditingQuotas(true)}>
              Edit quotas
            </button>
          ) : undefined
        }
      >
        {editingQuotas ? (
          <QuotaEditor
            quotas={project.quotas}
            onCancel={() => setEditingQuotas(false)}
            onSave={async (quotas) => {
              await api.updateProject(project.id, { quotas });
              setEditingQuotas(false);
              refresh();
            }}
          />
        ) : (
          <div className="grid grid-cols-2 md:grid-cols-6 gap-4">
            {QUOTA_KEYS.map((k) => (
              <div key={k}>
                <div className="text-xs text-gray-500">{QUOTA_LABELS[k]}</div>
                <div className="text-lg">
                  <UsageCell used={project.usage[k]} limit={project.quotas[k]} />
                </div>
              </div>
            ))}
          </div>
        )}
      </Card>

      {canReadBindings && (
        <Card title="Members">
          {bindings === null ? (
            <Loading />
          ) : (
            <BindingTable
              bindings={bindings}
              dir={dir ?? { users: [], teams: [] }}
              onRemove={
                can("manageBindings")
                  ? (b) => run(() => api.removeBinding(project.id, b.id), refreshBindings)
                  : undefined
              }
            />
          )}
          {can("manageBindings") && (
            <AddBindingForm
              roles={PROJECT_ROLES}
              dir={dir}
              onAdd={async (role, principal) => {
                await api.addBinding(project.id, role, principal);
                refreshBindings();
              }}
            />
          )}
        </Card>
      )}

      <Card title="Project networks">
        {networks.length === 0 ? (
          <p className="text-sm text-gray-500">This project has no networks of its own.</p>
        ) : (
          networks.map((n) => (
            <NetworkRow
              key={n.name}
              net={n}
              manage={can("createProjectNetwork")}
              share={canShare}
              projectName={projectName}
              onChange={refresh}
              onError={setError}
            />
          ))
        )}
        {can("createProjectNetwork") && (
          <form
            className="mt-4 flex items-end gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              run(
                () => api.createProjectNetwork(project.id, { name: newNetwork.trim(), mode: "nat" }),
                () => {
                  setNewNetwork("");
                  refresh();
                },
              );
            }}
          >
            <div>
              <label htmlFor="new-network" className="block text-sm font-medium text-gray-700">
                New NAT network
              </label>
              <input
                id="new-network"
                className={inputClass}
                required
                placeholder="name"
                value={newNetwork}
                onChange={(e) => setNewNetwork(e.target.value)}
              />
            </div>
            <button type="submit" className={primaryButton}>
              Create
            </button>
          </form>
        )}
      </Card>

      {canShare && (
        <Card title="Networks shared with this project">
          {shares === null || shares.length === 0 ? (
            <p className="text-sm text-gray-500">No offers or accepted shares.</p>
          ) : (
            <table className="w-full text-sm">
              <thead>
                <tr className="text-left text-gray-500 border-b border-gray-100">
                  <th className="py-2 font-medium">Network</th>
                  <th className="py-2 font-medium">From project</th>
                  <th className="py-2 font-medium">Status</th>
                  <th className="py-2" />
                </tr>
              </thead>
              <tbody>
                {shares.map((s) => (
                  <tr key={s.network} className="border-b border-gray-50">
                    <td className="py-2 font-medium">{s.network}</td>
                    <td className="py-2" title={s.owner_project ?? ""}>
                      {s.owner_project ? projectName(s.owner_project) : "—"}
                    </td>
                    <td className="py-2">
                      {s.status === "accepted" ? (
                        <Badge kind="ok">accepted</Badge>
                      ) : (
                        <>
                          <Badge kind="muted">offered</Badge>{" "}
                          <span className="text-xs text-gray-500">until {formatTime(s.expires_at)}</span>
                        </>
                      )}
                    </td>
                    <td className="py-2 text-right">
                      {s.status === "offered" ? (
                        <button
                          className={secondaryButton}
                          onClick={() => run(() => api.acceptNetworkShare(project.id, s.network), refreshShares)}
                        >
                          Accept
                        </button>
                      ) : (
                        <button
                          className={dangerLink}
                          onClick={() => run(() => api.leaveNetworkShare(project.id, s.network), refreshShares)}
                        >
                          Leave
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </Card>
      )}
    </div>
  );
}
