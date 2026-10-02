import { useState, type FormEvent } from "react";
import type { EntityRef } from "../types";
import { roleLabel } from "../types";
import { type Directory, errorMessage, inputClass, primaryButton, selectClass } from "./ui";

type Kind = "User" | "Team";

/** Role + principal picker. With a directory (listUsers/listTeams) the
 * principal is chosen from a list; otherwise its id is typed. */
export default function AddBindingForm({
  roles,
  dir,
  onAdd,
}: {
  roles: readonly string[];
  dir: Directory | null;
  onAdd: (role: string, principal: EntityRef) => Promise<void>;
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
      await onAdd(role, { type: kind, id: id.trim() });
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
            placeholder={kind === "User" ? "user id" : "team id or unix:<group>"}
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
