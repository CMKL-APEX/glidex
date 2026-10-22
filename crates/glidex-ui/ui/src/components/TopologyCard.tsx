import { useEffect, useState } from "react";
import * as api from "../api";
import type { HostTopology, PciFunction } from "../types";
import { cpuList, num, pciClassWord } from "../types";
import { Badge, Card, ErrorBanner, errorMessage } from "./ui";
import { LoadingCard } from "./Loading";

/** What the viewer may do with a node's devices, and how. */
interface DeviceOps {
  manage: boolean;
  cordoned: Set<string>;
  run: (op: "prepare" | "release" | "cordon" | "uncordon", address: string) => void;
  /** Set a PF's SR-IOV virtual functions (§10.1). */
  sriov: (address: string, current: number, total: number) => void;
  busy: string | null;
}

/** One device a VM could be given: what it is, where it sits, whether it
 * is ready for passthrough (and if not, why), its errors, and what a host
 * administrator can do with it (spec/host-topology.md §5.3–5.4). */
function Device({ d, ops }: { d: PciFunction; ops: DeviceOps }) {
  const cordoned = ops.cordoned.has(d.address);
  const errors = num(d.aer?.nonfatal) + num(d.aer?.fatal);
  const button = (op: "prepare" | "release" | "cordon" | "uncordon", label: string) => (
    <button className="text-xs text-sky-700 hover:underline disabled:opacity-50" disabled={ops.busy !== null} onClick={() => ops.run(op, d.address)}>
      {ops.busy === `${op}:${d.address}` ? "…" : label}
    </button>
  );
  return (
    <li className="py-1 border-t border-gray-100 first:border-t-0">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono">{d.address}</span>
        <span className="text-gray-600">{pciClassWord(d.class_id)}</span>
        <span className="font-mono text-xs text-gray-400">
          {(d.vendor_id ?? "").replace(/^0x/, "")}:{(d.device_id ?? "").replace(/^0x/, "")}
        </span>
        {d.ready ? <Badge kind="ok">ready</Badge> : <Badge kind="warn">not ready</Badge>}
        {cordoned && <Badge kind="denied">cordoned</Badge>}
        {errors > 0 && <Badge kind="denied">{errors} PCIe errors</Badge>}
        {ops.manage && (
          <span className="ml-auto flex gap-2">
            {d.driver !== "vfio-pci" ? button("prepare", "Prepare") : button("release", "Release")}
            {cordoned ? button("uncordon", "Uncordon") : button("cordon", "Cordon")}
            {d.sriov && (
              <button
                className="text-xs text-sky-700 hover:underline disabled:opacity-50"
                disabled={ops.busy !== null}
                onClick={() => ops.sriov(d.address, d.sriov?.enabled_vfs ?? 0, d.sriov?.total_vfs ?? 0)}
              >
                {ops.busy === `sriov:${d.address}` ? "…" : "VFs…"}
              </button>
            )}
          </span>
        )}
      </div>
      <div className="text-xs text-gray-500">
        {d.driver ?? "no driver"}
        {d.iommu_group && ` · IOMMU group ${d.iommu_group}`}
        {d.switch && ` · switch ${d.switch}`}
        {d.link && ` · ${d.link.current_speed} x${d.link.current_width}`}
        {d.sriov && ` · VFs ${d.sriov.enabled_vfs ?? 0}/${d.sriov.total_vfs ?? 0}`}
        {d.sriov && (d.sriov.enabled_vfs ?? 0) > 0 && ` (${d.sriov.ready_vfs ?? 0} ready${d.sriov.vf_driver ? `, on ${d.sriov.vf_driver}` : ""})`}
      </div>
      {!d.ready && (d.not_ready ?? []).length > 0 && <div className="text-xs text-amber-700">{(d.not_ready ?? []).join("; ")}</div>}
    </li>
  );
}

/** A node's NUMA nodes with their CPUs, memory, hugepages and devices
 * (spec/host-topology.md §4.3), read on the node itself. */
export default function TopologyCard({ node, cordoned, manage, onChanged }: { node: string; cordoned: string[]; manage: boolean; onChanged: () => void }) {
  const [t, setT] = useState<HostTopology | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    let live = true;
    api
      .getNodeTopology(node)
      .then((v) => live && setT(v))
      .catch((e) => live && setError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [node, tick]);
  const ops: DeviceOps = {
    sriov: () => {},
    manage,
    cordoned: new Set(cordoned),
    busy,
    run: async (op, address) => {
      if (op === "prepare" && !confirm(`Bind ${address} and the rest of its IOMMU group to vfio-pci? The host's driver lets go of them.`)) return;
      setBusy(`${op}:${address}`);
      setError(null);
      try {
        const call = { prepare: api.prepareDevice, release: api.releaseDevice, cordon: api.cordonDevice, uncordon: api.uncordonDevice }[op];
        await call(node, address);
        setTick((x) => x + 1);
        onChanged();
      } catch (e) {
        setError(errorMessage(e));
      } finally {
        setBusy(null);
      }
    },
  };
  ops.sriov = async (address, current, total) => {
    const answer = prompt(
      `Virtual functions for ${address} (0-${total}, now ${current}). Changing the number removes every VF it has; one a VM holds refuses it.`,
      String(current),
    );
    if (answer === null) return;
    const vfs = Number(answer);
    if (!Number.isInteger(vfs) || vfs < 0 || vfs > total) {
      setError(`${answer}: a number from 0 to ${total}`);
      return;
    }
    setBusy(`sriov:${address}`);
    setError(null);
    try {
      await api.setSriovVfs(node, address, vfs);
      setTick((x) => x + 1);
      onChanged();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  if (error && !t) {
    return (
      <Card title="Topology">
        <ErrorBanner error={error} />
      </Card>
    );
  }
  if (!t) return <LoadingCard />;
  const numa = t.summary?.numa_nodes ?? [];
  const devices = (t.devices ?? []).filter((d) => d.assignable);
  const reserved = Object.keys(t.summary?.reserved_cpus ?? {}).map(Number);
  const loose = devices.filter((d) => d.numa_node === undefined);
  const fabric = (t.devices ?? []).filter((d) => d.fabric);
  return (
    <Card title={`Topology (${numa.length} NUMA ${numa.length === 1 ? "node" : "nodes"})`}>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {(t.warnings ?? []).length > 0 && (
        <ul className="mb-3 p-3 bg-amber-50 border border-amber-200 rounded-lg text-sm text-amber-800 space-y-0.5">
          {(t.warnings ?? []).map((w, i) => (
            <li key={i}>
              <span className="font-medium">{w.kind}</span>: {w.detail}
            </li>
          ))}
        </ul>
      )}
      <div className="grid grid-cols-1 md:grid-cols-2 xl:grid-cols-4 gap-4">
        {numa.map((n) => {
          // Proto JSON leaves a zero out: node 0 comes without an id.
          const id = n.id ?? 0;
          const cpus = (n.cores ?? []).flatMap((c) => c.threads ?? []);
          const mine = devices.filter((d) => d.numa_node === id);
          const hp = Object.entries(n.hugepages ?? {});
          return (
            <div key={id} className="border border-gray-200 rounded-lg p-3">
              <div className="font-medium text-gray-900">NUMA node {id}</div>
              <dl className="mt-1 grid grid-cols-[auto_1fr] gap-x-3 text-xs">
                <dt className="text-gray-500">CPUs</dt>
                <dd className="font-mono break-all">
                  {cpuList(cpus)} <span className="font-sans text-gray-400">({(n.cores ?? []).length} cores)</span>
                </dd>
                <dt className="text-gray-500">Memory</dt>
                <dd>{num(n.memory_mib)} MiB</dd>
                {hp.length > 0 && (
                  <>
                    <dt className="text-gray-500">Hugepages</dt>
                    <dd>{hp.map(([kib, pages]) => `${num(pages)} × ${kib} KiB`).join(", ")}</dd>
                  </>
                )}
              </dl>
              {mine.length > 0 ? (
                <ul className="mt-2 text-sm">
                  {mine.map((d) => (
                    <Device key={d.address} d={d} ops={ops} />
                  ))}
                </ul>
              ) : (
                <p className="mt-2 text-xs text-gray-400">No devices a VM could be given.</p>
              )}
            </div>
          );
        })}
      </div>
      {loose.length > 0 && (
        <div className="mt-4">
          <div className="text-sm font-medium text-gray-900">Devices without a NUMA node</div>
          <ul className="text-sm">
            {loose.map((d) => (
              <Device key={d.address} d={d} ops={ops} />
            ))}
          </ul>
        </div>
      )}
      {fabric.length > 0 && (
        <div className="mt-4">
          <div className="text-sm font-medium text-gray-900">
            NVLink fabric ({fabric.length} NVSwitch{fabric.length === 1 ? "" : "es"})
          </div>
          <p className="text-xs text-gray-500">
            Fabric Manager owns these, on the host or in a service VM; they are never given to a VM.
          </p>
          <ul className="text-sm">
            {fabric.map((d) => (
              <li key={d.address} className="py-1 border-t border-gray-100 first:border-t-0">
                <span className="font-mono">{d.address}</span>{" "}
                <span className="text-gray-600">NVSwitch</span>{" "}
                <span className="font-mono text-xs text-gray-400">
                  {(d.vendor_id ?? "").replace(/^0x/, "")}:{(d.device_id ?? "").replace(/^0x/, "")}
                </span>
                <div className="text-xs text-gray-500">
                  {d.driver ?? "no driver"}
                  {d.iommu_group && ` · IOMMU group ${d.iommu_group}`}
                  {d.numa_node !== undefined && ` · NUMA node ${d.numa_node}`}
                </div>
              </li>
            ))}
          </ul>
        </div>
      )}
      {reserved.length > 0 && (
        <p className="mt-3 text-xs text-gray-500">
          Reserved for the host and OVS-DPDK: CPUs <span className="font-mono">{cpuList(reserved)}</span>
          {t.summary?.reserved_by_config ? " (from the configuration)" : ""}.
        </p>
      )}
    </Card>
  );
}
