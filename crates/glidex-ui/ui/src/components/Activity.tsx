import { useCallback, useEffect, useRef, useState } from "react";
import { Link } from "react-router-dom";
import * as api from "../api";
import { diskActivity, diskWaiting, imageActivity, vmActivity } from "../types";
import { useSession } from "../session";

/** One object a controller is still working on. */
interface Item {
  key: string;
  kind: "VM" | "Disk" | "Image";
  name: string;
  what: string;
  to: string;
  /** Waits on something outside the control plane: no fast polling. */
  waiting?: boolean;
}

/** Faster while something is in progress. */
const BUSY_MS = 3000;
const IDLE_MS = 15000;

/** What the controllers are still doing in the selected project (spec/
 * reconciliation.md: writes are carried out asynchronously). `null` until
 * the first look. */
export function useActivity(): Item[] | null {
  const { project } = useSession();
  const [items, setItems] = useState<Item[] | null>(null);
  const timer = useRef<number | undefined>(undefined);

  const look = useCallback(async () => {
    const [vms, disks, images] = await Promise.all([
      api.listVms(project).catch(() => []),
      api.listDisks(project).catch(() => []),
      api.listImages().catch(() => []),
    ]);
    const found: Item[] = [];
    for (const vm of vms) {
      const what = vmActivity(vm);
      if (what) found.push({ key: `vm/${vm.id}`, kind: "VM", name: vm.name, what, to: `/vms/${vm.id}` });
    }
    for (const d of disks) {
      const what = diskActivity(d);
      if (what) found.push({ key: `disk/${d.id}`, kind: "Disk", name: d.name, what, to: "/disks", waiting: diskWaiting(d) });
    }
    for (const i of images) {
      const what = imageActivity(i);
      if (what) found.push({ key: `image/${i.id}`, kind: "Image", name: i.name, what, to: "/images" });
    }
    return found;
  }, [project]);

  useEffect(() => {
    let live = true;
    const run = async () => {
      window.clearTimeout(timer.current);
      const found = await look();
      if (!live) return;
      setItems(found);
      timer.current = window.setTimeout(run, found.some((i) => !i.waiting) ? BUSY_MS : IDLE_MS);
    };
    // A write just happened: look again shortly (the controller picks it
    // up within a moment).
    const kick = () => {
      window.clearTimeout(timer.current);
      timer.current = window.setTimeout(run, 500);
    };
    run();
    window.addEventListener(api.CHANGED_EVENT, kick);
    return () => {
      live = false;
      window.clearTimeout(timer.current);
      window.removeEventListener(api.CHANGED_EVENT, kick);
    };
  }, [look]);

  return items;
}

/** The header's "N in progress" indicator, with the list on click. */
export default function Activity() {
  const items = useActivity();
  const [open, setOpen] = useState(false);
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (box.current && !box.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    return () => document.removeEventListener("mousedown", close);
  }, [open]);

  if (!items || items.length === 0) {
    return (
      <span className="text-xs text-gray-400" title="No reconciliation in progress">
        Up to date
      </span>
    );
  }
  return (
    <div ref={box} className="relative">
      <button
        className="flex items-center gap-1.5 px-2 py-1 text-xs font-medium text-sky-700 bg-sky-50 hover:bg-sky-100 rounded-full"
        aria-label="Reconciling"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        <Spinner />
        {items.length} in progress
      </button>
      {open && (
        <div className="absolute right-0 z-20 mt-2 w-80 bg-white border border-gray-200 rounded-lg shadow-lg p-2 text-sm">
          <ul className="max-h-80 overflow-auto divide-y divide-gray-100">
            {items.map((it) => (
              <li key={it.key}>
                <Link to={it.to} className="flex items-center gap-2 px-2 py-1.5 hover:bg-gray-50 rounded" onClick={() => setOpen(false)}>
                  <span className="w-10 shrink-0 text-xs text-gray-400">{it.kind}</span>
                  <span className="font-mono truncate">{it.name}</span>
                  <span className="ml-auto shrink-0 text-xs text-sky-700">{it.what}</span>
                </Link>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

/** A small inline spinner for "in progress". */
export function Spinner({ className = "" }: { className?: string }) {
  return (
    <span
      className={`inline-block w-3 h-3 border-2 border-current border-t-transparent rounded-full animate-spin ${className}`}
      aria-hidden="true"
    />
  );
}
