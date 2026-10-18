import { useCallback, useEffect, useState, type FormEvent } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import * as api from "../api";
import type { Binding, EntityRef, Tenant, TenantBinding, TenantMember } from "../types";
import { TENANT_ROLES, entityLabel, formatTimestamp, isOidcKind } from "../types";
import Modal from "../components/Modal";
import ConfirmDialog from "../components/ConfirmDialog";
import AddBindingForm from "../components/AddBindingForm";
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
} from "../components/ui";
import { useCan } from "../session";
import { useDirectory } from "../hooks";
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
] as const;
type Cap = (typeof CAPS)[number];

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
      setLink(r.link);
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

export default function TenantDetail() {
  const { id = "" } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [dir] = useDirectory();
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
                    </td>
                    <td className="py-2 text-gray-500">
                      {formatTimestamp(m.at)}
                      {m.by && <span className="text-xs text-gray-400"> by {m.by.slice(0, 8)}</span>}
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

      {canReadBindings && (
        <Card title="Tenant roles">
          {bindings === null ? (
            <Loading />
          ) : (
            <BindingTable
              bindings={bindings.map((b) => asBinding(tenant.id, b))}
              dir={names}
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
