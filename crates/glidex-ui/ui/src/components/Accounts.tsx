import { useEffect, useRef, useState } from "react";
import { NavLink } from "react-router-dom";
import * as api from "../api";
import type { BrowserAccount } from "../types";

/** The browser's accounts (spec/tenancy.md D71), `null` while loading. */
export function useAccounts(): [BrowserAccount[] | null, number, (a: BrowserAccount[]) => void] {
  const [accounts, setAccounts] = useState<BrowserAccount[] | null>(null);
  const [max, setMax] = useState(5);
  useEffect(() => {
    let live = true;
    api
      .listAccounts()
      .then((r) => {
        if (!live) return;
        setAccounts(r.accounts);
        setMax(r.max);
      })
      .catch(() => live && setAccounts([]));
    return () => {
      live = false;
    };
  }, []);
  return [accounts, max, setAccounts];
}

/** Switch to `a`, or sign in to it again; then the app starts over. */
async function pick(a: BrowserAccount) {
  if (a.active) return;
  if (!a.signed_in) {
    api.signInAgain(a);
    return;
  }
  await api.switchAccount(a.directory);
  window.location.assign("/");
}

async function addAnother() {
  await api.addAccount();
  window.location.assign("/");
}

async function logoutAll() {
  await api.logoutAll().catch(() => {});
  window.location.assign("/");
}

function Row({ a }: { a: BrowserAccount }) {
  return (
    <span className="flex flex-col text-left">
      <span className="text-sm text-gray-800">{a.user_name ?? a.directory_name}</span>
      <span className="text-xs text-gray-500">
        {a.directory_name}
        {!a.signed_in && " · signed out"}
      </span>
    </span>
  );
}

/** The header's account menu: the user's name opens their accounts. */
export function AccountMenu({ who }: { who: string }) {
  const [open, setOpen] = useState(false);
  const [accounts, max] = useAccounts();
  const [error, setError] = useState<string | null>(null);
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (box.current && !box.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, [open]);
  const run = (f: () => Promise<void>) => () => f().catch((e) => setError(e instanceof Error ? e.message : String(e)));
  const others = (accounts ?? []).filter((a) => !a.active);
  const full = (accounts?.length ?? 0) >= max;
  return (
    <div className="relative" ref={box}>
      <button
        type="button"
        className="text-sm text-gray-700 hover:underline"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
        title="Your accounts"
      >
        <span>{who}</span>
        <span aria-hidden="true"> ▾</span>
      </button>
      {open && (
        <div role="menu" className="absolute right-0 z-20 mt-2 w-72 max-w-[calc(100vw-2rem)] bg-white border border-gray-200 rounded-lg shadow-lg py-1" data-testid="account-menu">
          {(accounts ?? []).filter((a) => a.active).map((a) => (
            <div key={a.directory} className="px-3 py-2 border-b border-gray-100">
              <Row a={a} />
            </div>
          ))}
          <NavLink to="/account" role="menuitem" className="block px-3 py-2 text-sm text-gray-700 hover:bg-gray-50" onClick={() => setOpen(false)}>
            Account settings
          </NavLink>
          {others.length > 0 && <div className="px-3 pt-2 pb-1 text-xs text-gray-400">Switch to</div>}
          {others.map((a) => (
            <button key={a.directory} type="button" role="menuitem" className="w-full px-3 py-2 hover:bg-gray-50" onClick={run(() => pick(a))}>
              <Row a={a} />
            </button>
          ))}
          <div className="border-t border-gray-100 mt-1 pt-1">
            <button
              type="button"
              role="menuitem"
              className="w-full text-left px-3 py-2 text-sm text-sky-700 hover:bg-gray-50 disabled:text-gray-400"
              disabled={full}
              title={full ? `At most ${max} accounts: remove one on the Account page` : undefined}
              onClick={run(addAnother)}
            >
              Add another account
            </button>
            {others.length > 0 && (
              <button type="button" role="menuitem" className="w-full text-left px-3 py-2 text-sm text-gray-700 hover:bg-gray-50" onClick={run(logoutAll)}>
                Sign out of all accounts
              </button>
            )}
          </div>
          {error && <p className="px-3 py-1 text-xs text-red-600">{error}</p>}
        </div>
      )}
    </div>
  );
}

/** The accounts as a list: the Account page's (with Remove) and the
 *  login page's (to switch back without signing in). */
export function AccountList({ removable = false, heading }: { removable?: boolean; heading?: string }) {
  const [accounts, , setAccounts] = useAccounts();
  const [error, setError] = useState<string | null>(null);
  if (!accounts || accounts.length === 0) return null;
  const shown = removable ? accounts : accounts.filter((a) => !a.active);
  if (shown.length === 0) return null;
  const remove = async (a: BrowserAccount) => {
    try {
      setAccounts(await api.removeAccount(a.directory));
      if (a.active) window.location.assign("/");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  return (
    <div data-testid="account-list">
      {heading && <p className="mb-1 text-xs text-gray-500">{heading}</p>}
      <ul className="divide-y divide-gray-100 border border-gray-200 rounded-lg">
        {shown.map((a) => (
          <li key={a.directory} className="flex items-center justify-between gap-2 px-3 py-2">
            <Row a={a} />
            <span className="flex gap-2 shrink-0">
              {a.active ? (
                <span className="text-xs text-gray-500">current</span>
              ) : (
                <button type="button" className="text-sm text-sky-700 hover:underline" onClick={() => pick(a).catch((e) => setError(String(e)))}>
                  {a.signed_in ? "Switch" : "Sign in"}
                </button>
              )}
              {removable && (
                <button type="button" className="text-sm text-red-600 hover:underline" onClick={() => remove(a)}>
                  Remove
                </button>
              )}
            </span>
          </li>
        ))}
      </ul>
      {error && <p className="mt-1 text-xs text-red-600">{error}</p>}
    </div>
  );
}
