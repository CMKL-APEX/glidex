// Small shared pieces for the access-control pages, in the same Tailwind
// style as the rest of the UI.
import type { ReactNode } from "react";
import type { Binding, EntityRef, Team, UserView } from "../types";
import { entityLabel, roleLabel } from "../types";

export const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";
export const selectClass = `${inputClass} bg-white`;
export const primaryButton =
  "px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg transition-colors disabled:opacity-50";
export const secondaryButton =
  "px-3 py-1.5 text-sm font-medium text-gray-700 bg-white border border-gray-300 hover:bg-gray-50 rounded-lg disabled:opacity-50";
export const dangerLink = "text-sm text-red-600 hover:text-red-800 disabled:opacity-50";

export function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function ErrorBanner({ error, onDismiss }: { error: string | null; onDismiss?: () => void }) {
  if (!error) return null;
  return (
    <div className="mb-4 p-4 bg-red-50 border border-red-200 rounded-lg">
      <div className="flex items-center justify-between gap-4">
        <p className="text-red-700 text-sm whitespace-pre-wrap">{error}</p>
        {onDismiss && (
          <button className="text-red-500 hover:text-red-700 text-sm" onClick={onDismiss}>
            Dismiss
          </button>
        )}
      </div>
    </div>
  );
}

export function PageHeader({ title, subtitle, children }: { title: string; subtitle?: ReactNode; children?: ReactNode }) {
  return (
    <div className="flex items-center justify-between mb-6 gap-4">
      <div>
        <h1 className="text-2xl font-bold text-gray-900">{title}</h1>
        {subtitle && <p className="text-gray-500 mt-1 text-sm">{subtitle}</p>}
      </div>
      {children && <div className="flex items-center gap-2">{children}</div>}
    </div>
  );
}

export function Card({ title, actions, children }: { title?: string; actions?: ReactNode; children: ReactNode }) {
  return (
    <section className="bg-white rounded-xl shadow-sm border border-gray-200 p-5 mb-6">
      {(title || actions) && (
        <div className="flex items-center justify-between mb-4 gap-4">
          {title && <h2 className="text-lg font-semibold text-gray-900">{title}</h2>}
          {actions && <div className="flex items-center gap-2">{actions}</div>}
        </div>
      )}
      {children}
    </section>
  );
}

const BADGE: Record<string, string> = {
  base: "bg-gray-100 text-gray-700",
  role: "bg-indigo-50 text-indigo-700",
  link: "bg-sky-50 text-sky-700",
  site: "bg-amber-50 text-amber-800",
  file: "bg-emerald-50 text-emerald-700",
  ok: "bg-green-50 text-green-700",
  denied: "bg-red-50 text-red-700",
  warn: "bg-amber-50 text-amber-800",
  muted: "bg-gray-100 text-gray-500",
};

export function Badge({ kind, children }: { kind: string; children: ReactNode }) {
  return (
    <span className={`inline-block px-2 py-0.5 text-xs font-medium rounded-full ${BADGE[kind] ?? BADGE.muted}`}>
      {children}
    </span>
  );
}

export function Field({ label, htmlFor, children }: { label: string; htmlFor?: string; children: ReactNode }) {
  return (
    <div>
      <label htmlFor={htmlFor} className="block text-sm font-medium text-gray-700">
        {label}
      </label>
      {children}
    </div>
  );
}

/** Names for principals, from whatever the caller may list. */
export interface Directory {
  users: UserView[];
  teams: Team[];
}

/** `known`: the name the server sent with the link, if any. */
export function principalName(e: EntityRef, dir: Directory, known?: string): string {
  if (e.type === "User") {
    const u = dir.users.find((x) => x.id === e.id);
    return u?.display_name ?? known ?? `user ${e.id.slice(0, 8)}`;
  }
  if (e.type === "Team") {
    if (e.id.startsWith("unix:")) return `team ${e.id}`;
    const t = dir.teams.find((x) => x.id === e.id);
    const name = t?.name ?? known;
    return name ? `team ${name}` : `team ${e.id.slice(0, 8)}`;
  }
  if (e.type === "Token") return known ? `token ${known}` : `token ${e.id.slice(0, 8)}`;
  return entityLabel(e);
}

/** A role-link table with optional remove buttons. */
/** How an identity signs in: a local account, or a connector by its name
 *  (`conn:<id>`; `oidc:<issuer>` is the file-defined OIDC connector). */
export function signInLabel(provider: string, connectors: Record<string, string>): string {
  if (provider === "pam" || provider === "unix") return "Local account";
  if (provider.startsWith("conn:")) {
    const id = provider.slice(5);
    return connectors[id] ?? id;
  }
  if (provider.startsWith("oidc:")) return connectors["file-oidc"] ?? "SSO";
  return provider;
}

/** The user an entity names, if it is one. */
export const userIdOf = (e: EntityRef): string | null => (e.type === "User" ? e.id : null);

/** A user's ways to sign in, as small badges: two people with one name,
 *  one through Google and one a local account, look different. */
export function SignInMethods({ user, connectors }: { user?: UserView; connectors: Record<string, string> }) {
  const labels = [...new Set((user?.identities ?? []).map((i) => signInLabel(i.provider, connectors)))];
  if (labels.length === 0) return null;
  return (
    <span className="ml-2 inline-flex flex-wrap gap-1 align-middle" data-testid="sign-in-methods">
      {labels.map((l) => (
        <span key={l} className="px-1.5 py-0.5 rounded bg-gray-100 text-gray-600 text-xs">
          {l}
        </span>
      ))}
    </span>
  );
}

export function BindingTable({
  bindings,
  dir,
  onRemove,
  connectors,
}: {
  bindings: Binding[];
  dir: Directory;
  onRemove?: (b: Binding) => void;
  /** With these (connector id → name), users show how they sign in. */
  connectors?: Record<string, string>;
}) {
  if (bindings.length === 0) return <p className="text-sm text-gray-500">No role links.</p>;
  return (
    <table className="w-full text-sm">
      <thead>
        <tr className="text-left text-gray-500 border-b border-gray-100">
          <th className="py-2 font-medium">Principal</th>
          <th className="py-2 font-medium">Role</th>
          <th className="py-2 font-medium">Link</th>
          {onRemove && <th className="py-2" />}
        </tr>
      </thead>
      <tbody>
        {bindings.map((b) => (
          <tr key={b.id} className="border-b border-gray-50">
            <td className="py-2" title={entityLabel(b.principal)}>
              {principalName(b.principal, dir, b.principal_name)}
              {connectors && userIdOf(b.principal) && (
                <SignInMethods user={dir.users.find((u) => u.id === userIdOf(b.principal))} connectors={connectors} />
              )}
            </td>
            <td className="py-2">
              <Badge kind="role">{roleLabel(b.role)}</Badge>
            </td>
            <td className="py-2 font-mono text-xs text-gray-500">{b.id}</td>
            {onRemove && (
              <td className="py-2 text-right">
                <button className={dangerLink} onClick={() => onRemove(b)}>
                  Remove
                </button>
              </td>
            )}
          </tr>
        ))}
      </tbody>
    </table>
  );
}
