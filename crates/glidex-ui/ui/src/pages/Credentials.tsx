import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import { useCan, useSession } from "../session";
import type { CredentialInfo } from "../types";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";

const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";

/** One OpenSSH public key per non-empty, non-comment line. */
function parseKeys(text: string): string[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0 && !l.startsWith("#"));
}

function formatTime(secs: number): string {
  return new Date(secs * 1000).toLocaleString();
}

function PasswordFields({
  password,
  confirm,
  setPassword,
  setConfirm,
  required,
}: {
  password: string;
  confirm: string;
  setPassword: (v: string) => void;
  setConfirm: (v: string) => void;
  required: boolean;
}) {
  return (
    <div className="grid grid-cols-2 gap-4">
      <div>
        <label className="block text-sm font-medium text-gray-700">
          Password{required ? "" : " (optional)"}
        </label>
        <input
          type="password"
          autoComplete="new-password"
          className={inputClass}
          minLength={8}
          required={required}
          value={password}
          onChange={(e) => setPassword(e.target.value)}
        />
      </div>
      <div>
        <label className="block text-sm font-medium text-gray-700">
          Confirm
        </label>
        <input
          type="password"
          autoComplete="new-password"
          className={inputClass}
          required={required || password.length > 0}
          value={confirm}
          onChange={(e) => setConfirm(e.target.value)}
        />
      </div>
    </div>
  );
}

function AddCredentialForm({
  onDone,
  onCancel,
}: {
  onDone: () => void;
  onCancel: () => void;
}) {
  const { project } = useSession();
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [keys, setKeys] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // The user's own ~/.ssh/*.pub, read by the server (local accounts only).
  const [mine, setMine] = useState<api.MySshKeys | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .mySshKeys()
      .then((m) => {
        if (cancelled) return;
        setMine(m);
        // Prefill only what the user hasn't typed yet.
        if (m.available && m.keys.length > 0) {
          setKeys((k) => (k.trim() ? k : m.keys.join("\n")));
        }
        if (m.available && m.username) {
          setUsername((u) => u || m.username!);
        }
      })
      .catch(() => !cancelled && setMine({ available: false, keys: [], reason: "could not load your public keys" }));
    return () => {
      cancelled = true;
    };
  }, []);

  const useMyKeys = () => {
    if (mine?.available && mine.keys.length > 0) {
      const have = parseKeys(keys);
      setKeys([...have, ...mine.keys.filter((k) => !have.includes(k))].join("\n"));
    }
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (password !== confirm) {
      setError("Passwords do not match");
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await api.createCredential({
        project: project ?? undefined,
        username,
        password: password || undefined,
        ssh_authorized_keys: parseKeys(keys),
      });
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to create credential");
      setSubmitting(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <div>
        <label className="block text-sm font-medium text-gray-700">
          Username
        </label>
        <input
          type="text"
          className={inputClass}
          placeholder="alice"
          pattern="[a-z_][a-z0-9_\-]{0,31}"
          title="Lowercase letters, digits, _ or -, starting with a letter or _"
          required
          value={username}
          onChange={(e) => setUsername(e.target.value)}
        />
      </div>
      <PasswordFields
        password={password}
        confirm={confirm}
        setPassword={setPassword}
        setConfirm={setConfirm}
        required={false}
      />
      <div>
        <label className="block text-sm font-medium text-gray-700">
          SSH public keys (optional)
        </label>
        <textarea
          className={`${inputClass} font-mono text-xs`}
          rows={3}
          placeholder="ssh-ed25519 AAAA... user@host"
          value={keys}
          onChange={(e) => setKeys(e.target.value)}
        />
        <p className="mt-1 text-xs text-gray-500">
          One public key per line. A password, a key, or both is required.
        </p>
        {mine === null ? (
          <p className="mt-1 text-xs text-gray-400">Looking for your public keys…</p>
        ) : mine.available && mine.keys.length > 0 ? (
          <p className="mt-1 text-xs text-gray-500" data-testid="my-keys-note">
            Filled in from your <span className="font-mono">~/.ssh/*.pub</span> ({mine.keys.length}{" "}
            {mine.keys.length === 1 ? "key" : "keys"}).{" "}
            <button type="button" className="text-sky-600 hover:underline" onClick={useMyKeys}>
              Add my keys again
            </button>
          </p>
        ) : mine.available ? (
          <p className="mt-1 text-xs text-gray-500" data-testid="my-keys-note">
            No public keys found in your <span className="font-mono">~/.ssh</span>; paste one above.
          </p>
        ) : (
          <p className="mt-1 text-xs text-gray-500" data-testid="my-keys-note">
            Your keys can't be filled in automatically: {mine.reason}.
          </p>
        )}
      </div>
      <FormButtons onCancel={onCancel} submitting={submitting} label="Add" />
    </form>
  );
}

function EditCredentialForm({
  credential,
  onDone,
  onCancel,
}: {
  credential: CredentialInfo;
  onDone: () => void;
  onCancel: () => void;
}) {
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const { project } = useSession();
  const [keys, setKeys] = useState(credential.ssh_authorized_keys.join("\n"));
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (password !== confirm) {
      setError("Passwords do not match");
      return;
    }
    setSubmitting(true);
    setError(null);
    try {
      await api.updateCredential(credential.username, {
        password: password || undefined,
        ssh_authorized_keys: parseKeys(keys),
      }, credential.project ?? project);
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to update credential");
      setSubmitting(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <p className="text-sm text-gray-600">
        Leave the password empty to keep the current one. Changes apply to VMs
        on their first boot only.
      </p>
      <PasswordFields
        password={password}
        confirm={confirm}
        setPassword={setPassword}
        setConfirm={setConfirm}
        required={false}
      />
      <div>
        <label className="block text-sm font-medium text-gray-700">
          SSH public keys
        </label>
        <textarea
          className={`${inputClass} font-mono text-xs`}
          rows={3}
          value={keys}
          onChange={(e) => setKeys(e.target.value)}
        />
      </div>
      <FormButtons onCancel={onCancel} submitting={submitting} label="Save" />
    </form>
  );
}

function FormButtons({
  onCancel,
  submitting,
  label,
}: {
  onCancel: () => void;
  submitting: boolean;
  label: string;
}) {
  return (
    <div className="flex justify-end space-x-3 pt-2">
      <button
        type="button"
        className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg transition-colors"
        onClick={onCancel}
      >
        Cancel
      </button>
      <button
        type="submit"
        className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg transition-colors disabled:opacity-50"
        disabled={submitting}
      >
        {submitting ? "Saving..." : label}
      </button>
    </div>
  );
}

export default function Credentials() {
  const { project } = useSession();
  const [canAdd] = useCan(project ? [{ action: "createCredential", resource: { type: "Project", id: project } }] : []) ?? [];
  const [credentials, setCredentials] = useState<CredentialInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<CredentialInfo | null>(null);

  const refresh = useCallback(async () => {
    try {
      const data = await api.listCredentials(project);
      data.sort((a, b) => a.username.localeCompare(b.username));
      setCredentials(data);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to load credentials");
    }
  }, [project]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const remove = async (username: string) => {
    if (!confirm(`Delete credential "${username}"?`)) return;
    try {
      await api.deleteCredential(username, project);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to delete credential");
    }
  };

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">Credentials</h1>
          <p className="text-gray-500 mt-1">
            Guest logins provisioned by cloud-init on a VM's first boot.
            Passwords are stored as SHA-512-crypt hashes only.
          </p>
        </div>
        {canAdd && (
          <button
            className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg transition-colors"
            onClick={() => setAdding(true)}
          >
            Add Credential
          </button>
        )}
      </div>

      {error && (
        <div className="mb-4 p-4 bg-red-50 border border-red-200 rounded-lg text-red-700">
          {error}
        </div>
      )}

      {credentials === null ? (
        <LoadingCard />
      ) : credentials.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">
          No credentials yet. Add one to choose it when creating a firmware-boot VM.
        </div>
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-hidden">
          <table className="w-full text-sm">
            <thead className="bg-gray-50 text-gray-600 text-left">
              <tr>
                <th className="px-4 py-3 font-medium">Username</th>
                <th className="px-4 py-3 font-medium">Password</th>
                <th className="px-4 py-3 font-medium">SSH keys</th>
                <th className="px-4 py-3 font-medium">Updated</th>
                <th className="px-4 py-3" />
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {credentials.map((c) => (
                <tr key={c.username}>
                  <td className="px-4 py-3 font-mono">{c.username}</td>
                  <td className="px-4 py-3">{c.has_password ? "Set" : "—"}</td>
                  <td className="px-4 py-3">{c.ssh_authorized_keys.length}</td>
                  <td className="px-4 py-3 text-gray-500">
                    {formatTime(c.updated_at)}
                  </td>
                  <td className="px-4 py-3 text-right space-x-2 whitespace-nowrap">
                    <button
                      className="px-3 py-1 text-xs font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg"
                      onClick={() => setEditing(c)}
                    >
                      Edit
                    </button>
                    <button
                      className="px-3 py-1 text-xs font-medium text-red-700 bg-red-50 hover:bg-red-100 rounded-lg"
                      onClick={() => remove(c.username)}
                    >
                      Delete
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {adding && (
        <Modal title="Add Credential" onClose={() => setAdding(false)}>
          <AddCredentialForm
            onCancel={() => setAdding(false)}
            onDone={() => {
              setAdding(false);
              refresh();
            }}
          />
        </Modal>
      )}

      {editing && (
        <Modal
          title={`Edit ${editing.username}`}
          onClose={() => setEditing(null)}
        >
          <EditCredentialForm
            credential={editing}
            onCancel={() => setEditing(null)}
            onDone={() => {
              setEditing(null);
              refresh();
            }}
          />
        </Modal>
      )}
    </div>
  );
}
