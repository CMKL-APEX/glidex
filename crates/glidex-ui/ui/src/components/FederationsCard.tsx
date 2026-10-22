import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import type { Tenant } from "../types";
import ConfirmDialog from "./ConfirmDialog";
import { Loading } from "./Loading";
import { Badge, Card, ErrorBanner, Field, dangerLink, errorMessage, inputClass, primaryButton, secondaryButton, selectClass } from "./ui";
import { useSession } from "../session";

/** Issuers a federation usually trusts, to fill the form with. */
const PRESETS = [
  { label: "GitHub Actions", issuer: "https://token.actions.githubusercontent.com", pinned: "repository" },
  { label: "GitLab.com CI", issuer: "https://gitlab.com", pinned: "project_path" },
  { label: "Other (any OIDC workload issuer)", issuer: "", pinned: "sub" },
] as const;

interface RuleForm {
  /** One `claim=value` per line. */
  claims: string;
  service_account: string;
  ttl: string;
}

const parseClaims = (s: string): Record<string, string> =>
  Object.fromEntries(
    s
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean)
      .map((l) => {
        const i = l.indexOf("=");
        return i < 0 ? [l, ""] : [l.slice(0, i).trim(), l.slice(i + 1).trim()];
      }),
  );

const showClaims = (m?: Record<string, string>) =>
  Object.entries(m ?? {})
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([k, v]) => `${k}=${v}`)
    .join(", ");

/** A tenant's workload federations (spec/tenancy.md §9): CI jobs trade
 * their own OIDC token for a short-lived one acting as a service account. */
export default function FederationsCard({ tenant, canManage }: { tenant: Tenant; canManage: boolean }) {
  const { projects } = useSession();
  const [list, setList] = useState<api.Federation[] | null>(null);
  const [accounts, setAccounts] = useState<{ id: string; label: string }[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [deleting, setDeleting] = useState<api.Federation | null>(null);
  const [preset, setPreset] = useState(0);
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [issuer, setIssuer] = useState<string>(PRESETS[0].issuer);
  const [audience, setAudience] = useState("glidex");
  const [rules, setRules] = useState<RuleForm[]>([{ claims: "repository=\nref=refs/heads/main", service_account: "", ttl: "" }]);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(() => {
    api
      .listFederations(tenant.id)
      .then(setList)
      .catch((e) => setError(errorMessage(e)));
  }, [tenant.id]);
  useEffect(() => refresh(), [refresh]);
  // The tenant's service accounts: what a rule may name.
  useEffect(() => {
    if (!canManage) return;
    const mine = new Map(projects.filter((p) => p.tenant_id === tenant.id).map((p) => [p.id, p.name]));
    api
      .listTokens()
      .then((ts) =>
        setAccounts(
          ts
            .filter((t) => t.kind === "TOKEN_KIND_SERVICE_ACCOUNT" && t.project && mine.has(t.project))
            .map((t) => ({ id: t.id, label: `${t.name} (${mine.get(t.project!)})` })),
        ),
      )
      .catch(() => setAccounts([]));
  }, [canManage, projects, tenant.id]);

  const name_of = (sa: string) => accounts.find((a) => a.id === sa)?.label ?? sa;
  const run = async (f: () => Promise<unknown>) => {
    setError(null);
    try {
      await f();
      refresh();
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  const create = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.createFederation(tenant.id, {
        id: id.trim(),
        display_name: name.trim(),
        issuer: issuer.trim(),
        audience: audience.trim(),
        enabled: true,
        rules: rules.map((r) => ({
          match: parseClaims(r.claims),
          service_account: r.service_account,
          ttl_secs: r.ttl.trim() ? Number(r.ttl) : undefined,
        })),
      });
      setAdding(false);
      setId("");
      setName("");
      refresh();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };
  const setRule = (i: number, patch: Partial<RuleForm>) => setRules((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));

  return (
    <Card title="Workload federation">
      <p className="mb-3 text-sm text-gray-500">
        CI jobs (GitHub Actions, GitLab CI, Kubernetes) trade the OIDC token their platform gives them for a short-lived glidex token that
        acts as one of this tenant's service accounts, so they store no glidex secret. A rule matches claims exactly.
      </p>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {list === null ? (
        <Loading />
      ) : list.length === 0 ? (
        <p className="text-sm text-gray-500">No federations.</p>
      ) : (
        <ul className="divide-y divide-gray-100" data-testid="federations">
          {list.map((f) => (
            <li key={f.id} className="py-3 text-sm">
              <div className="flex items-center justify-between">
                <div>
                  <span className="font-medium text-gray-900">{f.display_name || f.id}</span>{" "}
                  <code className="text-xs text-gray-400 font-mono">{f.id}</code>{" "}
                  {f.enabled ? <Badge kind="ok">enabled</Badge> : <Badge kind="muted">disabled</Badge>}
                </div>
                {canManage && (
                  <div className="space-x-3">
                    <button
                      className="text-sky-700 hover:underline"
                      onClick={() => run(() => api.updateFederation(tenant.id, f.id, { enabled: !f.enabled }, ["enabled"]))}
                    >
                      {f.enabled ? "Disable" : "Enable"}
                    </button>
                    <button className={dangerLink} onClick={() => setDeleting(f)}>
                      Delete
                    </button>
                  </div>
                )}
              </div>
              <div className="mt-1 text-gray-600">
                <span className="font-mono text-xs">{f.issuer}</span> · audience <span className="font-mono text-xs">{f.audience}</span>
              </div>
              <ol className="mt-1 list-decimal pl-5 text-gray-600">
                {(f.rules ?? []).map((r, i) => (
                  <li key={i}>
                    <span className="font-mono text-xs">{showClaims(r.match)}</span> → {name_of(r.service_account ?? "")}, {r.ttl_secs ?? 900} s
                  </li>
                ))}
              </ol>
              <details className="mt-1">
                <summary className="cursor-pointer text-sky-700">How a job uses it</summary>
                <pre className="mt-1 p-2 bg-gray-50 rounded text-xs overflow-x-auto" data-testid="exchange-snippet">
                  {f.issuer.includes("githubusercontent")
                    ? `permissions:\n  id-token: write\nsteps:\n  - run: |\n      JWT=$(curl -sH "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \\\n        "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=${f.audience}" | jq -r .value)\n      GLIDEX_TOKEN=$(curl -s ${window.location.origin}/api/auth/exchange \\\n        -d grant_type=urn:ietf:params:oauth:grant-type:token-exchange \\\n        -d subject_token_type=urn:ietf:params:oauth:token-type:jwt \\\n        -d federation=${tenant.slug}/${f.id} -d subject_token="$JWT" | jq -r .access_token)`
                    : `curl -s ${window.location.origin}/api/auth/exchange \\\n  -d grant_type=urn:ietf:params:oauth:grant-type:token-exchange \\\n  -d subject_token_type=urn:ietf:params:oauth:token-type:jwt \\\n  -d federation=${tenant.slug}/${f.id} -d subject_token="$JWT"`}
                </pre>
              </details>
            </li>
          ))}
        </ul>
      )}
      {canManage && !adding && (
        <button className={`mt-3 ${secondaryButton}`} onClick={() => setAdding(true)}>
          + Add federation
        </button>
      )}
      {canManage && adding && (
        <form onSubmit={create} className="mt-4 space-y-3 p-3 border border-gray-200 rounded-lg">
          <div className="grid grid-cols-2 gap-3">
            <Field label="Issuer" htmlFor="fed-preset">
              <select
                id="fed-preset"
                className={selectClass}
                value={preset}
                onChange={(e) => {
                  const n = Number(e.target.value);
                  setPreset(n);
                  setIssuer(PRESETS[n].issuer);
                  setRules((rs) => rs.map((r, i) => (i === 0 ? { ...r, claims: `${PRESETS[n].pinned}=\n` } : r)));
                }}
              >
                {PRESETS.map((p, i) => (
                  <option key={p.label} value={i}>
                    {p.label}
                  </option>
                ))}
              </select>
            </Field>
            <Field label="Issuer URL" htmlFor="fed-issuer">
              <input id="fed-issuer" className={inputClass} required value={issuer} onChange={(e) => setIssuer(e.target.value)} />
            </Field>
            <Field label="Id" htmlFor="fed-id">
              <input id="fed-id" className={inputClass} required placeholder="github" value={id} onChange={(e) => setId(e.target.value)} />
            </Field>
            <Field label="Name" htmlFor="fed-name">
              <input id="fed-name" className={inputClass} value={name} onChange={(e) => setName(e.target.value)} />
            </Field>
            <Field label="Audience" htmlFor="fed-audience">
              <input id="fed-audience" className={inputClass} required value={audience} onChange={(e) => setAudience(e.target.value)} />
            </Field>
          </div>
          {rules.map((r, i) => (
            <div key={i} className="grid grid-cols-3 gap-3 items-start">
              <Field label={`Rule ${i + 1}: claims`} htmlFor={`fed-claims-${i}`}>
                <textarea
                  id={`fed-claims-${i}`}
                  className={`${inputClass} font-mono text-xs`}
                  rows={3}
                  placeholder={"repository=acme/infra\nref=refs/heads/main"}
                  value={r.claims}
                  onChange={(e) => setRule(i, { claims: e.target.value })}
                />
              </Field>
              <Field label="Service account" htmlFor={`fed-sa-${i}`}>
                <select
                  id={`fed-sa-${i}`}
                  className={selectClass}
                  required
                  value={r.service_account}
                  onChange={(e) => setRule(i, { service_account: e.target.value })}
                >
                  <option value="">Choose…</option>
                  {accounts.map((a) => (
                    <option key={a.id} value={a.id}>
                      {a.label}
                    </option>
                  ))}
                </select>
                {accounts.length === 0 && (
                  <p className="mt-1 text-xs text-gray-500">This tenant's projects have no service accounts: make one on the Tokens page.</p>
                )}
              </Field>
              <Field label="Lifetime (seconds)" htmlFor={`fed-ttl-${i}`}>
                <input
                  id={`fed-ttl-${i}`}
                  className={inputClass}
                  type="number"
                  min={60}
                  max={3600}
                  placeholder="900"
                  value={r.ttl}
                  onChange={(e) => setRule(i, { ttl: e.target.value })}
                />
              </Field>
            </div>
          ))}
          <p className="text-xs text-gray-500">
            One claim per line, matched exactly. Name the repository (GitHub), project_path (GitLab) or sub, and usually the branch or
            environment too.
          </p>
          <div className="flex justify-between">
            <button
              type="button"
              className={secondaryButton}
              onClick={() => setRules((rs) => [...rs, { claims: "", service_account: "", ttl: "" }])}
            >
              + Rule
            </button>
            <div className="space-x-3">
              <button type="button" className="px-4 py-2 text-sm text-gray-700 hover:bg-gray-100 rounded-lg" onClick={() => setAdding(false)}>
                Cancel
              </button>
              <button type="submit" className={primaryButton} disabled={busy}>
                {busy ? "Checking the issuer..." : "Add federation"}
              </button>
            </div>
          </div>
        </form>
      )}
      {deleting && (
        <ConfirmDialog
          title={`Delete ${deleting.display_name || deleting.id}`}
          consequence={<p>Its jobs can't exchange tokens any more, and the tokens it gave stop working now.</p>}
          actionLabel="Delete"
          onConfirm={async () => {
            await api.deleteFederation(tenant.id, deleting.id);
            refresh();
          }}
          onClose={() => setDeleting(null)}
        />
      )}
    </Card>
  );
}
