import { useState } from "react";
import * as api from "../api";
import type { Team } from "../types";
import { formatTimestamp } from "../types";
import { Loading } from "../components/Loading";
import ConfirmDialog from "../components/ConfirmDialog";
import SystemRoles from "../components/SystemRoles";
import {
  Badge,
  Card,
  ErrorBanner,
  PageHeader,
  dangerLink,
  errorMessage,
  primaryButton,
  secondaryButton,
  signInLabel,
} from "../components/ui";
import { useSession } from "../session";
import { useConnectorNames, useDirectory, useNodeNames } from "../hooks";

function TeamCard({
  team,
  manage,
  users,
  connectors,
  onChange,
  onError,
}: {
  team: Team;
  manage: boolean;
  connectors: Record<string, string>;
  users: { id: string; display_name: string }[];
  onChange: () => void;
  onError: (e: string) => void;
}) {
  const [add, setAdd] = useState("");
  const name = (id: string) => users.find((u) => u.id === id)?.display_name ?? id.slice(0, 8);
  const run = async (f: () => Promise<unknown>) => {
    try {
      await f();
      onChange();
    } catch (e) {
      onError(errorMessage(e));
    }
  };
  const teamMembers = team.members ?? [];
  const members = teamMembers.map((m) => m.user_id);
  return (
    <div className="py-3 border-b border-gray-100 last:border-0">
      <div className="flex items-center justify-between">
        <div>
          <span className="font-medium text-gray-900">{team.name}</span>{" "}
          <code className="text-xs text-gray-400 font-mono">{team.id}</code>
          {team.managed_by && (
            <span className="ml-2" title="Its name and its connector memberships come from the IdP">
              <Badge kind="muted">from {connectors[team.managed_by] ?? team.managed_by}</Badge>
            </span>
          )}
        </div>
        {manage && !team.managed_by && (
          <button className={dangerLink} onClick={() => confirm(`Delete team ${team.name}?`) && run(() => api.deleteTeam(team.id))}>
            Delete
          </button>
        )}
      </div>
      <div className="mt-2 flex flex-wrap gap-2">
        {teamMembers.length === 0 && <span className="text-sm text-gray-500">No members.</span>}
        {teamMembers.map((m) => (
          <span key={`${m.user_id}-${m.source}`} className="inline-flex items-center gap-1 px-2 py-0.5 bg-gray-100 rounded-full text-sm">
            {name(m.user_id)}
            {m.source !== "manual" && <span className="text-xs text-gray-500">({m.source})</span>}
            {manage && m.source === "manual" && (
              <button
                className="text-gray-400 hover:text-red-600"
                aria-label={`Remove ${name(m.user_id)}`}
                onClick={() => run(() => api.removeTeamMember(team.id, m.user_id))}
              >
                ×
              </button>
            )}
          </span>
        ))}
      </div>
      {manage && (
        <div className="mt-2 flex items-center gap-2">
          <select className="px-2 py-1 border border-gray-300 rounded-lg text-sm bg-white" value={add} onChange={(e) => setAdd(e.target.value)}>
            <option value="">Add member…</option>
            {users
              .filter((u) => !members.includes(u.id))
              .map((u) => (
                <option key={u.id} value={u.id}>
                  {u.display_name}
                </option>
              ))}
          </select>
          <button
            className={secondaryButton}
            disabled={!add}
            onClick={() =>
              run(async () => {
                await api.addTeamMember(team.id, add);
                setAdd("");
              })
            }
          >
            Add
          </button>
        </div>
      )}
    </div>
  );
}

function disabledWhy(r: string | undefined): string {
  switch (r) {
    case "scim":
      return "Its IdP deactivated or removed it; it is enabled again there";
    case "connector_deleted":
      return "Its connector was deleted";
    case "admin":
      return "An administrator disabled it";
    default:
      return "";
  }
}

export default function Access() {
  const { host, projects, projectName, me } = useSession();
  const [dir, reloadDir] = useDirectory();
  const connectors = useConnectorNames();
  const nodes = useNodeNames();
  const [error, setError] = useState<string | null>(null);
  // An administrator taking one of a user's sign-in methods away.
  const [unlinking, setUnlinking] = useState<{ user: string; name: string; key: string; label: string } | null>(null);
  const [endSessions, setEndSessions] = useState(true);
  const [deleting, setDeleting] = useState<{ id: string; name: string } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [teamName, setTeamName] = useState("");

  const run = async (f: () => Promise<unknown>, after: () => void) => {
    setError(null);
    try {
      await f();
      after();
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  if (!host.listUsers && !host.listTeams && !host.readSystemBindings) {
    return (
      <div>
        <PageHeader title="Access" />
        <p className="text-gray-500">You can't manage users, teams or system roles.</p>
      </div>
    );
  }

  return (
    <div>
      <PageHeader title="Access" subtitle="Users, teams and host-wide roles. Project roles are set on each project's page." />
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {notice && (
        <div className="mb-4 p-3 rounded-lg bg-green-50 border border-green-200 text-sm text-green-800" role="status">
          {notice}
        </div>
      )}

      {host.listUsers && (
        <Card title="Users">
          {dir === null ? (
            <Loading />
          ) : (
            <table className="w-full text-sm">
              <thead>
                <tr className="text-left text-gray-500 border-b border-gray-100">
                  <th className="py-2 font-medium">Name</th>
                  <th className="py-2 font-medium">Identities</th>
                  <th className="py-2 font-medium">Default project</th>
                  <th className="py-2 font-medium">Created</th>
                  <th className="py-2 font-medium">Status</th>
                </tr>
              </thead>
              <tbody>
                {dir.users.map((u) => (
                  <tr key={u.id} className="border-b border-gray-50">
                    <td className="py-2">
                      <div className="font-medium text-gray-900">{u.display_name}</div>
                      <code className="text-xs text-gray-400 font-mono">{u.id}</code>
                    </td>
                    <td className="py-2 text-xs font-mono text-gray-600">
                      {(u.identities ?? []).map((i) => {
                        const key = `${i.provider}:${i.subject}`;
                        const label = signInLabel(i.provider, connectors, i.subject, nodes);
                        return (
                          <div key={key}>
                            <span className="font-sans text-gray-700">{label}</span>{" "}
                            <span className="text-gray-400">{i.email ?? i.subject}</span>
                            {host.manageUsers && ((u.identities ?? []).length > 1 || u.disabled) && (
                              <button
                                className="ml-2 font-sans text-red-600 hover:underline"
                                title="Remove this sign-in method"
                                onClick={() => {
                                  setEndSessions(true);
                                  setUnlinking({ user: u.id, name: u.display_name, key, label });
                                }}
                              >
                                remove
                              </button>
                            )}
                          </div>
                        );
                      })}
                    </td>
                    <td className="py-2">
                      {host.manageUsers ? (
                        <select
                          aria-label={`Default project of ${u.display_name}`}
                          className="px-2 py-1 border border-gray-300 rounded-lg text-sm bg-white"
                          value={u.default_project ?? ""}
                          onChange={(e) =>
                            run(() => api.updateUser(u.id, { default_project: e.target.value }), reloadDir)
                          }
                        >
                          <option value="">none</option>
                          {u.default_project && !projects.some((p) => p.id === u.default_project) && (
                            <option value={u.default_project}>{u.default_project}</option>
                          )}
                          {projects.map((p) => (
                            <option key={p.id} value={p.id}>
                              {p.name}
                            </option>
                          ))}
                        </select>
                      ) : (
                        projectName(u.default_project)
                      )}
                    </td>
                    <td className="py-2 text-gray-500">{formatTimestamp(u.created_at)}</td>
                    <td className="py-2">
                      {u.disabled ? (
                        <span title={disabledWhy(u.disabled_reason)}>
                          <Badge kind="denied">{u.disabled_reason === "scim" ? "disabled by its IdP" : "disabled"}</Badge>
                        </span>
                      ) : (
                        <Badge kind="ok">active</Badge>
                      )}
                      {u.managed_by && (
                        <span className="ml-2" title="Its name, email and whether it is enabled come from the IdP">
                          <Badge kind="muted">managed by {connectors[u.managed_by] ?? u.managed_by}</Badge>
                        </span>
                      )}
                      {/* An administrator may stop a managed user, and undo only that (§8.3). */}
                      {host.manageUsers && !(u.managed_by && u.disabled && u.disabled_reason !== "admin") && (
                        <button
                          className="ml-2 text-sm text-sky-700 hover:underline"
                          onClick={() =>
                            (!u.disabled ? confirm(`Disable ${u.display_name}? Their sessions end now.`) : true) &&
                            run(() => api.updateUser(u.id, { disabled: !u.disabled }), reloadDir)
                          }
                        >
                          {u.disabled ? "Enable" : "Disable"}
                        </button>
                      )}
                      {host.manageUsers && u.id !== me.user?.id && !(u.managed_by && !u.disabled) && (
                        <button className={`ml-2 ${dangerLink}`} onClick={() => setDeleting({ id: u.id, name: u.display_name })}>
                          Delete
                        </button>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </Card>
      )}

      {host.listTeams && (
        <Card title="Teams">
          {dir === null ? (
            <Loading />
          ) : dir.teams.length === 0 ? (
            <p className="text-sm text-gray-500">No teams.</p>
          ) : (
            dir.teams.map((t) => (
              <TeamCard
                key={t.id}
                team={t}
                manage={host.manageTeams}
                users={dir.users}
                connectors={connectors}
                onChange={reloadDir}
                onError={setError}
              />
            ))
          )}
          {host.manageTeams && (
            <form
              className="mt-4 flex items-center gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                run(
                  () => api.createTeam(teamName.trim()),
                  () => {
                    setTeamName("");
                    reloadDir();
                  },
                );
              }}
            >
              <input
                aria-label="New team name"
                className="px-3 py-2 border border-gray-300 rounded-lg text-sm"
                placeholder="new team name"
                required
                value={teamName}
                onChange={(e) => setTeamName(e.target.value)}
              />
              <button type="submit" className={primaryButton}>
                Create team
              </button>
            </form>
          )}
        </Card>
      )}

      <SystemRoles dir={dir} />
      {deleting && (
        <ConfirmDialog
          title={`Delete ${deleting.name}`}
          consequence={
            <>
              The user goes with its sign-in methods, sessions, personal tokens, team memberships, role links and tenant
              memberships. Service-account tokens it made stay. This can't be undone; a local account that signs in again
              becomes a new user.
            </>
          }
          typeToConfirm={deleting.name}
          actionLabel="Delete user"
          onConfirm={async () => {
            const d = await api.deleteUser(deleting.id);
            setNotice(
              `Deleted ${deleting.name}: ${d.identities} sign-in methods, ${d.sessions} sessions, ${d.tokens} tokens, ${d.links} role links, ${d.teams} teams, ${d.tenants} tenant memberships.`,
            );
            reloadDir();
          }}
          onClose={() => setDeleting(null)}
        />
      )}
      {unlinking && (
        <ConfirmDialog
          title="Remove a sign-in method"
          consequence={`${unlinking.name} won't be able to sign in with ${unlinking.label} any more. Their other sign-in methods stay; a disabled user's last one can go, so it can be added to the right user.`}
          actionLabel="Remove"
          onConfirm={async () => {
            await api.unlinkIdentity(unlinking.user, unlinking.key, endSessions);
            reloadDir();
          }}
          onClose={() => setUnlinking(null)}
        >
          <label className="flex items-center gap-2 text-sm text-gray-700">
            <input type="checkbox" checked={endSessions} onChange={(e) => setEndSessions(e.target.checked)} />
            Also sign them out everywhere (end their sessions)
          </label>
        </ConfirmDialog>
      )}
    </div>
  );
}
