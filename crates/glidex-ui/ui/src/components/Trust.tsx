import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import { formatTimestamp } from "../types";
import ConfirmDialog from "./ConfirmDialog";
import Modal from "./Modal";
import { Badge, Card, ErrorBanner, Field, dangerLink, errorMessage, inputClass, primaryButton, secondaryButton } from "./ui";

/** Seconds as a short age: "40 s", "12 min", "3 h". */
function age(secs: number): string {
  if (secs < 120) return `${secs} s`;
  if (secs < 7200) return `${Math.round(secs / 60)} min`;
  return `${Math.round(secs / 3600)} h`;
}

/** The directories of other organizations this cluster trusts
 *  (spec/19-tenancy.md D65): their tenants, how fresh their feed is, and the
 *  staleness limit past which their people are refused here (D13). */
export function TrustedDirectoriesCard({ manage }: { manage: boolean }) {
  const [dirs, setDirs] = useState<api.TrustedDirectory[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<api.TrustedDirectory | null>(null);
  const [minutes, setMinutes] = useState("");

  const refresh = useCallback(() => {
    api
      .listTrustedDirectories()
      .then(setDirs)
      .catch((e) => setError(errorMessage(e)));
  }, []);
  useEffect(() => refresh(), [refresh]);

  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (!editing) return;
    setError(null);
    try {
      await api.setTrustedDirectory(editing.id ?? "", minutes.trim() ? Number(minutes) : undefined);
      setEditing(null);
      refresh();
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  return (
    <Card title="Directories of other organizations">
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {dirs === null ? (
        <p className="text-sm text-gray-500">Loading…</p>
      ) : dirs.length === 0 ? (
        <p className="text-sm text-gray-500">None yet. Send a tenant's administrator an offer below; this cluster trusts their directory once they accept it.</p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm" data-testid="trusted-directories-table">
            <thead>
              <tr className="text-left text-xs text-gray-500">
                <th className="py-1 pr-3 font-medium">Directory</th>
                <th className="py-1 pr-3 font-medium">Tenants</th>
                <th className="py-1 pr-3 font-medium">Last feed</th>
                <th className="py-1 pr-3 font-medium">Limit</th>
                {manage && <th className="py-1" />}
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {dirs.map((d) => (
                <tr key={d.id}>
                  <td className="py-1.5 pr-3">
                    <div className="font-mono text-xs break-all">{d.issuer}</div>
                    <div className="text-xs text-gray-500 font-mono">{d.id}</div>
                  </td>
                  <td className="py-1.5 pr-3">{(d.tenants ?? []).map((t) => t.display_name || t.slug).join(", ") || "—"}</td>
                  <td className="py-1.5 pr-3">
                    {d.last_sync ? `${age(Number(d.lag_seconds ?? 0))} ago` : "not yet"}
                    {d.stale && (
                      <span className="ml-2">
                        <Badge kind="denied">stale: its people are refused</Badge>
                      </span>
                    )}
                  </td>
                  <td className="py-1.5 pr-3">
                    {d.effective_max_staleness_minutes ?? 60} min
                    {d.max_staleness_minutes === undefined && <span className="text-xs text-gray-500"> (default)</span>}
                  </td>
                  {manage && (
                    <td className="py-1.5 text-right">
                      <button
                        type="button"
                        className="text-sm text-sky-700 hover:underline"
                        onClick={() => {
                          setMinutes(d.max_staleness_minutes ?? "");
                          setEditing(d);
                        }}
                      >
                        Limit
                      </button>
                    </td>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {editing && (
        <Modal title="Staleness limit" onClose={() => setEditing(null)}>
          <form onSubmit={save} className="space-y-4">
            <p className="text-sm text-gray-600">
              If no feed comes from <span className="font-mono">{editing.issuer}</span> for longer than this, its people's sessions and tokens stop working here until
              one does, since a removal may be missing. Leave it empty for this cluster's default.
            </p>
            <Field label="Minutes" htmlFor="staleness-minutes">
              <input id="staleness-minutes" className={inputClass} type="number" min={1} value={minutes} onChange={(e) => setMinutes(e.target.value)} />
            </Field>
            <div className="flex justify-end gap-2">
              <button type="button" className={secondaryButton} onClick={() => setEditing(null)}>
                Cancel
              </button>
              <button type="submit" className={primaryButton}>
                Save
              </button>
            </div>
          </form>
        </Modal>
      )}
    </Card>
  );
}

/** Offers (spec/19-tenancy.md D64): admission for a tenant of another
 *  organization's directory, as a one-time token its administrator
 *  redeems at their directory. */
export function OffersCard({ manage }: { manage: boolean }) {
  const [offers, setOffers] = useState<api.Offer[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tenant, setTenant] = useState("");
  const [directory, setDirectory] = useState("");
  const [days, setDays] = useState("7");
  const [made, setMade] = useState<string | null>(null);
  const [revoking, setRevoking] = useState<api.Offer | null>(null);

  const refresh = useCallback(() => {
    api
      .listOffers()
      .then(setOffers)
      .catch((e) => setError(errorMessage(e)));
  }, []);
  useEffect(() => refresh(), [refresh]);

  const offer = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    try {
      const r = await api.offerAdmission(tenant.trim(), directory.trim(), days ? Number(days) : undefined);
      setMade(r.token);
      setTenant("");
      setDirectory("");
      refresh();
    } catch (err) {
      setError(errorMessage(err));
    }
  };
  const state = (o: api.Offer) => (o.redeemed_at ? "redeemed" : o.expires_at && new Date(o.expires_at) < new Date() ? "expired" : "open");

  return (
    <Card title="Offers to other organizations' tenants">
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {manage && (
        <form onSubmit={offer} className="grid gap-3 sm:grid-cols-[1fr_2fr_6rem_auto] items-end mb-4">
          <Field label="Tenant (its slug there)" htmlFor="offer-tenant">
            <input id="offer-tenant" className={inputClass} required value={tenant} onChange={(e) => setTenant(e.target.value)} placeholder="acme" />
          </Field>
          <Field label="Its directory's issuer" htmlFor="offer-directory">
            <input
              id="offer-directory"
              className={inputClass}
              required
              type="url"
              value={directory}
              onChange={(e) => setDirectory(e.target.value)}
              placeholder="https://id.acme.example:8841/d/local"
            />
          </Field>
          <Field label="Days" htmlFor="offer-days">
            <input id="offer-days" className={inputClass} type="number" min={1} max={30} value={days} onChange={(e) => setDays(e.target.value)} />
          </Field>
          <button type="submit" className={primaryButton}>
            Make offer
          </button>
        </form>
      )}
      {made && (
        <div className="mb-4 rounded-lg border border-amber-200 bg-amber-50 p-3 text-sm" data-testid="offer-token">
          <p className="mb-1 text-amber-900">
            Send this to the tenant's administrator. It is shown once; they accept it at their directory (the tenant's page there, or{" "}
            <code>gxctl tenant opt-in</code>).
          </p>
          <div className="font-mono text-xs break-all select-all">{made}</div>
          <div className="mt-2 flex gap-2">
            <button type="button" className={secondaryButton} onClick={() => navigator.clipboard?.writeText(made).catch(() => {})}>
              Copy
            </button>
            <button type="button" className={secondaryButton} onClick={() => setMade(null)}>
              Done
            </button>
          </div>
        </div>
      )}
      {offers === null ? (
        <p className="text-sm text-gray-500">Loading…</p>
      ) : offers.length === 0 ? (
        <p className="text-sm text-gray-500">No offers.</p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-xs text-gray-500">
                <th className="py-1 pr-3 font-medium">Tenant</th>
                <th className="py-1 pr-3 font-medium">Directory</th>
                <th className="py-1 pr-3 font-medium">State</th>
                <th className="py-1 pr-3 font-medium">Expires</th>
                {manage && <th className="py-1" />}
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {offers.map((o) => (
                <tr key={o.id}>
                  <td className="py-1.5 pr-3">{o.tenant}</td>
                  <td className="py-1.5 pr-3 font-mono text-xs break-all">{o.directory}</td>
                  <td className="py-1.5 pr-3">{state(o)}</td>
                  <td className="py-1.5 pr-3 text-xs text-gray-500">{formatTimestamp(o.expires_at)}</td>
                  {manage && (
                    <td className="py-1.5 text-right">
                      {state(o) === "open" && (
                        <button type="button" className={dangerLink} onClick={() => setRevoking(o)}>
                          Revoke
                        </button>
                      )}
                    </td>
                  )}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {revoking && (
        <ConfirmDialog
          title="Revoke Offer"
          consequence={<p>The offer for {revoking.tenant} can't be accepted any more.</p>}
          actionLabel="Revoke"
          onConfirm={async () => {
            await api.revokeOffer(revoking.id ?? "");
            refresh();
          }}
          onClose={() => setRevoking(null)}
        />
      )}
    </Card>
  );
}
