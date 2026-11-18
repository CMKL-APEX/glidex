import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import ConfirmDialog from "../components/ConfirmDialog";
import { Badge, Card, ErrorBanner, Field, PageHeader, dangerLink, errorMessage, inputClass, primaryButton } from "../components/ui";
import { useSession, useTenantLabel } from "../session";

/** Where a tenant's opt-in stands at a cluster (D93). */
export function AdmissionStatus({ status }: { status?: string }) {
  if (status === "CLUSTER_ADMISSION_PENDING" || status === "TENANT_CLUSTER_STATUS_PENDING") return <Badge kind="warn">pending</Badge>;
  if (status === "CLUSTER_ADMISSION_REFUSED" || status === "TENANT_CLUSTER_STATUS_REFUSED") return <Badge kind="denied">refused</Badge>;
  return <Badge kind="ok">admitted</Badge>;
}

function admitLabel(c: api.DirectoryCluster): string {
  if (!c.whole) return "Its tenants' own offers";
  if (c.admit === "ADMIT_SETTING_APPROVAL") return "As a whole; the operator approves each tenant";
  return "As a whole; tenants admitted at once";
}

/** The clusters the directory trusts (spec/19-tenancy.md §12.6): a cluster
 * trusted as a whole takes the directory's tenants when they opt in, with
 * no offer each (D92, D93), or all of them by itself (D99). */
export default function DirectoryClusters() {
  const { host } = useSession();
  const label = useTenantLabel();
  const [clusters, setClusters] = useState<api.DirectoryCluster[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [offer, setOffer] = useState("");
  const [auto, setAuto] = useState(false);
  const [busy, setBusy] = useState(false);
  const [leaving, setLeaving] = useState<api.DirectoryCluster | null>(null);

  const refresh = useCallback(() => {
    api
      .listDirectoryClusters()
      .then(setClusters)
      .catch((e) => setError(errorMessage(e)));
  }, []);
  useEffect(() => refresh(), [refresh]);

  // The offer is a secret: it is sent once and cleared, never shown back.
  const trust = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.trustCluster(offer.trim(), auto);
      setOffer("");
      setAuto(false);
      refresh();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const setAutoOptIn = async (c: api.DirectoryCluster, on: boolean) => {
    setError(null);
    try {
      await api.setClusterAutoOptIn(c.id ?? "", on);
      refresh();
    } catch (err) {
      setError(errorMessage(err));
    }
  };

  return (
    <div>
      <PageHeader
        title="Clusters"
        subtitle={`Clusters of other organizations this directory trusts. A cluster trusted as a whole lets ${label.many.toLowerCase()} opt in without an offer each.`}
      />
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {host.trustCluster && (
        <Card title="Trust a cluster as a whole">
          <form onSubmit={trust} className="space-y-3">
            <Field label="The directory offer its operator sent you" htmlFor="directory-offer">
              <input
                id="directory-offer"
                type="password"
                className={`${inputClass} font-mono`}
                required
                value={offer}
                onChange={(e) => setOffer(e.target.value)}
                placeholder="gxo_…"
                autoComplete="off"
                spellCheck={false}
              />
            </Field>
            <label className="flex items-start gap-2 text-sm text-gray-700">
              <input id="trust-auto-opt-in" type="checkbox" className="mt-1" checked={auto} onChange={(e) => setAuto(e.target.checked)} />
              <span>
                Opt every {label.one.toLowerCase()} in by itself, new ones too. Leave it off to let each {label.one.toLowerCase()}'s
                administrators choose.
              </span>
            </label>
            <button type="submit" className={primaryButton} disabled={busy || !offer.trim().startsWith("gxo_")}>
              Trust cluster
            </button>
          </form>
        </Card>
      )}
      <div className="mt-6">
        <Card title="Trusted clusters">
          {clusters === null ? (
            <p className="text-sm text-gray-500">Loading…</p>
          ) : clusters.length === 0 ? (
            <p className="text-sm text-gray-500">None yet. A cluster's operator makes a directory offer; paste it above.</p>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-sm" data-testid="directory-clusters">
                <thead>
                  <tr className="text-left text-xs text-gray-500">
                    <th className="py-1 pr-3 font-medium">Cluster</th>
                    <th className="py-1 pr-3 font-medium">Trusted</th>
                    <th className="py-1 pr-3 font-medium">Automatic opt-in</th>
                    <th className="py-1 pr-3 font-medium">{label.many}</th>
                    {host.untrustCluster && <th className="py-1" />}
                  </tr>
                </thead>
                <tbody className="divide-y divide-gray-100">
                  {clusters.map((c) => (
                    <tr key={c.id} className="align-top">
                      <td className="py-1.5 pr-3">
                        <div className="text-gray-800">{c.name || c.id}</div>
                        <div className="text-xs text-gray-500 font-mono">{c.api}</div>
                      </td>
                      <td className="py-1.5 pr-3">{admitLabel(c)}</td>
                      <td className="py-1.5 pr-3">
                        {c.whole ? (
                          <label className="flex items-center gap-1">
                            <input
                              type="checkbox"
                              aria-label={`Automatic opt-in for ${c.name || c.id}`}
                              checked={!!c.auto_opt_in}
                              disabled={!host.trustCluster}
                              onChange={(e) => setAutoOptIn(c, e.target.checked)}
                            />
                            {c.auto_opt_in ? "on" : "off"}
                          </label>
                        ) : (
                          "—"
                        )}
                      </td>
                      <td className="py-1.5 pr-3">
                        {(c.tenants ?? []).length === 0 ? (
                          <span className="text-xs text-gray-500">none opted in</span>
                        ) : (
                          <ul className="space-y-0.5">
                            {(c.tenants ?? []).map((t) => (
                              <li key={t.id} className="flex items-center gap-2">
                                <span>{t.slug || t.id}</span>
                                <AdmissionStatus status={t.status} />
                              </li>
                            ))}
                          </ul>
                        )}
                      </td>
                      {host.untrustCluster && (
                        <td className="py-1.5 text-right">
                          <button type="button" className={dangerLink} onClick={() => setLeaving(c)}>
                            Untrust
                          </button>
                        </td>
                      )}
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </Card>
      </div>
      {leaving && (
        <ConfirmDialog
          title="Untrust Cluster"
          consequence={
            <p>
              This directory stops trusting <span className="font-semibold">{leaving.name || leaving.id}</span>: every{" "}
              {label.one.toLowerCase()} there is opted out, its people lose access, and the cluster is told. What they have there
              stays until that cluster's operator removes it.
            </p>
          }
          actionLabel="Untrust"
          onConfirm={async () => {
            await api.untrustCluster(leaving.id ?? "");
            refresh();
          }}
          onClose={() => setLeaving(null)}
        />
      )}
    </div>
  );
}
