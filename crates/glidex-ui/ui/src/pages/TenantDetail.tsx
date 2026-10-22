import { useCallback, useEffect, useState, type FormEvent } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import * as api from "../api";
import type { Binding, EntityRef, Tenant, TenantBinding, TenantMember } from "../types";
import { TENANT_ROLES, entityLabel, formatTimestamp, isOidcKind } from "../types";
import Modal from "../components/Modal";
import ConfirmDialog from "../components/ConfirmDialog";
import AddBindingForm from "../components/AddBindingForm";
import FederationsCard from "../components/FederationsCard";
import { Loading } from "../components/Loading";
import {
  BindingTable,
  Card,
  type Directory,
  ErrorBanner,
  Field,
  dangerLink,
  errorMessage,
  inputClass,
  primaryButton,
  principalName,
  secondaryButton,
  selectClass,
  SignInMethods,
  userIdOf,
} from "../components/ui";
import { useCan } from "../session";
import { useConnectorNames, useDirectory, useNodeNames } from "../hooks";
import { AdmissionBadge, TenantFlags } from "./Tenants";

/** What the page offers, each checked on the tenant. */
const CAPS = [
  "updateTenant",
  "suspendTenant",
  "resumeTenant",
  "deleteTenant",
  "readTenantMembers",
  "manageTenantMembers",
  "readTenantBindings",
  "manageTenantBindings",
  "readTenantInvitations",
  "manageTenantInvitations",
  "readTenantDevicePools",
  "manageTenantDevicePools",
  "readTenantClusters",
  "manageTenantClusters",
  "readTenantFederations",
  "manageTenantFederations",
] as const;
type Cap = (typeof CAPS)[number];

/** The tenant's shares of cluster device pools, split among its projects
 *  (spec/host-topology.md §9.1a). */
/** The clusters of other organizations the tenant uses, and what each last
 *  reported of it (spec/tenancy.md D70). */
function ClustersCard({ tenant, canManage }: { tenant: Tenant; canManage: boolean }) {
  const [clusters, setClusters] = useState<api.TenantCluster[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [offer, setOffer] = useState("");
  const [busy, setBusy] = useState(false);
  const [leaving, setLeaving] = useState<api.TenantCluster | null>(null);
  const refresh = useCallback(() => {
    api
      .listTenantClusters(tenant.id)
      .then(setClusters)
      .catch((e) => setError(errorMessage(e)));
  }, [tenant.id]);
  useEffect(() => refresh(), [refresh]);
  // An offer from another organization's operator (D65).
  const optIn = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.optInCluster(tenant.id, offer.trim());
      setOffer("");
      refresh();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };
  if (clusters !== null && clusters.length === 0 && !error && !canManage) return null;
  return (
    <Card title="Clusters of other organizations">
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {canManage && (
        <form onSubmit={optIn} className="mb-4 flex flex-wrap items-end gap-2">
          <div className="flex-1 min-w-[16rem]">
            <Field label="Use a cluster: the offer its operator sent you" htmlFor="opt-in-offer">
              <input
                id="opt-in-offer"
                className={`${inputClass} font-mono`}
                required
                value={offer}
                onChange={(e) => setOffer(e.target.value)}
                placeholder="gxo_…"
                autoComplete="off"
              />
            </Field>
          </div>
          <button type="submit" className={primaryButton} disabled={busy || !offer.trim().startsWith("gxo_")}>
            Accept offer
          </button>
        </form>
      )}
      {clusters !== null && clusters.length === 0 && <p className="text-sm text-gray-500">This tenant uses no cluster of another organization.</p>}
      {clusters === null ? (
        <p className="text-sm text-gray-500">Loading…</p>
      ) : clusters.length === 0 ? null : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm" data-testid="tenant-clusters">
            <thead>
              <tr className="text-left text-xs text-gray-500">
                <th className="py-1 pr-3 font-medium">Cluster</th>
                <th className="py-1 pr-3 font-medium">Admission</th>
                <th className="py-1 pr-3 font-medium">Projects</th>
                <th className="py-1 pr-3 font-medium">VMs</th>
                <th className="py-1 pr-3 font-medium">Disks</th>
                <th className="py-1 pr-3 font-medium">Networks</th>
                <th className="py-1 font-medium">Reported</th>
                {canManage && <th className="py-1" />}
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {clusters.map((c) => {
                const r = c.report;
                return (
                  <tr key={c.cluster_id}>
                    <td className="py-1.5 pr-3">
                      <div className="text-gray-800">{c.name || c.cluster_id}</div>
                      <div className="text-xs text-gray-500 font-mono">{c.api}</div>
                    </td>
                    {r ? (
                      <>
                        <td className="py-1.5 pr-3">{r.status === "ADMISSION_STATUS_SUSPENDED" ? <span className="text-amber-700">suspended</span> : "admitted"}</td>
                        <td className="py-1.5 pr-3">{r.projects ?? 0}</td>
                        <td className="py-1.5 pr-3">
                          {r.vms ?? 0}
                          {Number(r.running_vms ?? 0) > 0 && <span className="text-xs text-gray-500"> ({r.running_vms} running)</span>}
                        </td>
                        <td className="py-1.5 pr-3">
                          {r.disks ?? 0}
                          {Number(r.disk_gib ?? 0) > 0 && <span className="text-xs text-gray-500"> ({r.disk_gib} GiB)</span>}
                        </td>
                        <td className="py-1.5 pr-3">{r.networks ?? 0}</td>
                        <td className="py-1.5 text-xs text-gray-500">{r.reported_at ? new Date(r.reported_at).toLocaleString() : "—"}</td>
                      </>
                    ) : (
                      <td colSpan={6} className="py-1.5 text-xs text-gray-500">
                        No report yet
                      </td>
                    )}
                    {canManage && (
                      <td className="py-1.5 text-right">
                        <button type="button" className={dangerLink} onClick={() => setLeaving(c)}>
                          Opt out
                        </button>
                      </td>
                    )}
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {leaving && (
        <ConfirmDialog
          title="Opt Out"
          consequence={
            <p>
              This tenant stops using <span className="font-semibold">{leaving.name || leaving.cluster_id}</span>: its people lose access there, and the cluster is
              told. What the tenant has there stays until that cluster's operator removes it.
            </p>
          }
          actionLabel="Opt out"
          onConfirm={async () => {
            await api.optOutCluster(tenant.id, leaving.cluster_id ?? "");
            refresh();
          }}
          onClose={() => setLeaving(null)}
        />
      )}
    </Card>
  );
}

function DevicePoolsCard({ tenant, canManage }: { tenant: Tenant; canManage: boolean }) {
  const [pools, setPools] = useState<api.TenantDevicePool[] | null>(null);
  const [projects, setProjects] = useState<{ id: string; name: string }[]>([]);
  const [name, setName] = useState("");
  const [from, setFrom] = useState("");
  const [max, setMax] = useState(1);
  const [chosen, setChosen] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .listTenantDevicePools(tenant.id)
      .then(setPools)
      .catch((e) => setError(errorMessage(e)));
  }, [tenant.id]);
  useEffect(() => refresh(), [refresh]);
  useEffect(() => {
    api
      .listProjects()
      .then((ps) => setProjects(ps.filter((p) => p.tenant_id === tenant.id).map((p) => ({ id: p.id, name: p.name }))))
      .catch(() => setProjects([]));
  }, [tenant.id]);
  const projectName = (id: string) => projects.find((p) => p.id === id)?.name ?? id;

  const save = async (next: api.TenantDevicePool[]) => {
    setError(null);
    try {
      setPools(await api.setTenantDevicePools(tenant.id, next));
      return true;
    } catch (e) {
      setError(errorMessage(e));
      return false;
    }
  };
  const add = async (e: FormEvent) => {
    e.preventDefault();
    const rest = (pools ?? []).filter((p) => p.name !== name.trim());
    if (await save([...rest, { name: name.trim(), from: from.trim(), max, projects: chosen }])) {
      setName("");
      setFrom("");
      setMax(1);
      setChosen([]);
    }
  };

  return (
    <Card title="Device pools">
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {pools === null ? (
        <Loading />
      ) : pools.length === 0 ? (
        <p className="text-sm text-gray-500">
          No tenant pools on this cluster. A tenant pool shares devices of a cluster pool the tenant is granted among
          some of its projects.
        </p>
      ) : (
        <table className="w-full text-sm">
          <thead>
            <tr className="text-left text-gray-500 border-b border-gray-100">
              <th className="py-2 font-medium">Pool</th>
              <th className="py-2 font-medium">From</th>
              <th className="py-2 font-medium">Held / max</th>
              <th className="py-2 font-medium">Free in cluster pool</th>
              <th className="py-2 font-medium">Projects</th>
              {canManage && <th />}
            </tr>
          </thead>
          <tbody>
            {pools.map((p) => (
              <tr key={p.name} className="border-b border-gray-50">
                <td className="py-2 font-mono">{p.name}</td>
                <td className="py-2 font-mono">
                  {p.from}
                  {p.ungranted && <span className="ml-2 text-xs text-red-600">ungranted</span>}
                </td>
                <td className="py-2">
                  {p.used ?? 0} / {p.max ?? 0}
                </td>
                <td className="py-2">{p.free ?? 0}</td>
                <td className="py-2">{(p.projects ?? []).map(projectName).join(", ") || "—"}</td>
                {canManage && (
                  <td className="py-2 text-right">
                    <button
                      className={dangerLink}
                      onClick={() => void save(pools.filter((o) => o.name !== p.name))}
                    >
                      Remove
                    </button>
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {canManage && (
        <form onSubmit={add} className="mt-4 grid grid-cols-1 sm:grid-cols-3 gap-3">
          <Field label="Name" htmlFor="tenant-pool-name">
            <input
              id="tenant-pool-name"
              className={inputClass}
              required
              pattern="[a-z0-9\-]{1,63}"
              title="1-63 lowercase letters, digits and dashes"
              value={name}
              onChange={(e) => setName(e.target.value)}
            />
          </Field>
          <Field label="From cluster pool" htmlFor="tenant-pool-from">
            <input id="tenant-pool-from" className={inputClass} required value={from} onChange={(e) => setFrom(e.target.value)} />
          </Field>
          <Field label="Max devices" htmlFor="tenant-pool-max">
            <input
              id="tenant-pool-max"
              className={inputClass}
              type="number"
              min={1}
              required
              value={max}
              onChange={(e) => setMax(Math.max(1, Number(e.target.value)))}
            />
          </Field>
          <div className="sm:col-span-3">
            <div className="text-sm font-medium text-gray-700">Projects</div>
            <div className="mt-1 flex flex-wrap gap-3">
              {projects.map((p) => (
                <label key={p.id} className="flex items-center gap-1 text-sm">
                  <input
                    type="checkbox"
                    checked={chosen.includes(p.id)}
                    onChange={(e) => setChosen(e.target.checked ? [...chosen, p.id] : chosen.filter((x) => x !== p.id))}
                  />
                  {p.name}
                </label>
              ))}
            </div>
          </div>
          <div className="sm:col-span-3">
            <button type="submit" className={primaryButton}>
              Add or replace pool
            </button>
          </div>
        </form>
      )}
    </Card>
  );
}

/** A tenant role link in the shape the shared binding table shows. */
function asBinding(tenant: string, b: TenantBinding): Binding {
  return {
    id: b.id,
    role: b.role,
    principal: b.principal,
    resource: { type: "Tenant", id: tenant },
    created_by: b.created_by,
    created_at: b.created_at,
    principal_name: b.principal_name || undefined,
  };
}

function EditTenantForm({ tenant, onDone, onCancel }: { tenant: Tenant; onDone: () => void; onCancel: () => void }) {
  const [displayName, setDisplayName] = useState(tenant.display_name);
  const [operator, setOperator] = useState(!!tenant.operator);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const req: { display_name?: string; operator?: boolean } = {};
    if (displayName !== tenant.display_name) req.display_name = displayName;
    if (operator !== !!tenant.operator) req.operator = operator;
    try {
      if (Object.keys(req).length > 0) await api.updateTenant(tenant.id, req);
      onDone();
    } catch (err) {
      setError(errorMessage(err));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <Field label="Display name" htmlFor="tenant-edit-name">
        <input id="tenant-edit-name" className={inputClass} value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
      </Field>
      <label className="flex items-start gap-2 text-sm text-gray-700">
        <input type="checkbox" className="mt-1" checked={operator} onChange={(e) => setOperator(e.target.checked)} />
        <span>Operator tenant</span>
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

/** User or team picker for a new member: from the directory when the caller
 * may list it, otherwise the id typed. */
function AddMemberForm({ dir, onAdd }: { dir: Directory | null; onAdd: (p: EntityRef) => Promise<void> }) {
  const [kind, setKind] = useState<"User" | "Team">("User");
  const [id, setId] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const options =
    dir && kind === "User"
      ? dir.users.filter((u) => !u.disabled).map((u) => ({ id: u.id, label: u.display_name }))
      : dir && kind === "Team"
        ? dir.teams.map((t) => ({ id: t.id, label: t.name }))
        : null;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!id.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await onAdd({ type: kind, id: id.trim() });
      setId("");
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="mt-4 grid grid-cols-1 md:grid-cols-3 gap-3 items-end">
      <div>
        <label className="block text-sm font-medium text-gray-700">Principal type</label>
        <select
          className={selectClass}
          value={kind}
          onChange={(e) => {
            setKind(e.target.value as "User" | "Team");
            setId("");
          }}
        >
          <option value="User">User</option>
          <option value="Team">Team</option>
        </select>
      </div>
      <div>
        <label className="block text-sm font-medium text-gray-700">{kind}</label>
        {options ? (
          <select className={selectClass} value={id} onChange={(e) => setId(e.target.value)} required>
            <option value="">Choose…</option>
            {options.map((o) => (
              <option key={o.id} value={o.id}>
                {o.label}
              </option>
            ))}
          </select>
        ) : (
          <input
            className={inputClass}
            placeholder={kind === "User" ? "user id" : "team id or unix:<group>"}
            value={id}
            onChange={(e) => setId(e.target.value)}
            required
          />
        )}
      </div>
      <div>
        <button type="submit" className={primaryButton} disabled={busy}>
          Add member
        </button>
      </div>
      {error && <p className="md:col-span-3 text-sm text-red-600">{error}</p>}
    </form>
  );
}

const INVITE_ROLES = ["member", ...TENANT_ROLES] as const;
const inviteRoleLabel = (r: string) => (r === "member" ? "member (no tenant role)" : r);

/** A tenant's invitations (spec/tenancy.md §6.1): a link, shown once, that
 *  lets one email address in through one connector. */
function InvitationsCard({ tenant, canManage }: { tenant: Tenant; canManage: boolean }) {
  const [list, setList] = useState<api.Invitation[] | null>(null);
  const [connectors, setConnectors] = useState<{ id: string; label: string }[]>([]);
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<string>("member");
  const [connector, setConnector] = useState("");
  const [link, setLink] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    api
      .listInvitations(tenant.id)
      .then(setList)
      .catch((e) => setError(errorMessage(e)));
  }, [tenant.id]);
  useEffect(() => refresh(), [refresh]);
  // The login connectors anyone may see: an invitation goes through one of
  // those that log in by a verified email.
  useEffect(() => {
    if (!canManage) return;
    api
      .authMethods()
      .then((m) => {
        const cs = (m.connectors ?? [])
          .filter((c) => isOidcKind(c.kind))
          .map((c) => ({ id: c.id, label: c.display_name || c.id }));
        setConnectors(cs);
        setConnector((cur) => cur || cs[0]?.id || "");
      })
      .catch(() => setConnectors([]));
  }, [canManage]);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    try {
      const r = await api.createInvitation({ tenant: tenant.id, email: email.trim(), role, connector });
      // On the address used here: the server's is its first allowed origin.
      const u = new URL(r.link);
      setLink(`${window.location.origin}${u.pathname}${u.hash}`);
      setCopied(false);
      setEmail("");
      refresh();
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  const revoke = async (id: string) => {
    setError(null);
    try {
      await api.revokeInvitation(id);
      refresh();
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  const state = (i: api.Invitation) =>
    i.state === "INVITATION_STATE_OPEN" ? "open" : i.state === "INVITATION_STATE_TAKEN" ? "taken" : "expired";

  return (
    <Card title="Invitations">
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {link && (
        <div className="mb-4 p-3 rounded-lg bg-amber-50 border border-amber-200 text-sm" data-testid="invitation-link">
          <p className="text-amber-900">Send this link to the person invited. It is shown once and can't be shown again.</p>
          <div className="mt-2 flex items-center gap-2">
            <code className="flex-1 break-all font-mono text-xs bg-white px-2 py-1 rounded border border-amber-200">{link}</code>
            <button
              type="button"
              className={secondaryButton}
              onClick={() => {
                void navigator.clipboard?.writeText(link).then(() => setCopied(true));
              }}
            >
              {copied ? "Copied" : "Copy"}
            </button>
            <button type="button" className="text-sm text-gray-500 hover:underline" onClick={() => setLink(null)}>
              Done
            </button>
          </div>
        </div>
      )}
      {list === null ? (
        <Loading />
      ) : list.length === 0 ? (
        <p className="text-sm text-gray-500">No invitations.</p>
      ) : (
        <table className="w-full text-sm">
          <thead>
            <tr className="text-left text-gray-500 border-b border-gray-100">
              <th className="py-2 font-medium">Email</th>
              <th className="py-2 font-medium">Role</th>
              <th className="py-2 font-medium">Connector</th>
              <th className="py-2 font-medium">State</th>
              <th className="py-2 font-medium">Expires</th>
              {canManage && <th className="py-2" />}
            </tr>
          </thead>
          <tbody>
            {list.map((i) => (
              <tr key={i.id} className="border-b border-gray-50">
                <td className="py-2">{i.email}</td>
                <td className="py-2">{inviteRoleLabel(i.role ?? "member")}</td>
                <td className="py-2 font-mono text-xs">{i.connector_id}</td>
                <td className="py-2">{state(i)}</td>
                <td className="py-2 text-gray-500">{formatTimestamp(i.expires_at)}</td>
                {canManage && (
                  <td className="py-2 text-right">
                    {state(i) === "open" && (
                      <button className={dangerLink} onClick={() => void revoke(i.id ?? "")}>
                        Revoke
                      </button>
                    )}
                  </td>
                )}
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {canManage && (
        <form onSubmit={submit} className="mt-4 grid grid-cols-1 md:grid-cols-4 gap-3 items-end">
          <Field label="Email" htmlFor="invite-email">
            <input
              id="invite-email"
              type="email"
              required
              className={inputClass}
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              placeholder="person@example.org"
            />
          </Field>
          <Field label="Role" htmlFor="invite-role">
            <select id="invite-role" className={selectClass} value={role} onChange={(e) => setRole(e.target.value)}>
              {INVITE_ROLES.map((r) => (
                <option key={r} value={r}>
                  {inviteRoleLabel(r)}
                </option>
              ))}
            </select>
          </Field>
          <Field label="Connector" htmlFor="invite-connector">
            <select
              id="invite-connector"
              className={selectClass}
              value={connector}
              onChange={(e) => setConnector(e.target.value)}
              required
            >
              {connectors.length === 0 && <option value="">No OIDC or Google connector</option>}
              {connectors.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.label}
                </option>
              ))}
            </select>
          </Field>
          <button type="submit" className={primaryButton} disabled={!connector || !email.trim()}>
            Invite
          </button>
        </form>
      )}
    </Card>
  );
}

/** How a membership was made: `connector:<id>` (a connector's default
 *  tenant), `invitation:<id>`, or the user who added it. */
function addedBy(by: string | undefined, connectors: Record<string, string>, names: Directory): string | null {
  if (!by) return null;
  if (by.startsWith("connector:")) {
    const id = by.slice("connector:".length);
    return `joined through ${connectors[id] ?? id}`;
  }
  if (by.startsWith("invitation:")) return "by invitation";
  const u = names.users.find((x) => x.id === by);
  return `by ${u?.display_name ?? by.slice(0, 8)}`;
}

export default function TenantDetail() {
  const { id = "" } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [dir] = useDirectory();
  const connectors = useConnectorNames();
  const nodes = useNodeNames();
  const [tenant, setTenant] = useState<Tenant | null>(null);
  const [members, setMembers] = useState<TenantMember[] | null>(null);
  const [bindings, setBindings] = useState<TenantBinding[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);
  const [confirm, setConfirm] = useState<"suspend" | "resume" | "delete" | null>(null);

  const allowed = useCan(CAPS.map((action) => ({ action, resource: { type: "Tenant" as const, id } })));
  const can = (c: Cap) => !!allowed?.[CAPS.indexOf(c)];

  const refresh = useCallback(() => {
    api
      .getTenant(id)
      .then((t) => {
        setTenant(t);
        setLoadError(null);
      })
      .catch((e) => setLoadError(errorMessage(e)));
  }, [id]);

  const refreshMembers = useCallback(() => {
    api
      .listTenantMembers(id)
      .then(setMembers)
      .catch((e) => setError(errorMessage(e)));
  }, [id]);

  const refreshBindings = useCallback(() => {
    api
      .listTenantBindings(id)
      .then(setBindings)
      .catch((e) => setError(errorMessage(e)));
  }, [id]);

  useEffect(() => {
    refresh();
  }, [refresh]);
  const canReadMembers = can("readTenantMembers");
  const canReadBindings = can("readTenantBindings");
  useEffect(() => {
    if (canReadMembers) refreshMembers();
  }, [canReadMembers, refreshMembers]);
  useEffect(() => {
    if (canReadBindings) refreshBindings();
  }, [canReadBindings, refreshBindings]);

  if (loadError) {
    return (
      <div className="text-center py-12">
        <p className="text-red-500">{loadError}</p>
        <Link to="/tenants" className="mt-4 inline-block text-sky-700 hover:underline">
          Back to tenants
        </Link>
      </div>
    );
  }
  if (!tenant || allowed === null) return <Loading />;

  const run = async (f: () => Promise<unknown>, after: () => void) => {
    setError(null);
    try {
      await f();
      after();
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const names = dir ?? { users: [], teams: [] };
  // Neither the directory's owner nor this cluster's operator can be suspended.
  const suspendable = !tenant.owns_directory && !tenant.operates_here;

  return (
    <div>
      <Link to="/tenants" className="text-sky-600 hover:text-sky-700 text-sm">
        ← Tenants
      </Link>
      <div className="flex items-center justify-between mt-2 mb-6 gap-4">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">
            {tenant.display_name || tenant.slug}{" "}
            <span className="text-base font-normal text-gray-500 font-mono">{tenant.slug}</span>
          </h1>
          <div className="mt-1 flex flex-wrap items-center gap-2 text-sm text-gray-500">
            <TenantFlags tenant={tenant} />
            <AdmissionBadge tenant={tenant} />
            <code className="text-xs text-gray-400 font-mono">{tenant.id}</code>
            <span>created {formatTimestamp(tenant.created_at)}</span>
          </div>
        </div>
        <div className="flex items-center gap-2">
          {can("updateTenant") && (
            <button className={secondaryButton} onClick={() => setEditing(true)}>
              Edit
            </button>
          )}
          {suspendable && !tenant.suspended && can("suspendTenant") && (
            <button className={secondaryButton} onClick={() => setConfirm("suspend")}>
              Suspend
            </button>
          )}
          {tenant.suspended && can("resumeTenant") && (
            <button className={secondaryButton} onClick={() => setConfirm("resume")}>
              Resume
            </button>
          )}
          {can("deleteTenant") && (
            <button
              className="px-3 py-1.5 text-sm font-medium text-red-600 bg-white border border-red-200 hover:bg-red-50 rounded-lg"
              onClick={() => setConfirm("delete")}
            >
              Delete
            </button>
          )}
        </div>
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />

      {canReadMembers && (
        <Card title="Members">
          {members === null ? (
            <Loading />
          ) : members.length === 0 ? (
            <p className="text-sm text-gray-500">No explicit members.</p>
          ) : (
            <table className="w-full text-sm">
              <thead>
                <tr className="text-left text-gray-500 border-b border-gray-100">
                  <th className="py-2 font-medium">Principal</th>
                  <th className="py-2 font-medium">Added</th>
                  {can("manageTenantMembers") && <th className="py-2" />}
                </tr>
              </thead>
              <tbody>
                {members.map((m) => (
                  <tr key={entityLabel(m.principal)} className="border-b border-gray-50">
                    <td className="py-2" title={entityLabel(m.principal)}>
                      {m.principal_name || principalName(m.principal, names)}
                      {userIdOf(m.principal) && (
                        <SignInMethods user={names.users.find((u) => u.id === userIdOf(m.principal))} connectors={connectors} nodes={nodes} />
                      )}
                    </td>
                    <td className="py-2 text-gray-500">
                      {formatTimestamp(m.at)}
                      {addedBy(m.by, connectors, names) && (
                        <span className="text-xs text-gray-400"> {addedBy(m.by, connectors, names)}</span>
                      )}
                    </td>
                    {can("manageTenantMembers") && (
                      <td className="py-2 text-right">
                        <button
                          className={dangerLink}
                          onClick={() => run(() => api.removeTenantMember(tenant.id, m.principal), refreshMembers)}
                        >
                          Remove
                        </button>
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          {can("manageTenantMembers") && (
            <AddMemberForm
              dir={dir}
              onAdd={async (p) => {
                await api.addTenantMember(tenant.id, p);
                refreshMembers();
              }}
            />
          )}
        </Card>
      )}

      {can("readTenantInvitations") && <InvitationsCard tenant={tenant} canManage={can("manageTenantInvitations")} />}

      {can("readTenantDevicePools") && <DevicePoolsCard tenant={tenant} canManage={can("manageTenantDevicePools")} />}
      {can("readTenantClusters") && <ClustersCard tenant={tenant} canManage={can("manageTenantClusters")} />}

      {can("readTenantFederations") && <FederationsCard tenant={tenant} canManage={can("manageTenantFederations")} />}

      {canReadBindings && (
        <Card title="Tenant roles">
          {bindings === null ? (
            <Loading />
          ) : (
            <BindingTable
              bindings={bindings.map((b) => asBinding(tenant.id, b))}
              dir={names}
              connectors={connectors}
              nodes={nodes}
              onRemove={
                can("manageTenantBindings")
                  ? (b) => run(() => api.removeTenantBinding(tenant.id, b.id), refreshBindings)
                  : undefined
              }
            />
          )}
          {can("manageTenantBindings") && (
            <AddBindingForm
              roles={TENANT_ROLES}
              dir={dir}
              onAdd={async (role, principal) => {
                await api.addTenantBinding(tenant.id, role, principal);
                refreshBindings();
              }}
            />
          )}
        </Card>
      )}

      {editing && (
        <Modal title="Edit Tenant" onClose={() => setEditing(false)}>
          <EditTenantForm
            tenant={tenant}
            onCancel={() => setEditing(false)}
            onDone={() => {
              setEditing(false);
              refresh();
            }}
          />
        </Modal>
      )}
      {confirm === "suspend" && (
        <ConfirmDialog
          title="Suspend Tenant"
          consequence={
            <p>
              Members of <span className="font-semibold">{tenant.slug}</span> won't be able to act on its projects and
              resources until it is resumed. Running VMs keep running.
            </p>
          }
          actionLabel="Suspend"
          onConfirm={async () => {
            await api.suspendTenant(tenant.id);
            refresh();
          }}
          onClose={() => setConfirm(null)}
        />
      )}
      {confirm === "resume" && (
        <ConfirmDialog
          title="Resume Tenant"
          consequence={
            <p>
              Members of <span className="font-semibold">{tenant.slug}</span> can use its resources again.
            </p>
          }
          actionLabel="Resume"
          danger={false}
          onConfirm={async () => {
            await api.resumeTenant(tenant.id);
            refresh();
          }}
          onClose={() => setConfirm(null)}
        />
      )}
      {confirm === "delete" && (
        <ConfirmDialog
          title="Delete Tenant"
          consequence={
            <p>
              Deletes tenant <span className="font-semibold">{tenant.slug}</span>, its members and its role links. It is
              refused while the tenant still has projects.
            </p>
          }
          typeToConfirm={tenant.slug}
          actionLabel="Delete"
          onConfirm={async () => {
            await api.deleteTenant(tenant.id);
            navigate("/tenants");
          }}
          onClose={() => setConfirm(null)}
        />
      )}
    </div>
  );
}
