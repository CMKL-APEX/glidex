import { useCallback, useEffect, useState } from "react";
import * as api from "../api";
import type { HostedDirectory } from "../api";
import { useSession } from "../session";
import ConfirmDialog from "../components/ConfirmDialog";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";
import { Card, ErrorBanner, PageHeader, errorMessage, inputClass, primaryButton, secondaryButton } from "../components/ui";
import { formatTimestamp } from "../types";

/** The directories this host serves (spec/tenancy.md D27, D59): the
 * operator creates one for an organization and sends its first
 * administrator the invitation; what is inside is theirs. */
export default function Directories() {
  const { host } = useSession();
  const [dirs, setDirs] = useState<HostedDirectory[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [removing, setRemoving] = useState<HostedDirectory | null>(null);
  const [made, setMade] = useState<{ id: string; link: string; email: string } | null>(null);

  const load = useCallback(async () => {
    try {
      setDirs(await api.listDirectories());
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);
  useEffect(() => {
    load();
  }, [load]);

  return (
    <div>
      <PageHeader title="Hosted directories" subtitle="Organizations whose users and sign-in this host keeps. You see that they exist, not what is in them.">
        {host.manageDirectories && (
          <button className={primaryButton} onClick={() => setCreating(true)}>
            New directory
          </button>
        )}
      </PageHeader>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {made && (
        <div className="mb-4 p-4 bg-green-50 border border-green-200 rounded-lg text-sm" data-testid="directory-invitation">
          <p>
            Directory <span className="font-mono">{made.id}</span> is ready. Send this link to {made.email}; it is shown only now. They create a
            passkey with it and administer the directory from there.
          </p>
          <p className="mt-2 font-mono break-all">{made.link}</p>
          <p className="mt-2 text-gray-600">
            Its people sign in at <span className="font-mono">/login/{made.id}</span>, or by their email on the login page once they have a sign-in
            provider.
          </p>
        </div>
      )}
      {dirs === null ? (
        <LoadingCard />
      ) : (
        <Card>
          <table className="w-full text-sm">
            <thead>
              <tr className="text-left text-gray-500">
                <th className="py-1">Directory</th>
                <th>Name</th>
                <th>Issuer</th>
                <th>Trust domain</th>
                <th>Created</th>
                <th />
              </tr>
            </thead>
            <tbody>
              {dirs.map((d) => (
                <tr key={d.id} className="border-t border-gray-100">
                  <td className="py-1.5 font-mono">{d.id}</td>
                  <td>{d.name}</td>
                  <td className="font-mono text-xs">{d.issuer}</td>
                  <td className="font-mono text-xs">{d.trust_domain}</td>
                  <td>{formatTimestamp(d.created_at)}</td>
                  <td className="text-right">
                    {host.manageDirectories && d.id !== "local" && (
                      <button className="text-red-600 hover:underline text-xs" onClick={() => setRemoving(d)}>
                        Delete
                      </button>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </Card>
      )}
      {creating && (
        <NewDirectory
          onClose={() => setCreating(false)}
          onDone={(m) => {
            setCreating(false);
            setMade(m);
            load();
          }}
        />
      )}
      {removing && (
        <ConfirmDialog
          title={`Delete directory ${removing.id}`}
          consequence="Only a directory nobody uses yet can be deleted: one with users, sign-in providers or projects stays until its administrators remove them."
          actionLabel="Delete"
          onConfirm={async () => {
            await api.deleteDirectory(removing.id);
            load();
          }}
          onClose={() => setRemoving(null)}
        />
      )}
    </div>
  );
}

function NewDirectory({ onClose, onDone }: { onClose: () => void; onDone: (m: { id: string; link: string; email: string }) => void }) {
  const [id, setId] = useState("");
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [trustDomain, setTrustDomain] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await api.createDirectory({ id, name, owner: `${id}-admin`, admin_email: email, trust_domain: trustDomain || undefined });
      onDone({ id, link: r.link, email });
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };
  return (
    <Modal title="New directory" onClose={onClose}>
      <div className="space-y-3 text-sm">
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        <div>
          <label htmlFor="dir-id" className="block text-xs text-gray-500">
            Id (its issuer and login page use it; it can't change)
          </label>
          <input id="dir-id" className={inputClass} value={id} placeholder="acme" onChange={(e) => setId(e.target.value.toLowerCase())} />
        </div>
        <div>
          <label htmlFor="dir-name" className="block text-xs text-gray-500">
            Name
          </label>
          <input id="dir-name" className={inputClass} value={name} placeholder="Acme Corp" onChange={(e) => setName(e.target.value)} />
        </div>
        <div>
          <label htmlFor="dir-admin" className="block text-xs text-gray-500">
            Its first administrator's email
          </label>
          <input id="dir-admin" type="email" className={inputClass} value={email} onChange={(e) => setEmail(e.target.value)} />
        </div>
        <div>
          <label htmlFor="dir-td" className="block text-xs text-gray-500">
            Trust domain (optional; its workloads&apos; identities use it, and it can&apos;t change)
          </label>
          <input
            id="dir-td"
            className={inputClass}
            value={trustDomain}
            placeholder={id ? `${id}.<this host>` : "acme.example"}
            onChange={(e) => setTrustDomain(e.target.value.toLowerCase())}
          />
        </div>
        <div className="flex justify-end gap-2">
          <button className={secondaryButton} onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className={primaryButton} onClick={create} disabled={busy || !/^[a-z0-9-]{1,32}$/.test(id) || !email.includes("@") || (trustDomain !== "" && !/^[a-z0-9_-]+(\.[a-z0-9_-]+)*$/.test(trustDomain))}>
            Create
          </button>
        </div>
      </div>
    </Modal>
  );
}
