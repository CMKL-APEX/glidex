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
}

export default function VmActions({
  vmId,
  state,
  desired,
  deleting = false,
  onAction,
  loading = false,
}: VmActionsProps) {
  const [confirmForceOff, setConfirmForceOff] = useState(false);
  if (deleting) return <span className="text-sm text-gray-500">Being deleted…</span>;
  const canStart =
    state === "created" || state === "stopped" || state === "paused" || (state === "failed" && desired !== "running");
  // Also while stopping: a guest that ignores the power button would
  // otherwise keep the page waiting out the whole grace.
  const canStop =
    state === "running" ||
    state === "paused" ||
    state === "stopping" ||
    (desired !== undefined && desired !== "stopped");
  const canPause = state === "running";
  // The power button only reaches a running guest.
  const canShutdown = state === "running";

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
      <button
        className="px-3 py-1.5 text-sm font-medium text-white bg-red-600 hover:bg-red-700 rounded-lg transition-colors disabled:opacity-50"
        disabled={loading}
        onClick={() => onAction(vmId, "delete")}
      >
        Delete
      </button>
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
