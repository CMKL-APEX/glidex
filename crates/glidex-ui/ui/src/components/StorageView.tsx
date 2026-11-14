import { useEffect, useState } from "react";
import * as api from "../api";
import type { Datastore, ProjectView, Tenant } from "../types";
import { formatBytes } from "../types";
import { ErrorBanner, errorMessage } from "./ui";

/** The hyperstor cluster a datastore's identity names
 *  (`hyperstor://<cluster>/mgmt/<name>`, spec/24-storage.md S20). */
export function datastoreCluster(d: Datastore): string | null {
  const m = /^hyperstor:\/\/([^/]+)\//.exec(d.hyperstor_identity ?? "");
  return m ? m[1] : null;
}

const n = (v: api.HsVolume["size"]) => Number(v ?? 0);

interface Loaded {
  tenant: api.HsTenant | null;
  volumes: api.HsVolume[];
  buckets: api.HsBucket[];
  keys: api.HsKey[];
  /** Pools of this cluster known here (from glidex's datastores). */
  pools: number[];
}

/** Per project: what it holds on the cluster. */
interface Usage {
  key: string;
  label: string;
  volumes: number;
  provisioned: number;
  buckets: number;
  bucketBytes: number;
  keys: number;
}

/** A tenant's storage on a trusted hyperstor cluster (spec/19-tenancy.md
 *  D86): its volumes, buckets and S3 keys, and usage per glidex project,
 *  read through this server's storage relay as the signed-in person.
 *  Read-only; the cluster's own UI administers it. */
export default function StorageView({ tenant, cluster }: { tenant: Tenant; cluster: api.TenantCluster }) {
  const [data, setData] = useState<Loaded | null>(null);
  const [projects, setProjects] = useState<ProjectView[]>([]);
  const [error, setError] = useState<string | null>(null);
  const id = cluster.cluster_id ?? "";

  useEffect(() => {
    let live = true;
    const call = <T,>(c: api.StorageCall, body: unknown = {}) => api.storageCall<T>(tenant.id, id, c, body);
    (async () => {
      try {
        // Volumes are listed per pool: the pools of glidex's datastores on
        // this cluster. Glidex's own projects name the rows.
        const [tenants, buckets, keys, stores, mine] = await Promise.all([
          call<{ tenants?: api.HsTenant[] }>("hyperstor.v1.TenantService/ListTenants"),
          call<{ buckets?: api.HsBucket[] }>("hyperstor.v1.S3Service/ListS3Buckets"),
          call<{ keys?: api.HsKey[] }>("hyperstor.v1.S3Service/ListS3Keys"),
          api.listDatastores().catch(() => [] as Datastore[]),
          api.listProjects().catch(() => [] as ProjectView[]),
        ]);
        const pools = [...new Set(stores.filter((d) => datastoreCluster(d) === id && d.hyperstor_pool).map((d) => Number(d.hyperstor_pool)))].sort(
          (a, b) => a - b,
        );
        const lists = await Promise.all(pools.map((pool) => call<{ volumes?: api.HsVolume[] }>("hyperstor.v1.ClusterService/ListVolumes", { pool })));
        const mineThere = (tenants.tenants ?? []).find((t) => t.glidex_id === tenant.id) ?? null;
        const ours = <T extends { tenant?: number }>(xs: T[]) => (mineThere ? xs.filter((x) => (x.tenant ?? 0) === mineThere.id) : xs);
        if (!live) return;
        setProjects(mine);
        setData({
          tenant: mineThere,
          volumes: ours(lists.flatMap((l) => l.volumes ?? [])),
          buckets: ours(buckets.buckets ?? []),
          keys: ours(keys.keys ?? []),
          pools,
        });
      } catch (e) {
        if (live) setError(errorMessage(e));
      }
    })();
    return () => {
      live = false;
    };
  }, [tenant.id, id]);

  if (error) return <ErrorBanner error={error} onDismiss={() => setError(null)} />;
  if (!data) return <p className="text-sm text-gray-500">Loading storage…</p>;

  // Rows by glidex project: the cluster's project linked to it, named by
  // glidex's name when this person sees it here, else the cluster's.
  const byId = new Map((data.tenant?.projects ?? []).map((p) => [p.id ?? 0, p]));
  const glidexName = new Map(projects.map((p) => [p.id, p.name]));
  const rowOf = (project: number | undefined, fallback?: string) => {
    // 0: the tenant's default project (D300).
    const p = byId.get(project || data.tenant?.default_project || 0);
    const key = p?.glidex_id || `hs:${p?.id ?? project ?? 0}`;
    const label = (p?.glidex_id && glidexName.get(p.glidex_id)) || p?.name || fallback || "—";
    return { key, label };
  };
  const usage = new Map<string, Usage>();
  const at = (project: number | undefined, fallback?: string) => {
    const { key, label } = rowOf(project, fallback);
    let u = usage.get(key);
    if (!u) {
      u = { key, label, volumes: 0, provisioned: 0, buckets: 0, bucketBytes: 0, keys: 0 };
      usage.set(key, u);
    }
    return u;
  };
  for (const v of data.volumes) {
    const u = at(v.project, v.project_name);
    u.volumes++;
    u.provisioned += n(v.size);
  }
  for (const b of data.buckets) {
    const u = at(b.project, b.project_name);
    u.buckets++;
    u.bucketBytes += n(b.bytes);
  }
  for (const k of data.keys) at(k.project).keys++;
  const rows = [...usage.values()].sort((a, b) => a.label.localeCompare(b.label));

  return (
    <div className="mt-4 space-y-4" data-testid="storage-view">
      <section>
        <h3 className="text-sm font-semibold text-gray-700 mb-1">Usage per project</h3>
        {rows.length === 0 ? (
          <p className="text-sm text-gray-500">Nothing of this tenant's on {cluster.name || id} that you can see.</p>
        ) : (
          <table className="w-full text-sm" data-testid="storage-usage">
            <thead>
              <tr className="text-left text-xs text-gray-500">
                <th className="py-1 pr-3 font-medium">Project</th>
                <th className="py-1 pr-3 font-medium">Volumes</th>
                <th className="py-1 pr-3 font-medium">Provisioned</th>
                <th className="py-1 pr-3 font-medium">Buckets</th>
                <th className="py-1 pr-3 font-medium">Bucket data</th>
                <th className="py-1 font-medium">S3 keys</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {rows.map((u) => (
                <tr key={u.key}>
                  <td className="py-1.5 pr-3">{u.label}</td>
                  <td className="py-1.5 pr-3">{u.volumes}</td>
                  <td className="py-1.5 pr-3">{formatBytes(u.provisioned)}</td>
                  <td className="py-1.5 pr-3">{u.buckets}</td>
                  <td className="py-1.5 pr-3">{formatBytes(u.bucketBytes)}</td>
                  <td className="py-1.5">{u.keys}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
        <p className="mt-1 text-xs text-gray-500">The space volumes actually use isn't shown yet: the cluster doesn't count it per project.</p>
      </section>
      <section>
        <h3 className="text-sm font-semibold text-gray-700 mb-1">Volumes</h3>
        {data.pools.length === 0 ? (
          <p className="text-sm text-gray-500">No datastore here uses a pool of this cluster, so its volumes aren't listed.</p>
        ) : data.volumes.length === 0 ? (
          <p className="text-sm text-gray-500">No volumes.</p>
        ) : (
          <table className="w-full text-sm" data-testid="storage-volumes">
            <thead>
              <tr className="text-left text-xs text-gray-500">
                <th className="py-1 pr-3 font-medium">Name</th>
                <th className="py-1 pr-3 font-medium">Project</th>
                <th className="py-1 pr-3 font-medium">Pool</th>
                <th className="py-1 font-medium">Size</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {data.volumes.map((v) => (
                <tr key={v.id}>
                  <td className="py-1.5 pr-3">{v.name || <span className="font-mono text-xs">{v.id}</span>}</td>
                  <td className="py-1.5 pr-3">{rowOf(v.project, v.project_name).label}</td>
                  <td className="py-1.5 pr-3">{v.pool}</td>
                  <td className="py-1.5">{formatBytes(n(v.size))}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      <section>
        <h3 className="text-sm font-semibold text-gray-700 mb-1">Buckets</h3>
        {data.buckets.length === 0 ? (
          <p className="text-sm text-gray-500">No buckets.</p>
        ) : (
          <table className="w-full text-sm" data-testid="storage-buckets">
            <thead>
              <tr className="text-left text-xs text-gray-500">
                <th className="py-1 pr-3 font-medium">Name</th>
                <th className="py-1 pr-3 font-medium">Project</th>
                <th className="py-1 pr-3 font-medium">Objects</th>
                <th className="py-1 font-medium">Size</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {data.buckets.map((b) => (
                <tr key={b.name}>
                  <td className="py-1.5 pr-3 font-mono text-xs">{b.name}</td>
                  <td className="py-1.5 pr-3">{rowOf(b.project, b.project_name).label}</td>
                  <td className="py-1.5 pr-3">{b.counted === false ? "—" : String(b.objects ?? 0)}</td>
                  <td className="py-1.5">{b.counted === false ? "—" : formatBytes(n(b.bytes))}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
      <section>
        <h3 className="text-sm font-semibold text-gray-700 mb-1">S3 keys</h3>
        {data.keys.length === 0 ? (
          <p className="text-sm text-gray-500">No S3 keys.</p>
        ) : (
          <table className="w-full text-sm" data-testid="storage-keys">
            <thead>
              <tr className="text-left text-xs text-gray-500">
                <th className="py-1 pr-3 font-medium">Access key</th>
                <th className="py-1 pr-3 font-medium">Description</th>
                <th className="py-1 font-medium">New buckets go to</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {data.keys.map((k) => (
                <tr key={k.id}>
                  <td className="py-1.5 pr-3 font-mono text-xs">{k.id}</td>
                  <td className="py-1.5 pr-3">{k.description || "—"}</td>
                  <td className="py-1.5">{rowOf(k.project).label}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </div>
  );
}
