import { useState } from "react";
import * as api from "../api";
import { errorMessage, secondaryButton } from "./ui";

/** "Sign in with a passkey" (spec/passkeys.md P3): no user name; the
 * browser asks which passkey, with the user's PIN, fingerprint or face. */
export default function PasskeyButton({ onDone, label = "Sign in with a passkey" }: { onDone: () => void; label?: string }) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  if (!api.passkeysSupported()) return null;
  return (
    <div>
      <button
        type="button"
        className={`w-full ${secondaryButton}`}
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          setError(null);
          try {
            await api.loginPasskey();
            onDone();
          } catch (e) {
            // The user closing the browser's prompt isn't an error to show.
            const m = errorMessage(e);
            if (!/NotAllowedError|aborted|cancel/i.test(m)) setError(m.includes("passkey_refused") ? "That passkey can't sign in here." : m);
          } finally {
            setBusy(false);
          }
        }}
      >
        {busy ? "Waiting for your passkey..." : label}
      </button>
      {error && <p className="mt-2 text-sm text-red-600">{error}</p>}
    </div>
  );
}
