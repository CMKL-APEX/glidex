import { useCallback, useEffect, useState, type FormEvent } from "react";
import * as api from "../api";
import { ApiRequestError } from "../api";
import { useCan, useSession } from "../session";
import type { DiskInfo, ImageInfo, VmResponse } from "../types";
import { formatBytes, notReady } from "../types";
import Modal from "../components/Modal";
import { LoadingCard } from "../components/Loading";

const inputClass =
  "mt-1 w-full px-3 py-2 border border-gray-300 rounded-lg focus:ring-2 focus:ring-sky-500 focus:border-transparent";

const GIB = 1024 * 1024 * 1024;

function errorText(e: unknown): string {
  if (e instanceof ApiRequestError && e.details?.min_size_bytes) {
    return `${e.message} (minimum ${formatBytes(e.details.min_size_bytes)})`;
  }
  return e instanceof Error ? e.message : String(e);
}

function resultNotes(d: DiskInfo): string[] {
  const notes = [...(d.warnings ?? [])];
  const waiting = notReady(d.conditions);
  if (d.deleting) notes.unshift(`${d.name} is deleted once its current operation finishes.`);
  else if (waiting) notes.unshift(`${d.name}: ${waiting.message || waiting.reason}`);
  if (d.pending_growpart) notes.push("The root partition will be grown by the guest on next boot.");
  return notes;
}

/** Still changing: the list is polled until none is. */
function settling(d: DiskInfo): boolean {
  return (
    d.status === "pending" || d.status === "creating" || d.status === "resizing" || d.status === "busy" ||
    d.pending_size_bytes !== undefined || !!d.deleting
  );
}

function DiskStatus({ d }: { d: DiskInfo }) {
  const why = notReady(d.conditions);
  const color = d.status === "failed" || d.status === "missing" ? "text-red-700" : d.status === "ready" ? "" : "text-sky-700";
  return (
    <div title={why ? `${why.reason}: ${why.message}` : undefined}>
      <span className={color}>
        {d.deleting ? "deleting" : d.status === "busy" ? `busy (${d.busy_op})` : d.status}
      </span>
      {d.pending_size_bytes !== undefined && (
        <span className="ml-1 text-sky-700">· {formatBytes(d.pending_size_bytes)} once its VM stops</span>
      )}
      {d.pending_growpart && <span className="ml-1 text-sky-700">· grows on boot</span>}
      {why && d.status !== "ready" && <div className="text-gray-500 truncate max-w-xs">{why.message || why.reason}</div>}
    </div>
  );
}

function CreateDiskForm({
  images,
  onDone,
  onCancel,
}: {
  images: ImageInfo[];
  onDone: (d: DiskInfo) => void;
  onCancel: () => void;
}) {
  const { project } = useSession();
  const ready = images.filter((i) => i.status.state === "ready");
  const [name, setName] = useState("");
  const [image, setImage] = useState(ready[0]?.id ?? "");
  const [size, setSize] = useState("");
  const [clone, setClone] = useState<"linked" | "full">("linked");
  const [format, setFormat] = useState<"qcow2" | "raw">("qcow2");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      const d = await api.createDisk({
        project: project ?? undefined,
        name,
        image: image || undefined,
        size_gib: size ? Number(size) : undefined,
        clone: image ? clone : undefined,
        format,
      });
      onDone(d);
    } catch (err) {
      setError(errorText(err));
      setSubmitting(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <div>
        <label className="block text-sm font-medium text-gray-700">Name</label>
        <input className={inputClass} required pattern="[A-Za-z0-9_\-][A-Za-z0-9._\-]{0,63}" value={name} onChange={(e) => setName(e.target.value)} />
      </div>
      <div>
        <label className="block text-sm font-medium text-gray-700">Contents</label>
        <select className={`${inputClass} bg-white`} value={image} onChange={(e) => setImage(e.target.value)}>
          <option value="">Blank disk</option>
          {ready.map((i) => (
            <option key={i.id} value={i.id}>
              From image {i.name} ({formatBytes(i.virtual_size_bytes)})
            </option>
          ))}
        </select>
      </div>
      <div className="grid grid-cols-2 gap-4">
        <div>
          <label className="block text-sm font-medium text-gray-700">Size (GiB)</label>
          <input
            type="number"
            min="1"
            className={inputClass}
            required={!image}
            placeholder={image ? "default: 10" : ""}
            value={size}
            onChange={(e) => setSize(e.target.value)}
          />
        </div>
        <div>
          <label className="block text-sm font-medium text-gray-700">Format</label>
          <select
            className={`${inputClass} bg-white`}
            value={format}
            onChange={(e) => {
              const f = e.target.value as "qcow2" | "raw";
              setFormat(f);
              if (f === "raw") setClone("full");
            }}
          >
            <option value="qcow2">qcow2</option>
            <option value="raw">raw</option>
          </select>
        </div>
      </div>
      {image && (
        <div>
          <label className="block text-sm font-medium text-gray-700">Clone</label>
          <select className={`${inputClass} bg-white`} value={clone} onChange={(e) => setClone(e.target.value as "linked" | "full")}>
            <option value="linked" disabled={format === "raw"}>
              Linked (thin overlay; the image must stay)
            </option>
            <option value="full">Full copy</option>
          </select>
          <p className="mt-1 text-xs text-gray-500">The root partition is extended to fill the disk.</p>
        </div>
      )}
      <div className="flex justify-end space-x-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" disabled={submitting} className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50">
          {submitting ? "Creating…" : "Create disk"}
        </button>
      </div>
    </form>
  );
}

function ResizeForm({ disk, onDone, onCancel }: { disk: DiskInfo; onDone: (d: DiskInfo) => void; onCancel: () => void }) {
  const [size, setSize] = useState(String(Math.ceil(disk.size_bytes / GIB)));
  const [extend, setExtend] = useState(disk.origin.kind === "image");
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const shrinking = Number(size) * GIB < disk.size_bytes;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      onDone(await api.resizeDisk(disk.id, Number(size), shrinking ? undefined : extend));
    } catch (err) {
      setError(errorText(err));
      setSubmitting(false);
    }
  };

  return (
    <form onSubmit={submit} className="space-y-4">
      {error && <p className="text-sm text-red-600">{error}</p>}
      <p className="text-sm text-gray-600">Current size: {formatBytes(disk.size_bytes)}</p>
      <div>
        <label className="block text-sm font-medium text-gray-700">New size (GiB)</label>
        <input type="number" min="1" className={inputClass} value={size} onChange={(e) => setSize(e.target.value)} />
      </div>
      {shrinking ? (
        <p className="text-xs text-amber-700">
          Shrinking only gives back unpartitioned space at the end of the disk; glidex never shrinks filesystems.
        </p>
      ) : (
        <label className="flex items-center space-x-2 text-sm">
          <input type="checkbox" checked={extend} onChange={(e) => setExtend(e.target.checked)} />
          <span>Extend the root partition into the new space</span>
        </label>
      )}
      <div className="flex justify-end space-x-3 pt-2">
        <button type="button" className="px-4 py-2 text-sm font-medium text-gray-700 bg-gray-200 hover:bg-gray-300 rounded-lg" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" disabled={submitting} className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50">
          {submitting ? "Resizing…" : "Resize"}
        </button>
      </div>
    </form>
  );
}

export default function Disks() {
  const { project } = useSession();
  const [canCreate] = useCan(project ? [{ action: "createDisk", resource: { type: "Project", id: project } }] : []) ?? [];
  const [disks, setDisks] = useState<DiskInfo[] | null>(null);
  const [images, setImages] = useState<ImageInfo[]>([]);
  const [vms, setVms] = useState<VmResponse[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notes, setNotes] = useState<string[]>([]);
  const [creating, setCreating] = useState(false);
  const [resizing, setResizing] = useState<DiskInfo | null>(null);
  const [detail, setDetail] = useState<DiskInfo | null>(null);

  const refresh = useCallback(() => {
    api.listDisks(project).then(setDisks).catch((e) => setError(errorText(e)));
    api.listImages().then(setImages).catch(() => setImages([]));
    api.listVms(project).then(setVms).catch(() => setVms([]));
  }, [project]);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Poll while the disk controller is still working on one.
  const busy = disks?.some(settling) ?? false;
  useEffect(() => {
    if (!busy) return;
    const id = setInterval(refresh, 2000);
    return () => clearInterval(id);
  }, [busy, refresh]);

  const done = (d: DiskInfo) => {
    setNotes(resultNotes(d));
    setCreating(false);
    setResizing(null);
    refresh();
  };

  const vmName = (id?: string | null) => (id ? vms.find((v) => v.id === id)?.name ?? id : null);
  const imageName = (id: string) => images.find((i) => i.id === id)?.name ?? id.slice(0, 8);

  const act = async (f: () => Promise<DiskInfo | void>) => {
    setError(null);
    setNotes([]);
    try {
      const d = await f();
      if (d) setNotes(resultNotes(d));
      refresh();
    } catch (e) {
      setError(errorText(e));
    }
  };

  return (
    <div>
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-gray-900">Disks</h1>
          <p className="text-sm text-gray-500 mt-1">
            Writable volumes VMs boot from or attach. A resize or extend-root on a disk a running VM uses is applied once
            the VM stops.
          </p>
        </div>
        {canCreate && (
          <button className="px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg" onClick={() => setCreating(true)}>
            Create Disk
          </button>
        )}
      </div>

      {error && <div className="mb-4 p-4 bg-red-50 border border-red-200 rounded-lg text-red-700">{error}</div>}
      {notes.length > 0 && (
        <div className="mb-4 p-4 bg-sky-50 border border-sky-200 rounded-lg text-sky-800 text-sm space-y-1">
          {notes.map((n) => (
            <div key={n}>{n}</div>
          ))}
        </div>
      )}

      {disks === null ? (
        <LoadingCard />
      ) : disks.length === 0 ? (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 p-8 text-center text-gray-500">
          No disks yet. Create one, or create a VM from an image.
        </div>
      ) : (
        <div className="bg-white rounded-xl shadow-sm border border-gray-200 overflow-hidden">
          <table className="w-full text-sm">
            <thead className="bg-gray-50 text-gray-600 text-left">
              <tr>
                <th className="px-4 py-3 font-medium">Name</th>
                <th className="px-4 py-3 font-medium">Size</th>
                <th className="px-4 py-3 font-medium">Format</th>
                <th className="px-4 py-3 font-medium">Origin</th>
                <th className="px-4 py-3 font-medium">Attached to</th>
                <th className="px-4 py-3 font-medium">Status</th>
                <th className="px-4 py-3" />
              </tr>
            </thead>
            <tbody className="divide-y divide-gray-100">
              {disks.map((d) => (
                <tr key={d.id}>
                  <td className="px-4 py-3 font-mono">
                    <button className="hover:underline" onClick={() => api.getDisk(d.id).then(setDetail).catch((e) => setError(errorText(e)))}>
                      {d.name}
                    </button>
                  </td>
                  <td className="px-4 py-3">{formatBytes(d.size_bytes)}</td>
                  <td className="px-4 py-3">{d.format}</td>
                  <td className="px-4 py-3 text-xs">
                    {d.origin.kind === "image" ? `${imageName(d.origin.image_id)} (${d.origin.mode})` : "blank"}
                  </td>
                  <td className="px-4 py-3 font-mono text-xs">{vmName(d.attached_to) ?? "—"}</td>
                  <td className="px-4 py-3 text-xs">
                    <DiskStatus d={d} />
                  </td>
                  <td className="px-4 py-3 text-right space-x-2 whitespace-nowrap">
                    <button
                      className="px-3 py-1 text-xs font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg disabled:opacity-40"
                      disabled={d.phase !== "ready" || !!d.deleting}
                      onClick={() => setResizing(d)}
                    >
                      Resize
                    </button>
                    {d.origin.kind === "image" && (
                      <button
                        className="px-3 py-1 text-xs font-medium text-gray-700 bg-gray-100 hover:bg-gray-200 rounded-lg disabled:opacity-40"
                        disabled={d.phase !== "ready" || !!d.deleting}
                        onClick={() => act(() => api.extendRoot(d.id, "offline"))}
                      >
                        Extend root
                      </button>
                    )}
                    <button
                      className="px-3 py-1 text-xs font-medium text-red-700 bg-red-50 hover:bg-red-100 rounded-lg disabled:opacity-40"
                      disabled={!!d.attached_to || !!d.owner || !!d.deleting}
                      title={d.attached_to || d.owner ? "Detach it or delete the VM first" : ""}
                      onClick={() => confirm(`Delete disk ${d.name} and its data?`) && act(() => api.deleteDisk(d.id))}
                    >
                      Delete
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {creating && (
        <Modal title="Create Disk" onClose={() => setCreating(false)}>
          <CreateDiskForm images={images} onCancel={() => setCreating(false)} onDone={done} />
        </Modal>
      )}
      {resizing && (
        <Modal title={`Resize ${resizing.name}`} onClose={() => setResizing(null)}>
          <ResizeForm disk={resizing} onCancel={() => setResizing(null)} onDone={done} />
        </Modal>
      )}
      {detail && (
        <Modal title={detail.name} onClose={() => setDetail(null)}>
          <div className="text-sm space-y-2">
            <div className="font-mono text-xs break-all text-gray-600">{detail.path}</div>
            {detail.partition_table ? (
              <table className="w-full text-xs">
                <thead className="text-gray-500 text-left">
                  <tr>
                    <th className="py-1">#</th>
                    <th>Start</th>
                    <th>Size</th>
                    <th>Type</th>
                  </tr>
                </thead>
                <tbody>
                  {detail.partition_table.partitions.map((p) => (
                    <tr key={p.number} className={p.is_root ? "font-semibold" : ""}>
                      <td className="py-1">{p.number}</td>
                      <td>{formatBytes(p.start_bytes)}</td>
                      <td>{formatBytes(p.size_bytes)}</td>
                      <td className="font-mono">
                        {p.type}
                        {p.is_root && " (root)"}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            ) : (
              <p className="text-gray-500">No partition table.</p>
            )}
            {detail.partition_table && (
              <p className="text-gray-600">
                {detail.partition_table.kind.toUpperCase()}, free at end: {formatBytes(detail.partition_table.free_tail_bytes)}
              </p>
            )}
          </div>
        </Modal>
      )}
    </div>
  );
}
