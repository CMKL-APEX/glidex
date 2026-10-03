import { Link } from "react-router-dom";
import type { VmResponse } from "../types";
import { stateColor, stateLabel, settled, notReadyReason, HYPERVISOR_LABELS } from "../types";
import VmActions, { type VmAction } from "./VmActions";

interface VmCardProps {
  vm: VmResponse;
  onAction: (vmId: string, action: VmAction) => void;
}

export default function VmCard({ vm, onAction }: VmCardProps) {
  const vfioCount = (vm.vfio_devices ?? []).length;

  return (
    <div className="bg-white rounded-xl shadow-md p-6 border border-gray-100 hover:shadow-lg transition-shadow duration-200">
      <div className="flex items-start justify-between">
        <div className="flex-1 min-w-0">
          <div className="flex items-center space-x-3">
            <h3 className="text-lg font-semibold text-gray-900 truncate">
              {vm.name}
            </h3>
            <span
              className={`px-2 py-1 text-xs font-medium text-white rounded-full ${stateColor(vm.state)}`}
            >
              {stateLabel(vm.state)}
              {!settled(vm) && vm.desired_state ? ` → ${stateLabel(vm.desired_state)}` : ""}
            </span>
          </div>
          <p className="mt-1 text-sm text-gray-500 font-mono truncate">
            {vm.id}
          </p>
          {notReadyReason(vm) && (
            <p className="mt-1 text-xs text-amber-700 truncate" title={notReadyReason(vm) ?? ""}>
              {notReadyReason(vm)}
            </p>
          )}
        </div>
      </div>

      <div className="mt-4 grid grid-cols-2 gap-4 text-sm">
        <div>
          <span className="text-gray-500">vCPUs:</span>
          <span className="ml-2 font-medium text-gray-900">
            {vm.vcpu_count}
          </span>
        </div>
        <div>
          <span className="text-gray-500">Hypervisor:</span>
          <span className="ml-2 font-medium text-gray-900">
            {HYPERVISOR_LABELS[vm.hypervisor] ?? vm.hypervisor}
          </span>
        </div>
        <div>
          <span className="text-gray-500">Memory:</span>
          <span className="ml-2 font-medium text-gray-900">
            {vm.mem_size_mib} MiB
          </span>
        </div>
        {vfioCount > 0 && (
          <div>
            <span className="text-gray-500">GPU:</span>
            <span className="ml-2 font-medium text-gray-900">
              {vfioCount} device{vfioCount === 1 ? "" : "s"}
            </span>
          </div>
        )}
      </div>

      <div className="mt-4 pt-4 border-t border-gray-100">
        <VmActions vmId={vm.id} state={vm.state} desired={vm.desired_state} onAction={onAction} />
      </div>

      <Link
        to={`/vms/${vm.id}`}
        className="mt-3 inline-block text-sm text-sky-600 hover:text-sky-700"
      >
        View Details
      </Link>
    </div>
  );
}
