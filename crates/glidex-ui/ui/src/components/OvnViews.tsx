import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import * as api from "../api";
import type { ChassisRef, NetworkOvn, OvnOverview } from "../types";
import { Badge, Card, errorMessage } from "./ui";
import Modal from "./Modal";

/** A chassis as its node's name, linked. */
function Chassis({ c }: { c: ChassisRef | null }) {
  if (!c) return <span className="text-gray-400">none</span>;
  if (!c.node) return <span className="font-mono text-xs">{c.chassis}</span>;
  return (
    <Link to={`/cluster/nodes/${encodeURIComponent(c.node)}`} className="text-sky-700 hover:underline">
      {c.node_name ?? c.node.slice(0, 8)}
    </Link>
  );
}

/** Seconds since an `nb_cfg_timestamp` (ms), as "12 s ago". */
function since(ms: number | null): string {
  if (!ms) return "never";
  const s = Math.max(0, Math.round(Date.now() / 1000 - ms / 1000));
  return s < 120 ? `${s} s ago` : s < 7200 ? `${Math.round(s / 60)} min ago` : `${Math.round(s / 3600)} h ago`;
}

/** Load `GET /cluster/ovn` once (it probes every database server). */
export function useOvnOverview(enabled: boolean): { ovn: OvnOverview | null; error: string | null } {
  const [ovn, setOvn] = useState<OvnOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!enabled) return;
    let live = true;
    api
      .clusterOvn()
      .then((o) => live && setOvn(o))
      .catch((e) => live && setError(errorMessage(e)));
    return () => {
      live = false;
    };
  }, [enabled]);
  return { ovn, error };
}

/** The OVN databases, chassis and gateways (spec/clustering-ui.md §8). */
export function OvnCard({ ovn, error }: { ovn: OvnOverview | null; error: string | null }) {
  if (error) return null;
  if (!ovn || !ovn.enabled) return null;
  // Only meaningful once something advanced nb_cfg (a client waiting for the hypervisors).
  const timed = (ovn.chassis ?? []).some((c) => c.nb_cfg_timestamp);
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
    <Card title="OVN">
      {db("Northbound", ovn.nb)}
      {db("Southbound", ovn.sb)}
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
              <td className="font-mono text-xs">{c.encaps.join(", ") || "—"}</td>
              {timed && <td>{since(c.nb_cfg_timestamp)}</td>}
            </tr>
          ))}
        </tbody>
      </table>
      {(ovn.gateways ?? []).length > 0 && (
        <div className="mt-3 text-sm space-y-1">
          {ovn.gateways!.map((g) => (
            <p key={g.router}>
              <span className="font-mono text-xs">{g.router}</span>: active on <Chassis c={g.active} />; candidates{" "}
              {g.candidates.map((c, i) => (
                <span key={c.chassis}>
                  {i > 0 && ", "}
                  <Chassis c={c} /> ({c.priority})
                </span>
              ))}
            </p>
          ))}
        </div>
      )}
    </Card>
  );
}

/** One node's part: its chassis and the gateways it carries. */
export function NodeOvnCard({ node }: { node: string }) {
  const { ovn } = useOvnOverview(true);
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
          <dd className="font-mono text-xs">{mine.encaps.join(", ")}</dd>
          {mine.nb_cfg_timestamp && (
            <>
              <dt className="text-gray-500">ovn-controller caught up</dt>
              <dd>{since(mine.nb_cfg_timestamp)}</dd>
            </>
          )}
          <dt className="text-gray-500">Gateways carried</dt>
          <dd>{carries.map((g) => g.router).join(", ") || "none"}</dd>
        </dl>
      ) : (
        <p className="text-sm text-amber-700">This node is not a chassis in the southbound database: VMs here can't use cluster networks.</p>
      )}
    </Card>
  );
}

/** A cluster network as OVN has it: its ports, where each is bound, and its router. */
export function NetworkOvnDialog({ name, onClose }: { name: string; onClose: () => void }) {
  const [v, setV] = useState<NetworkOvn | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api
      .networkOvn(name)
      .then(setV)
      .catch((e) => setError(errorMessage(e)));
  }, [name]);
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
              {v.ports.map((p) => (
                <tr key={p.name} className="border-t border-gray-100">
                  <td className="py-1 font-mono text-xs">
                    {p.name}
                    {p.kind && <span className="text-gray-400"> ({p.kind})</span>}
                  </td>
                  <td>{p.vm ? <Link to={`/vms/${p.vm.id}`} className="text-sky-700 hover:underline">{p.vm.name}</Link> : "—"}</td>
                  <td className="font-mono text-xs">{p.addresses.join(" ")}</td>
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
              {v.router.nat.length > 0 && (
                <ul className="mt-1 font-mono text-xs text-gray-600">
                  {v.router.nat.map((n) => (
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
