import { useEffect, useState } from "react";
import * as api from "../api";
import type { PlacementPreview } from "../types";
import { selectClass } from "./ui";

/** Where a new VM runs (spec/clustering-ui.md §5.1): "Automatic" (the
 * scheduler picks) or a node, each shown with what it has left, and why it
 * can't take the VM when it can't. Asks the scheduler again as the request
 * changes. */
export default function PlacementPicker({
  request,
  value,
  onChange,
}: {
  request: api.PreviewRequest;
  /** A node id, or "" for automatic. */
  value: string;
  onChange: (node: string) => void;
}) {
  const [preview, setPreview] = useState<PlacementPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const key = JSON.stringify(request);

  useEffect(() => {
    let live = true;
    // Typing changes the request quickly: ask once it settles.
    const t = window.setTimeout(() => {
      api
        .previewPlacement(JSON.parse(key) as api.PreviewRequest)
        .then((p) => {
          if (!live) return;
          setPreview(p);
          setError(null);
        })
        .catch((e) => live && setError(e instanceof Error ? e.message : String(e)));
    }, 300);
    return () => {
      live = false;
      window.clearTimeout(t);
    };
  }, [key]);

  const pick = preview?.nodes.find((n) => n.id === preview.pick);
  const chosen = preview?.nodes.find((n) => n.id === value);
  const nothingFits = preview !== null && preview.pick === null;

  return (
    <div>
      <label htmlFor="vm-placement" className="block text-sm font-medium text-gray-700">
        Placement
      </label>
      <select id="vm-placement" className={`mt-1 ${selectClass}`} value={value} onChange={(e) => onChange(e.target.value)}>
        <option value="">{pick ? `Automatic (now: ${pick.name})` : "Automatic"}</option>
        {preview?.nodes
          .slice()
          .sort((a, b) => a.name.localeCompare(b.name))
          .map((n) => (
            <option key={n.id} value={n.id} disabled={!!n.refused && !n.refused.startsWith("pinned")}>
              {n.name} — {Math.max(0, n.vcpus_capacity - n.vcpus_taken)} vCPUs, {Math.max(0, n.mem_mib_capacity - n.mem_mib_taken)} MiB free
              {n.refused && !n.refused.startsWith("pinned") ? ` (${n.refused})` : ""}
            </option>
          ))}
      </select>
      {error && <p className="mt-1 text-xs text-gray-500">Placement preview unavailable: {error}</p>}
      {chosen?.refused && !chosen.refused.startsWith("pinned") && (
        <p className="mt-1 text-xs text-amber-700">{chosen.name} can't take it now ({chosen.refused}); it waits until it can.</p>
      )}
      {!value && nothingFits && (
        <p className="mt-1 text-xs text-amber-700">
          No node can take it now; it is created and waits for one:{" "}
          {preview!.nodes.map((n) => `${n.name}: ${n.refused}`).join("; ")}
        </p>
      )}
    </div>
  );
}
