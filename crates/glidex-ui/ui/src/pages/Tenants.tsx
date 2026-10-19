import { useCallback, useEffect, useState, type FormEvent } from "react";
import { Link } from "react-router-dom";
import * as api from "../api";
import type { Tenant } from "../types";
import { formatTimestamp, isOidcKind } from "../types";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";
import { Badge, ErrorBanner, Field, PageHeader, errorMessage, inputClass, primaryButton, secondaryButton, selectClass } from "../components/ui";
import { useSession } from "../session";

/** The tenant's flags as badges: owner of the directory, operator of this
 * cluster, suspended. */
export function TenantFlags({ tenant }: { tenant: Tenant }) {
  return (
    <span className="inline-flex flex-wrap gap-1">
      {tenant.owns_directory && <Badge kind="role">owns directory</Badge>}
      {tenant.operator && <Badge kind="link">operator</Badge>}
      {tenant.suspended && <Badge kind="denied">suspended</Badge>}
    </span>
  );
}

/** Whether this cluster admits the tenant (spec/tenancy.md D9). */
export function AdmissionBadge({ tenant }: { tenant: Tenant }) {
  const a = tenant.admission;
  if (!a) return <Badge kind="muted">not admitted</Badge>;
  if (a.status === "ADMISSION_STATUS_SUSPENDED") return <Badge kind="warn">admission suspended</Badge>;
  return <Badge kind="ok">admitted</Badge>;
}

function CreateTenantForm({ onDone, onCancel }: { onDone: () => void; onCancel: () => void }) {
  const [slug, setSlug] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [operator, setOperator] = useState(false);
  // The first tenant-admin, invited (spec/tenancy.md §11): optional.
  const [adminEmail, setAdminEmail] = useState("");
  const [connector, setConnector] = useState("");
  const [connectors, setConnectors] = useState<{ id: string; label: string }[]>([]);
  const [link, setLink] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api
      .authMethods()
      .then((m) => {
        const cs = (m.connectors ?? []).filter((c) => isOidcKind(c.kind)).map((c) => ({ id: c.id, label: c.display_name || c.id }));
        setConnectors(cs);
        setConnector((cur) => cur || cs[0]?.id || "");
      })
      .catch(() => setConnectors([]));
  }, []);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const r = await api.createTenant({
        slug,
        display_name: displayName || undefined,
        operator: operator || undefined,
        first_admin: adminEmail.trim() ? { email: adminEmail.trim(), connector } : undefined,
      });
      if (r.link) {
        // On the address used here: the server's is its first allowed origin.
        const u = new URL(r.link);
        setLink(`${window.location.origin}${u.pathname}${u.hash}`);
        setBusy(false);
      } else {
        onDone();
      }
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  if (link) {
    return (
      <div className="space-y-4" data-testid="first-admin-link">
        <p className="text-sm text-gray-700">
          The tenant is made. Send this link to its first administrator; it is shown once and can't be shown again.
        </p>
        <code className="block break-all font-mono text-xs bg-gray-50 px-2 py-2 rounded border border-gray-200">{link}</code>
        <div className="flex justify-end gap-3">
          <button type="button" className={secondaryButton} onClick={() => void navigator.clipboard?.writeText(link)}>
            Copy
          </button>
          <button type="button" className={primaryButton} onClick={onDone}>
            Done
          </button>
        </div>
      </div>
    );
  }

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <Field label="Slug" htmlFor="tenant-slug">
        <input
          id="tenant-slug"
          className={inputClass}
          required
          pattern="[a-z0-9\-]{1,32}"
          title="1-32 lowercase letters, digits and dashes"
          value={slug}
          onChange={(e) => setSlug(e.target.value)}
        />
      </Field>
      <Field label="Display name" htmlFor="tenant-name">
        <input id="tenant-name" className={inputClass} value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
      </Field>
      <label className="flex items-start gap-2 text-sm text-gray-700">
        <input type="checkbox" className="mt-1" checked={operator} onChange={(e) => setOperator(e.target.checked)} />
        <span>Operator tenant: its members can hold host roles on the clusters it operates.</span>
      </label>
      <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
        <Field label="First administrator's email (optional)" htmlFor="tenant-admin-email">
          <input
            id="tenant-admin-email"
            type="email"
            className={inputClass}
            placeholder="person@example.org"
            value={adminEmail}
            onChange={(e) => setAdminEmail(e.target.value)}
          />
        </Field>
        <Field label="Signs in through" htmlFor="tenant-admin-connector">
          <select
            id="tenant-admin-connector"
            className={selectClass}
            value={connector}
            onChange={(e) => setConnector(e.target.value)}
            disabled={!adminEmail.trim()}
          >
            {connectors.length === 0 && <option value="">No OIDC or Google connector</option>}
            {connectors.map((c) => (
              <option key={c.id} value={c.id}>
                {c.label}
              </option>
            ))}
          </select>
        </Field>
      </div>
      <p className="text-xs text-gray-500">
        With an email, they get an invitation to be the tenant's administrator. A new tenant isn't admitted to this cluster
        yet; admit it on the Admissions page.
      </p>
      <div className="flex justify-end gap-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm text-gray-700 hover:bg-gray-100 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primaryButton} disabled={busy}>
          {busy ? "Creating..." : "Create Tenant"}
        </button>
      </div>
    </form>
  );
}

export default function Tenants() {
  const { host } = useSession();
  const [tenants, setTenants] = useState<Tenant[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);

  const refresh = useCallback(() => {
    api
      .listTenants()
      .then((t) => setTenants(t.sort((a, b) => a.slug.localeCompare(b.slug))))
      .catch((e) => setError(errorMessage(e)));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  return (
    <div>
      <PageHeader title="Tenants" subtitle="Tenants own projects; their members and roles decide who may use them.">
        {host.createTenant && (
          <button className={primaryButton} onClick={() => setCreating(true)}>
            + Create Tenant
          </button>
        )}
      </PageHeader>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {tenants === null ? (
        <LoadingCard />
      ) : tenants.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">
          You can't see any tenant.
        </div>
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500 border-b border-gray-100">
                <th className="px-4 py-3 font-medium">Tenant</th>
                <th className="px-4 py-3 font-medium">Flags</th>
                <th className="px-4 py-3 font-medium">This cluster</th>
                <th className="px-4 py-3 font-medium">Created</th>
              </tr>
            </thead>
            <tbody>
              {tenants.map((t) => (
                <tr key={t.id} className="border-b border-gray-50 hover:bg-gray-50">
                  <td className="px-4 py-3">
                    <Link to={`/tenants/${t.id}`} className="font-medium text-sky-700 hover:underline">
                      {t.slug}
                    </Link>
                    {t.display_name && t.display_name !== t.slug && <div className="text-xs text-gray-500">{t.display_name}</div>}
                  </td>
                  <td className="px-4 py-3">
                    <TenantFlags tenant={t} />
                  </td>
                  <td className="px-4 py-3">
                    <AdmissionBadge tenant={t} />
                  </td>
                  <td className="px-4 py-3 text-gray-500">{formatTimestamp(t.created_at)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {creating && (
        <Modal title="Create Tenant" onClose={() => setCreating(false)}>
          <CreateTenantForm
            onCancel={() => setCreating(false)}
            onDone={() => {
              setCreating(false);
              refresh();
            }}
          />
        </Modal>
      )}
    </div>
  );
}
