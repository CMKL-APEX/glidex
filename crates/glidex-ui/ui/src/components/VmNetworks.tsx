import { useEffect, useState } from "react";
import * as api from "../api";
import type { Network, NicState, VmResponse } from "../types";
import { networkModeLabel, networkUsableBy, nicNetworkLabel } from "../types";

/** A VM's NICs: each detachable, and attaching one more on a network the
 * project may use. On a running VM both apply at its next start; until
 * then the NICs listed are the ones it runs with. */
export default function VmNetworks({
  vm,
  onChanged,
  canAttach = true,
  canDetach = true,
}: {
  vm: VmResponse;
  onChanged: () => void;
  /** What the caller may do here; without it the list is only shown. */
  canAttach?: boolean;
  canDetach?: boolean;
}) {
  const [networks, setNetworks] = useState<Network[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [picked, setPicked] = useState("");

  const nics = vm.nics ?? [];
  useEffect(() => {
    api
      .listNetworks()
      .then((all) => setNetworks(all.filter((n) => networkUsableBy(n, vm.project) && !n.deletion_requested_at)))
      .catch(() => setNetworks([]));
  }, [vm.project]);

  const detach = async (nic: NicState) => {
    if (!confirm(`Remove ${vm.name}'s NIC ${nic.mac} on ${nicNetworkLabel(nic)}?`)) return;
    setBusy(nic.mac);
    setError(null);
    try {
      // The MAC says which NIC, when the VM has several on the network.
      await api.detachVmNetwork(vm.id, nic.network, nic.mac);
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Removing the NIC failed");
    } finally {
      setBusy(null);
    }
  };

  const attach = async () => {
    setBusy(picked);
    setError(null);
    try {
      await api.attachVmNetwork(vm.id, picked);
      setPicked("");
      onChanged();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Attaching the network failed");
    } finally {
      setBusy(null);
    }
  };

  if (nics.length === 0 && (!canAttach || networks.length === 0)) return null;
  const button = "px-2 py-0.5 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-lg disabled:opacity-50";
  return (
    <div data-testid="vm-networks">
      <h3 className="text-sm font-medium text-gray-500">Network</h3>
      <ul className="mt-1 space-y-1 text-sm">
        {nics.map((nic) => (
          <li key={nic.mac} className="flex flex-wrap items-center gap-2 font-mono">
            <span className="text-gray-900" title={nic.network}>
              {nicNetworkLabel(nic)}
            </span>
            <span className="text-gray-500">{nic.mac}</span>
            <span className={nic.ipv4 ? "text-green-700" : "text-gray-400"}>{nic.ipv4 ?? "no address"}</span>
            {canDetach && (
              <button className={`${button} font-sans`} disabled={busy !== null || !!vm.deleting} onClick={() => detach(nic)}>
                {busy === nic.mac ? "Removing…" : "Detach"}
              </button>
            )}
          </li>
        ))}
      </ul>
      {canAttach && networks.length > 0 && !vm.deleting && (
        <div className="mt-2 flex flex-wrap items-center gap-2">
          <label htmlFor="attach-network" className="sr-only">
            Network to attach
          </label>
          <select
            id="attach-network"
            className="px-2 py-1 border border-gray-300 rounded-sm text-sm bg-white"
            value={picked}
            onChange={(e) => setPicked(e.target.value)}
            disabled={busy !== null}
          >
            <option value="">Attach a network…</option>
            {networks.map((n) => (
              <option key={n.id} value={n.id}>
                {n.name} ({networkModeLabel(n.mode)})
              </option>
            ))}
          </select>
          <button className={button} disabled={!picked || busy !== null} onClick={attach}>
            {busy !== null && busy === picked ? "Attaching…" : "Attach"}
          </button>
        </div>
      )}
      {error && <p className="mt-1 text-xs text-red-600">{error}</p>}
    </div>
  );
}
