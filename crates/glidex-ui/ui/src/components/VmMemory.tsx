import { useEffect, useState } from "react";
import * as api from "../api";
import { vmState, type VmResponse } from "../types";

/** A VM's memory, and changing it (`UpdateVm` `mem_size_mib`): from the
 * next start on a running VM; an increase counts against the quota. */
export default function VmMemory({ vm, onChanged }: { vm: VmResponse; onChanged: () => void }) {
  const [editing, setEditing] = useState(false);
  const [mib, setMib] = useState(vm.mem_size_mib);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => setMib(vm.mem_size_mib), [vm.mem_size_mib]);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.setVmMemory(vm.id, Number(mib));
      setEditing(false);
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Changing the memory failed");
    } finally {
      setBusy(false);
    }
  };

  if (!editing) {
    return (
      <div className="mt-1">
        <button
          type="button"
          onClick={() => setEditing(true)}
          disabled={!!vm.deleting}
          className="text-xs text-sky-600 hover:text-sky-800 underline focus:outline-none"
        >
          Change memory
        </button>
      </div>
    );
  }

  const valid = Number.isInteger(Number(mib)) && Number(mib) > 0;
  return (
    <div className="mt-2 p-3 bg-gray-50 border border-gray-200 rounded-lg text-sm max-w-md">
      <div className="font-medium text-gray-700 mb-2">Change memory</div>
      {error && <div className="text-xs text-red-600 mb-2">{error}</div>}
      <div className="mb-3">
        <label htmlFor="vm-memory" className="block text-xs text-gray-500">
          Memory (MiB)
        </label>
        <input
          id="vm-memory"
          type="number"
          min={1}
          step={256}
          value={mib}
          onChange={(e) => setMib(Number(e.target.value))}
          className="w-full px-2 py-1 border border-gray-300 rounded text-sm bg-white"
        />
        <p className="mt-1 text-xs text-gray-500">
          {valid && Number(mib) >= 1024 ? `${(Number(mib) / 1024).toFixed(Number(mib) % 1024 ? 2 : 0)} GiB. ` : ""}
          {["stopped", "created"].includes(vmState(vm))
            ? "Applies when the VM starts."
            : "Applies at the VM's next start."}
        </p>
      </div>
      <div className="flex gap-2">
        <button
          type="button"
          onClick={save}
          disabled={busy || !valid || Number(mib) === vm.mem_size_mib}
          className="px-3 py-1 text-xs font-medium text-white bg-sky-600 hover:bg-sky-700 rounded disabled:opacity-50"
        >
          {busy ? "Saving..." : "Apply"}
        </button>
        <button
          type="button"
          onClick={() => {
            setEditing(false);
            setMib(vm.mem_size_mib);
            setError(null);
          }}
          disabled={busy}
          className="px-3 py-1 text-xs font-medium text-gray-600 bg-gray-200 hover:bg-gray-300 rounded"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}
