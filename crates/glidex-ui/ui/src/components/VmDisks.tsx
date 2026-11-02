import { useEffect, useState } from "react";
import * as api from "../api";
import type { DiskInfo, VmResponse } from "../types";

/** A VM's disks by name: the root disk, and whether it was made for the
 * VM (deleted with it unless kept); the data disks, each detachable (on a
 * running VM at its next start; the disk stays claimed until then); and
 * attaching another of the project's free disks (also from the next
 * start on a running VM). */
export default function VmDisks({ vm, onChanged }: { vm: VmResponse; onChanged: () => void }) {
  const [disks, setDisks] = useState<DiskInfo[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [picked, setPicked] = useState("");

  const data = vm.data_disks ?? [];
  const dataKey = data.join();
  useEffect(() => {
    api
      .listDisks(vm.project)
      .then(setDisks)
      .catch(() => setDisks([]));
  }, [vm.project, vm.root_disk, dataKey]);

  const disk = (id: string) => disks.find((d) => d.id === id);
  const name = (id: string) => disk(id)?.name ?? id;
  const detach = async (id: string) => {
    if (!confirm(`Detach disk ${name(id)} from ${vm.name}? The disk and its data are kept.`)) return;
    setBusy(id);
    setError(null);
    try {
      await api.detachVmDisk(vm.id, id);
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Detaching the disk failed");
    } finally {
      setBusy(null);
    }
  };

  const attach = async () => {
    setBusy(picked);
    setError(null);
    try {
      await api.attachVmDisk(vm.id, picked);
      setPicked("");
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Attaching the disk failed");
    } finally {
      setBusy(null);
    }
  };
  // Free disks: not attached anywhere, ready, not on their way out.
  const free = disks.filter((d) => !d.attached_to && !d.deleting && d.status === "ready" && d.id !== vm.root_disk && !data.includes(d.id));

  if (!vm.root_disk && data.length === 0 && free.length === 0) return null;
  return (
    <div data-testid="vm-disks">
      <h3 className="text-sm font-medium text-gray-500">Disks</h3>
      <ul className="text-sm text-gray-700 space-y-1">
        {vm.root_disk && (
          <li>
            <span className="font-mono break-all">{name(vm.root_disk)}</span> <span className="text-gray-500">root</span>
            {disk(vm.root_disk)?.owner === vm.id && (
              <span className="text-gray-500" title="Made for this VM; delete the VM with Keep disk to keep it">
                {" "}
                · deleted with the VM
              </span>
            )}
          </li>
        )}
        {data.map((id) => (
          <li key={id} className="flex flex-wrap items-center gap-2">
            <span className="font-mono break-all">{name(id)}</span>
            <span className="text-gray-500">data · kept when the VM is deleted</span>
            <button
              className="px-2 py-0.5 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-lg disabled:opacity-50"
              disabled={busy !== null || !!vm.deleting}
              onClick={() => detach(id)}
            >
              {busy === id ? "Detaching…" : "Detach"}
            </button>
          </li>
        ))}
      </ul>
      {free.length > 0 && !vm.deleting && (
        <div className="mt-2 flex flex-wrap items-center gap-2">
          <label htmlFor="attach-disk" className="sr-only">
            Disk to attach
          </label>
          <select
            id="attach-disk"
            className="px-2 py-1 border border-gray-300 rounded text-sm bg-white"
            value={picked}
            onChange={(e) => setPicked(e.target.value)}
            disabled={busy !== null}
          >
            <option value="">Attach a disk…</option>
            {free.map((d) => (
              <option key={d.id} value={d.id}>
                {d.name}
              </option>
            ))}
          </select>
          <button
            className="px-2 py-0.5 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-lg disabled:opacity-50"
            disabled={!picked || busy !== null}
            onClick={attach}
          >
            {busy !== null && busy === picked ? "Attaching…" : "Attach"}
          </button>
        </div>
      )}
      {error && <p className="mt-1 text-xs text-red-600">{error}</p>}
    </div>
  );
}
