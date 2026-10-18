import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import type { Connector, ConnectorInput, ConnectorTestStep, Tenant } from "../types";
import { formatTimestamp } from "../types";
import Modal from "../components/Modal";
import ConfirmDialog from "../components/ConfirmDialog";
import { Loading, LoadingCard } from "../components/Loading";
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

/** The secret field each kind has (spec/rest-api.md: `oidc`: `client_secret`). */
const SECRET_FIELD: Record<Connector["kind"], string | null> = {
  pam: null,
  oidc: "client_secret",
  google: "client_secret",
  jumpcloud: "client_secret",
};

/** Every secret a connector has: a `google` one's device client has its own. */
const secretFields = (c: Connector): string[] => {
  const main = SECRET_FIELD[c.kind];
  const out = main ? [main] : [];
  if (c.kind === "google" && c.google?.device_client_id) out.push("device_client_secret");
  return out;
};

const KIND_LABEL: Record<Connector["kind"], string> = { oidc: "OIDC", pam: "PAM", google: "Google", jumpcloud: "JumpCloud" };

/** Where the IdP sends the browser back: what to register with it. */
const callbackUri = (id: string) => `${window.location.origin}/api/auth/c/${id || "<id>"}/callback`;

/** Read-only connectors come from control-plane.json. */
const fromFile = (c: Connector) => c.source === "file";

function secretSet(c: Connector): boolean | null {
  const fields = secretFields(c);
  if (fields.length === 0) return null;
  return fields.every((f) => !!c.secrets?.find((s) => s.field === f)?.set);
}

/** Comma- or space-separated words. */
const words = (s: string) => s.split(/[\s,]+/).filter(Boolean);

/** `group = team` lines. */
function parseGroupTeams(s: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of s.split("\n")) {
    const m = line.match(/^\s*([^=:]+?)\s*[=:]\s*(.+?)\s*$/);
    if (m) out[m[1]] = m[2];
  }
  return out;
}

const showGroupTeams = (m?: Record<string, string>) =>
  Object.entries(m ?? {})
    .map(([g, t]) => `${g} = ${t}`)
    .join("\n");

/** The form's fields as text, from a connector or blank. */
interface FormState {
  kind: Connector["kind"];
  id: string;
  display_name: string;
  enabled: boolean;
  jit: boolean;
  required_groups: string;
  /** "" leaves it to the server (on for a guarded OIDC connector, D22). */
  require_mfa: "" | "on" | "off";
  group_teams: string;
  default_tenant: string;
  token_max_days: string;
  allowed_groups: string;
  issuer: string;
  client_id: string;
  scopes: string;
  groups_claim: string;
  allowed_domains: string;
  redirect_uri: string;
  mfa_acr_values: string;
  /** `google`: Workspace by `hd`, or any account for invited and listed people. */
  google_accounts: "workspace" | "any";
  domains: string;
  allowed_emails: string;
  device_client_id: string;
  trust_2sv: boolean;
}

function formState(c?: Connector): FormState {
  return {
    kind: c?.kind ?? "oidc",
    id: c?.id ?? "",
    display_name: c?.display_name ?? "",
    enabled: c ? !!c.enabled : true,
    jit: !!c?.jit,
    required_groups: (c?.required_groups ?? []).join(", "),
    require_mfa: c ? (c.require_mfa ? "on" : "off") : "",
    group_teams: showGroupTeams(c?.group_teams),
    default_tenant: c?.default_tenant ?? "",
    token_max_days: c?.token_max_days ? String(c.token_max_days) : "",
    allowed_groups: (c?.pam?.allowed_groups ?? []).join(", "),
    issuer: c?.oidc?.issuer ?? "",
    client_id: c?.oidc?.client_id ?? c?.google?.client_id ?? c?.jumpcloud?.client_id ?? "",
    scopes: (c?.oidc?.scopes ?? []).join(" "),
    groups_claim: c?.oidc?.groups_claim ?? c?.jumpcloud?.groups_claim ?? "",
    allowed_domains: (c?.oidc?.allowed_domains ?? []).join(", "),
    redirect_uri: c?.oidc?.redirect_uri ?? c?.google?.redirect_uri ?? c?.jumpcloud?.redirect_uri ?? "",
    mfa_acr_values: (c?.oidc?.mfa_acr_values ?? []).join(" "),
    google_accounts: c?.google?.accounts === "GOOGLE_ACCOUNTS_ANY" ? "any" : "workspace",
    domains: (c?.google?.domains ?? []).join(", "),
    allowed_emails: (c?.google?.allowed_emails ?? c?.jumpcloud?.allowed_emails ?? []).join(", "),
    device_client_id: c?.google?.device_client_id ?? "",
    trust_2sv: !!c?.google?.trust_2sv,
  };
}

/** The connector a form describes, as POST /connectors takes it. */
function toInput(f: FormState): ConnectorInput {
  const c: ConnectorInput = {
    id: f.id.trim(),
    display_name: f.display_name.trim(),
    enabled: f.enabled,
    jit: f.jit,
    required_groups: words(f.required_groups),
    group_teams: parseGroupTeams(f.group_teams),
    default_tenant: f.default_tenant.trim() || null,
    token_max_days: f.token_max_days.trim() ? Number(f.token_max_days) : null,
  };
  if (f.require_mfa) c.require_mfa = f.require_mfa === "on";
  if (f.kind === "pam") {
    c.pam = { allowed_groups: words(f.allowed_groups) };
  } else if (f.kind === "google") {
    const workspace = f.google_accounts === "workspace";
    c.google = {
      accounts: workspace ? "GOOGLE_ACCOUNTS_WORKSPACE" : "GOOGLE_ACCOUNTS_ANY",
      client_id: f.client_id.trim(),
      domains: workspace ? words(f.domains) : [],
      allowed_emails: words(f.allowed_emails),
      trust_2sv: workspace && f.trust_2sv,
    };
    if (f.device_client_id.trim()) c.google.device_client_id = f.device_client_id.trim();
    if (f.redirect_uri.trim()) c.google.redirect_uri = f.redirect_uri.trim();
  } else if (f.kind === "jumpcloud") {
    c.jumpcloud = { client_id: f.client_id.trim(), allowed_emails: words(f.allowed_emails) };
    if (f.groups_claim.trim()) c.jumpcloud.groups_claim = f.groups_claim.trim();
    if (f.redirect_uri.trim()) c.jumpcloud.redirect_uri = f.redirect_uri.trim();
  } else {
    c.oidc = {
      issuer: f.issuer.trim(),
      client_id: f.client_id.trim(),
      scopes: words(f.scopes),
      groups_claim: f.groups_claim.trim(),
      allowed_domains: words(f.allowed_domains),
      mfa_acr_values: words(f.mfa_acr_values),
    };
    if (f.redirect_uri.trim()) c.oidc.redirect_uri = f.redirect_uri.trim();
  }
  return c;
}

/** The fields of `next` that differ from `prev`: a PATCH changes only those. */
function changes(prev: ConnectorInput, next: ConnectorInput): Partial<ConnectorInput> {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(next)) {
    if (k === "id") continue;
    const before = (prev as Record<string, unknown>)[k];
    if (JSON.stringify(v ?? null) !== JSON.stringify(before ?? null)) out[k] = v;
  }
  return out as Partial<ConnectorInput>;
}

function ConnectorForm({
  connector,
  tenants,
  onDone,
  onCancel,
}: {
  /** The connector edited; none: a new one. */
  connector?: Connector;
  tenants: Tenant[];
  onDone: () => void;
  onCancel: () => void;
}) {
  const [f, setF] = useState<FormState>(() => formState(connector));
  // A new OIDC connector's client secret, set right after it is created;
  // never kept beyond that.
  const [secret, setSecret] = useState("");
  const [deviceSecret, setDeviceSecret] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const set = <K extends keyof FormState>(k: K, v: FormState[K]) => setF((s) => ({ ...s, [k]: v }));
  const editing = !!connector;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const next = toInput(f);
      // A new connector's redirect URI is the one shown: this page's
      // address, unless one was typed (the server's own default is its
      // first allowed origin, which may not be the name used here).
      if (!connector && f.kind !== "pam" && !f.redirect_uri.trim()) {
        const uri = callbackUri(f.id.trim());
        if (next.oidc) next.oidc.redirect_uri = uri;
        if (next.google) next.google.redirect_uri = uri;
        if (next.jumpcloud) next.jumpcloud.redirect_uri = uri;
      }
      if (connector) {
        const patch = changes(toInput(formState(connector)), next);
        if (Object.keys(patch).length > 0) await api.updateConnector(connector.id, patch);
      } else {
        const created = await api.createConnector(next);
        const field = SECRET_FIELD[created.kind];
        if (field && secret) {
          const value = secret;
          setSecret("");
          await api.setConnectorSecret(created.id, field, value);
        }
        if (created.kind === "google" && deviceSecret) {
          const value = deviceSecret;
          setDeviceSecret("");
          await api.setConnectorSecret(created.id, "device_client_secret", value);
        }
      }
      onDone();
    } catch (err) {
      setSecret("");
      setDeviceSecret("");
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  const text = (k: keyof FormState, label: string, opts: { required?: boolean; placeholder?: string; hint?: string } = {}) => (
    <Field label={label} htmlFor={`connector-${k}`}>
      <input
        id={`connector-${k}`}
        className={inputClass}
        required={opts.required}
        placeholder={opts.placeholder}
        value={f[k] as string}
        onChange={(e) => set(k, e.target.value as never)}
      />
      {opts.hint && <p className="mt-1 text-xs text-gray-500">{opts.hint}</p>}
    </Field>
  );

  const secretInput = (id: string, label: string, value: string, setValue: (v: string) => void) => (
    <Field label={`${label} (optional)`} htmlFor={id}>
      <input id={id} type="password" autoComplete="new-password" className={inputClass} value={value} onChange={(e) => setValue(e.target.value)} />
      <p className="mt-1 text-xs text-gray-500">Write-only: it is sealed on the server and never shown again.</p>
    </Field>
  );

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600 whitespace-pre-wrap">{error}</p>}
      {!editing && (
        <div className="grid grid-cols-2 gap-3">
          <Field label="Kind" htmlFor="connector-kind">
            <select id="connector-kind" className={selectClass} value={f.kind} onChange={(e) => set("kind", e.target.value as Connector["kind"])}>
              <option value="oidc">OIDC</option>
              <option value="google">Google</option>
              <option value="jumpcloud">JumpCloud</option>
              <option value="pam">PAM</option>
            </select>
          </Field>
          <Field label="ID" htmlFor="connector-id">
            <input
              id="connector-id"
              className={`${inputClass} font-mono`}
              required
              pattern="[a-z0-9\-]{1,48}"
              title="1-48 lowercase letters, digits and dashes"
              value={f.id}
              onChange={(e) => set("id", e.target.value)}
            />
          </Field>
        </div>
      )}
      {text("display_name", "Display name", { hint: "Shown on the login page." })}
      <div className="flex flex-wrap gap-x-6 gap-y-2 text-sm text-gray-700">
        <label className="flex items-center gap-2">
          <input type="checkbox" checked={f.enabled} onChange={(e) => set("enabled", e.target.checked)} />
          Enabled
        </label>
        <label className="flex items-center gap-2">
          <input type="checkbox" checked={f.jit} onChange={(e) => set("jit", e.target.checked)} />
          Create users at their first login
        </label>
      </div>
      <Field label="Require MFA" htmlFor="connector-mfa">
        <select id="connector-mfa" className={selectClass} value={f.require_mfa} onChange={(e) => set("require_mfa", e.target.value as FormState["require_mfa"])}>
          {!editing && <option value="">Server default</option>}
          <option value="on">Required</option>
          <option value="off">Not required</option>
        </select>
      </Field>

      {f.kind !== "pam" && (
        <div className="p-3 rounded-lg bg-sky-50 border border-sky-100 text-xs text-sky-900 space-y-2" data-testid="redirect-uri">
          <p>Register this redirect URI with the identity provider:</p>
          <code className="block font-mono break-all">
            {f.redirect_uri.trim() || (editing ? "the server's default (its first allowed origin)" : callbackUri(f.id.trim()))}
          </code>
          <input
            id="connector-redirect_uri"
            aria-label="Redirect URI"
            className={`${inputClass} font-mono text-xs`}
            placeholder={editing ? "the server's default" : callbackUri(f.id.trim())}
            value={f.redirect_uri}
            onChange={(e) => set("redirect_uri", e.target.value)}
          />
          {!editing && <p className="text-sky-800">Empty: this page's address, as above.</p>}
        </div>
      )}
      {f.kind === "pam" ? (
        text("allowed_groups", "Allowed Unix groups", { placeholder: "any", hint: "Comma-separated; empty lets any account in." })
      ) : f.kind === "google" ? (
        <>
          <Field label="Google accounts" htmlFor="connector-google-accounts">
            <select
              id="connector-google-accounts"
              className={selectClass}
              value={f.google_accounts}
              onChange={(e) => set("google_accounts", e.target.value as FormState["google_accounts"])}
            >
              <option value="workspace">Google Workspace (by domain)</option>
              <option value="any">Any Google account (invited or listed people)</option>
            </select>
          </Field>
          {f.google_accounts === "workspace" ? (
            <>
              {text("domains", "Workspace domains", {
                required: true,
                placeholder: "example.org",
                hint: "Comma-separated. A login is allowed by the account's Workspace domain (hd), never by its email address.",
              })}
              <label className="flex items-start gap-2 text-sm text-gray-700">
                <input id="connector-trust-2sv" type="checkbox" className="mt-1" checked={f.trust_2sv} onChange={(e) => set("trust_2sv", e.target.checked)} />
                <span>
                  This Workspace enforces 2-step verification: count its logins as MFA.
                  <span className="block text-xs text-gray-500">Google doesn't say whether a login used a second factor; only check this if the Workspace's admin console enforces it.</span>
                </span>
              </label>
              <p className="text-xs text-gray-500">On Google's side, set the OAuth consent screen's user type to Internal, so only the Workspace's accounts can use the client.</p>
            </>
          ) : (
            <>
              {text("allowed_emails", "Allowed emails", {
                placeholder: "bob@gmail.com",
                hint: "Comma-separated. With “Create users” on, only these addresses get an account; otherwise invite people from a tenant's page.",
              })}
              <p className="text-xs text-amber-700" data-testid="consumer-warning">
                Anyone can open a Google account. Without allowed emails, turn off “Create users” and invite people.
              </p>
            </>
          )}
          {text("client_id", "Client ID (web application)", { required: true })}
          {!editing && secretInput("connector-secret", "Client secret", secret, setSecret)}
          {text("device_client_id", "Device client ID (optional)", {
            hint: "For gxctl logins: a separate OAuth client of type “TVs and Limited Input devices”.",
          })}
          {!editing && f.device_client_id.trim() && secretInput("connector-device-secret", "Device client secret", deviceSecret, setDeviceSecret)}
        </>
      ) : f.kind === "jumpcloud" ? (
        <>
          <p className="text-xs text-gray-600">
            In JumpCloud, set the application's Client Authentication Type to <strong>Client Secret Basic</strong>, and its Login URL to{" "}
            <code className="font-mono break-all">{`${window.location.origin}/api/auth/c/${f.id.trim() || "<id>"}/start`}</code>.
          </p>
          {text("client_id", "Client ID", { required: true })}
          {!editing && secretInput("connector-secret", "Client secret", secret, setSecret)}
          {text("groups_claim", "Groups claim", {
            placeholder: "memberOf",
            hint: "The name set under the application's “Include group attribute” in JumpCloud.",
          })}
          {text("allowed_emails", "Allowed emails", { hint: "Comma-separated. With “Create users” on, only these addresses get an account." })}
        </>
      ) : (
        <>
          {text("issuer", "Issuer", { required: true, placeholder: "https://idp.example.com" })}
          {text("client_id", "Client ID", { required: true })}
          {!editing && secretInput("connector-secret", "Client secret", secret, setSecret)}
          <div className="grid grid-cols-2 gap-3">
            {text("scopes", "Scopes", { placeholder: "openid profile email" })}
            {text("groups_claim", "Groups claim", { placeholder: "groups" })}
          </div>
          {text("allowed_domains", "Allowed email domains", { hint: "Comma-separated; empty allows any." })}
          {text("mfa_acr_values", "MFA acr values", { hint: "Space-separated values that count as MFA." })}
        </>
      )}

      {text("required_groups", "Required groups", {
        hint: "Comma-separated; a login needs one of them. Needed for JIT into the owner or an operator tenant.",
      })}
      <Field label="Group → team" htmlFor="connector-group-teams">
        <textarea
          id="connector-group-teams"
          className={`${inputClass} font-mono text-sm`}
          rows={3}
          placeholder="group = team"
          value={f.group_teams}
          onChange={(e) => set("group_teams", e.target.value)}
        />
      </Field>
      <div className="grid grid-cols-2 gap-3">
        <Field label="Default tenant" htmlFor="connector-tenant">
          {tenants.length > 0 ? (
            <select id="connector-tenant" className={selectClass} value={f.default_tenant} onChange={(e) => set("default_tenant", e.target.value)}>
              <option value="">None</option>
              {tenants.map((t) => (
                <option key={t.id} value={t.id}>
                  {t.slug}
                </option>
              ))}
              {f.default_tenant && !tenants.some((t) => t.id === f.default_tenant) && (
                <option value={f.default_tenant}>{f.default_tenant}</option>
              )}
            </select>
          ) : (
            <input id="connector-tenant" className={inputClass} placeholder="tenant id" value={f.default_tenant} onChange={(e) => set("default_tenant", e.target.value)} />
          )}
        </Field>
        <Field label="Longest token (days)" htmlFor="connector-days">
          <input
            id="connector-days"
            className={inputClass}
            type="number"
            min={1}
            placeholder="30"
            value={f.token_max_days}
            onChange={(e) => set("token_max_days", e.target.value)}
          />
        </Field>
      </div>
      <div className="flex justify-end gap-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm text-gray-700 hover:bg-gray-100 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primaryButton} disabled={busy}>
          {busy ? "Saving..." : editing ? "Save" : "Create Connector"}
        </button>
      </div>
    </form>
  );
}

/** Sets a write-only secret; the input is cleared once it is sent. */
function SecretForm({ connector, field, onDone, onCancel }: { connector: Connector; field: string; onDone: () => void; onCancel: () => void }) {
  const [value, setValue] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const v = value;
    setValue("");
    setBusy(true);
    setError(null);
    try {
      await api.setConnectorSecret(connector.id, field, v);
      onDone();
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <Field label={field.replace(/_/g, " ")} htmlFor="connector-secret-value">
        <input
          id="connector-secret-value"
          type="password"
          autoComplete="new-password"
          className={inputClass}
          required
          value={value}
          onChange={(e) => setValue(e.target.value)}
        />
        <p className="mt-1 text-xs text-gray-500">
          Replaces the current value. It is sealed on the server and never shown again.
        </p>
      </Field>
      <div className="flex justify-end gap-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm text-gray-700 hover:bg-gray-100 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className={primaryButton} disabled={busy}>
          {busy ? "Saving..." : "Set secret"}
        </button>
      </div>
    </form>
  );
}

function TestResult({ connector }: { connector: Connector }) {
  const [steps, setSteps] = useState<ConnectorTestStep[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    api
      .testConnector(connector.id)
      .then((s) => live && setSteps(s))
      .catch((e) => live && setError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [connector.id]);
  if (error) return <ErrorBanner error={error} />;
  if (steps === null) return <Loading />;
  if (steps.length === 0) return <p className="text-sm text-gray-500">Nothing to check.</p>;
  return (
    <ul className="space-y-2 text-sm">
      {steps.map((s, i) => (
        <li key={i} className="flex items-start gap-2">
          <Badge kind={s.ok ? "ok" : "denied"}>{s.ok ? "ok" : "failed"}</Badge>
          <div>
            <div className="font-medium text-gray-900">{s.name}</div>
            {s.message && <div className="text-gray-600 whitespace-pre-wrap">{s.message}</div>}
          </div>
        </li>
      ))}
    </ul>
  );
}

type Dialog =
  | { kind: "create" }
  | { kind: "edit"; connector: Connector }
  | { kind: "secret"; connector: Connector; field: string }
  | { kind: "test"; connector: Connector }
  | { kind: "delete"; connector: Connector };

export default function Connectors() {
  const { host } = useSession();
  const [connectors, setConnectors] = useState<Connector[] | null>(null);
  const [tenants, setTenants] = useState<Tenant[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [dialog, setDialog] = useState<Dialog | null>(null);

  const refresh = useCallback(() => {
    api
      .listConnectors()
      .then((c) => setConnectors(c.sort((a, b) => a.id.localeCompare(b.id))))
      .catch((e) => setError(errorMessage(e)));
  }, []);

  useEffect(() => {
    if (!host.listConnectors) return;
    refresh();
    // For the default-tenant picker and names; not fatal if refused.
    api
      .listTenants()
      .then(setTenants)
      .catch(() => setTenants([]));
  }, [host.listConnectors, refresh]);

  if (!host.listConnectors) {
    return (
      <div>
        <PageHeader title="Login connectors" />
        <p className="text-gray-500">You can't see the directory's login connectors.</p>
      </div>
    );
  }

  const run = async (f: () => Promise<unknown>) => {
    setError(null);
    try {
      await f();
      refresh();
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  const tenantSlug = (id: string) => tenants.find((t) => t.id === id)?.slug ?? id;
  const close = () => setDialog(null);
  const done = () => {
    setDialog(null);
    refresh();
  };

  return (
    <div>
      <PageHeader title="Login connectors" subtitle="The identity providers and user sources the directory's users log in through.">
        {host.createConnector && (
          <button className={primaryButton} onClick={() => setDialog({ kind: "create" })}>
            + Add Connector
          </button>
        )}
      </PageHeader>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {connectors === null ? (
        <LoadingCard />
      ) : connectors.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">No login connectors.</div>
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500 border-b border-gray-100">
                <th className="px-4 py-3 font-medium">Connector</th>
                <th className="px-4 py-3 font-medium">Kind</th>
                <th className="px-4 py-3 font-medium">Status</th>
                <th className="px-4 py-3 font-medium">Source</th>
                <th className="px-4 py-3 font-medium">Secret</th>
                <th className="px-4 py-3 font-medium">Settings</th>
                <th className="px-4 py-3" />
              </tr>
            </thead>
            <tbody>
              {connectors.map((c) => {
                const readOnly = fromFile(c);
                const secret = secretSet(c);
                const fields = secretFields(c);
                return (
                  <tr key={c.id} className="border-b border-gray-50 hover:bg-gray-50 align-top">
                    <td className="px-4 py-3">
                      <div className="font-medium text-gray-900">{c.display_name || c.id}</div>
                      <code className="text-xs font-mono text-gray-500">{c.id}</code>
                    </td>
                    <td className="px-4 py-3">
                      <Badge kind="base">{KIND_LABEL[c.kind] ?? c.kind}</Badge>
                    </td>
                    <td className="px-4 py-3">{c.enabled ? <Badge kind="ok">enabled</Badge> : <Badge kind="muted">disabled</Badge>}</td>
                    <td className="px-4 py-3">
                      {readOnly ? (
                        <span title="Defined in control-plane.json; read-only here">
                          <Badge kind="file">file</Badge>
                        </span>
                      ) : (
                        <Badge kind="link">api</Badge>
                      )}
                    </td>
                    <td className="px-4 py-3">
                      {secret === null ? (
                        <span className="text-gray-400">—</span>
                      ) : secret ? (
                        <Badge kind="ok">set</Badge>
                      ) : (
                        <Badge kind="warn">NOT SET</Badge>
                      )}
                    </td>
                    <td className="px-4 py-3 text-xs text-gray-600 space-y-0.5">
                      {c.oidc && <div className="font-mono">{c.oidc.issuer}</div>}
                      {c.jumpcloud && <div>groups in {c.jumpcloud.groups_claim || "memberOf"}</div>}
                      {c.google && (
                        <div>
                          {c.google.accounts === "GOOGLE_ACCOUNTS_ANY"
                            ? "any Google account (invited or listed people)"
                            : `Workspace: ${(c.google.domains ?? []).join(", ")}`}
                        </div>
                      )}
                      {c.jit && <div>JIT{c.default_tenant ? ` into ${tenantSlug(c.default_tenant)}` : ""}</div>}
                      {c.require_mfa && <div>MFA required</div>}
                      {(c.required_groups?.length ?? 0) > 0 && <div>groups: {c.required_groups!.join(", ")}</div>}
                      {c.updated_at && <div className="text-gray-400">updated {formatTimestamp(c.updated_at)}</div>}
                    </td>
                    <td className="px-4 py-3 text-right whitespace-nowrap space-x-3">
                      {host.testConnector && (
                        <button className="text-sm text-sky-700 hover:underline" onClick={() => setDialog({ kind: "test", connector: c })}>
                          Test
                        </button>
                      )}
                      {!readOnly && host.updateConnector && (
                        <>
                          <button className="text-sm text-sky-700 hover:underline" onClick={() => setDialog({ kind: "edit", connector: c })}>
                            Edit
                          </button>
                          <button
                            className="text-sm text-sky-700 hover:underline"
                            onClick={() => run(() => api.updateConnector(c.id, { enabled: !c.enabled }))}
                          >
                            {c.enabled ? "Disable" : "Enable"}
                          </button>
                        </>
                      )}
                      {!readOnly &&
                        host.setConnectorSecret &&
                        fields.map((field) => (
                          <button
                            key={field}
                            className="text-sm text-sky-700 hover:underline"
                            onClick={() => setDialog({ kind: "secret", connector: c, field })}
                          >
                            {field === "device_client_secret" ? "Set device secret" : "Set secret"}
                          </button>
                        ))}
                      {!readOnly && host.deleteConnector && (
                        <button className={dangerLink} onClick={() => setDialog({ kind: "delete", connector: c })}>
                          Delete
                        </button>
                      )}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {dialog?.kind === "create" && (
        <Modal title="Add Connector" onClose={close}>
          <ConnectorForm tenants={tenants} onCancel={close} onDone={done} />
        </Modal>
      )}
      {dialog?.kind === "edit" && (
        <Modal title={`Edit ${dialog.connector.id}`} onClose={close}>
          <ConnectorForm connector={dialog.connector} tenants={tenants} onCancel={close} onDone={done} />
        </Modal>
      )}
      {dialog?.kind === "secret" && (
        <Modal title={`Set secret of ${dialog.connector.id}`} onClose={close}>
          <SecretForm connector={dialog.connector} field={dialog.field} onCancel={close} onDone={done} />
        </Modal>
      )}
      {dialog?.kind === "test" && (
        <Modal title={`Test ${dialog.connector.id}`} onClose={close}>
          <TestResult connector={dialog.connector} />
        </Modal>
      )}
      {dialog?.kind === "delete" && (
        <ConfirmDialog
          title="Delete Connector"
          consequence={
            <p>
              Nobody can log in through <span className="font-semibold">{dialog.connector.display_name || dialog.connector.id}</span>{" "}
              any more.
            </p>
          }
          typeToConfirm={dialog.connector.id}
          actionLabel="Delete"
          onConfirm={async () => {
            await api.deleteConnector(dialog.connector.id);
            refresh();
          }}
          onClose={close}
        />
      )}
    </div>
  );
}
