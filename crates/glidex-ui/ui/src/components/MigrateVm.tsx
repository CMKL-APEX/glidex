import { useState } from "react";
import * as api from "../api";
import { formatTime, migrating, migrationPhase, type VmResponse } from "../types";
import { useCan } from "../session";
import { useNodes } from "../nodes";
import Modal from "./Modal";
import NodeName from "./NodeName";

const button = "px-3 py-2 text-sm font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg";

/** "Migrate…" (spec/32-live-migration.md decision 20): a running VM in a
 * cluster moves to a chosen node, or where it fits best, while it runs.
 * The dialog shows `migrate --check`'s answer for each node. Shown to
 * cluster operators (`migrateVm`). */
export default function MigrateVm({ vm, running, onDone }: { vm: VmResponse; running: boolean; onDone: () => void }) {
  const dir = useNodes();
  const [can] = useCan([{ action: "migrateVm", resource: { type: "Vm", id: vm.id } }]) ?? [];
  const [open, setOpen] = useState(false);
  const [check, setCheck] = useState<api.MigrationCheck | null>(null);
  const [node, setNode] = useState("");
  const [downtime, setDowntime] = useState("300");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  if (!can || !dir.clustered || !running || migrating(vm)) return null;

  const show = async () => {
    setOpen(true);
    setCheck(null);
    setNode("");
    setError(null);
    try {
      setCheck(await api.checkMigration(vm.id));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  const ms = Number(downtime);
  const downtimeOk = Number.isInteger(ms) && ms >= 1;
  const own = check?.reasons ?? [];
  const fitting = (check?.targets ?? []).filter((t) => !(t.reasons ?? []).length);
  const migrate = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.migrateVm(vm.id, node || undefined, ms);
      setOpen(false);
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <button className={button} onClick={show}>
        Migrate…
      </button>
      {open && (
        <Modal title={`Migrate ${vm.name}`} onClose={() => setOpen(false)}>
          <div className="space-y-4 text-sm">
            {error && <p className="text-red-600">{error}</p>}
            <p className="text-gray-600">
              It moves to another node while it runs. The guest pauses for about the downtime at the switch, and keeps its addresses,
              disks and console.
            </p>
            {!check && !error && <p className="text-gray-500">Checking where it can go…</p>}
            {check && own.length > 0 && (
              <div data-testid="migrate-vm-reasons">
                <p className="font-medium text-gray-700">It can't migrate:</p>
                <ul className="list-disc pl-5 text-gray-600">
                  {own.map((r) => (
                    <li key={r}>{r}</li>
                  ))}
                </ul>
              </div>
            )}
            {check && own.length === 0 && (
              <>
                <div>
                  <label className="block text-sm font-medium text-gray-700" htmlFor="migrate-node">
                    Node
                  </label>
                  <select
                    id="migrate-node"
                    className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg bg-white"
                    value={node}
                    onChange={(e) => setNode(e.target.value)}
                  >
                    <option value="" disabled={!fitting.length}>
                      Where it fits best
                    </option>
                    {(check.targets ?? []).map((t) => (
                      <option key={t.node} value={t.node} disabled={!!(t.reasons ?? []).length}>
                        {t.name}
                        {(t.reasons ?? []).length ? ` — ${(t.reasons ?? []).join("; ")}` : ""}
                      </option>
                    ))}
                  </select>
                  {!fitting.length && <p className="mt-1 text-red-600">No node can take it now.</p>}
                </div>
                <div>
                  <label className="block text-sm font-medium text-gray-700" htmlFor="migrate-downtime">
                    Longest pause at the switch (ms)
                  </label>
                  <input
                    id="migrate-downtime"
                    type="number"
                    min={1}
                    className="mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg"
                    value={downtime}
                    onChange={(e) => setDowntime(e.target.value)}
                  />
                </div>
              </>
            )}
            <div className="flex justify-end space-x-3">
              <button className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg" onClick={() => setOpen(false)}>
                Close
              </button>
              <button
                disabled={busy || !check || own.length > 0 || !fitting.length || !downtimeOk}
                className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50"
                onClick={migrate}
              >
                {busy ? "Starting…" : "Migrate"}
              </button>
            </div>
          </div>
        </Modal>
      )}
    </>
  );
}

/** The VM's migration on its page (decision 20): in progress, with Cancel
 * for those with `migrateVm`; or the last one's failure. */
export function MigrationStatus({ vm, onDone }: { vm: VmResponse; onDone: () => void }) {
  const [can] = useCan([{ action: "migrateVm", resource: { type: "Vm", id: vm.id } }]) ?? [];
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const m = vm.migration;
  if (!m) return null;
  const target = <NodeName id={m.target} className="font-medium" />;
  if (!migrating(vm)) {
    return (
      <div className="mb-6 p-3 bg-amber-50 border border-amber-200 rounded-lg text-sm text-amber-800" data-testid="migration-failed">
        The last migration, to {target}, failed{m.reason ? `: ${m.reason}` : ""}. The VM stayed where it was.
      </div>
    );
  }
  const cancellable = m.phase === "MIGRATION_PHASE_PREPARING" || m.phase === "MIGRATION_PHASE_COPYING";
  const cancel = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.cancelMigration(vm.id);
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="mb-6 p-3 bg-sky-50 border border-sky-200 rounded-lg text-sm text-sky-800" data-testid="migration-status">
      <div className="flex items-center justify-between gap-3">
        <span>
          <span className="font-medium">Migrating</span> to {target}: {migrationPhase(m)}
          {m.started_at && <>, since {formatTime(Number(m.started_at))}</>}.
          {m.reason && <span className="block text-xs mt-1">{m.reason}</span>}
        </span>
        {can && cancellable && (
          <button disabled={busy} className={`${button} disabled:opacity-50`} onClick={cancel}>
            {busy ? "Cancelling…" : "Cancel migration"}
          </button>
        )}
      </div>
      {error && <p className="text-red-600 mt-1">{error}</p>}
    </div>
  );
}
