import { useEffect, useState } from "react";
import * as api from "../api";
import type { SeriesResponse, VmStats } from "../types";
import { TimeChart, SERIES_COLORS, type ChartPoint } from "./TimeChart";

/** Live rates come from the meter's last round (every 30 s by default). */
const POLL_MS = 30_000;

function points(s: SeriesResponse | null, keys: string[]): ChartPoint[] {
  return (s?.points ?? []).map((p) => ({
    t: Date.parse(p.slot as string),
    values: Object.fromEntries(keys.map((k) => [k, typeof p[k] === "number" ? (p[k] as number) : undefined])),
  }));
}

function num(v: unknown): number | null {
  return typeof v === "number" ? v : null;
}

function Tile({ label, value, unit }: { label: string; value: number | null | undefined; unit: string }) {
  return (
    <div className="px-3 py-2 rounded-lg border border-gray-200 min-w-28">
      <div className="text-xs text-gray-500">{label}</div>
      <div className="text-lg font-semibold text-gray-900 tabular-nums">
        {value === null || value === undefined ? "—" : value.toLocaleString(undefined, { maximumFractionDigits: 1 })}
        <span className="text-xs font-normal text-gray-500 ml-1">{unit}</span>
      </div>
    </div>
  );
}

/** VM detail "Usage" card (spec/metering.md §12): current rates and the
 * last 24 h of CPU, memory, bandwidth and disk I/O with their 95th
 * percentiles. */
export default function VmUsage({ vmId, hypervisor }: { vmId: string; hypervisor: string }) {
  const [stats, setStats] = useState<VmStats | null>(null);
  const [bw, setBw] = useState<SeriesResponse | null>(null);
  const [io, setIo] = useState<SeriesResponse | null>(null);
  const [cm, setCm] = useState<SeriesResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    const load = async () => {
      try {
        const [s, b, i, c] = await Promise.all([
          api.getVmStats(vmId),
          api.getVmBandwidth(vmId),
          api.getVmIo(vmId),
          api.getVmCompute(vmId),
        ]);
        if (!live) return;
        setStats(s);
        setBw(b);
        setIo(i);
        setCm(c);
        setError(null);
      } catch (e) {
        if (live) setError(e instanceof api.ApiRequestError && e.status === 503 ? "Metering is not running." : String(e));
      }
    };
    load();
    const id = setInterval(load, POLL_MS);
    return () => {
      live = false;
      clearInterval(id);
    };
  }, [vmId]);

  if (error) return <p className="text-sm text-gray-500">{error}</p>;

  const nicSum = (k: string) => stats?.nics.reduce((a, n) => a + (n.values[k] ?? 0), 0) ?? null;
  const diskSum = (k: string) => stats?.disks.reduce((a, d) => a + (d.values[k] ?? 0), 0) ?? null;
  const bwP95 = num(bw?.p95?.billable_mbps);
  const ioP95 = num(io?.p95?.billable_iops);
  const cpuP95 = num(cm?.p95?.cpu_percent);
  const memP95 = num(cm?.p95?.mem_mib);
  const ioPoints = points(io, ["read_iops", "write_iops"]);
  const latPoints = points(io, ["read_latency_ms", "write_latency_ms"]);
  const hasLatency = latPoints.some((p) => p.values.read_latency_ms !== undefined || p.values.write_latency_ms !== undefined);

  return (
    <div>
      <div className="flex flex-wrap gap-2 mb-4" aria-live="polite">
        <Tile label="CPU (per vCPU)" value={stats?.vm?.cpu_percent} unit="%" />
        <Tile label="Memory (working set)" value={stats?.vm?.mem_used_mib} unit="MiB" />
        <Tile label="Network in" value={nicSum("rx_mbps")} unit="Mbps" />
        <Tile label="Network out" value={nicSum("tx_mbps")} unit="Mbps" />
        <Tile label="Disk read" value={diskSum("read_iops")} unit="IOPS" />
        <Tile label="Disk write" value={diskSum("write_iops")} unit="IOPS" />
      </div>
      <p className="text-xs text-gray-500 mb-3">
        {stats?.sampled_at
          ? `Current rates: average over the last ${stats.resolution_secs} s, sampled ${new Date(stats.sampled_at).toLocaleTimeString()}.`
          : "No current sample (the VM isn't running, or metering hasn't sampled it yet)."}{" "}
        Charts: 5-minute averages over the last 24 h; the dashed line is the 95th percentile.
      </p>
      <TimeChart
        title="CPU (of allocated vCPUs)"
        unit="%"
        series={[{ key: "cpu_percent", label: "CPU", color: SERIES_COLORS[0] }]}
        points={points(cm, ["cpu_percent"])}
        reference={cpuP95 !== null ? { value: cpuP95, label: `p95 ${cpuP95.toFixed(1)} %` } : null}
      />
      <TimeChart
        title="Memory (working set)"
        unit="MiB"
        series={[{ key: "mem_mib", label: "Used", color: SERIES_COLORS[0] }]}
        points={points(cm, ["mem_mib"])}
        reference={memP95 !== null ? { value: memP95, label: `p95 ${Math.round(memP95)} MiB` } : null}
      />
      <TimeChart
        title="Network"
        unit="Mbps"
        series={[
          { key: "rx_mbps", label: "In", color: SERIES_COLORS[0] },
          { key: "tx_mbps", label: "Out", color: SERIES_COLORS[1] },
        ]}
        points={points(bw, ["rx_mbps", "tx_mbps"])}
        reference={bwP95 !== null ? { value: bwP95, label: `p95 ${bwP95.toFixed(1)} Mbps` } : null}
      />
      <TimeChart
        title="Disk operations"
        unit="IOPS"
        series={[
          { key: "read_iops", label: "Read", color: SERIES_COLORS[0] },
          { key: "write_iops", label: "Write", color: SERIES_COLORS[1] },
        ]}
        points={ioPoints}
        reference={ioP95 !== null ? { value: ioP95, label: `p95 ${Math.round(ioP95)} IOPS` } : null}
      />
      {hasLatency ? (
        <TimeChart
          title="Disk latency (average per operation)"
          unit="ms"
          height={120}
          series={[
            { key: "read_latency_ms", label: "Read", color: SERIES_COLORS[0] },
            { key: "write_latency_ms", label: "Write", color: SERIES_COLORS[1] },
          ]}
          points={latPoints}
        />
      ) : (
        ioPoints.length > 0 && (
          <p className="text-xs text-gray-500">
            Disk latency not available
            {hypervisor === "cloudhypervisor" ? " (Cloud Hypervisor doesn't report a usable latency counter)" : ""}.
          </p>
        )
      )}
    </div>
  );
}
