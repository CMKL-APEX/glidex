import { useState, type FormEvent } from "react";
import * as api from "../api";
import PasskeyButton from "../components/PasskeyButton";
import { secondaryButton } from "../components/ui";
import { isOidcKind, offersSso, type AuthMethods } from "../types";

const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";

/** Username and password for a PAM login (also the re-login dialog). */
export function PamForm({
  initialUsername = "",
  submitLabel,
  onDone,
}: {
  initialUsername?: string;
  submitLabel: string;
  onDone: () => void;
}) {
  const [username, setUsername] = useState(initialUsername);
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      await api.loginPam(username, password);
      setPassword("");
      onDone();
    } catch (err) {
      setError(err instanceof api.ApiRequestError && err.code === "login_failed" ? "Wrong username or password." : String(err instanceof Error ? err.message : err));
      setSubmitting(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      <div>
        <label htmlFor="login-username" className="block text-sm font-medium text-gray-700">
          Username
        </label>
        <input
          id="login-username"
          className={inputClass}
          autoComplete="username"
          required
          value={username}
          onChange={(e) => setUsername(e.target.value)}
        />
      </div>
      <div>
        <label htmlFor="login-password" className="block text-sm font-medium text-gray-700">
          Password
        </label>
        <input
          id="login-password"
          type="password"
          className={inputClass}
          autoComplete="current-password"
          required
          value={password}
          onChange={(e) => setPassword(e.target.value)}
        />
      </div>
      {error && <p className="text-sm text-red-600">{error}</p>}
      <button
        type="submit"
        disabled={submitting}
        className="w-full px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50"
      >
        {submitting ? "Signing in..." : submitLabel}
      </button>
    </form>
  );
}

function currentPath(): string {
  return window.location.pathname + window.location.search;
}

/** One button per OIDC-code connector of the directory (`oidc`, the
 *  `google` and `jumpcloud` presets); one plain "SSO" button for a server
 *  that doesn't list them. */
export function SsoButtons({ methods, reauth = false }: { methods: AuthMethods; reauth?: boolean }) {
  const oidc = methods.connectors?.filter((c) => isOidcKind(c.kind));
  const buttons = oidc ? oidc.map((c) => ({ id: c.id as string | undefined, label: c.display_name || "SSO" })) : [{ id: undefined, label: "SSO" }];
  return (
    <div className="space-y-2">
      {buttons.map((b) => (
        <button
          key={b.id ?? "sso"}
          type="button"
          onClick={() => api.startOidc(currentPath(), reauth, b.id)}
          className="w-full px-4 py-2 text-sm font-medium text-sky-700 bg-white border border-sky-300 hover:bg-sky-50 rounded-lg"
        >
          Sign in with {b.label}
        </button>
      ))}
    </div>
  );
}

/** "Your email" (spec/tenancy.md D61): an organization whose sign-in this
 * host serves has its own login page; the email finds it. */
function EmailRoute({ current }: { current: string }) {
  const [email, setEmail] = useState("");
  const [note, setNote] = useState<string | null>(null);
  const go = async (e: FormEvent) => {
    e.preventDefault();
    setNote(null);
    try {
      const r = await api.discoverLogin(email.trim());
      if (r.directory && r.directory !== current) window.location.assign(`/login/${encodeURIComponent(r.directory)}`);
      else setNote("Sign in below.");
    } catch {
      setNote("Sign in below.");
    }
  };
  return (
    <form className="mt-4 text-sm" onSubmit={go}>
      <label htmlFor="login-email" className="block text-xs text-gray-500">
        Signing in with your organization? Your email
      </label>
      <div className="flex gap-2">
        <input id="login-email" type="email" className={inputClass} value={email} onChange={(e) => setEmail(e.target.value)} placeholder="you@example.org" />
        <button type="submit" className={secondaryButton} disabled={!email.includes("@")}>
          Continue
        </button>
      </div>
      {note && <p className="mt-1 text-xs text-gray-500">{note}</p>}
    </form>
  );
}

export default function Login({ methods, onLoggedIn }: { methods: AuthMethods; onLoggedIn: () => void }) {
  const sso = offersSso(methods);
  const none = !methods.pam && !sso;
  const directory = methods.directory ?? "local";
  return (
    <div className="min-h-screen flex items-start justify-center pt-24 px-4">
      <div className="max-w-sm w-full bg-white rounded-xl shadow-md border border-gray-100 p-6">
        <div className="mb-6 text-center">
          <div className="text-2xl font-bold text-sky-700">GlideX</div>
          <h1 className="mt-1 text-sm text-gray-500">Sign in to the VM Control Panel</h1>
          {directory !== "local" && (
            <p className="mt-1 text-xs text-gray-500" data-testid="login-directory">
              Directory <span className="font-mono">{directory}</span>
            </p>
          )}
        </div>
        {methods.pam && <PamForm submitLabel="Sign in" onDone={onLoggedIn} />}
        {methods.pam && sso && (
          <div className="my-4 flex items-center text-xs text-gray-400">
            <span className="flex-1 border-t border-gray-200" />
            <span className="px-2">or</span>
            <span className="flex-1 border-t border-gray-200" />
          </div>
        )}
        {sso && <SsoButtons methods={methods} />}
        {methods.passkeys && (methods.pam || sso) && <div className="my-3" />}
        {methods.passkeys && <PasskeyButton onDone={onLoggedIn} />}
        {directory === "local" && <EmailRoute current={directory} />}
        {none && !methods.passkeys && (
          <p className="text-sm text-gray-600">
            No login method is enabled on this host. Ask an administrator, or use <code>gxctl</code> on the host.
          </p>
        )}
      </div>
    </div>
  );
}
