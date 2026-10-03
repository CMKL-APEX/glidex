import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import * as api from "../api";
import type { VmResponse } from "../types";
import { stateColor, stateLabel, settled, notReadyReason, HYPERVISOR_LABELS } from "../types";
import VmActions, { type VmAction } from "../components/VmActions";
import { Loading } from "../components/Loading";
import { useSession } from "../session";

export default function VmDetail() {
  const { id } = useParams<{ id: string }>();
  const { projectName } = useSession();
  const navigate = useNavigate();
  const [vm, setVm] = useState<VmResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [actionLoading, setActionLoading] = useState(false);
  const [events, setEvents] = useState<api.VmEvent[]>([]);

  const fetchVm = useCallback(async () => {
    if (!id) return;
    try {
      const data = await api.getVm(id);
      setVm(data);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load VM");
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    fetchVm();
  }, [fetchVm]);

  // Follow the VM while it converges; refresh its history with it.
  const converging = vm !== null && !settled(vm);
  useEffect(() => {
    if (!converging) return;
    const t = setInterval(fetchVm, 2000);
    return () => clearInterval(t);
  }, [converging, fetchVm]);
  useEffect(() => {
    if (!id) return;
    api.vmEvents(id).then((r) => setEvents(r.events)).catch(() => setEvents([]));
  }, [id, vm?.state, vm?.observed_generation]);

  const handleAction = async (vmId: string, action: VmAction) => {
    setActionLoading(true);
    setError(null);
    try {
      switch (action) {
        case "start":
          await api.startVm(vmId);
          break;
        case "shutdown":
          await api.stopVm(vmId, 60);
          break;
        case "stop":
          await api.stopVm(vmId);
          break;
        case "pause":
          await api.pauseVm(vmId);
          break;
        case "delete":
          await api.deleteVm(vmId);
          navigate("/");
          return;
      }
      fetchVm();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Action failed");
    } finally {
      setActionLoading(false);
    }
  };

  return (
    <div>
      <Link
        to="/"
        className="text-sky-600 hover:text-sky-700 mb-4 inline-flex items-center"
      >
        <svg
          className="w-4 h-4 mr-1"
          fill="none"
          stroke="currentColor"
          viewBox="0 0 24 24"
        >
          <path
            strokeLinecap="round"
            strokeLinejoin="round"
            strokeWidth="2"
            d="M15 19l-7-7 7-7"
          />
        </svg>
        Back to Dashboard
      </Link>

      {error && (
        <div className="mb-4 p-4 bg-red-50 border border-red-200 rounded-lg">
          <div className="flex items-center justify-between">
            <p className="text-red-700">{error}</p>
            <button
              className="text-red-500 hover:text-red-700"
              onClick={() => setError(null)}
            >
              Dismiss
            </button>
          </div>
        </div>
      )}

      {loading ? (
        <Loading />
      ) : vm ? (
        <div className="bg-white rounded-xl shadow-md p-6 border border-gray-100 mt-4">
          <div className="flex items-start justify-between mb-6">
            <div>
              <h1 className="text-2xl font-bold text-gray-900">{vm.name}</h1>
              <p className="text-gray-500 font-mono text-sm mt-1">{vm.id}</p>
            </div>
            <span
              className={`px-3 py-1 text-sm font-medium text-white rounded-full ${stateColor(vm.state)}`}
            >
              {stateLabel(vm.state)}
              {!settled(vm) && vm.desired_state ? ` → ${stateLabel(vm.desired_state)}` : ""}
            </span>
          </div>

          {notReadyReason(vm) && (
            <div className="mb-6 p-3 bg-amber-50 border border-amber-200 rounded-lg text-sm text-amber-800 whitespace-pre-wrap">
              {notReadyReason(vm)}
            </div>
          )}
          {vm.restart_required && (
            <div className="mb-6 p-3 bg-sky-50 border border-sky-200 rounded-lg text-sm text-sky-800">
              Configuration changes take effect at the next start.
            </div>
          )}

          <div className="grid grid-cols-1 md:grid-cols-2 gap-6 mb-6">
            <div className="space-y-4">
              <div>
                <h3 className="text-sm font-medium text-gray-500">
                  Hypervisor
                </h3>
                <p className="text-lg font-semibold text-gray-900">
                  {HYPERVISOR_LABELS[vm.hypervisor] ?? vm.hypervisor}
                </p>
              </div>
              <div>
                <h3 className="text-sm font-medium text-gray-500">
                  vCPU Count
                </h3>
                <p className="text-lg font-semibold text-gray-900">
                  {vm.vcpu_count}
                </p>
              </div>
              <div>
                <h3 className="text-sm font-medium text-gray-500">Memory</h3>
                <p className="text-lg font-semibold text-gray-900">
                  {vm.mem_size_mib} MiB
                </p>
              </div>
              {vm.nics && vm.nics.length > 0 && (
                <div>
                  <h3 className="text-sm font-medium text-gray-500">Network</h3>
                  <ul className="mt-1 space-y-1 text-sm">
                    {vm.nics.map((nic, i) => (
                      <li key={i} className="font-mono">
                        <span className="text-gray-900">{nic.network}</span>{" "}
                        <span className="text-gray-500">{nic.mac}</span>{" "}
                        <span className={nic.ipv4 ? "text-green-700" : "text-gray-400"}>
                          {nic.ipv4 ?? "no address"}
                        </span>
                      </li>
                    ))}
                  </ul>
                </div>
              )}
            </div>
            <div className="space-y-4">
              <div>
                <h3 className="text-sm font-medium text-gray-500">Project</h3>
                <p className="text-lg font-semibold text-gray-900">
                  <Link to={`/projects/${vm.project}`} className="text-sky-700 hover:underline">
                    {projectName(vm.project)}
                  </Link>
                </p>
              </div>
              {vm.root_disk && (
                <div>
                  <h3 className="text-sm font-medium text-gray-500">Root Disk</h3>
                  <p className="font-mono text-sm text-gray-700 break-all">{vm.root_disk}</p>
                </div>
              )}
            </div>
          </div>

          {(vm.vfio_devices ?? []).length > 0 && (
            <div className="mb-6">
              <h3 className="text-sm font-medium text-gray-500 mb-2">
                VFIO PCI Devices
              </h3>
              <ul className="space-y-1">
                {(vm.vfio_devices ?? []).map((dev) => (
                  <li key={dev} className="font-mono text-sm text-gray-700">
                    {dev}
                  </li>
                ))}
              </ul>
            </div>
          )}

          <div className="pt-6 border-t border-gray-100">
            <h3 className="text-sm font-medium text-gray-500 mb-3">Actions</h3>
            <div className="flex items-center gap-3 flex-wrap">
              <VmActions
                vmId={vm.id}
                state={vm.state}
                desired={vm.desired_state}
                onAction={handleAction}
                loading={actionLoading}
              />
              <Link
                to={`/vms/${vm.id}/console`}
                className="inline-flex items-center px-3 py-2 text-sm font-medium text-white bg-gray-800 hover:bg-gray-900 rounded-lg"
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
            </div>
          </div>

          {events.length > 0 && (
            <div className="pt-6 mt-6 border-t border-gray-100">
              <h3 className="text-sm font-medium text-gray-500 mb-3">Events</h3>
              <ul className="space-y-1 text-sm">
                {[...events].reverse().map((e, i) => (
                  <li key={i} className="flex gap-3">
                    <span className="text-gray-400 font-mono shrink-0">
                      {new Date(e.at * 1000).toLocaleString()}
                    </span>
                    <span className={e.kind === "warning" ? "text-amber-700 font-medium shrink-0" : "text-gray-700 font-medium shrink-0"}>
                      {e.reason}
                    </span>
                    <span className="text-gray-600 break-all">{e.message}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}
        </div>
      ) : (
        <div className="text-center py-12 mt-4">
          <p className="text-red-500 text-lg">Error: {error}</p>
          <Link
            to="/"
            className="mt-4 inline-block px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg"
          >
            Back to Dashboard
          </Link>
        </div>
      )}
    </div>
  );
}
