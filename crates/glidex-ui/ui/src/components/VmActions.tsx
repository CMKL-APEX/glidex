import type { PowerState, VmState } from "../types";

export type VmAction = "start" | "shutdown" | "stop" | "pause" | "delete";

interface VmActionsProps {
  vmId: string;
  state: VmState;
  /** What the VM should be doing; a VM still trying to start can be stopped. */
  desired?: PowerState;
  onAction: (vmId: string, action: VmAction) => void;
  loading?: boolean;
}

export default function VmActions({
  vmId,
  state,
  desired,
  onAction,
  loading = false,
}: VmActionsProps) {
  const canStart =
    state === "created" || state === "stopped" || state === "paused" || (state === "failed" && desired !== "running");
  const canStop =
    state === "running" || state === "paused" || (desired !== undefined && desired !== "stopped");
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
          disabled={loading}
          title="Stop immediately, like pulling the plug"
          onClick={() => onAction(vmId, "stop")}
        >
          Stop
        </button>
      )}
      <button
        className="px-3 py-1.5 text-sm font-medium text-white bg-red-600 hover:bg-red-700 rounded-lg transition-colors disabled:opacity-50"
        disabled={loading}
        onClick={() => onAction(vmId, "delete")}
      >
        Delete
      </button>
    </div>
  );
}
