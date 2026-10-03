import { useState, type FormEvent } from "react";
import * as api from "../api";
import type { AuthMethods } from "../types";

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

export function SsoButton({ reauth = false }: { reauth?: boolean }) {
  return (
    <button
      type="button"
      onClick={() => api.startOidc(currentPath(), reauth)}
      className="w-full px-4 py-2 text-sm font-medium text-sky-700 bg-white border border-sky-300 hover:bg-sky-50 rounded-lg"
    >
      Sign in with SSO
    </button>
  );
}

export default function Login({ methods, onLoggedIn }: { methods: AuthMethods; onLoggedIn: () => void }) {
  const none = !methods.pam && !methods.oidc;
  return (
    <div className="min-h-screen flex items-start justify-center pt-24 px-4">
      <div className="max-w-sm w-full bg-white rounded-xl shadow-md border border-gray-100 p-6">
        <div className="mb-6 text-center">
          <div className="text-2xl font-bold text-sky-700">GlideX</div>
          <h1 className="mt-1 text-sm text-gray-500">Sign in to the VM Control Panel</h1>
        </div>
        {methods.pam && <PamForm submitLabel="Sign in" onDone={onLoggedIn} />}
        {methods.pam && methods.oidc && (
          <div className="my-4 flex items-center text-xs text-gray-400">
            <span className="flex-1 border-t border-gray-200" />
            <span className="px-2">or</span>
            <span className="flex-1 border-t border-gray-200" />
          </div>
        )}
        {methods.oidc && <SsoButton />}
        {none && (
          <p className="text-sm text-gray-600">
            No login method is enabled on this host. Ask an administrator, or use <code>gxctl</code> on the host.
          </p>
        )}
      </div>
    </div>
  );
}
