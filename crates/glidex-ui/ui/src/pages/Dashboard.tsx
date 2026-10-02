import { useCallback, useEffect, useState } from "react";
import * as api from "../api";
import type { VmResponse } from "../types";
import type { VmAction } from "../components/VmActions";
import VmCard from "../components/VmCard";
import { LoadingCard } from "../components/Loading";
import Modal from "../components/Modal";
import CreateVmForm from "../components/CreateVmForm";
import type { CreateVmRequest } from "../types";

export default function Dashboard() {
  const [vms, setVms] = useState<VmResponse[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [showCreateModal, setShowCreateModal] = useState(false);

  const fetchVms = useCallback(async () => {
    try {
      const data = await api.listVms();
      setVms(data);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load VMs");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchVms();
  }, [fetchVms]);

  const handleAction = async (vmId: string, action: VmAction) => {
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
          break;
      }
      fetchVms();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Action failed");
    }
  };

  const handleCreate = async (request: CreateVmRequest) => {
    setError(null);
    try {
      await api.createVm(request);
      setShowCreateModal(false);
      fetchVms();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to create VM");
    }
  };

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">
            Virtual Machines
          </h1>
          <p className="text-gray-500 mt-1">Manage your Cloud Hypervisor and QEMU VMs</p>
        </div>
        <button
          className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg transition-colors"
          onClick={() => setShowCreateModal(true)}
        >
          + Create VM
        </button>
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

      {loading ? (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6">
          <LoadingCard />
          <LoadingCard />
          <LoadingCard />
        </div>
      ) : vms && vms.length > 0 ? (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6">
          {vms.map((vm) => (
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
