import { Link } from "react-router-dom";
import type { VmResponse } from "../types";
import { nodeUnreachable, notReadyReason, HYPERVISOR_LABELS } from "../types";
import NodeName from "./NodeName";
import VmActions, { type VmAction } from "./VmActions";
import VmStateBadge from "./VmStateBadge";

interface VmCardProps {
  vm: VmResponse;
  onAction: (vmId: string, action: VmAction) => void;
}

export default function VmCard({ vm, onAction }: VmCardProps) {
  const vfioCount = (vm.vfio_devices ?? []).length;
  const unreachable = nodeUnreachable(vm);
  // Only a live guest has a console worth opening, on a node that answers.
  const hasConsole = !vm.deleting && !unreachable && (vm.state === "running" || vm.state === "paused");

  return (
    <div className="bg-white rounded-xl shadow-md p-6 border border-gray-100 hover:shadow-lg transition-shadow duration-200">
      <div className="flex items-start justify-between">
        <div className="flex-1 min-w-0">
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
            <h3 className="text-lg font-semibold text-gray-900 truncate">
              {vm.name}
            </h3>
            <VmStateBadge vm={vm} />
            {unreachable && (
              <span
                className="px-2 py-1 text-xs font-medium rounded-full bg-gray-200 text-gray-700"
                title="Its node stopped reporting; the VM may still be running. Nothing is restarted elsewhere."
              >
                node unreachable
              </span>
            )}
          </div>
          <p className="mt-1 text-sm text-gray-500 font-mono truncate">
            {vm.id}
          </p>
          {!unreachable && notReadyReason(vm) && (
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
        {vm.node && (
          <div>
            <span className="text-gray-500">Node:</span>
            <NodeName id={vm.node} name={vm.node_name} className="ml-2 font-medium text-gray-900" />
          </div>
        )}
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

      <div className="mt-4 pt-4 border-t border-gray-100 flex items-center gap-2 flex-wrap">
        <VmActions vmId={vm.id} state={vm.state} desired={vm.desired_state} deleting={vm.deleting} onAction={onAction} />
        {hasConsole && (
          <Link
            to={`/vms/${vm.id}/console`}
            className="inline-flex items-center px-3 py-1.5 text-sm font-medium text-white bg-gray-800 hover:bg-gray-900 rounded-lg transition-colors"
          >
            <svg
              className="w-4 h-4 mr-1.5"
              fill="none"
              stroke="currentColor"
              viewBox="0 0 24 24"
            >
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                strokeWidth="2"
                d="M8 9l3 3-3 3m5 0h3M5 20h14a2 2 0 002-2V6a2 2 0 00-2-2H5a2 2 0 00-2 2v12a2 2 0 002 2z"
              />
            </svg>
            Open Console
          </Link>
        )}
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
