import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { rootCertificates } from "node:tls";
import { fileURLToPath } from "node:url";
import { brotliCompressSync, constants as zlibConstants, gzipSync } from "node:zlib";

// GLIDEX_API_SOCKET: the control plane's UI socket (`glidex-ui --dev` and
// the e2e suite set it). The UI's RPCs are gRPC-Web, which the control
// plane takes there only (spec/grpc.md §6). Without it, GLIDEX_API_URL
// points the dev proxy at a control plane over TCP (REST only, then).
const apiSocket = process.env.GLIDEX_API_SOCKET;
const apiUrl = process.env.GLIDEX_API_URL ?? "https://localhost:8841";

/** The file at env var `name`, if set. */
function fileFromEnv(name: string): string | undefined {
  const path = process.env[name];
  return path ? readFileSync(path, "utf8") : undefined;
}

// HTTPS for the dev server: `glidex-ui --dev` passes the UI's certificate
// (configured or self-signed). Run bare (`bunx vite`), it serves plain
// HTTP on localhost.
const cert = fileFromEnv("GLIDEX_UI_TLS_CERT");
const key = fileFromEnv("GLIDEX_UI_TLS_KEY");
const https = cert && key ? { cert, key } : undefined;

// The control plane's certificate is verified, never skipped:
// GLIDEX_API_CA_CERT (`glidex-ui --dev` sets it to the local control
// plane's published tls.crt) is trusted in addition to Node's CA store
// (a `ca` option replaces that store, so it is passed along).
const extraCa = fileFromEnv("GLIDEX_API_CA_CERT");
const apiCa = extraCa ? [...rootCertificates, extraCa] : undefined;

/** `git <args>`, trimmed; `undefined` outside a checkout or on failure. */
function git(...args: string[]): string | undefined {
  try {
    const out = execFileSync("git", args, { cwd: fileURLToPath(new URL(".", import.meta.url)), stdio: ["ignore", "pipe", "ignore"] }).toString().trim();
    return out || undefined;
  } catch {
    return undefined;
  }
}

/**
 * What the footer shows: the version tag on the built commit (if any), the
 * branch and the commit. GLIDEX_BUILD_TAG / _BRANCH / _COMMIT override them
 * for builds from a tarball or a detached CI checkout.
 */
function buildInfo() {
  const env = process.env;
  const branch = env.GLIDEX_BUILD_BRANCH ?? env.GITHUB_REF_NAME ?? git("rev-parse", "--abbrev-ref", "HEAD");
  return {
    tag: env.GLIDEX_BUILD_TAG ?? git("describe", "--tags", "--exact-match", "HEAD") ?? null,
    // A detached HEAD has no branch.
    branch: branch && branch !== "HEAD" ? branch : null,
    commit: env.GLIDEX_BUILD_COMMIT ?? git("rev-parse", "--short=10", "HEAD") ?? null,
    dirty: env.GLIDEX_BUILD_COMMIT ? false : git("status", "--porcelain", "--untracked-files=no") !== undefined,
  };
}

/**
 * Third-party code in chunks of its own: they change only with the
 * dependencies (or the protos), so browsers keep them across UI releases,
 * and no chunk passes Vite's 500 kB warning.
 */
function manualChunks(id: string): string | undefined {
  if (/\/node_modules\/(react|react-dom|react-router|react-router-dom|scheduler)\//.test(id)) {
    return "react";
  }
  if (/\/node_modules\/@(bufbuild|connectrpc)\//.test(id) || id.includes("/src/gen/")) {
    return "rpc";
  }
  return undefined;
}

/**
 * A `.gz` and a `.br` beside each built text (or WebAssembly) file: glidex-ui serves them
 * to browsers that accept them (`ServeDir::precompressed_*`).
 */
function precompress(): Plugin {
  return {
    name: "glidex-precompress",
    apply: "build",
    writeBundle(options, bundle) {
      const dir = options.dir ?? "dist";
      const files = [...Object.keys(bundle)].filter((f) => /\.(js|css|html|svg|json|wasm)$/.test(f));
      for (const file of files) {
        const path = join(dir, file);
        const data = readFileSync(path);
        if (data.length < 1024) continue;
        writeFileSync(`${path}.gz`, gzipSync(data, { level: 9 }));
        writeFileSync(
          `${path}.br`,
          brotliCompressSync(data, { params: { [zlibConstants.BROTLI_PARAM_QUALITY]: 11 } }),
        );
      }
    },
  };
}

export default defineConfig({
  plugins: [react(), precompress()],
  define: {
    __GLIDEX_BUILD__: JSON.stringify(buildInfo()),
  },
  build: {
    rollupOptions: { output: { manualChunks } },
  },
  server: {
    port: 5173,
    https,
    proxy: {
      "/api": apiSocket
        ? {
            target: { socketPath: apiSocket, host: "localhost", protocol: "http:" } as unknown as string,
            ws: true,
            rewrite: (path) => path.replace(/^\/api/, ""),
          }
        : {
            target: apiUrl,
            changeOrigin: true,
            ws: true,
            secure: true,
            ...(apiCa ? { ca: apiCa } : {}),
            rewrite: (path) => path.replace(/^\/api/, ""),
          },
    },
  },
});
