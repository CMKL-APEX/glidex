import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import { formatTimestamp } from "../types";
import ConfirmDialog from "./ConfirmDialog";
import { Loading } from "./Loading";
import { Card, ErrorBanner, dangerLink, errorMessage, inputClass, primaryButton } from "./ui";

/** The signed-in user's passkeys (spec/25-passkeys.md §4): add, rename,
 * remove. A passkey is one more way to sign in, alongside the others. */
export default function PasskeysCard({ user, onChange }: { user: string; onChange?: () => void }) {
  const [list, setList] = useState<api.Passkey[] | null>(null);
  const [rpId, setRpId] = useState<string | undefined>(undefined);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [removing, setRemoving] = useState<api.Passkey | null>(null);

  const refresh = useCallback(() => {
    api
      .listPasskeys(user)
      .then((r) => {
        setList(r.passkeys);
        setRpId(r.rpId ?? undefined);
      })
      .catch((e) => setError(errorMessage(e)));
  }, [user]);
  useEffect(() => refresh(), [refresh]);

  const add = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await api.addPasskey(name.trim() || "Passkey");
      setName("");
      setNotice("Passkey added: you can sign in with it now.");
      refresh();
      onChange?.();
    } catch (err) {
      const m = errorMessage(err);
      if (!/NotAllowedError|aborted|cancel/i.test(m)) setError(m);
    } finally {
      setBusy(false);
    }
  };

  if (!api.passkeysSupported()) {
    return (
      <Card title="Passkeys">
        <p className="text-sm text-gray-500">This browser can't make passkeys.</p>
      </Card>
    );
  }
  return (
    <Card title="Passkeys">
      <p className="mb-3 text-sm text-gray-500">
        Sign in with your device's fingerprint, face or PIN instead of a password. Keep another sign-in method, or a second passkey,
        in case you lose this device.
        {!rpId && <> Passkeys made here work under this page's name ({window.location.hostname}) only.</>}
      </p>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {notice && <p className="mb-2 text-sm text-green-700">{notice}</p>}
      {list === null ? (
        <Loading />
      ) : list.length === 0 ? (
        <p className="text-sm text-gray-500">No passkeys yet.</p>
      ) : (
        <ul className="divide-y divide-gray-100" data-testid="passkeys">
          {list.map((p) => (
            <li key={p.id} className="py-2 flex items-center justify-between text-sm">
              <div>
                <span className="font-medium text-gray-900">{p.name}</span>{" "}
                <span className="text-gray-500">
                  made {formatTimestamp(p.created_at)}
                  {p.last_used_at ? `, last used ${formatTimestamp(p.last_used_at)}` : ", not used yet"}
                  {p.backed_up ? ", synced" : ""}
                </span>
              </div>
              <div className="space-x-3">
                <button
                  className="text-sky-700 hover:underline"
                  onClick={async () => {
                    const n = prompt("New name for this passkey", p.name);
                    if (n && n.trim()) {
                      try {
                        await api.renamePasskey(user, p.id, n.trim());
                        refresh();
                      } catch (e) {
                        setError(errorMessage(e));
                      }
                    }
                  }}
                >
                  Rename
                </button>
                <button className={dangerLink} onClick={() => setRemoving(p)}>
                  Remove
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
      <form onSubmit={add} className="mt-3 flex items-end gap-3">
        <div className="flex-1">
          <label className="block text-sm font-medium text-gray-700" htmlFor="passkey-name">
            Name
          </label>
          <input
            id="passkey-name"
            className={inputClass}
            placeholder="e.g. work laptop"
            value={name}
            maxLength={64}
            onChange={(e) => setName(e.target.value)}
          />
        </div>
        <button type="submit" className={primaryButton} disabled={busy}>
          {busy ? "Waiting for your device..." : "Add a passkey"}
        </button>
      </form>
      {removing && (
        <ConfirmDialog
          title={`Remove ${removing.name}`}
          consequence={<p>You won't be able to sign in with this passkey any more.</p>}
          actionLabel="Remove"
          onConfirm={async () => {
            await api.deletePasskey(user, removing.id);
            refresh();
            onChange?.();
          }}
          onClose={() => setRemoving(null)}
        />
      )}
    </Card>
  );
}
