import { useState } from "react";
import * as api from "../api";
import type { VmResponse } from "../types";
import { useCan } from "../session";
import { useNodes } from "../nodes";
import Modal from "./Modal";

/** Whether the VM's node was forgotten (spec/14-clustering.md §5.6). */
const isLost = (vm: VmResponse) => (vm.conditions ?? []).some((c) => c.kind === "Ready" && c.reason === "Lost");

/** "Place on another node" (spec/24-storage.md S10): a stopped VM, or a lost
 * one, goes to a chosen node or where the scheduler finds room. Shown to
 * cluster operators (`placeVm`). */
export default function PlaceVm({ vm, stopped, onDone }: { vm: VmResponse; stopped: boolean; onDone: () => void }) {
  const dir = useNodes();
  const [can] = useCan([{ action: "placeVm", resource: { type: "Vm", id: vm.id } }]) ?? [];
  const [open, setOpen] = useState(false);
  const [node, setNode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const lost = isLost(vm);
  if (!can || !dir.clustered || (!stopped && !lost)) return null;
  const nodes = [...dir.nodes.values()].filter(
    (n) => n.meta.id !== vm.node && n.status?.ready === "CONDITION_STATUS_TRUE",
  );

  const place = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.placeVm(vm.id, node || undefined);
      setOpen(false);
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <button
        className="px-3 py-2 text-sm font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg"
        onClick={() => setOpen(true)}
      >
        {lost ? "Place on a live node" : "Place on another node"}
      </button>
      {open && (
        <Modal title={`Place ${vm.name}`} onClose={() => setOpen(false)}>
          <div className="space-y-4 text-sm">
            {error && <p className="text-red-600">{error}</p>}
            <p className="text-gray-600">
              {lost
                ? "Its node was forgotten. It can run on another node if its disks are on shared datastores."
                : "It moves while stopped, with its disks on shared datastores; start it again afterwards."}
            </p>
            <div>
              <label className="block text-sm font-medium text-gray-700">Node</label>
              <select
                className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg bg-white"
                value={node}
                onChange={(e) => setNode(e.target.value)}
              >
                <option value="">Where there is room</option>
                {nodes.map((n) => (
                  <option key={n.meta.id} value={n.meta.id}>
                    {n.spec.name}
                  </option>
                ))}
              </select>
            </div>
            <div className="flex justify-end space-x-3">
              <button className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg" onClick={() => setOpen(false)}>
                Cancel
              </button>
              <button
                disabled={busy}
                className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50"
                onClick={place}
              >
                {busy ? "Placing…" : "Place"}
              </button>
            </div>
          </div>
        </Modal>
      )}
    </>
  );
}
