import { TokenReveal as SecretReveal } from "@glidex/ui-kit";
import type { IssuedToken } from "../types";
import { num } from "../types";

/** A join or rejoin token, shown once (spec/16-clustering-ui.md §2.3): masked
 * until asked, copied to the clipboard on request, never put in a URL,
 * storage or a command line. The commands read it from a file. The caller
 * drops the token when the dialog closes. */
export default function TokenReveal({ issued, commands }: { issued: IssuedToken; commands: string[] }) {
  const hours = Math.round(num(issued.ttl_secs) / 360) / 10;
  return (
    <SecretReveal
      secret={issued.token}
      warning={
        <>
          This token is shown only now and works for {hours} hour{hours === 1 ? "" : "s"}. Treat it like a password: whoever has it can join a
          node to this cluster.
        </>
      }
      copiedHint="Copied. Paste it into a file readable only by you (mode 0600) on the new host."
    >
      {issued.ca_fingerprint && (
        <p className="text-xs text-gray-600">
          The cluster CA's fingerprint, to compare on the joining host: <span className="font-mono break-all">{issued.ca_fingerprint}</span>
        </p>
      )}
      <div>
        <p className="text-gray-700">On the host, with the token saved in a file:</p>
        <pre className="mt-1 p-3 bg-gray-900 text-gray-100 rounded-lg text-xs overflow-x-auto">{commands.join("\n")}</pre>
      </div>
    </SecretReveal>
  );
}
