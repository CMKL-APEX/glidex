import { useCallback, useEffect, useState } from "react";
import * as api from "../api";
import { ApiRequestError } from "../api";
import type { BridgeRecord, EntityRef, OvsProfile, OvsStatus } from "../types";
import { netdAccessLabel } from "../types";
import BridgesCard from "./BridgesCard";
import Modal from "./Modal";
import { LoadingCard } from "./Loading";

const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";

function errorText(e: unknown): string {
  if (e instanceof ApiRequestError && e.details?.missing?.length) {
    return `${e.message} (missing: ${e.details.missing.join(", ")})`;
  }
  return e instanceof Error ? e.message : String(e);
}

function Badge({ ok, children }: { ok: boolean; children: React.ReactNode }) {
  return (
    <span
      className={`px-2 py-0.5 text-xs font-medium rounded-full ${
        ok ? "bg-green-100 text-green-800" : "bg-yellow-100 text-yellow-800"
      }`}
    >
      {children}
    </span>
  );
}

function StatusPanel({
  status,
  onInstall,
}: {
  status: OvsStatus;
  onInstall: () => void;
}) {
  if (!status.netd.available) {
    return (
      <div className="p-4 bg-yellow-50 border border-yellow-200 rounded-lg text-yellow-800">
        glidex-netd is not reachable ({status.netd.error ?? "unknown error"}).
        Install it with <code>glidex-install</code> and make sure the control
        plane's user is in the <code>glidex</code> group.
      </div>
    );
  }
  const host = status.host ?? {};
  return (
    <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-6">
      <div className="flex items-center justify-between">
        <div className="space-y-1 text-sm">
          <p>
            <span className="text-gray-500">Open vSwitch:</span>{" "}
            {host.ovs_installed ? (
              <>
                <span className="font-medium">{host.ovs_version}</span>{" "}
                <Badge ok={!!host.ovs_running}>
                  {host.ovs_running ? "running" : "not running"}
                </Badge>
              </>
            ) : (
              <Badge ok={false}>not installed</Badge>
            )}
          </p>
          <p>
            <span className="text-gray-500">glidex-netd access:</span>{" "}
            <Badge ok={status.netd.access === "NETD_ACCESS_FULL"}>{netdAccessLabel(status.netd.access)}</Badge>
          </p>
          {(host.firewalls ?? []).length > 0 && (
            <p className="text-yellow-700">
              Active firewall ({host.firewalls?.join(", ")}) may drop forwarded
              NAT traffic; allow the NAT subnets there.
            </p>
          )}
        </div>
        {status.netd.access === "NETD_ACCESS_FULL" && (
          <button
            className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg"
            onClick={onInstall}
          >
            {host.ovs_installed ? "Install / switch profile" : "Install Open vSwitch"}
          </button>
        )}
      </div>
      <table className="w-full text-sm mt-4">
        <tbody className="divide-y divide-gray-100">
          {(host.combinations ?? []).map((c) => (
            <tr key={c.id}>
              <td className="py-2 pr-3 font-mono">{c.id}</td>
              <td className="py-2 pr-3">{c.description}</td>
              <td className="py-2 text-right">
                {c.available ? (
                  <Badge ok>available</Badge>
                ) : (
                  <span className="text-xs text-gray-500">
                    missing: {(c.missing ?? []).join(", ")}
                  </span>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function InstallDialog({
  node,
  onClose,
  onDone,
}: {
  node?: string;
  onClose: () => void;
  onDone: () => void;
}) {
  const [profile, setProfile] = useState<OvsProfile>("OVS_PROFILE_KERNEL");
  const [impact, setImpact] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async (confirm: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.installOvs(profile, confirm, node);
      onDone();
    } catch (e) {
      if (e instanceof ApiRequestError && e.code === "confirmation_required") {
        setImpact(e.details?.impact ?? e.message);
      } else {
        setError(errorText(e));
      }
      setBusy(false);
    }
  };

  return (
    <Modal title="Install Open vSwitch" onClose={onClose}>
      <div className="space-y-4">
        {error && <p className="text-sm text-red-600">{error}</p>}
        {impact ? (
          <>
            <p className="text-sm text-yellow-800 bg-yellow-50 border border-yellow-200 rounded-lg p-3">
              {impact}
            </p>
            <div className="flex justify-end space-x-3">
              <button className="px-4 py-2 text-sm bg-gray-200 rounded-lg" onClick={onClose}>
                Cancel
              </button>
              <button
                className="px-4 py-2 text-sm text-white bg-red-600 hover:bg-red-700 rounded-lg disabled:opacity-50"
                disabled={busy}
                onClick={() => run(true)}
              >
                {busy ? "Installing..." : "Restart and install"}
              </button>
            </div>
          </>
        ) : (
          <>
            <div>
              <label className="block text-sm font-medium text-gray-700">Profile</label>
              <select
                className={`${inputClass} bg-white`}
                value={profile}
                onChange={(e) => setProfile(e.target.value as OvsProfile)}
              >
                <option value="OVS_PROFILE_KERNEL">kernel (NAT, bridged, AF_XDP with tap)</option>
                <option value="OVS_PROFILE_DPDK">dpdk (vhost-user, DPDK uplinks)</option>
              </select>
              <p className="mt-1 text-xs text-gray-500">
                Installs distribution packages; may take a few minutes.
              </p>
            </div>
            <div className="flex justify-end space-x-3">
              <button className="px-4 py-2 text-sm bg-gray-200 rounded-lg" onClick={onClose}>
                Cancel
              </button>
              <button
                className="px-4 py-2 text-sm text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50"
                disabled={busy}
                onClick={() => run(false)}
              >
                {busy ? "Installing..." : "Install"}
              </button>
            </div>
          </>
        )}
      </div>
    </Modal>
  );
}


/** A host's Open vSwitch, install and bridges (spec/28-host-page.md W1), on
 * `node` through the server serving the UI (W2). Hidden without the right
 * to read the host's OVS status. */
export default function HostNetworking({ node, host }: { node: string; host: EntityRef }) {
  const [status, setStatus] = useState<OvsStatus | null>(null);
  const [bridges, setBridges] = useState<BridgeRecord[]>([]);
  const [forbidden, setForbidden] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [installing, setInstalling] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const st = await api.ovsStatus(node);
      setStatus(st);
      setBridges(st.netd.access === "NETD_ACCESS_FULL" ? await api.listBridges(node) : []);
      setError(null);
    } catch (e) {
      if (e instanceof ApiRequestError && e.status === 403) setForbidden(true);
      else setError(errorText(e));
    }
  }, [node]);
  useEffect(() => {
    refresh();
  }, [refresh]);

  if (forbidden) return null;
  return (
    <section className="space-y-4 mt-6" aria-label="Host networking">
      <h2 className="text-lg font-semibold text-gray-900">Networking</h2>
      {error && <div className="p-4 bg-red-50 border border-red-200 rounded-lg text-red-700">{error}</div>}
      {status ? <StatusPanel status={status} onInstall={() => setInstalling(true)} /> : !error && <LoadingCard />}
      {status?.netd.access === "NETD_ACCESS_FULL" && <BridgesCard bridges={bridges} node={node} host={host} onChanged={refresh} />}
      {installing && (
        <InstallDialog
          node={node}
          onClose={() => setInstalling(false)}
          onDone={() => {
            setInstalling(false);
            refresh();
          }}
        />
      )}
    </section>
  );
}
