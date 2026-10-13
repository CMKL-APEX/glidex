import { useCallback, useEffect, useState } from "react";
import { Link } from "react-router-dom";
import * as api from "../api";
import type { ClusterStatus } from "../api";
import type { Departure, NodeRecord, VmResponse } from "../types";
import { formatTime, isTombstone } from "../types";
import { useLive, useLiveCluster, useLiveRefresh } from "../live";
import { useNodes } from "../nodes";
import { useSession } from "../session";
import { LoadingCard } from "../components/Loading";
import NodeActions from "../components/NodeActions";
import AddNodeDialog from "../components/AddNodeDialog";
import { Badge, Card, ErrorBanner, PageHeader, errorMessage, primaryButton } from "../components/ui";
import { PHASE_KIND } from "./NodeDetail";
import { ClusterCa, DangerZone, InitCluster } from "../components/ClusterLifecycle";

/** Port checks open connections to every node: not on every change. */
const PORTS_EVERY_MS = 60_000;

function ReadyCell({ n }: { n: NodeRecord }) {
  const r = n.status.ready;
  return (
    <span title={n.status.ready_reason ?? ""}>
      <Badge kind={r === "True" ? "ok" : r === "Unknown" ? "warn" : "denied"}>{r === "True" ? "ready" : r === "Unknown" ? "unreachable" : "not ready"}</Badge>
    </span>
  );
}

/** The cluster (spec/clustering-ui.md §6): its nodes and what can be done to
 * them, pending imports and detaches, Raft, the CA, and the cluster's own
 * lifecycle. */
export default function Cluster() {
  const { host } = useSession();
  const live = useLive();
  const liveCluster = useLiveCluster();
  const dir = useNodes();
  const [status, setStatus] = useState<ClusterStatus | null>(null);
  const [vms, setVms] = useState<VmResponse[]>([]);
  const [departures, setDepartures] = useState<Departure[]>([]);
  const [imports, setImports] = useState(live.imports);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  const loadStatus = useCallback(async () => {
    try {
      setStatus(await api.clusterStatus());
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  const loadRest = useCallback(async () => {
    const [v, d, i] = await Promise.all([
      api.listVms(null).catch(() => [] as VmResponse[]),
      api.listDepartures().catch(() => [] as Departure[]),
      host.listImports && !live.live ? api.listImports().catch(() => null) : Promise.resolve(null),
    ]);
    setVms(v);
    setDepartures(d);
    if (i) setImports(new Map(i.map((p) => [p.plan, p])));
  }, [host.listImports, live.live]);

  useEffect(() => {
    loadStatus();
    const t = window.setInterval(loadStatus, PORTS_EVERY_MS);
    return () => window.clearInterval(t);
  }, [loadStatus]);
  useEffect(() => {
    loadRest();
  }, [loadRest]);
  useEffect(() => {
    if (live.live) setImports(live.imports);
  }, [live.live, live.imports]);
  useLiveRefresh(["vm", "node"], loadRest);
  // Without the stream, poll the node list and the detaches.
  useEffect(() => {
    if (live.live) return;
    const t = window.setInterval(loadRest, 10_000);
    return () => window.clearInterval(t);
  }, [live.live, loadRest]);

  const refreshAll = () => {
    loadStatus();
    loadRest();
  };

  if (!status) return error ? <ErrorBanner error={error} /> : <LoadingCard />;
  if (status.clustered === false) {
    return (
      <div>
        <PageHeader title="Cluster" subtitle="This host is standalone." />
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        <InitCluster onDone={refreshAll} />
      </div>
    );
  }

  // The live summary is fresher; the polled status has the port checks.
  const summary = liveCluster ?? status;
  const nodes: NodeRecord[] = [...(dir.nodes.size ? dir.nodes.values() : [])].sort((a, b) => a.spec.name.localeCompare(b.spec.name));
  const current = nodes.filter((n) => !isTombstone(n.status.phase));
  const gone = nodes.filter((n) => isTombstone(n.status.phase));
  const vmCount = (id: string) => vms.filter((v) => v.node === id).length;
  const gaps = (status.ports ?? []).filter((p) => !p.reachable);
  const pendingImports = [...imports.values()].filter((p) => p.state === "pending");
  const activeDetaches = departures.filter((d) => d.state === "frozen");
  const rotating = !!summary.ca && summary.ca.retiring.length > 0;
  const raft = summary.raft;

  const nodeRow = (n: NodeRecord) => (
    <tr key={n.meta.id} className="border-t border-gray-100 align-top">
      <td className="py-2">
        <Link to={`/cluster/nodes/${encodeURIComponent(n.meta.id)}`} className="font-medium text-sky-700 hover:underline">
          {n.spec.name}
        </Link>
        {dir.self === n.meta.id && <span className="text-gray-400"> (this node)</span>}
      </td>
      <td>{n.spec.role}</td>
      <td>
        <Badge kind={PHASE_KIND[n.status.phase] ?? "muted"}>{n.status.phase}</Badge>
      </td>
      <td>{isTombstone(n.status.phase) ? "—" : <ReadyCell n={n} />}</td>
      <td className="font-mono text-xs">{n.status.advertise ?? "—"}</td>
      <td>{n.status.versions.glidex}</td>
      <td>{vmCount(n.meta.id) || "—"}</td>
      <td className="text-right">
        <NodeActions node={n} onDone={refreshAll} />
      </td>
    </tr>
  );

  return (
    <div>
      <PageHeader title="Cluster" subtitle={`${summary.cluster_id ?? ""} · feature level ${summary.feature_level ?? 1}`}>
        {host.createJoinToken && (
          <button className={primaryButton} onClick={() => setAdding(true)}>
            + Add node
          </button>
        )}
      </PageHeader>
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
      {rotating && (
        <div className="mb-4 p-4 bg-sky-50 border border-sky-200 rounded-lg text-sm text-sky-800">
          A CA rotation is under way: {summary.ca!.retiring.length} old CA still trusted
          {summary.ca!.retire_at ? ` until ${formatTime(summary.ca!.retire_at)}` : ""}.
        </div>
      )}
      {activeDetaches.length > 0 && (
        <div className="mb-4 p-4 bg-amber-50 border border-amber-200 rounded-lg text-sm text-amber-800">
          {activeDetaches.map((d) => (
            <div key={d.plan}>
              {d.node_name ?? d.node} is detaching: frozen since {formatTime(d.started_at)}
              {d.acknowledged ? ", acknowledged by the node" : ", waiting for the node"}; gives up {formatTime(d.deadline)}.
            </div>
          ))}
        </div>
      )}

      <Card title="Nodes">
        {!host.listNodes ? (
          <p className="text-sm text-gray-500">You may not list the nodes.</p>
        ) : (
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500">
                <th className="py-1">Name</th>
                <th>Role</th>
                <th>Phase</th>
                <th>Ready</th>
                <th>Address</th>
                <th>Version</th>
                <th>VMs</th>
                <th />
              </tr>
            </thead>
            <tbody>{current.map(nodeRow)}</tbody>
          </table>
        )}
        {gone.length > 0 && (
          <details className="mt-3 text-sm">
            <summary className="cursor-pointer text-gray-600">{gone.length} departed, removed or forgotten</summary>
            <table className="w-full mt-2">
              <tbody>{gone.map(nodeRow)}</tbody>
            </table>
          </details>
        )}
      </Card>

      {host.listImports && (
        <Card title={`Pending imports (${pendingImports.length})`}>
          {pendingImports.length === 0 ? (
            <p className="text-sm text-gray-500">No host is waiting to join with its VMs.</p>
          ) : (
            <table className="w-full text-sm">
              <thead>
                <tr className="text-left text-gray-500">
                  <th className="py-1">Host</th>
                  <th>Brings</th>
                  <th>Problems</th>
                  <th>Expires</th>
                </tr>
              </thead>
              <tbody>
                {pendingImports.map((p) => (
                  <tr key={p.plan} className="border-t border-gray-100">
                    <td className="py-1.5">
                      <Link to={`/cluster/imports/${encodeURIComponent(p.plan)}`} className="text-sky-700 hover:underline">
                        {p.node}
                      </Link>{" "}
                      <span className="font-mono text-xs text-gray-400">{p.advertise}</span>
                    </td>
                    <td>
                      {p.summary.vms} VMs, {p.summary.disks} disks, {p.summary.networks} networks, {p.summary.projects} projects
                    </td>
                    <td>{p.problems.length ? <Badge kind="warn">{p.problems.length}</Badge> : <Badge kind="ok">none</Badge>}</td>
                    <td>{formatTime(p.expires_at)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </Card>
      )}

      {departures.length > 0 && (
        <Card title="Detaches">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500">
                <th className="py-1">Node</th>
                <th>State</th>
                <th>Started</th>
                <th>Networks mapped</th>
              </tr>
            </thead>
            <tbody>
              {departures
                .slice()
                .sort((a, b) => b.started_at - a.started_at)
                .map((d) => (
                  <tr key={d.plan} className="border-t border-gray-100">
                    <td className="py-1.5">{d.node_name ?? d.node}</td>
                    <td>
                      <Badge kind={d.state === "frozen" ? "warn" : d.state === "committed" ? "ok" : "muted"}>{d.state}</Badge>
                    </td>
                    <td>{formatTime(d.started_at)}</td>
                    <td className="text-xs">
                      {Object.entries(d.map_networks)
                        .map(([a, b]) => `${a} → ${b}`)
                        .join(", ") || "—"}
                    </td>
                  </tr>
                ))}
            </tbody>
          </table>
        </Card>
      )}

      {raft && (
        <Card title="Raft">
          <p className="text-sm text-gray-700">
            {raft.state}, term {raft.term}, leader {raft.leader?.name ?? "none"}
          </p>
          <p className="text-sm text-gray-500 mt-1">
            Voters: {raft.voters.map((v) => v.name ?? v.address).join(", ") || "none"}
            {raft.learners.length > 0 && ` · learners: ${raft.learners.map((v) => v.name ?? v.address).join(", ")}`}
          </p>
        </Card>
      )}

      <ClusterCa status={summary} nodes={current} onDone={refreshAll} />
      <DangerZone status={summary} onDone={refreshAll} />

      {adding && <AddNodeDialog onClose={() => setAdding(false)} />}
    </div>
  );
}
