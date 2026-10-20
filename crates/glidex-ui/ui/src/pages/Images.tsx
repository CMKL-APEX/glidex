import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import type { CatalogItem, FirmwareCatalogItem, HypervisorType, ImageInfo, NodeRecord } from "../types";
import {
  HYPERVISOR_LABELS,
  HYPERVISOR_TO_PROTO,
  archLabel,
  archToProto,
  formatBytes,
  hypervisorOf,
  imageState,
  isDiskImage,
  isFirmware,
  isKernel,
  isTombstone,
  num,
  sourceKey,
} from "../types";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";
import { useLiveRefresh } from "../live";
import { useNodes } from "../nodes";

const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";

function StatusBadge({ image }: { image: ImageInfo }) {
  const s = image.status ?? {};
  if (image.deleting) return <span className="text-xs text-gray-500">Deleting…</span>;
  switch (imageState(image)) {
    case "downloading": {
      const total = num(s.total_bytes);
      const pct = total ? Math.floor((num(s.received_bytes) * 100) / total) : null;
      return (
        <div className="w-40">
          <div className="text-xs text-sky-700">
            Downloading {pct !== null ? `${pct}%` : formatBytes(s.received_bytes)}
          </div>
          <div className="mt-1 h-1.5 bg-gray-200 rounded">
            <div className="h-1.5 bg-sky-500 rounded" style={{ width: `${pct ?? 5}%` }} />
          </div>
        </div>
      );
    }
    case "verifying":
      return <span className="text-xs text-sky-700">Verifying…</span>;
    case "extracting":
      return <span className="text-xs text-sky-700">Extracting…</span>;
    case "ready":
      return <span className="text-xs text-green-700">Ready</span>;
    case "missing":
      return <span className="text-xs text-red-700">File missing</span>;
    case "failed": {
      const reason = s.reason ?? "";
      return (
        <span className="text-xs text-red-700" title={reason}>
          Failed: {reason.length > 60 ? `${reason.slice(0, 60)}…` : reason}
        </span>
      );
    }
  }
  return null;
}

function PullUrlForm({ onDone, onCancel }: { onDone: () => void; onCancel: () => void }) {
  const [url, setUrl] = useState("");
  const [sha256, setSha256] = useState("");
  const [name, setName] = useState("");
  const [kind, setKind] = useState<"disk" | "firmware">("disk");
  const [hypervisor, setHypervisor] = useState<HypervisorType>("cloudhypervisor");
  const [varsUrl, setVarsUrl] = useState("");
  const [varsSha256, setVarsSha256] = useState("");
  const [error, setError] = useState<string | null>(null);
  // QEMU maps split OVMF builds as code plus a variable store.
  const withVars = kind === "firmware" && hypervisor === "qemu";

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    try {
      await api.pullImage({
        url,
        sha256: sha256 || undefined,
        name: name || undefined,
        ...(kind === "firmware" ? { kind: "IMAGE_KIND_FIRMWARE" as const, hypervisor: HYPERVISOR_TO_PROTO[hypervisor] } : {}),
        ...(withVars && varsUrl ? { vars_url: varsUrl, vars_sha256: varsSha256 || undefined } : {}),
      });
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to start download");
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <div className="grid grid-cols-2 gap-4">
        <div>
          <label className="block text-sm font-medium text-gray-700">Type</label>
          <select className={`${inputClass} bg-white`} value={kind} onChange={(e) => setKind(e.target.value as "disk" | "firmware")}>
            <option value="disk">Cloud image (qcow2 or raw)</option>
            <option value="firmware">UEFI firmware (.fd)</option>
          </select>
        </div>
        {kind === "firmware" && (
          <div>
            <label className="block text-sm font-medium text-gray-700">For hypervisor</label>
            <select className={`${inputClass} bg-white`} value={hypervisor} onChange={(e) => setHypervisor(e.target.value as HypervisorType)}>
              {(Object.entries(HYPERVISOR_LABELS) as [HypervisorType, string][]).map(([value, label]) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </select>
          </div>
        )}
      </div>
      <div>
        <label className="block text-sm font-medium text-gray-700">{kind === "firmware" ? "Firmware" : "Image"} URL (https)</label>
        <input className={inputClass} required placeholder={kind === "firmware" ? "https://…/firmware.fd" : "https://…/image.qcow2"} value={url} onChange={(e) => setUrl(e.target.value)} />
      </div>
      <div>
        <label className="block text-sm font-medium text-gray-700">sha256 (optional)</label>
        <input className={`${inputClass} font-mono text-xs`} value={sha256} onChange={(e) => setSha256(e.target.value)} />
        <p className="mt-1 text-xs text-gray-500">Without it the image is stored but marked unverified.</p>
      </div>
      {withVars && (
        <>
          <div>
            <label className="block text-sm font-medium text-gray-700">Variable store URL (optional)</label>
            <input className={inputClass} placeholder="https://…/OVMF_VARS.fd" value={varsUrl} onChange={(e) => setVarsUrl(e.target.value)} />
            <p className="mt-1 text-xs text-gray-500">
              The OVMF_VARS file that goes with a split OVMF_CODE build. Each VM gets its own copy, so boot settings persist.
              Without it the firmware is loaded as one file, as a combined OVMF.fd needs.
            </p>
          </div>
          {varsUrl && (
            <div>
              <label className="block text-sm font-medium text-gray-700">Variable store sha256 (optional)</label>
              <input className={`${inputClass} font-mono text-xs`} value={varsSha256} onChange={(e) => setVarsSha256(e.target.value)} />
            </div>
          )}
        </>
      )}
      <div>
        <label className="block text-sm font-medium text-gray-700">Name (optional)</label>
        <input className={inputClass} value={name} onChange={(e) => setName(e.target.value)} />
      </div>
      <div className="flex justify-end space-x-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg">
          Download
        </button>
      </div>
    </form>
  );
}

const cardClass = "bg-white rounded-xl shadow-sm border border-gray-200 p-4 flex items-center justify-between gap-3";
const pullClass = "px-3 py-1 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-lg disabled:opacity-50 whitespace-nowrap";

/** Where an image came from, for the tables. */
function SourceCell({ img }: { img: ImageInfo }) {
  const s = img.source ?? {};
  const cat = s.catalog ?? s.firmware;
  return (
    <>
      {s.url ? (
        <span className="break-all">{s.url.url}</span>
      ) : s.host ? (
        <span className="break-all">{s.host.path}</span>
      ) : s.extracted ? (
        <span className="break-all">{s.extracted.path ? `extracted: ${s.extracted.path}` : "extracting from its image"}</span>
      ) : (
        <span>
          {cat?.key} <span className="text-gray-400">{cat?.version}</span>
          {s.firmware?.url?.startsWith("/") && <span className="block text-gray-400 break-all">{s.firmware.url}</span>}
        </span>
      )}
      {!img.verified && <span className="ml-1 text-amber-700">(unverified)</span>}
    </>
  );
}

interface TableProps {
  images: ImageInfo[];
  kind: "disk" | "firmware" | "kernel";
  onRetry: (img: ImageInfo) => void;
  onRemove: (img: ImageInfo) => void;
  /** In a cluster: copies on chosen nodes (spec/clustering.md §9.2.2). */
  clustered?: boolean;
  onCopy?: (img: ImageInfo) => void;
  onRemoveCopy?: (img: ImageInfo, node: string) => void;
  /** Disk images: take the kernel out of one (spec/kernel-boot.md §10),
   * offered when no kernel image comes from it yet. */
  onExtractKernel?: (img: ImageInfo) => void;
  hasKernel?: (img: ImageInfo) => boolean;
}

function ImageTable({ images, kind, onRetry, onRemove, clustered, onCopy, onRemoveCopy, onExtractKernel, hasKernel }: TableProps) {
  // Firmware and kernels are booted through, so VMs are their users.
  const firmware = kind === "firmware";
  const boot = kind !== "disk";
  return (
    <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-x-auto">
      <table className="w-full text-sm" data-testid={`${kind}-images`}>
        <thead className="bg-gray-50 text-gray-600 text-left">
          <tr>
            <th className="px-4 py-3 font-medium">Name</th>
            {firmware && <th className="px-4 py-3 font-medium">Hypervisor</th>}
            {kind === "kernel" && <th className="px-4 py-3 font-medium">Release</th>}
            <th className="px-4 py-3 font-medium">Status</th>
            <th className="px-4 py-3 font-medium">Size</th>
            <th className="px-4 py-3 font-medium">Source</th>
            <th className="px-4 py-3 font-medium">{boot ? "Used by VMs" : "Linked disks"}</th>
            <th className="px-4 py-3" />
          </tr>
        </thead>
        <tbody className="divide-y divide-gray-100">
          {images.map((img) => {
            const users = (boot ? img.used_by_vms : img.linked_disks) ?? [];
            return (
              <tr key={img.id}>
                <td className="px-4 py-3 font-mono">
                  {img.name}
                  {clustered && <span className="ml-1 text-xs text-gray-500 font-sans">{archLabel(img.arch)}</span>}
                </td>
                {firmware && (
                  <td className="px-4 py-3">
                    {img.hypervisor ? HYPERVISOR_LABELS[hypervisorOf(img.hypervisor)!] : "—"}
                    {hypervisorOf(img.hypervisor) === "qemu" && (
                      <span className="block text-xs text-gray-500">
                        {img.vars_template ? "with variable store" : "single file (no saved UEFI settings)"}
                      </span>
                    )}
                  </td>
                )}
                {kind === "kernel" && (
                  <td className="px-4 py-3">
                    <span className="font-mono">{img.kernel?.release ?? "—"}</span>
                    <span className="block text-xs text-gray-500">{img.kernel?.initrd ? "with initramfs" : "no initramfs"}</span>
                    {img.kernel?.cmdline && <span className="block text-xs text-gray-500 font-mono break-all">{img.kernel.cmdline}</span>}
                  </td>
                )}
                <td className="px-4 py-3">
                  <StatusBadge image={img} />
                  <Copies img={img} onRemove={onRemoveCopy && ((node) => onRemoveCopy(img, node))} />
                </td>
                <td className="px-4 py-3">
                  {boot
                    ? num(img.file_size_bytes)
                      ? formatBytes(img.file_size_bytes)
                      : "—"
                    : num(img.virtual_size_bytes)
                      ? formatBytes(img.virtual_size_bytes)
                      : "—"}
                </td>
                <td className="px-4 py-3 text-xs text-gray-600">
                  <SourceCell img={img} />
                </td>
                <td className="px-4 py-3 text-xs font-mono">{users.join(", ") || "—"}</td>
                <td className="px-4 py-3 text-right space-x-2 whitespace-nowrap">
                  {onExtractKernel && imageState(img) === "ready" && !img.deleting && !hasKernel?.(img) && (
                    <button className="px-3 py-1 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-lg" onClick={() => onExtractKernel(img)}>
                      Extract kernel
                    </button>
                  )}
                  {clustered && onCopy && imageState(img) === "ready" && !img.deleting && (
                    <button className="px-3 py-1 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-lg" onClick={() => onCopy(img)}>
                      Copy to node…
                    </button>
                  )}
                  {imageState(img) === "failed" && (
                    <button
                      className="px-3 py-1 text-xs font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg"
                      onClick={() => onRetry(img)}
                    >
                      Retry
                    </button>
                  )}
                  <button
                    className="px-3 py-1 text-xs font-medium text-red-700 bg-red-50 hover:bg-red-100 rounded-lg disabled:opacity-50"
                    disabled={users.length > 0 && boot}
                    title={users.length > 0 && firmware ? "VMs boot through it" : undefined}
                    onClick={() => onRemove(img)}
                  >
                    {imageState(img) === "downloading" ? "Cancel" : "Delete"}
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

/** Where the image's file is: one line per node's copy (in a cluster),
 * and per shared datastore's copy. */
function Copies({ img, onRemove }: { img: ImageInfo; onRemove?: (node: string) => void }) {
  const all = img.copies ?? [];
  const copies = all.filter((c) => !c.datastore);
  const stores = all.filter((c) => c.datastore);
  if (all.length === 0) return null;
  const ready = copies.filter((c) => c.state === "ready").length;
  const pending = copies.length - ready;
  const inStores = stores.filter((c) => c.state === "ready").length;
  return (
    <details className="mt-1 text-xs text-gray-600">
      <summary className="cursor-pointer">
        {copies.length > 0 && `on ${ready} node${ready === 1 ? "" : "s"}`}
        {pending ? `, ${pending} on the way, going or failed` : ""}
        {stores.length > 0 && `${copies.length > 0 ? "; " : ""}in ${inStores} shared datastore${inStores === 1 ? "" : "s"}`}
      </summary>
      <ul className="mt-1 space-y-0.5">
        {stores.map((c) => (
          <li key={`ds:${c.datastore}`}>
            <span className="font-medium">datastore {c.datastore_name ?? c.datastore}</span>:{" "}
            {c.state === "failed" ? (
              <span className="text-red-600">failed{c.error ? `: ${c.error}` : ""}</span>
            ) : c.state === "downloading" ? (
              "copying"
            ) : (
              "ready"
            )}
          </li>
        ))}
        {copies.map((c) => (
          <li key={c.node}>
            <span className="font-medium">{c.node_name ?? c.node.slice(0, 8)}</span>:{" "}
            {c.state === "ready" ? (
              "ready"
            ) : c.state === "downloading" ? (
              `copying${num(img.file_size_bytes) ? ` (${Math.min(100, Math.round((num(c.bytes) / num(img.file_size_bytes)) * 100))} %)` : ""}`
            ) : c.state === "wanted" ? (
              "wanted"
            ) : c.state === "releasing" ? (
              "going"
            ) : (
              <span className="text-red-600">failed{c.error ? `: ${c.error}` : ""}</span>
            )}
            {c.pinned && <span className="ml-1 text-gray-400">(kept here)</span>}
            {onRemove && c.state !== "releasing" && (
              <button
                className="ml-2 text-red-600 hover:underline disabled:opacity-40 disabled:no-underline"
                disabled={c.state === "ready" && ready <= 1}
                title={c.state === "ready" && ready <= 1 ? "The last ready copy: delete the image instead" : "Let this node's copy go"}
                onClick={() => onRemove(c.node)}
              >
                Remove
              </button>
            )}
          </li>
        ))}
      </ul>
    </details>
  );
}

/** Nodes an image of `arch` may go to (an older node, reporting none, may). */
function nodesFor(nodes: Map<string, NodeRecord>, arch: string): NodeRecord[] {
  return [...nodes.values()]
    .filter((n) => !isTombstone(n.status.phase) && (!n.status.features?.arch || !arch || n.status.features.arch === arch))
    .sort((a, b) => a.spec.name.localeCompare(b.spec.name));
}

/** A pull in a cluster (spec/clustering.md §9.2.1–2): which nodes get a
 * copy once it is downloaded, and whether the server keeps its own. */
function PullDialog({
  what,
  arch,
  nodes,
  onClose,
  onDone,
}: {
  what: { catalog?: string; firmware?: string; label: string };
  arch: string;
  nodes: Map<string, NodeRecord>;
  onClose: () => void;
  onDone: () => void;
}) {
  const candidates = nodesFor(nodes, arch);
  const [picked, setPicked] = useState<string[]>([]);
  const [keep, setKeep] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.pullImage({ catalog: what.catalog, firmware: what.firmware, arch: arch ? archToProto(arch) : undefined, nodes: picked.length ? picked : undefined, keep_server_copy: picked.length ? keep : undefined });
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to start download");
      setBusy(false);
    }
  };
  return (
    <Modal title={`Pull ${what.label}`} onClose={onClose}>
      <div className="space-y-4 text-sm">
        {error && <p className="text-red-600">{error}</p>}
        <p className="text-gray-600">
          A server downloads it{arch ? ` for ${arch}` : ""} and checks it. Nodes copy it from there when they need it; pick nodes to have a copy
          ready before then.
        </p>
        <div>
          <span className="block font-medium text-gray-700">Copy to nodes</span>
          {candidates.length === 0 ? (
            <p className="text-gray-500">No node of this architecture.</p>
          ) : (
            <div className="mt-1 flex flex-wrap gap-3">
              {candidates.map((n) => (
                <label key={n.meta.id} className="flex items-center gap-1">
                  <input type="checkbox" checked={picked.includes(n.meta.id)} onChange={(e) => setPicked(e.target.checked ? [...picked, n.meta.id] : picked.filter((x) => x !== n.meta.id))} />
                  {n.spec.name}
                </label>
              ))}
            </div>
          )}
        </div>
        <label className="flex items-start gap-2">
          <input type="checkbox" className="mt-1" checked={keep} disabled={picked.length === 0} onChange={(e) => setKeep(e.target.checked)} />
          <span>
            Keep the server's copy
            <span className="block text-xs text-gray-500">Unchecked: it goes once a chosen node has one, so it takes no space on the server.</span>
          </span>
        </label>
        <div className="flex justify-end gap-2">
          <button className="px-4 py-2 text-sm bg-gray-200 rounded-lg" onClick={onClose}>
            Cancel
          </button>
          <button className="px-4 py-2 text-sm text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50" disabled={busy} onClick={submit}>
            {busy ? "Starting…" : "Pull"}
          </button>
        </div>
      </div>
    </Modal>
  );
}

/** Keep a copy of an image on a node ahead of need (§9.2.2). */
function CopyDialog({ img, nodes, onClose, onDone }: { img: ImageInfo; nodes: Map<string, NodeRecord>; onClose: () => void; onDone: () => void }) {
  const has = new Set((img.copies ?? []).filter((c) => c.state !== "releasing").map((c) => c.node));
  const arch = archLabel(img.arch);
  const candidates = nodesFor(nodes, arch).filter((n) => !has.has(n.meta.id));
  const [error, setError] = useState<string | null>(null);
  const copy = async (node: string) => {
    setError(null);
    try {
      await api.addImageCopy(img.id, node);
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to copy");
    }
  };
  return (
    <Modal title={`Copy ${img.name} to a node`} onClose={onClose}>
      <div className="space-y-3 text-sm">
        {error && <p className="text-red-600">{error}</p>}
        <p className="text-gray-600">The node copies it from a node that has it, checks it, and keeps it until it is let go. Only {arch} nodes can run it.</p>
        {candidates.length === 0 ? (
          <p className="text-gray-500">Every {arch} node has a copy already.</p>
        ) : (
          <ul className="divide-y divide-gray-100">
            {candidates.map((n) => (
              <li key={n.meta.id} className="py-2 flex items-center justify-between">
                <span>{n.spec.name}</span>
                <button className="px-3 py-1 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-lg" onClick={() => copy(n.meta.id)}>
                  Copy here
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
    </Modal>
  );
}

export default function Images() {
  const [catalog, setCatalog] = useState<CatalogItem[]>([]);
  const [firmwareCatalog, setFirmwareCatalog] = useState<FirmwareCatalogItem[]>([]);
  const [images, setImages] = useState<ImageInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pullingUrl, setPullingUrl] = useState(false);
  // In a cluster (spec/clustering.md §9.2.1): the nodes' architectures, and
  // which one the catalogs are for (default: the server's).
  const dir = useNodes();
  const archs = [...new Set([...dir.nodes.values()].filter((n) => !isTombstone(n.status.phase)).map((n) => n.status.features?.arch).filter((a): a is string => !!a))].sort();
  const selfArch = (dir.self && dir.nodes.get(dir.self)?.status.features?.arch) || "";
  const [arch, setArch] = useState("");
  const [pulling, setPulling] = useState<{ catalog?: string; firmware?: string; label: string } | null>(null);
  const [copying, setCopying] = useState<ImageInfo | null>(null);

  const refresh = useCallback(() => {
    api.imageCatalog(arch || undefined).then(setCatalog).catch(() => setCatalog([]));
    api.firmwareCatalog(arch || undefined).then(setFirmwareCatalog).catch(() => setFirmwareCatalog([]));
    api
      .listImages()
      .then(setImages)
      .catch((e) => setError(e instanceof Error ? e.message : "Failed to load images"));
  }, [arch]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Follow the live stream; without it, poll while anything is downloading.
  const live = useLiveRefresh(["image", "disk", "vm"], refresh);
  const busy = images?.some(
    (i) => imageState(i) === "downloading" || imageState(i) === "verifying" || imageState(i) === "extracting" || i.deleting,
  );
  useEffect(() => {
    if (!busy || live) return;
    const id = setInterval(refresh, 1500);
    return () => clearInterval(id);
  }, [busy, live, refresh]);

  const pull = async (req: { catalog?: string; firmware?: string }, label = "") => {
    setError(null);
    // In a cluster: where copies go (§9.2.2).
    if (dir.clustered) {
      setPulling({ ...req, label: label || req.catalog || req.firmware || "" });
      return;
    }
    try {
      await api.pullImage(req);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to start download");
    }
  };

  const extractKernel = async (img: ImageInfo) => {
    setError(null);
    try {
      await api.pullImage({ kernel_from: img.id });
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to start the kernel extraction");
    }
  };

  const remove = async (img: ImageInfo) => {
    const what =
      imageState(img) === "downloading" ? "Cancel the download of" : isFirmware(img) ? "Delete firmware" : isKernel(img) ? "Delete kernel" : "Delete image";
    if (!confirm(`${what} ${img.name}?`)) return;
    setError(null);
    try {
      await api.deleteImage(img.id);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to delete image");
    }
  };

  const retry = async (img: ImageInfo) => {
    setError(null);
    try {
      await api.retryImage(img.id);
      refresh();
      // The image controller restarts the download a moment later; until
      // then the image still reads "failed" and nothing would poll.
      setTimeout(refresh, 1000);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to retry the download");
    }
  };

  const removeCopy = async (img: ImageInfo, node: string) => {
    const name = dir.nodes.get(node)?.spec.name ?? node;
    if (!confirm(`Let ${name}'s copy of ${img.name} go?`)) return;
    setError(null);
    try {
      await api.removeImageCopy(img.id, node);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to remove the copy");
    }
  };

  const inFlight = (kind: "catalog" | "firmware", key: string) =>
    images?.some(
      (i) =>
        sourceKey(i)?.kind === kind &&
        sourceKey(i)?.key === key &&
        (!arch || archLabel(i.arch) === arch) &&
        (imageState(i) === "downloading" || imageState(i) === "verifying"),
    );

  const diskImages = images?.filter(isDiskImage) ?? [];
  const firmwareImages = images?.filter(isFirmware) ?? [];
  const kernelImages = images?.filter(isKernel) ?? [];

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">Images</h1>
          <p className="text-sm text-gray-500 mt-1">
            Base cloud images disks are created from, and the UEFI firmware VMs boot through. Each is checked against its
            published checksum.
          </p>
        </div>
        <button className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg whitespace-nowrap" onClick={() => setPullingUrl(true)}>
          Download from URL
        </button>
      </div>

      {error && <div className="mb-4 p-4 bg-red-50 border border-red-200 rounded-lg text-red-700">{error}</div>}

      {archs.length > 1 && (
        <div className="mb-4 flex items-center gap-2 text-sm">
          <label htmlFor="catalog-arch" className="text-gray-600">
            Catalogs for
          </label>
          <select id="catalog-arch" className="px-2 py-1 border border-gray-300 rounded-lg bg-white" value={arch || selfArch} onChange={(e) => setArch(e.target.value)}>
            {archs.map((a) => (
              <option key={a} value={a}>
                {a} ({[...dir.nodes.values()].filter((n) => n.status.features?.arch === a && !isTombstone(n.status.phase)).map((n) => n.spec.name).join(", ")})
              </option>
            ))}
          </select>
        </div>
      )}
      <h2 className="text-lg font-semibold text-gray-800 mb-1">Cloud image catalog</h2>
      <p className="text-sm text-gray-500 mb-3">
        Pulling an image also extracts its kernel, for VMs that boot it directly (listed under Kernels).
      </p>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 mb-8">
        {catalog.map((c) => (
          <div key={c.key} className={cardClass}>
            <div>
              <div className="font-medium text-gray-900">
                {c.distro} {c.release}
              </div>
              <div className="text-xs font-mono text-gray-500">{c.key}</div>
            </div>
            {c.downloaded_image_id ? (
              <span className="text-xs text-green-700">Downloaded</span>
            ) : (
              <button className={pullClass} disabled={inFlight("catalog", c.key)} onClick={() => pull({ catalog: c.key }, `${c.distro} ${c.release}`)}>
                {inFlight("catalog", c.key) ? "Downloading…" : "Pull"}
              </button>
            )}
          </div>
        ))}
      </div>

      <h2 className="text-lg font-semibold text-gray-800 mb-1">Firmware catalog</h2>
      <p className="text-sm text-gray-500 mb-3">
        UEFI firmware VMs boot cloud images through, one build per hypervisor. A new VM uses the newest firmware image for
        its hypervisor unless you pick another.
      </p>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 mb-8" data-testid="firmware-catalog">
        {firmwareCatalog.map((f) => (
          <div key={f.key} className={cardClass}>
            <div className="min-w-0">
              <div className="font-medium text-gray-900">{f.name}</div>
              <div className="text-xs text-gray-500">
                <span className="font-mono">{f.key}</span> · {f.hypervisor ? HYPERVISOR_LABELS[hypervisorOf(f.hypervisor)!] : "—"}
              </div>
              <div className="text-xs text-gray-400 truncate" title={f.url}>
                {f.source === "download"
                  ? `Pinned build ${f.version ?? ""}`
                  : f.source === "debian"
                    ? `Debian package ${f.version ?? ""}, downloaded`
                    : f.url
                      ? `From ${f.url}`
                      : "From the host's package"}
              </div>
              {f.hint && <div className="text-xs text-amber-700">Not installed: {f.hint}</div>}
            </div>
            {f.downloaded_image_id ? (
              <span className="text-xs text-green-700">{f.source === "host" ? "Imported" : "Downloaded"}</span>
            ) : (
              <button
                className={pullClass}
                disabled={!f.available || inFlight("firmware", f.key)}
                onClick={() => pull({ firmware: f.key }, f.name)}
              >
                {inFlight("firmware", f.key) ? "Downloading…" : f.source === "host" ? "Import" : "Pull"}
              </button>
            )}
          </div>
        ))}
      </div>

      <h2 className="text-lg font-semibold text-gray-800 mb-3">Cloud images</h2>
      {images === null ? (
        <LoadingCard />
      ) : diskImages.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">
          No images yet. Pull one from the catalog above.
        </div>
      ) : (
        <ImageTable
          images={diskImages}
          kind="disk"
          onRetry={retry}
          onRemove={remove}
          clustered={dir.clustered}
          onCopy={setCopying}
          onRemoveCopy={removeCopy}
          onExtractKernel={(img) => void extractKernel(img)}
          hasKernel={(img) => kernelImages.some((k) => k.for_image === img.id)}
        />
      )}

      <h2 className="text-lg font-semibold text-gray-800 mt-8 mb-3">Firmware</h2>
      {images === null ? (
        <LoadingCard />
      ) : firmwareImages.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">
          No firmware yet. VMs booting cloud images need one: pull it from the firmware catalog above.
        </div>
      ) : (
        <ImageTable images={firmwareImages} kind="firmware" onRetry={retry} onRemove={remove} clustered={dir.clustered} onCopy={setCopying} onRemoveCopy={removeCopy} />
      )}

      {kernelImages.length > 0 && (
        <>
          <h2 className="text-lg font-semibold text-gray-800 mt-8 mb-3">Kernels</h2>
          <ImageTable images={kernelImages} kind="kernel" onRetry={retry} onRemove={remove} clustered={dir.clustered} onCopy={setCopying} onRemoveCopy={removeCopy} />
        </>
      )}

      {pulling && (
        <PullDialog
          what={pulling}
          arch={arch || selfArch}
          nodes={dir.nodes}
          onClose={() => setPulling(null)}
          onDone={() => {
            setPulling(null);
            refresh();
          }}
        />
      )}
      {copying && (
        <CopyDialog
          img={copying}
          nodes={dir.nodes}
          onClose={() => setCopying(null)}
          onDone={() => {
            setCopying(null);
            refresh();
          }}
        />
      )}
      {pullingUrl && (
        <Modal title="Download from URL" onClose={() => setPullingUrl(false)}>
          <PullUrlForm
            onCancel={() => setPullingUrl(false)}
            onDone={() => {
              setPullingUrl(false);
              refresh();
            }}
          />
        </Modal>
      )}
    </div>
  );
}
