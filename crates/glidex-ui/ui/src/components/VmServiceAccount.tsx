import { useEffect, useState } from "react";
import * as api from "../api";
import type { ApiToken, VmResponse } from "../types";

/** A VM's service account (spec/29-workload-identity.md W5, W6): the glidex
 * identity its guest gets short-lived tokens for from its metadata
 * endpoint. Changing it applies to the next token, no restart. */
export default function VmServiceAccount({ vm, onChanged }: { vm: VmResponse; onChanged: () => void }) {
  const [accounts, setAccounts] = useState<ApiToken[]>([]);
  const [choice, setChoice] = useState(vm.service_account ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .listServiceAccounts(vm.project)
      .then(setAccounts)
      .catch(() => setAccounts([]));
  }, [vm.project]);
  useEffect(() => setChoice(vm.service_account ?? ""), [vm.service_account]);

  const name = (id?: string) => accounts.find((a) => a.id === id)?.name ?? id;
  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.setVmServiceAccount(vm.id, choice || null);
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Changing the service account failed");
    } finally {
      setBusy(false);
    }
  };

  // Nothing to show or pick.
  if (!vm.service_account && accounts.length === 0) return null;
  return (
    <div data-testid="vm-service-account">
      <h3 className="text-sm font-medium text-gray-500">Service Account</h3>
      <p className="text-sm text-gray-700">
        {vm.service_account ? <span className="font-mono">{name(vm.service_account)}</span> : "None"}
      </p>
      <div className="mt-2 flex flex-wrap items-center gap-2 text-sm">
        <select
          className="px-2 py-1 border border-gray-300 rounded-lg bg-white"
          aria-label="Service account"
          value={choice}
          onChange={(e) => setChoice(e.target.value)}
        >
          <option value="">None</option>
          {accounts.map((a) => (
            <option key={a.id} value={a.id}>
              {a.name}
            </option>
          ))}
        </select>
        <button
          className="px-3 py-1 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-lg disabled:opacity-50"
          disabled={busy || choice === (vm.service_account ?? "")}
          onClick={save}
        >
          Use
        </button>
      </div>
      <p className="mt-1 text-xs text-gray-500">
        The guest gets short-lived tokens acting as it from its metadata endpoint; it holds no secret.
      </p>
      {error && <p className="mt-1 text-xs text-red-700">{error}</p>}
    </div>
  );
}
