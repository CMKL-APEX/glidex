import { useEffect, useState } from "react";
import * as api from "../api";
import type { VmResponse } from "../types";

export default function VmCpuScale({ vm, onChanged }: { vm: VmResponse; onChanged: () => void }) {
  const [editing, setEditing] = useState(false);
  const [vcpus, setVcpus] = useState(vm.vcpu_count);
  const [minVcpus, setMinVcpus] = useState(vm.min_vcpus ?? vm.vcpu_count);
  const [maxVcpus, setMaxVcpus] = useState(vm.max_vcpus ?? vm.vcpu_count);
  const [weight, setWeight] = useState(vm.cpu_weight ?? 100);
  const [isElastic, setIsElastic] = useState(
    vm.min_vcpus !== undefined && vm.max_vcpus !== undefined && vm.min_vcpus < vm.max_vcpus,
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setVcpus(vm.vcpu_count);
    setMinVcpus(vm.min_vcpus ?? vm.vcpu_count);
    setMaxVcpus(vm.max_vcpus ?? vm.vcpu_count);
    setWeight(vm.cpu_weight ?? 100);
    setIsElastic(vm.min_vcpus !== undefined && vm.max_vcpus !== undefined && vm.min_vcpus < vm.max_vcpus);
  }, [vm]);

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      if (isElastic) {
        await api.setVmCpuPolicy(vm.id, {
          min_vcpus: Number(minVcpus),
          max_vcpus: Number(maxVcpus),
          cpu_weight: Number(weight),
        });
      } else {
        await api.setVmCpuPolicy(vm.id, {
          vcpu_count: Number(vcpus),
          cpu_weight: Number(weight),
        });
      }
      setEditing(false);
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Scaling CPU policy failed");
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
          className="text-xs text-sky-600 hover:text-sky-800 underline focus:outline-none"
        >
          Scale CPU / Elasticity
        </button>
      </div>
    );
  }

  return (
    <div className="mt-2 p-3 bg-gray-50 border border-gray-200 rounded-lg text-sm max-w-md">
      <div className="font-medium text-gray-700 mb-2">Adjust CPU Policy</div>
      {error && <div className="text-xs text-red-600 mb-2">{error}</div>}

      <div className="mb-2">
        <label className="flex items-center gap-2 text-xs text-gray-600 mb-1">
          <input
            type="checkbox"
            checked={isElastic}
            onChange={(e) => setIsElastic(e.target.checked)}
            className="rounded text-sky-600"
          />
          Elastic CPU scaling (pylond microsecond elasticity)
        </label>
      </div>

      {isElastic ? (
        <div className="grid grid-cols-2 gap-2 mb-2">
          <div>
            <label className="block text-xs text-gray-500">Min vCPUs</label>
            <input
              type="number"
              min={1}
              max={maxVcpus}
              value={minVcpus}
              onChange={(e) => setMinVcpus(Number(e.target.value))}
              className="w-full px-2 py-1 border border-gray-300 rounded text-sm bg-white"
            />
          </div>
          <div>
            <label className="block text-xs text-gray-500">Max vCPUs</label>
            <input
              type="number"
              min={minVcpus}
              max={64}
              value={maxVcpus}
              onChange={(e) => setMaxVcpus(Number(e.target.value))}
              className="w-full px-2 py-1 border border-gray-300 rounded text-sm bg-white"
            />
          </div>
        </div>
      ) : (
        <div className="mb-2">
          <label className="block text-xs text-gray-500">vCPUs</label>
          <input
            type="number"
            min={1}
            max={64}
            value={vcpus}
            onChange={(e) => setVcpus(Number(e.target.value))}
            className="w-full px-2 py-1 border border-gray-300 rounded text-sm bg-white"
          />
        </div>
      )}

      <div className="mb-3">
        <label className="block text-xs text-gray-500">Weight (1–10000)</label>
        <input
          type="number"
          min={1}
          max={10000}
          value={weight}
          onChange={(e) => setWeight(Number(e.target.value))}
          className="w-full px-2 py-1 border border-gray-300 rounded text-sm bg-white"
        />
      </div>

      <div className="flex gap-2">
        <button
          type="button"
          onClick={save}
          disabled={busy}
          className="px-3 py-1 text-xs font-medium text-white bg-sky-600 hover:bg-sky-700 rounded disabled:opacity-50"
        >
          {busy ? "Saving..." : "Apply"}
        </button>
        <button
          type="button"
          onClick={() => setEditing(false)}
          disabled={busy}
          className="px-3 py-1 text-xs font-medium text-gray-600 bg-gray-200 hover:bg-gray-300 rounded"
        >
          Cancel
        </button>
      </div>
    </div>
  );
}
