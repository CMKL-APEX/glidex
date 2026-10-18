import { useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import * as api from "../api";
import { nodeUnreachable } from "../types";
import {
  type ConsoleTerminal,
  type TerminalKind,
  TERMINAL_KINDS,
  openTerminal,
  saveTerminalKind,
  savedTerminalKind,
} from "./consoleTerminal";

/** When the console fails: its node may be the reason (spec/clustering-ui.md §4). */
async function whyFailed(id: string, fallback: string): Promise<string> {
  const vm = await api.getVm(id).catch(() => null);
  if (vm && nodeUnreachable(vm)) {
    return `Its node${vm.node_name ? `, ${vm.node_name},` : ""} stopped reporting: the console can't be reached until the node is back. The VM may still be running.`;
  }
  return fallback;
}

type Status = "connecting" | "connected" | "closed" | "error";

export default function VmConsole() {
  const { id } = useParams<{ id: string }>();
  const containerRef = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<Status>("connecting");
  const [error, setError] = useState<string | null>(null);
  // Bumped by Reconnect: the VM outlives the connection (a restart of the
  // VM or of the control plane closes it), so connecting again picks up.
  const [attempt, setAttempt] = useState(0);
  const [kind, setKind] = useState<TerminalKind>(savedTerminalKind);

  useEffect(() => {
    const container = containerRef.current;
    if (!id || !container) return;
    setStatus("connecting");
    setError(null);

    let term: ConsoleTerminal | null = null;
    let ws: WebSocket | null = null;
    let inputDisposable: { dispose(): void } | null = null;
    let disposed = false;
    const encoder = new TextEncoder();

    const handleResize = () => {
      try {
        term?.fit();
      } catch {
        /* container not laid out yet */
      }
    };
    window.addEventListener("resize", handleResize);

    const fail = (e: unknown) => {
      if (disposed) return;
      setStatus("error");
      whyFailed(id, e instanceof Error ? e.message : String(e)).then((m) => !disposed && setError(m));
    };

    openTerminal(kind, container)
      .then((t) => {
        if (disposed) {
          t.dispose();
          return;
        }
        term = t;
        // The terminal's text, for the e2e suite: ghostty-web draws on a
        // canvas. (Nothing new to a script on the page: xterm.js keeps
        // the same text in the DOM.)
        Object.defineProperty(container, "screenText", { value: () => t.screenText(), configurable: true });
        inputDisposable = t.onData((data) => {
          if (ws?.readyState === WebSocket.OPEN) {
            ws.send(encoder.encode(data));
          }
        });
        // Browsers open the console with a fresh single-use ticket
        // (spec/security.md §5.6): POST for it, then connect at once.
        return api.consoleTicket(id).then(({ ticket }) => {
          if (disposed) return;
          const wsProto = window.location.protocol === "https:" ? "wss:" : "ws:";
          const wsUrl = `${wsProto}//${window.location.host}/api/vms/${encodeURIComponent(id)}/console/ws?ticket=${encodeURIComponent(ticket)}`;
          const sock = new WebSocket(wsUrl);
          sock.binaryType = "arraybuffer";
          ws = sock;

          sock.onopen = () => {
            setStatus("connected");
            setError(null);
          };
          sock.onclose = (ev) => {
            setStatus("closed");
            if (ev.code !== 1000 && ev.code !== 1005) {
              whyFailed(id, `WebSocket closed (code ${ev.code})`).then((m) => !disposed && setError(m));
            }
          };
          sock.onerror = () => {
            setStatus("error");
            whyFailed(id, "WebSocket connection error").then((m) => !disposed && setError(m));
          };
          sock.onmessage = (ev) => {
            if (typeof ev.data === "string") {
              t.write(ev.data);
            } else {
              t.write(new Uint8Array(ev.data as ArrayBuffer));
            }
          };
        });
      })
      .catch(fail);

    return () => {
      disposed = true;
      window.removeEventListener("resize", handleResize);
      inputDisposable?.dispose();
      // Detach first: a message already in flight would otherwise be
      // written to the disposed terminal (xterm throws on "dimensions").
      if (ws) {
        ws.onopen = ws.onclose = ws.onerror = ws.onmessage = null;
        try {
          ws.close();
        } catch {
          /* already closed */
        }
      }
      term?.dispose();
      Reflect.deleteProperty(container, "screenText");
      // Whatever the terminal left behind: the next one opens here.
      container.replaceChildren();
    };
  }, [id, attempt, kind]);

  const statusColor =
    status === "connected"
      ? "text-green-600"
      : status === "error"
        ? "text-red-600"
        : status === "closed"
          ? "text-gray-500"
          : "text-sky-600";

  return (
    <div className="flex flex-col" style={{ height: "calc(100vh - 140px)" }}>
      <div className="flex items-center justify-between mb-3">
        <Link
          to={`/vms/${id}`}
          className="text-sky-600 hover:text-sky-700 inline-flex items-center"
        >
          <svg
            className="w-4 h-4 mr-1"
            fill="none"
            stroke="currentColor"
            viewBox="0 0 24 24"
          >
            <path
              strokeLinecap="round"
              strokeLinejoin="round"
              strokeWidth="2"
              d="M15 19l-7-7 7-7"
            />
          </svg>
          Back to VM
        </Link>
        <span className="flex items-center gap-3 text-sm text-gray-600">
          <label className="flex items-center gap-1">
            Terminal:
            <select
              className="px-2 py-1 text-xs border border-gray-300 rounded-lg bg-white"
              value={kind}
              onChange={(e) => {
                const k = e.target.value as TerminalKind;
                saveTerminalKind(k);
                setKind(k);
              }}
            >
              {TERMINAL_KINDS.map((t) => (
                <option key={t.kind} value={t.kind}>
                  {t.label}
                </option>
              ))}
            </select>
          </label>
          <span>
            Status: <span className={`font-medium ${statusColor}`}>{status}</span>
          </span>
          {(status === "closed" || status === "error") && (
            <button
              className="px-3 py-1 text-xs font-medium text-white bg-sky-600 hover:bg-sky-700 rounded-lg"
              onClick={() => setAttempt((a) => a + 1)}
            >
              Reconnect
            </button>
          )}
        </span>
      </div>

      {error && (
        <div className="mb-3 p-3 bg-red-50 border border-red-200 rounded-lg text-red-700 text-sm">
          {error}
        </div>
      )}

      <div
        ref={containerRef}
        data-testid="console-terminal"
        className="flex-1 rounded-lg overflow-hidden border border-gray-800"
        style={{ backgroundColor: "#000" }}
      />

      <p className="mt-2 text-xs text-gray-500">
        Tip: input is sent directly to the VM's serial console. Copy and
        paste with Ctrl+Shift+C / Ctrl+Shift+V (⌘C / ⌘V on macOS).
      </p>
    </div>
  );
}
