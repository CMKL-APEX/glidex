import { useCallback, useEffect, useState } from "react";
import * as api from "../api";
import type { BridgeRecord, Datapath, UplinkKind, UplinkResult } from "../types";
import { datapathLabel, formatTimestamp, uplinkKindLabel } from "../types";
import { useCan } from "../session";
import ConfirmDialog from "./ConfirmDialog";
import Modal from "./Modal";
import { Badge, ErrorBanner, errorMessage, inputClass, primaryButton, secondaryButton, selectClass } from "./ui";

/** Commit a moved address once the API still answers through the new path
 * (networking.md §8.5): netd rolls it back by itself otherwise. */
async function commitWhenReachable(r: UplinkResult): Promise<string> {
  if (r.phase !== "UPLINK_PHASE_PENDING_COMMIT" || !r.record.pending) return "Added.";
  // Fresh request through the API: if it answers, the host is reachable.
  await api.healthCheck();
  await api.commitUplink(r.record.spec.bridge, r.record.spec.name, r.record.pending.token);
  return "The host's address moved to the bridge and was committed.";
}

function UplinkRow({ u, canChange, onChanged }: { u: UplinkResult; canChange: boolean; onChanged: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [removing, setRemoving] = useState(false);
  const r = u.record;
  const nic = r.spec.ifname ?? r.spec.pci;
  const live = u.live ?? {};
  const held = r.boot_rollback;
  const reapply = async () => {
    setBusy(true);
    setError(null);
    try {
      await commitWhenReachable(await api.reapplyUplink(r.spec.bridge, r.spec.name));
      onChanged();
    } catch (e) {
      setError(`${errorMessage(e)} (if the address had moved, netd puts it back within a minute)`);
    } finally {
      setBusy(false);
    }
  };
  return (
    <li className="py-1">
      <span className="font-mono">{r.spec.name}</span> <span className="text-gray-500">({uplinkKindLabel(r.spec.kind)}{nic && nic !== r.spec.name ? ` ${nic}` : ""})</span>{" "}
      {held ? (
        <Badge kind="denied">held off the bridge</Badge>
      ) : u.phase === "UPLINK_PHASE_PENDING_COMMIT" ? (
        <Badge kind="warn">waiting for commit</Badge>
      ) : (
        <Badge kind={live.error ? "denied" : "ok"}>{live.error ? "error" : (live.link_state ?? "active")}</Badge>
      )}
      {r.spec.migrate_ip && <span className="text-xs text-gray-500"> · carries the host's address</span>}
      {held && (
        <p className="text-xs text-red-700 mt-1">
          At start {formatTimestamp(held.at)} it was rolled back so the host stayed reachable: {held.reason}. Fix the cause, then re-apply.
        </p>
      )}
      {live.error && <p className="text-xs text-red-700">{live.error}</p>}
      {(live.warnings ?? []).map((w) => (
        <p key={w} className="text-xs text-amber-700">
          {w}
        </p>
      ))}
      {error && <p className="text-xs text-red-700">{error}</p>}
      {canChange && (
        <span className="ml-2 space-x-3 text-xs">
          {held && (
            <button className="text-sky-700 hover:underline disabled:opacity-50" disabled={busy} onClick={reapply}>
              {busy ? "Re-applying…" : "Re-apply"}
            </button>
          )}
          <button className="text-red-600 hover:underline" onClick={() => setRemoving(true)}>
            Remove
          </button>
        </span>
      )}
      {removing && (
        <ConfirmDialog
          title={`Remove uplink ${r.spec.name}`}
          consequence={r.spec.migrate_ip ? "The NIC leaves the bridge and gets the host's address back. Networks on this bridge lose their way out." : "The NIC leaves the bridge."}
          actionLabel="Remove"
          onConfirm={async () => {
            await api.deleteUplink(r.spec.bridge, r.spec.name);
            onChanged();
          }}
          onClose={() => setRemoving(false)}
        />
      )}
    </li>
  );
}

/** Add a NIC to a bridge, optionally moving the host's address onto it. */
function AddUplinkDialog({ bridge, onClose, onDone }: { bridge: string; onClose: () => void; onDone: () => void }) {
  const [kind, setKind] = useState<UplinkKind["kind"]>("UPLINK_KIND_KERNEL");
  const [nic, setNic] = useState("");
  const [port, setPort] = useState("");
  const [migrate, setMigrate] = useState(false);
  const [acceptDhcp, setAcceptDhcp] = useState(false);
  const [step, setStep] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);

  const add = async () => {
    setError(null);
    setStep(migrate ? "Moving the host's address to the bridge and checking the gateway…" : "Adding…");
    try {
      const name = kind === "UPLINK_KIND_DPDK" ? port : nic;
      const k: UplinkKind = kind === "UPLINK_KIND_DPDK" ? { kind, pci: nic } : { kind, ifname: nic };
      const r = await api.addUplink(bridge, { name, migrate_ip: migrate, confirm: migrate, accept_dhcp: acceptDhcp, ...k });
      setStep("Checking the host still answers through the bridge, then committing…");
      setDone(await commitWhenReachable(r));
      onDone();
    } catch (e) {
      setError(`${errorMessage(e)}${migrate ? " — if the address had moved, netd put it back (or does within a minute)." : ""}`);
    } finally {
      setStep(null);
    }
  };

  return (
    <Modal title={`Add an uplink to ${bridge}`} onClose={step ? () => {} : onClose}>
      {done ? (
        <div className="space-y-4 text-sm">
          <p className="text-green-700">{done}</p>
          <div className="flex justify-end">
            <button className={primaryButton} onClick={onClose}>
              Done
            </button>
          </div>
        </div>
      ) : (
        <div className="space-y-4 text-sm">
          <ErrorBanner error={error} onDismiss={() => setError(null)} />
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label htmlFor="uplink-kind" className="block text-xs text-gray-500">
                Attach as
              </label>
              <select id="uplink-kind" className={selectClass} value={kind} onChange={(e) => setKind(e.target.value as typeof kind)}>
                <option value="UPLINK_KIND_KERNEL">Kernel NIC</option>
                <option value="UPLINK_KIND_AFXDP">AF_XDP (userspace bridge)</option>
                <option value="UPLINK_KIND_DPDK">DPDK (userspace bridge, takes the NIC from the kernel)</option>
              </select>
            </div>
            <div>
              <label htmlFor="uplink-nic" className="block text-xs text-gray-500">
                {kind === "UPLINK_KIND_DPDK" ? "PCI address" : "Interface"}
              </label>
              <input id="uplink-nic" className={inputClass} value={nic} placeholder={kind === "UPLINK_KIND_DPDK" ? "0000:41:00.0" : "eno1"} onChange={(e) => setNic(e.target.value.trim())} />
            </div>
            {kind === "UPLINK_KIND_DPDK" && (
              <div>
                <label className="block text-xs text-gray-500">Port name</label>
                <input aria-label="Port name" className={inputClass} value={port} placeholder="up0" onChange={(e) => setPort(e.target.value.trim())} />
              </div>
            )}
          </div>
          {kind !== "UPLINK_KIND_DPDK" && (
            <label className="flex items-start gap-2">
              <input type="checkbox" className="mt-1" checked={migrate} onChange={(e) => setMigrate(e.target.checked)} />
              <span>
                The host uses this NIC: move its address and routes to the bridge.
                <span className="block text-xs text-gray-500">
                  glidex checks the default gateway still answers, then waits for this page to confirm; if either fails, the NIC gets its address back
                  within a minute. After a reboot it is moved again the same way, and rolled back if the gateway doesn't answer.
                </span>
              </span>
            </label>
          )}
          {migrate && (
            <label className="flex items-start gap-2">
              <input type="checkbox" className="mt-1" checked={acceptDhcp} onChange={(e) => setAcceptDhcp(e.target.checked)} />
              <span>
                If the address comes from DHCP, move it anyway.
                <span className="block text-xs text-amber-700">The DHCP client stays on the NIC: the address may be lost when its lease ends. Prefer a static address.</span>
              </span>
            </label>
          )}
          {step && <p className="text-sky-700">{step}</p>}
          <div className="flex justify-end gap-2">
            <button className={secondaryButton} onClick={onClose} disabled={!!step}>
              Cancel
            </button>
            <button className={primaryButton} onClick={add} disabled={!!step || !nic || (kind === "UPLINK_KIND_DPDK" && !port)}>
              Add uplink
            </button>
          </div>
        </div>
      )}
    </Modal>
  );
}

/** The host's glidex bridges and their uplinks (networking.md §8). */
export default function BridgesCard({ bridges, onChanged }: { bridges: BridgeRecord[]; onChanged: () => void }) {
  const [uplinks, setUplinks] = useState<Record<string, UplinkResult[]>>({});
  const [adding, setAdding] = useState<string | null>(null);
  const [newBridge, setNewBridge] = useState(false);
  const [name, setName] = useState("gxbr-up");
  const [datapath, setDatapath] = useState<Datapath>("DATAPATH_SYSTEM");
  const [error, setError] = useState<string | null>(null);
  const caps = useCan([
    { action: "createBridge", resource: { type: "Host" } },
    { action: "ensureUplink", resource: { type: "Host" } },
    { action: "deleteUplink", resource: { type: "Host" } },
  ]);
  const [canBridge, canUplink, canRemove] = caps ?? [false, false, false];

  const load = useCallback(async () => {
    const out: Record<string, UplinkResult[]> = {};
    await Promise.all(
      bridges.map(async (b) => {
        out[b.spec.name] = await api.listUplinks(b.spec.name).catch(() => []);
      }),
    );
    setUplinks(out);
  }, [bridges]);
  useEffect(() => {
    load();
  }, [load]);
  const changed = () => {
    load();
    onChanged();
  };

  return (
    <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-4 text-sm">
      <div className="flex items-center justify-between mb-2">
        <h2 className="font-medium text-gray-700">glidex bridges</h2>
        {canBridge && (
          <button className="text-sky-700 hover:underline" onClick={() => setNewBridge(true)}>
            + New bridge
          </button>
        )}
      </div>
      <ErrorBanner error={error} onDismiss={() => setError(null)} />
      {bridges.length === 0 && <p className="text-gray-500">No glidex bridges yet.</p>}
      <ul className="divide-y divide-gray-100">
        {bridges.map((b) => (
          <li key={b.spec.name} className="py-2">
            <div className="flex items-center justify-between">
              <span>
                <span className="font-mono">{b.spec.name}</span>{" "}
                <span className="text-gray-500">
                  ({datapathLabel(b.spec.datapath)}
                  {b.live ? `, ${b.live.ports?.length ?? 0} ports` : ", missing on host"})
                </span>
              </span>
              {canUplink && (
                <button className="text-sky-700 hover:underline text-xs" onClick={() => setAdding(b.spec.name)}>
                  Add uplink…
                </button>
              )}
            </div>
            {(uplinks[b.spec.name] ?? []).length > 0 && (
              <ul className="ml-4 mt-1">
                {uplinks[b.spec.name].map((u) => (
                  <UplinkRow key={u.record.spec.name} u={u} canChange={canRemove} onChanged={changed} />
                ))}
              </ul>
            )}
          </li>
        ))}
      </ul>
      {adding && <AddUplinkDialog bridge={adding} onClose={() => setAdding(null)} onDone={changed} />}
      {newBridge && (
        <ConfirmDialog
          title="New bridge"
          consequence="An Open vSwitch bridge glidex owns, for uplinks and bridged or provider networks. For an OVN gateway's physical network use the kernel datapath."
          actionLabel="Create"
          danger={false}
          onConfirm={async () => {
            await api.createBridge(name.trim(), datapath);
            changed();
          }}
          onClose={() => setNewBridge(false)}
        >
          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="block text-xs text-gray-500">Name</label>
              <input aria-label="Bridge name" className={inputClass} value={name} onChange={(e) => setName(e.target.value)} />
            </div>
            <div>
              <label className="block text-xs text-gray-500">Datapath</label>
              <select aria-label="Datapath" className={selectClass} value={datapath} onChange={(e) => setDatapath(e.target.value as Datapath)}>
                <option value="DATAPATH_SYSTEM">kernel (system)</option>
                <option value="DATAPATH_NETDEV">userspace (netdev, DPDK)</option>
              </select>
            </div>
          </div>
        </ConfirmDialog>
      )}
    </div>
  );
}
