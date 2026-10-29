import { useEffect, useState } from "react";
import * as api from "../api";
import { Loading } from "../components/Loading";
import { errorMessage, primaryButton } from "../components/ui";

type Info = Awaited<ReturnType<typeof api.inspectInvitation>>;

/** The invitation in the address bar (`/invite/<id>#<secret>`), read once:
 *  the fragment is dropped from the address so it isn't left in history. */
function takeToken(): string | null {
  const id = window.location.pathname.split("/").filter(Boolean)[1];
  const secret = window.location.hash.replace(/^#/, "");
  if (secret) window.history.replaceState(null, "", window.location.pathname);
  return id && secret ? `${id}.${secret}` : null;
}

const roleLabel = (r?: string) =>
  r === "role.tenant-admin" ? "tenant administrator" : r === "role.tenant-auditor" ? "tenant auditor" : "member";

/** An invitation link (spec/19-tenancy.md §6.1): what it is for, and a login
 *  through its connector that takes it. Works signed out. */
export default function Invite() {
  const [token] = useState(takeToken);
  const [info, setInfo] = useState<Info | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!token) {
      setError("This invitation link is incomplete: open the whole link you were sent.");
      return;
    }
    api
      .inspectInvitation(token)
      .then(setInfo)
      .catch(() => setError("This invitation can't be used: it was already used, revoked, or it expired. Ask for a new one."));
  }, [token]);

  const passkey = info?.connector_kind === "passkey";
  const accept = async () => {
    if (!token || !info?.connector_id) return;
    setBusy(true);
    setError(null);
    try {
      // A passkey invitation (spec/19-tenancy.md D60): the first passkey signs in.
      if (passkey) {
        await api.acceptPasskeyInvitation(token);
        window.location.assign("/");
        return;
      }
      const { redirect } = await api.startInvitedLogin(info.connector_id, token);
      window.location.assign(redirect);
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  return (
    <div className="min-h-screen flex items-center justify-center bg-gray-50 px-4">
      <div className="w-full max-w-md bg-white rounded-xl shadow-md border border-gray-100 p-6">
        <h1 className="text-xl font-semibold text-gray-900">You've been invited</h1>
        {error ? (
          <p className="mt-4 text-sm text-red-600" role="alert">
            {error}
          </p>
        ) : !info ? (
          <Loading />
        ) : (
          <>
            <p className="mt-3 text-sm text-gray-700">
              Join <strong>{info.tenant_display_name || info.tenant_slug}</strong> as {roleLabel(info.role)}.
            </p>
            {passkey ? (
              <p className="mt-2 text-sm text-gray-600">
                Your account (<strong>{info.email}</strong>) is new: create a passkey for it on this device, with your PIN,
                fingerprint or face. You sign in with it from then on, and can add your organization's sign-in later.
              </p>
            ) : (
              <p className="mt-2 text-sm text-gray-600">
                Sign in as <strong>{info.email}</strong> through {info.connector_display_name || info.connector_id}. Another
                account won't take the invitation.
              </p>
            )}
            <button className={`${primaryButton} mt-6 w-full`} disabled={busy} onClick={accept}>
              {passkey ? "Create a passkey and sign in" : `Sign in with ${info.connector_display_name || info.connector_id}`}
            </button>
          </>
        )}
      </div>
    </div>
  );
}
