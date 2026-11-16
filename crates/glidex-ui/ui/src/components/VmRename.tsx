import { useEffect, useState } from "react";
import * as api from "../api";
import type { VmResponse } from "../types";

/** Renaming a VM (`UpdateVm` `name`, spec/12-reconciliation.md §7.7): in
 * any state; a root disk the VM made is renamed with it. */
export default function VmRename({ vm, onChanged }: { vm: VmResponse; onChanged: () => void }) {
  const [editing, setEditing] = useState(false);
  const [name, setName] = useState(vm.name);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => setName(vm.name), [vm.name]);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.setVmName(vm.id, name.trim());
      setEditing(false);
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Renaming the VM failed");
    } finally {
      setBusy(false);
    }
  };

  if (!editing) {
    return (
      <button
        type="button"
        onClick={() => setEditing(true)}
        disabled={!!vm.deleting}
        className="mt-1 text-xs text-sky-600 hover:text-sky-800 underline focus:outline-hidden"
      >
        Rename
      </button>
    );
  }

  const trimmed = name.trim();
  return (
    <div className="mt-2 p-3 bg-gray-50 border border-gray-200 rounded-lg text-sm max-w-md">
      <div className="font-medium text-gray-700 mb-2">Rename</div>
      {error && <div className="text-xs text-red-600 mb-2">{error}</div>}
      <div className="mb-3">
        <label htmlFor="vm-name" className="block text-xs text-gray-500">
          New name
        </label>
        <input
          id="vm-name"
          value={name}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && trimmed && trimmed !== vm.name && !busy) save();
          }}
          className="w-full px-2 py-1 border border-gray-300 rounded-sm text-sm bg-white"
          autoFocus
        />
        <p className="mt-1 text-xs text-gray-500">
          Free in the project. A root disk the VM made is renamed with it; the guest's hostname follows at its next
          start.
        </p>
      </div>
      <div className="flex gap-2">
        <button
          type="button"
          onClick={save}
          disabled={busy || !trimmed || trimmed === vm.name}
          className="px-3 py-1 text-xs font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-sm disabled:opacity-50"
        >
          {busy ? "Saving..." : "Rename"}
        </button>
        <button
          type="button"
          onClick={() => {
            setEditing(false);
            setName(vm.name);
            setError(null);
          }}
          disabled={busy}
          className="px-3 py-1 text-xs font-medium text-gray-600 bg-gray-200 hover:bg-gray-300 rounded-sm"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}
