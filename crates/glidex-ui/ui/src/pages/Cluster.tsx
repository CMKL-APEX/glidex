import { useCallback, useEffect, useState } from "react";
import * as api from "../api";
import type { ClusterStatus } from "../api";
import { LoadingCard } from "../components/Loading";
import { Badge, Card, ErrorBanner, PageHeader, errorMessage } from "../components/ui";

const PHASE_KIND: Record<string, string> = { Active: "ok", Draining: "warn", Departing: "warn" };

/** Nodes, the Raft group, the CA and the reachability of the cluster ports
 * (spec/clustering.md §5.11). Removing, forgetting and rejoining stay in
 * `gxctl`: they need a confirmation the CLI asks for. */
export default function Cluster() {
  const [status, setStatus] = useState<ClusterStatus | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      setStatus(await api.clusterStatus());
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);

  useEffect(() => {
    load();
    const id = setInterval(load, 10_000);
    return () => clearInterval(id);
  }, [load]);

  const drain = async (id: string, on: boolean) => {
    try {
      await api.drainNode(id, on);
      await load();
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  if (!status) return error ? <ErrorBanner error={error} /> : <LoadingCard />;
  if (status.clustered === false) {
    return (
      <div>
        <PageHeader title="Cluster" subtitle="This host is standalone." />
        <Card>
          <p className="text-sm text-gray-600">Run <code>gxctl cluster init</code> to turn it into a cluster of one.</p>
        </Card>
      </div>
    );
  }
  const gaps = (status.ports ?? []).filter((p) => !p.reachable);
  const raft = status.raft;
  return (
    <div>
      <PageHeader title="Cluster" subtitle={`${status.cluster_id ?? ""} · feature level ${status.feature_level ?? 1}`} />
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {gaps.length > 0 && (
        <div className="mb-4 p-4 bg-amber-50 border border-amber-200 rounded-lg text-sm text-amber-800">
          {gaps.map((g) => (
            <div key={`${g.node}:${g.port}`}>
              Port {g.port}/{g.proto} on {g.node} does not answer from {status.name}.
            </div>
          ))}
        </div>
      )}
      <Card title="Nodes">
        <table className="w-full text-sm">
          <thead>
            <tr className="text-left text-gray-500">
              <th className="py-1">Name</th>
              <th>Role</th>
              <th>Phase</th>
              <th>Address</th>
              <th>Version</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {status.nodes.map((n) => (
              <tr key={n.id} className="border-t border-gray-100">
                <td className="py-1.5 font-medium">{n.name}</td>
                <td>{n.role}</td>
                <td>
                  <Badge kind={PHASE_KIND[n.phase] ?? "muted"}>{n.phase}</Badge>
                </td>
                <td>{n.advertise ?? "—"}</td>
                <td>{n.version ?? "—"}</td>
                <td className="text-right">
                  {n.phase === "Active" && (
                    <button className="text-sky-700 hover:underline" onClick={() => drain(n.id, true)}>
                      Drain
                    </button>
                  )}
                  {n.phase === "Draining" && (
                    <button className="text-sky-700 hover:underline" onClick={() => drain(n.id, false)}>
                      Undrain
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </Card>
      {raft && (
        <div className="mt-4">
          <Card title="Raft">
            <p className="text-sm text-gray-700">
              {raft.state}, term {raft.term}, leader {raft.leader?.name ?? "none"}, applied {raft.last_applied ?? "—"} of {raft.last_log_index ?? "—"}
            </p>
            <p className="text-sm text-gray-500 mt-1">
              Voters: {raft.voters.map((v) => v.name ?? v.address).join(", ") || "none"}
              {raft.learners.length > 0 && ` · learners: ${raft.learners.map((v) => v.name ?? v.address).join(", ")}`}
            </p>
          </Card>
        </div>
      )}
      <div className="mt-4">
        <Card title="Certificate authority">
          <p className="text-sm text-gray-700">
            Signing CA <code>{(status.ca?.signing ?? status.ca_fingerprint ?? "").slice(0, 16)}</code>
            {status.ca && status.ca.retiring.length > 0 ? ` · rotating, ${status.ca.retiring.length} old CA still trusted` : ""}
          </p>
        </Card>
      </div>
    </div>
  );
}
