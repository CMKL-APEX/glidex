import { Navigate, useParams } from "react-router-dom";
import { useNodes } from "../nodes";
import { isTombstone } from "../types";
import { Loading } from "../components/Loading";
import { PageHeader } from "../components/ui";
import { useSession } from "../session";

/** `/hosts` (spec/28-host-page.md W1): a standalone host's one page; in a
 * cluster, the node list on the Cluster page. */
export default function Hosts() {
  const dir = useNodes();
  const { host } = useSession();
  if (!host.listNodes) {
    return (
      <div>
        <PageHeader title="Hosts" />
        <p className="text-gray-500">You can't see this cluster's hosts.</p>
      </div>
    );
  }
  if (dir.clustered) return <Navigate to="/cluster" replace />;
  const only = dir.self ?? [...dir.nodes.values()].find((n) => !isTombstone(n.status.phase))?.meta.id;
  if (!only) return <Loading />;
  return <Navigate to={`/hosts/${encodeURIComponent(only)}`} replace />;
}

/** `/cluster/nodes/:id`, the Node page's old address. */
export function OldNodePage() {
  const { id = "" } = useParams<{ id: string }>();
  return <Navigate to={`/hosts/${encodeURIComponent(id)}`} replace />;
}
