import { useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import type { Team } from "../types";
import { LoadingCard } from "../components/Loading";
import { Card, ErrorBanner, Field, PageHeader, errorMessage, inputClass, primaryButton, selectClass } from "../components/ui";
import { useSession } from "../session";

/** The form's view of the settings: numbers as text, so a field can be empty. */
interface Form {
  singular: string;
  plural: string;
  enabled: boolean;
  everyone: boolean;
  teams: string[];
  maxPerUser: string;
  maxTotal: string;
  approval: boolean;
  slugPrefix: string;
  adminsMayDelete: boolean;
  autoCreate: boolean;
}

function toForm(s: api.DirectorySettings): Form {
  const ss = s.self_service ?? {};
  return {
    singular: s.tenant_label?.singular || "Tenant",
    plural: s.tenant_label?.plural || "Tenants",
    enabled: !!ss.enabled,
    everyone: !(ss.creator_teams ?? []).length,
    teams: ss.creator_teams ?? [],
    maxPerUser: String(ss.max_per_user ?? 3),
    maxTotal: ss.max_total === undefined ? "" : String(ss.max_total),
    approval: !!ss.approval_required,
    slugPrefix: ss.slug_prefix ?? "",
    adminsMayDelete: ss.admins_may_delete ?? true,
    autoCreate: !!ss.auto_create,
  };
}

function fromForm(f: Form): api.DirectorySettings {
  return {
    tenant_label: { singular: f.singular.trim(), plural: f.plural.trim() },
    self_service: {
      enabled: f.enabled,
      creator_teams: f.everyone ? [] : f.teams,
      max_per_user: Number(f.maxPerUser),
      max_total: f.maxTotal.trim() === "" ? undefined : Number(f.maxTotal),
      approval_required: f.approval,
      slug_prefix: f.slugPrefix.trim(),
      admins_may_delete: f.adminsMayDelete,
      auto_create: f.autoCreate,
    },
  };
}

/** The directory's settings (spec/19-tenancy.md §11.1): what the web UI calls
 * a tenant, and whether its users may create their own. */
export default function DirectorySettings() {
  const { host, refresh } = useSession();
  const [form, setForm] = useState<Form | null>(null);
  const [teams, setTeams] = useState<Team[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [busy, setBusy] = useState(false);
  const editable = host.updateDirectorySettings;

  useEffect(() => {
    api
      .getDirectorySettings()
      .then((s) => setForm(toForm(s)))
      .catch((e) => setError(errorMessage(e)));
    if (host.listTeams) api.listTeams().then(setTeams).catch(() => setTeams([]));
  }, [host.listTeams]);

  const set = (patch: Partial<Form>) => {
    setSaved(false);
    setForm((f) => (f ? { ...f, ...patch } : f));
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!form) return;
    setBusy(true);
    setError(null);
    try {
      setForm(toForm(await api.updateDirectorySettings(fromForm(form))));
      setSaved(true);
      // The header and pages name tenants with the label from whoami.
      await refresh();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  if (!form) {
    return (
      <div>
        <PageHeader title="Settings" />
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        {!error && <LoadingCard />}
      </div>
    );
  }
  const teamName = (id: string) => teams.find((t) => t.id === id)?.name ?? id;
  return (
    <div>
      <PageHeader title="Settings" subtitle="What this directory's web UI calls a tenant, and whether its users may create their own." />
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <form onSubmit={submit} className="space-y-6" data-testid="directory-settings">
        <fieldset disabled={!editable || busy} className="space-y-6">
          <Card title="Display label">
            <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
              <Field label="One" htmlFor="label-singular">
                <input
                  id="label-singular"
                  className={inputClass}
                  required
                  maxLength={24}
                  value={form.singular}
                  onChange={(e) => set({ singular: e.target.value })}
                />
              </Field>
              <Field label="Many" htmlFor="label-plural">
                <input
                  id="label-plural"
                  className={inputClass}
                  required
                  maxLength={24}
                  value={form.plural}
                  onChange={(e) => set({ plural: e.target.value })}
                />
              </Field>
            </div>
            <p className="mt-2 text-xs text-gray-500">
              The web UI uses it for this directory's users, for example <em>Lab</em> and <em>Labs</em>. The API, gxctl and the
              docs keep saying tenant.
            </p>
          </Card>
          <Card title="Self-service">
            <div className="space-y-4">
              <label className="flex items-start gap-2 text-sm text-gray-700">
                <input
                  id="self-service-enabled"
                  type="checkbox"
                  className="mt-1"
                  checked={form.enabled}
                  onChange={(e) => set({ enabled: e.target.checked })}
                />
                <span>
                  Users may create {form.plural.toLowerCase()} of their own and become their administrators. Turning this off
                  stops new ones; existing ones stay.
                </span>
              </label>
              <Field label="Who may create one" htmlFor="self-service-creators">
                <select
                  id="self-service-creators"
                  className={selectClass}
                  value={form.everyone ? "all" : "teams"}
                  onChange={(e) => set({ everyone: e.target.value === "all" })}
                >
                  <option value="all">Every user of the directory</option>
                  <option value="teams">Members of these teams</option>
                </select>
              </Field>
              {!form.everyone && (
                <div className="flex flex-wrap gap-3" aria-label="Creator teams">
                  {teams.length === 0 && form.teams.length === 0 && <span className="text-sm text-gray-500">No teams yet.</span>}
                  {[...new Set([...teams.map((t) => t.id), ...form.teams])].map((id) => (
                    <label key={id} className="flex items-center gap-1 text-sm text-gray-700">
                      <input
                        type="checkbox"
                        checked={form.teams.includes(id)}
                        onChange={(e) => set({ teams: e.target.checked ? [...form.teams, id] : form.teams.filter((t) => t !== id) })}
                      />
                      {teamName(id)}
                    </label>
                  ))}
                </div>
              )}
              <div className="grid grid-cols-1 md:grid-cols-3 gap-3">
                <Field label="Per user" htmlFor="self-service-per-user">
                  <input
                    id="self-service-per-user"
                    type="number"
                    min={0}
                    max={100}
                    required
                    className={inputClass}
                    value={form.maxPerUser}
                    onChange={(e) => set({ maxPerUser: e.target.value })}
                  />
                </Field>
                <Field label="In all (empty: no limit)" htmlFor="self-service-total">
                  <input
                    id="self-service-total"
                    type="number"
                    min={0}
                    className={inputClass}
                    value={form.maxTotal}
                    onChange={(e) => set({ maxTotal: e.target.value })}
                  />
                </Field>
                <Field label="Slug prefix" htmlFor="self-service-prefix">
                  <input
                    id="self-service-prefix"
                    className={inputClass}
                    pattern="[a-z0-9][a-z0-9\-]{0,15}"
                    title="up to 16 lowercase letters, digits and dashes"
                    placeholder="lab-"
                    value={form.slugPrefix}
                    onChange={(e) => set({ slugPrefix: e.target.value })}
                  />
                </Field>
              </div>
              <label className="flex items-start gap-2 text-sm text-gray-700">
                <input
                  id="self-service-approval"
                  type="checkbox"
                  className="mt-1"
                  checked={form.approval}
                  onChange={(e) => set({ approval: e.target.checked })}
                />
                <span>A directory administrator approves each new one before it can be used.</span>
              </label>
              <label className="flex items-start gap-2 text-sm text-gray-700">
                <input
                  id="self-service-delete"
                  type="checkbox"
                  className="mt-1"
                  checked={form.adminsMayDelete}
                  onChange={(e) => set({ adminsMayDelete: e.target.checked })}
                />
                <span>Their administrators may delete them once they hold nothing.</span>
              </label>
              <label className="flex items-start gap-2 text-sm text-gray-700">
                <input
                  id="self-service-auto-create"
                  type="checkbox"
                  className="mt-1"
                  checked={form.autoCreate}
                  onChange={(e) => set({ autoCreate: e.target.checked })}
                />
                <span>
                  A user who is a member of none gets one of their own when they first sign in, named after them (once; it counts
                  toward their limit).
                </span>
              </label>
              <p className="text-xs text-gray-500">
                A new one can hold projects; it runs VMs on a cluster once that cluster admits it: by hand, or automatically with a default cap (its Admissions page).
              </p>
            </div>
          </Card>
        </fieldset>
        {editable && (
          <div className="flex items-center justify-end gap-3">
            {saved && <span className="text-sm text-green-700">Saved.</span>}
            <button type="submit" className={primaryButton} disabled={busy}>
              {busy ? "Saving..." : "Save"}
            </button>
          </div>
        )}
      </form>
    </div>
  );
}
