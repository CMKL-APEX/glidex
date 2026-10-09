// The live stream (spec/reconciliation.md §12.6): one EventSource on
// `GET /watch` for the selected project, shared by the whole app. It keeps
// the latest copy of every VM, disk, image and network the user can see,
// and, in a cluster, of its nodes, import plans and status, plus a marker
// that changes with the policies (spec/clustering-ui.md §3.7); it tells
// subscribers when one of a kind changes. Without it (an old control
// plane, a proxy that buffers) pages fall back to polling.
import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { DiskInfo, ImageInfo, ImportPlanView, Network, NodeRecord, VmResponse } from "./types";
import type { ClusterStatus } from "./api";
import { useSession } from "./session";

export type LiveKind = "vm" | "disk" | "image" | "network" | "node" | "import" | "cluster" | "policy";

const KINDS: LiveKind[] = ["vm", "disk", "image", "network", "node", "import", "cluster", "policy"];
const WATCH_KINDS = "vms,disks,images,networks,nodes,imports,cluster,policy";

export interface LiveState {
  /** The stream delivered its snapshot and is open: the maps are current. */
  live: boolean;
  vms: Map<string, VmResponse>;
  disks: Map<string, DiskInfo>;
  images: Map<string, ImageInfo>;
  networks: Map<string, Network>;
  /** Nodes the caller may list (`listNodes`); empty otherwise. */
  nodes: Map<string, NodeRecord>;
  /** Import plans (`listImports`). */
  imports: Map<string, ImportPlanView>;
  /** The cluster summary (`readCluster`, clustered hosts only): keyed by cluster id. */
  clusters: Map<string, ClusterStatus>;
  /** Changes whenever policies or role links change. */
  policies: Map<string, { generation: number }>;
  /** Call `cb` (debounced) whenever an object of one of `kinds` changes. */
  subscribe: (kinds: LiveKind[], cb: () => void) => () => void;
}

const empty = (): Omit<LiveState, "live" | "subscribe"> => ({
  vms: new Map(),
  disks: new Map(),
  images: new Map(),
  networks: new Map(),
  nodes: new Map(),
  imports: new Map(),
  clusters: new Map(),
  policies: new Map(),
});

/** The map that holds objects of `kind`, by its field name. */
const FIELD: Record<LiveKind, keyof ReturnType<typeof empty>> = {
  vm: "vms",
  disk: "disks",
  image: "images",
  network: "networks",
  node: "nodes",
  import: "imports",
  cluster: "clusters",
  policy: "policies",
};

const LiveContext = createContext<LiveState | null>(null);

export function useLive(): LiveState {
  const s = useContext(LiveContext);
  if (!s) throw new Error("useLive outside LiveProvider");
  return s;
}

/** Re-run `refresh` when an object of `kinds` changes, while live. Returns
 * whether the stream is live (pages poll only when it isn't). */
export function useLiveRefresh(kinds: LiveKind[], refresh: () => void): boolean {
  const { live, subscribe } = useLive();
  const latest = useRef(refresh);
  latest.current = refresh;
  const key = kinds.join(",");
  useEffect(() => subscribe(key.split(",") as LiveKind[], () => latest.current()), [key, subscribe]);
  return live;
}

/** Changes closer together than this go out as one notification. */
const DEBOUNCE_MS = 300;

/** The cluster summary from the stream, if the caller may read it. */
export function useLiveCluster(): ClusterStatus | null {
  const { clusters } = useLive();
  return clusters.values().next().value ?? null;
}

export function LiveProvider({ children }: { children: ReactNode }) {
  const { project, refreshCapabilities } = useSession();
  const refreshCaps = useRef(refreshCapabilities);
  refreshCaps.current = refreshCapabilities;
  /** The policy generation last seen, across reconnects. */
  const policyGen = useRef<number | null>(null);
  const [live, setLive] = useState(false);
  const [objects, setObjects] = useState(empty);
  const subscribers = useRef(new Set<{ kinds: LiveKind[]; cb: () => void; timer?: number }>());

  useEffect(() => {
    if (typeof EventSource === "undefined") return;
    const q = new URLSearchParams({ kinds: WATCH_KINDS });
    if (project) q.set("project", project);
    const source = new EventSource(`/api/watch?${q}`, { withCredentials: true });
    // Built up until `synced`, then applied per event.
    let next = empty();
    let synced = false;
    const changed = new Set<LiveKind>();
    let flush: number | undefined;

    const notify = () => {
      flush = undefined;
      const kinds = [...changed];
      changed.clear();
      for (const s of subscribers.current) {
        if (!s.kinds.some((k) => kinds.includes(k))) continue;
        window.clearTimeout(s.timer);
        s.timer = window.setTimeout(s.cb, DEBOUNCE_MS);
      }
    };
    const mapFor = (o: ReturnType<typeof empty>, kind: LiveKind) => o[FIELD[kind]] as Map<string, unknown>;

    const apply = (type: "added" | "modified" | "deleted") => (e: MessageEvent) => {
      const { kind, id, object } = JSON.parse(e.data) as { kind: LiveKind; id: string; object?: unknown };
      if (!KINDS.includes(kind)) return;
      if (!synced) {
        if (type !== "deleted") mapFor(next, kind).set(id, object);
        return;
      }
      // Policies changed: what the session may do may have too.
      if (kind === "policy") {
        policyGen.current = (object as { generation: number } | undefined)?.generation ?? null;
        refreshCaps.current();
      }
      setObjects((prev) => {
        const copy = { ...prev, [FIELD[kind]]: new Map(mapFor(prev, kind)) } as ReturnType<typeof empty>;
        if (type === "deleted") mapFor(copy, kind).delete(id);
        else mapFor(copy, kind).set(id, object);
        return copy;
      });
      changed.add(kind);
      if (flush === undefined) flush = window.setTimeout(notify, 0);
    };
    source.addEventListener("added", apply("added"));
    source.addEventListener("modified", apply("modified"));
    source.addEventListener("deleted", apply("deleted"));
    source.addEventListener("synced", () => {
      synced = true;
      // A policy change while the stream was down (or on another server).
      const gen = next.policies.values().next().value?.generation ?? null;
      if (policyGen.current !== null && gen !== null && gen !== policyGen.current) refreshCaps.current();
      policyGen.current = gen;
      setObjects(next);
      next = empty();
      setLive(true);
      // Pages may have missed changes while reconnecting.
      for (const k of KINDS) changed.add(k);
      notify();
    });
    // The server ends a stream after a while; EventSource reconnects and
    // the next snapshot replaces the maps.
    const restart = () => {
      synced = false;
      next = empty();
      setLive(false);
    };
    source.addEventListener("expired", restart);
    source.onerror = restart;
    return () => {
      source.close();
      window.clearTimeout(flush);
      setLive(false);
    };
  }, [project]);

  const subscribe = useMemo(
    () => (kinds: LiveKind[], cb: () => void) => {
      const entry = { kinds, cb, timer: undefined as number | undefined };
      subscribers.current.add(entry);
      return () => {
        window.clearTimeout(entry.timer);
        subscribers.current.delete(entry);
      };
    },
    [],
  );

  const value = useMemo(() => ({ live, ...objects, subscribe }), [live, objects, subscribe]);
  return <LiveContext.Provider value={value}>{children}</LiveContext.Provider>;
}
