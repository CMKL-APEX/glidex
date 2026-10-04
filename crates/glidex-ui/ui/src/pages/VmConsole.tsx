import { useEffect, useRef, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import * as api from "../api";

type Status = "connecting" | "connected" | "closed" | "error";

export default function VmConsole() {
  const { id } = useParams<{ id: string }>();
  const containerRef = useRef<HTMLDivElement>(null);
  const [status, setStatus] = useState<Status>("connecting");
  const [error, setError] = useState<string | null>(null);
  // Bumped by Reconnect: the VM outlives the connection (a restart of the
  // VM or of the control plane closes it), so connecting again picks up.
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    if (!id || !containerRef.current) return;
    setStatus("connecting");
    setError(null);

    const term = new Terminal({
      fontFamily: "Menlo, Monaco, Consolas, monospace",
      fontSize: 13,
      cursorBlink: true,
      convertEol: true,
      theme: { background: "#000000", foreground: "#e6e6e6" },
    });
    const fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.open(containerRef.current);
    fitAddon.fit();

    const handleResize = () => {
      try {
        fitAddon.fit();
      } catch {
        /* container not laid out yet */
      }
    };
    window.addEventListener("resize", handleResize);

    let ws: WebSocket | null = null;
    let disposed = false;
    const encoder = new TextEncoder();
    const inputDisposable = term.onData((data) => {
      if (ws?.readyState === WebSocket.OPEN) {
        ws.send(encoder.encode(data));
      }
    });

    // Browsers open the console with a fresh single-use ticket
    // (spec/security.md §5.6): POST for it, then connect at once.
    api
      .consoleTicket(id)
      .then(({ ticket }) => {
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
            setError(`WebSocket closed (code ${ev.code})`);
          }
        };
        sock.onerror = () => {
          setStatus("error");
          setError("WebSocket connection error");
        };
        sock.onmessage = (ev) => {
          if (typeof ev.data === "string") {
            term.write(ev.data);
          } else {
            term.write(new Uint8Array(ev.data as ArrayBuffer));
          }
        };
      })
      .catch((e) => {
        if (disposed) return;
        setStatus("error");
        setError(e instanceof Error ? e.message : String(e));
      });

    return () => {
      disposed = true;
      window.removeEventListener("resize", handleResize);
      inputDisposable.dispose();
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
      // xterm 5.5 queues a setTimeout(syncScrollArea) in open(); disposing
      // before it runs (StrictMode's mount/unmount, a quick navigation)
      // makes it throw on the disposed renderer. Take the terminal off the
      // page now and dispose it once that timer has run.
      term.element?.remove();
      setTimeout(() => term.dispose(), 0);
    };
  }, [id, attempt]);

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
        className="flex-1 rounded-lg overflow-hidden border border-gray-800"
        style={{ backgroundColor: "#000" }}
      />

      <p className="mt-2 text-xs text-gray-500">
        Tip: input is sent directly to the VM's serial console.
      </p>
    </div>
  );
}
