import { useState } from "react";
import * as api from "../api";
import type { BrIntDatapath, NodeRecord } from "../types";
import ConfirmDialog from "./ConfirmDialog";

const CHOICES: { value: BrIntDatapath; label: string; help: string }[] = [
  { value: "BR_INT_DATAPATH_AUTO", label: "Automatic", help: "Userspace (DPDK) where OVS runs DPDK, else the kernel datapath." },
  {
    value: "BR_INT_DATAPATH_SYSTEM",
    label: "Kernel (system)",
    help: "What a gateway node needs: its external traffic is metered from kernel conntrack. VMs here get tap NICs on cluster networks; DPDK stays available to node networks.",
  },
  { value: "BR_INT_DATAPATH_NETDEV", label: "Userspace (netdev, DPDK)", help: "For vhost-user NICs on cluster networks. Needs DPDK; can't carry a gateway." },
];

/** A node's br-int datapath (spec/14-clustering.md §11.6). The node switches
 * within half a minute; netd refuses while VM ports sit on br-int. */
export default function BrIntDialog({ node, onClose, onDone }: { node: NodeRecord; onClose: () => void; onDone: () => void }) {
  const [value, setValue] = useState<BrIntDatapath>(node.spec.br_int_datapath ?? "BR_INT_DATAPATH_AUTO");
  return (
    <ConfirmDialog
      title={`br-int datapath of ${node.spec.name}`}
      consequence={
        <>
          OVN's integration bridge on this node moves to the chosen datapath within half a minute. It can only move while no VM port is on it:
          stop or move this node's VMs on cluster networks first.
        </>
      }
      actionLabel="Save"
      danger={false}
      onConfirm={async () => {
        await api.setNodeBrInt(node.meta.id, value);
        onDone();
      }}
      onClose={onClose}
    >
      <div className="space-y-2">
        {CHOICES.map((c) => (
          <label key={c.value} className="flex items-start gap-2">
            <input type="radio" name="br-int" className="mt-1" checked={value === c.value} onChange={() => setValue(c.value)} />
            <span>
              <span className="font-medium">{c.label}</span>
              <span className="block text-xs text-gray-500">{c.help}</span>
            </span>
          </label>
        ))}
      </div>
    </ConfirmDialog>
  );
}
