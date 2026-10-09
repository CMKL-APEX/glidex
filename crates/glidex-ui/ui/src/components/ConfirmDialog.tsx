import { useState, type ReactNode } from "react";
import Modal from "./Modal";
import { ErrorBanner, errorMessage, inputClass, secondaryButton } from "./ui";

/** A confirmation for an operation `gxctl` guards with `--force` or a typed
 * confirmation (spec/clustering-ui.md §3.4): the consequence, then the
 * name typed back before the destructive button works. A refusal stays in
 * the dialog with the server's message; a step-up login is asked for by the
 * API client and the request retried. */
export default function ConfirmDialog({
  title,
  consequence,
  typeToConfirm,
  actionLabel,
  danger = true,
  onConfirm,
  onClose,
  children,
}: {
  title: string;
  consequence: ReactNode;
  /** The text to type back (a node's or cluster's name); none: a plain confirm. */
  typeToConfirm?: string;
  actionLabel: string;
  danger?: boolean;
  /** Resolves when done (the dialog closes); throws to show the error. */
  onConfirm: () => Promise<void>;
  onClose: () => void;
  /** Extra fields (options) between the consequence and the confirmation. */
  children?: ReactNode;
}) {
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const ready = !typeToConfirm || typed.trim() === typeToConfirm;

  const go = async () => {
    setBusy(true);
    setError(null);
    try {
      await onConfirm();
      onClose();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const button = danger
    ? "px-4 py-2 text-sm font-medium text-white bg-red-600 hover:bg-red-700 rounded-lg disabled:opacity-50"
    : "px-4 py-2 text-sm font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg disabled:opacity-50";

  return (
    <Modal title={title} onClose={busy ? () => {} : onClose}>
      <div className="space-y-4 text-sm text-gray-700">
        <div>{consequence}</div>
        {children}
        {typeToConfirm && (
          <div>
            <label htmlFor="confirm-name" className="block text-sm text-gray-600 mb-1">
              Type <span className="font-mono font-semibold">{typeToConfirm}</span> to confirm
            </label>
            <input
              id="confirm-name"
              className={`${inputClass} font-mono`}
              autoComplete="off"
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
            />
          </div>
        )}
        <ErrorBanner error={error} onDismiss={() => setError(null)} />
        <div className="flex justify-end gap-2">
          <button className={secondaryButton} onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className={button} onClick={go} disabled={!ready || busy}>
            {busy ? "Working..." : actionLabel}
          </button>
        </div>
      </div>
    </Modal>
  );
}
