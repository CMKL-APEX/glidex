import { useState } from "react";
import * as api from "../api";
import type { IssuedToken } from "../types";
import { nodeRoleLabel } from "../types";
import Modal from "./Modal";
import TokenReveal from "./TokenReveal";
import { ErrorBanner, errorMessage, primaryButton, secondaryButton, selectClass } from "./ui";

const TTLS: [number, string][] = [
  [600, "10 minutes"],
  [3600, "1 hour"],
  [86400, "1 day"],
  [7 * 86400, "7 days"],
];

/** A join token for a new node (spec/clustering-ui.md §7.1). */
export default function AddNodeDialog({ onClose }: { onClose: () => void }) {
  const [role, setRole] = useState<"agent" | "server">("agent");
  const [ttl, setTtl] = useState(3600);
  const [allowImport, setAllowImport] = useState(false);
  const [issued, setIssued] = useState<IssuedToken | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const close = () => {
    // The token goes with the dialog.
    setIssued(null);
    onClose();
  };

  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      setIssued(await api.createJoinToken({ role: role === "server" ? "NODE_ROLE_SERVER" : "NODE_ROLE_AGENT", ttl_secs: ttl, allow_import: allowImport }));
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const server = issued?.servers?.[0] ?? "<server>:8842";
  return (
    <Modal title="Add a node" onClose={close}>
      {issued ? (
        <>
          <TokenReveal
            issued={issued}
            commands={[
              "# a fresh host, with the installer:",
              `sudo glidex-install --join ${server} --role ${nodeRoleLabel(issued.role) || role}${allowImport ? " --import" : ""} --token-file ./join-token`,
              "# or a host that already runs glidex:",
              `gxctl cluster join --server ${server} --role ${nodeRoleLabel(issued.role) || role} --token-file ./join-token`,
            ]}
          />
          <div className="mt-4 flex justify-end">
            <button className={primaryButton} onClick={close}>
              Done
            </button>
          </div>
        </>
      ) : (
        <div className="space-y-4 text-sm">
          <ErrorBanner error={error} onDismiss={() => setError(null)} />
          <div>
            <label htmlFor="join-role" className="block font-medium text-gray-700">
              Role
            </label>
            <select id="join-role" className={selectClass} value={role} onChange={(e) => setRole(e.target.value as "agent" | "server")}>
              <option value="agent">Agent: runs VMs</option>
              <option value="server">Server: runs VMs and holds the cluster's database</option>
            </select>
          </div>
          <div>
            <label htmlFor="join-ttl" className="block font-medium text-gray-700">
              Token valid for
            </label>
            <select id="join-ttl" className={selectClass} value={ttl} onChange={(e) => setTtl(Number(e.target.value))}>
              {TTLS.map(([s, l]) => (
                <option key={s} value={s}>
                  {l}
                </option>
              ))}
            </select>
          </div>
          <label className="flex items-start gap-2">
            <input type="checkbox" className="mt-1" checked={allowImport} onChange={(e) => setAllowImport(e.target.checked)} />
            <span>The host already has VMs to bring along (an import, approved here before anything moves).</span>
          </label>
          <div className="flex justify-end gap-2">
            <button className={secondaryButton} onClick={close}>
              Cancel
            </button>
            <button className={primaryButton} onClick={create} disabled={busy}>
              {busy ? "Creating..." : "Create token"}
            </button>
          </div>
        </div>
      )}
    </Modal>
  );
}
