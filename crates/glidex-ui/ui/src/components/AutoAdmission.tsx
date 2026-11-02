import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import type { TenantQuotas } from "../types";
import { QUOTA_KEYS, QUOTA_LABELS } from "../types";
import Modal from "./Modal";
import { Card, ErrorBanner, Field, dangerLink, errorMessage, inputClass, primaryButton, selectClass } from "./ui";
import { useSession } from "../session";

const MODES: { value: api.AutoAdmission["mode"]; label: string }[] = [
  { value: "AUTO_ADMISSION_MODE_SELF_SERVICE", label: "Self-service tenants, once active" },
  { value: "AUTO_ADMISSION_MODE_ALL", label: "Every new user tenant" },
];

function capText(q?: TenantQuotas): string {
  const parts = QUOTA_KEYS.filter((k) => q?.[k] !== undefined).map((k) => `${QUOTA_LABELS[k]} ${q?.[k]}`);
  for (const [pool, n] of Object.entries(q?.devices ?? {})) parts.push(`${pool} ${n}`);
  return parts.length ? parts.join(", ") : "unlimited";
}

function AutoForm({
  current,
  directories,
  onDone,
  onCancel,
}: {
  current?: api.AutoAdmission;
  directories: string[];
  onDone: (updated: number) => void;
  onCancel: () => void;
}) {
  const [directory, setDirectory] = useState(current?.directory_id ?? directories[0] ?? "local");
  const [mode, setMode] = useState<api.AutoAdmission["mode"]>(current?.mode ?? "AUTO_ADMISSION_MODE_SELF_SERVICE");
  const [follow, setFollow] = useState(current?.follow_default ?? true);
  const [cap, setCap] = useState<Record<string, string>>(() =>
    Object.fromEntries(QUOTA_KEYS.map((k) => [k, current?.cap?.[k] !== undefined ? String(current.cap[k]) : ""])),
  );
  const [pools, setPools] = useState(() =>
    Object.entries(current?.cap?.devices ?? {})
      .map(([p, n]) => `${p}=${n}`)
      .join(", "),
  );
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    const q: TenantQuotas = {};
    for (const k of QUOTA_KEYS) if (cap[k].trim() !== "") q[k] = Number(cap[k]);
    const devices: Record<string, number> = {};
    for (const part of pools.split(",").map((p) => p.trim()).filter(Boolean)) {
      const [pool, n] = part.split("=").map((x) => x.trim());
      if (!pool || !n || Number.isNaN(Number(n))) {
        setError(`'${part}': write device limits as pool=N`);
        return;
      }
      devices[pool] = Number(n);
    }
    if (Object.keys(devices).length) q.devices = devices;
    setBusy(true);
    try {
      const r = await api.setAutoAdmission(directory, { mode, cap: q, follow_default: follow });
      onDone(r.updated);
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
        <Field label="Directory" htmlFor="auto-directory">
          <select
            id="auto-directory"
            className={selectClass}
            value={directory}
            disabled={!!current}
            onChange={(e) => setDirectory(e.target.value)}
          >
            {directories.map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Admits" htmlFor="auto-mode">
          <select id="auto-mode" className={selectClass} value={mode} onChange={(e) => setMode(e.target.value as api.AutoAdmission["mode"])}>
            {MODES.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </select>
        </Field>
      </div>
      <div>
        <p className="text-sm font-medium text-gray-700">Default cap</p>
        <p className="text-xs text-gray-500">What each admitted tenant's projects together may use here; empty is unlimited.</p>
        <div className="mt-2 grid grid-cols-2 gap-3">
          {QUOTA_KEYS.map((k) => (
            <Field key={k} label={QUOTA_LABELS[k]} htmlFor={`auto-cap-${k}`}>
              <input
                id={`auto-cap-${k}`}
                className={inputClass}
                type="number"
                min={0}
                placeholder="∞"
                value={cap[k]}
                onChange={(e) => setCap({ ...cap, [k]: e.target.value })}
              />
            </Field>
          ))}
        </div>
        <div className="mt-3">
          <Field label="Devices per pool (pool=N, …)" htmlFor="auto-cap-devices">
            <input
              id="auto-cap-devices"
              className={inputClass}
              placeholder="a100=1"
              value={pools}
              onChange={(e) => setPools(e.target.value)}
            />
          </Field>
        </div>
      </div>
      <label className="flex items-start gap-2 text-sm text-gray-700">
        <input id="auto-follow" type="checkbox" className="mt-1" checked={follow} onChange={(e) => setFollow(e.target.checked)} />
        <span>Admissions follow later changes to this cap, until you set a tenant's cap by hand.</span>
      </label>
      <div className="flex justify-end gap-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm text-gray-700 hover:bg-gray-100 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primaryButton} disabled={busy}>
          {busy ? "Saving..." : "Save"}
        </button>
      </div>
    </form>
  );
}

/** Which new tenants of each directory this cluster hosts it admits by
 * itself, with what default cap (spec/19-tenancy.md D79). */
export function AutoAdmissionCard({ manage, onChanged }: { manage: boolean; onChanged: () => void }) {
  const { host } = useSession();
  const [autos, setAutos] = useState<api.AutoAdmission[] | null>(null);
  const [directories, setDirectories] = useState<string[]>(["local"]);
  const [editing, setEditing] = useState<api.AutoAdmission | "new" | null>(null);
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .listAutoAdmissions()
      .then(setAutos)
      .catch((e) => setError(errorMessage(e)));
  }, []);
  useEffect(() => {
    refresh();
    if (host.listDirectories)
      api
        .listDirectories()
        .then((ds) => setDirectories(["local", ...ds.map((d) => d.id).filter((d) => d !== "local")]))
        .catch(() => {});
  }, [refresh, host.listDirectories]);

  const free = directories.filter((d) => !autos?.some((a) => a.directory_id === d));
  return (
    <Card
      title="Automatic admission"
      actions={
        manage && free.length > 0 ? (
          <button className="text-sm text-sky-700 hover:underline" onClick={() => setEditing("new")}>
            + Add
          </button>
        ) : undefined
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {note && <p className="mb-2 text-sm text-green-700">{note}</p>}
      {autos === null ? null : autos.length === 0 ? (
        <p className="text-sm text-gray-500">Off: a new tenant waits for you to admit it.</p>
      ) : (
        <table className="w-full text-sm" data-testid="auto-admissions">
          <thead>
            <tr className="text-left text-gray-500 border-b border-gray-100">
              <th className="py-1 pr-3 font-medium">Directory</th>
              <th className="py-1 pr-3 font-medium">Admits</th>
              <th className="py-1 pr-3 font-medium">Default cap</th>
              {manage && <th />}
            </tr>
          </thead>
          <tbody>
            {autos.map((a) => (
              <tr key={a.directory_id} className="border-b border-gray-50">
                <td className="py-2 pr-3 font-mono">{a.directory_id}</td>
                <td className="py-2 pr-3">{MODES.find((m) => m.value === a.mode)?.label ?? a.mode}</td>
                <td className="py-2 pr-3">
                  {capText(a.cap)}
                  {a.follow_default === false && <div className="text-xs text-gray-500">admissions keep the cap they got</div>}
                </td>
                {manage && (
                  <td className="py-2 text-right whitespace-nowrap space-x-3">
                    <button className="text-sm text-sky-700 hover:underline" onClick={() => setEditing(a)}>
                      Edit
                    </button>
                    <button
                      className={dangerLink}
                      onClick={async () => {
                        try {
                          await api.removeAutoAdmission(a.directory_id);
                          refresh();
                        } catch (e) {
                          setError(errorMessage(e));
                        }
                      }}
                    >
                      Turn off
                    </button>
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {editing && (
        <Modal title="Automatic admission" onClose={() => setEditing(null)}>
          <AutoForm
            current={editing === "new" ? undefined : editing}
            directories={editing === "new" ? free : directories}
            onCancel={() => setEditing(null)}
            onDone={(updated) => {
              setEditing(null);
              setNote(updated > 0 ? `${updated} admissions took the new cap.` : null);
              refresh();
              onChanged();
            }}
          />
        </Modal>
      )}
    </Card>
  );
}
