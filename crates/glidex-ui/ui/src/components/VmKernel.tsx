import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import * as api from "../api";
import type { ImageInfo, VmResponse } from "../types";
import { imageState, isKernel } from "../types";

/** A kernel-boot VM's kernel (spec/kernel-boot.md §6, §10.5): the kernel
 * image it names, the one it boots when following the guest's kernel, the
 * one before, and changing them from the next start. */
export default function VmKernel({ vm, onChanged }: { vm: VmResponse; onChanged: () => void }) {
  const [kernels, setKernels] = useState<ImageInfo[]>([]);
  const [choice, setChoice] = useState(vm.kernel ?? "");
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = () =>
    api
      .listImages()
      .then((all) => setKernels(all.filter((i) => isKernel(i) && !i.deleting)))
      .catch(() => setKernels([]));
  useEffect(() => {
    void load();
  }, [vm.kernel, vm.booted_kernel]);
  useEffect(() => setChoice(vm.kernel ?? ""), [vm.kernel]);

  if (!vm.kernel) return null;
  const byId = (id?: string) => kernels.find((k) => k.id === id);
  const label = (id?: string) => {
    const k = byId(id);
    if (!k) return id ?? "";
    return k.kernel?.release ? `${k.name} (${k.kernel.release})` : k.name;
  };
  const ready = kernels.filter((k) => imageState(k) === "ready");
  const stopped = vm.state === "VM_STATE_STOPPED" || vm.state === "VM_STATE_CREATED";

  const act = async (what: string, f: () => Promise<unknown>) => {
    setBusy(what);
    setError(null);
    try {
      await f();
      onChanged();
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : `${what} failed`);
    } finally {
      setBusy(null);
    }
  };

  return (
    <div data-testid="vm-kernel">
      <h3 className="text-sm font-medium text-gray-500">Kernel</h3>
      <p className="text-sm text-gray-700">
        <Link to="/images" className="font-mono text-sky-700 hover:underline">
          {label(vm.kernel)}
        </Link>
      </p>
      {vm.kernel_from_disk && (
        <p className="text-xs text-gray-600 mt-1" data-testid="vm-booted-kernel">
          Following the guest&apos;s kernel: boots{" "}
          <span className="font-mono">{vm.booted_kernel ? label(vm.booted_kernel) : label(vm.kernel)}</span>
          {vm.previous_kernel && (
            <>
              , before that <span className="font-mono">{label(vm.previous_kernel)}</span>
            </>
          )}
        </p>
      )}
      <div className="mt-2 flex flex-wrap items-center gap-2 text-sm">
        <select
          className="px-2 py-1 border border-gray-300 rounded-lg bg-white"
          aria-label="Kernel image"
          value={choice}
          onChange={(e) => setChoice(e.target.value)}
        >
          {ready.map((k) => (
            <option key={k.id} value={k.id}>
              {label(k.id)}
              {k.id === vm.previous_kernel ? " — previous" : ""}
            </option>
          ))}
        </select>
        <button
          className="px-3 py-1 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-lg disabled:opacity-50"
          disabled={busy !== null || !choice || choice === vm.kernel}
          onClick={() => act("Changing the kernel", () => api.setVmKernel(vm.id, { kernel: choice }))}
        >
          Use from the next start
        </button>
        <label className="flex items-center gap-1 text-xs text-gray-700">
          <input
            type="checkbox"
            checked={!!vm.kernel_from_disk}
            disabled={busy !== null || !vm.root_disk}
            onChange={(e) =>
              act("Changing kernel following", () => api.setVmKernel(vm.id, { follow: e.target.checked }))
            }
          />
          Follow the guest&apos;s kernel
        </label>
        {vm.root_disk && (
          <button
            className="px-3 py-1 text-xs font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg disabled:opacity-50"
            disabled={busy !== null || !stopped}
            title={stopped ? undefined : "Stop the VM first"}
            onClick={() =>
              act("Extracting the kernel", async () => {
                const k = await api.extractVmKernel(vm.id);
                // Seconds; offer it once it is ready.
                for (let i = 0; i < 90; i++) {
                  const img = await api.getImage(k.id);
                  if (imageState(img) === "ready") {
                    setChoice(img.id);
                    return;
                  }
                  if (imageState(img) !== "extracting") throw new Error(img.status?.reason ?? "extraction failed");
                  await new Promise((r) => setTimeout(r, 2000));
                }
              })
            }
          >
            {busy === "Extracting the kernel" ? "Extracting…" : "Extract from its disk now"}
          </button>
        )}
      </div>
      {busy && <p className="mt-1 text-xs text-gray-500">{busy}…</p>}
      {error && <p className="mt-1 text-xs text-red-700">{error}</p>}
    </div>
  );
}
