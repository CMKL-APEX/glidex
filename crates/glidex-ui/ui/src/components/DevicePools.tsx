import { useCallback, useEffect, useState } from "react";
import * as api from "../api";
import type { PciGrantJson, PciPoolJson, PciPoolSpecJson } from "../gen/glidex/cluster/v1/cluster_pb";
import { formatTimestamp, type ProjectView, type Tenant } from "../types";
import { useSession } from "../session";
import ConfirmDialog from "./ConfirmDialog";
import Modal from "./Modal";
import { Badge, Card, ErrorBanner, errorMessage, inputClass, primaryButton, secondaryButton } from "./ui";

/** Who a pool is granted to, from the setting's grants. */
function grantees(allow: PciGrantJson[], pool: string): string {
  const who = allow
    .filter((g) => g.pool === pool)
    .flatMap((g) => [...(g.tenants ?? []).map((t) => (t === "*" ? "every tenant" : `tenant ${t}`)), ...(g.projects ?? []).map((p) => `project ${p}`)]);
  return who.length ? who.join(", ") : "host roles only";
}

/** Devices across the nodes, and how many of them are free. */
function devicesLine(p: PciPoolJson): string {
  const total = (p.nodes ?? []).reduce((n, x) => n + (x.devices ?? []).length, 0);
  return `${p.free ?? 0} of ${total} free`;
}

/** The cluster's device pools and grants (spec/22-host-topology.md H20,
 * H21): defined pools, pools discovered from GPUs no pool covers, and, for
 * system admins, adopting, removing and editing them. */
export default function DevicePools() {
  const { host } = useSession();
  const [settings, setSettings] = useState<api.PciSettingsView | null>(null);
  const [pools, setPools] = useState<PciPoolJson[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [adopting, setAdopting] = useState<string | null>(null);
  const [removing, setRemoving] = useState<string | null>(null);
  const [granting, setGranting] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);

  const load = useCallback(() => {
    Promise.all([api.getPciSettings(), api.listPciPools()])
      .then(([s, p]) => {
        setSettings(s);
        setPools(p);
        setError(null);
      })
      .catch((e) => setError(errorMessage(e)));
  }, []);
  useEffect(load, [load]);

  if (!host.readCluster) return null;
  const s = settings?.settings ?? {};
  const defined: PciPoolSpecJson[] = s.pools ?? [];
  const allow: PciGrantJson[] = s.allow ?? [];
  const spec = (name: string) => defined.find((d) => d.name === name);
  const fromFile = settings?.source === "PCI_SETTINGS_SOURCE_FILE";
  const canWrite = host.configurePciPools;

  /** The setting with `pools` and `allow` replaced; a refusal is shown. */
  const save = async (next: { pools: PciPoolSpecJson[]; allow: PciGrantJson[] }) => {
    await api.putPciSettings(next);
    load();
  };

  return (
    <Card
      title="Device pools"
      actions={
        canWrite && (
          <button className={secondaryButton} onClick={() => setEditing(true)}>
            Edit pools and grants…
          </button>
        )
      }
    >
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <p className="text-sm text-gray-500 mb-2">
        {fromFile
          ? "From this server's control-plane.json until the cluster setting is saved."
          : `The cluster setting${s.updated_by ? `, by ${s.updated_by}` : ""}${s.updated_at ? ` at ${formatTimestamp(s.updated_at)}` : ""}.`}
      </p>
      {pools.length === 0 ? (
        <p className="text-sm text-gray-500">No device pools, and no GPUs a pool could be discovered from.</p>
      ) : (
        <table className="w-full text-sm">
          <thead>
            <tr className="text-left text-gray-500">
              <th className="py-1">Pool</th>
              <th>Class</th>
              <th>Devices</th>
              <th>Per device</th>
              <th>Granted to</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {pools.map((p) => {
              const d = spec(p.name ?? "");
              return (
                <tr key={p.name} className="border-t border-gray-100 align-top">
                  <td className="py-2 font-medium">
                    {p.name} {p.discovered && <Badge kind="muted">discovered</Badge>}
                  </td>
                  <td>{p.class ?? "—"}</td>
                  <td>
                    {devicesLine(p)}
                    <span className="text-gray-400"> · {(p.nodes ?? []).map((n) => n.node_name).join(", ") || "no node"}</span>
                  </td>
                  <td>
                    {p.per_device_vcpus ? `${p.per_device_vcpus} vCPUs, ${p.per_device_memory_mib} MiB` : "—"}
                    {d?.bar_mib ? <span className="text-gray-400"> · window {d.bar_mib} MiB</span> : null}
                  </td>
                  <td>{p.discovered ? "host roles only" : grantees(allow, p.name ?? "")}</td>
                  <td className="text-right whitespace-nowrap">
                    {canWrite && (
                      <button className={`${secondaryButton} mr-2`} onClick={() => setGranting(p.name ?? "")}>
                        Grants…
                      </button>
                    )}
                    {canWrite && p.discovered && (
                      <button className={secondaryButton} onClick={() => setAdopting(p.name ?? "")}>
                        Adopt…
                      </button>
                    )}
                    {canWrite && !p.discovered && d && (
                      <button className={secondaryButton} onClick={() => setRemoving(p.name ?? "")}>
                        Remove…
                      </button>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
      {adopting && (
        <AdoptDialog
          slug={adopting}
          pool={pools.find((p) => p.name === adopting)}
          onClose={() => setAdopting(null)}
          onAdopt={async (spec) => {
            await save({ pools: [...defined, spec], allow });
            setAdopting(null);
          }}
        />
      )}
      {granting && (
        <GrantsDialog
          pool={granting}
          discovered={pools.find((p) => p.name === granting)?.discovered === true}
          allow={allow}
          onClose={() => setGranting(null)}
          onSave={async (grant) => {
            // A grant names a pool of the setting: a discovered pool is
            // adopted under its name in the same write (§9.1b).
            const found = pools.find((p) => p.name === granting);
            const match = found?.discovered ? api.discoveredMatch(granting) : null;
            const nextPools = match && !spec(granting) ? [...defined, { name: granting, class: found?.class, match }] : defined;
            const rest = allow.filter((g) => g.pool !== granting || g.bdf);
            await save({ pools: nextPools, allow: grant ? [...rest, grant] : rest });
            setGranting(null);
          }}
        />
      )}
      {removing && (
        <ConfirmDialog
          title={`Remove pool ${removing}`}
          consequence={
            <>
              Its grants go with it. It's refused while VMs or tenant pools still ask for it. GPUs it covered form a discovered pool again, for
              host roles only.
            </>
          }
          actionLabel="Remove"
          onClose={() => setRemoving(null)}
          onConfirm={async () => {
            await save({ pools: defined.filter((d) => d.name !== removing), allow: allow.filter((g) => g.pool !== removing) });
            setRemoving(null);
          }}
        />
      )}
      {editing && (
        <EditDialog
          pools={defined}
          allow={allow}
          onClose={() => setEditing(false)}
          onSave={async (next) => {
            await save(next);
            setEditing(false);
          }}
        />
      )}
    </Card>
  );
}

/** Who may use one pool: projects and tenants picked from the directory,
 * or every admitted tenant (`"*"`, which goes alone). Saving replaces the
 * pool's grants with one; names the directory doesn't list stay as they
 * were unless unticked. */
function GrantsDialog({
  pool,
  discovered,
  allow,
  onClose,
  onSave,
}: {
  pool: string;
  discovered: boolean;
  allow: PciGrantJson[];
  onClose: () => void;
  onSave: (grant: PciGrantJson | null) => Promise<void>;
}) {
  const current = allow.filter((g) => g.pool === pool && !g.bdf);
  const [projects, setProjects] = useState<Set<string>>(new Set(current.flatMap((g) => g.projects ?? [])));
  const [tenants, setTenants] = useState<Set<string>>(new Set(current.flatMap((g) => g.tenants ?? []).filter((t) => t !== "*")));
  const [everyTenant, setEveryTenant] = useState(current.some((g) => (g.tenants ?? []).includes("*")));
  const [known, setKnown] = useState<{ projects: ProjectView[]; tenants: Tenant[] } | null>(null);
  const [filter, setFilter] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    Promise.all([api.listProjects(), api.listTenants()])
      .then(([p, t]) => setKnown({ projects: p, tenants: t }))
      .catch((e) => setError(errorMessage(e)));
  }, []);

  const toggle = (set: Set<string>, update: (s: Set<string>) => void, name: string) => {
    const next = new Set(set);
    if (next.has(name)) next.delete(name);
    else next.add(name);
    update(next);
  };
  // What the directory lists, by the name a grant keeps (a project's name,
  // a tenant's slug), with granted names it doesn't list kept on the list.
  const projectNames = [...new Set([...(known?.projects ?? []).map((p) => p.name), ...projects])].sort();
  const tenantNames = [...new Set([...(known?.tenants ?? []).map((t) => t.slug), ...tenants])].sort();
  const shown = (n: string) => n.toLowerCase().includes(filter.trim().toLowerCase());

  const go = async () => {
    const p = [...projects].sort();
    const t = everyTenant ? ["*"] : [...tenants].sort();
    setBusy(true);
    try {
      await onSave(p.length || t.length ? { pool, ...(p.length ? { projects: p } : {}), ...(t.length ? { tenants: t } : {}) } : null);
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const list = (label: string, names: string[], set: Set<string>, update: (s: Set<string>) => void, disabled = false) => (
    <fieldset className="mb-3" disabled={disabled}>
      <legend className="text-sm font-medium text-gray-700 mb-1">{label}</legend>
      <div className="max-h-40 overflow-y-auto border border-gray-200 rounded p-2">
        {names.filter(shown).length === 0 ? (
          <p className="text-sm text-gray-500">{known ? "None" : "Loading…"}</p>
        ) : (
          names.filter(shown).map((n) => (
            <label key={n} className="flex items-center gap-2 text-sm text-gray-700">
              <input type="checkbox" checked={set.has(n)} onChange={() => toggle(set, update, n)} />
              {n}
            </label>
          ))
        )}
      </div>
    </fieldset>
  );

  return (
    <Modal title={`Grants for ${pool}`} onClose={onClose}>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <p className="text-sm text-gray-600 mb-3">
        Who may ask for this pool's devices. Quotas (<code>devices.{pool}</code>) and tenant pools still apply.
        {discovered && ` Saving a grant adopts ${pool} into the cluster setting under this name.`}
      </p>
      <input className={`${inputClass} mb-3`} placeholder="Filter" aria-label="Filter" value={filter} onChange={(e) => setFilter(e.target.value)} />
      {list("Projects", projectNames, projects, setProjects)}
      <label className="flex items-start gap-2 text-sm text-gray-700 mb-2">
        <input type="checkbox" className="mt-1" checked={everyTenant} onChange={(e) => setEveryTenant(e.target.checked)} />
        <span>Every tenant admitted on this cluster (an open pool)</span>
      </label>
      {list("Tenants", tenantNames, tenants, setTenants, everyTenant)}
      <div className="flex justify-end gap-2 mt-4">
        <button className={secondaryButton} onClick={onClose}>
          Cancel
        </button>
        <button className={primaryButton} disabled={busy || !known} onClick={go}>
          Save
        </button>
      </div>
    </Modal>
  );
}

/** Adopt a discovered pool: a pool matching its model, under its name or another. */
function AdoptDialog({
  slug,
  pool,
  onClose,
  onAdopt,
}: {
  slug: string;
  pool?: PciPoolJson;
  onClose: () => void;
  onAdopt: (spec: PciPoolSpecJson) => Promise<void>;
}) {
  const [name, setName] = useState(slug);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const match = api.discoveredMatch(slug);
  const go = async () => {
    if (!match) return;
    setBusy(true);
    try {
      await onAdopt({ name: name.trim(), class: pool?.class, match });
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title={`Adopt ${slug}`} onClose={onClose}>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <p className="text-sm text-gray-600 mb-3">
        A pool matching this model is written to the cluster setting. Then you can size it, set its window and grant it. A new name is refused
        while VMs ask for {slug}.
      </p>
      <label className="block text-sm font-medium text-gray-700">
        Name
        <input className={inputClass} value={name} onChange={(e) => setName(e.target.value)} />
      </label>
      <div className="flex justify-end gap-2 mt-4">
        <button className={secondaryButton} onClick={onClose}>
          Cancel
        </button>
        <button className={primaryButton} disabled={busy || !match || !name.trim()} onClick={go}>
          Adopt
        </button>
      </div>
    </Modal>
  );
}

/** The whole setting as JSON, the shape of control-plane.json's `pci`. */
function EditDialog({
  pools,
  allow,
  onClose,
  onSave,
}: {
  pools: PciPoolSpecJson[];
  allow: PciGrantJson[];
  onClose: () => void;
  onSave: (next: { pools: PciPoolSpecJson[]; allow: PciGrantJson[] }) => Promise<void>;
}) {
  const [text, setText] = useState(JSON.stringify({ pools, allow }, null, 2));
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const go = async () => {
    let next: { pools?: PciPoolSpecJson[]; allow?: PciGrantJson[] };
    try {
      next = JSON.parse(text);
    } catch (e) {
      setError(`Not JSON: ${errorMessage(e)}`);
      return;
    }
    setBusy(true);
    try {
      await onSave({ pools: next.pools ?? [], allow: next.allow ?? [] });
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title="Device pools and grants" onClose={onClose}>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      <p className="text-sm text-gray-600 mb-2">
        <code>pools</code> and <code>allow</code>, as in a configuration file's <code>pci</code> section. Saving replaces them for every server.
      </p>
      <textarea
        aria-label="Pools and grants (JSON)"
        className={`${inputClass} font-mono text-xs h-80`}
        value={text}
        onChange={(e) => setText(e.target.value)}
        spellCheck={false}
      />
      <div className="flex justify-end gap-2 mt-4">
        <button className={secondaryButton} onClick={onClose}>
          Cancel
        </button>
        <button className={primaryButton} disabled={busy} onClick={go}>
          Save
        </button>
      </div>
    </Modal>
  );
}
