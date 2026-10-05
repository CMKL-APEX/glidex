import { useEffect, useMemo, useRef, useState } from "react";

/** Categorical slots 1-2 of the validated reference palette (light). */
export const SERIES_COLORS = ["#2a78d6", "#eb6834"];

const TEXT_SECONDARY = "#52514e";
const GRID = "#e7e5e4";
const SLOT_MS = 5 * 60 * 1000;
const PAD = { top: 12, right: 92, bottom: 24, left: 48 };

export interface ChartSeries {
  key: string;
  label: string;
  color: string;
}

export interface ChartPoint {
  t: number; // unix ms, slot start
  values: Record<string, number | undefined>;
}

function niceMax(v: number): number {
  if (v <= 0) return 1;
  const mag = 10 ** Math.floor(Math.log10(v));
  const n = v / mag;
  return (n <= 1 ? 1 : n <= 2 ? 2 : n <= 5 ? 5 : 10) * mag;
}

function fmt(v: number): string {
  if (v >= 100) return v.toFixed(0);
  if (v >= 10) return v.toFixed(1);
  return v.toFixed(2);
}

function timeLabel(t: number, spanMs: number): string {
  const d = new Date(t);
  return spanMs > 2 * 86400_000
    ? d.toLocaleDateString(undefined, { month: "short", day: "numeric" })
    : d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}

/** A 5-minute time series: one y axis, thin lines, legend + end labels,
 * an optional labelled reference line (the 95th percentile), crosshair
 * tooltip, and a table view. Lines break where slots are missing. */
export function TimeChart({
  title,
  unit,
  series,
  points,
  reference,
  height = 180,
}: {
  title: string;
  unit: string;
  series: ChartSeries[];
  points: ChartPoint[];
  reference?: { value: number; label: string } | null;
  height?: number;
}) {
  const box = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(640);
  const [hover, setHover] = useState<number | null>(null);
  const [table, setTable] = useState(false);

  useEffect(() => {
    if (!box.current) return;
    const ro = new ResizeObserver((e) => setWidth(Math.max(320, e[0].contentRect.width)));
    ro.observe(box.current);
    return () => ro.disconnect();
  }, []);

  const geo = useMemo(() => {
    const t0 = points.length ? points[0].t : 0;
    const t1 = points.length ? points[points.length - 1].t + SLOT_MS : 1;
    let max = reference?.value ?? 0;
    for (const p of points) for (const s of series) max = Math.max(max, p.values[s.key] ?? 0);
    const yMax = niceMax(max * 1.1);
    const w = width - PAD.left - PAD.right;
    const h = height - PAD.top - PAD.bottom;
    const x = (t: number) => PAD.left + ((t - t0) / Math.max(1, t1 - t0)) * w;
    const y = (v: number) => PAD.top + h - (v / yMax) * h;
    return { t0, t1, yMax, w, h, x, y };
  }, [points, series, reference, width, height]);

  // Paths broken at gaps (slots the VM wasn't running in).
  const paths = series.map((s) => {
    let d = "";
    let prev: number | null = null;
    for (const p of points) {
      const v = p.values[s.key];
      if (v === undefined) {
        prev = null;
        continue;
      }
      const cx = geo.x(p.t + SLOT_MS / 2);
      d += `${prev !== null && p.t - prev <= SLOT_MS ? "L" : "M"}${cx.toFixed(1)},${geo.y(v).toFixed(1)}`;
      prev = p.t;
    }
    return d;
  });

  // End labels, nudged apart when they would collide.
  const last = points[points.length - 1];
  const ends = series
    .map((s, i) => ({ s, i, y: last && last.values[s.key] !== undefined ? geo.y(last.values[s.key]!) : null }))
    .filter((e) => e.y !== null)
    .sort((a, b) => a.y! - b.y!);
  for (let k = 1; k < ends.length; k++) if (ends[k].y! - ends[k - 1].y! < 14) ends[k].y = ends[k - 1].y! + 14;

  const ticks = [0, 0.25, 0.5, 0.75, 1].map((f) => f * geo.yMax);
  const xTicks = points.length ? [0, 0.5, 1].map((f) => geo.t0 + f * (geo.t1 - geo.t0)) : [];
  const hp = hover !== null ? points[hover] : null;

  const onMove = (e: React.MouseEvent<SVGRectElement>) => {
    if (!points.length) return;
    const rect = e.currentTarget.getBoundingClientRect();
    const t = geo.t0 + ((e.clientX - rect.left) / rect.width) * (geo.t1 - geo.t0);
    let best = 0;
    for (let i = 1; i < points.length; i++) if (Math.abs(points[i].t - t) < Math.abs(points[best].t - t)) best = i;
    setHover(best);
  };

  return (
    <figure className="mb-4">
      <figcaption className="flex flex-wrap items-center justify-between gap-2 mb-1">
        <span className="text-sm font-medium text-gray-800">
          {title} <span className="text-gray-500 font-normal">({unit})</span>
        </span>
        <span className="flex items-center gap-3 text-xs text-gray-600">
          {series.map((s) => (
            <span key={s.key} className="flex items-center gap-1">
              <span className="inline-block w-3 h-0.5" style={{ background: s.color }} aria-hidden />
              {s.label}
            </span>
          ))}
          {reference && (
            <span className="flex items-center gap-1">
              <span className="inline-block w-3 border-t border-dashed" style={{ borderColor: TEXT_SECONDARY }} aria-hidden />
              {reference.label}
            </span>
          )}
          <button type="button" className="text-sky-700 hover:underline" onClick={() => setTable(!table)}>
            {table ? "Chart" : "Table"}
          </button>
        </span>
      </figcaption>
      {table ? (
        <div className="max-h-56 overflow-auto border border-gray-200 rounded">
          <table className="w-full text-xs">
            <thead className="bg-gray-50 text-gray-600">
              <tr>
                <th className="text-left px-2 py-1">5-minute slot</th>
                {series.map((s) => (
                  <th key={s.key} className="text-right px-2 py-1">
                    {s.label} ({unit})
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {points.map((p) => (
                <tr key={p.t} className="border-t border-gray-100">
                  <td className="px-2 py-0.5">{new Date(p.t).toLocaleString()}</td>
                  {series.map((s) => (
                    <td key={s.key} className="text-right px-2 py-0.5 tabular-nums">
                      {p.values[s.key] === undefined ? "—" : fmt(p.values[s.key]!)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <div ref={box} className="relative">
          {points.length === 0 ? (
            <div className="text-sm text-gray-500 py-8 text-center border border-dashed border-gray-200 rounded">
              No samples in this range yet.
            </div>
          ) : (
            <svg width={width} height={height} role="img" aria-label={`${title}, ${unit}`}>
              {ticks.map((v) => (
                <g key={v}>
                  <line x1={PAD.left} x2={PAD.left + geo.w} y1={geo.y(v)} y2={geo.y(v)} stroke={GRID} strokeWidth={1} />
                  <text x={PAD.left - 6} y={geo.y(v) + 4} textAnchor="end" fontSize={11} fill={TEXT_SECONDARY}>
                    {fmt(v)}
                  </text>
                </g>
              ))}
              {xTicks.map((t, i) => (
                <text
                  key={t}
                  x={geo.x(t)}
                  y={height - 6}
                  textAnchor={i === 0 ? "start" : i === xTicks.length - 1 ? "end" : "middle"}
                  fontSize={11}
                  fill={TEXT_SECONDARY}
                >
                  {timeLabel(t, geo.t1 - geo.t0)}
                </text>
              ))}
              {reference && (
                <g>
                  <line
                    x1={PAD.left}
                    x2={PAD.left + geo.w}
                    y1={geo.y(reference.value)}
                    y2={geo.y(reference.value)}
                    stroke={TEXT_SECONDARY}
                    strokeWidth={1}
                    strokeDasharray="4 3"
                  />
                  <text x={PAD.left + geo.w + 6} y={geo.y(reference.value) + 4} fontSize={11} fill={TEXT_SECONDARY}>
                    {reference.label}
                  </text>
                </g>
              )}
              {paths.map((d, i) => (
                <path key={series[i].key} d={d} fill="none" stroke={series[i].color} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" />
              ))}
              {!reference &&
                ends.map((e) => (
                  <g key={e.s.key}>
                    <rect x={PAD.left + geo.w + 6} y={e.y! - 1} width={8} height={2} fill={e.s.color} />
                    <text x={PAD.left + geo.w + 18} y={e.y! + 4} fontSize={11} fill={TEXT_SECONDARY}>
                      {e.s.label}
                    </text>
                  </g>
                ))}
              {hp && (
                <g pointerEvents="none">
                  <line
                    x1={geo.x(hp.t + SLOT_MS / 2)}
                    x2={geo.x(hp.t + SLOT_MS / 2)}
                    y1={PAD.top}
                    y2={PAD.top + geo.h}
                    stroke={TEXT_SECONDARY}
                    strokeWidth={1}
                  />
                  {series.map((s) =>
                    hp.values[s.key] === undefined ? null : (
                      <circle
                        key={s.key}
                        cx={geo.x(hp.t + SLOT_MS / 2)}
                        cy={geo.y(hp.values[s.key]!)}
                        r={4}
                        fill={s.color}
                        stroke="#fff"
                        strokeWidth={2}
                      />
                    ),
                  )}
                </g>
              )}
              <rect
                x={PAD.left}
                y={PAD.top}
                width={geo.w}
                height={geo.h}
                fill="transparent"
                onMouseMove={onMove}
                onMouseLeave={() => setHover(null)}
              />
            </svg>
          )}
          {hp && (
            <div
              className="absolute pointer-events-none bg-white border border-gray-200 shadow-sm rounded px-2 py-1 text-xs text-gray-800"
              style={{
                left: Math.min(geo.x(hp.t + SLOT_MS / 2) + 10, width - 170),
                top: PAD.top,
              }}
            >
              <div className="text-gray-500">{new Date(hp.t).toLocaleString()}</div>
              {series.map((s) => (
                <div key={s.key} className="flex items-center gap-1 tabular-nums">
                  <span className="inline-block w-2 h-2 rounded-sm" style={{ background: s.color }} aria-hidden />
                  {s.label}: {hp.values[s.key] === undefined ? "—" : `${fmt(hp.values[s.key]!)} ${unit}`}
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </figure>
  );
}
