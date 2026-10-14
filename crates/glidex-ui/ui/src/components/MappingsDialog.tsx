import { useState } from "react";
import * as api from "../api";
import type { NodeRecord } from "../types";
import ConfirmDialog from "./ConfirmDialog";
import { inputClass } from "./ui";

/** A node's physical networks (spec/clustering.md §11.3): which of its
 * bridges (with an uplink) is which physical network, for provider networks
 * and OVN gateways. Takes effect without a restart. */
export default function MappingsDialog({ node, onClose, onDone }: { node: NodeRecord; onClose: () => void; onDone: () => void }) {
  const own = node.spec.bridge_mappings ?? {};
  const fromFile = Object.entries(node.status.features?.physnets ?? {}).filter(([p, b]) => own[p] !== b);
  const [rows, setRows] = useState<[string, string][]>(Object.entries(own).length ? Object.entries(own) : [["", ""]]);
  const set = (i: number, j: 0 | 1, v: string) => setRows(rows.map((r, k) => (k === i ? ((j === 0 ? [v, r[1]] : [r[0], v]) as [string, string]) : r)));
  return (
    <ConfirmDialog
      title={`Physical networks of ${node.spec.name}`}
      consequence="Each name maps to a bridge on this node that has the uplink to that network. Applied within half a minute, no restart."
      actionLabel="Save"
      danger={false}
      onConfirm={async () => {
        const maps = Object.fromEntries(rows.map(([p, b]) => [p.trim(), b.trim()]).filter(([p, b]) => p && b));
        await api.setNodeMappings(node.meta.id, maps);
        onDone();
      }}
      onClose={onClose}
    >
      <div className="space-y-2">
        {rows.map(([p, b], i) => (
          <div key={i} className="grid grid-cols-[1fr_1fr_auto] gap-2 items-center">
            <input aria-label={`Physical network ${i + 1}`} className={inputClass} placeholder="uplink" value={p} onChange={(e) => set(i, 0, e.target.value)} />
            <input aria-label={`Bridge ${i + 1}`} className={inputClass} placeholder="gxbr-up" value={b} onChange={(e) => set(i, 1, e.target.value)} />
            <button className="text-red-600 text-xs" onClick={() => setRows(rows.filter((_, k) => k !== i))}>
              Remove
            </button>
          </div>
        ))}
        <button className="text-sky-700 text-xs hover:underline" onClick={() => setRows([...rows, ["", ""]])}>
          + Add
        </button>
        {fromFile.length > 0 && (
          <p className="text-xs text-gray-500">
            Also from this host's control-plane.json: {fromFile.map(([p, br]) => `${p} → ${br}`).join(", ")} (a mapping here wins).
          </p>
        )}
      </div>
    </ConfirmDialog>
  );
}
