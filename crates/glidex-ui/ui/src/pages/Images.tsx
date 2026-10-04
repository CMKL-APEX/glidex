import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import type { CatalogItem, ImageInfo } from "../types";
import { formatBytes } from "../types";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";

const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";

function StatusBadge({ image }: { image: ImageInfo }) {
  const s = image.status;
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
  const [error, setError] = useState<string | null>(null);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setError(null);
    try {
      await api.pullImage({ url, sha256: sha256 || undefined, name: name || undefined });
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to start download");
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <div>
        <label className="block text-sm font-medium text-gray-700">Image URL (https)</label>
        <input className={inputClass} required placeholder="https://…/image.qcow2" value={url} onChange={(e) => setUrl(e.target.value)} />
      </div>
      <div>
        <label className="block text-sm font-medium text-gray-700">sha256 (optional)</label>
        <input className={`${inputClass} font-mono text-xs`} value={sha256} onChange={(e) => setSha256(e.target.value)} />
        <p className="mt-1 text-xs text-gray-500">Without it the image is stored but marked unverified.</p>
      </div>
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

export default function Images() {
  const [catalog, setCatalog] = useState<CatalogItem[]>([]);
  const [images, setImages] = useState<ImageInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pullingUrl, setPullingUrl] = useState(false);

  const refresh = useCallback(() => {
    api.imageCatalog().then(setCatalog).catch(() => setCatalog([]));
    api
      .listImages()
      .then(setImages)
      .catch((e) => setError(e instanceof Error ? e.message : "Failed to load images"));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Poll while anything is downloading.
  const busy = images?.some((i) => i.status.state === "downloading" || i.status.state === "verifying");
  useEffect(() => {
    if (!busy) return;
    const id = setInterval(refresh, 1500);
    return () => clearInterval(id);
  }, [busy, refresh]);

  const pull = async (key: string) => {
    setError(null);
    try {
      await api.pullImage({ catalog: key });
      refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to start download");
    }
  };

  const remove = async (img: ImageInfo) => {
    const what = img.status.state === "downloading" ? "Cancel the download of" : "Delete image";
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
    } catch (e) {
      setError(e instanceof Error ? e.message : "Failed to retry the download");
    }
  };

  const inFlight = (key: string) =>
    images?.some(
      (i) => i.source.kind === "catalog" && i.source.key === key && (i.status.state === "downloading" || i.status.state === "verifying"),
    );

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">Images</h1>
          <p className="text-sm text-gray-500 mt-1">
            Base cloud images, downloaded and checked against the vendor's published checksum. Disks are created from these.
          </p>
        </div>
        <button className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg" onClick={() => setPullingUrl(true)}>
          Download from URL
        </button>
      </div>

      {error && <div className="mb-4 p-4 bg-red-50 border border-red-200 rounded-lg text-red-700">{error}</div>}

      <h2 className="text-lg font-semibold text-gray-800 mb-3">Catalog</h2>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3 mb-8">
        {catalog.map((c) => (
          <div key={c.key} className="bg-white rounded-xl shadow-sm border border-gray-200 p-4 flex items-center justify-between">
            <div>
              <div className="font-medium text-gray-900">
                {c.distro} {c.release}
              </div>
              <div className="text-xs font-mono text-gray-500">{c.key}</div>
            </div>
            {c.downloaded_image_id ? (
              <span className="text-xs text-green-700">Downloaded</span>
            ) : (
              <button
                className="px-3 py-1 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-lg disabled:opacity-50"
                disabled={inFlight(c.key)}
                onClick={() => pull(c.key)}
              >
                {inFlight(c.key) ? "Downloading…" : "Pull"}
              </button>
            )}
          </div>
        ))}
      </div>

      <h2 className="text-lg font-semibold text-gray-800 mb-3">Downloaded</h2>
      {images === null ? (
        <LoadingCard />
      ) : images.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">
          No images yet. Pull one from the catalog above.
        </div>
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-hidden">
          <table className="w-full text-sm">
            <thead className="bg-gray-50 text-gray-600 text-left">
              <tr>
                <th className="px-4 py-3 font-medium">Name</th>
                <th className="px-4 py-3 font-medium">Status</th>
                <th className="px-4 py-3 font-medium">Size</th>
                <th className="px-4 py-3 font-medium">Source</th>
                <th className="px-4 py-3 font-medium">Linked disks</th>
                <th className="px-4 py-3" />
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {images.map((img) => (
                <tr key={img.id}>
                  <td className="px-4 py-3 font-mono">{img.name}</td>
                  <td className="px-4 py-3">
                    <StatusBadge image={img} />
                  </td>
                  <td className="px-4 py-3">{img.virtual_size_bytes ? formatBytes(img.virtual_size_bytes) : "—"}</td>
                  <td className="px-4 py-3 text-xs text-gray-600">
                    {img.source.kind === "catalog" ? (
                      <span>
                        {img.source.key} <span className="text-gray-400">{img.source.version}</span>
                      </span>
                    ) : (
                      <span className="break-all">{img.source.url}</span>
                    )}
                    {!img.verified && <span className="ml-1 text-amber-700">(unverified)</span>}
                  </td>
                  <td className="px-4 py-3 text-xs font-mono">{img.linked_disks.join(", ") || "—"}</td>
                  <td className="px-4 py-3 text-right space-x-2 whitespace-nowrap">
                    {img.status.state === "failed" && (
                      <button
                        className="px-3 py-1 text-xs font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg"
                        onClick={() => retry(img)}
                      >
                        Retry
                      </button>
                    )}
                    <button
                      className="px-3 py-1 text-xs font-medium text-red-700 bg-red-50 hover:bg-red-100 rounded-lg"
                      onClick={() => remove(img)}
                    >
                      {img.status.state === "downloading" ? "Cancel" : "Delete"}
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {pullingUrl && (
        <Modal title="Download image from URL" onClose={() => setPullingUrl(false)}>
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
