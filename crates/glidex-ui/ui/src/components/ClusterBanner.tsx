import { useEffect, useState } from "react";
import { CLUSTER_EVENT, clusterAvailability, type ClusterAvailability } from "../api";

/** Shown while the cluster has no leader (spec/clustering-ui.md §3.3): the
 * API client saw `503 cluster_unavailable` and nothing has answered
 * normally since. */
export default function ClusterBanner() {
  const [state, setState] = useState<ClusterAvailability>(clusterAvailability);
  useEffect(() => {
    const on = (e: Event) => setState((e as CustomEvent<ClusterAvailability>).detail);
    window.addEventListener(CLUSTER_EVENT, on);
    return () => window.removeEventListener(CLUSTER_EVENT, on);
  }, []);
  if (state.available) return null;
  return (
    <div role="alert" className="bg-amber-50 border-b border-amber-200 text-amber-900 text-sm px-4 py-2 text-center">
      The cluster has no leader: changes are refused until a majority of servers is back.
      {state.stale && " What you see comes from this server's own copy and may be out of date."}
    </div>
  );
}
