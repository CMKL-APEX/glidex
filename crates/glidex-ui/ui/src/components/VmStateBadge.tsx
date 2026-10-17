import type { VmResponse } from "../types";
import { desiredState, stateColor, stateLabel, vmActivity, vmState } from "../types";
import { Spinner } from "./Activity";

/** The VM's observed state, and what the controller is still doing to
 * reach the desired one (spec/reconciliation.md §7.4). */
export default function VmStateBadge({ vm, large = false }: { vm: VmResponse; large?: boolean }) {
  const activity = vmActivity(vm);
  const state = vmState(vm);
  const desired = desiredState(vm);
  const size = large ? "px-3 py-1 text-sm" : "px-2 py-1 text-xs";
  return (
    <span className="inline-flex items-center gap-2">
      <span className={`${size} font-medium text-white rounded-full ${vm.deleting ? "bg-gray-500" : stateColor(state)}`}>
        {vm.deleting ? "Deleting" : stateLabel(state)}
        {!vm.deleting && activity && desired && activity !== "Applying changes" && activity !== "Reconciling"
          ? ` → ${stateLabel(desired)}`
          : ""}
      </span>
      {activity && (
        <span className="inline-flex items-center gap-1 text-xs text-sky-700" title="The control plane is still working on this VM">
          <Spinner />
          {activity}
        </span>
      )}
    </span>
  );
}
