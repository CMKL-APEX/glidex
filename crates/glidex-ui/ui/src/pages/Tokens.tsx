import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import type { ApiToken, CreatedToken, CreateTokenRequest } from "../types";
import { HOST_ROLES, PROJECT_ROLES, formatTimestamp, isServiceToken, roleLabel, tsSecs } from "../types";
import Modal from "../components/Modal";
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
  secondaryButton,
  selectClass,
} from "../components/ui";
import { useCan, useSession } from "../session";

interface RoleRow {
  role: string;
  project: string;
}

const isProjectRole = (r: string) => (PROJECT_ROLES as readonly string[]).includes(r);

function CreateTokenForm({ onCreated, onCancel }: { onCreated: (t: CreatedToken) => void; onCancel: () => void }) {
  const { project, projects, projectName } = useSession();
  const [canService] = useCan(project ? [{ action: "manageServiceTokens", resource: { type: "Project", id: project } }] : []) ?? [];
  const [name, setName] = useState("");
  const [days, setDays] = useState("90");
  const [kind, setKind] = useState<"personal" | "service_account">("personal");
  const [roles, setRoles] = useState<RoleRow[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Clusters of other organizations its tenants use (spec/19-tenancy.md §7.4).
  const [others, setOthers] = useState<{ id: string; name: string }[]>([]);
  const [also, setAlso] = useState<string[]>([]);
  useEffect(() => {
    let live = true;
    api
      .listTenants()
      .then((ts) => Promise.all(ts.map((t) => api.listTenantClusters(t.id).catch(() => [] as api.TenantCluster[]))))
      .then((lists) => {
        const seen = new Map<string, string>();
        for (const c of lists.flat()) if (c.cluster_id) seen.set(c.cluster_id, c.name || c.cluster_id);
        if (live) setOthers([...seen].map(([id, name]) => ({ id, name })));
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);

  const service = kind === "service_account";
  const roleChoices: readonly string[] = service ? PROJECT_ROLES : [...PROJECT_ROLES, ...HOST_ROLES];

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const req: CreateTokenRequest = {
      name,
      expires_in_days: days ? Number(days) : undefined,
      kind: service ? "TOKEN_KIND_SERVICE_ACCOUNT" : "TOKEN_KIND_PERSONAL",
      project: service ? project ?? undefined : undefined,
      roles: roles.map((r) => ({
        role: r.role,
        project: isProjectRole(r.role) ? (service ? project ?? undefined : r.project) : undefined,
      })),
      clusters: also.length > 0 ? also : undefined,
    };
    try {
      onCreated(await api.createToken(req));
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <Field label="Name" htmlFor="token-name">
        <input id="token-name" className={inputClass} required value={name} onChange={(e) => setName(e.target.value)} />
      </Field>
      <Field label="Expires in (days)" htmlFor="token-days">
        <input
          id="token-days"
          className={inputClass}
          type="number"
          min={1}
          max={365}
          value={days}
          onChange={(e) => setDays(e.target.value)}
        />
      </Field>
      {others.length > 0 && (
        <fieldset>
          <legend className="block text-sm font-medium text-gray-700">Also works on (other organizations' clusters)</legend>
          <p className="text-xs text-gray-500">It always works here. Each cluster you add learns its hash and could replay it to the others it lists, so add only those you need.</p>
          <label className="mt-1 flex items-center gap-2 text-sm">
            <input type="checkbox" checked={also.includes("all")} onChange={(e) => setAlso(e.target.checked ? ["all"] : [])} />
            Every cluster my tenants use
          </label>
          {!also.includes("all") &&
            others.map((c) => (
              <label key={c.id} className="flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={also.includes(c.id)}
                  onChange={(e) => setAlso(e.target.checked ? [...also, c.id] : also.filter((x) => x !== c.id))}
                />
                {c.name} <span className="text-xs text-gray-500 font-mono">{c.id}</span>
              </label>
            ))}
        </fieldset>
      )}
      {canService && (
        <Field label="Kind" htmlFor="token-kind">
          <select
            id="token-kind"
            className={selectClass}
            value={kind}
            onChange={(e) => {
              setKind(e.target.value as typeof kind);
              setRoles([]);
            }}
          >
            <option value="personal">Personal (acts as you, never more)</option>
            <option value="service_account">Service account of {projectName(project)}</option>
          </select>
        </Field>
      )}
      <div>
        <div className="flex items-center justify-between">
          <span className="block text-sm font-medium text-gray-700">Roles {service ? "" : "(optional: narrow the token)"}</span>
          <button
            type="button"
            className="text-sm text-sky-700 hover:underline"
            onClick={() => setRoles([...roles, { role: roleChoices[0], project: project ?? projects[0]?.id ?? "" }])}
          >
            + Add role
          </button>
        </div>
        {roles.length === 0 && (
          <p className="text-xs text-gray-500 mt-1">
            {service
              ? "A service account without roles can do nothing."
              : "Without roles the token can do everything you can."}
          </p>
        )}
        {roles.map((r, i) => (
          <div key={i} className="mt-2 flex items-center gap-2">
            <select
              aria-label="Token role"
              className="px-2 py-1 border border-gray-300 rounded-lg text-sm bg-white"
              value={r.role}
              onChange={(e) => setRoles(roles.map((x, j) => (j === i ? { ...x, role: e.target.value } : x)))}
            >
              {roleChoices.map((c) => (
                <option key={c} value={c}>
                  {roleLabel(c)}
                </option>
              ))}
            </select>
            {isProjectRole(r.role) && !service && (
              <select
                aria-label="Role project"
                className="px-2 py-1 border border-gray-300 rounded-lg text-sm bg-white"
                value={r.project}
                onChange={(e) => setRoles(roles.map((x, j) => (j === i ? { ...x, project: e.target.value } : x)))}
              >
                {projects.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
              </select>
            )}
            {!isProjectRole(r.role) && <span className="text-xs text-gray-500">on the host</span>}
            <button type="button" className={dangerLink} onClick={() => setRoles(roles.filter((_, j) => j !== i))}>
              Remove
            </button>
          </div>
        ))}
      </div>
      <div className="flex justify-end gap-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm text-gray-700 hover:bg-gray-100 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primaryButton} disabled={busy}>
          {busy ? "Creating..." : "Create Token"}
        </button>
      </div>
    </form>
  );
}

/** The new token's secret, shown this once. */
function SecretBox({ created, onClose }: { created: CreatedToken; onClose: () => void }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="space-y-4">
      <div className="p-3 bg-amber-50 border border-amber-200 rounded-lg text-sm text-amber-900">
        Copy this token now. It is shown <strong>only once</strong> and can't be recovered; anyone who has it can act
        as this token until it expires or is revoked. Store it in a secret manager, not in scripts or chat.
      </div>
      <div className="flex items-center gap-2">
        <input
          aria-label="New token"
          readOnly
          className="flex-1 px-3 py-2 font-mono text-sm border border-gray-300 rounded-lg bg-gray-50"
          value={created.token}
          onFocus={(e) => e.target.select()}
        />
        <button
          className={secondaryButton}
          onClick={() =>
            navigator.clipboard
              ?.writeText(created.token)
              .then(() => setCopied(true))
              .catch(() => setCopied(false))
          }
        >
          {copied ? "Copied" : "Copy"}
        </button>
      </div>
      <p className="text-xs text-gray-500">
        Send it as <code>Authorization: Bearer …</code>. Expires{" "}
        {formatTimestamp(created.record.expires_at)}.
      </p>
      <div className="flex justify-end">
        <button className={primaryButton} onClick={onClose}>
          Done
        </button>
      </div>
    </div>
  );
}

export default function Tokens() {
  const { me, projectName } = useSession();
  const [tokens, setTokens] = useState<ApiToken[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [created, setCreated] = useState<CreatedToken | null>(null);

  const refresh = useCallback(() => {
    api
      .listTokens()
      .then((t) => setTokens(t.sort((a, b) => tsSecs(b.created_at) - tsSecs(a.created_at))))
      .catch((e) => setError(errorMessage(e)));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const revoke = async (t: ApiToken) => {
    if (!confirm(`Revoke token ${t.name}? Anything using it stops working.`)) return;
    try {
      await api.revokeToken(t.id);
      refresh();
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const now = Date.now() / 1000;

  return (
    <div>
      <PageHeader title="Access Tokens" subtitle="For gxctl and automation. A personal token can never do more than you can.">
        <button className={primaryButton} onClick={() => setCreating(true)}>
          + Create Token
        </button>
      </PageHeader>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {tokens === null ? (
        <LoadingCard />
      ) : tokens.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">No tokens.</div>
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500 border-b border-gray-100">
                <th className="px-4 py-3 font-medium">Name</th>
                <th className="px-4 py-3 font-medium">Kind</th>
                <th className="px-4 py-3 font-medium">Roles</th>
                <th className="px-4 py-3 font-medium">Expires</th>
                <th className="px-4 py-3 font-medium">Last used</th>
                <th className="px-4 py-3" />
              </tr>
            </thead>
            <tbody>
              {tokens.map((t) => (
                <tr key={t.id} className="border-b border-gray-50">
                  <td className="px-4 py-3">
                    <div className="font-medium text-gray-900">{t.name}</div>
                    {(t.clusters ?? []).length > 0 && (
                      <div className="text-xs text-gray-500">
                        {t.clusters?.includes("all") ? "on every cluster its tenant uses" : `on ${t.clusters?.length} clusters`}
                      </div>
                    )}
                    <code className="text-xs text-gray-400 font-mono">{t.id}</code>
                  </td>
                  <td className="px-4 py-3">
                    {isServiceToken(t) ? (
                      <>
                        <Badge kind="role">service account</Badge>{" "}
                        <span className="text-xs text-gray-500">{projectName(t.project)}</span>
                      </>
                    ) : (
                      <>
                        <Badge kind="muted">personal</Badge>
                        {t.owner && t.owner !== me.user?.id && (
                          <span className="ml-1 text-xs text-gray-500">of {t.owner.slice(0, 8)}</span>
                        )}
                      </>
                    )}
                  </td>
                  <td className="px-4 py-3">
                    {(t.roles ?? []).length === 0 ? (
                      <span className="text-xs text-gray-500">{isServiceToken(t) ? "none" : "as owner"}</span>
                    ) : (
                      <div className="flex flex-wrap gap-1">
                        {(t.roles ?? []).map((r) => (
                          <Badge key={r.id} kind="role">
                            {roleLabel(r.role)}
                            {r.resource?.type === "Project" ? ` @ ${projectName(r.resource.id)}` : ""}
                          </Badge>
                        ))}
                      </div>
                    )}
                  </td>
                  <td className={`px-4 py-3 ${t.expires_at && tsSecs(t.expires_at) < now ? "text-red-600" : "text-gray-600"}`}>
                    {formatTimestamp(t.expires_at)}
                  </td>
                  <td className="px-4 py-3 text-gray-600">
                    {t.last_used_at ? (
                      <>
                        {formatTimestamp(t.last_used_at)}
                        {t.last_used_from && <div className="text-xs text-gray-400">{t.last_used_from}</div>}
                      </>
                    ) : (
                      "never"
                    )}
                  </td>
                  <td className="px-4 py-3 text-right">
                    <button className={dangerLink} onClick={() => revoke(t)}>
                      Revoke
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {creating && (
        <Modal title="Create Access Token" onClose={() => setCreating(false)}>
          <CreateTokenForm
            onCancel={() => setCreating(false)}
            onCreated={(t) => {
              setCreating(false);
              setCreated(t);
              refresh();
            }}
          />
        </Modal>
      )}
      {created && (
        <Modal title="Token created" onClose={() => setCreated(null)}>
          <SecretBox created={created} onClose={() => setCreated(null)} />
        </Modal>
      )}
    </div>
  );
}
