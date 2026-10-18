import { useEffect, useState } from "react";
import * as api from "../api";
import { isOidcKind } from "../types";
import { Card, ErrorBanner, PageHeader, dangerLink, errorMessage, secondaryButton, signInLabel } from "../components/ui";
import ConfirmDialog from "../components/ConfirmDialog";
import { useConnectorNames } from "../hooks";
import { useSession } from "../session";

const LINK_ERRORS: Record<string, string> = {
  taken: "That sign-in already belongs to another user. Ask an administrator to remove it there first.",
  not_allowed: "The connector didn't allow that sign-in (a group, MFA or domain rule).",
  not_signed_in: "You were signed out before the sign-in finished. Sign in and try again.",
};

/** Your account (spec/tenancy.md §6.2): the ways you sign in. You may
 *  have several; add one through any connector, remove any but the last. */
export default function Account() {
  const { me, methods, refresh } = useSession();
  const connectors = useConnectorNames();
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [removing, setRemoving] = useState<{ key: string; label: string } | null>(null);

  // What the IdP's round trip came back with; then a clean address.
  useEffect(() => {
    const q = new URLSearchParams(window.location.search);
    const linked = q.get("linked");
    const failed = q.get("link_error");
    if (linked) setNotice(`Added: you can now sign in with ${connectors[linked] ?? linked}.`);
    if (failed) setError(LINK_ERRORS[failed] ?? "The sign-in wasn't added.");
    if (linked || failed) window.history.replaceState(null, "", window.location.pathname);
  }, [connectors]);

  const user = me.user;
  const identities = user?.identities ?? [];
  const available = (methods.connectors ?? []).filter((c) => isOidcKind(c.kind));

  if (!user) {
    return (
      <div>
        <PageHeader title="Your account" />
        <p className="text-sm text-gray-600">This session is a token's: tokens have no sign-in methods.</p>
      </div>
    );
  }
  const add = async (id: string) => {
    setError(null);
    try {
      await api.startLink(id);
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  return (
    <div>
      <PageHeader title="Your account" subtitle={user.display_name} />
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {notice && (
        <div className="mb-4 p-3 rounded-lg bg-green-50 border border-green-200 text-sm text-green-800" role="status">
          {notice}
        </div>
      )}
      <Card title="Sign-in methods">
        <p className="text-sm text-gray-600 mb-3">
          Each is a way to sign in as you, with the same roles. Remove any but the last.
        </p>
        <table className="w-full text-sm" data-testid="sign-in-methods-list">
          <thead>
            <tr className="text-left text-gray-500 border-b border-gray-100">
              <th className="py-2 font-medium">Method</th>
              <th className="py-2 font-medium">Account</th>
              <th className="py-2" />
            </tr>
          </thead>
          <tbody>
            {identities.map((i) => {
              const key = `${i.provider}:${i.subject}`;
              const label = signInLabel(i.provider, connectors);
              return (
                <tr key={key} className="border-b border-gray-50">
                  <td className="py-2">{label}</td>
                  <td className="py-2 text-gray-600">{i.email ?? i.subject}</td>
                  <td className="py-2 text-right">
                    {identities.length > 1 && (
                      <button className={dangerLink} onClick={() => setRemoving({ key, label })}>
                        Remove
                      </button>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
        {available.length > 0 && (
          <div className="mt-4">
            <p className="text-sm text-gray-700 mb-2">Add a way to sign in:</p>
            <div className="flex flex-wrap gap-2">
              {available.map((c) => (
                <button key={c.id} className={secondaryButton} onClick={() => void add(c.id)}>
                  {c.display_name || c.id}
                </button>
              ))}
            </div>
            <p className="mt-2 text-xs text-gray-500">You'll sign in there; adding one needs a recent login here.</p>
          </div>
        )}
      </Card>
      {removing && (
        <ConfirmDialog
          title="Remove a sign-in method"
          consequence={`You won't be able to sign in with ${removing.label} any more.`}
          actionLabel="Remove"
          onConfirm={async () => {
            await api.unlinkIdentity(user.id, removing.key);
            await refresh();
          }}
          onClose={() => setRemoving(null)}
        />
      )}
    </div>
  );
}
