import { useEffect, useState } from "react";
import * as api from "../api";
import type { DiskInfo, VmResponse } from "../types";
import ConfirmDialog from "./ConfirmDialog";

/** Delete a VM, saying which of its disks go with it: a root disk made
 * for it (its `owner`) is deleted unless kept (`keep_disk`); every other
 * disk is only detached (docs: user-guide/vms.md, Delete). */
export default function DeleteVmDialog({
  vm,
  onDelete,
  onClose,
}: {
  vm: VmResponse;
  /** Resolves when the delete is done or failed (the page shows its errors). */
  onDelete: (keepDisk: boolean) => Promise<void>;
  onClose: () => void;
}) {
  const [disks, setDisks] = useState<DiskInfo[] | null>(null);
  const [keep, setKeep] = useState(false);

  useEffect(() => {
    const ids = [vm.root_disk, ...(vm.data_disks ?? [])].filter((d): d is string => !!d);
    if (ids.length === 0) {
      setDisks([]);
      return;
    }
    api
      .listDisks(vm.project)
      .then((all) => setDisks(ids.map((id) => all.find((d) => d.id === id) ?? ({ id, name: id } as DiskInfo))))
      // Not readable: name them by id, and assume nothing is owned.
      .catch(() => setDisks(ids.map((id) => ({ id, name: id }) as DiskInfo)));
  }, [vm]);

  const owned = disks?.find((d) => d.id === vm.root_disk && d.owner === vm.id);
  const kept = (disks ?? []).filter((d) => d !== owned);
  const names = (ds: DiskInfo[]) => ds.map((d) => d.name).join(", ");

  return (
    <ConfirmDialog
      title={`Delete VM ${vm.name}`}
      consequence="It is stopped if it runs, and its NICs and their addresses are released."
      actionLabel={owned && !keep ? "Delete VM and disk" : "Delete VM"}
      onConfirm={() => onDelete(keep)}
      onClose={onClose}
    >
      {disks === null ? (
        <p className="text-gray-500">Checking its disks…</p>
      ) : (
        <>
          {owned && (
            <div className="space-y-2">
              <p>
                Root disk <span className="font-mono">{owned.name}</span> was made for this VM and is{" "}
                {keep ? "kept, detached" : <span className="font-medium text-red-700">deleted with it</span>}.
              </p>
              <label className="flex items-center gap-2">
                <input type="checkbox" checked={keep} onChange={(e) => setKeep(e.target.checked)} />
                <span>
                  Keep disk <span className="font-mono">{owned.name}</span>
                </span>
              </label>
            </div>
          )}
          {kept.length > 0 && (
            <p>
              Only detached, and kept: <span className="font-mono">{names(kept)}</span>.
            </p>
          )}
          {disks.length === 0 && <p>It has no disks.</p>}
        </>
      )}
    </ConfirmDialog>
  );
}
