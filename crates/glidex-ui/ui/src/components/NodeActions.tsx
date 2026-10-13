import { useState } from "react";
import * as api from "../api";
import type { NodeRecord } from "../types";
import { hostOf, useAllowed, useNodes } from "../nodes";
import ConfirmDialog from "./ConfirmDialog";
import { secondaryButton } from "./ui";

/** Operations on one node, each shown only when allowed
 * (spec/clustering-ui.md §7). */
export default function NodeActions({ node, onDone }: { node: NodeRecord; onDone: () => void }) {
  const dir = useNodes();
  const id = node.meta.id;
  const host = hostOf(id, dir.self);
  const can = useAllowed(["drainNode", "undrainNode"], [host]);
  const [dialog, setDialog] = useState<"drain" | null>(null);
  const phase = node.status.phase;

  return (
    <div className="flex flex-wrap gap-2">
      {phase === "Active" && can?.(host, "drainNode") && (
        <button className={secondaryButton} onClick={() => setDialog("drain")}>
          Drain
        </button>
      )}
      {phase === "Draining" && can?.(host, "undrainNode") && (
        <button
          className={secondaryButton}
          onClick={async () => {
            await api.drainNode(id, false).catch(() => {});
            onDone();
          }}
        >
          Undrain
        </button>
      )}
      {dialog === "drain" && (
        <ConfirmDialog
          title={`Drain ${node.spec.name}`}
          consequence="No new VMs are placed on this node. VMs already on it keep running; move or stop them before removing the node."
          actionLabel="Drain"
          danger={false}
          onConfirm={async () => {
            await api.drainNode(id, true);
            onDone();
          }}
          onClose={() => setDialog(null)}
        />
      )}
    </div>
  );
}
