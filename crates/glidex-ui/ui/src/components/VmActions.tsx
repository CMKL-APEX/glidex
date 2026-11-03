import { useState } from "react";
import type { PowerState, VmState } from "../types";
import ConfirmDialog from "./ConfirmDialog";

export type VmAction = "start" | "shutdown" | "stop" | "pause" | "delete";

interface VmActionsProps {
  vmId: string;
  state: VmState;
  /** What the VM should be doing; a VM still trying to start can be stopped. */
  desired?: PowerState;
  /** Being deleted: nothing to do but wait. */
  deleting?: boolean;
  onAction: (vmId: string, action: VmAction) => void;
  loading?: boolean;
  /** What the caller may do (startVm, pauseVm, stopVm, deleteVm); a button
   * the caller can't use isn't shown. Absent: all are. */
  allow?: VmAllow;
}

/** The caller's VM power and delete rights, as `useCan` answers them. */
export type VmAllow = { start: boolean; pause: boolean; stop: boolean; delete: boolean };

/** The actions behind `VmAllow`, in its order. */
export const VM_ALLOW_ACTIONS = ["startVm", "pauseVm", "stopVm", "deleteVm"] as const;

/** `VmAllow` from `useCan`'s answers to `VM_ALLOW_ACTIONS`; all false while loading. */
export function vmAllow(allowed: boolean[] | null | undefined): VmAllow {
  const a = allowed ?? [];
  return { start: !!a[0], pause: !!a[1], stop: !!a[2], delete: !!a[3] };
}

export default function VmActions({
  vmId,
  state,
  desired,
  deleting = false,
  onAction,
  loading = false,
  allow,
}: VmActionsProps) {
  const [confirmForceOff, setConfirmForceOff] = useState(false);
  if (deleting) return <span className="text-sm text-gray-500">Being deleted…</span>;
  const may = (k: keyof VmAllow) => !allow || allow[k];
  const canStart =
    may("start") &&
    state === "created" || state === "stopped" || state === "paused" || (state === "failed" && desired !== "running");
  // Also while stopping: a guest that ignores the power button would
  // otherwise keep the page waiting out the whole grace.
  const canStop =
    may("stop") &&
    (state === "running" ||
    state === "paused" ||
    state === "stopping" ||
      (desired !== undefined && desired !== "stopped"));
  const canPause = may("pause") && state === "running";
  // The power button only reaches a running guest.
  const canShutdown = may("stop") && state === "running";

  return (
    <div className="flex items-center space-x-2">
      {canStart && (
        <button
          className="px-3 py-1.5 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg transition-colors disabled:opacity-50"
          disabled={loading}
          onClick={() => onAction(vmId, "start")}
        >
          {state === "paused" ? "Resume" : "Start"}
        </button>
      )}
      {canPause && (
        <button
          className="px-3 py-1.5 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg transition-colors disabled:opacity-50"
          disabled={loading}
          onClick={() => onAction(vmId, "pause")}
        >
          Pause
        </button>
      )}
      {canShutdown && (
        <button
          className="px-3 py-1.5 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg transition-colors disabled:opacity-50"
          disabled={loading}
          title="Press the guest's power button; stop it if it hasn't shut down within 60 s"
          onClick={() => onAction(vmId, "shutdown")}
        >
          Shut down
        </button>
      )}
      {canStop && (
        <button
          className="px-3 py-1.5 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg transition-colors disabled:opacity-50"
          // Not held back by `loading`: a Shut down waits for the VM (up to
          // 80 s), and this is what cuts that wait short.
          title="Stop immediately, like pulling the plug"
          onClick={() => setConfirmForceOff(true)}
        >
          Force off
        </button>
      )}
      {may("delete") && (
        <button
          className="px-3 py-1.5 text-sm font-medium text-white bg-red-600 hover:bg-red-700 rounded-lg transition-colors disabled:opacity-50"
          disabled={loading}
          onClick={() => onAction(vmId, "delete")}
        >
          Delete
        </button>
      )}
      {confirmForceOff && (
        <ConfirmDialog
          title="Force off"
          consequence="The VM stops at once, like pulling the plug, without waiting for the guest to shut down. The guest may lose data it hasn't written to disk."
          actionLabel="Force off"
          onConfirm={async () => onAction(vmId, "stop")}
          onClose={() => setConfirmForceOff(false)}
        />
      )}
    </div>
  );
}
