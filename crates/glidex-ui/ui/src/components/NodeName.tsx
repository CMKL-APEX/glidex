import { Link } from "react-router-dom";
import { useNodes } from "../nodes";
import { useSession } from "../session";

/** A node by name (spec/clustering-ui.md §3.2): a link to its page for
 * those who may list nodes, "(this node)" for the one serving the UI. */
export default function NodeName({ id, name, className = "" }: { id: string | null | undefined; name?: string | null; className?: string }) {
  const { host } = useSession();
  const dir = useNodes();
  if (!id) return <span className={className}>—</span>;
  const label = name || dir.name(id);
  const self = dir.self === id ? <span className="text-gray-400 font-normal"> (this node)</span> : null;
  if (!host.listNodes) {
    return (
      <span className={className} title={id}>
        {label}
        {self}
      </span>
    );
  }
  return (
    <span className={className}>
      <Link to={`/cluster/nodes/${encodeURIComponent(id)}`} className="text-sky-700 hover:underline" title={id}>
        {label}
      </Link>
      {self}
    </span>
  );
}
