import { useCallback, useEffect, useState, type FormEvent } from "react";
import { useSearchParams } from "react-router-dom";
import * as api from "../api";
import { ApiRequestError } from "../api";
import type {
  BridgeRecord,
  Network,
  NetworkMode,
  OvsStatus,
  PortType,
} from "../types";
import { networkUsableBy, notReady } from "../types";
import { useLiveRefresh } from "../live";
import { useCan, useSession } from "../session";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";
import NodeName from "../components/NodeName";
import { useNodes } from "../nodes";

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

/** Where a new network goes: `""` is a host network (net-admin), else a
 * project id (the project's owner; spec/security.md §6.2). */
type Scope = { value: string; label: string };

function AddNetworkForm({
  bridges,
  scopes,
  initialScope,
  onDone,
  onCancel,
}: {
  bridges: BridgeRecord[];
  scopes: Scope[];
  initialScope: string;
  onDone: () => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [scope, setScope] = useState(initialScope);
  const [mode, setMode] = useState<NetworkMode>("nat");
  const [portType, setPortType] = useState<PortType>("tap");
  const [subnet, setSubnet] = useState("");
  const [bridge, setBridge] = useState("");
  const [vlan, setVlan] = useState("");
  const [physnet, setPhysnet] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // In a cluster: one OVN network for every node, or a bridge on this node.
  const nodes = useNodes();
  const [span, setSpan] = useState<"cluster" | "node">("cluster");
  const clusterWide = nodes.clustered && span === "cluster";

  // Project networks are NAT or isolated, on a bridge glidex names.
  const project = scope !== "";
  const pickScope = (s: string) => {
    setScope(s);
    if (s !== "" && mode === "bridged") setMode("nat");
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      // Cluster networks get an address range for isolated mode too (cluster IPAM).
      const subnetValue = (mode === "nat" || (clusterWide && mode === "isolated")) && subnet ? subnet : undefined;
      const scopeValue = nodes.clustered ? span : undefined;
      if (project) {
        await api.createProjectNetwork(scope, { name, mode, port_type: portType, subnet: subnetValue, scope: scopeValue });
      } else {
        await api.createNetwork({
          name,
          mode,
          port_type: portType,
          subnet: subnetValue,
          scope: scopeValue,
          bridge: clusterWide ? undefined : bridge || undefined,
          vlan: vlan ? Number(vlan) : undefined,
          physnet: clusterWide && mode === "bridged" ? physnet : undefined,
        });
      }
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
        <label htmlFor="network-scope" className="block text-sm font-medium text-gray-700">
          Network for
        </label>
        <select id="network-scope" className={`${inputClass} bg-white`} value={scope} onChange={(e) => pickScope(e.target.value)}>
          {scopes.map((s) => (
            <option key={s.value} value={s.value}>
              {s.label}
            </option>
          ))}
        </select>
        <p className="mt-1 text-xs text-gray-500">
          {project
            ? "Only this project's VMs can use it, unless you share it. NAT or isolated; counts against the project's network quota."
            : "A host network: grant it to projects after creating it."}
        </p>
      </div>
      {nodes.clustered && (
        <div>
          <label htmlFor="network-span" className="block text-sm font-medium text-gray-700">
            Spans
          </label>
          <select id="network-span" className={`${inputClass} bg-white`} value={span} onChange={(e) => setSpan(e.target.value as "cluster" | "node")}>
            <option value="cluster">Every node (cluster network on OVN)</option>
            <option value="node">This node only (a bridge here)</option>
          </select>
          <p className="mt-1 text-xs text-gray-500">
            {span === "cluster"
              ? "VMs on any node can use it; traffic between nodes goes through Geneve tunnels."
              : "Only VMs placed on the node serving this page can use it."}
          </p>
        </div>
      )}
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
            {!project && <option value="bridged">{clusterWide ? "Provider (a physical network)" : "Bridged (existing bridge)"}</option>}
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
      {(mode === "nat" || (clusterWide && mode === "isolated")) && (
        <div>
          <label className="block text-sm font-medium text-gray-700">Subnet (optional)</label>
          <input
            className={inputClass}
            placeholder={clusterWide ? "auto: a free range from the cluster's pool" : "auto: first free /24 in 10.88.0.0/16"}
            value={subnet}
            onChange={(e) => setSubnet(e.target.value)}
          />
        </div>
      )}
      {clusterWide && mode === "bridged" && (
        <div className="grid grid-cols-2 gap-4">
          <div>
            <label className="block text-sm font-medium text-gray-700">Physical network</label>
            <input className={inputClass} required placeholder="physnet1" value={physnet} onChange={(e) => setPhysnet(e.target.value)} />
          </div>
          <div>
            <label className="block text-sm font-medium text-gray-700">VLAN (optional)</label>
            <input className={inputClass} type="number" min={1} max={4094} value={vlan} onChange={(e) => setVlan(e.target.value)} />
          </div>
        </div>
      )}
      {!project && !clusterWide && (
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
      )}
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

/** The network controller's view (spec/reconciliation.md §10.3). */
function NetworkStatus({ n }: { n: Network }) {
  const why = notReady(n.conditions);
  const phase = n.phase ?? "ready";
  if (phase === "ready" && !n.deletion_requested_at) return <span className="text-green-700">ready</span>;
  return (
    <div title={why ? `${why.reason}: ${why.message}` : undefined}>
      <span className={n.deletion_requested_at ? "text-gray-500" : phase === "degraded" ? "text-red-700" : "text-amber-700"}>
        {n.deletion_requested_at ? "deleting" : phase.replace("_", " ")}
      </span>
      {why?.message && <div className="text-gray-500 truncate max-w-xs">{why.message}</div>}
    </div>
  );
}

export default function Networking() {
  const { host, projects, project, projectName } = useSession();
  const [params, setParams] = useSearchParams();
  // Projects whose owners may create (and delete) project networks.
  const projectCan = useCan(projects.map((p) => ({ action: "createProjectNetwork", resource: { type: "Project" as const, id: p.id } })));
  const ownProjects = projects.filter((_, i) => projectCan?.[i]);
  const scopes: Scope[] = [
    ...(host.createNetwork ? [{ value: "", label: "Host network" }] : []),
    ...ownProjects.map((p) => ({ value: p.id, label: `Project ${projectName(p.id)}` })),
  ];
  const [status, setStatus] = useState<OvsStatus | null>(null);
  // Host status (Open vSwitch, bridges) needs host.read. Without it the
  // page falls back to the networks the current project can use.
  // `null` until the first answer.
  const [hostView, setHostView] = useState<boolean | null>(null);
  const [networks, setNetworks] = useState<Network[]>([]);
  const [bridges, setBridges] = useState<BridgeRecord[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [installing, setInstalling] = useState(false);
  const [adding, setAdding] = useState(false);
  // Host admins see every network; others the ones their project can use.
  const shown = hostView ? networks : networks.filter((n) => networkUsableBy(n, project));

  const refresh = useCallback(async () => {
    // Independent requests: the host status being forbidden must not hide
    // the network list.
    const [nets, st] = await Promise.allSettled([api.listNetworks(), api.ovsStatus()]);
    const errors: string[] = [];
    if (nets.status === "fulfilled") {
      setNetworks([...nets.value].sort((a, c) => a.name.localeCompare(c.name)));
    } else {
      errors.push(errorText(nets.reason));
    }
    if (st.status === "fulfilled") {
      setStatus(st.value);
      setHostView(true);
      if (st.value.netd.access === "full") {
        try {
          setBridges(await api.listBridges());
        } catch (e) {
          errors.push(errorText(e));
        }
      }
    } else if (st.reason instanceof ApiRequestError && st.reason.status === 403) {
      setStatus(null);
      setBridges([]);
      setHostView(false);
    } else {
      errors.push(errorText(st.reason));
      setHostView((v) => v ?? false);
    }
    setError(errors.length ? errors.join("; ") : null);
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Follow the live stream; without it, poll while a network is deleting.
  const live = useLiveRefresh(["network"], refresh);
  const deleting = shown.some((n) => n.deletion_requested_at);
  useEffect(() => {
    if (!deleting || live) return;
    const id = setInterval(refresh, 3000);
    return () => clearInterval(id);
  }, [deleting, live, refresh]);

  const remove = async (name: string) => {
    if (!confirm(`Delete network "${name}"?`)) return;
    try {
      await api.deleteNetwork(name);
      refresh();
    } catch (e) {
      setError(errorText(e));
    }
  };

  // Without the host status, the server decides whether netd can do it.
  const netdReady = hostView === false || (status?.netd.access === "full" && !!status.host?.ovs_running);
  const canAdd = netdReady && scopes.length > 0;
  const canDelete = (n: Network) =>
    netdReady && (n.project ? ownProjects.some((p) => p.id === n.project) : host.createNetwork);
  // `?new=<project id>` (from a project's page) opens the form for that project.
  const requested = params.get("new");
  // Otherwise a host network when allowed, else the selected project.
  const initialScope =
    scopes.find((s) => s.value === requested)?.value ??
    (host.createNetwork ? "" : (scopes.find((s) => s.value === project)?.value ?? scopes[0]?.value ?? ""));
  useEffect(() => {
    if (requested !== null && canAdd && scopes.some((s) => s.value === requested)) setAdding(true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [requested, canAdd]);
  const closeForm = () => {
    setAdding(false);
    if (requested !== null) setParams({}, { replace: true });
  };
  const { clustered } = useNodes();
  // Without host rights, a network is shown by how this project gets it.
  const scope = (n: Network) =>
    n.project ? (n.project === project ? "project" : `shared by ${projectName(n.project)}`) : "host";

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">Networking</h1>
          <p className="text-gray-500 mt-1">
            {hostView === false
              ? `Networks the project ${projectName(project)} can attach VMs to.`
              : "Open vSwitch bridges and the networks VMs attach to."}
          </p>
        </div>
        {canAdd && (
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

      {hostView === null ? (
        <LoadingCard />
      ) : (
        status && <StatusPanel status={status} onInstall={() => setInstalling(true)} />
      )}

      <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-hidden">
        <table className="w-full text-sm">
          <thead className="bg-gray-50 text-gray-600 text-left">
            <tr>
              <th className="px-4 py-3 font-medium">Network</th>
              <th className="px-4 py-3 font-medium">{hostView === false ? "Scope" : "Project"}</th>
              {clustered && <th className="px-4 py-3 font-medium">Spans</th>}
              <th className="px-4 py-3 font-medium">Mode</th>
              <th className="px-4 py-3 font-medium">Bridge</th>
              <th className="px-4 py-3 font-medium">VM port</th>
              <th className="px-4 py-3 font-medium">VLAN</th>
              <th className="px-4 py-3 font-medium">Status</th>
              <th className="px-4 py-3" />
            </tr>
          </thead>
          <tbody className="divide-y divide-gray-100">
            {shown.length === 0 && (
              <tr>
                <td className="px-4 py-6 text-center text-gray-500" colSpan={clustered ? 9 : 8}>
                  {hostView === false ? "No networks are available to this project." : "No networks yet."}
                </td>
              </tr>
            )}
            {shown.map((n) => (
              <tr key={n.name}>
                <td className="px-4 py-3 font-mono">{n.name}</td>
                <td className="px-4 py-3">
                  {hostView === false ? (
                    <span className="text-gray-600">{scope(n)}</span>
                  ) : n.project ? (
                    projectName(n.project)
                  ) : (
                    <span className="text-gray-500">host</span>
                  )}
                </td>
                {clustered && (
                  <td className="px-4 py-3 text-xs">
                    {n.scope === "cluster" ? <span className="text-gray-700">every node</span> : <NodeName id={n.node} />}
                  </td>
                )}
                <td className="px-4 py-3">{n.mode === "bridged" && n.physnet ? `provider (${n.physnet})` : n.mode}</td>
                <td className="px-4 py-3 font-mono">{n.scope === "cluster" ? <span className="text-gray-400">OVN</span> : n.bridge}</td>
                <td className="px-4 py-3">{n.port_type === "vhost_user" ? "vhost-user" : "tap"}</td>
                <td className="px-4 py-3">{n.vlan ?? "—"}</td>
                <td className="px-4 py-3 text-xs">
                  <NetworkStatus n={n} />
                </td>
                <td className="px-4 py-3 text-right">
                  {canDelete(n) && (
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
        <Modal title="Add Network" onClose={closeForm}>
          <AddNetworkForm
            bridges={bridges}
            scopes={scopes}
            initialScope={initialScope}
            onCancel={closeForm}
            onDone={() => {
              closeForm();
              refresh();
            }}
          />
        </Modal>
      )}
    </div>
  );
}
