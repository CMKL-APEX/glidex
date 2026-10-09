// Cluster nodes for every page (spec/clustering-ui.md §3.2, §3.5): the node
// directory from the live stream, and per-resource capability queries.
import { useCallback, useEffect, useMemo, useState } from "react";
import * as api from "./api";
import { useLive, useLiveCluster, useLiveRefresh } from "./live";
import { useSession } from "./session";
import type { EntityRef, NodeRecord } from "./types";

export interface NodeDirectory {
  /** Every node the caller may list, by id; empty without `listNodes`. */
  nodes: Map<string, NodeRecord>;
  /** The node serving this UI, when known. */
  self: string | null;
  /** Whether this host is part of a cluster (false while unknown). */
  clustered: boolean;
  /** A node's name; its id's first 8 characters when it isn't listed. */
  name: (id: string | null | undefined) => string;
}

/** How often to look again while the live stream is down. */
const POLL_MS = 10_000;

export function useNodes(): NodeDirectory {
  const { host } = useSession();
  const live = useLive();
  const cluster = useLiveCluster();
  const [polled, setPolled] = useState<Map<string, NodeRecord> | null>(null);

  // Without the stream (an old server, a buffering proxy), poll.
  useEffect(() => {
    if (live.live || !host.listNodes) return;
    let stop = false;
    const load = () =>
      api
        .listNodes()
        .then((ns) => !stop && setPolled(new Map(ns.map((n) => [n.meta.id, n]))))
        .catch(() => {});
    load();
    const t = window.setInterval(load, POLL_MS);
    return () => {
      stop = true;
      window.clearInterval(t);
    };
  }, [live.live, host.listNodes]);

  const nodes = live.live ? live.nodes : (polled ?? new Map<string, NodeRecord>());
  const self = cluster?.node_id ?? null;
  return useMemo(
    () => ({
      nodes,
      self,
      clustered: !!cluster || nodes.size > 1,
      name: (id) => {
        if (!id) return "—";
        return nodes.get(id)?.spec.name ?? (id.length > 8 ? id.slice(0, 8) : id);
      },
    }),
    [nodes, self, cluster],
  );
}

/** A stable key for an entity reference. */
export function entityKey(e: EntityRef): string {
  return "id" in e ? `${e.type}:${e.id}` : e.type;
}

/** The resource a host-specific action on node `id` is checked on: `Host`
 * for the node serving the UI, `Node` for the others (the server's
 * `Ent::host_of`). */
export function hostOf(id: string, self: string | null): EntityRef {
  return id === self ? { type: "Host" } : { type: "Node", id };
}

/** Which of `actions` the caller may take on each of `resources`, in one
 * `POST /authz/allowed` call; re-asked when policies change. `null` while
 * loading; a failed call counts as nothing allowed. */
export function useAllowed(actions: string[], resources: EntityRef[]): ((resource: EntityRef, action: string) => boolean) | null {
  const key = JSON.stringify([actions, resources]);
  const [result, setResult] = useState<{ key: string; allowed: Map<string, Set<string>> } | null>(null);
  const [tick, setTick] = useState(0);
  const again = useCallback(() => setTick((t) => t + 1), []);
  useLiveRefresh(["policy"], again);

  useEffect(() => {
    let live = true;
    const [acts, res] = JSON.parse(key) as [string[], EntityRef[]];
    api
      .allowedActions(acts, res)
      .then((out) => live && setResult({ key, allowed: new Map(res.map((r, i) => [entityKey(r), new Set(out[i] ?? [])])) }))
      .catch(() => live && setResult({ key, allowed: new Map() }));
    return () => {
      live = false;
    };
  }, [key, tick]);

  if (!result || result.key !== key) return null;
  return (resource, action) => result.allowed.get(entityKey(resource))?.has(action) ?? false;
}
