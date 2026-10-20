// The live stream (spec/reconciliation.md §12.6): one
// `glidex.watch.v1.WatchService.Watch` server stream (gRPC-Web) for the
// selected project, shared by the whole app. It keeps the latest copy of
// every VM, disk, image and network the user can see, and, in a cluster,
// of its nodes, import plans and status, plus a marker that changes with
// the policies (spec/clustering-ui.md §3.7); it tells subscribers when one
// of a kind changes. Without it (an old control plane, a proxy that
// buffers) pages fall back to polling. Objects arrive in the proto3 JSON
// shape their list operations answer in; the cluster summary is read into
// the same shape.
import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { toJson } from "@bufbuild/protobuf";
import { createClient } from "@connectrpc/connect";
import type { ClusterStatus, Datastore, DiskInfo, ImageInfo, ImportPlanView, Network, NodeRecord, VmResponse } from "./types";
import { clusterStatusOf } from "./types";
import { useSession } from "./session";
import { transport } from "./api";
import { WatchKind, WatchResponseSchema, WatchService, type WatchResponseJson } from "./gen/glidex/watch/v1/watch_pb";

export type LiveKind = "vm" | "disk" | "image" | "network" | "node" | "import" | "cluster" | "policy" | "datastore";

const KINDS: LiveKind[] = ["vm", "disk", "image", "network", "node", "import", "cluster", "policy", "datastore"];
const WATCH_KINDS = [
  WatchKind.VM,
  WatchKind.DISK,
  WatchKind.IMAGE,
  WatchKind.NETWORK,
  WatchKind.NODE,
  WatchKind.IMPORT,
  WatchKind.CLUSTER,
  WatchKind.POLICY,
  WatchKind.DATASTORE,
];

/** Wait before opening the stream again after it ended or failed (what
 * EventSource did by default). */
const RECONNECT_MS = 3000;

type EventType = "added" | "modified" | "deleted" | "synced" | "expired";

/** A WatchResponse as the `/watch` events read: its type, and `{kind, id, object}`. */
function eventOf(r: WatchResponseJson): { type: EventType; kind: LiveKind; id: string; object?: unknown } {
  return {
    type: (r.type ?? "").replace("WATCH_EVENT_TYPE_", "").toLowerCase() as EventType,
    kind: (r.kind ?? "").replace("WATCH_KIND_", "").toLowerCase() as LiveKind,
    id: r.id ?? "",
    object: r.vm ?? r.disk ?? r.image ?? r.network ?? r.node ?? r.import_plan ?? r.cluster ?? r.policy ?? r.datastore,
  };
}

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
  /** Datastores, with each node's view of them. */
  datastores: Map<string, Datastore>;
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
  datastores: new Map(),
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
  datastore: "datastores",
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
    const abort = new AbortController();
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

    const apply = (type: "added" | "modified" | "deleted", kind: LiveKind, id: string, raw: unknown) => {
      if (!KINDS.includes(kind)) return;
      // The summary may come in the older shape (`raft.state: "Leader"`).
      const object = kind === "cluster" && raw !== undefined ? clusterStatusOf(raw) : raw;
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
    const onSynced = () => {
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
    };
    // The server ends a stream after a while (`expired`), or it fails: open
    // it again; the next snapshot replaces the maps.
    const restart = () => {
      synced = false;
      next = empty();
      setLive(false);
    };

    const client = createClient(WatchService, transport);
    const pause = (ms: number) =>
      new Promise<void>((resolve) => {
        const t = window.setTimeout(resolve, ms);
        abort.signal.addEventListener("abort", () => (window.clearTimeout(t), resolve()), { once: true });
      });
    void (async () => {
      while (!abort.signal.aborted) {
        try {
          const stream = client.watch({ kinds: WATCH_KINDS, project: project || undefined }, { signal: abort.signal });
          for await (const msg of stream) {
            const e = eventOf(toJson(WatchResponseSchema, msg) as WatchResponseJson);
            if (e.type === "synced") onSynced();
            else if (e.type === "expired") restart();
            else if (e.type === "added" || e.type === "modified" || e.type === "deleted") apply(e.type, e.kind, e.id, e.object);
          }
        } catch {
          /* closed, refused or cut off: open it again below */
        }
        if (abort.signal.aborted) break;
        restart();
        await pause(RECONNECT_MS);
      }
    })();
    return () => {
      abort.abort();
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
