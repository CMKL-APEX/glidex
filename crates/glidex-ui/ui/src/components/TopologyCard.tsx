import { useEffect, useState } from "react";
import * as api from "../api";
import type { HostTopology, PciFunction } from "../types";
import { cpuList, num, pciClassWord } from "../types";
import { Badge, Card, ErrorBanner, errorMessage } from "./ui";
import { LoadingCard } from "./Loading";

/** One device a VM could be given: what it is, where it sits, and whether
 * it is ready for passthrough (and if not, why). */
function Device({ d }: { d: PciFunction }) {
  return (
    <li className="py-1 border-t border-gray-100 first:border-t-0">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono">{d.address}</span>
        <span className="text-gray-600">{pciClassWord(d.class_id)}</span>
        <span className="font-mono text-xs text-gray-400">
          {(d.vendor_id ?? "").replace(/^0x/, "")}:{(d.device_id ?? "").replace(/^0x/, "")}
        </span>
        {d.ready ? <Badge kind="ok">ready</Badge> : <Badge kind="warn">not ready</Badge>}
      </div>
      <div className="text-xs text-gray-500">
        {d.driver ?? "no driver"}
        {d.iommu_group && ` · IOMMU group ${d.iommu_group}`}
        {d.switch && ` · switch ${d.switch}`}
        {d.link && ` · ${d.link.current_speed} x${d.link.current_width}`}
        {d.sriov && ` · VFs ${d.sriov.enabled_vfs ?? 0}/${d.sriov.total_vfs ?? 0}`}
      </div>
      {!d.ready && (d.not_ready ?? []).length > 0 && <div className="text-xs text-amber-700">{(d.not_ready ?? []).join("; ")}</div>}
    </li>
  );
}

/** A node's NUMA nodes with their CPUs, memory, hugepages and devices
 * (spec/host-topology.md §4.3), read on the node itself. */
export default function TopologyCard({ node }: { node: string }) {
  const [t, setT] = useState<HostTopology | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    api
      .getNodeTopology(node)
      .then((v) => live && setT(v))
      .catch((e) => live && setError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [node]);
  if (error) {
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
  return (
    <Card title={`Topology (${numa.length} NUMA ${numa.length === 1 ? "node" : "nodes"})`}>
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
          const cpus = (n.cores ?? []).flatMap((c) => c.threads ?? []);
          const mine = devices.filter((d) => d.numa_node === n.id);
          const hp = Object.entries(n.hugepages ?? {});
          return (
            <div key={n.id} className="border border-gray-200 rounded-lg p-3">
              <div className="font-medium text-gray-900">NUMA node {n.id}</div>
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
                    <Device key={d.address} d={d} />
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
              <Device key={d.address} d={d} />
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
