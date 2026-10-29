import { useState } from "react";
import * as api from "../api";
import type { ClusterStatus, ClusterNodeRow } from "../api";
import type { NodeRecord } from "../types";
import { formatTimestamp, isTombstone, tsSecs } from "../types";
import { useSession } from "../session";
import { useNodes } from "../nodes";
import ConfirmDialog from "./ConfirmDialog";
import { Card, inputClass, primaryButton, secondaryButton } from "./ui";

const DAY = 86400;

/** "in 3 days" / "2 days ago", for expiry dates (RFC 3339). */
function relative(at: string | undefined): string {
  const d = Math.round((tsSecs(at) - Date.now() / 1000) / DAY);
  if (d === 0) return "today";
  return d > 0 ? `in ${d} day${d === 1 ? "" : "s"}` : `${-d} day${d === -1 ? "" : "s"} ago`;
}

/** A standalone host becomes a cluster of one (spec/16-clustering-ui.md §7.8). */
export function InitCluster({ onDone }: { onDone: () => void }) {
  const { host, refresh } = useSession();
  const [open, setOpen] = useState(false);
  const [advertise, setAdvertise] = useState("");
  const [tunnel, setTunnel] = useState("");
  const name = window.location.hostname;
  return (
    <Card title="Make this host a cluster">
      <p className="text-sm text-gray-600">
        Turns this host into a cluster of one; other hosts can then join it. Every record is re-keyed for clustering. A backup is kept, but
        this can't be undone from here. To run cluster networks on OVN, set <code>{'"cluster": {"ovn": {"enabled": true}}'}</code> in{" "}
        <code>/etc/glidex/control-plane.json</code> and restart the control plane first.
      </p>
      <p className="text-sm text-gray-600 mt-2">
        Joining an existing cluster stays on the command line (<code>gxctl cluster join … --token-file</code>): it needs a join token.
      </p>
      {host.initCluster ? (
        <button className={`${primaryButton} mt-4`} onClick={() => setOpen(true)}>
          Initialize cluster…
        </button>
      ) : (
        <p className="text-sm text-gray-500 mt-4">You may not initialize a cluster on this host.</p>
      )}
      {open && (
        <ConfirmDialog
          title="Initialize a cluster"
          consequence="This host becomes the first server of a new cluster. Its VMs keep running."
          typeToConfirm={name}
          actionLabel="Initialize"
          onConfirm={async () => {
            await api.initCluster({ advertise: advertise.trim() || undefined, tunnel_ip: tunnel.trim() || undefined });
            await refresh();
            onDone();
          }}
          onClose={() => setOpen(false)}
        >
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="block text-sm text-gray-600">Advertise address (ip:port)</label>
              <input className={inputClass} placeholder="auto: this host's address, port 8842" value={advertise} onChange={(e) => setAdvertise(e.target.value)} />
            </div>
            <div>
              <label className="block text-sm text-gray-600">Tunnel IP (optional)</label>
              <input className={inputClass} placeholder="the advertise address" value={tunnel} onChange={(e) => setTunnel(e.target.value)} />
            </div>
          </div>
        </ConfirmDialog>
      )}
    </Card>
  );
}

/** The cluster CA, node certificates, rotation and the snapshot
 * (spec/16-clustering-ui.md §7.6–7.7). */
export function ClusterCa({ status, nodes, onDone }: { status: ClusterStatus; nodes: NodeRecord[]; onDone: () => void }) {
  const { host } = useSession();
  const [rotating, setRotating] = useState(false);
  const [snapshot, setSnapshot] = useState(false);
  const [grace, setGrace] = useState("");
  const ca = status.ca;
  const rows: ClusterNodeRow[] = status.nodes ?? [];
  const live = new Set(nodes.map((n) => n.meta.id));
  const behind = rows.filter((r) => live.has(r.id) && r.cert && !r.cert.current_ca);
  const soon = rows.filter((r) => live.has(r.id) && r.cert?.not_after && tsSecs(r.cert.not_after) - Date.now() / 1000 < 30 * DAY);
  return (
    <Card
      title="Certificate authority"
      actions={
        <>
          {host.snapshotCluster && (
            <button className={secondaryButton} onClick={() => setSnapshot(true)}>
              Download snapshot…
            </button>
          )}
          {host.rotateCa && (
            <button className={secondaryButton} onClick={() => setRotating(true)}>
              Rotate CA…
            </button>
          )}
        </>
      }
    >
      <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-sm">
        <dt className="text-gray-500">Signing CA</dt>
        <dd className="font-mono text-xs break-all">{ca?.signing ?? status.ca_fingerprint ?? "—"}</dd>
        {ca?.not_after && (
          <>
            <dt className="text-gray-500">CA expires</dt>
            <dd>
              {formatTimestamp(ca.not_after)} ({relative(ca.not_after)})
            </dd>
          </>
        )}
        {ca?.retiring && ca.retiring.length > 0 && (
          <>
            <dt className="text-gray-500">Rotation</dt>
            <dd>
              {ca.retiring.length} old CA still trusted{ca.retire_at ? `, retired ${formatTimestamp(ca.retire_at)} at the latest` : ""}
            </dd>
          </>
        )}
      </dl>
      {behind.length > 0 && <p className="mt-3 text-sm text-amber-700">Not yet renewed under the current CA: {behind.map((r) => r.name).join(", ")}.</p>}
      {soon.length > 0 && (
        <p className="mt-1 text-sm text-amber-700">
          Certificates expiring within 30 days: {soon.map((r) => `${r.name} (${relative(r.cert?.not_after)})`).join(", ")}. Nodes renew on their own;
          one that doesn't is probably unreachable.
        </p>
      )}
      {rotating && (
        <ConfirmDialog
          title="Rotate the cluster CA"
          consequence={
            <ol className="list-decimal pl-5 space-y-1">
              <li>A new CA is made and added to every node's trust next to the old one.</li>
              <li>The new CA signs from now on; every node renews its certificate under it.</li>
              <li>Once every node has renewed (or the grace period ends), the old CA is retired and certificates it signed stop working.</li>
            </ol>
          }
          typeToConfirm={status.cluster_id}
          actionLabel="Rotate"
          onConfirm={async () => {
            await api.rotateCa(grace ? Number(grace) * DAY : undefined);
            onDone();
          }}
          onClose={() => setRotating(false)}
        >
          <div>
            <label className="block text-sm text-gray-600">Grace period in days (optional)</label>
            <input className={inputClass} type="number" min={1} placeholder="the cluster's default (7)" value={grace} onChange={(e) => setGrace(e.target.value)} />
          </div>
        </ConfirmDialog>
      )}
      {snapshot && (
        <ConfirmDialog
          title="Download a cluster snapshot"
          consequence={
            <>
              The snapshot holds the whole replicated database, <strong>including credential and token hashes</strong>. Store it like the
              database itself: encrypted, readable only by administrators, and deleted when no longer needed. The download is recorded in the
              audit log.
            </>
          }
          actionLabel="Download"
          danger={false}
          onConfirm={async () => {
            // A browser download with the session cookie; nothing is kept in the page.
            const a = document.createElement("a");
            a.href = api.clusterSnapshotUrl();
            a.download = "glidex-cluster.gxsnap";
            a.click();
          }}
          onClose={() => setSnapshot(false)}
        />
      )}
    </Card>
  );
}

/** Ending this node's or the whole cluster's membership (spec/16-clustering-ui.md §7.5). */
export function DangerZone({ status, onDone }: { status: ClusterStatus; onDone: () => void }) {
  const { host, refresh } = useSession();
  const dir = useNodes();
  const [op, setOp] = useState<"leave" | "dissolve" | null>(null);
  const members = [...dir.nodes.values()].filter((n) => !isTombstone(n.status.phase));
  const self = dir.self ? dir.nodes.get(dir.self) : undefined;
  const canDissolve = host.dissolveCluster && members.length === 1;
  const canLeave = host.leaveCluster && self?.status.phase === "NODE_PHASE_REMOVED";
  if (!canDissolve && !canLeave) return null;
  return (
    <details className="mb-6 bg-white rounded-xl shadow-sm border border-red-200 p-5">
      <summary className="cursor-pointer text-lg font-semibold text-red-700">Danger zone</summary>
      <div className="mt-4 space-y-3 text-sm">
        {canLeave && (
          <div className="flex items-center justify-between gap-4">
            <span>This node was removed from the cluster: make it a standalone host again.</span>
            <button className="px-3 py-1.5 text-sm text-white bg-red-600 hover:bg-red-700 rounded-lg" onClick={() => setOp("leave")}>
              Leave cluster…
            </button>
          </div>
        )}
        {canDissolve && (
          <div className="flex items-center justify-between gap-4">
            <span>This is the last node: end the cluster and keep everything here as a standalone host.</span>
            <button className="px-3 py-1.5 text-sm text-white bg-red-600 hover:bg-red-700 rounded-lg" onClick={() => setOp("dissolve")}>
              Dissolve cluster…
            </button>
          </div>
        )}
      </div>
      {op && (
        <ConfirmDialog
          title={op === "leave" ? "Leave the cluster" : "Dissolve the cluster"}
          consequence={
            op === "leave"
              ? "This host stops being part of the cluster and becomes standalone with what it still holds."
              : "The cluster ends. This host becomes standalone with every VM, disk, network and user it held. A final snapshot is kept."
          }
          typeToConfirm={status.cluster_id}
          actionLabel={op === "leave" ? "Leave" : "Dissolve"}
          onConfirm={async () => {
            if (op === "leave") await api.leaveCluster();
            else await api.dissolveCluster();
            await refresh();
            onDone();
          }}
          onClose={() => setOp(null)}
        />
      )}
    </details>
  );
}
