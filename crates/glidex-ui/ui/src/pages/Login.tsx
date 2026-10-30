import { useState, type FormEvent } from "react";
import { currentPath, PamForm, SsoButtons as KitSsoButtons, type SsoProvider } from "@glidex/ui-kit";
import * as api from "../api";
import { AccountList } from "../components/Accounts";
import PasskeyButton from "../components/PasskeyButton";
import { secondaryButton } from "../components/ui";
import { isOidcKind, offersSso, type AuthMethods } from "../types";

const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";

/** The single sign-on choices: one per OIDC-code connector of the
 *  directory (`oidc`, the `google` and `jumpcloud` presets); one plain
 *  "SSO" for a server that doesn't list them. */
export function ssoProviders(methods: AuthMethods): SsoProvider[] {
  const oidc = methods.connectors?.filter((c) => isOidcKind(c.kind));
  return oidc ? oidc.map((c) => ({ id: c.id as string | undefined, label: c.display_name || "SSO" })) : [{ id: undefined, label: "SSO" }];
}

/** A "Sign in with" button for each of ssoProviders(). */
export function SsoButtons({ methods, reauth = false }: { methods: AuthMethods; reauth?: boolean }) {
  return <KitSsoButtons providers={ssoProviders(methods)} reauth={reauth} />;
}

/** "Sign in with <organization>" for each directory of another
 *  organization this cluster trusts (spec/19-tenancy.md D69): its login page,
 *  then back here. */
export function TrustedButtons({ methods }: { methods: AuthMethods }) {
  const ds = methods.trusted_directories ?? [];
  if (ds.length === 0) return null;
  return (
    <div className="mt-4 space-y-2" data-testid="trusted-directories">
      {ds.map((d) => (
        <button
          key={d.id}
          type="button"
          onClick={() => api.startTrustedLogin(d.id, currentPath())}
          className="w-full px-4 py-2 text-sm font-medium text-sky-700 bg-white border border-sky-300 hover:bg-sky-50 rounded-lg"
        >
          Sign in with {d.tenants.length > 0 ? d.tenants.join(", ") : d.host}
          {d.tenants.length > 0 && <span className="block text-xs font-normal text-gray-500">at {d.host}</span>}
        </button>
      ))}
    </div>
  );
}

/** "Your email" (spec/19-tenancy.md D61): an organization whose sign-in this
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
  const none = !methods.pam && !sso && !(methods.trusted_directories?.length ?? 0);
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
        <div className="mt-4">
          <AccountList heading="Your accounts in this browser" />
        </div>
        {directory === "local" && <TrustedButtons methods={methods} />}
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
