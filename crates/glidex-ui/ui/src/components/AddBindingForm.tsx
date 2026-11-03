import { useState, type FormEvent } from "react";
import type { PrincipalInput } from "../types";
import { roleLabel } from "../types";
import { type Directory, errorMessage, inputClass, primaryButton, selectClass } from "./ui";

type Kind = "User" | "Team";

/** What was typed for a principal: a user's id or email (D83), a team's id. */
export function typedPrincipal(kind: Kind, text: string, emails = true): PrincipalInput {
  const t = text.trim();
  return emails && kind === "User" && t.includes("@") ? { type: "User", email: t } : { type: kind, id: t };
}

/** Role + principal picker. With a directory (listUsers/listTeams) the
 * principal is chosen from a list; otherwise its id (or a user's email) is typed. */
export default function AddBindingForm({
  roles,
  dir,
  onAdd,
  emails = false,
}: {
  roles: readonly string[];
  dir: Directory | null;
  onAdd: (role: string, principal: PrincipalInput) => Promise<void>;
  /** A user may be given by email (D83: project and tenant roles). */
  emails?: boolean;
}) {
  const [role, setRole] = useState<string>(roles[0]);
  const [kind, setKind] = useState<Kind>("User");
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
      await onAdd(role, typedPrincipal(kind, id, emails));
      setId("");
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="mt-4 grid grid-cols-1 md:grid-cols-4 gap-3 items-end">
      <div>
        <label className="block text-sm font-medium text-gray-700">Role</label>
        <select className={selectClass} value={role} onChange={(e) => setRole(e.target.value)}>
          {roles.map((r) => (
            <option key={r} value={r}>
              {roleLabel(r)}
            </option>
          ))}
        </select>
      </div>
      <div>
        <label className="block text-sm font-medium text-gray-700">Principal type</label>
        <select
          className={selectClass}
          value={kind}
          onChange={(e) => {
            setKind(e.target.value as Kind);
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
            placeholder={kind === "User" ? (emails ? "user id or email" : "user id") : "team id or unix:<group>"}
            value={id}
            onChange={(e) => setId(e.target.value)}
            required
          />
        )}
      </div>
      <div>
        <button type="submit" className={primaryButton} disabled={busy}>
          Add role
        </button>
      </div>
      {error && <p className="md:col-span-4 text-sm text-red-600">{error}</p>}
    </form>
  );
}
