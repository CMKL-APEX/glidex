import { useCallback, useEffect, useState, type FormEvent } from "react";
import { Link } from "react-router-dom";
import * as api from "../api";
import type { Admission, Tenant, TenantQuotas } from "../types";
import { QUOTA_KEYS, QUOTA_LABELS, formatTimestamp } from "../types";
import Modal from "../components/Modal";
import ConfirmDialog from "../components/ConfirmDialog";
import { LoadingCard } from "../components/Loading";
import {
  Badge,
  ErrorBanner,
  Field,
  PageHeader,
  dangerLink,
  errorMessage,
  inputClass,
  primaryButton,
  selectClass,
} from "../components/ui";
import { useSession } from "../session";

const suspended = (a: Admission) => a.status === "ADMISSION_STATUS_SUSPENDED";

/** Admit a tenant (no `admission`) or change an admission's cap. Each limit
 * left empty is unlimited. */
function AdmissionForm({
  admission,
  tenants,
  onDone,
  onCancel,
}: {
  admission?: Admission;
  /** Tenants the caller may list that aren't admitted yet (new admissions). */
  tenants: Tenant[];
  onDone: () => void;
  onCancel: () => void;
}) {
  const [tenant, setTenant] = useState(admission?.tenant_id ?? "");
  const [cap, setCap] = useState<Record<string, string>>(() =>
    Object.fromEntries(QUOTA_KEYS.map((k) => [k, admission?.cap?.[k] !== undefined ? String(admission.cap[k]) : ""])),
  );
  const [isSuspended, setSuspended] = useState(admission ? suspended(admission) : false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    // Pool limits aren't edited here: keep them.
    const q: TenantQuotas = admission?.cap?.devices ? { devices: admission.cap.devices } : {};
    for (const k of QUOTA_KEYS) if (cap[k].trim() !== "") q[k] = Number(cap[k]);
    try {
      await api.setAdmission(tenant.trim(), { cap: q, suspended: isSuspended });
      onDone();
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      {!admission && (
        <Field label="Tenant" htmlFor="admission-tenant">
          {tenants.length > 0 ? (
            <select id="admission-tenant" className={selectClass} required value={tenant} onChange={(e) => setTenant(e.target.value)}>
              <option value="">Choose…</option>
              {tenants.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.slug}
                  {t.display_name && t.display_name !== t.slug ? ` (${t.display_name})` : ""}
                </option>
              ))}
            </select>
          ) : (
            <input
              id="admission-tenant"
              className={inputClass}
              required
              placeholder="tenant slug or id"
              value={tenant}
              onChange={(e) => setTenant(e.target.value)}
            />
          )}
        </Field>
      )}
      <div>
        <p className="text-sm font-medium text-gray-700">Cap</p>
        <p className="text-xs text-gray-500">What all of the tenant's projects together may use on this cluster; empty is unlimited.</p>
        <div className="mt-2 grid grid-cols-2 gap-3">
          {QUOTA_KEYS.map((k) => (
            <Field key={k} label={QUOTA_LABELS[k]} htmlFor={`cap-${k}`}>
              <input
                id={`cap-${k}`}
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
      </div>
      <label className="flex items-start gap-2 text-sm text-gray-700">
        <input type="checkbox" className="mt-1" checked={isSuspended} onChange={(e) => setSuspended(e.target.checked)} />
        <span>Suspended: no new allocations or starts; running resources stay.</span>
      </label>
      <div className="flex justify-end gap-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm text-gray-700 hover:bg-gray-100 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primaryButton} disabled={busy}>
          {busy ? "Saving..." : admission ? "Save" : "Admit"}
        </button>
      </div>
    </form>
  );
}

export default function Admissions() {
  const { host } = useSession();
  const [admissions, setAdmissions] = useState<Admission[] | null>(null);
  const [tenants, setTenants] = useState<Tenant[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Admission | "new" | null>(null);
  const [removing, setRemoving] = useState<Admission | null>(null);

  const refresh = useCallback(() => {
    api
      .listAdmissions()
      .then(setAdmissions)
      .catch((e) => setError(errorMessage(e)));
    // Slugs for the tenant ids, as far as the caller may read tenants.
    api
      .listTenants()
      .then(setTenants)
      .catch(() => setTenants([]));
  }, []);

  useEffect(() => {
    if (host.listAdmissions) refresh();
  }, [host.listAdmissions, refresh]);

  if (!host.listAdmissions) {
    return (
      <div>
        <PageHeader title="Admissions" />
        <p className="text-gray-500">You can't see which tenants this cluster admits.</p>
      </div>
    );
  }

  const tenantOf = (id: string) => tenants.find((t) => t.id === id);
  const manage = host.manageAdmissions;
  const run = async (f: () => Promise<unknown>) => {
    setError(null);
    try {
      await f();
      refresh();
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  return (
    <div>
      <PageHeader title="Admissions" subtitle="The tenants this cluster admits, and the cap on what each may use here.">
        {manage && (
          <button className={primaryButton} onClick={() => setEditing("new")}>
            + Admit Tenant
          </button>
        )}
      </PageHeader>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {admissions === null ? (
        <LoadingCard />
      ) : admissions.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">
          No tenant is admitted.
        </div>
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500 border-b border-gray-100">
                <th className="px-4 py-3 font-medium">Tenant</th>
                <th className="px-4 py-3 font-medium">Status</th>
                {QUOTA_KEYS.map((k) => (
                  <th key={k} className="px-4 py-3 font-medium">
                    {QUOTA_LABELS[k]}
                  </th>
                ))}
                <th className="px-4 py-3 font-medium">Changed</th>
                {manage && <th className="px-4 py-3" />}
              </tr>
            </thead>
            <tbody>
              {admissions.map((a) => {
                const t = tenantOf(a.tenant_id);
                return (
                  <tr key={a.tenant_id} className="border-b border-gray-50 hover:bg-gray-50">
                    <td className="px-4 py-3">
                      {t ? (
                        <Link to={`/tenants/${t.id}`} className="font-medium text-sky-700 hover:underline">
                          {t.slug}
                        </Link>
                      ) : a.tenant_slug ? (
                        <span className="font-medium text-gray-800" title={a.tenant_id}>
                          {a.tenant_slug}
                        </span>
                      ) : (
                        <code className="text-xs font-mono text-gray-700">{a.tenant_id}</code>
                      )}
                    </td>
                    <td className="px-4 py-3">
                      {suspended(a) ? <Badge kind="warn">suspended</Badge> : <Badge kind="ok">admitted</Badge>}
                    </td>
                    {QUOTA_KEYS.map((k) => (
                      <td key={k} className="px-4 py-3 text-gray-700">
                        {a.cap?.[k] ?? <span className="text-gray-400">∞</span>}
                      </td>
                    ))}
                    <td className="px-4 py-3 text-gray-500 whitespace-nowrap">
                      {formatTimestamp(a.at)}
                      {a.by && <div className="text-xs text-gray-400">by {a.by.slice(0, 8)}</div>}
                    </td>
                    {manage && (
                      <td className="px-4 py-3 text-right whitespace-nowrap space-x-3">
                        <button className="text-sm text-sky-700 hover:underline" onClick={() => setEditing(a)}>
                          Edit
                        </button>
                        {/* This cluster's operator stays admitted (D4). */}
                        {!t?.operates_here && (
                          <>
                            <button
                              className="text-sm text-sky-700 hover:underline"
                              onClick={() => run(() => api.setAdmission(a.tenant_id, { suspended: !suspended(a) }))}
                            >
                              {suspended(a) ? "Resume" : "Suspend"}
                            </button>
                            <button className={dangerLink} onClick={() => setRemoving(a)}>
                              Remove
                            </button>
                          </>
                        )}
                      </td>
                    )}
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {editing && (
        <Modal
          title={editing === "new" ? "Admit Tenant" : `Admission of ${tenantOf(editing.tenant_id)?.slug ?? (editing.tenant_slug || editing.tenant_id)}`}
          onClose={() => setEditing(null)}
        >
          <AdmissionForm
            admission={editing === "new" ? undefined : editing}
            tenants={tenants.filter((t) => !admissions?.some((a) => a.tenant_id === t.id))}
            onCancel={() => setEditing(null)}
            onDone={() => {
              setEditing(null);
              refresh();
            }}
          />
        </Modal>
      )}
      {removing && (
        <ConfirmDialog
          title="Remove Admission"
          consequence={
            <p>
              Tenant <span className="font-semibold">{tenantOf(removing.tenant_id)?.slug ?? (removing.tenant_slug || removing.tenant_id)}</span> can't
              create VMs, disks, networks or routers, or start VMs, on this cluster any more.
            </p>
          }
          actionLabel="Remove"
          onConfirm={async () => {
            await api.removeAdmission(removing.tenant_id);
            refresh();
          }}
          onClose={() => setRemoving(null)}
        />
      )}
    </div>
  );
}
