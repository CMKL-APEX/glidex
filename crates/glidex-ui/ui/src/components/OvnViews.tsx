import { useEffect, useState, type ReactNode } from "react";
import { Link } from "react-router-dom";
import * as api from "../api";
import type { ChassisRef, NetworkOvn, NodeTunnels, OvnOverview } from "../types";
import { num, tsSecs, tunnelStateLabel } from "../types";
import { Badge, Card, errorMessage } from "./ui";
import Modal from "./Modal";

/** A chassis as its node's name, linked. */
function Chassis({ c }: { c: Pick<ChassisRef, "chassis" | "node" | "node_name"> | null | undefined }) {
  if (!c) return <span className="text-gray-400">none</span>;
  if (!c.node) return <span className="font-mono text-xs">{c.chassis}</span>;
  return (
    <Link to={`/hosts/${encodeURIComponent(c.node)}`} className="text-sky-700 hover:underline">
      {c.node_name ?? c.node.slice(0, 8)}
    </Link>
  );
}

/** Time since an `nb_cfg_at` (RFC 3339), as "12 s ago". */
function since(at: string | undefined): string {
  const secs = tsSecs(at);
  if (!secs) return "never";
  const s = Math.max(0, Math.round(Date.now() / 1000 - secs));
  return s < 120 ? `${s} s ago` : s < 7200 ? `${Math.round(s / 60)} min ago` : `${Math.round(s / 3600)} h ago`;
}

/** Load `GET /cluster/ovn` once (it probes every database server). */
export function useOvnOverview(enabled: boolean, tick = 0): { ovn: OvnOverview | null; error: string | null } {
  const [ovn, setOvn] = useState<OvnOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!enabled) return;
    let live = true;
    api
      .clusterOvn()
      .then((o) => {
        if (!live) return;
        setOvn(o);
        setError(null);
      })
      .catch((e) => live && setError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [enabled, tick]);
  return { ovn, error };
}

/** The OVN databases, chassis and gateways (spec/clustering-ui.md §8). */
export function OvnCard({ ovn, error, actions, nodes = [], children }: { ovn: OvnOverview | null; error: string | null; actions?: ReactNode; nodes?: { id: string; name: string }[]; children?: ReactNode }) {
  // The card (and its settings button) stays whatever OVN says right now.
  if (error) {
    return (
      <Card title="OVN" actions={actions}>
        <p className="text-sm text-red-700">OVN's state couldn't be read: {error}</p>
        {children}
      </Card>
    );
  }
  if (!ovn || !ovn.enabled) {
    return (
      <Card title="OVN" actions={actions}>
        <p className="text-sm text-gray-500">{ovn ? "OVN is on; nodes are starting it." : "Reading OVN's state…"}</p>
      </Card>
    );
  }
  // Nodes not yet a chassis: OVN was just turned on, or one is in trouble.
  const joining = nodes.filter((n) => !(ovn.chassis ?? []).some((c) => c.node === n.id));
  // Only meaningful once something advanced nb_cfg (a client waiting for the hypervisors).
  const timed = (ovn.chassis ?? []).some((c) => c.nb_cfg_at);
  const db = (name: string, ms: OvnOverview["nb"]) => (
    <p className="text-sm">
      <span className="text-gray-500">{name}:</span>{" "}
      {(ms ?? []).map((m) => (
        <span key={m.address} className="mr-3">
          <span className="font-mono text-xs">{m.address.replace(/^ssl:/, "")}</span>{" "}
          {m.leader ? <Badge kind="ok">leader</Badge> : m.reachable ? <Badge kind="muted">follower</Badge> : <Badge kind="denied">down</Badge>}
        </span>
      ))}
    </p>
  );
  return (
    <Card title="OVN" actions={actions}>
      {db("Northbound", ovn.nb)}
      {db("Southbound", ovn.sb)}
      {joining.length > 0 && (
        <p className="text-sm text-amber-700 mt-2">Not a chassis yet: {joining.map((n) => n.name).join(", ")}. Nodes follow the setting within a minute.</p>
      )}
      <table className="w-full text-sm mt-3">
        <thead>
          <tr className="text-left text-gray-500">
            <th className="py-1">Chassis</th>
            <th>Tunnel endpoint</th>
            {timed && <th>Caught up</th>}
          </tr>
        </thead>
        <tbody>
          {(ovn.chassis ?? []).map((c) => (
            <tr key={c.chassis} className="border-t border-gray-100">
              <td className="py-1.5">
                <Chassis c={c} />
              </td>
              <td className="font-mono text-xs">{(c.encaps ?? []).join(", ") || "—"}</td>
              {timed && <td>{since(c.nb_cfg_at)}</td>}
            </tr>
          ))}
        </tbody>
      </table>
      {(ovn.gateways ?? []).length > 0 && (
        <div className="mt-3 text-sm space-y-1">
          {ovn.gateways!.map((g) => (
            <p key={g.router}>
              <span className="font-mono text-xs">{g.router}</span>: active on <Chassis c={g.active} />; candidates{" "}
              {(g.candidates ?? []).map((c, i) => (
                <span key={c.chassis}>
                  {i > 0 && ", "}
                  <Chassis c={c} /> ({num(c.priority)})
                </span>
              ))}
            </p>
          ))}
        </div>
      )}
      {children}
    </Card>
  );
}

/** One node's part: its chassis and the gateways it carries. */
export function NodeOvnCard({ node }: { node: string }) {
  const { ovn } = useOvnOverview(true);
  const tunnels = useTunnels(!!ovn?.enabled);
  if (!ovn?.enabled) return null;
  const mine = (ovn.chassis ?? []).find((c) => c.node === node);
  const carries = (ovn.gateways ?? []).filter((g) => g.active?.node === node);
  return (
    <Card title="OVN">
      {mine ? (
        <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-sm">
          <dt className="text-gray-500">Chassis</dt>
          <dd className="font-mono text-xs">{mine.chassis}</dd>
          <dt className="text-gray-500">Tunnel endpoint</dt>
          <dd className="font-mono text-xs">{(mine.encaps ?? []).join(", ")}</dd>
          {mine.nb_cfg_at && (
            <>
              <dt className="text-gray-500">ovn-controller caught up</dt>
              <dd>{since(mine.nb_cfg_at)}</dd>
            </>
          )}
          <dt className="text-gray-500">Gateways carried</dt>
          <dd>{carries.map((g) => g.router).join(", ") || "none"}</dd>
        </dl>
      ) : (
        <p className="text-sm text-amber-700">This node is not a chassis in the southbound database: VMs here can't use cluster networks.</p>
      )}
      <NodeTunnelTable entry={tunnels.nodes?.find((t) => t.node === node)} />
    </Card>
  );
}

/** A cluster network as OVN has it: its ports, where each is bound, and its router. */
export function NetworkOvnDialog({ id, name, onClose }: { id: string; name: string; onClose: () => void }) {
  const [v, setV] = useState<NetworkOvn | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api
      .networkOvn(id)
      .then(setV)
      .catch((e) => setError(errorMessage(e)));
  }, [id]);
  return (
    <Modal title={`${name} on OVN`} onClose={onClose}>
      {error && <p className="text-sm text-red-600">{error}</p>}
      {!v && !error && <p className="text-sm text-gray-500">Reading OVN…</p>}
      {v && (
        <div className="space-y-4 text-sm">
          <p>
            Logical switch <span className="font-mono">{v.switch}</span>
          </p>
          <table className="w-full">
            <thead>
              <tr className="text-left text-gray-500">
                <th className="py-1">Port</th>
                <th>VM</th>
                <th>Address</th>
                <th>Bound to</th>
              </tr>
            </thead>
            <tbody>
              {(v.ports ?? []).map((p) => (
                <tr key={p.name} className="border-t border-gray-100">
                  <td className="py-1 font-mono text-xs">
                    {p.name}
                    {p.kind && <span className="text-gray-400"> ({p.kind})</span>}
                  </td>
                  <td>{p.vm ? <Link to={`/vms/${p.vm.id}`} className="text-sky-700 hover:underline">{p.vm.name}</Link> : "—"}</td>
                  <td className="font-mono text-xs">{(p.addresses ?? []).join(" ")}</td>
                  <td>
                    {p.bound ? <Chassis c={p.bound} /> : p.kind ? "—" : <span className="text-amber-700">not bound</span>}
                    {p.up === false && <span className="text-amber-700"> (down)</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {v.router && (
            <div>
              <p>
                Router <span className="font-mono">{v.router.name}</span>, gateway active on <Chassis c={v.router.active_gateway} />
              </p>
              {(v.router.nat ?? []).length > 0 && (
                <ul className="mt-1 font-mono text-xs text-gray-600">
                  {(v.router.nat ?? []).map((n) => (
                    <li key={n}>{n}</li>
                  ))}
                </ul>
              )}
            </div>
          )}
        </div>
      )}
    </Modal>
  );
}

/** Each node's tunnels, gathered from the nodes (spec/clustering-ui.md §8b). */
export function useTunnels(enabled: boolean, tick = 0): { nodes: NodeTunnels[] | null; error: string | null } {
  const [nodes, setNodes] = useState<NodeTunnels[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!enabled) return;
    let live = true;
    api
      .clusterTunnels()
      .then((t) => {
        if (!live) return;
        setNodes(t.nodes ?? []);
        setError(null);
      })
      .catch((e) => live && setError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [enabled, tick]);
  return { nodes, error };
}

/** A tunnel's state for people. */
function TunnelBadge({ state, title }: { state: "present" | "error" | "bfd_down" | "missing"; title?: string }) {
  const [kind, label] =
    state === "present" ? ["ok", "up"] : state === "missing" ? ["denied", "missing"] : state === "bfd_down" ? ["denied", "BFD down"] : ["denied", "error"];
  return (
    <span title={title}>
      <Badge kind={kind}>{label}</Badge>
    </span>
  );
}

/** Rows: a node; columns: its tunnel to each other node. "up" means OVS has the
 * tunnel and reports nothing wrong (BFD up where OVN runs it). */
export function TunnelMatrix({ nodes, error, onRefresh }: { nodes: NodeTunnels[] | null; error: string | null; onRefresh: () => void }) {
  if (error) return <p className="text-sm text-gray-500 mt-3">Tunnel status unavailable: {error}</p>;
  if (!nodes) return <p className="text-sm text-gray-500 mt-3">Asking the nodes about their tunnels…</p>;
  if (nodes.length < 2) return null;
  const cell = (from: NodeTunnels, to: NodeTunnels) => {
    if (from.node === to.node) return <span className="text-gray-300">—</span>;
    if (!from.answered) return <span className="text-gray-400">?</span>;
    const t = from.tunnels?.find((x) => x.peer?.node === to.node);
    if (!t) return <TunnelBadge state="missing" title={`${from.node_name ?? from.node} has no tunnel to ${to.node_name ?? to.node}`} />;
    const why = t.error ?? (t.bfd_diagnostic ? `BFD: ${t.bfd_diagnostic}` : `${t.remote_ip ?? ""}: rx ${num(t.rx_packets)} / tx ${num(t.tx_packets)} packets`);
    return <TunnelBadge state={tunnelStateLabel(t.state)} title={why} />;
  };
  return (
    <div className="mt-4">
      <div className="flex items-center justify-between">
        <p className="text-sm font-medium text-gray-700">Geneve tunnels (row → column)</p>
        <button className="text-xs text-sky-700 hover:underline" onClick={onRefresh}>
          Check again
        </button>
      </div>
      <table className="text-sm mt-1">
        <thead>
          <tr className="text-left text-gray-500">
            <th className="py-1 pr-4" />
            {nodes.map((n) => (
              <th key={n.node} className="pr-4 font-normal">
                {n.node_name ?? n.node.slice(0, 8)}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {nodes.map((from) => (
            <tr key={from.node} className="border-t border-gray-100">
              <td className="py-1.5 pr-4">
                {from.node_name ?? from.node.slice(0, 8)}
                {!from.answered && <span className="text-xs text-amber-700"> (no answer: {from.error})</span>}
              </td>
              {nodes.map((to) => (
                <td key={to.node} className="pr-4">
                  {cell(from, to)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      <p className="text-xs text-gray-500 mt-1">
        Counters only rise when VMs on the two nodes talk, so an idle tunnel at 0 packets is fine. Hover a cell for its counters.
      </p>
    </div>
  );
}

/** One node's tunnels with their counters. */
export function NodeTunnelTable({ entry }: { entry: NodeTunnels | undefined }) {
  if (!entry) return null;
  if (!entry.answered) return <p className="text-sm text-amber-700 mt-3">The node didn't answer about its tunnels: {entry.error}</p>;
  return (
    <table className="w-full text-sm mt-3">
      <thead>
        <tr className="text-left text-gray-500">
          <th className="py-1">To</th>
          <th>Endpoint</th>
          <th>State</th>
          <th>Received</th>
          <th>Sent</th>
        </tr>
      </thead>
      <tbody>
        {(entry.tunnels ?? []).map((t) => (
          <tr key={t.name} className="border-t border-gray-100">
            <td className="py-1.5">{t.peer ? <Chassis c={{ chassis: "", node: t.peer.node, node_name: t.peer.node_name }} /> : <span className="text-gray-500">unknown peer</span>}</td>
            <td className="font-mono text-xs">{t.remote_ip}</td>
            <td>
              <TunnelBadge state={tunnelStateLabel(t.state)} title={t.error ?? t.bfd_diagnostic} />
            </td>
            <td>
              {num(t.rx_packets)} pkts ({num(t.rx_bytes)} B)
            </td>
            <td>
              {num(t.tx_packets)} pkts ({num(t.tx_bytes)} B)
            </td>
          </tr>
        ))}
        {(entry.missing ?? []).map((m) => (
          <tr key={m.node} className="border-t border-gray-100">
            <td className="py-1.5">{m.node_name ?? m.node.slice(0, 8)}</td>
            <td>—</td>
            <td>
              <TunnelBadge state="missing" />
            </td>
            <td colSpan={2} className="text-xs text-gray-500">
              no tunnel: the peer isn't a chassis yet, or an endpoint is wrong
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
