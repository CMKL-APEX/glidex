import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import * as api from "../api";
import { useSession } from "../session";
import { Card, ErrorBanner, PageHeader, errorMessage, inputClass, primaryButton, secondaryButton } from "../components/ui";

/** `/device?directory=…&code=…` (spec/19-tenancy.md D54): approve a device
 * login, such as `gxctl auth login --directory`, as the signed-in user. */
export default function Device() {
  const [params] = useSearchParams();
  const directory = params.get("directory") || "local";
  const { me } = useSession();
  const [code, setCode] = useState(params.get("code") ?? "");
  const [found, setFound] = useState<{ client_name: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<"approved" | "denied" | null>(null);
  const [busy, setBusy] = useState(false);

  const look = async (c: string) => {
    setError(null);
    setFound(null);
    try {
      setFound(await api.inspectDeviceLogin(directory, c));
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  useEffect(() => {
    const c = params.get("code");
    if (c) look(c);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const answer = async (approve: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.answerDeviceLogin(directory, code, approve);
      setDone(approve ? "approved" : "denied");
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const who = me.user?.display_name ?? "you";
  return (
    <div className="max-w-xl">
      <PageHeader title="Device login" subtitle="Sign in a command-line tool or another device with your account." />
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <Card title="Code">
        {done ? (
          <p className="text-sm" data-testid="device-done">
            {done === "approved" ? "Approved. You can go back to the device; it is signed in as " + who + "." : "Denied. The device is not signed in."}
          </p>
        ) : (
          <div className="space-y-4 text-sm">
            <div>
              <label htmlFor="device-code" className="block text-xs text-gray-500">
                The code the device shows
              </label>
              <div className="flex gap-2">
                <input
                  id="device-code"
                  className={`${inputClass} font-mono uppercase`}
                  value={code}
                  placeholder="ABCD-EFGH"
                  onChange={(e) => {
                    setCode(e.target.value);
                    setFound(null);
                  }}
                />
                {!found && (
                  <button className={secondaryButton} disabled={!code.trim()} onClick={() => look(code)}>
                    Continue
                  </button>
                )}
              </div>
            </div>
            {found && (
              <>
                <p>
                  <span className="font-medium">{found.client_name}</span> asks to sign in as <span className="font-medium">{who}</span>. Approve it
                  only if you started this login and the code matches the one it shows.
                </p>
                <div className="flex gap-2">
                  <button className={primaryButton} disabled={busy} onClick={() => answer(true)}>
                    Approve
                  </button>
                  <button className={secondaryButton} disabled={busy} onClick={() => answer(false)}>
                    Deny
                  </button>
                </div>
              </>
            )}
          </div>
        )}
      </Card>
    </div>
  );
}
