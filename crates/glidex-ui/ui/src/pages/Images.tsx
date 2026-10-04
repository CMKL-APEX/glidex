import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import type { CatalogItem, FirmwareCatalogItem, HypervisorType, ImageInfo, ImageKind } from "../types";
import { HYPERVISOR_LABELS, formatBytes } from "../types";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";
import { useLiveRefresh } from "../live";

const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";

function StatusBadge({ image }: { image: ImageInfo }) {
  const s = image.status;
  if (image.deleting) return <span className="text-xs text-gray-500">Deleting…</span>;
  switch (s.state) {
    case "downloading": {
      const pct = s.total_bytes ? Math.floor((s.received_bytes * 100) / s.total_bytes) : null;
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
    case "ready":
      return <span className="text-xs text-green-700">Ready</span>;
    case "missing":
      return <span className="text-xs text-red-700">File missing</span>;
    case "failed":
      return (
        <span className="text-xs text-red-700" title={s.reason}>
          Failed: {s.reason.length > 60 ? `${s.reason.slice(0, 60)}…` : s.reason}
        </span>
      );
  }
}

function PullUrlForm({ onDone, onCancel }: { onDone: () => void; onCancel: () => void }) {
  const [url, setUrl] = useState("");
  const [sha256, setSha256] = useState("");
  const [name, setName] = useState("");
  const [kind, setKind] = useState<ImageKind>("disk");
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
        ...(kind === "firmware" ? { kind: "firmware" as const, hypervisor } : {}),
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
          <select className={`${inputClass} bg-white`} value={kind} onChange={(e) => setKind(e.target.value as ImageKind)}>
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
  const s = img.source;
  return (
    <>
      {s.kind === "url" ? (
        <span className="break-all">{s.url}</span>
      ) : (
        <span>
          {s.key} <span className="text-gray-400">{s.version}</span>
          {s.kind === "firmware" && s.url.startsWith("/") && <span className="block text-gray-400 break-all">{s.url}</span>}
        </span>
      )}
      {!img.verified && <span className="ml-1 text-amber-700">(unverified)</span>}
    </>
  );
}

interface TableProps {
  images: ImageInfo[];
  firmware: boolean;
  onRetry: (img: ImageInfo) => void;
  onRemove: (img: ImageInfo) => void;
}

function ImageTable({ images, firmware, onRetry, onRemove }: TableProps) {
  return (
    <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-x-auto">
      <table className="w-full text-sm" data-testid={firmware ? "firmware-images" : "disk-images"}>
        <thead className="bg-gray-50 text-gray-600 text-left">
          <tr>
            <th className="px-4 py-3 font-medium">Name</th>
            {firmware && <th className="px-4 py-3 font-medium">Hypervisor</th>}
            <th className="px-4 py-3 font-medium">Status</th>
            <th className="px-4 py-3 font-medium">Size</th>
            <th className="px-4 py-3 font-medium">Source</th>
            <th className="px-4 py-3 font-medium">{firmware ? "Used by VMs" : "Linked disks"}</th>
            <th className="px-4 py-3" />
          </tr>
        </thead>
        <tbody className="divide-y divide-gray-100">
          {images.map((img) => {
            const users = firmware ? img.used_by_vms ?? [] : img.linked_disks;
            return (
              <tr key={img.id}>
                <td className="px-4 py-3 font-mono">{img.name}</td>
                {firmware && (
                  <td className="px-4 py-3">
                    {img.hypervisor ? HYPERVISOR_LABELS[img.hypervisor] : "—"}
                    {img.hypervisor === "qemu" && (
                      <span className="block text-xs text-gray-500">
                        {img.vars_template ? "with variable store" : "single file (no saved UEFI settings)"}
                      </span>
                    )}
                  </td>
                )}
                <td className="px-4 py-3">
                  <StatusBadge image={img} />
                </td>
                <td className="px-4 py-3">
                  {firmware
                    ? img.file_size_bytes
                      ? formatBytes(img.file_size_bytes)
                      : "—"
                    : img.virtual_size_bytes
                      ? formatBytes(img.virtual_size_bytes)
                      : "—"}
                </td>
                <td className="px-4 py-3 text-xs text-gray-600">
                  <SourceCell img={img} />
                </td>
                <td className="px-4 py-3 text-xs font-mono">{users.join(", ") || "—"}</td>
                <td className="px-4 py-3 text-right space-x-2 whitespace-nowrap">
                  {img.status.state === "failed" && (
                    <button
                      className="px-3 py-1 text-xs font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg"
                      onClick={() => onRetry(img)}
                    >
                      Retry
                    </button>
                  )}
                  <button
                    className="px-3 py-1 text-xs font-medium text-red-700 bg-red-50 hover:bg-red-100 rounded-lg disabled:opacity-50"
                    disabled={users.length > 0 && firmware}
                    title={users.length > 0 && firmware ? "VMs boot through it" : undefined}
                    onClick={() => onRemove(img)}
                  >
                    {img.status.state === "downloading" ? "Cancel" : "Delete"}
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

export default function Images() {
  const [catalog, setCatalog] = useState<CatalogItem[]>([]);
  const [firmwareCatalog, setFirmwareCatalog] = useState<FirmwareCatalogItem[]>([]);
  const [images, setImages] = useState<ImageInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pullingUrl, setPullingUrl] = useState(false);

  const refresh = useCallback(() => {
    api.imageCatalog().then(setCatalog).catch(() => setCatalog([]));
    api.firmwareCatalog().then(setFirmwareCatalog).catch(() => setFirmwareCatalog([]));
    api
      .listImages()
      .then(setImages)
      .catch((e) => setError(e instanceof Error ? e.message : "Failed to load images"));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Follow the live stream; without it, poll while anything is downloading.
  const live = useLiveRefresh(["image", "disk", "vm"], refresh);
  const busy = images?.some((i) => i.status.state === "downloading" || i.status.state === "verifying" || i.deleting);
  useEffect(() => {
    if (!busy || live) return;
    const id = setInterval(refresh, 1500);
    return () => clearInterval(id);
  }, [busy, live, refresh]);

  const pull = async (req: { catalog?: string; firmware?: string }) => {
    setError(null);
    try {
      await api.pullImage(req);
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to start download");
    }
  };

  const remove = async (img: ImageInfo) => {
    const what = img.status.state === "downloading" ? "Cancel the download of" : img.kind === "firmware" ? "Delete firmware" : "Delete image";
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

  const inFlight = (kind: "catalog" | "firmware", key: string) =>
    images?.some(
      (i) => i.source.kind === kind && i.source.key === key && (i.status.state === "downloading" || i.status.state === "verifying"),
    );

  const diskImages = images?.filter((i) => i.kind !== "firmware") ?? [];
  const firmwareImages = images?.filter((i) => i.kind === "firmware") ?? [];

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

      <h2 className="text-lg font-semibold text-gray-800 mb-3">Cloud image catalog</h2>
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
              <button className={pullClass} disabled={inFlight("catalog", c.key)} onClick={() => pull({ catalog: c.key })}>
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
                <span className="font-mono">{f.key}</span> · {HYPERVISOR_LABELS[f.hypervisor]}
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
                onClick={() => pull({ firmware: f.key })}
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
        <ImageTable images={diskImages} firmware={false} onRetry={retry} onRemove={remove} />
      )}

      <h2 className="text-lg font-semibold text-gray-800 mt-8 mb-3">Firmware</h2>
      {images === null ? (
        <LoadingCard />
      ) : firmwareImages.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">
          No firmware yet. VMs booting cloud images need one: pull it from the firmware catalog above.
        </div>
      ) : (
        <ImageTable images={firmwareImages} firmware onRetry={retry} onRemove={remove} />
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
