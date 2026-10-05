import { useEffect, useState } from "react";
import * as api from "../api";
import type { GroupKey, Named, RateResponse, RateRow, UsageResponse, UsageRow } from "../types";
import { LoadingCard } from "../components/Loading";
import { ErrorBanner, PageHeader, errorMessage, selectClass } from "../components/ui";
import { useSession } from "../session";

type Tab = "usage" | "bandwidth" | "disk-io";

const TABS: { id: Tab; label: string }[] = [
  { id: "usage", label: "Usage" },
  { id: "bandwidth", label: "Bandwidth" },
  { id: "disk-io", label: "Disk I/O" },
];

const GROUPS: Record<Tab, GroupKey[]> = {
  usage: ["project", "vm", "disk", "network"],
  bandwidth: ["project", "vm", "nic", "network"],
  "disk-io": ["project", "vm", "disk"],
};

/** The meters shown on the Usage tab, in billing order. */
const USAGE_METERS: [string, string][] = [
  ["cpu.used", "CPU used"],
  ["cpu.alloc", "vCPUs allocated"],
  ["mem.alloc", "Memory allocated"],
  ["disk.alloc", "Disk provisioned"],
  ["disk.stored", "Disk stored"],
  ["net.bytes", "Network"],
  ["net.ext_bytes", "of which internet"],
  ["bridge.bytes", "Network traffic"],
  ["bridge.ext_bytes", "of which internet"],
];

const th = "text-left px-3 py-2 font-medium text-gray-600 whitespace-nowrap";
const td = "px-3 py-1.5 whitespace-nowrap";
const tdNum = `${td} text-right tabular-nums`;

function thisMonth(): string {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}`;
}

function who(r: { [k in GroupKey]?: Named }, group: GroupKey): string {
  return r[group]?.name ?? (group === "project" ? "(host)" : "—");
}

function n(v: number | null | undefined, digits = 1): string {
  return v === null || v === undefined ? "—" : v.toLocaleString(undefined, { maximumFractionDigits: digits });
}

function Flags({ flags }: { flags?: string[] }) {
  if (!flags?.length) return null;
  const shown = flags.filter((f) => f !== "provisional");
  return shown.length ? <span className="text-xs text-amber-700 ml-1">({shown.join(", ")})</span> : null;
}

function UsageTable({ data, group }: { data: UsageResponse; group: GroupKey }) {
  const cols = USAGE_METERS.filter(([m]) => data.rows.some((r) => r.meters[m]));
  return (
    <table className="w-full text-sm">
      <thead className="bg-gray-50">
        <tr>
          <th className={th}>{group}</th>
          {cols.map(([m, label]) => (
            <th key={m} className={`${th} text-right`}>
              {label}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {data.rows.map((r: UsageRow, i) => (
          <tr key={i} className="border-t border-gray-100">
            <td className={td}>
              {who(r, group)}
              <Flags flags={r.flags} />
            </td>
            {cols.map(([m]) => (
              <td key={m} className={tdNum}>
                {r.meters[m] ? (
                  <>
                    {n(r.meters[m].value, 2)} <span className="text-xs text-gray-500">{r.meters[m].unit}</span>
                  </>
                ) : (
                  "—"
                )}
              </td>
            ))}
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function BandwidthTable({ data, group }: { data: RateResponse; group: GroupKey }) {
  const net = group === "network";
  return (
    <table className="w-full text-sm">
      <thead className="bg-gray-50">
        <tr>
          <th className={th}>{group}</th>
          <th className={`${th} text-right`}>Average{net ? "" : " in / out"}</th>
          <th className={`${th} text-right`}>30-s peak{net ? "" : " in / out"}</th>
          <th className={`${th} text-right`}>p95{net ? "" : " in / out"}</th>
          <th className={`${th} text-right`}>Billable p95</th>
          <th className={`${th} text-right`}>Internet p95</th>
          <th className={`${th} text-right`}>5-min slots</th>
        </tr>
      </thead>
      <tbody>
        {data.rows.map((r: RateRow, i) => (
          <tr key={i} className="border-t border-gray-100">
            <td className={td}>{who(r, group)}</td>
            <td className={tdNum}>{net ? n(r.avg?.mbps) : `${n(r.avg?.rx_mbps)} / ${n(r.avg?.tx_mbps)}`}</td>
            <td className={tdNum}>{net ? n(r.peak?.mbps) : `${n(r.peak?.rx_mbps)} / ${n(r.peak?.tx_mbps)}`}</td>
            <td className={tdNum}>{net ? n(r.p95?.billable_mbps) : `${n(r.p95?.rx_mbps)} / ${n(r.p95?.tx_mbps)}`}</td>
            <td className={`${tdNum} font-medium`}>{n(r.p95?.billable_mbps)}</td>
            <td className={tdNum}>{n(r.p95?.ext_billable_mbps)}</td>
            <td className={tdNum}>{r.slots ? r.slots.counted : "—"}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function DiskIoTable({ data, group }: { data: RateResponse; group: GroupKey }) {
  return (
    <table className="w-full text-sm">
      <thead className="bg-gray-50">
        <tr>
          <th className={th}>{group}</th>
          <th className={`${th} text-right`}>Avg IOPS read / write</th>
          <th className={`${th} text-right`}>Avg latency read / write (ms)</th>
          <th className={`${th} text-right`}>30-s peak IOPS</th>
          <th className={`${th} text-right`}>p95 IOPS read / write</th>
          <th className={`${th} text-right`}>Billable p95 IOPS</th>
          <th className={`${th} text-right`}>Billable p95 MB/s</th>
        </tr>
      </thead>
      <tbody>
        {data.rows.map((r: RateRow, i) => (
          <tr key={i} className="border-t border-gray-100">
            <td className={td}>{who(r, group)}</td>
            <td className={tdNum}>
              {n(r.avg?.read_iops)} / {n(r.avg?.write_iops)}
            </td>
            <td className={tdNum}>
              {r.latency_source === "none" ? (
                <span className="text-xs text-gray-500">not available</span>
              ) : (
                `${n(r.avg?.read_latency_ms, 2)} / ${n(r.avg?.write_latency_ms, 2)}`
              )}
            </td>
            <td className={tdNum}>{n(r.peak?.iops)}</td>
            <td className={tdNum}>
              {n(r.p95?.read_iops)} / {n(r.p95?.write_iops)}
            </td>
            <td className={`${tdNum} font-medium`}>{n(r.p95?.billable_iops)}</td>
            <td className={tdNum}>{n(r.p95?.billable_mbps, 2)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** Usage for a billing month (spec/metering.md §12): totals, bandwidth
 * and disk I/O, grouped, with a CSV download of the same view. */
export default function Usage() {
  const { host, projects, project: selected, projectName } = useSession();
  const [tab, setTab] = useState<Tab>("usage");
  const [project, setProject] = useState<string>(host.readUsage ? "" : (selected ?? ""));
  const [month, setMonth] = useState(thisMonth());
  const [group, setGroup] = useState<GroupKey>("project");
  const [usage, setUsage] = useState<UsageResponse | null>(null);
  const [rates, setRates] = useState<RateResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  const g = GROUPS[tab].includes(group) ? group : "project";
  const q: api.UsageQuery = { project: project || undefined, month, group_by: g };

  useEffect(() => {
    let live = true;
    setError(null);
    setUsage(null);
    setRates(null);
    const load =
      tab === "usage"
        ? api.getUsage({ ...q, granularity: "month" }).then((u) => live && setUsage(u))
        : (tab === "bandwidth" ? api.getBandwidth(q) : api.getDiskIo(q)).then((r) => live && setRates(r));
    load.catch((e) => live && setError(errorMessage(e)));
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab, project, month, g]);

  const data = tab === "usage" ? usage : rates;
  const status =
    tab === "usage"
      ? usage &&
        `${Date.parse(usage.complete_through) > 0 ? `Complete through ${new Date(usage.complete_through).toLocaleString()}` : "No hour closed yet"} · ${usage.timezone}`
      : rates && `${rates.final ? "Final" : "Month to date"} · ${rates.timezone}${rates.from_final_figures ? " · stored monthly figures" : ""}`;

  return (
    <div>
      <PageHeader title="Usage" subtitle="What VMs, disks and networks used, per billing month" />
      <div className="flex flex-wrap items-end gap-3 mb-4" role="group" aria-label="Filters">
        <div className="flex rounded-lg border border-gray-300 overflow-hidden" role="tablist">
          {TABS.map((t) => (
            <button
              key={t.id}
              role="tab"
              aria-selected={tab === t.id}
              onClick={() => setTab(t.id)}
              className={`px-3 py-1.5 text-sm ${tab === t.id ? "bg-sky-600 text-white" : "bg-white text-gray-700 hover:bg-gray-50"}`}
            >
              {t.label}
            </button>
          ))}
        </div>
        <label className="text-sm text-gray-600">
          Month
          <input type="month" value={month} onChange={(e) => setMonth(e.target.value)} className={`${selectClass} block`} />
        </label>
        <label className="text-sm text-gray-600">
          Project
          <select value={project} onChange={(e) => setProject(e.target.value)} className={`${selectClass} block`}>
            {host.readUsage && <option value="">All projects</option>}
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {projectName(p.id)}
              </option>
            ))}
          </select>
        </label>
        <label className="text-sm text-gray-600">
          Group by
          <select value={g} onChange={(e) => setGroup(e.target.value as GroupKey)} className={`${selectClass} block`}>
            {GROUPS[tab].map((k) => (
              <option key={k} value={k}>
                {k}
              </option>
            ))}
          </select>
        </label>
        <a
          href={api.usageCsvUrl(tab, { ...q, ...(tab === "usage" ? { granularity: "month" as const } : {}) })}
          download={`glidex-${tab}-${month}.csv`}
          className="ml-auto text-sm text-sky-700 hover:underline py-2"
        >
          Download CSV
        </a>
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {!data && !error ? (
        <LoadingCard />
      ) : data ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-x-auto">
          <div className="px-3 py-2 text-xs text-gray-500 border-b border-gray-100">{status}</div>
          {data.rows.length === 0 ? (
            <p className="p-6 text-sm text-gray-500">Nothing recorded for this month.</p>
          ) : tab === "usage" ? (
            <UsageTable data={usage!} group={g} />
          ) : tab === "bandwidth" ? (
            <BandwidthTable data={rates!} group={g} />
          ) : (
            <DiskIoTable data={rates!} group={g} />
          )}
          {tab !== "usage" && (
            <p className="px-3 py-2 text-xs text-gray-500 border-t border-gray-100">
              Peaks are the busiest 30 seconds; p95 is the 95th percentile of 5-minute averages over the slots the{" "}
              {g} was running. Billable: the larger of in and out for bandwidth, read + write for disks.
            </p>
          )}
        </div>
      ) : null}
    </div>
  );
}
