import { useCallback, useEffect, useState } from "react";
import { Link, useParams } from "react-router-dom";
import * as api from "../api";
import type { DiskInfo, NodeRecord, VmResponse } from "../types";
import { brIntLabel, formatBytes, formatTimestamp, nodeLabel, nodePhaseLabel, nodeReady, nodeRoleLabel, num, stateLabel, vmState, wantedBrInt } from "../types";
import BrIntDialog from "../components/BrIntDialog";
import MappingsDialog from "../components/MappingsDialog";
import { hostOf, useAllowed } from "../nodes";
import { useLive, useLiveRefresh } from "../live";
import { useNodes } from "../nodes";
import NodeActions from "../components/NodeActions";
import { NodeOvnCard } from "../components/OvnViews";
import { useCan, useSession } from "../session";
import { LoadingCard } from "../components/Loading";
import { Badge, Card, ErrorBanner, PageHeader, errorMessage } from "../components/ui";

/** Badge kinds by phase label (`nodePhaseLabel`). */
export const PHASE_KIND: Record<string, string> = { Active: "ok", Draining: "warn", Departing: "warn", Departed: "muted", Removed: "muted", Forgotten: "muted" };

function ReadyBadge({ node }: { node: NodeRecord }) {
  const r = nodeReady(node);
  const kind = r === "True" ? "ok" : r === "Unknown" ? "warn" : "denied";
  const label = r === "True" ? "ready" : r === "Unknown" ? "unreachable" : "not ready";
  return (
    <span title={node.status.ready_reason ?? ""}>
      <Badge kind={kind}>{label}</Badge>
    </span>
  );
}

/** One row of the capacity table: what VMs ask for against what may be given. */
function Use({ label, taken, total, unit }: { label: string; taken: number; total: number; unit: string }) {
  const pct = total > 0 ? Math.min(100, Math.round((taken / total) * 100)) : 0;
  return (
    <div>
      <div className="flex justify-between text-sm">
        <span className="text-gray-600">{label}</span>
        <span className="text-gray-900">
          {taken} / {total} {unit}
        </span>
      </div>
      <div className="h-2 bg-gray-100 rounded-full mt-1">
        <div className={`h-2 rounded-full ${pct > 90 ? "bg-red-500" : pct > 70 ? "bg-amber-500" : "bg-sky-500"}`} style={{ width: `${pct}%` }} />
      </div>
    </div>
  );
}

/** A node (spec/clustering-ui.md §5.2): its state, capacity, what runs on
 * it, what it can do, and the operations on it. */
export default function NodeDetail() {
  const { id = "" } = useParams<{ id: string }>();
  const live = useLive();
  const dir = useNodes();
  const { host } = useSession();
  const [polled, setPolled] = useState<NodeRecord | null>(null);
  const [vms, setVms] = useState<VmResponse[] | null>(null);
  const [disks, setDisks] = useState<DiskInfo[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [brIntOpen, setBrIntOpen] = useState(false);
  const [mapsOpen, setMapsOpen] = useState(false);
  const nodeHost = hostOf(id, dir.self);
  const canConf = useAllowed(["configureNode"], [nodeHost]);
  const canConfigure = !!canConf?.(nodeHost, "configureNode");

  const load = useCallback(async () => {
    try {
      const [n, v, d] = await Promise.all([live.live ? Promise.resolve(null) : api.getNode(id), api.listVms(null), api.listDisks(null).catch(() => [] as DiskInfo[])]);
      if (n) setPolled(n);
      setVms(v.filter((x) => x.node === id));
      setDisks(d.filter((x) => x.node === id));
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, [id, live.live]);
  useEffect(() => {
    load();
  }, [load]);
  useLiveRefresh(["vm", "disk"], load);

  const node = (live.live ? live.nodes.get(id) : null) ?? polled;
  if (!node) return error ? <ErrorBanner error={error} /> : <LoadingCard />;
  const s = node.status;
  const ready = nodeReady(node);
  const phase = nodePhaseLabel(s.phase);
  const cap = { cpus: s.capacity?.cpus ?? 0, mem: num(s.capacity?.memory_mib) };
  const alloc = { cpus: s.allocatable?.cpus ?? 0, mem: num(s.allocatable?.memory_mib) };
  const labels = node.spec.labels ?? {};
  const requested = (vms ?? []).reduce((a, v) => ({ cpu: a.cpu + v.vcpu_count, mem: a.mem + v.mem_size_mib }), { cpu: 0, mem: 0 });
  const running = (vms ?? []).filter((v) => vmState(v) === "running").length;

  return (
    <div>
      <p className="mb-2 text-sm">
        <Link to="/cluster" className="text-sky-700 hover:underline">
          ← Cluster
        </Link>
      </p>
      <PageHeader
        title={nodeLabel(node)}
        subtitle={
          <span className="inline-flex flex-wrap items-center gap-2">
            <span>{nodeRoleLabel(node.spec.role)}</span>
            <Badge kind={PHASE_KIND[phase] ?? "muted"}>{phase}</Badge>
            <ReadyBadge node={node} />
            {dir.self === id && <span className="text-gray-400">(this node)</span>}
            <span className="font-mono text-xs text-gray-400">{id}</span>
          </span>
        }
      >
        <NodeActions node={node} onDone={load} />
      </PageHeader>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {ready !== "True" && s.ready_reason && (
        <div className="mb-4 p-3 bg-amber-50 border border-amber-200 rounded-lg text-sm text-amber-800">
          {ready === "Unknown"
            ? "This node stopped reporting. Its VMs may still be running; nothing is restarted elsewhere."
            : `Not ready: ${s.ready_reason}`}
        </div>
      )}

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        <Card title="Capacity">
          <div className="space-y-3">
            <Use label="vCPUs requested by its VMs" taken={requested.cpu} total={alloc.cpus} unit="allocatable" />
            <Use label="Memory requested by its VMs" taken={requested.mem} total={alloc.mem} unit="MiB allocatable" />
          </div>
          <dl className="mt-4 grid grid-cols-2 gap-x-4 gap-y-1 text-sm">
            <dt className="text-gray-500">CPUs</dt>
            <dd>
              {cap.cpus} ({alloc.cpus} allocatable)
            </dd>
            <dt className="text-gray-500">Memory</dt>
            <dd>
              {formatBytes(cap.mem * 1048576)} ({formatBytes(alloc.mem * 1048576)} allocatable)
            </dd>
            {Object.entries(s.allocatable?.hugepages ?? {}).map(([size, n]) => (
              <span key={size} className="contents">
                <dt className="text-gray-500">Hugepages {size} KiB</dt>
                <dd>{num(n)}</dd>
              </span>
            ))}
            <dt className="text-gray-500">VMs</dt>
            <dd>
              {(vms ?? []).length} placed, {running} running
            </dd>
          </dl>
          <p className="mt-3 text-xs text-gray-500">The scheduler may overcommit these by the cluster's ratios.</p>
        </Card>

        <Card title="Node">
          <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-sm">
            <dt className="text-gray-500">Address</dt>
            <dd className="font-mono">{s.advertise ?? "—"}</dd>
            <dt className="text-gray-500">Tunnel IP</dt>
            <dd className="font-mono">{s.tunnel_ip ?? "—"}</dd>
            <dt className="text-gray-500">glidex</dt>
            <dd>{s.versions?.glidex ?? "—"}</dd>
            {s.versions?.ovs && (
              <>
                <dt className="text-gray-500">Open vSwitch</dt>
                <dd>{s.versions.ovs}</dd>
              </>
            )}
            {s.versions?.ovn && (
              <>
                <dt className="text-gray-500">OVN</dt>
                <dd>{s.versions.ovn}</dd>
              </>
            )}
            <dt className="text-gray-500">Architecture</dt>
            <dd>{s.features?.arch || "—"}</dd>
            <dt className="text-gray-500">Hypervisors</dt>
            <dd>{(s.features?.hypervisors ?? []).join(", ") || "—"}</dd>
            <dt className="text-gray-500">KVM / IOMMU / DPDK</dt>
            <dd>
              {s.features?.kvm ? "yes" : "no"} / {s.features?.iommu ? "yes" : "no"} / {s.features?.dpdk ? "yes" : "no"}
            </dd>
            <dt className="text-gray-500">br-int datapath</dt>
            <dd>
              {s.features?.br_int_datapath ?? <span className="text-gray-400">not a chassis</span>}
              <span className="text-gray-500">
                {" "}
                (setting: {brIntLabel(node.spec.br_int_datapath)}
                {s.features?.br_int_datapath && s.features.br_int_datapath !== wantedBrInt(node) ? `; moving to ${wantedBrInt(node)}` : ""})
              </span>
              {canConfigure && (
                <button className="ml-2 text-sky-700 hover:underline" onClick={() => setBrIntOpen(true)}>
                  Change…
                </button>
              )}
            </dd>
            <dt className="text-gray-500">Physical networks</dt>
            <dd>
              {Object.entries(s.features?.physnets ?? {})
                .map(([p, b]) => `${p} (${b})`)
                .join(", ") || <span className="text-gray-400">none</span>}
              {canConfigure && (
                <button className="ml-2 text-sky-700 hover:underline" onClick={() => setMapsOpen(true)}>
                  Change…
                </button>
              )}
            </dd>
            <dt className="text-gray-500">PCI devices</dt>
            <dd>{s.pci_devices?.length ?? 0}</dd>
            <dt className="text-gray-500">Joined</dt>
            <dd>{formatTimestamp(node.meta.created_at)}</dd>
            {s.heartbeat && (
              <>
                <dt className="text-gray-500">Last readiness change</dt>
                <dd>{formatTimestamp(s.heartbeat)}</dd>
              </>
            )}
            {Object.keys(labels).length > 0 && (
              <>
                <dt className="text-gray-500">Labels</dt>
                <dd>
                  {Object.entries(labels)
                    .map(([k, v]) => `${k}=${v}`)
                    .join(", ")}
                </dd>
              </>
            )}
          </dl>
        </Card>
      </div>

      {host.readCluster && <NodeOvnCard node={id} />}

      <SpareFilesCard node={node} />

      {mapsOpen && (
        <MappingsDialog
          node={node}
          onClose={() => setMapsOpen(false)}
          onDone={() => {
            setMapsOpen(false);
            load();
          }}
        />
      )}
      {brIntOpen && (
        <BrIntDialog
          node={node}
          onClose={() => setBrIntOpen(false)}
          onDone={() => {
            setBrIntOpen(false);
            load();
          }}
        />
      )}

      <Card title={`VMs (${(vms ?? []).length})`}>
        {vms === null ? (
          <LoadingCard />
        ) : vms.length === 0 ? (
          <p className="text-sm text-gray-500">No VMs you can see are placed here.</p>
        ) : (
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500">
                <th className="py-1">Name</th>
                <th>State</th>
                <th>vCPUs</th>
                <th>Memory</th>
              </tr>
            </thead>
            <tbody>
              {vms
                .slice()
                .sort((a, b) => a.name.localeCompare(b.name))
                .map((v) => (
                  <tr key={v.id} className="border-t border-gray-100">
                    <td className="py-1.5">
                      <Link to={`/vms/${v.id}`} className="text-sky-700 hover:underline">
                        {v.name}
                      </Link>
                    </td>
                    <td>{stateLabel(vmState(v))}</td>
                    <td>{v.vcpu_count}</td>
                    <td>{v.mem_size_mib} MiB</td>
                  </tr>
                ))}
            </tbody>
          </table>
        )}
      </Card>

      {disks.length > 0 && (
        <Card title={`Disks (${disks.length})`}>
          <ul className="text-sm space-y-1">
            {disks
              .slice()
              .sort((a, b) => a.name.localeCompare(b.name))
              .map((d) => (
                <li key={d.id}>
                  <span className="font-mono">{d.name}</span> <span className="text-gray-500">{formatBytes(d.size_bytes)}</span>
                </li>
              ))}
          </ul>
        </Card>
      )}
    </div>
  );
}

/** Files in the node's image directory that no image record names
 * (spec/clustering.md §9.2.3): from before it joined, mostly. Files
 * matching a ready image are adopted as its copies; these are the rest. */
function SpareFilesCard({ node }: { node: NodeRecord }) {
  const files = node.status.features?.spare_image_files ?? [];
  const caps = useCan([{ action: "pullImage", resource: { type: "Cluster" } }]);
  const [error, setError] = useState<string | null>(null);
  const [asked, setAsked] = useState(false);
  if (files.length === 0) return null;
  const bytes = files.reduce((n, f) => n + num(f.bytes), 0);
  const drop = async () => {
    if (!confirm(`Delete ${files.length} unrecorded file(s) (${formatBytes(bytes)}) on ${node.spec.name}?`)) return;
    setError(null);
    try {
      await api.deleteSpareImageFiles(node.meta.id);
      setAsked(true);
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  return (
    <Card
      title={`Unrecorded image files (${files.length}, ${formatBytes(bytes)})`}
      actions={
        caps?.[0] && (
          <button className="px-3 py-1.5 text-sm text-red-700 bg-red-50 hover:bg-red-100 rounded-lg disabled:opacity-50" disabled={asked} onClick={drop}>
            {asked ? "Deleting…" : "Delete unrecorded files"}
          </button>
        )
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <p className="text-sm text-gray-600 mb-2">
        In this node's image directory, but no image refers to them (from before it joined, say). A file matching an image is used as
        its copy here; these match none.
      </p>
      <ul className="text-sm font-mono space-y-0.5">
        {files.map((f) => (
          <li key={f.name}>
            {f.name} <span className="text-gray-500 font-sans">{formatBytes(f.bytes)}</span>
          </li>
        ))}
      </ul>
    </Card>
  );
}
