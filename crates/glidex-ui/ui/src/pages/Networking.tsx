import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import { ApiRequestError } from "../api";
import type {
  BridgeRecord,
  Network,
  NetworkMode,
  OvsStatus,
  PortType,
} from "../types";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";

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
  const host = status.host!;
  return (
    <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-6">
      <div className="flex items-center justify-between">
        <div className="space-y-1 text-sm">
          <p>
            <span className="text-gray-500">Open vSwitch:</span>{" "}
            {host.ovs_installed ? (
              <>
                <span className="font-medium">{host.ovs_version}</span>{" "}
                <Badge ok={host.ovs_running}>
                  {host.ovs_running ? "running" : "not running"}
                </Badge>
              </>
            ) : (
              <Badge ok={false}>not installed</Badge>
            )}
          </p>
          <p>
            <span className="text-gray-500">glidex-netd access:</span>{" "}
            <Badge ok={status.netd.access === "full"}>{status.netd.access}</Badge>
          </p>
          {host.firewalls.length > 0 && (
            <p className="text-yellow-700">
              Active firewall ({host.firewalls.join(", ")}) may drop forwarded
              NAT traffic; allow the NAT subnets there.
            </p>
          )}
        </div>
        {status.netd.access === "full" && (
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
          {host.combinations.map((c) => (
            <tr key={c.id}>
              <td className="py-2 pr-3 font-mono">{c.id}</td>
              <td className="py-2 pr-3">{c.description}</td>
              <td className="py-2 text-right">
                {c.available ? (
                  <Badge ok>available</Badge>
                ) : (
                  <span className="text-xs text-gray-500">
                    missing: {c.missing.join(", ")}
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
  onClose,
  onDone,
}: {
  onClose: () => void;
  onDone: () => void;
}) {
  const [profile, setProfile] = useState<"kernel" | "dpdk">("kernel");
  const [impact, setImpact] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async (confirm: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await api.installOvs(profile, confirm);
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
                onChange={(e) => setProfile(e.target.value as "kernel" | "dpdk")}
              >
                <option value="kernel">kernel (NAT, bridged, AF_XDP with tap)</option>
                <option value="dpdk">dpdk (vhost-user, DPDK uplinks)</option>
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

function AddNetworkForm({
  bridges,
  onDone,
  onCancel,
}: {
  bridges: BridgeRecord[];
  onDone: () => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [mode, setMode] = useState<NetworkMode>("nat");
  const [portType, setPortType] = useState<PortType>("tap");
  const [subnet, setSubnet] = useState("");
  const [bridge, setBridge] = useState("");
  const [vlan, setVlan] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.createNetwork({
        name,
        mode,
        port_type: portType,
        subnet: mode === "nat" && subnet ? subnet : undefined,
        bridge: bridge || undefined,
        vlan: vlan ? Number(vlan) : undefined,
      });
      onDone();
    } catch (err) {
      setError(errorText(err));
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <div>
        <label className="block text-sm font-medium text-gray-700">Name</label>
        <input
          className={inputClass}
          required
          pattern="[a-z0-9][a-z0-9\-]{0,31}"
          title="Lowercase letters, digits and -"
          placeholder="lab"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
      </div>
      <div className="grid grid-cols-2 gap-4">
        <div>
          <label className="block text-sm font-medium text-gray-700">Mode</label>
          <select className={`${inputClass} bg-white`} value={mode} onChange={(e) => setMode(e.target.value as NetworkMode)}>
            <option value="nat">NAT (DHCP + masquerade)</option>
            <option value="isolated">Isolated (VM-to-VM)</option>
            <option value="bridged">Bridged (existing bridge)</option>
          </select>
        </div>
        <div>
          <label className="block text-sm font-medium text-gray-700">VM port</label>
          <select className={`${inputClass} bg-white`} value={portType} onChange={(e) => setPortType(e.target.value as PortType)}>
            <option value="tap">tap</option>
            <option value="vhost_user">vhost-user (DPDK)</option>
          </select>
        </div>
      </div>
      {mode === "nat" && (
        <div>
          <label className="block text-sm font-medium text-gray-700">Subnet (optional)</label>
          <input className={inputClass} placeholder="auto: first free /24 in 10.88.0.0/16" value={subnet} onChange={(e) => setSubnet(e.target.value)} />
        </div>
      )}
      <div className="grid grid-cols-2 gap-4">
        <div>
          <label className="block text-sm font-medium text-gray-700">
            Bridge {mode === "bridged" ? "" : "(optional)"}
          </label>
          {mode === "bridged" ? (
            <select className={`${inputClass} bg-white`} required value={bridge} onChange={(e) => setBridge(e.target.value)}>
              <option value="">Select a glidex bridge</option>
              {bridges.map((b) => (
                <option key={b.spec.name} value={b.spec.name}>{b.spec.name}</option>
              ))}
            </select>
          ) : (
            <input className={inputClass} placeholder={`gxbr-${name || "<name>"}`} value={bridge} onChange={(e) => setBridge(e.target.value)} />
          )}
        </div>
        <div>
          <label className="block text-sm font-medium text-gray-700">VLAN (optional)</label>
          <input className={inputClass} type="number" min={1} max={4094} value={vlan} onChange={(e) => setVlan(e.target.value)} />
        </div>
      </div>
      <div className="flex justify-end space-x-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm bg-gray-200 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" disabled={busy} className="px-4 py-2 text-sm text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50">
          {busy ? "Creating..." : "Create"}
        </button>
      </div>
    </form>
  );
}

export default function Networking() {
  const [status, setStatus] = useState<OvsStatus | null>(null);
  const [networks, setNetworks] = useState<Network[]>([]);
  const [bridges, setBridges] = useState<BridgeRecord[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [installing, setInstalling] = useState(false);
  const [adding, setAdding] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const s = await api.ovsStatus();
      setStatus(s);
      if (s.netd.access === "full") {
        const [n, b] = await Promise.all([api.listNetworks(), api.listBridges()]);
        n.sort((a, c) => a.name.localeCompare(c.name));
        setNetworks(n);
        setBridges(b);
      } else {
        setNetworks(await api.listNetworks());
      }
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const remove = async (name: string) => {
    if (!confirm(`Delete network "${name}"?`)) return;
    try {
      await api.deleteNetwork(name);
      refresh();
    } catch (e) {
      setError(errorText(e));
    }
  };

  const canManage = status?.netd.access === "full" && status.host?.ovs_running;

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">Networking</h1>
          <p className="text-gray-500 mt-1">
            Open vSwitch bridges and the networks VMs attach to (Cloud Hypervisor).
          </p>
        </div>
        {canManage && (
          <button
            className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg"
            onClick={() => setAdding(true)}
          >
            Add Network
          </button>
        )}
      </div>

      {error && (
        <div className="p-4 bg-red-50 border border-red-200 rounded-lg text-red-700">{error}</div>
      )}

      {status === null ? (
        <LoadingCard />
      ) : (
        <StatusPanel status={status} onInstall={() => setInstalling(true)} />
      )}

      <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-hidden">
        <table className="w-full text-sm">
          <thead className="bg-gray-50 text-gray-600 text-left">
            <tr>
              <th className="px-4 py-3 font-medium">Network</th>
              <th className="px-4 py-3 font-medium">Mode</th>
              <th className="px-4 py-3 font-medium">Bridge</th>
              <th className="px-4 py-3 font-medium">VM port</th>
              <th className="px-4 py-3 font-medium">VLAN</th>
              <th className="px-4 py-3" />
            </tr>
          </thead>
          <tbody className="divide-y divide-gray-100">
            {networks.length === 0 && (
              <tr>
                <td className="px-4 py-6 text-center text-gray-500" colSpan={6}>
                  No networks yet.
                </td>
              </tr>
            )}
            {networks.map((n) => (
              <tr key={n.name}>
                <td className="px-4 py-3 font-mono">{n.name}</td>
                <td className="px-4 py-3">{n.mode}</td>
                <td className="px-4 py-3 font-mono">{n.bridge}</td>
                <td className="px-4 py-3">{n.port_type === "vhost_user" ? "vhost-user" : "tap"}</td>
                <td className="px-4 py-3">{n.vlan ?? "—"}</td>
                <td className="px-4 py-3 text-right">
                  {canManage && (
                    <button
                      className="px-3 py-1 text-xs font-medium text-red-700 bg-red-50 hover:bg-red-100 rounded-lg"
                      onClick={() => remove(n.name)}
                    >
                      Delete
                    </button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {bridges.length > 0 && (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-4 text-sm">
          <h2 className="font-medium text-gray-700 mb-2">glidex bridges</h2>
          <ul className="space-y-1">
            {bridges.map((b) => (
              <li key={b.spec.name} className="font-mono">
                {b.spec.name}{" "}
                <span className="text-gray-500">
                  ({b.spec.datapath}
                  {b.live ? `, ${b.live.ports.length} ports` : ", missing on host"})
                </span>
              </li>
            ))}
          </ul>
        </div>
      )}

      {installing && (
        <InstallDialog
          onClose={() => setInstalling(false)}
          onDone={() => {
            setInstalling(false);
            refresh();
          }}
        />
      )}
      {adding && (
        <Modal title="Add Network" onClose={() => setAdding(false)}>
          <AddNetworkForm
            bridges={bridges}
            onCancel={() => setAdding(false)}
            onDone={() => {
              setAdding(false);
              refresh();
            }}
          />
        </Modal>
      )}
    </div>
  );
}
