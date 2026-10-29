// The create form's GPUs (spec/27-gpu-vms.md §3): a count from a GPU pool,
// or a node's own GPUs picked from its topology.
import { useEffect, useState } from "react";
import { getNodeTopology } from "../api";
import type { PciPoolJson } from "../gen/glidex/cluster/v1/cluster_pb";
import type { PciFunction } from "../types";

/** Vendors worth a name next to their ids. */
const VENDORS: Record<string, string> = {
  "0x10de": "NVIDIA",
  "0x1002": "AMD",
  "0x8086": "Intel",
  "0x1a03": "ASPEED",
};

/** The sysfs path `vfio_devices` takes for a device. */
export const sysfsPath = (address: string) => `/sys/bus/pci/devices/${address}`;

export interface PoolAsk {
  count: number;
  sameSwitch: boolean;
}

interface GpuPickerProps {
  /** The GPU pools the project may use. */
  pools: PciPoolJson[];
  asks: Record<string, PoolAsk>;
  onAsk: (pool: string, change: Partial<PoolAsk>) => void;
  /** Pool sizing applies (the vCPU and memory fields weren't set by hand). */
  sizesVm: boolean;
  /** The node whose GPUs are listed; null: none chosen yet (a cluster). */
  node: string | null;
  /** Addresses picked on that node. */
  picked: string[];
  onPick: (addresses: string[]) => void;
}

export default function GpuPicker({ pools, asks, onAsk, sizesVm, node, picked, onPick }: GpuPickerProps) {
  // undefined: loading; null: can't be read (no host role, an old node).
  const [gpus, setGpus] = useState<PciFunction[] | null | undefined>(undefined);
  useEffect(() => {
    if (!node) {
      setGpus(null);
      return;
    }
    let stop = false;
    setGpus(undefined);
    getNodeTopology(node)
      .then((t) => {
        if (stop) return;
        // Display-class functions a VM could be given; never an NVSwitch.
        setGpus(
          (t.devices ?? []).filter((d) => d.assignable && !d.fabric && (d.class_id ?? "").replace(/^0x/, "").startsWith("03")),
        );
      })
      .catch(() => !stop && setGpus(null));
    return () => {
      stop = true;
    };
  }, [node]);
  // GPUs picked on another node, or gone, aren't asked for.
  useEffect(() => {
    if (gpus === undefined) return;
    const here = new Set((gpus ?? []).filter((g) => g.ready).map((g) => g.address));
    if (picked.some((a) => !here.has(a))) onPick(picked.filter((a) => here.has(a)));
  }, [gpus]);

  const listed = gpus && gpus.length > 0;
  if (pools.length === 0 && !listed && node !== null) return null;
  const fromPools = pools.reduce((n, p) => n + (asks[p.name ?? ""]?.count ?? 0), 0);
  const total = fromPools + picked.length;
  const toggle = (address: string, on: boolean) =>
    onPick(on ? [...picked, address] : picked.filter((a) => a !== address));

  return (
    <div>
      <label className="block text-sm font-medium text-gray-700">GPUs (optional)</label>
      {pools.length > 0 && (
        <div className="mt-1 space-y-2">
          <p className="text-xs text-gray-500">From a pool: the scheduler picks which.</p>
          {pools.map((p) => {
            const name = p.name ?? "";
            const ask = asks[name] ?? { count: 0, sameSwitch: false };
            return (
              <div key={name} className="flex flex-wrap items-center gap-3 text-sm">
                <span className="font-mono w-32 truncate" title={name}>{name}</span>
                <span className="text-xs text-gray-500">{p.free ?? 0} free</span>
                {p.from && (
                  <span className="text-xs text-gray-500">
                    tenant pool of {p.from}, {p.used ?? 0}/{p.max ?? 0} held
                  </span>
                )}
                {p.per_device_vcpus && p.per_device_memory_mib && (
                  <span className="text-xs text-gray-500">
                    {p.per_device_vcpus} vCPUs and {Math.round(p.per_device_memory_mib / 1024)} GiB each
                  </span>
                )}
                <input
                  type="number"
                  min="0"
                  aria-label={`GPUs from ${name}`}
                  className="w-20 px-2 py-1 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent"
                  value={ask.count}
                  onChange={(e) => onAsk(name, { count: Math.max(0, Number(e.target.value)) })}
                />
                {ask.count > 1 && (
                  <label className="flex items-center gap-1 text-xs text-gray-600">
                    <input type="checkbox" checked={ask.sameSwitch} onChange={(e) => onAsk(name, { sameSwitch: e.target.checked })} />
                    Under one PCIe switch
                  </label>
                )}
              </div>
            );
          })}
        </div>
      )}

      {node === null ? (
        <p className="mt-1 text-xs text-gray-500">Choose a node under Placement to pick its GPUs.</p>
      ) : gpus === undefined ? (
        <p className="mt-1 text-xs text-gray-500">Looking for this node&apos;s GPUs…</p>
      ) : (
        listed && (
          <div className="mt-2">
            {pools.length > 0 && <p className="text-xs text-gray-500">Or pick this node&apos;s GPUs:</p>}
            <ul className="mt-1 space-y-1">
              {gpus.map((g) => {
                const vendor = VENDORS[g.vendor_id ?? ""];
                const ids = `${(g.vendor_id ?? "").replace(/^0x/, "")}:${(g.device_id ?? "").replace(/^0x/, "")}`;
                return (
                  <li key={g.address} className="text-sm">
                    <label className={`flex flex-wrap items-center gap-2 ${g.ready ? "cursor-pointer" : "text-gray-400"}`}>
                      <input
                        type="checkbox"
                        aria-label={`GPU ${g.address}`}
                        disabled={!g.ready}
                        checked={picked.includes(g.address)}
                        onChange={(e) => toggle(g.address, e.target.checked)}
                      />
                      <span className="font-mono">{g.address}</span>
                      <span>{vendor ?? "GPU"}</span>
                      <span className="font-mono text-xs text-gray-500">{ids}</span>
                      {g.numa_node !== undefined && g.numa_node >= 0 && (
                        <span className="text-xs text-gray-500">NUMA {g.numa_node}</span>
                      )}
                      {g.ready ? (
                        <span className="text-xs text-green-700">ready</span>
                      ) : (
                        <span className="text-xs">{(g.not_ready ?? []).join("; ") || "not ready"}</span>
                      )}
                    </label>
                  </li>
                );
              })}
            </ul>
            {gpus.some((g) => !g.ready) && (
              <p className="mt-1 text-xs text-gray-500">
                A GPU must be bound to vfio-pci to be picked: an operator prepares it on the node&apos;s page
                (Topology, Prepare).
              </p>
            )}
          </div>
        )
      )}

      {total > 0 && (
        <p className="mt-1 text-xs text-gray-600">
          {total} GPU{total === 1 ? "" : "s"}
          {fromPools > 0 && sizesVm ? "; the vCPU and memory fields follow the pool's sizing" : ""}.
        </p>
      )}
    </div>
  );
}
