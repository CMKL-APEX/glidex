import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import { useSession } from "../session";
import type { Datastore, DatastoreNode } from "../types";
import { formatBytes, num } from "../types";
import { useLiveRefresh } from "../live";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";
import NodeName from "../components/NodeName";
import { useNodes } from "../nodes";

const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";

/** Where registered datastores are mounted (spec/24-storage.md S11). */
const ROOT = "/srv/glidex";

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** How many nodes reach it, and the most free space any of them has. */
function reach(d: Datastore): { ok: number; all: number; free: number | null } {
  const rows = d.node_status ?? [];
  const ok = rows.filter((r) => r.reachable);
  return { ok: ok.length, all: rows.length, free: ok.length ? Math.max(...ok.map((r) => num(r.free_bytes))) : null };
}

function RowStatus({ r }: { r: DatastoreNode }) {
  if (r.reachable && (r.warnings ?? []).length)
    return (
      <span className="text-amber-700" title={(r.warnings ?? []).join("\n")}>
        reachable, with warnings
      </span>
    );
  if (r.reachable) return <span className="text-green-700">reachable</span>;
  return (
    <span className="text-red-700" title={r.message}>
      {r.reason || "unreachable"}
    </span>
  );
}

/** `node=path` lines, as the form takes per-node paths. */
function parsePaths(text: string): Record<string, string> | string {
  const out: Record<string, string> = {};
  for (const line of text.split("\n").map((l) => l.trim()).filter(Boolean)) {
    const i = line.indexOf("=");
    if (i <= 0) return `"${line}": write node=path`;
    out[line.slice(0, i).trim()] = line.slice(i + 1).trim();
  }
  return out;
}

function CreateForm({ onDone, onCancel }: { onDone: () => void; onCancel: () => void }) {
  const dir = useNodes();
  const [name, setName] = useState("");
  const [path, setPath] = useState("");
  const [onNodes, setOnNodes] = useState<string[]>([]);
  const [paths, setPaths] = useState("");
  const [isDefault, setIsDefault] = useState(false);
  const [kind, setKind] = useState<"dir" | "shared" | "lvm" | "hyperstor">("dir");
  const [vg, setVg] = useState("");
  const [pool, setPool] = useState("");
  const lvm = kind === "lvm";
  const hyperstor = kind === "hyperstor";
  const [hsApi, setHsApi] = useState("");
  const [hsPool, setHsPool] = useState("1");
  const [hsToken, setHsToken] = useState("");
  const [overcommit, setOvercommit] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const node_paths = parsePaths(paths);
    if (typeof node_paths === "string") return setError(node_paths);
    setSubmitting(true);
    setError(null);
    try {
      await api.createDatastore({
        name,
        ...(hyperstor
          ? {
              hyperstor: true,
              hyperstor_api: hsApi.split(",").map((a) => a.trim()).filter(Boolean),
              hyperstor_pool: Number(hsPool),
              hyperstor_token: hsToken.trim(),
            }
          : lvm
            ? { lvm: true, volume_group: vg, thin_pool: pool }
            : { path: path || `${ROOT}/${name}`, shared: kind === "shared" }),
        node_paths,
        nodes: onNodes,
        default_for_disks: isDefault,
        overcommit,
      });
      onDone();
    } catch (err) {
      setError(errorText(err));
      setSubmitting(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <div className="flex space-x-4 text-sm">
        {(
          [
            ["dir", "Directory on each node"],
            ["shared", "Shared filesystem"],
            ["lvm", "LVM-thin on each node"],
            ["hyperstor", "hyperstor cluster"],
          ] as const
        ).map(([k, label]) => (
          <label key={k} className="flex items-center space-x-1">
            <input type="radio" name="kind" checked={kind === k} onChange={() => setKind(k)} />
            <span>{label}</span>
          </label>
        ))}
      </div>
      <p className="text-sm text-gray-600">
        {hyperstor ? (
          <>
            A pool of a hyperstor cluster. Its volumes reach each node's VMs through{" "}
            <span className="font-mono">hyperstor-vhost</span>, so a VM can run on any node that reaches it. glidex enrolls with a
            token from <span className="font-mono">hyperstorctl token create --role mgmt --name glidex</span>, which isn't kept.
          </>
        ) : lvm ? (
          <>
            A volume group and thin pool you made on each node, allowed in glidex-stord's{" "}
            <span className="font-mono">/etc/glidex/stord.json</span>. Disks are raw volumes in it; each node checks it and says
            what it finds.
          </>
        ) : (
          <>
            A directory under <span className="font-mono">{ROOT}</span> on each node, owned by the glidex user: a local disk or a
            mount you made (NFS, CephFS, a vendor's agent). glidex never mounts it; each node checks it and says what it finds.
          </>
        )}
      </p>
      <div>
        <label className="block text-sm font-medium text-gray-700">Name</label>
        <input className={inputClass} required pattern="[A-Za-z0-9_\-][A-Za-z0-9._\-]{0,63}" value={name} onChange={(e) => setName(e.target.value)} />
      </div>
      {hyperstor ? (
        <div className="space-y-3">
          <div className="grid grid-cols-3 gap-3">
            <div className="col-span-2">
              <label className="block text-sm font-medium text-gray-700">Cluster API</label>
              <input className={`${inputClass} font-mono`} required placeholder="10.0.0.5:8850, 10.0.0.6:8850" value={hsApi} onChange={(e) => setHsApi(e.target.value)} />
            </div>
            <div>
              <label className="block text-sm font-medium text-gray-700">Pool</label>
              <input className={`${inputClass} font-mono`} required type="number" min={1} value={hsPool} onChange={(e) => setHsPool(e.target.value)} />
            </div>
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-700">Token</label>
            <input className={`${inputClass} font-mono`} required type="password" autoComplete="off" placeholder="hsj1-…" value={hsToken} onChange={(e) => setHsToken(e.target.value)} />
          </div>
        </div>
      ) : lvm ? (
        <div className="grid grid-cols-2 gap-3">
          <div>
            <label className="block text-sm font-medium text-gray-700">Volume group</label>
            <input className={`${inputClass} font-mono`} required placeholder="vg-glidex" value={vg} onChange={(e) => setVg(e.target.value)} />
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-700">Thin pool</label>
            <input className={`${inputClass} font-mono`} required placeholder="pool" value={pool} onChange={(e) => setPool(e.target.value)} />
          </div>
        </div>
      ) : (
        <div>
          <label className="block text-sm font-medium text-gray-700">Path</label>
          <input className={`${inputClass} font-mono`} placeholder={`${ROOT}/${name || "<name>"}`} value={path} onChange={(e) => setPath(e.target.value)} />
        </div>
      )}
      {dir.clustered && dir.nodes.size > 0 && (
        <div>
          <label className="block text-sm font-medium text-gray-700">Nodes</label>
          <p className="text-xs text-gray-500">None ticked: every node.</p>
          <div className="mt-1 grid grid-cols-2 gap-1">
            {[...dir.nodes.values()].map((n) => (
              <label key={n.meta.id} className="flex items-center space-x-2 text-sm">
                <input
                  type="checkbox"
                  checked={onNodes.includes(n.meta.id)}
                  onChange={(e) => setOnNodes(e.target.checked ? [...onNodes, n.meta.id] : onNodes.filter((x) => x !== n.meta.id))}
                />
                <span>{n.spec.name}</span>
              </label>
            ))}
          </div>
        </div>
      )}
      {dir.clustered && !hyperstor && (
        <div>
          <label className="block text-sm font-medium text-gray-700">{lvm ? "Volume groups of their own" : "Paths of their own"}</label>
          <textarea
            className={`${inputClass} font-mono text-xs`}
            rows={2}
            placeholder={lvm ? "node-2=vg-nvme" : `node-2=${ROOT}/fast-ssd`}
            value={paths}
            onChange={(e) => setPaths(e.target.value)}
          />
          <p className="text-xs text-gray-500">
            {lvm ? "One node=volume-group per line, for nodes whose group is named otherwise." : "One node=path per line, for nodes that mount it somewhere else."}
          </p>
        </div>
      )}
      {kind === "shared" && (
        <p className="text-sm text-gray-600">
          Shared: one filesystem every node mounts (NFS, CephFS). Disks on it aren't tied to a node, and their VMs can run on any
          node that reaches it. The directory must be empty; glidex marks it as this datastore's.
        </p>
      )}
      <label className="flex items-center space-x-2 text-sm">
        <input type="checkbox" checked={isDefault} onChange={(e) => setIsDefault(e.target.checked)} />
        <span>New disks go here unless they name a datastore</span>
      </label>
      <label className="flex items-center space-x-2 text-sm">
        <input type="checkbox" checked={overcommit} onChange={(e) => setOvercommit(e.target.checked)} />
        <span>Overcommit: disks may promise more than the free space (thin {hyperstor ? "volumes" : "files"})</span>
      </label>
      <div className="flex justify-end space-x-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" disabled={submitting} className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50">
          {submitting ? "Adding…" : "Add datastore"}
        </button>
      </div>
    </form>
  );
}

function Detail({ d }: { d: Datastore }) {
  const dir = useNodes();
  const rows = d.node_status ?? [];
  const own = Object.entries(d.node_paths ?? {});
  return (
    <div className="text-sm space-y-3">
      {!d.builtin && (
        <div className="space-y-1">
          <div>
            {d.driver === "DATASTORE_DRIVER_HYPERSTOR" ? (
              <>
                hyperstor: pool <span className="font-mono text-xs">{d.hyperstor_pool}</span> of{" "}
                <span className="font-mono text-xs">{(d.hyperstor_api ?? []).join(", ")}</span>
                {d.hyperstor_identity && (
                  <div>
                    As <span className="font-mono text-xs">{d.hyperstor_identity}</span>
                    {d.hyperstor_identity_expires && <>, certificate until {new Date(d.hyperstor_identity_expires).toLocaleString()}</>}
                  </div>
                )}
              </>
            ) : d.driver === "DATASTORE_DRIVER_LVM" ? (
              <>
                LVM-thin: <span className="font-mono text-xs">{d.volume_group}</span> pool{" "}
                <span className="font-mono text-xs">{d.thin_pool}</span>
              </>
            ) : (
              <>
                Path: <span className="font-mono text-xs">{d.path}</span>
              </>
            )}
          </div>
          {own.map(([n, p]) => (
            <div key={n}>
              On <NodeName id={n} />: <span className="font-mono text-xs">{p}</span>
            </div>
          ))}
          <div>Nodes: {(d.nodes ?? []).length ? (d.nodes ?? []).map((n) => dir.name(n)).join(", ") : "every node"}</div>
        </div>
      )}
      <div>Overcommit: {d.overcommit ? "yes (thin files)" : "no: a disk that doesn't fit is refused"}</div>
      {rows.length === 0 ? (
        <p className="text-gray-500">No node has looked at it yet.</p>
      ) : (
        <table className="w-full text-xs">
          <thead className="text-gray-500 text-left">
            <tr>
              <th className="py-1">Node</th>
              <th>Status</th>
              <th>Free</th>
              <th>Path</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.node}>
                <td className="py-1">
                  <NodeName id={r.node} name={r.node_name} />
                </td>
                <td>
                  <RowStatus r={r} />
                  {!r.reachable && r.message && <div className="text-gray-500 max-w-xs truncate">{r.message}</div>}
                  {(r.warnings ?? []).map((w) => (
                    <div key={w} className="text-amber-700 max-w-xs">
                      {w}
                    </div>
                  ))}
                </td>
                <td>{r.reachable ? `${formatBytes(r.free_bytes)} of ${formatBytes(r.total_bytes)}` : "—"}</td>
                <td className="font-mono break-all">{r.path}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}

export default function Datastores() {
  const { host } = useSession();
  const [all, setAll] = useState<Datastore[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [detail, setDetail] = useState<Datastore | null>(null);

  const refresh = useCallback(() => {
    api.listDatastores().then(setAll).catch((e) => setError(errorText(e)));
  }, []);
  useEffect(() => {
    refresh();
  }, [refresh]);
  const live = useLiveRefresh(["datastore", "disk"], refresh);
  useEffect(() => {
    if (live) return;
    const id = setInterval(refresh, 15000);
    return () => clearInterval(id);
  }, [live, refresh]);

  const act = async (f: () => Promise<unknown>) => {
    setError(null);
    try {
      await f();
      refresh();
    } catch (e) {
      setError(errorText(e));
    }
  };

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">Datastores</h1>
          <p className="text-sm text-gray-500 mt-1">
            Where disks live. <span className="font-mono">local</span> is each node's own disk directory; others are
            directories under <span className="font-mono">{ROOT}</span> that each node checks.
          </p>
        </div>
        {host.manageDatastore && (
          <button className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg" onClick={() => setAdding(true)}>
            Add Datastore
          </button>
        )}
      </div>

      {error && <div className="mb-4 p-4 bg-red-50 border border-red-200 rounded-lg text-red-700">{error}</div>}

      {all === null ? (
        <LoadingCard />
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-hidden">
          <table className="w-full text-sm">
            <thead className="bg-gray-50 text-gray-600 text-left">
              <tr>
                <th className="px-4 py-3 font-medium">Name</th>
                <th className="px-4 py-3 font-medium">Path</th>
                <th className="px-4 py-3 font-medium">Nodes reaching it</th>
                <th className="px-4 py-3 font-medium">Free</th>
                <th className="px-4 py-3 font-medium">Disks</th>
                <th className="px-4 py-3" />
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {all.map((d) => {
                const r = reach(d);
                return (
                  <tr key={d.id}>
                    <td className="px-4 py-3">
                      <button className="font-mono hover:underline" onClick={() => setDetail(d)}>
                        {d.name}
                      </button>
                      {d.builtin && <span className="ml-2 text-xs text-gray-500">built in</span>}
                      {d.topology === "DATASTORE_TOPOLOGY_SHARED" && <span className="ml-2 text-xs text-sky-700">shared</span>}
                      {d.default_for_disks && <span className="ml-2 text-xs text-green-700">default for disks</span>}
                      {!d.overcommit && <span className="ml-2 text-xs text-amber-700">no overcommit</span>}
                    </td>
                    <td className="px-4 py-3 font-mono text-xs">{d.builtin
                        ? "each node's disk directory"
                        : d.driver === "DATASTORE_DRIVER_HYPERSTOR"
                          ? `hyperstor pool ${d.hyperstor_pool}`
                          : d.driver === "DATASTORE_DRIVER_LVM"
                            ? `lvm ${d.volume_group}/${d.thin_pool}`
                            : d.path}</td>
                    <td className={`px-4 py-3 ${r.ok < r.all ? "text-amber-700" : ""}`}>
                      {r.all === 0 ? "—" : `${r.ok} of ${r.all}`}
                    </td>
                    <td className="px-4 py-3">{r.free === null ? "—" : formatBytes(r.free)}</td>
                    <td className="px-4 py-3">{d.disks ?? 0}</td>
                    <td className="px-4 py-3 text-right space-x-2 whitespace-nowrap">
                      {host.manageDatastore && !d.default_for_disks && (
                        <button
                          className="px-3 py-1 text-xs font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg"
                          onClick={() =>
                            act(async () => {
                              // `local` is the default when no other is.
                              const current = all.find((o) => o.default_for_disks && !o.builtin);
                              if (d.builtin) {
                                if (current) await api.updateDatastore(current.id, { default_for_disks: false });
                              } else {
                                await api.updateDatastore(d.id, { default_for_disks: true });
                              }
                            })
                          }
                        >
                          Make default
                        </button>
                      )}
                      {host.manageDatastore && !d.builtin && (
                        <button
                          className="px-3 py-1 text-xs font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg"
                          onClick={() => act(() => api.updateDatastore(d.id, { overcommit: !d.overcommit }))}
                        >
                          {d.overcommit ? "Stop overcommit" : "Allow overcommit"}
                        </button>
                      )}
                      {host.manageDatastore && !d.builtin && (
                        <button
                          className="px-3 py-1 text-xs font-medium text-red-700 bg-red-50 hover:bg-red-100 rounded-lg disabled:opacity-40"
                          disabled={(d.disks ?? 0) > 0}
                          title={(d.disks ?? 0) > 0 ? "Disks are on it" : ""}
                          onClick={() => confirm(`Delete datastore ${d.name}? Its directory and files stay.`) && act(() => api.deleteDatastore(d.id))}
                        >
                          Delete
                        </button>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {adding && (
        <Modal title="Add Datastore" onClose={() => setAdding(false)}>
          <CreateForm
            onCancel={() => setAdding(false)}
            onDone={() => {
              setAdding(false);
              refresh();
            }}
          />
        </Modal>
      )}
      {detail && (
        <Modal title={detail.name} onClose={() => setDetail(null)}>
          <Detail d={all?.find((d) => d.id === detail.id) ?? detail} />
        </Modal>
      )}
    </div>
  );
}
