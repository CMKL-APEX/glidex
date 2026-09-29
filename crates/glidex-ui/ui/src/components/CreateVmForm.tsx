import { useState, type FormEvent } from "react";
import type { CreateVmRequest, HypervisorType } from "../types";
import { HYPERVISOR_LABELS } from "../types";

interface CreateVmFormProps {
  onSubmit: (request: CreateVmRequest) => void;
  onCancel: () => void;
}

export default function CreateVmForm({ onSubmit, onCancel }: CreateVmFormProps) {
  const [name, setName] = useState("");
  const [vcpuCount, setVcpuCount] = useState(1);
  const [memSizeMib, setMemSizeMib] = useState(512);
  const [hypervisor, setHypervisor] = useState<HypervisorType>("qemu");
  const [kernelPath, setKernelPath] = useState("");
  const [rootfsPath, setRootfsPath] = useState("");
  const [kernelArgs, setKernelArgs] = useState("");
  const [vfioDevices, setVfioDevices] = useState("");
  const [submitting, setSubmitting] = useState(false);

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault();
    setSubmitting(true);

    const devices = vfioDevices
      .split(",")
      .map((d) => d.trim())
      .filter((d) => d.length > 0);

    onSubmit({
      name,
      vcpu_count: vcpuCount,
      mem_size_mib: memSizeMib,
      kernel_image_path: kernelPath,
      rootfs_path: rootfsPath,
      hypervisor,
      kernel_args: kernelArgs || undefined,
      vfio_devices: devices.length > 0 ? devices : undefined,
    });
  };

  return (
    <form onSubmit={handleSubmit} className="space-y-4">
      <div>
        <label className="block text-sm font-medium text-gray-700">
          VM Name
        </label>
        <input
          type="text"
          className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
          placeholder="my-vm"
          required
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-700">
          Hypervisor Backend
        </label>
        <select
          className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent bg-white"
          value={hypervisor}
          onChange={(e) => setHypervisor(e.target.value as HypervisorType)}
        >
          {(Object.entries(HYPERVISOR_LABELS) as [HypervisorType, string][]).map(
            ([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ),
          )}
        </select>
      </div>

      <div className="grid grid-cols-2 gap-4">
        <div>
          <label className="block text-sm font-medium text-gray-700">
            vCPU Count
          </label>
          <input
            type="number"
            className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
            min="1"
            max="32"
            value={vcpuCount}
            onChange={(e) => setVcpuCount(Number(e.target.value))}
          />
        </div>
        <div>
          <label className="block text-sm font-medium text-gray-700">
            Memory (MiB)
          </label>
          <input
            type="number"
            className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
            min="128"
            max="32768"
            value={memSizeMib}
            onChange={(e) => setMemSizeMib(Number(e.target.value))}
          />
        </div>
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-700">
          Kernel Image Path
        </label>
        <input
          type="text"
          className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
          placeholder="/path/to/vmlinux"
          required
          value={kernelPath}
          onChange={(e) => setKernelPath(e.target.value)}
        />
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-700">
          Root Filesystem Path
        </label>
        <input
          type="text"
          className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
          placeholder="/path/to/rootfs.ext4"
          required
          value={rootfsPath}
          onChange={(e) => setRootfsPath(e.target.value)}
        />
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-700">
          Kernel Arguments (optional)
        </label>
        <input
          type="text"
          className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
          placeholder="root=/dev/vda reboot=k panic=1"
          value={kernelArgs}
          onChange={(e) => setKernelArgs(e.target.value)}
        />
      </div>

      <div>
        <label className="block text-sm font-medium text-gray-700">
          VFIO PCI Devices (optional)
        </label>
        <input
          type="text"
          className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
          placeholder="/sys/bus/pci/devices/0000:41:00.0"
          value={vfioDevices}
          onChange={(e) => setVfioDevices(e.target.value)}
        />
        <p className="mt-1 text-xs text-gray-500">
          Comma-separated VFIO device paths for GPU passthrough (Cloud
          Hypervisor only)
        </p>
      </div>

      <div className="flex justify-end space-x-3 pt-4">
        <button
          type="button"
          className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg transition-colors"
          onClick={onCancel}
        >
          Cancel
        </button>
        <button
          type="submit"
          className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg transition-colors disabled:opacity-50"
          disabled={submitting}
        >
          {submitting ? "Creating..." : "Create VM"}
        </button>
      </div>
    </form>
  );
}
