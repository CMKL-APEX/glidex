import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "../api";
import type { OvnEdge, OvnSettings, OvnSettingsView } from "../types";
import { formatTime } from "../types";
import { useSession } from "../session";
import ConfirmDialog from "./ConfirmDialog";
import Modal from "./Modal";
import { Badge, Card, ErrorBanner, errorMessage, inputClass, primaryButton, secondaryButton } from "./ui";

const emptyEdge: OvnEdge = { physnet: "", external_cidr: "", gateway: "", external_ip: "", external_pool: "", gateway_nodes: [] };

/** OVN while it is off (spec/clustering-ui.md §8a): which nodes are ready
 * for it, and the button that turns it on for the whole cluster. */
export function OvnOff({ view, onDone }: { view: OvnSettingsView; onDone: () => void }) {
  const { host } = useSession();
  const [open, setOpen] = useState(false);
  const nodes = view.nodes ?? [];
  const missing = nodes.filter((n) => n.missing);
  return (
    <Card
      title="OVN"
      actions={
        host.configureOvn && (
          <button className={primaryButton} onClick={() => setOpen(true)} disabled={missing.length > 0} title={missing.length ? "Every node needs OVN installed first" : ""}>
            Enable OVN…
          </button>
        )
      }
    >
      <p className="text-sm text-gray-600">
        OVN is off: networks exist on one node each. With OVN on, cluster networks span every node, VMs on different nodes share them over Geneve
        tunnels, and NAT networks leave through a shared edge.
      </p>
      <table className="w-full text-sm mt-3">
        <thead>
          <tr className="text-left text-gray-500">
            <th className="py-1">Node</th>
            <th>OVN</th>
          </tr>
        </thead>
        <tbody>
          {nodes.map((n) => (
            <tr key={n.id} className="border-t border-gray-100">
              <td className="py-1.5">
                {n.name} <span className="text-gray-400">({n.role})</span>
              </td>
              <td>{n.missing ? <span className="text-amber-700">{n.missing}</span> : <Badge kind="ok">{n.ovn_version} ready</Badge>}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {missing.length > 0 && (
        <p className="mt-2 text-xs text-gray-600">
          Install it on {missing.map((n) => n.name).join(", ")} with <code>sudo glidex-install --ovn</code>; nodes report it within two minutes.
        </p>
      )}
      {open && (
        <ConfirmDialog
          title="Enable OVN for the cluster"
          consequence={
            <>
              Every node follows within a minute, with no restart: servers start OVN's databases, every node becomes a chassis. Existing node
              networks and VMs are not touched. NAT cluster networks also need the shared edge; you can set it now or later in OVN settings.
            </>
          }
          typeToConfirm={undefined}
          actionLabel="Enable OVN"
          danger={false}
          onConfirm={async () => {
            await api.putOvnSettings({ ...(view.settings ?? {}), enabled: true });
            onDone();
          }}
          onClose={() => setOpen(false)}
        />
      )}
    </Card>
  );
}

/** The cluster's OVN options, and turning it off (§8a). */
export function OvnSettingsDialog({ view, onClose, onDone }: { view: OvnSettingsView; onClose: () => void; onDone: () => void }) {
  const s = view.settings ?? { enabled: true };
  const [edgeOn, setEdgeOn] = useState(!!s.edge);
  const [edge, setEdge] = useState<OvnEdge>(s.edge ?? emptyEdge);
  const edgeRef = useRef(edge);
  edgeRef.current = edge;
  const [dns, setDns] = useState((s.dns_servers ?? []).join(", "));
  const [supernet, setSupernet] = useState(s.nat_supernet ?? "");
  const [mtu, setMtu] = useState(s.underlay_mtu ? String(s.underlay_mtu) : "");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [disabling, setDisabling] = useState(false);
  const [notes, setNotes] = useState<string[]>([]);
  const nodes = view.nodes ?? [];

  // Suggested values for the cluster: fill what is empty (or everything, on request).
  const suggest = useCallback(async (overwrite: boolean) => {
    try {
      const d = await api.getEdgeDefaults();
      const filled: string[] = [];
      const cur = edgeRef.current;
      const pick = <K extends keyof OvnEdge>(k: K): OvnEdge[K] => {
        const mine = cur[k];
        const empty = mine === undefined || mine === null || mine === "" || (Array.isArray(mine) && mine.length === 0);
        if (!(overwrite || empty)) return mine;
        const v = d.edge[k];
        if (typeof v === "string" && v) filled.push(v);
        return v;
      };
      setEdge({
        physnet: pick("physnet"),
        external_cidr: pick("external_cidr"),
        gateway: pick("gateway"),
        external_ip: pick("external_ip"),
        external_pool: pick("external_pool") ?? "",
        gateway_nodes: pick("gateway_nodes"),
      });
      // Only what concerns a value filled in now (an existing edge keeps its own).
      setNotes(overwrite ? d.notes : d.notes.filter((n) => filled.some((v) => n.includes(v))));
    } catch (e) {
      setNotes([`No suggestions: ${errorMessage(e)}`]);
    }
  }, []);
  // A new edge starts from the suggestions; an existing one gets its empty
  // fields filled (the VPC router pool, say).
  useEffect(() => {
    if (edgeOn) suggest(false);
  }, [edgeOn, suggest]);
  const networks = view.cluster_networks ?? [];

  const body = (enabled: boolean): Omit<OvnSettings, "updated_by" | "updated_at"> => ({
    enabled,
    underlay_mtu: mtu ? Number(mtu) : null,
    nat_supernet: supernet.trim() || null,
    snat_ct_zones: s.snat_ct_zones ?? null,
    dns_servers: dns
      .split(/[\s,]+/)
      .map((x) => x.trim())
      .filter(Boolean),
    edge: edgeOn ? { ...edge, external_pool: edge.external_pool?.trim() || null } : null,
  });

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.putOvnSettings(body(true));
      onDone();
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const field = (label: string, value: string, set: (v: string) => void, placeholder = "") => (
    <div>
      <label className="block text-xs text-gray-500">{label}</label>
      <input aria-label={label} className={inputClass} value={value} placeholder={placeholder} onChange={(e) => set(e.target.value)} />
    </div>
  );

  return (
    <Modal title="OVN settings" onClose={onClose}>
      <div className="space-y-4 text-sm">
        <p className="text-gray-600">
          For the whole cluster{s.updated_by ? `; last changed by ${s.updated_by}${s.updated_at ? ` ${formatTime(s.updated_at)}` : ""}` : ""}.
          {view.source === "file" && " Now taken from a server's control-plane.json; saving here makes it the cluster's setting."}
        </p>
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        <div className="grid grid-cols-3 gap-3">
          {field("DNS servers for guests", dns, setDns, "the hosts' resolvers")}
          {field("NAT supernet", supernet, setSupernet, "10.89.0.0/16")}
          {field("Underlay MTU", mtu, setMtu, "1500")}
        </div>
        <label className="flex items-center gap-2 font-medium">
          <input type="checkbox" checked={edgeOn} onChange={(e) => setEdgeOn(e.target.checked)} />
          Shared edge for NAT cluster networks
        </label>
        {edgeOn && (
          <div className="flex items-center justify-between text-xs">
            <span className="text-gray-500">Prefilled from what the nodes report; edit anything.</span>
            <button className="text-sky-700 hover:underline" onClick={() => suggest(true)}>
              Suggest values
            </button>
          </div>
        )}
        {edgeOn && notes.length > 0 && (
          <ul className="text-xs text-amber-800 bg-amber-50 border border-amber-200 rounded-lg p-2 list-disc pl-5 space-y-1">
            {notes.map((n) => (
              <li key={n}>{n}</li>
            ))}
          </ul>
        )}
        {edgeOn && (
          <div className="grid grid-cols-2 gap-3">
            {field("Physical network", edge.physnet, (v) => setEdge({ ...edge, physnet: v }), "uplink")}
            {field("External network", edge.external_cidr, (v) => setEdge({ ...edge, external_cidr: v }), "192.0.2.0/24")}
            {field("Gateway", edge.gateway, (v) => setEdge({ ...edge, gateway: v }), "192.0.2.1")}
            {field("Edge address", edge.external_ip, (v) => setEdge({ ...edge, external_ip: v }), "192.0.2.50")}
            {field("Pool for VPC routers (optional)", edge.external_pool ?? "", (v) => setEdge({ ...edge, external_pool: v }), "192.0.2.64/26")}
            <div>
              <label className="block text-xs text-gray-500">Gateway nodes, by priority</label>
              <div className="flex flex-wrap gap-2 mt-1">
                {nodes.map((n) => {
                  const on = edge.gateway_nodes.includes(n.name) || edge.gateway_nodes.includes(n.id);
                  return (
                    <label key={n.id} className="flex items-center gap-1">
                      <input
                        type="checkbox"
                        checked={on}
                        onChange={(e) =>
                          setEdge({
                            ...edge,
                            gateway_nodes: e.target.checked ? [...edge.gateway_nodes, n.name] : edge.gateway_nodes.filter((g) => g !== n.name && g !== n.id),
                          })
                        }
                      />
                      {n.name}
                    </label>
                  );
                })}
              </div>
            </div>
            <p className="col-span-2 text-xs text-gray-500">
              The physical network must be mapped on each gateway node (its <code>bridge_mappings</code> in control-plane.json).
            </p>
          </div>
        )}
        <div className="flex items-center justify-between gap-2 pt-2">
          <button
            className="text-sm text-red-600 hover:text-red-800 disabled:opacity-50"
            disabled={networks.length > 0}
            title={networks.length ? `Delete the cluster networks first: ${networks.join(", ")}` : ""}
            onClick={() => setDisabling(true)}
          >
            Disable OVN…
          </button>
          <div className="flex gap-2">
            <button className={secondaryButton} onClick={onClose}>
              Cancel
            </button>
            <button className={primaryButton} onClick={save} disabled={busy}>
              {busy ? "Saving..." : "Save"}
            </button>
          </div>
        </div>
        {networks.length > 0 && <p className="text-xs text-gray-500">OVN can be turned off once no cluster network is left ({networks.join(", ")}).</p>}
      </div>
      {disabling && (
        <ConfirmDialog
          title="Disable OVN"
          consequence="Every node stops being an OVN chassis. Node networks and their VMs are not affected; OVN's database files are kept."
          actionLabel="Disable OVN"
          onConfirm={async () => {
            await api.putOvnSettings(body(false));
            onDone();
            onClose();
          }}
          onClose={() => setDisabling(false)}
        />
      )}
    </Modal>
  );
}
