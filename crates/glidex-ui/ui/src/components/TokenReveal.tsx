import { useState } from "react";
import type { IssuedToken } from "../types";

/** A join or rejoin token, shown once (spec/clustering-ui.md §2.3): masked
 * until asked, copied to the clipboard on request, never put in a URL,
 * storage or a command line. The commands read it from a file. The caller
 * drops the token when the dialog closes. */
export default function TokenReveal({ issued, commands }: { issued: IssuedToken; commands: string[] }) {
  const [shown, setShown] = useState(false);
  const [copied, setCopied] = useState<"ok" | "failed" | null>(null);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(issued.token);
      setCopied("ok");
    } catch {
      setCopied("failed");
    }
  };
  const hours = Math.round(issued.ttl_secs / 360) / 10;
  return (
    <div className="space-y-3 text-sm">
      <p className="text-gray-700">
        This token is shown only now and works for {hours} hour{hours === 1 ? "" : "s"}. Treat it like a password: whoever has it can join a
        node to this cluster.
      </p>
      <div className="flex gap-2">
        <input
          readOnly
          aria-label="token"
          type={shown ? "text" : "password"}
          autoComplete="off"
          spellCheck={false}
          className="flex-1 px-3 py-2 border border-gray-300 rounded-lg font-mono text-xs bg-gray-50"
          value={issued.token}
          onFocus={(e) => e.currentTarget.select()}
        />
        <button type="button" className="px-3 py-2 text-xs bg-gray-100 hover:bg-gray-200 rounded-lg" onClick={() => setShown((s) => !s)}>
          {shown ? "Hide" : "Show"}
        </button>
        <button type="button" className="px-3 py-2 text-xs text-white bg-sky-600 hover:bg-sky-700 rounded-lg" onClick={copy}>
          Copy
        </button>
      </div>
      {copied === "ok" && <p className="text-xs text-green-700">Copied. Paste it into a file readable only by you (mode 0600) on the new host.</p>}
      {copied === "failed" && <p className="text-xs text-amber-700">The browser refused the clipboard; select the field and copy it.</p>}
      {issued.ca_fingerprint && (
        <p className="text-xs text-gray-600">
          The cluster CA's fingerprint, to compare on the joining host: <span className="font-mono break-all">{issued.ca_fingerprint}</span>
        </p>
      )}
      <div>
        <p className="text-gray-700">On the host, with the token saved in a file:</p>
        <pre className="mt-1 p-3 bg-gray-900 text-gray-100 rounded-lg text-xs overflow-x-auto">{commands.join("\n")}</pre>
      </div>
    </div>
  );
}
