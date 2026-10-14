import { useCallback, useEffect, useState } from "react";
import * as api from "../api";
import type { VmResponse } from "../types";
import { settled } from "../types";
import type { VmAction } from "../components/VmActions";
import VmCard from "../components/VmCard";
import { LoadingCard } from "../components/Loading";
import Modal from "../components/Modal";
import CreateVmForm from "../components/CreateVmForm";
import type { CreateVmRequest } from "../types";
import { useCan, useSession } from "../session";
import { useLiveRefresh } from "../live";
import { useNodes } from "../nodes";
import { selectClass } from "../components/ui";

export default function Dashboard() {
  const { project, projectName } = useSession();
  const [vms, setVms] = useState<VmResponse[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [showCreateModal, setShowCreateModal] = useState(false);
  const nodes = useNodes();
  const [nodeFilter, setNodeFilter] = useState("");
  const [canCreate] = useCan(project ? [{ action: "createVm", resource: { type: "Project", id: project } }] : []) ?? [];

  const fetchVms = useCallback(async () => {
    try {
      const data = await api.listVms(project);
      setVms(data);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load VMs");
    } finally {
      setLoading(false);
    }
  }, [project]);

  useEffect(() => {
    fetchVms();
  }, [fetchVms]);

  // Follow changes on the live stream; without it, poll while any VM is
  // on its way to its desired state.
  const live = useLiveRefresh(["vm"], fetchVms);
  const converging = (vms ?? []).some((vm) => !settled(vm));
  useEffect(() => {
    if (!converging || live) return;
    const t = setInterval(fetchVms, 2000);
    return () => clearInterval(t);
  }, [converging, live, fetchVms]);

  const handleAction = async (vmId: string, action: VmAction) => {
    if (action === "delete") {
      const name = vms?.find((v) => v.id === vmId)?.name ?? vmId;
      if (!confirm(`Delete VM ${name}? A root disk created with it is deleted too.`)) return;
    }
    setError(null);
    // The call waits for the controller (up to a minute); show the VM's
    // progress meanwhile instead of the state from before the click.
    const peek = setTimeout(fetchVms, 500);
    try {
      await api.vmAction(vmId, action);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Action failed");
    } finally {
      clearTimeout(peek);
      fetchVms();
    }
  };

  // Errors propagate to the form, which shows them and stays open.
  const handleCreate = async (request: CreateVmRequest) => {
    setError(null);
    await api.createVm({ ...request, project: project ?? undefined });
    setShowCreateModal(false);
    fetchVms();
  };

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">
            Virtual Machines
          </h1>
          <p className="text-gray-500 mt-1">
            Cloud Hypervisor and QEMU VMs in project <span className="font-medium">{projectName(project)}</span>
          </p>
        </div>
        {canCreate && (
          <button
            className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg transition-colors"
            onClick={() => setShowCreateModal(true)}
          >
            + Create VM
          </button>
        )}
      </div>

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

      {(() => {
        // In a cluster, narrow the list to one node.
        const placed = [...new Set((vms ?? []).map((v) => v.node).filter((n): n is string => !!n))];
        if (!nodes.clustered || placed.length < 2) return null;
        return (
          <div className="mb-4 flex items-center gap-2 text-sm">
            <label htmlFor="vm-node-filter" className="text-gray-600">
              Node
            </label>
            <select id="vm-node-filter" className={`${selectClass} w-auto`} value={nodeFilter} onChange={(e) => setNodeFilter(e.target.value)}>
              <option value="">All nodes</option>
              {placed
                .map((id) => ({ id, name: vms?.find((v) => v.node === id)?.node_name ?? nodes.name(id) }))
                .sort((a, b) => a.name.localeCompare(b.name))
                .map((n) => (
                  <option key={n.id} value={n.id}>
                    {n.name}
                  </option>
                ))}
            </select>
          </div>
        );
      })()}

      {loading ? (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6">
          <LoadingCard />
          <LoadingCard />
          <LoadingCard />
        </div>
      ) : vms && vms.length > 0 ? (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6">
          {vms
            .filter((vm) => !nodeFilter || vm.node === nodeFilter)
            .map((vm) => (
              <VmCard key={vm.id} vm={vm} onAction={handleAction} />
            ))}
        </div>
      ) : vms && vms.length === 0 ? (
        <div className="text-center py-12">
          <div className="text-gray-400 mb-4">
            <svg
              className="w-16 h-16 mx-auto"
              fill="none"
              stroke="currentColor"
              viewBox="0 0 24 24"
            >
              <path
                strokeLinecap="round"
                strokeLinejoin="round"
                strokeWidth="1"
                d="M19 11H5m14 0a2 2 0 012 2v6a2 2 0 01-2 2H5a2 2 0 01-2-2v-6a2 2 0 012-2m14 0V9a2 2 0 00-2-2M5 11V9a2 2 0 012-2m0 0V5a2 2 0 012-2h6a2 2 0 012 2v2M7 7h10"
              />
            </svg>
          </div>
          <p className="text-gray-500 text-lg">No VMs found</p>
          <p className="text-gray-400 mt-1">
            Create your first VM to get started
          </p>
        </div>
      ) : null}

      {showCreateModal && (
        <Modal title="Create New VM" onClose={() => setShowCreateModal(false)}>
          <CreateVmForm
            onSubmit={handleCreate}
            onCancel={() => setShowCreateModal(false)}
          />
        </Modal>
      )}
    </div>
  );
}
