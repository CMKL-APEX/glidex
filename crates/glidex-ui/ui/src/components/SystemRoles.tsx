import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import * as api from "../api";
import type { Binding } from "../types";
import { HOST_ROLES, PER_HOST_ROLES, isTombstone } from "../types";
import { Loading } from "./Loading";
import AddBindingForm from "./AddBindingForm";
import { BindingTable, Card, type Directory, ErrorBanner, errorMessage, selectClass } from "./ui";
import { useSession } from "../session";
import { useNodes } from "../nodes";

const EMPTY: Directory = { users: [], teams: [] };

/** The node a binding is on, when it is on one node's host. */
function hostIdOf(b: Binding): string | null {
  const r = b.resource as { type: string; id?: string } | undefined;
  return r?.type === "Host" ? (r.id ?? null) : null;
}

/** System roles (`/system/bindings`): every binding with where it applies,
 * or with `node` those on that node's host only (spec/clustering.md §12.2). */
export default function SystemRoles({ dir, node }: { dir: Directory | null; node?: string }) {
  const { host } = useSession();
  const nodes = useNodes();
  const [bindings, setBindings] = useState<Binding[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Where a new binding goes: "" for the whole cluster, else a node id.
  const [scope, setScope] = useState(node ?? "");

  const refresh = useCallback(() => {
    if (!host.readSystemBindings) return;
    api
      .listSystemBindings(node)
      .then(setBindings)
      .catch((e) => setError(errorMessage(e)));
  }, [host.readSystemBindings, node]);
  useEffect(() => {
    refresh();
  }, [refresh]);

  if (!host.readSystemBindings) return null;
  const live = [...nodes.nodes.values()].filter((n) => !isTombstone(n.status.phase));
  const perHost = nodes.clustered && live.length > 0;
  const where = (b: Binding) => {
    const id = hostIdOf(b);
    if (!id) return "Whole cluster";
    const known = nodes.nodes.has(id);
    return (
      <span>
        Host{" "}
        {known ? (
          <Link to={`/cluster/nodes/${id}`} className="text-sky-700 hover:underline">
            {nodes.name(id)}
          </Link>
        ) : (
          <span className="font-mono text-xs" title="This node has left the cluster">
            {id.slice(0, 8)} (gone)
          </span>
        )}
      </span>
    );
  };

  return (
    <Card title={node ? "Roles on this host" : "System roles"}>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <p className="mb-3 text-sm text-gray-500">
        {node
          ? "Given on this host only: auditor reads it, net-admin manages its networking and devices, host paths lets VMs here use its directories. Roles given on the whole cluster cover it too."
          : perHost
            ? "Auditor (read everything), image-admin, net-admin, system-admin, and the host-paths grant. Auditor, net-admin and host paths can also be given on one host."
            : "Host-wide roles: auditor (read everything), image-admin, net-admin, system-admin, and the host-paths grant."}
      </p>
      {bindings === null ? (
        <Loading />
      ) : bindings.length === 0 && node ? (
        <p className="text-sm text-gray-500">No roles given on this host alone.</p>
      ) : (
        <BindingTable
          bindings={bindings}
          dir={dir ?? EMPTY}
          scope={node ? undefined : perHost || bindings.some((b) => hostIdOf(b) !== null) ? where : undefined}
          onRemove={
            host.manageSystemBindings
              ? (b) =>
                  confirm(`Remove ${b.role} from this principal?`) &&
                  api
                    .removeSystemBinding(b.id)
                    .then(refresh)
                    .catch((e) => setError(errorMessage(e)))
              : undefined
          }
        />
      )}
      {host.manageSystemBindings && (
        <>
          {!node && perHost && (
            <div className="mt-4 max-w-xs">
              <label className="block text-sm font-medium text-gray-700" htmlFor="role-scope">
                Scope
              </label>
              <select id="role-scope" className={selectClass} value={scope} onChange={(e) => setScope(e.target.value)}>
                <option value="">Whole cluster</option>
                {live
                  .sort((a, b) => a.spec.name.localeCompare(b.spec.name))
                  .map((n) => (
                    <option key={n.meta.id} value={n.meta.id}>
                      Host {n.spec.name}
                    </option>
                  ))}
              </select>
            </div>
          )}
          <AddBindingForm
            key={scope ? "host" : "cluster"}
            roles={scope ? PER_HOST_ROLES : HOST_ROLES}
            dir={dir}
            onAdd={async (role, principal) => {
              await api.addSystemBinding(role, principal, scope || undefined);
              refresh();
            }}
          />
        </>
      )}
    </Card>
  );
}
