import { useCallback, useEffect, useState, type FormEvent } from "react";
import { Link } from "react-router-dom";
import * as api from "../api";
import type { Network, OvnOverview, Router } from "../types";
import { useNodes } from "../nodes";
import { useSession } from "../session";
import ConfirmDialog from "./ConfirmDialog";
import Modal from "./Modal";
import { Badge, Card, ErrorBanner, dangerLink, errorMessage, inputClass, primaryButton, secondaryButton } from "./ui";

/** Create a VPC router. Its address and gateway nodes are site resources:
 * offered only to whoever may create host networks. */
function NewRouterDialog({ project, pool, onClose, onDone }: { project: string; pool: boolean; onClose: () => void; onDone: () => void }) {
  const { host } = useSession();
  const nodes = useNodes();
  const [name, setName] = useState("");
  const [external, setExternal] = useState(pool);
  const [address, setAddress] = useState("");
  const [gateways, setGateways] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const site = host.createNetwork;
  const nodeNames = [...nodes.nodes.values()].map((n) => n.spec.name).sort();

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.createRouter(project, {
        name,
        external,
        external_ip: external && site && address.trim() ? address.trim() : null,
        gateway_nodes: external && site && gateways.length ? gateways : null,
      });
      onDone();
      onClose();
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  return (
    <Modal title="New router" onClose={onClose}>
      <form onSubmit={submit} className="space-y-4 text-sm">
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        <div>
          <label htmlFor="router-name" className="block font-medium text-gray-700">
            Name
          </label>
          <input
            id="router-name"
            className={inputClass}
            required
            pattern="[a-z0-9][a-z0-9\-]{0,31}"
            title="Lowercase letters, digits and -"
            placeholder="r1"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </div>
        <label className="flex items-start gap-2">
          <input type="checkbox" className="mt-1" checked={external} disabled={!pool} onChange={(e) => setExternal(e.target.checked)} />
          <span>
            Reaches outside, with its own address
            <span className="block text-xs text-gray-500">
              {pool
                ? "Its networks' traffic leaves with an address of its own from the cluster's pool, not the shared edge's. Counts against the project's external IPs."
                : "Not possible yet: the cluster's shared edge has no pool of addresses for routers (OVN settings on the Cluster page). Without it, the router only joins its networks."}
            </span>
          </span>
        </label>
        {external && site && (
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label htmlFor="router-address" className="block text-xs text-gray-500">
                Address (optional)
              </label>
              <input id="router-address" className={inputClass} placeholder="next free in the pool" value={address} onChange={(e) => setAddress(e.target.value)} />
            </div>
            <div>
              <span className="block text-xs text-gray-500">Gateway nodes (optional)</span>
              <div className="flex flex-wrap gap-2 mt-1">
                {nodeNames.map((n) => (
                  <label key={n} className="flex items-center gap-1">
                    <input
                      type="checkbox"
                      checked={gateways.includes(n)}
                      onChange={(e) => setGateways(e.target.checked ? [...gateways, n] : gateways.filter((g) => g !== n))}
                    />
                    {n}
                  </label>
                ))}
              </div>
              <p className="text-xs text-gray-500 mt-1">In priority order of ticking; none: the shared edge's.</p>
            </div>
          </div>
        )}
        {!external && <p className="text-xs text-gray-500">Its networks reach each other and nothing else.</p>}
        <div className="flex justify-end gap-2">
          <button type="button" className={secondaryButton} onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className={primaryButton} disabled={busy}>
            {busy ? "Creating..." : "Create"}
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** A project's VPC routers (spec/clustering.md §11.2a): its own way out,
 * instead of the shared edge, for the NAT networks attached to it. */
export default function RoutersCard({ project, networks, manage, onChanged }: { project: string; networks: Network[]; manage: boolean; onChanged?: () => void }) {
  const { host } = useSession();
  const [routers, setRouters] = useState<Router[] | null>(null);
  const [pool, setPool] = useState(false);
  const [ovn, setOvn] = useState<boolean | null>(null);
  const [overview, setOverview] = useState<OvnOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<Router | null>(null);

  const load = useCallback(() => {
    api
      .listRouters(project)
      .then(setRouters)
      .catch((e) => setError(errorMessage(e)));
    if (host.readCluster) {
      api
        .clusterOvn()
        .then(setOverview)
        .catch(() => setOverview(null));
    }
  }, [project, host.readCluster]);
  // After a create or delete: this list, and the page's networks' labels.
  const changed = () => {
    load();
    onChanged?.();
  };
  useEffect(() => {
    api
      .getOvnSettings()
      .then((v) => {
        setOvn(v.enabled);
        setPool(!!v.vpc_pool);
      })
      .catch(() => setOvn(false));
  }, []);
  useEffect(() => {
    if (ovn) load();
  }, [ovn, load]);

  // Routers exist only on OVN.
  if (!ovn) return null;
  const attached = (r: Router) => networks.filter((n) => n.router === r.id);
  const active = (r: Router) => overview?.gateways?.find((g) => g.router === `gxr-${r.id}`)?.active;

  return (
    <Card
      title="Routers"
      actions={
        manage && (
          <button className={secondaryButton} onClick={() => setAdding(true)}>
            New router
          </button>
        )
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <p className="text-sm text-gray-600 mb-2">
        A router of the project's own: the NAT networks on it reach each other, and leave through it instead of the cluster's shared edge.
      </p>
      {routers === null ? null : routers.length === 0 ? (
        <p className="text-sm text-gray-500">No routers: this project's NAT networks use the shared edge.</p>
      ) : (
        <table className="w-full text-sm">
          <thead>
            <tr className="text-left text-gray-500 border-b border-gray-100">
              <th className="py-2 font-medium">Router</th>
              <th className="py-2 font-medium">Outside address</th>
              <th className="py-2 font-medium">Gateway</th>
              <th className="py-2 font-medium">Networks</th>
              <th className="py-2" />
            </tr>
          </thead>
          <tbody>
            {routers.map((r) => {
              const nets = attached(r);
              const a = active(r);
              return (
                <tr key={r.id} className="border-b border-gray-50 align-top">
                  <td className="py-2 font-medium">
                    {r.spec.name} <span className="font-mono text-xs text-gray-400">gxr-{r.id}</span>
                  </td>
                  <td className="py-2">{r.spec.external ? <span className="font-mono">{r.status.external_ip ?? "—"}</span> : <Badge kind="muted">internal only</Badge>}</td>
                  <td className="py-2 text-xs">
                    {!r.spec.external ? (
                      "—"
                    ) : (
                      <>
                        {a ? <Badge kind="ok">on {a.node_name ?? a.node ?? a.chassis}</Badge> : overview ? <Badge kind="warn">not scheduled</Badge> : null}
                        <div className="text-gray-500 mt-0.5">{r.spec.gateway_nodes?.length ? r.spec.gateway_nodes.join(", ") : "the shared edge's nodes"}</div>
                      </>
                    )}
                  </td>
                  <td className="py-2">
                    {nets.length ? nets.map((n) => n.name).join(", ") : <span className="text-gray-400">none</span>}
                    {manage && (
                      <div>
                        <Link
                          to={`/networking?new=${encodeURIComponent(project)}&router=${encodeURIComponent(r.spec.name)}`}
                          className="text-xs text-sky-700 hover:underline"
                        >
                          + network on {r.spec.name}
                        </Link>
                      </div>
                    )}
                  </td>
                  <td className="py-2 text-right">
                    {manage && (
                      <button
                        className={dangerLink}
                        disabled={nets.length > 0}
                        title={nets.length ? `Delete its networks first: ${nets.map((n) => n.name).join(", ")}` : ""}
                        onClick={() => setRemoving(r)}
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
      )}
      {adding && <NewRouterDialog project={project} pool={pool} onClose={() => setAdding(false)} onDone={changed} />}
      {removing && (
        <ConfirmDialog
          title={`Delete router ${removing.spec.name}`}
          consequence={removing.status.external_ip ? `Its address ${removing.status.external_ip} goes back to the pool.` : "It has no networks left."}
          actionLabel="Delete"
          onConfirm={async () => {
            await api.deleteRouter(project, removing.spec.name);
            changed();
          }}
          onClose={() => setRemoving(null)}
        />
      )}
    </Card>
  );
}
