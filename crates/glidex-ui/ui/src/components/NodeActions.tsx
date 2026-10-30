import { useState } from "react";
import * as api from "../api";
import type { IssuedToken, NodeRecord } from "../types";
import { isClusterNetwork, isTombstone, nodeLabel, nodePhaseLabel, nodeReady } from "../types";
import { hostOf, useAllowed, useNodes } from "../nodes";
import { useSession } from "../session";
import { useLive } from "../live";
import ConfirmDialog from "./ConfirmDialog";
import Modal from "./Modal";
import TokenReveal from "./TokenReveal";
import { ErrorBanner, errorMessage, inputClass, primaryButton, secondaryButton, selectClass } from "./ui";

type Op = "drain" | "remove" | "forget" | "purge" | "rejoin" | "promote" | "detach" | "abort";

/** The refusals `gxctl` lets through with `--force`. */
const asksForForce = (m: string) => m.includes("--force");

/** Operations on one node, each shown only when allowed and when its phase
 * permits (spec/16-clustering-ui.md §7.2–7.3). */
export default function NodeActions({ node, onDone }: { node: NodeRecord; onDone: () => void }) {
  const dir = useNodes();
  const { host: caps } = useSession();
  const id = node.meta.id;
  const name = node.spec.name;
  // A node that left keeps its name: its short id tells the records apart.
  const label = nodeLabel(node);
  const host = hostOf(id, dir.self);
  const can = useAllowed(["drainNode", "undrainNode"], [host]);
  const [op, setOp] = useState<Op | null>(null);
  const [error, setError] = useState<string | null>(null);
  const phase = nodePhaseLabel(node.status.phase);
  const ready = nodeReady(node);
  const tombstone = isTombstone(node.status.phase);
  const done = () => {
    setOp(null);
    onDone();
  };

  const items: { op: Op; label: string; show: boolean }[] = [
    { op: "drain", label: "Drain", show: phase === "Active" && !!can?.(host, "drainNode") },
    { op: "promote", label: "Promote to server", show: phase === "Active" && node.spec.role === "NODE_ROLE_AGENT" && caps.promoteNode },
    { op: "detach", label: "Detach with its VMs…", show: (phase === "Active" || phase === "Draining") && dir.self !== id && caps.detachNode },
    { op: "abort", label: "Abort the detach", show: phase === "Departing" && caps.detachNode },
    { op: "remove", label: "Remove…", show: (phase === "Active" || phase === "Draining") && dir.self !== id && caps.removeNode },
    { op: "forget", label: "Forget (it is gone)…", show: !tombstone && ready === "Unknown" && caps.forgetNode },
    { op: "rejoin", label: "Rejoin token…", show: (phase === "Forgotten" || phase === "Removed") && caps.createRejoinToken },
    { op: "purge", label: "Purge its records…", show: phase === "Forgotten" && caps.purgeNode },
  ];
  const shown = items.filter((i) => i.show);

  return (
    <div className="flex flex-wrap items-center gap-2">
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {phase === "Draining" && can?.(host, "undrainNode") && (
        <button
          className={secondaryButton}
          onClick={() =>
            api
              .drainNode(id, false)
              .then(onDone)
              .catch((e) => setError(errorMessage(e)))
          }
        >
          Undrain
        </button>
      )}
      {shown.length > 0 && (
        <select
          aria-label={`Operations on ${label}`}
          className={`${selectClass} w-auto py-1 text-sm`}
          value=""
          onChange={(e) => setOp(e.target.value as Op)}
        >
          <option value="">Operations…</option>
          {shown.map((i) => (
            <option key={i.op} value={i.op}>
              {i.label}
            </option>
          ))}
        </select>
      )}

      {op === "drain" && (
        <ConfirmDialog
          title={`Drain ${label}`}
          consequence="No new VMs are placed on this node. VMs already on it keep running; move or stop them before removing the node."
          actionLabel="Drain"
          danger={false}
          onConfirm={async () => {
            await api.drainNode(id, true);
          }}
          onClose={done}
        />
      )}
      {op === "promote" && (
        <ConfirmDialog
          title={`Promote ${label}`}
          consequence="It becomes a Raft voter and a member of the OVN databases. Keep the number of servers odd (1, 3 or 5)."
          actionLabel="Promote"
          danger={false}
          forceWhen={asksForForce}
          onConfirm={async (force) => {
            await api.promoteNodes([id], force);
          }}
          onClose={done}
        />
      )}
      {op === "remove" && (
        <ConfirmDialog
          title={`Remove ${label}`}
          consequence="The node leaves the cluster: Raft and the OVN databases forget it and its certificate is refused from now on. It must have no VMs, disks or node networks left."
          typeToConfirm={name}
          actionLabel="Remove"
          forceWhen={asksForForce}
          onConfirm={async (force) => {
            await api.removeNode(id, force);
          }}
          onClose={done}
        />
      )}
      {op === "forget" && <ForgetDialog node={node} onClose={done} />}
      {op === "purge" && (
        <ConfirmDialog
          title={`Purge ${label}`}
          consequence="Deletes the records of the VMs, disks and networks that were on this forgotten node. Their files, if the host ever comes back, are not touched. This cannot be undone."
          typeToConfirm={name}
          actionLabel="Purge"
          onConfirm={async () => {
            await api.purgeNode(id);
          }}
          onClose={done}
        />
      )}
      {op === "rejoin" && <RejoinDialog node={node} onClose={done} />}
      {op === "detach" && <DetachDialog node={node} onClose={done} />}
      {op === "abort" && (
        <ConfirmDialog
          title={`Abort the detach of ${label}`}
          consequence="The node goes back to Active and keeps its VMs in the cluster. A detach the node already acknowledged can't be aborted."
          actionLabel="Abort the detach"
          danger={false}
          onConfirm={async () => {
            await api.abortDetach(id);
          }}
          onClose={done}
        />
      )}
    </div>
  );
}

function ForgetDialog({ node, onClose }: { node: NodeRecord; onClose: () => void }) {
  const [fenced, setFenced] = useState(false);
  return (
    <ConfirmDialog
      title={`Forget ${nodeLabel(node)}`}
      consequence="For a node that is gone for good. Its VMs stay recorded on it, marked lost, until it rejoins with its disks or you purge it. Its certificate is refused from now on."
      typeToConfirm={fenced ? node.spec.name : undefined}
      actionLabel="Forget"
      forceWhen={asksForForce}
      onConfirm={async (force) => {
        if (!fenced) throw new Error("Confirm that the node is powered off or fenced first.");
        await api.forgetNode(node.meta.id, true, force);
      }}
      onClose={onClose}
    >
      <label className="flex items-start gap-2">
        <input type="checkbox" className="mt-1" checked={fenced} onChange={(e) => setFenced(e.target.checked)} />
        <span>The node is powered off or fenced, and will not come back by itself.</span>
      </label>
    </ConfirmDialog>
  );
}

function RejoinDialog({ node, onClose }: { node: NodeRecord; onClose: () => void }) {
  const [raftIntact, setRaftIntact] = useState(true);
  const [issued, setIssued] = useState<IssuedToken | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const live = useLive();
  const server = [...live.nodes.values()].find((n) => n.spec.role === "NODE_ROLE_SERVER" && n.status.phase === "NODE_PHASE_ACTIVE")?.status.advertise ?? "<server>:8842";
  const close = () => {
    setIssued(null);
    onClose();
  };
  return (
    <Modal title={`Rejoin token for ${nodeLabel(node)}`} onClose={close}>
      {issued ? (
        <>
          <TokenReveal
            issued={issued}
            commands={[
              `sudo glidex-install --rejoin ${server} --node-id ${node.meta.id} --token-file ./rejoin-token`,
              "# or, where glidex still runs:",
              `gxctl cluster rejoin --server ${server} --node-id ${node.meta.id} --token-file ./rejoin-token`,
            ]}
          />
          <div className="mt-4 flex justify-end">
            <button className={primaryButton} onClick={close}>
              Done
            </button>
          </div>
        </>
      ) : (
        <div className="space-y-4 text-sm">
          <p>The node comes back as itself, with its disks; its VMs are adopted where they are.</p>
          <ErrorBanner error={error} onDismiss={() => setError(null)} />
          {node.spec.role === "NODE_ROLE_SERVER" && (
            <label className="flex items-start gap-2">
              <input type="checkbox" className="mt-1" checked={raftIntact} onChange={(e) => setRaftIntact(e.target.checked)} />
              <span>Its Raft log is intact (it rejoins as the same voter; otherwise as a learner).</span>
            </label>
          )}
          <div className="flex justify-end gap-2">
            <button className={secondaryButton} onClick={close}>
              Cancel
            </button>
            <button
              className={primaryButton}
              disabled={busy}
              onClick={async () => {
                setBusy(true);
                setError(null);
                try {
                  setIssued(await api.rejoinToken(node.meta.id, { raft_intact: raftIntact, ttl_secs: 3600 }));
                } catch (e) {
                  setError(errorMessage(e));
                } finally {
                  setBusy(false);
                }
              }}
            >
              Create token
            </button>
          </div>
        </div>
      )}
    </Modal>
  );
}

function DetachDialog({ node, onClose }: { node: NodeRecord; onClose: () => void }) {
  const live = useLive();
  // Cluster networks its VMs use need a node network to land on.
  // Keyed by id (names repeat across tenants); the target may be a name or an id.
  const clusterNets = [...live.networks.values()].filter(isClusterNetwork);
  const [map, setMap] = useState<Record<string, string>>({});
  const [withAccess, setWithAccess] = useState(false);
  return (
    <ConfirmDialog
      title={`Detach ${nodeLabel(node)}`}
      consequence={
        <>
          The node leaves the cluster <strong>with its VMs, disks and node networks</strong> and becomes a standalone host (or can be imported
          into another cluster). Its VMs keep running. NICs on a cluster network move to the node network you map them to; unmapped ones are
          dropped.
        </>
      }
      typeToConfirm={node.spec.name}
      actionLabel="Detach"
      onConfirm={async () => {
        const m = Object.fromEntries(Object.entries(map).filter(([, v]) => v.trim()));
        await api.detachNode(node.meta.id, { map_networks: m, with_access: withAccess });
      }}
      onClose={onClose}
    >
      {clusterNets.length > 0 && (
        <div className="space-y-2">
          <p className="font-medium">Cluster network → node network on {node.spec.name}</p>
          {clusterNets.map((n) => (
            <div key={n.id} className="grid grid-cols-2 gap-2 items-center">
              <span className="font-mono text-xs" title={n.id}>
                {n.name}
              </span>
              <input
                className={inputClass}
                placeholder="leave empty to drop"
                value={map[n.id] ?? ""}
                onChange={(e) => setMap((m) => ({ ...m, [n.id]: e.target.value }))}
              />
            </div>
          ))}
        </div>
      )}
      <label className="flex items-start gap-2">
        <input type="checkbox" className="mt-1" checked={withAccess} onChange={(e) => setWithAccess(e.target.checked)} />
        <span>Carry the projects' users, teams and role links too (never sessions or tokens).</span>
      </label>
    </ConfirmDialog>
  );
}
